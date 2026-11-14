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
    /// Count passive strips that cannot record anything without sweeping
    /// them (`passive_without_plates`).
    pub passive_shortcut: bool,
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
            passive_shortcut: true,
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
    events: Vec<u64>,
    wide_events: Vec<u128>,
    layered: Vec<(u32, i64, i64, u32)>,
    contributions: Vec<(u32, f64, f64)>,
    face: FaceScratch,
    probes: Vec<Probe>,
    samples: SampleCache,
    sweep: Sweep,
    keys: Vec<u64>,
    spare: Vec<Cand>,
    spare_own: Vec<(Cand, u8)>,
}
impl Scratch {
    pub(super) fn new(instances: usize) -> Result<Self> {
        Ok(Self {
            instances: InstanceQueryScratch::new(instances)?,
            other: Vec::new(),
            own: Vec::new(),
            cands: Vec::new(),
            events: Vec::new(),
            wide_events: Vec::new(),
            layered: Vec::new(),
            contributions: Vec::new(),
            face: FaceScratch::default(),
            probes: Vec::new(),
            samples: SampleCache::default(),
            sweep: Sweep::default(),
            keys: Vec::new(),
            spare: Vec::new(),
            spare_own: Vec::new(),
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

/// The conductor shape of a grid entry. Wire bodies are shapes `0..runs`
/// (owner `Run(id)`) and pieces are contiguous from `piece_shape0`: their
/// owners follow from the ID, so only passive shapes are looked up.
#[inline]
fn shape_of(ctx: &Ctx<'_, '_>, id: u32, layer: u32, rect: Rect) -> Shape {
    if id < ctx.shapes.virtual_offset() as u32 {
        Shape {
            layer,
            rect,
            owner: Owner::Run(id),
        }
    } else if id.wrapping_sub(ctx.piece_shape0) < ctx.pieces.len() as u32 {
        Shape {
            layer,
            rect,
            owner: Owner::Piece(id - ctx.piece_shape0),
        }
    } else {
        ctx.grid.shape(ctx.shapes, id, layer, rect)
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
                    shape: shape_of(ctx, id, layer, rect),
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
    sort_by_id(&mut sc.other, |c| c.id, &mut sc.keys, &mut sc.spare);
    Ok(())
}

/// Sorts `v` by unique shape ID through packed `(ID, position)` keys,
/// moving each element once.
fn sort_by_id<T: Copy>(
    v: &mut Vec<T>,
    id: impl Fn(&T) -> u32,
    keys: &mut Vec<u64>,
    out: &mut Vec<T>,
) {
    if v.len() <= 1 {
        return;
    }
    keys.clear();
    keys.extend(
        v.iter()
            .enumerate()
            .map(|(i, c)| u64::from(id(c)) << 32 | i as u64),
    );
    keys.sort_unstable();
    out.clear();
    out.extend(keys.iter().map(|&k| v[k as u32 as usize]));
    std::mem::swap(v, out);
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
    sort_by_id(&mut sc.own, |c| c.0.id, &mut sc.keys, &mut sc.spare_own);
    counts.max_run_candidates = counts.max_run_candidates.max(base + sc.own.len());
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
        if run.passive.is_some() && ctx.passive_shortcut {
            match passive_without_plates(
                ctx,
                run,
                &sc.other,
                &sc.own,
                flag,
                &mut sc.events,
                work,
                counts,
            ) {
                Ok(true) => {
                    #[cfg(test)]
                    SHORTCUTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    continue;
                }
                Ok(false) => {}
                Err(stop) => {
                    sc.cands = cands;
                    return Err(stop);
                }
            }
        }
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
    let flags_of = |id: u32, r: Rect| {
        u8::from(accepts(x, Some((cx0, cx1)), id, r))
            | u8::from(accepts(y, Some((cy0, cy1)), id, r)) << 1
    };
    // The grid part: the x window, then the y window for hits the x strip
    // does not accept (every x hit strictly intersects the x window), so
    // each hit is taken once with both flags; the union's cells and entries
    // are charged exactly as one union query. A candidate cap reached here
    // replays the original union query from the original work count, so
    // the outcome and its work are the original's.
    if ctx.instances.is_none() {
        let charge = ctx.grid.query_work(x.layer, union, ctx.limits)?;
        let own = &mut sc.own;
        own.clear();
        let (mut nx, mut ny) = (0usize, 0usize);
        let mut fits = true;
        for (part, q) in [(1u8, qx), (2u8, qy)] {
            let complete = ctx.grid.visit_uncharged(x.layer, q, |id, r| {
                let flags = flags_of(id, r);
                if flags & part == 0 || (part == 2 && flags & 1 != 0) {
                    return Ok(true);
                }
                nx += usize::from(flags & 1);
                ny += usize::from(flags >> 1);
                if base + nx.max(ny) > cap {
                    fits = false;
                    return Ok(false);
                }
                own.push((
                    Cand {
                        id,
                        shape: shape_of(ctx, id, x.layer, r),
                    },
                    flags,
                ));
                Ok(true)
            })?;
            if !complete {
                break;
            }
        }
        if fits {
            *work = work
                .checked_add(charge)
                .filter(|&n| n <= ctx.limits.max_work)
                .ok_or_else(|| budget("extraction grid-index work"))?;
            sort_by_id(&mut sc.own, |c| c.0.id, &mut sc.keys, &mut sc.spare_own);
            return Ok(base + nx.max(ny));
        }
    }
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
        flags_of,
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
    sort_by_id(&mut sc.own, |c| c.0.id, &mut sc.keys, &mut sc.spare_own);
    Ok(base + nx.max(ny))
}

/// A passive strip records only plate shares toward routed conductors on
/// other layers: a share goes to the conductor under a side edge of the
/// nearest plate, the nearest calibration metal whose conductors, merged
/// across the strip, cover its cross-section with one strip width of margin
/// on each side. When no metal level can do that while it has routed metal
/// under a side edge, every interval of the strip records nothing, and only
/// its interval count and work charges remain (`Ok(true)`). Both follow
/// from the candidates' clamped extents along the strip: the intervals are
/// bounded by the strip ends and the distinct extents, and a candidate is
/// active from its entry to its exit, a degenerate one (entered at its own
/// exit) from its entry on. Errors and charges are those of `numeric`.
/// `Ok(false)` charges nothing: the strip may record something, or it has a
/// candidate on a layer without a model (an error once it spans), or its
/// extents do not pack; the full sweep then runs.
#[expect(
    clippy::too_many_arguments,
    reason = "the strip's candidates come as the shared pair lists"
)]
fn passive_without_plates(
    ctx: &Ctx<'_, '_>,
    run: &Run,
    other: &[Cand],
    own: &[(Cand, u8)],
    flag: u8,
    events: &mut Vec<u64>,
    work: &mut usize,
    counts: &mut Counts,
) -> std::result::Result<bool, Stop> {
    let cross = |r: Rect| {
        if run.vertical {
            (r.ly, r.uy, r.lx, r.ux)
        } else {
            (r.lx, r.ux, r.ly, r.uy)
        }
    };
    let (minus, plus) = (run.minus, run.plus);
    let margin = plus - minus;
    let (need_lo, need_hi) = (minus - margin, plus + margin);
    // Per metal level of the layered candidates: the hull of their cross
    // extents (a merged span lies inside it) and whether a routed one lies
    // under a side edge (the only owners a share can go to).
    let mut levels: [(u32, i64, i64, bool); 8] = [(0, 0, 0, false); 8];
    let mut used = 0;
    for c in other {
        let (a0, a1, x0, x1) = cross(c.shape.rect);
        if a0 >= a1 {
            continue;
        }
        let Some(metal) = ctx
            .layers
            .get(c.shape.layer as usize)
            .and_then(Option::as_ref)
            .map(|i| i.model.metal)
        else {
            return Ok(false);
        };
        if x1.min(plus) - x0.max(minus) <= 0 {
            continue;
        }
        let edge = c.shape.routed() && ((x0 < plus && x1 >= plus) || (x1 > minus && x0 <= minus));
        match levels[..used].iter().position(|l| l.0 == metal) {
            Some(i) => {
                let l = &mut levels[i];
                l.1 = l.1.min(x0);
                l.2 = l.2.max(x1);
                l.3 |= edge;
            }
            None if used < levels.len() => {
                levels[used] = (metal, x0, x1, edge);
                used += 1;
            }
            None => return Ok(false),
        }
    }
    if levels[..used]
        .iter()
        .any(|l| l.3 && l.1 <= need_lo && l.2 >= need_hi)
    {
        return Ok(false);
    }
    let span = match u64::try_from(i128::from(run.hi) - i128::from(run.lo)) {
        Ok(span) if span >> 62 == 0 => span,
        _ => return Ok(false),
    };
    // Entries (+1, low bit set) and exits (-1) relative to the strip start.
    events.clear();
    let mut n = 0usize;
    let cands = other
        .iter()
        .chain(own.iter().filter(|c| c.1 & flag != 0).map(|c| &c.0));
    for c in cands {
        let (a0, a1, _, _) = cross(c.shape.rect);
        let (enter, exit) = (a0.max(run.lo), a1.min(run.hi));
        if enter > exit || enter > run.hi || exit < run.lo {
            return Ok(false);
        }
        n += 1;
        events.push(((enter - run.lo) as u64) << 1 | 1);
        if enter < exit {
            events.push(((exit - run.lo) as u64) << 1);
        }
    }
    let event_count = 2 * n;
    let index_work = event_count
        .checked_mul(3 * event_count.max(1).ilog2() as usize + 4)
        .ok_or_else(|| budget("extraction interval index work"))?;
    *work = work
        .checked_add(index_work)
        .filter(|&w| w <= ctx.limits.max_work)
        .ok_or_else(|| budget("extraction interval index work"))?;
    events.sort_unstable();
    let (mut at, mut active, mut t0) = (0usize, 0usize, 0u64);
    let (mut intervals, mut charge) = (0usize, 0usize);
    loop {
        while at < events.len() && events[at] >> 1 <= t0 {
            if events[at] & 1 == 1 {
                active += 1;
            } else {
                active -= 1;
            }
            at += 1;
        }
        let t1 = events.get(at).map_or(span, |e| e >> 1);
        if t1 <= t0 {
            break;
        }
        intervals += 1;
        charge = charge
            .checked_add(active.max(1))
            .ok_or_else(|| budget("extraction work"))?;
        t0 = t1;
    }
    *work = work
        .checked_add(charge)
        .filter(|&w| w <= ctx.limits.max_work)
        .ok_or_else(|| budget("extraction work"))?;
    counts.passive_intervals += intervals;
    Ok(true)
}

/// Passive strips counted by `passive_without_plates` (tests).
#[cfg(test)]
pub(super) static SHORTCUTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[inline]
fn event(pos: i64, k: usize, enter: bool) -> u128 {
    (u128::from(pos as u64 ^ (1 << 63)) << 64) | ((k as u128) << 1) | u128::from(enter)
}
#[inline]
fn event_pos(e: u128) -> i64 {
    ((e >> 64) as u64 ^ (1 << 63)) as i64
}

/// A strip's sweep events packed into 64 bits: the offset from the strip
/// start above `shift` bits of candidate position and enter flag. Packed
/// order equals the order of `event` (position, then candidate, exit
/// first) whenever every position lies in `lo..=hi` and the offsets and
/// candidate positions fit; otherwise the strip keeps the 128-bit events.
#[derive(Clone, Copy)]
struct Packing {
    lo: i64,
    shift: u32,
}
impl Packing {
    /// The packing of a strip `lo..=hi` with `candidates` candidates.
    #[inline]
    fn new(lo: i64, hi: i64, candidates: usize) -> Option<Self> {
        let shift = usize::BITS - candidates.leading_zeros() + 1;
        let span = u64::try_from(i128::from(hi) - i128::from(lo)).ok()?;
        (shift < 64 && span >> (64 - shift) == 0).then_some(Self { lo, shift })
    }
    #[inline]
    fn event(self, pos: i64, k: usize, enter: bool) -> u64 {
        ((pos - self.lo) as u64) << self.shift | (k as u64) << 1 | u64::from(enter)
    }
    #[inline]
    fn pos(self, e: u64) -> i64 {
        self.lo + (e >> self.shift) as i64
    }
    #[inline]
    fn k(self, e: u64) -> usize {
        ((e & ((1 << self.shift) - 1)) >> 1) as usize
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
    /// Role in this strip's sweep, with the covered sides of `Cover`.
    role: Role,
    cover: u8,
    /// Same metal as the strip (own-layer metal touching or across a side
    /// edge only).
    same: bool,
    /// Routed (wire body or piece).
    routed: bool,
    /// Rank of a layered candidate in `(metal, x0, x1, position)` order.
    rank: u32,
    /// `x0 - plus` (plus side) or `minus - x1` (minus side).
    gap: i64,
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

/// A candidate's fixed role in one strip's sweep, decided once per strip
/// from its geometry (the original loop re-derived it every interval).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Never seen by the interval body: degenerate along the strip (it can
    /// never span an interval), or own-layer metal beyond the window.
    Ignore,
    /// Own-layer metal at or beyond the plus side edge, inside the window.
    Plus,
    /// Own-layer metal at or beyond the minus side edge, inside the window.
    Minus,
    /// Own-layer metal across a side edge of another net or class.
    Foreign,
    /// Same metal across a side edge (`cover` names the covered sides).
    Cover,
    /// Other-layer metal overlapping the strip face (a `layered` entry).
    Layered,
    /// Other-layer metal without an RC model: an error once it spans.
    NoModel,
}

/// Ascending sets of small integers (candidate positions or ranks) with a
/// summary word per 64 words, so iteration costs the occupied words only.
#[derive(Default)]
struct Set {
    words: Vec<u64>,
    summary: Vec<u64>,
}
impl Set {
    fn reset(&mut self, n: usize) {
        let words = n.div_ceil(64);
        self.words.clear();
        self.words.resize(words, 0);
        self.summary.clear();
        self.summary.resize(words.div_ceil(64), 0);
    }
    #[inline]
    fn insert(&mut self, i: usize) {
        let w = i / 64;
        self.words[w] |= 1 << (i % 64);
        self.summary[w / 64] |= 1 << (w % 64);
    }
    #[inline]
    fn remove(&mut self, i: usize) {
        let w = i / 64;
        self.words[w] &= !(1 << (i % 64));
        if self.words[w] == 0 {
            self.summary[w / 64] &= !(1 << (w % 64));
        }
    }
    /// Calls `f` with every member in ascending order.
    #[inline]
    fn for_each(&self, mut f: impl FnMut(usize)) {
        let mut bits = |w: usize| {
            let mut word = self.words[w];
            while word != 0 {
                f(w * 64 + word.trailing_zeros() as usize);
                word &= word - 1;
            }
        };
        if self.words.len() == 1 {
            bits(0);
            return;
        }
        for (s, &summary) in self.summary.iter().enumerate() {
            let mut summary = summary;
            while summary != 0 {
                bits(s * 64 + summary.trailing_zeros() as usize);
                summary &= summary - 1;
            }
        }
    }
}

/// The least `(gap, position)` of a set of lateral neighbours, kept across
/// insertions and recomputed from the set only after the least one leaves.
#[derive(Default)]
struct Nearest {
    best: Option<(i64, u32)>,
    stale: bool,
}
impl Nearest {
    #[inline]
    fn insert(&mut self, gap: i64, k: usize) {
        if !self.stale && self.best.is_none_or(|b| (gap, k as u32) < b) {
            self.best = Some((gap, k as u32));
        }
    }
    #[inline]
    fn remove(&mut self, k: usize) {
        if self.best.is_some_and(|b| b.1 == k as u32) {
            self.stale = true;
        }
    }
    /// The least member: the first minimal gap in ascending position.
    #[inline]
    fn get(&mut self, set: &Set, probes: &[Probe]) -> Option<(i64, u32)> {
        if self.stale {
            let mut best: Option<(i64, u32)> = None;
            set.for_each(|k| {
                let gap = probes[k].gap;
                if best.is_none_or(|b| gap < b.0) {
                    best = Some((gap, k as u32));
                }
            });
            (self.best, self.stale) = (best, false);
        }
        self.best
    }
}

/// One side's share of the context value per unit length.
#[derive(Clone, Copy, Default)]
enum Side {
    /// Inside same-net metal: not exposed.
    Covered,
    /// Touching same metal: not exposed.
    #[default]
    Skip,
    /// Exposed: ground `g` (before `max(0)`), up to two plate shares
    /// `(owner, d)` and a lateral coupling `(neighbour, cc)`.
    Open {
        g: f64,
        shares: [(u32, f64); 2],
        n: u8,
        lateral: Option<(u32, f64)>,
    },
}

/// Interval state of the previous interval of one strip.
type PlateState = (
    Option<(u32, [u32; 2])>,
    Option<(u32, [u32; 2])>,
    usize,
    (bool, bool),
    Option<usize>,
);
/// A side's nearest lateral neighbour and cover flag.
type SideKey = (Option<(i64, u32)>, bool);
#[derive(Default)]
struct IntervalCache {
    plates_valid: bool,
    layered: Vec<(u32, i64, i64, u32)>,
    plates: PlateState,
    nonspanning: bool,
    fallback: bool,
    /// Lower face portions `(has data, covered um, owners)`, upper ones
    /// `(has data, owners)`; owners `(shape, width um, ov * width um)`.
    lower: Vec<(bool, f64, usize, usize)>,
    upper: Vec<(bool, usize, usize)>,
    owners: Vec<(u32, f64, f64)>,
    sides_valid: bool,
    sides_key: [SideKey; 2],
    sides: [Side; 2],
}

/// Per-strip sweep state reused across strips.
#[derive(Default)]
pub(super) struct Sweep {
    cache: IntervalCache,
    /// Layered candidates in `(metal, x0, x1, position)` order.
    by_rank: Vec<(u32, i64, i64, u32)>,
    plus: Set,
    minus: Set,
    layered: Set,
}

/// The original per-strip interval sweep over ID-sorted candidates. Each
/// candidate's role (lateral neighbour per side, covering or foreign metal,
/// layered metal) depends only on the strip and the candidate, so it is
/// decided once; the sets of spanning candidates per role change only at
/// sweep events. Every interval sees exactly the neighbours, flags, counts
/// and the sorted `layered` list of the original loop, and evaluates the
/// same f64 expressions in the same order.
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
        wide_events,
        layered,
        contributions,
        face,
        probes,
        samples,
        sweep,
        ..
    } = sc;
    let tech = ctx.tech;
    let limits = ctx.limits;
    let info = ctx.info(run.layer)?;
    let model = info.model;
    let window_dbu = info.window_dbu;
    let (edge_minus, edge_plus) = (run.minus, run.plus);
    events.clear();
    wide_events.clear();
    probes.clear();
    let Sweep {
        cache,
        by_rank,
        plus: plus_set,
        minus: minus_set,
        layered: layered_set,
    } = sweep;
    by_rank.clear();
    cache.plates_valid = false;
    cache.sides_valid = false;
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
    let mut packing = Packing::new(run.lo, run.hi, cands.len());
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
        // Roles. A candidate spans an interval exactly while it is active
        // and `a0 < a1`: its clamped ends are then distinct interval
        // boundaries; a degenerate one (entered at its own exit) never
        // spans.
        let (mut role, mut cover, mut gap, mut same) = (Role::Ignore, 0u8, 0i64, false);
        if a0 < a1 {
            if metal == OWN {
                if x0 >= edge_plus {
                    gap = x0 - edge_plus;
                    if gap < window_dbu {
                        role = Role::Plus;
                        same = gap == 0 && same_metal(&c.shape);
                    }
                } else if x1 <= edge_minus {
                    gap = edge_minus - x1;
                    if gap < window_dbu {
                        role = Role::Minus;
                        same = gap == 0 && same_metal(&c.shape);
                    }
                } else if !same_metal(&c.shape) {
                    role = Role::Foreign;
                } else {
                    role = Role::Cover;
                    cover = u8::from(x1 > edge_plus) | u8::from(x0 < edge_minus) << 1;
                }
            } else if metal == NO_MODEL {
                role = Role::NoModel;
            } else if x1.min(edge_plus) - x0.max(edge_minus) > 0 {
                role = Role::Layered;
                by_rank.push((metal, x0, x1, k as u32));
            }
        }
        probes.push(Probe {
            role,
            cover,
            same,
            routed: c.shape.routed(),
            rank: 0,
            gap,
        });
        // A candidate has one entry and one exit, the exit no earlier: it
        // spans from its entry until its exit. A degenerate one (entered at
        // its own exit, which is ordered first and finds it inactive) stays
        // active from its entry on, so its exit is never stored.
        let (enter, exit) = (a0.max(run.lo), a1.min(run.hi));
        let degenerate = enter == exit;
        if let Some(p) = packing
            && enter <= run.hi
            && exit >= run.lo
            && enter <= exit
        {
            events.push(p.event(enter, k, true));
            if !degenerate {
                events.push(p.event(exit, k, false));
            }
        } else {
            if let Some(p) = packing.take() {
                // Unpack what was packed so far; the strip uses wide events.
                for &e in events.iter() {
                    wide_events.push(event(p.pos(e), p.k(e), e & 1 == 1));
                }
                events.clear();
            }
            wide_events.push(event(enter, k, true));
            if !degenerate {
                wide_events.push(event(exit, k, false));
            }
        }
    }
    by_rank.sort_unstable();
    for (r, e) in by_rank.iter().enumerate() {
        probes[e.3 as usize].rank = r as u32;
    }
    events.sort_unstable();
    wide_events.sort_unstable();
    // Every candidate counts two events in the index work, as when both
    // were stored.
    let event_count = 2 * cands.len();
    let stored = events.len() + wide_events.len();
    // Position, candidate and enter flag of the i-th sorted event.
    let at = |i: usize| -> (i64, usize, bool) {
        match packing {
            Some(p) => {
                let e = events[i];
                (p.pos(e), p.k(e), e & 1 == 1)
            }
            None => {
                let e = wide_events[i];
                (event_pos(e), ((e as u64) >> 1) as usize, e & 1 == 1)
            }
        }
    };
    let index_work = event_count
        .checked_mul(3 * event_count.max(1).ilog2() as usize + 4)
        .ok_or_else(|| budget("extraction interval index work"))?;
    *work = work
        .checked_add(index_work)
        .filter(|&n| n <= limits.max_work)
        .ok_or_else(|| budget("extraction interval index work"))?;
    let mut active_n = 0usize;
    let span = (run.hi - run.lo) as f64;
    // Passive strips attach to their routed partners only; a routed strip
    // attaches to its end nodes by position (pi split).
    let ends = match run.passive {
        None => {
            let local = ctx.net_index[&run.net];
            Some((
                ctx.nodes.local(local, run.node_lo),
                ctx.nodes.local(local, run.node_hi),
            ))
        }
        Some(_) => None,
    };
    let area_cap = model
        .area_cap
        .filter(|_| run.passive.is_none() && !(run.piece && run.vertical));
    plus_set.reset(cands.len());
    minus_set.reset(cands.len());
    layered_set.reset(by_rank.len());
    let (mut nearest_plus, mut nearest_minus) = (Nearest::default(), Nearest::default());
    // Spanning candidates per counted role.
    let (mut foreign_n, mut cover_plus_n, mut cover_minus_n) = (0usize, 0usize, 0usize);
    let (mut no_model_n, mut routed_layered_n) = (0usize, 0usize);
    // Whether the layered set changed since `layered` was last built.
    let mut layered_dirty = true;
    // Interval boundaries are run.lo, run.hi and every (clamped) candidate
    // end, ascending and distinct: exactly the original sorted point list.
    let mut event_index = 0;
    let mut t0 = run.lo;
    loop {
        let mut next = None;
        while event_index < stored {
            let (pos, k, enter) = at(event_index);
            if pos > t0 {
                next = Some(pos);
                break;
            }
            // +1 when the candidate starts spanning, -1 when it stops.
            let change = if enter {
                active_n += 1;
                1isize
            } else {
                active_n -= 1;
                -1
            };
            {
                let add = |n: &mut usize| {
                    *n = n.wrapping_add_signed(change);
                };
                let p = &probes[k];
                match p.role {
                    Role::Ignore => {}
                    Role::Plus if change > 0 => {
                        plus_set.insert(k);
                        nearest_plus.insert(p.gap, k);
                    }
                    Role::Plus => {
                        plus_set.remove(k);
                        nearest_plus.remove(k);
                    }
                    Role::Minus if change > 0 => {
                        minus_set.insert(k);
                        nearest_minus.insert(p.gap, k);
                    }
                    Role::Minus => {
                        minus_set.remove(k);
                        nearest_minus.remove(k);
                    }
                    Role::Foreign => add(&mut foreign_n),
                    Role::Cover => {
                        if p.cover & 1 != 0 {
                            add(&mut cover_plus_n);
                        }
                        if p.cover & 2 != 0 {
                            add(&mut cover_minus_n);
                        }
                    }
                    Role::Layered => {
                        if change > 0 {
                            layered_set.insert(p.rank as usize);
                        } else {
                            layered_set.remove(p.rank as usize);
                        }
                        layered_dirty = true;
                        if p.routed {
                            add(&mut routed_layered_n);
                        }
                    }
                    Role::NoModel => add(&mut no_model_n),
                }
            }
            event_index += 1;
        }
        let t1 = next.unwrap_or(run.hi);
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
        if no_model_n > 0 {
            return Err(invalid("routing layer has no RC profile entry").into());
        }
        // A passive strip records only plate shares toward routed metal of
        // other layers: without spanning routed layered metal the interval
        // records nothing and changes no other count.
        if run.passive.is_some() && routed_layered_n == 0 {
            t0 = t1;
            continue;
        }
        // Nearest lateral neighbour per side and nearest planes. Candidate
        // positions are in shape-ID order, so `(gap, position)` breaks ties
        // exactly like `(gap, shape ID)`; the first minimal gap in ascending
        // position is that minimum.
        let plus = nearest_plus.get(plus_set, probes);
        let minus = nearest_minus.get(minus_set, probes);
        let (covered_plus, covered_minus) = (cover_plus_n > 0, cover_minus_n > 0);
        if run.passive.is_none() {
            counts.foreign_overlaps += foreign_n;
        }
        // Plates, context and face coverage depend only on the sorted
        // `layered` list, and each side's template (its share of the
        // context value per unit length) only on that state and the side's
        // neighbour and cover flag: both are reused while unchanged, and
        // only the length-dependent products are evaluated per interval, as
        // the same f64 expressions in the same order.
        let mut plates_changed = false;
        // The sorted `layered` list changes only when a layered candidate
        // enters or leaves; until then it equals the cached list.
        let mut recompute = false;
        if layered_dirty || !cache.plates_valid {
            layered.clear();
            layered_set.for_each(|r| layered.push(by_rank[r]));
            layered_dirty = false;
            recompute = !cache.plates_valid || cache.layered != *layered;
        }
        if recompute {
            // A conductor on another layer is a plate for this strip (the
            // calibration OVER/UNDER contexts) only where its layer's metal
            // spans the whole strip cross-section and reaches one strip
            // width beyond both side edges: crossings and clearly wider
            // shapes.
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
            let nonspanning =
                nearest_below != below.map(|b| b.0) || nearest_above != above.map(|a| a.0);
            let over = below.map_or(0, |b| b.0);
            let under = above.map(|a| a.0);
            let ctx_map = &info.contexts;
            let (context, plane_split, fallback) = match ctx_map.get(over, under) {
                Some(i) => (i, (below.is_some(), above.is_some()), false),
                None => {
                    if let Some(i) = ctx_map.get(over, None).filter(|_| over > 0) {
                        (i, (true, false), true)
                    } else if let Some(i) = ctx_map.get(0, under) {
                        (i, (false, above.is_some()), true)
                    } else {
                        (info.substrate, (false, false), true)
                    }
                }
            };
            let below_only = if plane_split.0 && plane_split.1 {
                ctx_map.get(over, None)
            } else {
                None
            };
            let plates = (below, above, context, plane_split, below_only);
            plates_changed = !cache.plates_valid || plates != cache.plates;
            cache.plates = plates;
            cache.nonspanning = nonspanning;
            cache.fallback = fallback;
            // Overlap area of non-plate metal on other layers, counted once
            // from the upper face; a piece counts its face on its x strip
            // only.
            cache.lower.clear();
            cache.upper.clear();
            cache.owners.clear();
            if area_cap.is_some() {
                if below.is_none() {
                    face_portions(layered, model.metal, false, edge_minus, edge_plus, face);
                    for &(metal, covered, first, last) in &face.portions {
                        let ov = model.overlap.iter().find(|o| o.0 == metal).map(|o| o.1);
                        let start = cache.owners.len();
                        if let Some(ov) = ov {
                            for &(sh, w) in &face.owners[first..last] {
                                cache.owners.push((sh, um(tech, w), ov * um(tech, w)));
                            }
                        }
                        cache.lower.push((
                            ov.is_some(),
                            um(tech, covered),
                            start,
                            cache.owners.len(),
                        ));
                    }
                }
                if above.is_none() {
                    // A routed conductor above counts the overlap from its
                    // own face; passive metal does not model itself.
                    face_portions(layered, model.metal, true, edge_minus, edge_plus, face);
                    let passive = |o: &(u32, i64)| {
                        matches!(cands[o.0 as usize].shape.owner, Owner::Passive(_))
                    };
                    for &(metal, _, first, last) in &face.portions {
                        let owners = &face.owners[first..last];
                        if !owners.iter().any(passive) {
                            continue;
                        }
                        let upper = ctx
                            .metal_layer
                            .get(&metal)
                            .and_then(|&l| tech.layers[l].as_ref())
                            .and_then(|u| u.overlap.iter().find(|o| o.0 == model.metal));
                        let start = cache.owners.len();
                        if let Some(&(_, ov)) = upper {
                            for &(sh, w) in owners.iter().filter(|o| passive(o)) {
                                cache.owners.push((sh, um(tech, w), ov * um(tech, w)));
                            }
                        }
                        cache
                            .upper
                            .push((upper.is_some(), start, cache.owners.len()));
                    }
                }
            }
            std::mem::swap(&mut cache.layered, layered);
            cache.plates_valid = true;
        }
        let (below, above, context, plane_split, below_only) = cache.plates;
        if run.passive.is_none() && cache.nonspanning {
            counts.nonspanning_plane_intervals += 1;
        }
        if run.passive.is_none() && cache.fallback {
            counts.context_fallback_intervals += 1;
        }
        // Each side's template depends only on the plates and the side's
        // own neighbour and cover flag: a side is re-evaluated when either
        // changed.
        let keys = [(plus, covered_plus), (minus, covered_minus)];
        let all = plates_changed || !cache.sides_valid;
        if all || cache.sides_key != keys {
            let sub = &model.contexts[info.substrate].rows;
            let rows = &model.contexts[context].rows;
            let below_only = below_only.map(|i| &model.contexts[i].rows);
            for (k, (side, covered)) in keys.into_iter().enumerate() {
                if !all && cache.sides_key[k] == (side, covered) {
                    continue;
                }
                cache.sides[k] = if covered {
                    Side::Covered
                } else if side.is_some_and(|(g, s)| g == 0 && probes[s as usize].same) {
                    Side::Skip
                } else {
                    let spacing = side.map(|(g, _)| um(tech, g));
                    let (cc, cg) = samples.get(rows, spacing);
                    let (_, cg_sub) = samples.get(sub, spacing);
                    // Split this side's context value between the substrate
                    // and the planes. A conductor below is the plate of an
                    // OVER context: it shields the substrate, so the whole
                    // OVER value goes to it. A plane above takes only the
                    // excess over what goes down (the substrate share, or
                    // the OVER value).
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
                    let mut shares = [(0u32, 0.0f64); 2];
                    let mut n = 0;
                    // Each conductor's own cross-section is authoritative
                    // for its own field: a crossing is counted from both
                    // wires.
                    for (d, plane) in [(d_below, below), (d_above, above)] {
                        if d > 0.0 {
                            match plane {
                                Some((_, owners)) => {
                                    shares[n] = (owners[k], d);
                                    n += 1;
                                }
                                None => g += d,
                            }
                        }
                    }
                    // Lateral coupling between two runs is computed from
                    // both sides.
                    let lateral = side.filter(|_| cc > 0.0).map(|(_, s)| (s, cc));
                    Side::Open {
                        g,
                        shares,
                        n: n as u8,
                        lateral,
                    }
                };
            }
            cache.sides_key = keys;
            cache.sides_valid = true;
        }
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
        for side in &cache.sides {
            match *side {
                Side::Covered => {
                    if run.passive.is_none() {
                        counts.covered_side_intervals += 1;
                    }
                }
                Side::Skip => {}
                Side::Open {
                    g,
                    shares,
                    n,
                    lateral,
                } => {
                    for &(owner, d) in &shares[..n as usize] {
                        contributions.push((owner, d * length, 1.0));
                    }
                    ground += g.max(0.0) * length;
                    if let Some((s, cc)) = lateral {
                        contributions.push((s, cc * length, 0.5));
                    }
                }
            }
        }
        if let Some(area_cap) = area_cap {
            let mut shielded = 0.0;
            for &(data, covered, first, last) in &cache.lower {
                if !data {
                    counts.overlap_pairs_without_data += 1;
                    continue;
                }
                let area = covered * length;
                sink.op(Op::area(area))?;
                shielded += area_cap * area;
                for &(sh, _, ovw) in &cache.owners[first..last] {
                    contributions.push((sh, ovw * length, 1.0));
                }
            }
            for &(data, first, last) in &cache.upper {
                if !data {
                    counts.overlap_pairs_without_data += 1;
                    continue;
                }
                for &(sh, w, ovw) in &cache.owners[first..last] {
                    sink.op(Op::area(w * length))?;
                    contributions.push((sh, ovw * length, 1.0));
                }
            }
            ground = (ground - shielded).max(0.0);
        }
        let Some((g_lo, g_hi)) = ends else {
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
        };
        // Attach to this run's end nodes by position (pi split).
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
    fn packed_events_sort_and_decode_like_wide_events() {
        let mut seed = 5u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for (lo, hi, candidates) in [
            (-7i64, 9i64, 5usize),
            (0, 0, 1),
            (1 << 40, (1 << 40) + 3_000_000, 3000),
            (-(1 << 45), (1 << 45) - 1, 65_536),
        ] {
            let p = Packing::new(lo, hi, candidates).unwrap();
            let span = (hi - lo) as u64 + 1;
            let raw: Vec<(i64, usize, bool)> = (0..2000)
                .map(|i| {
                    let pos = match i % 4 {
                        0 => lo,
                        1 => hi,
                        _ => lo + next(span) as i64,
                    };
                    (pos, next(candidates as u64) as usize, next(2) == 1)
                })
                .collect();
            let mut wide: Vec<u128> = raw.iter().map(|&(a, k, e)| event(a, k, e)).collect();
            let mut packed: Vec<u64> = raw.iter().map(|&(a, k, e)| p.event(a, k, e)).collect();
            wide.sort_unstable();
            packed.sort_unstable();
            let wide: Vec<_> = wide
                .iter()
                .map(|&e| (event_pos(e), ((e as u64) >> 1) as usize, e & 1 == 1))
                .collect();
            let packed: Vec<_> = packed
                .iter()
                .map(|&e| (p.pos(e), p.k(e), e & 1 == 1))
                .collect();
            assert_eq!(packed, wide);
        }
        // Spans or candidate counts beyond 64 bits keep the wide events.
        assert!(Packing::new(i64::MIN, i64::MAX, 2).is_none());
        assert!(Packing::new(0, 1 << 50, 1 << 20).is_none());
        assert!(Packing::new(5, 4, 1).is_none());
    }

    #[test]
    fn sets_visit_their_members_in_ascending_order() {
        let mut seed = 9u64;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for n in [1usize, 63, 64, 65, 700, 5000] {
            let mut set = Set::default();
            set.reset(n);
            let mut want = std::collections::BTreeSet::new();
            for _ in 0..3 * n {
                let i = next(n as u64) as usize;
                if want.contains(&i) {
                    set.remove(i);
                    want.remove(&i);
                } else {
                    set.insert(i);
                    want.insert(i);
                }
                if next(17) == 0 {
                    let mut got = Vec::new();
                    set.for_each(|i| got.push(i));
                    assert_eq!(got, want.iter().copied().collect::<Vec<_>>());
                }
            }
            let mut got = Vec::new();
            set.for_each(|i| got.push(i));
            assert_eq!(got, want.iter().copied().collect::<Vec<_>>());
        }
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
