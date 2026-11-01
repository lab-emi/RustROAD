//! Independent numerical formulations: FV matrix elimination and finite differences.
use super::{
    Counters, Domain, ElectrostaticConfig, Meter, PlaceConfig, PlaceLimits, PlaceProblem, Work,
    density, poisson, wire,
};
use crate::place::{Interval, PinRef};
use serde_json::json;

fn run_meter<T>(config: &ElectrostaticConfig, f: impl FnOnce(&mut Meter<'_>) -> T) -> T {
    let mut counts = Counters::default();
    let mut work = Work {
        held: 0,
        used: 0,
        max: 20_000_000_000,
    };
    let mut meter = Meter {
        config,
        counts: &mut counts,
        work: &mut work,
        start_work: 0,
        current_interactions: 0,
    };
    f(&mut meter)
}

#[test]
fn total_and_additional_work_caps_compose_without_spending_the_budget() {
    let hard = super::HARD_TOTAL_WORK;
    for requested in [0, 11, hard - 1, hard, hard + 1, u64::MAX] {
        let caller = PlaceLimits {
            max_cells: 73,
            max_pins: 311,
            max_work: requested,
        };
        let capped = super::bounded_limits(&caller);
        assert_eq!(capped.max_cells, caller.max_cells);
        assert_eq!(capped.max_pins, caller.max_pins);
        assert!(capped.max_work <= requested && capped.max_work <= hard);
        assert_eq!(caller.max_work, requested);
        if requested >= hard {
            assert_eq!(capped.max_work, hard);
        }
    }
    // Work charges are scalar counters, so boundary behavior needs no large
    // fixture or billions of operations. Common candidate helpers use Work.
    for (caller, baseline, additional, expected) in [
        (u64::MAX, hard - 3, 100, hard),
        (u64::MAX, hard - 10, 7, hard - 3),
        (11, 3, 4, 7),
        (11, 9, 8, 11),
        (u64::MAX, hard, u64::MAX, hard),
    ] {
        let cap = super::refinement_work_limit(caller, baseline, additional);
        assert_eq!(cap, expected);
        let mut work = Work {
            held: 0,
            used: baseline,
            max: cap,
        };
        work.add(cap - baseline).unwrap();
        assert!(work.add(1).is_err());
        assert_eq!(work.used, cap);
        assert!(work.used <= hard && work.used <= caller);
        assert!(work.used - baseline <= additional);
    }
}
// This oracle assembles conservative edge fluxes directly, including the
// zero-mean constraint, then solves an augmented dense matrix with pivoting.
// It does not use cosine modes, eigenvalues, or the production transform.
fn finite_volume(nx: usize, ny: usize, extent: [f64; 2], rho: &[f64]) -> Vec<f64> {
    let n = nx * ny;
    assert!(n <= 64);
    let mut a = vec![vec![0.0; n + 2]; n + 1];
    for x in 0..nx {
        for y in 0..ny {
            let i = x * ny + y;
            for (j, conductance) in [
                (
                    if x + 1 < nx { Some(i + ny) } else { None },
                    (nx as f64 / extent[0]).powi(2),
                ),
                (
                    if y + 1 < ny { Some(i + 1) } else { None },
                    (ny as f64 / extent[1]).powi(2),
                ),
            ] {
                if let Some(j) = j {
                    a[i][i] += conductance;
                    a[j][j] += conductance;
                    a[i][j] -= conductance;
                    a[j][i] -= conductance;
                }
            }
            a[i][n] = 1.0;
            a[n][i] = 1.0;
            a[i][n + 1] = rho[i];
        }
    }
    for col in 0..=n {
        let pivot = (col..=n)
            .max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))
            .unwrap();
        a.swap(col, pivot);
        let divisor = a[col][col];
        assert!(divisor.abs() > 1e-14);
        for value in &mut a[col][col..=n + 1] {
            *value /= divisor;
        }
        let pivot_row = a[col].clone();
        for (i, row) in a.iter_mut().enumerate() {
            if i == col {
                continue;
            }
            let multiplier = row[col];
            for (value, pivot) in row[col..=n + 1].iter_mut().zip(&pivot_row[col..=n + 1]) {
                *value -= multiplier * pivot;
            }
        }
    }
    (0..n).map(|i| a[i][n + 1]).collect()
}
#[test]
fn poisson_matches_independently_assembled_finite_volume() {
    let cfg = ElectrostaticConfig::default();
    for (nx, ny, extent) in [
        (2, 2, [1.0, 1.0]),
        (3, 4, [0.7, 1.0]),
        (5, 3, [1.0, 0.3]),
        (8, 8, [1.0, 0.8]),
    ] {
        for constant in [false, true] {
            let rho: Vec<_> = (0..nx * ny)
                .map(|i| {
                    if constant {
                        2.75
                    } else {
                        ((i * 17 + 3) % 23) as f64 / 11.0 - 0.7
                    }
                })
                .collect();
            let reference = finite_volume(nx, ny, extent, &rho);
            let grid = poisson::Grid::new(nx, ny, extent).unwrap();
            let (mut phi, mut temp, mut spectrum) =
                (vec![0.0; nx * ny], vec![0.0; nx * ny], vec![0.0; nx * ny]);
            let residual = run_meter(&cfg, |m| {
                grid.solve(&rho, &mut phi, &mut temp, &mut spectrum, 1, m)
            })
            .unwrap();
            assert!(residual < 1e-11 || constant, "{nx}x{ny}: {residual}");
            let scale = reference.iter().map(|x| x.abs()).fold(1e-12, f64::max);
            for (a, b) in phi.iter().zip(reference) {
                assert!(
                    (a - b).abs() <= 1e-13 + 1e-11 * scale,
                    "{nx}x{ny}: {a} != {b}"
                );
            }
        }
    }
}
fn density_value(
    extent: [f64; 2],
    bodies: &[density::Body],
    positions: &[[f64; 2]],
    capacity: &[f64],
) -> (f64, Vec<[f64; 2]>, Vec<f64>) {
    let cfg = ElectrostaticConfig::default();
    let mut d =
        density::Density::test_grid(poisson::Grid::new(4, 4, extent).unwrap(), capacity.to_vec())
            .unwrap();
    let mut gradient = vec![[0.0; 2]; positions.len()];
    let value = run_meter(&cfg, |m| {
        m.evaluation().unwrap();
        d.evaluate(bodies, positions, 0.6, 1, &mut gradient, m)
    })
    .unwrap();
    assert!(value.mass_residual < 1e-12);
    (value.energy, gradient, d.fields().0.to_vec())
}
#[test]
fn full_density_gradient_matches_step_refined_finite_differences_at_edges() {
    let extent = [1.0, 0.75];
    let capacity = vec![extent[0] * extent[1] / 16.0; 16];
    let bodies = [
        density::Body {
            width: 0.12,
            height: 0.09,
            area: 0.0108,
        },
        density::Body {
            width: 0.08,
            height: 0.06,
            area: 0.0048,
        },
    ];
    for positions in [
        [[0.06, 0.045], [0.47, 0.32]],
        [[0.94, 0.705], [0.41, 0.37]],
        [[0.34, 0.31], [0.39, 0.34]],
    ] {
        let (_, gradient, mass) = density_value(extent, &bodies, &positions, &capacity);
        assert!((mass.iter().sum::<f64>() - 0.0156).abs() < 1e-13);
        for i in 0..positions.len() {
            for axis in 0..2 {
                let mut errors = Vec::new();
                for step in [2e-5, 1e-5, 5e-6] {
                    let (mut plus, mut minus) = (positions, positions);
                    plus[i][axis] += step;
                    minus[i][axis] -= step;
                    let fd = (density_value(extent, &bodies, &plus, &capacity).0
                        - density_value(extent, &bodies, &minus, &capacity).0)
                        / (2.0 * step);
                    let error = (fd - gradient[i][axis]).abs();
                    errors.push(error);
                    assert!(
                        error <= 1e-8 + 1e-5 * gradient[i][axis].abs(),
                        "{i}/{axis} step{step}: {fd} vs{}",
                        gradient[i][axis]
                    );
                }
                assert!(
                    errors[2] <= errors[0] + 1e-11,
                    "step refinement: {errors:?}"
                );
            }
        }
    }
}
#[test]
fn wa_gradient_matches_independent_differences_including_coincident_pins() {
    for (values, gamma) in [
        (vec![0.1, 0.4, 0.9], 0.08),
        (vec![0.5; 128], 0.01),
        ((0..256).map(|i| (i % 31) as f64 / 31.0).collect(), 0.03),
    ] {
        let mut gradient = vec![0.0; values.len()];
        let value = wire::axis(&values, gamma, &mut gradient).unwrap();
        assert!(value.is_finite());
        for i in [0, values.len() / 2, values.len() - 1] {
            for h in [2e-6, 1e-6, 5e-7] {
                let (mut a, mut b) = (values.clone(), values.clone());
                a[i] += h;
                b[i] -= h;
                let fd = (wire::axis(&a, gamma, &mut vec![0.0; a.len()]).unwrap()
                    - wire::axis(&b, gamma, &mut vec![0.0; b.len()]).unwrap())
                    / (2.0 * h);
                assert!(
                    (fd - gradient[i]).abs() < 1e-8 + 1e-5 * gradient[i].abs(),
                    "{i}: {fd} vs{}",
                    gradient[i]
                );
            }
        }
        assert!(gradient.iter().sum::<f64>().abs() < 1e-11);
    }
}
#[test]
fn numerical_coordinates_are_common_translation_invariant() {
    let local = Domain::new(crate::Rect {
        lx: 0,
        ly: 0,
        ux: 1_000_000,
        uy: 750_000,
    })
    .unwrap();
    let moved = Domain::new(crate::Rect {
        lx: 1i64 << 55,
        ly: -(1i64 << 54),
        ux: (1i64 << 55) + 1_000_000,
        uy: -(1i64 << 54) + 750_000,
    })
    .unwrap();
    for p in [[100, 200], [999_000, 740_000], [-100, 760_000]] {
        assert_eq!(
            local.relative(p).unwrap(),
            moved
                .relative([p[0] + moved.origin[0], p[1] + moved.origin[1]])
                .unwrap()
        );
    }
}
#[test]
fn capacity_uses_integer_free_rectangles_without_double_spacing() {
    let p = PlaceProblem {
        widths: vec![100; 2],
        row_height: 100,
        site: 10,
        gap: 37,
        nets: vec![vec![PinRef::Cell(0), PinRef::Cell(1)]],
        weights: Vec::new(),
        intervals: vec![
            Interval {
                y: 0,
                lx: 0,
                ux: 200,
            },
            Interval {
                y: 0,
                lx: 300,
                ux: 500,
            },
            Interval {
                y: 100,
                lx: 0,
                ux: 500,
            },
        ],
    };
    let c = PlaceConfig {
        electrostatic: Some(ElectrostaticConfig::default()),
        ..PlaceConfig::default()
    };
    let domain = Domain::new(crate::Rect {
        lx: 0,
        ly: 0,
        ux: 500,
        uy: 200,
    })
    .unwrap();
    let model = super::prepare_model(&p, &c, domain, 2, 1).unwrap();
    let cfg = ElectrostaticConfig::default();
    let density = run_meter(&cfg, |m| density::Density::new(&model, m)).unwrap();
    let expected = (200 + 200 + 500) as f64 * 100.0 / 500f64.powi(2);
    assert!((density.capacity.iter().sum::<f64>() - expected).abs() < 1e-13);
}
fn problem() -> PlaceProblem {
    let mut nets = vec![vec![PinRef::Fixed([0, 50]), PinRef::Cell(0)]];
    for i in 1..20 {
        nets.push(vec![PinRef::Cell(i - 1), PinRef::Cell(i)]);
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
#[test]
fn late_stall_retains_checkpoint_but_hard_failure_keeps_baseline() {
    let p = problem();
    let limits = PlaceLimits::default();
    let base = PlaceConfig {
        global_iterations: 2,
        cg_iterations: 20,
        detailed_passes: 1,
        ..PlaceConfig::default()
    };
    let legacy = super::super::place(&p, &base, &limits).unwrap();
    let mode = ElectrostaticConfig {
        max_iterations: 15,
        max_evaluations: 64,
        max_fillers: 64,
        initial_step_milli: 100,
        max_backtracks: 0,
        ..ElectrostaticConfig::default()
    };
    let config = PlaceConfig {
        electrostatic: Some(mode.clone()),
        ..base.clone()
    };
    let r = super::super::place(&p, &config, &limits).unwrap();
    super::super::check_legal(&p, &r.positions).unwrap();
    assert!(r.hpwl < legacy.hpwl);
    let report = r.electrostatic.unwrap();
    assert_eq!(report.status, "accepted_no_worse");
    assert_eq!(report.termination, "line_search_stalled_after_checkpoint");
    assert!(report.counts.iterations > 0);
    assert!(report.candidate_weighted_hpwl_milli <= report.baseline_weighted_hpwl_milli);
    println!("P1_LATE_STALL={}", serde_json::to_string(&report).unwrap());

    // The same fixture with an oversized initial step never makes progress.
    let config = PlaceConfig {
        electrostatic: Some(ElectrostaticConfig {
            initial_step_milli: 1000,
            ..mode.clone()
        }),
        ..base.clone()
    };
    let r = super::super::place(&p, &config, &limits).unwrap();
    assert_eq!(r.positions, legacy.positions);
    let report = r.electrostatic.unwrap();
    assert_eq!(report.status, "fallback_line_search");
    assert_eq!(report.counts.iterations, 0);

    // A real accepted checkpoint must not hide a subsequent evaluation cap.
    let capped = ElectrostaticConfig {
        max_iterations: 6,
        max_evaluations: 4,
        initial_step_milli: 1000,
        max_backtracks: 4,
        ..mode
    };
    let config = PlaceConfig {
        electrostatic: Some(capped.clone()),
        ..base.clone()
    };
    let r = super::super::place(&p, &config, &limits).unwrap();
    assert_eq!(r.positions, legacy.positions);
    let report = r.electrostatic.unwrap();
    assert_eq!(report.status, "fallback_budget");
    assert_eq!(report.reason, "max_evaluations");
    assert!(report.counts.iterations > 0);
    assert_eq!(report.counts.evaluations, 4);
    println!(
        "P1_HARD_CAP_AFTER_PROGRESS={}",
        serde_json::to_string(&report).unwrap()
    );

    // Only the explicit option keeps that checkpoint, and it says so.
    let config = PlaceConfig {
        electrostatic: Some(ElectrostaticConfig {
            stop_at_evaluation_cap: true,
            ..capped
        }),
        ..base
    };
    let r = super::super::place(&p, &config, &limits).unwrap();
    let report = r.electrostatic.unwrap();
    assert_eq!(report.termination, "evaluation_horizon_after_checkpoint");
    assert_eq!(report.counts.evaluations, 4);
    assert!(report.counts.iterations > 0);
    assert!(report.status == "accepted_no_worse" || report.status == "fallback_no_improvement");
    assert!(r.hpwl <= legacy.hpwl);
}

#[test]
fn spacing_aware_density_deposits_the_gap_but_keeps_physical_edges_and_defaults_off() {
    let mut p = problem();
    p.gap = 2 * p.site;
    let limits = PlaceLimits::default();
    let base = PlaceConfig {
        global_iterations: 2,
        cg_iterations: 20,
        detailed_passes: 1,
        ..PlaceConfig::default()
    };
    let mode = ElectrostaticConfig {
        max_iterations: 12,
        max_evaluations: 64,
        workers: 2,
        ..ElectrostaticConfig::default()
    };
    // The options stay out of serialized configs unless explicitly enabled.
    let text = serde_json::to_string(&mode).unwrap();
    assert!(!text.contains("spacing_aware_density") && !text.contains("stop_at_evaluation_cap"));
    let enabled = ElectrostaticConfig {
        spacing_aware_density: true,
        ..mode.clone()
    };
    assert!(
        serde_json::to_string(&enabled)
            .unwrap()
            .contains("spacing_aware_density")
    );
    let domain = Domain::new(super::interval_region(&p).unwrap()).unwrap();
    let relative = Domain {
        origin: [0, 0],
        ..domain
    };
    let pins = p.nets.iter().map(Vec::len).sum();
    let bodies = |mode: &ElectrostaticConfig| {
        let config = PlaceConfig {
            electrostatic: Some(mode.clone()),
            ..base.clone()
        };
        let model = super::prepare_model(&p, &config, relative, pins, 2)
            .ok()
            .unwrap();
        let edge = model.project(0, [0.0, 0.0]).ok().unwrap();
        (model.bodies.clone(), edge)
    };
    let (plain, plain_edge) = bodies(&mode);
    let (padded, padded_edge) = bodies(&enabled);
    let scale = domain.scale as f64;
    for (i, (a, b)) in plain.iter().zip(&padded).enumerate() {
        assert_eq!(a.width, p.widths[i] as f64 / scale);
        assert_eq!(b.width, (p.widths[i] + p.gap) as f64 / scale);
        assert!(b.area > a.area);
    }
    // Spacing separates neighbours only: a cell may still touch the core edge.
    assert_eq!(plain_edge, padded_edge);
    let result = super::super::place(
        &p,
        &PlaceConfig {
            electrostatic: Some(enabled),
            ..base
        },
        &limits,
    )
    .unwrap();
    super::super::check_legal(&p, &result.positions).unwrap();
}
#[test]
fn opt_in_is_bounded_legal_and_thread_deterministic_with_exact_fallback() {
    let p = problem();
    let limits = PlaceLimits::default();
    let base = PlaceConfig {
        global_iterations: 2,
        cg_iterations: 20,
        detailed_passes: 1,
        ..PlaceConfig::default()
    };
    let legacy = super::super::place(&p, &base, &limits).unwrap();
    let mode = ElectrostaticConfig {
        max_iterations: 6,
        max_evaluations: 24,
        max_fillers: 64,
        ..ElectrostaticConfig::default()
    };
    let config = PlaceConfig {
        electrostatic: Some(mode.clone()),
        ..base.clone()
    };
    let mut expected = None;
    for workers in [1, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .unwrap();
        let r = pool
            .install(|| super::super::place(&p, &config, &limits))
            .unwrap();
        super::super::check_legal(&p, &r.positions).unwrap();
        assert!(r.hpwl <= legacy.hpwl);
        let report = r.electrostatic.unwrap();
        println!(
            "P1_SMALL_OUTCOME={}",
            serde_json::to_string(&report).unwrap()
        );
        assert_eq!(report.status, "accepted_no_worse", "{report:?}");
        assert_eq!(report.counts.iterations, mode.max_iterations);
        assert!(r.hpwl < legacy.hpwl);
        assert!(report.counts.evaluations <= mode.max_evaluations);
        assert!(r.work <= limits.max_work);
        if report.status.starts_with("fallback") {
            assert_eq!(r.positions, legacy.positions);
        }
        if let Some(ref expected) = expected {
            assert_eq!(expected, &r.positions);
        } else {
            expected = Some(r.positions);
        }
    }
    for mode in [
        ElectrostaticConfig {
            max_evaluations: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_state_mib: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_density_interactions: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_transform_terms: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_net_pin_evaluations: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_capacity_interactions: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_bins_per_object: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_interactions_per_evaluation: 1,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_fillers: 0,
            ..mode.clone()
        },
        ElectrostaticConfig {
            max_work: 1,
            ..mode.clone()
        },
    ] {
        let numerical_cap = mode.max_work;
        let c = PlaceConfig {
            electrostatic: Some(mode),
            ..base.clone()
        };
        let r = super::super::place(&p, &c, &limits).unwrap();
        assert_eq!(r.positions, legacy.positions);
        let report = r.electrostatic.unwrap();
        assert!(report.status.starts_with("fallback"));
        assert!(report.numerical_and_candidate_work <= numerical_cap);
    }
}
#[test]
fn none_serialization_and_seeded_semantics_remain_legacy() {
    let default = PlaceConfig::default();
    let text = serde_json::to_string(&default).unwrap();
    assert_eq!(
        text,
        r#"{"global_iterations":12,"cg_iterations":150,"detailed_passes":3,"max_net_pins":256}"#
    );
    let p = problem();
    let seeds: Vec<_> = (0..20).map(|i| Some([i * 30, 0])).collect();
    let limits = PlaceLimits::default();
    let baseline = super::super::place_seeded(&p, &seeds, &default, &limits).unwrap();
    let configured = PlaceConfig {
        electrostatic: Some(ElectrostaticConfig::default()),
        ..default.clone()
    };
    let actual = super::super::place_seeded(&p, &seeds, &configured, &limits).unwrap();
    assert_eq!(baseline.positions, actual.positions);
    assert_eq!(baseline.work, actual.work);
    assert_eq!(actual.electrostatic.unwrap().status, "not_applied_seeded");
    let bad = PlaceConfig {
        anchor_isolated: true,
        ..configured
    };
    assert!(super::super::place(&p, &bad, &limits).is_err());
    let mut grad = vec![0.0; 2];
    assert!(wire::axis(&[f64::NAN, 0.0], 0.1, &mut grad).is_err());
    assert!(wire::axis(&[0.0, 1.0], 0.0, &mut grad).is_err());
}

// The external independent oracle invokes this entry with its frozen data.
// Only mathematical input/name fields are read; expected outputs are ignored.
#[test]
fn independent_fixture_probe() {
    let payload = if let Some(path) = std::env::var_os("RUSTROAD_P1_NUMERICAL_INPUT") {
        use std::io::Read;
        let file = std::fs::File::open(path).unwrap();
        let mut bytes = Vec::new();
        file.take((1 << 20) + 1).read_to_end(&mut bytes).unwrap();
        assert!(bytes.len() <= 1 << 20);
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    } else {
        json!({"wa":[{"name":"default_wa","input":{"values":["0.1","0.4","0.9"],"gamma":"0.08","weight_milli":1000}}]})
    };
    let number = |v: &serde_json::Value| {
        v.as_str()
            .map(|s| s.parse::<f64>().unwrap())
            .or_else(|| v.as_f64())
            .unwrap()
    };
    let pair = |v: &serde_json::Value| [number(&v[0]), number(&v[1])];
    let vec_numbers =
        |v: &serde_json::Value| v.as_array().unwrap().iter().map(number).collect::<Vec<_>>();
    let cfg = ElectrostaticConfig::default();
    let mut output = serde_json::Map::new();
    let mut cases = 0;
    let mut numbers = 0usize;
    for kind in ["normalization", "wa", "deposition", "poisson", "density"] {
        let Some(rows) = payload.get(kind).and_then(|v| v.as_array()) else {
            continue;
        };
        let mut actual = Vec::new();
        for row in rows {
            cases += 1;
            assert!(cases <= 64);
            let input = &row["input"];
            let result = match kind {
                "normalization" => {
                    let origin = [
                        input["core_origin_dbu"][0].as_i64().unwrap(),
                        input["core_origin_dbu"][1].as_i64().unwrap(),
                    ];
                    let size = [
                        input["core_extent_dbu"][0].as_i64().unwrap(),
                        input["core_extent_dbu"][1].as_i64().unwrap(),
                    ];
                    let core = crate::Rect {
                        lx: origin[0],
                        ly: origin[1],
                        ux: origin[0].checked_add(size[0]).unwrap(),
                        uy: origin[1].checked_add(size[1]).unwrap(),
                    };
                    let domain = Domain::new(core).unwrap();
                    let scale = domain.scale as f64;
                    let lo = domain
                        .relative([
                            input["lower_left_dbu"][0].as_i64().unwrap(),
                            input["lower_left_dbu"][1].as_i64().unwrap(),
                        ])
                        .unwrap();
                    let (width, height) = (
                        input["width_dbu"].as_i64().unwrap(),
                        input["height_dbu"].as_i64().unwrap(),
                    );
                    json!({"L_dbu":domain.scale,"extent":domain.extent,"center":[(lo[0] as f64+width as f64/2.0)/scale,(lo[1] as f64+height as f64/2.0)/scale],
                        "width":width as f64/scale,"height":height as f64/scale,"area":(i128::from(width)*i128::from(height)) as f64/(scale*scale)})
                }
                "wa" => {
                    let values = vec_numbers(&input["values"]);
                    assert!(values.len() <= 4096);
                    let gamma = number(&input["gamma"]);
                    let weight = input["weight_milli"].as_u64().unwrap() as f64 / 1000.0;
                    let mut gradient = vec![0.0; values.len()];
                    let value = wire::axis(&values, gamma, &mut gradient).unwrap() * weight;
                    for g in &mut gradient {
                        *g *= weight;
                    }
                    numbers += gradient.len() + 1;
                    json!({"value":value,"gradient":gradient})
                }
                _ => {
                    let (nx, ny) = (
                        input["nx"].as_u64().unwrap() as usize,
                        input["ny"].as_u64().unwrap() as usize,
                    );
                    let extent = pair(&input["extent"]);
                    let grid = poisson::Grid::new(nx, ny, extent).unwrap();
                    numbers += 8 * grid.bins();
                    assert!(numbers <= 1_000_000);
                    if kind == "poisson" {
                        let raw = vec_numbers(&input["raw_rho"]);
                        assert_eq!(raw.len(), grid.bins());
                        let mean = raw.iter().sum::<f64>() / grid.bins() as f64;
                        let rho: Vec<_> = raw.iter().map(|r| r - mean).collect();
                        let (mut phi, mut tmp, mut spectrum) = (
                            vec![0.0; grid.bins()],
                            vec![0.0; grid.bins()],
                            vec![0.0; grid.bins()],
                        );
                        run_meter(&cfg, |m| {
                            grid.solve(&rho, &mut phi, &mut tmp, &mut spectrum, 1, m)
                        })
                        .unwrap();
                        let energy = 0.5
                            * grid.hx
                            * grid.hy
                            * rho.iter().zip(&phi).map(|(r, p)| r * p).sum::<f64>();
                        json!({"rho":rho,"phi":phi,"removed_rho_mean":mean,"energy":energy})
                    } else {
                        let objects = input["objects"].as_array().unwrap();
                        assert!(objects.len() <= 256);
                        numbers += 2 * objects.len() * grid.bins();
                        assert!(numbers <= 1_000_000);
                        let bodies: Vec<_> = objects
                            .iter()
                            .map(|o| density::Body {
                                width: number(&o["width"]),
                                height: number(&o["height"]),
                                area: number(&o["area"]),
                            })
                            .collect();
                        let positions: Vec<_> =
                            objects.iter().map(|o| pair(&o["center"])).collect();
                        if kind == "deposition" {
                            density::Density::deposition_probe(&grid, &bodies, &positions).unwrap()
                        } else {
                            let capacity = vec_numbers(&input["capacity"]);
                            let target = number(&input["target_density"]);
                            let bin_area = grid.hx * grid.hy;
                            let mut d =
                                density::Density::test_grid(grid, capacity.clone()).unwrap();
                            let mut gradient = vec![[0.0; 2]; positions.len()];
                            let value = run_meter(&cfg, |m| {
                                m.evaluation().unwrap();
                                d.evaluate(&bodies, &positions, target, 1, &mut gradient, m)
                            })
                            .unwrap();
                            let (mass, rho, phi) = d.fields();
                            let charge: Vec<_> = mass
                                .iter()
                                .zip(&capacity)
                                .map(|(m, c)| m - target * c)
                                .collect();
                            let removed_mean =
                                charge.iter().sum::<f64>() / (bin_area * charge.len() as f64);
                            let retained =
                                density::Density::deposition_probe(&d.grid, &bodies, &positions)
                                    .unwrap()["retained_axis_mass"]
                                    .clone();
                            json!({"mass":mass,"charge":charge,"rho":rho,"phi":phi,"energy":value.energy,"gradient":gradient,
                                "removed_rho_mean":removed_mean,"retained_axis_mass":retained})
                        }
                    }
                }
            };
            actual.push(json!({"name":row["name"],"output":result}));
        }
        output.insert(kind.into(), serde_json::Value::Array(actual));
    }
    println!("P1_NUMERICAL_OUTPUT={}", serde_json::Value::Object(output));
}
