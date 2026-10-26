//! Deterministic, bounded rectilinear routing on a finite coordinate lattice.
//! This compute view is separate from mutable netlist arenas. All geometry is
//! integer DBU. Resource preflight: docs/routing-budget.md.
//!
//! With `RouteInput::rules` set, the router also uses every supplied fixed-via
//! variant per layer pair, a per-(master, orientation, pin) access table and a
//! bounded post-route same-net pass (dangling-stub pruning, notch patches and
//! minimum-area stubs, see `route_rules`). Without it, behavior is unchanged.
pub use crate::instance_obstacles::{
    InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
};
use crate::route_rules;
pub(crate) use crate::route_rules::Violation;
use crate::{Error, Rect, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLimits {
    pub max_axis: usize,
    pub max_grid_nodes: usize,
    pub max_obstacles: usize,
    pub max_bin_entries: usize,
    pub max_segments: usize,
    pub max_expansions: usize,
    pub max_search_expansions: usize,
    /// Optional earlier handoff from hard search to bounded routing repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hard_search_expansions: Option<usize>,
    /// Bounded retries after two vias on a newly traced path conflict.
    #[serde(default = "default_path_retries")]
    pub max_path_retries: usize,
    /// Nodes or raw edges retained for one detailed net, independently of the design output size.
    #[serde(default = "default_active_nodes")]
    pub max_active_nodes: usize,
    /// Exact routing-tree segments retained by the bounded distance index.
    #[serde(default = "default_target_segments")]
    pub max_target_segments: usize,
    /// Retries that may reopen earlier nets after a later pin loses access.
    #[serde(default = "default_batch_retries")]
    pub max_batch_retries: usize,
    /// Blocked pin regions collected before returning one failed reservation pass.
    #[serde(default = "default_batch_blocked_pins")]
    pub max_batch_blocked_pins: usize,
    /// Distinct completed nets reopened during one batch.
    #[serde(default = "default_batch_displaced_nets")]
    pub max_batch_displaced_nets: usize,
    /// Total terminals, including reopened nets, admitted to a repair batch.
    #[serde(default = "default_batch_repair_terminals")]
    pub max_batch_repair_terminals: usize,
    /// Retained same-net violations per check; exceeding the limit is an error.
    #[serde(default = "default_rule_violations")]
    pub max_rule_violations: usize,
    /// Cells in one coordinate-compressed hole-detection grid.
    #[serde(default = "default_rule_grid_cells")]
    pub max_rule_grid_cells: usize,
    pub max_comparisons: usize,
    pub max_heap: usize,
}
fn default_rule_grid_cells() -> usize {
    1_000_000
}

fn default_rule_violations() -> usize {
    100_000
}

fn default_active_nodes() -> usize {
    200_000
}
fn default_path_retries() -> usize {
    64
}
fn default_target_segments() -> usize {
    8192
}
fn default_batch_retries() -> usize {
    16
}
fn default_batch_blocked_pins() -> usize {
    64
}
fn default_batch_displaced_nets() -> usize {
    1024
}
fn default_batch_repair_terminals() -> usize {
    1_000_000
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_axis: 1024,
            max_grid_nodes: 2_000_000,
            max_obstacles: 200_000,
            max_bin_entries: 1_000_000,
            max_segments: 100_000,
            max_expansions: 2_000_000,
            max_search_expansions: 200_000,
            max_hard_search_expansions: None,
            max_path_retries: default_path_retries(),
            max_active_nodes: default_active_nodes(),
            max_target_segments: default_target_segments(),
            max_batch_retries: default_batch_retries(),
            max_batch_blocked_pins: default_batch_blocked_pins(),
            max_batch_displaced_nets: default_batch_displaced_nets(),
            max_batch_repair_terminals: default_batch_repair_terminals(),
            max_rule_violations: default_rule_violations(),
            max_rule_grid_cells: default_rule_grid_cells(),
            max_comparisons: 200_000_000,
            max_heap: 500_000,
        }
    }
}
impl RouteLimits {
    /// Absolute ceilings for the track router (`RouteInput::tracks`). The
    /// lattice router keeps its defaults as hard caps. The router also checks
    /// its upfront memory estimate against `TrackPlan::memory_mib`.
    pub fn ceiling() -> Self {
        Self {
            max_axis: 1 << 20,
            max_grid_nodes: u32::MAX as usize - 1,
            max_obstacles: 256_000_000,
            max_bin_entries: 512_000_000,
            max_segments: 128_000_000,
            max_expansions: 2_000_000_000,
            max_search_expansions: 50_000_000,
            max_hard_search_expansions: None,
            max_path_retries: 4096,
            max_active_nodes: 50_000_000,
            max_target_segments: 1_000_000,
            max_batch_retries: 128,
            max_batch_blocked_pins: 4096,
            max_batch_displaced_nets: 4096,
            max_batch_repair_terminals: 1_000_000,
            max_rule_violations: 1_000_000,
            max_rule_grid_cells: 4_000_000,
            max_comparisons: 1usize << 40,
            max_heap: 64_000_000,
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_against(&Self::default())
    }
    /// Track-mode bounds: every limit must be positive and at most `ceiling()`.
    pub fn validate_track(&self) -> Result<()> {
        self.validate_against(&Self::ceiling())
    }
    fn validate_against(&self, hard: &Self) -> Result<()> {
        if self.max_batch_retries > hard.max_batch_retries {
            return Err(budget("batch retry limit outside hard bounds"));
        }
        if self.max_path_retries > hard.max_path_retries {
            return Err(budget("path retry limit outside hard bounds"));
        }
        if self
            .max_hard_search_expansions
            .is_some_and(|n| n == 0 || n > self.max_search_expansions)
        {
            return Err(budget("hard search limit exceeds per-search bounds"));
        }
        for (a, b) in [
            (self.max_axis, hard.max_axis),
            (self.max_grid_nodes, hard.max_grid_nodes),
            (self.max_obstacles, hard.max_obstacles),
            (self.max_bin_entries, hard.max_bin_entries),
            (self.max_segments, hard.max_segments),
            (self.max_expansions, hard.max_expansions),
            (self.max_search_expansions, hard.max_search_expansions),
            (self.max_active_nodes, hard.max_active_nodes),
            (self.max_target_segments, hard.max_target_segments),
            (self.max_batch_blocked_pins, hard.max_batch_blocked_pins),
            (self.max_batch_displaced_nets, hard.max_batch_displaced_nets),
            (
                self.max_batch_repair_terminals,
                hard.max_batch_repair_terminals,
            ),
            (self.max_rule_violations, hard.max_rule_violations),
            (self.max_rule_grid_cells, hard.max_rule_grid_cells),
            (self.max_comparisons, hard.max_comparisons),
            (self.max_heap, hard.max_heap),
        ] {
            if a == 0 || a > b {
                return Err(budget("routing limit outside hard bounds"));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLayer {
    pub name: String,
    pub width: i64,
    pub spacing: i64,
    /// None is a cut layer; true is preferred vertical, false horizontal.
    pub vertical: Option<bool>,
    /// LEF AREA in DBU^2 (rules mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area: Option<i64>,
    /// LEF MINSTEP in DBU (rules mode, pin-access landing pads only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_step: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerRect {
    pub layer: u32,
    pub rect: Rect,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteVia {
    pub name: String,
    pub lower: u32,
    pub upper: u32,
    pub shapes: Vec<LayerRect>,
}
#[derive(Clone, Debug)]
pub struct Blockage {
    pub layer: u32,
    pub rect: Rect,
    pub net: Option<u32>,
}
#[derive(Clone, Debug)]
pub struct Terminal {
    pub pin: u32,
    pub net: u32,
    pub driver: bool,
    pub shapes: Vec<LayerRect>,
    /// Access-class index and instance origin; shapes = class shapes + origin.
    pub class: Option<(u32, [i64; 2])>,
}
/// One (master, orientation, pin) access geometry relative to the origin.
#[derive(Clone, Debug)]
pub struct AccessClass {
    pub shapes: Vec<LayerRect>,
    /// Other-net geometry of the same master (other pins, OBS) on selected layers.
    pub blockers: Vec<LayerRect>,
}
/// Per-layer track grid and router parameters of the track router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackPlan {
    /// Indexed like `RouteInput::layers`: `(pitch, offset)` of the cross-axis
    /// track coordinates (vertical layer: x = offset + k * pitch); `None` for
    /// cut layers.
    pub tracks: Vec<Option<(i64, i64)>>,
    /// Square GCell side in DBU for global routing.
    pub gcell: i64,
    pub global_iterations: usize,
    /// Fraction (percent) of free tracks a GCell edge offers global routing.
    pub capacity_percent: u32,
    /// GCells added around every global route when building guides.
    pub guide_margin: usize,
    /// Extra GCells for the second detailed attempt outside the guide.
    pub expand_margin: usize,
    pub max_ripups_per_net: usize,
    pub max_repair_rounds: usize,
    /// Path cost of one via, DBU-equivalent.
    pub via_cost: u64,
    /// Cost multiplier of non-preferred-direction wire edges.
    pub wrong_way_factor: u64,
    /// Integer multiplier of the detailed-search lower bound (1..=16).
    /// Values above one favor progress over shortest-path optimality.
    pub heuristic_weight: u64,
    /// Upfront memory estimate ceiling for graph, guides and search state.
    pub memory_mib: u64,
    /// Separate cap for cumulative congestion-history entries.
    pub max_history_entries: usize,
}
pub const DEFAULT_MAX_HISTORY_ENTRIES: usize = 4_000_000;
/// Process baseline counted in every route memory estimate (binary, libc,
/// stacks, thread pool).
pub const PROCESS_BASE_BYTES: u64 = 8 << 20;
/// Conservative pre-load allowance per on-disk workspace byte. This is also
/// the fallback on platforms without a process-footprint probe.
pub const WORKSPACE_LOAD_FACTOR: u64 = 3;

/// Bytes the caller holds while the router runs: the process baseline, the
/// loaded workspace (`workspace_bytes` on disk) and this input, counted
/// twice (vector growth and the builder's transient lookup tables). Used
/// before loading or when the platform cannot report the live footprint.
pub fn held_estimate(workspace_bytes: u64, input: &RouteInput) -> u64 {
    PROCESS_BASE_BYTES
        .saturating_add(workspace_bytes.saturating_mul(WORKSPACE_LOAD_FACTOR))
        .saturating_add(input_heap_estimate(input).saturating_mul(2))
}

/// After the complete input has been decoded, account for the live process
/// rather than charging it for already-freed builder maps and metadata clones.
/// Add 25% allocator/page headroom; never fall below the input's structural
/// allowance. Callers must include swapped pages in `live_process_bytes`.
/// Future router allocations are separately covered by MemoryEstimate.
pub fn loaded_held_estimate(
    workspace_bytes: u64,
    input: &RouteInput,
    live_process_bytes: Option<u64>,
) -> u64 {
    match live_process_bytes.filter(|&n| n > 0) {
        Some(bytes) => bytes
            .saturating_add(bytes.div_ceil(4))
            .max(PROCESS_BASE_BYTES.saturating_add(input_heap_estimate(input).saturating_mul(2))),
        None => held_estimate(workspace_bytes, input),
    }
}

fn input_heap_estimate(input: &RouteInput) -> u64 {
    use std::mem::size_of;
    let shapes = |v: &[LayerRect]| std::mem::size_of_val(v) as u64;
    let terminals: u64 = input
        .terminals
        .iter()
        .map(|t| size_of::<Terminal>() as u64 + shapes(&t.shapes))
        .sum();
    let classes: u64 = input
        .access_classes
        .iter()
        .map(|c| size_of::<AccessClass>() as u64 + shapes(&c.shapes) + shapes(&c.blockers))
        .sum();
    // Obstacles plus the caller's parallel owner table (8 bytes each).
    let obstacles = (input.obstacles.len() * (size_of::<Blockage>() + 8)) as u64;
    let ties = (input.ties.len() * size_of::<SupplyTie>()) as u64;
    let instances = input
        .instances
        .as_ref()
        .map_or(0, InstanceObstacles::heap_bytes);
    obstacles
        .saturating_add(terminals)
        .saturating_add(classes)
        .saturating_add(ties)
        .saturating_add(instances)
}

/// Supply tie (rail-input tie cells): router net `net` holds a cell input
/// pin and a pseudo-driver on the cell's own rail rectangle `zone`; it is
/// not a logical net. A routed shape of `net` that overlaps `zone` may touch
/// (never merely approach) fixed shapes of the supply net `supply`.
#[derive(Clone, Debug)]
pub struct SupplyTie {
    pub net: u32,
    pub supply: u32,
    pub zone: LayerRect,
}
pub struct RouteInput {
    pub die: Rect,
    pub grid: i64,
    pub step: i64,
    pub access_layer: u32,
    pub layers: Vec<RouteLayer>,
    pub vias: Vec<RouteVia>,
    pub obstacles: Vec<Blockage>,
    /// Shared cell templates and connectivity; translated only during queries.
    pub instances: Option<InstanceObstacles>,
    pub terminals: Vec<Terminal>,
    /// Enable LEF-rule mode (variants, access table, same-net cleanup/checks).
    pub rules: bool,
    pub access_classes: Vec<AccessClass>,
    /// Track router (per-layer track graph, global guides, rip-up and
    /// reroute). `None` keeps the lattice router.
    pub tracks: Option<TrackPlan>,
    /// Supply ties (track router only), sorted by `net`.
    pub ties: Vec<SupplyTie>,
    /// Bytes the caller already holds for this route (loaded workspace and
    /// this input), counted with the router's own estimate against the
    /// track plan's `memory_mib`.
    pub held_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wire {
    pub net: u32,
    pub layer: u32,
    pub start: [i64; 2],
    pub end: [i64; 2],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Via {
    pub net: u32,
    pub kind: u32,
    pub at: [i64; 2],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub pin: u32,
    pub net: u32,
    pub layer: u32,
    pub at: [i64; 2],
}
/// Same-net fill metal (DEF routing RECT); rules mode only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub net: u32,
    pub layer: u32,
    pub rect: Rect,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStats {
    pub grid_nodes: usize,
    pub expansions: usize,
    pub comparisons: usize,
    pub wire_length_dbu: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    pub wires: Vec<Wire>,
    pub vias: Vec<Via>,
    pub access: Vec<Access>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<Patch>,
    pub nets: Vec<u32>,
    pub stats: RouteStats,
}
pub(crate) fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
pub(crate) fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
pub(crate) fn no_route(s: &str) -> Error {
    Error::Invalid(format!("NO_ROUTE: {s}"))
}
pub(crate) fn int(v: i128) -> Result<i64> {
    i64::try_from(v).map_err(|_| invalid("routing coordinate overflow"))
}
pub(crate) fn expand(r: Rect, amount: i64) -> Result<Rect> {
    Ok(Rect {
        lx: int(i128::from(r.lx) - i128::from(amount))?,
        ly: int(i128::from(r.ly) - i128::from(amount))?,
        ux: int(i128::from(r.ux) + i128::from(amount))?,
        uy: int(i128::from(r.uy) + i128::from(amount))?,
    })
}
pub fn wire_rect(w: &Wire, width: i64) -> Result<Rect> {
    if width <= 0
        || width % 2 != 0
        || w.start == w.end
        || (w.start[0] != w.end[0] && w.start[1] != w.end[1])
    {
        return Err(invalid("non-Manhattan/zero wire or odd width"));
    }
    let h = i128::from(width / 2);
    Ok(Rect {
        lx: int(i128::from(w.start[0].min(w.end[0])) - h)?,
        ux: int(i128::from(w.start[0].max(w.end[0])) + h)?,
        ly: int(i128::from(w.start[1].min(w.end[1])) - h)?,
        uy: int(i128::from(w.start[1].max(w.end[1])) + h)?,
    })
}
pub fn shifted(r: Rect, at: [i64; 2]) -> Result<Rect> {
    Ok(Rect {
        lx: int(i128::from(r.lx) + i128::from(at[0]))?,
        ly: int(i128::from(r.ly) + i128::from(at[1]))?,
        ux: int(i128::from(r.ux) + i128::from(at[0]))?,
        uy: int(i128::from(r.uy) + i128::from(at[1]))?,
    })
}
pub(crate) fn contains(r: Rect, p: [i64; 2]) -> bool {
    r.lx <= p[0] && p[0] <= r.ux && r.ly <= p[1] && p[1] <= r.uy
}
pub(crate) fn overlap(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

/// Deterministic multiplicative hasher for spatial bin keys (lookups only;
/// no iteration order depends on it).
#[derive(Clone, Copy, Default)]
pub(crate) struct BuildFx;
pub(crate) struct Fx(u64);
impl std::hash::BuildHasher for BuildFx {
    type Hasher = Fx;
    fn build_hasher(&self) -> Fx {
        Fx(0)
    }
}
impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 =
                (self.0.rotate_left(5) ^ u64::from(*b)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(v)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_i64(&mut self, v: i64) {
        self.0 = (self.0.rotate_left(5) ^ v as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}
const NIL: u32 = u32::MAX;
pub(crate) const BIN_PAGE_SIDE: i64 = 32;
const BIN_PAGE_CELLS: usize = (BIN_PAGE_SIDE * BIN_PAGE_SIDE) as usize;
type BinPageKey = (u32, i64, i64);
type BinPage = Box<[[u32; 2]; BIN_PAGE_CELLS]>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinMode {
    Dense,
    Paged,
    Sparse,
}
pub(crate) fn bin_mode(input: &RouteInput, cells: u64) -> BinMode {
    if cells.saturating_mul(input.layers.len() as u64) <= 1_000_000 {
        BinMode::Dense
    } else if input.obstacles.len() + input.instances.as_ref().map_or(0, InstanceObstacles::len)
        >= 1024
    {
        BinMode::Paged
    } else {
        BinMode::Sparse
    }
}
/// Spatial bins: one dense head/tail grid per layer over the die and every
/// fixed shape, entries chained in insertion order (so iteration order, and
/// with it every work count, matches a per-bin list). A layer's grid is
/// allocated on its first shape: 8 bytes per bin plus 8 bytes per entry.
/// Bins outside the grid (never produced by router inputs) use a hash map.
struct BinGrid {
    mode: BinMode,
    bx0: i64,
    by0: i64,
    nx: i64,
    ny: i64,
    /// Per layer: `[head, tail]` entry index per bin, or empty.
    cells: Vec<Vec<[u32; 2]>>,
    /// `[shape id, next entry]`.
    entries: Vec<[u32; 2]>,
    pages: HashMap<BinPageKey, BinPage, BuildFx>,
    overflow: HashMap<(u32, i64, i64), Vec<usize>, BuildFx>,
}
enum BinIter<'g> {
    Chain { entries: &'g [[u32; 2]], at: u32 },
    List(std::slice::Iter<'g, usize>),
}
impl Iterator for BinIter<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        match self {
            BinIter::Chain { entries, at } => {
                if *at == NIL {
                    return None;
                }
                let [id, next] = entries[*at as usize];
                *at = next;
                Some(id as usize)
            }
            BinIter::List(it) => it.next().copied(),
        }
    }
}
impl BinGrid {
    fn slot(&self, x: i64, y: i64) -> Option<usize> {
        let (dx, dy) = (x - self.bx0, y - self.by0);
        (dx >= 0 && dy >= 0 && dx < self.nx && dy < self.ny).then(|| (dy * self.nx + dx) as usize)
    }
    fn push(&mut self, layer: u32, x: i64, y: i64, id: usize) -> Result<()> {
        if self.mode == BinMode::Sparse
            || (self.mode == BinMode::Dense && self.slot(x, y).is_none())
        {
            self.overflow
                .try_reserve(1)
                .map_err(|_| budget("sparse routing bin allocation"))?;
            let bin = self.overflow.entry((layer, x, y)).or_default();
            bin.try_reserve(1)
                .map_err(|_| budget("sparse routing bin entries"))?;
            bin.push(id);
            return Ok(());
        }
        let e = u32::try_from(self.entries.len())
            .ok()
            .filter(|&e| e != NIL)
            .ok_or_else(|| budget("routing spatial-bin count"))?;
        let id = u32::try_from(id).map_err(|_| budget("routing obstacle count"))?;
        let cell = if self.mode == BinMode::Paged {
            let key = (
                layer,
                x.div_euclid(BIN_PAGE_SIDE),
                y.div_euclid(BIN_PAGE_SIDE),
            );
            self.pages
                .try_reserve(1)
                .map_err(|_| budget("routing bin-page allocation"))?;
            let page = self
                .pages
                .entry(key)
                .or_insert_with(|| Box::new([[NIL, NIL]; BIN_PAGE_CELLS]));
            let slot = (y.rem_euclid(BIN_PAGE_SIDE) * BIN_PAGE_SIDE + x.rem_euclid(BIN_PAGE_SIDE))
                as usize;
            &mut page[slot]
        } else {
            let k = self.slot(x, y).expect("dense bin is inside the grid");
            let grid = &mut self.cells[layer as usize];
            if grid.is_empty() {
                grid.resize((self.nx * self.ny) as usize, [NIL, NIL]);
            }
            &mut grid[k]
        };
        if cell[1] == NIL {
            cell[0] = e;
        } else {
            self.entries[cell[1] as usize][1] = e;
        }
        cell[1] = e;
        self.entries.push([id, NIL]);
        Ok(())
    }
    fn iter(&self, layer: u32, x: i64, y: i64) -> BinIter<'_> {
        if self.mode == BinMode::Paged {
            let key = (
                layer,
                x.div_euclid(BIN_PAGE_SIDE),
                y.div_euclid(BIN_PAGE_SIDE),
            );
            let slot = (y.rem_euclid(BIN_PAGE_SIDE) * BIN_PAGE_SIDE + x.rem_euclid(BIN_PAGE_SIDE))
                as usize;
            return BinIter::Chain {
                entries: &self.entries,
                at: self.pages.get(&key).map_or(NIL, |p| p[slot][0]),
            };
        }
        match self.slot(x, y).filter(|_| self.mode == BinMode::Dense) {
            Some(k) => BinIter::Chain {
                entries: &self.entries,
                at: self.cells[layer as usize].get(k).map_or(NIL, |c| c[0]),
            },
            None => BinIter::List(
                self.overflow
                    .get(&(layer, x, y))
                    .map_or([].iter(), |v| v.iter()),
            ),
        }
    }
    /// Grid bins allocated over all layers.
    fn allocated(&self) -> usize {
        self.cells.iter().map(Vec::len).sum::<usize>()
            + self.overflow.len()
            + self.pages.len() * BIN_PAGE_CELLS
    }
}
/// Bin index range `[x0, x1] x [y0, y1]` covering the die and every fixed
/// obstacle, and the bins one layer's grid holds.
pub(crate) fn bin_extent(input: &RouteInput, bin: i64) -> (i64, i64, i64, i64, u64) {
    let mut r = input.die;
    for b in &input.obstacles {
        r.lx = r.lx.min(b.rect.lx);
        r.ly = r.ly.min(b.rect.ly);
        r.ux = r.ux.max(b.rect.ux);
        r.uy = r.uy.max(b.rect.uy);
    }
    let (x0, x1, y0, y1) = (
        r.lx.div_euclid(bin),
        r.ux.div_euclid(bin),
        r.ly.div_euclid(bin),
        r.uy.div_euclid(bin),
    );
    let cells = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
    (x0, x1, y0, y1, u64::try_from(cells).unwrap_or(u64::MAX))
}
/// Spatial bin side of the router (track mode: two access steps).
pub(crate) fn bin_side(input: &RouteInput) -> Result<i64> {
    input
        .step
        .checked_mul(if input.tracks.is_some() { 2 } else { 8 })
        .filter(|&side| side > 0)
        .ok_or_else(|| invalid("bin overflow"))
}

pub(crate) struct InstanceIndex<'a> {
    source: &'a InstanceObstacles,
    bin: i64,
    bins: BinGrid,
    marks: Vec<u64>,
    generation: u64,
    entries: usize,
}

fn instance_extent(die: Rect, bin: i64, source: &InstanceObstacles) -> Result<(i64, Rect, usize)> {
    if bin <= 0 {
        return Err(invalid("instance bin size"));
    }
    let mut bounds = die;
    let mut entries = 0usize;
    for i in 0..source.len() {
        if let Some(r) = source.bounds(i)? {
            bounds.lx = bounds.lx.min(r.lx);
            bounds.ly = bounds.ly.min(r.ly);
            bounds.ux = bounds.ux.max(r.ux);
            bounds.uy = bounds.uy.max(r.uy);
            let nx = i128::from(r.ux.div_euclid(bin)) - i128::from(r.lx.div_euclid(bin)) + 1;
            let ny = i128::from(r.uy.div_euclid(bin)) - i128::from(r.ly.div_euclid(bin)) + 1;
            let n = usize::try_from(nx * ny).map_err(|_| budget("instance bin count"))?;
            entries = entries
                .checked_add(n)
                .ok_or_else(|| budget("instance bin count"))?;
        }
    }
    Ok((bin, bounds, entries))
}

pub(crate) fn instance_index_bytes(input: &RouteInput, limits: &RouteLimits) -> Result<u64> {
    let Some(source) = &input.instances else {
        return Ok(0);
    };
    instance_geometry_bytes(
        source,
        input.die,
        input
            .step
            .checked_mul(16)
            .ok_or_else(|| budget("instance bin size"))?,
        input.layers.len(),
        limits,
    )
}

pub(crate) fn instance_geometry_bytes(
    source: &InstanceObstacles,
    die: Rect,
    bin: i64,
    layer_count: usize,
    limits: &RouteLimits,
) -> Result<u64> {
    source.validate(layer_count, limits)?;
    let (bin, b, entries) = instance_extent(die, bin, source)?;
    if entries > limits.max_bin_entries {
        return Err(budget("instance bin-entry limit"));
    }
    let (x0, x1, y0, y1) = (
        b.lx.div_euclid(bin),
        b.ux.div_euclid(bin),
        b.ly.div_euclid(bin),
        b.uy.div_euclid(bin),
    );
    let slots = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
    let pages = (i128::from(x1.div_euclid(BIN_PAGE_SIDE))
        - i128::from(x0.div_euclid(BIN_PAGE_SIDE))
        + 1)
        * (i128::from(y1.div_euclid(BIN_PAGE_SIDE)) - i128::from(y0.div_euclid(BIN_PAGE_SIDE)) + 1);
    let grid = if slots <= 1_000_000 {
        slots as u64 * 8
    } else if source.len() >= 1024 {
        u64::try_from(pages)
            .unwrap_or(u64::MAX)
            .min(entries as u64)
            .saturating_mul(8320)
    } else {
        u64::try_from(slots)
            .unwrap_or(u64::MAX)
            .min(entries as u64)
            .saturating_mul(96)
    };
    Ok(grid
        .saturating_add(entries as u64 * 16)
        .saturating_add(source.len() as u64 * 8))
}

impl<'a> InstanceIndex<'a> {
    fn new(
        input: &RouteInput,
        source: &'a InstanceObstacles,
        limits: &RouteLimits,
    ) -> Result<Self> {
        Self::for_geometry(
            source,
            input.die,
            input
                .step
                .checked_mul(16)
                .ok_or_else(|| budget("instance bin size"))?,
            input.layers.len(),
            limits,
        )
    }
    pub(crate) fn for_geometry(
        source: &'a InstanceObstacles,
        die: Rect,
        bin: i64,
        layer_count: usize,
        limits: &RouteLimits,
    ) -> Result<Self> {
        source.validate(layer_count, limits)?;
        let (bin, b, entries) = instance_extent(die, bin, source)?;
        if entries > limits.max_bin_entries {
            return Err(budget("instance bin-entry limit"));
        }
        let (x0, x1, y0, y1) = (
            b.lx.div_euclid(bin),
            b.ux.div_euclid(bin),
            b.ly.div_euclid(bin),
            b.uy.div_euclid(bin),
        );
        let cells = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        let mode = if cells <= 1_000_000 {
            BinMode::Dense
        } else if source.len() >= 1024 {
            BinMode::Paged
        } else {
            BinMode::Sparse
        };
        let mut bins = BinGrid {
            mode,
            bx0: x0,
            by0: y0,
            nx: x1 - x0 + 1,
            ny: y1 - y0 + 1,
            cells: vec![Vec::new()],
            entries: Vec::new(),
            pages: HashMap::with_hasher(BuildFx),
            overflow: HashMap::with_hasher(BuildFx),
        };
        for i in 0..source.len() {
            if let Some(r) = source.bounds(i)? {
                for x in r.lx.div_euclid(bin)..=r.ux.div_euclid(bin) {
                    for y in r.ly.div_euclid(bin)..=r.uy.div_euclid(bin) {
                        bins.push(0, x, y, i)?;
                    }
                }
            }
        }
        let mut marks = Vec::new();
        marks
            .try_reserve_exact(source.len())
            .map_err(|_| budget("instance query-mark allocation"))?;
        marks.resize(source.len(), 0);
        Ok(Self {
            source,
            bin,
            bins,
            marks,
            generation: 0,
            entries,
        })
    }
    fn visit(
        &mut self,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        mut visitor: impl FnMut(Blockage) -> Result<bool>,
    ) -> Result<bool> {
        let source = self.source;
        self.visit_cells(layer, query, work, limits, |i, work| {
            source.visit_near(
                i,
                layer,
                query,
                || {
                    *work = work
                        .checked_add(1)
                        .filter(|&n| n <= limits.max_comparisons)
                        .ok_or_else(|| budget("instance geometry work"))?;
                    Ok(())
                },
                |_, b, _| visitor(b),
            )
        })
    }
    pub(crate) fn visit_cells(
        &mut self,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        mut visitor: impl FnMut(usize, &mut usize) -> Result<bool>,
    ) -> Result<bool> {
        if !self.source.has_layer(layer) {
            return Ok(true);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| budget("instance query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let extent = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if extent > limits.max_bin_entries as i128 {
            return Err(budget("instance query extent"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                *work = work
                    .checked_add(1)
                    .filter(|&n| n <= limits.max_comparisons)
                    .ok_or_else(|| budget("instance query work"))?;
                for i in self.bins.iter(0, x, y) {
                    if self.marks[i] == self.generation {
                        continue;
                    }
                    self.marks[i] = self.generation;
                    *work = work
                        .checked_add(1)
                        .filter(|&n| n <= limits.max_comparisons)
                        .ok_or_else(|| budget("instance query work"))?;
                    if !self.source.bounds(i)?.is_some_and(|b| overlap(query, b)) {
                        continue;
                    }
                    let clear = visitor(i, work)?;
                    if !clear {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

pub(crate) struct Spatial<'a> {
    pub(crate) input: &'a RouteInput,
    limits: &'a RouteLimits,
    bin: i64,
    /// Added geometry only; immutable fixed shapes remain in `input.obstacles`.
    shapes: Vec<Blockage>,
    bins: BinGrid,
    marks: Vec<u64>,
    generation: u64,
    entries: usize,
    pub(crate) work: usize,
    /// False once removed by rip-up (track router); ids stay in their bins.
    alive: Vec<bool>,
    /// True for rip-up-able routed shapes (track router); obstacles, pins and
    /// reserved pin access are fixed.
    routed: Vec<bool>,
    instances: Option<InstanceIndex<'a>>,
    /// Supply ties by router net: (supply net, rail zone).
    ties: HashMap<u32, (u32, LayerRect)>,
}
/// Result of a soft probe: fixed blockage, or clear except for routed shapes
/// of the listed other nets (sorted, deduplicated).
pub(crate) enum Soft {
    Blocked,
    Clear { near: bool, victims: Vec<u32> },
}
fn same_net_shapes(
    ties: &HashMap<u32, (u32, LayerRect)>,
    net: u32,
    layer: u32,
    rect: Rect,
    b: &Blockage,
) -> bool {
    if b.net == Some(net) {
        return true;
    }
    match (ties.get(&net), b.net) {
        (Some((supply, zone)), Some(n)) if n == *supply => {
            zone.layer == layer
                && overlap(rect, zone.rect)
                && route_rules::relation(rect, b.rect, 0) == route_rules::Relation::Connected
        }
        _ => false,
    }
}

impl<'a> Spatial<'a> {
    pub(crate) fn new(input: &'a RouteInput, limits: &'a RouteLimits) -> Result<Self> {
        if input
            .obstacles
            .len()
            .saturating_add(input.instances.as_ref().map_or(0, InstanceObstacles::len))
            > limits.max_obstacles
        {
            return Err(budget("routing obstacle count"));
        }
        let instances = input
            .instances
            .as_ref()
            .map(|s| InstanceIndex::new(input, s, limits))
            .transpose()?;
        let instance_entries = instances.as_ref().map_or(0, |i| i.entries);
        // The track router queries far more often with short edges; it
        // uses bins of two access steps (the lattice router keeps eight).
        let bin = bin_side(input)?;
        let (x0, x1, y0, y1, cells) = bin_extent(input, bin);
        let mut s = Self {
            input,
            limits,
            bin,
            shapes: Vec::new(),
            bins: BinGrid {
                mode: bin_mode(input, cells),
                bx0: x0,
                by0: y0,
                nx: x1 - x0 + 1,
                ny: y1 - y0 + 1,
                cells: vec![Vec::new(); input.layers.len()],
                entries: Vec::new(),
                pages: HashMap::with_hasher(BuildFx),
                overflow: HashMap::with_hasher(BuildFx),
            },
            marks: vec![0; input.obstacles.len()],
            generation: 0,
            entries: instance_entries,
            work: 0,
            alive: Vec::new(),
            routed: Vec::new(),
            instances,
            ties: input
                .ties
                .iter()
                .map(|t| (t.net, (t.supply, t.zone.clone())))
                .collect(),
        };
        for (id, b) in input.obstacles.iter().enumerate() {
            s.index(id, b)?;
        }
        Ok(s)
    }
    /// Is stored shape `b` the query net's own metal? A supply tie also owns
    /// supply shapes that its query touches inside its rail zone.
    fn same_net(&self, net: u32, layer: u32, rect: Rect, b: &Blockage) -> bool {
        same_net_shapes(&self.ties, net, layer, rect, b)
    }
    fn probe_instances(
        &mut self,
        layer: u32,
        rect: Rect,
        query: Rect,
        net: u32,
    ) -> Result<Option<bool>> {
        let Some(instances) = &mut self.instances else {
            return Ok(Some(false));
        };
        let ties = &self.ties;
        let routing = self.input.layers[layer as usize].vertical.is_some();
        let rules = self.input.rules;
        let spacing = self.input.layers[layer as usize].spacing;
        let mut near = false;
        let clear = instances.visit(layer, query, &mut self.work, self.limits, |b| {
            if !overlap(query, b.rect) {
                return Ok(true);
            }
            let shared = same_net_shapes(ties, net, layer, rect, &b) && (routing || rect == b.rect);
            if !shared {
                return Ok(false);
            }
            if rules
                && routing
                && route_rules::relation(rect, b.rect, spacing) == route_rules::Relation::Near
            {
                near = true;
            }
            Ok(true)
        })?;
        Ok(clear.then_some(near))
    }
    /// Add a rip-up-able routed shape; returns its id.
    pub(crate) fn add_routed(&mut self, b: Blockage) -> Result<usize> {
        let added = self.shapes.len();
        self.add(b)?;
        let id = self.len() - 1;
        self.routed[added] = true;
        Ok(id)
    }
    /// Add a fixed shape; returns its id.
    pub(crate) fn add_fixed(&mut self, b: Blockage) -> Result<usize> {
        self.add(b)?;
        Ok(self.len() - 1)
    }
    pub(crate) fn remove(&mut self, id: usize) {
        self.alive[id - self.input.obstacles.len()] = false;
    }
    pub(crate) fn restore(&mut self, id: usize) -> Result<()> {
        let offset = id
            .checked_sub(self.input.obstacles.len())
            .ok_or_else(|| invalid("cannot restore an input obstacle"))?;
        let alive = self
            .alive
            .get_mut(offset)
            .ok_or_else(|| invalid("invalid restored obstacle"))?;
        *alive = true;
        Ok(())
    }
    /// (allocated grid bins, bin entries, shapes, bin side) for telemetry.
    pub(crate) fn footprint(&self) -> (usize, usize, usize, i64) {
        (
            self.bins.allocated() + self.instances.as_ref().map_or(0, |i| i.bins.allocated()),
            self.entries,
            self.len()
                + self
                    .input
                    .instances
                    .as_ref()
                    .map_or(0, InstanceObstacles::len),
            self.bin,
        )
    }
    pub(crate) fn add(&mut self, b: Blockage) -> Result<()> {
        if self.len().saturating_add(
            self.input
                .instances
                .as_ref()
                .map_or(0, InstanceObstacles::len),
        ) >= self.limits.max_obstacles
        {
            return Err(budget("routing obstacle count"));
        }
        self.index(self.len(), &b)?;
        self.shapes.push(b);
        self.marks.push(0);
        self.alive.push(true);
        self.routed.push(false);
        Ok(())
    }
    fn len(&self) -> usize {
        self.input.obstacles.len() + self.shapes.len()
    }
    fn shape(&self, id: usize) -> &Blockage {
        if id < self.input.obstacles.len() {
            &self.input.obstacles[id]
        } else {
            &self.shapes[id - self.input.obstacles.len()]
        }
    }
    fn alive(&self, id: usize) -> bool {
        id < self.input.obstacles.len() || self.alive[id - self.input.obstacles.len()]
    }
    fn routed(&self, id: usize) -> bool {
        id >= self.input.obstacles.len() && self.routed[id - self.input.obstacles.len()]
    }
    fn index(&mut self, id: usize, b: &Blockage) -> Result<()> {
        if b.layer as usize >= self.input.layers.len() || !b.rect.valid() {
            return Err(invalid("routing obstacle"));
        }
        let (x0, x1, y0, y1) = (
            b.rect.lx.div_euclid(self.bin),
            b.rect.ux.div_euclid(self.bin),
            b.rect.ly.div_euclid(self.bin),
            b.rect.uy.div_euclid(self.bin),
        );
        let n = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if n < 0 || n > self.limits.max_bin_entries.saturating_sub(self.entries) as i128 {
            return Err(budget("routing spatial-bin count"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.bins.push(b.layer, x, y, id)?;
                self.entries += 1;
            }
        }
        Ok(())
    }
    pub(crate) fn clear(&mut self, layer: u32, rect: Rect, net: u32) -> Result<bool> {
        Ok(self.probe(layer, rect, net)?.is_some())
    }
    /// Like `probe`, but routed shapes of other nets do not block: their nets
    /// are returned as victims. Fixed shapes of other nets still block.
    pub(crate) fn probe_soft(&mut self, layer: u32, rect: Rect, net: u32) -> Result<Soft> {
        if !self.input.die.contains(rect) {
            return Ok(Soft::Blocked);
        }
        let mut near = false;
        let mut victims = Vec::new();
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| budget("routing query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let bins = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("routing query extent"));
        }
        let routing = self.input.layers[layer as usize].vertical.is_some();
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.work += 1;
                if self.work > self.limits.max_comparisons {
                    return Err(budget("routing comparison limit"));
                }
                {
                    for id in self.bins.iter(layer, x, y) {
                        if self.marks[id] == self.generation {
                            continue;
                        }
                        self.marks[id] = self.generation;
                        self.work += 1;
                        if self.work > self.limits.max_comparisons {
                            return Err(budget("routing comparison limit"));
                        }
                        if !self.alive(id) {
                            continue;
                        }
                        let b = self.shape(id);
                        if !overlap(query, b.rect) {
                            continue;
                        }
                        let shared =
                            self.same_net(net, layer, rect, b) && (routing || rect == b.rect);
                        if !shared {
                            match b.net {
                                Some(other) if self.routed(id) && other != net => {
                                    victims.push(other)
                                }
                                _ => return Ok(Soft::Blocked),
                            }
                            continue;
                        }
                        if self.input.rules
                            && routing
                            && route_rules::relation(
                                rect,
                                b.rect,
                                self.input.layers[layer as usize].spacing,
                            ) == route_rules::Relation::Near
                        {
                            near = true;
                        }
                    }
                }
            }
        }
        match self.probe_instances(layer, rect, query, net)? {
            Some(n) => near |= n,
            None => return Ok(Soft::Blocked),
        }
        victims.sort_unstable();
        victims.dedup();
        Ok(Soft::Clear { near, victims })
    }
    pub(crate) fn via_probe_soft(&mut self, kind: usize, at: [i64; 2], net: u32) -> Result<Soft> {
        let mut near = false;
        let mut victims = Vec::new();
        for shape in &self.input.vias[kind].shapes {
            match self.probe_soft(shape.layer, shifted(shape.rect, at)?, net)? {
                Soft::Blocked => return Ok(Soft::Blocked),
                Soft::Clear {
                    near: n,
                    victims: v,
                } => {
                    near |= n;
                    victims.extend(v);
                }
            }
        }
        victims.sort_unstable();
        victims.dedup();
        Ok(Soft::Clear { near, victims })
    }
    /// None when blocked; otherwise whether a same-net routing-layer shape is
    /// within spacing without connecting (a potential notch; rules mode only).
    pub(crate) fn probe(&mut self, layer: u32, rect: Rect, net: u32) -> Result<Option<bool>> {
        if !self.input.die.contains(rect) {
            return Ok(None);
        }
        let mut near = false;
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| budget("routing query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let bins = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("routing query extent"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.work += 1;
                if self.work > self.limits.max_comparisons {
                    return Err(budget("routing comparison limit"));
                }
                {
                    for id in self.bins.iter(layer, x, y) {
                        if self.marks[id] == self.generation {
                            continue;
                        }
                        self.marks[id] = self.generation;
                        self.work += 1;
                        if self.work > self.limits.max_comparisons {
                            return Err(budget("routing comparison limit"));
                        }
                        if !self.alive(id) {
                            continue;
                        }
                        let b = self.shape(id);
                        let routing = self.input.layers[layer as usize].vertical.is_some();
                        if !overlap(query, b.rect) {
                            continue;
                        }
                        let shared =
                            self.same_net(net, layer, rect, b) && (routing || rect == b.rect);
                        if !shared {
                            return Ok(None);
                        }
                        if self.input.rules
                            && routing
                            && route_rules::relation(
                                rect,
                                b.rect,
                                self.input.layers[layer as usize].spacing,
                            ) == route_rules::Relation::Near
                        {
                            near = true;
                        }
                    }
                }
            }
        }
        match self.probe_instances(layer, rect, query, net)? {
            Some(n) => near |= n,
            None => return Ok(None),
        }
        Ok(Some(near))
    }
    pub(crate) fn via_clear(&mut self, kind: usize, at: [i64; 2], net: u32) -> Result<bool> {
        Ok(self.via_probe(kind, at, net)?.is_some())
    }
    /// Explain one rejected shape without collecting its neighborhood. This is
    /// only called after validation fails; every repeated probe remains charged.
    fn explain_collision(&mut self, layer: u32, rect: Rect, net: u32) -> Result<String> {
        if !self.input.die.contains(rect) {
            return Ok(format!("outside die {:?}", self.input.die));
        }
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        let routing = self.input.layers[layer as usize].vertical.is_some();
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let bins = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("collision diagnostic query extent"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.work = self
                    .work
                    .checked_add(1)
                    .filter(|&n| n <= self.limits.max_comparisons)
                    .ok_or_else(|| budget("collision diagnostic work"))?;
                for id in self.bins.iter(layer, x, y) {
                    self.work = self
                        .work
                        .checked_add(1)
                        .filter(|&n| n <= self.limits.max_comparisons)
                        .ok_or_else(|| budget("collision diagnostic work"))?;
                    if !self.alive(id) {
                        continue;
                    }
                    let b = self.shape(id);
                    if overlap(query, b.rect)
                        && !(self.same_net(net, layer, rect, b) && (routing || rect == b.rect))
                    {
                        let source = if id < self.input.obstacles.len() {
                            "input obstacle"
                        } else {
                            "added shape"
                        };
                        return Ok(format!("{source} {id}: {b:?}"));
                    }
                }
            }
        }
        let mut culprit = None;
        if let Some(instances) = &mut self.instances {
            let ties = &self.ties;
            instances.visit(layer, query, &mut self.work, self.limits, |b| {
                if overlap(query, b.rect)
                    && !(same_net_shapes(ties, net, layer, rect, &b) && (routing || rect == b.rect))
                {
                    culprit = Some(b);
                    return Ok(false);
                }
                Ok(true)
            })?;
        }
        Ok(match culprit {
            Some(b) => format!("instance shape: {b:?}"),
            None => "no blocking shape found in repeated bounded probe".into(),
        })
    }
    pub(crate) fn via_probe(
        &mut self,
        kind: usize,
        at: [i64; 2],
        net: u32,
    ) -> Result<Option<bool>> {
        let mut near = false;
        for shape in &self.input.vias[kind].shapes {
            match self.probe(shape.layer, shifted(shape.rect, at)?, net)? {
                None => return Ok(None),
                Some(n) => near |= n,
            }
        }
        Ok(Some(near))
    }
    /// First clear kind in order; in rules mode prefer one without a near miss.
    pub(crate) fn choose(
        &mut self,
        kinds: &[usize],
        at: [i64; 2],
        net: u32,
    ) -> Result<Option<usize>> {
        let mut fallback = None;
        for &k in kinds {
            match self.via_probe(k, at, net)? {
                None => {}
                Some(near) if !near || !self.input.rules => return Ok(Some(k)),
                Some(_) => {
                    fallback = fallback.or(Some(k));
                }
            }
        }
        Ok(fallback)
    }
    pub(crate) fn add_wire(&mut self, w: &Wire) -> Result<()> {
        self.add(Blockage {
            layer: w.layer,
            rect: wire_rect(w, self.input.layers[w.layer as usize].width)?,
            net: Some(w.net),
        })
    }
    pub(crate) fn add_via(&mut self, v: &Via) -> Result<()> {
        for s in &self.input.vias[v.kind as usize].shapes {
            self.add(Blockage {
                layer: s.layer,
                rect: shifted(s.rect, v.at)?,
                net: Some(v.net),
            })?;
        }
        Ok(())
    }
}

pub(crate) fn preflight(input: &RouteInput, limits: &RouteLimits) -> Result<()> {
    if let Some(plan) = &input.tracks {
        limits.validate_track()?;
        crate::troute::check_plan(input, plan)?;
    } else {
        limits.validate()?;
    }
    if !input.die.valid()
        || input.grid <= 0
        || input.step < input.grid
        || input.step % input.grid != 0
        || input.layers.len() > 24
        || input.layers.is_empty()
    {
        return Err(invalid("routing domain/grid/layers"));
    }
    if input.access_layer as usize >= input.layers.len()
        || input.layers[input.access_layer as usize].vertical.is_none()
    {
        return Err(invalid("routing access layer"));
    }
    for l in &input.layers {
        if l.width <= 0
            || l.width % 2 != 0
            || i128::from(l.width) % (2 * i128::from(input.grid)) != 0
            || l.spacing < 0
            || !crate::valid_object_name(&l.name)
        {
            return Err(invalid("routing layer width/spacing/name"));
        }
    }
    let mut pairs = HashSet::new();
    for v in &input.vias {
        if v.lower == v.upper
            || v.lower as usize >= input.layers.len()
            || v.upper as usize >= input.layers.len()
            || input.layers[v.lower as usize].vertical.is_none()
            || input.layers[v.upper as usize].vertical.is_none()
            || (!pairs.insert((v.lower.min(v.upper), v.lower.max(v.upper))) && !input.rules)
            || v.lower > v.upper
            || v.shapes.is_empty()
            || v.shapes.len() > 64
            || v.shapes
                .iter()
                .any(|s| s.layer as usize >= input.layers.len() || !s.rect.valid())
        {
            return Err(invalid("routing via definition"));
        }
        if !v.shapes.iter().any(|s| s.layer == v.lower)
            || !v.shapes.iter().any(|s| s.layer == v.upper)
        {
            return Err(invalid("via lacks routing enclosure"));
        }
    }
    if let Some(instances) = &input.instances {
        instances.validate(input.layers.len(), limits)?;
        if instances.len().saturating_add(input.obstacles.len()) > limits.max_obstacles {
            return Err(budget("retained instance-obstacle records"));
        }
    }
    let mut pins = HashSet::new();
    // The track router bounds terminals and classes by the obstacle cap.
    let max_terminals = if input.tracks.is_some() {
        limits.max_obstacles
    } else {
        16_000
    };
    if input.terminals.len() > max_terminals
        || input.obstacles.len() > limits.max_obstacles
        || input.access_classes.len() > max_terminals
        || (!input.rules && !input.access_classes.is_empty())
    {
        return Err(budget("routing input count"));
    }
    // Aggregate class geometry shares the obstacle cap (<= 200k rectangles).
    if input
        .access_classes
        .iter()
        .map(|c| c.shapes.len() + c.blockers.len())
        .sum::<usize>()
        > limits.max_obstacles
    {
        return Err(budget("routing access class geometry"));
    }
    for c in &input.access_classes {
        if c.shapes.is_empty()
            || c.shapes.len() > 64
            || c.blockers.len() > 4096
            || c.shapes
                .iter()
                .chain(&c.blockers)
                .any(|s| s.layer as usize >= input.layers.len() || !s.rect.valid())
        {
            return Err(invalid("routing access class"));
        }
    }
    for l in &input.layers {
        if l.min_area.is_some_and(|a| a <= 0) || l.min_step.is_some_and(|s| s <= 0) {
            return Err(invalid("routing layer area/step rule"));
        }
    }
    for t in &input.terminals {
        if let Some((class, origin)) = t.class {
            let c = input
                .access_classes
                .get(class as usize)
                .ok_or_else(|| invalid("routing terminal access class"))?;
            let moved: Vec<_> = c
                .shapes
                .iter()
                .map(|s| shifted(s.rect, origin).map(|r| (s.layer, r)))
                .collect::<Result<_>>()?;
            if moved.len() != t.shapes.len()
                || moved
                    .iter()
                    .zip(&t.shapes)
                    .any(|(m, s)| m.0 != s.layer || m.1 != s.rect)
            {
                return Err(invalid("routing terminal differs from its access class"));
            }
        }
        if !pins.insert(t.pin)
            || t.shapes.is_empty()
            || t.shapes.len() > 64
            || t.shapes.iter().any(|s| {
                s.layer as usize >= input.layers.len()
                    || !s.rect.valid()
                    || input.layers[s.layer as usize].vertical.is_none()
            })
        {
            return Err(invalid("routing terminal"));
        }
    }
    if !input.ties.is_empty() {
        if input.tracks.is_none() || !input.rules {
            return Err(invalid("supply ties need the track router"));
        }
        let mut per_net = HashMap::<u32, (usize, usize)>::new();
        for t in &input.terminals {
            let e = per_net.entry(t.net).or_default();
            e.0 += 1;
            e.1 += usize::from(t.driver);
        }
        for (i, tie) in input.ties.iter().enumerate() {
            if (i > 0 && input.ties[i - 1].net >= tie.net)
                || tie.net == tie.supply
                || per_net.get(&tie.net) != Some(&(2, 1))
                || per_net.contains_key(&tie.supply)
                || input
                    .ties
                    .binary_search_by_key(&tie.supply, |t| t.net)
                    .is_ok()
                || tie.zone.layer as usize >= input.layers.len()
                || input.layers[tie.zone.layer as usize].vertical.is_none()
                || !tie.zone.rect.valid()
                || !input.terminals.iter().any(|t| {
                    t.net == tie.net
                        && t.driver
                        && t.class.is_none()
                        && t.shapes.len() == 1
                        && t.shapes[0].layer == tie.zone.layer
                        && t.shapes[0].rect == tie.zone.rect
                })
            {
                return Err(invalid("supply tie"));
            }
        }
    }
    Ok(())
}
pub(crate) fn candidates(t: &Terminal, input: &RouteInput) -> Result<Vec<(u32, [i64; 2])>> {
    quartile_points(&t.shapes, input.grid, Some(input.die))
}
/// Fixed-via kinds grouped by connected layer pair, in first-use order.
pub(crate) struct ViaPair {
    pub(crate) lower: u32,
    pub(crate) upper: u32,
    pub(crate) kinds: Vec<usize>,
}
/// Via pairs in first-use order (shared by the lattice and track routers).
pub(crate) fn via_pairs(input: &RouteInput) -> Vec<ViaPair> {
    let mut pairs: Vec<ViaPair> = Vec::new();
    for (k, v) in input.vias.iter().enumerate() {
        if let Some(p) = pairs
            .iter_mut()
            .find(|p| p.lower == v.lower && p.upper == v.upper)
        {
            p.kinds.push(k);
        } else {
            pairs.push(ViaPair {
                lower: v.lower,
                upper: v.upper,
                kinds: vec![k],
            });
        }
    }
    pairs
}
struct Lattice {
    xs: Vec<i64>,
    ys: Vec<i64>,
    layers: Vec<u32>,
    slots: HashMap<u32, usize>,
    pairs: Vec<ViaPair>,
    len: usize,
}
impl Lattice {
    fn new(input: &RouteInput, limits: &RouteLimits) -> Result<Self> {
        let axis = |lo: i64, hi: i64| -> Result<BTreeSet<i64>> {
            let first = i128::from(lo).div_euclid(i128::from(input.grid)) * i128::from(input.grid);
            let count = (i128::from(hi) - first) / i128::from(input.step) + 1;
            if count > limits.max_axis as i128 {
                return Err(budget("routing axis count"));
            }
            let mut out = BTreeSet::new();
            for i in 0..count {
                let v = int(first + i * i128::from(input.step))?;
                if v >= lo && v <= hi {
                    out.insert(v);
                }
            }
            Ok(out)
        };
        let mut xs = axis(input.die.lx, input.die.ux)?;
        let mut ys = axis(input.die.ly, input.die.uy)?;
        for t in &input.terminals {
            for (_, p) in candidates(t, input)? {
                xs.insert(p[0]);
                ys.insert(p[1]);
            }
        }
        if xs.len() > limits.max_axis || ys.len() > limits.max_axis {
            return Err(budget("routing pin-coordinate axis count"));
        }
        let layers: Vec<_> = input
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.vertical.map(|_| i as u32))
            .collect();
        if layers.is_empty() || layers.len() > 8 {
            return Err(budget("routing grid layer count"));
        }
        let len = xs
            .len()
            .checked_mul(ys.len())
            .and_then(|n| n.checked_mul(layers.len()))
            .ok_or_else(|| budget("routing grid size overflow"))?;
        if len == 0 || len > limits.max_grid_nodes {
            return Err(budget("routing grid node count"));
        }
        let slots = layers.iter().enumerate().map(|(i, l)| (*l, i)).collect();
        let pairs = via_pairs(input);
        Ok(Self {
            xs: xs.into_iter().collect(),
            ys: ys.into_iter().collect(),
            layers,
            slots,
            pairs,
            len,
        })
    }
    fn id(&self, layer: u32, p: [i64; 2]) -> u32 {
        let x = self.xs.binary_search(&p[0]).unwrap();
        let y = self.ys.binary_search(&p[1]).unwrap();
        (self.slots[&layer] * self.xs.len() * self.ys.len() + y * self.xs.len() + x) as u32
    }
    fn point(&self, id: u32) -> (u32, [i64; 2]) {
        let i = id as usize;
        let plane = self.xs.len() * self.ys.len();
        let k = i % plane;
        (
            self.layers[i / plane],
            [self.xs[k % self.xs.len()], self.ys[k / self.xs.len()]],
        )
    }
    fn neighbors(&self, id: u32) -> Vec<(u32, Option<usize>)> {
        let i = id as usize;
        let nx = self.xs.len();
        let ny = self.ys.len();
        let plane = nx * ny;
        let x = i % nx;
        let y = (i % plane) / nx;
        let mut n = Vec::with_capacity(8);
        if x > 0 {
            n.push((id - 1, None));
        }
        if x + 1 < nx {
            n.push((id + 1, None));
        }
        if y > 0 {
            n.push((id - nx as u32, None));
        }
        if y + 1 < ny {
            n.push((id + nx as u32, None));
        }
        let (l, p) = self.point(id);
        for (k, v) in self.pairs.iter().enumerate() {
            if v.lower == l {
                n.push((self.id(v.upper, p), Some(k)));
            } else if v.upper == l {
                n.push((self.id(v.lower, p), Some(k)));
            }
        }
        n
    }
}
pub(crate) fn distance(a: [i64; 2], b: [i64; 2]) -> Result<u64> {
    let d =
        (i128::from(a[0]) - i128::from(b[0])).abs() + (i128::from(a[1]) - i128::from(b[1])).abs();
    u64::try_from(d).map_err(|_| invalid("routing distance overflow"))
}

/// A reserved pin escape: lattice edges with via kind (`u32::MAX` = wire).
struct AccessPlan {
    access: Access,
    top: u32,
    edges: Vec<((u32, u32), u32)>,
}
pub(crate) const WIRE: u32 = u32::MAX;

/// One precomputed landing for a (master, orientation, pin) class: relative
/// point, legal upward via kinds (pads inside the pin first) and whether any
/// legal pad lies entirely inside a pin rectangle.
pub(crate) struct AccessEntry {
    pub(crate) layer: u32,
    pub(crate) at: [i64; 2],
    pub(crate) kinds: Vec<usize>,
    pub(crate) fits: bool,
}

pub(crate) fn quartile_points(
    shapes: &[LayerRect],
    grid: i64,
    die: Option<Rect>,
) -> Result<Vec<(u32, [i64; 2])>> {
    let mut seen = BTreeSet::new();
    let mut points = Vec::new();
    for s in shapes {
        for i in [2, 1, 3] {
            for j in [2, 1, 3] {
                let x =
                    i128::from(s.rect.lx) + (i128::from(s.rect.ux) - i128::from(s.rect.lx)) * i / 4;
                let y =
                    i128::from(s.rect.ly) + (i128::from(s.rect.uy) - i128::from(s.rect.ly)) * j / 4;
                let snap = |v: i128| {
                    int((v + i128::from(grid) / 2).div_euclid(i128::from(grid)) * i128::from(grid))
                };
                let p = [snap(x)?, snap(y)?];
                if contains(s.rect, p)
                    && die.is_none_or(|d| contains(d, p))
                    && seen.insert((s.layer, p))
                {
                    points.push((s.layer, p));
                }
            }
        }
    }
    if points.len() > 128 {
        return Err(budget("pin access candidates"));
    }
    Ok(points)
}

/// Build the access table of one class once: each candidate via is checked
/// against the master's own other-net geometry (spacing), against same-pin
/// notches, and against LEF MINSTEP when its pad protrudes from the pin.
fn access_table(
    input: &RouteInput,
    grid: &Lattice,
    class: &AccessClass,
    work: &mut usize,
    limits: &RouteLimits,
) -> Result<Vec<AccessEntry>> {
    let points = quartile_points(&class.shapes, input.grid, None)?;
    access_table_at(
        input,
        &grid.slots,
        &grid.pairs,
        class,
        &points,
        work,
        limits,
    )
}
/// Access table over the given relative landing points (see `access_table`).
pub(crate) fn access_table_at(
    input: &RouteInput,
    slots: &HashMap<u32, usize>,
    pairs: &[ViaPair],
    class: &AccessClass,
    points: &[(u32, [i64; 2])],
    work: &mut usize,
    limits: &RouteLimits,
) -> Result<Vec<AccessEntry>> {
    let cap = limits.max_comparisons;
    let access = slots[&input.access_layer];
    let mut entries = Vec::new();
    for &(layer, at) in points {
        let pins = |l: u32| -> Vec<Rect> {
            class
                .shapes
                .iter()
                .filter(|s| s.layer == l)
                .map(|s| s.rect)
                .collect()
        };
        let own = pins(layer);
        let (mut fit, mut other) = (Vec::new(), Vec::new());
        for pair in pairs {
            if pair.lower != layer || slots.get(&pair.upper).is_none_or(|s| *s > access) {
                continue;
            }
            'kind: for &k in &pair.kinds {
                let mut fits = false;
                for s in &input.vias[k].shapes {
                    let rect = shifted(s.rect, at)?;
                    let rule = &input.layers[s.layer as usize];
                    let halo = expand(rect, rule.spacing)?;
                    for b in class.blockers.iter().filter(|b| b.layer == s.layer) {
                        *work += 1;
                        if overlap(halo, b.rect) {
                            continue 'kind;
                        }
                    }
                    if rule.vertical.is_none() {
                        continue;
                    }
                    let same = pins(s.layer);
                    if !same.is_empty()
                        && !route_rules::local_ok(rect, &same, rule.spacing, work, cap)?
                    {
                        continue 'kind;
                    }
                    if s.layer == layer {
                        fits = own.iter().any(|p| p.contains(rect));
                        if !fits
                            && let Some(step) = rule.min_step
                            && let Some(p) = own.iter().find(|p| contains(**p, at))
                        {
                            let sides = [
                                p.lx - rect.lx,
                                rect.ux - p.ux,
                                p.ly - rect.ly,
                                rect.uy - p.uy,
                            ];
                            if sides.iter().any(|d| *d > 0 && *d < step) {
                                continue 'kind;
                            }
                        }
                    }
                }
                if fits {
                    fit.push(k);
                } else {
                    other.push(k);
                }
            }
        }
        entries.push(AccessEntry {
            layer,
            at,
            fits: !fit.is_empty(),
            kinds: fit.into_iter().chain(other).collect(),
        });
        if *work > cap {
            return Err(budget("pin access table work"));
        }
    }
    entries.sort_by_key(|e| !e.fits);
    Ok(entries)
}

/// Access search start: layer, point, initial cost and table via kinds.
type Start = (u32, [i64; 2], u64, Option<Vec<usize>>);

fn reserve_access(
    input: &RouteInput,
    grid: &Lattice,
    spatial: &mut Spatial<'_>,
    terminals: &BTreeMap<u32, Vec<&Terminal>>,
    limits: &RouteLimits,
) -> Result<(HashMap<u32, AccessPlan>, usize)> {
    let mut plans = HashMap::new();
    let mut total = 0usize;
    let mut tables = BTreeMap::<u32, Vec<AccessEntry>>::new();
    for group in terminals.values().filter(|g| g.len() >= 2) {
        for t in group {
            let generic = candidates(t, input)?;
            // (layer, point) -> (start cost, table via kinds)
            let mut starts: Vec<Start> = Vec::new();
            if input.rules
                && let Some((class, origin)) = t.class
            {
                if let std::collections::btree_map::Entry::Vacant(slot) = tables.entry(class) {
                    let mut work = 0usize;
                    let table = access_table(
                        input,
                        grid,
                        &input.access_classes[class as usize],
                        &mut work,
                        limits,
                    )?;
                    spatial.work = spatial
                        .work
                        .checked_add(work)
                        .ok_or_else(|| budget("routing comparison limit"))?;
                    slot.insert(table);
                }
                let known: BTreeSet<_> = generic.iter().copied().collect();
                for e in &tables[&class] {
                    let p = [
                        int(i128::from(e.at[0]) + i128::from(origin[0]))?,
                        int(i128::from(e.at[1]) + i128::from(origin[1]))?,
                    ];
                    if known.contains(&(e.layer, p)) {
                        let cost = if e.fits { 0 } else { input.step as u64 };
                        starts.push((e.layer, p, cost, Some(e.kinds.clone())));
                    }
                }
            }
            if starts.is_empty() {
                starts = generic.iter().map(|&(l, p)| (l, p, 0, None)).collect();
            }
            if starts.is_empty() {
                return Err(no_route("no access candidates"));
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
            let mut heap = BinaryHeap::new();
            // node -> (cost, parent, via kind of the parent edge)
            let mut visited = HashMap::<u32, (u64, u32, u32)>::new();
            for (layer, at, cost, _) in &starts {
                let (layer, at, cost) = (*layer, *at, *cost);
                let half = input.layers[layer as usize].width / 2;
                let r = Rect {
                    lx: int(i128::from(at[0]) - i128::from(half))?,
                    ly: int(i128::from(at[1]) - i128::from(half))?,
                    ux: int(i128::from(at[0]) + i128::from(half))?,
                    uy: int(i128::from(at[1]) + i128::from(half))?,
                };
                if spatial.clear(layer, r, t.net)? {
                    let id = grid.id(layer, at);
                    if visited.get(&id).is_none_or(|v| v.0 > cost) {
                        visited.insert(id, (cost, u32::MAX, WIRE));
                        heap.push(Reverse((cost, id)));
                    }
                }
            }
            let mut found = None;
            let mut expanded = 0usize;
            while let Some(Reverse((cost, id))) = heap.pop() {
                if visited[&id].0 != cost {
                    continue;
                }
                let (layer, at) = grid.point(id);
                if grid.slots[&layer] >= grid.slots[&input.access_layer] {
                    found = Some(id);
                    break;
                }
                expanded += 1;
                total += 1;
                if expanded > limits.max_search_expansions || total > limits.max_expansions {
                    return Err(budget("pin access expansion limit"));
                }
                for (next, via) in grid.neighbors(id) {
                    let (nl, q) = grid.point(next);
                    if grid.slots[&nl] > grid.slots[&input.access_layer] || !contains(bounds, q) {
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
                    if visited.get(&next).is_some_and(|v| v.0 <= nc) {
                        continue;
                    }
                    let kind = if let Some(pi) = via {
                        let pair = &grid.pairs[pi];
                        let listed: Vec<usize> = match table_kinds.get(&(layer, at)) {
                            Some(ks) if pair.lower == layer => ks
                                .iter()
                                .copied()
                                .filter(|k| pair.kinds.contains(k))
                                .collect(),
                            _ => pair.kinds.clone(),
                        };
                        let mut chosen = None;
                        for k in listed {
                            if input.rules {
                                let mut ok = true;
                                for s in &input.vias[k].shapes {
                                    let rule = &input.layers[s.layer as usize];
                                    let own = pin_shapes(s.layer);
                                    if rule.vertical.is_some()
                                        && !own.is_empty()
                                        && !route_rules::local_ok(
                                            shifted(s.rect, at)?,
                                            &own,
                                            rule.spacing,
                                            &mut spatial.work,
                                            limits.max_comparisons,
                                        )?
                                    {
                                        ok = false;
                                        break;
                                    }
                                }
                                if !ok {
                                    continue;
                                }
                            }
                            if spatial.via_clear(k, at, t.net)? {
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
                            input.layers[layer as usize].width,
                        )?;
                        if input.rules {
                            let own = pin_shapes(layer);
                            if !own.is_empty()
                                && !route_rules::local_ok(
                                    rect,
                                    &own,
                                    input.layers[layer as usize].spacing,
                                    &mut spatial.work,
                                    limits.max_comparisons,
                                )?
                            {
                                continue;
                            }
                        }
                        if !spatial.clear(layer, rect, t.net)? {
                            continue;
                        }
                        WIRE
                    };
                    if heap.len() >= limits.max_heap || visited.len() >= limits.max_grid_nodes {
                        return Err(budget("pin access frontier"));
                    }
                    visited.insert(next, (nc, id, kind));
                    heap.push(Reverse((nc, next)));
                }
            }
            let top = found
                .ok_or_else(|| no_route(&format!("pin {} has no legal local escape", t.pin)))?;
            let mut current = top;
            let mut edges = Vec::new();
            while visited[&current].1 != u32::MAX {
                let (_, next, kind) = visited[&current];
                edges.push(((current.min(next), current.max(next)), kind));
                current = next;
                if edges.len() > limits.max_segments {
                    return Err(budget("access path limit"));
                }
            }
            let (layer, at) = grid.point(current);
            for &((a, b), kind) in &edges {
                let (l, p) = grid.point(a);
                let (other, q) = grid.point(b);
                if l == other {
                    spatial.add_wire(&Wire {
                        net: t.net,
                        layer: l,
                        start: p,
                        end: q,
                    })?;
                } else {
                    spatial.add_via(&Via {
                        net: t.net,
                        kind,
                        at: p,
                    })?;
                }
            }
            plans.insert(
                t.pin,
                AccessPlan {
                    access: Access {
                        pin: t.pin,
                        net: t.net,
                        layer,
                        at,
                    },
                    top,
                    edges,
                },
            );
        }
    }
    Ok((plans, total))
}

/// Whole-route memory estimate of the track router for this input (`None`
/// for the lattice router), as `route_cpu_ref` enforces it.
pub fn track_memory_estimate(
    input: &RouteInput,
    limits: &RouteLimits,
) -> Result<Option<crate::troute::MemoryEstimate>> {
    crate::troute::estimate_for(input, limits)
}

pub(crate) fn route_greedy_access(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
    preflight(input, limits)?;
    if input.tracks.is_none() {
        return Err(invalid("deferred access repair requires track routing"));
    }
    crate::troute::route(input, limits, false)
}

pub fn route_cpu_ref(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
    preflight(input, limits)?;
    if input.tracks.is_some() {
        return crate::troute::route(input, limits, true);
    }
    let grid = Lattice::new(input, limits)?;
    let mut spatial = Spatial::new(input, limits)?;
    let mut groups = BTreeMap::<u32, Vec<&Terminal>>::new();
    for t in &input.terminals {
        groups.entry(t.net).or_default().push(t);
    }
    let (plans, access_expansions) = reserve_access(input, &grid, &mut spatial, &groups, limits)?;
    let mut out = Routes {
        wires: Vec::new(),
        vias: Vec::new(),
        access: Vec::new(),
        patches: Vec::new(),
        nets: Vec::new(),
        stats: RouteStats {
            grid_nodes: grid.len,
            expansions: access_expansions,
            ..Default::default()
        },
    };
    let mut costs = vec![u64::MAX; grid.len];
    let mut parents = vec![u32::MAX; grid.len];
    let mut parent_kinds = vec![WIRE; grid.len];
    let mut tags = vec![0u32; grid.len];
    let mut epoch = 0u32;
    for (net, mut terminals) in groups {
        if terminals.len() < 2 {
            continue;
        }
        terminals.sort_by_key(|p| (!p.driver, p.pin));
        if terminals.iter().filter(|p| p.driver).count() != 1 {
            return Err(invalid("routed net needs one driver"));
        }
        out.nets.push(net);
        let root = &plans[&terminals[0].pin];
        out.access.push(root.access.clone());
        let mut tree = HashSet::from([root.top]);
        let mut edges: BTreeMap<(u32, u32), u32> = root.edges.iter().copied().collect();
        for &((a, b), _) in &root.edges {
            tree.insert(a);
            tree.insert(b);
        }
        for terminal in &terminals[1..] {
            let plan = &plans[&terminal.pin];
            out.access.push(plan.access.clone());
            let starts = vec![plan.top];
            epoch = epoch
                .checked_add(1)
                .ok_or_else(|| budget("routing search count"))?;
            let mut heap = BinaryHeap::new();
            let points: Vec<_> = tree.iter().map(|id| grid.point(*id).1).collect();
            let bbox = Rect {
                lx: points.iter().map(|p| p[0]).min().unwrap(),
                ly: points.iter().map(|p| p[1]).min().unwrap(),
                ux: points.iter().map(|p| p[0]).max().unwrap(),
                uy: points.iter().map(|p| p[1]).max().unwrap(),
            };
            let heuristic = |p: [i64; 2]| {
                distance(
                    p,
                    [p[0].clamp(bbox.lx, bbox.ux), p[1].clamp(bbox.ly, bbox.uy)],
                )
            };
            for &id in &starts {
                costs[id as usize] = 0;
                parents[id as usize] = u32::MAX;
                parent_kinds[id as usize] = WIRE;
                tags[id as usize] = epoch;
                heap.push(Reverse((heuristic(grid.point(id).1)?, 0u64, id)));
            }
            let mut found = None;
            let mut expanded = 0;
            while let Some(Reverse((_, cost, id))) = heap.pop() {
                if tags[id as usize] != epoch || costs[id as usize] != cost {
                    continue;
                }
                if tree.contains(&id) {
                    found = Some(id);
                    break;
                }
                expanded += 1;
                out.stats.expansions += 1;
                if expanded > limits.max_search_expansions {
                    return Err(budget(&format!(
                        "routing expansion limit: per-sink search, pin {} net {net}, {} nets done",
                        terminal.pin,
                        out.nets.len() - 1
                    )));
                }
                if out.stats.expansions > limits.max_expansions {
                    return Err(budget(&format!(
                        "routing expansion limit: total, pin {} net {net}, {} nets done",
                        terminal.pin,
                        out.nets.len() - 1
                    )));
                }
                let (layer, p) = grid.point(id);
                for (next, via) in grid.neighbors(id) {
                    let (_, q) = grid.point(next);
                    let length = if via.is_some() {
                        u64::try_from(input.step)
                            .unwrap()
                            .checked_mul(3)
                            .ok_or_else(|| budget("via cost overflow"))?
                    } else {
                        distance(p, q)?
                    };
                    let preferred = input.layers[layer as usize].vertical.unwrap();
                    let multiplier = if via.is_none()
                        && ((preferred && p[1] == q[1]) || (!preferred && p[0] == q[0]))
                    {
                        2
                    } else {
                        1
                    };
                    let next_cost = cost
                        .checked_add(
                            length
                                .checked_mul(multiplier)
                                .ok_or_else(|| budget("routing cost"))?,
                        )
                        .ok_or_else(|| budget("routing path cost"))?;
                    if tags[next as usize] == epoch && costs[next as usize] <= next_cost {
                        continue;
                    }
                    let kind = if let Some(pi) = via {
                        match spatial.choose(&grid.pairs[pi].kinds, p, net)? {
                            Some(k) => k as u32,
                            None => continue,
                        }
                    } else {
                        if !spatial.clear(
                            layer,
                            wire_rect(
                                &Wire {
                                    net,
                                    layer,
                                    start: p,
                                    end: q,
                                },
                                input.layers[layer as usize].width,
                            )?,
                            net,
                        )? {
                            continue;
                        }
                        WIRE
                    };
                    tags[next as usize] = epoch;
                    costs[next as usize] = next_cost;
                    parents[next as usize] = id;
                    parent_kinds[next as usize] = kind;
                    if heap.len() >= limits.max_heap {
                        return Err(budget("routing frontier limit"));
                    }
                    heap.push(Reverse((
                        next_cost
                            .checked_add(heuristic(q)?)
                            .ok_or_else(|| budget("routing estimate overflow"))?,
                        next_cost,
                        next,
                    )));
                }
            }
            let mut current = found.ok_or_else(|| {
                no_route(&format!("pin {} exhausted finite lattice", terminal.pin))
            })?;
            let mut length = 0usize;
            while parents[current as usize] != u32::MAX {
                let next = parents[current as usize];
                edges
                    .entry((current.min(next), current.max(next)))
                    .or_insert(parent_kinds[current as usize]);
                tree.insert(current);
                current = next;
                length += 1;
                if length > limits.max_search_expansions || edges.len() > limits.max_segments {
                    return Err(budget("routing trace/segment limit"));
                }
            }
            tree.insert(current);
            for &((a, b), kind) in &plan.edges {
                edges.insert((a, b), kind);
                tree.insert(a);
                tree.insert(b);
            }
        }
        for ((a, b), kind) in edges {
            if out.wires.len() + out.vias.len() >= limits.max_segments {
                return Err(budget("routing output segment limit"));
            }
            let (l, p) = grid.point(a);
            let (other, q) = grid.point(b);
            if l == other {
                let wire = Wire {
                    net,
                    layer: l,
                    start: p,
                    end: q,
                };
                out.stats.wire_length_dbu = out
                    .stats
                    .wire_length_dbu
                    .checked_add(distance(p, q)?)
                    .ok_or_else(|| budget("total wire length"))?;
                spatial.add_wire(&wire)?;
                out.wires.push(wire);
            } else {
                if kind == WIRE
                    || input.vias.get(kind as usize).is_none_or(|v| {
                        !((v.lower == l && v.upper == other) || (v.upper == l && v.lower == other))
                    })
                {
                    return Err(invalid("missing path via"));
                }
                let v = Via { net, kind, at: p };
                spatial.add_via(&v)?;
                out.vias.push(v);
            }
        }
    }
    out.stats.comparisons = spatial.work;
    if input.rules {
        let mut cleanup_limits = limits.clone();
        cleanup_limits.max_comparisons = limits
            .max_comparisons
            .checked_sub(out.stats.comparisons)
            .filter(|&n| n > 0)
            .ok_or_else(|| budget("routing comparison limit before cleanup"))?;
        let work = cleanup(input, &mut out, &cleanup_limits)?;
        out.stats.comparisons = out
            .stats
            .comparisons
            .checked_add(work)
            .ok_or_else(|| budget("routing comparison limit"))?;
        if out.stats.comparisons > limits.max_comparisons {
            return Err(budget("routing comparison limit"));
        }
        out.stats.wire_length_dbu = out.wires.iter().try_fold(0u64, |n, w| {
            n.checked_add(distance(w.start, w.end)?)
                .ok_or_else(|| budget("total wire length"))
        })?;
    }
    validate(input, &out, limits)?;
    Ok(out)
}

#[cfg(test)]
type Node = (u32, u32, i64, i64);

/// Remove dead-end wires/vias that lead to no access point (bounded by edges).
#[cfg(test)]
fn prune_legacy(input: &RouteInput, out: &mut Routes) {
    let anchors: BTreeSet<Node> = out
        .access
        .iter()
        .map(|a| (a.net, a.layer, a.at[0], a.at[1]))
        .collect();
    let nw = out.wires.len();
    let ends = |i: usize| -> (Node, Node) {
        if i < nw {
            let w = &out.wires[i];
            (
                (w.net, w.layer, w.start[0], w.start[1]),
                (w.net, w.layer, w.end[0], w.end[1]),
            )
        } else {
            let v = &out.vias[i - nw];
            let s = &input.vias[v.kind as usize];
            (
                (v.net, s.lower, v.at[0], v.at[1]),
                (v.net, s.upper, v.at[0], v.at[1]),
            )
        }
    };
    let total = nw + out.vias.len();
    let mut incident = BTreeMap::<Node, Vec<usize>>::new();
    for i in 0..total {
        let (a, b) = ends(i);
        incident.entry(a).or_default().push(i);
        incident.entry(b).or_default().push(i);
    }
    let mut degree: BTreeMap<Node, usize> = incident.iter().map(|(k, v)| (*k, v.len())).collect();
    let mut alive = vec![true; total];
    let mut queue: BTreeSet<Node> = degree
        .iter()
        .filter(|(n, d)| **d == 1 && !anchors.contains(n))
        .map(|(n, _)| *n)
        .collect();
    while let Some(node) = queue.pop_first() {
        if degree[&node] != 1 {
            continue;
        }
        let Some(&e) = incident[&node].iter().find(|&&e| alive[e]) else {
            continue;
        };
        alive[e] = false;
        let (a, b) = ends(e);
        for end in [a, b] {
            let d = degree.get_mut(&end).unwrap();
            *d -= 1;
            if *d == 1 && !anchors.contains(&end) {
                queue.insert(end);
            }
        }
    }
    let mut i = 0;
    out.wires.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    let mut j = nw;
    out.vias.retain(|_| {
        j += 1;
        alive[j - 1]
    });
}

/// Prune one endpoint graph at a time; output order and cyclic metal remain unchanged.
fn prune(input: &RouteInput, out: &mut Routes, limits: &RouteLimits) -> Result<usize> {
    let mut work = 0;
    let index = crate::route_graph::NetIndex::new(out, limits, &mut work)?;
    let nw = out.wires.len();
    let mut alive = vec![true; nw + out.vias.len()];
    for &net in index.nets() {
        let sources = index.sources(out, net);
        let graph = crate::route_graph::Graph::new(input, out, sources, limits, &mut work)?;
        let kept = graph.pruned(&mut work, limits.max_comparisons)?;
        for (&i, &keep) in sources.wires.iter().zip(&kept) {
            alive[i as usize] = keep;
        }
        for (&i, &keep) in sources.vias.iter().zip(&kept[sources.wires.len()..]) {
            alive[nw + i as usize] = keep;
        }
    }
    let mut i = 0;
    out.wires.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    out.vias.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    Ok(work)
}

/// Post-route same-net legalization: prune dead ends, then up to eight rounds
/// of notch/point-contact patches and minimum-area stubs, each checked against
/// other nets. Unresolved violations fail explicitly (HEAD stays unchanged).
pub(crate) fn cleanup(input: &RouteInput, out: &mut Routes, limits: &RouteLimits) -> Result<usize> {
    let (work, remaining) = cleanup_report(input, out, limits)?;
    if let Some(v) = remaining.first() {
        return Err(no_route(&format!(
            "ROUTING_RULE unresolved {} ({} total)",
            v.describe(),
            remaining.len()
        )));
    }
    Ok(work)
}
/// `cleanup` without the final failure: returns the work and every violation
/// left after the bounded repair rounds (the track router re-routes them).
pub(crate) fn cleanup_report(
    input: &RouteInput,
    out: &mut Routes,
    limits: &RouteLimits,
) -> Result<(usize, Vec<Violation>)> {
    cleanup_with_reservations(input, out, limits, std::iter::empty())
}

/// Keep negotiated clearance regions protected while other nets receive patches.
pub(crate) fn cleanup_with_reservations(
    input: &RouteInput,
    out: &mut Routes,
    limits: &RouteLimits,
    reservations: impl IntoIterator<Item = Blockage>,
) -> Result<(usize, Vec<Violation>)> {
    let work = prune(input, out, limits)?;
    let mut spatial = Spatial::new(input, limits)?;
    spatial.work = spatial
        .work
        .checked_add(work)
        .filter(|&n| n <= limits.max_comparisons)
        .ok_or_else(|| budget("cleanup comparison limit"))?;
    for w in &out.wires {
        spatial.add_wire(w)?;
    }
    for v in &out.vias {
        spatial.add_via(v)?;
    }
    for reservation in reservations {
        spatial.add_fixed(reservation)?;
    }
    let mut tried = BTreeSet::new();
    for _ in 0..8 {
        let violations = route_rules::check(input, out, limits, &mut spatial.work)?;
        if violations.is_empty() {
            break;
        }
        let mut changed = false;
        let cache = if input.tracks.is_some()
            && violations
                .iter()
                .any(|v| matches!(v, Violation::Area { .. }))
        {
            Some(crate::rule_index::RuleIndex::new(
                input,
                out,
                &mut spatial.work,
                limits.max_comparisons,
            )?)
        } else {
            None
        };
        for v in &violations {
            match v {
                Violation::Spacing { net, layer, a, b } => {
                    let width = input.layers[*layer as usize].width;
                    let fill = if input.tracks.is_some() {
                        route_rules::fill_for_robust(*a, *b, width, input.grid)
                    } else {
                        route_rules::fill_for(*a, *b, width, input.grid)
                    };
                    if tried.insert((*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy))
                        && spatial.clear(*layer, fill, *net)?
                    {
                        check_cleanup_record_room(out, limits)?;
                        spatial.add(Blockage {
                            layer: *layer,
                            rect: fill,
                            net: Some(*net),
                        })?;
                        out.patches.push(Patch {
                            net: *net,
                            layer: *layer,
                            rect: fill,
                        });
                        changed = true;
                    }
                }
                Violation::Area {
                    net, layer, shapes, ..
                } => {
                    if let Some(w) = area_stub(
                        input,
                        out,
                        &mut spatial,
                        *net,
                        *layer,
                        shapes,
                        limits,
                        cache.as_ref(),
                    )? {
                        check_cleanup_record_room(out, limits)?;
                        spatial.add_wire(&w)?;
                        out.wires.push(w);
                        changed = true;
                    }
                }
                Violation::Hole { net, layer, rect } => {
                    let fill = Rect {
                        lx: rect.lx.div_euclid(input.grid) * input.grid,
                        ly: rect.ly.div_euclid(input.grid) * input.grid,
                        ux: -((-rect.ux).div_euclid(input.grid) * input.grid),
                        uy: -((-rect.uy).div_euclid(input.grid) * input.grid),
                    };
                    if tried.insert((*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy))
                        && spatial.clear(*layer, fill, *net)?
                    {
                        check_cleanup_record_room(out, limits)?;
                        spatial.add(Blockage {
                            layer: *layer,
                            rect: fill,
                            net: Some(*net),
                        })?;
                        out.patches.push(Patch {
                            net: *net,
                            layer: *layer,
                            rect: fill,
                        });
                        changed = true;
                    }
                }
                Violation::Floating { .. } => {}
            }
        }
        if !changed {
            break;
        }
    }
    let remaining = route_rules::check(input, out, limits, &mut spatial.work)?;
    Ok((spatial.work, remaining))
}

fn check_cleanup_record_room(out: &Routes, limits: &RouteLimits) -> Result<()> {
    if out
        .wires
        .len()
        .saturating_add(out.vias.len())
        .saturating_add(out.patches.len())
        >= limits.max_segments
    {
        return Err(budget("cleanup output record limit"));
    }
    Ok(())
}

/// Shortest default-width stub from a via/wire node of the component that
/// lifts its merged area to the LEF minimum without new same-net notches.
#[allow(clippy::too_many_arguments)]
fn area_stub(
    input: &RouteInput,
    out: &Routes,
    spatial: &mut Spatial<'_>,
    net: u32,
    layer: u32,
    shapes: &[Rect],
    limits: &RouteLimits,
    cache: Option<&crate::rule_index::RuleIndex>,
) -> Result<Option<Wire>> {
    let rule = &input.layers[layer as usize];
    let Some(min) = rule.min_area else {
        return Ok(None);
    };
    let cap = limits.max_comparisons;
    let owned;
    let index = if let Some(index) = cache {
        index
    } else {
        owned = crate::rule_index::RuleIndex::new(input, out, &mut spatial.work, cap)?;
        &owned
    };
    let inside = |p: [i64; 2]| shapes.iter().any(|s| contains(*s, p));
    let mut anchors: Vec<[i64; 2]> = Vec::new();
    let vias: BTreeSet<[i64; 2]> = index
        .vias(out, net)?
        .filter(|v| {
            v.net == net
                && input.vias[v.kind as usize]
                    .shapes
                    .iter()
                    .any(|s| s.layer == layer)
                && inside(v.at)
        })
        .map(|v| v.at)
        .collect();
    anchors.extend(vias.iter().copied());
    let ends: BTreeSet<[i64; 2]> = index
        .wires(out, net)?
        .filter(|w| w.net == net && w.layer == layer)
        .flat_map(|w| [w.start, w.end])
        .filter(|p| inside(*p))
        .collect();
    anchors.extend(ends.into_iter().filter(|p| !vias.contains(p)));
    let others: Vec<Rect> = index
        .collect(input, out, net, limits, &mut spatial.work)?
        .remove(&(net, layer))
        .unwrap_or_default()
        .into_iter()
        .map(|shape| shape.rect)
        .collect();
    let span = shapes
        .iter()
        .map(|s| (s.ux - s.lx).max(s.uy - s.ly))
        .max()
        .unwrap_or(0);
    let steps = ((min / rule.width.max(1)) + 2 * span + 2 * input.grid) / input.grid + 1;
    let steps = steps.min(1 << 16);
    let dirs: [[i64; 2]; 4] = if rule.vertical == Some(true) {
        [[0, 1], [0, -1], [1, 0], [-1, 0]]
    } else {
        [[1, 0], [-1, 0], [0, 1], [0, -1]]
    };
    let stub = |a: [i64; 2], d: [i64; 2], n: i64| -> Result<(Wire, Rect)> {
        let end = [
            int(i128::from(a[0]) + i128::from(d[0]) * i128::from(n) * i128::from(input.grid))?,
            int(i128::from(a[1]) + i128::from(d[1]) * i128::from(n) * i128::from(input.grid))?,
        ];
        let w = Wire {
            net,
            layer,
            start: a,
            end,
        };
        let r = wire_rect(&w, rule.width)?;
        Ok((w, r))
    };
    for a in anchors {
        for d in dirs {
            let area = |n: i64, work: &mut usize| -> Result<i128> {
                let (_, r) = stub(a, d, n)?;
                let mut all = shapes.to_vec();
                all.push(r);
                route_rules::union_area(&all, work, cap)
            };
            if area(steps, &mut spatial.work)? < i128::from(min) {
                continue;
            }
            let (mut lo, mut hi) = (1i64, steps);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if area(mid, &mut spatial.work)? >= i128::from(min) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            let (w, r) = stub(a, d, lo)?;
            if !spatial.clear(layer, r, net)?
                || !route_rules::local_ok(r, &others, rule.spacing, &mut spatial.work, cap)?
            {
                continue;
            }
            return Ok(Some(w));
        }
    }
    Ok(None)
}

/// Independent of search history: check geometry against blockages and bounded
/// endpoint-graph connectivity. External OpenDB/KLayout are the physical oracle.
pub fn validate(input: &RouteInput, routes: &Routes, limits: &RouteLimits) -> Result<()> {
    preflight(input, limits)?;
    let length = routes.wires.iter().try_fold(0u64, |n, w| {
        n.checked_add(distance(w.start, w.end)?)
            .ok_or_else(|| budget("wire length overflow"))
    })?;
    let nodes = if input.tracks.is_some() {
        crate::troute::graph_nodes(input, limits)?
    } else {
        Lattice::new(input, limits)?.len
    };
    if routes.stats.grid_nodes != nodes
        || routes.stats.expansions > limits.max_expansions
        || routes.stats.comparisons > limits.max_comparisons
        || routes.stats.wire_length_dbu != length
    {
        return Err(invalid("routing telemetry/geometry mismatch"));
    }
    if routes.wires.len() + routes.vias.len() + routes.patches.len() > limits.max_segments
        || routes.access.len() > input.terminals.len()
    {
        return Err(budget("stored routing size"));
    }
    if !input.rules && !routes.patches.is_empty() {
        return Err(invalid("routing patches require LEF-rule mode"));
    }
    let mut spatial = Spatial::new(input, limits)?;
    let mut selected: Vec<u32> = input.terminals.iter().map(|t| t.net).collect();
    selected.sort_unstable();
    let (mut read, mut write) = (0, 0);
    while read < selected.len() {
        let net = selected[read];
        let start = read;
        while read < selected.len() && selected[read] == net {
            read += 1;
        }
        if read - start >= 2 {
            selected[write] = net;
            write += 1;
        }
    }
    selected.truncate(write);
    let selected = selected.into_boxed_slice();
    if selected.as_ref() != routes.nets {
        return Err(invalid("stored routed-net coverage"));
    }
    for w in &routes.wires {
        if selected.binary_search(&w.net).is_err()
            || w.layer as usize >= input.layers.len()
            || input.layers[w.layer as usize].vertical.is_none()
        {
            return Err(invalid("stored wire identity"));
        }
        if [w.start[0], w.start[1], w.end[0], w.end[1]]
            .iter()
            .any(|x| x % input.grid != 0)
        {
            return Err(invalid("off-grid routing"));
        }
        let rect = wire_rect(w, input.layers[w.layer as usize].width)?;
        if !spatial.clear(w.layer, rect, w.net)? {
            let cause = spatial.explain_collision(w.layer, rect, w.net)?;
            return Err(invalid(&format!(
                "wire collides with blockage/other net: {w:?}, rect {rect:?}, {cause}"
            )));
        }
        spatial.add_wire(w)?;
    }
    for v in &routes.vias {
        if selected.binary_search(&v.net).is_err()
            || v.kind as usize >= input.vias.len()
            || v.at.iter().any(|x| x % input.grid != 0)
        {
            return Err(invalid("stored via identity/grid"));
        }
        if !spatial.via_clear(v.kind as usize, v.at, v.net)? {
            for shape in &input.vias[v.kind as usize].shapes {
                let rect = shifted(shape.rect, v.at)?;
                if !spatial.clear(shape.layer, rect, v.net)? {
                    let cause = spatial.explain_collision(shape.layer, rect, v.net)?;
                    return Err(invalid(&format!(
                        "via collides with blockage/other net: {v:?}, layer {}, rect {rect:?}, {cause}",
                        shape.layer
                    )));
                }
            }
            return Err(invalid(
                "via collision not reproduced by bounded diagnostic",
            ));
        }
        spatial.add_via(v)?;
    }
    for p in &routes.patches {
        if selected.binary_search(&p.net).is_err()
            || p.layer as usize >= input.layers.len()
            || input.layers[p.layer as usize].vertical.is_none()
            || !p.rect.valid()
            || [p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy]
                .iter()
                .any(|x| x % input.grid != 0)
        {
            return Err(invalid("stored patch identity/grid"));
        }
        if !spatial.clear(p.layer, p.rect, p.net)? {
            return Err(invalid("patch collides with blockage/other net"));
        }
        spatial.add(Blockage {
            layer: p.layer,
            rect: p.rect,
            net: Some(p.net),
        })?;
    }
    let mut work = 0;
    let terminals = crate::rule_index::order(
        input.terminals.len(),
        |i| input.terminals[i].pin,
        &mut work,
        limits.max_comparisons,
    )?;
    let mut seen = vec![false; input.terminals.len()];
    for a in &routes.access {
        let slot = terminals
            .binary_search_by_key(&a.pin, |&i| input.terminals[i as usize].pin)
            .map_err(|_| invalid("routing access pin absent"))?;
        let i = terminals[slot] as usize;
        let t = &input.terminals[i];
        if selected.binary_search(&a.net).is_err()
            || t.net != a.net
            || seen[i]
            || !t
                .shapes
                .iter()
                .any(|s| s.layer == a.layer && contains(s.rect, a.at))
        {
            return Err(invalid("routing access does not touch its pin"));
        }
        seen[i] = true;
    }
    if input
        .terminals
        .iter()
        .enumerate()
        .any(|(i, t)| selected.binary_search(&t.net).is_ok() && !seen[i])
    {
        return Err(invalid("missing routed terminal"));
    }
    drop(terminals);
    drop(seen);
    let index = crate::route_graph::NetIndex::new(routes, limits, &mut work)?;
    for &net in index.nets() {
        let graph = crate::route_graph::Graph::new(
            input,
            routes,
            index.sources(routes, net),
            limits,
            &mut work,
        )?;
        if !graph.connected(&mut work, limits.max_comparisons)? {
            return Err(invalid("disconnected routed net"));
        }
    }
    drop(index);
    if input.rules {
        let mut work = 0usize;
        let violations = route_rules::check(input, routes, limits, &mut work)?;
        if let Some(v) = violations.first() {
            return Err(invalid(&format!(
                "ROUTING_RULE {} ({} total)",
                v.describe(),
                violations.len()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_net_pruning_matches_whole_graph_with_cycles_branches_and_vias() {
        let input = fixture();
        let mut state = 0x5a17u32;
        for case in 0..100 {
            let mut next = || {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                state ^ (state >> 16)
            };
            let mut original = Routes {
                wires: Vec::new(),
                vias: Vec::new(),
                access: Vec::new(),
                patches: Vec::new(),
                nets: (0..8).collect(),
                stats: RouteStats::default(),
            };
            for net in 0..8 {
                for _ in 0..24 {
                    let a = i64::from(next() % 8);
                    let b = i64::from(next() % 8);
                    original.wires.push(Wire {
                        net,
                        layer: next() % 2,
                        start: [a, 0],
                        end: [b, 0],
                    });
                }
                for _ in 0..4 {
                    original.vias.push(Via {
                        net,
                        kind: 0,
                        at: [i64::from(next() % 8), 0],
                    });
                }
                for pin in 0..4 {
                    original.access.push(Access {
                        pin: net * 4 + pin,
                        net,
                        layer: next() % 2,
                        at: [i64::from(next() % 8), 0],
                    });
                }
            }
            original.wires.reverse();
            original.vias.reverse();
            let mut expected = original.clone();
            prune_legacy(&input, &mut expected);
            prune(&input, &mut original, &RouteLimits::default()).unwrap();
            assert_eq!(original.wires, expected.wires, "wire pruning case {case}");
            assert_eq!(original.vias, expected.vias, "via pruning case {case}");
        }
    }

    #[test]
    fn endpoint_graph_rejects_disconnected_access_and_enforces_per_net_budget() {
        let input = fixture();
        let mut out = Routes {
            wires: vec![Wire {
                net: 0,
                layer: 0,
                start: [0, 0],
                end: [10, 0],
            }],
            vias: Vec::new(),
            access: vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 0,
                    at: [10, 0],
                },
            ],
            patches: Vec::new(),
            nets: vec![0],
            stats: RouteStats::default(),
        };
        let limits = RouteLimits::default();
        for connected in [true, false] {
            if !connected {
                out.access[1].at = [20, 0];
            }
            let mut work = 0;
            let index = crate::route_graph::NetIndex::new(&out, &limits, &mut work).unwrap();
            let graph = crate::route_graph::Graph::new(
                &input,
                &out,
                index.sources(&out, 0),
                &limits,
                &mut work,
            )
            .unwrap();
            assert_eq!(
                graph.connected(&mut work, limits.max_comparisons).unwrap(),
                connected
            );
            let tiny = RouteLimits {
                max_active_nodes: 1,
                ..limits.clone()
            };
            assert!(matches!(
                crate::route_graph::Graph::new(
                    &input,
                    &out,
                    index.sources(&out, 0),
                    &tiny,
                    &mut work
                ),
                Err(Error::Budget(_))
            ));
            assert!(graph.connected(&mut 0, 1).is_err());
        }
    }

    #[test]
    #[ignore = "explicit bounded whole-graph versus per-net RSS comparison"]
    fn endpoint_pruning_memory_fixture() {
        let input = fixture();
        let nets = 100_001u32;
        let mut out = Routes {
            wires: Vec::new(),
            vias: Vec::new(),
            access: Vec::new(),
            patches: Vec::new(),
            nets: (0..nets).collect(),
            stats: RouteStats::default(),
        };
        for net in 0..nets {
            for x in [0, 10, 20] {
                out.wires.push(Wire {
                    net,
                    layer: 0,
                    start: [x, 0],
                    end: [x + 10, 0],
                });
            }
            out.access.extend([
                Access {
                    pin: net * 2,
                    net,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: net * 2 + 1,
                    net,
                    layer: 0,
                    at: [20, 0],
                },
            ]);
        }
        if std::env::var_os("RUSTROAD_LEGACY_PRUNE").is_some() {
            prune_legacy(&input, &mut out);
        } else {
            prune(
                &input,
                &mut out,
                &RouteLimits {
                    max_segments: 400_004,
                    max_comparisons: 100_000_000,
                    ..RouteLimits::ceiling()
                },
            )
            .unwrap();
        }
        assert_eq!(out.wires.len(), 2 * nets as usize);
        assert!(out.wires.iter().all(|w| w.end[0] <= 20));
        std::hint::black_box(out);
    }

    #[test]
    fn instanced_wall_routes_identically_to_explicit_geometry() {
        let flat = fixture();
        let mut instanced = fixture();
        instanced.obstacles.clear();
        instanced.instances = Some(
            InstanceObstacles::new(
                vec![
                    ObstacleTemplate::new(
                        100,
                        0,
                        vec![TemplateObstacle {
                            layer: 0,
                            rect: Rect {
                                lx: 0,
                                ly: 0,
                                ux: 10,
                                uy: 100,
                            },
                            pin: None,
                        }],
                    )
                    .unwrap(),
                ],
                vec![ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [45, 0],
                    orientation: crate::Orientation::N,
                }],
                Vec::new(),
            )
            .unwrap(),
        );
        let limits = RouteLimits::default();
        let expected = route_cpu_ref(&flat, &limits).unwrap();
        let actual = route_cpu_ref(&instanced, &limits).unwrap();
        assert_eq!(actual.wires, expected.wires);
        assert_eq!(actual.vias, expected.vias);
        assert_eq!(actual.access, expected.access);
        validate(&instanced, &actual, &limits).unwrap();
    }

    #[test]
    fn instanced_flipped_pin_matches_explicit_negative_coordinates() {
        let mut flat = fixture();
        flat.die = Rect {
            lx: -100,
            ly: -100,
            ux: 100,
            uy: 100,
        };
        flat.obstacles = vec![Blockage {
            layer: 0,
            rect: Rect {
                lx: -45,
                ly: -40,
                ux: -35,
                uy: -30,
            },
            net: Some(7),
        }];
        let mut instanced = fixture();
        instanced.die = flat.die;
        instanced.obstacles.clear();
        instanced.instances = Some(
            InstanceObstacles::new(
                vec![
                    ObstacleTemplate::new(
                        30,
                        1,
                        vec![TemplateObstacle {
                            layer: 0,
                            rect: Rect {
                                lx: 0,
                                ly: 10,
                                ux: 10,
                                uy: 20,
                            },
                            pin: Some(0),
                        }],
                    )
                    .unwrap(),
                ],
                vec![ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [-45, -50],
                    orientation: crate::Orientation::FS,
                }],
                vec![7],
            )
            .unwrap(),
        );
        let limits = RouteLimits::default();
        let mut a = Spatial::new(&flat, &limits).unwrap();
        let mut b = Spatial::new(&instanced, &limits).unwrap();
        for net in [7, 8] {
            for x in (-60..0).step_by(5) {
                for y in (-60..0).step_by(5) {
                    let rect = Rect {
                        lx: x,
                        ly: y,
                        ux: x + 2,
                        uy: y + 2,
                    };
                    assert_eq!(
                        a.probe(0, rect, net).unwrap(),
                        b.probe(0, rect, net).unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn millions_of_virtual_obstacles_keep_compact_storage() {
        let mut input = fixture();
        input.die = Rect {
            lx: 0,
            ly: 0,
            ux: 20_000,
            uy: 200_000,
        };
        input.obstacles.clear();
        let template = ObstacleTemplate::new(
            100,
            0,
            (0..102)
                .map(|_| TemplateObstacle {
                    layer: 0,
                    rect: Rect {
                        lx: 1,
                        ly: 1,
                        ux: 3,
                        uy: 3,
                    },
                    pin: None,
                })
                .collect(),
        )
        .unwrap();
        let instances = (0..100_000)
            .map(|i| ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [(i % 1000) * 20, (i / 1000) * 100],
                orientation: crate::Orientation::N,
            })
            .collect();
        let provider = InstanceObstacles::new(vec![template], instances, Vec::new()).unwrap();
        assert_eq!(provider.virtual_shapes(), 10_200_000);
        assert!(provider.heap_bytes() < 8 * 1024 * 1024);
        input.instances = Some(provider);
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(
            !spatial
                .clear(
                    0,
                    Rect {
                        lx: 1,
                        ly: 1,
                        ux: 2,
                        uy: 2
                    },
                    5
                )
                .unwrap()
        );
        assert!(spatial.shapes.is_empty());
    }

    #[test]
    fn every_bin_mode_matches_an_independent_ordered_bucket_oracle() {
        let mut reference = BTreeMap::<(u32, i64, i64), Vec<usize>>::new();
        let mut sequence = Vec::new();
        let mut random = 0x25bd_7c91_ae02_1843u64;
        for id in 0..4096usize {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let x = ((random >> 16) % 201) as i64 - 100;
            let y = ((random >> 32) % 201) as i64 - 100;
            let layer = (random & 1) as u32;
            sequence.push((layer, x, y, id));
            reference.entry((layer, x, y)).or_default().push(id);
        }
        for mode in [BinMode::Dense, BinMode::Sparse, BinMode::Paged] {
            let mut grid = BinGrid {
                mode,
                bx0: -64,
                by0: -64,
                nx: 129,
                ny: 129,
                cells: vec![Vec::new(); 2],
                entries: Vec::new(),
                pages: HashMap::with_hasher(BuildFx),
                overflow: HashMap::with_hasher(BuildFx),
            };
            for &(layer, x, y, id) in &sequence {
                grid.push(layer, x, y, id).unwrap();
            }
            for (&(layer, x, y), expected) in &reference {
                assert_eq!(grid.iter(layer, x, y).collect::<Vec<_>>(), *expected);
            }
            assert_eq!(grid.iter(0, 300, -300).count(), 0);
        }
    }

    #[test]
    fn immutable_obstacles_are_shared_and_survive_route_ripup() {
        let input = fixture();
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(spatial.shapes.is_empty());
        assert!(std::ptr::eq(spatial.shape(0), &input.obstacles[0]));
        let fixed = Rect {
            lx: 46,
            ly: 20,
            ux: 54,
            uy: 30,
        };
        assert!(!spatial.clear(0, fixed, 8).unwrap());
        let routed = Rect {
            lx: 10,
            ly: 10,
            ux: 20,
            uy: 20,
        };
        let id = spatial
            .add_routed(Blockage {
                layer: 0,
                rect: routed,
                net: Some(7),
            })
            .unwrap();
        assert_eq!(id, input.obstacles.len());
        assert!(!spatial.clear(0, routed, 8).unwrap());
        assert!(matches!(spatial.probe_soft(0, routed, 8).unwrap(),
            Soft::Clear { victims, .. } if victims == [7]));
        spatial.remove(id);
        assert!(spatial.clear(0, routed, 8).unwrap());
        assert!(!spatial.clear(0, fixed, 8).unwrap());
    }

    #[test]
    fn large_spatial_extent_allocates_only_touched_pages() {
        let mut input = fixture();
        input.die = Rect {
            lx: -100_000,
            ly: -100_000,
            ux: 100_000,
            uy: 100_000,
        };
        input.obstacles = vec![input.obstacles[0].clone(); 1024];
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(spatial.bins.mode == BinMode::Paged);
        assert_eq!(spatial.bins.pages.len(), 1);
        let fixed = Rect {
            lx: 46,
            ly: 20,
            ux: 54,
            uy: 30,
        };
        assert!(!spatial.clear(0, fixed, 8).unwrap());
        let negative = Rect {
            lx: -100,
            ly: -100,
            ux: -90,
            uy: -90,
        };
        let id = spatial
            .add_routed(Blockage {
                layer: 0,
                rect: negative,
                net: Some(7),
            })
            .unwrap();
        assert_eq!(spatial.bins.pages.len(), 2);
        assert!(!spatial.clear(0, negative, 8).unwrap());
        spatial.remove(id);
        assert!(spatial.clear(0, negative, 8).unwrap());
    }

    fn fixture() -> RouteInput {
        let layers = vec![
            RouteLayer {
                name: "M1".into(),
                width: 2,
                spacing: 2,
                vertical: Some(false),
                min_area: None,
                min_step: None,
            },
            RouteLayer {
                name: "M2".into(),
                width: 2,
                spacing: 2,
                vertical: Some(true),
                min_area: None,
                min_step: None,
            },
            RouteLayer {
                name: "V1".into(),
                width: 2,
                spacing: 2,
                vertical: None,
                min_area: None,
                min_step: None,
            },
        ];
        RouteInput {
            die: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 100,
            },
            grid: 1,
            step: 10,
            access_layer: 1,
            layers,
            vias: vec![RouteVia {
                name: "V12".into(),
                lower: 0,
                upper: 1,
                shapes: (0..3)
                    .map(|layer| LayerRect {
                        layer,
                        rect: Rect {
                            lx: -1,
                            ly: -1,
                            ux: 1,
                            uy: 1,
                        },
                    })
                    .collect(),
            }],
            obstacles: vec![Blockage {
                layer: 0,
                rect: Rect {
                    lx: 45,
                    ly: 0,
                    ux: 55,
                    uy: 100,
                },
                net: None,
            }],
            terminals: vec![
                Terminal {
                    pin: 0,
                    net: 0,
                    driver: true,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: Rect {
                            lx: 9,
                            ly: 9,
                            ux: 11,
                            uy: 11,
                        },
                    }],
                    class: None,
                },
                Terminal {
                    pin: 1,
                    net: 0,
                    driver: false,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: Rect {
                            lx: 89,
                            ly: 89,
                            ux: 91,
                            uy: 91,
                        },
                    }],
                    class: None,
                },
            ],
            rules: false,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }

    fn rect(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    fn layer(name: &str, vertical: Option<bool>, spacing: i64) -> RouteLayer {
        RouteLayer {
            name: name.into(),
            width: 2,
            spacing,
            vertical,
            min_area: None,
            min_step: None,
        }
    }
    fn via(name: &str, m1: Rect) -> RouteVia {
        RouteVia {
            name: name.into(),
            lower: 0,
            upper: 1,
            shapes: vec![
                LayerRect { layer: 0, rect: m1 },
                LayerRect {
                    layer: 2,
                    rect: rect(-1, -1, 1, 1),
                },
                LayerRect {
                    layer: 1,
                    rect: rect(-1, -1, 1, 1),
                },
            ],
        }
    }
    fn terminal(pin: u32, driver: bool, shapes: Vec<Rect>) -> Terminal {
        Terminal {
            pin,
            net: 0,
            driver,
            shapes: shapes
                .into_iter()
                .map(|rect| LayerRect { layer: 0, rect })
                .collect(),
            class: None,
        }
    }
    /// Two M1 pins of one net, no obstacles; M1 spacing 4, cut spacing 0.
    fn rules_input() -> RouteInput {
        RouteInput {
            die: rect(0, 0, 100, 100),
            grid: 1,
            step: 10,
            access_layer: 1,
            layers: vec![
                layer("M1", Some(false), 4),
                layer("M2", Some(true), 4),
                layer("V1", None, 0),
            ],
            vias: vec![via("V12", rect(-1, -1, 1, 1))],
            obstacles: Vec::new(),
            terminals: vec![
                terminal(0, true, vec![rect(9, 9, 11, 11)]),
                terminal(1, false, vec![rect(9, 24, 11, 26)]),
            ],
            rules: true,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }
    fn manual(
        input: &RouteInput,
        wires: &[([i64; 2], [i64; 2], u32)],
        vias: &[[i64; 2]],
    ) -> Routes {
        let wires: Vec<_> = wires
            .iter()
            .map(|&(start, end, layer)| Wire {
                net: 0,
                layer,
                start,
                end,
            })
            .collect();
        Routes {
            stats: RouteStats {
                grid_nodes: Lattice::new(input, &RouteLimits::default()).unwrap().len,
                expansions: 0,
                comparisons: 0,
                wire_length_dbu: wires
                    .iter()
                    .map(|w| distance(w.start, w.end).unwrap())
                    .sum(),
            },
            wires,
            vias: vias
                .iter()
                .map(|&at| Via {
                    net: 0,
                    kind: 0,
                    at,
                })
                .collect(),
            access: vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [10, 10],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 0,
                    at: [10, 25],
                },
            ],
            patches: Vec::new(),
            nets: vec![0],
        }
    }
    #[test]
    fn planted_same_net_notch_is_rejected_by_native_validate() {
        let mut input = rules_input();
        // Straight connection plus a dangling hook 2 DBU from it (< spacing 4).
        let routes = manual(
            &input,
            &[
                ([10, 10], [10, 25], 0),
                ([10, 25], [14, 25], 0),
                ([14, 25], [14, 12], 0),
            ],
            &[],
        );
        let err = validate(&input, &routes, &RouteLimits::default()).unwrap_err();
        assert!(
            err.to_string().contains("ROUTING_RULE same-net notch"),
            "{err}"
        );
        // The legacy checker exempts same-net shapes and accepts the defect.
        input.rules = false;
        validate(&input, &routes, &RouteLimits::default()).unwrap();
        // Without the hook the same geometry is clean under the rules.
        input.rules = true;
        let clean = manual(&input, &[([10, 10], [10, 25], 0)], &[]);
        validate(&input, &clean, &RouteLimits::default()).unwrap();
    }
    #[test]
    fn planted_min_area_island_and_floating_patch_are_rejected() {
        let mut input = rules_input();
        input.layers[1].min_area = Some(50);
        // Up and immediately down: the M2 island is 2 x 4 = 8 DBU^2 < 50.
        let routes = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let err = validate(&input, &routes, &RouteLimits::default()).unwrap_err();
        assert!(err.to_string().contains("minimum area"), "{err}");
        input.rules = false;
        validate(&input, &routes, &RouteLimits::default()).unwrap();
        // A patch must touch same-net routed metal.
        input.rules = true;
        let mut floating = manual(&input, &[([10, 10], [10, 25], 0)], &[]);
        floating.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(40, 40, 44, 44),
        });
        let err = validate(&input, &floating, &RouteLimits::default()).unwrap_err();
        assert!(err.to_string().contains("floating patch"), "{err}");
        // Patches are never legal in legacy mode.
        input.rules = false;
        assert!(validate(&input, &floating, &RouteLimits::default()).is_err());
    }
    #[test]
    fn cleanup_preserves_reserved_clearance_when_choosing_area_stubs() {
        let mut input = rules_input();
        input.layers[1].min_area = Some(50);
        let original = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let limits = RouteLimits::default();
        let mut first = original.clone();
        assert!(
            cleanup_report(&input, &mut first, &limits)
                .unwrap()
                .1
                .is_empty()
        );
        let added = first
            .wires
            .iter()
            .find(|w| !original.wires.contains(w))
            .unwrap();
        let reservation = Blockage {
            layer: added.layer,
            rect: rect(
                added.end[0] - 1,
                added.end[1] - 1,
                added.end[0] + 1,
                added.end[1] + 1,
            ),
            net: Some(99),
        };
        let mut check = Spatial::new(&input, &limits).unwrap();
        for wire in &original.wires {
            check.add_wire(wire).unwrap();
        }
        for via in &original.vias {
            check.add_via(via).unwrap();
        }
        assert!(
            check
                .clear(reservation.layer, reservation.rect, 99)
                .unwrap(),
            "reservation must clear the original route"
        );
        let mut changed = original.clone();
        assert!(
            cleanup_with_reservations(&input, &mut changed, &limits, [reservation.clone()])
                .unwrap()
                .1
                .is_empty()
        );
        assert_ne!(first.wires, changed.wires);
        let mut clearance = Spatial::new(&input, &limits).unwrap();
        clearance.add_fixed(reservation).unwrap();
        for wire in &changed.wires {
            assert!(
                clearance
                    .clear(
                        wire.layer,
                        wire_rect(wire, input.layers[wire.layer as usize].width).unwrap(),
                        wire.net
                    )
                    .unwrap()
            );
        }
    }

    #[test]
    fn cleanup_charges_joint_rule_work_and_refuses_extra_output_records() {
        let mut input = rules_input();
        input.layers[1].min_area = Some(50);
        let original = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let (work, remaining) =
            cleanup_report(&input, &mut original.clone(), &RouteLimits::default()).unwrap();
        assert!(remaining.is_empty());
        assert!(work > 1);
        let too_little_work = RouteLimits {
            max_comparisons: work - 1,
            ..Default::default()
        };
        assert!(matches!(
            cleanup_report(&input, &mut original.clone(), &too_little_work),
            Err(Error::Budget(_))
        ));
        let full_output = RouteLimits {
            max_segments: original.wires.len() + original.vias.len(),
            ..Default::default()
        };
        assert!(matches!(
            cleanup_report(&input, &mut original.clone(), &full_output),
            Err(Error::Budget(_))
        ));
    }

    #[test]
    fn rules_mode_adds_min_area_stub_and_is_deterministic() {
        // A full-height M1 wall forces an M2 hop that is below its area rule.
        let mut input = rules_input();
        input.terminals = vec![
            terminal(0, true, vec![rect(9, 49, 11, 51)]),
            terminal(1, false, vec![rect(89, 49, 91, 51)]),
        ];
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(49, 0, 51, 100),
            net: None,
        });
        input.layers[1].min_area = Some(120);
        let limits = RouteLimits::default();
        let a = route_cpu_ref(&input, &limits).unwrap();
        let b = route_cpu_ref(&input, &limits).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert_eq!(a.patches, b.patches);
        validate(&input, &a, &limits).unwrap();
        let mut work = 0;
        assert!(
            route_rules::check(&input, &a, &limits, &mut work)
                .unwrap()
                .is_empty()
        );
        // Without the area rule the same hop routes with less M2 metal.
        input.layers[1].min_area = None;
        let plain = route_cpu_ref(&input, &limits).unwrap();
        let m2 = |r: &Routes| -> u64 {
            r.wires
                .iter()
                .filter(|w| w.layer == 1)
                .map(|w| distance(w.start, w.end).unwrap())
                .sum()
        };
        assert!(m2(&a) > m2(&plain), "{} <= {}", m2(&a), m2(&plain));
        // Stored routes from the area-free run violate the stricter rule.
        input.layers[1].min_area = Some(120);
        assert!(validate(&input, &plain, &limits).is_err());
    }
    #[test]
    fn access_table_picks_the_variant_whose_pad_fits_the_pin() {
        let mut input = rules_input();
        input.layers[0].spacing = 3;
        input.layers[1].spacing = 3;
        // Wide pad (6 x 2) versus rotated pad (2 x 6) on a 4-wide pin leg.
        input.vias = vec![
            via("V12", rect(-3, -1, 3, 1)),
            via("V12_R", rect(-1, -3, 1, 3)),
        ];
        let leg = rect(8, 0, 12, 30);
        let bar = rect(8, 30, 30, 34);
        input.access_classes = vec![AccessClass {
            shapes: vec![
                LayerRect {
                    layer: 0,
                    rect: leg,
                },
                LayerRect {
                    layer: 0,
                    rect: bar,
                },
            ],
            blockers: Vec::new(),
        }];
        let mut t0 = terminal(0, true, vec![leg, bar]);
        t0.class = Some((0, [0, 0]));
        input.terminals = vec![t0, terminal(1, false, vec![rect(79, 79, 81, 81)])];
        // An M1 wall keeps the landing via in the final route (no pruning).
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(49, 0, 51, 100),
            net: None,
        });
        let limits = RouteLimits::default();
        let grid = Lattice::new(&input, &limits).unwrap();
        let mut work = 0;
        let table =
            access_table(&input, &grid, &input.access_classes[0], &mut work, &limits).unwrap();
        let name = |k: usize| input.vias[k].name.as_str();
        // Leg points land the rotated pad inside the pin; bar points the wide one.
        let on = |r: Rect, e: &AccessEntry| contains(r, e.at) && !contains(bar, e.at);
        assert!(table.iter().any(|e| on(leg, e) && e.fits));
        for e in &table {
            if on(leg, e) && e.fits {
                assert_eq!(name(e.kinds[0]), "V12_R");
            }
            if contains(bar, e.at) && !contains(leg, e.at) && e.fits {
                assert_eq!(name(e.kinds[0]), "V12");
            }
        }
        // Table order puts fitting landings first.
        let first_misfit = table.iter().position(|e| !e.fits).unwrap_or(table.len());
        assert!(table[first_misfit..].iter().all(|e| !e.fits));
        let routes = route_cpu_ref(&input, &limits).unwrap();
        validate(&input, &routes, &limits).unwrap();
        let access = routes.access.iter().find(|a| a.pin == 0).unwrap();
        if let Some(landing) = routes.vias.iter().find(|v| v.at == access.at) {
            assert_eq!(name(landing.kind as usize), "V12_R");
        }
        assert_eq!(
            route_cpu_ref(&input, &limits).unwrap().wires,
            routes.wires,
            "rules-mode routing must be repeatable"
        );
        // A class whose shapes disagree with the terminal is rejected.
        input.terminals[0].class = Some((0, [1, 0]));
        assert!(route_cpu_ref(&input, &limits).is_err());
        // Legacy mode refuses multiple vias for one layer pair.
        input.terminals[0].class = None;
        input.access_classes.clear();
        input.rules = false;
        assert!(route_cpu_ref(&input, &limits).is_err());
    }
    #[test]
    fn routes_around_blockage_using_vias_and_is_deterministic() {
        let i = fixture();
        let a = route_cpu_ref(&i, &RouteLimits::default()).unwrap();
        let b = route_cpu_ref(&i, &RouteLimits::default()).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert!(a.vias.len() >= 2);
        validate(&i, &a, &RouteLimits::default()).unwrap();
        let mut bad = a.clone();
        bad.vias.clear();
        assert!(validate(&i, &bad, &RouteLimits::default()).is_err());
    }
    #[test]
    fn routing_work_and_memory_caps_fail_explicitly() {
        let i = fixture();
        let l = RouteLimits {
            max_grid_nodes: 1,
            ..Default::default()
        };
        assert!(matches!(route_cpu_ref(&i, &l), Err(Error::Budget(_))));
        let l = RouteLimits {
            max_search_expansions: 1,
            ..Default::default()
        };
        assert!(matches!(route_cpu_ref(&i, &l), Err(Error::Budget(_))));
        let mut i = fixture();
        i.vias.clear();
        assert!(route_cpu_ref(&i, &RouteLimits::default()).is_err());
    }
    #[test]
    fn loaded_footprint_keeps_headroom_and_conservative_fallback() {
        let input = rules_input();
        let fallback = held_estimate(1 << 30, &input);
        assert_eq!(loaded_held_estimate(1 << 30, &input, None), fallback);
        assert_eq!(loaded_held_estimate(1 << 30, &input, Some(0)), fallback);
        assert_eq!(
            loaded_held_estimate(1 << 30, &input, Some(1 << 30)),
            5 << 28
        );
        assert!(loaded_held_estimate(0, &input, Some(1)) >= PROCESS_BASE_BYTES);
        assert_eq!(loaded_held_estimate(0, &input, Some(u64::MAX)), u64::MAX);
    }
}
