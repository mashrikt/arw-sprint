//! Event-driven desktop shell. Filesystem, JPEG, and GPU uploads run on workers.
mod deleted;
mod deletion;
mod filmstrip;
mod io;
mod menu;
mod scan;
mod session;
mod shortcuts;
mod thumbnails;
mod upload;

use crate::{
    browser::{
        filter::{choose_selection, select_visible, PhotoFilter},
        navigator::Navigator,
    },
    image::{
        cache::MemoryBudget,
        loader::{ImageLoader, LoaderConfig, LoaderEvent},
    },
    metadata::ExifMetadata,
    renderer::{FilmstripItem, PreparedTexture, Renderer},
};
use io::{IoEvent, IoWorker};
use menu::{AppMenu, Command};
use session::{SessionEvent, SessionWorker};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use thumbnails::{ThumbnailEvent, ThumbnailFailure, ThumbnailWorker};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey},
    platform::macos::EventLoopBuilderExtMacOS,
    window::{Fullscreen, Window, WindowId},
};

pub struct Options {
    pub path: Option<PathBuf>,
    pub cache_mb: usize,
    pub smoke_test: Option<PathBuf>,
}
enum Event {
    Open(PathBuf),
    Scan(Result<scan::ScanResult, (u64, String)>),
    Load(LoaderEvent),
    Uploaded(upload::Finished),
    Io(IoEvent),
    Menu(muda::MenuEvent),
    Session(SessionEvent),
    Thumbnail(ThumbnailEvent),
    ThumbUploaded(upload::ThumbFinished),
}
#[derive(Default)]
struct PhotoInfo {
    metadata: Option<ExifMetadata>,
    rating: Option<i8>,
    read: bool,
    revision: u64,
    saved_revision: u64,
    error: Option<String>,
}
struct Smoke {
    directory: PathBuf,
    step: usize,
    started: Instant,
    next: Instant,
    thumb_idle_counts: Option<(u64, u64)>,
    thumb_stable: Vec<(PathBuf, usize)>,
    thumb_stability_checks: u64,
    strip_main_index: Option<usize>,
    strip_main_path: Option<PathBuf>,
    strip_displayed_path: Option<PathBuf>,
    strip_generation: Option<u64>,
    strip_requests: u64,
    strip_viewport: Option<crate::renderer::Viewport>,
    strip_expected_range: Option<std::ops::Range<usize>>,
    zoom_transition: Option<(crate::renderer::Viewport, u16, PathBuf, u64)>,
}
struct App {
    proxy: EventLoopProxy<Event>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    menu: Option<AppMenu>,
    uploader: Option<upload::UploadWorker>,
    thumbnails: ThumbnailWorker,
    thumbnail_budget: Arc<MemoryBudget>,
    filmstrip_visible: bool,
    strip_window: crate::browser::filmstrip::FilmstripWindow,
    thumb_cache: VecDeque<(PathBuf, Arc<PreparedTexture>)>,
    thumb_unavailable: HashSet<PathBuf>,
    thumb_serial: u64,
    thumb_range: std::ops::Range<usize>,
    thumb_plan_sent: bool,
    thumb_deadline: Option<Instant>,
    thumb_decodes: u64,
    thumb_uploads: u64,
    thumb_retries: u8,
    strip_scroll: f64,
    loader: Arc<ImageLoader>,
    io: IoWorker,
    session_store: Option<SessionWorker>,
    restore_allowed: bool,
    zoom_preference_changed: bool,
    auto_preference_changed: bool,
    brightness_preference_changed: bool,
    zoom_locked: bool,
    auto_advance: bool,
    brightness_steps: i8,
    pinned: Option<(PathBuf, Arc<PreparedTexture>)>,
    rating_history: VecDeque<(u64, deletion::UndoAction)>,
    rating_sequence: u64,
    revision_sequence: u64,
    interaction_serial: u64,
    pending_undos: HashMap<u64, u64>,
    budget: Arc<MemoryBudget>,
    folder_epoch: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
    session: u64,
    all_files: Arc<Vec<PathBuf>>,
    files: Arc<Vec<PathBuf>>,
    filter: PhotoFilter,
    rating_index_started: bool,
    rating_index_complete: bool,
    ratings_read: usize,
    rating_errors: usize,
    rating_view_dirty: bool,
    folder: Option<PathBuf>,
    scanning: bool,
    scan_complete_only: bool,
    user_navigated: bool,
    navigator: Navigator,
    info: HashMap<PathBuf, PhotoInfo>,
    textures: VecDeque<(PathBuf, Arc<PreparedTexture>)>,
    gpu_prefetched: HashSet<PathBuf>,
    displayed: Option<PathBuf>,
    pending_upload: Option<u64>,
    pending_gpu_prefetch: Option<(u64, PathBuf)>,
    attempted_gpu_prefetch: Option<PathBuf>,
    requested_at: Instant,
    first_render: bool,
    modifiers: ModifiersState,
    cursor: [f64; 2],
    dragging: bool,
    initial_path: Option<PathBuf>,
    quitting: bool,
    trash_busy: bool,
    deleted_batch: bool,
    pending_deleted: Option<deletion::PendingMove>,
    notice: String,
    log: bool,
    requests: u64,
    gpu_hits: u64,
    cpu_hits: u64,
    prefetch_hits: u64,
    smoke: Option<Smoke>,
    fatal: Option<String>,
}

pub fn run(options: Options) -> Result<(), String> {
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_default_menu(false)
        .build()
        .map_err(|e| e.to_string())?;
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    muda::MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(Event::Menu(event));
    }));
    let open_proxy = proxy.clone();
    crate::platform::install_open_handler(move |path| {
        let _ = open_proxy.send_event(Event::Open(path));
    });
    // Carve a fixed, small pool out of the configured total so a main-image
    // upload cannot evict the visible strip or starve thumbnail browsing.
    let total_bytes = options
        .cache_mb
        .checked_mul(1024 * 1024)
        .ok_or("image budget overflow")?;
    let (main_bytes, thumb_bytes) = filmstrip::budget_limits(total_bytes)?;
    let budget = MemoryBudget::new(main_bytes);
    let thumbnail_budget = MemoryBudget::new(thumb_bytes);
    let load_proxy = proxy.clone();
    let mut config = LoaderConfig::new(Arc::clone(&budget));
    // Four full-resolution allocations coexist during an upload. The 256MiB
    // option uses libjpeg-turbo's half-size decode to leave this headroom.
    if options.cache_mb <= 256 {
        config.decode_scale = 2;
    }
    config.cpu_cache_bytes = budget.limit() / 2;
    let loader = Arc::new(ImageLoader::new(
        config,
        Arc::new(move |event| {
            let _ = load_proxy.send_event(Event::Load(event));
        }),
    )?);
    let io_proxy = proxy.clone();
    let io = IoWorker::new(Arc::new(move |event| {
        let _ = io_proxy.send_event(Event::Io(event));
    }))?;
    let restore_allowed = options.path.is_none() && options.smoke_test.is_none();
    let session_store = if options.smoke_test.is_none() {
        let session_proxy = proxy.clone();
        match SessionWorker::new(Arc::new(move |event| {
            let _ = session_proxy.send_event(Event::Session(event));
        })) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("Session persistence unavailable: {error}");
                None
            }
        }
    } else {
        None
    };
    let thumb_proxy = proxy.clone();
    let thumbnails = ThumbnailWorker::new(
        Arc::clone(&thumbnail_budget),
        Arc::new(move |event| {
            let _ = thumb_proxy.send_event(Event::Thumbnail(event));
        }),
    )?;
    let mut app = App {
        proxy,
        window: None,
        renderer: None,
        menu: None,
        uploader: None,
        thumbnails,
        thumbnail_budget,
        filmstrip_visible: false,
        strip_window: Default::default(),
        thumb_cache: VecDeque::new(),
        thumb_unavailable: HashSet::new(),
        thumb_serial: 0,
        thumb_range: 0..0,
        thumb_plan_sent: false,
        thumb_deadline: None,
        thumb_decodes: 0,
        thumb_uploads: 0,
        thumb_retries: 0,
        strip_scroll: 0.0,
        loader,
        io,
        session_store,
        restore_allowed,
        zoom_preference_changed: false,
        auto_preference_changed: false,
        brightness_preference_changed: false,
        zoom_locked: true,
        auto_advance: false,
        brightness_steps: 0,
        pinned: None,
        rating_history: VecDeque::new(),
        rating_sequence: 0,
        revision_sequence: 0,
        interaction_serial: 0,
        pending_undos: HashMap::new(),
        budget,
        folder_epoch: Arc::new(AtomicU64::new(0)),
        generation: Arc::new(AtomicU64::new(0)),
        session: 0,
        all_files: Arc::new(Vec::new()),
        files: Arc::new(Vec::new()),
        filter: PhotoFilter::All,
        rating_index_started: false,
        rating_index_complete: false,
        ratings_read: 0,
        rating_errors: 0,
        rating_view_dirty: false,
        folder: None,
        scanning: false,
        scan_complete_only: false,
        user_navigated: false,
        navigator: Navigator::new(0),
        info: HashMap::new(),
        textures: VecDeque::new(),
        gpu_prefetched: HashSet::new(),
        displayed: None,
        pending_upload: None,
        pending_gpu_prefetch: None,
        attempted_gpu_prefetch: None,
        requested_at: Instant::now(),
        first_render: false,
        modifiers: ModifiersState::empty(),
        cursor: [0.0; 2],
        dragging: false,
        initial_path: options.path,
        quitting: false,
        trash_busy: false,
        deleted_batch: false,
        pending_deleted: None,
        notice: String::new(),
        log: std::env::var_os("FASTCULL_LOG").is_some(),
        requests: 0,
        gpu_hits: 0,
        cpu_hits: 0,
        prefetch_hits: 0,
        smoke: options.smoke_test.map(|directory| Smoke {
            directory,
            step: 0,
            started: Instant::now(),
            next: Instant::now(),
            thumb_idle_counts: None,
            thumb_stable: Vec::new(),
            thumb_stability_checks: 0,
            strip_main_index: None,
            strip_main_path: None,
            strip_displayed_path: None,
            strip_generation: None,
            strip_requests: 0,
            strip_viewport: None,
            strip_expected_range: None,
            zoom_transition: None,
        }),
        fatal: None,
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.generation.fetch_add(1, Ordering::AcqRel);
    app.uploader.take();
    if app.log {
        eprintln!("navigation requests={} gpu_hits={} cpu_hits={} prefetch_hits={} managed_memory={:.1}MiB/{:.0}MiB", app.requests, app.gpu_hits, app.cpu_hits, app.prefetch_hits, app.managed_used() as f64 / 1048576.0, app.managed_limit() as f64 / 1048576.0);
    }
    app.fatal.map_or(Ok(()), Err)
}

/// Choose before changing filter membership so auto-advance cannot skip twice
/// when a rating removes the current photo from the visible list.
fn rating_destination(files: &[PathBuf], index: Option<usize>, advance: bool) -> Option<PathBuf> {
    let index = index?;
    let target = if advance {
        index.saturating_add(1).min(files.len().saturating_sub(1))
    } else {
        index
    };
    files.get(target).cloned()
}

impl App {
    fn current(&self) -> Option<PathBuf> {
        self.navigator
            .index()
            .and_then(|index| self.files.get(index))
            .cloned()
    }
    fn redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
    fn message(&mut self, text: impl Into<String>) {
        self.notice = text.into();
        self.status();
    }
    fn status(&mut self) {
        self.refresh_filmstrip();
        let path = self.current();
        if let Some(menu) = &self.menu {
            menu.set_workflow(
                self.zoom_locked,
                self.auto_advance,
                self.pinned.is_some(),
                path.is_some() && path == self.displayed,
                !self.rating_history.is_empty(),
            );
        }
        let mut text = if let Some(path) = &path {
            let info = self.info.get(path);
            let rating = match info.and_then(|i| i.rating) {
                Some(-1) => "REJECT".into(),
                Some(0) => "-".into(),
                Some(stars) => "*".repeat(stars.clamp(0, 5) as usize),
                None => "...".into(),
            };
            let mut text = format!(
                "{} / {}    {}    {}",
                self.navigator.index().unwrap_or(0) + 1,
                self.files.len(),
                rating,
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            if let Some((_, texture)) = self.textures.iter().find(|(p, _)| p == path) {
                text.push_str(&format!(
                    "    {}x{}",
                    texture.original_width, texture.original_height
                ));
            }
            if let Some(info) = info {
                if let Some(exif) = &info.metadata {
                    if let Some(t) = exif.exposure_time {
                        text.push_str(&if t > 0.0 && t < 1.0 {
                            format!("    1/{:.0}", 1.0 / t)
                        } else {
                            format!("    {t:.1}s")
                        });
                    }
                    if let Some(f) = exif.aperture {
                        text.push_str(&format!("  f/{f:.1}"));
                    }
                    if let Some(iso) = exif.iso {
                        text.push_str(&format!("  ISO {iso}"));
                    }
                    if let Some(mm) = exif.focal_length {
                        text.push_str(&format!("  {mm:.0}mm"));
                    }
                }
                if info.error.is_some() {
                    text.push_str("    XMP ERROR (see log)");
                } else if info.revision != info.saved_revision {
                    text.push_str("    Saving...");
                }
            }
            text
        } else {
            "ARW Sprint    Cmd+O: open folder".into()
        };
        if self.scanning {
            text.push_str("    Scanning...");
        }
        if self.brightness_steps != 0 {
            // Keep the viewing adjustment visible even if long EXIF text is clipped.
            text = format!(
                "VIEW {:+.2} EV    {text}",
                f32::from(self.brightness_steps) / 3.0
            );
        }
        if self.zoom_locked {
            text.push_str("    ZOOM LOCK");
        }
        if self.auto_advance {
            text.push_str("    AUTO NEXT");
        }
        if let Some((reference, _)) = &self.pinned {
            text.push_str(&format!(
                "    LEFT: {} | RIGHT: current",
                reference.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
        if self.filter != PhotoFilter::All {
            text = format!(
                "{} ({}/{})    {text}",
                self.filter.label(),
                self.files.len(),
                self.all_files.len()
            );
            if !self.rating_index_complete && !self.all_files.is_empty() {
                text.push_str(&format!(
                    "    Reading ratings {}/{}...",
                    self.ratings_read,
                    self.all_files.len()
                ));
            }
            if self.rating_errors > 0 {
                text.push_str(&format!(
                    "    {} unreadable ratings (see log)",
                    self.rating_errors
                ));
            }
        }
        let unsaved = self
            .info
            .values()
            .filter(|info| info.revision != info.saved_revision)
            .count();
        if unsaved > 0 {
            text.push_str(&format!("    {unsaved} unsaved rating(s)"));
        }
        if self.displayed != path && path.is_some() {
            text.push_str("    Loading...");
        }
        if !self.notice.is_empty() {
            text.push_str("    ");
            text.push_str(&self.notice);
        }
        if let Some(window) = &self.window {
            window.set_title(&format!(
                "ARW Sprint: {}",
                path.as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Sony ARW viewer".into())
            ));
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.set_status(&text);
        }
        self.redraw();
    }
    fn open(&mut self, path: PathBuf) {
        if self.quitting || self.trash_busy {
            return;
        }
        self.restore_allowed = false;
        self.interaction_serial += 1;
        self.stop_thumbnail_work();
        self.thumb_cache.clear();
        self.thumb_unavailable.clear();
        self.strip_window.reset();
        self.clear_pin();
        self.end_peek();
        let epoch = self.folder_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.session += 1;
        self.io.cancel_rating_scan();
        self.all_files = Arc::new(Vec::new());
        self.files = Arc::new(Vec::new());
        self.filter = PhotoFilter::All;
        self.rating_index_started = false;
        self.rating_index_complete = false;
        self.ratings_read = 0;
        self.rating_errors = 0;
        self.rating_view_dirty = false;
        if let Some(menu) = &self.menu {
            menu.set_filter(self.filter);
        }
        self.folder = None;
        self.info
            .retain(|_, info| info.revision != info.saved_revision);
        self.loader.set_files(self.session, Arc::clone(&self.files));
        if let Some(uploader) = &self.uploader {
            uploader.cancel();
        }
        self.textures.clear();
        self.gpu_prefetched.clear();
        self.displayed = None;
        self.pending_upload = None;
        self.pending_gpu_prefetch = None;
        self.attempted_gpu_prefetch = None;
        self.navigator = Navigator::new(0);
        self.user_navigated = false;
        self.scanning = true;
        self.scan_complete_only = false;
        self.notice.clear();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_image(None);
            renderer.set_message("Opening folder...");
        }
        let proxy = self.proxy.clone();
        scan::start(
            path,
            epoch,
            Arc::clone(&self.folder_epoch),
            Arc::new(move |result| {
                let _ = proxy.send_event(Event::Scan(result));
            }),
        );
        self.status();
    }
    fn scanned(&mut self, result: Result<scan::ScanResult, (u64, String)>) {
        let result = match result {
            Ok(result) if result.session == self.folder_epoch.load(Ordering::Acquire) => result,
            Err((epoch, error)) if epoch == self.folder_epoch.load(Ordering::Acquire) => {
                self.scanning = false;
                eprintln!("open folder: {error}");
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_message("Folder unavailable - Cmd+O to choose a folder");
                }
                self.message(error);
                return;
            }
            _ => return,
        };
        if self.scan_complete_only && !result.complete {
            return;
        }
        if result.complete {
            self.scan_complete_only = false;
        }
        let previous = self.current();
        let selected = scan::selected_index(
            &result.files,
            result.requested.as_ref(),
            previous.as_ref(),
            self.user_navigated,
        );
        self.folder = Some(result.folder);
        self.scanning = !result.complete;
        let preferred = result.files.get(selected).cloned();
        self.all_files = result.files;
        if self.log {
            eprintln!(
                "directory files={} complete={} scan_ms={:.2}",
                self.all_files.len(),
                result.complete,
                result.elapsed.as_secs_f64() * 1000.0
            );
        }
        self.start_rating_index();
        self.refresh_visible(preferred.as_deref());
    }
    fn start_rating_index(&mut self) {
        if self.filter != PhotoFilter::All && !self.scanning && !self.rating_index_started {
            self.rating_index_started = true;
            self.io.scan_ratings(
                self.folder_epoch.load(Ordering::Acquire),
                Arc::clone(&self.all_files),
            );
        }
    }
    fn set_filter(&mut self, filter: PhotoFilter) {
        if !self.trash_busy && !self.quitting {
            self.interaction_serial += 1;
            self.filter = filter;
            self.start_rating_index();
            let previous = self.current();
            self.refresh_visible(previous.as_deref());
        }
        if let Some(menu) = &self.menu {
            menu.set_filter(self.filter);
        }
    }
    /// Every changed list gets a new loader session: an old completion's index
    /// must never resolve to a different photograph after filtering.
    fn refresh_visible(&mut self, preferred: Option<&std::path::Path>) {
        if self.trash_busy || self.quitting {
            return;
        }
        self.rating_view_dirty = false;
        // Normal unfiltered navigation must not clone a 10,000-path directory
        // on every metadata completion or rating edit.
        if self.filter == PhotoFilter::All
            && Arc::ptr_eq(&self.files, &self.all_files)
            && preferred == self.current().as_deref()
        {
            self.status();
            return;
        }
        let visible = if self.filter == PhotoFilter::All {
            Arc::clone(&self.all_files)
        } else {
            Arc::new(select_visible(&self.all_files, self.filter, |path| {
                self.info.get(path).and_then(|info| info.rating)
            }))
        };
        let selected = choose_selection(&visible, preferred, self.navigator.index());
        if *visible != *self.files {
            self.stop_thumbnail_work();
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.session += 1;
            if let Some(uploader) = &self.uploader {
                uploader.cancel();
            }
            self.pending_upload = None;
            self.pending_gpu_prefetch = None;
            self.attempted_gpu_prefetch = None;
            self.first_render = false;
            self.files = visible;
            self.navigator = Navigator::new(self.files.len());
            self.navigator.seek(selected.unwrap_or(0));
            self.loader.set_files(self.session, Arc::clone(&self.files));
            let paths: HashSet<_> = self.files.iter().collect();
            self.textures.retain(|(path, _)| paths.contains(path));
            self.thumb_cache.retain(|(path, _)| paths.contains(path));
            self.gpu_prefetched.retain(|path| paths.contains(path));
            if self
                .displayed
                .as_ref()
                .is_some_and(|path| !paths.contains(path))
            {
                self.displayed = None;
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_image(None);
                }
            }
            self.sync_gpu_cache();
            self.request_current();
        } else if selected.is_some_and(|index| self.navigator.seek(index)) {
            self.request_current();
        }
        if self.files.is_empty() {
            let message = if self.scanning {
                "Scanning folder..."
            } else if self.all_files.is_empty() {
                "No ARWs in this folder. Cmd+O to open a folder."
            } else if !self.rating_index_complete {
                "Reading ratings..."
            } else {
                "No photos match. Cmd+Option+0 shows all photos."
            };
            if let Some(renderer) = &mut self.renderer {
                renderer.set_image(None);
                renderer.set_message(message);
            }
        }
        self.status();
    }
    fn ratings_changed(&mut self) {
        let previous = self.current();
        // Do not cancel an unchanged foreground decode for every 32-file index
        // batch. Apply accumulated matches when it displays, or at index end.
        if !self.rating_index_complete
            && previous.is_some()
            && previous != self.displayed
            && previous.as_ref().is_some_and(|path| {
                self.filter
                    .matches(self.info.get(path).and_then(|info| info.rating))
            })
        {
            self.rating_view_dirty = true;
            self.status();
        } else {
            self.refresh_visible(previous.as_deref());
        }
    }
    fn request_current(&mut self) {
        self.stop_thumbnail_work();
        self.thumb_retries = 0;
        self.reveal_current_thumbnail();
        let Some(index) = self.navigator.index() else {
            return;
        };
        let path = self.files[index].clone();
        self.end_peek();
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.requests += 1;
        self.requested_at = Instant::now();
        self.first_render = false;
        self.pending_upload = None;
        self.pending_gpu_prefetch = None;
        self.attempted_gpu_prefetch = None;
        self.notice.clear();
        if let Some(uploader) = &self.uploader {
            if uploader.retarget(self.session, &path, generation) {
                self.pending_upload = Some(generation);
            }
        }
        if !self.info.get(&path).is_some_and(|info| info.read) {
            self.io.read(self.session, path.clone());
        }
        if !self.textures.iter().any(|(p, _)| p == &path) {
            if let Some((reference, texture)) = &self.pinned {
                if reference == &path {
                    self.textures.push_back((path.clone(), Arc::clone(texture)));
                }
            }
        }
        if let Some(position) = self.textures.iter().position(|(p, _)| *p == path) {
            let (_, texture) = self
                .textures
                .remove(position)
                .expect("cache position checked");
            self.gpu_hits += 1;
            if self.gpu_prefetched.remove(&path) {
                self.prefetch_hits += 1;
            }
            self.loader
                .request_cached(index, generation, self.navigator.direction());
            self.show(path, texture);
            self.loader.resume_prefetch();
            self.prefetch_gpu();
        } else {
            self.textures.retain(|(p, _)| {
                Some(p) == self.displayed.as_ref()
                    || self.pinned.as_ref().is_some_and(|(pin, _)| pin == p)
            });
            self.sync_gpu_cache();
            self.loader
                .request(index, generation, self.navigator.direction());
            if let Some(renderer) = &mut self.renderer {
                renderer.set_message("Loading...");
            }
        }
        self.status();
        self.arm_thumbnails();
    }
    fn navigate(&mut self, delta: isize) {
        self.interaction_serial += 1;
        if !self.quitting && !self.trash_busy && self.navigator.advance(delta) {
            self.user_navigated = true;
            self.request_current();
        }
    }
    fn show(&mut self, path: PathBuf, texture: Arc<PreparedTexture>) {
        let orientation = self
            .info
            .get(&path)
            .and_then(|i| i.metadata.as_ref())
            .map_or(texture.orientation, |m| m.orientation);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_image_with_orientation(Some(Arc::clone(&texture)), orientation);
            renderer.set_message("");
        }
        self.displayed = Some(path.clone());
        self.gpu_prefetched.remove(&path);
        self.textures.retain(|(p, _)| *p != path);
        self.textures.push_back((path, texture));
        while self.textures.len() > 3 {
            self.textures.pop_front();
        }
        self.first_render = true;
        self.sync_gpu_cache();
        self.status();
        if self.rating_view_dirty {
            let previous = self.current();
            self.refresh_visible(previous.as_deref());
        }
    }
    fn sync_gpu_cache(&self) {
        let mut indices: Vec<_> = self
            .textures
            .iter()
            .filter_map(|(path, _)| self.files.iter().position(|p| p == path))
            .collect();
        if let Some((path, _)) = &self.pinned {
            if let Some(index) = self.files.iter().position(|p| p == path) {
                if !indices.contains(&index) {
                    indices.push(index);
                }
            }
        }
        self.loader.set_gpu_cached(&indices);
    }
    fn next_prefetch_index(&self) -> Option<usize> {
        let index = self.navigator.index()?;
        match self.navigator.direction() {
            crate::browser::prefetch::Direction::Forward => {
                index.checked_add(1).filter(|next| *next < self.files.len())
            }
            crate::browser::prefetch::Direction::Backward => index.checked_sub(1),
        }
    }
    fn prefetch_gpu(&mut self) {
        if self.quitting
            || self.pending_upload.is_some()
            || self.pending_gpu_prefetch.is_some()
            || self.current() != self.displayed
        {
            return;
        }
        let Some(index) = self.next_prefetch_index() else {
            return;
        };
        let path = self.files[index].clone();
        if self.attempted_gpu_prefetch.as_ref() == Some(&path) {
            return;
        }
        if self.textures.iter().any(|(p, _)| *p == path) {
            return;
        }
        let Some(image) = self.loader.get(index) else {
            return;
        };
        let Ok(needed) = crate::renderer::GpuUploader::bytes_needed(image.width, image.height)
        else {
            return;
        };
        self.loader.evict_bytes(0); // Hold speculative CPU work through the upload.
        if self.budget.available() < needed {
            self.textures
                .retain(|(p, _)| Some(p) == self.displayed.as_ref());
            self.sync_gpu_cache();
        }
        // A reference must never cause speculative uploads to wait/retry for
        // seconds while holding the CPU prefetch worker. Keep the decoded next
        // photo instead; foreground navigation can release the outgoing image.
        if self.pinned.is_some() && self.budget.available() < needed {
            self.loader.resume_prefetch();
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let orientation = self
            .info
            .get(&path)
            .and_then(|i| i.metadata.as_ref())
            .map_or(1, |m| m.orientation);
        self.pending_gpu_prefetch = Some((generation, path.clone()));
        self.attempted_gpu_prefetch = Some(path.clone());
        if let Some(uploader) = &self.uploader {
            uploader.request(upload::Job {
                session: self.session,
                generation,
                path,
                image,
                orientation,
            });
        }
    }
    fn loaded(&mut self, event: LoaderEvent) {
        match event {
            LoaderEvent::MemoryPressure {
                session,
                index,
                generation,
                bytes_needed,
            } if session == self.session
                && Some(index) == self.navigator.index()
                && generation == self.generation.load(Ordering::Acquire) =>
            {
                self.textures
                    .retain(|(path, _)| Some(path) == self.displayed.as_ref());
                if self.budget.available() < bytes_needed {
                    self.textures.clear();
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_image(None);
                    }
                    self.displayed = None;
                }
                if self.budget.available() < bytes_needed && self.pinned.is_some() {
                    self.clear_pin();
                    self.notice = "Comparison closed to free memory for the requested photo".into();
                }
                self.sync_gpu_cache();
                self.redraw();
            }
            LoaderEvent::Ready {
                session,
                index,
                generation,
                timings,
                prefetched,
            } if session == self.session
                && Some(index) == self.navigator.index()
                && generation == self.generation.load(Ordering::Acquire)
                && self.pending_upload != Some(generation)
                && self.current() != self.displayed =>
            {
                let Some(image) = self.loader.get(index) else {
                    return;
                };
                if timings.cache_hit {
                    self.cpu_hits += 1;
                }
                if prefetched {
                    self.prefetch_hits += 1;
                }
                let path = self.files[index].clone();
                if self.log {
                    eprintln!("{} parse={:.2}ms jpeg_read={:.2}ms decode={:.2}ms pipeline={:.2}ms cpu_hit={} prefetch_hit={}", path.display(), timings.parse.as_secs_f64()*1000.0, timings.jpeg_read.as_secs_f64()*1000.0, timings.decode.as_secs_f64()*1000.0, timings.total.as_secs_f64()*1000.0, timings.cache_hit, prefetched);
                }
                let orientation = self
                    .info
                    .get(&path)
                    .and_then(|i| i.metadata.as_ref())
                    .map_or(1, |m| m.orientation);
                self.pending_upload = Some(generation);
                self.prepare_foreground_upload(image.width, image.height);
                if let Some(uploader) = &self.uploader {
                    uploader.request(upload::Job {
                        session,
                        generation,
                        path,
                        image,
                        orientation,
                    });
                }
            }
            LoaderEvent::Ready {
                session,
                generation,
                ..
            } if session == self.session
                && generation == self.generation.load(Ordering::Acquire) =>
            {
                self.prefetch_gpu()
            }
            LoaderEvent::Failed {
                session,
                index,
                generation,
                error,
            } if session == self.session
                && Some(index) == self.navigator.index()
                && generation == self.generation.load(Ordering::Acquire) =>
            {
                eprintln!("{}: {error}", self.files[index].display());
                self.displayed = None;
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_image(None);
                    renderer.set_message("Preview unavailable - use arrows to continue");
                }
                self.message("Preview unavailable");
            }
            _ => {}
        }
    }
    fn uploaded(&mut self, event: upload::Finished) {
        if let Some(uploader) = &self.uploader {
            uploader.acknowledge(event.session, &event.path, event.generation);
        }
        if event.session != self.session {
            return;
        }
        let is_current = self.current().as_ref() == Some(&event.path);
        if is_current
            && (self.current() == self.displayed
                || (event.result.is_err()
                    && event.generation != self.generation.load(Ordering::Acquire)))
        {
            return;
        }
        if !is_current {
            if self.pending_gpu_prefetch.as_ref() != Some(&(event.generation, event.path.clone())) {
                return;
            }
            self.pending_gpu_prefetch = None;
            if event.generation != self.generation.load(Ordering::Acquire) {
                return;
            }
            match event.result {
                Ok(texture) => {
                    if self.log {
                        eprintln!(
                            "gpu_prefetched={} upload={:.2}ms",
                            event.path.display(),
                            texture.timings.completion_ms
                        );
                    }
                    self.textures.retain(|(p, _)| *p != event.path);
                    self.gpu_prefetched.insert(event.path.clone());
                    self.textures.push_back((event.path, texture));
                    while self.textures.len() > 3 {
                        self.textures.pop_front();
                    }
                    self.sync_gpu_cache();
                }
                Err(error) => {
                    if self.log {
                        eprintln!("GPU prefetch skipped: {error}");
                    }
                }
            }
            self.loader.resume_prefetch();
            return;
        }
        self.pending_upload = None;
        self.pending_gpu_prefetch = None;
        match event.result {
            Ok(texture) => {
                if self.log {
                    eprintln!("gpu_submit={:.2}ms gpu_complete={:.2}ms requested_to_ready={:.2}ms managed={:.1}MiB", texture.timings.submit_ms, texture.timings.completion_ms, self.requested_at.elapsed().as_secs_f64()*1000.0, self.managed_used() as f64 /1048576.0);
                }
                self.show(event.path, texture);
                // The texture owns this photo now. Reclaim duplicate CPU copies
                // before scheduling the nearest neighbor.
                if let Some(index) = self.navigator.index() {
                    self.loader.discard(index);
                }
                self.loader.resume_prefetch();
                self.prefetch_gpu();
            }
            Err(error) => {
                eprintln!("GPU upload: {error}");
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_image(None);
                    renderer.set_message("Preview unavailable - use arrows to continue");
                }
                self.displayed = None;
                self.message(error);
            }
        }
    }
    fn rating(&mut self, rating: i8) {
        if self.quitting || self.trash_busy || self.smoke.is_some() {
            return;
        }
        let Some(path) = self
            .current()
            .filter(|p| self.displayed.as_ref() == Some(p))
        else {
            return;
        };
        let preferred = rating_destination(&self.files, self.navigator.index(), self.auto_advance);
        self.interaction_serial += 1;
        self.rating_sequence += 1;
        self.revision_sequence += 1;
        let edit_id = self.rating_sequence;
        let info = self.info.entry(path.clone()).or_default();
        info.rating = Some(rating);
        info.revision = self.revision_sequence;
        info.error = None;
        self.io
            .save(self.session, path.clone(), info.revision, rating, edit_id);
        self.rating_history
            .push_back((edit_id, deletion::UndoAction::Rating(path)));
        while self.rating_history.len() > 100 {
            self.rating_history.pop_front();
        }
        self.user_navigated = true;
        self.refresh_visible(preferred.as_deref());
    }
    fn undo_rating(&mut self) {
        if self.quitting || self.trash_busy || self.smoke.is_some() {
            return;
        }
        let Some((edit_id, action)) = self.rating_history.pop_back() else {
            return;
        };
        let path = match action {
            deletion::UndoAction::Rating(path) => path,
            deletion::UndoAction::Moved(records) => {
                self.begin_file_mutation(edit_id);
                self.io.restore_deleted(self.session, edit_id, records);
                self.message("Restoring photos from _Rejected...");
                return;
            }
        };
        self.interaction_serial += 1;
        self.pending_undos.insert(edit_id, self.interaction_serial);
        self.revision_sequence += 1;
        let info = self.info.entry(path.clone()).or_default();
        info.revision = self.revision_sequence;
        info.error = None;
        self.io.undo(self.session, path, info.revision, edit_id);
        self.message("Undoing last rating...");
    }
    fn end_peek(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            if renderer.is_peeking() {
                renderer.end_peek();
                self.redraw();
            }
        }
    }
    fn clear_pin(&mut self) {
        if let Some((path, _)) = self.pinned.take() {
            self.textures
                .retain(|(p, _)| p != &path || self.displayed.as_ref() == Some(p));
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.set_reference(None, None);
        }
    }
    fn pin_current(&mut self, replace: bool) {
        self.end_peek();
        if self.pinned.is_some() && !replace {
            self.clear_pin();
            self.sync_gpu_cache();
            self.loader.resume_prefetch();
            self.message("Comparison closed");
            return;
        }
        let Some(path) = self
            .current()
            .filter(|p| Some(p) == self.displayed.as_ref())
        else {
            return;
        };
        let Some(texture) = self
            .textures
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, t)| Arc::clone(t))
        else {
            return;
        };
        let orientation = self
            .info
            .get(&path)
            .and_then(|i| i.metadata.as_ref())
            .map_or(texture.orientation, |m| m.orientation);
        if let Some(renderer) = &mut self.renderer {
            renderer.set_reference(Some(Arc::clone(&texture)), Some(orientation));
        }
        self.pinned = Some((path, texture));
        self.sync_gpu_cache();
        self.message(
            "Reference on left; arrows and ratings affect the right photo. C closes comparison.",
        );
    }
    fn prepare_foreground_upload(&mut self, width: u32, height: u32) {
        let Ok(needed) = crate::renderer::GpuUploader::bytes_needed(width, height) else {
            return;
        };
        if self.pinned.is_none() {
            return;
        }
        self.loader.evict_for(needed);
        if self.budget.available() < needed {
            // Keep the reference while releasing the outgoing main texture.
            // Submitted frames may retain it briefly; the upload worker reaps
            // those leases without waiting on the event thread.
            self.textures.clear();
            if let Some(renderer) = &mut self.renderer {
                renderer.set_image(None);
            }
            self.displayed = None;
            self.sync_gpu_cache();
            self.redraw();
        }
        if let Some((_, reference)) = &self.pinned {
            let minimum = (width as usize)
                .checked_mul(height as usize)
                .and_then(|n| n.checked_mul(4))
                .and_then(|n| n.checked_add(needed))
                .and_then(|n| n.checked_add(reference.byte_size));
            if minimum.is_none_or(|n| n > self.budget.limit()) {
                self.clear_pin();
                self.sync_gpu_cache();
                self.message("Comparison closed to free memory for the requested photo");
            }
        }
    }
    fn persist_preferences(&self) {
        if let Some(session) = &self.session_store {
            session.preferences(
                self.zoom_preference_changed.then_some(self.zoom_locked),
                self.auto_preference_changed.then_some(self.auto_advance),
                self.brightness_preference_changed
                    .then_some(self.brightness_steps),
            );
        }
    }
    fn session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Loaded { state, resume_path } => match state {
                Ok(state) => {
                    if !self.zoom_preference_changed {
                        self.zoom_locked = state.zoom_locked;
                        if let Some(renderer) = &mut self.renderer {
                            renderer.set_preserve_view(self.zoom_locked);
                        }
                    }
                    if !self.auto_preference_changed {
                        self.auto_advance = state.auto_advance;
                    }
                    if !self.brightness_preference_changed {
                        self.brightness_steps = state.brightness_steps;
                        if let Some(renderer) = &mut self.renderer {
                            renderer.set_brightness_stops(f32::from(self.brightness_steps) / 3.0);
                        }
                    }
                    if self.restore_allowed {
                        self.restore_allowed = false;
                        if let Some(path) = resume_path {
                            self.open(path);
                        } else if state.photo.is_some() || state.folder.is_some() {
                            self.message("Previous folder unavailable. Cmd+O opens a folder.");
                        }
                    }
                    self.status();
                }
                Err(error) => {
                    eprintln!("Session restore: {error}");
                }
            },
            SessionEvent::Saved(Err(error)) => {
                eprintln!("Session save: {error}");
                self.message("Could not save last-viewed position (see log)");
            }
            _ => {}
        }
    }
    fn quit(&mut self) {
        if self.quitting || self.trash_busy {
            return;
        }
        self.quitting = true;
        self.stop_thumbnail_work();
        self.restore_allowed = false;
        self.end_peek();
        if let Some(session) = &self.session_store {
            session.flush();
        }
        self.io.flush();
        self.message("Saving ratings...");
    }
    fn alert(&self, title: &str, message: &str) {
        let mut dialog = rfd::MessageDialog::new()
            .set_title(title)
            .set_description(message);
        if let Some(window) = &self.window {
            dialog = dialog.set_parent(window.as_ref());
        }
        dialog.show();
    }
    fn io_event(&mut self, event: IoEvent, event_loop: &ActiveEventLoop) {
        match event {
            IoEvent::Ratings {
                folder_epoch,
                entries,
                complete,
            } if folder_epoch == self.folder_epoch.load(Ordering::Acquire)
                && self.rating_index_started =>
            {
                self.ratings_read += entries.len();
                for (path, result) in entries {
                    let info = self.info.entry(path.clone()).or_default();
                    // Optimistic edits win even if a chunk was read before the
                    // edit or its save has not reached disk yet.
                    if info.revision != 0 {
                        continue;
                    }
                    match result {
                        Ok(rating) => {
                            info.rating = Some(rating);
                            info.error = None;
                        }
                        Err(error) => {
                            eprintln!("XMP {}: {error}", path.display());
                            info.rating = None;
                            info.error = Some(error);
                            self.rating_errors += 1;
                        }
                    }
                }
                self.rating_index_complete = complete;
                self.ratings_changed();
            }
            IoEvent::Metadata {
                session,
                path,
                metadata,
                rating,
            } if session == self.session => {
                let info = self.info.entry(path.clone()).or_default();
                info.read = true;
                match metadata {
                    Ok(metadata) => info.metadata = Some(metadata),
                    Err(error) => eprintln!("EXIF {}: {error}", path.display()),
                }
                if info.revision == 0 {
                    match rating {
                        Ok(rating) => {
                            info.rating = Some(rating.unwrap_or(0));
                            info.error = None;
                        }
                        Err(error) => {
                            eprintln!("XMP {}: {error}", path.display());
                            info.rating = None;
                            info.error = Some(error);
                        }
                    }
                }
                if self.displayed.as_ref() == Some(&path) {
                    if let (Some(renderer), Some(metadata)) = (&mut self.renderer, &info.metadata) {
                        renderer.set_orientation(metadata.orientation);
                    }
                }
                if self.pinned.as_ref().is_some_and(|(p, _)| p == &path) {
                    if let (Some(renderer), Some(metadata)) = (&mut self.renderer, &info.metadata) {
                        renderer.set_reference_orientation(metadata.orientation);
                    }
                }
                self.ratings_changed();
            }
            IoEvent::Saved {
                session,
                path,
                revision,
                edit_id,
                undo,
                rating,
                unsaved,
                result,
            } => {
                if self.log {
                    eprintln!(
                        "XMP save session={session} revision={revision} success={} {}",
                        result.is_ok(),
                        path.display()
                    );
                }
                let focus_undo =
                    undo && self.pending_undos.remove(&edit_id) == Some(self.interaction_serial);
                if result.is_err() && !undo {
                    self.rating_history.retain(|(id, _)| *id != edit_id);
                }
                if result.is_err() && undo && !unsaved {
                    // Keep failed/conflicting undo retryable, in its original
                    // order behind any newer user edits.
                    let position = self
                        .rating_history
                        .iter()
                        .position(|(id, _)| *id > edit_id)
                        .unwrap_or(self.rating_history.len());
                    self.rating_history.insert(
                        position,
                        (edit_id, deletion::UndoAction::Rating(path.clone())),
                    );
                    while self.rating_history.len() > 100 {
                        self.rating_history.pop_front();
                    }
                }
                let succeeded = result.is_ok();
                let mut applied = false;
                if let Some(info) = self.info.get_mut(&path) {
                    if info.revision == revision {
                        applied = true;
                        match result {
                            Ok(()) => {
                                info.saved_revision = revision;
                                info.error = None;
                                if undo {
                                    info.rating = Some(rating.unwrap_or(0));
                                }
                            }
                            Err(error) => {
                                eprintln!("XMP save {}: {error}", path.display());
                                info.error = Some(error);
                                if undo && !unsaved {
                                    info.saved_revision = revision;
                                }
                            }
                        }
                    }
                }
                if undo && succeeded {
                    // Restore filter membership first. Prefer the undone photo
                    // if it belongs to this folder and the active filter.
                    let preferred = if focus_undo && applied && self.all_files.contains(&path) {
                        Some(path.clone())
                    } else {
                        self.current()
                    };
                    self.refresh_visible(preferred.as_deref());
                    self.message(format!(
                        "Restored rating: {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                } else {
                    if undo && !succeeded {
                        self.notice = "Could not undo rating (see log); Cmd+Z retries".into();
                    }
                    self.status();
                }
            }
            IoEvent::Flushed { failures } if self.quitting => {
                if failures == 0 {
                    event_loop.exit();
                    return;
                }
                let choice = rfd::MessageDialog::new().set_title("Ratings could not be saved")
                    .set_description(format!("{failures} photo(s) have unsaved ratings. Retry saving, quit without those changes, or keep ARW Sprint open."))
                    .set_buttons(rfd::MessageButtons::YesNoCancelCustom("Retry".into(), "Quit Without Saving".into(), "Cancel".into())).show();
                match choice {
                    rfd::MessageDialogResult::Custom(ref value) if value == "Retry" => {
                        self.io.retry_failed()
                    }
                    rfd::MessageDialogResult::Custom(ref value)
                        if value == "Quit Without Saving" =>
                    {
                        event_loop.exit()
                    }
                    _ => {
                        self.quitting = false;
                        if !self.rating_index_complete {
                            self.rating_index_started = false;
                            self.ratings_read = 0;
                            self.rating_errors = 0;
                            self.start_rating_index();
                        }
                        self.message("Unsaved ratings");
                    }
                }
            }
            IoEvent::Rejected { session, result } if session == self.session => {
                if self.deleted_batch {
                    self.collected_deleted(result);
                    return;
                }
                match result {
                    Ok(paths) if !paths.is_empty() => {
                        let choice = rfd::MessageDialog::new().set_title("Move rejected photos to Trash?")
                            .set_description(format!("Move {} rejected photograph(s) to macOS Trash? XMP sidecars will stay in the folder. You can recover the RAWs from Trash.", paths.len()))
                            .set_buttons(rfd::MessageButtons::OkCancelCustom("Move to Trash".into(), "Cancel".into())).show();
                        if choice == rfd::MessageDialogResult::Custom("Move to Trash".into()) {
                            self.io.trash_confirmed(self.session, paths);
                            self.message("Moving rejected photos to Trash...");
                            return;
                        }
                    }
                    Ok(_) => self.alert(
                        "No rejected photos",
                        "No saved rejected photographs were found in this selection.",
                    ),
                    Err(error) => self.alert("Could not collect rejected photos", &error),
                }
                self.trash_busy = false;
                self.message("");
                let previous = self.current();
                self.refresh_visible(previous.as_deref());
            }
            IoEvent::Trashed {
                session,
                removed,
                errors,
            } if session == self.session => {
                self.trash_busy = false;
                if !errors.is_empty() {
                    self.alert("Some photos could not be moved", &errors.join("\n"));
                }
                if self.log {
                    eprintln!("moved {} rejected photos to macOS Trash", removed.len());
                }
                if let Some(folder) = self.folder.clone() {
                    self.open(folder);
                }
            }
            IoEvent::MovedDeleted {
                session,
                op_id,
                records,
                errors,
            } if session == self.session => self.moved_deleted(op_id, records, errors),
            IoEvent::RestoredDeleted {
                session,
                op_id,
                restored,
                ratings,
                failed,
            } if session == self.session => self.restored_deleted(op_id, restored, ratings, failed),
            _ => {}
        }
    }
    fn command(&mut self, command: Command) {
        if self.quitting {
            return;
        }
        match command {
            Command::Open if !self.trash_busy => {
                self.restore_allowed = false;
                let mut picker = rfd::FileDialog::new().set_title("Open folder containing Sony ARWs");
                if let Some(window) = &self.window { picker = picker.set_parent(window.as_ref()); }
                if let Some(folder) = picker.pick_folder() { self.open(folder); }
            }
            Command::Quit => self.quit(),
            Command::OpenDeleted if !self.trash_busy => {
                if let Some(folder) = &self.folder {
                    if deleted::is_rejected_folder(folder) {
                        self.message("Already viewing the _Rejected folder");
                    } else {
                        self.open(folder.join(deleted::REJECTED_FOLDER));
                    }
                }
            }
            Command::MoveDeletedCurrent => self.move_current_to_deleted(),
            Command::MoveDeletedRejected => self.collect_deleted(),
            Command::Filter(filter) => self.set_filter(filter),
            Command::Fullscreen => self.fullscreen(),
            Command::Filmstrip => self.toggle_filmstrip(),
            Command::Brighten => self.set_brightness(self.brightness_steps.saturating_add(1)),
            Command::Darken => self.set_brightness(self.brightness_steps.saturating_sub(1)),
            Command::ResetBrightness => self.set_brightness(0),
            Command::ZoomLock => {
                self.end_peek();
                self.zoom_locked = !self.zoom_locked;
                self.zoom_preference_changed = true;
                if let Some(renderer) = &mut self.renderer { renderer.set_preserve_view(self.zoom_locked); }
                self.persist_preferences();
                self.message(if self.zoom_locked { "Zoom and position locked across photos" } else { "New photos fit to window" });
            }
            Command::AutoAdvance => {
                self.auto_advance = !self.auto_advance;
                self.auto_preference_changed = true;
                self.persist_preferences();
                self.message(if self.auto_advance { "Auto-advance on: assigning a rating opens next photo" } else { "Auto-advance off" });
            }
            Command::UndoRating => self.undo_rating(),
            Command::Pin => self.pin_current(false),
            Command::ReplacePin => self.pin_current(true),
            Command::Fit => { if let Some(renderer) = &mut self.renderer { renderer.viewport_mut().fit(); } self.redraw(); }
            Command::Fill => { if let Some(renderer) = &mut self.renderer { renderer.viewport_mut().fill(); } self.redraw(); }
            Command::Actual => { if let Some(renderer) = &mut self.renderer { renderer.viewport_mut().actual_size(); } self.redraw(); }
            Command::TrashCurrent | Command::TrashRejected if !self.trash_busy && self.smoke.is_none() => {
                let files = if command == Command::TrashCurrent { Arc::new(self.current().into_iter().collect()) } else { Arc::clone(&self.all_files) };
                self.deleted_batch = false;
                self.trash_busy = true; self.io.collect_rejected(self.session, files); self.message("Checking saved rejects...");
            }
            Command::Help => self.alert("ARW Sprint shortcuts", "Right: next\nLeft / Shift+Space: previous\nTab: show / hide bottom thumbnails\nClick thumbnail: open it; scroll strip: browse without changing photo\nSpace / X: move photo + XMP to _Rejected, then advance\nU / 0: clear rating (does not restore a moved photo)\n1–5: stars    Cmd+Z: undo rating or move (last 100 actions this session)\nA: toggle auto-advance after assigning ratings\nCmd+Option+1–5: show only that star rating\nCmd+Option+0: show all photos\nCmd+Option+X: show rejected photos\nFilter menu: rated, unrated, or not rejected\nF: fullscreen    Z: fit / 100%\nL: keep zoom and position between photos\nHold P: temporary 100% peek at pointer; release to restore\nC: pin current photo / close comparison\nShift+C: replace pinned reference with current photo\nComparison: reference left, current right; linked zoom/pan\nS / +: zoom in    D / -: zoom out\n] / [: brighten / darken by 1/3 stop\nBackslash: reset viewing brightness\nBrightness carries across all photos and is saved on quit.\nWheel / pinch: zoom    Drag: pan\nCmd+O: open folder    Cmd+Q: quit\nCmd+Delete: move current rejected photo to Trash\n\nRestores last photo, brightness and A/L preferences on launch.\n100% uses embedded-preview pixels; 256 MiB mode decodes at half resolution.\nFile menu: open _Rejected or move previously marked rejects.\nRatings save to XMP sidecars. Viewing brightness never changes RAW or XMP.\nBrightening the JPEG cannot recover details missing from that preview.\nFolder scanning is not recursive.") ,
            _ => {}
        }
    }
    fn fullscreen(&self) {
        if let Some(window) = &self.window {
            window.set_fullscreen(if window.fullscreen().is_some() {
                None
            } else {
                Some(Fullscreen::Borderless(None))
            });
        }
    }
    fn key(&mut self, key: Key, repeat: bool) {
        if self.modifiers.super_key()
            || self.modifiers.control_key()
            || self.modifiers.alt_key()
            || self.quitting
        {
            return;
        } // Native menu owns Cmd shortcuts.
        if shortcuts::move_requested(&key, self.modifiers, repeat) {
            self.move_current_to_deleted();
            return;
        }
        match key.as_ref() {
            Key::Named(NamedKey::Tab) if !repeat => self.command(Command::Filmstrip),
            Key::Named(NamedKey::ArrowRight) => self.navigate(1),
            Key::Named(NamedKey::ArrowLeft) => self.navigate(-1),
            Key::Named(NamedKey::Space) | Key::Character(" ") => {
                if self.modifiers.shift_key() {
                    self.navigate(-1);
                }
            }
            Key::Named(NamedKey::Escape) => {
                if self
                    .window
                    .as_ref()
                    .is_some_and(|w| w.fullscreen().is_some())
                {
                    self.fullscreen();
                } else {
                    self.command(Command::Fit);
                }
            }
            Key::Character(character) => {
                let character = character.to_ascii_lowercase();
                // File moves, ratings and modes act once per press. Navigation
                // and incremental zoom/brightness may repeat.
                if repeat
                    && !matches!(
                        character.as_str(),
                        "+" | "=" | "-" | "_" | "s" | "d" | "[" | "]"
                    )
                {
                    return;
                }
                match character.as_str() {
                    "u" | "0" => self.rating(0),
                    "1" => self.rating(1),
                    "2" => self.rating(2),
                    "3" => self.rating(3),
                    "4" => self.rating(4),
                    "5" => self.rating(5),
                    "f" => self.fullscreen(),
                    "l" => self.command(Command::ZoomLock),
                    "a" => self.command(Command::AutoAdvance),
                    "c" => self.command(if self.modifiers.shift_key() {
                        Command::ReplacePin
                    } else {
                        Command::Pin
                    }),
                    "p" => {
                        if self.current().is_some() && self.current() == self.displayed {
                            if let Some(renderer) = &mut self.renderer {
                                renderer.begin_peek(self.cursor);
                            }
                            self.redraw();
                        }
                    }
                    "z" => {
                        if let Some(renderer) = &mut self.renderer {
                            renderer.viewport_mut().toggle_fit_actual();
                        }
                        self.redraw();
                    }
                    "+" | "=" | "s" => self.zoom(1.25),
                    "-" | "_" | "d" => self.zoom(0.8),
                    "]" => self.command(Command::Brighten),
                    "[" => self.command(Command::Darken),
                    "\\" => self.command(Command::ResetBrightness),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    fn set_brightness(&mut self, steps: i8) {
        // A display preference, independent of navigation, zoom and XMP ratings.
        // Mark even a neutral reset as user intent during asynchronous restore.
        let steps = steps.clamp(-9, 9);
        if self.brightness_preference_changed && self.brightness_steps == steps {
            return;
        }
        self.brightness_steps = steps;
        self.brightness_preference_changed = true;
        if let Some(renderer) = &mut self.renderer {
            renderer.set_brightness_stops(f32::from(self.brightness_steps) / 3.0);
        }
        self.persist_preferences();
        self.status();
    }
    fn zoom(&mut self, factor: f64) {
        if let Some(renderer) = &mut self.renderer {
            renderer.zoom(factor, Some(self.cursor));
        }
        self.redraw();
    }
    fn smoke_tick(&mut self, event_loop: &ActiveEventLoop) {
        fn displayed_geometry(app: &App) -> Result<(PathBuf, [u32; 2], u16), String> {
            let path = app.displayed.as_ref().ok_or("no displayed smoke photo")?;
            let texture = app
                .textures
                .iter()
                .find(|(cached, _)| cached == path)
                .map(|(_, texture)| texture)
                .ok_or("no displayed smoke texture")?;
            let orientation = app
                .info
                .get(path)
                .and_then(|info| info.metadata.as_ref())
                .map_or(texture.orientation, |metadata| metadata.orientation);
            let orientation = if (1..=8).contains(&orientation) {
                orientation
            } else {
                1
            };
            Ok((
                path.clone(),
                crate::renderer::viewport::oriented_size(
                    texture.original_width,
                    texture.original_height,
                    orientation,
                ),
                orientation,
            ))
        }

        fn remember_zoom_transition(app: &mut App) -> Result<(), String> {
            let (path, _, orientation) = displayed_geometry(app)?;
            let renderer = app.renderer.as_ref().ok_or("no smoke renderer")?;
            if !app.zoom_locked || !renderer.preserve_view() {
                return Err("zoom retention is not enabled for navigation check".into());
            }
            let saved = renderer.viewport().clone();
            app.smoke.as_mut().ok_or("no smoke state")?.zoom_transition =
                Some((saved, orientation, path, app.gpu_hits));
            Ok(())
        }

        fn check_zoom_transition(app: &mut App) -> Result<(), String> {
            let Some((saved, old_orientation, old_path, previous_gpu_hits)) = app
                .smoke
                .as_mut()
                .and_then(|smoke| smoke.zoom_transition.take())
            else {
                return Ok(());
            };
            let (path, dimensions, orientation) = displayed_geometry(app)?;
            let actual = app.renderer.as_ref().ok_or("no smoke renderer")?.viewport();
            // Derive the expected view from the prior snapshot and target photo,
            // allowing legitimate edge clamping for different preview dimensions.
            let mut expected = saved.for_geometry(dimensions, actual.view_size());
            let stored =
                crate::renderer::viewport::stored_focus(saved.normalized_focus(), old_orientation);
            expected.set_normalized_focus(crate::renderer::viewport::displayed_focus(
                stored,
                orientation,
            ));
            if actual.image_size() != dimensions
                || actual.mode() != expected.mode()
                || (actual.scale() - expected.scale()).abs() > 1e-10
                || actual
                    .normalized_focus()
                    .iter()
                    .zip(expected.normalized_focus())
                    .any(|(actual, expected)| (actual - expected).abs() > 1e-8)
            {
                return Err(format!(
                    "navigation lost zoom/subject position: expected {expected:?}, got {actual:?}"
                ));
            }
            eprintln!(
                "SMOKE ZOOM CONTINUITY: mode={:?} scale={:.6} focus={:?} changed_photo={} gpu_cache_hit={}",
                actual.mode(), actual.scale(), actual.normalized_focus(), path != old_path,
                app.gpu_hits > previous_gpu_hits,
            );
            Ok(())
        }

        fn zoom_shortcuts(app: &mut App) -> Result<(), String> {
            let identity = |app: &App| {
                (
                    app.navigator.index(),
                    app.current(),
                    app.displayed.clone(),
                    app.session,
                    app.generation.load(Ordering::Acquire),
                    app.requests,
                    app.textures
                        .iter()
                        .find(|(path, _)| Some(path) == app.displayed.as_ref())
                        .map(|(_, texture)| Arc::as_ptr(texture) as usize),
                )
            };
            let expected_identity = identity(app);
            if expected_identity.6.is_none() {
                return Err("no displayed GPU texture for zoom shortcut check".into());
            }
            let renderer = app.renderer.as_mut().ok_or("no smoke renderer")?;
            let viewport = renderer.viewport().clone();
            renderer.viewport_mut().actual_size();
            let modifiers = app.modifiers;
            app.modifiers = ModifiersState::empty();
            // Start at 100% so even the 8x8 fixture is below the zoom limit.
            // Exercise press/repeat and letter case without any file mutations.
            let result: Result<(), String> = (|| {
                for (key, repeat, expected_scale) in [
                    ("S", false, 1.25),
                    ("s", true, 1.5625),
                    ("D", false, 1.25),
                    ("d", true, 1.0),
                ] {
                    app.key(Key::Character(key.into()), repeat);
                    let scale = app
                        .renderer
                        .as_ref()
                        .ok_or("no smoke renderer")?
                        .viewport()
                        .scale();
                    if (scale - expected_scale).abs() > 1e-10 {
                        return Err("S/D press or repeat did not apply incremental zoom".into());
                    }
                    if identity(app) != expected_identity {
                        return Err("S/D changed main selection, loading, or GPU texture".into());
                    }
                }
                Ok(())
            })();
            app.modifiers = modifiers;
            if let Some(renderer) = &mut app.renderer {
                *renderer.viewport_mut() = viewport;
            }
            result?;
            eprintln!("SMOKE ZOOM SHORTCUTS: S/D press+repeat changed transforms only; no main-photo requests or texture replacement");
            Ok(())
        }

        fn brightness_shortcuts(app: &mut App, directory: &std::path::Path) -> Result<(), String> {
            let identity = |app: &App| {
                (
                    app.current(),
                    app.displayed.clone(),
                    app.generation.load(Ordering::Acquire),
                    app.requests,
                    app.revision_sequence,
                    app.textures
                        .iter()
                        .map(|(path, texture)| (path.clone(), Arc::as_ptr(texture) as usize))
                        .collect::<Vec<_>>(),
                    app.thumb_cache
                        .iter()
                        .map(|(path, texture)| (path.clone(), Arc::as_ptr(texture) as usize))
                        .collect::<Vec<_>>(),
                )
            };
            let expected = identity(app);
            let modifiers = app.modifiers;
            app.modifiers = ModifiersState::empty();
            let result = (|| -> Result<(), String> {
                for (key, count, steps, name) in [
                    ("\\", 1, 0, "original"),
                    ("]", 3, 3, "bright"),
                    ("[", 6, -3, "dim"),
                    ("\\", 1, 0, "reset"),
                    ("]", 40, 9, "upper-limit"),
                    ("[", 40, -9, "lower-limit"),
                ] {
                    for repeat in 0..count {
                        app.key(Key::Character(key.into()), repeat > 0);
                    }
                    let renderer = app.renderer.as_mut().ok_or("no brightness renderer")?;
                    if app.brightness_steps != steps
                        || (renderer.brightness_stops() - f32::from(steps) / 3.0).abs() > 1e-6
                    {
                        return Err(
                            "brightness shortcut/repeat/clamp did not reach expected value".into(),
                        );
                    }
                    renderer.capture(&directory.join(format!("brightness-{name}.ppm")))?;
                    if identity(app) != expected {
                        return Err("brightness changed selection, load requests, ratings or cached textures".into());
                    }
                }
                app.key(Key::Character("\\".into()), false);
                for modifier in [
                    ModifiersState::SUPER,
                    ModifiersState::CONTROL,
                    ModifiersState::ALT,
                ] {
                    app.modifiers = modifier;
                    app.key(Key::Character("]".into()), false);
                    if app.brightness_steps != 0 {
                        return Err("modified brightness key was not ignored".into());
                    }
                }
                app.modifiers = ModifiersState::empty();
                // Leave +1 EV active for the following navigation/comparison frames.
                for _ in 0..3 {
                    app.command(Command::Brighten);
                }
                Ok(())
            })();
            app.modifiers = modifiers;
            result?;
            eprintln!("SMOKE BRIGHTNESS: shortcuts/repeat/clamps/reset pass; no load requests, rating edits or texture replacement");
            Ok(())
        }

        fn stable_thumbnails(app: &mut App) -> Result<(), String> {
            let Some(smoke) = &mut app.smoke else {
                return Ok(());
            };
            if !app.filmstrip_visible {
                smoke.thumb_stable.clear();
                return Ok(());
            }
            if smoke.step < 33 {
                return Ok(());
            }
            let visible: Vec<_> = app
                .thumb_range
                .clone()
                .filter_map(|index| app.files.get(index))
                .collect();
            // Store addresses, never Arc clones: the diagnostic must not keep
            // textures alive or hide eviction/memory-pressure regressions.
            smoke
                .thumb_stable
                .retain(|(path, _)| visible.contains(&path));
            for (path, expected) in &smoke.thumb_stable {
                let actual = app
                    .thumb_cache
                    .iter()
                    .find(|(cached, _)| cached == path)
                    .map(|(_, texture)| Arc::as_ptr(texture) as usize);
                if actual != Some(*expected) {
                    return Err(format!(
                        "visible thumbnail disappeared or changed GPU allocation: {}",
                        path.display()
                    ));
                }
                smoke.thumb_stability_checks += 1;
            }
            for path in visible {
                if smoke.thumb_stable.iter().any(|(cached, _)| cached == path) {
                    continue;
                }
                if let Some((_, texture)) =
                    app.thumb_cache.iter().find(|(cached, _)| cached == path)
                {
                    smoke
                        .thumb_stable
                        .push((path.clone(), Arc::as_ptr(texture) as usize));
                }
            }
            Ok(())
        }

        fn remember_main(app: &mut App) -> Result<(), String> {
            let current = app.current();
            let viewport = app
                .renderer
                .as_ref()
                .ok_or("no renderer")?
                .viewport()
                .clone();
            let smoke = app.smoke.as_mut().ok_or("no smoke state")?;
            smoke.strip_main_index = app.navigator.index();
            smoke.strip_main_path = current;
            smoke.strip_displayed_path = app.displayed.clone();
            smoke.strip_generation = Some(app.generation.load(Ordering::Acquire));
            smoke.strip_requests = app.requests;
            smoke.strip_viewport = Some(viewport);
            Ok(())
        }

        fn unchanged_main(app: &App) -> Result<(), String> {
            let smoke = app.smoke.as_ref().ok_or("no smoke state")?;
            if app.navigator.index() != smoke.strip_main_index
                || app.current() != smoke.strip_main_path
                || app.displayed != smoke.strip_displayed_path
                || Some(app.generation.load(Ordering::Acquire)) != smoke.strip_generation
                || app.requests != smoke.strip_requests
                || app.renderer.as_ref().map(Renderer::viewport) != smoke.strip_viewport.as_ref()
            {
                return Err(
                    "scrolling the filmstrip changed main selection/loading/viewport".into(),
                );
            }
            Ok(())
        }

        fn scroll_to_edge(app: &mut App, end: bool) -> Result<(), String> {
            let size = app.window.as_ref().ok_or("no smoke window")?.inner_size();
            app.cursor = [
                f64::from(size.width) / 2.0,
                f64::from(size.height.saturating_sub(64)),
            ];
            remember_main(app)?;
            // Exercise the real wheel routing. Each large event is bounded to
            // eight cells; even the 10k-photo case needs only 1,251 cheap events.
            for _ in 0..app.files.len() / 8 + 2 {
                let before = app.thumb_range.clone();
                app.scroll(MouseScrollDelta::LineDelta(
                    0.0,
                    if end { -8.0 } else { 8.0 },
                ));
                unchanged_main(app)?;
                stable_thumbnails(app)?;
                if app.thumb_range == before {
                    break;
                }
            }
            let count = app.thumb_range.len();
            let expected = if end {
                app.files.len().saturating_sub(count)..app.files.len()
            } else {
                0..count
            };
            if app.thumb_range != expected || app.strip_window.range() != expected {
                return Err("filmstrip wheel did not reach the requested range edge".into());
            }
            Ok(())
        }

        // Run before timing/current-image gates: a cache purge must be caught
        // during a main upload, not only after replacement thumbnails refill it.
        if let Err(error) = stable_thumbnails(self) {
            self.fatal = Some(error);
            event_loop.exit();
            return;
        }
        let Some(smoke) = &self.smoke else {
            return;
        };
        if smoke.started.elapsed() > Duration::from_secs(90) {
            self.fatal = Some("desktop smoke test timed out".into());
            event_loop.exit();
            return;
        }
        if Instant::now() < smoke.next
            || (smoke.step == 0 && self.requested_at.elapsed() < Duration::from_millis(850))
            || self.scanning
            || (smoke.step < 11 && self.current().is_none())
            || (smoke.step >= 11 && !self.rating_index_complete)
            || self.current() != self.displayed
            || self.first_render
        {
            return;
        }
        let step = smoke.step;
        let directory = smoke.directory.clone();
        // Keep frames 0–32 identical to the original workflow smoke. Later
        // captures wait for the visible strip, never synchronously for a worker.
        if matches!(step, 33 | 35 | 36 | 37 | 40 | 41 | 42 | 43 | 45)
            && self.filmstrip_visible
            && self.thumb_range.clone().any(|index| {
                self.files.get(index).is_some_and(|path| {
                    !self.thumb_cache.iter().any(|(cached, _)| cached == path)
                        && !self.thumb_unavailable.contains(path)
                })
            })
        {
            if let Some(smoke) = &mut self.smoke {
                smoke.next = Instant::now() + Duration::from_millis(16);
            }
            return;
        }
        if matches!(step, 38 | 44) {
            let (started, completed) = self.thumbnails.activity();
            if started != completed {
                if let Some(smoke) = &mut self.smoke {
                    smoke.next = Instant::now() + Duration::from_millis(16);
                }
                return; // Let an already-running canceled read/decode unwind.
            }
        }
        let result = (|| -> Result<(), String> {
            check_zoom_transition(self)?;
            if step >= 11 {
                let expected = select_visible(&self.all_files, self.filter, |path| {
                    self.info.get(path).and_then(|info| info.rating)
                });
                if *self.files != expected || (self.files.is_empty() && self.displayed.is_some()) {
                    return Err("filtered navigation does not match indexed ratings".into());
                }
                eprintln!(
                    "SMOKE FILTER: {:?} visible={} total={} selected={:?}",
                    self.filter,
                    self.files.len(),
                    self.all_files.len(),
                    self.current()
                        .and_then(|path| path.file_name().map(|name| name.to_owned()))
                );
            }
            std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
            let renderer = self.renderer.as_mut().ok_or("no renderer")?;
            renderer.capture(&directory.join(format!("frame-{step}.ppm")))?;
            match step {
                0 => self.navigate(1),
                1 => self.navigate(-1),
                2 => self.navigate(1),
                3 => {
                    for _ in 0..10 {
                        self.navigate(1);
                    }
                }
                4 => self.navigate(-1),
                5 => {
                    renderer.viewport_mut().actual_size();
                    renderer.viewport_mut().pan(260.0, -120.0);
                    self.redraw();
                }
                6 => {
                    renderer.viewport_mut().fit();
                    if let Some(window) = &self.window {
                        let _ = window.request_inner_size(LogicalSize::new(1040.0, 720.0));
                    }
                    self.redraw();
                }
                7 | 8 => self.fullscreen(),
                9 => {
                    if self.navigator.seek(0) {
                        self.request_current();
                    }
                }
                10 => self.command(Command::Filter(PhotoFilter::Stars(5))),
                11 => self.navigate(1),
                12 => self.command(Command::Filter(PhotoFilter::Rejected)),
                13 => self.command(Command::Filter(PhotoFilter::Unrated)),
                14 => self.command(Command::Filter(PhotoFilter::Stars(3))),
                15 => self.command(Command::Filter(PhotoFilter::All)),
                // Exercise exposed background while drawables are reused,
                // including zoom below fit, restoration, and a photo change.
                16 => {
                    renderer.viewport_mut().fit();
                    renderer.viewport_mut().zoom(0.5, None);
                    self.redraw();
                }
                17 | 19 => {
                    renderer.viewport_mut().zoom(0.5, None);
                    self.redraw();
                }
                18 | 20 => {
                    renderer.viewport_mut().zoom(2.0, None);
                    self.redraw();
                }
                21 => {
                    remember_zoom_transition(self)?;
                    self.navigate(1);
                }
                22 => {
                    renderer.viewport_mut().fit();
                    self.redraw();
                }
                23 => {
                    if !self.zoom_locked {
                        return Err("zoom retention was not enabled by default".into());
                    }
                    if self.navigator.seek(0) {
                        self.request_current();
                    }
                    self.command(Command::Actual);
                    if let Some(renderer) = &mut self.renderer {
                        renderer.pan(150.0, -90.0);
                    }
                    self.redraw();
                }
                24 => {
                    remember_zoom_transition(self)?;
                    self.navigate(1);
                }
                25 => {
                    if renderer.viewport().mode()
                        != crate::renderer::viewport::DisplayMode::ActualSize
                    {
                        return Err("zoom lock was lost while navigating".into());
                    }
                    let before = renderer.viewport().clone();
                    renderer.begin_peek([350.0, 300.0]);
                    renderer.end_peek();
                    if renderer.viewport() != &before {
                        return Err("peek did not restore viewport".into());
                    }
                    self.command(Command::Fit);
                    self.pin_current(false);
                }
                26 => self.navigate(1),
                27 => {
                    if self.pinned.is_none() {
                        return Err("reference did not survive navigation".into());
                    }
                    self.cursor = [220.0, 260.0];
                    self.key(Key::Character("p".into()), false);
                }
                28 => {
                    if !renderer.is_peeking() {
                        return Err("held focus peek was not active".into());
                    }
                    self.end_peek();
                    self.navigate(-1);
                }
                29 => {
                    if renderer.is_peeking() {
                        return Err("focus peek survived release/navigation".into());
                    }
                    self.pin_current(true);
                    self.command(Command::Actual);
                    if let Some(renderer) = &mut self.renderer {
                        renderer.pan_at(120.0, -70.0, Some([200.0, 200.0]));
                    }
                    self.redraw();
                }
                30 => self.pin_current(false),
                31 => {
                    self.command(Command::ZoomLock);
                    self.command(Command::Fit);
                    self.command(Command::AutoAdvance);
                    self.key(Key::Character("a".into()), true);
                    if !self.auto_advance {
                        return Err("repeated A toggled auto-advance".into());
                    }
                    self.command(Command::AutoAdvance);
                    self.redraw();
                }
                32 => {
                    if self.filmstrip_visible {
                        return Err("filmstrip unexpectedly enabled before Tab smoke stage".into());
                    }
                    self.key(Key::Named(NamedKey::Tab), false);
                    if !self.filmstrip_visible {
                        return Err("Tab did not show the filmstrip".into());
                    }
                    self.key(Key::Named(NamedKey::Tab), true);
                    if !self.filmstrip_visible {
                        return Err("repeated Tab hid the filmstrip".into());
                    }
                }
                33 => {
                    if !renderer.filmstrip_visible() || self.thumb_range.is_empty() {
                        return Err("visible filmstrip has no displayed cells".into());
                    }
                    let size = self.window.as_ref().ok_or("no smoke window")?.inner_size();
                    let y = f64::from(size.height.saturating_sub(64));
                    let current = self.navigator.index();
                    let hit = (0..size.width)
                        .step_by(4)
                        .find_map(|x| {
                            let point = [f64::from(x), y];
                            renderer
                                .filmstrip_hit(point)
                                .filter(|index| Some(*index) != current || self.files.len() == 1)
                                .map(|index| (point, index))
                        })
                        .ok_or("could not hit a filmstrip cell")?;
                    eprintln!("SMOKE FILMSTRIP: visible={} cached={} unavailable={} decodes={} uploads={} click_index={}",
                        self.thumb_range.len(), self.thumb_cache.len(), self.thumb_unavailable.len(), self.thumb_decodes, self.thumb_uploads, hit.1);
                    if !self.filmstrip_click(hit.0) || self.navigator.index() != Some(hit.1) {
                        return Err("filmstrip click did not select the hit photo".into());
                    }
                    stable_thumbnails(self)?;
                }
                34 => {
                    let size = self.window.as_ref().ok_or("no smoke window")?.inner_size();
                    self.cursor = [
                        f64::from(size.width) / 2.0,
                        f64::from(size.height.saturating_sub(64)),
                    ];
                    if !renderer.over_filmstrip(self.cursor) {
                        return Err("wheel smoke pointer missed the filmstrip".into());
                    }
                    let before = self.thumb_range.clone();
                    remember_main(self)?;
                    self.scroll(MouseScrollDelta::LineDelta(0.0, -1.0));
                    unchanged_main(self)?;
                    stable_thumbnails(self)?;
                    if self.files.len() > before.len()
                        && before.end < self.files.len()
                        && self.thumb_range != (before.start + 1..before.end + 1)
                    {
                        return Err("filmstrip wheel did not move only the visible range".into());
                    }
                    if self.files.len() <= before.len() && self.thumb_range != before {
                        return Err("filmstrip scrolled despite every photo fitting".into());
                    }
                    if let Some(window) = &self.window {
                        let _ = window.request_inner_size(LogicalSize::new(1180.0, 760.0));
                    }
                    self.redraw();
                }
                35 => {
                    for _ in 0..10 {
                        self.navigate(1);
                        stable_thumbnails(self)?;
                    }
                    for _ in 0..3 {
                        self.navigate(-1);
                        stable_thumbnails(self)?;
                    }
                }
                36 => {
                    if !self.filmstrip_visible {
                        return Err("rapid navigation hid the filmstrip".into());
                    }
                    self.pin_current(false);
                    self.navigate(1);
                    stable_thumbnails(self)?;
                }
                37 => {
                    if self.pinned.is_none() || !renderer.filmstrip_visible() {
                        return Err("reference comparison and filmstrip did not coexist".into());
                    }
                    self.key(Key::Named(NamedKey::Tab), false);
                    if self.filmstrip_visible {
                        return Err("Tab did not hide the filmstrip".into());
                    }
                }
                38 => {
                    if self.filmstrip_visible
                        || renderer.filmstrip_visible()
                        || !self.thumb_cache.is_empty()
                        || self.thumb_plan_sent
                        || self.thumb_deadline.is_some()
                    {
                        return Err("hidden filmstrip retained cache or scheduled work".into());
                    }
                    let activity = self.thumbnails.activity();
                    if let Some(smoke) = &mut self.smoke {
                        smoke.thumb_idle_counts = Some(activity);
                    }
                    if self.pinned.is_some() {
                        self.pin_current(false);
                    }
                    eprintln!(
                        "SMOKE FILMSTRIP HIDDEN: started={} completed={}",
                        activity.0, activity.1
                    );
                }
                39 => {
                    let activity = self.thumbnails.activity();
                    if self
                        .smoke
                        .as_ref()
                        .and_then(|smoke| smoke.thumb_idle_counts)
                        != Some(activity)
                        || activity.0 != activity.1
                        || self.filmstrip_visible
                        || self.thumb_plan_sent
                        || self.thumb_deadline.is_some()
                    {
                        return Err("thumbnail jobs continued while the strip was hidden".into());
                    }
                    eprintln!(
                        "SMOKE FILMSTRIP IDLE: started={} completed={} decodes={} uploads={}",
                        activity.0, activity.1, self.thumb_decodes, self.thumb_uploads
                    );
                    self.key(Key::Named(NamedKey::Tab), false);
                    scroll_to_edge(self, true)?;
                }
                40 => {
                    // Far-away thumbnail reads/uploads have now completed, but
                    // the selected full-resolution image must not have changed.
                    unchanged_main(self)?;
                    scroll_to_edge(self, false)?;
                }
                41 => {
                    unchanged_main(self)?;
                    let before = self.thumb_range.clone();
                    let target = before
                        .end
                        .checked_sub(1)
                        .ok_or("no visible thumbnail to click")?;
                    let size = self.window.as_ref().ok_or("no smoke window")?.inner_size();
                    let y = f64::from(size.height.saturating_sub(64));
                    let hit = (0..size.width)
                        .step_by(4)
                        .find_map(|x| {
                            let point = [f64::from(x), y];
                            self.renderer
                                .as_ref()
                                .and_then(|renderer| renderer.filmstrip_hit(point))
                                .filter(|index| *index == target)
                                .map(|_| point)
                        })
                        .ok_or("could not hit the last visible thumbnail")?;
                    if !self.filmstrip_click(hit) || self.navigator.index() != Some(target) {
                        return Err("filmstrip click did not change the main selection".into());
                    }
                    if self.thumb_range != before {
                        return Err(
                            "clicking an already-visible thumbnail recentered the strip".into()
                        );
                    }
                    stable_thumbnails(self)?;
                }
                42 => {
                    let before = self.thumb_range.clone();
                    let current = self
                        .navigator
                        .index()
                        .ok_or("no main photo for arrow reveal")?;
                    if before.len() > 1 {
                        self.navigate(-1);
                        stable_thumbnails(self)?;
                        if self.thumb_range != before {
                            return Err("arrow navigation inside the strip moved its range".into());
                        }
                        self.navigate(1);
                        stable_thumbnails(self)?;
                        if self.thumb_range != before {
                            return Err("returning to the visible edge recentered the strip".into());
                        }
                    }
                    self.navigate(1);
                    stable_thumbnails(self)?;
                    let expected = if current + 1 < self.files.len() {
                        before.start + 1..before.end + 1
                    } else {
                        before
                    };
                    if self.thumb_range != expected {
                        return Err("arrow reveal did not shift by exactly one edge cell".into());
                    }
                    if let Some(smoke) = &mut self.smoke {
                        smoke.strip_expected_range = Some(expected);
                    }
                }
                43 => {
                    if self
                        .smoke
                        .as_ref()
                        .and_then(|smoke| smoke.strip_expected_range.as_ref())
                        != Some(&self.thumb_range)
                    {
                        return Err(
                            "thumbnail completion moved the strip after arrow reveal".into()
                        );
                    }
                    self.key(Key::Named(NamedKey::Tab), false);
                    if self.filmstrip_visible {
                        return Err("final Tab failed to hide strip".into());
                    }
                }
                44 => {
                    let activity = self.thumbnails.activity();
                    if self.filmstrip_visible
                        || renderer.filmstrip_visible()
                        || !self.thumb_cache.is_empty()
                        || self.thumb_plan_sent
                        || self.thumb_deadline.is_some()
                        || activity.0 != activity.1
                    {
                        return Err("final hidden filmstrip retained cache or work".into());
                    }
                    zoom_shortcuts(self)?;
                    eprintln!(
                        "SMOKE FILMSTRIP STABLE: pointer_checks={} started={} completed={}",
                        self.smoke
                            .as_ref()
                            .map_or(0, |smoke| smoke.thumb_stability_checks),
                        activity.0,
                        activity.1
                    );
                    self.command(Command::Fit);
                    self.command(Command::ResetBrightness);
                    self.command(Command::Filmstrip);
                }
                45 => {
                    brightness_shortcuts(self, &directory)?;
                    self.navigate(
                        if self.navigator.index().unwrap_or(0) + 1 < self.files.len() {
                            1
                        } else {
                            -1
                        },
                    );
                }
                46 | 47 => {
                    if self.brightness_steps != 3
                        || (renderer.brightness_stops() - 1.0).abs() > 1e-6
                    {
                        return Err("navigation/comparison reset viewing brightness".into());
                    }
                    if step == 46 {
                        self.command(Command::Pin);
                    } else {
                        self.command(Command::ResetBrightness);
                    }
                }
                _ => {
                    if self.brightness_steps != 0 || renderer.brightness_stops() != 0.0 {
                        return Err("viewing brightness reset failed after comparison".into());
                    }
                    eprintln!("SMOKE BRIGHTNESS CONTINUITY: navigation/comparison preserved +1 EV; reset restored neutral");
                    eprintln!("SMOKE PASS: folder={} files={} requests={} gpu_hits={} cpu_hits={} prefetch_hits={} managed={:.1}/{:.0}MiB", self.folder.as_ref().map(|p| p.display().to_string()).unwrap_or_default(), self.files.len(), self.requests, self.gpu_hits, self.cpu_hits, self.prefetch_hits, self.managed_used() as f64 /1048576.0, self.managed_limit() as f64 /1048576.0);
                    self.smoke = None;
                    self.quit();
                    return Ok(());
                }
            }
            if let Some(smoke) = &mut self.smoke {
                smoke.step += 1;
                smoke.next = Instant::now() + Duration::from_millis(850);
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fatal = Some(error);
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let result = (|| -> Result<(), String> {
            let window = Arc::new(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("ARW Sprint")
                            .with_inner_size(LogicalSize::new(1280.0, 850.0))
                            .with_min_inner_size(LogicalSize::new(480.0, 320.0)),
                    )
                    .map_err(|e| e.to_string())?,
            );
            let renderer = Renderer::new(Arc::clone(&window))?;
            let proxy = self.proxy.clone();
            let thumb_proxy = self.proxy.clone();
            self.uploader = Some(upload::UploadWorker::new(
                renderer.uploader(),
                Arc::clone(&self.budget),
                Arc::clone(&self.thumbnail_budget),
                Arc::clone(&self.loader),
                Arc::new(move |event| {
                    let _ = proxy.send_event(Event::Uploaded(event));
                }),
                Arc::new(move |event| {
                    let _ = thumb_proxy.send_event(Event::ThumbUploaded(event));
                }),
            )?);
            self.renderer = Some(renderer);
            if let Some(renderer) = &mut self.renderer {
                renderer.set_preserve_view(self.zoom_locked);
                renderer.set_brightness_stops(f32::from(self.brightness_steps) / 3.0);
            }
            self.window = Some(window);
            self.menu = Some(AppMenu::new()?);
            self.status();
            if let Some(path) = self.initial_path.take() {
                self.open(path);
            } else {
                self.redraw();
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("ARW Sprint startup: {error}");
            self.fatal = Some(error.clone());
            self.alert("ARW Sprint could not start", &error);
            event_loop.exit();
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Open(path) => self.open(path),
            Event::Scan(result) => self.scanned(result),
            Event::Load(event) => {
                self.loaded(event);
                self.arm_thumbnails();
            }
            Event::Uploaded(event) => {
                self.uploaded(event);
                self.arm_thumbnails();
            }
            Event::Thumbnail(event) => self.thumbnail_event(event),
            Event::ThumbUploaded(event) => self.thumbnail_uploaded(event),
            Event::Io(event) => self.io_event(event, event_loop),
            Event::Session(event) => self.session_event(event),
            Event::Menu(event) => {
                if let Some(command) = self.menu.as_ref().and_then(|menu| menu.command(&event)) {
                    self.command(command);
                }
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.window.as_ref().is_none_or(|w| w.id() != id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.quit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size);
                }
                self.refresh_filmstrip();
                self.arm_thumbnails();
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer {
                    match renderer.render() {
                        Err(error) => {
                            eprintln!("render: {error}");
                            self.fatal = Some(error);
                            self.quit();
                        }
                        Ok(true) if self.first_render => {
                            if self.log {
                                eprintln!("first_frame_submitted={:.2}ms (request to present submission; not display scanout)", self.requested_at.elapsed().as_secs_f64()*1000.0);
                            }
                            self.first_render = false;
                            if let (Some(session), Some(path)) =
                                (&self.session_store, &self.displayed)
                            {
                                session.remember(path.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::DroppedFile(path) => self.open(path),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Released
                    && event.physical_key == PhysicalKey::Code(KeyCode::KeyP)
                {
                    self.end_peek();
                } else if event.state == ElementState::Pressed {
                    self.key(event.logical_key, event.repeat);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
                    if let Some(renderer) = &mut self.renderer {
                        renderer.pan_at(
                            position.x - self.cursor[0],
                            position.y - self.cursor[1],
                            Some(self.cursor),
                        );
                    }
                    self.redraw();
                }
                self.cursor = [position.x, position.y];
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging =
                    state == ElementState::Pressed && !self.filmstrip_click(self.cursor);
            }
            WindowEvent::MouseWheel { delta, .. } => self.scroll(delta),
            WindowEvent::PinchGesture { delta, .. } => self.zoom((1.0 + delta).clamp(0.2, 5.0)),
            WindowEvent::Focused(false) => {
                self.dragging = false;
                self.modifiers = ModifiersState::empty();
                self.end_peek();
            }
            _ => {}
        }
        if event_loop.exiting() {
            self.folder_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.smoke_tick(event_loop);
        if self
            .thumb_deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            self.thumb_deadline = None;
            self.start_thumbnails(true);
        }
        let deadline = if self.smoke.is_some() {
            Some(
                self.thumb_deadline
                    .map_or(Instant::now() + Duration::from_millis(50), |deadline| {
                        deadline.min(Instant::now() + Duration::from_millis(50))
                    }),
            )
        } else {
            self.thumb_deadline
        };
        event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}
#[cfg(test)]
mod workflow_tests {
    use super::*;

    #[test]
    fn auto_advance_selects_once_before_filter_removal_and_stops_at_end() {
        let files: Vec<_> = ["1.ARW", "2.ARW", "3.ARW"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let destination = rating_destination(&files, Some(0), true);
        let remaining = &files[1..];
        assert_eq!(
            choose_selection(remaining, destination.as_deref(), Some(0)),
            Some(0)
        );
        assert_eq!(
            rating_destination(&files, Some(2), true),
            Some(files[2].clone())
        );
        assert_eq!(
            rating_destination(&files, Some(1), false),
            Some(files[1].clone())
        );
        assert_eq!(rating_destination(&[], None, true), None);
    }
}
