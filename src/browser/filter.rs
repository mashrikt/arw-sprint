use std::path::{Path, PathBuf};

/// Unknown ratings stay out of filtered views until their sidecars are read.
/// A failed sidecar read is unknown, not an implicit zero-star rating.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PhotoFilter {
    #[default]
    All,
    Stars(u8),
    Rated,
    Unrated,
    Rejected,
    NotRejected,
}

impl PhotoFilter {
    pub fn matches(self, rating: Option<i8>) -> bool {
        match (self, rating) {
            (Self::All, _) => true,
            (Self::Stars(stars), Some(rating)) => (1..=5).contains(&stars) && rating == stars as i8,
            (Self::Rated, Some(1..=5))
            | (Self::Unrated, Some(0))
            | (Self::Rejected, Some(-1))
            | (Self::NotRejected, Some(0..=5)) => true,
            _ => false,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All photos",
            Self::Stars(1) => "1 star only",
            Self::Stars(2) => "2 stars only",
            Self::Stars(3) => "3 stars only",
            Self::Stars(4) => "4 stars only",
            Self::Stars(5) => "5 stars only",
            Self::Stars(_) => "Invalid star filter",
            Self::Rated => "Rated photos",
            Self::Unrated => "Unrated photos",
            Self::Rejected => "Rejected photos",
            Self::NotRejected => "Not rejected",
        }
    }
}

/// Preserve the existing natural filename order. `rating` should consult the
/// in-memory metadata state; sidecar I/O belongs to the metadata worker.
pub fn select_visible(
    all: &[PathBuf],
    filter: PhotoFilter,
    rating: impl Fn(&Path) -> Option<i8>,
) -> Vec<PathBuf> {
    all.iter()
        .filter(|path| filter == PhotoFilter::All || filter.matches(rating(path.as_path())))
        .cloned()
        .collect()
}

/// Preserve the selected path across filtering. If it disappears, the item
/// sliding into its old position becomes current, clamped at the end.
pub fn choose_selection(
    visible: &[PathBuf],
    previous: Option<&Path>,
    old_visible_index: Option<usize>,
) -> Option<usize> {
    if visible.is_empty() {
        return None;
    }
    if let Some(index) =
        previous.and_then(|previous| visible.iter().position(|path| path == previous))
    {
        return Some(index);
    }
    Some(old_visible_index.unwrap_or(0).min(visible.len() - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_matrix_distinguishes_unknown_zero_and_rejected() {
        let ratings = [
            None,
            Some(-2),
            Some(-1),
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(6),
        ];
        for (filter, included) in [
            (PhotoFilter::All, vec![true; ratings.len()]),
            (
                PhotoFilter::Rated,
                vec![
                    false, false, false, false, true, true, true, true, true, false,
                ],
            ),
            (
                PhotoFilter::Unrated,
                vec![
                    false, false, false, true, false, false, false, false, false, false,
                ],
            ),
            (
                PhotoFilter::Rejected,
                vec![
                    false, false, true, false, false, false, false, false, false, false,
                ],
            ),
            (
                PhotoFilter::NotRejected,
                vec![
                    false, false, false, true, true, true, true, true, true, false,
                ],
            ),
        ] {
            for (rating, expected) in ratings.into_iter().zip(included) {
                assert_eq!(filter.matches(rating), expected, "{filter:?}, {rating:?}");
            }
        }
        assert_eq!(PhotoFilter::default(), PhotoFilter::All);
    }

    #[test]
    fn stars_are_exact_and_invalid_star_filters_match_nothing() {
        for stars in 1..=5 {
            let filter = PhotoFilter::Stars(stars);
            assert!(!filter.matches(None));
            for rating in -2..=6 {
                assert_eq!(filter.matches(Some(rating)), rating == stars as i8);
            }
        }
        for stars in [0, 6, 128, 255] {
            for rating in [None, Some(-1), Some(0), Some(5)] {
                assert!(!PhotoFilter::Stars(stars).matches(rating));
            }
        }
        assert_eq!(PhotoFilter::Stars(5).label(), "5 stars only");
        assert_eq!(PhotoFilter::Stars(1).label(), "1 star only");
    }

    #[test]
    fn filtering_preserves_order_and_excludes_missing_or_failed_reads() {
        let all: Vec<PathBuf> = ["DSC1.ARW", "DSC2.ARW", "DSC10.ARW", "DSC20.ARW"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let visible = select_visible(&all, PhotoFilter::Rated, |path| {
            match path.file_name().and_then(|name| name.to_str()) {
                Some("DSC1.ARW") => Some(5),
                Some("DSC10.ARW") => Some(1),
                _ => None, // Unread or failed sidecar.
            }
        });
        assert_eq!(visible, [all[0].clone(), all[2].clone()]);
        assert_eq!(
            select_visible(&all, PhotoFilter::All, |_| panic!(
                "All must not require rating lookup"
            )),
            all
        );
    }

    #[test]
    fn selection_preserves_path_then_uses_the_vacated_position() {
        let visible: Vec<PathBuf> = ["a.ARW", "c.ARW", "d.ARW"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(
            choose_selection(&visible, Some(Path::new("c.ARW")), Some(2)),
            Some(1)
        );
        assert_eq!(
            choose_selection(&visible, Some(Path::new("b.ARW")), Some(1)),
            Some(1)
        );
        assert_eq!(
            choose_selection(&visible, Some(Path::new("e.ARW")), Some(3)),
            Some(2)
        );
        assert_eq!(choose_selection(&visible, None, None), Some(0));
        assert_eq!(choose_selection(&visible, None, Some(usize::MAX)), Some(2));
        assert_eq!(
            choose_selection(&[], Some(Path::new("a.ARW")), Some(0)),
            None
        );
    }
}
