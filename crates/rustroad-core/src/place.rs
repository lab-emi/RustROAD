//! Bounded analytic placement (`cpu_ref`, deterministic, single worker).
//!
//! * **Global placement.** Bound-to-bound quadratic wirelength over cell
//!   centres with the I/O pins as fixed anchors, solved per axis by
//!   Jacobi-preconditioned conjugate gradient in f64. After each solve the
//!   cells are spread by recursive area bisection over the free row intervals
//!   (capacity-proportional cuts, alternating axes); the spread positions
//!   become pseudo-net anchors of growing weight for the next solve.
//! * **Legalization.** Cells sorted by target x are packed greedily into the
//!   free intervals between reserved taps (one frontier per interval), each at
//!   the minimum-displacement legal site.
//! * **Detailed placement.** Bounded passes of HPWL-improving moves of a cell
//!   into the widest free gap near the median of its nets, and swaps of
//!   neighbouring cells of equal width.
//!
//! * **Net weights (timing-driven mode).** `PlaceProblem::weights` scales
//!   each net's springs and its detailed-placement HPWL cost by an integer
//!   weight in milli-units (1000 = 1.0, bounded by `MAX_NET_WEIGHT`); an
//!   empty vector is the unweighted placer, bit for bit.
//!
//! Inputs and outputs are integer DBU; only the solver works in f64. Memory is
//! O(cells + pins); work is counted against `PlaceLimits::max_work`.
//! See docs/scaling.md for the estimate.
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinRef {
    /// Movable cell index.
    Cell(u32),
    /// Fixed point (I/O pin centre), DBU.
    Fixed([i64; 2]),
}
/// One free row interval between reserved taps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub y: i64,
    pub lx: i64,
    pub ux: i64,
}
pub struct PlaceProblem {
    /// Cell widths (DBU); every cell is one row high.
    pub widths: Vec<i64>,
    pub row_height: i64,
    pub site: i64,
    /// Extra free space kept after every cell (cell spacing policy).
    pub gap: i64,
    pub nets: Vec<Vec<PinRef>>,
    /// Sorted by (y, lx); disjoint.
    pub intervals: Vec<Interval>,
    /// Per-net weight in milli-units (1000 = 1.0), one per `nets` entry, or
    /// empty for the unweighted placer. Each weight is within
    /// `1..=MAX_NET_WEIGHT`.
    pub weights: Vec<u32>,
}
/// Largest net weight (milli-units): 16x the default.
pub const MAX_NET_WEIGHT: u32 = 16_000;
impl PlaceProblem {
    /// Weight of net `k` in milli-units (1000 when unweighted).
    fn weight(&self, k: usize) -> u32 {
        self.weights.get(k).copied().unwrap_or(1000)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceConfig {
    #[serde(default = "default_global")]
    pub global_iterations: usize,
    #[serde(default = "default_cg")]
    pub cg_iterations: usize,
    #[serde(default = "default_detailed")]
    pub detailed_passes: usize,
    /// Nets with more pins are ignored by the wirelength model.
    #[serde(default = "default_max_net_pins")]
    pub max_net_pins: usize,
}
fn default_global() -> usize {
    12
}
fn default_cg() -> usize {
    150
}
fn default_detailed() -> usize {
    3
}
fn default_max_net_pins() -> usize {
    256
}
impl Default for PlaceConfig {
    fn default() -> Self {
        Self {
            global_iterations: default_global(),
            cg_iterations: default_cg(),
            detailed_passes: default_detailed(),
            max_net_pins: default_max_net_pins(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct PlaceLimits {
    pub max_cells: usize,
    pub max_pins: usize,
    pub max_work: u64,
}
impl Default for PlaceLimits {
    fn default() -> Self {
        Self {
            max_cells: 250_000,
            max_pins: 2_000_000,
            max_work: 20_000_000_000,
        }
    }
}
#[derive(Clone, Debug)]
pub struct PlaceResult {
    /// Lower-left corner of every cell (DBU).
    pub positions: Vec<[i64; 2]>,
    pub hpwl: u64,
    pub global_hpwl: u64,
    pub work: u64,
}

fn budget(s: &str) -> Error {
    Error::Budget(format!("placement: {s}"))
}
fn invalid(s: &str) -> Error {
    Error::Invalid(format!("placement: {s}"))
}

struct Work {
    used: u64,
    max: u64,
}
impl Work {
    fn add(&mut self, n: u64) -> Result<()> {
        self.used = self.used.saturating_add(n);
        if self.used > self.max {
            return Err(budget("work limit"));
        }
        Ok(())
    }
}

/// Centre-to-centre half-perimeter wirelength of lower-left positions.
pub fn hpwl(problem: &PlaceProblem, positions: &[[i64; 2]]) -> u64 {
    let mut total = 0u64;
    for net in &problem.nets {
        if net.len() < 2 {
            continue;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for p in net {
            let [x, y] = match *p {
                PinRef::Cell(c) => {
                    let c = c as usize;
                    [
                        positions[c][0] + problem.widths[c] / 2,
                        positions[c][1] + problem.row_height / 2,
                    ]
                }
                PinRef::Fixed(f) => f,
            };
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        total = total.saturating_add((x1 - x0) as u64 + (y1 - y0) as u64);
    }
    total
}

/// `hpwl` with every net multiplied by its milli-unit weight.
pub fn weighted_hpwl(problem: &PlaceProblem, positions: &[[i64; 2]]) -> u64 {
    let mut total = 0u64;
    for (k, net) in problem.nets.iter().enumerate() {
        if net.len() < 2 {
            continue;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for p in net {
            let [x, y] = match *p {
                PinRef::Cell(c) => {
                    let c = c as usize;
                    [
                        positions[c][0] + problem.widths[c] / 2,
                        positions[c][1] + problem.row_height / 2,
                    ]
                }
                PinRef::Fixed(f) => f,
            };
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let h = (x1 - x0) as u64 + (y1 - y0) as u64;
        total = total.saturating_add(h.saturating_mul(u64::from(problem.weight(k))));
    }
    total
}

/// Symmetric sparse system A x = b over movable cells (one axis).
struct System {
    diag: Vec<f64>,
    /// Off-diagonal entries (row, col, value) accumulated then compressed.
    off: Vec<(u32, u32, f64)>,
    b: Vec<f64>,
}
impl System {
    fn new(n: usize) -> Self {
        Self {
            diag: vec![0.0; n],
            off: Vec::new(),
            b: vec![0.0; n],
        }
    }
    /// Spring of weight w between two pins.
    fn spring(&mut self, a: Option<usize>, fa: f64, b: Option<usize>, fb: f64, w: f64) {
        match (a, b) {
            (Some(i), Some(j)) if i != j => {
                self.diag[i] += w;
                self.diag[j] += w;
                self.off.push((i as u32, j as u32, -w));
                self.off.push((j as u32, i as u32, -w));
            }
            (Some(_), Some(_)) => {}
            (Some(i), None) => {
                self.diag[i] += w;
                self.b[i] += w * fb;
            }
            (None, Some(j)) => {
                self.diag[j] += w;
                self.b[j] += w * fa;
            }
            (None, None) => {}
        }
    }
}
struct Csr {
    start: Vec<usize>,
    col: Vec<u32>,
    val: Vec<f64>,
}
fn compress(n: usize, mut off: Vec<(u32, u32, f64)>) -> Csr {
    off.sort_unstable_by_key(|e| (e.0, e.1));
    let mut start = vec![0usize; n + 1];
    let mut col = Vec::with_capacity(off.len());
    let mut val: Vec<f64> = Vec::with_capacity(off.len());
    let mut last: Option<(u32, u32)> = None;
    for (r, c, v) in off {
        if last == Some((r, c)) {
            *val.last_mut().unwrap() += v;
            continue;
        }
        last = Some((r, c));
        col.push(c);
        val.push(v);
        start[r as usize + 1] = col.len();
    }
    for i in 1..=n {
        start[i] = start[i].max(start[i - 1]);
    }
    Csr { start, col, val }
}

/// Jacobi-preconditioned conjugate gradient, fixed iteration cap.
fn solve(
    diag: &[f64],
    csr: &Csr,
    b: &[f64],
    x: &mut [f64],
    iterations: usize,
    work: &mut Work,
) -> Result<()> {
    let n = x.len();
    let mul = |v: &[f64], out: &mut [f64]| {
        for i in 0..n {
            let mut s = diag[i] * v[i];
            for k in csr.start[i]..csr.start[i + 1] {
                s += csr.val[k] * v[csr.col[k] as usize];
            }
            out[i] = s;
        }
    };
    let mut r = vec![0.0; n];
    let mut ap = vec![0.0; n];
    mul(x, &mut ap);
    for i in 0..n {
        r[i] = b[i] - ap[i];
    }
    let mut z: Vec<f64> = (0..n).map(|i| r[i] / diag[i]).collect();
    let mut p = z.clone();
    let mut rz: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
    let norm_b: f64 = b.iter().map(|v| v * v).sum::<f64>().sqrt().max(1.0);
    for _ in 0..iterations {
        work.add((n + csr.col.len()) as u64)?;
        let rn: f64 = r.iter().map(|v| v * v).sum::<f64>().sqrt();
        if rn <= 1e-9 * norm_b {
            break;
        }
        mul(&p, &mut ap);
        let pap: f64 = p.iter().zip(&ap).map(|(a, b)| a * b).sum();
        if pap <= 0.0 || !pap.is_finite() {
            break;
        }
        let alpha = rz / pap;
        for i in 0..n {
            x[i] += alpha * p[i];
            r[i] -= alpha * ap[i];
        }
        for i in 0..n {
            z[i] = r[i] / diag[i];
        }
        let rz_new: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
        let beta = rz_new / rz;
        rz = rz_new;
        for i in 0..n {
            p[i] = z[i] + beta * p[i];
        }
    }
    if x.iter().any(|v| !v.is_finite()) {
        return Err(invalid("non-finite solution"));
    }
    Ok(())
}

/// One bound-to-bound solve of both axes, with optional anchors.
fn quadratic(
    problem: &PlaceProblem,
    config: &PlaceConfig,
    pos: &mut [[f64; 2]],
    anchors: Option<(&[[f64; 2]], f64)>,
    region: [f64; 4],
    work: &mut Work,
) -> Result<()> {
    let n = pos.len();
    let half_w: Vec<f64> = problem.widths.iter().map(|w| *w as f64 / 2.0).collect();
    let half_h = problem.row_height as f64 / 2.0;
    let eps = problem.site.max(1) as f64;
    for axis in 0..2 {
        let mut sys = System::new(n);
        for (k, net) in problem.nets.iter().enumerate() {
            let p = net.len();
            if p < 2 || p > config.max_net_pins {
                continue;
            }
            work.add(p as u64)?;
            let net_weight = f64::from(problem.weight(k)) / 1000.0;
            let coord = |r: &PinRef| -> (Option<usize>, f64) {
                match *r {
                    PinRef::Cell(c) => (Some(c as usize), pos[c as usize][axis]),
                    PinRef::Fixed(f) => (None, f[axis] as f64),
                }
            };
            let pins: Vec<(Option<usize>, f64)> = net.iter().map(coord).collect();
            let (mut lo, mut hi) = (0usize, 0usize);
            for (i, q) in pins.iter().enumerate() {
                if q.1 < pins[lo].1 || (q.1 == pins[lo].1 && i < lo) {
                    lo = i;
                }
                if q.1 > pins[hi].1 {
                    hi = i;
                }
            }
            if lo == hi {
                hi = if lo == 0 { 1 } else { 0 };
            }
            let scale = net_weight * 2.0 / (p as f64 - 1.0);
            let mut link = |i: usize, j: usize| {
                let (a, b) = (pins[i], pins[j]);
                let w = scale / (a.1 - b.1).abs().max(eps);
                sys.spring(a.0, a.1, b.0, b.1, w);
            };
            link(lo, hi);
            for k in 0..p {
                if k != lo && k != hi {
                    link(k, lo);
                    link(k, hi);
                }
            }
        }
        // Pull towards the region centre keeps the system positive definite.
        let centre = if axis == 0 {
            (region[0] + region[2]) / 2.0
        } else {
            (region[1] + region[3]) / 2.0
        };
        for i in 0..n {
            let w = 1e-6 / eps;
            sys.diag[i] += w;
            sys.b[i] += w * centre;
            if let Some((a, alpha)) = anchors {
                let d = (pos[i][axis] - a[i][axis]).abs().max(eps);
                let w = alpha / d;
                sys.diag[i] += w;
                sys.b[i] += w * a[i][axis];
            }
        }
        let csr = compress(n, std::mem::take(&mut sys.off));
        let mut x: Vec<f64> = pos.iter().map(|p| p[axis]).collect();
        solve(&sys.diag, &csr, &sys.b, &mut x, config.cg_iterations, work)?;
        let (lo, hi) = if axis == 0 {
            (region[0], region[2])
        } else {
            (region[1], region[3])
        };
        for i in 0..n {
            let h = if axis == 0 { half_w[i] } else { half_h };
            pos[i][axis] = x[i].clamp(lo + h, (hi - h).max(lo + h));
        }
    }
    Ok(())
}

/// Recursive capacity-proportional bisection: target centres for every cell.
struct Spread<'a> {
    problem: &'a PlaceProblem,
    rows: Vec<(i64, Vec<(i64, i64)>)>,
    out: Vec<[f64; 2]>,
}
impl Spread<'_> {
    fn capacity(&self, r0: usize, r1: usize, x0: i64, x1: i64) -> i64 {
        let mut c = 0i64;
        for (_, ivs) in &self.rows[r0..r1] {
            for &(a, b) in ivs {
                let (a, b) = (a.max(x0), b.min(x1));
                if b > a {
                    c += b - a;
                }
            }
        }
        c
    }
    fn width(&self, c: usize) -> i64 {
        self.problem.widths[c] + self.problem.gap
    }
    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        cells: &mut [usize],
        pos: &[[f64; 2]],
        r0: usize,
        r1: usize,
        x0: i64,
        x1: i64,
        work: &mut Work,
    ) -> Result<()> {
        if cells.is_empty() {
            return Ok(());
        }
        work.add(cells.len() as u64)?;
        let height = (r1 - r0) as i64 * self.problem.row_height;
        let wide = x1 - x0;
        if r1 - r0 == 1 && (cells.len() <= 1 || wide <= 4 * self.problem.row_height) {
            return self.leaf(cells, pos, r0, x0, x1);
        }
        let vertical_cut = (wide > height && wide > 2 * self.problem.site) || r1 - r0 == 1;
        let (a_cap, b_cap, cut_x, cut_r);
        if vertical_cut {
            let mid = x0 + (wide / 2 / self.problem.site) * self.problem.site;
            let mid = if mid <= x0 {
                x0 + self.problem.site
            } else {
                mid
            };
            a_cap = self.capacity(r0, r1, x0, mid);
            b_cap = self.capacity(r0, r1, mid, x1);
            cut_x = mid;
            cut_r = r0;
            cells.sort_by(|a, b| pos[*a][0].total_cmp(&pos[*b][0]).then(a.cmp(b)));
        } else {
            let mid = r0 + (r1 - r0) / 2;
            a_cap = self.capacity(r0, mid, x0, x1);
            b_cap = self.capacity(mid, r1, x0, x1);
            cut_x = x0;
            cut_r = mid;
            cells.sort_by(|a, b| pos[*a][1].total_cmp(&pos[*b][1]).then(a.cmp(b)));
        }
        let total: i64 = cells.iter().map(|c| self.width(*c)).sum();
        let cap = a_cap + b_cap;
        if cap <= 0 {
            return Err(Error::Capacity(
                "analytic spreading region without capacity".into(),
            ));
        }
        // Left share proportional to capacity, never above the left capacity.
        let target = (i128::from(total) * i128::from(a_cap) / i128::from(cap)) as i64;
        let mut k = 0usize;
        let mut acc = 0i64;
        while k < cells.len() {
            let w = self.width(cells[k]);
            if acc + w / 2 > target || acc + w > a_cap {
                break;
            }
            acc += w;
            k += 1;
        }
        // The right side must hold the rest.
        let mut rest = total - acc;
        while rest > b_cap && k < cells.len() {
            let w = self.width(cells[k]);
            if acc + w > a_cap {
                break;
            }
            acc += w;
            rest -= w;
            k += 1;
        }
        let (left, right) = cells.split_at_mut(k);
        if vertical_cut {
            self.run(left, pos, r0, r1, x0, cut_x, work)?;
            self.run(right, pos, r0, r1, cut_x, x1, work)
        } else {
            self.run(left, pos, r0, cut_r, x0, x1, work)?;
            self.run(right, pos, cut_r, r1, x0, x1, work)
        }
    }
    /// Spread cells of one row segment over its free intervals in x order,
    /// whitespace shared evenly.
    fn leaf(
        &mut self,
        cells: &mut [usize],
        pos: &[[f64; 2]],
        r: usize,
        x0: i64,
        x1: i64,
    ) -> Result<()> {
        cells.sort_by(|a, b| pos[*a][0].total_cmp(&pos[*b][0]).then(a.cmp(b)));
        let ivs: Vec<(i64, i64)> = self.rows[r]
            .1
            .iter()
            .map(|&(a, b)| (a.max(x0), b.min(x1)))
            .filter(|(a, b)| b > a)
            .collect();
        let cap: i64 = ivs.iter().map(|(a, b)| b - a).sum();
        let need: i64 = cells.iter().map(|c| self.width(*c)).sum();
        let y = self.rows[r].0 as f64 + self.problem.row_height as f64 / 2.0;
        let slack = ((cap - need).max(0) as f64) / cells.len() as f64;
        let mut iv = 0usize;
        let mut x = ivs.first().map_or(x0, |v| v.0) as f64;
        for &c in cells.iter() {
            let w = self.width(c) as f64;
            x += slack / 2.0;
            while iv < ivs.len() && x + w > ivs[iv].1 as f64 && iv + 1 < ivs.len() {
                iv += 1;
                x = (ivs[iv].0 as f64).max(x.min(ivs[iv].0 as f64));
            }
            self.out[c] = [x + self.problem.widths[c] as f64 / 2.0, y];
            x += w + slack / 2.0;
        }
        Ok(())
    }
}

/// Legalization in two steps. (1) Assignment: cells sorted by target x pick
/// the interval of least estimated displacement that still has room for
/// their width (capacity counts widths plus spacing, so every interval's
/// cells always fit). (2) Per interval, an order-preserving minimum
/// displacement solve: cells keep their target order and overlapping
/// neighbours merge into clusters placed at their mean target, clamped to
/// the interval and snapped to the site grid.
fn legalize(problem: &PlaceProblem, target: &[[f64; 2]], work: &mut Work) -> Result<Vec<[i64; 2]>> {
    let n = target.len();
    let ivs = &problem.intervals;
    let mut rows: Vec<i64> = ivs.iter().map(|v| v.y).collect();
    rows.dedup();
    let mut by_row: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    for (i, v) in ivs.iter().enumerate() {
        let r = rows.binary_search(&v.y).unwrap();
        by_row[r].push(i);
    }
    let gap = problem.gap;
    let mut used: Vec<i64> = vec![0; ivs.len()];
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); ivs.len()];
    let lx_of = |c: usize| target[c][0] - problem.widths[c] as f64 / 2.0;
    let mut order: Vec<usize> = (0..n).collect();
    // Wide cells first so they find room; then by target x.
    order.sort_by(|a, b| {
        problem.widths[*b]
            .cmp(&problem.widths[*a])
            .then(lx_of(*a).total_cmp(&lx_of(*b)))
            .then(a.cmp(b))
    });
    for c in order {
        let w = problem.widths[c];
        let tx = lx_of(c);
        let ty = target[c][1] - problem.row_height as f64 / 2.0;
        let tr = rows
            .partition_point(|&y| (y as f64) < ty)
            .min(rows.len() - 1);
        let mut best: Option<(f64, usize)> = None;
        let mut radius = 2usize;
        loop {
            let lo = tr.saturating_sub(radius);
            let hi = (tr + radius + 1).min(rows.len());
            for r in lo..hi {
                let dy = (rows[r] as f64 - ty).abs();
                if best.is_some_and(|b| dy >= b.0) {
                    continue;
                }
                for &i in &by_row[r] {
                    work.add(1)?;
                    let v = ivs[i];
                    let need = if members[i].is_empty() { w } else { w + gap };
                    if used[i] + need > v.ux - v.lx {
                        continue;
                    }
                    // Displacement estimate, plus a crowding term so cells
                    // spread over equally close intervals.
                    let x = tx.clamp(v.lx as f64, (v.ux - w) as f64);
                    let fill = (used[i] + need) as f64 / (v.ux - v.lx) as f64;
                    let cost = (x - tx).abs() + dy + fill * problem.row_height as f64;
                    if best.is_none_or(|b| cost < b.0) {
                        best = Some((cost, i));
                    }
                }
            }
            if best.is_some() || (lo == 0 && hi == rows.len()) {
                break;
            }
            radius *= 2;
        }
        let (_, i) = best.ok_or_else(|| {
            let free: i64 = ivs.iter().zip(&used).map(|(v, u)| v.ux - v.lx - u).sum();
            Error::Capacity(format!(
                "cell {c} (width {w}): no interval has room during legalization (free {free})"
            ))
        })?;
        used[i] += if members[i].is_empty() { w } else { w + gap };
        members[i].push(c);
    }
    let mut out = vec![[0i64; 2]; n];
    let site = problem.site;
    for (i, cells) in members.iter_mut().enumerate() {
        if cells.is_empty() {
            continue;
        }
        let v = ivs[i];
        cells.sort_by(|a, b| lx_of(*a).total_cmp(&lx_of(*b)).then(a.cmp(b)));
        work.add(cells.len() as u64 * 2)?;
        // Clusters: (first index, count, total width incl. inner gaps, sum of
        // (target - offset within cluster)).
        let mut clusters: Vec<(usize, usize, i64, f64)> = Vec::new();
        for (k, &c) in cells.iter().enumerate() {
            let w = problem.widths[c];
            clusters.push((k, 1, w, lx_of(c)));
            loop {
                let len = clusters.len();
                let place = |cl: &(usize, usize, i64, f64)| -> i64 {
                    let want = cl.3 / cl.1 as f64;
                    let x = want.clamp(v.lx as f64, (v.ux - cl.2) as f64);
                    let snapped = v.lx + ((x - v.lx as f64) / site as f64).round() as i64 * site;
                    snapped.clamp(v.lx, v.ux - cl.2)
                };
                if len < 2 {
                    break;
                }
                let (a, b) = (clusters[len - 2], clusters[len - 1]);
                if place(&a) + a.2 + gap <= place(&b) {
                    break;
                }
                // Merge: b's cells shift by a's width + gap inside the cluster.
                let shift = (a.2 + gap) as f64;
                let merged = (
                    a.0,
                    a.1 + b.1,
                    a.2 + gap + b.2,
                    a.3 + b.3 - shift * b.1 as f64,
                );
                clusters.pop();
                clusters.pop();
                clusters.push(merged);
            }
        }
        for cl in &clusters {
            let want = cl.3 / cl.1 as f64;
            let x = want.clamp(v.lx as f64, (v.ux - cl.2) as f64);
            let mut x = (v.lx + ((x - v.lx as f64) / site as f64).round() as i64 * site)
                .clamp(v.lx, v.ux - cl.2);
            for &c in &cells[cl.0..cl.0 + cl.1] {
                out[c] = [x, v.y];
                x += problem.widths[c] + gap;
            }
        }
    }
    Ok(out)
}

/// Bounded HPWL-driven detailed placement on legal positions.
fn detailed(
    problem: &PlaceProblem,
    pos: &mut [[i64; 2]],
    passes: usize,
    work: &mut Work,
) -> Result<()> {
    let n = pos.len();
    let mut nets_of: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (k, net) in problem.nets.iter().enumerate() {
        for p in net {
            if let PinRef::Cell(c) = p {
                let v = &mut nets_of[*c as usize];
                if v.last() != Some(&(k as u32)) {
                    v.push(k as u32);
                }
            }
        }
    }
    let net_hpwl = |pos: &[[i64; 2]], k: u32| -> u64 {
        let (mut x0, mut x1, mut y0, mut y1) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for p in &problem.nets[k as usize] {
            let [x, y] = match *p {
                PinRef::Cell(c) => [
                    pos[c as usize][0] + problem.widths[c as usize] / 2,
                    pos[c as usize][1] + problem.row_height / 2,
                ],
                PinRef::Fixed(f) => f,
            };
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        (x1 - x0) as u64 + (y1 - y0) as u64
    };
    // Weighted (milli-unit) HPWL; with no weights every net counts 1000x,
    // which orders moves exactly like the unweighted sum.
    let cost = |pos: &[[i64; 2]], cells: &[usize]| -> u64 {
        let mut ks: Vec<u32> = cells
            .iter()
            .flat_map(|c| nets_of[*c].iter().copied())
            .collect();
        ks.sort_unstable();
        ks.dedup();
        ks.iter()
            .map(|k| net_hpwl(pos, *k).saturating_mul(u64::from(problem.weight(*k as usize))))
            .fold(0u64, u64::saturating_add)
    };
    let gap = problem.gap;
    for _ in 0..passes {
        let mut improved = false;
        // Occupancy per interval: sorted (x, width, cell).
        let mut occ: Vec<Vec<(i64, i64, usize)>> = vec![Vec::new(); problem.intervals.len()];
        let find = |p: [i64; 2]| -> Option<usize> {
            let i = problem
                .intervals
                .partition_point(|v| (v.y, v.lx) <= (p[1], p[0]));
            (i > 0 && problem.intervals[i - 1].y == p[1] && problem.intervals[i - 1].ux > p[0])
                .then(|| i - 1)
        };
        for (c, p) in pos.iter().enumerate() {
            let i = find(*p).ok_or_else(|| invalid("legal cell outside intervals"))?;
            occ[i].push((p[0], problem.widths[c], c));
        }
        for v in &mut occ {
            v.sort_unstable();
        }
        // 1. Swap neighbouring cells of equal width.
        for v in occ.iter_mut() {
            for j in 1..v.len() {
                work.add(4)?;
                let (a, b) = (v[j - 1].2, v[j].2);
                if problem.widths[a] != problem.widths[b] {
                    continue;
                }
                let before = cost(pos, &[a, b]);
                let (pa, pb) = (pos[a], pos[b]);
                pos[a] = pb;
                pos[b] = pa;
                if cost(pos, &[a, b]) < before {
                    v[j - 1].2 = b;
                    v[j].2 = a;
                    improved = true;
                } else {
                    pos[a] = pa;
                    pos[b] = pb;
                }
            }
        }
        // 2. Move a cell to a free gap near the median of its nets.
        for c in 0..n {
            work.add(nets_of[c].len() as u64 + 1)?;
            let mut xs = Vec::new();
            let mut ys = Vec::new();
            for &k in &nets_of[c] {
                let (mut x0, mut x1, mut y0, mut y1) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
                for p in &problem.nets[k as usize] {
                    let [x, y] = match *p {
                        PinRef::Cell(o) if o as usize == c => continue,
                        PinRef::Cell(o) => [
                            pos[o as usize][0] + problem.widths[o as usize] / 2,
                            pos[o as usize][1] + problem.row_height / 2,
                        ],
                        PinRef::Fixed(f) => f,
                    };
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
                if x0 <= x1 {
                    xs.extend([x0, x1]);
                    ys.extend([y0, y1]);
                }
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_unstable();
            ys.sort_unstable();
            let (mx, my) = (xs[xs.len() / 2], ys[ys.len() / 2]);
            let w = problem.widths[c];
            let cur = find(pos[c]).unwrap();
            // Candidate gaps in rows near my.
            let mut best: Option<(u64, usize, i64)> = None;
            let before = cost(pos, &[c]);
            for (i, v) in problem.intervals.iter().enumerate() {
                if (v.y + problem.row_height / 2 - my).abs() > 2 * problem.row_height
                    || v.ux < mx - 8 * problem.row_height
                    || v.lx > mx + 8 * problem.row_height
                {
                    continue;
                }
                work.add(occ[i].len() as u64 + 1)?;
                let mut left = v.lx;
                let items = &occ[i];
                for k in 0..=items.len() {
                    let right = if k < items.len() { items[k].0 } else { v.ux };
                    // Free gap [left, right) (own cell counts as free).
                    let (gl, gr) = (left, right);
                    if k < items.len() && items[k].2 == c {
                        left = left.max(items[k].0); // keep scanning past self
                        continue;
                    }
                    let room_end = if k < items.len() { gr - gap } else { gr };
                    if room_end - gl >= w {
                        let want = mx - w / 2;
                        let x = gl
                            + (((want - gl).clamp(0, room_end - w - gl)) / problem.site)
                                * problem.site;
                        if x >= gl && x + w <= room_end {
                            let old = pos[c];
                            pos[c] = [x, v.y];
                            let after = cost(pos, &[c]);
                            pos[c] = old;
                            if after < before && best.is_none_or(|b| after < b.0) {
                                best = Some((after, i, x));
                            }
                        }
                    }
                    if k < items.len() {
                        left = items[k].0 + items[k].1 + gap;
                    }
                }
            }
            if let Some((_, i, x)) = best {
                occ[cur].retain(|e| e.2 != c);
                pos[c] = [x, problem.intervals[i].y];
                let at = occ[i].partition_point(|e| e.0 < x);
                occ[i].insert(at, (x, w, c));
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    Ok(())
}

/// Validate the legal result independently of the algorithm.
fn check_legal(problem: &PlaceProblem, pos: &[[i64; 2]]) -> Result<()> {
    let mut items: Vec<(i64, i64, i64)> = pos
        .iter()
        .enumerate()
        .map(|(c, p)| (p[1], p[0], p[0] + problem.widths[c]))
        .collect();
    items.sort_unstable();
    for w in items.windows(2) {
        if w[0].0 == w[1].0 && w[0].2 + problem.gap > w[1].1 && w[0].2 > w[1].1 - problem.gap {
            return Err(invalid("overlap or spacing violation after placement"));
        }
    }
    for (c, p) in pos.iter().enumerate() {
        let i = problem
            .intervals
            .partition_point(|v| (v.y, v.lx) <= (p[1], p[0]));
        if i == 0 {
            return Err(invalid("cell outside free intervals"));
        }
        let v = problem.intervals[i - 1];
        if v.y != p[1]
            || p[0] < v.lx
            || p[0] + problem.widths[c] > v.ux
            || (p[0] - v.lx) % problem.site != 0
        {
            return Err(invalid("cell outside free intervals"));
        }
    }
    Ok(())
}

fn validate(problem: &PlaceProblem, config: &PlaceConfig, limits: &PlaceLimits) -> Result<()> {
    let n = problem.widths.len();
    if n > limits.max_cells || problem.nets.iter().map(Vec::len).sum::<usize>() > limits.max_pins {
        return Err(budget("problem size"));
    }
    if problem.row_height <= 0
        || problem.site <= 0
        || problem.gap < 0
        || problem.intervals.is_empty()
        || problem
            .widths
            .iter()
            .any(|w| *w <= 0 || w % problem.site != 0)
        || problem.intervals.windows(2).any(|w| {
            (w[0].y, w[0].lx) >= (w[1].y, w[1].lx) || (w[0].y == w[1].y && w[0].ux > w[1].lx)
        })
        || problem.intervals.iter().any(|v| v.ux <= v.lx)
        || (!problem.weights.is_empty() && problem.weights.len() != problem.nets.len())
        || problem
            .weights
            .iter()
            .any(|w| *w == 0 || *w > MAX_NET_WEIGHT)
        || config.global_iterations > 100
        || config.cg_iterations > 10_000
        || config.detailed_passes > 100
    {
        return Err(invalid("problem/config"));
    }
    for net in &problem.nets {
        for p in net {
            if let PinRef::Cell(c) = p
                && *c as usize >= n
            {
                return Err(invalid("net pin index"));
            }
        }
    }
    Ok(())
}

/// Passes that propagate targets to unseeded cells through their nets.
pub const SEED_PASSES: usize = 4;

/// ECO placement from a seed (e.g. the previous layout of the same netlist
/// before a small netlist edit): every seeded cell targets its seed (lower
/// left, DBU); a cell without one targets the mean centre of its seeded,
/// already targeted or fixed net neighbours (index order, `SEED_PASSES`
/// passes, else the region centre). No global placement runs: the targets
/// go straight to the same legalizer and detailed placement as `place`, so
/// unchanged cells move only by the legalization displacement the new cells
/// cause. Deterministic; work is counted against `PlaceLimits::max_work`.
pub fn place_seeded(
    problem: &PlaceProblem,
    seeds: &[Option<[i64; 2]>],
    config: &PlaceConfig,
    limits: &PlaceLimits,
) -> Result<PlaceResult> {
    validate(problem, config, limits)?;
    let n = problem.widths.len();
    if seeds.len() != n {
        return Err(invalid("seed count differs from the cell count"));
    }
    let mut work = Work {
        used: 0,
        max: limits.max_work,
    };
    if n == 0 {
        return Ok(PlaceResult {
            positions: Vec::new(),
            hpwl: 0,
            global_hpwl: 0,
            work: 0,
        });
    }
    let region = [
        problem.intervals.iter().map(|v| v.lx).min().unwrap() as f64,
        problem.intervals.iter().map(|v| v.y).min().unwrap() as f64,
        problem.intervals.iter().map(|v| v.ux).max().unwrap() as f64,
        (problem.intervals.iter().map(|v| v.y).max().unwrap() + problem.row_height) as f64,
    ];
    let half_h = problem.row_height as f64 / 2.0;
    let mut target: Vec<Option<[f64; 2]>> = seeds
        .iter()
        .enumerate()
        .map(|(c, s)| {
            s.map(|p| {
                [
                    p[0] as f64 + problem.widths[c] as f64 / 2.0,
                    p[1] as f64 + half_h,
                ]
            })
        })
        .collect();
    let mut nets_of: Vec<Vec<u32>> = vec![Vec::new(); n];
    for (k, net) in problem.nets.iter().enumerate() {
        for p in net {
            if let PinRef::Cell(c) = p {
                nets_of[*c as usize].push(k as u32);
            }
        }
    }
    for _ in 0..SEED_PASSES {
        let mut changed = false;
        for c in 0..n {
            if target[c].is_some() {
                continue;
            }
            let (mut sx, mut sy, mut m) = (0.0, 0.0, 0usize);
            for &k in &nets_of[c] {
                let net = &problem.nets[k as usize];
                work.add(net.len() as u64)?;
                for p in net {
                    let q = match *p {
                        PinRef::Cell(o) if o as usize == c => None,
                        PinRef::Cell(o) => target[o as usize],
                        PinRef::Fixed(f) => Some([f[0] as f64, f[1] as f64]),
                    };
                    if let Some(q) = q {
                        sx += q[0];
                        sy += q[1];
                        m += 1;
                    }
                }
            }
            if m > 0 {
                target[c] = Some([sx / m as f64, sy / m as f64]);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let centre = [(region[0] + region[2]) / 2.0, (region[1] + region[3]) / 2.0];
    let targets: Vec<[f64; 2]> = target.iter().map(|t| t.unwrap_or(centre)).collect();
    let global: Vec<[i64; 2]> = targets
        .iter()
        .enumerate()
        .map(|(c, t)| {
            [
                (t[0] - problem.widths[c] as f64 / 2.0).round() as i64,
                (t[1] - half_h).round() as i64,
            ]
        })
        .collect();
    let global_hpwl = hpwl(problem, &global);
    let mut legal = legalize(problem, &targets, &mut work)?;
    detailed(problem, &mut legal, config.detailed_passes, &mut work)?;
    check_legal(problem, &legal)?;
    Ok(PlaceResult {
        hpwl: hpwl(problem, &legal),
        positions: legal,
        global_hpwl,
        work: work.used,
    })
}

pub fn place(
    problem: &PlaceProblem,
    config: &PlaceConfig,
    limits: &PlaceLimits,
) -> Result<PlaceResult> {
    validate(problem, config, limits)?;
    let n = problem.widths.len();
    let mut work = Work {
        used: 0,
        max: limits.max_work,
    };
    if n == 0 {
        return Ok(PlaceResult {
            positions: Vec::new(),
            hpwl: 0,
            global_hpwl: 0,
            work: 0,
        });
    }
    let region = [
        problem.intervals.iter().map(|v| v.lx).min().unwrap() as f64,
        problem.intervals.iter().map(|v| v.y).min().unwrap() as f64,
        problem.intervals.iter().map(|v| v.ux).max().unwrap() as f64,
        (problem.intervals.iter().map(|v| v.y).max().unwrap() + problem.row_height) as f64,
    ];
    let centre = [(region[0] + region[2]) / 2.0, (region[1] + region[3]) / 2.0];
    // Deterministic small initial jitter by index breaks the all-coincident start.
    let mut pos: Vec<[f64; 2]> = (0..n)
        .map(|i| {
            let k = (i as f64 * 0.618_033_988_75).fract() - 0.5;
            let l = (i as f64 * 0.414_213_562_37).fract() - 0.5;
            [
                centre[0] + k * problem.site as f64,
                centre[1] + l * problem.site as f64,
            ]
        })
        .collect();
    let mut rows: Vec<(i64, Vec<(i64, i64)>)> = Vec::new();
    for v in &problem.intervals {
        match rows.last_mut() {
            Some(r) if r.0 == v.y => r.1.push((v.lx, v.ux)),
            _ => rows.push((v.y, vec![(v.lx, v.ux)])),
        }
    }
    let x_lo = region[0] as i64;
    let x_hi = region[2] as i64;
    for _ in 0..3 {
        quadratic(problem, config, &mut pos, None, region, &mut work)?;
    }
    let mut spread = Spread {
        problem,
        rows,
        out: vec![[0.0; 2]; n],
    };
    let mut targets = pos.clone();
    for k in 0..config.global_iterations {
        let mut cells: Vec<usize> = (0..n).collect();
        let nrows = spread.rows.len();
        spread.run(&mut cells, &pos, 0, nrows, x_lo, x_hi, &mut work)?;
        targets.clone_from(&spread.out);
        if k + 1 == config.global_iterations {
            break;
        }
        let alpha = 0.02 * (k as f64 + 1.0).powi(2);
        quadratic(
            problem,
            config,
            &mut pos,
            Some((&targets, alpha)),
            region,
            &mut work,
        )?;
    }
    let global: Vec<[i64; 2]> = targets
        .iter()
        .enumerate()
        .map(|(c, t)| {
            [
                (t[0] - problem.widths[c] as f64 / 2.0).round() as i64,
                (t[1] - problem.row_height as f64 / 2.0).round() as i64,
            ]
        })
        .collect();
    let global_hpwl = hpwl(problem, &global);
    let mut legal = legalize(problem, &targets, &mut work)?;
    detailed(problem, &mut legal, config.detailed_passes, &mut work)?;
    check_legal(problem, &legal)?;
    Ok(PlaceResult {
        hpwl: hpwl(problem, &legal),
        positions: legal,
        global_hpwl,
        work: work.used,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chain(n: usize, rows: usize, width: i64) -> PlaceProblem {
        let mut nets = vec![vec![PinRef::Fixed([0, 50]), PinRef::Cell(0)]];
        for i in 1..n {
            nets.push(vec![PinRef::Cell(i as u32 - 1), PinRef::Cell(i as u32)]);
        }
        nets.push(vec![PinRef::Cell(n as u32 - 1), PinRef::Fixed([width, 50])]);
        PlaceProblem {
            weights: Vec::new(),
            widths: vec![20; n],
            row_height: 100,
            site: 10,
            gap: 0,
            nets,
            intervals: (0..rows)
                .map(|r| Interval {
                    y: r as i64 * 100,
                    lx: 0,
                    ux: width,
                })
                .collect(),
        }
    }
    #[test]
    fn chain_is_legal_deterministic_and_short() {
        let p = chain(40, 4, 400);
        let a = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        let b = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        assert_eq!(a.positions, b.positions);
        check_legal(&p, &a.positions).unwrap();
        // Fixed-order packing in netlist order is optimal for a chain only up
        // to the row turn; the analytic result must not be worse than 2x it.
        let mut packed = Vec::new();
        for i in 0..40i64 {
            packed.push([(i % 20) * 20, (i / 20) * 100]);
        }
        assert!(
            a.hpwl <= 2 * hpwl(&p, &packed),
            "{} vs {}",
            a.hpwl,
            hpwl(&p, &packed)
        );
    }
    #[test]
    fn unit_weights_are_bit_identical_and_a_heavy_net_gets_shorter() {
        // Two fixed anchors far apart pull a 2-cell chain; a third net ties
        // cell 0 to a distant fixed pin.
        let mut p = chain(12, 3, 400);
        p.nets
            .push(vec![PinRef::Cell(0), PinRef::Fixed([400, 250])]);
        let base = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        p.weights = vec![1000; p.nets.len()];
        let unit = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        assert_eq!(base.positions, unit.positions);
        assert_eq!(base.hpwl, unit.hpwl);
        let k = p.nets.len() - 1;
        p.weights[k] = MAX_NET_WEIGHT;
        let heavy = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        check_legal(&p, &heavy.positions).unwrap();
        let one = |pos: &[[i64; 2]]| {
            let only = PlaceProblem {
                weights: Vec::new(),
                widths: p.widths.clone(),
                row_height: p.row_height,
                site: p.site,
                gap: p.gap,
                nets: vec![p.nets[k].clone()],
                intervals: p.intervals.clone(),
            };
            hpwl(&only, pos)
        };
        assert!(
            one(&heavy.positions) < one(&base.positions),
            "{} vs {}",
            one(&heavy.positions),
            one(&base.positions)
        );
        // Deterministic, and weights outside 1..=MAX_NET_WEIGHT or of the
        // wrong length are refused.
        let again = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        assert_eq!(heavy.positions, again.positions);
        for bad in [
            vec![0; p.nets.len()],
            vec![MAX_NET_WEIGHT + 1; p.nets.len()],
            vec![1000],
        ] {
            p.weights = bad;
            assert!(matches!(
                place(&p, &PlaceConfig::default(), &PlaceLimits::default()),
                Err(Error::Invalid(_))
            ));
        }
    }
    #[test]
    fn seeded_placement_keeps_cells_and_puts_new_ones_near_their_nets() {
        let p = chain(30, 3, 400);
        let base = place(&p, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        // Seeded with its own legal result: every cell stays within a few sites.
        let seeds: Vec<Option<[i64; 2]>> = base.positions.iter().map(|x| Some(*x)).collect();
        let again =
            place_seeded(&p, &seeds, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        check_legal(&p, &again.positions).unwrap();
        let moved: i64 = base
            .positions
            .iter()
            .zip(&again.positions)
            .map(|(a, b)| (a[0] - b[0]).abs() + (a[1] - b[1]).abs())
            .sum();
        assert!(again.hpwl <= base.hpwl, "{} vs {}", again.hpwl, base.hpwl);
        assert!(moved <= 30 * 100, "total displacement {moved}");
        // Cell 15 loses its seed: it lands next to its chain neighbours.
        let mut s2 = seeds.clone();
        s2[15] = None;
        let r = place_seeded(&p, &s2, &PlaceConfig::default(), &PlaceLimits::default()).unwrap();
        check_legal(&p, &r.positions).unwrap();
        let d = |a: [i64; 2], b: [i64; 2]| (a[0] - b[0]).abs() + (a[1] - b[1]).abs();
        assert!(
            d(r.positions[15], base.positions[14]) <= 200,
            "{:?}",
            r.positions[15]
        );
        // Deterministic; a seed vector of the wrong length is refused.
        assert_eq!(
            r.positions,
            place_seeded(&p, &s2, &PlaceConfig::default(), &PlaceLimits::default())
                .unwrap()
                .positions
        );
        assert!(
            place_seeded(
                &p,
                &seeds[..3],
                &PlaceConfig::default(),
                &PlaceLimits::default()
            )
            .is_err()
        );
    }
    #[test]
    fn capacity_and_budget_fail_explicitly() {
        let p = chain(30, 1, 400);
        assert!(matches!(
            place(&p, &PlaceConfig::default(), &PlaceLimits::default()),
            Err(Error::Capacity(_))
        ));
        let p = chain(10, 2, 400);
        let tight = PlaceLimits {
            max_work: 10,
            ..Default::default()
        };
        assert!(matches!(
            place(&p, &PlaceConfig::default(), &tight),
            Err(Error::Budget(_))
        ));
    }
}
