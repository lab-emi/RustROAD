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
use crate::route_rules;
use crate::route_search::{History, Search, State};
use crate::routing::{
    Access, AccessEntry, Blockage, LayerRect, RouteInput, RouteLimits, RouteStats, Routes, Soft,
    Spatial, Terminal, TrackPlan, Via, ViaPair, WIRE, Wire, access_table_at, budget, contains,
    distance, expand, int, invalid, no_route, quartile_points, shifted, via_pairs, wire_rect,
};
use crate::{Rect, Result};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

const NONE: u32 = u32::MAX;
/// Bytes per graph node held by the detailed search (cost, parent, move,
/// epoch tag, history).
const NODE_BYTES: u64 = 24;
/// Bytes per GCell (search state and guide marks) and per GCell edge.
const GCELL_BYTES: u64 = 48;
/// Bytes per spatial shape: the blockage (48), query mark (8), alive and
/// routed flags, rounded up.
const SHAPE_BYTES: u64 = 64;
/// One route-backed shape of a validation index: a u64 query mark and, for a
/// via shape, a u32 via and u8 shape index, rounded up.
const ROUTE_SHAPE_BYTES: u64 = 16;
/// Bytes per spatial bin entry: 8, doubled for vector growth.
const ENTRY_BYTES: u64 = 16;
/// Bytes per grid bin of one layer (head and tail entry index).
const GRID_BYTES: u64 = 8;
/// Shapes the router adds per terminal (escape, routed runs and vias,
/// ripped-up shapes that stay indexed; measured 13-16), and bin entries per
/// added shape (measured 2.4-4 over all shapes). Two further shapes per
/// terminal bound negotiated rule-fill reservations.
const SHAPES_PER_TERMINAL: u64 = 22;
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
/// Sorted window index plus construction headroom (one entry per terminal).
const WINDOW_INDEX_BYTES: u64 = 32;
/// Alternate access: at most one landing zone per terminal: a 56-byte zone
/// record, its x-index (B-tree) and terminal-map entries, with growth
/// headroom (~200 bytes worst case), rounded up.
const LANDING_BYTES: u64 = 256;
/// Alternate access: extra cost, in access steps, of an escape move whose
/// metal comes within spacing of a protected landing zone (one escape-window
/// radius): a free escape up to that much longer wins, and an escape that
/// can only encroach is still taken.
const LANDING_PENALTY_STEPS: u64 = 4;
/// No-path diagnostics (trace mode only): shapes within this many access
/// steps of the failing sink's start, at most `DUMP_SHAPES` of them, and at
/// most `DUMP_GUIDE` guide cells.
const DUMP_RADIUS_STEPS: i64 = 24;
const DUMP_SHAPES: usize = 20_000;
const DUMP_GUIDE: usize = 1024;
type AccessKey = (u32, Vec<(u32, [i64; 2])>);
type AccessTables = BTreeMap<AccessKey, Vec<AccessEntry>>;

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
            internal != 0 && !t.driver,
            p[0].abs_diff(x).saturating_add(p[1].abs_diff(y)),
            !t.driver,
            t.pin,
        )
    });
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

/// Immutable x-interval index for pin-access repair. A query scans only
/// lower bounds in [query.lx - widest_window, query.ux], then checks y and
/// exact overlap. Every candidate and the one-time build are work charged.
pub(crate) struct WindowIndex {
    order: Vec<(i64, usize)>,
    width: i64,
}
impl WindowIndex {
    pub(crate) fn new(windows: &[Rect], work: &mut usize, cap: usize) -> Result<Self> {
        *work = work
            .checked_add(
                windows
                    .len()
                    .saturating_mul(windows.len().checked_ilog2().unwrap_or(0) as usize + 1),
            )
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
    pub(crate) fn touches_any(
        &self,
        windows: &[Rect],
        query: Rect,
        work: &mut usize,
        cap: usize,
    ) -> Result<bool> {
        let lo = query.lx.saturating_sub(self.width);
        let begin = self.order.partition_point(|&(x, _)| x < lo);
        let end = self.order.partition_point(|&(x, _)| x <= query.ux);
        let search_work = 2 * (self.order.len().checked_ilog2().unwrap_or(0) as usize + 1);
        *work = work
            .checked_add(search_work.saturating_add(end - begin))
            .filter(|&n| n <= cap)
            .ok_or_else(|| budget("batch access window-query work"))?;
        Ok(self.order[begin..end].iter().any(|&(_, i)| {
            let r = windows[i];
            r.lx <= query.ux && query.lx <= r.ux && r.ly <= query.uy && query.ly <= r.uy
        }))
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
    slot: HashMap<u32, usize>,
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
        let mut slot = HashMap::new();
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

/// Layers whose geometry can block a pin-access escape, as a mask over
/// `RouteInput::layers`: every routing layer from the bottom of the via stack
/// to the access layer, the layers one via above it (an escape may end in a
/// via there) and the shapes of the vias between them. Every bit when the
/// layers do not fit the mask.
pub(crate) fn access_stack(input: &RouteInput) -> u64 {
    let fits = input.layers.len() <= 64
        && input
            .vias
            .iter()
            .flat_map(|v| v.shapes.iter().map(|s| s.layer).chain([v.lower, v.upper]))
            .chain([input.access_layer])
            .all(|l| (l as usize) < input.layers.len());
    if !fits {
        return u64::MAX;
    }
    let mut metal = 1u64 << input.access_layer;
    loop {
        let below = input
            .vias
            .iter()
            .filter(|v| metal & (1 << v.upper) != 0)
            .fold(metal, |m, v| m | 1 << v.lower);
        if below == metal {
            break;
        }
        metal = below;
    }
    metal |= input
        .vias
        .iter()
        .filter(|v| v.lower == input.access_layer)
        .fold(0, |m, v| m | 1 << v.upper);
    input
        .vias
        .iter()
        .filter(|v| metal & (1 << v.lower) != 0 && metal & (1 << v.upper) != 0)
        .flat_map(|v| &v.shapes)
        .fold(metal, |m, s| m | 1 << s.layer)
}

/// `access_stack` for one terminal; every layer when a pin shape lies outside it.
fn escape_layers(stack: u64, t: &Terminal) -> u64 {
    if t.shapes
        .iter()
        .all(|s| s.layer < 64 && stack & (1 << s.layer) != 0)
    {
        stack
    } else {
        u64::MAX
    }
}

/// Per input layer: percent of a wire edge's length that the detailed search
/// charges (`TrackPlan::wire_cost_percent`, 100 when the plan is uniform).
fn wire_costs(input: &RouteInput, plan: &TrackPlan) -> Vec<u64> {
    (0..input.layers.len())
        .map(|l| plan.wire_cost_percent.get(l).map_or(100, |&p| u64::from(p)))
        .collect()
}
/// Validate the plan against the input (called from `routing::preflight`).
pub(crate) fn check_plan(input: &RouteInput, plan: &TrackPlan) -> Result<()> {
    if !input.rules {
        return Err(invalid("track router requires LEF-rule mode"));
    }
    if input.layers.len().saturating_mul(input.vias.len()) > MAX_VIA_PAD_ENTRIES {
        return Err(budget("via-pad area table"));
    }
    if plan.tracks.len() != input.layers.len()
        || plan.gcell <= 0
        || plan.global_iterations > 1000
        || plan.capacity_percent == 0
        || plan.capacity_percent > 100
        || plan.guide_margin > 64
        || plan.expand_margin > 256
        || plan.max_ripups_per_net > 1000
        || plan.max_repair_rounds > 1_000_000
        || plan.via_cost == 0
        || plan.via_cost > 1 << 40
        || plan.wrong_way_factor == 0
        || plan.wrong_way_factor > 1000
        || (!plan.wire_cost_percent.is_empty()
            && plan.wire_cost_percent.len() != input.layers.len())
        || plan
            .wire_cost_percent
            .iter()
            .any(|p| !(100..=1000).contains(p))
        || !(1..=16).contains(&plan.heuristic_weight)
        || plan.memory_mib == 0
        || plan.memory_mib > 65_536
        || plan.max_history_entries == 0
        || plan.max_history_entries > 64_000_000
    {
        return Err(invalid("track plan parameters"));
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
/// Expected bin entries of the router's added shapes: the escape, wire and
/// via shapes `memory_estimate_limited` charges for its terminals.
fn router_entries(input: &RouteInput, limits: &RouteLimits) -> u64 {
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
    (input.terminals.len() as u64)
        .saturating_mul(SHAPES_PER_TERMINAL.saturating_add(portal_shapes))
        .min(limits.max_obstacles.saturating_sub(fixed_records) as u64)
        .saturating_mul(ENTRIES_PER_SHAPE)
}
/// Bin layout of a track route's spatial indexes (`routing::BinGrid`).
struct BinLayout {
    mode: crate::routing::BinMode,
    nx: i64,
    ny: i64,
    layers: usize,
    slots: u64,
    pages: u64,
}
impl BinLayout {
    fn new(input: &RouteInput, bin: i64) -> Self {
        let (x0, x1, y0, y1, cells) = crate::routing::bin_extent(input, bin);
        let layers = input.layers.len();
        Self {
            mode: crate::routing::bin_mode(input, cells),
            nx: x1 - x0 + 1,
            ny: y1 - y0 + 1,
            layers,
            slots: cells.saturating_mul(layers as u64),
            pages: crate::routing::grid_pages(x0, x1, y0, y1).saturating_mul(layers as u64),
        }
    }
    /// Grid bytes of a fixed-layout index holding `entries` bin entries.
    fn fixed_grid(&self, entries: u64) -> u64 {
        match self.mode {
            crate::routing::BinMode::Sparse => self.slots.min(entries).saturating_mul(96),
            crate::routing::BinMode::Dense => self.slots.saturating_mul(GRID_BYTES),
            _ => self
                .pages
                .min(entries)
                .saturating_mul(crate::routing::PAGE_BYTES),
        }
    }
    /// Grid bytes of a Spatial's added index holding `entries` bin entries,
    /// in the layout `routing::added_hashed` chooses.
    fn added_grid(&self, entries: u64) -> u64 {
        if crate::routing::added_hashed(
            self.mode,
            self.nx,
            self.ny,
            self.layers,
            self.pages,
            entries,
        ) {
            entries.saturating_mul(crate::routing::HASHED_BIN_BYTES)
        } else {
            self.fixed_grid(entries)
        }
    }
    /// Bytes of the fixed index (obstacles and instances) of a route call.
    /// Fixed rectangles are shared with RouteInput and need no query marks.
    fn fixed_index(
        &self,
        input: &RouteInput,
        bin: i64,
        limits: &RouteLimits,
    ) -> Result<(u64, u64)> {
        let mut entries = 0u64;
        for b in &input.obstacles {
            let span =
                |lo: i64, hi: i64| (hi.div_euclid(bin) - lo.div_euclid(bin) + 1).max(0) as u64;
            entries = entries.saturating_add(
                span(b.rect.lx, b.rect.ux).saturating_mul(span(b.rect.ly, b.rect.uy)),
            );
        }
        let bytes = self
            .fixed_grid(entries)
            .saturating_add(crate::routing::instance_index_bytes(input, limits)?)
            .saturating_add(entries.saturating_mul(ENTRY_BYTES));
        Ok((bytes, entries))
    }
}

fn memory_estimate_limited(
    input: &RouteInput,
    nodes: usize,
    gcells: usize,
    limits: &RouteLimits,
) -> Result<MemoryEstimate> {
    let bin = crate::routing::bin_side(input)?;
    let layout = BinLayout::new(input, bin);
    let layers = layout.layers as u64;
    let (fixed, fixed_entries) = layout.fixed_index(input, bin, limits)?;
    let cells = layout.slots / layers.max(1);
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
    let router_shapes = t
        .saturating_mul(SHAPES_PER_TERMINAL.saturating_add(portal_shapes))
        .min(limits.max_obstacles.saturating_sub(fixed_records) as u64);
    // One fixed index (obstacles and instances) serves every Spatial of the
    // route call.
    // Each Spatial bins its added geometry apart, in the fixed layout or
    // hashed per occupied bin as `routing::added_hashed` chooses for its
    // expected entries.
    let added = |shapes: u64| {
        // Both Spatial implementations enforce this before every insertion.
        // The checker cannot retain 2*max_segments shapes past this limit.
        let shapes = shapes.min(limits.max_obstacles.saturating_sub(fixed_records) as u64);
        let entries = shapes.saturating_mul(ENTRIES_PER_SHAPE);
        layout
            .added_grid(entries)
            .saturating_add(shapes.saturating_mul(SHAPE_BYTES))
            .saturating_add(entries.saturating_mul(ENTRY_BYTES))
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
            (nodes.min(limits.max_search_expansions.saturating_add(limits.max_heap)) as u64)
                .saturating_mul(64)
        })
        .saturating_add((nodes.min(limits.max_heap) as u64).saturating_mul(128))
        .saturating_add((gcells as u64).saturating_mul(GCELL_BYTES))
        .saturating_add(access_memory)
        .saturating_add(layers.saturating_mul(input.vias.len() as u64 + 32))
        .saturating_add(crate::routing_target::memory_bytes(
            limits.max_target_segments,
        ))
        .saturating_add((limits.max_path_retries as u64).saturating_mul(12))
        .saturating_add(
            (trace_cut_capacity(input, nodes, limits) as u64)
                .saturating_mul(std::mem::size_of::<TraceCut>() as u64),
        )
        .saturating_add((limits.max_batch_blocked_pins as u64).saturating_mul(128)),
        spatial: fixed.saturating_add(added(router_shapes)),
        routed: t
            .saturating_mul(ROUTED_BYTES_PER_TERMINAL)
            // Selected seed/generation in Plan plus one pending preference.
            .saturating_add(t.saturating_mul(32))
            .saturating_add(t.saturating_mul(LANDING_BYTES))
            .saturating_add(
                t.saturating_mul(portal_shapes.saturating_add(1))
                    .saturating_mul(8),
            )
            .saturating_add(t.saturating_mul(WINDOW_INDEX_BYTES))
            .saturating_add(
                t.saturating_mul(2)
                    .min(limits.max_segments as u64)
                    .saturating_mul(80),
            )
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
        check: added(
            records
                .saturating_mul(2)
                .saturating_add(t.saturating_mul(2).min(limits.max_segments as u64)),
        )
        .saturating_add(records.saturating_mul(RECORD_BYTES))
        .saturating_add(rule_grid_cells.saturating_mul(64)),
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

/// Extra memory of a full-design validation once the completed output
/// records are resident: its own fixed index; every route shape in the added
/// index, from the exact shape and bin-entry counts of `out` (`validate`
/// reserves exactly these) in the layout `routing::added_hashed` chooses,
/// where a shape is computed from its record and keeps only a query mark and
/// at most a via reference (`ROUTE_SHAPE_BYTES`); the rule grid; and the
/// per-net connectivity and rule-check state.
pub(crate) fn validation_working_bytes(
    input: &RouteInput,
    out: &Routes,
    limits: &RouteLimits,
) -> Result<u64> {
    let bin = crate::routing::bin_side(input)?;
    let layout = BinLayout::new(input, bin);
    let (fixed, _) = layout.fixed_index(input, bin, limits)?;
    let mut work = 0;
    let entries = crate::routing::route_entries(input, out, bin, &mut work, limits)?;
    let shapes = crate::routing::route_shapes(input, out)?;
    let records = out
        .wires
        .len()
        .saturating_add(out.vias.len())
        .saturating_add(out.patches.len())
        .saturating_add(out.access.len()) as u64;
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
    Ok(fixed
        .saturating_add(layout.added_grid(entries))
        .saturating_add(shapes.saturating_mul(ROUTE_SHAPE_BYTES))
        .saturating_add(entries.saturating_mul(ENTRY_BYTES))
        .saturating_add(rule_grid_cells.saturating_mul(64))
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
#[derive(Clone)]
struct Plan {
    access: Access,
    source_index: usize,
    generation: usize,
    top: u32,
    /// The escape reaches `top` through a via (a further via would stack).
    top_via: bool,
    /// Search-only via envelopes at the end of `shapes`.
    portal_shapes: u8,
    wires: Box<[Wire]>,
    vias: Box<[Via]>,
    shapes: Box<[usize]>,
}

#[derive(Clone, Copy)]
struct AccessPreference {
    next_start: usize,
    generation: usize,
}
impl Default for AccessPreference {
    fn default() -> Self {
        Self {
            next_start: usize::MAX,
            generation: 0,
        }
    }
}

/// Landing zones of terminals whose escape search failed (alternate access
/// only). A zone bounds every first-via pad, on the via's upper layer, at the
/// terminal's start points. In a dense pin row a neighbour's cheapest escape
/// can cover all landings of a pin; repair then re-plans that neighbour
/// first and seals the pin again until its attempt limit blocks it. While a
/// failed terminal has no escape, other nets' escape moves that come within
/// spacing of its zone cost `LANDING_PENALTY_STEPS` more; nothing is
/// forbidden.
#[derive(Default)]
struct Landings {
    /// (zone, terminal, net, layer, terminal currently has no escape).
    zones: Vec<(Rect, usize, u32, u32, bool)>,
    by_terminal: HashMap<usize, usize>,
    /// (zone.lx, zone index); `width` bounds every zone's x extent.
    by_x: BTreeSet<(i64, usize)>,
    width: i64,
    open: usize,
}
impl Landings {
    fn insert(&mut self, terminal: usize, net: u32, layer: u32, zone: Rect) -> Result<()> {
        if self.by_terminal.contains_key(&terminal) {
            return Ok(());
        }
        self.zones
            .try_reserve(1)
            .and_then(|()| self.by_terminal.try_reserve(1))
            .map_err(|_| budget("landing-zone records"))?;
        self.width = self.width.max(
            zone.ux
                .checked_sub(zone.lx)
                .ok_or_else(|| budget("landing-zone span"))?,
        );
        let z = self.zones.len();
        self.zones.push((zone, terminal, net, layer, false));
        self.by_terminal.insert(terminal, z);
        self.by_x.insert((zone.lx, z));
        Ok(())
    }
    /// Track whether `terminal` (if it has a zone) currently lacks an escape.
    fn set_open(&mut self, terminal: usize, open: bool) {
        let Some(&z) = self.by_terminal.get(&terminal) else {
            return;
        };
        if self.zones[z].4 != open {
            self.zones[z].4 = open;
            if open {
                self.open += 1;
            } else {
                self.open -= 1;
            }
        }
    }
    /// Is metal `rect` on `layer` of terminal `me` (net `net`) closer than
    /// `spacing` to the open zone of another net's terminal?
    #[allow(clippy::too_many_arguments)]
    fn encroached(
        &self,
        me: usize,
        net: u32,
        layer: u32,
        rect: Rect,
        spacing: i64,
        work: &mut usize,
        cap: usize,
    ) -> Result<bool> {
        if self.open == 0 {
            return Ok(false);
        }
        let query = expand(rect, spacing)?;
        let lo = query.lx.saturating_sub(self.width);
        let mut scanned = self.by_x.len().checked_ilog2().unwrap_or(0) as usize + 1;
        let mut hit = false;
        for &(_, z) in self.by_x.range((lo, 0)..=(query.ux, usize::MAX)) {
            scanned += 1;
            let (zone, terminal, zone_net, zone_layer, open) = self.zones[z];
            if open
                && terminal != me
                && zone_net != net
                && zone_layer == layer
                && crate::routing::overlap(zone, query)
            {
                hit = true;
                break;
            }
        }
        *work = work
            .checked_add(scanned)
            .filter(|&n| n <= cap)
            .ok_or_else(|| budget("landing-zone query work"))?;
        Ok(hit)
    }
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
    limits: &'a RouteLimits,
    graph: Graph,
    access_hops: Vec<Vec<u16>>,
    target_costs: crate::routing_target::CostModel,
    /// Per input layer: percent of a wire edge's length charged (100 if uniform).
    wire_cost: Vec<u64>,
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
    access_entries: usize,
    access_cache_bytes: u64,
    /// Escape searches that found no legal escape (telemetry and tests).
    access_failures: usize,
    /// Last detailed sink search that ended without reaching its tree
    /// (diagnostics): pin, expanded nodes, extent of the expanded points.
    last_unreached: Option<(u32, usize, [i64; 4])>,
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

type RuleReservation = (u32, u32, i64, i64, i64, i64);

/// Protect a legal fill from the active routes that currently block it.
fn reserve_rule_clearance(
    input: &RouteInput,
    limits: &RouteLimits,
    spatial: &mut Spatial<'_>,
    violations: &[route_rules::Violation],
    reservations: &mut BTreeSet<RuleReservation>,
    maximum: usize,
) -> Result<BTreeSet<u32>> {
    let mut victims = BTreeSet::new();
    for violation in violations {
        spatial.work = spatial
            .work
            .checked_add(1)
            .filter(|&n| n <= limits.max_comparisons)
            .ok_or_else(|| budget("rule-clearance work"))?;
        let route_rules::Violation::Spacing { net, layer, a, b } = violation else {
            continue;
        };
        let fill =
            route_rules::fill_for_robust(*a, *b, input.layers[*layer as usize].width, input.grid);
        let key = (*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy);
        if reservations.contains(&key) {
            continue;
        }
        if let Soft::Clear {
            near: false,
            victims: blocked,
        } = spatial.probe_soft(*layer, fill, *net)?
            && !blocked.is_empty()
        {
            if reservations.len() >= maximum {
                return Err(budget("rule-clearance reservation limit"));
            }
            for victim in blocked {
                if !victims.contains(&victim) && victims.len() >= input.terminals.len().max(1) {
                    return Err(budget("rule-clearance victim limit"));
                }
                victims.insert(victim);
            }
            spatial.add_fixed(Blockage {
                layer: *layer,
                rect: fill,
                net: Some(*net),
            })?;
            reservations.insert(key);
        }
    }
    Ok(victims)
}

enum Outcome {
    Routed(NetRoute, Vec<u32>),
    Failed(u32),
    Limited(u32),
}

type ForbiddenVia = (u32, u32, u32);
struct TraceCut {
    layer: u32,
    rect: Rect,
    edge: ForbiddenVia,
}

fn trace_cut_capacity(input: &RouteInput, nodes: usize, limits: &RouteLimits) -> usize {
    // Count all via shapes as an inexpensive upper bound on cut shapes.
    let per_via = input.vias.iter().map(|v| v.shapes.len()).max().unwrap_or(0);
    nodes
        .min(limits.max_search_expansions)
        .saturating_mul(per_via)
        .min(limits.max_active_nodes)
}

/// Check only the new path's cuts before inserting any of its geometry. Existing
/// metal was checked during search. Exact duplicate cuts are legal and collapse
/// before the sweep; a bounded x-order sweep catches every remaining near pair.
fn trace_cut_conflict(
    input: &RouteInput,
    graph: &Graph,
    path: &[(u32, u32, u32)],
    work: &mut usize,
    limits: &RouteLimits,
) -> Result<Option<ForbiddenVia>> {
    let charge = |work: &mut usize, count: usize| -> Result<()> {
        *work = work
            .checked_add(count)
            .filter(|&n| n <= limits.max_comparisons)
            .ok_or_else(|| budget("trace cut comparison work"))?;
        Ok(())
    };
    let mut count = 0usize;
    let capacity = trace_cut_capacity(input, graph.len, limits);
    for &(_, _, kind) in path {
        charge(work, 1)?;
        if kind == WIRE {
            continue;
        }
        for shape in &input.vias[kind as usize].shapes {
            charge(work, 1)?;
            if input.layers[shape.layer as usize].vertical.is_none() {
                count = count
                    .checked_add(1)
                    .filter(|&n| n <= capacity)
                    .ok_or_else(|| budget("trace cut state limit"))?;
            }
        }
    }
    let mut cuts = Vec::new();
    cuts.try_reserve_exact(count)
        .map_err(|_| budget("trace cut allocation"))?;
    for &(a, b, kind) in path {
        if kind == WIRE {
            continue;
        }
        let at = graph.point(a).1;
        for shape in &input.vias[kind as usize].shapes {
            charge(work, 1)?;
            if input.layers[shape.layer as usize].vertical.is_none() {
                cuts.push(TraceCut {
                    layer: shape.layer,
                    rect: shifted(shape.rect, at)?,
                    edge: (a.min(b), a.max(b), kind),
                });
            }
        }
    }
    charge(
        work,
        count.saturating_mul(count.max(1).ilog2() as usize + 2),
    )?;
    cuts.sort_unstable_by_key(|c| (c.layer, c.rect.lx, c.rect.ly, c.rect.ux, c.rect.uy, c.edge));
    cuts.dedup_by(|a, b| a.layer == b.layer && a.rect == b.rect);
    for (i, a) in cuts.iter().enumerate() {
        let query = crate::routing::expand(a.rect, input.layers[a.layer as usize].spacing)?;
        for b in &cuts[i + 1..] {
            charge(work, 1)?;
            if a.layer != b.layer || b.rect.lx >= query.ux {
                break;
            }
            if crate::routing::overlap(query, b.rect) {
                return Ok(Some(b.edge));
            }
        }
    }
    Ok(None)
}

impl<'a> Router<'a> {
    /// Search state, spatial index and via-pad certificates for `input`
    /// (the caller has already checked the upfront memory estimate).
    fn new(
        input: &'a RouteInput,
        plan: &'a TrackPlan,
        limits: &'a RouteLimits,
        spatial: Spatial<'a>,
        graph: Graph,
        cells: Cells,
    ) -> Result<Self> {
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
        let target_costs = crate::routing_target::CostModel::new(
            &graph.layers.iter().map(|l| l.vertical).collect::<Vec<_>>(),
            &graph
                .pairs
                .iter()
                .map(|p| (p.lower, p.upper))
                .collect::<Vec<_>>(),
            plan.via_cost,
            plan.wrong_way_factor,
        )?;
        let mut r = Router {
            input,
            plan,
            limits,
            graph,
            access_hops,
            target_costs,
            wire_cost: wire_costs(input, plan),
            cells,
            spatial,
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
            access_entries: 0,
            access_cache_bytes: 0,
            access_failures: 0,
            last_unreached: None,
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
        r.guard("setup")?;
        Ok(r)
    }
    fn width(&self, layer: u32) -> i64 {
        self.input.layers[layer as usize].width
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
    /// The no-path failure of `pin`: its pin metal, joined with the extent of
    /// its exhausted search when that lies within eight access steps of the
    /// metal (a sealed pocket). A batched route reopens the completed nets
    /// there (see `route_batches`); a wider search keeps only the pin metal.
    fn no_path_failure(&mut self, net: u32, pin: u32, group: &[&Terminal]) -> Result<crate::Error> {
        self.spatial.work = self
            .spatial
            .work
            .checked_add(group.len())
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("no-path region work"))?;
        let terminal = group
            .iter()
            .find(|t| t.pin == pin)
            .ok_or_else(|| invalid("failed pin is not a terminal of its net"))?;
        self.spatial.work = self
            .spatial
            .work
            .checked_add(terminal.shapes.len())
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("no-path region work"))?;
        let union = |a: Rect, b: Rect| Rect {
            lx: a.lx.min(b.lx),
            ly: a.ly.min(b.ly),
            ux: a.ux.max(b.ux),
            uy: a.uy.max(b.uy),
        };
        let metal = terminal
            .shapes
            .iter()
            .map(|s| s.rect)
            .reduce(union)
            .ok_or_else(|| invalid("failed pin has no shapes"))?;
        let mut region = metal;
        if let Some((sink, _, [lx, ly, ux, uy])) = self.last_unreached
            && sink == pin
            && lx <= ux
            && ly <= uy
        {
            let near = expand(metal, self.input.step.saturating_mul(8))?;
            if contains(near, [lx, ly]) && contains(near, [ux, uy]) {
                region = union(region, Rect { lx, ly, ux, uy });
            }
        }
        Ok(crate::Error::NoPath(Box::new(crate::NoPathFailure {
            pin,
            net,
            region,
            comparisons: self.spatial.work,
            expansions: self.expansions,
        })))
    }
    /// Bounded no-path diagnostic (trace mode only): the repair region, every
    /// terminal's access plan, the extent of the last unreached sink search
    /// and the stored and instance shapes near the failing sink's start.
    /// Every visited bin and shape is charged.
    fn dump_no_path(
        &mut self,
        net: u32,
        pin: u32,
        group: &[&Terminal],
        plans: &HashMap<u32, Plan>,
        guide: &[u32],
        margin: usize,
    ) -> Result<()> {
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for &c in guide {
            let (x, y) = self.cells.xy(c);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        let (x0, y0) = (x0.saturating_sub(margin), y0.saturating_sub(margin));
        let (x1, y1) = (
            (x1 + margin).min(self.cells.nx - 1),
            (y1 + margin).min(self.cells.ny - 1),
        );
        let g = self.cells.g;
        let at = |i: usize, origin: i64| origin + i as i64 * g;
        eprintln!(
            "[troute-dump] net {net} pin {pin} region gcells [{x0}, {y0}, {x1}, {y1}] dbu [{}, {}, {}, {}] margin {margin} guide {}",
            at(x0, self.cells.lx),
            at(y0, self.cells.ly),
            at(x1 + 1, self.cells.lx),
            at(y1 + 1, self.cells.ly),
            guide.len()
        );
        let cells: Vec<_> = guide
            .iter()
            .take(DUMP_GUIDE)
            .map(|&c| self.cells.xy(c))
            .collect();
        eprintln!("[troute-dump] guide {cells:?}");
        for t in group.iter().take(64) {
            let plan = &plans[&t.pin];
            eprintln!(
                "[troute-dump] terminal pin {} driver {} top {:?} top_via {} shapes {:?} escape wires {:?} vias {:?}",
                t.pin,
                t.driver,
                self.graph.point(plan.top),
                plan.top_via,
                &t.shapes[..t.shapes.len().min(16)],
                plan.wires,
                plan.vias
            );
        }
        if let Some((sink, expanded, extent)) = self.last_unreached {
            eprintln!(
                "[troute-dump] last unreached pin {sink} expanded {expanded} extent {extent:?}"
            );
        }
        let (_, start) = self.graph.point(plans[&pin].top);
        let radius = self.input.step.saturating_mul(DUMP_RADIUS_STEPS);
        let window = Rect {
            lx: start[0].saturating_sub(radius),
            ly: start[1].saturating_sub(radius),
            ux: start[0].saturating_add(radius),
            uy: start[1].saturating_add(radius),
        };
        let mut left = DUMP_SHAPES;
        for layer in 0..self.input.layers.len() as u32 {
            for (id, b, routed) in self.spatial.shapes_in(layer, window, left)? {
                left -= 1;
                let kind = match id {
                    None => "instance",
                    Some(id) if id < self.input.obstacles.len() => "input",
                    Some(_) if routed => "routed",
                    Some(_) => "fixed",
                };
                eprintln!(
                    "[troute-dump] shape layer {layer} {kind} id {id:?} net {:?} rect [{}, {}, {}, {}]",
                    b.net, b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy
                );
            }
            if left == 0 {
                eprintln!("[troute-dump] shape cap {DUMP_SHAPES} reached");
                break;
            }
        }
        Ok(())
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
            for e in &tables[&key] {
                let p = [
                    int(i128::from(e.at[0]) + i128::from(origin[0]))?,
                    int(i128::from(e.at[1]) + i128::from(origin[1]))?,
                ];
                if allowed.contains(&(e.layer, p)) && contains(self.input.die, p) {
                    let cost = if e.fits { 0 } else { step };
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

    /// Does a legal escape move of terminal `me` encroach on an open landing
    /// zone of another terminal (`Landings`)?
    #[allow(clippy::too_many_arguments)]
    fn encroaches(
        &mut self,
        landings: &Landings,
        me: usize,
        net: u32,
        kind: u32,
        layer: u32,
        from: [i64; 2],
        to: [i64; 2],
    ) -> Result<bool> {
        let cap = self.limits.max_comparisons;
        if kind == WIRE {
            let rect = wire_rect(
                &Wire {
                    net,
                    layer,
                    start: from,
                    end: to,
                },
                self.width(layer),
            )?;
            let spacing = self.input.layers[layer as usize].spacing;
            return landings.encroached(me, net, layer, rect, spacing, &mut self.spatial.work, cap);
        }
        for shape in &self.input.vias[kind as usize].shapes {
            let rule = &self.input.layers[shape.layer as usize];
            if rule.vertical.is_some()
                && landings.encroached(
                    me,
                    net,
                    shape.layer,
                    shifted(shape.rect, from)?,
                    rule.spacing,
                    &mut self.spatial.work,
                    cap,
                )?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One terminal escape search; None when no legal escape exists.
    /// `landings` (alternate access only) prices moves that encroach on the
    /// landing zones of terminals that have no escape yet.
    fn escape(
        &mut self,
        t: &Terminal,
        tables: &mut AccessTables,
        preference: AccessPreference,
        landings: Option<(&Landings, usize)>,
    ) -> Result<Option<Plan>> {
        let input = self.input;
        let mut starts = self.starts(t, tables, true)?;
        if starts.is_empty() {
            return Ok(None);
        }
        // Duplicate seeds otherwise make "next choice" retry the same point.
        // Preserve the cheapest existing seed at each point. Initial search
        // still uses its original costs and node-id tie breaking.
        self.spatial.work = self
            .spatial
            .work
            .checked_add(
                starts
                    .len()
                    .saturating_mul(starts.len().ilog2() as usize + 2),
            )
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("access seed ordering work"))?;
        starts.sort_unstable_by_key(|s| (s.0, s.1, s.2));
        starts.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
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
        let mut visited = HashMap::<u32, (u64, u32, u32)>::new();
        for (index, (layer, at, base_cost, _)) in starts.iter().enumerate() {
            let rank = if preference.next_start == usize::MAX {
                0
            } else {
                (index + starts.len() - preference.next_start % starts.len()) % starts.len()
            };
            let cost = (rank as u64)
                .checked_mul(input.step as u64)
                .and_then(|n| n.checked_add(*base_cost))
                .ok_or_else(|| budget("access seed preference cost"))?;
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
                if visited.get(&id).is_none_or(|v| v.0 > cost) {
                    visited.insert(id, (cost, NONE, WIRE));
                    heap.push(Reverse((
                        cost.saturating_add(potential.cost(id)?),
                        cost,
                        id,
                    )));
                }
            }
        }
        let mut found = None;
        let mut search = 0usize;
        while let Some(Reverse((_, cost, id))) = heap.pop() {
            if visited[&id].0 != cost {
                continue;
            }
            let (li, at) = local.at(id);
            if slots[li] == goal_slot && self.graph.find(goal_slot, at).is_some() {
                let needs_portal = !above && self.graph.pairs.iter().any(|p| p.lower == goal_slot);
                let incoming = visited[&id].2;
                let enough = incoming == WIRE
                    || self.via_pad_ok[self.graph.layers[goal_slot].layer as usize]
                        [incoming as usize];
                let portal = if needs_portal {
                    self.access_portal(goal_slot, at, t.net, enough)?
                } else {
                    None
                };
                if !needs_portal || portal.is_some() {
                    found = Some((id, portal));
                    break;
                }
            }
            self.tick(&mut search, "pin access", t.pin, t.net)?;
            let layer = self.graph.layers[slots[li]].layer;
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
                    distance(at, q)?
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
                    if !listed.is_empty() && preference.generation != 0 {
                        let shift = preference.generation % listed.len();
                        listed.rotate_left(shift);
                    }
                    let mut chosen = None;
                    'kinds: for k in listed {
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
                    if !self.spatial.clear(layer, rect, t.net)? {
                        continue;
                    }
                    WIRE
                };
                let nc = match landings {
                    Some((zones, me))
                        if self.encroaches(zones, me, t.net, kind, layer, at, q)? =>
                    {
                        let nc = (input.step as u64)
                            .checked_mul(LANDING_PENALTY_STEPS)
                            .and_then(|p| nc.checked_add(p))
                            .ok_or_else(|| budget("access landing cost overflow"))?;
                        if visited.get(&nid).is_some_and(|v| v.0 <= nc) {
                            continue;
                        }
                        nc
                    }
                    _ => nc,
                };
                if heap.len() >= self.limits.max_heap || visited.len() >= self.limits.max_grid_nodes
                {
                    return Err(budget("pin access frontier"));
                }
                visited.insert(nid, (nc, id, kind));
                heap.push(Reverse((nc.saturating_add(potential.cost(nid)?), nc, nid)));
            }
        }
        let Some((top, portal)) = found else {
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
        self.spatial.work = self
            .spatial
            .work
            .checked_add(starts.len())
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("access seed identity work"))?;
        let source_index = starts
            .iter()
            .position(|s| s.0 == access_layer && s.1 == at)
            .ok_or_else(|| invalid("access traceback did not reach a seed"))?;
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
        Ok(Some(Plan {
            access: Access {
                pin: t.pin,
                net: t.net,
                layer: access_layer,
                at,
            },
            source_index,
            generation: preference.generation,
            top: top_node,
            top_via,
            portal_shapes,
            wires: wires.into_boxed_slice(),
            vias: vias.into_boxed_slice(),
            shapes: shapes.into_boxed_slice(),
        }))
    }

    fn access_failure(&self, blocked: Vec<crate::BlockedPin>) -> crate::Error {
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            for pin in blocked.iter().take(8) {
                eprintln!(
                    "[troute] blocked access pin {} net {} region {:?} (first8 of {})",
                    pin.pin,
                    pin.net,
                    pin.region,
                    blocked.len()
                );
            }
        }
        crate::Error::PinAccess(Box::new(crate::PinAccessFailure {
            blocked: blocked.into_boxed_slice(),
            comparisons: self.spatial.work,
            expansions: self.expansions,
        }))
    }

    /// Landing zone (see `Landings`) and its layer for a terminal below the
    /// access layer: every first-via kind the escape may try at a start on
    /// the lowest pin layer (table kinds, else the pair's kinds), unfiltered
    /// by current clearance so it covers the landings a later retry can use.
    fn landing_zone(
        &mut self,
        t: &Terminal,
        tables: &mut AccessTables,
    ) -> Result<Option<(Rect, u32)>> {
        let access_slot = self.graph.slot[&self.input.access_layer];
        let starts = self.starts(t, tables, true)?;
        let Some(slot) = starts
            .iter()
            .filter_map(|s| self.graph.slot.get(&s.0).copied())
            .min()
            .filter(|&s| s < access_slot)
        else {
            return Ok(None);
        };
        let pin_layer = self.graph.layers[slot].layer;
        let Some(pair) = self.pairs.iter().find(|p| {
            p.lower == pin_layer
                && self
                    .graph
                    .slot
                    .get(&p.upper)
                    .is_some_and(|&s| s <= access_slot)
        }) else {
            return Ok(None);
        };
        let mut zone: Option<Rect> = None;
        let mut work = 0usize;
        for (layer, at, _, kinds) in &starts {
            if *layer != pin_layer {
                continue;
            }
            for &k in kinds.as_deref().unwrap_or(&pair.kinds) {
                if !pair.kinds.contains(&k) {
                    continue;
                }
                for shape in &self.input.vias[k].shapes {
                    work += 1;
                    if shape.layer != pair.upper {
                        continue;
                    }
                    let r = shifted(shape.rect, *at)?;
                    zone = Some(zone.map_or(r, |z| Rect {
                        lx: z.lx.min(r.lx),
                        ly: z.ly.min(r.ly),
                        ux: z.ux.max(r.ux),
                        uy: z.uy.max(r.uy),
                    }));
                }
            }
        }
        self.spatial.work = self
            .spatial
            .work
            .checked_add(work.saturating_add(starts.len()))
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("landing-zone work"))?;
        Ok(zone.map(|z| (z, pair.upper)))
    }

    /// Conflict-aware escapes for every terminal of a routed net.
    fn reserve(
        &mut self,
        terminals: &[&'a Terminal],
        alternatives: bool,
    ) -> Result<HashMap<u32, Plan>> {
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
        let mut landings = alternatives.then(Landings::default);
        let mut queue: VecDeque<usize> = order.into_iter().map(|o| o.2).collect();
        let mut plans: HashMap<u32, Plan> = HashMap::new();
        let mut plan_records = 0usize;
        let mut owner: BTreeMap<u32, usize> = BTreeMap::new();
        let mut attempts = vec![0usize; terminals.len()];
        let mut preferences = vec![AccessPreference::default(); terminals.len()];
        let mut rounds = 0usize;
        let mut window_index = None;
        let mut blocked = Vec::new();
        let stack = access_stack(self.input);
        while let Some(i) = queue.pop_front() {
            rounds += 1;
            if rounds > terminals.len().saturating_mul(8) + 64 {
                return Err(budget("pin access repair rounds"));
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
            let zones = landings.as_ref().filter(|l| l.open > 0).map(|l| (l, i));
            if let Some(plan) = self.escape(t, &mut tables, preferences[i], zones)? {
                plan_records = plan_records
                    .checked_add(plan.wires.len())
                    .and_then(|n| n.checked_add(plan.vias.len()))
                    .filter(|&n| n <= self.limits.max_segments)
                    .ok_or_else(|| budget("pin access retained-record limit"))?;
                plans.insert(t.pin, plan);
                owner.insert(t.pin, i);
                if let Some(l) = landings.as_mut() {
                    l.set_open(i, false);
                }
                if plans.len().is_multiple_of(4096) {
                    self.guard("pin access")?;
                }
                continue;
            }
            self.access_failures += 1;
            attempts[i] += 1;
            if let Some(l) = landings.as_mut() {
                if let Some((zone, layer)) = self.landing_zone(t, &mut tables)? {
                    l.insert(i, t.net, layer, zone)?;
                }
                l.set_open(i, true);
            }
            if attempts[i] > 3 {
                blocked
                    .try_reserve(1)
                    .map_err(|_| budget("blocked pin records"))?;
                blocked.push(crate::BlockedPin {
                    pin: t.pin,
                    net: t.net,
                    region: windows[i],
                    layers: escape_layers(stack, t),
                });
                if blocked.len() >= self.limits.max_batch_blocked_pins {
                    return Err(self.access_failure(blocked));
                }
                continue;
            }
            // Rip up escapes of terminals whose windows overlap this one.
            if window_index.is_none() {
                window_index = Some(WindowIndex::new(
                    &windows,
                    &mut self.spatial.work,
                    self.limits.max_comparisons,
                )?);
            }
            let mut victims: Vec<u32> = window_index
                .as_ref()
                .unwrap()
                .overlapping(
                    &windows,
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
                blocked
                    .try_reserve(1)
                    .map_err(|_| budget("blocked pin records"))?;
                blocked.push(crate::BlockedPin {
                    pin: t.pin,
                    net: t.net,
                    region: windows[i],
                    layers: escape_layers(stack, t),
                });
                if blocked.len() >= self.limits.max_batch_blocked_pins {
                    return Err(self.access_failure(blocked));
                }
                continue;
            }
            let mut back = Vec::new();
            for p in victims {
                let plan = plans.remove(&p).unwrap();
                let j = owner.remove(&p).unwrap();
                if let Some(l) = landings.as_mut() {
                    l.set_open(j, true);
                }
                if alternatives {
                    preferences[j] = AccessPreference {
                        next_start: plan
                            .source_index
                            .checked_add(1)
                            .ok_or_else(|| budget("access seed index"))?,
                        generation: plan
                            .generation
                            .checked_add(1)
                            .ok_or_else(|| budget("access repair generation"))?,
                    };
                }
                plan_records -= plan.wires.len() + plan.vias.len();
                for id in plan.shapes {
                    self.spatial.remove(id);
                }
                back.push(j);
            }
            // Rotate the victims so repeated repairs try different orders.
            let rot = attempts[i] % back.len().max(1);
            back.rotate_left(rot);
            for j in back.into_iter().rev() {
                queue.push_front(j);
            }
            queue.push_front(i);
        }
        if blocked.is_empty() {
            Ok(plans)
        } else {
            Err(self.access_failure(blocked))
        }
    }

    // ---------------------------------------------------------------- global

    /// Per-edge track capacity against fixed geometry (net-less probes).
    fn capacities(&mut self) -> Result<Vec<u32>> {
        let c = &self.cells;
        let mut cap = vec![0u32; c.edges()];
        let (nx, ny, g, lx, ly) = (c.nx, c.ny, c.g, c.lx, c.ly);
        for l in 0..self.graph.layers.len() {
            let layer = self.graph.layers[l].layer;
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
                    let rect = if vertical {
                        Rect {
                            lx: tc - half,
                            ux: tc + half,
                            ly: along - half,
                            uy: along + half,
                        }
                    } else {
                        Rect {
                            lx: along - half,
                            ux: along + half,
                            ly: tc - half,
                            uy: tc + half,
                        }
                    };
                    if !self.spatial.clear(layer, rect, NONE)? {
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
        let mut forbidden: Vec<ForbiddenVia> = Vec::new();
        let mut edges = BTreeMap::new();
        let root = plans[&terminals[0].pin].top;
        let (root_layer, root_point) = self.graph.point(root);
        let mut target = crate::routing_target::TreeDistance::new(
            self.graph.slot[&root_layer],
            root_point,
            self.limits.max_target_segments,
        )?;
        let mut tree: HashSet<u32> = HashSet::from([root]);
        // Tree junctions remember whether an actual incident pad meets AREA.
        let mut via_nodes: HashMap<u32, bool> = HashMap::new();
        if plans[&terminals[0].pin].top_via {
            via_nodes.insert(root, self.escape_pad_ok(&plans[&terminals[0].pin]));
        }
        let mut victims_all: BTreeSet<u32> = BTreeSet::new();
        let via_cost = self.plan.via_cost;
        let wrong = self.plan.wrong_way_factor;
        // Each crossed net is charged by its own rip-up history below. Neither
        // unrelated repairs nor this net's attempts multiply that penalty again.
        let conflict = via_cost.saturating_mul(10);
        let near_cost = via_cost;
        let hist_unit = via_cost / 2;
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
        let own = crate::own_pins::OwnPins::new(
            own,
            self.input.layers.len(),
            &mut self.spatial.work,
            self.limits.max_comparisons,
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
            loop {
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
                heap.offer((
                    target
                        .distance(
                            self.graph.slot[&self.graph.point(start).0],
                            self.graph.point(start).1,
                            &self.target_costs,
                            &mut self.spatial.work,
                            self.limits.max_comparisons,
                        )?
                        .saturating_mul(self.plan.heuristic_weight),
                    Reverse(0u64),
                    start,
                ))?;
                let mut found = None;
                let mut limited = false;
                let mut projection_attempts = 0;
                let mut search = 0usize;
                let mut extent = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
                while let Some((priority, Reverse(cost), id)) = heap.pop() {
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
                    if !soft
                        && search
                            >= self
                                .limits
                                .max_hard_search_expansions
                                .unwrap_or(self.limits.max_search_expansions)
                    {
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                            eprintln!(
                                "[troute] deferred hard search net {net} pin {} expansions {search}",
                                t.pin
                            );
                        }
                        break;
                    }
                    if search == self.limits.max_search_expansions {
                        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
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
                    let (l, t_, s_) = self.graph.locate(id);
                    let layer = self.graph.layers[l].layer;
                    let wire_cost = self.wire_cost[layer as usize];
                    let p = self.graph.xy(l, t_, s_);
                    extent = [
                        extent[0].min(p[0]),
                        extent[1].min(p[1]),
                        extent[2].max(p[0]),
                        extent[3].max(p[1]),
                    ];
                    let target_distance = target.bounds_distance(p);
                    let interior_distance = if target_distance == 0 {
                        // During this search the tree is fixed. A non-saturated
                        // queue score retains exactly g + weight*h, so reuse h.
                        Some(if priority != u64::MAX {
                            (priority - cost) / self.plan.heuristic_weight
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
                    let no_stack = strict && !self.stack_ok[layer as usize] && {
                        let need = self.min_run[layer as usize];
                        let mut run = 0u64;
                        let mut cur = id;
                        let mut steps = 0usize;
                        loop {
                            if cur == start {
                                break start_via && !start_pad_ok && run < need;
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
                        let (nl, q) = self.graph.point(next);
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
                            continue;
                        }
                        let mut step = match mv {
                            Move::Along => distance(p, q)?.saturating_mul(wire_cost) / 100,
                            Move::Wrong => {
                                distance(p, q)?
                                    .saturating_mul(wrong)
                                    .saturating_mul(wire_cost)
                                    / 100
                            }
                            Move::Via(_) => via_cost,
                        };
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
                            self.spatial.work = self
                                .spatial
                                .work
                                .checked_add(cell_count + if inspect_nodes { jump } else { 0 })
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
                                    usable = false;
                                    break;
                                }
                            }
                            if tree.len() == 1 && id.min(next) < root && root < id.max(next) {
                                usable = false;
                            }
                            if usable && inspect_nodes {
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
                                // A wire that ends here on this layer wants the
                                // pad elongated along it (end-of-line enclosure).
                                let original = self.graph.pairs[pi].kinds.clone();
                                let mut kinds = Vec::with_capacity(original.len());
                                for kind in original {
                                    if !forbidden.is_empty() {
                                        self.spatial.work = self
                                            .spatial
                                            .work
                                            .checked_add(forbidden.len().ilog2() as usize + 1)
                                            .filter(|&n| n <= self.limits.max_comparisons)
                                            .ok_or_else(|| {
                                                budget("path via-conflict lookup work")
                                            })?;
                                        if forbidden
                                            .binary_search(&(
                                                id.min(next),
                                                id.max(next),
                                                kind as u32,
                                            ))
                                            .is_ok()
                                        {
                                            continue;
                                        }
                                    }
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
                                            let c = victims
                                                .iter()
                                                .map(|v| {
                                                    conflict
                                                        * (1 + rips.get(v).copied().unwrap_or(0)
                                                            as u64)
                                                })
                                                .sum::<u64>()
                                                + if near { near_cost } else { 0 };
                                            if strict && near {
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
                                } else if strict {
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
                                        Soft::Clear { near: true, .. } if strict => continue,
                                        Soft::Clear { near, victims } => (
                                            WIRE,
                                            victims
                                                .iter()
                                                .map(|v| {
                                                    conflict
                                                        * (1 + rips.get(v).copied().unwrap_or(0)
                                                            as u64)
                                                })
                                                .sum::<u64>()
                                                + if near { near_cost } else { 0 },
                                        ),
                                    }
                                } else {
                                    match self.spatial.probe(layer, rect, net)? {
                                        None => continue,
                                        Some(true) if strict => continue,
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
                        heap.offer((
                            nc.saturating_add(remaining.saturating_mul(self.plan.heuristic_weight)),
                            Reverse(nc),
                            next,
                        ))
                        .map_err(|e| budget(&format!("{e}; pin {} net {net}", t.pin)))?;
                    }
                }
                let Some(mut current) = found else {
                    for id in out.shapes.drain(..) {
                        self.spatial.remove(id);
                    }
                    self.last_unreached = Some((t.pin, search, extent));
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
                    if state.kind == WIRE && a.0 == b.0 && a.1 == b.1 && prev.abs_diff(current) > 1
                    {
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
                if let Some(edge) = trace_cut_conflict(
                    self.input,
                    &self.graph,
                    &path,
                    &mut self.spatial.work,
                    self.limits,
                )? {
                    if forbidden.len() >= self.limits.max_path_retries {
                        return Err(budget(&format!(
                            "path via-conflict retry limit: pin {} net {net}",
                            t.pin
                        )));
                    }
                    self.guard("path via-conflict retry")?;
                    if forbidden.is_empty() {
                        forbidden
                            .try_reserve_exact(self.limits.max_path_retries)
                            .map_err(|_| budget("path retry allocation"))?;
                    }
                    self.spatial.work = self
                        .spatial
                        .work
                        .checked_add(forbidden.len().saturating_mul(2).saturating_add(1))
                        .filter(|&n| n <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("path via-conflict insertion work"))?;
                    let at = forbidden
                        .binary_search(&edge)
                        .err()
                        .ok_or_else(|| invalid("forbidden via reappeared in traceback"))?;
                    forbidden.insert(at, edge);
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        eprintln!(
                            "[troute] bounded via-conflict retry net {net} pin {} edge {edge:?} attempt {}",
                            t.pin,
                            forbidden.len()
                        );
                    }
                    continue;
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
                    let new_nodes =
                        usize::from(!tree.contains(&a)) + usize::from(!tree.contains(&b));
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
                            self.history.increment(b)?;
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
                                        self.history.increment(b)?;
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
                break;
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

/// Track-router entry point (called by `routing::route_cpu_ref`).
pub(crate) fn route(
    input: &RouteInput,
    limits: &RouteLimits,
    access_alternatives: bool,
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
    // One fixed index serves the router, every cleanup pass and validation.
    let fixed = crate::routing::FixedIndex::new(input, limits)?;
    let mut r = Router::new(
        input,
        plan,
        limits,
        Spatial::shared(&fixed, limits, router_entries(input, limits)),
        graph,
        cells,
    )?;
    trace("spatial index", t0, format_args!("work {}", r.spatial.work));
    let all: Vec<&Terminal> = groups.values().flatten().copied().collect();
    let plans = r.reserve(&all, access_alternatives)?;
    r.guard("pin access")?;
    trace(
        "access",
        t0,
        format_args!(
            "terminals {} work {} exp {} failed {}",
            all.len(),
            r.spatial.work,
            r.expansions,
            r.access_failures
        ),
    );
    drop(all);
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
    // Detailed routing: small nets first.
    let mut order: Vec<(usize, u32)> = guides.iter().map(|(n, g)| (g.len(), *n)).collect();
    order.sort_unstable();
    let mut routed: BTreeMap<u32, NetRoute> = BTreeMap::new();
    let mut retained_edges = 0usize;
    let mut queue: VecDeque<u32> = VecDeque::new();
    let none = BTreeMap::new();
    for (index, &(_, net)) in order.iter().enumerate() {
        if index.is_multiple_of(10_000) {
            trace(
                "detailed progress",
                t0,
                format_args!(
                    "nets {index}/{} failed {} work {} exp {}",
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
            r.mark_region(g, expand_by);
            if let Outcome::Routed(route, _) =
                r.route_net(net, &groups[&net], &plans, false, false, &none, 0)?
            {
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
    let mut rule_reservations = BTreeSet::new();
    let reservation_limit = limits
        .max_segments
        .min(input.terminals.len().saturating_mul(2));
    let via_layers: Vec<(u32, u32)> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
    for pass in 0..=4usize {
        // Repair: negotiated rip-up and reroute.
        while let Some(net) = queue.pop_front() {
            round += 1;
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
            let g = &guides[&net];
            let tight = strict.contains(&net);
            r.mark_region(g, Some(r.plan.expand_margin));
            if let Outcome::Routed(route, _) =
                r.route_net(net, &groups[&net], &plans, false, tight, &rips, round)?
            {
                retain_route(
                    &mut routed,
                    &mut retained_edges,
                    net,
                    route,
                    limits.max_segments,
                )?;
                continue;
            }
            let wide = r
                .plan
                .expand_margin
                .saturating_mul(2)
                .saturating_add(rips.get(&net).copied().unwrap_or(0).min(16));
            r.mark_region(g, Some(wide));
            let mut outcome = r.route_net(net, &groups[&net], &plans, true, tight, &rips, round)?;
            let mut conservative = tight;
            if matches!(outcome, Outcome::Limited(_) | Outcome::Failed(_))
                && alternate_repair_root(
                    groups.get_mut(&net).unwrap(),
                    |t| r.graph.point(plans[&t.pin].top).1,
                    &mut r.spatial.work,
                    limits.max_comparisons,
                )?
            {
                if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                    eprintln!("[troute] bounded alternate-root retry net {net}");
                }
                outcome = r.route_net(net, &groups[&net], &plans, true, tight, &rips, round)?;
            }
            // Strict min-area/near-miss pruning is path-dependent and can hide
            // a route in this one-cost-per-node search. Exact cleanup remains
            // mandatory for a route found by the final bounded attempt.
            if tight && matches!(outcome, Outcome::Failed(_)) {
                r.guard("cleanup-validated retry")?;
                if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                    eprintln!("[troute] bounded cleanup-validated retry net {net}");
                }
                conservative = false;
                outcome = r.route_net(net, &groups[&net], &plans, true, false, &rips, round)?;
            }
            match outcome {
                Outcome::Limited(pin) => {
                    return Err(budget(&format!(
                        "per-search detailed limit after bounded root retries, pin {pin} net {net}"
                    )));
                }
                Outcome::Failed(pin) => {
                    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                        let group = &groups[&net];
                        r.spatial.work = r
                            .spatial
                            .work
                            .checked_add(group.len().min(64))
                            .and_then(|n| n.checked_add(rule_reservations.len()))
                            .filter(|&n| n <= limits.max_comparisons)
                            .ok_or_else(|| budget("failed-route diagnostic work"))?;
                        let points: Vec<_> = group
                            .iter()
                            .take(64)
                            .map(|t| (t.pin, t.driver, r.graph.point(plans[&t.pin].top)))
                            .collect();
                        eprintln!(
                            "[troute] no-path context net {net} pin {pin} strict {conservative} terminals {} first64 {points:?}",
                            group.len()
                        );
                        let (_, start) = r.graph.point(plans[&pin].top);
                        let radius = input.step.saturating_mul(8);
                        for key in rule_reservations
                            .iter()
                            .filter(|&&(_, _, lx, ly, ux, uy)| {
                                lx <= start[0].saturating_add(radius)
                                    && ux >= start[0].saturating_sub(radius)
                                    && ly <= start[1].saturating_add(radius)
                                    && uy >= start[1].saturating_sub(radius)
                            })
                            .take(16)
                        {
                            eprintln!("[troute] no-path nearby reservation {key:?}");
                        }
                        r.dump_no_path(net, pin, group, &plans, g, wide)?;
                    }
                    return Err(r.no_path_failure(net, pin, &groups[&net])?);
                }
                Outcome::Routed(route, victims) => {
                    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
                        eprintln!(
                            "[troute] round {round} net {net} soft victims {:?} work {} exp {}",
                            victims, r.spatial.work, r.expansions
                        );
                    }
                    retain_route(
                        &mut routed,
                        &mut retained_edges,
                        net,
                        route,
                        limits.max_segments,
                    )?;
                    for v in victims {
                        let c = rips.entry(v).or_insert(0);
                        *c += 1;
                        if *c > r.plan.max_ripups_per_net {
                            return Err(no_route(&format!(
                                "net {v} ripped up more than {} times",
                                r.plan.max_ripups_per_net
                            )));
                        }
                        if let Some(old) = remove_route(&mut routed, &mut retained_edges, v) {
                            for id in old.shapes {
                                r.spatial.remove(id);
                            }
                        }
                        if !queue.contains(&v) {
                            queue.push_back(v);
                        }
                    }
                }
            }
        }
        trace(
            "repair",
            t0,
            format_args!(
                "pass {pass} rounds {round} rips {} work {} exp {}",
                rips.values().sum::<usize>(),
                r.spatial.work,
                r.expansions
            ),
        );
        r.guard("repair")?;
        let oriented = r.orient_vias(&mut routed)?;
        trace("orient", t0, format_args!("vias re-picked {oriented}"));
        let mut out = assemble(&r, &groups, &plans, &routed, input, limits, &via_layers)?;
        r.spatial.work = out.stats.comparisons;
        let mut cleanup_limits = limits.clone();
        cleanup_limits.max_comparisons = limits
            .max_comparisons
            .checked_sub(r.spatial.work)
            .filter(|&n| n > 0)
            .ok_or_else(|| budget("routing comparison limit before cleanup"))?;
        let protected = rule_reservations
            .iter()
            .map(|&(net, layer, lx, ly, ux, uy)| Blockage {
                net: Some(net),
                layer,
                rect: Rect { lx, ly, ux, uy },
            });
        let (work, remaining) =
            crate::routing::cleanup_shared(&fixed, &mut out, &cleanup_limits, protected)?;
        r.spatial.work = r
            .spatial
            .work
            .checked_add(work)
            .filter(|&n| n <= limits.max_comparisons)
            .ok_or_else(|| budget("routing comparison limit"))?;
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
        out.stats.comparisons = r.spatial.work;
        if remaining.is_empty() {
            if out.stats.comparisons > limits.max_comparisons {
                return Err(budget("routing comparison limit"));
            }
            out.stats.wire_length_dbu = out.wires.iter().try_fold(0u64, |n, w| {
                n.checked_add(distance(w.start, w.end)?)
                    .ok_or_else(|| budget("total wire length"))
            })?;
            crate::routing::validate_shared(&fixed, &out, limits)?;
            r.guard("validation")?;
            let fp = r.spatial.footprint();
            let [fixed_bins, fixed_entries, added_bins, added_entries] =
                r.spatial.index_footprint();
            trace(
                "validated",
                t0,
                format_args!(
                    "records {} wires {} vias {} access {} patches {} bins {} entries {} shapes {} bin {} obstacles {} terminals {} die {:?} layers {} fixed_bins {fixed_bins} fixed_entries {fixed_entries} added_bins {added_bins} added_entries {added_entries}",
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
        if pass == 4 {
            let v = &remaining[0];
            let detail = format!("unresolved {} ({} total)", v.describe(), remaining.len());
            r.spatial.work = r
                .spatial
                .work
                .checked_add(remaining.len())
                .filter(|&n| n <= limits.max_comparisons)
                .ok_or_else(|| budget("routing conflict diagnostic work"))?;
            if remaining.iter().all(|v| {
                matches!(
                    v,
                    route_rules::Violation::Spacing { .. }
                        | route_rules::Violation::Area { .. }
                        | route_rules::Violation::Hole { .. }
                )
            }) {
                if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
                    for conflict in remaining.iter().take(8) {
                        if let route_rules::Violation::Spacing { net, layer, a, b } = conflict {
                            let fill = route_rules::fill_for_robust(
                                *a,
                                *b,
                                input.layers[*layer as usize].width,
                                input.grid,
                            );
                            match r.spatial.probe_soft(*layer, fill, *net)? {
                                Soft::Blocked => eprintln!(
                                    "[troute] rule search context {} fill {fill:?} fixed blocker",
                                    conflict.describe()
                                ),
                                Soft::Clear { near, victims } => eprintln!(
                                    "[troute] rule search context {} fill {fill:?} near {near} routed blockers {:?} ({} total)",
                                    conflict.describe(),
                                    &victims[..victims.len().min(8)],
                                    victims.len()
                                ),
                            }
                        } else {
                            eprintln!("[troute] rule search context {}", conflict.describe());
                        }
                    }
                }
                let count = remaining.len().min(limits.max_batch_blocked_pins);
                let mut regions = Vec::new();
                regions
                    .try_reserve_exact(count)
                    .map_err(|_| budget("routing conflict regions"))?;
                for v in remaining.iter().take(count) {
                    let region = match v {
                        route_rules::Violation::Spacing { a, b, .. } => Rect {
                            lx: a.lx.min(b.lx),
                            ly: a.ly.min(b.ly),
                            ux: a.ux.max(b.ux),
                            uy: a.uy.max(b.uy),
                        },
                        route_rules::Violation::Area { shapes, .. } => {
                            r.spatial.work = r
                                .spatial
                                .work
                                .checked_add(shapes.len())
                                .filter(|&n| n <= limits.max_comparisons)
                                .ok_or_else(|| budget("area recovery region work"))?;
                            let first = *shapes
                                .first()
                                .ok_or_else(|| invalid("empty area violation"))?;
                            shapes[1..].iter().fold(first, |a, b| Rect {
                                lx: a.lx.min(b.lx),
                                ly: a.ly.min(b.ly),
                                ux: a.ux.max(b.ux),
                                uy: a.uy.max(b.uy),
                            })
                        }
                        route_rules::Violation::Hole { rect, .. } => *rect,
                        route_rules::Violation::Floating { .. } => {
                            unreachable!("repairable violations checked above")
                        }
                    };
                    regions.push(region);
                }
                return Err(crate::Error::RoutingRule(Box::new(
                    crate::RoutingRuleFailure {
                        detail,
                        regions: regions.into_boxed_slice(),
                        comparisons: r.spatial.work,
                        expansions: r.expansions,
                    },
                )));
            }
            return Err(no_route(&format!("ROUTING_RULE {detail}")));
        }
        // A nearby active route can block a legal same-net fill. Protect that
        // fill before rerouting its blockers, otherwise they choose it again.
        let reserved_before = rule_reservations.len();
        let victims = reserve_rule_clearance(
            input,
            limits,
            &mut r.spatial,
            &remaining,
            &mut rule_reservations,
            reservation_limit,
        )?;
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() && !victims.is_empty() {
            eprintln!(
                "[troute] rule clearance reserved {} fills; reroute {} blocking nets",
                rule_reservations.len() - reserved_before,
                victims.len()
            );
        }
        for net in victims {
            let count = rips.entry(net).or_default();
            *count += 1;
            if *count > r.plan.max_ripups_per_net {
                return Err(no_route(&format!(
                    "net {net} exceeded the rip-up limit during rule repair"
                )));
            }
            let old = remove_route(&mut routed, &mut retained_edges, net)
                .ok_or_else(|| invalid("rule blocker has no retained route"))?;
            for id in old.shapes {
                r.spatial.remove(id);
            }
            if !queue.contains(&net) {
                queue.push_back(net);
            }
        }
        r.guard("rule-clearance reservations")?;
        // Re-route the offending nets in strict mode (no near misses).
        let bad: BTreeSet<u32> = remaining.iter().map(|v| v.net()).collect();
        for net in bad {
            if let Some(old) = remove_route(&mut routed, &mut retained_edges, net) {
                for id in old.shapes {
                    r.spatial.remove(id);
                }
            }
            strict.insert(net);
            if !queue.contains(&net) {
                queue.push_back(net);
            }
        }
    }
    unreachable!("bounded repair passes return")
}

/// Records of the current routing state (escapes plus routed edges), wires
/// merged into maximal collinear runs split at junctions.
/// Is `r` wholly covered by the union of `by` (rectilinear, coordinate
/// compression; `by` is one pin's few rectangles)?
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
    let work_cap = limits
        .max_comparisons
        .checked_sub(r.spatial.work)
        .filter(|&n| n > 0)
        .ok_or_else(|| budget("routing assembly work"))?;
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
            work_cap,
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
            comparisons: r
                .spatial
                .work
                .checked_add(work)
                .filter(|&n| n <= limits.max_comparisons)
                .ok_or_else(|| budget("routing assembly work"))?,
            wire_length_dbu: 0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::validate;

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
            gcell: 200,
            global_iterations: 10,
            capacity_percent: 80,
            guide_margin: 1,
            expand_margin: 4,
            max_ripups_per_net: 8,
            max_repair_rounds: 1000,
            via_cost: 80,
            wrong_way_factor: 4,
            wire_cost_percent: Vec::new(),
            heuristic_weight: 1,
            memory_mib: 64,
            max_history_entries: crate::routing::DEFAULT_MAX_HISTORY_ENTRIES,
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

    #[test]
    fn layer_wire_costs_move_long_runs_to_cheaper_layers() {
        let run = |percent: Vec<u32>| {
            let mut i = input(
                vec![
                    terminal(0, 0, true, rect(90, 490, 110, 510)),
                    terminal(1, 0, false, rect(890, 490, 910, 510)),
                ],
                Vec::new(),
            );
            i.tracks.as_mut().unwrap().wire_cost_percent = percent;
            let routes = route_cpu_ref(&i, &limits())?;
            let mut length = [0i64; 3];
            for w in &routes.wires {
                length[w.layer as usize] +=
                    (w.end[0] - w.start[0]).abs() + (w.end[1] - w.start[1]).abs();
            }
            Ok::<_, crate::Error>((length, format!("{:?} {:?}", routes.wires, routes.vias)))
        };
        let (uniform, geometry) = run(Vec::new()).unwrap();
        assert!(uniform[0] + uniform[2] >= 700, "{uniform:?}");
        // An explicit all-100 table is the uniform plan, record for record.
        assert_eq!(run(vec![100; 5]).unwrap().1, geometry);
        let (m1, _) = run(vec![300, 100, 100, 100, 100]).unwrap();
        assert!(m1[0] < 100 && m1[2] >= 700, "M1 at 300%: {m1:?}");
        let (m3, _) = run(vec![100, 100, 300, 100, 100]).unwrap();
        assert!(m3[2] < 100 && m3[0] >= 700, "M3 at 300%: {m3:?}");
        // Factors below 100 would break the lower bound; tables must be complete.
        for bad in [
            vec![99, 100, 100, 100, 100],
            vec![1001, 100, 100, 100, 100],
            vec![200; 4],
        ] {
            assert!(run(bad).is_err());
        }
    }

    /// The track fixture plus M4 (vertical) and its V34 cut above M3.
    fn four_metal(terminals: Vec<Terminal>, obstacles: Vec<Blockage>) -> RouteInput {
        let mut design = input(terminals, obstacles);
        design.layers.push(layer("M4", Some(true)));
        design.layers.push(layer("V3", None));
        design.vias.push(via("V34", 2, 6, 5));
        let plan = design.tracks.as_mut().unwrap();
        plan.tracks.push(Some((40, 20)));
        plan.tracks.push(None);
        design
    }

    #[test]
    fn access_stack_holds_layers_up_to_one_via_above_the_access_layer() {
        // M1 M2 M3 V1 V2: every layer can meet an escape that may end in V23.
        assert_eq!(access_stack(&input(Vec::new(), Vec::new())), 0b11111);
        let mut design = four_metal(Vec::new(), Vec::new());
        // M1 M2, the portal M3 and the V12/V23 cuts; not M4 (5) or V34 (6).
        assert_eq!(access_stack(&design), 0b0011111);
        design.access_layer = 0;
        assert_eq!(access_stack(&design), 0b0001011);
        design.access_layer = 2;
        assert_eq!(access_stack(&design), 0b1111111);
        design.access_layer = 1;
        let stack = access_stack(&design);
        let low = terminal(0, 0, true, rect(90, 90, 110, 110));
        assert_eq!(escape_layers(stack, &low), stack);
        let mut high = low.clone();
        high.shapes[0].layer = 5;
        assert_eq!(escape_layers(stack, &high), u64::MAX);
        design.vias[2].upper = 9;
        assert_eq!(access_stack(&design), u64::MAX, "invalid via layer");
    }

    #[test]
    fn pin_access_recovery_reopens_only_nets_on_escape_layers() {
        let terms = vec![
            terminal(0, 7, true, rect(490, 90, 510, 110)),
            terminal(1, 7, false, rect(490, 890, 510, 910)),
            terminal(2, 8, true, rect(450, 90, 470, 110)),
            terminal(3, 8, false, rect(450, 890, 470, 910)),
        ];
        let design = four_metal(terms, Vec::new());
        let limits = limits();
        let order = [0, 1, 2, 3];
        let batch = crate::route_batches::Batch {
            input: &design,
            limits: &limits,
            order: &order,
            begin: 4,
            cap: 1 << 40,
        };
        // Net 7 completed on M4 and net 8 on M2, both across the window of a
        // later pin whose escape is blocked.
        let window = rect(400, 400, 600, 600);
        let reopen = |layers: u64| {
            let mut view = four_metal(Vec::new(), Vec::new());
            let mut out = Routes {
                wires: Vec::new(),
                vias: Vec::new(),
                access: Vec::new(),
                patches: Vec::new(),
                nets: vec![7, 8],
                stats: RouteStats::default(),
            };
            for (net, layer, x) in [(7, 5, 500), (8, 1, 460)] {
                out.wires.push(Wire {
                    net,
                    layer,
                    start: [x, 100],
                    end: [x, 900],
                });
                view.obstacles.push(Blockage {
                    layer,
                    rect: rect(x - 10, 100, x + 10, 900),
                    net: Some(net),
                });
            }
            out.stats.wire_length_dbu = 1600;
            let reopened = batch.reopen(&mut view, &mut out, &[window], layers, 0)?;
            let kept: Vec<_> = view.obstacles.iter().map(|b| b.net).collect();
            let active: Vec<_> = view.terminals.iter().map(|t| t.net).collect();
            Ok::<_, crate::Error>((reopened, out.nets, kept, active))
        };
        assert_eq!(
            reopen(access_stack(&design)).unwrap(),
            (1, vec![7], vec![Some(7)], vec![8, 8])
        );
        // Negative control: the former any-layer reopen also displaces net 7.
        assert_eq!(
            reopen(u64::MAX).unwrap(),
            (2, Vec::new(), Vec::new(), vec![7, 7, 8, 8])
        );
    }

    #[test]
    fn coupled_pin_landings_need_bounded_alternatives() {
        let mut first = terminal(0, 0, true, rect(410, 290, 430, 310));
        first.shapes.push(LayerRect {
            layer: 0,
            rect: rect(295, 290, 315, 310),
        });
        let mut second = terminal(1, 1, true, rect(570, 290, 590, 310));
        second.shapes.push(LayerRect {
            layer: 0,
            rect: rect(695, 290, 715, 310),
        });
        let mut target_a = terminal(2, 0, false, rect(890, 90, 910, 110));
        let mut target_b = terminal(3, 1, false, rect(890, 890, 910, 910));
        target_a.shapes[0].layer = 2;
        target_b.shapes[0].layer = 2;
        let mut obstacles = Vec::new();
        for x in [305, 420, 580, 705] {
            for r in [
                rect(x - 60, 240, x - 30, 360),
                rect(x + 30, 240, x + 60, 360),
                rect(x - 30, 240, x + 30, 270),
                rect(x - 30, 330, x + 30, 360),
            ] {
                obstacles.push(Blockage {
                    layer: 0,
                    rect: r,
                    net: None,
                });
            }
        }
        let mut design = input(vec![first, second, target_a, target_b], obstacles);
        design.layers[3].spacing = 320;
        let mut witness_input = input(design.terminals.clone(), design.obstacles.clone());
        witness_input.layers[3].spacing = 320;
        witness_input.terminals[0].shapes.remove(0);
        witness_input.terminals[1].shapes.remove(0);
        let witness =
            route_cpu_ref(&witness_input, &limits()).expect("outer landings are feasible");
        validate(&design, &witness, &limits()).expect("witness satisfies the full pin geometry");
        let routed =
            route_cpu_ref(&design, &limits()).expect("both pins can choose outer landings");
        validate(&design, &routed, &limits()).unwrap();
        let repeated = route_cpu_ref(&design, &limits()).unwrap();
        assert_eq!(routed.wires, repeated.wires);
        assert_eq!(routed.vias, repeated.vias);
        assert_eq!(routed.access, repeated.access);

        let batch_limits = RouteLimits {
            max_active_nodes: 2000,
            max_rule_violations: 256,
            ..limits()
        };
        let (batched, report) = crate::route_batches::route(&design, &batch_limits, 4)
            .expect("alternate access repairs a batch with no completed blockers");
        validate(&design, &batched, &batch_limits).unwrap();
        assert_eq!(report.repair_attempts, 1);
        assert_eq!(report.displaced_nets, 0);
        assert_eq!(batched.wires, routed.wires);
        assert_eq!(batched.vias, routed.vias);
        assert!(batched.stats.expansions > routed.stats.expansions);
        let no_retry = RouteLimits {
            max_batch_retries: 0,
            ..batch_limits.clone()
        };
        assert!(matches!(
            crate::route_batches::route(&design, &no_retry, 4),
            Err(crate::Error::Budget(_))
        ));
        let no_extra_work = RouteLimits {
            max_expansions: routed.stats.expansions,
            ..batch_limits
        };
        assert!(matches!(
            crate::route_batches::route(&design, &no_extra_work, 4),
            Err(crate::Error::Budget(_))
        ));

        // Exercise the master-relative cache used by physical workspaces.
        design.access_classes = design
            .terminals
            .iter()
            .map(|t| {
                let mut blockers: Vec<_> = design
                    .obstacles
                    .iter()
                    .map(|b| LayerRect {
                        layer: b.layer,
                        rect: b.rect,
                    })
                    .collect();
                blockers.extend(
                    design
                        .terminals
                        .iter()
                        .filter(|p| p.pin != t.pin)
                        .flat_map(|p| p.shapes.iter().cloned()),
                );
                crate::routing::AccessClass {
                    shapes: t.shapes.clone(),
                    blockers,
                }
            })
            .collect();
        for (i, t) in design.terminals.iter_mut().enumerate() {
            t.class = Some((i as u32, [0, 0]));
        }
        validate(&design, &witness, &limits()).unwrap();
        let cached =
            route_cpu_ref(&design, &limits()).expect("cached alternatives remain available");
        validate(&design, &cached, &limits()).unwrap();

        // Without the outer choices, fixed cut spacing makes this impossible.
        let mut impossible = input(design.terminals.clone(), design.obstacles.clone());
        impossible.layers[3].spacing = 320;
        for t in &mut impossible.terminals {
            t.class = None;
        }
        impossible.terminals[0].shapes.truncate(1);
        impossible.terminals[1].shapes.truncate(1);
        assert!(matches!(
            route_cpu_ref(&impossible, &limits()),
            Err(crate::Error::PinAccess(_))
        ));
    }

    /// Two M1 pins in one row on the 40-DBU pitch. V12 lands on M2 with a
    /// 30-DBU pad, so V12s on adjacent tracks are closer than M2 spacing.
    /// M1/M2 walls leave `first` (net 0) its own landing at x = 300 only;
    /// `second` lands at x = 340 (its cheapest choice, beside `first`) or
    /// further right unless `walled`.
    fn landing_row(second_net: u32, walled: bool) -> RouteInput {
        let pins = [
            terminal(0, 0, true, rect(290, 290, 310, 310)),
            terminal(1, second_net, true, rect(330, 290, 430, 310)),
        ];
        let mut obstacles: Vec<Blockage> = pins
            .iter()
            .map(|t| Blockage {
                layer: 0,
                rect: t.shapes[0].rect,
                net: Some(t.net),
            })
            .collect();
        let mut walls = vec![
            (0, rect(0, 0, 1000, 270)),
            (0, rect(0, 330, 1000, 1000)),
            (0, rect(0, 270, 270, 330)),
            (1, rect(0, 0, 1000, 265)),
            (1, rect(0, 335, 1000, 1000)),
            (1, rect(0, 265, 265, 335)),
        ];
        if walled {
            walls.push((1, rect(375, 265, 1000, 335)));
        }
        obstacles.extend(walls.into_iter().map(|(layer, rect)| Blockage {
            layer,
            rect,
            net: None,
        }));
        let mut design = input(pins.into(), obstacles);
        design.vias[0].shapes[2].rect = rect(-15, -15, 15, 15);
        design
    }

    /// Escape `second` while `first` has failed before; `first_open` says
    /// whether `first` still lacks an escape (None: no landing zones, the
    /// greedy policy). Returns `second`'s V12 site and whether `first` can
    /// still escape afterwards.
    fn second_beside_first(design: &RouteInput, first_open: Option<bool>) -> ([i64; 2], bool) {
        let limits = limits();
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(design, plan, &limits).unwrap();
        let cells = Cells::new(design.die, plan.gcell).unwrap();
        let spatial = Spatial::new(design, &limits).unwrap();
        let mut r = Router::new(design, plan, &limits, spatial, graph, cells).unwrap();
        let mut tables = AccessTables::new();
        let (first, second) = (&design.terminals[0], &design.terminals[1]);
        let (zone, layer) = r.landing_zone(first, &mut tables).unwrap().unwrap();
        assert_eq!(layer, 1, "V12 lands on M2");
        assert!(zone.lx <= 285 && zone.ux >= 315 && zone.ly <= 285 && zone.uy >= 315);
        let mut landings = Landings::default();
        landings.insert(0, first.net, layer, zone).unwrap();
        landings.set_open(0, first_open.unwrap_or(false));
        let guard = first_open.map(|_| (&landings, 1));
        let plan = r
            .escape(second, &mut tables, AccessPreference::default(), guard)
            .unwrap()
            .expect("second escapes");
        let site = plan.vias.iter().find(|v| v.kind == 0).unwrap().at;
        let first_free = r
            .escape(first, &mut tables, AccessPreference::default(), None)
            .unwrap()
            .is_some();
        (site, first_free)
    }

    #[test]
    fn failed_pin_landing_zones_steer_neighbour_escapes() {
        let row = landing_row(1, false);
        // Hazard: the cheapest escape seals a pin that has no other landing.
        assert_eq!(second_beside_first(&row, None), ([340, 300], false));
        // A failed, unplanned neighbour's landing steers the escape aside.
        assert_eq!(second_beside_first(&row, Some(true)), ([380, 300], true));
        // Planned zones and same-net zones are not protected.
        assert_eq!(second_beside_first(&row, Some(false)), ([340, 300], false));
        let same_net = landing_row(0, false);
        assert_eq!(second_beside_first(&same_net, Some(true)).0, [340, 300]);
        // Protection prices, never forbids: the only escape still encroaches.
        let walled = landing_row(1, true);
        assert_eq!(second_beside_first(&walled, None), ([340, 300], false));
        assert_eq!(
            second_beside_first(&walled, Some(true)),
            ([340, 300], false)
        );
        // Landing zones only exist below the access layer.
        let mut above = landing_row(1, false);
        above.terminals[0].shapes[0].layer = 1;
        let limits = limits();
        let plan = above.tracks.as_ref().unwrap();
        let graph = Graph::new(&above, plan, &limits).unwrap();
        let cells = Cells::new(above.die, plan.gcell).unwrap();
        let spatial = Spatial::new(&above, &limits).unwrap();
        let mut r = Router::new(&above, plan, &limits, spatial, graph, cells).unwrap();
        let first = above.terminals[0].clone();
        assert!(
            r.landing_zone(&first, &mut AccessTables::new())
                .unwrap()
                .is_none()
        );
    }

    fn narrow_cut_design() -> RouteInput {
        let mut design = input(
            vec![
                terminal(0, 0, true, rect(90, 90, 110, 110)),
                terminal(1, 0, false, rect(90, 170, 110, 190)),
            ],
            vec![
                Blockage {
                    layer: 0,
                    rect: rect(0, 0, 1000, 1000),
                    net: None,
                },
                Blockage {
                    layer: 1,
                    rect: rect(0, 130, 1000, 150),
                    net: None,
                },
            ],
        );
        for terminal in &mut design.terminals {
            terminal.shapes[0].layer = 1;
        }
        design.layers[4].spacing = 80;
        design.vias[1].shapes[1].rect = rect(-40, -40, 40, 40);
        design
    }

    fn route_cut_fixture(design: &RouteInput, limits: &RouteLimits, soft: bool) -> Result<Routes> {
        let plan = design.tracks.as_ref().unwrap();
        let graph = Graph::new(design, plan, limits)?;
        let nodes = graph.len;
        let cells = Cells::new(design.die, plan.gcell)?;
        let cell_count = cells.len();
        let mut router = Router {
            input: design,
            plan,
            limits,
            access_hops: graph.hop_distances()?,
            target_costs: crate::routing_target::CostModel::new(
                &[false, true, false],
                &[(0, 1), (1, 2)],
                plan.via_cost,
                plan.wrong_way_factor,
            )?,
            wire_cost: wire_costs(design, plan),
            graph,
            cells,
            spatial: Spatial::new(design, limits)?,
            pairs: via_pairs(design),
            slots: HashMap::from([(0, 0), (1, 1), (2, 2)]),
            expansions: 0,
            search: Search::new(nodes, nodes)?,
            epoch: 0,
            history: History::new(nodes, nodes)?,
            gmark: vec![1; cell_count],
            gepoch: 1,
            stack_ok: vec![true; design.layers.len()],
            via_pad_ok: vec![vec![true; design.vias.len()]; design.layers.len()],
            min_run: vec![0; design.layers.len()],
            access_entries: 0,
            access_cache_bytes: 0,
            access_failures: 0,
            last_unreached: None,
        };
        // Fixed, already legal escape tops isolate the detailed traceback;
        // automatic pin-access selection can avoid the narrow path beforehand.
        let plans: HashMap<_, _> = [(0, [100, 100]), (1, [100, 180])]
            .into_iter()
            .map(|(pin, at)| {
                (
                    pin,
                    Plan {
                        access: Access {
                            pin,
                            net: 0,
                            layer: 1,
                            at,
                        },
                        source_index: 0,
                        generation: 0,
                        top: router.graph.find(1, at).unwrap(),
                        top_via: false,
                        portal_shapes: 0,
                        wires: Box::new([]),
                        vias: Box::new([]),
                        shapes: Box::new([]),
                    },
                )
            })
            .collect();
        let terminals: Vec<_> = design.terminals.iter().collect();
        let outcome = router.route_net(0, &terminals, &plans, soft, false, &BTreeMap::new(), 0);
        if outcome.is_err() {
            // A refused retry must remove every partially committed cut.
            assert!(router.spatial.via_clear(1, [100, 100], 0)?);
            assert!(router.spatial.via_clear(1, [100, 180], 0)?);
        }
        let Outcome::Routed(route, victims) = outcome? else {
            panic!("fixture has a legal cut-separated detour");
        };
        assert!(victims.is_empty());
        let pairs: Vec<_> = design.vias.iter().map(|v| (v.lower, v.upper)).collect();
        let mut result = assemble(
            &router,
            &BTreeMap::from([(0, terminals)]),
            &plans,
            &BTreeMap::from([(0, route)]),
            design,
            limits,
            &pairs,
        )?;
        result.stats.wire_length_dbu = result
            .wires
            .iter()
            .map(|w| distance(w.start, w.end).unwrap())
            .sum();
        Ok(result)
    }

    #[test]
    fn a_trace_cannot_put_two_via_cuts_too_close() {
        let design = narrow_cut_design();
        for soft in [false, true] {
            let routed = route_cut_fixture(&design, &limits(), soft).unwrap();
            validate(&design, &routed, &limits()).unwrap();
            // Independent rectangle distance: distinct 80-DBU cuts require
            // another 80 DBU of clearance in at least one coordinate direction.
            let cuts: Vec<_> = routed.vias.iter().filter(|v| v.kind == 1).collect();
            assert!(cuts.len() >= 2);
            for (i, a) in cuts.iter().enumerate() {
                for b in &cuts[i + 1..] {
                    assert!(a.at[0].abs_diff(b.at[0]) >= 160 || a.at[1].abs_diff(b.at[1]) >= 160);
                }
            }
        }
    }

    #[test]
    fn cut_conflict_retries_refuse_at_the_declared_budget() {
        let design = narrow_cut_design();
        for soft in [false, true] {
            let limit = RouteLimits {
                max_path_retries: 0,
                ..limits()
            };
            let error = route_cut_fixture(&design, &limit, soft).unwrap_err();
            assert!(
                error.to_string().contains("path via-conflict retry limit"),
                "{error}"
            );
        }
        assert!(
            RouteLimits {
                max_path_retries: 4097,
                ..limits()
            }
            .validate_track()
            .is_err()
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
                limits: &limits,
                access_hops: graph.hop_distances().unwrap(),
                target_costs: crate::routing_target::CostModel::new(
                    &[false, true, false],
                    &[(0, 1), (1, 2)],
                    plan.via_cost,
                    plan.wrong_way_factor,
                )
                .unwrap(),
                wire_cost: wire_costs(&design, plan),
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
                access_entries: 0,
                access_cache_bytes: 0,
                access_failures: 0,
                last_unreached: None,
            };
            let terminals: Vec<_> = design.terminals.iter().collect();
            let plans = r.reserve(&terminals, true).unwrap();
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
        let capped = RouteLimits {
            max_search_expansions: 1,
            ..limits()
        };
        let error = route_cpu_ref(&design, &capped).unwrap_err();
        assert!(
            error.to_string().contains("bounded root retries"),
            "{error}"
        );
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
    fn rule_fill_reservation_moves_only_routed_blockers_and_honors_its_cap() {
        // Dimensions from the full-core failure, translated near the origin.
        let a = rect(305, 535, 595, 765);
        let b = rect(690, 340, 1010, 600);
        let blocker = rect(210, 200, 390, 340);
        let mut design = input(
            Vec::new(),
            vec![
                Blockage {
                    layer: 0,
                    rect: a,
                    net: Some(0),
                },
                Blockage {
                    layer: 0,
                    rect: b,
                    net: Some(0),
                },
            ],
        );
        design.die = rect(0, 0, 2000, 2000);
        design.grid = 5;
        design.layers[0].width = 140;
        design.layers[0].spacing = 140;
        let limits = limits();
        let violation = route_rules::Violation::Spacing {
            net: 0,
            layer: 0,
            a,
            b,
        };
        let fill = route_rules::fill_for_robust(a, b, 140, 5);
        let obstruction = || Blockage {
            layer: 0,
            rect: blocker,
            net: Some(1),
        };
        let mut spatial = Spatial::new(&design, &limits).unwrap();
        let id = spatial.add_routed(obstruction()).unwrap();
        assert!(spatial.clear(0, a, 0).unwrap());
        assert!(spatial.clear(0, b, 0).unwrap());
        assert!(!spatial.clear(0, fill, 0).unwrap());
        let mut reservations = BTreeSet::new();
        let victims = reserve_rule_clearance(
            &design,
            &limits,
            &mut spatial,
            std::slice::from_ref(&violation),
            &mut reservations,
            4,
        )
        .unwrap();
        assert_eq!(victims, BTreeSet::from([1]));
        assert_eq!(reservations.len(), 1);
        spatial.remove(id);
        assert!(spatial.clear(0, fill, 0).unwrap());
        assert!(
            !spatial.clear(0, blocker, 1).unwrap(),
            "the blocking route cannot choose its old path again"
        );

        let mut fixed = Spatial::new(&design, &limits).unwrap();
        fixed.add_fixed(obstruction()).unwrap();
        let mut untouched = BTreeSet::new();
        assert!(
            reserve_rule_clearance(
                &design,
                &limits,
                &mut fixed,
                std::slice::from_ref(&violation),
                &mut untouched,
                4
            )
            .unwrap()
            .is_empty()
        );
        assert!(untouched.is_empty());
        let mut limited = Spatial::new(&design, &limits).unwrap();
        limited.add_routed(obstruction()).unwrap();
        assert!(matches!(
            reserve_rule_clearance(
                &design,
                &limits,
                &mut limited,
                &[violation],
                &mut BTreeSet::new(),
                0
            ),
            Err(crate::Error::Budget(_))
        ));
        assert!(
            limited.clear(0, blocker, 1).unwrap(),
            "refusal must not add a reservation"
        );
    }

    #[test]
    fn grouped_access_failures_recover_with_one_batch_retry() {
        let mut terms = Vec::new();
        let mut fixed = Vec::new();
        for i in 0..2u32 {
            let x = i64::from(i) * 1000;
            let mut a = terminal(i * 2, i, true, rect(x + 490, 90, x + 510, 110));
            let mut b = terminal(i * 2 + 1, i, false, rect(x + 490, 890, x + 510, 910));
            a.shapes[0].layer = 1;
            b.shapes[0].layer = 1;
            terms.extend([a, b]);
            terms.push(terminal(
                4 + i * 2,
                2 + i,
                true,
                rect(x + 490, 490, x + 510, 510),
            ));
            terms.push(terminal(
                5 + i * 2,
                2 + i,
                false,
                rect(x + 90, 690, x + 110, 710),
            ));
            fixed.push(Blockage {
                layer: 0,
                rect: rect(x + 490, 490, x + 510, 510),
                net: Some(2 + i),
            });
            for r in [
                rect(420, 420, 470, 580),
                rect(530, 420, 580, 580),
                rect(470, 420, 530, 470),
                rect(470, 530, 530, 580),
            ] {
                fixed.push(Blockage {
                    layer: 0,
                    rect: shifted(r, [x, 0]).unwrap(),
                    net: None,
                });
            }
        }
        let mut design = input(terms, fixed);
        design.die = rect(0, 0, 2000, 1000);
        design.tracks.as_mut().unwrap().memory_mib = 256;
        let limits = RouteLimits {
            max_batch_retries: 1,
            // The entire fixture has 3,750 graph nodes. A matching active
            // bound avoids reserving 200k nodes of unrelated validation state.
            max_active_nodes: 4_000,
            ..limits()
        };
        let witness = route_cpu_ref(&design, &limits).unwrap();
        validate(&design, &witness, &limits).unwrap();
        let (routed, report) = crate::route_batches::route(&design, &limits, 4).unwrap();
        validate(&design, &routed, &limits).unwrap();
        assert_eq!(routed.nets, [0, 1, 2, 3]);
        assert_eq!(routed.access.len(), 8);
        assert_eq!(report.repair_attempts, 1);
        assert_eq!(report.displaced_nets, 2);
        let single = RouteLimits {
            max_batch_blocked_pins: 1,
            ..limits.clone()
        };
        assert!(matches!(
            crate::route_batches::route(&design, &single, 4),
            Err(crate::Error::Budget(_))
        ));
        let exact = RouteLimits {
            max_batch_blocked_pins: 2,
            ..limits
        };
        let (again, _) = crate::route_batches::route(&design, &exact, 4).unwrap();
        assert_eq!(routed.wires, again.wires);
        assert_eq!(routed.vias, again.vias);
        assert_eq!(routed.access, again.access);
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
        // The fixture needs a few hundred active nodes; the default cap would
        // reserve ~190 MiB of validation scratch near the 256 MiB admission
        // edge, where concurrently running tests' live heap decides the result.
        let limits = RouteLimits {
            max_active_nodes: 4_000,
            ..limits()
        };
        let together = route_cpu_ref(&design, &limits).unwrap();
        validate(&design, &together, &limits).unwrap();
        let (batched, report) = crate::route_batches::route(&design, &limits, 2).unwrap();
        validate(&design, &batched, &limits).unwrap();
        assert_eq!(batched.nets, [0, 1]);
        assert_eq!(batched.access.len(), 4);
        assert_eq!(report.repair_attempts, 1);
        assert_eq!(report.displaced_nets, 1);
        let (again, _) = crate::route_batches::route(&design, &limits, 2).unwrap();
        assert_eq!(batched.wires, again.wires);
        assert_eq!(batched.vias, again.vias);
        assert_eq!(batched.access, again.access);
        for limited in [
            RouteLimits {
                max_batch_retries: 0,
                ..limits.clone()
            },
            RouteLimits {
                max_batch_repair_terminals: 3,
                ..limits.clone()
            },
            RouteLimits {
                max_expansions: batched.stats.expansions - 1,
                ..limits.clone()
            },
        ] {
            assert!(matches!(
                crate::route_batches::route(&design, &limited, 2),
                Err(crate::Error::Budget(_))
            ));
        }
    }

    /// Net 1's port on M2 sits in a ring of fixed cell metal over fixed M1,
    /// so its only exit is V23 at its top (500,500). Net 0, routed first by
    /// the batches, runs straight along M3 over that top: the next batch's
    /// search from the port exhausts its frontier at once (net 3611 of the
    /// full OpenC910 route, sealed by net 1307's completed bridge).
    fn sealed_port_design(seal_is_completed_route: bool) -> RouteInput {
        let mut terms = vec![
            terminal(0, 0, true, rect(90, 490, 110, 510)),
            terminal(1, 0, false, rect(890, 490, 910, 510)),
            terminal(2, 1, true, rect(470, 470, 530, 530)),
            terminal(3, 1, false, rect(90, 90, 110, 110)),
        ];
        terms[0].shapes[0].layer = 2;
        terms[1].shapes[0].layer = 2;
        terms[2].shapes[0].layer = 1;
        let mut fixed = vec![
            Blockage {
                layer: 1,
                rect: terms[2].shapes[0].rect,
                net: Some(1),
            },
            Blockage {
                layer: 0,
                rect: rect(450, 450, 550, 550),
                net: None,
            },
        ];
        for r in [
            rect(420, 420, 450, 580),
            rect(550, 420, 580, 580),
            rect(450, 420, 550, 450),
            rect(450, 550, 550, 580),
        ] {
            fixed.push(Blockage {
                layer: 1,
                rect: r,
                net: None,
            });
        }
        if !seal_is_completed_route {
            // The same seal as immovable geometry: nothing can reopen it.
            terms.truncate(2);
            terms[0].net = 1;
            terms[1].net = 1;
            terms[0].pin = 2;
            terms[1].pin = 3;
            terms[0].shapes[0] = LayerRect {
                layer: 1,
                rect: rect(470, 470, 530, 530),
            };
            terms[1].shapes[0] = LayerRect {
                layer: 0,
                rect: rect(90, 90, 110, 110),
            };
            fixed.push(Blockage {
                layer: 2,
                rect: rect(300, 490, 700, 510),
                net: None,
            });
        }
        let mut design = input(terms, fixed);
        design.tracks.as_mut().unwrap().memory_mib = 256;
        design
    }

    #[test]
    fn a_completed_route_sealing_a_later_port_is_reopened() {
        let design = sealed_port_design(true);
        let limits = RouteLimits {
            max_active_nodes: 4_000,
            ..limits()
        };
        // Independent witness: routing both nets together succeeds.
        let together = route_cpu_ref(&design, &limits).unwrap();
        validate(&design, &together, &limits).unwrap();
        let (batched, report) = crate::route_batches::route(&design, &limits, 2).unwrap();
        validate(&design, &batched, &limits).unwrap();
        assert_eq!(batched.nets, [0, 1]);
        assert_eq!(batched.access.len(), 4);
        assert_eq!(report.repair_attempts, 1);
        assert_eq!(report.displaced_nets, 1);
        // The port now leaves through V23 at its top.
        assert!(
            batched.vias.iter().any(|v| v.net == 1
                && v.at == [500, 500]
                && design.vias[v.kind as usize].upper == 2)
        );
        let (again, _) = crate::route_batches::route(&design, &limits, 2).unwrap();
        assert_eq!(batched.wires, again.wires);
        assert_eq!(batched.vias, again.vias);
        assert_eq!(batched.access, again.access);
        for limited in [
            RouteLimits {
                max_batch_retries: 0,
                ..limits.clone()
            },
            RouteLimits {
                max_batch_displaced_nets: 0,
                ..limits.clone()
            },
        ] {
            assert!(matches!(
                crate::route_batches::route(&design, &limited, 2),
                Err(crate::Error::Budget(_))
            ));
        }
    }

    #[test]
    fn a_port_sealed_by_fixed_geometry_reports_its_pocket() {
        let design = sealed_port_design(false);
        let limits = RouteLimits {
            max_active_nodes: 4_000,
            ..limits()
        };
        for result in [
            route_cpu_ref(&design, &limits),
            crate::route_batches::route(&design, &limits, 2).map(|(routes, _)| routes),
        ] {
            let Err(crate::Error::NoPath(failure)) = result else {
                panic!("expected a structured no-path failure, got {result:?}");
            };
            assert_eq!((failure.pin, failure.net), (2, 1));
            assert_eq!(
                failure.to_string(),
                "NO_ROUTE: pin 2 net 1: no path even with rip-up (fixed geometry blocks)"
            );
            // The pocket: the port metal and the one node its search expanded.
            assert_eq!(failure.region, rect(470, 470, 530, 530));
            assert!(failure.expansions > 0 && failure.comparisons > 0);
        }
    }

    #[test]
    fn resumed_batches_repeat_the_uninterrupted_route() {
        use crate::route_batches::{BatchCheckpoint, route_resumable};
        // Same fixture as above: batch 1 reopens the completed net of batch 0.
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
        // The fixture needs a few hundred active nodes; the default cap would
        // reserve ~190 MiB of validation scratch near the 256 MiB admission
        // edge, where concurrently running tests' live heap decides the result.
        let limits = RouteLimits {
            max_active_nodes: 4_000,
            ..limits()
        };
        let mut saved = Vec::new();
        let (full, report) = route_resumable(&design, &limits, 2, None, &mut |p| {
            saved.push(BatchCheckpoint {
                begin: p.begin,
                routes: p.routes.clone(),
                report: p.report.clone(),
                completed: p.completed.to_vec(),
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(report.displaced_nets, 1);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].begin, 2);
        assert_eq!(saved[0].report.batches, 1);
        let same = |a: &Routes, b: &Routes| {
            assert_eq!(a.wires, b.wires);
            assert_eq!(a.vias, b.vias);
            assert_eq!(a.access, b.access);
            assert_eq!(a.patches, b.patches);
            assert_eq!(a.nets, b.nets);
            assert_eq!(a.stats.expansions, b.stats.expansions);
            assert_eq!(a.stats.comparisons, b.stats.comparisons);
            assert_eq!(a.stats.wire_length_dbu, b.stats.wire_length_dbu);
            assert_eq!(a.stats.grid_nodes, b.stats.grid_nodes);
        };
        for state in &saved {
            let mut later = Vec::new();
            let (again, again_report) =
                route_resumable(&design, &limits, 2, Some(state.clone()), &mut |p| {
                    later.push(p.begin);
                    Ok(())
                })
                .unwrap();
            same(&full, &again);
            assert_eq!(again_report.batches, report.batches);
            assert_eq!(again_report.repair_attempts, report.repair_attempts);
            assert_eq!(again_report.displaced_nets, report.displaced_nets);
            let expected: Vec<_> = saved
                .iter()
                .map(|s| s.begin)
                .filter(|&b| b > state.begin)
                .collect();
            assert_eq!(later, expected);
        }
        // Negative controls: a state off the primary batch boundaries, with a
        // wrong batch count, or with geometry that does not match its routes.
        let mut off = saved[0].clone();
        off.begin = 1;
        off.report.batches = 0;
        let mut miscounted = saved[0].clone();
        miscounted.report.batches = 2;
        let mut missing = saved[0].clone();
        missing.completed.pop();
        let mut ownerless = saved[0].clone();
        ownerless.completed[0].net = None;
        for bad in [off, miscounted, missing, ownerless] {
            assert!(matches!(
                route_resumable(&design, &limits, 2, Some(bad), &mut |_| Ok(())),
                Err(crate::Error::Invalid(_))
            ));
        }
        // A failing sink stops the route with its own error.
        assert!(matches!(
            route_resumable(&design, &limits, 2, None, &mut |_| Err(budget("sink"))),
            Err(crate::Error::Budget(m)) if m == "sink"
        ));
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
                source_index: 0,
                generation: 0,
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
        let index = WindowIndex::new(&windows, &mut work, 200_000).unwrap();
        let build_work = work;
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
                .overlapping(&windows, query, &mut work, 200_000)
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
            assert_eq!(
                index
                    .touches_any(&windows, query, &mut work, 200_000)
                    .unwrap(),
                !expected.is_empty()
            );
        }
        assert!(
            work - build_work < 2_000,
            "queries must avoid whole-design rescans"
        );
        assert!(WindowIndex::new(&windows, &mut 0, 9999).is_err());
        assert!(index.overlapping(&windows, windows[0], &mut 0, 99).is_err());
        assert!(index.touches_any(&windows, windows[0], &mut 0, 0).is_err());
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
}
