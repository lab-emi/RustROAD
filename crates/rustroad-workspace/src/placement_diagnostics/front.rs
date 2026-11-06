//! Narrow imported-snapshot admission before the ordinary typed loader.
use super::{ByteLedger, FileId};
use crate::{Error, Metadata, Result, SnapshotCaps, Stage};
use fs2::FileExt;
use serde::Serialize;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) const DESIGN_CAP: u64 = 16 << 20;
const REVISION_CAP: u64 = 32 << 20;
const FILES: [(&str, u64); 5] = [
    ("design.json", DESIGN_CAP),
    ("instances.rbin", 20_000 * 40),
    ("net_offsets.rbin", 20_001 * 8),
    ("net_pins.rbin", 100_000 * 4),
    ("pins.rbin", 100_000 * 16),
];

fn invalid(code: &str) -> Error {
    Error::Invalid(format!("P1_DIAGNOSTIC: {code}"))
}
fn budget(code: &str) -> Error {
    Error::Budget(format!("P1_DIAGNOSTIC: {code}"))
}

pub(super) fn plain(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(invalid("absolute contained path required"));
    }
    for part in path.ancestors() {
        if fs::symlink_metadata(part)?.file_type().is_symlink() {
            return Err(invalid("symlink path"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn unique_inode(value: &fs::Metadata) -> bool {
    value.nlink() == 1
}
#[cfg(not(unix))]
fn unique_inode(_: &fs::Metadata) -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct FileState {
    device: u64,
    inode: u64,
    bytes: u64,
    links: u64,
    mode: u32,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
#[cfg(unix)]
fn state(value: &fs::Metadata) -> FileState {
    FileState {
        device: value.dev(),
        inode: value.ino(),
        bytes: value.len(),
        links: value.nlink(),
        mode: value.mode(),
        mtime: value.mtime(),
        mtime_ns: value.mtime_nsec(),
        ctime: value.ctime(),
        ctime_ns: value.ctime_nsec(),
    }
}
#[cfg(not(unix))]
fn state(value: &fs::Metadata) -> FileState {
    FileState {
        device: 0,
        inode: 0,
        bytes: value.len(),
        links: 0,
        mode: 0,
        mtime: 0,
        mtime_ns: 0,
        ctime: 0,
        ctime_ns: 0,
    }
}

fn regular(path: &Path, cap: u64, readonly: bool) -> Result<fs::Metadata> {
    plain(path)?;
    let value = fs::symlink_metadata(path)?;
    if !value.is_file() || value.len() > cap {
        return Err(budget("file type/length admission"));
    }
    if !unique_inode(&value) {
        return Err(invalid("unique Unix inode admission"));
    }
    if readonly && !value.permissions().readonly() {
        return Err(invalid("source file is writable"));
    }
    Ok(value)
}

fn open_exact(path: &Path, before: &fs::Metadata) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let current = file.metadata()?;
    if !current.is_file() || state(before) != state(&current) {
        return Err(invalid("file changed before open"));
    }
    Ok(file)
}

pub(super) fn identity(
    path: &Path,
    cap: u64,
    readonly: bool,
    io: &mut ByteLedger,
) -> Result<FileId> {
    let before = regular(path, cap, readonly)?;
    io.charge(
        before
            .len()
            .checked_mul(2)
            .ok_or_else(|| budget("hash admission overflow"))?,
    )?;
    let mut file = open_exact(path, &before)?;
    let mut hash = Sha256::new();
    let mut left = before.len();
    let mut buffer = [0u8; 65536];
    while left > 0 {
        let amount =
            usize::try_from(left.min(buffer.len() as u64)).map_err(|_| budget("read length"))?;
        let n = file.read(&mut buffer[..amount])?;
        if n == 0 {
            return Err(invalid("short source read"));
        }
        hash.update(&buffer[..n]);
        left -= n as u64;
    }
    let mut end = [0u8; 1];
    if file.read(&mut end)? != 0 {
        return Err(invalid("source grew during read"));
    }
    let after = regular(path, cap, readonly)?;
    if state(&before) != state(&after) {
        return Err(invalid("source changed during read"));
    }
    Ok(FileId {
        path: path.to_path_buf(),
        bytes: before.len(),
        sha256: format!("{:x}", hash.finalize()),
    })
}

pub(super) fn read_small(
    path: &Path,
    cap: u64,
    readonly: bool,
    io: &mut ByteLedger,
) -> Result<Vec<u8>> {
    let before = regular(path, cap, readonly)?;
    io.charge(before.len())?;
    let file = open_exact(path, &before)?;
    let mut data = Vec::new();
    data.try_reserve_exact(usize::try_from(before.len()).map_err(|_| budget("input length"))?)
        .map_err(|_| budget("input allocation"))?;
    file.take(before.len() + 1).read_to_end(&mut data)?;
    if data.len() as u64 != before.len() {
        return Err(invalid("bounded input drift"));
    }
    Ok(data)
}

pub(super) fn disjoint(source: &Path, output: &Path) -> Result<()> {
    plain(source)?;
    if source.starts_with(output) || output.starts_with(source) {
        return Err(invalid("diagnostic source/output containment alias"));
    }
    #[cfg(unix)]
    {
        let original = fs::symlink_metadata(source)?;
        for parent in output
            .parent()
            .ok_or_else(|| invalid("output parent"))?
            .ancestors()
        {
            let other = fs::symlink_metadata(parent)?;
            if original.dev() == other.dev() && original.ino() == other.ino() {
                return Err(invalid("diagnostic source/output directory alias"));
            }
        }
    }
    Ok(())
}

fn inventory(revision: &Path) -> Result<BTreeMap<String, u64>> {
    plain(revision)?;
    let mut result = BTreeMap::new();
    let mut bytes = 0u64;
    let mut inodes = HashSet::new();
    for entry in fs::read_dir(revision)? {
        let entry = entry?;
        if result.len() >= 5 {
            return Err(budget("revision file count"));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non UTF8 source name"))?;
        let cap = FILES
            .iter()
            .find_map(|(n, cap)| (*n == name).then_some(*cap))
            .ok_or_else(|| invalid("unknown revision file"))?;
        let m = regular(&entry.path(), cap, true)?;
        #[cfg(unix)]
        if !inodes.insert((m.dev(), m.ino())) {
            return Err(invalid("source aliases"));
        }
        bytes = bytes
            .checked_add(m.len())
            .filter(|n| *n <= REVISION_CAP)
            .ok_or_else(|| budget("revision aggregate bytes"))?;
        result.insert(name, m.len());
    }
    if result.len() != 5 {
        return Err(invalid("incomplete revision inventory"));
    }
    Ok(result)
}

/// Observed structural counts; no whole-file JSON Value is constructed.
#[derive(Debug, Default, Serialize)]
pub(super) struct LexicalStats {
    pub(super) nodes: u64,
    pub(super) max_depth: usize,
    pub(super) decoded_string_bytes: u64,
    pub(super) max_decoded_string_bytes: usize,
    pub(super) aggregate_array_elements: u64,
    pub(super) arrays: BTreeMap<String, u64>,
    pub(super) format_version: Option<u64>,
}

struct Scan<'stats> {
    stats: &'stats mut LexicalStats,
    path: Vec<String>,
    depth: usize,
}
struct ArrayElement<'scan, 'stats>(&'scan mut Scan<'stats>);
struct RejectElement;
impl<'de> DeserializeSeed<'de> for RejectElement {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> std::result::Result<(), D::Error> {
        Err(serde::de::Error::custom(
            "nonempty generated diagnostic state",
        ))
    }
}
impl<'de> DeserializeSeed<'de> for ArrayElement<'_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<(), D::Error> {
        self.0.stats.aggregate_array_elements = self
            .0
            .stats
            .aggregate_array_elements
            .checked_add(1)
            .filter(|n| *n <= 1_000_000)
            .ok_or_else(|| serde::de::Error::custom("array element cap before value"))?;
        (&mut *self.0).deserialize(de)
    }
}
impl Scan<'_> {
    fn node(&mut self) -> std::result::Result<(), String> {
        self.stats.nodes = self
            .stats
            .nodes
            .checked_add(1)
            .filter(|n| *n <= 2_000_000)
            .ok_or("node cap")?;
        if self.depth > 32 {
            return Err("depth cap".into());
        }
        self.stats.max_depth = self.stats.max_depth.max(self.depth);
        Ok(())
    }
    fn string(&mut self, value: &str) -> std::result::Result<(), String> {
        if value.len() > 4096 {
            return Err("decoded string cap".into());
        }
        self.stats.decoded_string_bytes = self
            .stats
            .decoded_string_bytes
            .checked_add(value.len() as u64)
            .filter(|n| *n <= 16 << 20)
            .ok_or("aggregate decoded strings")?;
        self.stats.max_decoded_string_bytes = self.stats.max_decoded_string_bytes.max(value.len());
        Ok(())
    }
    fn unsupported_container(&self) -> bool {
        matches!(self.path.as_slice(), [one] if one=="tables")
            || matches!(self.path.as_slice(), [one,two] if one=="physical" && ["routing","state","clock_tree","placement","placement_seed","net_weights"].contains(&two.as_str()))
    }
    fn generated_state(&self) -> bool {
        matches!(self.path.as_slice(), [one,two] if one=="physical" && ["generated_taps","generated_fillers"].contains(&two.as_str()))
    }
}
impl<'de> DeserializeSeed<'de> for &mut Scan<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> std::result::Result<(), D::Error> {
        self.node().map_err(serde::de::Error::custom)?;
        de.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for &mut Scan<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bounded inline imported snapshot")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, n: i64) -> std::result::Result<(), E> {
        if i128::from(n).abs() > 1i128 << 40 {
            return Err(E::custom("diagnostic integer range"));
        }
        if self.path == ["format_version"] {
            return Err(E::custom("unsigned schema required"));
        }
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, n: u64) -> std::result::Result<(), E> {
        if n > 1u64 << 40 {
            return Err(E::custom("diagnostic integer range"));
        }
        if self.path == ["format_version"] {
            self.stats.format_version = Some(n);
        }
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Err(E::custom(
            "floating token unsupported in imported diagnostic snapshot",
        ))
    }
    fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<(), E> {
        self.string(s).map_err(E::custom)
    }
    fn visit_string<E: serde::de::Error>(self, s: String) -> std::result::Result<(), E> {
        self.visit_str(&s)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        if self.unsupported_container() {
            return Err(serde::de::Error::custom(
                "unsupported materialized/side-table state",
            ));
        }
        if self.generated_state() {
            // next_element_seed does not invoke the rejecting seed for an empty
            // sequence; nonempty state rejects before constructing its value.
            seq.next_element_seed(RejectElement)?;
            return Ok(());
        }
        let key = self.path.join("/");
        let mut n = 0u64;
        self.path.push("*".into());
        self.depth += 1;
        while let Some(()) = seq.next_element_seed(ArrayElement(&mut *self))? {
            n += 1;
        }
        self.depth -= 1;
        self.path.pop();
        if self.stats.arrays.len() >= 256 && !self.stats.arrays.contains_key(&key) {
            return Err(serde::de::Error::custom("array path cap"));
        }
        let total = self.stats.arrays.entry(key).or_default();
        *total = total
            .checked_add(n)
            .ok_or_else(|| serde::de::Error::custom("array count overflow"))?;
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        if self.unsupported_container() || self.generated_state() {
            return Err(serde::de::Error::custom(
                "unsupported materialized/side-table state",
            ));
        }
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            self.node().map_err(serde::de::Error::custom)?;
            self.string(&key).map_err(serde::de::Error::custom)?;
            if self.path.is_empty()
                && ![
                    "tables",
                    "format_version",
                    "revision",
                    "parent_revision",
                    "stage",
                    "design",
                    "expansions",
                    "physical",
                ]
                .contains(&key.as_str())
            {
                return Err(serde::de::Error::custom("unknown top-level field"));
            }
            if seen.len() >= 1024 || !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom("duplicate/too many object keys"));
            }
            self.path.push(key);
            self.depth += 1;
            map.next_value_seed(&mut *self)?;
            self.depth -= 1;
            self.path.pop();
        }
        Ok(())
    }
}

fn lexical(raw: &[u8]) -> Result<LexicalStats> {
    // This raw-token gate runs before serde can allocate an escaped key/string.
    let (mut quoted, mut escape, mut start, mut depth, mut scalar) =
        (false, false, 0usize, 0usize, 0usize);
    for (i, &b) in raw.iter().enumerate() {
        if quoted {
            if i - start > 24576 {
                return Err(budget("raw JSON string cap"));
            }
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                quoted = false;
            }
            continue;
        }
        match b {
            b'"' => {
                quoted = true;
                start = i;
                scalar = 0;
            }
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .filter(|n| *n <= 33)
                    .ok_or_else(|| budget("JSON depth"))?;
                scalar = 0;
            }
            b'}' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("JSON nesting"))?;
                scalar = 0;
            }
            b':' | b',' | b' ' | b'\r' | b'\n' | b'\t' => scalar = 0,
            _ => {
                scalar += 1;
                if scalar > 128 {
                    return Err(budget("JSON scalar token cap"));
                }
            }
        }
    }
    if quoted || depth != 0 {
        return Err(invalid("incomplete JSON framing"));
    }
    let mut stats = LexicalStats::default();
    let mut de = serde_json::Deserializer::from_slice(raw);
    (&mut Scan {
        stats: &mut stats,
        path: Vec::new(),
        depth: 0,
    })
        .deserialize(&mut de)?;
    de.end()?;
    if !matches!(stats.format_version, Some(3 | 4)) {
        return Err(invalid("only inline imported schema3/4 is admitted"));
    }
    let caps = [
        ("design/cells", 20_000),
        ("design/rows", 1024),
        ("physical/connectivity/instance_masters", 20_000),
        ("physical/connectivity/masters", 256),
        ("physical/connectivity/layers", 256),
        ("physical/technology/layers", 256),
        ("physical/connectivity/nets", 20_000),
        ("physical/connectivity/pins", 100_000),
        ("physical/connectivity/ports", 4096),
        ("physical/netlist/instances", 20_000),
        ("physical/connectivity/masters/*/pins", 16_384),
        ("physical/connectivity/masters/*/pins/*/ports/*", 131_072),
        ("physical/config/placement_blockages", 1024),
        ("physical/diagnostics", 4096),
        ("physical/sources", 16),
    ];
    for (path, cap) in caps {
        if stats.arrays.get(path).copied().unwrap_or(0) > cap {
            return Err(budget("predecode object count"));
        }
    }
    let records = ["instances", "ports", "wires", "assigns"]
        .iter()
        .try_fold(0u64, |n, k| {
            n.checked_add(
                stats
                    .arrays
                    .get(&format!("physical/netlist/{k}"))
                    .copied()
                    .unwrap_or(0),
            )
        })
        .ok_or_else(|| budget("netlist count"))?;
    if records > 200_000 {
        return Err(budget("netlist aggregate records"));
    }
    Ok(stats)
}

#[derive(Debug, Serialize)]
pub(super) struct SourceProof {
    pub(super) workspace: PathBuf,
    pub(super) revision: String,
    #[serde(rename = "HEAD")]
    pub(super) head: FileId,
    pub(super) files: BTreeMap<String, FileId>,
    pub(super) inventory: BTreeMap<String, u64>,
    pub(super) lexical: LexicalStats,
    pub(super) states: BTreeMap<PathBuf, FileState>,
}

pub(super) struct Opened {
    pub(super) metadata: Metadata,
    pub(super) lock: File,
    pub(super) proof: SourceProof,
}

pub(super) fn open(
    source: &Path,
    base: &str,
    limits: &rustroad_core::Limits,
    io: &mut ByteLedger,
) -> Result<Opened> {
    plain(source)?;
    if limits.max_cells > 250_000 || limits.max_rows > 100_000 || limits.max_expansions > 4_000_000
    {
        return Err(budget("diagnostic original workspace limits"));
    }
    let lock_path = source.join("workspace.lock");
    let lm = regular(&lock_path, 0, false)?;
    let lock = open_exact(&lock_path, &lm)?;
    FileExt::try_lock_shared(&lock).map_err(|_| invalid("SOURCE_LOCK_BUSY"))?;
    if state(&regular(&lock_path, 0, false)?) != state(&lm) {
        return Err(invalid("lock path changed"));
    }
    let head_id = identity(&source.join("HEAD"), 64, true, io)?;
    let head_raw = read_small(&source.join("HEAD"), 64, true, io)?;
    let revision = std::str::from_utf8(&head_raw)
        .map_err(|_| invalid("HEAD UTF8"))?
        .trim_end_matches('\n')
        .to_owned();
    if head_raw != format!("{revision}\n").as_bytes()
        || !revision
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid("HEAD handle"));
    }
    crate::revision_number(&revision)?;
    crate::check_base(base, &revision)?;
    let path = source.join("revisions").join(&revision);
    let inventory = inventory(&path)?;
    let mut files = BTreeMap::new();
    let mut states = BTreeMap::new();
    states.insert(lock_path, state(&lm));
    states.insert(
        source.join("HEAD"),
        state(&regular(&source.join("HEAD"), 64, true)?),
    );
    for (name, cap) in FILES {
        files.insert(name.to_owned(), identity(&path.join(name), cap, true, io)?);
        states.insert(
            path.join(name),
            state(&regular(&path.join(name), cap, true)?),
        );
    }
    let raw = read_small(&path.join("design.json"), DESIGN_CAP, true, io)?;
    io.charge(raw.len() as u64 * 2)?;
    let stats = lexical(&raw)?;
    drop(raw);
    // One metadata read and each authoritative mapped extent in the old loader.
    io.charge(
        inventory
            .values()
            .try_fold(0u64, |a, b| a.checked_add(*b))
            .ok_or_else(|| budget("restore extents"))?,
    )?;
    let caps = SnapshotCaps {
        route_bytes: 0,
        route_records: 0,
        routed_ids: 0,
        memory_bytes: 768 << 20,
        inventory_bytes: 1 << 20,
    };
    // Always the full validation, whatever receipt the workspace holds for
    // the source revision (receipts live outside the revision directory).
    let snapshot = crate::load_with(
        source,
        lock,
        limits,
        Some(caps),
        crate::LoadOptions {
            routes: true,
            force: true,
        },
    )?;
    let (lock, metadata) = snapshot.into_metadata();
    validate_typed(&metadata, &inventory, &stats)?;
    Ok(Opened {
        metadata,
        lock,
        proof: SourceProof {
            workspace: source.to_path_buf(),
            revision,
            head: head_id,
            files,
            inventory,
            lexical: stats,
            states,
        },
    })
}

fn validate_typed(
    metadata: &Metadata,
    files: &BTreeMap<String, u64>,
    stats: &LexicalStats,
) -> Result<()> {
    if metadata.stage != Stage::Imported
        || metadata.expansions != 0
        || metadata.tables.is_some()
        || metadata.format_version != stats.format_version.unwrap_or(0) as u32
    {
        return Err(invalid("typed imported schema mismatch"));
    }
    let p = metadata
        .physical
        .as_ref()
        .ok_or_else(|| invalid("physical bundle required"))?;
    if p.config
        .as_ref()
        .and_then(|c| c.placer.as_ref())
        .and_then(|c| c.electrostatic.as_ref())
        .is_none()
        || p.routing.is_some()
        || p.state.is_some()
        || p.clock_tree.is_some()
        || p.placement.is_some()
        || p.placement_seed.is_some()
        || p.net_weights.is_some()
        || !p.generated_fillers.is_empty()
        || !p.generated_taps.is_empty()
    {
        return Err(invalid("unsupported seeded/materialized snapshot"));
    }
    let counts = [
        ("design/cells", metadata.design.cells.len()),
        ("design/rows", metadata.design.rows.len()),
        (
            "physical/connectivity/instance_masters",
            p.connectivity.instance_masters.len(),
        ),
        (
            "physical/connectivity/masters",
            p.connectivity.masters.len(),
        ),
        ("physical/connectivity/nets", p.connectivity.nets.len()),
        ("physical/connectivity/pins", p.connectivity.pins.len()),
        ("physical/netlist/instances", p.netlist.instances.len()),
    ];
    for (path, count) in counts {
        if stats.arrays.get(path).copied().unwrap_or(0) != count as u64 {
            return Err(invalid("pre/post decode count mismatch"));
        }
    }
    if files["instances.rbin"] != metadata.design.cells.len() as u64 * 40
        || files["pins.rbin"] != p.connectivity.pins.len() as u64 * 16
        || files["net_offsets.rbin"] != (p.connectivity.nets.len() as u64 + 1) * 8
        || !files["net_pins.rbin"].is_multiple_of(4)
        || files["net_pins.rbin"] > p.connectivity.pins.len() as u64 * 4
    {
        return Err(invalid("canonical table lengths"));
    }
    Ok(())
}

pub(super) fn recheck(proof: &SourceProof, io: &mut ByteLedger) -> Result<()> {
    for (path, expected) in &proof.states {
        if state(&fs::symlink_metadata(path)?) != *expected {
            return Err(invalid("source inode/state changed"));
        }
    }
    if identity(&proof.workspace.join("HEAD"), 64, true, io)? != proof.head {
        return Err(invalid("HEAD changed"));
    }
    let path = proof.workspace.join("revisions").join(&proof.revision);
    if inventory(&path)? != proof.inventory {
        return Err(invalid("source inventory changed"));
    }
    for (name, cap) in FILES {
        if identity(&path.join(name), cap, true, io)? != proof.files[name] {
            return Err(invalid("source bytes changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sealed source revision holds exactly its five tables: validation
    /// receipts live outside revision directories, and nothing else (a
    /// receipt-named file included) is admitted.
    #[test]
    fn revision_inventory_admits_exactly_the_five_tables() {
        let dir =
            std::env::temp_dir().join(format!("rustroad-p1-inventory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let seal = |name: &str| {
            let path = dir.join(name);
            fs::write(&path, b"xx").unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();
        };
        for (name, _) in FILES {
            seal(name);
        }
        assert_eq!(inventory(&dir).unwrap().len(), 5);
        seal("validated.json");
        assert!(inventory(&dir).is_err());
        let unseal = |name: &str| {
            let path = dir.join(name);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
            fs::remove_file(path).unwrap();
        };
        unseal("validated.json");
        assert_eq!(inventory(&dir).unwrap().len(), 5);
        unseal("pins.rbin");
        assert!(inventory(&dir).is_err(), "a required table is missing");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn anchored_schema_and_strict_numeric_admission() {
        assert!(lexical(br#"{"format_version":3}"#).is_ok());
        for raw in [
            br#"{"design":{"format_version":3}}"#.as_slice(),
            br#"{"format_version":true}"#,
            br#"{"format_version":3.0}"#,
            br#"{"format_version":3,"design":{"x":1e3}}"#,
            br#"{"format_version":3,"design":{"x":1099511627777}}"#,
            br#"{"format_version":3,"format_version":3}"#,
        ] {
            assert!(
                lexical(raw).is_err(),
                "must reject {}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn generated_payload_rejects_before_decoding_its_first_value() {
        for field in ["generated_taps", "generated_fillers"] {
            assert!(
                lexical(
                    format!(r#"{{"format_version":4,"physical":{{"{field}":[]}}}}"#).as_bytes()
                )
                .is_ok()
            );
            // The malformed first body proves the rejecting seed is reached
            // before parsing or allocating any generated object members.
            let error = lexical(
                format!(r#"{{"format_version":4,"physical":{{"{field}":[{{"deep":???}}]}}}}"#)
                    .as_bytes(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("nonempty generated"), "{error}");
        }
    }

    #[test]
    fn unsupported_tables_and_materialized_containers_are_not_restored() {
        for field in [
            "routing",
            "state",
            "clock_tree",
            "placement",
            "placement_seed",
            "net_weights",
        ] {
            assert!(
                lexical(
                    format!(r#"{{"format_version":4,"physical":{{"{field}":{{}}}}}}"#).as_bytes()
                )
                .is_err()
            );
        }
        assert!(lexical(br#"{"format_version":3,"tables":{}}"#).is_err());
    }

    #[test]
    fn count_and_raw_token_boundaries_are_checked() {
        let rows = std::iter::repeat_n("null", 1025)
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            lexical(format!(r#"{{"format_version":3,"design":{{"rows":[{rows}]}}}}"#).as_bytes())
                .is_err()
        );
        let long = "x".repeat(24577);
        assert!(
            lexical(format!(r#"{{"format_version":3,"design":{{"x":"{long}"}}}}"#).as_bytes())
                .is_err()
        );
        let deep = format!("{}null{}", "[".repeat(34), "]".repeat(34));
        assert!(
            lexical(format!(r#"{{"format_version":3,"design":{{"x":{deep}}}}}"#).as_bytes())
                .is_err()
        );
    }
}
