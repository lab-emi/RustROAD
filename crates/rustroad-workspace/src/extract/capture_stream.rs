//! Default-off, bounded static/query producer; it never invokes a component.
use super::capture::{
    FileId, IoWork, blocks, budget, encoded, fingerprint, invalid, output_file, plain, read_head,
    shape, verify,
};
use super::{ExtractRequest, with_preprocessed};
use crate::{Error, Result};
use rustroad_core::Limits;
use rustroad_core::extract::{ExtractLimits, capture_stream as core};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

const JOB_BYTES: usize = 64 << 10;
const RECEIPT_BYTES: usize = 64 << 10;
const STDOUT_BYTES: usize = 8 << 10;
const HEADER: usize = 4096;
const SOURCE_BYTES: u64 = 128 << 20;
const BITS_BYTES: u64 = 128 << 10;
const FILES: [&str; 7] = [
    "design.json",
    "instances.rbin",
    "pins.rbin",
    "net_offsets.rbin",
    "net_pins.rbin",
    "physical.rbin",
    "routes.rbin",
];
/// Format identity; the actual ELF and source-manifest independently bind code.
const PROTOCOL: &str = concat!(
    "rustroad.stream.v2;RG1STA02=4096,4,4,96,4,48;",
    "RG1QRY02=4096,40Q,4096;RG1END02;Q12000000;",
    "origin=rusteda.g1-road-stream-capture.v2;nodes4096;depth32;bytes65536"
);

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EngineLimits {
    memory_mib: u64,
    max_work: usize,
    max_shapes: usize,
    max_virtual_shapes: usize,
    max_bin_entries: usize,
    max_instance_bin_entries: usize,
    max_nodes: usize,
    max_elements: usize,
    max_couplings: usize,
    max_candidates: usize,
    max_segment_um: f64,
    min_coupling_ff: f64,
}
impl EngineLimits {
    fn checked(&self, io: usize) -> Result<ExtractLimits> {
        if !(1..=3584).contains(&self.memory_mib)
            || self.max_work > 100_000_000_000
            || !self.max_segment_um.is_finite()
            || !(0.1..=10_000.0).contains(&self.max_segment_um)
            || !self.min_coupling_ff.is_finite()
            || !(0.0..=1_000_000.0).contains(&self.min_coupling_ff)
        {
            return Err(budget("STREAM_ORIGINAL_LIMITS"));
        }
        let value = ExtractLimits {
            memory_mib: self.memory_mib,
            max_work: self
                .max_work
                .checked_sub(io)
                .ok_or_else(|| budget("STREAM_IO_ADMISSION"))?,
            max_shapes: self.max_shapes,
            max_virtual_shapes: self.max_virtual_shapes,
            max_bin_entries: self.max_bin_entries,
            max_instance_bin_entries: self.max_instance_bin_entries,
            max_nodes: self.max_nodes,
            max_elements: self.max_elements,
            max_couplings: self.max_couplings,
            max_candidates: self.max_candidates,
            max_segment_length: 1,
            min_coupling_pf: self.min_coupling_ff / 1000.0,
            query_workers: 1,
            crossing_workers: 1,
            ..Default::default()
        };
        value.validate()?;
        Ok(value)
    }
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceLimits {
    max_cells: usize,
    max_rows: usize,
    max_expansions: usize,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    schema: String,
    source_workspace: PathBuf,
    source_revision: String,
    source_files: BTreeMap<String, FileId>,
    source_metadata: BTreeMap<String, FileId>,
    profile: FileId,
    corner: String,
    producer: FileId,
    source_manifest: FileId,
    protocol_sha256: String,
    limits: EngineLimits,
    workspace_limits: WorkspaceLimits,
    origin: serde_json::Value,
}

/// Bound tokens before decoding the tree; no allocator-sized metadata surprise.
fn preparse(data: &[u8]) -> Result<()> {
    if data.len() > JOB_BYTES {
        return Err(budget("STREAM_JSON_BYTES"));
    }
    let (mut depth, mut nodes, mut quoted, mut escaped, mut scalar) =
        (0usize, 0usize, false, false, false);
    for &byte in data {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                quoted = true;
                nodes = nodes
                    .checked_add(1)
                    .ok_or_else(|| budget("STREAM_JSON_NODES"))?;
                scalar = false;
            }
            b'{' | b'[' => {
                depth += 1;
                nodes += 1;
                scalar = false;
            }
            b'}' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("STREAM_JSON_DEPTH"))?;
                scalar = false;
            }
            b':' | b',' | b' ' | b'\n' | b'\r' | b'\t' => scalar = false,
            _ => {
                if !scalar {
                    nodes += 1;
                    scalar = true;
                }
            }
        }
        if depth > 32 || nodes > 4096 {
            return Err(budget("STREAM_JSON_DEPTH_NODES"));
        }
    }
    if depth != 0 || quoted {
        return Err(invalid("STREAM_JSON_TRUNCATED"));
    }
    Ok(())
}
struct UniqueValue(serde_json::Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("finite unique bounded JSON")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(n.into()))
                    .ok_or_else(|| E::custom("nonfinite JSON"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(serde_json::Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(UniqueValue(value)) = a.next_element()? {
                    if out.len() >= 4096 {
                        return Err(serde::de::Error::custom("array cap"));
                    }
                    out.try_reserve(1)
                        .map_err(|_| serde::de::Error::custom("array allocation"))?;
                    out.push(value);
                }
                Ok(UniqueValue(out.into()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some((key, UniqueValue(value))) = a.next_entry::<String, UniqueValue>()? {
                    if out.len() >= 4096 || out.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate/object cap"));
                    }
                    out.insert(key, value);
                }
                Ok(UniqueValue(out.into()))
            }
        }
        d.deserialize_any(V)
    }
}
fn strict_job(data: &[u8]) -> Result<Job> {
    preparse(data)?;
    let UniqueValue(value) =
        serde_json::from_slice(data).map_err(|_| invalid("STREAM_JOB_JSON"))?;
    serde_json::from_value(value).map_err(|_| invalid("STREAM_JOB_SCHEMA"))
}
/// SHA-256 of the independently bound v2 format-description string.
pub fn protocol_sha256() -> String {
    format!("{:x}", Sha256::digest(PROTOCOL.as_bytes()))
}
fn control(io: &mut IoWork, n: usize) -> Result<()> {
    io.used = io
        .used
        .checked_add(n)
        .filter(|&v| v <= io.reserved)
        .ok_or_else(|| budget("STREAM_CONTROL_IO_TAIL"))?;
    Ok(())
}
fn cache_pages(bytes: u64) -> Result<u64> {
    bytes
        .checked_add(4095)
        .map(|n| n / 4096 * 4096)
        .ok_or_else(|| budget("STREAM_CACHE_PAGES"))
}
fn frozen(value: &FileId, cap: u64) -> Result<()> {
    shape(value, cap)?;
    if fs::symlink_metadata(&value.path)?.permissions().mode() & 0o222 != 0 {
        return Err(invalid("STREAM_INPUT_WRITABLE"));
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mn: i64,
    ctime: i64,
    cn: i64,
    mode: u32,
    nlink: u64,
}
fn state(path: &Path) -> Result<FileState> {
    let m = fs::symlink_metadata(plain(path, true)?)?;
    if !m.is_file() || m.nlink() != 1 {
        return Err(invalid("STREAM_FILE_ALIAS"));
    }
    Ok(FileState {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mtime: m.mtime(),
        mn: m.mtime_nsec(),
        ctime: m.ctime(),
        cn: m.ctime_nsec(),
        mode: m.mode(),
        nlink: m.nlink(),
    })
}

fn job_parent_state(job: &Path) -> Result<FileState> {
    let parent = job.parent().ok_or_else(|| invalid("STREAM_JOB_PARENT"))?;
    let m = fs::symlink_metadata(plain(parent, true)?)?;
    if !m.is_dir() || m.permissions().mode() & (0o222 | 0o077) != 0 {
        return Err(invalid("STREAM_JOB_PARENT_NOT_SEALED_PRIVATE"));
    }
    let mut entries = fs::read_dir(parent)?;
    if entries
        .next()
        .transpose()?
        .is_none_or(|entry| entry.path() != job)
        || entries.next().transpose()?.is_some()
    {
        return Err(invalid("STREAM_JOB_PARENT_INVENTORY"));
    }
    Ok(FileState {
        dev: m.dev(),
        ino: m.ino(),
        len: m.len(),
        mtime: m.mtime(),
        mn: m.mtime_nsec(),
        ctime: m.ctime(),
        cn: m.ctime_nsec(),
        mode: m.mode(),
        nlink: m.nlink(),
    })
}

fn source_inventory(root: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut found = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut entries = 0usize;
    while let Some((path, depth)) = stack.pop() {
        if depth > 64 {
            return Err(budget("STREAM_INPUT_DEPTH"));
        }
        for entry in fs::read_dir(path)? {
            entries += 1;
            if entries > 512 {
                return Err(budget("STREAM_INPUT_ENTRIES"));
            }
            let entry = entry?;
            let m = fs::symlink_metadata(entry.path())?;
            if m.file_type().is_symlink() {
                return Err(invalid("STREAM_INPUT_SYMLINK"));
            }
            if m.is_dir() {
                stack.push((entry.path(), depth + 1));
            } else if m.is_file() {
                found.insert(entry.path());
            } else {
                return Err(invalid("STREAM_INPUT_TYPE"));
            }
        }
    }
    Ok(found)
}

#[derive(Clone, Debug)]
struct Artifacts {
    static_view: FileId,
    query_tape: FileId,
    legacy_bits: FileId,
}
struct Sink<'a> {
    target: PathBuf,
    io: &'a mut IoWork,
    geometry: Option<core::GeometryCounts>,
    static_view: Option<FileId>,
    tape: Option<File>,
    tape_path: PathBuf,
    tape_id: Option<FileId>,
    bits: Option<FileId>,
    payload_sha: Sha256,
    payload_bytes: u64,
    next_query: usize,
    error: Option<Error>,
}
impl<'a> Sink<'a> {
    fn new(target: PathBuf, io: &'a mut IoWork) -> Self {
        Self {
            tape_path: target.join("queries.rbin"),
            target,
            io,
            geometry: None,
            static_view: None,
            tape: None,
            tape_id: None,
            bits: None,
            payload_sha: Sha256::new(),
            payload_bytes: 0,
            next_query: 0,
            error: None,
        }
    }
    fn core_result<T>(&mut self, value: Result<T>) -> rustroad_core::Result<T> {
        value.map_err(|e| {
            self.error = Some(e);
            rustroad_core::Error::Invalid("STREAM_SINK_IO".into())
        })
    }
    fn begin(&mut self, data: &[u8], counts: &core::GeometryCounts) -> Result<()> {
        if self.geometry.is_some()
            || data.len() != counts.static_bytes
            || data.len() > core::STATIC_BYTES
        {
            return Err(invalid("STREAM_STATIC_ORDER"));
        }
        let id = output_file(
            &self.target.join("static.rbin"),
            std::iter::once(data),
            data.len() as u64,
            self.io,
        )?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.tape_path)?;
        file.seek(SeekFrom::Start(HEADER as u64))?;
        self.tape = Some(file);
        self.static_view = Some(id);
        self.geometry = Some(counts.clone());
        Ok(())
    }
    fn payload(&mut self, first: usize, data: &[u8]) -> Result<()> {
        let geometry = self
            .geometry
            .as_ref()
            .ok_or_else(|| invalid("STREAM_QUERY_ORDER"))?;
        if self.tape_id.is_some()
            || first != self.next_query
            || data.is_empty()
            || !data.len().is_multiple_of(40)
            || data.len() > core::TILE_QUERIES * 40
        {
            return Err(invalid("STREAM_QUERY_CHUNK"));
        }
        let after = self
            .payload_bytes
            .checked_add(data.len() as u64)
            .filter(|&n| n <= geometry.query_payload_bytes as u64)
            .ok_or_else(|| budget("STREAM_PAYLOAD_CAP"))?;
        self.tape
            .as_mut()
            .ok_or_else(|| invalid("STREAM_TAPE_FD"))?
            .write_all(data)?;
        self.payload_sha.update(data);
        let newly = blocks(after)?
            .checked_sub(blocks(self.payload_bytes)?)
            .ok_or_else(|| budget("STREAM_PAYLOAD_CARRY"))?;
        control(
            self.io,
            newly
                .checked_mul(2)
                .ok_or_else(|| budget("STREAM_PAYLOAD_WORK"))?,
        )?;
        self.payload_bytes = after;
        self.next_query += data.len() / 40;
        Ok(())
    }
    fn finish_queries(&mut self, total: usize) -> Result<()> {
        let g = self
            .geometry
            .as_ref()
            .ok_or_else(|| invalid("STREAM_QUERY_ORDER"))?;
        if total != g.queries
            || self.next_query != total
            || self.payload_bytes != g.query_payload_bytes as u64
            || self.tape_id.is_some()
        {
            return Err(invalid("STREAM_QUERY_COMPLETE"));
        }
        let mut header = [0u8; HEADER];
        let mut footer = [0u8; HEADER];
        header[..8].copy_from_slice(b"RG1QRY02");
        footer[..8].copy_from_slice(b"RG1END02");
        for record in [&mut header, &mut footer] {
            record[8..12].copy_from_slice(&2u32.to_le_bytes());
            record[12..16].copy_from_slice(&(HEADER as u32).to_le_bytes());
        }
        for (offset, value) in [
            (16, g.tape_bytes as u64),
            (24, total as u64),
            (32, 40),
            (40, self.payload_bytes),
            (112, HEADER as u64 + self.payload_bytes),
        ] {
            header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        let static_id = self
            .static_view
            .as_ref()
            .ok_or_else(|| invalid("STREAM_STATIC_ID"))?;
        let mut static_sha = [0u8; 32];
        for (i, value) in static_sha.iter_mut().enumerate() {
            *value = u8::from_str_radix(&static_id.sha256[i * 2..i * 2 + 2], 16)
                .map_err(|_| invalid("STREAM_STATIC_SHA"))?;
        }
        let payload_sha = self.payload_sha.clone().finalize();
        header[48..80].copy_from_slice(&static_sha);
        header[80..112].copy_from_slice(&payload_sha);
        footer[16..24].copy_from_slice(&(total as u64).to_le_bytes());
        footer[24..32].copy_from_slice(&self.payload_bytes.to_le_bytes());
        footer[32..64].copy_from_slice(&payload_sha);
        footer[64..96].copy_from_slice(&Sha256::digest(header));
        footer[96..128].copy_from_slice(&static_sha);
        let mut file = self.tape.take().ok_or_else(|| invalid("STREAM_TAPE_FD"))?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.seek(SeekFrom::Start(HEADER as u64 + self.payload_bytes))?;
        file.write_all(&footer)?;
        file.sync_all()?;
        if file.metadata()?.len() != g.tape_bytes as u64 {
            return Err(invalid("STREAM_TAPE_LENGTH"));
        }
        drop(file);
        fs::set_permissions(&self.tape_path, fs::Permissions::from_mode(0o444))?;
        // Payload write+SHA uses cumulative carry; header/write+SHA and footer
        // write are three complete framing passes. Encoding has a fixed fee.
        self.io.charge(HEADER as u64, 3)?;
        control(self.io, 2 * HEADER)?;
        self.tape_id = Some(fingerprint(&self.tape_path, g.tape_bytes as u64, self.io)?);
        Ok(())
    }
    fn bits(&mut self, words: &[u64]) -> Result<()> {
        let g = self
            .geometry
            .as_ref()
            .ok_or_else(|| invalid("STREAM_BITS_ORDER"))?;
        if self.tape_id.is_none()
            || self.bits.is_some()
            || words.len() != g.virtual_shapes.div_ceil(64)
        {
            return Err(invalid("STREAM_BITS_COMPLETE"));
        }
        let bytes = (words.len() as u64)
            .checked_mul(8)
            .filter(|&n| n <= BITS_BYTES)
            .ok_or_else(|| budget("STREAM_BITS_CAP"))?;
        self.bits = Some(output_file(
            &self.target.join("legacy-bits.rbin"),
            words.iter().map(|w| w.to_le_bytes()),
            bytes,
            self.io,
        )?);
        self.io.charge(bytes, 1)?;
        Ok(())
    }
    fn artifacts(&self) -> Result<Artifacts> {
        Ok(Artifacts {
            static_view: self
                .static_view
                .clone()
                .ok_or_else(|| invalid("STREAM_NO_STATIC"))?,
            query_tape: self
                .tape_id
                .clone()
                .ok_or_else(|| invalid("STREAM_NO_TAPE"))?,
            legacy_bits: self.bits.clone().ok_or_else(|| invalid("STREAM_NO_BITS"))?,
        })
    }
}
impl core::CaptureSink for Sink<'_> {
    fn static_view(
        &mut self,
        data: &[u8],
        counts: &core::GeometryCounts,
    ) -> rustroad_core::Result<()> {
        let r = self.begin(data, counts);
        self.core_result(r)
    }
    fn query_chunk(&mut self, first: usize, data: &[u8]) -> rustroad_core::Result<()> {
        let r = self.payload(first, data);
        self.core_result(r)
    }
    fn queries_complete(&mut self, total: usize) -> rustroad_core::Result<()> {
        let r = self.finish_queries(total);
        self.core_result(r)
    }
    fn legacy_words(&mut self, words: &[u64]) -> rustroad_core::Result<()> {
        let r = self.bits(words);
        self.core_result(r)
    }
}

#[derive(Debug, Serialize)]
struct SevenCounts {
    instances: usize,
    templates: usize,
    rectangles: usize,
    virtual_shapes: usize,
    bins: usize,
    entries: usize,
    queries: usize,
}
#[derive(Debug, Serialize)]
struct OriginRecord {
    schema: &'static str,
    static_view: FileId,
    query_tape: FileId,
    observer_legacy_bits: FileId,
    counts: SevenCounts,
    provenance: serde_json::Value,
}
/// Conditional envelope and locks survive until stdout and atomic publication.
#[derive(Debug)]
pub struct Emission {
    bytes: Vec<u8>,
    output: PathBuf,
    temporary_head: PathBuf,
    _source_lock: File,
    _output_lock: File,
}
impl Emission {
    /// Precommit output failure leaves no HEAD. Post-rename failures remain
    /// unverified unless the external actual exit0 receipt closes authority.
    pub fn write_stdout(self, output: &mut impl Write) -> Result<()> {
        output.write_all(&self.bytes)?;
        output.flush()?;
        if self.output.join("HEAD").exists() {
            return Err(invalid("REVISION_CONFLICT: stream publication"));
        }
        // Persist the complete immutable namespace before exposing its HEAD.
        File::open(self.output.join("revisions/rev1"))?.sync_all()?;
        File::open(self.output.join("revisions"))?.sync_all()?;
        fs::rename(&self.temporary_head, self.output.join("HEAD"))?;
        File::open(&self.output)?.sync_all()?;
        if read_head(&self.output)? != b"rev1\n" {
            return Err(invalid("STREAM_OUTPUT_HEAD"));
        }
        fs::set_permissions(
            self.output.join("workspace.lock"),
            fs::Permissions::from_mode(0o444),
        )?;
        for directory in [
            self.output.join("revisions/rev1"),
            self.output.join("revisions"),
            self.output.clone(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o500))?;
        }
        Ok(())
    }
}
fn reserve_add(value: &mut usize, bytes: u64, passes: usize) -> Result<()> {
    *value = value
        .checked_add(
            blocks(bytes)?
                .checked_mul(passes)
                .ok_or_else(|| budget("STREAM_IO_OVERFLOW"))?,
        )
        .ok_or_else(|| budget("STREAM_IO_OVERFLOW"))?;
    Ok(())
}
fn validate_metadata_key(
    source: &Path,
    key: &str,
    value: &FileId,
    selected: &BTreeSet<PathBuf>,
) -> Result<()> {
    let relative = Path::new(key);
    if relative.is_absolute()
        || relative
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
        || relative.as_os_str().len() > 4096
        || value.path != source.join(relative)
        || selected.contains(&value.path)
        || matches!(key, "HEAD" | "workspace.lock")
    {
        return Err(invalid("STREAM_METADATA_PATH"));
    }
    frozen(value, 1 << 20)
}

/// Explicit v2 entry, separate from the unchanged v1 tiny command.
pub fn run(
    output: &Path,
    base_revision: &str,
    inputs: &Path,
    expected_sha: &str,
    limits: &Limits,
    global_threads: usize,
) -> Result<Emission> {
    let tick = Instant::now();
    if base_revision != "empty" {
        return Err(invalid("REVISION_CONFLICT: stream requires empty"));
    }
    if global_threads != 1 {
        return Err(budget("STREAM_GLOBAL_THREADS"));
    }
    let mut actual_argv = Vec::new();
    let mut argv_bytes = 0usize;
    for item in std::env::args_os() {
        let arg = item
            .into_string()
            .map_err(|_| invalid("STREAM_ARGV_UTF8"))?;
        argv_bytes = argv_bytes
            .checked_add(arg.len())
            .ok_or_else(|| budget("STREAM_ARGV"))?;
        if actual_argv.len() >= 64 || arg.len() > 4096 || argv_bytes > 16 << 10 {
            return Err(budget("STREAM_ARGV"));
        }
        actual_argv.push(arg);
    }
    let actual_cwd = plain(&std::env::current_dir()?, true)?;
    plain(inputs, true)?;
    let initial_job_state = state(inputs)?;
    let initial_job_parent = job_parent_state(inputs)?;
    if initial_job_state.len > JOB_BYTES as u64
        || initial_job_state.mode & 0o222 != 0
        || expected_sha.len() != 64
        || !expected_sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(budget("STREAM_BOOTSTRAP"));
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(JOB_BYTES + 1)
        .map_err(|_| budget("STREAM_JOB_ALLOCATION"))?;
    File::open(inputs)?
        .take((JOB_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
    if raw.len() as u64 != initial_job_state.len
        || format!("{:x}", Sha256::digest(&raw)) != expected_sha
        || state(inputs)? != initial_job_state
    {
        return Err(invalid("STREAM_JOB_SHA_OR_STATE"));
    }
    // Bound the actual output evidence before any large hash/lowering. The
    // remaining fixed proof fields have an independently reviewed32KiB bound.
    if raw
        .len()
        .checked_add(argv_bytes)
        .and_then(|n| n.checked_add(32 << 10))
        .is_none_or(|n| n > RECEIPT_BYTES)
    {
        return Err(budget("STREAM_PROVENANCE_PREFLIGHT"));
    }
    let job = strict_job(&raw)?;
    let job_id = FileId {
        path: inputs.to_path_buf(),
        sha256: expected_sha.into(),
        bytes: raw.len() as u64,
    };
    if job.schema != "rustroad.legacy-crossings-stream-inputs.v2"
        || job.protocol_sha256 != protocol_sha256()
        || !matches!(job.corner.as_str(), "min" | "nom" | "max")
        || job.source_revision.len() > 32
        || job.source_files.len() != FILES.len()
        || FILES.iter().any(|n| !job.source_files.contains_key(*n))
        || job.source_metadata.len() > 64
    {
        return Err(invalid("STREAM_JOB_SCHEMA"));
    }
    if job.workspace_limits.max_cells != limits.max_cells
        || job.workspace_limits.max_rows != limits.max_rows
        || job.workspace_limits.max_expansions != limits.max_expansions
    {
        return Err(budget("STREAM_WORKSPACE_PARAMETER_MISMATCH"));
    }
    crate::revision_number(&job.source_revision)?;
    let source = plain(&job.source_workspace, true)?;
    let output = plain(output, false)?;
    if source.starts_with(&output)
        || output.starts_with(&source)
        || inputs.starts_with(&output)
        || inputs.starts_with(&source)
    {
        return Err(invalid("STREAM_WORKSPACE_ALIAS"));
    }
    let source_lock = crate::lock(&source, false, false)?;
    let head_before = read_head(&source)?;
    if head_before != format!("{}\n", job.source_revision).as_bytes() {
        return Err(invalid("REVISION_CONFLICT: stream private source HEAD"));
    }
    let revision = source.join("revisions").join(&job.source_revision);
    let mut declared = BTreeSet::new();
    let mut source_total = 0u64;
    for name in FILES {
        let value = &job.source_files[name];
        frozen(value, 64 << 20)?;
        if value.path != revision.join(name) {
            return Err(invalid("STREAM_SOURCE_PATH"));
        }
        declared.insert(value.path.clone());
        source_total = source_total
            .checked_add(value.bytes)
            .ok_or_else(|| budget("STREAM_SOURCE_BYTES"))?;
    }
    let mut auxiliary = 0u64;
    for (key, value) in &job.source_metadata {
        validate_metadata_key(&source, key, value, &declared)?;
        auxiliary = auxiliary
            .checked_add(value.bytes)
            .filter(|&n| n <= 1 << 20)
            .ok_or_else(|| budget("STREAM_METADATA_BYTES"))?;
        declared.insert(value.path.clone());
    }
    source_total
        .checked_add(auxiliary)
        .and_then(|n| n.checked_add(64))
        .filter(|&n| n <= SOURCE_BYTES)
        .ok_or_else(|| budget("STREAM_SOURCE_CACHE"))?;
    declared.insert(source.join("HEAD"));
    declared.insert(source.join("workspace.lock"));
    if source_inventory(&source)? != declared {
        return Err(invalid("STREAM_COMPLETE_SOURCE_INVENTORY"));
    }
    if state(&source.join("workspace.lock"))?.len != 0 {
        return Err(invalid("STREAM_SOURCE_LOCK"));
    }
    frozen(&job.profile, 4 << 20)?;
    frozen(&job.producer, 128 << 20)?;
    frozen(&job.source_manifest, 2 << 20)?;
    if std::env::current_exe()?.canonicalize()? != job.producer.path {
        return Err(invalid("STREAM_ACTUAL_ELF"));
    }
    let binding_bytes = cache_pages(job.profile.bytes)?
        .checked_add(cache_pages(job.source_manifest.bytes)?)
        .and_then(|n| n.checked_add(cache_pages(job_id.bytes).ok()?))
        .filter(|&n| n <= 8 << 20)
        .ok_or_else(|| budget("STREAM_BINDING_CACHE"))?;
    let source_cache = job
        .source_files
        .values()
        .chain(job.source_metadata.values())
        .try_fold(4096u64, |sum, v| {
            sum.checked_add(cache_pages(v.bytes)?)
                .ok_or_else(|| budget("STREAM_SOURCE_CACHE_PAGES"))
        })?;
    if source_cache > SOURCE_BYTES {
        return Err(budget("STREAM_SOURCE_CACHE_PAGES"));
    }
    let snapshot_estimate = job.source_files["design.json"]
        .bytes
        .saturating_add(auxiliary)
        .saturating_mul(16)
        .saturating_add(job.source_files["routes.rbin"].bytes.saturating_mul(2))
        .saturating_add(256 << 20)
        .saturating_add(core::MAX_LIVE)
        .saturating_add(source_cache.saturating_mul(2))
        .saturating_add(cache_pages(job.producer.bytes)?)
        .saturating_add(binding_bytes)
        .saturating_add(128 << 20)
        .saturating_add(1 << 20);
    if snapshot_estimate > job.limits.memory_mib.saturating_mul(1 << 20) {
        return Err(budget("STREAM_SNAPSHOT_ADMISSION"));
    }
    let mut all_inputs = job
        .source_files
        .values()
        .chain(job.source_metadata.values())
        .cloned()
        .collect::<Vec<_>>();
    if all_inputs.len() > 71 {
        return Err(budget("STREAM_INPUT_FILE_COUNT"));
    }
    all_inputs.extend([
        job.profile.clone(),
        job.producer.clone(),
        job.source_manifest.clone(),
        job_id.clone(),
    ]);
    let mut initial_states = BTreeMap::new();
    let mut initial_inodes = BTreeSet::new();
    for path in all_inputs
        .iter()
        .map(|value| value.path.clone())
        .chain([source.join("HEAD"), source.join("workspace.lock")])
    {
        let before = state(&path)?;
        if !initial_inodes.insert((before.dev, before.ino))
            || initial_states.insert(path, before).is_some()
        {
            return Err(invalid("STREAM_EFFECTIVE_INPUT_ALIAS"));
        }
    }
    let mut reserved = 4096usize;
    for value in job
        .source_files
        .values()
        .chain(job.source_metadata.values())
    {
        reserve_add(&mut reserved, value.bytes, 5)?;
    }
    let payload_cap = (core::MAX_QUERIES as u64)
        .checked_mul(40)
        .ok_or_else(|| budget("STREAM_TAPE_WORK"))?;
    let tape_cap = payload_cap + 2 * HEADER as u64;
    for (bytes, passes) in [
        (job_id.bytes, 7),
        (64, 4),
        (job.profile.bytes, 7),
        (job.producer.bytes, 4),
        (job.source_manifest.bytes, 4),
        (core::STATIC_BYTES as u64, 6),
        (payload_cap, 2),
        (HEADER as u64, 3),
        (tape_cap, 4),
        (BITS_BYTES, 7),
        (PROTOCOL.len() as u64, 1),
    ] {
        reserve_add(&mut reserved, bytes, passes)?;
    }
    // The scalar preparse/unique-tree passes and framing encode have explicit
    // bounded fees, distinct from old loader logical fees or original work.
    let metadata_control = 2 * JOB_BYTES + 8 * 4096 + 2 * HEADER;
    reserved = reserved
        .checked_add(metadata_control)
        .ok_or_else(|| budget("STREAM_METADATA_WORK"))?;
    let tail = 2 * RECEIPT_BYTES
        + 4 * blocks(RECEIPT_BYTES as u64)?
        + 2 * STDOUT_BYTES
        + blocks(STDOUT_BYTES as u64)?
        + 4
        + 4;
    reserved = reserved
        .checked_add(tail)
        .ok_or_else(|| budget("STREAM_IO_TAIL"))?;
    let core_limits = job.limits.checked(reserved)?;
    let mut io = IoWork {
        used: 4096,
        reserved,
    };
    io.charge(job_id.bytes, 3)?;
    control(&mut io, raw.len() + 2 * 4096)?;
    verify(&job_id, JOB_BYTES as u64, &mut io)?;
    let source_head = fingerprint(&source.join("HEAD"), 64, &mut io)?;
    for value in job
        .source_files
        .values()
        .chain(job.source_metadata.values())
    {
        verify(value, 64 << 20, &mut io)?;
    }
    verify(&job.profile, 4 << 20, &mut io)?;
    verify(&job.producer, 128 << 20, &mut io)?;
    verify(&job.source_manifest, 2 << 20, &mut io)?;
    io.charge(PROTOCOL.len() as u64, 1)?;
    if output.exists() {
        if output.join("HEAD").exists() {
            return Err(invalid("REVISION_CONFLICT: stream output"));
        }
        if fs::read_dir(&output)?.next().transpose()?.is_some() {
            return Err(invalid("STREAM_OUTPUT_NOT_EMPTY"));
        }
    } else {
        fs::create_dir(&output)?;
    }
    let output_lock = crate::lock(&output, true, true)?;
    if output.join("HEAD").exists() {
        return Err(invalid("REVISION_CONFLICT: stream output"));
    }
    let revisions = output.join("revisions");
    fs::create_dir(&revisions)?;
    let target = revisions.join("rev1");
    fs::create_dir(&target)?;
    let phase_inputs = core::PhaseInputs {
        source_bytes_per_copy: source_cache,
        producer_elf_bytes: cache_pages(job.producer.bytes)?,
        binding_file_bytes: binding_bytes,
    };
    let request = ExtractRequest {
        profile: job.profile.path.clone(),
        corner: job.corner.clone(),
        out: output.join("unused.spef"),
        max_spef_bytes: 1,
        limits: core_limits,
    };
    let mut sink = Sink::new(target.clone(), &mut io);
    let result = with_preprocessed(
        &source,
        &request,
        limits,
        Some(job.limits.max_segment_um),
        |view, _snapshot, _profile, _pin, request, _out| {
            Ok(core::capture_cpu_ref(
                view,
                &request.limits,
                phase_inputs,
                &mut sink,
            )?)
        },
    );
    let counts = match result {
        Ok(v) => v,
        Err(error) => return Err(sink.error.take().unwrap_or(error)),
    };
    let artifacts = sink.artifacts()?;
    drop(sink);
    verify(&artifacts.static_view, core::STATIC_BYTES as u64, &mut io)?;
    verify(&artifacts.query_tape, core::TAPE_BYTES as u64, &mut io)?;
    verify(&artifacts.legacy_bits, BITS_BYTES, &mut io)?;
    for value in job
        .source_files
        .values()
        .chain(job.source_metadata.values())
    {
        io.charge(value.bytes, 1)?;
    }
    io.charge(job.profile.bytes, 3)?;
    for value in job
        .source_files
        .values()
        .chain(job.source_metadata.values())
    {
        verify(value, 64 << 20, &mut io)?;
    }
    verify(&job.profile, 4 << 20, &mut io)?;
    verify(&job.producer, 128 << 20, &mut io)?;
    verify(&job.source_manifest, 2 << 20, &mut io)?;
    verify(&job_id, JOB_BYTES as u64, &mut io)?;
    verify(&source_head, 64, &mut io)?;
    for (value, before) in &initial_states {
        if state(value)? != *before {
            return Err(invalid("STREAM_INPUT_STATE_CHANGED"));
        }
    }
    if job_parent_state(inputs)? != initial_job_parent {
        return Err(invalid("STREAM_JOB_PARENT_CHANGED"));
    }
    if read_head(&source)? != head_before || source_inventory(&source)? != declared {
        return Err(invalid("STREAM_SOURCE_CHANGED"));
    }
    let io_on_success = io
        .used
        .checked_add(tail)
        .filter(|&n| n <= io.reserved)
        .ok_or_else(|| budget("STREAM_IO_ON_SUCCESS"))?;
    let total_on_success = counts
        .legacy_work_after
        .checked_add(counts.control_completed)
        .and_then(|n| n.checked_add(io_on_success))
        .filter(|&n| n <= job.limits.max_work)
        .ok_or_else(|| budget("STREAM_GLOBAL_100B"))?;
    let seven = SevenCounts {
        instances: counts.geometry.instances,
        templates: counts.geometry.templates,
        rectangles: counts.geometry.rectangles,
        virtual_shapes: counts.geometry.virtual_shapes,
        bins: counts.geometry.bins,
        entries: counts.geometry.entries,
        queries: counts.geometry.queries,
    };
    let mut provenance = serde_json::Map::new();
    for part in [
        serde_json::json!({"schema":"rustroad.legacy-crossings-stream-capture.v2",
            "status":"captured_original_serial_boundary_conditional","producer":job.producer,
            "producer_pid":std::process::id(),"producer_argv":actual_argv,"producer_cwd":actual_cwd,
            "source_manifest":job.source_manifest,"protocol_sha256":job.protocol_sha256,"job":job_id}),
        serde_json::json!({"source_workspace":source,"source_revision":job.source_revision,
            "source_HEAD_before_after":source_head,"private_source_unchanged":true,"job_parent_sealed_private_unchanged":true,
            "source_files_before_after":job.source_files,"source_metadata_before_after":job.source_metadata,
            "profile":job.profile,"corner":job.corner,"limits":job.limits,"workspace_limits":job.workspace_limits}),
        serde_json::json!({"actual_serial":counts,"origin_evidence":job.origin,
            "published_copy_lineage_status":"unchecked_requires_independent_verifier",
            "global_threads":1,"query_workers":1,"crossing_workers":1,"native_processes":1,
            "component_status":"unchecked","physical_acceptance":false}),
        serde_json::json!({"io_completed_before_receipt":io.used,"io_reserved_work":io.reserved,
            "io_work_on_success":io_on_success,"total_work_on_success":total_on_success,
            "in_process_wall_seconds_before_receipt":tick.elapsed().as_secs_f64(),
            "io_scope":"defined64B read/write/SHA passes with payload carry; explicit framing/JSON/node fees; existing loader per-file fee not exhaustive CPU/IO",
            "success_authority":"actual exit0 plus complete typed stdout, six-file inventory and HEADrev1"}),
    ] {
        let serde_json::Value::Object(fields) = part else {
            return Err(invalid("STREAM_PROVENANCE_OBJECT"));
        };
        provenance.extend(fields);
    }
    let record = OriginRecord {
        schema: "rusteda.g1-road-stream-capture.v2",
        static_view: artifacts.static_view,
        query_tape: artifacts.query_tape,
        observer_legacy_bits: artifacts.legacy_bits,
        counts: seven,
        provenance: provenance.into(),
    };
    let receipt = encoded(&record, RECEIPT_BYTES)?;
    preparse(&receipt)?;
    control(&mut io, 2 * RECEIPT_BYTES)?;
    let result = output_file(
        &target.join("capture.json"),
        std::iter::once(receipt.as_slice()),
        RECEIPT_BYTES as u64,
        &mut io,
    )?;
    let envelope = encoded(
        &serde_json::json!({"schema":"rustroad.legacy-crossings-stream-envelope.v2",
        "status":"staged_conditional_until_actual_exit0","result":result,"revision":"rev1"}),
        STDOUT_BYTES,
    )?;
    preparse(&envelope)?;
    control(&mut io, 2 * STDOUT_BYTES)?;
    let temporary_head = output.join("HEAD.tmp");
    output_file(
        &temporary_head,
        std::iter::once(b"rev1\n".as_slice()),
        5,
        &mut io,
    )?;
    let remaining = blocks(STDOUT_BYTES as u64)?
        .checked_add(4)
        .ok_or_else(|| budget("STREAM_PUBLISH_TAIL"))?;
    if io.used.checked_add(remaining) != Some(io_on_success) {
        return Err(budget("STREAM_FINAL_LEDGER"));
    }
    Ok(Emission {
        bytes: envelope,
        output,
        temporary_head,
        _source_lock: source_lock,
        _output_lock: output_lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rustroad-stream-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            for path in [
                self.0.clone(),
                self.0.join("output"),
                self.0.join("output/revisions"),
                self.0.join("output/revisions/rev1"),
                self.0.join("jobdir"),
            ] {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn geometry() -> core::GeometryCounts {
        core::GeometryCounts {
            instances: 1,
            templates: 1,
            rectangles: 1,
            virtual_shapes: 1,
            bins: 1,
            entries: 1,
            queries: 2,
            virtual_front: 0,
            prefix_count: 2,
            grid_origin_bins: [0, 0],
            grid_width: 1,
            grid_height: 1,
            coarse_bin_dbu: 16,
            layer_count: 1,
            routing_layers: vec![0],
            static_bytes: HEADER,
            query_payload_bytes: 80,
            tape_bytes: 2 * HEADER + 80,
        }
    }
    fn unique(data: &[u8]) -> bool {
        preparse(data).is_ok() && serde_json::from_slice::<UniqueValue>(data).is_ok()
    }

    #[test]
    fn strict_json_rejects_nested_duplicates_and_exponent_overflow() {
        assert!(unique(br#"{"origin":{"a":1},"finite":1.25}"#));
        assert!(!unique(br#"{"origin":{"a":1,"a":2}}"#));
        assert!(!unique(br#"{"origin":[{"a":1,"a":2}]}"#));
        for text in [b"1e999".as_slice(), b"NaN", b"Infinity", b"-Infinity"] {
            assert!(!unique(text));
        }
    }

    #[test]
    fn tree_bounds_apply_before_any_value_decode() {
        assert!(preparse(format!("{}0{}", "[".repeat(32), "]".repeat(32)).as_bytes()).is_ok());
        assert!(preparse(format!("{}0{}", "[".repeat(33), "]".repeat(33)).as_bytes()).is_err());
        assert!(preparse(format!("[{}0]", "0,".repeat(4096)).as_bytes()).is_err());
        assert!(preparse(&vec![b' '; JOB_BYTES + 1]).is_err());
        assert!(preparse(br#"{"open":"unterminated}"#).is_err());
    }

    #[test]
    fn cache_rounding_and_cumulative_block_carry_do_not_reset_per_tile() {
        assert_eq!(cache_pages(0).unwrap(), 0);
        assert_eq!(cache_pages(4097).unwrap(), 8192);
        assert!(cache_pages(u64::MAX).is_err());
        let chunks = [40u64, 40, 40, 40];
        let (mut total, mut charged) = (0, 0);
        for chunk in chunks {
            charged += blocks(total + chunk).unwrap() - blocks(total).unwrap();
            total += chunk;
        }
        assert_eq!(charged, blocks(160).unwrap());
        assert_ne!(charged, 4 * blocks(40).unwrap());
    }

    #[test]
    fn stream_framing_joins_exact_static_payload_and_header_hashes() {
        let scratch = Scratch::new();
        let mut io = IoWork {
            used: 0,
            reserved: 1_000_000,
        };
        let mut sink = Sink::new(scratch.0.clone(), &mut io);
        let g = geometry();
        sink.begin(&vec![0; HEADER], &g).unwrap();
        let query = [7u8; 40];
        sink.payload(0, &query).unwrap();
        sink.payload(1, &query).unwrap();
        sink.finish_queries(2).unwrap();
        sink.bits(&[1]).unwrap();
        let artifacts = sink.artifacts().unwrap();
        drop(sink);
        let tape = fs::read(&artifacts.query_tape.path).unwrap();
        let footer = HEADER + 80;
        assert_eq!(&tape[..8], b"RG1QRY02");
        assert_eq!(&tape[footer..footer + 8], b"RG1END02");
        assert_eq!(
            u64::from_le_bytes(tape[16..24].try_into().unwrap()),
            tape.len() as u64
        );
        assert_eq!(u64::from_le_bytes(tape[24..32].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_le_bytes(tape[112..120].try_into().unwrap()),
            footer as u64
        );
        assert_eq!(&tape[80..112], &Sha256::digest(&tape[HEADER..footer])[..]);
        assert_eq!(
            &tape[footer + 64..footer + 96],
            &Sha256::digest(&tape[..HEADER])[..]
        );
        assert_eq!(&tape[48..80], &tape[footer + 96..footer + 128]);
        assert!(tape[128..HEADER].iter().all(|&b| b == 0));
        assert!(tape[footer + 128..].iter().all(|&b| b == 0));
    }

    #[test]
    fn sink_rejects_reordered_duplicate_partial_or_oversized_chunks() {
        let scratch = Scratch::new();
        let mut io = IoWork {
            used: 0,
            reserved: 1_000_000,
        };
        let mut sink = Sink::new(scratch.0.clone(), &mut io);
        assert!(sink.payload(0, &[0; 40]).is_err());
        assert!(sink.bits(&[0]).is_err());
        sink.begin(&vec![0; HEADER], &geometry()).unwrap();
        assert!(sink.begin(&vec![0; HEADER], &geometry()).is_err());
        assert!(sink.payload(1, &[0; 40]).is_err());
        assert!(sink.payload(0, &[0; 39]).is_err());
        assert!(sink.payload(0, &[]).is_err());
        assert!(sink.finish_queries(2).is_err());
        sink.payload(0, &[0; 40]).unwrap();
        assert!(sink.payload(0, &[0; 40]).is_err());
        assert!(sink.payload(1, &[0; 80]).is_err());
        assert!(sink.finish_queries(1).is_err());
        assert!(!scratch.0.join("HEAD").exists());
    }

    #[test]
    fn reserved_io_never_wraps_and_is_removed_before_legacy_admission() {
        let mut amount = 7usize;
        assert!(reserve_add(&mut amount, u64::MAX, usize::MAX).is_err());
        let limits = EngineLimits {
            memory_mib: 512,
            max_work: 100,
            max_shapes: 100,
            max_virtual_shapes: 100,
            max_bin_entries: 100,
            max_instance_bin_entries: 100,
            max_nodes: 100,
            max_elements: 100,
            max_couplings: 100,
            max_candidates: 8,
            max_segment_um: 25.0,
            min_coupling_ff: 0.0,
        };
        assert_eq!(limits.checked(99).unwrap().max_work, 1);
        assert!(limits.checked(100).is_err());
        assert!(limits.checked(101).is_err());
        assert_eq!(limits.max_work, 100);
    }

    #[test]
    fn stdout_failure_never_publishes_a_head() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("intentional bounded stdout failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let scratch = Scratch::new();
        let output = scratch.0.join("output");
        fs::create_dir(&output).unwrap();
        let source_lock = File::create(scratch.0.join("source.lock")).unwrap();
        let output_lock = File::create(output.join("workspace.lock")).unwrap();
        let temporary_head = output.join("HEAD.tmp");
        fs::write(&temporary_head, b"rev1\n").unwrap();
        let emission = Emission {
            bytes: b"conditional\n".to_vec(),
            output: output.clone(),
            temporary_head,
            _source_lock: source_lock,
            _output_lock: output_lock,
        };
        assert!(emission.write_stdout(&mut Broken).is_err());
        assert!(!output.join("HEAD").exists());
        assert_eq!(fs::read(output.join("HEAD.tmp")).unwrap(), b"rev1\n");
    }

    #[test]
    fn external_job_parent_must_be_sealed_private_and_contain_only_that_job() {
        let scratch = Scratch::new();
        let parent = scratch.0.join("jobdir");
        fs::create_dir(&parent).unwrap();
        let job = parent.join("inputs.json");
        fs::write(&job, b"{}").unwrap();
        assert!(job_parent_state(&job).is_err());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(job_parent_state(&job).is_ok());
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(parent.join("unexpected"), b"").unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(job_parent_state(&job).is_err());
    }

    #[test]
    fn metadata_cannot_alias_selected_files_or_escape_the_source() {
        let root = Path::new("/private-source");
        let id = FileId {
            path: root.join("HEAD"),
            bytes: 0,
            sha256: "0".repeat(64),
        };
        assert!(validate_metadata_key(root, "HEAD", &id, &BTreeSet::new()).is_err());
        assert!(validate_metadata_key(root, "../other", &id, &BTreeSet::new()).is_err());
        let selected = BTreeSet::from([id.path.clone()]);
        assert!(validate_metadata_key(root, "model.json", &id, &selected).is_err());
    }
}
