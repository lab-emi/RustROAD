//! Bounded per-net RC-tree extraction compute view (`cpu_ref`).
//!
//! Inputs are the router's checked lattice records (wires, fixed vias, pin
//! access points) plus every other conductor shape on the routing layers. The
//! geometry stays integer DBU; resistance and capacitance are f64 (ohm, pF).
//! Nothing here enumerates paths: each net is one graph walk, each straight
//! wire run is one resistor, and neighbour search is a bounded binned query.
//! Model, budgets and the memory estimate are in docs/extraction.md.
use crate::routing::{RouteLayer, RouteVia, Routes, Terminal};
use crate::{Error, Rect, Result};
use std::collections::{BTreeMap, HashMap};
mod bins;
mod geometry;
mod stripes;
use geometry::{InstanceGeometry, Shapes};
pub use stripes::{
    PartitionStats, StripePlan, StripeStats, even_cuts, extract_partitioned, minimum_halo,
};

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
    /// Bin-entry scans plus candidate-by-interval comparisons, whole design.
    pub max_work: usize,
    /// Candidate shapes considered for one wire run. Its scratch memory is
    /// `CANDIDATE_SCRATCH_BYTES` per candidate (the per-run candidate list,
    /// the interval cut points and the plane list); the ceiling keeps that
    /// below `CANDIDATE_SCRATCH_CEILING_BYTES`. Work (intervals x candidates)
    /// is charged to `max_work`.
    pub max_candidates: usize,
    /// Spatial-bin entries.
    pub max_bin_entries: usize,
    /// Straight runs longer than this are split at existing lattice nodes (DBU).
    pub max_segment_length: i64,
    /// Strips of passive conductors (supply rails and straps, pins,
    /// obstructions) longer than this are evaluated in equal pieces (DBU);
    /// `i64::MAX` keeps whole strips. A passive strip only counts plate
    /// shares toward routed conductors, so pieces change no extracted value
    /// beyond floating-point summation order; with pieces, a strip whose
    /// cross-section is too wide for any plate is skipped.
    pub max_passive_strip_length: i64,
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
            max_bin_entries: 8_000_000,
            max_segment_length: 25_000,
            max_passive_strip_length: i64::MAX,
        }
    }
}
/// Scratch bytes per wire-run candidate: the candidate id (u32), two clamped
/// cut points (2 x i64) and at most one plane entry (u32, i64, i64, u32).
pub const CANDIDATE_SCRATCH_BYTES: usize = 4 + 16 + 24;
/// Rule-9 ceiling of the per-run candidate scratch (one run at a time).
pub const CANDIDATE_SCRATCH_CEILING_BYTES: usize = 48 << 20;
/// Absolute ceiling of `max_candidates` derived from the scratch ceiling
/// (1,143,901 candidates; 48 MiB).
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
            (self.max_bin_entries, 512_000_000),
        ] {
            if value == 0 || value > ceiling {
                return Err(budget("extraction limit outside hard bounds"));
            }
        }
        if self.memory_mib == 0 || self.memory_mib > 65_536 {
            return Err(budget("extraction memory allowance"));
        }
        if self.max_segment_length <= 0 || self.max_passive_strip_length <= 0 {
            return Err(invalid("extraction segment length must be positive"));
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
    /// Stripes of `extract_partitioned` (absent for a single extraction);
    /// `shapes` and `max_run_candidates` are then the largest stripe's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition: Option<PartitionStats>,
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

fn populated_bin_bound(input: &ExtractInput<'_>, limits: &ExtractLimits, bin: i64) -> Result<u64> {
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
    for wire in &input.routes.wires {
        let layer = input
            .layers
            .get(wire.layer as usize)
            .ok_or_else(|| invalid("extraction wire layer"))?;
        include(wire.layer, crate::routing::wire_rect(wire, layer.width)?)?;
    }
    for via in &input.routes.vias {
        let kind = input
            .vias
            .get(via.kind as usize)
            .ok_or_else(|| invalid("extraction via kind"))?;
        for shape in &kind.shapes {
            include(shape.layer, crate::routing::shifted(shape.rect, via.at)?)?;
        }
    }
    for patch in &input.routes.patches {
        include(patch.layer, patch.rect)?;
    }
    for shape in input.passive {
        include(shape.layer, shape.rect)?;
    }
    Ok(bin_count(&bounds, bin, limits.max_bin_entries))
}

fn memory_estimate(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<ExtractMemory> {
    let live = crate::memory::live_bytes().unwrap_or(input.held_bytes);
    let held = live.max(input.held_bytes);
    let held = held.saturating_add(held.div_ceil(4));
    let bin = extraction_bin(input)?;
    let populated = populated_bin_bound(input, limits, bin)?;
    let working = (limits.max_nodes as u64)
        .saturating_mul(64)
        .saturating_add((limits.max_elements as u64).saturating_mul(16))
        .saturating_add((limits.max_shapes as u64).saturating_mul(320))
        .saturating_add((limits.max_couplings as u64).saturating_mul(128))
        // Linked entries include old+new vector storage during reallocation;
        // fixed headers include hash-table occupancy and rehash headroom.
        .saturating_add((limits.max_bin_entries as u64).saturating_mul(24))
        .saturating_add(populated.saturating_mul(96))
        .saturating_add((input.routes.nets.len() as u64).saturating_mul(192))
        .saturating_add(limits.candidate_scratch_bytes() as u64);
    let geometry =
        InstanceGeometry::estimated_bytes(input.instances, bin, input.layers.len(), limits)?;
    let memory = ExtractMemory {
        held_bytes: held,
        working_bytes: working,
        geometry_bytes: geometry,
        estimate_bytes: held.saturating_add(working).saturating_add(geometry),
        limit_bytes: limits.memory_mib.saturating_mul(1 << 20),
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
/// Calibration context `(over, under)` to its index in `RcLayer::contexts`.
type ContextIndex = HashMap<(u32, Option<u32>), usize>;
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
    cross: i64,
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
/// Owner of a conductor shape. Every routed-net shape (wire body or piece) is
/// a `Run` strip that models its own exposed sides.
#[derive(Clone, Copy, Debug)]
enum Owner {
    Run(u32),
    Passive(PassiveOwner),
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
            Owner::Run(_) => None,
            Owner::Passive(PassiveOwner::Pin { net, .. }) => Some(net),
            Owner::Passive(PassiveOwner::Ground) => None,
        }
    }
}
fn shape_net(runs: &[Run], s: &Shape) -> Option<u32> {
    match s.owner {
        Owner::Run(o) => Some(runs[o as usize].net),
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
    /// Per net: its driver access point is in the region's core (always
    /// outside a region). Only home nets count in the net statistics.
    home: Vec<bool>,
    runs: Vec<Run>,
    pads: Vec<Pad>,
    /// Pin index -> (net, local node).
    pin_nodes: HashMap<u32, (u32, u32)>,
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

/// Build per-net graphs, straight runs, the tree order and resistors. In a
/// region, the statistics count only nets whose driver access point lies in
/// its core; the budgets count every net.
fn build_trees(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    region: Option<&ExtractRegion<'_>>,
) -> Result<(Built, ExtractStats)> {
    let routes = input.routes;
    let mut stats = ExtractStats::default();
    let mut all_nodes = 0usize;
    let terminals: HashMap<u32, &Terminal> = input.terminals.iter().map(|t| (t.pin, t)).collect();
    let mut wires = BTreeMap::<u32, Vec<usize>>::new();
    let mut vias = BTreeMap::<u32, Vec<usize>>::new();
    let mut access = BTreeMap::<u32, Vec<usize>>::new();
    for (i, w) in routes.wires.iter().enumerate() {
        wires.entry(w.net).or_default().push(i);
    }
    for (i, v) in routes.vias.iter().enumerate() {
        if v.kind as usize >= input.vias.len() {
            return Err(invalid("via kind outside routing input"));
        }
        vias.entry(v.net).or_default().push(i);
    }
    for (i, a) in routes.access.iter().enumerate() {
        access.entry(a.net).or_default().push(i);
    }
    let mut out = Built {
        nets: Vec::new(),
        home: Vec::new(),
        runs: Vec::new(),
        pads: Vec::new(),
        pin_nodes: HashMap::new(),
    };
    for (net_index, &net) in routes.nets.iter().enumerate() {
        if net_index.is_multiple_of(4096) {
            guard_memory(limits)?;
        }
        let snapshot = region.map(|_| stats.clone());
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
        if all_nodes + keys.len() > limits.max_nodes.saturating_mul(4) {
            return Err(budget("extraction graph nodes"));
        }
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
        stats.duplicate_wire_edges += before - edges.len();
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
        let mut driver_at = [0, 0];
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
                driver_at = a.at;
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
                        x != from_edge
                            && matches!(edges[x as usize].2, EdgeKind::Wire(l) if l == layer)
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
        stats.segment_cut_nodes += virtual_keys.len();
        let point = |g: u32| -> Key {
            if (g as usize) < n {
                keys[g as usize]
            } else {
                virtual_keys[g as usize - n]
            }
        };
        let mut elems: Vec<(u32, u32, f64)> = Vec::new();
        for &(a, b, layer) in &run_ends {
            let m = layer_model(input, layer)?;
            let (ka, kb) = (point(a), point(b));
            let length = (ka.1 - kb.1).abs() + (ka.2 - kb.2).abs();
            let ohm = m.sheet_ohm * um(input.tech, length) / um(input.tech, m.width);
            elems.push((a, b, ohm));
            stats.wire_length_dbu = stats
                .wire_length_dbu
                .checked_add(length as u64)
                .ok_or_else(|| budget("wire length"))?;
        }
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
            stats.coincident_pin_links += 1;
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
                    stats.opened_loop_resistors += 1;
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
        for (i, node) in nodes.iter().enumerate() {
            if let RcNode::Pin(pin) = node {
                out.pin_nodes.insert(*pin, (net, i as u32));
            }
        }
        all_nodes += nodes.len();
        if all_nodes > limits.max_nodes {
            return Err(budget("extraction RC nodes"));
        }
        stats.nodes += nodes.len();
        stats.resistors += resistors.len();
        stats.resistance_ohm += resistors.iter().map(|r| r.2).sum::<f64>();
        // Straight runs with tree-order end nodes.
        for &(a, b, layer) in &run_ends {
            let (ka, kb) = (point(a), point(b));
            let vertical = ka.1 == kb.1;
            let (pa, pb) = if vertical { (ka.2, kb.2) } else { (ka.1, kb.1) };
            let (lo_node, hi_node) = if pa <= pb { (a, b) } else { (b, a) };
            let cross = if vertical { ka.1 } else { ka.2 };
            let half = input.layers[layer as usize].width / 2;
            out.runs.push(Run {
                net,
                layer,
                vertical,
                cross,
                lo: pa.min(pb),
                hi: pa.max(pb),
                minus: cross - half,
                plus: cross + half,
                shape: out.runs.len() as u32,
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
                out.pads.push(Pad {
                    net,
                    layer: s.layer,
                    rect: crate::routing::shifted(s.rect, v.at)?,
                    node: local[g as usize],
                });
            }
        }
        stats.lattice_wires += net_wires.len();
        stats.vias += net_vias.len();
        let home = region.is_none_or(|g| contains_point(&g.core, driver_at));
        if !home && let Some(before) = snapshot {
            stats = before;
        }
        out.home.push(home);
        out.nets.push(RcNet {
            net,
            ground: vec![0.0; nodes.len()],
            nodes,
            resistors,
        });
    }
    stats.nets = out.home.iter().filter(|&&h| h).count();
    Ok((out, stats))
}

struct Bins<'a> {
    size: i64,
    map: bins::BinLists,
    instances: Option<InstanceGeometry<'a>>,
}
impl Bins<'_> {
    fn range(&self, lo: i64, hi: i64) -> (i64, i64) {
        (lo.div_euclid(self.size), hi.div_euclid(self.size))
    }
    fn visit(
        &mut self,
        layer: u32,
        query: Rect,
        shapes: &Shapes<'_>,
        stats: &mut ExtractStats,
        limits: &ExtractLimits,
        mut visitor: impl FnMut(u32, Shape) -> Result<()>,
    ) -> Result<()> {
        let (x0, x1) = self.range(query.lx, query.ux);
        let (y0, y1) = self.range(query.ly, query.uy);
        let cells = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if cells > limits.max_bin_entries as i128 {
            return Err(budget("extraction query extent"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                let list = self.map.iter((layer, x, y));
                stats.work = stats
                    .work
                    .checked_add(list.len().max(1))
                    .filter(|&n| n <= limits.max_work)
                    .ok_or_else(|| budget("extraction work"))?;
                for id in list {
                    visitor(id, shapes.get(id))?;
                }
            }
        }
        if let Some(instances) = &mut self.instances {
            instances.visit(shapes, layer, query, &mut stats.work, |id, shape| {
                visitor(id, shape)?;
                Ok(true)
            })?;
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
/// in shape-index order. Bin scans are charged to the work budget.
#[allow(clippy::too_many_arguments)]
fn same_net_near(
    r: Rect,
    net: u32,
    layer: u32,
    built: &Built,
    shapes: &Shapes<'_>,
    bins: &mut Bins<'_>,
    limits: &ExtractLimits,
    stats: &mut ExtractStats,
) -> Result<Vec<u32>> {
    let mut found = std::collections::BTreeSet::new();
    bins.visit(layer, r, shapes, stats, limits, |id, shape| {
        let q = shape.rect;
        if shape_net(&built.runs, &shape) == Some(net)
            && q.lx <= r.ux
            && r.lx <= q.ux
            && q.ly <= r.uy
            && r.ly <= q.uy
        {
            found.insert(id);
            if found.len() > limits.max_candidates {
                return Err(budget("extraction candidates per metal piece"));
            }
        }
        Ok(())
    })?;
    let near = found.into_iter().collect();
    Ok(near)
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

/// Add one rectangle piece as a conductor shape with two side strips (along x
/// and along y), all of its capacitance at `node`. Pieces add capacitance only:
/// never a node or a resistor.
#[allow(clippy::too_many_arguments)]
fn add_piece(
    piece: Rect,
    net: u32,
    layer: u32,
    node: u32,
    built: &mut Built,
    shapes: &mut Shapes<'_>,
    bins: &mut Bins<'_>,
    entries: &mut usize,
    limits: &ExtractLimits,
) -> Result<()> {
    let shape = shapes.len() as u32;
    let first = built.runs.len() as u32;
    for vertical in [false, true] {
        let (lo, hi, minus, plus) = if vertical {
            (piece.ly, piece.uy, piece.lx, piece.ux)
        } else {
            (piece.lx, piece.ux, piece.ly, piece.uy)
        };
        built.runs.push(Run {
            net,
            layer,
            vertical,
            cross: minus + (plus - minus) / 2,
            lo,
            hi,
            minus,
            plus,
            shape,
            piece: true,
            passive: None,
            node_lo: node,
            node_hi: node,
        });
    }
    shapes.push(Shape {
        layer,
        rect: piece,
        owner: Owner::Run(first),
    });
    if shapes.retained_len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    let (x0, x1) = bins.range(piece.lx, piece.ux);
    let (y0, y1) = bins.range(piece.ly, piece.uy);
    let cells = usize::try_from((x1 - x0 + 1) * (y1 - y0 + 1)).map_err(|_| budget("bin span"))?;
    *entries = entries
        .checked_add(cells)
        .ok_or_else(|| budget("bin entries"))?;
    if *entries > limits.max_bin_entries {
        return Err(budget("extraction spatial-bin entries"));
    }
    for bx in x0..=x1 {
        for by in y0..=y1 {
            bins.map
                .push((layer, bx, by), shape, limits.max_bin_entries)?;
        }
    }
    Ok(())
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
    bins: &mut Bins<'_>,
    entries: &mut usize,
    net_index: &HashMap<u32, usize>,
    stats: &mut ExtractStats,
) -> Result<()> {
    // (net, layer, rect, node, is_pad) in a fixed order.
    let mut candidates: Vec<(u32, u32, Rect, u32, bool)> = Vec::new();
    for r in built.runs.iter().filter(|r| !r.piece) {
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
        let covering = same_net_near(rect, net, layer, built, shapes, bins, limits, stats)?;
        let pieces = uncovered(rect, &covering, shapes)?;
        if built.home[net_index[&net]] {
            if is_pad {
                stats.pad_pieces += pieces.len();
            } else {
                stats.end_pieces += pieces.len();
            }
        }
        for piece in pieces {
            add_piece(
                piece, net, layer, node, built, shapes, bins, entries, limits,
            )?;
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
    bins: &mut Bins<'_>,
    entries: &mut usize,
    net_index: &HashMap<u32, usize>,
    stats: &mut ExtractStats,
) -> Result<()> {
    for patch in &input.routes.patches {
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
        let touching = same_net_near(
            r,
            patch.net,
            patch.layer,
            built,
            shapes,
            bins,
            limits,
            stats,
        )?;
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
            Owner::Run(o) => {
                let run = &built.runs[o as usize];
                if run.piece {
                    run.node_lo
                } else {
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
        if built.home[net_index[&patch.net]] {
            stats.patches += 1;
            stats.patch_pieces += pieces.len();
        }
        for piece in pieces {
            add_piece(
                piece,
                patch.net,
                patch.layer,
                node,
                built,
                shapes,
                bins,
                entries,
                limits,
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
            [run.cross as f64, run.lo as f64],
            [run.cross as f64, run.hi as f64],
        ]
    } else {
        [
            [run.lo as f64, run.cross as f64],
            [run.hi as f64, run.cross as f64],
        ]
    };
    let d = |p: [f64; 2]| (p[0] - point[0]).abs() + (p[1] - point[1]).abs();
    if d(ends[1]) < d(ends[0]) {
        run.node_hi
    } else {
        run.node_lo
    }
}

/// `(metal, covered width, [(shape, width)])` of one layer over a strip face.
type FacePortion = (u32, i64, Vec<(u32, i64)>);

/// Coverage of a strip face `minus..plus` by the metal of other layers below
/// (`upper == false`) or above, nearest layer first; a nearer layer shields
/// the farther ones. Each entry is `(metal, covered width, [(shape, width)])`
/// in exact integer DBU. `layered` is sorted by metal, then cross extent.
fn face_portions(
    layered: &[(u32, i64, i64, u32)],
    own: u32,
    upper: bool,
    minus: i64,
    plus: i64,
) -> Vec<FacePortion> {
    let mut metals: Vec<u32> = layered
        .iter()
        .map(|e| e.0)
        .filter(|&m| if upper { m > own } else { m < own })
        .collect();
    metals.dedup();
    if !upper {
        metals.reverse();
    }
    let mut remaining: Vec<(i64, i64)> = vec![(minus, plus)];
    let mut out = Vec::new();
    for metal in metals {
        let mut owners: Vec<(u32, i64)> = Vec::new();
        let mut covered = 0;
        for e in layered.iter().filter(|e| e.0 == metal) {
            let mut next = Vec::with_capacity(remaining.len() + 1);
            let mut width = 0;
            for &(r0, r1) in &remaining {
                let (c0, c1) = (r0.max(e.1), r1.min(e.2));
                if c1 > c0 {
                    width += c1 - c0;
                    if r0 < c0 {
                        next.push((r0, c0));
                    }
                    if c1 < r1 {
                        next.push((c1, r1));
                    }
                } else {
                    next.push((r0, r1));
                }
            }
            remaining = next;
            if width > 0 {
                owners.push((e.3, width));
                covered += width;
            }
        }
        if covered > 0 {
            out.push((metal, covered, owners));
        }
        if remaining.is_empty() {
            break;
        }
    }
    out
}

/// Whether a routed conductor on another routing layer overlaps `r`.
fn crossed_by_route(
    routing: &[u32],
    layer: u32,
    r: Rect,
    shapes: &Shapes<'_>,
    bins: &Bins<'_>,
    stats: &mut ExtractStats,
    limits: &ExtractLimits,
) -> Result<bool> {
    let (x0, x1) = bins.range(r.lx, r.ux);
    let (y0, y1) = bins.range(r.ly, r.uy);
    for &other in routing.iter().filter(|&&l| l != layer) {
        for bx in x0..=x1 {
            for by in y0..=y1 {
                let list = bins.map.iter((other, bx, by));
                if list.len() == 0 {
                    continue;
                }
                stats.work = stats
                    .work
                    .checked_add(list.len())
                    .ok_or_else(|| budget("extraction work"))?;
                if stats.work > limits.max_work {
                    return Err(budget("extraction work"));
                }
                for o in list {
                    let q = &shapes.get(o);
                    if matches!(q.owner, Owner::Run(_))
                        && q.rect.lx < r.ux
                        && r.lx < q.rect.ux
                        && q.rect.ly < r.uy
                        && r.ly < q.rect.uy
                    {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Appends `whole` (a passive conductor strip) cut into equal pieces of at
/// most `piece` DBU along its axis, at exact integer coordinates. With
/// `span` (the extent of all metal along x and y), a strip whose
/// cross-section cannot have a plate is dropped: a plate covers the strip
/// and one strip width beyond both side edges (`plate`), three widths that
/// no metal spans. A passive strip only counts plate shares, so neither the
/// pieces nor the drop change an extracted value beyond summation order.
fn passive_pieces(whole: Run, piece: i64, span: Option<[i64; 2]>, out: &mut Vec<Run>) {
    if let Some(span) = span {
        let across = if whole.vertical { span[0] } else { span[1] };
        if (whole.plus - whole.minus).saturating_mul(3) > across {
            return;
        }
    }
    let length = whole.hi - whole.lo;
    let count = if length > piece {
        (length - 1) / piece + 1
    } else {
        1
    };
    let at = |i: i64| whole.lo + (i128::from(length) * i128::from(i) / i128::from(count)) as i64;
    for i in 0..count {
        out.push(Run {
            lo: at(i),
            hi: at(i + 1),
            ..whole
        });
    }
}

/// Extent along x and y of all conductor metal of an input: routed wires
/// (with their end extensions), via shapes, patches, passive shapes and the
/// instance geometry. No plate is wider.
pub fn metal_extent(input: &ExtractInput<'_>) -> Result<[i64; 2]> {
    let mut bounds: Option<Rect> = None;
    let mut include = |r: Rect| {
        bounds = Some(bounds.map_or(r, |b| Rect {
            lx: b.lx.min(r.lx),
            ly: b.ly.min(r.ly),
            ux: b.ux.max(r.ux),
            uy: b.uy.max(r.uy),
        }));
    };
    for w in &input.routes.wires {
        let layer = input
            .layers
            .get(w.layer as usize)
            .ok_or_else(|| invalid("extraction wire layer"))?;
        include(crate::routing::wire_rect(w, layer.width)?);
    }
    for v in &input.routes.vias {
        let kind = input
            .vias
            .get(v.kind as usize)
            .ok_or_else(|| invalid("extraction via kind"))?;
        for shape in &kind.shapes {
            include(crate::routing::shifted(shape.rect, v.at)?);
        }
    }
    for p in &input.routes.patches {
        include(p.rect);
    }
    for p in input.passive {
        include(p.rect);
    }
    if let Some(instances) = input.instances {
        for i in 0..instances.geometry.len() {
            if let Some(r) = instances.geometry.bounds(i)? {
                include(r);
            }
        }
    }
    let Some(b) = bounds else {
        return Ok([0, 0]);
    };
    let extent = |lo: i64, hi: i64| {
        hi.checked_sub(lo)
            .ok_or_else(|| budget("extraction metal extent"))
    };
    Ok([extent(b.lx, b.ux)?, extent(b.ly, b.uy)?])
}

/// Passive conductors (pins, obstructions, supply metal) that a routed
/// conductor on another routing layer overlaps get two strips (along x and
/// along y, like a piece) so that their own cross-section is counted where a
/// routed conductor is their calibration plate: a crossing is counted from
/// both conductors' cross-sections whoever owns them. With a passive piece
/// length, each strip is cut into pieces (`passive_pieces`) and only pieces
/// that a routed conductor overlaps are kept; in a region, only pieces
/// anchored in its core.
#[allow(clippy::too_many_arguments)]
fn add_passive_strips(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    built: &mut Built,
    shapes: &Shapes<'_>,
    bins: &mut Bins<'_>,
    stats: &mut ExtractStats,
    region: Option<&ExtractRegion<'_>>,
    span: Option<[i64; 2]>,
) -> Result<()> {
    let routing: Vec<u32> = (0..input.layers.len() as u32)
        .filter(|&l| input.tech.layers[l as usize].is_some())
        .collect();
    let piece = limits.max_passive_strip_length;
    let mut strips = Vec::new();
    for (si, sh) in shapes.owned() {
        let Owner::Passive(owner) = sh.owner else {
            continue;
        };
        let r = sh.rect;
        let net = match owner {
            PassiveOwner::Pin { net, .. } => net,
            PassiveOwner::Ground => u32::MAX,
        };
        strips.clear();
        for vertical in [false, true] {
            let (lo, hi, minus, plus) = if vertical {
                (r.ly, r.uy, r.lx, r.ux)
            } else {
                (r.lx, r.ux, r.ly, r.uy)
            };
            let whole = Run {
                net,
                layer: sh.layer,
                vertical,
                cross: minus + (plus - minus) / 2,
                lo,
                hi,
                minus,
                plus,
                shape: si,
                piece: true,
                passive: Some(owner),
                node_lo: 0,
                node_hi: 0,
            };
            passive_pieces(whole, piece, span, &mut strips);
        }
        // Both whole strips of a shape share its rectangle: one test.
        let mut last: Option<(Rect, bool)> = None;
        for strip in strips.drain(..) {
            let body = body_rect(&strip);
            if let Some(g) = region {
                if !contains_point(&g.core, strip_anchor(&strip)) {
                    continue;
                }
                // A routed conductor crossing this piece must be in the input.
                if !contains_rect(&g.complete, &body) {
                    return Err(invalid(
                        "extraction stripe halo does not cover a passive conductor piece",
                    ));
                }
            }
            let crossed = match last {
                Some((rect, crossed)) if rect == body => crossed,
                _ => crossed_by_route(&routing, sh.layer, body, shapes, bins, stats, limits)?,
            };
            last = Some((body, crossed));
            if !crossed {
                continue;
            }
            built.runs.push(strip);
            if built.runs.len() > limits.max_shapes.saturating_mul(2) {
                return Err(budget("extraction strips"));
            }
        }
    }
    Ok(())
}

/// Mark only virtual conductors crossed by routed metal. One bit per virtual
/// shape replaces a retained rectangle plus two passive-strip records.
fn virtual_crossings(
    shapes: &Shapes<'_>,
    bins: &mut Bins<'_>,
    layers: &[u32],
    stats: &mut ExtractStats,
) -> Result<Vec<u64>> {
    let mut marked = vec![0u64; shapes.virtual_count().div_ceil(64)];
    let Some(instances) = &mut bins.instances else {
        return Ok(marked);
    };
    for (_, shape) in shapes.owned() {
        if !matches!(shape.owner, Owner::Run(_)) {
            continue;
        }
        for &layer in layers.iter().filter(|&&l| l != shape.layer) {
            instances.visit(shapes, layer, shape.rect, &mut stats.work, |id, other| {
                let (a, b) = (shape.rect, other.rect);
                if a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy {
                    let k = id as usize - shapes.virtual_offset();
                    marked[k / 64] |= 1 << (k % 64);
                }
                Ok(true)
            })?;
        }
    }
    Ok(marked)
}

fn virtual_strip(id: u32, shape: Shape, vertical: bool) -> Run {
    let Owner::Passive(owner) = shape.owner else {
        unreachable!("virtual conductor is passive")
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
        cross: minus + (plus - minus) / 2,
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

/// The plate below (`upper == false`) or above a strip: the nearest other
/// calibration metal whose conductors (merged along the strip's cross axis)
/// cover `minus..plus` and extend at least one strip width beyond each side
/// edge (where the edge fringe lands), with the conductor under each side
/// edge (`[plus side, minus side]`). `layered` is sorted by metal, then cross
/// extent.
fn plate(
    layered: &[(u32, i64, i64, u32)],
    own: u32,
    upper: bool,
    minus: i64,
    plus: i64,
) -> Option<(u32, [u32; 2])> {
    let mut metals: Vec<u32> = layered
        .iter()
        .map(|e| e.0)
        .filter(|&m| if upper { m > own } else { m < own })
        .collect();
    metals.dedup();
    if !upper {
        metals.reverse();
    }
    for metal in metals {
        let on: Vec<&(u32, i64, i64, u32)> = layered.iter().filter(|e| e.0 == metal).collect();
        // Merged cross-axis coverage; `on` is sorted by its lower end.
        let margin = plus - minus;
        let (need_lo, need_hi) = (minus - margin, plus + margin);
        let mut spans = false;
        let (mut lo, mut hi) = (on[0].1, on[0].2);
        for e in &on[1..] {
            if e.1 <= hi {
                hi = hi.max(e.2);
            } else {
                spans |= lo <= need_lo && hi >= need_hi;
                (lo, hi) = (e.1, e.2);
            }
        }
        spans |= lo <= need_lo && hi >= need_hi;
        if !spans {
            continue;
        }
        let plus_owner = on
            .iter()
            .filter(|e| e.1 < plus && e.2 >= plus)
            .max_by_key(|e| (e.2, std::cmp::Reverse(e.3)))?;
        let minus_owner = on
            .iter()
            .filter(|e| e.2 > minus && e.1 <= minus)
            .min_by_key(|e| (e.1, e.3))?;
        return Some((metal, [plus_owner.3, minus_owner.3]));
    }
    None
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

/// One stripe of a partitioned extraction (`extract_partitioned`).
#[derive(Clone, Copy, Debug)]
pub struct ExtractRegion<'a> {
    /// Strips anchored in this half-open rectangle (the low end of their
    /// centre line) contribute capacitance; every other strip is only a
    /// neighbour. Nets whose driver access point lies here are counted in
    /// the tree statistics.
    pub core: Rect,
    /// The input holds, whole, every routed net and passive conductor that
    /// meets this rectangle. A contributing strip's neighbourhood must lie
    /// inside it, otherwise extraction stops (`RC_EXTRACT` halo error).
    pub complete: Rect,
    /// `metal_extent` of the whole design (the plate-impossibility rule).
    pub span: [i64; 2],
    /// Bit per pin of a routed net of the whole design: a pin partner of a
    /// contributing strip must have its tree in this stripe.
    pub routed_pins: &'a [u64],
}
/// A pin partner without a tree here is an error in a stripe whose whole
/// design routes the pin's net: that tree belongs in the stripe's input.
fn missing_partner(region: Option<&ExtractRegion<'_>>, pin: u32) -> Result<()> {
    if let Some(g) = region
        && g.routed_pins
            .get(pin as usize / 64)
            .is_some_and(|w| w & (1 << (pin % 64)) != 0)
    {
        return Err(invalid(&format!(
            "extraction stripe lacks the routed net of pin {pin}"
        )));
    }
    Ok(())
}
fn strip_anchor(run: &Run) -> [i64; 2] {
    if run.vertical {
        [run.cross, run.lo]
    } else {
        [run.lo, run.cross]
    }
}
fn contains_point(r: &Rect, p: [i64; 2]) -> bool {
    r.lx <= p[0] && p[0] < r.ux && r.ly <= p[1] && p[1] < r.uy
}
fn contains_rect(outer: &Rect, r: &Rect) -> bool {
    outer.lx <= r.lx && r.ux <= outer.ux && outer.ly <= r.ly && r.uy <= outer.uy
}

/// Compute an RC tree for every routed net plus grounded and coupling caps.
pub fn extract_cpu_ref(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<RcNetwork> {
    extract_region(input, limits, None)
}

/// `extract_cpu_ref` restricted to the strips anchored in `region.core`
/// (whole design when `None`). Trees cover every input net; ground and
/// coupling capacitance come only from the region's strips.
pub fn extract_region(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    region: Option<&ExtractRegion<'_>>,
) -> Result<RcNetwork> {
    preflight(input, limits)?;
    let memory = memory_estimate(input, limits)?;
    let (mut built, mut stats) = build_trees(input, limits, region)?;
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
    let bin_size = extraction_bin(input)?;
    let mut bins = Bins {
        size: bin_size,
        map: bins::BinLists::default(),
        instances: InstanceGeometry::new(&shapes, bin_size, input.layers.len(), limits)?,
    };
    let mut entries = 0usize;
    for (i, s) in shapes.owned() {
        let (x0, x1) = bins.range(s.rect.lx, s.rect.ux);
        let (y0, y1) = bins.range(s.rect.ly, s.rect.uy);
        let cells = (x1 - x0 + 1)
            .checked_mul(y1 - y0 + 1)
            .ok_or_else(|| budget("bin span"))?;
        entries = entries
            .checked_add(usize::try_from(cells).map_err(|_| budget("bin span"))?)
            .ok_or_else(|| budget("bin entries"))?;
        if entries > limits.max_bin_entries {
            return Err(budget("extraction spatial-bin entries"));
        }
        for bx in x0..=x1 {
            for by in y0..=y1 {
                bins.map
                    .push((s.layer, bx, by), i, limits.max_bin_entries)?;
            }
        }
    }
    let net_index: HashMap<u32, usize> = built
        .nets
        .iter()
        .enumerate()
        .map(|(i, n)| (n.net, i))
        .collect();
    add_metal_pieces(
        input,
        limits,
        &mut built,
        &mut shapes,
        &mut bins,
        &mut entries,
        &net_index,
        &mut stats,
    )?;
    add_patches(
        input,
        limits,
        &mut built,
        &mut shapes,
        &mut bins,
        &mut entries,
        &net_index,
        &mut stats,
    )?;
    let routed_runs = built.runs.len();
    // With passive pieces, the extent of all metal bounds any plate's extent.
    let span = if limits.max_passive_strip_length < i64::MAX {
        Some(match region {
            Some(g) => g.span,
            None => metal_extent(input)?,
        })
    } else {
        None
    };
    add_passive_strips(
        input, limits, &mut built, &shapes, &mut bins, &mut stats, region, span,
    )?;
    let crossed = virtual_crossings(&shapes, &mut bins, &routing_layers, &mut stats)?;
    stats.wire_runs = built
        .runs
        .iter()
        .filter(|r| !r.piece && built.home[net_index[&r.net]])
        .count();
    stats.shapes = shapes.len();
    if shapes.retained_len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    let mut coupling = BTreeMap::<((u32, u32), (u32, u32)), f64>::new();
    let mut seen = std::collections::HashSet::new();
    let mut candidates: Vec<u32> = Vec::new();
    let mut points: Vec<i64> = Vec::new();
    // (calibration metal, cross-axis lo, hi, shape) of other-layer conductors.
    let mut layered: Vec<(u32, i64, i64, u32)> = Vec::new();
    let metal_layer: HashMap<u32, usize> = tech
        .layers
        .iter()
        .enumerate()
        .filter_map(|(i, l)| l.as_ref().map(|m| (m.metal, i)))
        .collect();
    let contexts: Vec<Option<ContextIndex>> = tech
        .layers
        .iter()
        .map(|l| {
            l.as_ref().map(|m| {
                m.contexts
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ((c.over, c.under), i))
                    .collect()
            })
        })
        .collect();
    let piece = limits.max_passive_strip_length;
    let extra = (0..shapes.virtual_count())
        .filter(|&i| crossed[i / 64] & (1 << (i % 64)) != 0)
        .flat_map(|i| {
            let id = (shapes.virtual_offset() + i) as u32;
            let shape = shapes.get(id);
            let mut strips = Vec::new();
            for vertical in [false, true] {
                let whole = virtual_strip(id, shape, vertical);
                passive_pieces(whole, piece, span, &mut strips);
            }
            strips
        });
    for (run_index, run) in built.runs[..routed_runs]
        .iter()
        .cloned()
        .chain(extra)
        .chain(built.runs[routed_runs..].iter().cloned())
        .enumerate()
    {
        if run_index.is_multiple_of(4096) {
            guard_memory(limits)?;
        }
        let model = layer_model(input, run.layer)?;
        let ctx_map = contexts[run.layer as usize].as_ref().unwrap();
        let window = model
            .contexts
            .iter()
            .map(|c| c.rows[c.rows.len() - 1][0])
            .fold(0.0f64, f64::max);
        let window_dbu = (window * tech.dbu_per_um).ceil() as i64;
        let (edge_minus, edge_plus) = (run.minus, run.plus);
        // Query region: lateral window on the run layer, footprint elsewhere.
        let (c0, c1) = (edge_minus - window_dbu, edge_plus + window_dbu);
        let query = if run.vertical {
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
        if let Some(region) = region {
            if !contains_point(&region.core, strip_anchor(&run)) {
                continue;
            }
            if !contains_rect(&region.complete, &query) {
                return Err(invalid(
                    "extraction stripe halo does not cover a strip neighbourhood",
                ));
            }
        }
        if run.passive.is_some() {
            stats.passive_strips += 1;
        }
        candidates.clear();
        seen.clear();
        for &layer in &routing_layers {
            bins.visit(layer, query, &shapes, &mut stats, limits, |id, shape| {
                if id == run.shape || seen.contains(&id) {
                    return Ok(());
                }
                let r = shape.rect;
                let (a0, a1, x0, x1) = if run.vertical {
                    (r.ly, r.uy, r.lx, r.ux)
                } else {
                    (r.lx, r.ux, r.ly, r.uy)
                };
                if a1 <= run.lo || a0 >= run.hi {
                    return Ok(());
                }
                let relevant = if layer == run.layer {
                    x1 > c0 && x0 < c1
                } else {
                    x1 > edge_minus && x0 < edge_plus
                };
                if relevant {
                    if candidates.len() >= limits.max_candidates {
                        return Err(budget("extraction candidates per wire run"));
                    }
                    seen.insert(id);
                    candidates.push(id);
                }
                Ok(())
            })?;
        }
        stats.max_run_candidates = stats.max_run_candidates.max(candidates.len());
        if candidates.len() > limits.max_candidates {
            return Err(budget(&format!(
                "extraction candidates per wire run ({} > max_candidates {}; raise \
                 --max-extract-candidates up to {MAX_CANDIDATES_CEILING})",
                candidates.len(),
                limits.max_candidates
            )));
        }
        candidates.sort_unstable();
        points.clear();
        points.push(run.lo);
        points.push(run.hi);
        for &s in &candidates {
            let r = shapes.get(s).rect;
            let (a0, a1) = if run.vertical {
                (r.ly, r.uy)
            } else {
                (r.lx, r.ux)
            };
            points.push(a0.clamp(run.lo, run.hi));
            points.push(a1.clamp(run.lo, run.hi));
        }
        points.sort_unstable();
        points.dedup();
        let span = (run.hi - run.lo) as f64;
        // Passive strips attach to their routed partners only.
        let net_local = match run.passive {
            None => net_index[&run.net],
            Some(_) => usize::MAX,
        };
        // Same metal as this strip: its own net, or for a passive conductor
        // the same supply/ground class or the same pin net.
        let same_metal = |sh: &Shape| -> bool {
            match (sh.owner, run.passive) {
                (Owner::Run(o), _) => built.runs[o as usize].net == run.net,
                (Owner::Passive(p), None) => {
                    matches!(p, PassiveOwner::Pin { net, .. } if net == run.net)
                }
                (Owner::Passive(p), Some(PassiveOwner::Ground)) => p == PassiveOwner::Ground,
                (Owner::Passive(p), Some(PassiveOwner::Pin { net, .. })) => {
                    matches!(p, PassiveOwner::Pin { net: other, .. } if other == net)
                }
            }
        };
        for w in points.windows(2) {
            let (t0, t1) = (w[0], w[1]);
            if t1 <= t0 {
                continue;
            }
            if run.passive.is_some() {
                stats.passive_intervals += 1;
            } else {
                stats.intervals += 1;
            }
            stats.work = stats
                .work
                .checked_add(candidates.len().max(1))
                .ok_or_else(|| budget("extraction work"))?;
            if stats.work > limits.max_work {
                return Err(budget("extraction work"));
            }
            // Nearest lateral neighbour per side and nearest planes.
            let mut plus: Option<(i64, u32)> = None;
            let mut minus: Option<(i64, u32)> = None;
            let (mut covered_plus, mut covered_minus) = (false, false);
            layered.clear();
            for &s in &candidates {
                let sh = &shapes.get(s);
                let r = sh.rect;
                let (a0, a1, x0, x1) = if run.vertical {
                    (r.ly, r.uy, r.lx, r.ux)
                } else {
                    (r.lx, r.ux, r.ly, r.uy)
                };
                if a0 > t0 || a1 < t1 {
                    continue;
                }
                let same = same_metal(sh);
                if sh.layer == run.layer {
                    if x0 >= edge_plus {
                        let gap = x0 - edge_plus;
                        if gap < window_dbu && plus.is_none_or(|p| (gap, s) < p) {
                            plus = Some((gap, s));
                        }
                    } else if x1 <= edge_minus {
                        let gap = edge_minus - x1;
                        if gap < window_dbu && minus.is_none_or(|p| (gap, s) < p) {
                            minus = Some((gap, s));
                        }
                    } else if !same {
                        if run.passive.is_none() {
                            stats.foreign_overlaps += 1;
                        }
                    } else {
                        // Same-net metal across a side edge: that side is
                        // inside the net's metal union, not exposed.
                        covered_plus |= x1 > edge_plus;
                        covered_minus |= x0 < edge_minus;
                    }
                } else {
                    let metal = layer_model(input, sh.layer)?.metal;
                    let overlap = x1.min(edge_plus) - x0.max(edge_minus);
                    if overlap <= 0 {
                        continue;
                    }
                    layered.push((metal, x0, x1, s));
                }
            }
            // A conductor on another layer is a plate for this strip (the
            // calibration OVER/UNDER contexts) only where its layer's metal
            // spans the whole strip cross-section and reaches one strip width
            // beyond both side edges: crossings and clearly wider shapes.
            // Narrower, offset or barely wider parallel metal is not a plate;
            // that overlap coupling is not modelled.
            layered.sort_unstable();
            let below = plate(&layered, model.metal, false, edge_minus, edge_plus);
            let above = plate(&layered, model.metal, true, edge_minus, edge_plus);
            let nearest_below = layered
                .iter()
                .filter(|e| e.0 < model.metal)
                .map(|e| e.0)
                .max();
            let nearest_above = layered
                .iter()
                .filter(|e| e.0 > model.metal)
                .map(|e| e.0)
                .min();
            if run.passive.is_none()
                && (nearest_below != below.map(|b| b.0) || nearest_above != above.map(|a| a.0))
            {
                stats.nonspanning_plane_intervals += 1;
            }
            let over = below.map_or(0, |b| b.0);
            let under = above.map(|a| a.0);
            let exact = ctx_map.get(&(over, under));
            let (ctx, plane_split) = match exact {
                Some(&i) => (i, (below.is_some(), above.is_some())),
                None => {
                    if run.passive.is_none() {
                        stats.context_fallback_intervals += 1;
                    }
                    if let Some(&i) = ctx_map.get(&(over, None)).filter(|_| over > 0) {
                        (i, (true, false))
                    } else if let Some(&i) = ctx_map.get(&(0, under)) {
                        (i, (false, above.is_some()))
                    } else {
                        (ctx_map[&(0, None)], (false, false))
                    }
                }
            };
            let sub = &model.contexts[ctx_map[&(0, None)]].rows;
            let rows = &model.contexts[ctx].rows;
            let below_only = if plane_split.0 && plane_split.1 {
                ctx_map.get(&(over, None)).map(|&i| &model.contexts[i].rows)
            } else {
                None
            };
            let length = um(tech, t1 - t0);
            let mid = (t0 + t1) as f64 / 2.0;
            let w_hi = if span > 0.0 {
                (mid - run.lo as f64) / span
            } else {
                0.5
            };
            let mid_point = if run.vertical {
                [run.cross as f64, mid]
            } else {
                [mid, run.cross as f64]
            };
            let mut ground = 0.0f64;
            // (shape, pF, factor applied when the partner is another run).
            let mut contributions: Vec<(u32, f64, f64)> = Vec::with_capacity(6);
            for (k, side, covered) in [(0, plus, covered_plus), (1, minus, covered_minus)] {
                if covered {
                    if run.passive.is_none() {
                        stats.covered_side_intervals += 1;
                    }
                    continue;
                }
                let spacing = side.map(|(g, _)| um(tech, g));
                let touching_same = side.is_some_and(|(g, s)| g == 0 && same_metal(&shapes.get(s)));
                if touching_same {
                    continue;
                }
                let (cc, cg) = sample(rows, spacing);
                let (_, cg_sub) = sample(sub, spacing);
                // Split this side's context value between the substrate and
                // the planes. A conductor below is the plate of an OVER
                // context: it shields the substrate, so the whole OVER value
                // goes to it. A plane above takes only the excess over what
                // goes down (the substrate share, or the OVER value).
                let cg = cg.max(0.0);
                let (g, d_below, d_above) = match plane_split {
                    (true, true) => {
                        let down = below_only.map_or(cg_sub, |r| sample(r, spacing).1).min(cg);
                        (0.0, down, cg - down)
                    }
                    (true, false) => (0.0, cg, 0.0),
                    (false, true) => {
                        let up = (cg - cg_sub).max(0.0);
                        (cg - up, 0.0, up)
                    }
                    (false, false) => (cg, 0.0, 0.0),
                };
                let mut g = g;
                // Each conductor's own cross-section is authoritative for its
                // own field: a crossing is counted from both wires (the upper
                // wire's OVER cut over the lower wire's width, the lower wire's
                // UNDER cut over the upper wire's width).
                for (d, plane) in [(d_below, below), (d_above, above)] {
                    if d > 0.0 {
                        match plane {
                            Some((_, owners)) => contributions.push((owners[k], d * length, 1.0)),
                            None => g += d,
                        }
                    }
                }
                ground += g.max(0.0) * length;
                // Lateral coupling between two runs is computed from both sides.
                if let Some((_, s)) = side
                    && cc > 0.0
                {
                    contributions.push((s, cc * length, 0.5));
                }
            }
            // Overlap area of non-plate metal on other layers: a conductor
            // that covers part of this strip's face (stacked, offset or barely
            // wider metal) but is not a calibration plate couples over the
            // covered width, counted once from the upper face; the covered
            // face no longer sees the substrate. A piece counts its face on
            // its x strip only.
            if run.passive.is_none()
                && !(run.piece && run.vertical)
                && let Some(area_cap) = model.area_cap
            {
                let mut shielded = 0.0;
                if below.is_none() {
                    for (metal, covered, owners) in
                        face_portions(&layered, model.metal, false, edge_minus, edge_plus)
                    {
                        let Some(&(_, ov)) = model.overlap.iter().find(|o| o.0 == metal) else {
                            stats.overlap_pairs_without_data += 1;
                            continue;
                        };
                        let area = um(tech, covered) * length;
                        stats.overlap_area_um2 += area;
                        shielded += area_cap * area;
                        for (sh, w) in owners {
                            contributions.push((sh, ov * um(tech, w) * length, 1.0));
                        }
                    }
                }
                if above.is_none() {
                    // A routed conductor above counts the overlap from its
                    // own face; passive metal does not model itself.
                    for (metal, _, owners) in
                        face_portions(&layered, model.metal, true, edge_minus, edge_plus)
                    {
                        let passive: Vec<(u32, i64)> = owners
                            .into_iter()
                            .filter(|o| matches!(shapes.get(o.0).owner, Owner::Passive(_)))
                            .collect();
                        if passive.is_empty() {
                            continue;
                        }
                        let upper = metal_layer
                            .get(&metal)
                            .and_then(|&l| tech.layers[l].as_ref())
                            .and_then(|u| u.overlap.iter().find(|o| o.0 == model.metal));
                        let Some(&(_, ov)) = upper else {
                            stats.overlap_pairs_without_data += 1;
                            continue;
                        };
                        for (sh, w) in passive {
                            stats.overlap_area_um2 += um(tech, w) * length;
                            contributions.push((sh, ov * um(tech, w) * length, 1.0));
                        }
                    }
                }
                ground = (ground - shielded).max(0.0);
            }
            if run.passive.is_some() {
                // Only this conductor's plate shares toward routed conductors
                // of other nets: ground and lateral sides are counted from the
                // routed side.
                for (s, value, run_factor) in contributions {
                    if run_factor < 1.0 {
                        continue;
                    }
                    let Owner::Run(o) = shapes.get(s).owner else {
                        continue;
                    };
                    let other = &built.runs[o as usize];
                    if other.passive.is_some() || other.net == run.net {
                        continue;
                    }
                    let pnode = nearer_end(other, mid_point);
                    let li = net_index[&other.net];
                    let pin_node = match run.passive {
                        Some(PassiveOwner::Pin { pin, .. }) => {
                            let node = built.pin_nodes.get(&pin).copied();
                            if node.is_none() {
                                missing_partner(region, pin)?;
                            }
                            node
                        }
                        _ => None,
                    };
                    match pin_node {
                        Some(a) => {
                            let b = (other.net, pnode);
                            let key = if a <= b { (a, b) } else { (b, a) };
                            *coupling.entry(key).or_insert(0.0) += value;
                            if coupling.len() > limits.max_couplings {
                                return Err(budget("extraction coupling capacitors"));
                            }
                        }
                        None => {
                            if matches!(run.passive, Some(PassiveOwner::Pin { .. })) {
                                stats.unrouted_pin_partners += 1;
                            }
                            built.nets[li].ground[pnode as usize] += value;
                        }
                    }
                }
                continue;
            }
            // Attach to this run's end nodes by position (pi split).
            let rc = &mut built.nets[net_local];
            rc.ground[run.node_lo as usize] += ground * (1.0 - w_hi);
            rc.ground[run.node_hi as usize] += ground * w_hi;
            for (s, value, run_factor) in contributions {
                let sh = shapes.get(s);
                let partner = match sh.owner {
                    Owner::Run(o) => {
                        // Every routed-net shape models its own exposed sides,
                        // so a lateral pair is counted half from each side.
                        let other = &built.runs[o as usize];
                        if other.net == run.net {
                            None
                        } else {
                            Some(Some((other.net, nearer_end(other, mid_point), run_factor)))
                        }
                    }
                    Owner::Passive(PassiveOwner::Ground) => Some(None),
                    Owner::Passive(PassiveOwner::Pin { net, pin }) => {
                        if net == run.net {
                            None
                        } else if let Some(&(n, node)) = built.pin_nodes.get(&pin) {
                            Some(Some((n, node, 1.0)))
                        } else {
                            missing_partner(region, pin)?;
                            stats.unrouted_pin_partners += 1;
                            Some(None)
                        }
                    }
                };
                match partner {
                    None => {}
                    Some(None) => {
                        let rc = &mut built.nets[net_local];
                        rc.ground[run.node_lo as usize] += value * (1.0 - w_hi);
                        rc.ground[run.node_hi as usize] += value * w_hi;
                    }
                    Some(Some((pnet, pnode, factor))) => {
                        for (node, weight) in [(run.node_lo, 1.0 - w_hi), (run.node_hi, w_hi)] {
                            let v = value * factor * weight;
                            if v <= 0.0 {
                                continue;
                            }
                            let a = (run.net, node);
                            let b = (pnet, pnode);
                            let key = if a <= b { (a, b) } else { (b, a) };
                            *coupling.entry(key).or_insert(0.0) += v;
                            if coupling.len() > limits.max_couplings {
                                return Err(budget("extraction coupling capacitors"));
                            }
                        }
                    }
                }
            }
        }
    }
    let couplings: Vec<RcCoupling> = coupling
        .into_iter()
        .filter(|(_, v)| *v > 0.0)
        .map(|((a, b), pf)| RcCoupling { a, b, pf })
        .collect();
    stats.couplings = couplings.len();
    stats.coupling_pf = couplings.iter().map(|c| c.pf).sum();
    stats.ground_pf = built.nets.iter().flat_map(|n| n.ground.iter()).sum();
    let elements = stats
        .resistors
        .checked_add(stats.nodes)
        .and_then(|n| n.checked_add(2 * stats.couplings))
        .ok_or_else(|| budget("extraction elements"))?;
    if elements > limits.max_elements {
        return Err(budget("extraction RC elements"));
    }
    guard_memory(limits)?;
    Ok(RcNetwork {
        nets: built.nets,
        couplings,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{Access, LayerRect, Patch, RouteStats, Via, Wire};

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
            },
            RouteLayer {
                name: "M2".into(),
                width: 200,
                spacing: 100,
                vertical: Some(true),
                min_area: None,
                min_step: None,
            },
            RouteLayer {
                name: "V1".into(),
                width: 100,
                spacing: 100,
                vertical: None,
                min_area: None,
                min_step: None,
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
    fn run(
        r: &Routes,
        t: &[Terminal],
        passive: &[PassiveShape],
        tech: &RcTech,
    ) -> Result<RcNetwork> {
        let (l, v) = (layers(), vias());
        extract_cpu_ref(
            &ExtractInput {
                layers: &l,
                vias: &v,
                routes: r,
                terminals: t,
                passive,
                held_bytes: 0,
                instances: None,
                tech,
            },
            &ExtractLimits::default(),
        )
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

    /// A 300 x 300 um block: `count` random L-shaped nets (M1, a via, M2),
    /// some with a third pin or a patch, over M1 supply rails and M2 straps;
    /// retained pin shapes, unrouted pin shapes, a grid of cells (an M1
    /// obstruction and an unrouted pin each) and per-cell driver pins.
    struct Block {
        routes: Routes,
        terminals: Vec<Terminal>,
        passive: Vec<PassiveShape>,
        cells: crate::instance_obstacles::InstanceObstacles,
        signal: Vec<bool>,
    }
    fn block(count: u32, seed: u64) -> Block {
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        let mut state = seed;
        let mut next = |n: i64| -> i64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % n as u64) as i64
        };
        let um = 1000;
        let (mut wires, mut vias, mut access, mut patches) = (vec![], vec![], vec![], vec![]);
        let (mut terminals, mut passive) = (vec![], vec![]);
        let mut cells = vec![];
        let (mut pin_nets, mut pin_ids) = (vec![], vec![]);
        let mut pin = 0u32;
        for net in 0..count {
            let x0 = (5 + next(240)) * um;
            let x1 = x0 + (3 + next(45)) * um;
            let y = (5 + next(290)) * um;
            let rise = (3 + next(40)) * um;
            let y2 = if (next(2) == 0 && y + rise < 297 * um) || y < rise + 3 * um {
                y + rise
            } else {
                y - rise
            };
            for x in (x0..x1).step_by(um as usize) {
                wires.push(Wire {
                    net,
                    layer: 0,
                    start: [x, y],
                    end: [x + um, y],
                });
            }
            vias.push(Via {
                net,
                kind: 0,
                at: [x1, y],
            });
            for yy in (y.min(y2)..y.max(y2)).step_by(um as usize) {
                wires.push(Wire {
                    net,
                    layer: 1,
                    start: [x1, yy],
                    end: [x1, yy + um],
                });
            }
            access.push(Access {
                pin,
                net,
                layer: 0,
                at: [x0, y],
            });
            terminals.push(terminal(pin, net, true));
            if net % 2 == 0 {
                passive.push(PassiveShape {
                    layer: 0,
                    rect: Rect {
                        lx: x0 - 150,
                        ly: y - 150,
                        ux: x0 + 150,
                        uy: y + 150,
                    },
                    owner: PassiveOwner::Pin { net, pin },
                });
            } else {
                // The driver pin as a cell shape (one pin, no obstruction).
                cells.push(ObstacleInstance {
                    template: 1,
                    pin_start: pin_nets.len() as u32,
                    at: [x0 - 100, y - 100],
                    orientation: crate::Orientation::N,
                });
                pin_nets.push(net);
                pin_ids.push(pin);
            }
            access.push(Access {
                pin: pin + 1,
                net,
                layer: 1,
                at: [x1, y2],
            });
            terminals.push(terminal(pin + 1, net, false));
            pin += 2;
            let xm = x0 + (x1 - x0) / um / 2 * um;
            if net % 3 == 0 && xm > x0 {
                access.push(Access {
                    pin,
                    net,
                    layer: 0,
                    at: [xm, y],
                });
                terminals.push(terminal(pin, net, false));
                pin += 1;
            }
            if net % 4 == 1 {
                patches.push(Patch {
                    net,
                    layer: 0,
                    rect: Rect {
                        lx: x0 - 300,
                        ly: y - 50,
                        ux: x0 + 300,
                        uy: y + 400,
                    },
                });
            }
        }
        for y in (10..300).step_by(20) {
            passive.push(PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: -5 * um,
                    ly: y * um - 240,
                    ux: 305 * um,
                    uy: y * um + 240,
                },
                owner: PassiveOwner::Ground,
            });
        }
        for x in [60, 160, 260] {
            passive.push(PassiveShape {
                layer: 1,
                rect: Rect {
                    lx: x * um - 800,
                    ly: -5 * um,
                    ux: x * um + 800,
                    uy: 305 * um,
                },
                owner: PassiveOwner::Ground,
            });
        }
        // Pin shapes of a net that is not routed.
        for k in 0..20 {
            let (x, y) = ((5 + next(290)) * um, (5 + next(290)) * um);
            passive.push(PassiveShape {
                layer: 0,
                rect: Rect {
                    lx: x,
                    ly: y,
                    ux: x + 400,
                    uy: y + 400,
                },
                owner: PassiveOwner::Pin {
                    net: count,
                    pin: 1_000_000 + k,
                },
            });
        }
        // A grid of cells: an obstruction and a pin no route reaches.
        for i in 0..12 {
            for j in 0..17 {
                cells.push(ObstacleInstance {
                    template: 0,
                    pin_start: pin_nets.len() as u32,
                    at: [7 * um + 23 * um * i, 3 * um + 17 * um * j],
                    orientation: crate::Orientation::N,
                });
                pin_nets.push(((i * 17 + j) % i64::from(count)) as u32);
                pin_ids.push(2_000_000 + (i * 17 + j) as u32);
            }
        }
        let templates = vec![
            ObstacleTemplate::new(
                2720,
                1,
                vec![
                    TemplateObstacle {
                        layer: 0,
                        rect: Rect {
                            lx: 0,
                            ly: 0,
                            ux: 1500,
                            uy: 300,
                        },
                        pin: None,
                    },
                    TemplateObstacle {
                        layer: 0,
                        rect: Rect {
                            lx: 1700,
                            ly: 0,
                            ux: 1900,
                            uy: 600,
                        },
                        pin: Some(0),
                    },
                ],
            )
            .unwrap(),
            ObstacleTemplate::new(
                200,
                1,
                vec![TemplateObstacle {
                    layer: 0,
                    rect: Rect {
                        lx: 0,
                        ly: 0,
                        ux: 200,
                        uy: 200,
                    },
                    pin: Some(0),
                }],
            )
            .unwrap(),
        ];
        let cells = InstanceObstacles::new(templates, cells, pin_nets)
            .unwrap()
            .with_pin_ids(pin_ids)
            .unwrap();
        let mut r = routes(wires, vias, access, (0..count).collect());
        r.patches = patches;
        Block {
            routes: r,
            terminals,
            passive,
            cells,
            signal: vec![true; count as usize + 1],
        }
    }
    fn same_network(a: &RcNetwork, b: &RcNetwork, tolerance: f64) {
        let close = |p: f64, q: f64| (p - q).abs() <= tolerance * p.abs().max(q.abs());
        assert_eq!(a.nets.len(), b.nets.len());
        for (x, y) in a.nets.iter().zip(&b.nets) {
            assert_eq!(
                (x.net, &x.nodes, &x.resistors),
                (y.net, &y.nodes, &y.resistors)
            );
            for (p, q) in x.ground.iter().zip(&y.ground) {
                assert!(close(*p, *q), "net {} ground {p} vs {q}", x.net);
            }
        }
        assert_eq!(a.couplings.len(), b.couplings.len());
        for (x, y) in a.couplings.iter().zip(&b.couplings) {
            assert_eq!((x.a, x.b), (y.a, y.b));
            assert!(close(x.pf, y.pf), "{x:?} vs {y:?}");
        }
        let counts = |s: &ExtractStats| {
            [
                s.nets,
                s.lattice_wires,
                s.wire_runs,
                s.vias,
                s.nodes,
                s.resistors,
                s.opened_loop_resistors,
                s.coincident_pin_links,
                s.duplicate_wire_edges,
                s.segment_cut_nodes,
                s.couplings,
                s.intervals,
                s.max_run_candidates,
                s.context_fallback_intervals,
                s.foreign_overlaps,
                s.unrouted_pin_partners,
                s.patches,
                s.patch_pieces,
                s.end_pieces,
                s.pad_pieces,
                s.covered_side_intervals,
                s.nonspanning_plane_intervals,
                s.passive_strips,
                s.passive_intervals,
                s.overlap_pairs_without_data,
                s.wire_length_dbu as usize,
            ]
        };
        assert_eq!(counts(&a.stats), counts(&b.stats));
        for (p, q) in [
            (a.stats.ground_pf, b.stats.ground_pf),
            (a.stats.coupling_pf, b.stats.coupling_pf),
            (a.stats.resistance_ohm, b.stats.resistance_ohm),
            (a.stats.overlap_area_um2, b.stats.overlap_area_um2),
        ] {
            assert!(close(p, q), "{p} vs {q}");
        }
    }
    fn block_input<'a>(
        b: &'a Block,
        passive: &'a [PassiveShape],
        l: &'a [RouteLayer],
        v: &'a [RouteVia],
        tech: &'a RcTech,
    ) -> ExtractInput<'a> {
        ExtractInput {
            layers: l,
            vias: v,
            routes: &b.routes,
            terminals: &b.terminals,
            passive,
            held_bytes: 0,
            instances: Some(InstancedPassive {
                geometry: &b.cells,
                signal_nets: &b.signal,
            }),
            tech,
        }
    }
    fn plan(count: usize, halo: i64) -> StripePlan {
        StripePlan {
            cuts: even_cuts(0, 300_000, count).unwrap(),
            halo,
            max_nodes: 1 << 20,
            max_elements: 1 << 22,
            max_couplings: 1 << 20,
            parallel: 1,
        }
    }
    #[test]
    fn stripes_reproduce_the_whole_extraction() {
        let b = block(160, 7);
        let (l, v, tech) = (layers(), vias(), tech(true));
        let input = block_input(&b, &b.passive, &l, &v, &tech);
        // Per-call caps sized to the block, so four stripe working sets fit
        // the default allowance at once.
        let limits = ExtractLimits {
            max_passive_strip_length: 10_000,
            max_shapes: 100_000,
            max_bin_entries: 1_000_000,
            max_nodes: 50_000,
            max_elements: 200_000,
            max_couplings: 100_000,
            ..Default::default()
        };
        let whole = extract_cpu_ref(&input, &limits).unwrap();
        let s = &whole.stats;
        // The block exercises what a partition must preserve.
        assert!(s.patches > 10 && s.unrouted_pin_partners > 0 && s.foreign_overlaps > 0);
        assert!(
            s.passive_strips > 200 && whole.couplings.len() > 400,
            "{s:?}"
        );
        let halo = minimum_halo(&input, &limits).unwrap();
        assert_eq!(halo, 25_000 + 200 + 1000 + 1);
        for (count, parallel) in [(1, 1), (2, 1), (3, 2), (7, 4)] {
            let part = extract_partitioned(
                &input,
                &limits,
                &StripePlan {
                    parallel,
                    ..plan(count, halo)
                },
            )
            .unwrap();
            // One stripe repeats the whole extraction bit for bit.
            same_network(&whole, &part, if count == 1 { 0.0 } else { 1e-12 });
            let stripes = &part.stats.partition.as_ref().unwrap();
            assert_eq!(stripes.stripes.len(), count);
            assert_eq!(stripes.repeated_trees > 0, count > 1);
            let homes: usize = stripes.stripes.iter().map(|s| s.home_nets).sum();
            assert_eq!(homes, 160);
        }
    }
    #[test]
    fn passive_pieces_keep_perpendicular_crossing_values() {
        let b = block(160, 11);
        let (l, v, tech) = (layers(), vias(), tech(true));
        let input = block_input(&b, &b.passive, &l, &v, &tech);
        let whole = extract_cpu_ref(&input, &ExtractLimits::default()).unwrap();
        let limits = ExtractLimits {
            max_passive_strip_length: 10_000,
            ..Default::default()
        };
        let pieces = extract_cpu_ref(&input, &limits).unwrap();
        // Every plate here crosses its strip, so a piece boundary inside an
        // interval only splits its sum; rail ends and strap sides, which no
        // metal can plate, are dropped.
        assert_eq!(whole.nets.len(), pieces.nets.len());
        for (x, y) in whole.nets.iter().zip(&pieces.nets) {
            assert_eq!(x.resistors, y.resistors);
            for (p, q) in x.ground.iter().zip(&y.ground) {
                assert!((p - q).abs() <= 1e-12 * p.abs().max(q.abs()), "{p} {q}");
            }
        }
        assert_eq!(whole.couplings.len(), pieces.couplings.len());
        for (x, y) in whole.couplings.iter().zip(&pieces.couplings) {
            assert_eq!((x.a, x.b), (y.a, y.b));
            assert!((x.pf - y.pf).abs() <= 1e-12 * x.pf.max(y.pf));
        }
        assert!(pieces.stats.passive_strips > whole.stats.passive_strips);
        assert!(pieces.stats.max_run_candidates < whole.stats.max_run_candidates);
    }
    #[test]
    fn stripe_halo_and_missing_nets_fail_explicitly() {
        let b = block(160, 7);
        let (l, v, tech) = (layers(), vias(), tech(true));
        let input = block_input(&b, &b.passive, &l, &v, &tech);
        let limits = ExtractLimits {
            max_passive_strip_length: 10_000,
            ..Default::default()
        };
        let halo = minimum_halo(&input, &limits).unwrap();
        let refused = |r: Result<RcNetwork>, text: &str| match r {
            Err(Error::Invalid(m)) => assert!(m.contains(text), "{m}"),
            other => panic!("expected {text}, got {:?}", other.map(|n| n.stats)),
        };
        refused(
            extract_partitioned(&input, &limits, &plan(3, halo - 1)),
            "below the minimum",
        );
        // Four default-cap stripe working sets (about 1 GiB each) do not fit
        // the 2 GiB allowance: refused before any stripe runs.
        assert!(matches!(
            extract_partitioned(&input, &limits, &StripePlan {
                parallel: 4,
                ..plan(3, halo)
            }),
            Err(Error::Budget(m)) if m.contains("4 concurrent extraction stripes")
        ));
        refused(
            extract_partitioned(
                &input,
                &ExtractLimits {
                    max_passive_strip_length: i64::MAX,
                    ..limits.clone()
                },
                &plan(3, halo),
            ),
            "finite passive strip length",
        );
        // An 80 um wide obstruction anchored just inside the middle stripe:
        // its side strip reaches beyond the minimum halo and is refused, a
        // wider halo extracts it like the whole design.
        let mut passive = b.passive.clone();
        passive.push(PassiveShape {
            layer: 0,
            rect: Rect {
                lx: 62_000,
                ly: 150_600,
                ux: 142_000,
                uy: 151_000,
            },
            owner: PassiveOwner::Ground,
        });
        let wide = block_input(&b, &passive, &l, &v, &tech);
        refused(
            extract_partitioned(&wide, &limits, &plan(3, halo)),
            "halo does not cover",
        );
        let whole = extract_cpu_ref(&wide, &limits).unwrap();
        let part = extract_partitioned(&wide, &limits, &plan(3, 50_000)).unwrap();
        same_network(&whole, &part, 1e-12);
        // A region whose input misses a routed net that owns a pin shape in
        // its core: refused wherever that pin is a partner of a core strip.
        let core = Rect {
            lx: 100_000,
            ly: i64::MIN,
            ux: 200_000,
            uy: i64::MAX,
        };
        let mut pins = vec![0u64; 64];
        for a in &b.routes.access {
            pins[a.pin as usize / 64] |= 1 << (a.pin % 64);
        }
        let span = metal_extent(&input).unwrap();
        let region = |pins| ExtractRegion {
            core,
            complete: Rect {
                lx: core.lx - halo,
                ux: core.ux + halo,
                ..core
            },
            span,
            routed_pins: pins,
        };
        let mut refusals = 0;
        for net in b.passive.iter().filter_map(|s| match s.owner {
            PassiveOwner::Pin { net, .. }
                if net < 160 && s.rect.lx >= core.lx && s.rect.ux < core.ux =>
            {
                Some(net)
            }
            _ => None,
        }) {
            let mut sub = b.routes.clone();
            sub.wires.retain(|w| w.net != net);
            sub.vias.retain(|w| w.net != net);
            sub.access.retain(|w| w.net != net);
            sub.patches.retain(|w| w.net != net);
            sub.nets.retain(|&n| n != net);
            let lacking = ExtractInput {
                routes: &sub,
                ..block_input(&b, &b.passive, &l, &v, &tech)
            };
            match extract_region(&lacking, &limits, Some(&region(&pins))) {
                Err(Error::Invalid(m)) if m.contains("lacks the routed net of pin") => {
                    refusals += 1;
                    // Without the routed-pin record the same input passes
                    // silently: the check turns a partition gap into an error.
                    assert!(extract_region(&lacking, &limits, Some(&region(&[]))).is_ok());
                }
                Ok(_) => {}
                Err(e) => panic!("{e}"),
            }
        }
        assert!(refusals > 0);
    }
}
