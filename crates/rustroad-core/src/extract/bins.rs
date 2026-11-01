//! Compact insertion-ordered lists for extraction spatial bins.
use crate::Result;
use std::collections::HashMap;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Header {
    first: u32,
    last: u32,
    count: u32,
}
impl Default for Header {
    fn default() -> Self {
        Self {
            first: NONE,
            last: NONE,
            count: 0,
        }
    }
}

#[derive(Default)]
pub(super) struct BinLists {
    headers: HashMap<(u32, i64, i64), Header>,
    entries: Vec<[u32; 2]>,
}

pub(super) struct Iter<'a> {
    entries: &'a [[u32; 2]],
    at: u32,
    left: usize,
}
impl Iterator for Iter<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            return None;
        }
        let [id, next] = self.entries[self.at as usize];
        self.at = next;
        self.left -= 1;
        Some(id)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}
impl ExactSizeIterator for Iter<'_> {}

impl BinLists {
    pub fn push(&mut self, key: (u32, i64, i64), id: u32, limit: usize) -> Result<()> {
        if self.entries.len() >= limit || self.entries.len() >= NONE as usize {
            return Err(super::budget("extraction spatial-bin entries"));
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| super::budget("extraction bin entry allocation"))?;
        self.headers
            .try_reserve(1)
            .map_err(|_| super::budget("extraction bin header allocation"))?;
        let next = self.entries.len() as u32;
        let header = self.headers.entry(key).or_default();
        if header.last == NONE {
            header.first = next;
        } else {
            self.entries[header.last as usize][1] = next;
        }
        header.last = next;
        header.count += 1;
        self.entries.push([id, NONE]);
        Ok(())
    }
    pub fn iter(&self, key: (u32, i64, i64)) -> Iter<'_> {
        let h = self.headers.get(&key).copied().unwrap_or_default();
        Iter {
            entries: &self.entries,
            at: h.first,
            left: h.count as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interleaved_bins_keep_exact_order_and_counts_under_a_hard_cap() {
        let mut bins = BinLists::default();
        let mut reference = HashMap::<_, Vec<_>>::new();
        for i in 0..100_001u32 {
            let key = (i % 5, (i % 97) as i64 - 50, (i % 13) as i64 - 7);
            bins.push(key, i, 100_001).unwrap();
            reference.entry(key).or_default().push(i);
        }
        for (key, values) in reference {
            let mut actual = bins.iter(key);
            assert_eq!(actual.len(), values.len());
            assert_eq!(actual.next(), values.first().copied());
            assert_eq!(actual.len(), values.len() - 1);
            assert_eq!(actual.collect::<Vec<_>>(), values[1..]);
        }
        assert_eq!(bins.iter((9, -100, 200)).count(), 0);
        assert!(bins.push((0, 0, 0), 999, 100_001).is_err());
        assert_eq!(bins.entries.len(), 100_001);
    }
}
