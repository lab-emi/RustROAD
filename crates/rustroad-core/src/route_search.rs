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
    state: State,
}
const EMPTY: Tagged = Tagged {
    epoch: 0,
    state: State {
        cost: u64::MAX,
        parent: NONE,
        kind: NONE,
    },
};

enum Records {
    Dense(Vec<Tagged>),
    Sparse(HashMap<u32, State>),
}
pub(crate) struct Search {
    records: Records,
    epoch: u32,
    limit: usize,
}
impl Search {
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
            Records::Sparse(records) => records.get(&id).copied(),
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
                records.insert(id, state);
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
}
impl History {
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
        })
    }
    pub fn is_zero(&self) -> bool {
        !self.changed
    }
    pub fn get(&self, id: u32) -> u16 {
        match &self.records {
            HistoryRecords::Dense(records) => records.get(id as usize).copied().unwrap_or(0),
            HistoryRecords::Sparse(records) => records.get(&id).copied().unwrap_or(0),
        }
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
