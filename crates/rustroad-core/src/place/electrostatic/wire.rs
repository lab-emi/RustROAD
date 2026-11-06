//! Stable weighted-average wire model; gradients gather in stored pin order.
use super::{Failure, Meter, Model};

/// Scalar WA and its exact analytic derivative, with shifted exponentials.
#[cfg(test)]
pub(super) fn axis(values: &[f64], gamma: f64, gradient: &mut [f64]) -> Result<f64, Failure> {
    let mut scratch = vec![0.0; values.len()];
    axis_with(values, gamma, gradient, &mut scratch)
}
/// [`axis`] with caller scratch: each shifted exponential is evaluated once
/// and reused by the gradient (the same values the energy sums used).
fn axis_with(
    values: &[f64],
    gamma: f64,
    gradient: &mut [f64],
    scratch: &mut [f64],
) -> Result<f64, Failure> {
    if values.is_empty()
        || values.len() != gradient.len()
        || values.len() != scratch.len()
        || !gamma.is_finite()
        || gamma <= 0.0
        || values.iter().any(|x| !x.is_finite())
    {
        return Err(Failure::Numerical("invalid_wa_input"));
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut sp, mut sn, mut zp, mut zn) = (0.0, 0.0, 0.0, 0.0);
    for ((&z, p_out), n_out) in values
        .iter()
        .zip(gradient.iter_mut())
        .zip(scratch.iter_mut())
    {
        let (p, n) = (((z - hi) / gamma).exp(), ((lo - z) / gamma).exp());
        (*p_out, *n_out) = (p, n);
        sp += p;
        sn += n;
        zp += (z - hi) * p;
        zn += (z - lo) * n;
    }
    if !sp.is_finite() || !sn.is_finite() || sp < 1.0 || sn < 1.0 {
        return Err(Failure::Numerical("wa_denominator"));
    }
    let (mp, mn) = (zp / sp, zn / sn);
    for ((&z, g), &n) in values.iter().zip(gradient.iter_mut()).zip(scratch.iter()) {
        let p = *g / sp;
        let n = n / sn;
        *g = p * (1.0 + ((z - hi) - mp) / gamma) - n * (1.0 - ((z - lo) - mn) / gamma);
        if !g.is_finite() {
            return Err(Failure::Numerical("nonfinite_wa_gradient"));
        }
    }
    let energy = (hi - lo) + mp - mn;
    if !energy.is_finite() || energy < -1e-12 {
        return Err(Failure::Numerical("nonfinite_wa_energy"));
    }
    Ok(energy.max(0.0))
}
/// Energies and pin derivatives of one contiguous net range, in stored net
/// and pin order; the first failure carries its net index.
#[derive(Default)]
struct Chunk {
    nets: std::ops::Range<usize>,
    energy: Vec<[f64; 2]>,
    derivative: Vec<[f64; 2]>,
    values: Vec<f64>,
    deriv: Vec<f64>,
    scratch: Vec<f64>,
    failure: Option<(usize, Failure)>,
}
/// Per-job wire buffers, sized once in the calling thread: numerical workers
/// never allocate, so no allocator state is created on pool threads.
#[derive(Default)]
pub(super) struct Scratch {
    chunks: Vec<Chunk>,
}
impl Scratch {
    fn prepare(&mut self, model: &Model<'_>, workers: usize) {
        let nets = model.problem.nets.len();
        let size = nets.div_ceil(workers).max(1);
        let range = |w: usize| (w * size).min(nets)..((w + 1) * size).min(nets);
        if self.chunks.len() == workers
            && self
                .chunks
                .iter()
                .enumerate()
                .all(|(w, c)| c.nets == range(w))
        {
            return;
        }
        self.chunks = (0..workers)
            .map(|w| {
                let nets = range(w);
                let modeled = |k: &usize| {
                    let n = model.problem.nets[*k].len();
                    (2..=model.max_net_pins).contains(&n).then_some(n)
                };
                let pins = nets.clone().filter_map(|k| modeled(&k)).sum();
                Chunk {
                    energy: Vec::with_capacity(nets.len()),
                    derivative: Vec::with_capacity(pins),
                    values: vec![0.0; model.max_net_pins],
                    deriv: vec![0.0; model.max_net_pins],
                    scratch: vec![0.0; model.max_net_pins],
                    nets,
                    failure: None,
                }
            })
            .collect();
    }
}
fn chunk(model: &Model<'_>, positions: &[[f64; 2]], gamma: f64, out: &mut Chunk) {
    out.energy.clear();
    out.derivative.clear();
    out.failure = None;
    for k in out.nets.clone() {
        let net = &model.problem.nets[k];
        if net.len() < 2 || net.len() > model.max_net_pins {
            continue;
        }
        let at = out.derivative.len();
        out.derivative.resize(at + net.len(), [0.0; 2]);
        let mut energy = [0.0; 2];
        for dimension in [0, 1] {
            let (values, deriv, scratch) = (&mut out.values, &mut out.deriv, &mut out.scratch);
            let mut evaluate = || {
                for (value, pin) in values.iter_mut().zip(net) {
                    *value = model.pin(*pin, positions)?[dimension];
                }
                axis_with(
                    &values[..net.len()],
                    gamma,
                    &mut deriv[..net.len()],
                    &mut scratch[..net.len()],
                )
            };
            match evaluate() {
                Ok(e) => energy[dimension] = e,
                Err(failure) => {
                    out.failure = Some((k, failure));
                    return;
                }
            }
            for (slot, &g) in out.derivative[at..].iter_mut().zip(&out.deriv) {
                slot[dimension] = g;
            }
        }
        out.energy.push(energy);
    }
}
pub(super) fn evaluate(
    model: &Model<'_>,
    positions: &[[f64; 2]],
    gamma: f64,
    gradient: &mut [[f64; 2]],
    scratch: &mut Scratch,
    meter: &mut Meter<'_>,
) -> Result<f64, Failure> {
    gradient.fill([0.0; 2]);
    meter.pins(model.modeled_pins as u64)?;
    // Independent contiguous net ranges evaluate in parallel; energies and
    // gradients are then gathered serially in stored net and pin order, so
    // every sum has the same operands in the same order for any worker count.
    let nets = model.problem.nets.len();
    let workers = model.workers.clamp(1, nets.max(1));
    scratch.prepare(model, workers);
    if workers == 1 {
        chunk(model, positions, gamma, &mut scratch.chunks[0]);
    } else {
        use rayon::prelude::*;
        scratch
            .chunks
            .par_iter_mut()
            .for_each(|c| chunk(model, positions, gamma, c));
    }
    let mut energy = 0.0;
    for c in &scratch.chunks {
        let (mut e, mut d) = (0usize, 0usize);
        for k in c.nets.clone() {
            if let Some((at, failure)) = c.failure
                && at == k
            {
                return Err(failure);
            }
            let net = &model.problem.nets[k];
            if net.len() < 2 || net.len() > model.max_net_pins {
                continue;
            }
            let weight = f64::from(model.problem.weight(k)) / 1000.0;
            let pins = &c.derivative[d..d + net.len()];
            for dimension in [0, 1] {
                energy += weight * c.energy[e][dimension];
                for (pin, g) in net.iter().zip(pins) {
                    if let super::super::PinRef::Cell(cell) = *pin {
                        gradient[cell as usize][dimension] += weight * g[dimension];
                    }
                }
            }
            e += 1;
            d += net.len();
        }
    }
    if !energy.is_finite() {
        return Err(Failure::Numerical("nonfinite_wire_objective"));
    }
    Ok(energy)
}
