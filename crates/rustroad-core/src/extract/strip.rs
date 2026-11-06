//! Electrical contributions of one strip (or one x/y strip pair) as a pure
//! function of immutable extraction inputs.
//!
//! A strip's arithmetic is unchanged from the original serial loop: the same
//! candidates in shape-ID order, the same interval boundaries, the same f64
//! expressions. Every accumulation (`ground[node] += v`, `coupling[key] += v`,
//! `overlap_area_um2 += a`) is emitted to a `Sink` in its original order. The
//! serial path applies them immediately; parallel tiles record them and apply
//! the records in run order, so every accumulator sees the identical sequence
//! of f64 additions at any thread count.
use super::index::GridIndex;
use super::{
    ExtractInput, ExtractLimits, ExtractStats, InstanceGeometry, NodeIds, Owner, PassiveOwner,
    PieceInfo, RcLayer, RcTech, Run, Shape, Shapes, body_rect, budget, conductor_strip, invalid,
    nearer_end, piece_strip, sample, um,
};
use crate::routing::{BuildFx, InstanceQueryScratch};
use crate::{Error, Rect, Result};
use std::collections::HashMap;

/// A candidate conductor of one strip, decoded once per query.
#[derive(Clone, Copy, Debug)]
pub(super) struct Cand {
    pub id: u32,
    pub shape: Shape,
}

/// Key prefix of a ground record: `GROUND | g` adds to global node `g`.
pub(super) const GROUND: u64 = 0xFFFF_FFFF_0000_0000;
/// Key of the overlap-area statistic.
pub(super) const AREA: u64 = u64::MAX;

/// One ordered accumulation of the original serial loop, in 16 bytes.
/// `key` names the accumulator: a coupling capacitor between global nodes
/// `ga <= gb` is `ga << 32 | gb` (global node IDs ascend with the
/// `(net, node)` endpoint, so key order is the original endpoint order),
/// ground at global node `g` is `GROUND | g`, and the overlap-area statistic
/// is `AREA`. Global node IDs stay below `u32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Op {
    pub key: u64,
    pub pf: f64,
}
impl Op {
    /// `ground[g] += pf`.
    #[inline]
    pub(super) fn ground(g: u32, pf: f64) -> Self {
        Self {
            key: GROUND | u64::from(g),
            pf,
        }
    }
    /// `coupling[{a, b}] += pf`, then the coupling-count budget.
    #[inline]
    pub(super) fn couple(a: u32, b: u32, pf: f64) -> Self {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        Self {
            key: u64::from(lo) << 32 | u64::from(hi),
            pf,
        }
    }
    /// `stats.overlap_area_um2 += um2`.
    #[inline]
    pub(super) fn area(um2: f64) -> Self {
        Self { key: AREA, pf: um2 }
    }
}

/// Why a strip stopped: a real error, or a parallel-only bound (candidate
/// cap below `max_candidates`, or a full transcript) that defers the unit
/// to the serial path.
pub(super) enum Stop {
    Error(Error),
    Defer,
}
impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Stop::Error(e)
    }
}
pub(super) type Flow = std::result::Result<(), Stop>;

pub(super) trait Sink {
    fn op(&mut self, op: Op) -> Flow;
}

/// Integer statistics of one unit (added to `ExtractStats` at commit).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Counts {
    pub intervals: usize,
    pub passive_intervals: usize,
    pub foreign_overlaps: usize,
    pub covered_side_intervals: usize,
    pub nonspanning_plane_intervals: usize,
    pub context_fallback_intervals: usize,
    pub overlap_pairs_without_data: usize,
    pub unrouted_pin_partners: usize,
    pub max_run_candidates: usize,
}
impl Counts {
    pub(super) fn add_to(&self, stats: &mut ExtractStats) {
        stats.intervals += self.intervals;
        stats.passive_intervals += self.passive_intervals;
        stats.foreign_overlaps += self.foreign_overlaps;
        stats.covered_side_intervals += self.covered_side_intervals;
        stats.nonspanning_plane_intervals += self.nonspanning_plane_intervals;
        stats.context_fallback_intervals += self.context_fallback_intervals;
        stats.overlap_pairs_without_data += self.overlap_pairs_without_data;
        stats.unrouted_pin_partners += self.unrouted_pin_partners;
        stats.max_run_candidates = stats.max_run_candidates.max(self.max_run_candidates);
    }
}

/// Calibration context `(over, under)` to its index in `RcLayer::contexts`.
/// Later duplicates win, exactly like collecting into a map. Small metal
/// indices use a dense table; anything larger falls back to a hash map.
pub(super) enum ContextTable {
    Dense {
        width: usize,
        table: Vec<Option<u32>>,
    },
    Map(HashMap<(u32, Option<u32>), usize, BuildFx>),
}
const DENSE_METALS: u32 = 64;
impl ContextTable {
    fn new(model: &RcLayer) -> Self {
        let top = model
            .contexts
            .iter()
            .map(|c| c.over.max(c.under.unwrap_or(0)))
            .max()
            .unwrap_or(0);
        if top >= DENSE_METALS {
            return Self::Map(
                model
                    .contexts
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ((c.over, c.under), i))
                    .collect(),
            );
        }
        let width = top as usize + 2;
        let mut table = vec![None; (top as usize + 1) * width];
        for (i, c) in model.contexts.iter().enumerate() {
            let under = c.under.map_or(0, |u| u as usize + 1);
            table[c.over as usize * width + under] = Some(i as u32);
        }
        Self::Dense { width, table }
    }
    #[inline]
    fn get(&self, over: u32, under: Option<u32>) -> Option<usize> {
        match self {
            Self::Dense { width, table } => {
                let under = under.map_or(0, |u| u as usize + 1);
                if under >= *width {
                    return None;
                }
                table
                    .get((over as usize).checked_mul(*width)?.checked_add(under)?)
                    .copied()
                    .flatten()
                    .map(|i| i as usize)
            }
            Self::Map(map) => map.get(&(over, under)).copied(),
        }
    }
}

/// Per routing layer: its model, lateral window and context table.
pub(super) struct LayerInfo<'a> {
    model: &'a RcLayer,
    window_dbu: i64,
    contexts: ContextTable,
    substrate: usize,
}

/// Immutable inputs shared by every strip computation.
pub(super) struct Ctx<'c, 'a> {
    pub tech: &'a RcTech,
    pub shapes: &'c Shapes<'a>,
    pub grid: &'c GridIndex,
    pub instances: Option<&'c InstanceGeometry<'a>>,
    pub routing_layers: &'c [u32],
    pub limits: &'c ExtractLimits,
    /// Wire bodies (`Owner::Run`).
    pub runs: &'c [Run],
    /// Rectangle pieces (`Owner::Piece`); piece `p` is shape `piece_shape0 + p`.
    pub pieces: &'c [PieceInfo],
    pub piece_shape0: u32,
    pub pin_nodes: &'c HashMap<u32, (u32, u32), BuildFx>,
    pub net_index: &'c HashMap<u32, usize, BuildFx>,
    pub nodes: &'c NodeIds,
    layers: Vec<Option<LayerInfo<'a>>>,
    metal_layer: HashMap<u32, usize, BuildFx>,
}
impl<'c, 'a> Ctx<'c, 'a> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one constructor binds every immutable strip input"
    )]
    pub(super) fn new(
        input: &ExtractInput<'a>,
        shapes: &'c Shapes<'a>,
        grid: &'c GridIndex,
        instances: Option<&'c InstanceGeometry<'a>>,
        routing_layers: &'c [u32],
        limits: &'c ExtractLimits,
        runs: &'c [Run],
        (pieces, piece_shape0): (&'c [PieceInfo], u32),
        pin_nodes: &'c HashMap<u32, (u32, u32), BuildFx>,
        net_index: &'c HashMap<u32, usize, BuildFx>,
        nodes: &'c NodeIds,
    ) -> Result<Self> {
        let tech = input.tech;
        let mut layers = Vec::with_capacity(tech.layers.len());
        for model in &tech.layers {
            layers.push(match model {
                None => None,
                Some(model) => {
                    let window = model
                        .contexts
                        .iter()
                        .map(|c| c.rows[c.rows.len() - 1][0])
                        .fold(0.0f64, f64::max);
                    let contexts = ContextTable::new(model);
                    let substrate = contexts
                        .get(0, None)
                        .ok_or_else(|| invalid("layer lacks the substrate calibration context"))?;
                    Some(LayerInfo {
                        model,
                        window_dbu: (window * tech.dbu_per_um).ceil() as i64,
                        contexts,
                        substrate,
                    })
                }
            });
        }
        let metal_layer = tech
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().map(|m| (m.metal, i)))
            .collect();
        Ok(Self {
            tech,
            shapes,
            grid,
            instances,
            routing_layers,
            limits,
            runs,
            pieces,
            piece_shape0,
            pin_nodes,
            net_index,
            nodes,
            layers,
            metal_layer,
        })
    }
    #[inline]
    fn info(&self, layer: u32) -> Result<&LayerInfo<'a>> {
        self.layers
            .get(layer as usize)
            .and_then(Option::as_ref)
            .ok_or_else(|| invalid("routing layer has no RC profile entry"))
    }
    /// Lateral calibration window of a routing layer, DBU.
    #[cfg(test)]
    pub(super) fn window_dbu(&self, layer: u32) -> Result<i64> {
        Ok(self.info(layer)?.window_dbu)
    }
}

/// Reusable per-worker buffers. Capacity follows the largest strip seen.
pub(super) struct Scratch {
    pub instances: InstanceQueryScratch,
    other: Vec<Cand>,
    own: Vec<(Cand, u8)>,
    cands: Vec<Cand>,
    events: Vec<u128>,
    active: Vec<u64>,
    layered: Vec<(u32, i64, i64, u32)>,
    contributions: Vec<(u32, f64, f64)>,
    face: FaceScratch,
    probes: Vec<Probe>,
    samples: SampleCache,
}
impl Scratch {
    pub(super) fn new(instances: usize) -> Result<Self> {
        Ok(Self {
            instances: InstanceQueryScratch::new(instances)?,
            other: Vec::new(),
            own: Vec::new(),
            cands: Vec::new(),
            events: Vec::new(),
            active: Vec::new(),
            layered: Vec::new(),
            contributions: Vec::new(),
            face: FaceScratch::default(),
            probes: Vec::new(),
            samples: SampleCache::default(),
        })
    }
}

/// One unit of strip work: a wire body, or the x/y strip pair of one
/// rectangle (they share one shape): a piece of routed metal, or a crossed
/// passive conductor (retained or virtual). Pair strips are generated from
/// the shape when the unit is computed; they are never stored.
#[derive(Clone, Copy, Debug)]
pub(super) enum UnitRef {
    Body(u32),
    Piece(u32),
    Conductor(u32),
}

/// The strips of one unit, in their original order: a body alone, or a
/// rectangle's x strip then its y strip.
#[inline]
pub(super) fn strips(ctx: &Ctx<'_, '_>, unit: UnitRef) -> (Run, Option<Run>) {
    match unit {
        UnitRef::Body(i) => (ctx.runs[i as usize], None),
        UnitRef::Piece(p) => {
            let id = ctx.piece_shape0 + p;
            let (shape, info) = (ctx.shapes.get(id), ctx.pieces[p as usize]);
            (
                piece_strip(id, shape.rect, shape.layer, info, false),
                Some(piece_strip(id, shape.rect, shape.layer, info, true)),
            )
        }
        UnitRef::Conductor(id) => {
            let shape = ctx.shapes.get(id);
            (
                conductor_strip(id, shape, false),
                Some(conductor_strip(id, shape, true)),
            )
        }
    }
}

/// Computes one unit. `cap` bounds the candidates of each strip: exceeding
/// `limits.max_candidates` is the budget error, exceeding only `cap` defers.
pub(super) fn unit<S: Sink>(
    ctx: &Ctx<'_, '_>,
    unit: UnitRef,
    cap: usize,
    sc: &mut Scratch,
    sink: &mut S,
    work: &mut usize,
    counts: &mut Counts,
) -> Flow {
    match strips(ctx, unit) {
        (run, None) => single(ctx, &run, cap, sc, sink, work, counts),
        (x, Some(y)) => pair(ctx, &x, &y, cap, sc, sink, work, counts),
    }
}

/// Lateral window rectangle of a strip on its own layer.
fn window(run: &Run, w: i64) -> (Rect, i64, i64) {
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
    (q, c0, c1)
}

/// The original acceptance predicate: overlap along the strip, and the
/// lateral window (own layer, `Some`) or the strip face (other layers).
#[inline]
pub(super) fn accepts(run: &Run, lateral: Option<(i64, i64)>, id: u32, r: Rect) -> bool {
    if id == run.shape {
        return false;
    }
    let (a0, a1, x0, x1) = if run.vertical {
        (r.ly, r.uy, r.lx, r.ux)
    } else {
        (r.lx, r.ux, r.ly, r.uy)
    };
    if a1 <= run.lo || a0 >= run.hi {
        return false;
    }
    match lateral {
        Some((c0, c1)) => x1 > c0 && x0 < c1,
        None => x1 > run.minus && x0 < run.plus,
    }
}

/// The over-cap outcome: the budget error once `max_candidates` is exceeded,
/// otherwise a deferral of a parallel-only bound.
fn over_cap(limits: &ExtractLimits, count: usize) -> Stop {
    if count > limits.max_candidates {
        Stop::Error(budget("extraction candidates per wire run"))
    } else {
        Stop::Defer
    }
}

/// Visit retained and virtual conductors of `layer` strictly intersecting
/// `q`. `filter` tests the geometry first and returns non-zero flags for an
/// accepted conductor; only then is its shape fetched and passed to `take`,
/// which returns `false` to stop (then `Ok(false)`).
fn visit_layer(
    ctx: &Ctx<'_, '_>,
    instances: &mut InstanceQueryScratch,
    layer: u32,
    q: Rect,
    work: &mut usize,
    filter: impl Fn(u32, Rect) -> u8,
    mut take: impl FnMut(Cand, u8) -> bool,
) -> Result<bool> {
    let shapes = ctx.shapes;
    let grid = ctx.grid;
    if !grid.visit(layer, q, ctx.limits, work, |id, rect| {
        Ok(match filter(id, rect) {
            0 => true,
            flags => take(
                Cand {
                    id,
                    shape: grid.shape(shapes, id, layer, rect),
                },
                flags,
            ),
        })
    })? {
        return Ok(false);
    }
    match ctx.instances {
        Some(geometry) => geometry.visit_shared(instances, shapes, layer, q, work, |id, shape| {
            Ok(match filter(id, shape.rect) {
                0 => true,
                flags => take(Cand { id, shape }, flags),
            })
        }),
        None => Ok(true),
    }
}

/// Other-layer candidates of a strip face (shared by an x/y pair: both
/// strips of one rectangle accept exactly the conductors overlapping it).
fn gather_other(
    ctx: &Ctx<'_, '_>,
    run: &Run,
    cap: usize,
    sc: &mut Scratch,
    work: &mut usize,
) -> Flow {
    sc.other.clear();
    let q = body_rect(run);
    for &layer in ctx.routing_layers {
        if layer == run.layer {
            continue;
        }
        let other = &mut sc.other;
        let mut over = false;
        let complete = visit_layer(
            ctx,
            &mut sc.instances,
            layer,
            q,
            work,
            |id, r| u8::from(accepts(run, None, id, r)),
            |c, _| {
                if other.len() >= cap {
                    over = true;
                    return false;
                }
                other.push(c);
                true
            },
        )?;
        if over || !complete {
            return Err(over_cap(ctx.limits, other.len() + 1));
        }
    }
    sc.other.sort_unstable_by_key(|c| c.id);
    Ok(())
}

/// Merges two ID-sorted lists (IDs are unique across them).
fn merge(a: &[Cand], b: impl Iterator<Item = Cand>, out: &mut Vec<Cand>) {
    out.clear();
    let mut i = 0;
    for c in b {
        while i < a.len() && a[i].id < c.id {
            out.push(a[i]);
            i += 1;
        }
        debug_assert!(i >= a.len() || a[i].id != c.id, "duplicate candidate");
        out.push(c);
    }
    out.extend_from_slice(&a[i..]);
}

fn single<S: Sink>(
    ctx: &Ctx<'_, '_>,
    run: &Run,
    cap: usize,
    sc: &mut Scratch,
    sink: &mut S,
    work: &mut usize,
    counts: &mut Counts,
) -> Flow {
    let w = ctx.info(run.layer)?.window_dbu;
    gather_other(ctx, run, cap, sc, work)?;
    let (q, c0, c1) = window(run, w);
    let base = sc.other.len();
    let own = &mut sc.own;
    own.clear();
    let mut over = false;
    let complete = visit_layer(
        ctx,
        &mut sc.instances,
        run.layer,
        q,
        work,
        |id, r| u8::from(accepts(run, Some((c0, c1)), id, r)),
        |c, flags| {
            if base + own.len() >= cap {
                over = true;
                return false;
            }
            own.push((c, flags));
            true
        },
    )?;
    if over || !complete {
        return Err(over_cap(ctx.limits, base + own.len() + 1));
    }
    own.sort_unstable_by_key(|c| c.0.id);
    counts.max_run_candidates = counts.max_run_candidates.max(base + own.len());
    let mut cands = std::mem::take(&mut sc.cands);
    merge(&sc.other, sc.own.iter().map(|c| c.0), &mut cands);
    let result = numeric(ctx, run, &cands, sc, sink, work, counts);
    sc.cands = cands;
    result
}

/// The x and y strips of one rectangle: one other-layer face query and one
/// own-layer query over both lateral windows, then each strip's numerics in
/// order. When the shared queries fail (a candidate cap or a query budget,
/// possibly the y strip's alone), the strips are recomputed one after the
/// other from the original work count, so the x strip's accumulations and
/// budgets come before any y strip error, as for two separate strips.
#[expect(
    clippy::too_many_arguments,
    reason = "a pair shares one scratch, sink and work counter"
)]
fn pair<S: Sink>(
    ctx: &Ctx<'_, '_>,
    x: &Run,
    y: &Run,
    cap: usize,
    sc: &mut Scratch,
    sink: &mut S,
    work: &mut usize,
    counts: &mut Counts,
) -> Flow {
    debug_assert!(x.shape == y.shape && !x.vertical && y.vertical);
    debug_assert!(body_rect(x) == body_rect(y));
    let start = *work;
    let widest = match pair_candidates(ctx, x, y, cap, sc, work) {
        Ok(widest) => widest,
        Err(Stop::Error(_)) => {
            *work = start;
            single(ctx, x, cap, sc, sink, work, counts)?;
            return single(ctx, y, cap, sc, sink, work, counts);
        }
        Err(stop) => return Err(stop),
    };
    counts.max_run_candidates = counts.max_run_candidates.max(widest);
    let mut cands = std::mem::take(&mut sc.cands);
    for (run, flag) in [(x, 1u8), (y, 2u8)] {
        merge(
            &sc.other,
            sc.own.iter().filter(|c| c.1 & flag != 0).map(|c| c.0),
            &mut cands,
        );
        if let Err(stop) = numeric(ctx, run, &cands, sc, sink, work, counts) {
            sc.cands = cands;
            return Err(stop);
        }
    }
    sc.cands = cands;
    Ok(())
}

/// Shared candidate queries of a pair: other-layer candidates in
/// `sc.other`, own-layer hits with per-strip flags (1: x, 2: y) in
/// `sc.own`, both ID-sorted. Returns the larger strip's candidate count.
fn pair_candidates(
    ctx: &Ctx<'_, '_>,
    x: &Run,
    y: &Run,
    cap: usize,
    sc: &mut Scratch,
    work: &mut usize,
) -> std::result::Result<usize, Stop> {
    let w = ctx.info(x.layer)?.window_dbu;
    gather_other(ctx, x, cap, sc, work)?;
    // One own-layer query covers both lateral windows; each hit is tested
    // with each strip's own predicate.
    let (qx, cx0, cx1) = window(x, w);
    let (qy, cy0, cy1) = window(y, w);
    let union = Rect {
        lx: qx.lx.min(qy.lx),
        ly: qx.ly.min(qy.ly),
        ux: qx.ux.max(qy.ux),
        uy: qx.uy.max(qy.uy),
    };
    let base = sc.other.len();
    let own = &mut sc.own;
    own.clear();
    let (mut nx, mut ny) = (0usize, 0usize);
    let mut over = 0usize;
    let complete = visit_layer(
        ctx,
        &mut sc.instances,
        x.layer,
        union,
        work,
        |id, r| {
            u8::from(accepts(x, Some((cx0, cx1)), id, r))
                | u8::from(accepts(y, Some((cy0, cy1)), id, r)) << 1
        },
        |c, flags| {
            nx += usize::from(flags & 1);
            ny += usize::from(flags >> 1);
            if base + nx.max(ny) > cap {
                over = base + nx.max(ny);
                return false;
            }
            own.push((c, flags));
            true
        },
    )?;
    if over > 0 || !complete {
        return Err(over_cap(ctx.limits, over.max(base + 1)));
    }
    own.sort_unstable_by_key(|c| c.0.id);
    Ok(base + nx.max(ny))
}

#[inline]
fn event(pos: i64, k: usize, enter: bool) -> u128 {
    (u128::from(pos as u64 ^ (1 << 63)) << 64) | ((k as u128) << 1) | u128::from(enter)
}
#[inline]
fn event_pos(e: u128) -> i64 {
    ((e >> 64) as u64 ^ (1 << 63)) as i64
}

struct Bits<'b> {
    words: &'b [u64],
    at: usize,
    current: u64,
}
impl<'b> Bits<'b> {
    fn new(words: &'b [u64]) -> Self {
        Self {
            words,
            at: 0,
            current: words.first().copied().unwrap_or(0),
        }
    }
}
impl Iterator for Bits<'_> {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some(self.at * 64 + bit);
            }
            self.at += 1;
            if self.at >= self.words.len() {
                return None;
            }
            self.current = self.words[self.at];
        }
    }
}

/// Indices `[start, end)` of consecutive metal groups of `layered` (sorted
/// by metal) on one side of `own`: ascending above, descending below.
struct Groups<'l> {
    layered: &'l [(u32, i64, i64, u32)],
    upper: bool,
    at: usize,
}
impl<'l> Groups<'l> {
    fn new(layered: &'l [(u32, i64, i64, u32)], own: u32, upper: bool) -> Self {
        let at = if upper {
            layered.partition_point(|e| e.0 <= own)
        } else {
            layered.partition_point(|e| e.0 < own)
        };
        Self { layered, upper, at }
    }
}
impl Iterator for Groups<'_> {
    type Item = (usize, usize);
    fn next(&mut self) -> Option<(usize, usize)> {
        let l = self.layered;
        if self.upper {
            let start = self.at;
            let metal = l.get(start)?.0;
            let mut end = start + 1;
            while end < l.len() && l[end].0 == metal {
                end += 1;
            }
            self.at = end;
            Some((start, end))
        } else {
            let end = self.at;
            if end == 0 {
                return None;
            }
            let metal = l[end - 1].0;
            let mut start = end - 1;
            while start > 0 && l[start - 1].0 == metal {
                start -= 1;
            }
            self.at = start;
            Some((start, end))
        }
    }
}

/// The plate below (`upper == false`) or above a strip: the nearest other
/// calibration metal whose conductors (merged along the strip's cross axis)
/// cover `minus..plus` and extend at least one strip width beyond each side
/// edge, with the conductor under each side edge (`[plus side, minus side]`).
/// `layered` is sorted by metal, then cross extent.
fn plate(
    layered: &[(u32, i64, i64, u32)],
    own: u32,
    upper: bool,
    minus: i64,
    plus: i64,
) -> Option<(u32, [u32; 2])> {
    for (start, end) in Groups::new(layered, own, upper) {
        let on = &layered[start..end];
        let metal = on[0].0;
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

/// `(metal, covered width, owners[start..end])` portions of a strip face.
#[derive(Default)]
struct FaceScratch {
    remaining: Vec<(i64, i64)>,
    next: Vec<(i64, i64)>,
    owners: Vec<(u32, i64)>,
    portions: Vec<(u32, i64, usize, usize)>,
}

/// Coverage of a strip face `minus..plus` by the metal of other layers below
/// (`upper == false`) or above, nearest layer first; a nearer layer shields
/// the farther ones. Exact integer DBU; `layered` is sorted by metal, then
/// cross extent.
fn face_portions(
    layered: &[(u32, i64, i64, u32)],
    own: u32,
    upper: bool,
    minus: i64,
    plus: i64,
    f: &mut FaceScratch,
) {
    f.portions.clear();
    f.owners.clear();
    f.remaining.clear();
    f.remaining.push((minus, plus));
    for (start, end) in Groups::new(layered, own, upper) {
        let metal = layered[start].0;
        let first = f.owners.len();
        let mut covered = 0;
        for e in &layered[start..end] {
            f.next.clear();
            let mut width = 0;
            for &(r0, r1) in &f.remaining {
                let (c0, c1) = (r0.max(e.1), r1.min(e.2));
                if c1 > c0 {
                    width += c1 - c0;
                    if r0 < c0 {
                        f.next.push((r0, c0));
                    }
                    if c1 < r1 {
                        f.next.push((c1, r1));
                    }
                } else {
                    f.next.push((r0, r1));
                }
            }
            std::mem::swap(&mut f.remaining, &mut f.next);
            if width > 0 {
                f.owners.push((e.3, width));
                covered += width;
            }
        }
        if covered > 0 {
            f.portions.push((metal, covered, first, f.owners.len()));
        }
        if f.remaining.is_empty() {
            break;
        }
    }
}

/// A candidate seen from one strip: its extent along the strip (`a0..a1`)
/// and across it (`x0..x1`), and its layer's calibration metal (`OWN` on
/// the strip's own layer, `NO_MODEL` when its layer has no RC model).
#[derive(Clone, Copy)]
struct Probe {
    a0: i64,
    a1: i64,
    x0: i64,
    x1: i64,
    metal: u32,
}
const OWN: u32 = u32::MAX;
const NO_MODEL: u32 = u32::MAX - 1;

/// Memo of `sample(rows, spacing)`, a pure function of the (immutable)
/// calibration rows and the spacing, direct-mapped by both.
struct SampleCache {
    entries: [(usize, u64, (f64, f64)); SAMPLE_SLOTS],
}
const SAMPLE_SLOTS: usize = 64;
impl Default for SampleCache {
    fn default() -> Self {
        Self {
            entries: [(0, 0, (0.0, 0.0)); SAMPLE_SLOTS],
        }
    }
}
impl SampleCache {
    #[inline]
    fn get(&mut self, rows: &[[f64; 3]], spacing: Option<f64>) -> (f64, f64) {
        let at = rows.as_ptr() as usize;
        // NaN never occurs (spacings are finite); `None` keeps its own key.
        let key = spacing.map_or(u64::MAX, f64::to_bits);
        let slot = (at as u64 ^ key.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_mul(0xD6E8_FEB8_6659_FD93)
            >> 58;
        let e = &mut self.entries[slot as usize % SAMPLE_SLOTS];
        if e.0 == at && e.1 == key {
            return e.2;
        }
        let v = sample(rows, spacing);
        *e = (at, key, v);
        v
    }
}

/// The original per-strip interval sweep over ID-sorted candidates.
fn numeric<S: Sink>(
    ctx: &Ctx<'_, '_>,
    run: &Run,
    cands: &[Cand],
    sc: &mut Scratch,
    sink: &mut S,
    work: &mut usize,
    counts: &mut Counts,
) -> Flow {
    let Scratch {
        events,
        active,
        layered,
        contributions,
        face,
        probes,
        samples,
        ..
    } = sc;
    let tech = ctx.tech;
    let limits = ctx.limits;
    let info = ctx.info(run.layer)?;
    let model = info.model;
    let window_dbu = info.window_dbu;
    let (edge_minus, edge_plus) = (run.minus, run.plus);
    events.clear();
    probes.clear();
    for (k, c) in cands.iter().enumerate() {
        let r = c.shape.rect;
        let (a0, a1, x0, x1) = if run.vertical {
            (r.ly, r.uy, r.lx, r.ux)
        } else {
            (r.lx, r.ux, r.ly, r.uy)
        };
        let metal = if c.shape.layer == run.layer {
            OWN
        } else {
            ctx.info(c.shape.layer).map_or(NO_MODEL, |i| i.model.metal)
        };
        probes.push(Probe {
            a0,
            a1,
            x0,
            x1,
            metal,
        });
        events.push(event(a0.max(run.lo), k, true));
        events.push(event(a1.min(run.hi), k, false));
    }
    events.sort_unstable();
    let index_work = events
        .len()
        .checked_mul(3 * events.len().max(1).ilog2() as usize + 4)
        .ok_or_else(|| budget("extraction interval index work"))?;
    *work = work
        .checked_add(index_work)
        .filter(|&n| n <= limits.max_work)
        .ok_or_else(|| budget("extraction interval index work"))?;
    active.clear();
    active.resize(cands.len().div_ceil(64), 0);
    let mut active_n = 0usize;
    let span = (run.hi - run.lo) as f64;
    // Passive strips attach to their routed partners only.
    let net_local = match run.passive {
        None => ctx.net_index[&run.net],
        Some(_) => usize::MAX,
    };
    // Same metal as this strip: its own net, or for a passive conductor
    // the same supply/ground class or the same pin net.
    let same_metal = |sh: &Shape| -> bool {
        match (sh.owner, run.passive) {
            (Owner::Run(o), _) => ctx.runs[o as usize].net == run.net,
            (Owner::Piece(p), _) => ctx.pieces[p as usize].net == run.net,
            (Owner::Passive(p), None) => {
                matches!(p, PassiveOwner::Pin { net, .. } if net == run.net)
            }
            (Owner::Passive(p), Some(PassiveOwner::Ground)) => p == PassiveOwner::Ground,
            (Owner::Passive(p), Some(PassiveOwner::Pin { net, .. })) => {
                matches!(p, PassiveOwner::Pin { net: other, .. } if other == net)
            }
        }
    };
    // Interval boundaries are run.lo, run.hi and every (clamped) candidate
    // end, ascending and distinct: exactly the original sorted point list.
    let mut event_index = 0;
    let mut t0 = run.lo;
    loop {
        while event_index < events.len() && event_pos(events[event_index]) <= t0 {
            let e = events[event_index];
            let k = ((e as u64) >> 1) as usize;
            let (w, bit) = (k / 64, 1u64 << (k % 64));
            if e & 1 == 1 {
                if active[w] & bit == 0 {
                    active_n += 1;
                    active[w] |= bit;
                }
            } else if active[w] & bit != 0 {
                active_n -= 1;
                active[w] &= !bit;
            }
            event_index += 1;
        }
        let t1 = if event_index < events.len() {
            event_pos(events[event_index])
        } else {
            run.hi
        };
        if t1 <= t0 {
            break;
        }
        if run.passive.is_some() {
            counts.passive_intervals += 1;
        } else {
            counts.intervals += 1;
        }
        *work = work
            .checked_add(active_n.max(1))
            .ok_or_else(|| budget("extraction work"))?;
        if *work > limits.max_work {
            return Err(budget("extraction work").into());
        }
        // Nearest lateral neighbour per side and nearest planes. Candidate
        // positions are in shape-ID order, so `(gap, position)` breaks ties
        // exactly like `(gap, shape ID)`.
        let mut plus: Option<(i64, u32)> = None;
        let mut minus: Option<(i64, u32)> = None;
        let (mut covered_plus, mut covered_minus) = (false, false);
        layered.clear();
        for k in Bits::new(active) {
            let Probe {
                a0,
                a1,
                x0,
                x1,
                metal,
            } = probes[k];
            if a0 > t0 || a1 < t1 {
                continue;
            }
            let k = k as u32;
            if metal == OWN {
                if x0 >= edge_plus {
                    let gap = x0 - edge_plus;
                    if gap < window_dbu && plus.is_none_or(|p| (gap, k) < p) {
                        plus = Some((gap, k));
                    }
                } else if x1 <= edge_minus {
                    let gap = edge_minus - x1;
                    if gap < window_dbu && minus.is_none_or(|p| (gap, k) < p) {
                        minus = Some((gap, k));
                    }
                } else if !same_metal(&cands[k as usize].shape) {
                    if run.passive.is_none() {
                        counts.foreign_overlaps += 1;
                    }
                } else {
                    // Same-net metal across a side edge: that side is
                    // inside the net's metal union, not exposed.
                    covered_plus |= x1 > edge_plus;
                    covered_minus |= x0 < edge_minus;
                }
            } else {
                if metal == NO_MODEL {
                    return Err(invalid("routing layer has no RC profile entry").into());
                }
                let overlap = x1.min(edge_plus) - x0.max(edge_minus);
                if overlap <= 0 {
                    continue;
                }
                layered.push((metal, x0, x1, k));
            }
        }
        // A conductor on another layer is a plate for this strip (the
        // calibration OVER/UNDER contexts) only where its layer's metal
        // spans the whole strip cross-section and reaches one strip width
        // beyond both side edges: crossings and clearly wider shapes.
        layered.sort_unstable();
        let below = plate(layered, model.metal, false, edge_minus, edge_plus);
        let above = plate(layered, model.metal, true, edge_minus, edge_plus);
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
            counts.nonspanning_plane_intervals += 1;
        }
        let over = below.map_or(0, |b| b.0);
        let under = above.map(|a| a.0);
        let ctx_map = &info.contexts;
        let (context, plane_split) = match ctx_map.get(over, under) {
            Some(i) => (i, (below.is_some(), above.is_some())),
            None => {
                if run.passive.is_none() {
                    counts.context_fallback_intervals += 1;
                }
                if let Some(i) = ctx_map.get(over, None).filter(|_| over > 0) {
                    (i, (true, false))
                } else if let Some(i) = ctx_map.get(0, under) {
                    (i, (false, above.is_some()))
                } else {
                    (info.substrate, (false, false))
                }
            }
        };
        let sub = &model.contexts[info.substrate].rows;
        let rows = &model.contexts[context].rows;
        let below_only = if plane_split.0 && plane_split.1 {
            ctx_map.get(over, None).map(|i| &model.contexts[i].rows)
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
            [run.cross() as f64, mid]
        } else {
            [mid, run.cross() as f64]
        };
        let mut ground = 0.0f64;
        // (candidate, pF, factor applied when the partner is another run).
        contributions.clear();
        for (k, side, covered) in [(0, plus, covered_plus), (1, minus, covered_minus)] {
            if covered {
                if run.passive.is_none() {
                    counts.covered_side_intervals += 1;
                }
                continue;
            }
            let spacing = side.map(|(g, _)| um(tech, g));
            let touching_same =
                side.is_some_and(|(g, s)| g == 0 && same_metal(&cands[s as usize].shape));
            if touching_same {
                continue;
            }
            let (cc, cg) = samples.get(rows, spacing);
            let (_, cg_sub) = samples.get(sub, spacing);
            // Split this side's context value between the substrate and
            // the planes. A conductor below is the plate of an OVER
            // context: it shields the substrate, so the whole OVER value
            // goes to it. A plane above takes only the excess over what
            // goes down (the substrate share, or the OVER value).
            let cg = cg.max(0.0);
            let (g, d_below, d_above) = match plane_split {
                (true, true) => {
                    let down = below_only
                        .map_or(cg_sub, |r| samples.get(r, spacing).1)
                        .min(cg);
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
            // own field: a crossing is counted from both wires.
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
        // Overlap area of non-plate metal on other layers, counted once
        // from the upper face; a piece counts its face on its x strip only.
        if run.passive.is_none()
            && !(run.piece && run.vertical)
            && let Some(area_cap) = model.area_cap
        {
            let mut shielded = 0.0;
            if below.is_none() {
                face_portions(layered, model.metal, false, edge_minus, edge_plus, face);
                for &(metal, covered, first, last) in &face.portions {
                    let Some(&(_, ov)) = model.overlap.iter().find(|o| o.0 == metal) else {
                        counts.overlap_pairs_without_data += 1;
                        continue;
                    };
                    let area = um(tech, covered) * length;
                    sink.op(Op::area(area))?;
                    shielded += area_cap * area;
                    for &(sh, w) in &face.owners[first..last] {
                        contributions.push((sh, ov * um(tech, w) * length, 1.0));
                    }
                }
            }
            if above.is_none() {
                // A routed conductor above counts the overlap from its own
                // face; passive metal does not model itself.
                face_portions(layered, model.metal, true, edge_minus, edge_plus, face);
                let passive =
                    |o: &&(u32, i64)| matches!(cands[o.0 as usize].shape.owner, Owner::Passive(_));
                for &(metal, _, first, last) in &face.portions {
                    let owners = &face.owners[first..last];
                    if !owners.iter().any(|o| passive(&o)) {
                        continue;
                    }
                    let upper = ctx
                        .metal_layer
                        .get(&metal)
                        .and_then(|&l| tech.layers[l].as_ref())
                        .and_then(|u| u.overlap.iter().find(|o| o.0 == model.metal));
                    let Some(&(_, ov)) = upper else {
                        counts.overlap_pairs_without_data += 1;
                        continue;
                    };
                    for &(sh, w) in owners.iter().filter(passive) {
                        sink.op(Op::area(um(tech, w) * length))?;
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
            for &(s, value, run_factor) in contributions.iter() {
                if run_factor < 1.0 {
                    continue;
                }
                let (onet, pnode) = match cands[s as usize].shape.owner {
                    Owner::Run(o) => {
                        let other = &ctx.runs[o as usize];
                        if other.passive.is_some() {
                            continue;
                        }
                        (other.net, nearer_end(other, mid_point))
                    }
                    // A piece's two end nodes are its one node.
                    Owner::Piece(p) => {
                        let info = ctx.pieces[p as usize];
                        (info.net, info.node)
                    }
                    Owner::Passive(_) => continue,
                };
                if onet == run.net {
                    continue;
                }
                let b = ctx.nodes.of(ctx.net_index, onet, pnode);
                let pin_node = match run.passive {
                    Some(PassiveOwner::Pin { pin, .. }) => ctx.pin_nodes.get(&pin).copied(),
                    _ => None,
                };
                match pin_node {
                    Some((net, node)) => {
                        let a = ctx.nodes.of(ctx.net_index, net, node);
                        sink.op(Op::couple(a, b, value))?;
                    }
                    None => {
                        if matches!(run.passive, Some(PassiveOwner::Pin { .. })) {
                            counts.unrouted_pin_partners += 1;
                        }
                        sink.op(Op::ground(b, value))?;
                    }
                }
            }
            t0 = t1;
            continue;
        }
        // Attach to this run's end nodes by position (pi split).
        let (g_lo, g_hi) = (
            ctx.nodes.local(net_local, run.node_lo),
            ctx.nodes.local(net_local, run.node_hi),
        );
        sink.op(Op::ground(g_lo, ground * (1.0 - w_hi)))?;
        sink.op(Op::ground(g_hi, ground * w_hi))?;
        for &(s, value, run_factor) in contributions.iter() {
            let partner = match cands[s as usize].shape.owner {
                Owner::Run(o) => {
                    // Every routed-net shape models its own exposed sides,
                    // so a lateral pair is counted half from each side.
                    let other = &ctx.runs[o as usize];
                    if other.net == run.net {
                        None
                    } else {
                        Some(Some((other.net, nearer_end(other, mid_point), run_factor)))
                    }
                }
                Owner::Piece(p) => {
                    let info = ctx.pieces[p as usize];
                    if info.net == run.net {
                        None
                    } else {
                        Some(Some((info.net, info.node, run_factor)))
                    }
                }
                Owner::Passive(PassiveOwner::Ground) => Some(None),
                Owner::Passive(PassiveOwner::Pin { net, pin }) => {
                    if net == run.net {
                        None
                    } else if let Some(&(n, node)) = ctx.pin_nodes.get(&pin) {
                        Some(Some((n, node, 1.0)))
                    } else {
                        counts.unrouted_pin_partners += 1;
                        Some(None)
                    }
                }
            };
            match partner {
                None => {}
                Some(None) => {
                    sink.op(Op::ground(g_lo, value * (1.0 - w_hi)))?;
                    sink.op(Op::ground(g_hi, value * w_hi))?;
                }
                Some(Some((pnet, pnode, factor))) => {
                    let b = ctx.nodes.of(ctx.net_index, pnet, pnode);
                    for (a, weight) in [(g_lo, 1.0 - w_hi), (g_hi, w_hi)] {
                        let v = value * factor * weight;
                        if v <= 0.0 {
                            continue;
                        }
                        sink.op(Op::couple(a, b, v))?;
                    }
                }
            }
        }
        t0 = t1;
    }
    Ok(())
}

/// Numerical body over externally gathered candidates (reference path).
#[cfg(test)]
pub(super) fn numeric_with<S: Sink>(
    ctx: &Ctx<'_, '_>,
    run: &Run,
    cands: &[Cand],
    sc: &mut Scratch,
    sink: &mut S,
    work: &mut usize,
    counts: &mut Counts,
) -> Flow {
    numeric(ctx, run, cands, sc, sink, work, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_sort_by_position_then_candidate_then_exit_first() {
        let mut events = [
            event(5, 1, true),
            event(-7, 9, false),
            event(5, 0, false),
            event(5, 1, false),
            event(i64::MIN, 3, true),
            event(i64::MAX, 2, false),
        ];
        events.sort_unstable();
        let decoded: Vec<_> = events
            .iter()
            .map(|&e| (event_pos(e), ((e as u64) >> 1) as usize, e & 1 == 1))
            .collect();
        assert_eq!(
            decoded,
            [
                (i64::MIN, 3, true),
                (-7, 9, false),
                (5, 0, false),
                (5, 1, false),
                (5, 1, true),
                (i64::MAX, 2, false)
            ]
        );
    }

    #[test]
    fn dense_context_table_matches_a_last_wins_map() {
        use super::super::RcContext;
        let ctx = |over, under| RcContext {
            over,
            under,
            rows: vec![[0.1, 0.0, 0.0]],
        };
        for contexts in [
            vec![
                ctx(0, None),
                ctx(1, None),
                ctx(0, Some(3)),
                ctx(1, Some(3)),
                ctx(1, None),
            ],
            vec![ctx(0, None), ctx(70, Some(2)), ctx(2, Some(80))],
        ] {
            let model = RcLayer {
                metal: 2,
                width: 1,
                sheet_ohm: 1.0,
                contexts,
                area_cap: None,
                overlap: vec![],
            };
            let table = ContextTable::new(&model);
            let map: HashMap<(u32, Option<u32>), usize> = model
                .contexts
                .iter()
                .enumerate()
                .map(|(i, c)| ((c.over, c.under), i))
                .collect();
            for over in 0..100 {
                for under in [
                    None,
                    Some(0),
                    Some(1),
                    Some(3),
                    Some(63),
                    Some(64),
                    Some(80),
                ] {
                    assert_eq!(table.get(over, under), map.get(&(over, under)).copied());
                }
            }
        }
    }

    #[test]
    fn groups_walk_metals_away_from_the_own_layer() {
        let layered = [
            (1, 0, 1, 0),
            (1, 2, 3, 1),
            (2, 0, 1, 2),
            (4, 0, 1, 3),
            (5, 0, 1, 4),
        ];
        let up: Vec<_> = Groups::new(&layered, 3, true).collect();
        let down: Vec<_> = Groups::new(&layered, 3, false).collect();
        assert_eq!(up, [(3, 4), (4, 5)]);
        assert_eq!(down, [(2, 3), (0, 2)]);
        assert_eq!(
            Groups::new(&layered, 2, true).collect::<Vec<_>>(),
            [(3, 4), (4, 5)]
        );
        assert_eq!(Groups::new(&layered, 1, false).count(), 0);
    }
}
