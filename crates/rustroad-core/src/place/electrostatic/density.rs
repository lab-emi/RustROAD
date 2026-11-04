//! Conservative mass deposition and the adjoint of its discrete potential energy.
use super::{Failure, Meter, Model, poisson::Grid};

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct Body {
    pub width: f64,
    pub height: f64,
    pub area: f64,
}

#[derive(Clone, Copy, Default)]
struct AxisWeight {
    bin: usize,
    weight: f64,
    derivative: f64,
}
#[cfg(test)]
struct Axis {
    values: Vec<AxisWeight>,
    len: usize,
    retained: f64,
}

// Averages of the hat CDF and hat density over an interval. Integrating each
// polynomial piece separately avoids subtracting nearly equal cubic primitives.
fn averaged_cdf(at: f64, width: f64, radius: f64) -> f64 {
    let (lo, hi) = (at - width / 2.0, at + width / 2.0);
    let mut value = 0.0;
    let (a, b) = (lo.max(-radius), hi.min(0.0));
    if b > a {
        let (a, b) = (a + radius, b + radius);
        value += (b - a) / width * (a * a + a * b + b * b) / (6.0 * radius * radius);
    }
    let (a, b) = (lo.max(0.0), hi.min(radius));
    if b > a {
        let (u, v) = (radius - a, radius - b);
        value += (b - a) / width * (1.0 - (u * u + u * v + v * v) / (6.0 * radius * radius));
    }
    if hi > radius {
        value += (hi - lo.max(radius)) / width;
    }
    value
}
fn averaged_hat(at: f64, width: f64, radius: f64) -> f64 {
    let (lo, hi) = (at - width / 2.0, at + width / 2.0);
    let mut value = 0.0;
    let (a, b) = (lo.max(-radius), hi.min(0.0));
    if b > a {
        value += (b - a) / width * (a + b + 2.0 * radius) / (2.0 * radius * radius);
    }
    let (a, b) = (lo.max(0.0), hi.min(radius));
    if b > a {
        value += (b - a) / width * (2.0 * radius - a - b) / (2.0 * radius * radius);
    }
    value
}
/// Appends one object's normalized axis weights to `out` and returns the
/// retained (pre-normalization) kernel mass. Each bin boundary's averaged CDF
/// and hat value is evaluated once and shared by the two bins it separates.
fn axis(
    center: f64,
    width: f64,
    bins: usize,
    radius: f64,
    out: &mut Vec<AxisWeight>,
) -> Result<f64, Failure> {
    if !center.is_finite()
        || !width.is_finite()
        || width <= 0.0
        || !radius.is_finite()
        || radius <= 0.0
        || width / radius < 1e-9
    {
        return Err(Failure::Numerical("unsupported_kernel_extent"));
    }
    let first = ((center - width / 2.0 - radius) / radius).floor().max(0.0) as usize;
    let last = ((center + width / 2.0 + radius) / radius)
        .floor()
        .min((bins - 1) as f64) as usize;
    if first >= bins || first > last {
        return Err(Failure::Numerical("kernel_outside_domain"));
    }
    let start = out.len();
    let (mut sum, mut derivative) = (0.0, 0.0);
    let lo = first as f64 * radius - center;
    let (mut cdf_lo, mut hat_lo) = (
        averaged_cdf(lo, width, radius),
        averaged_hat(lo, width, radius),
    );
    for bin in first..=last {
        let hi = (bin + 1) as f64 * radius - center;
        let (cdf_hi, hat_hi) = (
            averaged_cdf(hi, width, radius),
            averaged_hat(hi, width, radius),
        );
        let raw = cdf_hi - cdf_lo;
        let d = hat_lo - hat_hi;
        (cdf_lo, hat_lo) = (cdf_hi, hat_hi);
        if !raw.is_finite() || !d.is_finite() || raw < -1e-12 {
            out.truncate(start);
            return Err(Failure::Numerical("invalid_deposition_weight"));
        }
        let (raw, d) = if raw < 0.0 { (0.0, 0.0) } else { (raw, d) };
        out.push(AxisWeight {
            bin,
            weight: raw,
            derivative: d,
        });
        sum += raw;
        derivative += d;
    }
    if !sum.is_finite() || sum <= 1e-12 || sum > 1.0 + 1e-10 {
        out.truncate(start);
        return Err(Failure::Numerical(if !sum.is_finite() || sum <= 1e-12 {
            "zero_kernel_mass"
        } else {
            "integrated_kernel_mass"
        }));
    }
    for v in &mut out[start..] {
        v.weight /= sum;
        v.derivative = (v.derivative - v.weight * derivative) / sum;
    }
    Ok(sum)
}
#[cfg(test)]
fn axis_values(center: f64, width: f64, bins: usize, radius: f64) -> Result<Axis, Failure> {
    let mut values = Vec::with_capacity(8);
    let retained = axis(center, width, bins, radius, &mut values)?;
    Ok(Axis {
        len: values.len(),
        values,
        retained,
    })
}
/// Compact deposition weights of one contiguous object range, computed once
/// per evaluation and shared by the forward deposit and its adjoint. The
/// buffers are sized in the calling thread from each object's bin bound, so
/// numerical workers never allocate.
struct Weights {
    objects: std::ops::Range<usize>,
    x: Vec<AxisWeight>,
    y: Vec<AxisWeight>,
    /// Per object: x length, y length.
    lengths: Vec<[u8; 2]>,
    failure: Option<(usize, Failure)>,
    adjoint: Option<Failure>,
}
fn weights(bodies: &[Body], positions: &[[f64; 2]], grid: &Grid, out: &mut Weights) {
    out.x.clear();
    out.y.clear();
    out.lengths.clear();
    out.failure = None;
    for i in out.objects.clone() {
        let (body, position) = (bodies[i], positions[i]);
        let (x0, y0) = (out.x.len(), out.y.len());
        let axes = axis(position[0], body.width, grid.nx, grid.hx, &mut out.x)
            .and_then(|_| axis(position[1], body.height, grid.ny, grid.hy, &mut out.y));
        match axes {
            // Axis lengths are at most 64 (the grid side) by construction.
            Ok(_) => out
                .lengths
                .push([(out.x.len() - x0) as u8, (out.y.len() - y0) as u8]),
            Err(failure) => {
                out.failure = Some((i, failure));
                break;
            }
        }
    }
}
/// Bins one object can touch along one axis (the admission bound).
fn axis_bound(extent: f64, step: f64, bins: usize) -> usize {
    ((extent / step).ceil() as usize + 3).min(bins)
}
pub(super) fn interaction_bound(width: f64, height: f64, grid: &Grid) -> usize {
    axis_bound(width, grid.hx, grid.nx) * axis_bound(height, grid.hy, grid.ny)
}
pub(super) struct Density {
    pub grid: Grid,
    pub capacity: Vec<f64>,
    mass: Vec<f64>,
    rho: Vec<f64>,
    phi: Vec<f64>,
    temp: Vec<f64>,
    spectral: Vec<f64>,
    jobs: Vec<Weights>,
}
pub(super) struct Value {
    pub energy: f64,
    pub mass_residual: f64,
    pub poisson_residual: f64,
}
impl Density {
    pub fn new(model: &Model<'_>, meter: &mut Meter<'_>) -> Result<Self, Failure> {
        let grid = Grid::new(64, 64, model.domain.extent)?;
        let mut exact = vec![0i128; grid.bins()];
        let [width, height] = model.domain.size;
        let [ox, oy] = model.domain.origin;
        let mut last_row = None;
        for iv in &model.problem.intervals {
            if let Some(y) = last_row
                && iv.y != y
                && i128::from(iv.y) - i128::from(y) < i128::from(model.problem.row_height)
            {
                return Err(Failure::Numerical("overlapping_capacity_rows"));
            }
            last_row = Some(iv.y);
            let (lx, ux) = (
                (i128::from(iv.lx) - i128::from(ox)) * grid.nx as i128,
                (i128::from(iv.ux) - i128::from(ox)) * grid.nx as i128,
            );
            let (ly, uy) = (
                (i128::from(iv.y) - i128::from(oy)) * grid.ny as i128,
                (i128::from(iv.y) + i128::from(model.problem.row_height) - i128::from(oy))
                    * grid.ny as i128,
            );
            if lx < 0
                || ly < 0
                || ux > i128::from(width) * grid.nx as i128
                || uy > i128::from(height) * grid.ny as i128
            {
                return Err(Failure::Numerical("capacity_outside_core"));
            }
            let (x0, x1) = (
                (lx / i128::from(width)) as usize,
                ((ux - 1) / i128::from(width)) as usize,
            );
            let (y0, y1) = (
                (ly / i128::from(height)) as usize,
                ((uy - 1) / i128::from(height)) as usize,
            );
            meter.capacity(((x1 - x0 + 1) * (y1 - y0 + 1)) as u64)?;
            for x in x0..=x1 {
                let dx = ux.min((x + 1) as i128 * i128::from(width))
                    - lx.max(x as i128 * i128::from(width));
                for y in y0..=y1 {
                    let dy = uy.min((y + 1) as i128 * i128::from(height))
                        - ly.max(y as i128 * i128::from(height));
                    exact[x * grid.ny + y] += dx * dy;
                }
            }
        }
        let full = i128::from(width) * i128::from(height);
        if exact.iter().any(|x| *x < 0 || *x > full) {
            return Err(Failure::Numerical("overlapping_capacity"));
        }
        let denominator = (grid.bins() as f64) * (model.domain.scale as f64).powi(2);
        let capacity: Vec<_> = exact.into_iter().map(|v| v as f64 / denominator).collect();
        if capacity.iter().sum::<f64>() <= 0.0 {
            return Err(Failure::Numerical("empty_capacity"));
        }
        let bins = grid.bins();
        Ok(Self {
            grid,
            capacity,
            mass: vec![0.0; bins],
            rho: vec![0.0; bins],
            phi: vec![0.0; bins],
            temp: vec![0.0; bins],
            spectral: vec![0.0; bins],
            jobs: Vec::new(),
        })
    }
    pub fn evaluate(
        &mut self,
        bodies: &[Body],
        positions: &[[f64; 2]],
        target_density: f64,
        workers: usize,
        gradient: &mut [[f64; 2]],
        meter: &mut Meter<'_>,
    ) -> Result<Value, Failure> {
        if positions.len() != bodies.len()
            || gradient.len() != positions.len()
            || !target_density.is_finite()
            || !(0.0..=1.0).contains(&target_density)
            || bodies.iter().any(|b| !b.area.is_finite() || b.area <= 0.0)
        {
            return Err(Failure::Numerical("density_buffer_extent"));
        }
        meter.work((8 * self.grid.bins()) as u64)?;
        self.mass.fill(0.0);
        gradient.fill([0.0; 2]);
        // Deposition weights depend only on each object's own position:
        // contiguous object ranges compute them in parallel, once for both
        // the forward deposit and its adjoint. Every sum below still runs
        // serially in object order, so any worker count gives identical bits.
        let objects = positions.len();
        let jobs = workers.clamp(1, objects.max(1));
        let size = objects.div_ceil(jobs).max(1);
        let range = |w: usize| (w * size).min(objects)..((w + 1) * size).min(objects);
        if self.jobs.len() != jobs
            || self
                .jobs
                .iter()
                .enumerate()
                .any(|(w, j)| j.objects != range(w))
        {
            let grid = &self.grid;
            self.jobs = (0..jobs)
                .map(|w| {
                    let objects = range(w);
                    let (mut x, mut y) = (0, 0);
                    // One spare bin per axis covers an exact integer extent.
                    for b in &bodies[objects.clone()] {
                        x += axis_bound(b.width, grid.hx, grid.nx) + 1;
                        y += axis_bound(b.height, grid.hy, grid.ny) + 1;
                    }
                    Weights {
                        x: Vec::with_capacity(x),
                        y: Vec::with_capacity(y),
                        lengths: Vec::with_capacity(objects.len()),
                        objects,
                        failure: None,
                        adjoint: None,
                    }
                })
                .collect();
        }
        let (grid, chunks) = (&self.grid, &mut self.jobs);
        if jobs == 1 {
            weights(bodies, positions, grid, &mut chunks[0]);
        } else {
            use rayon::prelude::*;
            chunks
                .par_iter_mut()
                .for_each(|c| weights(bodies, positions, grid, c));
        }
        let chunks = &self.jobs;
        let mut mass = 0.0;
        for c in chunks {
            let (mut xo, mut yo) = (0usize, 0usize);
            for (j, i) in c.objects.clone().enumerate() {
                if let Some((at, failure)) = c.failure
                    && at == i
                {
                    return Err(failure);
                }
                let [xl, yl] = c.lengths[j].map(usize::from);
                meter.interactions(xl * yl)?;
                let area = bodies[i].area;
                mass += area;
                for a in &c.x[xo..xo + xl] {
                    for b in &c.y[yo..yo + yl] {
                        self.mass[a.bin * self.grid.ny + b.bin] += area * a.weight * b.weight;
                    }
                }
                (xo, yo) = (xo + xl, yo + yl);
            }
        }
        let deposited = self.mass.iter().sum::<f64>();
        let mass_residual = (deposited - mass).abs() / mass.max(1e-20);
        if !mass_residual.is_finite() || mass_residual > 1e-10 {
            return Err(Failure::Numerical("mass_conservation"));
        }
        let area = self.grid.hx * self.grid.hy;
        for ((rho, mass), capacity) in self.rho.iter_mut().zip(&self.mass).zip(&self.capacity) {
            *rho = (*mass - target_density * capacity) / area;
        }
        let mean = self.rho.iter().sum::<f64>() / self.grid.bins() as f64;
        for rho in &mut self.rho {
            *rho -= mean;
        }
        let poisson_residual = self.grid.solve(
            &self.rho,
            &mut self.phi,
            &mut self.temp,
            &mut self.spectral,
            workers,
            meter,
        )?;
        let (mut dot, mut absolute) = (0.0, 0.0);
        for (&rho, &phi) in self.rho.iter().zip(&self.phi) {
            dot += rho * phi;
            absolute += (rho * phi).abs();
        }
        let energy = 0.5 * area * dot;
        if !energy.is_finite() || dot < -1e-12 * absolute.max(1e-12) {
            return Err(Failure::Numerical("negative_poisson_energy"));
        }
        // This is the adjoint of the same normalized deposition operator.
        // The potential has zero mean, so the removed DC charge has no force.
        // The adjoint visits are charged in object order as before; each
        // object's force is then independent of every other.
        for c in chunks {
            for &[xl, yl] in &c.lengths {
                meter.interactions(usize::from(xl) * usize::from(yl))?;
            }
        }
        let (phi, ny) = (&self.phi, self.grid.ny);
        let force = |c: &Weights, out: &mut [[f64; 2]]| -> Result<(), Failure> {
            let (mut xo, mut yo) = (0usize, 0usize);
            for ((i, out), [xl, yl]) in c.objects.clone().zip(out).zip(&c.lengths) {
                let (xl, yl) = (usize::from(*xl), usize::from(*yl));
                let (mut gx, mut gy) = (0.0, 0.0);
                for a in &c.x[xo..xo + xl] {
                    for b in &c.y[yo..yo + yl] {
                        let phi = phi[a.bin * ny + b.bin];
                        gx += phi * a.derivative * b.weight;
                        gy += phi * a.weight * b.derivative;
                    }
                }
                let area = bodies[i].area;
                *out = [area * gx, area * gy];
                if out.iter().any(|v| !v.is_finite()) {
                    return Err(Failure::Numerical("nonfinite_density_gradient"));
                }
                (xo, yo) = (xo + xl, yo + yl);
            }
            Ok(())
        };
        if jobs == 1 {
            force(&self.jobs[0], gradient)?;
        } else {
            use rayon::prelude::*;
            gradient
                .par_chunks_mut(size)
                .zip(self.jobs.par_iter_mut())
                .for_each(|(out, c)| {
                    let n = c.objects.len();
                    c.adjoint = force(c, &mut out[..n]).err();
                });
            if let Some(failure) = self.jobs.iter().find_map(|c| c.adjoint) {
                return Err(failure);
            }
        }
        Ok(Value {
            energy: energy.max(0.0),
            mass_residual,
            poisson_residual,
        })
    }
    #[cfg(test)]
    pub(super) fn test_grid(grid: Grid, capacity: Vec<f64>) -> Result<Self, Failure> {
        let bins = grid.bins();
        if capacity.len() != bins
            || capacity
                .iter()
                .any(|v| !v.is_finite() || *v < 0.0 || *v > grid.hx * grid.hy + 1e-12)
        {
            return Err(Failure::Numerical("test_capacity"));
        }
        Ok(Self {
            grid,
            capacity,
            mass: vec![0.0; bins],
            rho: vec![0.0; bins],
            phi: vec![0.0; bins],
            temp: vec![0.0; bins],
            spectral: vec![0.0; bins],
            jobs: Vec::new(),
        })
    }
    #[cfg(test)]
    pub(super) fn fields(&self) -> (&[f64], &[f64], &[f64]) {
        (&self.mass, &self.rho, &self.phi)
    }

    #[cfg(test)]
    pub(super) fn deposition_probe(
        grid: &Grid,
        bodies: &[Body],
        positions: &[[f64; 2]],
    ) -> Result<serde_json::Value, Failure> {
        if bodies.len() != positions.len() || bodies.len() * grid.bins() * 2 > 1_000_000 {
            return Err(Failure::Budget("probe_output"));
        }
        let mut mass = vec![0.0; grid.bins()];
        let mut derivative_mass = Vec::new();
        let mut retained = Vec::new();
        for (body, p) in bodies.iter().zip(positions) {
            let x = axis_values(p[0], body.width, grid.nx, grid.hx)?;
            let y = axis_values(p[1], body.height, grid.ny, grid.hy)?;
            let (mut dx, mut dy) = (vec![0.0; grid.bins()], vec![0.0; grid.bins()]);
            for a in &x.values[..x.len] {
                for b in &y.values[..y.len] {
                    let i = a.bin * grid.ny + b.bin;
                    mass[i] += body.area * a.weight * b.weight;
                    dx[i] = body.area * a.derivative * b.weight;
                    dy[i] = body.area * a.weight * b.derivative;
                }
            }
            derivative_mass.push([dx, dy]);
            retained.push([x.retained, y.retained]);
        }
        Ok(
            serde_json::json!({"mass":mass,"derivative_mass":derivative_mass,"retained_axis_mass":retained}),
        )
    }
}
