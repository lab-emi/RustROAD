//! Memory-admitted groups of whole nets with final full-design validation.
use crate::Result;
use crate::routing::{self, Blockage, RouteInput, RouteLimits, Routes, budget, invalid};
use crate::troute::MemoryEstimate;
use std::mem::size_of;

/// Maximum terminals admitted by one routing batch.
pub const MAX_BATCH_TERMINALS: usize = 1_000_000;
const MAX_BATCHES: usize = 8192;

/// Whole-process admission evidence for a completed batched route.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BatchReport {
    pub batches: usize,
    pub peak_estimate: MemoryEstimate,
    pub final_validation_bytes: u64,
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

/// Route whole nets in bounded groups; previously completed metal is fixed geometry.
///
/// The final validator sees every original terminal and every completed route.
/// A congested batch fails rather than moving or omitting an earlier net.
pub fn route(
    input: &RouteInput,
    limits: &RouteLimits,
    batch_terminals: usize,
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
        peak_estimate: MemoryEstimate::default(),
        final_validation_bytes: 0,
    };
    let mut begin = 0;
    while begin < order.len() {
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
        view.held_bytes = routing::loaded_held_estimate(0, &view, crate::memory::live_bytes());
        let mut local = limits.clone();
        let stored = out
            .wires
            .len()
            .saturating_add(out.vias.len())
            .saturating_add(out.patches.len());
        local.max_segments = limits
            .max_segments
            .checked_sub(stored)
            .filter(|&n| n > 0)
            .ok_or_else(|| budget("batched routing output segments"))?;
        local.max_expansions = limits
            .max_expansions
            .checked_sub(out.stats.expansions)
            .filter(|&n| n > 0)
            .ok_or_else(|| budget("batched routing total expansions"))?;
        local.max_comparisons = limits
            .max_comparisons
            .checked_sub(out.stats.comparisons)
            .filter(|&n| n > 0)
            .ok_or_else(|| budget("batched routing total work"))?;
        let estimate = routing::track_memory_estimate(&view, &local)?.expect("checked track plan");
        if estimate.total() > report.peak_estimate.total() {
            report.peak_estimate = estimate;
        }
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            eprintln!(
                "[batch] {} terminals {}..{end}/{} nets {} estimate {} MiB",
                report.batches,
                begin,
                order.len(),
                selected.len(),
                estimate.total() >> 20
            );
        }
        let mut done = routing::route_cpu_ref(&view, &local)?;
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
