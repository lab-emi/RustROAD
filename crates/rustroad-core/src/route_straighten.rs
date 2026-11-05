//! Post-route topology cleanup of the track router (`RouteQor::straighten`;
//! the contract is on `routing::Straighten`).
//!
//! A *chain* is a maximal sequence of one net's routed runs on one layer
//! joined at plain corners: points where exactly two of its runs end and
//! nothing else of the net is (no via landing on that layer, no pin access
//! point, no escape wire, no pin shape, no other run passing through). The
//! chain's two end points are junctions and stay where they are. A chain of
//! three or more runs is rewritten as the first candidate with fewer runs
//! that passes every check below; the rest of the net, and every via, is
//! untouched.
//!
//! Candidates (`candidates`), fewest runs first: the straight run when the
//! junctions are aligned, the two Ls, the Zs whose middle leg lies on one of
//! the chain's corner lines; then, once, the minimum-bend shortest path over
//! the layer's track grid in the junctions' box (`Router::min_bend_path`).
//! Without `turn_at_ends` a candidate leaves each junction along the axis the
//! chain used there.
//!
//! Every candidate is a shortest rectilinear path between the junctions
//! (monotone), so its length is at most the chain's. A candidate is kept
//! only if each new run (1) is clear of every other net's metal, fixed
//! geometry and cell obstructions (`Spatial::probe`, as the search tests a
//! wire step), (2) keeps the wide-metal spacing to every shape of another
//! net that is wide on its own (`Spatial::wide_clear`), (3) touches no
//! same-net metal except what the chain's own end run touched at that end
//! junction (no new branch, no loop through the net, no further overlap of
//! its own pins), and (4) the net's same-net rule check on that layer
//! (`route_rules::check_group`: notches, necks, holes, minimum area) reports
//! no violation that the layer did not already have. The rewritten net must
//! pass `validate`'s endpoint connectivity check, else it keeps its route.
use super::{
    Access, Blockage, BuildFx, NetRoute, Outcome, Plan, PriorRoutes, Router, Terminal, Via, Wire,
    budget, discard_route, expand, invalid, on_wire, remove_route, retain_route, routed_size,
    shifted, trace, wire_rect,
};
use crate::route_rules;
use crate::{Rect, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Nets with more routed runs than this keep their route (the chain scan
/// and the per-layer rule check are quadratic in the worst case).
const MAX_NET_RUNS: usize = 4096;
/// Track-grid nodes of the largest box the minimum-bend search covers.
const MAX_BOX_NODES: usize = 4096;

/// Totals of one straightening run, summed over its passes (the
/// `straighten` trace lines; unit tests read them through `TALLY`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StraightenTally {
    /// Chains of at least three runs examined.
    pub chains: usize,
    /// Chains rewritten.
    pub kept: usize,
    /// Nets with at least one rewritten chain.
    pub nets: usize,
    /// Runs removed (chain runs minus new runs, summed).
    pub runs_removed: usize,
    /// Wire length removed (DBU).
    pub length_removed: u64,
    /// Candidates refused by the clearance probe (other nets, fixed metal).
    pub blocked: usize,
    /// Candidates clear at the normal spacing but refused by the wide-metal
    /// spacing of a wide shape of another net (`Spatial::wide_clear`).
    pub wide: usize,
    /// Candidates refused for same-net contact (branch, loop, pin overlap).
    pub contact: usize,
    /// Candidates refused by the same-net rule check.
    pub rules: usize,
    /// Nets whose rewritten geometry failed the connectivity self-check
    /// (kept as they were).
    pub disconnected: usize,
    /// Minimum-bend searches run, and paths they found.
    pub searched: usize,
    pub found: usize,
    /// Bend-priced re-routes tried, kept, capped and failed.
    pub rerouted: usize,
    pub reroute_kept: usize,
    pub reroute_limited: usize,
    pub reroute_failed: usize,
    /// Runs removed by kept re-routes.
    pub reroute_runs: usize,
    /// Nets skipped: still holding their ECO prior geometry, or too large.
    pub skipped: usize,
}

#[cfg(test)]
thread_local! {
    /// The tally of the last straightening run on this thread (tests only).
    pub(super) static TALLY: std::cell::Cell<Option<StraightenTally>> =
        const { std::cell::Cell::new(None) };
}

/// One chain: its run indices in order from junction `points[0]` to
/// junction `points[last]`, and the polyline through its corners.
pub(super) struct Chain {
    pub wires: Vec<usize>,
    pub points: Vec<[i64; 2]>,
    pub layer: u32,
}

fn vertical(a: [i64; 2], b: [i64; 2]) -> bool {
    a[0] == b[0]
}

fn length(points: &[[i64; 2]]) -> u64 {
    points
        .windows(2)
        .map(|w| w[0][0].abs_diff(w[1][0]) + w[0][1].abs_diff(w[1][1]))
        .sum()
}

/// Whether the centre lines of two axis-parallel wires share a point other
/// than an end point of `a` (a crossing or touch inside `a`, or a collinear
/// overlap of positive length, which always covers inner points of `a`).
fn meets_inside(a: &Wire, b: &Wire) -> bool {
    let lo = |w: &Wire, k: usize| w.start[k].min(w.end[k]);
    let hi = |w: &Wire, k: usize| w.start[k].max(w.end[k]);
    let x = (lo(a, 0).max(lo(b, 0)), hi(a, 0).min(hi(b, 0)));
    let y = (lo(a, 1).max(lo(b, 1)), hi(a, 1).min(hi(b, 1)));
    if x.0 > x.1 || y.0 > y.1 {
        return false;
    }
    if x.0 < x.1 || y.0 < y.1 {
        return true;
    }
    let q = [x.0, y.0];
    q != a.start && q != a.end
}

fn charge(work: &mut usize, n: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(n)
        .filter(|&w| w <= cap)
        .ok_or_else(|| budget("straightening work"))?;
    Ok(())
}

/// Closed rectangles share at least one point.
fn touch(a: Rect, b: Rect) -> bool {
    a.lx <= b.ux && b.lx <= a.ux && a.ly <= b.uy && b.ly <= a.uy
}

fn clip(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect {
        lx: a.lx.max(b.lx),
        ly: a.ly.max(b.ly),
        ux: a.ux.min(b.ux),
        uy: a.uy.min(b.uy),
    };
    (r.lx <= r.ux && r.ly <= r.uy).then_some(r)
}

fn inside(a: Rect, b: Rect) -> bool {
    b.lx <= a.lx && a.ux <= b.ux && b.ly <= a.ly && a.uy <= b.uy
}

/// Fixed same-net geometry of one net: pin escapes and pin shapes.
pub(super) struct Fixed<'p> {
    pub plan_wires: Vec<&'p Wire>,
    pub plan_vias: Vec<&'p Via>,
    pub access: Vec<&'p Access>,
    pub pins: Vec<(u32, Rect)>,
}

/// The two end junctions of a chain with the rectangle and axis
/// (`true` = along y) of the chain run at each.
struct End {
    a: [i64; 2],
    b: [i64; 2],
    first: (Rect, bool),
    last: (Rect, bool),
}

/// Same-net metal on a chain's layer (other than the chain; `true` marks a
/// pin shape) and which of it the chain's end runs touch at `a` and `b`.
struct Near<'o> {
    others: &'o [(Rect, bool)],
    at_a: &'o [bool],
    at_b: &'o [bool],
}

/// The fewest runs any path between the chain's junctions can have: one when
/// they are aligned (and, without `turn`, both end axes are that line's),
/// else two (an L), or three without `turn` when both ends share an axis.
fn fewest_runs(chain: &Chain, old: &[Wire], turn: bool) -> usize {
    let first = &old[chain.wires[0]];
    let last = &old[*chain.wires.last().unwrap_or(&chain.wires[0])];
    let (a0, a1) = (
        vertical(first.start, first.end),
        vertical(last.start, last.end),
    );
    let a = chain.points[0];
    let b = *chain.points.last().unwrap_or(&a);
    let aligned_v = a[0] == b[0];
    let aligned_h = a[1] == b[1];
    if turn {
        if aligned_v || aligned_h { 1 } else { 2 }
    } else if (aligned_v && a0 && a1) || (aligned_h && !a0 && !a1) {
        1
    } else if a0 != a1 {
        2
    } else {
        3
    }
}

/// Candidate polylines for `chain` (see the module docs), in order: fewest
/// runs first, then the fewest end junctions left along another axis than
/// the chain's, then generation order (straight, the two Ls, the Zs on the
/// chain's corner lines in chain order). Without `turn` only candidates
/// that keep both end axes are returned. Every candidate is monotone, so it
/// is never longer than the chain, and has fewer runs than the chain.
fn candidates(chain: &Chain, old: &[Wire], turn: bool) -> Vec<Vec<[i64; 2]>> {
    let first = &old[chain.wires[0]];
    let last = &old[*chain.wires.last().unwrap_or(&chain.wires[0])];
    let (a0, a1) = (
        vertical(first.start, first.end),
        vertical(last.start, last.end),
    );
    let a = chain.points[0];
    let b = *chain.points.last().unwrap_or(&a);
    let runs = chain.wires.len();
    if a == b {
        // A loop back to its own junction: left alone.
        return Vec::new();
    }
    let mut all: Vec<Vec<[i64; 2]>> = Vec::new();
    if a[0] == b[0] || a[1] == b[1] {
        all.push(vec![a, b]);
    } else {
        all.push(vec![a, [a[0], b[1]], b]);
        all.push(vec![a, [b[0], a[1]], b]);
        // Z shapes: the middle leg on a corner line strictly between the ends.
        let mut seen = HashSet::with_hasher(BuildFx);
        for p in &chain.points[1..chain.points.len() - 1] {
            for along in [1usize, 0] {
                let (lo, hi) = (a[along].min(b[along]), a[along].max(b[along]));
                let m = p[along];
                if m <= lo || m >= hi || !seen.insert((along, m)) {
                    continue;
                }
                let mut c1 = a;
                c1[along] = m;
                let mut c2 = b;
                c2[along] = m;
                all.push(vec![a, c1, c2, b]);
            }
        }
    }
    let changes = |c: &Vec<[i64; 2]>| {
        let n = c.len();
        usize::from(vertical(c[0], c[1]) != a0) + usize::from(vertical(c[n - 2], c[n - 1]) != a1)
    };
    let mut out: Vec<(usize, usize, Vec<[i64; 2]>)> = all
        .into_iter()
        .filter(|c| c.len() - 1 < runs)
        .map(|c| (c.len(), changes(&c), c))
        .filter(|(_, ch, _)| turn || *ch == 0)
        .collect();
    // Stable: generation order breaks ties.
    out.sort_by_key(|(n, ch, _)| (*n, *ch));
    out.into_iter().map(|(_, _, c)| c).collect()
}

impl Router<'_> {
    /// The chains of one net's runs with at least three runs (module docs).
    pub(super) fn chains(
        &mut self,
        wires: &[Wire],
        vias: &[Via],
        fixed: &Fixed<'_>,
    ) -> Result<Vec<Chain>> {
        let cap = self.limits.max_comparisons;
        let n = wires.len();
        charge(
            &mut self.spatial.work,
            n.saturating_mul(n.max(1).ilog2() as usize + 4),
            cap,
        )?;
        // Junction points per layer that are not plain corners on their own.
        let mut marked: HashSet<(u32, [i64; 2]), BuildFx> = HashSet::with_hasher(BuildFx);
        for v in vias.iter().chain(fixed.plan_vias.iter().copied()) {
            let k = &self.input.vias[v.kind as usize];
            marked.insert((k.lower, v.at));
            marked.insert((k.upper, v.at));
        }
        for a in &fixed.access {
            marked.insert((a.layer, a.at));
        }
        let mut ends: HashMap<(u32, [i64; 2]), Vec<usize>, BuildFx> = HashMap::with_hasher(BuildFx);
        for (i, w) in wires.iter().enumerate() {
            ends.entry((w.layer, w.start)).or_default().push(i);
            ends.entry((w.layer, w.end)).or_default().push(i);
        }
        // Wires by layer, for the pass-through test.
        let mut by_layer: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (i, w) in wires.iter().enumerate() {
            by_layer.entry(w.layer).or_default().push(i);
        }
        let mut plain: HashMap<(u32, [i64; 2]), [usize; 2], BuildFx> =
            HashMap::with_hasher(BuildFx);
        for (&(layer, p), list) in &ends {
            if list.len() != 2 || marked.contains(&(layer, p)) {
                continue;
            }
            charge(
                &mut self.spatial.work,
                by_layer[&layer].len() + fixed.plan_wires.len() + fixed.pins.len(),
                cap,
            )?;
            let through = by_layer[&layer].iter().any(|&j| {
                let w = &wires[j];
                on_wire(w, p) && w.start != p && w.end != p
            }) || fixed
                .plan_wires
                .iter()
                .any(|w| w.layer == layer && on_wire(w, p))
                || fixed
                    .pins
                    .iter()
                    .any(|&(l, r)| l == layer && super::contains(r, p));
            if !through {
                plain.insert((layer, p), [list[0], list[1]]);
            }
        }
        // Runs that meet fixed same-net geometry away from their own ends
        // (an escape wire ending on or crossing the run, an escape via or a
        // pin access point inside it) join that geometry there; the run is
        // not split at such a point (`collinear_runs` sees only the route's
        // own records), so a chain through it is left alone.
        charge(
            &mut self.spatial.work,
            n.saturating_mul(fixed.plan_wires.len() + fixed.plan_vias.len() + fixed.access.len()),
            cap,
        )?;
        let inner = |w: &Wire, p: [i64; 2]| on_wire(w, p) && p != w.start && p != w.end;
        let pinned: Vec<bool> = wires
            .iter()
            .map(|w| {
                fixed
                    .plan_wires
                    .iter()
                    .any(|f| f.layer == w.layer && meets_inside(w, f))
                    || fixed.plan_vias.iter().any(|v| {
                        let k = &self.input.vias[v.kind as usize];
                        (k.lower == w.layer || k.upper == w.layer) && inner(w, v.at)
                    })
                    || fixed
                        .access
                        .iter()
                        .any(|a| a.layer == w.layer && inner(w, a.at))
            })
            .collect();
        let other_end = |w: &Wire, p: [i64; 2]| if w.start == p { w.end } else { w.start };
        let mut visited = vec![false; n];
        let mut out = Vec::new();
        for i in 0..n {
            if visited[i] {
                continue;
            }
            visited[i] = true;
            let layer = wires[i].layer;
            // Walk forward from `end`, then backward from `start`.
            let mut forward = vec![(i, wires[i].start, wires[i].end)];
            let mut cycle = false;
            let mut p = wires[i].end;
            let mut cur = i;
            while let Some(pair) = plain.get(&(layer, p)) {
                let next = if pair[0] == cur { pair[1] } else { pair[0] };
                if next == i {
                    cycle = true;
                    break;
                }
                if visited[next] {
                    // Not reachable (plain points join exactly two runs);
                    // such a chain is left alone.
                    cycle = true;
                    break;
                }
                visited[next] = true;
                let q = other_end(&wires[next], p);
                forward.push((next, p, q));
                cur = next;
                p = q;
            }
            let mut backward = Vec::new();
            if !cycle {
                let mut p = wires[i].start;
                let mut cur = i;
                while let Some(pair) = plain.get(&(layer, p)) {
                    let next = if pair[0] == cur { pair[1] } else { pair[0] };
                    if visited[next] {
                        cycle = true;
                        break;
                    }
                    visited[next] = true;
                    let q = other_end(&wires[next], p);
                    backward.push((next, q, p));
                    cur = next;
                    p = q;
                }
            }
            if cycle {
                continue;
            }
            backward.reverse();
            backward.extend(forward);
            if backward.len() < 3 || backward.iter().any(|s| pinned[s.0]) {
                continue;
            }
            // Consecutive runs of a chain are perpendicular (collinear runs
            // that meet at a plain point would be one run).
            if backward.windows(2).any(|w| {
                let (a, b) = (&wires[w[0].0], &wires[w[1].0]);
                vertical(a.start, a.end) == vertical(b.start, b.end)
            }) {
                continue;
            }
            let mut points = vec![backward[0].1];
            points.extend(backward.iter().map(|s| s.2));
            out.push(Chain {
                wires: backward.iter().map(|s| s.0).collect(),
                points,
                layer,
            });
        }
        Ok(out)
    }

    /// The fewest-run shortest path on `layer` from `ends.a` to `ends.b`
    /// over the layer's track grid (tracks and stops) inside their bounding
    /// box, every unit step clear of other nets and fixed metal (and of the
    /// wide-metal spacing of wide shapes) and touching same-net metal only at the end
    /// junctions as the chain did. Monotone paths are exactly the shortest
    /// ones, so a dynamic programme over the box (nodes in increasing
    /// distance from `a`, arrival axis as state) gives the minimum-bend
    /// shortest path; ties prefer continuing straight, then horizontal
    /// arrival. Without `turn` the path leaves and arrives along the
    /// chain's end axes, with it a changed end axis costs less than a bend.
    /// `None` when no such path exists or the box exceeds `MAX_BOX_NODES`.
    #[allow(clippy::too_many_arguments)]
    fn min_bend_path(
        &mut self,
        net: u32,
        layer: u32,
        wide: Option<crate::routing::WideSpacing>,
        ends: &End,
        near: &Near<'_>,
        turn: bool,
        tally: &mut StraightenTally,
    ) -> Result<Option<Vec<[i64; 2]>>> {
        let cap = self.limits.max_comparisons;
        let (a, b) = (ends.a, ends.b);
        let Some(&slot) = self.graph.slot.get(&layer) else {
            return Ok(None);
        };
        let gl = &self.graph.layers[slot];
        // Coordinates along x and y from `a` toward `b`.
        let (xs_all, ys_all) = if gl.vertical {
            (&gl.tracks, &gl.stops)
        } else {
            (&gl.stops, &gl.tracks)
        };
        let axis = |all: &[i64], from: i64, to: i64| -> Option<Vec<i64>> {
            let (lo, hi) = (from.min(to), from.max(to));
            let start = all.partition_point(|&v| v < lo);
            let end = all.partition_point(|&v| v <= hi);
            let mut v: Vec<i64> = all[start..end].to_vec();
            if v.first() != Some(&lo) || v.last() != Some(&hi) {
                return None;
            }
            if from > to {
                v.reverse();
            }
            Some(v)
        };
        let (Some(xs), Some(ys)) = (axis(xs_all, a[0], b[0]), axis(ys_all, a[1], b[1])) else {
            return Ok(None);
        };
        let (nx, ny) = (xs.len(), ys.len());
        if nx.saturating_mul(ny) > MAX_BOX_NODES || nx * ny < 2 {
            return Ok(None);
        }
        tally.searched += 1;
        let width = self.input.layers[layer as usize].width;
        // Same-net shapes near the box only.
        let bbox = expand(
            Rect {
                lx: a[0].min(b[0]),
                ly: a[1].min(b[1]),
                ux: a[0].max(b[0]),
                uy: a[1].max(b[1]),
            },
            width,
        )?;
        let local: Vec<usize> = (0..near.others.len())
            .filter(|&k| touch(near.others[k].0, bbox))
            .collect();
        charge(&mut self.spatial.work, nx * ny * 2 * (local.len() + 1), cap)?;
        // Legality of the unit step into node (i, j) along x (h) or y.
        let step_ok = |r: &mut Router<'_>, i: usize, j: usize, h: bool| -> Result<bool> {
            let p = [xs[i], ys[j]];
            let q = if h {
                [xs[i - 1], ys[j]]
            } else {
                [xs[i], ys[j - 1]]
            };
            let w = Wire {
                net,
                layer,
                start: q,
                end: p,
            };
            let rect = wire_rect(&w, width)?;
            if r.spatial.probe(layer, rect, net)?.is_none() {
                return Ok(false);
            }
            if let Some(w) = wide
                && !r.spatial.wide_clear(layer, rect, net, w)?
            {
                return Ok(false);
            }
            // A step on the junctions' own row or column (index 0 at `a`,
            // the last at `b`, along the step's axis) lies on the straight
            // first or last leg of every monotone path that uses it, where
            // the chain's end run touched the junction's metal too.
            let at_start = if h { j == 0 } else { i == 0 };
            let at_end = if h { j == ny - 1 } else { i == nx - 1 };
            for &k in &local {
                let (shape, pin) = near.others[k];
                if !touch(rect, shape) {
                    continue;
                }
                let end_ok = |old_rect: Rect| {
                    !pin || clip(rect, shape)
                        .is_some_and(|c| clip(old_rect, shape).is_some_and(|o| inside(c, o)))
                };
                let ok = (at_start && near.at_a[k] && end_ok(ends.first.0))
                    || (at_end && near.at_b[k] && end_ok(ends.last.0));
                if !ok {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        const BEND: u32 = 4;
        const CHANGE: u32 = 1;
        const INF: u32 = u32::MAX;
        // cost[(j * nx + i) * 2 + d]: d = 0 arrived along x, 1 along y;
        // from[..] = the arrival axis at the previous node.
        let mut cost = vec![INF; nx * ny * 2];
        let mut from = vec![0u8; nx * ny * 2];
        let start = |d: usize| -> u32 {
            let along_y = d == 1;
            if along_y == ends.first.1 {
                0
            } else if turn {
                CHANGE
            } else {
                INF
            }
        };
        for j in 0..ny {
            for i in 0..nx {
                if i == 0 && j == 0 {
                    continue;
                }
                for d in 0..2usize {
                    let (pi, pj) = if d == 0 {
                        if i == 0 {
                            continue;
                        }
                        (i - 1, j)
                    } else {
                        if j == 0 {
                            continue;
                        }
                        (i, j - 1)
                    };
                    let here = (j * nx + i) * 2 + d;
                    let prev = (pj * nx + pi) * 2;
                    let best = if pi == 0 && pj == 0 {
                        Some((start(d), d as u8))
                    } else {
                        let same = cost[prev + d];
                        let other = cost[prev + (1 - d)].saturating_add(BEND);
                        match (same, other) {
                            (INF, INF) => None,
                            (s, o) if s <= o => Some((s, d as u8)),
                            (_, o) => Some((o, (1 - d) as u8)),
                        }
                    };
                    let Some((c, f)) = best else {
                        continue;
                    };
                    if c == INF || !step_ok(self, i, j, d == 0)? {
                        continue;
                    }
                    cost[here] = c;
                    from[here] = f;
                }
            }
        }
        let last = ((ny - 1) * nx + (nx - 1)) * 2;
        let end = |d: usize| -> u32 {
            let along_y = d == 1;
            if along_y == ends.last.1 {
                0
            } else if turn {
                CHANGE
            } else {
                INF
            }
        };
        let mut best: Option<(u32, usize)> = None;
        for d in 0..2usize {
            let c = cost[last + d];
            if c == INF || end(d) == INF {
                continue;
            }
            let total = c + end(d);
            if best.is_none_or(|(b, _)| total < b) {
                best = Some((total, d));
            }
        }
        let Some((_, mut d)) = best else {
            return Ok(None);
        };
        // Walk back to `a`, keeping the corners.
        let (mut i, mut j) = (nx - 1, ny - 1);
        let mut path = vec![b];
        while i != 0 || j != 0 {
            let f = from[(j * nx + i) * 2 + d] as usize;
            if d == 0 {
                i -= 1;
            } else {
                j -= 1;
            }
            if (i != 0 || j != 0) && f != d {
                path.push([xs[i], ys[j]]);
            }
            d = f;
        }
        path.push(a);
        path.reverse();
        tally.found += 1;
        Ok(Some(path))
    }

    /// Whether the net's output geometry for route `wires` and `vias` (with
    /// its escapes, split into runs at every junction as `assemble` writes
    /// it) is one connected endpoint graph, as `routing::validate` checks.
    fn net_connected(
        &mut self,
        net: u32,
        wires: &[Wire],
        vias: &[Via],
        fixed: &Fixed<'_>,
    ) -> Result<bool> {
        let input = self.input;
        let cap = self.limits.max_comparisons;
        let wire_refs: Vec<&Wire> = fixed
            .plan_wires
            .iter()
            .copied()
            .chain(wires.iter())
            .collect();
        let mut via_refs: Vec<&Via> = fixed.plan_vias.iter().copied().chain(vias.iter()).collect();
        via_refs.sort_unstable_by_key(|v| (v.kind, v.at));
        via_refs.dedup();
        let pairs: Vec<(u32, u32)> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
        let runs = crate::wire_runs::net_runs(
            &wire_refs,
            &via_refs,
            &pairs,
            &fixed.access,
            &mut self.spatial.work,
            cap,
            self.limits.max_segments,
        )?;
        let mini = crate::routing::Routes {
            wires: runs,
            vias: via_refs.into_iter().cloned().collect(),
            access: fixed.access.iter().map(|a| (*a).clone()).collect(),
            patches: Vec::new(),
            nets: vec![net],
            stats: crate::routing::RouteStats::default(),
        };
        let index = crate::route_graph::NetIndex::new(&mini, self.limits, &mut self.spatial.work)?;
        let graph = crate::route_graph::Graph::new(
            input,
            &mini,
            index.sources(&mini, net),
            self.limits,
            &mut self.spatial.work,
        )?;
        graph.connected(&mut self.spatial.work, cap)
    }

    /// Same-net rule violations of `net` on `layer` for `wires` (all of the
    /// net's runs on that layer and its escape wires), its vias and pins.
    fn layer_violations(
        &mut self,
        net: u32,
        layer: u32,
        wires: &[&Wire],
        vias: &[&Via],
        terminals: &[&Terminal],
    ) -> Result<Vec<String>> {
        let input = self.input;
        let on_layer: Vec<&Via> = vias
            .iter()
            .copied()
            .filter(|v| {
                let k = &input.vias[v.kind as usize];
                k.lower == layer || k.upper == layer
            })
            .collect();
        charge(
            &mut self.spatial.work,
            wires.len() + on_layer.len() * 3 + 16,
            self.limits.max_comparisons,
        )?;
        let groups = route_rules::collect_parts(
            input,
            wires.iter().copied().filter(|w| w.layer == layer),
            on_layer.iter().copied(),
            std::iter::empty(),
            terminals.iter().copied(),
            &[net],
        )?;
        let mut out = Vec::new();
        if let Some(shapes) = groups.get(&(net, layer)) {
            let mut diagnostic = 0usize;
            route_rules::check_group(
                input,
                (net, layer),
                shapes,
                self.limits,
                &mut self.spatial.work,
                &mut out,
                &mut diagnostic,
            )?;
        }
        let mut text: Vec<String> = out.iter().map(|v| v.describe()).collect();
        text.sort_unstable();
        Ok(text)
    }

    /// Straighten one net's chains. The clearance probes ignore same-net
    /// metal, so the old route stays in the index while candidates are
    /// tested; on `Ok(Some(route))` its shapes have been replaced by the new
    /// route's, on `Ok(None)` nothing changed.
    fn straighten_net(
        &mut self,
        net: u32,
        old: &NetRoute,
        terminals: &[&Terminal],
        plans: &HashMap<u32, Plan>,
        turn: bool,
        tally: &mut StraightenTally,
    ) -> Result<Option<NetRoute>> {
        let input = self.input;
        let cap = self.limits.max_comparisons;
        let fixed = fixed_of(terminals, plans);
        let chains = self.chains(&old.wires, &old.vias, &fixed)?;
        if chains.is_empty() {
            return Ok(None);
        }
        let all_vias: Vec<&Via> = old
            .vias
            .iter()
            .chain(fixed.plan_vias.iter().copied())
            .collect();
        let mut wires: Vec<Option<Wire>> = old.wires.iter().cloned().map(Some).collect();
        let mut before: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        let mut changed = false;
        for chain in &chains {
            tally.chains += 1;
            let layer = chain.layer;
            let rule = &input.layers[layer as usize];
            let width = rule.width;
            let wide = rule.wide_spacing;
            let first_wire = &old.wires[chain.wires[0]];
            let last_wire = &old.wires[*chain.wires.last().unwrap()];
            let first_rect = wire_rect(first_wire, width)?;
            let last_rect = wire_rect(last_wire, width)?;
            let (a, b) = (chain.points[0], *chain.points.last().unwrap());
            // Same-net metal on this layer other than the chain itself.
            let mut others: Vec<(Rect, bool)> = Vec::new();
            for w in wires.iter().enumerate().filter_map(|(i, w)| {
                w.as_ref()
                    .filter(|w| w.layer == layer && !chain.wires.contains(&i))
            }) {
                others.push((wire_rect(w, width)?, false));
            }
            for w in fixed.plan_wires.iter().filter(|w| w.layer == layer) {
                others.push((wire_rect(w, width)?, false));
            }
            for v in &all_vias {
                for s in input.vias[v.kind as usize]
                    .shapes
                    .iter()
                    .filter(|s| s.layer == layer)
                {
                    others.push((shifted(s.rect, v.at)?, false));
                }
            }
            for &(l, r) in &fixed.pins {
                if l == layer {
                    others.push((r, true));
                }
            }
            // What the chain's end runs touch at their end junctions.
            let at_a: Vec<bool> = others
                .iter()
                .map(|&(r, _)| touch(r, first_rect) && super::contains(r, a))
                .collect();
            let at_b: Vec<bool> = others
                .iter()
                .map(|&(r, _)| touch(r, last_rect) && super::contains(r, b))
                .collect();
            let old_len = length(&chain.points);
            let mut list = candidates(chain, &old.wires, turn);
            let mut searched = false;
            let mut next = 0usize;
            loop {
                if next == list.len() {
                    // Explicit shapes exhausted: the fewest-run monotone path
                    // over the layer's track grid (once per chain), unless no
                    // path between these junctions can have fewer runs.
                    if searched || chain.wires.len() <= fewest_runs(chain, &old.wires, turn) {
                        break;
                    }
                    searched = true;
                    let ends = End {
                        a,
                        b,
                        first: (first_rect, vertical(first_wire.start, first_wire.end)),
                        last: (last_rect, vertical(last_wire.start, last_wire.end)),
                    };
                    let near = Near {
                        others: &others,
                        at_a: &at_a,
                        at_b: &at_b,
                    };
                    match self.min_bend_path(net, layer, wide, &ends, &near, turn, tally)? {
                        Some(path) if !list.contains(&path) => list.push(path),
                        _ => break,
                    }
                }
                let cand = list[next].clone();
                next += 1;
                if cand.len() > chain.wires.len() || length(&cand) > old_len {
                    continue;
                }
                let legs: Vec<Wire> = cand
                    .windows(2)
                    .map(|p| Wire {
                        net,
                        layer,
                        start: p[0],
                        end: p[1],
                    })
                    .collect();
                let rects: Vec<Rect> = legs
                    .iter()
                    .map(|w| wire_rect(w, width))
                    .collect::<Result<_>>()?;
                // (1, 2) Other nets, fixed metal, obstructions, wide metal.
                let mut clear = true;
                for &rect in &rects {
                    if self.spatial.probe(layer, rect, net)?.is_none() {
                        clear = false;
                        tally.blocked += 1;
                        break;
                    }
                    if let Some(wide) = wide
                        && !self.spatial.wide_clear(layer, rect, net, wide)?
                    {
                        clear = false;
                        tally.wide += 1;
                        break;
                    }
                }
                if !clear {
                    continue;
                }
                // (3) Same-net contact only where the end runs had it.
                charge(
                    &mut self.spatial.work,
                    rects.len().saturating_mul(others.len() + rects.len()),
                    cap,
                )?;
                let last = rects.len() - 1;
                let mut contact_ok = true;
                'legs: for (i, &rect) in rects.iter().enumerate() {
                    for (j, &(r, pin)) in others.iter().enumerate() {
                        if !touch(rect, r) {
                            continue;
                        }
                        let end_ok = |old_rect: Rect| {
                            !pin || clip(rect, r)
                                .is_some_and(|c| clip(old_rect, r).is_some_and(|o| inside(c, o)))
                        };
                        let ok = (i == 0 && at_a[j] && end_ok(first_rect))
                            || (i == last && at_b[j] && end_ok(last_rect));
                        if !ok {
                            contact_ok = false;
                            break 'legs;
                        }
                    }
                    for (k, &other) in rects.iter().enumerate() {
                        if k + 1 < i && touch(rect, other) {
                            contact_ok = false;
                            break 'legs;
                        }
                    }
                }
                if !contact_ok {
                    tally.contact += 1;
                    continue;
                }
                // (4) No new same-net rule violation on this layer.
                let plan_wires = &fixed.plan_wires;
                let current: Vec<&Wire> = wires
                    .iter()
                    .flatten()
                    .chain(plan_wires.iter().copied())
                    .collect();
                if let std::collections::btree_map::Entry::Vacant(slot) = before.entry(layer) {
                    slot.insert(self.layer_violations(net, layer, &current, &all_vias, terminals)?);
                }
                let trial: Vec<&Wire> = wires
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !chain.wires.contains(i))
                    .filter_map(|(_, w)| w.as_ref())
                    .chain(legs.iter())
                    .chain(plan_wires.iter().copied())
                    .collect();
                let after = self.layer_violations(net, layer, &trial, &all_vias, terminals)?;
                let old_set = &before[&layer];
                if after.iter().any(|v| old_set.binary_search(v).is_err()) {
                    tally.rules += 1;
                    continue;
                }
                before.insert(layer, after);
                tally.kept += 1;
                tally.runs_removed += chain.wires.len() - legs.len();
                tally.length_removed += old_len - length(&cand);
                for &i in &chain.wires {
                    wires[i] = None;
                }
                wires.extend(legs.into_iter().map(Some));
                changed = true;
                break;
            }
        }
        if !changed {
            return Ok(None);
        }
        let kept: Vec<Wire> = wires.into_iter().flatten().collect();
        let pairs: Vec<(u32, u32)> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
        let runs = crate::wire_runs::collinear_runs(
            &kept.iter().collect::<Vec<_>>(),
            &all_vias,
            &pairs,
            &fixed.access,
            cap,
        )?;
        // Connectivity as `validate` sees it; every chain end is a junction
        // and every new run touches same-net metal only there, so this holds
        // by construction and is checked anyway (the whole net is kept as it
        // was otherwise).
        if !self.net_connected(net, &runs, &old.vias, &fixed)? {
            tally.disconnected += 1;
            if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                eprintln!(
                    "[troute-straighten] net {net} disconnected; old {:?} new {:?} vias {:?} plan_wires {:?} plan_vias {:?} access {:?}",
                    old.wires, runs, old.vias, fixed.plan_wires, fixed.plan_vias, fixed.access
                );
            }
            return Ok(None);
        }
        charge(&mut self.spatial.work, old.shapes.len(), cap)?;
        for &id in &old.shapes {
            self.spatial.remove(id);
        }
        let mut route = NetRoute {
            wires: runs,
            vias: old.vias.clone(),
            shapes: Vec::new(),
        };
        for w in &route.wires {
            let rect = self.edge_rect(w.layer, w.start, w.end, net)?;
            route.shapes.push(self.spatial.add_routed(Blockage {
                layer: w.layer,
                rect,
                net: Some(net),
            })?);
        }
        for v in &route.vias {
            for s in &input.vias[v.kind as usize].shapes {
                route.shapes.push(self.spatial.add_routed(Blockage {
                    layer: s.layer,
                    rect: shifted(s.rect, v.at)?,
                    net: Some(net),
                })?);
            }
        }
        Ok(Some(route))
    }
}

/// `RouteQor::straighten` over every routed net (see `routing::Straighten`
/// and the module docs).
#[allow(clippy::too_many_arguments)]
pub(super) fn straighten(
    r: &mut Router<'_>,
    cfg: &crate::routing::Straighten,
    groups: &BTreeMap<u32, Vec<&Terminal>>,
    plans: &HashMap<u32, Plan>,
    region: Region<'_>,
    routed: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    prior: Option<&PriorRoutes>,
    t0: std::time::Instant,
) -> Result<()> {
    let limits = r.limits;
    let mut total = StraightenTally::default();
    if let Some(reroute) = &cfg.reroute {
        reroute_bends(
            r,
            reroute,
            groups,
            plans,
            region,
            routed,
            retained_edges,
            prior,
            &mut total,
            t0,
        )?;
    }
    for pass in 0..cfg.passes {
        let mut tally = StraightenTally::default();
        let nets: Vec<u32> = routed.keys().copied().collect();
        for net in nets {
            let Some(terminals) = groups.get(&net) else {
                continue;
            };
            let route = &routed[&net];
            if route.wires.len() > MAX_NET_RUNS
                || prior
                    .and_then(|p| p.nets.get(&net))
                    .is_some_and(|(w, v)| route.wires == *w && route.vias == *v)
            {
                tally.skipped += 1;
                continue;
            }
            if let Some(new) =
                r.straighten_net(net, route, terminals, plans, cfg.turn_at_ends, &mut tally)?
            {
                tally.nets += 1;
                retain_route(routed, retained_edges, net, new, limits.max_segments)?;
            }
        }
        trace(
            "straighten",
            t0,
            format_args!(
                "pass {pass} chains {} kept {} nets {} runs -{} length -{} blocked {} wide {} contact {} rules {} searched {} found {} disconnected {} skipped {} work {}",
                tally.chains,
                tally.kept,
                tally.nets,
                tally.runs_removed,
                tally.length_removed,
                tally.blocked,
                tally.wide,
                tally.contact,
                tally.rules,
                tally.searched,
                tally.found,
                tally.disconnected,
                tally.skipped,
                r.spatial.work
            ),
        );
        total.chains += tally.chains;
        total.kept += tally.kept;
        total.nets += tally.nets;
        total.runs_removed += tally.runs_removed;
        total.length_removed += tally.length_removed;
        total.blocked += tally.blocked;
        total.wide += tally.wide;
        total.contact += tally.contact;
        total.rules += tally.rules;
        total.searched += tally.searched;
        total.found += tally.found;
        total.disconnected += tally.disconnected;
        total.skipped += tally.skipped;
        if tally.kept == 0 {
            break;
        }
    }
    trace(
        "straighten",
        t0,
        format_args!(
            "total chains {} kept {} nets {} runs -{} length -{} disconnected {}",
            total.chains,
            total.kept,
            total.nets,
            total.runs_removed,
            total.length_removed,
            total.disconnected
        ),
    );
    #[cfg(test)]
    TALLY.with(|t| t.set(Some(total)));
    Ok(())
}

/// Global guides and area-rule demotions of the routed nets (for the
/// bend-priced re-route's search regions, as the via reduction uses them).
#[derive(Clone, Copy)]
pub(super) struct Region<'g> {
    pub guides: &'g BTreeMap<u32, Vec<u32>>,
    pub demoted: &'g BTreeMap<u32, u8>,
}

/// Fixed same-net geometry of `terminals` (their escapes and pins).
fn fixed_of<'p>(terminals: &[&Terminal], plans: &'p HashMap<u32, Plan>) -> Fixed<'p> {
    Fixed {
        plan_wires: terminals
            .iter()
            .flat_map(|t| plans[&t.pin].wires.iter())
            .collect(),
        plan_vias: terminals
            .iter()
            .flat_map(|t| plans[&t.pin].vias.iter())
            .collect(),
        access: terminals.iter().map(|t| &plans[&t.pin].access).collect(),
        pins: terminals
            .iter()
            .flat_map(|t| t.shapes.iter().map(|s| (s.layer, s.rect)))
            .collect(),
    }
}

/// `Straighten::reroute` (see `routing::StraightenReroute`).
#[allow(clippy::too_many_arguments)]
fn reroute_bends(
    r: &mut Router<'_>,
    cfg: &crate::routing::StraightenReroute,
    groups: &BTreeMap<u32, Vec<&Terminal>>,
    plans: &HashMap<u32, Plan>,
    region: Region<'_>,
    routed: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    prior: Option<&PriorRoutes>,
    total: &mut StraightenTally,
    t0: std::time::Instant,
) -> Result<()> {
    if r.trial_search_limit.is_some()
        || r.trial_expansion_limit.is_some()
        || r.allowed_victim.is_some()
        || r.protected.is_some()
    {
        return Err(invalid("straightening re-route inside a trial search"));
    }
    let limits = r.limits;
    // Candidates: chain runs (three or more per chain) per net.
    let mut order: Vec<(std::cmp::Reverse<usize>, u32)> = Vec::new();
    let nets: Vec<u32> = routed.keys().copied().collect();
    for net in nets {
        let Some(terminals) = groups.get(&net) else {
            continue;
        };
        let route = &routed[&net];
        if terminals.len() > cfg.max_terminals
            || route.wires.len() > MAX_NET_RUNS
            || prior
                .and_then(|p| p.nets.get(&net))
                .is_some_and(|(w, v)| route.wires == *w && route.vias == *v)
        {
            continue;
        }
        let fixed = fixed_of(terminals, plans);
        let runs: usize = r
            .chains(&route.wires, &route.vias, &fixed)?
            .iter()
            .map(|c| c.wires.len())
            .sum();
        if runs > 0 {
            order.push((std::cmp::Reverse(runs), net));
        }
    }
    order.sort_unstable();
    charge(
        &mut r.spatial.work,
        order
            .len()
            .saturating_mul(order.len().max(1).ilog2() as usize + 1),
        limits.max_comparisons,
    )?;
    let saved_bend = std::mem::replace(&mut r.bend_cost, cfg.bend_cost);
    let saved_weight = r.heuristic_weight_override.replace(cfg.weight);
    let result = (|| -> Result<()> {
        let none = BTreeMap::new();
        for &(_, net) in &order {
            let terminals = &groups[&net];
            let old = remove_route(routed, retained_edges, net)
                .ok_or_else(|| invalid("straightening re-route without a route"))?;
            charge(
                &mut r.spatial.work,
                old.shapes.len(),
                limits.max_comparisons,
            )?;
            for &id in &old.shapes {
                r.spatial.remove(id);
            }
            r.area_level = if r.plan.area_stacks {
                2 - region.demoted.get(&net).copied().unwrap_or(0)
            } else {
                0
            };
            let guide = &region.guides[&net];
            if r.close_expanded {
                r.mark_region_corridor(guide, r.plan.expand_margin);
            } else {
                r.mark_region(guide, Some(r.plan.expand_margin));
            }
            total.rerouted += 1;
            r.trial_expansion_limit = Some(r.expansions.saturating_add(cfg.net_expansions));
            let outcome = r.route_net(net, terminals, plans, false, false, &none, 0);
            r.trial_expansion_limit = None;
            let (old_vias, old_len) = routed_size(&old, terminals, plans);
            let kept = match outcome? {
                Outcome::Routed(new, victims) => {
                    if !victims.is_empty() {
                        return Err(invalid("hard straightening re-route displaced a net"));
                    }
                    let (new_vias, new_len) = routed_size(&new, terminals, plans);
                    if new.wires.len() < old.wires.len()
                        && new_vias <= old_vias
                        && new.vias.len() <= old.vias.len()
                        && new_len <= old_len
                    {
                        total.reroute_runs += old.wires.len() - new.wires.len();
                        Some(new)
                    } else {
                        discard_route(r, new)?;
                        None
                    }
                }
                Outcome::Limited(_) => {
                    total.reroute_limited += 1;
                    None
                }
                Outcome::Failed(_) => {
                    total.reroute_failed += 1;
                    None
                }
            };
            let route = match kept {
                Some(new) => {
                    total.reroute_kept += 1;
                    new
                }
                None => {
                    for &id in &old.shapes {
                        r.spatial.restore(id)?;
                    }
                    old
                }
            };
            retain_route(routed, retained_edges, net, route, limits.max_segments)?;
        }
        Ok(())
    })();
    r.trial_expansion_limit = None;
    r.heuristic_weight_override = saved_weight;
    r.bend_cost = saved_bend;
    result?;
    trace(
        "straighten",
        t0,
        format_args!(
            "re-route candidates {} kept {} limited {} failed {} runs -{} work {} exp {}",
            total.rerouted,
            total.reroute_kept,
            total.reroute_limited,
            total.reroute_failed,
            total.reroute_runs,
            r.spatial.work,
            r.expansions
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chain through `points` on layer 1, one run per consecutive pair.
    fn chain(points: &[[i64; 2]]) -> (Chain, Vec<Wire>) {
        let wires: Vec<Wire> = points
            .windows(2)
            .map(|p| Wire {
                net: 0,
                layer: 1,
                start: p[0],
                end: p[1],
            })
            .collect();
        (
            Chain {
                wires: (0..wires.len()).collect(),
                points: points.to_vec(),
                layer: 1,
            },
            wires,
        )
    }

    #[test]
    fn candidates_keep_both_end_axes_and_are_never_longer() {
        // Staircase V H V H V: Z shapes on the chain's own corner rows.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80], [80, 80], [80, 120]]);
        assert_eq!(
            candidates(&c, &w, false),
            vec![
                vec![[0, 0], [0, 40], [80, 40], [80, 120]],
                vec![[0, 0], [0, 80], [80, 80], [80, 120]],
            ]
        );
        // V H V H: the one L that leaves along y and arrives along x.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80], [80, 80]]);
        assert_eq!(
            candidates(&c, &w, false),
            vec![vec![[0, 0], [0, 80], [80, 80]]]
        );
        // H V H V from the other side: the L leaves along x.
        let (c, w) = chain(&[[0, 0], [40, 0], [40, 40], [80, 40], [80, 80]]);
        assert_eq!(
            candidates(&c, &w, false),
            vec![vec![[0, 0], [80, 0], [80, 80]]]
        );
        // A zigzag whose ends are aligned: the straight run.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80], [0, 80], [0, 120]]);
        assert_eq!(candidates(&c, &w, false), vec![vec![[0, 0], [0, 120]]]);
        // A three-run Z is already minimal: no candidate with fewer runs.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80]]);
        assert!(candidates(&c, &w, false).is_empty());
        // A U turn (ends along y, aligned in y) has no axis-keeping rewrite.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 0]]);
        assert!(candidates(&c, &w, false).is_empty());
        // A loop back to its own junction is left alone.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 0], [0, 0]]);
        assert!(candidates(&c, &w, false).is_empty());
        // Turning at the ends: a three-run staircase becomes either L (one
        // end changes axis), the L keeping the start axis first.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80]]);
        assert_eq!(
            candidates(&c, &w, true),
            vec![
                vec![[0, 0], [0, 80], [40, 80]],
                vec![[0, 0], [40, 0], [40, 80]],
            ]
        );
        // The axis-keeping Z of a five-run staircase comes after both Ls.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 80], [80, 80], [80, 120]]);
        let turned = candidates(&c, &w, true);
        assert_eq!(turned[0], vec![[0, 0], [0, 120], [80, 120]]);
        assert_eq!(turned[1], vec![[0, 0], [80, 0], [80, 120]]);
        assert_eq!(turned[2], vec![[0, 0], [0, 40], [80, 40], [80, 120]]);
        assert_eq!(turned[3], vec![[0, 0], [0, 80], [80, 80], [80, 120]]);
        assert!(turned[4..].iter().all(|c| c.len() == 4));
        // A U turn whose ends are aligned across it: the straight run.
        let (c, w) = chain(&[[0, 0], [0, 40], [40, 40], [40, 0]]);
        assert_eq!(candidates(&c, &w, true), vec![vec![[0, 0], [40, 0]]]);
        for points in [
            &[[0, 0], [0, 40], [40, 40], [40, 80], [80, 80], [80, 120]][..],
            &[[0, 0], [0, 40], [40, 40], [40, 80], [0, 80], [0, 120]][..],
            &[[0, 0], [0, 40], [40, 40], [40, 80], [80, 80]][..],
        ] {
            let (c, w) = chain(points);
            for cand in candidates(&c, &w, false) {
                assert!(length(&cand) <= length(points));
                assert!(cand.len() < points.len());
                assert_eq!(cand[0], points[0]);
                assert_eq!(cand.last(), points.last());
                let axis = |a: [i64; 2], b: [i64; 2]| vertical(a, b);
                assert_eq!(axis(cand[0], cand[1]), axis(points[0], points[1]));
                let (n, m) = (cand.len(), points.len());
                assert_eq!(
                    axis(cand[n - 2], cand[n - 1]),
                    axis(points[m - 2], points[m - 1])
                );
            }
        }
    }
}
