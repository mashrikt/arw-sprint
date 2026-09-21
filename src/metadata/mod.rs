//! Bounded TIFF/EXIF reads for the background metadata worker.
mod exif;

pub use exif::{read_exif, ExifMetadata, MetadataError};
