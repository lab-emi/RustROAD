//! Track router (`cpu_ref`, deterministic, single worker).
//!
//! 1. **Track graph.** Every routing layer gets its own track lines from the
//!    LEF PITCH/OFFSET (or a profile override). Along a track the nodes are
//!    the track lines of the layers it shares a via with ("stops"), so a via
//!    always lands on a node of both layers. Wrong-way edges join neighbouring
//!    tracks at a shared stop. Node ids are dense per layer; the graph holds no
//!    per-edge storage.
//! 2. **Pin access** (conflict aware). Each terminal climbs from its pin
//!    shapes to an on-graph node of the access layer through a small local
//!    lattice (pin coordinates plus the nearby track coordinates), reusing the
//!    LEF-rule access table, via variants and same-pin notch checks. The most
//!    constrained terminals go first; a terminal without a legal escape rips
//!    up the escapes of nearby terminals, takes its own first and lets them
//!    re-search (bounded).
//! 3. **Global routing.** A GCell grid with per-edge track capacity measured
//!    against fixed geometry. Nets are routed tree-wise with A*; overflowing
//!    nets are ripped up and re-routed with present and history costs for a
//!    bounded number of iterations (negotiated congestion). The dilated GCell
//!    set of each net is its guide.
//! 4. **Detailed routing.** A* on the track graph inside the guide (then an
//!    expanded box) with every wire and via checked against obstacles and
//!    other nets (`Spatial`). Nets that still fail are re-routed in a soft mode
//!    in which routed metal of other nets is passable at a cost; the nets it
//!    crosses are ripped up and queued, with node history raising the price of
//!    contested places (bounded rounds and rip-ups per net).
//! 5. **Output.** Wires are maximal collinear runs split at junctions, then the
//!    bounded same-net pass (`routing::cleanup`) and `routing::validate` run.
//!
//! Every search, queue and table is capped; see docs/scaling.md for the
//! memory estimate that `check_plan` and `route` enforce before allocating.
#[path = "route_cycle_repair.rs"]
mod cycle_repair;
#[path = "route_owner_repair.rs"]
mod owner_repair;
#[path = "route_replay.rs"]
pub(crate) mod replay;

use crate::route_rules;
use crate::route_search::{History, Search, State};
use crate::routing::{
    Access, AccessEntry, Blockage, BuildFx, LayerRect, RouteInput, RouteLimits, RouteStats, Routes,
    Soft, Spatial, Terminal, TrackPlan, Via, ViaPair, WIRE, Wire, access_table_at, budget,
    contains, distance, expand, int, invalid, no_route, quartile_points, shifted, validate,
    via_pairs, wire_rect,
};
use crate::{Rect, Result};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

const NONE: u32 = u32::MAX;
/// Bytes per graph node held by the detailed search (cost, parent, move,
/// epoch tag, history).
const NODE_BYTES: u64 = 24;
/// Bytes per GCell (search state and guide marks) and per GCell edge.
const GCELL_BYTES: u64 = 56;
/// Bytes per spatial shape: the blockage (48), query mark (8), alive and
/// routed flags, rounded up.
const SHAPE_BYTES: u64 = 64;
/// Bytes per spatial bin entry: 8, doubled for vector growth.
const ENTRY_BYTES: u64 = 16;
/// Bytes per grid bin of one layer (head and tail entry index).
const GRID_BYTES: u64 = 8;
/// Shapes the router adds per terminal (escape, routed runs and vias,
/// ripped-up shapes that stay indexed; measured 13-16), and bin entries per
/// added shape (measured 2.4-4 over all shapes).
const SHAPES_PER_TERMINAL: u64 = 20;
const ENTRIES_PER_SHAPE: u64 = 4;
/// Output records per terminal (wires, vias, patches, access; measured
/// 5.2-7.0) and bytes per record across the output and the same-net groups.
const RECORDS_PER_TERMINAL: u64 = 10;
const RECORD_BYTES: u64 = 128;
/// Escape plans, hash/tree metadata, groups, guides and queues. Completed
/// wire/via vectors and the active net's raw topology are counted separately.
const ROUTED_BYTES_PER_TERMINAL: u64 = 768 - 3 * std::mem::size_of::<usize>() as u64;
const ACTIVE_TOPOLOGY_BYTES: u64 = 172;
const MAX_VIA_PAD_ENTRIES: usize = 1_000_000;
const MAX_TARGET_PROJECTIONS_PER_SINK: usize = 1024;
/// Preserve the existing new-owner price while evaluating same-round owner reuse.
/// Lowering this independently can accelerate searches but increase queue growth.
const NEW_OWNER_VIAS: u64 = 80;
/// A constrained trial should prefer short clear detours before displacing its owner.
const SINGLE_OWNER_VIAS: u64 = 4;
/// A scout may use less than, but never more than, the configured search budget.
const SOFT_SCOUT_EXPANSIONS: usize = 250_000;
/// Recover clear sink prefixes without unbounded exhaustive searches.
const REUSE_ONLY_EXPANSIONS: usize = 10_000;
/// Sorted window index plus construction headroom (one entry per terminal).
const WINDOW_INDEX_BYTES: u64 = 32;
/// Net-aware pin access: extra escape cost (DBU-equivalent) a top may have
/// over the cheapest one and still be chosen for being nearer the net.
const ACCESS_HINT_SLACK: u64 = 840;
/// Net-aware pin access looks at nets of at most this many terminals.
const ACCESS_HINT_TERMINALS: usize = 8;
type AccessKey = (u32, Vec<(u32, [i64; 2])>);
type AccessTables = BTreeMap<AccessKey, Vec<AccessEntry>>;

/// Inputs of one pin-access pass (`Router::reserve_pass`).
#[derive(Clone, Copy)]
struct AccessPass<'p, 'a> {
    terminals: &'p [&'a Terminal],
    /// Terminal indices, most constrained first.
    order: &'p [usize],
    /// Repair window of each terminal.
    windows: &'p [Rect],
    /// Net members for net-aware access (empty: cheapest tops only).
    by_net: &'p HashMap<u32, Vec<usize>, BuildFx>,
    started: std::time::Instant,
}

fn access_key(class: u32, origin: [i64; 2], points: &[(u32, [i64; 2])]) -> Result<AccessKey> {
    Ok((
        class,
        points
            .iter()
            .map(|&(layer, p)| {
                Ok((
                    layer,
                    [
                        int(i128::from(p[0]) - i128::from(origin[0]))?,
                        int(i128::from(p[1]) - i128::from(origin[1]))?,
                    ],
                ))
            })
            .collect::<Result<_>>()?,
    ))
}

/// A driver at a remote port need not be the geometric root of an undirected
/// wire tree. During repair, join central internal terminals first and ports
/// last. Reuse the group storage and charge partitioning plus bounded sorts.
fn repair_terminal_order(
    group: &mut [&Terminal],
    point: impl Fn(&Terminal) -> [i64; 2],
    work: &mut usize,
    cap: usize,
) -> Result<()> {
    if group.len() < 2 {
        return Ok(());
    }
    let cost = group
        .len()
        .checked_mul(group.len().ilog2() as usize + 1)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| budget("repair terminal-order work"))?;
    *work = work
        .checked_add(cost)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("repair terminal-order work"))?;
    let mut internal = 0;
    for i in 0..group.len() {
        if group[i].class.is_some() {
            group.swap(i, internal);
            internal += 1;
        }
    }
    let central = if internal == 0 { group.len() } else { internal };
    group[..central].sort_unstable_by_key(|t| (point(t)[0], t.pin));
    let x = point(group[central / 2])[0];
    group[..central].sort_unstable_by_key(|t| (point(t)[1], t.pin));
    let y = point(group[central / 2])[1];
    group.sort_unstable_by_key(|t| {
        let p = point(t);
        (
            internal != 0 && t.class.is_none(),
            p[0].abs_diff(x).saturating_add(p[1].abs_diff(y)),
            !t.driver,
            t.pin,
        )
    });
    Ok(())
}

/// Prim connection order for the initial detailed route (Prim 1957, "Shortest
/// connection networks and some generalizations"): `group[0]` (the driver)
/// stays first; every later position takes the remaining terminal whose
/// point is nearest (L1) to any terminal already placed, ties broken by pin
/// id. A sequential router that joins each sink to the tree built so far then
/// grows a spanning tree close to the minimum one instead of zig-zagging in
/// pin-id order. Pure function of the points and pin ids; O(k^2) work, charged.
fn prim_terminal_order(
    group: &mut [&Terminal],
    point: impl Fn(&Terminal) -> [i64; 2],
    work: &mut usize,
    cap: usize,
) -> Result<()> {
    let k = group.len();
    if k < 3 {
        return Ok(());
    }
    *work = work
        .checked_add(
            k.checked_mul(k)
                .ok_or_else(|| budget("prim terminal-order work"))?,
        )
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("prim terminal-order work"))?;
    let points: Vec<[i64; 2]> = group.iter().map(|t| point(t)).collect();
    let l1 = |a: [i64; 2], b: [i64; 2]| a[0].abs_diff(b[0]).saturating_add(a[1].abs_diff(b[1]));
    // order[i]: index into `group` of the terminal at position i.
    let mut order: Vec<usize> = (0..k).collect();
    let mut best: Vec<u64> = points.iter().map(|&p| l1(p, points[0])).collect();
    for position in 1..k {
        let mut pick = position;
        for candidate in position + 1..k {
            let (c, p) = (order[candidate], order[pick]);
            if (best[c], group[c].pin) < (best[p], group[p].pin) {
                pick = candidate;
            }
        }
        order.swap(position, pick);
        let joined = points[order[position]];
        for &rest in &order[position + 1..] {
            best[rest] = best[rest].min(l1(points[rest], joined));
        }
    }
    let original: Vec<&Terminal> = group.to_vec();
    for (slot, index) in group.iter_mut().zip(order) {
        *slot = original[index];
    }
    Ok(())
}

fn alternate_repair_root(
    group: &mut [&Terminal],
    point: impl Fn(&Terminal) -> [i64; 2],
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    *work = work
        .checked_add(group.len())
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("alternate-root work"))?;
    let Some(first) = group.first() else {
        return Ok(false);
    };
    let origin = point(first);
    let best = group
        .iter()
        .enumerate()
        .skip(1)
        .max_by_key(|(_, t)| {
            let at = point(t);
            (
                t.class.is_some(),
                t.driver,
                origin[0]
                    .abs_diff(at[0])
                    .saturating_add(origin[1].abs_diff(at[1])),
                Reverse(t.pin),
            )
        })
        .map(|(i, _)| i);
    if let Some(i) = best {
        group.swap(0, i);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn alternate_repair_root_by_distance(
    group: &mut [&Terminal],
    point: impl Fn(&Terminal) -> [i64; 2],
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    *work = work
        .checked_add(group.len())
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("alternate-root work"))?;
    let Some(first) = group.first() else {
        return Ok(false);
    };
    let origin = point(first);
    let best = group
        .iter()
        .enumerate()
        .skip(1)
        .max_by_key(|(_, t)| {
            let at = point(t);
            (
                t.class.is_some(),
                origin[0]
                    .abs_diff(at[0])
                    .saturating_add(origin[1].abs_diff(at[1])),
                t.driver,
                Reverse(t.pin),
            )
        })
        .map(|(i, _)| i);
    if let Some(i) = best {
        group.swap(0, i);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Immutable x-interval index for pin-access repair. A query scans only
/// lower bounds in [query.lx - widest_window, query.ux], then checks y and
/// exact overlap. Every candidate and the one-time build are work charged.
struct WindowIndex {
    order: Vec<(i64, usize)>,
    width: i64,
}
impl WindowIndex {
    fn new(windows: &[Rect], work: &mut usize, cap: usize) -> Result<Self> {
        *work = work
            .checked_add(windows.len())
            .filter(|&n| n <= cap)
            .ok_or_else(|| budget("pin access window-index work"))?;
        let mut order = Vec::new();
        order
            .try_reserve_exact(windows.len())
            .map_err(|_| budget("pin access window index"))?;
        let mut width = 0;
        for (i, r) in windows.iter().enumerate() {
            width = width.max(
                r.ux.checked_sub(r.lx)
                    .ok_or_else(|| budget("pin access window span"))?,
            );
            order.push((r.lx, i));
        }
        order.sort_unstable();
        Ok(Self { order, width })
    }
    fn overlapping(
        &self,
        windows: &[Rect],
        query: Rect,
        work: &mut usize,
        cap: usize,
    ) -> Result<Vec<usize>> {
        let lo = query.lx.saturating_sub(self.width);
        let begin = self.order.partition_point(|&(x, _)| x < lo);
        let end = self.order.partition_point(|&(x, _)| x <= query.ux);
        *work = work
            .checked_add(end - begin)
            .filter(|&n| n <= cap)
            .ok_or_else(|| budget("pin access window-query work"))?;
        Ok(self.order[begin..end]
            .iter()
            .filter_map(|&(_, i)| {
                let r = windows[i];
                (r.lx <= query.ux && query.lx <= r.ux && r.ly <= query.uy && query.ly <= r.uy)
                    .then_some(i)
            })
            .collect())
    }
}

fn access_cache_limit(plan: &TrackPlan) -> u64 {
    (plan.memory_mib.saturating_mul(1 << 20) / 32).clamp(1 << 16, 64 << 20)
}

/// Upfront whole-route memory estimate (bytes) of the track router.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct MemoryEstimate {
    /// Held by the caller before routing (`RouteInput::held_bytes`).
    pub held: u64,
    /// Detailed-search arrays and GCell state.
    pub search: u64,
    /// The router's spatial index (grid, fixed and routed shapes, entries).
    pub spatial: u64,
    /// Routed state (per-net edges, escape plans, guides).
    pub routed: u64,
    /// Rule repair and validation: a second index over fixed shapes and the
    /// result, same-net groups and the output records.
    pub check: u64,
}
impl MemoryEstimate {
    pub fn total(&self) -> u64 {
        self.held
            .saturating_add(self.search)
            .saturating_add(self.spatial)
            .saturating_add(self.routed)
            .saturating_add(self.check)
    }
}

struct GLayer {
    layer: u32,
    vertical: bool,
    tracks: Vec<i64>,
    stops: Vec<i64>,
    base: usize,
}
struct GPair {
    lower: usize,
    upper: usize,
    kinds: Vec<usize>,
    lo_stop_up_track: Vec<u32>,
    lo_track_up_stop: Vec<u32>,
    up_stop_lo_track: Vec<u32>,
    up_track_lo_stop: Vec<u32>,
}
struct Graph {
    layers: Vec<GLayer>,
    slot: HashMap<u32, usize, BuildFx>,
    pairs: Vec<GPair>,
    len: usize,
}

/// Track coordinates of one layer inside the die (wire fully inside).
fn layer_tracks(input: &RouteInput, layer: u32, pitch: i64, offset: i64) -> Result<Vec<i64>> {
    let l = &input.layers[layer as usize];
    let vertical = l
        .vertical
        .ok_or_else(|| invalid("track layer is a cut layer"))?;
    let (lo, hi) = if vertical {
        (input.die.lx, input.die.ux)
    } else {
        (input.die.ly, input.die.uy)
    };
    let half = l.width / 2;
    let first = i128::from(lo) + i128::from(half);
    let k0 = (first - i128::from(offset) + i128::from(pitch) - 1).div_euclid(i128::from(pitch));
    let mut out = Vec::new();
    let mut k = k0;
    loop {
        let c = i128::from(offset) + k * i128::from(pitch);
        if c + i128::from(half) > i128::from(hi) {
            break;
        }
        out.push(int(c)?);
        if out.len() > (1 << 20) {
            return Err(budget("track count per layer"));
        }
        k += 1;
    }
    Ok(out)
}

fn index_of(v: &[i64], x: i64) -> u32 {
    v.binary_search(&x).map_or(NONE, |i| i as u32)
}

impl Graph {
    fn hop_distances(&self) -> Result<Vec<Vec<u16>>> {
        let n = self.layers.len();
        if n > 64 {
            return Err(budget("pin-access layer-hop table"));
        }
        let mut hops = vec![vec![u16::MAX; n]; n];
        for (i, row) in hops.iter_mut().enumerate() {
            row[i] = 0;
        }
        for pair in &self.pairs {
            hops[pair.lower][pair.upper] = 1;
            hops[pair.upper][pair.lower] = 1;
        }
        for via in 0..n {
            for from in 0..n {
                for to in 0..n {
                    hops[from][to] =
                        hops[from][to].min(hops[from][via].saturating_add(hops[via][to]));
                }
            }
        }
        Ok(hops)
    }
    fn new(input: &RouteInput, plan: &TrackPlan, limits: &RouteLimits) -> Result<Self> {
        let mut layers = Vec::new();
        let mut slot = HashMap::with_hasher(BuildFx);
        for (i, l) in input.layers.iter().enumerate() {
            let Some(vertical) = l.vertical else {
                continue;
            };
            let (pitch, offset) = plan
                .tracks
                .get(i)
                .copied()
                .flatten()
                .ok_or_else(|| invalid("routing layer without track pitch"))?;
            slot.insert(i as u32, layers.len());
            layers.push(GLayer {
                layer: i as u32,
                vertical,
                tracks: layer_tracks(input, i as u32, pitch, offset)?,
                stops: Vec::new(),
                base: 0,
            });
        }
        let mut pairs = Vec::new();
        for p in via_pairs(input) {
            let (lo, up) = (slot[&p.lower], slot[&p.upper]);
            if layers[lo].vertical == layers[up].vertical {
                return Err(invalid(
                    "track router needs alternating layer directions per via pair",
                ));
            }
            pairs.push(GPair {
                lower: lo,
                upper: up,
                kinds: p.kinds,
                lo_stop_up_track: Vec::new(),
                lo_track_up_stop: Vec::new(),
                up_stop_lo_track: Vec::new(),
                up_track_lo_stop: Vec::new(),
            });
        }
        let mut stops: Vec<BTreeSet<i64>> = vec![BTreeSet::new(); layers.len()];
        for p in &pairs {
            stops[p.lower].extend(layers[p.upper].tracks.iter().copied());
            stops[p.upper].extend(layers[p.lower].tracks.iter().copied());
        }
        let mut len = 0usize;
        for (l, s) in layers.iter_mut().zip(stops) {
            if s.is_empty() || l.tracks.is_empty() {
                return Err(invalid("track layer without tracks or via stops"));
            }
            l.stops = s.into_iter().collect();
            l.base = len;
            len = len
                .checked_add(
                    l.tracks
                        .len()
                        .checked_mul(l.stops.len())
                        .ok_or_else(|| budget("track graph size overflow"))?,
                )
                .ok_or_else(|| budget("track graph size overflow"))?;
            if len > limits.max_grid_nodes || len >= NONE as usize {
                return Err(budget("routing grid node count"));
            }
        }
        for p in &mut pairs {
            let (lo, up) = (&layers[p.lower], &layers[p.upper]);
            p.lo_stop_up_track = lo.stops.iter().map(|&c| index_of(&up.tracks, c)).collect();
            p.lo_track_up_stop = lo.tracks.iter().map(|&c| index_of(&up.stops, c)).collect();
            p.up_stop_lo_track = up.stops.iter().map(|&c| index_of(&lo.tracks, c)).collect();
            p.up_track_lo_stop = up.tracks.iter().map(|&c| index_of(&lo.stops, c)).collect();
        }
        Ok(Self {
            layers,
            slot,
            pairs,
            len,
        })
    }
    fn locate(&self, id: u32) -> (usize, usize, usize) {
        let i = id as usize;
        let l = self.layers.partition_point(|g| g.base <= i) - 1;
        let g = &self.layers[l];
        let k = i - g.base;
        (l, k / g.stops.len(), k % g.stops.len())
    }
    fn xy(&self, l: usize, t: usize, s: usize) -> [i64; 2] {
        let g = &self.layers[l];
        if g.vertical {
            [g.tracks[t], g.stops[s]]
        } else {
            [g.stops[s], g.tracks[t]]
        }
    }
    fn point(&self, id: u32) -> (u32, [i64; 2]) {
        let (l, t, s) = self.locate(id);
        (self.layers[l].layer, self.xy(l, t, s))
    }
    fn node(&self, l: usize, t: usize, s: usize) -> u32 {
        (self.layers[l].base + t * self.layers[l].stops.len() + s) as u32
    }
    fn find(&self, l: usize, p: [i64; 2]) -> Option<u32> {
        let g = &self.layers[l];
        let (c, a) = if g.vertical {
            (p[0], p[1])
        } else {
            (p[1], p[0])
        };
        let t = g.tracks.binary_search(&c).ok()?;
        let s = g.stops.binary_search(&a).ok()?;
        Some(self.node(l, t, s))
    }

    /// Add the two stops bracketing the root's preferred-axis coordinate.
    /// These are ordinary macro wires; callers run the same whole-span checks.
    fn target_neighbors(&self, id: u32, target: [i64; 2], out: &mut Vec<(u32, Move)>) -> usize {
        let (layer, track, _) = self.locate(id);
        let g = &self.layers[layer];
        let at = target[usize::from(g.vertical)];
        let right = g.stops.partition_point(|&p| p < at);
        let work = g.stops.len().max(1).ilog2() as usize + 1 + 2 * out.len();
        for stop in [
            right.checked_sub(1),
            (right < g.stops.len()).then_some(right),
        ]
        .into_iter()
        .flatten()
        {
            let next = self.node(layer, track, stop);
            if next != id && !out.iter().any(|&(n, _)| n == next) {
                out.push((next, Move::Along));
            }
        }
        work
    }
    /// Neighbours: (node, move) with move = WIRE (preferred), WRONG or via
    /// pair index. Writes into `out` and returns the count.
    fn neighbors(&self, id: u32, out: &mut Vec<(u32, Move)>) {
        out.clear();
        let (l, t, s) = self.locate(id);
        let g = &self.layers[l];
        if s > 0 {
            out.push((id - 1, Move::Along));
        }
        if s + 1 < g.stops.len() {
            out.push((id + 1, Move::Along));
        }
        // Additional straight, fully checked moves accelerate large open
        // channels. Unit moves remain available for every turn and obstacle.
        for jump in [16usize, 64, 256] {
            if s >= jump {
                out.push((id - jump as u32, Move::Along));
            }
            if s + jump < g.stops.len() {
                out.push((id + jump as u32, Move::Along));
            }
        }
        let n = g.stops.len() as u32;
        if t > 0 {
            out.push((id - n, Move::Wrong));
        }
        if t + 1 < g.tracks.len() {
            out.push((id + n, Move::Wrong));
        }
        for (pi, p) in self.pairs.iter().enumerate() {
            if p.lower == l {
                let (ut, us) = (p.lo_stop_up_track[s], p.lo_track_up_stop[t]);
                if ut != NONE && us != NONE {
                    out.push((self.node(p.upper, ut as usize, us as usize), Move::Via(pi)));
                }
            } else if p.upper == l {
                let (lt, ls) = (p.up_stop_lo_track[s], p.up_track_lo_stop[t]);
                if lt != NONE && ls != NONE {
                    out.push((self.node(p.lower, lt as usize, ls as usize), Move::Via(pi)));
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Move {
    Along,
    Wrong,
    Via(usize),
}

/// Quartile landing points of many-rectangle pins, bounded: the centre of
/// every rectangle first, then the other quartile points, at most 128.
fn bounded_points(
    shapes: &[LayerRect],
    grid: i64,
    die: Option<Rect>,
) -> Result<Vec<(u32, [i64; 2])>> {
    if let Ok(points) = quartile_points(shapes, grid, die) {
        return Ok(points);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for pass in 0..2 {
        for s in shapes {
            let all = quartile_points(std::slice::from_ref(s), grid, die)?;
            let pick: Vec<_> = if pass == 0 {
                all.into_iter().take(1).collect()
            } else {
                all.into_iter().skip(1).collect()
            };
            for p in pick {
                if out.len() < 128 && seen.insert(p) {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

/// Validate the plan against the input (called from `routing::preflight`).
pub(crate) fn check_plan(input: &RouteInput, plan: &TrackPlan) -> Result<()> {
    if !input.rules {
        return Err(invalid("track router requires LEF-rule mode"));
    }
    if input.layers.len().saturating_mul(input.vias.len()) > MAX_VIA_PAD_ENTRIES {
        return Err(budget("via-pad area table"));
    }
    if let Some(config) = &plan.single_owner_repair {
        config.validate(&RouteLimits::ceiling())?;
    }
    if let Some([lower, upper]) = plan.routing_layers
        && (lower > upper
            || [lower, upper].iter().any(|&layer| {
                input
                    .layers
                    .get(layer as usize)
                    .is_none_or(|l| l.vertical.is_none())
            })
            || !plan.allows_routing_layer(input.access_layer))
    {
        return Err(invalid(
            "signal-routing layer range or access layer outside range",
        ));
    }
    if plan.tracks.len() != input.layers.len()
        || plan.gcell <= 0
        || plan.global_iterations > 1000
        || plan.capacity_percent == 0
        || plan.capacity_percent > 100
        || plan.guide_margin > 64
        || plan.expand_margin > 256
        || plan.max_ripups_per_net > 10_000
        || plan.max_repair_rounds > 10_000_000
        || plan.via_cost == 0
        || plan.via_cost > 1 << 40
        || plan.wrong_way_factor == 0
        || plan.wrong_way_factor > 1000
        || !(1..=16).contains(&plan.heuristic_weight)
        || plan
            .repair_heuristic_weight
            .is_some_and(|weight| !(1..=16).contains(&weight))
        || plan.memory_mib == 0
        || plan.memory_mib > 65_536
        || plan.max_history_entries == 0
        || plan.max_history_entries > 64_000_000
    {
        return Err(invalid("track plan parameters"));
    }
    if let Some(qor) = &plan.qor {
        qor.validate(input.layers.len())?;
    }
    for (l, t) in input.layers.iter().zip(&plan.tracks) {
        match (l.vertical, t) {
            (Some(_), Some((pitch, offset))) => {
                if *pitch <= 0
                    || pitch % input.grid != 0
                    || offset % input.grid != 0
                    || *pitch < l.width
                {
                    return Err(invalid("track pitch/offset off the manufacturing grid"));
                }
            }
            (None, None) => {}
            _ => return Err(invalid("track plan layer kinds")),
        }
    }
    Ok(())
}

/// Number of track-graph nodes (validate() telemetry; no search state).
pub(crate) fn graph_nodes(input: &RouteInput, limits: &RouteLimits) -> Result<usize> {
    let plan = input
        .tracks
        .as_ref()
        .ok_or_else(|| invalid("track plan absent"))?;
    Ok(Graph::new(input, plan, limits)?.len)
}

/// Upfront whole-route estimate for this input (graph `nodes`, `gcells`),
/// computed without allocating router state: fixed bin entries are counted
/// shape by shape, the bin grid from the die and obstacle extent.
fn memory_estimate_limited(
    input: &RouteInput,
    nodes: usize,
    gcells: usize,
    limits: &RouteLimits,
) -> Result<MemoryEstimate> {
    let bin = crate::routing::bin_side(input)?;
    let (x0, x1, y0, y1, cells) = crate::routing::bin_extent(input, bin);
    let layers = input.layers.len() as u64;
    let slots = cells.saturating_mul(layers);
    let mode = crate::routing::bin_mode(input, cells);
    let page_side = crate::routing::BIN_PAGE_SIDE;
    let page_x = (x1.div_euclid(page_side) - x0.div_euclid(page_side) + 1) as u64;
    let page_y = (y1.div_euclid(page_side) - y0.div_euclid(page_side) + 1) as u64;
    let pages = page_x.saturating_mul(page_y).saturating_mul(layers);

    let mut fixed_entries = 0u64;
    for b in &input.obstacles {
        let span = |lo: i64, hi: i64| (hi.div_euclid(bin) - lo.div_euclid(bin) + 1).max(0) as u64;
        fixed_entries = fixed_entries
            .saturating_add(span(b.rect.lx, b.rect.ux).saturating_mul(span(b.rect.ly, b.rect.uy)));
    }
    let instance_index = crate::routing::instance_index_bytes(input, limits)?;
    let o = input.obstacles.len() as u64;
    let t = input.terminals.len() as u64;
    let fixed_records = input
        .obstacles
        .len()
        .saturating_add(input.instances.as_ref().map_or(0, |i| i.len()));
    let portal_shapes = input
        .vias
        .iter()
        .map(|v| v.shapes.len() as u64)
        .max()
        .unwrap_or(0);
    let added = t
        .saturating_mul(SHAPES_PER_TERMINAL.saturating_add(portal_shapes))
        .min(limits.max_obstacles.saturating_sub(fixed_records) as u64);
    let index = |shapes: u64| {
        // Both Spatial implementations enforce this before every insertion.
        // The checker cannot retain 2*max_segments shapes past this limit.
        let shapes = shapes.min(limits.max_obstacles.saturating_sub(fixed_records) as u64);
        let entries = fixed_entries.saturating_add(shapes.saturating_mul(ENTRIES_PER_SHAPE));
        let grid = match mode {
            crate::routing::BinMode::Sparse => slots.min(entries).saturating_mul(96),
            crate::routing::BinMode::Paged => pages.min(entries).saturating_mul(8320),
            crate::routing::BinMode::Dense => slots.saturating_mul(GRID_BYTES),
        };
        // Fixed rectangles are shared with RouteInput; only their query marks
        // are additional. Added geometry retains its full state allowance.
        grid.saturating_add(instance_index)
            .saturating_add(o.saturating_mul(16))
            .saturating_add(shapes.saturating_mul(SHAPE_BYTES))
            .saturating_add(
                fixed_entries
                    .saturating_add(shapes.saturating_mul(ENTRIES_PER_SHAPE))
                    .saturating_mul(ENTRY_BYTES),
            )
    };
    let records = t
        .saturating_mul(RECORDS_PER_TERMINAL)
        .min(limits.max_segments as u64);
    let (mut width, mut height) = (0i128, 0i128);
    for terminal in &input.terminals {
        if let Some(first) = terminal.shapes.first() {
            let (mut lx, mut ly, mut ux, mut uy) =
                (first.rect.lx, first.rect.ly, first.rect.ux, first.rect.uy);
            for s in &terminal.shapes[1..] {
                lx = lx.min(s.rect.lx);
                ly = ly.min(s.rect.ly);
                ux = ux.max(s.rect.ux);
                uy = uy.max(s.rect.uy);
            }
            width = width.max(i128::from(ux) - i128::from(lx));
            height = height.max(i128::from(uy) - i128::from(ly));
        }
    }
    // Only one escape's potential is live. Its axes consist of grid points
    // within the existing four-step halo, plus at most two end positions.
    let halo = i128::from(input.step.max(0)).saturating_mul(8);
    let grid = i128::from(input.grid.max(1));
    let axis_points = (width + halo) / grid + (height + halo) / grid + 4;
    let access_memory = u64::try_from(axis_points)
        .unwrap_or(u64::MAX)
        .saturating_mul(8)
        .saturating_add(layers.saturating_mul(layers).saturating_mul(2))
        .saturating_add(layers.saturating_mul(32));
    let shapes_per_record = input
        .vias
        .iter()
        .map(|v| v.shapes.len() as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let rule_shapes = records.saturating_mul(shapes_per_record).saturating_add(
        input
            .terminals
            .iter()
            .map(|t| t.shapes.len() as u64)
            .sum::<u64>(),
    );
    let rule_grid_cells =
        (limits.max_rule_grid_cells as u64).min(rule_shapes.saturating_mul(2).saturating_pow(2));
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        eprintln!(
            "[troute] estimate inputs grid_cells {cells} layers {layers} fixed_entries {fixed_entries} obstacles {o} terminals {t} nodes {nodes} gcells {gcells} held {}",
            input.held_bytes
        );
    }
    Ok(MemoryEstimate {
        held: input.held_bytes,
        search: (if nodes <= crate::route_search::DENSE_NODES {
            (nodes as u64).saturating_mul(NODE_BYTES)
        } else {
            // Paged records keep at most 32 B per record of page storage
            // (`route_search::page_budget`); records beyond it overflow to a
            // hash map, as all sparse records did before pages.
            (nodes.min(limits.max_search_expansions.saturating_add(limits.max_heap)) as u64)
                .saturating_mul(64)
        })
        .saturating_add((nodes.min(limits.max_heap) as u64).saturating_mul(128))
        .saturating_add((gcells as u64).saturating_mul(GCELL_BYTES))
        .saturating_add(access_memory)
        .saturating_add(
            if input
                .tracks
                .as_ref()
                .is_some_and(|p| p.own_pin_index.is_some())
            {
                input.layers.len() as u64 * 64
            } else {
                0
            },
        )
        .saturating_add(layers.saturating_mul(input.vias.len() as u64 + 32))
        .saturating_add(crate::routing_target::memory_bytes(
            limits.max_target_segments,
        ))
        .saturating_add(
            input
                .tracks
                .as_ref()
                .and_then(|p| p.single_owner_repair.as_ref())
                .map_or(0, |config| {
                    config.working_bytes(
                        limits,
                        input.vias.iter().map(|v| v.shapes.len()).max().unwrap_or(1),
                    )
                }),
        ),
        spatial: index(added),
        routed: t
            // Bounded set of nets already considered for local cycle repair.
            .saturating_mul(ROUTED_BYTES_PER_TERMINAL + 64)
            .saturating_add(
                t.saturating_mul(portal_shapes.saturating_add(1))
                    .saturating_mul(8),
            )
            .saturating_add(t.saturating_mul(WINDOW_INDEX_BYTES))
            .saturating_add(records.saturating_mul(96))
            .saturating_add(
                (nodes.min(limits.max_segments).min(limits.max_active_nodes) as u64)
                    .saturating_mul(ACTIVE_TOPOLOGY_BYTES),
            )
            .saturating_add(access_cache_limit(
                input.tracks.as_ref().expect("track plan"),
            ))
            .saturating_add(if nodes <= crate::route_search::DENSE_NODES {
                0
            } else {
                (nodes
                    .min(limits.max_expansions)
                    .min(input.tracks.as_ref().unwrap().max_history_entries)
                    as u64)
                    .saturating_mul(32)
            }),
        check: index(records.saturating_mul(2))
            .saturating_add(records.saturating_mul(RECORD_BYTES))
            .saturating_add(rule_grid_cells.saturating_mul(64))
            .saturating_add(if input.layers.iter().any(|l| l.wide_spacing.is_some()) {
                // One retained Rect plus the worst-case component Vec and
                // its capacity growth; storage is shared across threads.
                rule_shapes
                    .min(limits.max_obstacles as u64)
                    .saturating_mul(96)
                    .saturating_add(16 * 1024 * 1024)
            } else {
                0
            }),
    })
}

/// `memory_estimate` for a track-router input (graph and GCell counts
/// derived as `route` derives them); `None` without a track plan.
pub(crate) fn estimate_for(
    input: &RouteInput,
    limits: &RouteLimits,
) -> Result<Option<MemoryEstimate>> {
    let Some(plan) = input.tracks.as_ref() else {
        return Ok(None);
    };
    let graph = Graph::new(input, plan, limits)?;
    let cells = Cells::new(input.die, plan.gcell)?;
    memory_estimate_limited(input, graph.len, cells.len(), limits).map(Some)
}

/// Extra validation storage once completed output records are already resident.
pub(crate) fn validation_working_bytes(
    input: &RouteInput,
    out: &Routes,
    limits: &RouteLimits,
) -> Result<u64> {
    let estimate = estimate_for(input, limits)?.ok_or_else(|| invalid("track plan absent"))?;
    let assumed_records = (input.terminals.len() as u64)
        .saturating_mul(RECORDS_PER_TERMINAL)
        .min(limits.max_segments as u64);
    let records = out
        .wires
        .len()
        .saturating_add(out.vias.len())
        .saturating_add(out.patches.len())
        .saturating_add(out.access.len()) as u64;
    Ok(estimate
        .check
        .saturating_sub(assumed_records.saturating_mul(RECORD_BYTES))
        // Pin uniqueness, net counts and compact terminal coverage indices.
        .saturating_add((input.terminals.len() as u64).saturating_mul(32))
        .saturating_add(records.saturating_mul(24))
        .saturating_add(
            (limits.max_active_nodes as u64)
                .saturating_mul(16)
                .saturating_mul(std::mem::size_of::<crate::route_rules::Shape>() as u64),
        )
        .saturating_add((limits.max_rule_violations as u64).saturating_mul(1024)))
}

/// GCell grid over the die.
struct Cells {
    lx: i64,
    ly: i64,
    g: i64,
    nx: usize,
    ny: usize,
}
impl Cells {
    fn new(die: Rect, g: i64) -> Result<Self> {
        let nx = usize::try_from(
            (i128::from(die.ux) - i128::from(die.lx) + i128::from(g) - 1) / i128::from(g),
        )
        .map_err(|_| budget("gcell count"))?;
        let ny = usize::try_from(
            (i128::from(die.uy) - i128::from(die.ly) + i128::from(g) - 1) / i128::from(g),
        )
        .map_err(|_| budget("gcell count"))?;
        if nx == 0 || ny == 0 || nx.saturating_mul(ny) > 16_000_000 {
            return Err(budget("gcell grid size"));
        }
        Ok(Self {
            lx: die.lx,
            ly: die.ly,
            g,
            nx,
            ny,
        })
    }
    fn of(&self, p: [i64; 2]) -> u32 {
        let x = ((p[0] - self.lx) / self.g).clamp(0, self.nx as i64 - 1) as usize;
        let y = ((p[1] - self.ly) / self.g).clamp(0, self.ny as i64 - 1) as usize;
        (y * self.nx + x) as u32
    }
    fn len(&self) -> usize {
        self.nx * self.ny
    }
    fn xy(&self, c: u32) -> (usize, usize) {
        (c as usize % self.nx, c as usize / self.nx)
    }
    /// Horizontal-crossing edges first ((x,y)-(x+1,y)), then vertical.
    fn edges(&self) -> usize {
        (self.nx - 1) * self.ny + self.nx * (self.ny - 1)
    }
    fn edge(&self, a: u32, b: u32) -> usize {
        let ((ax, ay), (bx, by)) = (self.xy(a), self.xy(b));
        if ay == by {
            ay * (self.nx - 1) + ax.min(bx)
        } else {
            (self.nx - 1) * self.ny + ay.min(by) * self.nx + ax
        }
    }
    fn neighbors(&self, c: u32, out: &mut Vec<u32>) {
        out.clear();
        let (x, y) = self.xy(c);
        if x > 0 {
            out.push(c - 1);
        }
        if x + 1 < self.nx {
            out.push(c + 1);
        }
        if y > 0 {
            out.push(c - self.nx as u32);
        }
        if y + 1 < self.ny {
            out.push(c + self.nx as u32);
        }
    }
}

/// A committed pin escape: geometry plus the on-graph top node.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Plan {
    access: Access,
    top: u32,
    /// A further via at `top` would stack: the escape reaches it through a
    /// via, or its own top-layer metal is below the layer's AREA.
    top_via: bool,
    /// Search-only via envelopes at the end of `shapes`.
    portal_shapes: u8,
    wires: Box<[Wire]>,
    vias: Box<[Via]>,
    shapes: Box<[usize]>,
}

/// Local search lattice for one terminal's escape.
struct Local {
    xs: Vec<i64>,
    ys: Vec<i64>,
}
impl Local {
    fn id(&self, li: usize, p: [i64; 2]) -> Option<u32> {
        let x = self.xs.binary_search(&p[0]).ok()?;
        let y = self.ys.binary_search(&p[1]).ok()?;
        Some((li * self.xs.len() * self.ys.len() + y * self.xs.len() + x) as u32)
    }
    fn at(&self, id: u32) -> (usize, [i64; 2]) {
        let i = id as usize;
        let plane = self.xs.len() * self.ys.len();
        let k = i % plane;
        (
            i / plane,
            [self.xs[k % self.xs.len()], self.ys[k / self.xs.len()]],
        )
    }
}

fn nearest_axis(axis: &[i64], point: i64) -> u64 {
    let right = axis.partition_point(|&x| x < point);
    axis.get(right)
        .into_iter()
        .chain(right.checked_sub(1).and_then(|i| axis.get(i)))
        .map(|&x| point.abs_diff(x))
        .min()
        .unwrap_or(0)
}

struct AccessPotential {
    dx: Vec<u64>,
    dy: Vec<u64>,
    hops: Vec<u64>,
}
impl AccessPotential {
    fn cost(&self, id: u32) -> Result<u64> {
        let plane = self.dx.len() * self.dy.len();
        let cell = id as usize % plane;
        self.dx[cell % self.dx.len()]
            .checked_add(self.dy[cell / self.dx.len()])
            .and_then(|d| d.checked_add(self.hops[id as usize / plane]))
            .ok_or_else(|| budget("pin-access lower-bound cost"))
    }
}

struct Router<'a> {
    input: &'a RouteInput,
    plan: &'a TrackPlan,
    heuristic_weight_override: Option<u64>,
    close_expanded: bool,
    last_search_touched_guide: bool,
    query_indices_override: Option<crate::routing::QueryIndices>,
    /// Replay-only counterfactual; normal routing permits all routed owners.
    allowed_victim: Option<u32>,
    /// The partner a net yields to after a two-net rip cycle: the soft search
    /// treats that partner's metal as blocked instead of ripping it.
    protected: Option<u32>,
    trial_search_limit: Option<usize>,
    trial_expansion_limit: Option<usize>,
    deferred_history: Option<Vec<u32>>,
    /// Ordinary soft routing updates history immediately, recording old values
    /// only while a replaceable candidate is being constructed.
    recorded_history: Option<Vec<(u32, u16)>>,
    single_owner_stats: Option<crate::routing::SingleOwnerRepairStats>,
    limits: &'a RouteLimits,
    graph: Graph,
    access_hops: Vec<Vec<u16>>,
    target_costs: crate::routing_target::CostModel,
    /// Wire-step multiplier per input layer, thousandths (1000 = legacy).
    layer_milli: Vec<u64>,
    /// Via step cost per graph via pair (legacy: `via_cost` for every pair).
    pair_via: Vec<u64>,
    /// Same-layer turn surcharge (0 = legacy).
    bend_cost: u64,
    /// Terminals whose access portals were released (`RouteQor::release_portals`).
    portals_released: HashSet<u32, BuildFx>,
    /// Pin access of the current terminal: where the net's other terminals
    /// are (their escape tops once reserved, else their pin boxes). Empty
    /// keeps the legacy cheapest-top choice.
    access_hint: Vec<Rect>,
    /// Tests only: hinted terminals keep their hints after a failed escape
    /// (no fallback to the cheapest top), to reproduce the livelock the
    /// fallback removes.
    #[cfg(test)]
    keep_hints_after_repair: bool,
    /// Tests only: a failed net-aware access pass fails pin access instead
    /// of being redone without hints.
    #[cfg(test)]
    no_access_retry: bool,
    /// Net-aware pin-access passes that failed and were redone without hints.
    access_retries: usize,
    cells: Cells,
    spatial: Spatial<'a>,
    pairs: Vec<ViaPair>,
    slots: HashMap<u32, usize>,
    expansions: usize,
    // Detailed search state.
    search: Search,
    epoch: u32,
    history: History,
    gmark: Vec<u32>,
    gepoch: u32,
    /// Per input layer: may two vias stack through it (every via pad on the
    /// layer meets the LEF AREA alone)?
    stack_ok: Vec<bool>,
    /// Actual via-kind pad area certificates, indexed by layer then kind.
    via_pad_ok: Vec<Vec<bool>>,
    /// Per input layer: wire run (DBU) between an entering and a leaving via
    /// on a layer that is not `stack_ok` (AREA / width, rounded up).
    min_run: Vec<u64>,
    /// Minimum-run stack rule of the current net in ordinary searches:
    /// 0 off, 1 routed stacks (an escape stack only costs more), 2 both.
    area_level: u8,
    /// Escape search in progress requires AREA-complete top metal.
    escape_area: bool,
    access_entries: usize,
    access_cache_bytes: u64,
    /// Rule-repair notch zones by net: within them a relaxed net is searched
    /// with the strict same-net rule (no near misses), so a repeated notch or
    /// patched wide-metal pair cannot be rebuilt identically. Empty before
    /// rule repair; at most `MAX_NOTCH_ZONES` in total.
    notch_zones: BTreeMap<u32, Vec<LayerRect>>,
    /// Rule-repair exclusions by net. A search tests near misses only against
    /// committed metal, so a notch between two parts of one path survives
    /// strict re-routing unchanged. Own metal may not enter these rectangles;
    /// dropped if they wall the net in. Shares `MAX_NOTCH_ZONES`.
    own_exclusions: BTreeMap<u32, Vec<LayerRect>>,
    /// Trace mode: why prior nets were not kept.
    prior_rejects: Option<BTreeMap<String, usize>>,
}

/// Completed net geometry; the active search alone retains raw grid edges.
#[derive(Default, Clone)]
struct NetRoute {
    wires: Vec<Wire>,
    vias: Vec<Via>,
    shapes: Vec<usize>,
}
impl NetRoute {
    fn segments(&self) -> usize {
        self.wires.len().saturating_add(self.vias.len())
    }
}

fn retain_route(
    routes: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    net: u32,
    route: NetRoute,
    limit: usize,
) -> Result<()> {
    let count = retained_edges.saturating_sub(routes.get(&net).map_or(0, NetRoute::segments));
    let count = count
        .checked_add(route.segments())
        .filter(|&n| n <= limit)
        .ok_or_else(|| budget("retained routing-segment limit"))?;
    routes.insert(net, route);
    *retained_edges = count;
    Ok(())
}

fn remove_route(
    routes: &mut BTreeMap<u32, NetRoute>,
    retained_edges: &mut usize,
    net: u32,
) -> Option<NetRoute> {
    let route = routes.remove(&net)?;
    *retained_edges -= route.segments();
    Some(route)
}

enum Outcome {
    Routed(NetRoute, Vec<u32>),
    Failed(u32),
    Limited(u32),
}

impl<'a> Router<'a> {
    fn prepared(
        input: &'a RouteInput,
        limits: &'a RouteLimits,
        graph: Graph,
        cells: Cells,
    ) -> Result<Self> {
        let plan = input
            .tracks
            .as_ref()
            .ok_or_else(|| invalid("track plan absent"))?;
        let pairs = via_pairs(input);
        let mut slots = HashMap::new();
        let mut k = 0usize;
        for (i, l) in input.layers.iter().enumerate() {
            if l.vertical.is_some() {
                slots.insert(i as u32, k);
                k += 1;
            }
        }
        let n = graph.len;
        let gl = cells.len();
        let access_hops = graph.hop_distances()?;
        // Step prices: legacy unit multipliers unless quality controls set them.
        let layer_milli: Vec<u64> = (0..input.layers.len() as u32)
            .map(|l| plan.qor.as_ref().map_or(1000, |q| q.layer_milli(l)))
            .collect();
        let pair_via: Vec<u64> = graph
            .pairs
            .iter()
            .map(|p| {
                let upper = graph.layers[p.upper].layer;
                match plan.qor.as_ref().map(|q| q.via_milli(upper)) {
                    None | Some(1000) => plan.via_cost,
                    Some(milli) => plan.via_cost.saturating_mul(milli) / 1000,
                }
            })
            .collect();
        let target_costs = crate::routing_target::CostModel::weighted(
            &graph.layers.iter().map(|l| l.vertical).collect::<Vec<_>>(),
            &graph
                .pairs
                .iter()
                .zip(&pair_via)
                .map(|(p, &cost)| (p.lower, p.upper, cost))
                .collect::<Vec<_>>(),
            plan.wrong_way_factor,
            &graph
                .layers
                .iter()
                .map(|l| layer_milli[l.layer as usize])
                .collect::<Vec<_>>(),
        )?;
        let bend_cost = plan.qor.as_ref().map_or(0, |q| q.bend_cost);
        let mut r = Router {
            input,
            plan,
            heuristic_weight_override: None,
            close_expanded: plan
                .single_owner_repair
                .as_ref()
                .and_then(|c| c.heuristic_weight)
                .is_some()
                && input.terminals.len() > 64,
            last_search_touched_guide: false,
            query_indices_override: None,
            allowed_victim: None,
            protected: None,
            trial_search_limit: None,
            trial_expansion_limit: None,
            deferred_history: None,
            recorded_history: None,
            single_owner_stats: plan
                .single_owner_repair
                .as_ref()
                .map(|_| Default::default()),
            limits,
            graph,
            access_hops,
            target_costs,
            layer_milli,
            pair_via,
            bend_cost,
            portals_released: HashSet::with_hasher(BuildFx),
            access_hint: Vec::new(),
            #[cfg(test)]
            keep_hints_after_repair: false,
            #[cfg(test)]
            no_access_retry: false,
            access_retries: 0,
            cells,
            spatial: Spatial::new(input, limits)?,
            pairs,
            slots,
            expansions: 0,
            search: Search::new(
                n,
                limits
                    .max_search_expansions
                    .saturating_add(limits.max_heap)
                    .min(n),
            )?,
            epoch: 0,
            history: History::new(
                n,
                limits.max_expansions.min(n).min(plan.max_history_entries),
            )?,
            gmark: vec![0; gl],
            gepoch: 0,
            stack_ok: vec![true; input.layers.len()],
            via_pad_ok: vec![vec![true; input.vias.len()]; input.layers.len()],
            min_run: vec![0; input.layers.len()],
            area_level: if plan.area_stacks { 2 } else { 0 },
            escape_area: false,
            access_entries: 0,
            access_cache_bytes: 0,
            notch_zones: BTreeMap::new(),
            own_exclusions: BTreeMap::new(),
            prior_rejects: std::env::var_os("RUSTROAD_ROUTE_TRACE").map(|_| BTreeMap::new()),
        };
        for (i, l) in input.layers.iter().enumerate() {
            let Some(min) = l.min_area else {
                continue;
            };
            r.min_run[i] = u64::try_from(min)
                .unwrap_or(0)
                .div_ceil(l.width.max(1) as u64);
            for (kind, via) in input.vias.iter().enumerate() {
                let pads: Vec<_> = via
                    .shapes
                    .iter()
                    .filter(|s| s.layer == i as u32)
                    .map(|s| s.rect)
                    .collect();
                r.via_pad_ok[i][kind] = pads.is_empty()
                    || route_rules::union_area(&pads, &mut r.spatial.work, limits.max_comparisons)?
                        >= i128::from(min);
            }
            r.stack_ok[i] = r.via_pad_ok[i].iter().all(|&enough| enough);
        }
        Ok(r)
    }

    fn width(&self, layer: u32) -> i64 {
        self.input.layers[layer as usize].width
    }
    /// Drop the access portals of a routed net's terminals (opt-in). Each
    /// terminal is released once, so a shape slot reused after compaction is
    /// never removed by a later release.
    fn release_portals(&mut self, terminals: &[&Terminal], plans: &HashMap<u32, Plan>) {
        if !self
            .plan
            .qor
            .as_ref()
            .is_some_and(|q| q.release_portals || q.drop_portals)
        {
            return;
        }
        for t in terminals {
            let plan = &plans[&t.pin];
            if plan.portal_shapes == 0 || !self.portals_released.insert(t.pin) {
                continue;
            }
            for &id in plan
                .shapes
                .iter()
                .rev()
                .take(usize::from(plan.portal_shapes))
            {
                self.spatial.remove(id);
            }
        }
    }
    fn note_history(&mut self, node: u32) -> Result<()> {
        if let Some(pending) = &mut self.deferred_history {
            if pending.len() >= self.limits.max_active_nodes {
                return Err(budget("single-owner deferred history cap"));
            }
            pending
                .try_reserve(1)
                .map_err(|_| budget("single-owner deferred history allocation"))?;
            pending.push(node);
            Ok(())
        } else {
            if let Some(recorded) = &mut self.recorded_history {
                if recorded.len() >= self.limits.max_active_nodes {
                    return Err(budget("soft history journal cap"));
                }
                recorded
                    .try_reserve(1)
                    .map_err(|_| budget("soft history journal allocation"))?;
                recorded.push((node, self.history.get(node)));
            }
            self.history.increment(node)
        }
    }
    fn detail_weight(&self, soft: bool) -> u64 {
        self.heuristic_weight_override.unwrap_or_else(|| {
            if soft {
                self.plan
                    .repair_heuristic_weight
                    .unwrap_or(self.plan.heuristic_weight)
            } else {
                self.plan.heuristic_weight
            }
        })
    }
    fn escape_pad_ok(&self, plan: &Plan) -> bool {
        let (layer, at) = self.graph.point(plan.top);
        plan.vias.iter().any(|v| {
            v.at == at
                && self.input.vias[v.kind as usize]
                    .shapes
                    .iter()
                    .any(|s| s.layer == layer)
                && self.via_pad_ok[layer as usize][v.kind as usize]
        })
    }

    /// Protect one usable upward via while the rest of the pin escapes are
    /// reserved. Its envelope is search geometry only, never output metal.
    fn access_portal(
        &mut self,
        slot: usize,
        at: [i64; 2],
        net: u32,
        incoming_ok: bool,
    ) -> Result<Option<usize>> {
        let layer = self.graph.layers[slot].layer as usize;
        for pair in &self.graph.pairs {
            if pair.lower != slot || self.graph.find(pair.upper, at).is_none() {
                continue;
            }
            for &kind in &pair.kinds {
                if !incoming_ok && !self.via_pad_ok[layer][kind] {
                    continue;
                }
                if self.spatial.via_clear(kind, at, net)? {
                    return Ok(Some(kind));
                }
            }
        }
        Ok(None)
    }
    /// Runtime guard: the process's peak resident set (where the platform
    /// reports it) must stay within `memory_mib`; checked at phase
    /// boundaries and every 65536 expansions.
    fn guard(&self, what: &str) -> Result<()> {
        let cap = self.plan.memory_mib.saturating_mul(1 << 20);
        match crate::memory::peak_rss_bytes() {
            Some(rss) if rss > cap => Err(budget(&format!(
                "memory_mib {}: peak resident set {} MiB at {what}",
                self.plan.memory_mib,
                rss >> 20
            ))),
            _ => Ok(()),
        }
    }
    fn tick(&mut self, search: &mut usize, what: &str, pin: u32, net: u32) -> Result<()> {
        *search += 1;
        self.expansions += 1;
        if self.expansions.is_multiple_of(1 << 16) {
            self.guard(what)?;
        }
        if *search > self.limits.max_search_expansions {
            return Err(budget(&format!(
                "routing expansion limit: per-search {what}, pin {pin} net {net}"
            )));
        }
        if self.expansions > self.limits.max_expansions {
            return Err(budget(&format!(
                "routing expansion limit: total {what}, pin {pin} net {net}"
            )));
        }
        Ok(())
    }

    // ---------------------------------------------------------------- access

    /// On-graph points of the pin layers inside the shapes (absolute).
    fn graph_points(&self, shapes: &[LayerRect]) -> Vec<(u32, [i64; 2])> {
        let mut out = BTreeSet::new();
        for s in shapes {
            let Some(&l) = self.graph.slot.get(&s.layer) else {
                continue;
            };
            let g = &self.graph.layers[l];
            let (clo, chi, alo, ahi) = if g.vertical {
                (s.rect.lx, s.rect.ux, s.rect.ly, s.rect.uy)
            } else {
                (s.rect.ly, s.rect.uy, s.rect.lx, s.rect.ux)
            };
            let t0 = g.tracks.partition_point(|&c| c < clo);
            let t1 = g.tracks.partition_point(|&c| c <= chi);
            let s0 = g.stops.partition_point(|&c| c < alo);
            let s1 = g.stops.partition_point(|&c| c <= ahi);
            for t in t0..t1 {
                for st in s0..s1 {
                    if out.len() >= 256 {
                        break;
                    }
                    out.insert((s.layer, self.graph.xy(l, t, st)));
                }
            }
        }
        out.into_iter().collect()
    }

    /// Start points (layer, point, cost, table kinds) of one terminal.
    #[allow(clippy::type_complexity)]
    fn starts(
        &mut self,
        t: &Terminal,
        tables: &mut AccessTables,
        with_via_kinds: bool,
    ) -> Result<Vec<(u32, [i64; 2], u64, Option<Vec<usize>>)>> {
        let step = self.input.step as u64;
        let generic = bounded_points(&t.shapes, self.input.grid, Some(self.input.die))?;
        let on_graph = self.graph_points(&t.shapes);
        let mut starts = Vec::new();
        if let Some((class, origin)) = t.class {
            // These are the exact inputs of the relative geometry computation.
            // Unrelated upper-layer track phases must not fragment this cache.
            let key = access_key(class, origin, &on_graph)?;
            if !tables.contains_key(&key) {
                let c = &self.input.access_classes[class as usize];
                let mut points: Vec<(u32, [i64; 2])> =
                    bounded_points(&c.shapes, self.input.grid, None)?;
                let known: BTreeSet<_> = points.iter().copied().collect();
                for &r in &key.1 {
                    if !known.contains(&r) {
                        points.push(r);
                    }
                }
                let mut work = 0usize;
                let table = access_table_at(
                    self.input,
                    &self.slots,
                    &self.pairs,
                    c,
                    &points,
                    &mut work,
                    self.limits,
                )?;
                let bytes = (table.capacity() * std::mem::size_of::<AccessEntry>()) as u64
                    + table
                        .iter()
                        .map(|e| (e.kinds.capacity() * std::mem::size_of::<usize>() + 16) as u64)
                        .sum::<u64>()
                    + (key.1.capacity() * std::mem::size_of::<(u32, [i64; 2])>()) as u64
                    + 256;
                let cap = access_cache_limit(self.plan);
                if bytes > cap {
                    return Err(budget("single pin access table memory limit"));
                }
                if self.access_cache_bytes.saturating_add(bytes) > cap {
                    self.spatial.work = self
                        .spatial
                        .work
                        .checked_add(self.access_entries)
                        .filter(|&n| n <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("access cache work limit"))?;
                    tables.clear();
                    self.access_entries = 0;
                    self.access_cache_bytes = 0;
                }
                self.access_entries = self
                    .access_entries
                    .checked_add(table.len())
                    .filter(|&n| n <= self.limits.max_segments)
                    .ok_or_else(|| budget("pin access cache-entry limit"))?;
                self.spatial.work = self
                    .spatial
                    .work
                    .checked_add(work)
                    .filter(|&n| n <= self.limits.max_comparisons)
                    .ok_or_else(|| budget("routing comparison limit"))?;
                self.access_cache_bytes += bytes;
                tables.insert(key.clone(), table);
            }
            let allowed: BTreeSet<_> = generic.iter().chain(&on_graph).copied().collect();
            let graph: BTreeSet<_> = on_graph.iter().copied().collect();
            for e in &tables[&key] {
                let p = [
                    int(i128::from(e.at[0]) + i128::from(origin[0]))?,
                    int(i128::from(e.at[1]) + i128::from(origin[1]))?,
                ];
                if allowed.contains(&(e.layer, p)) && contains(self.input.die, p) {
                    let cost = match (graph.contains(&(e.layer, p)), e.fits) {
                        (true, true) => 0,
                        (false, false) => step,
                        _ => step / 2,
                    };
                    starts.push((e.layer, p, cost, with_via_kinds.then(|| e.kinds.clone())));
                }
            }
        }
        if starts.is_empty() {
            // Classless pins (I/O): on-graph points whose default-width
            // square lies inside the pin first, so routed metal never forms
            // a narrow neck with a partly covered pin edge.
            let graph: BTreeSet<_> = on_graph.iter().copied().collect();
            for &(l, p) in on_graph.iter().chain(&generic) {
                if starts
                    .iter()
                    .any(|s: &(u32, [i64; 2], u64, _)| s.0 == l && s.1 == p)
                {
                    continue;
                }
                let half = self.width(l) / 2;
                let square = Rect {
                    lx: p[0] - half,
                    ly: p[1] - half,
                    ux: p[0] + half,
                    uy: p[1] + half,
                };
                let fits = t.shapes.iter().any(|s| {
                    s.layer == l
                        && s.rect.lx <= square.lx
                        && s.rect.ly <= square.ly
                        && s.rect.ux >= square.ux
                        && s.rect.uy >= square.uy
                });
                let cost = match (graph.contains(&(l, p)), fits) {
                    (true, true) => 0,
                    (true, false) => step / 2,
                    _ => step,
                };
                starts.push((l, p, cost, None));
            }
        }
        Ok(starts)
    }

    /// One terminal escape search; None when no legal escape exists.
    fn escape(&mut self, t: &Terminal, tables: &mut AccessTables) -> Result<Option<Plan>> {
        self.escape_avoiding(t, tables, &[])
    }

    /// Escape search whose metal above the pin layer stays off `avoid`, the
    /// landing footprints of neighbouring pins that this escape displaced.
    /// With `area_stacks`, an escape whose own top metal meets AREA is tried
    /// first; the ordinary escape remains the fallback for a crowded pin.
    fn escape_avoiding(
        &mut self,
        t: &Terminal,
        tables: &mut AccessTables,
        avoid: &[LayerRect],
    ) -> Result<Option<Plan>> {
        if self.plan.area_stacks {
            self.escape_area = true;
            let plan = self.escape_search(t, tables, avoid);
            self.escape_area = false;
            if let Some(plan) = plan? {
                return Ok(Some(plan));
            }
        }
        self.escape_search(t, tables, avoid)
    }

    fn escape_search(
        &mut self,
        t: &Terminal,
        tables: &mut AccessTables,
        avoid: &[LayerRect],
    ) -> Result<Option<Plan>> {
        let input = self.input;
        let starts = self.starts(t, tables, true)?;
        if starts.is_empty() {
            return Ok(None);
        }
        let access_slot = self.graph.slot[&input.access_layer];
        let pin_slot = starts
            .iter()
            .filter_map(|s| self.graph.slot.get(&s.0).copied())
            .min()
            .ok_or_else(|| invalid("terminal without a routing-layer shape"))?;
        let above = pin_slot >= access_slot;
        let slots: Vec<usize> = if above {
            vec![pin_slot]
        } else {
            (pin_slot..=access_slot).collect()
        };
        let goal_slot = if above { pin_slot } else { access_slot };
        let reach = input
            .step
            .checked_mul(4)
            .ok_or_else(|| invalid("access radius overflow"))?;
        let bounds = expand(
            Rect {
                lx: starts.iter().map(|p| p.1[0]).min().unwrap(),
                ly: starts.iter().map(|p| p.1[1]).min().unwrap(),
                ux: starts.iter().map(|p| p.1[0]).max().unwrap(),
                uy: starts.iter().map(|p| p.1[1]).max().unwrap(),
            },
            reach,
        )?;
        let mut xs = BTreeSet::new();
        let mut ys = BTreeSet::new();
        for s in &starts {
            xs.insert(s.1[0]);
            ys.insert(s.1[1]);
        }
        for &l in &slots {
            let g = &self.graph.layers[l];
            let (cs, ss) = (&g.tracks, &g.stops);
            let (xv, yv) = if g.vertical { (cs, ss) } else { (ss, cs) };
            // A local escape must not scan the full-chip axes for every pin.
            let x0 = xv.partition_point(|&x| x < bounds.lx);
            let x1 = xv.partition_point(|&x| x <= bounds.ux);
            let y0 = yv.partition_point(|&y| y < bounds.ly);
            let y1 = yv.partition_point(|&y| y <= bounds.uy);
            xs.extend(xv[x0..x1].iter().copied());
            ys.extend(yv[y0..y1].iter().copied());
        }
        let local = Local {
            xs: xs.into_iter().collect(),
            ys: ys.into_iter().collect(),
        };
        let goal = &self.graph.layers[goal_slot];
        let (gx, gy) = if goal.vertical {
            (&goal.tracks, &goal.stops)
        } else {
            (&goal.stops, &goal.tracks)
        };
        let potential = AccessPotential {
            dx: local.xs.iter().map(|&x| nearest_axis(gx, x)).collect(),
            dy: local.ys.iter().map(|&y| nearest_axis(gy, y)).collect(),
            hops: slots
                .iter()
                .map(|&s| {
                    let n = self.access_hops[s][goal_slot];
                    if n == u16::MAX {
                        Ok(0)
                    } else {
                        u64::from(n)
                            .checked_mul(input.step as u64)
                            .ok_or_else(|| budget("pin-access layer-hop cost"))
                    }
                })
                .collect::<Result<_>>()?,
        };
        let plane = local.xs.len() * local.ys.len();
        if plane.saturating_mul(slots.len()) > self.limits.max_grid_nodes {
            return Err(budget("pin access lattice"));
        }
        let pin_shapes = |l: u32| -> Vec<Rect> {
            t.shapes
                .iter()
                .filter(|s| s.layer == l)
                .map(|s| s.rect)
                .collect()
        };
        let table_kinds: HashMap<(u32, [i64; 2]), Vec<usize>> = starts
            .iter()
            .filter_map(|(l, p, _, k)| k.clone().map(|k| ((*l, *p), k)))
            .collect();
        let mut heap = BinaryHeap::new();
        let mut visited = HashMap::<u32, (u64, u32, u32), BuildFx>::with_hasher(BuildFx);
        for (layer, at, cost, _) in &starts {
            let Some(&gs) = self.graph.slot.get(layer) else {
                continue;
            };
            let Some(li) = slots.iter().position(|s| *s == gs) else {
                continue;
            };
            let half = self.width(*layer) / 2;
            let r = Rect {
                lx: at[0] - half,
                ly: at[1] - half,
                ux: at[0] + half,
                uy: at[1] + half,
            };
            if self.spatial.clear(*layer, r, t.net)? {
                let id = local.id(li, *at).unwrap();
                if visited.get(&id).is_none_or(|v| v.0 > *cost) {
                    visited.insert(id, (*cost, NONE, WIRE));
                    heap.push(Reverse((
                        cost.saturating_add(potential.cost(id)?),
                        *cost,
                        id,
                    )));
                }
            }
        }
        let mut found = None;
        let mut search = 0usize;
        // Net-aware choice (opt-in): among the legal tops within
        // `ACCESS_HINT_SLACK` of the cheapest, take the one nearest the net's
        // other terminals (twice their L1 distance added to the cost), so
        // two pins that can share a track land on it. Ties: cost, then id.
        let hint = self.access_hint.clone();
        let mut first_goal: Option<u64> = None;
        let mut best_goal: Option<((u64, u64, u32), Option<usize>)> = None;
        while let Some(Reverse((priority, cost, id))) = heap.pop() {
            if first_goal.is_some_and(|c| priority > c.saturating_add(ACCESS_HINT_SLACK)) {
                break;
            }
            if visited[&id].0 != cost {
                continue;
            }
            let (li, at) = local.at(id);
            if slots[li] == goal_slot && self.graph.find(goal_slot, at).is_some() {
                let needs_portal = !above && self.graph.pairs.iter().any(|p| p.lower == goal_slot);
                let incoming = visited[&id].2;
                let goal_layer = self.graph.layers[goal_slot].layer;
                let enough = if self.escape_area && !self.stack_ok[goal_layer as usize] {
                    // The top metal itself must meet AREA: a short off-track
                    // jog is a wire, not enough metal for a later via.
                    let mut metal = Vec::new();
                    let mut cur = id;
                    for _ in 0..64 {
                        let (_, parent, kind) = visited[&cur];
                        if parent == NONE {
                            break;
                        }
                        let (cl, cp) = local.at(cur);
                        if slots[cl] != goal_slot {
                            break;
                        }
                        if kind == WIRE {
                            metal.push(wire_rect(
                                &Wire {
                                    net: t.net,
                                    layer: goal_layer,
                                    start: local.at(parent).1,
                                    end: cp,
                                },
                                self.width(goal_layer),
                            )?);
                        } else {
                            for pad in &input.vias[kind as usize].shapes {
                                if pad.layer == goal_layer {
                                    metal.push(shifted(pad.rect, cp)?);
                                }
                            }
                            break;
                        }
                        cur = parent;
                    }
                    match input.layers[goal_layer as usize].min_area {
                        Some(min) => {
                            route_rules::union_area(
                                &metal,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )? >= i128::from(min)
                        }
                        None => true,
                    }
                } else {
                    incoming == WIRE || self.via_pad_ok[goal_layer as usize][incoming as usize]
                };
                let portal = if needs_portal {
                    self.access_portal(goal_slot, at, t.net, enough)?
                } else {
                    None
                };
                if !needs_portal || portal.is_some() {
                    if hint.is_empty() {
                        found = Some((id, portal));
                        break;
                    }
                    let near = hint
                        .iter()
                        .map(|r| {
                            at[0].abs_diff(at[0].clamp(r.lx, r.ux))
                                + at[1].abs_diff(at[1].clamp(r.ly, r.uy))
                        })
                        .min()
                        .unwrap_or(0);
                    let key = (cost.saturating_add(near.saturating_mul(2)), cost, id);
                    if best_goal.is_none_or(|(b, _)| key < b) {
                        best_goal = Some((key, portal));
                    }
                    first_goal.get_or_insert(cost);
                }
            }
            self.tick(&mut search, "pin access", t.pin, t.net)?;
            let layer = self.graph.layers[slots[li]].layer;
            let short_run = if slots[li] == pin_slot || self.stack_ok[layer as usize] {
                false
            } else {
                let need = self.min_run[layer as usize];
                let mut cur = id;
                let origin = local.at(id).1;
                let mut short = true;
                // Intermediate escape metal is fixed during later detail
                // repair, so it must already have enough area before another
                // via leaves it. Coordinate extent times wire width is a
                // lower bound on its union area; total path length would
                // overcount overlapping metal in short bends.
                for _ in 0..64 {
                    self.spatial.work = self
                        .spatial
                        .work
                        .checked_add(1)
                        .filter(|&n| n <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("pin access area work"))?;
                    let (_, parent, kind) = visited[&cur];
                    if parent == NONE {
                        short = false;
                        break;
                    }
                    if kind != WIRE {
                        short = !self.via_pad_ok[layer as usize][kind as usize];
                        break;
                    }
                    let point = local.at(parent).1;
                    if origin[0]
                        .abs_diff(point[0])
                        .max(origin[1].abs_diff(point[1]))
                        >= need
                    {
                        short = false;
                        break;
                    }
                    cur = parent;
                }
                short
            };
            let (xi, yi) = (
                local.xs.binary_search(&at[0]).unwrap(),
                local.ys.binary_search(&at[1]).unwrap(),
            );
            let mut next: Vec<(u32, Option<usize>)> = Vec::with_capacity(6);
            if xi > 0 {
                next.push((id - 1, None));
            }
            if xi + 1 < local.xs.len() {
                next.push((id + 1, None));
            }
            if yi > 0 {
                next.push((id - local.xs.len() as u32, None));
            }
            if yi + 1 < local.ys.len() {
                next.push((id + local.xs.len() as u32, None));
            }
            for (pi, p) in self.pairs.iter().enumerate() {
                let other = if p.lower == layer {
                    p.upper
                } else if p.upper == layer {
                    p.lower
                } else {
                    continue;
                };
                let Some(&os) = self.graph.slot.get(&other) else {
                    continue;
                };
                if let Some(oi) = slots.iter().position(|s| *s == os) {
                    next.push((local.id(oi, at).unwrap(), Some(pi)));
                }
            }
            for (nid, via) in next {
                let (_, q) = local.at(nid);
                if !contains(bounds, q) {
                    continue;
                }
                let length = if via.is_some() {
                    input.step as u64
                } else {
                    let along = input.layers[layer as usize].vertical == Some(at[0] == q[0]);
                    distance(at, q)?
                        .checked_mul(if along { 1 } else { self.plan.wrong_way_factor })
                        .ok_or_else(|| budget("pin access wrong-way cost"))?
                };
                let nc = cost
                    .checked_add(length)
                    .ok_or_else(|| budget("access cost overflow"))?;
                if visited.get(&nid).is_some_and(|v| v.0 <= nc) {
                    continue;
                }
                let kind = if let Some(pi) = via {
                    let pair = &self.pairs[pi];
                    let mut listed: Vec<usize> = match table_kinds.get(&(layer, at)) {
                        Some(ks) if pair.lower == layer => ks
                            .iter()
                            .copied()
                            .filter(|k| pair.kinds.contains(k))
                            .collect(),
                        _ => pair.kinds.clone(),
                    };
                    self.spatial.work = self
                        .spatial
                        .work
                        .checked_add(
                            listed
                                .len()
                                .saturating_mul(listed.len().max(1).ilog2() as usize + 1),
                        )
                        .filter(|&n| n <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("pin access via-order work"))?;
                    let (_, parent, incoming) = visited[&id];
                    let direction = if parent == NONE {
                        None
                    } else if incoming == WIRE {
                        Some(local.at(parent).1[0] == at[0])
                    } else {
                        self.pad_direction(incoming as usize, layer)
                    };
                    let other = if pair.lower == layer {
                        pair.upper
                    } else {
                        pair.lower
                    };
                    if let Some(vertical) = direction {
                        // Access vias become fixed obstacles during detail
                        // repair. Prefer a pad along the incoming wire here,
                        // before cleanup can create a blocked notch patch.
                        // With `area_stacks` the upper pad follows its own
                        // layer: a cross pad narrows both adjacent tracks.
                        let upper = if self.plan.area_stacks {
                            input.layers[other as usize].vertical.unwrap_or(vertical)
                        } else {
                            vertical
                        };
                        listed.sort_by_key(|&k| {
                            (
                                !self.pad_along(k, layer, vertical),
                                !self.pad_along(k, other, upper),
                            )
                        });
                    } else {
                        // Inside the pin there is no external incoming lead.
                        // A cross-track-wide upper pad can seal the adjacent
                        // pin even when both lower pads fit. Prefer the next
                        // layer's preferred direction before reserving either.
                        let vertical = input.layers[other as usize].vertical.unwrap_or(false);
                        listed.sort_by_key(|&k| !self.pad_along(k, other, vertical));
                    }
                    let mut chosen = None;
                    'kinds: for k in listed {
                        if short_run && !self.via_pad_ok[layer as usize][k] {
                            continue;
                        }
                        for s in &input.vias[k].shapes {
                            if overlaps_any(
                                avoid,
                                s.layer,
                                shifted(s.rect, at)?,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )? {
                                continue 'kinds;
                            }
                        }
                        for s in &input.vias[k].shapes {
                            let rule = &input.layers[s.layer as usize];
                            let own = pin_shapes(s.layer);
                            if rule.vertical.is_some()
                                && !own.is_empty()
                                && !route_rules::local_ok(
                                    shifted(s.rect, at)?,
                                    &own,
                                    rule.spacing,
                                    &mut self.spatial.work,
                                    self.limits.max_comparisons,
                                )?
                            {
                                continue 'kinds;
                            }
                        }
                        if self.spatial.via_clear(k, at, t.net)? {
                            chosen = Some(k as u32);
                            break;
                        }
                    }
                    match chosen {
                        Some(k) => k,
                        None => continue,
                    }
                } else {
                    let rect = wire_rect(
                        &Wire {
                            net: t.net,
                            layer,
                            start: at,
                            end: q,
                        },
                        self.width(layer),
                    )?;
                    let own = pin_shapes(layer);
                    if !own.is_empty()
                        && !route_rules::local_ok(
                            rect,
                            &own,
                            input.layers[layer as usize].spacing,
                            &mut self.spatial.work,
                            self.limits.max_comparisons,
                        )?
                    {
                        continue;
                    }
                    if overlaps_any(
                        avoid,
                        layer,
                        rect,
                        &mut self.spatial.work,
                        self.limits.max_comparisons,
                    )? {
                        continue;
                    }
                    if !self.spatial.clear(layer, rect, t.net)? {
                        continue;
                    }
                    WIRE
                };
                if heap.len() >= self.limits.max_heap || visited.len() >= self.limits.max_grid_nodes
                {
                    return Err(budget("pin access frontier"));
                }
                visited.insert(nid, (nc, id, kind));
                heap.push(Reverse((nc.saturating_add(potential.cost(nid)?), nc, nid)));
            }
        }
        if found.is_none() {
            found = best_goal.map(|((_, _, id), portal)| (id, portal));
        }
        let Some((top, portal)) = found else {
            if std::env::var("RUSTROAD_ROUTE_PIN_TRACE").is_ok_and(|filter| {
                filter
                    .split(',')
                    .take(16)
                    .filter_map(|p| p.parse::<u32>().ok())
                    .any(|p| p == t.pin)
            }) {
                for (layer, at, _, kinds) in starts
                    .iter()
                    .filter(|s| s.3.as_ref().is_some_and(|k| !k.is_empty()))
                    .take(2)
                {
                    let Some(kinds) = kinds else { continue };
                    for &kind in kinds.iter().take(4) {
                        for shape in &input.vias[kind].shapes {
                            let r = shifted(shape.rect, *at)?;
                            if self.spatial.probe(shape.layer, r, t.net)?.is_none() {
                                eprintln!(
                                    "[troute] blocked start pin {} layer {} at {:?} via {} shape_layer {} rect {:?} {}",
                                    t.pin,
                                    layer,
                                    at,
                                    input.vias[kind].name,
                                    shape.layer,
                                    r,
                                    self.spatial.stored_collision(shape.layer, r, t.net)?
                                );
                                break;
                            }
                        }
                    }
                }
            }
            trace(
                "access failed",
                std::time::Instant::now(),
                format_args!(
                    "pin {} net {} shapes {:?} starts {:?} slots {:?} visited {}",
                    t.pin,
                    t.net,
                    t.shapes,
                    starts,
                    slots,
                    visited.len(),
                ),
            );
            return Ok(None);
        };
        let top_via = visited[&top].1 != NONE && visited[&top].2 != WIRE;
        let mut wires = Vec::new();
        let mut vias = Vec::new();
        let mut current = top;
        let mut steps = 0usize;
        while visited[&current].1 != NONE {
            let (_, prev, kind) = visited[&current];
            let (la, a) = local.at(current);
            let (lb, b) = local.at(prev);
            if la == lb {
                wires.push(Wire {
                    net: t.net,
                    layer: self.graph.layers[slots[la]].layer,
                    start: b,
                    end: a,
                });
            } else {
                vias.push(Via {
                    net: t.net,
                    kind,
                    at: a,
                });
            }
            current = prev;
            steps += 1;
            if steps > self.limits.max_segments {
                return Err(budget("access path limit"));
            }
        }
        let (li, mut at) = local.at(current);
        let access_layer = self.graph.layers[slots[li]].layer;
        // A leading escape wire that lies wholly inside the pin's own metal is
        // redundant (the pin already conducts there) and would connect the pin
        // twice (wire and the via at its end, both on the pin): start the
        // escape where it leaves the pin instead.
        while let Some(w) = wires.last() {
            let rect = wire_rect(w, self.width(w.layer))?;
            let pin: Vec<Rect> = t
                .shapes
                .iter()
                .filter(|s| s.layer == w.layer)
                .map(|s| s.rect)
                .collect();
            let inside = covered(rect, &pin);
            if w.layer != access_layer || w.start != at || !inside {
                break;
            }
            at = w.end;
            wires.pop();
        }
        let (tli, tat) = local.at(top);
        let top_node = self
            .graph
            .find(slots[tli], tat)
            .ok_or_else(|| invalid("access top off graph"))?;
        // An escape that reaches its top by a short jog still stacks when the
        // route leaves by a via: its own top-layer metal is below AREA.
        let top_layer = self.graph.layers[slots[tli]].layer;
        let top_via = top_via
            || (!self.stack_ok[top_layer as usize]
                && !t.shapes.iter().any(|s| s.layer == top_layer)
                && match input.layers[top_layer as usize].min_area {
                    Some(min) => {
                        let mut metal = Vec::new();
                        for w in wires.iter().filter(|w| w.layer == top_layer) {
                            metal.push(wire_rect(w, self.width(top_layer))?);
                        }
                        for v in &vias {
                            for pad in &input.vias[v.kind as usize].shapes {
                                if pad.layer == top_layer {
                                    metal.push(shifted(pad.rect, v.at)?);
                                }
                            }
                        }
                        !metal.is_empty()
                            && route_rules::union_area(
                                &metal,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )? < i128::from(min)
                    }
                    None => false,
                });
        let mut shapes = Vec::new();
        for w in &wires {
            shapes.push(self.spatial.add_fixed(Blockage {
                layer: w.layer,
                rect: wire_rect(w, self.width(w.layer))?,
                net: Some(w.net),
            })?);
        }
        for v in &vias {
            for s in &input.vias[v.kind as usize].shapes {
                shapes.push(self.spatial.add_fixed(Blockage {
                    layer: s.layer,
                    rect: shifted(s.rect, v.at)?,
                    net: Some(v.net),
                })?);
            }
        }
        let portal_shapes = if let Some(kind) = portal {
            let (_, at) = local.at(top);
            for shape in &input.vias[kind].shapes {
                shapes.push(self.spatial.add_fixed(Blockage {
                    layer: shape.layer,
                    rect: shifted(shape.rect, at)?,
                    net: Some(t.net),
                })?);
            }
            input.vias[kind].shapes.len() as u8
        } else {
            0
        };
        if std::env::var("RUSTROAD_ROUTE_PIN_TRACE").is_ok_and(|filter| {
            filter
                .split(',')
                .take(16)
                .filter_map(|p| p.parse::<u32>().ok())
                .any(|p| p == t.pin)
        }) {
            eprintln!(
                "[troute] escape pin {} net {} access {:?} wires {:?} vias {:?}",
                t.pin,
                t.net,
                at,
                &wires[..wires.len().min(64)],
                vias.iter()
                    .take(64)
                    .map(|v| (&input.vias[v.kind as usize].name, v.at))
                    .collect::<Vec<_>>()
            );
        }
        Ok(Some(Plan {
            access: Access {
                pin: t.pin,
                net: t.net,
                layer: access_layer,
                at,
            },
            top: top_node,
            top_via,
            portal_shapes,
            wires: wires.into_boxed_slice(),
            vias: vias.into_boxed_slice(),
            shapes: shapes.into_boxed_slice(),
        }))
    }

    #[cfg(test)]
    fn hints_survive_repair(&self) -> bool {
        self.keep_hints_after_repair
    }
    #[cfg(not(test))]
    fn hints_survive_repair(&self) -> bool {
        false
    }
    #[cfg(test)]
    fn access_retry_disabled(&self) -> bool {
        self.no_access_retry
    }
    #[cfg(not(test))]
    fn access_retry_disabled(&self) -> bool {
        false
    }

    /// Conflict-aware escapes for every terminal of a routed net.
    fn reserve(&mut self, terminals: &[&'a Terminal]) -> Result<HashMap<u32, Plan>> {
        let started = std::time::Instant::now();
        let mut tables = BTreeMap::new();
        // Most constrained first: fewest start candidates, then pin index.
        let mut order: Vec<(usize, u32, usize)> = Vec::with_capacity(terminals.len());
        for (i, t) in terminals.iter().enumerate() {
            let n = self.starts(t, &mut tables, false)?.len();
            order.push((n, t.pin, i));
            if i.is_multiple_of(100_000) {
                trace(
                    "access count",
                    started,
                    format_args!(
                        "terminals {i}/{} tables {} work {}",
                        terminals.len(),
                        tables.len(),
                        self.spatial.work
                    ),
                );
            }
        }
        order.sort_unstable();
        trace(
            "access candidates",
            started,
            format_args!("terminals {} work {}", terminals.len(), self.spatial.work),
        );
        let windows: Vec<Rect> = terminals
            .iter()
            .map(|t| {
                let mut r = t.shapes[0].rect;
                for s in &t.shapes {
                    r.lx = r.lx.min(s.rect.lx);
                    r.ly = r.ly.min(s.rect.ly);
                    r.ux = r.ux.max(s.rect.ux);
                    r.uy = r.uy.max(s.rect.uy);
                }
                expand(r, self.input.step.saturating_mul(4))
            })
            .collect::<Result<_>>()?;
        let order: Vec<usize> = order.into_iter().map(|o| o.2).collect();
        // Net members for net-aware access (opt-in); small nets only.
        let mut by_net: HashMap<u32, Vec<usize>, BuildFx> = HashMap::with_hasher(BuildFx);
        if self.plan.qor.as_ref().is_some_and(|q| q.net_aware_access) {
            for (i, t) in terminals.iter().enumerate() {
                by_net.entry(t.net).or_default().push(i);
            }
        }
        let mut plans: HashMap<u32, Plan> = HashMap::new();
        let mut window_index = None;
        let pass = AccessPass {
            terminals,
            order: &order,
            windows: &windows,
            by_net: &by_net,
            started,
        };
        let Some(failure) = self.reserve_pass(&pass, &mut tables, &mut window_index, &mut plans)?
        else {
            return Ok(plans);
        };
        if by_net.is_empty() || self.access_retry_disabled() {
            return Err(failure);
        }
        // Net-aware access is a preference, never a reason to fail: if its
        // pass cannot place every escape, drop it and redo pin access without
        // hints (the legacy pass, from the same start state). Deterministic,
        // and the shipped flows never take this path.
        trace(
            "access retry",
            started,
            format_args!("without net hints after: {failure}"),
        );
        for plan in plans.drain().map(|(_, plan)| plan) {
            for id in plan.shapes {
                self.spatial.remove(id);
            }
        }
        self.access_retries += 1;
        let empty = HashMap::with_hasher(BuildFx);
        let pass = AccessPass {
            by_net: &empty,
            ..pass
        };
        match self.reserve_pass(&pass, &mut tables, &mut window_index, &mut plans)? {
            None => Ok(plans),
            Some(failure) => Err(failure),
        }
    }

    /// One pin-access pass in `pass.order` with conflict repair. Escapes go
    /// into `plans` (and the spatial index). `Ok(Some(error))` is an access
    /// failure (a pin with no legal escape, or the repair-round cap) with
    /// `plans` holding the escapes placed so far; `Err` is any other error.
    fn reserve_pass(
        &mut self,
        pass: &AccessPass<'_, 'a>,
        tables: &mut AccessTables,
        window_index: &mut Option<WindowIndex>,
        plans: &mut HashMap<u32, Plan>,
    ) -> Result<Option<crate::Error>> {
        let AccessPass {
            terminals,
            windows,
            by_net,
            started,
            ..
        } = *pass;
        let mut queue: VecDeque<usize> = pass.order.iter().copied().collect();
        // A terminal whose escape failed, and every escape it displaced, take
        // the cheapest top from then on, so a repair cannot be re-blocked by
        // the same nearer-to-the-net top of a neighbour on every attempt.
        let mut unhinted: HashSet<usize, BuildFx> = HashSet::with_hasher(BuildFx);
        let mut plan_records = 0usize;
        let mut owner: BTreeMap<u32, usize> = BTreeMap::new();
        let mut attempts = vec![0usize; terminals.len()];
        // Landing footprints of the pins a failed terminal displaced. Its
        // next escape first stays off them, so the displaced pins can land.
        let mut protect: HashMap<usize, Vec<LayerRect>> = HashMap::new();
        let mut rounds = 0usize;
        while let Some(i) = queue.pop_front() {
            rounds += 1;
            if rounds > terminals.len().saturating_mul(8) + 64 {
                return Ok(Some(budget("pin access repair rounds")));
            }
            let t = terminals[i];
            if rounds.is_multiple_of(100_000) {
                trace(
                    "access progress",
                    started,
                    format_args!(
                        "complete {} rounds {rounds} work {}",
                        plans.len(),
                        self.spatial.work
                    ),
                );
            }
            let avoid = protect.remove(&i).unwrap_or_default();
            let hint: Vec<Rect> = match by_net.get(&t.net) {
                Some(members)
                    if members.len() <= ACCESS_HINT_TERMINALS && !unhinted.contains(&i) =>
                {
                    members
                        .iter()
                        .filter(|&&j| j != i)
                        .map(|&j| {
                            let o = terminals[j];
                            match plans.get(&o.pin) {
                                Some(p) => {
                                    let at = self.graph.point(p.top).1;
                                    Rect {
                                        lx: at[0],
                                        ly: at[1],
                                        ux: at[0],
                                        uy: at[1],
                                    }
                                }
                                None => pin_box(o),
                            }
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            let mut plan = None;
            if !avoid.is_empty() {
                self.access_hint.clone_from(&hint);
                plan = self.escape_avoiding(t, tables, &avoid)?;
            }
            if plan.is_none() {
                self.access_hint.clone_from(&hint);
                plan = self.escape(t, tables)?;
            }
            self.access_hint.clear();
            if let Some(plan) = plan {
                plan_records = plan_records
                    .checked_add(plan.wires.len())
                    .and_then(|n| n.checked_add(plan.vias.len()))
                    .filter(|&n| n <= self.limits.max_segments)
                    .ok_or_else(|| budget("pin access retained-record limit"))?;
                plans.insert(t.pin, plan);
                owner.insert(t.pin, i);
                if plans.len().is_multiple_of(4096) {
                    self.guard("pin access")?;
                }
                continue;
            }
            attempts[i] += 1;
            if attempts[i] > 3 {
                return Ok(Some(crate::Error::PinAccess {
                    pin: t.pin,
                    net: t.net,
                    region: windows[i],
                    comparisons: self.spatial.work,
                    expansions: self.expansions,
                }));
            }
            // Rip up escapes of terminals whose windows overlap this one.
            if window_index.is_none() {
                *window_index = Some(WindowIndex::new(
                    windows,
                    &mut self.spatial.work,
                    self.limits.max_comparisons,
                )?);
            }
            let mut victims: Vec<u32> = window_index
                .as_ref()
                .unwrap()
                .overlapping(
                    windows,
                    windows[i],
                    &mut self.spatial.work,
                    self.limits.max_comparisons,
                )?
                .into_iter()
                .map(|j| terminals[j].pin)
                .filter(|pin| owner.contains_key(pin))
                .collect();
            // Match the original ascending-pin repair order exactly.
            victims.sort_unstable();
            if victims.is_empty() {
                return Ok(Some(crate::Error::PinAccess {
                    pin: t.pin,
                    net: t.net,
                    region: windows[i],
                    comparisons: self.spatial.work,
                    expansions: self.expansions,
                }));
            }
            let mut footprints = Vec::new();
            for &p in &victims {
                for shape in &terminals[owner[&p]].shapes {
                    let above = shape.layer + 1;
                    if let Some(rule) = self.input.layers.get(above as usize) {
                        footprints.push(LayerRect {
                            layer: above,
                            rect: expand(shape.rect, rule.spacing)?,
                        });
                    }
                }
            }
            if footprints.len() > self.limits.max_active_nodes {
                return Err(budget("pin access protected footprints"));
            }
            protect.insert(i, footprints);
            let mut back = Vec::new();
            for p in victims {
                let plan = plans.remove(&p).unwrap();
                plan_records -= plan.wires.len() + plan.vias.len();
                for id in plan.shapes {
                    self.spatial.remove(id);
                }
                back.push(owner.remove(&p).unwrap());
            }
            if !by_net.is_empty() && !self.hints_survive_repair() {
                unhinted.insert(i);
                unhinted.extend(back.iter().copied());
            }
            // Rotate the victims so repeated repairs try different orders.
            let rot = attempts[i] % back.len().max(1);
            back.rotate_left(rot);
            for j in back.into_iter().rev() {
                queue.push_front(j);
            }
            queue.push_front(i);
        }
        Ok(None)
    }

    // ---------------------------------------------------------------- global

    /// Per-edge track capacity against fixed geometry (net-less probes).
    fn capacities(&mut self) -> Result<Vec<u32>> {
        let c = &self.cells;
        let mut cap = vec![0u32; c.edges()];
        let (nx, ny, g, lx, ly) = (c.nx, c.ny, c.g, c.lx, c.ly);
        for l in 0..self.graph.layers.len() {
            let layer = self.graph.layers[l].layer;
            if !self.plan.allows_routing_layer(layer) {
                continue;
            }
            let vertical = self.graph.layers[l].vertical;
            let tracks = self.graph.layers[l].tracks.clone();
            let half = self.width(layer) / 2;
            for &tc in &tracks {
                // Tracks cross boundaries perpendicular to the layer direction.
                let (row, count) = if vertical {
                    (((tc - lx) / g).clamp(0, nx as i64 - 1) as usize, ny)
                } else {
                    (((tc - ly) / g).clamp(0, ny as i64 - 1) as usize, nx)
                };
                for b in 1..count {
                    let along = if vertical { ly } else { lx } + b as i64 * g;
                    // Capacity means a continuous channel between the adjacent
                    // GCell centres. A boundary-only square misses crossbars
                    // inside either cell and can trap detailed routing in a
                    // guide that excludes the required obstacle detour.
                    let near = along.saturating_sub(g / 2).saturating_sub(half);
                    let far = along.saturating_add(g - g / 2).saturating_add(half);
                    let rect = if vertical {
                        Rect {
                            lx: tc - half,
                            ux: tc + half,
                            ly: near.max(self.input.die.ly),
                            uy: far.min(self.input.die.uy),
                        }
                    } else {
                        Rect {
                            lx: near.max(self.input.die.lx),
                            ux: far.min(self.input.die.ux),
                            ly: tc - half,
                            uy: tc + half,
                        }
                    };
                    if !self.spatial.span_clear(layer, rect, NONE)? {
                        continue;
                    }
                    let e = if vertical {
                        (nx - 1) * ny + (b - 1) * nx + row
                    } else {
                        row * (nx - 1) + (b - 1)
                    };
                    cap[e] += 1;
                }
            }
        }
        for v in &mut cap {
            *v = (u64::from(*v) * u64::from(self.plan.capacity_percent) / 100) as u32;
        }
        Ok(cap)
    }

    /// Components of continuous positive-capacity GCell channels. Labels and
    /// the iterative traversal stack each retain at most one entry per cell.
    fn positive_components(&mut self, cap: &[u32]) -> Result<Vec<u32>> {
        let count = self.cells.len();
        let mut labels = vec![NONE; count];
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(count)
            .map_err(|_| budget("global component traversal allocation"))?;
        let mut neighbors = Vec::with_capacity(4);
        for root in 0..count {
            if labels[root] != NONE {
                continue;
            }
            let label = root as u32;
            labels[root] = label;
            pending.push(label);
            while let Some(cell) = pending.pop() {
                self.cells.neighbors(cell, &mut neighbors);
                self.spatial.work = self
                    .spatial
                    .work
                    .checked_add(1 + neighbors.len())
                    .filter(|&work| work <= self.limits.max_comparisons)
                    .ok_or_else(|| budget("global component work"))?;
                for &next in &neighbors {
                    if labels[next as usize] == NONE && cap[self.cells.edge(cell, next)] > 0 {
                        labels[next as usize] = label;
                        pending.push(next);
                    }
                }
            }
        }
        Ok(labels)
    }

    /// Negotiated-congestion global routing; returns one guide per net.
    fn global(
        &mut self,
        nets: &[(u32, Vec<u32>)],
        plans: &HashMap<u32, Plan>,
    ) -> Result<BTreeMap<u32, Vec<u32>>> {
        // Envelopes protect detailed access but are not existing conductor
        // capacity. In particular they must not seal a terminal's GCell.
        let count = plans.values().try_fold(0usize, |count, p| {
            count
                .checked_add(usize::from(p.portal_shapes))
                .ok_or_else(|| budget("access portal count"))
        })?;
        self.spatial.work = self
            .spatial
            .work
            .checked_add(count.saturating_mul(2))
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("access portal capacity work"))?;
        for plan in plans.values() {
            for &id in plan
                .shapes
                .iter()
                .rev()
                .take(usize::from(plan.portal_shapes))
            {
                self.spatial.remove(id);
            }
        }
        let result = self.capacities();
        for plan in plans.values() {
            for &id in plan
                .shapes
                .iter()
                .rev()
                .take(usize::from(plan.portal_shapes))
            {
                self.spatial.restore(id)?;
            }
        }
        let cap = result?;
        let components = self.positive_components(&cap)?;
        let n = self.cells.len();
        let mut usage = vec![0u32; cap.len()];
        let mut hist = vec![0u32; cap.len()];
        let mut cost = vec![u64::MAX; n];
        let mut parent = vec![NONE; n];
        let mut tag = vec![0u32; n];
        let mut epoch = 0u32;
        let mut routes: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        let mut cellsets: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut retained_edges = 0usize;
        let mut buf = Vec::with_capacity(4);
        let unit = 1000u64;
        let mut order: Vec<(u64, u32)> = nets
            .iter()
            .map(|(net, pins)| {
                let xs: Vec<_> = pins.iter().map(|c| self.cells.xy(*c)).collect();
                let w =
                    xs.iter().map(|p| p.0).max().unwrap() - xs.iter().map(|p| p.0).min().unwrap();
                let h =
                    xs.iter().map(|p| p.1).max().unwrap() - xs.iter().map(|p| p.1).min().unwrap();
                ((w + h) as u64, *net)
            })
            .collect();
        order.sort_unstable();
        let pins: BTreeMap<u32, &Vec<u32>> = nets.iter().map(|(n, p)| (*n, p)).collect();
        let mut work = 0usize;
        for iteration in 0..=self.plan.global_iterations {
            let reroute: Vec<u32> = if iteration == 0 {
                order.iter().map(|o| o.1).collect()
            } else {
                let over: Vec<bool> = usage.iter().zip(&cap).map(|(u, c)| u > c).collect();
                if !over.iter().any(|o| *o) {
                    break;
                }
                for (e, o) in over.iter().enumerate() {
                    if *o {
                        hist[e] = hist[e].saturating_add(1);
                    }
                }
                order
                    .iter()
                    .map(|o| o.1)
                    .filter(|net| routes[net].iter().any(|e| over[*e]))
                    .collect()
            };
            let factor = 1 + iteration as u64;
            for net in reroute {
                if let Some(old) = routes.remove(&net) {
                    retained_edges -= old.len();
                    for e in old {
                        usage[e] -= 1;
                    }
                }
                let targets = pins[&net];
                let mut tree: BTreeSet<u32> = BTreeSet::from([targets[0]]);
                let mut edges = BTreeSet::new();
                let root = self.cells.xy(targets[0]);
                let mut rest: Vec<u32> = targets[1..].to_vec();
                rest.sort_by_key(|c| {
                    let (x, y) = self.cells.xy(*c);
                    (x.abs_diff(root.0) + y.abs_diff(root.1), *c)
                });
                for sink in rest {
                    if tree.contains(&sink) {
                        continue;
                    }
                    // When a continuous-capacity detour exists, zero-capacity
                    // shortcuts cannot help detailed routing. Preserve the
                    // negotiated fallback for terminals in separate components:
                    // conservative capacity does not prove disconnection.
                    self.spatial.work = self
                        .spatial
                        .work
                        .checked_add(tree.len())
                        .filter(|&work| work <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("global component target work"))?;
                    let positive = tree
                        .iter()
                        .any(|&cell| components[cell as usize] == components[sink as usize]);
                    epoch += 1;
                    let pts: Vec<_> = tree.iter().map(|c| self.cells.xy(*c)).collect();
                    let (bx0, bx1) = (
                        pts.iter().map(|p| p.0).min().unwrap(),
                        pts.iter().map(|p| p.0).max().unwrap(),
                    );
                    let (by0, by1) = (
                        pts.iter().map(|p| p.1).min().unwrap(),
                        pts.iter().map(|p| p.1).max().unwrap(),
                    );
                    let h = |c: u32| -> u64 {
                        let (x, y) = self.cells.xy(c);
                        let dx = if x < bx0 {
                            bx0 - x
                        } else {
                            x.saturating_sub(bx1)
                        };
                        let dy = if y < by0 {
                            by0 - y
                        } else {
                            y.saturating_sub(by1)
                        };
                        (dx + dy) as u64 * unit
                    };
                    let mut heap = BinaryHeap::new();
                    cost[sink as usize] = 0;
                    parent[sink as usize] = NONE;
                    tag[sink as usize] = epoch;
                    heap.push(Reverse((h(sink), Reverse(0u64), sink)));
                    let mut found = None;
                    while let Some(Reverse((_, Reverse(c), id))) = heap.pop() {
                        if tag[id as usize] != epoch || cost[id as usize] != c {
                            continue;
                        }
                        if tree.contains(&id) {
                            found = Some(id);
                            break;
                        }
                        work += 1;
                        if work.is_multiple_of(1 << 16) {
                            self.guard("global search")?;
                        }
                        if self.expansions.saturating_add(work) > self.limits.max_expansions {
                            return Err(budget("global routing expansion limit"));
                        }
                        self.cells.neighbors(id, &mut buf);
                        for &nb in &buf {
                            let e = self.cells.edge(id, nb);
                            if positive && cap[e] == 0 {
                                continue;
                            }
                            let over = u64::from((usage[e] + 1).saturating_sub(cap[e]));
                            let step =
                                unit + u64::from(hist[e]) * unit / 2 + over * factor * unit * 4;
                            let nc = c + step;
                            if tag[nb as usize] == epoch && cost[nb as usize] <= nc {
                                continue;
                            }
                            tag[nb as usize] = epoch;
                            cost[nb as usize] = nc;
                            parent[nb as usize] = id;
                            if heap.len() >= self.limits.max_heap {
                                return Err(budget("global routing frontier"));
                            }
                            heap.push(Reverse((nc + h(nb), Reverse(nc), nb)));
                        }
                    }
                    let mut cur = found.ok_or_else(|| invalid("global routing disconnected"))?;
                    while parent[cur as usize] != NONE {
                        let p = parent[cur as usize];
                        edges.insert(self.cells.edge(cur, p));
                        tree.insert(cur);
                        if edges.len() > self.limits.max_segments
                            || tree.len() > self.limits.max_segments
                        {
                            return Err(budget("global routing net size"));
                        }
                        cur = p;
                    }
                    tree.insert(cur);
                }
                for &e in &edges {
                    usage[e] += 1;
                }
                retained_edges = retained_edges
                    .checked_add(edges.len())
                    .filter(|&n| n <= self.limits.max_segments)
                    .ok_or_else(|| budget("retained global-edge limit"))?;
                routes.insert(net, edges.into_iter().collect());
                cellsets.insert(net, tree.into_iter().collect());
            }
        }
        self.expansions = self.expansions.saturating_add(work);
        // Guides: dilated GCell sets.
        let m = self.plan.guide_margin as i64;
        let mut guides = BTreeMap::new();
        let mut guide_cells = 0usize;
        for (net, set) in cellsets {
            let mut g = BTreeSet::new();
            for c in set {
                let (x, y) = self.cells.xy(c);
                for dy in -m..=m {
                    for dx in -m..=m {
                        self.spatial.work = self
                            .spatial
                            .work
                            .checked_add(1)
                            .filter(|&n| n <= self.limits.max_comparisons)
                            .ok_or_else(|| budget("global guide work limit"))?;
                        let (xx, yy) = (x as i64 + dx, y as i64 + dy);
                        if xx >= 0
                            && yy >= 0
                            && (xx as usize) < self.cells.nx
                            && (yy as usize) < self.cells.ny
                        {
                            g.insert((yy as usize * self.cells.nx + xx as usize) as u32);
                            if g.len() > self.limits.max_segments {
                                return Err(budget("global guide size"));
                            }
                        }
                    }
                }
            }
            guide_cells = guide_cells
                .checked_add(g.len())
                .filter(|&n| n <= self.limits.max_segments)
                .ok_or_else(|| budget("retained global-guide limit"))?;
            guides.insert(net, g.into_iter().collect());
        }
        Ok(guides)
    }

    // -------------------------------------------------------------- detailed

    fn mark_region(&mut self, guide: &[u32], expand_by: Option<usize>) {
        self.gepoch += 1;
        match expand_by {
            None => {
                for &c in guide {
                    self.gmark[c as usize] = self.gepoch;
                }
            }
            Some(m) => {
                let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
                for &c in guide {
                    let (x, y) = self.cells.xy(c);
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
                let (x0, y0) = (x0.saturating_sub(m), y0.saturating_sub(m));
                let (x1, y1) = (
                    (x1 + m).min(self.cells.nx - 1),
                    (y1 + m).min(self.cells.ny - 1),
                );
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        self.gmark[y * self.cells.nx + x] = self.gepoch;
                    }
                }
            }
        }
    }

    fn mark_region_corridor(&mut self, guide: &[u32], m: usize) {
        self.gepoch += 1;
        for &c in guide {
            let (x, y) = self.cells.xy(c);
            let (x0, y0) = (x.saturating_sub(m), y.saturating_sub(m));
            let (x1, y1) = (
                (x + m).min(self.cells.nx - 1),
                (y + m).min(self.cells.ny - 1),
            );
            for yy in y0..=y1 {
                let row = yy * self.cells.nx;
                for xx in x0..=x1 {
                    self.gmark[row + xx] = self.gepoch;
                }
            }
        }
    }

    /// Is the pad of via `kind` on `layer` at least as long along the given
    /// direction as across it?
    fn pad_along(&self, kind: usize, layer: u32, vertical: bool) -> bool {
        self.input.vias[kind]
            .shapes
            .iter()
            .filter(|s| s.layer == layer)
            .all(|s| {
                let (w, h) = (s.rect.ux - s.rect.lx, s.rect.uy - s.rect.ly);
                if vertical { h >= w } else { w >= h }
            })
    }

    fn pad_direction(&self, kind: usize, layer: u32) -> Option<bool> {
        match (
            self.pad_along(kind, layer, true),
            self.pad_along(kind, layer, false),
        ) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// Re-pick routed via variants so that on each layer where exactly one
    /// wire ends at the via, the pad is elongated along that wire (LEF58
    /// end-of-line enclosure rules are not modelled natively). A variant is
    /// taken only when it clears every other net.
    fn orient_vias(&mut self, routed: &mut BTreeMap<u32, NetRoute>) -> Result<usize> {
        let mut changed = 0usize;
        for (&net, route) in routed.iter_mut() {
            // (layer, point) -> incident wire directions and sides.
            type Ends = HashMap<(u32, [i64; 2]), Vec<(bool, i8)>>;
            let mut ends: Ends = HashMap::new();
            for wire in &route.wires {
                let la = wire.layer;
                let (pa, pb) = (wire.start, wire.end);
                let vertical = pa[0] == pb[0];
                let axis = usize::from(vertical);
                let (lo, hi) = if pa[axis] <= pb[axis] {
                    (pa, pb)
                } else {
                    (pb, pa)
                };
                ends.entry((la, lo)).or_default().push((vertical, 1));
                ends.entry((la, hi)).or_default().push((vertical, -1));
            }
            for via in &mut route.vias {
                let (kind, at) = (via.kind, via.at);
                let v = &self.input.vias[kind as usize];
                let need = |layer: u32| -> Option<bool> {
                    let list = ends.get(&(layer, at))?;
                    let first = list[0];
                    (list.len() == 1).then_some(first.0)
                };
                let wants: Vec<(u32, bool)> = [v.lower, v.upper]
                    .into_iter()
                    .filter_map(|l| need(l).map(|d| (l, d)))
                    .collect();
                if wants
                    .iter()
                    .all(|&(l, d)| self.pad_along(kind as usize, l, d))
                {
                    continue;
                }
                let pair = self
                    .graph
                    .pairs
                    .iter()
                    .find(|p| p.kinds.contains(&(kind as usize)))
                    .map(|p| p.kinds.clone())
                    .unwrap_or_default();
                for k in pair {
                    if k == kind as usize || !wants.iter().all(|&(l, d)| self.pad_along(k, l, d)) {
                        continue;
                    }
                    // Clear of other nets and without a new same-net near
                    // miss (which the same-net pass would have to patch).
                    if self.spatial.via_probe(k, at, net)? == Some(false) {
                        for s in &self.input.vias[k].shapes {
                            route.shapes.push(self.spatial.add_routed(Blockage {
                                layer: s.layer,
                                rect: shifted(s.rect, at)?,
                                net: Some(net),
                            })?);
                        }
                        via.kind = k as u32;
                        changed += 1;
                        break;
                    }
                }
            }
        }
        Ok(changed)
    }

    /// Would via `kind` at `at` land on a pin of its own net other than at
    /// that pin's escape top?
    fn via_on_own_pin(
        &mut self,
        kind: usize,
        at: [i64; 2],
        own: &crate::own_pins::OwnPins,
    ) -> Result<bool> {
        for shape in &self.input.vias[kind].shapes {
            if let Ok(rect) = shifted(shape.rect, at)
                && own.blocks(
                    shape.layer,
                    rect,
                    &[at],
                    &mut self.spatial.work,
                    self.limits.max_comparisons,
                )?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Prior geometry of `net` as a routed net, when it still reaches the
    /// planned escape top of every terminal and each shape is clear of fixed
    /// geometry and of other nets already in the index; otherwise `None` and
    /// nothing is added (the net routes from scratch).
    fn import_prior(
        &mut self,
        net: u32,
        terminals: &[&Terminal],
        plans: &HashMap<u32, Plan>,
        prior: &PriorRoutes,
    ) -> Result<Option<NetRoute>> {
        let Some((wires, vias)) = prior.nets.get(&net) else {
            return Ok(None);
        };
        if wires
            .iter()
            .any(|w| w.net != net || w.layer as usize >= self.input.layers.len())
            || vias
                .iter()
                .any(|v| v.net != net || v.kind as usize >= self.input.vias.len())
        {
            return Err(invalid(
                "prior route record of another net or outside the stack",
            ));
        }
        let on = |w: &Wire, p: [i64; 2]| {
            let (lx, ux) = (w.start[0].min(w.end[0]), w.start[0].max(w.end[0]));
            let (ly, uy) = (w.start[1].min(w.end[1]), w.start[1].max(w.end[1]));
            (lx..=ux).contains(&p[0]) && (ly..=uy).contains(&p[1]) && (lx == ux || ly == uy)
        };
        // Two axis-parallel centre lines on one layer share a point.
        let touch = |a: &Wire, b: &Wire| {
            a.layer == b.layer
                && a.start[0].min(a.end[0]).max(b.start[0].min(b.end[0]))
                    <= a.start[0].max(a.end[0]).min(b.start[0].max(b.end[0]))
                && a.start[1].min(a.end[1]).max(b.start[1].min(b.end[1]))
                    <= a.start[1].max(a.end[1]).min(b.start[1].max(b.end[1]))
        };
        for t in terminals {
            // A route may join a pin's escape anywhere; the escape beyond the
            // junction is then pruned from the stored result. The prior reaches
            // the terminal when its metal shares a point with the escape (same
            // layer) or lands a via of the escape, or covers the escape top.
            let plan = &plans[&t.pin];
            let (layer, p) = self.graph.point(plan.top);
            self.spatial.work = self
                .spatial
                .work
                .checked_add(
                    (wires.len() + vias.len())
                        .saturating_mul(plan.wires.len() + plan.vias.len() + 1),
                )
                .filter(|&n| n <= self.limits.max_comparisons)
                .ok_or_else(|| budget("prior route connectivity work"))?;
            let reached = wires.iter().any(|w| w.layer == layer && on(w, p))
                || vias.iter().any(|v| {
                    let k = &self.input.vias[v.kind as usize];
                    v.at == p && (k.lower == layer || k.upper == layer)
                })
                || plan.wires.iter().any(|e| wires.iter().any(|w| touch(w, e)))
                || plan
                    .vias
                    .iter()
                    .any(|e| vias.iter().any(|v| v.kind == e.kind && v.at == e.at));
            if !reached {
                self.reject_prior("terminal".into());
                return Ok(None);
            }
        }
        let mut rects = Vec::new();
        for w in wires {
            rects.push((
                w.layer,
                self.edge_rect(w.layer, w.start, w.end, net)?,
                false,
            ));
        }
        for v in vias {
            for s in &self.input.vias[v.kind as usize].shapes {
                rects.push((s.layer, shifted(s.rect, v.at)?, true));
            }
        }
        for &(layer, rect, via) in &rects {
            if self.spatial.probe(layer, rect, net)?.is_none() {
                if self.prior_rejects.is_some() {
                    let kind = if via { "via" } else { "wire" };
                    self.reject_prior(format!("{kind} {}", self.input.layers[layer as usize].name));
                }
                return Ok(None);
            }
        }
        let rects: Vec<_> = rects.into_iter().map(|(l, r, _)| (l, r)).collect();
        let mut route = NetRoute {
            wires: wires.clone(),
            vias: vias.clone(),
            shapes: Vec::with_capacity(rects.len()),
        };
        for (layer, rect) in rects {
            route.shapes.push(self.spatial.add_routed(Blockage {
                layer,
                rect,
                net: Some(net),
            })?);
        }
        Ok(Some(route))
    }

    /// Diagnostic tally of rejected prior nets (trace mode only).
    fn reject_prior(&mut self, reason: String) {
        if let Some(map) = self.prior_rejects.as_mut() {
            *map.entry(reason).or_insert(0) += 1;
        }
    }

    fn edge_rect(&self, layer: u32, a: [i64; 2], b: [i64; 2], net: u32) -> Result<Rect> {
        wire_rect(
            &Wire {
                net,
                layer,
                start: a,
                end: b,
            },
            self.width(layer),
        )
    }

    /// Route one net from its escapes. Hard mode: nothing may be crossed.
    /// Soft mode: routed metal of other nets is crossable at a price; the
    /// crossed nets are returned as victims.
    #[allow(clippy::too_many_arguments)]
    fn route_net(
        &mut self,
        net: u32,
        terminals: &[&Terminal],
        plans: &HashMap<u32, Plan>,
        soft: bool,
        strict: bool,
        rips: &BTreeMap<u32, usize>,
        round: usize,
    ) -> Result<Outcome> {
        let heuristic_weight = match self.plan.qor.as_ref() {
            Some(q)
                if !soft
                    && self.heuristic_weight_override.is_none()
                    && terminals.len() <= q.small_net_terminals =>
            {
                q.small_net_weight
            }
            _ => self.detail_weight(soft),
        };
        let large_trace = (terminals.len() >= 1024
            && std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some())
        .then(std::time::Instant::now);
        if let Some(started) = large_trace {
            trace(
                "large net",
                started,
                format_args!("net {net} terminals {} soft {soft}", terminals.len()),
            );
        }
        let mut out = NetRoute::default();
        let mut edges = BTreeMap::new();
        let root = plans[&terminals[0].pin].top;
        let (root_layer, root_point) = self.graph.point(root);
        if terminals.iter().any(|t| {
            !self
                .plan
                .allows_routing_layer(self.graph.point(plans[&t.pin].top).0)
        }) {
            return Err(no_route(
                "pin escape top lies outside signal-routing layers",
            ));
        }
        let mut target = crate::routing_target::TreeDistance::new(
            self.graph.slot[&root_layer],
            root_point,
            self.limits.max_target_segments,
        )?;
        let mut tree: HashSet<u32, BuildFx> = HashSet::with_hasher(BuildFx);
        tree.insert(root);
        // Tree junctions remember whether an actual incident pad meets AREA.
        let mut via_nodes: HashMap<u32, bool, BuildFx> = HashMap::with_hasher(BuildFx);
        if plans[&terminals[0].pin].top_via {
            via_nodes.insert(root, self.escape_pad_ok(&plans[&terminals[0].pin]));
        }
        let mut victims_all: BTreeSet<u32> = BTreeSet::new();
        let via_cost = self.plan.via_cost;
        let wrong = self.plan.wrong_way_factor;
        let conflict = if self.close_expanded && soft {
            if self.allowed_victim.is_some() {
                via_cost.saturating_mul(SINGLE_OWNER_VIAS)
            } else {
                via_cost.saturating_mul(NEW_OWNER_VIAS)
            }
        } else {
            via_cost.saturating_mul(10)
        };
        let near_cost = via_cost;
        let hist_unit = via_cost / 2;
        let zones = self.notch_zones.get(&net).cloned().unwrap_or_default();
        let excluded = self.own_exclusions.get(&net).cloned().unwrap_or_default();
        let mut nbuf = Vec::with_capacity(8);
        // Own pin metal, except where a terminal's escape reaches the graph:
        // a path that runs over, or drops a via onto, a pin of its own net
        // would connect that pin a second time (an RC loop through the pin
        // metal that extractors see as a mesh).
        let own_count = terminals.iter().try_fold(0usize, |n, t| {
            n.checked_add(t.shapes.len())
                .filter(|&n| n <= self.limits.max_active_nodes)
                .ok_or_else(|| budget("active detailed routing topology: same-net pin geometry"))
        })?;
        let mut own = Vec::new();
        own.try_reserve_exact(own_count)
            .map_err(|_| budget("same-net pin geometry allocation"))?;
        for t in terminals {
            let top = self.graph.point(plans[&t.pin].top).1;
            own.extend(t.shapes.iter().map(|s| (s.layer, s.rect, top)));
        }
        let own = crate::own_pins::OwnPins::with_index(
            own,
            self.input.layers.len(),
            &mut self.spatial.work,
            self.limits.max_comparisons,
            self.query_indices_override
                .as_ref()
                .map_or(self.plan.own_pin_index, |q| q.own_pin_index),
        )?;
        for (sink_index, t) in terminals[1..].iter().enumerate() {
            if let Some(started) = large_trace
                && sink_index.is_multiple_of(256)
            {
                trace(
                    "large net progress",
                    started,
                    format_args!(
                        "net {net} sink {sink_index}/{} expansions {}",
                        terminals.len(),
                        self.expansions
                    ),
                );
            }
            let start = plans[&t.pin].top;
            let start_via = plans[&t.pin].top_via;
            let start_pad_ok = self.escape_pad_ok(&plans[&t.pin]);
            if tree.contains(&start) {
                continue;
            }
            let mut tree_sorted = Vec::new();
            if self.close_expanded {
                self.history.ensure_prefix()?;
                if tree.len() > 1 {
                    tree_sorted
                        .try_reserve_exact(tree.len())
                        .map_err(|_| budget("routing tree sort allocation"))?;
                    tree_sorted.extend(tree.iter().copied());
                    tree_sorted.sort_unstable();
                }
            }
            let mut found = None;
            let mut limited = false;
            let mut touched_guide = false;
            let full_soft_limit = self
                .trial_search_limit
                .unwrap_or(self.limits.max_search_expansions);
            let stages: [(u64, usize, bool); 3] = if self.close_expanded && soft {
                let reuse_budget = self
                    .limits
                    .scouts
                    .as_ref()
                    .map_or(REUSE_ONLY_EXPANSIONS, |scouts| scouts.reuse_expansions);
                let soft_budget = self
                    .limits
                    .scouts
                    .as_ref()
                    .map_or(SOFT_SCOUT_EXPANSIONS, |scouts| scouts.soft_expansions);
                let reuse_limit = self
                    .limits
                    .max_hard_search_expansions
                    .unwrap_or(reuse_budget)
                    .min(reuse_budget)
                    .min(full_soft_limit);
                if self.allowed_victim.is_some() {
                    [
                        (heuristic_weight, reuse_limit, false),
                        (heuristic_weight, full_soft_limit, true),
                        (0, 0, true),
                    ]
                } else {
                    [
                        (heuristic_weight, reuse_limit, false),
                        (heuristic_weight, soft_budget.min(full_soft_limit), true),
                        if heuristic_weight == 3 && full_soft_limit <= soft_budget {
                            (0, 0, true)
                        } else {
                            (3, full_soft_limit, true)
                        },
                    ]
                }
            } else {
                let cap = if !soft {
                    if self.close_expanded
                        && let Some(limit) = self.trial_search_limit
                    {
                        limit
                    } else {
                        self.limits
                            .max_hard_search_expansions
                            .unwrap_or(self.limits.max_search_expansions)
                    }
                } else {
                    full_soft_limit
                };
                [(heuristic_weight, cap, true), (0, 0, true), (0, 0, true)]
            };
            for (stage, (pin_weight, pin_limit, new_owners_allowed)) in
                stages.into_iter().enumerate()
            {
                if pin_limit == 0 {
                    break;
                }
                self.epoch = self
                    .epoch
                    .checked_add(1)
                    .ok_or_else(|| budget("routing search count"))?;
                let epoch = self.epoch;
                // Prefer greater progress when priority costs tie. Taking
                // the smallest g first floods every equally short interior path.
                // Node id remains the final deterministic tie-break.
                let mut heap = crate::route_queue::Pending::new(self.limits.max_heap);
                self.search.begin(epoch);
                self.search.set(
                    start,
                    State {
                        cost: 0,
                        parent: NONE,
                        kind: WIRE,
                    },
                )?;
                heap.offer(
                    (
                        target
                            .distance(
                                self.graph.slot[&self.graph.point(start).0],
                                self.graph.point(start).1,
                                &self.target_costs,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )?
                            .saturating_mul(pin_weight),
                        Reverse(0u64),
                        start,
                    ),
                    &mut self.search,
                )?;
                limited = false;
                touched_guide = false;
                let mut projection_attempts = 0;
                let mut search = 0usize;
                while let Some((priority, Reverse(cost), id)) = heap.pop(&mut self.search) {
                    let close_nodes = self.close_expanded || (pin_weight > 1 && search >= 32_768);
                    if close_nodes && self.search.is_closed(id) {
                        continue;
                    }
                    if self.search.get(id).is_none_or(|s| s.cost != cost) {
                        continue;
                    }
                    if tree.contains(&id) {
                        found = Some(id);
                        break;
                    }
                    // A bounded hard search may defer to the existing wider-guide
                    // and rip-up attempts. Partial metal is removed by the same
                    // failed-attempt cleanup as an exhausted frontier. Soft-search
                    // and aggregate budgets still stop the command explicitly.
                    if !soft && search >= pin_limit {
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[troute] deferred hard search net {net} pin {} expansions {search}",
                                t.pin
                            );
                        }
                        limited = true;
                        break;
                    }
                    if self
                        .trial_expansion_limit
                        .is_some_and(|limit| self.expansions >= limit)
                    {
                        limited = true;
                        break;
                    }
                    if soft && search == pin_limit {
                        if pin_limit == full_soft_limit
                            && std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some()
                        {
                            eprintln!(
                                "[troute] search cap net {net} pin {} soft {soft} repair_step {round} ripups {} start {:?} root {:?} current {:?} tree_nodes {} targets {} cost {cost}",
                                t.pin,
                                rips.get(&net).copied().unwrap_or(0),
                                self.graph.point(start),
                                (root_layer, root_point),
                                self.graph.point(id),
                                tree.len(),
                                target.len(),
                            );
                        }
                        limited = true;
                        break;
                    }
                    self.tick(&mut search, "detailed", t.pin, net)?;
                    if close_nodes {
                        self.search.close(id);
                    }
                    let (l, t_, s_) = self.graph.locate(id);
                    let layer = self.graph.layers[l].layer;
                    let p = self.graph.xy(l, t_, s_);
                    let layer_milli = self.layer_milli[layer as usize];
                    // Axis of the wire step that reached this node (None after
                    // a via or at the start); only read for the bend surcharge.
                    let incoming_vertical = if self.bend_cost != 0 && id != start {
                        let state = self
                            .search
                            .get(id)
                            .ok_or_else(|| invalid("missing routing parent state"))?;
                        (state.kind == WIRE && state.parent != NONE)
                            .then(|| self.graph.point(state.parent))
                            .filter(|&(parent_layer, _)| parent_layer == layer)
                            .map(|(_, at)| at[0] == p[0])
                    } else {
                        None
                    };
                    let target_distance = target.bounds_distance(p);
                    let interior_distance = if target_distance == 0 {
                        // During this search the tree is fixed. A non-saturated
                        // queue score retains exactly g + weight*h, so reuse h.
                        Some(if priority != u64::MAX {
                            (priority - cost) / pin_weight
                        } else {
                            target.distance(
                                l,
                                p,
                                &self.target_costs,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )?
                        })
                    } else {
                        None
                    };
                    self.graph.neighbors(id, &mut nbuf);
                    // Dense channels otherwise retry nearly identical long-span
                    // probes at every unit stop. These optional edges are sampled;
                    // ordinary edges still cover every legal local detour.
                    if projection_attempts < MAX_TARGET_PROJECTIONS_PER_SINK
                        && (search <= 16 || s_.is_multiple_of(64))
                    {
                        projection_attempts += 1;
                        let work = self.graph.target_neighbors(id, root_point, &mut nbuf);
                        self.spatial.work = self
                            .spatial
                            .work
                            .checked_add(work)
                            .filter(|&n| n <= self.limits.max_comparisons)
                            .ok_or_else(|| budget("target projection work"))?;
                    }
                    // Minimum area depends on the incoming path, so it cannot be
                    // a general edge restriction in a one-cost-per-node search.
                    // Initial routes use bounded geometric cleanup; a net whose
                    // cleanup fails is retried with conservative strict rules.
                    // Area-aware ordinary searches forbid short routed stacks
                    // and, at level 2, short stacks on an escape's top metal;
                    // at level 1 an escape stack only costs more.
                    let mut escape_stack = false;
                    let no_stack =
                        (strict || self.area_level > 0) && !self.stack_ok[layer as usize] && {
                            let need = self.min_run[layer as usize];
                            let mut run = 0u64;
                            let mut cur = id;
                            let mut steps = 0usize;
                            loop {
                                if cur == start {
                                    escape_stack = start_via && !start_pad_ok && run < need;
                                    break (strict || self.area_level > 1) && escape_stack;
                                }
                                let previous = self
                                    .search
                                    .get(cur)
                                    .ok_or_else(|| invalid("missing routing parent state"))?;
                                if previous.kind != WIRE {
                                    let kind = previous.kind as usize;
                                    break !self.via_pad_ok[layer as usize][kind] && run < need;
                                }
                                let prev = self
                                    .search
                                    .get(cur)
                                    .ok_or_else(|| invalid("missing routing parent state"))?
                                    .parent;
                                run += distance(self.graph.point(cur).1, self.graph.point(prev).1)?;
                                cur = prev;
                                steps += 1;
                                if run >= need || steps > 64 {
                                    break false;
                                }
                            }
                        };
                    for &(next, mv) in nbuf.iter() {
                        if close_nodes && self.search.is_closed(next) {
                            continue;
                        }
                        let (nl, q) = self.graph.point(next);
                        if !self.plan.allows_routing_layer(nl) {
                            continue;
                        }
                        let mut next_distance = None;
                        if matches!(mv, Move::Along) && id.abs_diff(next) > 1 {
                            if let Some(current) = interior_distance {
                                let remaining = target.distance(
                                    self.graph.slot[&nl],
                                    q,
                                    &self.target_costs,
                                    &mut self.spatial.work,
                                    self.limits.max_comparisons,
                                )?;
                                if remaining >= current {
                                    continue;
                                }
                                next_distance = Some(remaining);
                            } else if target.bounds_distance(q) >= target_distance {
                                continue;
                            }
                        }
                        let joins = tree.contains(&next);
                        if self.gmark[self.cells.of(q) as usize] != self.gepoch && !joins {
                            touched_guide = true;
                            continue;
                        }
                        let mut step = match mv {
                            Move::Along => scaled(distance(p, q)?, layer_milli),
                            Move::Wrong => {
                                scaled(distance(p, q)?.saturating_mul(wrong), layer_milli)
                            }
                            Move::Via(pi) if escape_stack => self.pair_via[pi].saturating_mul(3),
                            Move::Via(pi) => self.pair_via[pi],
                        };
                        if self.bend_cost != 0
                            && !matches!(mv, Move::Via(_))
                            && incoming_vertical.is_some_and(|v| v != (p[0] == q[0]))
                        {
                            step = step.saturating_add(self.bend_cost);
                        }
                        if self
                            .search
                            .get(next)
                            .is_some_and(|s| s.cost <= cost.saturating_add(step))
                        {
                            continue;
                        }
                        let jump = if matches!(mv, Move::Along) {
                            id.abs_diff(next) as usize
                        } else {
                            1
                        };
                        if jump > 1 {
                            let from_cell = self.cells.of(p);
                            let to_cell = self.cells.of(q);
                            let stride = if p[0] == q[0] { self.cells.nx } else { 1 };
                            let cell_count = from_cell.abs_diff(to_cell) as usize / stride + 1;
                            let inspect_nodes = tree.len() > 1 || !self.history.is_zero();
                            let extra_work = if !inspect_nodes {
                                0
                            } else if self.close_expanded {
                                tree_sorted.len().max(1).ilog2() as usize + 1
                            } else {
                                jump
                            };
                            self.spatial.work = self
                                .spatial
                                .work
                                .checked_add(cell_count + extra_work)
                                .filter(|&n| n <= self.limits.max_comparisons)
                                .ok_or_else(|| budget("routing macro-move work"))?;
                            let before_end = if next > id { next - 1 } else { next + 1 };
                            let only_end_in_cell =
                                self.cells.of(self.graph.point(before_end).1) != to_cell;
                            let mut usable = true;
                            for cell in
                                (from_cell.min(to_cell)..=from_cell.max(to_cell)).step_by(stride)
                            {
                                if self.gmark[cell as usize] != self.gepoch
                                    && !(cell == to_cell && joins && only_end_in_cell)
                                {
                                    touched_guide = true;
                                    usable = false;
                                    break;
                                }
                            }
                            if tree.len() == 1 && id.min(next) < root && root < id.max(next) {
                                usable = false;
                            }
                            if usable && inspect_nodes {
                                if self.close_expanded {
                                    if tree.len() > 1 {
                                        let idx =
                                            tree_sorted.partition_point(|&n| n <= id.min(next));
                                        if tree_sorted.get(idx).is_some_and(|&n| n < id.max(next)) {
                                            usable = false;
                                        }
                                    }
                                    if usable && !self.history.is_zero() {
                                        let (h_lo, h_hi) = if next > id {
                                            (id + 1, next)
                                        } else {
                                            (next, id - 1)
                                        };
                                        let (h_sum, h_work) = self.history.range_sum(h_lo, h_hi);
                                        self.spatial.work = self
                                            .spatial
                                            .work
                                            .checked_add(h_work)
                                            .filter(|&n| n <= self.limits.max_comparisons)
                                            .ok_or_else(|| budget("routing macro-move work"))?;
                                        step = step.saturating_add(h_sum.saturating_mul(hist_unit));
                                    }
                                } else {
                                    for node in id.min(next)..=id.max(next) {
                                        if node == id {
                                            continue;
                                        }
                                        if node != next && tree.contains(&node) {
                                            usable = false;
                                            break;
                                        }
                                        if !self.history.is_zero() {
                                            step = step.saturating_add(
                                                u64::from(self.history.get(node))
                                                    .saturating_mul(hist_unit),
                                            );
                                        }
                                    }
                                }
                            }
                            if !usable {
                                continue;
                            }
                        } else {
                            step = step.saturating_add(
                                u64::from(self.history.get(next)).saturating_mul(hist_unit),
                            );
                        }
                        let base = cost.saturating_add(step);
                        if self.search.get(next).is_some_and(|s| s.cost <= base) {
                            continue;
                        }
                        let (kind, extra) = match mv {
                            Move::Via(pi) => {
                                if zoned_point(&excluded, layer, nl, p) {
                                    continue;
                                }
                                // A wire that ends here on this layer wants the
                                // pad elongated along it (end-of-line enclosure).
                                let original = self.graph.pairs[pi].kinds.clone();
                                let mut kinds = Vec::with_capacity(original.len());
                                for kind in original {
                                    if no_stack && !self.via_pad_ok[layer as usize][kind] {
                                        continue;
                                    }
                                    if strict
                                        && joins
                                        && via_nodes.get(&next) == Some(&false)
                                        && !self.via_pad_ok[nl as usize][kind]
                                    {
                                        continue;
                                    }
                                    if !self.via_on_own_pin(kind, p, &own)? {
                                        kinds.push(kind);
                                    }
                                }
                                if kinds.is_empty() {
                                    continue;
                                }
                                if id != start
                                    && self
                                        .search
                                        .get(id)
                                        .ok_or_else(|| invalid("missing routing parent state"))?
                                        .kind
                                        == WIRE
                                {
                                    let prev = self
                                        .graph
                                        .point(
                                            self.search
                                                .get(id)
                                                .ok_or_else(|| {
                                                    invalid("missing routing parent state")
                                                })?
                                                .parent,
                                        )
                                        .1;
                                    let vertical = prev[0] == p[0];
                                    kinds.sort_by_key(|k| !self.pad_along(*k, layer, vertical));
                                }
                                if soft {
                                    let mut best: Option<(u64, usize)> = None;
                                    for k in kinds {
                                        if let Soft::Clear { near, victims } =
                                            self.spatial.via_probe_soft(k, p, net)?
                                        {
                                            if self.allowed_victim.is_some_and(|allowed| {
                                                victims.iter().any(|&owner| owner != allowed)
                                            }) || self
                                                .protected
                                                .is_some_and(|p| victims.contains(&p))
                                            {
                                                continue;
                                            }
                                            if !new_owners_allowed
                                                && victims.iter().any(|v| !victims_all.contains(v))
                                            {
                                                continue;
                                            }
                                            let c = victims
                                                .iter()
                                                .map(|v| {
                                                    if self.close_expanded {
                                                        // Earlier sinks have already committed to
                                                        // removing this owner. Keep only a small
                                                        // traversal cost instead of paying for a
                                                        // second victim. No per-path owner sets.
                                                        if victims_all.contains(v) {
                                                            via_cost
                                                        } else if self.allowed_victim.is_none() {
                                                            conflict.saturating_mul(
                                                                1 + rips
                                                                    .get(v)
                                                                    .copied()
                                                                    .unwrap_or(0)
                                                                    as u64,
                                                            )
                                                        } else {
                                                            conflict
                                                        }
                                                    } else {
                                                        conflict.saturating_mul(
                                                            1 + rips.get(v).copied().unwrap_or(0)
                                                                as u64,
                                                        )
                                                    }
                                                })
                                                .fold(0u64, u64::saturating_add)
                                                .saturating_add(if near { near_cost } else { 0 });
                                            if near && (strict || zoned_point(&zones, layer, nl, p))
                                            {
                                                continue;
                                            }
                                            if best.is_none_or(|b| c < b.0) {
                                                best = Some((c, k));
                                            }
                                            if c == 0 {
                                                break;
                                            }
                                        }
                                    }
                                    match best {
                                        Some((c, k)) => (k as u32, c),
                                        None => continue,
                                    }
                                } else if strict || zoned_point(&zones, layer, nl, p) {
                                    let mut chosen = None;
                                    for k in kinds {
                                        if self.spatial.via_probe(k, p, net)? == Some(false) {
                                            chosen = Some(k);
                                            break;
                                        }
                                    }
                                    match chosen {
                                        Some(k) => (k as u32, 0),
                                        None => continue,
                                    }
                                } else {
                                    match self.spatial.choose(&kinds, p, net)? {
                                        Some(k) => (k as u32, 0),
                                        None => continue,
                                    }
                                }
                            }
                            _ => {
                                let rect = self.edge_rect(layer, p, q, net)?;
                                if zoned(&excluded, layer, rect) {
                                    continue;
                                }
                                if own.blocks(
                                    layer,
                                    rect,
                                    &[p, q],
                                    &mut self.spatial.work,
                                    self.limits.max_comparisons,
                                )? {
                                    continue;
                                }
                                if soft {
                                    match self.spatial.probe_soft(layer, rect, net)? {
                                        Soft::Blocked => continue,
                                        Soft::Clear { near: true, .. }
                                            if strict || zoned(&zones, layer, rect) =>
                                        {
                                            continue;
                                        }
                                        Soft::Clear { ref victims, .. }
                                            if self.allowed_victim.is_some_and(|allowed| {
                                                victims.iter().any(|&owner| owner != allowed)
                                            }) || self
                                                .protected
                                                .is_some_and(|p| victims.contains(&p)) =>
                                        {
                                            continue;
                                        }
                                        Soft::Clear { ref victims, .. }
                                            if !new_owners_allowed
                                                && victims
                                                    .iter()
                                                    .any(|v| !victims_all.contains(v)) =>
                                        {
                                            continue;
                                        }
                                        Soft::Clear { near, victims } => (
                                            WIRE,
                                            victims
                                                .iter()
                                                .map(|v| {
                                                    if self.close_expanded {
                                                        // Earlier sinks have already committed to
                                                        // removing this owner. Keep only a small
                                                        // traversal cost instead of paying for a
                                                        // second victim. No per-path owner sets.
                                                        if victims_all.contains(v) {
                                                            via_cost
                                                        } else if self.allowed_victim.is_none() {
                                                            conflict.saturating_mul(
                                                                1 + rips
                                                                    .get(v)
                                                                    .copied()
                                                                    .unwrap_or(0)
                                                                    as u64,
                                                            )
                                                        } else {
                                                            conflict
                                                        }
                                                    } else {
                                                        conflict.saturating_mul(
                                                            1 + rips.get(v).copied().unwrap_or(0)
                                                                as u64,
                                                        )
                                                    }
                                                })
                                                .fold(0u64, u64::saturating_add)
                                                .saturating_add(if near { near_cost } else { 0 }),
                                        ),
                                    }
                                } else {
                                    match self.spatial.probe(layer, rect, net)? {
                                        None => continue,
                                        Some(true) if strict || zoned(&zones, layer, rect) => {
                                            continue;
                                        }
                                        Some(near) => (WIRE, if near { near_cost } else { 0 }),
                                    }
                                }
                            }
                        };
                        let nc = base.saturating_add(extra);
                        if self.search.get(next).is_some_and(|s| s.cost <= nc) {
                            continue;
                        }
                        self.search.set(
                            next,
                            State {
                                cost: nc,
                                parent: id,
                                kind,
                            },
                        )?;
                        let remaining = match next_distance {
                            Some(value) => value,
                            None => target.distance(
                                self.graph.slot[&nl],
                                q,
                                &self.target_costs,
                                &mut self.spatial.work,
                                self.limits.max_comparisons,
                            )?,
                        };
                        heap.offer(
                            (
                                nc.saturating_add(remaining.saturating_mul(pin_weight)),
                                Reverse(nc),
                                next,
                            ),
                            &mut self.search,
                        )
                        .map_err(|e| budget(&format!("{e}; pin {} net {net}", t.pin)))?;
                    }
                }
                if soft && std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
                    eprintln!(
                        "[troute-search] {}",
                        serde_json::json!({
                            "round": round, "net": net, "pin": t.pin, "stage": stage,
                            "weight": pin_weight, "cap": pin_limit, "expansions": search,
                        "new_owners_allowed": new_owners_allowed,
                            "owner": self.allowed_victim, "selected_owners": victims_all.len(),
                            "status": if found.is_some() { "routed" } else if limited { "limited" } else { "exhausted" },
                        })
                    );
                }
                // Exhausting the reuse-only graph says nothing about a path
                // that may displace another owner. Continue to the soft stage.
                if found.is_some() || (!limited && new_owners_allowed) {
                    break;
                }
            }
            let Some(mut current) = found else {
                self.last_search_touched_guide = touched_guide;
                for id in out.shapes.drain(..) {
                    self.spatial.remove(id);
                }
                return Ok(if limited {
                    Outcome::Limited(t.pin)
                } else {
                    Outcome::Failed(t.pin)
                });
            };
            // Trace back, commit shapes so later sinks see this net's metal.
            let mut path = Vec::new();
            loop {
                let state = self
                    .search
                    .get(current)
                    .ok_or_else(|| invalid("missing routing trace state"))?;
                if state.parent == NONE {
                    break;
                }
                let prev = state.parent;
                let a = self.graph.locate(prev);
                let b = self.graph.locate(current);
                if state.kind == WIRE && a.0 == b.0 && a.1 == b.1 && prev.abs_diff(current) > 1 {
                    let mut at = current;
                    while at != prev {
                        if path.len() >= self.limits.max_search_expansions {
                            return Err(budget("routing macro trace limit"));
                        }
                        let before = if at < prev { at + 1 } else { at - 1 };
                        path.push((before, at, state.kind));
                        at = before;
                    }
                } else {
                    path.push((prev, current, state.kind));
                }
                current = prev;
                if path.len() > self.limits.max_search_expansions {
                    return Err(budget("routing trace limit"));
                }
            }
            // Collinear new wire edges of one track line become one shape.
            type Line = (u32, bool, i64);
            type Span = (i64, i64, u32, u32);
            let mut lines: BTreeMap<Line, Vec<Span>> = BTreeMap::new();
            for &(a, b, kind) in &path {
                let (la, pa) = self.graph.point(a);
                let (lb, pb) = self.graph.point(b);
                let key = (a.min(b), a.max(b));
                if edges.contains_key(&key) {
                    continue;
                }
                if edges.len() >= self.limits.max_segments {
                    return Err(budget("detailed routing net size"));
                }
                let new_nodes = usize::from(!tree.contains(&a)) + usize::from(!tree.contains(&b));
                if edges.len() >= self.limits.max_active_nodes
                    || tree.len().saturating_add(new_nodes) > self.limits.max_active_nodes
                {
                    return Err(budget("active detailed routing topology"));
                }
                edges.insert(key, kind);
                tree.insert(a);
                tree.insert(b);
                if la == lb {
                    let vertical = pa[0] == pb[0];
                    let (cross, lo, hi) = if vertical {
                        (pa[0], pa[1].min(pb[1]), pa[1].max(pb[1]))
                    } else {
                        (pa[1], pa[0].min(pb[0]), pa[0].max(pb[0]))
                    };
                    lines
                        .entry((la, vertical, cross))
                        .or_default()
                        .push((lo, hi, a, b));
                } else {
                    target.add(
                        self.graph.slot[&la],
                        pa,
                        pa,
                        &mut self.spatial.work,
                        self.limits.max_comparisons,
                    )?;
                    target.add(
                        self.graph.slot[&lb],
                        pb,
                        pb,
                        &mut self.spatial.work,
                        self.limits.max_comparisons,
                    )?;
                    for (node, layer) in [(a, la), (b, lb)] {
                        let enough = self.via_pad_ok[layer as usize][kind as usize];
                        via_nodes
                            .entry(node)
                            .and_modify(|value| *value |= enough)
                            .or_insert(enough);
                    }
                    if soft
                        && let Soft::Clear { victims, .. } =
                            self.spatial.via_probe_soft(kind as usize, pa, net)?
                        && !victims.is_empty()
                    {
                        victims_all.extend(victims);
                        self.note_history(b)?;
                    }
                    for s in &self.input.vias[kind as usize].shapes {
                        out.shapes.push(self.spatial.add_routed(Blockage {
                            layer: s.layer,
                            rect: shifted(s.rect, pa)?,
                            net: Some(net),
                        })?);
                    }
                }
            }
            for ((layer, vertical, cross), mut spans) in lines {
                spans.sort_unstable();
                let mut first = 0;
                while first < spans.len() {
                    let (lo, mut hi, _, _) = spans[first];
                    let mut end = first + 1;
                    while end < spans.len() && spans[end].0 <= hi {
                        hi = hi.max(spans[end].1);
                        end += 1;
                    }
                    let (pa, pb) = if vertical {
                        ([cross, lo], [cross, hi])
                    } else {
                        ([lo, cross], [hi, cross])
                    };
                    target.add(
                        self.graph.slot[&layer],
                        pa,
                        pb,
                        &mut self.spatial.work,
                        self.limits.max_comparisons,
                    )?;
                    let rect = self.edge_rect(layer, pa, pb, net)?;
                    if soft
                        && let Soft::Clear { victims, .. } =
                            self.spatial.probe_soft(layer, rect, net)?
                        && !victims.is_empty()
                    {
                        victims_all.extend(victims);
                        // A crossing is local to its conflicting unit edges.
                        // Marking the entire run also penalizes clear space and
                        // can exhaust sparse history on a single long wire.
                        for &(_, _, a, b) in &spans[first..end] {
                            let unit = self.edge_rect(
                                layer,
                                self.graph.point(a).1,
                                self.graph.point(b).1,
                                net,
                            )?;
                            match self.spatial.probe_soft(layer, unit, net)? {
                                Soft::Clear { victims, .. } if !victims.is_empty() => {
                                    self.note_history(b)?;
                                }
                                Soft::Clear { .. } => {}
                                Soft::Blocked => {
                                    return Err(invalid(
                                        "merged routing span contains blocked unit geometry",
                                    ));
                                }
                            }
                        }
                    }
                    out.shapes.push(self.spatial.add_routed(Blockage {
                        layer,
                        rect,
                        net: Some(net),
                    })?);
                    first = end;
                }
            }
            target.add(
                self.graph.slot[&self.graph.point(start).0],
                self.graph.point(start).1,
                self.graph.point(start).1,
                &mut self.spatial.work,
                self.limits.max_comparisons,
            )?;
            tree.insert(start);
            if start_via {
                via_nodes
                    .entry(start)
                    .and_modify(|value| *value |= start_pad_ok)
                    .or_insert(start_pad_ok);
            }
        }
        victims_all.remove(&net);
        let mut wires = Vec::new();
        for ((a, b), kind) in edges {
            let (la, pa) = self.graph.point(a);
            let (lb, pb) = self.graph.point(b);
            if la == lb {
                wires.push(Wire {
                    net,
                    layer: la,
                    start: pa,
                    end: pb,
                });
            } else {
                if kind == WIRE {
                    return Err(invalid("missing path via"));
                }
                out.vias.push(Via { net, kind, at: pa });
            }
        }
        let vias: Vec<&Via> = out
            .vias
            .iter()
            .chain(terminals.iter().flat_map(|t| plans[&t.pin].vias.iter()))
            .collect();
        let access: Vec<&Access> = terminals.iter().map(|t| &plans[&t.pin].access).collect();
        let pairs: Vec<(u32, u32)> = self.input.vias.iter().map(|v| (v.lower, v.upper)).collect();
        out.wires = crate::wire_runs::collinear_runs(
            &wires.iter().collect::<Vec<_>>(),
            &vias,
            &pairs,
            &access,
            self.limits.max_comparisons,
        )?;
        Ok(Outcome::Routed(out, victims_all.into_iter().collect()))
    }
}

/// Fill to reserve for a same-net notch between `a` and `b` on `layer` that
/// survived a re-route and that cleanup could not fill
/// (`RouteQor::notch_bridge`): the first of `route_rules::local_fills`
/// (robust box, stair bridge, bars clipped to two widths past the gap) that
/// no fixed geometry blocks and that routed metal of other nets does block,
/// with those nets (sorted) as the victims to re-route. A candidate that is
/// already clear is skipped (cleanup's own clearance test failed on geometry
/// this index does not hold). `None` when no candidate qualifies. A pure
/// function of the geometry and the index, so the choice is order-stable.
fn notch_fill_reservation(
    spatial: &mut Spatial<'_>,
    net: u32,
    layer: u32,
    a: Rect,
    b: Rect,
) -> Result<Option<(Vec<Rect>, BTreeSet<u32>)>> {
    let width = spatial.input.layers[layer as usize].width;
    let candidates = route_rules::local_fills(a, b, width, spatial.input.grid);
    'candidate: for rects in candidates {
        let mut victims = BTreeSet::new();
        for rect in &rects {
            match spatial.probe_soft(layer, *rect, net)? {
                Soft::Blocked => continue 'candidate,
                Soft::Clear { victims: v, .. } => victims.extend(v),
            }
        }
        victims.remove(&net);
        if !victims.is_empty() {
            return Ok(Some((rects, victims)));
        }
    }
    Ok(None)
}

/// Bounding box of a terminal's pin shapes.
fn pin_box(t: &Terminal) -> Rect {
    t.shapes.iter().skip(1).fold(t.shapes[0].rect, |r, s| Rect {
        lx: r.lx.min(s.rect.lx),
        ly: r.ly.min(s.rect.ly),
        ux: r.ux.max(s.rect.ux),
        uy: r.uy.max(s.rect.uy),
    })
}

/// Wire-step cost at a layer multiplier in thousandths (1000 is exact),
/// rounded up: the search bound scales whole distances and rounds down
/// (`routing_target::CostModel`), so a path of several fractional steps never
/// costs less than the bound charges for it.
pub(crate) fn scaled(cost: u64, milli: u64) -> u64 {
    if milli == 1000 {
        cost
    } else {
        cost.saturating_mul(milli).div_ceil(1000)
    }
}

fn trace(what: &str, start: std::time::Instant, extra: std::fmt::Arguments<'_>) {
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        let mib = |b: Option<u64>| b.map_or_else(|| "-".into(), |b| format!("{}", b >> 20));
        eprintln!(
            "[troute] {what} {:.3}s rss {} MiB peak {} MiB {extra}",
            start.elapsed().as_secs_f64(),
            mib(crate::memory::rss_bytes()),
            mib(crate::memory::peak_rss_bytes()),
        );
    }
}

/// The queue ledger includes hard successes and counts actual enqueues. Raw
/// victim counts alone overstate growth when a victim is already pending.
fn trace_repair_round(
    router: &Router<'_>,
    round: usize,
    net: u32,
    before: (usize, usize, usize),
    queue_after: usize,
    route: &NetRoute,
    victims: &[u32],
) {
    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
        let wire_length = route.wires.iter().try_fold(0u64, |sum, w| {
            sum.checked_add(w.start[0].abs_diff(w.end[0]))
                .and_then(|sum| sum.checked_add(w.start[1].abs_diff(w.end[1])))
        });
        eprintln!(
            "[troute-round] {}",
            serde_json::json!({
                "round": round, "net": net, "queue_before": before.0,
                "queue_after": queue_after, "victims": victims,
                "newly_queued": queue_after.saturating_add(1).saturating_sub(before.0),
                "expansions": router.expansions - before.1,
                "comparisons": router.spatial.work - before.2,
                "wire_length_dbu": wire_length, "vias": route.vias.len(),
            })
        );
    }
}

/// One non-recursive, bounded attempt to reconnect a displaced routed net.
/// First routed length per net and the bounded rejections of longer
/// re-routes (`RouteLimits::repair_detour`). Without a bound it admits all.
struct DetourLedger {
    bound: Option<crate::routing::DetourBound>,
    first: HashMap<u32, u64>,
    rejected: HashMap<u32, usize>,
    total: usize,
}
impl DetourLedger {
    fn new(bound: Option<crate::routing::DetourBound>) -> Self {
        Self {
            bound,
            first: HashMap::new(),
            rejected: HashMap::new(),
            total: 0,
        }
    }
    fn record(&mut self, net: u32, route: &NetRoute) {
        if self.bound.is_some() {
            self.first.entry(net).or_insert_with(|| route_length(route));
        }
    }
    /// Whether `route` may be retained for `net`; a refusal is counted. The
    /// first route of a net is its reference and is always admitted.
    fn admits(&mut self, net: u32, route: &NetRoute) -> bool {
        let Some(bound) = self.bound else {
            return true;
        };
        let length = route_length(route);
        let Some(&first) = self.first.get(&net) else {
            self.first.insert(net, length);
            return true;
        };
        if length <= bound.allowed(first) || self.total >= bound.max_rejections {
            return true;
        }
        let count = self.rejected.entry(net).or_insert(0);
        if *count >= bound.max_rejections_per_net {
            return true;
        }
        *count += 1;
        self.total += 1;
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            eprintln!(
                "[troute] detour rejected net {net} length {length} first {first} allowed {}",
                bound.allowed(first)
            );
        }
        false
    }
}

/// Manhattan wire length of a completed route (vias excluded).
fn route_length(route: &NetRoute) -> u64 {
    route
        .wires
        .iter()
        .map(|w| w.start[0].abs_diff(w.end[0]) + w.start[1].abs_diff(w.end[1]))
        .sum()
}

/// Remove a refused route's geometry; nothing else was retained for it.
fn discard_route(router: &mut Router<'_>, route: NetRoute) -> Result<()> {
    router.spatial.work = router
        .spatial
        .work
        .checked_add(route.shapes.len())
        .filter(|&n| n <= router.limits.max_comparisons)
        .ok_or_else(|| budget("detour rejection work"))?;
    for id in route.shapes {
        router.spatial.remove(id);
    }
    Ok(())
}

fn eager_hard_repair(
    router: &mut Router<'_>,
    net: u32,
    terminals: &[&Terminal],
    plans: &HashMap<u32, Plan>,
    strict: bool,
    rips: &BTreeMap<u32, usize>,
    round: usize,
) -> Result<Option<NetRoute>> {
    let Some(allowance) = router.limits.eager_repair_expansions else {
        return Ok(None);
    };
    if router.trial_search_limit.is_some()
        || router.trial_expansion_limit.is_some()
        || router.allowed_victim.is_some()
    {
        return Err(invalid("nested eager victim repair"));
    }
    let before = router.expansions;
    // An explicit per-search cap lets a displaced net search past the ordinary
    // hard handoff; the whole-net allowance below still bounds the attempt.
    router.trial_search_limit = Some(
        router.limits.eager_repair_search_expansions.unwrap_or(
            allowance.min(
                router
                    .limits
                    .max_hard_search_expansions
                    .unwrap_or(router.limits.max_search_expansions),
            ),
        ),
    );
    router.trial_expansion_limit = Some(before.saturating_add(allowance));
    let result = router.route_net(net, terminals, plans, false, strict, rips, round);
    // Global budget/memory failures remain errors, but never leave nested state.
    router.trial_search_limit = None;
    router.trial_expansion_limit = None;
    let repaired = match result? {
        Outcome::Routed(route, victims) => {
            if !victims.is_empty() {
                return Err(invalid("hard eager repair displaced another owner"));
            }
            Some(route)
        }
        Outcome::Failed(_) | Outcome::Limited(_) => None,
    };
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        eprintln!(
            "[troute] eager victim {net} repaired {} expansions {} cap {allowance}",
            repaired.is_some(),
            router.expansions - before,
        );
    }
    Ok(repaired)
}

/// Track-router entry point (called by `routing::route_cpu_ref`).
pub(crate) fn route(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
    route_with_prior(input, limits, None)
}

/// ECO routing: the geometry of an earlier routing of the same design, by
/// router net id of *this* input (the caller maps names and drops nets whose
/// pins changed). Only wires and vias are kept; cleanup patches are rebuilt.
#[derive(Clone, Debug, Default)]
pub struct PriorRoutes {
    pub nets: BTreeMap<u32, (Vec<Wire>, Vec<Via>)>,
}

/// Track routing that first keeps every prior net whose geometry still
/// reaches each planned escape top of its terminals and clears the fixed
/// shapes and the nets kept before it; all other nets route as usual and may
/// rip kept nets up. Cleanup, rule repair and `validate` treat kept geometry
/// like any routed net, so the result is checked exactly like a full route.
pub(crate) fn route_with_prior(
    input: &RouteInput,
    limits: &RouteLimits,
    prior: Option<&PriorRoutes>,
) -> Result<Routes> {
    let t0 = std::time::Instant::now();
    let plan = input
        .tracks
        .as_ref()
        .ok_or_else(|| invalid("track plan absent"))?;
    let graph = Graph::new(input, plan, limits)?;
    let cells = Cells::new(input.die, plan.gcell)?;
    let estimate = memory_estimate_limited(input, graph.len, cells.len(), limits)?;
    trace(
        "estimate",
        t0,
        format_args!(
            "nodes {} gcells {} obstacles {} terminals {} {estimate:?} total {} MiB (cap {} MiB)",
            graph.len,
            cells.len(),
            input.obstacles.len(),
            input.terminals.len(),
            estimate.total() >> 20,
            plan.memory_mib
        ),
    );
    let cap = plan.memory_mib.saturating_mul(1 << 20);
    if estimate.total() > cap {
        return Err(budget(&format!(
            "track router memory estimate {} MiB exceeds memory_mib {} \
             (held {} search {} spatial {} routed {} check {} MiB)",
            estimate.total() >> 20,
            plan.memory_mib,
            estimate.held >> 20,
            estimate.search >> 20,
            estimate.spatial >> 20,
            estimate.routed >> 20,
            estimate.check >> 20
        )));
    }
    let mut groups = BTreeMap::<u32, Vec<&Terminal>>::new();
    for t in &input.terminals {
        groups.entry(t.net).or_default().push(t);
    }
    groups.retain(|_, g| g.len() >= 2);
    for g in groups.values_mut() {
        g.sort_by_key(|p| (!p.driver, p.pin));
        if g.iter().filter(|p| p.driver).count() != 1 {
            return Err(invalid("routed net needs one driver"));
        }
    }
    let mut r = Router::prepared(input, limits, graph, cells)?;
    r.guard("setup")?;
    trace("spatial index", t0, format_args!("work {}", r.spatial.work));
    let all: Vec<&Terminal> = groups.values().flatten().copied().collect();
    let plans = r.reserve(&all)?;
    r.guard("pin access")?;
    trace(
        "access",
        t0,
        format_args!(
            "terminals {} work {} exp {}",
            all.len(),
            r.spatial.work,
            r.expansions
        ),
    );
    drop(all);
    if r.plan.qor.as_ref().is_some_and(|q| q.prim_order) {
        for group in groups.values_mut() {
            prim_terminal_order(
                group,
                |t| r.graph.point(plans[&t.pin].top).1,
                &mut r.spatial.work,
                limits.max_comparisons,
            )?;
        }
    }
    // Global routing over the escape tops.
    let nets: Vec<(u32, Vec<u32>)> = groups
        .iter()
        .map(|(net, g)| {
            let mut cells: Vec<u32> = g
                .iter()
                .map(|t| r.cells.of(r.graph.point(plans[&t.pin].top).1))
                .collect();
            let first = cells[0];
            cells.sort_unstable();
            cells.dedup();
            let pos = cells.iter().position(|c| *c == first).unwrap();
            cells.swap(0, pos);
            (*net, cells)
        })
        .collect();
    let guides = r.global(&nets, &plans)?;
    r.guard("global routing")?;
    trace(
        "global",
        t0,
        format_args!(
            "nets {} work {} exp {}",
            nets.len(),
            r.spatial.work,
            r.expansions
        ),
    );
    drop(nets);
    if r.plan.qor.as_ref().is_some_and(|q| q.drop_portals) {
        for group in groups.values() {
            r.release_portals(group, &plans);
        }
    }
    // Detailed routing: small nets first.
    let mut order: Vec<(usize, u32)> = guides.iter().map(|(n, g)| (g.len(), *n)).collect();
    order.sort_unstable();
    let mut routed: BTreeMap<u32, NetRoute> = BTreeMap::new();
    let mut retained_edges = 0usize;
    let mut queue: VecDeque<u32> = VecDeque::new();
    let none = BTreeMap::new();
    let mut detour = DetourLedger::new(limits.repair_detour);
    if let Some(prior) = prior {
        let mut kept = 0usize;
        for (&net, terminals) in &groups {
            if let Some(route) = r.import_prior(net, terminals, &plans, prior)? {
                detour.record(net, &route);
                retain_route(
                    &mut routed,
                    &mut retained_edges,
                    net,
                    route,
                    limits.max_segments,
                )?;
                kept += 1;
            }
        }
        r.guard("prior routes")?;
        trace(
            "prior",
            t0,
            format_args!(
                "kept {kept} of {} nets ({} offered) work {} rejected {:?}",
                groups.len(),
                prior.nets.len(),
                r.spatial.work,
                r.prior_rejects
            ),
        );
    }
    for (index, &(_, net)) in order.iter().enumerate() {
        if routed.contains_key(&net) {
            continue;
        }
        if index.is_multiple_of(1000) {
            let (_, entries, shapes, _) = r.spatial.footprint();
            trace(
                "detailed progress",
                t0,
                format_args!(
                    "nets {index}/{} failed {} shapes {shapes} bin_entries {entries} work {} exp {}",
                    order.len(),
                    queue.len(),
                    r.spatial.work,
                    r.expansions
                ),
            );
        }
        let g = &guides[&net];
        let mut done = false;
        for expand_by in [None, Some(r.plan.expand_margin)] {
            if r.close_expanded {
                if let Some(m) = expand_by {
                    r.mark_region_corridor(g, m);
                } else {
                    r.mark_region(g, None);
                }
            } else {
                r.mark_region(g, expand_by);
            }
            match r.route_net(net, &groups[&net], &plans, false, false, &none, 0)? {
                Outcome::Routed(route, _) => {
                    r.release_portals(&groups[&net], &plans);
                    detour.record(net, &route);
                    retain_route(
                        &mut routed,
                        &mut retained_edges,
                        net,
                        route,
                        limits.max_segments,
                    )?;
                    done = true;
                    break;
                }
                Outcome::Failed(_) if r.close_expanded && !r.last_search_touched_guide => {
                    break;
                }
                Outcome::Limited(pin) if r.close_expanded => {
                    if let Some(group) = groups.get_mut(&net)
                        && group
                            .get(1)
                            .is_some_and(|t| t.pin == pin && t.class.is_some())
                    {
                        group.swap(0, 1);
                    }
                }
                _ => {}
            }
        }
        if !done {
            queue.push_back(net);
        }
    }
    trace(
        "detailed",
        t0,
        format_args!(
            "failed {} work {} exp {}",
            queue.len(),
            r.spatial.work,
            r.expansions
        ),
    );
    let mut rips: BTreeMap<u32, usize> = BTreeMap::new();
    let mut strict: BTreeSet<u32> = BTreeSet::new();
    // Nets whose strict re-route found no path; they get one ordinary retry.
    let mut relaxed: BTreeSet<u32> = BTreeSet::new();
    // Area-rule levels a net lost after the rule walled it in (at most 2).
    let mut demoted: BTreeMap<u32, u8> = BTreeMap::new();
    // Wide-metal pairs (owner, other, layer) -> repairs already tried.
    let mut wide_seen: BTreeMap<(u32, u32, u32), u8> = BTreeMap::new();
    let mut cycle_tried: BTreeSet<u32> = BTreeSet::new();
    // Remaining violations by exact geometry -> passes seen, and the notch
    // zones placed for them (bounded by `MAX_NOTCH_ZONES`).
    let mut rule_seen: BTreeMap<String, u8> = BTreeMap::new();
    let mut zone_count = 0usize;
    // Same-net notch fills reserved as fixed metal (net, layer, rects).
    let mut notch_reserved: BTreeSet<(u32, u32, Vec<[i64; 4]>)> = BTreeSet::new();
    let notch_bridge = r.plan.qor.as_ref().is_some_and(|q| q.notch_bridge);
    // Directed rip counts (ripper, victim), at most `MAX_PAIR_RIPS` pairs. Two
    // nets that keep ripping each other rebuild the same pair of routes every
    // time (their rip prices grow together), so once both directions reach
    // `PAIR_YIELD` the net just ripped yields: its soft search treats the
    // partner as blocked. A yield that walls the net in is dropped; each
    // direction is tried once.
    let mut pair_rips: BTreeMap<(u32, u32), u16> = BTreeMap::new();
    let mut yields: BTreeMap<u32, u32> = BTreeMap::new();
    let mut yield_tried: BTreeSet<(u32, u32)> = BTreeSet::new();
    if !queue.is_empty() {
        for group in groups.values_mut() {
            repair_terminal_order(
                group,
                |t| r.graph.point(plans[&t.pin].top).1,
                &mut r.spatial.work,
                limits.max_comparisons,
            )?;
        }
    }
    let mut round = 0usize;
    let via_layers: Vec<(u32, u32)> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
    for pass in 0..=MAX_RULE_PASS {
        // Repair: negotiated rip-up and reroute.
        while let Some(net) = queue.pop_front() {
            round += 1;
            let round_before = (queue.len() + 1, r.expansions, r.spatial.work);
            if round.is_multiple_of(100) {
                trace(
                    "repair progress",
                    t0,
                    format_args!(
                        "rounds {round} queued {} work {} exp {}",
                        queue.len(),
                        r.spatial.work,
                        r.expansions
                    ),
                );
            }
            if round > r.plan.max_repair_rounds {
                return Err(no_route(&format!(
                    "repair rounds exhausted, {} nets unrouted (net {net} first)",
                    queue.len() + 1
                )));
            }
            if r.close_expanded && r.spatial.should_compact() {
                r.spatial.compact_dead()?;
            }
            let g = &guides[&net];
            let tight = strict.contains(&net);
            r.area_level = if r.plan.area_stacks {
                2 - demoted.get(&net).copied().unwrap_or(0)
            } else {
                0
            };
            if r.close_expanded {
                r.mark_region_corridor(g, r.plan.expand_margin);
            } else {
                r.mark_region(g, Some(r.plan.expand_margin));
            }
            let hard = r
                .route_net(net, &groups[&net], &plans, false, tight, &rips, round)
                .inspect_err(|error| {
                    replay::capture_budget_error(
                        &r,
                        error,
                        replay::BudgetCapturePoint {
                            net,
                            terminals: &groups[&net],
                            plans: &plans,
                            strict: tight,
                            rips: &rips,
                            round,
                            stage: replay::BudgetErrorStage::RepairHard,
                        },
                    )
                })?;
            let mut repair_pin = match hard {
                Outcome::Routed(route, _) if detour.admits(net, &route) => {
                    retain_route(
                        &mut routed,
                        &mut retained_edges,
                        net,
                        route,
                        limits.max_segments,
                    )?;
                    trace_repair_round(
                        &r,
                        round,
                        net,
                        round_before,
                        queue.len(),
                        &routed[&net],
                        &[],
                    );
                    continue;
                }
                Outcome::Routed(route, _) => {
                    discard_route(&mut r, route)?;
                    None
                }
                Outcome::Failed(pin) => Some(pin),
                Outcome::Limited(pin) => {
                    if r.close_expanded
                        && let Some(group) = groups.get_mut(&net)
                    {
                        let group_len = group.len();
                        if let Some(pos) = group
                            .iter()
                            .position(|t| t.pin == pin && (group_len == 2 || t.class.is_some()))
                            && pos > 0
                        {
                            group.swap(0, pos);
                        }
                    }
                    None
                }
            };
            let hard_touched_guide = r.last_search_touched_guide;
            let wide = r
                .plan
                .expand_margin
                .saturating_mul(2)
                .saturating_add(rips.get(&net).copied().unwrap_or(0).min(16));
            if r.close_expanded {
                r.mark_region_corridor(g, wide);
            } else {
                r.mark_region(g, Some(wide));
            }
            if r.close_expanded && (repair_pin.is_none() || hard_touched_guide) {
                let prev_weight = r.heuristic_weight_override;
                let prev_limit = r.trial_search_limit;
                if repair_pin.is_some() {
                    r.heuristic_weight_override = Some(r.plan.repair_heuristic_weight.unwrap_or(2));
                } else {
                    r.trial_search_limit = Some(
                        r.limits
                            .scouts
                            .as_ref()
                            .map_or(25_000, |scouts| scouts.wide_hard_expansions)
                            .min(r.limits.max_search_expansions),
                    );
                }
                let wide_hard = r.route_net(net, &groups[&net], &plans, false, tight, &rips, round);
                r.heuristic_weight_override = prev_weight;
                r.trial_search_limit = prev_limit;
                let wide_hard = wide_hard.inspect_err(|error| {
                    replay::capture_budget_error(
                        &r,
                        error,
                        replay::BudgetCapturePoint {
                            net,
                            terminals: &groups[&net],
                            plans: &plans,
                            strict: tight,
                            rips: &rips,
                            round,
                            stage: replay::BudgetErrorStage::RepairHard,
                        },
                    )
                })?;
                match wide_hard {
                    Outcome::Routed(route, _) if detour.admits(net, &route) => {
                        retain_route(
                            &mut routed,
                            &mut retained_edges,
                            net,
                            route,
                            limits.max_segments,
                        )?;
                        trace_repair_round(
                            &r,
                            round,
                            net,
                            round_before,
                            queue.len(),
                            &routed[&net],
                            &[],
                        );
                        continue;
                    }
                    Outcome::Routed(route, _) => {
                        discard_route(&mut r, route)?;
                        repair_pin = None;
                    }
                    Outcome::Failed(pin) => repair_pin = Some(pin),
                    Outcome::Limited(_) => repair_pin = None,
                }
            }
            replay::capture_if_requested(&r, net, &groups[&net], &plans, tight, &rips, round)?;
            r.protected = yields.get(&net).copied();
            let owner_config = r.plan.single_owner_repair.clone();
            let mut owner_allocation = owner_config.as_ref().map(owner_repair::Allocation::new);
            let single = if let (Some(config), Some(failed_pin)) = (&owner_config, repair_pin) {
                owner_repair::run(
                    &mut r,
                    &owner_repair::Request {
                        config,
                        net,
                        terminals: &groups[&net],
                        plans: &plans,
                        strict: tight,
                        rips: &rips,
                        round,
                        failed_pin,
                    },
                    owner_allocation.as_mut().expect("owner allocation"),
                )
                .inspect_err(|error| {
                    replay::capture_budget_error(
                        &r,
                        error,
                        replay::BudgetCapturePoint {
                            net,
                            terminals: &groups[&net],
                            plans: &plans,
                            strict: tight,
                            rips: &rips,
                            round,
                            stage: replay::BudgetErrorStage::RepairOwner,
                        },
                    )
                })?
            } else {
                None
            };
            let mut outcome = if let Some((route, victims)) = single {
                Outcome::Routed(route, victims)
            } else if let Some(config) = &owner_config {
                owner_repair::soft(
                    &mut r,
                    &owner_repair::Request {
                        config,
                        net,
                        terminals: &groups[&net],
                        plans: &plans,
                        strict: tight,
                        rips: &rips,
                        round,
                        failed_pin: repair_pin.unwrap_or(groups[&net][0].pin),
                    },
                    owner_allocation.as_mut().expect("owner allocation"),
                )
                .inspect_err(|error| {
                    replay::capture_budget_error(
                        &r,
                        error,
                        replay::BudgetCapturePoint {
                            net,
                            terminals: &groups[&net],
                            plans: &plans,
                            strict: tight,
                            rips: &rips,
                            round,
                            stage: replay::BudgetErrorStage::RepairSoft,
                        },
                    )
                })?
            } else {
                r.route_net(net, &groups[&net], &plans, true, tight, &rips, round)
                    .inspect_err(|error| {
                        replay::capture_budget_error(
                            &r,
                            error,
                            replay::BudgetCapturePoint {
                                net,
                                terminals: &groups[&net],
                                plans: &plans,
                                strict: tight,
                                rips: &rips,
                                round,
                                stage: replay::BudgetErrorStage::RepairSoft,
                            },
                        )
                    })?
            };
            // A strict failure is walled in by the strict rules themselves;
            // relaxing the net is cheaper than a full alternate-root search.
            let retry_needed = if r.close_expanded {
                matches!(outcome, Outcome::Limited(_))
                    || (!tight && matches!(outcome, Outcome::Failed(_)))
            } else {
                matches!(outcome, Outcome::Limited(_))
            };
            if retry_needed {
                if r.close_expanded {
                    r.mark_region(g, Some(wide));
                }
                let swapped = if r.close_expanded {
                    if let Outcome::Failed(failed_pin) = outcome
                        && let Some(pos) = groups[&net].iter().position(|t| t.pin == failed_pin)
                        && pos > 0
                    {
                        groups.get_mut(&net).unwrap().swap(0, pos);
                        Ok(true)
                    } else {
                        alternate_repair_root_by_distance(
                            groups.get_mut(&net).unwrap(),
                            |t| r.graph.point(plans[&t.pin].top).1,
                            &mut r.spatial.work,
                            limits.max_comparisons,
                        )
                    }
                } else {
                    alternate_repair_root(
                        groups.get_mut(&net).unwrap(),
                        |t| r.graph.point(plans[&t.pin].top).1,
                        &mut r.spatial.work,
                        limits.max_comparisons,
                    )
                }
                .inspect_err(|error| {
                    replay::capture_budget_error(
                        &r,
                        error,
                        replay::BudgetCapturePoint {
                            net,
                            terminals: &groups[&net],
                            plans: &plans,
                            strict: tight,
                            rips: &rips,
                            round,
                            stage: replay::BudgetErrorStage::RepairAlternateRoot,
                        },
                    )
                })?;
                if swapped || r.close_expanded {
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        eprintln!("[troute] bounded alternate-root retry net {net}");
                    }
                    let prev_weight = r.heuristic_weight_override;
                    if r.close_expanded {
                        r.heuristic_weight_override = Some(1);
                    }
                    let retry_outcome = if let Some(config) = &owner_config {
                        owner_repair::soft(
                            &mut r,
                            &owner_repair::Request {
                                config,
                                net,
                                terminals: &groups[&net],
                                plans: &plans,
                                strict: tight,
                                rips: &rips,
                                round,
                                failed_pin: repair_pin.unwrap_or(groups[&net][0].pin),
                            },
                            owner_allocation.as_mut().expect("owner allocation"),
                        )
                    } else {
                        r.route_net(net, &groups[&net], &plans, true, tight, &rips, round)
                    };
                    r.heuristic_weight_override = prev_weight;
                    outcome = retry_outcome.inspect_err(|error| {
                        replay::capture_budget_error(
                            &r,
                            error,
                            replay::BudgetCapturePoint {
                                net,
                                terminals: &groups[&net],
                                plans: &plans,
                                strict: tight,
                                rips: &rips,
                                round,
                                stage: replay::BudgetErrorStage::RepairAlternateSoft,
                            },
                        )
                    })?;
                }
            }
            if r.protected.take().is_some()
                && matches!(outcome, Outcome::Failed(_) | Outcome::Limited(_))
            {
                // Yielding walled the net in: route it as before.
                if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                    eprintln!("[troute] yield walls in net {net}, dropped");
                }
                yields.remove(&net);
                queue.push_back(net);
                continue;
            }
            match outcome {
                Outcome::Limited(pin) => {
                    let captured = replay::capture_at_phase(
                        &r,
                        net,
                        &groups[&net],
                        &plans,
                        tight,
                        &rips,
                        replay::CaptureMoment::post_failure(round, pin),
                    );
                    replay::finish_limited_capture(&r, captured, net, round)?;
                    return Err(budget(&format!(
                        "per-search detailed limit after bounded root retries, pin {pin} net {net}"
                    )));
                }
                Outcome::Failed(pin) => {
                    if r.own_exclusions.remove(&net).is_some() {
                        // An exclusion walled the net in: route it as before.
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!("[troute] exclusion walls in net {net} pin {pin}, dropped");
                        }
                        queue.push_back(net);
                        continue;
                    }
                    // Strict rules (no near misses, no short stacked pads) can
                    // wall in a pin; the ordinary mode plus cleanup and the
                    // final rule check stays sound, so retry once without them.
                    if tight && relaxed.insert(net) {
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[troute] strict re-route failed, relaxing net {net} pin {pin}"
                            );
                        }
                        strict.remove(&net);
                        queue.push_back(net);
                        continue;
                    }
                    if !tight && r.notch_zones.remove(&net).is_some() {
                        // A notch zone walled the net in: route it as before.
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!("[troute] notch zone walls in net {net} pin {pin}, dropped");
                        }
                        queue.push_back(net);
                        continue;
                    }
                    if r.area_level > 0 {
                        *demoted.entry(net).or_insert(0) += 1;
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[troute] area-aware route failed, level {} for net {net} pin {pin}",
                                r.area_level - 1
                            );
                        }
                        queue.push_back(net);
                        continue;
                    }
                    return Err(no_route(&format!(
                        "pin {pin} net {net}: no path even with rip-up (fixed geometry blocks)"
                    )));
                }
                Outcome::Routed(route, victims) => {
                    for &v in &victims {
                        if !pair_rips.contains_key(&(net, v)) && pair_rips.len() >= MAX_PAIR_RIPS {
                            continue;
                        }
                        let c = pair_rips.entry((net, v)).or_insert(0);
                        *c = c.saturating_add(1);
                        if *c >= PAIR_YIELD
                            && pair_rips
                                .get(&(v, net))
                                .is_some_and(|&back| back >= PAIR_YIELD)
                            && yield_tried.insert((v, net))
                        {
                            if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                                eprintln!("[troute] rip cycle nets {net} and {v}: {v} yields");
                            }
                            yields.insert(v, net);
                        }
                    }
                    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
                        eprintln!(
                            "[troute] round {round} net {net} soft victims {:?} work {} exp {}",
                            victims, r.spatial.work, r.expansions
                        );
                    }
                    detour.record(net, &route);
                    retain_route(
                        &mut routed,
                        &mut retained_edges,
                        net,
                        route,
                        limits.max_segments,
                    )?;
                    let eager_enabled = limits.eager_repair_expansions.is_some()
                        && victims.len() <= limits.eager_repair_max_victims.unwrap_or(1);
                    // At most sixteen IDs; no displaced route is kept alive.
                    let mut eager_candidates =
                        Vec::with_capacity(if eager_enabled { victims.len() } else { 0 });
                    for &v in &victims {
                        let c = rips.entry(v).or_insert(0);
                        *c += 1;
                        if *c > r.plan.max_ripups_per_net {
                            return Err(no_route(&format!(
                                "net {v} ripped up more than {} times",
                                r.plan.max_ripups_per_net
                            )));
                        }
                        let had_route =
                            if let Some(old) = remove_route(&mut routed, &mut retained_edges, v) {
                                for id in old.shapes {
                                    r.spatial.remove(id);
                                }
                                true
                            } else {
                                false
                            };
                        if eager_enabled && had_route {
                            eager_candidates.push(v);
                        }
                    }
                    // Free all affected old routes first, then try to reconnect
                    // within the resulting space without displacing third parties.
                    r.spatial.work = r
                        .spatial
                        .work
                        .checked_add(victims.len().saturating_mul(eager_candidates.len()))
                        .filter(|&n| n <= limits.max_comparisons)
                        .ok_or_else(|| budget("eager candidate membership work"))?;
                    for &v in &victims {
                        if eager_candidates.contains(&v) {
                            let guide = &guides[&v];
                            if r.close_expanded {
                                r.mark_region_corridor(guide, wide);
                            } else {
                                r.mark_region(guide, Some(wide));
                            }
                            if let Some(repaired) = eager_hard_repair(
                                &mut r,
                                v,
                                &groups[&v],
                                &plans,
                                strict.contains(&v),
                                &rips,
                                round,
                            )? {
                                if detour.admits(v, &repaired) {
                                    retain_route(
                                        &mut routed,
                                        &mut retained_edges,
                                        v,
                                        repaired,
                                        limits.max_segments,
                                    )?;
                                    r.spatial.work = r
                                        .spatial
                                        .work
                                        .checked_add(queue.len())
                                        .filter(|&n| n <= limits.max_comparisons)
                                        .ok_or_else(|| budget("eager repair queue work"))?;
                                    queue.retain(|pending| *pending != v);
                                    continue;
                                }
                                discard_route(&mut r, repaired)?;
                            }
                        }
                        if !queue.contains(&v) {
                            queue.push_back(v);
                        }
                    }
                    trace_repair_round(
                        &r,
                        round,
                        net,
                        round_before,
                        queue.len(),
                        &routed[&net],
                        &victims,
                    );
                }
            }
        }
        trace(
            "repair",
            t0,
            format_args!(
                "pass {pass} rounds {round} rips {} detour_rejections {} work {} exp {}",
                rips.values().sum::<usize>(),
                detour.total,
                r.spatial.work,
                r.expansions
            ),
        );
        r.guard("repair")?;
        if r.plan
            .qor
            .as_ref()
            .is_some_and(|q| q.release_portals || q.drop_portals)
        {
            let nets: Vec<u32> = routed.keys().copied().collect();
            for net in nets {
                r.release_portals(&groups[&net], &plans);
            }
        }
        let oriented = r.orient_vias(&mut routed)?;
        trace("orient", t0, format_args!("vias re-picked {oriented}"));
        let mut out = assemble(&r, &groups, &plans, &routed, input, limits, &via_layers)?;
        let (work, remaining) = crate::routing::cleanup_report(input, &mut out, limits)?;
        r.guard("rule repair")?;
        trace(
            "cleanup",
            t0,
            format_args!(
                "pass {pass} work {work} remaining {} patches {}",
                remaining.len(),
                out.patches.len()
            ),
        );
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            for violation in remaining.iter().take(16) {
                eprintln!("[troute] rule repair {}", violation.describe());
            }
        }
        if let Some(path) = std::env::var_os("RUSTROAD_RULE_DUMP") {
            dump_rule_state(std::path::Path::new(&path), pass, &out, &remaining)?;
        }
        out.stats.comparisons = out
            .stats
            .comparisons
            .checked_add(work)
            .ok_or_else(|| budget("routing comparison limit"))?;
        if remaining.is_empty() {
            if out.stats.comparisons > limits.max_comparisons {
                return Err(budget("routing comparison limit"));
            }
            out.stats.wire_length_dbu = out.wires.iter().try_fold(0u64, |n, w| {
                n.checked_add(distance(w.start, w.end)?)
                    .ok_or_else(|| budget("total wire length"))
            })?;
            validate(input, &out, limits)?;
            r.guard("validation")?;
            let fp = r.spatial.footprint();
            trace(
                "validated",
                t0,
                format_args!(
                    "records {} wires {} vias {} access {} patches {} bins {} entries {} shapes {} bin {} obstacles {} terminals {} die {:?} layers {}",
                    out.wires.len() + out.vias.len(),
                    out.wires.len(),
                    out.vias.len(),
                    out.access.len(),
                    out.patches.len(),
                    fp.0,
                    fp.1,
                    fp.2,
                    fp.3,
                    input.obstacles.len(),
                    input.terminals.len(),
                    input.die,
                    input.layers.len()
                ),
            );
            return Ok(out);
        }
        if pass == MAX_RULE_PASS {
            let v = &remaining[0];
            return Err(no_route(&format!(
                "ROUTING_RULE unresolved {} ({} total)",
                v.describe(),
                remaining.len()
            )));
        }
        // Re-route the offending nets in strict mode (no near misses). A
        // wide-metal pair that survives a repair is fixed geometry: an escape
        // U-notch or a routed loop of the owner, filled by cleanup, or a pin
        // escape of the neighbour. Second time: a fixed halo of the owner
        // (wide parts grown by wide minus normal spacing) keeps the routed
        // neighbour out, and the neighbour moves. Third time: a halo of the
        // neighbour's facing metal near the wide parts keeps the owner out,
        // and the owner moves. A halo is only placed where it keeps clear of
        // every fixed shape of other nets (pins, escapes, earlier halos), so
        // it can never wall in a pin.
        //
        // A relaxed net (its strict re-route found no path) is re-routed with
        // same-net near misses allowed and, its surroundings unchanged,
        // rebuilds the same notch every pass; a fixed pin escape next to its
        // own routed metal then becomes a patched wide shape no halo can
        // move. Once the same violation geometry repeats, the net is searched
        // strictly within `notch_zone` of it (the rest of the net stays
        // relaxed, so the zone cannot wall in a distant pin).
        let mut bad = BTreeSet::new();
        for v in &remaining {
            let seen = rule_seen.entry(v.describe()).or_insert(0);
            *seen = seen.saturating_add(1);
            let n = v.net();
            if *seen >= 2
                && relaxed.contains(&n)
                && zone_count < MAX_NOTCH_ZONES
                && let Some(zone) = notch_zone(input, v)?
            {
                let zones = r.notch_zones.entry(n).or_default();
                if !zones
                    .iter()
                    .any(|z| z.layer == zone.layer && z.rect == zone.rect)
                {
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        eprintln!(
                            "[troute] notch zone net {n} layer {} {:?} for {}",
                            zone.layer,
                            zone.rect,
                            v.describe()
                        );
                    }
                    zones.push(zone);
                    zone_count += 1;
                }
            }
            // A same-net notch that survives a re-route and that cleanup could
            // not fill is walled in by other nets' metal. Reserve the first
            // fill (robust box, then the bridges, smallest first) that only
            // routed metal blocks as fixed metal of this net, and re-route
            // the nets in its way, so the next cleanup can place the patch.
            // Candidates and victims are pure functions of the geometry
            // (order-stable); each reservation is placed once.
            // Only with `RouteQor::notch_bridge`.
            if notch_bridge
                && *seen >= 2
                && zone_count < MAX_NOTCH_ZONES
                && let route_rules::Violation::Spacing { layer, a, b, .. } = *v
                && let Some((rects, victims)) =
                    notch_fill_reservation(&mut r.spatial, n, layer, a, b)?
            {
                let key = rects.iter().map(|q| [q.lx, q.ly, q.ux, q.uy]).collect();
                if notch_reserved.insert((n, layer, key)) {
                    for rect in rects {
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[troute] notch fill reserved net {n} layer {layer} {rect:?} victims {victims:?}"
                            );
                        }
                        r.spatial.add_fixed(Blockage {
                            layer,
                            rect,
                            net: Some(n),
                        })?;
                        zone_count += 1;
                    }
                    bad.extend(victims);
                }
            }
            // A notch that survives a strict re-route lies between two parts of
            // one path: the search never sees it. Keep the net out of its
            // smaller shape, so the next path cannot rebuild it.
            if *seen >= 2
                && !relaxed.contains(&n)
                && zone_count < MAX_NOTCH_ZONES
                && let route_rules::Violation::Spacing { layer, a, b, .. } = *v
            {
                let size = |r: Rect| i128::from(r.ux - r.lx) * i128::from(r.uy - r.ly);
                let rect = if size(a) <= size(b) { a } else { b };
                let exclusions = r.own_exclusions.entry(n).or_default();
                if !exclusions
                    .iter()
                    .any(|z| z.layer == layer && z.rect == rect)
                {
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        eprintln!("[troute] exclusion net {n} layer {layer} {rect:?}");
                    }
                    exclusions.push(LayerRect { layer, rect });
                    zone_count += 1;
                }
            }
            if let route_rules::Violation::Wide {
                owner,
                other,
                layer,
                wide,
                near,
                ..
            } = *v
                && owner != other
                && let Some(rule) = input.layers[layer as usize].wide_spacing
            {
                let tries = wide_seen.entry((owner, other, layer)).or_insert(0);
                *tries = tries.saturating_add(1);
                let grow = (rule.spacing - input.layers[layer as usize].spacing).max(0);
                let halo = expand(wide, grow)?;
                if *tries == 2
                    && routed.contains_key(&other)
                    && !matches!(r.spatial.probe_soft(layer, halo, owner)?, Soft::Blocked)
                {
                    r.spatial.add_fixed(Blockage {
                        layer,
                        rect: halo,
                        net: Some(owner),
                    })?;
                    bad.insert(other);
                    continue;
                }
                let zone = expand(wide, rule.spacing)?;
                let facing = Rect {
                    lx: near.lx.max(zone.lx),
                    ly: near.ly.max(zone.ly),
                    ux: near.ux.min(zone.ux),
                    uy: near.uy.min(zone.uy),
                };
                if *tries >= 2 && facing.lx < facing.ux && facing.ly < facing.uy {
                    let halo = expand(facing, grow)?;
                    if !matches!(r.spatial.probe_soft(layer, halo, other)?, Soft::Blocked) {
                        r.spatial.add_fixed(Blockage {
                            layer,
                            rect: halo,
                            net: Some(other),
                        })?;
                        bad.insert(owner);
                        continue;
                    }
                }
                if *tries >= 3
                    && relaxed.contains(&owner)
                    && cycle_tried.insert(owner)
                    && let Some(detail) = routed.get_mut(&owner)
                    && cycle_repair::repair(
                        &mut r,
                        &groups[&owner],
                        &plans,
                        detail,
                        LayerRect { layer, rect: wide },
                    )?
                {
                    retained_edges = retained_edges
                        .checked_sub(1)
                        .ok_or_else(|| invalid("wide-cycle retained record count"))?;
                    // Every span in `remaining` was measured before this cut.
                    // Recheck geometry next pass instead of undoing the repair
                    // through an earlier or later stale entry for this owner.
                    bad.clear();
                    break;
                }
            }
            bad.insert(v.net());
        }
        for net in bad {
            if let Some(old) = remove_route(&mut routed, &mut retained_edges, net) {
                for id in old.shapes {
                    r.spatial.remove(id);
                }
            }
            if !relaxed.contains(&net) {
                strict.insert(net);
            }
            queue.push_back(net);
        }
    }
    unreachable!("bounded repair passes return")
}

/// Last cleanup pass: a wide-metal pair needs up to three repairs (owner,
/// neighbour behind an owner halo, owner behind a neighbour halo), and a
/// repair can expose a new pair.
const MAX_RULE_PASS: usize = 8;

/// Mutual rips of one net pair before the net just ripped yields (converging
/// Ibex routes peak at 3–5; the fe22b cycle reached the 256 rip limit).
const PAIR_YIELD: u16 = 8;

/// Directed rip pairs tracked for yields (about 40 B each).
const MAX_PAIR_RIPS: usize = 1 << 18;

/// Total rule-repair notch zones (each one rectangle of one net).
const MAX_NOTCH_ZONES: usize = 4096;

/// Where a repeated violation of a relaxed net is searched strictly: its
/// same-net notch (or neck) grown by the layer spacing, or the patched wide
/// part grown by the wide spacing when the wide owner itself is re-routed.
/// Other violations (area, holes, floating metal, a routed neighbour of
/// unpatched wide metal) are not same-net notches.
fn notch_zone(input: &RouteInput, v: &route_rules::Violation) -> Result<Option<LayerRect>> {
    use route_rules::Violation;
    let bound = |a: Rect, b: Rect| Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    };
    let zone = match *v {
        Violation::Spacing { layer, a, b, .. } | Violation::Neck { layer, a, b, .. } => {
            let spacing = input.layers[layer as usize].spacing;
            Some((layer, expand(bound(a, b), spacing)?))
        }
        Violation::Wide {
            net,
            owner,
            layer,
            wide,
            ..
        } if net == owner => match input.layers[layer as usize].wide_spacing {
            Some(rule) => Some((layer, expand(wide, rule.spacing)?)),
            None => None,
        },
        _ => None,
    };
    Ok(zone.map(|(layer, rect)| LayerRect { layer, rect }))
}

/// Does a routed edge rectangle reach a notch zone of its net?
fn zoned(zones: &[LayerRect], layer: u32, rect: Rect) -> bool {
    zones
        .iter()
        .any(|z| z.layer == layer && crate::routing::overlap(z.rect, rect))
}

/// Does a via at `p` between `a` and `b` land in a notch zone of its net?
fn zoned_point(zones: &[LayerRect], a: u32, b: u32, p: [i64; 2]) -> bool {
    zones
        .iter()
        .any(|z| (z.layer == a || z.layer == b) && contains(z.rect, p))
}

/// Diagnostic only (`RUSTROAD_RULE_DUMP=<file>`): the cleaned routes of one
/// pass and every violation cleanup left, written to `<file>.pass<N>.json`.
/// Never part of a routing revision; at most 1 GiB is written.
fn dump_rule_state(
    path: &std::path::Path,
    pass: usize,
    out: &Routes,
    remaining: &[crate::route_rules::Violation],
) -> Result<()> {
    use std::io::Write;
    let file = path.with_extension(format!("pass{pass}.json"));
    let violations: Vec<serde_json::Value> = remaining
        .iter()
        .map(|v| serde_json::json!({"net": v.net(), "describe": v.describe()}))
        .collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "schema": "rustroad.rule-dump.v1",
        "pass": pass,
        "violations": violations,
        "routes": out,
    }))
    .map_err(|e| invalid(&format!("rule dump encode: {e}")))?;
    if body.len() > 1 << 30 {
        return Err(budget("rule dump byte limit"));
    }
    std::fs::File::create(&file)
        .and_then(|mut f| f.write_all(&body))
        .map_err(|e| invalid(&format!("rule dump write: {e}")))
}

/// Records of the current routing state (escapes plus routed edges), wires
/// merged into maximal collinear runs split at junctions.
/// Is `r` wholly covered by the union of `by` (rectilinear, coordinate
/// compression; `by` is one pin's few rectangles)?
/// True when `r` on `layer` has positive-area overlap with an `avoid` rect.
fn overlaps_any(
    avoid: &[LayerRect],
    layer: u32,
    r: Rect,
    work: &mut usize,
    max_work: usize,
) -> Result<bool> {
    *work = work
        .checked_add(avoid.len())
        .filter(|&n| n <= max_work)
        .ok_or_else(|| budget("pin access protected-footprint work"))?;
    Ok(avoid.iter().any(|a| {
        a.layer == layer
            && r.lx < a.rect.ux
            && a.rect.lx < r.ux
            && r.ly < a.rect.uy
            && a.rect.ly < r.uy
    }))
}

fn covered(r: Rect, by: &[Rect]) -> bool {
    if by.len() > 64 {
        return false;
    }
    let mut xs = vec![r.lx, r.ux];
    let mut ys = vec![r.ly, r.uy];
    for b in by {
        xs.extend([b.lx.clamp(r.lx, r.ux), b.ux.clamp(r.lx, r.ux)]);
        ys.extend([b.ly.clamp(r.ly, r.uy), b.uy.clamp(r.ly, r.uy)]);
    }
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    xs.windows(2).all(|x| {
        ys.windows(2).all(|y| {
            let (cx, cy) = (x[0] + (x[1] - x[0]) / 2, y[0] + (y[1] - y[0]) / 2);
            by.iter().any(|b| {
                b.lx <= x[0]
                    && x[1] <= b.ux
                    && b.ly <= y[0]
                    && y[1] <= b.uy
                    && contains(*b, [cx, cy])
            })
        })
    })
}

fn assemble(
    r: &Router<'_>,
    groups: &BTreeMap<u32, Vec<&Terminal>>,
    plans: &HashMap<u32, Plan>,
    routed: &BTreeMap<u32, NetRoute>,
    input: &RouteInput,
    limits: &RouteLimits,
    via_layers: &[(u32, u32)],
) -> Result<Routes> {
    let records = plans.values().try_fold(0usize, |n, p| {
        n.checked_add(p.wires.len())
            .and_then(|n| n.checked_add(p.vias.len()))
            .ok_or_else(|| budget("routing record count overflow"))
    })?;
    let records = routed.values().try_fold(records, |n, r| {
        n.checked_add(r.segments())
            .ok_or_else(|| budget("routing record count overflow"))
    })?;
    if records > limits.max_segments {
        return Err(budget("routing intermediate record limit"));
    }
    let mut runs = Vec::new();
    let mut vias: Vec<Via> = Vec::new();
    let mut access: Vec<Access> = Vec::new();
    let mut nets_out = Vec::new();
    let mut wire_refs = Vec::new();
    let mut via_refs = Vec::new();
    let mut access_refs = Vec::new();
    let mut work = 0;
    for (&net, terminals) in groups {
        nets_out.push(net);
        wire_refs.clear();
        via_refs.clear();
        access_refs.clear();
        for t in terminals {
            let p = &plans[&t.pin];
            wire_refs.extend(p.wires.iter());
            via_refs.extend(p.vias.iter());
            access_refs.push(&p.access);
        }
        let route = routed
            .get(&net)
            .ok_or_else(|| no_route(&format!("net {net} left unrouted")))?;
        wire_refs.extend(route.wires.iter());
        via_refs.extend(route.vias.iter());
        via_refs.sort_unstable_by_key(|v| (v.kind, v.at));
        via_refs.dedup();
        let net_runs = crate::wire_runs::net_runs(
            &wire_refs,
            &via_refs,
            via_layers,
            &access_refs,
            &mut work,
            limits.max_comparisons,
            limits
                .max_segments
                .saturating_sub(runs.len())
                .saturating_sub(vias.len())
                .saturating_sub(via_refs.len()),
        )?;
        runs.len()
            .checked_add(vias.len())
            .and_then(|n| n.checked_add(net_runs.len()))
            .and_then(|n| n.checked_add(via_refs.len()))
            .filter(|&n| n <= limits.max_segments)
            .ok_or_else(|| budget("routing output segment limit"))?;
        runs.extend(net_runs);
        vias.extend(via_refs.iter().map(|v| (*v).clone()));
        access.extend(access_refs.iter().map(|a| (*a).clone()));
    }
    let _ = input;
    Ok(Routes {
        wires: runs,
        vias,
        access,
        patches: Vec::new(),
        nets: nets_out,
        stats: RouteStats {
            grid_nodes: r.graph.len,
            expansions: r.expansions,
            comparisons: r.spatial.work,
            wire_length_dbu: 0,
            single_owner_repair: r.single_owner_stats.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_edge_limit_is_transactional_and_released_on_ripup() {
        let mut routes = BTreeMap::new();
        let mut edges = 0;
        let one = || NetRoute {
            wires: vec![Wire {
                net: 1,
                layer: 0,
                start: [0, 0],
                end: [10, 0],
            }],
            vias: Vec::new(),
            shapes: Vec::new(),
        };
        retain_route(&mut routes, &mut edges, 1, one(), 1).unwrap();
        assert!(retain_route(&mut routes, &mut edges, 2, one(), 1).is_err());
        assert_eq!(edges, 1);
        assert_eq!(routes.len(), 1);
        remove_route(&mut routes, &mut edges, 1).unwrap();
        assert_eq!(edges, 0);
        retain_route(&mut routes, &mut edges, 2, one(), 1).unwrap();
        assert_eq!(routes[&2].segments(), 1);
    }

    #[test]
    fn covered_handles_unions_of_pin_rectangles() {
        let l = [rect(0, 0, 10, 4), rect(0, 4, 4, 10)];
        assert!(covered(rect(1, 1, 9, 3), &l));
        assert!(covered(rect(1, 2, 3, 9), &l), "spans both rectangles");
        assert!(!covered(rect(3, 3, 6, 6), &l), "pokes out of the L");
        assert!(!covered(rect(-1, 1, 2, 2), &l));
        assert!(!covered(rect(0, 0, 1, 1), &[]));
    }
    use crate::routing::{LayerRect, RouteLayer, RouteVia, route_cpu_ref};

    fn rect(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    fn layer(name: &str, vertical: Option<bool>) -> RouteLayer {
        RouteLayer {
            name: name.into(),
            width: 20,
            spacing: 20,
            vertical,
            min_area: None,
            min_step: None,
            wide_spacing: None,
        }
    }
    fn via(name: &str, lower: u32, cut: u32, upper: u32) -> RouteVia {
        let pad = rect(-10, -10, 10, 10);
        RouteVia {
            name: name.into(),
            lower,
            upper,
            shapes: vec![
                LayerRect {
                    layer: lower,
                    rect: pad,
                },
                LayerRect {
                    layer: cut,
                    rect: rect(-5, -5, 5, 5),
                },
                LayerRect {
                    layer: upper,
                    rect: pad,
                },
            ],
        }
    }
    fn plan() -> TrackPlan {
        TrackPlan {
            tracks: vec![Some((40, 20)), Some((40, 20)), Some((40, 20)), None, None],
            routing_layers: None,
            gcell: 200,
            global_iterations: 10,
            capacity_percent: 80,
            guide_margin: 1,
            expand_margin: 4,
            max_ripups_per_net: 8,
            max_repair_rounds: 1000,
            via_cost: 80,
            wrong_way_factor: 4,
            heuristic_weight: 1,
            repair_heuristic_weight: None,
            single_owner_repair: None,
            own_pin_index: None,
            instance_index: None,
            memory_mib: 64,
            max_history_entries: crate::routing::DEFAULT_MAX_HISTORY_ENTRIES,
            area_stacks: false,
            qor: None,
        }
    }
    fn terminal(pin: u32, net: u32, driver: bool, r: Rect) -> Terminal {
        Terminal {
            pin,
            net,
            driver,
            shapes: vec![LayerRect { layer: 0, rect: r }],
            class: None,
        }
    }
    /// M1 (H) / M2 (V) / M3 (H) on a 40-DBU track pitch, pins on M1 tracks.
    fn input(terminals: Vec<Terminal>, obstacles: Vec<Blockage>) -> RouteInput {
        RouteInput {
            die: rect(0, 0, 1000, 1000),
            grid: 5,
            step: 40,
            access_layer: 1,
            layers: vec![
                layer("M1", Some(false)),
                layer("M2", Some(true)),
                layer("M3", Some(false)),
                layer("V1", None),
                layer("V2", None),
            ],
            vias: vec![via("V12", 0, 3, 1), via("V23", 1, 4, 2)],
            obstacles,
            terminals,
            rules: true,
            access_classes: Vec::new(),
            tracks: Some(plan()),
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }
    fn limits() -> RouteLimits {
        RouteLimits::default()
    }

    // Hand-constructed cuts: no external PDK or alternate routing algorithm is
    // used as an oracle. At most 1,875 graph nodes and 10,000 expansions/trial.
    fn conflict_fixture(points: &[[i64; 2]]) -> RouteInput {
        let mut design = input(
            points
                .iter()
                .enumerate()
                .map(|(pin, &[x, y])| {
                    terminal(
                        pin as u32,
                        7,
                        pin == 0,
                        rect(x - 10, y - 10, x + 10, y + 10),
                    )
                })
                .collect(),
            Vec::new(),
        );
        design.access_layer = 0;
        let plan = design.tracks.as_mut().unwrap();
        plan.routing_layers = Some([0, 0]);
        plan.wrong_way_factor = 1;
        plan.repair_heuristic_weight = Some(1);
        design
    }

    #[test]
    fn prior_routes_keep_unchanged_nets_and_reroute_moved_ones() {
        // Three two-pin nets. Re-routing with their own result as the prior
        // keeps every net and reproduces the routing; after one pin of net 2
        // moves, only net 2 is routed again (its prior misses the new escape
        // top) and the result is validated like a full route.
        let pins = |moved: bool| {
            input(
                vec![
                    terminal(0, 0, true, rect(90, 90, 110, 110)),
                    terminal(1, 0, false, rect(890, 90, 910, 110)),
                    terminal(2, 1, true, rect(90, 490, 110, 510)),
                    terminal(3, 1, false, rect(890, 490, 910, 510)),
                    terminal(4, 2, true, rect(90, 850, 110, 870)),
                    if moved {
                        terminal(5, 2, false, rect(650, 850, 670, 870))
                    } else {
                        terminal(5, 2, false, rect(890, 850, 910, 870))
                    },
                ],
                Vec::new(),
            )
        };
        let limits = limits();
        let design = pins(false);
        let full = route(&design, &limits).unwrap();
        validate(&design, &full, &limits).unwrap();
        let mut prior = PriorRoutes::default();
        for w in &full.wires {
            prior.nets.entry(w.net).or_default().0.push(w.clone());
        }
        for v in &full.vias {
            prior.nets.entry(v.net).or_default().1.push(v.clone());
        }
        let again = route_with_prior(&design, &limits, Some(&prior)).unwrap();
        validate(&design, &again, &limits).unwrap();
        assert_eq!(again.nets, full.nets);
        let key = |r: &Routes, net: u32| {
            let mut w: Vec<_> = r
                .wires
                .iter()
                .filter(|w| w.net == net)
                .map(|w| (w.layer, w.start, w.end))
                .collect();
            let mut v: Vec<_> = r
                .vias
                .iter()
                .filter(|v| v.net == net)
                .map(|v| (v.kind, v.at))
                .collect();
            w.sort_unstable();
            v.sort_unstable();
            (w, v)
        };
        for net in 0..3 {
            assert_eq!(key(&again, net), key(&full, net), "net {net}");
        }
        let moved = pins(true);
        let eco = route_with_prior(&moved, &limits, Some(&prior)).unwrap();
        validate(&moved, &eco, &limits).unwrap();
        assert_eq!(key(&eco, 0), key(&full, 0));
        assert_eq!(key(&eco, 1), key(&full, 1));
        assert_ne!(key(&eco, 2), key(&full, 2));
        // A prior record of a foreign net is an input error, not a route.
        let mut wrong = prior.clone();
        let stolen = wrong.nets[&1].0[0].clone();
        wrong.nets.get_mut(&0).unwrap().0.push(stolen);
        assert!(route_with_prior(&design, &limits, Some(&wrong)).is_err());
        // Batched: a one-batch prior run replayed through the batch router keeps
        // every net; a plan that splits the design is refused with a prior.
        let mut design = design;
        design.tracks.as_mut().unwrap().memory_mib = 1024;
        let (batched, _) = crate::route_batches::route(&design, &limits, 100).unwrap();
        let mut own = PriorRoutes::default();
        for w in &batched.wires {
            own.nets.entry(w.net).or_default().0.push(w.clone());
        }
        for v in &batched.vias {
            own.nets.entry(v.net).or_default().1.push(v.clone());
        }
        let (replay, _) =
            crate::route_batches::route_with_prior(&design, &limits, 100, Some(&own)).unwrap();
        for net in 0..3 {
            assert_eq!(key(&replay, net), key(&batched, net), "batched net {net}");
        }
        assert!(crate::route_batches::route_with_prior(&design, &limits, 2, Some(&own)).is_err());
    }

    #[test]
    fn own_exclusion_keeps_the_net_out_of_a_notch_shape_on_its_own_path() {
        let design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(90, 890, 110, 910)),
            ],
            Vec::new(),
        );
        let limits = limits();
        let plan = design.tracks.as_ref().unwrap();
        let route = |exclusions: Vec<LayerRect>| {
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            if !exclusions.is_empty() {
                router.own_exclusions.insert(7, exclusions);
            }
            match router
                .route_net(7, &terminals, &plans, false, false, &BTreeMap::new(), 1)
                .unwrap()
            {
                Outcome::Routed(route, _) => route,
                _ => panic!("the fixture is open around the exclusion"),
            }
        };
        let free = route(Vec::new());
        let track = free
            .wires
            .iter()
            .find(|w| w.layer == 1 && w.end[1] - w.start[1] > 400)
            .expect("fixture premise: a long M2 run");
        let middle = (track.start[1] + track.end[1]) / 2;
        let exclusion = LayerRect {
            layer: 1,
            rect: rect(
                track.start[0] - 10,
                middle - 40,
                track.start[0] + 10,
                middle + 40,
            ),
        };
        let moved = route(vec![exclusion.clone()]);
        assert_ne!(moved.wires, free.wires);
        for wire in moved.wires.iter().filter(|w| w.layer == 1) {
            assert!(
                !crate::routing::overlap(wire_rect(wire, 20).unwrap(), exclusion.rect),
                "{wire:?}"
            );
        }
    }

    #[test]
    fn notch_zone_forbids_same_net_near_misses_only_inside_it() {
        // A fixed same-net M2 stub (a pin escape) 3 DBU beside the x = 100 M2
        // track. The cheapest relaxed route between the pins runs straight
        // up that track past the stub, a same-net notch that cleanup must
        // fill. A notch zone around the stub must divert exactly that part
        // of the search; elsewhere the net stays relaxed.
        let design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(90, 890, 110, 910)),
            ],
            Vec::new(),
        );
        let limits = limits();
        let plan = design.tracks.as_ref().unwrap();
        let stub = rect(113, 480, 127, 520);
        let route = |zones: Vec<LayerRect>| {
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            router
                .spatial
                .add_fixed(Blockage {
                    layer: 1,
                    rect: stub,
                    net: Some(7),
                })
                .unwrap();
            if !zones.is_empty() {
                router.notch_zones.insert(7, zones);
            }
            match router
                .route_net(7, &terminals, &plans, false, false, &BTreeMap::new(), 1)
                .unwrap()
            {
                Outcome::Routed(route, _) => route,
                _ => panic!("the fixture is open around the stub"),
            }
        };
        let near = |r: &NetRoute| {
            r.wires.iter().any(|w| {
                w.layer == 1
                    && route_rules::relation(wire_rect(w, 20).unwrap(), stub, 20)
                        == route_rules::Relation::Near
            })
        };
        let relaxed = route(Vec::new());
        assert!(near(&relaxed), "fixture premise: {:?}", relaxed.wires);
        let zone = LayerRect {
            layer: 1,
            rect: expand(stub, 20).unwrap(),
        };
        let zoned = route(vec![zone.clone()]);
        assert!(!near(&zoned), "{:?}", zoned.wires);
        // A zone on another layer, or away from the stub, changes nothing.
        for other in [
            LayerRect {
                layer: 2,
                rect: zone.rect,
            },
            LayerRect {
                layer: 1,
                rect: rect(700, 100, 800, 200),
            },
        ] {
            assert_eq!(route(vec![other]).wires, relaxed.wires);
        }
        // Zones come only from same-net notches and patched wide owners.
        let spacing = route_rules::Violation::Spacing {
            net: 7,
            layer: 1,
            a: rect(90, 480, 110, 520),
            b: stub,
        };
        assert_eq!(
            notch_zone(&design, &spacing)
                .unwrap()
                .map(|z| (z.layer, z.rect)),
            Some((1, rect(70, 460, 147, 540)))
        );
        let floating = route_rules::Violation::Floating {
            net: 7,
            layer: 1,
            rect: stub,
        };
        assert!(notch_zone(&design, &floating).unwrap().is_none());
    }
    #[test]
    fn yielding_net_never_rips_its_protected_partner() {
        // fe22b: nets 14972 and 13203 ripped each other 256 times in rule
        // repair pass 1. The net that yields must treat its partner's metal
        // as blocked: it detours where it can and fails where it cannot.
        let design = conflict_fixture(&[[100, 100], [900, 100]]);
        let limits = RouteLimits {
            max_search_expansions: 10_000,
            ..limits()
        };
        let route = |wall: Rect, protected: Option<u32>| {
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.close_expanded = true;
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            router
                .spatial
                .add_routed(Blockage {
                    layer: 0,
                    rect: wall,
                    net: Some(9),
                })
                .unwrap();
            router.protected = protected;
            router
                .route_net(7, &terminals, &plans, true, false, &BTreeMap::new(), 1)
                .unwrap()
        };
        let full = rect(480, 0, 520, 1000);
        match route(full, None) {
            Outcome::Routed(_, victims) => assert_eq!(victims, [9]),
            _ => panic!("a full-height wall of owner 9 is crossed by ripping it"),
        }
        assert!(
            matches!(route(full, Some(9)), Outcome::Failed(_)),
            "a protected partner wall cannot be crossed"
        );
        assert!(
            matches!(route(full, Some(10)), Outcome::Routed(_, ref v) if v == &[9]),
            "protecting another net changes nothing"
        );
        let Outcome::Routed(detour, victims) = route(rect(480, 0, 520, 800), Some(9)) else {
            panic!("the wall leaves a gap above y 800");
        };
        assert!(victims.is_empty());
        assert!(
            !detour.wires.iter().any(|w| {
                w.start[0].min(w.end[0]) < 520
                    && w.start[0].max(w.end[0]) > 480
                    && w.start[1].min(w.end[1]) < 800
            }),
            "independent geometry check: the detour stays clear of the wall"
        );
    }
    #[test]
    fn later_sink_reuses_an_owner_already_selected_by_the_first_sink() {
        let design = conflict_fixture(&[[100, 100], [100, 900], [900, 900]]);
        let limits = RouteLimits {
            max_search_expansions: 10_000,
            ..limits()
        };
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(&design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        router.close_expanded = true;
        router.gmark.fill(1);
        router.gepoch = 1;
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).unwrap();
        // The first sink must cross owner 9. The second can cross 9 again at
        // y<700 or take the shorter route through a previously untouched 10.
        for (owner, rect) in [
            (9, rect(0, 480, 1000, 520)),
            (9, rect(480, 450, 520, 700)),
            (10, rect(480, 700, 520, 1000)),
        ] {
            router
                .spatial
                .add_routed(Blockage {
                    layer: 0,
                    rect,
                    net: Some(owner),
                })
                .unwrap();
        }
        let Outcome::Routed(route, victims) = router
            .route_net(
                7,
                &terminals,
                &plans,
                true,
                false,
                &BTreeMap::from([(9, 8)]),
                1,
            )
            .unwrap()
        else {
            panic!("the fixture has a route through owner 9");
        };
        assert_eq!(
            victims,
            [9],
            "the committed first sink already requires owner 9's removal"
        );
        assert!(
            !route.wires.iter().any(|w| {
                w.start[0].min(w.end[0]) < 520
                    && w.start[0].max(w.end[0]) > 480
                    && w.start[1].max(w.end[1]) >= 700
            }),
            "independent geometry check: the route must avoid owner 10"
        );
    }

    #[test]
    fn single_owner_trial_prefers_a_short_clear_detour() {
        use crate::routing::{SingleOwnerRepairConfig, SingleOwnerRepairStats};
        let design = conflict_fixture(&[[100, 100], [900, 100]]);
        let limits = RouteLimits {
            max_search_expansions: 10_000,
            ..limits()
        };
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(&design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        router.close_expanded = true;
        router.gmark.fill(1);
        router.gepoch = 1;
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).unwrap();
        router
            .spatial
            .add_routed(Blockage {
                layer: 0,
                rect: rect(480, 0, 520, 180),
                net: Some(9),
            })
            .unwrap();
        let config = SingleOwnerRepairConfig {
            max_candidates: 1,
            max_frontier_nodes: 4096,
            first_search_expansions: 10_000,
            max_progress_candidates: 0,
            progress_search_expansions: 10_000,
            max_total_expansions: 10_000,
            post_soft_refinement: None,
            heuristic_weight: Some(1),
        };
        router.single_owner_stats = Some(SingleOwnerRepairStats::default());
        let rips = BTreeMap::new();
        let request = owner_repair::Request {
            config: &config,
            net: 7,
            terminals: &terminals,
            plans: &plans,
            strict: false,
            rips: &rips,
            round: 1,
            failed_pin: 1,
        };
        let Outcome::Routed(_, victims) = owner_repair::trial(
            &mut router,
            &request,
            &mut owner_repair::Allocation::new(&config),
            9,
            10_000,
        )
        .unwrap() else {
            panic!("the fixture has an unobstructed detour above y180");
        };
        assert!(
            victims.is_empty(),
            "do not displace owner 9 merely to avoid a 240 DBU detour"
        );
        assert!(
            router.history.is_zero(),
            "a clear detour must not raise congestion history"
        );
    }

    #[test]
    fn soft_repair_preserves_a_clear_prefix_before_a_later_blocked_sink() {
        let mut design = conflict_fixture(&[[100, 100], [900, 100], [100, 4900]]);
        design.die = rect(0, 0, 1000, 5000);
        let limits = RouteLimits {
            max_search_expansions: 10_000,
            ..limits()
        };
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(&design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        router.close_expanded = true;
        router.gmark.fill(1);
        router.gepoch = 1;
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).unwrap();
        for (owner, rect) in [
            (9, rect(480, 0, 520, 4000)),
            (10, rect(0, 4480, 1000, 4520)),
        ] {
            router
                .spatial
                .add_routed(Blockage {
                    layer: 0,
                    rect,
                    net: Some(owner),
                })
                .unwrap();
        }
        let hard = router
            .route_net(7, &terminals, &plans, false, false, &BTreeMap::new(), 1)
            .unwrap();
        assert!(
            matches!(hard, Outcome::Failed(2)),
            "the first sink is clear; only the last sink requires owner 10"
        );
        let Outcome::Routed(_, victims) = router
            .route_net(7, &terminals, &plans, true, false, &BTreeMap::new(), 1)
            .unwrap()
        else {
            panic!("crossing owner 10 connects all three terminals");
        };
        assert_eq!(
            victims,
            [10],
            "soft repair must not sacrifice the earlier clear sink to shorten its wire"
        );
    }

    #[test]
    fn soft_stages_respect_a_smaller_configured_search_cap() {
        let design = conflict_fixture(&[[100, 100], [900, 900]]);
        let limits = RouteLimits {
            max_search_expansions: 1,
            ..limits()
        };
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(&design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        router.close_expanded = true;
        router.gmark.fill(1);
        router.gepoch = 1;
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).unwrap();
        let before = router.expansions;
        let result = router
            .route_net(7, &terminals, &plans, true, false, &BTreeMap::new(), 1)
            .unwrap();
        assert!(
            matches!(result, Outcome::Limited(1)),
            "one expansion cannot connect these diagonal terminals"
        );
        assert!(
            router.expansions - before <= 3,
            "all stages must honor the one-expansion cap"
        );
    }

    #[test]
    fn eager_repair_is_bounded_never_displaces_and_restores_trial_state() {
        let design = conflict_fixture(&[[100, 100], [900, 900]]);
        for (allowance, blocked, expected) in
            [(5000, false, true), (5000, true, false), (1, false, false)]
        {
            let limits = RouteLimits {
                eager_repair_expansions: Some(allowance),
                ..limits()
            };
            limits.validate_track().unwrap();
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.close_expanded = true;
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            if blocked {
                router
                    .spatial
                    .add_routed(Blockage {
                        layer: 0,
                        rect: rect(0, 480, 1000, 520),
                        net: Some(9),
                    })
                    .unwrap();
            }
            let live_shapes = |router: &Router<'_>| {
                let value = serde_json::to_value(router.spatial.capture().unwrap()).unwrap();
                value["shapes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(value["alive"].as_array().unwrap())
                    .filter_map(|(shape, alive)| alive.as_bool().unwrap().then_some(shape.clone()))
                    .collect::<Vec<_>>()
            };
            let shapes = live_shapes(&router);
            let history = router.history.capture().unwrap();
            let before = router.expansions;
            let result = eager_hard_repair(
                &mut router,
                7,
                &terminals,
                &plans,
                false,
                &BTreeMap::new(),
                1,
            )
            .unwrap();
            assert_eq!(result.is_some(), expected);
            assert!(router.expansions - before <= allowance);
            assert!(router.trial_search_limit.is_none() && router.trial_expansion_limit.is_none());
            assert_eq!(router.history.capture().unwrap(), history);
            if let Some(route) = result {
                let groups = BTreeMap::from([(7, terminals)]);
                let routed = BTreeMap::from([(7, route)]);
                let via_layers = design
                    .vias
                    .iter()
                    .map(|v| (v.lower, v.upper))
                    .collect::<Vec<_>>();
                let mut output = assemble(
                    &router,
                    &groups,
                    &plans,
                    &routed,
                    &design,
                    &limits,
                    &via_layers,
                )
                .unwrap();
                output.stats.wire_length_dbu = output
                    .wires
                    .iter()
                    .map(|wire| {
                        wire.start[0].abs_diff(wire.end[0]) + wire.start[1].abs_diff(wire.end[1])
                    })
                    .sum();
                validate(&design, &output, &limits).unwrap();
            } else {
                assert_eq!(
                    live_shapes(&router),
                    shapes,
                    "failed eager attempt must leave no tentative metal"
                );
            }
        }
        assert!(
            serde_json::to_value(limits())
                .unwrap()
                .get("eager_repair_expansions")
                .is_none()
        );
        for allowance in [0, limits().max_search_expansions + 1] {
            assert!(
                RouteLimits {
                    eager_repair_expansions: Some(allowance),
                    ..limits()
                }
                .validate_track()
                .is_err()
            );
        }
        assert!(
            serde_json::to_value(limits())
                .unwrap()
                .get("eager_repair_max_victims")
                .is_none()
        );
        for max_victims in [0, 17] {
            assert!(
                RouteLimits {
                    eager_repair_expansions: Some(5000),
                    eager_repair_max_victims: Some(max_victims),
                    ..limits()
                }
                .validate_track()
                .is_err()
            );
        }
        assert!(
            RouteLimits {
                eager_repair_max_victims: Some(4),
                ..limits()
            }
            .validate_track()
            .is_err()
        );
        RouteLimits {
            eager_repair_expansions: Some(5000),
            eager_repair_max_victims: Some(4),
            ..limits()
        }
        .validate_track()
        .unwrap();
        assert!(
            serde_json::to_value(limits())
                .unwrap()
                .get("eager_repair_search_expansions")
                .is_none()
        );
        for (allowance, search) in [(Some(5000), 0), (Some(5000), 5001), (None, 10)] {
            assert!(
                RouteLimits {
                    eager_repair_expansions: allowance,
                    eager_repair_search_expansions: Some(search),
                    ..limits()
                }
                .validate_track()
                .is_err()
            );
        }
        RouteLimits {
            eager_repair_expansions: Some(5000),
            eager_repair_search_expansions: Some(5000),
            ..limits()
        }
        .validate_track()
        .unwrap();
    }

    #[test]
    fn eager_search_cap_lets_a_displaced_net_pass_the_hard_handoff() {
        // A detour longer than the ordinary hard cap but inside the eager
        // allowance: the default cap fails, the explicit cap reconnects.
        let design = conflict_fixture(&[[100, 100], [900, 900]]);
        let base = RouteLimits {
            max_hard_search_expansions: Some(1),
            eager_repair_expansions: Some(5000),
            ..limits()
        };
        let attempt = |limits: &RouteLimits| {
            limits.validate_track().unwrap();
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, limits, graph, cells).unwrap();
            router.close_expanded = true;
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            let before = router.expansions;
            let result = eager_hard_repair(
                &mut router,
                7,
                &terminals,
                &plans,
                false,
                &BTreeMap::new(),
                1,
            )
            .unwrap();
            assert!(router.expansions - before <= 5000);
            assert!(router.trial_search_limit.is_none() && router.trial_expansion_limit.is_none());
            result.is_some()
        };
        assert!(!attempt(&base), "ordinary hard cap must still defer");
        assert!(attempt(&RouteLimits {
            eager_repair_search_expansions: Some(5000),
            ..base.clone()
        }));
    }

    #[test]
    fn fast_candidate_accepts_only_non_growing_routes_and_rolls_back_rejections() {
        use crate::routing::SingleOwnerRepairConfig;
        let design = conflict_fixture(&[[100, 100], [900, 900]]);
        let limits = limits();
        let config = SingleOwnerRepairConfig {
            max_candidates: 1,
            max_frontier_nodes: 4096,
            first_search_expansions: 10_000,
            max_progress_candidates: 0,
            progress_search_expansions: 10_000,
            max_total_expansions: 10_000,
            post_soft_refinement: None,
            heuristic_weight: Some(3),
        };
        for barriers in 0..=2 {
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.close_expanded = true;
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            // Each full-width barrier is an independent, unavoidable owner.
            for n in 0..barriers {
                let y = 390 + 200 * n;
                router
                    .spatial
                    .add_routed(Blockage {
                        layer: 0,
                        rect: rect(0, y, 1000, y + 20),
                        net: Some(9 + n as u32),
                    })
                    .unwrap();
            }
            let live_shapes = |router: &Router<'_>| {
                let value = serde_json::to_value(router.spatial.capture().unwrap()).unwrap();
                value["shapes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(value["alive"].as_array().unwrap())
                    .filter_map(|(shape, alive)| alive.as_bool().unwrap().then_some(shape.clone()))
                    .collect::<Vec<_>>()
            };
            router.history.increment(plans[&0].top).unwrap();
            let original_history = router.history.capture().unwrap();
            let original_shapes = live_shapes(&router);
            let rips = BTreeMap::new();
            let before = router.expansions;
            let result = owner_repair::fast_non_growing(
                &mut router,
                &owner_repair::Request {
                    config: &config,
                    net: 7,
                    terminals: &terminals,
                    plans: &plans,
                    strict: false,
                    rips: &rips,
                    round: 1,
                    failed_pin: 1,
                },
            )
            .unwrap();
            if barriers < 2 {
                let Some(Outcome::Routed(_, victims)) = result else {
                    panic!("zero or one unavoidable owner is acceptable");
                };
                assert_eq!(victims.len(), barriers as usize);
            } else {
                assert!(
                    result.is_none(),
                    "a fast route crossing two independent cuts must be rejected"
                );
                assert_eq!(router.history.capture().unwrap(), original_history);
                assert_eq!(live_shapes(&router), original_shapes);
            }
            assert!(router.expansions > before, "even rejected work is charged");
            assert!(router.expansions - before <= config.max_total_expansions);
            assert!(
                router.recorded_history.is_none()
                    && router.trial_search_limit.is_none()
                    && router.trial_expansion_limit.is_none()
                    && router.heuristic_weight_override.is_none()
            );
        }
    }

    #[test]
    fn fast_candidate_restores_limits_on_local_and_global_budget_failure() {
        use crate::routing::SingleOwnerRepairConfig;
        for global in [false, true] {
            let design = conflict_fixture(&[[100, 100], [900, 900]]);
            let limits = limits();
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            router.close_expanded = true;
            router.heuristic_weight_override = Some(2);
            router.gmark.fill(1);
            router.gepoch = 1;
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            let config = SingleOwnerRepairConfig {
                max_candidates: 1,
                max_frontier_nodes: 4096,
                first_search_expansions: 10_000,
                max_progress_candidates: 0,
                progress_search_expansions: 10_000,
                max_total_expansions: 1,
                post_soft_refinement: None,
                heuristic_weight: Some(3),
            };
            let rips = BTreeMap::new();
            if global {
                router.spatial.work = limits.max_comparisons;
            }
            let before = router.expansions;
            let result = owner_repair::fast_non_growing(
                &mut router,
                &owner_repair::Request {
                    config: &config,
                    net: 7,
                    terminals: &terminals,
                    plans: &plans,
                    strict: false,
                    rips: &rips,
                    round: 1,
                    failed_pin: 1,
                },
            );
            if global {
                assert!(matches!(result, Err(crate::Error::Budget(_))));
            } else {
                assert!(matches!(result, Ok(None)));
            }
            assert!(router.expansions - before <= 1);
            assert_eq!(router.heuristic_weight_override, Some(2));
            assert!(
                router.recorded_history.is_none()
                    && router.trial_search_limit.is_none()
                    && router.trial_expansion_limit.is_none()
            );
        }
    }

    #[test]
    fn global_guides_prefer_existing_positive_capacity_detours() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(4990, 90, 5010, 110)),
                terminal(1, 0, false, rect(4990, 890, 5010, 910)),
            ],
            vec![
                Blockage {
                    layer: 1,
                    rect: rect(1000, 490, 9000, 510),
                    net: None,
                },
                Blockage {
                    layer: 2,
                    rect: rect(1000, 490, 9000, 510),
                    net: None,
                },
            ],
        );
        design.die.ux = 10_000;
        design.access_layer = 2;
        let plan = design.tracks.as_mut().unwrap();
        plan.routing_layers = Some([1, 2]);
        plan.global_iterations = 0;
        plan.guide_margin = 0;
        plan.expand_margin = 0;
        plan.memory_mib = 128;
        let routes = route_cpu_ref(&design, &limits()).expect(
            "a positive-capacity path around a wide fixed bar must outrank a zero-capacity shortcut",
        );
        validate(&design, &routes, &limits()).unwrap();
        assert!(routes.wires.iter().any(|wire| {
            wire.start[0].min(wire.end[0]) < 980 || wire.start[0].max(wire.end[0]) > 9020
        }));
    }

    #[test]
    fn global_guides_see_fixed_crossbars_between_cell_boundaries() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(490, 90, 510, 110)),
                terminal(1, 0, false, rect(490, 890, 510, 910)),
            ],
            vec![
                Blockage {
                    layer: 1,
                    rect: rect(200, 490, 800, 510),
                    net: None,
                },
                Blockage {
                    layer: 2,
                    rect: rect(200, 490, 800, 510),
                    net: None,
                },
            ],
        );
        design.access_layer = 2;
        let plan = design.tracks.as_mut().unwrap();
        plan.routing_layers = Some([1, 2]);
        plan.guide_margin = 0;
        plan.expand_margin = 0;
        let routes = route_cpu_ref(&design, &limits()).expect(
            "a bar between y400/y600 boundaries requires the global guide to detour around x200..800",
        );
        validate(&design, &routes, &limits()).unwrap();
        assert!(
            routes
                .wires
                .iter()
                .any(|w| { w.start[0].min(w.end[0]) < 180 || w.start[0].max(w.end[0]) > 820 })
        );
    }

    #[test]
    fn signal_layer_limits_keep_lower_pin_access_and_restrict_detail() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 90, 910, 110)),
            ],
            Vec::new(),
        );
        let unrestricted = route_cpu_ref(&design, &limits()).unwrap();
        assert!(unrestricted.wires.iter().any(|wire| wire.layer == 0));
        design.tracks.as_mut().unwrap().routing_layers = Some([1, 1]);
        let routes = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &routes, &limits()).unwrap();
        assert!(routes.wires.iter().all(|wire| wire.layer == 1));
        assert_eq!(routes.vias.len(), 2, "only the two M1-to-M2 pin escapes");
        assert!(routes.vias.iter().all(|via| via.kind == 0));
        assert!(routes.access.iter().all(|access| access.layer == 0));
        design.tracks.as_mut().unwrap().routing_layers = Some([2, 1]);
        assert!(route_cpu_ref(&design, &limits()).is_err());
        design.tracks.as_mut().unwrap().routing_layers = Some([1, 3]);
        assert!(
            route_cpu_ref(&design, &limits()).is_err(),
            "cut as max layer"
        );
        design.tracks.as_mut().unwrap().routing_layers = Some([2, 2]);
        assert!(
            route_cpu_ref(&design, &limits()).is_err(),
            "access below min layer"
        );
    }

    #[test]
    fn unrelated_repairs_do_not_make_a_crossing_exceed_the_search_budget() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(9890, 9890, 9910, 9910)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 10000, 10000);
        design.tracks.as_mut().unwrap().memory_mib = 256;
        design.tracks.as_mut().unwrap().heuristic_weight = 4;
        design.tracks.as_mut().unwrap().max_ripups_per_net = 24;
        let limits = RouteLimits {
            max_search_expansions: 2000,
            ..limits()
        };
        for (round, current_rips, victim_rips, history_limit) in [
            (0, 0, 0, None),
            (750, 0, 0, None),
            (750, 23, 23, None),
            (0, 0, 0, Some(16)),
        ] {
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let nodes = graph.len;
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let cell_count = cells.len();
            let mut r = Router {
                input: &design,
                plan,
                heuristic_weight_override: None,
                close_expanded: false,
                last_search_touched_guide: false,
                query_indices_override: None,
                allowed_victim: None,
                protected: None,
                trial_search_limit: None,
                trial_expansion_limit: None,
                deferred_history: None,
                recorded_history: None,
                single_owner_stats: None,
                limits: &limits,
                access_hops: graph.hop_distances().unwrap(),
                target_costs: crate::routing_target::CostModel::new(
                    &[false, true, false],
                    &[(0, 1), (1, 2)],
                    plan.via_cost,
                    plan.wrong_way_factor,
                )
                .unwrap(),
                layer_milli: vec![1000; design.layers.len()],
                pair_via: vec![plan.via_cost; graph.pairs.len()],
                bend_cost: 0,
                portals_released: HashSet::with_hasher(BuildFx),
                access_hint: Vec::new(),
                keep_hints_after_repair: false,
                no_access_retry: false,
                access_retries: 0,
                graph,
                cells,
                spatial: Spatial::new(&design, &limits).unwrap(),
                pairs: via_pairs(&design),
                slots: HashMap::from([(0, 0), (1, 1), (2, 2)]),
                expansions: 0,
                search: Search::new(nodes, nodes).unwrap(),
                epoch: 0,
                history: match history_limit {
                    // Embed the small fixture in the sparse history backend
                    // to check a real storage limit without a huge layout.
                    Some(cap) => History::new(3_000_000_000, cap).unwrap(),
                    None => History::new(nodes, nodes).unwrap(),
                },
                gmark: vec![1; cell_count],
                gepoch: 1,
                stack_ok: vec![true; design.layers.len()],
                via_pad_ok: vec![vec![true; design.vias.len()]; design.layers.len()],
                min_run: vec![0; design.layers.len()],
                area_level: 0,
                escape_area: false,
                access_entries: 0,
                access_cache_bytes: 0,
                notch_zones: BTreeMap::new(),
                own_exclusions: BTreeMap::new(),
                prior_rejects: std::env::var_os("RUSTROAD_ROUTE_TRACE").map(|_| BTreeMap::new()),
            };
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = r.reserve(&terminals).unwrap();
            // Three previously routed wires form a cut across all routing layers.
            // The original input is clear once their owning net has been ripped up.
            for layer in 0..3 {
                r.spatial
                    .add_routed(Blockage {
                        layer,
                        rect: rect(0, 4980, 10000, 5020),
                        net: Some(9),
                    })
                    .unwrap();
            }
            let outcome = r
                .route_net(
                    0,
                    &terminals,
                    &plans,
                    true,
                    false,
                    &BTreeMap::from([(0, current_rips), (9, victim_rips)]),
                    round,
                )
                .unwrap();
            let Outcome::Routed(route, victims) = outcome else {
                panic!(
                    "crossing must finish within 2000 expansions after repair {round}, \
                     current rip-ups {current_rips}, victim rip-ups {victim_rips}"
                );
            };
            assert_eq!(victims, [9], "the crossing must declare the ripped net");
            if history_limit.is_some() {
                assert!(nodes <= 200_000, "bounded history inspection fixture");
                let mut marked = 0;
                for id in 0..nodes as u32 {
                    if r.history.get(id) != 0 {
                        let p = r.graph.point(id).1;
                        assert!(
                            (4900..=5100).contains(&p[1]),
                            "clear wire location {p:?} was marked congested"
                        );
                        marked += 1;
                    }
                }
                assert!((1..=16).contains(&marked), "localized history: {marked}");
            }
            let groups = BTreeMap::from([(0, terminals)]);
            let routes = BTreeMap::from([(0, route)]);
            let mut out = assemble(
                &r,
                &groups,
                &plans,
                &routes,
                &design,
                &limits,
                &[(0, 1), (1, 2)],
            )
            .unwrap();
            out.stats.wire_length_dbu = out
                .wires
                .iter()
                .map(|w| w.start[0].abs_diff(w.end[0]) + w.start[1].abs_diff(w.end[1]))
                .sum();
            validate(&design, &out, &limits).unwrap();
        }
    }

    #[test]
    fn long_moves_reach_segments_inside_a_branched_tree_bounding_box() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(249970, 249970, 249990, 249990)),
                terminal(2, 0, false, rect(124970, 124970, 124990, 124990)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 250000, 250000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 100000;
        plan.capacity_percent = 100;
        plan.memory_mib = 256;
        let bounded = RouteLimits {
            max_axis: 32768,
            max_grid_nodes: 200_000_000,
            max_expansions: 200,
            ..limits()
        };
        let result = route_cpu_ref(&design, &bounded).unwrap();
        validate(&design, &result, &bounded).unwrap();
        assert_eq!(result.access.len(), 3);
        assert_eq!(result.nets, [0]);
    }

    #[test]
    fn open_global_plateau_reaches_the_target_within_the_linear_work_budget() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(9970, 9970, 9990, 9990)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 10000, 10000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 40;
        plan.capacity_percent = 100;
        plan.memory_mib = 256;
        let bounded = RouteLimits {
            max_expansions: 10_000,
            ..limits()
        };
        let result = route_cpu_ref(&design, &bounded).unwrap();
        validate(&design, &result, &bounded).unwrap();
        assert!(result.stats.expansions <= 10_000);
        assert_eq!(result.nets, [0]);
        assert_eq!(result.access.len(), 2);
    }

    #[test]
    fn long_track_moves_keep_checked_unit_geometry_under_a_tight_search_budget() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(9970, 9970, 9990, 9990)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 10000, 10000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 40;
        plan.capacity_percent = 100;
        plan.memory_mib = 256;
        let bounded = RouteLimits {
            max_expansions: 800,
            ..limits()
        };
        let result = route_cpu_ref(&design, &bounded).unwrap();
        validate(&design, &result, &bounded).unwrap();
        assert!(result.stats.expansions < 800);
        assert_eq!(result.access.len(), 2);
        let graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &bounded).unwrap();
        let node = graph.find(0, [500, 500]).unwrap();
        let mut neighbours = Vec::new();
        graph.neighbors(node, &mut neighbours);
        let long: Vec<_> = neighbours
            .iter()
            .filter(|(next, kind)| *kind == Move::Along && node.abs_diff(*next) > 1)
            .collect();
        assert!(!long.is_empty() && long.len() <= 6);
        for &&(next, _) in &long {
            let a = graph.locate(node);
            let b = graph.locate(next);
            assert_eq!((a.0, a.1), (b.0, b.1));
            assert!(node.abs_diff(next) <= 256);
        }
    }

    #[test]
    fn target_projection_crosses_large_open_tracks_with_bounded_expansions() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(999970, 999970, 999990, 999990)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 1_000_000, 1_000_000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 50_000;
        plan.memory_mib = 256;
        let capped = RouteLimits {
            max_axis: 30_000,
            max_grid_nodes: 2_000_000_000,
            max_search_expansions: 100_000,
            max_heap: 20_000,
            max_active_nodes: 100_000,
            max_expansions: 80,
            ..limits()
        };
        let out = route_cpu_ref(&design, &capped).unwrap();
        validate(&design, &out, &capped).unwrap();
        assert!(out.stats.expansions <= 80);
        assert!(out.stats.grid_nodes > 1_000_000_000);
    }

    #[test]
    fn weighted_search_is_deterministic_checked_and_still_budgeted() {
        let mut obstacles = Vec::new();
        for layer in [0, 1, 2] {
            for rect in [rect(4980, 0, 5020, 4930), rect(4980, 5070, 5020, 10000)] {
                obstacles.push(Blockage {
                    layer,
                    rect,
                    net: None,
                });
            }
        }
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(9970, 9970, 9990, 9990)),
            ],
            obstacles,
        );
        design.die = rect(0, 0, 10000, 10000);
        design.tracks.as_mut().unwrap().gcell = 1000;
        design.tracks.as_mut().unwrap().memory_mib = 256;
        design.tracks.as_mut().unwrap().heuristic_weight = 2;
        let weighted = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &weighted, &limits()).unwrap();
        let again = route_cpu_ref(&design, &limits()).unwrap();
        assert_eq!(weighted.wires, again.wires);
        assert_eq!(weighted.vias, again.vias);
        assert_eq!(weighted.access, again.access);
        let capped = RouteLimits {
            max_expansions: weighted.stats.expansions - 1,
            ..limits()
        };
        assert!(matches!(
            route_cpu_ref(&design, &capped),
            Err(crate::Error::Budget(_))
        ));
        for invalid_weight in [0, 17, u64::MAX] {
            design.tracks.as_mut().unwrap().heuristic_weight = invalid_weight;
            assert!(check_plan(&design, design.tracks.as_ref().unwrap()).is_err());
        }
    }

    #[test]
    fn repair_order_starts_in_the_cluster_and_is_permutation_independent() {
        let pins = [
            [0, 0],
            [1000, 1000],
            [1000, 1040],
            [1040, 1000],
            [1040, 1040],
        ];
        let terminals: Vec<_> = pins
            .iter()
            .enumerate()
            .map(|(i, &p)| terminal(i as u32, 0, i == 0, rect(p[0], p[1], p[0] + 1, p[1] + 1)))
            .collect();
        let mut ordered: Vec<_> = terminals.iter().collect();
        let original = ordered.clone();
        assert!(repair_terminal_order(&mut ordered, |t| pins[t.pin as usize], &mut 0, 1).is_err());
        assert_eq!(
            ordered.iter().map(|t| t.pin).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        repair_terminal_order(&mut ordered, |t| pins[t.pin as usize], &mut 0, 100).unwrap();
        assert_eq!(ordered[0].pin, 1);
        assert_eq!(ordered.last().unwrap().pin, 0);
        let mut permuted = original.into_iter().rev().collect::<Vec<_>>();
        repair_terminal_order(&mut permuted, |t| pins[t.pin as usize], &mut 0, 100).unwrap();
        assert_eq!(
            ordered.iter().map(|t| t.pin).collect::<Vec<_>>(),
            permuted.iter().map(|t| t.pin).collect::<Vec<_>>()
        );
    }

    #[test]
    fn remote_internal_driver_does_not_override_the_cluster_root() {
        let points = [[0, 9000], [100, 100], [120, 120], [140, 100], [160, 140]];
        let terminals: Vec<_> = points
            .iter()
            .enumerate()
            .map(|(i, &p)| {
                let mut pin = terminal(i as u32, 0, i == 0, rect(p[0], p[1], p[0] + 1, p[1] + 1));
                pin.class = Some((0, [0, 0]));
                pin
            })
            .collect();
        let mut ordered: Vec<_> = terminals.iter().collect();
        repair_terminal_order(&mut ordered, |t| points[t.pin as usize], &mut 0, 100).unwrap();
        assert_eq!(
            ordered[0].pin, 2,
            "geometric tree starts in the sink cluster"
        );
        assert_eq!(ordered.last().unwrap().pin, 0);
        assert!(
            ordered.last().unwrap().driver,
            "the electrical driver is unchanged"
        );
        let mut reversed: Vec<_> = terminals.iter().rev().collect();
        repair_terminal_order(&mut reversed, |t| points[t.pin as usize], &mut 0, 100).unwrap();
        assert_eq!(
            ordered.iter().map(|p| p.pin).collect::<Vec<_>>(),
            reversed.iter().map(|p| p.pin).collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_terminal_repair_grows_from_the_internal_cell_toward_the_port() {
        let port = terminal(0, 0, true, rect(8990, 8990, 9010, 9010));
        let mut cell = terminal(1, 0, false, rect(90, 90, 110, 110));
        cell.class = Some((0, [0, 0]));
        let mut ordered = [&port, &cell];
        repair_terminal_order(
            &mut ordered,
            |t| if t.pin == 0 { [9000, 9000] } else { [100, 100] },
            &mut 0,
            100,
        )
        .unwrap();
        assert_eq!([ordered[0].pin, ordered[1].pin], [1, 0]);
        assert!(
            ordered[1].driver,
            "the physical root does not change the logical driver"
        );
    }

    #[test]
    fn alternate_root_prefers_an_internal_driver_and_honors_work_caps() {
        let mut sink = terminal(0, 0, false, rect(90, 90, 110, 110));
        sink.class = Some((0, [0, 0]));
        let mut driver = terminal(1, 0, true, rect(190, 190, 210, 210));
        driver.class = Some((0, [0, 0]));
        let port = terminal(2, 0, false, rect(8990, 8990, 9010, 9010));
        let mut ordered = [&sink, &driver, &port];
        let point = |t: &Terminal| [t.shapes[0].rect.lx, t.shapes[0].rect.ly];
        assert!(alternate_repair_root(&mut ordered, point, &mut 0, 1).is_err());
        assert_eq!(ordered[0].pin, 0);
        assert!(alternate_repair_root(&mut ordered, point, &mut 0, 10).unwrap());
        assert_eq!(ordered[0].pin, 1);
    }

    #[test]
    fn soft_search_exhaustion_still_fails_after_bounded_root_retries() {
        for requested_phase in [
            replay::ReplayPhase::PostFailure,
            replay::ReplayPhase::PostBudgetError,
        ] {
            let mut design = input(
                vec![
                    terminal(0, 0, true, rect(10, 10, 30, 30)),
                    terminal(1, 0, false, rect(9970, 9970, 9990, 9990)),
                ],
                Vec::new(),
            );
            design.die = rect(0, 0, 10000, 10000);
            design.access_layer = 0;
            design.tracks.as_mut().unwrap().gcell = 1000;
            design.tracks.as_mut().unwrap().memory_mib = 256;
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustroad-post-failure-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&directory).unwrap();
            let snapshot = directory.join("failure.json");
            let capped = RouteLimits {
                max_search_expansions: 1,
                replay: Some(replay::ReplayCapture {
                    path: snapshot.clone(),
                    net: None,
                    round: None,
                    max_bytes: 1 << 20,
                    phase: requested_phase,
                }),
                ..limits()
            };
            let error = route_cpu_ref(&design, &capped).unwrap_err();
            assert!(
                error.to_string().contains("bounded root retries"),
                "{error}"
            );
            let state: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&snapshot).unwrap()).unwrap();
            assert_eq!(state["phase"], "post_failure");
            assert_eq!(state["net"], 0);
            assert!(state["failed_pin"].as_u64().is_some());
            assert!(state["expansions"].as_u64().unwrap() > 0);
            let report = crate::routing::replay_route(
                &design,
                &capped,
                &snapshot,
                &replay::ReplayOptions::default(),
            )
            .unwrap();
            assert_eq!(report.source_phase, replay::ReplayPhase::PostFailure);
            assert_eq!(
                report.source_failed_pin.unwrap(),
                state["failed_pin"].as_u64().unwrap() as u32
            );
            assert!(
                report
                    .scope
                    .contains("original failed call is not replayed")
            );
            assert_eq!(
                report.expansions_before as u64,
                state["expansions"].as_u64().unwrap()
            );
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn global_budget_error_capture_keeps_primary_error_and_actual_partial_scope() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(10, 10, 30, 30)),
                terminal(1, 0, false, rect(9970, 9970, 9990, 9990)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 10000, 10000);
        design.access_layer = 0;
        design.tracks.as_mut().unwrap().gcell = 1000;
        design.tracks.as_mut().unwrap().memory_mib = 256;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustroad-global-budget-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let before_path = directory.join("before.json");
        let mut cap = RouteLimits {
            max_search_expansions: 1,
            replay: Some(replay::ReplayCapture {
                path: before_path.clone(),
                net: Some(0),
                round: None,
                max_bytes: 1 << 20,
                phase: replay::ReplayPhase::PreSoft,
            }),
            ..limits()
        };
        assert!(
            route_cpu_ref(&design, &cap)
                .unwrap_err()
                .to_string()
                .contains("bounded root retries")
        );
        let before: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&before_path).unwrap()).unwrap();
        cap.max_comparisons = before["spatial"]["work"].as_u64().unwrap() as usize;
        let actual = directory.join("actual.json");
        cap.replay = Some(replay::ReplayCapture {
            path: actual.clone(),
            net: None,
            round: None,
            max_bytes: 1 << 20,
            phase: replay::ReplayPhase::PostBudgetError,
        });
        let error = route_cpu_ref(&design, &cap).unwrap_err().to_string();
        assert!(error.starts_with("BUDGET_EXCEEDED:"), "{error}");
        assert!(
            !error.contains("bounded root retries"),
            "must exercise global Err rather than Outcome::Limited"
        );
        let original = std::fs::read(&actual).unwrap();
        let state: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(state["phase"], "post_budget_error");
        assert_eq!(state["net"], 0);
        assert!(state["failed_pin"].is_null());
        assert_eq!(state["budget_error"]["source_stage"], "repair_soft");
        assert_eq!(
            state["budget_error"]["source_error_code"],
            "BUDGET_EXCEEDED"
        );
        assert_eq!(
            state["budget_error"]["contains_uncommitted_current_net_state"],
            true
        );
        assert_eq!(state["spatial"]["work"], before["spatial"]["work"]);
        assert_eq!(state["expansions"], before["expansions"]);
        let mut disabled = cap.clone();
        disabled.replay = None;
        assert_eq!(
            route_cpu_ref(&design, &disabled).unwrap_err().to_string(),
            error
        );
        // An existing artifact and a failed/oversize output retain the original error.
        assert_eq!(route_cpu_ref(&design, &cap).unwrap_err().to_string(), error);
        assert_eq!(std::fs::read(&actual).unwrap(), original);
        let small = directory.join("oversize.json");
        cap.replay.as_mut().unwrap().path = small.clone();
        cap.replay.as_mut().unwrap().max_bytes = 1024;
        assert_eq!(route_cpu_ref(&design, &cap).unwrap_err().to_string(), error);
        assert!(!small.exists());
        let parent_file = directory.join("not-a-directory");
        std::fs::write(&parent_file, b"preserve").unwrap();
        cap.replay.as_mut().unwrap().path = parent_file.join("state.json");
        cap.replay.as_mut().unwrap().max_bytes = 1 << 20;
        assert_eq!(route_cpu_ref(&design, &cap).unwrap_err().to_string(), error);
        assert_eq!(std::fs::read(&parent_file).unwrap(), b"preserve");
        design.layers[0].spacing += 1;
        assert!(
            crate::routing::replay_route(
                &design,
                &disabled,
                &actual,
                &replay::ReplayOptions::default()
            )
            .unwrap_err()
            .to_string()
            .contains("REPLAY_INPUT_CHANGED")
        );
        design.layers[0].spacing -= 1;
        let mut forged = state;
        forged["budget_error"]["contains_uncommitted_current_net_state"] = false.into();
        let forged_path = directory.join("forged.json");
        std::fs::write(&forged_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert!(
            crate::routing::replay_route(
                &design,
                &disabled,
                &forged_path,
                &replay::ReplayOptions::default()
            )
            .unwrap_err()
            .to_string()
            .contains("budget-error context differs")
        );
        assert_eq!(std::fs::read(&actual).unwrap(), original);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn actual_via_pad_area_allows_a_legal_stack_despite_a_smaller_variant() {
        // M2 offers only isolated via landing squares, so the only connection
        // crosses M3. The large actual pad meets M2 AREA; an unused small
        // variant must not force a wire run out of the isolated landing.
        let mut blocks = vec![Blockage {
            layer: 0,
            rect: rect(140, 0, 860, 1000),
            net: None,
        }];
        for r in [
            rect(0, 0, 1000, 60),
            rect(0, 140, 1000, 1000),
            rect(0, 60, 60, 140),
            rect(140, 60, 860, 140),
            rect(940, 60, 1000, 140),
        ] {
            blocks.push(Blockage {
                layer: 1,
                rect: r,
                net: None,
            });
        }
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 90, 910, 110)),
            ],
            blocks,
        );
        design.layers[1].min_area = Some(400);
        let mut small = via("small", 0, 3, 1);
        small.shapes.iter_mut().find(|s| s.layer == 1).unwrap().rect = rect(-5, -5, 5, 5);
        design.vias.push(small);
        let out = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &out, &limits()).unwrap();
        assert!(out.wires.iter().any(|w| w.layer == 2));
        for via in &mut design.vias {
            for pad in &mut via.shapes {
                if pad.layer == 1 {
                    pad.rect = rect(-5, -5, 5, 5);
                }
            }
        }
        design.layers[1].min_area = Some(10_000);
        assert!(
            route_cpu_ref(&design, &limits()).is_err(),
            "an isolated island with no room for its required area must fail"
        );
    }

    #[test]
    fn area_aware_escape_top_metal_meets_the_area_rule() {
        // Pins on M1 escape to M2 (AREA 1600, a lone pad is 400). Only the
        // area-aware escape must carry enough M2 metal of its own.
        let top_areas = |area_stacks: bool| -> Vec<i128> {
            let mut design = input(
                vec![
                    terminal(0, 0, true, rect(90, 90, 110, 110)),
                    terminal(1, 0, false, rect(890, 890, 910, 910)),
                ],
                Vec::new(),
            );
            design.layers[1].min_area = Some(1600);
            design.tracks.as_mut().unwrap().area_stacks = area_stacks;
            let limits = limits();
            let plan = design.tracks.as_ref().unwrap();
            let graph = Graph::new(&design, plan, &limits).unwrap();
            let cells = Cells::new(design.die, plan.gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = router.reserve(&terminals).unwrap();
            let mut areas: Vec<i128> = plans
                .values()
                .map(|p| {
                    let mut metal: Vec<Rect> = p
                        .wires
                        .iter()
                        .filter(|w| w.layer == 1)
                        .map(|w| wire_rect(w, 20).unwrap())
                        .collect();
                    for v in p.vias.iter() {
                        for pad in &design.vias[v.kind as usize].shapes {
                            if pad.layer == 1 {
                                metal.push(shifted(pad.rect, v.at).unwrap());
                            }
                        }
                    }
                    let mut work = 0;
                    route_rules::union_area(&metal, &mut work, usize::MAX).unwrap()
                })
                .collect();
            areas.sort_unstable();
            areas
        };
        assert!(
            top_areas(true).iter().all(|&a| a >= 1600),
            "{:?}",
            top_areas(true)
        );
        assert!(
            top_areas(false).iter().any(|&a| a < 1600),
            "{:?}",
            top_areas(false)
        );
    }

    #[test]
    fn area_aware_search_runs_on_a_layer_before_stacking_through_it() {
        // M1 is cut between the pins, so the route climbs M1 -> M2 -> M3. A
        // lone M2 via pad (400) is below AREA 1600; the area-aware search
        // must leave every M2 landing along a run instead of stacking.
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 90, 910, 110)),
            ],
            vec![Blockage {
                layer: 0,
                rect: rect(300, 0, 700, 1000),
                net: None,
            }],
        );
        design.access_layer = 0;
        design.layers[1].min_area = Some(1600);
        design.tracks.as_mut().unwrap().area_stacks = true;
        let out = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &out, &limits()).unwrap();
        assert!(out.wires.iter().any(|w| w.layer == 2));
        let lower: BTreeSet<[i64; 2]> = out
            .vias
            .iter()
            .filter(|v| v.kind == 0)
            .map(|v| v.at)
            .collect();
        assert!(
            out.vias
                .iter()
                .all(|v| v.kind != 1 || !lower.contains(&v.at)),
            "no V12/V23 stack through the M2 area rule: {:?}",
            out.vias
        );
    }

    #[test]
    fn access_reserves_an_extendable_landing_on_phase_offset_tracks() {
        let mut blocks = vec![Blockage {
            layer: 0,
            rect: rect(340, 0, 660, 1000),
            net: None,
        }];
        for r in [
            rect(0, 0, 1000, 60),
            rect(0, 140, 1000, 1000),
            rect(0, 60, 70, 140),
            rect(130, 60, 160, 140),
            rect(300, 60, 700, 140),
            rect(840, 60, 870, 140),
            rect(930, 60, 1000, 140),
        ] {
            blocks.push(Blockage {
                layer: 1,
                rect: r,
                net: None,
            });
        }
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 90, 910, 110)),
            ],
            blocks,
        );
        design.tracks.as_mut().unwrap().tracks[0] = Some((40, 0));
        design.vias[1]
            .shapes
            .iter_mut()
            .find(|s| s.layer == 1)
            .unwrap()
            .rect = rect(-20, -20, 20, 20);
        let out = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &out, &limits()).unwrap();
        assert!(out.wires.iter().any(|w| w.layer == 2));
        let again = route_cpu_ref(&design, &limits()).unwrap();
        assert_eq!(out.wires, again.wires);
        assert_eq!(out.vias, again.vias);
    }

    #[test]
    fn minimum_area_via_islands_are_repaired_before_returning_a_route() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                Terminal {
                    pin: 1,
                    net: 0,
                    driver: false,
                    shapes: vec![LayerRect {
                        layer: 2,
                        rect: rect(90, 90, 110, 110),
                    }],
                    class: None,
                },
            ],
            Vec::new(),
        );
        design.access_layer = 0;
        design.layers[1].min_area = Some(1000);
        let out = route_cpu_ref(&design, &limits()).unwrap();
        assert!(out.wires.iter().any(|w| w.layer == 1));
        validate(&design, &out, &limits()).unwrap();
        let mut unpatched = out;
        unpatched.wires.retain(|w| w.layer != 1);
        assert!(validate(&design, &unpatched, &limits()).is_err());
    }

    #[test]
    fn multilayer_access_meets_internal_area_before_detailed_routing() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 90, 910, 110)),
            ],
            Vec::new(),
        );
        design.access_layer = 2;
        design.layers[1].min_area = Some(1600);
        let limits = RouteLimits {
            max_rule_violations: 1,
            ..limits()
        };
        let routes = route_cpu_ref(&design, &limits)
            .expect("multi-layer escapes must not publish two undersized internal islands");
        validate(&design, &routes, &limits).unwrap();
    }

    #[test]
    fn vertical_access_pad_does_not_force_an_unrepairable_pin_notch() {
        let mut target = terminal(1, 0, false, rect(1680, 830, 1700, 850));
        target.shapes[0].layer = 2;
        let mut design = input(
            vec![terminal(0, 0, true, rect(265, 270, 535, 605)), target],
            vec![
                Blockage {
                    layer: 0,
                    rect: rect(200, 785, 250, 900),
                    net: None,
                },
                Blockage {
                    layer: 1,
                    rect: rect(0, 0, 495, 2000),
                    net: None,
                },
                Blockage {
                    layer: 1,
                    rect: rect(0, 0, 2000, 525),
                    net: None,
                },
            ],
        );
        design.die = rect(0, 0, 2000, 2000);
        design.step = 420;
        for (i, layer) in design.layers.iter_mut().enumerate() {
            layer.width = if i == 0 { 160 } else { 200 };
            layer.spacing = if i == 0 { 180 } else { 210 };
        }
        let mut horizontal = via("V12H", 0, 3, 1);
        horizontal.shapes[0].rect = rect(-145, -105, 145, 105);
        horizontal.shapes[1].rect = rect(-95, -95, 95, 95);
        horizontal.shapes[2].rect = rect(-145, -100, 145, 100);
        let mut vertical = horizontal.clone();
        vertical.name = "V12V".into();
        vertical.shapes[0].rect = rect(-105, -145, 105, 145);
        let mut upper = via("V23", 1, 4, 2);
        upper.shapes[0].rect = rect(-145, -100, 145, 100);
        upper.shapes[1].rect = rect(-95, -95, 95, 95);
        upper.shapes[2].rect = rect(-145, -100, 145, 100);
        design.vias = vec![horizontal, vertical, upper];
        let plan = design.tracks.as_mut().unwrap();
        plan.tracks = vec![
            Some((420, 100)),
            Some((420, 10)),
            Some((420, 0)),
            None,
            None,
        ];
        plan.gcell = 840;
        let routes = route_cpu_ref(&design, &limits())
            .expect("a vertical landing variant permits a connected notch-free escape");
        validate(&design, &routes, &limits()).unwrap();
    }

    #[test]
    fn adjacent_pin_escapes_keep_room_on_the_upper_layer() {
        let mut target0 = terminal(2, 0, false, rect(1670, 410, 1690, 430));
        let mut target1 = terminal(3, 1, false, rect(1670, 1250, 1690, 1270));
        target0.shapes[0].layer = 2;
        target1.shapes[0].layer = 2;
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(515, 220, 745, 625)),
                terminal(1, 1, true, rect(945, 220, 1215, 625)),
                target0,
                target1,
            ],
            [
                rect(0, 0, 335, 2000),
                rect(1395, 0, 2000, 2000),
                rect(0, 0, 2000, 40),
                rect(0, 805, 2000, 2000),
            ]
            .into_iter()
            .map(|rect| Blockage {
                layer: 0,
                rect,
                net: None,
            })
            .collect(),
        );
        design.die = rect(0, 0, 2000, 2000);
        design.step = 420;
        for (i, layer) in design.layers.iter_mut().enumerate() {
            layer.width = if i == 0 { 160 } else { 200 };
            layer.spacing = if i == 0 { 180 } else { 210 };
        }
        let mut wide = via("V12wide", 0, 3, 1);
        wide.shapes[0].rect = rect(-105, -145, 105, 145);
        wide.shapes[1].rect = rect(-95, -95, 95, 95);
        wide.shapes[2].rect = rect(-145, -100, 145, 100);
        let mut narrow = wide.clone();
        narrow.name = "V12narrow".into();
        narrow.shapes[2].rect = rect(-100, -145, 100, 145);
        let mut upper = via("V23", 1, 4, 2);
        upper.shapes[0].rect = rect(-100, -145, 100, 145);
        upper.shapes[1].rect = rect(-95, -95, 95, 95);
        upper.shapes[2].rect = rect(-145, -100, 145, 100);
        design.vias = vec![wide, narrow, upper];
        let plan = design.tracks.as_mut().unwrap();
        plan.tracks = vec![Some((420, 0)), Some((420, 210)), Some((420, 0)), None, None];
        plan.gcell = 840;
        let routes = route_cpu_ref(&design, &limits()).expect(
            "both narrow pins need compatible upper pads, independent of reservation order",
        );
        validate(&design, &routes, &limits()).unwrap();
        // The next cut needs the same choice when the middle layer admits
        // only one landing row; either wide upper pad seals its neighbour.
        design.access_layer = 2;
        design.terminals[2].shapes[0].rect = rect(200, 410, 220, 430);
        let mut second_narrow = design.vias[2].clone();
        second_narrow.name = "V23narrow".into();
        second_narrow.shapes[2].rect = rect(-100, -145, 100, 145);
        design.vias.push(second_narrow);
        design.obstacles.extend(
            [
                rect(0, 0, 2000, 65),
                rect(0, 775, 2000, 2000),
                rect(0, 0, 320, 2000),
                rect(1405, 0, 2000, 2000),
            ]
            .into_iter()
            .map(|rect| Blockage {
                layer: 1,
                rect,
                net: None,
            }),
        );
        let routes = route_cpu_ref(&design, &limits())
            .expect("compatible narrow orientation must propagate through the middle layer");
        validate(&design, &routes, &limits()).unwrap();
    }

    #[test]
    fn protected_footprints_reject_only_positive_area_overlap_on_their_layer() {
        let avoid = [LayerRect {
            layer: 1,
            rect: rect(0, 0, 100, 100),
        }];
        let mut work = 0;
        assert!(overlaps_any(&avoid, 1, rect(99, 99, 200, 200), &mut work, 10).unwrap());
        assert!(!overlaps_any(&avoid, 1, rect(100, 0, 200, 100), &mut work, 10).unwrap());
        assert!(!overlaps_any(&avoid, 2, rect(10, 10, 20, 20), &mut work, 10).unwrap());
        assert!(!overlaps_any(&[], 1, rect(10, 10, 20, 20), &mut work, 10).unwrap());
        assert_eq!(work, 3);
        assert!(overlaps_any(&avoid, 1, rect(10, 10, 20, 20), &mut work, 3).is_err());
    }

    #[test]
    fn stacked_pins_on_one_track_escape_away_from_each_other() {
        // Real EZ130 NOR4BBX1 (FS) C/D geometry: both pins only reach one
        // Metal2 track and Metal2 needs a minimum-area run before Via2. The
        // cheaper run of either pin points at the other pin's landing; the
        // retry must keep the first escape off the displaced pin's footprint.
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(1315, 1870, 1615, 2220)),
                terminal(1, 1, true, rect(1365, 1115, 1805, 1480)),
                terminal(2, 0, false, rect(3990, 6300, 4010, 6320)),
                terminal(3, 1, false, rect(3990, 100, 4010, 120)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 4200, 6720);
        design.step = 420;
        design.grid = 5;
        design.access_layer = 2;
        for (i, layer) in design.layers.iter_mut().enumerate() {
            layer.width = if i == 0 { 160 } else { 200 };
            layer.spacing = if i == 0 { 180 } else { 210 };
        }
        design.layers[1].min_area = Some(144_000);
        design.layers[2].min_area = Some(144_000);
        design.terminals[2].shapes[0].layer = 2;
        design.terminals[3].shapes[0].layer = 2;
        let mut v1 = via("Via1", 0, 3, 1);
        v1.shapes[0].rect = rect(-145, -100, 145, 100);
        v1.shapes[1].rect = rect(-95, -95, 95, 95);
        v1.shapes[2].rect = rect(-100, -145, 100, 145);
        let mut v2 = via("Via2", 1, 4, 2);
        v2.shapes[0].rect = rect(-100, -145, 100, 145);
        v2.shapes[1].rect = rect(-95, -95, 95, 95);
        v2.shapes[2].rect = rect(-145, -100, 145, 100);
        design.vias = vec![v1, v2];
        let plan = design.tracks.as_mut().unwrap();
        plan.tracks = vec![Some((420, 0)), Some((420, 210)), Some((420, 0)), None, None];
        plan.gcell = 1680;
        let routes = route_cpu_ref(&design, &limits())
            .expect("pins sharing one track must both reserve an escape");
        validate(&design, &routes, &limits()).unwrap();
        // Net-aware access leans each escape toward its net's far pin; the
        // displaced pin and its repair fall back to the cheapest tops, so the
        // same repair still converges (and portals may be dropped later).
        design.tracks.as_mut().unwrap().qor = Some(crate::routing::RouteQor {
            prim_order: true,
            drop_portals: true,
            net_aware_access: true,
            ..Default::default()
        });
        let aware = route_cpu_ref(&design, &limits())
            .expect("net-aware access must keep the shared-track repair");
        validate(&design, &aware, &limits()).unwrap();
    }

    #[test]
    fn an_earlier_batch_must_not_seal_a_later_pins_only_escape() {
        // A lower-layer pin is enclosed by fixed cell metal. Its only escape
        // is upward at (500,500), where the earlier net prefers a straight wire.
        let mut terms = vec![
            terminal(0, 0, true, rect(490, 90, 510, 110)),
            terminal(1, 0, false, rect(490, 890, 510, 910)),
            terminal(2, 1, true, rect(490, 490, 510, 510)),
            terminal(3, 1, false, rect(90, 690, 110, 710)),
        ];
        terms[0].shapes[0].layer = 1;
        terms[1].shapes[0].layer = 1;
        let mut fixed = vec![Blockage {
            layer: 0,
            rect: terms[2].shapes[0].rect,
            net: Some(1),
        }];
        for rect in [
            rect(420, 420, 470, 580),
            rect(530, 420, 580, 580),
            rect(470, 420, 530, 470),
            rect(470, 530, 530, 580),
        ] {
            fixed.push(Blockage {
                layer: 0,
                rect,
                net: None,
            });
        }
        let mut design = input(terms, fixed);
        design.tracks.as_mut().unwrap().memory_mib = 256;
        design.tracks.as_mut().unwrap().single_owner_repair =
            Some(crate::routing::SingleOwnerRepairConfig {
                max_candidates: 4,
                max_frontier_nodes: 4096,
                first_search_expansions: 50_000,
                max_progress_candidates: 2,
                progress_search_expansions: 200_000,
                max_total_expansions: 1_000_000,
                post_soft_refinement: None,
                heuristic_weight: None,
            });
        let together = route_cpu_ref(&design, &limits()).unwrap();
        validate(&design, &together, &limits()).unwrap();
        let (batched, report) = crate::route_batches::route(&design, &limits(), 2).unwrap();
        validate(&design, &batched, &limits()).unwrap();
        assert_eq!(batched.nets, [0, 1]);
        assert_eq!(batched.access.len(), 4);
        assert!(
            batched.stats.single_owner_repair.is_some(),
            "batch assembly retains measured optional stage telemetry"
        );
        assert_eq!(report.repair_attempts, 1);
        assert_eq!(report.displaced_nets, 1);
        let (again, _) = crate::route_batches::route(&design, &limits(), 2).unwrap();
        assert_eq!(batched.wires, again.wires);
        assert_eq!(batched.vias, again.vias);
        assert_eq!(batched.access, again.access);
        for limited in [
            RouteLimits {
                max_batch_retries: 0,
                ..limits()
            },
            RouteLimits {
                max_batch_repair_terminals: 3,
                ..limits()
            },
            RouteLimits {
                max_expansions: batched.stats.expansions - 1,
                ..limits()
            },
        ] {
            assert!(matches!(
                crate::route_batches::route(&design, &limited, 2),
                Err(crate::Error::Budget(_))
            ));
        }
    }

    #[test]
    fn bounded_batches_preserve_all_nets_and_validate_the_combined_geometry() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(490, 90, 510, 110)),
                terminal(2, 1, true, rect(90, 490, 110, 510)),
                terminal(3, 1, false, rect(490, 490, 510, 510)),
            ],
            Vec::new(),
        );
        design.tracks.as_mut().unwrap().memory_mib = 256;
        let (out, report) = crate::route_batches::route(&design, &limits(), 2).unwrap();
        assert_eq!(report.batches, 2);
        assert_eq!(out.nets, [0, 1]);
        assert_eq!(out.access.len(), 4);
        validate(&design, &out, &limits()).unwrap();
        let (again, _) = crate::route_batches::route(&design, &limits(), 2).unwrap();
        assert_eq!(out.wires, again.wires);
        assert_eq!(out.vias, again.vias);
        assert_eq!(out.access, again.access);
        let repair = RouteLimits {
            max_hard_search_expansions: Some(1),
            ..limits()
        };
        let (repaired, _) = crate::route_batches::route(&design, &repair, 2).unwrap();
        validate(&design, &repaired, &repair).unwrap();
        assert_eq!(repaired.nets, [0, 1]);
        assert_eq!(repaired.access.len(), 4);
        assert!(crate::route_batches::route(&design, &limits(), 1).is_err());
        let limited = RouteLimits {
            max_expansions: out.stats.expansions - 1,
            ..limits()
        };
        assert!(crate::route_batches::route(&design, &limited, 2).is_err());
        let mut disconnected = out.clone();
        disconnected.access.pop();
        assert!(validate(&design, &disconnected, &limits()).is_err());
        design.terminals[2].net = 0;
        assert!(crate::route_batches::route(&design, &limits(), 2).is_err());
    }

    // This fixture intentionally reproduces push-grown spare capacity.
    #[allow(clippy::vec_init_then_push)]
    fn escape_buffers(net: u32) -> (Vec<Wire>, Vec<Via>, Vec<usize>) {
        let mut wires = Vec::new();
        wires.push(Wire {
            net,
            layer: 0,
            start: [0, 0],
            end: [10, 0],
        });
        let mut vias = Vec::new();
        vias.push(Via {
            net,
            kind: 0,
            at: [10, 0],
        });
        let mut shapes = Vec::new();
        for i in 0..6 {
            shapes.push(i);
        }
        (wires, vias, shapes)
    }

    #[test]
    #[ignore = "isolated memory fixture; use an OS memory/time guard"]
    fn retained_escape_untrimmed_memory_fixture() {
        type Untrimmed = (Access, u32, bool, Vec<Wire>, Vec<Via>, Vec<usize>);
        let mut plans: Vec<Untrimmed> = Vec::with_capacity(500_001);
        for net in 0..500_001 {
            let (wires, vias, shapes) = escape_buffers(net);
            plans.push((
                Access {
                    pin: net,
                    net,
                    layer: 0,
                    at: [0, 0],
                },
                net,
                true,
                wires,
                vias,
                shapes,
            ));
        }
        assert_eq!(
            plans
                .iter()
                .map(|p| p.3.len() + p.4.len() + p.5.len())
                .sum::<usize>(),
            500_001 * 8
        );
        std::hint::black_box(plans);
    }

    #[test]
    #[ignore = "isolated memory fixture; use an OS memory/time guard"]
    fn retained_escape_compact_memory_fixture() {
        let mut plans = Vec::with_capacity(500_001);
        for net in 0..500_001 {
            let (wires, vias, shapes) = escape_buffers(net);
            plans.push(Plan {
                access: Access {
                    pin: net,
                    net,
                    layer: 0,
                    at: [0, 0],
                },
                top: net,
                top_via: true,
                portal_shapes: 0,
                wires: wires.into_boxed_slice(),
                vias: vias.into_boxed_slice(),
                shapes: shapes.into_boxed_slice(),
            });
        }
        assert_eq!(
            plans
                .iter()
                .map(|p| p.wires.len() + p.vias.len() + p.shapes.len())
                .sum::<usize>(),
            500_001 * 8
        );
        std::hint::black_box(plans);
    }

    fn rule_scale_fixture() -> (RouteInput, Routes) {
        let mut terminals = Vec::new();
        let mut wires = Vec::new();
        let mut vias = Vec::new();
        for net in 0..100_001u32 {
            let x = i64::from(net % 1000) * 400 + 100;
            let y = i64::from(net / 1000) * 400 + 100;
            terminals.push(terminal(
                2 * net,
                net,
                true,
                rect(x - 10, y - 10, x + 10, y + 10),
            ));
            terminals.push(terminal(
                2 * net + 1,
                net,
                false,
                rect(x + 110, y - 10, x + 130, y + 10),
            ));
            for start in [0, 40, 80] {
                wires.push(Wire {
                    net,
                    layer: 0,
                    start: [x + start, y],
                    end: [x + start + 40, y],
                });
            }
            vias.push(Via {
                net,
                kind: 0,
                at: [x + 40, y],
            });
        }
        let mut design = input(terminals, vec![]);
        design.die = rect(0, 0, 400_000, 401_000);
        let routes = Routes {
            wires,
            vias,
            access: vec![],
            patches: vec![],
            nets: (0..100_001).collect(),
            stats: Default::default(),
        };
        (design, routes)
    }

    #[test]
    #[ignore = "memory comparison fixture; run under an OS memory and wall-clock guard"]
    fn rule_groups_whole_design_memory_fixture() {
        let (design, routes) = rule_scale_fixture();
        let groups = crate::route_rules::collect(&design, &routes).unwrap();
        assert_eq!(groups.len(), 200_002);
        assert_eq!(groups.values().map(Vec::len).sum::<usize>(), 500_005);
    }

    #[test]
    #[ignore = "memory comparison fixture; run under an OS memory and wall-clock guard"]
    fn rule_groups_indexed_memory_fixture() {
        let (design, routes) = rule_scale_fixture();
        let mut work = 0;
        let limits = RouteLimits {
            max_comparisons: 1_000_000_000,
            ..limits()
        };
        let index =
            crate::rule_index::RuleIndex::new(&design, &routes, &mut work, limits.max_comparisons)
                .unwrap();
        let mut shapes = 0;
        let mut groups = 0;
        for &net in index.nets() {
            let collected = index
                .collect(&design, &routes, net, &limits, &mut work)
                .unwrap();
            groups += collected.len();
            shapes += collected.values().map(Vec::len).sum::<usize>();
        }
        assert_eq!(groups, 200_002);
        assert_eq!(shapes, 500_005);
    }

    #[test]
    fn same_net_rule_diagnostic_limit_never_reports_a_clean_layout() {
        let design = input(vec![], vec![]);
        let routes = Routes {
            wires: [100, 130, 160]
                .into_iter()
                .map(|y| Wire {
                    net: 1,
                    layer: 0,
                    start: [100, y],
                    end: [200, y],
                })
                .collect(),
            vias: vec![],
            patches: vec![],
            access: vec![],
            nets: vec![1],
            stats: Default::default(),
        };
        let all = crate::route_rules::check(&design, &routes, &limits(), &mut 0).unwrap();
        assert_eq!(all.len(), 2);
        let capped = RouteLimits {
            max_rule_violations: 1,
            ..limits()
        };
        let error = crate::route_rules::check(&design, &routes, &capped, &mut 0).unwrap_err();
        assert!(error.to_string().contains("same-net diagnostic count"));
    }

    #[test]
    fn indexed_rule_groups_match_whole_collection_and_freeze_the_round() {
        let design = input(
            vec![
                terminal(0, 9, false, rect(90, 90, 110, 110)),
                terminal(1, 2, true, rect(10, 10, 30, 30)),
                terminal(2, 4, false, rect(290, 290, 310, 310)),
            ],
            vec![],
        );
        let mut routes = Routes {
            wires: vec![
                Wire {
                    net: 9,
                    layer: 0,
                    start: [100, 100],
                    end: [140, 100],
                },
                Wire {
                    net: 2,
                    layer: 0,
                    start: [20, 20],
                    end: [60, 20],
                },
                Wire {
                    net: 9,
                    layer: 0,
                    start: [120, 100],
                    end: [160, 100],
                },
            ],
            vias: vec![
                Via {
                    net: 9,
                    kind: 0,
                    at: [100, 100],
                },
                Via {
                    net: 4,
                    kind: 1,
                    at: [300, 300],
                },
            ],
            patches: vec![crate::routing::Patch {
                net: 2,
                layer: 0,
                rect: rect(40, 10, 50, 30),
            }],
            access: vec![],
            nets: vec![9, 2],
            stats: Default::default(),
        };
        let expected = crate::route_rules::collect(&design, &routes).unwrap();
        let index = crate::rule_index::RuleIndex::new(&design, &routes, &mut 0, 10_000).unwrap();
        let mut actual = BTreeMap::new();
        for &net in index.nets() {
            actual.extend(
                index
                    .collect(&design, &routes, net, &limits(), &mut 0)
                    .unwrap(),
            );
        }
        let normalized = |groups: BTreeMap<_, Vec<crate::route_rules::Shape>>| {
            groups
                .into_iter()
                .map(|(key, values)| {
                    (
                        key,
                        values
                            .into_iter()
                            .map(|s| (s.rect, s.origin))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(normalized(actual), normalized(expected));
        let original = index
            .collect(&design, &routes, 9, &limits(), &mut 0)
            .unwrap();
        routes.wires.push(Wire {
            net: 9,
            layer: 0,
            start: [160, 100],
            end: [200, 100],
        });
        assert_eq!(index.wires(&routes, 9).unwrap().count(), 3);
        assert_eq!(
            normalized(
                index
                    .collect(&design, &routes, 9, &limits(), &mut 0)
                    .unwrap()
            ),
            normalized(original)
        );
        let tiny = RouteLimits {
            max_obstacles: 1,
            ..limits()
        };
        assert!(index.collect(&design, &routes, 9, &tiny, &mut 0).is_err());
        assert!(crate::rule_index::RuleIndex::new(&design, &routes, &mut 0, 1).is_err());
        routes.wires.clear();
        assert!(index.wires(&routes, 9).is_err());
    }

    #[test]
    fn large_implicit_grid_routes_with_bounded_sparse_state() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(890, 890, 910, 910)),
            ],
            Vec::new(),
        );
        design.die = rect(0, 0, 400_000, 400_000);
        let plan = design.tracks.as_mut().expect("track plan");
        plan.gcell = 10_000;
        plan.memory_mib = 256;
        let caps = RouteLimits {
            max_grid_nodes: 400_000_000,
            max_expansions: 100_000,
            max_search_expansions: 5_000,
            max_heap: 20_000,
            ..RouteLimits::default()
        };
        let estimate = estimate_for(&design, &caps)
            .expect("estimate")
            .expect("track estimate");
        // Includes the indexed frontier while remaining independent of the
        // 300-million-node implicit lattice size.
        assert!(estimate.total() < 40 * 1024 * 1024, "{estimate:?}");
        let result = route_cpu_ref(&design, &caps).expect("route the local connection");
        assert_eq!(result.stats.grid_nodes, 300_000_000);
        assert_eq!(result.nets, vec![0]);
        assert!(!result.wires.is_empty());
        assert!(result.stats.expansions <= caps.max_expansions);
    }

    #[test]
    fn track_graph_nodes_follow_pitch_and_offset() {
        let i = input(Vec::new(), Vec::new());
        let g = Graph::new(&i, i.tracks.as_ref().unwrap(), &limits()).unwrap();
        // 25 tracks per layer (20..980 step 40), 25 stops each.
        assert_eq!(g.len, 3 * 25 * 25);
        assert_eq!(g.layers[0].tracks.first(), Some(&20));
        assert_eq!(g.layers[1].stops, g.layers[0].tracks);
        let id = g.find(1, [100, 220]).unwrap();
        assert_eq!(g.point(id), (1, [100, 220]));
        let mut n = Vec::new();
        g.neighbors(id, &mut n);
        // Two unit along-track, two wrong-way, via down and via up;
        // additional long moves keep the same underlying graph nodes.
        assert_eq!(
            n.iter()
                .filter(|(next, kind)| *kind != Move::Along || id.abs_diff(*next) == 1)
                .count(),
            6
        );
        assert!(n.len() > 6);
        assert_eq!(graph_nodes(&i, &limits()).unwrap(), g.len);
    }

    #[test]
    fn routes_several_nets_validates_and_is_deterministic() {
        let mut t = Vec::new();
        // Three nets crossing each other between the die corners.
        let pins = [
            ((100, 100), (900, 900)),
            ((100, 900), (900, 100)),
            ((500, 100), (500, 900)),
        ];
        for (n, (a, b)) in pins.iter().enumerate() {
            let n = n as u32;
            t.push(terminal(
                2 * n,
                n,
                true,
                rect(a.0 - 10, a.1 - 10, a.0 + 10, a.1 + 10),
            ));
            t.push(terminal(
                2 * n + 1,
                n,
                false,
                rect(b.0 - 10, b.1 - 10, b.0 + 10, b.1 + 10),
            ));
        }
        let i = input(t, Vec::new());
        let a = route_cpu_ref(&i, &limits()).unwrap();
        let b = route_cpu_ref(&i, &limits()).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert_eq!(a.nets, vec![0, 1, 2]);
        crate::routing::validate(&i, &a, &limits()).unwrap();
        // Output wires are runs: no same-net collinear pair overlaps.
        for (k, w) in a.wires.iter().enumerate() {
            for v in &a.wires[k + 1..] {
                if w.net == v.net && w.layer == v.layer {
                    let collinear_x = w.start[0] == w.end[0]
                        && v.start[0] == v.end[0]
                        && w.start[0] == v.start[0];
                    let collinear_y = w.start[1] == w.end[1]
                        && v.start[1] == v.end[1]
                        && w.start[1] == v.start[1];
                    if collinear_x {
                        let (a0, a1) = (w.start[1].min(w.end[1]), w.start[1].max(w.end[1]));
                        let (b0, b1) = (v.start[1].min(v.end[1]), v.start[1].max(v.end[1]));
                        assert!(a1 <= b0 || b1 <= a0, "{w:?} overlaps {v:?}");
                    }
                    if collinear_y {
                        let (a0, a1) = (w.start[0].min(w.end[0]), w.start[0].max(w.end[0]));
                        let (b0, b1) = (v.start[0].min(v.end[0]), v.start[0].max(v.end[0]));
                        assert!(a1 <= b0 || b1 <= a0, "{w:?} overlaps {v:?}");
                    }
                }
            }
        }
        // Removing a via disconnects a net: validate rejects it.
        let mut bad = a.clone();
        bad.vias.pop();
        assert!(crate::routing::validate(&i, &bad, &limits()).is_err());
    }

    #[test]
    fn prim_order_joins_the_nearest_terminal_first_and_ignores_input_order() {
        // Driver at the origin; sinks on a line and one off to the side.
        let at = |pin: u32, x: i64, y: i64, driver: bool| {
            terminal(pin, 0, driver, rect(x - 5, y - 5, x + 5, y + 5))
        };
        let terminals = [
            at(0, 0, 0, true),
            at(1, 900, 0, false),
            at(2, 300, 0, false),
            at(3, 600, 0, false),
            at(4, 300, 400, false),
            at(5, 0, 300, false),
        ];
        let centre = |t: &Terminal| {
            let r = t.shapes[0].rect;
            [(r.lx + r.ux) / 2, (r.ly + r.uy) / 2]
        };
        let mut group: Vec<&Terminal> = terminals.iter().collect();
        let mut work = 0;
        prim_terminal_order(&mut group, centre, &mut work, 1_000).unwrap();
        // Each step takes the smallest L1 distance to any joined terminal,
        // ties by pin id: 2 (300 from 0, ties 5), 3 (300 from 2, ties 5),
        // 1 (300 from 3, ties 5), 5 (300 from 0), 4 (400 from 2 or 5).
        let order: Vec<u32> = group.iter().map(|t| t.pin).collect();
        assert_eq!(order, vec![0, 2, 3, 1, 5, 4]);
        assert_eq!(work, 36);
        // Any input order of the sinks gives the same sequence.
        let mut shuffled: Vec<&Terminal> = [0usize, 4, 1, 5, 3, 2]
            .iter()
            .map(|&i| &terminals[i])
            .collect();
        prim_terminal_order(&mut shuffled, centre, &mut 0, 1_000).unwrap();
        assert_eq!(shuffled.iter().map(|t| t.pin).collect::<Vec<_>>(), order);
        // Two-terminal groups are untouched; the work cap is enforced.
        let mut pair: Vec<&Terminal> = vec![&terminals[0], &terminals[1]];
        prim_terminal_order(&mut pair, centre, &mut 0, 0).unwrap();
        assert!(prim_terminal_order(&mut group, centre, &mut 0, 35).is_err());
    }

    #[test]
    fn notch_fill_reservation_takes_the_first_fill_only_routed_metal_blocks() {
        // The captured Metal2 stair notch (net 12085) moved near the origin:
        // an upper pad and a lower escape of net 7 face over 80 DBU across a
        // 175 DBU gap (width 200, spacing 210). Candidates, in order: robust
        // box, stair bridge, two bars clipped to two widths past the gap.
        let mut design = input(Vec::new(), Vec::new());
        design.die = rect(0, 0, 3000, 3000);
        for l in &mut design.layers[..3] {
            (l.width, l.spacing) = (200, 210);
        }
        let (a, b) = (rect(550, 1275, 750, 1565), rect(670, 480, 870, 1100));
        let robust = rect(550, 875, 870, 1500);
        let stair = rect(550, 1085, 870, 1290);
        assert_eq!(
            route_rules::local_fills(a, b, 200, 5),
            vec![
                vec![robust],
                vec![stair],
                vec![rect(550, 700, 750, 1565)],
                vec![rect(670, 700, 870, 1565)],
            ]
        );
        let limits = limits();
        // Fixed metal of net 11 above the pair (within spacing of the robust
        // box and both clipped bars, 310 DBU clear of the stair bridge) and a
        // routed net-9 wire 30 DBU right of the pair.
        let fixed = rect(800, 1600, 1000, 1700);
        let wire = rect(900, 1150, 1300, 1250);
        let reserve = |with_fixed: bool, with_wire: bool, swap: bool| {
            let mut spatial = Spatial::new(&design, &limits).unwrap();
            for shape in [a, b] {
                spatial
                    .add_routed(Blockage {
                        layer: 1,
                        rect: shape,
                        net: Some(7),
                    })
                    .unwrap();
            }
            if with_fixed {
                spatial
                    .add_fixed(Blockage {
                        layer: 1,
                        rect: fixed,
                        net: Some(11),
                    })
                    .unwrap();
            }
            if with_wire {
                spatial
                    .add_routed(Blockage {
                        layer: 1,
                        rect: wire,
                        net: Some(9),
                    })
                    .unwrap();
            }
            let (x, y) = if swap { (b, a) } else { (a, b) };
            notch_fill_reservation(&mut spatial, 7, 1, x, y).unwrap()
        };
        let victims = BTreeSet::from([9u32]);
        // Only routed metal in the way: the robust box, net 9 re-routed.
        assert_eq!(
            reserve(false, true, false),
            Some((vec![robust], victims.clone()))
        );
        // Fixed metal blocks the robust box: the stair bridge is reserved.
        assert_eq!(
            reserve(true, true, false),
            Some((vec![stair], victims.clone()))
        );
        // Order-stable: the pair in either order picks the same fill.
        assert_eq!(reserve(true, true, true), Some((vec![stair], victims)));
        // Nothing to re-route: every fill is clear or blocked by fixed metal.
        assert_eq!(reserve(false, false, false), None);
        assert_eq!(reserve(true, false, false), None);
    }

    /// One EZ130-like 8-track standard-cell row: Metal1 pins (net, box)
    /// between Metal1 rails, real Via1/Via2 pads and widths, Metal2 and
    /// Metal3 minimum area, net-aware pin access on `access_layer`. Pin ids
    /// follow the slice; the first pin of each net drives it.
    fn access_row(width: i64, access_layer: u32, pins: &[(u32, Rect)]) -> RouteInput {
        let mut seen = BTreeSet::new();
        let terminals = pins
            .iter()
            .enumerate()
            .map(|(pin, &(net, r))| terminal(pin as u32, net, seen.insert(net), r))
            .collect();
        let rails = [rect(0, 0, width, 120), rect(0, 3240, width, 3360)]
            .map(|rect| Blockage {
                layer: 0,
                rect,
                net: None,
            })
            .to_vec();
        let mut design = input(terminals, rails);
        design.die = rect(0, 0, width, 3360);
        design.step = 420;
        design.access_layer = access_layer;
        for (i, layer) in design.layers.iter_mut().enumerate() {
            layer.width = if i == 0 { 160 } else { 200 };
            layer.spacing = if i == 0 { 180 } else { 210 };
        }
        design.layers[1].min_area = Some(144_000);
        design.layers[2].min_area = Some(144_000);
        let mut v1 = via("Via1", 0, 3, 1);
        v1.shapes[0].rect = rect(-145, -100, 145, 100);
        v1.shapes[1].rect = rect(-95, -95, 95, 95);
        v1.shapes[2].rect = rect(-100, -145, 100, 145);
        let mut v2 = via("Via2", 1, 4, 2);
        v2.shapes[0].rect = rect(-100, -145, 100, 145);
        v2.shapes[1].rect = rect(-95, -95, 95, 95);
        v2.shapes[2].rect = rect(-145, -100, 145, 100);
        design.vias = vec![v1, v2];
        let plan = design.tracks.as_mut().unwrap();
        plan.tracks = vec![Some((420, 0)), Some((420, 210)), Some((420, 0)), None, None];
        plan.gcell = 1680;
        plan.qor = Some(crate::routing::RouteQor {
            net_aware_access: true,
            ..Default::default()
        });
        design
    }

    /// Pin access only: (escapes as (access, top, wires, vias) by pin, or
    /// the error; net-aware passes redone without hints).
    #[allow(clippy::type_complexity)]
    fn access_only(
        design: &RouteInput,
        keep_hints: bool,
        no_retry: bool,
    ) -> (
        Result<BTreeMap<u32, (Access, u32, Vec<Wire>, Vec<Via>)>>,
        usize,
    ) {
        let limits = limits();
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(design, &limits, graph, cells).unwrap();
        router.gmark.fill(1);
        router.gepoch = 1;
        router.keep_hints_after_repair = keep_hints;
        router.no_access_retry = no_retry;
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).map(|plans| {
            plans
                .into_iter()
                .map(|(pin, p)| (pin, (p.access, p.top, p.wires.into(), p.vias.into())))
                .collect()
        });
        (plans, router.access_retries)
    }

    #[test]
    fn net_aware_access_falls_back_to_the_cheapest_tops_after_a_failed_escape() {
        // Smallest row of a seeded search over random EZ130-like rows (3,000
        // rows; 140 behave like this) that reproduces the Ibex flow-placement
        // livelock (pin 30742 net 7237): with the net-nearer tops kept after
        // a repair, a displaced neighbour re-blocks the failed pin on every
        // attempt and the hinted pass ends with "has no legal access"; the
        // failed pin and the escapes it displaced falling back to their
        // cheapest tops converges in the hinted pass itself.
        let design = access_row(
            2940,
            1,
            &[
                (2, rect(1855, 1630, 2145, 1945)),
                (0, rect(230, 1595, 520, 1910)),
                (0, rect(2350, 830, 2640, 1145)),
                (0, rect(885, 980, 1085, 1580)),
                (1, rect(1220, 1780, 1510, 2095)),
                (1, rect(1780, 570, 1980, 1170)),
                (1, rect(1695, 2320, 1895, 2920)),
                (2, rect(1130, 370, 1480, 630)),
                (2, rect(2355, 2295, 2655, 2645)),
            ],
        );
        let (fallback, retries) = access_only(&design, false, false);
        assert_eq!(fallback.unwrap().len(), 9);
        assert_eq!(retries, 0, "the hinted pass converges by itself");
        let (kept, _) = access_only(&design, true, true);
        match kept {
            Err(crate::Error::PinAccess { .. }) => {}
            other => panic!("kept hints must livelock: {:?}", other.map(|p| p.len())),
        }
        // With the whole-pass retry even kept hints end with legal access.
        let (kept, retries) = access_only(&design, true, false);
        assert_eq!(kept.unwrap().len(), 9);
        assert_eq!(retries, 1);
        // Deterministic.
        assert_eq!(
            access_only(&design, false, false).0.unwrap(),
            access_only(&design, false, false).0.unwrap()
        );
    }

    #[test]
    fn failed_net_aware_access_is_redone_without_hints() {
        // A row from the same search where the hinted pass fails even with
        // the per-pin fallback (13 of 3,000 rows) while legacy access
        // succeeds: pin access is redone without hints from the same start
        // state, so the escapes equal the legacy ones exactly. With the retry
        // every searched row whose legacy access succeeded also succeeded.
        let pins = [
            (0, rect(710, 1245, 870, 1945)),
            (0, rect(1050, 730, 1400, 990)),
            (2, rect(1370, 2540, 1810, 2905)),
            (0, rect(2305, 1930, 2465, 2630)),
            (1, rect(80, 1600, 240, 2300)),
            (1, rect(1295, 1750, 1610, 2300)),
            (1, rect(490, 2275, 930, 2640)),
            (1, rect(2130, 640, 2430, 990)),
            (2, rect(1175, 1235, 1525, 1495)),
            (3, rect(55, 560, 345, 875)),
            (2, rect(1795, 1550, 1995, 2150)),
            (2, rect(525, 380, 725, 980)),
        ];
        let design = access_row(2520, 2, &pins);
        let (aware, retries) = access_only(&design, false, false);
        assert_eq!(retries, 1);
        let (failed, _) = access_only(&design, false, true);
        assert!(matches!(failed, Err(crate::Error::PinAccess { .. })));
        let mut legacy = access_row(2520, 2, &pins);
        legacy.tracks.as_mut().unwrap().qor = None;
        let (legacy, retries) = access_only(&legacy, false, false);
        assert_eq!(retries, 0);
        assert_eq!(aware.unwrap(), legacy.unwrap());
    }

    fn crossing_nets() -> RouteInput {
        let mut t = Vec::new();
        // A four-sink net, two crossing two-pin nets and a three-pin net.
        let pins: [&[(i64, i64)]; 4] = [
            &[(100, 500), (900, 520), (300, 900), (700, 100), (500, 500)],
            &[(100, 100), (900, 900)],
            &[(100, 900), (900, 100)],
            &[(500, 100), (500, 900), (200, 300)],
        ];
        let mut pin = 0;
        for (net, points) in pins.iter().enumerate() {
            for (k, &(x, y)) in points.iter().enumerate() {
                t.push(terminal(
                    pin,
                    net as u32,
                    k == 0,
                    rect(x - 10, y - 10, x + 10, y + 10),
                ));
                pin += 1;
            }
        }
        input(t, Vec::new())
    }

    #[test]
    fn default_quality_controls_reproduce_the_legacy_route() {
        let legacy = crossing_nets();
        let mut defaulted = crossing_nets();
        defaulted.tracks.as_mut().unwrap().qor = Some(crate::routing::RouteQor::default());
        let a = route_cpu_ref(&legacy, &limits()).unwrap();
        let b = route_cpu_ref(&defaulted, &limits()).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert_eq!(a.access, b.access);
        assert_eq!(a.patches, b.patches);
    }

    #[test]
    fn quality_controls_route_validly_and_deterministically() {
        let mut design = crossing_nets();
        design.tracks.as_mut().unwrap().qor = Some(crate::routing::RouteQor {
            prim_order: true,
            release_portals: true,
            drop_portals: true,
            net_aware_access: true,
            layer_cost_milli: vec![1000, 1000, 1300, 1000, 1000],
            via_cost_milli: vec![1000, 1000, 1500, 1000, 1000],
            bend_cost: 40,
            small_net_terminals: 3,
            small_net_weight: 1,
            notch_bridge: true,
        });
        let a = route_cpu_ref(&design, &limits()).unwrap();
        let b = route_cpu_ref(&design, &limits()).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert_eq!(a.access, b.access);
        assert_eq!(a.nets, vec![0, 1, 2, 3]);
        crate::routing::validate(&design, &a, &limits()).unwrap();
        // Out-of-range controls are rejected before routing.
        for bad in [
            crate::routing::RouteQor {
                layer_cost_milli: vec![999, 1000, 1000, 1000, 1000],
                ..Default::default()
            },
            crate::routing::RouteQor {
                via_cost_milli: vec![1000; 4],
                ..Default::default()
            },
            crate::routing::RouteQor {
                small_net_terminals: 4,
                ..Default::default()
            },
            crate::routing::RouteQor {
                small_net_terminals: 4,
                small_net_weight: 17,
                ..Default::default()
            },
        ] {
            let mut d = crossing_nets();
            d.tracks.as_mut().unwrap().qor = Some(bad);
            assert!(route_cpu_ref(&d, &limits()).is_err());
        }
    }

    #[test]
    fn detour_ledger_admits_first_bounds_reroutes_and_escalates() {
        let route = |len: i64| NetRoute {
            wires: vec![Wire {
                net: 7,
                layer: 0,
                start: [0, 0],
                end: [len, 0],
            }],
            ..NetRoute::default()
        };
        let bound = crate::routing::DetourBound {
            percent: 150,
            slack: 10,
            max_rejections_per_net: 2,
            max_rejections: 3,
        };
        assert_eq!(bound.allowed(100), 160);
        let mut legacy = DetourLedger::new(None);
        legacy.record(7, &route(100));
        assert!(legacy.admits(7, &route(10_000)) && legacy.first.is_empty());
        let mut ledger = DetourLedger::new(Some(bound));
        // The first route of a net is its reference, however long.
        assert!(ledger.admits(7, &route(100)));
        assert!(ledger.admits(7, &route(160)));
        assert!(!ledger.admits(7, &route(161)));
        assert!(!ledger.admits(7, &route(500)));
        // Per-net escalation: a third refusal is never issued for net 7.
        assert!(ledger.admits(7, &route(500)));
        ledger.record(8, &route(100));
        ledger.record(8, &route(1));
        assert_eq!(ledger.first[&8], 100);
        assert!(!ledger.admits(8, &route(400)));
        // The global cap (3) is reached: everything is admitted afterwards.
        assert!(ledger.admits(8, &route(400)));
        assert_eq!(ledger.total, 3);
    }

    #[test]
    fn repair_detour_bound_keeps_routes_valid_and_deterministic() {
        let mut t = Vec::new();
        let pins = [
            ((100, 100), (900, 900)),
            ((100, 900), (900, 100)),
            ((500, 100), (500, 900)),
            ((100, 500), (900, 500)),
        ];
        for (n, (a, b)) in pins.iter().enumerate() {
            let n = n as u32;
            t.push(terminal(
                2 * n,
                n,
                true,
                rect(a.0 - 10, a.1 - 10, a.0 + 10, a.1 + 10),
            ));
            t.push(terminal(
                2 * n + 1,
                n,
                false,
                rect(b.0 - 10, b.1 - 10, b.0 + 10, b.1 + 10),
            ));
        }
        let i = input(t, Vec::new());
        let mut bounded = limits();
        bounded.repair_detour = Some(crate::routing::DetourBound {
            percent: 100,
            slack: 0,
            max_rejections_per_net: 4,
            max_rejections: 64,
        });
        bounded.validate_track().unwrap();
        let a = route_cpu_ref(&i, &bounded).unwrap();
        let b = route_cpu_ref(&i, &bounded).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        crate::routing::validate(&i, &a, &bounded).unwrap();
        let mut invalid = bounded.clone();
        invalid.repair_detour.as_mut().unwrap().percent = 99;
        assert!(invalid.validate_track().is_err());
    }

    #[test]
    fn blocked_corridor_forces_a_detour_or_fails_explicitly() {
        // A full-height M2 and M3 wall with a single gap on M1 track 500.
        let mut obstacles = Vec::new();
        for l in [0u32, 1, 2] {
            obstacles.push(Blockage {
                layer: l,
                rect: rect(480, 0, 520, 470),
                net: None,
            });
            obstacles.push(Blockage {
                layer: l,
                rect: rect(480, 530, 520, 1000),
                net: None,
            });
        }
        let t = vec![
            terminal(0, 0, true, rect(90, 490, 110, 510)),
            terminal(1, 0, false, rect(890, 490, 910, 510)),
        ];
        let i = input(t, obstacles.clone());
        let r = route_cpu_ref(&i, &limits()).unwrap();
        crate::routing::validate(&i, &r, &limits()).unwrap();
        // Close the gap: no path, a structured NO_ROUTE, never a panic.
        let mut closed = obstacles;
        for l in [0u32, 1, 2] {
            closed.push(Blockage {
                layer: l,
                rect: rect(480, 460, 520, 540),
                net: None,
            });
        }
        let t = vec![
            terminal(0, 0, true, rect(90, 490, 110, 510)),
            terminal(1, 0, false, rect(890, 490, 910, 510)),
        ];
        let err = route_cpu_ref(&input(t, closed), &limits()).unwrap_err();
        assert!(err.to_string().contains("NO_ROUTE"), "{err}");
    }

    #[test]
    fn supply_tie_joins_a_pin_to_its_own_rail_zone_only() {
        use crate::routing::SupplyTie;
        // Ground rail (net 9) and power rail (net 8) on M1 tracks y=20/980.
        let ground = rect(0, 0, 1000, 40);
        let obstacles = vec![
            Blockage {
                layer: 0,
                rect: ground,
                net: Some(9),
            },
            Blockage {
                layer: 0,
                rect: rect(0, 960, 1000, 1000),
                net: Some(8),
            },
        ];
        let zone = rect(200, 0, 400, 40);
        let tie = |zone: Rect| {
            vec![
                terminal(0, 5, false, rect(290, 490, 310, 510)),
                Terminal {
                    pin: 1,
                    net: 5,
                    driver: true,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: zone,
                    }],
                    class: None,
                },
            ]
        };
        let mut i = input(tie(zone), obstacles.clone());
        // Undeclared, the rail is foreign metal and the zone has no start.
        assert!(route_cpu_ref(&i, &limits()).is_err());
        i.ties = vec![SupplyTie {
            net: 5,
            supply: 9,
            zone: LayerRect {
                layer: 0,
                rect: zone,
            },
        }];
        let r = route_cpu_ref(&i, &limits()).unwrap();
        crate::routing::validate(&i, &r, &limits()).unwrap();
        assert_eq!(r.nets, vec![5]);
        // Every tie shape that reaches rail metal does so inside its zone.
        let mut shapes: Vec<(u32, Rect)> = r
            .wires
            .iter()
            .map(|w| (w.layer, crate::routing::wire_rect(w, 20).unwrap()))
            .collect();
        for v in &r.vias {
            for sh in &i.vias[v.kind as usize].shapes {
                shapes.push((sh.layer, shifted(sh.rect, v.at).unwrap()));
            }
        }
        let touching: Vec<_> = shapes
            .iter()
            .filter(|(l, s)| *l == 0 && crate::routing::overlap(*s, ground))
            .collect();
        assert!(!touching.is_empty(), "the tie reaches its rail");
        assert!(
            touching
                .iter()
                .all(|(_, s)| crate::routing::overlap(*s, zone)),
            "{touching:?}"
        );
        // A tie whose pseudo-driver is not its zone, or that names a routed
        // net as its supply, is refused before search.
        let mut bad = input(tie(rect(200, 0, 420, 40)), obstacles.clone());
        bad.ties = i.ties.clone();
        assert!(route_cpu_ref(&bad, &limits()).is_err());
        let mut bad = input(tie(zone), obstacles);
        bad.ties = i.ties.clone();
        bad.ties[0].supply = 5;
        assert!(route_cpu_ref(&bad, &limits()).is_err());
    }

    #[test]
    fn plan_budgets_and_memory_estimate_are_enforced() {
        let t = vec![
            terminal(0, 0, true, rect(90, 90, 110, 110)),
            terminal(1, 0, false, rect(890, 890, 910, 910)),
        ];
        let mut i = input(t, Vec::new());
        let tight = RouteLimits {
            max_grid_nodes: 100,
            ..RouteLimits::default()
        };
        assert!(matches!(
            route_cpu_ref(&i, &tight),
            Err(crate::Error::Budget(_))
        ));
        let few = RouteLimits {
            max_search_expansions: 1,
            ..RouteLimits::default()
        };
        assert!(matches!(
            route_cpu_ref(&i, &few),
            Err(crate::Error::Budget(_))
        ));
        let invalid_hard = RouteLimits {
            max_hard_search_expansions: Some(few.max_search_expansions + 1),
            ..few.clone()
        };
        assert!(invalid_hard.validate_track().is_err());
        // Above the lattice defaults is fine for the track router, above the
        // absolute ceiling is not.
        let raised = RouteLimits {
            max_grid_nodes: 10_000_000,
            ..RouteLimits::default()
        };
        route_cpu_ref(&i, &raised).unwrap();
        let absurd = RouteLimits {
            max_grid_nodes: usize::MAX,
            ..RouteLimits::default()
        };
        assert!(route_cpu_ref(&i, &absurd).is_err());
        // The runtime guard: an estimate below a 4 MiB cap still stops once
        // the process' peak resident set (any test process) is above it.
        if crate::memory::peak_rss_bytes().is_some() {
            let mut small = input(i.terminals.clone(), Vec::new());
            small.tracks.as_mut().unwrap().memory_mib = 4;
            assert!(estimate_for(&small, &limits()).unwrap().unwrap().total() < 4 << 20);
            let err = route_cpu_ref(&small, &limits()).unwrap_err();
            assert!(err.to_string().contains("peak resident set"), "{err}");
        }
        let e = memory_estimate_limited(&i, 1_000_000, 10_000, &RouteLimits::default()).unwrap();
        assert!(e.search > 24_000_000 && e.total() > e.search + e.spatial);
        i.held_bytes = 1 << 40;
        assert!(matches!(
            route_cpu_ref(&i, &limits()),
            Err(crate::Error::Budget(_))
        ));
        i.held_bytes = 0;
        if let Some(p) = i.tracks.as_mut() {
            p.capacity_percent = 0;
        }
        assert!(route_cpu_ref(&i, &limits()).is_err());
        i.tracks.as_mut().unwrap().capacity_percent = 80;
        i.tracks.as_mut().unwrap().max_history_entries = 0;
        assert!(route_cpu_ref(&i, &limits()).is_err(), "zero history budget");
        i.tracks.as_mut().unwrap().max_history_entries =
            crate::routing::DEFAULT_MAX_HISTORY_ENTRIES;
        i.tracks.as_mut().unwrap().tracks[0] = Some((43, 20));
        assert!(route_cpu_ref(&i, &limits()).is_err(), "off-grid pitch");
        i.tracks.as_mut().unwrap().tracks[0] = Some((40, 20));
        i.rules = false;
        assert!(
            route_cpu_ref(&i, &limits()).is_err(),
            "track mode needs rules"
        );
    }

    #[test]
    fn soft_probe_reports_routed_victims_but_fixed_shapes_block() {
        let i = input(Vec::new(), Vec::new());
        let l = limits();
        let mut s = Spatial::new(&i, &l).unwrap();
        let routed = s
            .add_routed(Blockage {
                layer: 1,
                rect: rect(100, 100, 120, 300),
                net: Some(7),
            })
            .unwrap();
        s.add_fixed(Blockage {
            layer: 1,
            rect: rect(400, 100, 420, 300),
            net: Some(8),
        })
        .unwrap();
        let probe = |s: &mut Spatial<'_>, r: Rect| match s.probe_soft(1, r, 1).unwrap() {
            Soft::Blocked => None,
            Soft::Clear { victims, .. } => Some(victims),
        };
        assert_eq!(probe(&mut s, rect(100, 200, 200, 220)), Some(vec![7]));
        assert_eq!(probe(&mut s, rect(380, 200, 400, 220)), None);
        assert_eq!(probe(&mut s, rect(600, 200, 620, 220)), Some(vec![]));
        s.remove(routed);
        assert_eq!(probe(&mut s, rect(100, 200, 200, 220)), Some(vec![]));
        assert!(s.clear(1, rect(100, 200, 200, 220), 1).unwrap());
    }
    #[test]
    fn window_index_matches_exhaustive_overlap_and_bounds_work() {
        let windows: Vec<_> = (0..10_000)
            .map(|i| {
                let x = (i % 100) * 10 - 500;
                let y = (i / 100) * 10 - 500;
                Rect {
                    lx: x,
                    ly: y,
                    ux: x + 4,
                    uy: y + 4,
                }
            })
            .collect();
        let mut work = 0;
        let index = WindowIndex::new(&windows, &mut work, 20_000).unwrap();
        for query in [
            Rect {
                lx: -502,
                ly: -502,
                ux: -499,
                uy: -499,
            },
            Rect {
                lx: -4,
                ly: -4,
                ux: 4,
                uy: 4,
            },
            Rect {
                lx: 1000,
                ly: 0,
                ux: 1004,
                uy: 4,
            },
            Rect {
                lx: -15,
                ly: 12,
                ux: 23,
                uy: 26,
            },
        ] {
            let mut actual = index
                .overlapping(&windows, query, &mut work, 20_000)
                .unwrap();
            actual.sort_unstable();
            let expected: Vec<_> = windows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    (r.lx <= query.ux && query.lx <= r.ux && r.ly <= query.uy && query.ly <= r.uy)
                        .then_some(i)
                })
                .collect();
            assert_eq!(actual, expected);
        }
        assert!(work < 11_000, "queries must avoid whole-design rescans");
        assert!(WindowIndex::new(&windows, &mut 0, 9999).is_err());
        assert!(index.overlapping(&windows, windows[0], &mut 0, 99).is_err());
    }
    #[test]
    fn access_keys_share_exact_geometry_across_unrelated_track_phases() {
        let points = [(0, [10, 20]), (0, [30, 40]), (2, [50, 60])];
        let key = access_key(7, [0, 0], &points).unwrap();
        // Arbitrary translation, deliberately unrelated to any track LCM.
        let translated: Vec<_> = points
            .iter()
            .map(|&(l, p)| (l, [p[0] - 37, p[1] + 93]))
            .collect();
        assert_eq!(key, access_key(7, [-37, 93], &translated).unwrap());
        assert_ne!(key, access_key(8, [-37, 93], &translated).unwrap());
        assert_ne!(key, access_key(7, [-37, 93], &translated[..2]).unwrap());
        assert!(access_key(7, [i64::MIN, 0], &[(0, [i64::MAX, 0])]).is_err());
    }
    #[test]
    fn branched_tree_routes_without_exploring_its_empty_bounding_box() {
        let points = [[100, 100], [39_900, 100], [39_900, 39_900], [100, 39_900]];
        let mut design = input(
            points
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    terminal(
                        i as u32,
                        0,
                        i == 0,
                        rect(p[0] - 10, p[1] - 10, p[0] + 10, p[1] + 10),
                    )
                })
                .collect(),
            vec![],
        );
        design.die = rect(0, 0, 40_000, 40_000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 40_000; // one guide cell; no narrow-guide shortcut
        plan.memory_mib = 256;
        let limits = RouteLimits {
            max_grid_nodes: 4_000_000,
            max_expansions: 100_000,
            max_search_expansions: 20_000,
            max_heap: 50_000,
            max_comparisons: 20_000_000,
            ..Default::default()
        };
        let routed = route_cpu_ref(&design, &limits).unwrap();
        assert_eq!(routed.nets, vec![0]);
        assert_eq!(routed.access.len(), 4);
        assert!(routed.stats.expansions < 100_000);
        let bounded = RouteLimits {
            max_active_nodes: 100,
            ..limits
        };
        assert!(
            route_cpu_ref(&design, &bounded)
                .unwrap_err()
                .to_string()
                .contains("active detailed routing topology")
        );
    }
    #[test]
    fn diagonal_route_bounds_equal_cost_frontiers() {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(39_890, 39_890, 39_910, 39_910)),
            ],
            vec![],
        );
        design.die = rect(0, 0, 40_000, 40_000);
        let plan = design.tracks.as_mut().unwrap();
        plan.gcell = 40_000;
        plan.memory_mib = 256;
        let limits = RouteLimits {
            max_grid_nodes: 4_000_000,
            max_expansions: 40_000,
            max_search_expansions: 20_000,
            max_heap: 50_000,
            max_comparisons: 10_000_000,
            ..Default::default()
        };
        let routed = route_cpu_ref(&design, &limits).unwrap();
        assert_eq!(routed.nets, vec![0]);
        assert_eq!(routed.access.len(), 2);
        assert!(routed.stats.expansions < 40_000);
    }

    #[test]
    fn access_potential_uses_exact_axis_distances_and_shortest_layer_hops() {
        assert_eq!(nearest_axis(&[-10, 10], 0), 10);
        assert_eq!(nearest_axis(&[-10, 10], -12), 2);
        assert_eq!(nearest_axis(&[-10, 10], 10), 0);
        let design = input(vec![], vec![]);
        let mut graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &limits()).unwrap();
        assert_eq!(graph.hop_distances().unwrap()[0][2], 2);
        let shortcut = GPair {
            lower: 0,
            upper: 2,
            kinds: vec![],
            lo_stop_up_track: vec![],
            lo_track_up_stop: vec![],
            up_stop_lo_track: vec![],
            up_track_lo_stop: vec![],
        };
        // Only topology matters to this helper; it must not assume adjacent layer ids.
        graph.pairs.push(shortcut);
        assert_eq!(graph.hop_distances().unwrap()[0][2], 1);
        let p = AccessPotential {
            dx: vec![10, 0],
            dy: vec![5, 0],
            hops: vec![80, 0],
        };
        assert_eq!(p.cost(0).unwrap(), 95);
        assert_eq!(p.cost(3).unwrap(), 80);
        assert_eq!(p.cost(4).unwrap(), 15);
    }
    #[test]
    fn soft_weight_is_explicit_bounded_and_omitted_from_legacy_encoding() {
        let mut design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(690, 690, 710, 710)),
            ],
            Vec::new(),
        );
        let limits = limits();
        let original = serde_json::to_value(design.tracks.as_ref().unwrap()).unwrap();
        assert!(original.get("repair_heuristic_weight").is_none());
        design.tracks.as_mut().unwrap().heuristic_weight = 4;
        design.tracks.as_mut().unwrap().repair_heuristic_weight = Some(1);
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(&design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        assert_eq!(router.detail_weight(false), 4);
        assert_eq!(router.detail_weight(true), 1);
        router.heuristic_weight_override = Some(4);
        assert_eq!(router.detail_weight(false), 4);
        assert_eq!(router.detail_weight(true), 4);
        drop(router);
        for invalid_weight in [0, 17, u64::MAX] {
            design.tracks.as_mut().unwrap().repair_heuristic_weight = Some(invalid_weight);
            assert!(check_plan(&design, design.tracks.as_ref().unwrap()).is_err());
        }
    }

    #[test]
    fn owner_weight_is_bounded_and_restored_for_all_trial_outcomes() {
        use crate::routing::{SingleOwnerRepairConfig, SingleOwnerRepairStats};
        // Ten tiny (<1,000-state) trials, no PDK parse or path enumeration.
        let config = SingleOwnerRepairConfig {
            max_candidates: 4,
            max_frontier_nodes: 4096,
            first_search_expansions: 50_000,
            max_progress_candidates: 1,
            progress_search_expansions: 200_000,
            max_total_expansions: 1_000_000,
            post_soft_refinement: None,
            heuristic_weight: Some(1),
        };
        let limits = limits();
        config.validate(&limits).unwrap();
        let mut legacy = config.clone();
        legacy.heuristic_weight = None;
        let encoded = serde_json::to_value(&legacy).unwrap();
        assert!(encoded.get("heuristic_weight").is_none());
        assert_eq!(
            serde_json::from_value::<SingleOwnerRepairConfig>(encoded).unwrap(),
            legacy
        );
        for value in [0, 17, u64::MAX] {
            let mut invalid = config.clone();
            invalid.heuristic_weight = Some(value);
            assert!(invalid.validate(&limits).is_err());
        }
        for outer in [None, Some(2)] {
            for case in 0..5 {
                let mut design = input(
                    vec![
                        terminal(0, 7, true, rect(90, 90, 110, 110)),
                        terminal(1, 7, false, rect(690, 690, 710, 710)),
                    ],
                    Vec::new(),
                );
                let plan = design.tracks.as_mut().unwrap();
                plan.heuristic_weight = 4;
                plan.repair_heuristic_weight = Some(2);
                if case == 4 {
                    plan.routing_layers = Some([0, 1]);
                }
                let graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &limits).unwrap();
                let cells = Cells::new(design.die, design.tracks.as_ref().unwrap().gcell).unwrap();
                let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
                let terminals: Vec<_> = design.terminals.iter().collect();
                let mut plans = router.reserve(&terminals).unwrap();
                router.gmark.fill(1);
                router.gepoch = 1;
                for layer in 0..3 {
                    router
                        .spatial
                        .add_routed(Blockage {
                            layer,
                            rect: rect(design.die.lx, 390, design.die.ux, 410),
                            net: Some(9),
                        })
                        .unwrap();
                }
                router.heuristic_weight_override = outer;
                router.single_owner_stats = Some(SingleOwnerRepairStats::default());
                if case == 3 {
                    router.spatial.work = limits.max_comparisons;
                } else if case == 4 {
                    plans.get_mut(&terminals[0].pin).unwrap().top = router.graph.node(2, 0, 0);
                }
                let rips = BTreeMap::new();
                let request = owner_repair::Request {
                    config: &config,
                    net: 7,
                    terminals: &terminals,
                    plans: &plans,
                    strict: false,
                    rips: &rips,
                    round: 1,
                    failed_pin: terminals[1].pin,
                };
                let result = owner_repair::trial(
                    &mut router,
                    &request,
                    &mut owner_repair::Allocation::new(&config),
                    if case == 1 { 8 } else { 9 },
                    if case == 2 { 1 } else { 200_000 },
                );
                match case {
                    0 => assert!(matches!(result, Ok(Outcome::Routed(_, _)))),
                    1 => assert!(matches!(result, Ok(Outcome::Failed(_)))),
                    2 => assert!(matches!(result, Ok(Outcome::Limited(_)))),
                    3 => assert!(matches!(result, Err(crate::Error::Budget(_)))),
                    4 => assert!(
                        matches!(result, Err(crate::Error::Invalid(ref message)) if message.contains("NO_ROUTE"))
                    ),
                    _ => unreachable!(),
                }
                assert_eq!(router.heuristic_weight_override, outer);
                assert_eq!(router.detail_weight(false), outer.unwrap_or(4));
                assert_eq!(router.detail_weight(true), outer.unwrap_or(2));
                assert_eq!(
                    router.single_owner_stats.as_ref().unwrap().heuristic_weight,
                    Some(1)
                );
                assert!(router.allowed_victim.is_none());
                assert!(router.deferred_history.is_none());
                assert!(router.trial_search_limit.is_none());
                assert!(router.trial_expansion_limit.is_none());
            }
        }
    }

    #[test]
    fn post_soft_failure_restores_geometry_history_and_shared_allocation() {
        use crate::routing::{PostSoftRefinement, SingleOwnerRepairConfig};
        let design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(690, 690, 710, 710)),
                terminal(4, 7, false, rect(890, 690, 910, 710)),
                terminal(2, 9, true, rect(890, 890, 910, 910)),
                terminal(3, 8, true, rect(790, 890, 810, 910)),
            ],
            Vec::new(),
        );
        let limits = limits();
        let terminals: Vec<_> = design.terminals.iter().filter(|t| t.net == 7).collect();
        let prepare = || {
            let graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &limits).unwrap();
            let cells = Cells::new(design.die, design.tracks.as_ref().unwrap().gcell).unwrap();
            let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
            let plans = router.reserve(&terminals).unwrap();
            router.gmark.fill(1);
            router.gepoch = 1;
            for layer in 0..3 {
                for (y, net) in [(390, 8), (590, 9)] {
                    router
                        .spatial
                        .add_routed(Blockage {
                            layer,
                            rect: rect(design.die.lx, y, design.die.ux, y + 20),
                            net: Some(net),
                        })
                        .unwrap();
                }
            }
            (router, plans)
        };
        let live_shapes = |router: &Router<'_>| {
            let value = serde_json::to_value(router.spatial.capture().unwrap()).unwrap();
            value["shapes"]
                .as_array()
                .unwrap()
                .iter()
                .zip(value["alive"].as_array().unwrap())
                .filter_map(|(shape, alive)| alive.as_bool().unwrap().then_some(shape.clone()))
                .collect::<Vec<_>>()
        };
        let rips = BTreeMap::new();
        let (mut baseline, plans) = prepare();
        let Outcome::Routed(original, original_victims) = baseline
            .route_net(7, &terminals, &plans, true, false, &rips, 1)
            .unwrap()
        else {
            panic!("two routed barriers must be rippable together");
        };
        assert_eq!(original_victims, [8, 9]);
        let history = baseline.history.capture().unwrap();
        assert!(
            !history.is_empty(),
            "fixture must exercise the history journal"
        );
        let shapes = live_shapes(&baseline);
        let baseline_expansions = baseline.expansions;
        let config = SingleOwnerRepairConfig {
            max_candidates: 4,
            max_frontier_nodes: 4096,
            first_search_expansions: 50_000,
            max_progress_candidates: 2,
            progress_search_expansions: 200_000,
            max_total_expansions: 1_000_000,
            post_soft_refinement: Some(PostSoftRefinement::SingleOwner),
            heuristic_weight: None,
        };
        let mut legacy = config.clone();
        legacy.post_soft_refinement = None;
        let legacy_json = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_json.get("post_soft_refinement").is_none());
        let decoded: SingleOwnerRepairConfig = serde_json::from_value(legacy_json.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), legacy_json);
        assert!(config.working_bytes(&limits, 3) > legacy.working_bytes(&limits, 3));
        for quota in [1, 1_000_000] {
            let config = SingleOwnerRepairConfig {
                max_total_expansions: quota,
                ..config.clone()
            };
            let (mut router, plans) = prepare();
            let mut allocation = owner_repair::Allocation::new(&config);
            let request = owner_repair::Request {
                config: &config,
                net: 7,
                terminals: &terminals,
                plans: &plans,
                strict: false,
                rips: &rips,
                round: 1,
                failed_pin: terminals[1].pin,
            };
            if quota == 1 {
                assert!(matches!(
                    router
                        .route_net(7, &terminals, &plans, false, false, &rips, 1)
                        .unwrap(),
                    Outcome::Failed(_)
                ));
                assert!(
                    owner_repair::run(&mut router, &request, &mut allocation)
                        .unwrap()
                        .is_none()
                );
            }
            let Outcome::Routed(route, victims) =
                owner_repair::soft(&mut router, &request, &mut allocation).unwrap()
            else {
                panic!("failed refinement must retain the completed fallback");
            };
            assert_eq!(route.wires, original.wires);
            assert_eq!(route.vias, original.vias);
            assert_eq!(victims, original_victims);
            assert_eq!(router.history.capture().unwrap(), history);
            assert_eq!(live_shapes(&router), shapes);
            assert!(router.recorded_history.is_none() && router.deferred_history.is_none());
            let stats = router.single_owner_stats.unwrap();
            assert!(stats.trial_expansions <= quota);
            let post = stats.post_soft_refinement.unwrap();
            assert_eq!(post.successes, 0);
            if quota == 1 {
                assert_eq!(
                    stats.trial_expansions, 1,
                    "pre and post share one allocation"
                );
                assert_eq!(post.attempts, 0);
                assert_eq!(post.quota_skips, 1);
            } else {
                assert_eq!(post.fallbacks, 1);
                assert_eq!(post.attempts, 2);
            }
        }
        let (mut router, plans) = prepare();
        router.expansions = limits.max_expansions - baseline_expansions;
        let mut allocation = owner_repair::Allocation::new(&config);
        let error = owner_repair::soft(
            &mut router,
            &owner_repair::Request {
                config: &config,
                net: 7,
                terminals: &terminals,
                plans: &plans,
                strict: false,
                rips: &rips,
                round: 1,
                failed_pin: terminals[1].pin,
            },
            &mut allocation,
        )
        .err()
        .expect("aggregate exhaustion must abort");
        assert!(
            error.to_string().contains("routing expansion limit: total"),
            "{error}"
        );
        assert!(router.expansions > limits.max_expansions);
        assert!(router.recorded_history.is_none() && router.deferred_history.is_none());
    }

    #[test]
    fn hard_replay_preserves_occupied_routes_and_requires_an_explicit_bound() {
        let design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(690, 690, 710, 710)),
                terminal(2, 9, true, rect(890, 890, 910, 910)),
                terminal(3, 8, true, rect(790, 890, 810, 910)),
            ],
            Vec::new(),
        );
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rustroad-hard-replay-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let snapshot = directory.join("state.json");
        let mut limits = limits();
        limits.replay = Some(replay::ReplayCapture {
            path: snapshot.clone(),
            net: Some(7),
            round: Some(7),
            max_bytes: 1 << 20,
            phase: replay::ReplayPhase::PreSoft,
        });
        let graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &limits).unwrap();
        let cells = Cells::new(design.die, design.tracks.as_ref().unwrap().gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        let terminals: Vec<_> = design.terminals.iter().filter(|t| t.net == 7).collect();
        let plans = router.reserve(&terminals).unwrap();
        router.gmark.fill(1);
        router.gepoch = 1;
        for layer in 0..3 {
            router
                .spatial
                .add_routed(Blockage {
                    layer,
                    rect: rect(design.die.lx, 390, design.die.ux, 410),
                    net: Some(9),
                })
                .unwrap();
        }
        replay::capture_if_requested(&router, 7, &terminals, &plans, false, &BTreeMap::new(), 7)
            .unwrap();
        let original = std::fs::read(&snapshot).unwrap();
        let soft = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions::default(),
        )
        .unwrap();
        assert_eq!(soft.search_mode, "soft_ripup");
        assert_eq!(soft.status, "routed");
        assert_eq!(soft.victims, [9]);
        let hard = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                max_search_expansions: Some(limits.max_search_expansions),
                hard_search: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(hard.search_mode, "hard_no_ripup");
        assert_eq!(hard.status, "no_route");
        assert!(hard.victims.is_empty() && hard.wires.is_none());
        let traced = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                max_search_expansions: Some(limits.max_search_expansions),
                hard_search: true,
                probe_search_frontier: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(traced.status, hard.status);
        assert_eq!(traced.expansions_after, hard.expansions_after);
        assert!(!traced.search_frontier.unwrap().is_empty());
        assert!(traced.search_frontier_work.unwrap() > 0);
        assert!(
            traced
                .boundary_owners
                .unwrap()
                .iter()
                .any(|row| row.net == 9)
        );
        let stage_config = crate::routing::SingleOwnerRepairConfig {
            max_candidates: 4,
            max_frontier_nodes: 4096,
            first_search_expansions: 50_000,
            max_progress_candidates: 2,
            progress_search_expansions: 200_000,
            max_total_expansions: 1_000_000,
            post_soft_refinement: None,
            heuristic_weight: None,
        };
        let selected = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                single_owner_repair: Some(stage_config.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(selected.status, "routed");
        assert_eq!(selected.victims, [9]);
        let stats = selected.single_owner_repair.unwrap();
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.attempts, 1);
        assert!(
            stats.trial_expansions > 0
                && stats.trial_expansions <= stage_config.max_total_expansions
        );
        assert!(stats.committed_history_updates > 0);
        let fallback = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                single_owner_repair: Some(crate::routing::SingleOwnerRepairConfig {
                    max_total_expansions: 1,
                    ..stage_config.clone()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            fallback.status, "routed",
            "local allocation falls back without resetting the global ledger"
        );
        let stats = fallback.single_owner_repair.unwrap();
        assert_eq!(stats.trial_expansions, 1);
        assert_eq!(stats.successes, 0);
        assert_eq!(stats.committed_history_updates, 0);
        assert_eq!(stats.local_budget_exhaustions, 1);
        for invalid in [
            crate::routing::SingleOwnerRepairConfig {
                max_candidates: 17,
                ..stage_config.clone()
            },
            crate::routing::SingleOwnerRepairConfig {
                max_frontier_nodes: 0,
                ..stage_config.clone()
            },
            crate::routing::SingleOwnerRepairConfig {
                first_search_expansions: 500001,
                ..stage_config.clone()
            },
            crate::routing::SingleOwnerRepairConfig {
                max_total_expansions: 20000001,
                ..stage_config.clone()
            },
        ] {
            assert!(invalid.validate(&limits).is_err());
        }
        assert!(
            serde_json::to_value(design.tracks.as_ref().unwrap())
                .unwrap()
                .get("single_owner_repair")
                .is_none()
        );
        for (owner, expected) in [(9, "routed"), (8, "no_route")] {
            let single = crate::routing::replay_route(
                &design,
                &limits,
                &snapshot,
                &replay::ReplayOptions {
                    max_search_expansions: Some(limits.max_search_expansions),
                    allowed_victim: Some(owner),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(single.search_mode, "soft_single_owner");
            assert_eq!(single.status, expected);
            assert!(single.victims.iter().all(|&net| net == owner));
            if owner == 9 {
                assert_eq!(single.victims, [9]);
            }
        }
        let limited = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                max_search_expansions: Some(1),
                hard_search: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(limited.status, "budget_exceeded");
        for options in [
            replay::ReplayOptions {
                hard_search: true,
                ..Default::default()
            },
            replay::ReplayOptions {
                hard_search: true,
                probe_terminals: true,
                max_search_expansions: Some(limits.max_search_expansions),
                ..Default::default()
            },
            replay::ReplayOptions {
                probe_search_frontier: true,
                ..Default::default()
            },
            replay::ReplayOptions {
                allowed_victim: Some(9),
                ..Default::default()
            },
            replay::ReplayOptions {
                allowed_victim: Some(7),
                max_search_expansions: Some(limits.max_search_expansions),
                ..Default::default()
            },
            replay::ReplayOptions {
                allowed_victim: Some(u32::MAX),
                max_search_expansions: Some(limits.max_search_expansions),
                ..Default::default()
            },
            replay::ReplayOptions {
                allowed_victim: Some(9),
                hard_search: true,
                max_search_expansions: Some(limits.max_search_expansions),
                ..Default::default()
            },
        ] {
            assert!(crate::routing::replay_route(&design, &limits, &snapshot, &options).is_err());
        }
        assert_eq!(original, std::fs::read(&snapshot).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_replay_roundtrips_one_search_and_rejects_stale_or_excessive_state() {
        let design = input(
            vec![
                terminal(0, 7, true, rect(90, 90, 110, 110)),
                terminal(1, 7, false, rect(690, 690, 710, 710)),
            ],
            Vec::new(),
        );
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("rustroad-replay-{}-{unique}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let snapshot = directory.join("state.json");
        let mut limits = limits();
        limits.replay = Some(replay::ReplayCapture {
            path: snapshot.clone(),
            net: Some(7),
            round: Some(7),
            max_bytes: 1 << 20,
            phase: replay::ReplayPhase::PreSoft,
        });
        let graph = Graph::new(&design, design.tracks.as_ref().unwrap(), &limits).unwrap();
        let cells = Cells::new(design.die, design.tracks.as_ref().unwrap().gcell).unwrap();
        let mut router = Router::prepared(&design, &limits, graph, cells).unwrap();
        let terminals: Vec<_> = design.terminals.iter().collect();
        let plans = router.reserve(&terminals).unwrap();
        router.gmark.fill(1);
        router.gepoch = 1;
        let before = (router.spatial.work, router.expansions);
        replay::capture_if_requested(&router, 7, &terminals, &plans, false, &BTreeMap::new(), 7)
            .unwrap();
        assert_eq!(
            before,
            (router.spatial.work, router.expansions),
            "capture must not consume routing state"
        );
        let original = std::fs::read(&snapshot).unwrap();
        let probe = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions {
                probe_terminals: true,
                heuristic_weight: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(probe.status, "geometry_probed");
        assert_eq!(probe.expansions_before, probe.expansions_after);
        assert!(probe.wires.is_none() && probe.vias.is_none());
        assert_eq!(probe.configured_heuristic_weight, 1);
        assert_eq!(probe.used_heuristic_weight, 2);
        let probes = probe.terminal_probes.unwrap();
        assert_eq!(probes.len(), 2);
        assert!(probes.iter().all(|p| !p.neighbors.is_empty()));
        assert!(probes.iter().flat_map(|p| &p.neighbors).any(|p| {
            p.geometry == "clear_or_rippable" && p.in_guide && p.routing_layer_allowed
        }));
        assert_eq!(original, std::fs::read(&snapshot).unwrap());
        for weight in [0, 17] {
            assert!(
                crate::routing::replay_route(
                    &design,
                    &limits,
                    &snapshot,
                    &replay::ReplayOptions {
                        heuristic_weight: Some(weight),
                        ..Default::default()
                    },
                )
                .is_err()
            );
        }
        let mut legacy: serde_json::Value = serde_json::from_slice(&original).unwrap();
        legacy.as_object_mut().unwrap().remove("phase");
        legacy.as_object_mut().unwrap().remove("failed_pin");
        let legacy_path = directory.join("legacy-pre-soft.json");
        std::fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let legacy_report = crate::routing::replay_route(
            &design,
            &limits,
            &legacy_path,
            &replay::ReplayOptions::default(),
        )
        .unwrap();
        assert_eq!(
            legacy_report.comparisons_before, before.0,
            "legacy restoration must not charge the reconstructed spatial index twice"
        );
        assert_eq!(legacy_report.source_phase, replay::ReplayPhase::PreSoft);
        assert!(legacy_report.source_failed_pin.is_none());
        assert!(
            replay::capture_if_requested(
                &router,
                7,
                &terminals,
                &plans,
                false,
                &BTreeMap::new(),
                7
            )
            .is_err(),
            "published snapshots cannot be overwritten"
        );
        assert_eq!(original, std::fs::read(&snapshot).unwrap());
        let Outcome::Routed(direct, victims) = router
            .route_net(7, &terminals, &plans, true, false, &BTreeMap::new(), 7)
            .unwrap()
        else {
            panic!("small direct search must route");
        };
        let report = crate::routing::replay_route(
            &design,
            &limits,
            &snapshot,
            &replay::ReplayOptions::default(),
        )
        .unwrap();
        assert_eq!(report.status, "routed");
        assert_eq!(report.wires.as_ref().unwrap(), &direct.wires);
        assert_eq!(report.vias.as_ref().unwrap(), &direct.vias);
        assert_eq!(report.victims, victims);
        let mut outer = limits.clone();
        outer.max_comparisons += 100;
        outer.max_expansions += 100;
        let bounded = crate::routing::replay_route(
            &design,
            &outer,
            &snapshot,
            &replay::ReplayOptions::default(),
        )
        .unwrap();
        assert_eq!(
            bounded.wires, report.wires,
            "batch ledger must retain captured smaller budgets"
        );
        outer.max_comparisons = limits.max_comparisons - 1;
        assert!(
            crate::routing::replay_route(
                &design,
                &outer,
                &snapshot,
                &replay::ReplayOptions::default()
            )
            .unwrap_err()
            .to_string()
            .contains("REPLAY_LIMITS_CHANGED")
        );

        let mut changed = design;
        changed.grid = 10;
        assert!(
            crate::routing::replay_route(
                &changed,
                &limits,
                &snapshot,
                &replay::ReplayOptions::default()
            )
            .unwrap_err()
            .to_string()
            .contains("REPLAY_INPUT_CHANGED")
        );
        let oversized = directory.join("oversized.json");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len((512 << 20) + 1)
            .unwrap();
        assert!(matches!(
            crate::routing::replay_route(
                &changed,
                &limits,
                &oversized,
                &replay::ReplayOptions::default()
            ),
            Err(crate::Error::Budget(_))
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
