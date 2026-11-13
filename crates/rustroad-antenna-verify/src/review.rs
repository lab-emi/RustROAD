//! The review itself. Names follow the reference verifier so the two can be
//! read side by side; every `need` carries the reference's rejection code.
use rustroad_pyjson::{
    Dict, Duplicates, FileId, Layout, ParseError, Value, dumps, is_sha256, list, parse_prefix,
    parse_with, sha256_hex,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Why a lineage was not verified: a rejection code of a failed check, or a
/// description of a malformed record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected(pub String);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Rejected {}
impl From<std::io::Error> for Rejected {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

type R<T> = Result<T, Rejected>;

const SCHEMA_V1: &str = "rusteda.antenna-v3-independent-verifier-inputs.v1";
const SCHEMA_V2: &str = "rusteda.antenna-v3-independent-verifier-inputs.v2";
const MAX_WORK: i128 = 200_000_000;
const MAX_REPORT_BYTES: i128 = 8 << 30;
const MAX_CUMULATIVE_WORK: i128 = 200_000_000_000;
const ADDRESS_SPACE: u64 = 2 << 30;
const HEADER: [&str; 13] = [
    "schema",
    "status",
    "workspace_revision",
    "scope",
    "safety_margin",
    "nets",
    "violations",
    "lef_violations",
    "sources",
    "unsupported",
    "design",
    "selection",
    "stats",
];
const REPAIR_FIELDS: [&str; 16] = [
    "schema",
    "status",
    "base_revision",
    "revision",
    "routing_scope",
    "repair_scope",
    "selected_nets",
    "requires_full_revalidation",
    "drc_status",
    "antenna_status",
    "safety_margin",
    "jumpers",
    "stats",
    "before",
    "after",
    "selected_analysis",
];

fn policy() -> Value {
    Value::Dict(
        Dict::new()
            .with("group_names", 128)
            .with("max_groups", 1024)
            .with("max_attempts", 64)
            .with("max_total_jumpers", 4096)
            .with("max_commands", 4096)
            .with("max_report_bytes", MAX_REPORT_BYTES)
            .with("max_selected_report_bytes", 128 << 20)
            .with("max_metadata_bytes", 4 << 20)
            .with("max_cumulative_work", MAX_CUMULATIVE_WORK),
    )
}

fn limits() -> Value {
    Value::Dict(
        Dict::new()
            .with("max_shapes_per_net", 200_000)
            .with("max_total_shapes", 2_000_000)
            .with("max_pair_checks", 200_000_000)
            .with("max_union_cells", 4_000_000)
            .with("max_rows", 100_000)
            .with("max_work", MAX_WORK)
            .with("report_memory_bytes", 128 << 20),
    )
}

fn need(ok: bool, code: &str) -> R<()> {
    if ok {
        Ok(())
    } else {
        Err(Rejected(code.to_owned()))
    }
}

/// `value[key]` on an object.
fn at<'a>(value: &'a Value, key: &str) -> R<&'a Value> {
    match value {
        Value::Dict(dict) => dict.get(key).ok_or_else(|| Rejected(format!("'{key}'"))),
        _ => Err(Rejected(format!("TypeError: not an object at '{key}'"))),
    }
}

fn text(value: &Value) -> R<&str> {
    value
        .as_str()
        .ok_or_else(|| Rejected("TypeError: string required".into()))
}

fn items(value: &Value) -> R<&[Value]> {
    value
        .as_list()
        .ok_or_else(|| Rejected("TypeError: list required".into()))
}

fn object(value: &Value) -> R<&Dict> {
    value
        .as_dict()
        .ok_or_else(|| Rejected("TypeError: object required".into()))
}

/// `type(n) is int and n >= 0`.
fn integer(value: &Value) -> Option<i128> {
    value.as_int().filter(|n| *n >= 0)
}

fn int(value: &Value) -> R<i128> {
    value
        .as_int()
        .ok_or_else(|| Rejected("TypeError: integer required".into()))
}

fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(is_sha256)
}

fn fingerprint(value: &Value) -> R<String> {
    dumps(value, Layout::Compact)
        .map(|text| sha256_hex(text.as_bytes()))
        .ok_or_else(|| Rejected("NONFINITE_JSON_NUMBER".into()))
}

fn stamp(path: &Path) -> R<(u64, u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path)?;
    Ok((m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

fn identity(path: &str, cap: u64) -> R<Value> {
    let resolved = std::fs::canonicalize(path)?;
    let before = stamp(&resolved)?;
    need(
        std::fs::metadata(&resolved)?.is_file() && before.2 <= cap,
        "ARTIFACT_BYTE_BUDGET",
    )?;
    let id = FileId::of(&resolved, u64::MAX).map_err(Rejected)?;
    need(stamp(&resolved)? == before, "ARTIFACT_CHANGED")?;
    Ok(id.to_value())
}

fn decode_error(error: &ParseError, text: &str) -> Rejected {
    Rejected(match error {
        ParseError::DuplicateKey => "DUPLICATE_JSON_KEY".into(),
        ParseError::Depth => "JSON_DEPTH_BUDGET".into(),
        ParseError::Syntax(at)
            if ["NaN", "Infinity", "-Infinity"]
                .iter()
                .any(|word| text[*at..].starts_with(word)) =>
        {
            "NONFINITE_JSON_NUMBER".into()
        }
        other => format!("MALFORMED_JSON: {other}"),
    })
}

fn read_text(path: &Path, cap: u64, code: &str) -> R<String> {
    need(std::fs::metadata(path)?.len() <= cap, code)?;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(cap + 1)
        .read_to_end(&mut data)?;
    need(data.len() as u64 <= cap, code)?;
    String::from_utf8(data).map_err(|_| Rejected("invalid UTF-8 in a JSON record".into()))
}

fn read(path: &Path, cap: u64) -> R<Value> {
    let text = read_text(path, cap, "JSON_BYTE_BUDGET")?;
    parse_with(&text, Duplicates::Reject).map_err(|error| decode_error(&error, &text))
}

/// A canonical name list: sorted, distinct, bounded.
fn names(value: &Value, cap: usize) -> R<Vec<String>> {
    let list = value.as_list().filter(|list| list.len() <= cap);
    let list = list.ok_or_else(|| Rejected("NAME_COUNT_BUDGET".into()))?;
    let mut out = Vec::with_capacity(list.len());
    let mut total = 0usize;
    for item in list {
        let name = item
            .as_str()
            .filter(|name| !name.is_empty() && name.len() <= 4096)
            .ok_or_else(|| Rejected("NAME_BYTE_BUDGET".into()))?;
        total += name.len();
        out.push(name.to_owned());
    }
    need(total <= 16 << 20, "TOTAL_NAME_BYTE_BUDGET")?;
    need(
        out.windows(2).all(|pair| pair[0] < pair[1]),
        "NAMES_NOT_CANONICAL",
    )?;
    Ok(out)
}

/// Strict incremental top-level reader, handing the `checks` array to the
/// caller one row at a time; every other field is decoded under a byte cap.
struct Reader {
    field_cap: usize,
    row_cap: usize,
    max_rows: usize,
    metadata_cap: usize,
}

impl Reader {
    fn new(metadata_cap: Option<usize>) -> Self {
        Self {
            field_cap: 4 << 20,
            row_cap: 1 << 20,
            max_rows: 100_000,
            metadata_cap: metadata_cap.unwrap_or(4 << 20),
        }
    }

    fn scan(
        &self,
        path: &Path,
        required: &[&str],
        on_check: &mut dyn FnMut(Value) -> R<()>,
    ) -> R<(Dict, usize)> {
        let text = read_text(path, 128 << 20, "RAW_REPORT_BYTE_BUDGET")?;
        let bytes = text.as_bytes();
        let space = |mut at: usize| {
            while matches!(bytes.get(at), Some(b' ' | b'\r' | b'\n' | b'\t')) {
                at += 1;
            }
            at
        };
        // `char()`: the next non-space character, or TRUNCATED_JSON.
        let take = |at: usize| -> R<(u8, usize)> {
            let at = space(at);
            bytes
                .get(at)
                .map(|byte| (*byte, at + 1))
                .ok_or_else(|| Rejected("TRUNCATED_JSON".into()))
        };
        let value = |at: usize, cap: usize| -> R<(Value, usize)> {
            let start = space(at);
            let (value, end) =
                parse_prefix(&text, start, Duplicates::Reject).map_err(|error| match error {
                    ParseError::Truncated => Rejected("MALFORMED_OR_TRUNCATED_JSON".into()),
                    ParseError::Syntax(at)
                        if !["NaN", "Infinity", "-Infinity"]
                            .iter()
                            .any(|word| text[at..].starts_with(word)) =>
                    {
                        Rejected("MALFORMED_OR_TRUNCATED_JSON".into())
                    }
                    other => decode_error(&other, &text),
                })?;
            need(end - start <= cap, "JSON_VALUE_BYTE_BUDGET")?;
            Ok((value, end))
        };
        let (open, mut at) = take(0)?;
        need(open == b'{', "REPORT_OBJECT_REQUIRED")?;
        let mut metadata = Dict::new();
        let mut keys: HashSet<String> = HashSet::new();
        let (mut first, mut count, mut selected_bytes) = (true, 0usize, 0usize);
        while bytes.get(space(at)) != Some(&b'}') {
            if !first {
                let (comma, next) = take(at)?;
                need(comma == b',', "TOP_SEPARATOR")?;
                at = next;
            }
            first = false;
            let (key, next) = value(at, 8192)?;
            at = next;
            let key = key
                .as_str()
                .filter(|key| !keys.contains(*key) && keys.len() < 128)
                .ok_or_else(|| Rejected("TOP_KEY_BUDGET_OR_DUPLICATE".into()))?
                .to_owned();
            keys.insert(key.clone());
            let (colon, next) = take(at)?;
            need(colon == b':', "KEY_SEPARATOR")?;
            at = next;
            if key == "checks" {
                let (open, next) = take(at)?;
                need(open == b'[', "CHECKS_ARRAY_REQUIRED")?;
                at = next;
                let mut item_first = true;
                while bytes.get(space(at)) != Some(&b']') {
                    if !item_first {
                        let (comma, next) = take(at)?;
                        need(comma == b',', "CHECK_SEPARATOR")?;
                        at = next;
                    }
                    item_first = false;
                    count += 1;
                    need(count <= self.max_rows, "CHECK_ROW_BUDGET")?;
                    let (row, next) = value(at, self.row_cap)?;
                    at = next;
                    on_check(row)?;
                }
                let (close, next) = take(at)?;
                need(close == b']', "CHECK_ARRAY_END")?;
                at = next;
            } else {
                let (field, next) = value(at, self.field_cap)?;
                at = next;
                selected_bytes += dumps(&field, Layout::LineUtf8)
                    .ok_or_else(|| Rejected("NONFINITE_JSON_NUMBER".into()))?
                    .len();
                need(
                    selected_bytes <= self.metadata_cap,
                    "TOTAL_METADATA_BYTE_BUDGET",
                )?;
                metadata.insert(key, field);
            }
        }
        let (close, next) = take(at)?;
        need(
            close == b'}' && space(next) == bytes.len(),
            "TRAILING_JSON_DATA",
        )?;
        need(
            required.iter().all(|name| keys.contains(*name)),
            "REPORT_FIELDS_MISSING",
        )?;
        Ok((metadata, count))
    }
}

struct Inventory {
    names: Vec<String>,
    ids: Vec<i128>,
    source: String,
    sources: Value,
    counts: Value,
}

#[derive(Clone)]
struct Coverage {
    rows: i128,
    model_partial: bool,
    observation: String,
}

struct Summary {
    names: Vec<String>,
    /// Coverage rows in selection order.
    coverage: Vec<(String, Coverage)>,
    partial: bool,
    violating: BTreeSet<String>,
    source: String,
    header: Value,
}
impl Summary {
    fn covered(&self, name: &str) -> R<&Coverage> {
        self.coverage
            .binary_search_by(|(known, _)| known.as_str().cmp(name))
            .map(|at| &self.coverage[at].1)
            .map_err(|_| Rejected(format!("'{name}'")))
    }
}

struct RepairCost {
    antenna: i128,
    planner: i128,
    base: String,
    child: String,
}

struct Verifier<'a> {
    cfg: &'a Dict,
    start: Instant,
    /// Checked file identities by path, in first-use order.
    records: Vec<Value>,
    record_index: HashMap<String, usize>,
    report_summaries: HashMap<String, Value>,
    repair_costs: HashMap<String, RepairCost>,
    reported_work: i128,
    validation_upper_bound: i128,
    work_budget_charge: i128,
    workspace: PathBuf,
    operation_map: HashMap<String, Value>,
    logical: bool,
    violated_only: bool,
    route_scope: String,
}

impl<'a> Verifier<'a> {
    fn new(cfg: &'a Dict) -> R<Self> {
        let schema = cfg.get("schema").and_then(Value::as_str);
        let common = [
            "schema",
            "lineage",
            "tool",
            "design_workspace",
            "evidence_kind",
            "controller_inputs",
        ];
        let mut with_manifest = common.to_vec();
        with_manifest.push("source_manifest");
        need(
            (schema == Some(SCHEMA_V1) && cfg.has_exactly(&with_manifest))
                || (schema == Some(SCHEMA_V2) && cfg.has_exactly(&common)),
            "INPUT_SCHEMA",
        )?;
        need(
            matches!(
                cfg.get("evidence_kind").and_then(Value::as_str),
                Some("protocol_fixture" | "actual_native_artifacts")
            ),
            "EVIDENCE_SCOPE",
        )?;
        let workspace = std::fs::canonicalize(text(
            cfg.get("design_workspace")
                .ok_or_else(|| Rejected("'design_workspace'".into()))?,
        )?)?;
        Ok(Self {
            cfg,
            start: Instant::now(),
            records: Vec::new(),
            record_index: HashMap::new(),
            report_summaries: HashMap::new(),
            repair_costs: HashMap::new(),
            reported_work: 0,
            validation_upper_bound: 0,
            work_budget_charge: 0,
            workspace,
            operation_map: HashMap::new(),
            logical: false,
            violated_only: false,
            route_scope: String::new(),
        })
    }

    fn cfg(&self, key: &str) -> R<&'a Value> {
        self.cfg
            .get(key)
            .ok_or_else(|| Rejected(format!("'{key}'")))
    }

    fn check_time(&self) -> R<()> {
        need(
            self.start.elapsed().as_secs_f64() <= 170.0,
            "VERIFIER_DEADLINE",
        )
    }

    /// Register a file identity: schema, byte cap, and the bytes on disk.
    fn file(&mut self, record: &Value, cap: u64) -> R<PathBuf> {
        self.check_time()?;
        need(
            record
                .as_dict()
                .is_some_and(|dict| dict.has_exactly(&["path", "sha256", "bytes"])),
            "FILE_ID_SCHEMA",
        )?;
        let path = text(at(record, "path")?)?;
        let bytes = integer(at(record, "bytes")?);
        need(
            Path::new(path).is_absolute() && digest(at(record, "sha256")?) && bytes.is_some(),
            "FILE_ID_SCHEMA",
        )?;
        need(
            bytes.is_some_and(|bytes| bytes <= i128::from(cap)),
            "FILE_ID_BYTE_BUDGET",
        )?;
        match self.record_index.get(path) {
            Some(index) => need(&self.records[*index] == record, "CONFLICTING_FILE_ID")?,
            None => {
                // <=4 records/command plus <=200 source/revision records; 4096 commands.
                need(self.records.len() < 20000, "FILE_COUNT_BUDGET")?;
                need(&identity(path, cap)? == record, "STALE_FILE_ID")?;
                self.record_index
                    .insert(path.to_owned(), self.records.len());
                self.records.push(record.clone());
            }
        }
        Ok(PathBuf::from(path))
    }

    fn data(&mut self, record: &Value, cap: u64) -> R<Value> {
        let path = self.file(record, cap)?;
        read(&path, cap)
    }

    /// A materialized revision: its design file and the header fields in it.
    fn revision(&mut self, row: &Value, expected: &str, parent: Option<&str>) -> R<()> {
        need(
            revision_name(expected) && at(row, "revision")?.as_str() == Some(expected),
            "REVISION_IDENTITY",
        )?;
        let path = self.file(at(row, "design_file")?, 512 << 20)?;
        need(
            path == self
                .workspace
                .join("revisions")
                .join(expected)
                .join("design.json"),
            "FOREIGN_REVISION_PATH",
        )?;
        // The native format header precedes the immutable IR; no graph is decoded.
        let mut prefix = Vec::with_capacity(65536);
        std::fs::File::open(&path)?
            .take(65536)
            .read_to_end(&mut prefix)?;
        let valid = match std::str::from_utf8(&prefix) {
            Ok(text) => text,
            Err(error) => std::str::from_utf8(&prefix[..error.valid_up_to()])
                .map_err(|_| Rejected("revision header is not UTF-8".into()))?,
        };
        let bytes = valid.as_bytes();
        let space = |mut at: usize| {
            while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                at += 1;
            }
            at
        };
        let mut i = space(0);
        need(bytes.get(i) == Some(&b'{'), "REVISION_OBJECT")?;
        i += 1;
        let mut values = Dict::new();
        let decode = |at: usize| -> R<(Value, usize)> {
            parse_prefix(valid, at, Duplicates::Reject).map_err(|error| decode_error(&error, valid))
        };
        for index in 0..128 {
            i = space(i);
            if index > 0 {
                need(bytes.get(i) == Some(&b','), "REVISION_HEADER_SEPARATOR")?;
                i = space(i + 1);
            }
            let (key, next) = decode(i)?;
            let key = text(&key)?.to_owned();
            need(!values.contains_key(&key), "REVISION_DUPLICATE_KEY")?;
            i = space(next);
            need(bytes.get(i) == Some(&b':'), "REVISION_KEY_SEPARATOR")?;
            let (value, next) = decode(space(i + 1))?;
            i = next;
            values.insert(key, value);
            if values.has_all(&["format_version", "revision", "parent_revision"]) {
                break;
            }
        }
        let header = Value::Dict(values);
        need(
            matches!(at(&header, "format_version")?.as_int(), Some(4..=6)),
            "REVISION_FORMAT",
        )?;
        need(
            at(&header, "revision")?.as_str() == Some(expected)
                && at(&header, "parent_revision")? == at(row, "parent_revision")?,
            "REVISION_HEADER_MISMATCH",
        )?;
        if let Some(parent) = parent {
            need(
                at(row, "parent_revision")?.as_str() == Some(parent),
                "BROKEN_PARENT_CHAIN",
            )?;
        }
        Ok(())
    }

    fn inventory(&mut self, record: &Value, revision: &str, scope: &str) -> R<Inventory> {
        let d = self.data(record, 64 << 20)?;
        need(
            at(&d, "schema")?.as_str() == Some("rustroad.antenna-inventory.v1")
                && at(&d, "status")?.as_str() == Some("inventory_only"),
            "INVENTORY_SCHEMA",
        )?;
        need(
            at(&d, "workspace_revision")?.as_str() == Some(revision)
                && at(&d, "routing_scope")?.as_str() == Some(scope)
                && digest(at(&d, "source_sha256")?),
            "INVENTORY_BINDING",
        )?;
        let mut counts = Dict::new();
        let mut count = |key: &str| -> R<i128> {
            let value = integer(at(&d, key)?).filter(|n| *n <= 1_000_000);
            let value = value.ok_or_else(|| Rejected("INVENTORY_COUNT_BUDGET".into()))?;
            if key != "routed_logical_net_count" {
                counts.insert(key, value);
            }
            Ok(value)
        };
        let model_nets = count("model_logical_net_count")?;
        let declared = count("router_declared_net_count")?;
        let routed = count("routed_logical_net_count")?;
        let excluded = count("excluded_nonlogical_supply_tie_count")?;
        let rows = at(&d, "nets")?
            .as_list()
            .filter(|rows| rows.len() <= 100_000);
        let rows = rows.ok_or_else(|| Rejected("INVENTORY_ROW_BUDGET".into()))?;
        let mut listed = Vec::with_capacity(rows.len());
        for row in rows {
            listed.push(at(row, "canonical_name")?.clone());
        }
        let ns = names(&Value::List(listed), 100_000)?;
        let mut ids = Vec::with_capacity(rows.len());
        let mut canonical = Vec::with_capacity(rows.len());
        for row in rows {
            let id = integer(at(row, "net_id")?).filter(|id| {
                *id < model_nets
                    && object(row).is_ok_and(|row| row.has_exactly(&["canonical_name", "net_id"]))
            });
            let id = id.ok_or_else(|| Rejected("INVENTORY_NET_ID".into()))?;
            ids.push(id);
            canonical.push(Value::Dict(
                Dict::new()
                    .with("net_id", id)
                    .with("canonical_name", at(row, "canonical_name")?),
            ));
        }
        need(
            ids.iter().collect::<HashSet<_>>().len() == ids.len(),
            "DUPLICATE_NET_ID",
        )?;
        need(
            ns.len() as i128 == routed && ns.len() as i128 + excluded == declared,
            "INVENTORY_COUNT_RECONCILIATION",
        )?;
        let source = text(at(&d, "source_sha256")?)?.to_owned();
        need(
            at(&d, "inventory_sha256")?.as_str()
                == Some(&fingerprint(&list([
                    Value::from(revision),
                    Value::from(scope),
                    Value::from(source.as_str()),
                    Value::List(canonical),
                ]))?),
            "INVENTORY_DIGEST",
        )?;
        let sources = items(at(&d, "sources")?)?;
        let mut roles = BTreeSet::new();
        for source in sources {
            roles.insert(text(at(source, "role")?)?);
        }
        need(
            sources.len() == 2 && roles.into_iter().eq(["cell_lef", "technology_lef"]),
            "INVENTORY_LEF_ROLES",
        )?;
        for source in sources {
            let path = at(source, "path")?.as_str();
            need(
                digest(at(source, "sha256")?)
                    && path.is_some_and(|path| Path::new(path).is_absolute()),
                "INVENTORY_LEF_BINDING",
            )?;
            need(
                at(&identity(path.unwrap_or_default(), 4 << 30)?, "sha256")?
                    == at(source, "sha256")?,
                "STALE_ORIGINAL_LEF",
            )?;
        }
        Ok(Inventory {
            names: ns,
            ids,
            source,
            sources: at(&d, "sources")?.clone(),
            counts: Value::Dict(counts),
        })
    }

    /// The connectivity model: every net name, and the nets a BLOCK touches.
    fn model(&mut self, record: &Value) -> R<(HashSet<String>, HashSet<String>)> {
        let d = self.data(record, 64 << 20)?;
        need(
            object(&d)?.has_exactly(&[
                "layers",
                "masters",
                "instance_masters",
                "ports",
                "nets",
                "pins",
            ]),
            "MODEL_SCHEMA",
        )?;
        let (ms, im, ns, ps) = (
            items(at(&d, "masters")?)?,
            items(at(&d, "instance_masters")?)?,
            items(at(&d, "nets")?)?,
            items(at(&d, "pins")?)?,
        );
        need(
            ms.len() <= 4096 && im.len() <= 100_000 && ns.len() <= 100_000 && ps.len() <= 1_000_000,
            "MODEL_OBJECT_BUDGET",
        )?;
        let mut net_names = Vec::with_capacity(ns.len());
        for net in ns {
            net_names.push(at(net, "name")?.clone());
        }
        let mut sorted: Vec<&str> = Vec::with_capacity(net_names.len());
        for name in &net_names {
            sorted.push(
                name.as_str()
                    .ok_or_else(|| Rejected("NAME_BYTE_BUDGET".into()))?,
            );
        }
        sorted.sort_unstable();
        // `names(sorted(...))`: bounded names; a repeated name is not canonical.
        let unique = sorted.windows(2).all(|pair| pair[0] != pair[1]);
        names(
            &Value::List({
                let mut distinct = sorted.clone();
                distinct.dedup();
                distinct.into_iter().map(Value::from).collect()
            }),
            100_000,
        )?;
        need(unique, "NAMES_NOT_CANONICAL")?;
        let mut blocks = HashSet::new();
        for (n, m) in im.iter().enumerate() {
            let master = integer(m).filter(|m| (*m as usize) < ms.len());
            let master = master.ok_or_else(|| Rejected("MODEL_MASTER_INDEX".into()))?;
            if at(&ms[master as usize], "class")?.as_str() == Some("BLOCK") {
                blocks.insert(n as i128);
            }
        }
        let mut touches = HashSet::new();
        for pin in ps {
            let (net, owner) = (at(pin, "net")?, at(pin, "owner")?);
            let net_index = integer(net).filter(|n| (*n as usize) < ns.len());
            need(net.is_null() || net_index.is_some(), "MODEL_NET_INDEX")?;
            if at(owner, "kind")?.as_str() == Some("instance") {
                let inst = integer(at(owner, "instance")?).filter(|i| (*i as usize) < im.len());
                let inst = inst.ok_or_else(|| Rejected("MODEL_INSTANCE_INDEX".into()))?;
                if blocks.contains(&inst)
                    && let Some(net) = net_index
                {
                    touches.insert(net_name(&net_names, net)?);
                }
            } else {
                need(
                    at(owner, "kind")?.as_str() == Some("port"),
                    "MODEL_OWNER_KIND",
                )?;
            }
        }
        let all = sorted.into_iter().map(str::to_owned).collect();
        Ok((all, touches))
    }

    /// The typed operation that produced `record`.
    fn operation(
        &self,
        record: &Value,
        verb: &str,
        selection: &Value,
        revision: Option<&str>,
    ) -> R<&Value> {
        let operation = self
            .operation_map
            .get(text(at(record, "path")?)?)
            .ok_or_else(|| Rejected("ARTIFACT_HAS_NO_NATIVE_OPERATION".into()))?;
        need(
            at(operation, "report")? == record
                && at(operation, "operation")?.as_str() == Some(verb),
            "NATIVE_OPERATION_BINDING",
        )?;
        need(
            at(operation, "selection")? == selection,
            "NATIVE_OPERATION_SELECTION_BINDING",
        )?;
        if let Some(revision) = revision {
            need(
                at(operation, "revision")?.as_str() == Some(revision),
                "NATIVE_OPERATION_REVISION",
            )?;
        }
        Ok(operation)
    }

    /// One check report against its selection, revision and inventory.
    #[allow(clippy::too_many_lines)]
    fn report(
        &mut self,
        record: &Value,
        selection: &Value,
        revision: &str,
        inv: &Inventory,
        source: Option<&str>,
    ) -> R<Summary> {
        self.check_time()?;
        need(self.report_summaries.len() <= 4096, "GROUP_REPORT_COUNT")?;
        let ns = names(&self.data(selection, 1 << 20)?, 128)?;
        let index: HashMap<&str, usize> = ns
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        self.operation(record, "check-antenna", selection, Some(revision))?;
        let mut counts = vec![0i128; ns.len()];
        let mut bad = vec![0i128; ns.len()];
        let mut lef = vec![0i128; ns.len()];
        // Rows stream before the metadata is read: they are checked against the
        // session's declared row scope, which the report must then repeat.
        let violated_only = self.violated_only;
        let start = self.start;
        let mut row = |r: Value| -> R<()> {
            need(start.elapsed().as_secs_f64() <= 170.0, "VERIFIER_DEADLINE")?;
            let slot = r
                .as_dict()
                .and_then(|row| row.get("net"))
                .and_then(Value::as_str)
                .and_then(|net| index.get(net));
            let slot = *slot.ok_or_else(|| Rejected("CHECK_ROW_NET_SCOPE".into()))?;
            let verdict = (
                r.get("violated").and_then(Value::as_bool),
                r.get("exceeds_lef_limit").and_then(Value::as_bool),
            );
            let (Some(violated), Some(exceeds)) = verdict else {
                return Err(Rejected("CHECK_ROW_VERDICT".into()));
            };
            need(
                violated || !violated_only,
                "NONVIOLATED_ROW_IN_VIOLATED_ONLY_REPORT",
            )?;
            need(violated || !exceeds, "LEF_EXCESS_WITHOUT_VIOLATION")?;
            counts[slot] += 1;
            bad[slot] += i128::from(violated);
            lef[slot] += i128::from(exceeds);
            for key in [
                "gate_area_um2",
                "diff_area_um2",
                "metal_um2",
                "denominator_um2",
                "ratio",
                "limit",
                "effective_limit",
            ] {
                let fine = match r.get(key) {
                    Some(Value::Int(n)) => *n >= 0,
                    Some(Value::Float(f)) => f.is_finite() && *f >= 0.0,
                    _ => false,
                };
                need(fine, "CHECK_ROW_NUMERICS")?;
            }
            Ok(())
        };
        let path = self.file(record, 128 << 20)?;
        let mut required = HEADER.to_vec();
        required.push("checks");
        let (d, nrows) = Reader::new(None).scan(&path, &required, &mut row)?;
        let d = Value::Dict(d);
        let nrows = nrows as i128;
        let check_rows = d.get("check_rows");
        need(
            if violated_only {
                check_rows.and_then(Value::as_str) == Some("violated")
            } else {
                check_rows.is_none_or(Value::is_null)
            },
            "CHECK_ROW_SCOPE",
        )?;
        need(
            at(&d, "schema")?.as_str() == Some("rustroad.antenna-check.v1")
                && at(&d, "workspace_revision")?.as_str() == Some(revision),
            "REPORT_REVISION",
        )?;
        let scope = at(&d, "scope")?;
        need(
            at(&d, "sources")? == &inv.sources
                && at(scope, "routing_scope")?.as_str() == Some(&self.route_scope),
            "REPORT_SOURCE_SCOPE",
        )?;
        need(
            at(scope, "extra_shapes")? == &Value::Int(0)
                && at(scope, "drc_status")?.as_str() == Some("unchecked")
                && at(at(&d, "safety_margin")?, "fraction")? == &Value::Float(0.1),
            "REPORT_MODEL_SCOPE",
        )?;
        let s = at(&d, "selection")?;
        need(
            at(s, "schema")?.as_str() == Some("rustroad.antenna-selected.v1")
                && at(s, "scope")?.as_str() == Some("explicit_routed_net_subset"),
            "REPORT_SELECTION_SCOPE",
        )?;
        let listed = list(ns.iter());
        need(
            at(s, "canonical_nets")? == &listed
                && at(s, "selection_sha256")?.as_str() == Some(&fingerprint(&listed)?)
                && at(s, "limits")? == &limits()
                && digest(at(s, "source_sha256")?),
            "REPORT_SELECTION_BINDING",
        )?;
        let report_source = text(at(s, "source_sha256")?)?.to_owned();
        if let Some(source) = source {
            need(report_source == source, "REPORT_PHYSICAL_SOURCE")?;
        }
        let coverage = at(s, "coverage")?;
        let rows_listed = coverage.as_list().filter(|rows| rows.len() == ns.len());
        let rows_listed = rows_listed.ok_or_else(|| Rejected("REPORT_COVERAGE_COUNT".into()))?;
        let mut partial = false;
        let mut rows = Vec::with_capacity(ns.len());
        let mut evaluated = 0i128;
        for (slot, (expected, c)) in ns.iter().zip(rows_listed).enumerate() {
            need(
                c.as_dict().is_some_and(|c| {
                    c.has_exactly(&[
                        "net",
                        "shapes",
                        "rows",
                        "gate_terminals",
                        "observation",
                        "model_partial",
                    ])
                }) && at(c, "net")?.as_str() == Some(expected),
                "REPORT_COVERAGE_IDENTITY",
            )?;
            let fields = (
                at(c, "model_partial")?.as_bool(),
                integer(at(c, "shapes")?),
                integer(at(c, "rows")?),
                integer(at(c, "gate_terminals")?),
            );
            let (Some(model_partial), Some(shapes), Some(row_count), Some(gate_terminals)) = fields
            else {
                return Err(Rejected("REPORT_COVERAGE_TYPES".into()));
            };
            need(
                shapes <= 200_000
                    && if violated_only {
                        row_count >= counts[slot]
                    } else {
                        row_count == counts[slot]
                    },
                "REPORT_ROW_COUNT_MISMATCH",
            )?;
            let observation = at(c, "observation")?.as_str().unwrap_or("");
            need(
                matches!(
                    observation,
                    "observed" | "no_geometry" | "no_gate_area" | "no_applicable_rule_observation"
                ),
                "OBSERVATION_KIND",
            )?;
            if observation == "observed" {
                need(row_count > 0, "OBSERVED_WITH_NO_ROWS")?;
            } else {
                need(row_count == 0, "UNOBSERVED_WITH_ROWS")?;
            }
            if observation == "no_geometry" {
                need(shapes == 0, "NO_GEOMETRY_HAS_SHAPES")?;
            }
            if observation == "no_gate_area" {
                need(gate_terminals == 0, "NO_GATE_HAS_TERMINALS")?;
            }
            partial |= model_partial || observation != "observed";
            evaluated += row_count;
            rows.push((
                expected.clone(),
                Coverage {
                    rows: row_count,
                    model_partial,
                    observation: observation.to_owned(),
                },
            ));
        }
        need(
            counts.iter().sum::<i128>() == nrows && nrows <= evaluated && evaluated <= 100_000,
            "REPORT_TOTAL_ROWS",
        )?;
        need(
            integer(at(&d, "violations")?) == Some(bad.iter().sum())
                && integer(at(&d, "lef_violations")?) == Some(lef.iter().sum()),
            "REPORT_VERDICT_COUNT_MISMATCH",
        )?;
        let unsupported = at(&d, "unsupported")?
            .as_list()
            .filter(|u| u.len() <= 100_000);
        let unsupported = unsupported.ok_or_else(|| Rejected("UNSUPPORTED_BUDGET".into()))?;
        partial |= !unsupported.is_empty();
        let worst = at(&d, "nets")?.as_list().filter(|w| w.len() <= ns.len());
        let worst = worst.ok_or_else(|| Rejected("WORST_NET_BUDGET".into()))?;
        let mut seen = vec![false; ns.len()];
        for r in worst {
            let slot = at(r, "net")?
                .as_str()
                .and_then(|name| index.get(name))
                .copied();
            let violated = at(r, "violated")?.as_bool();
            let (Some(slot), Some(violated)) = (slot.filter(|slot| !seen[*slot]), violated) else {
                return Err(Rejected("WORST_NET_IDENTITY".into()));
            };
            seen[slot] = true;
            need(
                violated == (bad[slot] != 0),
                "WORST_NET_HIDES_ROW_VIOLATION",
            )?;
        }
        need(
            (0..ns.len()).all(|slot| {
                (counts[slot] == 0 || seen[slot]) && ((rows[slot].1.rows != 0) == seen[slot])
            }),
            "WORST_NET_OMITS_OBSERVED_NET",
        )?;
        let violated: BTreeSet<String> = ns
            .iter()
            .enumerate()
            .filter(|(slot, _)| bad[*slot] != 0)
            .map(|(_, name)| name.clone())
            .collect();
        let status = at(&d, "status")?.as_str();
        partial |= status == Some("subset_partial");
        let expected_status = if !violated.is_empty() {
            "subset_violated"
        } else if partial {
            "subset_partial"
        } else {
            "subset_clean"
        };
        need(status == Some(expected_status), "REPORT_STATUS")?;
        let stats = at(&d, "stats")?;
        let work = integer(at(stats, "work")?).filter(|work| *work <= MAX_WORK);
        let work = work.ok_or_else(|| Rejected("REPORT_WORK".into()))?;
        if let Some(checked) = object(stats)?.get("partial_nets_checked") {
            need(checked == &Value::Int(evaluated), "STATS_CHECK_ROW_COUNT")?;
        }
        let memory = at(stats, "report_memory")?;
        let mut ledger = [0i128; 5];
        for (slot, key) in ledger.iter_mut().zip([
            "limit_bytes",
            "live_bytes",
            "peak_live_bytes",
            "cumulative_reserved_bytes",
            "cumulative_row_bytes",
        ]) {
            *slot =
                integer(at(memory, key)?).ok_or_else(|| Rejected("REPORT_MEMORY_TYPE".into()))?;
        }
        let [limit, live, peak, reserved, _] = ledger;
        need(
            limit == 128 << 20 && live <= peak && peak <= limit && peak <= reserved,
            "REPORT_MEMORY_LEDGER",
        )?;
        let compact = Value::Dict(
            Dict::new()
                .with("revision", revision)
                .with("source", report_source.as_str())
                .with("work", work)
                .with("selection", selection)
                .with("coverage_sha256", fingerprint(coverage)?)
                .with("row_count", nrows),
        );
        let path = text(at(record, "path")?)?;
        match self.report_summaries.get(path) {
            Some(known) => need(known == &compact, "REPORT_REPARSE_DIFFERS")?,
            None => {
                self.report_summaries.insert(path.to_owned(), compact);
            }
        }
        let mut header = Dict::new();
        for key in HEADER {
            header.insert(key, at(&d, key)?);
        }
        Ok(Summary {
            names: ns,
            coverage: rows,
            partial,
            violating: violated,
            source: report_source,
            header: Value::Dict(header),
        })
    }

    fn group_record(
        &mut self,
        record: &Value,
        revision: &str,
        inv: &Inventory,
        source: Option<&str>,
    ) -> R<Summary> {
        let summary = self.report(
            at(record, "report")?,
            at(record, "selection")?,
            revision,
            inv,
            source,
        )?;
        need(
            self.data(at(record, "header")?, 4 << 20)? == summary.header,
            "HEADER_DIFFERS_FROM_RAW_REPORT",
        )?;
        Ok(summary)
    }

    fn repair_report(&mut self, record: &Value) -> R<Dict> {
        let path = self.file(record, 128 << 20)?;
        let mut refuse =
            |_: Value| -> R<()> { Err(Rejected("UNEXPECTED_REPAIR_CHECK_ARRAY".into())) };
        Ok(Reader::new(Some(8 << 20))
            .scan(&path, &REPAIR_FIELDS, &mut refuse)?
            .0)
    }

    #[allow(clippy::too_many_lines)]
    fn run(&mut self) -> R<Dict> {
        let cfg_lineage = self.cfg("lineage")?;
        let tool = self.cfg("tool")?;
        let workspace_text = self
            .workspace
            .to_str()
            .ok_or_else(|| Rejected("workspace path is not UTF-8".into()))?
            .to_owned();
        let lineage = self.data(cfg_lineage, 16 << 20)?;
        self.route_scope = text(at(&lineage, "route_scope")?)?.to_owned();
        need(
            self.route_scope == "all_signal_nets",
            "FULL_ROUTED_SCOPE_REQUIRED",
        )?;
        let schema = at(&lineage, "schema")?.as_str().unwrap_or("");
        need(
            matches!(
                schema,
                "rusteda.native-antenna-lineage.v3" | "rusteda.native-antenna-lineage.v4"
            ) && at(&lineage, "scope")?.as_str() == Some("supported_standard_cell_subset"),
            "LINEAGE_SCHEMA",
        )?;
        self.logical = schema.ends_with(".v4");
        need(
            at(&lineage, "workspace")?.as_str() == Some(&workspace_text)
                && at(&lineage, "tool")? == tool
                && at(&lineage, "policy")? == &policy(),
            "LINEAGE_IDENTITY_POLICY",
        )?;
        self.file(tool, 4 << 30)?;
        if let Some(manifest_id) = self.cfg.get("source_manifest") {
            let manifest = self.data(manifest_id, 4 << 20)?;
            let sources = at(&manifest, "sources")?
                .as_list()
                .filter(|s| s.len() <= 128);
            let sources = sources.ok_or_else(|| Rejected("SOURCE_COUNT_BUDGET".into()))?;
            for row in sources {
                let record = Dict::new()
                    .with("path", at(row, "path")?)
                    .with("sha256", at(row, "sha256")?)
                    .with("bytes", at(row, "bytes")?);
                self.file(&Value::Dict(record), 4 << 20)?;
            }
        }
        let controller_id = self.cfg("controller_inputs")?;
        let controller = self.data(controller_id, 2 << 20)?;
        need(
            object(&controller)?.has_exactly(&[
                "schema",
                "design_workspace",
                "initial_revision",
                "tool",
                "limits",
                "grouping",
            ]) && at(&controller, "schema")?.as_str() == Some("rusteda.native-antenna-inputs.v2"),
            "CONTROLLER_INPUT_SCHEMA",
        )?;
        need(
            at(&controller, "design_workspace")?.as_str() == Some(&workspace_text)
                && at(&controller, "tool")? == tool
                && at(&controller, "initial_revision")? == at(&lineage, "initial_revision")?
                && at(&controller, "grouping")? == &policy(),
            "CONTROLLER_INPUT_BINDING",
        )?;
        let limits_in = at(&controller, "limits")?;
        need(
            object(limits_in)?.has_exactly(&[
                "threads",
                "max_rounds",
                "max_selected_nets",
                "max_work",
                "max_expansions",
                "deadline_seconds",
            ]),
            "CONTROLLER_LIMIT_SCHEMA",
        )?;
        let limit = |key: &str| at(limits_in, key).ok().and_then(integer);
        need(
            object(limits_in)?.values().all(|v| integer(v).is_some())
                && matches!(limit("threads"), Some(2 | 4 | 16))
                && matches!(limit("max_rounds"), Some(1..=3))
                && limit("max_selected_nets") == Some(100_000)
                && limit("max_work") == Some(MAX_WORK)
                && limit("max_expansions") == Some(8_000_000)
                && limit("deadline_seconds") == Some(1800),
            "CONTROLLER_LIMIT_BINDING",
        )?;
        let (threads, max_rounds) = (
            limit("threads").unwrap_or(0),
            limit("max_rounds").unwrap_or(0),
        );
        if let Some(bound) = object(&lineage)?.get("controller_inputs") {
            need(bound == controller_id, "LINEAGE_CONTROLLER_INPUT_BINDING")?;
        }
        need(
            at(&lineage, "execution_mode")?.as_str() == Some("single_native_process")
                && at(&lineage, "actual_native_processes")?.as_int() == Some(1),
            "NATIVE_EXECUTION_MODE",
        )?;
        let commands = at(&lineage, "commands")?.as_list().filter(|c| c.len() == 1);
        let commands = commands.ok_or_else(|| Rejected("COMMAND_COUNT_EXACTLY_ONE".into()))?;
        let command = &commands[0];
        need(
            object(command)?.has_exactly(&[
                "command",
                "exit_code",
                "stdout",
                "stderr",
                "wall_seconds",
                "output_bytes",
            ]),
            "NATIVE_PROCESS_RECEIPT_SCHEMA",
        )?;
        need(
            at(command, "exit_code")?.as_int() == Some(0),
            "FAILED_COMMAND_IN_SUCCESS_LINEAGE",
        )?;
        let (out, err) = (at(command, "stdout")?, at(command, "stderr")?);
        self.file(out, 16 << 20)?;
        self.file(err, 16 << 20)?;
        let count = int(at(out, "bytes")?)? + int(at(err, "bytes")?)?;
        need(
            integer(at(command, "output_bytes")?) == Some(count),
            "COMMAND_BYTE_COUNT",
        )?;
        need(
            integer(at(&lineage, "process_report_bytes")?) == Some(count)
                && count <= MAX_REPORT_BYTES,
            "PROCESS_REPORT_BYTE_BUDGET",
        )?;
        let finite_seconds = |value: &Value| -> Option<f64> {
            match value {
                Value::Int(n) => Some(*n as f64),
                Value::Float(f) if f.is_finite() => Some(*f),
                _ => None,
            }
        };
        let command_wall = finite_seconds(at(command, "wall_seconds")?)
            .filter(|wall| (0.0..=1800.0).contains(wall));
        let command_wall =
            command_wall.ok_or_else(|| Rejected("NATIVE_PROCESS_WALL_BUDGET".into()))?;
        let native_plan = at(&lineage, "native_plan")?;
        let plan_path = self.file(native_plan, 64 << 10)?;
        let plan = self.data(native_plan, 64 << 10)?;
        need(
            object(&plan)?.has_exactly(&[
                "schema",
                "output",
                "max_rounds",
                "deadline_seconds",
                "policy",
            ]),
            "NATIVE_PLAN_SCHEMA",
        )?;
        need(
            at(&plan, "schema")?.as_str() == Some("rustroad.antenna-group-repair.v1")
                && at(&plan, "max_rounds")?.as_int() == Some(max_rounds)
                && at(&plan, "deadline_seconds")?.as_int() == Some(1800),
            "NATIVE_PLAN_DEFAULTS",
        )?;
        let full = policy();
        let mut plan_policy = Dict::new();
        for key in [
            "group_names",
            "max_groups",
            "max_attempts",
            "max_total_jumpers",
            "max_commands",
            "max_report_bytes",
            "max_cumulative_work",
        ] {
            plan_policy.insert(key, at(&full, key)?);
        }
        need(
            at(&plan, "policy")? == &Value::Dict(plan_policy),
            "NATIVE_PLAN_POLICY",
        )?;
        let session_dir = plan_path
            .parent()
            .ok_or_else(|| Rejected("NATIVE_PLAN_OUTPUT".into()))?
            .to_owned();
        let native = session_dir.join("native-reports");
        let native_text = native.to_string_lossy().into_owned();
        let plan_text = plan_path.to_string_lossy().into_owned();
        need(
            plan_path
                .file_name()
                .is_some_and(|name| name == "native-plan.json")
                && at(&plan, "output")?.as_str() == Some(&native_text),
            "NATIVE_PLAN_OUTPUT",
        )?;
        need(
            Path::new(text(at(out, "path")?)?) == session_dir.join("native-session.json")
                && Path::new(text(at(err, "path")?)?) == session_dir.join("native-session.stderr"),
            "NATIVE_PROCESS_CAPTURE_PATHS",
        )?;
        let argv = at(command, "command")?;
        let argv_items = argv
            .as_list()
            .filter(|argv| argv.len() == 12 && argv.iter().all(|x| x.as_str().is_some()));
        need(argv_items.is_some(), "NATIVE_COMMAND_SCHEMA")?;
        let initial = text(at(&lineage, "initial_revision")?)?.to_owned();
        let expected_argv = list([
            text(at(tool, "path")?)?,
            "--workspace",
            &workspace_text,
            "--threads",
            &threads.to_string(),
            "--max-expansions",
            "8000000",
            "repair-antenna-groups",
            "--base-rev",
            &initial,
            "--plan",
            &plan_text,
        ]);
        need(argv == &expected_argv, "NATIVE_COMMAND_EXACT_BINDING")?;
        let captured_command = identity(
            &session_dir
                .join("native-session-command.json")
                .to_string_lossy(),
            64 << 10,
        )?;
        need(
            &self.data(&captured_command, 64 << 10)? == argv,
            "NATIVE_CAPTURED_COMMAND_DIFFERS",
        )?;
        let summary_id = at(&lineage, "native_summary")?;
        let native_summary_path = self.file(summary_id, 16 << 20)?;
        need(
            native_summary_path == native.join("summary.json")
                && at(out, "sha256")? == at(summary_id, "sha256")?
                && at(out, "bytes")? == at(summary_id, "bytes")?,
            "NATIVE_STDOUT_SUMMARY_BYTES",
        )?;
        need(
            std::fs::read(text(at(out, "path")?)?)? == std::fs::read(&native_summary_path)?,
            "NATIVE_STDOUT_SUMMARY_RAW_BYTES",
        )?;
        self.check_time()?;
        let native_summary = self.data(summary_id, 16 << 20)?;
        need(
            at(&native_summary, "schema")?.as_str()
                == Some(if self.logical {
                    "rustroad.antenna-group-repair-result.v2"
                } else {
                    "rustroad.antenna-group-repair-result.v1"
                }),
            "NATIVE_SUMMARY_SCHEMA",
        )?;
        let mut shared = vec![
            "status",
            "initial_revision",
            "final_revision",
            "scope",
            "route_scope",
            "inventory_status",
            "native_analysis_status",
            "observations",
            "excluded_macro_touch",
            "unresolved_supported_nets",
            "requires_full_revalidation",
            "whole_layout_antenna_status",
            "total_jumpers",
            "stop_reason",
            "skipped_futile_groups",
            "reported_work",
            "unreported_validation_work_upper_bound",
            "work_budget_charge",
            "report_bytes",
        ];
        if self.logical {
            shared.extend(["logical_revisions", "committed_revision", "check_rows"]);
            let check_rows = at(&native_summary, "check_rows")?.as_str();
            need(
                matches!(check_rows, Some("violated" | "all"))
                    && integer(at(&native_summary, "logical_revisions")?).is_some(),
                "NATIVE_LOGICAL_SUMMARY",
            )?;
            self.violated_only = check_rows == Some("violated");
        }
        for key in shared {
            need(
                at(&native_summary, key)? == at(&lineage, key)?,
                "NATIVE_SUMMARY_LINEAGE_ACCOUNTING",
            )?;
        }
        need(
            at(&native_summary, "whole_layout_antenna_status")?.as_str() == Some("unchecked")
                && at(&native_summary, "drc_status")?.as_str() == Some("unchecked")
                && at(&native_summary, "corner")?.as_str() == Some("not_applicable")
                && at(&native_summary, "wire_model")?.as_str() == Some("none")
                && at(&native_summary, "unconstrained_endpoints")?.is_null()
                && at(&native_summary, "requires_full_revalidation")? == &Value::Bool(true)
                && at(&native_summary, "requires_full_revalidation")?
                    .as_bool()
                    .is_some(),
            "NATIVE_SUMMARY_TRUTHFULNESS",
        )?;
        need(
            finite_seconds(at(&native_summary, "wall_seconds")?)
                .is_some_and(|wall| 0.0 <= wall && wall <= command_wall),
            "NATIVE_SUMMARY_DEADLINE",
        )?;
        let final_groups = items(at(&lineage, "final_groups")?)?;
        let group_count = integer(at(&native_summary, "groups")?);
        need(
            group_count == Some(final_groups.len() as i128)
                && integer(at(&native_summary, "skipped_futile_groups")?).is_some(),
            "NATIVE_SUMMARY_GROUP_COUNT",
        )?;
        let group_count = group_count.unwrap_or(0);
        let operations = at(&lineage, "operations")?
            .as_list()
            .filter(|ops| (4..=4096).contains(&ops.len()));
        need(
            operations.is_some_and(|ops| {
                at(&lineage, "logical_operations")
                    .ok()
                    .and_then(Value::as_int)
                    == Some(ops.len() as i128)
            }),
            "LOGICAL_OPERATION_COUNT",
        )?;
        let operations = operations.unwrap_or_default();
        let mut operation_names = HashSet::new();
        let mut report_bytes = 0i128;
        for operation in operations {
            need(
                operation.as_dict().is_some_and(|op| {
                    op.has_exactly(&[
                        "kind",
                        "name",
                        "operation",
                        "revision",
                        "report",
                        "selection",
                        "selected_report",
                        "base_revision",
                    ])
                }) && at(operation, "kind")?.as_str() == Some("native_in_process_operation"),
                "NATIVE_OPERATION_SCHEMA",
            )?;
            let name = at(operation, "name")?.as_str().filter(|name| {
                (1..=128).contains(&name.len())
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    && !operation_names.contains(*name)
            });
            let name = name.ok_or_else(|| Rejected("NATIVE_OPERATION_NAME".into()))?;
            operation_names.insert(name.to_owned());
            let verb = at(operation, "operation")?.as_str().unwrap_or("");
            let revision = at(operation, "revision")?.as_str();
            need(
                revision.is_some_and(revision_name),
                "NATIVE_OPERATION_REVISION",
            )?;
            let record = at(operation, "report")?;
            let path = self.file(record, 128 << 20)?;
            let record_path = text(at(record, "path")?)?;
            need(
                path == native.join(format!("{name}.json"))
                    && !self.operation_map.contains_key(record_path),
                "NATIVE_OPERATION_REPORT_PATH",
            )?;
            self.operation_map
                .insert(record_path.to_owned(), operation.clone());
            report_bytes += int(at(record, "bytes")?)?;
            let numbered = |prefix: &str, rest: &str| -> bool {
                rest.strip_prefix(prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            };
            match verb {
                "check-antenna" => {
                    let round_group = name
                        .strip_prefix("round-")
                        .and_then(|rest| rest.split_once("-group-"));
                    let named = round_group
                        .is_some_and(|(a, b)| numbered("", a) && numbered("", b))
                        || name
                            .strip_suffix("-before")
                            .is_some_and(|rest| numbered("repair-", rest))
                        || numbered("final-group-", name);
                    need(named, "NATIVE_CHECK_NAME")?;
                    need(
                        self.file(at(operation, "selection")?, 1 << 20)?
                            == native.join(format!("{name}-selection.json"))
                            && at(operation, "selected_report")?.is_null()
                            && at(operation, "base_revision")?.is_null(),
                        "NATIVE_CHECK_INPUTS",
                    )?;
                }
                "repair-antenna" => {
                    need(
                        numbered("repair-", name)
                            && at(operation, "base_revision")?.as_str() == revision,
                        "NATIVE_REPAIR_NAME_BASE",
                    )?;
                    need(
                        self.file(at(operation, "selection")?, 1 << 20)?
                            == native.join(format!("{name}-selection.json"))
                            && self.file(at(operation, "selected_report")?, 128 << 20)?
                                == native.join(format!("{name}-before.json")),
                        "NATIVE_REPAIR_INPUTS",
                    )?;
                }
                _ => {
                    need(
                        matches!(
                            (verb, name),
                            ("antenna-inventory", "inventory-initial" | "inventory-final")
                                | ("export-connectivity", "model-initial" | "model-final")
                        ),
                        "UNKNOWN_NATIVE_OPERATION",
                    )?;
                    need(
                        at(operation, "selection")?.is_null()
                            && at(operation, "selected_report")?.is_null()
                            && at(operation, "base_revision")?.is_null(),
                        "NATIVE_READONLY_INPUTS",
                    )?;
                }
            }
        }
        need(
            integer(at(&lineage, "report_bytes")?) == Some(report_bytes)
                && report_bytes <= MAX_REPORT_BYTES,
            "GLOBAL_REPORT_BYTE_BUDGET",
        )?;
        need(
            count + report_bytes <= MAX_REPORT_BYTES,
            "TOTAL_PROCESS_AND_REPORT_BYTE_BUDGET",
        )?;
        for (key, name) in [
            ("initial_inventory", "inventory-initial"),
            ("final_inventory", "inventory-final"),
            ("initial_model", "model-initial"),
            ("final_model", "model-final"),
        ] {
            need(
                at(at(&lineage, key)?, "path")?.as_str()
                    == Some(&native.join(format!("{name}.json")).to_string_lossy()),
                "NATIVE_INITIAL_FINAL_ROLE_BINDING",
            )?;
        }
        let final_revision = text(at(&lineage, "final_revision")?)?.to_owned();
        let mut current = initial.clone();
        self.revision(at(&lineage, "initial")?, &initial, None)?;
        let scope = self.route_scope.clone();
        let inv = self.inventory(at(&lineage, "initial_inventory")?, &initial, &scope)?;
        self.operation(
            at(&lineage, "initial_inventory")?,
            "antenna-inventory",
            &Value::Null,
            Some(&initial),
        )?;
        let (model_names, touches) = self.model(at(&lineage, "initial_model")?)?;
        self.operation(
            at(&lineage, "initial_model")?,
            "export-connectivity",
            &Value::Null,
            Some(&initial),
        )?;
        need(
            inv.names.iter().all(|name| model_names.contains(name))
                && at(&inv.counts, "model_logical_net_count")?
                    == &Value::Int(model_names.len() as i128),
            "INVENTORY_MODEL_MEMBERSHIP",
        )?;
        let attempts = at(&lineage, "attempts")?
            .as_list()
            .filter(|a| a.len() <= 64);
        let attempts = attempts.ok_or_else(|| Rejected("ATTEMPT_COUNT".into()))?;
        let mut jumpers = 0usize;
        let mut mutating: Vec<Value> = Vec::new();
        let mut states: HashMap<String, String> = HashMap::new();
        let mut pending_child: Option<(String, String)> = None;
        for attempt in attempts {
            need(
                integer(at(attempt, "round")?).is_some_and(|round| round < max_rounds)
                    && integer(at(attempt, "group")?).is_some_and(|group| group < group_count),
                "ATTEMPT_ROUND_GROUP_BUDGET",
            )?;
            let base = text(at(attempt, "base_revision")?)?.to_owned();
            need(base == current, "BROKEN_ATTEMPT_CHAIN")?;
            let plan = self.group_record(at(attempt, "planning")?, &base, &inv, None)?;
            if let Some((revision, source)) = pending_child.take() {
                // The first check of a new logical revision: a different physical state.
                need(
                    revision == base && plan.source != source,
                    "LOGICAL_REVISION_UNCHANGED_SOURCE",
                )?;
            }
            let mut eligible = Vec::new();
            for name in &plan.violating {
                if touches.contains(name) {
                    continue;
                }
                let covered = plan.covered(name)?;
                if !covered.model_partial && covered.observation == "observed" {
                    eligible.push(name.clone());
                }
            }
            let before = self.group_record(at(attempt, "before")?, &base, &inv, None)?;
            let selected = names(&self.data(at(attempt, "selected")?, 1 << 20)?, 128)?;
            need(
                selected == eligible
                    && eligible == before.names
                    && before.violating.iter().eq(selected.iter()),
                "REPAIR_EXACT_CANONICAL_SELECTION",
            )?;
            need(
                plan.source == before.source,
                "FRESH_CHECK_PHYSICAL_SOURCE_DIFFERS",
            )?;
            for name in &selected {
                let covered = before.covered(name)?;
                need(
                    !covered.model_partial && covered.observation == "observed",
                    "FRESH_REPAIR_ELIGIBILITY_CHANGED",
                )?;
            }
            let repair_id = at(attempt, "repair_report")?;
            let repair = Value::Dict(self.repair_report(repair_id)?);
            let operation = self
                .operation(
                    repair_id,
                    "repair-antenna",
                    at(attempt, "selected")?,
                    Some(&base),
                )?
                .clone();
            need(
                at(&operation, "selected_report")? == at(at(attempt, "before")?, "report")?,
                "REPAIR_CONSUMED_FOREIGN_REPORT",
            )?;
            need(
                at(&operation, "base_revision")?.as_str() == Some(&base),
                "REPAIR_BASE_OPERATION",
            )?;
            need(
                at(&repair, "schema")?.as_str() == Some("rustroad.antenna-repair.v1")
                    && at(&repair, "base_revision")?.as_str() == Some(&base)
                    && at(&repair, "revision")? == at(attempt, "revision")?,
                "REPAIR_REVISION_BINDING",
            )?;
            let selected_value = list(selected.iter());
            need(
                at(&repair, "routing_scope")?.as_str() == Some(&self.route_scope)
                    && at(&repair, "repair_scope")?.as_str() == Some("selected_supported_nets")
                    && at(&repair, "selected_nets")? == &selected_value,
                "REPAIR_SCOPE",
            )?;
            need(
                at(&repair, "requires_full_revalidation")?.as_bool() == Some(true)
                    && at(&repair, "drc_status")?.as_str() == Some("unchecked")
                    && at(&repair, "antenna_status")?.as_str()
                        == Some("partial_selected_supported_nets")
                    && at(&repair, "safety_margin")? == &Value::Float(0.1),
                "REPAIR_TRUTHFULNESS",
            )?;
            let js = at(&repair, "jumpers")?
                .as_list()
                .filter(|js| jumpers + js.len() <= 4096);
            let js = js.ok_or_else(|| Rejected("TOTAL_JUMPER_BUDGET".into()))?;
            jumpers += js.len();
            let analysis = at(&repair, "selected_analysis")?;
            need(
                matches!(
                    at(&repair, "status")?.as_str(),
                    Some(
                        "subset_clean" | "subset_repaired" | "subset_partial" | "subset_unrepaired"
                    )
                ) && at(analysis, "canonical_nets")? == &selected_value
                    && at(analysis, "limits")? == &limits(),
                "REPAIR_SELECTED_ANALYSIS",
            )?;
            let stats = at(&repair, "stats")?;
            let costs = (
                integer(at(stats, "antenna_work")?).filter(|work| *work <= MAX_WORK),
                integer(at(stats, "planner_work")?).filter(|work| *work <= 2 * MAX_WORK),
            );
            let (Some(antenna), Some(planner)) = costs else {
                return Err(Rejected("REPAIR_NEGATIVE_OR_EXCESS_WORK".into()));
            };
            let accounting = Value::Dict(
                Dict::new()
                    .with("reported_antenna_work", antenna)
                    .with("reported_planner_work", planner)
                    .with("unreported_validation_work_upper_bound", MAX_WORK)
                    .with("budget_charge", antenna + planner + MAX_WORK),
            );
            let recorded = at(attempt, "work_accounting")?;
            need(
                recorded == &accounting && object(recorded)?.values().all(|x| integer(x).is_some()),
                "UNREPORTED_VALIDATION_MISLABELED",
            )?;
            let child = text(at(attempt, "revision")?)?.to_owned();
            self.repair_costs.insert(
                text(at(repair_id, "path")?)?.to_owned(),
                RepairCost {
                    antenna,
                    planner,
                    base: base.clone(),
                    child: child.clone(),
                },
            );
            let row = at(attempt, "child")?;
            let materialized = object(row)?.get("materialized");
            if self.logical && !materialized.is_some_and(Value::truthy) {
                need(
                    object(row)?.has_exactly(&["revision", "parent_revision", "materialized"])
                        && materialized.and_then(Value::as_bool) == Some(false),
                    "LOGICAL_REVISION_SCHEMA",
                )?;
                need(
                    revision_name(&child)
                        && at(row, "revision")?.as_str() == Some(&child)
                        && child != final_revision
                        && child != initial,
                    "LOGICAL_REVISION_IDENTITY",
                )?;
                need(
                    !self.workspace.join("revisions").join(&child).exists(),
                    "LOGICAL_REVISION_MATERIALIZED",
                )?;
                if child != base {
                    need(
                        at(row, "parent_revision")?.as_str() == Some(&base),
                        "BROKEN_PARENT_CHAIN",
                    )?;
                }
            } else if self.logical {
                need(
                    materialized.and_then(Value::as_bool) == Some(true)
                        && (child == initial || child == final_revision),
                    "MATERIALIZED_REVISION_SCOPE",
                )?;
                let plain = Value::Dict(
                    Dict::new()
                        .with("revision", at(row, "revision")?)
                        .with("parent_revision", at(row, "parent_revision")?)
                        .with("design_file", at(row, "design_file")?),
                );
                self.revision(
                    &plain,
                    &child,
                    (child != initial).then_some(initial.as_str()),
                )?;
                if child != base {
                    need(
                        at(row, "logical_parent_revision")?.as_str() == Some(&base),
                        "BROKEN_PARENT_CHAIN",
                    )?;
                }
            } else {
                self.revision(row, &child, (child != base).then_some(base.as_str()))?;
            }
            if self.logical {
                // A logical revision is one physical state: each check names its source.
                let known = states
                    .entry(base.clone())
                    .or_insert_with(|| plan.source.clone())
                    .clone();
                need(
                    known == plan.source && plan.source == before.source,
                    "LOGICAL_REVISION_SOURCE",
                )?;
                if child != base {
                    need(!states.contains_key(&child), "LOGICAL_REVISION_REUSED")?;
                    pending_child = Some((child.clone(), known));
                } else {
                    pending_child = None;
                }
            }
            if child == base {
                need(
                    js.is_empty() && at(&repair, "before")? == at(&repair, "after")?,
                    "NOOP_REPAIR_CHANGED",
                )?;
            } else {
                need(!js.is_empty(), "MUTATING_REPAIR_HAS_NO_JUMPER")?;
                mutating.push(attempt.clone());
            }
            current = child;
        }
        need(
            at(&lineage, "rounds")? == &Value::List(mutating.clone())
                && at(&lineage, "total_jumpers")? == &Value::Int(jumpers as i128),
            "ROUND_OR_JUMPER_LEDGER",
        )?;
        need(current == final_revision, "FINAL_REVISION_CHAIN")?;
        let last = self.inventory(at(&lineage, "final_inventory")?, &current, &scope)?;
        self.operation(
            at(&lineage, "final_inventory")?,
            "antenna-inventory",
            &Value::Null,
            Some(&current),
        )?;
        if self.logical {
            if let Some((revision, source)) = &pending_child {
                need(
                    *revision == current && last.source != *source,
                    "LOGICAL_REVISION_UNCHANGED_SOURCE",
                )?;
            }
            need(
                states
                    .get(&current)
                    .is_none_or(|known| *known == last.source),
                "LOGICAL_REVISION_SOURCE",
            )?;
            let committed = at(&lineage, "committed_revision")?;
            need(
                at(&lineage, "logical_revisions")? == &Value::Int(mutating.len() as i128)
                    && if mutating.is_empty() {
                        committed.is_null()
                    } else {
                        committed.as_str() == Some(&current)
                    },
                "COMMITTED_REVISION_LEDGER",
            )?;
            let materialized: BTreeSet<&str> = [initial.as_str(), current.as_str()].into();
            need(
                at(&lineage, "materialized_revisions")? == &list(materialized)
                    && at(&lineage, "check_rows")?.as_str()
                        == Some(if self.violated_only {
                            "violated"
                        } else {
                            "all"
                        }),
                "MATERIALIZED_REVISION_LEDGER",
            )?;
            let last_record = at(&lineage, "final")?;
            need(
                at(last_record, "materialized")?.as_bool() == Some(true)
                    && if mutating.is_empty() {
                        at(last_record, "parent_revision")?
                            == at(at(&lineage, "initial")?, "parent_revision")?
                    } else {
                        at(last_record, "parent_revision")?.as_str() == Some(&initial)
                    },
                "FINAL_REVISION_PARENT",
            )?;
            need(
                match mutating.last() {
                    Some(attempt) => {
                        at(last_record, "logical_parent_revision")? == at(attempt, "base_revision")?
                    }
                    None => at(last_record, "logical_parent_revision")?.is_null(),
                },
                "FINAL_LOGICAL_PARENT",
            )?;
        }
        need(
            last.names == inv.names
                && last.ids == inv.ids
                && last.counts == inv.counts
                && last.sources == inv.sources,
            "ROUTED_INVENTORY_CHANGED",
        )?;
        self.operation(
            at(&lineage, "final_model")?,
            "export-connectivity",
            &Value::Null,
            Some(&current),
        )?;
        self.file(at(&lineage, "final_model")?, 64 << 20)?;
        need(
            at(at(&lineage, "initial_model")?, "sha256")?
                == at(at(&lineage, "final_model")?, "sha256")?,
            "LOGICAL_CONNECTIVITY_CHANGED",
        )?;
        let mut covered: HashSet<String> = HashSet::new();
        let mut violating: BTreeSet<String> = BTreeSet::new();
        let mut partial = false;
        let mut observations: Vec<(String, i128)> = Vec::new();
        let expected: Vec<&[String]> = last.names.chunks(128).collect();
        need(
            !final_groups.is_empty()
                && final_groups.len() == expected.len()
                && expected.len() <= 1024,
            "FINAL_GROUP_COUNT",
        )?;
        for (index, (group, canonical)) in final_groups.iter().zip(&expected).enumerate() {
            need(
                at(group, "group")? == &Value::Int(index as i128)
                    && at(group, "revision")?.as_str() == Some(&current),
                "MIXED_FINAL_GROUP_REVISION",
            )?;
            let summary = self.group_record(group, &current, &last, Some(&last.source))?;
            need(
                summary.names == *canonical && !canonical.iter().any(|name| covered.contains(name)),
                "FINAL_GROUP_OVERLAP_OR_PARTITION",
            )?;
            covered.extend(canonical.iter().cloned());
            violating.extend(summary.violating.iter().cloned());
            partial |= summary.partial;
            for (_, row) in &summary.coverage {
                match observations
                    .iter_mut()
                    .find(|(kind, _)| *kind == row.observation)
                {
                    Some(entry) => entry.1 += 1,
                    None => observations.push((row.observation.clone(), 1)),
                }
            }
        }
        need(
            covered.len() == last.names.len() && last.names.iter().all(|n| covered.contains(n)),
            "FINAL_EXACT_UNION",
        )?;
        // BLOCK contacts have no independently proved complete native macro model.
        // Do not permit a raw clean label to turn this standard-cell scope into one.
        let macro_touch = covered
            .iter()
            .filter(|name| touches.contains(*name))
            .count();
        if macro_touch > 0 {
            need(partial, "MACRO_TOUCH_WITHOUT_PARTIAL_WITNESS")?;
        }
        need(
            at(&lineage, "native_analysis_status")?.as_str()
                == Some(if partial {
                    "partial"
                } else if violating.is_empty() {
                    "clean"
                } else {
                    "violated"
                }),
            "FALSE_NATIVE_CLEAN_LABEL",
        )?;
        let unresolved: Vec<&String> = violating.iter().filter(|n| !touches.contains(*n)).collect();
        let excluded: Vec<&String> = violating.iter().filter(|n| touches.contains(*n)).collect();
        need(
            at(&lineage, "status")?.as_str()
                == Some(if unresolved.is_empty() {
                    "complete"
                } else {
                    "unresolved"
                })
                && at(&lineage, "inventory_status")?.as_str() == Some("complete"),
            "LINEAGE_COMPLETION_LABEL",
        )?;
        let observed = Value::Dict(
            observations
                .iter()
                .map(|(kind, count)| (kind.clone(), Value::Int(*count)))
                .collect(),
        );
        need(
            at(&lineage, "observations")? == &observed
                && at(&lineage, "excluded_macro_touch")? == &list(excluded)
                && at(&lineage, "unresolved_supported_nets")? == &list(unresolved),
            "FINAL_OBSERVATION_LEDGER",
        )?;
        need(
            at(&lineage, "whole_layout_antenna_status")?.as_str() == Some("unchecked")
                && at(&lineage, "requires_full_revalidation")?.as_bool() == Some(true),
            "FALSE_WHOLE_LAYOUT_PASS",
        )?;
        // Replay every typed in-process operation, including planning groups which
        // scheduled no repair. No argv or subprocess receipt is invented per report.
        let mut command_revision = initial.clone();
        let mut source_by_revision: HashMap<String, String> = HashMap::new();
        source_by_revision.insert(initial.clone(), inv.source.clone());
        source_by_revision.insert(current.clone(), last.source.clone());
        for operation in operations {
            let path = at(operation, "report")?;
            let verb = at(operation, "operation")?.as_str().unwrap_or("");
            self.operation(
                path,
                verb,
                at(operation, "selection")?,
                Some(&command_revision),
            )?;
            let report_path = text(at(path, "path")?)?;
            match verb {
                "check-antenna" => {
                    need(
                        self.work_budget_charge + MAX_WORK <= MAX_CUMULATIVE_WORK,
                        "CHECK_WORK_ADMISSION",
                    )?;
                    let known = source_by_revision.get(&command_revision).cloned();
                    let summary = self.report(
                        path,
                        at(operation, "selection")?,
                        &command_revision,
                        &inv,
                        known.as_deref(),
                    )?;
                    source_by_revision
                        .entry(command_revision.clone())
                        .or_insert_with(|| summary.source.clone());
                    let stem = report_path
                        .strip_suffix(".json")
                        .ok_or_else(|| Rejected("UNREFERENCED_CHECK_HEADER_DIFFERS".into()))?;
                    let header = identity(&format!("{stem}-header.json"), 4 << 20)?;
                    need(
                        self.data(&header, 4 << 20)? == summary.header,
                        "UNREFERENCED_CHECK_HEADER_DIFFERS",
                    )?;
                    let work = self
                        .report_summaries
                        .get(report_path)
                        .and_then(|summary| summary.get("work"))
                        .and_then(Value::as_int)
                        .unwrap_or(0);
                    self.reported_work += work;
                    self.work_budget_charge += work;
                }
                "repair-antenna" => {
                    let cost = self
                        .repair_costs
                        .get(report_path)
                        .ok_or_else(|| Rejected("REPAIR_NOT_IN_ATTEMPT_CHAIN".into()))?;
                    need(cost.base == command_revision, "COMMAND_REPAIR_CHAIN")?;
                    need(
                        self.work_budget_charge + 4 * MAX_WORK <= MAX_CUMULATIVE_WORK,
                        "REPAIR_4W_ADMISSION",
                    )?;
                    self.reported_work += cost.antenna + cost.planner;
                    self.validation_upper_bound += MAX_WORK;
                    self.work_budget_charge += cost.antenna + cost.planner + MAX_WORK;
                    command_revision.clone_from(&cost.child);
                }
                _ => need(
                    matches!(verb, "antenna-inventory" | "export-connectivity"),
                    "UNKNOWN_LINEAGE_COMMAND",
                )?,
            }
        }
        need(command_revision == current, "OPERATION_REVISION_SCOPE")?;
        let native_attempts = at(&native_summary, "attempts")?
            .as_list()
            .filter(|rows| rows.len() == attempts.len());
        let native_attempts =
            native_attempts.ok_or_else(|| Rejected("NATIVE_SUMMARY_ATTEMPT_COUNT".into()))?;
        for (attempt, native_attempt) in attempts.iter().zip(native_attempts) {
            need(
                object(native_attempt)?.has_exactly(&[
                    "round",
                    "group",
                    "base_revision",
                    "revision",
                    "selected_nets",
                    "jumpers",
                ]),
                "NATIVE_SUMMARY_ATTEMPT_SCHEMA",
            )?;
            let mut bound = true;
            for key in ["round", "group", "base_revision", "revision"] {
                bound &= at(native_attempt, key)? == at(attempt, key)?;
            }
            need(
                bound
                    && at(native_attempt, "selected_nets")?
                        == &self.data(at(attempt, "selected")?, 1 << 20)?,
                "NATIVE_SUMMARY_ATTEMPT_BINDING",
            )?;
            let repair = Value::Dict(self.repair_report(at(attempt, "repair_report")?)?);
            need(
                at(native_attempt, "jumpers")?.as_int()
                    == Some(items(at(&repair, "jumpers")?)?.len() as i128),
                "NATIVE_SUMMARY_ATTEMPT_JUMPERS",
            )?;
        }
        need(
            integer(at(&lineage, "reported_work")?) == Some(self.reported_work)
                && integer(at(&lineage, "unreported_validation_work_upper_bound")?)
                    == Some(self.validation_upper_bound)
                && integer(at(&lineage, "work_budget_charge")?) == Some(self.work_budget_charge)
                && self.work_budget_charge <= MAX_CUMULATIVE_WORK,
            "WORK_BUDGET_LEDGER",
        )?;
        let last_record = at(&lineage, "final")?;
        if self.logical {
            let plain = Value::Dict(
                Dict::new()
                    .with("revision", at(last_record, "revision")?)
                    .with("parent_revision", at(last_record, "parent_revision")?)
                    .with("design_file", at(last_record, "design_file")?),
            );
            self.revision(&plain, &current, None)?;
        } else {
            self.revision(last_record, &current, None)?;
        }
        let snapshot = self.file(at(&lineage, "workspace_head")?, 128)?;
        let actual_head = self.workspace.join("HEAD");
        need(
            std::fs::metadata(&actual_head)?.len() <= 128,
            "WORKSPACE_HEAD_BYTE_BUDGET",
        )?;
        need(
            std::fs::read_to_string(&snapshot)? == format!("{current}\n")
                && strip(&std::fs::read_to_string(&actual_head)?) == current,
            "STALE_FINAL_HEAD",
        )?;
        need(
            matches!(
                at(&lineage, "wall_seconds")?,
                Value::Int(_) | Value::Float(_)
            ) && at(&lineage, "wall_seconds")?
                .as_f64()
                .is_some_and(|wall| (0.0..=1800.0).contains(&wall)),
            "CONTROLLER_WALL_BUDGET",
        )?;
        for index in 0..self.records.len() {
            self.check_time()?;
            let record = &self.records[index];
            need(
                &identity(text(at(record, "path")?)?, 4 << 30)? == record,
                "ARTIFACT_CHANGED_DURING_VERIFICATION",
            )?;
        }
        self.check_time()?;
        let mut result = Dict::new()
            .with("schema", "rusteda.antenna-v3-independent-lineage-review.v1")
            .with("status", "metadata_lineage_verified_requires_full_signoff")
            .with("evidence_kind", self.cfg("evidence_kind")?)
            .with("lineage", cfg_lineage);
        if let Some(manifest) = self.cfg.get("source_manifest") {
            result.insert("source_manifest", manifest);
        }
        Ok(result
            .with("tool", tool)
            .with("workspace", workspace_text)
            .with("initial_revision", initial.as_str())
            .with("final_revision", current.as_str())
            .with("initial", at(&lineage, "initial")?)
            .with("final", last_record)
            .with("execution_mode", "single_native_process")
            .with("actual_native_processes", 1)
            .with("logical_operations", operations.len())
            .with("native_plan", native_plan)
            .with("native_summary", summary_id)
            .with("controller_inputs", controller_id)
            .with("process_report_bytes", count)
            .with("report_bytes", report_bytes)
            .with("workspace_head", at(&lineage, "workspace_head")?)
            .with("initial_inventory", at(&lineage, "initial_inventory")?)
            .with("final_inventory", at(&lineage, "final_inventory")?)
            .with("policy", policy())
            .with("source_lefs", &last.sources)
            .with("routed_named_nets", covered.len())
            .with("final_group_count", final_groups.len())
            .with(
                "native_analysis_status",
                at(&lineage, "native_analysis_status")?,
            )
            .with("observations", observed)
            .with("macro_touch_nets", macro_touch)
            .with("parsed_group_reports", self.report_summaries.len())
            .with("checked_file_ids", self.records.len())
            .with("reported_work", self.reported_work)
            .with(
                "unreported_validation_work_upper_bound",
                self.validation_upper_bound,
            )
            .with("work_budget_charge", self.work_budget_charge)
            .with("whole_layout_antenna_status", "unchecked")
            .with(
                "engine_qualification",
                "unverified_by_this_metadata_verifier",
            )
            .with(
                "manufacturing_DRC_device_LVS_TTSS_dual_source_parity",
                "unverified_by_this_metadata_verifier",
            )
            .with("benchmark", "not_run"))
    }
}

fn net_name(names: &[Value], index: i128) -> R<String> {
    names
        .get(index as usize)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Rejected("MODEL_NET_INDEX".into()))
}

/// `[A-Za-z0-9_-]{1,64}`.
fn revision_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Python `str.strip()` for the ASCII whitespace a HEAD file can hold.
fn strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

fn apply_address_space_limit() -> R<()> {
    let limit = libc::rlimit {
        rlim_cur: ADDRESS_SPACE,
        rlim_max: ADDRESS_SPACE,
    };
    // SAFETY: `setrlimit` reads the plain-data struct it is given.
    #[allow(unsafe_code)]
    let status = unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) };
    need(status == 0, "ADDRESS_SPACE_LIMIT")
}

fn peak_rss_kib() -> i64 {
    // SAFETY: `getrusage` only writes the plain-data struct it is given.
    #[allow(unsafe_code)]
    unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0;
        }
        usage.ru_maxrss
    }
}

fn write_result(out: &Path, value: &Value) -> R<()> {
    let mut text =
        dumps(value, Layout::Indent(2)).ok_or_else(|| Rejected("NONFINITE_JSON_NUMBER".into()))?;
    text.push('\n');
    std::fs::write(out.join("result.json"), text)?;
    Ok(())
}

/// Review the lineage named by `inputs` into the fresh directory `out_dir`.
///
/// On success `out_dir` holds `result.json` (the review, with the reviewing
/// executable, the input manifest and the resources) and `HEAD`, and the
/// one-line form of the review is returned. On rejection `result.json` says
/// `rejected_or_unverified` with the rejection code, and the code is returned
/// as the error.
///
/// # Errors
/// `REVISION_CONFLICT` for a used output directory; otherwise the code of the
/// first failed check or a description of the malformed record.
pub fn run(inputs: &Path, out_dir: &Path) -> Result<String, Rejected> {
    apply_address_space_limit()?;
    std::fs::create_dir_all(out_dir)?;
    let out = std::fs::canonicalize(out_dir)?;
    let lock = OpenOptions::new()
        .append(true)
        .create(true)
        .open(out.join("workspace.lock"))?;
    need(
        fs2::FileExt::try_lock_exclusive(&lock).is_ok(),
        "REVISION_CONFLICT",
    )?;
    for entry in std::fs::read_dir(&out)? {
        need(entry?.file_name() == "workspace.lock", "REVISION_CONFLICT")?;
    }
    let reviewed = read(inputs, 1 << 20).and_then(|config| {
        let config = object(&config)?.clone();
        Verifier::new(&config)?.run()
    });
    let mut result = match reviewed {
        Ok(result) => result,
        Err(error) => {
            write_result(
                &out,
                &Value::Dict(
                    Dict::new()
                        .with("status", "rejected_or_unverified")
                        .with("error", error.0.as_str())
                        .with("whole_layout_antenna_status", "unchecked")
                        .with("benchmark", "not_run"),
                ),
            )?;
            return Err(error);
        }
    };
    let executable = std::env::current_exe()?;
    result.insert(
        "verifier_source",
        identity(&executable.to_string_lossy(), 4 << 30)?,
    );
    result.insert(
        "input_manifest",
        identity(&inputs.to_string_lossy(), 1 << 20)?,
    );
    result.insert(
        "resources",
        Dict::new()
            .with("address_space_limit_bytes", ADDRESS_SPACE)
            .with("peak_rss_kib_self", peak_rss_kib()),
    );
    let result = Value::Dict(result);
    write_result(&out, &result)?;
    std::fs::write(out.join("HEAD"), "rev1\n")?;
    dumps(&result, Layout::Line).ok_or_else(|| Rejected("NONFINITE_JSON_NUMBER".into()))
}
