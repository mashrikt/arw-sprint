//! Optional keyboard preferences. All filesystem access stays on one worker;
//! constructing the store and queuing a change never access the disk.
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
};

const MAX_BYTES: usize = 64 * 1024;
// Darwin O_NOFOLLOW prevents a replaced final component from being followed.
const O_NOFOLLOW: i32 = 0x0000_0100;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum StoreEvent {
    Loaded(Result<Option<String>, String>),
    Saved(Result<(), String>),
}

#[derive(Default)]
struct Pending {
    text: Option<String>,
    flush: bool,
    stop: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    changed: Condvar,
}

pub struct ShortcutStore {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl ShortcutStore {
    /// The callback runs on the worker and should post a UI event.
    pub fn new(callback: Arc<dyn Fn(StoreEvent) + Send + Sync>) -> Result<Self, String> {
        Self::start(None, callback)
    }

    fn start(
        override_path: Option<PathBuf>,
        callback: Arc<dyn Fn(StoreEvent) + Send + Sync>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("fastcull-shortcuts".into())
            .spawn(move || {
                let path = override_path.map_or_else(preferences_path, Ok);
                run(worker_shared, path, callback);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    /// Queue already validated configuration. New values replace queued older
    /// values, including changes made before the initial read has completed.
    pub fn save(&self, text: String) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.text = Some(text);
        self.shared.changed.notify_one();
    }

    /// Request an asynchronous Saved acknowledgment, even when nothing changed.
    pub fn flush(&self) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.flush = true;
        self.shared.changed.notify_one();
    }
}

impl Drop for ShortcutStore {
    fn drop(&mut self) {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop = true;
        self.shared.changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn preferences_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME is unavailable or not an absolute path")?;
    Ok(home.join("Library/Application Support/FastCull/shortcuts.conf"))
}

fn run(
    shared: Arc<Shared>,
    path: Result<PathBuf, String>,
    callback: Arc<dyn Fn(StoreEvent) + Send + Sync>,
) {
    let loaded = path
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|path| read_text(path));
    // Never replace data that we were unable to read. Syntax validation is the
    // caller's job, and loading alone never creates or rewrites a file.
    let initial_error = loaded.as_ref().err().cloned();
    let mut persisted = loaded.as_ref().ok().cloned().flatten();
    let mut dirty = None;
    callback(StoreEvent::Loaded(loaded));
    loop {
        let (text, stop) = {
            let mut pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            while pending.text.is_none() && !pending.flush && !pending.stop {
                pending = shared
                    .changed
                    .wait(pending)
                    .unwrap_or_else(|e| e.into_inner());
            }
            pending.flush = false;
            (pending.text.take(), pending.stop)
        };
        if let Some(text) = text {
            dirty = Some(text);
        }
        let result = if let Some(error) = &initial_error {
            Err(format!("Shortcut preferences were not saved: {error}"))
        } else if let Some(text) = &dirty {
            if persisted.as_ref() == Some(text) {
                Ok(())
            } else {
                path.as_ref()
                    .map_err(Clone::clone)
                    .and_then(|path| write_text(path, text))
            }
        } else {
            Ok(())
        };
        if result.is_ok() {
            if let Some(text) = dirty.take() {
                persisted = Some(text);
            }
        }
        callback(StoreEvent::Saved(result));
        if stop {
            break;
        }
    }
}

fn checked_file(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata)),
        Ok(_) => Err("shortcut preferences must be a regular file, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn checked_directory(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(metadata)),
        Ok(_) => Err("shortcut preferences parent must be a directory, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn same_version(left: &Option<fs::Metadata>, right: &Option<fs::Metadata>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            same_file(left, right)
                && left.len() == right.len()
                && left.mtime() == right.mtime()
                && left.mtime_nsec() == right.mtime_nsec()
                && left.ctime() == right.ctime()
                && left.ctime_nsec() == right.ctime_nsec()
        }
        _ => false,
    }
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    let parent = path.parent().ok_or("shortcut preferences have no parent")?;
    if checked_directory(parent)?.is_none() {
        return Ok(None);
    }
    let Some(before) = checked_file(path)? else {
        return Ok(None);
    };
    if before.len() > MAX_BYTES as u64 {
        return Err("shortcut preferences exceed 64 KiB".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())?;
    let opened = file.metadata().map_err(|error| error.to_string())?;
    if !opened.is_file() || !same_file(&before, &opened) {
        return Err("shortcut preferences changed while opening".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_BYTES {
        return Err("shortcut preferences exceed 64 KiB".into());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "shortcut preferences must be valid UTF-8".into())
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    if text.len() > MAX_BYTES {
        return Err("shortcut preferences exceed 64 KiB".into());
    }
    let parent = path.parent().ok_or("shortcut preferences have no parent")?;
    if checked_directory(parent)?.is_none() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| error.to_string())?;
    }
    let before_directory = checked_directory(parent)?.ok_or("preferences directory disappeared")?;
    let before_file = checked_file(path)?;
    // A file may have become unreadable, oversized, or invalid UTF-8 since load.
    // Do not silently overwrite it with an earlier in-memory preference set.
    read_text(path)?;
    let (temporary, mut file) = (0..128)
        .find_map(|_| {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(".shortcuts-{}-{sequence}.tmp", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(O_NOFOLLOW)
                .open(&temp)
            {
                Ok(file) => Some(Ok((Temporary(temp), file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error.to_string())),
            }
        })
        .ok_or("could not create a unique shortcut preferences temporary file")??;
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    let after_directory = checked_directory(parent)?.ok_or("preferences directory disappeared")?;
    if !same_file(&before_directory, &after_directory)
        || !same_version(&before_file, &checked_file(path)?)
    {
        return Err("shortcut preferences changed during save".into());
    }
    fs::rename(&temporary.0, path).map_err(|error| error.to_string())?;
    if let Ok(directory) = File::open(parent) {
        // Not all macOS volumes support directory fsync. File data was synced.
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        sync::mpsc::{self, Receiver},
        time::Duration,
    };

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fastcull-shortcuts-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn preferences(&self) -> PathBuf {
            self.0.join("support/shortcuts.conf")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn next(receiver: &Receiver<StoreEvent>) -> StoreEvent {
        receiver.recv_timeout(Duration::from_secs(10)).unwrap()
    }

    fn store(path: PathBuf) -> (ShortcutStore, Receiver<StoreEvent>) {
        let (sender, receiver) = mpsc::channel();
        let store = ShortcutStore::start(
            Some(path),
            Arc::new(move |event| {
                let _ = sender.send(event);
            }),
        )
        .unwrap();
        (store, receiver)
    }

    #[test]
    fn load_does_not_create_preferences_and_flush_acknowledges() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        let (store, events) = store(path.clone());
        assert!(matches!(next(&events), StoreEvent::Loaded(Ok(None))));
        store.flush();
        assert!(matches!(next(&events), StoreEvent::Saved(Ok(()))));
        drop(store);
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn atomic_round_trip_is_private_and_leaves_no_temporary_files() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        write_text(&path, "# UTF-8 comment: café\nnext = Right\n").unwrap();
        assert_eq!(
            read_text(&path).unwrap().as_deref(),
            Some("# UTF-8 comment: café\nnext = Right\n")
        );
        write_text(&path, "next = N\n").unwrap();
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("next = N\n"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_oversized_writes_without_changing_existing_file() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        write_text(&path, "previous").unwrap();
        assert!(write_text(&path, &"x".repeat(MAX_BYTES + 1)).is_err());
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("previous"));
        write_text(&path, &"x".repeat(MAX_BYTES)).unwrap();
        assert_eq!(read_text(&path).unwrap().unwrap().len(), MAX_BYTES);
        fs::write(&path, vec![b'x'; MAX_BYTES + 1]).unwrap();
        assert!(read_text(&path).is_err());
        assert!(write_text(&path, "replacement").is_err());
    }

    #[test]
    fn rejects_symlink_file_and_parent_and_nonregular_target() {
        let directory = TestDirectory::new();
        let target = directory.0.join("target");
        fs::write(&target, "untouched").unwrap();
        let link = directory.0.join("shortcuts.conf");
        symlink(&target, &link).unwrap();
        assert!(read_text(&link).is_err());
        assert!(write_text(&link, "bad").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        let parent_link = directory.0.join("support");
        symlink(&directory.0, &parent_link).unwrap();
        assert!(read_text(&parent_link.join("new.conf")).is_err());
        assert!(write_text(&parent_link.join("new.conf"), "bad").is_err());
        assert!(read_text(&directory.0).is_err());
        assert!(write_text(&directory.0, "bad").is_err());
    }

    #[test]
    fn invalid_utf8_load_blocks_save_and_preserves_original() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let (store, events) = store(path.clone());
        assert!(matches!(next(&events), StoreEvent::Loaded(Err(_))));
        store.save("replacement".into());
        assert!(matches!(next(&events), StoreEvent::Saved(Err(_))));
        drop(store);
        assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe]);
    }

    #[test]
    fn syntactically_unknown_text_is_not_rewritten_on_load_or_flush() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        write_text(&path, "future configuration format").unwrap();
        let (store, events) = store(path.clone());
        match next(&events) {
            StoreEvent::Loaded(Ok(Some(text))) => assert_eq!(text, "future configuration format"),
            other => panic!("unexpected event: {other:?}"),
        }
        store.flush();
        assert!(matches!(next(&events), StoreEvent::Saved(Ok(()))));
        drop(store);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "future configuration format"
        );
    }

    #[test]
    fn saves_queued_during_initial_load_are_coalesced_and_survive_drop() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        let (loaded_sender, loaded_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let release_receiver = Mutex::new(release_receiver);
        let (event_sender, event_receiver) = mpsc::channel();
        let store = ShortcutStore::start(
            Some(path.clone()),
            Arc::new(move |event| {
                if matches!(event, StoreEvent::Loaded(_)) {
                    loaded_sender.send(()).unwrap();
                    release_receiver.lock().unwrap().recv().unwrap();
                }
                event_sender.send(event).unwrap();
            }),
        )
        .unwrap();
        loaded_receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        store.save("first".into());
        store.save("second".into());
        store.save("latest".into());
        release_sender.send(()).unwrap();
        drop(store);
        assert!(matches!(
            next(&event_receiver),
            StoreEvent::Loaded(Ok(None))
        ));
        assert!(matches!(next(&event_receiver), StoreEvent::Saved(Ok(()))));
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("latest"));
    }

    #[test]
    fn oversized_pending_save_can_be_replaced_by_valid_configuration() {
        let directory = TestDirectory::new();
        let path = directory.preferences();
        let (store, events) = store(path.clone());
        assert!(matches!(next(&events), StoreEvent::Loaded(Ok(None))));
        store.save("x".repeat(MAX_BYTES + 1));
        assert!(matches!(next(&events), StoreEvent::Saved(Err(_))));
        store.save("next = N\n".into());
        assert!(matches!(next(&events), StoreEvent::Saved(Ok(()))));
        drop(store);
        assert_eq!(read_text(&path).unwrap().as_deref(), Some("next = N\n"));
    }
}
