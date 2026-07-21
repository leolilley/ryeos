#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Focus {
    current: usize,
    count: usize,
}

impl Focus {
    pub(crate) const fn new(count: usize) -> Self {
        Self { current: 0, count }
    }

    pub(crate) const fn current(&self) -> usize {
        self.current
    }

    pub(crate) fn set_count(&mut self, count: usize) {
        self.count = count;
        self.current = self.current.min(count.saturating_sub(1));
    }

    pub(crate) fn next(&mut self) {
        if self.count > 0 {
            self.current = (self.current + 1) % self.count;
        }
    }

    pub(crate) fn previous(&mut self) {
        if self.count > 0 {
            self.current = (self.current + self.count - 1) % self.count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_wraps_and_clamps_when_fields_disappear() {
        let mut focus = Focus::new(3);
        focus.previous();
        assert_eq!(focus.current(), 2);
        focus.next();
        assert_eq!(focus.current(), 0);
        focus.previous();
        focus.set_count(1);
        assert_eq!(focus.current(), 0);
    }
}
