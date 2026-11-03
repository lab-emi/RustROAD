//! Diode-free process-antenna repair: layer jumpers on routed geometry.
//!
//! A jumper breaks one straight run of routing layer L of a net and bridges
//! the gap on the next layer up: two L/L+1 vias and a short L+1 wire (a
//! non-preferred-direction jog). While layer L is etched the bridge does not
//! exist yet, so the gate-side partial net keeps only the L metal between the
//! gates and the cut; when L+1 is etched the bridge is the only L+1 metal the
//! jumper adds. The caller (the native antenna check) decides which
//! (net, layer) needs a jumper and accepts a candidate only when the net's
//! partial-net ratios improve; this module proposes geometrically legal
//! candidates (nearest the gates first) and applies one.
//!
//! Legality is checked like the router's own validation: other-net clearance
//! of both vias and the bridge in the spatial index (layer and cut spacing),
//! no same-net patch or pin bridging the cut, and the same-net rules (notch
//! spacing, minimum area, holes, floating patches) of the edited net. The
//! full `routing::validate` runs again when the edited routes are committed;
//! the foundry decks remain the DRC and antenna oracle (Rule 8).
//!
//! Bounds (Rule 9): the per-net graph holds O(wire records + vias + access)
//! nodes and edges, Dijkstra is O(E log V), at most `max_positions` cut
//! positions are probed per run and `max_candidates` legal candidates are
//! returned; every spatial probe is charged to `RouteLimits::max_comparisons`
//! and every rule check to the same cap. Memory: the spatial index of the
//! routed design (as in validation) plus O(net size).
use crate::route_rules;
use crate::routing::{
    Blockage, RouteInput, RouteLimits, Routes, Spatial, Via, Wire, budget, distance, invalid,
    shifted, wire_rect,
};
use crate::{Rect, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};

/// One layer jumper: the layer-`layer` metal between the two via points is
/// removed and replaced by a `upper` wire between two `via` instances.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Jumper {
    pub net: u32,
    pub layer: u32,
    pub upper: u32,
    pub via: u32,
    /// Via point on the gate side of the cut (shorter graph distance).
    pub gate_side: [i64; 2],
    pub far_side: [i64; 2],
    /// Graph wire length (DBU, layers up to `layer`) from the nearest gate
    /// access point to `gate_side`.
    pub gate_distance: u64,
    /// Length of the straight `layer` run the jumper cuts (DBU).
    pub run_length: u64,
}

#[derive(Clone, Debug)]
pub struct JumperLimits {
    /// Cut positions probed per straight run.
    pub max_positions: usize,
    /// Legal candidates kept per straight run.
    pub per_run: usize,
    /// Legal candidates returned per call.
    pub max_candidates: usize,
    /// Largest via-to-via distance of a bridge, in upper-layer pitches.
    pub max_gap_pitches: usize,
    /// Run order: longest first (cut the largest contributor) or nearest
    /// the gates first (isolate one gate from the rest of its partial net).
    pub nearest_first: bool,
}
impl Default for JumperLimits {
    fn default() -> Self {
        Self {
            max_positions: 512,
            per_run: 4,
            max_candidates: 32,
            max_gap_pitches: 3,
            nearest_first: false,
        }
    }
}
impl JumperLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_positions == 0
            || self.max_positions > 4096
            || self.per_run == 0
            || self.per_run > 256
            || self.max_candidates == 0
            || self.max_candidates > 4096
            || self.max_gap_pitches == 0
            || self.max_gap_pitches > 16
        {
            return Err(budget("jumper limits outside hard bounds"));
        }
        Ok(())
    }
}

type Node = (u32, i64, i64);
/// Layer key of the per-terminal virtual nodes of the net graph.
const TERMINAL: u32 = u32::MAX;
/// (pin id, routing-layer shapes) of every terminal of one net.
type NetTerminals = Vec<(u32, Vec<(u32, Rect)>)>;

/// Stack rank of every routing layer from the via table (lower -> upper).
fn ranks(input: &RouteInput) -> Result<HashMap<u32, u32>> {
    let mut rank: HashMap<u32, u32> = HashMap::new();
    let routing: Vec<u32> = (0..input.layers.len() as u32)
        .filter(|&l| input.layers[l as usize].vertical.is_some())
        .collect();
    for &l in &routing {
        if !input.vias.iter().any(|v| v.upper == l) {
            rank.insert(l, 0);
        }
    }
    for _ in 0..=routing.len() {
        for v in &input.vias {
            if let Some(&r) = rank.get(&v.lower) {
                let next = r + 1;
                match rank.get(&v.upper) {
                    Some(&u) if u != next => {
                        return Err(invalid("jumper: inconsistent via layer stack"));
                    }
                    _ => {
                        rank.insert(v.upper, next);
                    }
                }
            }
        }
    }
    if routing.iter().any(|l| !rank.contains_key(l)) {
        return Err(invalid("jumper: routing layer outside the via stack"));
    }
    Ok(rank)
}

/// A maximal straight run of layer-L wire edges whose interior nodes carry
/// no via, access point or branch.
#[derive(Clone, Debug)]
struct Run {
    horizontal: bool,
    cross: i64,
    lo: i64,
    hi: i64,
    dist_lo: u64,
    dist_hi: u64,
}

pub struct JumperPlanner<'a> {
    input: &'a RouteInput,
    limits: &'a RouteLimits,
    spatial: Spatial<'a>,
    rank: HashMap<u32, u32>,
    /// Routing-layer terminal shapes per net (pins bridge a cut).
    pins: HashMap<u32, Vec<(u32, Rect)>>,
    /// Per net: (pin id, routing-layer shapes) of every terminal.
    terminals: HashMap<u32, NetTerminals>,
    rule_work: usize,
    /// Present on a planner kept across repair attempts (`persistent`).
    order: Option<Order>,
}

/// Placeholder shape id of a record added by `splice`, indexed by `rebase`.
const PENDING: u32 = u32::MAX;
/// Keys of shapes `commit` adds during an attempt: after every routed key.
const ATTEMPT_KEYS: u64 = 1 << 62;

/// Bookkeeping that keeps a persistent planner's index equal to a fresh
/// `JumperPlanner::new` over the current routes at every attempt start:
/// the shape ids of each wire, via and patch record, every added shape's
/// position in the fresh insertion order, and the shapes to drop.
struct Order {
    /// Spatial work right after construction (the instance index build).
    base_work: usize,
    /// Fresh insertion position + 1 (obstacles: 0) per added shape offset.
    keys: Vec<u64>,
    wire_ids: Vec<u32>,
    /// First shape id of each via (its shapes have consecutive ids).
    via_ids: Vec<u32>,
    patch_ids: Vec<u32>,
    /// Shapes `commit` appended during the current attempt.
    attempt: Vec<usize>,
    /// Shapes of wires cut by spliced jumpers, unlinked on `rebase`.
    cut: Vec<usize>,
    /// Shapes unlinked so far (ids are never reused).
    dead: usize,
}

impl<'a> JumperPlanner<'a> {
    /// Spatial index of the obstacles and every routed shape (as in validation).
    pub fn new(input: &'a RouteInput, routes: &Routes, limits: &'a RouteLimits) -> Result<Self> {
        let mut spatial = Spatial::new(input, limits)?;
        for w in &routes.wires {
            spatial.add_wire(w)?;
        }
        for v in &routes.vias {
            spatial.add_via(v)?;
        }
        for p in &routes.patches {
            spatial.add(Blockage {
                layer: p.layer,
                rect: p.rect,
                net: Some(p.net),
            })?;
        }
        let mut pins: HashMap<u32, Vec<(u32, Rect)>> = HashMap::new();
        let mut terminals: HashMap<u32, NetTerminals> = HashMap::new();
        for t in &input.terminals {
            for s in &t.shapes {
                pins.entry(t.net).or_default().push((s.layer, s.rect));
            }
            terminals
                .entry(t.net)
                .or_default()
                .push((t.pin, t.shapes.iter().map(|s| (s.layer, s.rect)).collect()));
        }
        Ok(Self {
            input,
            limits,
            spatial,
            rank: ranks(input)?,
            pins,
            terminals,
            rule_work: 0,
            order: None,
        })
    }

    /// A planner kept across repair attempts. Each attempt starts with
    /// `begin`; jumpers applied to the routes are reported with `splice` and
    /// `rebase` then makes the index (bin chain order included, so probe work
    /// and with it every budget decision) and work counters equal to a fresh
    /// `new` over the edited routes.
    pub fn persistent(
        input: &'a RouteInput,
        routes: &Routes,
        limits: &'a RouteLimits,
    ) -> Result<Self> {
        let mut planner = Self::new(input, routes, limits)?;
        let base = input.obstacles.len();
        let shapes = planner.spatial.len() - base;
        let mut keys = Vec::with_capacity(shapes);
        keys.extend(1..=shapes as u64);
        let mut next = base as u32;
        let mut take = |n: usize| {
            let id = next;
            next += n as u32;
            id
        };
        let wire_ids = routes.wires.iter().map(|_| take(1)).collect();
        let via_ids = routes
            .vias
            .iter()
            .map(|v| take(input.vias[v.kind as usize].shapes.len()))
            .collect();
        let patch_ids = routes.patches.iter().map(|_| take(1)).collect();
        if next as usize != planner.spatial.len() || planner.spatial.len() >= PENDING as usize {
            return Err(invalid("jumper: persistent planner shape count"));
        }
        planner.order = Some(Order {
            base_work: planner.spatial.charged(),
            keys,
            wire_ids,
            via_ids,
            patch_ids,
            attempt: Vec::new(),
            cut: Vec::new(),
            dead: 0,
        });
        Ok(planner)
    }

    /// Start an attempt with the work counters of a fresh planner.
    pub fn begin(&mut self) -> Result<()> {
        let order = self
            .order
            .as_ref()
            .ok_or_else(|| invalid("jumper: begin needs a persistent planner"))?;
        if !order.attempt.is_empty() || !order.cut.is_empty() {
            return Err(invalid("jumper: attempt not rebased"));
        }
        self.spatial.reset_work(order.base_work);
        self.rule_work = 0;
        Ok(())
    }

    /// Record that `apply_in_place` applied a jumper with this splice to the
    /// routes this planner tracks (its index changes on `rebase`).
    pub fn splice(&mut self, splice: &Splice) -> Result<()> {
        let order = self
            .order
            .as_mut()
            .ok_or_else(|| invalid("jumper: splice needs a persistent planner"))?;
        let mut wires = Vec::with_capacity(order.wire_ids.len() + splice.cuts.len() + 1);
        let mut from = 0;
        for &(index, pieces) in &splice.cuts {
            let id = *order
                .wire_ids
                .get(index)
                .ok_or_else(|| invalid("jumper: splice outside the tracked wires"))?;
            wires.extend_from_slice(&order.wire_ids[from..index]);
            if id != PENDING {
                order.cut.push(id as usize);
            }
            wires.extend(std::iter::repeat_n(PENDING, pieces as usize));
            from = index + 1;
        }
        wires.extend_from_slice(&order.wire_ids[from..]);
        wires.push(PENDING);
        order.wire_ids = wires;
        order.via_ids.extend([PENDING, PENDING]);
        Ok(())
    }

    /// End an attempt: drop the shapes `commit` appended and the cut wires,
    /// and index the spliced records of `routes` at their fresh positions.
    pub fn rebase(&mut self, routes: &Routes) -> Result<()> {
        let order = self
            .order
            .as_mut()
            .ok_or_else(|| invalid("jumper: rebase needs a persistent planner"))?;
        if order.wire_ids.len() != routes.wires.len()
            || order.via_ids.len() != routes.vias.len()
            || order.patch_ids.len() != routes.patches.len()
        {
            return Err(invalid("jumper: rebase routes differ from the splices"));
        }
        for id in order.attempt.drain(..).chain(order.cut.drain(..)) {
            self.spatial.unlink(id)?;
            order.dead += 1;
        }
        // Fresh insertion positions of the surviving shapes (relative order
        // is unchanged by splicing, so every bin chain stays sorted).
        let base = self.input.obstacles.len();
        let mut position = 0u64;
        let mut pending = Vec::new();
        for (i, &id) in order.wire_ids.iter().enumerate() {
            position += 1;
            if id == PENDING {
                pending.push((position, Shape::Wire(i)));
            } else {
                order.keys[id as usize - base] = position;
            }
        }
        for (i, &id) in order.via_ids.iter().enumerate() {
            let shapes = self.input.vias[routes.vias[i].kind as usize].shapes.len();
            if id == PENDING {
                pending.push((position + 1, Shape::Via(i)));
            } else {
                for s in 0..shapes {
                    order.keys[id as usize - base + s] = position + 1 + s as u64;
                }
            }
            position += shapes as u64;
        }
        for &id in &order.patch_ids {
            position += 1;
            order.keys[id as usize - base] = position;
        }
        for (key, shape) in pending {
            let keys = &order.keys;
            let key_of = |id: usize| if id < base { 0 } else { keys[id - base] };
            match shape {
                Shape::Wire(i) => {
                    let w = &routes.wires[i];
                    let b = Blockage {
                        layer: w.layer,
                        rect: wire_rect(w, self.input.layers[w.layer as usize].width)?,
                        net: Some(w.net),
                    };
                    let id = self.spatial.insert_ordered(b, key, &key_of, order.dead)?;
                    order.wire_ids[i] = id as u32;
                    order.keys.push(key);
                }
                Shape::Via(i) => {
                    let v = &routes.vias[i];
                    for (s, shape) in self.input.vias[v.kind as usize].shapes.iter().enumerate() {
                        let keys = &order.keys;
                        let key_of = |id: usize| if id < base { 0 } else { keys[id - base] };
                        let b = Blockage {
                            layer: shape.layer,
                            rect: shifted(shape.rect, v.at)?,
                            net: Some(v.net),
                        };
                        let id =
                            self.spatial
                                .insert_ordered(b, key + s as u64, &key_of, order.dead)?;
                        if s == 0 {
                            order.via_ids[i] = id as u32;
                        }
                        order.keys.push(key + s as u64);
                    }
                }
            }
        }
        if self.spatial.len() >= PENDING as usize {
            return Err(budget("jumper: persistent planner shape ids"));
        }
        Ok(())
    }

    /// Does this planner's index equal a fresh `new` over `routes` (every bin
    /// chain in order, entry count and starting work)? A diagnostic for
    /// persistent planners right after `rebase`; cost O(routes).
    pub fn matches_fresh(&self, routes: &Routes) -> Result<bool> {
        let fresh = Self::new(self.input, routes, self.limits)?;
        let base = self.order.as_ref().map(|o| o.base_work);
        Ok(self.spatial.dump_bins() == fresh.spatial.dump_bins()
            && base.is_none_or(|w| w == fresh.spatial.charged()))
    }

    /// Spatial and rule-check work charged so far.
    pub fn work(&self) -> usize {
        self.spatial.work.saturating_add(self.rule_work)
    }

    /// The layer a jumper over `layer` uses and its via kinds (input order).
    pub fn upper_of(&self, layer: u32) -> Option<(u32, Vec<u32>)> {
        let kinds: Vec<u32> = self
            .input
            .vias
            .iter()
            .enumerate()
            .filter(|(_, v)| v.lower == layer)
            .map(|(k, _)| k as u32)
            .collect();
        let upper = self.input.vias.get(*kinds.first()? as usize)?.upper;
        Some((upper, kinds))
    }

    /// Legal jumper candidates over `layer` of `net`: per run the positions
    /// nearest `gate_pins` first; runs longest first, or nearest the gates
    /// first with `JumperLimits::nearest_first`. Empty when the layer has no
    /// layer above it or no run of the gates' partial net can take a jumper.
    pub fn candidates(
        &mut self,
        routes: &Routes,
        net: u32,
        layer: u32,
        gate_pins: &[u32],
        jl: &JumperLimits,
    ) -> Result<Vec<Jumper>> {
        jl.validate()?;
        let Some((upper, kinds)) = self.upper_of(layer) else {
            return Ok(Vec::new());
        };
        if self
            .input
            .layers
            .get(layer as usize)
            .and_then(|l| l.vertical)
            .is_none()
        {
            return Err(invalid("jumper: cut layer is not a routing layer"));
        }
        let top = self.rank[&layer];
        let below = |l: u32, rank: &HashMap<u32, u32>| rank.get(&l).is_some_and(|&r| r <= top);
        // Per-net graph on layers at or below `layer`.
        let wires: Vec<&Wire> = routes.wires.iter().filter(|w| w.net == net).collect();
        let vias: Vec<&Via> = routes.vias.iter().filter(|v| v.net == net).collect();
        let mut lines: BTreeMap<(u32, bool, i64), BTreeSet<i64>> = BTreeMap::new();
        let mut put = |l: u32, p: [i64; 2]| {
            lines.entry((l, true, p[1])).or_default().insert(p[0]);
            lines.entry((l, false, p[0])).or_default().insert(p[1]);
        };
        // Points where something other than a straight layer wire attaches.
        let mut anchored: BTreeSet<Node> = BTreeSet::new();
        for w in &wires {
            put(w.layer, w.start);
            put(w.layer, w.end);
        }
        for v in &vias {
            let spec = &self.input.vias[v.kind as usize];
            for l in [spec.lower, spec.upper] {
                put(l, v.at);
                anchored.insert((l, v.at[0], v.at[1]));
            }
        }
        let gates: BTreeSet<u32> = gate_pins.iter().copied().collect();
        let mut access_at: HashMap<u32, Node> = HashMap::new();
        for a in routes.access.iter().filter(|a| a.net == net) {
            put(a.layer, a.at);
            anchored.insert((a.layer, a.at[0], a.at[1]));
            access_at.insert(a.pin, (a.layer, a.at[0], a.at[1]));
        }
        // One virtual node per terminal (layer TERMINAL, x = pin id): joined
        // to its access point and to every node inside its pin shapes, as
        // the pin metal connects them; the gates' virtual nodes are seeds.
        let terminals: NetTerminals = self.terminals.get(&net).cloned().unwrap_or_default();
        let seeds: Vec<Node> = terminals
            .iter()
            .filter(|(pin, _)| gates.contains(pin))
            .map(|(pin, _)| (TERMINAL, i64::from(*pin), 0))
            .collect();
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let mut index: HashMap<Node, usize> = HashMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut id = |n: Node, nodes: &mut Vec<Node>| -> usize {
            *index.entry(n).or_insert_with(|| {
                nodes.push(n);
                nodes.len() - 1
            })
        };
        let mut edges: BTreeSet<(usize, usize, u64)> = BTreeSet::new();
        // Layer-L wire edges: (line, lo, hi).
        let mut layer_edges: BTreeSet<(bool, i64, i64, i64)> = BTreeSet::new();
        for w in &wires {
            if !below(w.layer, &self.rank) {
                continue;
            }
            let horizontal = w.start[1] == w.end[1];
            let (cross, a, b) = if horizontal {
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
            let points: Vec<i64> = lines[&(w.layer, horizontal, cross)]
                .range(a..=b)
                .copied()
                .collect();
            self.charge(points.len())?;
            for pair in points.windows(2) {
                let at = |t: i64| -> Node {
                    if horizontal {
                        (w.layer, t, cross)
                    } else {
                        (w.layer, cross, t)
                    }
                };
                let (u, v) = (id(at(pair[0]), &mut nodes), id(at(pair[1]), &mut nodes));
                edges.insert((u.min(v), u.max(v), (pair[1] - pair[0]) as u64));
                if w.layer == layer {
                    layer_edges.insert((horizontal, cross, pair[0], pair[1]));
                }
            }
        }
        for v in &vias {
            let spec = &self.input.vias[v.kind as usize];
            if below(spec.upper, &self.rank) {
                let a = id((spec.lower, v.at[0], v.at[1]), &mut nodes);
                let b = id((spec.upper, v.at[0], v.at[1]), &mut nodes);
                edges.insert((a.min(b), a.max(b), 0));
            }
        }
        // Terminal joins (after the wire and via nodes exist).
        let routed_nodes = nodes.len();
        for (pin, shapes) in &terminals {
            let v = id((TERMINAL, i64::from(*pin), 0), &mut nodes);
            if let Some(&a) = access_at.get(pin)
                && below(a.0, &self.rank)
            {
                let u = id(a, &mut nodes);
                edges.insert((u.min(v), u.max(v), 0));
            }
            self.charge(routed_nodes.saturating_mul(shapes.len()))?;
            for (n, &node) in nodes.iter().enumerate().take(routed_nodes) {
                let (l, x, y) = node;
                if shapes
                    .iter()
                    .any(|&(sl, r)| sl == l && r.lx <= x && x <= r.ux && r.ly <= y && y <= r.uy)
                {
                    anchored.insert(node);
                    edges.insert((n.min(v), n.max(v), 0));
                }
            }
        }
        let mut adj: Vec<Vec<(usize, u64)>> = vec![Vec::new(); nodes.len()];
        for &(a, b, d) in &edges {
            adj[a].push((b, d));
            adj[b].push((a, d));
        }
        // Dijkstra from the gate access points.
        let mut dist = vec![u64::MAX; nodes.len()];
        let mut heap = BinaryHeap::new();
        for s in &seeds {
            let i = index[s];
            dist[i] = 0;
            heap.push(Reverse((0u64, i)));
        }
        while let Some(Reverse((d, i))) = heap.pop() {
            if d > dist[i] {
                continue;
            }
            self.charge(adj[i].len().max(1))?;
            for &(j, w) in &adj[i] {
                let nd = d.saturating_add(w);
                if nd < dist[j] {
                    dist[j] = nd;
                    heap.push(Reverse((nd, j)));
                }
            }
        }
        // Layer-L degree per node and straight runs through plain nodes.
        let mut degree: HashMap<Node, (usize, usize)> = HashMap::new();
        for &(horizontal, cross, a, b) in &layer_edges {
            for t in [a, b] {
                let n = if horizontal {
                    (layer, t, cross)
                } else {
                    (layer, cross, t)
                };
                let e = degree.entry(n).or_default();
                if horizontal {
                    e.0 += 1;
                } else {
                    e.1 += 1;
                }
            }
        }
        let plain = |n: Node, horizontal: bool| -> bool {
            !anchored.contains(&n)
                && degree.get(&n).copied() == Some(if horizontal { (2, 0) } else { (0, 2) })
        };
        let mut runs: Vec<Run> = Vec::new();
        let mut current: Option<Run> = None;
        for &(horizontal, cross, a, b) in &layer_edges {
            let node = |t: i64| -> Node {
                if horizontal {
                    (layer, t, cross)
                } else {
                    (layer, cross, t)
                }
            };
            let joins = current.as_ref().is_some_and(|r| {
                r.horizontal == horizontal
                    && r.cross == cross
                    && r.hi == a
                    && plain(node(a), horizontal)
            });
            if joins {
                current.as_mut().unwrap().hi = b;
                continue;
            }
            if let Some(r) = current.take() {
                runs.push(r);
            }
            current = Some(Run {
                horizontal,
                cross,
                lo: a,
                hi: b,
                dist_lo: 0,
                dist_hi: 0,
            });
        }
        runs.extend(current);
        for r in &mut runs {
            let at = |t: i64| -> Node {
                if r.horizontal {
                    (layer, t, r.cross)
                } else {
                    (layer, r.cross, t)
                }
            };
            r.dist_lo = index.get(&at(r.lo)).map_or(u64::MAX, |&i| dist[i]);
            r.dist_hi = index.get(&at(r.hi)).map_or(u64::MAX, |&i| dist[i]);
        }
        // Runs of the gates' partial net, longest first.
        runs.retain(|r| r.dist_lo != u64::MAX && r.dist_hi != u64::MAX);
        if jl.nearest_first {
            runs.sort_by_key(|r| {
                (
                    r.dist_lo.min(r.dist_hi),
                    Reverse(r.hi - r.lo),
                    r.horizontal,
                    r.cross,
                    r.lo,
                )
            });
        } else {
            runs.sort_by_key(|r| {
                (
                    Reverse(r.hi - r.lo),
                    r.dist_lo.min(r.dist_hi),
                    r.horizontal,
                    r.cross,
                    r.lo,
                )
            });
        }
        let upper_vertical = self.input.layers[upper as usize]
            .vertical
            .ok_or_else(|| invalid("jumper: upper layer is not a routing layer"))?;
        let before_net = net_only(routes, net);
        let before: BTreeSet<String> = if self.input.rules {
            let mut work = 0usize;
            let v = route_rules::check(self.input, &before_net, self.limits, &mut work)?;
            self.charge(work)?;
            v.iter().map(|v| v.describe()).collect()
        } else {
            BTreeSet::new()
        };
        let trace = std::env::var_os("RUSTROAD_JUMPER_TRACE").is_some();
        if trace {
            eprintln!(
                "[jumper] net {net} layer {layer}: {} seeds, {} graph nodes, {} runs {:?}",
                seeds.len(),
                nodes.len(),
                runs.len(),
                runs.iter()
                    .take(8)
                    .map(|r| (r.horizontal, r.cross, r.lo, r.hi, r.dist_lo, r.dist_hi))
                    .collect::<Vec<_>>()
            );
        }
        let mut out = Vec::new();
        for r in &runs {
            if out.len() >= jl.max_candidates {
                break;
            }
            // Positions along the run: the upper layer's tracks when they
            // cross the run, else the router lattice.
            let (pitch, offset) = match self
                .input
                .tracks
                .as_ref()
                .and_then(|t| t.tracks[upper as usize])
            {
                Some((p, o)) if upper_vertical == r.horizontal => (p, o),
                _ => {
                    let g = self.input.grid;
                    let origin = if r.horizontal {
                        self.input.die.lx
                    } else {
                        self.input.die.ly
                    };
                    (self.input.step, origin.div_euclid(g) * g)
                }
            };
            if pitch <= 0 {
                return Err(invalid("jumper: non-positive pitch"));
            }
            let gate_lo = r.dist_lo <= r.dist_hi;
            let first = offset + (r.lo - offset).div_euclid(pitch) * pitch;
            let mut positions: Vec<i64> = Vec::new();
            let mut t = first;
            while t < r.hi {
                if t > r.lo {
                    positions.push(t);
                }
                t += pitch;
                self.charge(1)?;
            }
            if !gate_lo {
                positions.reverse();
            }
            let mut kept = 0;
            for &p in positions.iter().take(jl.max_positions) {
                if kept >= jl.per_run || out.len() >= jl.max_candidates {
                    break;
                }
                for k in 1..=jl.max_gap_pitches as i64 {
                    let q = if gate_lo {
                        p + k * pitch
                    } else {
                        p - k * pitch
                    };
                    if q <= r.lo || q >= r.hi {
                        break;
                    }
                    let point = |t: i64| -> [i64; 2] {
                        if r.horizontal {
                            [t, r.cross]
                        } else {
                            [r.cross, t]
                        }
                    };
                    let gate_distance = if gate_lo {
                        r.dist_lo.saturating_add((p - r.lo) as u64)
                    } else {
                        r.dist_hi.saturating_add((r.hi - p) as u64)
                    };
                    let mut found = None;
                    for &kind in &kinds {
                        let j = Jumper {
                            net,
                            layer,
                            upper,
                            via: kind,
                            gate_side: point(p),
                            far_side: point(q),
                            gate_distance,
                            run_length: (r.hi - r.lo) as u64,
                        };
                        let ok = self.legal(routes, &before_net, &before, &j)?;
                        if trace {
                            eprintln!(
                                "[jumper]   {:?} -> {:?} via {kind}: legal {ok}",
                                j.gate_side, j.far_side
                            );
                        }
                        if ok {
                            found = Some(j);
                            break;
                        }
                    }
                    if let Some(j) = found {
                        out.push(j);
                        kept += 1;
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    fn charge(&mut self, n: usize) -> Result<()> {
        self.rule_work = self.rule_work.saturating_add(n);
        if self.work() > self.limits.max_comparisons {
            return Err(budget("jumper work (max_comparisons)"));
        }
        Ok(())
    }

    /// Other-net clearance, no same-net pin or patch across the cut, and the
    /// same-net rules of the edited net.
    fn legal(
        &mut self,
        routes: &Routes,
        before_net: &Routes,
        before: &BTreeSet<String>,
        j: &Jumper,
    ) -> Result<bool> {
        let input = self.input;
        let bridge = Wire {
            net: j.net,
            layer: j.upper,
            start: j.gate_side,
            end: j.far_side,
        };
        let bridge_rect = wire_rect(&bridge, input.layers[j.upper as usize].width)?;
        let trace = std::env::var_os("RUSTROAD_JUMPER_TRACE").is_some();
        let a = self
            .spatial
            .via_probe(j.via as usize, j.gate_side, j.net)?
            .is_none();
        let b = self
            .spatial
            .via_probe(j.via as usize, j.far_side, j.net)?
            .is_none();
        let c = self.spatial.probe(j.upper, bridge_rect, j.net)?.is_none();
        if a || b || c {
            if trace {
                eprintln!("[jumper]     blocked: gate via {a} far via {b} bridge {c}");
            }
            return Ok(false);
        }
        // The removed metal: a same-net pin or patch there keeps the
        // partial net connected (and would leave a stub).
        let gap = wire_rect(
            &Wire {
                net: j.net,
                layer: j.layer,
                start: j.gate_side,
                end: j.far_side,
            },
            input.layers[j.layer as usize].width,
        )?;
        let hit = |r: Rect| r.lx < gap.ux && gap.lx < r.ux && r.ly < gap.uy && gap.ly < r.uy;
        if self
            .pins
            .get(&j.net)
            .is_some_and(|v| v.iter().any(|&(l, r)| l == j.layer && hit(r)))
            || routes
                .patches
                .iter()
                .any(|p| p.net == j.net && p.layer == j.layer && hit(p.rect))
        {
            if trace {
                eprintln!("[jumper]     same-net pin or patch across the cut");
            }
            return Ok(false);
        }
        if !input.rules {
            return Ok(true);
        }
        let after_net = apply(input, before_net, j)?;
        let mut work = 0usize;
        let after = route_rules::check(input, &after_net, self.limits, &mut work)?;
        self.charge(work)?;
        if trace {
            for v in after.iter().filter(|v| !before.contains(&v.describe())) {
                eprintln!("[jumper]     rule: {}", v.describe());
            }
        }
        Ok(after.iter().all(|v| before.contains(&v.describe())))
    }

    /// Record an accepted jumper's new shapes for later probes.
    pub fn commit(&mut self, j: &Jumper) -> Result<()> {
        let Some(order) = &mut self.order else {
            for at in [j.gate_side, j.far_side] {
                self.spatial.add_via(&Via {
                    net: j.net,
                    kind: j.via,
                    at,
                })?;
            }
            return self.spatial.add_wire(&Wire {
                net: j.net,
                layer: j.upper,
                start: j.gate_side,
                end: j.far_side,
            });
        };
        // Appended after every routed shape, as a fresh planner's `add` does;
        // the cut wire stays indexed until `rebase`, as it did there.
        let base = self.input.obstacles.len();
        let mut shapes: Vec<Blockage> = Vec::new();
        for at in [j.gate_side, j.far_side] {
            for s in &self.input.vias[j.via as usize].shapes {
                shapes.push(Blockage {
                    layer: s.layer,
                    rect: shifted(s.rect, at)?,
                    net: Some(j.net),
                });
            }
        }
        shapes.push(Blockage {
            layer: j.upper,
            rect: wire_rect(
                &Wire {
                    net: j.net,
                    layer: j.upper,
                    start: j.gate_side,
                    end: j.far_side,
                },
                self.input.layers[j.upper as usize].width,
            )?,
            net: Some(j.net),
        });
        for b in shapes {
            let key = ATTEMPT_KEYS + order.attempt.len() as u64;
            let keys = &order.keys;
            let key_of = |id: usize| if id < base { 0 } else { keys[id - base] };
            let id = self.spatial.insert_ordered(b, key, &key_of, order.dead)?;
            order.keys.push(key);
            order.attempt.push(id);
        }
        Ok(())
    }
}

enum Shape {
    Wire(usize),
    Via(usize),
}

/// Wire records a jumper replaced: (index before the edit, pieces kept).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Splice {
    pub cuts: Vec<(usize, u8)>,
}

/// The routes of one net (no access records; rule checks only).
fn net_only(r: &Routes, net: u32) -> Routes {
    Routes {
        wires: r.wires.iter().filter(|w| w.net == net).cloned().collect(),
        vias: r.vias.iter().filter(|v| v.net == net).cloned().collect(),
        access: Vec::new(),
        patches: r.patches.iter().filter(|p| p.net == net).cloned().collect(),
        nets: vec![net],
        stats: Default::default(),
    }
}

/// The routes with `j` applied: layer-`j.layer` wire records of the net on
/// the jumper's line lose the open interval between the via points; the two
/// vias and the bridge are appended. Telemetry wire length is recomputed.
pub fn apply(input: &RouteInput, routes: &Routes, j: &Jumper) -> Result<Routes> {
    let mut out = routes.clone();
    apply_in_place(input, &mut out, j)?;
    Ok(out)
}

/// `apply` without copying the other records: the same edit, made in place
/// (unchanged on error), with the replaced wire records.
pub fn apply_in_place(input: &RouteInput, routes: &mut Routes, j: &Jumper) -> Result<Splice> {
    let horizontal = j.gate_side[1] == j.far_side[1];
    if horizontal == (j.gate_side[0] == j.far_side[0]) {
        return Err(invalid("jumper: via points must differ along one axis"));
    }
    let (cross, a, b) = if horizontal {
        (j.gate_side[1], j.gate_side[0], j.far_side[0])
    } else {
        (j.gate_side[0], j.gate_side[1], j.far_side[1])
    };
    let (lo, hi) = (a.min(b), a.max(b));
    let spec = input
        .vias
        .get(j.via as usize)
        .ok_or_else(|| invalid("jumper: via kind outside the technology"))?;
    if spec.lower != j.layer || spec.upper != j.upper {
        return Err(invalid("jumper: via kind does not join the jumper layers"));
    }
    let point = |t: i64| -> [i64; 2] { if horizontal { [t, cross] } else { [cross, t] } };
    let mut splice = Splice::default();
    let mut wires: Vec<Wire> = Vec::with_capacity(routes.wires.len() + 2);
    let mut removed = 0i64;
    for (index, w) in routes.wires.iter().enumerate() {
        let on_line = w.net == j.net
            && w.layer == j.layer
            && if horizontal {
                w.start[1] == cross && w.end[1] == cross && w.start[0] != w.end[0]
            } else {
                w.start[0] == cross && w.end[0] == cross && w.start[1] != w.end[1]
            };
        let (s, e) = if horizontal {
            (w.start[0].min(w.end[0]), w.start[0].max(w.end[0]))
        } else {
            (w.start[1].min(w.end[1]), w.start[1].max(w.end[1]))
        };
        if !on_line || e <= lo || s >= hi {
            wires.push(w.clone());
            continue;
        }
        removed += e.min(hi) - s.max(lo);
        let mut pieces = 0u8;
        if s < lo {
            wires.push(Wire {
                net: w.net,
                layer: w.layer,
                start: point(s),
                end: point(lo),
            });
            pieces += 1;
        }
        if e > hi {
            wires.push(Wire {
                net: w.net,
                layer: w.layer,
                start: point(hi),
                end: point(e),
            });
            pieces += 1;
        }
        splice.cuts.push((index, pieces));
    }
    if removed < hi - lo {
        return Err(invalid(
            "jumper: the cut interval is not covered by the net's wire",
        ));
    }
    wires.push(Wire {
        net: j.net,
        layer: j.upper,
        start: j.gate_side,
        end: j.far_side,
    });
    let length = wires.iter().try_fold(0u64, |n, w| {
        n.checked_add(distance(w.start, w.end)?)
            .ok_or_else(|| budget("wire length overflow"))
    })?;
    // Shapes must stay on the manufacturing grid.
    if [j.gate_side, j.far_side]
        .iter()
        .flatten()
        .any(|x| x % input.grid != 0)
    {
        return Err(invalid("jumper: via point off the manufacturing grid"));
    }
    routes
        .vias
        .try_reserve(2)
        .map_err(|_| budget("jumper via allocation"))?;
    for at in [j.gate_side, j.far_side] {
        routes.vias.push(Via {
            net: j.net,
            kind: j.via,
            at,
        });
    }
    routes.wires = wires;
    routes.stats.wire_length_dbu = length;
    Ok(splice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{LayerRect, RouteLayer, RouteVia, Terminal, route_cpu_ref, validate};

    fn rect(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    fn layer(name: &str, vertical: Option<bool>) -> RouteLayer {
        RouteLayer {
            name: name.into(),
            width: 2,
            spacing: 2,
            vertical,
            min_area: None,
            min_step: None,
            wide_spacing: None,
        }
    }
    /// One net: a driver pin at x = 10 and a gate pin at x = 90 on M1.
    fn input(blocked_m2: Option<Rect>) -> RouteInput {
        let pin = |p: u32, x: i64| Terminal {
            pin: p,
            net: 0,
            driver: p == 0,
            shapes: vec![LayerRect {
                layer: 0,
                rect: rect(x - 1, 49, x + 1, 51),
            }],
            class: None,
        };
        RouteInput {
            die: rect(0, 0, 100, 100),
            grid: 1,
            step: 10,
            access_layer: 0,
            layers: vec![
                layer("M1", Some(false)),
                layer("M2", Some(true)),
                layer("V1", None),
            ],
            vias: vec![RouteVia {
                name: "V12".into(),
                lower: 0,
                upper: 1,
                shapes: (0..3)
                    .map(|layer| LayerRect {
                        layer,
                        rect: rect(-1, -1, 1, 1),
                    })
                    .collect(),
            }],
            obstacles: blocked_m2
                .map(|rect| Blockage {
                    layer: 1,
                    rect,
                    net: Some(7),
                })
                .into_iter()
                .collect(),
            terminals: vec![pin(0, 10), pin(1, 90)],
            rules: false,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }

    /// Union of the M1 wire intervals along x.
    fn m1_metal(routes: &Routes) -> Vec<(i64, i64)> {
        let mut v: Vec<_> = routes
            .wires
            .iter()
            .filter(|w| w.layer == 0)
            .map(|w| (w.start[0].min(w.end[0]), w.start[0].max(w.end[0])))
            .collect();
        v.sort();
        let mut out: Vec<(i64, i64)> = Vec::new();
        for (a, b) in v {
            match out.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        out
    }

    /// Nets 0..n on M1 rows: driver pin at x0 + 10, gate pin at x0 + 90,
    /// hand-made straight routes with access points; `obstacles` far away
    /// M2 squares select the paged bin mode on a large die.
    fn rows(die: i64, nets: u32, obstacles: usize) -> (RouteInput, Routes) {
        let mut input = input(None);
        input.die = rect(0, 0, die, die);
        input.terminals.clear();
        let mut routes = Routes {
            wires: Vec::new(),
            vias: Vec::new(),
            access: Vec::new(),
            patches: Vec::new(),
            nets: (0..nets).collect(),
            stats: Default::default(),
        };
        for net in 0..nets {
            let (x0, y) = (i64::from(net % 3) * 120, 20 + i64::from(net / 3) * 30);
            for (k, x) in [(0, x0 + 10), (1, x0 + 90)] {
                let pin = net * 2 + k;
                input.terminals.push(Terminal {
                    pin,
                    net,
                    driver: k == 0,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: rect(x - 1, y - 1, x + 1, y + 1),
                    }],
                    class: None,
                });
                routes.access.push(crate::routing::Access {
                    pin,
                    net,
                    layer: 0,
                    at: [x, y],
                });
            }
            // Two collinear records (a later cut may hit either).
            routes.wires.push(Wire {
                net,
                layer: 0,
                start: [x0 + 10, y],
                end: [x0 + 50, y],
            });
            routes.wires.push(Wire {
                net,
                layer: 0,
                start: [x0 + 50, y],
                end: [x0 + 90, y],
            });
        }
        for i in 0..obstacles {
            let x = die - 50 - (i as i64 % 100) * 40;
            let y = die - 50 - (i as i64 / 100) * 40;
            input.obstacles.push(Blockage {
                layer: 1,
                rect: rect(x, y, x + 4, y + 4),
                net: Some(1000),
            });
        }
        routes.stats.wire_length_dbu = routes
            .wires
            .iter()
            .map(|w| distance(w.start, w.end).unwrap())
            .sum();
        (input, routes)
    }

    /// A persistent planner reproduces a fresh per-attempt planner exactly:
    /// candidates and charged work within attempts (with commits), and an
    /// index equal to a fresh build after every rebase, in all bin modes.
    #[test]
    fn persistent_planner_matches_a_fresh_planner_every_attempt() {
        let limits = RouteLimits::default();
        // Dense bins; sparse bins (large die); paged bins (plus obstacles).
        for (die, obstacles) in [(400, 0), (200_000, 0), (200_000, 1100)] {
            let (input, mut routes) = rows(die, 9, obstacles);
            let mut persistent = JumperPlanner::persistent(&input, &routes, &limits).unwrap();
            assert!(persistent.matches_fresh(&routes).unwrap());
            let jl = JumperLimits {
                max_gap_pitches: 1,
                ..JumperLimits::default()
            };
            for attempt in 0..4u32 {
                persistent.begin().unwrap();
                let mut fresh = JumperPlanner::new(&input, &routes, &limits).unwrap();
                let mut trial = routes.clone();
                let mut accepted = Vec::new();
                for k in 0..3u32 {
                    let net = (attempt * 2 + k) % 9;
                    let gate = [net * 2 + 1];
                    let a = persistent.candidates(&trial, net, 0, &gate, &jl).unwrap();
                    let b = fresh.candidates(&trial, net, 0, &gate, &jl).unwrap();
                    assert_eq!(a, b, "die {die} attempt {attempt} net {net}");
                    assert_eq!(persistent.work(), fresh.work());
                    // Commit one jumper per net; twice on the first net.
                    for c in a.iter().take(if k == 0 { 2 } else { 1 }) {
                        let Ok(next) = apply(&input, &trial, c) else {
                            continue;
                        };
                        persistent.commit(c).unwrap();
                        fresh.commit(c).unwrap();
                        trial = next;
                        accepted.push(c.clone());
                    }
                }
                assert!(!accepted.is_empty());
                for c in &accepted {
                    let splice = apply_in_place(&input, &mut routes, c).unwrap();
                    persistent.splice(&splice).unwrap();
                }
                assert_eq!(routes.wires, trial.wires);
                assert_eq!(routes.vias, trial.vias);
                assert_eq!(routes.stats.wire_length_dbu, trial.stats.wire_length_dbu);
                persistent.rebase(&routes).unwrap();
                assert!(persistent.matches_fresh(&routes).unwrap(), "die {die}");
            }
        }
    }

    #[test]
    fn jumper_cuts_the_run_next_to_the_gate_and_validates() {
        let limits = RouteLimits::default();
        let input = input(None);
        let routes = route_cpu_ref(&input, &limits).unwrap();
        // The router joins the pins with one straight M1 run at y = 50.
        // The router joins the pins with one straight M1 run at y = 49.
        assert!(
            routes
                .wires
                .iter()
                .all(|w| w.layer == 0 && w.start[1] == 49)
        );
        let mut planner = JumperPlanner::new(&input, &routes, &limits).unwrap();
        let c = planner
            .candidates(&routes, 0, 0, &[1], &JumperLimits::default())
            .unwrap();
        // Nearest the gate (x = 90) first, on the lattice, one step apart.
        assert_eq!((c[0].gate_side, c[0].far_side), ([80, 49], [70, 49]));
        assert_eq!((c[0].layer, c[0].upper, c[0].gate_distance), (0, 1, 9));
        // The run starts at x = 10, the last node inside the driver pin.
        assert_eq!(c[0].run_length, 79);
        let out = apply(&input, &routes, &c[0]).unwrap();
        validate(&input, &out, &limits).unwrap();
        assert_eq!(m1_metal(&routes), vec![(9, 89)]);
        assert_eq!(m1_metal(&out), vec![(9, 70), (80, 89)]);
        assert_eq!(out.stats.wire_length_dbu, routes.stats.wire_length_dbu);
        assert_eq!(out.vias.len(), routes.vias.len() + 2);
        assert!(
            out.wires
                .iter()
                .any(|w| w.layer == 1 && w.start == [80, 49] && w.end == [70, 49])
        );
        // The driver as the only "gate" moves the cut to the other end; the
        // cut 10..20 would leave the pin (9..11) bridging it.
        let d = planner
            .candidates(&routes, 0, 0, &[0], &JumperLimits::default())
            .unwrap();
        assert_eq!((d[0].gate_side, d[0].far_side), ([20, 49], [30, 49]));
        // No layer above M2: no candidates; no gate on the net: none.
        assert!(
            planner
                .candidates(&routes, 0, 1, &[1], &JumperLimits::default())
                .unwrap()
                .is_empty()
        );
        assert!(
            planner
                .candidates(&routes, 0, 0, &[9], &JumperLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn other_net_metal_on_the_bridge_layer_moves_the_jumper() {
        let limits = RouteLimits::default();
        let input = input(Some(rect(74, 40, 84, 60)));
        let routes = route_cpu_ref(&input, &limits).unwrap();
        let mut planner = JumperPlanner::new(&input, &routes, &limits).unwrap();
        let c = planner
            .candidates(&routes, 0, 0, &[1], &JumperLimits::default())
            .unwrap();
        // A via at x = 80 overlaps the M2 blockage (74..84); x = 70 keeps
        // its spacing (pad 69..71).
        assert_eq!((c[0].gate_side, c[0].far_side), ([70, 49], [60, 49]));
        let out = apply(&input, &routes, &c[0]).unwrap();
        validate(&input, &out, &limits).unwrap();
        // On the edited routes the gate's M1 partial net is the run 70..89
        // only (the bridge is above M1): no lattice pair fits inside it.
        planner.commit(&c[0]).unwrap();
        assert!(
            planner
                .candidates(&out, 0, 0, &[1], &JumperLimits::default())
                .unwrap()
                .is_empty()
        );
        // The driver's partial net is 9..60: its candidates stay inside it.
        let d = planner
            .candidates(&out, 0, 0, &[0], &JumperLimits::default())
            .unwrap();
        assert!(!d.is_empty());
        for j in &d {
            let (a, b) = (
                j.gate_side[0].min(j.far_side[0]),
                j.gate_side[0].max(j.far_side[0]),
            );
            assert!(a > 9 && b < 60, "{j:?}");
        }
        // Bounds are explicit.
        let bad = JumperLimits {
            max_candidates: 0,
            ..JumperLimits::default()
        };
        assert!(planner.candidates(&routes, 0, 0, &[1], &bad).is_err());
    }
}
