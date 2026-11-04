//! Same-net shape merging for GDSII streamout.
//!
//! `export-gds` writes routed metal with DEF semantics: one rectangle per
//! maximal collinear run (split at via landings, pin-access points and
//! branches), plus same-net patches. Runs of one net on one layer overlap at
//! every split and corner, so the stream carries redundant edges and covered
//! shapes that every downstream geometry engine merges again. This module
//! replaces the rectangles of one net on one layer by an exact cover of their
//! union: maximal non-overlapping rectangles, or one simple polygon per
//! connected piece. The covered point set (the metal) is unchanged; only its
//! representation is.
//!
//! Method: rectangles are grouped into connected pieces (positive-area overlap
//! or a shared edge of positive length; corner contact does not connect, so
//! no polygon ever touches itself). Each piece is rasterized on its own
//! compressed coordinate grid (the distinct rectangle edges), the classic
//! coordinate-compression treatment of rectangle unions (Bentley's
//! rectangle-union problem; Preparata & Shamos, *Computational Geometry*,
//! 1985, sec. 8.4). The cover is then decomposed into maximal slabs, rows of
//! covered cells merged with identical neighbours, in both orientations; the
//! decomposition with fewer rectangles is kept (horizontal on ties). A polygon
//! is the boundary of the covered cells traced as directed edges (interior on
//! the left, i.e. counter-clockwise); a piece whose cover has holes yields
//! more than one loop and keeps its rectangle decomposition, because a GDS
//! BOUNDARY cannot carry a hole without cut lines.
//!
//! Work is bounded: grid cells per piece are capped, and a piece over the cap
//! keeps its input rectangles unchanged (still exact). Output order is
//! canonical (sorted), so the stream is independent of input order.

use crate::{Error, Result};
use std::collections::BTreeMap;

/// Grid cells one piece may rasterize. A piece of n rectangles has at most
/// (2n - 1)^2 cells; Ibex nets stay far below (largest pieces ~10^3 cells).
pub const MAX_PIECE_CELLS: usize = 1 << 22;

/// How a merged piece is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStyle {
    /// Input rectangles as given (no merging).
    Off,
    /// Maximal non-overlapping rectangles covering the union.
    Rectangles,
    /// One simple polygon per hole-free piece; rectangles otherwise.
    Polygons,
}

impl MergeStyle {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "off" => Ok(Self::Off),
            "rectangles" => Ok(Self::Rectangles),
            "polygons" => Ok(Self::Polygons),
            other => Err(Error::Gds {
                code: "GDS_MERGE_STYLE",
                message: format!("unknown shape merge style {other:?} (off, rectangles, polygons)"),
            }),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Rectangles => "rectangles",
            Self::Polygons => "polygons",
        }
    }
}

/// One written shape: a rectangle `[lx, ly, ux, uy]` or a simple rectilinear
/// polygon (counter-clockwise, not closed, no collinear vertices).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Piece {
    Rect([i64; 4]),
    Polygon(Vec<[i64; 2]>),
}

/// Merge statistics of one call (summed by the exporter).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub input: usize,
    pub rectangles: usize,
    pub polygons: usize,
    /// Pieces whose cover has holes (written as rectangles).
    pub holed_pieces: usize,
    /// Pieces over the grid cap (written as their input rectangles).
    pub capped_pieces: usize,
}

fn valid(r: &[i64; 4]) -> bool {
    r[0] < r[2] && r[1] < r[3]
}

/// Positive-area overlap or a shared edge of positive length.
fn connected(a: &[i64; 4], b: &[i64; 4]) -> bool {
    let w = a[2].min(b[2]) - a[0].max(b[0]);
    let h = a[3].min(b[3]) - a[1].max(b[1]);
    w >= 0 && h >= 0 && (w > 0 || h > 0)
}

struct Sets(Vec<usize>);
impl Sets {
    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            let (lo, hi) = (a.min(b), a.max(b));
            self.0[hi] = lo;
        }
    }
}

/// Connected pieces of `rects` (sorted, deduplicated), each sorted.
fn pieces(rects: &[[i64; 4]]) -> Vec<Vec<[i64; 4]>> {
    let mut sets = Sets((0..rects.len()).collect());
    // Sweep in lx order: only rectangles whose lx <= current ux can touch.
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            if rects[j][0] > rects[i][2] {
                break;
            }
            if connected(&rects[i], &rects[j]) {
                sets.join(i, j);
            }
        }
    }
    let mut groups: BTreeMap<usize, Vec<[i64; 4]>> = BTreeMap::new();
    for (i, r) in rects.iter().enumerate() {
        let root = sets.find(i);
        groups.entry(root).or_default().push(*r);
    }
    groups.into_values().collect()
}

/// Compressed coverage grid of one piece.
struct Grid {
    xs: Vec<i64>,
    ys: Vec<i64>,
    /// Row-major (y, x) coverage of cell [xs[i], xs[i+1]] x [ys[j], ys[j+1]].
    covered: Vec<bool>,
}
impl Grid {
    fn new(rects: &[[i64; 4]]) -> Option<Self> {
        let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r[0], r[2]]).collect();
        let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r[1], r[3]]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let (nx, ny) = (xs.len() - 1, ys.len() - 1);
        if nx.checked_mul(ny).is_none_or(|c| c > MAX_PIECE_CELLS) {
            return None;
        }
        // 2-D difference array over (ny + 1) x (nx + 1), then prefix sums.
        let mut diff = vec![0i32; (nx + 1) * (ny + 1)];
        for r in rects {
            let x0 = xs.binary_search(&r[0]).expect("edge");
            let x1 = xs.binary_search(&r[2]).expect("edge");
            let y0 = ys.binary_search(&r[1]).expect("edge");
            let y1 = ys.binary_search(&r[3]).expect("edge");
            diff[y0 * (nx + 1) + x0] += 1;
            diff[y0 * (nx + 1) + x1] -= 1;
            diff[y1 * (nx + 1) + x0] -= 1;
            diff[y1 * (nx + 1) + x1] += 1;
        }
        for y in 0..=ny {
            for x in 1..=nx {
                diff[y * (nx + 1) + x] += diff[y * (nx + 1) + x - 1];
            }
        }
        for y in 1..=ny {
            for x in 0..=nx {
                diff[y * (nx + 1) + x] += diff[(y - 1) * (nx + 1) + x];
            }
        }
        let mut covered = vec![false; nx * ny];
        for y in 0..ny {
            for x in 0..nx {
                covered[y * nx + x] = diff[y * (nx + 1) + x] > 0;
            }
        }
        Some(Self { xs, ys, covered })
    }
    fn nx(&self) -> usize {
        self.xs.len() - 1
    }
    fn ny(&self) -> usize {
        self.ys.len() - 1
    }
    fn at(&self, x: usize, y: usize) -> bool {
        self.covered[y * self.nx() + x]
    }

    /// Maximal slabs: covered runs along the major axis, merged across the
    /// minor axis while the run is identical. Non-overlapping, exact cover.
    fn slabs(&self, horizontal: bool) -> Vec<[i64; 4]> {
        let (major, minor) = if horizontal {
            (self.nx(), self.ny())
        } else {
            (self.ny(), self.nx())
        };
        let cell = |a: usize, b: usize| {
            if horizontal {
                self.at(a, b)
            } else {
                self.at(b, a)
            }
        };
        // Open slabs: (start, end) run on the major axis -> first minor index.
        let mut open: BTreeMap<(usize, usize), usize> = BTreeMap::new();
        let mut out = Vec::new();
        let close = |run: (usize, usize), from: usize, to: usize, out: &mut Vec<[i64; 4]>| {
            out.push(if horizontal {
                [self.xs[run.0], self.ys[from], self.xs[run.1], self.ys[to]]
            } else {
                [self.xs[from], self.ys[run.0], self.xs[to], self.ys[run.1]]
            });
        };
        for b in 0..=minor {
            let mut runs = Vec::new();
            if b < minor {
                let mut a = 0;
                while a < major {
                    if cell(a, b) {
                        let start = a;
                        while a < major && cell(a, b) {
                            a += 1;
                        }
                        runs.push((start, a));
                    } else {
                        a += 1;
                    }
                }
            }
            let keys: Vec<(usize, usize)> = open.keys().copied().collect();
            for run in keys {
                if runs.binary_search(&run).is_err() {
                    let from = open.remove(&run).expect("open");
                    close(run, from, b, &mut out);
                }
            }
            for run in runs {
                open.entry(run).or_insert(b);
            }
        }
        out
    }

    /// Boundary loops of the covered cells, interior on the left. At a vertex
    /// where two covered cells meet diagonally, the walk turns so that it keeps
    /// the cell it just bordered (left turn), which separates the loops there.
    fn loops(&self) -> Vec<Vec<[i64; 2]>> {
        let (nx, ny) = (self.nx(), self.ny());
        let inside = |x: isize, y: isize| {
            x >= 0
                && y >= 0
                && (x as usize) < nx
                && (y as usize) < ny
                && self.at(x as usize, y as usize)
        };
        // Directed boundary edges between grid vertices (vx, vy), keyed by start.
        let mut next: BTreeMap<(usize, usize), Vec<(usize, usize)>> = BTreeMap::new();
        for y in 0..ny {
            for x in 0..nx {
                if !self.at(x, y) {
                    continue;
                }
                let (xi, yi) = (x as isize, y as isize);
                if !inside(xi, yi - 1) {
                    next.entry((x, y)).or_default().push((x + 1, y)); // bottom, eastward
                }
                if !inside(xi + 1, yi) {
                    next.entry((x + 1, y)).or_default().push((x + 1, y + 1)); // right, northward
                }
                if !inside(xi, yi + 1) {
                    next.entry((x + 1, y + 1)).or_default().push((x, y + 1)); // top, westward
                }
                if !inside(xi - 1, yi) {
                    next.entry((x, y + 1)).or_default().push((x, y)); // left, southward
                }
            }
        }
        let mut loops = Vec::new();
        while let Some((&start, _)) = next.iter().next() {
            let mut path = vec![start];
            let mut prev = start;
            let mut at = {
                let outs = next.get_mut(&start).expect("start");
                let to = outs.remove(0);
                if outs.is_empty() {
                    next.remove(&start);
                }
                to
            };
            while at != start {
                path.push(at);
                let outs = next.get_mut(&at).expect("closed boundary");
                let pick = if outs.len() == 1 {
                    0
                } else {
                    // Two exits at a diagonal pinch: take the left turn.
                    let d = (
                        at.0 as isize - prev.0 as isize,
                        at.1 as isize - prev.1 as isize,
                    );
                    let left = (-d.1, d.0);
                    outs.iter()
                        .position(|o| {
                            (o.0 as isize - at.0 as isize).signum() == left.0.signum()
                                && (o.1 as isize - at.1 as isize).signum() == left.1.signum()
                        })
                        .unwrap_or(0)
                };
                let to = outs.remove(pick);
                if outs.is_empty() {
                    next.remove(&at);
                }
                prev = at;
                at = to;
            }
            // Drop collinear vertices (cyclic).
            let n = path.len();
            let mut simple = Vec::with_capacity(n);
            for i in 0..n {
                let (a, b, c) = (path[(i + n - 1) % n], path[i], path[(i + 1) % n]);
                let collinear = (a.0 == b.0 && b.0 == c.0) || (a.1 == b.1 && b.1 == c.1);
                if !collinear {
                    simple.push([self.xs[b.0], self.ys[b.1]]);
                }
            }
            loops.push(simple);
        }
        loops
    }
}

/// No vertex repeats (the loop never touches itself).
fn simple(polygon: &[[i64; 2]]) -> bool {
    let mut seen = polygon.to_vec();
    seen.sort_unstable();
    seen.windows(2).all(|w| w[0] != w[1])
}

/// Exact cover of the union of `rects` in the requested style, in canonical
/// order. Rectangles must have positive width and height.
pub fn merge(rects: &[[i64; 4]], style: MergeStyle) -> Result<(Vec<Piece>, MergeStats)> {
    if let Some(bad) = rects.iter().find(|r| !valid(r)) {
        return Err(Error::Gds {
            code: "GDS_GEOMETRY",
            message: format!("empty rectangle {bad:?} in shape merge"),
        });
    }
    let mut stats = MergeStats {
        input: rects.len(),
        ..MergeStats::default()
    };
    if style == MergeStyle::Off {
        stats.rectangles = rects.len();
        return Ok((rects.iter().map(|r| Piece::Rect(*r)).collect(), stats));
    }
    let mut sorted = rects.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    for piece in pieces(&sorted) {
        if piece.len() == 1 {
            out.push(Piece::Rect(piece[0]));
            continue;
        }
        let Some(grid) = Grid::new(&piece) else {
            stats.capped_pieces += 1;
            out.extend(piece.iter().map(|r| Piece::Rect(*r)));
            continue;
        };
        if style == MergeStyle::Polygons {
            let loops = grid.loops();
            if loops.len() == 1 && loops[0].len() <= 8190 && simple(&loops[0]) {
                let polygon = loops.into_iter().next().expect("one loop");
                if polygon.len() == 4 {
                    let xs = polygon.iter().map(|p| p[0]);
                    let ys = polygon.iter().map(|p| p[1]);
                    out.push(Piece::Rect([
                        xs.clone().min().expect("x"),
                        ys.clone().min().expect("y"),
                        xs.max().expect("x"),
                        ys.max().expect("y"),
                    ]));
                } else {
                    out.push(Piece::Polygon(polygon));
                }
                continue;
            }
            if loops.len() > 1 {
                stats.holed_pieces += 1;
            }
        }
        let h = grid.slabs(true);
        let v = grid.slabs(false);
        let best = if v.len() < h.len() { v } else { h };
        out.extend(best.into_iter().map(Piece::Rect));
    }
    out.sort_unstable();
    for p in &out {
        match p {
            Piece::Rect(_) => stats.rectangles += 1,
            Piece::Polygon(_) => stats.polygons += 1,
        }
    }
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact union area by brute-force compression (independent of `Grid`).
    fn union_area(rects: &[[i64; 4]]) -> i128 {
        let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r[0], r[2]]).collect();
        let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r[1], r[3]]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let mut area = 0i128;
        for i in 0..xs.len().saturating_sub(1) {
            for j in 0..ys.len().saturating_sub(1) {
                if rects
                    .iter()
                    .any(|r| r[0] <= xs[i] && xs[i] < r[2] && r[1] <= ys[j] && ys[j] < r[3])
                {
                    area += i128::from(xs[i + 1] - xs[i]) * i128::from(ys[j + 1] - ys[j]);
                }
            }
        }
        area
    }
    fn polygon_area(p: &[[i64; 2]]) -> i128 {
        let n = p.len();
        let twice: i128 = (0..n)
            .map(|i| {
                let (a, b) = (p[i], p[(i + 1) % n]);
                i128::from(a[0]) * i128::from(b[1]) - i128::from(b[0]) * i128::from(a[1])
            })
            .sum();
        twice / 2
    }
    /// Point-in-region test at cell centres (scaled by 2 to stay integral).
    fn covers(pieces: &[Piece], x2: i64, y2: i64) -> usize {
        pieces
            .iter()
            .filter(|p| match p {
                Piece::Rect(r) => 2 * r[0] < x2 && x2 < 2 * r[2] && 2 * r[1] < y2 && y2 < 2 * r[3],
                Piece::Polygon(poly) => {
                    let n = poly.len();
                    let mut inside = false;
                    for i in 0..n {
                        let (a, b) = (poly[i], poly[(i + 1) % n]);
                        if a[0] == b[0] && 2 * a[0] > x2 {
                            let (lo, hi) = (2 * a[1].min(b[1]), 2 * a[1].max(b[1]));
                            if lo < y2 && y2 < hi {
                                inside = !inside;
                            }
                        }
                    }
                    inside
                }
            })
            .count()
    }
    /// Every compressed cell is covered exactly once if in the input union,
    /// never otherwise (so the output is an exact, non-overlapping cover).
    fn check_exact(input: &[[i64; 4]], style: MergeStyle) -> Vec<Piece> {
        let (out, stats) = merge(input, style).unwrap();
        let mut xs: Vec<i64> = input.iter().flat_map(|r| [r[0], r[2]]).collect();
        let mut ys: Vec<i64> = input.iter().flat_map(|r| [r[1], r[3]]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        for i in 0..xs.len() - 1 {
            for j in 0..ys.len() - 1 {
                let (cx, cy) = (xs[i] + xs[i + 1], ys[j] + ys[j + 1]);
                let want = input
                    .iter()
                    .any(|r| 2 * r[0] < cx && cx < 2 * r[2] && 2 * r[1] < cy && cy < 2 * r[3]);
                assert_eq!(
                    covers(&out, cx, cy),
                    usize::from(want),
                    "cell {i},{j} {style:?}"
                );
            }
        }
        let area: i128 = out
            .iter()
            .map(|p| match p {
                Piece::Rect(r) => i128::from(r[2] - r[0]) * i128::from(r[3] - r[1]),
                Piece::Polygon(poly) => polygon_area(poly),
            })
            .sum();
        assert_eq!(area, union_area(input));
        assert_eq!(stats.rectangles + stats.polygons, out.len());
        out
    }

    #[test]
    fn collinear_runs_split_at_vias_merge_into_one_rectangle() {
        // Three 200-wide runs with half-width end extensions, overlapping at splits.
        let runs = [
            [-100, -100, 1100, 100],
            [900, -100, 3100, 100],
            [2900, -100, 5100, 100],
        ];
        for style in [MergeStyle::Rectangles, MergeStyle::Polygons] {
            assert_eq!(
                check_exact(&runs, style),
                vec![Piece::Rect([-100, -100, 5100, 100])]
            );
        }
    }

    #[test]
    fn duplicates_and_covered_shapes_disappear() {
        let rects = [
            [0, 0, 1000, 200],
            [0, 0, 1000, 200],
            [100, 0, 300, 200],
            [500, 50, 600, 150],
        ];
        assert_eq!(
            check_exact(&rects, MergeStyle::Rectangles),
            vec![Piece::Rect([0, 0, 1000, 200])]
        );
    }

    #[test]
    fn l_and_t_junctions_become_one_polygon_or_two_or_three_slabs() {
        let l = [[0, 0, 1000, 200], [800, 0, 1000, 1000]];
        let polys = check_exact(&l, MergeStyle::Polygons);
        assert_eq!(
            polys,
            vec![Piece::Polygon(vec![
                [0, 0],
                [1000, 0],
                [1000, 1000],
                [800, 1000],
                [800, 200],
                [0, 200]
            ])]
        );
        assert_eq!(check_exact(&l, MergeStyle::Rectangles).len(), 2);
        let t = [[0, 0, 2000, 200], [900, 0, 1100, 1000]];
        assert_eq!(check_exact(&t, MergeStyle::Rectangles).len(), 2);
        assert!(
            matches!(check_exact(&t, MergeStyle::Polygons)[..], [Piece::Polygon(ref p)] if p.len() == 8)
        );
    }

    #[test]
    fn the_orientation_with_fewer_slabs_is_kept() {
        // A vertical trunk with three horizontal branches: vertical slabs need 4, horizontal 7.
        let rects = [
            [0, 0, 200, 3000],
            [200, 400, 1000, 600],
            [200, 1400, 1000, 1600],
            [200, 2400, 1000, 2600],
        ];
        assert_eq!(check_exact(&rects, MergeStyle::Rectangles).len(), 4);
    }

    #[test]
    fn corner_contact_does_not_connect_and_rings_keep_rectangles() {
        let corner = [[0, 0, 100, 100], [100, 100, 200, 200]];
        assert_eq!(
            check_exact(&corner, MergeStyle::Polygons),
            vec![
                Piece::Rect([0, 0, 100, 100]),
                Piece::Rect([100, 100, 200, 200])
            ]
        );
        // A closed ring (a same-net loop) has a hole: no polygon, exact rectangles.
        let ring = [
            [0, 0, 1000, 100],
            [0, 900, 1000, 1000],
            [0, 0, 100, 1000],
            [900, 0, 1000, 1000],
        ];
        let (out, stats) = merge(&ring, MergeStyle::Polygons).unwrap();
        assert_eq!(stats.holed_pieces, 1);
        assert!(out.iter().all(|p| matches!(p, Piece::Rect(_))));
        check_exact(&ring, MergeStyle::Polygons);
        // A pinch: two cells meeting diagonally inside one connected piece.
        let pinch = [
            [0, 0, 200, 100],
            [0, 0, 100, 300],
            [0, 200, 300, 300],
            [200, 0, 300, 300],
            [100, 100, 200, 200],
        ];
        check_exact(&pinch, MergeStyle::Polygons);
        check_exact(&pinch, MergeStyle::Rectangles);
    }

    #[test]
    fn output_is_independent_of_input_order_and_off_is_identity() {
        let mut rects = vec![
            [0, 0, 1000, 200],
            [800, 0, 1000, 1000],
            [5000, 0, 6000, 200],
            [5900, 0, 6000, 900],
        ];
        let (a, _) = merge(&rects, MergeStyle::Polygons).unwrap();
        rects.reverse();
        let (b, _) = merge(&rects, MergeStyle::Polygons).unwrap();
        assert_eq!(a, b);
        let (off, stats) = merge(&rects, MergeStyle::Off).unwrap();
        assert_eq!(
            off,
            rects.iter().map(|r| Piece::Rect(*r)).collect::<Vec<_>>()
        );
        assert_eq!(stats.rectangles, 4);
        assert!(merge(&[[0, 0, 0, 5]], MergeStyle::Rectangles).is_err());
        assert!(MergeStyle::parse("triangles").is_err());
        for s in ["off", "rectangles", "polygons"] {
            assert_eq!(MergeStyle::parse(s).unwrap().name(), s);
        }
    }

    #[test]
    fn random_rectilinear_sets_are_covered_exactly() {
        // Deterministic LCG; small coordinates force many overlaps, pinches and holes.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |m: i64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) % m as u64) as i64
        };
        for _ in 0..300 {
            let n = 1 + next(12) as usize;
            let rects: Vec<[i64; 4]> = (0..n)
                .map(|_| {
                    let (x, y) = (next(20) * 10, next(20) * 10);
                    [x, y, x + 10 + next(8) * 10, y + 10 + next(8) * 10]
                })
                .collect();
            for style in [MergeStyle::Rectangles, MergeStyle::Polygons] {
                let out = check_exact(&rects, style);
                for p in &out {
                    if let Piece::Polygon(poly) = p {
                        assert!(polygon_area(poly) > 0, "counter-clockwise");
                        let mut seen = poly.clone();
                        seen.sort_unstable();
                        seen.dedup();
                        assert_eq!(seen.len(), poly.len(), "simple polygon: no repeated vertex");
                    }
                }
            }
        }
    }
}
