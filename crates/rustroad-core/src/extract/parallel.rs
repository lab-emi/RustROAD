//! Strip computation on the `--threads` pool with an order-preserving,
//! sharded commit.
//!
//! Units (a wire body, or the x/y strip pair of one rectangle) are taken from
//! the lazy unit iterator in numbered chunks. Lanes compute chunks
//! concurrently into bounded transcripts of ordered accumulations.
//!
//! Every accumulator (the ground capacitance of one RC node, one coupling
//! capacitor, the overlap-area statistic) belongs to exactly one shard.
//! A complete chunk's transcript is split by shard, and within a shard it is
//! stably grouped by accumulator, so each accumulator keeps the run order of
//! its additions. A leader clears chunks strictly in run order: it applies
//! integer statistics and cumulative work and proves that no budget can fail
//! inside the chunk. Each shard then applies the cleared chunks' records for
//! its own accumulators, in chunk order, on whichever lane is free. Shards
//! never touch each other's accumulators, so every accumulator sees the
//! identical sequence of f64 additions as the serial loop at any thread
//! count, chunk size and shard count.
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
/// Largest shard count (a power of two).
const MAX_SHARDS: usize = 64;

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
}
impl Default for Bounds {
    fn default() -> Self {
        Self {
            candidates: 16_384,
            chunk_ops: 1 << 17,
            pending_bytes: 4 << 20,
            virtual_grid: true,
            net_bin_list: super::NET_BIN_LIST,
        }
    }
}

/// Entries per arena chunk of a coupling table (20 bytes each).
const CHUNK_ENTRIES: usize = 1 << 15;
const NIL: u32 = u32::MAX;

/// One coupling accumulator: key and value bits in 32-bit halves (20
/// bytes, 4-byte aligned) and the bucket chain link.
#[derive(Clone, Copy)]
struct Cell {
    key: [u32; 2],
    value: [u32; 2],
    next: u32,
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

/// The coupling accumulators of one shard: a chained hash table whose
/// entries live in fixed-size arena chunks. Growth appends a chunk and now
/// and then rebuilds the 4-byte head array; entries are never moved, so
/// the table never holds two generations of its entries.
pub(super) struct Couplings {
    heads: Vec<u32>,
    bits: u32,
    chunks: Vec<Vec<Cell>>,
    len: usize,
}
impl Couplings {
    const MIN_BITS: u32 = 10;
    fn new() -> Self {
        Self {
            heads: vec![NIL; 1 << Self::MIN_BITS],
            bits: Self::MIN_BITS,
            chunks: Vec::new(),
            len: 0,
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
    /// Index of `key`'s accumulator (created at 0.0) and whether it is new.
    #[inline]
    fn find_or_insert(&mut self, key: u64) -> (u32, bool) {
        let mut i = self.heads[Self::bucket(key, self.bits)];
        while i != NIL {
            let slot = self.slot(i);
            if slot.key() == key {
                return (i, false);
            }
            i = slot.next;
        }
        if self.len >= 2 << self.bits {
            self.grow();
        }
        let b = Self::bucket(key, self.bits);
        let index = u32::try_from(self.len)
            .ok()
            .filter(|&i| i != NIL)
            .expect("coupling count is bounded by max_couplings");
        if self.len.is_multiple_of(CHUNK_ENTRIES) {
            self.chunks.push(Vec::with_capacity(CHUNK_ENTRIES));
        }
        let zero = 0.0f64.to_bits();
        self.chunks.last_mut().expect("chunk").push(Cell {
            key: [key as u32, (key >> 32) as u32],
            value: [zero as u32, (zero >> 32) as u32],
            next: self.heads[b],
        });
        self.heads[b] = index;
        self.len += 1;
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
    /// Doubles the head array and relinks every entry in place.
    fn grow(&mut self) {
        self.bits += 1;
        self.heads.clear();
        self.heads.shrink_to_fit();
        self.heads.resize(1 << self.bits, NIL);
        let bits = self.bits;
        let mut index = 0u32;
        for chunk in &mut self.chunks {
            for slot in chunk.iter_mut() {
                let b = Self::bucket(slot.key(), bits);
                slot.next = self.heads[b];
                self.heads[b] = index;
                index += 1;
            }
        }
    }
    /// Every `(key, value)`, releasing chunks as they are read.
    fn drain_into(self, out: &mut Vec<(u64, f64)>) {
        let Self { heads, chunks, .. } = self;
        drop(heads);
        for chunk in chunks {
            out.extend(chunk.iter().map(|s| (s.key(), s.value())));
        }
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

/// Accumulators owned by one shard: ground at the global nodes `g` with
/// `g % shards == s` (stored at `g / shards`), the coupling keys whose
/// selector picks `s`, and in shard 0 the overlap-area statistic.
pub(super) struct Shard {
    ground: Vec<f64>,
    couplings: Couplings,
    area: f64,
    keys: usize,
}
impl Shard {
    fn new(index: usize, shift: u32, nodes: usize) -> Result<Self> {
        let count = (nodes + (1 << shift) - 1 - index) >> shift;
        let mut ground = Vec::new();
        ground
            .try_reserve_exact(count)
            .map_err(|_| budget("extraction ground accumulator allocation"))?;
        ground.resize(count, 0.0);
        Ok(Self {
            ground,
            couplings: Couplings::new(),
            area: 0.0,
            keys: 0,
        })
    }
    /// `coupling[key] += pf` (created at 0.0); whether the key is new.
    #[inline]
    fn couple(&mut self, key: u64, pf: f64) -> bool {
        let (i, new) = self.couplings.find_or_insert(key);
        self.keys += usize::from(new);
        let v = self.couplings.get(i) + pf;
        self.couplings.set(i, v);
        new
    }
    /// Applies one shard's grouped records (consecutive records of one
    /// accumulator are in their run order). Returns the new coupling keys.
    fn apply_grouped(&mut self, ops: &[Op], shift: u32) -> usize {
        let mut fresh = 0;
        let mut at = 0;
        while at < ops.len() {
            let key = ops[at].key;
            if key < GROUND {
                let (i, new) = self.couplings.find_or_insert(key);
                self.keys += usize::from(new);
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
}

/// Every network accumulator of the strip phase, split into shards.
pub(super) struct Accumulators {
    shards: Vec<Shard>,
    shift: u32,
}
impl Accumulators {
    /// `shards` is rounded up to a power of two; `area` seeds the overlap
    /// statistic.
    pub(super) fn new(shards: usize, nodes: usize, area: f64) -> Result<Self> {
        let count = shards.clamp(1, MAX_SHARDS).next_power_of_two();
        let shift = count.trailing_zeros();
        let mut out = Self {
            shards: (0..count)
                .map(|s| Shard::new(s, shift, nodes))
                .collect::<Result<_>>()?,
            shift,
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
    /// Accumulated ground capacitance at global node `g`.
    pub(super) fn ground(&self, g: u32) -> f64 {
        let g = g as usize;
        self.shards[g & ((1 << self.shift) - 1)].ground[g >> self.shift]
    }
    /// Every coupling `(key, pF)` in ascending key order (keys are unique).
    /// Shards are drained and sorted in parallel, each map released as soon
    /// as it is moved out; the sorted shard lists are merged lazily.
    pub(super) fn into_sorted_couplings(self) -> Result<Sorted> {
        use rayon::prelude::*;
        let lists = self
            .shards
            .into_par_iter()
            .map(|shard| -> Result<Vec<(u64, f64)>> {
                let Shard {
                    ground,
                    couplings,
                    keys,
                    ..
                } = shard;
                drop(ground);
                let mut list = Vec::new();
                list.try_reserve_exact(keys)
                    .map_err(|_| budget("extraction coupling list allocation"))?;
                couplings.drain_into(&mut list);
                list.sort_unstable_by_key(|c| c.0);
                Ok(list)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Sorted {
            heads: lists
                .into_iter()
                .map(|l| l.into_iter().peekable())
                .collect(),
        })
    }
}

/// Ascending merge of per-shard sorted coupling lists.
pub(super) struct Sorted {
    heads: Vec<std::iter::Peekable<std::vec::IntoIter<(u64, f64)>>>,
}
impl Iterator for Sorted {
    type Item = (u64, f64);
    #[inline]
    fn next(&mut self) -> Option<(u64, f64)> {
        let mut best: Option<(usize, u64)> = None;
        for (i, h) in self.heads.iter_mut().enumerate() {
            if let Some(&(key, _)) = h.peek()
                && best.is_none_or(|(_, k)| key < k)
            {
                best = Some((i, key));
            }
        }
        self.heads[best?.0].next()
    }
}

/// The serial sink: applies each record immediately, exactly like the
/// original loop (budget check after every coupling insertion).
pub(super) struct Commit<'n> {
    shards: Vec<&'n mut Shard>,
    shift: u32,
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
        Self {
            shift: acc.shift,
            shards: acc.shards.iter_mut().collect(),
            stats,
            max_couplings,
            keys,
        }
    }
    pub(super) fn stats(&mut self) -> &mut ExtractStats {
        self.stats
    }
    #[inline]
    fn apply(&mut self, op: Op) -> Result<()> {
        let shard = &mut *self.shards[shard_of(op.key, self.shift)];
        if op.key == AREA {
            shard.area += op.pf;
        } else if op.key >= GROUND {
            shard.ground[(op.key as u32 as usize) >> self.shift] += op.pf;
        } else {
            let new = shard.couple(op.key, op.pf);
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
    units: Vec<UnitRef>,
    /// Ordered accumulations in run order. Emptied once a complete chunk
    /// is split into `buckets`.
    ops: Vec<Op>,
    /// A complete chunk's records per shard, stably grouped by accumulator.
    buckets: Vec<Vec<Op>>,
    /// Distinct coupling keys per bucket (an upper bound of new keys).
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
            for b in &mut self.buckets {
                b.clear();
            }
            for &op in &self.ops {
                self.buckets[shard_of(op.key, shift)].push(op);
            }
            self.ops.clear();
            for (bucket, distinct) in self.buckets.iter_mut().zip(&mut self.distinct) {
                // Stable: each accumulator's records keep their run order.
                bucket.sort_by_key(|op| op.key);
                *distinct = bucket
                    .iter()
                    .enumerate()
                    .filter(|&(i, op)| op.key < GROUND && (i == 0 || bucket[i - 1].key != op.key))
                    .count();
            }
        }
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
/// Accumulator shards for `lanes`: one for the serial path, otherwise the
/// lane count rounded up to a power of two.
pub(super) fn shard_count(lanes: usize, limits: &ExtractLimits) -> usize {
    if lanes <= 1 && limits.tile_units == 0 {
        1
    } else {
        lanes.clamp(1, MAX_SHARDS).next_power_of_two()
    }
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
    max_couplings: usize,
    instances: usize,
    shards: Vec<Mutex<Slot<'n>>>,
    /// Lock-free copies of each shard's turn.
    turns: Vec<AtomicU64>,
    /// Chunks below this are cleared (or committed at a barrier).
    cleared: AtomicU64,
    /// Over applied shard records: distinct keys minus new keys.
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
        let fresh = slot.acc.apply_grouped(&chunk.buckets[s], self.shift);
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
        }
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
    let shared = Shared {
        shift,
        max_couplings: limits.max_couplings,
        instances,
        shards: acc
            .shards
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
                        chunk.units.extend(src.units.by_ref().take(per_chunk));
                        if chunk.units.len() < per_chunk {
                            src.done = true;
                        }
                        if chunk.units.is_empty() {
                            return;
                        }
                        chunk.index = src.next;
                        src.next += 1;
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
    fn bits(acc: Accumulators, nodes: usize) -> (Vec<u64>, Vec<(u64, u64)>, u64) {
        let ground = (0..nodes as u32).map(|g| acc.ground(g).to_bits()).collect();
        let area = acc.area().to_bits();
        let couplings = acc
            .into_sorted_couplings()
            .unwrap()
            .map(|(k, v)| (k, v.to_bits()))
            .collect();
        (ground, couplings, area)
    }

    #[test]
    fn sharded_records_match_ordered_updates_bit_for_bit() {
        let nodes = 97usize;
        let ops = stream(30_000, nodes as u32);
        let want = reference(&ops, nodes);
        let mut stats = ExtractStats::default();
        // The serial sink, one record at a time, at every shard count.
        for shards in [1, 2, 4, 8] {
            let mut acc = Accumulators::new(shards, nodes, 0.0).unwrap();
            let mut commit = Commit::new(&mut acc, &mut stats, usize::MAX);
            for &op in &ops {
                commit.apply(op).unwrap();
            }
            assert_eq!(acc.keys(), want.1.len());
            assert_eq!(bits(acc, nodes), want);
        }
        // Chunks split by shard and grouped by accumulator, applied shard
        // by shard in chunk order (shards progress independently).
        for shift in 0..4u32 {
            let mut acc = Accumulators::new(1 << shift, nodes, 0.0).unwrap();
            let chunks: Vec<Vec<Vec<Op>>> = ops
                .chunks(777)
                .map(|chunk| {
                    let mut buckets = vec![Vec::new(); 1 << shift];
                    for &op in chunk {
                        buckets[shard_of(op.key, shift)].push(op);
                    }
                    for b in &mut buckets {
                        b.sort_by_key(|op| op.key);
                    }
                    buckets
                })
                .collect();
            for (s, shard) in acc.shards.iter_mut().enumerate().rev() {
                for chunk in &chunks {
                    shard.apply_grouped(&chunk[s], shift);
                }
            }
            assert_eq!(bits(acc, nodes), want);
        }
    }

    #[test]
    fn coupling_budget_fails_at_the_first_excess_key() {
        let nodes = 50usize;
        let ops = stream(5_000, nodes as u32);
        let keys = reference(&ops, nodes).1.len();
        for shards in [1, 4] {
            let mut stats = ExtractStats::default();
            let mut acc = Accumulators::new(shards, nodes, 0.0).unwrap();
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
    fn shard_counts_are_powers_of_two_and_serial_uses_one() {
        let serial = ExtractLimits::default();
        let forced = ExtractLimits {
            tile_units: 7,
            ..Default::default()
        };
        assert_eq!(shard_count(1, &serial), 1);
        assert_eq!(shard_count(1, &forced), 1);
        assert_eq!(shard_count(3, &serial), 4);
        assert_eq!(shard_count(4, &serial), 4);
        assert_eq!(shard_count(1000, &serial), MAX_SHARDS);
        for shift in 0..6u32 {
            for key in [0u64, 7, 1 << 40, GROUND | 5, GROUND | 0xFFFF_FFFE, AREA] {
                assert!(shard_of(key, shift) < 1 << shift);
            }
            assert_eq!(shard_of(AREA, shift), 0);
            assert_eq!(shard_of(GROUND | 13, shift), 13 & ((1 << shift) - 1));
        }
    }
}
