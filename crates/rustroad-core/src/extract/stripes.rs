//! Stripe-partitioned extraction (`cpu_ref`). The design is cut along x into
//! stripes. Each stripe's input holds, whole, every routed net and passive
//! conductor that meets the stripe core widened by a halo, and extracts only
//! the strips anchored in its core (`ExtractRegion`). A tree is built from
//! its whole net, so every stripe holding a net builds the same tree (checked
//! here); each strip's capacitance is computed in exactly one stripe and
//! summed. The network equals `extract_cpu_ref` with the same limits up to
//! the order of floating-point sums; peak memory is one stripe's working set
//! plus the merged network.
use super::{
    ExtractInput, ExtractLimits, ExtractMemory, ExtractRegion, ExtractStats, PassiveOwner,
    PassiveShape, RcCoupling, RcNet, RcNetwork, budget, extract_region, invalid, memory_estimate,
    metal_extent, preflight,
};
use crate::Rect;
use crate::Result;
use crate::routing::{Routes, Terminal};
use rayon::prelude::*;

/// How `extract_partitioned` cuts the design.
#[derive(Clone, Debug)]
pub struct StripePlan {
    /// Interior stripe boundaries along x (DBU), strictly increasing: `n`
    /// cuts make `n + 1` stripes that together cover every x.
    pub cuts: Vec<i64>,
    /// Halo on both sides of each stripe core (DBU), at least
    /// `minimum_halo`.
    pub halo: i64,
    /// Caps of the merged network; each stripe obeys `ExtractLimits`.
    pub max_nodes: usize,
    pub max_elements: usize,
    pub max_couplings: usize,
    /// Stripes extracted at once (each serial; merged in stripe order, so
    /// the result does not depend on it). Memory is checked upfront for
    /// this many stripe working sets.
    pub parallel: usize,
}
/// Cuts of `count` stripes of equal width over `lo..hi` (the outer stripes
/// also cover everything beyond).
pub fn even_cuts(lo: i64, hi: i64, count: usize) -> Result<Vec<i64>> {
    if count == 0 || hi <= lo || count as u128 > (i128::from(hi) - i128::from(lo)) as u128 {
        return Err(invalid("stripe count must be within 1..=die width"));
    }
    let width = i128::from(hi) - i128::from(lo);
    Ok((1..count)
        .map(|i| (i128::from(lo) + width * i as i128 / count as i128) as i64)
        .collect())
}

/// One stripe of a partitioned extraction.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct StripeStats {
    /// Core x range `[lo, hi)`; the outer stripes are open-ended
    /// (`i64::MIN` / `i64::MAX`).
    pub core: [i64; 2],
    /// Nets held whole by this stripe and the nets it counts (driver access
    /// point in its core).
    pub nets: usize,
    pub home_nets: usize,
    /// Retained passive shapes held, and all conductor shapes (with the
    /// virtual cell shapes).
    pub passive_shapes: usize,
    pub shapes: usize,
    pub couplings: usize,
    pub work: usize,
    pub estimate_bytes: u64,
    /// Wall time of this stripe (input selection and extraction).
    pub seconds: f64,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct PartitionStats {
    pub halo_dbu: i64,
    pub passive_strip_dbu: i64,
    /// Trees that more than one stripe built (each checked identical).
    pub repeated_trees: usize,
    pub stripes: Vec<StripeStats>,
}

/// Smallest halo holding the neighbourhood of every routed strip and of
/// every passive piece of the instance geometry: the longest such strip
/// (wire run, passive piece, instance, patch or via shape) plus the widest
/// routing layer and the widest calibration window. Retained passive shapes
/// wider than that are checked piece by piece (`RC_EXTRACT` halo error).
pub fn minimum_halo(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<i64> {
    let mut longest = limits
        .max_segment_length
        .max(limits.max_passive_strip_length)
        .max(instance_extent(input)?);
    let mut include = |r: &Rect| {
        longest = longest.max(r.ux - r.lx).max(r.uy - r.ly);
    };
    for patch in &input.routes.patches {
        include(&patch.rect);
    }
    for via in input.vias {
        for shape in &via.shapes {
            include(&shape.rect);
        }
    }
    let width = input
        .layers
        .iter()
        .filter(|l| l.vertical.is_some())
        .map(|l| l.width)
        .max()
        .unwrap_or(0);
    let window = input
        .tech
        .layers
        .iter()
        .flatten()
        .flat_map(|m| &m.contexts)
        .map(|c| c.rows[c.rows.len() - 1][0])
        .fold(0.0f64, f64::max);
    let window = (window * input.tech.dbu_per_um).ceil();
    if !(window.is_finite() && window < (i64::MAX / 4) as f64) {
        return Err(budget("extraction calibration window"));
    }
    longest
        .checked_add(width)
        .and_then(|n| n.checked_add(window as i64 + 1))
        .ok_or_else(|| budget("extraction stripe halo"))
}

/// Widest or tallest cell instance (bounds of its shared geometry).
fn instance_extent(input: &ExtractInput<'_>) -> Result<i64> {
    let mut extent = 0;
    if let Some(instances) = input.instances {
        for i in 0..instances.geometry.len() {
            if let Some(r) = instances.geometry.bounds(i)? {
                extent = extent.max(r.ux - r.lx).max(r.uy - r.ly);
            }
        }
    }
    Ok(extent)
}

/// `extract_cpu_ref` computed stripe by stripe (`StripePlan`): the same
/// trees, ground and coupling capacitors up to floating-point summation
/// order. Needs a finite `max_passive_strip_length`.
pub fn extract_partitioned(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    plan: &StripePlan,
) -> Result<RcNetwork> {
    preflight(input, limits)?;
    if limits.max_passive_strip_length == i64::MAX {
        return Err(invalid(
            "partitioned extraction needs a finite passive strip length",
        ));
    }
    if plan.cuts.windows(2).any(|w| w[0] >= w[1]) {
        return Err(invalid("stripe cuts must increase strictly"));
    }
    if plan.max_nodes == 0 || plan.max_elements == 0 || plan.max_couplings == 0 {
        return Err(budget("merged extraction caps must be positive"));
    }
    if !(1..=256).contains(&plan.parallel) {
        return Err(budget(
            "concurrent extraction stripes must be within 1..=256",
        ));
    }
    // Each concurrent stripe may reach the per-call working estimate (the
    // whole design's bin bound is an upper bound for a stripe's).
    let whole = memory_estimate(input, limits)?;
    let stripe = whole.working_bytes.saturating_add(whole.geometry_bytes);
    let need = whole
        .held_bytes
        .saturating_add(stripe.saturating_mul(plan.parallel as u64) / 4 * 5);
    if need > whole.limit_bytes {
        return Err(budget(&format!(
            "{} concurrent extraction stripes estimate {} MiB exceeds memory_mib {}",
            plan.parallel,
            need.div_ceil(1 << 20),
            limits.memory_mib
        )));
    }
    let need = minimum_halo(input, limits)?;
    if plan.halo < need {
        return Err(invalid(&format!(
            "extraction stripe halo {} DBU is below the minimum {need} DBU",
            plan.halo
        )));
    }
    let span = metal_extent(input)?;
    let margin = instance_extent(input)?;
    let routes = input.routes;
    // Position of every routed net (the output order).
    let top = routes
        .nets
        .iter()
        .copied()
        .max()
        .map_or(0, |n| n as usize + 1);
    if top > 1 << 28 {
        return Err(budget("routed net ids for stripe partition"));
    }
    let mut position = vec![u32::MAX; top];
    for (i, &net) in routes.nets.iter().enumerate() {
        if position[net as usize] != u32::MAX {
            return Err(invalid("routed net listed twice"));
        }
        position[net as usize] = i as u32;
    }
    let at = |net: u32| -> Option<usize> {
        position
            .get(net as usize)
            .filter(|&&p| p != u32::MAX)
            .map(|&p| p as usize)
    };
    // The x range of each routed net's metal (one interval: a connected
    // net projects onto an interval) and of its pin access points.
    let n = routes.nets.len();
    let mut metal = vec![(i64::MAX, i64::MIN); n];
    let mut access = vec![(i64::MAX, i64::MIN); n];
    let widen = |r: &mut (i64, i64), lo: i64, hi: i64| {
        r.0 = r.0.min(lo);
        r.1 = r.1.max(hi);
    };
    for w in &routes.wires {
        if let Some(p) = at(w.net) {
            let layer = input
                .layers
                .get(w.layer as usize)
                .ok_or_else(|| invalid("extraction wire layer"))?;
            let r = crate::routing::wire_rect(w, layer.width)?;
            widen(&mut metal[p], r.lx, r.ux);
        }
    }
    for v in &routes.vias {
        if let Some(p) = at(v.net) {
            let kind = input
                .vias
                .get(v.kind as usize)
                .ok_or_else(|| invalid("via kind outside routing input"))?;
            for shape in &kind.shapes {
                let r = crate::routing::shifted(shape.rect, v.at)?;
                widen(&mut metal[p], r.lx, r.ux);
            }
        }
    }
    for patch in &routes.patches {
        let p =
            at(patch.net).ok_or_else(|| invalid("routing patch on a net without an RC tree"))?;
        widen(&mut metal[p], patch.rect.lx, patch.rect.ux);
    }
    let mut routed_pins = Vec::<u64>::new();
    for a in &routes.access {
        if let Some(p) = at(a.net) {
            widen(&mut metal[p], a.at[0], a.at[0]);
            widen(&mut access[p], a.at[0], a.at[0]);
            let word = a.pin as usize / 64;
            if word >= routed_pins.len() {
                routed_pins.resize(word + 1, 0);
            }
            routed_pins[word] |= 1 << (a.pin % 64);
        }
    }
    let mut cores = Vec::with_capacity(plan.cuts.len() + 1);
    let mut lo = i64::MIN;
    for &cut in &plan.cuts {
        cores.push((lo, cut));
        lo = cut;
    }
    cores.push((lo, i64::MAX));
    let mut trees: Vec<Option<RcNet>> = (0..n).map(|_| None).collect();
    let mut couplings: Vec<RcCoupling> = Vec::new();
    let mut stats = ExtractStats::default();
    let mut memory: Option<ExtractMemory> = None;
    let mut partition = PartitionStats {
        halo_dbu: plan.halo,
        passive_strip_dbu: limits.max_passive_strip_length,
        ..Default::default()
    };
    let one = |lo: i64, hi: i64| -> Result<(RcNetwork, StripeStats)> {
        let started = std::time::Instant::now();
        let complete = Rect {
            lx: lo.saturating_sub(plan.halo),
            ux: hi.saturating_add(plan.halo),
            ly: i64::MIN,
            uy: i64::MAX,
        };
        // Closed test: metal touching the halo edge is held as well.
        let meets = |a: i64, b: i64| a <= b && b >= complete.lx && a <= complete.ux;
        // Whole nets with metal in the halo, or with a pin access point near
        // enough for one of its cell's shapes to reach into it.
        let mut held: Vec<bool> = (0..n)
            .map(|p| {
                meets(metal[p].0, metal[p].1)
                    || meets(
                        access[p].0.saturating_sub(margin),
                        access[p].1.saturating_add(margin),
                    )
            })
            .collect();
        for shape in input.passive {
            if let PassiveOwner::Pin { net, .. } = shape.owner
                && meets(shape.rect.lx, shape.rect.ux)
                && let Some(p) = at(net)
            {
                held[p] = true;
            }
        }
        let keep = |net: u32| at(net).is_some_and(|p| held[p]);
        let sub_routes = Routes {
            wires: routes
                .wires
                .iter()
                .filter(|w| keep(w.net))
                .cloned()
                .collect(),
            vias: routes
                .vias
                .iter()
                .filter(|v| keep(v.net))
                .cloned()
                .collect(),
            access: routes
                .access
                .iter()
                .filter(|a| keep(a.net))
                .cloned()
                .collect(),
            patches: routes
                .patches
                .iter()
                .filter(|p| keep(p.net))
                .cloned()
                .collect(),
            nets: routes.nets.iter().copied().filter(|&n| keep(n)).collect(),
            stats: routes.stats.clone(),
        };
        let sub_terminals: Vec<Terminal> = input
            .terminals
            .iter()
            .filter(|t| keep(t.net))
            .cloned()
            .collect();
        // Passive metal in the halo, and every pin shape of a held net (its
        // pieces subtract same-net metal wherever it lies).
        let sub_passive: Vec<PassiveShape> = input
            .passive
            .iter()
            .filter(|s| {
                meets(s.rect.lx, s.rect.ux)
                    || matches!(s.owner, PassiveOwner::Pin { net, .. } if keep(net))
            })
            .cloned()
            .collect();
        let sub = ExtractInput {
            layers: input.layers,
            vias: input.vias,
            routes: &sub_routes,
            terminals: &sub_terminals,
            passive: &sub_passive,
            instances: input.instances,
            held_bytes: input.held_bytes,
            tech: input.tech,
        };
        let region = ExtractRegion {
            core: Rect {
                lx: lo,
                ux: hi,
                ly: i64::MIN,
                uy: i64::MAX,
            },
            complete,
            span,
            routed_pins: &routed_pins,
        };
        let network = extract_region(&sub, limits, Some(&region))?;
        let s = &network.stats;
        let stripe = StripeStats {
            core: [lo, hi],
            nets: network.nets.len(),
            home_nets: s.nets,
            passive_shapes: sub_passive.len(),
            shapes: s.shapes,
            couplings: s.couplings,
            work: s.work,
            estimate_bytes: s.memory.map_or(0, |m| m.estimate_bytes),
            seconds: started.elapsed().as_secs_f64(),
        };
        Ok((network, stripe))
    };
    for chunk in cores.chunks(plan.parallel) {
        let done: Vec<Result<(RcNetwork, StripeStats)>> = if chunk.len() == 1 {
            vec![one(chunk[0].0, chunk[0].1)]
        } else {
            chunk.par_iter().map(|&(lo, hi)| one(lo, hi)).collect()
        };
        // Merged in stripe order; the first failing stripe's error.
        for result in done {
            let (network, stripe) = result?;
            if std::env::var_os("RUSTROAD_EXTRACT_TRACE").is_some() {
                eprintln!(
                    "[extract] stripe {}/{} core {:?} nets {} home {} shapes {} couplings {} \
                     work {} {:.1}s peak {} MiB",
                    partition.stripes.len() + 1,
                    cores.len(),
                    stripe.core,
                    stripe.nets,
                    stripe.home_nets,
                    stripe.shapes,
                    stripe.couplings,
                    stripe.work,
                    stripe.seconds,
                    crate::memory::peak_rss_bytes().unwrap_or(0) >> 20
                );
            }
            let s = &network.stats;
            if let Some(m) = s.memory
                && memory.is_none_or(|old| m.estimate_bytes > old.estimate_bytes)
            {
                memory = Some(m);
            }
            add_stats(&mut stats, s)?;
            partition.stripes.push(stripe);
            for rc in network.nets {
                let p = at(rc.net).ok_or_else(|| invalid("stripe net outside the input"))?;
                match &mut trees[p] {
                    slot @ None => *slot = Some(rc),
                    Some(tree) => {
                        if tree.nodes != rc.nodes || tree.resistors != rc.resistors {
                            return Err(invalid(&format!(
                                "stripes built different trees for net {}",
                                rc.net
                            )));
                        }
                        for (g, add) in tree.ground.iter_mut().zip(&rc.ground) {
                            *g += add;
                        }
                        partition.repeated_trees += 1;
                    }
                }
            }
            couplings
                .try_reserve(network.couplings.len())
                .map_err(|_| budget("merged coupling capacitors"))?;
            couplings.extend(network.couplings);
            if couplings.len() > plan.max_couplings.saturating_mul(2) {
                return Err(budget("merged extraction coupling capacitors"));
            }
        }
    }
    let mut nets = Vec::with_capacity(n);
    for (p, tree) in trees.into_iter().enumerate() {
        nets.push(tree.ok_or_else(|| {
            invalid(&format!(
                "routed net {} is in no extraction stripe",
                routes.nets[p]
            ))
        })?);
    }
    if stats.nets != n {
        return Err(invalid("extraction stripes do not count every net once"));
    }
    // One capacitor per endpoint pair, summed in stripe order (a stable
    // sort, merged in place).
    couplings.sort_by_key(|c| (c.a, c.b));
    couplings.dedup_by(|later, kept| {
        let same = (later.a, later.b) == (kept.a, kept.b);
        if same {
            kept.pf += later.pf;
        }
        same
    });
    let merged = couplings;
    stats.memory = memory;
    stats.couplings = merged.len();
    stats.coupling_pf = merged.iter().map(|c| c.pf).sum();
    stats.ground_pf = nets.iter().flat_map(|n| n.ground.iter()).sum();
    stats.partition = Some(partition);
    let elements = stats
        .resistors
        .checked_add(stats.nodes)
        .and_then(|e| e.checked_add(2 * stats.couplings))
        .ok_or_else(|| budget("extraction elements"))?;
    if stats.nodes > plan.max_nodes
        || stats.couplings > plan.max_couplings
        || elements > plan.max_elements
    {
        return Err(budget("merged extraction network exceeds its caps"));
    }
    Ok(RcNetwork {
        nets,
        couplings: merged,
        stats,
    })
}

/// Sums one stripe's statistics into the total; the largest candidate list
/// and shape universe are maxima. Network totals are recomputed after the
/// merge. Work includes each stripe's own preparation (pieces, crossings).
fn add_stats(total: &mut ExtractStats, s: &ExtractStats) -> Result<()> {
    let sum = |a: &mut usize, b: usize| -> Result<()> {
        *a = a
            .checked_add(b)
            .ok_or_else(|| budget("extraction statistics"))?;
        Ok(())
    };
    sum(&mut total.nets, s.nets)?;
    sum(&mut total.lattice_wires, s.lattice_wires)?;
    sum(&mut total.wire_runs, s.wire_runs)?;
    sum(&mut total.vias, s.vias)?;
    sum(&mut total.nodes, s.nodes)?;
    sum(&mut total.resistors, s.resistors)?;
    sum(&mut total.opened_loop_resistors, s.opened_loop_resistors)?;
    sum(&mut total.coincident_pin_links, s.coincident_pin_links)?;
    sum(&mut total.duplicate_wire_edges, s.duplicate_wire_edges)?;
    sum(&mut total.segment_cut_nodes, s.segment_cut_nodes)?;
    total.shapes = total.shapes.max(s.shapes);
    sum(&mut total.intervals, s.intervals)?;
    sum(&mut total.work, s.work)?;
    total.max_run_candidates = total.max_run_candidates.max(s.max_run_candidates);
    sum(
        &mut total.context_fallback_intervals,
        s.context_fallback_intervals,
    )?;
    sum(&mut total.foreign_overlaps, s.foreign_overlaps)?;
    sum(&mut total.unrouted_pin_partners, s.unrouted_pin_partners)?;
    sum(&mut total.patches, s.patches)?;
    sum(&mut total.patch_pieces, s.patch_pieces)?;
    sum(&mut total.end_pieces, s.end_pieces)?;
    sum(&mut total.pad_pieces, s.pad_pieces)?;
    sum(&mut total.covered_side_intervals, s.covered_side_intervals)?;
    sum(
        &mut total.nonspanning_plane_intervals,
        s.nonspanning_plane_intervals,
    )?;
    sum(&mut total.passive_strips, s.passive_strips)?;
    sum(&mut total.passive_intervals, s.passive_intervals)?;
    total.overlap_area_um2 += s.overlap_area_um2;
    sum(
        &mut total.overlap_pairs_without_data,
        s.overlap_pairs_without_data,
    )?;
    total.wire_length_dbu = total
        .wire_length_dbu
        .checked_add(s.wire_length_dbu)
        .ok_or_else(|| budget("extraction statistics"))?;
    total.resistance_ohm += s.resistance_ohm;
    Ok(())
}
