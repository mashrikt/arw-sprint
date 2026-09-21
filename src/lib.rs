#![deny(unsafe_code)]

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
compile_error!("FastCull supports Intel macOS only; use --target x86_64-apple-darwin");

#[cfg(feature = "desktop")]
pub mod app;
pub mod arw;
pub mod browser;
pub mod image;
#[cfg(feature = "desktop")]
pub mod metadata;
#[cfg(feature = "desktop")]
pub mod renderer;
#[cfg(feature = "desktop")]
pub mod xmp;
// Narrow application FFI: Cocoa open-document handling and atomic native rename.
#[cfg(feature = "desktop")]
#[allow(unsafe_code)]
pub mod platform;
