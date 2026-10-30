//! Opt-in bounded numerical targets; the common integer legalizer remains authoritative.
use super::{Interval, PinRef, PlaceConfig, PlaceLimits, PlaceProblem, PlaceResult, Work};
use crate::{Error, Rect, Result};
mod density;
mod optimizer;
mod poisson;
mod wire;

const HARD_TOTAL_WORK: u64 = 20_000_000_000;

/// Both the retained baseline and refinement share the P1 total ceiling.
pub(super) fn bounded_limits(limits: &PlaceLimits) -> PlaceLimits {
    PlaceLimits {
        max_work: limits.max_work.min(HARD_TOTAL_WORK),
        ..limits.clone()
    }
}

fn refinement_work_limit(caller: u64, baseline: u64, additional: u64) -> u64 {
    caller
        .min(HARD_TOTAL_WORK)
        .min(baseline.saturating_add(additional))
}

/// Budgets and integer-scaled parameters for the unseeded CPU prototype.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ElectrostaticConfig {
    /// Total accepted-step horizon, not a convergence guarantee (1..=200).
    pub max_iterations: usize,
    /// Every full wire/deposition/Poisson trial consumes one evaluation (1..=512).
    pub max_evaluations: usize,
    /// Step halvings after the first trial (0..=4).
    pub max_backtracks: usize,
    /// Requested numerical workers, also bounded by the calling Rayon pool.
    pub workers: usize,
    /// Additional placement-state allowance (1..=256 MiB).
    pub max_state_mib: u64,
    /// Movable input ceiling (at most100000).
    pub max_cells: usize,
    /// Total input pin occurrences, including repeated and unmodelled pins.
    pub max_pins: usize,
    /// Total input net records, including nets above the fanout cutoff.
    pub max_nets: usize,
    /// Free rectangles; no spacing area is subtracted a second time.
    pub max_intervals: usize,
    /// Numerical-only filler budget; zero with positive slack explicitly falls back.
    pub max_fillers: usize,
    /// Physical-area target in milli-units, at most700.
    pub target_density_milli: u32,
    /// Three strictly decreasing smoothing lengths, in milli-bin units.
    pub smoothing_bins_milli: [u32; 3],
    /// Multiplier of the guarded gradient-norm density weight, in milli-units.
    pub density_weight_milli: u32,
    /// Initial preconditioned step, in milli-units.
    pub initial_step_milli: u32,
    /// Minimum permitted step, in micro-units.
    pub min_step_micros: u32,
    /// Maximum secant step, in milli-units.
    pub max_step_milli: u32,
    /// Maximum coordinate change per trial, in milli-bin units.
    pub max_move_bins_milli: u32,
    /// Maximum support Cartesian product for any one object (at most256).
    pub max_bins_per_object: usize,
    /// Counts both forward deposition and adjoint visits (at most2000000).
    pub max_interactions_per_evaluation: usize,
    /// Cumulative forward and adjoint bin visits.
    pub max_density_interactions: u64,
    /// Cumulative wire and exact-checkpoint pin visits.
    pub max_net_pin_evaluations: u64,
    /// Cumulative separable-transform multiply-add terms.
    pub max_transform_terms: u64,
    /// Exact integer interval/bin capacity intersections.
    pub max_capacity_interactions: u64,
    /// Additional numerical work, also bounded by the original remaining work cap.
    pub max_work: u64,
}
impl Default for ElectrostaticConfig {
    fn default() -> Self {
        Self {
            max_iterations: 200,
            max_evaluations: 512,
            max_backtracks: 4,
            workers: 4,
            max_state_mib: 256,
            max_cells: 100_000,
            max_pins: 1_000_000,
            max_nets: 100_000,
            max_intervals: 250_000,
            max_fillers: 4096,
            target_density_milli: 600,
            smoothing_bins_milli: [2000, 1000, 500],
            density_weight_milli: 1000,
            initial_step_milli: 1000,
            min_step_micros: 1,
            max_step_milli: 4000,
            max_move_bins_milli: 2000,
            max_bins_per_object: 256,
            max_interactions_per_evaluation: 2_000_000,
            max_density_interactions: 1_000_000_000,
            max_net_pin_evaluations: 1_000_000_000,
            max_transform_terms: 1_000_000_000,
            max_capacity_interactions: 64_000_000,
            max_work: 20_000_000_000,
        }
    }
}
impl ElectrostaticConfig {
    pub(super) fn validate(&self) -> Result<()> {
        for (v, cap) in [
            (self.max_iterations, 200),
            (self.max_evaluations, 512),
            (self.workers, 4),
            (self.max_cells, 100_000),
            (self.max_pins, 1_000_000),
            (self.max_nets, 100_000),
            (self.max_intervals, 250_000),
            (self.max_bins_per_object, 256),
            (self.max_interactions_per_evaluation, 2_000_000),
        ] {
            if v == 0 || v > cap {
                return Err(super::invalid("electrostatic limit outside hard bounds"));
            }
        }
        if self.max_backtracks > 4
            || self.max_fillers > 4096
            || self.max_state_mib == 0
            || self.max_state_mib > 256
            || !(1..=700).contains(&self.target_density_milli)
            || !(1..=16_000).contains(&self.density_weight_milli)
            || self
                .smoothing_bins_milli
                .iter()
                .any(|x| !(1..=16_000).contains(x))
            || !self.smoothing_bins_milli.windows(2).all(|w| w[0] > w[1])
            || self.initial_step_milli == 0
            || self.initial_step_milli > self.max_step_milli
            || self.max_step_milli > 16_000
            || self.min_step_micros == 0
            || u64::from(self.min_step_micros) > 1000 * u64::from(self.initial_step_milli)
            || !(1..=16_000).contains(&self.max_move_bins_milli)
        {
            return Err(super::invalid("electrostatic numerical parameter"));
        }
        for (v, cap) in [
            (self.max_density_interactions, 1_000_000_000),
            (self.max_net_pin_evaluations, 1_000_000_000),
            (self.max_transform_terms, 1_000_000_000),
            (self.max_capacity_interactions, 64_000_000),
            (self.max_work, 20_000_000_000),
        ] {
            if v == 0 || v > cap {
                return Err(super::invalid("electrostatic cumulative limit"));
            }
        }
        Ok(())
    }
}
/// Actual bounded numerical work; no count implies a physical signoff verdict.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Counters {
    pub iterations: usize,
    pub evaluations: usize,
    pub line_search_trials: usize,
    pub backtracks: usize,
    pub momentum_restarts: usize,
    pub secant_skips: usize,
    pub pin_evaluations: u64,
    pub density_interactions: u64,
    pub transform_terms: u64,
    pub capacity_interactions: u64,
}
/// One stage has fixed smoothing and density weight.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StageReport {
    pub gamma_normalized: f64,
    pub density_weight: f64,
    pub wire_gradient_norm: f64,
    pub density_gradient_norm: f64,
}
/// Prototype outcome, including explicit fallback and numerical diagnostics.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ElectrostaticReport {
    pub status: String,
    pub reason: String,
    pub termination: String,
    pub workers: usize,
    pub numerical_fillers: usize,
    pub grid: [usize; 2],
    pub state_estimate_bytes: Option<u64>,
    pub state_limit_bytes: u64,
    pub baseline_work: u64,
    pub numerical_and_candidate_work: u64,
    pub baseline_hpwl_dbu: u64,
    pub baseline_weighted_hpwl_milli: Option<u64>,
    pub candidate_hpwl_dbu: Option<u64>,
    pub candidate_weighted_hpwl_milli: Option<u64>,
    pub maximum_mass_residual: Option<f64>,
    pub maximum_poisson_residual: Option<f64>,
    pub counts: Counters,
    pub stages: Vec<StageReport>,
}
#[derive(Clone, Copy, Debug)]
enum Failure {
    Budget(&'static str),
    Numerical(&'static str),
    NoStep(&'static str),
}
impl Failure {
    fn status(self) -> &'static str {
        match self {
            Self::Budget(_) => "fallback_budget",
            Self::Numerical(_) => "fallback_numerical",
            Self::NoStep(_) => "fallback_line_search",
        }
    }
    fn reason(self) -> &'static str {
        match self {
            Self::Budget(s) | Self::Numerical(s) | Self::NoStep(s) => s,
        }
    }
}
struct Meter<'a> {
    config: &'a ElectrostaticConfig,
    counts: &'a mut Counters,
    work: &'a mut Work,
    start_work: u64,
    current_interactions: usize,
}
impl Meter<'_> {
    fn work(&mut self, n: u64) -> std::result::Result<(), Failure> {
        if n > self
            .config
            .max_work
            .saturating_sub(self.work.used.saturating_sub(self.start_work))
            || n > self.work.max.saturating_sub(self.work.used)
        {
            return Err(Failure::Budget("max_work"));
        }
        self.work.add(n).map_err(|_| Failure::Budget("max_work"))
    }
    fn evaluation(&mut self) -> std::result::Result<(), Failure> {
        if self.counts.evaluations >= self.config.max_evaluations {
            return Err(Failure::Budget("max_evaluations"));
        }
        self.work(1)?;
        self.counts.evaluations += 1;
        self.current_interactions = 0;
        Ok(())
    }
    fn pins(&mut self, n: u64) -> std::result::Result<(), Failure> {
        if n > self
            .config
            .max_net_pin_evaluations
            .saturating_sub(self.counts.pin_evaluations)
        {
            return Err(Failure::Budget("max_net_pin_evaluations"));
        }
        self.work(n)?;
        self.counts.pin_evaluations += n;
        Ok(())
    }
    fn transforms(&mut self, n: u64) -> std::result::Result<(), Failure> {
        if n > self
            .config
            .max_transform_terms
            .saturating_sub(self.counts.transform_terms)
        {
            return Err(Failure::Budget("max_transform_terms"));
        }
        self.work(n)?;
        self.counts.transform_terms += n;
        Ok(())
    }
    fn capacity(&mut self, n: u64) -> std::result::Result<(), Failure> {
        if n > self
            .config
            .max_capacity_interactions
            .saturating_sub(self.counts.capacity_interactions)
        {
            return Err(Failure::Budget("max_capacity_interactions"));
        }
        self.work(n)?;
        self.counts.capacity_interactions += n;
        Ok(())
    }
    fn interactions(&mut self, n: usize) -> std::result::Result<(), Failure> {
        if n > self.config.max_bins_per_object
            || n > self
                .config
                .max_interactions_per_evaluation
                .saturating_sub(self.current_interactions)
            || n as u64
                > self
                    .config
                    .max_density_interactions
                    .saturating_sub(self.counts.density_interactions)
        {
            return Err(Failure::Budget("density_interactions"));
        }
        self.work(n as u64)?;
        self.current_interactions += n;
        self.counts.density_interactions += n as u64;
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct Domain {
    origin: [i64; 2],
    size: [i64; 2],
    scale: i64,
    extent: [f64; 2],
}
impl Domain {
    fn new(region: Rect) -> std::result::Result<Self, Failure> {
        let (w, h) = (
            i128::from(region.ux) - i128::from(region.lx),
            i128::from(region.uy) - i128::from(region.ly),
        );
        if w <= 0 || h <= 0 || w > 1i128 << 52 || h > 1i128 << 52 {
            return Err(Failure::Numerical("unsupported_exact_core_extent"));
        }
        let size = [w as i64, h as i64];
        let scale = size[0].max(size[1]);
        Ok(Self {
            origin: [region.lx, region.ly],
            size,
            scale,
            extent: [size[0] as f64 / scale as f64, size[1] as f64 / scale as f64],
        })
    }
    fn relative(&self, p: [i64; 2]) -> std::result::Result<[i64; 2], Failure> {
        let delta = [
            i128::from(p[0]) - i128::from(self.origin[0]),
            i128::from(p[1]) - i128::from(self.origin[1]),
        ];
        if delta.iter().any(|x| x.abs() > 1i128 << 52) {
            return Err(Failure::Numerical("unsupported_exact_coordinate"));
        }
        Ok([delta[0] as i64, delta[1] as i64])
    }
}
struct Model<'a> {
    problem: &'a PlaceProblem,
    domain: Domain,
    real: usize,
    fillers: usize,
    bodies: Vec<density::Body>,
    degree: Vec<f64>,
    max_net_pins: usize,
    modeled_pins: usize,
    total_pins: usize,
    target_density: f64,
    workers: usize,
}
impl Model<'_> {
    fn objects(&self) -> usize {
        self.real + self.fillers
    }
    fn shape(&self, i: usize) -> density::Body {
        self.bodies[i]
    }
    fn project(&self, i: usize, p: [f64; 2]) -> std::result::Result<[f64; 2], Failure> {
        if p.iter().any(|x| !x.is_finite()) {
            return Err(Failure::Numerical("nonfinite_position"));
        }
        let density::Body {
            width: w,
            height: h,
            ..
        } = self.shape(i);
        let lo = [w / 2.0, h / 2.0];
        let hi = [
            self.domain.extent[0] - w / 2.0,
            self.domain.extent[1] - h / 2.0,
        ];
        if lo[0] > hi[0] || lo[1] > hi[1] {
            return Err(Failure::Numerical("object_larger_than_core"));
        }
        Ok([p[0].clamp(lo[0], hi[0]), p[1].clamp(lo[1], hi[1])])
    }
    fn pin(&self, pin: PinRef, positions: &[[f64; 2]]) -> std::result::Result<[f64; 2], Failure> {
        match pin {
            PinRef::Cell(c) => positions
                .get(c as usize)
                .copied()
                .ok_or(Failure::Numerical("pin_index")),
            PinRef::Fixed(p) => {
                let p = self.domain.relative(p)?;
                let scale = self.domain.scale as f64;
                Ok([p[0] as f64 / scale, p[1] as f64 / scale])
            }
        }
    }
    fn lower_left(&self, positions: &[[f64; 2]]) -> std::result::Result<Vec<[i64; 2]>, Failure> {
        let scale = self.domain.scale as f64;
        positions[..self.real]
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let lo = [
                    p[0] * scale - self.problem.widths[i] as f64 / 2.0,
                    p[1] * scale - self.problem.row_height as f64 / 2.0,
                ];
                if lo
                    .iter()
                    .any(|x| !x.is_finite() || x.abs() > (1u64 << 52) as f64)
                {
                    return Err(Failure::Numerical("target_conversion"));
                }
                Ok([lo[0].round() as i64, lo[1].round() as i64])
            })
            .collect()
    }
}
fn translated(
    problem: &PlaceProblem,
    domain: Domain,
) -> std::result::Result<PlaceProblem, Failure> {
    let intervals = problem
        .intervals
        .iter()
        .map(|iv| {
            let lo = domain.relative([iv.lx, iv.y])?;
            let hi = domain.relative([iv.ux, iv.y])?;
            Ok(Interval {
                lx: lo[0],
                ux: hi[0],
                y: lo[1],
            })
        })
        .collect::<std::result::Result<Vec<_>, Failure>>()?;
    let nets = problem
        .nets
        .iter()
        .map(|net| {
            net.iter()
                .map(|pin| match *pin {
                    PinRef::Cell(c) => Ok(PinRef::Cell(c)),
                    PinRef::Fixed(p) => Ok(PinRef::Fixed(domain.relative(p)?)),
                })
                .collect()
        })
        .collect::<std::result::Result<Vec<_>, Failure>>()?;
    Ok(PlaceProblem {
        widths: problem.widths.clone(),
        row_height: problem.row_height,
        site: problem.site,
        gap: problem.gap,
        weights: problem.weights.clone(),
        nets,
        intervals,
    })
}
fn exact_objective(
    problem: &PlaceProblem,
    positions: &[[i64; 2]],
) -> std::result::Result<(u64, u64), Failure> {
    let (mut raw, mut weighted) = (0u128, 0u128);
    for (k, net) in problem.nets.iter().enumerate() {
        if net.len() < 2 {
            continue;
        }
        let (mut lo, mut hi) = ([i128::MAX; 2], [i128::MIN; 2]);
        for pin in net {
            let p = match *pin {
                PinRef::Cell(c) => {
                    let c = c as usize;
                    [
                        i128::from(positions[c][0]) + i128::from(problem.widths[c] / 2),
                        i128::from(positions[c][1]) + i128::from(problem.row_height / 2),
                    ]
                }
                PinRef::Fixed(p) => [i128::from(p[0]), i128::from(p[1])],
            };
            for d in 0..2 {
                lo[d] = lo[d].min(p[d]);
                hi[d] = hi[d].max(p[d]);
            }
        }
        let length = ((hi[0] - lo[0]) + (hi[1] - lo[1])) as u128;
        raw += length;
        weighted += length * u128::from(problem.weight(k));
    }
    Ok((
        u64::try_from(raw).map_err(|_| Failure::Numerical("hpwl_range"))?,
        u64::try_from(weighted).map_err(|_| Failure::Numerical("weighted_hpwl_range"))?,
    ))
}

fn estimate(
    problem: &PlaceProblem,
    cfg: &ElectrostaticConfig,
) -> std::result::Result<(u64, usize), Failure> {
    let n = problem.widths.len();
    if n > cfg.max_cells {
        return Err(Failure::Budget("max_cells"));
    }
    if problem.nets.len() > cfg.max_nets {
        return Err(Failure::Budget("max_nets"));
    }
    if problem.intervals.len() > cfg.max_intervals {
        return Err(Failure::Budget("max_intervals"));
    }
    let pins = problem
        .nets
        .iter()
        .try_fold(0usize, |n, net| n.checked_add(net.len()))
        .ok_or(Failure::Budget("max_pins"))?;
    if pins > cfg.max_pins {
        return Err(Failure::Budget("max_pins"));
    }
    // Includes the retained baseline, translated integer view, checkpoint and
    // all coordinate/gradient histories. No allocation scales as cells*bins.
    let bytes = 256 * (n + cfg.max_fillers) as u64
        + 64 * pins as u64
        + 96 * problem.nets.len() as u64
        + 112 * problem.intervals.len() as u64
        + (1 << 20);
    if bytes > cfg.max_state_mib * (1 << 20) {
        return Err(Failure::Budget("max_state_mib"));
    }
    Ok((bytes, pins))
}
pub(super) fn interval_region(problem: &PlaceProblem) -> Result<Rect> {
    let first = problem
        .intervals
        .first()
        .ok_or_else(|| super::invalid("electrostatic empty intervals"))?;
    let (mut lx, mut ux, mut ly, mut uy) = (
        first.lx,
        first.ux,
        first.y,
        i128::from(first.y) + i128::from(problem.row_height),
    );
    for v in &problem.intervals {
        lx = lx.min(v.lx);
        ux = ux.max(v.ux);
        ly = ly.min(v.y);
        uy = uy.max(i128::from(v.y) + i128::from(problem.row_height));
    }
    Ok(Rect {
        lx,
        ux,
        ly,
        uy: i64::try_from(uy).map_err(|_| super::invalid("electrostatic core extent"))?,
    })
}
fn prepare_model<'a>(
    problem: &'a PlaceProblem,
    config: &PlaceConfig,
    domain: Domain,
    pins: usize,
    workers: usize,
) -> std::result::Result<Model<'a>, Failure> {
    let cfg = config
        .electrostatic
        .as_ref()
        .ok_or(Failure::Numerical("missing_mode"))?;
    let n = problem.widths.len();
    let scale = domain.scale as f64;
    let bodies: Vec<_> = problem
        .widths
        .iter()
        .map(|w| density::Body {
            width: *w as f64 / scale,
            height: problem.row_height as f64 / scale,
            area: (i128::from(*w) * i128::from(problem.row_height)) as f64 / (scale * scale),
        })
        .collect();
    if bodies.iter().any(|a| !a.area.is_finite() || a.area <= 0.0) {
        return Err(Failure::Numerical("cell_area"));
    }
    let mut degree = vec![0.0; n + cfg.max_fillers];
    let mut modeled_pins = 0;
    for (k, net) in problem.nets.iter().enumerate() {
        if net.len() < 2 || net.len() > config.max_net_pins {
            continue;
        }
        modeled_pins += net.len();
        let weight = f64::from(problem.weight(k)) / 1000.0;
        for pin in net {
            if let PinRef::Cell(c) = *pin {
                degree[c as usize] += weight;
            }
        }
    }
    Ok(Model {
        problem,
        domain,
        real: n,
        fillers: 0,
        bodies,
        degree,
        max_net_pins: config.max_net_pins.min(pins),
        modeled_pins,
        total_pins: pins,
        target_density: f64::from(cfg.target_density_milli) / 1000.0,
        workers,
    })
}
fn initial_positions(
    model: &mut Model<'_>,
    baseline: &[[i64; 2]],
    density: &density::Density,
    cfg: &ElectrostaticConfig,
) -> std::result::Result<Vec<[f64; 2]>, Failure> {
    let scale = model.domain.scale as f64;
    let mut positions = Vec::with_capacity(model.real + cfg.max_fillers);
    for (i, p) in baseline.iter().enumerate() {
        positions.push(model.project(
            i,
            [
                (p[0] as f64 + model.problem.widths[i] as f64 / 2.0) / scale,
                (p[1] as f64 + model.problem.row_height as f64 / 2.0) / scale,
            ],
        )?);
    }
    let free = density.capacity.iter().sum::<f64>();
    let real = model.bodies.iter().map(|b| b.area).sum::<f64>();
    let slack = (model.target_density * free - real).max(0.0);
    if !slack.is_finite() {
        return Err(Failure::Numerical("filler_charge"));
    }
    if slack > 1e-20 {
        if cfg.max_fillers == 0 {
            return Err(Failure::Budget("max_fillers"));
        }
        model.fillers = cfg.max_fillers;
        let area = slack / model.fillers as f64;
        let fraction = (area / (model.domain.extent[0] * model.domain.extent[1])).sqrt();
        let body = density::Body {
            width: model.domain.extent[0] * fraction,
            height: model.domain.extent[1] * fraction,
            area,
        };
        model
            .bodies
            .try_reserve_exact(model.fillers)
            .map_err(|_| Failure::Budget("filler_allocation"))?;
        model.bodies.resize(model.real + model.fillers, body);
        let (mut bin, mut before) = (0usize, 0.0);
        for k in 0..model.fillers {
            let target = (k as f64 + 0.5) * free / model.fillers as f64;
            while bin + 1 < density.capacity.len() && before + density.capacity[bin] < target {
                before += density.capacity[bin];
                bin += 1;
            }
            if density.capacity[bin] <= 0.0 {
                return Err(Failure::Numerical("filler_capacity"));
            }
            positions.push(model.project(
                model.real + k,
                [
                    (bin / density.grid.ny) as f64 * density.grid.hx + density.grid.hx / 2.0,
                    (bin % density.grid.ny) as f64 * density.grid.hy + density.grid.hy / 2.0,
                ],
            )?);
        }
    }
    Ok(positions)
}
fn admit_evaluations(
    model: &Model<'_>,
    density: &density::Density,
    cfg: &ElectrostaticConfig,
    meter: &mut Meter<'_>,
) -> std::result::Result<(), Failure> {
    let mut interactions = 0u64;
    meter.work(model.objects() as u64)?;
    for i in 0..model.objects() {
        let density::Body {
            width: w,
            height: h,
            ..
        } = model.shape(i);
        let n = density::interaction_bound(w, h, &density.grid);
        if n > cfg.max_bins_per_object {
            return Err(Failure::Budget("max_bins_per_object"));
        }
        interactions = interactions
            .checked_add(n as u64 * 2)
            .ok_or(Failure::Budget("interaction_bound"))?;
    }
    if interactions > cfg.max_interactions_per_evaluation as u64 {
        return Err(Failure::Budget("max_interactions_per_evaluation"));
    }
    let evals = cfg.max_evaluations as u64;
    let pin_bound = (model.modeled_pins as u64)
        .saturating_mul(evals)
        .saturating_add((model.total_pins as u64).saturating_mul(cfg.max_iterations as u64 + 2));
    let density_bound = interactions.saturating_mul(evals);
    let transform_bound = density.grid.terms().saturating_mul(evals);
    if pin_bound
        > cfg
            .max_net_pin_evaluations
            .saturating_sub(meter.counts.pin_evaluations)
    {
        return Err(Failure::Budget("requested_net_pin_evaluations"));
    }
    if density_bound
        > cfg
            .max_density_interactions
            .saturating_sub(meter.counts.density_interactions)
    {
        return Err(Failure::Budget("requested_density_interactions"));
    }
    if transform_bound
        > cfg
            .max_transform_terms
            .saturating_sub(meter.counts.transform_terms)
    {
        return Err(Failure::Budget("requested_transform_terms"));
    }
    let work = pin_bound
        .saturating_add(density_bound)
        .saturating_add(transform_bound)
        .saturating_add(evals.saturating_mul(density.grid.bins() as u64 * 10))
        .saturating_add((cfg.max_iterations as u64).saturating_mul(model.objects() as u64 * 20));
    if work
        > cfg
            .max_work
            .saturating_sub(meter.work.used.saturating_sub(meter.start_work))
        || work > meter.work.max.saturating_sub(meter.work.used)
    {
        return Err(Failure::Budget("requested_evaluation_work"));
    }
    Ok(())
}
fn legal_error(error: Error) -> Failure {
    match error {
        Error::Budget(_) => Failure::Budget("candidate_legalizer_work"),
        _ => Failure::NoStep("candidate_legalizer"),
    }
}
pub(super) fn refine(
    problem: &PlaceProblem,
    config: &PlaceConfig,
    limits: &PlaceLimits,
    mut baseline: PlaceResult,
    region: Option<Rect>,
) -> Result<PlaceResult> {
    let cfg = config
        .electrostatic
        .as_ref()
        .ok_or_else(|| super::invalid("missing electrostatic config"))?;
    let baseline_work = baseline.work;
    let mut report = ElectrostaticReport {
        status: "fallback".into(),
        reason: String::new(),
        termination: String::new(),
        workers: 0,
        numerical_fillers: 0,
        grid: [64, 64],
        state_estimate_bytes: None,
        state_limit_bytes: cfg.max_state_mib * (1 << 20),
        baseline_work,
        numerical_and_candidate_work: 0,
        baseline_hpwl_dbu: baseline.hpwl,
        baseline_weighted_hpwl_milli: None,
        candidate_hpwl_dbu: None,
        candidate_weighted_hpwl_milli: None,
        maximum_mass_residual: None,
        maximum_poisson_residual: None,
        counts: Counters::default(),
        stages: Vec::new(),
    };
    if problem.widths.is_empty() {
        report.status = "not_applied_empty".into();
        report.reason = "no_movable_cells".into();
        report.termination = "empty".into();
        baseline.electrostatic = Some(report);
        return Ok(baseline);
    }
    let mut counts = Counters::default();
    let mut work = Work {
        used: baseline_work,
        // Common retarget/legalizer/detail helpers charge this same counter.
        // Their work must obey the configured additional cap as well.
        max: refinement_work_limit(limits.max_work, baseline_work, cfg.max_work),
    };
    let mut accepted = None;
    let attempt = (|| -> std::result::Result<(), Failure> {
        let mut meter = Meter {
            config: cfg,
            counts: &mut counts,
            work: &mut work,
            start_work: baseline_work,
            current_interactions: 0,
        };
        meter.work(problem.nets.len() as u64)?;
        let (bytes, pins) = estimate(problem, cfg)?;
        report.state_estimate_bytes = Some(bytes);
        let region = match region {
            Some(r) => r,
            None => interval_region(problem).map_err(|_| Failure::Numerical("core_extent"))?,
        };
        let domain = Domain::new(region)?;
        meter.work(
            (pins + problem.widths.len() + problem.nets.len() + problem.intervals.len()) as u64,
        )?;
        let shifted = translated(problem, domain)?;
        let relative_domain = Domain {
            origin: [0, 0],
            ..domain
        };
        let workers = cfg.workers.min(rayon::current_num_threads());
        report.workers = workers;
        let local_baseline = baseline
            .positions
            .iter()
            .map(|p| domain.relative(*p))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        meter.pins(pins as u64)?;
        meter.work((problem.widths.len() + problem.nets.len()) as u64)?;
        let mut model = prepare_model(&shifted, config, relative_domain, pins, workers)?;
        meter.pins(pins as u64)?;
        let base_cost = exact_objective(&shifted, &local_baseline)?;
        report.baseline_hpwl_dbu = base_cost.0;
        report.baseline_weighted_hpwl_milli = Some(base_cost.1);
        let mut density = density::Density::new(&model, &mut meter)?;
        meter.work((problem.widths.len() + cfg.max_fillers + 4096) as u64)?;
        let initial = initial_positions(&mut model, &local_baseline, &density, cfg)?;
        if model.lower_left(&initial)? != local_baseline {
            return Err(Failure::Numerical("normalization_roundtrip"));
        }
        drop(local_baseline);
        report.numerical_fillers = model.fillers;
        admit_evaluations(&model, &density, cfg, &mut meter)?;
        let outcome = optimizer::run(&model, &mut density, initial, cfg, &mut meter, &mut report)?;
        report.termination = outcome.termination;
        let scale = model.domain.scale as f64;
        let mut targets: Vec<[f64; 2]> = outcome.positions[..model.real]
            .iter()
            .map(|p| [p[0] * scale, p[1] * scale])
            .collect();
        drop(density);
        let mut anchored = 0;
        if config.retarget_isolated {
            let anchors = super::isolated_targets(&shifted, config.max_net_pins, meter.work)
                .map_err(legal_error)?;
            for (t, a) in targets.iter_mut().zip(anchors) {
                if let Some(a) = a {
                    *t = a;
                    anchored += 1;
                }
            }
        }
        let global: Vec<[i64; 2]> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                [
                    (t[0] - shifted.widths[i] as f64 / 2.0).round() as i64,
                    (t[1] - shifted.row_height as f64 / 2.0).round() as i64,
                ]
            })
            .collect();
        meter.pins(pins as u64)?;
        let global_hpwl = exact_objective(&shifted, &global)?.0;
        let mut legal =
            super::legalize(&shifted, &targets, None, meter.work).map_err(legal_error)?;
        super::detailed(
            &shifted,
            &mut legal,
            config.detailed_passes,
            config.max_net_pins,
            None,
            meter.work,
        )
        .map_err(legal_error)?;
        super::check_legal(&shifted, &legal).map_err(legal_error)?;
        meter.pins(pins as u64)?;
        let candidate_cost = exact_objective(&shifted, &legal)?;
        report.candidate_hpwl_dbu = Some(candidate_cost.0);
        report.candidate_weighted_hpwl_milli = Some(candidate_cost.1);
        if candidate_cost.0 > base_cost.0 || candidate_cost.1 > base_cost.1 {
            report.status = "fallback_no_improvement".into();
            report.reason = "post_legalization_exact_objective".into();
            return Ok(());
        }
        for p in &mut legal {
            for (d, value) in p.iter_mut().enumerate() {
                *value = i64::try_from(i128::from(*value) + i128::from(domain.origin[d]))
                    .map_err(|_| Failure::Numerical("published_coordinate_range"))?;
            }
        }
        super::check_legal(problem, &legal).map_err(legal_error)?;
        accepted = Some((legal, candidate_cost.0, global_hpwl, anchored));
        report.status = "accepted_no_worse".into();
        report.reason = "exact_integer_objective_gate".into();
        Ok(())
    })();
    if let Err(failure) = attempt {
        report.status = failure.status().into();
        report.reason = failure.reason().into();
        report.termination = failure.reason().into();
    }
    report.counts = counts;
    report.numerical_and_candidate_work = work.used.saturating_sub(baseline_work);
    baseline.work = work.used;
    if let Some((positions, hpwl, global_hpwl, anchored)) = accepted {
        baseline.positions = positions;
        baseline.hpwl = hpwl;
        baseline.global_hpwl = global_hpwl;
        baseline.anchored = anchored;
    }
    baseline.electrostatic = Some(report);
    Ok(baseline)
}
pub(super) fn skipped_seeded(
    baseline: &PlaceResult,
    cfg: &ElectrostaticConfig,
) -> ElectrostaticReport {
    ElectrostaticReport {
        status: "not_applied_seeded".into(),
        reason: "P1_is_unseeded_only".into(),
        termination: "seeded_path_unchanged".into(),
        workers: 0,
        numerical_fillers: 0,
        grid: [64, 64],
        state_estimate_bytes: None,
        state_limit_bytes: cfg.max_state_mib * (1 << 20),
        baseline_work: baseline.work,
        numerical_and_candidate_work: 0,
        baseline_hpwl_dbu: baseline.hpwl,
        baseline_weighted_hpwl_milli: None,
        candidate_hpwl_dbu: None,
        candidate_weighted_hpwl_milli: None,
        maximum_mass_residual: None,
        maximum_poisson_residual: None,
        counts: Counters::default(),
        stages: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
