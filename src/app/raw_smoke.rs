//! Read-only end-to-end source switching checks on actual camera files.
use super::{ActiveEventLoop, App, Duration, Instant};
use crate::renderer::Viewport;
use std::path::PathBuf;
pub(super) struct Smoke {
    directory: PathBuf,
    step: u8,
    started: Instant,
    next: Instant,
    view: Option<Viewport>,
    cached_path: Option<PathBuf>,
    mixed: bool,
}
impl Smoke {
    pub fn new(directory: PathBuf) -> Result<Self, String> {
        std::fs::create_dir(&directory).map_err(|e| format!("RAW smoke output: {e}"))?;
        Ok(Self {
            directory,
            step: 0,
            started: Instant::now(),
            next: Instant::now(),
            view: None,
            cached_path: None,
            mixed: false,
        })
    }

    /// Dedicated generated-fixture diagnostic; ordinary camera RAW smoke keeps
    /// its existing source-switch and neighbor-prefetch sequence unchanged.
    pub fn new_mixed(directory: PathBuf) -> Result<Self, String> {
        let mut smoke = Self::new(directory)?;
        smoke.mixed = true;
        Ok(smoke)
    }
}
fn same_view(app: &App, before: &Viewport) -> Result<(), String> {
    let after = app.renderer.as_ref().ok_or("renderer missing")?.viewport();
    if (after.scale() - before.scale()).abs() > 0.00001
        || after
            .normalized_focus()
            .iter()
            .zip(before.normalized_focus())
            .any(|(a, b)| (a - b).abs() > 0.00001)
    {
        return Err("RAW/preview switch changed zoom or focus".into());
    }
    Ok(())
}
pub(super) fn tick(app: &mut App, event_loop: &ActiveEventLoop) {
    let Some(mut smoke) = app.raw_smoke.take() else {
        return;
    };
    if Instant::now() < smoke.next {
        app.raw_smoke = Some(smoke);
        return;
    }
    let result = (|| -> Result<bool, String> {
        if smoke.started.elapsed() > Duration::from_secs(90) {
            return Err(format!(
                "{} smoke timed out at step {}: {}",
                if smoke.mixed { "Mixed format" } else { "RAW" },
                smoke.step,
                app.notice
            ));
        }
        if smoke.mixed {
            return mixed_tick(app, &mut smoke);
        }
        let ready = app.current().is_some() && app.current() == app.displayed;
        match smoke.step {
            0 if ready && app.files.len() >= 3 && !app.scanning => {
                app.zoom_locked = false;
                let renderer = app.renderer.as_mut().ok_or("renderer missing")?;
                renderer.set_preserve_view(false);
                renderer.viewport_mut().actual_size();
                renderer.viewport_mut().pan(80.0, -40.0);
                smoke.view = Some(renderer.viewport().clone());
                renderer.capture(&smoke.directory.join("preview.ppm"))?;
                app.toggle_raw(false);
                smoke.step = 1;
            }
            1 if ready && app.raw.displayed => {
                same_view(app, smoke.view.as_ref().unwrap())?;
                app.renderer
                    .as_mut()
                    .unwrap()
                    .capture(&smoke.directory.join("raw.ppm"))?;
                // A RAW comparison reference must not masquerade as a cached JPEG.
                app.pin_current(false);
                smoke.view = Some(app.renderer.as_ref().unwrap().viewport().clone());
                app.toggle_raw(false);
                smoke.step = 2;
            }
            2 if ready && !app.raw.displayed => {
                same_view(app, smoke.view.as_ref().unwrap())?;
                if !app.pinned_raw || app.pinned.is_none() {
                    return Err("RAW pin disappeared during preview switch".into());
                }
                app.clear_pin();
                smoke.view = Some(app.renderer.as_ref().unwrap().viewport().clone());
                app.zoom_locked = true;
                app.renderer.as_mut().unwrap().set_preserve_view(true);
                app.toggle_raw(true);
                smoke.step = 3;
            }
            3 if ready && app.raw.displayed && app.raw.cache.len() >= 2 => {
                let next = app.next_prefetch_index().ok_or("no RAW neighbor")?;
                let path = app.files[next].clone();
                if !app.raw.cache.iter().any(|(p, _)| p == &path) {
                    return Err("RAW neighbor missing".into());
                }
                smoke.cached_path = Some(path);
                app.navigate(1);
                if !app.raw.displayed || app.displayed != smoke.cached_path {
                    return Err("cached RAW navigation was not immediate".into());
                }
                smoke.step = 4;
            }
            4 if ready
                && app
                    .info
                    .get(app.displayed.as_ref().unwrap())
                    .is_some_and(|i| i.read) =>
            {
                same_view(app, smoke.view.as_ref().unwrap())?;
                app.renderer
                    .as_mut()
                    .unwrap()
                    .capture(&smoke.directory.join("raw-neighbor.ppm"))?;
                // Skip beyond the neighbor, then immediately cancel RAW mode.
                app.navigate(2);
                app.toggle_raw(true);
                smoke.step = 5;
            }
            5 if ready && !app.raw.displayed => {
                app.toggle_raw(false);
                smoke.step = 6;
                smoke.next = Instant::now() + Duration::from_millis(100);
            }
            6 => {
                app.toggle_raw(false);
                smoke.step = 7;
                smoke.next = Instant::now() + Duration::from_secs(2);
            }
            7 if ready && !app.raw.displayed => {
                if app.raw.active() {
                    return Err("RAW mode remained active after cancellation".into());
                }
                if app.budget.used() > app.budget.limit() {
                    return Err("managed memory exceeded budget".into());
                }
                app.renderer
                    .as_mut()
                    .unwrap()
                    .capture(&smoke.directory.join("preview-after-cancel.ppm"))?;
                std::fs::write(smoke.directory.join("report.txt"),format!("PASS: RAW/preview source switch preserves zoom and focus with zoom lock disabled.\nPASS: pinned RAW cannot suppress the JPEG replacement.\nPASS: RAW browsing prefetches one neighbor; navigation uses it immediately.\nPASS: cached RAW loads current EXIF/rating metadata.\nPASS: native cancellation cannot replace the selected preview.\nManaged bytes at end: {} / {}\n",app.budget.used(),app.budget.limit())).map_err(|e| e.to_string())?;
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    })();
    match result {
        Ok(true) => {
            app.cancel_raw_work();
            event_loop.exit();
        }
        Ok(false) => {
            app.raw_smoke = Some(smoke);
        }
        Err(error) => {
            eprintln!(
                "{} smoke: {error}",
                if smoke.mixed { "Mixed format" } else { "RAW" }
            );
            app.fatal = Some(error);
            app.cancel_raw_work();
            event_loop.exit();
        }
    }
}

fn mixed_tick(app: &mut App, smoke: &mut Smoke) -> Result<bool, String> {
    let ready = app.current().is_some() && app.current() == app.displayed;
    match smoke.step {
        0 if !app.scanning && app.folder.is_some() => {
            if app.all_files.len() != 2 || app.files.len() != 2 {
                return Err(format!("mixed fixture scan expected 2 photos, found {} visible / {} total (junk companions must be ignored)", app.files.len(), app.all_files.len()));
            }
            let names: Vec<_> = app
                .files
                .iter()
                .map(|path| path.file_name().unwrap_or_default().to_string_lossy())
                .collect();
            if names != ["01-synthetic-no-preview.dng", "02-synthetic.jpg"] {
                return Err(format!("mixed diagnostic requires the generated DNG/JPEG fixture folder; found {names:?}"));
            }
            if app.current().as_ref() != app.files.first() {
                return Err("mixed diagnostic did not start on the naturally first DNG".into());
            }
            if !ready || !app.raw.displayed {
                return Ok(false);
            }
            let dng = app
                .raw
                .cache
                .iter()
                .find(|(path, _)| Some(path) == app.current().as_ref())
                .map(|(_, texture)| texture)
                .ok_or("automatic DNG RAW texture missing")?;
            if (dng.original_width, dng.original_height, dng.orientation) != (64, 64, 6) {
                return Err(format!(
                    "unexpected DNG RAW layout {}x{} orientation {}",
                    dng.original_width, dng.original_height, dng.orientation
                ));
            }
            if !app.filmstrip_visible {
                app.toggle_filmstrip();
            }
            app.renderer
                .as_mut()
                .ok_or("renderer missing")?
                .capture(&smoke.directory.join("dng-auto-raw.ppm"))?;
            eprintln!("MIXED FORMAT SMOKE: preview-free DNG displayed as RAW, 64x64 orientation 6; scan excludes junk companions");
            smoke.cached_path = Some(app.files[1].clone());
            app.navigate(1);
            smoke.step = 1;
        }
        1 if ready && app.current() == smoke.cached_path => {
            if app.raw.displayed || app.raw.active() {
                return Err("JPEG navigation retained RAW display/mode".into());
            }
            let jpeg = app
                .textures
                .iter()
                .find(|(path, _)| Some(path) == smoke.cached_path.as_ref())
                .map(|(_, texture)| texture)
                .ok_or("JPEG main texture missing from preview cache")?;
            if (jpeg.original_width, jpeg.original_height) != (320, 240) {
                return Err(format!(
                    "JPEG did not display its original dimensions: {}x{}",
                    jpeg.original_width, jpeg.original_height
                ));
            }
            if app.raw_button_label().is_some() {
                return Err("JPEG incorrectly exposes a RAW button".into());
            }
            app.arm_thumbnails();
            smoke.step = 2;
        }
        2 if ready && app.current() == smoke.cached_path => {
            let Some((_, thumbnail)) = app
                .thumb_cache
                .iter()
                .find(|(path, _)| Some(path) == smoke.cached_path.as_ref())
            else {
                if smoke
                    .cached_path
                    .as_ref()
                    .is_some_and(|path| app.thumb_unavailable.contains(path))
                {
                    return Err("JPEG filmstrip thumbnail was marked unavailable".into());
                }
                return Ok(false);
            };
            if thumbnail.width > 192
                || thumbnail.height > 192
                || thumbnail.width == 0
                || thumbnail.height == 0
            {
                return Err(format!(
                    "JPEG thumbnail is not bounded: {}x{}",
                    thumbnail.width, thumbnail.height
                ));
            }
            if app.budget.used() > app.budget.limit() {
                return Err("mixed format managed memory exceeded budget".into());
            }
            app.renderer
                .as_mut()
                .ok_or("renderer missing")?
                .capture(&smoke.directory.join("jpeg-and-filmstrip.ppm"))?;
            std::fs::write(smoke.directory.join("report.txt"), format!(
                "PASS: scan contains only the DNG and JPEG; AppleDouble/hidden/temp companions ignored.\nPASS: preview-free DNG automatically displayed as sensor RAW, 64x64, orientation 6.\nPASS: navigation displays the original 320x240 JPEG and exits one-photo RAW mode.\nPASS: JPEG filmstrip thumbnail loaded within 192 pixels per side.\nManaged bytes at end: {} / {}\n",
                app.budget.used(), app.budget.limit(),
            )).map_err(|error| error.to_string())?;
            eprintln!("MIXED FORMAT SMOKE PASS: DNG RAW fallback, JPEG original, bounded filmstrip thumbnail, junk filtering");
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}
