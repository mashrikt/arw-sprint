//! Small, optional last-session state. Construct no worker in smoke tests.
//! The UI only replaces pending values; all filesystem work runs here.
use std::{
    ffi::OsString,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LEGACY_MAGIC: &[u8] = b"FASTCULL-SESSION\n1\n";
const VERSION_TWO_MAGIC: &[u8] = b"FASTCULL-SESSION\n2\n";
const MAGIC: &[u8] = b"FASTCULL-SESSION\n3\n";
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: usize = 64 * 1024;
const SAVE_DELAY: Duration = Duration::from_millis(500);
const MAX_DIRTY_AGE: Duration = Duration::from_secs(2);
// Darwin O_NOFOLLOW prevents following a replaced final path component.
const O_NOFOLLOW: i32 = 0x0000_0100;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    pub photo: Option<PathBuf>,
    pub folder: Option<PathBuf>,
    pub zoom_locked: bool,
    pub auto_advance: bool,
    /// Global display brightness in one-third-stop increments, from -9 to 9.
    pub brightness_steps: i8,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            photo: None,
            folder: None,
            zoom_locked: true,
            auto_advance: false,
            brightness_steps: 0,
        }
    }
}

pub enum SessionEvent {
    Loaded {
        state: Result<SessionState, String>,
        /// Already checked on this worker. Missing photos fall back to their
        /// saved parent; unavailable volumes yield None without erasing state.
        resume_path: Option<PathBuf>,
    },
    Saved(Result<(), String>),
}

#[derive(Default)]
struct Pending {
    photo: Option<PathBuf>,
    zoom_locked: Option<bool>,
    auto_advance: Option<bool>,
    brightness_steps: Option<i8>,
    first_change: Option<Instant>,
    last_change: Option<Instant>,
}
impl Pending {
    fn changed(&mut self) {
        let now = Instant::now();
        self.first_change.get_or_insert(now);
        self.last_change = Some(now);
    }
    fn deadline(&self) -> Option<Instant> {
        Some((self.last_change? + SAVE_DELAY).min(self.first_change? + MAX_DIRTY_AGE))
    }
}
#[derive(Default)]
struct State {
    pending: Pending,
    flush: bool,
    stop: bool,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub struct SessionWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
impl SessionWorker {
    /// Creating the worker performs no file access. The callback runs on the
    /// worker, so it should post an event rather than manipulate the UI.
    pub fn new(wake: Arc<dyn Fn(SessionEvent) + Send + Sync>) -> Result<Self, String> {
        Self::start(None, wake)
    }

    fn start(
        override_path: Option<PathBuf>,
        wake: Arc<dyn Fn(SessionEvent) + Send + Sync>,
    ) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("fastcull-session".into())
            .spawn(move || {
                let path = override_path.map_or_else(session_path, Ok);
                worker(worker_shared, path, wake);
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Call only after a photo has successfully been displayed. No stat or
    /// canonicalization occurs here; callers supply an absolute path.
    pub fn remember(&self, path: PathBuf) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.photo = Some(path);
        state.pending.changed();
        self.shared.changed.notify_one();
    }

    /// Update only preferences the user changed. This preserves untouched
    /// saved values even when the initial asynchronous restore is still busy.
    pub fn preferences(
        &self,
        zoom_locked: Option<bool>,
        auto_advance: Option<bool>,
        brightness_steps: Option<i8>,
    ) {
        if zoom_locked.is_none() && auto_advance.is_none() && brightness_steps.is_none() {
            return;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = zoom_locked {
            state.pending.zoom_locked = Some(value);
        }
        if let Some(value) = auto_advance {
            state.pending.auto_advance = Some(value);
        }
        if let Some(value) = brightness_steps {
            state.pending.brightness_steps = Some(value.clamp(-9, 9));
        }
        state.pending.changed();
        self.shared.changed.notify_one();
    }

    /// Request an asynchronous flush; Saved acknowledges it even if nothing
    /// changed. Drop also drains the last pending update before joining.
    pub fn flush(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.flush = true;
        self.shared.changed.notify_one();
    }
}
impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop = true;
        self.shared.changed.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn session_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("HOME is unavailable or not an absolute path")?;
    // Keep the established location so existing installations are migrated by
    // the worker. The record header, rather than the filename, versions its data.
    Ok(home.join("Library/Application Support/FastCull/session-v1.bin"))
}

fn resume_path(state: &SessionState) -> Option<PathBuf> {
    state
        .photo
        .as_ref()
        .filter(|path| path.is_file())
        .or_else(|| state.folder.as_ref().filter(|path| path.is_dir()))
        .cloned()
}

fn worker(
    shared: Arc<Shared>,
    path: Result<PathBuf, String>,
    wake: Arc<dyn Fn(SessionEvent) + Send + Sync>,
) {
    let loaded = path
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|path| read_session_record(path));
    let mut needs_save = loaded.as_ref().is_ok_and(|(_, legacy)| *legacy);
    let loaded = loaded.map(|(state, _)| state);
    let mut current = loaded.as_ref().cloned().unwrap_or_default();
    let resume_path = loaded.as_ref().ok().and_then(resume_path);
    wake(SessionEvent::Loaded {
        state: loaded,
        resume_path,
    });
    loop {
        let (pending, stop) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let due = state.pending.deadline();
                if state.stop || state.flush || due.is_some_and(|time| time <= Instant::now()) {
                    state.flush = false;
                    break (std::mem::take(&mut state.pending), state.stop);
                }
                state = if let Some(due) = due {
                    shared
                        .changed
                        .wait_timeout(state, due.saturating_duration_since(Instant::now()))
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                } else {
                    shared
                        .changed
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner())
                };
            }
        };
        let mut updated = current.clone();
        if let Some(photo) = pending.photo {
            updated.folder = photo.parent().map(Path::to_path_buf);
            updated.photo = Some(photo);
        }
        if let Some(zoom_locked) = pending.zoom_locked {
            updated.zoom_locked = zoom_locked;
        }
        if let Some(auto_advance) = pending.auto_advance {
            updated.auto_advance = auto_advance;
        }
        if let Some(brightness_steps) = pending.brightness_steps {
            updated.brightness_steps = brightness_steps;
        }
        needs_save |= updated != current;
        current = updated;
        let result = if needs_save {
            path.as_ref()
                .map_err(Clone::clone)
                .and_then(|path| write_session(path, &current))
        } else {
            Ok(())
        };
        if result.is_ok() {
            needs_save = false;
        }
        wake(SessionEvent::Saved(result));
        if stop {
            break;
        }
    }
}

fn checked_path(bytes: &[u8]) -> Result<Option<PathBuf>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_PATH_BYTES || bytes.contains(&0) {
        return Err("session path is too long or contains NUL".into());
    }
    let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    if !path.is_absolute() {
        return Err("session paths must be absolute".into());
    }
    Ok(Some(path))
}

fn encode(state: &SessionState) -> Result<Vec<u8>, String> {
    let photo = state
        .photo
        .as_deref()
        .map_or(&[][..], |p| p.as_os_str().as_bytes());
    let folder = state
        .folder
        .as_deref()
        .map_or(&[][..], |p| p.as_os_str().as_bytes());
    checked_path(photo)?;
    checked_path(folder)?;
    if let Some(photo) = &state.photo {
        if photo.file_name().is_none() || photo.parent() != state.folder.as_deref() {
            return Err("session folder must be the photo's parent".into());
        }
    }
    let mut bytes = Vec::with_capacity(MAGIC.len() + 10 + photo.len() + folder.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(u8::from(state.zoom_locked) | (u8::from(state.auto_advance) << 1));
    bytes.push(state.brightness_steps.clamp(-9, 9) as u8);
    bytes.extend_from_slice(&(photo.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(folder.len() as u32).to_le_bytes());
    bytes.extend_from_slice(photo);
    bytes.extend_from_slice(folder);
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<SessionState, String> {
    let legacy = bytes.starts_with(LEGACY_MAGIC);
    let version_two = bytes.starts_with(VERSION_TWO_MAGIC);
    let current = bytes.starts_with(MAGIC);
    let preferences_len = if current { 2 } else { 1 };
    let header = MAGIC.len() + preferences_len + 8;
    if bytes.len() > MAX_FILE_BYTES || bytes.len() < header || (!current && !version_two && !legacy)
    {
        return Err("invalid, oversized, or unsupported session file".into());
    }
    let flags = bytes[MAGIC.len()];
    if flags & !3 != 0 {
        return Err("unknown session preferences".into());
    }
    let lengths = &bytes[MAGIC.len() + preferences_len..header];
    let photo_len = u32::from_le_bytes(
        lengths[..4]
            .try_into()
            .map_err(|_| "invalid photo length")?,
    ) as usize;
    let folder_len = u32::from_le_bytes(
        lengths[4..]
            .try_into()
            .map_err(|_| "invalid folder length")?,
    ) as usize;
    if photo_len > MAX_PATH_BYTES
        || folder_len > MAX_PATH_BYTES
        || header
            .checked_add(photo_len)
            .and_then(|n| n.checked_add(folder_len))
            != Some(bytes.len())
    {
        return Err("invalid session path lengths".into());
    }
    let state = SessionState {
        photo: checked_path(&bytes[header..header + photo_len])?,
        folder: checked_path(&bytes[header + photo_len..])?,
        // Version1 did not distinguish its old default-off value from a choice.
        // Move all legacy records to the requested default-on behavior once;
        // Versions 2 and later honor an explicit L-off preference.
        zoom_locked: legacy || flags & 1 != 0,
        auto_advance: flags & 2 != 0,
        brightness_steps: if current {
            (bytes[MAGIC.len() + 1] as i8).clamp(-9, 9)
        } else {
            0
        },
    };
    // Validate the parent relationship too; this tiny allocation is bounded.
    encode(&state)?;
    Ok(state)
}

fn regular_file(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata))
        }
        Ok(_) => Err("session file must be a regular file, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn directory(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(metadata))
        }
        Ok(_) => Err("session parent must be a directory, not a symlink".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
fn read_session(path: &Path) -> Result<SessionState, String> {
    read_session_record(path).map(|(state, _)| state)
}

fn read_session_record(path: &Path) -> Result<(SessionState, bool), String> {
    let parent = path.parent().ok_or("session file has no parent")?;
    if directory(parent)?.is_none() {
        return Ok((SessionState::default(), false));
    }
    let Some(metadata) = regular_file(path)? else {
        return Ok((SessionState::default(), false));
    };
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err("session file exceeds 64 KiB".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    decode(&bytes).map(|state| (state, !bytes.starts_with(MAGIC)))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn write_session(path: &Path, state: &SessionState) -> Result<(), String> {
    // Validate before creating or replacing any file.
    let bytes = encode(state)?;
    let parent = path.parent().ok_or("session file has no parent")?;
    if directory(parent)?.is_none() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| error.to_string())?;
    }
    let before = directory(parent)?.ok_or("session directory disappeared")?;
    regular_file(path)?;
    let (temporary, mut file) = (0..128)
        .find_map(|_| {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp = parent.join(format!(".session-{}-{sequence}.tmp", std::process::id()));
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
        .ok_or("could not create a unique session temporary file")??;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())?;
    drop(file);
    let after = directory(parent)?.ok_or("session directory disappeared")?;
    if before.dev() != after.dev() || before.ino() != after.ino() {
        return Err("session directory changed during save".into());
    }
    regular_file(path)?;
    // Independent app instances use last-writer-wins session state. Rename
    // publishes an entire record atomically, never a partially written path.
    fs::rename(&temporary.0, path).map_err(|error| error.to_string())?;
    if let Ok(directory) = File::open(parent) {
        // Some macOS volumes reject directory fsync; file data was synced.
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        sync::mpsc,
    };

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let id = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("fastcull-session-test-{}-{id}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn session(&self) -> PathBuf {
            self.0.join("support/session-v1.bin")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn next(events: &mpsc::Receiver<SessionEvent>) -> SessionEvent {
        events
            .recv_timeout(Duration::from_secs(10))
            .expect("session worker timed out")
    }
    fn with_photo(photo: PathBuf) -> SessionState {
        SessionState {
            folder: photo.parent().map(Path::to_path_buf),
            photo: Some(photo),
            ..SessionState::default()
        }
    }
    fn previous_record(state: &SessionState, magic: &[u8]) -> Vec<u8> {
        let mut bytes = encode(state).unwrap();
        bytes.remove(MAGIC.len() + 1);
        bytes[..MAGIC.len()].copy_from_slice(magic);
        bytes
    }

    #[test]
    fn record_round_trips_non_utf8_paths_and_preferences_with_bounded_validation() {
        let mut state = with_photo(PathBuf::from(OsString::from_vec(
            b"/photos/trip/\xff\n.ARW".to_vec(),
        )));
        state.zoom_locked = true;
        state.auto_advance = true;
        state.brightness_steps = 4;
        let bytes = encode(&state).unwrap();
        assert_eq!(decode(&bytes).unwrap(), state);
        for length in 0..bytes.len() {
            assert!(decode(&bytes[..length]).is_err());
        }
        let mut overflow = bytes.clone();
        overflow[MAGIC.len() + 2..MAGIC.len() + 6].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&overflow).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        assert!(encode(&with_photo(PathBuf::from("relative.ARW"))).is_err());
        assert!(checked_path(b"/bad\0path").is_err());
        assert!(checked_path(&vec![b'/'; MAX_PATH_BYTES + 1]).is_err());
    }

    #[test]
    fn legacy_preferences_migrate_on_and_version_two_preserves_explicit_off() {
        assert!(SessionState::default().zoom_locked);
        assert_eq!(SessionState::default().brightness_steps, 0);
        let mut state = with_photo(PathBuf::from("/photos/trip/photo.ARW"));
        state.auto_advance = true;
        for old_zoom in [false, true] {
            state.zoom_locked = old_zoom;
            let migrated = decode(&previous_record(&state, LEGACY_MAGIC)).unwrap();
            assert!(migrated.zoom_locked);
            assert_eq!(migrated.photo, state.photo);
            assert_eq!(migrated.folder, state.folder);
            assert!(migrated.auto_advance);
            assert_eq!(migrated.brightness_steps, 0);
            let migrated = decode(&previous_record(&state, VERSION_TWO_MAGIC)).unwrap();
            assert_eq!(migrated, state);
        }
        state.zoom_locked = false;
        let current = encode(&state).unwrap();
        assert!(current.starts_with(MAGIC));
        assert_eq!(decode(&current).unwrap(), state);
    }

    #[test]
    fn brightness_round_trips_and_clamps_without_erasing_other_preferences() {
        let mut state = with_photo(PathBuf::from("/photos/trip/photo.ARW"));
        state.auto_advance = true;
        for steps in i8::MIN..=i8::MAX {
            state.brightness_steps = steps;
            let mut bytes = encode(&state).unwrap();
            let mut expected = state.clone();
            expected.brightness_steps = steps.clamp(-9, 9);
            assert_eq!(decode(&bytes).unwrap(), expected);
            // A malformed stored value also stays within the UI's safe range.
            bytes[MAGIC.len() + 1] = steps as u8;
            assert_eq!(decode(&bytes).unwrap(), expected);
        }
    }

    #[test]
    fn worker_migrates_legacy_record_atomically_and_saves_later_opt_out() {
        let directory = TestDirectory::new();
        let path = directory.session();
        let mut old = with_photo(directory.0.join("photo.ARW"));
        old.zoom_locked = false;
        old.auto_advance = true;
        let bytes = previous_record(&old, LEGACY_MAGIC);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let (send, events) = mpsc::channel();
        let worker = SessionWorker::start(
            Some(path.clone()),
            Arc::new(move |event| {
                send.send(event).unwrap();
            }),
        )
        .unwrap();
        let SessionEvent::Loaded {
            state: Ok(state), ..
        } = next(&events)
        else {
            panic!("legacy load failed");
        };
        assert!(state.zoom_locked && state.auto_advance);
        assert_eq!(state.photo, old.photo);
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "loading alone must not write"
        );
        worker.flush();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        assert!(fs::read(&path).unwrap().starts_with(MAGIC));
        assert!(read_session(&path).unwrap().zoom_locked);
        worker.preferences(Some(false), None, Some(3));
        worker.flush();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        drop(worker);
        let saved = read_session(&path).unwrap();
        assert!(!saved.zoom_locked);
        assert_eq!(saved.photo, old.photo);
        assert!(saved.auto_advance);
        assert_eq!(saved.brightness_steps, 3);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn version_two_migration_merges_brightness_changed_before_restore_completes() {
        let directory = TestDirectory::new();
        let path = directory.session();
        let mut old = with_photo(directory.0.join("photo.ARW"));
        old.zoom_locked = false;
        old.auto_advance = true;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, previous_record(&old, VERSION_TWO_MAGIC)).unwrap();
        let (send, events) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let worker = SessionWorker::start(
            Some(path.clone()),
            Arc::new(move |event| {
                let pause = matches!(&event, SessionEvent::Loaded { .. });
                send.send(event).unwrap();
                if pause {
                    blocked
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                }
            }),
        )
        .unwrap();
        let SessionEvent::Loaded {
            state: Ok(state), ..
        } = next(&events)
        else {
            panic!("version two load failed");
        };
        assert_eq!(state, old);
        worker.preferences(None, None, Some(3));
        worker.preferences(None, None, Some(-127));
        worker.flush();
        release.send(()).unwrap();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        old.brightness_steps = -9;
        assert_eq!(read_session(&path).unwrap(), old);
        assert!(fs::read(&path).unwrap().starts_with(MAGIC));
        drop(worker);
    }

    #[test]
    fn pending_navigation_coalesces_and_drop_flushes_latest_path_and_preferences() {
        let directory = TestDirectory::new();
        let path = directory.session();
        let (send, events) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let worker = SessionWorker::start(
            Some(path.clone()),
            Arc::new(move |event| {
                let loaded = matches!(&event, SessionEvent::Loaded { .. });
                send.send(event).unwrap();
                if loaded {
                    let _ = blocked
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }
            }),
        )
        .unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::Loaded {
                state: Ok(_),
                resume_path: None
            }
        ));
        assert!(!path.exists());
        for n in 0..1000 {
            worker.remember(directory.0.join(format!("photo-{n}.ARW")));
        }
        for steps in -9..=9 {
            worker.preferences(None, None, Some(steps));
        }
        worker.preferences(Some(true), Some(true), None);
        release.send(()).unwrap();
        drop(worker);
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        assert!(
            events.try_recv().is_err(),
            "navigation caused multiple writes"
        );
        let state = read_session(&path).unwrap();
        assert_eq!(state.photo, Some(directory.0.join("photo-999.ARW")));
        assert_eq!(state.folder, Some(directory.0.clone()));
        assert!(state.zoom_locked && state.auto_advance);
        assert_eq!(state.brightness_steps, 9);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn restore_falls_back_without_erasing_missing_photo_and_retains_preferences() {
        let directory = TestDirectory::new();
        let photo = directory.0.join("photo.ARW");
        let mut state = with_photo(photo.clone());
        state.zoom_locked = true;
        write_session(&directory.session(), &state).unwrap();
        let (send, events) = mpsc::channel();
        let worker = SessionWorker::start(
            Some(directory.session()),
            Arc::new(move |event| {
                send.send(event).unwrap();
            }),
        )
        .unwrap();
        let SessionEvent::Loaded {
            state: Ok(loaded),
            resume_path,
        } = next(&events)
        else {
            panic!("load failed")
        };
        assert_eq!(loaded, state);
        assert_eq!(resume_path, Some(directory.0.clone()));
        drop(worker);
        assert_eq!(read_session(&directory.session()).unwrap(), state);
        fs::write(&photo, b"synthetic").unwrap();
        assert_eq!(super::resume_path(&state), Some(photo));
        state.folder = Some(directory.0.join("unavailable-volume"));
        state.photo = Some(state.folder.as_ref().unwrap().join("missing.ARW"));
        assert_eq!(super::resume_path(&state), None);
        assert!(state.zoom_locked);
    }

    #[test]
    fn failed_atomic_save_preserves_target_and_retries_on_explicit_flush() {
        let directory = TestDirectory::new();
        let path = directory.session();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let protected = directory.0.join("protected");
        fs::write(&protected, b"unchanged").unwrap();
        symlink(&protected, &path).unwrap();
        let (send, events) = mpsc::channel();
        let worker = SessionWorker::start(
            Some(path.clone()),
            Arc::new(move |event| {
                send.send(event).unwrap();
            }),
        )
        .unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::Loaded {
                state: Err(_),
                resume_path: None
            }
        ));
        worker.remember(directory.0.join("new.ARW"));
        worker.flush();
        assert!(matches!(next(&events), SessionEvent::Saved(Err(_))));
        assert_eq!(fs::read(&protected).unwrap(), b"unchanged");
        fs::remove_file(&path).unwrap();
        worker.flush();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        assert_eq!(
            read_session(&path).unwrap().photo,
            Some(directory.0.join("new.ARW"))
        );
        drop(worker);
    }

    #[test]
    fn malformed_oversized_and_symlinked_parent_inputs_are_safe() {
        let directory = TestDirectory::new();
        let path = directory.session();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0; MAX_FILE_BYTES + 1]).unwrap();
        assert!(read_session(&path).is_err());
        fs::write(&path, b"bad version").unwrap();
        let original = fs::read(&path).unwrap();
        assert!(write_session(&path, &with_photo(PathBuf::from("relative.ARW"))).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let linked = directory.0.join("linked");
        symlink(path.parent().unwrap(), &linked).unwrap();
        assert!(read_session(&linked.join("session-v1.bin")).is_err());
        assert!(write_session(&linked.join("session-v1.bin"), &SessionState::default()).is_err());
    }

    #[test]
    fn repeated_navigation_has_a_bounded_flush_deadline() {
        let now = Instant::now();
        let pending = Pending {
            first_change: Some(now),
            last_change: Some(now + Duration::from_secs(10)),
            ..Pending::default()
        };
        assert_eq!(pending.deadline(), Some(now + MAX_DIRTY_AGE));
        let pending = Pending {
            first_change: Some(now),
            last_change: Some(now),
            ..Pending::default()
        };
        assert_eq!(pending.deadline(), Some(now + SAVE_DELAY));
    }

    #[test]
    fn partial_preferences_preserve_loaded_values_and_merge_pending_fields() {
        let directory = TestDirectory::new();
        let path = directory.session();
        let initial = SessionState {
            zoom_locked: true,
            brightness_steps: -3,
            ..SessionState::default()
        };
        write_session(&path, &initial).unwrap();
        let (send, events) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let blocked = Mutex::new(blocked);
        let worker = SessionWorker::start(
            Some(path.clone()),
            Arc::new(move |event| {
                let pause = matches!(&event, SessionEvent::Loaded { .. });
                send.send(event).unwrap();
                if pause {
                    let _ = blocked
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }
            }),
        )
        .unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::Loaded { state: Ok(_), .. }
        ));
        worker.preferences(None, Some(true), None);
        worker.preferences(None, None, None);
        worker.flush();
        release.send(()).unwrap();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        let saved = read_session(&path).unwrap();
        assert!(saved.zoom_locked && saved.auto_advance);
        assert_eq!(saved.brightness_steps, -3);
        worker.preferences(Some(false), None, None);
        worker.preferences(None, Some(false), None);
        worker.preferences(None, None, Some(127));
        worker.flush();
        assert!(matches!(next(&events), SessionEvent::Saved(Ok(()))));
        assert_eq!(
            read_session(&path).unwrap(),
            SessionState {
                zoom_locked: false,
                brightness_steps: 9,
                ..SessionState::default()
            }
        );
        drop(worker);
    }
}
