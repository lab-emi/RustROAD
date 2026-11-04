//! One bounded pending entry per detailed-search node, with decrease-key updates.
//!
//! Queue positions live in the node's search record (`Search::position`), so
//! sift steps update an array slot instead of a hash map. Every queued node
//! has a current search record: callers `Search::set` a node before offering it.
use crate::Result;
use crate::route_search::Search;
use crate::routing::budget;
use std::cmp::Reverse;

pub(crate) type Priority = (u64, Reverse<u64>, u32);

pub(crate) struct Pending {
    heap: Vec<Priority>,
    limit: usize,
}
impl Pending {
    pub fn new(limit: usize) -> Self {
        Self {
            heap: Vec::new(),
            limit,
        }
    }

    pub fn offer(&mut self, key: Priority, search: &mut Search) -> Result<()> {
        if let Some(at) = search.position(key.2) {
            let previous = self.heap[at];
            self.heap[at] = key;
            if key < previous {
                self.up(at, search);
            } else if key > previous {
                // A lower path cost can still increase the tie-break key
                // when the total estimate has saturated at u64::MAX.
                self.down(at, search);
            }
            return Ok(());
        }
        if self.heap.len() >= self.limit {
            return Err(budget("routing frontier unique-node limit"));
        }
        self.heap
            .try_reserve(1)
            .map_err(|_| budget("routing frontier allocation"))?;
        let at = self.heap.len();
        self.heap.push(key);
        search.set_position(key.2, Some(at));
        self.up(at, search);
        Ok(())
    }

    pub fn pop(&mut self, search: &mut Search) -> Option<Priority> {
        let first = *self.heap.first()?;
        let last = self.heap.pop().expect("frontier is not empty");
        search.set_position(first.2, None);
        if !self.heap.is_empty() {
            self.heap[0] = last;
            search.set_position(last.2, Some(0));
            self.down(0, search);
        }
        Some(first)
    }

    fn swap(&mut self, a: usize, b: usize, search: &mut Search) {
        self.heap.swap(a, b);
        search.set_position(self.heap[a].2, Some(a));
        search.set_position(self.heap[b].2, Some(b));
    }
    fn up(&mut self, mut at: usize, search: &mut Search) {
        while at > 0 {
            let parent = (at - 1) / 2;
            if self.heap[parent] <= self.heap[at] {
                break;
            }
            self.swap(at, parent, search);
            at = parent;
        }
    }
    fn down(&mut self, mut at: usize, search: &mut Search) {
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
            self.swap(at, child, search);
            at = child;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route_search::State;
    use std::collections::BTreeMap;

    fn search(nodes: usize) -> Search {
        let mut search = Search::new(nodes, nodes.min(1 << 20)).unwrap();
        search.begin(1);
        search
    }
    fn offer(queue: &mut Pending, search: &mut Search, key: Priority) -> Result<()> {
        if search.get(key.2).is_none() {
            search.set(
                key.2,
                State {
                    cost: key.0,
                    parent: u32::MAX,
                    kind: u32::MAX,
                },
            )?;
        }
        queue.offer(key, search)
    }

    #[test]
    fn updated_priorities_match_an_independent_ordered_map() {
        // Dense and paged search records hold the queue positions alike.
        for nodes in [1000, 3_000_000] {
            let mut search = search(nodes);
            let mut queue = Pending::new(1000);
            let mut reference = BTreeMap::<u32, Priority>::new();
            let mut random = 0x19fcu32;
            for step in 0..20_000 {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                let id = (random >> 12) % 1000 * (nodes as u32 / 1000);
                let cost = u64::from(random & 65535);
                let key = (cost, Reverse(cost / 2), id);
                reference.insert(id, key);
                offer(&mut queue, &mut search, key).unwrap();
                if step % 3 == 0 {
                    let expected = reference.values().min().copied();
                    assert_eq!(queue.pop(&mut search), expected);
                    reference.remove(&expected.unwrap().2);
                }
                assert_eq!(queue.heap.len(), reference.len());
            }
            while let Some(expected) = reference.values().min().copied() {
                assert_eq!(queue.pop(&mut search), Some(expected));
                reference.remove(&expected.2);
            }
            assert_eq!(queue.pop(&mut search), None);
        }
    }

    #[test]
    fn decreases_at_capacity_do_not_add_entries_and_rejection_is_transactional() {
        let mut search = search(16);
        let mut queue = Pending::new(1);
        offer(&mut queue, &mut search, (100, Reverse(5), 7)).unwrap();
        for cost in (0..100).rev() {
            offer(&mut queue, &mut search, (cost, Reverse(cost), 7)).unwrap();
        }
        assert_eq!(queue.heap.len(), 1);
        assert!(offer(&mut queue, &mut search, (0, Reverse(0), 8)).is_err());
        assert_eq!(queue.pop(&mut search), Some((0, Reverse(0), 7)));
        offer(&mut queue, &mut search, (20, Reverse(20), 7)).unwrap();
        assert_eq!(queue.pop(&mut search), Some((20, Reverse(20), 7)));
    }

    #[test]
    fn saturated_estimates_keep_the_latest_lower_path_cost() {
        let mut search = search(16);
        let mut queue = Pending::new(2);
        offer(&mut queue, &mut search, (u64::MAX, Reverse(100), 1)).unwrap();
        offer(&mut queue, &mut search, (u64::MAX, Reverse(60), 2)).unwrap();
        offer(&mut queue, &mut search, (u64::MAX, Reverse(50), 1)).unwrap();
        assert_eq!(queue.pop(&mut search), Some((u64::MAX, Reverse(60), 2)));
        assert_eq!(queue.pop(&mut search), Some((u64::MAX, Reverse(50), 1)));
    }

    #[test]
    fn a_new_search_epoch_forgets_queue_positions() {
        for nodes in [16, 3_000_000] {
            let mut search = search(nodes);
            let mut queue = Pending::new(4);
            offer(&mut queue, &mut search, (5, Reverse(5), 3)).unwrap();
            assert_eq!(search.position(3), Some(0));
            search.begin(2);
            assert_eq!(search.position(3), None);
            let mut queue = Pending::new(4);
            offer(&mut queue, &mut search, (9, Reverse(9), 3)).unwrap();
            assert_eq!(queue.pop(&mut search), Some((9, Reverse(9), 3)));
        }
    }
}
