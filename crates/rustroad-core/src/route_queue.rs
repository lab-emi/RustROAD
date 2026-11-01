//! One bounded pending entry per detailed-search node, with decrease-key updates.
use crate::Result;
use crate::routing::{BuildFx, budget};
use std::cmp::Reverse;
use std::collections::HashMap;

pub(crate) type Priority = (u64, Reverse<u64>, u32);

pub(crate) struct Pending {
    heap: Vec<Priority>,
    positions: HashMap<u32, usize, BuildFx>,
    limit: usize,
}
impl Pending {
    pub fn new(limit: usize) -> Self {
        Self {
            heap: Vec::new(),
            positions: HashMap::with_hasher(BuildFx),
            limit,
        }
    }

    pub fn offer(&mut self, key: Priority) -> Result<()> {
        if let Some(&at) = self.positions.get(&key.2) {
            let previous = self.heap[at];
            self.heap[at] = key;
            if key < previous {
                self.up(at);
            } else if key > previous {
                // A lower path cost can still increase the tie-break key
                // when the total estimate has saturated at u64::MAX.
                self.down(at);
            }
            return Ok(());
        }
        if self.heap.len() >= self.limit {
            return Err(budget("routing frontier unique-node limit"));
        }
        self.heap
            .try_reserve(1)
            .map_err(|_| budget("routing frontier allocation"))?;
        self.positions
            .try_reserve(1)
            .map_err(|_| budget("routing frontier position allocation"))?;
        let at = self.heap.len();
        self.heap.push(key);
        self.positions.insert(key.2, at);
        self.up(at);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Priority> {
        let first = *self.heap.first()?;
        let last = self.heap.pop().expect("frontier is not empty");
        self.positions.remove(&first.2);
        if !self.heap.is_empty() {
            self.heap[0] = last;
            self.positions.insert(last.2, 0);
            self.down(0);
        }
        Some(first)
    }

    fn swap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.positions.insert(self.heap[a].2, a);
        self.positions.insert(self.heap[b].2, b);
    }
    fn up(&mut self, mut at: usize) {
        while at > 0 {
            let parent = (at - 1) / 2;
            if self.heap[parent] <= self.heap[at] {
                break;
            }
            self.swap(at, parent);
            at = parent;
        }
    }
    fn down(&mut self, mut at: usize) {
        while let Some(left) = at
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .filter(|&n| n < self.heap.len())
        {
            let right = left + 1;
            let child = if right < self.heap.len() && self.heap[right] < self.heap[left] {
                right
            } else {
                left
            };
            if self.heap[at] <= self.heap[child] {
                break;
            }
            self.swap(at, child);
            at = child;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn updated_priorities_match_an_independent_ordered_map() {
        let mut queue = Pending::new(1000);
        let mut reference = BTreeMap::<u32, Priority>::new();
        let mut random = 0x19fcu32;
        for step in 0..20_000 {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            let id = (random >> 12) % 1000;
            let cost = u64::from(random & 65535);
            let key = (cost, Reverse(cost / 2), id);
            reference.insert(id, key);
            queue.offer(key).unwrap();
            if step % 3 == 0 {
                let expected = reference.values().min().copied();
                assert_eq!(queue.pop(), expected);
                reference.remove(&expected.unwrap().2);
            }
            assert_eq!(queue.heap.len(), reference.len());
        }
        while let Some(expected) = reference.values().min().copied() {
            assert_eq!(queue.pop(), Some(expected));
            reference.remove(&expected.2);
        }
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn decreases_at_capacity_do_not_add_entries_and_rejection_is_transactional() {
        let mut queue = Pending::new(1);
        queue.offer((100, Reverse(5), 7)).unwrap();
        for cost in (0..100).rev() {
            queue.offer((cost, Reverse(cost), 7)).unwrap();
        }
        assert_eq!(queue.heap.len(), 1);
        assert!(queue.offer((0, Reverse(0), 8)).is_err());
        assert_eq!(queue.pop(), Some((0, Reverse(0), 7)));
        queue.offer((20, Reverse(20), 7)).unwrap();
        assert_eq!(queue.pop(), Some((20, Reverse(20), 7)));
    }

    #[test]
    fn saturated_estimates_keep_the_latest_lower_path_cost() {
        let mut queue = Pending::new(2);
        queue.offer((u64::MAX, Reverse(100), 1)).unwrap();
        queue.offer((u64::MAX, Reverse(60), 2)).unwrap();
        queue.offer((u64::MAX, Reverse(50), 1)).unwrap();
        assert_eq!(queue.pop(), Some((u64::MAX, Reverse(60), 2)));
        assert_eq!(queue.pop(), Some((u64::MAX, Reverse(50), 1)));
    }
}
