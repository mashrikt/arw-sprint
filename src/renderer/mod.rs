//! Minimal Metal rendering. JPEG uploads belong to `GpuUploader` on its worker.

#[cfg(test)]
mod brightness_tests;
mod filmstrip;
pub mod texture;
pub mod viewport;

pub use filmstrip::FilmstripItem;
pub use texture::{GpuUploader, PreparedTexture, UploadError, UploadTimings};
pub use viewport::{DisplayMode, Viewport};

use font8x8::UnicodeFonts;
use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};
use winit::{dpi::PhysicalSize, window::Window};

const STATUS_HEIGHT: u32 = 32;
const FONT_SCALE: u32 = 2;

struct TextTexture {
    _texture: wgpu::Texture,
    binding: wgpu::BindGroup,
    width: u32,
    height: u32,
}

struct Caption {
    text: TextTexture,
    uniform: wgpu::Buffer,
    transform: wgpu::BindGroup,
}

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    uploader: GpuUploader,
    pipeline: wgpu::RenderPipeline,
    background_pipeline: wgpu::RenderPipeline,
    transform_layout: wgpu::BindGroupLayout,
    image_uniform: wgpu::Buffer,
    image_transform: wgpu::BindGroup,
    reference_uniform: wgpu::Buffer,
    reference_transform: wgpu::BindGroup,
    status_uniform: wgpu::Buffer,
    status_transform: wgpu::BindGroup,
    message_uniform: wgpu::Buffer,
    message_transform: wgpu::BindGroup,
    comparison_labels: [Caption; 2],
    filmstrip: filmstrip::Filmstrip,
    image: Option<Arc<PreparedTexture>>,
    reference: Option<(Arc<PreparedTexture>, u16)>,
    orientation: u16,
    viewport: Viewport,
    preserve_view: bool,
    brightness_stops: f32,
    brightness_multiplier: f32,
    peek_restore: Option<(Viewport, u16)>,
    status: String,
    message: String,
    status_texture: Option<TextTexture>,
    message_texture: Option<TextTexture>,
    text_dirty: bool,
    size: PhysicalSize<u32>,
    consecutive_timeouts: u8,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self, String> {
        pollster::block_on(Self::initialize(window))
    }

    async fn initialize(window: Arc<Window>) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|e| e.to_string())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or("no compatible Metal graphics adapter is available")?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("FastCull Metal device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                },
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        let errors = Arc::new(Mutex::new(None));
        let uncaptured_errors = Arc::clone(&errors);
        device.on_uncaptured_error(Box::new(move |error| {
            let error = format!("FastCull GPU: {error}");
            eprintln!("{error}");
            *uncaptured_errors.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
        }));
        let lost_errors = Arc::clone(&errors);
        device.set_device_lost_callback(move |reason, message| {
            if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
                return;
            }
            let error = format!("Metal device lost ({reason:?}): {message}");
            eprintln!("{error}");
            *lost_errors.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
        });
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let size = window.inner_size();
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| capabilities.formats.first().copied())
            .ok_or("Metal surface exposes no texture format")?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: if capabilities
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::Opaque)
            {
                wgpu::CompositeAlphaMode::Opaque
            } else {
                capabilities
                    .alpha_modes
                    .first()
                    .copied()
                    .ok_or("no surface alpha mode")?
            },
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        let layout = Arc::new(
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("JPEG texture layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            }),
        );
        let transform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewport transform layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(48),
                },
                count: None,
            }],
        });
        let sampler = Arc::new(device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("photo sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        }));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("photo and text shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("photo pipeline layout"),
            bind_group_layouts: &[&layout, &transform_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("photo pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let background_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("opaque background layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let background_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("opaque background"),
            layout: Some(&background_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_background"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_background"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let (image_uniform, image_transform) = transform_binding(&device, &transform_layout);
        let (reference_uniform, reference_transform) =
            transform_binding(&device, &transform_layout);
        let (status_uniform, status_transform) = transform_binding(&device, &transform_layout);
        let (message_uniform, message_transform) = transform_binding(&device, &transform_layout);
        let uploader = GpuUploader {
            device: Arc::clone(&device),
            queue: Arc::clone(&queue),
            layout,
            sampler,
            errors,
        };
        let comparison_labels = ["REFERENCE", "CURRENT"].map(|text| {
            let (uniform, transform) = transform_binding(&device, &transform_layout);
            Caption {
                text: text_texture(&uploader, text, 192, false),
                uniform,
                transform,
            }
        });
        uploader.check_error()?;
        let mut renderer = Self {
            window,
            surface,
            config,
            device,
            queue,
            uploader,
            pipeline,
            background_pipeline,
            transform_layout,
            image_uniform,
            image_transform,
            reference_uniform,
            reference_transform,
            status_uniform,
            status_transform,
            message_uniform,
            message_transform,
            comparison_labels,
            filmstrip: filmstrip::Filmstrip::new(),
            image: None,
            reference: None,
            orientation: 1,
            viewport: Viewport::default(),
            preserve_view: true,
            brightness_stops: 0.0,
            brightness_multiplier: 1.0,
            peek_restore: None,
            status: "FastCull".into(),
            message: "Cmd+O to open a folder".into(),
            status_texture: None,
            message_texture: None,
            text_dirty: true,
            size,
            consecutive_timeouts: 0,
        };
        renderer.update_filmstrip_layout();
        renderer.update_geometry();
        Ok(renderer)
    }

    pub fn uploader(&self) -> GpuUploader {
        self.uploader.clone()
    }

    pub fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    pub fn set_preserve_view(&mut self, preserve: bool) {
        self.preserve_view = preserve;
    }

    pub fn preserve_view(&self) -> bool {
        self.preserve_view
    }

    /// Display-only brightness, shared by every photo and retained across
    /// navigation. Existing decoded pixels, textures, and sidecars are untouched.
    pub fn set_brightness_stops(&mut self, stops: f32) {
        let stops = bounded_brightness_stops(stops);
        if self.brightness_stops == stops {
            return;
        }
        self.brightness_stops = stops;
        self.brightness_multiplier = stops.exp2();
        self.filmstrip
            .set_brightness_multiplier(&self.queue, self.brightness_multiplier);
    }

    pub fn brightness_stops(&self) -> f32 {
        self.brightness_stops
    }

    /// The strip is opt-in; hiding it releases all retained thumbnail handles.
    pub fn set_filmstrip_visible(&mut self, visible: bool) {
        if visible == self.filmstrip.visible() {
            return;
        }
        self.end_peek();
        self.filmstrip.set_visible(visible);
        self.update_filmstrip_layout();
        self.update_geometry();
        self.text_dirty = true;
    }

    pub fn filmstrip_visible(&self) -> bool {
        self.filmstrip.visible()
    }

    /// Number of visible cells, capped at 24. Coordinates are physical pixels.
    pub fn filmstrip_capacity(&self) -> usize {
        self.filmstrip.capacity()
    }

    pub fn set_filmstrip(&mut self, items: Vec<FilmstripItem>) {
        self.filmstrip.set_items(items);
        self.update_filmstrip_layout();
    }

    pub fn filmstrip_hit(&self, point: [f64; 2]) -> Option<usize> {
        self.filmstrip.hit(point)
    }

    pub fn over_filmstrip(&self, point: [f64; 2]) -> bool {
        self.filmstrip.contains(point)
    }

    fn update_filmstrip_layout(&mut self) {
        self.filmstrip.resize(
            [self.config.width, self.config.height],
            self.bar_height(),
            self.window.scale_factor(),
        );
    }

    /// The reference stays at left; the current image stays at right. This only
    /// retains the existing counted GPU texture; it never uploads or decodes.
    pub fn set_reference(
        &mut self,
        reference: Option<Arc<PreparedTexture>>,
        orientation: Option<u16>,
    ) {
        self.end_peek();
        self.reference = reference.map(|image| {
            let orientation = valid_orientation(orientation.unwrap_or(image.orientation));
            (image, orientation)
        });
        self.update_geometry();
        self.text_dirty = true;
    }

    pub fn set_reference_orientation(&mut self, orientation: u16) {
        if let Some((_, current)) = &mut self.reference {
            *current = valid_orientation(orientation);
        }
    }

    pub fn is_peeking(&self) -> bool {
        self.peek_restore.is_some()
    }

    pub fn begin_peek(&mut self, cursor: [f64; 2]) -> bool {
        if self.image.is_none()
            || self.is_peeking()
            || self.over_filmstrip(cursor)
            || !cursor.iter().all(|value| value.is_finite())
        {
            return false;
        }
        self.peek_restore = Some((self.viewport.clone(), self.orientation));
        let (mut selected, pane, orientation) = self.view_at_cursor(cursor);
        selected.actual_size_at(pane.local_cursor(cursor));
        self.adopt_view(selected, orientation);
        true
    }

    /// End on key release, lost focus, or navigation. Geometry may have changed
    /// while held, so restore the previous normalized point in the current pane.
    pub fn end_peek(&mut self) -> bool {
        let Some((saved, orientation)) = self.peek_restore.take() else {
            return false;
        };
        if orientation == self.orientation
            && saved.image_size() == self.current_dimensions()
            && saved.view_size() == self.current_pane_size()
        {
            // The ordinary press/release path restores the exact snapshot,
            // avoiding floating-point drift from a coordinate round trip.
            self.viewport = saved;
        } else {
            self.adopt_view(saved, orientation);
        }
        true
    }

    pub fn zoom(&mut self, factor: f64, cursor: Option<[f64; 2]>) {
        if cursor.is_some_and(|point| self.over_filmstrip(point)) {
            return;
        }
        if let Some(cursor) = cursor.filter(|point| point.iter().all(|value| value.is_finite())) {
            let (mut selected, pane, orientation) = self.view_at_cursor(cursor);
            selected.zoom(factor, Some(pane.local_cursor(cursor)));
            self.adopt_view(selected, orientation);
        } else {
            self.viewport.zoom(factor, None);
        }
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        self.viewport.pan(dx, dy);
    }

    pub fn pan_at(&mut self, dx: f64, dy: f64, cursor: Option<[f64; 2]>) {
        if cursor.is_some_and(|point| self.over_filmstrip(point)) {
            return;
        }
        if let Some(cursor) = cursor.filter(|point| point.iter().all(|value| value.is_finite())) {
            let (mut selected, _, orientation) = self.view_at_cursor(cursor);
            selected.pan(dx, dy);
            self.adopt_view(selected, orientation);
        } else {
            self.pan(dx, dy);
        }
    }

    fn view_at_cursor(&self, cursor: [f64; 2]) -> (Viewport, viewport::Pane, u16) {
        let (reference_pane, current_pane) = self.panes();
        if let (Some(pane), Some((_, orientation))) = (reference_pane, &self.reference) {
            if cursor[0] < f64::from(current_pane.x) {
                if let Some(view) = self.reference_viewport(pane) {
                    return (view, pane, *orientation);
                }
            }
        }
        (self.viewport.clone(), current_pane, self.orientation)
    }

    fn adopt_view(&mut self, view: Viewport, orientation: u16) {
        let focus = viewport::stored_focus(view.normalized_focus(), orientation);
        self.viewport = view.for_geometry(self.current_dimensions(), self.current_pane_size());
        self.viewport
            .set_normalized_focus(viewport::displayed_focus(focus, self.orientation));
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        self.size = size;
        if size.width > 0 && size.height > 0 {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
        self.text_dirty = true;
        self.update_filmstrip_layout();
        self.update_geometry();
    }

    pub fn set_image(&mut self, image: Option<Arc<PreparedTexture>>) {
        let orientation = image
            .as_ref()
            .map_or(self.orientation, |image| image.orientation);
        self.set_image_with_orientation(image, orientation);
    }

    /// Install a photo and its known EXIF orientation in one view transition.
    /// No intermediate unrotated geometry may clamp away the user's focal point.
    pub fn set_image_with_orientation(
        &mut self,
        image: Option<Arc<PreparedTexture>>,
        orientation: u16,
    ) {
        let unchanged = match (&self.image, &image) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            if image.is_some() {
                self.set_orientation(orientation);
            }
            return;
        }
        self.end_peek();
        if let Some(image) = &image {
            let orientation = valid_orientation(orientation);
            self.viewport.change_image(
                [image.original_width, image.original_height],
                self.current_pane_size(),
                self.orientation,
                orientation,
                self.preserve_view,
            );
            self.orientation = orientation;
        } else {
            // A pressure-related texture release retains the previous image's
            // geometry until its replacement arrives.
            if !self.preserve_view {
                self.viewport.fit();
            }
            self.update_geometry();
        }
        self.image = image;
    }

    pub fn set_orientation(&mut self, orientation: u16) {
        let orientation = valid_orientation(orientation);
        if orientation == self.orientation {
            return;
        }
        if let Some(image) = &self.image {
            self.viewport.change_image(
                [image.original_width, image.original_height],
                self.current_pane_size(),
                self.orientation,
                orientation,
                true,
            );
            self.orientation = orientation;
        }
    }

    pub fn set_status(&mut self, status: &str) {
        if self.status != status {
            self.status = status.chars().take(512).collect();
            self.text_dirty = true;
        }
    }

    pub fn set_message(&mut self, message: &str) {
        if self.message != message {
            self.message = message.chars().take(256).collect();
            self.text_dirty = true;
        }
    }

    fn bar_height(&self) -> u32 {
        STATUS_HEIGHT.min(self.config.height.saturating_sub(1))
    }

    fn panes(&self) -> (Option<viewport::Pane>, viewport::Pane) {
        viewport::image_panes(
            [self.config.width, self.config.height],
            self.bar_height() + self.filmstrip.height(),
            self.reference.is_some(),
        )
    }

    fn current_pane_size(&self) -> [u32; 2] {
        let pane = self.panes().1;
        [pane.width, pane.height]
    }

    fn current_dimensions(&self) -> [u32; 2] {
        self.image
            .as_ref()
            .map_or(self.viewport.image_size(), |image| {
                viewport::oriented_size(
                    image.original_width,
                    image.original_height,
                    self.orientation,
                )
            })
    }

    fn update_geometry(&mut self) {
        self.viewport
            .set_geometry_preserving_focus(self.current_dimensions(), self.current_pane_size());
    }

    fn reference_viewport(&self, pane: viewport::Pane) -> Option<Viewport> {
        let (image, orientation) = self.reference.as_ref()?;
        let dimensions =
            viewport::oriented_size(image.original_width, image.original_height, *orientation);
        let mut reference = self
            .viewport
            .for_geometry(dimensions, [pane.width, pane.height]);
        let focus = viewport::stored_focus(self.viewport.normalized_focus(), self.orientation);
        reference.set_normalized_focus(viewport::displayed_focus(focus, *orientation));
        Some(reference)
    }

    /// Returns true only when a frame was submitted for presentation.
    pub fn render(&mut self) -> Result<bool, String> {
        self.uploader.check_error()?;
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(false);
        }
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return Ok(false);
            }
            Err(wgpu::SurfaceError::Timeout) => {
                self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
                if self.consecutive_timeouts >= 3 {
                    return Err("Metal surface repeatedly timed out".into());
                }
                self.window.request_redraw();
                return Ok(false);
            }
            Err(error) => return Err(format!("Metal surface: {error}")),
        };
        self.consecutive_timeouts = 0;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.draw_to_view(&view);
        self.window.pre_present_notify();
        frame.present();
        // A nonblocking poll releases completed frame references. Blocking
        // device waits are reserved for the explicit capture diagnostic.
        let _ = self.device.poll(wgpu::Maintain::Poll);
        self.uploader.check_error()?;
        Ok(true)
    }

    fn update_text(&mut self) {
        if !self.text_dirty {
            return;
        }
        let width = self
            .config
            .width
            .min(self.device.limits().max_texture_dimension_2d)
            .max(1);
        self.status_texture = Some(text_texture(&self.uploader, &self.status, width, true));
        self.message_texture = Some(text_texture(
            &self.uploader,
            &self.message,
            self.panes().1.width.min(width),
            false,
        ));
        self.text_dirty = false;
    }

    fn draw_to_view(&mut self, view: &wgpu::TextureView) {
        self.update_text();
        let width = self.config.width;
        let height = self.config.height;
        let bar = self.bar_height();
        self.filmstrip
            .update(&self.uploader, &self.transform_layout, [width, height]);
        let (reference_pane, current_pane) = self.panes();
        let image_placement = self.viewport.transform_in([width, height], current_pane);
        self.queue.write_buffer(
            &self.image_uniform,
            0,
            &photo_transform_bytes(
                image_placement,
                self.orientation,
                self.brightness_multiplier,
            ),
        );
        if let (Some(pane), Some((_, orientation))) = (reference_pane, &self.reference) {
            if let Some(viewport) = self.reference_viewport(pane) {
                self.queue.write_buffer(
                    &self.reference_uniform,
                    0,
                    &photo_transform_bytes(
                        viewport.transform_in([width, height], pane),
                        *orientation,
                        self.brightness_multiplier,
                    ),
                );
            }
        }
        if let Some(pane) = reference_pane {
            for (pane, caption) in [pane, current_pane]
                .into_iter()
                .zip(&self.comparison_labels)
            {
                self.queue.write_buffer(
                    &caption.uniform,
                    0,
                    &transform_bytes(
                        [
                            caption.text.width as f32 / width as f32,
                            caption.text.height as f32 / height as f32,
                            ((pane.x + 8) * 2 + caption.text.width) as f32 / width as f32 - 1.0,
                            1.0 - ((pane.y + 8) * 2 + caption.text.height) as f32 / height as f32,
                        ],
                        1,
                    ),
                );
            }
        }
        if let Some(status) = &self.status_texture {
            self.queue.write_buffer(
                &self.status_uniform,
                0,
                &transform_bytes(
                    [
                        status.width as f32 / width as f32,
                        bar as f32 / height as f32,
                        status.width as f32 / width as f32 - 1.0,
                        -1.0 + bar as f32 / height as f32,
                    ],
                    1,
                ),
            );
        }
        if let Some(message) = &self.message_texture {
            self.queue.write_buffer(
                &self.message_uniform,
                0,
                &transform_bytes(
                    [
                        message.width as f32 / width as f32,
                        message.height as f32 / height as f32,
                        (current_pane.x * 2 + current_pane.width) as f32 / width as f32 - 1.0,
                        1.0 - (current_pane.y * 2 + current_pane.height) as f32 / height as f32,
                    ],
                    1,
                ),
            );
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("FastCull frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("photo and status"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Write every drawable pixel through a fragment shader. Leaving
            // margins to the attachment fast-clear alone can expose patterned
            // red artifacts on Intel Metal drawables during zoom/navigation.
            // One oversized triangle covers the surface without a diagonal seam.
            pass.set_pipeline(&self.background_pipeline);
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, width, height);
            pass.draw(0..3, 0..1);
            pass.set_pipeline(&self.pipeline);
            if let (Some(pane), Some((reference, _))) = (reference_pane, &self.reference) {
                pass.set_scissor_rect(pane.x, pane.y, pane.width, pane.height);
                pass.set_bind_group(0, &reference.bind_group, &[]);
                pass.set_bind_group(1, &self.reference_transform, &[]);
                pass.draw(0..6, 0..1);
            }
            pass.set_scissor_rect(
                current_pane.x,
                current_pane.y,
                current_pane.width,
                current_pane.height,
            );
            if let Some(image) = &self.image {
                pass.set_bind_group(0, &image.bind_group, &[]);
                pass.set_bind_group(1, &self.image_transform, &[]);
                pass.draw(0..6, 0..1);
            } else if let Some(message) = &self.message_texture {
                pass.set_bind_group(0, &message.binding, &[]);
                pass.set_bind_group(1, &self.message_transform, &[]);
                pass.draw(0..6, 0..1);
            }
            if let Some(pane) = reference_pane {
                for (pane, caption) in [pane, current_pane]
                    .into_iter()
                    .zip(&self.comparison_labels)
                {
                    pass.set_scissor_rect(pane.x, pane.y, pane.width, pane.height);
                    pass.set_bind_group(0, &caption.text.binding, &[]);
                    pass.set_bind_group(1, &caption.transform, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
            self.filmstrip.draw(&mut pass);
            if bar > 0 {
                pass.set_scissor_rect(0, height - bar, width, bar);
                if let Some(status) = &self.status_texture {
                    pass.set_bind_group(0, &status.binding, &[]);
                    pass.set_bind_group(1, &self.status_transform, &[]);
                    pass.draw(0..6, 0..1);
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        let images = (
            self.image.clone(),
            self.reference.as_ref().map(|(image, _)| Arc::clone(image)),
            self.filmstrip.retained_textures(),
        );
        if images.0.is_some() || images.1.is_some() || images.2.iter().any(Option::is_some) {
            // Every displayed texture keeps its counted lease through GPU use.
            self.queue.on_submitted_work_done(move || drop(images));
        }
    }

    /// Blocking, explicit diagnostic only. Captures this renderer's own image
    /// into a new PPM file; never captures the desktop or other applications.
    pub fn capture(&mut self, path: &Path) -> Result<(), String> {
        self.uploader.check_error()?;
        let width = self.config.width;
        let height = self.config.height;
        let row = width.checked_mul(4).ok_or("capture row overflow")?;
        let padded_row = row
            .checked_add(255)
            .map(|n| n / 256 * 256)
            .ok_or("capture padding overflow")?;
        let bytes = u64::from(padded_row)
            .checked_mul(u64::from(height))
            .filter(|n| *n <= 256 * 1024 * 1024)
            .ok_or("capture exceeds 256 MiB")?;
        if !matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm
                | wgpu::TextureFormat::Bgra8UnormSrgb
                | wgpu::TextureFormat::Rgba8Unorm
                | wgpu::TextureFormat::Rgba8UnormSrgb
        ) {
            return Err("unsupported capture surface format".into());
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("explicit capture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.draw_to_view(&view);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("capture copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            texture.size(),
        );
        self.queue.submit([encoder.finish()]);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = sender.send(result);
            });
        let _ = self.device.poll(wgpu::Maintain::Wait);
        self.uploader.check_error()?;
        receiver
            .recv()
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let mapped = buffer.slice(..).get_mapped_range();
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        write!(output, "P6\n{width} {height}\n255\n").map_err(|e| e.to_string())?;
        let bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut rgb = Vec::with_capacity(width as usize * 3);
        for padded in mapped
            .chunks_exact(padded_row as usize)
            .take(height as usize)
        {
            rgb.clear();
            for pixel in padded[..row as usize].chunks_exact(4) {
                if bgra {
                    rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                } else {
                    rgb.extend_from_slice(&pixel[..3]);
                }
            }
            output.write_all(&rgb).map_err(|e| e.to_string())?;
        }
        drop(mapped);
        buffer.unmap();
        Ok(())
    }
}

fn valid_orientation(orientation: u16) -> u16 {
    if (1..=8).contains(&orientation) {
        orientation
    } else {
        1
    }
}

fn transform_binding(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewport uniform"),
        size: 48,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let binding = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewport binding"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    (buffer, binding)
}

fn transform_bytes(placement: [f32; 4], orientation: u16) -> [u8; 48] {
    photo_transform_bytes(placement, orientation, 1.0)
}

fn bounded_brightness_stops(stops: f32) -> f32 {
    if stops.is_finite() {
        stops.clamp(-3.0, 3.0)
    } else {
        0.0
    }
}

fn photo_transform_bytes(
    placement: [f32; 4],
    orientation: u16,
    brightness_multiplier: f32,
) -> [u8; 48] {
    let mut rows = viewport::orientation_rows(orientation);
    rows[0][3] = brightness_multiplier;
    let mut bytes = [0u8; 48];
    for (slot, value) in bytes
        .chunks_exact_mut(4)
        .zip(placement.into_iter().chain(rows[0]).chain(rows[1]))
    {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn text_texture(
    uploader: &GpuUploader,
    text: &str,
    max_width: u32,
    full_width: bool,
) -> TextTexture {
    let max_characters = (max_width.saturating_sub(16) / (8 * FONT_SCALE)) as usize;
    let characters: Vec<char> = text.chars().take(max_characters.min(512)).collect();
    let width = if full_width {
        max_width
    } else {
        (characters.len() as u32 * 8 * FONT_SCALE + 16)
            .min(max_width)
            .max(1)
    };
    let height = STATUS_HEIGHT;
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[22, 22, 24, 255]);
    }
    for (character_index, character) in characters.into_iter().enumerate() {
        let glyph = if character == '★' {
            Some([0x18, 0x18, 0xff, 0x7e, 0x3c, 0x7e, 0x66, 0x42])
        } else if character == '×' {
            font8x8::BASIC_FONTS.get('x')
        } else {
            font8x8::BASIC_FONTS
                .get(character)
                .or_else(|| font8x8::BASIC_FONTS.get('?'))
        };
        if let Some(glyph) = glyph {
            for (glyph_y, bits) in glyph.into_iter().enumerate() {
                for glyph_x in 0..8 {
                    if bits & (1 << glyph_x) == 0 {
                        continue;
                    }
                    for sy in 0..FONT_SCALE {
                        for sx in 0..FONT_SCALE {
                            let x = 8
                                + character_index as u32 * 8 * FONT_SCALE
                                + glyph_x * FONT_SCALE
                                + sx;
                            let y = 8 + glyph_y as u32 * FONT_SCALE + sy;
                            if x < width && y < height {
                                let index = (y as usize * width as usize + x as usize) * 4;
                                rgba[index..index + 4].copy_from_slice(&[218, 218, 220, 255]);
                            }
                        }
                    }
                }
            }
        }
    }
    let texture = uploader.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("status text"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
        &rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let binding =
        texture::image_binding(&uploader.device, &uploader.layout, &uploader.sampler, &view);
    TextTexture {
        _texture: texture,
        binding,
        width,
        height,
    }
}
