//! Optional JPEG decode backends for measured, like-for-like RGBA comparisons.

pub mod cache;
pub mod decoder;
pub mod loader;

#[cfg(feature = "desktop")]
#[allow(unsafe_code)]
pub mod raw;
pub mod source;
