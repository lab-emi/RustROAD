//! The lineage review against the Python reference verifier on one recorded
//! session (`crates/rustroad-flow/tests/fixtures/steps/antenna`, written by
//! `generate.py`): the accepted review field for field, and the rejection
//! code of every recorded mutation. The session tree is rebuilt here from the
//! files the Python controller wrote; this crate and its tests use no code of
//! the crate that writes lineages.
use rustroad_pyjson::{Dict, FileId, Layout, Value, dumps, parse};
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rustroad-flow/tests/fixtures/steps/antenna")
}

fn text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Root {
    /// The session as the Python controller left it, at a private root.
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustroad-review-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let root = Self(std::fs::canonicalize(path).unwrap());
        copy_tree(&fixtures().join("tree"), &root.0);
        let session = root.0.join("session");
        std::fs::create_dir(&session).unwrap();
        let stored = fixtures().join("expected/session");
        let plan = session.join("native-plan.json");
        std::fs::write(&plan, root.fill(&text(&stored.join("native-plan.json")))).unwrap();
        // The recorded engine session (a stand-in that replays its reports).
        let engine = std::process::Command::new(root.0.join("engine/rustroad-standin.sh"))
            .arg("--workspace")
            .arg(root.0.join("ws"))
            .args(["--threads", "2", "--max-expansions", "8000000"])
            .args(["repair-antenna-groups", "--base-rev", "rev4", "--plan"])
            .arg(&plan)
            .output()
            .unwrap();
        assert!(engine.status.success(), "{:?}", engine);
        std::fs::write(session.join("native-session.json"), &engine.stdout).unwrap();
        std::fs::write(session.join("native-session.stderr"), &engine.stderr).unwrap();
        let mut pending = vec![PathBuf::new()];
        let mut names = Vec::new();
        while let Some(folder) = pending.pop() {
            for entry in std::fs::read_dir(stored.join(&folder)).unwrap() {
                let entry = entry.unwrap();
                let relative = folder.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    pending.push(relative);
                } else {
                    names.push(relative);
                }
            }
        }
        // The lineage pins every other file: it is written last.
        names.sort_by_key(|name| name.ends_with("lineage.json"));
        for relative in names {
            let (from, to) = (stored.join(&relative), session.join(&relative));
            if relative.ends_with("native-plan.json") || relative.ends_with("native-session.stderr")
            {
                continue;
            }
            if relative.extension().is_some_and(|e| e == "json") {
                std::fs::write(to, root.fill(&text(&from))).unwrap();
            } else {
                std::fs::copy(from, to).unwrap();
            }
        }
        for name in ["antenna-inputs.json", "source-manifest.json"] {
            std::fs::write(root.0.join(name), root.fill(&text(&fixtures().join(name)))).unwrap();
        }
        root
    }

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

    /// Write the verifier inputs (pinning the lineage as it is now) and review.
    fn review(
        &self,
        schema_v2: bool,
    ) -> (Result<String, rustroad_antenna_verify::Rejected>, PathBuf) {
        let mut inputs = self.fill(&text(&fixtures().join("verify-inputs-v1.json")));
        if schema_v2 {
            let mut value = parse(&inputs).unwrap();
            let Value::Dict(dict) = &mut value else {
                panic!("inputs object")
            };
            dict.remove("source_manifest").unwrap();
            dict.insert(
                "schema",
                "rusteda.antenna-v3-independent-verifier-inputs.v2",
            );
            inputs = dumps(&value, Layout::Indent(2)).unwrap() + "\n";
        }
        let path = self.0.join("verify-inputs.json");
        std::fs::write(&path, inputs).unwrap();
        let out = self.0.join(if schema_v2 { "review-v2" } else { "review" });
        (rustroad_antenna_verify::run(&path, &out), out)
    }
}

fn without_run_records(result: &str) -> (Dict, String) {
    let Value::Dict(mut dict) = parse(result).unwrap() else {
        panic!("review object")
    };
    let mut removed = Dict::new();
    for key in ["verifier_source", "input_manifest", "resources"] {
        removed.insert(key, dict.remove(key).unwrap());
    }
    (
        removed,
        dumps(&Value::Dict(dict), Layout::Indent(2)).unwrap() + "\n",
    )
}

#[test]
fn accepted_review_equals_the_reference_verifier() {
    let root = Root::new("ok");
    let (review, out) = root.review(false);
    let line = review.unwrap();
    let stored = text(&out.join("result.json"));
    assert_eq!(
        dumps(&parse(&line).unwrap(), Layout::Indent(2)).unwrap() + "\n",
        stored
    );
    assert_eq!(dumps(&parse(&stored).unwrap(), Layout::Line).unwrap(), line);
    let (records, review) = without_run_records(&stored);
    assert_eq!(
        review,
        root.fill(&text(&fixtures().join("expected/verify-result-v1.json")))
    );
    // The three records the reference writes about its own run.
    let resources = records.get("resources").unwrap();
    assert_eq!(
        resources.get("address_space_limit_bytes"),
        Some(&Value::Int(2 << 30))
    );
    assert!(
        resources
            .get("peak_rss_kib_self")
            .unwrap()
            .as_int()
            .unwrap()
            > 0
    );
    let input = FileId::from_value(records.get("input_manifest").unwrap()).unwrap();
    assert_eq!(
        input.path,
        root.0.join("verify-inputs.json").to_str().unwrap()
    );
    assert!(FileId::from_value(records.get("verifier_source").unwrap()).is_some());
    assert_eq!(text(&out.join("HEAD")), "rev1\n");
    // One review per directory.
    let again = rustroad_antenna_verify::run(&root.0.join("verify-inputs.json"), &out).unwrap_err();
    assert_eq!(again.0, "REVISION_CONFLICT");

    // Without a source manifest (v2 inputs): the same review minus that record
    // and the two file identities it added (the manifest and its one source).
    let (review, out) = root.review(true);
    review.unwrap();
    let Value::Dict(v2) = parse(&text(&out.join("result.json"))).unwrap() else {
        panic!("review object")
    };
    let Value::Dict(mut v1) = parse(&stored).unwrap() else {
        panic!("review object")
    };
    assert!(!v2.contains_key("source_manifest"));
    let count = |review: &Dict| review.get("checked_file_ids").unwrap().as_int().unwrap();
    assert_eq!(count(&v2) + 2, count(&v1));
    v1.remove("source_manifest");
    for key in ["checked_file_ids", "input_manifest", "resources"] {
        v1.insert(key, v2.get(key).unwrap());
    }
    assert_eq!(
        dumps(&Value::Dict(v1), Layout::Indent(2)),
        dumps(&Value::Dict(v2), Layout::Indent(2))
    );
}

#[test]
fn every_recorded_mutation_is_rejected_with_the_reference_code() {
    let cases = parse(&text(&fixtures().join("expected/rejections.json"))).unwrap();
    let cases = cases.as_list().unwrap();
    assert!(cases.len() >= 12);
    for case in cases {
        let field = |name: &str| case.get(name).and_then(Value::as_str);
        let name = field("name").unwrap();
        let root = Root::new(name);
        let path = root.0.join(field("file").unwrap());
        match field("old") {
            Some(old) => {
                let before = text(&path);
                assert!(before.contains(old), "{name}");
                std::fs::write(&path, before.replacen(old, field("new").unwrap(), 1)).unwrap();
            }
            None => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, field("new").unwrap()).unwrap();
            }
        }
        let (review, out) = root.review(false);
        assert_eq!(review.unwrap_err().0, field("error").unwrap(), "{name}");
        let record = parse(&text(&out.join("result.json"))).unwrap();
        assert_eq!(
            record.get("status").and_then(Value::as_str),
            Some("rejected_or_unverified"),
            "{name}"
        );
        assert_eq!(
            record.get("error").and_then(Value::as_str),
            field("error"),
            "{name}"
        );
        assert!(!out.join("HEAD").exists(), "{name}");
    }
}

#[test]
fn malformed_inputs_are_rejected_before_any_file_is_trusted() {
    let root = Root::new("inputs");
    let inputs = root.0.join("verify-inputs.json");
    for (text, code) in [
        ("{\"schema\": \"x\"}", "INPUT_SCHEMA"),
        ("{\"a\": 1, \"a\": 2}", "DUPLICATE_JSON_KEY"),
        ("[NaN]", "NONFINITE_JSON_NUMBER"),
    ] {
        std::fs::write(&inputs, text).unwrap();
        let out = root.0.join(format!("review-{code}"));
        let rejected = rustroad_antenna_verify::run(&inputs, &out).unwrap_err();
        assert_eq!(rejected.0, code);
    }
}
