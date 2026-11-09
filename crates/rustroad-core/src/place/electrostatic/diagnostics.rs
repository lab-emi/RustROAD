//! Opt-in stage-current observations; no observation changes optimizer selection.
use super::{Counters, Failure, Meter, StageReport};
use crate::place::{PinRef, PlaceConfig, PlaceLimits, PlaceProblem, PlaceResult};
use crate::{Error, Rect, Result};

#[cfg(test)]
mod tests;

/// An observed integer phase, separate from an optimizer acceptance verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticPhase {
    /// Existing legal legacy initializer, before numerical optimization.
    SourceBaseline,
    /// Integer projection of a retained current stage iterate.
    ProjectedTarget,
    /// The same target after the existing isolated-cell retarget policy.
    AfterRetarget,
    /// Output of the existing common legalizer.
    AfterLegalize,
    /// Output of the existing common detailed placer.
    AfterDetail,
    /// The ordinary selected result, frozen before diagnostic trials.
    DefaultSelected,
}

/// Exact native centre objective; physical terminal cost has a different scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeCost {
    /// Sum of centre/fixed-point bounding-box half-perimeters in DBU.
    pub hpwl_dbu: u64,
    /// The unscaled milli-weight numerator.
    pub weighted_hpwl_milli: u64,
}

/// Borrowed coordinates are valid only for the duration of an observer call.
#[derive(Debug)]
pub struct DiagnosticPhaseView<'positions> {
    /// Stable run-local label; default reference uses `default`.
    pub label: &'positions str,
    /// Present for a reached optimizer stage; zero-based, at most two.
    pub stage: Option<u8>,
    /// The actual operation that produced these coordinates.
    pub phase: DiagnosticPhase,
    /// Global lower-left integer DBU, in original movable order.
    pub positions: &'positions [[i64; 2]],
    /// Independently scoped native-centre metric of the borrowed positions.
    pub native_report: NativeCost,
    /// Actual completed work through this metric pass; not an estimate.
    pub cumulative_work: u64,
    /// Actual counters at this completed observation boundary.
    pub counters: &'positions Counters,
}

/// Small telemetry for one reached stage; normalized coordinates stay private.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DiagnosticStage {
    /// Zero-based stage index.
    pub stage: u8,
    /// Index into at most three distinct private coordinate payloads.
    pub payload: usize,
    /// Current normalized weighted-average wire value, not physical pin HPWL.
    #[serde(rename = "current_wire_wa_normalized")]
    pub current_wire: f64,
    /// Current density energy from an existing accepted evaluation.
    #[serde(rename = "current_density_energy_normalized")]
    pub current_density: f64,
    /// Stage parameters from the actual optimizer trajectory.
    pub parameters: StageReport,
    /// Actual stage exit, including a successful stalled checkpoint exit.
    pub exit: String,
}

/// Diagnostic completion has separate actual and unused held work.
#[derive(Debug, serde::Serialize)]
pub struct DiagnosticSummary {
    /// Reached stages only; omitted zero-horizon stages were not evaluated.
    pub stages: Vec<DiagnosticStage>,
    /// Actual pair-copy/comparison and stage-control units.
    pub capture_work: u64,
    /// Reservation released without being reported as completed work.
    pub released_hold: u64,
    /// One cumulative meter including the retained baseline and all trials.
    pub final_work: u64,
    /// Cumulative numerical and exact-checkpoint counters.
    pub final_counts: Counters,
    /// Conservatively admitted simultaneous numerical/diagnostic buffers.
    pub state_estimate_bytes: u64,
    /// Arithmetic front-door record visits, charged before the retained baseline.
    pub preparation_work: u64,
}

/// Ordinary selected geometry plus observations; this is not a physical revision.
#[derive(Debug)]
pub struct DiagnosticResult {
    /// The unchanged ordinary selection, completed before stage trials.
    pub reference: PlaceResult,
    /// Actual diagnostic work and stage provenance.
    pub summary: DiagnosticSummary,
}

/// Bounded caller preparation is carried into the same total placement meter.
#[derive(Clone, Copy, Debug)]
pub struct DiagnosticContext {
    /// Exact region used by the ordinary unseeded analytic call.
    pub core: Rect,
    /// Completed defined input/preparation units; never a guessed allowance.
    pub preparation_work: u64,
}

pub(super) struct Capture<'observer> {
    pub(super) observer: &'observer mut dyn FnMut(DiagnosticPhaseView<'_>) -> Result<()>,
    pub(super) payloads: Vec<Vec<[f64; 2]>>,
    pub(super) stages: Vec<DiagnosticStage>,
    pub(super) used: u64,
    pub(super) released: u64,
    pub(super) state_bytes: u64,
    pub(super) reference_work: u64,
    pub(super) reference_counts: Counters,
    pub(super) final_work: u64,
    pub(super) final_counts: Counters,
    pub(super) preparation_work: u64,
    pub(super) objects: Option<usize>,
}

impl<'observer> Capture<'observer> {
    pub(super) fn new(
        observer: &'observer mut dyn FnMut(DiagnosticPhaseView<'_>) -> Result<()>,
    ) -> Result<Self> {
        let mut payloads = Vec::new();
        let mut stages = Vec::new();
        payloads
            .try_reserve_exact(3)
            .map_err(|_| Error::Budget("diagnostic payload allocation".into()))?;
        stages
            .try_reserve_exact(3)
            .map_err(|_| Error::Budget("diagnostic stage allocation".into()))?;
        if payloads.capacity() > 3 || stages.capacity() > 3 {
            return Err(Error::Budget("diagnostic fixed capacity admission".into()));
        }
        Ok(Self {
            observer,
            payloads,
            stages,
            used: 0,
            released: 0,
            state_bytes: 0,
            reference_work: 0,
            reference_counts: Counters::default(),
            final_work: 0,
            final_counts: Counters::default(),
            preparation_work: 0,
            objects: None,
        })
    }

    pub(super) fn hold_units(objects: usize) -> std::result::Result<u64, Failure> {
        u64::try_from(objects)
            .ok()
            .and_then(|n| n.checked_mul(6))
            .and_then(|n| n.checked_add(6))
            .ok_or(Failure::Budget("diagnostic_hold_overflow"))
    }

    fn consume(&mut self, meter: &mut Meter<'_>, units: u64) -> std::result::Result<(), Failure> {
        meter
            .work
            .consume_reserved(units)
            .map_err(|_| Failure::Budget("diagnostic_held_work"))?;
        self.used = self
            .used
            .checked_add(units)
            .ok_or(Failure::Budget("diagnostic_capture_work"))?;
        Ok(())
    }

    pub(super) fn entered(&mut self, meter: &mut Meter<'_>) -> std::result::Result<(), Failure> {
        if self.objects.is_none() {
            return Err(Failure::Numerical("diagnostic_not_admitted"));
        }
        self.consume(meter, 1)
    }

    pub(super) fn finish_stage(
        &mut self,
        observation: StageObservation<'_>,
        meter: &mut Meter<'_>,
    ) -> std::result::Result<(), Failure> {
        let StageObservation {
            stage,
            positions,
            value,
            parameters,
            exit,
        } = observation;
        if self.objects != Some(positions.len())
            || stage >= 3
            || self.stages.len() >= 3
            || self
                .stages
                .last()
                .is_some_and(|s| usize::from(s.stage) >= stage)
            || !value.wire.is_finite()
            || !value.density.is_finite()
        {
            return Err(Failure::Numerical("diagnostic_stage_identity"));
        }
        self.consume(meter, 1)?;
        let mut found = None;
        for index in 0..self.payloads.len() {
            let mut equal = self.payloads[index].len() == positions.len();
            if equal {
                for (i, point) in positions.iter().enumerate() {
                    self.consume(meter, 1)?;
                    let previous = self.payloads[index][i];
                    if previous[0].to_bits() != point[0].to_bits()
                        || previous[1].to_bits() != point[1].to_bits()
                    {
                        equal = false;
                        break;
                    }
                }
            }
            if equal {
                found = Some(index);
                break;
            }
        }
        let payload = match found {
            Some(index) => index,
            None => {
                let mut copy = Vec::new();
                copy.try_reserve_exact(positions.len())
                    .map_err(|_| Failure::Budget("diagnostic_snapshot_allocation"))?;
                if copy.capacity() > positions.len() {
                    return Err(Failure::Budget("diagnostic_snapshot_capacity"));
                }
                for position in positions {
                    self.consume(meter, 1)?;
                    copy.push(*position);
                }
                let index = self.payloads.len();
                self.payloads.push(copy);
                index
            }
        };
        self.stages.push(DiagnosticStage {
            stage: u8::try_from(stage).map_err(|_| Failure::Numerical("diagnostic_stage_index"))?,
            payload,
            current_wire: value.wire,
            current_density: value.density,
            parameters: parameters.clone(),
            exit: exit.into(),
        });
        Ok(())
    }

    pub(super) fn emit_global(
        &mut self,
        label: Label<'_>,
        positions: &[[i64; 2]],
        problem: &PlaceProblem,
        meter: &mut Meter<'_>,
    ) -> std::result::Result<(), Failure> {
        let pins = problem
            .nets
            .iter()
            .try_fold(0u64, |n, net| n.checked_add(net.len() as u64))
            .ok_or(Failure::Budget("diagnostic_pin_count"))?;
        meter.pins(pins)?;
        let native_report = native_cost(problem, positions)
            .map_err(|_| Failure::Numerical("diagnostic_metric_range"))?;
        self.observe_known(label, positions, native_report, meter)
    }

    pub(super) fn observe_known(
        &mut self,
        label: Label<'_>,
        positions: &[[i64; 2]],
        native_report: NativeCost,
        meter: &Meter<'_>,
    ) -> std::result::Result<(), Failure> {
        (self.observer)(DiagnosticPhaseView {
            label: label.name,
            stage: label.stage,
            phase: label.phase,
            positions,
            native_report,
            cumulative_work: meter.work.used,
            counters: meter.counts,
        })
        .map_err(|error| match error {
            Error::Budget(_) => Failure::Budget("diagnostic_observer_budget"),
            _ => Failure::Numerical("diagnostic_observer_failed"),
        })
    }

    fn emit_local(
        &mut self,
        label: Label<'_>,
        positions: &[[i64; 2]],
        context: &TrialContext<'_>,
        meter: &mut Meter<'_>,
    ) -> std::result::Result<(), Failure> {
        meter.work(positions.len() as u64)?;
        let mut global = Vec::new();
        global
            .try_reserve_exact(positions.len())
            .map_err(|_| Failure::Budget("diagnostic_global_allocation"))?;
        if global.capacity() > positions.len() {
            return Err(Failure::Budget("diagnostic_global_capacity"));
        }
        for p in positions {
            global.push([
                i64::try_from(i128::from(p[0]) + i128::from(context.origin[0]))
                    .map_err(|_| Failure::Numerical("diagnostic_coordinate_range"))?,
                i64::try_from(i128::from(p[1]) + i128::from(context.origin[1]))
                    .map_err(|_| Failure::Numerical("diagnostic_coordinate_range"))?,
            ]);
        }
        self.emit_global(label, &global, context.global_problem, meter)
    }

    pub(super) fn replay(
        &mut self,
        context: TrialContext<'_>,
        meter: &mut Meter<'_>,
    ) -> std::result::Result<(), Failure> {
        let model = context.model;
        let problem = model.problem;
        for stage_index in 0..self.stages.len() {
            let stage = self.stages[stage_index].stage;
            let payload = self.stages[stage_index].payload;
            let label = format!("stage{stage}");
            let scale = model.domain.scale as f64;
            meter.work(model.real as u64)?;
            let mut targets = Vec::new();
            targets
                .try_reserve_exact(model.real)
                .map_err(|_| Failure::Budget("diagnostic_target_allocation"))?;
            if targets.capacity() > model.real {
                return Err(Failure::Budget("diagnostic_target_capacity"));
            }
            for p in &self.payloads[payload][..model.real] {
                targets.push([p[0] * scale, p[1] * scale]);
            }
            meter.work(model.real as u64)?;
            let projected = model.lower_left(&self.payloads[payload])?;
            self.emit_local(
                Label {
                    name: &label,
                    stage: Some(stage),
                    phase: DiagnosticPhase::ProjectedTarget,
                },
                &projected,
                &context,
                meter,
            )?;
            drop(projected);
            if context.config.retarget_isolated {
                let anchors = super::super::isolated_targets(
                    problem,
                    context.config.max_net_pins,
                    meter.work,
                )
                .map_err(super::legal_error)?;
                for (target, anchor) in targets.iter_mut().zip(anchors) {
                    if let Some(anchor) = anchor {
                        *target = anchor;
                    }
                }
            }
            meter.work(model.real as u64)?;
            let mut retargeted = Vec::new();
            retargeted
                .try_reserve_exact(model.real)
                .map_err(|_| Failure::Budget("diagnostic_retarget_allocation"))?;
            if retargeted.capacity() > model.real {
                return Err(Failure::Budget("diagnostic_retarget_capacity"));
            }
            for (i, target) in targets.iter().enumerate() {
                let p = [
                    target[0] - problem.widths[i] as f64 / 2.0,
                    target[1] - problem.row_height as f64 / 2.0,
                ];
                if p.iter()
                    .any(|v| !v.is_finite() || v.abs() > (1u64 << 52) as f64)
                {
                    return Err(Failure::Numerical("diagnostic_retarget_conversion"));
                }
                retargeted.push([p[0].round() as i64, p[1].round() as i64]);
            }
            self.emit_local(
                Label {
                    name: &label,
                    stage: Some(stage),
                    phase: DiagnosticPhase::AfterRetarget,
                },
                &retargeted,
                &context,
                meter,
            )?;
            drop(retargeted);
            let mut legal = super::super::legalize(problem, &targets, None, meter.work)
                .map_err(super::legal_error)?;
            drop(targets);
            super::super::check_legal(problem, &legal).map_err(super::legal_error)?;
            self.emit_local(
                Label {
                    name: &label,
                    stage: Some(stage),
                    phase: DiagnosticPhase::AfterLegalize,
                },
                &legal,
                &context,
                meter,
            )?;
            super::super::detailed(
                problem,
                &mut legal,
                context.config.detailed_passes,
                context.config.max_net_pins,
                None,
                meter.work,
            )
            .map_err(super::legal_error)?;
            super::super::check_legal(problem, &legal).map_err(super::legal_error)?;
            self.emit_local(
                Label {
                    name: &label,
                    stage: Some(stage),
                    phase: DiagnosticPhase::AfterDetail,
                },
                &legal,
                &context,
                meter,
            )?;
        }
        Ok(())
    }

    fn into_summary(self) -> DiagnosticSummary {
        DiagnosticSummary {
            stages: self.stages,
            capture_work: self.used,
            released_hold: self.released,
            final_work: self.final_work,
            final_counts: self.final_counts,
            state_estimate_bytes: self.state_bytes,
            preparation_work: self.preparation_work,
        }
    }
}

pub(super) struct Label<'label> {
    pub(super) name: &'label str,
    pub(super) stage: Option<u8>,
    pub(super) phase: DiagnosticPhase,
}

pub(super) struct StageObservation<'stage> {
    pub(super) stage: usize,
    pub(super) positions: &'stage [[f64; 2]],
    pub(super) value: super::optimizer::Value,
    pub(super) parameters: &'stage StageReport,
    pub(super) exit: &'stage str,
}

pub(super) struct TrialContext<'model> {
    pub(super) model: &'model super::Model<'model>,
    pub(super) global_problem: &'model PlaceProblem,
    pub(super) config: &'model PlaceConfig,
    pub(super) origin: [i64; 2],
}

/// Observe current stage iterates after completing the unchanged ordinary choice.
///
/// This explicit research API has no seeded path and publishes no physical IR.
/// The observer must stream borrowed coordinates; it must not retain every trial.
///
/// # Errors
/// Rejects unsupported arithmetic, resource admission, numerical or observer
/// failures. No failure is converted into a successful baseline observation.
pub fn diagnose_cpu_ref(
    problem: &PlaceProblem,
    config: &PlaceConfig,
    limits: &PlaceLimits,
    context: DiagnosticContext,
    observer: &mut dyn FnMut(DiagnosticPhaseView<'_>) -> Result<()>,
) -> Result<DiagnosticResult> {
    if !crate::place::diagnostic_pool_threads().is_some_and(|n| n <= 4) {
        return Err(Error::Budget(
            "diagnostic requires an explicitly installed pool of at most four workers".into(),
        ));
    }
    let limits = super::bounded_limits(limits);
    if context.preparation_work > limits.max_work {
        return Err(Error::Budget("diagnostic caller preparation work".into()));
    }
    super::super::validate(problem, config, &limits)?;
    let cfg = config.electrostatic.as_ref().ok_or_else(|| {
        Error::Invalid("diagnostic requires unseeded electrostatic placement".into())
    })?;
    super::estimate(problem, cfg)
        .map_err(|failure| Error::Budget(format!("diagnostic admission: {}", failure.reason())))?;
    if problem.widths.is_empty() {
        return Err(Error::Invalid("diagnostic has no movable cells".into()));
    }
    let preparation = admit_native_arithmetic(problem, context.core)?
        .checked_add(context.preparation_work)
        .ok_or_else(|| Error::Budget("diagnostic accumulated preparation work".into()))?;
    let mut legacy = config.clone();
    legacy.electrostatic = None;
    let legacy_limits = PlaceLimits {
        max_work: limits
            .max_work
            .checked_sub(preparation)
            .ok_or_else(|| Error::Budget("diagnostic arithmetic preflight work".into()))?,
        ..limits.clone()
    };
    let mut baseline = super::super::place_legacy(problem, &legacy, &legacy_limits)?;
    baseline.work = baseline
        .work
        .checked_add(preparation)
        .ok_or_else(|| Error::Budget("diagnostic preparation work".into()))?;
    let mut capture = Capture::new(observer)?;
    capture.preparation_work = preparation;
    let reference = super::refine_observed(
        problem,
        config,
        &limits,
        baseline,
        Some(context.core),
        Some(&mut capture),
    )?;
    Ok(DiagnosticResult {
        reference,
        summary: capture.into_summary(),
    })
}

fn admit_native_arithmetic(problem: &PlaceProblem, core: Rect) -> Result<u64> {
    const PRIMITIVE: i128 = 1i128 << 40;
    let within = |n: i64| i128::from(n).abs() <= PRIMITIVE;
    if [
        core.lx,
        core.ly,
        core.ux,
        core.uy,
        problem.row_height,
        problem.site,
        problem.gap,
    ]
    .iter()
    .any(|n| !within(*n))
        || core.lx >= core.ux
        || core.ly >= core.uy
        || problem.widths.iter().any(|n| !within(*n))
    {
        return Err(Error::Invalid(
            "diagnostic unsupported native primitive range".into(),
        ));
    }
    let mut lo = [i128::from(core.lx), i128::from(core.ly)];
    let mut hi = [i128::from(core.ux), i128::from(core.uy)];
    for interval in &problem.intervals {
        if !within(interval.lx)
            || !within(interval.ux)
            || !within(interval.y)
            || interval.lx < core.lx
            || interval.ux > core.ux
            || interval.y < core.ly
            || i128::from(interval.y) + i128::from(problem.row_height) > i128::from(core.uy)
        {
            return Err(Error::Invalid(
                "diagnostic interval outside native envelope".into(),
            ));
        }
    }
    let mut weights = 0u128;
    let mut nets = 0u128;
    let mut pins = 0u64;
    for (index, net) in problem.nets.iter().enumerate() {
        pins = pins
            .checked_add(net.len() as u64)
            .ok_or_else(|| Error::Budget("diagnostic pins".into()))?;
        if net.len() >= 2 {
            nets += 1;
            weights = weights
                .checked_add(u128::from(problem.weight(index)))
                .ok_or_else(|| Error::Invalid("diagnostic weights overflow".into()))?;
        }
        for pin in net {
            if let PinRef::Fixed(point) = pin {
                for d in 0..2 {
                    if !within(point[d]) {
                        return Err(Error::Invalid("diagnostic fixed pin range".into()));
                    }
                    lo[d] = lo[d].min(i128::from(point[d]) - 1);
                    hi[d] = hi[d].max(i128::from(point[d]) + 1);
                }
            }
        }
    }
    let span = [hi[0] - lo[0], hi[1] - lo[1]];
    if span.iter().any(|n| *n > i128::from(i64::MAX)) {
        return Err(Error::Invalid("diagnostic native axis span".into()));
    }
    let upper = u128::try_from(span[0] + span[1])
        .map_err(|_| Error::Invalid("diagnostic native span".into()))?;
    if upper
        .checked_mul(nets)
        .is_none_or(|n| n > u128::from(u64::MAX))
        || upper
            .checked_mul(weights)
            .is_none_or(|n| n > u128::from(u64::MAX))
    {
        return Err(Error::Invalid(
            "diagnostic native objective may saturate".into(),
        ));
    }
    pins.checked_add(problem.widths.len() as u64)
        .and_then(|n| n.checked_add(problem.nets.len() as u64))
        .and_then(|n| n.checked_add(problem.intervals.len() as u64))
        .ok_or_else(|| Error::Budget("diagnostic arithmetic work".into()))
}

pub(super) fn native_cost(problem: &PlaceProblem, positions: &[[i64; 2]]) -> Result<NativeCost> {
    if positions.len() != problem.widths.len() {
        return Err(Error::Invalid("diagnostic position count".into()));
    }
    let (hpwl_dbu, weighted_hpwl_milli) = super::exact_objective(problem, positions)
        .map_err(|_| Error::Invalid("diagnostic native objective range".into()))?;
    Ok(NativeCost {
        hpwl_dbu,
        weighted_hpwl_milli,
    })
}
