//! Opt-in desktop regressions for the read-only EXIF overlay.
use super::{App, Command, Key, ModifiersState, Ordering};
use std::{path::Path, sync::Arc};

impl App {
    pub(super) fn check_photo_details(&self) -> Result<(), String> {
        let renderer = self.renderer.as_ref().ok_or("no EXIF renderer")?;
        if !self.photo_details_visible
            || !renderer.photo_details_visible()
            || !renderer.photo_details_ready()
            || renderer.photo_details_content() != self.current_photo_details().as_deref()
        {
            return Err("EXIF overlay did not follow the displayed photograph".into());
        }
        if renderer.photo_details_texture_bytes() == 0 {
            return Err("visible EXIF overlay has no card texture".into());
        }
        Ok(())
    }

    pub(super) fn smoke_photo_details_controls(&mut self, directory: &Path) -> Result<(), String> {
        self.check_photo_details()?;
        let identity = |app: &App| {
            (
                app.current(),
                app.displayed.clone(),
                app.requests,
                app.revision_sequence,
                app.generation.load(Ordering::Acquire),
                app.textures
                    .iter()
                    .map(|(p, t)| (p.clone(), Arc::as_ptr(t) as usize))
                    .collect::<Vec<_>>(),
            )
        };
        let before = identity(self);
        let renderer = self.renderer.as_ref().ok_or("no EXIF renderer")?;
        let rasters = renderer.photo_details_rasterizations();
        let view = renderer.viewport().clone();
        self.modifiers = ModifiersState::empty();
        self.zoom(1.25);
        self.renderer
            .as_mut()
            .unwrap()
            .viewport_mut()
            .pan(35.0, -20.0);
        self.set_brightness(9);
        self.renderer
            .as_mut()
            .unwrap()
            .capture(&directory.join("exif-bright-zoom.ppm"))?;
        self.key(Key::Character("i".into()), true);
        if !self.photo_details_visible {
            return Err("holding EXIF shortcut repeatedly toggled the card".into());
        }
        self.key(Key::Character("i".into()), false);
        if self.photo_details_visible || self.renderer.as_ref().unwrap().photo_details_visible() {
            return Err("EXIF shortcut did not hide the card".into());
        }
        self.renderer
            .as_mut()
            .unwrap()
            .capture(&directory.join("exif-hidden.ppm"))?;
        self.key(Key::Character("i".into()), false);
        self.renderer
            .as_mut()
            .unwrap()
            .capture(&directory.join("exif-restored.ppm"))?;
        self.check_photo_details()?;
        if identity(self) != before
            || self
                .renderer
                .as_ref()
                .unwrap()
                .photo_details_rasterizations()
                != rasters
        {
            return Err("EXIF/zoom/pan/brightness changed photo requests, ratings, textures, or rerasterized the card".into());
        }
        *self.renderer.as_mut().unwrap().viewport_mut() = view;
        self.command(Command::ResetBrightness);
        eprintln!("SMOKE EXIF: toggle/repeat/zoom/pan/brightness pass; no photo requests, rating edits, texture replacement, or repeated card rasterization");
        Ok(())
    }
}
