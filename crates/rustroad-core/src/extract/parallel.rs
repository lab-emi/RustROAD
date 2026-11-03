//! Strip computation on the `--threads` pool with an in-order commit.
//!
//! Units (a wire body, or the x/y strip pair of one rectangle) are taken from
//! the lazy run iterator in numbered chunks. Lanes compute chunks
//! concurrently into bounded transcripts of ordered accumulations and hand
//! them to an ordered commit: whichever lane finds the next chunk number
//! ready applies it, then any later chunks already waiting. A commit applies
//! integer statistics, cumulative work, and every ground, coupling and
//! overlap-area addition exactly as the serial loop performs them, in run
//! order. Any unit that a lane did not finish (an error, the parallel
//! candidate cap or a full transcript) is computed by the serial path at its
//! place in run order, from the committed state, so budget errors carry the
//! serial path's exact code and message. Results, statistics and errors are
//! therefore identical at every thread count and chunk size.
use super::strip::{self, Counts, Ctx, Flow, Op, Scratch, Sink, Stop, UnitRef};
use super::{
    CANDIDATE_SCRATCH_BYTES, CouplingKey, ExtractLimits, ExtractStats, RcNet, budget, guard_memory,
    invalid,
};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

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
}
impl Default for Bounds {
    fn default() -> Self {
        Self {
            candidates: 16_384,
            chunk_ops: 1 << 16,
            pending_bytes: 4 << 20,
        }
    }
}

/// The network accumulators, mutated only in run order.
pub(super) struct Commit<'n> {
    nets: &'n mut [RcNet],
    coupling: &'n mut BTreeMap<CouplingKey, f64>,
    stats: &'n mut ExtractStats,
    max_couplings: usize,
}
impl<'n> Commit<'n> {
    pub(super) fn new(
        nets: &'n mut [RcNet],
        coupling: &'n mut BTreeMap<CouplingKey, f64>,
        stats: &'n mut ExtractStats,
        max_couplings: usize,
    ) -> Self {
        Self {
            nets,
            coupling,
            stats,
            max_couplings,
        }
    }
    pub(super) fn stats(&mut self) -> &mut ExtractStats {
        self.stats
    }
    #[inline]
    fn apply(&mut self, op: Op) -> Result<()> {
        match op {
            Op::Ground { net, node, pf } => self.nets[net as usize].ground[node as usize] += pf,
            Op::Couple { key, pf } => {
                *self.coupling.entry(key).or_insert(0.0) += pf;
                if self.coupling.len() > self.max_couplings {
                    return Err(budget("extraction coupling capacitors"));
                }
            }
            Op::Area(um2) => self.stats.overlap_area_um2 += um2,
        }
        Ok(())
    }
    /// Applies coupling additions grouped by key, each group in its original
    /// order: one map search per distinct key, identical f64 sequences.
    fn apply_grouped(&mut self, couples: &[(CouplingKey, f64)]) -> Result<()> {
        let mut at = 0;
        while at < couples.len() {
            let key = couples[at].0;
            let value = self.coupling.entry(key).or_insert(0.0);
            while at < couples.len() && couples[at].0 == key {
                *value += couples[at].1;
                at += 1;
            }
            if self.coupling.len() > self.max_couplings {
                return Err(budget("extraction coupling capacitors"));
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
    /// Ordered accumulations. In a complete chunk only the ground and area
    /// additions remain here; its coupling additions are in `couples`.
    ops: Vec<Op>,
    /// A complete chunk's coupling additions, stably sorted by key: each
    /// key's additions keep their run order, and keys are independent.
    couples: Vec<(CouplingKey, f64)>,
    /// Per computed unit: its record, or `None` when it was deferred to the
    /// serial path. A unit that failed ends the list (the serial path
    /// reproduces its error at commit).
    metas: Vec<Option<Meta>>,
    complete: bool,
}
impl Chunk {
    fn bytes(&self) -> usize {
        self.ops.capacity() * size_of::<Op>()
            + self.couples.capacity() * size_of::<(CouplingKey, f64)>()
            + self.metas.capacity() * size_of::<Option<Meta>>()
            + self.units.capacity() * size_of::<UnitRef>()
    }
    /// Computes units in order. A unit beyond a parallel bound is deferred;
    /// an error ends the chunk.
    fn compute(&mut self, ctx: &Ctx<'_, '_>, bounds: Bounds, sc: &mut Scratch) {
        self.ops.clear();
        self.couples.clear();
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
            let couples = &mut self.couples;
            self.ops.retain(|op| match *op {
                Op::Couple { key, pf } => {
                    couples.push((key, pf));
                    false
                }
                _ => true,
            });
            couples.sort_by_key(|c| c.0);
        }
    }
}

/// Upper bound of one chunk's transcript (with vector growth, plus the
/// sorted coupling copy) and metadata.
fn chunk_bytes(bounds: Bounds, units: usize) -> u64 {
    (bounds.chunk_ops as u64)
        .saturating_mul((2 * size_of::<Op>() + 2 * size_of::<(CouplingKey, f64)>()) as u64)
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

/// Phase timings and unit counts for the private profile.
#[derive(Default)]
pub(super) struct Report {
    pub lanes: usize,
    pub chunks: u64,
    pub units: usize,
    pub serial_units: usize,
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
    chunks: BTreeMap<u64, Chunk>,
    bytes: usize,
    spare: Vec<Chunk>,
}
struct Accum<'n> {
    commit: Commit<'n>,
    turn: u64,
    serial: Option<Scratch>,
    error: Option<Error>,
    report: Report,
    instances: usize,
}
impl Accum<'_> {
    /// Applies one chunk in run order; units it lacks run serially here.
    fn apply(&mut self, ctx: &Ctx<'_, '_>, chunk: &Chunk) -> Result<()> {
        let limits = ctx.limits;
        if chunk.complete {
            // Grouped coupling additions are exact when no budget can fail
            // inside the chunk; otherwise the serial path recomputes every
            // unit, failing at the exact serial point.
            let work = chunk
                .metas
                .iter()
                .flatten()
                .try_fold(self.commit.stats().work, |w, m| w.checked_add(m.work))
                .filter(|&w| w <= limits.max_work);
            let fits = self
                .commit
                .coupling
                .len()
                .checked_add(chunk.couples.len())
                .is_some_and(|n| n <= self.commit.max_couplings);
            let before = self.report.units;
            self.report.units += chunk.units.len();
            if before / 4096 != self.report.units / 4096 || before == 0 {
                guard_memory(limits)?;
            }
            if let (Some(work), true) = (work, fits) {
                for &op in &chunk.ops {
                    self.commit.apply(op)?;
                }
                self.commit.apply_grouped(&chunk.couples)?;
                self.commit.stats().work = work;
                for meta in chunk.metas.iter().flatten() {
                    meta.counts.add_to(self.commit.stats());
                }
                self.report.ops += chunk.ops.len() + chunk.couples.len();
                return Ok(());
            }
            for &u in &chunk.units {
                self.report.serial_units += 1;
                let sc = match &mut self.serial {
                    Some(sc) => sc,
                    None => self.serial.insert(Scratch::new(self.instances)?),
                };
                serial_unit(ctx, u, sc, &mut self.commit)?;
            }
            return Ok(());
        }
        let mut from = 0;
        for (j, &u) in chunk.units.iter().enumerate() {
            if self.report.units.is_multiple_of(4096) {
                guard_memory(limits)?;
            }
            self.report.units += 1;
            let Some(meta) = chunk.metas.get(j).copied().flatten() else {
                // Not finished by its lane: the serial path reproduces an
                // error exactly, or completes a deferred unit.
                self.report.serial_units += 1;
                let sc = match &mut self.serial {
                    Some(sc) => sc,
                    None => self.serial.insert(Scratch::new(self.instances)?),
                };
                serial_unit(ctx, u, sc, &mut self.commit)?;
                continue;
            };
            let total = self
                .commit
                .stats
                .work
                .checked_add(meta.work)
                .filter(|&n| n <= limits.max_work);
            let Some(total) = total else {
                // Cumulative work overflows inside this unit: replay it
                // serially from the committed work to fail at the exact
                // serial charge.
                let sc = match &mut self.serial {
                    Some(sc) => sc,
                    None => self.serial.insert(Scratch::new(self.instances)?),
                };
                serial_unit(ctx, u, sc, &mut self.commit)?;
                return Err(invalid("strip work overflow did not reproduce serially"));
            };
            self.commit.stats().work = total;
            for &op in &chunk.ops[from..meta.ops_end] {
                self.commit.apply(op)?;
            }
            self.report.ops += meta.ops_end - from;
            from = meta.ops_end;
            meta.counts.add_to(self.commit.stats());
        }
        Ok(())
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Computes and commits every unit. `lanes <= 1` without a forced chunk
/// size runs the serial path directly.
pub(super) fn run(
    ctx: &Ctx<'_, '_>,
    units: &mut (impl Iterator<Item = UnitRef> + Send),
    bounds: Bounds,
    lanes: usize,
    instances: usize,
    commit: Commit<'_>,
) -> Result<Report> {
    let limits = ctx.limits;
    if lanes <= 1 && limits.tile_units == 0 {
        let mut commit = commit;
        let mut report = Report {
            lanes: 1,
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
    let accum = Mutex::new(Accum {
        commit,
        turn: 0,
        serial: None,
        error: None,
        report: Report {
            lanes,
            ..Default::default()
        },
        instances,
    });
    let turn = AtomicU64::new(0);
    let failed = AtomicBool::new(false);
    let compute_ns = AtomicUsize::new(0);
    let commit_ns = AtomicUsize::new(0);
    // Commit every ready chunk in order. Whoever holds the accumulators
    // re-checks after releasing them, so a chunk deposited meanwhile (whose
    // depositor found them busy) is never left behind.
    let commit_ready = || {
        loop {
            let Ok(mut acc) = accum.try_lock() else {
                return;
            };
            let started = std::time::Instant::now();
            while acc.error.is_none() {
                let chunk = {
                    let mut p = lock(&pending);
                    let chunk = p.chunks.remove(&acc.turn);
                    if let Some(c) = &chunk {
                        p.bytes -= c.bytes();
                    }
                    chunk
                };
                let Some(chunk) = chunk else {
                    break;
                };
                space.notify_all();
                if let Err(e) = acc.apply(ctx, &chunk) {
                    acc.error = Some(e);
                    failed.store(true, Ordering::Release);
                    space.notify_all();
                }
                acc.turn += 1;
                turn.store(acc.turn, Ordering::Release);
                let mut p = lock(&pending);
                if p.spare.len() < lanes && chunk.bytes() <= SPARE_BYTES {
                    p.spare.push(chunk);
                }
            }
            commit_ns.fetch_add(started.elapsed().as_nanos() as usize, Ordering::Relaxed);
            drop(acc);
            let t = turn.load(Ordering::Acquire);
            if failed.load(Ordering::Acquire) || !lock(&pending).chunks.contains_key(&t) {
                return;
            }
        }
    };
    rayon::scope(|s| {
        for sc in scratch.iter_mut() {
            let (source, pending, space, failed) = (&source, &pending, &space, &failed);
            let (commit_ready, compute_ns) = (&commit_ready, &compute_ns);
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
                    let mut chunk = {
                        let mut p = lock(pending);
                        while p.bytes > bounds.pending_bytes && !failed.load(Ordering::Acquire) {
                            p = space.wait(p).unwrap_or_else(|e| e.into_inner());
                        }
                        p.spare.pop().unwrap_or_default()
                    };
                    if failed.load(Ordering::Acquire) {
                        return;
                    }
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
                    chunk.compute(ctx, bounds, sc);
                    compute_ns.fetch_add(started.elapsed().as_nanos() as usize, Ordering::Relaxed);
                    {
                        let mut p = lock(pending);
                        p.bytes += chunk.bytes();
                        p.chunks.insert(chunk.index, chunk);
                    }
                    commit_ready();
                }
            });
        }
    });
    let chunks = source.into_inner().unwrap_or_else(|e| e.into_inner()).next;
    let acc = accum.into_inner().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = acc.error {
        return Err(e);
    }
    if acc.turn != chunks {
        return Err(invalid("extraction chunks left uncommitted"));
    }
    let mut report = acc.report;
    report.chunks = chunks;
    report.compute_ns = compute_ns.into_inner() as u128;
    report.commit_ns = commit_ns.into_inner() as u128;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::RcNet;

    #[test]
    fn grouped_coupling_additions_match_ordered_updates_bit_for_bit() {
        // A skewed key stream (hot and cold keys) with non-associative
        // values: grouping by key with a stable sort keeps every key's
        // addition order, so every accumulator ends with identical bits.
        let mut seed = 11u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        let ops: Vec<(CouplingKey, f64)> = (0..20_000)
            .map(|i| {
                let r = next();
                let k = if i % 3 == 0 { r % 7 } else { r % 400 } as u32;
                (
                    ((k, k % 5), (k + 1, 3)),
                    (r % 1000) as f64 * 1.000_000_1e-3 + 1e-17,
                )
            })
            .collect();
        let mut direct = BTreeMap::new();
        for &(key, pf) in &ops {
            *direct.entry(key).or_insert(0.0) += pf;
        }
        // Applied in chunks, like committed transcripts.
        let mut grouped = BTreeMap::new();
        let (mut nets, mut stats): (Vec<RcNet>, _) = (Vec::new(), ExtractStats::default());
        let mut commit = Commit::new(&mut nets, &mut grouped, &mut stats, usize::MAX);
        for chunk in ops.chunks(777) {
            let mut sorted = chunk.to_vec();
            sorted.sort_by_key(|c| c.0);
            commit.apply_grouped(&sorted).unwrap();
        }
        let bits = |m: &BTreeMap<CouplingKey, f64>| {
            m.iter().map(|(k, v)| (*k, v.to_bits())).collect::<Vec<_>>()
        };
        assert_eq!(bits(&direct), bits(&grouped));
        let summed: f64 = ops.iter().map(|o| o.1).sum();
        assert!(direct.values().sum::<f64>() > 0.99 * summed);
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
}
