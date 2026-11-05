//! Same-net geometry rules for routed metal: notches and same-net spacing
//! between disjoint pieces (including touching corners), floating patches and
//! minimum area of merged per-layer components. Integer DBU only.
//!
//! Resource preflight (docs/routing-budget.md): shapes are grouped per
//! (net, routing layer); collinear default-width wire segments are merged
//! first, so a group holds O(route runs + vias + pin rectangles) rectangles.
//! Pairs are found by an lx-sorted sweep whose window is one spacing; coverage
//! tests subtract at most `MAX_PIECES` rectangles; component areas use a
//! coordinate-slab sweep, O(k^2) for k rectangles. Every unit of work is
//! charged to the caller's counter, bounded by `RouteLimits::max_comparisons`.
//! Nothing enumerates paths, cuts or polygons; memory is O(shapes).
//!
//! This is a native self-check only. Foundry Magic/KLayout decks remain the
//! independent DRC oracle (Rule 8).
use crate::routing::{
    Patch, RouteInput, RouteLimits, Routes, Terminal, Via, Wire, shifted, wire_rect,
};
use crate::{Error, Rect, Result};
use std::collections::BTreeMap;

const MAX_PIECES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Relation {
    /// Positive-area overlap or edge contact of positive length.
    Connected,
    /// At least one spacing apart (Euclidean at corners).
    Far,
    /// Closer than one spacing without connecting, or touching at a point.
    Near,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Origin {
    Pin,
    Route,
    Patch,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Shape {
    pub rect: Rect,
    pub origin: Origin,
}

#[derive(Clone, Debug)]
pub(crate) enum Violation {
    Spacing {
        net: u32,
        layer: u32,
        a: Rect,
        b: Rect,
    },
    Area {
        net: u32,
        layer: u32,
        area: i128,
        shapes: Vec<Rect>,
    },
    Floating {
        net: u32,
        layer: u32,
        rect: Rect,
    },
    /// Small hole enclosed by same-net metal (track router only); `rect` is
    /// its bounding box.
    Hole {
        net: u32,
        layer: u32,
        rect: Rect,
    },
    /// Two same-net rectangles overlap like stairs and their union narrows to
    /// the diagonal of the overlap, below the layer width (track router only).
    Neck {
        net: u32,
        layer: u32,
        a: Rect,
        b: Rect,
    },
    /// Metal of `owner` wider than the layer's wide-metal width runs parallel
    /// to metal of `other` closer than the wide spacing for `run` DBU. `net`
    /// is the one re-routed: the owner when cleanup patches widened it, else
    /// a routed neighbour. `wide` bounds every wide part facing `other` on
    /// that side and `near` every facing shape of `other`.
    Wide {
        net: u32,
        owner: u32,
        other: u32,
        layer: u32,
        wide: Rect,
        near: Rect,
        run: i64,
    },
}
impl Violation {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Spacing { .. } => "same-net spacing",
            Self::Area { .. } => "minimum area",
            Self::Floating { .. } => "floating patch",
            Self::Hole { .. } => "enclosed hole",
            Self::Neck { .. } => "minimum width neck",
            Self::Wide { .. } => "wide-metal spacing",
        }
    }
    pub(crate) fn layer(&self) -> u32 {
        match self {
            Self::Spacing { layer, .. }
            | Self::Area { layer, .. }
            | Self::Floating { layer, .. }
            | Self::Hole { layer, .. }
            | Self::Neck { layer, .. }
            | Self::Wide { layer, .. } => *layer,
        }
    }
    pub(crate) fn net(&self) -> u32 {
        match self {
            Self::Spacing { net, .. }
            | Self::Area { net, .. }
            | Self::Floating { net, .. }
            | Self::Hole { net, .. }
            | Self::Neck { net, .. }
            | Self::Wide { net, .. } => *net,
        }
    }
    pub(crate) fn describe(&self) -> String {
        let r = |r: &Rect| format!("({},{})-({},{})", r.lx, r.ly, r.ux, r.uy);
        match self {
            Self::Spacing { net, layer, a, b } => {
                format!(
                    "same-net notch/spacing net {net} layer {layer} {} {}",
                    r(a),
                    r(b)
                )
            }
            Self::Area {
                net,
                layer,
                area,
                shapes,
            } => format!(
                "minimum area net {net} layer {layer} area {area} DBU^2 near {}",
                shapes.first().map(r).unwrap_or_default()
            ),
            Self::Floating { net, layer, rect } => {
                format!("floating patch net {net} layer {layer} {}", r(rect))
            }
            Self::Hole { net, layer, rect } => {
                format!("enclosed hole net {net} layer {layer} {}", r(rect))
            }
            Self::Neck { net, layer, a, b } => {
                format!(
                    "minimum width neck net {net} layer {layer} {} {}",
                    r(a),
                    r(b)
                )
            }
            Self::Wide {
                owner,
                other,
                layer,
                wide,
                near,
                run,
                ..
            } => format!(
                "wide-metal spacing net {owner} vs net {other} layer {layer} run {run} {} {}",
                r(wide),
                r(near)
            ),
        }
    }
}

fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
fn charge(work: &mut usize, n: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(n)
        .ok_or_else(|| budget("routing rule work overflow"))?;
    if *work > cap {
        return Err(budget("routing rule work limit"));
    }
    Ok(())
}

fn overlap_x(a: Rect, b: Rect) -> i128 {
    i128::from(a.ux.min(b.ux)) - i128::from(a.lx.max(b.lx))
}
fn overlap_y(a: Rect, b: Rect) -> i128 {
    i128::from(a.uy.min(b.uy)) - i128::from(a.ly.max(b.ly))
}

pub(crate) fn relation(a: Rect, b: Rect, spacing: i64) -> Relation {
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    if (ox > 0 && oy >= 0) || (oy > 0 && ox >= 0) {
        return Relation::Connected;
    }
    let (dx, dy, s) = ((-ox).max(0), (-oy).max(0), i128::from(spacing));
    if dx >= s || dy >= s || (dx > 0 && dy > 0 && dx * dx + dy * dy >= s * s) {
        return Relation::Far;
    }
    Relation::Near
}

/// The region between two disjoint rectangles: overlap span by gap, or the
/// corner box. A zero-size dimension is a contact line (or point); then each
/// one-DBU strip beside the line (or each free corner square beside the
/// point) is a candidate. Covering either side joins both rectangles through
/// one polygon, and any point contact of that cover is its own near pair.
fn gap_regions(a: Rect, b: Rect) -> [Option<Rect>; 2] {
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    let (lx, ux) = if ox > 0 {
        (a.lx.max(b.lx), a.ux.min(b.ux))
    } else {
        (a.ux.min(b.ux), a.lx.max(b.lx))
    };
    let (ly, uy) = if oy > 0 {
        (a.ly.max(b.ly), a.uy.min(b.uy))
    } else {
        (a.uy.min(b.uy), a.ly.max(b.ly))
    };
    match (lx == ux, ly == uy) {
        (false, false) => [Some(Rect { lx, ly, ux, uy }), None],
        (true, false) => [
            Some(Rect {
                lx: lx - 1,
                ux: lx,
                ly,
                uy,
            }),
            Some(Rect {
                lx,
                ux: lx + 1,
                ly,
                uy,
            }),
        ],
        (false, true) => [
            Some(Rect {
                lx,
                ux,
                ly: ly - 1,
                uy: ly,
            }),
            Some(Rect {
                lx,
                ux,
                ly,
                uy: ly + 1,
            }),
        ],
        (true, true) => {
            // `a` and `b` hold opposite quadrants around the point.
            let square = |left: bool, below: bool| Rect {
                lx: if left { lx - 1 } else { lx },
                ux: if left { lx } else { lx + 1 },
                ly: if below { ly - 1 } else { ly },
                uy: if below { ly } else { ly + 1 },
            };
            let (left, below) = (a.ux <= lx, a.uy <= ly);
            [Some(square(left, !below)), Some(square(!left, below))]
        }
    }
}

/// True when some gap region of `a` and `b` lies in the union of `covers`.
fn gap_covered<I: Iterator<Item = Rect>>(
    a: Rect,
    b: Rect,
    mut covers: impl FnMut() -> I,
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    for region in gap_regions(a, b).into_iter().flatten() {
        if covered(region, covers(), work, cap)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// For a diagonal pair: is the corner of `a` that faces `b` no convex corner
/// of the merged metal, because `covers` continue `a` past it along one of its
/// two edges? The continuing shape is then nearer `b` and is its own pair.
fn corner_hidden<I: Iterator<Item = Rect>>(
    a: Rect,
    b: Rect,
    mut covers: impl FnMut() -> I,
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    let (right, up) = (b.lx >= a.ux, b.ly >= a.uy);
    let (x, y) = (
        if right { a.ux } else { a.lx },
        if up { a.uy } else { a.ly },
    );
    let (outer_x, inner_x) = if right { (x, x + 1) } else { (x - 1, x) };
    let (outer_y, inner_y) = if up { (y, y + 1) } else { (y - 1, y) };
    // Just past the corner in x at a's own edge row, and in y at its column.
    let along_x = Rect {
        lx: outer_x,
        ux: inner_x,
        ly: if up { y - 1 } else { y },
        uy: if up { y } else { y + 1 },
    };
    let along_y = Rect {
        lx: if right { x - 1 } else { x },
        ux: if right { x } else { x + 1 },
        ly: outer_y,
        uy: inner_y,
    };
    Ok(covered(along_x, covers(), work, cap)? || covered(along_y, covers(), work, cap)?)
}

/// A near pair is a violation unless the metal between it is covered or, for
/// a diagonal pair, one of the two facing corners is not a real corner.
fn near_ok<I: Iterator<Item = Rect>>(
    a: Rect,
    b: Rect,
    mut covers: impl FnMut() -> I,
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    if overlap_x(a, b) < 0
        && overlap_y(a, b) < 0
        && (corner_hidden(a, b, &mut covers, work, cap)?
            || corner_hidden(b, a, &mut covers, work, cap)?)
    {
        return Ok(true);
    }
    gap_covered(a, b, covers, work, cap)
}

/// Bounding box of every candidate gap region of `a` and `b`.
fn gap_hull(a: Rect, b: Rect) -> Rect {
    gap_regions(a, b)
        .into_iter()
        .flatten()
        .reduce(|h, r| Rect {
            lx: h.lx.min(r.lx),
            ly: h.ly.min(r.ly),
            ux: h.ux.max(r.ux),
            uy: h.uy.max(r.uy),
        })
        .expect("a gap has a region")
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

/// `a - b` as at most four disjoint rectangles.
fn subtract(a: Rect, b: Rect, out: &mut Vec<Rect>) {
    if !intersects(a, b) {
        out.push(a);
        return;
    }
    if b.ly > a.ly {
        out.push(Rect { uy: b.ly, ..a });
    }
    if b.uy < a.uy {
        out.push(Rect { ly: b.uy, ..a });
    }
    let (ly, uy) = (a.ly.max(b.ly), a.uy.min(b.uy));
    if b.lx > a.lx {
        out.push(Rect {
            lx: a.lx,
            ux: b.lx,
            ly,
            uy,
        });
    }
    if b.ux < a.ux {
        out.push(Rect {
            lx: b.ux,
            ux: a.ux,
            ly,
            uy,
        });
    }
}

/// True only when every point of `region` lies in the union of `covers`.
/// Fragmentation beyond `MAX_PIECES` is treated as uncovered (conservative).
fn covered(
    region: Rect,
    covers: impl Iterator<Item = Rect>,
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    let mut pieces = vec![region];
    for c in covers {
        charge(work, pieces.len(), cap)?;
        if !intersects(c, region) {
            continue;
        }
        let mut next = Vec::with_capacity(pieces.len() + 3);
        for p in pieces {
            subtract(p, c, &mut next);
        }
        if next.len() > MAX_PIECES {
            return Ok(false);
        }
        pieces = next;
        if pieces.is_empty() {
            return Ok(true);
        }
    }
    Ok(pieces.is_empty())
}

/// A candidate shape is acceptable against same-net `others` when every near
/// pair has its gap region fully covered by the other same-net rectangles.
pub(crate) fn local_ok(
    rect: Rect,
    others: &[Rect],
    spacing: i64,
    work: &mut usize,
    cap: usize,
) -> Result<bool> {
    for (i, o) in others.iter().enumerate() {
        charge(work, 1, cap)?;
        if relation(rect, *o, spacing) != Relation::Near {
            continue;
        }
        let rest = || {
            others
                .iter()
                .enumerate()
                .filter(move |(j, _)| *j != i)
                .map(|(_, r)| *r)
        };
        if !near_ok(rect, *o, rest, work, cap)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn floor_to(v: i64, g: i64) -> i64 {
    v.div_euclid(g) * g
}
fn ceil_to(v: i64, g: i64) -> i64 {
    -((-v).div_euclid(g) * g)
}

/// Patch metal that removes a near pair: the full gap across the projection,
/// widened to the layer width when the projection is narrower, or a square
/// bridge overlapping both shapes for corner/point configurations.
pub(crate) fn fill_for(a: Rect, b: Rect, width: i64, grid: i64) -> Rect {
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    let hull = Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    };
    let widen = |lo: i64, hi: i64, min: i64, max: i64| -> (i64, i64) {
        if hi - lo >= width {
            return (lo, hi);
        }
        let mut l = lo - (width - (hi - lo)) / 2;
        let mut h = l + width;
        if l < min {
            h += min - l;
            l = min;
        }
        if h > max {
            l -= h - max;
            h = max;
        }
        (l.max(min), h.min(max))
    };
    let mut r = if ox > 0 {
        let (lx, ux) = widen(a.lx.max(b.lx), a.ux.min(b.ux), hull.lx, hull.ux);
        Rect {
            lx,
            ux,
            ly: a.uy.min(b.uy),
            uy: a.ly.max(b.ly),
        }
    } else if oy > 0 {
        let (ly, uy) = widen(a.ly.max(b.ly), a.uy.min(b.uy), hull.ly, hull.uy);
        Rect {
            lx: a.ux.min(b.ux),
            ux: a.lx.max(b.lx),
            ly,
            uy,
        }
    } else {
        let (gx, gy) = ((-ox).max(0) as i64, (-oy).max(0) as i64);
        let t = ceil_to(((width - gx.min(gy)).max(2 * grid) + 1) / 2, grid);
        Rect {
            lx: a.ux.min(b.ux) - t,
            ux: a.lx.max(b.lx) + t,
            ly: a.uy.min(b.uy) - t,
            uy: a.ly.max(b.ly) + t,
        }
    };
    r.lx = floor_to(r.lx, grid);
    r.ly = floor_to(r.ly, grid);
    r.ux = ceil_to(r.ux, grid);
    r.uy = ceil_to(r.uy, grid);
    r
}

/// Track-router patch: like `fill_for`, but a corner/point bridge overlaps
/// each shape by at least the layer width (or the shape's full extent) in
/// both axes, so the union never forms a sub-width neck at the junction.
pub(crate) fn fill_for_robust(a: Rect, b: Rect, width: i64, grid: i64) -> Rect {
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    // Facing spans of at least one width: the legacy gap fill already has
    // full-width contact with both shapes.
    if ox >= i128::from(width) || oy >= i128::from(width) {
        return fill_for(a, b, width, grid);
    }
    // Corner or narrow facing span: the bounding box of the parts of each
    // shape within two widths of the other. It contains both facing corners,
    // so the union has no sub-width step or diagonal neck there.
    let reach = 2 * width;
    let near = |s: Rect, o: Rect| Rect {
        lx: s.lx.max(o.lx - reach),
        ly: s.ly.max(o.ly - reach),
        ux: s.ux.min(o.ux + reach),
        uy: s.uy.min(o.uy + reach),
    };
    let (pa, pb) = (near(a, b), near(b, a));
    let r = Rect {
        lx: pa.lx.min(pb.lx),
        ly: pa.ly.min(pb.ly),
        ux: pa.ux.max(pb.ux),
        uy: pa.uy.max(pb.uy),
    };
    Rect {
        lx: floor_to(r.lx, grid),
        ly: floor_to(r.ly, grid),
        ux: ceil_to(r.ux, grid),
        uy: ceil_to(r.uy, grid),
    }
}

/// Smaller same-net bridges tried when the `fill_for_robust` box touches
/// another net (two pads of one net a jog apart). Diagonal pairs: the two
/// L shapes, each leg as thick as the shape it extends; facing pairs: a bar
/// across the gap as tall (wide) as either shape, at least one width. With
/// `stair` (`RouteQor::notch_bridge`), facing pairs first try the stair
/// bridge (when they face over less than a width) and the bars clipped to
/// two widths past the gap; without it the candidates are exactly the legacy
/// ones. The caller checks clearance and re-runs the rule check on the result.
pub(crate) fn bridges(a: Rect, b: Rect, width: i64, grid: i64, stair: bool) -> Vec<Vec<Rect>> {
    let snap = |r: Rect| Rect {
        lx: floor_to(r.lx, grid),
        ly: floor_to(r.ly, grid),
        ux: ceil_to(r.ux, grid),
        uy: ceil_to(r.uy, grid),
    };
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    let (lx, ux) = (a.lx.min(b.lx), a.ux.max(b.ux));
    let (ly, uy) = (a.ly.min(b.ly), a.uy.max(b.uy));
    // [lo, hi) widened about its centre to at least one width.
    let span = |lo: i64, hi: i64| -> (i64, i64) {
        let grow = (width - (hi - lo)).max(0);
        (lo - grow / 2, hi + grow - grow / 2)
    };
    let mut out = Vec::new();
    if ox <= 0 && oy <= 0 {
        // `s` runs along y to the other's band, then a bar along x at that band.
        for (s, o) in [(a, b), (b, a)] {
            out.push(vec![
                snap(Rect {
                    lx: s.lx,
                    ly,
                    ux: s.ux,
                    uy,
                }),
                snap(Rect {
                    lx,
                    ly: o.ly,
                    ux,
                    uy: o.uy,
                }),
            ]);
        }
    } else {
        // Facing pair: work in (u, v) with the gap along v; `t` maps back.
        let across_x = ox <= 0;
        let t = |r: Rect| if across_x { transpose(r) } else { r };
        let (ta, tb) = (t(a), t(b));
        let overlap = if across_x { oy } else { ox };
        if stair && overlap < i128::from(width) {
            out.push(vec![snap(t(stair_bridge(ta, tb, width)))]);
        }
        let (hull_lo, hull_hi) = (ta.ly.min(tb.ly), ta.uy.max(tb.uy));
        let (gap_lo, gap_hi) = (ta.uy.min(tb.uy), ta.ly.max(tb.ly));
        // Bars clipped to two widths past the gap first, then full length.
        let (near_lo, near_hi) = (
            (gap_lo - 2 * width).max(hull_lo),
            (gap_hi + 2 * width).min(hull_hi),
        );
        if stair && (near_lo, near_hi) != (hull_lo, hull_hi) {
            for s in [ta, tb] {
                let (u0, u1) = span(s.lx, s.ux);
                out.push(vec![snap(t(Rect {
                    lx: u0,
                    ly: near_lo,
                    ux: u1,
                    uy: near_hi,
                }))]);
            }
        }
        for s in [ta, tb] {
            let (u0, u1) = span(s.lx, s.ux);
            out.push(vec![snap(t(Rect {
                lx: u0,
                ly: hull_lo,
                ux: u1,
                uy: hull_hi,
            }))]);
        }
    }
    out
}

/// Stair bridge of two same-net shapes that face each other across a gap
/// over a span narrower than the width (a stair-step notch, e.g. a via pad
/// half a track beside its own escape): one bar across the gap that spans
/// both shapes' extents along the facing axis (at most one width past the
/// facing span) and is at least one width thick across the gap. The union is
/// then a Z whose inner corners are at least `width` apart, so it has no
/// sub-width neck, and the bar touches no metal beyond the two shapes' own
/// extents plus one width. In (u, v) coordinates: the gap runs along v.
fn stair_bridge(a: Rect, b: Rect, width: i64) -> Rect {
    let (span_lo, span_hi) = (a.lx.max(b.lx), a.ux.min(b.ux));
    let u0 = a.lx.min(b.lx).max(span_lo - width);
    let u1 = a.ux.max(b.ux).min(span_hi + width);
    let (gap_lo, gap_hi) = (a.uy.min(b.uy), a.ly.max(b.ly));
    let grow = (width - (gap_hi - gap_lo)).max(0);
    let (hull_lo, hull_hi) = (a.ly.min(b.ly), a.uy.max(b.uy));
    Rect {
        lx: u0,
        ly: (gap_lo - grow / 2).max(hull_lo),
        ux: u1,
        uy: (gap_hi + grow - grow / 2).min(hull_hi),
    }
}

/// Same-net fills for a near pair that stay within two widths of its gap:
/// the robust box, then the bridges that fit in that window. These are the
/// fills worth reserving room for when other nets block all of them.
pub(crate) fn local_fills(a: Rect, b: Rect, width: i64, grid: i64) -> Vec<Vec<Rect>> {
    let window = {
        let g = gap_hull(a, b);
        Rect {
            lx: g.lx - 2 * width - grid,
            ly: g.ly - 2 * width - grid,
            ux: g.ux + 2 * width + grid,
            uy: g.uy + 2 * width + grid,
        }
    };
    let inside =
        |r: &Rect| window.lx <= r.lx && r.ux <= window.ux && window.ly <= r.ly && r.uy <= window.uy;
    let mut out = vec![vec![fill_for_robust(a, b, width, grid)]];
    out.extend(bridges(a, b, width, grid, true));
    out.retain(|rects| rects.iter().all(inside));
    out.dedup();
    out
}

fn transpose(r: Rect) -> Rect {
    Rect {
        lx: r.ly,
        ly: r.lx,
        ux: r.uy,
        uy: r.ux,
    }
}

/// Bounding boxes of holes enclosed by a rectangle union. Small planes use
/// dense flood fill; sparse large planes use a bounded sweep frontier.
pub(crate) fn holes(
    rects: &[Rect],
    work: &mut usize,
    cap: usize,
    max_cells: usize,
) -> Result<Vec<Rect>> {
    if (rects.len() as u64).saturating_mul(32) > (max_cells as u64).saturating_mul(64) {
        return Err(Error::Budget(
            "same-net compressed hole grid coordinates".into(),
        ));
    }
    if rects.iter().any(|r| !r.valid()) {
        return Err(Error::Invalid("invalid hole-detection rectangle".into()));
    }
    charge(
        work,
        rects
            .len()
            .saturating_mul(rects.len().max(1).ilog2() as usize + 1)
            .saturating_mul(2),
        cap,
    )?;
    let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r.lx, r.ux]).collect();
    let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r.ly, r.uy]).collect();
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    if xs.len() < 2 || ys.len() < 2 {
        return Ok(Vec::new());
    }
    let (nx, ny) = (xs.len() - 1, ys.len() - 1);
    let Some(cells) = nx.checked_mul(ny).filter(|&n| n <= max_cells) else {
        return crate::hole_sweep::find(rects, &xs, &ys, work, cap, max_cells);
    };
    charge(work, cells, cap)?;
    let mut covered = vec![false; cells];
    for r in rects {
        let (x0, x1) = (
            xs.binary_search(&r.lx).unwrap(),
            xs.binary_search(&r.ux).unwrap(),
        );
        let (y0, y1) = (
            ys.binary_search(&r.ly).unwrap(),
            ys.binary_search(&r.uy).unwrap(),
        );
        charge(work, (x1 - x0) * (y1 - y0), cap)?;
        for y in y0..y1 {
            for x in x0..x1 {
                covered[y * nx + x] = true;
            }
        }
    }
    // 0 = unvisited free, 1 = outside, 2.. = hole label.
    let mut label = vec![0u32; cells];
    let mut stack = Vec::new();
    for y in 0..ny {
        for x in 0..nx {
            if (x == 0 || y == 0 || x + 1 == nx || y + 1 == ny) && !covered[y * nx + x] {
                label[y * nx + x] = 1;
                stack.push(y * nx + x);
            }
        }
    }
    let mut next = 1u32;
    let mut seed = 0usize;
    loop {
        while let Some(c) = stack.pop() {
            let (x, y) = (c % nx, c / nx);
            let visit = |n: usize, stack: &mut Vec<usize>, label: &mut [u32], l: u32| {
                if !covered[n] && label[n] == 0 {
                    label[n] = l;
                    stack.push(n);
                }
            };
            let l = label[c];
            if x > 0 {
                visit(c - 1, &mut stack, &mut label, l);
            }
            if x + 1 < nx {
                visit(c + 1, &mut stack, &mut label, l);
            }
            if y > 0 {
                visit(c - nx, &mut stack, &mut label, l);
            }
            if y + 1 < ny {
                visit(c + nx, &mut stack, &mut label, l);
            }
        }
        while seed < nx * ny && (covered[seed] || label[seed] != 0) {
            seed += 1;
        }
        if seed == nx * ny {
            break;
        }
        next += 1;
        label[seed] = next;
        stack.push(seed);
    }
    let mut boxes: BTreeMap<u32, Rect> = BTreeMap::new();
    for y in 0..ny {
        for x in 0..nx {
            let l = label[y * nx + x];
            if l >= 2 {
                let cell = Rect {
                    lx: xs[x],
                    ly: ys[y],
                    ux: xs[x + 1],
                    uy: ys[y + 1],
                };
                boxes
                    .entry(l)
                    .and_modify(|b| {
                        b.lx = b.lx.min(cell.lx);
                        b.ly = b.ly.min(cell.ly);
                        b.ux = b.ux.max(cell.ux);
                        b.uy = b.uy.max(cell.uy);
                    })
                    .or_insert(cell);
            }
        }
    }
    Ok(boxes.into_values().collect())
}

/// Exact area of a rectangle union by x slabs.
pub(crate) fn union_area(rects: &[Rect], work: &mut usize, cap: usize) -> Result<i128> {
    let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r.lx, r.ux]).collect();
    xs.sort_unstable();
    xs.dedup();
    let mut area = 0i128;
    let mut spans = Vec::new();
    for w in xs.windows(2) {
        charge(work, rects.len(), cap)?;
        spans.clear();
        spans.extend(
            rects
                .iter()
                .filter(|r| r.lx <= w[0] && r.ux >= w[1])
                .map(|r| (r.ly, r.uy)),
        );
        spans.sort_unstable();
        let mut covered = 0i128;
        let mut current: Option<(i64, i64)> = None;
        for &(l, u) in &spans {
            match current {
                Some((cl, cu)) if l <= cu => current = Some((cl, cu.max(u))),
                Some((cl, cu)) => {
                    covered += i128::from(cu) - i128::from(cl);
                    current = Some((l, u));
                }
                None => current = Some((l, u)),
            }
        }
        if let Some((cl, cu)) = current {
            covered += i128::from(cu) - i128::from(cl);
        }
        area += covered * (i128::from(w[1]) - i128::from(w[0]));
    }
    Ok(area)
}

fn area_violation(
    rects: &[Rect],
    minimum: i128,
    work: &mut usize,
    cap: usize,
) -> Result<Option<i128>> {
    charge(work, rects.len(), cap)?;
    if rects.iter().any(|r| {
        let width = i128::from(r.ux) - i128::from(r.lx);
        let height = i128::from(r.uy) - i128::from(r.ly);
        width.checked_mul(height).is_none_or(|area| area >= minimum)
    }) {
        return Ok(None);
    }
    let area = union_area(rects, work, cap)?;
    Ok((area < minimum).then_some(area))
}

/// Merge collinear same-width segments into maximal runs (check view only).
fn merge_runs(mut horizontal: Vec<Rect>, mut vertical: Vec<Rect>) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::with_capacity(horizontal.len() + vertical.len());
    horizontal.sort_unstable_by_key(|r| (r.ly, r.uy, r.lx, r.ux));
    for r in horizontal {
        match out.last_mut() {
            Some(last) if last.ly == r.ly && last.uy == r.uy && r.lx <= last.ux => {
                last.ux = last.ux.max(r.ux)
            }
            _ => out.push(r),
        }
    }
    let split = out.len();
    vertical.sort_unstable_by_key(|r| (r.lx, r.ux, r.ly, r.uy));
    for r in vertical {
        match out[split..].last_mut() {
            Some(last) if last.lx == r.lx && last.ux == r.ux && r.ly <= last.uy => {
                last.uy = last.uy.max(r.uy)
            }
            _ => out.push(r),
        }
    }
    out
}

/// All same-net metal per (net, routing layer), sorted by lx.
#[cfg(test)]
pub(crate) fn collect(
    input: &RouteInput,
    routes: &Routes,
) -> Result<BTreeMap<(u32, u32), Vec<Shape>>> {
    let mut selected = routes.nets.clone();
    selected.sort_unstable();
    selected.dedup();
    collect_parts(
        input,
        routes.wires.iter(),
        routes.vias.iter(),
        routes.patches.iter(),
        input.terminals.iter(),
        &selected,
    )
}

pub(crate) fn collect_parts<'a>(
    input: &RouteInput,
    wires: impl Iterator<Item = &'a Wire>,
    vias: impl Iterator<Item = &'a Via>,
    patches: impl Iterator<Item = &'a Patch>,
    terminals: impl Iterator<Item = &'a Terminal>,
    selected: &[u32],
) -> Result<BTreeMap<(u32, u32), Vec<Shape>>> {
    let routing = |l: u32| {
        input
            .layers
            .get(l as usize)
            .is_some_and(|l| l.vertical.is_some())
    };
    let mut runs = BTreeMap::<(u32, u32), (Vec<Rect>, Vec<Rect>)>::new();
    for w in wires {
        let rect = wire_rect(w, input.layers[w.layer as usize].width)?;
        let e = runs.entry((w.net, w.layer)).or_default();
        if w.start[1] == w.end[1] {
            e.0.push(rect);
        } else {
            e.1.push(rect);
        }
    }
    let mut groups = BTreeMap::<(u32, u32), Vec<Shape>>::new();
    for (key, (h, v)) in runs {
        groups
            .entry(key)
            .or_default()
            .extend(merge_runs(h, v).into_iter().map(|rect| Shape {
                rect,
                origin: Origin::Route,
            }));
    }
    for v in vias {
        for s in &input.vias[v.kind as usize].shapes {
            if routing(s.layer) {
                groups.entry((v.net, s.layer)).or_default().push(Shape {
                    rect: shifted(s.rect, v.at)?,
                    origin: Origin::Route,
                });
            }
        }
    }
    for p in patches {
        groups.entry((p.net, p.layer)).or_default().push(Shape {
            rect: p.rect,
            origin: Origin::Patch,
        });
    }
    for t in terminals {
        if selected.binary_search(&t.net).is_err() {
            continue;
        }
        for s in &t.shapes {
            if let Some(g) = groups.get_mut(&(t.net, s.layer)) {
                g.push(Shape {
                    rect: s.rect,
                    origin: Origin::Pin,
                });
            }
        }
    }
    for g in groups.values_mut() {
        g.sort_unstable_by_key(|s| (s.rect.lx, s.rect.ly, s.rect.ux, s.rect.uy, s.origin));
        g.dedup_by(|a, b| a.rect == b.rect && a.origin == b.origin);
    }
    Ok(groups)
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn push_violation(
    out: &mut Vec<Violation>,
    shape_count: &mut usize,
    value: Violation,
    cap: usize,
) -> Result<()> {
    let shapes = match &value {
        Violation::Area { shapes, .. } => shapes.len(),
        _ => 0,
    };
    if out.len() >= cap {
        return Err(Error::Budget("same-net diagnostic count".into()));
    }
    *shape_count = shape_count
        .checked_add(shapes)
        .filter(|&n| n <= cap.saturating_mul(16))
        .ok_or_else(|| Error::Budget("same-net diagnostic shape count".into()))?;
    out.push(value);
    Ok(())
}

/// Every same-net rule violation of the stored geometry, deterministic order.
pub(crate) fn check(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    work: &mut usize,
) -> Result<Vec<Violation>> {
    check_with(input, routes, limits, work, None)
}

/// `check`, reusing the input's terminal order (same result and work).
pub(crate) fn check_with(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    work: &mut usize,
    terminals: Option<&crate::rule_index::TerminalOrder>,
) -> Result<Vec<Violation>> {
    let cap = limits.max_comparisons;
    let index = match terminals {
        Some(terminals) => {
            crate::rule_index::RuleIndex::with_terminals(input, routes, work, cap, terminals)?
        }
        None => crate::rule_index::RuleIndex::new(input, routes, work, cap)?,
    };
    let mut out = Vec::new();
    let mut diagnostic_shapes = 0;
    for &net in index.nets() {
        check_net(
            input,
            routes,
            &index,
            net,
            limits,
            work,
            &mut out,
            &mut diagnostic_shapes,
        )?;
    }
    Ok(out)
}

/// Whether the stored geometry has no same-net rule violation, decided net
/// by net on the rayon pool. `Some(work)` when every net is clean, with the
/// work `check` would charge after `start` (nets are independent and every
/// charge is `work <= cap`, so `check` succeeds exactly when this total fits);
/// `None` when a net has a violation or fails, so the caller must run `check`
/// for the canonical result.
pub(crate) fn clean_parallel(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    start: usize,
) -> Option<usize> {
    use rayon::prelude::*;
    let cap = limits.max_comparisons;
    let mut work = start;
    let index = crate::rule_index::RuleIndex::new(input, routes, &mut work, cap).ok()?;
    let nets = index.nets();
    let parts: Vec<Option<usize>> = nets
        .par_chunks(nets.len().div_ceil(rayon::current_num_threads() * 8).max(1))
        .map(|chunk| {
            let mut total = 0usize;
            let (mut out, mut shapes) = (Vec::new(), 0);
            for &net in chunk {
                let mut w = 0;
                check_net(
                    input,
                    routes,
                    &index,
                    net,
                    limits,
                    &mut w,
                    &mut out,
                    &mut shapes,
                )
                .ok()?;
                if !out.is_empty() {
                    return None;
                }
                total = total.saturating_add(w);
            }
            Some(total)
        })
        .collect();
    parts
        .into_iter()
        .try_fold(work, |sum, part| part.map(|w| sum.saturating_add(w)))
        .filter(|&total| total <= cap)
}

/// The same-net rule checks of one net (all of its routing layers).
#[expect(
    clippy::too_many_arguments,
    reason = "shared counters of the sequential pass stay explicit"
)]
fn check_net(
    input: &RouteInput,
    routes: &Routes,
    index: &crate::rule_index::RuleIndex,
    net: u32,
    limits: &RouteLimits,
    work: &mut usize,
    out: &mut Vec<Violation>,
    diagnostic_shapes: &mut usize,
) -> Result<()> {
    let groups = index.collect(input, routes, net, limits, work)?;
    for (&(net, layer), shapes) in &groups {
        check_group(
            input,
            (net, layer),
            shapes,
            limits,
            work,
            out,
            diagnostic_shapes,
        )?;
    }
    Ok(())
}

/// The same-net rule checks of one (net, routing layer) group of collected
/// shapes (`collect_parts`): the per-layer body of `check_net`, also used by
/// the track router's straightening to compare a net's layer before and
/// after a rewrite.
pub(crate) fn check_group(
    input: &RouteInput,
    (net, layer): (u32, u32),
    shapes: &[Shape],
    limits: &RouteLimits,
    work: &mut usize,
    out: &mut Vec<Violation>,
    diagnostic_shapes: &mut usize,
) -> Result<()> {
    let cap = limits.max_comparisons;
    let rule = &input.layers[layer as usize];
    let s = rule.spacing;
    let n = shapes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        let a = shapes[i];
        for (j, b) in shapes.iter().enumerate().skip(i + 1) {
            if i128::from(b.rect.lx) >= i128::from(a.rect.ux) + i128::from(s) {
                break;
            }
            charge(work, 1, cap)?;
            match relation(a.rect, b.rect, s) {
                Relation::Connected => {
                    let (x, y) = (find(&mut parent, i), find(&mut parent, j));
                    parent[x.max(y)] = x.min(y);
                    if input.tracks.is_none()
                        || (a.origin == Origin::Pin && b.origin == Origin::Pin)
                    {
                        continue;
                    }
                    if let Some((probes, _)) = neck(a.rect, b.rect, rule.width) {
                        charge(work, n, cap)?;
                        let closed = shapes.iter().enumerate().any(|(k, c)| {
                            k != i && k != j && probes.iter().any(|p| clip(*p, c.rect).is_some())
                        });
                        if !closed {
                            push_violation(
                                out,
                                diagnostic_shapes,
                                Violation::Neck {
                                    net,
                                    layer,
                                    a: a.rect,
                                    b: b.rect,
                                },
                                limits.max_rule_violations,
                            )?;
                        }
                    }
                }
                Relation::Far => {}
                Relation::Near => {
                    if a.origin == Origin::Pin && b.origin == Origin::Pin {
                        continue;
                    }
                    // One DBU wider: corner tests look just outside it.
                    let hull = gap_hull(a.rect, b.rect);
                    let region = Rect {
                        lx: hull.lx - 1,
                        ly: hull.ly - 1,
                        ux: hull.ux + 1,
                        uy: hull.uy + 1,
                    };
                    let rest = || {
                        shapes
                            .iter()
                            .enumerate()
                            .filter(move |(k, c)| {
                                *k != i
                                    && *k != j
                                    && c.rect.lx < region.ux
                                    && c.rect.ux > region.lx
                                    && c.rect.ly < region.uy
                                    && c.rect.uy > region.ly
                            })
                            .map(|(_, c)| c.rect)
                    };
                    if !near_ok(a.rect, b.rect, rest, work, cap)? {
                        push_violation(
                            out,
                            diagnostic_shapes,
                            Violation::Spacing {
                                net,
                                layer,
                                a: a.rect,
                                b: b.rect,
                            },
                            limits.max_rule_violations,
                        )?;
                    }
                }
            }
        }
    }
    let mut members = BTreeMap::<usize, Vec<usize>>::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        members.entry(root).or_default().push(i);
    }
    for list in members.values() {
        let route = list.iter().any(|&i| shapes[i].origin == Origin::Route);
        if !route {
            if let Some(&i) = list.iter().find(|&&i| shapes[i].origin == Origin::Patch) {
                push_violation(
                    out,
                    diagnostic_shapes,
                    Violation::Floating {
                        net,
                        layer,
                        rect: shapes[i].rect,
                    },
                    limits.max_rule_violations,
                )?;
            }
            continue;
        }
        if input.tracks.is_some()
            && list.len() >= 3
            && let Some(min) = rule.min_area
        {
            // Holes below four times the AREA rule are filled (the LEF
            // MINENCLOSEDAREA is not retained; every same-net hole of
            // this size can only be filled, never routed through).
            let rects: Vec<Rect> = list.iter().map(|&i| shapes[i].rect).collect();
            for h in holes(&rects, work, cap, limits.max_rule_grid_cells)? {
                let a = i128::from(h.ux - h.lx) * i128::from(h.uy - h.ly);
                if a < 4 * i128::from(min) {
                    push_violation(
                        out,
                        diagnostic_shapes,
                        Violation::Hole {
                            net,
                            layer,
                            rect: h,
                        },
                        limits.max_rule_violations,
                    )?;
                }
            }
        }
        if let Some(min) = rule.min_area {
            let rects: Vec<Rect> = list.iter().map(|&i| shapes[i].rect).collect();
            if let Some(area) = area_violation(&rects, i128::from(min), work, cap)? {
                push_violation(
                    out,
                    diagnostic_shapes,
                    Violation::Area {
                        net,
                        layer,
                        area,
                        shapes: rects,
                    },
                    limits.max_rule_violations,
                )?;
            }
        }
    }
    Ok(())
}

fn flip_y(r: Rect) -> Rect {
    Rect {
        lx: r.lx,
        ly: -r.uy,
        ux: r.ux,
        uy: -r.ly,
    }
}

/// Stair-step overlap of `a` and `b` whose union narrows below `width` at the
/// overlap diagonal: the two corner probes (unit squares just outside the
/// union at its concave corners) and two fills, each closing one concave
/// corner so the remaining corners are at least `width` apart. None when the
/// pair is no stair step or the diagonal is wide enough.
pub(crate) fn neck(a: Rect, b: Rect, width: i64) -> Option<([Rect; 2], [Rect; 2])> {
    let (l, r) = if a.lx < b.lx { (a, b) } else { (b, a) };
    if !(l.lx < r.lx && r.lx <= l.ux && l.ux < r.ux) {
        return None;
    }
    let lower = r.ly < l.ly;
    let (l, r) = if lower {
        (flip_y(l), flip_y(r))
    } else {
        (l, r)
    };
    if !(l.ly < r.ly && r.ly <= l.uy && l.uy < r.uy) {
        return None;
    }
    let (dx, dy) = (i128::from(l.ux - r.lx), i128::from(l.uy - r.ly));
    if dx * dx + dy * dy >= i128::from(width) * i128::from(width) {
        return None;
    }
    let probes = [
        Rect {
            lx: r.lx - 1,
            ly: l.uy,
            ux: r.lx,
            uy: l.uy + 1,
        },
        Rect {
            lx: l.ux,
            ly: r.ly - 1,
            ux: l.ux + 1,
            uy: r.ly,
        },
    ];
    let lower_right = Rect {
        lx: l.ux,
        ly: l.ly.max(r.ly.min(l.uy - width)),
        ux: r.ux.min(l.ux.max(r.lx + width)),
        uy: r.ly,
    };
    let upper_left = Rect {
        lx: l.lx.max(r.lx.min(l.ux - width)),
        ly: l.uy,
        ux: r.lx,
        uy: r.uy.min(l.uy.max(r.ly + width)),
    };
    let fills = [lower_right, upper_left];
    Some(if lower {
        (probes.map(flip_y), fills.map(flip_y))
    } else {
        (probes, fills)
    })
}

fn clip(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect {
        lx: a.lx.max(b.lx),
        ly: a.ly.max(b.ly),
        ux: a.ux.min(b.ux),
        uy: a.uy.min(b.uy),
    };
    (r.lx < r.ux && r.ly < r.uy).then_some(r)
}

/// The part of one connected same-net component wider than `width`: the
/// union of all squares of side `width` inside it whose lower-left corners
/// cover a positive area (a line exactly `width` wide is not wide). Exact;
/// evaluated per source rectangle on a compressed grid of its `width`
/// neighbourhood, which holds every square touching it. Output may overlap.
pub(crate) fn wide_parts(
    rects: &[Rect],
    width: i64,
    work: &mut usize,
    cap: usize,
) -> Result<Vec<Rect>> {
    let mut out = Vec::new();
    let Some(first) = rects.first() else {
        return Ok(out);
    };
    let bbox = rects.iter().fold(*first, |b, r| Rect {
        lx: b.lx.min(r.lx),
        ly: b.ly.min(r.ly),
        ux: b.ux.max(r.ux),
        uy: b.uy.max(r.uy),
    });
    if bbox.ux - bbox.lx <= width || bbox.uy - bbox.ly <= width {
        return Ok(out);
    }
    let candidates = |cs: &[i64]| -> Vec<i64> {
        let (lo, hi) = (cs[0], cs[cs.len() - 1] - width);
        let mut v: Vec<i64> = cs
            .iter()
            .flat_map(|&c| [c, c - width])
            .filter(|&c| c >= lo && c <= hi)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    for r in rects {
        let window = Rect {
            lx: r.lx - width,
            ly: r.ly - width,
            ux: r.ux + width,
            uy: r.uy + width,
        };
        charge(work, rects.len(), cap)?;
        let local: Vec<Rect> = rects.iter().filter_map(|&c| clip(c, window)).collect();
        let mut xs: Vec<i64> = local.iter().flat_map(|c| [c.lx, c.ux]).collect();
        let mut ys: Vec<i64> = local.iter().flat_map(|c| [c.ly, c.uy]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let (px, py) = (candidates(&xs), candidates(&ys));
        if px.len() < 2 || py.len() < 2 {
            continue;
        }
        let (nx, ny) = (xs.len() - 1, ys.len() - 1);
        charge(
            work,
            (nx + 1) * (ny + 1) + local.len() + px.len() * py.len(),
            cap,
        )?;
        // Prefix sums of uncovered compressed cells.
        let mut covered = vec![false; nx * ny];
        for c in &local {
            let (x0, x1) = (
                xs.partition_point(|&x| x < c.lx),
                xs.partition_point(|&x| x < c.ux),
            );
            let (y0, y1) = (
                ys.partition_point(|&y| y < c.ly),
                ys.partition_point(|&y| y < c.uy),
            );
            charge(work, (x1 - x0) * (y1 - y0), cap)?;
            for y in y0..y1 {
                for x in x0..x1 {
                    covered[y * nx + x] = true;
                }
            }
        }
        let mut free = vec![0u32; (nx + 1) * (ny + 1)];
        for y in 0..ny {
            for x in 0..nx {
                free[(y + 1) * (nx + 1) + x + 1] = free[y * (nx + 1) + x + 1]
                    + free[(y + 1) * (nx + 1) + x]
                    - free[y * (nx + 1) + x]
                    + u32::from(!covered[y * nx + x]);
            }
        }
        // Columns/rows met by the open square (p, p + width) for a corner p
        // inside the candidate cell (doubled coordinates: no breakpoint).
        let span = |cs: &[i64], a: i64, b: i64| {
            let p2 = a + b;
            (
                cs.partition_point(|&c| 2 * c < p2) - 1,
                cs.partition_point(|&c| 2 * c < p2 + 2 * width),
            )
        };
        for a in px.windows(2) {
            let (x0, x1) = span(&xs, a[0], a[1]);
            for c in py.windows(2) {
                let (y0, y1) = span(&ys, c[0], c[1]);
                let gaps = free[y1 * (nx + 1) + x1] + free[y0 * (nx + 1) + x0]
                    - free[y0 * (nx + 1) + x1]
                    - free[y1 * (nx + 1) + x0];
                if gaps == 0 {
                    let square = Rect {
                        lx: a[0],
                        ly: c[0],
                        ux: a[1] + width,
                        uy: c[1] + width,
                    };
                    if let Some(part) = clip(square, *r) {
                        out.push(part);
                    }
                }
            }
        }
    }
    out.sort_unstable_by_key(|r| (r.lx, r.ly, r.ux, r.uy));
    out.dedup();
    Ok(out)
}

/// Connected components (overlap or edge contact) of one lx-sorted group.
fn components(shapes: &[Shape], work: &mut usize, cap: usize) -> Result<Vec<Vec<usize>>> {
    let n = shapes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        let a = shapes[i].rect;
        for (j, b) in shapes.iter().enumerate().skip(i + 1) {
            if b.rect.lx > a.ux {
                break;
            }
            charge(work, 1, cap)?;
            if relation(a, b.rect, 1) == Relation::Connected {
                let (x, y) = (find(&mut parent, i), find(&mut parent, j));
                parent[x.max(y)] = x.min(y);
            }
        }
    }
    let mut members = BTreeMap::<usize, Vec<usize>>::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        members.entry(root).or_default().push(i);
    }
    Ok(members.into_values().collect())
}

/// Wide-metal spacing on layers with a `wide_spacing` row: the wide part of
/// every net component (`wide_parts`) against the wires, via pads, patches
/// and pins on that layer, including notches in the same polygon. Facing
/// shapes closer than the wide spacing are paired; exposed intervals merge per
/// (component, other net, side), and a merged run longer than the parallel
/// run is a violation. Merging runs at slightly different gaps errs
/// conservative. Fixed obstructions are not compared; the foundry deck stays
/// the DRC oracle. Candidate scans, corridor subtractions and local grids
/// all charge the comparison budget.
pub(crate) fn wide_check(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    work: &mut usize,
) -> Result<Vec<Violation>> {
    let cap = limits.max_comparisons;
    let rule = |layer: u32| {
        input
            .layers
            .get(layer as usize)
            .and_then(|l| l.wide_spacing)
    };
    if input.layers.iter().all(|l| l.wide_spacing.is_none()) {
        return Ok(Vec::new());
    }
    struct Part {
        layer: u32,
        net: u32,
        comp: usize,
        patched: bool,
        rect: Rect,
    }
    let index = crate::rule_index::RuleIndex::new(input, routes, work, cap)?;
    let mut parts = Vec::new();
    // Keep only components with wide metal. Their original rectangles tell
    // which same-net facing spans contain metal instead of a real gap.
    let mut members: Vec<(Vec<Rect>, i64)> = Vec::new();
    let mut member_shapes = 0usize;
    let mut comp = 0usize;
    for &net in index.nets() {
        let groups = index.collect(input, routes, net, limits, work)?;
        for (&(net, layer), shapes) in &groups {
            let Some(w) = rule(layer) else {
                continue;
            };
            for list in components(shapes, work, cap)? {
                let rects: Vec<Rect> = list.iter().map(|&i| shapes[i].rect).collect();
                let wide = wide_parts(&rects, w.width, work, cap)?;
                if wide.is_empty() {
                    continue;
                }
                let patched = list.iter().any(|&i| shapes[i].origin == Origin::Patch);
                for rect in wide {
                    parts.push(Part {
                        layer,
                        net,
                        comp,
                        patched,
                        rect,
                    });
                }
                let mut metal = rects;
                member_shapes = member_shapes
                    .checked_add(metal.len())
                    .filter(|&n| n <= limits.max_obstacles)
                    .ok_or_else(|| budget("wide-metal component storage"))?;
                charge(work, metal.len().saturating_mul(32), cap)?;
                metal.sort_unstable_by_key(|r| r.lx);
                let widest = metal.iter().map(|r| r.ux - r.lx).max().unwrap_or(0);
                members.push((metal, widest));
                comp += 1;
            }
        }
    }
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    charge(work, parts.len().saturating_mul(32), cap)?;
    parts.sort_unstable_by_key(|p| (p.layer, p.rect.lx, p.rect.ly, p.comp));
    let mut reach = BTreeMap::<u32, i64>::new();
    for p in &parts {
        let e = reach.entry(p.layer).or_insert(0);
        *e = (*e).max(p.rect.ux - p.rect.lx);
    }
    // (component, other net, side) -> facing intervals, first pair, bounding
    // box of the facing shapes of the other net, routed, bounding box of the
    // facing wide parts.
    type Key = (usize, u32, u8);
    let mut runs = BTreeMap::<Key, (Vec<(i64, i64)>, usize, Rect, bool, Rect)>::new();
    let mut visit = |layer: u32, net: u32, rect: Rect, routed: bool, work: &mut usize| {
        let Some(w) = rule(layer) else {
            return Ok(());
        };
        let Some(&layer_reach) = reach.get(&layer) else {
            // Only same-layer wide parts can own a spacing violation. An
            // empty layer still consumes the base unit for a spatial query.
            charge(work, 1, cap)?;
            return Ok(());
        };
        let s = w.spacing;
        let from = rect.lx - s - layer_reach;
        let lo = parts.partition_point(|p| (p.layer, p.rect.lx) < (layer, from));
        let hi = parts.partition_point(|p| (p.layer, p.rect.lx) < (layer, rect.ux + s));
        charge(work, hi.saturating_sub(lo) + 1, cap)?;
        for (k, p) in parts.iter().enumerate().take(hi).skip(lo) {
            let o = p.rect;
            let (side, gap, a, b) = if rect.ux <= o.lx {
                (0, o.lx - rect.ux, o.ly.max(rect.ly), o.uy.min(rect.uy))
            } else if rect.lx >= o.ux {
                (1, rect.lx - o.ux, o.ly.max(rect.ly), o.uy.min(rect.uy))
            } else if rect.uy <= o.ly {
                (2, o.ly - rect.uy, o.lx.max(rect.lx), o.ux.min(rect.ux))
            } else if rect.ly >= o.uy {
                (3, rect.ly - o.uy, o.lx.max(rect.lx), o.ux.min(rect.ux))
            } else {
                continue;
            };
            if gap <= 0 || gap >= s || b <= a {
                continue;
            }
            let mut facing = vec![(a, b)];
            if p.net == net {
                // A net can contain a long notch in one polygon. Discard
                // only facing spans occupied by metal, never the whole
                // component: a bridge elsewhere does not remove the gap.
                let corridor = match side {
                    0 => Rect {
                        lx: rect.ux,
                        ly: a,
                        ux: o.lx,
                        uy: b,
                    },
                    1 => Rect {
                        lx: o.ux,
                        ly: a,
                        ux: rect.lx,
                        uy: b,
                    },
                    2 => Rect {
                        lx: a,
                        ly: rect.uy,
                        ux: b,
                        uy: o.ly,
                    },
                    _ => Rect {
                        lx: a,
                        ly: o.uy,
                        ux: b,
                        uy: rect.ly,
                    },
                };
                let (metal, widest) = &members[p.comp];
                let from = metal.partition_point(|r| r.lx < corridor.lx - widest);
                let mut empty = vec![corridor];
                for m in &metal[from..] {
                    if m.lx >= corridor.ux || empty.is_empty() {
                        break;
                    }
                    charge(work, 1, cap)?;
                    if !intersects(*m, corridor) {
                        continue;
                    }
                    charge(work, empty.len(), cap)?;
                    let mut next = Vec::new();
                    for gap in empty {
                        subtract(gap, *m, &mut next);
                    }
                    if next.len() > MAX_PIECES {
                        return Err(budget("wide-metal empty corridor pieces"));
                    }
                    empty = next;
                }
                // Project only after subtracting metal in both dimensions.
                // A tongue which enters part of the gap leaves empty space
                // at the same height and cannot shorten the parallel run.
                facing = empty
                    .iter()
                    .map(|r| if side < 2 { (r.ly, r.uy) } else { (r.lx, r.ux) })
                    .collect();
                if facing.is_empty() {
                    continue;
                }
            }
            let e = runs
                .entry((p.comp, net, side))
                .or_insert_with(|| (Vec::new(), k, rect, false, o));
            e.2 = Rect {
                lx: e.2.lx.min(rect.lx),
                ly: e.2.ly.min(rect.ly),
                ux: e.2.ux.max(rect.ux),
                uy: e.2.uy.max(rect.uy),
            };
            e.4 = Rect {
                lx: e.4.lx.min(o.lx),
                ly: e.4.ly.min(o.ly),
                ux: e.4.ux.max(o.ux),
                uy: e.4.uy.max(o.uy),
            };
            if e.0.len() >= MAX_PIECES * 64 {
                charge(work, e.0.len().saturating_mul(8), cap)?;
                e.0.sort_unstable();
                let mut merged: Vec<(i64, i64)> = Vec::with_capacity(e.0.len());
                for &(a, b) in &e.0 {
                    match merged.last_mut() {
                        Some(last) if a <= last.1 => last.1 = last.1.max(b),
                        _ => merged.push((a, b)),
                    }
                }
                if merged.len() >= MAX_PIECES * 64 {
                    return Err(budget("wide-metal facing intervals"));
                }
                e.0 = merged;
            }
            e.0.extend(facing);
            e.3 |= routed;
        }
        Ok::<(), Error>(())
    };
    let routing = |l: u32| {
        input
            .layers
            .get(l as usize)
            .is_some_and(|l| l.vertical.is_some())
    };
    for wire in &routes.wires {
        if rule(wire.layer).is_some() {
            let rect = wire_rect(wire, input.layers[wire.layer as usize].width)?;
            visit(wire.layer, wire.net, rect, true, work)?;
        }
    }
    for via in &routes.vias {
        let kind = input
            .vias
            .get(via.kind as usize)
            .ok_or_else(|| Error::Invalid("wide-metal check via kind".into()))?;
        for s in &kind.shapes {
            if routing(s.layer) && rule(s.layer).is_some() {
                visit(s.layer, via.net, shifted(s.rect, via.at)?, true, work)?;
            }
        }
    }
    for patch in &routes.patches {
        visit(patch.layer, patch.net, patch.rect, true, work)?;
    }
    for t in &input.terminals {
        for s in &t.shapes {
            visit(s.layer, t.net, s.rect, false, work)?;
        }
    }
    let mut out = Vec::new();
    let mut diagnostic_shapes = 0;
    for ((_, other, _), (mut list, k, near, routed, wide)) in runs {
        let p = &parts[k];
        let Some(w) = rule(p.layer) else {
            continue;
        };
        charge(work, list.len().saturating_mul(8), cap)?;
        list.sort_unstable();
        let (mut run, mut cur) = (0i64, list[0]);
        for &(a, b) in &list[1..] {
            if a <= cur.1 {
                cur.1 = cur.1.max(b);
            } else {
                run = run.max(cur.1 - cur.0);
                cur = (a, b);
            }
        }
        run = run.max(cur.1 - cur.0);
        if run <= w.parallel_run {
            continue;
        }
        let net = if p.patched || !routed { p.net } else { other };
        push_violation(
            &mut out,
            &mut diagnostic_shapes,
            Violation::Wide {
                net,
                owner: p.net,
                other,
                layer: p.layer,
                wide,
                near,
                run,
            },
            limits.max_rule_violations,
        )?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    #[test]
    fn area_certificate_preserves_exact_failure_values_and_bounds_large_passes() {
        let small = [r(0, 0, 3, 3), r(2, 2, 5, 5)];
        let exact = union_area(&small, &mut 0, 1000).unwrap();
        for minimum in [1, 9, 10, exact, exact + 1, 100] {
            assert_eq!(
                area_violation(&small, minimum, &mut 0, 1000).unwrap(),
                (exact < minimum).then_some(exact)
            );
        }
        let large: Vec<_> = (0..5000).map(|i| r(i, i, i + 100, i + 100)).collect();
        assert_eq!(area_violation(&large, 9999, &mut 0, 5000).unwrap(), None);
        assert!(area_violation(&large, 9999, &mut 0, 4999).is_err());
    }
    #[test]
    fn relations_cover_overlap_edge_corner_and_euclidean_gaps() {
        assert_eq!(
            relation(r(0, 0, 10, 10), r(5, 5, 15, 15), 4),
            Relation::Connected
        );
        assert_eq!(
            relation(r(0, 0, 10, 10), r(10, 2, 20, 8), 4),
            Relation::Connected
        );
        assert_eq!(
            relation(r(0, 0, 10, 10), r(10, 10, 20, 20), 4),
            Relation::Near
        );
        assert_eq!(
            relation(r(0, 0, 10, 10), r(13, 0, 20, 10), 4),
            Relation::Near
        );
        assert_eq!(
            relation(r(0, 0, 10, 10), r(14, 0, 20, 10), 4),
            Relation::Far
        );
        // Corner gap (3,3) is 4.24 > 4 Euclidean: legal even though both axes < 4.
        assert_eq!(
            relation(r(0, 0, 10, 10), r(13, 13, 20, 20), 4),
            Relation::Far
        );
        assert_eq!(
            relation(r(0, 0, 10, 10), r(12, 12, 20, 20), 4),
            Relation::Near
        );
    }
    #[test]
    fn coverage_subtracts_and_area_unions_exactly() {
        let mut w = 0;
        assert!(
            covered(
                r(0, 0, 10, 2),
                [r(0, 0, 6, 2), r(5, 0, 10, 3)].into_iter(),
                &mut w,
                1000
            )
            .unwrap()
        );
        assert!(!covered(r(0, 0, 10, 2), [r(0, 0, 6, 2)].into_iter(), &mut w, 1000).unwrap());
        assert_eq!(
            union_area(&[r(0, 0, 10, 10), r(5, 5, 15, 15)], &mut w, 1000).unwrap(),
            175
        );
        assert!(union_area(&[r(0, 0, 10, 10), r(5, 5, 15, 15)], &mut 0, 1).is_err());
    }
    #[test]
    fn fills_bridge_gap_with_at_least_layer_width() {
        // Narrow projection is widened inside the hull.
        let f = fill_for(r(0, 0, 100, 20), r(95, 30, 200, 50), 20, 5);
        assert_eq!((f.ly, f.uy), (20, 30));
        assert!(f.ux - f.lx >= 20 && f.lx >= 0 && f.ux <= 200);
        // Corner case: square bridge overlapping both rectangles.
        let f = fill_for(r(0, 0, 50, 50), r(55, 55, 100, 100), 20, 5);
        assert!(f.lx < 50 && f.ux > 55 && f.ly < 50 && f.uy > 55);
        assert_eq!(relation(f, r(0, 0, 50, 50), 20), Relation::Connected);
        assert_eq!(relation(f, r(55, 55, 100, 100), 20), Relation::Connected);
    }
    #[test]
    fn contact_line_gap_is_covered_from_either_side() {
        // a sits on a third shape c; b lies inside c, its top on c's edge.
        let a = r(0, 200, 200, 375);
        let b = r(330, 0, 620, 200);
        let c = r(0, 0, 620, 200);
        let mut w = 0;
        assert_eq!(relation(a, b, 210), Relation::Near);
        assert!(local_ok(a, &[b, c], 210, &mut w, 10_000).unwrap());
        // Without c the corner spacing of 130 remains. A strip under the gap
        // alone covers the a-b pair but meets a at a point, itself near.
        assert!(!local_ok(a, &[b], 210, &mut w, 10_000).unwrap());
        let strip = r(200, 0, 330, 200);
        assert_eq!(relation(a, strip, 210), Relation::Near);
        assert!(!local_ok(a, &[b, strip], 210, &mut w, 10_000).unwrap());
        // Point contact: either free corner square joins the pair.
        let p = r(0, 0, 100, 100);
        let q = r(100, 100, 200, 200);
        assert!(!local_ok(p, &[q], 210, &mut w, 10_000).unwrap());
        assert!(local_ok(p, &[q, r(0, 100, 100, 200)], 210, &mut w, 10_000).unwrap());
        assert!(local_ok(p, &[q, r(100, 0, 200, 100)], 210, &mut w, 10_000).unwrap());
    }
    #[test]
    fn diagonal_pair_counts_only_real_corners() {
        // Escape pad `b` under a wire `w` that turns down along `v`: the pocket
        // between b and w is a notch until `f` fills it. Pad `a` on the wire
        // has no free corner toward b, so the a-b pair adds nothing.
        let a = r(65, 500, 355, 700);
        let b = r(425, 35, 625, 325);
        let w = r(110, 500, 730, 700);
        let v = r(530, 80, 730, 700);
        let f = r(425, 325, 530, 500);
        let mut work = 0;
        assert!(!local_ok(a, &[b], 210, &mut work, 10_000).unwrap());
        assert!(!local_ok(b, &[a, w, v], 210, &mut work, 10_000).unwrap());
        assert!(local_ok(b, &[a, w, v, f], 210, &mut work, 10_000).unwrap());
    }
    #[test]
    fn stair_step_neck_is_found_and_each_fill_clears_it() {
        let cap = 1 << 30;
        // Route end over an off-track boundary pin (130 x 140 overlap).
        let wire = r(699_830, 849_560, 720_190, 849_760);
        let pin = r(720_060, 849_620, 720_440, 850_000);
        let (_, fills) = neck(wire, pin, 200).unwrap();
        assert_eq!(fills[0], r(720_190, 849_560, 720_260, 849_620));
        assert_eq!(fills[1], r(719_990, 849_760, 720_060, 849_820));
        // Diagonal via pads (105 x 170), mirrored order and orientation.
        let low = r(538_410, 283_700, 538_610, 283_990);
        let high = r(538_505, 283_820, 538_795, 284_020);
        assert!(neck(high, low, 200).is_some());
        let flip = |q: Rect| r(q.lx, -q.uy, q.ux, -q.ly);
        let (probes, fills) = neck(flip(low), flip(high), 200).unwrap();
        for f in fills {
            // Each fill closes one concave corner and leaves no stair step
            // narrower than the width against either source rectangle.
            assert!(probes.iter().any(|p| clip(*p, f).is_some()));
            let a = union_area(&[flip(low), flip(high), f], &mut 0, cap).unwrap();
            assert!(a > union_area(&[flip(low), flip(high)], &mut 0, cap).unwrap());
            assert!(neck(f, flip(low), 200).is_none() && neck(f, flip(high), 200).is_none());
        }
        // Wide enough diagonals, contained or aligned pairs are no necks.
        assert!(neck(r(0, 0, 200, 1000), r(100, 500, 300, 1500), 200).is_none());
        assert!(neck(r(0, 0, 200, 1000), r(-45, 400, 245, 600), 200).is_none());
        assert!(neck(r(0, 0, 200, 1000), r(0, 900, 2000, 1100), 200).is_none());
        // Abutting stair step: the neck is the 100-long contact.
        assert!(neck(r(0, 0, 200, 1000), r(200, 900, 400, 2000), 200).is_some());
    }

    #[test]
    fn wide_parts_keep_only_metal_wider_than_the_rule() {
        let mut w = 0;
        let cap = 1 << 30;
        let area = |parts: &[Rect]| union_area(parts, &mut 0, cap).unwrap();
        // Two 200-wide tracks 220 apart joined by a fill: 620 wide over 1000.
        let joined = [
            r(0, 0, 200, 3000),
            r(420, 1000, 620, 2000),
            r(200, 1000, 420, 2000),
        ];
        let parts = wide_parts(&joined, 390, &mut w, cap).unwrap();
        assert_eq!(area(&parts), 620 * 1000);
        assert!(parts.iter().all(|p| p.ly >= 1000 && p.uy <= 2000));
        // A line exactly 390 wide is not wide; 395 is, all of it.
        assert!(
            wide_parts(&[r(0, 0, 390, 5000)], 390, &mut w, cap)
                .unwrap()
                .is_empty()
        );
        let line = wide_parts(&[r(0, 0, 395, 5000)], 390, &mut w, cap).unwrap();
        assert_eq!(area(&line), 395 * 5000);
        // Default-width L and T junctions with an off-axis via pad stay narrow.
        let junction = [
            r(0, 0, 200, 3000),
            r(0, 1400, 3000, 1600),
            r(-45, 1400, 245, 1600),
        ];
        assert!(wide_parts(&junction, 390, &mut w, cap).unwrap().is_empty());
        // A pad 175 from its own track, filled: 575 wide only where filled.
        let notch = [
            r(0, 0, 200, 4000),
            r(420, 0, 620, 4000),
            r(-45, 2000, 245, 2200),
            r(200, 2000, 420, 2200),
        ];
        let parts = wide_parts(&notch, 390, &mut w, cap).unwrap();
        assert!(
            parts.is_empty(),
            "200-high bridge is not 390 high: {parts:?}"
        );
    }

    #[test]
    fn captured_metal2_stair_notch_gets_a_neck_free_bridge() {
        // Metal2 access failure (net 12085): an upper pad and a lower escape
        // face each other over 80 DBU across a 175 DBU gap (spacing 210,
        // width 200). The robust box and the legacy bars touched other nets.
        let a = r(496_550, 63_275, 496_750, 63_565);
        let b = r(496_670, 62_480, 496_870, 63_100);
        let (cap, mut w) = (1 << 30, 0);
        assert_eq!(relation(a, b, 210), Relation::Near);
        assert!(!local_ok(a, &[b], 210, &mut w, cap).unwrap());
        let candidates = bridges(a, b, 200, 5, true);
        assert_eq!(candidates[0].len(), 1, "{candidates:?}");
        let bar = candidates[0][0];
        // One bar over both shapes' extents, a width thick across the gap,
        // snapped outward to the 5 DBU grid.
        assert_eq!(bar, r(496_550, 63_085, 496_870, 63_290));
        // Order-stable: swapped arguments give the same first candidate.
        assert_eq!(bridges(b, a, 200, 5, true)[0], vec![bar]);
        // The gap is covered and the bar meets both shapes: no near pair left.
        for (x, rest) in [(a, [b, bar]), (b, [a, bar]), (bar, [a, b])] {
            assert!(local_ok(x, &rest, 210, &mut w, cap).unwrap(), "{x:?}");
        }
        assert_eq!(relation(bar, a, 210), Relation::Connected);
        assert_eq!(relation(bar, b, 210), Relation::Connected);
        // No stair-step neck, and the Z's inner corners are a width apart.
        for (x, y) in [(a, bar), (b, bar), (a, b)] {
            assert!(neck(x, y, 200).is_none());
        }
        let (dx, dy) = (i128::from(a.ux - b.lx), i128::from(bar.uy - bar.ly));
        assert!(dx * dx + dy * dy >= 200 * 200);
        // It stays inside both shapes' extents (no metal past either end).
        assert!(bar.lx >= a.lx.min(b.lx) && bar.ux <= a.ux.max(b.ux));
        assert!(bar.ly >= b.ly && bar.uy <= a.uy);
        // Side-by-side shapes get the transposed bar.
        let t = |q: Rect| r(q.ly, q.lx, q.uy, q.ux);
        assert_eq!(bridges(t(a), t(b), 200, 5, true)[0], vec![t(bar)]);
        // Without `notch_bridge` the candidates are the legacy full-length
        // bars only (each shape's extent across the gap, over the pair's hull).
        let legacy = bridges(a, b, 200, 5, false);
        assert_eq!(
            legacy,
            vec![
                vec![r(496_550, 62_480, 496_750, 63_565)],
                vec![r(496_670, 62_480, 496_870, 63_565)],
            ]
        );
        // The flag only prepends candidates: the stair bridge and two clipped bars.
        assert_eq!(candidates.len(), 5);
        assert_eq!(candidates[3..], legacy[..]);
    }
    #[test]
    fn notch_against_own_pin_leg_is_detected_and_covered_gap_is_not() {
        // Pad protrudes 5 DBU past a 375-wide leg; bar above at 222 < 230.
        let leg = r(705, 1035, 1080, 1740);
        let bar = r(705, 1740, 1590, 2150);
        let pad = r(705, 1258, 1085, 1518);
        let mut w = 0;
        assert!(!local_ok(pad, &[leg, bar], 230, &mut w, 10_000).unwrap());
        let inside = r(760, 1200, 1020, 1580);
        assert!(local_ok(inside, &[leg, bar], 230, &mut w, 10_000).unwrap());
    }
}

#[cfg(test)]
mod scale_tests {
    use super::*;
    fn r(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    #[test]
    fn enclosed_holes_are_found_and_open_regions_are_not() {
        let mut w = 0;
        // A ring of four rectangles around a 20 x 20 hole.
        let ring = [
            r(0, 0, 60, 20),
            r(0, 40, 60, 60),
            r(0, 20, 20, 40),
            r(40, 20, 60, 40),
        ];
        assert_eq!(
            holes(&ring, &mut w, 10_000, 1_000).unwrap(),
            vec![r(20, 20, 40, 40)]
        );
        // A U shape has no hole.
        assert!(holes(&ring[..3], &mut w, 10_000, 1_000).unwrap().is_empty());
        assert!(holes(&ring, &mut 0, 1, 1_000).is_err());
        assert!(
            holes(&ring, &mut 0, 10_000, 1)
                .unwrap_err()
                .to_string()
                .contains("compressed hole grid")
        );
    }
    #[test]
    fn large_sparse_compressed_grid_uses_a_bounded_sweep() {
        let rectangles: Vec<_> = (0..5000i64)
            .map(|i| r(3 * i, 5 * i, 3 * i + 1, 5 * i + 1))
            .collect();
        assert!(
            holes(&rectangles, &mut 0, 10_000_000, 1_000_000)
                .unwrap()
                .is_empty()
        );
        assert!(holes(&rectangles, &mut 0, 10_000_000, 100).is_err());
    }

    #[test]
    fn robust_corner_bridge_overlaps_both_shapes_by_a_width() {
        // Pin corner (2000, 259260) and via pad corner (2420, 259560).
        let pin = r(0, 257260, 2000, 259260);
        let pad = r(2420, 259560, 3180, 260120);
        let f = fill_for_robust(pin, pad, 560, 10);
        let ov = |a: Rect, b: Rect| {
            (
                a.ux.min(b.ux) - a.lx.max(b.lx),
                a.uy.min(b.uy) - a.ly.max(b.ly),
            )
        };
        // The fill holds both facing corners and overlaps each shape by at
        // least the layer width or that shape's extent near the other.
        let inside = |p: [i64; 2]| f.lx <= p[0] && p[0] <= f.ux && f.ly <= p[1] && p[1] <= f.uy;
        assert!(inside([2000, 259260]) && inside([2420, 259560]), "{f:?}");
        let (x, y) = ov(f, pad);
        assert!(x > 0 && y > 0, "{f:?}");
        assert!(f.ux - f.lx >= 560 && f.uy - f.ly >= 560, "{f:?}");
        // The legacy corner bridge overlaps by less than a width (a neck).
        let g = fill_for(pin, pad, 560, 10);
        let (x, _) = ov(g, pin);
        assert!(x < 560);
        // Facing spans of at least a width keep the legacy gap fill.
        let a = r(0, 0, 100, 20);
        let b = r(10, 30, 90, 50);
        assert_eq!(fill_for_robust(a, b, 20, 5), fill_for(a, b, 20, 5));
        // Two pads facing over less than a width: the fill covers both
        // facing corners (no step narrower than the width).
        let p1 = r(31350, 197410, 31670, 197670);
        let p2 = r(31810, 197240, 32130, 197500);
        let f = fill_for_robust(p1, p2, 140, 5);
        assert!(f.ly <= 197410 && f.uy >= 197500 && f.lx <= 31670 - 140 && f.ux >= 31810 + 140);
        // A short facing span of a pad at a long wire stays local.
        let wire = r(0, 0, 10_000, 140);
        let pad = r(9_950, 200, 10_270, 460);
        let f = fill_for_robust(wire, pad, 140, 5);
        assert!(
            f.lx >= 9_950 - 2 * 140 - 5 && f.ux <= 10_270 && f.ly == 0,
            "{f:?}"
        );
        assert!(
            f.lx <= 9_950 && f.ux >= 10_000 && f.uy >= 200 + 140,
            "{f:?}"
        );
    }
}
