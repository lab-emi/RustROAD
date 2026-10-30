//! Small orthonormal cosine solve of the symmetric finite-volume Neumann operator.
use super::{Failure, Meter};
use rayon::prelude::*;

pub(super) struct Grid {
    pub nx: usize,
    pub ny: usize,
    pub hx: f64,
    pub hy: f64,
    cx: Vec<f64>,
    cy: Vec<f64>,
    eigenvalues: Vec<f64>,
}
impl Grid {
    pub fn new(nx: usize, ny: usize, extent: [f64; 2]) -> Result<Self, Failure> {
        if !(2..=64).contains(&nx)
            || !(2..=64).contains(&ny)
            || extent.iter().any(|x| !x.is_finite() || *x <= 0.0)
        {
            return Err(Failure::Numerical("invalid_poisson_grid"));
        }
        let (hx, hy) = (extent[0] / nx as f64, extent[1] / ny as f64);
        let basis = |n: usize| {
            (0..n)
                .flat_map(|k| {
                    (0..n).map(move |j| {
                        let factor: f64 = if k == 0 { 1.0 } else { 2.0 };
                        let alpha = (factor / n as f64).sqrt();
                        alpha
                            * (std::f64::consts::PI * k as f64 * (j as f64 + 0.5) / n as f64).cos()
                    })
                })
                .collect::<Vec<_>>()
        };
        let mut eigenvalues = Vec::with_capacity(nx * ny);
        for k in 0..nx {
            for l in 0..ny {
                let value = 4.0
                    * ((std::f64::consts::PI * k as f64 / (2 * nx) as f64).sin() / hx).powi(2)
                    + 4.0
                        * ((std::f64::consts::PI * l as f64 / (2 * ny) as f64).sin() / hy).powi(2);
                if (k != 0 || l != 0) && (!value.is_finite() || value <= 0.0) {
                    return Err(Failure::Numerical("nonpositive_poisson_eigenvalue"));
                }
                eigenvalues.push(value);
            }
        }
        Ok(Self {
            nx,
            ny,
            hx,
            hy,
            cx: basis(nx),
            cy: basis(ny),
            eigenvalues,
        })
    }
    pub fn bins(&self) -> usize {
        self.nx * self.ny
    }
    pub fn terms(&self) -> u64 {
        (2 * self.bins() * (self.nx + self.ny)) as u64
    }

    // Each output has its own fixed serial inner sum. There are at most
    // workers independent jobs; no reduction depends on Rayon scheduling.
    fn outputs(output: &mut [f64], workers: usize, value: impl Fn(usize) -> f64 + Sync) {
        let chunk = output.len().div_ceil(workers.max(1));
        if workers <= 1 {
            for (i, out) in output.iter_mut().enumerate() {
                *out = value(i);
            }
        } else {
            output
                .par_chunks_mut(chunk)
                .enumerate()
                .for_each(|(job, slice)| {
                    for (offset, out) in slice.iter_mut().enumerate() {
                        *out = value(job * chunk + offset);
                    }
                });
        }
    }
    pub fn solve(
        &self,
        rho: &[f64],
        phi: &mut [f64],
        temp: &mut [f64],
        spectral: &mut [f64],
        workers: usize,
        meter: &mut Meter<'_>,
    ) -> Result<f64, Failure> {
        let n = self.bins();
        if [rho.len(), phi.len(), temp.len(), spectral.len()]
            .iter()
            .any(|x| *x != n)
            || rho.iter().any(|x| !x.is_finite())
        {
            return Err(Failure::Numerical("invalid_poisson_buffer"));
        }
        meter.transforms(self.terms())?;
        meter.work((2 * n) as u64)?;
        let (nx, ny) = (self.nx, self.ny);
        Self::outputs(temp, workers, |i| {
            let (k, y) = (i / ny, i % ny);
            let mut sum = 0.0;
            for x in 0..nx {
                sum += self.cx[k * nx + x] * rho[x * ny + y];
            }
            sum
        });
        Self::outputs(spectral, workers, |i| {
            let (k, l) = (i / ny, i % ny);
            let mut sum = 0.0;
            for y in 0..ny {
                sum += self.cy[l * ny + y] * temp[k * ny + y];
            }
            if i == 0 {
                0.0
            } else {
                sum / self.eigenvalues[i]
            }
        });
        Self::outputs(temp, workers, |i| {
            let (x, l) = (i / ny, i % ny);
            let mut sum = 0.0;
            for k in 0..nx {
                sum += self.cx[k * nx + x] * spectral[k * ny + l];
            }
            sum
        });
        Self::outputs(phi, workers, |i| {
            let (x, y) = (i / ny, i % ny);
            let mut sum = 0.0;
            for l in 0..ny {
                sum += self.cy[l * ny + y] * temp[x * ny + l];
            }
            sum
        });
        let mean = phi.iter().sum::<f64>() / n as f64;
        for p in phi.iter_mut() {
            *p -= mean;
        }
        if phi.iter().any(|x| !x.is_finite()) {
            return Err(Failure::Numerical("nonfinite_poisson_potential"));
        }
        let mean_rho = rho.iter().sum::<f64>() / n as f64;
        let (mut residual, mut norm) = (0.0, 0.0);
        for x in 0..nx {
            for y in 0..ny {
                let i = x * ny + y;
                let mut applied = 0.0;
                if x > 0 {
                    applied += (phi[i] - phi[i - ny]) / self.hx.powi(2);
                }
                if x + 1 < nx {
                    applied += (phi[i] - phi[i + ny]) / self.hx.powi(2);
                }
                if y > 0 {
                    applied += (phi[i] - phi[i - 1]) / self.hy.powi(2);
                }
                if y + 1 < ny {
                    applied += (phi[i] - phi[i + 1]) / self.hy.powi(2);
                }
                let expected = rho[i] - mean_rho;
                residual += (applied - expected).powi(2);
                norm += expected.powi(2);
            }
        }
        let relative = residual.sqrt() / norm.sqrt().max(1e-13);
        if !relative.is_finite() || residual.sqrt() > 1e-11 + 1e-10 * norm.sqrt() {
            return Err(Failure::Numerical("poisson_residual"));
        }
        Ok(relative)
    }
}
