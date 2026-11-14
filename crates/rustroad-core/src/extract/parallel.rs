//! Strip computation on the `--threads` pool with an order-preserving,
//! sharded commit.
//!
//! Units (a wire body, or the x/y strip pair of one rectangle) are taken from
//! the lazy unit iterator in numbered chunks. Lanes compute chunks
//! concurrently into bounded transcripts of ordered accumulations.
//!
//! Every accumulator (the ground capacitance of one RC node, one coupling
//! capacitor, the overlap-area statistic) belongs to exactly one shard.
//! A complete chunk's transcript is split by shard in emission order, so
//! each accumulator keeps the run order of its additions. A leader clears
//! chunks strictly in run order: it applies integer statistics and
//! cumulative work and proves that no budget can fail inside the chunk.
//! Each shard then applies the cleared chunks' records for its own
//! accumulators, in chunk order, on whichever lane is free, and finalizes
//! the coupling keys due before the chunk's end. Shards never touch each
//! other's accumulators, so every accumulator sees the identical sequence of
//! f64 additions as the serial loop at any thread count and chunk size.
//!
//! A chunk the leader cannot clear (a unit a lane did not finish, or a work
//! or coupling budget that could overflow inside it) is committed at a
//! barrier: every shard first applies all earlier chunks, then the serial
//! path computes the chunk's unfinished units at their place in run order
//! from the committed state. Budget errors therefore carry the serial path's
//! exact code and message.
use super::strip::{self, AREA, Counts, Ctx, Flow, GROUND, Op, Scratch, Sink, Stop, UnitRef};
use super::{CANDIDATE_SCRATCH_BYTES, ExtractLimits, ExtractStats, budget, guard_memory, invalid};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Units per chunk unless `--extract-tile-units` sets it.
const CHUNK_UNITS: usize = 64;
/// Committed chunk buffers kept for reuse are at most this large; a buffer
/// that grew for an exceptional chunk is released.
const SPARE_BYTES: usize = 512 << 10;
/// Hidden `--extract-tile-units` ceiling.
pub const MAX_TILE_UNITS: usize = 1 << 20;

/// Parallel-only bounds. Units beyond them run serially at their commit;
/// results never depend on them.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    /// Largest candidate list a lane computes; larger strips run serially
    /// with the serial scratch (admitted by the original estimate).
    pub candidates: usize,
    /// Transcript capacity of one chunk, in ordered accumulations.
    pub chunk_ops: usize,
    /// Computed chunks waiting for their commit; lanes pause above this.
    pub pending_bytes: usize,
    /// Index virtual cell shapes in the per-layer grids when they fit (no
    /// instance queries); `false` keeps the instance-index path (tests).
    pub virtual_grid: bool,
    /// Same-net construction lists this long are bucketed; `usize::MAX`
    /// scans every list linearly (tests).
    pub net_bin_list: usize,
    /// Count passive strips that cannot record anything without sweeping
    /// them; `false` sweeps every strip (tests).
    pub passive_shortcut: bool,
}
impl Default for Bounds {
    fn default() -> Self {
        Self {
            candidates: 16_384,
            chunk_ops: 1 << 17,
            pending_bytes: 4 << 20,
            virtual_grid: true,
            net_bin_list: super::NET_BIN_LIST,
            passive_shortcut: true,
        }
    }
}

/// Entries per arena chunk of a coupling table (28 bytes each).
const CHUNK_ENTRIES: usize = 1 << 15;
const NIL: u32 = u32::MAX;
/// `due` of a free slot.
const FREE: u32 = u32::MAX;
/// `due` of a key without a completion unit: finalized at the end.
const NEVER: u32 = u32::MAX - 1;
/// Completion granules hold the keys due within `1 << GRANULE_SHIFT` units.
const GRANULE_SHIFT: u32 = 6;
/// Accumulator shards at every thread count. Each shard sums the couplings
/// it grounds per node, and the per-node totals are added in shard order,
/// so the shard count is fixed: it may not follow the lanes.
pub(super) const SHARDS: usize = 4;
const SHARD_SHIFT: u32 = SHARDS.trailing_zeros();

/// One coupling accumulator: key and value bits in 32-bit halves, the
/// bucket chain link (the free-list link of a free slot), the unit after
/// which no strip records toward the key, and the next key of its
/// completion granule (28 bytes, 4-byte aligned).
#[derive(Clone, Copy)]
struct Cell {
    key: [u32; 2],
    value: [u32; 2],
    next: u32,
    due: u32,
    next_due: u32,
}
impl Cell {
    #[inline]
    fn key(&self) -> u64 {
        u64::from(self.key[1]) << 32 | u64::from(self.key[0])
    }
    #[inline]
    fn value(&self) -> f64 {
        f64::from_bits(u64::from(self.value[1]) << 32 | u64::from(self.value[0]))
    }
}

/// The open coupling accumulators of one shard: a chained hash table whose
/// entries live in fixed-size arena chunks. A finalized key's slot goes to
/// a free list and is reused, so the arena holds at most the largest
/// number of keys open at once. Growth rebuilds only the 4-byte head array;
/// entries never move.
pub(super) struct Couplings {
    heads: Vec<u32>,
    bits: u32,
    chunks: Vec<Vec<Cell>>,
    /// Slots ever used.
    len: usize,
    /// Live keys, and the most at once.
    open: usize,
    peak: usize,
    /// First free slot (`NIL`: none), linked through `next`.
    free: u32,
}
impl Couplings {
    const MIN_BITS: u32 = 10;
    fn new() -> Self {
        Self {
            heads: vec![NIL; 1 << Self::MIN_BITS],
            bits: Self::MIN_BITS,
            chunks: Vec::new(),
            len: 0,
            open: 0,
            peak: 0,
            free: NIL,
        }
    }
    #[inline]
    fn hash(key: u64) -> u64 {
        let m = u128::from(key) * 0x9E37_79B9_7F4A_7C15;
        (m as u64) ^ ((m >> 64) as u64)
    }
    #[inline]
    fn bucket(key: u64, bits: u32) -> usize {
        (Self::hash(key) >> (64 - bits)) as usize
    }
    #[inline]
    fn slot(&self, i: u32) -> &Cell {
        &self.chunks[i as usize / CHUNK_ENTRIES][i as usize % CHUNK_ENTRIES]
    }
    #[inline]
    fn slot_mut(&mut self, i: u32) -> &mut Cell {
        &mut self.chunks[i as usize / CHUNK_ENTRIES][i as usize % CHUNK_ENTRIES]
    }
    /// Index of `key`'s accumulator (created at 0.0 with completion unit
    /// `due()`) and whether it is new.
    #[inline]
    fn find_or_insert(&mut self, key: u64, due: impl FnOnce() -> u32) -> (u32, bool) {
        let mut i = self.heads[Self::bucket(key, self.bits)];
        while i != NIL {
            let slot = self.slot(i);
            if slot.key() == key {
                return (i, false);
            }
            i = slot.next;
        }
        if self.open >= 2 << self.bits {
            self.grow();
        }
        let b = Self::bucket(key, self.bits);
        let zero = 0.0f64.to_bits();
        let cell = Cell {
            key: [key as u32, (key >> 32) as u32],
            value: [zero as u32, (zero >> 32) as u32],
            next: self.heads[b],
            due: due(),
            next_due: NIL,
        };
        let index = if self.free != NIL {
            let index = self.free;
            self.free = self.slot(index).next;
            *self.slot_mut(index) = cell;
            index
        } else {
            let index = u32::try_from(self.len)
                .ok()
                .filter(|&i| i < NEVER)
                .expect("coupling count is bounded by max_couplings");
            if self.len.is_multiple_of(CHUNK_ENTRIES) {
                self.chunks.push(Vec::with_capacity(CHUNK_ENTRIES));
            }
            self.chunks.last_mut().expect("chunk").push(cell);
            self.len += 1;
            index
        };
        self.heads[b] = index;
        self.open += 1;
        self.peak = self.peak.max(self.open);
        (index, true)
    }
    #[inline]
    fn get(&self, i: u32) -> f64 {
        self.slot(i).value()
    }
    #[inline]
    fn set(&mut self, i: u32, v: f64) {
        let bits = v.to_bits();
        self.slot_mut(i).value = [bits as u32, (bits >> 32) as u32];
    }
    /// Unlinks the live entry `i` and frees its slot.
    fn remove(&mut self, i: u32) {
        let b = Self::bucket(self.slot(i).key(), self.bits);
        let next = self.slot(i).next;
        if self.heads[b] == i {
            self.heads[b] = next;
        } else {
            let mut at = self.heads[b];
            while self.slot(at).next != i {
                at = self.slot(at).next;
            }
            self.slot_mut(at).next = next;
        }
        let free = self.free;
        let slot = self.slot_mut(i);
        slot.due = FREE;
        slot.next = free;
        self.free = i;
        self.open -= 1;
    }
    /// Doubles the head array and relinks every live entry in place.
    fn grow(&mut self) {
        self.bits += 1;
        self.heads.clear();
        self.heads.shrink_to_fit();
        self.heads.resize(1 << self.bits, NIL);
        let bits = self.bits;
        let mut index = 0u32;
        for chunk in &mut self.chunks {
            for slot in chunk.iter_mut() {
                if slot.due != FREE {
                    let b = Self::bucket(slot.key(), bits);
                    slot.next = self.heads[b];
                    self.heads[b] = index;
                }
                index += 1;
            }
        }
    }
    /// Every live `(due, key, value)`.
    fn live(&self) -> impl Iterator<Item = (u32, u64, f64)> + '_ {
        self.chunks
            .iter()
            .flatten()
            .filter(|s| s.due != FREE)
            .map(|s| (s.due, s.key(), s.value()))
    }
}

/// Selector bits of a coupling key, independent of the map hash.
#[inline]
fn select(key: u64) -> u64 {
    key.wrapping_mul(0xD6E8_FEB8_6659_FD93)
}
/// Owning shard of an accumulator. `shift` is log2 of the shard count.
#[inline]
pub(super) fn shard_of(key: u64, shift: u32) -> usize {
    if shift == 0 {
        0
    } else if key >= GROUND {
        // Ground at global node g (the area statistic is shard 0's).
        if key == AREA {
            0
        } else {
            (key as u32 as usize) & ((1 << shift) - 1)
        }
    } else {
        ((select(key) >> 32) as usize) & ((1 << shift) - 1)
    }
}

/// Completion unit of coupling `key`: the last unit whose strips have one
/// of its two nodes as their own node (`NEVER` without a schedule).
#[inline]
fn due(last: &[u32], key: u64) -> u32 {
    if last.is_empty() {
        NEVER
    } else {
        last[(key >> 32) as usize].max(last[key as u32 as usize])
    }
}

/// Accumulators owned by one shard: ground at the global nodes `g` with
/// `g % shards == s` (stored at `g / shards`), the coupling keys whose
/// selector picks `s`, and in shard 0 the overlap-area statistic. A
/// coupling key is finalized once its completion unit is committed: it
/// leaves the table and is grounded into this shard's per-node sums or
/// kept.
pub(super) struct Shard {
    ground: Vec<f64>,
    couplings: Couplings,
    area: f64,
    /// Coupling keys created (finalized ones included).
    keys: usize,
    /// Per completion granule: its first key, linked through `next_due`.
    due_heads: Vec<u32>,
    /// Granules before this one are finalized.
    next_granule: usize,
    /// Couplings this shard grounded, per global node, in completion order.
    grounded: Vec<f64>,
    grounded_count: usize,
    /// Kept couplings in completion order.
    kept: Vec<(u64, f64)>,
    batch: Vec<(u32, u64, f64)>,
}
impl Shard {
    fn new(index: usize, shift: u32, nodes: usize, units: usize) -> Result<Self> {
        let count = (nodes + (1 << shift) - 1 - index) >> shift;
        let alloc = |n: usize, what| -> Result<Vec<f64>> {
            let mut v = Vec::new();
            v.try_reserve_exact(n).map_err(|_| budget(what))?;
            v.resize(n, 0.0);
            Ok(v)
        };
        Ok(Self {
            ground: alloc(count, "extraction ground accumulator allocation")?,
            couplings: Couplings::new(),
            area: 0.0,
            keys: 0,
            due_heads: vec![NIL; (units >> GRANULE_SHIFT) + 1],
            next_granule: 0,
            grounded: alloc(nodes, "extraction grounded coupling allocation")?,
            grounded_count: 0,
            kept: Vec::new(),
            batch: Vec::new(),
        })
    }
    /// Index of `key`'s accumulator, scheduled by its completion unit when
    /// new; whether it is new.
    #[inline]
    fn find_or_insert(&mut self, key: u64, last: &[u32]) -> (u32, bool) {
        let (i, new) = self.couplings.find_or_insert(key, || due(last, key));
        if new {
            self.keys += 1;
            let due = self.couplings.slot(i).due;
            if let Some(head) = self.due_heads.get_mut((due >> GRANULE_SHIFT) as usize) {
                self.couplings.slot_mut(i).next_due = *head;
                *head = i;
            }
        }
        (i, new)
    }
    /// `coupling[key] += pf` (created at 0.0); whether the key is new.
    #[inline]
    fn couple(&mut self, key: u64, pf: f64, last: &[u32]) -> bool {
        let (i, new) = self.find_or_insert(key, last);
        let v = self.couplings.get(i) + pf;
        self.couplings.set(i, v);
        new
    }
    /// Applies one shard's records (each accumulator's in their run order;
    /// consecutive records of one key take one lookup). Returns the new
    /// coupling keys.
    fn apply_grouped(&mut self, ops: &[Op], shift: u32, last: &[u32]) -> usize {
        let mut fresh = 0;
        let mut at = 0;
        while at < ops.len() {
            let key = ops[at].key;
            if key < GROUND {
                let (i, new) = self.find_or_insert(key, last);
                fresh += usize::from(new);
                let mut v = self.couplings.get(i);
                while at < ops.len() && ops[at].key == key {
                    v += ops[at].pf;
                    at += 1;
                }
                self.couplings.set(i, v);
                continue;
            }
            let value = if key == AREA {
                &mut self.area
            } else {
                &mut self.ground[(key as u32 as usize) >> shift]
            };
            while at < ops.len() && ops[at].key == key {
                *value += ops[at].pf;
                at += 1;
            }
        }
        fresh
    }
    /// Finalizes every key due before unit `end` (every unit before it is
    /// committed, so no record toward such a key remains): they leave the
    /// table in `(due, key)` order and are grounded or kept (`settle`).
    fn finalize(&mut self, end: u32, min: f64) {
        let full = ((end >> GRANULE_SHIFT) as usize).min(self.due_heads.len());
        while self.next_granule < full {
            let mut i = std::mem::replace(&mut self.due_heads[self.next_granule], NIL);
            while i != NIL {
                let slot = *self.couplings.slot(i);
                self.batch.push((slot.due, slot.key(), slot.value()));
                self.couplings.remove(i);
                i = slot.next_due;
            }
            self.next_granule += 1;
        }
        // The granule of `end` itself: its keys due before `end`.
        if let Some(head) = self.due_heads.get(full).copied()
            && end & ((1 << GRANULE_SHIFT) - 1) != 0
        {
            let (mut i, mut keep) = (head, NIL);
            while i != NIL {
                let slot = *self.couplings.slot(i);
                if slot.due < end {
                    self.batch.push((slot.due, slot.key(), slot.value()));
                    self.couplings.remove(i);
                } else {
                    self.couplings.slot_mut(i).next_due = keep;
                    keep = i;
                }
                i = slot.next_due;
            }
            self.due_heads[full] = keep;
        }
        self.settle(min);
    }
    /// Grounds or keeps the finalized keys in ascending `(due, key)`: a
    /// positive coupling below `min` is added to this shard's grounded sum
    /// of both its nodes (the lower node first), every other positive one
    /// is kept.
    fn settle(&mut self, min: f64) {
        self.batch.sort_unstable_by_key(|&(due, key, _)| (due, key));
        for &(_, key, pf) in &self.batch {
            if pf <= 0.0 {
                continue;
            }
            if pf < min {
                self.grounded[(key >> 32) as usize] += pf;
                self.grounded[key as u32 as usize] += pf;
                self.grounded_count += 1;
            } else {
                self.kept.push((key, pf));
            }
        }
        self.batch.clear();
    }
    /// Finalizes every key still open (keys without a completion unit, and
    /// any the sweep did not reach), in `(due, key)` order.
    fn finalize_all(&mut self, min: f64) {
        let mut batch = std::mem::take(&mut self.batch);
        batch.extend(self.couplings.live());
        self.batch = batch;
        self.couplings = Couplings::new();
        self.settle(min);
    }
}

/// Every network accumulator of the strip phase, split into `SHARDS`
/// shards.
pub(super) struct Accumulators {
    shards: Vec<Shard>,
    shift: u32,
    /// Per global node: the last unit whose strips have it as their own
    /// node (empty: every key is finalized at the end).
    last: Vec<u32>,
    /// Couplings below this (pF) are grounded.
    min: f64,
}
impl Accumulators {
    /// `area` seeds the overlap statistic; `last` schedules every key's
    /// finalization over `units` units.
    pub(super) fn new(
        nodes: usize,
        area: f64,
        last: Vec<u32>,
        units: usize,
        min: f64,
    ) -> Result<Self> {
        let mut out = Self {
            shards: (0..SHARDS)
                .map(|s| Shard::new(s, SHARD_SHIFT, nodes, units))
                .collect::<Result<_>>()?,
            shift: SHARD_SHIFT,
            last,
            min,
        };
        out.shards[0].area = area;
        Ok(out)
    }
    pub(super) fn keys(&self) -> usize {
        self.shards.iter().map(|s| s.keys).sum()
    }
    pub(super) fn area(&self) -> f64 {
        self.shards[0].area
    }
    /// Accumulated strip ground at global node `g`.
    #[cfg(test)]
    pub(super) fn ground(&self, g: u32) -> f64 {
        let g = g as usize;
        self.shards[g & ((1 << self.shift) - 1)].ground[g >> self.shift]
    }
    /// Live keys over all shards (keys not yet finalized).
    #[cfg(test)]
    pub(super) fn open(&self) -> usize {
        self.shards.iter().map(|s| s.couplings.open).sum()
    }
    /// The sum over the shards of the most keys each held open at once.
    pub(super) fn open_peak(&self) -> usize {
        self.shards.iter().map(|s| s.couplings.peak).sum()
    }
}

impl Accumulators {
    /// Finishes the network accumulators: every key still open is
    /// finalized, then each node's ground is its accumulated strip ground
    /// plus, shard by shard in shard order, the couplings that shard
    /// grounded at it (each shard adds them in `(due, key)` order). `nets`
    /// holds the ground of each net's nodes with the global ID of its node
    /// 0 (a net's nodes have consecutive IDs). Returns the kept couplings
    /// in ascending key order and the count of grounded ones.
    pub(super) fn finish(
        mut self,
        nets: &mut [(u32, &mut [f64])],
    ) -> Result<(Vec<(u64, f64)>, usize)> {
        use rayon::prelude::*;
        let min = self.min;
        self.shards
            .par_iter_mut()
            .for_each(|shard| shard.finalize_all(min));
        let shards = &self.shards;
        let shift = self.shift;
        nets.par_iter_mut().for_each(|(base, ground)| {
            for (k, g) in ground.iter_mut().enumerate() {
                let node = *base as usize + k;
                let mut v = shards[node & ((1 << shift) - 1)].ground[node >> shift];
                for shard in shards {
                    v += shard.grounded[node];
                }
                *g = v;
            }
        });
        let count = shards.iter().map(|s| s.grounded_count).sum();
        let mut kept = Vec::new();
        kept.try_reserve_exact(shards.iter().map(|s| s.kept.len()).sum())
            .map_err(|_| budget("extraction coupling list allocation"))?;
        for shard in &mut self.shards {
            kept.append(&mut shard.kept);
        }
        kept.par_sort_unstable_by_key(|c| c.0);
        Ok((kept, count))
    }
}

/// The serial sink: applies each record immediately, exactly like the
/// original loop (budget check after every coupling insertion).
pub(super) struct Commit<'n> {
    shards: Vec<&'n mut Shard>,
    shift: u32,
    last: &'n [u32],
    min: f64,
    stats: &'n mut ExtractStats,
    max_couplings: usize,
    keys: usize,
}
impl<'n> Commit<'n> {
    pub(super) fn new(
        acc: &'n mut Accumulators,
        stats: &'n mut ExtractStats,
        max_couplings: usize,
    ) -> Self {
        let keys = acc.keys();
        let Accumulators {
            shards,
            shift,
            last,
            min,
        } = acc;
        Self {
            shift: *shift,
            shards: shards.iter_mut().collect(),
            last: last.as_slice(),
            min: *min,
            stats,
            max_couplings,
            keys,
        }
    }
    pub(super) fn stats(&mut self) -> &mut ExtractStats {
        self.stats
    }
    /// Finalizes, in every shard, the keys due before unit `end`.
    fn finalize(&mut self, end: u32) {
        for shard in &mut self.shards {
            shard.finalize(end, self.min);
        }
    }
    #[inline]
    fn apply(&mut self, op: Op) -> Result<()> {
        let shard = &mut *self.shards[shard_of(op.key, self.shift)];
        if op.key == AREA {
            shard.area += op.pf;
        } else if op.key >= GROUND {
            shard.ground[(op.key as u32 as usize) >> self.shift] += op.pf;
        } else {
            let new = shard.couple(op.key, op.pf, self.last);
            if new {
                self.keys += 1;
                if self.keys > self.max_couplings {
                    return Err(budget("extraction coupling capacitors"));
                }
            }
        }
        Ok(())
    }
}
impl Sink for Commit<'_> {
    #[inline]
    fn op(&mut self, op: Op) -> Flow {
        Ok(self.apply(op)?)
    }
}

/// A chunk's bounded transcript: never grows past its capacity bound.
struct Transcript<'t> {
    ops: &'t mut Vec<Op>,
    cap: usize,
}
impl Sink for Transcript<'_> {
    #[inline]
    fn op(&mut self, op: Op) -> Flow {
        if op.is_null_ground() {
            return Ok(());
        }
        if self.ops.len() >= self.cap {
            return Err(Stop::Defer);
        }
        self.ops.push(op);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Meta {
    ops_end: usize,
    work: usize,
    counts: Counts,
}

#[derive(Default)]
struct Chunk {
    index: u64,
    /// Index of the unit after this chunk's last one.
    end: u32,
    units: Vec<UnitRef>,
    /// Ordered accumulations in run order. Emptied once a complete chunk
    /// is split into `buckets`.
    ops: Vec<Op>,
    /// A complete chunk's records per shard, in emission order.
    buckets: Vec<Vec<Op>>,
    /// Coupling records per bucket (an upper bound of new keys).
    distinct: Vec<usize>,
    /// Per computed unit: its record, or `None` when it was deferred to the
    /// serial path. A unit that failed ends the list (the serial path
    /// reproduces its error at commit).
    metas: Vec<Option<Meta>>,
    complete: bool,
    /// Shards that have not applied this cleared chunk yet.
    unapplied: AtomicUsize,
}
impl Chunk {
    fn bytes(&self) -> usize {
        self.ops.capacity() * size_of::<Op>()
            + self
                .buckets
                .iter()
                .map(|b| b.capacity() * size_of::<Op>())
                .sum::<usize>()
            + self.metas.capacity() * size_of::<Option<Meta>>()
            + self.units.capacity() * size_of::<UnitRef>()
    }
    /// Computes units in order. A unit beyond a parallel bound is deferred;
    /// an error ends the chunk. A complete chunk is split by shard.
    fn compute(&mut self, ctx: &Ctx<'_, '_>, bounds: Bounds, shift: u32, sc: &mut Scratch) {
        self.ops.clear();
        self.metas.clear();
        self.complete = false;
        let cap = ctx.limits.max_candidates.min(bounds.candidates);
        for &u in &self.units {
            let start = self.ops.len();
            let (mut work, mut counts) = (0usize, Counts::default());
            let mut sink = Transcript {
                ops: &mut self.ops,
                cap: bounds.chunk_ops,
            };
            match strip::unit(ctx, u, cap, sc, &mut sink, &mut work, &mut counts) {
                Ok(()) => self.metas.push(Some(Meta {
                    ops_end: self.ops.len(),
                    work,
                    counts,
                })),
                Err(Stop::Defer) => {
                    self.ops.truncate(start);
                    self.metas.push(None);
                }
                Err(Stop::Error(_)) => {
                    self.ops.truncate(start);
                    return;
                }
            }
        }
        self.complete = self.metas.iter().all(Option::is_some);
        if self.complete {
            let shards = 1usize << shift;
            self.buckets.resize_with(shards, Vec::new);
            self.distinct.clear();
            self.distinct.resize(shards, 0);
            split(&self.ops, shift, &mut self.buckets, &mut self.distinct);
            self.ops.clear();
        }
    }
}

/// Splits a chunk's records by shard, each shard's in emission order, so
/// every accumulator sees its records in run order. A strip records one
/// value per accumulator (`strip::Sums`), so records of one key rarely
/// repeat within a chunk and are not grouped. `distinct[s]` receives the
/// shard's coupling records, an upper bound of the keys they create.
fn split(ops: &[Op], shift: u32, buckets: &mut [Vec<Op>], distinct: &mut [usize]) {
    // Each bucket is sized once, to its exact record count.
    let mut lens = [0usize; SHARDS];
    for op in ops {
        let s = shard_of(op.key, shift);
        lens[s] += 1;
        distinct[s] += usize::from(op.key < GROUND);
    }
    for (bucket, &len) in buckets.iter_mut().zip(&lens) {
        bucket.clear();
        bucket.reserve_exact(len);
    }
    for op in ops {
        buckets[shard_of(op.key, shift)].push(*op);
    }
}

/// Upper bound of one chunk's transcript (with vector growth, plus the
/// split copy) and metadata.
fn chunk_bytes(bounds: Bounds, units: usize) -> u64 {
    (bounds.chunk_ops as u64)
        .saturating_mul(4 * size_of::<Op>() as u64)
        .saturating_add(
            (units as u64)
                .saturating_mul((size_of::<Option<Meta>>() + size_of::<UnitRef>()) as u64),
        )
}
/// Bytes one lane may hold: its candidate scratch at the parallel cap, its
/// instance query marks, the chunk it computes, one recycled chunk buffer
/// and small fixed buffers.
pub(super) fn lane_bytes(bounds: Bounds, limits: &ExtractLimits, instances: usize) -> u64 {
    (limits.max_candidates.min(bounds.candidates) as u64)
        .saturating_mul(CANDIDATE_SCRATCH_BYTES as u64)
        .saturating_add((instances as u64).saturating_mul(8))
        .saturating_add(chunk_bytes(bounds, chunk_units(limits)))
        .saturating_add(SPARE_BYTES as u64)
        .saturating_add(1 << 20)
}
/// Share of a chunk a unit takes. A retained passive conductor (supply
/// rails and straps, which cross many cells and often outgrow a chunk's
/// transcript) takes a quarter of a chunk, so such strips are spread over
/// the lanes in small chunks instead of ending in a few large ones that a
/// lane must defer to the serial path. A forced tile size counts units.
/// Results never depend on the chunking.
#[inline]
fn unit_weight(
    ctx: &Ctx<'_, '_>,
    unit: UnitRef,
    per_chunk: usize,
    limits: &ExtractLimits,
) -> usize {
    match unit {
        UnitRef::Conductor(id) if limits.tile_units == 0 && !ctx.shapes.is_virtual(id) => {
            per_chunk.div_ceil(4)
        }
        _ => 1,
    }
}
fn chunk_units(limits: &ExtractLimits) -> usize {
    if limits.tile_units > 0 {
        limits.tile_units
    } else {
        CHUNK_UNITS
    }
}
/// Lanes admitted within `available` bytes (1 selects the serial path).
/// Chunks waiting for their commit add at most the pending bound.
pub(super) fn admit(
    bounds: Bounds,
    threads: usize,
    available: u64,
    limits: &ExtractLimits,
    instances: usize,
) -> usize {
    (2..=threads.max(1))
        .rev()
        .find(|&w| parallel_bytes(bounds, w, limits, instances) <= available)
        .unwrap_or(1)
}
pub(super) fn parallel_bytes(
    bounds: Bounds,
    lanes: usize,
    limits: &ExtractLimits,
    instances: usize,
) -> u64 {
    (lanes as u64)
        .saturating_mul(lane_bytes(bounds, limits, instances))
        .saturating_add(bounds.pending_bytes as u64)
}
/// Phase timings and unit counts for the private profile.
#[derive(Default)]
pub(super) struct Report {
    pub lanes: usize,
    pub shards: usize,
    pub chunks: u64,
    pub units: usize,
    pub serial_units: usize,
    /// Chunks committed at a barrier.
    pub barriers: usize,
    pub ops: usize,
    /// Coupling keys open at once (sum of the shards' peaks).
    pub open_peak: usize,
    pub compute_ns: u128,
    pub commit_ns: u128,
}

fn serial_unit(
    ctx: &Ctx<'_, '_>,
    u: UnitRef,
    sc: &mut Scratch,
    commit: &mut Commit<'_>,
) -> Result<()> {
    let mut counts = Counts::default();
    let mut work = commit.stats().work;
    let result = strip::unit(
        ctx,
        u,
        ctx.limits.max_candidates,
        sc,
        commit,
        &mut work,
        &mut counts,
    );
    commit.stats().work = work;
    match result {
        Ok(()) => {
            counts.add_to(commit.stats());
            Ok(())
        }
        Err(Stop::Error(e)) => Err(e),
        Err(Stop::Defer) => Err(invalid("serial strip deferred")),
    }
}

struct Source<I> {
    units: I,
    next: u64,
    /// Units taken so far.
    taken: u32,
    done: bool,
}
struct Pending {
    chunks: BTreeMap<u64, Arc<Chunk>>,
    bytes: usize,
    spare: Vec<Chunk>,
}
/// One shard's accumulators and the next chunk it applies.
struct Slot<'n> {
    acc: &'n mut Shard,
    turn: u64,
}
/// In-order chunk clearance: statistics, cumulative work and budgets.
struct Leader<'n> {
    stats: &'n mut ExtractStats,
    /// Next chunk to clear.
    turn: u64,
    /// Coupling keys after every cleared chunk is applied, plus `savings`:
    /// `bound - savings` never undercounts the keys.
    bound: usize,
    serial: Option<Scratch>,
    error: Option<Error>,
    report: Report,
}

struct Shared<'s, 'n> {
    shift: u32,
    /// Completion schedule of the coupling keys (`Accumulators::last`).
    last: &'n [u32],
    min: f64,
    max_couplings: usize,
    instances: usize,
    shards: Vec<Mutex<Slot<'n>>>,
    /// Lock-free copies of each shard's turn.
    turns: Vec<AtomicU64>,
    /// Chunks below this are cleared (or committed at a barrier).
    cleared: AtomicU64,
    /// Over applied shard records: coupling records minus new keys.
    savings: AtomicUsize,
    leader: Mutex<Leader<'n>>,
    pending: &'s Mutex<Pending>,
    space: &'s Condvar,
    failed: &'s AtomicBool,
    commit_ns: AtomicUsize,
}

/// The leader's serial scratch, allocated on first use.
fn scratch(serial: &mut Option<Scratch>, instances: usize) -> Result<&mut Scratch> {
    Ok(match serial {
        Some(sc) => sc,
        None => serial.insert(Scratch::new(instances)?),
    })
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<'n> Shared<'_, 'n> {
    /// Applies chunk `slot.turn` to shard `s`; the last shard releases it.
    fn apply_next(&self, s: usize, slot: &mut Slot<'n>) {
        let index = slot.turn;
        let chunk = lock(self.pending)
            .chunks
            .get(&index)
            .cloned()
            .expect("a cleared chunk stays pending until every shard applied it");
        let fresh = slot
            .acc
            .apply_grouped(&chunk.buckets[s], self.shift, self.last);
        slot.acc.finalize(chunk.end, self.min);
        self.savings
            .fetch_add(chunk.distinct[s] - fresh, Ordering::AcqRel);
        slot.turn += 1;
        self.turns[s].store(slot.turn, Ordering::Release);
        let last = chunk.unapplied.fetch_sub(1, Ordering::AcqRel) == 1;
        drop(chunk);
        if last {
            self.release(index);
        }
    }
    /// Removes a fully committed chunk, keeping a small buffer for reuse.
    fn release(&self, index: u64) {
        let mut p = lock(self.pending);
        if let Some(c) = p.chunks.remove(&index) {
            p.bytes -= c.bytes();
            if let Ok(c) = Arc::try_unwrap(c)
                && p.spare.len() < self.shards.len().max(2)
                && c.bytes() <= SPARE_BYTES
            {
                p.spare.push(c);
            }
        }
        drop(p);
        self.space.notify_all();
    }
    /// Applies every cleared chunk to shard `s` unless another lane holds
    /// it. Whoever holds a shard re-checks after releasing it, so a chunk
    /// cleared meanwhile is never left behind.
    fn drain(&self, s: usize) {
        loop {
            if self.turns[s].load(Ordering::Acquire) >= self.cleared.load(Ordering::Acquire) {
                return;
            }
            let Ok(mut slot) = self.shards[s].try_lock() else {
                return;
            };
            let started = std::time::Instant::now();
            while slot.turn < self.cleared.load(Ordering::Acquire) {
                self.apply_next(s, &mut slot);
            }
            drop(slot);
            self.commit_ns
                .fetch_add(started.elapsed().as_nanos() as usize, Ordering::Relaxed);
        }
    }
    /// Every shard applies every chunk below `upto`; the returned guards
    /// hold all shards at that point.
    fn barrier(&self, upto: u64) -> Vec<MutexGuard<'_, Slot<'n>>> {
        let mut guards = Vec::with_capacity(self.shards.len());
        for (s, m) in self.shards.iter().enumerate() {
            let mut slot = lock(m);
            while slot.turn < upto {
                self.apply_next(s, &mut slot);
            }
            guards.push(slot);
        }
        guards
    }
    /// Clears every pending chunk in order unless another lane leads.
    fn lead(&self, ctx: &Ctx<'_, '_>) {
        loop {
            let Ok(mut leader) = self.leader.try_lock() else {
                return;
            };
            let started = std::time::Instant::now();
            while leader.error.is_none() {
                let turn = leader.turn;
                let Some(chunk) = lock(self.pending).chunks.get(&turn).cloned() else {
                    break;
                };
                if let Err(e) = self.clear(ctx, &mut leader, chunk) {
                    leader.error = Some(e);
                    self.failed.store(true, Ordering::Release);
                    self.space.notify_all();
                }
            }
            drop(leader);
            self.commit_ns
                .fetch_add(started.elapsed().as_nanos() as usize, Ordering::Relaxed);
            let next = self.cleared.load(Ordering::Acquire);
            if self.failed.load(Ordering::Acquire) || !lock(self.pending).chunks.contains_key(&next)
            {
                return;
            }
        }
    }
    /// Clears chunk `leader.turn`, or commits it at a barrier.
    fn clear(&self, ctx: &Ctx<'_, '_>, leader: &mut Leader<'n>, chunk: Arc<Chunk>) -> Result<()> {
        let limits = ctx.limits;
        let before = leader.report.units;
        if chunk.complete {
            leader.report.units += chunk.units.len();
            if before / 4096 != leader.report.units / 4096 || before == 0 {
                guard_memory(limits)?;
            }
            // Grouped records are exact when no budget can fail inside the
            // chunk; otherwise the serial path recomputes every unit,
            // failing at the exact serial point.
            let work = chunk
                .metas
                .iter()
                .flatten()
                .try_fold(leader.stats.work, |w, m| w.checked_add(m.work))
                .filter(|&w| w <= limits.max_work);
            let distinct: usize = chunk.distinct.iter().sum();
            let fits = |keys: usize| {
                keys.checked_add(distinct)
                    .is_some_and(|n| n <= self.max_couplings)
            };
            let estimate = leader
                .bound
                .saturating_sub(self.savings.load(Ordering::Acquire));
            let mut guards = None;
            let clear = work.is_some()
                && (fits(estimate) || {
                    // Exact count once every earlier chunk is applied.
                    let g = self.barrier(leader.turn);
                    let exact = leader
                        .bound
                        .saturating_sub(self.savings.load(Ordering::Acquire));
                    guards = Some(g);
                    fits(exact)
                });
            if let (true, Some(work)) = (clear, work) {
                drop(guards);
                leader.stats.work = work;
                for meta in chunk.metas.iter().flatten() {
                    meta.counts.add_to(leader.stats);
                }
                leader.report.ops += chunk.buckets.iter().map(Vec::len).sum::<usize>();
                leader.bound += distinct;
                chunk.unapplied.store(self.shards.len(), Ordering::Release);
                drop(chunk);
                leader.turn += 1;
                self.cleared.store(leader.turn, Ordering::Release);
                return Ok(());
            }
            let mut guards = match guards {
                Some(g) => g,
                None => self.barrier(leader.turn),
            };
            leader.report.barriers += 1;
            let keys = self.serial_chunk(ctx, leader, &mut guards, &chunk, true)?;
            return self.finish_barrier(leader, guards, chunk, keys);
        }
        let mut guards = self.barrier(leader.turn);
        leader.report.barriers += 1;
        let keys = self.serial_chunk(ctx, leader, &mut guards, &chunk, false)?;
        self.finish_barrier(leader, guards, chunk, keys)
    }
    /// Commits a chunk with every shard held: a complete chunk is recomputed
    /// unit by unit (a budget may fail inside it); otherwise lane records
    /// are applied unit by unit and unfinished units are computed here.
    fn serial_chunk(
        &self,
        ctx: &Ctx<'_, '_>,
        leader: &mut Leader<'n>,
        guards: &mut [MutexGuard<'_, Slot<'n>>],
        chunk: &Chunk,
        recompute: bool,
    ) -> Result<usize> {
        let limits = ctx.limits;
        let keys = leader
            .bound
            .saturating_sub(self.savings.load(Ordering::Acquire));
        let Leader {
            stats,
            serial,
            report,
            ..
        } = leader;
        let mut commit = Commit {
            shards: guards.iter_mut().map(|g| &mut *g.acc).collect(),
            shift: self.shift,
            last: self.last,
            min: self.min,
            stats,
            max_couplings: self.max_couplings,
            keys,
        };
        debug_assert_eq!(
            commit.keys,
            commit.shards.iter().map(|s| s.keys).sum::<usize>()
        );
        let instances = self.instances;
        if recompute {
            for &u in &chunk.units {
                report.serial_units += 1;
                serial_unit(ctx, u, scratch(serial, instances)?, &mut commit)?;
            }
            return Ok(commit.keys);
        }
        let mut from = 0;
        for (j, &u) in chunk.units.iter().enumerate() {
            if report.units.is_multiple_of(4096) {
                guard_memory(limits)?;
            }
            report.units += 1;
            let Some(meta) = chunk.metas.get(j).copied().flatten() else {
                // Not finished by its lane: the serial path reproduces an
                // error exactly, or completes a deferred unit.
                report.serial_units += 1;
                serial_unit(ctx, u, scratch(serial, instances)?, &mut commit)?;
                continue;
            };
            let total = commit
                .stats
                .work
                .checked_add(meta.work)
                .filter(|&n| n <= limits.max_work);
            let Some(total) = total else {
                // Cumulative work overflows inside this unit: replay it
                // serially from the committed work to fail at the exact
                // serial charge.
                serial_unit(ctx, u, scratch(serial, instances)?, &mut commit)?;
                return Err(invalid("strip work overflow did not reproduce serially"));
            };
            commit.stats.work = total;
            for &op in &chunk.ops[from..meta.ops_end] {
                commit.apply(op)?;
            }
            report.ops += meta.ops_end - from;
            from = meta.ops_end;
            meta.counts.add_to(commit.stats);
        }
        Ok(commit.keys)
    }
    /// Advances every shard past a chunk committed at a barrier.
    fn finish_barrier(
        &self,
        leader: &mut Leader<'n>,
        guards: Vec<MutexGuard<'_, Slot<'n>>>,
        chunk: Arc<Chunk>,
        keys: usize,
    ) -> Result<()> {
        let next = leader.turn + 1;
        for (s, mut g) in guards.into_iter().enumerate() {
            g.acc.finalize(chunk.end, self.min);
            g.turn = next;
            self.turns[s].store(next, Ordering::Release);
        }
        leader.bound = keys.saturating_add(self.savings.load(Ordering::Acquire));
        let index = chunk.index;
        drop(chunk);
        leader.turn = next;
        self.cleared.store(next, Ordering::Release);
        self.release(index);
        Ok(())
    }
    /// Commit work any lane can do: clear chunks, then apply shards.
    fn help(&self, ctx: &Ctx<'_, '_>, first: usize) {
        self.lead(ctx);
        let n = self.shards.len();
        for k in 0..n {
            self.drain((first + k) % n);
        }
    }
}

/// Computes and commits every unit. `lanes <= 1` without a forced chunk
/// size runs the serial path directly.
pub(super) fn run(
    ctx: &Ctx<'_, '_>,
    units: &mut (impl Iterator<Item = UnitRef> + Send),
    bounds: Bounds,
    lanes: usize,
    instances: usize,
    acc: &mut Accumulators,
    stats: &mut ExtractStats,
) -> Result<Report> {
    let limits = ctx.limits;
    if lanes <= 1 && limits.tile_units == 0 {
        let mut commit = Commit::new(acc, stats, limits.max_couplings);
        let mut report = Report {
            lanes: 1,
            shards: commit.shards.len(),
            ..Default::default()
        };
        let mut sc = Scratch::new(instances)?;
        let started = std::time::Instant::now();
        for (n, u) in units.enumerate() {
            if n.is_multiple_of(4096) {
                guard_memory(limits)?;
            }
            serial_unit(ctx, u, &mut sc, &mut commit)?;
            report.units += 1;
            report.serial_units += 1;
            if (n + 1).is_multiple_of(1 << GRANULE_SHIFT) {
                commit.finalize((n + 1) as u32);
            }
        }
        commit.finalize(report.units as u32);
        report.compute_ns = started.elapsed().as_nanos();
        return Ok(report);
    }
    let lanes = lanes.max(1);
    let per_chunk = chunk_units(limits);
    let shift = acc.shift;
    let mut scratch = Vec::with_capacity(lanes);
    for _ in 0..lanes {
        scratch.push(Scratch::new(instances)?);
    }
    let source = Mutex::new(Source {
        units,
        next: 0,
        taken: 0,
        done: false,
    });
    let pending = Mutex::new(Pending {
        chunks: BTreeMap::new(),
        bytes: 0,
        spare: Vec::new(),
    });
    let space = Condvar::new();
    let failed = AtomicBool::new(false);
    let compute_ns = AtomicUsize::new(0);
    let shard_count = acc.shards.len();
    let Accumulators {
        shards: acc_shards,
        last,
        min,
        ..
    } = acc;
    let shared = Shared {
        shift,
        last: last.as_slice(),
        min: *min,
        max_couplings: limits.max_couplings,
        instances,
        shards: acc_shards
            .iter_mut()
            .map(|acc| Mutex::new(Slot { acc, turn: 0 }))
            .collect(),
        turns: (0..shard_count).map(|_| AtomicU64::new(0)).collect(),
        cleared: AtomicU64::new(0),
        savings: AtomicUsize::new(0),
        leader: Mutex::new(Leader {
            stats,
            turn: 0,
            bound: 0,
            serial: None,
            error: None,
            report: Report {
                lanes,
                shards: shard_count,
                ..Default::default()
            },
        }),
        pending: &pending,
        space: &space,
        failed: &failed,
        commit_ns: AtomicUsize::new(0),
    };
    rayon::scope(|s| {
        for (lane, sc) in scratch.iter_mut().enumerate() {
            let (source, pending, space, failed) = (&source, &pending, &space, &failed);
            let (shared, compute_ns) = (&shared, &compute_ns);
            s.spawn(move |_| {
                // A panicking lane must not leave the others waiting for
                // its chunk.
                struct Abort<'g>(&'g AtomicBool, &'g Condvar);
                impl Drop for Abort<'_> {
                    fn drop(&mut self) {
                        if std::thread::panicking() {
                            self.0.store(true, Ordering::Release);
                            self.1.notify_all();
                        }
                    }
                }
                let _abort = Abort(failed, space);
                loop {
                    // Wait for room, committing whatever is ready meanwhile.
                    loop {
                        if failed.load(Ordering::Acquire) {
                            return;
                        }
                        if lock(pending).bytes <= bounds.pending_bytes {
                            break;
                        }
                        shared.help(ctx, lane);
                        let p = lock(pending);
                        if p.bytes > bounds.pending_bytes && !failed.load(Ordering::Acquire) {
                            let _ = space
                                .wait_timeout(p, std::time::Duration::from_millis(1))
                                .unwrap_or_else(|e| e.into_inner());
                        }
                    }
                    let mut chunk = lock(pending).spare.pop().unwrap_or_default();
                    {
                        let mut src = lock(source);
                        if src.done {
                            return;
                        }
                        chunk.units.clear();
                        let mut weight = 0;
                        while weight < per_chunk {
                            let Some(u) = src.units.next() else {
                                src.done = true;
                                break;
                            };
                            weight += unit_weight(ctx, u, per_chunk, limits);
                            chunk.units.push(u);
                        }
                        if chunk.units.is_empty() {
                            return;
                        }
                        chunk.index = src.next;
                        src.next += 1;
                        src.taken += chunk.units.len() as u32;
                        chunk.end = src.taken;
                    }
                    let started = std::time::Instant::now();
                    chunk.compute(ctx, bounds, shift, sc);
                    compute_ns.fetch_add(started.elapsed().as_nanos() as usize, Ordering::Relaxed);
                    {
                        let mut p = lock(pending);
                        p.bytes += chunk.bytes();
                        p.chunks.insert(chunk.index, Arc::new(chunk));
                    }
                    shared.help(ctx, lane);
                }
            });
        }
    });
    let chunks = source.into_inner().unwrap_or_else(|e| e.into_inner()).next;
    // Clear and apply whatever the lanes left (normally nothing).
    if !failed.load(Ordering::Acquire) {
        shared.lead(ctx);
        drop(shared.barrier(shared.cleared.load(Ordering::Acquire)));
    }
    let turns: Vec<u64> = shared
        .turns
        .iter()
        .map(|t| t.load(Ordering::Acquire))
        .collect();
    let commit_ns = shared.commit_ns.load(Ordering::Relaxed);
    let leader = shared
        .leader
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(e) = leader.error {
        return Err(e);
    }
    if leader.turn != chunks || turns.iter().any(|&t| t != chunks) {
        return Err(invalid("extraction chunks left uncommitted"));
    }
    let mut report = leader.report;
    report.chunks = chunks;
    report.compute_ns = compute_ns.into_inner() as u128;
    report.commit_ns = commit_ns as u128;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A skewed stream of ground, coupling and area records with
    /// non-associative values (hot and cold accumulators).
    fn stream(n: usize, nodes: u32) -> Vec<Op> {
        let mut seed = 11u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        (0..n)
            .map(|i| {
                let r = next();
                let pf = (r % 1000) as f64 * 1.000_000_1e-3 + 1e-17;
                match r % 7 {
                    0 | 1 => Op::ground((r >> 3) as u32 % nodes, pf),
                    2 => Op::area(pf),
                    _ => {
                        let k = if i % 3 == 0 { r % 7 } else { r % 400 } as u32;
                        Op::couple(k % nodes, (k * 7 + 1) % nodes, pf)
                    }
                }
            })
            .collect()
    }
    /// The original accumulators: a `BTreeMap` keyed in run order.
    fn reference(ops: &[Op], nodes: usize) -> (Vec<u64>, Vec<(u64, u64)>, u64) {
        let mut ground = vec![0.0f64; nodes];
        let mut coupling = BTreeMap::<u64, f64>::new();
        let mut area = 0.0f64;
        for op in ops {
            if op.key == AREA {
                area += op.pf;
            } else if op.key >= GROUND {
                ground[op.key as u32 as usize] += op.pf;
            } else {
                *coupling.entry(op.key).or_insert(0.0) += op.pf;
            }
        }
        (
            ground.iter().map(|v| v.to_bits()).collect(),
            coupling.iter().map(|(k, v)| (*k, v.to_bits())).collect(),
            area.to_bits(),
        )
    }
    /// Raw accumulators of an unscheduled set (nothing finalized yet).
    fn bits(acc: &Accumulators, nodes: usize) -> (Vec<u64>, Vec<(u64, u64)>, u64) {
        let ground = (0..nodes as u32).map(|g| acc.ground(g).to_bits()).collect();
        let mut couplings: Vec<(u64, u64)> = acc
            .shards
            .iter()
            .flat_map(|s| s.couplings.live().map(|(_, k, v)| (k, v.to_bits())))
            .collect();
        couplings.sort_unstable();
        (ground, couplings, acc.area().to_bits())
    }
    /// The stream as units of `per` records, with the last unit of every node.
    fn schedule(ops: &[Op], nodes: usize, per: usize) -> Vec<u32> {
        let mut last = vec![0u32; nodes];
        for (i, op) in ops.iter().enumerate() {
            if op.key < GROUND {
                for g in [(op.key >> 32) as usize, op.key as u32 as usize] {
                    last[g] = last[g].max((i / per) as u32);
                }
            }
        }
        last
    }
    /// The finished network written out: strip ground, then per shard in
    /// shard order the couplings it grounds in `(due, key)` order.
    fn expected(
        ops: &[Op],
        nodes: usize,
        last: &[u32],
        min: f64,
    ) -> (Vec<u64>, Vec<(u64, u64)>, usize) {
        let mut ground = vec![0.0f64; nodes];
        let mut coupling = BTreeMap::<u64, f64>::new();
        for op in ops {
            if op.key >= GROUND {
                if op.key != AREA {
                    ground[op.key as u32 as usize] += op.pf;
                }
            } else {
                *coupling.entry(op.key).or_insert(0.0) += op.pf;
            }
        }
        let mut order: Vec<(u32, u64, f64)> = coupling
            .iter()
            .map(|(&k, &v)| (due(last, k), k, v))
            .collect();
        order.sort_by_key(|&(d, k, _)| (d, k));
        let mut per_shard = vec![vec![0.0f64; nodes]; SHARDS];
        let (mut kept, mut count) = (Vec::new(), 0);
        for &(_, key, pf) in &order {
            if pf <= 0.0 {
                continue;
            }
            if pf < min {
                let s = &mut per_shard[shard_of(key, SHARD_SHIFT)];
                s[(key >> 32) as usize] += pf;
                s[key as u32 as usize] += pf;
                count += 1;
            } else {
                kept.push((key, pf.to_bits()));
            }
        }
        kept.sort_unstable();
        for (g, v) in ground.iter_mut().enumerate() {
            for s in &per_shard {
                *v += s[g];
            }
        }
        (ground.iter().map(|g| g.to_bits()).collect(), kept, count)
    }
    fn finished(acc: Accumulators, sizes: &[usize]) -> (Vec<u64>, Vec<(u64, u64)>, usize) {
        let mut grounds: Vec<Vec<f64>> = sizes.iter().map(|&n| vec![f64::NAN; n]).collect();
        let mut base = 0u32;
        let mut nets: Vec<(u32, &mut [f64])> = grounds
            .iter_mut()
            .map(|g| {
                base += g.len() as u32;
                (base - g.len() as u32, &mut g[..])
            })
            .collect();
        let (kept, count) = acc.finish(&mut nets).unwrap();
        (
            grounds.iter().flatten().map(|g| g.to_bits()).collect(),
            kept.iter().map(|&(k, v)| (k, v.to_bits())).collect(),
            count,
        )
    }

    #[test]
    fn finish_grounds_each_shards_couplings_in_completion_order() {
        let nodes = 97usize;
        let mut ops = stream(30_000, nodes as u32);
        // Some couplings end below zero (finishing drops them); the stream
        // also couples some nodes with themselves.
        ops.extend((0..40u32).map(|k| Op::couple(k, (k * 7 + 1) % nodes as u32, -1e3)));
        let sizes = [10usize, 1, 30, 55, 1];
        assert_eq!(sizes.iter().sum::<usize>(), nodes);
        let mut totals = BTreeMap::<u64, f64>::new();
        for op in ops.iter().filter(|op| op.key < GROUND) {
            *totals.entry(op.key).or_insert(0.0) += op.pf;
        }
        let mut positive: Vec<f64> = totals.values().copied().filter(|&v| v > 0.0).collect();
        positive.sort_by(f64::total_cmp);
        assert!(positive.len() > 10 && positive.len() < totals.len());
        let median = positive[positive.len() / 2];
        let per = 50;
        let units = ops.len().div_ceil(per);
        let scheduled = schedule(&ops, nodes, per);
        for min in [0.0, positive[3], median, f64::INFINITY] {
            for last in [Vec::new(), scheduled.clone()] {
                let want = expected(&ops, nodes, &last, min);
                assert!(min != median || (want.2 > 0 && !want.1.is_empty()));
                // The serial sink, finalizing after every unit, after every
                // seventh, or only at the end.
                for every in [1usize, 7, usize::MAX] {
                    let mut acc = Accumulators::new(nodes, 0.0, last.clone(), units, min).unwrap();
                    let mut stats = ExtractStats::default();
                    let mut commit = Commit::new(&mut acc, &mut stats, usize::MAX);
                    for (i, &op) in ops.iter().enumerate() {
                        commit.apply(op).unwrap();
                        let unit = i / per + 1;
                        if (i + 1) % per == 0 && unit % every.min(units) == 0 {
                            commit.finalize(unit as u32);
                        }
                    }
                    if every == 1 && !last.is_empty() {
                        commit.finalize(units as u32);
                        // Every scheduled key has left the tables.
                        assert_eq!(acc.open(), 0);
                    }
                    assert_eq!(finished(acc, &sizes), want, "{min} {every}");
                }
                // Grouped chunks applied shard by shard, each shard
                // finalizing at its chunk ends (shards progress apart).
                for chunk_units in [1usize, 3, 64] {
                    let mut acc = Accumulators::new(nodes, 0.0, last.clone(), units, min).unwrap();
                    let mut chunks = Vec::new();
                    for (c, chunk) in ops.chunks(per * chunk_units).enumerate() {
                        let mut buckets = vec![Vec::new(); SHARDS];
                        let mut distinct = vec![0; SHARDS];
                        split(chunk, SHARD_SHIFT, &mut buckets, &mut distinct);
                        let end = ((c + 1) * chunk_units).min(units) as u32;
                        chunks.push((buckets, end));
                    }
                    let Accumulators { shards, last, .. } = &mut acc;
                    for (s, shard) in shards.iter_mut().enumerate().rev() {
                        for (buckets, end) in &chunks {
                            shard.apply_grouped(&buckets[s], SHARD_SHIFT, last);
                            shard.finalize(*end, min);
                        }
                    }
                    assert_eq!(finished(acc, &sizes), want, "{min} {chunk_units}");
                }
            }
        }
    }

    #[test]
    fn sharded_records_match_ordered_updates_bit_for_bit() {
        let nodes = 97usize;
        let ops = stream(30_000, nodes as u32);
        let want = reference(&ops, nodes);
        let mut stats = ExtractStats::default();
        // The serial sink, one record at a time.
        let mut acc = Accumulators::new(nodes, 0.0, Vec::new(), 0, 0.0).unwrap();
        let mut commit = Commit::new(&mut acc, &mut stats, usize::MAX);
        for &op in &ops {
            commit.apply(op).unwrap();
        }
        assert_eq!(acc.keys(), want.1.len());
        assert_eq!(bits(&acc, nodes), want);
        // Chunks split by shard and grouped by accumulator, applied shard
        // by shard in chunk order (shards progress independently).
        let mut acc = Accumulators::new(nodes, 0.0, Vec::new(), 0, 0.0).unwrap();
        let chunks: Vec<Vec<Vec<Op>>> = ops
            .chunks(777)
            .map(|chunk| {
                let mut buckets = vec![Vec::new(); SHARDS];
                for &op in chunk {
                    buckets[shard_of(op.key, SHARD_SHIFT)].push(op);
                }
                for b in &mut buckets {
                    b.sort_by_key(|op| op.key);
                }
                buckets
            })
            .collect();
        for (s, shard) in acc.shards.iter_mut().enumerate().rev() {
            for chunk in &chunks {
                shard.apply_grouped(&chunk[s], SHARD_SHIFT, &[]);
            }
        }
        assert_eq!(bits(&acc, nodes), want);
        // The lane split: one pass per chunk, each shard's records in
        // emission order, its coupling records counted.
        let mut acc = Accumulators::new(nodes, 0.0, Vec::new(), 0, 0.0).unwrap();
        for chunk in ops.chunks(777) {
            let mut buckets = vec![Vec::new(); SHARDS];
            let mut distinct = vec![0; SHARDS];
            split(chunk, SHARD_SHIFT, &mut buckets, &mut distinct);
            for (s, bucket) in buckets.iter().enumerate() {
                let mine: Vec<(u64, u64)> = chunk
                    .iter()
                    .filter(|op| shard_of(op.key, SHARD_SHIFT) == s)
                    .map(|op| (op.key, op.pf.to_bits()))
                    .collect();
                let got: Vec<(u64, u64)> =
                    bucket.iter().map(|op| (op.key, op.pf.to_bits())).collect();
                assert_eq!(got, mine);
                assert_eq!(distinct[s], mine.iter().filter(|m| m.0 < GROUND).count());
                acc.shards[s].apply_grouped(bucket, SHARD_SHIFT, &[]);
            }
        }
        assert_eq!(bits(&acc, nodes), want);
    }

    #[test]
    fn finalized_slots_are_reused_and_the_table_shrinks_to_the_open_keys() {
        let mut table = Couplings::new();
        // Keys come and go: at most 3,000 are open at once.
        let mut open = std::collections::VecDeque::new();
        for k in 0..60_000u64 {
            let key = k * 0x1_0000_0001 % 1_000_003;
            let (i, new) = table.find_or_insert(key, || k as u32);
            assert!(new);
            table.set(i, k as f64);
            open.push_back((i, key, k));
            if open.len() > 3_000 {
                let (i, key, v) = open.pop_front().unwrap();
                assert_eq!(table.slot(i).key(), key);
                assert_eq!(table.get(i).to_bits(), (v as f64).to_bits());
                table.remove(i);
            }
        }
        assert_eq!(table.open, 3_000);
        assert!(table.len <= 3_001, "{}", table.len);
        for &(i, key, v) in &open {
            assert_eq!(table.find_or_insert(key, || 0), (i, false));
            assert_eq!(table.get(i).to_bits(), (v as f64).to_bits());
        }
        let mut live: Vec<u64> = table.live().map(|(_, k, _)| k).collect();
        live.sort_unstable();
        let mut want: Vec<u64> = open.iter().map(|o| o.1).collect();
        want.sort_unstable();
        assert_eq!(live, want);
    }

    #[test]
    fn null_ground_records_change_no_accumulator_bit() {
        let nodes = 97usize;
        // Every ground record is followed by +0.0, by -0.0, or by a
        // cancellation through -0.0 back to its value.
        let mut ops = Vec::new();
        for (i, op) in stream(20_000, nodes as u32).into_iter().enumerate() {
            ops.push(op);
            if op.key >= GROUND && op.key != AREA {
                let g = op.key as u32;
                match i % 3 {
                    0 => ops.push(Op::ground(g, 0.0)),
                    1 => ops.push(Op::ground(g, -0.0)),
                    _ => ops.extend([
                        Op::ground(g, -op.pf),
                        Op::ground(g, -0.0),
                        Op::ground(g, op.pf),
                    ]),
                }
            }
        }
        let nulls = ops.iter().filter(|op| op.is_null_ground()).count();
        assert!(nulls > 3000);
        assert!(!Op::area(0.0).is_null_ground() && !Op::couple(1, 2, 0.0).is_null_ground());
        // The original accumulators add every record; none ends at -0.0.
        let want = reference(&ops, nodes);
        assert!(want.0.iter().all(|&g| g != (-0.0f64).to_bits()));
        // Without the null records, the serial sink gives the same bits.
        let kept: Vec<Op> = ops
            .iter()
            .copied()
            .filter(|op| !op.is_null_ground())
            .collect();
        assert_eq!(reference(&kept, nodes), want);
        let mut acc = Accumulators::new(nodes, 0.0, Vec::new(), 0, 0.0).unwrap();
        let mut stats = ExtractStats::default();
        let mut commit = Commit::new(&mut acc, &mut stats, usize::MAX);
        for &op in &ops {
            commit.apply(op).unwrap();
        }
        assert_eq!(bits(&acc, nodes), want);
        // A transcript stores none of them; its split records give the
        // same bits.
        let mut stored = Vec::new();
        let mut transcript = Transcript {
            ops: &mut stored,
            cap: usize::MAX,
        };
        for &op in &ops {
            assert!(transcript.op(op).is_ok());
        }
        assert_eq!(stored.len(), ops.len() - nulls);
        let mut acc = Accumulators::new(nodes, 0.0, Vec::new(), 0, 0.0).unwrap();
        for chunk in stored.chunks(777) {
            let mut buckets = vec![Vec::new(); SHARDS];
            let mut distinct = vec![0; SHARDS];
            split(chunk, SHARD_SHIFT, &mut buckets, &mut distinct);
            for (s, shard) in acc.shards.iter_mut().enumerate() {
                shard.apply_grouped(&buckets[s], SHARD_SHIFT, &[]);
            }
        }
        assert_eq!(bits(&acc, nodes), want);
    }

    #[test]
    fn coupling_budget_fails_at_the_first_excess_key() {
        let nodes = 50usize;
        let ops = stream(5_000, nodes as u32);
        let keys = reference(&ops, nodes).1.len();
        for last in [Vec::new(), schedule(&ops, nodes, 10)] {
            let mut stats = ExtractStats::default();
            let mut acc = Accumulators::new(nodes, 0.0, last, ops.len() / 10 + 1, 0.0).unwrap();
            let mut commit = Commit::new(&mut acc, &mut stats, keys - 1);
            let failed = ops.iter().position(|&op| commit.apply(op).is_err());
            // The first record that creates key number `keys`.
            let mut seen = std::collections::HashSet::new();
            let last_new = ops
                .iter()
                .enumerate()
                .filter(|(_, op)| op.key < GROUND && seen.insert(op.key))
                .map(|(i, _)| i)
                .last();
            assert_eq!(failed, last_new);
        }
    }

    #[test]
    fn admission_is_conservative_saturating_and_monotone() {
        let limits = ExtractLimits {
            max_candidates: 65_536,
            ..Default::default()
        };
        let b = Bounds::default();
        let lane = lane_bytes(b, &limits, 20_000);
        assert_eq!(
            lane,
            16_384 * CANDIDATE_SCRATCH_BYTES as u64
                + 20_000 * 8
                + chunk_bytes(b, CHUNK_UNITS)
                + SPARE_BYTES as u64
                + (1 << 20)
        );
        let need = |w: usize| parallel_bytes(b, w, &limits, 20_000);
        assert_eq!(need(4), 4 * lane + (4 << 20));
        assert_eq!(admit(b, 4, need(4), &limits, 20_000), 4);
        assert_eq!(admit(b, 4, need(4) - 1, &limits, 20_000), 3);
        assert_eq!(admit(b, 4, need(2) - 1, &limits, 20_000), 1);
        assert_eq!(admit(b, 1, u64::MAX, &limits, 20_000), 1);
        assert_eq!(admit(b, 64, 1 << 40, &limits, usize::MAX), 1);
        assert_eq!(admit(b, 64, u64::MAX, &limits, usize::MAX), 64);
        assert!(need(4) < 64 << 20);
    }

    #[test]
    fn shards_own_their_accumulators_at_every_shift() {
        for shift in 0..6u32 {
            for key in [0u64, 7, 1 << 40, GROUND | 5, GROUND | 0xFFFF_FFFE, AREA] {
                assert!(shard_of(key, shift) < 1 << shift);
            }
            assert_eq!(shard_of(AREA, shift), 0);
            assert_eq!(shard_of(GROUND | 13, shift), 13 & ((1 << shift) - 1));
        }
        assert_eq!(1 << SHARD_SHIFT, SHARDS);
    }
}
