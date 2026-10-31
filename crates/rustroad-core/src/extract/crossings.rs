//! Bounded private activation bits; all electrical additions remain serial.
use super::{Bins, ExtractLimits, ExtractMemory, ExtractStats, Owner, Shape, Shapes, budget};
use crate::routing::InstanceQueryScratch;
use crate::{Error, Rect, Result};
use rayon::prelude::*;

const TILE: usize = 64;
const SLOT_BYTES: usize = 128;
const ERROR_BYTES: usize = 256;
const HEADROOM: u64 = 4096;
const STACK_BYTES: u64 = 2 << 20;

/// Opt-in crossing admission and the additional whole-extraction work ledger.
///
/// `control_work` counts the documented initialization/dispatch/result/merge
/// units, not CPU instructions. Bounded admission probes are reported separately.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CrossingStats {
    pub requested_workers: usize,
    pub admitted_workers: usize,
    pub tile_queries: usize,
    pub scratch_bytes: u64,
    /// Complete conservative control reservation, not charged/consumed work.
    pub reserved_control_upper: usize,
    /// Extra initialization, dispatched queries, ordered results, tiles and ORs.
    pub control_work: usize,
    /// Original complete-extraction logical work; updated by the caller at end.
    pub legacy_work: usize,
    pub total_work: usize,
    pub original_max_work: usize,
    pub parallel_tiles: usize,
    /// None when the exact original serial implementation was used uninstrumented.
    pub serial_queries: Option<usize>,
    /// Bounded admission probes; excluded from the charged control-unit scope.
    pub bound_probes: usize,
    pub preflight_owned_shapes: usize,
    pub fallback: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct Query {
    layer: u32,
    rect: Rect,
}
struct Slot {
    query: Option<Query>,
    work: usize,
    result: Option<Result<()>>,
}
const _: () = assert!(size_of::<Slot>() <= SLOT_BYTES);

struct Worker {
    instances: InstanceQueryScratch,
    marked: Vec<u64>,
}
struct Plan {
    workers: usize,
    instances: usize,
    words: usize,
    allowance: u64,
    bytes: u64,
    init_work: usize,
    merge_work: usize,
    control_upper: usize,
}

fn queries<'a>(shapes: &'a Shapes<'_>, layers: &'a [u32]) -> impl Iterator<Item = Query> + 'a {
    shapes
        .owned()
        .filter_map(|(_, shape)| matches!(shape.owner, Owner::Run(_)).then_some(shape))
        .flat_map(move |shape| {
            layers
                .iter()
                .filter(move |&&layer| layer != shape.layer)
                .map(move |&layer| Query {
                    layer,
                    rect: shape.rect,
                })
        })
}

fn estimate(workers: usize, instances: usize, words: usize) -> u64 {
    (workers as u64)
        .saturating_mul(
            (instances as u64)
                .saturating_mul(8)
                .saturating_add(STACK_BYTES),
        )
        .saturating_add(
            (workers.saturating_sub(1) as u64)
                .saturating_mul(words as u64)
                .saturating_mul(8),
        )
        .saturating_add((TILE * (SLOT_BYTES + ERROR_BYTES)) as u64)
        .saturating_add(HEADROOM)
}

fn plan(
    shapes: &Shapes<'_>,
    layers: &[u32],
    limits: &ExtractLimits,
    memory: &ExtractMemory,
    legacy_work: usize,
    threads: usize,
) -> Option<Plan> {
    let instances = shapes.source?.geometry.len();
    let words = shapes.virtual_count().div_ceil(64);
    let query_upper = shapes.retained_len().checked_mul(layers.len())?;
    let allowance = memory
        .limit_bytes
        .saturating_sub(memory.estimate_bytes)
        .min(limits.crossing_scratch_mib.saturating_mul(1 << 20));
    let mut workers = limits.crossing_workers.min(threads).min(16);
    while workers > 1 {
        let extras = workers.checked_sub(1)?.checked_mul(words)?;
        let init_work = workers
            .checked_mul(instances)?
            .checked_add(extras)?
            .checked_add(workers)?
            .checked_add(TILE)?;
        let merge_work = extras;
        let control_upper = init_work
            .checked_add(merge_work)?
            .checked_add(query_upper.checked_mul(2)?)?
            .checked_add(query_upper.div_ceil(TILE))?;
        let bytes = estimate(workers, instances, words);
        if bytes <= allowance && control_upper <= limits.max_work.saturating_sub(legacy_work) {
            return Some(Plan {
                workers,
                instances,
                words,
                allowance,
                bytes,
                init_work,
                merge_work,
                control_upper,
            });
        }
        workers -= 1;
    }
    None
}

fn base_stats(limits: &ExtractLimits, legacy_work: usize) -> CrossingStats {
    CrossingStats {
        requested_workers: limits.crossing_workers,
        admitted_workers: 1,
        tile_queries: TILE,
        scratch_bytes: 0,
        reserved_control_upper: 0,
        control_work: 0,
        legacy_work,
        total_work: legacy_work,
        original_max_work: limits.max_work,
        parallel_tiles: 0,
        serial_queries: None,
        bound_probes: 0,
        preflight_owned_shapes: 0,
        fallback: None,
    }
}

fn serial(
    shapes: &Shapes<'_>,
    bins: &mut Bins<'_>,
    layers: &[u32],
    stats: &mut ExtractStats,
    mut receipt: CrossingStats,
    reason: &'static str,
) -> Result<(Vec<u64>, Option<CrossingStats>)> {
    receipt.fallback = Some(reason);
    let marked = super::virtual_crossings(shapes, bins, layers, stats)?;
    receipt.legacy_work = stats.work;
    receipt.total_work = stats.work;
    Ok((marked, Some(receipt)))
}

fn mark(marked: &mut [u64], shapes: &Shapes<'_>, rect: Rect, id: u32, other: Shape) -> Result<()> {
    let b = other.rect;
    if rect.lx < b.ux && b.lx < rect.ux && rect.ly < b.uy && b.ly < rect.uy {
        let k = (id as usize)
            .checked_sub(shapes.virtual_offset())
            .filter(|&k| k < shapes.virtual_count())
            .ok_or_else(|| budget("virtual crossing shape id"))?;
        let word = marked
            .get_mut(k / 64)
            .ok_or_else(|| budget("virtual crossing bit extent"))?;
        *word |= 1 << (k % 64);
    }
    Ok(())
}

fn bounded_result(result: Result<()>) -> Result<()> {
    result.map_err(|error| {
        let text = match &error {
            Error::Invalid(text) | Error::Budget(text) | Error::Capacity(text) => Some(text),
            Error::PinAccess { .. } => None,
        };
        if text.is_some_and(|text| text.capacity() > ERROR_BYTES) {
            budget("virtual crossing error payload")
        } else {
            error
        }
    })
}

fn zero_bits(words: usize) -> Result<Vec<u64>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(words)
        .map_err(|_| budget("crossing bit allocation"))?;
    values.resize(words, 0);
    Ok(values)
}

fn allocate(plan: &Plan) -> Result<(Vec<Worker>, Vec<Slot>, u64)> {
    let mut workers = Vec::new();
    workers
        .try_reserve_exact(plan.workers)
        .map_err(|_| budget("crossing worker allocation"))?;
    for _ in 0..plan.workers {
        workers.push(Worker {
            instances: InstanceQueryScratch::new(plan.instances)?,
            marked: zero_bits(plan.words)?,
        });
    }
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(TILE)
        .map_err(|_| budget("crossing tile allocation"))?;
    slots.resize_with(TILE, || Slot {
        query: None,
        work: 0,
        result: None,
    });
    let headers = workers
        .capacity()
        .saturating_mul(size_of::<Worker>())
        .saturating_add(size_of::<Vec<Worker>>())
        .saturating_add(size_of::<Vec<Slot>>())
        .saturating_add(size_of::<CrossingStats>())
        .saturating_add(size_of::<Plan>());
    if headers as u64 > HEADROOM {
        return Err(budget("crossing worker header capacity"));
    }
    let marks = workers
        .iter()
        .try_fold(0u64, |sum, worker| {
            sum.checked_add(worker.instances.allocation_bytes())
        })
        .ok_or_else(|| budget("crossing mark capacity"))?;
    let bits = workers
        .iter()
        .try_fold(0u64, |sum, worker| {
            sum.checked_add((worker.marked.capacity() as u64).checked_mul(8)?)
        })
        .and_then(|bytes| bytes.checked_sub((plan.words as u64).checked_mul(8)?))
        .ok_or_else(|| budget("crossing bit capacity"))?;
    let actual = marks
        .checked_add(bits)
        .and_then(|n| {
            n.checked_add((slots.capacity() as u64).checked_mul(size_of::<Slot>() as u64)?)
        })
        .and_then(|n| n.checked_add((TILE * ERROR_BYTES) as u64))
        .and_then(|n| n.checked_add((plan.workers as u64).checked_mul(STACK_BYTES)?))
        .and_then(|n| n.checked_add(HEADROOM))
        .ok_or_else(|| budget("crossing scratch capacity"))?;
    let bytes = actual.max(plan.bytes);
    if bytes > plan.allowance {
        return Err(budget("crossing actual scratch admission"));
    }
    Ok((workers, slots, bytes))
}

fn consume_results(
    slots: &mut [Slot],
    stats: &mut ExtractStats,
    receipt: &mut CrossingStats,
    legacy_cap: usize,
    control_upper: usize,
) -> Result<()> {
    // Admission covered every dispatched query, including slots after an
    // error. Consume only the original logical prefix to preserve its first
    // diagnostic; no successful receipt is returned on that error path.
    for slot in slots {
        receipt.control_work = receipt
            .control_work
            .checked_add(1)
            .filter(|&n| n <= control_upper)
            .ok_or_else(|| budget("crossing result control work"))?;
        stats.work = stats
            .work
            .checked_add(slot.work)
            .filter(|&n| n <= legacy_cap)
            .ok_or_else(|| budget("crossing ordered query work"))?;
        slot.result.take().expect("completed crossing query")?;
    }
    Ok(())
}

/// Private worker state is destroyed before returning the one original bitset.
pub(super) fn run(
    shapes: &Shapes<'_>,
    bins: &mut Bins<'_>,
    layers: &[u32],
    limits: &ExtractLimits,
    memory: &ExtractMemory,
    stats: &mut ExtractStats,
) -> Result<(Vec<u64>, Option<CrossingStats>)> {
    if limits.crossing_workers == 1 {
        return super::virtual_crossings(shapes, bins, layers, stats).map(|marked| (marked, None));
    }
    let mut receipt = base_stats(limits, stats.work);
    if shapes.virtual_count() == 0 || bins.instances.is_none() || layers.len() < 2 {
        return serial(shapes, bins, layers, stats, receipt, "no_virtual_queries");
    }
    // Do not initialize a global Rayon pool from a library admission probe.
    if rayon::current_thread_index().is_none() {
        return serial(shapes, bins, layers, stats, receipt, "no_existing_pool");
    }
    let Some(plan) = plan(
        shapes,
        layers,
        limits,
        memory,
        stats.work,
        rayon::current_num_threads(),
    ) else {
        return serial(
            shapes,
            bins,
            layers,
            stats,
            receipt,
            "memory_or_control_admission",
        );
    };
    receipt.reserved_control_upper = plan.control_upper;
    // Only a fixed tile is probed before any worker initialization or geometry.
    let instances = bins
        .instances
        .as_ref()
        .expect("validated virtual query index");
    let mut first_count = 0usize;
    let mut first_bound = Some(0usize);
    'probe: for (_, shape) in shapes.owned().take(TILE) {
        receipt.preflight_owned_shapes += 1;
        if !matches!(shape.owner, Owner::Run(_)) {
            continue;
        }
        for &layer in layers.iter().filter(|&&layer| layer != shape.layer) {
            first_count += 1;
            first_bound = first_bound.and_then(|sum| {
                instances
                    .query_work_bound(shapes, layer, shape.rect)
                    .and_then(|n| sum.checked_add(n))
            });
            if first_bound.is_none() || first_count == TILE {
                break 'probe;
            }
        }
    }
    receipt.bound_probes = first_count;
    if first_count < TILE && receipt.preflight_owned_shapes < shapes.retained_len() {
        return serial(shapes, bins, layers, stats, receipt, "sparse_query_prefix");
    }
    if first_count == 0 {
        return serial(shapes, bins, layers, stats, receipt, "no_routed_queries");
    }
    if !first_bound.is_some_and(|bound| {
        stats
            .work
            .checked_add(plan.control_upper)
            .and_then(|n| n.checked_add(bound))
            .is_some_and(|n| n <= limits.max_work)
    }) {
        return serial(
            shapes,
            bins,
            layers,
            stats,
            receipt,
            "first_tile_work_admission",
        );
    }

    // The whole control upper and mandatory merge were reserved before these
    // fallible allocations. Allocation failure is a structured error, no retry.
    let (mut workers, mut slots, bytes) = allocate(&plan)?;
    receipt.admitted_workers = plan.workers;
    receipt.scratch_bytes = bytes;
    receipt.control_work = plan.init_work;
    receipt.serial_queries = Some(0);
    let mut remaining = queries(shapes, layers);
    loop {
        let mut count = 0usize;
        for slot in &mut slots {
            slot.query = remaining.next();
            slot.work = 0;
            slot.result = None;
            if slot.query.is_none() {
                break;
            }
            count += 1;
        }
        if count == 0 {
            break;
        }
        let instances = bins
            .instances
            .as_ref()
            .expect("validated virtual query index");
        let upper = slots[..count].iter().try_fold(0usize, |sum, slot| {
            receipt.bound_probes = receipt.bound_probes.checked_add(1)?;
            let query = slot.query.expect("populated crossing query");
            instances
                .query_work_bound(shapes, query.layer, query.rect)
                .and_then(|n| sum.checked_add(n))
        });
        let parallel = upper.is_some_and(|bound| {
            stats
                .work
                .checked_add(plan.control_upper)
                .and_then(|n| n.checked_add(bound))
                .is_some_and(|n| n <= limits.max_work)
        });
        if !parallel {
            receipt.fallback = Some("remaining_tile_work_admission");
            let serial_cap = limits
                .max_work
                .checked_sub(receipt.control_work)
                .and_then(|n| n.checked_sub(plan.merge_work))
                .ok_or_else(|| budget("crossing serial tail reservation"))?;
            let instances = bins
                .instances
                .as_mut()
                .expect("validated virtual query index");
            instances.restrict_work(serial_cap);
            let prepared = slots[..count]
                .iter()
                .map(|slot| slot.query.expect("populated crossing query"));
            for query in prepared.chain(remaining) {
                let marked = &mut workers[0].marked;
                instances.visit(
                    shapes,
                    query.layer,
                    query.rect,
                    &mut stats.work,
                    |id, other| {
                        mark(marked, shapes, query.rect, id, other)?;
                        Ok(true)
                    },
                )?;
                receipt.serial_queries = Some(
                    receipt
                        .serial_queries
                        .expect("serial crossing tail measured")
                        .checked_add(1)
                        .ok_or_else(|| budget("crossing serial query count"))?,
                );
            }
            break;
        }
        let dispatch = count
            .checked_add(1)
            .ok_or_else(|| budget("crossing dispatch work"))?;
        receipt.control_work = receipt
            .control_work
            .checked_add(dispatch)
            .filter(|&n| n <= plan.control_upper)
            .ok_or_else(|| budget("crossing control reservation"))?;
        receipt.parallel_tiles = receipt
            .parallel_tiles
            .checked_add(1)
            .ok_or_else(|| budget("crossing tile count"))?;
        let chunk = count.div_ceil(plan.workers);
        slots[..count]
            .par_chunks_mut(chunk)
            .zip(workers.par_iter_mut())
            .for_each(|(slots, worker)| {
                for slot in slots {
                    let query = slot.query.expect("populated crossing query");
                    let marked = &mut worker.marked;
                    slot.result = Some(bounded_result(
                        instances
                            .visit_shared(
                                &mut worker.instances,
                                shapes,
                                query.layer,
                                query.rect,
                                &mut slot.work,
                                |id, other| {
                                    mark(marked, shapes, query.rect, id, other)?;
                                    Ok(true)
                                },
                            )
                            .map(|_| ()),
                    ));
                }
            });
        consume_results(
            &mut slots[..count],
            stats,
            &mut receipt,
            limits.max_work.saturating_sub(plan.control_upper),
            plan.control_upper,
        )?;
    }
    receipt.control_work = receipt
        .control_work
        .checked_add(plan.merge_work)
        .filter(|&n| n <= plan.control_upper)
        .ok_or_else(|| budget("crossing merge work"))?;
    receipt.total_work = stats
        .work
        .checked_add(receipt.control_work)
        .filter(|&n| n <= limits.max_work)
        .ok_or_else(|| budget("crossing aggregate work"))?;
    receipt.legacy_work = stats.work;
    let (first, others) = workers
        .split_first_mut()
        .expect("admitted crossing workers");
    for worker in others {
        for (out, word) in first.marked.iter_mut().zip(&worker.marked) {
            *out |= *word;
        }
    }
    let marked = std::mem::take(&mut first.marked);
    stats.passive_strips = stats
        .passive_strips
        .checked_add(
            marked
                .iter()
                .map(|word| word.count_ones() as usize)
                .sum::<usize>()
                .saturating_mul(2),
        )
        .ok_or_else(|| budget("virtual passive strips"))?;
    Ok((marked, Some(receipt)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Orientation;
    use crate::extract::{InstanceGeometry, InstancedPassive};
    use crate::instance_obstacles::{
        InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
    };

    /// <=192 retained and140 explicit virtual rectangles; no large fixture.
    struct Fixture {
        source: InstanceObstacles,
        retained: Vec<Shape>,
        explicit: Vec<(u32, Rect)>,
    }
    impl Fixture {
        fn new(retained: usize) -> Self {
            assert!(retained <= 192, "tiny retained fixture cap");
            let mut template = Vec::new();
            for i in 0..70i64 {
                template.push(TemplateObstacle {
                    layer: 1 + (i % 3) as u32,
                    rect: Rect {
                        lx: i * 20,
                        ly: 0,
                        ux: i * 20 + 10,
                        uy: 10,
                    },
                    pin: (i % 3 == 0).then_some(0),
                });
            }
            let explicit = template
                .iter()
                .map(|s| (s.layer, s.rect))
                .chain(template.iter().map(|s| {
                    (
                        s.layer,
                        Rect {
                            lx: s.rect.lx,
                            ux: s.rect.ux,
                            ly: 600 - s.rect.uy,
                            uy: 600 - s.rect.ly,
                        },
                    )
                }))
                .collect();
            let source = InstanceObstacles::new(
                vec![ObstacleTemplate::new(200, 1, template).expect("tiny obstacle template")],
                vec![
                    ObstacleInstance {
                        template: 0,
                        pin_start: 0,
                        at: [0, 0],
                        orientation: Orientation::N,
                    },
                    ObstacleInstance {
                        template: 0,
                        pin_start: 1,
                        at: [0, 400],
                        orientation: Orientation::FS,
                    },
                ],
                vec![0, 0],
            )
            .expect("tiny instances")
            .with_pin_ids(vec![0, 1])
            .expect("tiny pin ids");
            let retained = (0..retained)
                .map(|i| Shape {
                    layer: 0,
                    rect: Rect {
                        lx: -1,
                        ux: 1500,
                        ly: if i % 2 == 0 { -1 } else { 589 },
                        uy: if i % 2 == 0 { 11 } else { 601 },
                    },
                    owner: Owner::Run(i as u32),
                })
                .collect();
            Self {
                source,
                retained,
                explicit,
            }
        }
        fn expected(&self) -> Vec<u64> {
            let mut marked = vec![0u64; self.explicit.len().div_ceil(64)];
            assert!(
                self.retained.len() * self.explicit.len() <= 30_000,
                "tiny brute-force pair cap"
            );
            for (i, &(layer, b)) in self.explicit.iter().enumerate() {
                for a in &self.retained {
                    if matches!(a.owner, Owner::Run(_))
                        && a.layer != layer
                        && a.rect.lx < b.ux
                        && b.lx < a.rect.ux
                        && a.rect.ly < b.uy
                        && b.ly < a.rect.uy
                    {
                        marked[i / 64] |= 1 << (i % 64);
                    }
                }
            }
            marked
        }
        fn run(
            &self,
            limits: &ExtractLimits,
            available: u64,
        ) -> Result<(Vec<u64>, Option<CrossingStats>, ExtractStats)> {
            let shapes = Shapes::new(
                self.retained.clone(),
                self.retained.len(),
                Some(InstancedPassive {
                    geometry: &self.source,
                    signal_nets: &[true],
                }),
                limits,
            )?;
            let mut bins = Bins {
                size: 100,
                map: Default::default(),
                query_index: None,
                instances: InstanceGeometry::new(&shapes, 100, 4, limits)?,
            };
            let memory = ExtractMemory {
                held_bytes: 0,
                working_bytes: 0,
                geometry_bytes: 0,
                estimate_bytes: 0,
                limit_bytes: available,
                instance_query_bin_dbu: 100,
            };
            let mut stats = ExtractStats::default();
            let (bits, receipt) = super::run(
                &shapes,
                &mut bins,
                &[0, 1, 2, 3],
                limits,
                &memory,
                &mut stats,
            )?;
            Ok((bits, receipt, stats))
        }
    }

    #[test]
    fn private_bits_match_explicit_world_geometry_across_words_transforms_and_dense_hits() {
        let fixture = Fixture::new(128);
        let serial = fixture
            .run(&ExtractLimits::default(), 64 << 20)
            .expect("serial tiny crossings");
        assert_eq!(serial.0, fixture.expected());
        assert!(serial.1.is_none());
        assert_ne!(serial.0[0] & (1 << 63), 0);
        assert_ne!(serial.0[1] & 1, 0);
        for workers in [2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .expect("tiny pool");
            let limits = ExtractLimits {
                crossing_workers: workers,
                ..Default::default()
            };
            let (bits, receipt, stats) = pool
                .install(|| fixture.run(&limits, 64 << 20))
                .expect("parallel tiny crossings");
            assert_eq!(bits, fixture.expected());
            assert_eq!(stats.work, serial.2.work);
            assert_eq!(stats.passive_strips, serial.2.passive_strips);
            let receipt = receipt.expect("opt-in telemetry");
            assert_eq!(receipt.admitted_workers, workers);
            assert!(receipt.parallel_tiles > 0);
            assert!(receipt.control_work > 0);
            assert!(receipt.control_work <= receipt.reserved_control_upper);
            assert_eq!(receipt.total_work, stats.work + receipt.control_work);
            assert!(receipt.total_work <= limits.max_work);
            assert!(receipt.scratch_bytes <= 32 << 20);
            assert_eq!(receipt.serial_queries, Some(0));
        }
    }

    #[test]
    fn strict_tangency_and_probe_expansion_do_not_activate_false_crossings() {
        let mut fixture = Fixture::new(1);
        fixture.retained[0].rect = Rect {
            lx: -1,
            ux: 1500,
            ly: 10,
            uy: 20,
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("tiny pool");
        let limits = ExtractLimits {
            crossing_workers: 2,
            ..Default::default()
        };
        let (bits, _, stats) = pool
            .install(|| fixture.run(&limits, 64 << 20))
            .expect("tangent queries");
        assert_eq!(bits, fixture.expected());
        assert!(bits.iter().all(|&word| word == 0));
        assert_eq!(stats.passive_strips, 0);
        // Preserve the original strict-inequality straddling predicate even
        // for a zero-width query; do not silently "fix" its endpoint quirk.
        fixture.retained[0].rect = Rect {
            lx: 5,
            ux: 5,
            ly: 0,
            uy: 10,
        };
        let (bits, _, _) = pool
            .install(|| fixture.run(&limits, 64 << 20))
            .expect("straddled zero-width query");
        assert_eq!(bits, fixture.expected());
        assert_ne!(bits[0] & 1, 0);
    }

    #[test]
    fn memory_pool_and_first_tile_work_fallback_keep_original_bits_and_no_control_debit() {
        let fixture = Fixture::new(1);
        let baseline = fixture
            .run(&ExtractLimits::default(), 64 << 20)
            .expect("serial baseline");
        let enabled = ExtractLimits {
            crossing_workers: 4,
            ..Default::default()
        };
        let outside = fixture
            .run(&enabled, 64 << 20)
            .expect("outside-pool serial fallback");
        assert_eq!(outside.0, baseline.0);
        assert_eq!(
            outside.1.expect("fallback telemetry").fallback,
            Some("no_existing_pool")
        );
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("tiny pool");
        for (limits, available) in [
            (enabled.clone(), 1 << 20),
            (
                ExtractLimits {
                    crossing_scratch_mib: 1,
                    ..enabled.clone()
                },
                64 << 20,
            ),
            (
                ExtractLimits {
                    max_work: baseline.2.work,
                    ..enabled.clone()
                },
                64 << 20,
            ),
        ] {
            let (bits, receipt, stats) = pool
                .install(|| fixture.run(&limits, available))
                .expect("admission serial fallback");
            assert_eq!(bits, baseline.0);
            assert_eq!(stats.work, baseline.2.work);
            let receipt = receipt.expect("fallback telemetry");
            assert_eq!(receipt.admitted_workers, 1);
            assert_eq!(receipt.control_work, 0);
            assert_eq!(receipt.scratch_bytes, 0);
            assert!(receipt.serial_queries.is_none());
        }
    }

    #[test]
    fn sparse_owned_prefix_falls_back_without_a_whole_design_admission_scan() {
        let mut fixture = Fixture::new(128);
        for shape in &mut fixture.retained[..TILE] {
            shape.owner = Owner::Passive(super::super::PassiveOwner::Ground);
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("tiny pool");
        let limits = ExtractLimits {
            crossing_workers: 2,
            ..Default::default()
        };
        let (bits, receipt, _) = pool
            .install(|| fixture.run(&limits, 64 << 20))
            .expect("sparse prefix fallback");
        assert_eq!(bits, fixture.expected());
        let receipt = receipt.expect("opt-in fallback telemetry");
        assert_eq!(receipt.fallback, Some("sparse_query_prefix"));
        assert_eq!(receipt.preflight_owned_shapes, TILE);
        assert_eq!(receipt.bound_probes, 0);
        assert_eq!(receipt.control_work, 0);
        assert_eq!(receipt.scratch_bytes, 0);
    }

    #[test]
    fn later_work_admission_keeps_completed_bits_and_executes_the_serial_tail_once() {
        let fixture = Fixture::new(128);
        let baseline = fixture
            .run(&ExtractLimits::default(), 64 << 20)
            .expect("serial baseline");
        // Actual local hits are much smaller than the all-instance/all-virtual
        // upper bound. Initial tiles fit; a later whole-tile reservation cannot
        // fit, while the true serial tail plus all control work still can.
        let limits = ExtractLimits {
            crossing_workers: 2,
            max_work: baseline.2.work + 2_000,
            ..Default::default()
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("tiny pool");
        let (bits, receipt, stats) = pool
            .install(|| fixture.run(&limits, 64 << 20))
            .expect("admitted serial tail");
        assert_eq!(bits, fixture.expected());
        assert_eq!(stats.work, baseline.2.work);
        assert_eq!(stats.passive_strips, baseline.2.passive_strips);
        let receipt = receipt.expect("opt-in telemetry");
        assert!(receipt.parallel_tiles > 0);
        assert_eq!(receipt.fallback, Some("remaining_tile_work_admission"));
        assert!(receipt.serial_queries.is_some_and(|count| count > 0));
        assert!(receipt.control_work > 0);
        assert!(receipt.control_work < receipt.reserved_control_upper);
        assert_eq!(receipt.total_work, stats.work + receipt.control_work);
        assert!(receipt.total_work <= limits.max_work);
    }

    #[test]
    fn invalid_queries_preserve_the_first_original_error_without_launch() {
        let mut fixture = Fixture::new(2);
        fixture.retained[0].rect = Rect {
            lx: 0,
            ly: 0,
            ux: 10000,
            uy: 10000,
        };
        fixture.retained[1].rect.ux = i64::MAX;
        let limits = ExtractLimits {
            max_instance_bin_entries: 200,
            ..Default::default()
        };
        let expected = fixture
            .run(&limits, 64 << 20)
            .expect_err("original query extent rejection")
            .to_string();
        assert!(expected.contains("instance query extent"));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("tiny pool");
        let enabled = ExtractLimits {
            crossing_workers: 2,
            ..limits
        };
        let error = pool
            .install(|| fixture.run(&enabled, 64 << 20))
            .expect_err("same ordered rejection");
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn invalid_later_query_preserves_original_error_after_a_valid_prefix() {
        let mut fixture = Fixture::new(128);
        fixture.retained[127].rect.ux = i64::MAX;
        let expected = fixture
            .run(&ExtractLimits::default(), 64 << 20)
            .expect_err("serial coordinate overflow")
            .to_string();
        assert!(expected.contains("instance query coordinate"));
        let limits = ExtractLimits {
            crossing_workers: 2,
            ..Default::default()
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("tiny pool");
        let error = pool
            .install(|| fixture.run(&limits, 64 << 20))
            .expect_err("same serial-tail coordinate overflow");
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn completed_slot_errors_are_consumed_by_original_query_order() {
        let mut slots = [
            Slot {
                query: None,
                work: 3,
                result: Some(Err(Error::Invalid("first".into()))),
            },
            Slot {
                query: None,
                work: 5,
                result: Some(Err(budget("second"))),
            },
        ];
        let mut stats = ExtractStats::default();
        let mut receipt = base_stats(&ExtractLimits::default(), 0);
        let error = consume_results(&mut slots, &mut stats, &mut receipt, 100, 100)
            .expect_err("first error");
        assert_eq!(error.to_string(), "INVALID_DESIGN: first");
        assert_eq!(stats.work, 3);
        assert_eq!(receipt.control_work, 1);
        assert!(slots[1].result.is_some());
        let large = Error::Invalid("x".repeat(ERROR_BYTES + 1));
        assert!(
            bounded_result(Err(large))
                .expect_err("bounded diagnostic")
                .to_string()
                .contains("error payload")
        );
    }

    #[test]
    fn memory_and_control_arithmetic_is_bounded_and_saturating() {
        assert!(estimate(8, 85836, 12011) < 32 << 20);
        assert!(estimate(16, 85836, 12011) > 32 << 20);
        assert_eq!(estimate(usize::MAX, usize::MAX, usize::MAX), u64::MAX);
        for limits in [
            ExtractLimits {
                crossing_workers: 17,
                ..Default::default()
            },
            ExtractLimits {
                crossing_workers: 0,
                ..Default::default()
            },
            ExtractLimits {
                crossing_scratch_mib: 33,
                ..Default::default()
            },
            ExtractLimits {
                crossing_scratch_mib: 0,
                ..Default::default()
            },
        ] {
            assert!(limits.validate().is_err());
        }
    }
}
