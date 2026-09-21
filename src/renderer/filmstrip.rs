//! Bounded, cached filmstrip drawing. All photographs arrive already uploaded.

use super::{
    text_texture, texture, transform_binding, transform_bytes, viewport, GpuUploader,
    PreparedTexture, TextTexture, Viewport,
};
use std::sync::Arc;
use viewport::Pane;

pub const MAX_ITEMS: usize = 24;

pub struct FilmstripItem {
    pub index: usize,
    pub texture: Option<Arc<PreparedTexture>>,
    pub orientation: u16,
    pub selected: bool,
    pub rating: Option<i8>,
    pub label: String,
}

impl FilmstripItem {
    fn same_visual(&self, other: &Self) -> bool {
        self.index == other.index
            && self.orientation == other.orientation
            && self.rating == other.rating
            && self.label == other.label
            && match (&self.texture, &other.texture) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
        // Selection uses an existing border texture at draw time, so it does
        // not require any buffer writes or caption uploads.
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    band: Option<Pane>,
    capacity: usize,
    count: usize,
    cell_width: u32,
    first_x: u32,
    gap: u32,
    border: u32,
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    frame: Pane,
    preview: Pane,
    caption: Pane,
}

impl Layout {
    fn new(surface: [u32; 2], status: u32, scale: f64, visible: bool, count: usize) -> Self {
        let scale = if scale.is_finite() {
            scale.clamp(0.5, 4.0)
        } else {
            1.0
        };
        let width = surface[0].max(1);
        let capacity = (width / (120.0 * scale).round() as u32).clamp(1, MAX_ITEMS as u32) as usize;
        let available = surface[1].saturating_sub(status);
        let height = ((100.0 * scale).round() as u32).min(available / 3);
        let band = (visible && height > 0).then_some(Pane {
            x: 0,
            y: available.saturating_sub(height),
            width,
            height,
        });
        let count = count.min(capacity);
        let cell_width = width / capacity as u32;
        Self {
            band,
            capacity,
            count,
            cell_width,
            first_x: (width - cell_width * count as u32) / 2,
            gap: (4.0 * scale).round() as u32,
            border: (2.0 * scale).round() as u32,
        }
    }

    fn cell(self, index: usize) -> Option<Cell> {
        let band = self.band?;
        if index >= self.count {
            return None;
        }
        let inset_x = self.gap.min(self.cell_width.saturating_sub(1) / 2);
        let inset_y = self.gap.min(band.height.saturating_sub(1) / 2);
        let frame = Pane {
            x: self.first_x + index as u32 * self.cell_width + inset_x,
            y: band.y + inset_y,
            width: self.cell_width - 2 * inset_x,
            height: band.height - 2 * inset_y,
        };
        let border = self
            .border
            .min(frame.width.saturating_sub(1) / 2)
            .min(frame.height.saturating_sub(1) / 2);
        let inner = Pane {
            x: frame.x + border,
            y: frame.y + border,
            width: frame.width - 2 * border,
            height: frame.height - 2 * border,
        };
        let caption_height = super::STATUS_HEIGHT.min(inner.height / 3);
        let preview = Pane {
            height: inner.height - caption_height,
            ..inner
        };
        let caption = Pane {
            y: inner.y + preview.height,
            height: caption_height,
            ..inner
        };
        Some(Cell {
            frame,
            preview,
            caption,
        })
    }

    fn hit(self, point: [f64; 2]) -> Option<usize> {
        (0..self.count).find(|&index| {
            self.cell(index)
                .is_some_and(|cell| contains(cell.frame, point))
        })
    }
}

fn contains(pane: Pane, point: [f64; 2]) -> bool {
    point[0] >= f64::from(pane.x)
        && point[0] < f64::from(pane.x + pane.width)
        && point[1] >= f64::from(pane.y)
        && point[1] < f64::from(pane.y + pane.height)
}

fn caption_label(filename: &str, rating: Option<i8>, width: u32) -> String {
    let capacity = (width.saturating_sub(16) / (8 * super::FONT_SCALE)) as usize;
    let prefix = match rating {
        Some(-1) => "X ".to_owned(),
        Some(rating @ 1..=5) => format!("{rating}★ "),
        _ => String::new(),
    };
    let prefix_len = prefix.chars().count();
    if prefix_len + filename.chars().count() <= capacity {
        return format!("{prefix}{filename}");
    }
    // Retain distinguishing sequence digits in narrow 1x cells. The extension
    // adds no useful information in an ARW-only strip, so remove it first.
    let stem = filename
        .rsplit_once('.')
        .filter(|(_, extension)| extension.eq_ignore_ascii_case("arw"))
        .map_or(filename, |(stem, _)| stem);
    let room = capacity.saturating_sub(prefix_len);
    let name: Vec<_> = stem.chars().collect();
    let suffix: String = if name.len() <= room {
        name.into_iter().collect()
    } else if room <= 1 {
        name.into_iter().rev().take(room).collect()
    } else {
        std::iter::once('~')
            .chain(name[name.len() - (room - 1)..].iter().copied())
            .collect()
    };
    prefix
        .chars()
        .take(capacity)
        .chain(suffix.chars())
        .collect()
}

struct Quad {
    uniform: wgpu::Buffer,
    transform: wgpu::BindGroup,
    placement: CachedPlacement,
}

#[derive(Default)]
struct CachedPlacement(Option<([f32; 4], u16)>);

impl CachedPlacement {
    fn changed(&mut self, placement: [f32; 4], orientation: u16) -> bool {
        let value = (placement, orientation);
        if self.0 == Some(value) {
            return false;
        }
        self.0 = Some(value);
        true
    }
}

impl Quad {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout) -> Self {
        let (uniform, transform) = transform_binding(device, layout);
        Self {
            uniform,
            transform,
            placement: CachedPlacement::default(),
        }
    }

    fn place(&mut self, queue: &wgpu::Queue, surface: [u32; 2], pane: Pane) {
        self.write(
            queue,
            [
                pane.width as f32 / surface[0] as f32,
                pane.height as f32 / surface[1] as f32,
                (2.0 * pane.x as f32 + pane.width as f32) / surface[0] as f32 - 1.0,
                1.0 - (2.0 * pane.y as f32 + pane.height as f32) / surface[1] as f32,
            ],
            1,
        );
    }

    fn write(&mut self, queue: &wgpu::Queue, placement: [f32; 4], orientation: u16) {
        if self.placement.changed(placement, orientation) {
            queue.write_buffer(&self.uniform, 0, &transform_bytes(placement, orientation));
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, texture: &wgpu::BindGroup) {
        pass.set_bind_group(0, texture, &[]);
        pass.set_bind_group(1, &self.transform, &[]);
        pass.draw(0..6, 0..1);
    }
}

struct Slot {
    frame: Quad,
    preview: Quad,
    photo: Quad,
    caption: Quad,
    label_texture: Option<Arc<TextTexture>>,
}

#[derive(Clone, PartialEq, Eq)]
struct CaptionKey {
    text: String,
    width: u32,
}

// Captions belong to photographs, not positions in the strip. Retain all
// overlapping captions before allocating new ones, including on reverse
// scrolling. This is bounded by the visible slots, with no extra GPU history.
struct CaptionCache<T> {
    entries: Vec<(CaptionKey, Arc<T>)>,
}

impl<T> CaptionCache<T> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn update(
        &mut self,
        keys: &[CaptionKey],
        mut create: impl FnMut(&CaptionKey) -> T,
    ) -> Vec<Arc<T>> {
        let keys = &keys[..keys.len().min(MAX_ITEMS)];
        self.entries.retain(|(key, _)| keys.contains(key));
        keys.iter()
            .map(|key| {
                if let Some((_, texture)) = self.entries.iter().find(|(stored, _)| stored == key) {
                    return Arc::clone(texture);
                }
                let texture = Arc::new(create(key));
                self.entries.push((key.clone(), Arc::clone(&texture)));
                texture
            })
            .collect()
    }
}

struct Drawing {
    background: Quad,
    // Opaque one-pixel textures are created once, not once per thumbnail/frame.
    colors: [TextTexture; 4],
    slots: Vec<Slot>,
    captions: CaptionCache<TextTexture>,
}

pub(super) struct Filmstrip {
    visible: bool,
    items: Vec<FilmstripItem>,
    layout: Layout,
    drawing: Option<Drawing>,
    dirty: bool,
}

impl Filmstrip {
    pub fn new() -> Self {
        Self {
            visible: false,
            items: Vec::new(),
            layout: Layout::new([1, 1], 0, 1.0, false, 0),
            drawing: None,
            dirty: true,
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !visible {
            self.items.clear();
        }
        self.dirty = true;
    }

    pub fn set_items(&mut self, mut items: Vec<FilmstripItem>) {
        if !self.visible {
            return;
        }
        items.truncate(MAX_ITEMS);
        for item in &mut items {
            item.orientation = super::valid_orientation(item.orientation);
            if item.label.len() > 96 {
                if let Some((end, _)) = item.label.char_indices().nth(96) {
                    item.label.truncate(end);
                }
            }
        }
        self.dirty |= self.items.len() != items.len()
            || self
                .items
                .iter()
                .zip(&items)
                .any(|(previous, next)| !previous.same_visual(next));
        self.items = items;
    }

    pub fn resize(&mut self, surface: [u32; 2], status: u32, scale: f64) {
        let layout = Layout::new(surface, status, scale, self.visible, self.items.len());
        if layout != self.layout {
            self.layout = layout;
            self.dirty = true;
        }
    }

    pub fn capacity(&self) -> usize {
        self.layout.capacity
    }
    pub fn height(&self) -> u32 {
        self.layout.band.map_or(0, |band| band.height)
    }
    pub fn hit(&self, point: [f64; 2]) -> Option<usize> {
        self.layout
            .hit(point)
            .and_then(|slot| self.items.get(slot).map(|item| item.index))
    }
    pub fn contains(&self, point: [f64; 2]) -> bool {
        self.layout.band.is_some_and(|band| contains(band, point))
    }

    pub fn retained_textures(&self) -> [Option<Arc<PreparedTexture>>; MAX_ITEMS] {
        std::array::from_fn(|slot| {
            self.layout
                .cell(slot)
                .and_then(|_| self.items[slot].texture.clone())
        })
    }

    pub fn update(
        &mut self,
        uploader: &GpuUploader,
        transform_layout: &wgpu::BindGroupLayout,
        surface: [u32; 2],
    ) {
        let Some(band) = self.layout.band else {
            return;
        };
        if !self.dirty {
            return;
        }
        let drawing = self.drawing.get_or_insert_with(|| Drawing {
            background: Quad::new(&uploader.device, transform_layout),
            colors: [
                [22, 22, 24, 255],
                [55, 55, 59, 255],
                [93, 173, 255, 255],
                [37, 37, 41, 255],
            ]
            .map(|color| solid_texture(uploader, color)),
            slots: (0..MAX_ITEMS)
                .map(|_| Slot {
                    frame: Quad::new(&uploader.device, transform_layout),
                    preview: Quad::new(&uploader.device, transform_layout),
                    photo: Quad::new(&uploader.device, transform_layout),
                    caption: Quad::new(&uploader.device, transform_layout),
                    label_texture: None,
                })
                .collect(),
            captions: CaptionCache::new(),
        });
        let captions: Vec<_> = self
            .items
            .iter()
            .take(self.layout.count)
            .enumerate()
            .filter_map(|(index, item)| {
                self.layout.cell(index).map(|cell| CaptionKey {
                    text: caption_label(&item.label, item.rating, cell.caption.width),
                    width: cell.caption.width,
                })
            })
            .collect();
        let labels = drawing.captions.update(&captions, |key| {
            text_texture(uploader, &key.text, key.width, true)
        });
        drawing.background.place(&uploader.queue, surface, band);
        for (index, item) in self.items.iter().take(self.layout.count).enumerate() {
            let Some(cell) = self.layout.cell(index) else {
                continue;
            };
            let slot = &mut drawing.slots[index];
            slot.frame.place(&uploader.queue, surface, cell.frame);
            slot.preview.place(&uploader.queue, surface, cell.preview);
            slot.caption.place(&uploader.queue, surface, cell.caption);
            if let Some(image) = &item.texture {
                let mut view = Viewport::default();
                view.set_geometry(
                    viewport::oriented_size(image.width, image.height, item.orientation),
                    [cell.preview.width, cell.preview.height],
                );
                slot.photo.write(
                    &uploader.queue,
                    view.transform_in(surface, cell.preview),
                    item.orientation,
                );
            }
            slot.label_texture = Some(Arc::clone(&labels[index]));
        }
        for slot in drawing.slots.iter_mut().skip(self.layout.count) {
            slot.label_texture = None;
        }
        self.dirty = false;
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let (Some(band), Some(drawing)) = (self.layout.band, &self.drawing) else {
            return;
        };
        pass.set_scissor_rect(band.x, band.y, band.width, band.height);
        drawing.background.draw(pass, &drawing.colors[0].binding);
        for (index, item) in self.items.iter().take(self.layout.count).enumerate() {
            let Some(cell) = self.layout.cell(index) else {
                continue;
            };
            let slot = &drawing.slots[index];
            slot.frame.draw(
                pass,
                &drawing.colors[if item.selected { 2 } else { 1 }].binding,
            );
            slot.preview.draw(pass, &drawing.colors[3].binding);
            if let Some(image) = &item.texture {
                slot.photo.draw(pass, &image.bind_group);
            }
            if cell.caption.height > 0 {
                if let Some(label) = &slot.label_texture {
                    slot.caption.draw(pass, &label.binding);
                }
            }
        }
    }
}

fn solid_texture(uploader: &GpuUploader, rgba: [u8; 4]) -> TextTexture {
    let texture = uploader.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("filmstrip color"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
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
            bytes_per_row: Some(4),
            rows_per_image: Some(1),
        },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let binding =
        texture::image_binding(&uploader.device, &uploader.layout, &uploader.sampler, &view);
    TextTexture {
        _texture: texture,
        binding,
        width: 1,
        height: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize, selected: bool) -> FilmstripItem {
        FilmstripItem {
            index,
            texture: None,
            orientation: 1,
            selected,
            rating: None,
            label: format!("DSC{index:05}.ARW"),
        }
    }

    fn caption(text: &str, width: u32) -> CaptionKey {
        CaptionKey {
            text: text.to_owned(),
            width,
        }
    }

    #[test]
    fn selection_changes_keep_existing_gpu_geometry_and_captions() {
        let mut strip = Filmstrip::new();
        strip.set_visible(true);
        strip.set_items(vec![item(10, true), item(11, false)]);
        strip.dirty = false; // Existing slots have already been prepared.
        strip.set_items(vec![item(10, false), item(11, true)]);
        assert!(!strip.dirty);
        assert!(!strip.items[0].selected);
        assert!(strip.items[1].selected);
        strip.set_items(vec![item(10, false), item(11, true)]);
        assert!(!strip.dirty);

        let mut rated = item(11, true);
        rated.rating = Some(5);
        strip.set_items(vec![item(10, false), rated]);
        assert!(strip.dirty);
    }

    #[test]
    fn geometry_cache_writes_only_for_changed_transform_or_orientation() {
        let mut cache = CachedPlacement::default();
        let transform = [0.1, 0.2, -0.8, 0.4];
        assert!(cache.changed(transform, 1));
        assert!(!cache.changed(transform, 1));
        assert!(cache.changed(transform, 6));
        assert!(!cache.changed(transform, 6));
        assert!(cache.changed([0.1, 0.2, -0.6, 0.4], 6));
    }

    #[test]
    fn overlapping_captions_survive_forward_and_reverse_scroll() {
        let mut cache = CaptionCache::new();
        let mut uploads = 0;
        let mut upload = |_: &CaptionKey| {
            uploads += 1;
            uploads
        };
        let initial = cache.update(
            &[caption("A", 100), caption("B", 100), caption("C", 100)],
            &mut upload,
        );
        let forward = cache.update(
            &[caption("B", 100), caption("C", 100), caption("D", 100)],
            &mut upload,
        );
        assert!(Arc::ptr_eq(&initial[1], &forward[0]));
        assert!(Arc::ptr_eq(&initial[2], &forward[1]));
        assert_eq!(*forward[2], 4);
        let reverse = cache.update(
            &[caption("A", 100), caption("B", 100), caption("C", 100)],
            &mut upload,
        );
        assert!(Arc::ptr_eq(&forward[0], &reverse[1]));
        assert!(Arc::ptr_eq(&forward[1], &reverse[2]));
        assert_eq!(*reverse[0], 5);
        assert_eq!(cache.entries.len(), 3);
    }

    #[test]
    fn captions_refresh_for_rating_or_width_and_release_old_textures() {
        let mut cache = CaptionCache::new();
        let original = cache.update(&[caption("DSC1", 100)], |_| ());
        let old = Arc::downgrade(&original[0]);
        drop(original);
        let rated = cache.update(&[caption("5★ DSC1", 100)], |_| ());
        assert!(old.upgrade().is_none());
        let resized = cache.update(&[caption("5★ DSC1", 120)], |_| ());
        assert!(!Arc::ptr_eq(&rated[0], &resized[0]));
        assert_eq!(cache.entries.len(), 1);
        let empty = cache.update(&[], |_| ());
        assert!(empty.is_empty());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn caption_cache_is_bounded_and_reuses_identical_rendered_text() {
        let mut cache = CaptionCache::new();
        let same = cache.update(&[caption("same", 100), caption("same", 100)], |_| ());
        assert!(Arc::ptr_eq(&same[0], &same[1]));
        assert_eq!(cache.entries.len(), 1);
        let many: Vec<_> = (0..1000)
            .map(|index| caption(&index.to_string(), 100))
            .collect();
        let prepared = cache.update(&many, |_| ());
        assert_eq!(prepared.len(), MAX_ITEMS);
        assert_eq!(cache.entries.len(), MAX_ITEMS);
    }

    #[test]
    fn narrow_captions_keep_ratings_and_trailing_sequence_digits() {
        let six_characters = 16 + 6 * 8 * super::super::FONT_SCALE;
        assert_eq!(
            caption_label("DSC00330.ARW", None, six_characters),
            "~00330"
        );
        assert_eq!(
            caption_label("DSC00330.ARW", Some(5), six_characters),
            "5★ ~30"
        );
        assert_eq!(
            caption_label("DSC00331.arw", Some(-1), six_characters),
            "X ~331"
        );
        assert_eq!(
            caption_label("DSC00330.ARW", Some(2), 256),
            "2★ DSC00330.ARW"
        );
        assert_eq!(caption_label("DSC00330.ARW", Some(2), 1), "");
    }

    #[test]
    fn retina_layout_reserves_band_and_keeps_both_photo_panes_above_it() {
        let layout = Layout::new([2880, 1800], 32, 2.0, true, 99);
        assert_eq!(layout.capacity, 12);
        assert_eq!(layout.count, 12);
        let band = layout.band.unwrap();
        assert_eq!(
            band,
            Pane {
                x: 0,
                y: 1568,
                width: 2880,
                height: 200
            }
        );
        let (reference, current) = viewport::image_panes([2880, 1800], 32 + band.height, true);
        assert_eq!(current.height, band.y);
        assert_eq!(reference.unwrap().height, band.y);
    }

    #[test]
    fn hit_testing_excludes_gaps_status_photo_and_nonfinite_coordinates() {
        let layout = Layout::new([1200, 800], 32, 1.0, true, 3);
        for index in 0..3 {
            let cell = layout.cell(index).unwrap();
            assert_eq!(
                layout.hit([f64::from(cell.frame.x), f64::from(cell.frame.y)]),
                Some(index)
            );
            assert_eq!(
                layout.hit([
                    f64::from(cell.frame.x + cell.frame.width),
                    f64::from(cell.frame.y)
                ]),
                None
            );
        }
        for point in [
            [0.0, 700.0],
            [600.0, 100.0],
            [600.0, 790.0],
            [f64::NAN, 700.0],
            [600.0, f64::INFINITY],
        ] {
            assert_eq!(layout.hit(point), None);
        }
        assert!(contains(layout.band.unwrap(), [0.0, 700.0]));
    }

    #[test]
    fn tiny_and_extreme_surfaces_have_bounded_nonempty_scissors() {
        for width in [1, 2, 3, 100, 16384] {
            for height in [1, 2, 3, 32, 33, 40, 400] {
                for scale in [0.0, 1.0, 2.0, f64::NAN, f64::INFINITY] {
                    let status = 32.min(height - 1);
                    let layout = Layout::new([width, height], status, scale, true, usize::MAX);
                    assert!((1..=MAX_ITEMS).contains(&layout.capacity));
                    if let Some(band) = layout.band {
                        assert!(band.y > 0 && band.y + band.height <= height - status);
                        for index in 0..layout.count {
                            let cell = layout.cell(index).unwrap();
                            for pane in [cell.frame, cell.preview] {
                                assert!(pane.width > 0 && pane.height > 0);
                                assert!(pane.x + pane.width <= width);
                                assert!(
                                    pane.y >= band.y
                                        && pane.y + pane.height <= band.y + band.height
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn hiding_and_clearing_remove_items_and_hit_targets() {
        let mut strip = Filmstrip::new();
        strip.set_visible(true);
        strip.set_items(vec![FilmstripItem {
            index: 42,
            texture: None,
            orientation: 1,
            selected: true,
            rating: Some(5),
            label: "DSC00042.ARW".into(),
        }]);
        strip.resize([1200, 800], 32, 1.0);
        assert_eq!(strip.hit([600.0, 700.0]), Some(42));
        strip.set_visible(false);
        strip.resize([1200, 800], 32, 1.0);
        assert!(strip.items.is_empty());
        assert_eq!(strip.hit([600.0, 700.0]), None);
        assert!(strip.retained_textures().iter().all(Option::is_none));
        strip.set_visible(true);
        strip.resize([1200, 800], 32, 1.0);
        assert_eq!(strip.hit([600.0, 700.0]), None);
    }
}
