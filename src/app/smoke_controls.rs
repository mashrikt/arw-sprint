//! Read-only controls regression checks for the opt-in desktop smoke harness.
use super::{App, Bindings, Key, ModifiersState, MouseScrollDelta, Ordering};
use std::sync::Arc;
use winit::dpi::PhysicalPosition;

impl App {
    pub(super) fn smoke_controls(&mut self) -> Result<(), String> {
        let identity = |app: &App| {
            (
                app.current(),
                app.displayed.clone(),
                app.generation.load(Ordering::Acquire),
                app.requests,
                app.revision_sequence,
                app.textures
                    .iter()
                    .map(|(p, t)| (p.clone(), Arc::as_ptr(t) as usize))
                    .collect::<Vec<_>>(),
            )
        };
        let expected = identity(self);
        let renderer = self.renderer.as_mut().ok_or("no controls renderer")?;
        let saved_view = renderer.viewport().clone();
        renderer.viewport_mut().actual_size();
        self.cursor = [100.0, 100.0];
        self.set_modifiers(ModifiersState::empty());
        let before = self.renderer.as_ref().unwrap().viewport().clone();
        self.scroll(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            24.0, -18.0,
        )));
        let after = self.renderer.as_ref().unwrap().viewport();
        if after.scale() != before.scale() {
            return Err("two-finger scroll changed zoom instead of panning".into());
        }
        // Tiny synthetic fixtures fit inside the canvas even at 100% and cannot pan.
        let can_pan = before
            .image_size()
            .iter()
            .zip(before.view_size())
            .any(|(image, view)| *image > view);
        if can_pan && after.normalized_focus() == before.normalized_focus() {
            return Err("two-finger scroll did not pan an enlarged image".into());
        }
        self.set_modifiers(ModifiersState::SHIFT);
        self.scroll(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 40.0,
        )));
        let shifted = self.renderer.as_ref().unwrap().viewport().scale();
        if shifted <= 1.0 {
            return Err("Shift + two-finger scroll failed to zoom".into());
        }
        self.set_modifiers(ModifiersState::empty());
        self.drag_anchor = Some(self.cursor);
        let drag_start = self.renderer.as_ref().unwrap().viewport().clone();
        for position in [
            [140.0, 100.0],
            [100.0, 100.0],
            [100.0, 60.0],
            [100.0, 100.0],
        ] {
            let before = self.renderer.as_ref().unwrap().viewport().clone();
            let axis = usize::from(position[0] == self.cursor[0]);
            let can_pan_axis = f64::from(before.image_size()[axis]) * before.scale()
                > f64::from(before.view_size()[axis]);
            self.cursor_moved(position);
            let after = self.renderer.as_ref().unwrap().viewport();
            if after.scale() != shifted {
                return Err("plain click-drag changed zoom instead of panning".into());
            }
            if can_pan_axis && after.normalized_focus() == before.normalized_focus() {
                return Err("click-drag did not move an enlarged image".into());
            }
        }
        if self
            .renderer
            .as_ref()
            .unwrap()
            .viewport()
            .normalized_focus()
            .iter()
            .zip(drag_start.normalized_focus())
            .any(|(actual, expected)| (actual - expected).abs() > 1e-10)
        {
            return Err("reversing mouse pan did not restore the image position".into());
        }
        // Holding/releasing Shift during one drag switches the transform
        // without resetting the click anchor or changing photo ownership.
        self.set_modifiers(ModifiersState::SHIFT);
        self.cursor_moved(self.cursor);
        self.cursor_moved([100.0, 60.0]);
        let dragged = self.renderer.as_ref().unwrap().viewport().scale();
        if dragged <= shifted || self.drag_anchor != Some([100.0, 100.0]) {
            return Err("Shift + upward drag failed to zoom around the initial click".into());
        }
        self.cursor_moved([100.0, 100.0]);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted).abs() > 1e-10 {
            return Err("reverse drag did not restore zoom".into());
        }
        self.set_modifiers(ModifiersState::empty());
        self.drag_anchor = None;
        let released = self.renderer.as_ref().unwrap().viewport().clone();
        self.cursor_moved([100.0, 70.0]);
        if *self.renderer.as_ref().unwrap().viewport() != released {
            return Err("pointer movement continued transforming after drag release".into());
        }
        self.cursor = [100.0, 100.0];
        // Shift also zooms without any mouse button held.
        self.set_modifiers(ModifiersState::SHIFT);
        self.cursor_moved(self.cursor);
        let horizontal_start = self.renderer.as_ref().unwrap().viewport().clone();
        self.cursor_moved([140.0, 100.0]);
        self.cursor_moved([100.0, 100.0]);
        if *self.renderer.as_ref().unwrap().viewport() != horizontal_start {
            return Err("horizontal Shift pointer movement changed the image view".into());
        }
        self.cursor_moved([100.0, 60.0]);
        if self.drag_anchor.is_some()
            || self.renderer.as_ref().unwrap().viewport().scale() <= shifted
        {
            return Err("Shift + pointer movement did not zoom without a click".into());
        }
        self.cursor_moved([100.0, 100.0]);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted).abs() > 1e-10 {
            return Err("reversing Shift pointer zoom did not restore scale".into());
        }
        // Model leaving, then a modifier update before the first fresh cursor
        // event on re-entry. Neither order may re-arm a stale position.
        self.shift_zoom_anchor = None;
        self.set_modifiers(ModifiersState::empty());
        self.set_modifiers(ModifiersState::SHIFT);
        self.cursor_moved([400.0, 300.0]);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted).abs() > 1e-10 {
            return Err("Shift pointer re-entry caused a zoom jump".into());
        }
        self.set_modifiers(ModifiersState::empty());
        let released = self.renderer.as_ref().unwrap().viewport().clone();
        self.cursor_moved([100.0, 100.0]);
        if *self.renderer.as_ref().unwrap().viewport() != released
            || self.shift_zoom_anchor.is_some()
        {
            return Err("releasing Shift did not end pointer zoom".into());
        }
        self.apply_shortcuts(Bindings::parse(
            "brighten=Cmd+B\nzoom_in=V\npeek=K\nmove_rejected=\n",
        )?);
        self.key(Key::Character("s".into()), false);
        self.key(Key::Character("]".into()), false);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted).abs() > 1e-10
            || self.brightness_steps != 0
        {
            return Err("replaced default shortcut was still active".into());
        }
        self.key(Key::Character("v".into()), true);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted * 1.25).abs() > 1e-10 {
            return Err("custom zoom shortcut did not repeat".into());
        }
        let peek_view = self.renderer.as_ref().unwrap().viewport().clone();
        self.key(Key::Character("k".into()), false);
        self.release_key(&Key::Character("p".into()));
        if !self.renderer.as_ref().unwrap().is_peeking() {
            return Err("custom peek did not hold until its own key was released".into());
        }
        self.set_modifiers(ModifiersState::SHIFT);
        self.release_key(&Key::Character("k".into()));
        if self.renderer.as_ref().unwrap().is_peeking()
            || *self.renderer.as_ref().unwrap().viewport() != peek_view
        {
            return Err("custom peek release failed after modifiers changed".into());
        }
        self.set_modifiers(ModifiersState::empty());
        *self.renderer.as_mut().unwrap().viewport_mut() = saved_view;
        if identity(self) != expected {
            return Err(
                "gesture/shortcut edits changed photo, requests, ratings, or cached textures"
                    .into(),
            );
        }
        eprintln!("SMOKE CONTROLS: two-finger pan, mouse pan, Shift-scroll/Shift-pointer/Shift-drag zoom and custom keys/peek pass; no photo requests or texture replacement");
        Ok(())
    }
}
