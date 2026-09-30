//! One small, cached translucent card. Native font work stays on a sleeping
//! worker; photo transforms reuse its texture and never start more raster work.

use super::{texture, transform_binding, transform_bytes, viewport::Pane, GpuUploader};
use std::{
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};
use winit::window::Window;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RasterKey {
    content_revision: u64,
    width: u32,
    height: u32,
    scale_bits: u64,
}

impl RasterKey {
    fn same_geometry(self, other: Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.scale_bits == other.scale_bits
    }
}

#[derive(Clone, Copy, Debug)]
struct CardLayout {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    scale: f64,
}

impl CardLayout {
    fn new(pane: Pane, scale: f64, rows: usize, comparison: bool) -> Option<Self> {
        let scale = if scale.is_finite() {
            scale.clamp(1.0, 4.0)
        } else {
            1.0
        };
        let margin = (16.0 * scale).round() as u32;
        // Leave room for the existing CURRENT label in comparison mode.
        let top = if comparison {
            (34.0 * scale).round() as u32
        } else {
            margin
        };
        let width = ((352.0 * scale).round() as u32).min(pane.width.saturating_sub(margin * 2));
        // Reserve the normal eight EXIF rows and two-line lens area even while
        // metadata is pending or absent. Changing photos must not shrink/expand
        // the glass shell before the background read finishes.
        let height = (((94 + rows.clamp(8, 16) * 27 + 18) as f64 * scale).round() as u32)
            .min(2048)
            .min(pane.height.saturating_sub(top + margin));
        if width < (172.0 * scale) as u32 || height < (115.0 * scale) as u32 {
            return None;
        }
        Some(Self {
            x: pane.x + pane.width - margin - width,
            y: pane.y + top,
            width,
            height,
            scale,
        })
    }

    fn placement(self, viewport: [u32; 2]) -> [f32; 4] {
        [
            self.width as f32 / viewport[0] as f32,
            self.height as f32 / viewport[1] as f32,
            (self.x * 2 + self.width) as f32 / viewport[0] as f32 - 1.0,
            1.0 - (self.y * 2 + self.height) as f32 / viewport[1] as f32,
        ]
    }
}

struct CardTexture {
    _texture: wgpu::Texture,
    binding: wgpu::BindGroup,
    key: RasterKey,
}

pub(super) struct PhotoDetails {
    visible: bool,
    content: String,
    content_revision: u64,
    desired: Option<RasterKey>,
    requested: Option<RasterKey>,
    card: Option<CardTexture>,
    shell: Option<CardTexture>,
    worker: Option<RasterWorker>,
    uniform: wgpu::Buffer,
    transform: wgpu::BindGroup,
    rasterizations: u64,
    uploads: u64,
    layout: Option<CardLayout>,
    layout_checked: bool,
}

impl PhotoDetails {
    pub(super) fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
        let (uniform, transform) = transform_binding(device, layout);
        Self {
            visible: false,
            content: String::new(),
            content_revision: 0,
            desired: None,
            requested: None,
            card: None,
            shell: None,
            worker: None,
            uniform,
            transform,
            rasterizations: 0,
            uploads: 0,
            layout: None,
            layout_checked: false,
        }
    }

    pub(super) fn set(&mut self, content: Option<&str>) {
        self.visible = content.is_some();
        if let Some(content) = content {
            if self.content == content {
                return;
            }
            let content = bounded_content(content);
            if self.content != content {
                self.content = content;
                self.content_revision = self.content_revision.wrapping_add(1);
                self.desired = None;
                self.layout_checked = false;
            }
        } else if self.requested.take().is_some() {
            if let Some(worker) = &self.worker {
                worker.cancel();
            }
        }
    }

    pub(super) fn content(&self) -> Option<&str> {
        self.visible.then_some(self.content.as_str())
    }

    pub(super) fn visible(&self) -> bool {
        self.visible
    }

    pub(super) fn ready(&self) -> bool {
        !self.visible
            || (self.layout_checked
                && (self.layout.is_none()
                    || self
                        .desired
                        .is_some_and(|key| self.card.as_ref().is_some_and(|card| card.key == key))))
    }

    pub(super) fn invalidate_layout(&mut self) {
        self.layout_checked = false;
    }

    pub(super) fn rasterizations(&self) -> u64 {
        self.rasterizations
    }
    pub(super) fn uploads(&self) -> u64 {
        self.uploads
    }

    pub(super) fn background_ready(&self) -> bool {
        self.desired.is_some_and(|key| {
            self.shell
                .as_ref()
                .is_some_and(|shell| shell.key.same_geometry(key))
        })
    }

    pub(super) fn texture_bytes(&self) -> u64 {
        [&self.card, &self.shell]
            .into_iter()
            .flatten()
            .map(|card| u64::from(card.key.width) * u64::from(card.key.height) * 4)
            .sum()
    }

    pub(super) fn update(
        &mut self,
        uploader: &GpuUploader,
        window: &Arc<Window>,
        pane: Pane,
        viewport: [u32; 2],
        comparison: bool,
    ) {
        if !self.visible {
            return;
        }
        self.layout = CardLayout::new(
            pane,
            window.scale_factor(),
            self.content.lines().count().saturating_sub(1),
            comparison,
        );
        self.layout_checked = true;
        let Some(layout) = self.layout else {
            self.desired = None;
            if self.requested.take().is_some() {
                if let Some(worker) = &self.worker {
                    worker.cancel();
                }
            }
            return;
        };
        let key = RasterKey {
            content_revision: self.content_revision,
            width: layout.width,
            height: layout.height,
            scale_bits: layout.scale.to_bits(),
        };
        self.desired = Some(key);
        uploader.queue.write_buffer(
            &self.uniform,
            0,
            &transform_bytes(layout.placement(viewport), 1),
        );
        if let Some(result) = self.worker.as_ref().and_then(RasterWorker::take) {
            if result.key == key {
                self.requested = None;
                let started = Instant::now();
                let mut uploaded_bytes = 0;
                if let Some(Ok(rgba)) = result.shell {
                    uploaded_bytes += rgba.len();
                    self.shell = Some(upload_card(uploader, result.key, &rgba));
                    self.uploads += 1;
                }
                match result.rgba {
                    Ok(rgba) => {
                        uploaded_bytes += rgba.len();
                        self.card = Some(upload_card(uploader, result.key, &rgba));
                        self.uploads += 1;
                        if std::env::var_os("FASTCULL_LOG").is_some() {
                            eprintln!(
                                "photo details upload: {:.3} ms, {} bytes",
                                started.elapsed().as_secs_f64() * 1000.0,
                                uploaded_bytes
                            );
                        }
                    }
                    Err(error) => {
                        eprintln!("photo details: {error}");
                        // Remember the failed request to avoid a redraw/retry loop.
                        self.requested = Some(key);
                    }
                }
            }
        }
        if !self.card.as_ref().is_some_and(|card| card.key == key) && self.requested != Some(key) {
            if self.worker.is_none() {
                match RasterWorker::new(Arc::clone(window)) {
                    Ok(worker) => self.worker = Some(worker),
                    Err(error) => {
                        eprintln!("photo details worker: {error}");
                        self.requested = Some(key);
                        return;
                    }
                }
            }
            if let Some(worker) = &self.worker {
                let needs_shell = !self
                    .shell
                    .as_ref()
                    .is_some_and(|shell| shell.key.same_geometry(key));
                worker.request(key, self.content.clone(), needs_shell);
                self.requested = Some(key);
                self.rasterizations += 1;
            }
        }
    }

    pub(super) fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if !self.visible || self.layout.is_none() {
            return;
        }
        let Some(key) = self.desired else {
            return;
        };
        let card = self
            .card
            .as_ref()
            .filter(|card| card.key == key)
            .or_else(|| {
                self.shell
                    .as_ref()
                    .filter(|shell| shell.key.same_geometry(key))
            });
        if let Some(card) = card {
            pass.set_bind_group(0, &card.binding, &[]);
            pass.set_bind_group(1, &self.transform, &[]);
            pass.draw(0..6, 0..1);
        }
    }
}

fn bounded_content(content: &str) -> String {
    content
        .lines()
        .take(17)
        .map(|line| {
            line.chars()
                .filter(|ch| *ch != '\0')
                .take(256)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn upload_card(uploader: &GpuUploader, key: RasterKey, rgba: &[u8]) -> CardTexture {
    let texture = uploader.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("photo details card"),
        size: wgpu::Extent3d {
            width: key.width,
            height: key.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // Native pixels are already linear and premultiplied, unlike photo JPEGs.
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    uploader.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(key.width * 4),
            rows_per_image: Some(key.height),
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let binding =
        texture::image_binding(&uploader.device, &uploader.layout, &uploader.sampler, &view);
    CardTexture {
        _texture: texture,
        binding,
        key,
    }
}

struct RasterRequest {
    key: RasterKey,
    content: String,
    generation: u64,
    needs_shell: bool,
}
struct RasterResult {
    key: RasterKey,
    rgba: Result<Vec<u8>, String>,
    shell: Option<Result<Vec<u8>, String>>,
}

#[derive(Default)]
struct WorkerState {
    pending: Option<RasterRequest>,
    result: Option<RasterResult>,
    generation: u64,
    stopped: bool,
}

struct RasterWorker {
    state: Arc<(Mutex<WorkerState>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

impl RasterWorker {
    fn new(window: Arc<Window>) -> Result<Self, std::io::Error> {
        let state = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let shared = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("photo-details".into())
            .spawn(move || loop {
                let request = {
                    let (lock, wake) = &*shared;
                    let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
                    while state.pending.is_none() && !state.stopped {
                        state = wake.wait(state).unwrap_or_else(|p| p.into_inner());
                    }
                    if state.stopped {
                        return;
                    }
                    state.pending.take()
                };
                let Some(request) = request else {
                    continue;
                };
                // Auxiliary text work yields to requested-photo decoding.
                let _ = crate::platform::set_thumbnail_thread_qos();
                let started = Instant::now();
                let rgba = crate::platform::rasterize_photo_details(
                    &request.content,
                    request.key.width,
                    request.key.height,
                    f64::from_bits(request.key.scale_bits),
                );
                // Reuse this plain shell while the next photograph's text is
                // prepared, without retaining the previous photograph's fields.
                let shell = request.needs_shell.then(|| {
                    crate::platform::rasterize_photo_details(
                        "",
                        request.key.width,
                        request.key.height,
                        f64::from_bits(request.key.scale_bits),
                    )
                });
                if std::env::var_os("FASTCULL_LOG").is_some() {
                    eprintln!(
                        "photo details raster: {:.3} ms (worker), {}x{}",
                        started.elapsed().as_secs_f64() * 1000.0,
                        request.key.width,
                        request.key.height
                    );
                }
                let publish = {
                    let mut state = shared.0.lock().unwrap_or_else(|p| p.into_inner());
                    if state.stopped {
                        return;
                    }
                    if state.generation == request.generation {
                        state.result = Some(RasterResult {
                            key: request.key,
                            rgba,
                            shell,
                        });
                        true
                    } else {
                        false
                    }
                };
                if publish {
                    window.request_redraw();
                }
            })?;
        Ok(Self {
            state,
            handle: Some(handle),
        })
    }

    fn request(&self, key: RasterKey, content: String, needs_shell: bool) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.pending = Some(RasterRequest {
            key,
            content,
            generation: state.generation,
            needs_shell,
        });
        state.result = None;
        self.state.1.notify_one();
    }

    fn cancel(&self) {
        let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.result = None;
    }

    fn take(&self) -> Option<RasterResult> {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .result
            .take()
    }
}

impl Drop for RasterWorker {
    fn drop(&mut self) {
        {
            let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
            state.stopped = true;
            state.pending = None;
            self.state.1.notify_one();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_or_missing_metadata_keeps_the_glass_shell_geometry() {
        let pane = Pane {
            x: 0,
            y: 0,
            width: 1800,
            height: 1200,
        };
        let loading = CardLayout::new(pane, 2.0, 2, false).unwrap();
        let complete = CardLayout::new(pane, 2.0, 8, false).unwrap();
        assert_eq!(
            (loading.width, loading.height),
            (complete.width, complete.height)
        );
        let old = RasterKey {
            content_revision: 1,
            width: complete.width,
            height: complete.height,
            scale_bits: 2.0_f64.to_bits(),
        };
        let next = RasterKey {
            content_revision: 2,
            ..old
        };
        assert_ne!(old, next); // Old text cannot qualify as the next photo's card.
        assert!(old.same_geometry(next)); // Its empty shell remains reusable.
    }

    #[test]
    fn card_stays_inside_current_pane_above_chrome() {
        let pane = Pane {
            x: 800,
            y: 0,
            width: 800,
            height: 700,
        };
        let card = CardLayout::new(pane, 2.0, 8, true).unwrap();
        assert!(card.x >= pane.x && card.y >= pane.y);
        assert!(card.x + card.width <= pane.x + pane.width);
        assert!(card.y + card.height <= pane.y + pane.height);
        assert!(card.y >= 68);
    }

    #[test]
    fn tiny_viewports_hide_card_and_large_scale_stays_bounded() {
        assert!(CardLayout::new(
            Pane {
                x: 0,
                y: 0,
                width: 100,
                height: 100
            },
            2.0,
            8,
            false
        )
        .is_none());
        let card = CardLayout::new(
            Pane {
                x: 0,
                y: 0,
                width: 10000,
                height: 10000,
            },
            8.0,
            usize::MAX,
            false,
        )
        .unwrap();
        assert!(card.width <= 2048 && card.height <= 2048);
        assert_eq!(card.scale, 4.0);
    }

    #[test]
    fn metadata_text_is_bounded_without_breaking_unicode() {
        let content = format!("猫\0.ARW\n{}", "Camera\tα7 V".repeat(5000));
        let text = bounded_content(&content);
        assert!(!text.contains('\0'));
        assert_eq!(text.lines().nth(1).unwrap().chars().count(), 256);
        assert!(text.starts_with("猫.ARW\n"));
        assert_eq!(bounded_content(&"a\n".repeat(100)).lines().count(), 17);
    }
}
