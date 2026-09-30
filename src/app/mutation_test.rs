//! Opt-in desktop integration checks. Every filesystem mutation is confined to
//! a newly created synthetic fixture; an existing output directory is refused.
use super::*;
use std::{fs, path::Path};

pub(super) struct MutationTest {
    pub(super) folder: PathBuf,
    output: PathBuf,
    started: Instant,
    next: Instant,
    step: usize,
    selected: Option<PathBuf>,
    moved: Option<PathBuf>,
    anchor: Option<PathBuf>,
    cached: HashMap<PathBuf, usize>,
    old_serial: u64,
    retry: Option<PathBuf>,
    requests: u64,
    main_texture: Option<usize>,
}

impl MutationTest {
    pub(super) fn new(output: PathBuf) -> Result<Self, String> {
        fs::create_dir(&output).map_err(|e| {
            format!(
                "Mutation test requires a NEW output directory {}: {e}",
                output.display()
            )
        })?;
        let output = fs::canonicalize(output).map_err(|e| e.to_string())?;
        let folder = output.join("photos");
        fs::create_dir(&folder).map_err(|e| e.to_string())?;
        fs::create_dir(output.join("holding")).map_err(|e| e.to_string())?;
        let mut compressor = turbojpeg::Compressor::new().map_err(|e| e.to_string())?;
        for index in 0..40 {
            let pixels = vec![40 + index as u8 * 4; 160 * 120 * 3];
            let jpeg = compressor
                .compress_to_vec(turbojpeg::Image {
                    pixels: pixels.as_slice(),
                    width: 160,
                    pitch: 160 * 3,
                    height: 120,
                    format: turbojpeg::PixelFormat::RGB,
                })
                .map_err(|e| e.to_string())?;
            let mut raw = vec![0; 128];
            raw[..8].copy_from_slice(&[b'I', b'I', 42, 0, 8, 0, 0, 0]);
            raw[8..10].copy_from_slice(&3u16.to_le_bytes());
            for (entry, tag, kind, value) in [
                (0, 0x0112u16, 3u16, 1u32),
                (1, 0x0201, 4, 128),
                (2, 0x0202, 4, jpeg.len() as u32),
            ] {
                let p = 10 + entry * 12;
                raw[p..p + 2].copy_from_slice(&tag.to_le_bytes());
                raw[p + 2..p + 4].copy_from_slice(&kind.to_le_bytes());
                raw[p + 4..p + 8].copy_from_slice(&1u32.to_le_bytes());
                raw[p + 8..p + 12].copy_from_slice(&value.to_le_bytes());
            }
            raw.extend_from_slice(&jpeg);
            let path = folder.join(format!("DSC{index:05}.ARW"));
            fs::write(&path, raw).map_err(|e| e.to_string())?;
            crate::xmp::write_rating(&path, 5).map_err(|e| e.to_string())?;
        }
        let now = Instant::now();
        Ok(Self {
            folder,
            output,
            started: now,
            next: now,
            step: 0,
            selected: None,
            moved: None,
            anchor: None,
            cached: HashMap::new(),
            old_serial: 0,
            retry: None,
            requests: 0,
            main_texture: None,
        })
    }

    fn ready(app: &App) -> bool {
        !app.scanning
            && !app.refreshing
            && !app.trash_busy
            && app.pending_deleted.is_none()
            && app.pending_upload.is_none()
            && app.current() == app.displayed
    }

    fn strip_ready(app: &App) -> bool {
        !app.thumb_range.is_empty()
            && app.thumb_range.clone().all(|index| {
                app.files
                    .get(index)
                    .is_some_and(|path| app.thumb_cache.iter().any(|(cached, _)| cached == path))
            })
    }

    fn left_edge(app: &App) -> Option<PathBuf> {
        app.files.get(app.strip_window.range().start).cloned()
    }

    fn main_texture(app: &App) -> Option<usize> {
        app.textures
            .iter()
            .find(|(path, _)| Some(path) == app.displayed.as_ref())
            .map(|(_, texture)| Arc::as_ptr(texture) as usize)
    }

    fn remember_view(&mut self, app: &App) {
        self.selected = app.current();
        self.anchor = Self::left_edge(app);
        self.cached = app
            .thumb_cache
            .iter()
            .map(|(path, texture)| (path.clone(), Arc::as_ptr(texture) as usize))
            .collect();
        self.main_texture = Self::main_texture(app);
        self.requests = app.requests;
    }

    fn check_cached_survivors(&self, app: &App) -> Result<(), String> {
        for (path, pointer) in &self.cached {
            if app.files.contains(path)
                && !app.thumb_cache.iter().any(|(cached, texture)| {
                    cached == path && Arc::as_ptr(texture) as usize == *pointer
                })
            {
                return Err(format!(
                    "surviving thumbnail replaced/evicted: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    fn check_view(&self, app: &App) -> Result<(), String> {
        if app.current() != self.selected || app.displayed != self.selected {
            return Err("surviving selected photo changed".into());
        }
        if Self::left_edge(app) != self.anchor {
            return Err("independently scrolled filmstrip lost its leftmost photo".into());
        }
        if Self::main_texture(app) != self.main_texture {
            return Err("surviving main GPU texture was replaced".into());
        }
        self.check_cached_survivors(app)
    }

    /// Finder-like rename, including its sidecar, only between owned fixture
    /// directories. Refuse overwrites even in this disposable test workspace.
    fn external_move(&self, source: &Path, destination: &Path) -> Result<(), String> {
        if !source.starts_with(&self.output)
            || !destination.starts_with(&self.output)
            || destination.exists()
            || destination.with_extension("xmp").exists()
        {
            return Err("unsafe mutation fixture move refused".into());
        }
        fs::rename(source, destination).map_err(|e| e.to_string())?;
        fs::rename(
            source.with_extension("xmp"),
            destination.with_extension("xmp"),
        )
        .map_err(|e| e.to_string())
    }

    fn space(app: &mut App) {
        app.modifiers = ModifiersState::empty();
        app.key(Key::Named(NamedKey::Space), false);
    }

    fn refresh_key(app: &mut App) {
        app.modifiers = ModifiersState::SUPER;
        app.key(Key::Character("r".into()), false);
        app.modifiers = ModifiersState::empty();
    }

    /// True means all scenarios completed. Waiting does not advance the stage.
    fn advance(&mut self, app: &mut App) -> Result<bool, String> {
        if !Self::ready(app) {
            return Ok(false);
        }
        match self.step {
            0 => {
                if app.files.len() != 40 {
                    return Err("synthetic startup did not list 40 photos".into());
                }
                if !app.filmstrip_visible {
                    app.toggle_filmstrip();
                }
                app.navigate(2);
                app.scroll_strip(20);
            }
            1 => {
                if !Self::strip_ready(app) {
                    return Ok(false);
                }
                self.remember_view(app);
                self.moved = app.current();
                self.selected = app.files.get(3).cloned();
                self.old_serial = app.thumb_serial;
                Self::space(app);
                if !app.trash_busy {
                    return Err("Space did not start a move".into());
                }
            }
            2 => {
                let moved = self.moved.as_ref().ok_or("missing moved path")?;
                if app.files.len() != 39
                    || app.files.contains(moved)
                    || moved.exists()
                    || !self
                        .folder
                        .join("_Rejected")
                        .join(moved.file_name().unwrap())
                        .exists()
                    || app.current() != self.selected
                    || Self::left_edge(app) != self.anchor
                {
                    return Err(
                        "Space did not remove exactly the moved photo and retain strip position"
                            .into(),
                    );
                }
                self.check_cached_survivors(app)?;
                self.remember_view(app);
                app.command(Command::UndoRating);
            }
            3 => {
                if app.files.len() != 40 || !self.moved.as_ref().is_some_and(|p| p.exists()) {
                    return Err("Undo did not restore the moved photo".into());
                }
                self.check_view(app)?;
                let path = self.moved.clone().ok_or("missing restored path")?;
                let index = app
                    .files
                    .iter()
                    .position(|p| p == &path)
                    .ok_or("restored path absent")?;
                // Inject a completion whose current session/index/path all match.
                // Only its pre-move serial reveals that its missing-file result
                // is obsolete. Put it inside the warm range to exercise that gate.
                let stale_path = app.files[app.thumb_range.start].clone();
                let stale_index = app.thumb_range.start;
                for (index, path) in [(index, path), (stale_index, stale_path)] {
                    app.thumbnail_event(ThumbnailEvent {
                        session: app.session,
                        serial: self.old_serial,
                        index,
                        path: path.clone(),
                        orientation: 1,
                        timings: Default::default(),
                        rating: Err("synthetic obsolete rating".into()),
                        result: Err(ThumbnailFailure::Unavailable(
                            "synthetic pre-move failure".into(),
                        )),
                    });
                    if app.thumb_unavailable.contains(&path) {
                        return Err(
                            "obsolete thumbnail error survived move/undo invalidation".into()
                        );
                    }
                }
                let retry = app.files[app.thumb_range.start].clone();
                app.invalidate_thumbnail_work();
                app.thumb_cache.retain(|(path, _)| path != &retry);
                app.thumb_unavailable.insert(retry.clone());
                self.retry = Some(retry);
                self.remember_view(app);
                Self::refresh_key(app);
                if !app.refreshing {
                    return Err("Cmd+R did not start a background rescan".into());
                }
            }
            4 => {
                let retry = self.retry.as_ref().ok_or("missing retry path")?;
                if app.thumb_unavailable.contains(retry) {
                    return Err("unchanged rescan retained a thumbnail failure".into());
                }
                if !Self::strip_ready(app) {
                    return Ok(false);
                }
                self.check_view(app)?;
                if app.requests != self.requests || app.files.len() != 40 {
                    return Err("unchanged rescan reloaded the available main photo".into());
                }
                app.set_filter(PhotoFilter::Stars(5));
            }
            5 => {
                if !app.rating_index_complete || app.files.len() != 40 || !Self::strip_ready(app) {
                    return Ok(false);
                }
                self.remember_view(app);
                let source = self.folder.join("DSC00000.ARW");
                self.external_move(&source, &self.output.join("holding/DSC00000.ARW"))?;
                Self::refresh_key(app);
            }
            6 => {
                if app.files.len() != 39 || app.all_files.len() != 39 {
                    return Err("rescan failed to remove an externally moved photo".into());
                }
                self.check_view(app)?;
                self.remember_view(app);
                self.external_move(
                    &self.output.join("holding/DSC00000.ARW"),
                    &self.folder.join("DSC00000.ARW"),
                )?;
                Self::refresh_key(app);
            }
            7 => {
                if !app.rating_index_complete || app.files.len() != 40 || !Self::strip_ready(app) {
                    return Ok(false);
                }
                self.check_view(app)?;
                app.navigate(isize::MAX);
            }
            8 => {
                if !Self::strip_ready(app) {
                    return Ok(false);
                }
                self.moved = app.current();
                self.selected = app.files.get(38).cloned();
                Self::space(app);
            }
            9 => {
                if app.files.len() != 39 || app.current() != self.selected {
                    return Err(
                        "rejecting the last photo did not choose its previous survivor".into(),
                    );
                }
                app.command(Command::UndoRating);
            }
            10 => {
                if app.files.len() != 40 || app.current() != self.selected {
                    return Err("tail undo did not preserve the selected survivor".into());
                }
                for path in app
                    .all_files
                    .iter()
                    .filter(|p| Some(*p) != self.selected.as_ref())
                {
                    self.external_move(
                        path,
                        &self.output.join("holding").join(path.file_name().unwrap()),
                    )?;
                }
                Self::refresh_key(app);
            }
            11 => {
                if app.files.len() != 1 || app.current() != self.selected {
                    return Err("batch external removal did not preserve the sole survivor".into());
                }
                Self::space(app);
            }
            12 => {
                if !app.files.is_empty() || app.current().is_some() || app.displayed.is_some() {
                    return Err("rejecting the only photo left a stale selection/image".into());
                }
                app.command(Command::UndoRating);
            }
            13 => {
                if app.files.len() != 1
                    || app.current() != self.selected
                    || app.displayed != self.selected
                {
                    return Err("undo in an empty folder did not restore its only photo".into());
                }
                if !Self::strip_ready(app) {
                    return Ok(false);
                }
                // An unchanged list must not retarget a current decode that is
                // still pending. Only a known failure should be retried.
                let displayed = app.displayed.take();
                self.requests = app.requests;
                let generation = app.generation.load(Ordering::Acquire);
                app.apply_folder_refresh(scan::ScanResult {
                    session: app.folder_epoch.load(Ordering::Acquire),
                    folder: self.folder.clone(),
                    files: Arc::clone(&app.all_files),
                    requested: None,
                    complete: true,
                    elapsed: Duration::ZERO,
                });
                if app.requests != self.requests
                    || app.generation.load(Ordering::Acquire) != generation
                {
                    return Err("unchanged refresh restarted a pending main decode".into());
                }
                app.displayed = displayed;
                app.loaded(LoaderEvent::Failed {
                    session: app.session,
                    generation,
                    index: app.navigator.index().ok_or("no retry index")?,
                    error: "synthetic failed preview for refresh retry".into(),
                });
                Self::refresh_key(app);
            }
            14 => {
                if app.current() != self.selected
                    || app.requests != self.requests + 1
                    || app.failed_preview.is_some()
                {
                    return Err(
                        "Cmd+R did not retry a known failed main preview exactly once".into(),
                    );
                }
                let result = "MUTATION PASS: Space/undo, selected paths, independent strip anchors, surviving GPU thumbnails, stale completion rejection, unchanged Cmd+R retry, external remove/restore under five-star filter, tail and only-photo rejection. All filesystem writes confined to synthetic fixtures.\n";
                fs::write(self.output.join("result.txt"), result).map_err(|e| e.to_string())?;
                eprint!("{result}");
                return Ok(true);
            }
            _ => return Err("unexpected mutation test stage".into()),
        }
        eprintln!("MUTATION step={} passed", self.step);
        self.step += 1;
        self.next = Instant::now() + Duration::from_millis(150);
        Ok(false)
    }
}

pub(super) fn tick(app: &mut App, event_loop: &ActiveEventLoop) {
    let Some(mut test) = app.mutation_test.take() else {
        return;
    };
    if Instant::now() < test.next {
        app.mutation_test = Some(test);
        return;
    }
    let result = if test.started.elapsed() > Duration::from_secs(60) {
        Err(format!("timeout at step {} (files={}, scanning={}, current={:?}, displayed={:?}, thumbnails={})", test.step, app.files.len(), app.scanning, app.current(), app.displayed, app.thumb_cache.len()))
    } else {
        test.advance(app)
    };
    match result {
        Ok(true) => app.quit(),
        Ok(false) => app.mutation_test = Some(test),
        Err(error) => {
            let error = format!("Mutation test step {}: {error}", test.step);
            eprintln!("{error}");
            let _ = fs::write(test.output.join("failure.txt"), &error);
            app.fatal = Some(error);
            event_loop.exit();
        }
    }
}
