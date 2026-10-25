//! Binding of RustSTA reports to the netlist they timed (hold-eco).
//!
//! RustSTA reports name their workspace and revision but carry no hash of the
//! netlist they timed. That record lives in the timing engine's own revision
//! metadata: `<workspace>/revisions/rev<NNNN>/meta.json` lists the source file
//! of every table with its path and xxh3-64 hash (`sources[table=netlist]`).
//! hold-eco binds the reports in four independent ways and refuses any
//! mismatch with `STALE_REPORT`:
//! 1. provenance: the revision every report names records, as its netlist
//!    source, a file whose xxh3-64 equals that of `--timed-netlist` (read
//!    under a shared lock on the STA workspace; an unreadable or ambiguous
//!    record is `REPORT_PROVENANCE`, a held writer lock `STA_WORKSPACE_LOCKED`);
//! 2. identity: the timed netlist's SHA-256 equals the hash the caller
//!    recorded when it handed that file to the timing engine (optional);
//! 3. content: every hold_fix row (endpoint instance, master, endpoint net,
//!    net driver and every path stage pin/net) and every endpoints row must
//!    exist in the timed netlist, and all reports must come from one
//!    workspace revision;
//! 4. refinement: the netlist being edited must equal the timed netlist up
//!    to inserted buffer trees (clock tree) and tie cells; an instance with
//!    the ECO prefix that only the timed netlist has means the reports belong
//!    to a later netlist.
//!
//! Memory/work: one union-find over net keys (one entry per wire, port bit
//! and connection) per netlist, one pin index, per edited connection a
//! backward walk of at most `MAX_BUFFER_WALK` inserted buffers, and one
//! revision record of at most `MAX_REVISION_RECORD_BYTES`. Nothing is
//! enumerated beyond the report rows.
use super::fail;
use crate::Result;
use rustroad_formats::eco::PinDirections;
use rustroad_formats::verilog::{Expr, Netlist};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Inserted-buffer chain depth followed back from a redirected pin.
pub const MAX_BUFFER_WALK: usize = 64;
/// Size cap of one RustSTA revision record (`meta.json`).
pub const MAX_REVISION_RECORD_BYTES: u64 = 4 * 1024 * 1024;
/// Shared-lock attempts on the STA workspace, `LOCK_RETRY` apart (~2 s).
pub const LOCK_ATTEMPTS: u32 = 40;
const LOCK_RETRY: Duration = Duration::from_millis(50);
/// Schema of a RustSTA revision record.
pub const STA_REVISION_SCHEMA: &str = "rusteda.revision.v1";

pub fn expr_key(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.clone(),
        Expr::BitSelect { name, index } => format!("{name}[{index}]"),
        other => format!("{other:?}"),
    }
}

/// Report names may carry Verilog escapes (`\q[0] `); netlist keys do not.
pub fn norm(name: &str) -> String {
    name.trim().trim_start_matches('\\').trim().to_string()
}

/// Net classes (assign aliases merged), pin -> net and instance -> master.
pub struct NetIndex {
    parent: HashMap<String, String>,
    pub pin_net: HashMap<String, String>,
    pub ports: BTreeSet<String>,
    pub master: HashMap<String, String>,
}

impl NetIndex {
    pub fn new(n: &Netlist) -> Self {
        let mut x = NetIndex {
            parent: HashMap::new(),
            pin_net: HashMap::new(),
            ports: BTreeSet::new(),
            master: HashMap::new(),
        };
        for p in &n.ports {
            match p.range {
                None => {
                    x.ports.insert(p.name.clone());
                }
                Some(r) => {
                    for i in r.msb.min(r.lsb)..=r.msb.max(r.lsb) {
                        x.ports.insert(format!("{}[{i}]", p.name));
                    }
                }
            }
        }
        for a in &n.assigns {
            let (l, r) = (expr_key(&a.lhs), expr_key(&a.rhs));
            x.union(&l, &r);
        }
        for i in &n.instances {
            x.master.insert(i.name.clone(), i.cell.clone());
            for c in &i.connections {
                x.pin_net
                    .insert(format!("{}/{}", i.name, c.pin), expr_key(&c.signal));
            }
        }
        x
    }
    pub fn root(&self, key: &str) -> String {
        let mut k = key.to_string();
        // Bounded by the number of union operations (one per assign).
        for _ in 0..=self.parent.len() {
            match self.parent.get(&k) {
                Some(p) if *p != k => k = p.clone(),
                _ => break,
            }
        }
        k
    }
    fn union(&mut self, a: &str, b: &str) {
        let (ra, rb) = (self.root(a), self.root(b));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent.insert(hi, lo.clone());
            self.parent.entry(lo.clone()).or_insert(lo);
        }
    }
    pub fn same(&self, a: &str, b: &str) -> bool {
        a == b || self.root(a) == self.root(b)
    }
    /// The net key a report pin (`inst/pin` or a port bit) sits on.
    fn pin_key(&self, pin: &str) -> Option<String> {
        if pin.contains('/') {
            self.pin_net.get(pin).cloned()
        } else {
            self.ports.contains(pin).then(|| pin.to_string())
        }
    }
}

/// What the timing engine's own revision record says it timed.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StaRecord {
    /// `<workspace>/revisions/rev<NNNN>/meta.json` as read.
    pub revision_record: String,
    pub revision_record_sha256: String,
    /// Netlist source path and xxh3-64 recorded by the timing engine.
    pub netlist_source: String,
    pub netlist_source_xxh3: String,
    /// xxh3-64 of the `--timed-netlist` bytes hold-eco parsed.
    pub timed_netlist_xxh3: String,
    /// Informational: both paths name the same file (the hash is the binding).
    pub source_path_matches: bool,
    pub lock: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BindingSummary {
    pub method: &'static str,
    pub sta_workspace: String,
    pub sta_revision: String,
    pub sta_record: Option<StaRecord>,
    pub timed_netlist_sha256: String,
    pub expected_sha256: Option<String>,
    pub hold_rows_bound: usize,
    pub path_stages_bound: usize,
    pub endpoint_rows_bound: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pin_rows_bound: usize,
    pub inserted_instances: Vec<String>,
    pub redirected_pins: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn stale(m: impl std::fmt::Display) -> crate::Error {
    fail("STALE_REPORT", m)
}

fn provenance(m: impl std::fmt::Display) -> crate::Error {
    fail("REPORT_PROVENANCE", m)
}

/// `xxh3:<16 lowercase hex digits>` of `bytes` (the timing engine's notation).
pub fn xxh3_text(bytes: &[u8]) -> String {
    format!("xxh3:{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// `rev<N>` -> the record directory name `rev<N zero-padded to 4 digits>`.
fn revision_dir(rev: &str) -> Option<String> {
    let digits = rev.strip_prefix("rev")?;
    if digits.is_empty() || digits.len() > 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (format!("rev{n}") == rev).then(|| format!("rev{n:04}"))
}

/// Shared `flock` on `<workspace>/workspace.lock` (the timing engine's lock
/// protocol; bounded wait; the file is never created).
fn lock_shared(workspace: &Path) -> Result<File> {
    let path = workspace.join("workspace.lock");
    let file = File::open(&path).map_err(|e| {
        provenance(format!(
            "cannot open the STA workspace lock {}: {e}",
            path.display()
        ))
    })?;
    for attempt in 0..LOCK_ATTEMPTS {
        match file.try_lock_shared() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if attempt + 1 < LOCK_ATTEMPTS {
                    std::thread::sleep(LOCK_RETRY);
                }
            }
            Err(TryLockError::Error(e)) => {
                return Err(provenance(format!("cannot lock {}: {e}", path.display())));
            }
        }
    }
    Err(fail(
        "STA_WORKSPACE_LOCKED",
        format!(
            "{} is held exclusively by a writer; retry when the timing run has finished",
            path.display()
        ),
    ))
}

fn read_record(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| {
        provenance(format!(
            "cannot read the STA revision record {}: {e}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_REVISION_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| provenance(format!("cannot read {}: {e}", path.display())))?;
    if bytes.len() as u64 > MAX_REVISION_RECORD_BYTES {
        return Err(provenance(format!(
            "{} exceeds {MAX_REVISION_RECORD_BYTES} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Provenance binding: the STA revision the reports name must record, as its
/// netlist source, a file with the xxh3-64 of `timed_bytes` (the exact bytes
/// hold-eco parsed as `--timed-netlist`).
pub fn bind_sta_record(
    workspace: &str,
    rev: &str,
    timed_path: &Path,
    timed_bytes: &[u8],
    b: &mut BindingSummary,
) -> Result<()> {
    let dir = revision_dir(rev)
        .ok_or_else(|| provenance(format!("report revision {rev:?} is not rev<N>")))?;
    // A relative workspace is resolved like the timing engine resolved it
    // (from the current directory); any wrong resolution fails the hash check.
    let ws = PathBuf::from(workspace);
    let _lock = lock_shared(&ws)?;
    let record = ws.join("revisions").join(&dir).join("meta.json");
    let bytes = read_record(&record)?;
    let meta: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| provenance(format!("{}: {e}", record.display())))?;
    if s(&meta, "schema") != Some(STA_REVISION_SCHEMA) || s(&meta, "rev") != Some(rev) {
        return Err(provenance(format!(
            "{} is not the {STA_REVISION_SCHEMA} record of {rev}",
            record.display()
        )));
    }
    if meta.get("tables").and_then(|t| t.get("netlist")).is_none() {
        return Err(provenance(format!(
            "{rev} of {workspace} holds no netlist table",
            workspace = ws.display()
        )));
    }
    let sources: Vec<&serde_json::Value> = meta
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|x| s(x, "table") == Some("netlist"))
        .collect();
    let [source] = sources.as_slice() else {
        return Err(provenance(format!(
            "{} records {} netlist sources; exactly one is required",
            record.display(),
            sources.len()
        )));
    };
    let (Some(src), Some(recorded)) = (s(source, "path"), s(source, "hash")) else {
        return Err(provenance(format!(
            "{}: the netlist source lacks path/hash",
            record.display()
        )));
    };
    let actual = xxh3_text(timed_bytes);
    if recorded != actual {
        return Err(stale(format!(
            "the reports come from {} {rev}, which timed {src} ({recorded}); --timed-netlist {} \
             is {actual}: the reports were not produced from this netlist",
            ws.display(),
            timed_path.display()
        )));
    }
    let same_path = match (
        std::fs::canonicalize(src),
        std::fs::canonicalize(timed_path),
    ) {
        (Ok(a), Ok(c)) => a == c,
        _ => false,
    };
    b.sta_record = Some(StaRecord {
        revision_record: record.display().to_string(),
        revision_record_sha256: format!("{:x}", Sha256::digest(&bytes)),
        netlist_source: src.to_string(),
        netlist_source_xxh3: recorded.to_string(),
        timed_netlist_xxh3: actual,
        source_path_matches: same_path,
        lock: "shared flock on workspace.lock",
    });
    Ok(())
}

fn s<'a>(v: &'a serde_json::Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(serde_json::Value::as_str)
}

/// One hold_fix row and its worst-path stages against the timed netlist.
pub fn bind_hold_row(t: &NetIndex, row: &serde_json::Value, b: &mut BindingSummary) -> Result<()> {
    let endpoint = norm(s(row, "endpoint").unwrap_or_default());
    if let Some((inst, _)) = endpoint.rsplit_once('/') {
        let (Some(cell), Some(net)) = (s(row, "cell"), s(row, "net")) else {
            return Err(fail(
                "TIMING_REPORT",
                format!("hold_fix row {endpoint} lacks cell/net binding fields"),
            ));
        };
        match t.master.get(inst) {
            Some(m) if m == cell => {}
            Some(m) => {
                return Err(stale(format!(
                    "{endpoint}: report master {cell}, timed netlist master {m}"
                )));
            }
            None => {
                return Err(stale(format!(
                    "{endpoint}: instance absent from the timed netlist"
                )));
            }
        }
        let key = t.pin_net.get(&endpoint).ok_or_else(|| {
            stale(format!(
                "endpoint {endpoint}: pin not connected in the timed netlist"
            ))
        })?;
        if !t.same(key, &norm(net)) {
            return Err(stale(format!(
                "endpoint {endpoint}: report net {net}, timed netlist net {key}"
            )));
        }
        if let Some(d) = s(row, "net_driver") {
            let d = norm(d);
            match t.pin_key(&d) {
                Some(k) if t.same(&k, key) => {}
                _ => {
                    return Err(stale(format!(
                        "endpoint {endpoint}: report driver {d} is not on net {key} of the timed netlist"
                    )));
                }
            }
        }
    } else if !t.ports.contains(&endpoint) {
        return Err(stale(format!(
            "port endpoint {endpoint} absent from the timed netlist"
        )));
    }
    let stages = row
        .get("path")
        .and_then(|p| p.get("stages"))
        .and_then(serde_json::Value::as_array);
    for stage in stages.into_iter().flatten() {
        let (Some(pin), Some(net)) = (s(stage, "pin"), s(stage, "net")) else {
            return Err(fail(
                "TIMING_REPORT",
                format!("{endpoint}: path stage lacks pin/net"),
            ));
        };
        let pin = norm(pin);
        match t.pin_key(&pin) {
            Some(k) if t.same(&k, &norm(net)) => b.path_stages_bound += 1,
            _ => {
                return Err(stale(format!(
                    "{endpoint}: path stage {pin} on net {net} does not exist in the timed netlist"
                )));
            }
        }
    }
    b.hold_rows_bound += 1;
    Ok(())
}

/// An endpoints-report row: its pin or port must exist in the timed netlist.
pub fn bind_endpoint(t: &NetIndex, endpoint: &str, b: &mut BindingSummary) -> Result<()> {
    if t.pin_key(&norm(endpoint)).is_none() {
        return Err(stale(format!(
            "endpoint {endpoint} of the endpoints report is absent from the timed netlist"
        )));
    }
    b.endpoint_rows_bound += 1;
    Ok(())
}

/// A pins-report row: its pin or port must exist in the timed netlist.
pub fn bind_pin(t: &NetIndex, pin: &str, b: &mut BindingSummary) -> Result<()> {
    if t.pin_key(&norm(pin)).is_none() {
        return Err(stale(format!(
            "pin {pin} of the pins report is absent from the timed netlist"
        )));
    }
    b.pin_rows_bound += 1;
    Ok(())
}

fn signal_pins(dirs: &PinDirections, master: &str) -> Option<(Vec<String>, Vec<String>)> {
    let pins = dirs.get(master)?;
    let inputs = pins
        .iter()
        .filter(|(_, d)| d.as_str() == "INPUT")
        .map(|(p, _)| p.clone())
        .collect();
    let outputs = pins
        .iter()
        .filter(|(_, d)| d.starts_with("OUTPUT"))
        .map(|(p, _)| p.clone())
        .collect();
    Some((inputs, outputs))
}

/// `edited` must equal `timed` up to inserted buffer trees and tie cells.
pub fn check_refinement(
    edited: &Netlist,
    timed: &Netlist,
    t: &NetIndex,
    dirs: &PinDirections,
    eco_prefix: &str,
    b: &mut BindingSummary,
) -> Result<()> {
    let edited_names: BTreeSet<&str> = edited.instances.iter().map(|i| i.name.as_str()).collect();
    // Inserted instances: buffers (one input, one output) or ties (no input).
    let mut buffer_input: HashMap<&str, String> = HashMap::new();
    let mut ties: BTreeSet<&str> = BTreeSet::new();
    for i in timed
        .instances
        .iter()
        .filter(|i| !edited_names.contains(i.name.as_str()))
    {
        if i.name.starts_with(eco_prefix) {
            return Err(stale(format!(
                "the timed netlist contains ECO cell {} that the edited netlist lacks; the reports belong to a later netlist",
                i.name
            )));
        }
        let Some((inputs, outputs)) = signal_pins(dirs, &i.cell) else {
            return Err(stale(format!(
                "inserted instance {} ({}) has no cell-LEF pin directions",
                i.name, i.cell
            )));
        };
        let connected = |p: &String| i.connections.iter().find(|c| &c.pin == p);
        match (inputs.as_slice(), outputs.len()) {
            ([input], 1) => {
                let c = connected(input).ok_or_else(|| {
                    stale(format!(
                        "inserted buffer {} has no input connection",
                        i.name
                    ))
                })?;
                buffer_input.insert(i.name.as_str(), expr_key(&c.signal));
            }
            ([], n) if n > 0 => {
                ties.insert(i.name.as_str());
            }
            _ => {
                return Err(stale(format!(
                    "instance {} ({}) exists only in the timed netlist and is neither an inserted buffer nor a tie cell",
                    i.name, i.cell
                )));
            }
        }
        b.inserted_instances.push(i.name.clone());
    }
    // Driver of every timed net class (output pins by cell-LEF direction).
    let mut driver: HashMap<String, &str> = HashMap::new();
    for i in &timed.instances {
        let Some((_, outputs)) = signal_pins(dirs, &i.cell) else {
            continue;
        };
        for c in i.connections.iter().filter(|c| outputs.contains(&c.pin)) {
            driver.insert(t.root(&expr_key(&c.signal)), i.name.as_str());
        }
    }
    let timed_by_name: HashMap<&str, &rustroad_formats::verilog::Instance> = timed
        .instances
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();
    for i in &edited.instances {
        let ti = timed_by_name.get(i.name.as_str()).ok_or_else(|| {
            stale(format!(
                "instance {} of the edited netlist is absent from the timed netlist",
                i.name
            ))
        })?;
        if ti.cell != i.cell {
            return Err(stale(format!(
                "instance {}: master {} in the edited netlist, {} in the timed netlist",
                i.name, i.cell, ti.cell
            )));
        }
        for c in &i.connections {
            let tc = ti
                .connections
                .iter()
                .find(|x| x.pin == c.pin)
                .ok_or_else(|| {
                    stale(format!(
                        "{}/{} is not connected in the timed netlist",
                        i.name, c.pin
                    ))
                })?;
            let want = expr_key(&c.signal);
            let mut have = expr_key(&tc.signal);
            if t.same(&want, &have) {
                continue;
            }
            let mut ok = false;
            for _ in 0..MAX_BUFFER_WALK {
                let Some(d) = driver.get(&t.root(&have)) else {
                    break;
                };
                if ties.contains(d) && matches!(c.signal, Expr::Constant(_)) {
                    ok = true;
                    break;
                }
                let Some(input) = buffer_input.get(d) else {
                    break;
                };
                have = input.clone();
                if t.same(&want, &have) {
                    ok = true;
                    break;
                }
            }
            if !ok {
                return Err(stale(format!(
                    "{}/{} connects {} in the edited netlist but {} in the timed netlist, not through inserted buffers",
                    i.name,
                    c.pin,
                    want,
                    expr_key(&tc.signal)
                )));
            }
            b.redirected_pins += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xxh3_matches_the_reference_vector() {
        // XXH3-64 of the empty input, seed 0 (xxHash specification test vector).
        assert_eq!(xxh3_text(b""), "xxh3:2d06800538d394c2");
    }

    #[test]
    fn revision_directories_are_canonical() {
        assert_eq!(revision_dir("rev2").as_deref(), Some("rev0002"));
        assert_eq!(revision_dir("rev12345").as_deref(), Some("rev12345"));
        for bad in ["rev", "rev02", "r2", "rev-1", "rev2 ", "rev99999999999"] {
            assert_eq!(revision_dir(bad), None, "{bad}");
        }
    }
}
