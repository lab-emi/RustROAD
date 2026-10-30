//! Stable weighted-average wire model; gradients gather in stored pin order.
use super::{Failure, Meter, Model};

/// Scalar WA and its exact analytic derivative, with shifted exponentials.
pub(super) fn axis(values: &[f64], gamma: f64, gradient: &mut [f64]) -> Result<f64, Failure> {
    if values.is_empty()
        || values.len() != gradient.len()
        || !gamma.is_finite()
        || gamma <= 0.0
        || values.iter().any(|x| !x.is_finite())
    {
        return Err(Failure::Numerical("invalid_wa_input"));
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let (mut sp, mut sn, mut zp, mut zn) = (0.0, 0.0, 0.0, 0.0);
    for &z in values {
        let (p, n) = (((z - hi) / gamma).exp(), ((lo - z) / gamma).exp());
        sp += p;
        sn += n;
        zp += (z - hi) * p;
        zn += (z - lo) * n;
    }
    if !sp.is_finite() || !sn.is_finite() || sp < 1.0 || sn < 1.0 {
        return Err(Failure::Numerical("wa_denominator"));
    }
    let (mp, mn) = (zp / sp, zn / sn);
    for (&z, g) in values.iter().zip(gradient) {
        let p = ((z - hi) / gamma).exp() / sp;
        let n = ((lo - z) / gamma).exp() / sn;
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
pub(super) fn evaluate(
    model: &Model<'_>,
    positions: &[[f64; 2]],
    gamma: f64,
    gradient: &mut [[f64; 2]],
    meter: &mut Meter<'_>,
) -> Result<f64, Failure> {
    gradient.fill([0.0; 2]);
    meter.pins(model.modeled_pins as u64)?;
    let mut energy = 0.0;
    // One small bounded net buffer, never one input copy per worker.
    let mut values = vec![0.0; model.max_net_pins];
    let mut deriv = vec![0.0; model.max_net_pins];
    for (k, net) in model.problem.nets.iter().enumerate() {
        if net.len() < 2 || net.len() > model.max_net_pins {
            continue;
        }
        let weight = f64::from(model.problem.weight(k)) / 1000.0;
        for dimension in [0, 1] {
            for (out, pin) in values.iter_mut().zip(net) {
                *out = model.pin(*pin, positions)?[dimension];
            }
            energy += weight * axis(&values[..net.len()], gamma, &mut deriv[..net.len()])?;
            for (pin, &g) in net.iter().zip(&deriv) {
                if let super::super::PinRef::Cell(c) = *pin {
                    gradient[c as usize][dimension] += weight * g;
                }
            }
        }
    }
    if !energy.is_finite() {
        return Err(Failure::Numerical("nonfinite_wire_objective"));
    }
    Ok(energy)
}
