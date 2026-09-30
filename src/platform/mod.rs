//! Narrow macOS bridges for document-open events and atomic file publication.
//! Winit 0.30 handles window drops but does not expose these application events.
use std::{
    ffi::{c_char, CStr, CString, OsStr},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

static OPEN_CALLBACK: OnceLock<Box<dyn Fn(PathBuf) + Send + Sync>> = OnceLock::new();

extern "C" {
    fn fastcull_install_open_handler(callback: extern "C" fn(*const c_char));
    fn fastcull_edit_shortcuts(rows: *const c_char, error: *const c_char) -> *mut c_char;
    fn fastcull_free_shortcut_result(result: *mut c_char);
    fn fastcull_post_shortcut_test_key(character: *const c_char, command: u8, shift: u8);
    fn fastcull_draw_photo_details(
        content: *const c_char,
        rgba: *mut u8,
        width: u32,
        height: u32,
        scale: f64,
    ) -> i32;
    fn renamex_np(from: *const c_char, to: *const c_char, flags: u32) -> i32;
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// Rasterize a small offscreen card, never a native view. Safe to call from the
/// dedicated details worker: Cocoa drawing state and autoreleases are local.
pub(crate) fn rasterize_photo_details(
    content: &str,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width > 2048 || height > 2048 {
        return Err("photo details bitmap dimensions are out of range".into());
    }
    let content = CString::new(content).map_err(|_| "photo details contain NUL")?;
    let length = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("photo details bitmap overflow")?;
    let mut rgba = vec![0; length];
    // Rust owns the exactly-sized writable buffer and the C string throughout
    // the synchronous call. Native code bounds-checks and retains neither.
    let success = unsafe {
        fastcull_draw_photo_details(content.as_ptr(), rgba.as_mut_ptr(), width, height, scale)
    };
    if success == 0 {
        return Err("could not rasterize photo details".into());
    }
    Ok(rgba)
}

/// Open the native shortcut editor on the main thread. Rows are tab-separated
/// action IDs, labels and bindings. Cancel returns `None`; restoring defaults
/// returns an empty configuration. Validation and persistence stay in Rust.
pub fn edit_shortcuts(rows: &str, error: Option<&str>) -> Option<String> {
    let rows = CString::new(rows).ok()?;
    let error = error.map(CString::new).transpose().ok()?;
    // Both inputs remain alive throughout the synchronous modal editor. Cocoa
    // copies their strings and returns either NULL or a strdup-owned UTF-8 string.
    let result = unsafe {
        fastcull_edit_shortcuts(
            rows.as_ptr(),
            error
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
        )
    };
    if result.is_null() {
        return None;
    }
    // Copy before freeing through the matching C allocator, even if an invalid
    // UTF-8 result is returned. No foreign pointer escapes this function.
    let owned = unsafe { CStr::from_ptr(result) }
        .to_str()
        .ok()
        .map(str::to_owned);
    unsafe { fastcull_free_shortcut_result(result) };
    owned
}

/// Inject a key only into this app for the opt-in desktop smoke test. This is
/// main-thread-only, uses AppKit's local event queue, and requires no input access.
pub(crate) fn post_shortcut_test_key(character: &str, command: bool, shift: bool) {
    let Ok(character) = CString::new(character) else {
        return;
    };
    // Cocoa copies the string synchronously; no pointer is retained.
    unsafe {
        fastcull_post_shortcut_test_key(character.as_ptr(), u8::from(command), u8::from(shift))
    };
}

/// Mark only the calling auxiliary thread as utility work. This cannot lower
/// the main thread or change priorities of unrelated decode/upload workers.
pub(crate) fn set_thumbnail_thread_qos() -> io::Result<()> {
    // QOS_CLASS_UTILITY = 0x11 in the macOS SDK sys/qos.h (10.10+).
    // This C function accepts scalar values and changes the calling thread.
    let result = unsafe { pthread_set_qos_class_self_np(0x11, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

/// Atomically publish a file only when `destination` does not already exist.
/// macOS `renamex_np(RENAME_EXCL)` avoids the check/rename race and does not
/// require hard-link support on the filesystem. Unsupported volumes fail safely.
pub(crate) fn rename_exclusive(source: &Path, destination: &Path) -> io::Result<()> {
    let from = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // RENAME_EXCL is 0x00000004 in the macOS SDK's sys/stdio.h, available 10.12+.
    // Both CString buffers stay alive across this synchronous call. The C
    // function only reads their NUL-terminated bytes and retains no pointers.
    let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 0x0000_0004) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

extern "C" fn opened_path(path: *const c_char) {
    if path.is_null() {
        return;
    }
    // Cocoa supplies a NUL-terminated filesystem representation valid for this
    // synchronous call. Copy it before returning; never retain the foreign pointer.
    let owned = unsafe { PathBuf::from(OsStr::from_bytes(CStr::from_ptr(path).to_bytes())) };
    if let Some(callback) = OPEN_CALLBACK.get() {
        // Do not unwind through Objective-C. The callback only wakes the event loop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(owned)));
    }
}

/// Call once on the main thread, after Winit has initialized NSApplication.
pub fn install_open_handler(callback: impl Fn(PathBuf) + Send + Sync + 'static) {
    if OPEN_CALLBACK.set(Box::new(callback)).is_ok() {
        // This function retains its Cocoa handler until process exit and does not
        // take ownership of Rust data. `opened_path` is a process-lifetime function.
        unsafe { fastcull_install_open_handler(opened_path) };
    }
}

#[cfg(test)]
mod filesystem_tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn exclusive_rename_never_replaces_an_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "fastcull-rename-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let source = dir.join("temporary.xmp");
        let destination = dir.join("existing.xmp");
        fs::write(&source, b"new rating").unwrap();
        fs::write(&destination, b"external edit").unwrap();
        assert_eq!(
            rename_exclusive(&source, &destination).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&source).unwrap(), b"new rating");
        assert_eq!(fs::read(&destination).unwrap(), b"external edit");
        fs::remove_file(&destination).unwrap();
        rename_exclusive(&source, &destination).unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"new rating");
        fs::remove_dir_all(dir).unwrap();
    }
}
