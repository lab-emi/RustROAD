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
use crate::routing::{RouteInput, RouteLimits, Routes, shifted, wire_rect};
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
}
impl Violation {
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
/// corner box; zero-size dimensions are widened by one DBU on each side.
fn gap_region(a: Rect, b: Rect) -> Rect {
    let (ox, oy) = (overlap_x(a, b), overlap_y(a, b));
    let (mut lx, mut ux) = if ox > 0 {
        (a.lx.max(b.lx), a.ux.min(b.ux))
    } else {
        (a.ux.min(b.ux), a.lx.max(b.lx))
    };
    let (mut ly, mut uy) = if oy > 0 {
        (a.ly.max(b.ly), a.uy.min(b.uy))
    } else {
        (a.uy.min(b.uy), a.ly.max(b.ly))
    };
    if lx == ux {
        lx -= 1;
        ux += 1;
    }
    if ly == uy {
        ly -= 1;
        uy += 1;
    }
    Rect { lx, ly, ux, uy }
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
        let region = gap_region(rect, *o);
        let rest = others
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, r)| *r);
        if !covered(region, rest, work, cap)? {
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
pub(crate) fn collect(
    input: &RouteInput,
    routes: &Routes,
) -> Result<BTreeMap<(u32, u32), Vec<Shape>>> {
    let routing = |l: u32| {
        input
            .layers
            .get(l as usize)
            .is_some_and(|l| l.vertical.is_some())
    };
    let selected: std::collections::BTreeSet<u32> = routes.nets.iter().copied().collect();
    let mut runs = BTreeMap::<(u32, u32), (Vec<Rect>, Vec<Rect>)>::new();
    for w in &routes.wires {
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
    for v in &routes.vias {
        for s in &input.vias[v.kind as usize].shapes {
            if routing(s.layer) {
                groups.entry((v.net, s.layer)).or_default().push(Shape {
                    rect: shifted(s.rect, v.at)?,
                    origin: Origin::Route,
                });
            }
        }
    }
    for p in &routes.patches {
        groups.entry((p.net, p.layer)).or_default().push(Shape {
            rect: p.rect,
            origin: Origin::Patch,
        });
    }
    for t in &input.terminals {
        if !selected.contains(&t.net) {
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

/// Every same-net rule violation of the stored geometry, deterministic order.
pub(crate) fn check(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    work: &mut usize,
) -> Result<Vec<Violation>> {
    let cap = limits.max_comparisons;
    let groups = collect(input, routes)?;
    let mut out = Vec::new();
    for (&(net, layer), shapes) in &groups {
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
                    }
                    Relation::Far => {}
                    Relation::Near => {
                        if a.origin == Origin::Pin && b.origin == Origin::Pin {
                            continue;
                        }
                        let region = gap_region(a.rect, b.rect);
                        let rest = shapes
                            .iter()
                            .enumerate()
                            .filter(|(k, c)| {
                                *k != i
                                    && *k != j
                                    && c.rect.lx < region.ux
                                    && c.rect.ux > region.lx
                                    && c.rect.ly < region.uy
                                    && c.rect.uy > region.ly
                            })
                            .map(|(_, c)| c.rect);
                        if !covered(region, rest, work, cap)? {
                            out.push(Violation::Spacing {
                                net,
                                layer,
                                a: a.rect,
                                b: b.rect,
                            });
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
                    out.push(Violation::Floating {
                        net,
                        layer,
                        rect: shapes[i].rect,
                    });
                }
                continue;
            }
            if let Some(min) = rule.min_area {
                let rects: Vec<Rect> = list.iter().map(|&i| shapes[i].rect).collect();
                let area = union_area(&rects, work, cap)?;
                if area < i128::from(min) {
                    out.push(Violation::Area {
                        net,
                        layer,
                        area,
                        shapes: rects,
                    });
                }
            }
        }
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
