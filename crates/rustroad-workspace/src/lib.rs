//! Locked, immutable disk snapshots. Only HEAD publication changes visible state.
use fs2::FileExt;
use memmap2::Mmap;
use rustroad_core::{Design, DesignDb, Limits, Placed, place_cpu_ref};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const RECORD_BYTES: usize = 40;
const MAX_ORPHAN_SLOTS: u64 = 128;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO_ERROR: {0}")]
    Io(#[from] std::io::Error),
    #[error("INVALID_JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Design(#[from] rustroad_core::Error),
    #[error("REVISION_CONFLICT: expected {expected}, workspace HEAD is {actual}")]
    Conflict { expected: String, actual: String },
    #[error("INVALID_WORKSPACE: {0}")]
    Invalid(String),
    #[error("BUDGET_EXCEEDED: {0}")]
    Budget(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Imported,
    Placed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub format_version: u32,
    pub revision: String,
    pub parent_revision: String,
    pub stage: Stage,
    pub design: Design,
    pub expansions: usize,
}
#[derive(Debug, Serialize)]
pub struct Summary {
    pub revision: String,
    pub stage: Stage,
    pub design: String,
    pub cells: usize,
    pub rows: usize,
    pub expansions: usize,
    pub placement_status: &'static str,
    pub drc_status: &'static str,
    pub lvs_status: &'static str,
    pub routing_status: &'static str,
    pub corner: &'static str,
    pub wire_model: &'static str,
    pub unconstrained_endpoints: Option<usize>,
}
impl Metadata {
    pub fn summary(&self) -> Summary {
        Summary {
            revision: self.revision.clone(),
            stage: self.stage.clone(),
            design: self.design.name.clone(),
            cells: self.design.cells.len(),
            rows: self.design.rows.len(),
            expansions: self.expansions,
            placement_status: if self.stage == Stage::Placed {
                "placed_unverified"
            } else {
                "not_run"
            },
            drc_status: "unchecked",
            lvs_status: "unchecked",
            routing_status: "not_run",
            corner: "not_applicable",
            wire_model: "none",
            unconstrained_endpoints: None,
        }
    }
}

pub struct Snapshot {
    // Held while mappings are alive; readers must never observe an unpublished snapshot.
    _lock: File,
    metadata: Metadata,
    map: Mmap,
}
impl Snapshot {
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
    pub fn records(&self) -> impl ExactSizeIterator<Item = Placed> + '_ {
        self.map.chunks_exact(RECORD_BYTES).map(decode_record)
    }
    pub fn summary(&self) -> Summary {
        self.metadata.summary()
    }
    pub fn export_def(&self, output: &mut impl Write) -> Result<()> {
        if self.metadata.stage != Stage::Placed {
            return Err(Error::Invalid(
                "place the design before exporting DEF".into(),
            ));
        }
        let records = self.records().collect::<Vec<_>>();
        rustroad_core::write_def(&self.metadata.design, &records, output)?;
        Ok(())
    }
}

pub fn read_design(path: &Path, limits: &Limits) -> Result<Design> {
    let design: Design = serde_json::from_slice(&read_bounded(path, MAX_METADATA_BYTES)?)?;
    design.validate(limits)?;
    Ok(design)
}

pub fn init(path: &Path, base_rev: &str, design: Design, limits: &Limits) -> Result<Summary> {
    let lock = lock(path, true, true)?;
    let current = head(path)?;
    check_base(base_rev, &current)?;
    if current != "rev0" {
        return Err(Error::Invalid("workspace is already initialized".into()));
    }
    design.validate(limits)?;
    // Programmatic callers receive the same metadata byte cap as CLI imports.
    drop(serialize_bounded(&design)?);
    let db = DesignDb::new(&design, limits)?;
    let records = db.unplaced_records();
    let metadata = Metadata {
        format_version: 1,
        revision: String::new(),
        parent_revision: current,
        stage: Stage::Imported,
        design,
        expansions: 0,
    };
    let result = commit(path, metadata, &records)?;
    drop(lock);
    Ok(result)
}

pub fn place(path: &Path, base_rev: &str, limits: &Limits) -> Result<Summary> {
    let lock = lock(path, false, true)?;
    let current = head(path)?;
    check_base(base_rev, &current)?;
    let snapshot = load(path, lock, limits)?;
    let db = DesignDb::new(&snapshot.metadata.design, limits)?;
    let placement = place_cpu_ref(&db, limits)?;
    let mut metadata = snapshot.metadata.clone();
    metadata.parent_revision = current;
    metadata.stage = Stage::Placed;
    metadata.expansions = placement.expansions;
    // Keep the exclusive lock for the entire calculation and publication.
    commit(path, metadata, &placement.cells)
}

pub fn read(path: &Path, limits: &Limits) -> Result<Snapshot> {
    let lock = lock(path, false, false)?;
    load(path, lock, limits)
}

fn lock(path: &Path, create: bool, exclusive: bool) -> Result<File> {
    if create {
        fs::create_dir_all(path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path.join("workspace.lock"))?;
    if exclusive {
        file.lock_exclusive()?;
    } else {
        FileExt::lock_shared(&file)?;
    }
    Ok(file)
}
fn revision_number(revision: &str) -> Result<u64> {
    let digits = revision
        .strip_prefix("rev")
        .ok_or_else(|| Error::Invalid("invalid revision handle".into()))?;
    let n = digits
        .parse::<u64>()
        .map_err(|_| Error::Invalid("invalid revision handle".into()))?;
    if revision != format!("rev{n}") {
        return Err(Error::Invalid("noncanonical revision handle".into()));
    }
    Ok(n)
}
fn head(path: &Path) -> Result<String> {
    match read_bounded(&path.join("HEAD"), 32) {
        Ok(bytes) => {
            let value = std::str::from_utf8(&bytes)
                .map_err(|_| Error::Invalid("non-UTF8 HEAD".into()))?
                .trim_end_matches('\n');
            if revision_number(value)? == 0 {
                return Err(Error::Invalid("published HEAD cannot be rev0".into()));
            }
            Ok(value.into())
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok("rev0".into()),
        Err(error) => Err(error),
    }
}
fn check_base(expected: &str, actual: &str) -> Result<()> {
    revision_number(expected)?;
    if expected != actual {
        return Err(Error::Conflict {
            expected: expected.into(),
            actual: actual.into(),
        });
    }
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    // Do not block opening an ordinary named pipe supplied as an input file.
    if !fs::metadata(path)?.is_file() {
        return Err(Error::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Invalid("input is not a regular file".into()));
    }
    let length = file.metadata()?.len();
    if length > limit {
        return Err(Error::Budget(format!(
            "{} exceeds {limit} byte limit",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Budget("file grew while reading".into()));
    }
    Ok(bytes)
}
fn load(path: &Path, lock: File, limits: &Limits) -> Result<Snapshot> {
    let current = head(path)?;
    if current == "rev0" {
        return Err(Error::Invalid("workspace has no published revision".into()));
    }
    let revision_dir = path.join("revisions").join(&current);
    let metadata: Metadata = serde_json::from_slice(&read_bounded(
        &revision_dir.join("design.json"),
        MAX_METADATA_BYTES,
    )?)?;
    if metadata.format_version != 1
        || metadata.revision != current
        || revision_number(&metadata.parent_revision)? >= revision_number(&current)?
    {
        return Err(Error::Invalid("snapshot schema/revision mismatch".into()));
    }
    metadata.design.validate(limits)?;
    if metadata.stage == Stage::Imported && metadata.expansions != 0 {
        return Err(Error::Invalid(
            "imported snapshot has placement expansions".into(),
        ));
    }
    let file = File::open(revision_dir.join("instances.rbin"))?;
    let expected = metadata
        .design
        .cells
        .len()
        .checked_mul(RECORD_BYTES)
        .ok_or_else(|| Error::Budget("table length overflow".into()))?;
    if file.metadata()?.len() != expected as u64 {
        return Err(Error::Invalid("instance table length mismatch".into()));
    }
    // SAFETY: writers only create new immutable revision files while holding the
    // workspace lock, and never truncate a published table. Mapping is read-only.
    // External programs must honor this contract; arbitrary outside file mutation
    // is unsupported, as with every mmap-backed workspace.
    #[allow(unsafe_code)]
    let map = unsafe { Mmap::map(&file)? };
    let snapshot = Snapshot {
        _lock: lock,
        metadata,
        map,
    };
    let mut seen = HashSet::with_capacity(snapshot.metadata.design.cells.len());
    for (record, cell) in snapshot.records().zip(&snapshot.metadata.design.cells) {
        if record.id == 0
            || !seen.insert(record.id)
            || record.width != cell.width
            || record.height != cell.height
        {
            return Err(Error::Invalid(
                "instance table identity/dimension mismatch".into(),
            ));
        }
    }
    if snapshot.metadata.stage == Stage::Placed {
        check_placed(&snapshot)?;
    }
    Ok(snapshot)
}

// Linear validation for the version-1 fixed-order row-packer snapshot contract.
fn check_placed(snapshot: &Snapshot) -> Result<()> {
    let design = &snapshot.metadata.design;
    let mut row_index = 0;
    let mut previous_right = design.rows[0].x_start;
    for cell in snapshot.records() {
        while row_index < design.rows.len() && design.rows[row_index].y < cell.y {
            row_index += 1;
            if row_index < design.rows.len() {
                previous_right = design.rows[row_index].x_start;
            }
        }
        let row = design
            .rows
            .get(row_index)
            .ok_or_else(|| Error::Invalid("instance outside rows".into()))?;
        let right = cell
            .x
            .checked_add(cell.width)
            .ok_or_else(|| Error::Invalid("coordinate overflow".into()))?;
        if cell.y != row.y
            || cell.x < previous_right
            || right > row.x_end
            || (i128::from(cell.x) - i128::from(row.x_start)) % i128::from(design.site.width) != 0
        {
            return Err(Error::Invalid("illegal placement in snapshot".into()));
        }
        previous_right = right;
    }
    Ok(())
}

fn commit(path: &Path, mut metadata: Metadata, records: &[Placed]) -> Result<Summary> {
    fs::create_dir_all(path.join("revisions"))?;
    let first = revision_number(&metadata.parent_revision)?
        .checked_add(1)
        .ok_or_else(|| Error::Budget("revision number exhausted".into()))?;
    let mut chosen: Option<PathBuf> = None;
    for offset in 0..MAX_ORPHAN_SLOTS {
        let number = first
            .checked_add(offset)
            .ok_or_else(|| Error::Budget("revision number exhausted".into()))?;
        let revision = format!("rev{number}");
        let directory = path.join("revisions").join(&revision);
        match fs::create_dir(&directory) {
            Ok(()) => {
                metadata.revision = revision;
                chosen = Some(directory);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let directory = chosen.ok_or_else(|| {
        Error::Budget("too many orphan revisions; inspect workspace before recovery".into())
    })?;
    let bytes = serialize_bounded(&metadata)?;
    write_new(&directory.join("design.json"), &bytes)?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory.join("instances.rbin"))?;
    let mut table = BufWriter::with_capacity(256 * 1024, file);
    for record in records {
        table.write_all(&record.id.to_le_bytes())?;
        table.write_all(&record.x.to_le_bytes())?;
        table.write_all(&record.y.to_le_bytes())?;
        table.write_all(&record.width.to_le_bytes())?;
        table.write_all(&record.height.to_le_bytes())?;
    }
    table.flush()?;
    table.get_ref().sync_all()?;
    sync_directory(&directory)?;
    sync_directory(&path.join("revisions"))?;
    let pending = path.join(format!("HEAD-{}.pending", metadata.revision));
    write_new(&pending, format!("{}\n", metadata.revision).as_bytes())?;
    fs::rename(&pending, path.join("HEAD"))?;
    sync_directory(path)?;
    Ok(metadata.summary())
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn serialize_bounded(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Buffer {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > (MAX_METADATA_BYTES as usize).saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("metadata byte budget exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut buffer = Buffer {
        bytes: Vec::new(),
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut buffer, value) {
        if buffer.exceeded {
            return Err(Error::Budget("serialized metadata exceeds limit".into()));
        }
        return Err(Error::Json(error));
    }
    Ok(buffer.bytes)
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn decode_record(bytes: &[u8]) -> Placed {
    fn i64_at(b: &[u8], index: usize) -> i64 {
        i64::from_le_bytes(b[index..index + 8].try_into().expect("fixed record"))
    }
    Placed {
        id: u64::from_le_bytes(bytes[..8].try_into().expect("fixed record")),
        x: i64_at(bytes, 8),
        y: i64_at(bytes, 16),
        width: i64_at(bytes, 24),
        height: i64_at(bytes, 32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustroad_core::{Cell, Rect, Row, Site};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    fn dir() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn fixture() -> Design {
        Design {
            name: "tiny".into(),
            units_per_micron: 1000,
            die_area: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 40,
            },
            core_area: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 40,
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
            }],
            cells: vec![Cell {
                name: "u0".into(),
                master: "BUF".into(),
                width: 10,
                height: 20,
            }],
        }
    }
    #[test]
    fn revision_conflicts_and_binary_roundtrip() {
        let dir = dir();
        let limits = Limits::default();
        assert_eq!(
            init(&dir, "rev0", fixture(), &limits).unwrap().revision,
            "rev1"
        );
        assert!(matches!(
            place(&dir, "rev0", &limits),
            Err(Error::Conflict { .. })
        ));
        assert_eq!(place(&dir, "rev1", &limits).unwrap().revision, "rev2");
        let read = read(&dir, &limits).unwrap();
        assert_eq!(read.records().next().unwrap().width, 10);
        let mut def = Vec::new();
        read.export_def(&mut def).unwrap();
        assert!(
            String::from_utf8(def)
                .unwrap()
                .contains("+ PLACED ( 0 0 ) N")
        );
        drop(read);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_placement_leaves_head_and_orphan_is_skipped() {
        let dir = dir();
        let limits = Limits::default();
        init(&dir, "rev0", fixture(), &limits).unwrap();
        let limited = Limits {
            max_expansions: 0,
            ..limits.clone()
        };
        assert!(place(&dir, "rev1", &limited).is_err());
        assert_eq!(head(&dir).unwrap(), "rev1");
        fs::create_dir(dir.join("revisions/rev2")).unwrap();
        assert_eq!(place(&dir, "rev1", &limits).unwrap().revision, "rev3");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn corruption_is_not_accepted_as_a_snapshot() {
        let dir = dir();
        let limits = Limits::default();
        init(&dir, "rev0", fixture(), &limits).unwrap();
        fs::write(dir.join("revisions/rev1/instances.rbin"), [0; 3]).unwrap();
        assert!(matches!(read(&dir, &limits), Err(Error::Invalid(_))));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn concurrent_writers_require_the_current_revision() {
        let dir = dir();
        let limits = Limits::default();
        init(&dir, "rev0", fixture(), &limits).unwrap();
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let path = dir.clone();
                std::thread::spawn(move || place(&path, "rev1", &Limits::default()))
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(Error::Conflict { .. })))
                .count(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
