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
//!
//! The optional bend-priced re-route (`reroute_bends`, `Straighten::reroute`)
//! runs before the rewrites and may replace a net's whole route, vias
//! included; it keeps a new route only with fewer runs and no more vias or
//! wire.
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
    /// Joint moves (`neighbors`): tried, kept, refused because a ripped net
    /// did not route again, refused by the acceptance test, and the DEF
    /// wire pieces kept moves removed.
    pub moves: usize,
    pub moves_kept: usize,
    pub moves_unrouted: usize,
    pub moves_worse: usize,
    pub move_pieces: usize,
    /// Escape jogs (`escape_jogs`): escapes examined (a straight stub whose
    /// top the route leaves with a jog) and moved to their via.
    pub jogs: usize,
    pub jogs_kept: usize,
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
    plans: &mut HashMap<u32, Plan>,
    region: Region<'_>,
    routed: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    prior: Option<&PriorRoutes>,
    t0: std::time::Instant,
) -> Result<()> {
    let limits = r.limits;
    let mut total = StraightenTally::default();
    if cfg.escape_jogs {
        escape_jogs(
            r,
            groups,
            plans,
            routed,
            retained_edges,
            prior,
            &mut total,
            t0,
        )?;
    }
    let plans: &HashMap<u32, Plan> = plans;
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
    if let Some(moves) = &cfg.neighbors {
        neighbor_moves(
            r,
            moves,
            cfg.turn_at_ends,
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

/// `Straighten::escape_jogs`: every terminal of a routed net (nets and
/// terminals in id order) whose escape has a straight access-layer stub from
/// its via landing `v` to its top `t`, which the route leaves with a jog (a
/// wire across the stub's axis to a plain corner `c`, then a run `w2` along
/// the stub's axis), takes the jog at `v` instead: the escape becomes the jog
/// from `v` to `c`'s track and a stub of the old length along that track (to
/// `c` when `w2` continues away from `v`, else from `v`'s row toward `w2`'s
/// far end), the route keeps the rest of `w2`, and the jog at the top goes.
/// Wire never grows (it shrinks by twice the stub where the route turned
/// back past `v`) and the DEF loses at least one piece. Kept only when every
/// new rectangle is clear of other nets' metal and wide-metal spacing, it
/// touches no same-net metal but the via pad at `v` and `w2`, the net's
/// same-net rule check on the access layer reports no new violation, the new
/// top is a graph node, and the net passes the endpoint connectivity check;
/// otherwise the plan and the route stay as they were. In an ECO route a net
/// that still holds its prior geometry is left alone.
#[allow(clippy::too_many_arguments)]
fn escape_jogs(
    r: &mut Router<'_>,
    groups: &BTreeMap<u32, Vec<&Terminal>>,
    plans: &mut HashMap<u32, Plan>,
    routed: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    prior: Option<&PriorRoutes>,
    total: &mut StraightenTally,
    t0: std::time::Instant,
) -> Result<()> {
    let limits = r.limits;
    let nets: Vec<u32> = routed.keys().copied().collect();
    for net in nets {
        let Some(terminals) = groups.get(&net) else {
            continue;
        };
        if routed[&net].wires.len() > MAX_NET_RUNS
            || prior
                .and_then(|p| p.nets.get(&net))
                .is_some_and(|(w, v)| routed[&net].wires == *w && routed[&net].vias == *v)
        {
            continue;
        }
        for t in terminals {
            if let Some((plan, route)) =
                r.escape_jog(net, t.pin, terminals, plans, &routed[&net], total)?
            {
                plans.insert(t.pin, plan);
                retain_route(routed, retained_edges, net, route, limits.max_segments)?;
            }
        }
    }
    trace(
        "straighten",
        t0,
        format_args!(
            "escape jogs examined {} kept {} work {}",
            total.jogs, total.jogs_kept, r.spatial.work
        ),
    );
    Ok(())
}

impl Router<'_> {
    /// One terminal of `escape_jogs`: `Some((plan, route))` with the index
    /// updated when the jog moves to the via, `None` with nothing changed.
    fn escape_jog(
        &mut self,
        net: u32,
        pin: u32,
        terminals: &[&Terminal],
        plans: &HashMap<u32, Plan>,
        route: &NetRoute,
        tally: &mut StraightenTally,
    ) -> Result<Option<(Plan, NetRoute)>> {
        let input = self.input;
        let cap = self.limits.max_comparisons;
        let acc = input.access_layer;
        let plan = &plans[&pin];
        let (top_layer, top) = self.graph.point(plan.top);
        if top_layer != acc {
            return Ok(None);
        }
        charge(
            &mut self.spatial.work,
            plan.wires.len() + plan.vias.len() + route.wires.len() + route.vias.len() + 4,
            cap,
        )?;
        // The stub: the plan's access-layer wires, one straight interval on
        // the top's track ending at the top.
        let vertical = input.layers[acc as usize].vertical.unwrap_or(true);
        let ax = usize::from(vertical); // coordinate along the stub
        let cx = 1 - ax; // coordinate across it
        let stub: Vec<&Wire> = plan.wires.iter().filter(|w| w.layer == acc).collect();
        if stub.is_empty()
            || stub
                .iter()
                .any(|w| w.start[cx] != top[cx] || w.end[cx] != top[cx])
        {
            return Ok(None);
        }
        let mut spans: Vec<(i64, i64)> = stub
            .iter()
            .map(|w| (w.start[ax].min(w.end[ax]), w.start[ax].max(w.end[ax])))
            .collect();
        spans.sort_unstable();
        let (lo, mut hi) = spans[0];
        for &(a, b) in &spans[1..] {
            if a > hi {
                return Ok(None);
            }
            hi = hi.max(b);
        }
        let far = if top[ax] == hi {
            lo
        } else if top[ax] == lo {
            hi
        } else {
            return Ok(None);
        };
        if far == top[ax] {
            return Ok(None);
        }
        let mut v = top;
        v[ax] = far;
        // Exactly one via of the plan lands on the access layer, at `v`.
        let landing = |via: &Via| {
            let k = &input.vias[via.kind as usize];
            k.lower == acc || k.upper == acc
        };
        let on_acc: Vec<&Via> = plan.vias.iter().filter(|x| landing(x)).collect();
        if on_acc.len() != 1 || on_acc[0].at != v {
            return Ok(None);
        }
        // The route at the top: one wire across the stub (the jog), no via,
        // nothing passing through.
        let ends_at = |p: [i64; 2]| -> Vec<usize> {
            route
                .wires
                .iter()
                .enumerate()
                .filter(|(_, w)| w.layer == acc && (w.start == p || w.end == p))
                .map(|(i, _)| i)
                .collect()
        };
        let through = |p: [i64; 2]| {
            route
                .wires
                .iter()
                .any(|w| w.layer == acc && on_wire(w, p) && w.start != p && w.end != p)
        };
        let via_at = |p: [i64; 2]| route.vias.iter().any(|x| x.at == p && landing(x));
        let at_top = ends_at(top);
        if at_top.len() != 1 || through(top) || via_at(top) {
            return Ok(None);
        }
        let w1 = &route.wires[at_top[0]];
        let c = if w1.start == top { w1.end } else { w1.start };
        if c[ax] != top[ax] || c[cx] == top[cx] {
            return Ok(None);
        }
        // A plain corner at `c`, then a run along the stub's axis.
        let at_c = ends_at(c);
        if at_c.len() != 2 || through(c) || via_at(c) {
            return Ok(None);
        }
        let i2 = if at_c[0] == at_top[0] {
            at_c[1]
        } else {
            at_c[0]
        };
        let w2 = &route.wires[i2];
        let f = if w2.start == c { w2.end } else { w2.start };
        if f[cx] != c[cx] || f[ax] == c[ax] {
            return Ok(None);
        }
        // No other terminal's escape metal at the corner.
        let fixed_all = fixed_of(terminals, plans);
        if fixed_all
            .plan_wires
            .iter()
            .any(|w| w.layer == acc && on_wire(w, c))
            || fixed_all.plan_vias.iter().any(|x| x.at == c && landing(x))
        {
            return Ok(None);
        }
        tally.jogs += 1;
        let length = (top[ax] - far).abs();
        let away = (top[ax] - far).signum();
        let mono = (f[ax] - c[ax]).signum() == away;
        let mut corner = v;
        corner[cx] = c[cx];
        // New top: `c` when w2 leaves away from `v`, else one stub length
        // from `corner` toward `f` (strictly inside w2).
        let new_top = if mono {
            c
        } else {
            let mut p = corner;
            p[ax] = corner[ax] - away * length;
            if (f[ax] - p[ax]).signum() != -away || p[ax] == f[ax] {
                return Ok(None);
            }
            p
        };
        let Some(&slot) = self.graph.slot.get(&acc) else {
            return Ok(None);
        };
        let Some(top_node) = self.graph.find(slot, new_top) else {
            return Ok(None);
        };
        let jog = Wire {
            net,
            layer: acc,
            start: v,
            end: corner,
        };
        let leg = Wire {
            net,
            layer: acc,
            start: corner,
            end: new_top,
        };
        let width = input.layers[acc as usize].width;
        let wide = input.layers[acc as usize].wide_spacing;
        let new_rects = [wire_rect(&jog, width)?, wire_rect(&leg, width)?];
        // (1) Other nets, fixed metal, obstructions, wide metal.
        for &rect in &new_rects {
            if self.spatial.probe(acc, rect, net)?.is_none() {
                return Ok(None);
            }
            if let Some(w) = wide
                && !self.spatial.wide_clear(acc, rect, net, w)?
            {
                return Ok(None);
            }
        }
        // (2) Same-net contact: only the via pad at `v` and w2.
        let mut others: Vec<Rect> = Vec::new();
        for (i, w) in route.wires.iter().enumerate() {
            if w.layer == acc && i != at_top[0] && i != i2 {
                others.push(wire_rect(w, width)?);
            }
        }
        for w in fixed_all.plan_wires.iter().filter(|w| w.layer == acc) {
            if !stub.iter().any(|s| std::ptr::eq(*s, *w)) {
                others.push(wire_rect(w, width)?);
            }
        }
        for x in route.vias.iter().chain(fixed_all.plan_vias.iter().copied()) {
            if x.at == v && std::ptr::eq(x, on_acc[0]) {
                continue;
            }
            for sh in input.vias[x.kind as usize]
                .shapes
                .iter()
                .filter(|sh| sh.layer == acc)
            {
                others.push(shifted(sh.rect, x.at)?);
            }
        }
        for &(l, rect) in &fixed_all.pins {
            if l == acc {
                others.push(rect);
            }
        }
        charge(&mut self.spatial.work, others.len() * 2, cap)?;
        if new_rects
            .iter()
            .any(|&nr| others.iter().any(|&o| touch(nr, o)))
        {
            return Ok(None);
        }
        // The new plan and route.
        let mut new_plan = plan.clone();
        let mut wires: Vec<Wire> = plan
            .wires
            .iter()
            .filter(|w| w.layer != acc)
            .cloned()
            .collect();
        let kept_shapes: Vec<usize> = plan
            .wires
            .iter()
            .zip(plan.shapes.iter())
            .filter(|(w, _)| w.layer != acc)
            .map(|(_, &id)| id)
            .collect();
        let dropped_shapes: Vec<usize> = plan
            .wires
            .iter()
            .zip(plan.shapes.iter())
            .filter(|(w, _)| w.layer == acc)
            .map(|(_, &id)| id)
            .collect();
        wires.push(jog.clone());
        wires.push(leg.clone());
        let mut new_route = NetRoute {
            wires: Vec::with_capacity(route.wires.len()),
            vias: route.vias.clone(),
            shapes: Vec::new(),
        };
        for (i, w) in route.wires.iter().enumerate() {
            if i == at_top[0] {
                continue;
            }
            if i == i2 && !mono {
                new_route.wires.push(Wire {
                    net,
                    layer: acc,
                    start: new_top,
                    end: f,
                });
                continue;
            }
            new_route.wires.push(w.clone());
        }
        new_plan.wires = wires.into_boxed_slice();
        new_plan.top = top_node;
        // (3) No new same-net violation on the access layer.
        let old_fixed = fixed_all;
        let new_fixed = {
            let mut f = fixed_of(terminals, plans);
            f.plan_wires = terminals
                .iter()
                .flat_map(|t| {
                    if t.pin == pin {
                        new_plan.wires.iter()
                    } else {
                        plans[&t.pin].wires.iter()
                    }
                })
                .collect();
            f
        };
        let all_vias: Vec<&Via> = route
            .vias
            .iter()
            .chain(old_fixed.plan_vias.iter().copied())
            .collect();
        let before: Vec<&Wire> = route
            .wires
            .iter()
            .chain(old_fixed.plan_wires.iter().copied())
            .collect();
        let after: Vec<&Wire> = new_route
            .wires
            .iter()
            .chain(new_fixed.plan_wires.iter().copied())
            .collect();
        let old_set = self.layer_violations(net, acc, &before, &all_vias, terminals)?;
        let new_set = self.layer_violations(net, acc, &after, &all_vias, terminals)?;
        if new_set.iter().any(|x| old_set.binary_search(x).is_err()) {
            return Ok(None);
        }
        // (4) Connectivity as `validate` sees it.
        let pairs: Vec<(u32, u32)> = input.vias.iter().map(|x| (x.lower, x.upper)).collect();
        let runs = crate::wire_runs::collinear_runs(
            &new_route.wires.iter().collect::<Vec<_>>(),
            &new_route
                .vias
                .iter()
                .chain(new_fixed.plan_vias.iter().copied())
                .collect::<Vec<_>>(),
            &pairs,
            &new_fixed.access,
            cap,
        )?;
        if !self.net_connected(net, &runs, &new_route.vias, &new_fixed)? {
            return Ok(None);
        }
        // Top-layer area of the new escape decides whether a later via at
        // its top stacks (as the escape search records it).
        new_plan.top_via = !self.stack_ok[acc as usize]
            && !terminals
                .iter()
                .any(|t| t.pin == pin && t.shapes.iter().any(|s| s.layer == acc))
            && match input.layers[acc as usize].min_area {
                Some(min) => {
                    let mut metal = vec![new_rects[0], new_rects[1]];
                    for sh in &input.vias[on_acc[0].kind as usize].shapes {
                        if sh.layer == acc {
                            metal.push(shifted(sh.rect, v)?);
                        }
                    }
                    route_rules::union_area(&metal, &mut self.spatial.work, cap)? < i128::from(min)
                }
                None => false,
            };
        // Commit: index shapes of the stub and the old route out, new in.
        charge(
            &mut self.spatial.work,
            dropped_shapes.len() + route.shapes.len(),
            cap,
        )?;
        for &id in &dropped_shapes {
            self.spatial.remove(id);
        }
        for &id in &route.shapes {
            self.spatial.remove(id);
        }
        let mut shapes = kept_shapes;
        for w in [&jog, &leg] {
            shapes.push(self.spatial.add_fixed(Blockage {
                layer: acc,
                rect: wire_rect(w, width)?,
                net: Some(net),
            })?);
        }
        shapes.extend_from_slice(&plan.shapes[plan.wires.len()..]);
        new_plan.shapes = shapes.into_boxed_slice();
        for w in &new_route.wires {
            let rect = self.edge_rect(w.layer, w.start, w.end, net)?;
            new_route.shapes.push(self.spatial.add_routed(Blockage {
                layer: w.layer,
                rect,
                net: Some(net),
            })?);
        }
        for x in &new_route.vias {
            for sh in &input.vias[x.kind as usize].shapes {
                new_route.shapes.push(self.spatial.add_routed(Blockage {
                    layer: sh.layer,
                    rect: shifted(sh.rect, x.at)?,
                    net: Some(net),
                })?);
            }
        }
        tally.jogs_kept += 1;
        Ok(Some((new_plan, new_route)))
    }
}

/// DEF wire pieces of one net (its escapes and `route`): the wires merged
/// into maximal collinear runs split at every via landing, access point and
/// branch strictly inside them (`wire_runs`, as `export-def` writes them).
/// This is the count the reference extractor's work follows
/// (`scripts/def_pieces.py`).
pub(super) fn net_pieces(
    r: &mut Router<'_>,
    route: &NetRoute,
    terminals: &[&Terminal],
    plans: &HashMap<u32, Plan>,
) -> Result<usize> {
    let fixed = fixed_of(terminals, plans);
    let wires: Vec<&Wire> = fixed
        .plan_wires
        .iter()
        .copied()
        .chain(route.wires.iter())
        .collect();
    let mut vias: Vec<&Via> = fixed
        .plan_vias
        .iter()
        .copied()
        .chain(route.vias.iter())
        .collect();
    vias.sort_unstable_by_key(|v| (v.kind, v.at));
    vias.dedup();
    let cap = r.limits.max_comparisons;
    charge(
        &mut r.spatial.work,
        wires.len().saturating_add(vias.len()).saturating_mul(4),
        cap,
    )?;
    let pairs: Vec<(u32, u32)> = r.input.vias.iter().map(|v| (v.lower, v.upper)).collect();
    Ok(crate::wire_runs::collinear_runs(&wires, &vias, &pairs, &fixed.access, cap)?.len())
}

/// The other nets whose routed metal alone refuses a candidate of a chain of
/// `net` (per chain the first such candidate in the rewrites' order;
/// `Straighten::neighbors`); `None` when no chain has such a candidate, or
/// more than `max` nets would have to move.
fn walling_nets(
    r: &mut Router<'_>,
    net: u32,
    route: &NetRoute,
    fixed: &Fixed<'_>,
    turn: bool,
    max: usize,
) -> Result<Option<Vec<u32>>> {
    let chains = r.chains(&route.wires, &route.vias, fixed)?;
    let mut all: Vec<u32> = Vec::new();
    for chain in &chains {
        let width = r.input.layers[chain.layer as usize].width;
        // The first candidate (rewrite order) that routed metal of other
        // nets alone refuses; a candidate that fixed metal refuses, or that
        // no other net refuses (it failed a same-net check), is passed over.
        let mut found: Option<Vec<u32>> = None;
        for cand in candidates(chain, &route.wires, turn) {
            let mut victims: Vec<u32> = Vec::new();
            let mut blocked = false;
            for p in cand.windows(2) {
                let w = Wire {
                    net,
                    layer: chain.layer,
                    start: p[0],
                    end: p[1],
                };
                match r
                    .spatial
                    .probe_soft(chain.layer, wire_rect(&w, width)?, net)?
                {
                    crate::routing::Soft::Blocked => {
                        blocked = true;
                        break;
                    }
                    crate::routing::Soft::Clear { victims: v, .. } => victims.extend(v),
                }
            }
            if !blocked && !victims.is_empty() {
                found = Some(victims);
                break;
            }
        }
        let Some(victims) = found else {
            continue;
        };
        all.extend(victims);
        all.sort_unstable();
        all.dedup();
        if all.len() > max {
            return Ok(None);
        }
    }
    Ok((!all.is_empty()).then_some(all))
}

/// `Straighten::neighbors` (see `routing::StraightenNeighbors`).
#[allow(clippy::too_many_arguments)]
fn neighbor_moves(
    r: &mut Router<'_>,
    cfg: &crate::routing::StraightenNeighbors,
    turn: bool,
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
        return Err(invalid("straightening move inside a trial search"));
    }
    let limits = r.limits;
    let held = |net: u32, routed: &BTreeMap<u32, NetRoute>| {
        prior
            .and_then(|p| p.nets.get(&net))
            .zip(routed.get(&net))
            .is_some_and(|((w, v), route)| route.wires == *w && route.vias == *v)
    };
    let movable = |net: u32, routed: &BTreeMap<u32, NetRoute>| {
        groups
            .get(&net)
            .is_some_and(|t| t.len() <= cfg.max_terminals)
            && routed
                .get(&net)
                .is_some_and(|route| route.wires.len() <= MAX_NET_RUNS)
            && !held(net, routed)
    };
    for pass in 0..cfg.passes {
        // Nets with chains, most chain runs first.
        let mut order: Vec<(std::cmp::Reverse<usize>, u32)> = Vec::new();
        let nets: Vec<u32> = routed.keys().copied().collect();
        for net in nets {
            if !movable(net, routed) {
                continue;
            }
            let fixed = fixed_of(&groups[&net], plans);
            let runs: usize = r
                .chains(&routed[&net].wires, &routed[&net].vias, &fixed)?
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
        let kept_before = total.moves_kept;
        let none = BTreeMap::new();
        for &(_, net) in &order {
            // An earlier move may have changed or straightened this net.
            if !movable(net, routed) {
                continue;
            }
            let terminals = &groups[&net];
            let victims = {
                let fixed = fixed_of(terminals, plans);
                let route = routed[&net].clone();
                walling_nets(r, net, &route, &fixed, turn, cfg.max_victims)?
            };
            let Some(victims) = victims else {
                continue;
            };
            if victims.iter().any(|&v| v == net || !movable(v, routed)) {
                continue;
            }
            total.moves += 1;
            // Rip up the walling nets.
            let mut ripped: Vec<(u32, NetRoute)> = Vec::new();
            for &v in &victims {
                let old = remove_route(routed, retained_edges, v)
                    .ok_or_else(|| invalid("straightening move without a route"))?;
                charge(
                    &mut r.spatial.work,
                    old.shapes.len(),
                    limits.max_comparisons,
                )?;
                for &id in &old.shapes {
                    r.spatial.remove(id);
                }
                ripped.push((v, old));
            }
            let old = routed[&net].clone();
            let mut tally = StraightenTally::default();
            let rewritten = r.straighten_net(net, &old, terminals, plans, turn, &mut tally)?;
            // Route the ripped nets again (hard searches, all else fixed).
            let mut rerouted: Vec<(u32, NetRoute)> = Vec::new();
            let mut unrouted = rewritten.is_none();
            if !unrouted {
                for &(v, _) in &ripped {
                    r.area_level = if r.plan.area_stacks {
                        2 - region.demoted.get(&v).copied().unwrap_or(0)
                    } else {
                        0
                    };
                    let guide = &region.guides[&v];
                    if r.close_expanded {
                        r.mark_region_corridor(guide, r.plan.expand_margin);
                    } else {
                        r.mark_region(guide, Some(r.plan.expand_margin));
                    }
                    r.trial_expansion_limit = Some(r.expansions.saturating_add(cfg.net_expansions));
                    let outcome = r.route_net(v, &groups[&v], plans, false, false, &none, 0);
                    r.trial_expansion_limit = None;
                    match outcome? {
                        Outcome::Routed(new, displaced) => {
                            if !displaced.is_empty() {
                                return Err(invalid("hard straightening move displaced a net"));
                            }
                            rerouted.push((v, new));
                        }
                        Outcome::Limited(_) | Outcome::Failed(_) => {
                            unrouted = true;
                            break;
                        }
                    }
                }
            }
            // Acceptance: fewer DEF pieces in all, no more routed vias, wire
            // within the allowance.
            let mut keep = false;
            if !unrouted && let Some(new) = &rewritten {
                let size =
                    |route: &NetRoute, n: u32, r: &mut Router<'_>| -> Result<(usize, u64, u64)> {
                        let t = &groups[&n];
                        let (vias, len) = routed_size(route, t, plans);
                        Ok((net_pieces(r, route, t, plans)?, vias, len))
                    };
                let (mut p0, mut v0, mut l0) = size(&old, net, r)?;
                let (mut p1, mut v1, mut l1) = size(new, net, r)?;
                for ((v, before), (_, after)) in ripped.iter().zip(&rerouted) {
                    let a = size(before, *v, r)?;
                    let b = size(after, *v, r)?;
                    (p0, v0, l0) = (p0 + a.0, v0 + a.1, l0 + a.2);
                    (p1, v1, l1) = (p1 + b.0, v1 + b.1, l1 + b.2);
                }
                let allowance = (p0.saturating_sub(p1) as u64).saturating_mul(cfg.length_per_piece);
                keep = p1 < p0 && v1 <= v0 && l1 <= l0.saturating_add(allowance);
                if keep {
                    total.move_pieces += p0 - p1;
                } else {
                    total.moves_worse += 1;
                }
            } else {
                total.moves_unrouted += 1;
            }
            if keep {
                total.moves_kept += 1;
                let new = rewritten.expect("kept move holds the rewrite");
                retain_route(routed, retained_edges, net, new, limits.max_segments)?;
                for (v, route) in rerouted {
                    retain_route(routed, retained_edges, v, route, limits.max_segments)?;
                }
                continue;
            }
            // Undo: new shapes out, old shapes back, old routes kept.
            for (_, route) in rerouted {
                discard_route(r, route)?;
            }
            if let Some(new) = rewritten {
                discard_route(r, new)?;
                for &id in &old.shapes {
                    r.spatial.restore(id)?;
                }
            }
            for (v, route) in ripped {
                for &id in &route.shapes {
                    r.spatial.restore(id)?;
                }
                retain_route(routed, retained_edges, v, route, limits.max_segments)?;
            }
        }
        trace(
            "straighten",
            t0,
            format_args!(
                "moves pass {pass} tried {} kept {} unrouted {} worse {} pieces -{} work {} exp {}",
                total.moves,
                total.moves_kept,
                total.moves_unrouted,
                total.moves_worse,
                total.move_pieces,
                r.spatial.work,
                r.expansions
            ),
        );
        if total.moves_kept == kept_before {
            break;
        }
    }
    Ok(())
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
