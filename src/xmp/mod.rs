//! Small, conservative XMP updates. Call these synchronous functions on the
//! application's serialized I/O worker, never on the window thread.
mod reader;
mod writer;

pub use reader::read_rating;
pub use writer::{replace_rating, write_rating};

use std::{
    fmt, fs, io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(crate) const MAX_SIDECAR_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
pub(crate) const RDF_NS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

#[derive(Debug)]
pub enum XmpError {
    Io(io::Error),
    Invalid(&'static str),
    Xml(String),
    Conflict,
}

impl fmt::Display for XmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "XMP I/O: {e}"),
            Self::Invalid(reason) => write!(f, "XMP left unchanged: {reason}"),
            Self::Xml(reason) => write!(f, "XMP left unchanged: invalid XML ({reason})"),
            Self::Conflict => write!(f, "XMP changed concurrently; update not saved"),
        }
    }
}
impl std::error::Error for XmpError {}
impl From<io::Error> for XmpError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// ARWs retain their conventional stem.xmp path. Additional formats include
/// their complete filename so a camera's same-stem RAW and JPEG cannot share
/// ratings or take each other's sidecar when moved into `_Rejected`.
pub fn sidecar_path(photo_path: &Path) -> PathBuf {
    if photo_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("arw"))
    {
        photo_path.with_extension("xmp")
    } else {
        let mut name = photo_path.as_os_str().to_os_string();
        name.push(".xmp");
        name.into()
    }
}

pub(crate) fn sidecar_candidates(photo_path: &Path) -> [PathBuf; 2] {
    let lower = sidecar_path(photo_path);
    let upper = lower.with_extension("XMP");
    [lower, upper]
}

/// A renamed `image.JPG.ARW` would use the same sidecar as `image.JPG`.
/// Refuse that rare ambiguity instead of ever borrowing the other photo's XMP.
/// Ordinary ARW names do not require any extra filesystem lookup.
pub(crate) fn ensure_sidecar_owner(photo_path: &Path) -> io::Result<()> {
    let check = |other: &Path| match fs::symlink_metadata(other) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} and {} would share a sidecar; metadata ownership is ambiguous",
                photo_path.display(),
                other.display()
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };
    if crate::browser::directory::is_arw(photo_path) {
        let other = photo_path.with_extension("");
        if crate::browser::directory::is_photo(&other) && !crate::browser::directory::is_arw(&other)
        {
            check(&other)?;
        }
    } else {
        for extension in ["arw", "arW", "aRw", "aRW", "Arw", "ArW", "ARw", "ARW"] {
            let mut other = photo_path.as_os_str().to_os_string();
            other.push(".");
            other.push(extension);
            check(Path::new(&other))?;
        }
    }
    Ok(())
}

/// Preserve the spelling of an existing uppercase sidecar on case-sensitive
/// volumes. If both spellings identify different files, leave both untouched.
pub(crate) fn resolved_sidecar_path(photo_path: &Path) -> Result<PathBuf, XmpError> {
    ensure_sidecar_owner(photo_path)?;
    let mut found = None;
    for path in sidecar_candidates(photo_path) {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() {
            return Err(XmpError::Invalid(
                "sidecar must be a regular file, not a symlink",
            ));
        }
        let identity = (metadata.dev(), metadata.ino());
        if let Some((_, previous)) = &found {
            if *previous != identity {
                return Err(XmpError::Invalid(
                    "both .xmp and .XMP sidecars exist; ownership is ambiguous",
                ));
            }
        } else {
            found = Some((path, identity));
        }
    }
    Ok(found.map_or_else(|| sidecar_path(photo_path), |(path, _)| path))
}

pub(crate) fn checked_rating(rating: i8) -> Result<i8, XmpError> {
    if (-1..=5).contains(&rating) {
        Ok(rating)
    } else {
        Err(XmpError::Invalid("rating must be between -1 and 5"))
    }
}
