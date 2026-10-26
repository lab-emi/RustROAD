//! Memory-admitted groups of whole nets with final full-design validation.
use crate::Result;
use crate::routing::{self, Blockage, RouteInput, RouteLimits, Routes, budget, invalid};
use crate::troute::MemoryEstimate;
use std::collections::BTreeSet;
use std::mem::size_of;

/// Maximum terminals selected by one primary routing batch.
pub const MAX_BATCH_TERMINALS: usize = 1_000_000;
const MAX_BATCHES: usize = 8192;

/// Whole-process admission evidence for a completed batched route.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BatchReport {
    pub batches: usize,
    pub repair_attempts: usize,
    pub displaced_nets: usize,
    pub peak_estimate: MemoryEstimate,
    pub final_validation_bytes: u64,
}

/// Everything a batched route carries from one primary batch to the next,
/// for diagnostic resume. The caller binds the inputs and the stored payload;
/// [`route_resumable`] checks that the state ends at a primary batch boundary
/// of this input and that the completed geometry matches its routes.
#[derive(Clone, Debug)]
pub struct BatchCheckpoint {
    /// Terminal-order position where the next primary batch starts.
    pub begin: usize,
    pub routes: Routes,
    pub report: BatchReport,
    /// Completed-route obstacles after the input obstacles, in route order.
    pub completed: Vec<Blockage>,
}

/// State after a completed primary batch, borrowed for a checkpoint sink.
pub struct BatchProgress<'a> {
    pub begin: usize,
    pub routes: &'a Routes,
    pub report: &'a BatchReport,
    pub completed: &'a [Blockage],
}

fn guard(cap: u64, extra: u64, stage: &str) -> Result<()> {
    let live = crate::memory::live_bytes()
        .ok_or_else(|| budget("batched routing needs a process memory probe"))?;
    if live.saturating_add(extra) > cap {
        return Err(budget(&format!(
            "batched routing memory at {stage}: live {live} plus allocation {extra} exceeds {cap}"
        )));
    }
    Ok(())
}

fn reserve<T>(items: &mut Vec<T>, add: usize, cap: u64) -> Result<()> {
    let count = items
        .len()
        .checked_add(add)
        .ok_or_else(|| budget("batched routing count overflow"))?;
    if count > items.capacity() {
        guard(
            cap,
            (count as u64).saturating_mul(size_of::<T>() as u64),
            "vector growth",
        )?;
        items
            .try_reserve_exact(add)
            .map_err(|_| budget("batched routing allocation"))?;
    }
    Ok(())
}

fn extend<T>(items: &mut Vec<T>, added: &mut Vec<T>, cap: u64) -> Result<()> {
    reserve(items, added.len(), cap)?;
    items.append(added);
    Ok(())
}

fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(count)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("batched routing total work"))?;
    Ok(())
}

/// One primary batch from `begin`: whole nets in terminal order up to
/// `batch_terminals` terminals. Returns its end and its routable nets.
fn select(
    input: &RouteInput,
    order: &[u32],
    begin: usize,
    batch_terminals: usize,
) -> Result<(usize, Vec<u32>)> {
    let mut end = begin;
    let mut selected = Vec::new();
    while end < order.len() {
        let net = input.terminals[order[end] as usize].net;
        let mut next = end + 1;
        while next < order.len() && input.terminals[order[next] as usize].net == net {
            next += 1;
        }
        if next - end > batch_terminals {
            return Err(budget("one routed net exceeds batch terminal limit"));
        }
        if next - begin > batch_terminals {
            break;
        }
        if next - end >= 2 {
            selected.push(net);
        }
        end = next;
    }
    if end == begin {
        return Err(budget("empty routing batch"));
    }
    Ok((end, selected))
}

struct Batch<'a> {
    input: &'a RouteInput,
    limits: &'a RouteLimits,
    order: &'a [u32],
    begin: usize,
    cap: u64,
}

impl Batch<'_> {
    fn route(
        &self,
        view: &mut RouteInput,
        out: &mut Routes,
        report: &mut BatchReport,
    ) -> Result<Routes> {
        let mut retries = 0;
        let mut displaced = 0;
        let mut access_alternatives = false;
        loop {
            view.held_bytes = routing::loaded_held_estimate(0, view, crate::memory::live_bytes());
            let mut local = self.limits.clone();
            let stored = out.wires.len() + out.vias.len() + out.patches.len();
            local.max_segments = self
                .limits
                .max_segments
                .checked_sub(stored)
                .filter(|&n| n > 0)
                .ok_or_else(|| budget("batched routing output segments"))?;
            local.max_expansions = self
                .limits
                .max_expansions
                .checked_sub(out.stats.expansions)
                .filter(|&n| n > 0)
                .ok_or_else(|| budget("batched routing total expansions"))?;
            local.max_comparisons = self
                .limits
                .max_comparisons
                .checked_sub(out.stats.comparisons)
                .filter(|&n| n > 0)
                .ok_or_else(|| budget("batched routing total work"))?;
            let estimate =
                routing::track_memory_estimate(view, &local)?.expect("checked track plan");
            if estimate.total() > report.peak_estimate.total() {
                report.peak_estimate = estimate;
            }
            if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                eprintln!(
                    "[batch] {} active terminals {} retry {retries} estimate {} MiB",
                    report.batches,
                    view.terminals.len(),
                    estimate.total() >> 20
                );
            }
            let result = if access_alternatives {
                routing::route_cpu_ref(view, &local)
            } else {
                routing::route_greedy_access(view, &local)
            };
            match result {
                Ok(done) => return Ok(done),
                Err(error) => {
                    let Some(regions) = self.recovery_regions(&error, out)? else {
                        return Err(error);
                    };
                    if retries >= self.limits.max_batch_retries {
                        return Err(budget("batch recovery retry limit"));
                    }
                    let reopened = self.reopen(view, out, &regions, displaced)?;
                    if reopened == 0 {
                        // Preserve established choices while completed-net
                        // recovery can clear the access window. Only a remaining
                        // pin-access failure needs the alternate local policy.
                        if !access_alternatives && matches!(error, crate::Error::PinAccess(_)) {
                            access_alternatives = true;
                            retries += 1;
                            report.repair_attempts += 1;
                            if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                                eprintln!("[batch] bounded alternate access after {error}");
                            }
                            continue;
                        }
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[batch] no completed net overlaps the recovery regions: {error}"
                            );
                        }
                        return Err(error);
                    }
                    retries += 1;
                    displaced += reopened;
                    report.repair_attempts += 1;
                    report.displaced_nets += reopened;
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        eprintln!("[batch] reopened {reopened} completed nets after {error}");
                    }
                }
            }
        }
    }

    fn recovery_regions(
        &self,
        error: &crate::Error,
        out: &mut Routes,
    ) -> Result<Option<Vec<crate::Rect>>> {
        let (count, comparisons, expansions) = match error {
            crate::Error::PinAccess(f) => (f.blocked.len(), f.comparisons, f.expansions),
            crate::Error::RoutingRule(f) => (f.regions.len(), f.comparisons, f.expansions),
            crate::Error::NoPath(f) => (1, f.comparisons, f.expansions),
            _ => return Ok(None),
        };
        if count == 0 || count > self.limits.max_batch_blocked_pins {
            return Err(budget("batch recovery region count"));
        }
        // Every failed attempt consumes the aggregate allowance before a retry.
        charge(
            &mut out.stats.comparisons,
            comparisons,
            self.limits.max_comparisons,
        )?;
        charge(
            &mut out.stats.expansions,
            expansions,
            self.limits.max_expansions,
        )?;
        guard(
            self.cap,
            (count * size_of::<crate::Rect>()) as u64,
            "batch recovery regions",
        )?;
        let mut regions = Vec::new();
        regions
            .try_reserve_exact(count)
            .map_err(|_| budget("batch recovery region allocation"))?;
        match error {
            crate::Error::PinAccess(f) => regions.extend(f.blocked.iter().map(|p| p.region)),
            crate::Error::RoutingRule(f) => regions.extend_from_slice(&f.regions),
            crate::Error::NoPath(f) => regions.push(f.region),
            _ => unreachable!("recovery error kind was checked"),
        }
        Ok(Some(regions))
    }

    fn reopen(
        &self,
        view: &mut RouteInput,
        out: &mut Routes,
        regions: &[crate::Rect],
        displaced: usize,
    ) -> Result<usize> {
        guard(
            self.cap,
            (self.limits.max_batch_displaced_nets as u64) * 192 + (regions.len() as u64) * 256,
            "batch repair owners",
        )?;
        let halo = self
            .input
            .layers
            .iter()
            .map(|l| l.spacing.saturating_add(l.width))
            .max()
            .unwrap_or(0)
            .max(self.input.step.saturating_mul(2));
        let mut windows = Vec::new();
        windows
            .try_reserve_exact(regions.len())
            .map_err(|_| budget("batch blocked-pin windows"))?;
        for region in regions {
            windows.push(routing::expand(*region, halo)?);
        }
        let window_index = crate::troute::WindowIndex::new(
            &windows,
            &mut out.stats.comparisons,
            self.limits.max_comparisons,
        )?;
        let base = self.input.obstacles.len();
        charge(
            &mut out.stats.comparisons,
            view.obstacles.len().saturating_sub(base),
            self.limits.max_comparisons,
        )?;
        let mut victims = BTreeSet::new();
        // Only completed routing is eligible. Imported cell and supply geometry stays fixed.
        for obstacle in &view.obstacles[base..] {
            if let Some(net) = obstacle.net
                && !victims.contains(&net)
                && window_index.touches_any(
                    &windows,
                    obstacle.rect,
                    &mut out.stats.comparisons,
                    self.limits.max_comparisons,
                )?
            {
                if displaced + victims.len() >= self.limits.max_batch_displaced_nets {
                    return Err(budget("batch displaced-net limit"));
                }
                victims.insert(net);
            }
        }
        if victims.is_empty() {
            return Ok(0);
        }
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            // Bounded by `max_batch_displaced_nets`; listed nets also print
            // the completed geometry that reopening removes.
            eprintln!("[batch] reopen victims {victims:?}");
            let traced: BTreeSet<u32> = std::env::var("RUSTROAD_TRACE_NETS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .take(64)
                .collect();
            if traced.iter().any(|n| victims.contains(n)) {
                for b in view.obstacles[base..].iter().filter(|b| {
                    b.net
                        .is_some_and(|n| traced.contains(&n) && victims.contains(&n))
                }) {
                    eprintln!(
                        "[batch] reopen net {} shape layer {} rect [{}, {}, {}, {}]",
                        b.net.unwrap_or(u32::MAX),
                        b.layer,
                        b.rect.lx,
                        b.rect.ly,
                        b.rect.ux,
                        b.rect.uy
                    );
                }
            }
        }
        let prior = &self.order[..self.begin];
        let mut ranges = Vec::new();
        let mut added = 0usize;
        for &net in &victims {
            charge(
                &mut out.stats.comparisons,
                2 * usize::BITS as usize,
                self.limits.max_comparisons,
            )?;
            let lo = prior.partition_point(|&i| self.input.terminals[i as usize].net < net);
            let hi = prior.partition_point(|&i| self.input.terminals[i as usize].net <= net);
            if hi - lo < 2 {
                return Err(invalid("completed route is missing its original terminals"));
            }
            added = added
                .checked_add(hi - lo)
                .ok_or_else(|| budget("batch repair terminal count"))?;
            if view.terminals.len().saturating_add(added) > self.limits.max_batch_repair_terminals {
                return Err(budget("batch repair terminal limit"));
            }
            ranges.push(lo..hi);
        }
        charge(
            &mut out.stats.comparisons,
            added,
            self.limits.max_comparisons,
        )?;
        let shape_bytes = ranges
            .iter()
            .flat_map(|r| &prior[r.clone()])
            .map(|&i| {
                (self.input.terminals[i as usize].shapes.len() * size_of::<routing::LayerRect>())
                    as u64
            })
            .sum::<u64>();
        let total = view.terminals.len().saturating_add(added);
        let growth = if total > view.terminals.capacity() {
            (total as u64).saturating_mul(size_of::<routing::Terminal>() as u64)
        } else {
            0
        };
        guard(
            self.cap,
            shape_bytes.saturating_add(growth),
            "batch repair terminal shapes",
        )?;
        reserve(&mut view.terminals, added, self.cap)?;
        for range in ranges {
            view.terminals.extend(
                prior[range]
                    .iter()
                    .map(|&i| self.input.terminals[i as usize].clone()),
            );
        }
        let scans = out
            .wires
            .len()
            .saturating_mul(2)
            .saturating_add(out.vias.len())
            .saturating_add(out.patches.len())
            .saturating_add(out.access.len())
            .saturating_add(out.nets.len())
            .saturating_add(view.obstacles.len())
            .saturating_add(self.input.ties.len().saturating_mul(2));
        charge(
            &mut out.stats.comparisons,
            scans,
            self.limits.max_comparisons,
        )?;
        let mut removed_length = 0u64;
        for wire in out.wires.iter().filter(|w| victims.contains(&w.net)) {
            removed_length = removed_length
                .checked_add(wire.start[0].abs_diff(wire.end[0]))
                .and_then(|v| v.checked_add(wire.start[1].abs_diff(wire.end[1])))
                .ok_or_else(|| budget("batch repair wire length"))?;
        }
        out.stats.wire_length_dbu = out
            .stats
            .wire_length_dbu
            .checked_sub(removed_length)
            .ok_or_else(|| invalid("completed route wire length mismatch"))?;
        out.wires.retain(|w| !victims.contains(&w.net));
        out.vias.retain(|v| !victims.contains(&v.net));
        out.patches.retain(|p| !victims.contains(&p.net));
        out.access.retain(|a| !victims.contains(&a.net));
        out.nets.retain(|n| !victims.contains(n));
        let mut index = 0;
        view.obstacles.retain(|b| {
            let keep = index < base || b.net.is_none_or(|n| !victims.contains(&n));
            index += 1;
            keep
        });
        let ties = self.input.ties.iter().filter(|t| victims.contains(&t.net));
        reserve(&mut view.ties, ties.clone().count(), self.cap)?;
        view.ties.extend(ties.cloned());
        Ok(victims.len())
    }
}

/// Route whole nets in bounded groups; previously completed metal is fixed geometry.
///
/// The final validator sees every original terminal and every completed route.
/// Bounded repair may reopen nearby completed nets to preserve a later pin's access
/// or to open a later net's pocket that their metal seals.
/// Repair growth is separately capped by `RouteLimits::max_batch_repair_terminals`.
pub fn route(
    input: &RouteInput,
    limits: &RouteLimits,
    batch_terminals: usize,
) -> Result<(Routes, BatchReport)> {
    route_resumable(input, limits, batch_terminals, None, &mut |_| Ok(()))
}

/// [`route`] that may continue from a [`BatchCheckpoint`] and hands the
/// state after every completed primary batch to `progress` (an error there
/// stops the route). A resumed route repeats the remaining primary batches
/// exactly as the uninterrupted route would; it is diagnostic evidence only.
pub fn route_resumable(
    input: &RouteInput,
    limits: &RouteLimits,
    batch_terminals: usize,
    resume: Option<BatchCheckpoint>,
    progress: &mut dyn FnMut(BatchProgress<'_>) -> Result<()>,
) -> Result<(Routes, BatchReport)> {
    if !(2..=MAX_BATCH_TERMINALS).contains(&batch_terminals) || input.tracks.is_none() {
        return Err(invalid(
            "batched routing requires tracks and 2..1000000 terminals per batch",
        ));
    }
    let cap = input
        .tracks
        .as_ref()
        .expect("checked track plan")
        .memory_mib
        .min(65_536)
        .saturating_mul(1 << 20);
    guard(
        cap,
        (input.terminals.len() as u64).saturating_mul(32),
        "input validation",
    )?;
    routing::preflight(input, limits)?;
    guard(
        cap,
        (input.terminals.len() as u64).saturating_mul(8),
        "terminal order",
    )?;
    let mut work = 0;
    let order = crate::rule_index::order(
        input.terminals.len(),
        |i| input.terminals[i].net,
        &mut work,
        limits.max_comparisons,
    )?;
    let geometry_bytes = input
        .access_classes
        .iter()
        .map(|c| ((c.shapes.len() + c.blockers.len()) * size_of::<routing::LayerRect>()) as u64)
        .sum::<u64>()
        .saturating_add((input.access_classes.len() * size_of::<routing::AccessClass>()) as u64);
    guard(
        cap,
        (input.obstacles.len() as u64)
            .saturating_mul(size_of::<Blockage>() as u64)
            .saturating_add(geometry_bytes),
        "shared input view",
    )?;
    let mut view = RouteInput {
        die: input.die,
        grid: input.grid,
        step: input.step,
        access_layer: input.access_layer,
        layers: input.layers.clone(),
        vias: input.vias.clone(),
        obstacles: input.obstacles.clone(),
        instances: input.instances.clone(),
        terminals: Vec::new(),
        rules: input.rules,
        access_classes: input.access_classes.clone(),
        tracks: input.tracks.clone(),
        ties: Vec::new(),
        held_bytes: 0,
    };
    let mut out = Routes {
        wires: Vec::new(),
        vias: Vec::new(),
        access: Vec::new(),
        patches: Vec::new(),
        nets: Vec::new(),
        stats: Default::default(),
    };
    out.stats.grid_nodes = crate::troute::graph_nodes(input, limits)?;
    out.stats.comparisons = work;
    let mut report = BatchReport {
        batches: 0,
        repair_attempts: 0,
        displaced_nets: 0,
        peak_estimate: MemoryEstimate::default(),
        final_validation_bytes: 0,
    };
    let base = input.obstacles.len();
    let mut begin = 0;
    if let Some(state) = resume {
        // The primary batch boundaries depend only on the terminal order.
        let mut batches = 0;
        while begin < state.begin && begin < order.len() {
            if batches == MAX_BATCHES {
                return Err(budget("routing batch count"));
            }
            begin = select(input, &order, begin, batch_terminals)?.0;
            batches += 1;
        }
        if begin != state.begin || batches != state.report.batches {
            return Err(invalid(
                "checkpoint does not end at a primary batch boundary of this input",
            ));
        }
        let shapes = state.routes.vias.iter().try_fold(
            state
                .routes
                .wires
                .len()
                .saturating_add(state.routes.patches.len()),
            |n, via| {
                input
                    .vias
                    .get(via.kind as usize)
                    .and_then(|v| n.checked_add(v.shapes.len()))
                    .ok_or_else(|| invalid("checkpoint via kind"))
            },
        )?;
        if shapes != state.completed.len() || state.completed.iter().any(|b| b.net.is_none()) {
            return Err(invalid(
                "checkpoint completed geometry does not match its routes",
            ));
        }
        if base
            .saturating_add(shapes)
            .saturating_add(input.instances.as_ref().map_or(0, |v| v.len()))
            > limits.max_obstacles
        {
            return Err(budget("completed routing obstacle count"));
        }
        let mut completed = state.completed;
        extend(&mut view.obstacles, &mut completed, cap)?;
        out = state.routes;
        report = state.report;
    }
    while begin < order.len() {
        let (end, selected) = select(input, &order, begin, batch_terminals)?;
        if report.batches == MAX_BATCHES {
            return Err(budget("routing batch count"));
        }
        view.terminals.clear();
        let shape_bytes = order[begin..end]
            .iter()
            .map(|&i| {
                (input.terminals[i as usize].shapes.len() * size_of::<routing::LayerRect>()) as u64
            })
            .sum::<u64>();
        guard(cap, shape_bytes, "batch terminal shapes")?;
        reserve(&mut view.terminals, end - begin, cap)?;
        view.terminals.extend(
            order[begin..end]
                .iter()
                .map(|&i| input.terminals[i as usize].clone()),
        );
        view.ties.clear();
        charge(
            &mut out.stats.comparisons,
            (end - begin).saturating_add(input.ties.len().saturating_mul(2)),
            limits.max_comparisons,
        )?;
        let ties = input
            .ties
            .iter()
            .filter(|tie| selected.binary_search(&tie.net).is_ok());
        reserve(&mut view.ties, ties.clone().count(), cap)?;
        view.ties.extend(ties.cloned());
        let mut done = Batch {
            input,
            limits,
            order: &order,
            begin,
            cap,
        }
        .route(&mut view, &mut out, &mut report)?;
        report.batches += 1;
        out.stats.grid_nodes = done.stats.grid_nodes;
        out.stats.expansions = out
            .stats
            .expansions
            .checked_add(done.stats.expansions)
            .ok_or_else(|| budget("batched routing expansions"))?;
        out.stats.comparisons = out
            .stats
            .comparisons
            .checked_add(done.stats.comparisons)
            .ok_or_else(|| budget("batched routing work"))?;
        out.stats.wire_length_dbu = out
            .stats
            .wire_length_dbu
            .checked_add(done.stats.wire_length_dbu)
            .ok_or_else(|| budget("batched routing wire length"))?;
        let shapes = done.vias.iter().try_fold(
            done.wires.len().saturating_add(done.patches.len()),
            |n, via| {
                n.checked_add(input.vias[via.kind as usize].shapes.len())
                    .ok_or_else(|| budget("batched routing shape count"))
            },
        )?;
        charge(&mut out.stats.comparisons, shapes, limits.max_comparisons)?;
        if view
            .obstacles
            .len()
            .saturating_add(shapes)
            .saturating_add(input.instances.as_ref().map_or(0, |v| v.len()))
            > limits.max_obstacles
        {
            return Err(budget("completed routing obstacle count"));
        }
        reserve(&mut view.obstacles, shapes, cap)?;
        for wire in &done.wires {
            view.obstacles.push(Blockage {
                layer: wire.layer,
                rect: routing::wire_rect(wire, input.layers[wire.layer as usize].width)?,
                net: Some(wire.net),
            });
        }
        for via in &done.vias {
            for shape in &input.vias[via.kind as usize].shapes {
                view.obstacles.push(Blockage {
                    layer: shape.layer,
                    rect: routing::shifted(shape.rect, via.at)?,
                    net: Some(via.net),
                });
            }
        }
        for patch in &done.patches {
            view.obstacles.push(Blockage {
                layer: patch.layer,
                rect: patch.rect,
                net: Some(patch.net),
            });
        }
        extend(&mut out.wires, &mut done.wires, cap)?;
        extend(&mut out.vias, &mut done.vias, cap)?;
        extend(&mut out.patches, &mut done.patches, cap)?;
        extend(&mut out.access, &mut done.access, cap)?;
        extend(&mut out.nets, &mut done.nets, cap)?;
        guard(cap, 0, "batch completion")?;
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            let held = |len: usize, capacity: usize, size: usize| {
                format!("{len}/{capacity} ({} MiB)", (capacity * size) >> 20)
            };
            eprintln!(
                "[batch] {} complete obstacles {} wires {} vias {} patches {} access {} terminals {}",
                report.batches - 1,
                held(
                    view.obstacles.len(),
                    view.obstacles.capacity(),
                    size_of::<Blockage>()
                ),
                held(
                    out.wires.len(),
                    out.wires.capacity(),
                    size_of::<routing::Wire>()
                ),
                held(
                    out.vias.len(),
                    out.vias.capacity(),
                    size_of::<routing::Via>()
                ),
                held(
                    out.patches.len(),
                    out.patches.capacity(),
                    size_of::<routing::Patch>()
                ),
                held(
                    out.access.len(),
                    out.access.capacity(),
                    size_of::<routing::Access>()
                ),
                held(
                    view.terminals.len(),
                    view.terminals.capacity(),
                    size_of::<routing::Terminal>()
                ),
            );
        }
        progress(BatchProgress {
            begin: end,
            routes: &out,
            report: &report,
            completed: &view.obstacles[base..],
        })?;
        begin = end;
    }
    drop(view);
    drop(order);
    out.nets.sort_unstable();
    let extra = crate::troute::validation_working_bytes(input, &out, limits)?;
    let live = crate::memory::live_bytes().ok_or_else(|| budget("final route memory probe"))?;
    report.final_validation_bytes = live.saturating_add(extra);
    guard(cap, extra, "full-design validation")?;
    routing::validate(input, &out, limits)?;
    guard(cap, 0, "full-design validation complete")?;
    Ok((out, report))
}
