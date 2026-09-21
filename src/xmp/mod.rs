//! Small, conservative XMP updates. Call these synchronous functions on the
//! application's serialized I/O worker, never on the window thread.
mod reader;
mod writer;

pub use reader::read_rating;
pub use writer::{replace_rating, write_rating};

use std::{
    fmt, io,
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

pub fn sidecar_path(raw_path: &Path) -> PathBuf {
    raw_path.with_extension("xmp")
}

pub(crate) fn checked_rating(rating: i8) -> Result<i8, XmpError> {
    if (-1..=5).contains(&rating) {
        Ok(rating)
    } else {
        Err(XmpError::Invalid("rating must be between -1 and 5"))
    }
}
