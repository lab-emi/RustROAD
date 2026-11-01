//! Bounded detailed-router scratch, independent of a large implicit track grid.
use crate::{Error, Result};
use std::collections::HashMap;

pub(crate) const DENSE_NODES: usize = 1_000_000;
const NONE: u32 = u32::MAX;

#[derive(Clone, Copy)]
pub(crate) struct State {
    pub cost: u64,
    pub parent: u32,
    pub kind: u32,
}
#[derive(Clone, Copy)]
struct Tagged {
    epoch: u32,
    closed: bool,
    state: State,
}
const EMPTY: Tagged = Tagged {
    epoch: 0,
    closed: false,
    state: State {
        cost: u64::MAX,
        parent: NONE,
        kind: NONE,
    },
};

enum Records {
    Dense(Vec<Tagged>),
    Sparse(HashMap<u32, (State, bool)>),
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
            Records::Sparse(records) => records.len(),
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
            Records::Sparse(records) => (records.len(), records.len()),
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
                result.extend(records.iter().map(|(&id, &(s, _))| (id, s)));
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
            Records::Sparse(HashMap::new())
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
            records.clear();
        }
    }
    pub fn get(&self, id: u32) -> Option<State> {
        match &self.records {
            Records::Dense(records) => records
                .get(id as usize)
                .filter(|r| r.epoch == self.epoch)
                .map(|r| r.state),
            Records::Sparse(records) => records.get(&id).map(|&(s, _)| s),
        }
    }
    pub fn is_closed(&self, id: u32) -> bool {
        match &self.records {
            Records::Dense(records) => records
                .get(id as usize)
                .is_some_and(|r| r.epoch == self.epoch && r.closed),
            Records::Sparse(records) => records.get(&id).is_some_and(|&(_, closed)| closed),
        }
    }
    pub fn close(&mut self, id: u32) {
        match &mut self.records {
            Records::Dense(records) => {
                if let Some(slot) = records.get_mut(id as usize)
                    && slot.epoch == self.epoch
                {
                    slot.closed = true;
                }
            }
            Records::Sparse(records) => {
                if let Some((_, closed)) = records.get_mut(&id) {
                    *closed = true;
                }
            }
        }
    }
    pub fn set(&mut self, id: u32, state: State) -> Result<()> {
        match &mut self.records {
            Records::Dense(records) => {
                let slot = records
                    .get_mut(id as usize)
                    .ok_or_else(|| Error::Invalid("routing search node outside graph".into()))?;
                *slot = Tagged {
                    epoch: self.epoch,
                    closed: false,
                    state,
                };
            }
            Records::Sparse(records) => {
                if !records.contains_key(&id) {
                    if records.len() >= self.limit {
                        return Err(Error::Budget("routing visited-state limit".into()));
                    }
                    records
                        .try_reserve(1)
                        .map_err(|_| Error::Budget("routing visited-state allocation".into()))?;
                }
                records.insert(id, (state, false));
            }
        }
        Ok(())
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
