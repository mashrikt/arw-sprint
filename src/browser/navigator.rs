use super::prefetch::Direction;

#[derive(Clone, Debug, Default)]
pub struct Navigator {
    len: usize,
    index: Option<usize>,
    direction: Direction,
}

impl Navigator {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            index: (len != 0).then_some(0),
            direction: Direction::Forward,
        }
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        self.index = if len == 0 {
            None
        } else {
            Some(self.index.unwrap_or(0).min(len - 1))
        };
    }

    pub fn seek(&mut self, index: usize) -> bool {
        if index >= self.len || self.index == Some(index) {
            return false;
        }
        self.direction = if index < self.index.unwrap_or(0) {
            Direction::Backward
        } else {
            Direction::Forward
        };
        self.index = Some(index);
        true
    }

    pub fn advance(&mut self, delta: isize) -> bool {
        let Some(index) = self.index else {
            return false;
        };
        let target = if delta >= 0 {
            index.saturating_add(delta as usize).min(self.len - 1)
        } else {
            index.saturating_sub(delta.unsigned_abs())
        };
        self.seek(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_clamps_and_empty_folders_are_safe() {
        let mut navigator = Navigator::new(3);
        assert!(!navigator.advance(-1));
        assert!(navigator.advance(100));
        assert_eq!(navigator.index(), Some(2));
        assert!(!navigator.advance(1));
        assert!(navigator.advance(isize::MIN));
        assert_eq!(navigator.index(), Some(0));
        assert_eq!(navigator.direction(), Direction::Backward);
        navigator.set_len(0);
        assert!(!navigator.advance(1));
        assert_eq!(navigator.index(), None);
        navigator.set_len(5);
        assert_eq!(navigator.index(), Some(0));
        assert!(!navigator.seek(5));
    }
}
