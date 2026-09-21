#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Direction {
    #[default]
    Forward,
    Backward,
}

/// Ordered speculation, nearest in the current direction first. Arithmetic is
/// bounded even for a virtual list with usize::MAX entries.
pub fn plan(
    index: usize,
    len: usize,
    direction: Direction,
    ahead: usize,
    behind: usize,
) -> Vec<usize> {
    if index >= len {
        return Vec::new();
    }
    let ahead = ahead.min(64);
    let behind = behind.min(64);
    let mut result = Vec::with_capacity(ahead + behind);
    for (forward, count) in [
        (direction == Direction::Forward, ahead),
        (direction != Direction::Forward, behind),
    ] {
        for distance in 1..=count {
            let candidate = if forward {
                index.checked_add(distance)
            } else {
                index.checked_sub(distance)
            };
            if let Some(candidate) = candidate.filter(|candidate| *candidate < len) {
                result.push(candidate);
            } else {
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directional_windows_reverse_and_clip() {
        assert_eq!(plan(4, 12, Direction::Forward, 3, 2), [5, 6, 7, 3, 2]);
        assert_eq!(plan(4, 12, Direction::Backward, 3, 2), [3, 2, 1, 5, 6]);
        assert_eq!(plan(0, 3, Direction::Backward, 8, 3), [1, 2]);
        assert!(plan(0, 0, Direction::Forward, 8, 3).is_empty());
        assert_eq!(
            plan(usize::MAX - 1, usize::MAX, Direction::Forward, 8, 1),
            [usize::MAX - 2]
        );
    }
}
