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
use super::{ExtractLimits, Owner, PassiveOwner, Shape, Shapes, budget, invalid};
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
    /// Exact reciprocal of `cell` for 32-bit offsets (0: none).
    magic: u64,
    /// Largest offset of the last column and row (`n * cell - 1`).
    last_x: i64,
    last_y: i64,
    /// `nx * ny + 1` CSR offsets into `entries`.
    offsets: Vec<u32>,
    entries: Entries,
}
/// `floor(n / d)` for `n < 2^32` and `2 <= d < 2^32` with `magic =
/// u64::MAX / d + 1` (Lemire, Kaser and Kurz, "Faster remainder by direct
/// computation", Software: Practice and Experience 49(6), 2019).
#[inline]
fn fast_div(n: u64, magic: u64) -> u64 {
    ((u128::from(magic) * u128::from(n)) >> 64) as u64
}
impl LayerGrid {
    fn new(x0: i64, y0: i64, nx: i64, ny: i64, cell: i64, offsets: Vec<u32>) -> Self {
        let last = |n: i64| i64::try_from(i128::from(n) * i128::from(cell) - 1).unwrap_or(i64::MAX);
        let (last_x, last_y) = (last(nx), last(ny));
        let fits = |v: i64| v <= i64::from(u32::MAX);
        let magic = if (2..=i64::from(u32::MAX)).contains(&cell) && fits(last_x) && fits(last_y) {
            u64::MAX / cell as u64 + 1
        } else {
            0
        };
        Self {
            x0,
            y0,
            nx,
            ny,
            cell,
            magic,
            last_x,
            last_y,
            offsets,
            entries: Entries::Compact(Vec::new()),
        }
    }
    /// Cell of offset `v` along an axis whose last offset is `last`: the
    /// original `v.div_euclid(cell).clamp(0, n - 1)`.
    #[inline]
    fn axis(&self, v: i64, last: i64, n: i64) -> i64 {
        if self.magic != 0 {
            if v <= 0 {
                0
            } else {
                fast_div(v.min(last) as u64, self.magic) as i64
            }
        } else {
            v.div_euclid(self.cell).clamp(0, n - 1)
        }
    }
    /// Monotone cell column of `x`, clamped to the grid. Saturation keeps
    /// the order of far-away query coordinates, so clamping stays exact.
    #[inline]
    fn cx(&self, x: i64) -> i64 {
        self.axis(x.saturating_sub(self.x0), self.last_x, self.nx)
    }
    #[inline]
    fn cy(&self, y: i64) -> i64 {
        self.axis(y.saturating_sub(self.y0), self.last_y, self.ny)
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

impl LayerGrid {
    /// `scan` over compact entries, compared in the layer's 32-bit offsets:
    /// the query is translated once (saturating: every entry offset lies in
    /// `0..2^32`, so each comparison keeps its outcome) instead of every
    /// entry being translated.
    #[inline]
    fn scan_compact(
        &self,
        entries: &[Compact],
        query: Rect,
        (qx0, qx1, qy0, qy1): (i64, i64, i64, i64),
        local: &mut usize,
        visitor: &mut impl FnMut(u32, Rect) -> Result<bool>,
    ) -> Result<bool> {
        let (lx, ux) = (
            query.lx.saturating_sub(self.x0),
            query.ux.saturating_sub(self.x0),
        );
        let (ly, uy) = (
            query.ly.saturating_sub(self.y0),
            query.uy.saturating_sub(self.y0),
        );
        for y in qy0..=qy1 {
            let y_lo = y * self.cell;
            for x in qx0..=qx1 {
                let x_lo = x * self.cell;
                let c = (y * self.nx + x) as usize;
                let (a, b) = (self.offsets[c] as usize, self.offsets[c + 1] as usize);
                *local += b - a;
                for e in &entries[a..b] {
                    let (elx, ely) = (i64::from(e.lx), i64::from(e.ly));
                    let (eux, euy) = (i64::from(e.ux), i64::from(e.uy));
                    if (x == qx0 || elx >= x_lo)
                        && (y == qy0 || ely >= y_lo)
                        && elx < ux
                        && lx < eux
                        && ely < uy
                        && ly < euy
                    {
                        let (r, id) = e.get(self.x0, self.y0);
                        if !visitor(id, r)? {
                            return Ok(false);
                        }
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
    /// Virtual shape IDs `virtual_ids` are indexed too (on the layers given
    /// at build). By virtual offset: a bit per signal-pin shape, the pin
    /// count before each 64-shape word, and `(net, pin)` per pin in order;
    /// every other virtual shape is ground.
    virtual_ids: std::ops::Range<u32>,
    pin_bits: Vec<u64>,
    pin_rank: Vec<u32>,
    pins: Vec<(u32, u32)>,
}

/// Every indexed `(id, layer, rect)` in ascending ID: retained shapes below
/// the virtual range, virtual shapes on `virtual_layers`, then the
/// remaining retained shapes.
fn indexed<'s>(
    shapes: &'s Shapes<'_>,
    virtual_layers: Option<&'s [bool]>,
) -> impl Iterator<Item = (u32, u32, Rect)> + 's {
    let (front, count) = (shapes.virtual_offset(), shapes.virtual_count());
    let retained = shapes.retained();
    let head = retained[..front.min(retained.len())]
        .iter()
        .enumerate()
        .map(|(i, s)| (i as u32, s.layer, s.rect));
    let tail = retained[front.min(retained.len())..]
        .iter()
        .enumerate()
        .map(move |(i, s)| ((front + count + i) as u32, s.layer, s.rect));
    let virt = virtual_layers.into_iter().flat_map(move |layers| {
        shapes
            .virtual_shapes()
            .filter(move |(_, s)| layers.get(s.layer as usize).copied().unwrap_or(false))
            .map(|(id, s)| (id, s.layer, s.rect))
    });
    head.chain(virt).chain(tail)
}

/// `total` blank entries of grid `g`.
fn blank_entries<E: Entry>(g: &LayerGrid, total: usize) -> Result<Vec<E>> {
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
    Ok(entries)
}

/// `blank_entries`, written by the pool in parallel.
fn blank_table<E: Entry + Send + Sync>(g: &LayerGrid, total: usize) -> Result<Vec<E>> {
    use rayon::prelude::*;
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
    entries.par_extend(rayon::iter::repeat_n(blank, total));
    Ok(entries)
}

/// Places `(id, rect)` in every cell of `g` it occupies.
#[inline]
fn place<E: Entry>(g: &LayerGrid, cursor: &mut [u32], entries: &mut [E], id: u32, rect: Rect) {
    let (x0, x1, y0, y1) = g.cells(rect);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let at = &mut cursor[(y * g.nx + x) as usize];
            entries[*at as usize] = E::new(rect, id, g.x0, g.y0);
            *at += 1;
        }
    }
}

/// Fills every layer's entries in one pass over the indexed shapes, in
/// ascending shape ID, so the entries of each cell stay ID-ordered (the
/// order of one pass per layer). `per_layer` holds each layer's entry
/// count; offsets must already be prefix sums.
fn fill(
    shapes: &Shapes<'_>,
    virtual_layers: Option<&[bool]>,
    layers: &mut [Option<LayerGrid>],
    compact: &[bool],
    per_layer: &[usize],
) -> Result<()> {
    let mut fills: Vec<Option<(Entries, Vec<u32>)>> = Vec::with_capacity(layers.len());
    for (l, g) in layers.iter().enumerate() {
        fills.push(match g {
            None => None,
            Some(g) => {
                let entries = if compact[l] {
                    Entries::Compact(blank_entries(g, per_layer[l])?)
                } else {
                    Entries::Wide(blank_entries(g, per_layer[l])?)
                };
                Some((entries, g.offsets[..g.offsets.len() - 1].to_vec()))
            }
        });
    }
    for (id, layer, rect) in indexed(shapes, virtual_layers) {
        let (Some(g), Some((entries, cursor))) = (
            layers[layer as usize].as_ref(),
            fills[layer as usize].as_mut(),
        ) else {
            return Err(invalid("grid fill outside a planned layer"));
        };
        match entries {
            Entries::Compact(e) => place(g, cursor, e, id, rect),
            Entries::Wide(e) => place(g, cursor, e, id, rect),
        }
    }
    for (g, f) in layers.iter_mut().zip(fills) {
        if let (Some(g), Some((entries, _))) = (g.as_mut(), f) {
            g.entries = entries;
        }
    }
    Ok(())
}

/// Entries of the grids `plans` over the indexed shapes; counting stops
/// once the total exceeds `cap`.
fn count_entries(
    shapes: &Shapes<'_>,
    virtual_layers: Option<&[bool]>,
    plans: &[Option<Plan>],
    cap: usize,
) -> usize {
    let mut total = 0usize;
    for (_, layer, r) in indexed(shapes, virtual_layers) {
        let Some(p) = plans.get(layer as usize).copied().flatten() else {
            continue;
        };
        let at = |v: i64, lo: i64, n: i64| v.saturating_sub(lo).div_euclid(p.cell).clamp(0, n - 1);
        let nx = at(r.ux, p.bounds.lx, p.nx) - at(r.lx, p.bounds.lx, p.nx) + 1;
        let ny = at(r.uy, p.bounds.ly, p.ny) - at(r.ly, p.bounds.ly, p.ny) + 1;
        total = total.saturating_add((nx as usize).saturating_mul(ny as usize));
        if total > cap {
            break;
        }
    }
    total
}

/// Retained shapes per part of the parallel build.
const PART_SHAPES: usize = 1 << 14;
/// Instances per part of the parallel build.
const PART_INSTANCES: usize = 1 << 10;
/// Row-band tasks per pool thread of the parallel count and fill.
const BANDS_PER_THREAD: usize = 4;

/// A contiguous run of the indexed sequence (ascending shape ID).
#[derive(Clone, Copy, Debug)]
enum Part {
    /// Retained shapes `start..end`, whose IDs are their index plus `shift`.
    Retained {
        start: usize,
        end: usize,
        shift: usize,
    },
    /// The virtual shapes of instances `start..end` on the virtual layers.
    Virtual { start: usize, end: usize },
}

/// The indexed sequence of `indexed` cut into parts, in ID order.
fn parts(shapes: &Shapes<'_>, virtual_layers: Option<&[bool]>) -> Vec<Part> {
    let len = shapes.retained_len();
    let front = shapes.virtual_offset().min(len);
    let mut out = Vec::new();
    let retained = |start: usize, end: usize, shift: usize, out: &mut Vec<Part>| {
        let mut at = start;
        while at < end {
            let next = end.min(at + PART_SHAPES);
            out.push(Part::Retained {
                start: at,
                end: next,
                shift,
            });
            at = next;
        }
    };
    retained(0, front, 0, &mut out);
    if virtual_layers.is_some() {
        let instances = shapes.virtual_instances();
        let mut at = 0;
        while at < instances {
            let next = instances.min(at + PART_INSTANCES);
            out.push(Part::Virtual {
                start: at,
                end: next,
            });
            at = next;
        }
    }
    retained(front, len, shapes.virtual_count(), &mut out);
    out
}

/// Visits the `(id, layer, rect)` of one part in ascending ID, exactly as
/// `indexed` does.
fn visit_part(
    shapes: &Shapes<'_>,
    virtual_layers: Option<&[bool]>,
    part: Part,
    mut f: impl FnMut(u32, u32, Rect),
) {
    match part {
        Part::Retained { start, end, shift } => {
            for (i, s) in shapes.retained()[start..end].iter().enumerate() {
                f((start + i + shift) as u32, s.layer, s.rect);
            }
        }
        Part::Virtual { start, end } => {
            let layers = virtual_layers.unwrap_or(&[]);
            for (id, s) in shapes.virtual_shapes_of(start..end) {
                if layers.get(s.layer as usize).copied().unwrap_or(false) {
                    f(id, s.layer, s.rect);
                }
            }
        }
    }
}

/// Bounds and shape count of each layer, per part (`None`: a shape on a
/// layer past `layer_count` or with an invalid rectangle).
type LayerBounds = Vec<Option<(Rect, u64)>>;
fn part_bounds(
    shapes: &Shapes<'_>,
    virtual_layers: Option<&[bool]>,
    parts: &[Part],
    layer_count: usize,
) -> Option<Vec<LayerBounds>> {
    use rayon::prelude::*;
    parts
        .par_iter()
        .map(|&part| {
            let mut bounds: LayerBounds = vec![None; layer_count];
            let mut valid = true;
            visit_part(shapes, virtual_layers, part, |_, layer, rect| {
                if layer as usize >= layer_count || !rect.valid() {
                    valid = false;
                    return;
                }
                let item = &mut bounds[layer as usize];
                *item = Some(match *item {
                    None => (rect, 1),
                    Some((b, n)) => (union(b, rect), n + 1),
                });
            });
            valid.then_some(bounds)
        })
        .collect()
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    }
}

impl Entries {
    fn compact(&self) -> &[Compact] {
        match self {
            Entries::Compact(e) => e,
            Entries::Wide(_) => &[],
        }
    }
    fn wide(&self) -> &[Wide] {
        match self {
            Entries::Wide(e) => e,
            Entries::Compact(_) => &[],
        }
    }
}

/// Every part's shapes of each layer as that layer's entries (ID order),
/// split by the row bands `band_rows[layer]` they reach (a shape is in
/// every band its cell rows meet). `None` once the copies exceed `room`
/// bytes (checked part by part, so at most one part per pool thread more
/// is held).
fn part_lists(
    shapes: &Shapes<'_>,
    virtual_layers: Option<&[bool]>,
    parts: &[Part],
    layers: &[Option<LayerGrid>],
    compact: &[bool],
    band_rows: &[Vec<(i64, i64)>],
    room: u64,
) -> Option<Vec<Vec<Vec<Entries>>>> {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    let used = AtomicU64::new(0);
    // The band of each cell row.
    let band_of: Vec<Vec<u32>> = band_rows
        .iter()
        .map(|rows| {
            rows.iter()
                .enumerate()
                .flat_map(|(b, &(r0, r1))| (r0..r1).map(move |_| b as u32))
                .collect()
        })
        .collect();
    parts
        .par_iter()
        .map(|&part| {
            if used.load(Ordering::Relaxed) > room {
                return None;
            }
            let mut lists: Vec<Vec<Entries>> = band_rows
                .iter()
                .zip(compact)
                .map(|(rows, &c)| {
                    rows.iter()
                        .map(|_| {
                            if c {
                                Entries::Compact(Vec::new())
                            } else {
                                Entries::Wide(Vec::new())
                            }
                        })
                        .collect()
                })
                .collect();
            visit_part(shapes, virtual_layers, part, |id, layer, rect| {
                let l = layer as usize;
                if let Some(g) = layers[l].as_ref() {
                    let (b0, b1) = (
                        band_of[l][g.cy(rect.ly) as usize],
                        band_of[l][g.cy(rect.uy) as usize],
                    );
                    for list in &mut lists[l][b0 as usize..=b1 as usize] {
                        match list {
                            Entries::Compact(e) => e.push(Compact::new(rect, id, g.x0, g.y0)),
                            Entries::Wide(e) => e.push(Wide::new(rect, id, g.x0, g.y0)),
                        }
                    }
                }
            });
            let bytes = list_bytes(lists.iter().flatten());
            (used.fetch_add(bytes, Ordering::Relaxed) + bytes <= room).then_some(lists)
        })
        .collect()
}

/// Bytes of the entries held by `lists` (capacity included).
fn list_bytes<'a>(lists: impl Iterator<Item = &'a Entries>) -> u64 {
    lists
        .map(|e| match e {
            Entries::Compact(e) => (e.capacity() * size_of::<Compact>()) as u64,
            Entries::Wide(e) => (e.capacity() * size_of::<Wide>()) as u64,
        })
        .sum()
}

/// Rows `[r0, r1)` of each band of a layer with `ny` rows.
fn bands(ny: i64, count: usize) -> Vec<(i64, i64)> {
    let count = (count as i64).clamp(1, ny.max(1));
    (0..count)
        .map(|b| (b * ny / count, (b + 1) * ny / count))
        .collect()
}

/// Band count of each populated layer: about `BANDS_PER_THREAD` tasks per
/// pool thread, shared in proportion to the layers' shapes.
fn band_counts(layers: &[Option<LayerGrid>], items: &[u64]) -> Vec<usize> {
    let tasks = (rayon::current_num_threads() * BANDS_PER_THREAD) as u64;
    let total = items.iter().sum::<u64>().max(1);
    layers
        .iter()
        .zip(items)
        .map(|(g, &n)| {
            g.as_ref()
                .map_or(0, |_| (n * tasks).div_ceil(total).max(1) as usize)
        })
        .collect()
}

/// Counts the entries of the cells in rows `r0..r1` into `counts` (the
/// offsets `1 + r0 * nx..`), in item order. `None` once the band alone holds
/// more than `cap` entries (no cell count can then overflow).
fn count_band<'a, E: Entry + 'a>(
    g: &LayerGrid,
    items: impl Iterator<Item = &'a E>,
    (r0, r1): (i64, i64),
    counts: &mut [u32],
    cap: usize,
) -> Option<usize> {
    let nx = g.nx as usize;
    let mut total = 0usize;
    for e in items {
        let (rect, _) = e.get(g.x0, g.y0);
        let (x0, x1, y0, y1) = g.cells(rect);
        let (a, b) = (y0.max(r0), y1.min(r1 - 1));
        if a > b {
            continue;
        }
        total = total
            .checked_add(((x1 - x0 + 1) * (b - a + 1)) as usize)
            .filter(|&t| t <= cap)?;
        for y in a..=b {
            let row = (y - r0) as usize * nx;
            for x in x0..=x1 {
                counts[row + x as usize] += 1;
            }
        }
    }
    Some(total)
}

/// Places the items in the cells of rows `r0..r1` (`out` holds exactly
/// those cells' entries), in item order: each cell receives its entries in
/// ascending shape ID, as the serial fill does.
fn fill_band<'a, E: Entry + 'a>(
    g: &LayerGrid,
    items: impl Iterator<Item = &'a E>,
    (r0, r1): (i64, i64),
    out: &mut [E],
) {
    let nx = g.nx as usize;
    let cells = &g.offsets[r0 as usize * nx..r1 as usize * nx];
    let base = cells.first().copied().unwrap_or(0);
    let mut cursor: Vec<u32> = cells.iter().map(|&o| o - base).collect();
    for e in items {
        let (rect, _) = e.get(g.x0, g.y0);
        let (x0, x1, y0, y1) = g.cells(rect);
        let (a, b) = (y0.max(r0), y1.min(r1 - 1));
        for y in a..=b {
            let row = (y - r0) as usize * nx;
            for x in x0..=x1 {
                let at = &mut cursor[row + x as usize];
                out[*at as usize] = *e;
                *at += 1;
            }
        }
    }
}

/// One layer's entry table, written by row bands in parallel.
enum BandOut<'a> {
    Compact(&'a mut [Compact]),
    Wide(&'a mut [Wide]),
}

impl GridIndex {
    /// Exact heap bytes of the built index (offsets plus entries).
    pub(super) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(super) fn entries(&self) -> usize {
        self.entries
    }
    /// Whether virtual shapes are indexed (instance queries are then
    /// unnecessary on every routing layer).
    pub(super) fn has_virtual(&self) -> bool {
        !self.virtual_ids.is_empty()
    }
    #[inline]
    pub(super) fn is_virtual(&self, id: u32) -> bool {
        self.virtual_ids.contains(&id)
    }
    /// The conductor shape of an entry reported by `visit` on `layer`.
    #[inline]
    pub(super) fn shape(&self, shapes: &Shapes<'_>, id: u32, layer: u32, rect: Rect) -> Shape {
        if self.virtual_ids.contains(&id) {
            let k = (id - self.virtual_ids.start) as usize;
            let word = self.pin_bits[k / 64];
            let bit = 1u64 << (k % 64);
            let owner = if word & bit == 0 {
                PassiveOwner::Ground
            } else {
                let rank = self.pin_rank[k / 64] + (word & (bit - 1)).count_ones();
                let (net, pin) = self.pins[rank as usize];
                PassiveOwner::Pin { net, pin }
            };
            Shape {
                layer,
                rect,
                owner: Owner::Passive(owner),
            }
        } else {
            shapes.get(id)
        }
    }

    /// Builds one grid per populated layer. `base_cell[layer]` is the
    /// smallest cell side (DBU); populated layers beyond it use one DBU.
    /// Entries are capped by `limits.max_bin_entries`: when the base cells
    /// exceed the cap, every layer's cell is doubled until the entries fit
    /// (results never depend on the cell; the work count does), and only a
    /// grid of single cells over the cap fails. The exact table bytes must
    /// fit `allowance`. Every inserted entry, and every entry counted for a
    /// rejected cell size, is charged to `work`.
    pub(super) fn build(
        shapes: &Shapes<'_>,
        layer_count: usize,
        base_cell: &[i64],
        limits: &ExtractLimits,
        allowance: u64,
        work: &mut usize,
    ) -> Result<Self> {
        Self::build_with(
            shapes,
            None,
            layer_count,
            base_cell,
            true,
            limits,
            allowance,
            work,
        )
    }

    /// `build` that also indexes the virtual shapes on `virtual_layers`
    /// (with their owners). `None` when they outnumber the retained shapes
    /// (cell geometry then stays virtual and is queried per instance), do
    /// not fit the entry cap or the allowance, or a virtual rectangle is
    /// degenerate: the caller then builds the retained-only grid. Nothing
    /// is charged to `work` then.
    pub(super) fn build_virtual(
        shapes: &Shapes<'_>,
        virtual_layers: &[bool],
        layer_count: usize,
        base_cell: &[i64],
        limits: &ExtractLimits,
        allowance: u64,
        work: &mut usize,
    ) -> Option<Self> {
        if shapes.virtual_count() == 0 {
            return None;
        }
        // One pass over the virtual shapes, by instance parts in parallel:
        // validity, the indexed count and the signal-pin owners (a bit per
        // shape and `(net, pin)` per pin, in shape order).
        use rayon::prelude::*;
        let words = shapes.virtual_count().div_ceil(64);
        let (mut pin_bits, mut pin_rank, mut pins) = (Vec::new(), Vec::new(), Vec::new());
        pin_bits.try_reserve_exact(words).ok()?;
        pin_rank.try_reserve_exact(words).ok()?;
        pin_bits.resize(words, 0u64);
        let instances = shapes.virtual_instances();
        let offset = shapes.virtual_offset();
        type Scan = Option<(usize, Vec<(u32, (u32, u32))>)>;
        let scans: Vec<Scan> = (0..instances.div_ceil(PART_INSTANCES))
            .into_par_iter()
            .map(|part| {
                let start = part * PART_INSTANCES;
                let range = start..instances.min(start + PART_INSTANCES);
                let (mut indexed, mut found) = (0usize, Vec::new());
                for (id, s) in shapes.virtual_shapes_of(range) {
                    if virtual_layers
                        .get(s.layer as usize)
                        .copied()
                        .unwrap_or(false)
                    {
                        if !s.rect.valid() {
                            return None;
                        }
                        indexed += 1;
                    }
                    if let Owner::Passive(PassiveOwner::Pin { net, pin }) = s.owner {
                        found.try_reserve(1).ok()?;
                        found.push((id - offset as u32, (net, pin)));
                    }
                }
                Some((indexed, found))
            })
            .collect();
        let mut indexed_virtual = 0usize;
        let found = scans.iter().try_fold(0usize, |n, s| {
            s.as_ref().map(|(indexed, found)| {
                indexed_virtual += indexed;
                n + found.len()
            })
        })?;
        pins.try_reserve_exact(found).ok()?;
        for (_, part) in scans.into_iter().flatten() {
            for (k, owner) in part {
                let k = k as usize;
                pin_bits[k / 64] |= 1 << (k % 64);
                pins.push(owner);
            }
        }
        if indexed_virtual > shapes.retained_len() {
            return None;
        }
        pins.shrink_to_fit();
        let pin_count = pins.len();
        let owners = (words as u64)
            .saturating_mul(12)
            .saturating_add((pin_count as u64).saturating_mul(8));
        let mut attempt = *work;
        let mut grid = Self::build_with(
            shapes,
            Some(virtual_layers),
            layer_count,
            base_cell,
            false,
            limits,
            allowance.checked_sub(owners)?,
            &mut attempt,
        )
        .ok()?;
        let mut rank = 0u32;
        for w in &pin_bits {
            pin_rank.push(rank);
            rank += w.count_ones();
        }
        let start = shapes.virtual_offset() as u32;
        grid.virtual_ids = start..start + shapes.virtual_count() as u32;
        (grid.pin_bits, grid.pin_rank, grid.pins) = (pin_bits, pin_rank, pins);
        grid.bytes = grid.bytes.saturating_add(owners);
        *work = attempt;
        Some(grid)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one builder for the retained and the virtual grid"
    )]
    fn build_with(
        shapes: &Shapes<'_>,
        virtual_layers: Option<&[bool]>,
        layer_count: usize,
        base_cell: &[i64],
        coarsen: bool,
        limits: &ExtractLimits,
        allowance: u64,
        work: &mut usize,
    ) -> Result<Self> {
        // Bounds and counts per layer, by parts in parallel (minimum,
        // maximum and sum do not depend on the order).
        let parts = parts(shapes, virtual_layers);
        let per_part = part_bounds(shapes, virtual_layers, &parts, layer_count)
            .ok_or_else(|| invalid("invalid retained shape-index geometry"))?;
        let mut bounds: LayerBounds = vec![None; layer_count];
        for part in &per_part {
            for (item, b) in bounds.iter_mut().zip(part) {
                if let Some((r, n)) = *b {
                    *item = Some(item.map_or((r, n), |(a, m)| (union(a, r), m + n)));
                }
            }
        }
        let cap = limits.max_bin_entries.min(u32::MAX as usize - 1);
        let mut scale = 0u32;
        let plans = loop {
            let plans: Vec<Option<Plan>> = bounds
                .iter()
                .enumerate()
                .map(|(l, b)| {
                    let base = i128::from(base_cell.get(l).copied().unwrap_or(1).max(1));
                    let cell = (base << scale.min(64)).min(HUGE_SPAN) as i64;
                    b.map(|(r, n)| plan(r, n, cell))
                })
                .collect();
            if !coarsen || plans.iter().flatten().all(|p| p.nx * p.ny == 1) {
                break plans;
            }
            let counted = count_entries(shapes, virtual_layers, &plans, cap);
            if counted <= cap {
                break plans;
            }
            *work = work
                .checked_add(counted)
                .filter(|&n| n <= limits.max_work)
                .ok_or_else(|| budget("extraction grid-index work"))?;
            scale += 1;
        };
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
            layers.push(Some(LayerGrid::new(
                p.bounds.lx,
                p.bounds.ly,
                p.nx,
                p.ny,
                p.cell,
                offsets,
            )));
        }
        let compact: Vec<bool> = plans
            .iter()
            .map(|p| p.is_some_and(|p| p.compact()))
            .collect();
        // With more than one pool thread, every layer's shapes are first
        // copied as its entries (one pass over the shapes, by parts in
        // parallel) when they fit the allowance next to the offsets; the
        // count and the fill then run by row bands of each layer in
        // parallel. Results never depend on the path.
        let list_bytes: u64 = plans
            .iter()
            .zip(&bounds)
            .map(|(p, b)| match (p, b) {
                (Some(p), Some((_, n))) => n.saturating_mul(p.entry_bytes()),
                _ => 0,
            })
            .fold(0u64, u64::saturating_add);
        let lists = (rayon::current_num_threads() > 1
            && bytes.saturating_add(list_bytes) <= allowance)
            .then(|| {
                let items: Vec<u64> = bounds.iter().map(|b| b.map_or(0, |b| b.1)).collect();
                let band_rows: Vec<Vec<(i64, i64)>> = band_counts(&layers, &items)
                    .into_iter()
                    .zip(&layers)
                    .map(|(n, g)| g.as_ref().map_or_else(Vec::new, |g| bands(g.ny, n)))
                    .collect();
                let lists = part_lists(
                    shapes,
                    virtual_layers,
                    &parts,
                    &layers,
                    &compact,
                    &band_rows,
                    allowance - bytes,
                )?;
                Some((lists, band_rows))
            })
            .flatten();
        drop(per_part);
        if let Some((lists, band_rows)) = lists {
            return Self::build_banded(
                shapes,
                virtual_layers,
                layers,
                lists,
                &compact,
                &band_rows,
                bytes,
                limits,
                allowance,
                work,
            );
        }
        // Count pass: entries per cell, capped before any entry allocation.
        let mut total = 0usize;
        let mut per_layer = vec![0usize; layer_count];
        for (_, layer, rect) in indexed(shapes, virtual_layers) {
            let g = layers[layer as usize]
                .as_mut()
                .expect("planned populated layer");
            let (x0, x1, y0, y1) = g.cells(rect);
            let n = ((x1 - x0 + 1) as usize).saturating_mul((y1 - y0 + 1) as usize);
            total = total
                .checked_add(n)
                .filter(|&t| t <= limits.max_bin_entries && t < u32::MAX as usize)
                .ok_or_else(|| budget("extraction spatial-bin entries"))?;
            per_layer[layer as usize] += n;
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
        for g in layers.iter_mut().flatten() {
            for i in 1..g.offsets.len() {
                g.offsets[i] += g.offsets[i - 1];
            }
        }
        fill(shapes, virtual_layers, &mut layers, &compact, &per_layer)?;
        Ok(Self::finished(layers, total))
    }

    /// The index over its filled layers (the fill cursors are released).
    fn finished(layers: Vec<Option<LayerGrid>>, total: usize) -> Self {
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
        Self {
            layers,
            bytes: resident,
            entries: total,
            virtual_ids: 0..0,
            pin_bits: Vec::new(),
            pin_rank: Vec::new(),
            pins: Vec::new(),
        }
    }

    /// The count, prefix and fill passes of `build_with` from each layer's
    /// shape entries `lists` (by part, in ascending ID), by row bands in
    /// parallel. Every cell's count and entry order, the totals, the work
    /// charge and every budget outcome are those of the serial passes. The
    /// fill holds the lists next to the table only when both fit
    /// `allowance`; otherwise the lists are released and the serial fill
    /// reads the shapes again.
    #[expect(
        clippy::too_many_arguments,
        reason = "the state of build_with after planning"
    )]
    fn build_banded(
        shapes: &Shapes<'_>,
        virtual_layers: Option<&[bool]>,
        mut layers: Vec<Option<LayerGrid>>,
        lists: Vec<Vec<Vec<Entries>>>,
        compact: &[bool],
        band_rows: &[Vec<(i64, i64)>],
        mut bytes: u64,
        limits: &ExtractLimits,
        allowance: u64,
        work: &mut usize,
    ) -> Result<Self> {
        use rayon::prelude::*;
        let cap = limits.max_bin_entries.min(u32::MAX as usize - 1);
        // The copied lists hold a shape once per band it reaches.
        let list_bytes = list_bytes(lists.iter().flatten().flatten());
        let geometry: Vec<Option<LayerGrid>> = layers
            .iter()
            .map(|g| {
                g.as_ref()
                    .map(|g| LayerGrid::new(g.x0, g.y0, g.nx, g.ny, g.cell, Vec::new()))
            })
            .collect();
        // Count: each band owns its rows' offset slots.
        let mut tasks = Vec::new();
        for (l, g) in layers.iter_mut().enumerate() {
            let Some(g) = g.as_mut() else { continue };
            let nx = g.nx as usize;
            let mut rest = &mut g.offsets[1..];
            for (b, &(r0, r1)) in band_rows[l].iter().enumerate() {
                let (head, tail) = rest.split_at_mut((r1 - r0) as usize * nx);
                tasks.push((l, b, (r0, r1), head));
                rest = tail;
            }
        }
        let counted: Vec<Option<usize>> = tasks
            .into_par_iter()
            .map(|(l, b, rows, counts)| {
                let g = geometry[l].as_ref().expect("planned populated layer");
                if compact[l] {
                    let items = lists.iter().flat_map(|p| p[l][b].compact());
                    count_band(g, items, rows, counts, cap)
                } else {
                    let items = lists.iter().flat_map(|p| p[l][b].wide());
                    count_band(g, items, rows, counts, cap)
                }
            })
            .collect();
        drop(geometry);
        let mut total = 0usize;
        let mut per_layer = vec![0usize; layers.len()];
        let mut at = 0;
        for (l, rows) in band_rows.iter().enumerate() {
            for _ in rows {
                let n = counted[at].ok_or_else(|| budget("extraction spatial-bin entries"))?;
                at += 1;
                per_layer[l] += n;
                total = total
                    .checked_add(n)
                    .filter(|&t| t <= cap)
                    .ok_or_else(|| budget("extraction spatial-bin entries"))?;
            }
        }
        *work = work
            .checked_add(total)
            .filter(|&n| n <= limits.max_work)
            .ok_or_else(|| budget("extraction grid-index work"))?;
        for (l, g) in layers.iter().enumerate() {
            if g.is_some() {
                let entry = if compact[l] {
                    size_of::<Compact>()
                } else {
                    size_of::<Wide>()
                };
                bytes = bytes.saturating_add(per_layer[l] as u64 * entry as u64);
            }
        }
        if bytes > allowance {
            return Err(budget("extraction grid index memory"));
        }
        layers.par_iter_mut().flatten().for_each(|g| {
            for i in 1..g.offsets.len() {
                g.offsets[i] += g.offsets[i - 1];
            }
        });
        if bytes.saturating_add(list_bytes) > allowance {
            drop(lists);
            fill(shapes, virtual_layers, &mut layers, compact, &per_layer)?;
            return Ok(Self::finished(layers, total));
        }
        // Fill: each band owns its rows' entries, written in item order.
        let mut tables: Vec<Option<Entries>> = Vec::with_capacity(layers.len());
        for (l, g) in layers.iter().enumerate() {
            tables.push(match g {
                None => None,
                Some(g) => Some(if compact[l] {
                    Entries::Compact(blank_table(g, per_layer[l])?)
                } else {
                    Entries::Wide(blank_table(g, per_layer[l])?)
                }),
            });
        }
        let mut tasks = Vec::new();
        for (l, (g, table)) in layers.iter().zip(tables.iter_mut()).enumerate() {
            let (Some(g), Some(table)) = (g.as_ref(), table.as_mut()) else {
                continue;
            };
            let nx = g.nx as usize;
            let mut out = match table {
                Entries::Compact(e) => BandOut::Compact(&mut e[..]),
                Entries::Wide(e) => BandOut::Wide(&mut e[..]),
            };
            for (b, &(r0, r1)) in band_rows[l].iter().enumerate() {
                let len = (g.offsets[r1 as usize * nx] - g.offsets[r0 as usize * nx]) as usize;
                let (head, tail) = match out {
                    BandOut::Compact(e) => {
                        let (h, t) = e.split_at_mut(len);
                        (BandOut::Compact(h), BandOut::Compact(t))
                    }
                    BandOut::Wide(e) => {
                        let (h, t) = e.split_at_mut(len);
                        (BandOut::Wide(h), BandOut::Wide(t))
                    }
                };
                tasks.push((l, b, (r0, r1), head));
                out = tail;
            }
        }
        tasks.into_par_iter().for_each(|(l, b, rows, out)| {
            let g = layers[l].as_ref().expect("planned populated layer");
            match out {
                BandOut::Compact(out) => {
                    fill_band(g, lists.iter().flat_map(|p| p[l][b].compact()), rows, out)
                }
                BandOut::Wide(out) => {
                    fill_band(g, lists.iter().flat_map(|p| p[l][b].wide()), rows, out)
                }
            }
        });
        drop(lists);
        for (g, table) in layers.iter_mut().zip(tables) {
            if let (Some(g), Some(table)) = (g.as_mut(), table) {
                g.entries = table;
            }
        }
        Ok(Self::finished(layers, total))
    }

    /// The validation, extent check and work charge `visit` makes for
    /// `query` (scanned cells plus their entries), without scanning: the
    /// entries are summed per cell row from the CSR offsets. 0 when the
    /// layer has no grid.
    pub(super) fn query_work(
        &self,
        layer: u32,
        query: Rect,
        limits: &ExtractLimits,
    ) -> Result<usize> {
        if query.lx > query.ux || query.ly > query.uy {
            return Err(invalid("invalid shape-index query"));
        }
        let Some(g) = self.layers.get(layer as usize).and_then(Option::as_ref) else {
            return Ok(0);
        };
        let (qx0, qx1, qy0, qy1) = g.cells(query);
        let cells = (qx1 - qx0 + 1) as u64 * (qy1 - qy0 + 1) as u64;
        if cells > limits.max_bin_entries as u64 {
            return Err(budget("extraction query extent"));
        }
        let mut local = cells as usize;
        for y in qy0..=qy1 {
            let row = (y * g.nx) as usize;
            local += (g.offsets[row + qx1 as usize + 1] - g.offsets[row + qx0 as usize]) as usize;
        }
        Ok(local)
    }

    /// `visit` without the work charge: the caller charges `query_work` of
    /// a query covering `query`.
    #[inline]
    pub(super) fn visit_uncharged(
        &self,
        layer: u32,
        query: Rect,
        mut visitor: impl FnMut(u32, Rect) -> Result<bool>,
    ) -> Result<bool> {
        if query.lx > query.ux || query.ly > query.uy {
            return Err(invalid("invalid shape-index query"));
        }
        let Some(g) = self.layers.get(layer as usize).and_then(Option::as_ref) else {
            return Ok(true);
        };
        let range = g.cells(query);
        let mut local = 0usize;
        match &g.entries {
            Entries::Compact(e) => g.scan_compact(e, query, range, &mut local, &mut visitor),
            Entries::Wide(e) => g.scan(e, query, range, &mut local, &mut visitor),
        }
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
            Entries::Compact(e) => g.scan_compact(e, query, range, &mut local, &mut visitor),
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
    fn cell_columns_match_clamped_euclidean_division() {
        for (cell, n) in [
            (2i64, 1i64),
            (2, 9),
            (3, 7),
            (2160, 400),
            (2520, 1_704_000),
            (65_537, 65_000),
            (i64::from(u32::MAX), 1),
            (1, 50),
            (i64::MAX, 1),
            (1 << 40, 3),
        ] {
            let g = LayerGrid::new(-17, 4, n, n, cell, Vec::new());
            let last = n.saturating_mul(cell).saturating_sub(1);
            let mut values = vec![
                i64::MIN,
                -1,
                0,
                1,
                cell - 1,
                cell,
                cell + 1,
                last - 1,
                last,
                last.saturating_add(1),
                i64::from(u32::MAX),
                i64::MAX,
            ];
            let mut v = 7i64;
            for _ in 0..2000 {
                v = v.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                values.push(v.rem_euclid(last.saturating_add(cell).max(1)));
            }
            for v in values {
                let want = v.div_euclid(cell).clamp(0, n - 1);
                assert_eq!(g.axis(v, g.last_x, n), want, "cell {cell} n {n} v {v}");
            }
        }
    }

    #[test]
    fn query_work_and_uncharged_visits_match_charged_visits() {
        let mut rng = Lcg(3);
        let mut input = Vec::new();
        for i in 0..1200 {
            let (x, y) = (rng.next(300) * 7 - 500, rng.next(300) * 7 - 400);
            let (w, h) = match i % 5 {
                0 => (rng.next(3000) + 1, 3),
                1 => (3, rng.next(3000) + 1),
                _ => (rng.next(40) + 1, rng.next(40) + 1),
            };
            input.push((
                (i % 3) as u32,
                Rect {
                    lx: x,
                    ly: y,
                    ux: x + w,
                    uy: y + h,
                },
            ));
        }
        let shapes = shapes(input);
        let limits = ExtractLimits::default();
        for cell in [5, 64, 900] {
            let index = build(&shapes, 4, cell);
            for _ in 0..400 {
                let (x, y) = (rng.next(400) * 7 - 900, rng.next(400) * 7 - 800);
                let q = Rect {
                    lx: x,
                    ly: y,
                    ux: x + rng.next(700),
                    uy: y + rng.next(700),
                };
                for layer in 0..4 {
                    let (mut charged, mut seen) = (0usize, Vec::new());
                    index
                        .visit(layer, q, &limits, &mut charged, |id, r| {
                            seen.push((id, r));
                            Ok(true)
                        })
                        .unwrap();
                    assert_eq!(index.query_work(layer, q, &limits).unwrap(), charged);
                    let mut uncharged = Vec::new();
                    index
                        .visit_uncharged(layer, q, |id, r| {
                            uncharged.push((id, r));
                            Ok(true)
                        })
                        .unwrap();
                    assert_eq!(uncharged, seen);
                }
            }
            // Cell entries stay in ascending shape ID (one fill pass).
            for g in index.layers.iter().flatten() {
                let Entries::Compact(e) = &g.entries else {
                    panic!("compact layer")
                };
                for c in 0..g.offsets.len() - 1 {
                    let cell = &e[g.offsets[c] as usize..g.offsets[c + 1] as usize];
                    assert!(cell.windows(2).all(|w| w[0].id < w[1].id));
                }
            }
        }
        let bad = Rect {
            lx: 1,
            ly: 0,
            ux: 0,
            uy: 0,
        };
        let index = build(&shapes, 4, 64);
        assert!(index.query_work(0, bad, &limits).is_err());
        assert!(index.visit_uncharged(0, bad, |_, _| Ok(true)).is_err());
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

    /// A layer's offsets and entries (rectangle, ID) in table order.
    type Table = (Vec<u32>, Vec<(Rect, u32)>);
    /// Every layer's table.
    fn tables(index: &GridIndex) -> Vec<Option<Table>> {
        index
            .layers
            .iter()
            .map(|g| {
                g.as_ref().map(|g| {
                    let entries = match &g.entries {
                        Entries::Compact(e) => e.iter().map(|e| e.get(g.x0, g.y0)).collect(),
                        Entries::Wide(e) => e.iter().map(|e| e.get(g.x0, g.y0)).collect(),
                    };
                    (g.offsets.clone(), entries)
                })
            })
            .collect()
    }

    #[test]
    fn banded_parallel_build_matches_the_serial_passes_at_every_pool_size() {
        let mut rng = Lcg(11);
        let mut input = Vec::new();
        for i in 0..40_000 {
            // Layer 3 spans more than 32 bits (wide entries); layer 0 is
            // denser than the others (uneven band counts).
            let layer = [0, 0, 0, 1, 2, 3][i % 6];
            let scale = if layer == 3 { 1 << 22 } else { 5 };
            let (x, y) = (rng.next(2000) * scale - 3000, rng.next(2000) * 5 - 2000);
            let (w, h) = match i % 5 {
                0 => (rng.next(3000) + 1, 5),
                1 => (5, rng.next(3000) + 1),
                _ => (rng.next(40) + 1, rng.next(40) + 1),
            };
            input.push((
                layer as u32,
                Rect {
                    lx: x,
                    ly: y,
                    ux: x + w,
                    uy: y + h,
                },
            ));
        }
        let shapes = shapes(input);
        let limits = ExtractLimits::default();
        let run = |threads: usize, allowance: u64| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    let mut work = 0;
                    let index =
                        GridIndex::build(&shapes, 5, &[64; 5], &limits, allowance, &mut work)
                            .unwrap();
                    (tables(&index), index.entries(), index.bytes(), work)
                })
        };
        let serial = run(1, u64::MAX);
        assert!(serial.1 > 40_000);
        for threads in [2, 3, 4, 7] {
            assert!(run(threads, u64::MAX) == serial, "{threads} threads");
        }
        // The exact allowance of the serial passes (the table and the fill
        // cursors) leaves no room for the copied lists during the fill: the
        // serial fill runs, with the same outcome. One byte less fails
        // alike on every path.
        let offsets: u64 = serial
            .0
            .iter()
            .flatten()
            .map(|t| t.0.len() as u64 * 4)
            .sum();
        assert!(run(4, serial.2 + offsets) == serial);
        let fails = |threads: usize| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    let mut work = 0;
                    GridIndex::build(
                        &shapes,
                        5,
                        &[64; 5],
                        &limits,
                        serial.2 + offsets - 1,
                        &mut work,
                    )
                    .err()
                    .map(|e| e.to_string())
                })
        };
        assert!(fails(1).is_some());
        assert_eq!(fails(1), fails(4));
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
