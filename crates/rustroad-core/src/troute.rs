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
use crate::routing::{
    Access, AccessEntry, Blockage, LayerRect, RouteInput, RouteLimits, RouteStats, Routes, Soft,
    Spatial, Terminal, TrackPlan, Via, ViaPair, WIRE, Wire, access_table_at, budget, contains,
    distance, expand, int, invalid, no_route, overlap, quartile_points, shifted, validate,
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
/// Routed state per terminal (per-net edge maps, escape plans, guides,
/// queues), calibrated on the scale blocks (docs/scaling.md: 2.8-4.8 KiB
/// per terminal measured with the per-node term below).
const ROUTED_BYTES_PER_TERMINAL: u64 = 4096;
/// Routed graph edges are bounded by the nodes they occupy (one net per
/// node): bytes per node of per-net edge maps.
const ROUTED_BYTES_PER_NODE: u64 = 8;

/// Upfront whole-route memory estimate (bytes) of the track router.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
        || plan.memory_mib == 0
        || plan.memory_mib > 65_536
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
pub fn memory_estimate(input: &RouteInput, nodes: usize, gcells: usize) -> Result<MemoryEstimate> {
    let bin = crate::routing::bin_side(input)?;
    let (.., cells) = crate::routing::bin_extent(input, bin);
    let layers = input.layers.len() as u64;
    let grid = cells.saturating_mul(layers).saturating_mul(GRID_BYTES);
    let mut fixed_entries = 0u64;
    for b in &input.obstacles {
        let span = |lo: i64, hi: i64| (hi.div_euclid(bin) - lo.div_euclid(bin) + 1).max(0) as u64;
        fixed_entries = fixed_entries
            .saturating_add(span(b.rect.lx, b.rect.ux).saturating_mul(span(b.rect.ly, b.rect.uy)));
    }
    let o = input.obstacles.len() as u64;
    let t = input.terminals.len() as u64;
    let added = t.saturating_mul(SHAPES_PER_TERMINAL);
    let index = |shapes: u64| {
        grid.saturating_add((o + shapes).saturating_mul(SHAPE_BYTES))
            .saturating_add(
                fixed_entries
                    .saturating_add(shapes.saturating_mul(ENTRIES_PER_SHAPE))
                    .saturating_mul(ENTRY_BYTES),
            )
    };
    let records = t.saturating_mul(RECORDS_PER_TERMINAL);
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        eprintln!(
            "[troute] estimate inputs grid_cells {cells} layers {layers} fixed_entries {fixed_entries} obstacles {o} terminals {t} nodes {nodes} gcells {gcells} held {}",
            input.held_bytes
        );
    }
    Ok(MemoryEstimate {
        held: input.held_bytes,
        search: (nodes as u64)
            .saturating_mul(NODE_BYTES)
            .saturating_add((gcells as u64).saturating_mul(GCELL_BYTES)),
        spatial: index(added),
        routed: t
            .saturating_mul(ROUTED_BYTES_PER_TERMINAL)
            .saturating_add((nodes as u64).saturating_mul(ROUTED_BYTES_PER_NODE)),
        check: index(records.saturating_mul(2))
            .saturating_add(records.saturating_mul(RECORD_BYTES)),
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
    memory_estimate(input, graph.len, cells.len()).map(Some)
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
    wires: Vec<Wire>,
    vias: Vec<Via>,
    shapes: Vec<usize>,
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

struct Router<'a> {
    input: &'a RouteInput,
    plan: &'a TrackPlan,
    limits: &'a RouteLimits,
    graph: Graph,
    cells: Cells,
    spatial: Spatial<'a>,
    pairs: Vec<ViaPair>,
    slots: HashMap<u32, usize>,
    expansions: usize,
    // Detailed search state.
    cost: Vec<u64>,
    parent: Vec<u32>,
    via_kind: Vec<u32>,
    tag: Vec<u32>,
    epoch: u32,
    history: Vec<u16>,
    gmark: Vec<u32>,
    gepoch: u32,
    /// Per input layer: may two vias stack through it (every via pad on the
    /// layer meets the LEF AREA alone)?
    stack_ok: Vec<bool>,
    /// Per input layer: wire run (DBU) between an entering and a leaving via
    /// on a layer that is not `stack_ok` (AREA / width, rounded up).
    min_run: Vec<u64>,
}

/// One routed net: graph edges (a, b, kind) and its spatial shape ids.
#[derive(Default, Clone)]
struct NetRoute {
    edges: BTreeMap<(u32, u32), u32>,
    shapes: Vec<usize>,
}

enum Outcome {
    Routed(NetRoute, Vec<u32>),
    Failed(u32),
}

impl<'a> Router<'a> {
    fn width(&self, layer: u32) -> i64 {
        self.input.layers[layer as usize].width
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

    /// Access table cache key: class and instance origin modulo the track
    /// pattern period, so on-graph landing points are identical per key.
    fn period(&self) -> (i64, i64) {
        let gcd = |mut a: i64, mut b: i64| {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        };
        // Node x coordinates are vertical-layer tracks, y horizontal ones.
        let mut p = [1i64, 1i64];
        for g in &self.graph.layers {
            let (pitch, _) = self.plan.tracks[g.layer as usize].unwrap();
            let axis = usize::from(!g.vertical);
            if p[axis] == 0 {
                continue;
            }
            let l = p[axis] / gcd(p[axis], pitch) * pitch;
            p[axis] = if l > 1 << 24 { 0 } else { l };
        }
        (p[0], p[1])
    }

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
        tables: &mut BTreeMap<(u32, i64, i64), Vec<AccessEntry>>,
    ) -> Result<Vec<(u32, [i64; 2], u64, Option<Vec<usize>>)>> {
        let step = self.input.step as u64;
        let generic = bounded_points(&t.shapes, self.input.grid, Some(self.input.die))?;
        let on_graph = self.graph_points(&t.shapes);
        let mut starts = Vec::new();
        if let Some((class, origin)) = t.class {
            let (px, py) = self.period();
            let key = (
                class,
                if px > 0 {
                    origin[0].rem_euclid(px)
                } else {
                    origin[0]
                },
                if py > 0 {
                    origin[1].rem_euclid(py)
                } else {
                    origin[1]
                },
            );
            if let std::collections::btree_map::Entry::Vacant(slot) = tables.entry(key) {
                let rel = |(l, p): &(u32, [i64; 2])| -> Result<(u32, [i64; 2])> {
                    Ok((
                        *l,
                        [
                            int(i128::from(p[0]) - i128::from(origin[0]))?,
                            int(i128::from(p[1]) - i128::from(origin[1]))?,
                        ],
                    ))
                };
                let c = &self.input.access_classes[class as usize];
                let mut points: Vec<(u32, [i64; 2])> =
                    bounded_points(&c.shapes, self.input.grid, None)?;
                let known: BTreeSet<_> = points.iter().copied().collect();
                for p in &on_graph {
                    let r = rel(p)?;
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
                self.spatial.work = self
                    .spatial
                    .work
                    .checked_add(work)
                    .ok_or_else(|| budget("routing comparison limit"))?;
                slot.insert(table);
            }
            let allowed: BTreeSet<_> = generic.iter().chain(&on_graph).copied().collect();
            for e in &tables[&key] {
                let p = [
                    int(i128::from(e.at[0]) + i128::from(origin[0]))?,
                    int(i128::from(e.at[1]) + i128::from(origin[1]))?,
                ];
                if allowed.contains(&(e.layer, p)) && contains(self.input.die, p) {
                    let cost = if e.fits { 0 } else { step };
                    starts.push((e.layer, p, cost, Some(e.kinds.clone())));
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
    fn escape(
        &mut self,
        t: &Terminal,
        tables: &mut BTreeMap<(u32, i64, i64), Vec<AccessEntry>>,
    ) -> Result<Option<Plan>> {
        let input = self.input;
        let starts = self.starts(t, tables)?;
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
            xs.extend(
                xv.iter()
                    .copied()
                    .filter(|&x| bounds.lx <= x && x <= bounds.ux),
            );
            ys.extend(
                yv.iter()
                    .copied()
                    .filter(|&y| bounds.ly <= y && y <= bounds.uy),
            );
        }
        let local = Local {
            xs: xs.into_iter().collect(),
            ys: ys.into_iter().collect(),
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
                    heap.push(Reverse((*cost, id)));
                }
            }
        }
        let mut found = None;
        let mut search = 0usize;
        while let Some(Reverse((cost, id))) = heap.pop() {
            if visited[&id].0 != cost {
                continue;
            }
            let (li, at) = local.at(id);
            if slots[li] == goal_slot && self.graph.find(goal_slot, at).is_some() {
                found = Some(id);
                break;
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
                heap.push(Reverse((nc, nid)));
            }
        }
        let Some(top) = found else {
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
        Ok(Some(Plan {
            access: Access {
                pin: t.pin,
                net: t.net,
                layer: access_layer,
                at,
            },
            top: top_node,
            top_via,
            wires,
            vias,
            shapes,
        }))
    }

    /// Conflict-aware escapes for every terminal of a routed net.
    fn reserve(&mut self, terminals: &[&'a Terminal]) -> Result<HashMap<u32, Plan>> {
        let mut tables = BTreeMap::new();
        // Most constrained first: fewest start candidates, then pin index.
        let mut order: Vec<(usize, u32, usize)> = Vec::with_capacity(terminals.len());
        for (i, t) in terminals.iter().enumerate() {
            let n = self.starts(t, &mut tables)?.len();
            order.push((n, t.pin, i));
        }
        order.sort_unstable();
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
        let mut owner: BTreeMap<u32, usize> = BTreeMap::new();
        let mut attempts = vec![0usize; terminals.len()];
        let mut rounds = 0usize;
        while let Some(i) = queue.pop_front() {
            rounds += 1;
            if rounds > terminals.len().saturating_mul(8) + 64 {
                return Err(budget("pin access repair rounds"));
            }
            let t = terminals[i];
            if let Some(plan) = self.escape(t, &mut tables)? {
                plans.insert(t.pin, plan);
                owner.insert(t.pin, i);
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
            let w = windows[i];
            let victims: Vec<u32> = owner
                .iter()
                .filter(|(_, j)| {
                    let o = windows[**j];
                    o.lx <= w.ux && w.lx <= o.ux && o.ly <= w.uy && w.ly <= o.uy
                })
                .map(|(p, _)| *p)
                .collect();
            if victims.is_empty() {
                return Err(no_route(&format!(
                    "pin {} net {} has no legal access",
                    t.pin, t.net
                )));
            }
            let mut back = Vec::new();
            for p in victims {
                let plan = plans.remove(&p).unwrap();
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
    fn global(&mut self, nets: &[(u32, Vec<u32>)]) -> Result<BTreeMap<u32, Vec<u32>>> {
        let cap = self.capacities()?;
        let n = self.cells.len();
        let mut usage = vec![0u32; cap.len()];
        let mut hist = vec![0u32; cap.len()];
        let mut cost = vec![u64::MAX; n];
        let mut parent = vec![NONE; n];
        let mut tag = vec![0u32; n];
        let mut epoch = 0u32;
        let mut routes: BTreeMap<u32, BTreeSet<usize>> = BTreeMap::new();
        let mut cellsets: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
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
                    heap.push(Reverse((h(sink), 0u64, sink)));
                    let mut found = None;
                    while let Some(Reverse((_, c, id))) = heap.pop() {
                        if tag[id as usize] != epoch || cost[id as usize] != c {
                            continue;
                        }
                        if tree.contains(&id) {
                            found = Some(id);
                            break;
                        }
                        work += 1;
                        if work > self.limits.max_expansions {
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
                            heap.push(Reverse((nc + h(nb), nc, nb)));
                        }
                    }
                    let mut cur = found.ok_or_else(|| invalid("global routing disconnected"))?;
                    while parent[cur as usize] != NONE {
                        let p = parent[cur as usize];
                        edges.insert(self.cells.edge(cur, p));
                        tree.insert(cur);
                        cur = p;
                    }
                    tree.insert(cur);
                }
                for &e in &edges {
                    usage[e] += 1;
                }
                routes.insert(net, edges);
                cellsets.insert(net, tree);
            }
        }
        self.expansions = self.expansions.saturating_add(work);
        // Guides: dilated GCell sets.
        let m = self.plan.guide_margin as i64;
        let mut guides = BTreeMap::new();
        for (net, set) in cellsets {
            let mut g = BTreeSet::new();
            for c in set {
                let (x, y) = self.cells.xy(c);
                for dy in -m..=m {
                    for dx in -m..=m {
                        let (xx, yy) = (x as i64 + dx, y as i64 + dy);
                        if xx >= 0
                            && yy >= 0
                            && (xx as usize) < self.cells.nx
                            && (yy as usize) < self.cells.ny
                        {
                            g.insert((yy as usize * self.cells.nx + xx as usize) as u32);
                        }
                    }
                }
            }
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
            let mut vias = Vec::new();
            for (&(a, b), &kind) in &route.edges {
                let (la, pa) = self.graph.point(a);
                let (lb, pb) = self.graph.point(b);
                if la == lb {
                    let vertical = pa[0] == pb[0];
                    let (axis, lo, hi) = if vertical { (1, pa, pb) } else { (0, pa, pb) };
                    let (lo, hi) = if lo[axis] <= hi[axis] {
                        (lo, hi)
                    } else {
                        (hi, lo)
                    };
                    ends.entry((la, lo)).or_default().push((vertical, 1));
                    ends.entry((la, hi)).or_default().push((vertical, -1));
                } else {
                    vias.push(((a.min(b), a.max(b)), kind, if la < lb { pa } else { pb }));
                }
            }
            for (key, kind, at) in vias {
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
                        route.edges.insert(key, k as u32);
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
    fn via_on_own_pin(&self, kind: usize, at: [i64; 2], own: &[(u32, Rect, [i64; 2])]) -> bool {
        self.input.vias[kind].shapes.iter().any(|s| {
            shifted(s.rect, at).is_ok_and(|r| {
                own.iter()
                    .any(|(l, pin, top)| *l == s.layer && overlap(r, *pin) && at != *top)
            })
        })
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
        let mut out = NetRoute::default();
        let root = plans[&terminals[0].pin].top;
        let mut tree: HashSet<u32> = HashSet::from([root]);
        // Tree nodes with an incident via: a path may not join them with
        // another via through a layer whose single pads are below AREA.
        let mut via_nodes: HashSet<u32> = HashSet::new();
        if plans[&terminals[0].pin].top_via {
            via_nodes.insert(root);
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
        let own: Vec<(u32, Rect, [i64; 2])> = terminals
            .iter()
            .flat_map(|t| {
                let top = self.graph.point(plans[&t.pin].top).1;
                t.shapes.iter().map(move |s| (s.layer, s.rect, top))
            })
            .collect();
        for t in &terminals[1..] {
            let start = plans[&t.pin].top;
            let start_via = plans[&t.pin].top_via;
            if tree.contains(&start) {
                continue;
            }
            self.epoch = self
                .epoch
                .checked_add(1)
                .ok_or_else(|| budget("routing search count"))?;
            let epoch = self.epoch;
            let pts: Vec<[i64; 2]> = tree.iter().map(|id| self.graph.point(*id).1).collect();
            let bbox = Rect {
                lx: pts.iter().map(|p| p[0]).min().unwrap(),
                ly: pts.iter().map(|p| p[1]).min().unwrap(),
                ux: pts.iter().map(|p| p[0]).max().unwrap(),
                uy: pts.iter().map(|p| p[1]).max().unwrap(),
            };
            let heuristic = |p: [i64; 2]| {
                distance(
                    p,
                    [p[0].clamp(bbox.lx, bbox.ux), p[1].clamp(bbox.ly, bbox.uy)],
                )
            };
            let mut heap = BinaryHeap::new();
            self.cost[start as usize] = 0;
            self.parent[start as usize] = NONE;
            self.via_kind[start as usize] = WIRE;
            self.tag[start as usize] = epoch;
            heap.push(Reverse((
                heuristic(self.graph.point(start).1)?,
                0u64,
                start,
            )));
            let mut found = None;
            let mut search = 0usize;
            while let Some(Reverse((_, cost, id))) = heap.pop() {
                if self.tag[id as usize] != epoch || self.cost[id as usize] != cost {
                    continue;
                }
                if tree.contains(&id) {
                    found = Some(id);
                    break;
                }
                self.tick(&mut search, "detailed", t.pin, net)?;
                let (l, t_, s_) = self.graph.locate(id);
                let layer = self.graph.layers[l].layer;
                let p = self.graph.xy(l, t_, s_);
                self.graph.neighbors(id, &mut nbuf);
                // On a layer whose single via pads are below AREA, a via may
                // leave only after a wire run of at least AREA / width since
                // the via that entered the layer (no pad-only islands).
                let no_stack = !self.stack_ok[layer as usize] && {
                    let need = self.min_run[layer as usize];
                    let mut run = 0u64;
                    let mut cur = id;
                    let mut steps = 0usize;
                    loop {
                        if cur == start {
                            break start_via && run < need;
                        }
                        if self.via_kind[cur as usize] != WIRE {
                            break run < need;
                        }
                        let prev = self.parent[cur as usize];
                        run += distance(self.graph.point(cur).1, self.graph.point(prev).1)?;
                        cur = prev;
                        steps += 1;
                        if run >= need || steps > 64 {
                            break false;
                        }
                    }
                };
                for &(next, mv) in nbuf.iter() {
                    if no_stack && matches!(mv, Move::Via(_)) {
                        continue;
                    }
                    let (nl, q) = self.graph.point(next);
                    let joins = tree.contains(&next);
                    if self.gmark[self.cells.of(q) as usize] != self.gepoch && !joins {
                        continue;
                    }
                    if joins
                        && matches!(mv, Move::Via(_))
                        && !self.stack_ok[nl as usize]
                        && via_nodes.contains(&next)
                    {
                        continue;
                    }
                    let mut step = match mv {
                        Move::Along => distance(p, q)?,
                        Move::Wrong => distance(p, q)?.saturating_mul(wrong),
                        Move::Via(_) => via_cost,
                    };
                    step = step.saturating_add(u64::from(self.history[next as usize]) * hist_unit);
                    let base = cost.saturating_add(step);
                    if self.tag[next as usize] == epoch && self.cost[next as usize] <= base {
                        continue;
                    }
                    let (kind, extra) = match mv {
                        Move::Via(pi) => {
                            // A wire that ends here on this layer wants the
                            // pad elongated along it (end-of-line enclosure).
                            let mut kinds = self.graph.pairs[pi].kinds.clone();
                            kinds.retain(|&k| !self.via_on_own_pin(k, p, &own));
                            if kinds.is_empty() {
                                continue;
                            }
                            if id != start && self.via_kind[id as usize] == WIRE {
                                let prev = self.graph.point(self.parent[id as usize]).1;
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
                            if own.iter().any(|(l, r, top)| {
                                *l == layer && overlap(rect, *r) && p != *top && q != *top
                            }) {
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
                    if self.tag[next as usize] == epoch && self.cost[next as usize] <= nc {
                        continue;
                    }
                    self.tag[next as usize] = epoch;
                    self.cost[next as usize] = nc;
                    self.parent[next as usize] = id;
                    self.via_kind[next as usize] = kind;
                    if heap.len() >= self.limits.max_heap {
                        return Err(budget("routing frontier limit"));
                    }
                    heap.push(Reverse((nc.saturating_add(heuristic(q)?), nc, next)));
                }
            }
            let Some(mut current) = found else {
                for id in out.shapes.drain(..) {
                    self.spatial.remove(id);
                }
                return Ok(Outcome::Failed(t.pin));
            };
            // Trace back, commit shapes so later sinks see this net's metal.
            let mut path = Vec::new();
            while self.parent[current as usize] != NONE {
                let prev = self.parent[current as usize];
                path.push((prev, current, self.via_kind[current as usize]));
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
                if out.edges.contains_key(&key) {
                    continue;
                }
                out.edges.insert(key, kind);
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
                    via_nodes.insert(a);
                    via_nodes.insert(b);
                    if soft
                        && let Soft::Clear { victims, .. } =
                            self.spatial.via_probe_soft(kind as usize, pa, net)?
                        && !victims.is_empty()
                    {
                        victims_all.extend(victims);
                        self.history[b as usize] = self.history[b as usize].saturating_add(1);
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
                    let rect = self.edge_rect(layer, pa, pb, net)?;
                    if soft
                        && let Soft::Clear { victims, .. } =
                            self.spatial.probe_soft(layer, rect, net)?
                        && !victims.is_empty()
                    {
                        victims_all.extend(victims);
                        for &n in &nodes {
                            self.history[n as usize] = self.history[n as usize].saturating_add(1);
                        }
                    }
                    out.shapes.push(self.spatial.add_routed(Blockage {
                        layer,
                        rect,
                        net: Some(net),
                    })?);
                }
            }
            tree.insert(start);
            if start_via {
                via_nodes.insert(start);
            }
        }
        victims_all.remove(&net);
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
    let estimate = memory_estimate(input, graph.len, cells.len())?;
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
    let mut r = Router {
        input,
        plan,
        limits,
        graph,
        cells,
        spatial: Spatial::new(input, limits)?,
        pairs,
        slots,
        expansions: 0,
        cost: vec![u64::MAX; n],
        parent: vec![NONE; n],
        via_kind: vec![WIRE; n],
        tag: vec![0; n],
        epoch: 0,
        history: vec![0; n],
        gmark: vec![0; gl],
        gepoch: 0,
        stack_ok: vec![true; input.layers.len()],
        min_run: vec![0; input.layers.len()],
    };
    for (i, l) in input.layers.iter().enumerate() {
        let Some(min) = l.min_area else {
            continue;
        };
        r.min_run[i] = u64::try_from(min)
            .unwrap_or(0)
            .div_ceil(l.width.max(1) as u64);
        r.stack_ok[i] = input.vias.iter().all(|v| {
            v.shapes.iter().filter(|s| s.layer == i as u32).all(|s| {
                i128::from(s.rect.ux - s.rect.lx) * i128::from(s.rect.uy - s.rect.ly)
                    >= i128::from(min)
            })
        });
    }
    r.guard("setup")?;
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
    let guides = r.global(&nets)?;
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
    // Detailed routing: small nets first.
    let mut order: Vec<(usize, u32)> = guides.iter().map(|(n, g)| (g.len(), *n)).collect();
    order.sort_unstable();
    let mut routed: BTreeMap<u32, NetRoute> = BTreeMap::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    let none = BTreeMap::new();
    for &(_, net) in &order {
        let g = &guides[&net];
        let mut done = false;
        for expand_by in [None, Some(r.plan.expand_margin)] {
            r.mark_region(g, expand_by);
            if let Outcome::Routed(route, _) =
                r.route_net(net, &groups[&net], &plans, false, false, &none, 0)?
            {
                routed.insert(net, route);
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
                routed.insert(net, route);
                continue;
            }
            let wide = r
                .plan
                .expand_margin
                .saturating_mul(2)
                .saturating_add(rips.get(&net).copied().unwrap_or(0).min(16));
            r.mark_region(g, Some(wide));
            match r.route_net(net, &groups[&net], &plans, true, tight, &rips, round)? {
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
                    routed.insert(net, route);
                    for v in victims {
                        let c = rips.entry(v).or_insert(0);
                        *c += 1;
                        if *c > r.plan.max_ripups_per_net {
                            return Err(no_route(&format!(
                                "net {v} ripped up more than {} times",
                                r.plan.max_ripups_per_net
                            )));
                        }
                        if let Some(old) = routed.remove(&v) {
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
            if let Some(old) = routed.remove(&net) {
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
    let mut wires: Vec<Wire> = Vec::new();
    let mut vias: Vec<Via> = Vec::new();
    let mut access: Vec<Access> = Vec::new();
    let mut nets_out = Vec::new();
    for (&net, terminals) in groups {
        nets_out.push(net);
        for t in terminals {
            let p = &plans[&t.pin];
            access.push(p.access.clone());
            wires.extend(p.wires.iter().cloned());
            vias.extend(p.vias.iter().cloned());
        }
        let route = routed
            .get(&net)
            .ok_or_else(|| no_route(&format!("net {net} left unrouted")))?;
        for (&(a, b), &kind) in &route.edges {
            let (la, pa) = r.graph.point(a);
            let (lb, pb) = r.graph.point(b);
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
                vias.push(Via { net, kind, at: pa });
            }
        }
    }
    vias.sort_by_key(|v| (v.net, v.kind, v.at));
    vias.dedup();
    let runs = crate::wire_runs::collinear_runs(
        &wires.iter().collect::<Vec<_>>(),
        &vias.iter().collect::<Vec<_>>(),
        via_layers,
        &access.iter().collect::<Vec<_>>(),
        limits.max_comparisons,
    )?;
    if runs.len() + vias.len() > limits.max_segments {
        return Err(budget("routing output segment limit"));
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
            memory_mib: 64,
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
            held_bytes: 0,
        }
    }
    fn limits() -> RouteLimits {
        RouteLimits::default()
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
        // Two along-track, two wrong-way, via down and via up.
        assert_eq!(n.len(), 6);
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
        // The runtime guard: an estimate below a 1 MiB cap still stops once
        // the process' peak resident set (any test process) is above it.
        if crate::memory::peak_rss_bytes().is_some() {
            let mut small = input(i.terminals.clone(), Vec::new());
            small.tracks.as_mut().unwrap().memory_mib = 1;
            assert!(estimate_for(&small, &limits()).unwrap().unwrap().total() < 1 << 20);
            let err = route_cpu_ref(&small, &limits()).unwrap_err();
            assert!(err.to_string().contains("peak resident set"), "{err}");
        }
        let e = memory_estimate(&i, 1_000_000, 10_000).unwrap();
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
}
