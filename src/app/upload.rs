use crate::{
    image::{
        cache::{DecodedImage, MemoryBudget},
        loader::ImageLoader,
    },
    renderer::{GpuUploader, PreparedTexture, UploadError},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Job {
    pub session: u64,
    pub generation: u64,
    pub path: PathBuf,
    pub image: Arc<DecodedImage>,
    pub orientation: u16,
}
pub struct Finished {
    pub session: u64,
    pub generation: u64,
    pub path: PathBuf,
    pub result: Result<Arc<PreparedTexture>, String>,
}

pub struct ThumbJob {
    pub session: u64,
    pub serial: u64,
    pub index: usize,
    pub path: PathBuf,
    pub image: Arc<DecodedImage>,
    pub orientation: u16,
}
pub struct ThumbFinished {
    pub session: u64,
    pub serial: u64,
    pub index: usize,
    pub path: PathBuf,
    pub result: Result<Arc<PreparedTexture>, String>,
    /// Cancellation, replacement, or insufficient budget. No retry was made.
    pub deferred: bool,
}
impl ThumbFinished {
    fn new(job: ThumbJob, result: Result<Arc<PreparedTexture>, String>, deferred: bool) -> Self {
        let ThumbJob {
            session,
            serial,
            index,
            path,
            image,
            ..
        } = job;
        // Completion delivery never keeps the decoded image alive. This also
        // releases a replaced/canceled queue entry before the CPU worker resumes.
        drop(image);
        Self {
            session,
            serial,
            index,
            path,
            result,
            deferred,
        }
    }
    fn skipped(job: ThumbJob, reason: &str) -> Self {
        Self::new(job, Err(reason.into()), true)
    }
}
struct QueuedThumbnail {
    job: ThumbJob,
    epoch: u64,
}
enum Work {
    Photo(Job),
    Thumbnail(QueuedThumbnail),
}
#[derive(Default)]
struct State {
    pending: Option<Job>,
    running: Option<(u64, PathBuf)>,
    target: Option<(u64, PathBuf, u64)>,
    // No texture ownership: the event loop owns these completed results until
    // it acknowledges delivery. Avoid re-uploading during that delivery gap.
    completed: HashMap<(u64, PathBuf), u64>,
    thumbnail: Option<QueuedThumbnail>,
    thumbnail_epoch: u64,
    thumbnail_plan: Option<(u64, u64)>,
    stop: bool,
}
impl State {
    fn take_work(&mut self) -> Option<Work> {
        if self.stop {
            return None;
        }
        if let Some(job) = self.pending.take() {
            self.running = Some((job.session, job.path.clone()));
            return Some(Work::Photo(job));
        }
        self.thumbnail.take().map(Work::Thumbnail)
    }

    fn request_thumbnail(&mut self, job: ThumbJob) -> Option<(ThumbJob, &'static str)> {
        if self.stop
            || self
                .target
                .as_ref()
                .is_none_or(|(session, _, _)| *session != job.session)
        {
            return Some((job, "thumbnail session is no longer active"));
        }
        if self
            .thumbnail_plan
            .is_some_and(|(session, serial)| session == job.session && serial > job.serial)
        {
            return Some((job, "thumbnail plan is stale"));
        }
        if self.thumbnail_plan != Some((job.session, job.serial)) {
            self.thumbnail_epoch = self.thumbnail_epoch.wrapping_add(1);
            self.thumbnail_plan = Some((job.session, job.serial));
        }
        self.thumbnail
            .replace(QueuedThumbnail {
                job,
                epoch: self.thumbnail_epoch,
            })
            .map(|replaced| (replaced.job, "thumbnail was replaced by newer work"))
    }

    fn thumbnail_relevant(&self, queued: &QueuedThumbnail) -> bool {
        !self.stop
            && queued.epoch == self.thumbnail_epoch
            && self.thumbnail_plan == Some((queued.job.session, queued.job.serial))
            && self
                .target
                .as_ref()
                .is_some_and(|(session, _, _)| *session == queued.job.session)
    }

    fn cancel_thumbnails(&mut self) -> Option<ThumbJob> {
        self.thumbnail_epoch = self.thumbnail_epoch.wrapping_add(1);
        self.thumbnail.take().map(|queued| queued.job)
    }

    fn relevant(&self, job: &Job) -> bool {
        !self.stop
            && self
                .target
                .as_ref()
                .is_some_and(|(session, path, generation)| {
                    *session == job.session && (*generation == job.generation || *path == job.path)
                })
    }

    fn request(&mut self, job: Job) {
        if !self.relevant(&job) {
            return;
        }
        let key = (job.session, job.path.clone());
        if self.running.as_ref() == Some(&key) || self.completed.contains_key(&key) {
            return;
        }
        self.pending = Some(job);
    }

    fn retarget(&mut self, session: u64, path: &std::path::Path, generation: u64) -> bool {
        self.target = Some((session, path.to_path_buf(), generation));
        let pending_matches = self
            .pending
            .as_ref()
            .is_some_and(|job| job.session == session && job.path == path);
        if pending_matches {
            if let Some(job) = &mut self.pending {
                job.generation = generation;
            }
        } else {
            self.pending = None;
        }
        let key = (session, path.to_path_buf());
        pending_matches || self.running.as_ref() == Some(&key) || self.completed.contains_key(&key)
    }

    fn finish(&mut self, job: &Job, successful: bool) -> Option<u64> {
        self.running = None;
        if !self.relevant(job) {
            return None;
        }
        let generation = match &self.target {
            Some((session, path, generation)) if *session == job.session && *path == job.path => {
                *generation
            }
            _ => job.generation,
        };
        if successful {
            self.completed
                .insert((job.session, job.path.clone()), generation);
        }
        Some(generation)
    }

    fn acknowledge(&mut self, session: u64, path: &std::path::Path, generation: u64) {
        let key = (session, path.to_path_buf());
        if self.completed.get(&key) == Some(&generation) {
            self.completed.remove(&key);
        }
    }

    fn cancel(&mut self) {
        self.pending = None;
        self.target = None;
        self.completed.clear();
    }
}
pub struct UploadWorker {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: Option<JoinHandle<()>>,
    wake_thumb: Arc<dyn Fn(ThumbFinished) + Send + Sync>,
}
impl UploadWorker {
    pub fn new(
        uploader: GpuUploader,
        budget: Arc<MemoryBudget>,
        thumbnail_budget: Arc<MemoryBudget>,
        loader: Arc<ImageLoader>,
        wake: Arc<dyn Fn(Finished) + Send + Sync>,
        wake_thumb: Arc<dyn Fn(ThumbFinished) + Send + Sync>,
    ) -> Result<Self, String> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        let thumbnail_wake = Arc::clone(&wake_thumb);
        let thread = thread::Builder::new()
            .name("fastcull-upload".into())
            .spawn(move || loop {
                let work = {
                    let (state, changed) = &*worker;
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    while state.pending.is_none() && state.thumbnail.is_none() && !state.stop {
                        state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stop {
                        break;
                    }
                    state.take_work().expect("pending work checked")
                };
                let job = match work {
                    Work::Photo(job) => job,
                    Work::Thumbnail(queued) => {
                        let relevant = {
                            let state = worker.0.lock().unwrap_or_else(|e| e.into_inner());
                            // A main request can arrive just after thumbnail
                            // dequeue; yield before touching the GPU if so.
                            state.thumbnail_relevant(&queued) && state.pending.is_none()
                        };
                        if !relevant {
                            thumbnail_wake(ThumbFinished::skipped(
                                queued.job,
                                "thumbnail upload canceled or deferred for a photo",
                            ));
                            continue;
                        }
                        // Same worker/device-global error-scope stack as main
                        // uploads. Its small budget is carved from the total,
                        // so photo uploads cannot evict visible thumbnails and
                        // thumbnails cannot take the photo pipeline's headroom.
                        // Never evict photo caches or wait for a reservation.
                        let result = uploader.upload(
                            Arc::clone(&queued.job.image),
                            queued.job.orientation,
                            Arc::clone(&thumbnail_budget),
                        );
                        let relevant = worker
                            .0
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .thumbnail_relevant(&queued);
                        if !relevant {
                            drop(result); // Retire a newly stale GPU allocation.
                            thumbnail_wake(ThumbFinished::skipped(
                                queued.job,
                                "thumbnail upload canceled",
                            ));
                        } else {
                            let deferred = matches!(result, Err(UploadError::Budget { .. }));
                            thumbnail_wake(ThumbFinished::new(
                                queued.job,
                                result.map_err(|error| error.to_string()),
                                deferred,
                            ));
                        }
                        continue;
                    }
                };
                let start = Instant::now();
                let result = loop {
                    {
                        let mut state = worker.0.lock().unwrap_or_else(|e| e.into_inner());
                        if !state.relevant(&job) {
                            // Clear running under the same lock as cancellation.
                            // A rapid return can then queue a fresh job instead
                            // of waiting on a canceled job's error notification.
                            state.running = None;
                            break None;
                        }
                    }
                    if let Ok(needed) = GpuUploader::bytes_needed(job.image.width, job.image.height)
                    {
                        loader.evict_for(needed);
                    }
                    match uploader.upload(
                        Arc::clone(&job.image),
                        job.orientation,
                        Arc::clone(&budget),
                    ) {
                        Err(UploadError::Budget { .. })
                            if start.elapsed() < Duration::from_secs(3) =>
                        {
                            // A canceled decode or submitted frame may still own its lease.
                            // Only this worker waits; events and cached navigation stay live.
                            let (state, changed) = &*worker;
                            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if state.stop {
                                state.running = None;
                                break None;
                            }
                            drop(
                                changed
                                    .wait_timeout(state, Duration::from_millis(15))
                                    .unwrap_or_else(|e| e.into_inner()),
                            );
                        }
                        result => break Some(result.map_err(|e| e.to_string())),
                    }
                };
                let Some(result) = result else {
                    continue;
                };
                let finished_generation = {
                    let mut state = worker.0.lock().unwrap_or_else(|e| e.into_inner());
                    state.finish(&job, result.is_ok())
                };
                let Some(finished_generation) = finished_generation else {
                    continue;
                };
                // A next-photo prefetch can become the foreground during the GPU
                // copy. Publish by path so the UI can promote that completed work.
                wake(Finished {
                    session: job.session,
                    generation: finished_generation,
                    path: job.path,
                    result,
                });
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            thread: Some(thread),
            wake_thumb,
        })
    }
    pub fn request(&self, job: Job) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request(job);
        self.shared.1.notify_one();
    }
    /// At most one queued thumbnail owns CPU pixels. Replacement/cancellation
    /// always acknowledges the old job, so a producer waiting for it can resume.
    pub fn request_thumbnail(&self, job: ThumbJob) {
        let displaced = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_thumbnail(job);
        if let Some((job, reason)) = displaced {
            (self.wake_thumb)(ThumbFinished::skipped(job, reason));
        }
        self.shared.1.notify_one();
    }
    pub fn cancel_thumbnails(&self) {
        let canceled = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_thumbnails();
        if let Some(job) = canceled {
            (self.wake_thumb)(ThumbFinished::skipped(job, "thumbnail upload canceled"));
        }
        self.shared.1.notify_one();
    }
    pub fn cancel(&self) {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        state.cancel();
        let canceled = state.cancel_thumbnails();
        drop(state);
        if let Some(job) = canceled {
            (self.wake_thumb)(ThumbFinished::skipped(job, "thumbnail upload canceled"));
        }
        self.shared.1.notify_one();
    }
    /// Keep an already-running upload if the user just requested its photo.
    pub fn retarget(&self, session: u64, path: &std::path::Path, generation: u64) -> bool {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        let canceled = state.cancel_thumbnails();
        let retained = state.retarget(session, path, generation);
        drop(state);
        if let Some(job) = canceled {
            (self.wake_thumb)(ThumbFinished::skipped(
                job,
                "thumbnail upload canceled by navigation",
            ));
        }
        self.shared.1.notify_one();
        retained
    }
    pub fn acknowledge(&self, session: u64, path: &std::path::Path, generation: u64) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acknowledge(session, path, generation);
    }
}
impl Drop for UploadWorker {
    fn drop(&mut self) {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        state.stop = true;
        state.pending = None;
        let canceled = state.cancel_thumbnails();
        drop(state);
        if let Some(job) = canceled {
            (self.wake_thumb)(ThumbFinished::skipped(
                job,
                "thumbnail upload worker stopped",
            ));
        }
        self.shared.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn job(session: u64, generation: u64, path: &str) -> Job {
        let budget = MemoryBudget::new(4);
        let lease = budget.try_reserve(4).unwrap();
        let image = DecodedImage::new(vec![0; 4], 1, 1, 1, 1, 1, lease).unwrap();
        Job {
            session,
            generation,
            path: path.into(),
            image: Arc::new(image),
            orientation: 1,
        }
    }

    fn begin(state: &mut State) -> Job {
        match state.take_work().unwrap() {
            Work::Photo(job) => job,
            Work::Thumbnail(_) => panic!("expected photo upload"),
        }
    }

    fn thumb(session: u64, serial: u64, index: usize) -> ThumbJob {
        let image = job(session, serial, "thumbnail.ARW");
        ThumbJob {
            session,
            serial,
            index,
            path: image.path,
            image: image.image,
            orientation: 1,
        }
    }

    fn begin_thumb(state: &mut State) -> QueuedThumbnail {
        match state.take_work().unwrap() {
            Work::Thumbnail(job) => job,
            Work::Photo(_) => panic!("expected thumbnail upload"),
        }
    }

    #[test]
    fn retarget_promotes_running_prefetch_without_duplicate_upload() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        state.request(job(1, 1, "B.ARW"));
        let running = begin(&mut state);
        assert!(state.retarget(1, Path::new("B.ARW"), 2));
        assert!(state.relevant(&running));
        state.request(job(1, 2, "B.ARW"));
        assert!(state.pending.is_none());
        assert_eq!(state.finish(&running, true), Some(2));
    }

    #[test]
    fn completed_event_remains_promotable_until_acknowledged() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        state.request(job(1, 1, "B.ARW"));
        let running = begin(&mut state);
        assert_eq!(state.finish(&running, true), Some(1));
        assert!(state.retarget(1, Path::new("B.ARW"), 2));
        state.request(job(1, 2, "B.ARW"));
        assert!(state.pending.is_none());
        state.acknowledge(1, Path::new("B.ARW"), 0);
        assert!(!state.completed.is_empty());
        state.acknowledge(1, Path::new("B.ARW"), 1);
        assert!(state.completed.is_empty());
    }

    #[test]
    fn pending_promotion_updates_generation_and_releases_replaced_image() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        let pending = job(1, 1, "B.ARW");
        let weak = Arc::downgrade(&pending.image);
        state.request(pending);
        assert!(state.retarget(1, Path::new("B.ARW"), 2));
        assert_eq!(state.pending.as_ref().unwrap().generation, 2);
        assert!(!state.retarget(1, Path::new("C.ARW"), 3));
        assert!(state.pending.is_none());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn cancellation_never_becomes_a_current_photo_failure() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        state.request(job(1, 1, "B.ARW"));
        let running = begin(&mut state);
        state.retarget(1, Path::new("C.ARW"), 2);
        assert!(!state.relevant(&running));
        assert_eq!(state.finish(&running, false), None);
        assert!(!state.retarget(1, Path::new("B.ARW"), 3));
        state.request(job(1, 3, "B.ARW"));
        assert!(state.pending.is_some());
    }

    #[test]
    fn session_changes_and_explicit_cancel_release_all_pending_ownership() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        state.request(job(1, 1, "A.ARW"));
        let running = begin(&mut state);
        state.retarget(2, Path::new("A.ARW"), 2);
        assert!(!state.relevant(&running));
        assert_eq!(state.finish(&running, true), None);
        let pending = job(2, 2, "A.ARW");
        let weak = Arc::downgrade(&pending.image);
        state.request(pending);
        state.cancel();
        assert!(weak.upgrade().is_none());
        assert!(state.target.is_none());
        assert!(state.completed.is_empty());
    }

    #[test]
    fn foreground_and_photo_prefetch_both_outrank_a_queued_thumbnail() {
        for path in ["A.ARW", "B.ARW"] {
            let mut state = State::default();
            state.retarget(1, Path::new("A.ARW"), 7);
            assert!(state.request_thumbnail(thumb(1, 2, 0)).is_none());
            state.request(job(1, 7, path));
            let photo = begin(&mut state);
            assert_eq!(photo.path, Path::new(path));
            assert!(state.thumbnail.is_some());
            assert_eq!(state.finish(&photo, false), Some(7));
            let thumbnail = begin_thumb(&mut state);
            assert!(state.thumbnail_relevant(&thumbnail));
            assert_eq!(thumbnail.job.index, 0);
        }
    }

    #[test]
    fn thumbnail_replacement_and_cancellation_release_cpu_ownership_and_acknowledge_ids() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        let first = thumb(1, 4, 2);
        let first_pixels = Arc::downgrade(&first.image);
        assert!(state.request_thumbnail(first).is_none());
        let second = thumb(1, 4, 3);
        let second_pixels = Arc::downgrade(&second.image);
        let (replaced, reason) = state.request_thumbnail(second).unwrap();
        let completion = ThumbFinished::skipped(replaced, reason);
        assert!(completion.deferred && completion.result.is_err());
        assert_eq!(
            (completion.session, completion.serial, completion.index),
            (1, 4, 2)
        );
        assert!(first_pixels.upgrade().is_none());
        assert_eq!(state.thumbnail.as_ref().unwrap().job.index, 3);
        let canceled = state.cancel_thumbnails().unwrap();
        let completion = ThumbFinished::skipped(canceled, "canceled");
        assert_eq!(completion.index, 3);
        assert!(second_pixels.upgrade().is_none());
        assert!(state.thumbnail.is_none());
    }

    #[test]
    fn in_flight_thumbnail_is_invalidated_by_cancellation_and_newer_plan() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        assert!(state.request_thumbnail(thumb(1, 4, 2)).is_none());
        let running = begin_thumb(&mut state);
        assert!(state.thumbnail_relevant(&running));
        assert!(state.cancel_thumbnails().is_none());
        assert!(!state.thumbnail_relevant(&running));
        assert!(state.request_thumbnail(thumb(1, 5, 3)).is_none());
        let running = begin_thumb(&mut state);
        assert!(state.thumbnail_relevant(&running));
        assert!(state.request_thumbnail(thumb(1, 6, 4)).is_none());
        assert!(!state.thumbnail_relevant(&running));
        assert_eq!(state.thumbnail.as_ref().unwrap().job.serial, 6);
    }

    #[test]
    fn stale_serial_or_wrong_session_never_replaces_current_thumbnail_work() {
        let mut state = State::default();
        state.retarget(3, Path::new("A.ARW"), 2);
        assert!(state.request_thumbnail(thumb(3, 10, 5)).is_none());
        for stale in [thumb(3, 9, 4), thumb(2, 100, 4)] {
            let weak = Arc::downgrade(&stale.image);
            let (rejected, reason) = state.request_thumbnail(stale).unwrap();
            let completion = ThumbFinished::skipped(rejected, reason);
            assert!(completion.deferred);
            assert!(weak.upgrade().is_none());
            let current = state.thumbnail.as_ref().unwrap();
            assert_eq!(
                (current.job.session, current.job.serial, current.job.index),
                (3, 10, 5)
            );
        }
        let running = begin_thumb(&mut state);
        state.retarget(4, Path::new("A.ARW"), 3);
        assert!(!state.thumbnail_relevant(&running));
    }

    #[test]
    fn stopping_never_dequeues_thumbnail_work() {
        let mut state = State::default();
        state.retarget(1, Path::new("A.ARW"), 1);
        assert!(state.request_thumbnail(thumb(1, 1, 0)).is_none());
        state.stop = true;
        assert!(state.take_work().is_none());
        let canceled = state.cancel_thumbnails().unwrap();
        let weak = Arc::downgrade(&canceled.image);
        let completion = ThumbFinished::skipped(canceled, "worker stopped");
        assert!(completion.deferred && completion.result.is_err());
        assert!(weak.upgrade().is_none());
    }
}
