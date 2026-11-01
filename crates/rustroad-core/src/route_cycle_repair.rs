//! Bounded removal of a detail edge that closes a wide-metal routing cycle.
use super::{NetRoute, Plan, Router};
use crate::routing::{
    Access, Blockage, LayerRect, RouteInput, RouteLimits, Routes, Terminal, Via, Wire, budget,
    distance, shifted, wire_rect,
};
use crate::{Rect, Result};
use std::collections::{BTreeMap, HashMap};

/// Local graph and normalization scratch are capped independently of design size.
const MAX_RECORDS: usize = 8192;
/// Only a short stable prefix of local detail wires is tried.
const MAX_CANDIDATES: usize = 4;
/// Bound perpendicular junctions before normalization allocates either axis index.
const MAX_JUNCTION_PAIRS: usize = 8192;

struct Fixed<'a> {
    wires: Vec<&'a Wire>,
    vias: Vec<&'a Via>,
    access: Vec<&'a Access>,
}

fn charge(work: &mut usize, amount: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(amount)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("wide-cycle repair work"))?;
    Ok(())
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

fn shares_fixed_length(a: &Wire, b: &Wire) -> bool {
    if a.layer != b.layer {
        return false;
    }
    for axis in [0, 1] {
        let along = 1 - axis;
        if a.start[axis] == a.end[axis]
            && b.start[axis] == b.end[axis]
            && a.start[axis] == b.start[axis]
            && a.start[along]
                .min(a.end[along])
                .max(b.start[along].min(b.end[along]))
                < a.start[along]
                    .max(a.end[along])
                    .min(b.start[along].max(b.end[along]))
        {
            return true;
        }
    }
    false
}

fn connected_without(
    input: &RouteInput,
    limits: &RouteLimits,
    fixed: &Fixed<'_>,
    detail: &NetRoute,
    omitted: Option<usize>,
    work: &mut usize,
) -> Result<bool> {
    let mut wires = fixed.wires.clone();
    wires.extend(
        detail
            .wires
            .iter()
            .enumerate()
            .filter_map(|(i, w)| (Some(i) != omitted).then_some(w)),
    );
    let mut vias = fixed.vias.clone();
    vias.extend(detail.vias.iter());
    let mut axes = BTreeMap::<u32, (usize, usize)>::new();
    for wire in &wires {
        charge(work, 1, limits.max_comparisons)?;
        let counts = axes.entry(wire.layer).or_default();
        if wire.start[0] == wire.end[0] {
            counts.1 += 1;
        } else {
            counts.0 += 1;
        }
    }
    let pairs = axes
        .values()
        .fold(0usize, |n, &(h, v)| n.saturating_add(h.saturating_mul(v)));
    if pairs > MAX_JUNCTION_PAIRS {
        // This optional repair falls back to routing; its output cap alone
        // would not bound net_runs' intermediate crossing-point sets.
        return Ok(false);
    }
    charge(work, vias.len().saturating_mul(32), limits.max_comparisons)?;
    vias.sort_unstable_by_key(|v| (v.kind, v.at));
    vias.dedup();
    let via_layers: Vec<_> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
    let runs = crate::wire_runs::net_runs(
        &wires,
        &vias,
        &via_layers,
        &fixed.access,
        work,
        limits.max_comparisons,
        MAX_RECORDS
            .min(limits.max_segments)
            .saturating_sub(vias.len())
            .saturating_sub(fixed.access.len()),
    )?;
    let Some(first) = fixed.access.first() else {
        return Ok(false);
    };
    let local = Routes {
        wires: runs,
        vias: vias.iter().map(|v| (*v).clone()).collect(),
        access: fixed.access.iter().map(|a| (*a).clone()).collect(),
        patches: Vec::new(),
        nets: vec![first.net],
        stats: Default::default(),
    };
    let index = crate::route_graph::NetIndex::new(&local, limits, work)?;
    let graph = crate::route_graph::Graph::new(
        input,
        &local,
        index.sources(&local, first.net),
        limits,
        work,
    )?;
    graph.connected(work, limits.max_comparisons)
}

fn removable_wire(
    input: &RouteInput,
    limits: &RouteLimits,
    fixed: &Fixed<'_>,
    detail: &NetRoute,
    region: &LayerRect,
    work: &mut usize,
) -> Result<Option<usize>> {
    let count = fixed
        .wires
        .len()
        .saturating_add(fixed.vias.len())
        .saturating_add(fixed.access.len())
        .saturating_add(detail.segments());
    if count > MAX_RECORDS.min(limits.max_active_nodes) || fixed.access.is_empty() {
        return Ok(None);
    }
    charge(work, count.saturating_mul(32), limits.max_comparisons)?;
    let mut candidates = Vec::new();
    let vertical_region =
        region.rect.uy.abs_diff(region.rect.ly) >= region.rect.ux.abs_diff(region.rect.lx);
    for (i, wire) in detail.wires.iter().enumerate() {
        if wire.layer != region.layer
            || wire.start == wire.end
            || !overlaps(
                wire_rect(wire, input.layers[wire.layer as usize].width)?,
                region.rect,
            )
        {
            continue;
        }
        charge(work, fixed.wires.len(), limits.max_comparisons)?;
        if fixed
            .wires
            .iter()
            .any(|fixed| shares_fixed_length(wire, fixed))
        {
            continue;
        }
        // Prefer a short bar crossing the wide region over its long side.
        candidates.push((
            (wire.start[0] == wire.end[0]) == vertical_region,
            distance(wire.start, wire.end)?,
            i,
        ));
    }
    candidates.sort_unstable();
    candidates.truncate(MAX_CANDIDATES);
    if candidates.is_empty() || !connected_without(input, limits, fixed, detail, None, work)? {
        return Ok(None);
    }
    for (_, _, i) in candidates {
        if connected_without(input, limits, fixed, detail, Some(i), work)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

/// Fixed escapes stay untouched; only a proven redundant detail wire is removed.
pub(super) fn repair(
    router: &mut Router<'_>,
    terminals: &[&Terminal],
    plans: &HashMap<u32, Plan>,
    detail: &mut NetRoute,
    region: LayerRect,
) -> Result<bool> {
    if terminals.len() > MAX_RECORDS {
        return Ok(false);
    }
    charge(
        &mut router.spatial.work,
        terminals.len(),
        router.limits.max_comparisons,
    )?;
    let mut count = detail.segments();
    for terminal in terminals {
        let plan = &plans[&terminal.pin];
        count = count
            .saturating_add(plan.wires.len())
            .saturating_add(plan.vias.len())
            .saturating_add(1);
    }
    if count > MAX_RECORDS.min(router.limits.max_active_nodes) {
        return Ok(false);
    }
    let mut fixed = Fixed {
        wires: Vec::new(),
        vias: Vec::new(),
        access: Vec::new(),
    };
    for terminal in terminals {
        let plan = &plans[&terminal.pin];
        fixed.wires.extend(plan.wires.iter());
        fixed.vias.extend(plan.vias.iter());
        fixed.access.push(&plan.access);
    }
    let Some(index) = removable_wire(
        router.input,
        router.limits,
        &fixed,
        detail,
        &region,
        &mut router.spatial.work,
    )?
    else {
        return Ok(false);
    };
    let removed = detail.wires.remove(index);
    for id in detail.shapes.drain(..) {
        router.spatial.remove(id);
    }
    for wire in &detail.wires {
        detail.shapes.push(router.spatial.add_routed(Blockage {
            layer: wire.layer,
            rect: wire_rect(wire, router.input.layers[wire.layer as usize].width)?,
            net: Some(wire.net),
        })?);
    }
    for via in &detail.vias {
        for shape in &router.input.vias[via.kind as usize].shapes {
            detail.shapes.push(router.spatial.add_routed(Blockage {
                layer: shape.layer,
                rect: shifted(shape.rect, via.at)?,
                net: Some(via.net),
            })?);
        }
    }
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        eprintln!(
            "[troute] wide-cycle removed detail wire net {} layer {} {:?}->{:?}",
            removed.net, removed.layer, removed.start, removed.end
        );
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::RouteLayer;

    fn wire(start: [i64; 2], end: [i64; 2]) -> Wire {
        Wire {
            net: 0,
            layer: 0,
            start,
            end,
        }
    }
    fn fixture() -> RouteInput {
        RouteInput {
            die: Rect {
                lx: -20,
                ly: -20,
                ux: 120,
                uy: 120,
            },
            grid: 1,
            step: 20,
            access_layer: 0,
            layers: vec![RouteLayer {
                name: "M1".into(),
                width: 20,
                spacing: 20,
                vertical: Some(true),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            }],
            vias: Vec::new(),
            obstacles: Vec::new(),
            terminals: Vec::new(),
            rules: true,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }
    #[test]
    fn accepts_only_redundant_detail_edges_and_preserves_fixed_geometry() {
        let input = fixture();
        let limits = RouteLimits::default();
        let wires = [
            wire([0, 0], [0, 100]),
            wire([0, 0], [100, 0]),
            wire([100, 0], [100, 100]),
        ];
        let access = [
            Access {
                net: 0,
                pin: 0,
                layer: 0,
                at: [0, 100],
            },
            Access {
                net: 0,
                pin: 1,
                layer: 0,
                at: [100, 100],
            },
        ];
        let mut fixed = Fixed {
            wires: wires.iter().collect(),
            vias: Vec::new(),
            access: access.iter().collect(),
        };
        let detail = NetRoute {
            wires: vec![wire([0, 100], [100, 100])],
            ..Default::default()
        };
        let region = LayerRect {
            layer: 0,
            rect: Rect {
                lx: 0,
                ly: 90,
                ux: 100,
                uy: 110,
            },
        };
        assert_eq!(
            removable_wire(&input, &limits, &fixed, &detail, &region, &mut 0).unwrap(),
            Some(0),
            "the fourth side closes a cycle"
        );
        fixed.wires.remove(1);
        assert_eq!(
            removable_wire(&input, &limits, &fixed, &detail, &region, &mut 0).unwrap(),
            None,
            "the only connection between pins is indispensable"
        );
        fixed.wires.push(&detail.wires[0]);
        assert_eq!(
            removable_wire(&input, &limits, &fixed, &detail, &region, &mut 0).unwrap(),
            None,
            "fixed escape overlap is protected"
        );
        let capped = RouteLimits {
            max_comparisons: 1,
            ..limits
        };
        assert!(removable_wire(&input, &capped, &fixed, &detail, &region, &mut 0).is_err());
        assert_eq!(
            detail.wires.len(),
            1,
            "selection and a spent budget do not mutate detail"
        );
    }
    #[test]
    fn oversized_local_net_is_left_for_the_existing_repair_path() {
        let input = fixture();
        let limits = RouteLimits::default();
        let access = [Access {
            net: 0,
            pin: 0,
            layer: 0,
            at: [0, 0],
        }];
        let fixed = Fixed {
            wires: Vec::new(),
            vias: Vec::new(),
            access: access.iter().collect(),
        };
        let detail = NetRoute {
            wires: vec![wire([0, 0], [100, 0]); MAX_RECORDS],
            ..Default::default()
        };
        let region = LayerRect {
            layer: 0,
            rect: input.die,
        };
        assert_eq!(
            removable_wire(&input, &limits, &fixed, &detail, &region, &mut 0).unwrap(),
            None
        );
    }

    #[test]
    fn dense_crossings_fall_back_before_normalization_materializes_them() {
        // 4,096 perpendicular input wires fit the record cap, but would form
        // over four million junctions. The repair must reject before allocation.
        let input = fixture();
        let limits = RouteLimits::default();
        let access = Access {
            net: 0,
            pin: 0,
            layer: 0,
            at: [0, 0],
        };
        let fixed = Fixed {
            wires: Vec::new(),
            vias: Vec::new(),
            access: vec![&access],
        };
        let mut detail = NetRoute::default();
        for n in 0..2048 {
            detail.wires.push(wire([0, n], [2047, n]));
            detail.wires.push(wire([n, 0], [n, 2047]));
        }
        let mut work = 0;
        assert!(!connected_without(&input, &limits, &fixed, &detail, None, &mut work).unwrap());
        assert_eq!(work, detail.wires.len(), "only the bounded input scan ran");
        assert_eq!(
            detail.wires.len(),
            4096,
            "fallback leaves geometry unchanged"
        );
    }
}
