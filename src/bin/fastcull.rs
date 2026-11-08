use fastcull::app::{self, Options};
use std::{env, path::PathBuf, process::ExitCode};

fn run() -> Result<(), String> {
    let mut options = Options {
        path: None,
        cache_mb: 512,
        smoke_test: None,
        raw_smoke_test: None,
        format_smoke_test: None,
        mutation_test: None,
        navigation_bench_ms: None,
        navigation_bench_zoom: false,
    };
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("ARW Sprint for Intel Mac\n\nfastcull [folder | photo.ARW|photo.DNG|photo.JPG] [--cache-mb 256|512|1024|2048]\n\nCmd+O opens a folder; Cmd+R checks it for changes made outside the app.\nArrow keys browse; Shift+Space goes back.\nSpace/X moves the displayed photo and XMP into _Rejected, then advances. 1–5 rate.\nR toggles RAW for this photo; Shift+R toggles RAW browsing (one neighbor ahead).\nF fullscreen, I photo details (EXIF), Z fit/100%, S/+ zoom in, D/- zoom out, pinch zoom.\nTwo-finger scroll or click-drag pans; pinch zooms. Shift+scroll zooms.\nHold Shift and move the pointer up/down to zoom in/out; no click needed.\nCmd+, customizes keyboard shortcuts; Help shows the current bindings.\nL toggles keeping zoom and position (on by default), hold P peeks at 100%, C pins/closes comparison, Shift+C replaces reference.\nA toggles auto-advance after ratings; Cmd+Z undoes a rating or local move this session.\n] brightens, [ darkens by 1/3 stop; backslash resets viewing brightness.\nLast photo, brightness and L/A preferences resume on launch.\nFASTCULL_LOG=1 prints pipeline timings and cache statistics.\n\n--smoke-test NEW_OUTPUT_FOLDER captures only ARW Sprint's rendered frames,\nexercises read-only navigation/zoom/resize, then exits. Never changes ratings or moves files.\n--raw-smoke-test NEW_OUTPUT_FOLDER checks RAW/preview switching and cancellation on camera files.\n--mutation-test NEW_OUTPUT_FOLDER tests moves, undo and refresh on generated fixtures only.\n--navigation-bench-ms 20..2000 measures 32 read-only forward requests after a 3s warmup,\nwithout screenshots or preference writes; needs at least 33 photos.\n--navigation-bench-zoom measures at 100% instead of fit.");
                return Ok(());
            }
            Some("--navigation-bench-ms") => {
                options.navigation_bench_ms = Some(
                    args.next()
                        .and_then(|value| value.to_str().and_then(|s| s.parse().ok()))
                        .ok_or("--navigation-bench-ms needs 20..2000")?,
                );
            }
            Some("--navigation-bench-zoom") => options.navigation_bench_zoom = true,
            Some("--cache-mb") => {
                options.cache_mb = args
                    .next()
                    .and_then(|a| a.to_str().and_then(|s| s.parse().ok()))
                    .ok_or("--cache-mb needs 256, 512, 1024, or 2048")?;
                if ![256, 512, 1024, 2048].contains(&options.cache_mb) {
                    return Err("--cache-mb needs 256, 512, 1024, or 2048".into());
                }
            }
            Some("--format-smoke-test") => {
                options.format_smoke_test = Some(PathBuf::from(
                    args.next()
                        .ok_or("--format-smoke-test needs a new output directory")?,
                ));
            }
            Some("--raw-smoke-test") => {
                options.raw_smoke_test = Some(PathBuf::from(
                    args.next()
                        .ok_or("--raw-smoke-test needs a new output directory")?,
                ));
            }
            Some("--smoke-test") => {
                options.smoke_test = Some(PathBuf::from(
                    args.next()
                        .ok_or("--smoke-test needs a new output directory")?,
                ))
            }
            Some("--mutation-test") => {
                options.mutation_test = Some(PathBuf::from(
                    args.next()
                        .ok_or("--mutation-test needs a new output directory")?,
                ));
            }
            Some(value) if value.starts_with("-psn_") => {} // Finder on older macOS.
            Some(value) if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"))
            }
            _ if options.path.is_none() => options.path = Some(arg.into()),
            _ => return Err("provide one folder or ARW path".into()),
        }
    }
    if options.smoke_test.is_some() && options.path.is_none() {
        return Err("--smoke-test requires a folder or ARW path".into());
    }
    app::run(options)
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fastcull: {error}");
            ExitCode::FAILURE
        }
    }
}
