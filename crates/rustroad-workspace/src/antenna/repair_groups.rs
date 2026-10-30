//! Bounded ordered repair rounds sharing one decoded and exclusively locked state.
use super::{
    AntennaReport, AntennaRequest, InventoryBounds, InventoryReport, NetObservation, Prepared,
    RepairReport, RepairRequest, RepairScope, check_prepared, inventory, prepare, repair_prepared,
    source_fingerprint,
};
use crate::{Error, Metadata, Result, SnapshotCaps};
use rustroad_core::{Limits, Placed, connectivity::PinOwner};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Hard-capped grouped-runner policy; smaller bounds support bounded experiments.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRepairPolicy {
    pub group_names: usize,
    pub max_groups: usize,
    pub max_attempts: usize,
    pub max_total_jumpers: usize,
    pub max_commands: usize,
    pub max_report_bytes: u64,
    pub max_cumulative_work: usize,
}
impl Default for GroupRepairPolicy {
    fn default() -> Self {
        Self {
            group_names: 128,
            max_groups: 1024,
            max_attempts: 64,
            max_total_jumpers: 4096,
            max_commands: 4096,
            max_report_bytes: 8 << 30,
            max_cumulative_work: 200_000_000_000,
        }
    }
}

/// A deterministic selected-repair plan with a fresh artifact directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRepairPlan {
    pub schema: String,
    pub output: PathBuf,
    pub max_rounds: usize,
    pub deadline_seconds: u64,
    pub policy: GroupRepairPolicy,
}
impl GroupRepairPlan {
    /// Use the frozen native grouped-runner's round, deadline and budget policy.
    #[must_use]
    pub fn new(output: PathBuf) -> Self {
        Self {
            schema: "rustroad.antenna-group-repair.v1".into(),
            output,
            max_rounds: 3,
            deadline_seconds: 1800,
            policy: GroupRepairPolicy::default(),
        }
    }
    fn validate(&self) -> Result<()> {
        let p = &self.policy;
        if self.schema != "rustroad.antenna-group-repair.v1"
            || self.output.as_os_str().is_empty()
            || !(1..=3).contains(&self.max_rounds)
            || !(1..=1800).contains(&self.deadline_seconds)
            || !(1..=128).contains(&p.group_names)
            || !(1..=1024).contains(&p.max_groups)
            || !(1..=64).contains(&p.max_attempts)
            || !(1..=4096).contains(&p.max_total_jumpers)
            || !(1..=4096).contains(&p.max_commands)
            || !(1..=8 << 30).contains(&p.max_report_bytes)
            || !(1..=200_000_000_000).contains(&p.max_cumulative_work)
        {
            return Err(Error::Budget("ANTENNA_GROUP_REPAIR: plan bounds".into()));
        }
        Ok(())
    }
}

/// Read a bounded orchestration plan before acquiring any workspace mutation lock.
///
/// # Errors
/// Fails on malformed JSON, input larger than 64 KiB, or bounds outside hard caps.
pub fn read_group_repair_plan(path: &Path) -> Result<GroupRepairPlan> {
    let (text, _) = crate::pdk::read_input(path, 64 << 10, "antenna_group_plan")?;
    let plan: GroupRepairPlan = serde_json::from_str(&text)?;
    plan.validate()?;
    Ok(plan)
}

/// One repair decision, preserving the canonical selected set and real ancestry.
#[derive(Clone, Debug, Serialize)]
pub struct GroupRepairAttempt {
    pub round: usize,
    pub group: usize,
    pub base_revision: String,
    pub revision: String,
    pub selected_nets: Vec<String>,
    pub jumpers: usize,
}

/// Compact native pre-screening receipt; foundry signoff remains separate.
#[derive(Clone, Debug, Serialize)]
pub struct GroupRepairSummary {
    pub schema: &'static str,
    pub status: &'static str,
    pub initial_revision: String,
    pub final_revision: String,
    pub scope: &'static str,
    pub route_scope: &'static str,
    pub inventory_status: &'static str,
    pub native_analysis_status: &'static str,
    pub whole_layout_antenna_status: &'static str,
    pub drc_status: &'static str,
    pub corner: &'static str,
    pub wire_model: &'static str,
    pub unconstrained_endpoints: Option<usize>,
    pub requires_full_revalidation: bool,
    pub attempts: Vec<GroupRepairAttempt>,
    pub groups: usize,
    pub observations: BTreeMap<String, usize>,
    pub excluded_macro_touch: Vec<String>,
    pub unresolved_supported_nets: Vec<String>,
    pub total_jumpers: usize,
    pub stop_reason: Option<&'static str>,
    pub skipped_futile_groups: usize,
    pub reported_work: usize,
    pub unreported_validation_work_upper_bound: usize,
    pub work_budget_charge: usize,
    pub report_bytes: u64,
    pub wall_seconds: f64,
}

struct Output {
    directory: PathBuf,
    bytes: u64,
    count: usize,
    max_bytes: u64,
    max_count: usize,
}
struct CappedWriter<W> {
    output: W,
    bytes: u64,
    cap: u64,
}
impl<W: Write> Write for CappedWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() as u64 > self.cap.saturating_sub(self.bytes) {
            return Err(io::Error::other("ANTENNA_GROUP_REPAIR: report byte budget"));
        }
        let written = self.output.write(data)?;
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
impl Output {
    fn write(&mut self, name: &str, value: &impl Serialize, pretty: bool) -> Result<PathBuf> {
        if self.count >= self.max_count {
            return Err(Error::Budget("ANTENNA_GROUP_REPAIR: report count".into()));
        }
        let path = self.directory.join(format!("{name}.json"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut output = CappedWriter {
            output: BufWriter::new(file),
            bytes: 0,
            cap: self.max_bytes.saturating_sub(self.bytes).min(
                if name.starts_with("inventory-") || name.starts_with("model-") {
                    64 << 20
                } else {
                    128 << 20
                },
            ),
        };
        let result = if pretty {
            serde_json::to_writer_pretty(&mut output, value)
        } else {
            serde_json::to_writer(&mut output, value)
        };
        result.map_err(|error| {
            if error.is_io() {
                Error::Budget("ANTENNA_GROUP_REPAIR: report byte budget".into())
            } else {
                Error::Json(error)
            }
        })?;
        output.write_all(b"\n")?;
        output.flush()?;
        self.bytes += output.bytes;
        self.count += 1;
        Ok(path)
    }
}

struct Session {
    path: PathBuf,
    _lock: File,
    metadata: Metadata,
    placed: Vec<Placed>,
    prep: Prepared,
    source: String,
}
impl Session {
    fn open(path: &Path, base_rev: &str, limits: &Limits) -> Result<Self> {
        let lock = crate::lock(path, false, true)?;
        crate::check_base(base_rev, &crate::head(path)?)?;
        let snapshot = crate::load_capped(
            path,
            lock,
            limits,
            Some(SnapshotCaps {
                route_bytes: 256 << 20,
                route_records: 4_194_304,
                routed_ids: 1_000_000,
                memory_bytes: 2 << 30,
                inventory_bytes: 64 << 20,
            }),
        )?;
        let placed: Vec<_> = snapshot.records().collect();
        let (_lock, metadata) = snapshot.into_metadata();
        let bundle = metadata
            .physical
            .as_ref()
            .ok_or_else(|| super::fail("NO_PHYSICAL", "import first"))?;
        let state = bundle
            .routing
            .as_ref()
            .ok_or_else(|| super::fail("NO_ROUTING", "route first"))?;
        if state.scope != crate::routing::RouteScope::All {
            return Err(super::fail(
                "ANTENNA_GROUP_REPAIR",
                "all-signal routing is required",
            ));
        }
        // Preparation depends on static placement/PDN/LEF/config, not signal routes.
        // The selected jumper worker changes only signal routes and revision fields.
        let prep = prepare(&metadata.design, bundle, &placed, state)?;
        let source = source_fingerprint(
            &metadata.design,
            &bundle.connectivity,
            &placed,
            state,
            &prep.sources,
        )?;
        Ok(Self {
            path: path.to_owned(),
            _lock,
            metadata,
            placed,
            prep,
            source,
        })
    }
    fn inventory(&self) -> Result<InventoryReport> {
        inventory::from_prepared(
            &self.metadata,
            &self.placed,
            &InventoryBounds::default(),
            Some(&self.source),
        )
    }
    fn check(&self, names: &[String]) -> Result<AntennaReport> {
        let request = AntennaRequest {
            selected_nets: Some(names.to_vec()),
            ..AntennaRequest::default()
        };
        check_prepared(
            &self.metadata,
            &self.placed,
            &self.prep,
            &request,
            Some(&self.source),
        )
    }
    fn repair(
        &mut self,
        names: &[String],
        report: PathBuf,
        remaining: usize,
    ) -> Result<RepairReport> {
        let base = self.metadata.revision.clone();
        let result = repair_prepared(
            &self.path,
            &base,
            &RepairRequest {
                selected_report: Some(report),
                scope: RepairScope::SelectedSupportedNets(names.to_vec()),
                max_jumpers: remaining,
                ..RepairRequest::default()
            },
            &mut self.metadata,
            &self.placed,
            &self.prep,
        )?;
        if result.revision != base {
            let bundle = self
                .metadata
                .physical
                .as_ref()
                .ok_or_else(|| super::fail("NO_PHYSICAL", "repair lost physical state"))?;
            let state = bundle
                .routing
                .as_ref()
                .ok_or_else(|| super::fail("NO_ROUTING", "repair lost routing state"))?;
            self.source = source_fingerprint(
                &self.metadata.design,
                &bundle.connectivity,
                &self.placed,
                state,
                &self.prep.sources,
            )?;
        }
        Ok(result)
    }
    fn model(&self) -> Result<&rustroad_core::connectivity::ConnectedDesign> {
        self.metadata
            .physical
            .as_ref()
            .map(|bundle| &bundle.connectivity)
            .ok_or_else(|| super::fail("NO_PHYSICAL", "missing physical state"))
    }
}

fn macro_touches(session: &Session) -> Result<BTreeSet<String>> {
    let model = session.model()?;
    if model.masters.len() > 4096
        || model.instance_masters.len() > 100_000
        || model.nets.len() > 100_000
        || model.pins.len() > 1_000_000
    {
        return Err(Error::Budget("ANTENNA_GROUP_REPAIR: model objects".into()));
    }
    let mut touches = BTreeSet::new();
    for pin in &model.pins {
        if let (Some(net), PinOwner::Instance { instance, .. }) = (pin.net, pin.owner)
            && model.masters[model.instance_masters[instance as usize] as usize]
                .class
                .as_deref()
                == Some("BLOCK")
        {
            touches.insert(model.nets[net as usize].name.clone());
        }
    }
    Ok(touches)
}

fn violating(report: &AntennaReport) -> BTreeSet<String> {
    report
        .nets
        .iter()
        .filter(|net| net.violated)
        .map(|net| net.net.clone())
        .collect()
}
fn eligible(report: &AntennaReport, touches: &BTreeSet<String>) -> Vec<String> {
    let violated = violating(report);
    report
        .selection
        .as_ref()
        .map_or_else(Vec::new, |selection| {
            selection
                .coverage
                .iter()
                .filter(|net| {
                    violated.contains(&net.net)
                        && !touches.contains(&net.net)
                        && !net.model_partial
                        && net.observation == NetObservation::Observed
                })
                .map(|net| net.net.clone())
                .collect()
        })
}
fn charge(work: &mut usize, add: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(add)
        .filter(|&n| n <= cap)
        .ok_or_else(|| Error::Budget("ANTENNA_GROUP_REPAIR: aggregate work".into()))?;
    Ok(())
}

/// Reproduce bounded grouped jumper decisions in one daemonless native process.
///
/// Preserve each real revision and all legacy per-operation checks and budgets.
/// Reports use the legacy formatting so selected-report input work stays identical.
/// The output directory must not exist. Failure preserves completed revisions and
/// partial evidence; only a successfully written summary completes the operation.
///
/// # Errors
/// Fails on a base-revision conflict, invalid plan, stale sources, model/geometry
/// limits, aggregate budgets, or I/O errors. Foundry signoff is never inferred.
pub fn repair_antenna_groups(
    workspace: &Path,
    base_rev: &str,
    plan: &GroupRepairPlan,
    limits: &Limits,
) -> Result<GroupRepairSummary> {
    plan.validate()?;
    let started = Instant::now();
    let mut session = Session::open(workspace, base_rev, limits)?;
    fs::create_dir(&plan.output)?;
    let p = &plan.policy;
    // Reserve bounded summary/selection metadata in the aggregate output budget.
    let mut output = Output {
        directory: plan.output.clone(),
        bytes: 0,
        count: 0,
        max_bytes: p.max_report_bytes.saturating_sub(16 << 20),
        max_count: p.max_commands,
    };
    let tick = Instant::now();
    let initial = session.inventory()?;
    output.write("inventory-initial", &initial, false)?;
    let inventory_seconds = tick.elapsed().as_secs_f64();
    let names: Vec<_> = initial
        .nets
        .iter()
        .map(|net| net.canonical_name.clone())
        .collect();
    if names.is_empty() {
        return Err(super::fail(
            "ANTENNA_GROUP_REPAIR",
            "empty routed inventory",
        ));
    }
    let tick = Instant::now();
    output.write("model-initial", session.model()?, true)?;
    let initial_model = super::report::fingerprint(session.model()?)?;
    let model_seconds = tick.elapsed().as_secs_f64();
    let touches = macro_touches(&session)?;
    let groups: Vec<_> = names.chunks(p.group_names).collect();
    if groups.len() > p.max_groups {
        return Err(Error::Budget("ANTENNA_GROUP_REPAIR: groups".into()));
    }
    let mut attempts = Vec::new();
    let mut total_jumpers = 0usize;
    let mut work = 0usize;
    let mut reported_work = 0usize;
    let mut validation_work = 0usize;
    let mut futile = BTreeSet::new();
    let mut skipped = 0usize;
    let mut stop = None;
    let mut check_seconds = 0.0f64;
    let mut attempt_seconds = 0.0f64;
    let per_work = AntennaRequest::default().max_work;
    let remaining = || plan.deadline_seconds as f64 - started.elapsed().as_secs_f64();
    let reserve = |seconds: f64| {
        1.5 * (inventory_seconds + model_seconds + groups.len() as f64 * seconds) + 60.0
    };
    for round in 0..plan.max_rounds {
        let mut changed = false;
        for (group, names) in groups.iter().enumerate() {
            if remaining() < reserve(check_seconds) + 1.5 * check_seconds {
                stop = Some("deadline_reserve");
                break;
            }
            if per_work > p.max_cumulative_work.saturating_sub(work) {
                return Err(Error::Budget(
                    "ANTENNA_GROUP_REPAIR: check work admission".into(),
                ));
            }
            let tick = Instant::now();
            let report = session.check(names)?;
            output.write(&format!("round-{round}-group-{group}"), &report, true)?;
            charge(&mut work, report.stats.work, p.max_cumulative_work)?;
            reported_work += report.stats.work;
            check_seconds = check_seconds.max(tick.elapsed().as_secs_f64());
            let selected = eligible(&report, &touches);
            drop(report);
            if selected.is_empty() {
                continue;
            }
            if futile.contains(&selected) {
                skipped += 1;
                continue;
            }
            if attempts.len() >= p.max_attempts {
                stop = Some("attempt_budget");
                break;
            }
            if total_jumpers >= p.max_total_jumpers {
                stop = Some("jumper_budget");
                break;
            }
            if 4 * per_work > p.max_cumulative_work.saturating_sub(work) {
                stop = Some("work_budget");
                break;
            }
            if remaining() < reserve(check_seconds) + attempt_seconds.max(2.0 * check_seconds) {
                stop = Some("deadline_reserve");
                break;
            }
            let tick = Instant::now();
            let before = session.check(&selected)?;
            if eligible(&before, &touches) != selected || violating(&before).len() != selected.len()
            {
                return Err(super::fail(
                    "ANTENNA_GROUP_REPAIR",
                    "fresh supported obligations changed",
                ));
            }
            let before_path =
                output.write(&format!("repair-{}-before", attempts.len()), &before, true)?;
            charge(&mut work, before.stats.work, p.max_cumulative_work)?;
            reported_work += before.stats.work;
            drop(before);
            if 4 * per_work > p.max_cumulative_work.saturating_sub(work) {
                return Err(Error::Budget(
                    "ANTENNA_GROUP_REPAIR: repair work admission".into(),
                ));
            }
            let base = session.metadata.revision.clone();
            let repair =
                session.repair(&selected, before_path, p.max_total_jumpers - total_jumpers)?;
            output.write(&format!("repair-{}", attempts.len()), &repair, true)?;
            let reported = repair
                .stats
                .antenna_work
                .checked_add(repair.stats.planner_work)
                .ok_or_else(|| {
                    Error::Budget("ANTENNA_GROUP_REPAIR: repair work overflow".into())
                })?;
            if repair.stats.antenna_work > per_work || repair.stats.planner_work > 2 * per_work {
                return Err(Error::Budget(
                    "ANTENNA_GROUP_REPAIR: repair work bounds".into(),
                ));
            }
            charge(&mut work, reported + per_work, p.max_cumulative_work)?;
            reported_work += reported;
            validation_work += per_work;
            total_jumpers += repair.jumpers.len();
            if repair.revision != base {
                changed = true;
            } else {
                futile.insert(selected.clone());
            }
            attempts.push(GroupRepairAttempt {
                round,
                group,
                base_revision: base,
                revision: repair.revision.clone(),
                selected_nets: selected,
                jumpers: repair.jumpers.len(),
            });
            attempt_seconds = attempt_seconds.max(tick.elapsed().as_secs_f64());
        }
        if stop.is_some() || !changed {
            break;
        }
    }
    let final_inventory = session.inventory()?;
    if final_inventory.nets != initial.nets
        || final_inventory.excluded_nonlogical_supply_tie_count
            != initial.excluded_nonlogical_supply_tie_count
    {
        return Err(super::fail(
            "ANTENNA_GROUP_REPAIR",
            "routed membership changed",
        ));
    }
    output.write("inventory-final", &final_inventory, false)?;
    if super::report::fingerprint(session.model()?)? != initial_model {
        return Err(super::fail(
            "ANTENNA_GROUP_REPAIR",
            "logical connectivity changed",
        ));
    }
    output.write("model-final", session.model()?, true)?;
    let mut violations = BTreeSet::new();
    let mut partial = false;
    let mut observations = BTreeMap::new();
    for (group, names) in groups.iter().enumerate() {
        if per_work > p.max_cumulative_work.saturating_sub(work) {
            return Err(Error::Budget(
                "ANTENNA_GROUP_REPAIR: final check work admission".into(),
            ));
        }
        let report = session.check(names)?;
        output.write(&format!("final-group-{group}"), &report, true)?;
        charge(&mut work, report.stats.work, p.max_cumulative_work)?;
        reported_work += report.stats.work;
        violations.extend(violating(&report));
        if let Some(selection) = &report.selection {
            partial |= !report.unsupported.is_empty() || report.status == "subset_partial";
            for net in &selection.coverage {
                partial |= net.model_partial || net.observation != NetObservation::Observed;
                let key = match net.observation {
                    NetObservation::Observed => "observed",
                    NetObservation::NoGeometry => "no_geometry",
                    NetObservation::NoGateArea => "no_gate_area",
                    NetObservation::NoApplicableRuleObservation => "no_applicable_rule_observation",
                };
                *observations.entry(key.to_owned()).or_insert(0) += 1;
            }
        }
    }
    if started.elapsed().as_secs() > plan.deadline_seconds {
        return Err(super::fail("ANTENNA_GROUP_REPAIR", "deadline exceeded"));
    }
    let unresolved: Vec<_> = violations.difference(&touches).cloned().collect();
    let summary = GroupRepairSummary {
        schema: "rustroad.antenna-group-repair-result.v1",
        status: if unresolved.is_empty() {
            "complete"
        } else {
            "unresolved"
        },
        initial_revision: base_rev.to_owned(),
        final_revision: session.metadata.revision.clone(),
        scope: "supported_standard_cell_subset",
        route_scope: "all_signal_nets",
        inventory_status: "complete",
        native_analysis_status: if partial {
            "partial"
        } else if violations.is_empty() {
            "clean"
        } else {
            "violated"
        },
        whole_layout_antenna_status: "unchecked",
        drc_status: "unchecked",
        corner: "not_applicable",
        wire_model: "none",
        unconstrained_endpoints: None,
        requires_full_revalidation: true,
        attempts,
        groups: groups.len(),
        observations,
        excluded_macro_touch: violations.intersection(&touches).cloned().collect(),
        unresolved_supported_nets: unresolved,
        total_jumpers,
        stop_reason: stop,
        skipped_futile_groups: skipped,
        reported_work,
        unreported_validation_work_upper_bound: validation_work,
        work_budget_charge: work,
        report_bytes: output.bytes,
        wall_seconds: started.elapsed().as_secs_f64(),
    };
    output.max_bytes = p.max_report_bytes;
    // Final lineage metadata does not consume the legacy operation counter.
    output.max_count += 1;
    output.write("summary", &summary, true)?;
    Ok(summary)
}
