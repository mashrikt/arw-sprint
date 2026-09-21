use super::cache::{DecodedCache, DecodedImage, MemoryBudget, MemoryLease};
use super::decoder::{jpeg_layout, Backend, Decoder, MAX_DECODED_BYTES};
use crate::arw::{EmbeddedPreview, PreviewReader};
use crate::browser::prefetch::{self, Direction};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone)]
pub struct LoaderConfig {
    pub budget: Arc<MemoryBudget>,
    pub decode_scale: u32,
    pub ahead: usize,
    pub behind: usize,
    pub cpu_cache_bytes: usize,
}

impl LoaderConfig {
    pub fn new(budget: Arc<MemoryBudget>) -> Self {
        Self {
            budget,
            decode_scale: 1,
            ahead: 8,
            behind: 3,
            cpu_cache_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoadTimings {
    pub parse: Duration,
    pub jpeg_read: Duration,
    pub decode: Duration,
    pub total: Duration,
    pub cache_hit: bool,
}

/// Notifications carry no image ownership. The UI retrieves the current index
/// from the bounded cache, so a slow event loop cannot accumulate RGBA buffers.
#[derive(Clone, Debug)]
pub enum LoaderEvent {
    Ready {
        session: u64,
        index: usize,
        generation: u64,
        timings: LoadTimings,
        prefetched: bool,
    },
    Failed {
        session: u64,
        index: usize,
        generation: u64,
        error: String,
    },
    /// CPU eviction was insufficient for the active request. The UI can release
    /// spare GPU textures, then release the displayed texture only if necessary.
    MemoryPressure {
        session: u64,
        index: usize,
        generation: u64,
        bytes_needed: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Role {
    Foreground,
    Speculative,
}

#[derive(Clone, Copy, Debug)]
struct Task {
    session: u64,
    generation: u64,
    index: usize,
    role: Role,
}

struct State {
    session: u64,
    generation: u64,
    files: Arc<Vec<PathBuf>>,
    target: Option<usize>,
    wanted: Vec<usize>,
    foreground: Option<Task>,
    prefetch: VecDeque<Task>,
    inflight: HashSet<(u64, usize)>,
    foreground_running: bool,
    prefetch_paused: bool,
    current_available: bool,
    current_gpu_cached: bool,
    gpu_cached: HashSet<usize>,
    shutdown: bool,
    cache: DecodedCache,
}

impl State {
    fn new(budget: Arc<MemoryBudget>) -> Self {
        Self {
            session: 0,
            generation: 0,
            files: Arc::new(Vec::new()),
            target: None,
            wanted: Vec::new(),
            foreground: None,
            prefetch: VecDeque::new(),
            inflight: HashSet::new(),
            foreground_running: false,
            prefetch_paused: true,
            current_available: false,
            current_gpu_cached: false,
            gpu_cached: HashSet::new(),
            shutdown: false,
            cache: DecodedCache::new(budget),
        }
    }

    fn relevant(&self, task: Task) -> bool {
        if self.shutdown
            || task.session != self.session
            || task.generation > self.generation
            || task.index >= self.files.len()
        {
            return false;
        }
        if self.gpu_cached.contains(&task.index)
            || (self.current_gpu_cached && self.target == Some(task.index))
        {
            return false;
        }
        if task.role == Role::Foreground {
            return self.target == Some(task.index);
        }
        if self.target == Some(task.index) {
            return true;
        } // Promote an in-flight neighbor.
        task.generation == self.generation
            && !self.prefetch_paused
            && self.foreground.is_none()
            && !self.foreground_running
            && self.wanted.contains(&task.index)
    }

    fn set_files(&mut self, session: u64, files: Arc<Vec<PathBuf>>) {
        self.cache.rekey(&self.files, &files);
        self.session = session;
        self.generation = 0;
        self.files = files;
        self.target = None;
        self.wanted.clear();
        self.foreground = None;
        self.prefetch.clear();
        self.prefetch_paused = true;
        self.current_available = false;
        self.current_gpu_cached = false;
        self.gpu_cached.clear();
    }

    fn rebuild_prefetch(&mut self) {
        self.prefetch = self
            .wanted
            .iter()
            .copied()
            .filter(|index| {
                Some(*index) != self.target
                    && !self.cache.contains(*index)
                    && !self.gpu_cached.contains(index)
                    && !self.inflight.contains(&(self.session, *index))
            })
            .map(|index| Task {
                session: self.session,
                generation: self.generation,
                index,
                role: Role::Speculative,
            })
            .collect();
    }

    fn set_gpu_cached(&mut self, indices: &[usize]) {
        self.gpu_cached = indices
            .iter()
            .copied()
            .filter(|index| *index < self.files.len())
            .collect();
        for &index in &self.gpu_cached {
            self.cache.remove(index);
        }
        if let Some(index) = self.target {
            self.current_gpu_cached = self.gpu_cached.contains(&index);
            self.current_available = self.current_gpu_cached || self.cache.contains(index);
            if self.current_gpu_cached {
                self.foreground = None;
            }
        }
        // Removing a GPU texture never queues a decode for the old current
        // index. The next explicit navigation request owns that decision.
        self.rebuild_prefetch();
    }

    fn navigate(
        &mut self,
        config: &LoaderConfig,
        index: usize,
        generation: u64,
        direction: Direction,
        gpu_cached: bool,
    ) -> Option<LoaderEvent> {
        if index >= self.files.len() {
            return None;
        }
        self.generation = generation;
        self.target = Some(index);
        self.prefetch_paused = true;
        self.wanted = vec![index];
        self.wanted.extend(prefetch::plan(
            index,
            self.files.len(),
            direction,
            config.ahead,
            config.behind,
        ));
        self.cache.retain_indices(&self.wanted);
        self.current_available = gpu_cached || self.cache.contains(index);
        self.current_gpu_cached = gpu_cached;
        self.foreground = (!self.current_available).then_some(Task {
            session: self.session,
            generation,
            index,
            role: Role::Foreground,
        });
        self.rebuild_prefetch();
        if self.cache.contains(index) && !gpu_cached {
            let prefetched = self.cache.take_prefetched(index);
            Some(LoaderEvent::Ready {
                session: self.session,
                index,
                generation,
                timings: LoadTimings {
                    cache_hit: true,
                    ..LoadTimings::default()
                },
                prefetched,
            })
        } else {
            None
        }
    }
}

struct Shared {
    config: LoaderConfig,
    state: Mutex<State>,
    work: Condvar,
    wake: Arc<dyn Fn(LoaderEvent) + Send + Sync>,
}

pub struct ImageLoader {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl ImageLoader {
    pub fn new(
        config: LoaderConfig,
        wake: Arc<dyn Fn(LoaderEvent) + Send + Sync>,
    ) -> Result<Self, String> {
        // Validate configuration before spawning either worker.
        let foreground_decoder = Decoder::with_scale(Backend::Turbo, config.decode_scale)?;
        let speculative_decoder = Decoder::with_scale(Backend::Turbo, config.decode_scale)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(Arc::clone(&config.budget))),
            config,
            work: Condvar::new(),
            wake,
        });
        let mut loader = Self {
            shared,
            workers: Vec::new(),
        };
        for (role, decoder, name) in [
            (Role::Foreground, foreground_decoder, "fastcull-current"),
            (Role::Speculative, speculative_decoder, "fastcull-prefetch"),
        ] {
            let shared = Arc::clone(&loader.shared);
            let worker = thread::Builder::new()
                .name(name.into())
                .spawn(move || worker(shared, role, decoder))
                .map_err(|error| error.to_string())?;
            loader.workers.push(worker);
        }
        Ok(loader)
    }

    pub fn set_files(&self, session: u64, files: Arc<Vec<PathBuf>>) {
        let mut state = lock(&self.shared.state);
        state.set_files(session, files);
        drop(state);
        self.changed();
    }

    pub fn request(&self, index: usize, generation: u64, direction: Direction) {
        self.navigate(index, generation, direction, false);
    }

    /// The GPU already owns this image; schedule only its neighbors.
    pub fn request_cached(&self, index: usize, generation: u64, direction: Direction) {
        self.navigate(index, generation, direction, true);
    }

    fn navigate(&self, index: usize, generation: u64, direction: Direction, gpu_cached: bool) {
        let event = lock(&self.shared.state).navigate(
            &self.shared.config,
            index,
            generation,
            direction,
            gpu_cached,
        );
        self.changed();
        if let Some(event) = event {
            (self.shared.wake)(event);
        }
    }

    pub fn get(&self, index: usize) -> Option<Arc<DecodedImage>> {
        lock(&self.shared.state).cache.get(index)
    }

    /// Synchronize the GPU cache after an accepted upload or GPU eviction.
    /// GPU-owned images need no CPU copy or duplicate speculative decode.
    pub fn set_gpu_cached(&self, indices: &[usize]) {
        lock(&self.shared.state).set_gpu_cached(indices);
        self.changed();
    }

    /// Drop a redundant CPU copy after its GPU texture has been accepted. Any
    /// outstanding upload Arc still retains its lease until that upload ends.
    pub fn discard(&self, index: usize) {
        lock(&self.shared.state).cache.remove(index);
        self.changed();
    }

    /// Call after the current GPU image is accepted. Keeping speculation paused
    /// through upload reserves headroom for the foreground texture and staging.
    pub fn resume_prefetch(&self) {
        let mut state = lock(&self.shared.state);
        state.prefetch_paused = false;
        state.rebuild_prefetch();
        drop(state);
        self.changed();
    }

    pub fn evict_bytes(&self, bytes: usize) -> usize {
        let mut state = lock(&self.shared.state);
        state.prefetch_paused = true;
        let keep = state.target;
        let freed = state.cache.evict_bytes(bytes, keep);
        drop(state);
        self.changed();
        freed
    }

    pub fn evict_all_except(&self, keep: Option<usize>) -> usize {
        let mut state = lock(&self.shared.state);
        state.prefetch_paused = true;
        let freed = state.cache.evict_all_except(keep);
        drop(state);
        self.changed();
        freed
    }

    pub fn evict_for(&self, bytes: usize) -> bool {
        let mut state = lock(&self.shared.state);
        state.prefetch_paused = true;
        let keep = state.target;
        let available = state.cache.evict_for(bytes, keep);
        drop(state);
        self.changed();
        available
    }

    fn changed(&self) {
        self.shared.work.notify_all();
        self.shared.config.budget.wake_waiters();
    }
}

impl Drop for ImageLoader {
    fn drop(&mut self) {
        lock(&self.shared.state).shutdown = true;
        self.changed();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn next_task(shared: &Shared, role: Role) -> Option<(Task, PathBuf)> {
    let mut state = lock(&shared.state);
    loop {
        if state.shutdown {
            return None;
        }
        let task = match role {
            Role::Foreground => state.foreground.take(),
            Role::Speculative
                if !state.prefetch_paused
                    && state.current_available
                    && state.foreground.is_none()
                    && !state.foreground_running =>
            {
                state.prefetch.pop_front()
            }
            Role::Speculative => None,
        };
        if let Some(task) = task {
            if !state.relevant(task)
                || state.cache.contains(task.index)
                || state.inflight.contains(&(task.session, task.index))
            {
                continue;
            }
            let path = state.files[task.index].clone();
            state.inflight.insert((task.session, task.index));
            if role == Role::Foreground {
                state.foreground_running = true;
            }
            return Some((task, path));
        }
        state = shared
            .work
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

enum LoadError {
    Cancelled,
    Failed(String),
}

fn checkpoint(shared: &Shared, task: Task) -> Result<(), LoadError> {
    if lock(&shared.state).relevant(task) {
        Ok(())
    } else {
        Err(LoadError::Cancelled)
    }
}

fn reserve(
    shared: &Shared,
    task: Task,
    jpeg_bytes: usize,
    decoded_bytes: usize,
) -> Result<(MemoryLease, MemoryLease), LoadError> {
    let required = jpeg_bytes
        .checked_add(decoded_bytes)
        .ok_or_else(|| LoadError::Failed("image allocation overflow".into()))?;
    if required > shared.config.budget.limit() {
        return Err(LoadError::Failed(
            "image exceeds the configured memory budget".into(),
        ));
    }
    let mut notified_pressure = false;
    loop {
        let previous_epoch = shared.config.budget.epoch();
        let mut state = lock(&shared.state);
        if !state.relevant(task) {
            return Err(LoadError::Cancelled);
        }
        let foreground = task.role == Role::Foreground || state.target == Some(task.index);
        let keep = state.target;
        let can_allocate = if foreground {
            state.cache.evict_for(required, keep)
        } else {
            // Never evict N+1 just to decode N+8. Stop at the first image that
            // cannot fit; a navigation/upload/budget change wakes the worker.
            let fits = |state: &State| {
                state
                    .cache
                    .bytes()
                    .checked_add(decoded_bytes)
                    .is_some_and(|bytes| bytes <= shared.config.cpu_cache_bytes)
                    && shared.config.budget.available() >= required
            };
            while !fits(&state) {
                let priorities = state.wanted.clone();
                if state.cache.evict_lower_priority(&priorities, task.index) == 0 {
                    break;
                }
            }
            fits(&state)
        };
        if can_allocate {
            if let Some(mut decoded_lease) = shared.config.budget.try_reserve(required) {
                let jpeg_lease = decoded_lease
                    .split_off(jpeg_bytes)
                    .ok_or_else(|| LoadError::Failed("reservation split failed".into()))?;
                return Ok((jpeg_lease, decoded_lease));
            }
        }
        let pressure = if foreground && !notified_pressure {
            notified_pressure = true;
            Some(LoaderEvent::MemoryPressure {
                session: state.session,
                index: task.index,
                generation: state.generation,
                bytes_needed: required,
            })
        } else {
            None
        };
        drop(state);
        if let Some(event) = pressure {
            (shared.wake)(event);
        }
        shared.config.budget.wait_for_change(previous_epoch);
    }
}

fn layout(preview: &EmbeddedPreview, scale: u32) -> Result<(u32, u32, usize), String> {
    let (width, height) = preview
        .width
        .zip(preview.height)
        .ok_or("JPEG dimensions are unavailable")?;
    let pixels = (width.div_ceil(scale) as usize)
        .checked_mul(height.div_ceil(scale) as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_DECODED_BYTES)
        .ok_or("decoded image exceeds the supported memory limit")?;
    Ok((width, height, pixels))
}

/// Discovery and JPEG reading are separate filesystem operations. A replaced
/// preview must not allocate more RGBA bytes than the reservation made for it.
/// Only the bounded JPEG header is examined here, never its entropy payload.
fn validate_reserved_dimensions(jpeg: &[u8], width: u32, height: u32) -> Result<(), String> {
    let actual = jpeg_layout(jpeg)?;
    if actual.width != width || actual.height != height {
        return Err("JPEG preview dimensions changed during read".into());
    }
    Ok(())
}

fn load(
    shared: &Shared,
    task: Task,
    path: &Path,
    decoder: &mut Decoder,
) -> Result<(Arc<DecodedImage>, LoadTimings), LoadError> {
    checkpoint(shared, task)?;
    let start = Instant::now();
    let mut reader =
        PreviewReader::open(path).map_err(|error| LoadError::Failed(error.to_string()))?;
    let previews = reader.find_previews().map_err(|error| {
        let warnings = reader.warnings().join("; ");
        LoadError::Failed(if warnings.is_empty() {
            error.to_string()
        } else {
            format!("{error}: {warnings}")
        })
    })?;
    let mut timings = LoadTimings {
        parse: start.elapsed(),
        ..LoadTimings::default()
    };
    let mut last_error = String::from("no usable JPEG preview");
    for preview in previews {
        checkpoint(shared, task)?;
        let (original_width, original_height, decoded_bytes) =
            match layout(&preview, shared.config.decode_scale) {
                Ok(layout) => layout,
                Err(error) => {
                    last_error = error;
                    continue;
                }
            };
        let jpeg_bytes = usize::try_from(preview.length)
            .map_err(|_| LoadError::Failed("JPEG length overflow".into()))?;
        let (mut jpeg_lease, decoded_lease) = reserve(shared, task, jpeg_bytes, decoded_bytes)?;
        let mut jpeg = Vec::new();
        checkpoint(shared, task)?;
        let read_start = Instant::now();
        let read_result = reader.read_preview_into(&preview, &mut jpeg);
        timings.jpeg_read += read_start.elapsed();
        if !jpeg_lease.resize(jpeg.capacity()) {
            return Err(LoadError::Failed(
                "JPEG allocation exceeded reserved memory".into(),
            ));
        }
        if let Err(error) = read_result {
            last_error = error.to_string();
            continue;
        }
        checkpoint(shared, task)?;
        let decode_start = Instant::now();
        if let Err(error) = validate_reserved_dimensions(&jpeg, original_width, original_height) {
            timings.decode += decode_start.elapsed();
            last_error = error;
            continue;
        }
        let decoded = decoder.decode(&jpeg);
        timings.decode += decode_start.elapsed();
        // take_pixels clears decoder ownership on success AND failure. Buffers
        // therefore cannot hide in sleeping workers outside memory accounting.
        let pixels = decoder.take_pixels();
        drop(jpeg);
        drop(jpeg_lease);
        let info = match decoded {
            Ok(info) => info,
            Err(error) => {
                drop(pixels);
                last_error = error;
                continue;
            }
        };
        checkpoint(shared, task)?;
        let image = DecodedImage::new(
            pixels,
            info.width,
            info.height,
            original_width,
            original_height,
            shared.config.decode_scale,
            decoded_lease,
        )
        .map_err(LoadError::Failed)?;
        timings.total = start.elapsed();
        return Ok((Arc::new(image), timings));
    }
    Err(LoadError::Failed(last_error))
}

fn worker(shared: Arc<Shared>, role: Role, mut decoder: Decoder) {
    while let Some((task, path)) = next_task(&shared, role) {
        let result = load(&shared, task, &path, &mut decoder);
        let event = {
            let mut state = lock(&shared.state);
            state.inflight.remove(&(task.session, task.index));
            if role == Role::Foreground {
                state.foreground_running = false;
            }
            if !state.relevant(task) {
                if role == Role::Speculative {
                    state.rebuild_prefetch();
                }
                None
            } else {
                match result {
                    Ok((image, timings)) => {
                        let prefetched = role == Role::Speculative;
                        state.cache.insert(task.index, image, prefetched);
                        let keep = state.target;
                        state.cache.trim_to(shared.config.cpu_cache_bytes, keep);
                        if state.target == Some(task.index) {
                            state.current_available = true;
                        }
                        Some(LoaderEvent::Ready {
                            session: state.session,
                            index: task.index,
                            generation: state.generation,
                            timings,
                            prefetched,
                        })
                    }
                    Err(LoadError::Failed(error)) => Some(LoaderEvent::Failed {
                        session: state.session,
                        index: task.index,
                        generation: state.generation,
                        error,
                    }),
                    Err(LoadError::Cancelled) => None,
                }
            }
        };
        shared.work.notify_all();
        if let Some(event) = event {
            (shared.wake)(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_jpeg_dimensions_are_rejected_before_the_reserved_decode() {
        // A complete SOF/SOS header is enough: validation never touches entropy
        // bytes or allocates the advertised 125 MiB output.
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8];
        jpeg.extend_from_slice(&4672_u16.to_be_bytes());
        jpeg.extend_from_slice(&7008_u16.to_be_bytes());
        jpeg.extend_from_slice(&[1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
        assert!(validate_reserved_dimensions(&jpeg, 7008, 4672).is_ok());
        assert!(validate_reserved_dimensions(&jpeg, 160, 120)
            .unwrap_err()
            .contains("changed during read"));
        // Equal pixel counts with swapped dimensions still invalidate the
        // advertised layout/orientation; reduced IDCT keeps this same check.
        assert!(validate_reserved_dimensions(&jpeg, 4672, 7008).is_err());
        for length in 0..jpeg.len() {
            assert!(validate_reserved_dimensions(&jpeg[..length], 7008, 4672).is_err());
        }
    }

    fn state() -> (LoaderConfig, State) {
        let config = LoaderConfig::new(MemoryBudget::new(1024));
        let mut state = State::new(Arc::clone(&config.budget));
        state.session = 1;
        state.files = Arc::new(
            (0..100)
                .map(|index| PathBuf::from(format!("{index}.ARW")))
                .collect(),
        );
        (config, state)
    }

    #[test]
    fn latest_generation_replaces_queued_foreground_and_cancels_old_target() {
        let (config, mut state) = state();
        state.navigate(&config, 10, 1, Direction::Forward, false);
        let old = state.foreground.unwrap();
        state.navigate(&config, 30, 9, Direction::Backward, false);
        assert_eq!(state.foreground.unwrap().index, 30);
        assert_eq!(state.foreground.unwrap().generation, 9);
        assert!(!state.relevant(old));
        assert_eq!(state.prefetch.front().unwrap().index, 29);
        assert!(!state.prefetch.iter().any(|task| task.index == 11));
        state.session = 2;
        assert!(!state.relevant(state.foreground.unwrap()));
    }

    #[test]
    fn file_removal_reuses_shifted_neighbor_and_rejects_old_session_tasks() {
        let (config, mut state) = state();
        state.navigate(&config, 10, 8, Direction::Forward, false);
        let old_foreground = state.foreground.unwrap();
        let old_neighbor = *state.prefetch.front().unwrap();
        let lease = config.budget.try_reserve(4).unwrap();
        let image = Arc::new(DecodedImage::new(vec![17; 4], 1, 1, 1, 1, 1, lease).unwrap());
        let pointer = Arc::as_ptr(&image);
        state.cache.insert(11, image, true);
        state.gpu_cached.insert(12);
        let previous_files = Arc::clone(&state.files);
        let mut files = (*previous_files).clone();
        files.remove(10);
        state.set_files(2, Arc::new(files));
        assert_eq!(state.files[10], previous_files[11]);
        assert_eq!(state.generation, 0);
        assert!(state.target.is_none());
        assert!(state.foreground.is_none());
        assert!(state.prefetch.is_empty());
        assert!(state.wanted.is_empty());
        assert!(state.gpu_cached.is_empty());
        assert!(state.prefetch_paused);
        assert!(!state.current_available);
        assert!(!state.current_gpu_cached);
        assert!(!state.relevant(old_foreground));
        assert!(!state.relevant(old_neighbor));
        assert_eq!(config.budget.used(), 4);
        assert_eq!(Arc::as_ptr(&state.cache.get(10).unwrap()), pointer);
        assert!(!state.cache.contains(11));

        let event = state.navigate(&config, 10, 9, Direction::Forward, false);
        assert!(matches!(
            event,
            Some(LoaderEvent::Ready {
                session: 2,
                index: 10,
                generation: 9,
                timings: LoadTimings {
                    cache_hit: true,
                    ..
                },
                prefetched: true
            })
        ));
        assert!(
            state.foreground.is_none(),
            "retained neighbor must not be decoded again"
        );
        assert!(
            !state.relevant(old_foreground),
            "matching shifted index must not revive an old task"
        );
        state.set_files(3, Arc::new(Vec::new()));
        assert_eq!(config.budget.used(), 0);
    }

    #[test]
    fn gpu_hit_schedules_no_duplicate_decode_and_waits_for_upload_headroom() {
        let (config, mut state) = state();
        state.navigate(&config, 20, 3, Direction::Forward, true);
        assert!(state.foreground.is_none());
        assert!(state.current_available);
        let neighbor = *state.prefetch.front().unwrap();
        assert!(!state.relevant(neighbor));
        state.prefetch_paused = false;
        assert!(state.relevant(neighbor));
        state.navigate(&config, 21, 4, Direction::Forward, true);
        assert!(!state.relevant(neighbor)); // An existing GPU texture cancels a redundant in-flight decode.
        state.navigate(&config, 21, 4, Direction::Forward, false);
        assert!(state.relevant(neighbor)); // Promote the requested in-flight image.
    }

    #[test]
    fn gpu_cache_sync_cancels_neighbor_decodes_and_never_requeues_old_current() {
        let (config, mut state) = state();
        state.navigate(&config, 10, 1, Direction::Forward, true);
        state.prefetch_paused = false;
        let neighbor = Task {
            session: 1,
            generation: 1,
            index: 11,
            role: Role::Speculative,
        };
        assert!(state.relevant(neighbor));
        let lease = config.budget.try_reserve(4).unwrap();
        let image = Arc::new(DecodedImage::new(vec![0; 4], 1, 1, 1, 1, 1, lease).unwrap());
        state.cache.insert(11, image, true);
        state.set_gpu_cached(&[10, 11, 1000]);
        assert!(state.current_available);
        assert!(state.current_gpu_cached);
        assert_eq!(config.budget.used(), 0);
        assert!(!state.cache.contains(11));
        assert!(!state.relevant(neighbor));
        assert!(!state.gpu_cached.contains(&1000));
        assert_eq!(state.prefetch.front().unwrap().index, 12);
        state.set_gpu_cached(&[]);
        assert!(!state.current_available);
        assert!(!state.current_gpu_cached);
        assert!(state.foreground.is_none());
        assert!(state.prefetch.iter().all(|task| task.index != 10));
        state.navigate(&config, 12, 2, Direction::Forward, false);
        assert_eq!(state.foreground.unwrap().index, 12);
    }

    #[test]
    fn speculation_stops_at_capacity_and_keeps_the_nearest_neighbor() {
        let (mut config, mut state) = state();
        config.cpu_cache_bytes = 8;
        state.navigate(&config, 10, 1, Direction::Forward, true);
        for index in [10, 11] {
            let lease = config.budget.try_reserve(4).unwrap();
            let image = Arc::new(DecodedImage::new(vec![0; 4], 1, 1, 1, 1, 1, lease).unwrap());
            state.cache.insert(index, image, index != 10);
        }
        state.prefetch_paused = false;
        let shared = Arc::new(Shared {
            config,
            state: Mutex::new(state),
            work: Condvar::new(),
            wake: Arc::new(|_| {}),
        });
        let task = Task {
            session: 1,
            generation: 1,
            index: 12,
            role: Role::Speculative,
        };
        let worker_shared = Arc::clone(&shared);
        let (send, receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let result = reserve(&worker_shared, task, 0, 4);
            send.send(matches!(result, Err(LoadError::Cancelled)))
                .unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        let mut state = lock(&shared.state);
        assert!(state.cache.contains(11));
        assert!(!state.cache.contains(12));
        state.shutdown = true;
        drop(state);
        shared.config.budget.wake_waiters();
        assert!(receive.recv_timeout(Duration::from_secs(1)).unwrap());
        worker.join().unwrap();
    }

    #[test]
    fn nearer_prefetch_replaces_behind_and_foreground_can_reclaim_speculation() {
        let (mut config, mut state) = state();
        config.cpu_cache_bytes = 8;
        state.navigate(&config, 11, 2, Direction::Forward, true);
        state.prefetch_paused = false;
        for index in [10, 11] {
            let lease = config.budget.try_reserve(4).unwrap();
            let image = Arc::new(DecodedImage::new(vec![0; 4], 1, 1, 1, 1, 1, lease).unwrap());
            state.cache.insert(index, image, false);
        }
        let shared = Shared {
            config,
            state: Mutex::new(state),
            work: Condvar::new(),
            wake: Arc::new(|_| {}),
        };
        let task = Task {
            session: 1,
            generation: 2,
            index: 12,
            role: Role::Speculative,
        };
        let leases = reserve(&shared, task, 0, 4).ok().unwrap();
        assert!(!lock(&shared.state).cache.contains(10));
        assert!(lock(&shared.state).cache.contains(11));
        drop(leases);
        let gpu = shared.config.budget.try_reserve(1016).unwrap();
        let mut state = lock(&shared.state);
        state.navigate(&shared.config, 12, 3, Direction::Forward, false);
        drop(state);
        let task = Task {
            session: 1,
            generation: 3,
            index: 12,
            role: Role::Foreground,
        };
        let leases = reserve(&shared, task, 4, 4).ok().unwrap();
        assert!(!lock(&shared.state).cache.contains(11));
        assert_eq!(shared.config.budget.used(), 1024);
        drop(leases);
        drop(gpu);
        assert_eq!(shared.config.budget.used(), 0);
    }

    #[test]
    fn foreground_pressure_notifies_once_and_resumes_when_gpu_memory_is_released() {
        let (config, mut state) = state();
        state.navigate(&config, 10, 1, Direction::Forward, false);
        let gpu = config.budget.try_reserve(1020).unwrap();
        let (events_send, events_receive) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            config,
            state: Mutex::new(state),
            work: Condvar::new(),
            wake: Arc::new(move |event| {
                let _ = events_send.send(event);
            }),
        });
        let worker_shared = Arc::clone(&shared);
        let (done_send, done_receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let task = Task {
                session: 1,
                generation: 1,
                index: 10,
                role: Role::Foreground,
            };
            done_send
                .send(reserve(&worker_shared, task, 4, 4).ok())
                .unwrap();
        });
        assert!(matches!(
            events_receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            LoaderEvent::MemoryPressure {
                session: 1,
                index: 10,
                generation: 1,
                bytes_needed: 8
            }
        ));
        drop(gpu);
        let leases = done_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(shared.config.budget.used(), 8);
        assert!(events_receive
            .recv_timeout(Duration::from_millis(20))
            .is_err());
        drop(leases);
        assert_eq!(shared.config.budget.used(), 0);
        worker.join().unwrap();
    }

    #[test]
    fn navigation_cancels_a_memory_blocked_request_without_releasing_its_budget() {
        let (config, mut state) = state();
        state.navigate(&config, 10, 1, Direction::Forward, false);
        let held_gpu = config.budget.try_reserve(1024).unwrap();
        let (events_send, events_receive) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            config,
            state: Mutex::new(state),
            work: Condvar::new(),
            wake: Arc::new(move |event| {
                let _ = events_send.send(event);
            }),
        });
        let worker_shared = Arc::clone(&shared);
        let (done_send, done_receive) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let task = Task {
                session: 1,
                generation: 1,
                index: 10,
                role: Role::Foreground,
            };
            done_send
                .send(matches!(
                    reserve(&worker_shared, task, 4, 4),
                    Err(LoadError::Cancelled)
                ))
                .unwrap();
        });
        let loader = ImageLoader {
            shared,
            workers: vec![worker],
        };
        assert!(matches!(
            events_receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            LoaderEvent::MemoryPressure { index: 10, .. }
        ));
        // Exercise the public navigation wake path, including the race where
        // this event arrives just before the old request starts waiting.
        loader.request(30, 2, Direction::Forward);
        assert!(done_receive.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(loader.shared.config.budget.used(), 1024);
        drop(loader);
        drop(held_gpu);
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn asynchronous_pipeline_pauses_for_upload_then_prefetches_the_next_frame() {
        // Valid, generated 8x8 gray JPEG. No photographs or user sidecars involved.
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xdb, 0, 67, 0];
        jpeg.extend_from_slice(&[1; 64]);
        jpeg.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        jpeg.extend_from_slice(&[0xff, 0xc4, 0, 38]);
        for table in [0, 0x10] {
            jpeg.extend_from_slice(&[table, 1]);
            jpeg.extend_from_slice(&[0; 15]);
            jpeg.push(0);
        }
        jpeg.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x3f, 0xff, 0xd9]);
        let mut arw = vec![0; 512];
        arw[..8].copy_from_slice(&[b'I', b'I', 42, 0, 8, 0, 0, 0]);
        arw[8..10].copy_from_slice(&2u16.to_le_bytes());
        for (index, tag, value) in [(0, 0x0201_u16, 512_u32), (1, 0x0202_u16, jpeg.len() as u32)] {
            let offset = 10 + index * 12;
            arw[offset..offset + 2].copy_from_slice(&tag.to_le_bytes());
            arw[offset + 2..offset + 4].copy_from_slice(&4u16.to_le_bytes());
            arw[offset + 4..offset + 8].copy_from_slice(&1u32.to_le_bytes());
            arw[offset + 8..offset + 12].copy_from_slice(&value.to_le_bytes());
        }
        arw.extend_from_slice(&jpeg);
        struct TempDir(PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = TempDir(std::env::temp_dir().join(format!(
            "fastcull-loader-test-{}-{nonce}",
            std::process::id()
        )));
        std::fs::create_dir(&directory.0).unwrap();
        let paths: Vec<_> = (0..3)
            .map(|index| {
                let path = directory.0.join(format!("{index}.ARW"));
                std::fs::write(&path, &arw).unwrap();
                path
            })
            .collect();
        let budget = MemoryBudget::new(4096);
        let mut config = LoaderConfig::new(Arc::clone(&budget));
        config.cpu_cache_bytes = 512;
        let (send, receive) = std::sync::mpsc::channel();
        let loader = ImageLoader::new(
            config,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
        loader.set_files(7, Arc::new(paths));
        loader.request(0, 1, Direction::Forward);
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            LoaderEvent::Ready {
                session: 7,
                index: 0,
                generation: 1,
                ..
            }
        ));
        let image = loader.get(0).unwrap();
        assert_eq!((image.width, image.height, image.pixels.len()), (8, 8, 256));
        assert!(image
            .pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [128, 128, 128, 255]));
        drop(image);
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        loader.resume_prefetch();
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            LoaderEvent::Ready {
                index: 1,
                prefetched: true,
                ..
            }
        ));
        assert!(receive.recv_timeout(Duration::from_millis(30)).is_err());
        assert!(loader.get(1).is_some());
        assert!(loader.get(2).is_none());
        loader.request(1, 2, Direction::Forward);
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            LoaderEvent::Ready {
                index: 1,
                generation: 2,
                timings: LoadTimings {
                    cache_hit: true,
                    ..
                },
                prefetched: true,
                ..
            }
        ));
        loader.resume_prefetch();
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            LoaderEvent::Ready {
                index: 2,
                generation: 2,
                ..
            }
        ));
        assert!(loader.get(0).is_none());
        assert!(loader.get(1).is_some());
        assert!(loader.get(2).is_some());
        drop(loader);
        assert_eq!(budget.used(), 0);
    }
}
