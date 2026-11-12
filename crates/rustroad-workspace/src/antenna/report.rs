//! Bounded ownership of selected-report rows and their input inventory.
use super::{AntennaRequest, AntennaStats, CheckRow, Error, Result};
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Conservative report allocations owned by one selected operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportMemoryStats {
    pub limit_bytes: usize,
    pub live_bytes: usize,
    pub peak_live_bytes: usize,
    /// Monotonic sum of reservations, including buffers subsequently released.
    pub cumulative_reserved_bytes: usize,
    /// Legacy conservative per-row allocation telemetry, never reset on release.
    pub cumulative_row_bytes: usize,
}

#[derive(Debug)]
struct Budget {
    limit: usize,
    live: AtomicUsize,
    peak: AtomicUsize,
    allocated: AtomicUsize,
}

#[derive(Clone, Debug)]
pub(super) struct ReportMemory(Arc<Budget>);

impl ReportMemory {
    pub(super) fn new(limit: usize) -> Self {
        Self(Arc::new(Budget {
            limit,
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            allocated: AtomicUsize::new(0),
        }))
    }

    pub(super) fn reserve(&self, bytes: usize) -> Result<Reservation> {
        let mut lease = Reservation {
            budget: self.clone(),
            bytes: 0,
        };
        lease.grow(bytes)?;
        Ok(lease)
    }

    pub(super) fn stats(&self, cumulative_row_bytes: usize) -> ReportMemoryStats {
        ReportMemoryStats {
            limit_bytes: self.0.limit,
            live_bytes: self.0.live.load(Ordering::Relaxed),
            peak_live_bytes: self.0.peak.load(Ordering::Relaxed),
            cumulative_reserved_bytes: self.0.allocated.load(Ordering::Relaxed),
            cumulative_row_bytes,
        }
    }
}

/// A reservation follows the owned buffer and releases only when it is dropped.
#[derive(Debug)]
pub(super) struct Reservation {
    budget: ReportMemory,
    bytes: usize,
}

impl Reservation {
    pub(super) fn grow(&mut self, bytes: usize) -> Result<()> {
        let new_bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| Error::Budget("ANTENNA: live report memory budget".into()))?;
        let previous = self
            .budget
            .0
            .live
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(bytes).filter(|&n| n <= self.budget.0.limit)
            })
            .map_err(|_| Error::Budget("ANTENNA: live report memory budget".into()))?;
        if self
            .budget
            .0
            .allocated
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(bytes)
            })
            .is_err()
        {
            self.budget.0.live.fetch_sub(bytes, Ordering::Relaxed);
            return Err(Error::Budget("ANTENNA: report allocation counter".into()));
        }
        self.budget
            .0
            .peak
            .fetch_max(previous + bytes, Ordering::Relaxed);
        self.bytes = new_bytes;
        Ok(())
    }

    fn transfer_from(&mut self, other: &mut Self, bytes: usize) -> Result<()> {
        if !Arc::ptr_eq(&self.budget.0, &other.budget.0) || bytes > other.bytes {
            return Err(Error::Invalid(
                "ANTENNA_REPORT: incompatible reservation".into(),
            ));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| Error::Budget("ANTENNA: report allocation counter".into()))?;
        other.bytes -= bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.live.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Same row values and ordering as the legacy inventory; selected rows carry a lease.
#[derive(Debug)]
pub(super) struct Rows {
    pub(super) data: Vec<CheckRow>,
    lease: Option<Reservation>,
    variable_bytes: usize,
}

impl Rows {
    pub(super) fn new(memory: Option<&ReportMemory>) -> Result<Self> {
        Ok(Self {
            data: Vec::new(),
            lease: memory.map(|m| m.reserve(0)).transpose()?,
            variable_bytes: 0,
        })
    }

    fn reserve_slots(&mut self, additional: usize) -> Result<()> {
        let Some(lease) = self.lease.as_mut() else {
            return Ok(());
        };
        let needed = self
            .data
            .len()
            .checked_add(additional)
            .ok_or_else(|| Error::Budget("ANTENNA: row capacity".into()))?;
        if needed > self.data.capacity() {
            let capacity = needed.max(self.data.capacity().saturating_mul(2)).max(4);
            let bytes = capacity
                .checked_sub(self.data.capacity())
                .and_then(|n| n.checked_mul(mem::size_of::<CheckRow>()))
                .ok_or_else(|| Error::Budget("ANTENNA: row capacity".into()))?;
            lease.grow(bytes)?;
            self.data
                .try_reserve_exact(capacity - self.data.len())
                .map_err(|_| Error::Budget("ANTENNA: row allocation".into()))?;
        }
        Ok(())
    }

    /// Charge before the evaluator clones names, PWLs or grows the row vector.
    pub(super) fn charge_row(&mut self, bytes: usize, stats: &mut AntennaStats) -> Result<()> {
        stats.report_memory_bytes = stats
            .report_memory_bytes
            .checked_add(bytes)
            .filter(|&n| self.lease.is_some() || n <= super::MAX_REPORT_MEMORY_BYTES)
            .ok_or_else(|| Error::Budget("ANTENNA: report memory budget".into()))?;
        self.reserve_slots(1)?;
        if let Some(lease) = &mut self.lease {
            let variable = bytes.saturating_sub(mem::size_of::<CheckRow>());
            lease.grow(variable)?;
            self.variable_bytes = self
                .variable_bytes
                .checked_add(variable)
                .ok_or_else(|| Error::Budget("ANTENNA: row capacity".into()))?;
        }
        Ok(())
    }

    pub(super) fn append(&mut self, mut other: Self) -> Result<()> {
        self.reserve_slots(other.data.len())?;
        if let (Some(lease), Some(from)) = (&mut self.lease, &mut other.lease) {
            lease.transfer_from(from, other.variable_bytes)?;
            self.variable_bytes += other.variable_bytes;
        }
        self.data.extend(other.data);
        Ok(())
    }

    pub(super) fn into_output(self) -> (Vec<CheckRow>, Option<Arc<Reservation>>) {
        (self.data, self.lease.map(Arc::new))
    }
}

/// Frozen geometry/model budgets attached only to an explicit selected report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisLimits {
    pub max_shapes_per_net: usize,
    pub max_total_shapes: usize,
    pub max_pair_checks: usize,
    pub max_union_cells: usize,
    pub max_rows: usize,
    pub max_work: usize,
    pub report_memory_bytes: usize,
}

impl From<&AntennaRequest> for AnalysisLimits {
    fn from(r: &AntennaRequest) -> Self {
        Self {
            max_shapes_per_net: r.max_shapes_per_net,
            max_total_shapes: r.max_total_shapes,
            max_pair_checks: r.max_pair_checks,
            max_union_cells: r.max_union_cells,
            max_rows: r.max_rows,
            max_work: r.max_work,
            report_memory_bytes: super::MAX_REPORT_MEMORY_BYTES,
        }
    }
}

/// Why a requested net does or does not produce native antenna rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetObservation {
    Observed,
    NoGeometry,
    NoGateArea,
    NoApplicableRuleObservation,
}

/// One requested identity, including explicitly unobserved or partial nets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetCoverage {
    pub net: String,
    pub shapes: usize,
    pub rows: usize,
    pub gate_terminals: usize,
    pub observation: NetObservation,
    pub model_partial: bool,
}

/// An explicit subset cannot be mistaken for a complete routed-design inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedAnalysis {
    pub schema: String,
    pub scope: String,
    pub canonical_nets: Vec<String>,
    pub selection_sha256: String,
    pub source_sha256: String,
    pub limits: AnalysisLimits,
    pub coverage: Vec<NetCoverage>,
}

struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn digest(value: &impl Serialize) -> Result<[u8; 32]> {
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().into())
}

pub(super) fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(digest(value)?.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Deserialize)]
struct InputScope {
    extra_shapes: usize,
    routing_scope: String,
}

#[derive(Deserialize)]
struct InputMargin<'input> {
    #[serde(borrow)]
    fraction: &'input RawValue,
}

#[derive(Deserialize)]
struct Envelope<'input> {
    #[serde(borrow)]
    schema: &'input RawValue,
    #[serde(borrow)]
    status: &'input RawValue,
    #[serde(borrow)]
    workspace_revision: &'input RawValue,
    #[serde(borrow)]
    design: &'input RawValue,
    dbu_per_micron: u32,
    #[serde(borrow)]
    safety_margin: &'input RawValue,
    #[serde(borrow)]
    scope: &'input RawValue,
    violations: usize,
    lef_violations: usize,
    #[serde(borrow)]
    sources: &'input RawValue,
    #[serde(borrow)]
    selection: Option<&'input RawValue>,
    #[serde(borrow)]
    checks: &'input RawValue,
}

/// Input text is reserved before reading; only one bounded row is parsed at a time.
pub(super) struct SelectedInput {
    text: String,
    pub(super) analysis: SelectedAnalysis,
    _input_hold: Reservation,
    _header_hold: Reservation,
}

fn stale(message: impl std::fmt::Display) -> Error {
    super::fail("STALE_REPORT", message)
}

impl SelectedInput {
    pub(super) fn load(path: &Path, memory: &ReportMemory) -> Result<Self> {
        let bytes = usize::try_from(std::fs::metadata(path)?.len())
            .map_err(|_| Error::Budget("ANTENNA: report input bytes".into()))?;
        // The second copy bounds the JSON parser's escape scratch. Rows are borrowed
        // RawValues, not a whole serde_json::Value or a second CheckRow inventory.
        let reserve = bytes
            .checked_mul(2)
            .and_then(|n| n.checked_add(8192))
            .ok_or_else(|| Error::Budget("ANTENNA: report input bytes".into()))?;
        let input_hold = memory.reserve(reserve)?;
        let (text, _) = super::read_input(path, bytes as u64, "antenna_selected_report")?;
        let envelope: Envelope<'_> = serde_json::from_str(&text)?;
        let selected = envelope
            .selection
            .ok_or_else(|| stale("--selected-report requires an explicit selected inventory"))?;
        if selected.get().len() > 2 * 1024 * 1024 {
            return Err(Error::Budget("ANTENNA: selected report metadata".into()));
        }
        // Conservative bound for typed metadata, including Vec growth and escaped names.
        let header_hold = memory.reserve(selected.get().len().saturating_mul(16) + 8192)?;
        let analysis: SelectedAnalysis = serde_json::from_str(selected.get())?;
        super::validate_selection(&analysis.canonical_nets)?;
        if analysis.schema != "rustroad.antenna-selected.v1"
            || analysis.scope != "explicit_routed_net_subset"
            || analysis.canonical_nets.windows(2).any(|w| w[0] >= w[1])
            || analysis.coverage.len() != analysis.canonical_nets.len()
        {
            return Err(stale("invalid selected inventory identities or scope"));
        }
        Ok(Self {
            text,
            analysis,
            _input_hold: input_hold,
            _header_hold: header_hold,
        })
    }

    pub(super) fn input_bytes(&self) -> usize {
        self.text.len()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind(
        &self,
        revision: &str,
        margin: f64,
        design: &str,
        dbu: u32,
        routing_scope: &str,
        sources: &[super::SourceCheck],
        fresh: &SelectedAnalysis,
        rows: &[CheckRow],
        memory: &ReportMemory,
    ) -> Result<()> {
        let envelope: Envelope<'_> = serde_json::from_str(&self.text)?;
        let scalar_bytes = [
            envelope.schema,
            envelope.status,
            envelope.workspace_revision,
            envelope.design,
            envelope.safety_margin,
            envelope.scope,
        ]
        .iter()
        .try_fold(8192usize, |total, raw| {
            total
                .checked_add(raw.get().len().saturating_mul(16))
                .ok_or_else(|| Error::Budget("ANTENNA: report scalar metadata".into()))
        })?;
        let _scalar_hold = memory.reserve(scalar_bytes)?;
        let reported_schema: String = serde_json::from_str(envelope.schema.get())?;
        let reported_status: String = serde_json::from_str(envelope.status.get())?;
        let reported_revision: String = serde_json::from_str(envelope.workspace_revision.get())?;
        let reported_design: String = serde_json::from_str(envelope.design.get())?;
        let reported_margin: InputMargin<'_> = serde_json::from_str(envelope.safety_margin.get())?;
        let reported_scope: InputScope = serde_json::from_str(envelope.scope.get())?;
        let status = if rows.iter().any(|row| row.violated) {
            "subset_violated"
        } else if routing_scope != "all_signal_nets"
            || fresh
                .coverage
                .iter()
                .any(|net| net.model_partial || net.observation != NetObservation::Observed)
        {
            "subset_partial"
        } else {
            "subset_clean"
        };
        if reported_schema != super::ANTENNA_SCHEMA
            || reported_status != status
            || reported_revision != revision
            || reported_design != design
            || envelope.dbu_per_micron != dbu
            || scalar(reported_margin.fraction)? != margin
            || reported_scope.extra_shapes != 0
            || reported_scope.routing_scope != routing_scope
            || self.analysis != *fresh
        {
            return Err(stale(
                "selected scope, limits, margin, source or revision differs",
            ));
        }
        let source_raw = envelope.sources.get();
        if source_raw.len() > 2 * 1024 * 1024 {
            return Err(Error::Budget("ANTENNA: source report metadata".into()));
        }
        let _sources_hold = memory.reserve(source_raw.len().saturating_mul(16) + 8192)?;
        let reported_sources: Vec<super::SourceCheck> = serde_json::from_str(source_raw)?;
        if reported_sources != sources
            || envelope.violations != rows.iter().filter(|r| r.violated).count()
            || envelope.lef_violations != rows.iter().filter(|r| r.exceeds_lef_limit).count()
        {
            return Err(stale(
                "selected source identities or violation counts differ",
            ));
        }
        let _index_hold = memory.reserve(rows.len().saturating_mul(128) + 8192)?;
        let mut inventory = BTreeMap::new();
        for row in rows {
            *inventory.entry(digest(row)?).or_insert(0usize) += 1;
        }
        let mut deserializer = serde_json::Deserializer::from_str(envelope.checks.get());
        let mut budget_error = None;
        let result = BindingRows {
            expected: rows.len(),
            inventory: &mut inventory,
            memory,
            budget_error: &mut budget_error,
        }
        .deserialize(&mut deserializer);
        if let Some(error) = budget_error {
            return Err(error);
        }
        let count = result.map_err(|e| stale(format!("selected row inventory: {e}")))?;
        deserializer.end()?;
        if count != rows.len() || inventory.values().any(|&n| n != 0) {
            return Err(stale("selected row inventory is missing rows"));
        }
        Ok(())
    }
}

struct BindingRows<'scope> {
    expected: usize,
    inventory: &'scope mut BTreeMap<[u8; 32], usize>,
    memory: &'scope ReportMemory,
    budget_error: &'scope mut Option<Error>,
}

impl<'input> DeserializeSeed<'input> for BindingRows<'_> {
    type Value = usize;
    fn deserialize<D: serde::Deserializer<'input>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'input> Visitor<'input> for BindingRows<'_> {
    type Value = usize;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the complete bounded selected check-row inventory")
    }
    fn visit_seq<A: SeqAccess<'input>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        use serde::de::Error as _;
        let mut count = 0;
        while let Some(raw) = seq.next_element::<&RawValue>()? {
            if count >= self.expected {
                return Err(A::Error::custom("too many selected rows"));
            }
            let bytes = raw
                .get()
                .len()
                .checked_mul(16)
                .and_then(|n| n.checked_add(mem::size_of::<CheckRow>() + 512))
                .ok_or_else(|| A::Error::custom("selected row allocation overflow"))?;
            let _row_hold = match self.memory.reserve(bytes) {
                Ok(reservation) => reservation,
                Err(error) => {
                    *self.budget_error = Some(error);
                    return Err(A::Error::custom("selected row live memory budget"));
                }
            };
            let row = exact_row(raw.get()).map_err(A::Error::custom)?;
            let hash = digest(&row).map_err(A::Error::custom)?;
            let n = self
                .inventory
                .get_mut(&hash)
                .filter(|n| **n > 0)
                .ok_or_else(|| A::Error::custom("unknown, forged or duplicate selected row"))?;
            *n -= 1;
            count += 1;
        }
        Ok(count)
    }
}

// Report numbers must survive a writer/reader roundtrip exactly. Local raw-number
// parsing avoids changing the legacy JSON parser's features or numerical behavior.
fn scalar(raw: &RawValue) -> Result<f64> {
    raw.get()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| stale("selected numeric value must be a finite JSON number"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactLimit<'input> {
    kind: String,
    #[serde(borrow)]
    ratio: Option<&'input RawValue>,
    #[serde(borrow)]
    points: Option<Vec<[&'input RawValue; 2]>>,
}
impl ExactLimit<'_> {
    fn into_limit(self) -> Result<super::Limit> {
        match (self.kind.as_str(), self.ratio, self.points) {
            ("value", Some(ratio), None) => Ok(super::Limit::Value {
                ratio: scalar(ratio)?,
            }),
            ("pwl", None, Some(points)) => Ok(super::Limit::Pwl {
                points: points
                    .into_iter()
                    .map(|[x, y]| Ok([scalar(x)?, scalar(y)?]))
                    .collect::<Result<_>>()?,
            }),
            _ => Err(stale(
                "selected rule must have exactly its scalar or PWL values",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactRules<'input> {
    keyword_no_diff: String,
    #[serde(borrow)]
    no_diff: Option<ExactLimit<'input>>,
    keyword_with_diff: String,
    #[serde(borrow)]
    with_diff: Option<ExactLimit<'input>>,
    #[serde(borrow)]
    gate_plus_diff: Option<&'input RawValue>,
}
impl ExactRules<'_> {
    fn into_rules(self) -> Result<super::RulePair> {
        Ok(super::RulePair {
            keyword_no_diff: self.keyword_no_diff,
            no_diff: self.no_diff.map(ExactLimit::into_limit).transpose()?,
            keyword_with_diff: self.keyword_with_diff,
            with_diff: self.with_diff.map(ExactLimit::into_limit).transpose()?,
            gate_plus_diff: self.gate_plus_diff.map(scalar).transpose()?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactRow<'input> {
    net: String,
    layer: String,
    kind: String,
    gates: Vec<String>,
    diffusions: Vec<String>,
    #[serde(borrow)]
    gate_area_um2: &'input RawValue,
    #[serde(borrow)]
    diff_area_um2: &'input RawValue,
    #[serde(borrow)]
    metal_um2: &'input RawValue,
    #[serde(borrow)]
    denominator_um2: &'input RawValue,
    #[serde(borrow)]
    ratio: &'input RawValue,
    #[serde(borrow)]
    limit: &'input RawValue,
    #[serde(borrow)]
    effective_limit: &'input RawValue,
    rule: String,
    #[serde(borrow)]
    rules: ExactRules<'input>,
    violated: bool,
    exceeds_lef_limit: bool,
}
fn exact_row(text: &str) -> Result<CheckRow> {
    let row: ExactRow<'_> = serde_json::from_str(text)?;
    Ok(CheckRow {
        net: row.net,
        layer: row.layer,
        kind: row.kind,
        gates: row.gates,
        diffusions: row.diffusions,
        gate_area_um2: scalar(row.gate_area_um2)?,
        diff_area_um2: scalar(row.diff_area_um2)?,
        metal_um2: scalar(row.metal_um2)?,
        denominator_um2: scalar(row.denominator_um2)?,
        ratio: scalar(row.ratio)?,
        limit: scalar(row.limit)?,
        effective_limit: scalar(row.effective_limit)?,
        rule: row.rule,
        rules: row.rules.into_rules()?,
        violated: row.violated,
        exceeds_lef_limit: row.exceeds_lef_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_buffers_release_live_memory_without_resetting_allocations() {
        let memory = ReportMemory::new(100);
        let first = memory.reserve(60).unwrap();
        assert!(memory.reserve(41).is_err());
        drop(first);
        let second = memory.reserve(80).unwrap();
        let stats = memory.stats(140);
        assert_eq!(stats.live_bytes, 80);
        assert_eq!(stats.peak_live_bytes, 80);
        assert_eq!(stats.cumulative_reserved_bytes, 140);
        assert_eq!(stats.cumulative_row_bytes, 140);
        drop(second);
        assert_eq!(memory.stats(140).live_bytes, 0);
    }

    #[test]
    fn simultaneous_buffers_and_failed_growth_obey_one_live_cap() {
        let memory = ReportMemory::new(100);
        let mut before = memory.reserve(50).unwrap();
        let trial = memory.reserve(40).unwrap();
        assert!(before.grow(11).is_err());
        assert_eq!(memory.stats(90).live_bytes, 90);
        drop(trial);
        before.grow(20).unwrap();
        assert_eq!(memory.stats(110).live_bytes, 70);
        assert_eq!(memory.stats(110).cumulative_reserved_bytes, 110);
    }

    #[test]
    fn selected_row_decimals_and_pwl_values_round_trip_exactly() {
        let value = 221.39601139601137;
        let small = 1.1965811965811965;
        let row = CheckRow {
            net: "clock".into(),
            layer: "M2".into(),
            kind: "area".into(),
            gates: vec!["g/CK".into()],
            diffusions: vec![],
            gate_area_um2: small,
            diff_area_um2: 0.0,
            metal_um2: value,
            denominator_um2: small,
            ratio: value,
            limit: value,
            effective_limit: small,
            rule: "ANTENNAAREARATIO".into(),
            rules: super::super::RulePair {
                keyword_no_diff: "ANTENNAAREARATIO".into(),
                no_diff: Some(super::super::Limit::Value { ratio: value }),
                keyword_with_diff: "ANTENNADIFFAREARATIO".into(),
                with_diff: Some(super::super::Limit::Pwl {
                    points: vec![[small, value]],
                }),
                gate_plus_diff: Some(small),
            },
            violated: true,
            exceeds_lef_limit: true,
        };
        let text = serde_json::to_string(&row).unwrap();
        let decoded = exact_row(&text).unwrap();
        assert_eq!(decoded.ratio.to_bits(), row.ratio.to_bits());
        assert_eq!(serde_json::to_string(&decoded).unwrap(), text);
        assert_eq!(digest(&decoded).unwrap(), digest(&row).unwrap());
    }
}

/// `SelectedInput::bind` for the selected check this session computed and
/// wrote itself: the same identities, flags and row multiset, compared in
/// memory instead of re-reading and re-parsing the written report.
#[allow(clippy::too_many_arguments)]
pub(super) fn bind_in_memory(
    before: &super::AntennaReport,
    revision: &str,
    margin: f64,
    design: &str,
    dbu: u32,
    routing_scope: &str,
    sources: &[super::SourceCheck],
    fresh: &SelectedAnalysis,
    rows: &[CheckRow],
) -> Result<()> {
    let status = if rows.iter().any(|row| row.violated) {
        "subset_violated"
    } else if routing_scope != "all_signal_nets"
        || fresh
            .coverage
            .iter()
            .any(|net| net.model_partial || net.observation != NetObservation::Observed)
    {
        "subset_partial"
    } else {
        "subset_clean"
    };
    if before.schema != super::ANTENNA_SCHEMA
        || before.status != status
        || before.workspace_revision != revision
        || before.design != design
        || before.dbu_per_micron != dbu
        || before.safety_margin.fraction != margin
        || before.scope.extra_shapes != 0
        || before.scope.routing_scope != routing_scope
        || before.selection.as_ref() != Some(fresh)
    {
        return Err(stale(
            "selected scope, limits, margin, source or revision differs",
        ));
    }
    if before.sources != sources
        || before.violations != rows.iter().filter(|r| r.violated).count()
        || before.lef_violations != rows.iter().filter(|r| r.exceeds_lef_limit).count()
        || before.checks.len() != rows.len()
    {
        return Err(stale(
            "selected source identities or violation counts differ",
        ));
    }
    let mut inventory = BTreeMap::new();
    for row in rows {
        *inventory.entry(digest(row)?).or_insert(0usize) += 1;
    }
    for row in &before.checks {
        let n = inventory
            .get_mut(&digest(row)?)
            .filter(|n| **n > 0)
            .ok_or_else(|| stale("selected row inventory differs"))?;
        *n -= 1;
    }
    Ok(())
}

/// `source_fingerprint` with its routes-independent prefix hashed once: the
/// SHA-256 state after `[schema,design,model,placed,scope,config,` is kept,
/// and each new routes state is serialized (record chunks in parallel, the
/// same compact JSON bytes) and hashed after it with `,sources]`.
pub(super) struct SourceDigest {
    prefix: Sha256,
    suffix: Vec<u8>,
}

struct BufferedHash<'h>(&'h mut Sha256, Vec<u8>);
impl Write for BufferedHash<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.1.extend_from_slice(bytes);
        if self.1.len() >= 1 << 16 {
            self.0.update(&self.1);
            self.1.clear();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.update(&self.1);
        self.1.clear();
        Ok(())
    }
}

fn json_into(hash: &mut Sha256, value: &impl Serialize) -> Result<()> {
    let mut writer = BufferedHash(hash, Vec::with_capacity(1 << 16));
    serde_json::to_writer(&mut writer, value)?;
    writer.flush()?;
    Ok(())
}

/// A JSON array of `items` exactly as serde_json writes a `Vec`, with the
/// elements serialized in parallel chunks and hashed in order.
fn json_array_into<T: Serialize + Sync>(hash: &mut Sha256, items: &[T]) -> Result<()> {
    use rayon::prelude::*;
    hash.update(b"[");
    let mut first = true;
    for batch in items.chunks(1 << 18) {
        let parts = batch
            .par_chunks(1 << 13)
            .map(|chunk| -> Result<Vec<u8>> {
                let mut buffer = Vec::with_capacity(chunk.len() * 48);
                for (i, item) in chunk.iter().enumerate() {
                    if i > 0 {
                        buffer.push(b',');
                    }
                    serde_json::to_writer(&mut buffer, item)?;
                }
                Ok(buffer)
            })
            .collect::<Result<Vec<_>>>()?;
        for part in parts {
            if !first {
                hash.update(b",");
            }
            first = false;
            hash.update(&part);
        }
    }
    hash.update(b"]");
    Ok(())
}

impl SourceDigest {
    pub(super) fn new(
        design: &rustroad_core::Design,
        model: &rustroad_core::connectivity::ConnectedDesign,
        placed: &[rustroad_core::Placed],
        state: &crate::routing::RoutingState,
        sources: &[super::SourceCheck],
    ) -> Result<Self> {
        let mut prefix = Sha256::new();
        prefix.update(b"[");
        json_into(&mut prefix, &super::ANTENNA_SCHEMA)?;
        prefix.update(b",");
        json_into(&mut prefix, design)?;
        prefix.update(b",");
        json_into(&mut prefix, model)?;
        prefix.update(b",");
        json_into(&mut prefix, &placed)?;
        prefix.update(b",");
        json_into(&mut prefix, &state.scope)?;
        prefix.update(b",");
        json_into(&mut prefix, &state.config)?;
        prefix.update(b",");
        let mut suffix = b",".to_vec();
        serde_json::to_writer(&mut suffix, &sources)?;
        suffix.push(b']');
        Ok(Self { prefix, suffix })
    }

    /// `source_fingerprint` of the prefix state with these routes.
    pub(super) fn fingerprint(&self, routes: &rustroad_core::routing::Routes) -> Result<String> {
        let mut hash = self.prefix.clone();
        hash_routes(&mut hash, routes)?;
        hash.update(&self.suffix);
        let out: [u8; 32] = hash.finalize().into();
        Ok(out.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The compact JSON of `routes` (as serde_json writes it) into `hash`.
fn hash_routes(hash: &mut Sha256, routes: &rustroad_core::routing::Routes) -> Result<()> {
    hash.update(b"{\"wires\":");
    json_array_into(hash, &routes.wires)?;
    hash.update(b",\"vias\":");
    json_array_into(hash, &routes.vias)?;
    hash.update(b",\"access\":");
    json_array_into(hash, &routes.access)?;
    if !routes.patches.is_empty() {
        hash.update(b",\"patches\":");
        json_array_into(hash, &routes.patches)?;
    }
    hash.update(b",\"nets\":");
    json_array_into(hash, &routes.nets)?;
    hash.update(b",\"stats\":");
    json_into(hash, &routes.stats)?;
    hash.update(b"}");
    Ok(())
}

#[cfg(test)]
mod digest_tests {
    use super::*;
    use rustroad_core::Rect;
    use rustroad_core::routing::{Access, Patch, RouteStats, Routes, Via, Wire};

    /// The parallel chunked serialization hashes exactly serde_json's bytes,
    /// with and without patches, across several element batches.
    #[test]
    fn chunked_routes_json_matches_serde() {
        for (wires, patches) in [(0usize, 0usize), (3, 0), (300_000, 2), (7, 9)] {
            let routes = Routes {
                wires: (0..wires)
                    .map(|i| Wire {
                        net: (i % 977) as u32,
                        layer: (i % 5) as u32,
                        start: [i as i64 * 3 - 7, -(i as i64)],
                        end: [i as i64 * 3 + 5, -(i as i64)],
                    })
                    .collect(),
                vias: (0..wires / 2 + 1)
                    .map(|i| Via {
                        net: i as u32,
                        kind: 2,
                        at: [i as i64, i64::MIN + i as i64],
                    })
                    .collect(),
                access: vec![Access {
                    pin: 4,
                    net: 1,
                    layer: 0,
                    at: [10, 20],
                }],
                patches: (0..patches)
                    .map(|i| Patch {
                        net: i as u32,
                        layer: 1,
                        rect: Rect {
                            lx: 0,
                            ly: 1,
                            ux: 2,
                            uy: i as i64,
                        },
                    })
                    .collect(),
                nets: (0..wires.min(977) as u32).collect(),
                stats: RouteStats {
                    grid_nodes: 9,
                    expansions: 8,
                    comparisons: 7,
                    wire_length_dbu: 6,
                    single_owner_repair: None,
                },
            };
            let mut chunked = Sha256::new();
            hash_routes(&mut chunked, &routes).unwrap();
            let mut whole = Sha256::new();
            whole.update(serde_json::to_vec(&routes).unwrap());
            assert_eq!(chunked.finalize(), whole.finalize(), "{wires} wires");
        }
    }
}
