//! Bounded detailed-router scratch, independent of a large implicit track grid.
use crate::routing::BuildFx;
use crate::{Error, Result};
use std::collections::HashMap;

pub(crate) const DENSE_NODES: usize = 1_000_000;
const NONE: u32 = u32::MAX;
/// Heap position of a node outside the pending queue (31-bit field).
const NO_POSITION: u32 = 0x7fff_ffff;
const CLOSED: u32 = 0x8000_0000;

#[derive(Clone, Copy)]
pub(crate) struct State {
    pub cost: u64,
    pub parent: u32,
    pub kind: u32,
}
/// One search record: the state, its search epoch, and in `flags` the
/// closed bit (bit 31) plus the node's position in the pending queue.
#[derive(Clone, Copy)]
struct Tagged {
    state: State,
    epoch: u32,
    flags: u32,
}
impl Tagged {
    fn closed(&self) -> bool {
        self.flags & CLOSED != 0
    }
}
const EMPTY: Tagged = Tagged {
    state: State {
        cost: u64::MAX,
        parent: NONE,
        kind: NONE,
    },
    epoch: 0,
    flags: NO_POSITION,
};

/// Nodes per page of the sparse search records.
const PAGE_BITS: u32 = 9;
const PAGE: usize = 1 << PAGE_BITS;
/// Direct page tables up to 2^31 nodes (at most 16 MiB, grown on demand).
const DIRECT_PAGES: usize = 1 << (31 - PAGE_BITS);
/// Page storage one search may hold, per record of its distinct-node limit:
/// half of the 64 bytes per record the route memory estimate charges for
/// sparse search records, so scattered records (one per page, 12 KiB each)
/// cannot outgrow the estimate.
const POOL_BYTES_PER_RECORD: usize = 32;
const PAGE_BYTES: usize = PAGE * std::mem::size_of::<Tagged>();

/// Pages one search may map: the pool budget for `limit` records (at least
/// one page), never more than the graph has.
fn page_budget(nodes: usize, limit: usize) -> usize {
    (limit.saturating_mul(POOL_BYTES_PER_RECORD) / PAGE_BYTES)
        .max(1)
        .min(nodes.div_ceil(PAGE))
}

enum PageTable {
    Direct(Vec<u32>),
    Hashed(HashMap<u32, u32, BuildFx>),
}

/// Sparse search records of a large implicit graph: epoch-tagged pages of
/// `PAGE` consecutive node ids, mapped on first touch. A search maps at most
/// the pages it touches, up to `max_pages` (`page_budget`); records on
/// further pages of that search go to a hash map, the former layout, so a
/// search whose records scatter over many pages stays within the memory
/// estimate. `begin` unmaps the pages and keeps their storage for the next
/// search, whose fresh epoch invalidates every stale record without a clear.
/// Same semantics as the former hash map: one record per node, the
/// distinct-node count bounded by `limit`.
struct Paged {
    table: PageTable,
    /// Page storage; slots `0..used` hold the pages mapped in this epoch.
    pool: Vec<Tagged>,
    /// Page index of every mapped slot (to unmap on `begin`).
    mapped: Vec<u32>,
    used: usize,
    max_pages: usize,
    /// Records of this search on pages beyond `max_pages` (an id's page is
    /// either mapped or not for the whole search once overflow starts).
    overflow: HashMap<u32, Tagged, BuildFx>,
    count: usize,
    epoch: u32,
}
impl Paged {
    fn new(nodes: usize, limit: usize) -> Self {
        Self {
            table: if nodes.div_ceil(PAGE) <= DIRECT_PAGES {
                PageTable::Direct(Vec::new())
            } else {
                PageTable::Hashed(HashMap::with_hasher(BuildFx))
            },
            pool: Vec::new(),
            mapped: Vec::new(),
            used: 0,
            max_pages: page_budget(nodes, limit),
            overflow: HashMap::with_hasher(BuildFx),
            count: 0,
            epoch: 1,
        }
    }
    fn begin(&mut self) {
        match &mut self.table {
            PageTable::Direct(table) => {
                for &page in &self.mapped {
                    table[page as usize] = NONE;
                }
            }
            PageTable::Hashed(table) => table.clear(),
        }
        self.mapped.clear();
        self.overflow.clear();
        self.used = 0;
        self.count = 0;
        if self.epoch == u32::MAX {
            self.pool.fill(EMPTY);
            self.epoch = 1;
        } else {
            self.epoch += 1;
        }
    }
    #[inline]
    fn slot(&self, id: u32) -> Option<usize> {
        let page = id >> PAGE_BITS;
        let slot = match &self.table {
            PageTable::Direct(table) => *table.get(page as usize)?,
            PageTable::Hashed(table) => *table.get(&page)?,
        };
        (slot != NONE).then_some(((slot as usize) << PAGE_BITS) | (id as usize & (PAGE - 1)))
    }
    #[inline]
    fn current(&self, id: u32) -> Option<&Tagged> {
        match self.slot(id) {
            Some(at) => {
                let r = &self.pool[at];
                (r.epoch == self.epoch).then_some(r)
            }
            None if self.overflow.is_empty() => None,
            None => self.overflow.get(&id),
        }
    }
    #[inline]
    fn current_mut(&mut self, id: u32) -> Option<&mut Tagged> {
        match self.slot(id) {
            Some(at) => {
                let epoch = self.epoch;
                let r = &mut self.pool[at];
                (r.epoch == epoch).then_some(r)
            }
            None if self.overflow.is_empty() => None,
            None => self.overflow.get_mut(&id),
        }
    }
    /// Store a new record of this search: on its mapped page, on a newly
    /// mapped page while the budget allows, else in the overflow map.
    fn insert(&mut self, id: u32, record: Tagged) -> Result<()> {
        let at = match self.slot(id) {
            Some(at) => at,
            None if self.used < self.max_pages => self.map(id)? | (id as usize & (PAGE - 1)),
            None => {
                self.overflow
                    .try_reserve(1)
                    .map_err(|_| Error::Budget("routing visited-state allocation".into()))?;
                self.overflow.insert(id, record);
                return Ok(());
            }
        };
        self.pool[at] = record;
        Ok(())
    }
    /// Map the page of `id` (allocating storage only beyond earlier peaks).
    fn map(&mut self, id: u32) -> Result<usize> {
        let page = id >> PAGE_BITS;
        let fail = || Error::Budget("routing visited-state allocation".into());
        let slot = u32::try_from(self.used)
            .ok()
            .filter(|&s| s != NONE)
            .ok_or_else(fail)?;
        match &mut self.table {
            PageTable::Direct(table) => {
                if table.len() <= page as usize {
                    table
                        .try_reserve(page as usize + 1 - table.len())
                        .map_err(|_| fail())?;
                    table.resize(page as usize + 1, NONE);
                }
                table[page as usize] = slot;
            }
            PageTable::Hashed(table) => {
                table.try_reserve(1).map_err(|_| fail())?;
                table.insert(page, slot);
            }
        }
        self.mapped.try_reserve(1).map_err(|_| fail())?;
        self.mapped.push(page);
        let need = (self.used + 1) * PAGE;
        if self.pool.len() < need {
            if self.pool.capacity() < need {
                // Amortized growth, capped at the page budget.
                let target = self
                    .pool
                    .capacity()
                    .saturating_mul(2)
                    .max(need)
                    .min(self.max_pages * PAGE);
                self.pool
                    .try_reserve_exact(target - self.pool.len())
                    .map_err(|_| fail())?;
            }
            self.pool.resize(need, EMPTY);
        }
        self.used += 1;
        Ok((slot as usize) << PAGE_BITS)
    }
    /// Current-epoch records (paged ones, then the overflow; unordered).
    fn records(&self) -> impl Iterator<Item = (u32, &Tagged)> {
        self.mapped
            .iter()
            .enumerate()
            .flat_map(move |(slot, &page)| {
                self.pool[slot * PAGE..(slot + 1) * PAGE]
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.epoch == self.epoch)
                    .map(move |(i, r)| ((page << PAGE_BITS) | i as u32, r))
            })
            .chain(self.overflow.iter().map(|(&id, r)| (id, r)))
    }
}

enum Records {
    Dense(Vec<Tagged>),
    Sparse(Paged),
}
pub(crate) struct Search {
    records: Records,
    epoch: u32,
    limit: usize,
}
impl Search {
    pub(crate) fn current_count(&self) -> usize {
        match &self.records {
            Records::Dense(records) => records.iter().filter(|r| r.epoch == self.epoch).count(),
            Records::Sparse(records) => records.count,
        }
    }
    /// Bounded diagnostic of current-epoch discovered states, never all paths.
    pub(crate) fn capture_bounded(&self, maximum: usize) -> Result<(Vec<(u32, State)>, usize)> {
        if maximum == 0 || maximum > 4096 {
            return Err(Error::Budget("search trace node bound".into()));
        }
        let (count, scans) = match &self.records {
            Records::Dense(records) => (
                records.iter().filter(|r| r.epoch == self.epoch).count(),
                records.len().saturating_mul(2),
            ),
            Records::Sparse(records) => (records.count, records.count),
        };
        if count > maximum {
            return Err(Error::Budget(
                "search trace discovered-node cap exceeded".into(),
            ));
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(count)
            .map_err(|_| Error::Budget("search trace allocation".into()))?;
        match &self.records {
            Records::Dense(records) => {
                result.extend(
                    records.iter().enumerate().filter_map(|(id, r)| {
                        (r.epoch == self.epoch).then_some((id as u32, r.state))
                    }),
                );
            }
            Records::Sparse(records) => {
                result.extend(records.records().map(|(id, r)| (id, r.state)));
            }
        }
        result.sort_unstable_by_key(|&(id, _)| id);
        let sort_work = count.saturating_mul(count.max(1).ilog2() as usize + 1);
        Ok((result, scans.saturating_add(sort_work)))
    }

    pub fn new(nodes: usize, limit: usize) -> Result<Self> {
        let records = if nodes <= DENSE_NODES {
            let mut values = Vec::new();
            values
                .try_reserve_exact(nodes)
                .map_err(|_| Error::Budget("routing search allocation".into()))?;
            values.resize(nodes, EMPTY);
            Records::Dense(values)
        } else {
            Records::Sparse(Paged::new(nodes, limit))
        };
        Ok(Self {
            records,
            epoch: 0,
            limit,
        })
    }
    pub fn begin(&mut self, epoch: u32) {
        self.epoch = epoch;
        if let Records::Sparse(records) = &mut self.records {
            records.begin();
        }
    }
    #[inline]
    fn current(&self, id: u32) -> Option<&Tagged> {
        match &self.records {
            Records::Dense(records) => records.get(id as usize).filter(|r| r.epoch == self.epoch),
            Records::Sparse(records) => records.current(id),
        }
    }
    #[inline]
    fn current_mut(&mut self, id: u32) -> Option<&mut Tagged> {
        match &mut self.records {
            Records::Dense(records) => records
                .get_mut(id as usize)
                .filter(|r| r.epoch == self.epoch),
            Records::Sparse(records) => records.current_mut(id),
        }
    }
    pub fn get(&self, id: u32) -> Option<State> {
        self.current(id).map(|r| r.state)
    }
    pub fn is_closed(&self, id: u32) -> bool {
        self.current(id).is_some_and(Tagged::closed)
    }
    pub fn close(&mut self, id: u32) {
        if let Some(r) = self.current_mut(id) {
            r.flags |= CLOSED;
        }
    }
    /// Store `state` (open). A node already queued keeps its queue position.
    pub fn set(&mut self, id: u32, state: State) -> Result<()> {
        let epoch = self.epoch;
        match &mut self.records {
            Records::Dense(records) => {
                let slot = records
                    .get_mut(id as usize)
                    .ok_or_else(|| Error::Invalid("routing search node outside graph".into()))?;
                let position = if slot.epoch == epoch {
                    slot.flags & !CLOSED
                } else {
                    NO_POSITION
                };
                *slot = Tagged {
                    state,
                    epoch,
                    flags: position,
                };
            }
            Records::Sparse(records) => {
                if let Some(r) = records.current_mut(id) {
                    r.state = state;
                    r.flags &= !CLOSED;
                    return Ok(());
                }
                if records.count >= self.limit {
                    return Err(Error::Budget("routing visited-state limit".into()));
                }
                let record = Tagged {
                    state,
                    epoch: records.epoch,
                    flags: NO_POSITION,
                };
                records.insert(id, record)?;
                records.count += 1;
            }
        }
        Ok(())
    }
    /// Pending-queue position of a node with a current record.
    #[inline]
    pub(crate) fn position(&self, id: u32) -> Option<usize> {
        self.current(id)
            .map(|r| r.flags & !CLOSED)
            .filter(|&p| p != NO_POSITION)
            .map(|p| p as usize)
    }
    /// Record (or clear, with `None`) the queue position of a node with a
    /// current record; the queue only holds nodes whose state was set.
    #[inline]
    pub(crate) fn set_position(&mut self, id: u32, position: Option<usize>) {
        let value = position.map_or(NO_POSITION, |p| p as u32);
        debug_assert!(position.is_none_or(|p| p < NO_POSITION as usize));
        let record = self.current_mut(id);
        debug_assert!(record.is_some(), "queued node without a search record");
        if let Some(r) = record {
            r.flags = (r.flags & CLOSED) | value;
        }
    }
}

enum HistoryRecords {
    Dense(Vec<u16>),
    Sparse(HashMap<u32, u16>),
}
pub(crate) struct History {
    records: HistoryRecords,
    limit: usize,
    changed: bool,
    prefix: Vec<(u32, u64)>,
    prefix_dirty: bool,
}
impl History {
    pub(crate) fn capture(&self) -> Result<Vec<(u32, u16)>> {
        let count = match &self.records {
            HistoryRecords::Dense(v) => v.iter().filter(|&&x| x != 0).count(),
            HistoryRecords::Sparse(v) => v.len(),
        };
        if count > self.limit || count > 4_000_000 {
            return Err(Error::Budget("replay history capture limit".into()));
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(count)
            .map_err(|_| Error::Budget("replay history allocation".into()))?;
        match &self.records {
            HistoryRecords::Dense(v) => {
                for (i, &value) in v.iter().enumerate() {
                    if value != 0 {
                        result.push((i as u32, value));
                    }
                }
            }
            HistoryRecords::Sparse(v) => {
                result.extend(v.iter().map(|(&id, &value)| (id, value)));
                result.sort_unstable();
            }
        }
        Ok(result)
    }
    pub(crate) fn restore_capture(
        nodes: usize,
        limit: usize,
        entries: &[(u32, u16)],
    ) -> Result<Self> {
        if entries.len() > limit || entries.len() > 4_000_000 {
            return Err(Error::Budget("replay history state limit".into()));
        }
        let mut result = Self::new(nodes, limit)?;
        if let HistoryRecords::Sparse(records) = &mut result.records {
            records
                .try_reserve(entries.len())
                .map_err(|_| Error::Budget("replay history allocation".into()))?;
        }
        let mut last = None;
        for &(id, value) in entries {
            if id as usize >= nodes || value == 0 || last.is_some_and(|old| old >= id) {
                return Err(Error::Invalid("invalid replay history entry".into()));
            }
            match &mut result.records {
                HistoryRecords::Dense(v) => v[id as usize] = value,
                HistoryRecords::Sparse(v) => {
                    v.insert(id, value);
                }
            }
            last = Some(id);
        }
        result.changed = !entries.is_empty();
        result.prefix_dirty = result.changed;
        Ok(result)
    }

    pub fn new(nodes: usize, limit: usize) -> Result<Self> {
        let records = if nodes <= DENSE_NODES {
            let mut values = Vec::new();
            values
                .try_reserve_exact(nodes)
                .map_err(|_| Error::Budget("routing history allocation".into()))?;
            values.resize(nodes, 0);
            HistoryRecords::Dense(values)
        } else {
            HistoryRecords::Sparse(HashMap::new())
        };
        Ok(Self {
            records,
            limit,
            changed: false,
            prefix: Vec::new(),
            prefix_dirty: false,
        })
    }
    pub fn is_zero(&self) -> bool {
        !self.changed
    }
    pub fn ensure_prefix(&mut self) -> Result<()> {
        if !self.prefix_dirty {
            return Ok(());
        }
        self.prefix.clear();
        if let HistoryRecords::Sparse(records) = &self.records
            && !records.is_empty()
        {
            if self.prefix.capacity() < records.len() {
                self.prefix
                    .try_reserve(records.len() - self.prefix.capacity())
                    .map_err(|_| Error::Budget("routing history prefix allocation".into()))?;
            }
            self.prefix
                .extend(records.iter().map(|(&id, &v)| (id, u64::from(v))));
            self.prefix.sort_unstable_by_key(|&(id, _)| id);
            let mut acc = 0u64;
            for slot in &mut self.prefix {
                acc = acc.saturating_add(slot.1);
                slot.1 = acc;
            }
        }
        self.prefix_dirty = false;
        Ok(())
    }
    pub fn range_sum(&self, lo: u32, hi: u32) -> (u64, usize) {
        if lo > hi || self.is_zero() {
            return (0, 0);
        }
        match &self.records {
            HistoryRecords::Dense(records) => {
                let slice = &records[lo as usize..=hi as usize];
                (slice.iter().map(|&v| u64::from(v)).sum(), slice.len())
            }
            HistoryRecords::Sparse(_) => {
                let work = self.prefix.len().max(1).ilog2() as usize + 1;
                let end = self.prefix.partition_point(|&(id, _)| id <= hi);
                if end == 0 {
                    return (0, work);
                }
                let start = self.prefix[..end].partition_point(|&(id, _)| id < lo);
                let hi_sum = self.prefix[end - 1].1;
                let lo_sum = if start == 0 {
                    0
                } else {
                    self.prefix[start - 1].1
                };
                (hi_sum - lo_sum, work * 2)
            }
        }
    }
    pub fn get(&self, id: u32) -> u16 {
        match &self.records {
            HistoryRecords::Dense(records) => records.get(id as usize).copied().unwrap_or(0),
            HistoryRecords::Sparse(records) => records.get(&id).copied().unwrap_or(0),
        }
    }
    /// Reverse a bounded, caller-charged journal of previous values. Repeated
    /// nodes are restored backwards so saturated u16 updates are also exact.
    pub(crate) fn restore_updates(&mut self, updates: &[(u32, u16)], was_zero: bool) -> Result<()> {
        for &(id, value) in updates.iter().rev() {
            match &mut self.records {
                HistoryRecords::Dense(records) => {
                    let slot = records
                        .get_mut(id as usize)
                        .ok_or_else(|| Error::Invalid("history rollback outside graph".into()))?;
                    *slot = value;
                }
                HistoryRecords::Sparse(records) => {
                    if value == 0 {
                        records.remove(&id);
                    } else {
                        let slot = records.get_mut(&id).ok_or_else(|| {
                            Error::Invalid("history rollback missing node".into())
                        })?;
                        *slot = value;
                    }
                }
            }
        }
        self.changed = !was_zero;
        self.prefix_dirty = true;
        Ok(())
    }

    pub fn increment(&mut self, id: u32) -> Result<()> {
        match &mut self.records {
            HistoryRecords::Dense(records) => {
                let value = records
                    .get_mut(id as usize)
                    .ok_or_else(|| Error::Invalid("routing history node outside graph".into()))?;
                *value = value.saturating_add(1);
            }
            HistoryRecords::Sparse(records) => {
                if !records.contains_key(&id) {
                    if records.len() >= self.limit {
                        return Err(Error::Budget("routing history-state limit".into()));
                    }
                    records
                        .try_reserve(1)
                        .map_err(|_| Error::Budget("routing history-state allocation".into()))?;
                }
                let value = records.entry(id).or_default();
                *value = value.saturating_add(1);
            }
        }
        self.changed = true;
        self.prefix_dirty = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_journal_restores_repeated_saturated_and_new_entries() {
        for nodes in [16, 3_000_000_000] {
            let mut history = History::new(nodes, 2).unwrap();
            let mut updates = Vec::new();
            for node in [3, 3, 9, 3] {
                updates.push((node, history.get(node)));
                history.increment(node).unwrap();
            }
            assert_eq!(history.get(3), 3);
            history.restore_updates(&updates, true).unwrap();
            assert!(history.is_zero());
            assert!(history.capture().unwrap().is_empty());
            history = History::restore_capture(nodes, 2, &[(3, u16::MAX)]).unwrap();
            let before = history.capture().unwrap();
            let updates = [(3, history.get(3)), (9, history.get(9))];
            history.increment(3).unwrap();
            history.increment(9).unwrap();
            history.restore_updates(&updates, false).unwrap();
            assert_eq!(history.capture().unwrap(), before);
            assert!(!history.is_zero());
        }
    }

    #[test]
    fn discovered_state_capture_is_bounded_ordered_and_epoch_specific() {
        for nodes in [16, 3_000_000_000] {
            let mut search = Search::new(nodes, 4).unwrap();
            search.begin(1);
            for id in [9, 3] {
                search
                    .set(
                        id,
                        State {
                            cost: 7,
                            parent: NONE,
                            kind: NONE,
                        },
                    )
                    .unwrap();
            }
            assert!(search.capture_bounded(1).is_err());
            assert!(search.capture_bounded(0).is_err());
            assert!(search.capture_bounded(4097).is_err());
            let (rows, work) = search.capture_bounded(2).unwrap();
            assert_eq!(rows.iter().map(|x| x.0).collect::<Vec<_>>(), [3, 9]);
            assert!(work >= 2);
            search.begin(2);
            assert!(search.capture_bounded(2).unwrap().0.is_empty());
        }
    }

    #[test]
    fn sparse_search_is_bounded_and_resets_between_sinks() {
        let mut search =
            Search::new(3_000_000_000, 2).expect("implicit grid must not allocate its nodes");
        search.begin(1);
        search
            .set(
                2_500_000_000,
                State {
                    cost: 7,
                    parent: NONE,
                    kind: NONE,
                },
            )
            .expect("first state");
        search
            .set(
                9,
                State {
                    cost: 8,
                    parent: 2_500_000_000,
                    kind: 3,
                },
            )
            .expect("second state");
        assert_eq!(search.get(9).expect("stored").parent, 2_500_000_000);
        assert!(
            search
                .set(
                    10,
                    State {
                        cost: 9,
                        parent: 9,
                        kind: NONE
                    }
                )
                .is_err()
        );
        search.begin(2);
        assert!(search.get(9).is_none());
        search
            .set(
                10,
                State {
                    cost: 1,
                    parent: NONE,
                    kind: NONE,
                },
            )
            .expect("budget reused");
    }
    /// Paged sparse records against the former hash-map semantics: one record
    /// per node, `set` reopens and keeps its queue position, the distinct-node
    /// limit, epoch resets, and node-ordered capture.
    #[test]
    fn paged_records_match_a_reference_map() {
        use std::collections::BTreeMap;
        // Limit 300 maps one page and keeps the rest in the overflow map;
        // limit 6,000 maps up to 15 pages before overflowing.
        for (nodes, limit) in [
            (3_000_000usize, 300usize),
            (3_000_000_000, 300),
            (3_000_000, 6_000),
        ] {
            let mut search = Search::new(nodes, limit).unwrap();
            let mut reference: BTreeMap<u32, (u64, bool, Option<usize>)> = BTreeMap::new();
            let mut random = 0x5eed_u64;
            let mut next = || {
                random = random
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                random >> 33
            };
            let mut epoch = 1;
            search.begin(epoch);
            for step in 0..200_000u64 {
                // Clustered ids touch few pages; a few far ids touch new ones.
                let id = if next() % 8 == 0 {
                    (next() % nodes as u64) as u32
                } else {
                    (next() % 4096) as u32 * 7
                };
                match next() % 7 {
                    0..=2 => {
                        let cost = next();
                        let result = search.set(
                            id,
                            State {
                                cost,
                                parent: NONE,
                                kind: NONE,
                            },
                        );
                        if let Some(r) = reference.get_mut(&id) {
                            assert!(result.is_ok());
                            (r.0, r.1) = (cost, false);
                        } else if reference.len() >= limit {
                            assert!(result.is_err());
                        } else {
                            assert!(result.is_ok());
                            reference.insert(id, (cost, false, None));
                        }
                    }
                    3 => {
                        search.close(id);
                        if let Some(r) = reference.get_mut(&id) {
                            r.1 = true;
                        }
                    }
                    4 if reference.contains_key(&id) => {
                        let position = (next() % 1000) as usize;
                        search.set_position(id, Some(position));
                        reference.get_mut(&id).unwrap().2 = Some(position);
                    }
                    5 if step % 997 == 0 => {
                        epoch += 1;
                        search.begin(epoch);
                        reference.clear();
                    }
                    _ => {}
                }
                assert_eq!(
                    search.get(id).map(|s| s.cost),
                    reference.get(&id).map(|r| r.0)
                );
                assert_eq!(
                    search.is_closed(id),
                    reference.get(&id).is_some_and(|r| r.1)
                );
                assert_eq!(search.position(id), reference.get(&id).and_then(|r| r.2));
                assert_eq!(search.current_count(), reference.len());
            }
            if reference.len() <= 4096 {
                let (rows, _) = search.capture_bounded(4096).unwrap();
                let ids: Vec<u32> = rows.iter().map(|r| r.0).collect();
                assert_eq!(ids, reference.keys().copied().collect::<Vec<_>>());
            }
        }
    }

    /// Scattered records (one per page, the paged layout's worst case) never
    /// grow the page pool past its budget, half of the 64 bytes per record the
    /// route memory estimate charges; the records beyond it stay readable in
    /// the overflow map (direct and hashed page tables). Small node counts
    /// keep this test process's peak RSS low (other tests compare it with
    /// `memory_mib`).
    #[test]
    fn scattered_records_stay_within_the_page_budget() {
        for (nodes, limit) in [
            (40_000usize, 1_000usize),
            (400_000, 600),
            (3_000_000_000, 600),
        ] {
            let mut search = Search {
                records: Records::Sparse(Paged::new(nodes, limit)),
                epoch: 0,
                limit,
            };
            let pages = nodes.div_ceil(PAGE);
            let records = limit.min(pages);
            for epoch in 1..=3u32 {
                search.begin(epoch);
                let stride = pages / records;
                let id =
                    |k: usize| u32::try_from((k * stride + epoch as usize) % pages * PAGE).unwrap();
                for k in 0..records {
                    let state = State {
                        cost: k as u64,
                        parent: NONE,
                        kind: NONE,
                    };
                    search.set(id(k), state).unwrap();
                }
                for k in 0..records {
                    assert_eq!(search.get(id(k)).map(|s| s.cost), Some(k as u64));
                }
                assert_eq!(search.current_count(), records);
            }
            let Records::Sparse(paged) = &search.records else {
                panic!("sparse records expected");
            };
            let budget = page_budget(nodes, limit);
            assert!(paged.used <= budget);
            assert_eq!(paged.used + paged.overflow.len(), records);
            let pool = paged.pool.capacity() * std::mem::size_of::<Tagged>();
            assert!(
                pool <= (limit * POOL_BYTES_PER_RECORD).max(PAGE_BYTES),
                "{nodes} {limit}: pool {pool}"
            );
        }
    }

    #[test]
    fn dense_and_sparse_history_obey_the_same_saturation_rule() {
        for nodes in [16, 3_000_000_000] {
            let mut history = History::new(nodes, 1).expect("bounded history");
            assert!(history.is_zero());
            for _ in 0..70_000 {
                history.increment(3).expect("one entry only");
            }
            assert!(!history.is_zero());
            assert_eq!(history.get(3), u16::MAX);
            assert_eq!(history.get(4), 0);
            if nodes > DENSE_NODES {
                assert!(history.increment(4).is_err());
            }
        }
    }
}
