//! `antenna-session` against the records the Python controller wrote for the
//! same recorded session (`tests/fixtures/steps/antenna`, written by
//! `generate.py`). The engine is a stand-in that replays recorded reports;
//! nothing here is an EDA result.
use rustroad_flow::steps::antenna_session;
use rustroad_pyjson::{FileId, Layout, Value, dumps, parse};
use std::path::{Path, PathBuf};

const REPORTS: [&str; 12] = [
    "inventory-initial",
    "model-initial",
    "round-0-group-0",
    "repair-0-before",
    "repair-0",
    "round-1-group-0",
    "repair-1-before",
    "repair-1",
    "round-2-group-0",
    "inventory-final",
    "model-final",
    "final-group-0",
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steps/antenna")
}

fn text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustroad-antenna-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let root = Self(std::fs::canonicalize(path).unwrap());
        copy_tree(&fixtures().join("tree"), &root.0);
        root
    }
    /// A stored expectation for this root: the root, the two byte totals that
    /// depend on its length, and the identities of files whose text holds it.
    fn fill(&self, template: &str) -> String {
        let size = |path: PathBuf| std::fs::metadata(path).map_or(0, |m| m.len());
        let reports: u64 = REPORTS
            .iter()
            .map(|name| size(self.0.join(format!("session/native-reports/{name}.json"))))
            .sum();
        let mut out = template
            .replace("@ROOT@", self.0.to_str().unwrap())
            .replace("@REPORT_BYTES@", &reports.to_string())
            .replace(
                "@PROCESS_BYTES@",
                &size(self.0.join("session/native-session.json")).to_string(),
            );
        for (open, quoted) in [("\"@BYTES:", true), ("@SHA:", false)] {
            while let Some(start) = out.find(open) {
                let name_start = start + open.len();
                let end = name_start + out[name_start..].find('@').unwrap();
                let id = FileId::of(&self.0.join(&out[name_start..end]), 1 << 30).unwrap();
                let (value, stop) = if quoted {
                    (id.bytes.to_string(), end + 2)
                } else {
                    (id.sha256, end + 1)
                };
                out.replace_range(start..stop, &value);
            }
        }
        out
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(folder) = pending.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                out.push(entry.path().strip_prefix(root).unwrap().to_owned());
            }
        }
    }
    out.sort();
    out
}

/// The run times of a lineage, the only values that differ between two runs.
fn without_run_times(lineage: &str) -> String {
    let mut value = parse(lineage).unwrap();
    let Value::Dict(dict) = &mut value else {
        panic!("lineage object")
    };
    assert!(matches!(dict.get("wall_seconds"), Some(Value::Float(_))));
    dict.insert("wall_seconds", 0);
    let Some(Value::List(commands)) = dict.get_mut("commands") else {
        panic!("commands")
    };
    let Value::Dict(command) = &mut commands[0] else {
        panic!("command")
    };
    assert!(matches!(command.get("wall_seconds"), Some(Value::Float(_))));
    command.insert("wall_seconds", 0);
    dumps(&value, Layout::Indent(2)).unwrap()
}

#[test]
fn session_records_equal_the_reference_controller() {
    let root = Root::new("session");
    let inputs = root.0.join("antenna-inputs.json");
    std::fs::write(
        &inputs,
        root.fill(&text(&fixtures().join("antenna-inputs.json"))),
    )
    .unwrap();
    let session = root.0.join("session");
    let stdout = antenna_session::run(&inputs, &session).unwrap();
    assert_eq!(
        stdout + "\n",
        root.fill(&text(&fixtures().join("expected/session-stdout.txt")))
    );
    let expected = fixtures().join("expected/session");
    let written = files(&expected);
    for relative in &written {
        let (stored, actual) = (expected.join(relative), session.join(relative));
        if relative.extension().is_some_and(|e| e == "json") {
            let (stored, actual) = (root.fill(&text(&stored)), text(&actual));
            if relative.ends_with("lineage.json") {
                assert_eq!(without_run_times(&actual), without_run_times(&stored));
                // Laid out as CPython lays it out.
                let value = parse(&actual).unwrap();
                assert_eq!(dumps(&value, Layout::Indent(2)).unwrap() + "\n", actual);
            } else {
                assert_eq!(actual, stored, "{}", relative.display());
            }
        } else {
            assert_eq!(
                std::fs::read(&actual).unwrap(),
                std::fs::read(&stored).unwrap(),
                "{}",
                relative.display()
            );
        }
    }
    // Nothing else was written beside the engine's own reports and capture.
    let mut everything = written;
    everything.extend(
        REPORTS
            .iter()
            .chain(&["summary"])
            .map(|name| PathBuf::from(format!("native-reports/{name}.json"))),
    );
    everything.push("native-session.json".into());
    everything.sort();
    assert_eq!(files(&session), everything);
    assert_eq!(
        text(&session.join("native-session.json")),
        text(&session.join("native-reports/summary.json"))
    );
    assert_eq!(text(&root.0.join("ws/HEAD")), "rev6\n");
    // One session per directory.
    let again = antenna_session::run(&inputs, &session).unwrap_err();
    assert_eq!(again.code, "REVISION_CONFLICT");
}

#[test]
fn a_session_that_cannot_be_bound_leaves_a_failure_record() {
    // The workspace is not at the initial revision.
    let root = Root::new("moved");
    let inputs = root.0.join("antenna-inputs.json");
    std::fs::write(
        &inputs,
        root.fill(&text(&fixtures().join("antenna-inputs.json"))),
    )
    .unwrap();
    std::fs::write(root.0.join("ws/HEAD"), "rev3\n").unwrap();
    let failure = antenna_session::run(&inputs, &root.0.join("session")).unwrap_err();
    assert_eq!(failure.to_string(), "REVISION_CONFLICT: REVISION_CONFLICT");
    let record = parse(&text(&root.0.join("session/failure.json"))).unwrap();
    assert_eq!(record.get("status").and_then(Value::as_str), Some("failed"));
    assert!(!root.0.join("session/HEAD").exists());

    // The pinned tool is not the file on disk.
    let root = Root::new("tool");
    let inputs = root.0.join("antenna-inputs.json");
    let changed = root
        .fill(&text(&fixtures().join("antenna-inputs.json")))
        .replace("\"max_rounds\": 3", "\"max_rounds\": 4");
    std::fs::write(&inputs, changed).unwrap();
    let failure = antenna_session::run(&inputs, &root.0.join("session")).unwrap_err();
    assert_eq!(
        failure.to_string(),
        "ANTENNA_SESSION: antenna work/memory bounds"
    );

    // An engine that fails: the receipt is kept, the step fails.
    let root = Root::new("engine");
    let inputs = root.0.join("antenna-inputs.json");
    std::fs::remove_file(root.0.join("engine/summary.json")).unwrap();
    std::fs::write(
        &inputs,
        root.fill(&text(&fixtures().join("antenna-inputs.json"))),
    )
    .unwrap();
    let failure = antenna_session::run(&inputs, &root.0.join("session")).unwrap_err();
    assert_eq!(
        failure.to_string(),
        "ANTENNA_SESSION: native antenna command failed: native-session"
    );
    assert!(root.0.join("session/native-session.stderr").is_file());
    assert!(root.0.join("session/failure.json").is_file());
    assert!(!root.0.join("session/lineage.json").exists());
}
