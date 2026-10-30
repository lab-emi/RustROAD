//! Bounded candidate-query tiles. All electrical arithmetic remains in the
//! caller's original run/interval order. No index or contribution is cloned.
use super::{
    Bins, ExtractInput, ExtractLimits, ExtractMemory, Run, Shape, Shapes, body_rect, budget,
    layer_model,
};
use crate::routing::InstanceQueryScratch;
use crate::{Rect, Result};
use rayon::prelude::*;
use std::collections::HashSet;

#[derive(Clone, Debug, serde::Serialize)]
pub struct QueryStats {
    pub requested_workers: usize,
    pub admitted_workers: usize,
    pub batch_runs: usize,
    /// Conservative admission, including worker stacks and allocation headroom.
    pub scratch_bytes: u64,
    /// Original admission before query scratch; query-bin selection is unchanged.
    pub baseline_estimate_bytes: u64,
    pub parallel_tiles: usize,
    pub serial_tiles: usize,
}
struct Scratch {
    seen: HashSet<u32>,
    instances: InstanceQueryScratch,
}
pub(super) struct Slot {
    pub run: Option<(usize, Run)>,
    pub candidates: Vec<u32>,
    pub work: usize,
    pub result: Option<Result<()>>,
}
pub(super) struct Batch {
    pub stats: QueryStats,
    pub slots: Vec<Slot>,
    scratch: Vec<Scratch>,
}

fn scratch_bytes(workers: usize, batch: usize, candidates: usize, instances: usize) -> u64 {
    // Hash-set buckets/control bytes plus occupancy headroom; exact-sized
    // marks; existing Rayon worker stacks; tiled IDs plus allocator headroom.
    let worker = (candidates as u64)
        .saturating_mul(64)
        .saturating_add((instances as u64).saturating_mul(8))
        .saturating_add(2 << 20);
    worker
        .saturating_mul(workers as u64)
        .saturating_add(
            (batch as u64)
                .saturating_mul(candidates as u64)
                .saturating_mul(8),
        )
        .saturating_add(4096)
}
impl Batch {
    pub fn new(
        input: &ExtractInput<'_>,
        limits: &ExtractLimits,
        memory: &ExtractMemory,
        indexed: bool,
    ) -> Result<Self> {
        let instances = input.instances.map_or(0, |i| i.geometry.len());
        let allowance = memory
            .limit_bytes
            .saturating_sub(memory.estimate_bytes)
            .min(limits.query_scratch_mib.saturating_mul(1 << 20));
        // Do not initialize Rayon's global pool for the default serial library
        // path. CLI opt-in runs inside its already-created --threads pool.
        let mut workers = if indexed && limits.query_workers > 1 && limits.query_batch_runs > 1 {
            limits
                .query_workers
                .min(rayon::current_num_threads())
                .min(limits.query_batch_runs)
        } else {
            1
        };
        while workers > 1
            && scratch_bytes(
                workers,
                limits.query_batch_runs,
                limits.max_candidates,
                instances,
            ) > allowance
        {
            workers -= 1;
        }
        let bytes = if workers > 1 {
            scratch_bytes(
                workers,
                limits.query_batch_runs,
                limits.max_candidates,
                instances,
            )
        } else {
            0
        };
        let count = if workers > 1 {
            limits.query_batch_runs
        } else {
            1
        };
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            let mut candidates = Vec::new();
            if workers > 1 {
                candidates
                    .try_reserve_exact(limits.max_candidates)
                    .map_err(|_| budget("extraction query tile allocation"))?;
            }
            slots.push(Slot {
                run: None,
                candidates,
                work: 0,
                result: None,
            });
        }
        let mut scratch = Vec::with_capacity(if workers > 1 { workers } else { 0 });
        if workers > 1 {
            for _ in 0..workers {
                let mut seen = HashSet::new();
                seen.try_reserve(limits.max_candidates)
                    .map_err(|_| budget("extraction query dedup allocation"))?;
                scratch.push(Scratch {
                    seen,
                    instances: InstanceQueryScratch::new(instances)?,
                });
            }
        }
        Ok(Self {
            stats: QueryStats {
                requested_workers: limits.query_workers,
                admitted_workers: workers,
                batch_runs: count,
                scratch_bytes: bytes,
                baseline_estimate_bytes: memory.estimate_bytes,
                parallel_tiles: 0,
                serial_tiles: 0,
            },
            slots,
            scratch,
        })
    }
    /// Returns (populated slots, parallel). Speculation is admitted only if a
    /// bound on *all* query and numerical work in this tile fits the original
    /// remaining max_work. Otherwise the exact original serial path is used.
    #[expect(
        clippy::too_many_arguments,
        reason = "tile preparation separates immutable inputs from bounded private state"
    )]
    pub fn prepare(
        &mut self,
        runs: &mut impl Iterator<Item = (usize, Run)>,
        input: &ExtractInput<'_>,
        shapes: &Shapes<'_>,
        bins: &Bins<'_>,
        layers: &[u32],
        limits: &ExtractLimits,
        work: usize,
    ) -> Result<(usize, bool)> {
        let mut count = 0;
        for slot in &mut self.slots {
            slot.run = runs.next();
            slot.result = None;
            slot.work = 0;
            slot.candidates.clear();
            if slot.run.is_none() {
                break;
            }
            count += 1;
        }
        if count == 0 {
            return Ok((0, false));
        }
        let upper = if self.stats.admitted_workers > 1 {
            self.slots[..count].iter().try_fold(0usize, |sum, slot| {
                tile_run_work(
                    slot.run.as_ref().unwrap().1,
                    input,
                    shapes,
                    bins,
                    layers,
                    limits,
                )
                .and_then(|n| sum.checked_add(n))
            })
        } else {
            None
        };
        let parallel = upper.is_some_and(|n| n <= limits.max_work.saturating_sub(work));
        if !parallel {
            self.stats.serial_tiles += 1;
            return Ok((count, false));
        }
        self.stats.parallel_tiles += 1;
        let chunk = count.div_ceil(self.stats.admitted_workers);
        self.slots[..count]
            .par_chunks_mut(chunk)
            .zip(self.scratch.par_iter_mut())
            .for_each(|(slots, scratch)| {
                for slot in slots {
                    scratch.seen.clear();
                    slot.result = Some(query(
                        slot.run.as_ref().unwrap().1,
                        input,
                        shapes,
                        bins,
                        layers,
                        limits,
                        scratch,
                        &mut slot.candidates,
                        &mut slot.work,
                    ));
                }
            });
        Ok((count, true))
    }
}

fn region(run: Run, input: &ExtractInput<'_>) -> Result<(Rect, i64, i64)> {
    let model = layer_model(input, run.layer)?;
    let window = model
        .contexts
        .iter()
        .map(|c| c.rows[c.rows.len() - 1][0])
        .fold(0.0f64, f64::max);
    let window = (window * input.tech.dbu_per_um).ceil() as i64;
    let (c0, c1) = (run.minus - window, run.plus + window);
    let query = if run.vertical {
        Rect {
            lx: c0,
            ly: run.lo,
            ux: c1,
            uy: run.hi,
        }
    } else {
        Rect {
            lx: run.lo,
            ly: c0,
            ux: run.hi,
            uy: c1,
        }
    };
    Ok((query, c0, c1))
}
fn tile_run_work(
    run: Run,
    input: &ExtractInput<'_>,
    shapes: &Shapes<'_>,
    bins: &Bins<'_>,
    layers: &[u32],
    limits: &ExtractLimits,
) -> Option<usize> {
    let (query, _, _) = region(run, input).ok()?;
    let index = bins.query_index.as_ref()?;
    let mut work = 0usize;
    for &layer in layers {
        let q = if layer == run.layer {
            query
        } else {
            body_rect(&run)
        };
        work = work.checked_add(index.query_work_bound())?;
        if let Some(instances) = &bins.instances {
            work = work.checked_add(instances.query_work_bound(shapes, layer, q)?)?;
        }
    }
    let n = limits.max_candidates.min(shapes.len());
    let events = n.checked_mul(2)?;
    let event_work = events.checked_mul(3 * events.max(1).ilog2() as usize + 4)?;
    work.checked_add(event_work)?
        .checked_add(events.checked_add(1)?.checked_mul(n.max(1))?)
}

#[expect(
    clippy::too_many_arguments,
    reason = "bounded query state and immutable input have separate ownership"
)]
fn query(
    run: Run,
    input: &ExtractInput<'_>,
    shapes: &Shapes<'_>,
    bins: &Bins<'_>,
    layers: &[u32],
    limits: &ExtractLimits,
    scratch: &mut Scratch,
    candidates: &mut Vec<u32>,
    work: &mut usize,
) -> Result<()> {
    let (query, c0, c1) = region(run, input)?;
    for &layer in layers {
        let q = if layer == run.layer {
            query
        } else {
            body_rect(&run)
        };
        let (x0, x1) = bins.range(q.lx, q.ux);
        let (y0, y1) = bins.range(q.ly, q.uy);
        if (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1)
            > limits.max_bin_entries as i128
        {
            return Err(budget("extraction query extent"));
        }
        let mut accept = |id, shape| {
            accept_candidate(
                run,
                layer,
                (c0, c1),
                id,
                shape,
                limits,
                &mut scratch.seen,
                candidates,
            )
        };
        bins.query_index
            .as_ref()
            .expect("parallel queries require immutable index")
            .visit(layer, q, shapes, limits, work, &mut accept)?;
        if let Some(instances) = &bins.instances {
            instances.visit_shared(
                &mut scratch.instances,
                shapes,
                layer,
                q,
                work,
                |id, shape| {
                    accept(id, shape)?;
                    Ok(true)
                },
            )?;
        }
    }
    Ok(())
}
#[expect(
    clippy::too_many_arguments,
    reason = "the same acceptance predicate serves serial and immutable parallel queries"
)]
pub(super) fn accept_candidate(
    run: Run,
    layer: u32,
    (c0, c1): (i64, i64),
    id: u32,
    shape: Shape,
    limits: &ExtractLimits,
    seen: &mut HashSet<u32>,
    candidates: &mut Vec<u32>,
) -> Result<()> {
    if id == run.shape || seen.contains(&id) {
        return Ok(());
    }
    let r = shape.rect;
    let (a0, a1, x0, x1) = if run.vertical {
        (r.ly, r.uy, r.lx, r.ux)
    } else {
        (r.lx, r.ux, r.ly, r.uy)
    };
    if a1 <= run.lo || a0 >= run.hi {
        return Ok(());
    }
    let relevant = if layer == run.layer {
        x1 > c0 && x0 < c1
    } else {
        x1 > run.minus && x0 < run.plus
    };
    if relevant {
        if candidates.len() >= limits.max_candidates {
            return Err(budget("extraction candidates per wire run"));
        }
        seen.insert(id);
        candidates.push(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scratch_arithmetic_is_conservative_and_saturating() {
        assert!(scratch_bytes(4, 8, 65536, 18000) < 32 << 20);
        assert!(scratch_bytes(5, 8, 65536, 18000) > 32 << 20);
        assert_eq!(
            scratch_bytes(usize::MAX, usize::MAX, usize::MAX, usize::MAX),
            u64::MAX
        );
    }
}
