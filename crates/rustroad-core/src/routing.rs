//! Deterministic, bounded rectilinear routing on a finite coordinate lattice.
//! This compute view is separate from mutable netlist arenas. All geometry is
//! integer DBU. Resource preflight: docs/routing-budget.md.
use crate::{Error, Rect, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

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
    pub max_comparisons: usize,
    pub max_heap: usize,
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
            max_comparisons: 200_000_000,
            max_heap: 500_000,
        }
    }
}
impl RouteLimits {
    pub fn validate(&self) -> Result<()> {
        let hard = Self::default();
        for (a, b) in [
            (self.max_axis, hard.max_axis),
            (self.max_grid_nodes, hard.max_grid_nodes),
            (self.max_obstacles, hard.max_obstacles),
            (self.max_bin_entries, hard.max_bin_entries),
            (self.max_segments, hard.max_segments),
            (self.max_expansions, hard.max_expansions),
            (self.max_search_expansions, hard.max_search_expansions),
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
}
pub struct RouteInput {
    pub die: Rect,
    pub grid: i64,
    pub step: i64,
    pub access_layer: u32,
    pub layers: Vec<RouteLayer>,
    pub vias: Vec<RouteVia>,
    pub obstacles: Vec<Blockage>,
    pub terminals: Vec<Terminal>,
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
    pub nets: Vec<u32>,
    pub stats: RouteStats,
}
fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn no_route(s: &str) -> Error {
    Error::Invalid(format!("NO_ROUTE: {s}"))
}
fn int(v: i128) -> Result<i64> {
    i64::try_from(v).map_err(|_| invalid("routing coordinate overflow"))
}
fn expand(r: Rect, amount: i64) -> Result<Rect> {
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
fn contains(r: Rect, p: [i64; 2]) -> bool {
    r.lx <= p[0] && p[0] <= r.ux && r.ly <= p[1] && p[1] <= r.uy
}
fn overlap(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

struct Spatial<'a> {
    input: &'a RouteInput,
    limits: &'a RouteLimits,
    bin: i64,
    shapes: Vec<Blockage>,
    bins: HashMap<(u32, i64, i64), Vec<usize>>,
    marks: Vec<u64>,
    generation: u64,
    entries: usize,
    work: usize,
}
impl<'a> Spatial<'a> {
    fn new(input: &'a RouteInput, limits: &'a RouteLimits) -> Result<Self> {
        let mut s = Self {
            input,
            limits,
            bin: input
                .step
                .checked_mul(8)
                .ok_or_else(|| invalid("bin overflow"))?,
            shapes: Vec::new(),
            bins: HashMap::new(),
            marks: Vec::new(),
            generation: 0,
            entries: 0,
            work: 0,
        };
        for b in &input.obstacles {
            s.add(b.clone())?;
        }
        Ok(s)
    }
    fn add(&mut self, b: Blockage) -> Result<()> {
        if self.shapes.len() >= self.limits.max_obstacles {
            return Err(budget("routing obstacle count"));
        }
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
        let id = self.shapes.len();
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.bins.entry((b.layer, x, y)).or_default().push(id);
                self.entries += 1;
            }
        }
        self.shapes.push(b);
        self.marks.push(0);
        Ok(())
    }
    fn clear(&mut self, layer: u32, rect: Rect, net: u32) -> Result<bool> {
        if !self.input.die.contains(rect) {
            return Ok(false);
        }
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
                if let Some(ids) = self.bins.get(&(layer, x, y)) {
                    for &id in ids {
                        if self.marks[id] == self.generation {
                            continue;
                        }
                        self.marks[id] = self.generation;
                        self.work += 1;
                        if self.work > self.limits.max_comparisons {
                            return Err(budget("routing comparison limit"));
                        }
                        let b = &self.shapes[id];
                        let shared = b.net == Some(net)
                            && (self.input.layers[layer as usize].vertical.is_some()
                                || rect == b.rect);
                        if !shared && overlap(query, b.rect) {
                            return Ok(false);
                        }
                    }
                }
            }
        }
        Ok(true)
    }
    fn via_clear(&mut self, kind: usize, at: [i64; 2], net: u32) -> Result<bool> {
        for shape in &self.input.vias[kind].shapes {
            if !self.clear(shape.layer, shifted(shape.rect, at)?, net)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn add_wire(&mut self, w: &Wire) -> Result<()> {
        self.add(Blockage {
            layer: w.layer,
            rect: wire_rect(w, self.input.layers[w.layer as usize].width)?,
            net: Some(w.net),
        })
    }
    fn add_via(&mut self, v: &Via) -> Result<()> {
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

fn preflight(input: &RouteInput, limits: &RouteLimits) -> Result<()> {
    limits.validate()?;
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
            || !pairs.insert((v.lower.min(v.upper), v.lower.max(v.upper)))
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
    let mut pins = HashSet::new();
    if input.terminals.len() > 16_000 || input.obstacles.len() > limits.max_obstacles {
        return Err(budget("routing input count"));
    }
    for t in &input.terminals {
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
    Ok(())
}
fn candidates(t: &Terminal, input: &RouteInput) -> Result<Vec<(u32, [i64; 2])>> {
    let mut seen = BTreeSet::new();
    let mut points = Vec::new();
    for s in &t.shapes {
        for i in [2, 1, 3] {
            for j in [2, 1, 3] {
                let x =
                    i128::from(s.rect.lx) + (i128::from(s.rect.ux) - i128::from(s.rect.lx)) * i / 4;
                let y =
                    i128::from(s.rect.ly) + (i128::from(s.rect.uy) - i128::from(s.rect.ly)) * j / 4;
                let snap = |v: i128| {
                    int(
                        (v + i128::from(input.grid) / 2).div_euclid(i128::from(input.grid))
                            * i128::from(input.grid),
                    )
                };
                let p = [snap(x)?, snap(y)?];
                if contains(s.rect, p) && contains(input.die, p) && seen.insert((s.layer, p)) {
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
struct Lattice {
    xs: Vec<i64>,
    ys: Vec<i64>,
    layers: Vec<u32>,
    slots: HashMap<u32, usize>,
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
        Ok(Self {
            xs: xs.into_iter().collect(),
            ys: ys.into_iter().collect(),
            layers,
            slots,
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
    fn neighbors(&self, id: u32, input: &RouteInput) -> Vec<(u32, Option<usize>)> {
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
        for (k, v) in input.vias.iter().enumerate() {
            if v.lower == l {
                n.push((self.id(v.upper, p), Some(k)));
            } else if v.upper == l {
                n.push((self.id(v.lower, p), Some(k)));
            }
        }
        n
    }
}
fn distance(a: [i64; 2], b: [i64; 2]) -> Result<u64> {
    let d =
        (i128::from(a[0]) - i128::from(b[0])).abs() + (i128::from(a[1]) - i128::from(b[1])).abs();
    u64::try_from(d).map_err(|_| invalid("routing distance overflow"))
}

struct AccessPlan {
    access: Access,
    top: u32,
    edges: Vec<(u32, u32)>,
}
fn reserve_access(
    input: &RouteInput,
    grid: &Lattice,
    spatial: &mut Spatial<'_>,
    terminals: &BTreeMap<u32, Vec<&Terminal>>,
    limits: &RouteLimits,
) -> Result<(HashMap<u32, AccessPlan>, usize)> {
    let mut plans = HashMap::new();
    let mut total = 0usize;
    for group in terminals.values().filter(|g| g.len() >= 2) {
        for t in group {
            let points = candidates(t, input)?;
            if points.is_empty() {
                return Err(no_route("no access candidates"));
            }
            let reach = input
                .step
                .checked_mul(4)
                .ok_or_else(|| invalid("access radius overflow"))?;
            let bounds = expand(
                Rect {
                    lx: points.iter().map(|p| p.1[0]).min().unwrap(),
                    ly: points.iter().map(|p| p.1[1]).min().unwrap(),
                    ux: points.iter().map(|p| p.1[0]).max().unwrap(),
                    uy: points.iter().map(|p| p.1[1]).max().unwrap(),
                },
                reach,
            )?;
            let mut heap = BinaryHeap::new();
            let mut visited = HashMap::<u32, (u64, u32)>::new();
            for (layer, at) in points {
                let half = input.layers[layer as usize].width / 2;
                let r = Rect {
                    lx: int(i128::from(at[0]) - i128::from(half))?,
                    ly: int(i128::from(at[1]) - i128::from(half))?,
                    ux: int(i128::from(at[0]) + i128::from(half))?,
                    uy: int(i128::from(at[1]) + i128::from(half))?,
                };
                if spatial.clear(layer, r, t.net)? {
                    let id = grid.id(layer, at);
                    visited.insert(id, (0, u32::MAX));
                    heap.push(Reverse((0u64, id)));
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
                for (next, via) in grid.neighbors(id, input) {
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
                    let clear = if let Some(k) = via {
                        spatial.via_clear(k, at, t.net)?
                    } else {
                        spatial.clear(
                            layer,
                            wire_rect(
                                &Wire {
                                    net: t.net,
                                    layer,
                                    start: at,
                                    end: q,
                                },
                                input.layers[layer as usize].width,
                            )?,
                            t.net,
                        )?
                    };
                    if !clear {
                        continue;
                    }
                    if heap.len() >= limits.max_heap || visited.len() >= limits.max_grid_nodes {
                        return Err(budget("pin access frontier"));
                    }
                    visited.insert(next, (nc, id));
                    heap.push(Reverse((nc, next)));
                }
            }
            let top = found
                .ok_or_else(|| no_route(&format!("pin {} has no legal local escape", t.pin)))?;
            let mut current = top;
            let mut edges = Vec::new();
            while visited[&current].1 != u32::MAX {
                let next = visited[&current].1;
                edges.push((current.min(next), current.max(next)));
                current = next;
                if edges.len() > limits.max_segments {
                    return Err(budget("access path limit"));
                }
            }
            let (layer, at) = grid.point(current);
            for &(a, b) in &edges {
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
                    let kind = input
                        .vias
                        .iter()
                        .position(|v| {
                            (v.lower == l && v.upper == other) || (v.upper == l && v.lower == other)
                        })
                        .unwrap();
                    spatial.add_via(&Via {
                        net: t.net,
                        kind: kind as u32,
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

pub fn route_cpu_ref(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
    preflight(input, limits)?;
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
        nets: Vec::new(),
        stats: RouteStats {
            grid_nodes: grid.len,
            expansions: access_expansions,
            ..Default::default()
        },
    };
    let mut costs = vec![u64::MAX; grid.len];
    let mut parents = vec![u32::MAX; grid.len];
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
        let mut edges: BTreeSet<_> = root.edges.iter().copied().collect();
        for &(a, b) in &root.edges {
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
                if expanded > limits.max_search_expansions
                    || out.stats.expansions > limits.max_expansions
                {
                    return Err(budget("routing expansion limit"));
                }
                let (layer, p) = grid.point(id);
                for (next, via) in grid.neighbors(id, input) {
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
                    let clear = if let Some(v) = via {
                        spatial.via_clear(v, p, net)?
                    } else {
                        spatial.clear(
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
                        )?
                    };
                    if !clear {
                        continue;
                    }
                    tags[next as usize] = epoch;
                    costs[next as usize] = next_cost;
                    parents[next as usize] = id;
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
                edges.insert((current.min(next), current.max(next)));
                tree.insert(current);
                current = next;
                length += 1;
                if length > limits.max_search_expansions || edges.len() > limits.max_segments {
                    return Err(budget("routing trace/segment limit"));
                }
            }
            tree.insert(current);
            for &(a, b) in &plan.edges {
                edges.insert((a, b));
                tree.insert(a);
                tree.insert(b);
            }
        }
        for (a, b) in edges {
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
                let kind = input
                    .vias
                    .iter()
                    .position(|v| {
                        (v.lower == l && v.upper == other) || (v.upper == l && v.lower == other)
                    })
                    .ok_or_else(|| invalid("missing path via"))?;
                let v = Via {
                    net,
                    kind: kind as u32,
                    at: p,
                };
                spatial.add_via(&v)?;
                out.vias.push(v);
            }
        }
    }
    out.stats.comparisons = spatial.work;
    validate(input, &out, limits)?;
    Ok(out)
}

/// Independent of search history: check geometry against blockages and bounded
/// endpoint-graph connectivity. External OpenDB/KLayout are the physical oracle.
pub fn validate(input: &RouteInput, routes: &Routes, limits: &RouteLimits) -> Result<()> {
    preflight(input, limits)?;
    let length = routes.wires.iter().try_fold(0u64, |n, w| {
        n.checked_add(distance(w.start, w.end)?)
            .ok_or_else(|| budget("wire length overflow"))
    })?;
    if routes.stats.grid_nodes != Lattice::new(input, limits)?.len
        || routes.stats.expansions > limits.max_expansions
        || routes.stats.comparisons > limits.max_comparisons
        || routes.stats.wire_length_dbu != length
    {
        return Err(invalid("routing telemetry/geometry mismatch"));
    }
    if routes.wires.len() + routes.vias.len() > limits.max_segments
        || routes.access.len() > input.terminals.len()
    {
        return Err(budget("stored routing size"));
    }
    let mut spatial = Spatial::new(input, limits)?;
    let mut counts = BTreeMap::<u32, usize>::new();
    for t in &input.terminals {
        *counts.entry(t.net).or_default() += 1;
    }
    let selected: BTreeSet<_> = counts
        .into_iter()
        .filter_map(|(n, c)| (c >= 2).then_some(n))
        .collect();
    if selected.iter().copied().collect::<Vec<_>>() != routes.nets {
        return Err(invalid("stored routed-net coverage"));
    }
    type Key = (u32, i64, i64);
    let mut graph = BTreeMap::<u32, HashMap<Key, Vec<Key>>>::new();
    for w in &routes.wires {
        if !selected.contains(&w.net)
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
            return Err(invalid("wire collides with blockage/other net"));
        }
        spatial.add_wire(w)?;
        let (a, b) = (
            (w.layer, w.start[0], w.start[1]),
            (w.layer, w.end[0], w.end[1]),
        );
        let g = graph.entry(w.net).or_default();
        g.entry(a).or_default().push(b);
        g.entry(b).or_default().push(a);
    }
    for v in &routes.vias {
        if !selected.contains(&v.net)
            || v.kind as usize >= input.vias.len()
            || v.at.iter().any(|x| x % input.grid != 0)
        {
            return Err(invalid("stored via identity/grid"));
        }
        if !spatial.via_clear(v.kind as usize, v.at, v.net)? {
            return Err(invalid("via collides with blockage/other net"));
        }
        spatial.add_via(v)?;
        let spec = &input.vias[v.kind as usize];
        let (a, b) = (
            (spec.lower, v.at[0], v.at[1]),
            (spec.upper, v.at[0], v.at[1]),
        );
        let g = graph.entry(v.net).or_default();
        g.entry(a).or_default().push(b);
        g.entry(b).or_default().push(a);
    }
    let terminals: HashMap<_, _> = input.terminals.iter().map(|t| (t.pin, t)).collect();
    let mut seen = HashSet::new();
    for a in &routes.access {
        let t = terminals
            .get(&a.pin)
            .ok_or_else(|| invalid("routing access pin absent"))?;
        if !selected.contains(&a.net)
            || t.net != a.net
            || !seen.insert(a.pin)
            || !t
                .shapes
                .iter()
                .any(|s| s.layer == a.layer && contains(s.rect, a.at))
        {
            return Err(invalid("routing access does not touch its pin"));
        }
        graph
            .entry(a.net)
            .or_default()
            .entry((a.layer, a.at[0], a.at[1]))
            .or_default();
    }
    if input
        .terminals
        .iter()
        .filter(|t| selected.contains(&t.net))
        .any(|t| !seen.contains(&t.pin))
    {
        return Err(invalid("missing routed terminal"));
    }
    let mut work = 0usize;
    for g in graph.values() {
        let Some(&root) = g.keys().next() else {
            continue;
        };
        let mut visited = HashSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(node) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }
            for &next in &g[&node] {
                work += 1;
                if work > limits.max_comparisons {
                    return Err(budget("routing connectivity work"));
                }
                if !visited.contains(&next) {
                    queue.push_back(next);
                }
            }
        }
        if visited.len() != g.len() {
            return Err(invalid("disconnected routed net"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> RouteInput {
        let layers = vec![
            RouteLayer {
                name: "M1".into(),
                width: 2,
                spacing: 2,
                vertical: Some(false),
            },
            RouteLayer {
                name: "M2".into(),
                width: 2,
                spacing: 2,
                vertical: Some(true),
            },
            RouteLayer {
                name: "V1".into(),
                width: 2,
                spacing: 2,
                vertical: None,
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
                },
            ],
        }
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
}
