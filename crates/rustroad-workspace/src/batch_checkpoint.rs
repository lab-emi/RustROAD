//! Diagnostic checkpoints of batched routing (`route --checkpoint-dir`,
//! `route --resume`). A checkpoint holds everything a batched route carries
//! from one primary batch to the next. It is bound to the workspace revision,
//! the routing configuration, the scope and the load limits, and its bytes are
//! hashed as a whole. A resumed route repeats the remaining primary batches
//! exactly, but it is never committed: its result is diagnostic evidence only,
//! never a fresh end-to-end run.
//!
//! File: magic `RRBCKP01`, u64 header length, JSON header, little-endian
//! fixed-size records (wires, vias, patches, access, nets, completed
//! obstacles), then the SHA-256 of all preceding bytes.
//!
//! Resource bound: records stream through 1 MiB buffers, so writing holds no
//! copy of the route. A load checks the counts against the route limits and
//! allocates exactly the counted records under the track-router memory cap.

use crate::{Error, Result};
use rustroad_core::route_batches::{BatchCheckpoint, BatchProgress, BatchReport};
use rustroad_core::routing::{Access, Blockage, Patch, RouteLimits, RouteStats, Routes, Via, Wire};
use rustroad_core::{MemoryEstimate, Rect};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"RRBCKP01";
const SCHEMA: &str = "rustroad.batch-checkpoint.v1";
const MAX_HEADER: u64 = 1 << 20;
const BUFFER: usize = 1 << 20;
const WIRE: u64 = 40;
const VIA: u64 = 24;
const PATCH: u64 = 40;
const ACCESS: u64 = 28;
const NET: u64 = 4;
const OBSTACLE: u64 = 40;

/// Inputs a checkpoint is valid for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub workspace_revision: String,
    /// SHA-256 of the revision metadata (which pins its model table).
    pub metadata_sha256: String,
    /// Bytes of every file of the revision.
    pub revision_bytes: u64,
    pub config_sha256: String,
    pub scope: String,
    pub max_cells: usize,
    pub max_rows: usize,
    pub max_expansions: usize,
}

/// Primary batches routed by one executable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub producer_sha256: String,
    pub first_batch: usize,
    pub last_batch: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counts {
    wires: u64,
    vias: u64,
    patches: u64,
    access: u64,
    nets: u64,
    completed: u64,
}
impl Counts {
    fn payload(&self) -> Option<u64> {
        [
            (self.wires, WIRE),
            (self.vias, VIA),
            (self.patches, PATCH),
            (self.access, ACCESS),
            (self.nets, NET),
            (self.completed, OBSTACLE),
        ]
        .iter()
        .try_fold(0u64, |n, &(count, size)| {
            n.checked_add(count.checked_mul(size)?)
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Estimate {
    held: u64,
    search: u64,
    spatial: u64,
    routed: u64,
    check: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: String,
    diagnostic_only: bool,
    binding: Binding,
    lineage: Vec<Segment>,
    batches: usize,
    begin: usize,
    counts: Counts,
    grid_nodes: usize,
    expansions: usize,
    comparisons: usize,
    wire_length_dbu: u64,
    repair_attempts: usize,
    displaced_nets: usize,
    peak_estimate: Estimate,
}

fn format_error(message: impl Into<String>) -> Error {
    Error::Input {
        code: "CHECKPOINT_FORMAT",
        message: message.into(),
    }
}

/// SHA-256 of this executable (checkpoint provenance).
pub(crate) fn producer_sha256() -> Result<String> {
    let mut file = BufReader::with_capacity(BUFFER, File::open(std::env::current_exe()?)?);
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; BUFFER];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

struct Hashing<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}
impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(data)?;
        self.hash.update(&data[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Writes a checkpoint after every `every`-th completed primary batch into a
/// new directory and keeps the `keep` most recent files it wrote.
pub(crate) struct Writer {
    dir: PathBuf,
    every: usize,
    keep: usize,
    binding: Binding,
    lineage: Vec<Segment>,
    producer: String,
    first_batch: usize,
    written: VecDeque<PathBuf>,
}
impl Writer {
    pub(crate) fn create(
        dir: &Path,
        every: usize,
        keep: usize,
        binding: Binding,
        lineage: Vec<Segment>,
        producer: String,
        first_batch: usize,
    ) -> Result<Self> {
        if every == 0 || keep == 0 {
            return Err(Error::Input {
                code: "CHECKPOINT_OUTPUT",
                message: "checkpoint interval and retention must be positive".into(),
            });
        }
        fs::create_dir(dir).map_err(|e| Error::Input {
            code: "CHECKPOINT_OUTPUT",
            message: format!("checkpoint directory {} must be new: {e}", dir.display()),
        })?;
        Ok(Self {
            dir: dir.to_path_buf(),
            every,
            keep,
            binding,
            lineage,
            producer,
            first_batch,
            written: VecDeque::new(),
        })
    }

    pub(crate) fn save(&mut self, p: BatchProgress<'_>) -> Result<()> {
        if !p.report.batches.is_multiple_of(self.every) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let r = p.routes;
        let mut lineage = self.lineage.clone();
        lineage.push(Segment {
            producer_sha256: self.producer.clone(),
            first_batch: self.first_batch,
            last_batch: p.report.batches - 1,
        });
        let estimate = p.report.peak_estimate;
        let header = Header {
            schema: SCHEMA.into(),
            diagnostic_only: true,
            binding: self.binding.clone(),
            lineage,
            batches: p.report.batches,
            begin: p.begin,
            counts: Counts {
                wires: r.wires.len() as u64,
                vias: r.vias.len() as u64,
                patches: r.patches.len() as u64,
                access: r.access.len() as u64,
                nets: r.nets.len() as u64,
                completed: p.completed.len() as u64,
            },
            grid_nodes: r.stats.grid_nodes,
            expansions: r.stats.expansions,
            comparisons: r.stats.comparisons,
            wire_length_dbu: r.stats.wire_length_dbu,
            repair_attempts: p.report.repair_attempts,
            displaced_nets: p.report.displaced_nets,
            peak_estimate: Estimate {
                held: estimate.held,
                search: estimate.search,
                spatial: estimate.spatial,
                routed: estimate.routed,
                check: estimate.check,
            },
        };
        let name = format!("batch-{:05}.rrbckp", p.report.batches);
        let path = self.dir.join(&name);
        let temp = self.dir.join(format!("{name}.tmp"));
        let file = File::options().write(true).create_new(true).open(&temp)?;
        let mut out = Hashing {
            inner: BufWriter::with_capacity(BUFFER, file),
            hash: Sha256::new(),
            bytes: 0,
        };
        let json = serde_json::to_vec(&header)?;
        out.write_all(MAGIC)?;
        out.write_all(&(json.len() as u64).to_le_bytes())?;
        out.write_all(&json)?;
        let net =
            |n: Option<u32>| n.ok_or_else(|| format_error("completed obstacle without a net"));
        for w in &r.wires {
            out.write_all(&w.net.to_le_bytes())?;
            out.write_all(&w.layer.to_le_bytes())?;
            for v in [w.start[0], w.start[1], w.end[0], w.end[1]] {
                out.write_all(&v.to_le_bytes())?;
            }
        }
        for v in &r.vias {
            out.write_all(&v.net.to_le_bytes())?;
            out.write_all(&v.kind.to_le_bytes())?;
            out.write_all(&v.at[0].to_le_bytes())?;
            out.write_all(&v.at[1].to_le_bytes())?;
        }
        for q in &r.patches {
            out.write_all(&q.net.to_le_bytes())?;
            out.write_all(&q.layer.to_le_bytes())?;
            for v in [q.rect.lx, q.rect.ly, q.rect.ux, q.rect.uy] {
                out.write_all(&v.to_le_bytes())?;
            }
        }
        for a in &r.access {
            for v in [a.pin, a.net, a.layer] {
                out.write_all(&v.to_le_bytes())?;
            }
            out.write_all(&a.at[0].to_le_bytes())?;
            out.write_all(&a.at[1].to_le_bytes())?;
        }
        for n in &r.nets {
            out.write_all(&n.to_le_bytes())?;
        }
        for b in p.completed {
            out.write_all(&b.layer.to_le_bytes())?;
            out.write_all(&net(b.net)?.to_le_bytes())?;
            for v in [b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy] {
                out.write_all(&v.to_le_bytes())?;
            }
        }
        let digest = out.hash.clone().finalize();
        out.inner.write_all(&digest)?;
        let bytes = out.bytes + digest.len() as u64;
        let file = out
            .inner
            .into_inner()
            .map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &path)?;
        File::open(&self.dir)?.sync_all()?;
        self.written.push_back(path);
        while self.written.len() > self.keep {
            if let Some(old) = self.written.pop_front() {
                fs::remove_file(old)?;
            }
        }
        if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
            eprintln!(
                "[checkpoint] batches {} begin {} bytes {bytes} seconds {:.3} {}",
                p.report.batches,
                p.begin,
                started.elapsed().as_secs_f64(),
                self.dir.join(&name).display()
            );
        }
        Ok(())
    }
}

struct Hashed<R> {
    inner: R,
    hash: Sha256,
}
impl<R: Read> Hashed<R> {
    fn exact<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0u8; N];
        self.inner
            .read_exact(&mut bytes)
            .map_err(|e| format_error(format!("truncated checkpoint: {e}")))?;
        self.hash.update(bytes);
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.exact()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.exact()?))
    }
    fn rect(&mut self) -> Result<Rect> {
        Ok(Rect {
            lx: self.i64()?,
            ly: self.i64()?,
            ux: self.i64()?,
            uy: self.i64()?,
        })
    }
}

fn vector<T>(count: u64) -> Result<Vec<T>> {
    let count = usize::try_from(count).map_err(|_| format_error("record count"))?;
    let mut items = Vec::new();
    items
        .try_reserve_exact(count)
        .map_err(|_| Error::Budget("checkpoint record allocation".into()))?;
    Ok(items)
}

/// Load a checkpoint for `binding`. Returns the state and its lineage.
pub(crate) fn load(
    path: &Path,
    binding: &Binding,
    limits: &RouteLimits,
    memory_cap: u64,
) -> Result<(BatchCheckpoint, Vec<Segment>)> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut input = Hashed {
        inner: BufReader::with_capacity(BUFFER, file),
        hash: Sha256::new(),
    };
    if &input.exact::<8>()? != MAGIC {
        return Err(format_error("not a RustROAD batch checkpoint"));
    }
    let size = u64::from_le_bytes(input.exact()?);
    if size > MAX_HEADER {
        return Err(format_error("checkpoint header exceeds 1 MiB"));
    }
    let mut json = vec![0u8; size as usize];
    input
        .inner
        .read_exact(&mut json)
        .map_err(|e| format_error(format!("truncated checkpoint header: {e}")))?;
    input.hash.update(&json);
    let header: Header = serde_json::from_slice(&json)?;
    if header.schema != SCHEMA || !header.diagnostic_only {
        return Err(format_error("unsupported checkpoint schema"));
    }
    if header.binding != *binding {
        return Err(Error::Input {
            code: "CHECKPOINT_BINDING",
            message: format!(
                "checkpoint is bound to {:?}, this route is {binding:?}",
                header.binding
            ),
        });
    }
    let c = header.counts;
    let payload = c
        .payload()
        .ok_or_else(|| format_error("checkpoint payload size"))?;
    if 16 + size + payload + 32 != length {
        return Err(format_error("checkpoint length does not match its counts"));
    }
    let segments = c.wires.saturating_add(c.vias).saturating_add(c.patches);
    if segments > limits.max_segments as u64 || c.completed > limits.max_obstacles as u64 {
        return Err(Error::Budget(
            "checkpoint routes exceed the route limits".into(),
        ));
    }
    let bytes = [
        (c.wires, size_of::<Wire>()),
        (c.vias, size_of::<Via>()),
        (c.patches, size_of::<Patch>()),
        (c.access, size_of::<Access>()),
        (c.nets, size_of::<u32>()),
        (c.completed, size_of::<Blockage>()),
    ]
    .iter()
    .map(|&(n, s)| n.saturating_mul(s as u64))
    .fold(0u64, u64::saturating_add);
    let live = rustroad_core::memory::live_bytes()
        .ok_or_else(|| Error::Budget("checkpoint load needs a process memory probe".into()))?;
    if live.saturating_add(bytes) > memory_cap {
        return Err(Error::Budget(format!(
            "checkpoint load: live {live} plus {bytes} bytes exceeds {memory_cap}"
        )));
    }
    let mut wires = vector(c.wires)?;
    for _ in 0..c.wires {
        let (net, layer) = (input.u32()?, input.u32()?);
        let (sx, sy, ex, ey) = (input.i64()?, input.i64()?, input.i64()?, input.i64()?);
        wires.push(Wire {
            net,
            layer,
            start: [sx, sy],
            end: [ex, ey],
        });
    }
    let mut vias = vector(c.vias)?;
    for _ in 0..c.vias {
        let (net, kind) = (input.u32()?, input.u32()?);
        let (x, y) = (input.i64()?, input.i64()?);
        vias.push(Via {
            net,
            kind,
            at: [x, y],
        });
    }
    let mut patches = vector(c.patches)?;
    for _ in 0..c.patches {
        let (net, layer) = (input.u32()?, input.u32()?);
        patches.push(Patch {
            net,
            layer,
            rect: input.rect()?,
        });
    }
    let mut access = vector(c.access)?;
    for _ in 0..c.access {
        let (pin, net, layer) = (input.u32()?, input.u32()?, input.u32()?);
        let (x, y) = (input.i64()?, input.i64()?);
        access.push(Access {
            pin,
            net,
            layer,
            at: [x, y],
        });
    }
    let mut nets = vector(c.nets)?;
    for _ in 0..c.nets {
        nets.push(input.u32()?);
    }
    let mut completed = vector(c.completed)?;
    for _ in 0..c.completed {
        let (layer, net) = (input.u32()?, input.u32()?);
        completed.push(Blockage {
            layer,
            rect: input.rect()?,
            net: Some(net),
        });
    }
    let expected = input.hash.clone().finalize();
    let mut digest = [0u8; 32];
    input
        .inner
        .read_exact(&mut digest)
        .map_err(|e| format_error(format!("truncated checkpoint digest: {e}")))?;
    if digest[..] != expected[..] {
        return Err(format_error("checkpoint digest mismatch"));
    }
    let e = header.peak_estimate;
    Ok((
        BatchCheckpoint {
            begin: header.begin,
            routes: Routes {
                wires,
                vias,
                access,
                patches,
                nets,
                stats: RouteStats {
                    grid_nodes: header.grid_nodes,
                    expansions: header.expansions,
                    comparisons: header.comparisons,
                    wire_length_dbu: header.wire_length_dbu,
                },
            },
            report: BatchReport {
                batches: header.batches,
                repair_attempts: header.repair_attempts,
                displaced_nets: header.displaced_nets,
                peak_estimate: MemoryEstimate {
                    held: e.held,
                    search: e.search,
                    spatial: e.spatial,
                    routed: e.routed,
                    check: e.check,
                },
                final_validation_bytes: 0,
            },
            completed,
        },
        header.lineage,
    ))
}
