//! UI bookkeeping for serialized file moves. Filesystem work lives in `io`.
use super::{deleted::MoveRecord, scan, App, Event};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
};

pub(super) enum UndoAction {
    Rating(PathBuf),
    Moved(Vec<MoveRecord>),
}

pub(super) struct PendingMove {
    id: u64,
    resume_scan: bool,
}

/// Keep the selected survivor, otherwise select the next visible survivor,
/// falling back to the previous one at the end of the filtered list.
fn next_survivor(
    files: &[PathBuf],
    index: Option<usize>,
    removed: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let index = index?.min(files.len());
    files[index..]
        .iter()
        .chain(files[..index].iter().rev())
        .find(|path| !removed.contains(*path))
        .cloned()
}

fn restore_listing(
    files: &[PathBuf],
    folder: Option<&std::path::Path>,
    records: &[MoveRecord],
) -> Vec<PathBuf> {
    let destinations: HashSet<_> = records.iter().map(|r| &r.destination).collect();
    let mut present = HashSet::with_capacity(files.len() + records.len());
    let mut restored = Vec::with_capacity(files.len() + records.len());
    for path in files {
        if !destinations.contains(path) && present.insert(path) {
            restored.push(path.clone());
        }
    }
    for record in records {
        if record.source.parent() == folder && present.insert(&record.source) {
            restored.push(record.source.clone());
        }
    }
    restored.sort_unstable_by(|a, b| {
        crate::browser::directory::natural_filename_cmp(
            a.file_name().unwrap_or(a.as_os_str()),
            b.file_name().unwrap_or(b.as_os_str()),
        )
        .then_with(|| a.cmp(b))
    });
    restored
}

impl App {
    pub(super) fn begin_file_mutation(&mut self, id: u64) {
        self.interaction_serial += 1;
        self.user_navigated = true;
        // A queued directory snapshot must not resurrect a moved filename.
        // Resume an interrupted startup scan only after the mutation completes.
        self.pending_deleted = Some(PendingMove {
            id,
            resume_scan: self.scanning,
        });
        self.folder_epoch.fetch_add(1, Ordering::AcqRel);
        self.scanning = false;
        self.scan_complete_only = false;
        self.io.cancel_rating_scan();
        // Removing a known photo cannot invalidate ratings of the survivors.
        // Keep a completed index instead of rereading thousands of sidecars on
        // every Space press while browsing a star filter.
        if !self.rating_index_complete {
            self.rating_index_started = false;
            self.ratings_read = 0;
            self.rating_errors = 0;
        }
        self.trash_busy = true;
    }

    fn finish_file_mutation(&mut self, id: u64) -> bool {
        if self
            .pending_deleted
            .as_ref()
            .is_none_or(|pending| pending.id != id)
        {
            return false;
        }
        let pending = self.pending_deleted.take().expect("checked pending move");
        self.trash_busy = false;
        if pending.resume_scan {
            if let Some(folder) = self.folder.clone() {
                self.scanning = true;
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
            }
        }
        true
    }

    fn remember_move(&mut self, id: u64, records: Vec<MoveRecord>) {
        if records.is_empty() {
            return;
        }
        let position = self
            .rating_history
            .iter()
            .position(|(other, _)| *other > id)
            .unwrap_or(self.rating_history.len());
        self.rating_history
            .insert(position, (id, UndoAction::Moved(records)));
        while self.rating_history.len() > 100 {
            self.rating_history.pop_front();
        }
    }

    pub(super) fn move_current_to_deleted(&mut self) {
        if self.quitting || self.trash_busy || self.smoke.is_some() {
            return;
        }
        let Some(path) = self
            .current()
            .filter(|path| self.displayed.as_ref() == Some(path))
        else {
            return;
        };
        self.move_paths_to_deleted(vec![path]);
    }

    fn move_paths_to_deleted(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.rating_sequence += 1;
        let id = self.rating_sequence;
        self.begin_file_mutation(id);
        self.io.move_deleted(self.session, id, paths);
        self.message("Moving to _Rejected...");
    }

    pub(super) fn collect_deleted(&mut self) {
        if self.quitting || self.trash_busy || self.smoke.is_some() {
            return;
        }
        if self.scanning {
            self.message("Folder is still being listed; try moving saved rejects when it finishes");
            return;
        }
        self.deleted_batch = true;
        self.trash_busy = true;
        self.io
            .collect_rejected(self.session, Arc::clone(&self.all_files));
        self.message("Checking saved rejects...");
    }

    pub(super) fn collected_deleted(&mut self, result: Result<Vec<PathBuf>, String>) {
        self.deleted_batch = false;
        match result {
            Ok(paths) if !paths.is_empty() => {
                let choice = rfd::MessageDialog::new().set_title("Move rejected photos to _Rejected?")
                    .set_description(format!("Move {} rejected photograph(s) and their XMP sidecars into the _Rejected folder? Cmd+Z can restore this move while ARW Sprint stays open.", paths.len()))
                    .set_buttons(rfd::MessageButtons::OkCancelCustom("Move to _Rejected".into(), "Cancel".into())).show();
                if choice == rfd::MessageDialogResult::Custom("Move to _Rejected".into()) {
                    self.move_paths_to_deleted(paths);
                    return;
                }
            }
            Ok(_) => self.alert(
                "No rejected photos",
                "No saved rejected photographs were found in this folder.",
            ),
            Err(error) => self.alert("Could not collect rejected photos", &error),
        }
        self.trash_busy = false;
        self.notice.clear();
        let current = self.current();
        self.refresh_visible(current.as_deref());
    }

    pub(super) fn moved_deleted(
        &mut self,
        id: u64,
        records: Vec<MoveRecord>,
        errors: Vec<(PathBuf, String)>,
    ) {
        if !self.finish_file_mutation(id) {
            return;
        }
        let removed: HashSet<_> = records.iter().map(|record| record.source.clone()).collect();
        let preferred = next_survivor(&self.files, self.navigator.index(), &removed);
        if self
            .pinned
            .as_ref()
            .is_some_and(|(path, _)| removed.contains(path))
        {
            self.clear_pin();
        }
        if !removed.is_empty() {
            self.all_files = Arc::new(
                self.all_files
                    .iter()
                    .filter(|path| !removed.contains(*path))
                    .cloned()
                    .collect(),
            );
        }
        if self.rating_index_complete {
            self.ratings_read = self.all_files.len();
        }
        let count = records.len();
        self.remember_move(id, records);
        self.start_rating_index();
        self.refresh_visible(preferred.as_deref());
        self.message(format!(
            "Moved {count} photo(s) to _Rejected. Cmd+Z restores"
        ));
        if !errors.is_empty() {
            let details = errors
                .iter()
                .map(|(path, error)| format!("{}: {error}", path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("Move to _Rejected: {details}");
            self.message(format!(
                "Moved {count}; {} photo(s) could not be moved",
                errors.len()
            ));
            self.alert("Some photos could not be moved", &details);
        }
    }

    pub(super) fn restored_deleted(
        &mut self,
        id: u64,
        restored: Vec<MoveRecord>,
        ratings: Vec<(PathBuf, Result<i8, String>)>,
        failed: Vec<(MoveRecord, String)>,
    ) {
        if !self.finish_file_mutation(id) {
            return;
        }
        // The worker read these after restoring the current sidecar bytes.
        // Old source metadata may be absent after a folder reopen, or stale
        // after rating the photo while reviewing _Rejected.
        for (path, rating) in ratings {
            let info = self.info.entry(path.clone()).or_default();
            info.revision = 0;
            info.saved_revision = 0;
            info.read = false;
            match rating {
                Ok(rating) => {
                    info.rating = Some(rating);
                    info.error = None;
                }
                Err(error) => {
                    eprintln!("Restored XMP {}: {error}", path.display());
                    info.rating = None;
                    info.error = Some(error);
                }
            }
        }
        let removed: HashSet<_> = restored
            .iter()
            .map(|record| record.destination.clone())
            .collect();
        let preferred = restored
            .iter()
            .find(|record| record.source.parent() == self.folder.as_deref())
            .map(|record| record.source.clone())
            .or_else(|| next_survivor(&self.files, self.navigator.index(), &removed));
        if self
            .pinned
            .as_ref()
            .is_some_and(|(path, _)| removed.contains(path))
        {
            self.clear_pin();
        }
        if !restored.is_empty() {
            self.all_files = Arc::new(restore_listing(
                &self.all_files,
                self.folder.as_deref(),
                &restored,
            ));
        }
        if self.rating_index_complete {
            self.ratings_read = self.all_files.len();
        }
        let details = failed
            .iter()
            .map(|(record, error)| format!("{}: {error}", record.source.display()))
            .collect::<Vec<_>>()
            .join("\n");
        self.remember_move(id, failed.into_iter().map(|(record, _)| record).collect());
        self.start_rating_index();
        self.refresh_visible(preferred.as_deref());
        self.message(format!(
            "Restored {} photo(s) from _Rejected",
            restored.len()
        ));
        if !details.is_empty() {
            eprintln!("Restore from _Rejected: {details}");
            self.message("Some photos could not be restored; Cmd+Z retries");
            self.alert("Some photos could not be restored", &details);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "fastcull-restore-list-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn raw(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, b"synthetic listing fixture").unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn restoring_original_folder_reinserts_naturally_without_duplicate_paths() {
        let fixture = Fixture::new();
        let first = fixture.raw("DSC1.ARW");
        let second = fixture.raw("DSC2.ARW");
        let tenth = fixture.raw("DSC10.ARW");
        let record_second = crate::app::deleted::move_photo(&second).unwrap();
        let record_first = crate::app::deleted::move_photo(&first).unwrap();
        crate::app::deleted::restore_photo(&record_second).unwrap();
        crate::app::deleted::restore_photo(&record_first).unwrap();
        // Existing and repeated records cannot duplicate an inserted source.
        let listing = restore_listing(
            &[tenth.clone(), first.clone()],
            Some(&fixture.0),
            &[record_second.clone(), record_second, record_first],
        );
        assert_eq!(listing, vec![first, second, tenth]);
    }

    #[test]
    fn restoring_while_viewing_deleted_removes_destinations_without_adding_parent_files() {
        let fixture = Fixture::new();
        let raw = fixture.raw("DSC2.ARW");
        let record = crate::app::deleted::move_photo(&raw).unwrap();
        let folder = fixture.0.join("_Rejected");
        let survivor = folder.join("DSC10.ARW");
        fs::write(&survivor, b"another _Rejected photograph").unwrap();
        let before = vec![record.destination.clone(), survivor.clone()];
        crate::app::deleted::restore_photo(&record).unwrap();
        let listing = restore_listing(&before, Some(&folder), std::slice::from_ref(&record));
        assert_eq!(listing, vec![survivor.clone()]);
        assert_eq!(
            next_survivor(&before, Some(0), &[record.destination].into()),
            Some(survivor)
        );
        assert!(!listing.contains(&raw));
        assert!(raw.is_file());
    }

    #[test]
    fn undo_from_another_folder_preserves_its_listing_and_selected_survivor() {
        let original = Fixture::new();
        let unrelated = Fixture::new();
        let raw = original.raw("DSC2.ARW");
        let record = crate::app::deleted::move_photo(&raw).unwrap();
        let first = unrelated.raw("DSC1.ARW");
        let second = unrelated.raw("DSC2.ARW");
        let before = vec![first.clone(), second.clone()];
        crate::app::deleted::restore_photo(&record).unwrap();
        assert_eq!(
            restore_listing(&before, Some(&unrelated.0), std::slice::from_ref(&record)),
            before
        );
        assert_eq!(
            next_survivor(&before, Some(1), &[record.destination].into()),
            Some(second)
        );
        assert!(raw.is_file());
    }

    #[test]
    fn deletion_advances_through_visible_survivors_and_handles_last_photo() {
        let files: Vec<_> = ["a", "b", "c", "d"].map(PathBuf::from).into();
        let removed = [files[1].clone(), files[2].clone()].into();
        assert_eq!(
            next_survivor(&files, Some(1), &removed),
            Some(files[3].clone())
        );
        assert_eq!(
            next_survivor(&files, Some(0), &removed),
            Some(files[0].clone())
        );
        let removed = [files[3].clone()].into();
        assert_eq!(
            next_survivor(&files, Some(3), &removed),
            Some(files[2].clone())
        );
        assert_eq!(
            next_survivor(&files, Some(0), &files.iter().cloned().collect()),
            None
        );
        assert_eq!(next_survivor(&[], None, &HashSet::new()), None);
    }
}
