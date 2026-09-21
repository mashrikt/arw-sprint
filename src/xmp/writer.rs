use super::{
    checked_rating, reader::parse, sidecar_path, XmpError, MAX_SIDECAR_BYTES, RDF_NS, XMP_NS,
};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

// Darwin O_NOFOLLOW: opening a replaced symlink must fail, not follow it.
const O_NOFOLLOW: i32 = 0x0000_0100;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) struct Snapshot {
    pub bytes: Vec<u8>,
    metadata: Metadata,
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.mode() == b.mode()
}

pub(super) fn snapshot(path: &Path) -> Result<Option<Snapshot>, XmpError> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(XmpError::Invalid(
            "sidecar must be a regular file, not a symlink",
        ));
    }
    if before.len() > MAX_SIDECAR_BYTES {
        return Err(XmpError::Invalid("sidecar exceeds 8 MiB"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)?;
    if !same_file(&before, &file.metadata()?) {
        return Err(XmpError::Conflict);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SIDECAR_BYTES {
        return Err(XmpError::Invalid("sidecar exceeds 8 MiB"));
    }
    let after = file.metadata()?;
    if !same_file(&before, &after) || !same_file(&after, &fs::symlink_metadata(path)?) {
        return Err(XmpError::Conflict);
    }
    Ok(Some(Snapshot {
        bytes,
        metadata: after,
    }))
}

fn patch_optional(original: Option<&[u8]>, rating: Option<i8>) -> Result<Vec<u8>, XmpError> {
    let rating = rating.map(checked_rating).transpose()?;
    let Some(bytes) = original else {
        let Some(rating) = rating else {
            return Ok(Vec::new());
        };
        return Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"{RDF_NS}\">\n  <rdf:Description rdf:about=\"\" xmlns:xmp=\"{XMP_NS}\" xmp:Rating=\"{rating}\"/>\n </rdf:RDF>\n</x:xmpmeta>\n"
        ).into_bytes());
    };
    let document = parse(bytes)?;
    let (span, replacement) = match (document.rating, rating) {
        (Some(field), Some(rating)) => {
            if field.value == rating {
                return Ok(bytes.to_vec());
            }
            (field.span, rating.to_string())
        }
        (None, Some(rating)) => (
            document.insertion..document.insertion,
            format!(
                " xmlns:{}=\"{XMP_NS}\" {}:Rating=\"{rating}\"",
                document.prefix, document.prefix
            ),
        ),
        (Some(field), None) => (field.property_span, String::new()),
        (None, None) => return Ok(bytes.to_vec()),
    };
    let new_length = bytes.len() - span.len() + replacement.len();
    if new_length as u64 > MAX_SIDECAR_BYTES {
        return Err(XmpError::Invalid("updated sidecar exceeds 8 MiB"));
    }
    let mut output = Vec::with_capacity(new_length);
    output.extend_from_slice(&bytes[..span.start]);
    output.extend_from_slice(replacement.as_bytes());
    output.extend_from_slice(&bytes[span.end..]);
    // A malformed range or namespace insertion must never reach the filesystem.
    if parse(&output)?.rating.map(|field| field.value) != rating {
        return Err(XmpError::Invalid("rating patch failed validation"));
    }
    Ok(output)
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary(parent: &Path) -> Result<(TempFile, File), XmpError> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".fastcull-xmp-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((TempFile(path), file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(XmpError::Invalid(
        "could not allocate a unique sidecar temporary file",
    ))
}

fn parent_directory(path: &Path) -> Result<(PathBuf, Metadata), XmpError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(XmpError::Invalid(
            "sidecar parent must be a directory, not a symlink",
        ));
    }
    Ok((parent.to_path_buf(), metadata))
}

fn replace_impl(
    raw_path: &Path,
    rating: Option<i8>,
    expected: Option<Option<i8>>,
    before_commit: impl FnOnce(&Path),
) -> Result<Option<i8>, XmpError> {
    rating.map(checked_rating).transpose()?;
    if !raw_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("arw"))
    {
        return Err(XmpError::Invalid("ratings require an ARW source path"));
    }
    let path = sidecar_path(raw_path);
    let (parent, parent_before) = parent_directory(&path)?;
    let original = snapshot(&path)?;
    let previous = original
        .as_ref()
        .map(|source| parse(&source.bytes).map(|document| document.rating.map(|field| field.value)))
        .transpose()?
        .flatten();
    if expected.is_some_and(|value| value != previous) {
        return Err(XmpError::Conflict);
    }
    if original
        .as_ref()
        .is_some_and(|source| source.metadata.permissions().readonly())
    {
        return Err(XmpError::Invalid("existing sidecar is read-only"));
    }
    let updated = patch_optional(
        original.as_ref().map(|source| source.bytes.as_slice()),
        rating,
    )?;
    if (original.is_none() && updated.is_empty())
        || original
            .as_ref()
            .is_some_and(|source| source.bytes == updated)
    {
        return Ok(previous);
    }
    let (temp, mut file) = temporary(&parent)?;
    file.write_all(&updated)?;
    if let Some(source) = &original {
        file.set_permissions(source.metadata.permissions())?;
    }
    file.sync_all()?;
    drop(file);
    before_commit(&path);

    let parent_after = fs::symlink_metadata(&parent)?;
    if parent_after.file_type().is_symlink()
        || parent_before.dev() != parent_after.dev()
        || parent_before.ino() != parent_after.ino()
    {
        return Err(XmpError::Conflict);
    }
    let latest = snapshot(&path)?;
    let unchanged = match (&original, &latest) {
        (None, None) => true,
        (Some(a), Some(b)) => same_file(&a.metadata, &b.metadata) && a.bytes == b.bytes,
        _ => false,
    };
    if !unchanged {
        return Err(XmpError::Conflict);
    }
    if original.is_some() {
        // Atomic replacement. The immediate snapshot check prevents ordinary
        // external-editor conflicts; std's path-based rename cannot close the
        // final check/rename race with an uncooperative simultaneous writer.
        fs::rename(&temp.0, &path)?;
    } else {
        // Native exclusive rename is atomic without requiring filesystem
        // hard-link support, and cannot clobber a concurrently created sidecar.
        crate::platform::rename_exclusive(&temp.0, &path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                XmpError::Conflict
            } else {
                XmpError::Io(error)
            }
        })?;
    }
    // The data was synced before publication. Directory fsync is best-effort:
    // some macOS/external-volume filesystems reject syncing directory handles.
    if let Ok(directory) = File::open(&parent) {
        let _ = directory.sync_all();
    }
    Ok(previous)
}

/// Preserve unrelated XML bytes and file permissions; publish through a synced
/// sibling temporary file. Existing malformed/ambiguous sidecars are untouched.
/// Call on a serialized worker; conflicts should be surfaced, never auto-retried.
pub fn write_rating(raw_path: &Path, rating: i8) -> Result<(), XmpError> {
    replace_rating(raw_path, Some(rating), None).map(|_| ())
}

/// Replace only the rating and return its exact previous value from the same
/// snapshot used for the atomic update. `None` removes the rating property; an
/// existing sidecar remains in place so unrelated metadata is never deleted.
/// If `expected` is `Some(value)`, refuse updates when the current rating differs
/// from that value. This lets undo preserve later edits made by other programs.
pub fn replace_rating(
    raw_path: &Path,
    rating: Option<i8>,
    expected: Option<Option<i8>>,
) -> Result<Option<i8>, XmpError> {
    replace_impl(raw_path, rating, expected, |_| {})
}

#[cfg(test)]
fn patch(original: Option<&[u8]>, rating: i8) -> Result<Vec<u8>, XmpError> {
    patch_optional(original, Some(rating))
}

#[cfg(test)]
fn write_impl(
    raw_path: &Path,
    rating: i8,
    before_commit: impl FnOnce(&Path),
) -> Result<(), XmpError> {
    replace_impl(raw_path, Some(rating), None, before_commit).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::read_rating;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn xmp(body: &str) -> String {
        format!("<?xpacket begin='\u{feff}'?>\n<x:xmpmeta xmlns:x='adobe:ns:meta/'><r:RDF xmlns:r='{RDF_NS}'><r:Description r:about='' xmlns:q='{XMP_NS}'>{body}</r:Description></r:RDF></x:xmpmeta><?xpacket end='w'?>")
    }
    fn parsed(bytes: &[u8]) -> Option<i8> {
        parse(bytes).unwrap().rating.map(|field| field.value)
    }

    #[test]
    fn attribute_update_preserves_every_unrelated_byte_and_entities() {
        let original = format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description xmlns:unusual='{XMP_NS}' unusual:Rating = '&#51;' xmlns:dc='http://purl.org/dc/elements/1.1/' dc:format='raw&amp;kept'><!--keep--></r:Description></r:RDF>");
        let changed = patch(Some(original.as_bytes()), -1).unwrap();
        assert_eq!(changed, original.replace("&#51;", "-1").as_bytes());
        assert_eq!(parsed(&changed), Some(-1));
    }
    #[test]
    fn element_and_cdata_updates_only_change_value_bytes() {
        for property in [
            "<q:Rating> \t&#x32;\n</q:Rating>",
            "<q:Rating><![CDATA[  2  ]]></q:Rating>",
        ] {
            let original = xmp(property);
            let changed = patch(Some(original.as_bytes()), 5).unwrap();
            let expected = original.replace("&#x32;", "5").replace("  2  ", "  5  ");
            assert_eq!(changed, expected.as_bytes());
            assert_eq!(parsed(&changed), Some(5));
        }
    }
    #[test]
    fn rating_removal_preserves_all_bytes_outside_the_property() {
        let property = "unusual:Rating = '&#51;'";
        let original = format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description xmlns:unusual='{XMP_NS}' {property} xmlns:dc='http://purl.org/dc/elements/1.1/' dc:format='raw&amp;kept'><!--keep--></r:Description></r:RDF>");
        let changed = patch_optional(Some(original.as_bytes()), None).unwrap();
        assert_eq!(changed, original.replace(property, "").as_bytes());
        assert_eq!(parsed(&changed), None);
        for property in [
            "<q:Rating> \t&#x32;\n</q:Rating>",
            "<q:Rating><![CDATA[  2  ]]></q:Rating>",
            "<Rating xmlns='http://ns.adobe.com/xap/1.0/'>3</Rating>",
        ] {
            let original = xmp(&format!("<!--before-->{property}<!--after-->"));
            let changed = patch_optional(Some(original.as_bytes()), None).unwrap();
            assert_eq!(changed, original.replace(property, "").as_bytes());
            assert_eq!(parsed(&changed), None);
        }
        let original = xmp("<!--no rating-->");
        assert_eq!(
            patch_optional(Some(original.as_bytes()), None).unwrap(),
            original.as_bytes()
        );
    }
    #[test]
    fn namespace_shadowing_and_default_namespaces_are_resolved() {
        let xml = format!("<RDF xmlns='{RDF_NS}' xmlns:p='wrong'><Description><p:Rating xmlns:p='{XMP_NS}'>4</p:Rating><other xmlns='urn:other' p:Rating='untouched'/></Description></RDF>");
        let changed = patch(Some(xml.as_bytes()), 1).unwrap();
        assert_eq!(changed, xml.replace(">4<", ">1<").as_bytes());
        let xml = format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description><Rating xmlns='{XMP_NS}'>3</Rating></r:Description></r:RDF>");
        assert_eq!(parsed(xml.as_bytes()), Some(3));
    }

    #[test]
    fn namespace_entities_unicode_prefixes_and_utf8_bom_are_preserved() {
        let original = format!("\u{feff}<?xml version='1.0' encoding='utf-8' standalone='yes'?><r:RDF xmlns:r='{RDF_NS}'><r:Description xmlns:評価='http://ns.adobe.com/xap/1.0&#47;' 評価:Rating='3'/></r:RDF>");
        let changed = patch(Some(original.as_bytes()), 4).unwrap();
        assert_eq!(
            changed,
            original.replace("Rating='3'", "Rating='4'").as_bytes()
        );
        assert_eq!(parsed(&changed), Some(4));
    }
    #[test]
    fn inserts_namespace_only_at_description_without_rewriting_packet() {
        for ending in ["/>", "></r:Description>"] {
            let xml = format!("<r:RDF xmlns:r='{RDF_NS}' xmlns:fastcullXmp='urn:other'><r:Description r:about='' {ending}</r:RDF>");
            let output = patch(Some(xml.as_bytes()), 3).unwrap();
            let insertion = format!(" xmlns:fastcullXmp1=\"{XMP_NS}\" fastcullXmp1:Rating=\"3\"");
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output.replace(&insertion, ""), xml);
            assert_eq!(parsed(output.as_bytes()), Some(3));
        }
    }
    #[test]
    fn malformed_ambiguous_and_unsupported_sidecars_are_rejected() {
        let cases = [
            xmp("<q:Rating>1</q:Rating><q:Rating>1</q:Rating>"),
            xmp("<q:Rating>6</q:Rating>"),
            xmp("<q:Rating/>"),
            xmp("<q:Rating><q:x>2</q:x></q:Rating>"),
            xmp("<q:Rating>2<!--x--></q:Rating>"),
            xmp("<q:Rating>&missing;</q:Rating>"),
            xmp("<q:Rating>1</q:Other>"),
            format!("<!DOCTYPE r [<!ENTITY x '3'>]>{}", xmp("<q:Rating>3</q:Rating>")),
            format!("<?xml version='1.0' encoding='UTF-16'?>{}", xmp("")),
            format!(" <?xml version='1.0'?>{}", xmp("")),
            format!("<?XML version='1.0'?>{}", xmp("")),
            format!("<?xml version='1.0' encoding='UTF-8' encoding='UTF-16'?>{}", xmp("")),
            format!("<?xml version='1.0' standalone='yes' encoding='UTF-8'?>{}", xmp("")),
            format!("<?xml version='1.0' unexpected='value'?>{}", xmp("")),
            xmp("<q:Rating>2</q:Rating>]]>"),
            format!("{}{}", xmp(""), xmp("")),
            format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description xmlns:a='{XMP_NS}' xmlns:b='{XMP_NS}' a:Rating='2' b:Rating='3'/></r:RDF>"),
            format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description xmlns:a='{XMP_NS}' a:Rating='2'><a:Rating>2</a:Rating></r:Description></r:RDF>"),
            format!("<r:RDF xmlns:r='{RDF_NS}'><r:Description r:about='somebody-else'/></r:RDF>"),
        ];
        for xml in cases {
            assert!(patch(Some(xml.as_bytes()), 4).is_err(), "accepted {xml}");
        }
        assert!(patch(Some(&[0xff, 0xfe, 0, 0]), 1).is_err());
        assert!(patch(None, -2).is_err());
        assert!(patch(None, 6).is_err());
    }

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fastcull-xmp-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn raw(&self) -> PathBuf {
            self.0.join("synthetic.ARW")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_persistence_preserves_arw_and_permissions_and_leaves_no_temporaries() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        fs::write(&raw, b"synthetic raw untouched").unwrap();
        assert_eq!(read_rating(&raw).unwrap(), None);
        write_rating(&raw, -1).unwrap();
        assert_eq!(read_rating(&raw).unwrap(), Some(-1));
        let sidecar = sidecar_path(&raw);
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o640)).unwrap();
        for rating in 0..=5 {
            write_rating(&raw, rating).unwrap();
            assert_eq!(read_rating(&raw).unwrap(), Some(rating));
        }
        assert_eq!(fs::metadata(&sidecar).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read(&raw).unwrap(), b"synthetic raw untouched");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
    }
    #[test]
    fn exact_absence_and_expected_rating_are_preserved_atomically() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        let sidecar = sidecar_path(&raw);
        // Undo must not invent Rating=0 or create a new sidecar for no change.
        assert_eq!(replace_rating(&raw, None, Some(None)).unwrap(), None);
        assert!(!sidecar.exists());
        assert_eq!(replace_rating(&raw, Some(5), Some(None)).unwrap(), None);
        let five = fs::read(&sidecar).unwrap();
        assert!(matches!(
            replace_rating(&raw, None, Some(Some(3))),
            Err(XmpError::Conflict)
        ));
        assert_eq!(fs::read(&sidecar).unwrap(), five);
        assert_eq!(replace_rating(&raw, None, Some(Some(5))).unwrap(), Some(5));
        assert!(sidecar.exists(), "preserve existing sidecar and namespaces");
        assert_eq!(read_rating(&raw).unwrap(), None);
        assert_eq!(replace_rating(&raw, Some(0), Some(None)).unwrap(), None);
        assert_eq!(read_rating(&raw).unwrap(), Some(0));
        assert_eq!(replace_rating(&raw, None, Some(Some(0))).unwrap(), Some(0));
        assert_eq!(read_rating(&raw).unwrap(), None);
    }
    #[test]
    fn removal_rejects_external_changes_during_atomic_commit() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        write_rating(&raw, 2).unwrap();
        let external = patch(None, 4).unwrap();
        let result = replace_impl(&raw, None, Some(Some(2)), |path| {
            fs::write(path, &external).unwrap()
        });
        assert!(matches!(result, Err(XmpError::Conflict)));
        assert_eq!(fs::read(sidecar_path(&raw)).unwrap(), external);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
    #[test]
    fn malformed_readonly_and_symlink_targets_remain_untouched() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        let sidecar = sidecar_path(&raw);
        fs::write(&sidecar, b"<broken").unwrap();
        assert!(write_rating(&raw, 1).is_err());
        assert_eq!(fs::read(&sidecar).unwrap(), b"<broken");
        let original = patch(None, 2).unwrap();
        fs::write(&sidecar, &original).unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(write_rating(&raw, 1).is_err());
        assert_eq!(fs::read(&sidecar).unwrap(), original);
        fs::remove_file(&sidecar).unwrap();
        let target = directory.0.join("unrelated.xml");
        fs::write(&target, &original).unwrap();
        symlink(&target, &sidecar).unwrap();
        assert!(write_rating(&raw, 5).is_err());
        assert!(read_rating(&raw).is_err());
        assert_eq!(fs::read(&target).unwrap(), original);
    }
    #[test]
    fn concurrent_changes_are_not_overwritten_and_temp_is_cleaned() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        let sidecar = sidecar_path(&raw);
        write_rating(&raw, 1).unwrap();
        let external = patch(None, 4).unwrap();
        let result = write_impl(&raw, 5, |path| fs::write(path, &external).unwrap());
        assert!(matches!(result, Err(XmpError::Conflict)));
        assert_eq!(fs::read(&sidecar).unwrap(), external);
        fs::remove_file(&sidecar).unwrap();
        let result = write_impl(&raw, 5, |path| fs::write(path, &external).unwrap());
        assert!(matches!(result, Err(XmpError::Conflict)));
        assert_eq!(fs::read(&sidecar).unwrap(), external);
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
    }
    #[test]
    fn refuses_oversized_sidecar_before_loading_it() {
        let directory = TestDirectory::new();
        let raw = directory.raw();
        let file = File::create(sidecar_path(&raw)).unwrap();
        file.set_len(MAX_SIDECAR_BYTES + 1).unwrap();
        assert!(read_rating(&raw).is_err());
        assert!(write_rating(&raw, 1).is_err());
        assert_eq!(file.metadata().unwrap().len(), MAX_SIDECAR_BYTES + 1);
    }
}
