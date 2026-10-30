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
struct Axis {
    values: [AxisWeight; 64],
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
fn axis(center: f64, width: f64, bins: usize, radius: f64) -> Result<Axis, Failure> {
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
    let mut out = Axis {
        values: [AxisWeight::default(); 64],
        len: 0,
        retained: 0.0,
    };
    let (mut sum, mut derivative) = (0.0, 0.0);
    for bin in first..=last {
        let (lo, hi) = (
            bin as f64 * radius - center,
            (bin + 1) as f64 * radius - center,
        );
        let raw = averaged_cdf(hi, width, radius) - averaged_cdf(lo, width, radius);
        let d = averaged_hat(lo, width, radius) - averaged_hat(hi, width, radius);
        if !raw.is_finite() || !d.is_finite() || raw < -1e-12 {
            return Err(Failure::Numerical("invalid_deposition_weight"));
        }
        let (raw, d) = if raw < 0.0 { (0.0, 0.0) } else { (raw, d) };
        out.values[out.len] = AxisWeight {
            bin,
            weight: raw,
            derivative: d,
        };
        out.len += 1;
        sum += raw;
        derivative += d;
    }
    if !sum.is_finite() || sum <= 1e-12 {
        return Err(Failure::Numerical("zero_kernel_mass"));
    }
    out.retained = sum;
    if out.retained > 1.0 + 1e-10 {
        return Err(Failure::Numerical("integrated_kernel_mass"));
    }
    for v in &mut out.values[..out.len] {
        v.weight /= sum;
        v.derivative = (v.derivative - v.weight * derivative) / sum;
    }
    Ok(out)
}
pub(super) fn interaction_bound(width: f64, height: f64, grid: &Grid) -> usize {
    ((width / grid.hx).ceil() as usize + 3).min(grid.nx)
        * ((height / grid.hy).ceil() as usize + 3).min(grid.ny)
}
pub(super) struct Density {
    pub grid: Grid,
    pub capacity: Vec<f64>,
    mass: Vec<f64>,
    rho: Vec<f64>,
    phi: Vec<f64>,
    temp: Vec<f64>,
    spectral: Vec<f64>,
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
        let mut mass = 0.0;
        for (i, &position) in positions.iter().enumerate() {
            let Body {
                width,
                height,
                area,
            } = bodies[i];
            let x = axis(position[0], width, self.grid.nx, self.grid.hx)?;
            let y = axis(position[1], height, self.grid.ny, self.grid.hy)?;
            meter.interactions(x.len * y.len)?;
            mass += area;
            for a in &x.values[..x.len] {
                for b in &y.values[..y.len] {
                    self.mass[a.bin * self.grid.ny + b.bin] += area * a.weight * b.weight;
                }
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
        for (i, (&position, out)) in positions.iter().zip(gradient).enumerate() {
            let Body {
                width,
                height,
                area,
            } = bodies[i];
            let x = axis(position[0], width, self.grid.nx, self.grid.hx)?;
            let y = axis(position[1], height, self.grid.ny, self.grid.hy)?;
            meter.interactions(x.len * y.len)?;
            let (mut gx, mut gy) = (0.0, 0.0);
            for a in &x.values[..x.len] {
                for b in &y.values[..y.len] {
                    let phi = self.phi[a.bin * self.grid.ny + b.bin];
                    gx += phi * a.derivative * b.weight;
                    gy += phi * a.weight * b.derivative;
                }
            }
            *out = [area * gx, area * gy];
            if out.iter().any(|v| !v.is_finite()) {
                return Err(Failure::Numerical("nonfinite_density_gradient"));
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
            let x = axis(p[0], body.width, grid.nx, grid.hx)?;
            let y = axis(p[1], body.height, grid.ny, grid.hy)?;
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
