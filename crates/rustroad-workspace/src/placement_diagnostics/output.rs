//! Bounded observation output. HEAD is a commit marker, never a signoff verdict.
use super::{ByteLedger, FileId, OUTPUT_CAP, front};
use crate::{Error, Metadata, Result};
use fs2::FileExt;
use rustroad_core::place::{DiagnosticPhaseView, DiagnosticResult, PinRef, PlaceProblem};
use serde::Serialize;
use serde::ser::{SerializeSeq, SerializeStruct};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const SMALL_CAP: u64 = 64 << 10;
const OBSERVATION_CAP: u64 = OUTPUT_CAP - (4 << 20);

fn invalid(message: &str) -> Error {
    Error::Invalid(format!("P1_DIAGNOSTIC: {message}"))
}

fn directory(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).read(true).create_new(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    Ok(options.open(path)?)
}

fn seal(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    Ok(())
}

/// Check every existing ancestor before creating anything. No resolve hides a link.
fn new_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(invalid("absolute output path required"));
    }
    let parent = path.parent().ok_or_else(|| invalid("output parent"))?;
    front::plain(parent)?;
    if !fs::metadata(parent)?.is_dir() {
        return Err(invalid("output parent is not a directory"));
    }
    Ok(())
}

pub(super) fn check_fresh(path: &Path, base: &str) -> Result<()> {
    new_path(path)?;
    if base != "empty" {
        crate::revision_number(base)?;
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if base != "empty" {
                return Err(Error::Conflict {
                    expected: base.into(),
                    actual: "empty".into(),
                });
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
        Ok(value) => {
            if value.file_type().is_symlink() || !value.is_dir() {
                return Err(invalid("output alias/type"));
            }
            let head = path.join("HEAD");
            if head.try_exists()? {
                let mut io = ByteLedger::new();
                let id = front::identity(&head, 64, true, &mut io)?;
                let bytes = front::read_small(&id.path, 64, true, &mut io)?;
                if front::identity(&head, 64, true, &mut io)? != id {
                    return Err(invalid("output HEAD changed during read"));
                }
                let actual = std::str::from_utf8(&bytes)
                    .map_err(|_| invalid("output HEAD UTF8"))?
                    .strip_suffix('\n')
                    .ok_or_else(|| invalid("noncanonical output HEAD"))?;
                if crate::revision_number(actual)? == 0 {
                    return Err(invalid("published output HEAD cannot be rev0"));
                }
                if base != actual {
                    return Err(Error::Conflict {
                        expected: base.into(),
                        actual: actual.into(),
                    });
                }
            }
            // Partial or existing workspaces are never adopted or overwritten.
            Err(invalid("OUTPUT_NOT_FRESH"))
        }
    }
}

pub(super) struct OwnedOutput {
    root: PathBuf,
    revision: PathBuf,
    _lock: File,
}
impl OwnedOutput {
    pub(super) fn create(root: &Path, base: &str) -> Result<Self> {
        check_fresh(root, base)?;
        directory(root)?;
        let lock = new_file(&root.join("workspace.lock"))?;
        FileExt::try_lock_exclusive(&lock).map_err(|_| invalid("OUTPUT_LOCK_BUSY"))?;
        directory(&root.join("revisions"))?;
        let revision = root.join("revisions/rev1");
        directory(&revision)?;
        Ok(Self {
            root: root.into(),
            revision,
            _lock: lock,
        })
    }

    pub(super) fn summary(
        &self,
        source: &front::SourceProof,
        result: &DiagnosticResult,
        observation: &FileId,
        released_tail: u64,
        io: &mut ByteLedger,
    ) -> Result<FileId> {
        #[derive(Serialize)]
        struct Summary<'a> {
            schema: &'static str,
            status: &'static str,
            source_revision: &'a str,
            observation: &'a FileId,
            diagnostic: &'a rustroad_core::place::DiagnosticSummary,
            reference_work: u64,
            reference_hpwl_dbu: u64,
            ordinary_selected_position_set_id: &'static str,
            released_io_hold: u64,
            source_unchanged_at_precommit: bool,
            completion_policy: &'static str,
            reference_work_scope: &'static str,
        }
        let reference = &result.reference;
        let summary = Summary {
            schema: "rustroad.p1-stage-summary.v1",
            status: "observed_unverified",
            source_revision: &source.revision,
            observation,
            diagnostic: &result.summary,
            reference_work: reference.work,
            reference_hpwl_dbu: reference.hpwl,
            ordinary_selected_position_set_id: "ordinary_selected",
            released_io_hold: released_tail,
            source_unchanged_at_precommit: true,
            completion_policy: "HEAD is provisional without actual exit0, typed receipt and current source/output identities",
            reference_work_scope: "ordinary selected geometry; work includes diagnostic preparation/capture before selection, excludes later trials",
        };
        let path = self.revision.join("summary.json");
        write_small(&path, &summary, io)?;
        front::identity(&path, SMALL_CAP, true, io)
    }

    pub(super) fn failure(
        &self,
        error: &Error,
        source_error: Option<&Error>,
        io: &mut ByteLedger,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Failure<'a> {
            schema: &'static str,
            status: &'static str,
            error: String,
            source_recheck_error: Option<String>,
            scope: &'a str,
        }
        let failure = Failure {
            schema: "rustroad.p1-stage-failure.v1",
            status: "unverified",
            error: error.to_string().chars().take(4096).collect(),
            source_recheck_error: source_error.map(|e| e.to_string().chars().take(4096).collect()),
            scope: "precommit failure; partial private output retained; charged passes are attempted scope, not exact failed CPU or syscall work",
        };
        write_small(&self.root.join("failure.json"), &failure, io)
    }

    pub(super) fn publish(&self, io: &mut ByteLedger) -> Result<()> {
        // Every variable-size writer has finished. This exact allowlist admits
        // namespace and byte bounds before the first HEAD can become visible.
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.revision)? {
            if names.len() >= 2 {
                return Err(invalid("output revision inventory"));
            }
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("output UTF8"))?;
            if !["observations.json", "summary.json"].contains(&name.as_str()) {
                return Err(invalid("unknown output file"));
            }
            names.push(name);
        }
        if names.len() != 2 {
            return Err(invalid("missing output file"));
        }
        let mut revision_names = 0;
        for entry in fs::read_dir(self.root.join("revisions"))? {
            let entry = entry?;
            revision_names += 1;
            if revision_names > 1 || entry.file_name() != "rev1" || !entry.file_type()?.is_dir() {
                return Err(invalid("output revision namespace"));
            }
        }
        if revision_names != 1 {
            return Err(invalid("output revision namespace"));
        }
        let mut root_names = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            root_names += 1;
            if root_names > 2
                || !["workspace.lock", "revisions"]
                    .iter()
                    .any(|n| entry.file_name() == *n)
            {
                return Err(invalid("output root inventory"));
            }
        }
        if root_names != 2 {
            return Err(invalid("output root inventory"));
        }
        // Hold includes bounded HEAD, summary and subsequent CLI receipt costs.
        // CLI receipt passes are reserved, not misreported as already executed.
        io.admit_remaining(128 << 10)?;
        io.charge(10)?;
        let temporary = self.root.join("HEAD.tmp");
        let mut file = new_file(&temporary)?;
        file.write_all(b"rev1\n")?;
        file.sync_all()?;
        drop(file);
        seal(&temporary)?;
        #[cfg(unix)]
        fs::set_permissions(&self.revision, fs::Permissions::from_mode(0o500))?;
        File::open(&self.root)?.sync_all()?;
        // No data-dependent validation follows. IO error/kill uncertainty at or
        // after rename remains unverified even if a provisional HEAD is visible.
        fs::rename(temporary, self.root.join("HEAD"))?;
        Ok(())
    }
}

struct Sink<'io> {
    output: BufWriter<File>,
    io: &'io mut ByteLedger,
    bytes: u64,
    cap: u64,
}
impl Write for Sink<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(data.len() as u64)
            .filter(|n| *n <= self.cap)
            .ok_or_else(|| std::io::Error::other("diagnostic output byte cap"))?;
        self.io
            .charge(data.len() as u64 * 2)
            .map_err(std::io::Error::other)?;
        self.output.write_all(data)?;
        self.bytes = next;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
impl Sink<'_> {
    fn json(&mut self, value: &impl Serialize) -> Result<()> {
        serde_json::to_writer(self, value)?;
        Ok(())
    }
    fn close(mut self, path: &Path) -> Result<()> {
        self.flush()?;
        self.output.get_ref().sync_all()?;
        drop(self);
        seal(path)
    }
}

fn write_small(path: &Path, value: &impl Serialize, io: &mut ByteLedger) -> Result<()> {
    let mut sink = Sink {
        output: BufWriter::with_capacity(64 << 10, new_file(path)?),
        io,
        bytes: 0,
        cap: SMALL_CAP,
    };
    sink.json(value)?;
    sink.write_all(b"\n")?;
    sink.close(path)
}

pub(super) struct ObservationWriter<'io> {
    sink: Sink<'io>,
    path: PathBuf,
    phases: usize,
    header: bool,
    movables: usize,
    last_stage: Option<(u8, u8)>,
    seen_stages: [bool; 3],
}
impl<'io> ObservationWriter<'io> {
    pub(super) fn create(owner: &OwnedOutput, io: &'io mut ByteLedger) -> Result<Self> {
        let path = owner.revision.join("observations.json");
        let sink = Sink {
            output: BufWriter::with_capacity(64 << 10, new_file(&path)?),
            io,
            bytes: 0,
            cap: OBSERVATION_CAP,
        };
        Ok(Self {
            sink,
            path,
            phases: 0,
            header: false,
            movables: 0,
            last_stage: None,
            seen_stages: [false; 3],
        })
    }
    pub(super) fn header(
        &mut self,
        metadata: &Metadata,
        source: &front::SourceProof,
        input: &crate::physical::AnalyticProblem<'_>,
        initial: &[rustroad_core::Placed],
        effective_io: &[rustroad_core::io::IoPin],
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Header<'a> {
            schema: &'static str,
            scope: &'static str,
            source: &'a front::SourceProof,
            imported_metadata: &'a Metadata,
            initial_packing: &'a [rustroad_core::Placed],
            movable_instance_ids: &'a [usize],
            native_problem: Problem<'a>,
            effective_io_pins: &'a [rustroad_core::io::IoPin],
            ordinary_selected_position_set_id: &'static str,
        }
        if self.header {
            return Err(invalid("duplicate observation header"));
        }
        let value = Header {
            schema: "rustroad.p1-stage-observations.v1",
            scope: "source-bound pre-materialization coordinates; N/FS row orientation and physical terminal geometry remain explicit imported inputs; independent integer observer required",
            source,
            imported_metadata: metadata,
            initial_packing: initial,
            movable_instance_ids: &input.moving,
            native_problem: Problem(&input.problem),
            effective_io_pins: effective_io,
            ordinary_selected_position_set_id: "ordinary_selected",
        };
        self.sink.write_all(b"{\"header\":")?;
        self.sink.json(&value)?;
        self.sink.write_all(b",\"position_sets\":[")?;
        self.header = true;
        self.movables = input.moving.len();
        Ok(())
    }
    pub(super) fn phase(&mut self, view: &DiagnosticPhaseView<'_>) -> Result<()> {
        #[derive(Serialize)]
        struct Phase<'a> {
            position_set_id: String,
            label: &'a str,
            source_stage: Option<u8>,
            kind: rustroad_core::place::DiagnosticPhase,
            movable_positions_dbu: &'a [[i64; 2]],
            native_report: rustroad_core::place::NativeCost,
            cumulative_work: u64,
            counters: &'a rustroad_core::place::ElectrostaticCounters,
        }
        if !self.header || self.phases >= 14 || view.positions.len() != self.movables {
            return Err(invalid("observation phase cap/order"));
        }
        use rustroad_core::place::DiagnosticPhase;
        let position_set_id = match (view.phase, view.stage) {
            (DiagnosticPhase::SourceBaseline, None)
                if self.phases == 0 && view.label == "legacy_baseline" =>
            {
                "source_baseline".into()
            }
            (DiagnosticPhase::DefaultSelected, None)
                if self.phases == 1 && view.label == "default" =>
            {
                "ordinary_selected".into()
            }
            (phase, Some(stage)) if stage < 3 && self.phases >= 2 => {
                let (ordinal, suffix) = match phase {
                    DiagnosticPhase::ProjectedTarget => (1, "projected_target"),
                    DiagnosticPhase::AfterRetarget => (2, "after_retarget"),
                    DiagnosticPhase::AfterLegalize => (3, "after_legalize"),
                    DiagnosticPhase::AfterDetail => (4, "after_detail"),
                    _ => return Err(invalid("invalid stage phase")),
                };
                if view.label != format!("stage{stage}")
                    || match self.last_stage {
                        None => ordinal != 1,
                        Some((previous, 4)) => ordinal != 1 || stage <= previous,
                        Some((previous, last)) => stage != previous || ordinal != last + 1,
                    }
                {
                    return Err(invalid("stage phase identity/order"));
                }
                self.last_stage = Some((stage, ordinal));
                self.seen_stages[usize::from(stage)] = true;
                format!("stage{stage}_{suffix}")
            }
            _ => return Err(invalid("observation phase identity/order")),
        };
        if self.phases != 0 {
            self.sink.write_all(b",")?;
        }
        self.sink.json(&Phase {
            position_set_id,
            label: view.label,
            source_stage: view.stage,
            kind: view.phase,
            movable_positions_dbu: view.positions,
            native_report: view.native_report,
            cumulative_work: view.cumulative_work,
            counters: view.counters,
        })?;
        self.phases += 1;
        Ok(())
    }
    pub(super) fn finish(mut self, result: &DiagnosticResult) -> Result<PathBuf> {
        if !self.header
            || self.phases != 2 + 4 * result.summary.stages.len()
            || self.last_stage.is_some_and(|(_, phase)| phase != 4)
            || result
                .summary
                .stages
                .iter()
                .any(|s| s.stage >= 3 || !self.seen_stages[usize::from(s.stage)])
        {
            return Err(invalid("incomplete observations"));
        }
        self.sink.write_all(b"],\"diagnostic\":")?;
        self.sink.json(&result.summary)?;
        self.sink.write_all(b"}\n")?;
        self.sink.close(&self.path)?;
        Ok(self.path)
    }
}

/// Borrowed serialization avoids a second graph/coordinate representation.
struct Problem<'a>(&'a PlaceProblem);
impl Serialize for Problem<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let p = self.0;
        let mut s = serializer.serialize_struct("NativeProblem", 7)?;
        s.serialize_field("widths", &p.widths)?;
        s.serialize_field("row_height", &p.row_height)?;
        s.serialize_field("site", &p.site)?;
        s.serialize_field("gap", &p.gap)?;
        s.serialize_field("nets", &Nets(&p.nets))?;
        s.serialize_field("weights", &p.weights)?;
        s.serialize_field("free_intervals", &Intervals(&p.intervals))?;
        s.end()
    }
}
struct Nets<'a>(&'a [Vec<PinRef>]);
impl Serialize for Nets<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for net in self.0 {
            seq.serialize_element(&Pins(net))?;
        }
        seq.end()
    }
}
struct Pins<'a>(&'a [PinRef]);
impl Serialize for Pins<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for pin in self.0 {
            seq.serialize_element(&Pin(pin))?;
        }
        seq.end()
    }
}
struct Pin<'a>(&'a PinRef);
impl Serialize for Pin<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("NativePin", 2)?;
        match self.0 {
            PinRef::Cell(id) => {
                s.serialize_field("kind", "cell")?;
                s.serialize_field("cell", id)?;
            }
            PinRef::Fixed(xy) => {
                s.serialize_field("kind", "fixed")?;
                s.serialize_field("xy", xy)?;
            }
        }
        s.end()
    }
}
struct Intervals<'a>(&'a [rustroad_core::place::Interval]);
impl Serialize for Intervals<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for interval in self.0 {
            seq.serialize_element(&Interval(interval))?;
        }
        seq.end()
    }
}
struct Interval<'a>(&'a rustroad_core::place::Interval);
impl Serialize for Interval<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("FreeInterval", 3)?;
        s.serialize_field("y", &self.0.y)?;
        s.serialize_field("lx", &self.0.lx)?;
        s.serialize_field("ux", &self.0.ux)?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rustroad-p1-diag-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            directory(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fresh_only_never_adopts_partial_or_existing_head() {
        let tmp = Temp::new();
        let out = tmp.0.join("output");
        assert!(check_fresh(&out, "empty").is_ok());
        assert!(matches!(
            check_fresh(&out, "rev1"),
            Err(Error::Conflict { .. })
        ));
        directory(&out).unwrap();
        assert!(
            check_fresh(&out, "empty")
                .unwrap_err()
                .to_string()
                .contains("OUTPUT_NOT_FRESH")
        );
        fs::write(out.join("HEAD"), b"rev1\n").unwrap();
        seal(&out.join("HEAD")).unwrap();
        assert!(matches!(
            check_fresh(&out, "empty"),
            Err(Error::Conflict { expected, actual }) if expected == "empty" && actual == "rev1"
        ));
        assert!(matches!(
            check_fresh(&out, "rev2"),
            Err(Error::Conflict { .. })
        ));
        assert!(matches!(
            check_fresh(&out, "rev1"),
            Err(Error::Invalid(message)) if message.contains("OUTPUT_NOT_FRESH")
        ));
        assert_eq!(fs::read(out.join("HEAD")).unwrap(), b"rev1\n");
    }

    #[test]
    fn output_head_must_be_a_positive_canonical_published_revision() {
        // Eight bounded HEAD-only fixtures, no design/PDK/optimizer allocation.
        let tmp = Temp::new();
        for (index, bytes) in [
            b"rev0\n".as_slice(),
            b"rev01\n",
            b"rev7",
            b"rev7\n\n",
            b"empty\n",
            b"revx\n",
            b"rev18446744073709551616\n",
            b"\xff\n",
        ]
        .into_iter()
        .enumerate()
        {
            let out = tmp.0.join(format!("output{index}"));
            directory(&out).unwrap();
            let head = out.join("HEAD");
            fs::write(&head, bytes).unwrap();
            seal(&head).unwrap();
            assert!(matches!(check_fresh(&out, "empty"), Err(Error::Invalid(_))));
            assert_eq!(fs::read(&head).unwrap(), bytes);
            assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
        }
    }

    #[test]
    fn output_create_rejects_mismatched_matching_or_invalid_head_without_adoption() {
        let tmp = Temp::new();
        for (name, head, base, conflict) in [
            ("mismatch", b"rev7\n".as_slice(), "empty", true),
            ("matching", b"rev7\n", "rev7", false),
            ("invalid", b"rev07\n", "empty", false),
        ] {
            let out = tmp.0.join(name);
            directory(&out).unwrap();
            fs::write(out.join("HEAD"), head).unwrap();
            seal(&out.join("HEAD")).unwrap();
            let error = match OwnedOutput::create(&out, base) {
                Ok(_) => panic!("existing output was adopted"),
                Err(error) => error,
            };
            assert_eq!(matches!(error, Error::Conflict { .. }), conflict);
            assert_eq!(fs::read(out.join("HEAD")).unwrap(), head);
            assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_symlinks_and_hardlinked_head_reject_without_writing() {
        let tmp = Temp::new();
        let real = tmp.0.join("real");
        directory(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.0.join("alias")).unwrap();
        assert!(check_fresh(&tmp.0.join("alias/output"), "empty").is_err());
        assert!(!real.join("output").exists());
        let out = tmp.0.join("output");
        directory(&out).unwrap();
        fs::write(out.join("HEAD"), b"rev1\n").unwrap();
        seal(&out.join("HEAD")).unwrap();
        fs::hard_link(out.join("HEAD"), tmp.0.join("other")).unwrap();
        assert!(check_fresh(&out, "rev1").is_err());
    }

    #[test]
    fn variable_write_failure_never_creates_head() {
        let tmp = Temp::new();
        let root = tmp.0.join("output");
        let output = OwnedOutput::create(&root, "empty").unwrap();
        let mut io = ByteLedger::new();
        let mut sink = Sink {
            output: BufWriter::new(new_file(&output.revision.join("observations.json")).unwrap()),
            io: &mut io,
            bytes: 0,
            cap: 3,
        };
        assert!(sink.write_all(b"four").is_err());
        drop(sink);
        assert!(!root.join("HEAD").exists());
        assert!(output.publish(&mut io).is_err());
        assert!(!root.join("HEAD").exists());
    }
}
