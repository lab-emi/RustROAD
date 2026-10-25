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
    distance, expand, int, invalid, no_route, quartile_points, shifted, validate, via_pairs,
    wire_rect,
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
/// Sorted window index plus construction headroom (one entry per terminal).
const WINDOW_INDEX_BYTES: u64 = 32;
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
            (nodes.min(limits.max_search_expansions.saturating_add(limits.max_heap)) as u64)
                .saturating_mul(64)
        })
        .saturating_add((nodes.min(limits.max_heap) as u64).saturating_mul(128))
        .saturating_add((gcells as u64).saturating_mul(GCELL_BYTES))
        .saturating_add(access_memory)
        .saturating_add(layers.saturating_mul(input.vias.len() as u64 + 32))
        .saturating_add(crate::routing_target::MEMORY_ALLOWANCE),
        spatial: index(added),
        routed: t
            .saturating_mul(ROUTED_BYTES_PER_TERMINAL)
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
#[derive(Clone)]
struct Plan {
    access: Access,
    top: u32,
    /// The escape reaches `top` through a via (a further via would stack).
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
    limits: &'a RouteLimits,
    graph: Graph,
    access_hops: Vec<Vec<u16>>,
    target_costs: crate::routing_target::CostModel,
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

    /// One terminal escape search; None when no legal escape exists.
    fn escape(&mut self, t: &Terminal, tables: &mut AccessTables) -> Result<Option<Plan>> {
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
        let mut visited = HashMap::<u32, (u64, u32, u32)>::new();
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
                    let listed: Vec<usize> = match table_kinds.get(&(layer, at)) {
                        Some(ks) if pair.lower == layer => ks
                            .iter()
                            .copied()
                            .filter(|k| pair.kinds.contains(k))
                            .collect(),
                        _ => pair.kinds.clone(),
                    };
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
            top: top_node,
            top_via,
            portal_shapes,
            wires: wires.into_boxed_slice(),
            vias: vias.into_boxed_slice(),
            shapes: shapes.into_boxed_slice(),
        }))
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
        let mut queue: VecDeque<usize> = order.into_iter().map(|o| o.2).collect();
        let mut plans: HashMap<u32, Plan> = HashMap::new();
        let mut plan_records = 0usize;
        let mut owner: BTreeMap<u32, usize> = BTreeMap::new();
        let mut attempts = vec![0usize; terminals.len()];
        let mut rounds = 0usize;
        let mut window_index = None;
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
            if let Some(plan) = self.escape(t, &mut tables)? {
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
                return Err(no_route(&format!(
                    "pin {} net {} has no legal access after local repair",
                    t.pin, t.net
                )));
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
                return Err(no_route(&format!(
                    "pin {} net {} has no legal access",
                    t.pin, t.net
                )));
            }
            let mut back = Vec::new();
            for p in victims {
                let plan = plans.remove(&p).unwrap();
                plan_records -= plan.wires.len() + plan.vias.len();
                for id in plan.shapes {
                    self.spatial.remove(id);
                }
                back.push(owner.remove(&p).unwrap());
            }
            // Rotate the victims so repeated repairs try different orders.
            let rot = attempts[i] % back.len().max(1);
            back.rotate_left(rot);
            for j in back.into_iter().rev() {
                queue.push_front(j);
            }
            queue.push_front(i);
        }
        Ok(plans)
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
        let mut edges = BTreeMap::new();
        let root = plans[&terminals[0].pin].top;
        let (root_layer, root_point) = self.graph.point(root);
        let mut target =
            crate::routing_target::TreeDistance::new(self.graph.slot[&root_layer], root_point);
        let mut tree: HashSet<u32> = HashSet::from([root]);
        // Tree junctions remember whether an actual incident pad meets AREA.
        let mut via_nodes: HashMap<u32, bool> = HashMap::new();
        if plans[&terminals[0].pin].top_via {
            via_nodes.insert(root, self.escape_pad_ok(&plans[&terminals[0].pin]));
        }
        let mut victims_all: BTreeSet<u32> = BTreeSet::new();
        let via_cost = self.plan.via_cost;
        let wrong = self.plan.wrong_way_factor;
        let conflict = via_cost.saturating_mul(10).saturating_mul(1 + round as u64);
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
            let mut search = 0usize;
            while let Some((_, Reverse(cost), id)) = heap.pop() {
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
                            "[troute] search cap net {net} pin {} soft {soft} start {:?} root {:?} current {:?} tree_nodes {} cost {cost}",
                            t.pin,
                            self.graph.point(start),
                            (root_layer, root_point),
                            self.graph.point(id),
                            tree.len(),
                        );
                    }
                    limited = true;
                    break;
                }
                self.tick(&mut search, "detailed", t.pin, net)?;
                let (l, t_, s_) = self.graph.locate(id);
                let layer = self.graph.layers[l].layer;
                let p = self.graph.xy(l, t_, s_);
                let target_distance = target.bounds_distance(p);
                self.graph.neighbors(id, &mut nbuf);
                let work = self.graph.target_neighbors(id, root_point, &mut nbuf);
                self.spatial.work = self
                    .spatial
                    .work
                    .checked_add(work)
                    .filter(|&n| n <= self.limits.max_comparisons)
                    .ok_or_else(|| budget("target projection work"))?;
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
                    if matches!(mv, Move::Along)
                        && id.abs_diff(next) > 1
                        && target.bounds_distance(q) >= target_distance
                    {
                        continue;
                    }
                    let joins = tree.contains(&next);
                    if self.gmark[self.cells.of(q) as usize] != self.gepoch && !joins {
                        continue;
                    }
                    let mut step = match mv {
                        Move::Along => distance(p, q)?,
                        Move::Wrong => distance(p, q)?.saturating_mul(wrong),
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
                                        u64::from(self.history.get(node)).saturating_mul(hist_unit),
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
                                            .ok_or_else(|| invalid("missing routing parent state"))?
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
                                                    * (1 + rips.get(v).copied().unwrap_or(0) as u64)
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
                                                    * (1 + rips.get(v).copied().unwrap_or(0) as u64)
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
                    heap.offer((
                        nc.saturating_add(
                            target
                                .distance(
                                    self.graph.slot[&nl],
                                    q,
                                    &self.target_costs,
                                    &mut self.spatial.work,
                                    self.limits.max_comparisons,
                                )?
                                .saturating_mul(self.plan.heuristic_weight),
                        ),
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
            type Spans = (Vec<(i64, i64)>, Vec<u32>);
            let mut lines: BTreeMap<Line, Spans> = BTreeMap::new();
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
                    let e = lines.entry((la, vertical, cross)).or_default();
                    e.0.push((lo, hi));
                    e.1.push(b);
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
            for ((layer, vertical, cross), (mut spans, nodes)) in lines {
                spans.sort_unstable();
                let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
                for (lo, hi) in spans {
                    match merged.last_mut() {
                        Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                        _ => merged.push((lo, hi)),
                    }
                }
                for (lo, hi) in merged {
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
                        for &n in &nodes {
                            self.history.increment(n)?;
                        }
                    }
                    out.shapes.push(self.spatial.add_routed(Blockage {
                        layer,
                        rect,
                        net: Some(net),
                    })?);
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
pub(crate) fn route(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
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
        access_entries: 0,
        access_cache_bytes: 0,
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
    let via_layers: Vec<(u32, u32)> = input.vias.iter().map(|v| (v.lower, v.upper)).collect();
    for pass in 0..=4usize {
        // Repair: negotiated rip-up and reroute.
        while let Some(net) = queue.pop_front() {
            round += 1;
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
            if matches!(outcome, Outcome::Limited(_))
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
            match outcome {
                Outcome::Limited(pin) => {
                    return Err(budget(&format!(
                        "per-search detailed limit after bounded root retries, pin {pin} net {net}"
                    )));
                }
                Outcome::Failed(pin) => {
                    return Err(no_route(&format!(
                        "pin {pin} net {net}: no path even with rip-up (fixed geometry blocks)"
                    )));
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
        if pass == 4 {
            let v = &remaining[0];
            return Err(no_route(&format!(
                "ROUTING_RULE unresolved {} ({} total)",
                v.describe(),
                remaining.len()
            )));
        }
        // Re-route the offending nets in strict mode (no near misses).
        let bad: BTreeSet<u32> = remaining.iter().map(|v| v.net()).collect();
        for net in bad {
            if let Some(old) = remove_route(&mut routed, &mut retained_edges, net) {
                for id in old.shapes {
                    r.spatial.remove(id);
                }
            }
            strict.insert(net);
            queue.push_back(net);
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
}
