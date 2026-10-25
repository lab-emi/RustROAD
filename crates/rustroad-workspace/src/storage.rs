//! Framed physical side tables; large vectors never pass through a whole-file JSON buffer.
//!
//! Records are individually bounded to 1 MiB. Counts are checked before vector
//! growth, fixed pin records use 16 bytes, and the entire immutable file is bound
//! to its byte count and SHA-256 before decoding through a read-only mapping.
use crate::{Error, Metadata, Result};
use rustroad_core::connectivity::{ConnectedDesign, Connection, PinOwner};
use rustroad_core::{Cell, Limits};
use rustroad_formats::verilog::Netlist;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::Path;

const FILE: &str = "model.rbin";
const MAGIC: &[u8; 8] = b"RROADB01";
const MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_CELLS: usize = 8_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tables {
    file: String,
    bytes: u64,
    sha256: String,
    logical_version: u32,
}

fn budget(message: &str) -> Error {
    Error::Budget(message.into())
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

struct RecordBuffer(Vec<u8>);
impl Write for RecordBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RECORD_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("side-table record byte budget"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Writer<W> {
    output: W,
    bytes: u64,
    hash: Sha256,
}
impl<W: Write> Writer<W> {
    fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| budget("side-table length overflow"))?;
        if self.bytes > MAX_BYTES {
            return Err(budget("side-table byte budget"));
        }
        self.output.write_all(bytes)?;
        self.hash.update(bytes);
        Ok(())
    }
    fn count(&mut self, n: usize) -> Result<()> {
        self.raw(&(n as u64).to_le_bytes())
    }
    fn rows<T: Serialize>(&mut self, rows: &[T]) -> Result<()> {
        self.count(rows.len())?;
        let mut buffer = RecordBuffer(Vec::new());
        for row in rows {
            buffer.0.clear();
            serde_json::to_writer(&mut buffer, row)?;
            self.raw(&(buffer.0.len() as u32).to_le_bytes())?;
            self.raw(&buffer.0)?;
        }
        Ok(())
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl Reader<'_> {
    fn raw(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or_else(|| budget("side-table offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid("truncated physical side table"))?;
        self.offset = end;
        Ok(bytes)
    }
    fn count(&mut self, max: usize, minimum_bytes: usize) -> Result<usize> {
        let n = u64::from_le_bytes(self.raw(8)?.try_into().expect("eight-byte count"));
        let n = usize::try_from(n).map_err(|_| budget("side-table count overflow"))?;
        if n > max || n > (self.bytes.len() - self.offset) / minimum_bytes {
            return Err(budget("physical side-table object count"));
        }
        Ok(n)
    }
    fn rows<T: DeserializeOwned>(&mut self, max: usize) -> Result<Vec<T>> {
        let count = self.count(max, 4)?;
        let mut out = Vec::new();
        for _ in 0..count {
            let len =
                u32::from_le_bytes(self.raw(4)?.try_into().expect("four-byte length")) as usize;
            if len > MAX_RECORD_BYTES {
                return Err(budget("physical side-table record bytes"));
            }
            let row = serde_json::from_slice(self.raw(len)?)?;
            if out.len() == out.capacity() {
                out.try_reserve((count - out.len()).min(4096))
                    .map_err(|_| budget("physical side-table allocation"))?
            }
            out.push(row);
        }
        Ok(out)
    }
}

struct Held {
    cells: Vec<Cell>,
    netlist: Option<Netlist>,
    connectivity: Option<ConnectedDesign>,
    fillers: Vec<String>,
    taps: Vec<String>,
}
impl Held {
    fn take(metadata: &mut Metadata) -> Self {
        let mut held = Self {
            cells: std::mem::take(&mut metadata.design.cells),
            netlist: None,
            connectivity: None,
            fillers: Vec::new(),
            taps: Vec::new(),
        };
        if let Some(p) = &mut metadata.physical {
            let empty = Netlist {
                module: p.netlist.module.clone(),
                ports: Vec::new(),
                wires: Vec::new(),
                assigns: Vec::new(),
                instances: Vec::new(),
            };
            held.netlist = Some(std::mem::replace(&mut p.netlist, empty));
            let empty = ConnectedDesign {
                layers: Vec::new(),
                masters: Vec::new(),
                instance_masters: Vec::new(),
                ports: Vec::new(),
                nets: Vec::new(),
                pins: Vec::new(),
            };
            held.connectivity = Some(std::mem::replace(&mut p.connectivity, empty));
            held.fillers = std::mem::take(&mut p.generated_fillers);
            held.taps = std::mem::take(&mut p.generated_taps);
        }
        held
    }
    fn restore(self, metadata: &mut Metadata) {
        metadata.design.cells = self.cells;
        if let Some(p) = &mut metadata.physical {
            p.netlist = self.netlist.expect("physical netlist retained");
            p.connectivity = self.connectivity.expect("physical connectivity retained");
            p.generated_fillers = self.fillers;
            p.generated_taps = self.taps;
        }
    }
}

/// Write large arrays separately and serialize only their immutable references.
pub(crate) fn serialize(metadata: &mut Metadata, directory: &Path) -> Result<Vec<u8>> {
    metadata.tables = None;
    if metadata.design.cells.len() <= 100_000 {
        return crate::serialize_bounded(metadata);
    }
    spill(metadata, directory)
}

fn spill(metadata: &mut Metadata, directory: &Path) -> Result<Vec<u8>> {
    let held = Held::take(metadata);
    let result = (|| {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join(FILE))?;
        let mut w = Writer {
            output: BufWriter::with_capacity(256 * 1024, file),
            bytes: 0,
            hash: Sha256::new(),
        };
        w.raw(MAGIC)?;
        w.rows(&held.cells)?;
        if let (Some(n), Some(c)) = (&held.netlist, &held.connectivity) {
            w.rows(&n.ports)?;
            w.rows(&n.wires)?;
            w.rows(&n.assigns)?;
            w.rows(&n.instances)?;
            w.rows(&c.layers)?;
            w.rows(&c.masters)?;
            w.count(c.instance_masters.len())?;
            for id in &c.instance_masters {
                w.raw(&id.to_le_bytes())?;
            }
            w.rows(&c.ports)?;
            w.rows(&c.nets)?;
            w.count(c.pins.len())?;
            for pin in &c.pins {
                for field in crate::owner_record(pin) {
                    w.raw(&field.to_le_bytes())?;
                }
            }
            w.rows(&held.fillers)?;
            w.rows(&held.taps)?;
        }
        w.output.flush()?;
        w.output.get_ref().sync_all()?;
        metadata.tables = Some(Tables {
            file: FILE.into(),
            bytes: w.bytes,
            sha256: format!("{:x}", w.hash.finalize()),
            logical_version: metadata.format_version,
        });
        metadata.format_version = 7;
        crate::serialize_bounded(metadata)
    })();
    held.restore(metadata);
    result
}

/// Restore vectors after validating the file identity and all framing bounds.
pub(crate) fn restore(metadata: &mut Metadata, directory: &Path, limits: &Limits) -> Result<u32> {
    if metadata.format_version != 7 {
        if metadata.tables.is_some() {
            return Err(invalid("side tables require workspace schema 7"));
        }
        return Ok(metadata.format_version);
    }
    let table = metadata
        .tables
        .as_ref()
        .ok_or_else(|| invalid("schema 7 has no physical side table"))?;
    if table.file != FILE
        || table.bytes < MAGIC.len() as u64
        || table.bytes > MAX_BYTES
        || !(1..=6).contains(&table.logical_version)
        || limits.max_cells > MAX_CELLS
    {
        return Err(invalid("physical side-table descriptor"));
    }
    if !metadata.design.cells.is_empty() {
        return Err(invalid("schema 7 duplicates physical cell records"));
    }
    let length = usize::try_from(table.bytes).map_err(|_| budget("physical side-table size"))?;
    let map = crate::map_table(&directory.join(FILE), length)?
        .ok_or_else(|| invalid("empty physical side table"))?;
    if format!("{:x}", Sha256::digest(&map[..])) != table.sha256 {
        return Err(invalid("physical side-table hash mismatch"));
    }
    let mut r = Reader {
        bytes: &map,
        offset: 0,
    };
    if r.raw(8)? != MAGIC {
        return Err(invalid("physical side-table magic"));
    }
    let cells = limits.max_cells;
    let pins = cells
        .checked_mul(16)
        .ok_or_else(|| budget("physical pin count"))?;
    metadata.design.cells = r.rows(cells)?;
    if let Some(p) = &mut metadata.physical {
        if !p.netlist.ports.is_empty()
            || !p.netlist.wires.is_empty()
            || !p.netlist.assigns.is_empty()
            || !p.netlist.instances.is_empty()
            || !p.connectivity.layers.is_empty()
            || !p.connectivity.masters.is_empty()
            || !p.connectivity.instance_masters.is_empty()
            || !p.connectivity.ports.is_empty()
            || !p.connectivity.nets.is_empty()
            || !p.connectivity.pins.is_empty()
            || !p.generated_fillers.is_empty()
            || !p.generated_taps.is_empty()
        {
            return Err(invalid("schema 7 duplicates connectivity records"));
        }
        p.netlist.ports = r.rows(65_536)?;
        p.netlist.wires = r.rows(pins)?;
        p.netlist.assigns = r.rows(pins)?;
        p.netlist.instances = r.rows(cells)?;
        p.connectivity.layers = r.rows(4096)?;
        p.connectivity.masters = r.rows(cells)?;
        let count = r.count(cells, 4)?;
        p.connectivity
            .instance_masters
            .try_reserve(count)
            .map_err(|_| budget("instance-master allocation"))?;
        for _ in 0..count {
            p.connectivity.instance_masters.push(u32::from_le_bytes(
                r.raw(4)?.try_into().expect("four-byte id"),
            ));
        }
        p.connectivity.ports = r.rows(65_536)?;
        p.connectivity.nets = r.rows(pins)?;
        let count = r.count(pins, 16)?;
        p.connectivity
            .pins
            .try_reserve(count)
            .map_err(|_| budget("pin allocation"))?;
        for _ in 0..count {
            let b = r.raw(16)?;
            let field =
                |n| u32::from_le_bytes(b[n..n + 4].try_into().expect("four-byte pin field"));
            let owner = match field(0) {
                0 => PinOwner::Instance {
                    instance: field(4),
                    terminal: field(8),
                },
                1 => PinOwner::Port {
                    port: field(4),
                    bit: field(8),
                },
                _ => return Err(invalid("side-table pin owner")),
            };
            p.connectivity.pins.push(Connection {
                owner,
                net: (field(12) != u32::MAX).then(|| field(12)),
            });
        }
        p.generated_fillers = r.rows(cells)?;
        p.generated_taps = r.rows(cells)?;
    }
    if r.offset != r.bytes.len() {
        return Err(invalid("trailing physical side-table bytes"));
    }
    Ok(table.logical_version)
}

pub(crate) fn logical_version(metadata: &Metadata) -> Result<u32> {
    if metadata.format_version == 7 {
        metadata
            .tables
            .as_ref()
            .map(|t| t.logical_version)
            .ok_or_else(|| invalid("schema 7 missing table descriptor"))
    } else {
        Ok(metadata.format_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn fixture() -> (std::path::PathBuf, Metadata) {
        let dir = std::env::temp_dir().join(format!(
            "rustroad-tables-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).expect("new test directory");
        let design: rustroad_core::Design =
            serde_json::from_str(include_str!("../../../fixtures/physical-small.json"))
                .expect("fixture design");
        (
            dir,
            Metadata {
                tables: None,
                format_version: 1,
                revision: "rev1".into(),
                parent_revision: "rev0".into(),
                stage: crate::Stage::Imported,
                design,
                expansions: 0,
                physical: None,
            },
        )
    }
    #[test]
    fn external_cells_restore_and_preserve_logical_schema() {
        let (dir, mut metadata) = fixture();
        let expected = serde_json::to_value(&metadata).expect("original metadata");
        let json = spill(&mut metadata, &dir).expect("write side table");
        let mut decoded: Metadata = serde_json::from_slice(&json).expect("small revision metadata");
        assert!(decoded.design.cells.is_empty());
        assert_eq!(decoded.format_version, 7);
        let logical = restore(&mut decoded, &dir, &Limits::default()).expect("restore cells");
        assert_eq!(decoded.format_version, 7);
        decoded.format_version = logical;
        decoded.tables = None;
        assert_eq!(
            serde_json::to_value(decoded).expect("restored metadata"),
            expected
        );
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }
    #[test]
    fn corrupted_table_is_rejected_before_decoding() {
        let (dir, mut metadata) = fixture();
        let json = spill(&mut metadata, &dir).expect("write side table");
        let mut decoded: Metadata = serde_json::from_slice(&json).expect("metadata");
        let mut bytes = std::fs::read(dir.join(FILE)).expect("table bytes");
        *bytes.last_mut().expect("nonempty table") ^= 1;
        std::fs::write(dir.join(FILE), bytes).expect("corrupt table");
        assert!(
            restore(&mut decoded, &dir, &Limits::default())
                .expect_err("corrupt table must fail")
                .to_string()
                .contains("hash mismatch")
        );
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }
    #[test]
    fn physical_vectors_and_fixed_pin_records_round_trip() {
        let (dir, mut metadata) = fixture();
        metadata.format_version = 2;
        metadata.physical = Some(serde_json::from_value(serde_json::json!({
            "import_version": 1,
            "technology": {"dbu_per_micron":1000,"manufacturing_grid":null,"layers":[],"sites":[],"vias":[]},
            "netlist": {"module":"road_demo","ports":[{"name":"a","direction":"input","range":null}],"wires":[{"name":"n","range":null}],"assigns":[],"instances":[{"name":"u","cell":"BUF","connections":[{"pin":"A","signal":{"kind":"ident","value":"a"}}]}]},
            "connectivity": {"layers":["M1"],"masters":[],"instance_masters":[0],"ports":[{"name":"a","direction":"input","bits":["a"],"use_":"signal"}],"nets":[{"name":"a","aliases":["a"],"use_":"signal"}],"pins":[{"owner":{"kind":"instance","instance":0,"terminal":0},"net":0},{"owner":{"kind":"port","port":0,"bit":0},"net":null}]},
            "sources":[],"diagnostics":[],"generated_fillers":["fill0"],"generated_taps":["tap0"]
        })).expect("typed storage fixture"));
        let expected = serde_json::to_value(&metadata).expect("original vectors");
        let json = spill(&mut metadata, &dir).expect("serialize physical vectors");
        let mut decoded: Metadata = serde_json::from_slice(&json).expect("small metadata");
        let logical = restore(&mut decoded, &dir, &Limits::default())
            .expect("restore fixed and variable records");
        decoded.format_version = logical;
        decoded.tables = None;
        assert_eq!(
            serde_json::to_value(decoded).expect("decoded vectors"),
            expected
        );
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }
    #[test]
    fn malicious_count_with_matching_digest_does_not_allocate() {
        let (dir, mut metadata) = fixture();
        let json = spill(&mut metadata, &dir).expect("write side table");
        let mut decoded: Metadata = serde_json::from_slice(&json).expect("metadata");
        let mut bytes = std::fs::read(dir.join(FILE)).expect("table bytes");
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        decoded.tables.as_mut().expect("descriptor").sha256 =
            format!("{:x}", Sha256::digest(&bytes));
        std::fs::write(dir.join(FILE), bytes).expect("corrupt count");
        assert!(matches!(
            restore(&mut decoded, &dir, &Limits::default()),
            Err(Error::Budget(_))
        ));
        assert!(decoded.design.cells.is_empty());
        std::fs::remove_dir_all(dir).expect("remove test directory");
    }
}
