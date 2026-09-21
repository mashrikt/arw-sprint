//! One utility-priority decoder for visible and nearby filmstrip thumbnails.
//! No main-image eviction, full-preview fallback, or unbounded result queue.
use crate::{
    arw::{EmbeddedPreview, PreviewReader},
    image::{
        cache::{DecodedImage, MemoryBudget},
        decoder::{jpeg_layout, Backend, Decoder},
    },
};
use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_PLAN: usize = 48;
const MAX_EDGE: u32 = 512;
const MAX_OUTPUT_EDGE: u32 = 192;
const MAX_JPEG_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct ThumbnailTimings {
    /// Includes open and orientation, which is read during IFD discovery.
    pub parse: Duration,
    pub read: Duration,
    pub decode: Duration,
    pub metadata: Duration,
    pub total: Duration,
}

#[derive(Debug)]
pub enum ThumbnailFailure {
    /// No main-image allocation was evicted. Submit a fresh plan when there
    /// is spare memory; the remainder of this plan is paused to avoid spinning.
    Deferred,
    Unavailable(String),
}

#[derive(Debug)]
pub struct ThumbnailEvent {
    pub session: u64,
    pub serial: u64,
    pub index: usize,
    pub path: PathBuf,
    pub orientation: u16,
    pub timings: ThumbnailTimings,
    pub rating: Result<Option<i8>, String>,
    pub result: Result<Arc<DecodedImage>, ThumbnailFailure>,
}

#[derive(Clone)]
struct Task {
    epoch: u64,
    session: u64,
    serial: u64,
    index: usize,
    path: PathBuf,
}

#[derive(Default)]
struct State {
    epoch: u64,
    plan: VecDeque<Task>,
    awaiting: Option<(u64, usize)>,
    started: u64,
    completed: u64,
    stop: bool,
}
impl State {
    fn cancel(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.plan.clear();
        // An already published result still owns decoded pixels. Keep the
        // single-result gate until the consumer drops/uploads and acknowledges.
    }
    fn request(&mut self, session: u64, serial: u64, plan: Vec<(usize, PathBuf)>) {
        self.cancel();
        let mut indices = HashSet::new();
        for (index, path) in plan.into_iter().take(MAX_PLAN) {
            if indices.insert(index) {
                self.plan.push_back(Task {
                    epoch: self.epoch,
                    session,
                    serial,
                    index,
                    path,
                });
            }
        }
    }
    fn relevant(&self, task: &Task) -> bool {
        !self.stop && task.epoch == self.epoch
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    budget: Arc<MemoryBudget>,
}

pub struct ThumbnailWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl ThumbnailWorker {
    pub fn new(
        budget: Arc<MemoryBudget>,
        wake: Arc<dyn Fn(ThumbnailEvent) + Send + Sync>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            budget,
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("fastcull-thumbnails".into())
            .spawn(move || worker(worker_shared, wake))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Replace queued work with at most 48 missing visible/nearby cells. The caller
    /// supplies a fresh serial for every plan, and admits plans only after
    /// foreground work has settled. This method performs no filesystem work.
    pub fn request(&self, session: u64, serial: u64, plan: Vec<(usize, PathBuf)>) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request(session, serial, plan);
        self.shared.changed.notify_one();
    }
    pub fn pause(&self) {
        self.cancel();
    }
    pub fn cancel(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel();
        self.shared.changed.notify_one();
    }
    /// Acknowledge every event, including stale/failing events. For successful
    /// events, wait until GPU upload/cancellation has released the CPU image.
    pub fn acknowledge(&self, serial: u64, index: usize) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.awaiting == Some((serial, index)) {
            state.awaiting = None;
            self.shared.changed.notify_one();
        }
    }

    /// Cumulative dequeued/completed CPU tasks, including cancellation and
    /// failures. Equality means no CPU task is running; an event may still
    /// await upload/acknowledgment. A canceled worker must stay unchanged.
    pub fn activity(&self) -> (u64, u64) {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.started, state.completed)
    }
}
impl Drop for ThumbnailWorker {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stop = true;
        state.cancel();
        drop(state);
        self.shared.changed.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn candidates(previews: Vec<EmbeddedPreview>) -> Vec<EmbeddedPreview> {
    let mut result: Vec<_> = previews
        .into_iter()
        .filter(|preview| {
            matches!(
                (preview.width, preview.height),
                (Some(1..=MAX_EDGE), Some(1..=MAX_EDGE))
            ) && (4..=MAX_JPEG_BYTES).contains(&preview.length)
        })
        .collect();
    result.sort_unstable_by_key(|preview| {
        (
            u64::from(preview.width.unwrap_or(0)) * u64::from(preview.height.unwrap_or(0)),
            preview.length,
        )
    });
    result
}

/// Native reduced IDCT bounds every decoded thumbnail to 192 pixels per side.
/// The usual 160x120 Sony thumbnail retains its full resolution. The source
/// limit remains 512 pixels, so this never becomes a full-preview fallback.
fn output_layout(width: u32, height: u32) -> (u32, u32, u32) {
    let denominator = [1, 2, 4]
        .into_iter()
        .find(|&scale| {
            width.div_ceil(scale) <= MAX_OUTPUT_EDGE && height.div_ceil(scale) <= MAX_OUTPUT_EDGE
        })
        .unwrap_or(4);
    (
        width.div_ceil(denominator),
        height.div_ceil(denominator),
        denominator,
    )
}

/// Lazy native handles avoid rebuilding a decoder when files use different
/// thumbnail sizes. Completed RGBA allocations always move out of the handle.
#[derive(Default)]
struct ThumbnailDecoders {
    decoders: [Option<Result<Decoder, String>>; 3],
}
impl ThumbnailDecoders {
    fn get(&mut self, denominator: u32) -> Result<&mut Decoder, String> {
        let index = match denominator {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => return Err("unsupported thumbnail reduction".into()),
        };
        self.decoders[index]
            .get_or_insert_with(|| Decoder::with_scale(Backend::Turbo, denominator))
            .as_mut()
            .map_err(|error| error.clone())
    }
}

enum LoadFailure {
    Cancelled,
    Failed(ThumbnailFailure),
}
fn checkpoint(shared: &Shared, task: &Task) -> Result<(), LoadFailure> {
    if shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .relevant(task)
    {
        Ok(())
    } else {
        Err(LoadFailure::Cancelled)
    }
}
fn unavailable(error: impl ToString) -> LoadFailure {
    LoadFailure::Failed(ThumbnailFailure::Unavailable(error.to_string()))
}

struct ActivityGuard<'a>(&'a Shared);
impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.completed = state.completed.saturating_add(1);
    }
}

fn load(
    shared: &Shared,
    task: &Task,
    decoders: &mut ThumbnailDecoders,
    orientation: &mut u16,
    timings: &mut ThumbnailTimings,
) -> Result<Arc<DecodedImage>, LoadFailure> {
    checkpoint(shared, task)?;
    let stage = Instant::now();
    let mut reader = PreviewReader::open(&task.path).map_err(unavailable)?;
    let previews = reader.find_previews().map_err(unavailable)?;
    *orientation = reader.orientation();
    timings.parse = stage.elapsed();
    let mut last_error = "No embedded thumbnail within 512 pixels and 256 KiB".to_string();
    for preview in candidates(previews) {
        checkpoint(shared, task)?;
        let width = preview.width.unwrap_or(0);
        let height = preview.height.unwrap_or(0);
        let (output_width, output_height, scale) = output_layout(width, height);
        let rgba = (output_width as usize)
            .checked_mul(output_height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| unavailable("thumbnail dimensions overflow"))?;
        let jpeg_bytes = usize::try_from(preview.length).map_err(unavailable)?;
        let required = jpeg_bytes
            .checked_add(rgba)
            .ok_or_else(|| unavailable("thumbnail allocation overflow"))?;
        let mut jpeg_lease = shared
            .budget
            .try_reserve(required)
            .ok_or(LoadFailure::Failed(ThumbnailFailure::Deferred))?;
        let mut decoded_lease = jpeg_lease
            .split_off(rgba)
            .ok_or_else(|| unavailable("thumbnail reservation split"))?;
        let mut jpeg = Vec::new();
        let stage = Instant::now();
        let read = reader.read_preview_into(&preview, &mut jpeg);
        timings.read += stage.elapsed();
        if !jpeg_lease.resize(jpeg.capacity()) {
            return Err(LoadFailure::Failed(ThumbnailFailure::Deferred));
        }
        if let Err(error) = read {
            last_error = error.to_string();
            continue;
        }
        checkpoint(shared, task)?;
        // Revalidate the actual bytes before native decoding. A source changed
        // between discovery and read must never allocate a large main image
        // underneath a small thumbnail reservation.
        match jpeg_layout(&jpeg) {
            Ok(layout) if layout.width == width && layout.height == height => {}
            Ok(_) => {
                last_error = "Thumbnail dimensions changed during read".into();
                continue;
            }
            Err(error) => {
                last_error = error;
                continue;
            }
        }
        let stage = Instant::now();
        let decoder = decoders.get(scale).map_err(unavailable)?;
        let decoded = decoder.decode(&jpeg);
        timings.decode += stage.elapsed();
        // Clear retained decoder buffers on success AND failure before another
        // job, so an idle worker never hides an allocation outside the budget.
        let pixels = decoder.take_pixels();
        drop(jpeg);
        drop(jpeg_lease);
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                drop(pixels);
                last_error = error;
                continue;
            }
        };
        if !decoded_lease.resize(pixels.capacity()) {
            drop(pixels);
            return Err(LoadFailure::Failed(ThumbnailFailure::Deferred));
        }
        checkpoint(shared, task)?;
        let image = DecodedImage::new(
            pixels,
            decoded.width,
            decoded.height,
            width,
            height,
            scale,
            decoded_lease,
        )
        .map_err(unavailable)?;
        return Ok(Arc::new(image));
    }
    Err(unavailable(last_error))
}

fn worker(shared: Arc<Shared>, wake: Arc<dyn Fn(ThumbnailEvent) + Send + Sync>) {
    if let Err(error) = crate::platform::set_thumbnail_thread_qos() {
        eprintln!("Thumbnail utility QoS unavailable: {error}");
    }
    let mut decoders = ThumbnailDecoders::default();
    loop {
        let task = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stop {
                    return;
                }
                if state.awaiting.is_none() {
                    if let Some(task) = state.plan.pop_front() {
                        state.started = state.started.saturating_add(1);
                        break task;
                    }
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        let _activity = ActivityGuard(&shared);
        let start = Instant::now();
        let mut orientation = 1;
        let mut timings = ThumbnailTimings::default();
        let loaded = load(
            &shared,
            &task,
            &mut decoders,
            &mut orientation,
            &mut timings,
        );
        let rating = if loaded.is_ok() {
            if checkpoint(&shared, &task).is_err() {
                continue;
            }
            let stage = Instant::now();
            let rating = crate::xmp::read_rating(&task.path).map_err(|error| error.to_string());
            timings.metadata = stage.elapsed();
            rating
        } else {
            Err("Thumbnail metadata was not read".into())
        };
        timings.total = start.elapsed();
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.relevant(&task) {
            continue;
        }
        let result = match loaded {
            Ok(image) => Ok(image),
            Err(LoadFailure::Cancelled) => continue,
            Err(LoadFailure::Failed(failure)) => Err(failure),
        };
        if matches!(&result, Err(ThumbnailFailure::Deferred)) {
            state.plan.clear();
        }
        state.awaiting = Some((task.serial, task.index));
        drop(state);
        wake(ThumbnailEvent {
            session: task.session,
            serial: task.serial,
            index: task.index,
            path: task.path,
            orientation,
            timings,
            rating,
            result,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new(jpegs: &[Vec<u8>], orientation: u16) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "fastcull-thumbnails-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            let path = directory.join("synthetic.ARW");
            let mut raw = vec![0; 512];
            raw[..8].copy_from_slice(&[b'I', b'I', 42, 0, 8, 0, 0, 0]);
            let mut jpeg_offset = 512;
            for (i, jpeg) in jpegs.iter().enumerate() {
                let ifd = 8 + i * 48;
                raw[ifd..ifd + 2].copy_from_slice(&3u16.to_le_bytes());
                for (entry, tag, kind, value) in [
                    (0, 0x0112u16, 3u16, u32::from(orientation)),
                    (1, 0x0201, 4, jpeg_offset as u32),
                    (2, 0x0202, 4, jpeg.len() as u32),
                ] {
                    let p = ifd + 2 + entry * 12;
                    raw[p..p + 2].copy_from_slice(&tag.to_le_bytes());
                    raw[p + 2..p + 4].copy_from_slice(&kind.to_le_bytes());
                    raw[p + 4..p + 8].copy_from_slice(&1u32.to_le_bytes());
                    raw[p + 8..p + 12].copy_from_slice(&value.to_le_bytes());
                }
                if i + 1 < jpegs.len() {
                    raw[ifd + 38..ifd + 42].copy_from_slice(&((ifd + 48) as u32).to_le_bytes());
                }
                raw.extend_from_slice(jpeg);
                jpeg_offset += jpeg.len();
            }
            fs::write(&path, raw).unwrap();
            Self { directory, path }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xdb, 0, 67, 0];
        jpeg.extend_from_slice(&[1; 64]);
        jpeg.extend_from_slice(&[0xff, 0xc0, 0, 11, 8]);
        jpeg.extend_from_slice(&height.to_be_bytes());
        jpeg.extend_from_slice(&width.to_be_bytes());
        jpeg.extend_from_slice(&[1, 1, 0x11, 0, 0xff, 0xc4, 0, 38]);
        for table in [0, 0x10] {
            jpeg.extend_from_slice(&[table, 1]);
            jpeg.extend_from_slice(&[0; 15]);
            jpeg.push(0);
        }
        jpeg.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x3f, 0xff, 0xd9]);
        jpeg
    }
    fn next(events: &mpsc::Receiver<ThumbnailEvent>) -> ThumbnailEvent {
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("thumbnail worker timed out")
    }
    fn create(budget: Arc<MemoryBudget>) -> (ThumbnailWorker, mpsc::Receiver<ThumbnailEvent>) {
        let (send, events) = mpsc::channel();
        let worker = ThumbnailWorker::new(
            budget,
            Arc::new(move |event| {
                send.send(event).unwrap();
            }),
        )
        .unwrap();
        (worker, events)
    }

    #[test]
    fn selection_rejects_large_unknown_and_oversized_sources() {
        let preview = |w, h, length| EmbeddedPreview {
            offset: 0,
            length,
            width: w,
            height: h,
        };
        let selected = candidates(vec![
            preview(Some(7008), Some(4672), 7000),
            preview(Some(160), Some(120), 7000),
            preview(Some(80), Some(60), 4000),
            preview(Some(80), Some(60), 3000),
            preview(Some(8), Some(8), MAX_JPEG_BYTES + 1),
            preview(None, Some(120), 100),
            preview(Some(0), Some(1), 10),
        ]);
        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].length, 3000);
        assert_eq!(selected[2].width, Some(160));
    }

    #[test]
    fn native_reduction_preserves_tiny_sources_and_bounds_every_eligible_size() {
        assert_eq!(output_layout(160, 120), (160, 120, 1));
        assert_eq!(output_layout(192, 192), (192, 192, 1));
        assert_eq!(output_layout(193, 191), (97, 96, 2));
        assert_eq!(output_layout(384, 256), (192, 128, 2));
        assert_eq!(output_layout(385, 257), (97, 65, 4));
        assert_eq!(output_layout(512, 512), (128, 128, 4));
        for edge in 1..=MAX_EDGE {
            for (width, height) in [(edge, 1), (1, edge), (edge, edge)] {
                let (w, h, scale) = output_layout(width, height);
                assert!((1..=MAX_OUTPUT_EDGE).contains(&w));
                assert!((1..=MAX_OUTPUT_EDGE).contains(&h));
                assert_eq!((w, h), (width.div_ceil(scale), height.div_ceil(scale)));
            }
        }
        // The maximum visible band fits within its 4 MiB texture cache even
        // for cameras whose smallest embedded JPEG is larger than Sony's.
        assert!(24 * MAX_OUTPUT_EDGE as usize * MAX_OUTPUT_EDGE as usize * 4 < 4 * 1024 * 1024);
    }

    #[test]
    fn reduced_decode_accounts_only_native_output_and_keeps_source_dimensions() {
        let budget = MemoryBudget::new(256 * 1024);
        let (worker, events) = create(Arc::clone(&budget));
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        for (serial, (width, height)) in [
            (512, 384),
            (160, 120),
            (383, 255),
            (511, 509),
            (192, 192),
            (384, 383),
        ]
        .into_iter()
        .enumerate()
        {
            let pixels = vec![128; width * height * 3];
            let jpeg = compressor
                .compress_to_vec(turbojpeg::Image {
                    pixels: pixels.as_slice(),
                    width,
                    pitch: width * 3,
                    height,
                    format: turbojpeg::PixelFormat::RGB,
                })
                .unwrap();
            let fixture = Fixture::new(&[jpeg], 6);
            worker.request(1, serial as u64, vec![(0, fixture.path.clone())]);
            let event = next(&events);
            let image = event.result.as_ref().unwrap();
            let (expected_width, expected_height, expected_scale) =
                output_layout(width as u32, height as u32);
            assert_eq!(
                (image.width, image.height),
                (expected_width, expected_height)
            );
            assert_eq!(
                (image.original_width, image.original_height),
                (width as u32, height as u32)
            );
            assert_eq!(image.scale, expected_scale);
            assert_eq!(
                image.bytes(),
                (expected_width * expected_height * 4) as usize
            );
            assert_eq!(budget.used(), image.bytes());
            assert_eq!(event.orientation, 6);
            assert!(image.pixels.chunks_exact(4).all(|pixel| {
                pixel[..3].iter().all(|&v| (127..=129).contains(&v)) && pixel[3] == 255
            }));
            drop(event);
            assert_eq!(budget.used(), 0);
            worker.acknowledge(serial as u64, 0);
        }
        drop(worker);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn plan_is_bounded_latest_only_and_cancel_invalidates_inflight() {
        let mut state = State::default();
        state.request(
            1,
            1,
            (0..100).map(|i| (i, PathBuf::from("test.ARW"))).collect(),
        );
        assert_eq!(state.plan.len(), MAX_PLAN);
        let old = state.plan.pop_front().unwrap();
        state.request(
            2,
            2,
            vec![
                (2, PathBuf::from("new.ARW")),
                (2, PathBuf::from("duplicate.ARW")),
            ],
        );
        assert!(!state.relevant(&old));
        assert_eq!(state.plan.len(), 1);
        let current = state.plan.pop_front().unwrap();
        state.cancel();
        assert!(!state.relevant(&current));
    }

    #[test]
    fn success_has_one_event_backpressure_and_stale_ack_does_not_release_new_plan() {
        let fixture = Fixture::new(&[jpeg(8, 8)], 8);
        let budget = MemoryBudget::new(1024 * 1024);
        let (worker, events) = create(Arc::clone(&budget));
        worker.request(
            1,
            10,
            vec![(0, fixture.path.clone()), (1, fixture.path.clone())],
        );
        let first = next(&events);
        assert_eq!(first.orientation, 8);
        assert!(first.result.is_ok());
        assert_eq!(budget.used(), 256);
        assert!(events.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(worker.activity(), (1, 1));
        worker.pause();
        worker.request(2, 20, vec![(2, fixture.path.clone())]);
        worker.acknowledge(999, 0);
        assert!(events.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(worker.activity(), (1, 1));
        drop(first);
        worker.acknowledge(10, 0);
        let second = next(&events);
        assert_eq!((second.session, second.serial, second.index), (2, 20, 2));
        worker.acknowledge(10, 0);
        drop(worker); // Shutdown must not wait for an acknowledgement.
        drop(second);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn budget_deferral_does_not_evict_main_memory_or_spin() {
        let fixture = Fixture::new(&[jpeg(8, 8)], 1);
        let budget = MemoryBudget::new(1024);
        let main = budget.try_reserve(1024).unwrap();
        let (worker, events) = create(Arc::clone(&budget));
        worker.request(
            1,
            1,
            vec![(0, fixture.path.clone()), (1, fixture.path.clone())],
        );
        assert!(matches!(
            next(&events).result,
            Err(ThumbnailFailure::Deferred)
        ));
        assert_eq!(budget.used(), 1024);
        worker.acknowledge(1, 0);
        drop(main);
        assert!(events.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(worker.activity(), (1, 1));
        worker.request(1, 2, vec![(0, fixture.path.clone())]);
        let event = next(&events);
        assert!(event.result.is_ok());
        drop(event);
        drop(worker);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn malformed_small_preview_falls_back_only_to_another_small_preview() {
        let mut malformed = jpeg(1, 1);
        let sos = malformed
            .windows(2)
            .position(|p| p == [0xff, 0xda])
            .unwrap();
        malformed[sos + 5] = 2;
        let fixture = Fixture::new(&[malformed.clone(), jpeg(8, 8)], 6);
        let budget = MemoryBudget::new(1024 * 1024);
        let (worker, events) = create(Arc::clone(&budget));
        worker.request(1, 1, vec![(0, fixture.path.clone())]);
        let event = next(&events);
        let image = event.result.as_ref().unwrap();
        assert_eq!((image.width, image.height), (8, 8));
        assert_eq!(event.orientation, 6);
        drop(event);
        worker.acknowledge(1, 0);
        let no_fallback = Fixture::new(&[malformed, jpeg(7008, 4672)], 1);
        worker.request(1, 2, vec![(0, no_fallback.path.clone())]);
        assert!(matches!(
            next(&events).result,
            Err(ThumbnailFailure::Unavailable(_))
        ));
        drop(worker);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn failure_releases_allocations_and_obeys_acknowledgement() {
        let fixture = Fixture::new(&[jpeg(8, 8)], 1);
        let budget = MemoryBudget::new(1024 * 1024);
        let (worker, events) = create(Arc::clone(&budget));
        worker.request(
            1,
            1,
            vec![
                (0, fixture.directory.join("missing.ARW")),
                (1, fixture.path.clone()),
            ],
        );
        assert!(matches!(
            next(&events).result,
            Err(ThumbnailFailure::Unavailable(_))
        ));
        assert_eq!(budget.used(), 0);
        assert!(events.recv_timeout(Duration::from_millis(30)).is_err());
        assert_eq!(worker.activity(), (1, 1));
        worker.acknowledge(1, 0);
        let event = next(&events);
        assert!(event.result.is_ok());
        drop(event);
        drop(worker);
        assert_eq!(budget.used(), 0);
    }
}
