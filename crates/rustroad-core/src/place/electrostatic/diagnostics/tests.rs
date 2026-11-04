//! Protocol/trajectory properties, separate from the frozen external pin oracle.
use super::*;
use crate::place::{ElectrostaticConfig, Interval, Work};

fn parameters() -> StageReport {
    StageReport {
        gamma_normalized: 1.0,
        density_weight: 1.0,
        wire_gradient_norm: 1.0,
        density_gradient_norm: 1.0,
    }
}

#[test]
fn reservation_cannot_be_spent_twice_or_partially_on_error() {
    let mut work = Work {
        used: 5,
        max: 20,
        held: 0,
    };
    work.reserve(10).unwrap();
    work.add(5).unwrap();
    assert!(work.add(1).is_err());
    assert_eq!(work.used, 10);
    assert!(work.consume_reserved(11).is_err());
    assert_eq!(work.held, 10);
    work.consume_reserved(7).unwrap();
    assert_eq!(work.used, 17);
    assert_eq!(work.release_reserved(), 3);
    work.add(3).unwrap();
    assert_eq!(work.used, 20);
    assert_eq!(work.held, 0);
}

#[test]
fn stage_labels_survive_dedup_and_distinct_payloads_stay_bounded() {
    let cfg = ElectrostaticConfig::default();
    let mut counts = Counters::default();
    let mut work = Work {
        used: 0,
        max: 1000,
        held: 0,
    };
    work.reserve(Capture::hold_units(2).unwrap()).unwrap();
    let mut meter = Meter {
        config: &cfg,
        counts: &mut counts,
        work: &mut work,
        start_work: 0,
        current_interactions: 0,
    };
    let mut observer = |_: DiagnosticPhaseView<'_>| Ok(());
    let mut capture = Capture::new(&mut observer).unwrap();
    capture.objects = Some(2);
    let a = [[0.0, 1.0], [2.0, 3.0]];
    let b = [[0.0, 1.0], [2.0, 4.0]];
    for (stage, points) in [&a, &a, &b].into_iter().enumerate() {
        capture.entered(&mut meter).unwrap();
        capture
            .finish_stage(
                StageObservation {
                    stage,
                    positions: points,
                    value: super::super::optimizer::Value {
                        wire: stage as f64,
                        density: 0.0,
                    },
                    parameters: &parameters(),
                    exit: "stage_horizon",
                },
                &mut meter,
            )
            .unwrap();
    }
    assert_eq!(capture.payloads.len(), 2);
    assert_eq!(
        capture.stages.iter().map(|s| s.payload).collect::<Vec<_>>(),
        [0, 0, 1]
    );
    assert_eq!(
        capture.stages.iter().map(|s| s.stage).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    let before = meter.work.used;
    assert!(
        capture
            .finish_stage(
                StageObservation {
                    stage: 3,
                    positions: &a,
                    value: super::super::optimizer::Value {
                        wire: 0.0,
                        density: 0.0
                    },
                    parameters: &parameters(),
                    exit: "invalid"
                },
                &mut meter
            )
            .is_err()
    );
    assert_eq!(meter.work.used, before);
    assert!(capture.used <= Capture::hold_units(2).unwrap());
    assert_eq!(
        capture.used + meter.work.release_reserved(),
        Capture::hold_units(2).unwrap()
    );
}

fn problem() -> PlaceProblem {
    let mut nets = vec![vec![PinRef::Fixed([0, 50]), PinRef::Cell(0)]];
    for id in 1..20 {
        nets.push(vec![PinRef::Cell(id - 1), PinRef::Cell(id)]);
    }
    nets.push(vec![PinRef::Cell(19), PinRef::Fixed([1000, 350])]);
    PlaceProblem {
        widths: vec![20; 20],
        row_height: 100,
        site: 10,
        gap: 0,
        nets,
        weights: Vec::new(),
        intervals: (0..4)
            .map(|i| Interval {
                y: i * 100,
                lx: 0,
                ux: 1000,
            })
            .collect(),
    }
}
fn context() -> DiagnosticContext {
    DiagnosticContext {
        core: Rect {
            lx: 0,
            ly: 0,
            ux: 1000,
            uy: 400,
        },
        preparation_work: 0,
    }
}
fn config() -> PlaceConfig {
    PlaceConfig {
        global_iterations: 2,
        cg_iterations: 20,
        detailed_passes: 1,
        electrostatic: Some(ElectrostaticConfig {
            max_iterations: 15,
            max_evaluations: 64,
            max_fillers: 64,
            initial_step_milli: 100,
            max_backtracks: 0,
            workers: 1,
            ..ElectrostaticConfig::default()
        }),
        ..PlaceConfig::default()
    }
}

#[test]
fn observed_stage_trials_preserve_the_ordinary_choice_and_default_replay() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    pool.install(|| {
        let p = problem();
        let cfg = config();
        let limits = PlaceLimits::default();
        let before = crate::place::place_in_region(&p, &cfg, &limits, context().core).unwrap();
        let before_report = serde_json::to_vec(before.electrostatic.as_ref().unwrap()).unwrap();
        let mut seen = Vec::new();
        let mut observer = |view: DiagnosticPhaseView<'_>| {
            if matches!(
                view.phase,
                DiagnosticPhase::AfterLegalize
                    | DiagnosticPhase::AfterDetail
                    | DiagnosticPhase::DefaultSelected
            ) {
                crate::place::check_legal(&p, view.positions)?;
            }
            seen.push((view.phase, view.stage, view.positions.to_vec()));
            Ok(())
        };
        let diagnostic = diagnose_cpu_ref(&p, &cfg, &limits, context(), &mut observer).unwrap();
        assert_eq!(diagnostic.reference.positions, before.positions);
        assert_eq!(diagnostic.reference.hpwl, before.hpwl);
        let report = diagnostic.reference.electrostatic.as_ref().unwrap();
        assert_eq!(report.status, before.electrostatic.as_ref().unwrap().status);
        assert_eq!(report.termination, "line_search_stalled_after_checkpoint");
        assert!(!diagnostic.summary.stages.is_empty());
        assert!(diagnostic.summary.stages.len() <= 3);
        assert_eq!(seen.len(), 2 + 4 * diagnostic.summary.stages.len());
        assert!(diagnostic.summary.final_work >= diagnostic.reference.work);
        let after = crate::place::place_in_region(&p, &cfg, &limits, context().core).unwrap();
        assert_eq!(after.positions, before.positions);
        assert_eq!(
            serde_json::to_vec(after.electrostatic.as_ref().unwrap()).unwrap(),
            before_report,
            "the None diagnostic path must retain the entire ordinary report"
        );
    });
}

#[test]
fn observer_and_late_numerical_failure_never_become_baseline_success() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    pool.install(|| {
        let p = problem();
        let limits = PlaceLimits::default();
        let mut cfg = config();
        let mut observer = |view: DiagnosticPhaseView<'_>| {
            if view.phase == DiagnosticPhase::DefaultSelected {
                return Err(Error::Budget("injected bounded output failure".into()));
            }
            Ok(())
        };
        assert!(diagnose_cpu_ref(&p, &cfg, &limits, context(), &mut observer).is_err());
        let c = cfg.electrostatic.as_mut().unwrap();
        c.max_iterations = 6;
        c.max_evaluations = 4;
        c.initial_step_milli = 1000;
        c.max_backtracks = 4;
        // The strict option turns this evaluation cap after progress into a
        // late budget failure (the default keeps the checkpoint instead).
        c.stop_at_evaluation_cap = false;
        assert!(diagnose_cpu_ref(&p, &cfg, &limits, context(), &mut |_| Ok(())).is_err());
    });
}

#[test]
fn default_evaluation_cap_after_a_checkpoint_is_a_successful_horizon() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    pool.install(|| {
        let p = problem();
        let limits = PlaceLimits::default();
        let mut cfg = config();
        let c = cfg.electrostatic.as_mut().unwrap();
        c.max_iterations = 6;
        c.max_evaluations = 4;
        c.initial_step_milli = 1000;
        c.max_backtracks = 4;
        assert!(c.stop_at_evaluation_cap, "the default keeps the checkpoint");
        // The ordinary path ends at the cap with its best checkpoint ...
        let ordinary = crate::place::place_in_region(&p, &cfg, &limits, context().core).unwrap();
        let report = ordinary.electrostatic.as_ref().unwrap();
        assert_eq!(report.termination, "evaluation_horizon_after_checkpoint");
        assert_eq!(report.counts.evaluations, 4);
        crate::place::check_legal(&p, &ordinary.positions).unwrap();
        // ... and the diagnostic path reproduces it as a success, observing
        // the same selected geometry.
        let mut selected = None;
        let mut observer = |view: DiagnosticPhaseView<'_>| {
            if view.phase == DiagnosticPhase::DefaultSelected {
                selected = Some(view.positions.to_vec());
            }
            Ok(())
        };
        let diagnostic = diagnose_cpu_ref(&p, &cfg, &limits, context(), &mut observer).unwrap();
        assert_eq!(diagnostic.reference.positions, ordinary.positions);
        assert_eq!(diagnostic.reference.hpwl, ordinary.hpwl);
        let diagnosed = diagnostic.reference.electrostatic.as_ref().unwrap();
        assert_eq!(diagnosed.status, report.status);
        assert_eq!(diagnosed.termination, report.termination);
        assert_eq!(selected.as_deref(), Some(&ordinary.positions[..]));
        // The strict option makes the same cap an unverified error.
        cfg.electrostatic.as_mut().unwrap().stop_at_evaluation_cap = false;
        assert!(diagnose_cpu_ref(&p, &cfg, &limits, context(), &mut |_| Ok(())).is_err());
    });
}

#[test]
fn unsupported_native_arithmetic_rejects_before_old_cost_helpers() {
    let mut p = problem();
    assert!(admit_native_arithmetic(&p, context().core).is_ok());
    p.nets[0][0] = PinRef::Fixed([i64::MAX, 0]);
    assert!(admit_native_arithmetic(&p, context().core).is_err());
    p.nets[0][0] = PinRef::Fixed([0, 50]);
    p.intervals[0].lx = -1;
    assert!(admit_native_arithmetic(&p, context().core).is_err());
}
