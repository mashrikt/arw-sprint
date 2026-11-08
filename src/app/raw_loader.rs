//! One cancellable RAW job at a time. No RAW work occurs in normal preview mode.
use crate::image::{
    cache::{DecodedImage, MemoryBudget},
    loader::ImageLoader,
    raw::RawDecoder,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub session: u64,
    pub path: PathBuf,
}
pub struct Finished {
    pub request: Request,
    pub result: Result<(Arc<DecodedImage>, u16), String>,
    pub elapsed_ms: f64,
}
#[derive(Default)]
struct State {
    pending: Option<QueuedRequest>,
    stop: bool,
}
struct QueuedRequest {
    request: Request,
    epoch: u64,
}
type Decoded = Result<(Arc<DecodedImage>, u16), String>;
pub struct Worker {
    shared: Arc<(Mutex<State>, Condvar)>,
    latest: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn new(
        budget: Arc<MemoryBudget>,
        loader: Arc<ImageLoader>,
        wake: Arc<dyn Fn(Finished) + Send + Sync>,
    ) -> Result<Self, String> {
        // Native working memory is temporary and independent of the normal
        // JPEG/CPU/GPU pool. Only one decoder can hold this allowance.
        let scratch = MemoryBudget::new(384 * 1024 * 1024);
        Self::spawn(
            move |request, cancelled| {
                if cancelled() {
                    return Err("RAW request cancelled".into());
                }
                // Drop the decoder after each job, including errors, so
                // native buffers never remain hidden in an idle worker.
                let mut decoder = RawDecoder::new()?;
                let info = decoder.probe(&request.path)?;
                if cancelled() {
                    return Err("RAW request cancelled".into());
                }
                let scratch_bytes = info
                    .working_bytes
                    .checked_sub(info.bytes)
                    .ok_or("invalid RAW memory estimate")?;
                let _scratch = scratch
                    .try_reserve(scratch_bytes)
                    .ok_or("RAW working memory exceeds the 384 MiB limit")?;
                // Preview decodes already in flight and recently submitted
                // frames can briefly hold leases after the UI pauses prefetch.
                // Wait only here, never on the event or GPU upload threads.
                let deadline = Instant::now() + std::time::Duration::from_secs(1);
                let output = loop {
                    if cancelled() {
                        return Err("RAW request cancelled".into());
                    }
                    loader.evict_for(info.bytes);
                    if let Some(output) = budget.try_reserve(info.bytes) {
                        break output;
                    }
                    if Instant::now() >= deadline {
                        return Err(
                            "Not enough memory for RAW. Close comparison or use a larger cache."
                                .into(),
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(15));
                };
                let raw = decoder.decode_cancellable(&request.path, cancelled)?;
                if cancelled() {
                    return Err("RAW request cancelled".into());
                }
                let orientation = raw.info.orientation;
                let image = DecodedImage::new(
                    raw.pixels,
                    raw.info.width,
                    raw.info.height,
                    raw.info.width,
                    raw.info.height,
                    1,
                    output,
                )?;
                Ok((Arc::new(image), orientation))
            },
            wake,
        )
    }
    fn spawn(
        mut decode: impl FnMut(&Request, &dyn Fn() -> bool) -> Decoded + Send + 'static,
        wake: Arc<dyn Fn(Finished) + Send + Sync>,
    ) -> Result<Self, String> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let latest = Arc::new(AtomicU64::new(0));
        let state = Arc::clone(&shared);
        let serial = Arc::clone(&latest);
        let thread = thread::Builder::new()
            .name("arw-sprint-raw".into())
            .spawn(move || loop {
                let QueuedRequest { request, epoch } = {
                    let (lock, changed) = &*state;
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.pending.is_none() && !state.stop {
                        state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stop {
                        break;
                    }
                    state.pending.take().expect("pending job checked")
                };
                let start = Instant::now();
                let cancelled = || serial.load(Ordering::Acquire) != epoch;
                let result = decode(&request, &cancelled);
                if !cancelled() {
                    wake(Finished {
                        request,
                        result,
                        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
                    });
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            latest,
            thread: Some(thread),
        })
    }
    pub fn request(&self, request: Request) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        // Public IDs can restart for a new folder. An internal epoch prevents
        // an old request from becoming current again when those IDs repeat.
        let epoch = self.latest.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        state.pending = Some(QueuedRequest { request, epoch });
        changed.notify_one();
    }
    pub fn cancel(&self) {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        self.latest.fetch_add(1, Ordering::AcqRel);
        state.pending = None;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        self.shared.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    const WAIT: Duration = Duration::from_secs(5);

    fn request(id: u64) -> Request {
        Request {
            id,
            session: 7,
            path: format!("synthetic-{id}.ARW").into(),
        }
    }

    fn image(budget: &Arc<MemoryBudget>) -> Arc<DecodedImage> {
        Arc::new(
            DecodedImage::new(vec![0; 4], 1, 1, 1, 1, 1, budget.try_reserve(4).unwrap()).unwrap(),
        )
    }

    #[test]
    fn worker_discards_running_result_and_decodes_only_the_latest_pending_request() {
        let budget = MemoryBudget::new(8);
        let decode_budget = Arc::clone(&budget);
        let (started, starts) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (events, results) = mpsc::channel();
        let worker = Worker::spawn(
            move |request, cancelled| {
                let pixels = image(&decode_budget);
                started.send(request.id).unwrap();
                if request.id == 1 {
                    released.recv_timeout(WAIT).unwrap();
                    assert!(cancelled());
                }
                Ok((pixels, 1))
            },
            Arc::new(move |event| events.send(event).unwrap()),
        )
        .unwrap();
        worker.request(request(1));
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 1);
        worker.request(request(2));
        worker.request(request(3));
        release.send(()).unwrap();
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 3);
        let result = results.recv_timeout(WAIT).unwrap();
        assert_eq!(result.request, request(3));
        assert!(result.result.is_ok());
        assert_eq!(
            budget.used(),
            4,
            "the stale decoded image must release its lease"
        );
        drop(result);
        drop(worker);
        assert_eq!(budget.used(), 0);
        assert!(
            starts.try_recv().is_err(),
            "the replaced request must never decode"
        );
        assert!(
            results.try_recv().is_err(),
            "a stale completion must never publish"
        );
    }

    #[test]
    fn cancel_removes_queued_work_and_drops_a_running_result_before_later_requests() {
        let budget = MemoryBudget::new(8);
        let decode_budget = Arc::clone(&budget);
        let (started, starts) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (events, results) = mpsc::channel();
        let worker = Worker::spawn(
            move |request, cancelled| {
                let pixels = image(&decode_budget);
                started.send(request.id).unwrap();
                if request.id == 1 {
                    released.recv_timeout(WAIT).unwrap();
                    assert!(cancelled());
                }
                Ok((pixels, 1))
            },
            Arc::new(move |event| events.send(event).unwrap()),
        )
        .unwrap();
        worker.request(request(1));
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 1);
        worker.request(request(2));
        let before_cancel = worker.latest.load(Ordering::Acquire);
        worker.cancel();
        assert!(worker.shared.0.lock().unwrap().pending.is_none());
        assert_ne!(worker.latest.load(Ordering::Acquire), before_cancel);
        worker.request(request(4));
        release.send(()).unwrap();
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 4);
        let result = results.recv_timeout(WAIT).unwrap();
        assert_eq!(result.request, request(4));
        drop(result);
        drop(worker);
        assert_eq!(budget.used(), 0);
        assert!(starts.try_recv().is_err());
        assert!(results.try_recv().is_err());
    }

    #[test]
    fn dropping_worker_cancels_the_decoder_joins_and_releases_all_image_memory() {
        let budget = MemoryBudget::new(4);
        let decode_budget = Arc::clone(&budget);
        let (started, starts) = mpsc::channel();
        let (events, results) = mpsc::channel();
        let (stopped, stops) = mpsc::channel();
        let worker = Worker::spawn(
            move |request, cancelled| {
                let pixels = image(&decode_budget);
                started.send(request.id).unwrap();
                let deadline = Instant::now() + WAIT;
                while !cancelled() && Instant::now() < deadline {
                    thread::park_timeout(Duration::from_millis(1));
                }
                assert!(cancelled(), "dropping the worker must signal cancellation");
                stopped.send(()).unwrap();
                Ok((pixels, 1))
            },
            Arc::new(move |event| events.send(event).unwrap()),
        )
        .unwrap();
        worker.request(request(1));
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 1);
        drop(worker);
        stops.recv_timeout(WAIT).unwrap();
        assert_eq!(budget.used(), 0);
        assert!(starts.try_recv().is_err());
        assert!(results.try_recv().is_err());
    }

    #[test]
    fn new_session_reusing_public_id_cannot_revive_the_old_running_decode() {
        let budget = MemoryBudget::new(8);
        let decode_budget = Arc::clone(&budget);
        let (started, starts) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (events, results) = mpsc::channel();
        let worker = Worker::spawn(
            move |request, cancelled| {
                let pixels = image(&decode_budget);
                started.send(request.session).unwrap();
                if request.session == 7 {
                    released.recv_timeout(WAIT).unwrap();
                    assert!(
                        cancelled(),
                        "the same public ID in a new session is different work"
                    );
                }
                Ok((pixels, 1))
            },
            Arc::new(move |event| events.send(event).unwrap()),
        )
        .unwrap();
        worker.request(request(1));
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 7);
        worker.cancel();
        let new = Request {
            session: 8,
            ..request(1)
        };
        worker.request(new.clone());
        release.send(()).unwrap();
        assert_eq!(starts.recv_timeout(WAIT).unwrap(), 8);
        let result = results.recv_timeout(WAIT).unwrap();
        assert_eq!(result.request, new);
        drop(result);
        drop(worker);
        assert_eq!(budget.used(), 0);
        assert!(results.try_recv().is_err());
    }
}
