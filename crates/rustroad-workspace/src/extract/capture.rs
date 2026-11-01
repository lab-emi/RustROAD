//! Explicit tiny serial capture protocol. It never launches a component worker.
use super::{ExtractRequest, with_preprocessed};
use crate::{Error, Result};
use rustroad_core::Limits;
use rustroad_core::extract::{ExtractLimits, capture as core};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

const JOB_BYTES: usize = 64 << 10;
const RECEIPT_BYTES: usize = 64 << 10;
const STDOUT_BYTES: usize = 8 << 10;
const SOURCE_BYTES: u64 = 128 << 20;
const PACKET_BYTES: u64 = 64 << 20;
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

fn invalid(message: &'static str) -> Error {
    Error::Invalid(message.into())
}
fn budget(message: &'static str) -> Error {
    Error::Budget(message.into())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileId {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Deliberately the original tiny dual-PDK limits, not a large-design endpoint.
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
    fn checked(&self, io_reserve: usize) -> Result<ExtractLimits> {
        if !(1..=512).contains(&self.memory_mib)
            || self.max_work > 200_000_000
            || self.max_shapes > 100_000
            || self.max_virtual_shapes > 1_000_000
            || self.max_bin_entries > 1_000_000
            || self.max_instance_bin_entries > 1_000_000
            || self.max_nodes > 100_000
            || self.max_elements > 1_000_000
            || self.max_couplings > 100_000
            || self.max_candidates > 4096
            || !self.max_segment_um.is_finite()
            || !(0.1..=10_000.0).contains(&self.max_segment_um)
            || !self.min_coupling_ff.is_finite()
            || !(0.0..=1_000_000.0).contains(&self.min_coupling_ff)
        {
            return Err(budget("CAPTURE_TINY_LIMITS"));
        }
        let limits = ExtractLimits {
            memory_mib: self.memory_mib,
            max_work: self
                .max_work
                .checked_sub(io_reserve)
                .ok_or_else(|| budget("CAPTURE_IO_ADMISSION"))?,
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
        limits.validate()?;
        Ok(limits)
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
    #[serde(deserialize_with = "unique_files")]
    source_files: BTreeMap<String, FileId>,
    profile: FileId,
    corner: String,
    producer: FileId,
    source_manifest: FileId,
    limits: EngineLimits,
    workspace_limits: WorkspaceLimits,
}

fn unique_files<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, FileId>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, FileId>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("seven unique source files")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut files = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, FileId>()? {
                if files.len() >= FILES.len() || files.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("CAPTURE_DUPLICATE_SOURCE_FILE"));
                }
            }
            Ok(files)
        }
    }
    deserializer.deserialize_map(Visitor)
}

struct IoWork {
    used: usize,
    reserved: usize,
}
fn blocks(bytes: u64) -> Result<usize> {
    usize::try_from(bytes.div_ceil(64)).map_err(|_| budget("CAPTURE_IO_OVERFLOW"))
}
impl IoWork {
    fn charge(&mut self, bytes: u64, passes: usize) -> Result<()> {
        let n = blocks(bytes)?
            .checked_mul(passes)
            .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?;
        self.used = self
            .used
            .checked_add(n)
            .filter(|&v| v <= self.reserved)
            .ok_or_else(|| budget("CAPTURE_IO_BUDGET"))?;
        Ok(())
    }
}

fn plain(path: &Path, exists: bool) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path.components().count() > 256
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err(invalid("CAPTURE_PATH"));
    }
    for part in path.ancestors() {
        match fs::symlink_metadata(part) {
            Ok(m) if m.file_type().is_symlink() => return Err(invalid("CAPTURE_ALIAS")),
            Ok(_) => {}
            Err(e) if !exists && part == path && e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path.to_path_buf())
}
fn shape(value: &FileId, max: u64) -> Result<()> {
    plain(&value.path, true)?;
    if value.bytes > max
        || value.sha256.len() != 64
        || !value
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid("CAPTURE_FILE_ID"));
    }
    let m = fs::symlink_metadata(&value.path)?;
    if !m.is_file() || m.nlink() != 1 || m.len() != value.bytes {
        return Err(invalid("CAPTURE_FILE_SHAPE"));
    }
    Ok(())
}
fn fingerprint(path: &Path, cap: u64, io: &mut IoWork) -> Result<FileId> {
    plain(path, true)?;
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.nlink() != 1 || before.len() > cap {
        return Err(budget("CAPTURE_FILE_CAP"));
    }
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (before.dev(), before.ino()) {
        return Err(invalid("CAPTURE_OPEN_RACE"));
    }
    let mut hash = Sha256::new();
    let mut left = before.len();
    let mut buffer = [0u8; 64 << 10];
    while left > 0 {
        let n = usize::try_from(left.min(buffer.len() as u64))
            .map_err(|_| budget("CAPTURE_READ_SIZE"))?;
        file.read_exact(&mut buffer[..n])?;
        hash.update(&buffer[..n]);
        left -= n as u64;
    }
    if file.read(&mut buffer[..1])? != 0 {
        return Err(invalid("CAPTURE_FILE_GREW"));
    }
    let after = fs::symlink_metadata(path)?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
    ) {
        return Err(invalid("CAPTURE_FILE_CHANGED"));
    }
    io.charge(before.len(), 2)?;
    Ok(FileId {
        path: path.to_path_buf(),
        sha256: format!("{:x}", hash.finalize()),
        bytes: before.len(),
    })
}
fn verify(value: &FileId, cap: u64, io: &mut IoWork) -> Result<()> {
    if fingerprint(&value.path, cap, io)? != *value {
        return Err(invalid("CAPTURE_FILE_CHANGED"));
    }
    Ok(())
}
fn read_head(path: &Path) -> Result<Vec<u8>> {
    let head = plain(&path.join("HEAD"), true)?;
    let metadata = fs::symlink_metadata(&head)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid("CAPTURE_HEAD_ALIAS"));
    }
    let mut bytes = Vec::new();
    File::open(head)?.take(65).read_to_end(&mut bytes)?;
    if bytes.len() > 64 {
        return Err(budget("CAPTURE_HEAD"));
    }
    Ok(bytes)
}

struct FixedJson {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for FixedJson {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(data.len())
            .is_none_or(|v| v > self.cap)
        {
            return Err(std::io::Error::other("CAPTURE_JSON_CAP"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encoded(value: &impl Serialize, size: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| budget("CAPTURE_JSON_ALLOCATION"))?;
    let mut writer = FixedJson { bytes, cap: size };
    serde_json::to_writer(&mut writer, value).map_err(|_| budget("CAPTURE_JSON_CAP"))?;
    if writer.bytes.len() >= size {
        return Err(budget("CAPTURE_JSON_CAP"));
    }
    writer.bytes.resize(size, b' ');
    writer.bytes[size - 1] = b'\n';
    Ok(writer.bytes)
}
fn output_file(
    path: &Path,
    chunks: impl Iterator<Item = impl AsRef<[u8]>>,
    bytes: u64,
    io: &mut IoWork,
) -> Result<FileId> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut hash = Sha256::new();
    let mut total = 0u64;
    for data in chunks {
        let data = data.as_ref();
        total = total
            .checked_add(data.len() as u64)
            .filter(|&v| v <= bytes)
            .ok_or_else(|| budget("CAPTURE_OUTPUT_CAP"))?;
        file.write_all(data)?;
        hash.update(data);
    }
    if total != bytes {
        return Err(invalid("CAPTURE_OUTPUT_LENGTH"));
    }
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    io.charge(bytes, 2)?;
    let id = FileId {
        path: path.to_path_buf(),
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    };
    verify(&id, bytes, io)?;
    Ok(id)
}

/// Holds both workspace locks through the exact bounded stdout emission.
#[derive(Debug)]
pub struct Emission {
    bytes: Vec<u8>,
    _source_lock: File,
    _output_lock: File,
}
impl Emission {
    /// Emit exactly8192 bytes while the original source remains locked shared.
    /// An outer command receipt must additionally require actual process exit0.
    ///
    /// # Errors
    /// Returns an I/O error if the complete stdout cannot be written/flushed.
    pub fn write_stdout(self, writer: &mut impl Write) -> Result<()> {
        writer.write_all(&self.bytes)?;
        writer.flush()?;
        Ok(())
    }
}

/// Execute only the new tiny capture entry; stdout bytes are fixed and bounded.
/// The caller must write the returned bytes exactly and require actual exit0.
pub fn run(
    output: &Path,
    base_revision: &str,
    inputs: &Path,
    expected_sha: &str,
    limits: &Limits,
    global_threads: usize,
) -> Result<Emission> {
    let tick = Instant::now();
    let mut actual_argv = Vec::new();
    let mut argv_bytes = 0usize;
    for argument in std::env::args_os().take(65) {
        let arg = argument
            .into_string()
            .map_err(|_| invalid("CAPTURE_ARGV_UTF8"))?;
        argv_bytes = argv_bytes
            .checked_add(arg.len())
            .ok_or_else(|| budget("CAPTURE_ARGV_CAP"))?;
        if actual_argv.len() == 64 || argv_bytes > 16 << 10 || arg.len() > 4096 {
            return Err(budget("CAPTURE_ARGV_CAP"));
        }
        actual_argv.push(arg);
    }
    let actual_cwd = plain(&std::env::current_dir()?, true)?;
    if base_revision != "empty" {
        return Err(invalid("REVISION_CONFLICT: capture requires empty"));
    }
    if global_threads != 1 {
        return Err(budget("CAPTURE_GLOBAL_THREADS"));
    }
    plain(inputs, true)?;
    let before = fs::symlink_metadata(inputs)?;
    if !before.is_file()
        || before.nlink() != 1
        || before.len() > JOB_BYTES as u64
        || expected_sha.len() != 64
        || !expected_sha
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(budget("CAPTURE_BOOTSTRAP"));
    }
    let mut file = File::open(inputs)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(JOB_BYTES + 1)
        .map_err(|_| budget("CAPTURE_BOOTSTRAP_ALLOCATION"))?;
    Read::by_ref(&mut file)
        .take((JOB_BYTES + 1) as u64)
        .read_to_end(&mut raw)?;
    if raw.len() as u64 != before.len() || format!("{:x}", Sha256::digest(&raw)) != expected_sha {
        return Err(invalid("CAPTURE_JOB_SHA"));
    }
    let job: Job = serde_json::from_slice(&raw).map_err(|_| invalid("CAPTURE_JOB_SCHEMA"))?;
    let job_id = FileId {
        path: inputs.to_path_buf(),
        sha256: expected_sha.into(),
        bytes: raw.len() as u64,
    };
    if job.schema != "rustroad.legacy-crossings-inputs.v1"
        || job.source_revision.len() > 32
        || !matches!(job.corner.as_str(), "min" | "nom" | "max")
        || job.source_files.len() != FILES.len()
        || FILES
            .iter()
            .any(|name| !job.source_files.contains_key(*name))
    {
        return Err(invalid("CAPTURE_JOB_SCHEMA"));
    }
    if job.workspace_limits.max_cells > 2000
        || job.workspace_limits.max_rows > 128
        || job.workspace_limits.max_expansions > 4_000_000
        || job.workspace_limits.max_cells != limits.max_cells
        || job.workspace_limits.max_rows != limits.max_rows
        || job.workspace_limits.max_expansions != limits.max_expansions
    {
        return Err(budget("CAPTURE_WORKSPACE_LIMITS"));
    }
    crate::revision_number(&job.source_revision)?;
    let source = plain(&job.source_workspace, true)?;
    let output = plain(output, false)?;
    if source.starts_with(&output) || output.starts_with(&source) || inputs.starts_with(&output) {
        return Err(invalid("CAPTURE_WORKSPACE_ALIAS"));
    }
    let _source_lock = crate::lock(&source, false, false)?;
    let head_before = read_head(&source)?;
    if head_before != format!("{}\n", job.source_revision).as_bytes() {
        return Err(invalid("REVISION_CONFLICT: capture source HEAD"));
    }
    let revision = source.join("revisions").join(&job.source_revision);
    let mut source_total = 0u64;
    for name in FILES {
        let value = &job.source_files[name];
        shape(value, 64 << 20)?;
        if value.path != revision.join(name) {
            return Err(invalid("CAPTURE_SOURCE_PATH"));
        }
        source_total = source_total
            .checked_add(value.bytes)
            .filter(|&v| v <= SOURCE_BYTES)
            .ok_or_else(|| budget("CAPTURE_SNAPSHOT_BYTES"))?;
    }
    let mut inventory = 0usize;
    for entry in fs::read_dir(&revision)? {
        inventory += 1;
        let entry = entry?;
        if inventory > FILES.len()
            || !FILES
                .iter()
                .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
        {
            return Err(invalid("CAPTURE_REVISION_INVENTORY"));
        }
    }
    if inventory != FILES.len() {
        return Err(invalid("CAPTURE_REVISION_INVENTORY"));
    }
    shape(&job.profile, 4 << 20)?;
    shape(&job.producer, 128 << 20)?;
    shape(&job.source_manifest, 2 << 20)?;
    let actual_exe = std::env::current_exe()?.canonicalize()?;
    if actual_exe != job.producer.path {
        return Err(invalid("CAPTURE_ACTUAL_ELF"));
    }
    let metadata = job.source_files["design.json"].bytes;
    let routes = job.source_files["routes.rbin"].bytes;
    let snapshot_estimate = metadata
        .saturating_mul(16)
        .saturating_add(routes.saturating_mul(2))
        .saturating_add(256 << 20)
        .saturating_add(64 << 20);
    if snapshot_estimate > job.limits.memory_mib.saturating_mul(1 << 20) {
        return Err(budget("CAPTURE_SNAPSHOT_ADMISSION"));
    }
    let mut reserved = 4096usize;
    for value in job.source_files.values() {
        reserved = reserved
            .checked_add(
                blocks(value.bytes)?
                    .checked_mul(5)
                    .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?,
            )
            .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?;
    }
    for (bytes, passes) in [
        (job_id.bytes, 7),
        (64, 4),
        (job.profile.bytes, 7),
        (job.producer.bytes, 4),
        (job.source_manifest.bytes, 4),
        (PACKET_BYTES, 4),
        (BITS_BYTES, 5),
    ] {
        reserved = reserved
            .checked_add(
                blocks(bytes)?
                    .checked_mul(passes)
                    .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?,
            )
            .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?;
    }
    // Fixed receipt/envelope sizes remove length/self-hash accounting cycles.
    let tail = RECEIPT_BYTES
        + 4 * blocks(RECEIPT_BYTES as u64)?
        + STDOUT_BYTES
        + blocks(STDOUT_BYTES as u64)?
        + 4;
    reserved = reserved
        .checked_add(tail)
        .ok_or_else(|| budget("CAPTURE_IO_OVERFLOW"))?;
    let core_limits = job.limits.checked(reserved)?;
    let mut io = IoWork {
        used: 4096,
        reserved,
    };
    io.charge(job_id.bytes, 3)?;
    verify(&job_id, JOB_BYTES as u64, &mut io)?;
    let source_head = fingerprint(&source.join("HEAD"), 64, &mut io)?;
    for value in job.source_files.values() {
        verify(value, 64 << 20, &mut io)?;
    }
    verify(&job.profile, 4 << 20, &mut io)?;
    verify(&job.producer, 128 << 20, &mut io)?;
    verify(&job.source_manifest, 2 << 20, &mut io)?;
    if output.exists() {
        if output.join("HEAD").exists() {
            return Err(invalid("REVISION_CONFLICT: capture output"));
        }
        let mut items = fs::read_dir(&output)?;
        if items.next().transpose()?.is_some() {
            return Err(invalid("CAPTURE_OUTPUT_NOT_EMPTY"));
        }
    } else {
        fs::create_dir(&output)?;
    }
    let _output_lock = crate::lock(&output, true, true)?;
    if output.join("HEAD").exists() {
        return Err(invalid("REVISION_CONFLICT: capture output"));
    }
    let revisions = output.join("revisions");
    fs::create_dir(&revisions)?;
    let target = revisions.join("rev1");
    fs::create_dir(&target)?;
    let request = ExtractRequest {
        profile: job.profile.path.clone(),
        corner: job.corner.clone(),
        out: output.join("unused.spef"),
        max_spef_bytes: 1,
        limits: core_limits,
    };
    let capture = with_preprocessed(
        &source,
        &request,
        limits,
        Some(job.limits.max_segment_um),
        |view, _snapshot, _profile, _pin, request, _out| {
            Ok(core::capture_cpu_ref(view, &request.limits)?)
        },
    )?;
    // Defined per-file loader fees, not an exhaustive physical read/CPU count.
    // The unchanged loader's graph/model scans remain bounded by its own limits.
    let mut shared_loader_fee = BTreeMap::new();
    for (name, value) in &job.source_files {
        io.charge(value.bytes, 1)?;
        shared_loader_fee.insert(name.clone(), blocks(value.bytes)?);
    }
    // read_input reads and hashes; load_profile also decodes UTF8/Serde.
    io.charge(job.profile.bytes, 3)?;
    let packet = output_file(
        &target.join("view.rbin"),
        std::iter::once(capture.packet.as_slice()),
        capture.packet.len() as u64,
        &mut io,
    )?;
    let bit_bytes = (capture.original_bits.len() as u64)
        .checked_mul(8)
        .filter(|&v| v <= BITS_BYTES)
        .ok_or_else(|| budget("CAPTURE_BITSET_BYTES"))?;
    let bits = output_file(
        &target.join("legacy-bits.rbin"),
        capture.original_bits.iter().map(|w| w.to_le_bytes()),
        bit_bytes,
        &mut io,
    )?;
    io.charge(bit_bytes, 1)?; // explicit fixed-endian word encoding pass
    for value in job.source_files.values() {
        verify(value, 64 << 20, &mut io)?;
    }
    verify(&job.profile, 4 << 20, &mut io)?;
    verify(&job.producer, 128 << 20, &mut io)?;
    verify(&job.source_manifest, 2 << 20, &mut io)?;
    verify(&job_id, JOB_BYTES as u64, &mut io)?;
    verify(&source_head, 64, &mut io)?;
    if read_head(&source)? != head_before {
        return Err(invalid("CAPTURE_SOURCE_HEAD_CHANGED"));
    }
    let io_on_success = io
        .used
        .checked_add(tail)
        .filter(|&v| v <= io.reserved)
        .ok_or_else(|| budget("CAPTURE_IO_TAIL"))?;
    let total_on_success = capture
        .counts
        .legacy_work_after
        .checked_add(capture.counts.control_completed)
        .and_then(|v| v.checked_add(io_on_success))
        .filter(|&v| v <= job.limits.max_work)
        .ok_or_else(|| budget("CAPTURE_TOTAL_WORK"))?;
    let mut report = serde_json::Map::new();
    for part in [
        serde_json::json!({"schema":"rustroad.legacy-crossings-capture.v1","status":"captured_boundary_only",
        "producer":job.producer,"producer_pid":std::process::id(),"producer_argv":actual_argv,"producer_cwd":actual_cwd,"source_manifest":job.source_manifest,"job":job_id,"source_workspace":source,
        "source_revision":job.source_revision,"source_HEAD_before_after":source_head,"source_HEAD_unchanged":true,"source_files_before_after":job.source_files,
        "rc_profile":job.profile,"corner":job.corner,"global_threads":1,"query_workers":1,"crossing_workers":1}),
        serde_json::json!({
        "actual_serial_call_completed":true,"counts":capture.counts,"packet":packet,"raw_original_bitset":bits,
        "original_engine_limits":job.limits,"workspace_limits":job.workspace_limits,"io_completed_before_receipt":io.used,"io_reserved_work":io.reserved,
        "io_work_on_success":io_on_success,"total_work_on_success":total_on_success,
        "work_authority":"requires exact typed stdout, actual exit0 and HEADrev1","bootstrap_bytes":raw.len(),"job_io_passes":7}),
        serde_json::json!({"profile_loader_defined_charges":{"read":1,"SHA":1,"decode":1},"shared_loader_logical_fee_by_file":shared_loader_fee,"bit_encoding_io_work":blocks(bit_bytes)?,
        "io_scope":"Additional capture read/write/SHA/bit-encode in64-byte blocks; named per-file existing-loader fee is not an exhaustive actual read/SHA/CPU count; profile read/SHA/decode3 defined charges; fixed4096 bounded metadata/argv/stat/small-HEAD controls; fixed JSON encoding bytes",
        "in_process_wall_seconds_before_receipt":tick.elapsed().as_secs_f64(),"native_processes":1,
        "component_status":"unchecked","stream_status":"unsupported","drc_status":"unchecked","lvs_status":"unchecked","physical_acceptance":false}),
    ] {
        let serde_json::Value::Object(fields) = part else {
            return Err(budget("CAPTURE_REPORT_OBJECT"));
        };
        report.extend(fields);
    }
    let receipt = encoded(&report, RECEIPT_BYTES)?;
    let result = output_file(
        &target.join("capture.json"),
        std::iter::once(receipt.as_slice()),
        RECEIPT_BYTES as u64,
        &mut io,
    )?;
    let envelope = encoded(
        &serde_json::json!({"schema":"rustroad.legacy-crossings-envelope.v1",
        "status":"captured_boundary_only","result":result,"revision":"rev1"}),
        STDOUT_BYTES,
    )?;
    let head_tmp = output.join("HEAD.tmp");
    let _head = output_file(&head_tmp, std::iter::once(b"rev1\n".as_slice()), 5, &mut io)?;
    fs::rename(&head_tmp, output.join("HEAD"))?;
    File::open(&revisions)?.sync_all()?;
    File::open(&output)?.sync_all()?;
    if read_head(&output)? != b"rev1\n" {
        return Err(invalid("CAPTURE_OUTPUT_HEAD"));
    }
    if io
        .used
        .checked_add(RECEIPT_BYTES + STDOUT_BYTES + blocks(STDOUT_BYTES as u64)?)
        .is_none_or(|v| v != io_on_success)
    {
        return Err(budget("CAPTURE_SUCCESS_LEDGER"));
    }
    Ok(Emission {
        bytes: envelope,
        _source_lock,
        _output_lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Unique {
        #[serde(deserialize_with = "unique_files")]
        files: BTreeMap<String, FileId>,
    }

    #[test]
    fn duplicate_source_file_keys_cannot_override_a_hash_bound_job() {
        let file = r#"{"path":"/bounded","sha256":"00","bytes":0}"#;
        let text = format!(r#"{{"files":{{"design.json":{file},"design.json":{file}}}}}"#);
        assert!(serde_json::from_str::<Unique>(&text).is_err());
        let valid = format!(r#"{{"files":{{"design.json":{file}}}}}"#);
        assert_eq!(
            serde_json::from_str::<Unique>(&valid).unwrap().files.len(),
            1
        );
    }

    #[test]
    fn fixed_receipt_admission_prevents_length_accounting_cycles() {
        let small = serde_json::json!({"status":"conditional"});
        let first = encoded(&small, 256).unwrap();
        let second = encoded(&serde_json::json!({"work_on_success":9999999999u64}), 256).unwrap();
        assert_eq!((first.len(), second.len()), (256, 256));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&first).unwrap(),
            small
        );
        assert!(encoded(&"x".repeat(257), 256).is_err());
    }

    #[test]
    fn io_reservation_is_subtracted_before_core_work_is_available() {
        let budget = EngineLimits {
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
        assert_eq!(budget.checked(99).unwrap().max_work, 1);
        assert!(budget.checked(101).is_err());
        assert_eq!(budget.max_work, 100);
    }
}
