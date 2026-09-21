//! UI-side bounded thumbnail cache and latest visible-window scheduling.
//! Parsing, JPEG decoding, XMP reads, and GPU uploads stay on workers.
use super::*;

const THUMB_CACHE_BYTES: usize = 4 * 1024 * 1024;
const THUMB_CACHE_COUNT: usize = 48;
const IDLE_GRACE: Duration = Duration::from_millis(150);

const THUMB_POOL_BYTES: usize = 8 * 1024 * 1024;
const PREFETCH_HALO: usize = 12;

pub(super) fn budget_limits(total: usize) -> Result<(usize, usize), String> {
    let main = total
        .checked_sub(THUMB_POOL_BYTES)
        .filter(|n| *n > 0)
        .ok_or("Image budget must exceed the 8 MiB thumbnail allowance")?;
    Ok((main, THUMB_POOL_BYTES))
}

fn warm_window(visible: std::ops::Range<usize>, length: usize) -> std::ops::Range<usize> {
    if visible.is_empty() {
        return 0..0;
    }
    visible.start.saturating_sub(PREFETCH_HALO)
        ..visible.end.saturating_add(PREFETCH_HALO).min(length)
}

fn distance_from_window(index: usize, visible: &std::ops::Range<usize>) -> usize {
    if index < visible.start {
        visible.start - index
    } else if index >= visible.end {
        index.saturating_sub(visible.end).saturating_add(1)
    } else {
        0
    }
}

impl App {
    pub(super) fn scroll(&mut self, delta: MouseScrollDelta) {
        if self
            .renderer
            .as_ref()
            .is_some_and(|r| r.over_filmstrip(self.cursor))
        {
            let (x, y) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (f64::from(x) * 60.0, f64::from(y) * 60.0),
                MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
            };
            let movement = if x.abs() > y.abs() { x } else { y };
            self.strip_scroll = (self.strip_scroll - movement).clamp(-480.0, 480.0);
            let steps = (self.strip_scroll / 60.0).trunc() as isize;
            self.strip_scroll -= steps as f64 * 60.0;
            if steps != 0 {
                self.scroll_strip(steps);
            }
        } else {
            self.strip_scroll = 0.0;
            self.zoom(match delta {
                MouseScrollDelta::LineDelta(_, y) => (f64::from(y) * 0.15).clamp(-2.0, 2.0).exp(),
                MouseScrollDelta::PixelDelta(p) => (p.y * 0.005).clamp(-2.0, 2.0).exp(),
            });
        }
    }

    pub(super) fn stop_thumbnail_work(&mut self) {
        self.thumb_serial = self.thumb_serial.wrapping_add(1);
        self.thumbnails.pause();
        if let Some(uploader) = &self.uploader {
            uploader.cancel_thumbnails();
        }
        self.thumb_plan_sent = false;
        self.thumb_deadline = None;
    }

    pub(super) fn managed_used(&self) -> usize {
        self.budget.used() + self.thumbnail_budget.used()
    }

    pub(super) fn managed_limit(&self) -> usize {
        self.budget.limit() + self.thumbnail_budget.limit()
    }

    fn synchronize_strip(&mut self) {
        let capacity = self
            .renderer
            .as_ref()
            .map_or(1, Renderer::filmstrip_capacity)
            .clamp(1, 24);
        self.strip_window.synchronize(self.files.len(), capacity);
    }

    /// Selection-following happens only on an explicit main-photo request.
    /// Metadata completions and redraws must not undo independent scrolling.
    pub(super) fn reveal_current_thumbnail(&mut self) {
        self.synchronize_strip();
        if let Some(index) = self.navigator.index() {
            self.strip_window.reveal(index);
        }
    }

    pub(super) fn scroll_strip(&mut self, steps: isize) {
        if !self.filmstrip_visible || self.quitting || self.trash_busy {
            return;
        }
        self.synchronize_strip();
        if self.strip_window.scroll(steps) {
            self.refresh_filmstrip();
            self.arm_thumbnails();
            self.redraw();
        }
    }

    fn warm_thumbnails(&self) -> std::ops::Range<usize> {
        warm_window(self.thumb_range.clone(), self.files.len())
    }

    /// LRU order survives photo navigation. Never discard the visible cells
    /// merely because a main photograph needs decode/upload memory.
    fn touch_visible_thumbnails(&mut self) {
        for index in self.thumb_range.clone() {
            if let Some(position) = self
                .thumb_cache
                .iter()
                .position(|(path, _)| self.files.get(index) == Some(path))
            {
                if let Some(entry) = self.thumb_cache.remove(position) {
                    self.thumb_cache.push_back(entry);
                }
            }
        }
    }

    fn trim_thumbnails(&mut self) {
        let warm = self.warm_thumbnails();
        while self.thumb_cache.len() > THUMB_CACHE_COUNT
            || self
                .thumb_cache
                .iter()
                .map(|(_, t)| t.byte_size)
                .sum::<usize>()
                > THUMB_CACHE_BYTES
        {
            let outside_warm = self
                .thumb_cache
                .iter()
                .position(|(path, _)| !warm.clone().any(|i| self.files.get(i) == Some(path)));
            let distant_neighbor = || {
                self.thumb_cache
                    .iter()
                    .enumerate()
                    .filter_map(|(position, (path, _))| {
                        let index = warm.clone().find(|&i| self.files.get(i) == Some(path))?;
                        let distance = distance_from_window(index, &self.thumb_range);
                        (distance > 0).then_some((distance, std::cmp::Reverse(position), position))
                    })
                    .max_by_key(|&(distance, age, _)| (distance, age))
                    .map(|(_, _, position)| position)
            };
            // Native thumbnail scaling guarantees all 24 visible cells fit.
            self.thumb_cache
                .remove(outside_warm.or_else(distant_neighbor).unwrap_or(0));
        }
    }

    fn defer_thumbnails(&mut self) {
        self.stop_thumbnail_work();
        self.thumb_retries = self.thumb_retries.saturating_add(1);
        // GPU frames can briefly retain offscreen textures. Give completion
        // callbacks a bounded chance to retire them, never an idle polling loop.
        if self.thumb_retries < 3 && self.filmstrip_visible && !self.quitting {
            self.thumb_deadline = Some(Instant::now() + IDLE_GRACE);
            self.redraw(); // Retire completed frame leases via nonblocking GPU polling.
        }
    }

    pub(super) fn toggle_filmstrip(&mut self) {
        self.end_peek();
        self.filmstrip_visible = !self.filmstrip_visible;
        self.thumb_retries = 0;
        self.strip_scroll = 0.0;
        self.stop_thumbnail_work();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_filmstrip_visible(self.filmstrip_visible);
        }
        if let Some(menu) = &self.menu {
            menu.set_filmstrip(self.filmstrip_visible);
        }
        if !self.filmstrip_visible {
            self.thumb_cache.clear();
        }
        self.refresh_filmstrip();
        self.arm_thumbnails();
        self.message(if self.filmstrip_visible {
            "Scroll previews independently; click to open; Tab hides strip"
        } else {
            "Tab shows thumbnails"
        });
    }

    pub(super) fn refresh_filmstrip(&mut self) {
        if !self.filmstrip_visible {
            return;
        }
        self.synchronize_strip();
        let range = self.strip_window.range();
        if range != self.thumb_range {
            self.stop_thumbnail_work();
            self.thumb_retries = 0;
            self.thumb_range = range.clone();
            self.touch_visible_thumbnails();
            self.thumb_deadline = Some(Instant::now() + IDLE_GRACE);
        }
        let items = range
            .map(|index| {
                let path = &self.files[index];
                let texture = self
                    .thumb_cache
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, t)| Arc::clone(t));
                let info = self.info.get(path);
                FilmstripItem {
                    index,
                    orientation: info.and_then(|i| i.metadata.as_ref()).map_or_else(
                        || texture.as_ref().map_or(1, |t| t.orientation),
                        |m| m.orientation,
                    ),
                    texture,
                    selected: Some(index) == self.navigator.index(),
                    rating: info.and_then(|i| i.rating),
                    label: path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .chars()
                        .take(24)
                        .collect(),
                }
            })
            .collect();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_filmstrip(items);
        }
    }

    /// Give the main image and nearest neighbor first chance. One idle grace
    /// deadline also lets the strip fill if a malformed next image or a tight
    /// image budget prevents that neighbor from becoming ready.
    pub(super) fn arm_thumbnails(&mut self) {
        if !self.filmstrip_visible
            || self.quitting
            || self.thumb_plan_sent
            || self.thumb_retries >= 3
            || self.files.is_empty()
        {
            return;
        }
        self.thumb_deadline
            .get_or_insert_with(|| Instant::now() + IDLE_GRACE);
        self.start_thumbnails(false);
    }

    pub(super) fn start_thumbnails(&mut self, grace_elapsed: bool) {
        if !self.filmstrip_visible
            || self.quitting
            || self.thumb_plan_sent
            || self.thumb_retries >= 3
        {
            return;
        }
        if self.current().is_none()
            || self.current() != self.displayed
            || self.pending_upload.is_some()
        {
            if grace_elapsed {
                self.thumb_deadline = None;
            }
            return;
        }
        let next_ready = self.next_prefetch_index().is_none_or(|index| {
            self.textures.iter().any(|(p, _)| p == &self.files[index])
                || self.loader.get(index).is_some()
        });
        if self.pending_gpu_prefetch.is_some() || (!next_ready && !grace_elapsed) {
            return;
        }
        let anchor = self
            .navigator
            .index()
            .filter(|i| self.thumb_range.contains(i))
            .unwrap_or(self.thumb_range.start + self.thumb_range.len() / 2);
        let mut missing: Vec<_> = self
            .warm_thumbnails()
            .filter_map(|index| {
                let path = self.files.get(index)?;
                (!self.thumb_cache.iter().any(|(p, _)| p == path)
                    && !self.thumb_unavailable.contains(path))
                .then(|| (index, path.clone()))
            })
            .collect();
        missing
            .sort_by_key(|(index, _)| (!self.thumb_range.contains(index), index.abs_diff(anchor)));
        self.thumb_plan_sent = true;
        self.thumb_deadline = None;
        if !missing.is_empty() {
            self.thumbnails
                .request(self.session, self.thumb_serial, missing);
        }
    }

    pub(super) fn thumbnail_event(&mut self, event: ThumbnailEvent) {
        let valid = self.filmstrip_visible
            && event.session == self.session
            && self.files.get(event.index) == Some(&event.path)
            && self.warm_thumbnails().contains(&event.index)
            && !self.quitting;
        if !valid {
            self.thumbnails.acknowledge(event.serial, event.index);
            return;
        }
        if self.info.get(&event.path).is_none_or(|i| i.revision == 0) {
            if let Ok(rating) = event.rating {
                self.info.entry(event.path.clone()).or_default().rating = Some(rating.unwrap_or(0));
            }
        }
        match event.result {
            Ok(image) => {
                self.thumb_decodes += 1;
                if self.log {
                    eprintln!(
                        "thumbnail={} parse={:.2}ms read={:.2}ms decode={:.2}ms total={:.2}ms",
                        event.path.display(),
                        event.timings.parse.as_secs_f64() * 1000.0,
                        event.timings.read.as_secs_f64() * 1000.0,
                        event.timings.decode.as_secs_f64() * 1000.0,
                        event.timings.total.as_secs_f64() * 1000.0
                    );
                }
                if self.thumb_cache.iter().any(|(p, _)| p == &event.path) {
                    self.thumbnails.acknowledge(event.serial, event.index);
                } else if self.current() != self.displayed
                    || self.pending_upload.is_some()
                    || self.pending_gpu_prefetch.is_some()
                {
                    self.stop_thumbnail_work();
                    self.thumbnails.acknowledge(event.serial, event.index);
                    self.arm_thumbnails();
                } else if let Some(uploader) = &self.uploader {
                    uploader.request_thumbnail(upload::ThumbJob {
                        session: event.session,
                        serial: event.serial,
                        index: event.index,
                        path: event.path,
                        image,
                        orientation: event.orientation,
                    });
                } else {
                    self.thumbnails.acknowledge(event.serial, event.index);
                }
            }
            Err(ThumbnailFailure::Unavailable(error)) => {
                if self.log {
                    eprintln!("thumbnail unavailable {}: {error}", event.path.display());
                }
                self.thumb_unavailable.insert(event.path);
                self.thumbnails.acknowledge(event.serial, event.index);
                if self.thumb_range.contains(&event.index) {
                    self.refresh_filmstrip();
                    self.redraw();
                }
            }
            Err(ThumbnailFailure::Deferred) => {
                self.thumbnails.acknowledge(event.serial, event.index);
                if event.serial == self.thumb_serial {
                    self.defer_thumbnails();
                }
            }
        }
    }

    pub(super) fn thumbnail_uploaded(&mut self, event: upload::ThumbFinished) {
        let valid = self.filmstrip_visible
            && event.session == self.session
            && self.files.get(event.index) == Some(&event.path)
            && self.warm_thumbnails().contains(&event.index)
            && !self.quitting;
        if valid {
            match event.result {
                Ok(texture) => {
                    self.thumb_uploads += 1;
                    self.thumb_retries = 0;
                    // A completed older plan can still warm this window. Never
                    // replace an already cached texture for the same photograph.
                    if !self.thumb_cache.iter().any(|(path, _)| path == &event.path) {
                        self.thumb_cache.push_back((event.path, texture));
                        self.trim_thumbnails();
                    }
                    if self.thumb_range.contains(&event.index) {
                        self.refresh_filmstrip();
                        self.redraw();
                    }
                }
                Err(error) => {
                    if self.log {
                        eprintln!(
                            "thumbnail GPU skipped (deferred={}): {error}",
                            event.deferred
                        );
                    }
                    if event.serial == self.thumb_serial {
                        self.defer_thumbnails();
                    }
                }
            }
        }
        self.thumbnails.acknowledge(event.serial, event.index);
    }

    pub(super) fn filmstrip_click(&mut self, point: [f64; 2]) -> bool {
        let Some(renderer) = &self.renderer else {
            return false;
        };
        if !renderer.over_filmstrip(point) {
            return false;
        }
        let index = renderer.filmstrip_hit(point);
        if !self.quitting && !self.trash_busy {
            if let Some(index) = index {
                self.interaction_serial += 1;
                if self.navigator.seek(index) {
                    self.user_navigated = true;
                    self.request_current();
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_thumbnail_pool_is_part_of_the_configured_total() {
        for mib in [256, 512, 1024, 2048] {
            let total = mib * 1024 * 1024;
            let (main, thumbs) = budget_limits(total).unwrap();
            assert_eq!(main + thumbs, total);
            assert_eq!(thumbs, 8 * 1024 * 1024);
            let main = MemoryBudget::new(main);
            let thumbs = MemoryBudget::new(thumbs);
            let _full_photo_pool = main.try_reserve(main.limit()).unwrap();
            // Even under maximum main-photo pressure, a complete strip and
            // its next small upload have protected space inside the total.
            let _strip = thumbs.try_reserve(THUMB_CACHE_BYTES).unwrap();
            assert!(thumbs.try_reserve(1024 * 1024).is_some());
            assert!(main.try_reserve(1).is_none());
            assert!(main.used() + thumbs.used() <= total);
        }
        assert!(budget_limits(8 * 1024 * 1024).is_err());
        assert!(budget_limits(0).is_err());
    }

    #[test]
    fn sliding_prefetch_halo_is_bounded_and_keeps_visible_cells() {
        assert_eq!(warm_window(0..0, 0), 0..0);
        assert_eq!(warm_window(0..12, 10_000), 0..24);
        assert_eq!(warm_window(500..524, 10_000), 488..536);
        assert_eq!(warm_window(9990..10_000, 10_000), 9978..10_000);
        let range = warm_window(usize::MAX - 24..usize::MAX, usize::MAX);
        assert_eq!(range.len(), 36);
        assert_eq!(range.end, usize::MAX);
        assert_eq!(distance_from_window(499, &(500..524)), 1);
        assert_eq!(distance_from_window(524, &(500..524)), 1);
        assert_eq!(distance_from_window(535, &(500..524)), 12);
        assert_eq!(distance_from_window(510, &(500..524)), 0);
    }
}
