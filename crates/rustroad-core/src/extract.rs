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
    pub tech: &'a RcTech,
}
#[derive(Clone, Debug)]
pub struct ExtractLimits {
    /// All conductor shapes (wire runs, via pads, pins, obstructions, supply).
    pub max_shapes: usize,
    /// Electrical nodes over all nets.
    pub max_nodes: usize,
    /// Resistors plus ground and coupling capacitors.
    pub max_elements: usize,
    pub max_couplings: usize,
    /// Bin-entry scans plus candidate-by-interval comparisons, whole design.
    pub max_work: usize,
    /// Candidate shapes considered for one wire run.
    pub max_candidates: usize,
    /// Spatial-bin entries.
    pub max_bin_entries: usize,
    /// Straight runs longer than this are split at existing lattice nodes (DBU).
    pub max_segment_length: i64,
}
impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_shapes: 2_000_000,
            max_nodes: 500_000,
            max_elements: 1_000_000,
            max_couplings: 1_000_000,
            max_work: 200_000_000,
            max_candidates: 4096,
            max_bin_entries: 8_000_000,
            max_segment_length: 25_000,
        }
    }
}
impl ExtractLimits {
    pub fn validate(&self) -> Result<()> {
        let hard = Self::default();
        for (value, ceiling) in [
            (self.max_shapes, hard.max_shapes),
            (self.max_nodes, hard.max_nodes * 8),
            (self.max_elements, hard.max_elements * 8),
            (self.max_couplings, hard.max_couplings * 8),
            (self.max_work, hard.max_work * 8),
            (self.max_candidates, hard.max_candidates),
            (self.max_bin_entries, hard.max_bin_entries),
        ] {
            if value == 0 || value > ceiling {
                return Err(budget("extraction limit outside hard bounds"));
            }
        }
        if self.max_segment_length <= 0 {
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
    pub nets: usize,
    pub lattice_wires: usize,
    pub wire_runs: usize,
    pub vias: usize,
    pub nodes: usize,
    pub resistors: usize,
    /// Loop-closing conductors whose resistance was dropped to keep a tree.
    pub opened_loop_resistors: usize,
    pub coincident_pin_links: usize,
    pub couplings: usize,
    pub shapes: usize,
    pub intervals: usize,
    pub work: usize,
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
/// One capacitance-bearing strip. A routed wire run has its two long sides at
/// `cross -/+ width/2` and spans its centerline `lo..hi` (end caps are not
/// modelled). A routing patch contributes two strips per uncovered piece: one
/// along x (bottom and top edges) and one along y (left and right edges), so
/// all four piece edges are sides; both strips share one shape.
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
    /// Strip of a routing patch piece (`Routes::patches`).
    patch: bool,
    /// Local (tree-order) nodes at `lo` and `hi`.
    node_lo: u32,
    node_hi: u32,
}
#[derive(Clone, Copy, Debug)]
enum Owner {
    Run(u32),
    Pad { net: u32, node: u32 },
    Passive(PassiveOwner),
}
#[derive(Clone, Copy, Debug)]
struct Shape {
    layer: u32,
    rect: Rect,
    owner: Owner,
}
impl Shape {
    fn net(&self) -> Option<u32> {
        match self.owner {
            Owner::Run(_) => None,
            Owner::Pad { net, .. } => Some(net),
            Owner::Passive(PassiveOwner::Pin { net, .. }) => Some(net),
            Owner::Passive(PassiveOwner::Ground) => None,
        }
    }
}

struct Built {
    nets: Vec<RcNet>,
    runs: Vec<Run>,
    pads: Vec<Shape>,
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

/// Build per-net graphs, straight runs, the tree order and resistors.
fn build_trees(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<(Built, ExtractStats)> {
    let routes = input.routes;
    let mut stats = ExtractStats::default();
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
        runs: Vec::new(),
        pads: Vec::new(),
        pin_nodes: HashMap::new(),
    };
    for &net in &routes.nets {
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
        if stats.nodes + keys.len() > limits.max_nodes.saturating_mul(4) {
            return Err(budget("extraction graph nodes"));
        }
        let id = |k: &Key| keys.binary_search(k).map(|i| i as u32);
        let missing = |_| invalid("graph node lookup");
        let mut edges: Vec<(u32, u32, EdgeKind)> = Vec::new();
        for &i in net_wires {
            let w = &routes.wires[i];
            let a = id(&(w.layer, w.start[0], w.start[1])).map_err(missing)?;
            let b = id(&(w.layer, w.end[0], w.end[1])).map_err(missing)?;
            edges.push((a.min(b), a.max(b), EdgeKind::Wire(w.layer)));
        }
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
            // Split long chains at interior lattice nodes.
            let axis = |g: u32| {
                let k = point(g);
                if keys[chain[0] as usize].1 == keys[chain[chain.len() - 1] as usize].1 {
                    k.2
                } else {
                    k.1
                }
            };
            let mut seg_start = 0usize;
            for i in 1..chain.len() {
                let last = i == chain.len() - 1;
                let too_long = !last
                    && (axis(chain[i + 1]) - axis(chain[seg_start])).abs()
                        > limits.max_segment_length;
                if last || too_long {
                    junction[chain[i] as usize] = true;
                    run_ends.push((chain[seg_start], chain[i], layer));
                    seg_start = i;
                }
            }
        }
        if used
            .iter()
            .zip(&edges)
            .any(|(u, e)| !u && matches!(e.2, EdgeKind::Wire(_)))
        {
            return Err(invalid("wire not covered by a straight run"));
        }
        // Electrical elements over junction nodes plus extra pin nodes.
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
        let mut total_nodes = n as u32;
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
            if (g as usize) < n {
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
        stats.nodes += nodes.len();
        if stats.nodes > limits.max_nodes {
            return Err(budget("extraction RC nodes"));
        }
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
                patch: false,
                node_lo: local[lo_node as usize],
                node_hi: local[hi_node as usize],
            });
        }
        // Via landing pads are passive conductors of this net at the via node.
        for &i in net_vias {
            let v = &routes.vias[i];
            let spec = &input.vias[v.kind as usize];
            for s in &spec.shapes {
                if input.layers[s.layer as usize].vertical.is_none() {
                    continue;
                }
                let g = id(&(s.layer, v.at[0], v.at[1])).map_err(missing)?;
                out.pads.push(Shape {
                    layer: s.layer,
                    rect: crate::routing::shifted(s.rect, v.at)?,
                    owner: Owner::Pad {
                        net,
                        node: local[g as usize],
                    },
                });
            }
        }
        stats.lattice_wires += net_wires.len();
        stats.vias += net_vias.len();
        out.nets.push(RcNet {
            net,
            ground: vec![0.0; nodes.len()],
            nodes,
            resistors,
        });
    }
    stats.nets = out.nets.len();
    stats.wire_runs = out.runs.len();
    Ok((out, stats))
}

struct Bins {
    size: i64,
    map: HashMap<(u32, i64, i64), Vec<u32>>,
}
impl Bins {
    fn range(&self, lo: i64, hi: i64) -> (i64, i64) {
        (lo.div_euclid(self.size), hi.div_euclid(self.size))
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

/// Pieces of one patch that may exist per patch after subtraction.
const MAX_PATCH_PIECES: usize = 256;

/// Attach every routing patch to the RC node of a same-net conductor it
/// overlaps or touches (largest overlap area, then lowest shape index; a wire
/// run attaches at its end nearer the patch centre), and add each piece of the
/// patch not already covered by same-net metal on its layer as a conductor
/// shape with two side strips. Patches add capacitance only: they never add a
/// node or a resistor (the tree stays the wire graph's tree).
#[allow(clippy::too_many_arguments)]
fn add_patches(
    input: &ExtractInput<'_>,
    limits: &ExtractLimits,
    built: &mut Built,
    shapes: &mut Vec<Shape>,
    bins: &mut Bins,
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
        let (x0, x1) = bins.range(r.lx, r.ux);
        let (y0, y1) = bins.range(r.ly, r.uy);
        let mut near: Vec<u32> = Vec::new();
        for bx in x0..=x1 {
            for by in y0..=y1 {
                let Some(list) = bins.map.get(&(patch.layer, bx, by)) else {
                    continue;
                };
                stats.work = stats
                    .work
                    .checked_add(list.len())
                    .ok_or_else(|| budget("extraction work"))?;
                if stats.work > limits.max_work {
                    return Err(budget("extraction work"));
                }
                near.extend(list.iter().copied());
            }
        }
        near.sort_unstable();
        near.dedup();
        let net_of = |s: &Shape| match s.owner {
            Owner::Run(o) => Some(built.runs[o as usize].net),
            _ => s.net(),
        };
        let touching: Vec<u32> = near
            .into_iter()
            .filter(|&s| {
                let q = shapes[s as usize].rect;
                net_of(&shapes[s as usize]) == Some(patch.net)
                    && q.lx <= r.ux
                    && r.lx <= q.ux
                    && q.ly <= r.uy
                    && r.ly <= q.uy
            })
            .collect();
        if touching.len() > limits.max_candidates {
            return Err(budget("extraction candidates per routing patch"));
        }
        let overlap = |q: Rect| -> i128 {
            let w = i128::from(q.ux.min(r.ux)) - i128::from(q.lx.max(r.lx));
            let h = i128::from(q.uy.min(r.uy)) - i128::from(q.ly.max(r.ly));
            w.max(0) * h.max(0)
        };
        let anchor = touching
            .iter()
            .copied()
            .max_by_key(|&s| (overlap(shapes[s as usize].rect), std::cmp::Reverse(s)))
            .ok_or_else(|| invalid("floating routing patch (touches no same-net conductor)"))?;
        let node = match shapes[anchor as usize].owner {
            Owner::Run(o) => {
                let run = &built.runs[o as usize];
                if run.patch {
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
            Owner::Pad { node, .. } => node,
            Owner::Passive(PassiveOwner::Pin { pin, .. }) => built
                .pin_nodes
                .get(&pin)
                .filter(|(n, _)| *n == patch.net)
                .map(|(_, node)| *node)
                .ok_or_else(|| invalid("routing patch touches an unrouted pin only"))?,
            Owner::Passive(PassiveOwner::Ground) => unreachable!("ground has no net"),
        };
        // Uncovered pieces: subtract every same-net conductor overlapping it.
        let mut pieces = vec![r];
        for &s in &touching {
            let q = shapes[s as usize].rect;
            pieces = pieces.into_iter().flat_map(|p| subtract(p, q)).collect();
            if pieces.len() > MAX_PATCH_PIECES {
                return Err(budget("routing patch pieces"));
            }
        }
        stats.patches += 1;
        stats.patch_pieces += pieces.len();
        for piece in pieces {
            let shape = shapes.len() as u32;
            let first = built.runs.len() as u32;
            for vertical in [false, true] {
                let (lo, hi, minus, plus) = if vertical {
                    (piece.ly, piece.uy, piece.lx, piece.ux)
                } else {
                    (piece.lx, piece.ux, piece.ly, piece.uy)
                };
                built.runs.push(Run {
                    net: patch.net,
                    layer: patch.layer,
                    vertical,
                    cross: minus + (plus - minus) / 2,
                    lo,
                    hi,
                    minus,
                    plus,
                    shape,
                    patch: true,
                    node_lo: node,
                    node_hi: node,
                });
            }
            shapes.push(Shape {
                layer: patch.layer,
                rect: piece,
                owner: Owner::Run(first),
            });
            let (x0, x1) = bins.range(piece.lx, piece.ux);
            let (y0, y1) = bins.range(piece.ly, piece.uy);
            let cells =
                usize::try_from((x1 - x0 + 1) * (y1 - y0 + 1)).map_err(|_| budget("bin span"))?;
            *entries = entries
                .checked_add(cells)
                .ok_or_else(|| budget("bin entries"))?;
            if *entries > limits.max_bin_entries {
                return Err(budget("extraction spatial-bin entries"));
            }
            for bx in x0..=x1 {
                for by in y0..=y1 {
                    bins.map
                        .entry((patch.layer, bx, by))
                        .or_default()
                        .push(shape);
                }
            }
        }
    }
    stats.wire_runs = built.runs.iter().filter(|r| !r.patch).count();
    Ok(())
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
pub fn extract_cpu_ref(input: &ExtractInput<'_>, limits: &ExtractLimits) -> Result<RcNetwork> {
    preflight(input, limits)?;
    let (mut built, mut stats) = build_trees(input, limits)?;
    let tech = input.tech;
    // Conductor shapes: runs (with end extension), via pads, passive shapes.
    let mut shapes: Vec<Shape> = Vec::new();
    for (i, r) in built.runs.iter().enumerate() {
        let half = input.layers[r.layer as usize].width / 2;
        let rect = if r.vertical {
            Rect {
                lx: r.minus,
                ux: r.plus,
                ly: r.lo - half,
                uy: r.hi + half,
            }
        } else {
            Rect {
                lx: r.lo - half,
                ux: r.hi + half,
                ly: r.minus,
                uy: r.plus,
            }
        };
        shapes.push(Shape {
            layer: r.layer,
            rect,
            owner: Owner::Run(i as u32),
        });
    }
    shapes.append(&mut built.pads);
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
    stats.shapes = shapes.len();
    if shapes.len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    let routing_layers: Vec<u32> = (0..input.layers.len() as u32)
        .filter(|&l| tech.layers[l as usize].is_some())
        .collect();
    let max_window = routing_layers
        .iter()
        .filter_map(|&l| tech.layers[l as usize].as_ref())
        .flat_map(|m| m.contexts.iter().map(|c| c.rows[c.rows.len() - 1][0]))
        .fold(0.0f64, f64::max);
    let bin_size = ((max_window * tech.dbu_per_um).ceil() as i64).max(1) * 2;
    let mut bins = Bins {
        size: bin_size,
        map: HashMap::new(),
    };
    let mut entries = 0usize;
    for (i, s) in shapes.iter().enumerate() {
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
                    .entry((s.layer, bx, by))
                    .or_default()
                    .push(i as u32);
            }
        }
    }
    let net_index: HashMap<u32, usize> = built
        .nets
        .iter()
        .enumerate()
        .map(|(i, n)| (n.net, i))
        .collect();
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
    stats.shapes = shapes.len();
    if shapes.len() > limits.max_shapes {
        return Err(budget("extraction shape count"));
    }
    let mut coupling = BTreeMap::<((u32, u32), (u32, u32)), f64>::new();
    let mut stamp = vec![u32::MAX; shapes.len()];
    let mut candidates: Vec<u32> = Vec::new();
    let mut points: Vec<i64> = Vec::new();
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
    for (ri, run) in built.runs.iter().enumerate() {
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
        let (bx, by) = if run.vertical {
            (bins.range(c0, c1), bins.range(run.lo, run.hi))
        } else {
            (bins.range(run.lo, run.hi), bins.range(c0, c1))
        };
        candidates.clear();
        for &layer in &routing_layers {
            for x in bx.0..=bx.1 {
                for y in by.0..=by.1 {
                    let Some(list) = bins.map.get(&(layer, x, y)) else {
                        continue;
                    };
                    stats.work = stats
                        .work
                        .checked_add(list.len())
                        .ok_or_else(|| budget("extraction work"))?;
                    if stats.work > limits.max_work {
                        return Err(budget("extraction work"));
                    }
                    for &s in list {
                        if stamp[s as usize] == ri as u32 || s == run.shape {
                            continue;
                        }
                        stamp[s as usize] = ri as u32;
                        let r = shapes[s as usize].rect;
                        let (a0, a1, x0, x1) = if run.vertical {
                            (r.ly, r.uy, r.lx, r.ux)
                        } else {
                            (r.lx, r.ux, r.ly, r.uy)
                        };
                        if a1 <= run.lo || a0 >= run.hi {
                            continue;
                        }
                        let relevant = if layer == run.layer {
                            x1 > c0 && x0 < c1
                        } else {
                            x1 > edge_minus && x0 < edge_plus
                        };
                        if relevant {
                            candidates.push(s);
                        }
                    }
                }
            }
        }
        if candidates.len() > limits.max_candidates {
            return Err(budget("extraction candidates per wire run"));
        }
        candidates.sort_unstable();
        points.clear();
        points.push(run.lo);
        points.push(run.hi);
        for &s in &candidates {
            let r = shapes[s as usize].rect;
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
        let net_local = net_index[&run.net];
        for w in points.windows(2) {
            let (t0, t1) = (w[0], w[1]);
            if t1 <= t0 {
                continue;
            }
            stats.intervals += 1;
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
            let mut below: Option<(u32, i64, u32)> = None; // (metal, overlap, shape)
            let mut above: Option<(u32, i64, u32)> = None;
            for &s in &candidates {
                let sh = &shapes[s as usize];
                let r = sh.rect;
                let (a0, a1, x0, x1) = if run.vertical {
                    (r.ly, r.uy, r.lx, r.ux)
                } else {
                    (r.lx, r.ux, r.ly, r.uy)
                };
                if a0 > t0 || a1 < t1 {
                    continue;
                }
                let same = match sh.owner {
                    Owner::Run(o) => built.runs[o as usize].net == run.net,
                    _ => sh.net() == Some(run.net),
                };
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
                        stats.foreign_overlaps += 1;
                    }
                } else {
                    let metal = layer_model(input, sh.layer)?.metal;
                    let overlap = x1.min(edge_plus) - x0.max(edge_minus);
                    if overlap <= 0 {
                        continue;
                    }
                    if metal < model.metal {
                        let better = below.is_none_or(|(m, o, i)| {
                            (metal, overlap, std::cmp::Reverse(s)) > (m, o, std::cmp::Reverse(i))
                        });
                        if better {
                            below = Some((metal, overlap, s));
                        }
                    } else if metal > model.metal {
                        let better = above.is_none_or(|(m, o, i)| {
                            (std::cmp::Reverse(metal), overlap, std::cmp::Reverse(s))
                                > (std::cmp::Reverse(m), o, std::cmp::Reverse(i))
                        });
                        if better {
                            above = Some((metal, overlap, s));
                        }
                    }
                }
            }
            let over = below.map_or(0, |b| b.0);
            let under = above.map(|a| a.0);
            let exact = ctx_map.get(&(over, under));
            let (ctx, plane_split) = match exact {
                Some(&i) => (i, (below.is_some(), above.is_some())),
                None => {
                    stats.context_fallback_intervals += 1;
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
            for side in [plus, minus] {
                let spacing = side.map(|(g, _)| um(tech, g));
                let touching_same = side.is_some_and(|(g, s)| {
                    g == 0
                        && match shapes[s as usize].owner {
                            Owner::Run(o) => built.runs[o as usize].net == run.net,
                            _ => shapes[s as usize].net() == Some(run.net),
                        }
                });
                if touching_same {
                    continue;
                }
                let (cc, cg) = sample(rows, spacing);
                let (_, cg_sub) = sample(sub, spacing);
                let delta = cg - cg_sub;
                let (d_below, d_above) = match plane_split {
                    (true, true) => {
                        let db = below_only.map_or(delta / 2.0, |r| sample(r, spacing).1 - cg_sub);
                        (db, delta - db)
                    }
                    (true, false) => (delta, 0.0),
                    (false, true) => (0.0, delta),
                    (false, false) => (0.0, 0.0),
                };
                let mut g = cg_sub;
                for (d, plane, upper) in [(d_below, below, false), (d_above, above, true)] {
                    if d < 0.0 {
                        g += d;
                    } else if d > 0.0 {
                        match plane {
                            // A crossing between two runs is owned by the upper
                            // run (its OVER context); the lower run skips it.
                            Some((_, _, s))
                                if upper && matches!(shapes[s as usize].owner, Owner::Run(_)) => {}
                            Some((_, _, s)) => contributions.push((s, d * length, 1.0)),
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
            // Attach to this run's end nodes by position (pi split).
            let rc = &mut built.nets[net_local];
            rc.ground[run.node_lo as usize] += ground * (1.0 - w_hi);
            rc.ground[run.node_hi as usize] += ground * w_hi;
            for (s, value, run_factor) in contributions {
                let sh = shapes[s as usize];
                let partner = match sh.owner {
                    Owner::Run(o) => {
                        let other = &built.runs[o as usize];
                        // A patch strip facing the end cap of a wire run (axes
                        // differ) is not counted from the wire's side: own it.
                        let run_factor = if run.patch
                            && !other.patch
                            && other.vertical != run.vertical
                            && run_factor < 1.0
                        {
                            1.0
                        } else {
                            run_factor
                        };
                        if other.net == run.net {
                            None
                        } else {
                            let ends = if other.vertical {
                                [
                                    [other.cross as f64, other.lo as f64],
                                    [other.cross as f64, other.hi as f64],
                                ]
                            } else {
                                [
                                    [other.lo as f64, other.cross as f64],
                                    [other.hi as f64, other.cross as f64],
                                ]
                            };
                            let d = |p: [f64; 2]| {
                                (p[0] - mid_point[0]).abs() + (p[1] - mid_point[1]).abs()
                            };
                            let node = if d(ends[1]) < d(ends[0]) {
                                other.node_hi
                            } else {
                                other.node_lo
                            };
                            Some(Some((other.net, node, run_factor)))
                        }
                    }
                    Owner::Pad { net, node } => {
                        if net == run.net {
                            None
                        } else {
                            Some(Some((net, node, 1.0)))
                        }
                    }
                    Owner::Passive(PassiveOwner::Ground) => Some(None),
                    Owner::Passive(PassiveOwner::Pin { net, pin }) => {
                        if net == run.net {
                            None
                        } else if let Some(&(n, node)) = built.pin_nodes.get(&pin) {
                            Some(Some((n, node, 1.0)))
                        } else {
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
                }),
                Some(RcLayer {
                    metal: 2,
                    width: 200,
                    sheet_ohm: 0.05,
                    contexts,
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
                tech,
            },
            &ExtractLimits::default(),
        )
    }

    #[test]
    fn isolated_straight_wire_matches_hand_computed_rc() {
        // 10 um of M1, width 0.1 um, sheet 0.1 ohm/sq: R = 0.1 * 10 / 0.1 = 10 ohm.
        // Isolated ground per side 4e-5 pF/um, two sides: C = 2 * 4e-5 * 10 = 8e-4 pF.
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
        assert!((c - 8e-4).abs() < 1e-15, "{c}");
        assert!((net.ground[0] - 4e-4).abs() < 1e-15);
        assert!(n.couplings.is_empty());
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
        // Facing side: cc 4e-5, cg 3e-5; far side isolated cg 4e-5.
        let coupling: f64 = n.couplings.iter().map(|c| c.pf).sum();
        assert!((coupling - 4e-5 * 10.0).abs() < 1e-15, "{coupling}");
        for net in &n.nets {
            let g: f64 = net.ground.iter().sum();
            assert!((g - (3e-5 + 4e-5) * 10.0).abs() < 1e-15, "{g}");
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
        // Only the 0.1 um crossing interval of the upper run couples: on each
        // side OVER-1 ground minus substrate ground (5e-5-3e-5 facing the
        // obstruction at 0.5 um, 6e-5-4e-5 isolated) per um.
        let total: f64 = n.couplings.iter().map(|c| c.pf).sum();
        assert!((total - 4e-6).abs() < 1e-18, "{total}");
        // The lower run has no UNDER table: explicit fallback, no double count.
        assert!(n.stats.context_fallback_intervals > 0);
        // Upper run ground: substrate part both sides plus lateral to obstruction.
        let g: f64 = n.nets[0].ground.iter().sum();
        let want = 10.0 * (3e-5 + 4e-5 + 4e-5) - 0.1 * (4e-5 - 3e-5);
        assert!((g - want).abs() < 1e-15, "{g} {want}");
        assert_eq!(n.stats.foreign_overlaps, 0);
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
        let want = 4e-5 * (10.0 + 9.0) + 4e-5 * (1.0 + 0.2 + 0.2);
        assert!((total - want).abs() < 1e-15, "{total} {want}");
        // Patch centre x = 4.5 um is nearer the driver end (node 0).
        let wire0 = 4e-5 * (4.0 * 0.8 + 0.55 + 5.0 * 0.25) + 4e-5 * (4.0 * 0.8 + 5.0 * 0.25);
        assert!(
            (net.ground[0] - (wire0 + 5.6e-5)).abs() < 1e-15,
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
    fn patch_side_couples_once_and_owns_a_facing_wire_end() {
        // Net 1 parallel to net 0 at y = 750 (edges 0.65 um apart; the patch
        // top is 0.45 um away over 1 um). Lateral pairs count half per side.
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
        let want = 2.8e-5 * 9.0 + 4.75e-5 * 1.0;
        assert!((coupling - want).abs() < 1e-15, "{coupling} {want}");
        assert!(n.couplings.iter().all(|c| c.a.0 != c.b.0));
        // Net 1 wire end cap 0.45 um right of the patch, over 0.1 um of the
        // patch's right edge: no wire side faces it, so the patch owns it.
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
        assert!(extract_cpu_ref(&crowded, &ExtractLimits::default()).is_ok());
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
}
