//! Filesystem mutations are serialized separately from image workers. Ratings,
//! moves, and restores share one FIFO above optional metadata, so a queued
//! sidecar save cannot overtake a move or recreate a moved photo's old sidecar.
use super::deleted::{self, MoveError, MoveRecord};
use crate::{
    metadata::{read_exif, ExifMetadata},
    xmp,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

pub enum IoEvent {
    Ratings {
        folder_epoch: u64,
        entries: Vec<(PathBuf, Result<i8, String>)>,
        complete: bool,
    },
    Metadata {
        session: u64,
        path: PathBuf,
        metadata: Result<ExifMetadata, String>,
        rating: Result<Option<i8>, String>,
    },
    Saved {
        session: u64,
        path: PathBuf,
        revision: u64,
        edit_id: u64,
        undo: bool,
        /// Meaningful on success; None is an absent XMP Rating property.
        rating: Option<i8>,
        /// A rejected undo must not erase an earlier, still-unsaved normal edit.
        unsaved: bool,
        result: Result<(), String>,
    },
    Rejected {
        session: u64,
        result: Result<Vec<PathBuf>, String>,
    },
    Trashed {
        session: u64,
        removed: Vec<PathBuf>,
        errors: Vec<String>,
    },
    MovedDeleted {
        session: u64,
        op_id: u64,
        records: Vec<MoveRecord>,
        errors: Vec<(PathBuf, String)>,
    },
    RestoredDeleted {
        session: u64,
        op_id: u64,
        restored: Vec<MoveRecord>,
        ratings: Vec<(PathBuf, Result<i8, String>)>,
        failed: Vec<(MoveRecord, String)>,
    },
    Flushed {
        failures: usize,
    },
}

#[derive(Clone)]
struct Write {
    session: u64,
    revision: u64,
    edit_id: u64,
    operation: Edit,
}
#[derive(Clone, Copy)]
enum Edit {
    Set(i8),
    Undo,
}

#[derive(Clone)]
struct UndoRecord {
    edit_id: u64,
    path: PathBuf,
    previous: Option<i8>,
    rating: i8,
}
#[derive(Default)]
struct History(VecDeque<UndoRecord>);
impl History {
    fn record(&mut self, record: UndoRecord) {
        self.0.retain(|entry| entry.edit_id != record.edit_id);
        self.0.push_back(record);
        while self.0.len() > 100 {
            self.0.pop_front();
        }
    }
    fn target(&self, path: &std::path::Path, edit_id: u64) -> Result<UndoRecord, String> {
        let record = self.0.iter().rev().find(|entry| entry.path == path).ok_or(
            "Rating undo is unavailable: the edit failed, expired, or was already undone.",
        )?;
        if record.edit_id != edit_id {
            return Err("Cannot undo a rating while a newer edit to that photo remains.".into());
        }
        Ok(record.clone())
    }
    fn remove(&mut self, edit_id: u64) {
        self.0.retain(|entry| entry.edit_id != edit_id);
    }
}
enum Batch {
    Rejected(u64, Arc<Vec<PathBuf>>),
    Trash(u64, Vec<PathBuf>),
}
enum Mutation {
    Rating(PathBuf, Write),
    MoveDeleted {
        session: u64,
        op_id: u64,
        paths: Vec<PathBuf>,
    },
    RestoreDeleted {
        session: u64,
        op_id: u64,
        records: Vec<MoveRecord>,
    },
}
struct RatingScan {
    folder_epoch: u64,
    serial: u64,
    files: Arc<Vec<PathBuf>>,
    next: usize,
}
#[derive(Default)]
struct State {
    mutations: VecDeque<Mutation>,
    failed: BTreeMap<PathBuf, Write>,
    read: Option<(u64, PathBuf)>,
    batches: VecDeque<Batch>,
    rating_scan: Option<RatingScan>,
    rating_scan_serial: u64,
    flush: bool,
    stop: bool,
}
impl State {
    fn cancel_rating_scan(&mut self) {
        self.rating_scan_serial = self.rating_scan_serial.wrapping_add(1);
        self.rating_scan = None;
    }
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
pub struct IoWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl IoWorker {
    pub fn new(wake: Arc<dyn Fn(IoEvent) + Send + Sync>) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("fastcull-sidecars".into())
            .spawn(move || worker(worker_shared, wake))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
    pub fn read(&self, session: u64, path: PathBuf) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read = Some((session, path));
        self.shared.changed.notify_one();
    }
    pub fn save(&self, session: u64, path: PathBuf, revision: u64, rating: i8, edit_id: u64) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mutations
            .push_back(Mutation::Rating(
                path,
                Write {
                    session,
                    revision,
                    edit_id,
                    operation: Edit::Set(rating),
                },
            ));
        self.shared.changed.notify_one();
    }
    /// The target may still be queued. FIFO execution captures its old value
    /// before this command runs, independently of UI metadata/read completion.
    pub fn undo(&self, session: u64, path: PathBuf, revision: u64, edit_id: u64) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mutations
            .push_back(Mutation::Rating(
                path,
                Write {
                    session,
                    revision,
                    edit_id,
                    operation: Edit::Undo,
                },
            ));
        self.shared.changed.notify_one();
    }
    /// Queue behind prior rating edits and ahead of later ones. Successful
    /// moves preserve rating-undo history, but block edits at the old location.
    pub fn move_deleted(&self, session: u64, op_id: u64, paths: Vec<PathBuf>) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mutations
            .push_back(Mutation::MoveDeleted {
                session,
                op_id,
                paths,
            });
        self.shared.changed.notify_one();
    }

    /// Restore is ordered with moves/saves and removes the old-path tombstone
    /// only after the mover verifies and restores the complete RAW/XMP pair.
    pub fn restore_deleted(&self, session: u64, op_id: u64, records: Vec<MoveRecord>) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mutations
            .push_back(Mutation::RestoreDeleted {
                session,
                op_id,
                records,
            });
        self.shared.changed.notify_one();
    }

    /// Replace the optional background rating index. Only sibling XMP files
    /// are read: RAW existence, EXIF, and JPEG decoding are not prerequisites.
    pub fn scan_ratings(&self, folder_epoch: u64, files: Arc<Vec<PathBuf>>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.cancel_rating_scan();
        state.rating_scan = Some(RatingScan {
            folder_epoch,
            serial: state.rating_scan_serial,
            files,
            next: 0,
        });
        self.shared.changed.notify_one();
    }
    /// An event already posted to the UI can still arrive; consumers must use
    /// folder_epoch to reject results from previous folders.
    pub fn cancel_rating_scan(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_rating_scan();
        self.shared.changed.notify_one();
    }
    pub fn flush(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.flush = true;
        state.read = None;
        state.cancel_rating_scan();
        self.shared.changed.notify_one();
    }
    pub fn retry_failed(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let failed = state.failed.clone();
        for (path, write) in failed {
            if !state.mutations.iter().any(|queued| {
                matches!(queued,
                Mutation::Rating(queued, _) if queued == &path)
            }) {
                state.mutations.push_back(Mutation::Rating(path, write));
            }
        }
        state.flush = true;
        state.cancel_rating_scan();
        self.shared.changed.notify_one();
    }
    pub fn collect_rejected(&self, session: u64, files: Arc<Vec<PathBuf>>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.batches.len() < 2 {
            state.batches.push_back(Batch::Rejected(session, files));
        }
        self.shared.changed.notify_one();
    }
    /// Call only after the user confirms the exact set/count in a modal dialog.
    pub fn trash_confirmed(&self, session: u64, paths: Vec<PathBuf>) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.batches.len() < 2 {
            state.batches.push_back(Batch::Trash(session, paths));
        }
        self.shared.changed.notify_one();
    }
}
impl Drop for IoWorker {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stop = true;
        state.read = None;
        state.batches.clear();
        state.cancel_rating_scan();
        drop(state);
        self.shared.changed.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Work {
    Mutation(Mutation),
    Read(u64, PathBuf),
    Batch(Batch),
    Ratings(RatingScan),
    Flush(usize),
    Stop,
}

fn move_error(error: &MoveError) -> String {
    if !error.partial {
        return error.to_string();
    }
    format!(
        "{}; partial operation; RAW location: {}; XMP location: {}. Rating writes at the original path are blocked for safety.",
        error.message,
        error.raw_path.as_ref().map_or_else(|| "unknown or missing".into(), |path| path.display().to_string()),
        error.sidecar_path.as_ref().map_or_else(|| "unknown or absent".into(), |path| path.display().to_string()),
    )
}

fn worker(shared: Arc<Shared>, wake: Arc<dyn Fn(IoEvent) + Send + Sync>) {
    let mut history = History::default();
    let mut moved_paths = BTreeMap::<PathBuf, String>::new();
    loop {
        let work = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(mutation) = state.mutations.pop_front() {
                    break Work::Mutation(mutation);
                }
                if state.stop {
                    break Work::Stop;
                }
                if state.flush {
                    state.flush = false;
                    break Work::Flush(state.failed.len());
                }
                if let Some((session, path)) = state.read.take() {
                    break Work::Read(session, path);
                }
                if let Some(batch) = state.batches.pop_front() {
                    break Work::Batch(batch);
                }
                if let Some(scan) = state.rating_scan.take() {
                    break Work::Ratings(scan);
                }
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        match work {
            Work::Stop => break,
            Work::Mutation(Mutation::Rating(path, write)) => {
                let undo = matches!(write.operation, Edit::Undo);
                let mut rating = None;
                let result = if let Some(reason) = moved_paths.get(&path) {
                    Err(format!(
                        "Rating write blocked until the photo is restored: {reason}"
                    ))
                } else {
                    match write.operation {
                        Edit::Set(desired) => {
                            rating = Some(desired);
                            xmp::replace_rating(&path, Some(desired), None)
                                .map(|previous| {
                                    history.record(UndoRecord {
                                        edit_id: write.edit_id,
                                        path: path.clone(),
                                        previous,
                                        rating: desired,
                                    });
                                })
                                .map_err(|e| e.to_string())
                        }
                        Edit::Undo => history.target(&path, write.edit_id).and_then(|record| {
                            rating = record.previous;
                            xmp::replace_rating(&path, record.previous, Some(Some(record.rating)))
                                .map(|_| history.remove(write.edit_id))
                                .map_err(|e| e.to_string())
                        }),
                    }
                };
                let unsaved = {
                    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    if result.is_err() && !undo {
                        state.failed.insert(path.clone(), write.clone());
                    } else if result.is_ok() {
                        state.failed.remove(&path);
                    }
                    state.failed.contains_key(&path)
                };
                wake(IoEvent::Saved {
                    session: write.session,
                    path,
                    revision: write.revision,
                    edit_id: write.edit_id,
                    undo,
                    rating,
                    unsaved,
                    result,
                });
            }
            Work::Mutation(Mutation::MoveDeleted {
                session,
                op_id,
                paths,
            }) => {
                let mut records = Vec::new();
                let mut errors = Vec::new();
                let mut seen = HashSet::new();
                for path in paths {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    if let Some(reason) = moved_paths.get(&path) {
                        errors.push((
                            path,
                            format!("Photo must be restored before another move: {reason}"),
                        ));
                        continue;
                    }
                    let failed_save = shared
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .failed
                        .contains_key(&path);
                    if failed_save {
                        errors.push((path, "This photo has an unsaved rating. Retry that save before moving it to _Rejected.".into()));
                        continue;
                    }
                    match deleted::move_photo(&path) {
                        Ok(record) => {
                            moved_paths.insert(
                                record.source.clone(),
                                format!("moved to {}", record.destination.display()),
                            );
                            records.push(record);
                        }
                        Err(error) => {
                            let message = move_error(&error);
                            // Even if RAW stayed at source, a failed rollback
                            // can leave XMP elsewhere. Never recreate a second
                            // sidecar beside the source in that partial state.
                            if error.partial {
                                moved_paths.insert(path.clone(), message.clone());
                            }
                            errors.push((path, message));
                        }
                    }
                }
                wake(IoEvent::MovedDeleted {
                    session,
                    op_id,
                    records,
                    errors,
                });
            }
            Work::Mutation(Mutation::RestoreDeleted {
                session,
                op_id,
                records,
            }) => {
                let mut restored = Vec::new();
                let mut ratings = Vec::new();
                let mut failed = Vec::new();
                for record in records {
                    match deleted::restore_photo(&record) {
                        Ok(()) => {
                            moved_paths.remove(&record.source);
                            // Refresh only restored photos, including metadata
                            // edited while reviewing the _Rejected folder. This
                            // keeps a completed star filter valid without a
                            // folder-wide sidecar rescan on every undo.
                            ratings.push((
                                record.source.clone(),
                                xmp::read_rating(&record.source)
                                    .map(|rating| rating.unwrap_or(0))
                                    .map_err(|error| error.to_string()),
                            ));
                            restored.push(record);
                        }
                        Err(error) => {
                            let message = move_error(&error);
                            if error.partial {
                                moved_paths.insert(record.source.clone(), message.clone());
                            }
                            failed.push((record, message));
                        }
                    }
                }
                wake(IoEvent::RestoredDeleted {
                    session,
                    op_id,
                    restored,
                    ratings,
                    failed,
                });
            }
            Work::Read(session, path) => {
                let metadata = read_exif(&path).map_err(|e| e.to_string());
                let rating = xmp::read_rating(&path).map_err(|e| e.to_string());
                wake(IoEvent::Metadata {
                    session,
                    path,
                    metadata,
                    rating,
                });
            }
            Work::Flush(failures) => wake(IoEvent::Flushed { failures }),
            Work::Ratings(mut scan) => {
                let mut entries = Vec::with_capacity(32);
                while scan.next < scan.files.len() && entries.len() < 32 {
                    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    if scan.serial != state.rating_scan_serial || state.stop {
                        break;
                    }
                    // Yield even before a chunk is full when foreground I/O
                    // arrives. A single sidecar read is the non-preemptible unit.
                    if !state.mutations.is_empty()
                        || state.flush
                        || state.read.is_some()
                        || !state.batches.is_empty()
                    {
                        break;
                    }
                    drop(state);
                    let path = &scan.files[scan.next];
                    let rating = xmp::read_rating(path)
                        .map(|value| value.unwrap_or(0))
                        .map_err(|error| error.to_string());
                    entries.push((path.clone(), rating));
                    scan.next += 1;
                }
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if scan.serial != state.rating_scan_serial || state.stop {
                    continue;
                }
                let complete = scan.next == scan.files.len();
                let folder_epoch = scan.folder_epoch;
                if !complete {
                    state.rating_scan = Some(scan);
                }
                drop(state);
                if complete || !entries.is_empty() {
                    wake(IoEvent::Ratings {
                        folder_epoch,
                        entries,
                        complete,
                    });
                }
            }
            Work::Batch(Batch::Rejected(session, files)) => {
                let failed = !shared
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failed
                    .is_empty();
                let result = if failed {
                    Err(
                        "Some ratings could not be saved. Retry those saves before moving photos."
                            .into(),
                    )
                } else {
                    files.iter().try_fold(Vec::new(), |mut rejected, path| {
                        if xmp::read_rating(path).map_err(|e| format!("{}: {e}", path.display()))?
                            == Some(-1)
                        {
                            rejected.push(path.clone());
                        }
                        Ok(rejected)
                    })
                };
                wake(IoEvent::Rejected { session, result });
            }
            Work::Batch(Batch::Trash(session, paths)) => {
                use trash::macos::{DeleteMethod, TrashContextExtMacos};
                let mut context = trash::TrashContext::default();
                context.set_delete_method(DeleteMethod::NsFileManager);
                let mut removed = Vec::new();
                let mut errors = Vec::new();
                for path in paths {
                    let result = (|| {
                        if !std::fs::symlink_metadata(&path)
                            .map_err(|e| e.to_string())?
                            .is_file()
                            || !crate::browser::directory::is_arw(&path)
                        {
                            return Err("not a regular ARW file".into());
                        }
                        if xmp::read_rating(&path).map_err(|e| e.to_string())? != Some(-1) {
                            return Err("photo is no longer marked rejected".into());
                        }
                        context.delete_all([&path]).map_err(|e| e.to_string())
                    })();
                    match result {
                        Ok(()) => removed.push(path),
                        Err(error) => errors.push(format!("{}: {error}", path.display())),
                    }
                }
                wake(IoEvent::Trashed {
                    session,
                    removed,
                    errors,
                });
            }
        }
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
        time::Duration,
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fastcull-io-test-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn raw(&self) -> PathBuf {
            let path = self.0.join("synthetic.ARW");
            fs::write(&path, b"synthetic RAW remains unchanged").unwrap();
            path
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn next(receiver: &mpsc::Receiver<IoEvent>) -> IoEvent {
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("I/O worker did not complete within ten seconds")
    }

    fn moved(receiver: &mpsc::Receiver<IoEvent>, expected_op: u64) -> MoveRecord {
        let IoEvent::MovedDeleted {
            op_id,
            mut records,
            errors,
            ..
        } = next(receiver)
        else {
            panic!("expected move completion");
        };
        assert_eq!(op_id, expected_op);
        assert!(errors.is_empty(), "move failed: {errors:?}");
        assert_eq!(records.len(), 1);
        records.pop().unwrap()
    }

    #[test]
    fn move_is_fifo_after_pending_rating_and_blocks_later_orphan_sidecar_writes() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let io = IoWorker::new(Arc::new(move |event| {
            let pause = matches!(&event, IoEvent::Saved { revision: 1, .. });
            events.send(event).unwrap();
            if pause {
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        }))
        .unwrap();
        io.save(7, path.clone(), 1, 1, 1);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        io.save(7, path.clone(), 2, 4, 2);
        io.move_deleted(7, 40, vec![path.clone()]);
        // Deliberately violate the UI's edit lock to test the I/O boundary.
        io.save(7, path.clone(), 3, 5, 3);
        io.flush();
        release.send(()).unwrap();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 2,
                result: Ok(()),
                ..
            }
        ));
        let record = moved(&receive, 40);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 3,
                unsaved: true,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        assert!(!path.exists());
        assert!(!xmp::sidecar_path(&path).exists());
        assert_eq!(xmp::read_rating(&record.destination).unwrap(), Some(4));

        io.retry_failed();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 3,
                unsaved: true,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        assert!(!xmp::sidecar_path(&path).exists());
        io.restore_deleted(7, 41, vec![record]);
        io.flush();
        assert!(
            matches!(next(&receive), IoEvent::RestoredDeleted { op_id: 41, restored, ratings, failed, .. }
                if restored.len() == 1 && failed.is_empty() && ratings == vec![(path.clone(), Ok(4))])
        );
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        io.retry_failed();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 3,
                unsaved: false,
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(xmp::read_rating(&path).unwrap(), Some(5));
        assert_eq!(fs::read(path).unwrap(), b"synthetic RAW remains unchanged");
    }

    #[test]
    fn failed_rating_prevents_move_and_survives_until_explicit_retry() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let sidecar = xmp::sidecar_path(&path);
        fs::write(&sidecar, b"<malformed existing XMP").unwrap();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 5, 1);
        io.move_deleted(1, 10, vec![path.clone()]);
        io.flush();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                unsaved: true,
                result: Err(_),
                ..
            }
        ));
        assert!(
            matches!(next(&receive), IoEvent::MovedDeleted { op_id: 10, records, errors, .. } if records.is_empty() && errors.len() == 1 && errors[0].0 == path)
        );
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
        assert_eq!(fs::read(&sidecar).unwrap(), b"<malformed existing XMP");
        fs::remove_file(&sidecar).unwrap(); // Repair only this synthetic fixture.
        xmp::write_rating(&path, 2).unwrap();
        io.retry_failed();
        io.move_deleted(1, 11, vec![path.clone()]);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                unsaved: false,
                result: Ok(()),
                ..
            }
        ));
        // retry_failed requests a flush; it must not overtake the queued move.
        // If the worker already flushed before move_deleted was called, that
        // early completion is harmless; the subsequent move still executes.
        let mut first = next(&receive);
        if matches!(first, IoEvent::Flushed { failures: 0 }) {
            first = next(&receive);
        }
        let IoEvent::MovedDeleted {
            records, errors, ..
        } = first
        else {
            panic!("expected retried move");
        };
        assert!(errors.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(xmp::read_rating(&records[0].destination).unwrap(), Some(5));
        drop(io);
    }

    #[test]
    fn restore_keeps_rating_history_and_failed_restore_keeps_its_record_retryable() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        xmp::write_rating(&path, 3).unwrap();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 4, 10);
        io.move_deleted(1, 20, vec![path.clone()]);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        let record = moved(&receive, 20);
        io.undo(1, path.clone(), 2, 10);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: true,
                unsaved: false,
                result: Err(_),
                ..
            }
        ));
        assert!(!xmp::sidecar_path(&path).exists());

        fs::write(&path, b"unrelated replacement RAW").unwrap();
        io.restore_deleted(1, 21, vec![record]);
        let IoEvent::RestoredDeleted {
            restored,
            mut failed,
            ..
        } = next(&receive)
        else {
            panic!("expected failed restore");
        };
        assert!(restored.is_empty());
        assert_eq!(failed.len(), 1);
        assert_eq!(fs::read(&path).unwrap(), b"unrelated replacement RAW");
        let (record, _) = failed.pop().unwrap();
        assert_eq!(xmp::read_rating(&record.destination).unwrap(), Some(4));
        fs::remove_file(&path).unwrap(); // Remove only the synthetic collision.
        io.restore_deleted(1, 22, vec![record]);
        io.undo(1, path.clone(), 3, 10);
        io.flush();
        assert!(
            matches!(next(&receive), IoEvent::RestoredDeleted { op_id: 22, restored, failed, .. } if restored.len() == 1 && failed.is_empty())
        );
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: true,
                rating: Some(3),
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(xmp::read_rating(&path).unwrap(), Some(3));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
    }

    #[test]
    fn shutdown_drains_an_already_authorized_move_after_its_rating_save() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 2, 1);
        io.move_deleted(1, 2, vec![path.clone()]);
        drop(io);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        let record = moved(&receive, 2);
        assert_eq!(xmp::read_rating(&record.destination).unwrap(), Some(2));
        assert_eq!(
            fs::read(&record.destination).unwrap(),
            b"synthetic RAW remains unchanged"
        );
        assert!(!path.exists());
        assert!(!xmp::sidecar_path(&path).exists());
    }

    #[test]
    fn pending_edits_preserve_every_logical_revision_before_flush_and_reopen() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let io = IoWorker::new(Arc::new(move |event| {
            let pause = matches!(&event, IoEvent::Saved { revision: 1, .. });
            events.send(event).unwrap();
            if pause {
                // Hold the worker between writes, so both subsequent public
                // save calls are pending together rather than racing the disk.
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        }))
        .unwrap();
        io.save(7, path.clone(), 1, 1, 1);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                session: 7,
                revision: 1,
                result: Ok(()),
                ..
            }
        ));
        io.save(7, path.clone(), 2, 2, 2);
        io.save(7, path.clone(), 3, -1, 3);
        io.flush();
        release.send(()).unwrap();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 2,
                rating: Some(2),
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                session: 7,
                revision: 3,
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(xmp::read_rating(&path).unwrap(), Some(-1));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
        assert!(
            receive.try_recv().is_err(),
            "unexpected additional write event"
        );
    }

    #[test]
    fn failed_save_is_reported_by_flush_and_explicit_retry_persists_it() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let sidecar = xmp::sidecar_path(&path);
        fs::write(&sidecar, b"<malformed existing sidecar").unwrap();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(11, path.clone(), 9, 4, 9);
        io.flush();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                session: 11,
                revision: 9,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        assert_eq!(fs::read(&sidecar).unwrap(), b"<malformed existing sidecar");
        fs::remove_file(&sidecar).unwrap(); // Repair only this synthetic fixture.
        xmp::write_rating(&path, 0).unwrap();
        io.retry_failed();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                session: 11,
                revision: 9,
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(xmp::read_rating(&path).unwrap(), Some(4));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
    }

    #[test]
    fn shutdown_drains_pending_writes() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(3, path.clone(), 1, 5, 1);
        // No Trash job is submitted: these tests only exercise synthetic XMP.
        drop(io);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                revision: 1,
                result: Ok(()),
                ..
            }
        ));
        assert_eq!(xmp::read_rating(&path).unwrap(), Some(5));
        assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
    }

    #[test]
    fn undo_before_metadata_read_restores_existing_rating_and_exact_absence() {
        for original in [None, Some(3)] {
            let directory = TestDirectory::new();
            let path = directory.raw();
            if let Some(rating) = original {
                xmp::write_rating(&path, rating).unwrap();
            }
            let (events, receive) = mpsc::channel();
            let io = IoWorker::new(Arc::new(move |event| {
                events.send(event).unwrap();
            }))
            .unwrap();
            // No read() call: undo must capture the authoritative sidecar itself.
            io.save(1, path.clone(), 1, -1, 10);
            io.undo(1, path.clone(), 2, 10);
            io.flush();
            assert!(matches!(
                next(&receive),
                IoEvent::Saved {
                    edit_id: 10,
                    undo: false,
                    result: Ok(()),
                    ..
                }
            ));
            assert!(
                matches!(next(&receive), IoEvent::Saved { edit_id: 10, undo: true, rating, unsaved: false, result: Ok(()), .. } if rating == original)
            );
            assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
            drop(io);
            assert_eq!(xmp::read_rating(&path).unwrap(), original);
            assert_eq!(fs::read(&path).unwrap(), b"synthetic RAW remains unchanged");
        }
    }

    #[test]
    fn rapid_edits_and_undo_remain_fifo_even_when_every_command_is_pending() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let io = IoWorker::new(Arc::new(move |event| {
            let pause = matches!(&event, IoEvent::Saved { revision: 1, .. });
            events.send(event).unwrap();
            if pause {
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 1, 10);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        io.save(1, path.clone(), 2, 2, 11);
        io.save(1, path.clone(), 3, -1, 12);
        io.undo(1, path.clone(), 4, 12);
        io.save(1, path.clone(), 5, 5, 13);
        io.undo(1, path.clone(), 6, 13);
        io.undo(1, path.clone(), 7, 11);
        io.undo(1, path.clone(), 8, 10);
        io.flush();
        release.send(()).unwrap();
        for (revision, expected) in [
            (2, Some(2)),
            (3, Some(-1)),
            (4, Some(2)),
            (5, Some(5)),
            (6, Some(2)),
            (7, Some(1)),
            (8, None),
        ] {
            assert!(
                matches!(next(&receive), IoEvent::Saved { revision: got, rating, unsaved: false, result: Ok(()), .. } if got == revision && rating == expected)
            );
        }
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(xmp::read_rating(&path).unwrap(), None);
    }

    #[test]
    fn undo_refuses_external_rating_change_but_preserves_unrelated_external_fields() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        xmp::write_rating(&path, 2).unwrap();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 4, 10);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        let sidecar = xmp::sidecar_path(&path);
        let external = fs::read_to_string(&sidecar).unwrap().replace("xmp:Rating=\"4\"", "xmp:Rating=\"4\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" dc:format=\"external-kept\"");
        fs::write(&sidecar, &external).unwrap();
        io.undo(1, path.clone(), 2, 10);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: true,
                rating: Some(2),
                result: Ok(()),
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(&sidecar).unwrap(),
            external.replace("xmp:Rating=\"4\"", "xmp:Rating=\"2\"")
        );
        io.save(1, path.clone(), 3, 1, 11);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        xmp::write_rating(&path, 5).unwrap(); // Simulate another editor.
        let external = fs::read(&sidecar).unwrap();
        io.undo(1, path.clone(), 4, 11);
        io.flush();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: true,
                unsaved: false,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert_eq!(fs::read(&sidecar).unwrap(), external);
    }

    #[test]
    fn failed_edit_cannot_undo_another_edit_or_hide_unsaved_retry_state() {
        let directory = TestDirectory::new();
        let path = directory.raw();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.save(1, path.clone(), 1, 1, 10);
        assert!(matches!(
            next(&receive),
            IoEvent::Saved { result: Ok(()), .. }
        ));
        fs::write(xmp::sidecar_path(&path), b"<broken external sidecar").unwrap();
        io.save(1, path.clone(), 2, 4, 11);
        io.undo(1, path.clone(), 3, 11);
        io.flush();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: false,
                unsaved: true,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                undo: true,
                unsaved: true,
                result: Err(_),
                ..
            }
        ));
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 1 }));
        drop(io);
        assert_eq!(
            fs::read(xmp::sidecar_path(&path)).unwrap(),
            b"<broken external sidecar"
        );
    }

    #[test]
    fn history_is_bounded_and_rejects_wrong_path_and_out_of_order_undo() {
        let path = PathBuf::from("synthetic.ARW");
        let mut history = History::default();
        for edit_id in 0..101 {
            history.record(UndoRecord {
                edit_id,
                path: path.clone(),
                previous: Some(0),
                rating: 1,
            });
        }
        assert_eq!(history.0.len(), 100);
        assert!(history.0.iter().all(|record| record.edit_id != 0));
        assert!(history.target(&PathBuf::from("other.ARW"), 100).is_err());
        assert!(history.target(&path, 99).is_err());
        assert_eq!(history.target(&path, 100).unwrap().edit_id, 100);
        history.remove(100);
        assert_eq!(history.target(&path, 99).unwrap().edit_id, 99);
    }

    #[test]
    fn rating_index_reads_only_sidecars_and_distinguishes_absence_reject_stars_and_errors() {
        let directory = TestDirectory::new();
        let files: Vec<_> = [
            "absent.ARW",
            "zero.ARW",
            "rejected.ARW",
            "five.ARW",
            "broken.ARW",
        ]
        .map(|name| directory.0.join(name))
        .into_iter()
        .collect();
        // Deliberately do not create any ARWs. Indexing must never require
        // opening a sensor file or trying to parse its EXIF.
        xmp::write_rating(&files[1], 0).unwrap();
        xmp::write_rating(&files[2], -1).unwrap();
        xmp::write_rating(&files[3], 5).unwrap();
        fs::write(xmp::sidecar_path(&files[4]), b"<malformed").unwrap();
        let (events, receive) = mpsc::channel();
        let io = IoWorker::new(Arc::new(move |event| {
            events.send(event).unwrap();
        }))
        .unwrap();
        io.scan_ratings(42, Arc::new(files.clone()));
        let IoEvent::Ratings {
            folder_epoch,
            entries,
            complete,
        } = next(&receive)
        else {
            panic!("expected rating-index completion");
        };
        assert_eq!(folder_epoch, 42);
        assert!(complete);
        assert_eq!(entries.len(), files.len());
        for (index, expected) in [0, 0, -1, 5].into_iter().enumerate() {
            assert_eq!(entries[index].0, files[index]);
            assert_eq!(entries[index].1, Ok(expected));
        }
        assert_eq!(entries[4].0, files[4]);
        assert!(entries[4].1.is_err());
        assert!(files.iter().all(|path| !path.exists()));
        assert_eq!(
            fs::read(xmp::sidecar_path(&files[4])).unwrap(),
            b"<malformed"
        );
        io.scan_ratings(43, Arc::new(Vec::new()));
        assert!(
            matches!(next(&receive), IoEvent::Ratings { folder_epoch: 43, entries, complete: true } if entries.is_empty())
        );
        drop(io);
    }

    #[test]
    fn rating_index_yields_chunks_to_saves_current_metadata_and_batches() {
        let directory = TestDirectory::new();
        let files: Arc<Vec<_>> = Arc::new(
            (0..65)
                .map(|index| directory.0.join(format!("synthetic-{index}.ARW")))
                .collect(),
        );
        let (events, receive) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let pause_first = std::sync::atomic::AtomicBool::new(true);
        let io = IoWorker::new(Arc::new(move |event| {
            let pause = matches!(&event, IoEvent::Ratings { .. })
                && pause_first.swap(false, Ordering::SeqCst);
            events.send(event).unwrap();
            if pause {
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        }))
        .unwrap();
        io.scan_ratings(5, Arc::clone(&files));
        assert!(
            matches!(next(&receive), IoEvent::Ratings { folder_epoch: 5, entries, complete: false } if entries.len() == 32)
        );
        io.save(7, files[40].clone(), 1, 5, 1);
        io.read(8, files[50].clone());
        io.collect_rejected(9, Arc::new(Vec::new()));
        release.send(()).unwrap();
        assert!(matches!(
            next(&receive),
            IoEvent::Saved {
                session: 7,
                result: Ok(()),
                ..
            }
        ));
        assert!(matches!(
            next(&receive),
            IoEvent::Metadata { session: 8, .. }
        ));
        assert!(
            matches!(next(&receive), IoEvent::Rejected { session: 9, result: Ok(paths) } if paths.is_empty())
        );
        let IoEvent::Ratings {
            folder_epoch,
            entries,
            complete,
        } = next(&receive)
        else {
            panic!("expected second rating-index chunk after higher-priority work");
        };
        assert_eq!(folder_epoch, 5);
        assert!(!complete);
        assert_eq!(entries.len(), 32);
        assert!(entries
            .iter()
            .any(|(path, rating)| path == &files[40] && *rating == Ok(5)));
        assert!(
            matches!(next(&receive), IoEvent::Ratings { folder_epoch: 5, entries, complete: true } if entries.len() == 1)
        );
        drop(io);
        assert_eq!(xmp::read_rating(&files[40]).unwrap(), Some(5));
    }

    #[test]
    fn rating_index_replacement_cancellation_and_flush_discard_remaining_chunks() {
        let directory = TestDirectory::new();
        let files = Arc::new(
            (0..65)
                .map(|index| directory.0.join(format!("synthetic-{index}.ARW")))
                .collect(),
        );
        let (events, receive) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let io = IoWorker::new(Arc::new(move |event| {
            let pause = matches!(
                &event,
                IoEvent::Ratings {
                    complete: false,
                    ..
                }
            );
            events.send(event).unwrap();
            if pause {
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
            }
        }))
        .unwrap();
        io.scan_ratings(1, Arc::clone(&files));
        assert!(matches!(
            next(&receive),
            IoEvent::Ratings {
                folder_epoch: 1,
                complete: false,
                ..
            }
        ));
        io.scan_ratings(2, Arc::clone(&files));
        io.scan_ratings(3, Arc::clone(&files));
        release.send(()).unwrap();
        assert!(matches!(
            next(&receive),
            IoEvent::Ratings {
                folder_epoch: 3,
                complete: false,
                ..
            }
        ));
        io.cancel_rating_scan();
        io.scan_ratings(4, files);
        io.flush(); // Flush independently cancels the newly queued fourth scan.
        release.send(()).unwrap();
        assert!(matches!(next(&receive), IoEvent::Flushed { failures: 0 }));
        drop(io);
        assert!(
            receive.try_recv().is_err(),
            "canceled/replaced scan published another chunk"
        );
    }
}
