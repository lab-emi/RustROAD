//! Fixed-stage projected acceleration with bounded trials and an exact checkpoint.
use super::{
    ElectrostaticConfig, ElectrostaticReport, Failure, Meter, Model, StageReport, density,
    exact_objective, wire,
};

#[derive(Clone, Copy)]
pub(super) struct Value {
    pub(super) wire: f64,
    pub(super) density: f64,
}
impl Value {
    fn total(self, weight: f64) -> f64 {
        self.wire + weight * self.density
    }
}
pub(super) struct Output {
    pub positions: Vec<[f64; 2]>,
    pub termination: String,
}

fn evaluate(
    model: &Model<'_>,
    density: &mut density::Density,
    positions: &[[f64; 2]],
    gamma: f64,
    gradients: (&mut [[f64; 2]], &mut [[f64; 2]]),
    meter: &mut Meter<'_>,
    report: &mut ElectrostaticReport,
) -> Result<Value, Failure> {
    let (wire_gradient, density_gradient) = gradients;
    meter.evaluation()?;
    let wire = wire::evaluate(model, positions, gamma, wire_gradient, meter)?;
    let d = density.evaluate(
        &model.bodies,
        positions,
        model.target_density,
        model.workers,
        density_gradient,
        meter,
    )?;
    report.maximum_mass_residual = Some(
        report
            .maximum_mass_residual
            .map_or(d.mass_residual, |x| x.max(d.mass_residual)),
    );
    report.maximum_poisson_residual = Some(
        report
            .maximum_poisson_residual
            .map_or(d.poisson_residual, |x| x.max(d.poisson_residual)),
    );
    Ok(Value {
        wire,
        density: d.energy,
    })
}
fn combine(
    out: &mut [[f64; 2]],
    wire: &[[f64; 2]],
    density: &[[f64; 2]],
    weight: f64,
) -> Result<(), Failure> {
    for ((out, w), d) in out.iter_mut().zip(wire).zip(density) {
        *out = [w[0] + weight * d[0], w[1] + weight * d[1]];
        if out.iter().any(|x| !x.is_finite()) {
            return Err(Failure::Numerical("nonfinite_objective_gradient"));
        }
    }
    Ok(())
}
fn norm(v: &[[f64; 2]]) -> f64 {
    let mut sum = 0.0;
    for p in v {
        sum += p[0] * p[0];
        sum += p[1] * p[1];
    }
    sum.sqrt()
}
fn diagonal(model: &Model<'_>, i: usize, weight: f64) -> Result<f64, Failure> {
    let area = model.shape(i).area;
    let value = (model.degree[i] + weight * area).max(1e-12);
    if !value.is_finite() || value <= 0.0 {
        return Err(Failure::Numerical("preconditioner"));
    }
    Ok(value)
}
pub(super) fn run(
    model: &Model<'_>,
    density: &mut density::Density,
    initial: Vec<[f64; 2]>,
    cfg: &ElectrostaticConfig,
    meter: &mut Meter<'_>,
    report: &mut ElectrostaticReport,
    mut capture: Option<&mut super::diagnostics::Capture<'_>>,
) -> Result<Output, Failure> {
    let n = model.objects();
    let mut x = initial;
    let mut previous = x.clone();
    let mut best = x.clone();
    let mut y = vec![[0.0; 2]; n];
    let mut trial = vec![[0.0; 2]; n];
    let mut current_gradient = vec![[0.0; 2]; n];
    let mut previous_gradient = vec![[0.0; 2]; n];
    let mut at_y = vec![[0.0; 2]; n];
    let mut wire_gradient = vec![[0.0; 2]; n];
    let mut density_gradient = vec![[0.0; 2]; n];
    let mut best_cost = (
        report.baseline_hpwl_dbu,
        report
            .baseline_weighted_hpwl_milli
            .ok_or(Failure::Numerical("missing_baseline_objective"))?,
    );
    let baseline_cost = best_cost;
    let mut termination = "iteration_horizon".to_string();
    for stage in 0..3 {
        let horizon = cfg.max_iterations / 3 + usize::from(stage < cfg.max_iterations % 3);
        if horizon == 0 {
            continue;
        }
        if let Some(capture) = capture.as_deref_mut() {
            capture.entered(meter)?;
        }
        let mut stage_exit = "iteration_horizon";
        let gamma = f64::from(cfg.smoothing_bins_milli[stage]) / 1000.0
            * density.grid.hx.max(density.grid.hy);
        let mut current_value = evaluate(
            model,
            density,
            &x,
            gamma,
            (&mut wire_gradient, &mut density_gradient),
            meter,
            report,
        )?;
        let (wn, dn) = (norm(&wire_gradient), norm(&density_gradient));
        if !wn.is_finite() || !dn.is_finite() {
            return Err(Failure::Numerical("gradient_norm"));
        }
        let weight = if dn <= 1e-24 {
            0.0
        } else {
            ((wn.max(1e-12) / dn) * (f64::from(cfg.density_weight_milli) / 1000.0))
                .clamp(1e-12, 1e12)
        };
        report.stages.push(StageReport {
            gamma_normalized: gamma,
            density_weight: weight,
            wire_gradient_norm: wn,
            density_gradient_norm: dn,
        });
        combine(
            &mut current_gradient,
            &wire_gradient,
            &density_gradient,
            weight,
        )?;
        previous.clone_from(&x);
        previous_gradient.clone_from(&current_gradient);
        let (mut t, mut step, mut history): (f64, f64, bool) =
            (1.0, f64::from(cfg.initial_step_milli) / 1000.0, false);
        for _ in 0..horizon {
            meter.work((20 * n) as u64)?;
            if norm(&current_gradient) <= 1e-12 {
                termination = "gradient_tolerance".into();
                stage_exit = "gradient_tolerance";
                break;
            }
            if history {
                let (mut numerator, mut denominator) = (0.0, 0.0);
                for i in 0..n {
                    let p = diagonal(model, i, weight)?;
                    for d in 0..2 {
                        let dx = x[i][d] - previous[i][d];
                        let dg = current_gradient[i][d] - previous_gradient[i][d];
                        numerator += dx * dg;
                        denominator += dg * dg / p;
                    }
                }
                if numerator.is_finite()
                    && denominator.is_finite()
                    && numerator > 0.0
                    && denominator > 1e-30
                {
                    step = (numerator / denominator).clamp(
                        f64::from(cfg.min_step_micros) / 1e6,
                        f64::from(cfg.max_step_milli) / 1000.0,
                    );
                } else {
                    meter.counts.secant_skips += 1;
                }
            }
            let next_t = (1.0 + (1.0 + 4.0 * t * t).sqrt()) / 2.0;
            let mut beta = (t - 1.0) / next_t;
            let mut restarted = false;
            for i in 0..n {
                y[i] = model.project(
                    i,
                    [
                        x[i][0] + beta * (x[i][0] - previous[i][0]),
                        x[i][1] + beta * (x[i][1] - previous[i][1]),
                    ],
                )?;
            }
            let move_limit = f64::from(cfg.max_move_bins_milli) / 1000.0;
            if y.iter().zip(&x).any(|(y, x)| {
                (y[0] - x[0]).abs() > move_limit * density.grid.hx
                    || (y[1] - x[1]).abs() > move_limit * density.grid.hy
            }) {
                beta = 0.0;
                restarted = true;
                meter.counts.momentum_restarts += 1;
                y.clone_from(&x);
            }
            let mut at_y_value;
            if beta == 0.0 {
                at_y.clone_from(&current_gradient);
                at_y_value = current_value;
            } else {
                at_y_value = evaluate(
                    model,
                    density,
                    &y,
                    gamma,
                    (&mut wire_gradient, &mut density_gradient),
                    meter,
                    report,
                )?;
                combine(&mut at_y, &wire_gradient, &density_gradient, weight)?;
            }
            let mut accepted = None;
            for attempt in 0..=cfg.max_backtracks {
                meter.counts.line_search_trials += 1;
                let mut excessive = false;
                for i in 0..n {
                    let p = diagonal(model, i, weight)?;
                    trial[i] = model.project(
                        i,
                        [
                            y[i][0] - step * at_y[i][0] / p,
                            y[i][1] - step * at_y[i][1] / p,
                        ],
                    )?;
                    let limit = f64::from(cfg.max_move_bins_milli) / 1000.0;
                    excessive |= (trial[i][0] - y[i][0]).abs() > limit * density.grid.hx
                        || (trial[i][1] - y[i][1]).abs() > limit * density.grid.hy
                        || (trial[i][0] - x[i][0]).abs() > limit * density.grid.hx
                        || (trial[i][1] - x[i][1]).abs() > limit * density.grid.hy;
                }
                if !excessive {
                    let value = evaluate(
                        model,
                        density,
                        &trial,
                        gamma,
                        (&mut wire_gradient, &mut density_gradient),
                        meter,
                        report,
                    )?;
                    let (mut linear, mut quadratic) = (0.0, 0.0);
                    for i in 0..n {
                        let p = diagonal(model, i, weight)?;
                        for d in 0..2 {
                            let delta = trial[i][d] - y[i][d];
                            linear += at_y[i][d] * delta;
                            quadratic += p * delta * delta / (2.0 * step);
                        }
                    }
                    let fy = at_y_value.total(weight);
                    let ft = value.total(weight);
                    let current = current_value.total(weight);
                    let tolerance = 1e-12 * (1.0 + fy.abs().max(current.abs()));
                    if !fy.is_finite()
                        || !ft.is_finite()
                        || !linear.is_finite()
                        || !quadratic.is_finite()
                    {
                        return Err(Failure::Numerical("majorization_value"));
                    }
                    if ft <= fy + linear + quadratic + tolerance {
                        if ft <= current + tolerance {
                            accepted = Some(value);
                            break;
                        }
                        if beta != 0.0 {
                            beta = 0.0;
                            restarted = true;
                            meter.counts.momentum_restarts += 1;
                            y.clone_from(&x);
                            at_y.clone_from(&current_gradient);
                            at_y_value = current_value;
                        }
                    }
                }
                if attempt == cfg.max_backtracks {
                    break;
                }
                step *= 0.5;
                meter.counts.backtracks += 1;
                if step < f64::from(cfg.min_step_micros) / 1e6 {
                    break;
                }
            }
            let value = match accepted {
                Some(value) => value,
                None if meter.counts.iterations > 0 && best_cost != baseline_cost => {
                    // A later stalled search does not invalidate an earlier
                    // finite checkpoint. Hard/numerical errors above still
                    // propagate, and integer legalization decides acceptance.
                    if let Some(capture) = capture.as_deref_mut() {
                        let parameters = report
                            .stages
                            .last()
                            .ok_or(Failure::Numerical("diagnostic_stage_parameters"))?;
                        capture.finish_stage(
                            super::diagnostics::StageObservation {
                                stage,
                                positions: &x,
                                value: current_value,
                                parameters,
                                exit: "line_search_stalled_after_checkpoint",
                            },
                            meter,
                        )?;
                    }
                    return Ok(Output {
                        positions: best,
                        termination: "line_search_stalled_after_checkpoint".into(),
                    });
                }
                None => return Err(Failure::NoStep("majorization_or_movement_limit")),
            };
            std::mem::swap(&mut previous, &mut x);
            std::mem::swap(&mut x, &mut trial);
            std::mem::swap(&mut previous_gradient, &mut current_gradient);
            combine(
                &mut current_gradient,
                &wire_gradient,
                &density_gradient,
                weight,
            )?;
            current_value = value;
            history = !restarted;
            t = if restarted { 1.0 } else { next_t };
            meter.counts.iterations += 1;
            meter.pins(model.total_pins as u64)?;
            let cost = exact_objective(model.problem, &model.lower_left(&x)?)?;
            if cost.0 <= baseline_cost.0
                && cost.1 <= baseline_cost.1
                && (cost.1, cost.0) < (best_cost.1, best_cost.0)
            {
                best.clone_from(&x);
                best_cost = cost;
            }
        }
        if let Some(capture) = capture.as_deref_mut() {
            let parameters = report
                .stages
                .last()
                .ok_or(Failure::Numerical("diagnostic_stage_parameters"))?;
            capture.finish_stage(
                super::diagnostics::StageObservation {
                    stage,
                    positions: &x,
                    value: current_value,
                    parameters,
                    exit: stage_exit,
                },
                meter,
            )?;
        }
    }
    Ok(Output {
        positions: best,
        termination,
    })
}
