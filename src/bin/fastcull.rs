use fastcull::app::{self, Options};
use std::{env, path::PathBuf, process::ExitCode};

fn run() -> Result<(), String> {
    let mut options = Options {
        path: None,
        cache_mb: 512,
        smoke_test: None,
    };
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("FastCull — Intel macOS Sony ARW viewer\n\nfastcull [folder | photo.ARW] [--cache-mb 256|512|1024|2048]\n\nCmd+O opens a folder. Arrow keys browse; Shift+Space goes back.\nSpace/X moves the displayed photo and XMP into deleted, then advances. 1–5 rate.\nF fullscreen, Z fit/100%, S/+ zoom in, D/- zoom out, wheel/pinch zoom, drag pan.\nL locks zoom, hold P peeks at 100%, C pins/closes comparison, Shift+C replaces reference.\nA toggles auto-advance after ratings; Cmd+Z undoes a rating or local move this session.\nLast photo and L/A preferences resume on launch.\nFASTCULL_LOG=1 prints pipeline timings and cache statistics.\n\n--smoke-test NEW_OUTPUT_FOLDER captures only FastCull's rendered frames,\nexercises read-only navigation/zoom/resize, then exits. Never changes ratings or moves files.");
                return Ok(());
            }
            Some("--cache-mb") => {
                options.cache_mb = args
                    .next()
                    .and_then(|a| a.to_str().and_then(|s| s.parse().ok()))
                    .ok_or("--cache-mb needs 256, 512, 1024, or 2048")?;
                if ![256, 512, 1024, 2048].contains(&options.cache_mb) {
                    return Err("--cache-mb needs 256, 512, 1024, or 2048".into());
                }
            }
            Some("--smoke-test") => {
                options.smoke_test = Some(PathBuf::from(
                    args.next()
                        .ok_or("--smoke-test needs a new output directory")?,
                ))
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
