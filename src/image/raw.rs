//! Explicit, full-resolution sensor rendering. Normal browsing never enters here.
//!
//! Each background worker owns its decoder. Pixels are top-to-bottom sRGB RGBA8;
//! TIFF orientation is returned separately, just like the embedded JPEG pipeline.
//! RAW data is processed at its native precision, then converted for display.
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    marker::PhantomData,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::NonNull,
    rc::Rc,
};

const MAX_INPUT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_SCRATCH_BYTES: usize = 768 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const ERROR_CAPACITY: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawInfo {
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
    pub bytes: usize,
    /// Conservative decoder scratch estimate plus the RGBA output allocation.
    /// Lease `bytes` from the image cache; allow the remaining temporary scratch
    /// only for the single RAW worker. It is freed before returning the image.
    pub working_bytes: usize,
}

#[derive(Debug)]
pub struct RawImage {
    pub info: RawInfo,
    pub pixels: Vec<u8>,
}

#[repr(C)]
#[derive(Default)]
struct NativeInfo {
    width: u32,
    height: u32,
    orientation: u32,
    reserved: u32,
    scratch_bytes: u64,
}

extern "C" {
    fn fastcull_raw_create() -> *mut c_void;
    fn fastcull_raw_destroy(handle: *mut c_void);
    fn fastcull_raw_reset(handle: *mut c_void);
    fn fastcull_raw_probe(
        handle: *mut c_void,
        path: *const c_char,
        info: *mut NativeInfo,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
    fn fastcull_raw_decode(
        handle: *mut c_void,
        pixels: *mut u8,
        capacity: usize,
        cancelled: extern "C" fn(*mut c_void) -> c_int,
        context: *mut c_void,
        error: *mut c_char,
        error_capacity: usize,
    ) -> c_int;
}

/// Create inside the RAW worker; native state never crosses threads.
pub struct RawDecoder {
    handle: NonNull<c_void>,
    pending: Option<(PathBuf, Fingerprint, RawInfo)>,
    _thread_bound: PhantomData<Rc<()>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
}

fn fingerprint(path: &Path) -> Result<Fingerprint, String> {
    let metadata = path.metadata().map_err(|e| format!("RAW file: {e}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_INPUT_BYTES {
        return Err("RAW input is not a regular file within the 1 GiB limit".into());
    }
    Ok(Fingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
    })
}

fn checked_info(native: NativeInfo) -> Result<RawInfo, String> {
    if native.width == 0
        || native.height == 0
        || native.width > 16_384
        || native.height > 16_384
        || !(1..=8).contains(&native.orientation)
    {
        return Err("RAW dimensions or orientation exceed supported limits".into());
    }
    let bytes = (native.width as usize)
        .checked_mul(native.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&bytes| bytes <= MAX_OUTPUT_BYTES)
        .ok_or("RAW output exceeds the 256 MiB limit")?;
    let scratch = usize::try_from(native.scratch_bytes)
        .ok()
        .filter(|&bytes| bytes <= MAX_SCRATCH_BYTES)
        .ok_or("RAW working memory exceeds the supported limit")?;
    Ok(RawInfo {
        width: native.width,
        height: native.height,
        orientation: native.orientation as u16,
        bytes,
        working_bytes: bytes
            .checked_add(scratch)
            .ok_or("RAW working memory size overflow")?,
    })
}

fn native_error(error: &[c_char; ERROR_CAPACITY]) -> String {
    // Every bridge error uses bounded snprintf; the initial zeroes also cover
    // unexpected early exits without a message.
    let text = unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
    if text.is_empty() {
        "RAW decoder could not render this file".into()
    } else {
        text.into_owned()
    }
}

impl RawDecoder {
    pub fn new() -> Result<Self, String> {
        // The bridge catches all C++ allocation errors and returns null.
        let handle =
            NonNull::new(unsafe { fastcull_raw_create() }).ok_or("Unable to create RAW decoder")?;
        Ok(Self {
            handle,
            pending: None,
            _thread_bound: PhantomData,
        })
    }

    /// Read sensor metadata, retaining the open source for a subsequent decode.
    /// Callers can reserve the returned allocation before sensor decoding starts.
    pub fn probe(&mut self, path: &Path) -> Result<RawInfo, String> {
        self.pending = None;
        unsafe { fastcull_raw_reset(self.handle.as_ptr()) };
        let before = fingerprint(path)?;
        let path_c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "RAW path contains a NUL byte")?;
        let mut native = NativeInfo::default();
        let mut error = [0; ERROR_CAPACITY];
        let success = unsafe {
            fastcull_raw_probe(
                self.handle.as_ptr(),
                path_c.as_ptr(),
                &mut native,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if success == 0 {
            return Err(native_error(&error));
        }
        let info = checked_info(native)?;
        if fingerprint(path)? != before {
            unsafe { fastcull_raw_reset(self.handle.as_ptr()) };
            return Err("RAW file changed while reading its metadata".into());
        }
        self.pending = Some((path.to_path_buf(), before, info));
        Ok(info)
    }

    pub fn decode(&mut self, path: &Path) -> Result<RawImage, String> {
        self.decode_cancellable(path, &|| false)
    }

    /// Cancellation is checked between LibRaw processing stages and during the
    /// final pixel conversion. One native codec stage may finish before stopping.
    pub fn decode_cancellable(
        &mut self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<RawImage, String> {
        if cancelled() {
            self.reset();
            return Err("RAW decoding cancelled".into());
        }
        let reuse = self
            .pending
            .as_ref()
            .is_some_and(|(pending, _, _)| pending == path);
        if !reuse {
            self.probe(path)?;
        }
        let (_, before, info) = self.pending.take().ok_or("RAW metadata unavailable")?;
        if fingerprint(path)? != before {
            self.reset();
            return Err("RAW file changed before decoding".into());
        }
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(info.bytes)
            .map_err(|e| format!("Unable to allocate RAW output: {e}"))?;
        pixels.resize(info.bytes, 0);
        let mut error = [0; ERROR_CAPACITY];
        let mut callback = CancelContext { cancelled };
        let success = unsafe {
            fastcull_raw_decode(
                self.handle.as_ptr(),
                pixels.as_mut_ptr(),
                pixels.len(),
                cancel_callback,
                (&mut callback as *mut CancelContext<'_>).cast(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if success == 0 {
            return Err(native_error(&error));
        }
        if cancelled() || fingerprint(path)? != before {
            return Err("RAW decoding cancelled or source file changed".into());
        }
        Ok(RawImage { info, pixels })
    }

    pub fn reset(&mut self) {
        self.pending = None;
        unsafe { fastcull_raw_reset(self.handle.as_ptr()) };
    }
}

struct CancelContext<'a> {
    cancelled: &'a dyn Fn() -> bool,
}
extern "C" fn cancel_callback(context: *mut c_void) -> c_int {
    // This pointer lives on decode_cancellable's stack for the synchronous call.
    // A Rust panic must never unwind across the C++ boundary.
    let context = unsafe { &*context.cast::<CancelContext<'_>>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (context.cancelled)()))
        .map_or(1, i32::from)
}
impl Drop for RawDecoder {
    fn drop(&mut self) {
        unsafe { fastcull_raw_destroy(self.handle.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_native_dimensions_orientation_and_scratch() {
        for (width, height) in [(0, 1), (1, 0), (u32::MAX, 1), (16384, 16384)] {
            assert!(checked_info(NativeInfo {
                width,
                height,
                orientation: 1,
                ..NativeInfo::default()
            })
            .is_err());
        }
        for orientation in [0, 9, u32::MAX] {
            assert!(checked_info(NativeInfo {
                width: 1,
                height: 1,
                orientation,
                ..NativeInfo::default()
            })
            .is_err());
        }
        assert!(checked_info(NativeInfo {
            width: 1,
            height: 1,
            orientation: 1,
            scratch_bytes: u64::MAX,
            ..NativeInfo::default()
        })
        .is_err());
        let info = checked_info(NativeInfo {
            width: 7028,
            height: 4688,
            orientation: 6,
            scratch_bytes: 350 * 1024 * 1024,
            ..NativeInfo::default()
        })
        .unwrap();
        assert_eq!(info.bytes, 7028 * 4688 * 4);
        assert_eq!(info.working_bytes - info.bytes, 350 * 1024 * 1024);
    }

    #[test]
    fn native_decoder_rejects_nonraw_and_recovers_after_failure() {
        let mut decoder = RawDecoder::new().unwrap();
        assert!(decoder.probe(Path::new("/does/not/exist.ARW")).is_err());
        assert!(decoder.probe(Path::new("Cargo.toml")).is_err());
        assert!(decoder.pending.is_none());
        assert!(decoder.decode(Path::new("Cargo.toml")).is_err());
        assert!(decoder.pending.is_none());
        assert!(decoder
            .decode_cancellable(Path::new("/does/not/exist.ARW"), &|| true)
            .unwrap_err()
            .contains("cancelled"));
    }

    #[test]
    fn callback_panic_is_contained_at_ffi_boundary() {
        let callback = || panic!("cancel predicate panic");
        let mut context = CancelContext {
            cancelled: &callback,
        };
        assert_eq!(
            cancel_callback((&mut context as *mut CancelContext<'_>).cast()),
            1
        );
    }

    // Generated Bayer sensor data, not an embedded JPEG or a third-party photo.
    fn synthetic_dng() -> Vec<u8> {
        synthetic_dng_with_metadata(Vec::new())
    }

    fn synthetic_dng_with_metadata(mut tags: Vec<(u16, u16, Vec<u8>)>) -> Vec<u8> {
        for (tag, value) in [(256, 64u32), (257, 64), (273, 0), (278, 64), (279, 8192)] {
            tags.push((tag, 4, value.to_le_bytes().to_vec()));
        }
        for (tag, value) in [
            (258, 16u16),
            (259, 1),
            (262, 32803),
            (274, 6),
            (277, 1),
            (284, 1),
            (50711, 1),
            (50717, 65535),
            (50778, 21),
        ] {
            tags.push((tag, 3, value.to_le_bytes().to_vec()));
        }
        tags.extend([
            (271, 2, b"ARW Sprint\0".to_vec()),
            (272, 2, b"Synthetic DNG\0".to_vec()),
            (50708, 2, b"ARW Sprint Synthetic DNG\0".to_vec()),
            (33421, 3, vec![2, 0, 2, 0]),
            (33422, 1, vec![0, 1, 1, 2]),
            (50706, 1, vec![1, 4, 0, 0]),
            (50707, 1, vec![1, 1, 0, 0]),
            (50710, 1, vec![0, 1, 2]),
        ]);
        for (tag, values) in [
            (50714, vec![0u32]),
            (50728, vec![1, 1, 1]),
            (50721, vec![1, 0, 0, 0, 1, 0, 0, 0, 1]),
        ] {
            let data = values
                .iter()
                .flat_map(|v| v.to_le_bytes().into_iter().chain(1u32.to_le_bytes()))
                .collect();
            tags.push((tag, if tag == 50721 { 10 } else { 5 }, data));
        }
        tags.sort_by_key(|(tag, _, _)| *tag);
        let base = 8 + 2 + tags.len() * 12 + 4;
        let mut file = b"II*\0\x08\0\0\0".to_vec();
        file.extend_from_slice(&(tags.len() as u16).to_le_bytes());
        let mut payload = Vec::new();
        let mut strip_entry = 0;
        for (tag, kind, data) in tags {
            let unit = match kind {
                3 => 2,
                4 => 4,
                5 | 10 => 8,
                _ => 1,
            };
            file.extend_from_slice(&tag.to_le_bytes());
            file.extend_from_slice(&kind.to_le_bytes());
            file.extend_from_slice(&((data.len() / unit) as u32).to_le_bytes());
            if tag == 273 {
                strip_entry = file.len();
            }
            if data.len() <= 4 {
                file.extend_from_slice(&data);
                file.resize(file.len() + 4 - data.len(), 0);
            } else {
                file.extend_from_slice(&((base + payload.len()) as u32).to_le_bytes());
                payload.extend_from_slice(&data);
                if payload.len() % 2 != 0 {
                    payload.push(0);
                }
            }
        }
        file.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&payload);
        let raw_offset = file.len() as u32;
        file[strip_entry..strip_entry + 4].copy_from_slice(&raw_offset.to_le_bytes());
        for y in 0..64u16 {
            for x in 0..64u16 {
                file.extend_from_slice(&(8000 + x * 350 + y * 150).to_le_bytes());
            }
        }
        file
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            use std::{
                io::Write,
                sync::atomic::{AtomicU64, Ordering},
            };
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "arw-sprint-native-raw-{}-{}.dng",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            let fixture = Self(path);
            file.write_all(bytes).unwrap();
            fixture
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn oversized_and_repeated_metadata_are_rejected_before_sensor_allocation() {
        let mut decoder = RawDecoder::new().unwrap();
        for (count, bytes) in [(1, 9 * 1024 * 1024), (6, 3 * 1024 * 1024)] {
            let tags = (0..count).map(|_| (700, 1, vec![b' '; bytes])).collect();
            let fixture = Fixture::new(&synthetic_dng_with_metadata(tags));
            let error = decoder.probe(&fixture.0).unwrap_err();
            assert!(error.contains("metadata exceeds"), "{error}");
            assert!(decoder.pending.is_none());
        }
        let fixture = Fixture::new(&synthetic_dng());
        assert_eq!(
            decoder.decode(&fixture.0).unwrap().pixels.len(),
            64 * 64 * 4
        );
    }

    #[test]
    fn renders_sensor_only_dng_with_orientation_and_recovers_after_cancellation() {
        use std::cell::Cell;
        let fixture = Fixture::new(&synthetic_dng());
        let mut decoder = RawDecoder::new().unwrap();
        let info = decoder.probe(&fixture.0).unwrap();
        assert_eq!((info.width, info.height, info.orientation), (64, 64, 6));
        let expected = decoder.decode(&fixture.0).unwrap();
        assert_eq!(expected.pixels.len(), 64 * 64 * 4);
        assert!(expected.pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert_ne!(
            &expected.pixels[..4],
            &expected.pixels[expected.pixels.len() - 4..]
        );
        decoder.probe(&fixture.0).unwrap();
        let calls = Cell::new(0);
        let error = decoder
            .decode_cancellable(&fixture.0, &|| {
                calls.set(calls.get() + 1);
                calls.get() >= 4
            })
            .unwrap_err();
        assert!(error.to_lowercase().contains("cancel"), "{error}");
        assert_eq!(decoder.decode(&fixture.0).unwrap().pixels, expected.pixels);
    }
}
