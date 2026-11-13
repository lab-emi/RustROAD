//! The `be_antenna_repair` step: one `repair-antenna-groups` session of the
//! pinned engine, captured, and the lineage records that bind every report
//! of that session.
//!
//! The controller and the engine are two processes of the same executable.
//! The controller validates `rusteda.native-antenna-inputs.v2`, writes the
//! session plan, runs `<tool> --workspace W --threads N --max-expansions
//! 8000000 repair-antenna-groups --base-rev R --plan native-plan.json` as its
//! one child, stores that child's exact argv, stdout and stderr, and then
//! reads the session's reports from disk to write, per check report, the
//! canonical selection and the header (every top-level field except the raw
//! rows), per repair its selection, and `lineage.json`
//! (`rusteda.native-antenna-lineage.v4`): one typed entry per in-process
//! operation with the report's SHA-256, the logical revision chain, and the
//! session's accounting copied from the engine summary. Nothing in the
//! lineage is taken from the engine's memory: it is rebuilt from the files,
//! so the receipt of the child process (argv, exit code, captured bytes) is a
//! real parent-side record. `verify-antenna-lineage` re-checks all of it
//! without sharing any code with this module.
//!
//! Estimate: the child is the engine session (about 0.9 GiB on Ibex); this
//! process holds one report header (at most 4 MiB selected bytes) at a time.
//! All group, work, jumper and report budgets of the pinned policy remain.
use super::text::strip;
use super::{
    FreshDir, StepError, StepResult, dict_of, fail, key, read_bytes, read_json, render, require,
    str_of, write_json_new,
};
use rustroad_pyjson::{Dict, Duplicates, FileId, Layout, Value, list, parse_prefix};
use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CODE: &str = "ANTENNA_SESSION";
const MAX_WORK: i128 = 200_000_000;
const MAX_COMMANDS: usize = 4096;
const MAX_REPORT_BYTES: u64 = 8 << 30;
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

/// The pinned grouped-session policy (`grouping` of the inputs).
fn policy() -> Dict {
    Dict::new()
        .with("group_names", 128)
        .with("max_groups", 1024)
        .with("max_attempts", 64)
        .with("max_total_jumpers", 4096)
        .with("max_commands", 4096)
        .with("max_report_bytes", MAX_REPORT_BYTES)
        .with("max_selected_report_bytes", 128 << 20)
        .with("max_metadata_bytes", 4 << 20)
        .with("max_cumulative_work", 200_000_000_000i64)
}

fn need(ok: bool, message: impl Into<String>) -> StepResult<()> {
    require(ok, CODE, message)
}

fn get<'a>(dict: &'a Dict, name: &str) -> StepResult<&'a Value> {
    key(dict, name, CODE)
}

/// Hash a file and fail if it changed meanwhile (`identity` of the helpers).
fn identity(path: &Path, cap: u64) -> StepResult<Value> {
    use std::os::unix::fs::MetadataExt;
    let stamp = |path: &Path| -> StepResult<(u64, u64, u64, i64, i64)> {
        let m = std::fs::metadata(path)?;
        Ok((m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
    };
    let resolved =
        std::fs::canonicalize(path).map_err(|e| fail(CODE, format!("{}: {e}", path.display())))?;
    let before = stamp(&resolved)?;
    let id = FileId::of(&resolved, cap).map_err(|message| fail(CODE, message))?;
    need(stamp(&resolved)? == before, "file changed while hashing")?;
    Ok(id.to_value())
}

/// `save`: a new file holding `json.dump(value, indent=2)` and a newline.
fn save(path: &Path, value: &Value) -> StepResult<()> {
    write_json_new(path, value, Layout::Indent(2), CODE)
}

fn head(workspace: &Path) -> StepResult<String> {
    let text = String::from_utf8(read_bytes(&workspace.join("HEAD"), 128, CODE)?)
        .map_err(|_| fail(CODE, "HEAD is not UTF-8"))?;
    let value = strip(&text);
    need(
        (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "unsafe revision",
    )?;
    Ok(value.to_owned())
}

/// The leading `format_version`/`revision`/`parent_revision`/`stage` fields of
/// a revision's `design.json`; the native formats put them before the IR.
fn revision_header(path: &Path) -> StepResult<Dict> {
    use std::io::Read;
    let mut prefix = Vec::with_capacity(65536);
    std::fs::File::open(path)?
        .take(65536)
        .read_to_end(&mut prefix)?;
    let text =
        std::str::from_utf8(&prefix).map_err(|_| fail(CODE, "revision header is not UTF-8"))?;
    let bytes = text.as_bytes();
    let space = |mut at: usize| {
        while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            at += 1;
        }
        at
    };
    let mut cursor = space(0);
    need(bytes.get(cursor) == Some(&b'{'), "HEADER_OBJECT_REQUIRED")?;
    cursor += 1;
    let selected = ["format_version", "revision", "parent_revision", "stage"];
    let mut result = Dict::new();
    let mut seen = BTreeSet::new();
    for index in 0..128 {
        cursor = space(cursor);
        if index > 0 {
            need(
                bytes.get(cursor) == Some(&b','),
                "HEADER_FIELD_BUDGET_OR_SEPARATOR",
            )?;
            cursor = space(cursor + 1);
        }
        let (name, end) = parse_prefix(text, cursor, Duplicates::LastWins)
            .map_err(|_| fail(CODE, "HEADER_KEY_FORMAT"))?;
        let name = name
            .as_str()
            .filter(|name| seen.insert((*name).to_owned()))
            .ok_or_else(|| fail(CODE, "HEADER_DUPLICATE_OR_INVALID_KEY"))?
            .to_owned();
        cursor = space(end);
        need(bytes.get(cursor) == Some(&b':'), "HEADER_KEY_SEPARATOR")?;
        cursor = space(cursor + 1);
        let (value, end) = parse_prefix(text, cursor, Duplicates::LastWins)
            .map_err(|_| fail(CODE, "HEADER_FIELD_BYTE_BUDGET_OR_FORMAT"))?;
        cursor = end;
        if selected.contains(&name.as_str()) {
            result.insert(name, value);
        }
        if result.len() == selected.len() {
            return Ok(result);
        }
    }
    Err(fail(CODE, "HEADER_FIELD_COUNT_BUDGET"))
}

/// Top-level fields of a large report: selected values are decoded, every
/// other value (the raw row arrays) is only scanned for its extent.
pub(crate) fn fields(
    path: &Path,
    selected: &[&str],
    max_bytes: u64,
    max_field_bytes: usize,
    max_selected_bytes: usize,
) -> StepResult<Dict> {
    let data = read_bytes(path, max_bytes, CODE).map_err(|_| fail(CODE, "REPORT_BYTE_BUDGET"))?;
    let space = |mut at: usize| {
        while matches!(data.get(at), Some(9 | 10 | 13 | 32)) {
            at += 1;
        }
        at
    };
    let mut at = space(0);
    need(data.get(at) == Some(&b'{'), "REPORT_OBJECT_REQUIRED")?;
    at += 1;
    let mut result = Dict::new();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let (mut first, mut captured) = (true, 0usize);
    loop {
        at = space(at);
        if data.get(at) == Some(&b'}') {
            at += 1;
            break;
        }
        if !first {
            need(data.get(at) == Some(&b','), "REPORT_TOPLEVEL_SEPARATOR")?;
            at = space(at + 1);
        }
        first = false;
        need(data.get(at) == Some(&b'"'), "REPORT_TOPLEVEL_KEY")?;
        let start = at;
        at += 1;
        let mut escaped = false;
        loop {
            let byte = *data
                .get(at)
                .filter(|byte| **byte >= 32 && at - start < 8192)
                .ok_or_else(|| fail(CODE, "REPORT_KEY_BUDGET_OR_FORMAT"))?;
            at += 1;
            if !escaped && byte == b'"' {
                break;
            }
            escaped = !escaped && byte == b'\\';
        }
        let name = std::str::from_utf8(&data[start..at])
            .ok()
            .and_then(|raw| rustroad_pyjson::parse(raw).ok())
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| fail(CODE, "REPORT_KEY_BUDGET_OR_FORMAT"))?;
        need(
            keys.len() < 128 && keys.insert(name.clone()),
            "REPORT_DUPLICATE_KEY_OR_BUDGET",
        )?;
        at = space(at);
        need(data.get(at) == Some(&b':'), "REPORT_KEY_VALUE_SEPARATOR")?;
        at = space(at + 1);
        need(
            !matches!(data.get(at), None | Some(b',' | b'}')),
            "REPORT_MISSING_VALUE",
        )?;
        // One value: up to the next top-level `,` or `}` outside strings and brackets.
        let begin = at;
        let mut stack: Vec<u8> = Vec::new();
        let (mut quoted, mut escaped) = (false, false);
        loop {
            let byte = *data.get(at).ok_or_else(|| fail(CODE, "REPORT_TRUNCATED"))?;
            if quoted {
                need(byte >= 32, "REPORT_STRING_CONTROL")?;
                if escaped {
                    escaped = false;
                } else if byte == b'"' {
                    quoted = false;
                } else if byte == b'\\' {
                    escaped = true;
                }
            } else {
                match byte {
                    b',' | b'}' if stack.is_empty() => break,
                    b'"' => quoted = true,
                    b'[' | b'{' => {
                        stack.push(byte);
                        need(stack.len() <= 64, "REPORT_DEPTH_BUDGET")?;
                    }
                    b']' | b'}' => need(
                        stack.pop() == Some(if byte == b']' { b'[' } else { b'{' }),
                        "REPORT_UNBALANCED_VALUE",
                    )?,
                    _ => {}
                }
            }
            at += 1;
        }
        if selected.contains(&name.as_str()) {
            let raw = &data[begin..at];
            need(raw.len() <= max_field_bytes, "REPORT_FIELD_BYTE_BUDGET")?;
            captured += raw.len();
            need(
                captured <= max_selected_bytes,
                "REPORT_SELECTED_BYTE_BUDGET",
            )?;
            let value = std::str::from_utf8(raw)
                .ok()
                .and_then(|raw| rustroad_pyjson::parse(raw).ok())
                .ok_or_else(|| fail(CODE, format!("report field '{name}' is not JSON")))?;
            result.insert(name, value);
        }
    }
    need(space(at) == data.len(), "REPORT_TRAILING_DATA")?;
    need(
        selected.iter().all(|name| keys.contains(*name)),
        "REPORT_REQUIRED_FIELD_MISSING",
    )?;
    Ok(result)
}

struct Session {
    config: Dict,
    output: PathBuf,
    design: PathBuf,
    tool: Value,
    initial: Value,
    started: Instant,
    commands: Vec<Value>,
    /// `(path, stat)` -> identity of an immutable revision design file.
    design_ids: HashMap<String, Value>,
}

impl Session {
    fn open(config: Value, output: &Path) -> StepResult<Self> {
        let config = dict_of(&config, CODE, "inputs")?.clone();
        need(
            config.has_exactly(&[
                "schema",
                "design_workspace",
                "initial_revision",
                "tool",
                "limits",
                "grouping",
            ]) && config.get("schema").and_then(Value::as_str)
                == Some("rusteda.native-antenna-inputs.v2")
                && config.get("grouping") == Some(&Value::Dict(policy())),
            "native grouped antenna input policy differs",
        )?;
        let design = std::fs::canonicalize(str_of(
            get(&config, "design_workspace")?,
            CODE,
            "design_workspace",
        )?)?;
        let tool = get(&config, "tool")?.clone();
        let tool_path = tool
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(CODE, "native tool identity differs"))?;
        need(
            identity(Path::new(tool_path), 4 << 30)? == tool,
            "native tool identity differs",
        )?;
        let limits = dict_of(get(&config, "limits")?, CODE, "limits")?;
        need(
            limits.has_exactly(&[
                "threads",
                "max_rounds",
                "max_selected_nets",
                "max_work",
                "max_expansions",
                "deadline_seconds",
            ]),
            "antenna limits",
        )?;
        let whole = |name: &str| limits.get(name).and_then(Value::as_int);
        need(
            matches!(whole("threads"), Some(2 | 4 | 16))
                && matches!(whole("max_rounds"), Some(1..=3))
                && limits.get("max_selected_nets") == Some(&Value::Int(100_000))
                && limits.get("max_work") == Some(&Value::Int(MAX_WORK))
                && limits.get("max_expansions") == Some(&Value::Int(8_000_000))
                && limits.get("deadline_seconds") == Some(&Value::Int(1800)),
            "antenna work/memory bounds",
        )?;
        let initial_revision =
            str_of(get(&config, "initial_revision")?, CODE, "initial_revision")?.to_owned();
        need(head(&design)? == initial_revision, "REVISION_CONFLICT")?;
        let mut session = Self {
            output: output.to_owned(),
            design,
            tool,
            initial: Value::Null,
            started: Instant::now(),
            commands: Vec::new(),
            design_ids: HashMap::new(),
            config,
        };
        session.initial = Value::Dict(session.metadata(&initial_revision)?);
        session.started = Instant::now();
        Ok(session)
    }

    fn limit(&self, name: &str) -> StepResult<&Value> {
        get(dict_of(get(&self.config, "limits")?, CODE, "limits")?, name)
    }

    /// `{revision, parent_revision, design_file}` of a materialized revision.
    fn metadata(&mut self, revision: &str) -> StepResult<Dict> {
        use std::os::unix::fs::MetadataExt;
        let path = self
            .design
            .join("revisions")
            .join(revision)
            .join("design.json");
        let record = revision_header(&path)?;
        need(
            matches!(
                record.get("format_version").and_then(Value::as_int),
                Some(4..=6)
            ) && record.get("revision").and_then(Value::as_str) == Some(revision),
            "native revision header differs",
        )?;
        // Revision files are immutable once committed: hash each once per
        // session and re-stat it on every later use.
        let stat = std::fs::metadata(&path)?;
        let stamp = format!(
            "{}:{}:{}:{}:{}:{}",
            path.display(),
            stat.dev(),
            stat.ino(),
            stat.len(),
            stat.mtime(),
            stat.mtime_nsec()
        );
        let design_file = match self.design_ids.get(&stamp) {
            Some(known) => known.clone(),
            None => {
                let id = identity(&path, 512 << 20)?;
                self.design_ids.insert(stamp, id.clone());
                id
            }
        };
        Ok(Dict::new()
            .with("revision", revision)
            .with("parent_revision", get(&record, "parent_revision")?)
            .with("design_file", design_file))
    }

    /// Run the one engine child with its stdout and stderr captured to files.
    fn command(&mut self, name: &str, args: &[String], cap: u64) -> StepResult<PathBuf> {
        let output = self.output.join(format!("{name}.json"));
        let error = self.output.join(format!("{name}.stderr"));
        let tool_path = self
            .tool
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(CODE, "native tool identity differs"))?
            .to_owned();
        let threads = self
            .limit("threads")?
            .as_int()
            .ok_or_else(|| fail(CODE, "antenna limits"))?;
        let mut argv = vec![
            tool_path,
            "--workspace".into(),
            self.design.display().to_string(),
            "--threads".into(),
            threads.to_string(),
            "--max-expansions".into(),
            "8000000".into(),
        ];
        argv.extend(args.iter().cloned());
        save(
            &self.output.join(format!("{name}-command.json")),
            &list(argv.iter()),
        )?;
        let create = |path: &Path| OpenOptions::new().write(true).create_new(true).open(path);
        let tick = Instant::now();
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(create(&output)?)
            .stderr(create(&error)?)
            .spawn()
            .map_err(|e| fail(CODE, format!("cannot start {}: {e}", argv[0])))?;
        let size = |path: &Path| std::fs::metadata(path).map_or(0, |m| m.len());
        let mut pause = Duration::from_micros(200);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let failure =
                if tick.elapsed().as_secs() > 1800 || self.started.elapsed().as_secs() > 1800 {
                    Some("native antenna deadline")
                } else if size(&output) > cap || size(&error) > 16 << 20 {
                    Some("native report byte budget")
                } else {
                    None
                };
            if let Some(message) = failure {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(CODE, message));
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(5));
        };
        let wall = tick.elapsed().as_secs_f64();
        need(
            size(&output) <= cap && size(&error) <= 16 << 20,
            "native report byte budget",
        )?;
        let code = status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            -status.signal().unwrap_or(0)
        });
        let (stdout, stderr) = (identity(&output, 4 << 30)?, identity(&error, 4 << 30)?);
        let count = [&stdout, &stderr]
            .iter()
            .filter_map(|id| id.get("bytes").and_then(Value::as_int))
            .sum::<i128>();
        self.commands.push(Value::Dict(
            Dict::new()
                .with("command", list(argv.iter()))
                .with("exit_code", code)
                .with("stdout", stdout)
                .with("stderr", stderr)
                .with("wall_seconds", wall)
                .with("output_bytes", count),
        ));
        need(code == 0, format!("native antenna command failed: {name}"))?;
        Ok(output)
    }

    #[allow(clippy::too_many_lines)]
    fn run(&mut self) -> StepResult<Dict> {
        let initial_revision = str_of(
            get(&self.config, "initial_revision")?,
            CODE,
            "initial_revision",
        )?
        .to_owned();
        let mut current = initial_revision.clone();
        let plan = self.output.join("native-plan.json");
        let native = self.output.join("native-reports");
        let full = policy();
        let mut plan_policy = Dict::new();
        for name in [
            "group_names",
            "max_groups",
            "max_attempts",
            "max_total_jumpers",
            "max_commands",
            "max_report_bytes",
            "max_cumulative_work",
        ] {
            plan_policy.insert(name, get(&full, name)?);
        }
        save(
            &plan,
            &Value::Dict(
                Dict::new()
                    .with("schema", "rustroad.antenna-group-repair.v1")
                    .with("output", native.display().to_string())
                    .with("max_rounds", self.limit("max_rounds")?)
                    .with("deadline_seconds", self.limit("deadline_seconds")?)
                    .with("policy", plan_policy),
            ),
        )?;
        let stdout = self.command(
            "native-session",
            &[
                "repair-antenna-groups".into(),
                "--base-rev".into(),
                current.clone(),
                "--plan".into(),
                plan.display().to_string(),
            ],
            16 << 20,
        )?;
        let summary = read_json(&stdout, 16 << 20, CODE)?;
        let summary = dict_of(&summary, CODE, "native summary")?;
        let schema = str_of(get(summary, "schema")?, CODE, "schema")?;
        let summary_id = identity(&native.join("summary.json"), 4 << 30)?;
        need(
            matches!(
                schema,
                "rustroad.antenna-group-repair-result.v1"
                    | "rustroad.antenna-group-repair-result.v2"
            ) && identity(&stdout, 4 << 30)?.get("sha256") == summary_id.get("sha256"),
            "native stdout/summary binding differs",
        )?;
        let final_revision =
            str_of(get(summary, "final_revision")?, CODE, "final_revision")?.to_owned();
        need(
            get(summary, "initial_revision")?.as_str() == Some(&current)
                && final_revision == head(&self.design)?,
            "native session actual revision differs",
        )?;
        let logical = schema.ends_with(".v2");
        let mut materialized = BTreeSet::new();
        if logical {
            let committed = get(summary, "committed_revision")?;
            let logical_revisions = get(summary, "logical_revisions")?.as_int();
            need(
                matches!(
                    get(summary, "check_rows")?.as_str(),
                    Some("violated" | "all")
                ) && logical_revisions.is_some()
                    && (committed.is_null() || committed.as_str() == Some(&final_revision))
                    && committed.is_null() == (final_revision == current)
                    && committed.is_null() == (logical_revisions == Some(0)),
                "native logical revision summary differs",
            )?;
            materialized.insert(current.clone());
            materialized.insert(final_revision.clone());
        }
        let mut operations: Vec<Value> = Vec::new();
        let mut report_bytes = 0u64;
        self.operation(
            &native,
            &mut operations,
            &mut report_bytes,
            "inventory-initial",
            "antenna-inventory",
            &current,
            [Value::Null, Value::Null, Value::Null],
        )?;
        self.operation(
            &native,
            &mut operations,
            &mut report_bytes,
            "model-initial",
            "export-connectivity",
            &current,
            [Value::Null, Value::Null, Value::Null],
        )?;
        let (initial_inventory, initial_model) = (
            operations[0].get("report").cloned().unwrap_or(Value::Null),
            operations[1].get("report").cloned().unwrap_or(Value::Null),
        );
        let native_attempts = get(summary, "attempts")?
            .as_list()
            .ok_or_else(|| fail(CODE, "native attempt identity/count"))?;
        let integer = |row: &Value, name: &str| -> StepResult<i128> {
            row.get(name)
                .and_then(Value::as_int)
                .ok_or_else(|| fail(CODE, "native attempt identity/count"))
        };
        let mut by_group: HashMap<(i128, i128), usize> = HashMap::new();
        for (number, row) in native_attempts.iter().enumerate() {
            by_group.insert((integer(row, "round")?, integer(row, "group")?), number);
        }
        need(
            by_group.len() == native_attempts.len() && native_attempts.len() <= 64,
            "native attempt identity/count",
        )?;
        let mut planning: Vec<(i128, i128)> = Vec::new();
        for (scanned, entry) in std::fs::read_dir(&native)?.enumerate() {
            need(
                scanned < MAX_COMMANDS + 1,
                "native raw artifact count budget",
            )?;
            let name = entry?.file_name();
            let parsed = name
                .to_str()
                .and_then(|name| name.strip_prefix("round-"))
                .and_then(|rest| rest.strip_suffix(".json"))
                .and_then(|rest| rest.split_once("-group-"))
                .filter(|(a, b)| super::text::is_digits(a) && super::text::is_digits(b))
                .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
            planning.extend(parsed);
        }
        let max_rounds = self.limit("max_rounds")?.as_int().unwrap_or(0);
        need(
            planning.len() as i128 <= 1024 * max_rounds,
            "native planning operation budget",
        )?;
        planning.sort_unstable();
        let mut attempts: Vec<Value> = Vec::new();
        let mut rounds: Vec<Value> = Vec::new();
        let mut parents: HashMap<String, String> = HashMap::new();
        for (iteration, index) in planning {
            let planning_record = self.check(
                &native,
                &mut operations,
                &mut report_bytes,
                &format!("round-{iteration}-group-{index}"),
                &current,
            )?;
            let Some(number) = by_group.get(&(iteration, index)).copied() else {
                continue;
            };
            let row = &native_attempts[number];
            need(
                number == attempts.len()
                    && row.get("base_revision").and_then(Value::as_str) == Some(&current),
                "native attempt order/base differs",
            )?;
            let before = self.check(
                &native,
                &mut operations,
                &mut report_bytes,
                &format!("repair-{number}-before"),
                &current,
            )?;
            let selection = native.join(format!("repair-{number}-selection.json"));
            save(
                &selection,
                row.get("selected_nets")
                    .ok_or_else(|| fail(CODE, "native attempt without selected_nets"))?,
            )?;
            let repair = fields(
                &native.join(format!("repair-{number}.json")),
                &REPAIR_FIELDS,
                128 << 20,
                4 << 20,
                8 << 20,
            )?;
            let selection_id = identity(&selection, 4 << 30)?;
            let record = self.operation(
                &native,
                &mut operations,
                &mut report_bytes,
                &format!("repair-{number}"),
                "repair-antenna",
                &current,
                [
                    selection_id.clone(),
                    get(&before, "report")?.clone(),
                    Value::Str(current.clone()),
                ],
            )?;
            let child = row
                .get("revision")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(CODE, "native attempt without a revision"))?
                .to_owned();
            if child != current {
                parents.insert(child.clone(), current.clone());
            }
            let costs = dict_of(get(&repair, "stats")?, CODE, "repair stats")?;
            let cost = |name: &str| -> StepResult<i128> {
                get(costs, name)?
                    .as_int()
                    .ok_or_else(|| fail(CODE, "repair work is not an integer"))
            };
            let (antenna_work, planner_work) = (cost("antenna_work")?, cost("planner_work")?);
            let child_record =
                self.revision_record(logical, &materialized, &child, parents.get(&child))?;
            let attempt = Value::Dict(
                Dict::new()
                    .with("round", iteration)
                    .with("group", index)
                    .with("base_revision", current.as_str())
                    .with("revision", child.as_str())
                    .with("planning", planning_record)
                    .with("before", before)
                    .with("selected", selection_id)
                    .with("repair_report", record)
                    .with("child", child_record)
                    .with(
                        "work_accounting",
                        Dict::new()
                            .with("reported_antenna_work", antenna_work)
                            .with("reported_planner_work", planner_work)
                            .with("unreported_validation_work_upper_bound", MAX_WORK)
                            .with("budget_charge", antenna_work + planner_work + MAX_WORK),
                    ),
            );
            if child != current {
                rounds.push(attempt.clone());
            }
            attempts.push(attempt);
            current = child;
        }
        need(
            current == final_revision && attempts.len() == native_attempts.len(),
            "native final attempt chain differs",
        )?;
        let final_inventory = self.operation(
            &native,
            &mut operations,
            &mut report_bytes,
            "inventory-final",
            "antenna-inventory",
            &final_revision,
            [Value::Null, Value::Null, Value::Null],
        )?;
        let final_model = self.operation(
            &native,
            &mut operations,
            &mut report_bytes,
            "model-final",
            "export-connectivity",
            &final_revision,
            [Value::Null, Value::Null, Value::Null],
        )?;
        let groups = get(summary, "groups")?
            .as_int()
            .ok_or_else(|| fail(CODE, "native summary group count"))?;
        let mut final_groups = Vec::new();
        for index in 0..groups {
            let record = self.check(
                &native,
                &mut operations,
                &mut report_bytes,
                &format!("final-group-{index}"),
                &final_revision,
            )?;
            let mut row = Dict::new()
                .with("group", index)
                .with("revision", final_revision.as_str());
            for (name, value) in record.iter() {
                row.insert(name, value);
            }
            final_groups.push(Value::Dict(row));
        }
        need(
            get(summary, "report_bytes")? == &Value::Int(i128::from(report_bytes)),
            "native logical report byte ledger differs",
        )?;
        let tool_path = self
            .tool
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        need(
            identity(Path::new(&tool_path), 4 << 30)? == self.tool,
            "native tool changed",
        )?;
        let snapshot = self.output.join("final-head.txt");
        std::fs::write(&snapshot, format!("{final_revision}\n"))?;
        let final_metadata = Value::Dict(self.metadata(&final_revision)?);
        let process_report_bytes: i128 = self
            .commands
            .iter()
            .filter_map(|row| row.get("output_bytes").and_then(Value::as_int))
            .sum();
        let copy = |name: &str| get(summary, name).cloned();
        let mut result = Dict::new()
            .with(
                "schema",
                if logical {
                    "rusteda.native-antenna-lineage.v4"
                } else {
                    "rusteda.native-antenna-lineage.v3"
                },
            )
            .with("status", copy("status")?)
            .with("scope", copy("scope")?)
            .with("route_scope", copy("route_scope")?)
            .with("workspace", self.design.display().to_string())
            .with("tool", &self.tool)
            .with("initial_revision", initial_revision.as_str())
            .with("final_revision", final_revision.as_str())
            .with("initial", &self.initial)
            .with("final", final_metadata)
            .with("workspace_head", identity(&snapshot, 4 << 30)?)
            .with("initial_inventory", initial_inventory)
            .with("final_inventory", final_inventory)
            .with("initial_model", initial_model)
            .with("final_model", final_model)
            .with("policy", policy())
            .with("rounds", Value::List(rounds))
            .with("attempts", Value::List(attempts))
            .with("final_groups", Value::List(final_groups))
            .with("inventory_status", copy("inventory_status")?)
            .with("native_analysis_status", copy("native_analysis_status")?)
            .with("observations", copy("observations")?)
            .with("excluded_macro_touch", copy("excluded_macro_touch")?)
            .with(
                "unresolved_supported_nets",
                copy("unresolved_supported_nets")?,
            )
            .with("requires_full_revalidation", true)
            .with("whole_layout_antenna_status", "unchecked")
            .with("commands", Value::List(self.commands.clone()))
            .with("operations", Value::List(operations.clone()))
            .with("execution_mode", "single_native_process")
            .with("actual_native_processes", 1)
            .with("logical_operations", operations.len())
            .with("native_plan", identity(&plan, 4 << 30)?)
            .with("native_summary", summary_id)
            .with("process_report_bytes", process_report_bytes)
            .with("report_bytes", report_bytes)
            .with("reported_work", copy("reported_work")?)
            .with(
                "unreported_validation_work_upper_bound",
                copy("unreported_validation_work_upper_bound")?,
            )
            .with("work_budget_charge", copy("work_budget_charge")?)
            .with("total_jumpers", copy("total_jumpers")?)
            .with("stop_reason", copy("stop_reason")?)
            .with("skipped_futile_groups", copy("skipped_futile_groups")?)
            .with("wall_seconds", self.started.elapsed().as_secs_f64());
        if logical {
            let record = self.revision_record(
                logical,
                &materialized,
                &final_revision,
                parents.get(&final_revision),
            )?;
            result.insert("final", record);
            result.insert("logical_revisions", copy("logical_revisions")?);
            result.insert("committed_revision", copy("committed_revision")?);
            result.insert("check_rows", copy("check_rows")?);
            result.insert("materialized_revisions", list(materialized.iter()));
        }
        // Include generated metadata in the disk admission; no child timings are invented.
        let (mut total, mut count) = (0u64, 0usize);
        for directory in [&self.output, &native] {
            for entry in std::fs::read_dir(directory)? {
                let path = entry?.path();
                if path == native {
                    continue;
                }
                let kind = std::fs::symlink_metadata(&path)?;
                need(kind.file_type().is_file(), "native evidence file scope")?;
                count += 1;
                total += kind.len();
                need(
                    count <= 3 * MAX_COMMANDS + 256 && total + (16 << 20) <= MAX_REPORT_BYTES,
                    "native aggregate evidence count/byte budget",
                )?;
            }
        }
        save(
            &self.output.join("lineage.json"),
            &Value::Dict(result.clone()),
        )?;
        Ok(result)
    }

    /// Logical revisions exist only in reports; materialized ones are read.
    fn revision_record(
        &mut self,
        logical: bool,
        materialized: &BTreeSet<String>,
        revision: &str,
        logical_parent: Option<&String>,
    ) -> StepResult<Value> {
        if !logical {
            return Ok(Value::Dict(self.metadata(revision)?));
        }
        if materialized.contains(revision) {
            return Ok(Value::Dict(
                self.metadata(revision)?
                    .with("materialized", true)
                    .with("logical_parent_revision", logical_parent),
            ));
        }
        Ok(Value::Dict(
            Dict::new()
                .with("revision", revision)
                .with("parent_revision", logical_parent)
                .with("materialized", false),
        ))
    }

    /// One typed in-process operation: its report identity joins the ledger.
    #[allow(clippy::too_many_arguments)]
    fn operation(
        &self,
        native: &Path,
        operations: &mut Vec<Value>,
        report_bytes: &mut u64,
        name: &str,
        verb: &str,
        revision: &str,
        [selection, selected_report, base]: [Value; 3],
    ) -> StepResult<Value> {
        let record = identity(&native.join(format!("{name}.json")), 128 << 20)?;
        *report_bytes += record
            .get("bytes")
            .and_then(Value::as_int)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .unwrap_or(0);
        need(
            operations.len() < MAX_COMMANDS && *report_bytes <= MAX_REPORT_BYTES,
            "native logical operation count/byte budget",
        )?;
        operations.push(Value::Dict(
            Dict::new()
                .with("kind", "native_in_process_operation")
                .with("name", name)
                .with("operation", verb)
                .with("revision", revision)
                .with("report", &record)
                .with("selection", selection)
                .with("selected_report", selected_report)
                .with("base_revision", base),
        ));
        Ok(record)
    }

    /// A check report: write its selection and header, record the operation.
    fn check(
        &self,
        native: &Path,
        operations: &mut Vec<Value>,
        report_bytes: &mut u64,
        name: &str,
        revision: &str,
    ) -> StepResult<Dict> {
        let report = fields(
            &native.join(format!("{name}.json")),
            &HEADER,
            128 << 20,
            4 << 20,
            4 << 20,
        )?;
        let names = report
            .get("selection")
            .and_then(|selection| selection.get("canonical_nets"))
            .ok_or_else(|| fail(CODE, "check report without a canonical selection"))?;
        let selection = native.join(format!("{name}-selection.json"));
        let header = native.join(format!("{name}-header.json"));
        save(&selection, names)?;
        save(&header, &Value::Dict(report))?;
        let selection_id = identity(&selection, 4 << 30)?;
        let record = self.operation(
            native,
            operations,
            report_bytes,
            name,
            "check-antenna",
            revision,
            [selection_id, Value::Null, Value::Null],
        )?;
        Ok(Dict::new()
            .with("report", record)
            .with("header", identity(&header, 4 << 30)?)
            .with("selection", identity(&selection, 4 << 30)?))
    }
}

/// Run the step and return its one-line result. A failure after the output
/// directory was claimed leaves `failure.json` beside the partial evidence.
///
/// # Errors
/// `REVISION_CONFLICT` for a used output directory; `ANTENNA_SESSION` for an
/// input, identity, budget or binding failure or a failed engine child.
pub fn run(inputs: &Path, out_dir: &Path) -> StepResult<String> {
    let dir = FreshDir::claim(out_dir)?;
    let outcome = read_json(inputs, 2 << 20, CODE)
        .and_then(|config| Session::open(config, &dir.path))
        .and_then(|mut session| session.run());
    let result = match outcome {
        Ok(result) => result,
        Err(error) => {
            let error = StepError {
                code: if error.message == "REVISION_CONFLICT" {
                    "REVISION_CONFLICT"
                } else if error.code.starts_with("STEP_") {
                    CODE
                } else {
                    error.code
                },
                message: error.message,
            };
            save(
                &dir.path.join("failure.json"),
                &Value::Dict(
                    Dict::new()
                        .with("status", "failed")
                        .with("complete", false)
                        .with("error", error.message.as_str()),
                ),
            )?;
            return Err(error);
        }
    };
    dir.publish()?;
    let mut summary = Dict::new();
    for name in [
        "schema",
        "status",
        "initial_revision",
        "final_revision",
        "inventory_status",
        "native_analysis_status",
        "requires_full_revalidation",
    ] {
        summary.insert(name, get(&result, name)?);
    }
    render(&Value::Dict(summary), Layout::Line, CODE)
}
