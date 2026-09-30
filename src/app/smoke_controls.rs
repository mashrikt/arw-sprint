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
        self.modifiers = ModifiersState::empty();
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
        self.modifiers = ModifiersState::SHIFT;
        self.scroll(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            0.0, 40.0,
        )));
        let shifted = self.renderer.as_ref().unwrap().viewport().scale();
        if shifted <= 1.0 {
            return Err("Shift + two-finger scroll failed to zoom".into());
        }
        self.modifiers = ModifiersState::empty();
        self.drag_anchor = Some(self.cursor);
        self.cursor_moved([100.0, 60.0]);
        let dragged = self.renderer.as_ref().unwrap().viewport().scale();
        if dragged <= shifted || self.drag_anchor != Some([100.0, 100.0]) {
            return Err("upward drag failed to zoom around the initial click".into());
        }
        self.cursor_moved([100.0, 100.0]);
        if (self.renderer.as_ref().unwrap().viewport().scale() - shifted).abs() > 1e-10 {
            return Err("reverse drag did not restore zoom".into());
        }
        self.drag_anchor = None;
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
        self.modifiers = ModifiersState::SHIFT;
        self.release_key(&Key::Character("k".into()));
        if self.renderer.as_ref().unwrap().is_peeking()
            || *self.renderer.as_ref().unwrap().viewport() != peek_view
        {
            return Err("custom peek release failed after modifiers changed".into());
        }
        self.modifiers = ModifiersState::empty();
        *self.renderer.as_mut().unwrap().viewport_mut() = saved_view;
        if identity(self) != expected {
            return Err(
                "gesture/shortcut edits changed photo, requests, ratings, or cached textures"
                    .into(),
            );
        }
        eprintln!("SMOKE CONTROLS: pan/Shift-scroll/drag zoom and custom keys/peek pass; no photo requests or texture replacement");
        Ok(())
    }
}
