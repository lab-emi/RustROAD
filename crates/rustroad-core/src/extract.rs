//! Bounded per-net RC-tree extraction compute view (`cpu_ref`).
//!
//! Inputs are the router's checked lattice records (wires, fixed vias, pin
//! access points) plus every other conductor shape on the routing layers. The
//! geometry stays integer DBU; resistance and capacitance are f64 (ohm, pF).
//! Nothing here enumerates paths: each net is one graph walk, each straight
//! wire run is one resistor, and neighbour search is a bounded binned query.
//! Model, budgets and the memory estimate are in docs/extraction.md.
use crate::routing::{BuildFx, RouteLayer, RouteVia, Routes, Terminal};
use crate::{Error, Rect, Result};
use std::collections::{BTreeMap, HashMap};
#[cfg(test)]
mod bins;
mod geometry;
mod index;
mod parallel;
mod strip;
use crate::routing::InstanceQueryScratch;
use geometry::{InstanceGeometry, Shapes};
pub use parallel::MAX_TILE_UNITS;

/// One vertical context of a calibration table.
#[derive(Clone, Debug)]
pub struct RcContext {
    /// Calibration metal index of the nearest conductor below (0 = substrate).
    pub over: u32,
    /// Calibration metal index of the nearest conductor above, if any.
    pub under: Option<u32>,
    /// `[edge_spacing_um, coupling_pf_per_um, ground_pf_per_um_per_side]`,
    /// strictly increasing spacing.
    pub rows: Vec<[f64; 3]>,
}
/// Electrical model of one routing layer.
#[derive(Clone, Debug)]
pub struct RcLayer {
    pub metal: u32,
    /// Wire width the calibration rows describe, in DBU.
    pub width: i64,
    pub sheet_ohm: f64,
    pub contexts: Vec<RcContext>,
    /// Area capacitance of this layer's face to the substrate, pF/um^2 (the
    /// share a non-plate overlap below removes from the face); None when the
    /// profile has no overlap-area data (the term is then off).
    pub area_cap: Option<f64>,
    /// `(calibration metal below, pF/um^2)`: area capacitance of this layer's
    /// face over metal of that layer where it is not a calibration plate.
    pub overlap: Vec<(u32, f64)>,
}
/// Corner model bound to one routing input.
#[derive(Clone, Debug)]
pub struct RcTech {
    pub dbu_per_um: f64,
    /// Indexed like the routing layers; `None` for cut layers.
    pub layers: Vec<Option<RcLayer>>,
    /// Resistance of each routing via kind (all cuts in parallel), ohm.
    pub via_ohm: Vec<f64>,
}
/// Owner of a conductor that is not a routed wire of a net.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassiveOwner {
    /// Supply metal, obstructions and unconnected pins: AC ground.
    Ground,
    /// A signal pin or port shape; coupling attaches to that pin's node.
    Pin { net: u32, pin: u32 },
}
#[derive(Clone, Debug)]
pub struct PassiveShape {
    pub layer: u32,
    pub rect: Rect,
    pub owner: PassiveOwner,
}
pub struct ExtractInput<'a> {
    pub layers: &'a [RouteLayer],
    pub vias: &'a [RouteVia],
    pub routes: &'a Routes,
    /// When set, only the routed records (wires, vias, access points,
    /// patches, route nets) and terminals of nets `n` with `nets[n]` are
    /// extracted; every other routed record is ignored, exactly as if the
    /// caller had removed it (it passes that metal as passive shapes).
    pub nets: Option<&'a [bool]>,
    pub terminals: &'a [Terminal],
    pub passive: &'a [PassiveShape],
    /// Shared cell pin/obstruction geometry, decoded only for local queries.
    pub instances: Option<InstancedPassive<'a>>,
    /// Caller-held memory allowance; use 0 when only the process probe is available.
    pub held_bytes: u64,
    pub tech: &'a RcTech,
}
#[derive(Clone, Copy)]
pub struct InstancedPassive<'a> {
    pub geometry: &'a crate::instance_obstacles::InstanceObstacles,
    /// True for signal net ids; supply and synthetic rail-tie nets are ground.
    pub signal_nets: &'a [bool],
}
#[derive(Clone, Debug)]
pub struct ExtractLimits {
    pub memory_mib: u64,
    /// All conductor shapes (wire runs, via pads, pins, obstructions, supply).
    pub max_shapes: usize,
    /// Virtual shared-template shapes, distinct from retained route records.
    pub max_virtual_shapes: usize,
    /// Entries in the cell-instance spatial index, not per-rectangle entries.
    pub max_instance_bin_entries: usize,
    /// Electrical nodes over all nets.
    pub max_nodes: usize,
    /// Resistors plus ground and coupling capacitors.
    pub max_elements: usize,
    pub max_couplings: usize,
    /// Bin-entry scans, interval-index work and active-candidate comparisons.
    pub max_work: usize,
    /// Candidate shapes considered for one wire run. Its scratch memory is
    /// `CANDIDATE_SCRATCH_BYTES` per candidate (the per-run candidate list,
    /// the interval sweep and the plane list); the ceiling keeps that
    /// below `CANDIDATE_SCRATCH_CEILING_BYTES`. Work (including active intervals)
    /// is charged to `max_work`.
    pub max_candidates: usize,
    /// Accepted for compatibility (1..=16) and otherwise ignored: strips
    /// are computed on the caller's Rayon pool (`--threads`).
    pub query_workers: usize,
    /// Accepted for compatibility (1..=64) and otherwise ignored.
    pub query_batch_runs: usize,
    /// Accepted for compatibility (1..=64) and otherwise ignored; parallel
    /// strip scratch is admitted against `memory_mib`.
    pub query_scratch_mib: u64,
    /// Strip units per parallel chunk (commit granularity); 0 selects the
    /// default of 64. A non-zero value also forces the chunked path on one
    /// thread. Test knob: results are identical for every value.
    pub tile_units: usize,
    /// Spatial-bin entries.
    pub max_bin_entries: usize,
    /// Straight runs longer than this are split at existing lattice nodes (DBU).
    pub max_segment_length: i64,
    /// Coupling capacitors below this value (pF) are written as ground
    /// capacitance at both of their nodes; 0 keeps every coupling.
    pub min_coupling_pf: f64,
}
impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            memory_mib: 2048,
            max_shapes: 2_000_000,
            max_virtual_shapes: 256_000_000,
            max_instance_bin_entries: 64_000_000,
            max_nodes: 500_000,
            max_elements: 1_000_000,
            max_couplings: 1_000_000,
            max_work: 200_000_000,
            max_candidates: 4096,
            query_workers: 1,
            query_batch_runs: 8,
            query_scratch_mib: 32,
            tile_units: 0,
            max_bin_entries: 8_000_000,
            max_segment_length: 25_000,
            min_coupling_pf: 0.0,
        }
    }
}
/// Conservative scratch bytes per candidate, including vector capacity,
/// the deduplication hash set, two interval events, active tree and plane list.
pub const CANDIDATE_SCRATCH_BYTES: usize = 256;
/// Rule-9 ceiling of the per-run candidate scratch (one run at a time).
pub const CANDIDATE_SCRATCH_CEILING_BYTES: usize = 48 << 20;
/// Absolute ceiling of `max_candidates` derived from the scratch ceiling.
pub const MAX_CANDIDATES_CEILING: usize = CANDIDATE_SCRATCH_CEILING_BYTES / CANDIDATE_SCRATCH_BYTES;
impl ExtractLimits {
    /// Upfront scratch estimate of the per-run candidate buffers (bytes).
    pub fn candidate_scratch_bytes(&self) -> usize {
        self.max_candidates.saturating_mul(CANDIDATE_SCRATCH_BYTES)
    }
    pub fn validate(&self) -> Result<()> {
        for (value, ceiling) in [
            (self.max_shapes, 128_000_000),
            (self.max_virtual_shapes, 1_000_000_000),
            (self.max_instance_bin_entries, 512_000_000),
            (self.max_nodes, 64_000_000),
            (self.max_elements, 256_000_000),
            (self.max_couplings, 128_000_000),
            // Work counts comparisons, not memory (bounded by the shape,
            // node, element and bin caps): SERV-size blocks need ~2-5e9.
            (self.max_work, 1usize << 40),
            (self.max_candidates, MAX_CANDIDATES_CEILING),
            (self.query_workers, 16),
            (self.query_batch_runs, 64),
            (self.max_bin_entries, 512_000_000),
        ] {
            if value == 0 || value > ceiling {
                return Err(budget("extraction limit outside hard bounds"));
            }
        }
        if self.query_scratch_mib == 0 || self.query_scratch_mib > 64 {
            return Err(budget("extraction query scratch allowance"));
        }
        if self.tile_units > MAX_TILE_UNITS {
            return Err(budget("extraction tile size"));
        }
        if self.memory_mib == 0 || self.memory_mib > 65_536 {
            return Err(budget("extraction memory allowance"));
        }
        if self.max_segment_length <= 0 {
            return Err(invalid("extraction segment length must be positive"));
        }
        if !(self.min_coupling_pf.is_finite() && (0.0..=0.01).contains(&self.min_coupling_pf)) {
            return Err(invalid(
                "extraction coupling threshold must be within 0..=10 fF",
            ));
        }
        Ok(())
    }
}
/// Node of one net's RC tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RcNode {
    /// A routed terminal (instance pin or port), by connectivity pin index.
    Pin(u32),
    /// Internal junction, numbered from 1 in tree order.
    Internal(u32),
}
#[derive(Clone, Debug)]
pub struct RcNet {
    pub net: u32,
    /// Node 0 is the driver.
    pub nodes: Vec<RcNode>,
    /// Grounded capacitance per node, pF.
    pub ground: Vec<f64>,
    /// `(node, node, ohm)` tree edges; zero ohm only for coincident pins.
    pub resistors: Vec<(u32, u32, f64)>,
}
/// Cross-net coupling capacitor between `(net, node)` endpoints, pF.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RcCoupling {
    pub a: (u32, u32),
    pub b: (u32, u32),
    pub pf: f64,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ExtractStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<ExtractMemory>,
    pub nets: usize,
    pub lattice_wires: usize,
    pub wire_runs: usize,
    pub vias: usize,
    pub nodes: usize,
    pub resistors: usize,
    /// Loop-closing conductors whose resistance was dropped to keep a tree.
    pub opened_loop_resistors: usize,
    pub coincident_pin_links: usize,
    /// Wire-record pieces repeating metal another record of the net already
    /// stores (collinear overlap); counted once.
    pub duplicate_wire_edges: usize,
    /// Cut points of runs longer than the segment limit that are not stored
    /// nodes; each becomes an internal RC node.
    pub segment_cut_nodes: usize,
    pub couplings: usize,
    /// Couplings below `min_coupling_pf`, written as ground at both nodes.
    pub grounded_couplings: usize,
    pub shapes: usize,
    pub intervals: usize,
    pub work: usize,
    /// Largest candidate list of one wire run (checked against
    /// `ExtractLimits::max_candidates`).
    pub max_run_candidates: usize,
    /// Intervals whose exact over/under calibration context was absent.
    pub context_fallback_intervals: usize,
    /// Different-net shapes overlapping a wire run (a short; not extracted).
    pub foreign_overlaps: usize,
    /// Coupling to a pin shape whose net has no RC tree (grounded instead).
    pub unrouted_pin_partners: usize,
    /// Routing patches (same-net fill metal) and their pieces not already
    /// covered by same-net conductors; each piece adds capacitance only.
    pub patches: usize,
    pub patch_pieces: usize,
    /// Pieces of the half-width end extensions of wire runs (DEF default
    /// wire extension) not covered by same-net metal; capacitance only.
    pub end_pieces: usize,
    /// Pieces of via landing pads (LEF via geometry on routing layers) not
    /// covered by same-net metal; capacitance only.
    pub pad_pieces: usize,
    /// Wire-run side intervals inside same-net metal (not exposed, no C).
    pub covered_side_intervals: usize,
    /// Intervals whose nearest other-layer metal overlaps without spanning
    /// the strip (parallel or offset): not a calibration plate; its overlap
    /// area is extracted when the profile carries overlap data.
    pub nonspanning_plane_intervals: usize,
    /// Strips of passive conductors (pins, obstructions, supply) that a routed
    /// conductor on another layer overlaps, and their intervals: each counts
    /// its own cross-section where a routed conductor is its plate.
    pub passive_strips: usize,
    pub passive_intervals: usize,
    /// Non-plate overlap area extracted (um^2, face of the upper conductor)
    /// and overlapped layer pairs without overlap data (left out, counted).
    pub overlap_area_um2: f64,
    pub overlap_pairs_without_data: usize,
    pub wire_length_dbu: u64,
    pub ground_pf: f64,
    /// Each coupling capacitor counted once.
    pub coupling_pf: f64,
    pub resistance_ohm: f64,
}
#[derive(Clone, Debug)]
pub struct RcNetwork {
    pub nets: Vec<RcNet>,
    pub couplings: Vec<RcCoupling>,
    pub stats: ExtractStats,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct ExtractMemory {
    pub held_bytes: u64,
    pub working_bytes: u64,
    pub geometry_bytes: u64,
    pub estimate_bytes: u64,
    pub limit_bytes: u64,
    /// Actual cell-geometry query bin, DBU; coarse when refinement cannot fit.
    pub instance_query_bin_dbu: i64,
}

impl ExtractInput<'_> {
    /// Whether routed records of `net` are extracted.
    #[inline]
    fn extracts(&self, net: u32) -> bool {
        self.nets
            .is_none_or(|m| m.get(net as usize).copied().unwrap_or(false))
    }
}

fn extraction_bin(input: &ExtractInput<'_>) -> Result<i64> {
    let window = input
        .tech
        .layers
        .iter()
        .flatten()
        .flat_map(|m| &m.contexts)
        .map(|c| c.rows[c.rows.len() - 1][0])
        .fold(0.0f64, f64::max);
    let size = (window * input.tech.dbu_per_um).ceil().max(1.0);
    if !size.is_finite() || size > (i64::MAX / 2) as f64 {
        return Err(budget("extraction bin size"));
    }
    Ok((size as i64) * 2)
}

fn instance_query_bin(input: &ExtractInput<'_>) -> Result<i64> {
    let window = input
        .tech
        .layers
        .iter()
        .flatten()
        .map(|model| {
            model
                .contexts
                .iter()
                .map(|c| c.rows[c.rows.len() - 1][0])
                .fold(0.0f64, f64::max)
        })
        .reduce(f64::min)
        .unwrap_or(0.0);
    let size = (window * input.tech.dbu_per_um).ceil().max(1.0);
    if !size.is_finite() || size > (i64::MAX / 2) as f64 {
        return Err(budget("extraction instance query bin size"));
    }
    Ok((size as i64) * 2)
}

fn bin_count(bounds: &[Option<Rect>], bin: i64, cap: usize) -> u64 {
    let cap = cap as u128;
    let mut total = 0u128;
    for rect in bounds.iter().flatten() {
        let width =
            (i128::from(rect.ux.div_euclid(bin)) - i128::from(rect.lx.div_euclid(bin)) + 1) as u128;
        let height =
            (i128::from(rect.uy.div_euclid(bin)) - i128::from(rect.ly.div_euclid(bin)) + 1) as u128;
        total = total.saturating_add(width.saturating_mul(height)).min(cap);
    }
    total as u64
}

fn conductor_bounds(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<Vec<Option<Rect>>> {
    let mut bounds = vec![None::<Rect>; input.layers.len()];
    let mut work = 0usize;
    let mut include = |layer: u32, rect: Rect| -> Result<()> {
        work = work
            .checked_add(1)
            .filter(|&n| n <= limits.max_work)
            .ok_or_else(|| budget("extraction bin estimate work"))?;
        let item = bounds
            .get_mut(layer as usize)
            .ok_or_else(|| invalid("extraction shape layer"))?;
        if !rect.valid() {
            return Err(invalid("extraction shape bounds"));
        }
        if input.layers[layer as usize].vertical.is_none() {
            return Ok(());
        }
        *item = Some(match *item {
            None => rect,
            Some(r) => Rect {
                lx: r.lx.min(rect.lx),
                ly: r.ly.min(rect.ly),
                ux: r.ux.max(rect.ux),
                uy: r.uy.max(rect.uy),
            },
        });
        Ok(())
    };
    for wire in input.routes.wires.iter().filter(|w| input.extracts(w.net)) {
        let layer = input
            .layers
            .get(wire.layer as usize)
            .ok_or_else(|| invalid("extraction wire layer"))?;
        include(wire.layer, crate::routing::wire_rect(wire, layer.width)?)?;
    }
    for via in input.routes.vias.iter().filter(|v| input.extracts(v.net)) {
        let kind = input
            .vias
            .get(via.kind as usize)
            .ok_or_else(|| invalid("extraction via kind"))?;
        for shape in &kind.shapes {
            include(shape.layer, crate::routing::shifted(shape.rect, via.at)?)?;
        }
    }
    for patch in input
        .routes
        .patches
        .iter()
        .filter(|p| input.extracts(p.net))
    {
        include(patch.layer, patch.rect)?;
    }
    for shape in input.passive {
        include(shape.layer, shape.rect)?;
    }
    Ok(bounds)
}

fn query_extent_fits(bounds: Rect, bin: i64, window: i64, cap: usize) -> bool {
    let margin = i128::from(window) + 1;
    let span = |lo: i64, hi: i64| {
        (i128::from(hi) + margin).div_euclid(i128::from(bin))
            - (i128::from(lo) - margin).div_euclid(i128::from(bin))
            + 1
    };
    span(bounds.lx, bounds.ux)
        .checked_mul(span(bounds.ly, bounds.uy))
        .is_some_and(|n| n <= cap as i128)
}

/// Allowance of the spatial structures that are never alive together: the
/// same-net lists during metal construction, then the retained-shape grid,
/// with any unused remainder lending room to parallel strip scratch.
fn index_allowance(limits: &ExtractLimits) -> u64 {
    (limits.max_bin_entries as u64).saturating_mul(24)
}

fn memory_estimate(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<ExtractMemory> {
    let live = crate::memory::live_bytes().unwrap_or(input.held_bytes);
    let held = live.max(input.held_bytes);
    let held = held.saturating_add(held.div_ceil(4));
    let bin = extraction_bin(input)?;
    let bounds = conductor_bounds(input, limits)?;
    let populated = bin_count(&bounds, bin, limits.max_bin_entries);
    let working = (limits.max_nodes as u64)
        .saturating_mul(64)
        .saturating_add((limits.max_elements as u64).saturating_mul(16))
        .saturating_add((limits.max_shapes as u64).saturating_mul(320))
        .saturating_add((limits.max_couplings as u64).saturating_mul(128))
        .saturating_add(index_allowance(limits))
        .saturating_add(populated.saturating_mul(96))
        .saturating_add(
            (input
                .routes
                .nets
                .iter()
                .filter(|&&n| input.extracts(n))
                .count() as u64)
                .saturating_mul(192),
        )
        .saturating_add(limits.candidate_scratch_bytes() as u64);
    let mut geometry =
        InstanceGeometry::estimated_bytes(input.instances, bin, input.layers.len(), limits)?;
    let mut selected_bin = bin;
    let refined_bin = instance_query_bin(input)?;
    let mut world = bounds.into_iter().flatten().reduce(|a, b| Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    });
    if let Some(source) = input.instances {
        for i in 0..source.geometry.len() {
            if let Some(b) = source.geometry.bounds(i)? {
                world = Some(world.map_or(b, |a| Rect {
                    lx: a.lx.min(b.lx),
                    ly: a.ly.min(b.ly),
                    ux: a.ux.max(b.ux),
                    uy: a.uy.max(b.uy),
                }));
            }
        }
    }
    // The world bound covers every future strip query, including +/-1 DBU
    // geometry margins and the largest lateral window. Tiny calibration
    // windows must never turn a previously admissible macro into a huge index.
    if refined_bin < bin
        && world.is_none_or(|b| {
            query_extent_fits(b, refined_bin, bin / 2, limits.max_instance_bin_entries)
        })
        && let Ok(refined) = InstanceGeometry::estimated_bytes(
            input.instances,
            refined_bin,
            input.layers.len(),
            limits,
        )
        && held
            .saturating_add(working)
            .saturating_add(geometry.max(refined))
            <= limits.memory_mib.saturating_mul(1 << 20)
    {
        geometry = geometry.max(refined);
        selected_bin = refined_bin;
    }
    let memory = ExtractMemory {
        held_bytes: held,
        working_bytes: working,
        geometry_bytes: geometry,
        estimate_bytes: held.saturating_add(working).saturating_add(geometry),
        limit_bytes: limits.memory_mib.saturating_mul(1 << 20),
        instance_query_bin_dbu: selected_bin,
    };
    if memory.estimate_bytes > memory.limit_bytes {
        return Err(budget(&format!(
            "extraction estimate {} MiB exceeds memory_mib {}",
            memory.estimate_bytes.div_ceil(1 << 20),
            limits.memory_mib
        )));
    }
    Ok(memory)
}

fn guard_memory(limits: &ExtractLimits) -> Result<()> {
    if crate::memory::peak_rss_bytes()
        .is_some_and(|n| n > limits.memory_mib.saturating_mul(1 << 20))
    {
        return Err(budget("extraction peak resident set exceeds memory_mib"));
    }
    Ok(())
}

fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
fn invalid(s: &str) -> Error {
    Error::Invalid(format!("RC_EXTRACT: {s}"))
}

type Key = (u32, i64, i64);

/// Private phase-timing instrumentation: fixed state, no contribution records.
/// Enabled only by RUSTROAD_EXTRACT_PROFILE (stderr); never changes results.
struct PhaseProfile {
    enabled: bool,
    start: std::time::Instant,
    mark: std::time::Instant,
    ns: [u128; STAGES.len()],
}
/// Profile stage names, in order.
const STAGES: [&str; 8] = [
    "preflight_and_trees",
    "shapes_and_instance_index",
    "metal_construction",
    "grid",
    "passive_strips",
    "virtual_crossings",
    "strips",
    "finalization",
];
impl PhaseProfile {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            enabled: std::env::var_os("RUSTROAD_EXTRACT_PROFILE").is_some(),
            start: now,
            mark: now,
            ns: [0; STAGES.len()],
        }
    }
    fn phase(&mut self, at: usize) {
        if self.enabled {
            let now = std::time::Instant::now();
            self.ns[at] = self.ns[at].saturating_add(now.duration_since(self.mark).as_nanos());
            self.mark = now;
        }
    }
    fn report(
        &self,
        stats: &ExtractStats,
        strips: &parallel::Report,
        grid: (usize, u64),
        net_bins: (u32, usize),
    ) {
        if self.enabled {
            eprintln!(
                "{}",
                serde_json::json!({
                    "schema": "rustroad.extract-phase-profile.v3",
                    "scope": "extraction core; strips computed on the --threads pool, accumulations committed in run order per sharded accumulator",
                    "elapsed_ns": self.start.elapsed().as_nanos(),
                    "stage_ns": STAGES
                        .iter()
                        .zip(self.ns)
                        .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
                        .collect::<serde_json::Map<_, _>>(),
                    "strip_phases_ns": {
                        "compute": strips.compute_ns,
                        "ordered_commit": strips.commit_ns,
                    },
                    "lanes": strips.lanes,
                    "shards": strips.shards,
                    "barriers": strips.barriers,
                    "chunks": strips.chunks,
                    "units": strips.units,
                    "serial_units": strips.units.min(strips.serial_units),
                    "committed_ops": strips.ops,
                    "grid_entries": grid.0,
                    "grid_bytes": grid.1,
                    "same_net_bucketed_lists": net_bins.0,
                    "same_net_bucket_entries": net_bins.1,
                    "peak_rss_bytes": crate::memory::peak_rss_bytes(),
                    "intervals": stats.intervals,
                    "passive_intervals": stats.passive_intervals,
                    "work": stats.work,
                    "max_run_candidates": stats.max_run_candidates,
                    "floating_point_order": "unchanged individual additions in run order",
                })
            );
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum EdgeKind {
    Wire(u32),
    Via(u32),
}
/// One capacitance-bearing strip. A routed wire run (its body) has its two
/// long sides at `cross -/+ width/2` and spans its centerline `lo..hi`. Metal
/// beyond the body is cut into rectangle pieces (the half-width end
/// extensions, via landing pads and routing patches, minus same-net metal);
/// each piece contributes two strips: one along x (bottom and top edges) and
/// one along y (left and right edges), so all four piece edges are sides; both
/// strips share one shape. Sides inside or touching same-net metal are not
/// exposed and carry no capacitance.
#[derive(Clone, Copy, Debug)]
struct Run {
    net: u32,
    layer: u32,
    vertical: bool,
    lo: i64,
    hi: i64,
    /// Side edge coordinates across the axis (`minus < plus`).
    minus: i64,
    plus: i64,
    /// Index of this strip's conductor shape (excluded from its own query).
    shape: u32,
    /// Strip of a rectangle piece (end extension, via pad or routing patch).
    piece: bool,
    /// Strip of a passive conductor (pin, obstruction or supply metal): it
    /// counts only its own cross-section toward routed conductors that are
    /// its plate (the second cut of a crossing); `net` is the pin's net or
    /// `u32::MAX` for ground.
    passive: Option<PassiveOwner>,
    /// Local (tree-order) nodes at `lo` and `hi` (equal for a piece).
    node_lo: u32,
    node_hi: u32,
}
impl Run {
    /// Centre across the axis. Exact for every strip: a wire body has
    /// `minus = cross - w/2` and `plus = cross + w/2` (integer halves), and
    /// pieces and passive strips define their centre this way.
    #[inline]
    fn cross(&self) -> i64 {
        self.minus + (self.plus - self.minus) / 2
    }
}
/// Owner of a conductor shape. Every routed-net shape (wire body or piece)
/// models its own exposed sides.
#[derive(Clone, Copy, Debug)]
enum Owner {
    /// Wire body `Built::runs[i]`.
    Run(u32),
    /// Rectangle piece `Built::pieces[p]` (end extension, via pad or patch).
    Piece(u32),
    Passive(PassiveOwner),
}
/// A rectangle piece of routed metal: its net and the one RC node all of
/// its capacitance attaches to. Its rectangle and layer are its shape; its
/// x and y strips are generated when its unit is computed.
#[derive(Clone, Copy, Debug)]
struct PieceInfo {
    net: u32,
    node: u32,
}
/// Via landing-pad metal on a routing layer, at the via's node in the tree.
#[derive(Clone, Copy, Debug)]
struct Pad {
    net: u32,
    layer: u32,
    rect: Rect,
    node: u32,
}
#[derive(Clone, Copy, Debug)]
struct Shape {
    layer: u32,
    rect: Rect,
    owner: Owner,
}
impl Shape {
    /// Signal net of a passive pin shape (runs are resolved through `runs`).
    fn net(&self) -> Option<u32> {
        match self.owner {
            Owner::Run(_) | Owner::Piece(_) => None,
            Owner::Passive(PassiveOwner::Pin { net, .. }) => Some(net),
            Owner::Passive(PassiveOwner::Ground) => None,
        }
    }
    /// Routed metal (a wire body or a piece).
    fn routed(&self) -> bool {
        matches!(self.owner, Owner::Run(_) | Owner::Piece(_))
    }
}
fn shape_net(built: &Built, s: &Shape) -> Option<u32> {
    match s.owner {
        Owner::Run(o) => Some(built.runs[o as usize].net),
        Owner::Piece(p) => Some(built.pieces[p as usize].net),
        _ => s.net(),
    }
}
/// Body rectangle of a wire run: its centerline extent times the width.
fn body_rect(r: &Run) -> Rect {
    if r.vertical {
        Rect {
            lx: r.minus,
            ux: r.plus,
            ly: r.lo,
            uy: r.hi,
        }
    } else {
        Rect {
            lx: r.lo,
            ux: r.hi,
            ly: r.minus,
            uy: r.plus,
        }
    }
}

struct Built {
    nets: Vec<RcNet>,
    /// Wire bodies, in tree order per net.
    runs: Vec<Run>,
    /// Rectangle pieces in creation order; piece `p` is shape
    /// `piece_shape0 + p`.
    pieces: Vec<PieceInfo>,
    piece_shape0: u32,
    /// Retained passive conductors crossed by routed metal, by shape ID in
    /// shape order (each is an x/y strip pair).
    passive_units: Vec<u32>,
    pads: Vec<Pad>,
    /// Pin index -> (net, local node).
    pin_nodes: HashMap<u32, (u32, u32), BuildFx>,
}
impl Built {
    /// Strip records of the original layout: one per body, two per piece
    /// and per crossed retained passive conductor.
    fn strips(&self) -> usize {
        self.runs.len() + 2 * (self.pieces.len() + self.passive_units.len())
    }
}

/// Global RC node IDs: nets in ascending net ID, then local node order, so
/// global ID order equals `(net, node)` order. IDs stay below `u32::MAX`.
struct NodeIds {
    /// By local net index: global ID of its node 0.
    base: Vec<u32>,
    /// By net ID (dense IDs only): the base of `net_index[net]`.
    by_net: Vec<u32>,
    /// By global ID: its local net index.
    net_of: Vec<u32>,
    total: usize,
}
impl NodeIds {
    fn new(nets: &[RcNet], net_index: &HashMap<u32, usize, BuildFx>) -> Result<Self> {
        let mut order: Vec<u32> = (0..nets.len() as u32).collect();
        order.sort_by_key(|&l| (nets[l as usize].net, l));
        let mut base = vec![0u32; nets.len()];
        let mut net_of = Vec::new();
        for l in order {
            let b = u32::try_from(net_of.len())
                .ok()
                .filter(|&b| b < u32::MAX - 1)
                .ok_or_else(|| budget("extraction RC node IDs"))?;
            base[l as usize] = b;
            net_of.resize(net_of.len() + nets[l as usize].nodes.len(), l);
        }
        let total = net_of.len();
        if total >= u32::MAX as usize - 1 {
            return Err(budget("extraction RC node IDs"));
        }
        let top = nets.iter().map(|n| n.net as usize + 1).max().unwrap_or(0);
        let by_net = if top <= 4 * nets.len() + 65_536 {
            let mut v = vec![u32::MAX; top];
            for (&net, &l) in net_index {
                v[net as usize] = base[l];
            }
            v
        } else {
            Vec::new()
        };
        Ok(Self {
            base,
            by_net,
            net_of,
            total,
        })
    }
    /// Global ID of `node` of local net `local`.
    #[inline]
    fn local(&self, local: usize, node: u32) -> u32 {
        self.base[local] + node
    }
    /// Global ID of `(net, node)`; the net is resolved like `net_index`.
    #[inline]
    fn of(&self, net_index: &HashMap<u32, usize, BuildFx>, net: u32, node: u32) -> u32 {
        match self.by_net.get(net as usize) {
            Some(&b) if b != u32::MAX => b + node,
            _ => self.base[net_index[&net]] + node,
        }
    }
    /// `(local net index, node)` of a global ID.
    #[inline]
    fn decode(&self, g: u32) -> (usize, u32) {
        let l = self.net_of[g as usize] as usize;
        (l, g - self.base[l])
    }
}

fn layer_model<'a>(input: &ExtractInput<'a>, layer: u32) -> Result<&'a RcLayer> {
    input
        .tech
        .layers
        .get(layer as usize)
        .and_then(|l| l.as_ref())
        .ok_or_else(|| invalid("routing layer has no RC profile entry"))
}

fn preflight(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<()> {
    limits.validate()?;
    let tech = input.tech;
    if !(tech.dbu_per_um.is_finite() && tech.dbu_per_um > 0.0)
        || tech.layers.len() != input.layers.len()
        || tech.via_ohm.len() != input.vias.len()
        || tech.via_ohm.iter().any(|r| !r.is_finite() || *r < 0.0)
    {
        return Err(invalid("RC technology does not match the routing input"));
    }
    for (i, l) in input.layers.iter().enumerate() {
        match (&tech.layers[i], l.vertical) {
            (Some(m), Some(_)) => {
                if m.width != l.width {
                    return Err(invalid(&format!(
                        "layer {} routed width {} DBU differs from calibrated width {} DBU; \
                         non-default widths are unsupported",
                        l.name, l.width, m.width
                    )));
                }
                if !(m.sheet_ohm.is_finite() && m.sheet_ohm > 0.0) || m.contexts.is_empty() {
                    return Err(invalid("invalid layer resistance or empty calibration"));
                }
                if m.area_cap.is_some_and(|a| !(a.is_finite() && a >= 0.0))
                    || m.overlap
                        .iter()
                        .any(|o| !(o.1.is_finite() && o.1 >= 0.0) || o.0 >= m.metal)
                {
                    return Err(invalid("invalid overlap-area capacitance"));
                }
                if !m.contexts.iter().any(|c| c.over == 0 && c.under.is_none()) {
                    return Err(invalid("layer lacks the substrate calibration context"));
                }
                for c in &m.contexts {
                    if c.rows.is_empty()
                        || c.rows
                            .iter()
                            .any(|r| r.iter().any(|v| !v.is_finite() || *v < 0.0))
                        || c.rows.windows(2).any(|w| w[1][0] <= w[0][0])
                    {
                        return Err(invalid("invalid calibration rows"));
                    }
                }
            }
            (None, None) => {}
            (Some(_), None) => return Err(invalid("cut layer carries a wire model")),
            (None, Some(_)) => {
                // Only an error if a wire actually uses it; checked per wire.
            }
        }
    }
    if input.passive.len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    Ok(())
}

fn um(tech: &RcTech, dbu: i64) -> f64 {
    dbu as f64 / tech.dbu_per_um
}

/// Where one net's tree computation stopped, relative to the cumulative
/// budget checks of the serial loop (replayed in net order at merge).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum NetStage {
    /// Before the graph-node budget.
    #[default]
    Start,
    /// After the graph-node budget, before the wire-length sum.
    Graph,
    /// Inside the wire-length sum (`NetProbe::wire` is its prefix).
    Wire,
    /// After the wire-length sum, before the RC-node budget.
    Tree,
    /// After the RC-node budget.
    Tail,
}
#[derive(Default)]
struct NetProbe {
    stage: NetStage,
    keys: usize,
    wire: u64,
    nodes: usize,
}
/// Per-net statistics, added in net order at merge.
#[derive(Default)]
struct NetCounts {
    duplicate_wire_edges: usize,
    segment_cut_nodes: usize,
    coincident_pin_links: usize,
    opened_loop_resistors: usize,
    resistors: usize,
    resistance: f64,
    lattice_wires: usize,
    vias: usize,
}
/// One net's tree: its RC net, wire bodies (shape IDs assigned at merge),
/// via pads and `(pin, local node)` pairs in node order.
struct NetTree {
    rc: RcNet,
    runs: Vec<Run>,
    pads: Vec<Pad>,
    pins: Vec<(u32, u32)>,
    counts: NetCounts,
}
/// Nets computed concurrently before each in-order merge.
const TREE_BATCH: usize = 2048;

/// Build per-net graphs, straight runs, the tree order and resistors.
/// Nets are independent: batches are computed in parallel and merged in net
/// order, replaying the serial loop's cumulative budget checks at the
/// checkpoint each net reached, so results and errors are those of the
/// serial loop.
fn build_trees(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<(Built, ExtractStats)> {
    use rayon::prelude::*;
    let routes = input.routes;
    let mut stats = ExtractStats::default();
    let terminals: HashMap<u32, &Terminal> = input
        .terminals
        .iter()
        .filter(|t| input.extracts(t.net))
        .map(|t| (t.pin, t))
        .collect();
    let mut wires = BTreeMap::<u32, Vec<usize>>::new();
    let mut vias = BTreeMap::<u32, Vec<usize>>::new();
    let mut access = BTreeMap::<u32, Vec<usize>>::new();
    for (i, w) in routes.wires.iter().enumerate() {
        if input.extracts(w.net) {
            wires.entry(w.net).or_default().push(i);
        }
    }
    for (i, v) in routes.vias.iter().enumerate() {
        if !input.extracts(v.net) {
            continue;
        }
        if v.kind as usize >= input.vias.len() {
            return Err(invalid("via kind outside routing input"));
        }
        vias.entry(v.net).or_default().push(i);
    }
    for (i, a) in routes.access.iter().enumerate() {
        if input.extracts(a.net) {
            access.entry(a.net).or_default().push(i);
        }
    }
    let route_nets: Vec<u32> = routes
        .nets
        .iter()
        .copied()
        .filter(|&n| input.extracts(n))
        .collect();
    let mut out = Built {
        nets: Vec::new(),
        runs: Vec::new(),
        pieces: Vec::new(),
        piece_shape0: 0,
        passive_units: Vec::new(),
        pads: Vec::new(),
        pin_nodes: HashMap::default(),
    };
    let maps = (&wires, &vias, &access);
    for (b, batch) in route_nets.chunks(TREE_BATCH).enumerate() {
        let trees: Vec<(Result<NetTree>, NetProbe)> = batch
            .par_iter()
            .map(|&net| {
                let mut probe = NetProbe::default();
                let tree = net_tree(input, limits, &terminals, maps, net, &mut probe);
                (tree, probe)
            })
            .collect();
        out.runs
            .reserve_exact(trees.iter().flat_map(|t| &t.0).map(|t| t.runs.len()).sum());
        for (k, (tree, probe)) in trees.into_iter().enumerate() {
            if (b * TREE_BATCH + k).is_multiple_of(4096) {
                guard_memory(limits)?;
            }
            if probe.stage == NetStage::Start {
                return Err(tree.err().unwrap_or_else(|| invalid("net tree stage")));
            }
            if stats.nodes + probe.keys > limits.max_nodes.saturating_mul(4) {
                return Err(budget("extraction graph nodes"));
            }
            if probe.stage == NetStage::Graph {
                return Err(tree.err().unwrap_or_else(|| invalid("net tree stage")));
            }
            stats.wire_length_dbu = stats
                .wire_length_dbu
                .checked_add(probe.wire)
                .ok_or_else(|| budget("wire length"))?;
            if matches!(probe.stage, NetStage::Wire | NetStage::Tree) {
                return Err(tree.err().unwrap_or_else(|| invalid("net tree stage")));
            }
            stats.nodes += probe.nodes;
            if stats.nodes > limits.max_nodes {
                return Err(budget("extraction RC nodes"));
            }
            let tree = tree?;
            let c = &tree.counts;
            stats.duplicate_wire_edges += c.duplicate_wire_edges;
            stats.segment_cut_nodes += c.segment_cut_nodes;
            stats.coincident_pin_links += c.coincident_pin_links;
            stats.opened_loop_resistors += c.opened_loop_resistors;
            for &(pin, node) in &tree.pins {
                out.pin_nodes.insert(pin, (tree.rc.net, node));
            }
            stats.resistors += c.resistors;
            stats.resistance_ohm += c.resistance;
            for mut run in tree.runs {
                run.shape = out.runs.len() as u32;
                out.runs.push(run);
            }
            out.pads.extend(tree.pads);
            stats.lattice_wires += c.lattice_wires;
            stats.vias += c.vias;
            out.nets.push(tree.rc);
        }
    }
    stats.nets = out.nets.len();
    stats.wire_runs = out.runs.len();
    Ok((out, stats))
}

type NetLists<'m> = (
    &'m BTreeMap<u32, Vec<usize>>,
    &'m BTreeMap<u32, Vec<usize>>,
    &'m BTreeMap<u32, Vec<usize>>,
);

/// One net's graph, straight runs, tree order, resistors, runs and pads.
/// `probe` records the serial loop's budget checkpoints this net reached.
fn net_tree(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    terminals: &HashMap<u32, &Terminal>,
    (wires, vias, access): NetLists<'_>,
    net: u32,
    probe: &mut NetProbe,
) -> Result<NetTree> {
    let routes = input.routes;
    let mut counts = NetCounts::default();
    let net_wires = wires.get(&net).map(Vec::as_slice).unwrap_or(&[]);
    let net_vias = vias.get(&net).map(Vec::as_slice).unwrap_or(&[]);
    let net_access = access
        .get(&net)
        .ok_or_else(|| invalid("routed net without pin access"))?;
    // Graph nodes in key order.
    let mut keys: Vec<Key> = Vec::with_capacity(2 * (net_wires.len() + net_vias.len()) + 1);
    for &i in net_wires {
        let w = &routes.wires[i];
        if w.start == w.end || (w.start[0] != w.end[0] && w.start[1] != w.end[1]) {
            return Err(invalid("non-Manhattan or zero-length wire"));
        }
        layer_model(input, w.layer)?;
        keys.push((w.layer, w.start[0], w.start[1]));
        keys.push((w.layer, w.end[0], w.end[1]));
    }
    for &i in net_vias {
        let v = &routes.vias[i];
        let spec = &input.vias[v.kind as usize];
        keys.push((spec.lower, v.at[0], v.at[1]));
        keys.push((spec.upper, v.at[0], v.at[1]));
    }
    for &i in net_access {
        let a = &routes.access[i];
        keys.push((a.layer, a.at[0], a.at[1]));
    }
    keys.sort_unstable();
    keys.dedup();
    // Checkpoint: the cumulative graph-node budget (checked at merge).
    probe.keys = keys.len();
    probe.stage = NetStage::Graph;
    let id = |k: &Key| keys.binary_search(k).map(|i| i as u32);
    let missing = |_| invalid("graph node lookup");
    // A wire record may be one lattice edge or a longer collinear
    // segment: split every record at each same-net node of its layer that
    // lies strictly inside it, so the graph does not depend on how the
    // router stored the metal.
    let mut by_row = BTreeMap::<(u32, i64), Vec<i64>>::new(); // (layer, y) -> x
    let mut by_col = BTreeMap::<(u32, i64), Vec<i64>>::new(); // (layer, x) -> y
    for k in &keys {
        by_row.entry((k.0, k.2)).or_default().push(k.1);
        by_col.entry((k.0, k.1)).or_default().push(k.2);
    }
    for v in by_row.values_mut() {
        v.sort_unstable();
    }
    let mut edges: Vec<(u32, u32, EdgeKind)> = Vec::new();
    for &i in net_wires {
        let w = &routes.wires[i];
        let horizontal = w.start[1] == w.end[1];
        let (fixed, a0, a1) = if horizontal {
            (
                w.start[1],
                w.start[0].min(w.end[0]),
                w.start[0].max(w.end[0]),
            )
        } else {
            (
                w.start[0],
                w.start[1].min(w.end[1]),
                w.start[1].max(w.end[1]),
            )
        };
        let line = if horizontal {
            &by_row[&(w.layer, fixed)]
        } else {
            &by_col[&(w.layer, fixed)]
        };
        let lo = line.partition_point(|&c| c < a0);
        let hi = line.partition_point(|&c| c <= a1);
        let at = |c: i64| -> Key {
            if horizontal {
                (w.layer, c, fixed)
            } else {
                (w.layer, fixed, c)
            }
        };
        for pair in line[lo..hi].windows(2) {
            let a = id(&at(pair[0])).map_err(missing)?;
            let b = id(&at(pair[1])).map_err(missing)?;
            edges.push((a.min(b), a.max(b), EdgeKind::Wire(w.layer)));
        }
        if edges.len() > limits.max_elements.saturating_mul(4) {
            return Err(budget("extraction wire edges"));
        }
    }
    // Overlapping records describe the same metal once.
    let before = edges.len();
    edges.sort_unstable_by_key(|e| (e.0, e.1));
    edges.dedup_by_key(|e| (e.0, e.1));
    counts.duplicate_wire_edges += before - edges.len();
    for &i in net_vias {
        let v = &routes.vias[i];
        let spec = &input.vias[v.kind as usize];
        let a = id(&(spec.lower, v.at[0], v.at[1])).map_err(missing)?;
        let b = id(&(spec.upper, v.at[0], v.at[1])).map_err(missing)?;
        edges.push((a.min(b), a.max(b), EdgeKind::Via(v.kind)));
    }
    let n = keys.len();
    let mut incident = vec![Vec::<u32>::new(); n];
    for (e, &(a, b, _)) in edges.iter().enumerate() {
        incident[a as usize].push(e as u32);
        incident[b as usize].push(e as u32);
    }
    // Pins: first pin (lowest index) names a node; others get links.
    let mut pins: Vec<(u32, u32)> = Vec::new();
    let mut driver = None;
    for &i in net_access {
        let a = &routes.access[i];
        let t = terminals
            .get(&a.pin)
            .ok_or_else(|| invalid("pin access without terminal"))?;
        if t.net != net {
            return Err(invalid("pin access net mismatch"));
        }
        pins.push((a.pin, id(&(a.layer, a.at[0], a.at[1])).map_err(missing)?));
        if t.driver {
            if driver.is_some() {
                return Err(invalid("routed net has more than one driver"));
            }
            driver = Some(a.pin);
        }
    }
    pins.sort_unstable();
    let driver = driver.ok_or_else(|| invalid("routed net without driver"))?;
    let mut pin_of = vec![None::<u32>; n];
    let mut extra: Vec<(u32, u32)> = Vec::new(); // (pin, graph node)
    for &(pin, node) in &pins {
        if pin_of[node as usize].is_none() {
            pin_of[node as usize] = Some(pin);
        } else {
            extra.push((pin, node));
        }
    }
    // Junctions: everything but interior points of straight same-layer runs.
    let point = |g: u32| keys[g as usize];
    let mut virtual_keys: Vec<Key> = Vec::new();
    let mut junction = vec![true; n];
    for g in 0..n {
        if pin_of[g].is_some() || incident[g].len() != 2 {
            continue;
        }
        let (e0, e1) = (
            edges[incident[g][0] as usize],
            edges[incident[g][1] as usize],
        );
        if let (EdgeKind::Wire(l0), EdgeKind::Wire(l1)) = (e0.2, e1.2) {
            let other = |e: (u32, u32, EdgeKind)| if e.0 as usize == g { e.1 } else { e.0 };
            let (p, a, b) = (point(g as u32), point(other(e0)), point(other(e1)));
            let collinear = (a.1 == p.1 && b.1 == p.1) || (a.2 == p.2 && b.2 == p.2);
            // Straight-through only: the two neighbours lie on opposite sides.
            let through = (a.1 - p.1).signum() * (b.1 - p.1).signum() <= 0
                && (a.2 - p.2).signum() * (b.2 - p.2).signum() <= 0;
            if l0 == l1 && collinear && through {
                junction[g] = false;
            }
        }
    }
    // Straight runs between junctions, split at lattice nodes by length.
    let mut used = vec![false; edges.len()];
    let mut run_ends: Vec<(u32, u32, u32)> = Vec::new(); // (graph a, graph b, layer)
    for e in 0..edges.len() {
        let EdgeKind::Wire(layer) = edges[e].2 else {
            continue;
        };
        if used[e] {
            continue;
        }
        // Walk to one end of the straight chain.
        let mut start = edges[e].0;
        let mut via_edge = e as u32;
        loop {
            if junction[start as usize] {
                break;
            }
            let next_edge = *incident[start as usize]
                .iter()
                .find(|&&x| x != via_edge)
                .ok_or_else(|| invalid("chain walk"))?;
            let (a, b, _) = edges[next_edge as usize];
            start = if a == start { b } else { a };
            via_edge = next_edge;
            if start == edges[e].0 {
                return Err(invalid("closed straight chain"));
            }
        }
        // Collect nodes from that end through the chain.
        let mut chain = vec![start];
        let mut current = start;
        let mut from_edge = u32::MAX;
        loop {
            let candidates: Vec<u32> = incident[current as usize]
                .iter()
                .copied()
                .filter(|&x| {
                    x != from_edge && matches!(edges[x as usize].2, EdgeKind::Wire(l) if l == layer)
                })
                .collect();
            let next_edge = if current == start {
                // Leave the start junction along the seed chain only.
                via_edge
            } else if junction[current as usize] {
                break;
            } else {
                *candidates.first().ok_or_else(|| invalid("chain step"))?
            };
            if used[next_edge as usize] {
                return Err(invalid("chain revisited"));
            }
            used[next_edge as usize] = true;
            let (a, b, _) = edges[next_edge as usize];
            current = if a == current { b } else { a };
            chain.push(current);
            from_edge = next_edge;
            if chain.len() > keys.len() + 1 {
                return Err(invalid("chain length"));
            }
            if junction[current as usize] {
                break;
            }
        }
        // Split a run longer than the segment limit into equal pieces at
        // exact coordinates (independent of where records start and end):
        // n = ceil(L / limit) pieces; a cut point that is not a stored node
        // becomes a virtual node.
        let (ka, kb) = (point(start), point(current));
        let horizontal = ka.2 == kb.2;
        let (p0, p1) = if horizontal {
            (ka.1, kb.1)
        } else {
            (ka.2, kb.2)
        };
        let length = (p1 - p0).abs();
        let pieces = if length > limits.max_segment_length {
            (length + limits.max_segment_length - 1) / limits.max_segment_length
        } else {
            1
        };
        let mut prev = start;
        for i in 1..pieces {
            let c = p0
                + (p1 - p0).signum()
                    * ((i128::from(length) * i128::from(i) / i128::from(pieces)) as i64);
            let key = if horizontal {
                (layer, c, ka.2)
            } else {
                (layer, ka.1, c)
            };
            let node = match keys.binary_search(&key) {
                Ok(g) => {
                    junction[g] = true;
                    g as u32
                }
                Err(_) => {
                    virtual_keys.push(key);
                    (n + virtual_keys.len() - 1) as u32
                }
            };
            run_ends.push((prev, node, layer));
            prev = node;
        }
        run_ends.push((prev, current, layer));
    }
    if used
        .iter()
        .zip(&edges)
        .any(|(u, e)| !u && matches!(e.2, EdgeKind::Wire(_)))
    {
        return Err(invalid("wire not covered by a straight run"));
    }
    // Electrical elements over junction and virtual nodes plus extra pin
    // nodes (graph nodes, then virtual cut nodes, then extra pins).
    let node_count = n + virtual_keys.len();
    counts.segment_cut_nodes += virtual_keys.len();
    let point = |g: u32| -> Key {
        if (g as usize) < n {
            keys[g as usize]
        } else {
            virtual_keys[g as usize - n]
        }
    };
    let mut elems: Vec<(u32, u32, f64)> = Vec::new();
    // Checkpoint: the cumulative wire length (its prefix is added at merge).
    probe.stage = NetStage::Wire;
    for &(a, b, layer) in &run_ends {
        let m = layer_model(input, layer)?;
        let (ka, kb) = (point(a), point(b));
        let length = (ka.1 - kb.1).abs() + (ka.2 - kb.2).abs();
        let ohm = m.sheet_ohm * um(input.tech, length) / um(input.tech, m.width);
        elems.push((a, b, ohm));
        probe.wire = probe
            .wire
            .checked_add(length as u64)
            .ok_or_else(|| budget("wire length"))?;
    }
    probe.stage = NetStage::Tree;
    for (a, b, kind) in edges.iter().copied() {
        if let EdgeKind::Via(k) = kind {
            elems.push((a, b, input.tech.via_ohm[k as usize]));
        }
    }
    let mut total_nodes = node_count as u32;
    let mut extra_pin = Vec::new();
    for &(pin, node) in &extra {
        let v = total_nodes;
        total_nodes += 1;
        extra_pin.push((v, pin));
        elems.push((node, v, 0.0));
        counts.coincident_pin_links += 1;
    }
    let total = total_nodes as usize;
    let mut adjacency = vec![Vec::<u32>::new(); total];
    for (i, e) in elems.iter().enumerate() {
        adjacency[e.0 as usize].push(i as u32);
        adjacency[e.1 as usize].push(i as u32);
    }
    let mut pin_of_full = pin_of.clone();
    pin_of_full.resize(total, None);
    for &(v, pin) in &extra_pin {
        pin_of_full[v as usize] = Some(pin);
    }
    let electrical: Vec<bool> = (0..total)
        .map(|g| g >= n || junction[g] || pin_of_full[g].is_some())
        .collect();
    let sort_key = |g: u32| -> (u8, Key) {
        if (g as usize) < node_count {
            (0, point(g))
        } else {
            (1, (g, 0, 0))
        }
    };
    let root = pins
        .iter()
        .find(|p| p.0 == driver)
        .map(|p| p.1)
        .ok_or_else(|| invalid("driver node"))?;
    // Breadth-first tree order from the driver, deterministic by node key.
    let mut local = vec![u32::MAX; total];
    let mut order: Vec<u32> = vec![root];
    local[root as usize] = 0;
    let mut elem_done = vec![false; elems.len()];
    let mut resistors = Vec::new();
    let mut head = 0;
    while head < order.len() {
        let u = order[head];
        head += 1;
        let mut next: Vec<(u8, Key, u32)> = adjacency[u as usize]
            .iter()
            .map(|&e| {
                let (a, b, _) = elems[e as usize];
                let other = if a == u { b } else { a };
                let k = sort_key(other);
                (k.0, k.1, e)
            })
            .collect();
        next.sort_unstable();
        for (_, _, e) in next {
            if elem_done[e as usize] {
                continue;
            }
            elem_done[e as usize] = true;
            let (a, b, ohm) = elems[e as usize];
            let other = if a == u { b } else { a };
            if local[other as usize] != u32::MAX {
                counts.opened_loop_resistors += 1;
                continue;
            }
            local[other as usize] = order.len() as u32;
            order.push(other);
            resistors.push((local[u as usize], local[other as usize], ohm));
        }
    }
    if (0..total).any(|g| electrical[g] && local[g] == u32::MAX) {
        return Err(invalid(&format!("net {net} RC graph is disconnected")));
    }
    let mut counter = 0u32;
    let nodes: Vec<RcNode> = order
        .iter()
        .map(|&g| match pin_of_full[g as usize] {
            Some(pin) => RcNode::Pin(pin),
            None => {
                counter += 1;
                RcNode::Internal(counter)
            }
        })
        .collect();
    let pins: Vec<(u32, u32)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(i, node)| match node {
            RcNode::Pin(pin) => Some((*pin, i as u32)),
            RcNode::Internal(_) => None,
        })
        .collect();
    // Checkpoint: the cumulative RC-node budget (checked at merge).
    probe.nodes = nodes.len();
    probe.stage = NetStage::Tail;
    counts.resistors = resistors.len();
    counts.resistance = resistors.iter().map(|r| r.2).sum::<f64>();
    let mut runs = Vec::with_capacity(run_ends.len());
    let mut pads = Vec::new();
    // Straight runs with tree-order end nodes.
    for &(a, b, layer) in &run_ends {
        let (ka, kb) = (point(a), point(b));
        let vertical = ka.1 == kb.1;
        let (pa, pb) = if vertical { (ka.2, kb.2) } else { (ka.1, kb.1) };
        let (lo_node, hi_node) = if pa <= pb { (a, b) } else { (b, a) };
        let cross = if vertical { ka.1 } else { ka.2 };
        let half = input.layers[layer as usize].width / 2;
        runs.push(Run {
            net,
            layer,
            vertical,
            lo: pa.min(pb),
            hi: pa.max(pb),
            minus: cross - half,
            plus: cross + half,
            // Assigned at merge (the body's index).
            shape: 0,
            piece: false,
            passive: None,
            node_lo: local[lo_node as usize],
            node_hi: local[hi_node as usize],
        });
    }
    // Via landing-pad metal of this net, at the via's node on each layer.
    for &i in net_vias {
        let v = &routes.vias[i];
        let spec = &input.vias[v.kind as usize];
        for s in &spec.shapes {
            if input.layers[s.layer as usize].vertical.is_none() {
                continue;
            }
            let g = id(&(s.layer, v.at[0], v.at[1])).map_err(missing)?;
            pads.push(Pad {
                net,
                layer: s.layer,
                rect: crate::routing::shifted(s.rect, v.at)?,
                node: local[g as usize],
            });
        }
    }
    counts.lattice_wires = net_wires.len();
    counts.vias = net_vias.len();
    Ok(NetTree {
        rc: RcNet {
            net,
            ground: vec![0.0; nodes.len()],
            nodes,
            resistors,
        },
        runs,
        pads,
        pins,
        counts,
    })
}

/// Same-net lists at least this long are also bucketed by construction bin,
/// so a lookup scans the nearby entries instead of the whole list.
const NET_BIN_LIST: usize = 256;
/// Admitted bytes per bucket entry: the entry with vector growth slack plus
/// at most one cell header with hash-table growth slack.
const NET_BIN_ENTRY_BYTES: u64 = 128;

/// One `(net, layer)` same-net list: shape IDs in insertion order.
#[derive(Default)]
struct NetList {
    ids: Vec<u32>,
    /// Bucket key of the list, once bucketed.
    key: u32,
    /// Every ID of the list is in the buckets.
    binned: bool,
    /// The buckets ran out of admitted entries while this list was added;
    /// it is scanned linearly from then on.
    unbinnable: bool,
}

/// Construction-bin buckets of the long same-net lists: per `(list, cell)`
/// a chain of shape IDs in insertion order. A shape is in every cell its
/// closed rectangle touches.
struct NetBins {
    cell: i64,
    heads: HashMap<(u32, i64, i64), [u32; 3], BuildFx>,
    /// `(id, next)` chain links.
    entries: Vec<[u32; 2]>,
    /// Admitted entries (from the configured allowance only, so whether a
    /// list is bucketed never depends on live memory).
    capacity: usize,
    lists: u32,
}
impl NetBins {
    const NONE: u32 = u32::MAX;
    fn new(cell: i64, capacity: usize) -> Self {
        Self {
            cell: cell.max(1),
            heads: HashMap::default(),
            entries: Vec::new(),
            capacity: capacity.min(Self::NONE as usize),
            lists: 0,
        }
    }
    #[inline]
    fn range(&self, lo: i64, hi: i64) -> (i64, i64) {
        (lo.div_euclid(self.cell), hi.div_euclid(self.cell))
    }
    /// Closed cell span of `r`, as an entry count (saturating).
    fn span(&self, r: Rect) -> u128 {
        let (x0, x1) = self.range(r.lx, r.ux);
        let (y0, y1) = self.range(r.ly, r.uy);
        ((x1 as i128 - x0 as i128 + 1) as u128)
            .saturating_mul((y1 as i128 - y0 as i128 + 1) as u128)
    }
    /// Adds `id` (rectangle `r`) to list `key`; `false` when the admitted
    /// entries are exhausted (nothing is added then). Inserted entries are
    /// charged to `work`.
    fn insert(
        &mut self,
        key: u32,
        id: u32,
        r: Rect,
        work: &mut usize,
        limits: &ExtractLimits,
    ) -> Result<bool> {
        let n = self.span(r);
        if n > (self.capacity - self.entries.len()) as u128 {
            return Ok(false);
        }
        let n = n as usize;
        *work = work
            .checked_add(n)
            .filter(|&w| w <= limits.max_work)
            .ok_or_else(|| budget("extraction work"))?;
        self.entries
            .try_reserve(n)
            .map_err(|_| budget("extraction same-net index allocation"))?;
        self.heads
            .try_reserve(n)
            .map_err(|_| budget("extraction same-net index allocation"))?;
        let (x0, x1) = self.range(r.lx, r.ux);
        let (y0, y1) = self.range(r.ly, r.uy);
        for x in x0..=x1 {
            for y in y0..=y1 {
                let at = self.entries.len() as u32;
                let head = self
                    .heads
                    .entry((key, x, y))
                    .or_insert([Self::NONE, Self::NONE, 0]);
                if head[1] == Self::NONE {
                    head[0] = at;
                } else {
                    self.entries[head[1] as usize][1] = at;
                }
                head[1] = at;
                head[2] += 1;
                self.entries.push([id, Self::NONE]);
            }
        }
        Ok(true)
    }
    /// Every ID of list `key` whose rectangle touches `r` (closed), each
    /// once, appended to `found` (unordered). Each scanned cell's entries
    /// are charged to `work` before the cell is scanned.
    fn touching(
        &self,
        key: u32,
        r: Rect,
        rect_of: impl Fn(u32) -> Rect,
        found: &mut Vec<u32>,
        work: &mut usize,
        limits: &ExtractLimits,
    ) -> Result<()> {
        let (qx0, qx1) = self.range(r.lx, r.ux);
        let (qy0, qy1) = self.range(r.ly, r.uy);
        for x in qx0..=qx1 {
            for y in qy0..=qy1 {
                let [mut at, _, count] =
                    self.heads
                        .get(&(key, x, y))
                        .copied()
                        .unwrap_or([Self::NONE, Self::NONE, 0]);
                *work = work
                    .checked_add((count as usize).max(1))
                    .filter(|&w| w <= limits.max_work)
                    .ok_or_else(|| budget("extraction work"))?;
                while at != Self::NONE {
                    let [id, next] = self.entries[at as usize];
                    at = next;
                    let q = rect_of(id);
                    // Reported from the first query cell it occupies.
                    if q.lx <= r.ux
                        && r.lx <= q.ux
                        && q.ly <= r.uy
                        && r.ly <= q.uy
                        && x == q.lx.div_euclid(self.cell).max(qx0)
                        && y == q.ly.div_euclid(self.cell).max(qy0)
                    {
                        found.push(id);
                        if found.len() > limits.max_candidates {
                            return Err(budget("extraction candidates per metal piece"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Same-net conductors during metal construction: retained shape IDs and
/// virtual signal-pin IDs per `(net, layer)`, in insertion order. Pieces are
/// appended as they are added. Lists of `NET_BIN_LIST` or more IDs are
/// bucketed when first looked up, within the admitted bucket entries.
/// Released before the retained-shape grid.
struct NetShapes {
    lists: HashMap<(u32, u32), NetList, BuildFx>,
    bins: NetBins,
    /// Lists this long are bucketed (`usize::MAX` disables buckets: tests).
    long: usize,
}
impl NetShapes {
    /// Conservative bytes: IDs with vector growth headroom plus map slots.
    fn bound(retained: usize, virtual_pins: usize, keys: usize) -> u64 {
        ((retained as u64).saturating_add(virtual_pins as u64))
            .saturating_mul(8)
            .saturating_add((keys as u64).saturating_mul(96))
    }
    /// The lists' bound must fit `allowance` (index allowance plus memory
    /// headroom). Buckets get what the bound leaves of the configured index
    /// allowance alone, capped by `max_bin_entries`.
    fn new(
        shapes: &Shapes<'_>,
        built: &Built,
        limits: &ExtractLimits,
        allowance: u64,
        cell: i64,
        long: usize,
    ) -> Result<Self> {
        let mut lists: HashMap<(u32, u32), NetList, BuildFx> = HashMap::default();
        let mut virtual_pins = 0usize;
        for (id, s) in shapes.owned() {
            if let Some(net) = shape_net(built, s) {
                lists.entry((net, s.layer)).or_default().ids.push(id);
            }
        }
        for (id, s) in shapes.virtual_shapes() {
            if let Some(net) = s.net() {
                virtual_pins += 1;
                lists.entry((net, s.layer)).or_default().ids.push(id);
            }
        }
        // Later pieces grow the lists up to the retained-shape budget.
        let bound = Self::bound(limits.max_shapes, virtual_pins, lists.len());
        if bound > allowance {
            return Err(budget("extraction same-net index memory"));
        }
        let room = index_allowance(limits).saturating_sub(bound) / NET_BIN_ENTRY_BYTES;
        let capacity = usize::try_from(room)
            .unwrap_or(usize::MAX)
            .min(limits.max_bin_entries);
        Ok(Self {
            lists,
            bins: NetBins::new(cell, capacity),
            long,
        })
    }
    fn push(
        &mut self,
        net: u32,
        layer: u32,
        id: u32,
        rect: Rect,
        limits: &ExtractLimits,
        stats: &mut ExtractStats,
    ) -> Result<()> {
        let list = self.lists.entry((net, layer)).or_default();
        list.ids.push(id);
        if list.binned
            && !self
                .bins
                .insert(list.key, id, rect, &mut stats.work, limits)?
        {
            list.binned = false;
            list.unbinnable = true;
        }
        Ok(())
    }
}

/// `a` minus `b` as disjoint rectangles (left, right, bottom, top slabs).
fn subtract(a: Rect, b: Rect) -> Vec<Rect> {
    if !(a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy) {
        return vec![a];
    }
    let mut out = Vec::with_capacity(4);
    let (x0, x1) = (a.lx.max(b.lx), a.ux.min(b.ux));
    if a.lx < x0 {
        out.push(Rect { ux: x0, ..a });
    }
    if x1 < a.ux {
        out.push(Rect { lx: x1, ..a });
    }
    if a.ly < b.ly {
        out.push(Rect {
            lx: x0,
            ux: x1,
            uy: b.ly,
            ..a
        });
    }
    if b.uy < a.uy {
        out.push(Rect {
            lx: x0,
            ux: x1,
            ly: b.uy,
            ..a
        });
    }
    out
}

/// Pieces of one rectangle (patch, end extension or via pad) that may exist
/// after subtracting the same-net metal it overlaps.
const MAX_PATCH_PIECES: usize = 256;

/// Same-net conductor shapes on `layer` that overlap or touch `r` (closed),
/// in shape-index order. A long list is bucketed on its first lookup; a
/// lookup scans the buckets `r` touches when they are fewer than the list's
/// IDs, otherwise the list. Scanned entries are charged to the work budget.
fn same_net_near(
    r: Rect,
    net: u32,
    layer: u32,
    near: &mut NetShapes,
    shapes: &Shapes<'_>,
    limits: &ExtractLimits,
    stats: &mut ExtractStats,
) -> Result<Vec<u32>> {
    let charge = |stats: &mut ExtractStats, n: usize| -> Result<()> {
        stats.work = stats
            .work
            .checked_add(n.max(1))
            .filter(|&n| n <= limits.max_work)
            .ok_or_else(|| budget("extraction work"))?;
        Ok(())
    };
    let Some(list) = near.lists.get_mut(&(net, layer)) else {
        charge(stats, 0)?;
        return Ok(Vec::new());
    };
    if list.ids.len() >= near.long && !list.binned && !list.unbinnable {
        // Bucket every ID so far, in list order; on exhaustion the entries
        // already added stay unused.
        let key = near.bins.lists;
        near.bins.lists = key
            .checked_add(1)
            .ok_or_else(|| budget("extraction same-net index lists"))?;
        list.key = key;
        list.binned = true;
        for &id in &list.ids {
            if !near
                .bins
                .insert(key, id, shapes.get(id).rect, &mut stats.work, limits)?
            {
                list.binned = false;
                list.unbinnable = true;
                break;
            }
        }
    }
    let mut found = Vec::new();
    if list.binned && near.bins.span(r) < list.ids.len() as u128 {
        near.bins.touching(
            list.key,
            r,
            |id| shapes.get(id).rect,
            &mut found,
            &mut stats.work,
            limits,
        )?;
    } else {
        charge(stats, list.ids.len())?;
        for &id in &list.ids {
            let q = shapes.get(id).rect;
            if q.lx <= r.ux && r.lx <= q.ux && q.ly <= r.uy && r.ly <= q.uy {
                found.push(id);
                if found.len() > limits.max_candidates {
                    return Err(budget("extraction candidates per metal piece"));
                }
            }
        }
    }
    // IDs are unique per list; sorting reproduces the shape-index order.
    found.sort_unstable();
    Ok(found)
}

/// `r` minus every listed shape, as disjoint rectangles (exact integers).
fn uncovered(r: Rect, covering: &[u32], shapes: &Shapes<'_>) -> Result<Vec<Rect>> {
    let mut pieces = vec![r];
    for &s in covering {
        let q = shapes.get(s).rect;
        pieces = pieces.into_iter().flat_map(|p| subtract(p, q)).collect();
        if pieces.len() > MAX_PATCH_PIECES {
            return Err(budget("metal pieces per rectangle"));
        }
    }
    Ok(pieces)
}

/// Add one rectangle piece as a conductor shape, all of its capacitance at
/// `node`; its two side strips (along x and along y) are generated from the
/// shape when its unit is computed. Pieces add capacitance only: never a
/// node or a resistor.
#[allow(clippy::too_many_arguments)]
fn add_piece(
    piece: Rect,
    net: u32,
    layer: u32,
    node: u32,
    built: &mut Built,
    shapes: &mut Shapes<'_>,
    near: &mut NetShapes,
    limits: &ExtractLimits,
    stats: &mut ExtractStats,
) -> Result<()> {
    let shape = shapes.len() as u32;
    let index = built.pieces.len() as u32;
    if index == 0 {
        built.piece_shape0 = shape;
    } else if shape != built.piece_shape0 + index {
        return Err(invalid("piece shapes are not contiguous"));
    }
    built.pieces.push(PieceInfo { net, node });
    shapes.push(Shape {
        layer,
        rect: piece,
        owner: Owner::Piece(index),
    });
    if shapes.retained_len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    near.push(net, layer, shape, piece, limits, stats)
}

/// Metal of routed nets beyond the wire bodies, as capacitance-only pieces:
/// first each wire run's half-width end extensions (DEF default extension, at
/// the run's end node), then each via landing pad (LEF via geometry, at the
/// via node). Each rectangle minus the same-net metal already present on its
/// layer (wire bodies, pins, earlier pieces) is added; nothing is counted
/// twice, and the union of bodies and pieces is exactly the routed metal.
#[allow(clippy::too_many_arguments)]
fn add_metal_pieces(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    built: &mut Built,
    shapes: &mut Shapes<'_>,
    near: &mut NetShapes,
    stats: &mut ExtractStats,
) -> Result<()> {
    // (net, layer, rect, node, is_pad) in a fixed order.
    let mut candidates: Vec<(u32, u32, Rect, u32, bool)> = Vec::new();
    for r in &built.runs {
        let e = input.layers[r.layer as usize].width / 2;
        if e <= 0 {
            continue;
        }
        let ends = if r.vertical {
            [
                Rect {
                    lx: r.minus,
                    ux: r.plus,
                    ly: r.lo - e,
                    uy: r.lo,
                },
                Rect {
                    lx: r.minus,
                    ux: r.plus,
                    ly: r.hi,
                    uy: r.hi + e,
                },
            ]
        } else {
            [
                Rect {
                    lx: r.lo - e,
                    ux: r.lo,
                    ly: r.minus,
                    uy: r.plus,
                },
                Rect {
                    lx: r.hi,
                    ux: r.hi + e,
                    ly: r.minus,
                    uy: r.plus,
                },
            ]
        };
        candidates.push((r.net, r.layer, ends[0], r.node_lo, false));
        candidates.push((r.net, r.layer, ends[1], r.node_hi, false));
    }
    for p in &built.pads {
        if p.rect.valid() {
            candidates.push((p.net, p.layer, p.rect, p.node, true));
        }
    }
    for (net, layer, rect, node, is_pad) in candidates {
        let covering = same_net_near(rect, net, layer, near, shapes, limits, stats)?;
        let pieces = uncovered(rect, &covering, shapes)?;
        if is_pad {
            stats.pad_pieces += pieces.len();
        } else {
            stats.end_pieces += pieces.len();
        }
        for piece in pieces {
            add_piece(piece, net, layer, node, built, shapes, near, limits, stats)?;
        }
    }
    Ok(())
}

/// Attach every routing patch to the RC node of a same-net conductor it
/// overlaps or touches (largest overlap area, then lowest shape index; a wire
/// body attaches at its end nearer the patch centre, a piece at its node), and
/// add each piece of the patch not already covered by same-net metal on its
/// layer as a conductor shape with two side strips. Patches add capacitance
/// only: they never add a node or a resistor (the tree stays the wire graph's
/// tree).
#[allow(clippy::too_many_arguments)]
fn add_patches(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    built: &mut Built,
    shapes: &mut Shapes<'_>,
    near: &mut NetShapes,
    net_index: &HashMap<u32, usize, BuildFx>,
    stats: &mut ExtractStats,
) -> Result<()> {
    for patch in input
        .routes
        .patches
        .iter()
        .filter(|p| input.extracts(p.net))
    {
        let r = patch.rect;
        if !r.valid()
            || patch.layer as usize >= input.layers.len()
            || input.layers[patch.layer as usize].vertical.is_none()
        {
            return Err(invalid("routing patch is empty or not on a routing layer"));
        }
        layer_model(input, patch.layer)?;
        if !net_index.contains_key(&patch.net) {
            return Err(invalid("routing patch on a net without an RC tree"));
        }
        // Same-net conductors on the patch layer touching the patch (closed).
        let touching = same_net_near(r, patch.net, patch.layer, near, shapes, limits, stats)?;
        let overlap = |q: Rect| -> i128 {
            let w = i128::from(q.ux.min(r.ux)) - i128::from(q.lx.max(r.lx));
            let h = i128::from(q.uy.min(r.uy)) - i128::from(q.ly.max(r.ly));
            w.max(0) * h.max(0)
        };
        let anchor = touching
            .iter()
            .copied()
            .max_by_key(|&s| (overlap(shapes.get(s).rect), std::cmp::Reverse(s)))
            .ok_or_else(|| invalid("floating routing patch (touches no same-net conductor)"))?;
        let node = match shapes.get(anchor).owner {
            Owner::Piece(p) => built.pieces[p as usize].node,
            Owner::Run(o) => {
                let run = &built.runs[o as usize];
                let centre = if run.vertical {
                    r.ly + (r.uy - r.ly) / 2
                } else {
                    r.lx + (r.ux - r.lx) / 2
                };
                if (centre - run.lo).abs() <= (run.hi - centre).abs() {
                    run.node_lo
                } else {
                    run.node_hi
                }
            }
            Owner::Passive(PassiveOwner::Pin { pin, .. }) => built
                .pin_nodes
                .get(&pin)
                .filter(|(n, _)| *n == patch.net)
                .map(|(_, node)| *node)
                .ok_or_else(|| invalid("routing patch touches an unrouted pin only"))?,
            Owner::Passive(PassiveOwner::Ground) => unreachable!("ground has no net"),
        };
        // Uncovered pieces: subtract every same-net conductor overlapping it.
        let pieces = uncovered(r, &touching, shapes)?;
        stats.patches += 1;
        stats.patch_pieces += pieces.len();
        for piece in pieces {
            add_piece(
                piece,
                patch.net,
                patch.layer,
                node,
                built,
                shapes,
                near,
                limits,
                stats,
            )?;
        }
    }
    Ok(())
}

/// The end node of a strip nearer to `point` (Manhattan distance; ties go to
/// the lower end).
fn nearer_end(run: &Run, point: [f64; 2]) -> u32 {
    let ends = if run.vertical {
        [
            [run.cross() as f64, run.lo as f64],
            [run.cross() as f64, run.hi as f64],
        ]
    } else {
        [
            [run.lo as f64, run.cross() as f64],
            [run.hi as f64, run.cross() as f64],
        ]
    };
    let d = |p: [f64; 2]| (p[0] - point[0]).abs() + (p[1] - point[1]).abs();
    if d(ends[1]) < d(ends[0]) {
        run.node_hi
    } else {
        run.node_lo
    }
}

/// Passive conductors (pins, obstructions, supply metal) that a routed
/// conductor on another routing layer overlaps get two strips (along x and
/// along y, like a piece) so that their own cross-section is counted where a
/// routed conductor is their calibration plate: a crossing is counted from
/// both conductors' cross-sections whoever owns them. Their shape IDs are
/// recorded; the strips are generated when their units are computed.
fn add_passive_strips(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    built: &mut Built,
    shapes: &Shapes<'_>,
    grid: &index::GridIndex,
    stats: &mut ExtractStats,
) -> Result<()> {
    let routing: Vec<u32> = (0..input.layers.len() as u32)
        .filter(|&l| input.tech.layers[l as usize].is_some())
        .collect();
    for (si, sh) in shapes.owned() {
        if !matches!(sh.owner, Owner::Passive(_)) {
            continue;
        }
        let r = sh.rect;
        let mut crossed = false;
        for &layer in routing.iter().filter(|&&l| l != sh.layer) {
            grid.visit(layer, r, limits, &mut stats.work, |id, _| {
                crossed = !grid.is_virtual(id) && shapes.get(id).routed();
                Ok(!crossed)
            })?;
            if crossed {
                break;
            }
        }
        if !crossed {
            continue;
        }
        built.passive_units.push(si);
        stats.passive_strips += 2;
        if built.strips() > limits.max_shapes.saturating_mul(2) {
            return Err(budget("extraction strips"));
        }
    }
    Ok(())
}

/// Mark only virtual conductors crossed by routed metal. One bit per virtual
/// shape replaces a retained rectangle plus two passive-strip records.
///
/// Routed shapes are scanned in parallel chunks; each pool thread owns one
/// bitset and query scratch (admitted within `available` bytes, otherwise
/// the scan is serial), OR-merged at the end: the marked set and the total
/// work are order-independent. Any query error or a work total over the
/// budget is reproduced exactly by the serial scan from the original work
/// count. Returns the marks and the parallel scratch bytes used.
fn virtual_crossings(
    shapes: &Shapes<'_>,
    grid: &index::GridIndex,
    instances: Option<&InstanceGeometry<'_>>,
    layers: &[u32],
    limits: &ExtractLimits,
    available: u64,
    stats: &mut ExtractStats,
) -> Result<(Vec<u64>, u64)> {
    use rayon::prelude::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let words = shapes.virtual_count().div_ceil(64);
    let geometry = match instances {
        Some(g) => Some(g),
        None if grid.has_virtual() => None,
        None => return Ok((vec![0u64; words], 0)),
    };
    let instance_count = shapes.source.map_or(0, |s| s.geometry.len());
    let offset = shapes.virtual_offset();
    // Virtual shapes strictly overlapping a routed shape on another layer,
    // from the grid (strict intersection) or the instance index.
    let scan = |part: &[Shape],
                sc: &mut InstanceQueryScratch,
                marked: &mut [u64],
                work: &mut usize|
     -> Result<()> {
        for shape in part.iter().filter(|s| s.routed()) {
            for &layer in layers.iter().filter(|&&l| l != shape.layer) {
                let mut mark = |id: u32, b: Rect| {
                    let a = shape.rect;
                    if a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy {
                        let k = id as usize - offset;
                        marked[k / 64] |= 1 << (k % 64);
                    }
                };
                match geometry {
                    Some(geometry) => {
                        geometry.visit_shared(
                            sc,
                            shapes,
                            layer,
                            shape.rect,
                            work,
                            |id, other| {
                                mark(id, other.rect);
                                Ok(true)
                            },
                        )?;
                    }
                    None => {
                        grid.visit(layer, shape.rect, limits, work, |id, rect| {
                            if grid.is_virtual(id) {
                                mark(id, rect);
                            }
                            Ok(true)
                        })?;
                    }
                }
            }
        }
        Ok(())
    };
    let retained = shapes.retained();
    let scratch_instances = if geometry.is_some() {
        instance_count
    } else {
        0
    };
    // One bitset and instance-mark scratch per pool thread.
    let threads = rayon::current_num_threads();
    let per_thread = (words as u64)
        .saturating_mul(8)
        .saturating_add((scratch_instances as u64).saturating_mul(8));
    let parallel_bytes = (threads as u64).saturating_mul(per_thread);
    let parallel = (threads > 1 && parallel_bytes <= available)
        .then(|| {
            type State = Option<(Vec<u64>, InstanceQueryScratch)>;
            let states: Vec<Mutex<State>> = (0..threads).map(|_| Mutex::new(None)).collect();
            let work = AtomicUsize::new(0);
            let failed = AtomicBool::new(false);
            retained.par_chunks(4096).for_each(|part| {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let Some(slot) = rayon::current_thread_index().and_then(|i| states.get(i)) else {
                    failed.store(true, Ordering::Relaxed);
                    return;
                };
                // Only this pool thread locks its own slot.
                let mut state = slot.lock().unwrap_or_else(|e| e.into_inner());
                if state.is_none() {
                    *state = InstanceQueryScratch::new(scratch_instances)
                        .ok()
                        .map(|sc| (vec![0u64; words], sc));
                }
                let Some((marked, sc)) = state.as_mut() else {
                    failed.store(true, Ordering::Relaxed);
                    return;
                };
                let mut local = 0usize;
                if scan(part, sc, marked, &mut local).is_err() {
                    failed.store(true, Ordering::Relaxed);
                }
                let _ = work.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
                    Some(w.saturating_add(local))
                });
            });
            let mut marked = vec![0u64; words];
            for state in states {
                if let Some((m, _)) = state.into_inner().unwrap_or_else(|e| e.into_inner()) {
                    for (x, y) in marked.iter_mut().zip(&m) {
                        *x |= y;
                    }
                }
            }
            (marked, work.into_inner(), failed.into_inner())
        })
        .and_then(|(marked, work, failed)| {
            let total = stats
                .work
                .checked_add(work)
                .filter(|&n| !failed && n <= limits.max_work)?;
            Some((marked, total))
        });
    let (marked, used) = match parallel {
        Some((marked, total)) => {
            stats.work = total;
            (marked, parallel_bytes)
        }
        None => {
            let mut marked = vec![0u64; words];
            let mut sc = InstanceQueryScratch::new(scratch_instances)?;
            scan(retained, &mut sc, &mut marked, &mut stats.work)?;
            (marked, 0)
        }
    };
    stats.passive_strips = stats
        .passive_strips
        .checked_add(
            marked
                .iter()
                .map(|w| w.count_ones() as usize)
                .sum::<usize>()
                .saturating_mul(2),
        )
        .ok_or_else(|| budget("virtual passive strips"))?;
    Ok((marked, used))
}

/// One strip (x or y) of a crossed passive conductor (retained or virtual).
fn conductor_strip(id: u32, shape: Shape, vertical: bool) -> Run {
    let Owner::Passive(owner) = shape.owner else {
        unreachable!("crossed conductor is passive")
    };
    let r = shape.rect;
    let (lo, hi, minus, plus) = if vertical {
        (r.ly, r.uy, r.lx, r.ux)
    } else {
        (r.lx, r.ux, r.ly, r.uy)
    };
    Run {
        net: match owner {
            PassiveOwner::Pin { net, .. } => net,
            PassiveOwner::Ground => u32::MAX,
        },
        layer: shape.layer,
        vertical,
        lo,
        hi,
        minus,
        plus,
        shape: id,
        piece: true,
        passive: Some(owner),
        node_lo: 0,
        node_hi: 0,
    }
}

/// One strip (x or y) of a routed-metal piece: both ends at its node.
#[inline]
fn piece_strip(id: u32, r: Rect, layer: u32, info: PieceInfo, vertical: bool) -> Run {
    let (lo, hi, minus, plus) = if vertical {
        (r.ly, r.uy, r.lx, r.ux)
    } else {
        (r.lx, r.ux, r.ly, r.uy)
    };
    Run {
        net: info.net,
        layer,
        vertical,
        lo,
        hi,
        minus,
        plus,
        shape: id,
        piece: true,
        passive: None,
        node_lo: info.node,
        node_hi: info.node,
    }
}

fn sample(rows: &[[f64; 3]], spacing: Option<f64>) -> (f64, f64) {
    let last = rows[rows.len() - 1];
    let Some(s) = spacing else {
        return (0.0, last[2]);
    };
    if s <= rows[0][0] {
        return (rows[0][1], rows[0][2]);
    }
    if s >= last[0] {
        return (last[1], last[2]);
    }
    let i = rows.partition_point(|r| r[0] <= s);
    let (a, b) = (rows[i - 1], rows[i]);
    let t = (s - a[0]) / (b[0] - a[0]);
    (a[1] + t * (b[1] - a[1]), a[2] + t * (b[2] - a[2]))
}

/// Compute an RC tree for every routed net plus grounded and coupling caps.
/// Strips are computed on the caller's Rayon pool; results, statistics and
/// errors are identical at every pool size.
pub fn extract_cpu_ref(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<RcNetwork> {
    extract_with_queries(input, limits, Queries::Indexed(parallel::Bounds::default()))
}

/// Candidate-query implementation.
#[derive(Clone, Copy)]
enum Queries {
    /// Per-layer grids and instance index, face queries on other layers,
    /// shared x/y pair queries, parallel tiles within these bounds.
    Indexed(parallel::Bounds),
    /// Independent test oracle: coarse conductor bins, the full lateral
    /// window on every layer, one strip at a time, serial.
    #[cfg(test)]
    Reference,
}

fn extract_with_queries(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    queries: Queries,
) -> Result<RcNetwork> {
    let mut profile = PhaseProfile::new();
    preflight(input, limits)?;
    let memory = memory_estimate(input, limits)?;
    let (mut built, mut stats) = build_trees(input, limits)?;
    profile.phase(0);
    stats.memory = Some(memory);
    let tech = input.tech;
    // Conductor shapes: wire-run bodies and passive shapes; the end
    // extensions, via pads and patches are added as pieces below.
    let mut shapes: Vec<Shape> = Vec::new();
    for (i, r) in built.runs.iter().enumerate() {
        shapes.push(Shape {
            layer: r.layer,
            rect: body_rect(r),
            owner: Owner::Run(i as u32),
        });
    }
    for p in input.passive {
        if (p.layer as usize) < input.layers.len()
            && input.layers[p.layer as usize].vertical.is_some()
        {
            shapes.push(Shape {
                layer: p.layer,
                rect: p.rect,
                owner: Owner::Passive(p.owner),
            });
        }
    }
    let mut shapes = Shapes::new(shapes, built.runs.len(), input.instances, limits)?;
    stats.shapes = shapes.len();
    if shapes.retained_len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    let routing_layers: Vec<u32> = (0..input.layers.len() as u32)
        .filter(|&l| tech.layers[l as usize].is_some())
        .collect();
    // The same-net lists, then the grid, share the spatial-index allowance
    // (plus any headroom below the memory limit); they are never alive
    // together.
    let headroom = memory.limit_bytes.saturating_sub(memory.estimate_bytes);
    let index_budget = headroom.saturating_add(index_allowance(limits));
    // Cell geometry is indexed once, at the admitted query bin.
    let instances = InstanceGeometry::new(
        &shapes,
        memory.instance_query_bin_dbu,
        input.layers.len(),
        limits,
    )?;
    let net_index: HashMap<u32, usize, BuildFx> = built
        .nets
        .iter()
        .enumerate()
        .map(|(i, n)| (n.net, i))
        .collect();
    profile.phase(1);
    let net_bins = {
        let long = match queries {
            Queries::Indexed(bounds) => bounds.net_bin_list,
            #[cfg(test)]
            Queries::Reference => NET_BIN_LIST,
        };
        let mut near = NetShapes::new(
            &shapes,
            &built,
            limits,
            index_budget,
            extraction_bin(input)?,
            long,
        )?;
        add_metal_pieces(
            input,
            limits,
            &mut built,
            &mut shapes,
            &mut near,
            &mut stats,
        )?;
        add_patches(
            input,
            limits,
            &mut built,
            &mut shapes,
            &mut near,
            &net_index,
            &mut stats,
        )?;
        (near.bins.lists, near.bins.entries.len())
    };
    // Via pads are fully converted into pieces; release growth slack.
    built.pads = Vec::new();
    built.pieces.shrink_to_fit();
    shapes.shrink_to_fit();
    profile.phase(2);
    // Grid cells: at least 2 um and the layer's lateral window, so a strip
    // query spans few cells on every layer.
    let min_cell = (2.0 * tech.dbu_per_um).ceil().max(1.0) as i64;
    let base_cell: Vec<i64> = tech
        .layers
        .iter()
        .map(|model| {
            model.as_ref().map_or(min_cell, |m| {
                let window = m
                    .contexts
                    .iter()
                    .map(|c| c.rows[c.rows.len() - 1][0])
                    .fold(0.0f64, f64::max);
                ((window * tech.dbu_per_um).ceil() as i64).max(min_cell)
            })
        })
        .collect();
    // Virtual cell shapes on routing layers join the grid when it fits the
    // entry cap and the configured index allowance (never the live-memory
    // headroom, so the query path, and with it the work count, depends only
    // on the input and the limits): strip and crossing queries then need no
    // instance-index traversal.
    let virtual_grid = match queries {
        Queries::Indexed(bounds) => bounds.virtual_grid,
        #[cfg(test)]
        Queries::Reference => true,
    };
    let routing_mask: Vec<bool> = tech.layers.iter().map(Option::is_some).collect();
    let grid = match (virtual_grid && instances.is_some())
        .then(|| {
            index::GridIndex::build_virtual(
                &shapes,
                &routing_mask,
                input.layers.len(),
                &base_cell,
                limits,
                index_allowance(limits),
                &mut stats.work,
            )
        })
        .flatten()
    {
        Some(grid) => grid,
        None => index::GridIndex::build(
            &shapes,
            input.layers.len(),
            &base_cell,
            limits,
            index_budget,
            &mut stats.work,
        )?,
    };
    profile.phase(3);
    // A grid larger than the index allowance holds part of the headroom: it
    // joins the working estimate. Transient parallel scratch (crossing
    // marks, then strip lanes) gets the unused index allowance first, then
    // the headroom the grid left.
    let instance_count = input.instances.map_or(0, |i| i.geometry.len());
    let unused = index_allowance(limits).saturating_sub(grid.bytes());
    let grid_excess = grid.bytes().saturating_sub(index_allowance(limits));
    let headroom = headroom.saturating_sub(grid_excess);
    if grid_excess > 0
        && let Some(estimate) = stats.memory.as_mut()
    {
        estimate.working_bytes = estimate.working_bytes.saturating_add(grid_excess);
        estimate.estimate_bytes = estimate.estimate_bytes.saturating_add(grid_excess);
        if estimate.estimate_bytes > estimate.limit_bytes {
            return Err(budget("extraction grid index memory"));
        }
    }
    add_passive_strips(input, limits, &mut built, &shapes, &grid, &mut stats)?;
    profile.phase(4);
    let (crossed, crossing_bytes) = virtual_crossings(
        &shapes,
        &grid,
        instances.as_ref(),
        &routing_layers,
        limits,
        headroom.saturating_add(unused),
        &mut stats,
    )?;
    // After the crossings, the instance index serves only the strip queries
    // the grid does not cover (the test oracle keeps its own).
    #[cfg(test)]
    let keep = !grid.has_virtual() || matches!(queries, Queries::Reference);
    #[cfg(not(test))]
    let keep = !grid.has_virtual();
    let instances = instances.filter(|_| keep);
    let cell_index = instances.as_ref().filter(|_| !grid.has_virtual());
    stats.wire_runs = built.runs.len();
    stats.shapes = shapes.len();
    if shapes.retained_len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    profile.phase(5);
    let (bounds, threads) = match queries {
        Queries::Indexed(bounds) => (bounds, rayon::current_num_threads()),
        #[cfg(test)]
        Queries::Reference => (parallel::Bounds::default(), 1),
    };
    let lanes = parallel::admit(
        bounds,
        threads,
        headroom.saturating_add(unused),
        limits,
        instance_count,
    );
    let lane_bytes = if lanes > 1 || limits.tile_units > 0 {
        parallel::parallel_bytes(bounds, lanes, limits, instance_count)
    } else {
        0
    };
    // The crossing marks are released before the lanes start: the larger
    // excess over the unused allowance is the peak.
    let excess = lane_bytes.max(crossing_bytes).saturating_sub(unused);
    if excess > 0
        && let Some(estimate) = stats.memory.as_mut()
    {
        estimate.working_bytes = estimate.working_bytes.saturating_add(excess);
        estimate.estimate_bytes = estimate.estimate_bytes.saturating_add(excess);
        if estimate.estimate_bytes > estimate.limit_bytes {
            return Err(budget("extraction tile memory"));
        }
    }
    let nodes = NodeIds::new(&built.nets, &net_index)?;
    let mut acc = parallel::Accumulators::new(
        parallel::shard_count(lanes, limits),
        nodes.total,
        stats.overlap_area_um2,
    )?;
    let report = {
        // Units in the original run order: wire bodies, pieces, crossed
        // virtual conductors, then crossed retained passive conductors.
        let (offset, count) = (shapes.virtual_offset(), shapes.virtual_count());
        let mut units = (0..built.runs.len() as u32)
            .map(strip::UnitRef::Body)
            .chain((0..built.pieces.len() as u32).map(strip::UnitRef::Piece))
            .chain(
                (0..count)
                    .filter(|&i| crossed[i / 64] & (1 << (i % 64)) != 0)
                    .map(|i| strip::UnitRef::Conductor((offset + i) as u32)),
            )
            .chain(
                built
                    .passive_units
                    .iter()
                    .map(|&id| strip::UnitRef::Conductor(id)),
            );
        let ctx = strip::Ctx::new(
            input,
            &shapes,
            &grid,
            cell_index,
            &routing_layers,
            limits,
            &built.runs,
            (&built.pieces, built.piece_shape0),
            &built.pin_nodes,
            &net_index,
            &nodes,
        )?;
        match queries {
            Queries::Indexed(bounds) => parallel::run(
                &ctx,
                &mut units,
                bounds,
                lanes,
                instance_count,
                &mut acc,
                &mut stats,
            )?,
            #[cfg(test)]
            Queries::Reference => reference(
                &ctx,
                input,
                instances.as_ref(),
                &shapes,
                &mut units,
                instance_count,
                parallel::Commit::new(&mut acc, &mut stats, limits.max_couplings),
            )?,
        }
    };
    profile.phase(6);
    let grid_size = (grid.entries(), grid.bytes());
    // Geometry is no longer needed: release it before the coupling list.
    drop(grid);
    drop(instances);
    drop(shapes);
    drop(crossed);
    let Built {
        mut nets,
        runs,
        pieces,
        passive_units,
        pin_nodes,
        ..
    } = built;
    drop((runs, pieces, passive_units, pin_nodes));
    stats.overlap_area_um2 = acc.area();
    for (l, net) in nets.iter_mut().enumerate() {
        for (node, ground) in net.ground.iter_mut().enumerate() {
            *ground = acc.ground(nodes.local(l, node as u32));
        }
    }
    let sorted = acc.into_sorted_couplings()?;
    let mut couplings = Vec::new();
    for (key, pf) in sorted {
        if pf <= 0.0 {
            continue;
        }
        let ((la, na), (lb, nb)) = (nodes.decode((key >> 32) as u32), nodes.decode(key as u32));
        if pf < limits.min_coupling_pf {
            // Each side keeps the full value to ground (Miller factor one);
            // a net's total capacitance is unchanged.
            nets[la].ground[na as usize] += pf;
            nets[lb].ground[nb as usize] += pf;
            stats.grounded_couplings += 1;
            continue;
        }
        couplings.push(RcCoupling {
            a: (nets[la].net, na),
            b: (nets[lb].net, nb),
            pf,
        });
    }
    stats.couplings = couplings.len();
    stats.coupling_pf = couplings.iter().map(|c| c.pf).sum();
    stats.ground_pf = nets.iter().flat_map(|n| n.ground.iter()).sum();
    let elements = stats
        .resistors
        .checked_add(stats.nodes)
        .and_then(|n| n.checked_add(2 * stats.couplings))
        .ok_or_else(|| budget("extraction elements"))?;
    if elements > limits.max_elements {
        return Err(budget("extraction RC elements"));
    }
    guard_memory(limits)?;
    profile.phase(7);
    profile.report(&stats, &report, grid_size, net_bins);
    Ok(RcNetwork {
        nets,
        couplings,
        stats,
    })
}

/// Serial oracle: candidates from coarse insertion-ordered conductor bins and
/// the instance index over the full lateral window on every layer,
/// deduplicated and ID-sorted, then the shared numerical sweep.
#[cfg(test)]
fn reference(
    ctx: &strip::Ctx<'_, '_>,
    input: &ExtractInput<'_>,
    geometry: Option<&InstanceGeometry<'_>>,
    shapes: &Shapes<'_>,
    units: &mut impl Iterator<Item = strip::UnitRef>,
    instances: usize,
    mut commit: parallel::Commit<'_>,
) -> Result<parallel::Report> {
    let limits = ctx.limits;
    let size = extraction_bin(input)?;
    let mut bins = bins::BinLists::default();
    for (id, s) in shapes.owned() {
        for bx in s.rect.lx.div_euclid(size)..=s.rect.ux.div_euclid(size) {
            for by in s.rect.ly.div_euclid(size)..=s.rect.uy.div_euclid(size) {
                bins.push((s.layer, bx, by), id, limits.max_bin_entries)?;
            }
        }
    }
    let mut sc = strip::Scratch::new(instances)?;
    let mut report = parallel::Report::default();
    let mut cands = Vec::new();
    for u in units {
        let (first, second) = strip::strips(ctx, u);
        report.units += 1;
        for run in std::iter::once(first).chain(second) {
            let w = ctx.window_dbu(run.layer)?;
            let (c0, c1) = (run.minus - w, run.plus + w);
            let q = if run.vertical {
                Rect {
                    lx: c0,
                    ly: run.lo,
                    ux: c1,
                    uy: run.hi,
                }
            } else {
                Rect {
                    lx: run.lo,
                    ly: c0,
                    ux: run.hi,
                    uy: c1,
                }
            };
            cands.clear();
            for &layer in ctx.routing_layers {
                let lateral = (layer == run.layer).then_some((c0, c1));
                for bx in q.lx.div_euclid(size)..=q.ux.div_euclid(size) {
                    for by in q.ly.div_euclid(size)..=q.uy.div_euclid(size) {
                        for id in bins.iter((layer, bx, by)) {
                            let shape = shapes.get(id);
                            if strip::accepts(&run, lateral, id, shape.rect) {
                                cands.push(strip::Cand { id, shape });
                            }
                        }
                    }
                }
                if let Some(geometry) = geometry {
                    geometry.visit_shared(
                        &mut sc.instances,
                        shapes,
                        layer,
                        q,
                        &mut 0,
                        |id, shape| {
                            if strip::accepts(&run, lateral, id, shape.rect) {
                                cands.push(strip::Cand { id, shape });
                            }
                            Ok(true)
                        },
                    )?;
                }
            }
            cands.sort_unstable_by_key(|c| c.id);
            cands.dedup_by_key(|c| c.id);
            if cands.len() > limits.max_candidates {
                return Err(budget("extraction candidates per wire run"));
            }
            let mut counts = strip::Counts {
                max_run_candidates: cands.len(),
                ..Default::default()
            };
            let mut work = commit.stats().work;
            let result = strip::numeric_with(
                ctx,
                &run,
                &cands,
                &mut sc,
                &mut commit,
                &mut work,
                &mut counts,
            );
            commit.stats().work = work;
            match result {
                Ok(()) => counts.add_to(commit.stats()),
                Err(strip::Stop::Error(e)) => return Err(e),
                Err(strip::Stop::Defer) => return Err(invalid("reference strip deferred")),
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{Access, LayerRect, Patch, RouteStats, Via, Wire};

    #[test]
    fn strip_records_stay_compact() {
        // Millions of strips and transcript records are resident at once.
        assert_eq!(size_of::<Run>(), 72);
        assert_eq!(size_of::<Shape>(), 48);
        assert_eq!(size_of::<strip::Op>(), 16);
        assert_eq!(size_of::<PieceInfo>(), 8);
    }

    #[test]
    fn lazy_piece_and_conductor_strips_match_the_materialized_runs() {
        // The original construction pushed these records (x strip, then y
        // strip) per piece and per crossed retained passive conductor.
        fn materialized(
            r: Rect,
            net: u32,
            layer: u32,
            shape: u32,
            passive: Option<PassiveOwner>,
            node: u32,
        ) -> Vec<Run> {
            let mut runs = Vec::new();
            for vertical in [false, true] {
                let (lo, hi, minus, plus) = if vertical {
                    (r.ly, r.uy, r.lx, r.ux)
                } else {
                    (r.lx, r.ux, r.ly, r.uy)
                };
                runs.push(Run {
                    net,
                    layer,
                    vertical,
                    lo,
                    hi,
                    minus,
                    plus,
                    shape,
                    piece: true,
                    passive,
                    node_lo: node,
                    node_hi: node,
                });
            }
            runs
        }
        let fields = |r: &Run| {
            (
                r.net,
                r.layer,
                r.vertical,
                (r.lo, r.hi, r.minus, r.plus),
                r.shape,
                r.piece,
                r.passive,
                (r.node_lo, r.node_hi),
                r.cross(),
            )
        };
        let rects = [
            Rect {
                lx: -7,
                ly: 3,
                ux: 5,
                uy: 4,
            },
            Rect {
                lx: 100,
                ly: -2_000_000,
                ux: 101,
                uy: 2_000_000,
            },
            Rect {
                lx: 0,
                ly: 0,
                ux: 9,
                uy: 9,
            },
        ];
        for (i, &rect) in rects.iter().enumerate() {
            let (id, layer) = (40 + i as u32, i as u32 % 3);
            let info = PieceInfo {
                net: 7 + i as u32,
                node: 3 * i as u32,
            };
            let lazy = [false, true].map(|v| piece_strip(id, rect, layer, info, v));
            let want = materialized(rect, info.net, layer, id, None, info.node);
            assert_eq!(
                lazy.map(|r| fields(&r)).to_vec(),
                want.iter().map(fields).collect::<Vec<_>>()
            );
            for owner in [PassiveOwner::Ground, PassiveOwner::Pin { net: 9, pin: 11 }] {
                let shape = Shape {
                    layer,
                    rect,
                    owner: Owner::Passive(owner),
                };
                let lazy = [false, true].map(|v| conductor_strip(id, shape, v));
                let net = match owner {
                    PassiveOwner::Pin { net, .. } => net,
                    PassiveOwner::Ground => u32::MAX,
                };
                let want = materialized(rect, net, layer, id, Some(owner), 0);
                assert_eq!(
                    lazy.map(|r| fields(&r)).to_vec(),
                    want.iter().map(fields).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn global_node_ids_follow_net_then_node_order() {
        let net = |net: u32, nodes: usize| RcNet {
            net,
            nodes: vec![RcNode::Internal(1); nodes],
            ground: vec![0.0; nodes],
            resistors: vec![],
        };
        // Local order differs from net ID order.
        let nets = vec![net(30, 2), net(4, 3), net(17, 1), net(1_000_000, 2)];
        let index: HashMap<u32, usize, BuildFx> =
            nets.iter().enumerate().map(|(i, n)| (n.net, i)).collect();
        let ids = NodeIds::new(&nets, &index).unwrap();
        assert_eq!(ids.total, 8);
        let mut all = Vec::new();
        for (l, n) in nets.iter().enumerate() {
            for node in 0..n.nodes.len() as u32 {
                let g = ids.local(l, node);
                assert_eq!(ids.of(&index, n.net, node), g);
                assert_eq!(ids.decode(g), (l, node));
                all.push(((n.net, node), g));
            }
        }
        all.sort();
        assert!(all.windows(2).all(|w| w[0].1 < w[1].1));
        // Coupling keys sort like the original endpoint tuples.
        let ends = [
            ((30, 1), (4, 2)),
            ((17, 0), (4, 2)),
            ((4, 0), (1_000_000, 1)),
        ];
        let keys = ends.map(|(p, q)| {
            strip::Op::couple(ids.of(&index, p.0, p.1), ids.of(&index, q.0, q.1), 1.0).key
        });
        let tuples = ends.map(|(p, q)| if p <= q { (p, q) } else { (q, p) });
        for i in 0..3 {
            assert!(keys[i] < strip::GROUND);
            for j in 0..3 {
                assert_eq!(keys[i].cmp(&keys[j]), tuples[i].cmp(&tuples[j]));
            }
        }
    }

    #[test]
    fn spatial_header_bound_covers_negative_bins_and_saturates_extreme_extents() {
        let r = Rect {
            lx: -11,
            ly: -20,
            ux: 11,
            uy: 20,
        };
        assert_eq!(bin_count(&[Some(r)], 10, 1000), 20);
        assert_eq!(bin_count(&[Some(r), Some(r)], 10, 1000), 40);
        assert_eq!(bin_count(&[Some(r), Some(r)], 10, 25), 25);
        let huge = Rect {
            lx: i64::MIN,
            ly: i64::MIN,
            ux: i64::MAX,
            uy: i64::MAX,
        };
        assert_eq!(bin_count(&[Some(huge)], 1, 17), 17);
        assert_eq!(bin_count(&[None], 1, 17), 0);
    }

    #[test]
    fn refined_query_extent_matches_an_independent_finite_grid_and_bounds_overflow() {
        for bin in 1..9i64 {
            for lo in -5..5i64 {
                let bounds = Rect {
                    lx: lo,
                    ly: -3,
                    ux: lo + 3,
                    uy: 2,
                };
                // Enumerate finite cells with the index's inclusive upper
                // boundary; this oracle uses no division or span products.
                let count = (-32..32i64)
                    .flat_map(|x| (-32..32i64).map(move |y| (x, y)))
                    .filter(|&(x, y)| {
                        x * bin <= bounds.ux + 3
                            && (x + 1) * bin > bounds.lx - 3
                            && y * bin <= bounds.uy + 3
                            && (y + 1) * bin > bounds.ly - 3
                    })
                    .count();
                assert!(query_extent_fits(bounds, bin, 2, count));
                assert!(!query_extent_fits(bounds, bin, 2, count - 1));
            }
        }
        assert!(!query_extent_fits(
            Rect {
                lx: i64::MIN,
                ly: i64::MIN,
                ux: i64::MAX,
                uy: i64::MAX
            },
            1,
            i64::MAX / 2,
            usize::MAX
        ));
    }

    #[test]
    fn tiny_context_window_retains_coarse_geometry_index_when_refinement_cannot_fit() {
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        // One virtual macro rectangle; only arithmetic admission is exercised,
        // so the millions of rejected fine bins are never allocated.
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(
                    5000,
                    0,
                    vec![TemplateObstacle {
                        layer: 0,
                        rect: Rect {
                            lx: 0,
                            ly: 0,
                            ux: 10_000,
                            uy: 5000,
                        },
                        pin: None,
                    }],
                )
                .unwrap(),
            ],
            vec![ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [0, 0],
                orientation: crate::Orientation::N,
            }],
            vec![],
        )
        .unwrap()
        .with_pin_ids(vec![])
        .unwrap();
        let r = routes(vec![], vec![], vec![], vec![]);
        let (l, v) = (layers(), vias());
        let mut tk = tech(false);
        tk.layers[0].as_mut().unwrap().contexts[0].rows =
            vec![[0.0, 1e-4, 1e-5], [0.001, 0.0, 4e-5]];
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &[],
            passive: &[],
            instances: Some(InstancedPassive {
                geometry: &source,
                signal_nets: &[],
            }),
            held_bytes: 0,
            tech: &tk,
        };
        let limits = ExtractLimits {
            max_instance_bin_entries: 64,
            ..Default::default()
        };
        let memory = memory_estimate(&input, &limits).unwrap();
        assert_eq!(instance_query_bin(&input).unwrap(), 2);
        assert_eq!(
            memory.instance_query_bin_dbu,
            extraction_bin(&input).unwrap()
        );
        assert!(memory.estimate_bytes <= memory.limit_bytes);
    }

    const DBU: f64 = 1000.0;
    fn rows() -> Vec<[f64; 3]> {
        // spacing um, coupling pF/um, ground pF/um per side
        vec![[0.1, 1e-4, 1e-5], [0.5, 4e-5, 3e-5], [1.0, 0.0, 4e-5]]
    }
    fn tech(two_planes: bool) -> RcTech {
        let mut contexts = vec![RcContext {
            over: 0,
            under: None,
            rows: rows(),
        }];
        if two_planes {
            contexts.push(RcContext {
                over: 1,
                under: None,
                rows: vec![[0.1, 9e-5, 2e-5], [0.5, 3e-5, 5e-5], [1.0, 0.0, 6e-5]],
            });
        }
        RcTech {
            dbu_per_um: DBU,
            layers: vec![
                Some(RcLayer {
                    metal: 1,
                    width: 100,
                    sheet_ohm: 0.1,
                    contexts: vec![RcContext {
                        over: 0,
                        under: None,
                        rows: rows(),
                    }],
                    area_cap: None,
                    overlap: vec![],
                }),
                Some(RcLayer {
                    metal: 2,
                    width: 200,
                    sheet_ohm: 0.05,
                    contexts,
                    area_cap: None,
                    overlap: vec![],
                }),
                None,
            ],
            via_ohm: vec![2.0],
        }
    }
    fn layers() -> Vec<RouteLayer> {
        vec![
            RouteLayer {
                name: "M1".into(),
                width: 100,
                spacing: 100,
                vertical: Some(false),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
            RouteLayer {
                name: "M2".into(),
                width: 200,
                spacing: 100,
                vertical: Some(true),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
            RouteLayer {
                name: "V1".into(),
                width: 100,
                spacing: 100,
                vertical: None,
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
        ]
    }
    fn vias() -> Vec<RouteVia> {
        vec![RouteVia {
            name: "V12".into(),
            lower: 0,
            upper: 1,
            shapes: vec![
                LayerRect {
                    layer: 0,
                    rect: Rect {
                        lx: -100,
                        ly: -100,
                        ux: 100,
                        uy: 100,
                    },
                },
                LayerRect {
                    layer: 2,
                    rect: Rect {
                        lx: -50,
                        ly: -50,
                        ux: 50,
                        uy: 50,
                    },
                },
                LayerRect {
                    layer: 1,
                    rect: Rect {
                        lx: -100,
                        ly: -100,
                        ux: 100,
                        uy: 100,
                    },
                },
            ],
        }]
    }
    fn terminal(pin: u32, net: u32, driver: bool) -> Terminal {
        Terminal {
            pin,
            net,
            driver,
            shapes: vec![],
            class: None,
        }
    }
    fn routes(wires: Vec<Wire>, vias: Vec<Via>, access: Vec<Access>, nets: Vec<u32>) -> Routes {
        Routes {
            wires,
            vias,
            access,
            patches: vec![],
            nets,
            stats: RouteStats::default(),
        }
    }
    /// A straight M1 wire split into lattice edges: 0..10 um.
    fn straight(net: u32, y: i64, pieces: i64) -> Vec<Wire> {
        let step = 10_000 / pieces;
        (0..pieces)
            .map(|i| Wire {
                net,
                layer: 0,
                start: [i * step, y],
                end: [(i + 1) * step, y],
            })
            .collect()
    }
    fn assert_network_exact(a: &RcNetwork, b: &RcNetwork) {
        assert_eq!(a.nets.len(), b.nets.len());
        for (a, b) in a.nets.iter().zip(&b.nets) {
            assert_eq!((a.net, &a.nodes), (b.net, &b.nodes));
            assert_eq!(
                a.ground.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                b.ground.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(
                a.resistors
                    .iter()
                    .map(|&(x, y, v)| (x, y, v.to_bits()))
                    .collect::<Vec<_>>(),
                b.resistors
                    .iter()
                    .map(|&(x, y, v)| (x, y, v.to_bits()))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            a.couplings
                .iter()
                .map(|c| (c.a, c.b, c.pf.to_bits()))
                .collect::<Vec<_>>(),
            b.couplings
                .iter()
                .map(|c| (c.a, c.b, c.pf.to_bits()))
                .collect::<Vec<_>>()
        );
        for (a, b) in [
            (a.stats.overlap_area_um2, b.stats.overlap_area_um2),
            (a.stats.ground_pf, b.stats.ground_pf),
            (a.stats.coupling_pf, b.stats.coupling_pf),
            (a.stats.resistance_ohm, b.stats.resistance_ohm),
        ] {
            assert_eq!(a.to_bits(), b.to_bits());
        }
        let clean = |s: &ExtractStats| {
            let mut value = serde_json::to_value(s).unwrap();
            value.as_object_mut().unwrap().remove("memory");
            value
        };
        assert_eq!(clean(&a.stats), clean(&b.stats));
    }
    /// One shared pool per size: fresh pools per call would spread test
    /// allocations over new threads' malloc arenas, raising the process
    /// peak resident set that other tests' runtime guards observe.
    fn pool(threads: usize) -> &'static rayon::ThreadPool {
        static POOLS: [std::sync::OnceLock<rayon::ThreadPool>; 5] =
            [const { std::sync::OnceLock::new() }; 5];
        POOLS[threads].get_or_init(|| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
        })
    }
    /// The serial path: one pool thread, no forced tiles.
    fn serial(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<RcNetwork> {
        pool(1).install(|| {
            extract_cpu_ref(
                input,
                &ExtractLimits {
                    tile_units: 0,
                    ..limits.clone()
                },
            )
        })
    }
    /// Parallel bounds small enough that fixtures defer units to the serial
    /// path (candidate cap) and fill chunk transcripts.
    fn tight_bounds() -> parallel::Bounds {
        parallel::Bounds {
            candidates: 2,
            chunk_ops: 24,
            pending_bytes: 1,
            virtual_grid: true,
            net_bin_list: NET_BIN_LIST,
        }
    }
    /// Every pool size and tile size, with default and tight parallel
    /// bounds, reproduces the serial network and statistics bit for bit.
    fn check_parallel(input: &ExtractInput<'_>, limits: &ExtractLimits, serial: &RcNetwork) {
        for (threads, tile) in [(1, 1), (1, 3), (2, 0), (2, 7), (3, 1), (4, 0), (4, 64)] {
            let limits = ExtractLimits {
                tile_units: tile,
                ..limits.clone()
            };
            let instance_path = parallel::Bounds {
                virtual_grid: false,
                ..tight_bounds()
            };
            for bounds in [parallel::Bounds::default(), tight_bounds(), instance_path] {
                let mut parallel = pool(threads)
                    .install(|| extract_with_queries(input, &limits, Queries::Indexed(bounds)))
                    .unwrap();
                if bounds.virtual_grid {
                    assert_eq!(serial.stats.work, parallel.stats.work);
                } else {
                    // The instance-index path scans other index cells.
                    parallel.stats.work = serial.stats.work;
                }
                assert_network_exact(serial, &parallel);
                let memory = parallel.stats.memory.unwrap();
                assert!(memory.estimate_bytes <= memory.limit_bytes);
            }
        }
        // Every same-net list bucketed: the same network (only work differs).
        let bucketed = parallel::Bounds {
            net_bin_list: 1,
            ..parallel::Bounds::default()
        };
        let mut parallel = pool(2)
            .install(|| extract_with_queries(input, limits, Queries::Indexed(bucketed)))
            .unwrap();
        parallel.stats.work = serial.stats.work;
        assert_network_exact(serial, &parallel);
    }
    fn run(
        r: &Routes,
        t: &[Terminal],
        passive: &[PassiveShape],
        tech: &RcTech,
    ) -> Result<RcNetwork> {
        let (l, v) = (layers(), vias());
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: r,
            nets: None,
            terminals: t,
            passive,
            held_bytes: 0,
            instances: None,
            tech,
        };
        let limits = ExtractLimits::default();
        let actual = serial(&input, &limits)?;
        let reference = extract_with_queries(&input, &limits, Queries::Reference)?;
        assert_eq!(actual.nets.len(), reference.nets.len());
        for (a, b) in actual.nets.iter().zip(&reference.nets) {
            assert_eq!((a.net, &a.nodes), (b.net, &b.nodes));
            assert_eq!(
                a.ground.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                b.ground.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(
                a.resistors
                    .iter()
                    .map(|&(x, y, v)| (x, y, v.to_bits()))
                    .collect::<Vec<_>>(),
                b.resistors
                    .iter()
                    .map(|&(x, y, v)| (x, y, v.to_bits()))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            actual
                .couplings
                .iter()
                .map(|c| (c.a, c.b, c.pf.to_bits()))
                .collect::<Vec<_>>(),
            reference
                .couplings
                .iter()
                .map(|c| (c.a, c.b, c.pf.to_bits()))
                .collect::<Vec<_>>()
        );
        check_parallel(&input, &limits, &actual);
        Ok(actual)
    }

    #[test]
    fn isolated_straight_wire_matches_hand_computed_rc() {
        // 10 um of M1, width 0.1 um, sheet 0.1 ohm/sq: R = 0.1 * 10 / 0.1 = 10 ohm.
        // Isolated ground per side 4e-5 pF/um, two sides: 2 * 4e-5 * 10 = 8e-4 pF.
        // Each 0.05 um half-width end extension exposes 0.05 + 0.05 + 0.1 um
        // of edge: 4e-5 * 0.2 = 8e-6 pF at its end node.
        let r = routes(
            straight(0, 0, 10),
            vec![],
            vec![
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
                    at: [10_000, 0],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        assert_eq!(n.nets.len(), 1);
        let net = &n.nets[0];
        // Nine interior lattice nodes merge: one resistor between the pins.
        assert_eq!(net.nodes, vec![RcNode::Pin(0), RcNode::Pin(1)]);
        assert_eq!(net.resistors.len(), 1);
        assert!((net.resistors[0].2 - 10.0).abs() < 1e-12);
        let c: f64 = net.ground.iter().sum();
        assert!((c - 8.16e-4).abs() < 1e-15, "{c}");
        assert!((net.ground[0] - 4.08e-4).abs() < 1e-15);
        assert!(n.couplings.is_empty());
        assert_eq!((n.stats.end_pieces, n.stats.pad_pieces), (2, 0));
    }

    #[test]
    fn parallel_neighbour_couples_once_and_splits_ground() {
        // Two parallel 10 um wires, centerlines 0.6 um apart: edge gap 0.5 um.
        let mut wires = straight(0, 0, 4);
        wires.extend(straight(1, 600, 5));
        let access = vec![
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
                at: [10_000, 0],
            },
            Access {
                pin: 2,
                net: 1,
                layer: 0,
                at: [0, 600],
            },
            Access {
                pin: 3,
                net: 1,
                layer: 0,
                at: [10_000, 600],
            },
        ];
        let r = routes(wires, vec![], access, vec![0, 1]);
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        // Facing side: cc 4e-5, cg 3e-5; far side isolated cg 4e-5. The end
        // extensions (0.05 um long) face each other too; their outer short
        // edges (0.1 um) are isolated.
        let coupling: f64 = n.couplings.iter().map(|c| c.pf).sum();
        let want = 4e-5 * 10.0 + 2.0 * 4e-5 * 0.05;
        assert!((coupling - want).abs() < 1e-15, "{coupling}");
        for net in &n.nets {
            let g: f64 = net.ground.iter().sum();
            let end = 0.05 * (3e-5 + 4e-5) + 0.1 * 4e-5;
            assert!(
                (g - ((3e-5 + 4e-5) * 10.0 + 2.0 * end)).abs() < 1e-15,
                "{g}"
            );
        }
        // Deterministic.
        let m = run(&r, &t, &[], &tech(false)).unwrap();
        assert_eq!(n.couplings, m.couplings);
        assert_eq!(n.nets[0].ground, m.nets[0].ground);
    }

    #[test]
    fn couplings_below_the_threshold_become_ground_on_both_nets() {
        let mut wires = straight(0, 0, 4);
        wires.extend(straight(1, 600, 5));
        let access = [
            (0, 0, [0, 0]),
            (1, 0, [10_000, 0]),
            (2, 1, [0, 600]),
            (3, 1, [10_000, 600]),
        ]
        .map(|(pin, net, at)| Access {
            pin,
            net,
            layer: 0,
            at,
        })
        .to_vec();
        let r = routes(wires, vec![], access, vec![0, 1]);
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let kept = run(&r, &t, &[], &tech(false)).unwrap();
        let (l, v) = (layers(), vias());
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            held_bytes: 0,
            instances: None,
            tech: &tech(false),
        };
        let biggest = kept.couplings.iter().map(|c| c.pf).fold(0.0, f64::max);
        let limits = ExtractLimits {
            min_coupling_pf: biggest * 1.5,
            ..ExtractLimits::default()
        };
        let grounded = extract_cpu_ref(&input, &limits).unwrap();
        assert!(grounded.couplings.is_empty());
        assert_eq!(grounded.stats.grounded_couplings, kept.couplings.len());
        let coupling: f64 = kept.couplings.iter().map(|c| c.pf).sum();
        for (a, b) in kept.nets.iter().zip(&grounded.nets) {
            let before: f64 = a.ground.iter().sum();
            let after: f64 = b.ground.iter().sum();
            assert!(
                (after - before - coupling).abs() < 1e-15,
                "{before} {after}"
            );
        }
        let half = ExtractLimits {
            min_coupling_pf: 0.011,
            ..ExtractLimits::default()
        };
        assert!(
            extract_cpu_ref(&input, &half).is_err(),
            "above 10 fF is rejected"
        );
    }

    #[test]
    fn branches_vias_and_pins_split_runs_into_a_tree() {
        // M1 trunk 0..10 um with a pin at 4 um, a via at 10 um up to M2 and an
        // M2 branch to (10, 5) um. Via ohm 2, M2 R = 0.05 * 5 / 0.2 = 1.25.
        let mut wires = straight(0, 0, 10);
        wires.extend((0..5).map(|i| Wire {
            net: 0,
            layer: 1,
            start: [10_000, i * 1000],
            end: [10_000, (i + 1) * 1000],
        }));
        let r = routes(
            wires,
            vec![Via {
                net: 0,
                kind: 0,
                at: [10_000, 0],
            }],
            vec![
                Access {
                    pin: 5,
                    net: 0,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: 6,
                    net: 0,
                    layer: 0,
                    at: [4000, 0],
                },
                Access {
                    pin: 7,
                    net: 0,
                    layer: 1,
                    at: [10_000, 5000],
                },
            ],
            vec![0],
        );
        let t = [
            terminal(5, 0, true),
            terminal(6, 0, false),
            terminal(7, 0, false),
        ];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        let net = &n.nets[0];
        assert_eq!(net.nodes.len(), 5); // 3 pins + 2 via-end junctions
        assert_eq!(net.nodes[0], RcNode::Pin(5));
        assert_eq!(net.resistors.len(), 4);
        let mut ohms: Vec<f64> = net.resistors.iter().map(|r| r.2).collect();
        ohms.sort_by(f64::total_cmp);
        let want = [1.25, 2.0, 4.0, 6.0];
        for (a, b) in ohms.iter().zip(want) {
            assert!((a - b).abs() < 1e-12, "{ohms:?}");
        }
        assert!(net.nodes.contains(&RcNode::Internal(1)));
        assert!(net.nodes.contains(&RcNode::Internal(2)));
        assert_eq!(n.stats.opened_loop_resistors, 0);
    }

    #[test]
    fn long_runs_split_and_loops_open_deterministically() {
        // A rectangle loop: 4 sides of 10 um, driver and sink on opposite corners.
        let mut wires = straight(0, 0, 10);
        wires.extend(straight(0, 10_000, 10));
        for x in [0, 10_000] {
            wires.extend((0..10).map(|i| Wire {
                net: 0,
                layer: 0,
                start: [x, i * 1000],
                end: [x, (i + 1) * 1000],
            }));
        }
        let r = routes(
            wires,
            vec![],
            vec![
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
                    at: [10_000, 10_000],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let (l, v) = (layers(), vias());
        let limits = ExtractLimits {
            max_segment_length: 3000,
            ..Default::default()
        };
        let input = ExtractInput {
            held_bytes: 0,
            instances: None,
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            tech: &tech(false),
        };
        let a = extract_cpu_ref(&input, &limits).unwrap();
        let b = extract_cpu_ref(&input, &limits).unwrap();
        assert_eq!(a.stats.opened_loop_resistors, 1);
        let net = &a.nets[0];
        assert_eq!(net.resistors.len() + 1, net.nodes.len(), "tree");
        assert_eq!(net.resistors, b.nets[0].resistors);
        // Every run is at most 3 um: 4 sides * ceil(10/3) runs.
        assert_eq!(a.stats.wire_runs, 16);
    }

    #[test]
    fn plane_below_becomes_coupling_and_ground_pin_shapes_ground() {
        // M2 vertical wire over an M1 wire of another net (crossing), plus a
        // grounded M2 obstruction 0.5 um away on one side.
        let mut wires: Vec<Wire> = (0..10)
            .map(|i| Wire {
                net: 0,
                layer: 1,
                start: [0, i * 1000],
                end: [0, (i + 1) * 1000],
            })
            .collect();
        wires.push(Wire {
            net: 1,
            layer: 0,
            start: [-2000, 5000],
            end: [2000, 5000],
        });
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 1,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [0, 10_000],
                },
                Access {
                    pin: 2,
                    net: 1,
                    layer: 0,
                    at: [-2000, 5000],
                },
                Access {
                    pin: 3,
                    net: 1,
                    layer: 0,
                    at: [2000, 5000],
                },
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let obstruction = PassiveShape {
            layer: 1,
            rect: Rect {
                lx: 600,
                ly: 0,
                ux: 900,
                uy: 10_000,
            },
            owner: PassiveOwner::Ground,
        };
        let n = run(&r, &t, &[obstruction], &tech(true)).unwrap();
        assert!(n.couplings.iter().all(|c| c.a.0 != c.b.0));
        // Only the 0.1 um crossing interval of the upper run couples. The
        // plate below shields the substrate, so the whole OVER-1 ground of
        // each side goes to it: 5e-5 facing the obstruction at 0.5 um and
        // 6e-5 isolated, per um.
        let total: f64 = n.couplings.iter().map(|c| c.pf).sum();
        assert!((total - 1.1e-5).abs() < 1e-18, "{total}");
        // The lower run has no UNDER table: explicit fallback, no B-cut term.
        assert!(n.stats.context_fallback_intervals > 0);
        // Upper run ground: substrate part both sides plus lateral to the
        // obstruction outside the crossing; inside it only the lateral cc(0.5)
        // of OVER-1 (3e-5) to the obstruction; plus its two 0.2 x 0.1 um end
        // extensions (isolated except the edge touching the body).
        let g: f64 = n.nets[0].ground.iter().sum();
        let want = 9.9 * (3e-5 + 4e-5 + 4e-5) + 0.1 * 3e-5 + 2.0 * 0.4 * 4e-5;
        assert!((g - want).abs() < 1e-15, "{g} {want}");
        // The upper run's total is still its context total (shielding only
        // moves the substrate share to the plate).
        let old_total = 10.0 * (3e-5 + 4e-5 + 4e-5) - 0.1 * (4e-5 - 3e-5) + 4e-6;
        assert!((g + total - old_total - 2.0 * 0.4 * 4e-5).abs() < 1e-15);
        assert_eq!(n.stats.foreign_overlaps, 0);
    }

    /// Isolated ground per side and um of the test rows.
    const ISO: f64 = 4e-5;

    #[test]
    fn compact_segments_extract_like_lattice_edges() {
        // The same metal stored as one record per lattice edge or as one
        // record per straight run (another stream may store compact
        // segments) gives the same network.
        let build = |pieces: i64| {
            let step = 5000 / pieces;
            let mut wires: Vec<Wire> = (0..pieces)
                .map(|i| Wire {
                    net: 0,
                    layer: 0,
                    start: [i * step, 0],
                    end: [(i + 1) * step, 0],
                })
                .collect();
            wires.extend((0..pieces).map(|i| Wire {
                net: 0,
                layer: 1,
                start: [5000, i * step],
                end: [5000, (i + 1) * step],
            }));
            let mut r = routes(
                wires,
                vec![Via {
                    net: 0,
                    kind: 0,
                    at: [5000, 0],
                }],
                vec![
                    access(0, 0, [0, 0]),
                    Access {
                        pin: 1,
                        net: 0,
                        layer: 1,
                        at: [5000, 5000],
                    },
                ],
                vec![0],
            );
            r.wires.extend(straight(1, 600, pieces.min(10)));
            r.access
                .extend([access(2, 1, [0, 600]), access(3, 1, [10_000, 600])]);
            r.nets = vec![0, 1];
            r
        };
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let lattice = run(&build(10), &t, &[], &tech(true)).unwrap();
        let compact = run(&build(1), &t, &[], &tech(true)).unwrap();
        assert_eq!(lattice.nets.len(), compact.nets.len());
        for (a, b) in lattice.nets.iter().zip(&compact.nets) {
            assert_eq!(a.nodes, b.nodes);
            assert_eq!(a.resistors, b.resistors);
            for (x, y) in a.ground.iter().zip(&b.ground) {
                assert!((x - y).abs() < 1e-15, "{x} {y}");
            }
        }
        let sum = |n: &RcNetwork| n.couplings.iter().map(|c| c.pf).sum::<f64>();
        assert!((sum(&lattice) - sum(&compact)).abs() < 1e-15);
        assert_eq!(lattice.stats.wire_runs, compact.stats.wire_runs);
        assert_eq!(lattice.stats.pad_pieces, compact.stats.pad_pieces);
    }

    #[test]
    fn a_crossed_passive_conductor_counts_its_own_cut() {
        // An M2 wire (0.2 um, x = 0, y 0..10 um) crosses a grounded M1 rail
        // 1 um tall and 10 um wide (y 4.5..5.5 um). The rail is a plate below
        // the wire over 1 um; the wire is a plate above the rail over 0.2 um.
        // A passive conductor models only that second cut: its UNDER excess
        // (7e-5 - 4e-5 per side) goes to the wire's net as ground.
        let mut tech = tech(true);
        if let Some(m1) = tech.layers[0].as_mut() {
            m1.contexts.push(RcContext {
                over: 0,
                under: Some(2),
                rows: vec![[0.1, 9e-5, 3e-5], [0.5, 3e-5, 6e-5], [1.0, 0.0, 7e-5]],
            });
        }
        let wires: Vec<Wire> = vec![Wire {
            net: 0,
            layer: 1,
            start: [0, 0],
            end: [0, 10_000],
        }];
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 1,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [0, 10_000],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let rail = PassiveShape {
            layer: 0,
            rect: Rect {
                lx: -5000,
                ly: 4500,
                ux: 5000,
                uy: 5500,
            },
            owner: PassiveOwner::Ground,
        };
        let n = run(&r, &t, &[rail], &tech).unwrap();
        // Wire: substrate 4e-5 per side over 9 um of body and the exposed
        // 0.4 um of edge per end extension; OVER-1 6e-5 per side over the 1 um
        // rail. Rail cut: 2 sides x 3e-5 x 0.2 um.
        let want = 4e-5 * (2.0 * 9.0 + 0.8) + 2.0 * 6e-5 * 1.0 + 2.0 * 3e-5 * 0.2;
        let got: f64 = n.nets[0].ground.iter().sum();
        assert!((got - want).abs() < 1e-15, "{got} {want}");
        assert_eq!(n.stats.passive_strips, 2);
        assert!(n.couplings.is_empty());
        // The same rail as a routed conductor of another net gives the wire's
        // net the same total: the cut is the conductor's, whoever owns it.
        let mut r2 = r.clone();
        r2.wires.push(Wire {
            net: 1,
            layer: 0,
            start: [-5000, 5000],
            end: [5000, 5000],
        });
        r2.access
            .extend([access(2, 1, [-5000, 5000]), access(3, 1, [5000, 5000])]);
        r2.nets = vec![0, 1];
        let t2 = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let mut wide = tech.clone();
        if let Some(m1) = wide.layers[0].as_mut() {
            m1.width = 1000;
        }
        let mut wide_layers = layers();
        wide_layers[0].width = 1000;
        let v = vias();
        let routed = extract_cpu_ref(
            &ExtractInput {
                held_bytes: 0,
                instances: None,
                layers: &wide_layers,
                vias: &v,
                routes: &r2,
                nets: None,
                terminals: &t2,
                passive: &[],
                tech: &wide,
            },
            &ExtractLimits::default(),
        )
        .unwrap();
        let total0 = routed.nets[0].ground.iter().sum::<f64>()
            + routed
                .couplings
                .iter()
                .filter(|c| c.a.0 == 0 || c.b.0 == 0)
                .map(|c| c.pf)
                .sum::<f64>();
        assert!((total0 - want).abs() < 1e-15, "{total0} {want}");
    }

    #[test]
    fn non_plate_overlap_couples_over_the_covered_face_once() {
        // An M2 wire (0.2 um) over an aligned, narrower M1 wire (0.1 um) of
        // another net: not a calibration plate. With overlap data the upper
        // face couples over the covered 0.1 um at 3e-4 pF/um^2 (counted once)
        // and loses 1e-4 pF/um^2 of face-to-substrate capacitance. The
        // covered length is the M1 body plus its two 0.05 um end extensions:
        // 10.1 um.
        let wires = vec![
            Wire {
                net: 0,
                layer: 1,
                start: [0, 0],
                end: [10_000, 0],
            },
            Wire {
                net: 1,
                layer: 0,
                start: [0, 0],
                end: [10_000, 0],
            },
        ];
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 1,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [10_000, 0],
                },
                access(2, 1, [0, 0]),
                access(3, 1, [10_000, 0]),
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let off = run(&r, &t, &[], &tech(true)).unwrap();
        let mut with = tech(true);
        if let Some(m2) = with.layers[1].as_mut() {
            m2.area_cap = Some(1e-4);
            m2.overlap = vec![(1, 3e-4)];
        }
        if let Some(m1) = with.layers[0].as_mut() {
            m1.area_cap = Some(1e-4);
        }
        let on = run(&r, &t, &[], &with).unwrap();
        let area = 0.1 * 10.1;
        assert!(
            (on.stats.overlap_area_um2 - area).abs() < 1e-12,
            "{}",
            on.stats.overlap_area_um2
        );
        let coupling: f64 = on.couplings.iter().map(|c| c.pf).sum();
        let before: f64 = off.couplings.iter().map(|c| c.pf).sum();
        assert!(
            (coupling - before - 3e-4 * area).abs() < 1e-15,
            "{coupling} {before}"
        );
        let g = |n: &RcNetwork, k: usize| n.nets[k].ground.iter().sum::<f64>();
        assert!((g(&off, 0) - g(&on, 0) - 1e-4 * area).abs() < 1e-15);
        assert!(
            (g(&off, 1) - g(&on, 1)).abs() < 1e-18,
            "lower wire unchanged"
        );
        assert_eq!(on.stats.overlap_pairs_without_data, 0);
    }

    #[test]
    fn long_runs_split_at_the_same_coordinates_however_stored() {
        // An M1 run 0..10 um with a T branch at x = 4 um (up to y = 3 um) and
        // a segment limit of 3 um. Stored as 1 um lattice edges, or as one
        // record through the T point plus an overlapping duplicate record
        // (3..5 um): both give runs 0-2, 2-4, 4-7, 7-10 and the 3 um branch,
        // i.e. ceil(L / limit) equal pieces cut at exact coordinates.
        let build = |lattice: bool| {
            let mut wires = if lattice {
                straight(0, 0, 10)
            } else {
                vec![
                    Wire {
                        net: 0,
                        layer: 0,
                        start: [0, 0],
                        end: [10_000, 0],
                    },
                    Wire {
                        net: 0,
                        layer: 0,
                        start: [5000, 0],
                        end: [3000, 0],
                    },
                ]
            };
            let branch = if lattice { 3 } else { 1 };
            let step = 3000 / branch;
            wires.extend((0..branch).map(|i| Wire {
                net: 0,
                layer: 0,
                start: [4000, i * step],
                end: [4000, (i + 1) * step],
            }));
            routes(
                wires,
                vec![],
                vec![
                    access(0, 0, [0, 0]),
                    access(1, 0, [10_000, 0]),
                    access(2, 0, [4000, 3000]),
                ],
                vec![0],
            )
        };
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 0, false),
        ];
        let (l, v) = (layers(), vias());
        let limits = ExtractLimits {
            max_segment_length: 3000,
            ..Default::default()
        };
        let tech = tech(false);
        let extract = |r: &Routes| {
            extract_cpu_ref(
                &ExtractInput {
                    held_bytes: 0,
                    instances: None,
                    layers: &l,
                    vias: &v,
                    routes: r,
                    nets: None,
                    terminals: &t,
                    passive: &[],
                    tech: &tech,
                },
                &limits,
            )
            .unwrap()
        };
        let (a, b) = (extract(&build(true)), extract(&build(false)));
        assert_eq!(a.nets[0].nodes, b.nets[0].nodes);
        assert_eq!(a.nets[0].resistors, b.nets[0].resistors);
        for (x, y) in a.nets[0].ground.iter().zip(&b.nets[0].ground) {
            assert!((x - y).abs() < 1e-18, "{x} {y}");
        }
        // 0.1 ohm/sq, 0.1 um wide: 1 ohm per um.
        let mut ohms: Vec<f64> = a.nets[0].resistors.iter().map(|r| r.2).collect();
        ohms.sort_by(f64::total_cmp);
        let want = [2.0, 2.0, 3.0, 3.0, 3.0];
        assert_eq!(ohms.len(), want.len());
        for (got, want) in ohms.iter().zip(want) {
            assert!((got - want).abs() < 1e-12, "{ohms:?}");
        }
        assert_eq!((a.stats.wire_runs, b.stats.wire_runs), (5, 5));
        // Lattice storage has a node at every cut; one record needs two.
        assert_eq!(
            (a.stats.segment_cut_nodes, b.stats.segment_cut_nodes),
            (0, 2)
        );
        assert_eq!(
            (a.stats.duplicate_wire_edges, b.stats.duplicate_wire_edges),
            (0, 2)
        );
    }

    #[test]
    fn narrower_parallel_metal_below_is_not_a_plate() {
        // M2 (0.2 um) vertical net 0 over a parallel M1 (0.1 um) net 1 that
        // lies inside its footprint for 6 um: the M1 wire does not span the
        // M2 cross-section, so the M2 wire keeps its substrate context and no
        // plate coupling is invented (the overlap is counted as unmodelled).
        let mut wires: Vec<Wire> = (0..10)
            .map(|i| Wire {
                net: 0,
                layer: 1,
                start: [0, i * 1000],
                end: [0, (i + 1) * 1000],
            })
            .collect();
        wires.extend((0..6).map(|i| Wire {
            net: 1,
            layer: 0,
            start: [0, 2000 + i * 1000],
            end: [0, 3000 + i * 1000],
        }));
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 1,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [0, 10_000],
                },
                access(2, 1, [0, 2000]),
                access(3, 1, [0, 8000]),
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let n = run(&r, &t, &[], &tech(true)).unwrap();
        assert!(n.couplings.is_empty(), "{:?}", n.couplings);
        let g0: f64 = n.nets[0].ground.iter().sum();
        assert!(
            (g0 - (2.0 * ISO * 10.0 + 2.0 * 0.4 * ISO)).abs() < 1e-15,
            "{g0}"
        );
        assert!(n.stats.nonspanning_plane_intervals > 0);
    }

    #[test]
    fn a_crossing_is_counted_from_both_wires_cross_sections() {
        // M2 (0.2 um) vertical net 0 at x = 0 over an M1 (0.1 um) horizontal
        // net 1 at y = 5 um. M2 OVER-1 isolated ground 6e-5 per side; M1
        // UNDER-2 isolated ground 5e-5 per side against 4e-5 over substrate.
        let mut tk = tech(true);
        tk.layers[0].as_mut().unwrap().contexts.push(RcContext {
            over: 0,
            under: Some(2),
            rows: vec![[0.1, 8e-5, 1.5e-5], [0.5, 3e-5, 4.5e-5], [1.0, 0.0, 5e-5]],
        });
        let mut wires: Vec<Wire> = (0..10)
            .map(|i| Wire {
                net: 0,
                layer: 1,
                start: [0, i * 1000],
                end: [0, (i + 1) * 1000],
            })
            .collect();
        wires.extend((0..4).map(|i| Wire {
            net: 1,
            layer: 0,
            start: [-2000 + i * 1000, 5000],
            end: [-1000 + i * 1000, 5000],
        }));
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 1,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [0, 10_000],
                },
                access(2, 1, [-2000, 5000]),
                access(3, 1, [2000, 5000]),
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let n = run(&r, &t, &[], &tk).unwrap();
        // Upper cut: the plate shields the substrate, both sides' 6e-5 go to
        // the M1 wire over its 0.1 um width. Lower cut: the excess 5e-5 - 4e-5
        // per side goes to the M2 wire over its 0.2 um width.
        let coupling: f64 = n.couplings.iter().map(|c| c.pf).sum();
        let want = 2.0 * 6e-5 * 0.1 + 2.0 * (5e-5 - 4e-5) * 0.2;
        assert!((coupling - want).abs() < 1e-18, "{coupling} {want}");
        // M2 ground: substrate both sides except over the plate, plus ends.
        let g0: f64 = n.nets[0].ground.iter().sum();
        assert!(
            (g0 - (2.0 * ISO * 9.9 + 2.0 * 0.4 * ISO)).abs() < 1e-15,
            "{g0}"
        );
        // M1 keeps its substrate share under the plate: 4 um body, two ends.
        let g1: f64 = n.nets[1].ground.iter().sum();
        assert!(
            (g1 - (2.0 * ISO * 4.0 + 2.0 * 0.2 * ISO)).abs() < 1e-15,
            "{g1}"
        );
        assert_eq!(n.stats.context_fallback_intervals, 0);
    }

    #[test]
    fn l_corner_counts_exactly_the_union_perimeter() {
        // M1 (0.1 um wide) from (0,0) to (5,0) um, then to (5,5) um. The metal
        // union (bodies plus half-width end extensions) is an L whose perimeter
        // is 5.1 + 5.1 + 0.1 + 5.0 + 5.0 + 0.1 = 20.4 um; every edge is
        // isolated. The inner-corner sides inside the other body are covered,
        // and the outer corner is one 0.05 x 0.05 um piece.
        let mut wires = straight(0, 0, 10)
            .into_iter()
            .filter(|w| w.end[0] <= 5000)
            .collect::<Vec<_>>();
        wires.extend((0..5).map(|i| Wire {
            net: 0,
            layer: 0,
            start: [5000, i * 1000],
            end: [5000, (i + 1) * 1000],
        }));
        let r = routes(
            wires,
            vec![],
            vec![access(0, 0, [0, 0]), access(1, 0, [5000, 5000])],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        let total: f64 = n.nets[0].ground.iter().sum();
        assert!((total - ISO * 20.4).abs() < 1e-15, "{total}");
        assert_eq!(n.stats.wire_runs, 2);
        assert_eq!(n.stats.end_pieces, 3); // two free ends and the outer corner
        assert_eq!(n.stats.covered_side_intervals, 2);
    }

    #[test]
    fn via_pads_add_their_metal_outside_the_wires() {
        // M1 (0.1 um) from (0,0) to (5,0) um, via V12 at (5,0) um with 0.2 x
        // 0.2 um pads on M1 and M2, then M2 (0.2 um) up to (5,5) um.
        // M1 union: wire [-0.05,5.0] x [-0.05,0.05] and pad [4.9,5.1] x
        // [-0.1,0.1]; perimeter 0.1 + 2 * 4.95 + 2 * 0.05 + 3 * 0.2 = 10.7 um.
        // M2 union: [4.9,5.1] x [-0.1,5.1] (the pad is inside the wire and its
        // end extension); perimeter 2 * 0.2 + 2 * 5.2 = 10.8 um. Same-net
        // planes carry no capacitance, so every edge is isolated.
        let mut wires = straight(0, 0, 10)
            .into_iter()
            .filter(|w| w.end[0] <= 5000)
            .collect::<Vec<_>>();
        wires.extend((0..5).map(|i| Wire {
            net: 0,
            layer: 1,
            start: [5000, i * 1000],
            end: [5000, (i + 1) * 1000],
        }));
        let r = routes(
            wires,
            vec![Via {
                net: 0,
                kind: 0,
                at: [5000, 0],
            }],
            vec![
                access(0, 0, [0, 0]),
                Access {
                    pin: 1,
                    net: 0,
                    layer: 1,
                    at: [5000, 5000],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        let total: f64 = n.nets[0].ground.iter().sum();
        assert!((total - ISO * (10.7 + 10.8)).abs() < 1e-15, "{total}");
        // Pad pieces: the M1 pad minus the wire body and the end extension.
        assert_eq!((n.stats.end_pieces, n.stats.pad_pieces), (4, 5));
        // All M1 pad metal sits at the via's M1 node; R is unchanged.
        assert_eq!(n.nets[0].resistors.len(), 3);
        // A neighbour beside the pad couples to the pad edge (0.5 um away),
        // counted once: half from each side.
        let mut r2 = r.clone();
        r2.wires.extend((0..4).map(|i| Wire {
            net: 1,
            layer: 0,
            start: [5600, -2000 + i * 1000],
            end: [5600, -1000 + i * 1000],
        }));
        r2.access
            .extend([access(2, 1, [5600, -2000]), access(3, 1, [5600, 2000])]);
        r2.nets = vec![0, 1];
        let t2 = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let n2 = run(&r2, &t2, &[], &tech(false)).unwrap();
        let coupling: f64 = n2.couplings.iter().map(|c| c.pf).sum();
        // Pad right edge x = 5.1 um, net 1 left edge x = 5.55 um: cc(0.45)
        // over the pad's 0.2 um.
        assert!((coupling - 4.75e-5 * 0.2).abs() < 1e-18, "{coupling}");
    }

    #[test]
    fn sides_inside_the_nets_own_pin_are_not_exposed() {
        // M1 wire 0..10 um whose first 1 um lies on its own M1 pin shape
        // [-0.2,1.0] x [-0.2,0.2] um: both sides over that 1 um and the start
        // extension are pin metal (Liberty), not routing capacitance.
        let pin = PassiveShape {
            layer: 0,
            rect: Rect {
                lx: -200,
                ly: -200,
                ux: 1000,
                uy: 200,
            },
            owner: PassiveOwner::Pin { net: 0, pin: 0 },
        };
        let r = patched(vec![]);
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let n = run(&r, &t, &[pin], &tech(false)).unwrap();
        let total: f64 = n.nets[0].ground.iter().sum();
        assert!((total - ISO * (2.0 * 9.0 + 0.2)).abs() < 1e-15, "{total}");
        assert_eq!(n.stats.covered_side_intervals, 2);
        assert_eq!(n.stats.end_pieces, 1);
    }

    fn access(pin: u32, net: u32, at: [i64; 2]) -> Access {
        Access {
            pin,
            net,
            layer: 0,
            at,
        }
    }
    fn patch(net: u32, rect: [i64; 4]) -> Patch {
        Patch {
            net,
            layer: 0,
            rect: Rect {
                lx: rect[0],
                ly: rect[1],
                ux: rect[2],
                uy: rect[3],
            },
        }
    }
    /// Net 0: 10 um M1 wire on y = 0 (pins at both ends) plus `patches`.
    fn patched(patches: Vec<Patch>) -> Routes {
        let mut r = routes(
            straight(0, 0, 10),
            vec![],
            vec![access(0, 0, [0, 0]), access(1, 0, [10_000, 0])],
            vec![0],
        );
        r.patches = patches;
        r
    }

    #[test]
    fn patch_beside_wire_adds_its_exposed_edges_at_the_nearer_end_node() {
        // 1 x 0.2 um patch on the wire's top edge (touching, no overlap).
        // Wire: bottom side 10 um isolated, top side loses the 1 um the patch
        // covers. Patch: top edge 1 um and two 0.2 um short edges isolated;
        // its bottom edge touches the wire. Isolated ground 4e-5 pF/um/side.
        // Each end extension adds 0.2 um of isolated edge (8e-6 pF).
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let plain = run(&patched(vec![]), &t, &[], &tech(false)).unwrap();
        let n = run(
            &patched(vec![patch(0, [4000, 50, 5000, 250])]),
            &t,
            &[],
            &tech(false),
        )
        .unwrap();
        let net = &n.nets[0];
        // No new node or resistor: the tree is unchanged.
        assert_eq!(net.nodes, plain.nets[0].nodes);
        assert_eq!(net.resistors, plain.nets[0].resistors);
        let total: f64 = net.ground.iter().sum();
        let want = 4e-5 * (10.0 + 9.0) + 4e-5 * (1.0 + 0.2 + 0.2) + 2.0 * 8e-6;
        assert!((total - want).abs() < 1e-15, "{total} {want}");
        // Patch centre x = 4.5 um is nearer the driver end (node 0).
        let wire0 = 4e-5 * (4.0 * 0.8 + 0.55 + 5.0 * 0.25) + 4e-5 * (4.0 * 0.8 + 5.0 * 0.25);
        assert!(
            (net.ground[0] - (wire0 + 5.6e-5 + 8e-6)).abs() < 1e-15,
            "{:?}",
            net.ground
        );
        assert_eq!((n.stats.patches, n.stats.patch_pieces), (1, 1));
        assert_eq!(n.stats.wire_runs, plain.stats.wire_runs);
        // A patch overlapping the wire counts only its uncovered piece.
        let overlapping = run(
            &patched(vec![patch(0, [4000, 0, 5000, 250])]),
            &t,
            &[],
            &tech(false),
        )
        .unwrap();
        assert_eq!(overlapping.nets[0].ground, net.ground);
        assert_eq!(overlapping.stats.patch_pieces, 1);
        // A patch fully inside same-net metal adds nothing.
        let inside = run(
            &patched(vec![patch(0, [4000, -50, 5000, 50])]),
            &t,
            &[],
            &tech(false),
        )
        .unwrap();
        assert_eq!(inside.nets[0].ground, plain.nets[0].ground);
        assert_eq!((inside.stats.patches, inside.stats.patch_pieces), (1, 0));
    }

    #[test]
    fn patch_side_couples_once_including_a_facing_wire_end() {
        // Net 1 parallel to net 0 at y = 750 (edges 0.65 um apart; the patch
        // top is 0.45 um away over 1 um). Lateral pairs count half per side;
        // the 0.05 um end extensions of both wires face each other at 0.65 um.
        let mut r = patched(vec![patch(0, [4000, 50, 5000, 250])]);
        r.wires.extend(straight(1, 750, 5));
        r.access
            .extend([access(2, 1, [0, 750]), access(3, 1, [10_000, 750])]);
        r.nets = vec![0, 1];
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        let coupling: f64 = n.couplings.iter().map(|c| c.pf).sum();
        // cc(0.65) = 2.8e-5, cc(0.45) = 4.75e-5 pF/um (linear rows).
        let want = 2.8e-5 * 9.0 + 4.75e-5 * 1.0 + 2.0 * 2.8e-5 * 0.05;
        assert!((coupling - want).abs() < 1e-15, "{coupling} {want}");
        assert!(n.couplings.iter().all(|c| c.a.0 != c.b.0));
        // Net 1's wire end extension is 0.45 um right of the patch, over 0.1 um
        // of the patch's right edge: each side counts half of cc(0.45).
        let mut r = routes(
            straight(0, 0, 10)
                .into_iter()
                .filter(|w| w.end[0] <= 5000)
                .collect(),
            vec![],
            vec![access(0, 0, [0, 0]), access(1, 0, [5000, 0])],
            vec![0, 1],
        );
        r.patches = vec![patch(0, [4000, 50, 5000, 250])];
        r.wires.push(Wire {
            net: 1,
            layer: 0,
            start: [5500, 150],
            end: [9000, 150],
        });
        r.access
            .extend([access(2, 1, [5500, 150]), access(3, 1, [9000, 150])]);
        let n = run(&r, &t, &[], &tech(false)).unwrap();
        let coupling: f64 = n.couplings.iter().map(|c| c.pf).sum();
        assert!((coupling - 4.75e-5 * 0.1).abs() < 1e-18, "{coupling}");
    }

    #[test]
    fn floating_or_unrouted_patches_fail_explicitly() {
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let floating = run(
            &patched(vec![patch(0, [4000, 2000, 5000, 2200])]),
            &t,
            &[],
            &tech(false),
        )
        .unwrap_err();
        assert!(
            floating.to_string().contains("floating routing patch"),
            "{floating}"
        );
        let unrouted = run(
            &patched(vec![patch(7, [4000, 50, 5000, 250])]),
            &t,
            &[],
            &tech(false),
        )
        .unwrap_err();
        assert!(
            unrouted.to_string().contains("without an RC tree"),
            "{unrouted}"
        );
        let mut on_cut = patch(0, [4000, 50, 5000, 250]);
        on_cut.layer = 2;
        assert!(run(&patched(vec![on_cut]), &t, &[], &tech(false)).is_err());
    }

    #[test]
    fn budgets_and_profile_mismatch_fail_explicitly() {
        let r = routes(
            straight(0, 0, 10),
            vec![],
            vec![
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
                    at: [10_000, 0],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let (l, v) = (layers(), vias());
        let tk = tech(false);
        let input = ExtractInput {
            held_bytes: 0,
            instances: None,
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            tech: &tk,
        };
        let tight = ExtractLimits {
            max_nodes: 1,
            ..Default::default()
        };
        assert!(matches!(
            extract_cpu_ref(&input, &tight),
            Err(Error::Budget(_))
        ));
        let neighbours = [
            PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: 1000,
                    ly: 250,
                    ux: 2000,
                    uy: 350,
                },
                owner: PassiveOwner::Ground,
            },
            PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: 5000,
                    ly: -350,
                    ux: 6000,
                    uy: -250,
                },
                owner: PassiveOwner::Ground,
            },
        ];
        let crowded = ExtractInput {
            held_bytes: 0,
            instances: None,
            passive: &neighbours,
            ..input
        };
        let tight = ExtractLimits {
            max_work: 4,
            ..Default::default()
        };
        assert!(matches!(
            extract_cpu_ref(&crowded, &tight),
            Err(Error::Budget(_))
        ));
        let tight = ExtractLimits {
            max_candidates: 1,
            ..Default::default()
        };
        assert!(matches!(
            extract_cpu_ref(&crowded, &tight),
            Err(Error::Budget(_))
        ));
        let ok = extract_cpu_ref(&crowded, &ExtractLimits::default()).unwrap();
        assert!(ok.stats.max_run_candidates > 1);
        // The candidate cap is configurable up to the scratch-derived ceiling.
        let ceiling = ExtractLimits {
            max_candidates: MAX_CANDIDATES_CEILING,
            ..Default::default()
        };
        assert!(ceiling.validate().is_ok());
        assert!(ceiling.candidate_scratch_bytes() <= CANDIDATE_SCRATCH_CEILING_BYTES);
        let over = ExtractLimits {
            max_candidates: MAX_CANDIDATES_CEILING + 1,
            ..Default::default()
        };
        assert!(over.validate().is_err());
        assert!(extract_cpu_ref(&crowded, &ceiling).is_ok());
        let zero = ExtractLimits {
            max_shapes: 0,
            ..Default::default()
        };
        assert!(extract_cpu_ref(&input, &zero).is_err());
        let mut wrong = tech(false);
        wrong.layers[0].as_mut().unwrap().width = 140;
        let input = ExtractInput {
            tech: &wrong,
            ..input
        };
        let error = extract_cpu_ref(&input, &ExtractLimits::default()).unwrap_err();
        assert!(error.to_string().contains("non-default widths"), "{error}");
        // A net with two drivers is rejected.
        let t2 = [terminal(0, 0, true), terminal(1, 0, true)];
        let input = ExtractInput {
            tech: &tk,
            terminals: &t2,
            ..input
        };
        assert!(extract_cpu_ref(&input, &ExtractLimits::default()).is_err());
    }

    #[test]
    fn parallel_budget_errors_and_results_match_the_serial_path() {
        // Three close routed conductors produce distinct couplings; displaced
        // passive edges also create interval boundaries without a direct cap.
        let r = routes(
            (0..3)
                .flat_map(|n| straight(n, i64::from(n) * 400, 10))
                .collect(),
            vec![],
            (0..3)
                .flat_map(|n| {
                    [
                        Access {
                            pin: 2 * n,
                            net: n,
                            layer: 0,
                            at: [0, i64::from(n) * 400],
                        },
                        Access {
                            pin: 2 * n + 1,
                            net: n,
                            layer: 0,
                            at: [10_000, i64::from(n) * 400],
                        },
                    ]
                })
                .collect(),
            vec![0, 1, 2],
        );
        let t: Vec<_> = (0..3)
            .flat_map(|n| [terminal(2 * n, n, true), terminal(2 * n + 1, n, false)])
            .collect();
        let passive = [PassiveShape {
            layer: 1,
            rect: Rect {
                lx: 1500,
                ly: -200,
                ux: 2300,
                uy: 1200,
            },
            owner: PassiveOwner::Ground,
        }];
        let (l, v, tk) = (layers(), vias(), tech(true));
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &passive,
            instances: None,
            held_bytes: 0,
            tech: &tk,
        };
        let limits = ExtractLimits::default();
        let baseline = serial(&input, &limits).unwrap();
        assert!(baseline.couplings.len() > 1);
        assert!(baseline.stats.max_run_candidates > 2);
        let work = baseline.stats.work;
        let mut variants = vec![
            limits.clone(),
            // The old query-worker flags are accepted and have no effect.
            ExtractLimits {
                query_workers: 16,
                query_batch_runs: 64,
                query_scratch_mib: 1,
                ..limits.clone()
            },
            ExtractLimits {
                max_candidates: 1,
                ..limits.clone()
            },
            ExtractLimits {
                max_candidates: 2,
                ..limits.clone()
            },
            ExtractLimits {
                max_couplings: 1,
                ..limits.clone()
            },
            ExtractLimits {
                max_couplings: 2,
                ..limits.clone()
            },
            ExtractLimits {
                max_elements: 1,
                ..limits.clone()
            },
        ];
        // Work caps across the whole run, including every unit boundary.
        for cap in [100, work / 4, work / 2, work - 1, work] {
            variants.push(ExtractLimits {
                max_work: cap,
                ..limits.clone()
            });
        }
        let mut errors = 0;
        for limits in variants {
            let a = serial(&input, &limits);
            for (threads, tile) in [(4, 0), (4, 1), (2, 7), (3, 64), (1, 3)] {
                for bounds in [parallel::Bounds::default(), tight_bounds()] {
                    let limits = ExtractLimits {
                        tile_units: tile,
                        ..limits.clone()
                    };
                    let b = pool(threads).install(|| {
                        extract_with_queries(&input, &limits, Queries::Indexed(bounds))
                    });
                    match (&a, b) {
                        (Ok(a), Ok(b)) => assert_network_exact(a, &b),
                        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                        (a, b) => panic!("serial/parallel outcome differs: {a:?} / {b:?}"),
                    }
                }
            }
            errors += usize::from(a.is_err());
        }
        // Candidate, coupling, element and work caps all fail somewhere.
        assert!(errors >= 8, "{errors} failing variants");
    }

    #[test]
    fn same_net_buckets_report_exactly_the_touching_ids() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: i64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as i64
        };
        let limits = ExtractLimits {
            max_candidates: MAX_CANDIDATES_CEILING,
            max_work: 1 << 40,
            ..Default::default()
        };
        // Lattice coordinates make exact edge and corner contacts common.
        let mut rect = |big: bool, far: bool| {
            let span = if big { 400 } else { 30 };
            let origin = if far { (1i64 << 60) - 10_000 } else { 0 };
            let (x, y) = (origin + 100 * (next(400) - 200), 100 * (next(400) - 200));
            Rect {
                lx: x,
                ly: y,
                ux: x + 100 * (1 + next(span)),
                uy: y + 100 * (1 + next(span)),
            }
        };
        for (cell, capacity) in [
            (1000, usize::MAX),
            (2500, usize::MAX),
            (1 << 40, usize::MAX),
            (700, 3000),
            (1 << 40, 50),
        ] {
            let mut bins = NetBins::new(cell, capacity);
            let mut rects: Vec<(u32, Rect)> = Vec::new();
            let mut complete = [true, true];
            let mut work = 0usize;
            for id in 0..1200u32 {
                let r = rect(id % 97 == 0, id % 211 == 0);
                let key = id % 2;
                let before = bins.entries.len();
                if bins.insert(key, id, r, &mut work, &limits).unwrap() {
                    rects.push((key, r));
                } else {
                    // Nothing is added when the admitted entries run out.
                    assert_eq!(bins.entries.len(), before);
                    complete[key as usize] = false;
                    rects.push((u32::MAX, r));
                }
            }
            assert!(bins.entries.len() <= capacity);
            let rect_of = |id: u32| rects[id as usize].1;
            let mut checked = 0;
            for q in 0..300 {
                let query = rect(q % 13 == 0, q % 17 == 0);
                for key in [0u32, 1] {
                    if !complete[key as usize] {
                        continue;
                    }
                    let mut found = Vec::new();
                    bins.touching(key, query, rect_of, &mut found, &mut work, &limits)
                        .unwrap();
                    let n = found.len();
                    found.sort_unstable();
                    found.dedup();
                    assert_eq!(found.len(), n, "an ID is reported once");
                    let expected: Vec<u32> = (0..rects.len() as u32)
                        .filter(|&id| {
                            let (k, r) = rects[id as usize];
                            k == key
                                && r.lx <= query.ux
                                && query.lx <= r.ux
                                && r.ly <= query.uy
                                && query.ly <= r.uy
                        })
                        .collect();
                    assert_eq!(found, expected, "cell {cell} key {key} query {query:?}");
                    checked += 1;
                }
            }
            // Unbounded buckets answer every query; tight ones run out.
            assert_eq!(checked == 600, capacity == usize::MAX, "{checked}");
        }
        // Candidate and work budgets fail like the list scan.
        let mut bins = NetBins::new(1000, usize::MAX);
        let mut work = 0;
        let big = Rect {
            lx: 0,
            ly: 0,
            ux: 5000,
            uy: 5000,
        };
        for id in 0..10 {
            bins.insert(0, id, big, &mut work, &limits).unwrap();
        }
        let tight = ExtractLimits {
            max_candidates: 9,
            ..limits.clone()
        };
        let err = bins
            .touching(0, big, |_| big, &mut Vec::new(), &mut 0, &tight)
            .unwrap_err();
        assert!(
            err.to_string().contains("candidates per metal piece"),
            "{err}"
        );
        let tight = ExtractLimits {
            max_work: 5,
            ..limits.clone()
        };
        let err = bins
            .touching(0, big, |_| big, &mut Vec::new(), &mut 0, &tight)
            .unwrap_err();
        assert!(err.to_string().contains("extraction work"), "{err}");
    }

    /// One net: a trunk with `n` same-layer stubs (every stub and trunk run
    /// is on the net's single M1 list).
    fn comb(n: i64) -> Routes {
        let (pitch, stub) = (2000, 1000);
        let mut wires = vec![Wire {
            net: 0,
            layer: 0,
            start: [0, 0],
            end: [n * pitch, 0],
        }];
        for i in 1..=n {
            let x = i * pitch - pitch / 2;
            wires.push(Wire {
                net: 0,
                layer: 0,
                start: [x, 0],
                end: [x, stub],
            });
        }
        routes(
            wires,
            vec![],
            vec![Access {
                pin: 0,
                net: 0,
                layer: 0,
                at: [0, 0],
            }],
            vec![0],
        )
    }

    #[test]
    fn a_giant_net_costs_its_local_density_not_its_length() {
        // 2,001 runs on one net and layer (an unbuffered clock or reset):
        // the linear scan's work grows with the square of the list, the
        // bucketed lookups' with its length, for the same network.
        let (l, v, tk) = (layers(), vias(), tech(false));
        let t = [terminal(0, 0, true)];
        let r = comb(1000);
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            instances: None,
            held_bytes: 0,
            tech: &tk,
        };
        let run = |max_work, net_bin_list| {
            pool(1).install(|| {
                extract_with_queries(
                    &input,
                    &ExtractLimits {
                        max_work,
                        ..Default::default()
                    },
                    Queries::Indexed(parallel::Bounds {
                        net_bin_list,
                        ..parallel::Bounds::default()
                    }),
                )
            })
        };
        let mut bucketed = run(1 << 40, NET_BIN_LIST).unwrap();
        let scanned = run(1 << 40, usize::MAX).unwrap();
        assert_eq!(bucketed.stats.wire_runs, 2001);
        assert!(
            bucketed.stats.work * 8 < scanned.stats.work,
            "{} vs {}",
            bucketed.stats.work,
            scanned.stats.work
        );
        let cap = bucketed.stats.work;
        bucketed.stats.work = scanned.stats.work;
        assert_network_exact(&scanned, &bucketed);
        run(cap, NET_BIN_LIST).unwrap();
        let err = run(cap, usize::MAX).unwrap_err();
        assert!(err.to_string().contains("extraction work"), "{err}");
    }

    #[test]
    fn retained_grid_coarsens_to_fit_the_entry_cap() {
        // Small squares and a long rail make base-cell entries exceed the
        // shape count; a cap between them coarsens the cells instead of
        // failing, and only a cap below the shape count fails.
        let mut wires = straight(0, 0, 10);
        wires.extend(straight(1, 600, 10));
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 1,
                    layer: 0,
                    at: [0, 600],
                },
            ],
            vec![0, 1],
        );
        let t = [terminal(0, 0, true), terminal(1, 1, true)];
        let mut passive: Vec<PassiveShape> = (0..400)
            .map(|i| PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: -200_000 + i * 1000,
                    ly: 20_000,
                    ux: -199_500 + i * 1000,
                    uy: 20_500,
                },
                owner: PassiveOwner::Ground,
            })
            .collect();
        passive.push(PassiveShape {
            layer: 0,
            rect: Rect {
                lx: -200_000,
                ly: 5000,
                ux: 200_000,
                uy: 5100,
            },
            owner: PassiveOwner::Ground,
        });
        let (l, v, tk) = (layers(), vias(), tech(false));
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &passive,
            instances: None,
            held_bytes: 0,
            tech: &tk,
        };
        let limits = ExtractLimits::default();
        let base = serial(&input, &limits).unwrap();
        let shapes = base.stats.shapes;
        let capped = ExtractLimits {
            max_bin_entries: shapes,
            ..limits.clone()
        };
        let mut coarse = serial(&input, &capped).unwrap();
        // Rejected cell sizes are charged: the cells did change.
        assert!(coarse.stats.work > base.stats.work);
        coarse.stats.work = base.stats.work;
        assert_network_exact(&base, &coarse);
        check_parallel(&input, &capped, &serial(&input, &capped).unwrap());
        let err = serial(
            &input,
            &ExtractLimits {
                max_bin_entries: shapes - 1,
                ..limits
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("spatial-bin entries"), "{err}");
    }

    #[test]
    fn a_pair_reports_its_x_strip_before_a_y_strip_error() {
        // The end piece at x = 10 um (a pair unit): its x strip couples to
        // two pins of net 1 above and below it (two new coupling keys); its
        // y strip sees the wire body and eight ground slivers to its right
        // (nine candidates). With max_candidates 8 and max_couplings 1 the
        // x strip's coupling budget fails first, as for separate strips.
        let mut wires = straight(0, 0, 10);
        wires.extend(straight(1, 100_000, 10));
        let r = routes(
            wires,
            vec![],
            vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: 5,
                    net: 1,
                    layer: 0,
                    at: [0, 100_000],
                },
                Access {
                    pin: 6,
                    net: 1,
                    layer: 0,
                    at: [10_000, 100_000],
                },
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(5, 1, true),
            terminal(6, 1, false),
        ];
        let mut passive: Vec<PassiveShape> = (0..8)
            .map(|i| PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: 10_100 + 20 * i,
                    ly: -50,
                    ux: 10_110 + 20 * i,
                    uy: 50,
                },
                owner: PassiveOwner::Ground,
            })
            .collect();
        for (pin, ly) in [(5, 300), (6, -400)] {
            passive.push(PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: 10_005,
                    ly,
                    ux: 10_045,
                    uy: ly + 100,
                },
                owner: PassiveOwner::Pin { net: 1, pin },
            });
        }
        let (l, v, tk) = (layers(), vias(), tech(false));
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &passive,
            instances: None,
            held_bytes: 0,
            tech: &tk,
        };
        let limits = ExtractLimits::default();
        let ok = serial(&input, &limits).unwrap();
        assert_eq!(ok.couplings.len(), 2);
        assert_eq!(ok.stats.max_run_candidates, 9);
        for (max_candidates, max_couplings, expected) in [
            (8, 1, "extraction coupling capacitors"),
            (8, 2, "extraction candidates per wire run"),
            (9, 1, "extraction coupling capacitors"),
        ] {
            let limits = ExtractLimits {
                max_candidates,
                max_couplings,
                ..limits.clone()
            };
            for (threads, tile) in [(1, 0), (1, 1), (2, 0), (3, 1), (4, 7)] {
                for bounds in [parallel::Bounds::default(), tight_bounds()] {
                    let limits = ExtractLimits {
                        tile_units: tile,
                        ..limits.clone()
                    };
                    let err = pool(threads)
                        .install(|| extract_with_queries(&input, &limits, Queries::Indexed(bounds)))
                        .unwrap_err();
                    assert!(err.to_string().contains(expected), "{err} vs {expected}");
                }
            }
        }
    }

    #[test]
    fn parallel_net_trees_fail_at_the_serial_net_and_checkpoint() {
        // Five nets of different sizes; budgets on graph and RC nodes fail at
        // different nets, and a later net lacks its pin access. The batch of
        // concurrently built trees must report the serial loop's first error.
        let sizes = [3i64, 9, 2, 6, 4];
        let wires: Vec<Wire> = sizes
            .iter()
            .enumerate()
            .flat_map(|(n, &k)| straight(n as u32, n as i64 * 600, k))
            .collect();
        let access_for = |skip: Option<u32>| -> Vec<Access> {
            sizes
                .iter()
                .enumerate()
                .filter(|&(n, _)| Some(n as u32) != skip)
                .flat_map(|(n, &k)| {
                    let n = n as u32;
                    [
                        Access {
                            pin: 2 * n,
                            net: n,
                            layer: 0,
                            at: [0, i64::from(n) * 600],
                        },
                        Access {
                            pin: 2 * n + 1,
                            net: n,
                            layer: 0,
                            at: [k * 1000, i64::from(n) * 600],
                        },
                    ]
                })
                .collect()
        };
        let t: Vec<_> = (0..sizes.len() as u32)
            .flat_map(|n| [terminal(2 * n, n, true), terminal(2 * n + 1, n, false)])
            .collect();
        let (l, v, tk) = (layers(), vias(), tech(false));
        let nets: Vec<u32> = (0..sizes.len() as u32).collect();
        let complete = routes(wires.clone(), vec![], access_for(None), nets.clone());
        let broken = routes(wires, vec![], access_for(Some(3)), nets);
        let mut errors = std::collections::BTreeSet::new();
        for r in [&complete, &broken] {
            let input = ExtractInput {
                layers: &l,
                vias: &v,
                routes: r,
                nets: None,
                terminals: &t,
                passive: &[],
                instances: None,
                held_bytes: 0,
                tech: &tk,
            };
            let nodes = serial(
                &complete_input(&input, &complete),
                &ExtractLimits::default(),
            )
            .unwrap()
            .stats
            .nodes;
            for max_nodes in [1, 2, 3, 5, nodes / 3, nodes / 2, nodes - 1, nodes] {
                let limits = ExtractLimits {
                    max_nodes,
                    ..Default::default()
                };
                let a = serial(&input, &limits);
                for threads in [2, 3, 4] {
                    let b = pool(threads).install(|| extract_cpu_ref(&input, &limits));
                    match (&a, b) {
                        (Ok(a), Ok(b)) => assert_network_exact(a, &b),
                        (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                        (a, b) => panic!("serial/parallel outcome differs: {a:?} / {b:?}"),
                    }
                }
                if let Err(e) = a {
                    errors.insert(e.to_string());
                }
            }
        }
        // Graph-node, RC-node and missing-access errors all occur.
        assert!(
            errors.iter().any(|e| e.contains("graph nodes")),
            "{errors:?}"
        );
        assert!(errors.iter().any(|e| e.contains("RC nodes")), "{errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("pin access")),
            "{errors:?}"
        );
    }
    /// The same input over another routing table.
    fn complete_input<'a>(input: &ExtractInput<'a>, routes: &'a Routes) -> ExtractInput<'a> {
        ExtractInput { routes, ..*input }
    }

    #[test]
    fn disjoint_neighbours_use_bounded_active_interval_work() {
        let r = routes(
            vec![Wire {
                net: 0,
                layer: 0,
                start: [0, 0],
                end: [1_000_000, 0],
            }],
            vec![],
            vec![access(0, 0, [0, 0]), access(1, 0, [1_000_000, 0])],
            vec![0],
        );
        let terminals = [terminal(0, 0, true), terminal(1, 0, false)];
        let passive: Vec<_> = (0..1000)
            .map(|i| PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: i * 1000 + 100,
                    ux: i * 1000 + 200,
                    ly: 250,
                    uy: 350,
                },
                owner: PassiveOwner::Ground,
            })
            .collect();
        let (layers, vias, tech) = (layers(), vias(), tech(false));
        let input = ExtractInput {
            held_bytes: 0,
            instances: None,
            layers: &layers,
            vias: &vias,
            routes: &r,
            nets: None,
            terminals: &terminals,
            passive: &passive,
            tech: &tech,
        };
        let limits = ExtractLimits {
            max_work: 200_000,
            max_segment_length: 2_000_000,
            ..Default::default()
        };
        let network =
            extract_cpu_ref(&input, &limits).expect("disjoint local neighbours stay linear");
        let capacitance: f64 = network.nets[0].ground.iter().sum();
        // 900 um isolated (two 4e-5 sides), plus100 um with one grounded
        // neighbour at0.2 um (8.5e-5 coupling +1.5e-5 ground on that side).
        let expected = 900.0 * 8e-5 + 100.0 * 14e-5 + 1.6e-5;
        assert!(
            (capacitance - expected).abs() < 1e-12,
            "{capacitance} versus {expected}"
        );
    }
    #[test]
    fn shared_passive_geometry_matches_explicit_crossings_and_pin_coupling() {
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        let r = routes(
            [straight(0, 0, 10), straight(1, 1000, 10)].concat(),
            vec![],
            vec![
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
                    at: [10000, 0],
                },
                Access {
                    pin: 2,
                    net: 1,
                    layer: 0,
                    at: [0, 1000],
                },
                Access {
                    pin: 3,
                    net: 1,
                    layer: 0,
                    at: [10000, 1000],
                },
            ],
            vec![0, 1],
        );
        let t = [
            terminal(0, 0, true),
            terminal(1, 0, false),
            terminal(2, 1, true),
            terminal(3, 1, false),
        ];
        // Deliberately list M2 before M1 to exercise original-template order.
        let shapes = vec![
            TemplateObstacle {
                layer: 1,
                rect: Rect {
                    lx: 500,
                    ly: -500,
                    ux: 900,
                    uy: 1500,
                },
                pin: None,
            },
            TemplateObstacle {
                layer: 0,
                rect: Rect {
                    lx: -100,
                    ly: 900,
                    ux: 100,
                    uy: 1100,
                },
                pin: Some(0),
            },
        ];
        let source = InstanceObstacles::new(
            vec![ObstacleTemplate::new(2000, 1, shapes.clone()).unwrap()],
            vec![ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [0, 0],
                orientation: crate::Orientation::N,
            }],
            vec![1],
        )
        .unwrap()
        .with_pin_ids(vec![2])
        .unwrap();
        let explicit = vec![
            PassiveShape {
                layer: shapes[0].layer,
                rect: shapes[0].rect,
                owner: PassiveOwner::Ground,
            },
            PassiveShape {
                layer: shapes[1].layer,
                rect: shapes[1].rect,
                owner: PassiveOwner::Pin { net: 1, pin: 2 },
            },
        ];
        let (l, v, tech) = (layers(), vias(), tech(true));
        let a = run(&r, &t, &explicit, &tech).unwrap();
        let b = extract_cpu_ref(
            &ExtractInput {
                layers: &l,
                vias: &v,
                routes: &r,
                nets: None,
                terminals: &t,
                passive: &[],
                held_bytes: 0,
                instances: Some(InstancedPassive {
                    geometry: &source,
                    signal_nets: &[true, true],
                }),
                tech: &tech,
            },
            &ExtractLimits::default(),
        )
        .unwrap();
        check_parallel(
            &ExtractInput {
                layers: &l,
                vias: &v,
                routes: &r,
                nets: None,
                terminals: &t,
                passive: &[],
                held_bytes: 0,
                instances: Some(InstancedPassive {
                    geometry: &source,
                    signal_nets: &[true, true],
                }),
                tech: &tech,
            },
            &ExtractLimits::default(),
            &b,
        );
        assert_eq!(a.nets.len(), b.nets.len());
        for (a, b) in a.nets.iter().zip(&b.nets) {
            assert_eq!(a.nodes, b.nodes);
            assert_eq!(a.resistors, b.resistors);
            for (x, y) in a.ground.iter().zip(&b.ground) {
                assert!((x - y).abs() < 1e-14, "{x} != {y}");
            }
        }
        assert_eq!(a.stats.passive_strips, b.stats.passive_strips);
        assert_eq!(a.couplings.len(), b.couplings.len());
        for (a, b) in a.couplings.iter().zip(&b.couplings) {
            assert_eq!((a.a, a.b), (b.a, b.b));
            assert!((a.pf - b.pf).abs() < 1e-14);
        }
    }
    #[test]
    fn millions_of_virtual_conductors_do_not_consume_retained_shape_budget() {
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        let crossed = TemplateObstacle {
            layer: 1,
            rect: Rect {
                lx: 500,
                ly: -500,
                ux: 900,
                uy: 500,
            },
            pin: None,
        };
        let far = TemplateObstacle {
            layer: 0,
            rect: Rect {
                lx: 0,
                ly: 0,
                ux: 500,
                uy: 500,
            },
            pin: None,
        };
        let mut cells = vec![ObstacleInstance {
            template: 0,
            pin_start: 0,
            at: [0, 0],
            orientation: crate::Orientation::N,
        }];
        cells.extend((0..100_000).map(|i| ObstacleInstance {
            template: 1,
            pin_start: 0,
            at: [1_000_000 + (i % 500) * 1000, 1_000_000 + (i / 500) * 1000],
            orientation: crate::Orientation::N,
        }));
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(1000, 0, vec![crossed.clone()]).unwrap(),
                ObstacleTemplate::new(1000, 0, vec![far; 102]).unwrap(),
            ],
            cells,
            vec![],
        )
        .unwrap()
        .with_pin_ids(vec![])
        .unwrap();
        assert_eq!(source.virtual_shapes(), 10_200_001);
        let r = routes(
            straight(0, 0, 10),
            vec![],
            vec![
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
                    at: [10000, 0],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let (l, v, tech) = (layers(), vias(), tech(true));
        let expected = run(
            &r,
            &t,
            &[PassiveShape {
                layer: crossed.layer,
                rect: crossed.rect,
                owner: PassiveOwner::Ground,
            }],
            &tech,
        )
        .unwrap();
        let limits = ExtractLimits {
            max_shapes: 32,
            max_virtual_shapes: 20_000_000,
            ..Default::default()
        };
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            held_bytes: 0,
            instances: Some(InstancedPassive {
                geometry: &source,
                signal_nets: &[true],
            }),
            tech: &tech,
        };
        let actual = extract_cpu_ref(&input, &limits).unwrap();
        assert_eq!(actual.nets[0].ground, expected.nets[0].ground);
        assert_eq!(actual.nets[0].resistors, expected.nets[0].resistors);
        assert_eq!(actual.stats.passive_strips, expected.stats.passive_strips);
        let limited = ExtractLimits {
            max_virtual_shapes: 10,
            ..limits
        };
        assert!(matches!(
            extract_cpu_ref(&input, &limited),
            Err(Error::Budget(_))
        ));
    }
    #[test]
    fn extraction_memory_allowance_fails_before_graph_construction() {
        let r = routes(
            straight(0, 0, 10),
            vec![],
            vec![
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
                    at: [10000, 0],
                },
            ],
            vec![0],
        );
        let t = [terminal(0, 0, true), terminal(1, 0, false)];
        let (l, v, tech) = (layers(), vias(), tech(true));
        let input = ExtractInput {
            layers: &l,
            vias: &v,
            routes: &r,
            nets: None,
            terminals: &t,
            passive: &[],
            instances: None,
            held_bytes: 0,
            tech: &tech,
        };
        let limits = ExtractLimits {
            memory_mib: 1,
            ..Default::default()
        };
        assert!(matches!(
            extract_cpu_ref(&input, &limits),
            Err(Error::Budget(_))
        ));
        let reserved = ExtractInput {
            held_bytes: 1 << 40,
            ..input
        };
        assert!(matches!(
            extract_cpu_ref(&reserved, &ExtractLimits::default()),
            Err(Error::Budget(_))
        ));
    }
}
