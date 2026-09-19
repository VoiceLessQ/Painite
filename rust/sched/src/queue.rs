use std::collections::BTreeMap;

use crate::scheduler::Job;

/// Priority key: lower ticket level first (closer to a player), then
/// submission order. Fully ordered, so two runs with the same request
/// stream scan waiters in the same order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Key {
    pub level: i32,
    pub seq: u64,
}

#[derive(Default, Debug)]
pub struct WaitQueue {
    waiting: BTreeMap<Key, Job>,
}

impl WaitQueue {
    pub fn push(&mut self, key: Key, job: Job) {
        let prev = self.waiting.insert(key, job);
        debug_assert!(prev.is_none(), "duplicate seq {}", key.seq);
    }

    pub fn remove(&mut self, key: &Key) -> Option<Job> {
        self.waiting.remove(key)
    }

    /// Waiters in priority order.
    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Job)> {
        self.waiting.iter()
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }
}
