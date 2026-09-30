//! Filmstrip scrolling is independent of the selected/main photograph.
//!
//! Only explicit photo navigation should call `reveal`. Routine redraws and
//! thumbnail completions must not recenter a window the user has scrolled.

use std::{collections::HashMap, ops::Range, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilmstripWindow {
    len: usize,
    capacity: usize,
    start: usize,
}

impl FilmstripWindow {
    /// Update folder/filter length and available cells while retaining the
    /// first visible index where possible. Grow/shrink at the trailing edge
    /// without leaving an avoidable empty tail. Returns whether the range changed.
    pub fn synchronize(&mut self, len: usize, capacity: usize) -> bool {
        let previous = self.range();
        self.len = len;
        self.capacity = capacity.min(len);
        self.start = if self.capacity == 0 {
            0
        } else {
            self.start.min(self.len - self.capacity)
        };
        self.range() != previous
    }

    /// Retain the first visible photograph when a folder's contents change.
    /// If it disappeared, anchor to the next surviving old photograph, then
    /// the nearest previous survivor. A full trailing page takes precedence
    /// over keeping an anchor at the left edge. This never reveals selection.
    /// Returns whether the numeric range changed, not whether its contents did.
    ///
    /// Build a borrowed index once so even a large batch removal remains
    /// linear in the folder sizes rather than repeatedly searching the list.
    pub fn reconcile_paths(
        &mut self,
        previous: &[PathBuf],
        current: &[PathBuf],
        capacity: usize,
    ) -> bool {
        let previous_range = self.range();
        let old_start = self.start.min(previous.len());
        let positions: HashMap<_, _> = current
            .iter()
            .enumerate()
            .map(|(index, path)| (path.as_path(), index))
            .collect();
        let anchor = previous[old_start..]
            .iter()
            .chain(previous[..old_start].iter().rev())
            .find_map(|path| positions.get(path.as_path()).copied())
            .unwrap_or(0);
        self.synchronize(current.len(), capacity);
        self.start = if self.capacity == 0 {
            0
        } else {
            anchor.min(self.len - self.capacity)
        };
        self.range() != previous_range
    }

    pub fn range(&self) -> Range<usize> {
        // All mutators maintain start <= len - capacity, including usize::MAX.
        self.start..self.start + self.capacity
    }

    /// Move only the strip by a number of cells. Does not own or mutate a photo
    /// selection. Extreme signed deltas clamp safely at either end.
    pub fn scroll(&mut self, delta: isize) -> bool {
        if self.capacity == 0 {
            return false;
        }
        let previous = self.start;
        self.start = if delta >= 0 {
            self.start
                .saturating_add(delta as usize)
                .min(self.len - self.capacity)
        } else {
            self.start.saturating_sub(delta.unsigned_abs())
        };
        self.start != previous
    }

    /// Shift minimally to reveal a newly selected photo, leaving an already
    /// visible photo's surrounding cells fixed. Invalid indices are ignored.
    pub fn reveal(&mut self, index: usize) -> bool {
        if self.capacity == 0 || index >= self.len {
            return false;
        }
        let previous = self.start;
        if index < self.start {
            self.start = index;
        } else if index >= self.range().end {
            self.start = index - (self.capacity - 1);
        }
        self.start != previous
    }

    /// Return to the first cells, retaining the current length and capacity.
    pub fn reset(&mut self) -> bool {
        let changed = self.start != 0;
        self.start = 0;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(len: usize, capacity: usize) -> FilmstripWindow {
        let mut window = FilmstripWindow::default();
        window.synchronize(len, capacity);
        window
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn list_changes_before_the_window_keep_the_same_leftmost_photo() {
        let original = paths(&["a", "b", "c", "d", "e", "f"]);
        let inserted = paths(&["a", "b", "b1", "c", "d", "e", "f"]);
        let mut strip = window(original.len(), 2);
        strip.scroll(3);
        assert!(strip.reconcile_paths(&original, &inserted, 2));
        assert_eq!(strip.range(), 4..6);
        assert_eq!(inserted[strip.range().start], PathBuf::from("d"));

        let removed = paths(&["b", "c", "d", "e", "f"]);
        assert!(strip.reconcile_paths(&inserted, &removed, 2));
        assert_eq!(strip.range(), 2..4);
        assert_eq!(removed[strip.range().start], PathBuf::from("d"));
    }

    #[test]
    fn a_removed_leftmost_photo_anchors_to_the_next_survivor() {
        let original = paths(&["a", "b", "c", "d", "e", "f", "g"]);
        let removed = paths(&["a", "b", "e", "f", "g"]);
        let mut strip = window(original.len(), 2);
        strip.scroll(2);
        // The numeric range stays put, but now starts with the first survivor.
        assert!(!strip.reconcile_paths(&original, &removed, 2));
        assert_eq!(strip.range(), 2..4);
        assert_eq!(removed[strip.range().start], PathBuf::from("e"));
    }

    #[test]
    fn removing_the_tail_falls_back_to_previous_survivor_and_clamps() {
        let original = paths(&["a", "b", "c", "d", "e", "f"]);
        let removed = paths(&["a", "b", "c", "d"]);
        let mut strip = window(original.len(), 2);
        strip.scroll(4);
        assert!(strip.reconcile_paths(&original, &removed, 2));
        // The nearest survivor is d, but a full final page includes c as well.
        assert_eq!(strip.range(), 2..4);
    }

    #[test]
    fn an_existing_anchor_clamps_to_a_full_final_page() {
        let original = paths(&["a", "b", "c", "d", "e", "f", "g"]);
        let removed = paths(&["a", "b", "c", "d", "e"]);
        let mut strip = window(original.len(), 3);
        strip.scroll(4);
        assert!(strip.reconcile_paths(&original, &removed, 3));
        assert_eq!(strip.range(), 2..5);
        assert!(strip.range().contains(&4));
    }

    #[test]
    fn removing_every_photo_and_repopulating_is_safe() {
        let original = paths(&["a", "b", "c", "d"]);
        let mut strip = window(original.len(), 2);
        strip.scroll(2);
        assert!(strip.reconcile_paths(&original, &[], 2));
        assert_eq!(strip.range(), 0..0);
        assert!(strip.reconcile_paths(&[], &original, 2));
        assert_eq!(strip.range(), 0..2);

        strip.scroll(2);
        assert!(strip.reconcile_paths(&original, &paths(&["x", "y", "z"]), 2));
        assert_eq!(strip.range(), 0..2);
        assert!(strip.reconcile_paths(&paths(&["x", "y", "z"]), &[], 0));
        assert_eq!(strip.range(), 0..0);
    }

    #[test]
    fn ten_thousand_photos_scroll_without_a_selection_or_centering() {
        let mut strip = window(10_000, 12);
        assert_eq!(strip.range(), 0..12);
        assert!(strip.scroll(5));
        assert_eq!(strip.range(), 5..17);
        assert!(!strip.synchronize(10_000, 12));
        assert_eq!(strip.range(), 5..17);
        assert!(strip.scroll(10_000));
        assert_eq!(strip.range(), 9_988..10_000);
        assert!(!strip.scroll(1));
        assert!(strip.scroll(-1));
        assert_eq!(strip.range(), 9_987..9_999);
    }

    #[test]
    fn reveal_moves_only_when_a_photo_crosses_a_visible_edge() {
        let mut strip = window(100, 8);
        strip.scroll(20);
        for index in 20..28 {
            assert!(!strip.reveal(index));
            assert_eq!(strip.range(), 20..28);
        }
        assert!(strip.reveal(28));
        assert_eq!(strip.range(), 21..29);
        assert!(strip.reveal(19));
        assert_eq!(strip.range(), 19..27);
        assert!(strip.reveal(87));
        assert_eq!(strip.range(), 80..88);
        assert!(strip.reveal(0));
        assert_eq!(strip.range(), 0..8);
        assert!(!strip.reveal(100));
        assert!(!strip.reveal(usize::MAX));
    }

    #[test]
    fn resize_preserves_start_except_when_clamped_by_the_end() {
        let mut strip = window(100, 10);
        strip.scroll(40);
        assert!(strip.synchronize(100, 15));
        assert_eq!(strip.range(), 40..55);
        assert!(strip.synchronize(100, 4));
        assert_eq!(strip.range(), 40..44);
        strip.scroll(100);
        assert_eq!(strip.range(), 96..100);
        assert!(strip.synchronize(100, 12));
        assert_eq!(strip.range(), 88..100);
        assert!(strip.synchronize(50, 12));
        assert_eq!(strip.range(), 38..50);
        // A larger folder need not visibly move an unchanged range.
        assert!(!strip.synchronize(200, 12));
        assert_eq!(strip.range(), 38..50);
    }

    #[test]
    fn small_empty_and_zero_capacity_windows_are_safe() {
        let mut strip = window(3, 12);
        assert_eq!(strip.range(), 0..3);
        for delta in [isize::MIN, -1, 0, 1, isize::MAX] {
            assert!(!strip.scroll(delta));
        }
        assert!(!strip.reveal(2));
        assert!(strip.synchronize(0, 12));
        assert_eq!(strip.range(), 0..0);
        assert!(!strip.reveal(0));
        assert!(!strip.scroll(isize::MAX));
        assert!(!strip.synchronize(100, 0));
        assert_eq!(strip.range(), 0..0);
        assert!(!strip.reveal(99));
        assert!(!strip.scroll(isize::MIN));
        assert!(strip.synchronize(100, 1));
        assert!(strip.reveal(99));
        assert_eq!(strip.range(), 99..100);
    }

    #[test]
    fn extreme_offsets_and_lengths_never_overflow() {
        let mut strip = window(usize::MAX, 24);
        assert!(strip.reveal(usize::MAX - 1));
        assert_eq!(strip.range(), usize::MAX - 24..usize::MAX);
        assert!(!strip.scroll(isize::MAX));
        assert!(strip.scroll(isize::MIN));
        assert!(strip.scroll(isize::MIN));
        assert_eq!(strip.range(), 0..24);
        assert!(strip.synchronize(usize::MAX, usize::MAX));
        assert_eq!(strip.range(), 0..usize::MAX);
        assert!(!strip.scroll(isize::MAX));
        assert!(!strip.reveal(usize::MAX - 1));
    }

    #[test]
    fn reset_retains_geometry_and_reports_only_visible_changes() {
        let mut strip = window(100, 8);
        assert!(!strip.reset());
        strip.scroll(10);
        assert!(strip.reset());
        assert_eq!(strip.range(), 0..8);
        assert!(!strip.reset());
        assert!(strip.reveal(99));
        assert_eq!(strip.range(), 92..100);
    }

    #[test]
    fn bounded_windows_always_stay_inside_the_folder() {
        for len in 0..32 {
            for capacity in 0..=24 {
                let mut strip = window(len, capacity);
                for delta in [isize::MIN, -5, 0, 3, isize::MAX] {
                    strip.scroll(delta);
                    let range = strip.range();
                    assert!(range.start <= range.end && range.end <= len);
                    assert_eq!(range.len(), capacity.min(len));
                    for index in 0..=len {
                        strip.reveal(index);
                        let range = strip.range();
                        assert!(range.start <= range.end && range.end <= len);
                        if index < len && capacity > 0 {
                            assert!(range.contains(&index));
                        }
                    }
                }
            }
        }
    }
}
