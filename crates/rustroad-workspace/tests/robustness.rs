//! Resource preflight: two cells, one row, one scope; bounded fixture buffers,
//! metadata and records total <4 KiB per test (no paths/cuts/clauses). Two test
//! workers, plus one writer thread in the lock test; thread stacks/runtime are
//! outside that payload estimate. Expected test RSS <128 MiB. Run with two Rayon
//! threads, two test workers, an 8 GiB VM cap and a 180 s external deadline.
//! All disk output is under the repository's `.work/robustness` directory.

use fs2::FileExt;
use rustroad_core::{Cell, Design, Limits, Rect, Row, Site};
use rustroad_workspace::{Error, init, place, read};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const RECORD_BYTES: usize = 40;

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock must be after epoch")
            .as_nanos();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/robustness")
            .join(format!(
                "{}-{stamp}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn limits() -> Limits {
    Limits {
        max_cells: 2,
        max_rows: 1,
        max_expansions: 3,
    }
}

fn fixture() -> Design {
    Design {
        name: "robustness".into(),
        units_per_micron: 1000,
        die_area: Rect {
            lx: 0,
            ly: 0,
            ux: 100,
            uy: 20,
        },
        core_area: Rect {
            lx: 0,
            ly: 0,
            ux: 100,
            uy: 20,
        },
        site: Site {
            name: "rustroad_site".into(),
            width: 10,
            height: 20,
        },
        rows: vec![Row {
            x_start: 0,
            x_end: 100,
            y: 0,
            orientation: rustroad_core::Orientation::N,
        }],
        cells: vec![
            Cell {
                name: "u0".into(),
                master: "BUF".into(),
                width: 10,
                height: 20,
            },
            Cell {
                name: "u1".into(),
                master: "BUF".into(),
                width: 10,
                height: 20,
            },
        ],
    }
}

fn placed_table(workspace: &Workspace) -> PathBuf {
    let imported = init(workspace.path(), "rev0", fixture(), &limits()).unwrap();
    let placed = place(workspace.path(), &imported.revision, &limits()).unwrap();
    workspace
        .path()
        .join("revisions")
        .join(placed.revision)
        .join("instances.rbin")
}

fn assert_invalid(workspace: &Workspace, expected: &str) {
    match read(workspace.path(), &limits()) {
        Err(Error::Invalid(message)) => assert!(message.contains(expected), "{message}"),
        Err(other) => panic!("expected invalid snapshot, got {other}"),
        Ok(_) => panic!("tampered snapshot was accepted"),
    }
}

#[test]
fn reader_rejects_tampered_placed_coordinate() {
    let workspace = Workspace::new();
    let table = placed_table(&workspace);
    // No mmap is alive while simulating offline file corruption. The published
    // v1 record consists of five little-endian 64-bit fields: id,x,y,width,height.
    let mut bytes = fs::read(&table).unwrap();
    assert_eq!(bytes.len(), 2 * RECORD_BYTES);
    bytes[8..16].copy_from_slice(&1_i64.to_le_bytes());
    fs::write(&table, bytes).unwrap();
    assert_invalid(&workspace, "illegal placement");
}

#[test]
fn reader_rejects_duplicate_instance_identity() {
    let workspace = Workspace::new();
    let table = placed_table(&workspace);
    let mut bytes = fs::read(&table).unwrap();
    assert_eq!(bytes.len(), 2 * RECORD_BYTES);
    let first_id: [u8; 8] = bytes[..8].try_into().unwrap();
    bytes[RECORD_BYTES..RECORD_BYTES + 8].copy_from_slice(&first_id);
    fs::write(&table, bytes).unwrap();
    assert_invalid(&workspace, "identity/dimension mismatch");
}

#[test]
fn reader_holds_writer_until_snapshot_is_dropped() {
    let workspace = Workspace::new();
    let imported = init(workspace.path(), "rev0", fixture(), &limits()).unwrap();
    let snapshot = read(workspace.path(), &limits()).unwrap();

    // Establish actual OS lock contention independently of thread scheduling.
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace.path().join("workspace.lock"))
        .unwrap();
    let contention = probe.try_lock_exclusive().unwrap_err();
    assert_eq!(
        contention.raw_os_error(),
        fs2::lock_contended_error().raw_os_error()
    );
    drop(probe);

    let writer_path = workspace.path().to_path_buf();
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let writer = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = place(&writer_path, &imported.revision, &limits());
        result_tx.send(result).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let before_drop = result_rx.recv_timeout(Duration::from_millis(100));
    drop(snapshot);
    let (completed_early, result) = match before_drop {
        Ok(result) => (true, result),
        Err(RecvTimeoutError::Timeout) => (
            false,
            result_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        ),
        Err(RecvTimeoutError::Disconnected) => panic!("writer disconnected"),
    };
    writer.join().unwrap();
    assert!(
        !completed_early,
        "writer published while read snapshot was alive"
    );
    assert_eq!(result.unwrap().revision, "rev2");
    assert_eq!(
        read(workspace.path(), &limits())
            .unwrap()
            .summary()
            .revision,
        "rev2"
    );
}

#[test]
fn failed_publication_keeps_head_and_retry_skips_orphan() {
    let workspace = Workspace::new();
    init(workspace.path(), "rev0", fixture(), &limits()).unwrap();
    let original_head = fs::read(workspace.path().join("HEAD")).unwrap();

    // Inject a filesystem conflict at publication, after the next revision has
    // been written. No private commit helper or test hook is used.
    let conflict = workspace.path().join("HEAD-rev2.pending");
    fs::create_dir(&conflict).unwrap();
    assert!(matches!(
        place(workspace.path(), "rev1", &limits()),
        Err(Error::Io(_))
    ));
    assert_eq!(
        fs::read(workspace.path().join("HEAD")).unwrap(),
        original_head
    );
    assert_eq!(
        read(workspace.path(), &limits())
            .unwrap()
            .summary()
            .revision,
        "rev1"
    );
    let orphan = workspace.path().join("revisions/rev2");
    assert!(orphan.join("design.json").is_file());
    assert_eq!(
        fs::metadata(orphan.join("instances.rbin")).unwrap().len(),
        80
    );

    fs::remove_dir(conflict).unwrap();
    let recovered = place(workspace.path(), "rev1", &limits()).unwrap();
    assert_eq!(recovered.revision, "rev3");
    let snapshot = read(workspace.path(), &limits()).unwrap();
    assert_eq!(snapshot.metadata().parent_revision, "rev1");
    assert_eq!(snapshot.records().len(), 2);
    assert!(
        orphan.is_dir(),
        "recovery must preserve the failed revision"
    );
}
