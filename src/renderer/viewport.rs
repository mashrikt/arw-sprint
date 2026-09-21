//! Physical-pixel viewport math. Changes never request image decoding.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DisplayMode {
    Fit,
    Fill,
    ActualSize,
    Custom,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Viewport {
    mode: DisplayMode,
    zoom: f64,
    pan: [f64; 2],
    image: [u32; 2],
    view: [u32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pane {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Pane {
    pub fn local_cursor(self, cursor: [f64; 2]) -> [f64; 2] {
        [
            (cursor[0] - f64::from(self.x)).clamp(0.0, f64::from(self.width)),
            (cursor[1] - f64::from(self.y)).clamp(0.0, f64::from(self.height)),
        ]
    }
}

/// Reference at left, current at right, with a two-physical-pixel divider.
pub fn image_panes(
    surface: [u32; 2],
    status_height: u32,
    comparison: bool,
) -> (Option<Pane>, Pane) {
    let width = surface[0].max(1);
    let height = surface[1].saturating_sub(status_height).max(1);
    if comparison && width >= 4 {
        let left = (width - 2) / 2;
        (
            Some(Pane {
                x: 0,
                y: 0,
                width: left,
                height,
            }),
            Pane {
                x: left + 2,
                y: 0,
                width: width - left - 2,
                height,
            },
        )
    } else {
        (
            None,
            Pane {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Fit,
            zoom: 1.0,
            pan: [0.0; 2],
            image: [1, 1],
            view: [1, 1],
        }
    }
}

impl Viewport {
    pub fn mode(&self) -> DisplayMode {
        self.mode
    }

    pub fn scale(&self) -> f64 {
        let x = f64::from(self.view[0]) / f64::from(self.image[0]);
        let y = f64::from(self.view[1]) / f64::from(self.image[1]);
        match self.mode {
            DisplayMode::Fit => x.min(y),
            DisplayMode::Fill => x.max(y),
            DisplayMode::ActualSize => 1.0,
            DisplayMode::Custom => self.zoom,
        }
    }

    pub fn set_geometry(&mut self, image: [u32; 2], view: [u32; 2]) {
        self.image = image.map(|x| x.max(1));
        self.view = view.map(|x| x.max(1));
        self.clamp_pan();
    }

    pub fn image_size(&self) -> [u32; 2] {
        self.image
    }

    pub fn view_size(&self) -> [u32; 2] {
        self.view
    }

    /// Image-relative point at the center of the pane, in displayed orientation.
    pub fn normalized_focus(&self) -> [f64; 2] {
        let scale = self.scale();
        [
            0.5 - self.pan[0] / (f64::from(self.image[0]) * scale),
            0.5 - self.pan[1] / (f64::from(self.image[1]) * scale),
        ]
    }

    pub fn set_normalized_focus(&mut self, focus: [f64; 2]) {
        if !focus.iter().all(|value| value.is_finite()) {
            return;
        }
        let scale = self.scale();
        for (axis, value) in focus.into_iter().enumerate() {
            self.pan[axis] = (0.5 - value.clamp(0.0, 1.0)) * f64::from(self.image[axis]) * scale;
        }
        self.clamp_pan();
    }

    pub fn set_geometry_preserving_focus(&mut self, image: [u32; 2], view: [u32; 2]) {
        let focus = self.normalized_focus();
        self.image = image.map(|x| x.max(1));
        self.view = view.map(|x| x.max(1));
        self.set_normalized_focus(focus);
    }

    /// Apply the next photo's dimensions and final orientation together. Clamping
    /// against a temporary unrotated shape can otherwise discard the focal point.
    pub(super) fn change_image(
        &mut self,
        stored_image: [u32; 2],
        view: [u32; 2],
        old_orientation: u16,
        new_orientation: u16,
        preserve_view: bool,
    ) {
        let focus = stored_focus(self.normalized_focus(), old_orientation);
        self.image = oriented_size(stored_image[0], stored_image[1], new_orientation)
            .map(|value| value.max(1));
        self.view = view.map(|value| value.max(1));
        if preserve_view {
            self.set_normalized_focus(displayed_focus(focus, new_orientation));
        } else {
            self.fit();
        }
    }

    pub fn for_geometry(&self, image: [u32; 2], view: [u32; 2]) -> Self {
        let mut result = self.clone();
        result.set_geometry_preserving_focus(image, view);
        result
    }

    pub fn fit(&mut self) {
        self.mode = DisplayMode::Fit;
        self.pan = [0.0; 2];
    }

    pub fn fill(&mut self) {
        self.mode = DisplayMode::Fill;
        self.pan = [0.0; 2];
    }

    pub fn actual_size(&mut self) {
        self.mode = DisplayMode::ActualSize;
        self.pan = [0.0; 2];
    }

    /// Set 100% while keeping the image point under this pane-local cursor fixed.
    pub fn actual_size_at(&mut self, cursor: [f64; 2]) {
        self.zoom(1.0 / self.scale(), Some(cursor));
        self.mode = DisplayMode::ActualSize;
    }

    pub fn toggle_fit_actual(&mut self) {
        if self.mode == DisplayMode::Fit {
            self.actual_size();
        } else {
            self.fit();
        }
    }

    /// Cursor coordinates are physical pixels relative to the image viewport.
    pub fn zoom(&mut self, factor: f64, cursor: Option<[f64; 2]>) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let old = self.scale();
        let next = (old * factor).clamp(0.01, 32.0);
        if let Some(cursor) = cursor.filter(|p| p.iter().all(|x| x.is_finite())) {
            for (axis, coordinate) in cursor.iter().enumerate() {
                let from_center = coordinate - f64::from(self.view[axis]) * 0.5;
                self.pan[axis] = from_center - (from_center - self.pan[axis]) * next / old;
            }
        }
        self.zoom = next;
        self.mode = DisplayMode::Custom;
        self.clamp_pan();
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.pan[0] += dx;
        self.pan[1] += dy;
        self.clamp_pan();
    }

    fn clamp_pan(&mut self) {
        let scale = self.scale();
        for axis in 0..2 {
            let limit =
                ((f64::from(self.image[axis]) * scale - f64::from(self.view[axis])) * 0.5).max(0.0);
            self.pan[axis] = self.pan[axis].clamp(-limit, limit);
        }
    }

    /// Quad scale and offset in full-window clip coordinates, leaving a status bar.
    pub fn transform(&self, surface: [u32; 2], status_height: u32) -> [f32; 4] {
        self.transform_in(surface, image_panes(surface, status_height, false).1)
    }

    pub fn transform_in(&self, surface: [u32; 2], pane: Pane) -> [f32; 4] {
        let width = f64::from(surface[0].max(1));
        let height = f64::from(surface[1].max(1));
        let scale = self.scale();
        [
            (f64::from(self.image[0]) * scale / width) as f32,
            (f64::from(self.image[1]) * scale / height) as f32,
            ((f64::from(pane.x) * 2.0 + f64::from(pane.width) + self.pan[0] * 2.0) / width - 1.0)
                as f32,
            (1.0 - (f64::from(pane.y) * 2.0 + f64::from(pane.height) + self.pan[1] * 2.0) / height)
                as f32,
        ]
    }
}

/// Convert an oriented displayed point to the corresponding stored JPEG point.
pub fn stored_focus(point: [f64; 2], orientation: u16) -> [f64; 2] {
    let [x, y] = point;
    match orientation {
        2 => [1.0 - x, y],
        3 => [1.0 - x, 1.0 - y],
        4 => [x, 1.0 - y],
        5 => [y, x],
        6 => [y, 1.0 - x],
        7 => [1.0 - y, 1.0 - x],
        8 => [1.0 - y, x],
        _ => point,
    }
}

pub fn displayed_focus(point: [f64; 2], orientation: u16) -> [f64; 2] {
    stored_focus(
        point,
        match orientation {
            6 => 8,
            8 => 6,
            other => other,
        },
    )
}

pub fn oriented_size(width: u32, height: u32, orientation: u16) -> [u32; 2] {
    if matches!(orientation, 5..=8) {
        [height, width]
    } else {
        [width, height]
    }
}

/// Affine map from displayed top-left UV coordinates back to stored JPEG pixels.
pub fn orientation_rows(orientation: u16) -> [[f32; 4]; 2] {
    let (a, b) = match orientation {
        2 => ([-1.0, 0.0, 1.0, 0.0], [0.0, 1.0, 0.0, 0.0]),
        3 => ([-1.0, 0.0, 1.0, 0.0], [0.0, -1.0, 1.0, 0.0]),
        4 => ([1.0, 0.0, 0.0, 0.0], [0.0, -1.0, 1.0, 0.0]),
        5 => ([0.0, 1.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]),
        6 => ([0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 1.0, 0.0]),
        7 => ([0.0, -1.0, 1.0, 0.0], [-1.0, 0.0, 1.0, 0.0]),
        8 => ([0.0, -1.0, 1.0, 0.0], [1.0, 0.0, 0.0, 0.0]),
        _ => ([1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]),
    };
    [a, b]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_fill_and_actual_preserve_aspect_ratio() {
        let mut viewport = Viewport::default();
        viewport.set_geometry([4000, 2000], [1000, 1000]);
        assert_eq!(viewport.scale(), 0.25);
        viewport.fill();
        assert_eq!(viewport.scale(), 0.5);
        viewport.actual_size();
        assert_eq!(viewport.scale(), 1.0);
        viewport.toggle_fit_actual();
        assert_eq!(viewport.mode(), DisplayMode::Fit);
    }

    #[test]
    fn zoom_keeps_cursor_point_fixed_and_pan_is_bounded() {
        let mut viewport = Viewport::default();
        viewport.set_geometry([1000, 1000], [1000, 1000]);
        viewport.zoom(2.0, Some([750.0, 500.0]));
        assert_eq!(viewport.pan, [-250.0, 0.0]);
        viewport.pan(10_000.0, -10_000.0);
        assert_eq!(viewport.pan, [500.0, -500.0]);
        viewport.zoom(f64::NAN, None);
        assert_eq!(viewport.scale(), 2.0);
        viewport.fit();
        assert_eq!(viewport.pan, [0.0; 2]);
    }

    #[test]
    fn all_orientations_map_corners_inside_source() {
        for orientation in 1..=8 {
            let rows = orientation_rows(orientation);
            let mut corners = Vec::new();
            for (x, y) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let u = rows[0][0] * x + rows[0][1] * y + rows[0][2];
                let v = rows[1][0] * x + rows[1][1] * y + rows[1][2];
                assert!((0.0..=1.0).contains(&u));
                assert!((0.0..=1.0).contains(&v));
                assert!(!corners.contains(&(u, v)));
                corners.push((u, v));
            }
        }
        assert_eq!(oriented_size(7008, 4672, 6), [4672, 7008]);
        assert_eq!(
            orientation_rows(6),
            [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 1.0, 0.0]]
        );
    }

    fn close(left: [f64; 2], right: [f64; 2]) {
        assert!((left[0] - right[0]).abs() < 1e-10, "{left:?} != {right:?}");
        assert!((left[1] - right[1]).abs() < 1e-10, "{left:?} != {right:?}");
    }

    #[test]
    fn preserve_focus_across_different_image_sizes_and_panes() {
        let mut view = Viewport::default();
        view.set_geometry([4000, 3000], [1200, 800]);
        view.actual_size();
        view.set_normalized_focus([0.3, 0.7]);
        view.set_geometry_preserving_focus([8000, 6000], [599, 800]);
        assert_eq!(view.mode(), DisplayMode::ActualSize);
        assert_eq!(view.scale(), 1.0);
        close(view.normalized_focus(), [0.3, 0.7]);
        let reference = view.for_geometry([6000, 4000], [599, 800]);
        close(reference.normalized_focus(), [0.3, 0.7]);
        assert_eq!(reference.scale(), view.scale());
        view.fit();
        view.set_geometry_preserving_focus([2000, 4000], [800, 800]);
        assert_eq!(view.scale(), 0.2);
        close(view.normalized_focus(), [0.5, 0.5]);
    }

    #[test]
    fn custom_zoom_and_subject_position_survive_next_photo_and_loading_gap() {
        let mut view = Viewport::default();
        view.set_geometry([7008, 4672], [1600, 1000]);
        view.zoom(3.0, None);
        view.set_normalized_focus([0.35, 0.65]);
        let scale = view.scale();
        let focus = view.normalized_focus();

        // Releasing the GPU image keeps its last geometry while loading. Repeated
        // redraw/layout updates during that gap must not reset the user's view.
        for _ in 0..5 {
            view.set_geometry_preserving_focus(view.image_size(), view.view_size());
            assert_eq!(view.mode(), DisplayMode::Custom);
            assert_eq!(view.scale(), scale);
            close(view.normalized_focus(), focus);
        }

        // A cached neighbor and a newly decoded neighbor use the same geometry
        // transition; custom zoom is measured in physical pixels per image pixel.
        for size in [[7008, 4672], [4608, 3072], [7008, 4672]] {
            view.set_geometry_preserving_focus(size, [1600, 1000]);
            assert_eq!(view.mode(), DisplayMode::Custom);
            assert_eq!(view.scale(), scale);
            close(view.normalized_focus(), focus);
        }
    }

    #[test]
    fn preserved_display_modes_recompute_only_their_intended_scale() {
        for mode in [DisplayMode::Fit, DisplayMode::Fill, DisplayMode::ActualSize] {
            let mut view = Viewport::default();
            view.set_geometry([4000, 2000], [1000, 800]);
            match mode {
                DisplayMode::Fit => view.fit(),
                DisplayMode::Fill => view.fill(),
                DisplayMode::ActualSize => view.actual_size(),
                DisplayMode::Custom => unreachable!(),
            }
            view.set_geometry_preserving_focus([2000, 4000], [1000, 800]);
            assert_eq!(view.mode(), mode);
            let expected_scale = match mode {
                DisplayMode::Fit => 0.2,
                DisplayMode::Fill => 0.5,
                DisplayMode::ActualSize => 1.0,
                DisplayMode::Custom => unreachable!(),
            };
            assert_eq!(view.scale(), expected_scale);
            close(view.normalized_focus(), [0.5, 0.5]);
        }
    }

    #[test]
    fn smaller_photo_clamps_unreachable_focus_without_discarding_custom_zoom() {
        let mut view = Viewport::default();
        view.set_geometry([4000, 3000], [1000, 800]);
        view.actual_size();
        view.zoom(0.5, None);
        view.set_normalized_focus([0.25, 0.7]);
        view.set_geometry_preserving_focus([1000, 800], [1000, 800]);
        assert_eq!(view.mode(), DisplayMode::Custom);
        assert_eq!(view.scale(), 0.5);
        close(view.normalized_focus(), [0.5, 0.5]);
    }

    #[test]
    fn next_portrait_uses_final_orientation_before_clamping_the_subject_position() {
        let mut view = Viewport::default();
        view.set_geometry([4000, 6000], [1400, 800]);
        view.actual_size();
        view.zoom(0.5, None);
        view.set_normalized_focus([0.5, 0.8]);
        view.change_image([6000, 4000], [1400, 800], 8, 8, true);
        assert_eq!(view.mode(), DisplayMode::Custom);
        assert_eq!(view.scale(), 0.5);
        close(view.normalized_focus(), [0.5, 0.8]);
    }

    #[test]
    fn late_orientation_preserves_the_stored_subject_and_unlocked_navigation_fits() {
        let mut view = Viewport::default();
        view.set_geometry([4000, 3000], [1000, 800]);
        view.actual_size();
        view.set_normalized_focus([0.3, 0.7]);
        view.change_image([4000, 3000], [1000, 800], 1, 8, true);
        assert_eq!(view.mode(), DisplayMode::ActualSize);
        assert_eq!(view.image_size(), [3000, 4000]);
        close(stored_focus(view.normalized_focus(), 8), [0.3, 0.7]);

        // L-off explicitly requests a fresh fitted view for the next photograph.
        view.change_image([6000, 4000], [1000, 800], 8, 1, false);
        assert_eq!(view.mode(), DisplayMode::Fit);
        assert_eq!(view.scale(), 1.0 / 6.0);
        close(view.normalized_focus(), [0.5, 0.5]);
    }

    #[test]
    fn cursor_peek_keeps_subject_at_cursor_and_saved_view_restores() {
        let mut view = Viewport::default();
        view.set_geometry([4000, 3000], [1000, 800]);
        view.zoom(2.0, None);
        view.set_normalized_focus([0.35, 0.6]);
        let saved = view.clone();
        let cursor = [750.0, 350.0];
        let point_at_cursor = |v: &Viewport| {
            let focus = v.normalized_focus();
            [
                focus[0]
                    + (cursor[0] - f64::from(v.view[0]) * 0.5)
                        / (f64::from(v.image[0]) * v.scale()),
                focus[1]
                    + (cursor[1] - f64::from(v.view[1]) * 0.5)
                        / (f64::from(v.image[1]) * v.scale()),
            ]
        };
        let before = point_at_cursor(&view);
        view.actual_size_at(cursor);
        assert_eq!(view.scale(), 1.0);
        close(point_at_cursor(&view), before);
        let restored = saved.for_geometry([4000, 3000], [1200, 900]);
        assert_eq!(restored.mode(), DisplayMode::Custom);
        assert_eq!(restored.scale(), saved.scale());
        close(restored.normalized_focus(), saved.normalized_focus());
    }

    #[test]
    fn normalized_sensor_focus_survives_every_exif_orientation() {
        let sensor = [0.2, 0.7];
        for old_orientation in 1..=8 {
            for new_orientation in 1..=8 {
                let old_display = displayed_focus(sensor, old_orientation);
                let new_display =
                    displayed_focus(stored_focus(old_display, old_orientation), new_orientation);
                close(stored_focus(new_display, new_orientation), sensor);
            }
        }
        close(displayed_focus(sensor, 6), [0.3, 0.2]);
    }

    #[test]
    fn comparison_geometry_leaves_divider_and_status_outside_photo_scissors() {
        let (reference, current) = image_panes([1001, 832], 32, true);
        let reference = reference.unwrap();
        assert_eq!(
            reference,
            Pane {
                x: 0,
                y: 0,
                width: 499,
                height: 800
            }
        );
        assert_eq!(
            current,
            Pane {
                x: 501,
                y: 0,
                width: 500,
                height: 800
            }
        );
        let mut view = Viewport::default();
        view.set_geometry([4000, 2000], [current.width, current.height]);
        let transform = view.transform_in([1001, 832], current);
        assert!((transform[2] - 501.0 / 1001.0).abs() < 1e-6);
        assert!((transform[3] - 32.0 / 832.0).abs() < 1e-6);
        assert_eq!(current.local_cursor([600.0, 400.0]), [99.0, 400.0]);
        let (reference, full) = image_panes([1001, 832], 32, false);
        assert!(reference.is_none());
        assert_eq!(full.width, 1001);
        assert!(image_panes([1, 1], 32, true).0.is_none());
    }
}
