//! Bounded, immutable per-layer uniform grids over retained conductor shapes.
//!
//! Build only after every retained shape (wire bodies, pieces, patches and
//! passive conductors) exists. Virtual cell geometry stays in
//! `InstanceGeometry`; this index never materializes it. Each layer's grid
//! stores `(rect, id)` entries inline in CSR order: cell-major, ascending
//! shape ID within a cell. Rectangles are stored relative to the layer's
//! origin in 32 bits when the layer's extent allows (20 bytes per entry),
//! otherwise absolutely. A query reports every entry that strictly
//! intersects it exactly once, from the first query cell the entry occupies.
//! Report order has no electrical meaning: callers sort the accepted IDs.
use super::{ExtractLimits, Shapes, budget, invalid};
use crate::{Rect, Result};

/// Spans at or above this use a single cell, so no cell arithmetic can
/// overflow (`x0 + x * cell` stays inside the layer's own extent).
const HUGE_SPAN: i128 = 1 << 62;

/// One indexed shape relative to its layer origin.
#[derive(Clone, Copy, Debug, Default)]
struct Compact {
    lx: u32,
    ly: u32,
    ux: u32,
    uy: u32,
    id: u32,
}
/// One indexed shape of a layer whose extent exceeds 32 bits.
#[derive(Clone, Copy, Debug)]
struct Wide {
    rect: Rect,
    id: u32,
}

trait Entry: Copy {
    fn new(rect: Rect, id: u32, x0: i64, y0: i64) -> Self;
    fn get(&self, x0: i64, y0: i64) -> (Rect, u32);
}
impl Entry for Compact {
    #[inline]
    fn new(r: Rect, id: u32, x0: i64, y0: i64) -> Self {
        // The layer origin is the minimum corner and the span fits 32 bits.
        Self {
            lx: (r.lx - x0) as u32,
            ly: (r.ly - y0) as u32,
            ux: (r.ux - x0) as u32,
            uy: (r.uy - y0) as u32,
            id,
        }
    }
    #[inline]
    fn get(&self, x0: i64, y0: i64) -> (Rect, u32) {
        (
            Rect {
                lx: x0 + i64::from(self.lx),
                ly: y0 + i64::from(self.ly),
                ux: x0 + i64::from(self.ux),
                uy: y0 + i64::from(self.uy),
            },
            self.id,
        )
    }
}
impl Entry for Wide {
    #[inline]
    fn new(rect: Rect, id: u32, _: i64, _: i64) -> Self {
        Self { rect, id }
    }
    #[inline]
    fn get(&self, _: i64, _: i64) -> (Rect, u32) {
        (self.rect, self.id)
    }
}

enum Entries {
    Compact(Vec<Compact>),
    Wide(Vec<Wide>),
}

struct LayerGrid {
    x0: i64,
    y0: i64,
    nx: i64,
    ny: i64,
    cell: i64,
    /// `nx * ny + 1` CSR offsets into `entries`.
    offsets: Vec<u32>,
    entries: Entries,
}
impl LayerGrid {
    /// Monotone cell column of `x`, clamped to the grid. Saturation keeps
    /// the order of far-away query coordinates, so clamping stays exact.
    #[inline]
    fn cx(&self, x: i64) -> i64 {
        x.saturating_sub(self.x0)
            .div_euclid(self.cell)
            .clamp(0, self.nx - 1)
    }
    #[inline]
    fn cy(&self, y: i64) -> i64 {
        y.saturating_sub(self.y0)
            .div_euclid(self.cell)
            .clamp(0, self.ny - 1)
    }
    fn cells(&self, r: Rect) -> (i64, i64, i64, i64) {
        (self.cx(r.lx), self.cx(r.ux), self.cy(r.ly), self.cy(r.uy))
    }
    #[inline]
    fn scan<E: Entry>(
        &self,
        entries: &[E],
        query: Rect,
        (qx0, qx1, qy0, qy1): (i64, i64, i64, i64),
        local: &mut usize,
        visitor: &mut impl FnMut(u32, Rect) -> Result<bool>,
    ) -> Result<bool> {
        for y in qy0..=qy1 {
            // An entry is reported from its own first cell, or from the
            // query's first row/column when it starts before the query.
            let y_lo = self.y0 + y * self.cell;
            for x in qx0..=qx1 {
                let x_lo = self.x0 + x * self.cell;
                let c = (y * self.nx + x) as usize;
                let (a, b) = (self.offsets[c] as usize, self.offsets[c + 1] as usize);
                *local += b - a;
                for e in &entries[a..b] {
                    let (r, id) = e.get(self.x0, self.y0);
                    if (x == qx0 || r.lx >= x_lo)
                        && (y == qy0 || r.ly >= y_lo)
                        && intersects(r, query)
                        && !visitor(id, r)?
                    {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

/// Strict endpoint inequalities deliberately accept a zero-width query when
/// a positive-width shape straddles it, matching the extraction predicates.
#[inline]
fn intersects(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

/// Per-layer plan: bounds and the chosen cell.
#[derive(Clone, Copy)]
struct Plan {
    bounds: Rect,
    cell: i64,
    nx: i64,
    ny: i64,
}
impl Plan {
    fn compact(&self) -> bool {
        let fits = |lo: i64, hi: i64| i128::from(hi) - i128::from(lo) <= i128::from(u32::MAX);
        fits(self.bounds.lx, self.bounds.ux) && fits(self.bounds.ly, self.bounds.uy)
    }
    fn entry_bytes(&self) -> u64 {
        if self.compact() {
            size_of::<Compact>() as u64
        } else {
            size_of::<Wide>() as u64
        }
    }
}

/// Cell side for a layer: at least `base`, doubled until the layer has no
/// more cells than shapes. Cells therefore never outnumber entries, which
/// bounds the offset table by the entry table.
fn plan(bounds: Rect, shapes: u64, base: i64) -> Plan {
    let span_x = i128::from(bounds.ux) - i128::from(bounds.lx);
    let span_y = i128::from(bounds.uy) - i128::from(bounds.ly);
    let single = Plan {
        bounds,
        cell: i64::MAX,
        nx: 1,
        ny: 1,
    };
    if span_x >= HUGE_SPAN || span_y >= HUGE_SPAN || i128::from(base) >= HUGE_SPAN {
        return single;
    }
    let limit = i128::from(shapes.max(1));
    let mut cell = i128::from(base.max(1));
    loop {
        let (nx, ny) = (span_x / cell + 1, span_y / cell + 1);
        if nx * ny <= limit {
            return Plan {
                bounds,
                cell: cell as i64,
                nx: nx as i64,
                ny: ny as i64,
            };
        }
        if cell > span_x.max(span_y) {
            return single;
        }
        cell *= 2;
    }
}

/// Immutable per-layer grids. Shape IDs and geometry must remain unchanged
/// between build and queries.
pub(super) struct GridIndex {
    layers: Vec<Option<LayerGrid>>,
    bytes: u64,
    entries: usize,
}

fn fill<E: Entry>(shapes: &Shapes<'_>, layer: u32, g: &LayerGrid, total: usize) -> Result<Vec<E>> {
    let mut entries: Vec<E> = Vec::new();
    entries
        .try_reserve_exact(total)
        .map_err(|_| budget("extraction grid entry allocation"))?;
    let blank = E::new(
        Rect {
            lx: g.x0,
            ly: g.y0,
            ux: g.x0,
            uy: g.y0,
        },
        0,
        g.x0,
        g.y0,
    );
    entries.resize(total, blank);
    let mut cursor = g.offsets[..g.offsets.len() - 1].to_vec();
    // Ascending shape ID: entries of a cell stay ID-ordered.
    for (id, s) in shapes.owned().filter(|(_, s)| s.layer == layer) {
        let (x0, x1, y0, y1) = g.cells(s.rect);
        for y in y0..=y1 {
            for x in x0..=x1 {
                let at = &mut cursor[(y * g.nx + x) as usize];
                entries[*at as usize] = E::new(s.rect, id, g.x0, g.y0);
                *at += 1;
            }
        }
    }
    Ok(entries)
}

impl GridIndex {
    /// Exact heap bytes of the built index (offsets plus entries).
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(super) fn entries(&self) -> usize {
        self.entries
    }

    /// Builds one grid per populated layer. `base_cell[layer]` is the
    /// smallest cell side (DBU); populated layers beyond it use one DBU.
    /// Entries are capped by `limits.max_bin_entries`, and the exact table
    /// bytes must fit `allowance`. Every inserted entry is charged to `work`.
    pub(super) fn build(
        shapes: &Shapes<'_>,
        layer_count: usize,
        base_cell: &[i64],
        limits: &ExtractLimits,
        allowance: u64,
        work: &mut usize,
    ) -> Result<Self> {
        let mut bounds: Vec<Option<(Rect, u64)>> = vec![None; layer_count];
        for (_, s) in shapes.owned() {
            if s.layer as usize >= layer_count || !s.rect.valid() {
                return Err(invalid("invalid retained shape-index geometry"));
            }
            let item = &mut bounds[s.layer as usize];
            *item = Some(match *item {
                None => (s.rect, 1),
                Some((b, n)) => (
                    Rect {
                        lx: b.lx.min(s.rect.lx),
                        ly: b.ly.min(s.rect.ly),
                        ux: b.ux.max(s.rect.ux),
                        uy: b.uy.max(s.rect.uy),
                    },
                    n + 1,
                ),
            });
        }
        let plans: Vec<Option<Plan>> = bounds
            .iter()
            .enumerate()
            .map(|(l, b)| b.map(|(r, n)| plan(r, n, base_cell.get(l).copied().unwrap_or(1))))
            .collect();
        let mut layers: Vec<Option<LayerGrid>> = Vec::with_capacity(layer_count);
        let mut bytes = 0u64;
        for p in &plans {
            let Some(p) = *p else {
                layers.push(None);
                continue;
            };
            let cells = usize::try_from(p.nx * p.ny + 1)
                .map_err(|_| budget("extraction grid cell count"))?;
            // The fill cursor duplicates the offsets while entries are placed.
            bytes = bytes.saturating_add(2 * cells as u64 * size_of::<u32>() as u64);
            if bytes > allowance {
                return Err(budget("extraction grid index memory"));
            }
            let mut offsets = Vec::new();
            offsets
                .try_reserve_exact(cells)
                .map_err(|_| budget("extraction grid cell allocation"))?;
            offsets.resize(cells, 0u32);
            layers.push(Some(LayerGrid {
                x0: p.bounds.lx,
                y0: p.bounds.ly,
                nx: p.nx,
                ny: p.ny,
                cell: p.cell,
                offsets,
                entries: Entries::Compact(Vec::new()),
            }));
        }
        // Count pass: entries per cell, capped before any entry allocation.
        let mut total = 0usize;
        let mut per_layer = vec![0usize; layer_count];
        for (_, s) in shapes.owned() {
            let g = layers[s.layer as usize]
                .as_mut()
                .expect("planned populated layer");
            let (x0, x1, y0, y1) = g.cells(s.rect);
            let n = ((x1 - x0 + 1) as usize).saturating_mul((y1 - y0 + 1) as usize);
            total = total
                .checked_add(n)
                .filter(|&t| t <= limits.max_bin_entries && t < u32::MAX as usize)
                .ok_or_else(|| budget("extraction spatial-bin entries"))?;
            per_layer[s.layer as usize] += n;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    g.offsets[(y * g.nx + x) as usize + 1] += 1;
                }
            }
        }
        *work = work
            .checked_add(total)
            .filter(|&n| n <= limits.max_work)
            .ok_or_else(|| budget("extraction grid-index work"))?;
        for (l, p) in plans.iter().enumerate() {
            if let Some(p) = p {
                bytes = bytes.saturating_add(per_layer[l] as u64 * p.entry_bytes());
            }
        }
        if bytes > allowance {
            return Err(budget("extraction grid index memory"));
        }
        for (l, g) in layers.iter_mut().enumerate() {
            let (Some(g), Some(p)) = (g.as_mut(), plans[l]) else {
                continue;
            };
            for i in 1..g.offsets.len() {
                g.offsets[i] += g.offsets[i - 1];
            }
            g.entries = if p.compact() {
                Entries::Compact(fill(shapes, l as u32, g, per_layer[l])?)
            } else {
                Entries::Wide(fill(shapes, l as u32, g, per_layer[l])?)
            };
        }
        // Report the resident tables (the fill cursors are released).
        let resident = layers
            .iter()
            .flatten()
            .map(|g| {
                g.offsets.len() as u64 * size_of::<u32>() as u64
                    + match &g.entries {
                        Entries::Compact(e) => (e.len() * size_of::<Compact>()) as u64,
                        Entries::Wide(e) => (e.len() * size_of::<Wide>()) as u64,
                    }
            })
            .sum();
        Ok(Self {
            layers,
            bytes: resident,
            entries: total,
        })
    }

    /// Visit every entry of `layer` strictly intersecting `query` once, as
    /// `(shape ID, rectangle)`. The visitor returns `false` to stop early
    /// (`Ok(false)` is then returned). Each scanned cell and entry is
    /// charged to `work`.
    #[inline]
    pub(super) fn visit(
        &self,
        layer: u32,
        query: Rect,
        limits: &ExtractLimits,
        work: &mut usize,
        mut visitor: impl FnMut(u32, Rect) -> Result<bool>,
    ) -> Result<bool> {
        if query.lx > query.ux || query.ly > query.uy {
            return Err(invalid("invalid shape-index query"));
        }
        let Some(g) = self.layers.get(layer as usize).and_then(Option::as_ref) else {
            return Ok(true);
        };
        let range = g.cells(query);
        let (qx0, qx1, qy0, qy1) = range;
        let cells = (qx1 - qx0 + 1) as u64 * (qy1 - qy0 + 1) as u64;
        if cells > limits.max_bin_entries as u64 {
            return Err(budget("extraction query extent"));
        }
        let mut local = cells as usize;
        let result = match &g.entries {
            Entries::Compact(e) => g.scan(e, query, range, &mut local, &mut visitor),
            Entries::Wide(e) => g.scan(e, query, range, &mut local, &mut visitor),
        };
        *work = work
            .checked_add(local)
            .filter(|&n| n <= limits.max_work)
            .ok_or_else(|| budget("extraction grid-index work"))?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Owner, PassiveOwner, Shape};

    fn shapes(rects: impl IntoIterator<Item = (u32, Rect)>) -> Shapes<'static> {
        Shapes::new(
            rects
                .into_iter()
                .map(|(layer, rect)| Shape {
                    layer,
                    rect,
                    owner: Owner::Passive(PassiveOwner::Ground),
                })
                .collect(),
            0,
            None,
            &ExtractLimits::default(),
        )
        .unwrap()
    }

    fn build(shapes: &Shapes<'_>, layers: usize, cell: i64) -> GridIndex {
        let limits = ExtractLimits::default();
        let mut work = 0;
        GridIndex::build(
            shapes,
            layers,
            &vec![cell; layers],
            &limits,
            u64::MAX,
            &mut work,
        )
        .unwrap()
    }

    /// Independent oracle: direct endpoint comparisons, no grid helper.
    fn brute(shapes: &Shapes<'_>, layer: u32, q: Rect) -> Vec<u32> {
        shapes
            .owned()
            .filter(|(_, s)| {
                s.layer == layer
                    && s.rect.ux > q.lx
                    && s.rect.lx < q.ux
                    && s.rect.uy > q.ly
                    && s.rect.ly < q.uy
            })
            .map(|(id, _)| id)
            .collect()
    }

    fn query(index: &GridIndex, layer: u32, q: Rect) -> Vec<u32> {
        let limits = ExtractLimits::default();
        let mut found = Vec::new();
        index
            .visit(layer, q, &limits, &mut 0, |id, _| {
                found.push(id);
                Ok(true)
            })
            .unwrap();
        let reported = found.len();
        found.sort_unstable();
        found.dedup();
        assert_eq!(found.len(), reported, "duplicate report for {q:?}");
        found
    }

    /// Deterministic 64-bit LCG for reproducible random geometry.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) % n as u64) as i64
        }
    }

    #[test]
    fn random_boxes_touching_edges_and_zero_width_queries_match_brute_force() {
        let mut rng = Lcg(7);
        let mut input = Vec::new();
        for i in 0..1500 {
            let layer = (i % 4) as u32;
            // Snap to a coarse lattice so that many edges touch exactly and
            // many shapes share cell boundaries.
            let (x, y) = (rng.next(400) * 5 - 900, rng.next(400) * 5 - 700);
            let (w, h) = match i % 7 {
                0 => (rng.next(4000) + 1, 5),
                1 => (5, rng.next(4000) + 1),
                2 => (50, 50),
                _ => (rng.next(60) + 1, rng.next(60) + 1),
            };
            input.push((
                layer,
                Rect {
                    lx: x,
                    ly: y,
                    ux: x + w,
                    uy: y + h,
                },
            ));
        }
        let shapes = shapes(input);
        let mut queries = Vec::new();
        for _ in 0..600 {
            let (x, y) = (rng.next(500) * 5 - 1200, rng.next(500) * 5 - 1000);
            let (w, h) = match rng.next(5) {
                0 => (0, rng.next(200)),
                1 => (rng.next(200), 0),
                2 => (0, 0),
                _ => (rng.next(300), rng.next(300)),
            };
            queries.push(Rect {
                lx: x,
                ly: y,
                ux: x + w,
                uy: y + h,
            });
        }
        queries.push(Rect {
            lx: i64::MIN,
            ly: i64::MIN,
            ux: i64::MAX,
            uy: i64::MAX,
        });
        queries.push(Rect {
            lx: 1 << 40,
            ly: 1 << 40,
            ux: (1 << 40) + 5,
            uy: (1 << 40) + 5,
        });
        // A small base cell forces doubling; a huge one forces one cell.
        for cell in [1, 7, 64, 1000, 1 << 50] {
            let index = build(&shapes, 5, cell);
            for &q in &queries {
                for layer in 0..5 {
                    assert_eq!(query(&index, layer, q), brute(&shapes, layer, q), "{q:?}");
                }
            }
        }
    }

    #[test]
    fn extreme_coordinates_use_one_cell_and_stay_exact() {
        let shapes = shapes([
            (
                0,
                Rect {
                    lx: i64::MIN + 1,
                    ly: -1,
                    ux: i64::MIN + 10,
                    uy: 1,
                },
            ),
            (
                0,
                Rect {
                    lx: i64::MAX - 10,
                    ly: -1,
                    ux: i64::MAX,
                    uy: 1,
                },
            ),
            (
                0,
                Rect {
                    lx: -5,
                    ly: i64::MIN,
                    ux: 5,
                    uy: i64::MAX,
                },
            ),
        ]);
        let index = build(&shapes, 1, 1);
        for q in [
            Rect {
                lx: i64::MIN,
                ly: 0,
                ux: i64::MIN + 5,
                uy: 0,
            },
            Rect {
                lx: i64::MAX - 1,
                ly: -2,
                ux: i64::MAX,
                uy: 2,
            },
            Rect {
                lx: 0,
                ly: 0,
                ux: 0,
                uy: 0,
            },
            Rect {
                lx: 5,
                ly: 0,
                ux: 6,
                uy: 1,
            },
        ] {
            assert_eq!(query(&index, 0, q), brute(&shapes, 0, q), "{q:?}");
        }
        assert_eq!(index.entries(), 3);
    }

    #[test]
    fn cells_never_outnumber_shapes_and_bytes_are_exact() {
        let shapes = shapes((0..100i64).map(|i| {
            (
                (i % 2) as u32,
                Rect {
                    lx: i * 1000,
                    ly: 0,
                    ux: i * 1000 + 10,
                    uy: 10,
                },
            )
        }));
        let index = build(&shapes, 3, 1);
        let mut bytes = 0u64;
        for g in index.layers.iter().flatten() {
            assert!(g.offsets.len() - 1 <= 50, "{} cells", g.offsets.len() - 1);
            let Entries::Compact(e) = &g.entries else {
                panic!("small layers use compact entries")
            };
            bytes += (g.offsets.capacity() * 4 + e.capacity() * size_of::<Compact>()) as u64;
        }
        assert!(index.layers[2].is_none());
        assert_eq!(bytes, index.bytes());
        assert_eq!(size_of::<Compact>(), 20);
        assert_eq!(size_of::<Wide>(), 40);
    }

    #[test]
    fn budgets_fail_without_unbounded_scans() {
        let shapes = shapes((0..256).map(|i| {
            (
                0,
                Rect {
                    lx: i,
                    ly: 0,
                    ux: i + 1,
                    uy: 1,
                },
            )
        }));
        let mut limits = ExtractLimits::default();
        let mut work = 0;
        // Entry cap, memory allowance and construction work are enforced.
        let capped = ExtractLimits {
            max_bin_entries: 255,
            ..limits.clone()
        };
        assert!(GridIndex::build(&shapes, 1, &[1], &capped, u64::MAX, &mut 0).is_err());
        assert!(GridIndex::build(&shapes, 1, &[1], &limits, 1000, &mut 0).is_err());
        let tight = ExtractLimits {
            max_work: 10,
            ..limits.clone()
        };
        assert!(GridIndex::build(&shapes, 1, &[1], &tight, u64::MAX, &mut 0).is_err());
        let index = GridIndex::build(&shapes, 1, &[1], &limits, u64::MAX, &mut work).unwrap();
        assert_eq!(work, index.entries());
        assert!(index.entries() >= 256);
        // A local query is charged a few cells, not the whole layer.
        let mut local = 0;
        index
            .visit(
                0,
                Rect {
                    lx: 100,
                    ly: 0,
                    ux: 101,
                    uy: 1,
                },
                &limits,
                &mut local,
                |_, _| Ok(true),
            )
            .unwrap();
        assert!(local < 16, "local query charged {local}");
        limits.max_work = work + 3;
        let whole = Rect {
            lx: -1,
            ly: -1,
            ux: 300,
            uy: 2,
        };
        assert!(
            index
                .visit(0, whole, &limits, &mut work, |_, _| Ok(true))
                .is_err()
        );
        // Early stop, visitor errors and malformed queries propagate.
        let limits = ExtractLimits::default();
        let mut visits = 0;
        assert!(
            !index
                .visit(0, whole, &limits, &mut 0, |_, _| {
                    visits += 1;
                    Ok(visits < 3)
                })
                .unwrap()
        );
        assert_eq!(visits, 3);
        assert!(
            index
                .visit(0, whole, &limits, &mut 0, |_, _| Err(budget(
                    "visitor control"
                )))
                .is_err()
        );
        assert!(
            index
                .visit(0, Rect { ux: -3, ..whole }, &limits, &mut 0, |_, _| Ok(
                    true
                ))
                .is_err()
        );
        assert!(
            index
                .visit(9, whole, &limits, &mut 0, |_, _| Ok(true))
                .unwrap()
        );
        let bad = self::shapes([(
            1,
            Rect {
                lx: 0,
                ly: 0,
                ux: 1,
                uy: 1,
            },
        )]);
        assert!(GridIndex::build(&bad, 1, &[1], &limits, u64::MAX, &mut 0).is_err());
    }

    #[test]
    fn virtual_shape_ids_are_skipped_without_materializing_their_geometry() {
        use crate::extract::InstancedPassive;
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        let rect = Rect {
            lx: -1,
            ly: -1,
            ux: 1,
            uy: 1,
        };
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(
                    10,
                    0,
                    vec![TemplateObstacle {
                        layer: 0,
                        rect,
                        pin: None,
                    }],
                )
                .unwrap(),
            ],
            vec![ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [0, 0],
                orientation: crate::Orientation::FS,
            }],
            vec![],
        )
        .unwrap()
        .with_pin_ids(vec![])
        .unwrap();
        let limits = ExtractLimits::default();
        let shape = Shape {
            layer: 0,
            rect,
            owner: Owner::Passive(PassiveOwner::Ground),
        };
        let mut shapes = Shapes::new(
            vec![shape, shape],
            1,
            Some(InstancedPassive {
                geometry: &source,
                signal_nets: &[],
            }),
            &limits,
        )
        .unwrap();
        shapes.push(shape);
        assert_eq!(shapes.virtual_count(), 1);
        let index = build(&shapes, 1, 1);
        assert_eq!(query(&index, 0, rect), [0, 2, 3]);
        assert_eq!(index.entries(), 3);
    }
}
