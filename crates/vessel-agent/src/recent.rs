//! [`Recent`]: a set that remembers only its newest entries.
//!
//! The driver remembers which tool calls it announced and which permission
//! requests were settled, for answers like "already settled". A session
//! can run for days, so those sets must not grow with it; forgetting the
//! oldest entries costs almost nothing (a very late answer is told "not
//! found" instead of "already settled"; a tool call from long ago may be
//! announced again).

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Entries kept by default: far more than one turn produces.
pub(crate) const KEEP: usize = 1024;

/// A set of at most `capacity` entries; inserting beyond that forgets the
/// oldest.
#[derive(Debug)]
pub(crate) struct Recent<T> {
    members: HashSet<T>,
    /// Insertion order, oldest first, for forgetting.
    order: VecDeque<T>,
    capacity: usize,
}

impl<T: Clone + Eq + Hash> Recent<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            members: HashSet::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Add `value`; `false` if it was already there.
    pub(crate) fn insert(&mut self, value: T) -> bool {
        if self.members.contains(&value) {
            return false;
        }
        if self.order.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            let _ = self.members.remove(&oldest);
        }
        self.order.push_back(value.clone());
        self.members.insert(value)
    }

    pub(crate) fn contains(&self, value: &T) -> bool {
        self.members.contains(value)
    }
}

impl<T: Clone + Eq + Hash> Default for Recent<T> {
    fn default() -> Self {
        Self::new(KEEP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_newest_entries() {
        let mut recent = Recent::new(2);
        assert!(recent.insert(1));
        assert!(!recent.insert(1), "already there");
        assert!(recent.insert(2));
        assert!(recent.insert(3));
        assert!(!recent.contains(&1), "the oldest is forgotten");
        assert!(recent.contains(&2) && recent.contains(&3));
        assert!(recent.insert(1), "a forgotten entry can come back");
    }
}
