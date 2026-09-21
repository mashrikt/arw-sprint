//! Reversible, no-clobber moves into a photo folder's `_Rejected` subdirectory.
//! RAW and XMP bytes are never read or rewritten. Two-file moves use an
//! exclusive sidecar-first rename with rollback if the RAW rename fails. Undo
//! verifies the RAW identity and preserves its current paired metadata, including
//! sidecars atomically edited or created while the photo was in `_Rejected`.
use std::{
    ffi::OsString,
    fmt, fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

const MAX_NAMES: usize = 10_000;
pub(super) const REJECTED_FOLDER: &str = "_Rejected";

pub(super) fn is_rejected_folder(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(REJECTED_FOLDER))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileId {
    device: u64,
    inode: u64,
}
impl FileId {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SidecarMove {
    pub source: PathBuf,
    pub destination: PathBuf,
    identity: FileId,
}

#[derive(Clone, Debug)]
pub struct MoveRecord {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub sidecar: Option<SidecarMove>,
    identity: FileId,
}

#[derive(Debug)]
pub struct MoveError {
    pub message: String,
    /// Current known location of this RAW's original inode. None means another
    /// process removed/replaced it or the locations can no longer be inspected.
    pub raw_path: Option<PathBuf>,
    pub sidecar_path: Option<PathBuf>,
    /// The operation could not restore its initial arrangement of files.
    pub partial: bool,
}
impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if self.partial {
            write!(
                f,
                "; RAW location: {}",
                self.raw_path.as_ref().map_or_else(
                    || "unknown (file changed externally)".to_owned(),
                    |path| path.display().to_string(),
                )
            )?;
            if let Some(path) = &self.sidecar_path {
                write!(f, "; XMP location: {}", path.display())?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for MoveError {}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn metadata_if_present(path: &Path) -> io::Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn regular_file(path: &Path) -> io::Result<FileId> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(invalid(format!(
            "{} must be a regular file, not a symlink or directory",
            path.display()
        )));
    }
    Ok(FileId::of(&metadata))
}

fn directory(path: &Path) -> io::Result<FileId> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(invalid(format!(
            "{} must be a directory, not a symlink",
            path.display()
        )));
    }
    Ok(FileId::of(&metadata))
}

fn unchanged_file(path: &Path, identity: FileId) -> io::Result<()> {
    if regular_file(path)? != identity {
        return Err(invalid(format!(
            "{} changed since this operation was prepared",
            path.display()
        )));
    }
    Ok(())
}

fn vacant(path: &Path) -> io::Result<()> {
    if metadata_if_present(path)?.is_some() {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} already exists; nothing will be overwritten",
                path.display()
            ),
        ))
    } else {
        Ok(())
    }
}

fn matching_sidecar(raw: &Path) -> io::Result<Option<(PathBuf, FileId)>> {
    let mut found: Option<(PathBuf, FileId)> = None;
    for path in [raw.with_extension("xmp"), raw.with_extension("XMP")] {
        let Some(metadata) = metadata_if_present(&path)? else {
            continue;
        };
        if !metadata.file_type().is_file() {
            return Err(invalid(format!(
                "{} must be a regular sidecar, not a symlink or directory",
                path.display()
            )));
        }
        let identity = FileId::of(&metadata);
        if let Some((_, previous)) = &found {
            if *previous != identity {
                return Err(invalid(
                    "Both .xmp and .XMP sidecars exist; move is ambiguous",
                ));
            }
        } else {
            found = Some((path, identity));
        }
    }
    Ok(found)
}

fn vacant_pair(raw: &Path) -> io::Result<()> {
    vacant(raw)?;
    vacant(&raw.with_extension("xmp"))?;
    vacant(&raw.with_extension("XMP"))
}

fn destination_for(raw: &Path, folder: &Path, attempts: usize) -> io::Result<PathBuf> {
    let name = raw
        .file_name()
        .ok_or_else(|| invalid("photo has no filename"))?;
    let stem = raw
        .file_stem()
        .ok_or_else(|| invalid("photo has no filename stem"))?;
    let extension = raw
        .extension()
        .ok_or_else(|| invalid("photo has no extension"))?;
    for attempt in 0..attempts {
        let name = if attempt == 0 {
            name.to_os_string()
        } else {
            let mut name = OsString::from(stem);
            name.push(format!("-{}.", attempt + 1));
            name.push(extension);
            name
        };
        let candidate = folder.join(name);
        match vacant_pair(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "No unused RAW/XMP filename pair is available in _Rejected",
    ))
}

fn locate(identity: FileId, first: &Path, second: &Path) -> Option<PathBuf> {
    [first, second]
        .into_iter()
        .find(|path| regular_file(path).ok() == Some(identity))
        .map(Path::to_path_buf)
}

fn failure(record: &MoveRecord, restoring: bool, message: impl Into<String>) -> MoveError {
    let raw_path = locate(record.identity, &record.source, &record.destination);
    let sidecar_path = record
        .sidecar
        .as_ref()
        .and_then(|sidecar| locate(sidecar.identity, &sidecar.source, &sidecar.destination));
    let initial_raw = if restoring {
        &record.destination
    } else {
        &record.source
    };
    let initial_sidecar = record.sidecar.as_ref().map(|sidecar| {
        if restoring {
            &sidecar.destination
        } else {
            &sidecar.source
        }
    });
    let partial =
        raw_path.as_ref() != Some(initial_raw) || sidecar_path.as_ref() != initial_sidecar;
    MoveError {
        message: message.into(),
        raw_path,
        sidecar_path,
        partial,
    }
}

fn initial_error(raw: &Path, error: impl fmt::Display) -> MoveError {
    MoveError {
        message: error.to_string(),
        raw_path: regular_file(raw).is_ok().then(|| raw.to_path_buf()),
        sidecar_path: matching_sidecar(raw).ok().flatten().map(|(path, _)| path),
        partial: false,
    }
}

fn prepare(raw: &Path) -> io::Result<MoveRecord> {
    if !raw
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("arw"))
    {
        return Err(invalid("Only an ARW photograph can be moved to _Rejected"));
    }
    let identity = regular_file(raw)?;
    let parent = raw
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| invalid("photo needs a parent directory"))?;
    if is_rejected_folder(parent) {
        return Err(invalid(
            "This photograph is already in _Rejected; use Undo to restore it",
        ));
    }
    let parent_identity = directory(parent)?;
    let sidecar = matching_sidecar(raw)?;
    let folder = parent.join(REJECTED_FOLDER);
    match fs::create_dir(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let folder_identity = directory(&folder)?;
    if folder_identity.device != parent_identity.device {
        return Err(invalid(
            "_Rejected must be on the same filesystem as its photographs",
        ));
    }
    let destination = destination_for(raw, &folder, MAX_NAMES)?;
    Ok(MoveRecord {
        source: raw.to_path_buf(),
        sidecar: sidecar.map(|(source, identity)| SidecarMove {
            source,
            destination: destination.with_extension("xmp"),
            identity,
        }),
        destination,
        identity,
    })
}

/// Each individual rename is atomic and cannot clobber any destination. The
/// checks catch externally changed inputs; as with any path-based filesystem
/// operation they cannot serialize an uncooperative writer replacing parents.
fn transfer(
    record: &MoveRecord,
    restoring: bool,
    rename: &mut impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<(), MoveError> {
    let (source, destination) = if restoring {
        (&record.destination, &record.source)
    } else {
        (&record.source, &record.destination)
    };
    let sidecar = record.sidecar.as_ref().map(|sidecar| {
        let (source, destination) = if restoring {
            (&sidecar.destination, &sidecar.source)
        } else {
            (&sidecar.source, &sidecar.destination)
        };
        (source, destination, sidecar.identity)
    });
    let prepared = (|| {
        unchanged_file(source, record.identity)?;
        let source_parent = source
            .parent()
            .ok_or_else(|| invalid("source parent is missing"))?;
        let destination_parent = destination
            .parent()
            .ok_or_else(|| invalid("destination parent is missing"))?;
        let source_directory = directory(source_parent)?;
        let destination_directory = directory(destination_parent)?;
        if source_directory.device != destination_directory.device {
            return Err(invalid(
                "Source and destination must be on the same filesystem",
            ));
        }
        vacant_pair(destination)?;
        if let Some((source, _, identity)) = sidecar {
            unchanged_file(source, identity)?;
        }
        if matching_sidecar(source)?.map(|(_, identity)| identity)
            != sidecar.map(|(_, _, identity)| identity)
        {
            return Err(invalid(
                "The matching XMP sidecar changed since this operation was prepared",
            ));
        }
        Ok((
            source_parent,
            source_directory,
            destination_parent,
            destination_directory,
        ))
    })()
    .map_err(|error: io::Error| failure(record, restoring, error.to_string()))?;
    let parents_unchanged = || -> io::Result<()> {
        if directory(prepared.0)? != prepared.1 || directory(prepared.2)? != prepared.3 {
            return Err(invalid("A photo directory changed while moving files"));
        }
        Ok(())
    };
    if let Some((source, destination, identity)) = sidecar {
        parents_unchanged()
            .and_then(|()| unchanged_file(source, identity))
            .and_then(|()| rename(source, destination))
            .map_err(|error| failure(record, restoring, format!("Could not move XMP: {error}")))?;
    }
    let raw_result = parents_unchanged()
        .and_then(|()| unchanged_file(source, record.identity))
        .and_then(|()| rename(source, destination));
    if let Err(error) = raw_result {
        let mut message = format!("Could not move RAW: {error}");
        if let Some((original, moved, identity)) = sidecar {
            if let Err(rollback) = parents_unchanged()
                .and_then(|()| unchanged_file(moved, identity))
                .and_then(|()| rename(moved, original))
            {
                message.push_str(&format!("; XMP rollback also failed: {rollback}"));
            }
        }
        return Err(failure(record, restoring, message));
    }
    unchanged_file(destination, record.identity)
        .and_then(|()| {
            if let Some((_, destination, identity)) = sidecar {
                unchanged_file(destination, identity)
            } else {
                Ok(())
            }
        })
        .map_err(|error| {
            failure(
                record,
                restoring,
                format!("Files changed after the move: {error}"),
            )
        })
}

/// Move a RAW and its matching XMP into `_Rejected`, preserving existing files.
pub fn move_photo(path: &Path) -> Result<MoveRecord, MoveError> {
    move_with(path, &mut crate::platform::rename_exclusive)
}

fn move_with(
    path: &Path,
    rename: &mut impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<MoveRecord, MoveError> {
    let record = prepare(path).map_err(|error| initial_error(path, error))?;
    transfer(&record, false, rename)?;
    Ok(record)
}

/// Undo only this exact RAW move, carrying its current matching metadata back.
/// Sidecar editors normally replace XMP files atomically, so the original XMP
/// inode cannot identify the latest metadata. Snapshot the current sidecar only
/// after verifying the RAW, then apply the same identity checks and exclusive
/// rollback to that snapshot. Original filenames are never overwritten.
pub fn restore_photo(record: &MoveRecord) -> Result<(), MoveError> {
    unchanged_file(&record.destination, record.identity)
        .map_err(|error| failure(record, true, error.to_string()))?;
    let current = matching_sidecar(&record.destination)
        .map_err(|error| failure(record, true, error.to_string()))?;
    if record.sidecar.is_some() && current.is_none() {
        return Err(failure(
            record,
            true,
            "The moved photo's XMP is missing; restore was left untouched",
        ));
    }
    let mut snapshot = record.clone();
    snapshot.sidecar = current.map(|(destination, identity)| SidecarMove {
        source: record.sidecar.as_ref().map_or_else(
            || record.source.with_extension("xmp"),
            |sidecar| sidecar.source.clone(),
        ),
        destination,
        identity,
    });
    transfer(&snapshot, true, &mut crate::platform::rename_exclusive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::{
            fs::{symlink, PermissionsExt},
            net::UnixListener,
        },
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const RAW: &[u8] = b"synthetic RAW bytes\0\xff\r\n";
    const XMP: &[u8] = b"<?xpacket begin='preserve'?><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='5'/><?xpacket end='w'?>";

    struct Fixture {
        directory: PathBuf,
        raw: PathBuf,
    }
    impl Fixture {
        fn new(sidecar: bool) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "fastcull-rejected-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            let raw = directory.join("DSC00001.ARW");
            fs::write(&raw, RAW).unwrap();
            if sidecar {
                fs::write(raw.with_extension("xmp"), XMP).unwrap();
            }
            Self { directory, raw }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn exclusive_pair_move_and_restore_preserve_every_byte_and_inode() {
        let fixture = Fixture::new(true);
        fs::set_permissions(&fixture.raw, fs::Permissions::from_mode(0o440)).unwrap();
        let raw_identity = regular_file(&fixture.raw).unwrap();
        let sidecar_identity = regular_file(&fixture.raw.with_extension("xmp")).unwrap();
        let record = move_photo(&fixture.raw).unwrap();
        assert_eq!(record.source, fixture.raw);
        assert_eq!(
            record.destination,
            fixture.directory.join("_Rejected/DSC00001.ARW")
        );
        assert!(!record.source.exists());
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
        assert_eq!(regular_file(&record.destination).unwrap(), raw_identity);
        assert_eq!(
            fs::metadata(&record.destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o440
        );
        let sidecar = record.sidecar.as_ref().unwrap();
        assert_eq!(
            sidecar.destination,
            fixture.directory.join("_Rejected/DSC00001.xmp")
        );
        assert!(!sidecar.source.exists());
        assert_eq!(fs::read(&sidecar.destination).unwrap(), XMP);
        assert_eq!(
            regular_file(&sidecar.destination).unwrap(),
            sidecar_identity
        );
        restore_photo(&record).unwrap();
        assert_eq!(fs::read(&record.source).unwrap(), RAW);
        assert_eq!(fs::read(&sidecar.source).unwrap(), XMP);
        assert!(!record.destination.exists());
        assert!(!sidecar.destination.exists());
    }

    #[test]
    fn raw_without_sidecar_moves_and_restores_without_creating_metadata() {
        let fixture = Fixture::new(false);
        let record = move_photo(&fixture.raw).unwrap();
        assert!(record.sidecar.is_none());
        assert!(!record.destination.with_extension("xmp").exists());
        restore_photo(&record).unwrap();
        assert_eq!(fs::read(&fixture.raw).unwrap(), RAW);
        assert!(!fixture.raw.with_extension("xmp").exists());
    }

    #[test]
    fn either_existing_raw_or_sidecar_chooses_a_new_consistent_basename() {
        let fixture = Fixture::new(true);
        let deleted = fixture.directory.join("_Rejected");
        fs::create_dir(&deleted).unwrap();
        fs::write(deleted.join("DSC00001.ARW"), b"older raw").unwrap();
        fs::write(deleted.join("DSC00001-2.xmp"), b"orphan metadata").unwrap();
        fs::write(deleted.join("DSC00001-3.XMP"), b"uppercase metadata").unwrap();
        let record = move_photo(&fixture.raw).unwrap();
        assert_eq!(record.destination, deleted.join("DSC00001-4.ARW"));
        assert_eq!(
            record.sidecar.as_ref().unwrap().destination,
            deleted.join("DSC00001-4.xmp")
        );
        assert_eq!(
            fs::read(deleted.join("DSC00001.ARW")).unwrap(),
            b"older raw"
        );
        assert_eq!(
            fs::read(deleted.join("DSC00001-2.xmp")).unwrap(),
            b"orphan metadata"
        );
        assert_eq!(
            fs::read(deleted.join("DSC00001-3.XMP")).unwrap(),
            b"uppercase metadata"
        );
        restore_photo(&record).unwrap();
    }

    #[test]
    fn uppercase_xmp_is_preserved_and_restored_with_its_original_name() {
        let fixture = Fixture::new(false);
        let upper = fixture.raw.with_extension("XMP");
        fs::write(&upper, XMP).unwrap();
        let record = move_photo(&fixture.raw).unwrap();
        assert_eq!(
            fs::read(&record.sidecar.as_ref().unwrap().destination).unwrap(),
            XMP
        );
        restore_photo(&record).unwrap();
        assert_eq!(fs::read(upper).unwrap(), XMP);
    }

    #[test]
    fn permission_error_before_first_rename_leaves_both_files_in_place() {
        let fixture = Fixture::new(true);
        let error = move_with(&fixture.raw, &mut |_, _| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(!error.partial);
        assert_eq!(error.raw_path, Some(fixture.raw.clone()));
        assert_eq!(error.sidecar_path, Some(fixture.raw.with_extension("xmp")));
        assert_eq!(fs::read(&fixture.raw).unwrap(), RAW);
        assert_eq!(fs::read(fixture.raw.with_extension("xmp")).unwrap(), XMP);
    }

    #[test]
    fn raw_rename_failure_rolls_the_sidecar_back_without_rewriting_it() {
        let fixture = Fixture::new(true);
        let mut calls = 0;
        let error = move_with(&fixture.raw, &mut |from, to| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                crate::platform::rename_exclusive(from, to)
            }
        })
        .unwrap_err();
        assert_eq!(calls, 3);
        assert!(!error.partial);
        assert_eq!(error.raw_path, Some(fixture.raw.clone()));
        assert_eq!(fs::read(&fixture.raw).unwrap(), RAW);
        assert_eq!(fs::read(fixture.raw.with_extension("xmp")).unwrap(), XMP);
        assert!(!fixture.directory.join("_Rejected/DSC00001.xmp").exists());
    }

    #[test]
    fn collision_created_after_preflight_cannot_replace_an_external_raw() {
        let fixture = Fixture::new(true);
        let mut calls = 0;
        let error = move_with(&fixture.raw, &mut |from, to| {
            calls += 1;
            if calls == 2 {
                fs::write(to, b"external concurrent RAW").unwrap();
            }
            crate::platform::rename_exclusive(from, to)
        })
        .unwrap_err();
        assert!(!error.partial);
        assert_eq!(fs::read(&fixture.raw).unwrap(), RAW);
        assert_eq!(fs::read(fixture.raw.with_extension("xmp")).unwrap(), XMP);
        assert_eq!(
            fs::read(fixture.directory.join("_Rejected/DSC00001.ARW")).unwrap(),
            b"external concurrent RAW"
        );
    }

    #[test]
    fn failed_rollback_reports_split_locations_and_never_overwrites_new_metadata() {
        let fixture = Fixture::new(true);
        let mut calls = 0;
        let error = move_with(&fixture.raw, &mut |from, to| {
            calls += 1;
            if calls == 2 {
                fs::write(
                    fixture.raw.with_extension("xmp"),
                    b"external replacement XMP",
                )
                .unwrap();
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            crate::platform::rename_exclusive(from, to)
        })
        .unwrap_err();
        assert!(error.partial);
        assert_eq!(error.raw_path, Some(fixture.raw.clone()));
        assert_eq!(
            error.sidecar_path,
            Some(fixture.directory.join("_Rejected/DSC00001.xmp"))
        );
        assert!(error.to_string().contains("XMP rollback also failed"));
        assert_eq!(fs::read(&fixture.raw).unwrap(), RAW);
        assert_eq!(
            fs::read(fixture.raw.with_extension("xmp")).unwrap(),
            b"external replacement XMP"
        );
        assert_eq!(fs::read(error.sidecar_path.unwrap()).unwrap(), XMP);
    }

    #[test]
    fn restore_conflicts_and_external_replacements_are_left_untouched() {
        let fixture = Fixture::new(true);
        let record = move_photo(&fixture.raw).unwrap();
        fs::write(&fixture.raw, b"new original path RAW").unwrap();
        let error = restore_photo(&record).unwrap_err();
        assert!(!error.partial);
        assert_eq!(error.raw_path, Some(record.destination.clone()));
        assert_eq!(fs::read(&fixture.raw).unwrap(), b"new original path RAW");
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
        fs::remove_file(&fixture.raw).unwrap();
        fs::write(fixture.raw.with_extension("xmp"), b"new original XMP").unwrap();
        assert!(restore_photo(&record).is_err());
        assert_eq!(
            fs::read(fixture.raw.with_extension("xmp")).unwrap(),
            b"new original XMP"
        );
        fs::remove_file(fixture.raw.with_extension("xmp")).unwrap();
        let aside = fixture.directory.join("held.ARW");
        fs::rename(&record.destination, &aside).unwrap();
        fs::write(&record.destination, b"different inode RAW").unwrap();
        let error = restore_photo(&record).unwrap_err();
        assert!(error.partial);
        assert!(error.raw_path.is_none());
        assert_eq!(
            fs::read(&record.destination).unwrap(),
            b"different inode RAW"
        );
        assert_eq!(fs::read(aside).unwrap(), RAW);
    }

    #[test]
    fn failed_restore_of_raw_rolls_metadata_back_into_rejected() {
        let fixture = Fixture::new(true);
        let record = move_photo(&fixture.raw).unwrap();
        let mut calls = 0;
        let error = transfer(&record, true, &mut |from, to| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                crate::platform::rename_exclusive(from, to)
            }
        })
        .unwrap_err();
        assert!(!error.partial);
        assert_eq!(error.raw_path, Some(record.destination.clone()));
        assert!(!record.source.exists());
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
        assert_eq!(fs::read(&record.sidecar.unwrap().destination).unwrap(), XMP);
    }

    #[test]
    fn nested_rejected_moves_are_refused() {
        for name in ["_Rejected", "_REJECTED", "_rejected"] {
            assert!(is_rejected_folder(Path::new(name)));
            assert!(is_rejected_folder(&Path::new("/photos").join(name)));
        }
        for name in ["deleted", "Rejected", "_Rejected-2", "_Rejected/photos"] {
            assert!(!is_rejected_folder(Path::new(name)));
        }
        let fixture = Fixture::new(false);
        let record = move_photo(&fixture.raw).unwrap();
        assert!(move_photo(&record.destination)
            .unwrap_err()
            .to_string()
            .contains("already in _Rejected"));
        assert!(!fixture.directory.join("_Rejected/_Rejected").exists());
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
    }

    #[test]
    fn undo_preserves_metadata_created_while_the_photo_was_in_rejected() {
        let fixture = Fixture::new(false);
        let record = move_photo(&fixture.raw).unwrap();
        fs::write(
            record.destination.with_extension("xmp"),
            b"newly added metadata",
        )
        .unwrap();
        let sidecar_identity = regular_file(&record.destination.with_extension("xmp")).unwrap();
        restore_photo(&record).unwrap();
        assert_eq!(fs::read(&record.source).unwrap(), RAW);
        assert_eq!(
            fs::read(record.source.with_extension("xmp")).unwrap(),
            b"newly added metadata"
        );
        assert_eq!(
            regular_file(&record.source.with_extension("xmp")).unwrap(),
            sidecar_identity
        );
        assert!(!record.destination.exists());
        assert!(!record.destination.with_extension("xmp").exists());
    }

    #[test]
    fn undo_preserves_latest_atomic_xmp_replacement_and_verifies_the_same_raw() {
        let fixture = Fixture::new(true);
        let record = move_photo(&fixture.raw).unwrap();
        let sidecar = record.sidecar.as_ref().unwrap();
        let original_identity = regular_file(&sidecar.destination).unwrap();
        let temporary = fixture.directory.join("edited-metadata.tmp");
        let latest = b"<rdf:Description xmp:Rating='3' custom:Keep='latest unrelated metadata'/>";
        fs::write(&temporary, latest).unwrap();
        fs::rename(&temporary, &sidecar.destination).unwrap();
        let latest_identity = regular_file(&sidecar.destination).unwrap();
        assert_ne!(latest_identity, original_identity);
        let raw_identity = regular_file(&record.destination).unwrap();
        restore_photo(&record).unwrap();
        assert_eq!(fs::read(&sidecar.source).unwrap(), latest);
        assert_eq!(regular_file(&sidecar.source).unwrap(), latest_identity);
        assert_eq!(regular_file(&record.source).unwrap(), raw_identity);
        assert_eq!(fs::read(&record.source).unwrap(), RAW);
        assert!(!record.destination.exists());
        assert!(!sidecar.destination.exists());
    }

    #[test]
    fn undo_refuses_missing_or_symlinked_metadata_without_moving_the_raw() {
        let fixture = Fixture::new(true);
        let record = move_photo(&fixture.raw).unwrap();
        let sidecar = record.sidecar.as_ref().unwrap();
        fs::remove_file(&sidecar.destination).unwrap();
        assert!(restore_photo(&record).is_err());
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
        assert!(!record.source.exists());
        let unrelated = fixture.directory.join("unrelated.xmp");
        fs::write(&unrelated, b"external file must stay untouched").unwrap();
        symlink(&unrelated, &sidecar.destination).unwrap();
        assert!(restore_photo(&record).is_err());
        assert_eq!(fs::read(&record.destination).unwrap(), RAW);
        assert_eq!(
            fs::read(&unrelated).unwrap(),
            b"external file must stay untouched"
        );
        assert!(fs::symlink_metadata(&sidecar.destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn symlinks_directories_and_special_files_are_never_moved() {
        for kind in [
            "raw-link",
            "xmp-link",
            "rejected-link",
            "raw-directory",
            "raw-socket",
            "rejected-file",
        ] {
            let fixture = Fixture::new(false);
            let elsewhere = fixture.directory.join("elsewhere");
            fs::create_dir(&elsewhere).unwrap();
            let mut socket = None;
            match kind {
                "raw-link" => {
                    fs::remove_file(&fixture.raw).unwrap();
                    fs::write(elsewhere.join("original.ARW"), RAW).unwrap();
                    symlink(elsewhere.join("original.ARW"), &fixture.raw).unwrap();
                }
                "xmp-link" => {
                    fs::write(elsewhere.join("original.xmp"), XMP).unwrap();
                    symlink(
                        elsewhere.join("original.xmp"),
                        fixture.raw.with_extension("xmp"),
                    )
                    .unwrap();
                }
                "rejected-link" => {
                    symlink(&elsewhere, fixture.directory.join("_Rejected")).unwrap();
                }
                "raw-directory" => {
                    fs::remove_file(&fixture.raw).unwrap();
                    fs::create_dir(&fixture.raw).unwrap();
                }
                "raw-socket" => {
                    fs::remove_file(&fixture.raw).unwrap();
                    socket = Some(UnixListener::bind(&fixture.raw).unwrap());
                }
                "rejected-file" => {
                    fs::write(fixture.directory.join("_Rejected"), b"do not replace").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(move_photo(&fixture.raw).is_err(), "{kind}");
            assert!(fs::symlink_metadata(&fixture.raw).is_ok(), "{kind}");
            if let Some(socket) = socket {
                drop(socket);
            }
        }
    }

    #[test]
    fn broken_destination_symlinks_are_collisions_and_attempts_are_bounded() {
        let fixture = Fixture::new(false);
        let deleted = fixture.directory.join("_Rejected");
        fs::create_dir(&deleted).unwrap();
        symlink(deleted.join("missing"), deleted.join("DSC00001.ARW")).unwrap();
        fs::write(deleted.join("DSC00001-2.xmp"), b"occupied").unwrap();
        assert_eq!(
            destination_for(&fixture.raw, &deleted, 2)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        let record = move_photo(&fixture.raw).unwrap();
        assert_eq!(record.destination, deleted.join("DSC00001-3.ARW"));
        assert!(fs::symlink_metadata(deleted.join("DSC00001.ARW"))
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
