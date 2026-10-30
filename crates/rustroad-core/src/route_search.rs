//! Bounded detailed-router scratch, independent of a large implicit track grid.
use crate::{Error, Result};
use std::collections::HashMap;

pub(crate) const DENSE_NODES: usize = 1_000_000;
const NONE: u32 = u32::MAX;
/// Label arena (24 B/record) and key index, including growth/hash-table slack.
/// One aggregate cap counts all headings; no five-way grid allocation.
pub(crate) const LABEL_BYTES: u64 = 128;
/// Container headers and minimum Vec/hash allocation sizes for tiny caps.
pub(crate) const LABEL_FIXED_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Heading {
    #[default]
    None,
    East,
    West,
    North,
    South,
}
impl Heading {
    pub(crate) fn wire(from: [i64; 2], to: [i64; 2]) -> Self {
        if to[0] > from[0] {
            Self::East
        } else if to[0] < from[0] {
            Self::West
        } else if to[1] > from[1] {
            Self::North
        } else {
            Self::South
        }
    }
    pub(crate) fn price(self, next: Self, cost: u64) -> u64 {
        if self == Self::None || self == next {
            0
        } else {
            cost
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::East => "east",
            Self::West => "west",
            Self::North => "north",
            Self::South => "south",
        }
    }
}

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
    Labels {
        arena: Vec<Label>,
        ids: HashMap<(u32, Heading), u32>,
    },
}
struct Label {
    node: u32,
    heading: Heading,
    closed: bool,
    state: State,
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
            Records::Labels { arena, .. } => arena.len(),
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
            Records::Labels { arena, .. } => (arena.len(), arena.len()),
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
            Records::Labels { arena, .. } => {
                result.extend(
                    arena
                        .iter()
                        .enumerate()
                        .map(|(id, label)| (id as u32, label.state)),
                );
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
    pub(crate) fn with_labels(limit: usize) -> Result<Self> {
        if limit == 0 || limit > 64_000_000 {
            return Err(Error::Invalid("routing bend label limit".into()));
        }
        Ok(Self {
            records: Records::Labels {
                arena: Vec::new(),
                ids: HashMap::new(),
            },
            epoch: 0,
            limit,
        })
    }
    pub(crate) fn labeled(&self) -> bool {
        matches!(self.records, Records::Labels { .. })
    }
    pub(crate) fn add_cost(&self, base: u64, extra: u64) -> Result<u64> {
        if self.labeled() {
            base.checked_add(extra)
                .ok_or_else(|| Error::Budget("routing bend path cost overflow".into()))
        } else {
            Ok(base.saturating_add(extra))
        }
    }
    pub(crate) fn node(&self, id: u32) -> Result<u32> {
        match &self.records {
            Records::Labels { arena, .. } => arena
                .get(id as usize)
                .map(|label| label.node)
                .ok_or_else(|| Error::Invalid("missing routing label".into())),
            _ => Ok(id),
        }
    }
    pub(crate) fn heading(&self, id: u32) -> Result<Heading> {
        match &self.records {
            Records::Labels { arena, .. } => arena
                .get(id as usize)
                .map(|label| label.heading)
                .ok_or_else(|| Error::Invalid("missing routing label".into())),
            _ => Ok(Heading::None),
        }
    }
    pub(crate) fn find(&self, node: u32, heading: Heading) -> Option<u32> {
        match &self.records {
            Records::Labels { ids, .. } => ids.get(&(node, heading)).copied(),
            _ => Some(node),
        }
    }
    pub(crate) fn insert(&mut self, node: u32, heading: Heading, state: State) -> Result<u32> {
        if let Records::Labels { arena, ids } = &mut self.records {
            if let Some(&id) = ids.get(&(node, heading)) {
                arena[id as usize].state = state;
                arena[id as usize].closed = false;
                return Ok(id);
            }
            // Refuse before either allocation/index insertion; all headings share it.
            if arena.len() >= self.limit {
                return Err(Error::Budget("routing bend discovered-label limit".into()));
            }
            let id = u32::try_from(arena.len())
                .map_err(|_| Error::Budget("routing bend label ID".into()))?;
            arena
                .try_reserve(1)
                .map_err(|_| Error::Budget("routing bend label allocation".into()))?;
            ids.try_reserve(1)
                .map_err(|_| Error::Budget("routing bend label index allocation".into()))?;
            arena.push(Label {
                node,
                heading,
                closed: false,
                state,
            });
            ids.insert((node, heading), id);
            return Ok(id);
        }
        self.set(node, state)?;
        Ok(node)
    }
    pub fn begin(&mut self, epoch: u32) {
        self.epoch = epoch;
        if let Records::Sparse(records) = &mut self.records {
            records.clear();
        }
        if let Records::Labels { arena, ids } = &mut self.records {
            arena.clear();
            ids.clear();
        }
    }
    pub fn get(&self, id: u32) -> Option<State> {
        match &self.records {
            Records::Dense(records) => records
                .get(id as usize)
                .filter(|r| r.epoch == self.epoch)
                .map(|r| r.state),
            Records::Sparse(records) => records.get(&id).map(|&(s, _)| s),
            Records::Labels { arena, .. } => arena.get(id as usize).map(|label| label.state),
        }
    }
    pub fn is_closed(&self, id: u32) -> bool {
        match &self.records {
            Records::Dense(records) => records
                .get(id as usize)
                .is_some_and(|r| r.epoch == self.epoch && r.closed),
            Records::Sparse(records) => records.get(&id).is_some_and(|&(_, closed)| closed),
            Records::Labels { arena, .. } => {
                arena.get(id as usize).is_some_and(|label| label.closed)
            }
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
            Records::Labels { arena, .. } => {
                if let Some(label) = arena.get_mut(id as usize) {
                    label.closed = true;
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
            Records::Labels { arena, .. } => {
                let label = arena
                    .get_mut(id as usize)
                    .ok_or_else(|| Error::Invalid("missing routing label".into()))?;
                label.state = state;
                label.closed = false;
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
    fn heading_labels_keep_the_more_expensive_arrival_that_wins_later() {
        // Analytic diamond: N arrival costs 5, E arrival 6; east exit is 1.
        // With a 10 bend fee, their completed prices are 16 and 7. A single
        // point label incorrectly discards the second arrival. <1KiB fixture.
        let mut search = Search::with_labels(4).unwrap();
        search.begin(1);
        let north = search
            .insert(
                10,
                Heading::North,
                State {
                    cost: 5,
                    parent: NONE,
                    kind: NONE,
                },
            )
            .unwrap();
        let east = search
            .insert(
                10,
                Heading::East,
                State {
                    cost: 6,
                    parent: NONE,
                    kind: NONE,
                },
            )
            .unwrap();
        assert_ne!(north, east);
        search.close(north);
        assert!(!search.is_closed(east));
        let mut queue = crate::route_queue::Pending::new(4);
        for parent in [north, east] {
            let cost = search.get(parent).unwrap().cost
                + 1
                + search.heading(parent).unwrap().price(Heading::East, 10);
            let old = search.find(11, Heading::East).and_then(|id| search.get(id));
            if old.is_some_and(|state| state.cost <= cost) {
                continue;
            }
            let id = search
                .insert(
                    11,
                    Heading::East,
                    State {
                        cost,
                        parent,
                        kind: NONE,
                    },
                )
                .unwrap();
            queue.offer((cost, std::cmp::Reverse(cost), id)).unwrap();
        }
        let (price, _, goal) = queue.pop().unwrap();
        assert_eq!(price, 7);
        assert_eq!(search.get(goal).unwrap().parent, east);
        assert_eq!(search.node(goal).unwrap(), 11);
        assert_eq!(search.current_count(), 3);
    }

    #[test]
    fn via_chains_preserve_heading_and_pay_only_the_next_planar_bend() {
        let mut search = Search::with_labels(4).unwrap();
        search.begin(1);
        let first = search
            .insert(
                100,
                Heading::East,
                State {
                    cost: 4,
                    parent: NONE,
                    kind: NONE,
                },
            )
            .unwrap();
        let mut last = first;
        // Two arbitrary layer-changing vertices, each via costs 3.
        for node in [200, 300] {
            let heading = search.heading(last).unwrap();
            last = search
                .insert(
                    node,
                    heading,
                    State {
                        cost: search.get(last).unwrap().cost + 3,
                        parent: last,
                        kind: 1,
                    },
                )
                .unwrap();
            assert_eq!(search.heading(last).unwrap(), Heading::East);
        }
        let price = search.get(last).unwrap().cost
            + 2
            + search.heading(last).unwrap().price(Heading::North, 11);
        let goal = search
            .insert(
                400,
                Heading::North,
                State {
                    cost: price,
                    parent: last,
                    kind: NONE,
                },
            )
            .unwrap();
        assert_eq!(price, 23); // 4 + 3 + 3 + 2 + one 11 bend.
        assert_eq!(Heading::East.price(Heading::East, 11), 0);
        assert_eq!(Heading::None.price(Heading::North, 11), 0);
        assert_eq!(Heading::East.price(Heading::West, 11), 11); // U-turn policy.
        let mut id = goal;
        for node in [400, 300, 200, 100] {
            assert_eq!(search.node(id).unwrap(), node);
            id = search.get(id).unwrap().parent;
        }
        assert_eq!(id, NONE);
    }

    #[test]
    fn heading_label_cap_is_aggregate_preinsertion_and_reusable() {
        assert!(Search::with_labels(0).is_err());
        assert!(Search::with_labels(64_000_001).is_err());
        let mut search = Search::with_labels(2).unwrap();
        assert!(search.add_cost(u64::MAX, 1).is_err());
        assert_eq!(
            Search::new(2, 2).unwrap().add_cost(u64::MAX, 1).unwrap(),
            u64::MAX
        );
        search.begin(1);
        let state = State {
            cost: 9,
            parent: NONE,
            kind: NONE,
        };
        let north = search.insert(10, Heading::North, state).unwrap();
        search.insert(10, Heading::East, state).unwrap();
        assert!(
            search
                .insert(10, Heading::West, state)
                .unwrap_err()
                .to_string()
                .contains("BUDGET_EXCEEDED")
        );
        assert_eq!(search.current_count(), 2);
        assert!(search.find(10, Heading::West).is_none());
        assert_eq!(
            search
                .insert(10, Heading::North, State { cost: 1, ..state })
                .unwrap(),
            north
        );
        assert_eq!(search.current_count(), 2);
        search.begin(2);
        assert_eq!(search.current_count(), 0);
        assert!(search.find(10, Heading::North).is_none());
        assert_eq!(
            search.insert(u32::MAX - 1, Heading::South, state).unwrap(),
            0
        );
        assert!(std::mem::size_of::<Label>() <= 32);
        assert!(std::mem::size_of::<((u32, Heading), u32)>() <= 16);
    }

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
