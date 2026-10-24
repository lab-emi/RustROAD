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
//! Reports are bound to the netlist they timed (`--timed-netlist`, see
//! `binding`): the STA revision the reports name must record that file (by
//! xxh3-64) as its netlist source; stale reports, reports of another netlist
//! or revision and an edited netlist that the timed netlist does not refine
//! are refused with STALE_REPORT. Sink pins must be cell-LEF INPUT pins;
//! directions come from the profile's `cell_lef` source, re-verified by
//! SHA-256.
//!
//! Errors of these file-only commands carry their own code (STALE_REPORT,
//! REPORT_PROVENANCE, TIMING_REPORT, ECO_APPLY, ...), never INVALID_WORKSPACE.
//! No RustROAD workspace is read or written; hold-eco reads one revision
//! record of the STA workspace the reports name, under a shared lock.
//!
//! Memory/work preflight (Rule 9): reports and profiles are capped at 64 MiB /
//! 4 MiB, the cell LEF at 64 MiB (one parse; only the touched masters' pin
//! directions are kept), the STA revision record at 4 MiB; per sink the chain
//! search evaluates at most `max_combinations` multisets (default 200,000,
//! each O(max_chain x corners)); at most `max_sinks` sinks are planned. Peak
//! memory is two netlist ASTs plus the timed netlist text (16 MiB cap), the
//! LEF document and the parsed reports (well below 512 MiB at the input
//! caps). No timing graph, path or cone is built here; slacks come from the
//! timing engine.
mod binding;
pub use binding::BindingSummary;

use crate::pdk::read_input;
use crate::{Error, Result};
use rustroad_formats::eco::{
    self, BufferCell, DiodeCell, ECO_PLAN_SCHEMA, EcoCells, EcoLimits, EcoOp, EcoPlan, EcoReport,
    PinDirections, PinRef, SizeFamily,
};
use rustroad_formats::lef;
use rustroad_formats::verilog::{self, Expr, Netlist, VerilogLimits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub const DELAY_PROFILE_SCHEMA: &str = "rustroad.delay-cells.v1";
pub const SIZING_PROFILE_SCHEMA: &str = "rustroad.sizing-profile.v1";
pub const HOLD_PLAN_SCHEMA: &str = "rustroad.hold-eco-plan.v1";
const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NETLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LEF_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn fail(code: &'static str, message: impl std::fmt::Display) -> Error {
    Error::Input {
        code,
        message: message.to_string(),
    }
}

/// `read_input` for a command's own input file: a non-UTF-8 or oversized
/// file is an INPUT_FILE error of this command, not a workspace error.
fn read_file(path: &Path, limit: u64, role: &str) -> Result<(String, crate::pdk::SourcePin)> {
    read_input(path, limit, role).map_err(|e| match e {
        Error::Invalid(m) | Error::Budget(m) => fail("INPUT_FILE", m),
        other => other,
    })
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
            // Filled by `load_pin_directions` where an ECO buffers a sink.
            pin_directions: PinDirections::new(),
            // Filled by `load_sizing_profile` where an ECO resizes cells.
            families: Vec::new(),
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
    let (text, pin) = read_file(path, MAX_PROFILE_BYTES, "delay_cell_profile")?;
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

/// Cell-LEF pin directions of `masters`, read from the profile's `cell_lef`
/// source (or `override_path`) and verified against the profile's SHA-256.
pub fn load_pin_directions(
    profile: &DelayCellProfile,
    override_path: Option<&Path>,
    masters: &BTreeSet<String>,
) -> Result<(PinDirections, Artifact)> {
    let source = profile
        .sources
        .iter()
        .find(|s| s.role == "cell_lef")
        .ok_or_else(|| {
            fail(
                "ECO_PIN_DIRECTIONS",
                "the delay-cell profile lists no cell_lef source; sink pin directions are unknown",
            )
        })?;
    let path = override_path.unwrap_or(&source.path);
    let (text, pin) = read_file(path, MAX_LEF_BYTES, "cell_lef")?;
    if pin.sha256 != source.sha256 {
        return Err(fail(
            "ECO_PIN_DIRECTIONS",
            format!(
                "cell LEF {} has sha256 {}, the profile was built from {}",
                path.display(),
                pin.sha256,
                source.sha256
            ),
        ));
    }
    // Only names and directions are used; a fine DBU keeps every 0.1 nm
    // coordinate exact so the strict geometry parser accepts any library.
    let doc = lef::parse_lef(
        &text,
        10_000,
        &lef::LefLimits {
            max_input_bytes: MAX_LEF_BYTES as usize,
            ..Default::default()
        },
    )
    .map_err(|e| fail("ECO_PIN_DIRECTIONS", e))?;
    let directions = doc
        .macros
        .iter()
        .filter(|m| masters.contains(&m.name))
        .map(|m| {
            (
                m.name.clone(),
                m.pins
                    .iter()
                    .filter_map(|p| p.direction.clone().map(|d| (p.name.clone(), d)))
                    .collect(),
            )
        })
        .collect();
    Ok((directions, artifact_of(&pin)))
}

fn masters_of(netlists: &[&Netlist], profile: &DelayCellProfile) -> BTreeSet<String> {
    netlists
        .iter()
        .flat_map(|n| n.instances.iter().map(|i| i.cell.clone()))
        .chain(profile.buffers.iter().map(|b| b.name.clone()))
        .chain(profile.antenna_cells.iter().map(|d| d.name.clone()))
        .collect()
}

pub fn load_netlist(path: &Path) -> Result<(Netlist, Artifact)> {
    let (netlist, artifact, _) = load_netlist_text(path)?;
    Ok((netlist, artifact))
}

/// `load_netlist` that also returns the exact text it parsed.
fn load_netlist_text(path: &Path) -> Result<(Netlist, Artifact, String)> {
    let (text, pin) = read_file(path, MAX_NETLIST_BYTES, "mapped_verilog")?;
    let netlist = verilog::parse_verilog(&text, &VerilogLimits::default())
        .map_err(|e| fail("ECO_NETLIST", e))?;
    Ok((netlist, artifact_of(&pin), text))
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

/// The outputs may not overwrite an input, and the netlist and plan outputs
/// must be different files.
fn check_outputs(inputs: &[&Path], out: &Path, plan: &Path) -> Result<()> {
    if inputs
        .iter()
        .any(|input| same_file(input, out) || same_file(input, plan))
    {
        return Err(fail(
            "OUTPUT_PATH",
            "outputs must not overwrite an input netlist",
        ));
    }
    let resolve = |p: &Path| {
        p.parent()
            .map(|d| {
                if d.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    d
                }
            })
            .and_then(|d| fs::canonicalize(d).ok())
            .zip(p.file_name())
            .map(|(d, n)| d.join(n))
    };
    match (resolve(out), resolve(plan)) {
        (Some(a), Some(b)) if a == b => {
            Err(fail("OUTPUT_PATH", "--out and --plan-out must differ"))
        }
        (Some(_), Some(_)) => Ok(()),
        _ => Err(fail("OUTPUT_PATH", "output directories must exist")),
    }
}

// ----------------------------------------------------------------- reports

#[derive(Debug, Deserialize)]
struct Envelope {
    schema: String,
    ok: bool,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    rev: Option<String>,
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
    let (text, pin) = read_file(path, MAX_REPORT_BYTES, "timing_report")?;
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
    /// The netlist the timing engine imported (post-CTS export of `netlist`).
    pub timed_netlist: PathBuf,
    /// SHA-256 the caller recorded when it handed `timed_netlist` to the
    /// timing engine; a different file is refused.
    pub timed_netlist_sha256: Option<String>,
    /// Override of the profile's cell-LEF path (same SHA-256 required).
    pub cell_lef: Option<PathBuf>,
    /// `ruststa report --section pins --corner C --limit 0` of the same STA
    /// revision (one per setup corner; the setup corner is required). Given:
    /// path-aware placement (delay goes on the worst hold path where the
    /// setup slack of every path through the point allows it). Empty: one
    /// chain in front of each violating endpoint pin (the original planner).
    pub pins_reports: Vec<PathBuf>,
    /// Apply the feasible part of a plan and report the rest (`partial`)
    /// instead of failing the whole plan with HOLD_ECO_INFEASIBLE.
    pub allow_partial: bool,
    /// Largest number of sinks a net-level point (delay in front of every
    /// sink of one net, e.g. an input port) may move.
    pub max_group_sinks: usize,
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
    /// Path-aware mode: the point that carries this endpoint's delay
    /// (`sink_pin:<inst/pin>` or `net:<net>`), also when an earlier
    /// endpoint's point already covers it (then `cells` is empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<String>,
}
/// One insertion point of a path-aware hold plan.
#[derive(Clone, Debug, Serialize)]
pub struct PointPlan {
    /// `sink_pin` (one input pin moves behind the chain) or `net` (every
    /// input-pin sink of the net moves behind it).
    pub kind: &'static str,
    pub name: String,
    /// The pin whose setup slack bounds the point (the sink pin, or the
    /// net's driver pin / input port).
    pub setup_pin: String,
    pub setup_slack_ns: BTreeMap<String, f64>,
    pub sinks: Vec<String>,
    pub existing_chain: Vec<String>,
    pub cells: Vec<String>,
    pub instances: Vec<String>,
    pub area_um2: f64,
    /// Violating endpoints whose worst hold path crosses the point.
    pub endpoints: Vec<String>,
    pub added_min_ns: BTreeMap<String, f64>,
    pub added_setup_ns: BTreeMap<String, f64>,
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
    pub path_aware: bool,
    pub allow_partial: bool,
    pub max_group_sinks: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct HoldPlanInputs {
    pub netlist: Artifact,
    pub timed_netlist: Artifact,
    pub cell_lef: Artifact,
    pub profile: Artifact,
    pub reports: Vec<ReportInput>,
    pub binding: BindingSummary,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<PointPlan>,
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
use binding::expr_key;

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
    choose_chain_multi(
        candidates,
        need,
        hold_corners,
        &[(setup_corner.to_string(), setup_slack)],
        setup_margin,
        max_chain,
        max_combinations,
    )
}

/// `choose_chain` with a setup budget per setup corner: (corner, slack).
fn choose_chain_multi(
    candidates: &[&ProfiledBuffer],
    need: &BTreeMap<String, f64>,
    hold_corners: &[String],
    setup: &[(String, f64)],
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
            let setup_ok = setup.iter().all(|(c, slack)| {
                let added: f64 = idx.iter().map(|&i| candidates[i].delay_ns[c].max()).sum();
                slack - added >= setup_margin
            });
            if covers && setup_ok {
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

/// One stage of a hold_fix worst path, as the planner needs it.
#[derive(Clone, Debug)]
struct Stage {
    pin: String,
    inst: Option<String>,
    arc: String,
    driver: Option<String>,
}

fn stages_of(row: &serde_json::Value) -> Vec<Stage> {
    row.get("path")
        .and_then(|p| p.get("stages"))
        .and_then(serde_json::Value::as_array)
        .map(|st| {
            st.iter()
                .map(|x| Stage {
                    pin: binding::norm(x.get("pin").and_then(|v| v.as_str()).unwrap_or("")),
                    inst: x.get("inst").and_then(|v| v.as_str()).map(binding::norm),
                    arc: x
                        .get("arc")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    driver: x
                        .get("net_driver")
                        .and_then(|v| v.as_str())
                        .map(binding::norm),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A place a delay chain can go on a worst hold path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Point {
    /// Every input-pin sink of the net (edited-netlist expression key).
    Net(String),
    /// One sink pin `inst/pin`.
    Pin(String),
}
impl Point {
    fn label(&self) -> String {
        match self {
            Point::Net(n) => format!("net:{n}"),
            Point::Pin(p) => format!("sink_pin:{p}"),
        }
    }
}

struct PathInputs<'a> {
    r: &'a HoldEcoRequest,
    timing: &'a BTreeMap<String, EndpointTiming>,
    paths: &'a BTreeMap<String, BTreeMap<String, Vec<Stage>>>,
    pin_slack: &'a BTreeMap<String, BTreeMap<String, f64>>,
    candidates: &'a [&'a ProfiledBuffer],
    cells: &'a EcoCells,
    netlist: &'a Netlist,
    drivers: &'a HashMap<String, &'a verilog::Instance>,
    inputs: &'a HashMap<&'a str, &'a str>,
}
struct PathResult {
    points: Vec<PointPlan>,
    sinks: Vec<SinkPlan>,
    infeasible: Vec<String>,
    violating: usize,
}
/// Hard bound of chain searches of one path-aware plan.
const MAX_POINT_EVALUATIONS: usize = 200_000;

/// Path-aware hold planning. For each violating endpoint (largest need
/// first) the candidate points are the sink pins of its worst hold path and,
/// for each of them, the whole net feeding it (all input-pin sinks of that
/// net move behind one chain: e.g. an input port when all its sinks need
/// hold). A point is kept only if it lies on the worst path of every corner
/// where the endpoint violates, so its delay is credited at each of them.
/// The setup budget of a point is the setup slack of the pin it delays every
/// path through (the sink pin, or the net's driver), at every setup corner of
/// a pins report, minus the delay already planned on points of the same
/// path. Among the feasible points the one on the most violating paths
/// wins, then the most downstream one (it delays every parallel branch
/// after a reconvergence), then the smallest chain. Delay planned
/// on a shared point is credited to every later endpoint whose path crosses
/// it. An endpoint with no feasible point is infeasible, with the budget of
/// each of its points as evidence.
fn plan_paths(x: &PathInputs) -> Result<PathResult> {
    let r = x.r;
    let instances: HashMap<&str, &verilog::Instance> = x
        .netlist
        .instances
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    // Input-pin sinks of every single-bit net of the edited netlist.
    let mut net_sinks: HashMap<String, Vec<String>> = HashMap::new();
    for i in &x.netlist.instances {
        for c in &i.connections {
            if matches!(c.signal, Expr::Ident(_) | Expr::BitSelect { .. })
                && x.cells.check_sink(&i.cell, &i.name, &c.pin).is_ok()
            {
                net_sinks
                    .entry(expr_key(&c.signal))
                    .or_default()
                    .push(format!("{}/{}", i.name, c.pin));
            }
        }
    }
    let setup_corners: Vec<&String> = x.pin_slack.keys().collect();
    let pin_point = |pin: &str| -> Option<(Point, String, String)> {
        let (inst, p) = pin.rsplit_once('/')?;
        let i = instances.get(inst)?;
        let c = i.connections.iter().find(|c| c.pin == p)?;
        if !matches!(c.signal, Expr::Ident(_) | Expr::BitSelect { .. })
            || x.cells.check_sink(&i.cell, inst, p).is_err()
        {
            return None;
        }
        Some((
            Point::Pin(pin.to_string()),
            pin.to_string(),
            expr_key(&c.signal),
        ))
    };
    // Points of one endpoint at one corner, in path order: (point, setup pin).
    let points_of = |stages: &[Stage]| -> Vec<(Point, String)> {
        let mut out = Vec::new();
        for st in stages {
            if st.arc != "net" || st.inst.is_none() {
                continue;
            }
            let Some((pp, pin, key)) = pin_point(&st.pin) else {
                continue;
            };
            if let Some(d) = &st.driver {
                let n = net_sinks.get(&key).map_or(0, Vec::len);
                if n >= 1 && n <= r.max_group_sinks {
                    out.push((Point::Net(key), d.clone()));
                }
            }
            out.push((pp, pin));
        }
        out
    };
    // Pass 1: needs and candidate points of every violating endpoint.
    struct Want {
        endpoint: String,
        need: BTreeMap<String, f64>,
        points: Vec<(Point, String)>,
    }
    let mut wants = Vec::new();
    let mut infeasible = Vec::new();
    for (endpoint, t) in x.timing {
        let need: BTreeMap<String, f64> = r
            .hold_corners
            .iter()
            .filter_map(|c| t.hold.get(c).map(|s| (c.clone(), r.target_margin_ns - s)))
            .filter(|(_, n)| *n > 0.0)
            .collect();
        if need.is_empty() {
            continue;
        }
        if wants.len() + infeasible.len() >= r.max_sinks {
            return Err(Error::Budget(format!(
                "HOLD_ECO: more than {} violating endpoints",
                r.max_sinks
            )));
        }
        let by_corner = x.paths.get(endpoint);
        let worst = need
            .iter()
            .max_by(|a, b| a.1.total_cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(c, _)| c.clone())
            .expect("need is not empty");
        let mut pts = by_corner
            .and_then(|m| m.get(&worst))
            .map(|st| points_of(st))
            .unwrap_or_default();
        for c in need.keys() {
            if *c == worst {
                continue;
            }
            if let Some(st) = by_corner.and_then(|m| m.get(c)) {
                let here: BTreeSet<Point> = points_of(st).into_iter().map(|p| p.0).collect();
                pts.retain(|p| here.contains(&p.0));
            }
        }
        if pts.is_empty() {
            match pin_point(endpoint) {
                Some((pp, pin, _)) => pts.push((pp, pin)),
                None => {
                    infeasible.push(format!(
                        "{endpoint}: needs {need:?} ns but its worst hold path has no sink pin of the \
                         edited netlist to delay (top-level port endpoint or constant/unknown pins)"
                    ));
                    continue;
                }
            }
        }
        wants.push(Want {
            endpoint: endpoint.clone(),
            need,
            points: pts,
        });
    }
    let violating = wants.len() + infeasible.len();
    // Largest need at the setup corner's worst first (then name).
    let key_need = |w: &Want| w.need.values().copied().fold(0.0f64, f64::max);
    wants.sort_by(|a, b| {
        key_need(b)
            .total_cmp(&key_need(a))
            .then_with(|| a.endpoint.cmp(&b.endpoint))
    });
    let mut coverage: HashMap<Point, Vec<String>> = HashMap::new();
    for w in &wants {
        for (p, _) in &w.points {
            coverage
                .entry(p.clone())
                .or_default()
                .push(w.endpoint.clone());
        }
    }
    // Existing chain feeding a point (earlier hold ECO cells, walked back).
    let existing_of = |p: &Point| -> Vec<String> {
        let mut net = match p {
            Point::Net(k) => k.clone(),
            Point::Pin(pin) => match pin_point(pin) {
                Some((_, _, k)) => k,
                None => return Vec::new(),
            },
        };
        let mut chain = Vec::new();
        while let Some(d) = x.drivers.get(&net) {
            if !d.name.starts_with(&r.prefix) || chain.len() > r.max_total_chain {
                break;
            }
            chain.push(d.name.clone());
            let input = x.inputs[d.cell.as_str()];
            net = match d.connections.iter().find(|c| c.pin == input) {
                Some(c) => expr_key(&c.signal),
                None => break,
            };
        }
        chain.reverse();
        chain
    };
    // point -> (setup pin, chosen candidate indices in plan order)
    let mut planned: BTreeMap<Point, (String, Vec<usize>, BTreeSet<String>)> = BTreeMap::new();
    let mut sinks = Vec::new();
    let mut evaluations = 0usize;
    for w in &wants {
        let on_path: Vec<&Point> = w.points.iter().map(|p| &p.0).collect();
        let credit =
            |c: &str, planned: &BTreeMap<Point, (String, Vec<usize>, BTreeSet<String>)>| -> f64 {
                on_path
                    .iter()
                    .filter_map(|p| planned.get(*p))
                    .flat_map(|(_, idx, _)| idx.iter())
                    .map(|&i| x.candidates[i].delay_ns[c].min())
                    .sum()
            };
        let used_setup =
            |c: &str, planned: &BTreeMap<Point, (String, Vec<usize>, BTreeSet<String>)>| -> f64 {
                on_path
                    .iter()
                    .filter_map(|p| planned.get(*p))
                    .flat_map(|(_, idx, _)| idx.iter())
                    .map(|&i| x.candidates[i].delay_ns[c].max())
                    .sum()
            };
        let remaining: BTreeMap<String, f64> = w
            .need
            .iter()
            .map(|(c, n)| (c.clone(), n - credit(c, &planned)))
            .filter(|(_, n)| *n > 1e-12)
            .collect();
        let t = &x.timing[&w.endpoint];
        let ep_setup = t.setup.get(&r.setup_corner).copied().unwrap_or(f64::NAN);
        let (inst, pin) = w
            .endpoint
            .rsplit_once('/')
            .map_or((String::new(), String::new()), |(a, b)| {
                (a.to_string(), b.to_string())
            });
        if remaining.is_empty() {
            let by = w
                .points
                .iter()
                .find(|p| planned.contains_key(&p.0))
                .map(|p| p.0.label());
            if let Some(by) = &by
                && let Some(e) =
                    planned.get_mut(&w.points.iter().find(|p| p.0.label() == *by).unwrap().0)
            {
                e.2.insert(w.endpoint.clone());
            }
            sinks.push(SinkPlan {
                endpoint: w.endpoint.clone(),
                instance: inst,
                pin,
                driver_net: String::new(),
                existing_chain: Vec::new(),
                cells: Vec::new(),
                area_um2: 0.0,
                corners: estimate(&w.need, t, |c| credit(c, &planned)),
                setup_slack_ns: ep_setup,
                estimated_setup_slack_ns: ep_setup - used_setup(&r.setup_corner, &planned),
                combinations_evaluated: 0,
                point: by,
            });
            continue;
        }
        // Rank points: most violating paths, then the most downstream (a
        // point after a reconvergence delays every parallel branch of the
        // endpoint at once; an upstream branch pin fixes only its branch).
        let mut order: Vec<(usize, &(Point, String))> = w.points.iter().enumerate().collect();
        order.sort_by(|a, b| {
            let ca = coverage.get(&a.1.0).map_or(0, Vec::len);
            let cb = coverage.get(&b.1.0).map_or(0, Vec::len);
            cb.cmp(&ca).then(b.0.cmp(&a.0))
        });
        let mut chosen = None;
        let mut evidence = Vec::new();
        let mut combos = 0usize;
        for (_, (point, setup_pin)) in &order {
            let mut budget = Vec::new();
            let mut known = true;
            for c in &setup_corners {
                match x.pin_slack[*c].get(setup_pin) {
                    Some(s) => budget.push(((*c).clone(), s - used_setup(c, &planned))),
                    None => known = false,
                }
            }
            let shown: Vec<String> = budget.iter().map(|(c, b)| format!("{c} {b:.4}")).collect();
            if !known {
                evidence.push(format!(
                    "{} (setup pin {setup_pin}: no setup slack)",
                    point.label()
                ));
                continue;
            }
            let have = existing_of(point).len() + planned.get(point).map_or(0, |e| e.1.len());
            let room = r.max_total_chain.saturating_sub(have).min(r.max_chain);
            if room == 0 {
                evidence.push(format!(
                    "{} (chain cap {} reached)",
                    point.label(),
                    r.max_total_chain
                ));
                continue;
            }
            evaluations += 1;
            if evaluations > MAX_POINT_EVALUATIONS {
                return Err(Error::Budget(format!(
                    "HOLD_ECO: more than {MAX_POINT_EVALUATIONS} point evaluations"
                )));
            }
            let (choice, n) = choose_chain_multi(
                x.candidates,
                &remaining,
                &r.hold_corners,
                &budget,
                r.setup_margin_ns,
                room,
                r.max_combinations,
            )?;
            combos += n;
            match choice {
                Some(c) => {
                    chosen = Some(((*point).clone(), setup_pin.clone(), c));
                    break;
                }
                None => evidence.push(format!(
                    "{} (setup budget {})",
                    point.label(),
                    shown.join(", ")
                )),
            }
        }
        let Some((point, setup_pin, choice)) = chosen else {
            infeasible.push(format!(
                "{}: needs {remaining:?} ns more hold at the worst-path points, none of which has \
                 the setup room (setup margin {} ns): {}",
                w.endpoint,
                r.setup_margin_ns,
                evidence.join("; ")
            ));
            continue;
        };
        let e = planned
            .entry(point.clone())
            .or_insert_with(|| (setup_pin, Vec::new(), BTreeSet::new()));
        e.1.extend(choice.iter().copied());
        e.2.insert(w.endpoint.clone());
        let mut names: Vec<String> = choice
            .iter()
            .map(|&i| x.candidates[i].name.clone())
            .collect();
        names.sort();
        sinks.push(SinkPlan {
            endpoint: w.endpoint.clone(),
            instance: inst,
            pin,
            driver_net: String::new(),
            existing_chain: existing_of(&point),
            area_um2: choice.iter().map(|&i| x.candidates[i].area_um2).sum(),
            cells: names,
            corners: estimate(&w.need, t, |c| credit(c, &planned)),
            setup_slack_ns: ep_setup,
            estimated_setup_slack_ns: ep_setup - used_setup(&r.setup_corner, &planned),
            combinations_evaluated: combos,
            point: Some(point.label()),
        });
    }
    let mut points = Vec::new();
    for (point, (setup_pin, idx, endpoints)) in planned {
        let mut chosen: Vec<&ProfiledBuffer> = idx.iter().map(|&i| x.candidates[i]).collect();
        chosen.sort_by(|a, b| a.name.cmp(&b.name));
        let sum = |c: &str, max: bool| -> f64 {
            chosen
                .iter()
                .map(|b| {
                    if max {
                        b.delay_ns[c].max()
                    } else {
                        b.delay_ns[c].min()
                    }
                })
                .sum()
        };
        let (kind, name, moved) = match &point {
            Point::Net(k) => (
                "net",
                k.clone(),
                net_sinks.get(k).cloned().unwrap_or_default(),
            ),
            Point::Pin(p) => ("sink_pin", p.clone(), vec![p.clone()]),
        };
        points.push(PointPlan {
            kind,
            existing_chain: existing_of(&point),
            name,
            setup_slack_ns: setup_corners
                .iter()
                .filter_map(|c| x.pin_slack[*c].get(&setup_pin).map(|s| ((*c).clone(), *s)))
                .collect(),
            setup_pin,
            sinks: moved,
            cells: chosen.iter().map(|b| b.name.clone()).collect(),
            instances: Vec::new(),
            area_um2: chosen.iter().map(|b| b.area_um2).sum(),
            endpoints: endpoints.into_iter().collect(),
            added_min_ns: r
                .hold_corners
                .iter()
                .map(|c| (c.clone(), sum(c, false)))
                .collect(),
            added_setup_ns: setup_corners
                .iter()
                .map(|c| ((*c).clone(), sum(c, true)))
                .collect(),
        });
    }
    // Net points first, so a later sink-pin chain on one of their sinks
    // extends the moved connection.
    points.sort_by(|a, b| (a.kind != "net", &a.name).cmp(&(b.kind != "net", &b.name)));
    Ok(PathResult {
        points,
        sinks,
        infeasible,
        violating,
    })
}

fn estimate(
    need: &BTreeMap<String, f64>,
    t: &EndpointTiming,
    credit: impl Fn(&str) -> f64,
) -> BTreeMap<String, CornerEstimate> {
    need.keys()
        .map(|c| {
            let slack = t.hold.get(c).copied();
            let added = credit(c);
            (
                c.clone(),
                CornerEstimate {
                    hold_slack_ns: slack,
                    need_ns: need[c],
                    added_min_ns: added,
                    added_max_ns: added,
                    estimated_hold_slack_ns: slack.map(|s| s + added),
                },
            )
        })
        .collect()
}

/// ECO operations of one planned point: a net point moves every listed sink
/// behind the first cell (`buffer_sinks`), a sink-pin point buffers the pin;
/// further cells extend the chain on the driver side of the first one.
fn emit_point_ops(
    r: &HoldEcoRequest,
    cells: &EcoCells,
    pt: &mut PointPlan,
    seq: &mut usize,
    ops: &mut Vec<EcoOp>,
) -> Result<()> {
    let mut previous: Option<String> = None;
    for cell in &pt.cells {
        let name = format!("{}{}", r.prefix, *seq);
        *seq += 1;
        if !cells.buffers.iter().any(|b| b.name == *cell) {
            return Err(fail(
                "DELAY_PROFILE",
                format!("{cell} is not a profiled buffer"),
            ));
        }
        let op = match (&previous, pt.kind) {
            (None, "net") => EcoOp::BufferSinks {
                cell: cell.clone(),
                instance: name.clone(),
                net: format!("{name}_n"),
                sinks: pt
                    .sinks
                    .iter()
                    .map(|s| {
                        let (i, p) = s.rsplit_once('/').expect("instance pin");
                        PinRef {
                            instance: i.to_string(),
                            pin: p.to_string(),
                        }
                    })
                    .collect(),
                ports: Vec::new(),
            },
            (None, _) => {
                let (i, p) = pt.name.rsplit_once('/').expect("instance pin");
                EcoOp::InsertBuffer {
                    cell: cell.clone(),
                    instance: name.clone(),
                    net: format!("{name}_n"),
                    sink: PinRef {
                        instance: i.to_string(),
                        pin: p.to_string(),
                    },
                }
            }
            (Some(prev), _) => EcoOp::InsertBuffer {
                cell: cell.clone(),
                instance: name.clone(),
                net: format!("{name}_n"),
                sink: PinRef {
                    instance: prev.clone(),
                    pin: input_of(cells, prev, ops),
                },
            },
        };
        ops.push(op);
        pt.instances.push(name.clone());
        previous = Some(name);
    }
    Ok(())
}

/// Input pin of the buffer instance `name` inserted earlier in `ops`.
fn input_of(cells: &EcoCells, name: &str, ops: &[EcoOp]) -> String {
    ops.iter()
        .find_map(|op| match op {
            EcoOp::InsertBuffer { cell, instance, .. }
            | EcoOp::BufferSinks { cell, instance, .. }
                if instance == name =>
            {
                cells
                    .buffers
                    .iter()
                    .find(|b| b.name == *cell)
                    .map(|b| b.input_pin.clone())
            }
            _ => None,
        })
        .expect("chain cell inserted before")
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
    if r.max_group_sinks == 0 || r.max_group_sinks > 256 {
        return Err(fail(
            "INVALID_ARGUMENT",
            "max_group_sinks outside hard bounds 1..=256",
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
    check_outputs(&[&r.netlist, &r.timed_netlist], &r.out, &r.plan_out)?;
    let (netlist, netlist_pin) = load_netlist(&r.netlist)?;
    let (timed, timed_pin, timed_text) = load_netlist_text(&r.timed_netlist)?;
    let (profile, profile_pin) = load_profile(&r.profile)?;
    let (directions, lef_pin) = load_pin_directions(
        &profile,
        r.cell_lef.as_deref(),
        &masters_of(&[&netlist, &timed], &profile),
    )?;
    let mut cells = profile.eco_cells()?;
    cells.pin_directions = directions;
    let mut bind = BindingSummary {
        method: "the STA revision the reports name records the timed netlist as its netlist \
                 source (xxh3-64, from <workspace>/revisions/<rev>/meta.json under a shared \
                 lock) + sha256 of the timed netlist (when given) + report rows and path \
                 stages present in the timed netlist + one STA revision + edited netlist \
                 refined by the timed netlist up to inserted buffers/ties",
        timed_netlist_sha256: timed_pin.sha256.clone(),
        expected_sha256: r.timed_netlist_sha256.clone(),
        ..BindingSummary::default()
    };
    if let Some(want) = &r.timed_netlist_sha256
        && !want.eq_ignore_ascii_case(&timed_pin.sha256)
    {
        return Err(fail(
            "STALE_REPORT",
            format!(
                "timed netlist {} has sha256 {}, but the reports were produced from {want}",
                r.timed_netlist.display(),
                timed_pin.sha256
            ),
        ));
    }
    let tindex = binding::NetIndex::new(&timed);
    binding::check_refinement(
        &netlist,
        &timed,
        &tindex,
        &cells.pin_directions,
        &r.prefix,
        &mut bind,
    )?;
    let mut revision: Option<(String, String)> = None;
    // The first report fixes the revision; its record must name the timed
    // netlist before any row is bound (provenance first, content second).
    let mut same_revision = |env: &Envelope,
                             path: &Path,
                             bind: &mut BindingSummary|
     -> Result<()> {
        let (Some(w), Some(v)) = (env.workspace.clone(), env.rev.clone()) else {
            return Err(fail(
                "TIMING_REPORT",
                format!("{} names no STA workspace/revision", path.display()),
            ));
        };
        match &revision {
            None => {
                binding::bind_sta_record(&w, &v, &r.timed_netlist, timed_text.as_bytes(), bind)?;
                revision = Some((w, v));
                Ok(())
            }
            Some(first) if *first == (w.clone(), v.clone()) => Ok(()),
            Some(first) => Err(fail(
                "STALE_REPORT",
                format!(
                    "{} comes from {w} {v}, other reports from {} {}",
                    path.display(),
                    first.0,
                    first.1
                ),
            )),
        }
    };
    let mut timing: BTreeMap<String, EndpointTiming> = BTreeMap::new();
    // endpoint -> corner -> worst hold path stages (path-aware planning).
    let mut paths: BTreeMap<String, BTreeMap<String, Vec<Stage>>> = BTreeMap::new();
    let mut reports = Vec::new();
    let mut hold_seen = BTreeSet::new();
    for path in &r.hold_reports {
        let (env, artifact) = read_envelope(path)?;
        same_revision(&env, path, &mut bind)?;
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
                binding::bind_hold_row(&tindex, e, &mut bind)?;
                paths
                    .entry(name.to_string())
                    .or_default()
                    .insert(corner.clone(), stages_of(e));
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
        same_revision(&env, path, &mut bind)?;
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
            binding::bind_endpoint(&tindex, name, &mut bind)?;
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
    // corner -> pin/port -> setup slack (pins reports, path-aware mode).
    let mut pin_slack: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for path in &r.pins_reports {
        let (env, artifact) = read_envelope(path)?;
        same_revision(&env, path, &mut bind)?;
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
            .ok_or_else(|| fail("TIMING_REPORT", "pins report has no scope.corner"))?
            .to_string();
        let e = env.sections.get("pins").ok_or_else(|| {
            fail(
                "TIMING_REPORT",
                format!("{} has no pins section", path.display()),
            )
        })?;
        let total = num(e, "total", "pins")?;
        let rows = e
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| fail("TIMING_REPORT", "pins rows missing"))?;
        if num(e, "matched", "pins")? != total || rows.len() as f64 != total {
            return Err(fail(
                "TIMING_REPORT",
                "pins report is filtered or truncated; use --limit 0 and no --filter",
            ));
        }
        let map = pin_slack.entry(corner.clone()).or_default();
        for row in rows {
            let name = text(row, "pin", "pins row")?;
            binding::bind_pin(&tindex, name, &mut bind)?;
            if let Some(s) = row.get("slack_ns").and_then(serde_json::Value::as_f64) {
                map.insert(binding::norm(name), s);
            }
        }
        reports.push(ReportInput {
            artifact,
            kind: "pins",
            corners: vec![corner],
            wire_model: wire,
            clock_model: clock,
        });
    }
    if let Some((w, v)) = revision {
        bind.sta_workspace = w;
        bind.sta_revision = v;
    }
    let path_aware = !r.pins_reports.is_empty();
    if path_aware && !pin_slack.contains_key(&r.setup_corner) {
        return Err(fail(
            "MISSING_CORNER",
            format!("no pins report covers setup corner {}", r.setup_corner),
        ));
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
    let mut points = Vec::new();
    if path_aware {
        let planned = plan_paths(&PathInputs {
            r,
            timing: &timing,
            paths: &paths,
            pin_slack: &pin_slack,
            candidates: &candidates,
            cells: &cells,
            netlist: &netlist,
            drivers: &drivers,
            inputs: &inputs,
        })?;
        for mut pt in planned.points {
            emit_point_ops(r, &cells, &mut pt, &mut seq, &mut ops)?;
            points.push(pt);
        }
        sinks = planned.sinks;
        infeasible = planned.infeasible;
        violating = planned.violating;
    }
    for (endpoint, t) in timing.iter().filter(|_| !path_aware) {
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
            point: None,
        });
    }
    let eco_plan = EcoPlan {
        schema: ECO_PLAN_SCHEMA.into(),
        module: netlist.module.clone(),
        operations: ops,
    };
    let status = if !infeasible.is_empty() {
        if r.allow_partial && !eco_plan.operations.is_empty() {
            "partial"
        } else {
            "infeasible"
        }
    } else if eco_plan.operations.is_empty() {
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
            timed_netlist: timed_pin,
            cell_lef: lef_pin,
            profile: profile_pin,
            reports,
            binding: bind,
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
            path_aware,
            allow_partial: r.allow_partial,
            max_group_sinks: r.max_group_sinks,
        },
        totals: HoldPlanTotals {
            violating_sinks: violating,
            planned_sinks: sinks.len(),
            cells: eco_plan.operations.len(),
            area_um2: if path_aware {
                points.iter().map(|p| p.area_um2).sum()
            } else {
                sinks.iter().map(|s| s.area_um2).sum()
            },
        },
        sinks,
        points,
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

// ----------------------------------------------------------------- sizing

/// One member of a sizing family (scripts/build_sizing_profile.py).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizedCell {
    pub name: String,
    pub width_dbu: i64,
    pub area_um2: f64,
    pub max_capacitance_pf: Option<f64>,
    /// Liberty-derived drive data per corner (planner input only; the ECO
    /// itself never reads it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive: Option<serde_json::Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizingFamily {
    pub id: String,
    pub order: String,
    pub pins: BTreeMap<String, String>,
    pub functions: BTreeMap<String, String>,
    pub sequential: serde_json::Value,
    pub cells: Vec<SizedCell>,
}
/// `fixtures/pdk/<pdk>-sizing.json`: drive-strength families from Liberty.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizingProfile {
    pub schema: String,
    pub pdk: String,
    pub library: String,
    pub generator: String,
    pub sources: Vec<ProfileSource>,
    pub dbu_per_um: u32,
    pub rule: String,
    /// How `cells[].drive` was derived (absent in profiles without drive data).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_model: Option<serde_json::Value>,
    pub families: Vec<SizingFamily>,
    pub excluded: Vec<serde_json::Value>,
}

/// Load and check a sizing profile: every family has members with the same
/// declared pins, every output pin has a function, no master is listed twice.
pub fn load_sizing_profile(path: &Path) -> Result<(Vec<SizeFamily>, Artifact)> {
    let (text, pin) = read_file(path, MAX_PROFILE_BYTES, "sizing_profile")?;
    let profile: SizingProfile = serde_json::from_str(&text)?;
    if profile.schema != SIZING_PROFILE_SCHEMA {
        return Err(fail(
            "SIZING_PROFILE",
            format!("schema {} is not {SIZING_PROFILE_SCHEMA}", profile.schema),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(profile.families.len());
    for f in &profile.families {
        let outputs: BTreeSet<&String> = f
            .pins
            .iter()
            .filter(|(_, d)| d.as_str() == "output")
            .map(|(p, _)| p)
            .collect();
        let functions: BTreeSet<&String> = f.functions.keys().collect();
        if f.cells.is_empty()
            || outputs.is_empty()
            || outputs != functions
            || f.pins.values().any(|d| d != "input" && d != "output")
            || f.cells
                .iter()
                .any(|c| c.width_dbu <= 0 || !seen.insert(c.name.clone()))
        {
            return Err(fail(
                "SIZING_PROFILE",
                format!("family {} is malformed (pins, functions or members)", f.id),
            ));
        }
        out.push(SizeFamily {
            id: f.id.clone(),
            pins: f.pins.clone(),
            cells: f.cells.iter().map(|c| c.name.clone()).collect(),
        });
    }
    Ok((out, artifact_of(&pin)))
}

/// Non-inverting buffers of a sizing profile: every member of a family with
/// exactly one input and one output pin whose Liberty function (normalized by
/// the profile builder) is that input. `buffer_sinks`/`insert_buffer` accept
/// them after `BufferCell::check_identity` (the profile excludes cells the
/// cell policy reserves or forbids).
pub fn sizing_buffers(path: &Path) -> Result<Vec<BufferCell>> {
    let (text, _) = read_file(path, MAX_PROFILE_BYTES, "sizing_profile")?;
    let profile: SizingProfile = serde_json::from_str(&text)?;
    let mut out = Vec::new();
    for f in &profile.families {
        let inputs: Vec<&String> = f
            .pins
            .iter()
            .filter(|(_, d)| d.as_str() == "input")
            .map(|(p, _)| p)
            .collect();
        let outputs: Vec<&String> = f
            .pins
            .iter()
            .filter(|(_, d)| d.as_str() == "output")
            .map(|(p, _)| p)
            .collect();
        if inputs.len() != 1 || outputs.len() != 1 || f.pins.len() != 2 {
            continue;
        }
        let function = f.functions.get(outputs[0]).cloned().unwrap_or_default();
        for c in &f.cells {
            let b = BufferCell {
                name: c.name.clone(),
                input_pin: inputs[0].clone(),
                output_pin: outputs[0].clone(),
                function: function.clone(),
            };
            if b.check_identity().is_ok() {
                out.push(b);
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------- apply

#[derive(Clone, Debug, Serialize)]
pub struct ApplyReport {
    pub schema: &'static str,
    pub netlist: Artifact,
    pub plan: Artifact,
    pub profile: Artifact,
    pub cell_lef: Artifact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sizing_profile: Option<Artifact>,
    pub eco_report: EcoReport,
    pub output: Artifact,
}

/// Apply a given typed ECO plan (a planted control, or a sizing plan with
/// `sizing`: the sizing profile `resize_cell` operations are checked against)
/// and write the netlist.
pub fn apply_plan(
    netlist: &Path,
    plan: &Path,
    profile: &Path,
    cell_lef: Option<&Path>,
    sizing: Option<&Path>,
    out: &Path,
) -> Result<ApplyReport> {
    if same_file(netlist, out) {
        return Err(fail(
            "OUTPUT_PATH",
            "--out must not overwrite the input netlist",
        ));
    }
    let (n, npin) = load_netlist(netlist)?;
    let (text, ppin) = read_file(plan, MAX_REPORT_BYTES, "eco_plan")?;
    let p: EcoPlan = serde_json::from_str(&text)?;
    let (prof, fpin) = load_profile(profile)?;
    let mut cells = prof.eco_cells()?;
    let sizing_profile = match sizing {
        Some(path) => {
            let (families, art) = load_sizing_profile(path)?;
            cells.families = families;
            // Drive-strength buffers of the sizing profile (repeaters, port
            // buffers); a master already profiled as a delay cell keeps its
            // delay-profile entry.
            for b in sizing_buffers(path)? {
                if !cells.buffers.iter().any(|x| x.name == b.name) {
                    cells.buffers.push(b);
                }
            }
            Some(art)
        }
        None => None,
    };
    let mut masters = masters_of(&[&n], &prof);
    masters.extend(cells.buffers.iter().map(|b| b.name.clone()));
    let (directions, lef_pin) = load_pin_directions(&prof, cell_lef, &masters)?;
    cells.pin_directions = directions;
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
        cell_lef: lef_pin,
        sizing_profile,
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
    safety_margin: crate::antenna::SafetyMargin,
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
    /// infeasible | planned | partial (input report partial, nothing to
    /// repair: unmodeled rules, never "clean") | clean
    pub status: &'static str,
    /// Status of the check-antenna report this plan was made from.
    pub input_status: String,
    /// Margin of the input report; diodes are sized to its effective limit.
    pub safety_margin: f64,
    pub diagnostics: Vec<String>,
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
    check_outputs(&[&r.netlist], &r.out, &r.plan_out)?;
    let (netlist, netlist_pin) = load_netlist(&r.netlist)?;
    let (profile, profile_pin) = load_profile(&r.profile)?;
    let cells = profile.eco_cells()?;
    let (text, report_pin) = read_file(&r.report, MAX_REPORT_BYTES, "antenna_report")?;
    let report: AntennaReportIn = serde_json::from_str(&text)?;
    if report.schema != crate::antenna::ANTENNA_SCHEMA {
        return Err(fail(
            "ANTENNA_REPORT",
            format!("schema {} unsupported", report.schema),
        ));
    }
    if !matches!(report.status.as_str(), "passed" | "violated" | "partial") {
        return Err(fail(
            "ANTENNA_REPORT",
            format!("unknown report status {:?}", report.status),
        ));
    }
    let margin = report.safety_margin.fraction;
    if !(margin.is_finite() && (0.0..=0.5).contains(&margin)) {
        return Err(fail(
            "ANTENNA_REPORT",
            format!("safety margin {margin} outside 0 ..= 0.5"),
        ));
    }
    if report.status == "passed" && report.checks.iter().any(|c| c.violated) {
        return Err(fail(
            "ANTENNA_REPORT",
            "report status passed but it lists violated rows",
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
                // Sized against the conservative (effective) limit.
                match row.rules.limit(d) {
                    Some((limit, _)) => {
                        row.metal_um2 / row.rules.denominator(g, d) / (limit * (1.0 - margin))
                    }
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
    let mut diagnostics = Vec::new();
    if report.status == "partial" {
        diagnostics.push(
            "ANTENNA_INPUT_PARTIAL: the check-antenna report is partial (unmodeled LEF antenna \
             statements); this plan cannot certify the layout antenna-clean"
                .to_string(),
        );
    }
    let status = if !infeasible.is_empty() {
        "infeasible"
    } else if !nets.is_empty() {
        "planned"
    } else if report.status == "partial" {
        "partial"
    } else {
        "clean"
    };
    let mut plan = AntennaPlan {
        schema: ANTENNA_PLAN_SCHEMA,
        status,
        input_status: report.status.clone(),
        safety_margin: margin,
        diagnostics,
        tool: format!("rustroad {}", env!("CARGO_PKG_VERSION")),
        netlist: netlist_pin,
        report: artifact_of(&report_pin),
        profile: profile_pin,
        cell: diode.name.clone(),
        cell_gate_area_um2: diode.lef_gate_area_um2,
        cell_diff_area_um2: diode.lef_diff_area_um2,
        model: "each diode adds its LEF gate and diffusion area to the violating partial net; \
                diodes are counted against (1 - safety margin) x limit; re-layout, check-antenna \
                and the foundry deck verify the result",
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
