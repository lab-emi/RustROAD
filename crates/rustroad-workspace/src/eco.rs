//! Hold-closure and antenna ECO planning on mapped netlists (no workspace I/O).
//!
//! `plan_hold` reads RustSTA 0.4 `hold_fix` / `endpoints` JSON reports, a
//! Liberty-derived delay-cell profile (`rustroad.delay-cells.v1`) and a flat
//! mapped netlist, chooses per violating sink pin the smallest-area cell or
//! bounded chain whose minimum per-corner delay covers `target - slack` at
//! every hold corner while the estimated setup slack at the setup corner stays
//! at or above the setup margin, and applies the result through the typed ECO
//! primitive (`rustroad_formats::eco`). Ordering is deterministic.
//!
//! Memory/work preflight (Rule 9): reports and profiles are capped at 64 MiB /
//! 4 MiB; per sink the chain search evaluates at most `max_combinations`
//! multisets (default 200,000, each O(max_chain x corners)); at most
//! `max_sinks` sinks are planned. Peak memory is the netlist AST plus the
//! parsed reports (well below 256 MiB at the input caps). No timing graph,
//! path or cone is built here; slacks come from the timing engine.
use crate::pdk::read_input;
use crate::{Error, Result};
use rustroad_formats::eco::{
    self, BufferCell, DiodeCell, ECO_PLAN_SCHEMA, EcoCells, EcoLimits, EcoOp, EcoPlan, EcoReport,
    PinRef,
};
use rustroad_formats::verilog::{self, Expr, Netlist, VerilogLimits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub const DELAY_PROFILE_SCHEMA: &str = "rustroad.delay-cells.v1";
pub const HOLD_PLAN_SCHEMA: &str = "rustroad.hold-eco-plan.v1";
const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NETLIST_BYTES: u64 = 16 * 1024 * 1024;

fn fail(code: &str, message: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{code}: {message}"))
}

// ----------------------------------------------------------------- profile

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSource {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePolicy {
    pub path: String,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCorner {
    pub name: String,
    pub liberty: String,
    pub rc: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Representative {
    pub input_slew_ns: f64,
    pub load_pf: f64,
    pub reason: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiseFall {
    pub rise: f64,
    pub fall: f64,
}
impl RiseFall {
    fn min(self) -> f64 {
        self.rise.min(self.fall)
    }
    fn max(self) -> f64 {
        self.rise.max(self.fall)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledBuffer {
    pub name: String,
    pub input_pin: String,
    pub output_pin: String,
    pub function: String,
    pub function_check: String,
    pub timing_sense: String,
    pub area_um2: f64,
    pub lef_size_um: [f64; 2],
    pub policy_class: String,
    pub input_cap_pf: BTreeMap<String, f64>,
    pub delay_ns: BTreeMap<String, RiseFall>,
    pub output_slew_ns: BTreeMap<String, RiseFall>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledDiode {
    pub name: String,
    pub pin: String,
    pub area_um2: f64,
    pub lef_size_um: [f64; 2],
    pub lef_diff_area_um2: f64,
    pub lef_gate_area_um2: f64,
    pub policy_class: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Excluded {
    pub name: String,
    pub reason: String,
}
/// `fixtures/pdk/<pdk>-delay-cells.json` (scripts/build_delay_cells.py).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayCellProfile {
    pub schema: String,
    pub pdk: String,
    pub library: String,
    pub generator: String,
    pub sources: Vec<ProfileSource>,
    pub cell_policy: ProfilePolicy,
    pub corners: Vec<ProfileCorner>,
    pub representative: Representative,
    pub selection: serde_json::Value,
    pub buffers: Vec<ProfiledBuffer>,
    pub antenna_cells: Vec<ProfiledDiode>,
    pub excluded: Vec<Excluded>,
}
impl DelayCellProfile {
    /// Typed ECO cell data; every buffer must prove an identity function.
    pub fn eco_cells(&self) -> Result<EcoCells> {
        let cells = EcoCells {
            buffers: self
                .buffers
                .iter()
                .map(|b| BufferCell {
                    name: b.name.clone(),
                    input_pin: b.input_pin.clone(),
                    output_pin: b.output_pin.clone(),
                    function: b.function.clone(),
                })
                .collect(),
            diodes: self
                .antenna_cells
                .iter()
                .map(|d| DiodeCell {
                    name: d.name.clone(),
                    pin: d.pin.clone(),
                })
                .collect(),
        };
        for b in &cells.buffers {
            b.check_identity().map_err(|e| fail("DELAY_PROFILE", e))?;
        }
        Ok(cells)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
fn artifact_of(pin: &crate::pdk::SourcePin) -> Artifact {
    Artifact {
        path: pin.path.clone(),
        sha256: pin.sha256.clone(),
        bytes: pin.bytes as u64,
    }
}

pub fn load_profile(path: &Path) -> Result<(DelayCellProfile, Artifact)> {
    let (text, pin) = read_input(path, MAX_PROFILE_BYTES, "delay_cell_profile")?;
    let profile: DelayCellProfile = serde_json::from_str(&text)?;
    if profile.schema != DELAY_PROFILE_SCHEMA {
        return Err(fail(
            "DELAY_PROFILE",
            format!("schema {} is not {DELAY_PROFILE_SCHEMA}", profile.schema),
        ));
    }
    for b in &profile.buffers {
        let ok = |v: f64| v.is_finite() && v >= 0.0;
        if !(b.area_um2.is_finite() && b.area_um2 > 0.0)
            || b.delay_ns.values().any(|d| !ok(d.rise) || !ok(d.fall))
            || b.timing_sense != "positive_unate"
        {
            return Err(fail(
                "DELAY_PROFILE",
                format!("buffer {} has invalid area/delay/sense", b.name),
            ));
        }
    }
    Ok((profile, artifact_of(&pin)))
}

pub fn load_netlist(path: &Path) -> Result<(Netlist, Artifact)> {
    let (text, pin) = read_input(path, MAX_NETLIST_BYTES, "mapped_verilog")?;
    let netlist = verilog::parse_verilog(&text, &VerilogLimits::default())
        .map_err(|e| fail("ECO_NETLIST", e))?;
    Ok((netlist, artifact_of(&pin)))
}

/// Write `bytes` to `out` atomically (temp file in the same directory, fsync, rename).
pub fn write_atomic(out: &Path, bytes: &[u8]) -> Result<Artifact> {
    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| fail("OUTPUT_PATH", "output must name a file"))?;
    let temp = out.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut w = BufWriter::new(file);
        w.write_all(bytes)?;
        let file: File = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&temp, out) {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(Artifact {
        path: out.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    })
}

fn verilog_bytes(netlist: &Netlist) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    verilog::write_verilog(netlist, &mut out).map_err(|e| fail("ECO_NETLIST", e))?;
    Ok(out)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), b.parent().map(fs::canonicalize)) {
        (Ok(a), Some(Ok(parent))) => b.file_name().is_some_and(|n| parent.join(n) == a),
        _ => false,
    }
}

// ----------------------------------------------------------------- reports

#[derive(Debug, Deserialize)]
struct Envelope {
    schema: String,
    ok: bool,
    scope: serde_json::Value,
    sections: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
struct EndpointTiming {
    /// corner -> hold slack (only corners where the endpoint was listed).
    hold: BTreeMap<String, f64>,
    /// corner -> setup slack.
    setup: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportInput {
    pub artifact: Artifact,
    pub kind: &'static str,
    pub corners: Vec<String>,
    pub wire_model: String,
    pub clock_model: String,
}

fn num(v: &serde_json::Value, key: &str, ctx: &str) -> Result<f64> {
    v.get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|x| x.is_finite())
        .ok_or_else(|| {
            fail(
                "TIMING_REPORT",
                format!("{ctx}: {key} missing or not finite"),
            )
        })
}
fn text<'a>(v: &'a serde_json::Value, key: &str, ctx: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| fail("TIMING_REPORT", format!("{ctx}: {key} missing")))
}

fn read_envelope(path: &Path) -> Result<(Envelope, Artifact)> {
    let (text, pin) = read_input(path, MAX_REPORT_BYTES, "timing_report")?;
    let env: Envelope = serde_json::from_str(&text)?;
    if env.schema != "rusteda.v1" || !env.ok {
        return Err(fail(
            "TIMING_REPORT",
            format!("{} is not a successful rusteda.v1 report", path.display()),
        ));
    }
    Ok((env, artifact_of(&pin)))
}

fn scope_models(env: &Envelope) -> (String, String) {
    let s = |k: &str| {
        env.scope
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    (s("wire_model"), s("clock_model"))
}

fn merge_slack(
    map: &mut BTreeMap<String, f64>,
    corner: &str,
    value: f64,
    what: &str,
    endpoint: &str,
) -> Result<()> {
    if let Some(old) = map.insert(corner.to_string(), value)
        && (old - value).abs() > 1e-12
    {
        return Err(fail(
            "TIMING_REPORT",
            format!("{endpoint}: conflicting {what} slack at {corner} ({old} vs {value})"),
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------- planning

#[derive(Clone, Debug)]
pub struct HoldEcoRequest {
    pub netlist: PathBuf,
    pub hold_reports: Vec<PathBuf>,
    pub setup_reports: Vec<PathBuf>,
    pub profile: PathBuf,
    pub hold_corners: Vec<String>,
    pub setup_corner: String,
    pub target_margin_ns: f64,
    pub setup_margin_ns: f64,
    pub max_chain: usize,
    pub max_total_chain: usize,
    pub max_sinks: usize,
    pub max_combinations: usize,
    pub prefix: String,
    pub out: PathBuf,
    pub plan_out: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct CornerEstimate {
    pub hold_slack_ns: Option<f64>,
    pub need_ns: f64,
    pub added_min_ns: f64,
    pub added_max_ns: f64,
    pub estimated_hold_slack_ns: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct SinkPlan {
    pub endpoint: String,
    pub instance: String,
    pub pin: String,
    pub driver_net: String,
    pub existing_chain: Vec<String>,
    pub cells: Vec<String>,
    pub area_um2: f64,
    pub corners: BTreeMap<String, CornerEstimate>,
    pub setup_slack_ns: f64,
    pub estimated_setup_slack_ns: f64,
    pub combinations_evaluated: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct HoldParameters {
    pub target_margin_ns: f64,
    pub setup_margin_ns: f64,
    pub hold_corners: Vec<String>,
    pub setup_corner: String,
    pub max_chain: usize,
    pub max_total_chain: usize,
    pub max_sinks: usize,
    pub max_combinations: usize,
    pub prefix: String,
    pub delay_model: &'static str,
}
#[derive(Clone, Debug, Serialize)]
pub struct HoldPlanInputs {
    pub netlist: Artifact,
    pub profile: Artifact,
    pub reports: Vec<ReportInput>,
}
#[derive(Clone, Debug, Serialize)]
pub struct HoldPlanTotals {
    pub violating_sinks: usize,
    pub planned_sinks: usize,
    pub cells: usize,
    pub area_um2: f64,
}
#[derive(Clone, Debug, Serialize)]
pub struct HoldPlan {
    pub schema: &'static str,
    pub status: &'static str,
    pub tool: String,
    pub inputs: HoldPlanInputs,
    pub parameters: HoldParameters,
    pub sinks: Vec<SinkPlan>,
    pub infeasible: Vec<String>,
    pub eco: EcoPlan,
    pub eco_report: Option<EcoReport>,
    pub totals: HoldPlanTotals,
    pub output: Option<Artifact>,
    pub diagnostics: Vec<String>,
}

/// Driver lookup for net expressions in the (flat) AST: output-pin bit -> instance.
fn driver_map<'a>(
    netlist: &'a Netlist,
    outputs: &HashMap<&str, &str>,
) -> HashMap<String, &'a verilog::Instance> {
    let mut map = HashMap::new();
    for inst in &netlist.instances {
        if let Some(out_pin) = outputs.get(inst.cell.as_str())
            && let Some(c) = inst.connections.iter().find(|c| c.pin == *out_pin)
        {
            map.insert(expr_key(&c.signal), inst);
        }
    }
    map
}
fn expr_key(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.clone(),
        Expr::BitSelect { name, index } => format!("{name}[{index}]"),
        other => format!("{other:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn choose_chain(
    candidates: &[&ProfiledBuffer],
    need: &BTreeMap<String, f64>,
    hold_corners: &[String],
    setup_corner: &str,
    setup_slack: f64,
    setup_margin: f64,
    max_chain: usize,
    max_combinations: usize,
) -> Result<(Option<Vec<usize>>, usize)> {
    // Multisets of candidate indices, non-decreasing, size 1..=max_chain.
    let n = candidates.len();
    let mut best: Option<(f64, usize, Vec<usize>)> = None;
    let mut evaluated = 0usize;
    for size in 1..=max_chain {
        let mut idx = vec![0usize; size];
        loop {
            evaluated += 1;
            if evaluated > max_combinations {
                return Err(Error::Budget(format!(
                    "HOLD_ECO: chain search exceeded max_combinations {max_combinations}"
                )));
            }
            let area: f64 = idx.iter().map(|&i| candidates[i].area_um2).sum();
            let covers = hold_corners.iter().all(|c| {
                let added: f64 = idx.iter().map(|&i| candidates[i].delay_ns[c].min()).sum();
                added + 1e-12 >= need.get(c).copied().unwrap_or(0.0)
            });
            let setup_added: f64 = idx
                .iter()
                .map(|&i| candidates[i].delay_ns[setup_corner].max())
                .sum();
            if covers && setup_slack - setup_added >= setup_margin {
                let better = match &best {
                    None => true,
                    Some((a, l, v)) => {
                        let names = |v: &[usize]| -> Vec<&str> {
                            v.iter().map(|&i| candidates[i].name.as_str()).collect()
                        };
                        (area, size, names(&idx)) < (*a, *l, names(v))
                    }
                };
                if better {
                    best = Some((area, size, idx.clone()));
                }
            }
            // Next non-decreasing tuple.
            let mut k = size;
            while k > 0 && idx[k - 1] == n - 1 {
                k -= 1;
            }
            if k == 0 {
                break;
            }
            idx[k - 1] += 1;
            let v = idx[k - 1];
            for slot in idx.iter_mut().skip(k) {
                *slot = v;
            }
        }
        // A shorter chain with smaller area cannot be beaten by area alone,
        // but a longer chain might be smaller; keep searching every size.
    }
    Ok((best.map(|b| b.2), evaluated))
}

pub fn plan_hold(request: &HoldEcoRequest) -> Result<HoldPlan> {
    let r = request;
    if !(r.target_margin_ns.is_finite()
        && (0.0..=10.0).contains(&r.target_margin_ns)
        && r.setup_margin_ns.is_finite()
        && (0.0..=10.0).contains(&r.setup_margin_ns))
    {
        return Err(fail("INVALID_ARGUMENT", "margins must be within 0..=10 ns"));
    }
    if r.max_chain == 0
        || r.max_chain > 8
        || r.max_total_chain == 0
        || r.max_total_chain > 32
        || r.max_sinks == 0
        || r.max_sinks > 100_000
        || r.max_combinations == 0
        || r.max_combinations > 10_000_000
    {
        return Err(fail(
            "INVALID_ARGUMENT",
            "chain/sink/combination budgets outside hard bounds",
        ));
    }
    if r.hold_corners.is_empty() || r.hold_reports.is_empty() || r.setup_reports.is_empty() {
        return Err(fail(
            "INVALID_ARGUMENT",
            "need --hold-corner, --hold-report and --setup-report",
        ));
    }
    if !eco::handle_name_ok(&format!("{}0", r.prefix), &EcoLimits::default()) {
        return Err(fail(
            "INVALID_ARGUMENT",
            "ECO name prefix is not a shell-safe handle",
        ));
    }
    if same_file(&r.netlist, &r.out) {
        return Err(fail(
            "OUTPUT_PATH",
            "--out must not overwrite the input netlist",
        ));
    }
    let (netlist, netlist_pin) = load_netlist(&r.netlist)?;
    let (profile, profile_pin) = load_profile(&r.profile)?;
    let cells = profile.eco_cells()?;
    let mut timing: BTreeMap<String, EndpointTiming> = BTreeMap::new();
    let mut reports = Vec::new();
    let mut hold_seen = BTreeSet::new();
    for path in &r.hold_reports {
        let (env, artifact) = read_envelope(path)?;
        let (wire, clock) = scope_models(&env);
        if wire != "spef_rc" || clock != "propagated" {
            return Err(fail(
                "TIMING_REPORT",
                format!(
                    "{}: decisions need extracted spef_rc wires and propagated clocks, got {wire}/{clock}",
                    path.display()
                ),
            ));
        }
        let h = env.sections.get("hold_fix").ok_or_else(|| {
            fail(
                "TIMING_REPORT",
                format!("{} has no hold_fix section", path.display()),
            )
        })?;
        let margin = num(h, "target_margin_ns", "hold_fix")?;
        if margin + 1e-12 < r.target_margin_ns {
            return Err(fail(
                "TIMING_REPORT",
                format!(
                    "{}: report lists endpoints below {margin} ns, but the target margin is {} ns",
                    path.display(),
                    r.target_margin_ns
                ),
            ));
        }
        let corners = h
            .get("corners")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| fail("TIMING_REPORT", "hold_fix.corners missing"))?;
        let mut names = Vec::new();
        for c in corners {
            let corner = text(c, "corner", "hold_fix corner")?.to_string();
            let listed = num(c, "listed", &corner)?;
            let shown = num(c, "shown", &corner)?;
            let rows = c
                .get("endpoints")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| fail("TIMING_REPORT", "hold_fix endpoints missing"))?;
            if listed != shown || rows.len() as f64 != shown {
                return Err(fail(
                    "TIMING_REPORT",
                    format!(
                        "{corner}: hold_fix rows truncated ({shown} of {listed}); use --limit 0"
                    ),
                ));
            }
            for e in rows {
                let name = text(e, "endpoint", "hold_fix row")?;
                let slack = num(e, "hold_slack_ns", name)?;
                let t = timing.entry(name.to_string()).or_default();
                merge_slack(&mut t.hold, &corner, slack, "hold", name)?;
                if let Some(s) = e.get("setup_slack_ns").and_then(serde_json::Value::as_f64) {
                    merge_slack(&mut t.setup, &corner, s, "setup", name)?;
                }
            }
            hold_seen.insert(corner.clone());
            names.push(corner);
        }
        reports.push(ReportInput {
            artifact,
            kind: "hold_fix",
            corners: names,
            wire_model: wire,
            clock_model: clock,
        });
    }
    let mut setup_seen = BTreeSet::new();
    for path in &r.setup_reports {
        let (env, artifact) = read_envelope(path)?;
        let (wire, clock) = scope_models(&env);
        if wire != "spef_rc" || clock != "propagated" {
            return Err(fail(
                "TIMING_REPORT",
                format!("{}: not spef_rc/propagated", path.display()),
            ));
        }
        let corner = env
            .scope
            .get("corner")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| fail("TIMING_REPORT", "endpoints report has no scope.corner"))?
            .to_string();
        let e = env.sections.get("endpoints").ok_or_else(|| {
            fail(
                "TIMING_REPORT",
                format!("{} has no endpoints section", path.display()),
            )
        })?;
        let total = num(e, "total", "endpoints")?;
        let rows = e
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| fail("TIMING_REPORT", "endpoints rows missing"))?;
        if num(e, "matched", "endpoints")? != total || rows.len() as f64 != total {
            return Err(fail(
                "TIMING_REPORT",
                "endpoints report is filtered or truncated; use --limit 0 and no --filter",
            ));
        }
        for row in rows {
            let name = text(row, "endpoint", "endpoints row")?;
            let s = num(row, "setup_slack_ns", name)?;
            let t = timing.entry(name.to_string()).or_default();
            merge_slack(&mut t.setup, &corner, s, "setup", name)?;
        }
        setup_seen.insert(corner.clone());
        reports.push(ReportInput {
            artifact,
            kind: "endpoints",
            corners: vec![corner],
            wire_model: wire,
            clock_model: clock,
        });
    }
    for c in &r.hold_corners {
        if !hold_seen.contains(c) {
            return Err(fail(
                "MISSING_CORNER",
                format!("no hold_fix report covers corner {c}"),
            ));
        }
    }
    if !setup_seen.contains(&r.setup_corner) {
        return Err(fail(
            "MISSING_CORNER",
            format!("no endpoints report covers setup corner {}", r.setup_corner),
        ));
    }
    let mut candidates: Vec<&ProfiledBuffer> = profile
        .buffers
        .iter()
        .filter(|b| {
            r.hold_corners
                .iter()
                .chain(std::iter::once(&r.setup_corner))
                .all(|c| b.delay_ns.contains_key(c))
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.area_um2
            .total_cmp(&b.area_um2)
            .then_with(|| a.name.cmp(&b.name))
    });
    if candidates.is_empty() {
        return Err(fail(
            "DELAY_PROFILE",
            "no buffer has delays for every requested corner",
        ));
    }
    // Existing ECO chains feeding sinks.
    let outputs: HashMap<&str, &str> = profile
        .buffers
        .iter()
        .map(|b| (b.name.as_str(), b.output_pin.as_str()))
        .collect();
    let inputs: HashMap<&str, &str> = profile
        .buffers
        .iter()
        .map(|b| (b.name.as_str(), b.input_pin.as_str()))
        .collect();
    let drivers = driver_map(&netlist, &outputs);
    let instances: HashMap<&str, &verilog::Instance> = netlist
        .instances
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    let mut diagnostics = Vec::new();
    let mut sinks = Vec::new();
    let mut infeasible = Vec::new();
    let mut ops = Vec::new();
    let mut seq = eco::next_sequence(&netlist, &r.prefix);
    let mut violating = 0usize;
    for (endpoint, t) in &timing {
        let need: BTreeMap<String, f64> = r
            .hold_corners
            .iter()
            .filter_map(|c| t.hold.get(c).map(|s| (c.clone(), r.target_margin_ns - s)))
            .filter(|(_, n)| *n > 0.0)
            .collect();
        if need.is_empty() {
            continue;
        }
        violating += 1;
        if sinks.len() >= r.max_sinks {
            return Err(Error::Budget(format!(
                "HOLD_ECO: more than {} sinks",
                r.max_sinks
            )));
        }
        let Some((instance, pin)) = endpoint.rsplit_once('/') else {
            infeasible.push(format!(
                "{endpoint}: hold violation at a top-level port; port ECO is unsupported"
            ));
            continue;
        };
        let inst = instances.get(instance).ok_or_else(|| {
            fail(
                "STALE_REPORT",
                format!("endpoint {endpoint}: instance absent from the netlist"),
            )
        })?;
        let conn = inst
            .connections
            .iter()
            .find(|c| c.pin == pin)
            .ok_or_else(|| {
                fail(
                    "STALE_REPORT",
                    format!("endpoint {endpoint}: pin not connected"),
                )
            })?;
        // Walk back through earlier ECO cells (bounded).
        let mut existing = Vec::new();
        let mut net = expr_key(&conn.signal);
        while let Some(d) = drivers.get(&net) {
            if !d.name.starts_with(&r.prefix) || existing.len() > r.max_total_chain {
                break;
            }
            existing.push(d.name.clone());
            let input = inputs[d.cell.as_str()];
            net = match d.connections.iter().find(|c| c.pin == input) {
                Some(c) => expr_key(&c.signal),
                None => break,
            };
        }
        existing.reverse();
        let setup_slack = *t.setup.get(&r.setup_corner).ok_or_else(|| {
            fail(
                "MISSING_CORNER",
                format!("{endpoint}: no setup slack at {}", r.setup_corner),
            )
        })?;
        let room = r
            .max_total_chain
            .saturating_sub(existing.len())
            .min(r.max_chain);
        let (choice, evaluated) = if room == 0 {
            (None, 0)
        } else {
            choose_chain(
                &candidates,
                &need,
                &r.hold_corners,
                &r.setup_corner,
                setup_slack,
                r.setup_margin_ns,
                room,
                r.max_combinations,
            )?
        };
        let Some(choice) = choice else {
            infeasible.push(format!(
                "{endpoint}: no chain of <= {room} profiled cells covers {need:?} ns with setup slack {setup_slack:.6} ns at {} >= {} ns (existing chain {})",
                r.setup_corner,
                r.setup_margin_ns,
                existing.len()
            ));
            continue;
        };
        let mut chosen: Vec<&ProfiledBuffer> = choice.iter().map(|&i| candidates[i]).collect();
        chosen.sort_by(|a, b| a.name.cmp(&b.name));
        let mut corners = BTreeMap::new();
        for c in &r.hold_corners {
            let added_min: f64 = chosen.iter().map(|b| b.delay_ns[c].min()).sum();
            let added_max: f64 = chosen.iter().map(|b| b.delay_ns[c].max()).sum();
            let slack = t.hold.get(c).copied();
            corners.insert(
                c.clone(),
                CornerEstimate {
                    hold_slack_ns: slack,
                    need_ns: need.get(c).copied().unwrap_or(0.0),
                    added_min_ns: added_min,
                    added_max_ns: added_max,
                    estimated_hold_slack_ns: slack.map(|s| s + added_min),
                },
            );
        }
        let setup_added: f64 = chosen
            .iter()
            .map(|b| b.delay_ns[&r.setup_corner].max())
            .sum();
        for b in &chosen {
            let name = format!("{}{seq}", r.prefix);
            seq += 1;
            ops.push(EcoOp::InsertBuffer {
                cell: b.name.clone(),
                net: format!("{name}_n"),
                instance: name,
                sink: PinRef {
                    instance: instance.to_string(),
                    pin: pin.to_string(),
                },
            });
        }
        sinks.push(SinkPlan {
            endpoint: endpoint.clone(),
            instance: instance.to_string(),
            pin: pin.to_string(),
            driver_net: expr_key(&conn.signal),
            existing_chain: existing,
            cells: chosen.iter().map(|b| b.name.clone()).collect(),
            area_um2: chosen.iter().map(|b| b.area_um2).sum(),
            corners,
            setup_slack_ns: setup_slack,
            estimated_setup_slack_ns: setup_slack - setup_added,
            combinations_evaluated: evaluated,
        });
    }
    let eco_plan = EcoPlan {
        schema: ECO_PLAN_SCHEMA.into(),
        module: netlist.module.clone(),
        operations: ops,
    };
    let status = if !infeasible.is_empty() {
        "infeasible"
    } else if sinks.is_empty() {
        "closed"
    } else {
        "planned"
    };
    if violating == 0 {
        diagnostics.push(format!(
            "HOLD_ECO_CLOSED: no endpoint below {} ns at {:?}; the output netlist equals the input",
            r.target_margin_ns, r.hold_corners
        ));
    }
    let mut plan = HoldPlan {
        schema: HOLD_PLAN_SCHEMA,
        status,
        tool: format!("rustroad {}", env!("CARGO_PKG_VERSION")),
        inputs: HoldPlanInputs {
            netlist: netlist_pin,
            profile: profile_pin,
            reports,
        },
        parameters: HoldParameters {
            target_margin_ns: r.target_margin_ns,
            setup_margin_ns: r.setup_margin_ns,
            hold_corners: r.hold_corners.clone(),
            setup_corner: r.setup_corner.clone(),
            max_chain: r.max_chain,
            max_total_chain: r.max_total_chain,
            max_sinks: r.max_sinks,
            max_combinations: r.max_combinations,
            prefix: r.prefix.clone(),
            delay_model: "profile delay at the representative slew/load; hold covers with \
                          min(rise,fall) per corner, setup charges max(rise,fall) at the setup \
                          corner; re-time with extracted parasitics after every ECO",
        },
        totals: HoldPlanTotals {
            violating_sinks: violating,
            planned_sinks: sinks.len(),
            cells: eco_plan.operations.len(),
            area_um2: sinks.iter().map(|s| s.area_um2).sum(),
        },
        sinks,
        infeasible,
        eco: eco_plan,
        eco_report: None,
        output: None,
        diagnostics,
    };
    if plan.status != "infeasible" {
        let (edited, report) = eco::apply(
            &netlist,
            &plan.eco,
            &cells,
            &EcoLimits::default(),
            &VerilogLimits::default(),
        )
        .map_err(|e| fail("ECO_APPLY", e))?;
        plan.output = Some(write_atomic(&r.out, &verilog_bytes(&edited)?)?);
        plan.eco_report = Some(report);
    }
    let bytes = serde_json::to_vec_pretty(&plan)?;
    write_atomic(&r.plan_out, &bytes)?;
    if plan.status == "infeasible" {
        return Err(fail(
            "HOLD_ECO_INFEASIBLE",
            format!(
                "{} (plan written to {})",
                plan.infeasible.join("; "),
                r.plan_out.display()
            ),
        ));
    }
    Ok(plan)
}

// ----------------------------------------------------------------- apply

#[derive(Clone, Debug, Serialize)]
pub struct ApplyReport {
    pub schema: &'static str,
    pub netlist: Artifact,
    pub plan: Artifact,
    pub profile: Artifact,
    pub eco_report: EcoReport,
    pub output: Artifact,
}

/// Apply a given typed ECO plan (e.g. a planted control) and write the netlist.
pub fn apply_plan(netlist: &Path, plan: &Path, profile: &Path, out: &Path) -> Result<ApplyReport> {
    if same_file(netlist, out) {
        return Err(fail(
            "OUTPUT_PATH",
            "--out must not overwrite the input netlist",
        ));
    }
    let (n, npin) = load_netlist(netlist)?;
    let (text, ppin) = read_input(plan, MAX_REPORT_BYTES, "eco_plan")?;
    let p: EcoPlan = serde_json::from_str(&text)?;
    let (prof, fpin) = load_profile(profile)?;
    let cells = prof.eco_cells()?;
    let (edited, report) = eco::apply(
        &n,
        &p,
        &cells,
        &EcoLimits::default(),
        &VerilogLimits::default(),
    )
    .map_err(|e| fail("ECO_APPLY", e))?;
    let output = write_atomic(out, &verilog_bytes(&edited)?)?;
    Ok(ApplyReport {
        schema: "rustroad.eco-apply.v1",
        netlist: npin,
        plan: artifact_of(&ppin),
        profile: fpin,
        eco_report: report,
        output,
    })
}

// ----------------------------------------------------------------- antenna

pub const ANTENNA_PLAN_SCHEMA: &str = "rustroad.antenna-eco-plan.v1";

#[derive(Clone, Debug)]
pub struct AntennaEcoRequest {
    pub netlist: PathBuf,
    pub report: PathBuf,
    pub profile: PathBuf,
    pub cell: Option<String>,
    pub min_diodes: usize,
    pub max_diodes_per_net: usize,
    pub prefix: String,
    pub out: PathBuf,
    pub plan_out: PathBuf,
}

#[derive(Debug, Deserialize)]
struct AntennaReportIn {
    schema: String,
    status: String,
    checks: Vec<crate::antenna::CheckRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NetRepair {
    pub net: String,
    pub violations: usize,
    pub worst_ratio_over_limit: f64,
    pub diodes: usize,
    pub estimated_worst_ratio_over_limit: f64,
    pub instances: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct AntennaPlan {
    pub schema: &'static str,
    pub status: &'static str,
    pub tool: String,
    pub netlist: Artifact,
    pub report: Artifact,
    pub profile: Artifact,
    pub cell: String,
    pub cell_gate_area_um2: f64,
    pub cell_diff_area_um2: f64,
    pub model: &'static str,
    pub nets: Vec<NetRepair>,
    pub infeasible: Vec<String>,
    pub eco: EcoPlan,
    pub eco_report: Option<EcoReport>,
    pub output: Option<Artifact>,
}

/// Plan antenna diodes for every violating net of a `check-antenna` report,
/// assuming each diode joins the violating partial net (verified afterwards
/// by re-layout, the native check and the foundry deck).
pub fn plan_antenna(r: &AntennaEcoRequest) -> Result<AntennaPlan> {
    if r.min_diodes == 0
        || r.max_diodes_per_net == 0
        || r.max_diodes_per_net > 64
        || r.min_diodes > r.max_diodes_per_net
    {
        return Err(fail(
            "INVALID_ARGUMENT",
            "need 1 <= min_diodes <= max_diodes_per_net <= 64",
        ));
    }
    if !eco::handle_name_ok(&format!("{}0", r.prefix), &EcoLimits::default()) {
        return Err(fail(
            "INVALID_ARGUMENT",
            "ECO name prefix is not a shell-safe handle",
        ));
    }
    if same_file(&r.netlist, &r.out) {
        return Err(fail(
            "OUTPUT_PATH",
            "--out must not overwrite the input netlist",
        ));
    }
    let (netlist, netlist_pin) = load_netlist(&r.netlist)?;
    let (profile, profile_pin) = load_profile(&r.profile)?;
    let cells = profile.eco_cells()?;
    let (text, report_pin) = read_input(&r.report, MAX_REPORT_BYTES, "antenna_report")?;
    let report: AntennaReportIn = serde_json::from_str(&text)?;
    if report.schema != crate::antenna::ANTENNA_SCHEMA {
        return Err(fail(
            "ANTENNA_REPORT",
            format!("schema {} unsupported", report.schema),
        ));
    }
    let diode = match &r.cell {
        Some(c) => profile.antenna_cells.iter().find(|d| &d.name == c),
        None if profile.antenna_cells.len() == 1 => profile.antenna_cells.first(),
        None => None,
    }
    .ok_or_else(|| {
        fail(
            "DELAY_PROFILE",
            "choose exactly one profiled antenna cell (--cell)",
        )
    })?;
    let mut by_net: BTreeMap<&str, Vec<&crate::antenna::CheckRow>> = BTreeMap::new();
    for row in report.checks.iter().filter(|c| c.violated) {
        by_net.entry(row.net.as_str()).or_default().push(row);
    }
    if report.status == "violated" && by_net.is_empty() {
        return Err(fail(
            "ANTENNA_REPORT",
            "violated report lists no violated rows",
        ));
    }
    let worst = |rows: &[&crate::antenna::CheckRow], k: usize| -> f64 {
        rows.iter()
            .map(|row| {
                let g = row.gate_area_um2 + k as f64 * diode.lef_gate_area_um2;
                let d = row.diff_area_um2 + k as f64 * diode.lef_diff_area_um2;
                match row.rules.limit(d) {
                    Some((limit, _)) => row.metal_um2 / row.rules.denominator(g, d) / limit,
                    None => f64::INFINITY,
                }
            })
            .fold(0.0, f64::max)
    };
    let mut seq = eco::next_sequence(&netlist, &r.prefix);
    let mut ops = Vec::new();
    let mut nets = Vec::new();
    let mut infeasible = Vec::new();
    for (net, rows) in &by_net {
        if eco::resolve_net(&netlist, net).is_err() {
            infeasible.push(format!(
                "{net}: not a net of the mapped netlist (clock-tree or generated nets are not repaired here)"
            ));
            continue;
        }
        let Some(k) = (r.min_diodes..=r.max_diodes_per_net).find(|&k| worst(rows, k) <= 1.0) else {
            infeasible.push(format!(
                "{net}: {} diodes leave ratio/limit {:.3}",
                r.max_diodes_per_net,
                worst(rows, r.max_diodes_per_net)
            ));
            continue;
        };
        let mut instances = Vec::new();
        for _ in 0..k {
            let name = format!("{}{seq}", r.prefix);
            seq += 1;
            instances.push(name.clone());
            ops.push(EcoOp::InsertDiode {
                cell: diode.name.clone(),
                instance: name,
                net: net.to_string(),
            });
        }
        nets.push(NetRepair {
            net: net.to_string(),
            violations: rows.len(),
            worst_ratio_over_limit: worst(rows, 0),
            diodes: k,
            estimated_worst_ratio_over_limit: worst(rows, k),
            instances,
        });
    }
    let eco_plan = EcoPlan {
        schema: ECO_PLAN_SCHEMA.into(),
        module: netlist.module.clone(),
        operations: ops,
    };
    let status = if !infeasible.is_empty() {
        "infeasible"
    } else if nets.is_empty() {
        "clean"
    } else {
        "planned"
    };
    let mut plan = AntennaPlan {
        schema: ANTENNA_PLAN_SCHEMA,
        status,
        tool: format!("rustroad {}", env!("CARGO_PKG_VERSION")),
        netlist: netlist_pin,
        report: artifact_of(&report_pin),
        profile: profile_pin,
        cell: diode.name.clone(),
        cell_gate_area_um2: diode.lef_gate_area_um2,
        cell_diff_area_um2: diode.lef_diff_area_um2,
        model: "each diode adds its LEF gate and diffusion area to the violating partial net; \
                re-layout, check-antenna and the foundry deck verify the result",
        nets,
        infeasible,
        eco: eco_plan,
        eco_report: None,
        output: None,
    };
    if plan.status != "infeasible" {
        let (edited, rep) = eco::apply(
            &netlist,
            &plan.eco,
            &cells,
            &EcoLimits::default(),
            &VerilogLimits::default(),
        )
        .map_err(|e| fail("ECO_APPLY", e))?;
        plan.output = Some(write_atomic(&r.out, &verilog_bytes(&edited)?)?);
        plan.eco_report = Some(rep);
    }
    write_atomic(&r.plan_out, &serde_json::to_vec_pretty(&plan)?)?;
    if plan.status == "infeasible" {
        return Err(fail(
            "ANTENNA_ECO_INFEASIBLE",
            format!(
                "{} (plan written to {})",
                plan.infeasible.join("; "),
                r.plan_out.display()
            ),
        ));
    }
    Ok(plan)
}
