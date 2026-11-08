//! Explicit RAW viewing state, kept separate from the ordinary JPEG caches.
use super::{raw_loader, upload, App};
use crate::{
    browser::directory::{is_jpeg, is_photo},
    renderer::PreparedTexture,
};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Default, Debug, PartialEq, Eq)]
enum Mode {
    #[default]
    Preview,
    Current(PathBuf),
    Browse,
}
#[derive(Default)]
pub(super) struct RawView {
    mode: Mode,
    serial: u64,
    pending: Option<raw_loader::Request>,
    pub displayed: bool,
    pub cache: VecDeque<(PathBuf, Arc<PreparedTexture>)>,
    failed: HashSet<PathBuf>,
}
impl RawView {
    fn wanted(&self, path: &Path) -> bool {
        eligible(path)
            && match &self.mode {
                Mode::Preview => false,
                Mode::Current(p) => p == path,
                Mode::Browse => true,
            }
    }
    fn navigate(&mut self, path: &Path) {
        if matches!(&self.mode, Mode::Current(p) if p != path) {
            self.mode = Mode::Preview;
        }
    }
    pub fn active(&self) -> bool {
        self.mode != Mode::Preview
    }
}
fn eligible(path: &Path) -> bool {
    is_photo(path) && !is_jpeg(path)
}
impl App {
    pub(super) fn cancel_raw_work(&mut self) {
        if let Some(worker) = &self.raw_worker {
            worker.cancel();
        }
        if let Some(uploader) = &self.uploader {
            uploader.cancel_raw();
        }
        self.raw.pending = None;
    }
    pub(super) fn reset_raw(&mut self) {
        self.cancel_raw_work();
        self.raw = RawView::default();
    }
    pub(super) fn raw_button_label(&self) -> Option<&'static str> {
        let path = self.current()?;
        if !eligible(&path) {
            return None;
        }
        Some(if self.raw.active() {
            "View Preview"
        } else {
            "View RAW"
        })
    }
    pub(super) fn raw_status(&self) -> &'static str {
        if self.raw.displayed {
            if self.raw.mode == Mode::Browse {
                "RAW BROWSE"
            } else {
                "RAW"
            }
        } else if self
            .raw
            .pending
            .as_ref()
            .is_some_and(|r| Some(&r.path) == self.current().as_ref())
        {
            "PREVIEW / RAW loading..."
        } else if self
            .current()
            .as_ref()
            .is_some_and(|p| crate::browser::directory::is_jpeg(p))
        {
            "JPEG"
        } else {
            "PREVIEW"
        }
    }
    pub(super) fn toggle_raw(&mut self, browse: bool) {
        let Some(path) = self.current() else {
            return;
        };
        if !eligible(&path) && !self.raw.active() {
            self.message("JPEG is already the original image; RAW view is for ARW/DNG");
            return;
        }
        let turn_off = if browse {
            self.raw.mode == Mode::Browse
        } else {
            self.raw.active()
        };
        self.cancel_raw_work();
        self.raw.failed.clear();
        if turn_off {
            self.raw.mode = Mode::Preview;
            self.raw.cache.clear();
            // Keep the RAW visible until its JPEG replacement is ready.
            self.request_current_preserving_strip();
        } else {
            self.raw.mode = if browse {
                Mode::Browse
            } else {
                Mode::Current(path)
            };
            self.schedule_raw();
        }
        self.status();
    }
    /// Returns true when a cached RAW can satisfy navigation immediately.
    pub(super) fn raw_navigation(&mut self, path: &Path) -> bool {
        self.raw.navigate(path);
        if self
            .raw
            .pending
            .as_ref()
            .is_some_and(|r| r.path != path || r.session != self.session)
        {
            self.cancel_raw_work();
        }
        if self.raw.wanted(path) {
            if let Some(texture) = self
                .raw
                .cache
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, t)| Arc::clone(t))
            {
                self.show_raw(path.to_path_buf(), texture);
                return true;
            }
        }
        // An old RAW must not compete with the requested JPEG and its staging
        // buffers. The renderer alone retains the outgoing image until replaced.
        self.raw.cache.retain(|(p, _)| p == path);
        false
    }
    pub(super) fn schedule_raw(&mut self) {
        if self.quitting || self.raw.pending.is_some() || !self.raw.active() {
            return;
        }
        let Some(current) = self.current() else {
            return;
        };
        let current_ready = self.displayed.as_ref() == Some(&current);
        if !current_ready && self.failed_preview.as_ref() != Some(&current) {
            return;
        }
        let target = if self.raw.wanted(&current) && !(self.raw.displayed && current_ready) {
            Some(current.clone())
        } else if self.raw.mode == Mode::Browse && current_ready {
            self.next_prefetch_index()
                .map(|i| self.files[i].clone())
                .filter(|p| eligible(p))
        } else {
            None
        };
        let Some(path) = target else {
            return;
        };
        if self.raw.failed.contains(&path) || self.raw.cache.iter().any(|(p, _)| p == &path) {
            return;
        }
        // Only current + one neighbor. Drop the previous neighbor before
        // allocating native working memory or another RGBA image.
        self.raw.cache.retain(|(p, _)| p == &current);
        self.textures
            .retain(|(p, _)| p == &current && !self.raw.displayed);
        self.gpu_prefetched.clear();
        self.pending_gpu_prefetch = None;
        self.loader.evict_all_except(None);
        self.sync_gpu_cache();
        self.raw.serial = self.raw.serial.wrapping_add(1).max(1);
        let request = raw_loader::Request {
            id: self.raw.serial,
            session: self.session,
            path,
        };
        if let Some(worker) = &self.raw_worker {
            worker.request(request.clone());
            self.raw.pending = Some(request);
        }
    }
    pub(super) fn raw_decoded(&mut self, event: raw_loader::Finished) {
        if self.raw.pending.as_ref() != Some(&event.request)
            || event.request.session != self.session
        {
            return;
        }
        if self.log {
            eprintln!(
                "RAW {} decode_pipeline={:.1}ms",
                event.request.path.display(),
                event.elapsed_ms
            );
        }
        match event.result {
            Ok((image, orientation)) => {
                if let Some(uploader) = &self.uploader {
                    uploader.request_raw(upload::RawJob {
                        id: event.request.id,
                        session: event.request.session,
                        path: event.request.path,
                        image,
                        orientation,
                    });
                }
            }
            Err(error) => {
                self.raw_failure(event.request.path, error);
            }
        }
    }
    pub(super) fn raw_uploaded(&mut self, event: upload::RawFinished) {
        if !self
            .raw
            .pending
            .as_ref()
            .is_some_and(|r| r.id == event.id && r.session == event.session && r.path == event.path)
            || event.session != self.session
        {
            return;
        }
        self.raw.pending = None;
        match event.result {
            Ok(texture) => {
                if self.log {
                    eprintln!(
                        "RAW {} upload={:.1}ms managed={:.1}MiB",
                        event.path.display(),
                        texture.timings.completion_ms,
                        self.budget.used() as f64 / 1048576.0
                    );
                }
                self.raw.cache.retain(|(p, _)| p != &event.path);
                self.raw
                    .cache
                    .push_back((event.path.clone(), Arc::clone(&texture)));
                if self.current().as_ref() == Some(&event.path) && self.raw.wanted(&event.path) {
                    self.show_raw(event.path, texture);
                }
                self.schedule_raw();
                self.status();
            }
            Err(error) => self.raw_failure(event.path, error),
        }
    }
    fn raw_failure(&mut self, path: PathBuf, error: String) {
        self.raw.pending = None;
        self.raw.failed.insert(path.clone());
        eprintln!("RAW {}: {error}", path.display());
        if self.current().as_ref() == Some(&path) {
            if self.failed_preview.as_ref() == Some(&path) {
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_message("RAW unavailable - use arrows to continue");
                }
            }
            self.message(format!("RAW unavailable: {error}"));
        }
    }
    fn show_raw(&mut self, path: PathBuf, texture: Arc<PreparedTexture>) {
        let same_photo = self.displayed.as_ref() == Some(&path);
        self.end_peek();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_preserve_view(same_photo || self.zoom_locked);
            renderer.set_image_with_orientation(Some(Arc::clone(&texture)), texture.orientation);
            renderer.set_preserve_view(self.zoom_locked);
            renderer.set_message("");
        }
        self.displayed = Some(path);
        self.raw.displayed = true;
        self.failed_preview = None;
        self.textures.clear();
        self.loader.evict_all_except(None);
        self.sync_gpu_cache();
        self.first_render = true;
        self.status();
    }
    pub(super) fn raw_fallback(&mut self, path: PathBuf) {
        if eligible(&path) {
            if !self.raw.active() {
                self.raw.mode = Mode::Current(path);
            }
            self.schedule_raw();
            self.status();
        }
    }
    pub(super) fn retry_raw_failures(&mut self) {
        self.raw.failed.clear();
        self.schedule_raw();
    }
    pub(super) fn reconcile_raw(&mut self) {
        self.cancel_raw_work();
        let paths: HashSet<_> = self.files.iter().collect();
        self.raw.cache.retain(|(p, _)| paths.contains(p));
        self.raw.failed.retain(|p| paths.contains(p));
        if self.current() != self.displayed {
            self.raw.displayed = false;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_raw_resets_on_navigation_browse_persists() {
        let a = PathBuf::from("a.ARW");
        let b = PathBuf::from("b.ARW");
        let mut state = RawView {
            mode: Mode::Current(a.clone()),
            ..Default::default()
        };
        assert!(state.wanted(&a));
        assert!(!state.wanted(&b));
        state.navigate(&b);
        assert!(!state.active());
        state.mode = Mode::Browse;
        state.navigate(&a);
        assert!(state.wanted(&b));
        assert!(!state.wanted(Path::new("a.JPG")));
        assert!(state.wanted(Path::new("a.dng")));
    }
}
