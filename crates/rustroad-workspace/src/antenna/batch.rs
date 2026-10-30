//! Ordered selected checks sharing one locked snapshot and prepared LEF model.
use super::{
    AnalysisLimits, AntennaRequest, check_budgets, check_prepared, prepare, source_fingerprint,
    validate_selection,
};
use crate::{Error, Result, SnapshotCaps};
use rustroad_core::Limits;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;

/// A bounded same-revision selection sequence; each group keeps legacy check limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckBatchPlan {
    pub schema: String,
    /// Required snapshot identity, checked under the shared workspace lock.
    pub revision: String,
    /// Canonical, distinct names within each group; group order is preserved.
    pub groups: Vec<Vec<String>>,
    pub analysis: AnalysisLimits,
    pub safety_margin: f64,
    /// Aggregate measured check work; each next check reserves its full work bound.
    pub max_work: usize,
    /// Aggregate serialized JSON-array bytes, including framing and final newline.
    pub max_output_bytes: u64,
    /// Snapshot admission bound, independent of the per-group 128 MiB report limit.
    pub snapshot_memory_bytes: u64,
}
impl CheckBatchPlan {
    /// Create a bounded plan with the native grouped-runner's check and work limits.
    #[must_use]
    pub fn new(revision: String, groups: Vec<Vec<String>>) -> Self {
        Self {
            schema: "rustroad.antenna-check-batch.v1".into(),
            revision,
            groups,
            analysis: AnalysisLimits::from(&AntennaRequest::default()),
            safety_margin: super::DEFAULT_SAFETY_MARGIN,
            max_work: 200_000_000_000,
            max_output_bytes: 8 << 30,
            snapshot_memory_bytes: 2 << 30,
        }
    }

    fn request(&self) -> Result<AntennaRequest> {
        if self.schema != "rustroad.antenna-check-batch.v1"
            || self.groups.is_empty()
            || self.groups.len() > 1024
            || !(1..=200_000_000_000).contains(&self.max_work)
            || !(3..=8 << 30).contains(&self.max_output_bytes)
            || !(1..=4 << 30).contains(&self.snapshot_memory_bytes)
            || self.analysis.report_memory_bytes != super::MAX_REPORT_MEMORY_BYTES
        {
            return Err(Error::Budget("ANTENNA_BATCH: invalid plan bounds".into()));
        }
        let mut count = 0usize;
        let mut bytes = 0usize;
        for group in &self.groups {
            if group.len() > 128 || group.windows(2).any(|names| names[0] >= names[1]) {
                return Err(Error::Budget(
                    "ANTENNA_BATCH: canonical group/name bound".into(),
                ));
            }
            validate_selection(group)?;
            count = count.saturating_add(group.len());
            bytes = group
                .iter()
                .fold(bytes, |total, name| total.saturating_add(name.len()));
            if count > 100_000 || bytes > 16 << 20 {
                return Err(Error::Budget("ANTENNA_BATCH: aggregate names".into()));
            }
        }
        let a = &self.analysis;
        let request = AntennaRequest {
            max_shapes_per_net: a.max_shapes_per_net,
            max_total_shapes: a.max_total_shapes,
            max_pair_checks: a.max_pair_checks,
            max_union_cells: a.max_union_cells,
            max_rows: a.max_rows,
            max_work: a.max_work,
            safety_margin: self.safety_margin,
            ..AntennaRequest::default()
        };
        check_budgets(&request)?;
        Ok(request)
    }
}

/// Completed batch accounting; native checks retain their original scope/status fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CheckBatchSummary {
    pub revision: String,
    pub groups: usize,
    pub work: usize,
    pub output_bytes: u64,
}

/// Read and validate a capped plan before decoding a workspace or emitting reports.
///
/// # Errors
/// Fails on malformed input, a plan larger than 16 MiB, or any hard group/byte bound.
pub fn read_check_batch_plan(path: &Path) -> Result<CheckBatchPlan> {
    let (text, _) = crate::pdk::read_input(path, 16 << 20, "antenna_batch_plan")?;
    let plan: CheckBatchPlan = serde_json::from_str(&text)?;
    plan.request()?;
    Ok(plan)
}

struct CountBytes {
    bytes: u64,
    limit: u64,
}
impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .filter(|&n| n <= self.limit)
            .ok_or_else(|| io::Error::other("ANTENNA_BATCH: output byte budget"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stream ordered legacy check reports from one prepared, shared-locked revision.
///
/// A single report is retained at a time. Per-report statistics and JSON values
/// match separate selected checks; source fingerprints are cached for this immutable
/// snapshot. The shared lock remains held through the final writer flush. No revision
/// is created. A failure may leave an incomplete JSON array; callers must discard it
/// unless this function succeeds. Output bytes are counted before each report is sent.
///
/// # Errors
/// Fails on revision conflict, stale sources, unsupported or absent selected identities,
/// malformed snapshots, per-group/aggregate work or memory bounds, and writer errors.
pub fn check_antenna_batch(
    workspace: &Path,
    plan: &CheckBatchPlan,
    limits: &Limits,
    output: &mut impl Write,
) -> Result<CheckBatchSummary> {
    let mut request = plan.request()?;
    let lock = crate::lock(workspace, false, false)?;
    crate::check_base(&plan.revision, &crate::head(workspace)?)?;
    let snapshot = crate::load_capped(
        workspace,
        lock,
        limits,
        Some(SnapshotCaps {
            route_bytes: 256 << 20,
            route_records: 4_194_304,
            routed_ids: 1_000_000,
            memory_bytes: plan.snapshot_memory_bytes,
            inventory_bytes: 64 << 20,
        }),
    )?;
    let metadata = snapshot.metadata();
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| super::fail("NO_PHYSICAL", "import a PDK design first"))?;
    let state = bundle
        .routing
        .as_ref()
        .ok_or_else(|| super::fail("NO_ROUTING", "route before the antenna check"))?;
    let placed: Vec<_> = snapshot.records().collect();
    let prep = prepare(&metadata.design, bundle, &placed, state)?;
    let source = source_fingerprint(
        &metadata.design,
        &bundle.connectivity,
        &placed,
        state,
        &prep.sources,
    )?;
    let mut work = 0usize;
    // Reserve the array brackets and final newline before any output.
    let mut counted = CountBytes {
        bytes: 3,
        limit: plan.max_output_bytes,
    };
    output.write_all(b"[")?;
    for (index, names) in plan.groups.iter().enumerate() {
        if request.max_work > plan.max_work.saturating_sub(work) {
            return Err(Error::Budget(
                "ANTENNA_BATCH: aggregate work admission".into(),
            ));
        }
        request.selected_nets = Some(names.clone());
        let report = check_prepared(metadata, &placed, &prep, &request, Some(&source))?;
        work = work
            .checked_add(report.stats.work)
            .filter(|&n| n <= plan.max_work)
            .ok_or_else(|| Error::Budget("ANTENNA_BATCH: aggregate work".into()))?;
        if index > 0 {
            counted.write_all(b",")?;
        }
        serde_json::to_writer(&mut counted, &report).map_err(|error| {
            if error.is_io() {
                Error::Budget("ANTENNA_BATCH: output byte budget".into())
            } else {
                Error::Json(error)
            }
        })?;
        if index > 0 {
            output.write_all(b",")?;
        }
        serde_json::to_writer(&mut *output, &report)?;
    }
    output.write_all(b"]\n")?;
    output.flush()?;
    Ok(CheckBatchSummary {
        revision: metadata.revision.clone(),
        groups: plan.groups.len(),
        work,
        output_bytes: counted.bytes,
    })
}
