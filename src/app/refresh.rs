//! Explicit folder rescan: keep the live view until a complete background list
//! arrives, then reconcile paths without reopening the folder or purging caches.
use super::*;

impl App {
    pub(super) fn refresh_folder(&mut self) {
        if self.quitting || self.trash_busy || self.refreshing {
            return;
        }
        let Some(folder) = self.folder.clone() else {
            return;
        };
        self.restore_allowed = false;
        self.user_navigated = true;
        self.interaction_serial += 1;
        self.io.cancel_rating_scan();
        self.rating_index_started = false;
        self.scanning = true;
        self.refreshing = true;
        self.scan_complete_only = true;
        let epoch = self.folder_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let proxy = self.proxy.clone();
        scan::start(
            folder,
            epoch,
            Arc::clone(&self.folder_epoch),
            Arc::new(move |result| {
                let _ = proxy.send_event(Event::Scan(result));
            }),
        );
        self.message("Checking folder for changes...");
    }

    pub(super) fn apply_folder_refresh(&mut self, result: scan::ScanResult) {
        self.scanning = false;
        self.refreshing = false;
        self.scan_complete_only = false;
        let present: HashSet<_> = result.files.iter().collect();
        let previous: HashSet<_> = self.all_files.iter().collect();
        let added: HashSet<_> = result
            .files
            .iter()
            .filter(|path| !previous.contains(path))
            .cloned()
            .collect();
        let removed: HashSet<_> = self
            .all_files
            .iter()
            .filter(|path| !present.contains(path))
            .cloned()
            .collect();
        // Restore guards are reconciled on the serialized I/O worker before
        // any later rating or move can run against a Finder-restored file.
        self.io.reconcile_restored(result.folder.clone());
        // Use the selection at completion: navigation can continue during a scan.
        let preferred = deletion::next_survivor(&self.files, self.navigator.index(), &removed);
        self.invalidate_thumbnail_work();
        self.thumb_unavailable.clear(); // Cmd+R explicitly retries prior failures.
        self.info.retain(|path, info| {
            (!removed.contains(path) && !added.contains(path))
                || info.revision != info.saved_revision
        });
        if self
            .pinned
            .as_ref()
            .is_some_and(|(path, _)| removed.contains(path))
        {
            self.clear_pin();
        }
        if *self.all_files != *result.files {
            self.all_files = result.files;
        }
        self.start_rating_index();
        let requests_before = self.requests;
        self.refresh_visible(preferred.as_deref());
        if self.current().is_some()
            && self.current() != self.displayed
            && self.current() == self.failed_preview
            && self.pending_upload.is_none()
            && self.requests == requests_before
        {
            self.request_current_preserving_strip();
        }
        self.retry_raw_failures();
        self.arm_thumbnails();
        self.message(format!(
            "Folder checked: {} added, {} removed",
            added.len(),
            removed.len()
        ));
    }
}
