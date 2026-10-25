//! Read-only, byte-deterministic GDSII streamout of a placed/routed workspace.
//!
//! Resource contract: docs/gds-budget.md. The workspace is only read under its
//! shared lock; HEAD and every revision file are untouched. Output is streamed
//! through a byte/record-capped, hashing writer into a temporary file in the
//! destination directory and atomically renamed; it is never held in memory.
//!
//! Hierarchy: the used PDK cell structures and their reference closure are
//! copied verbatim (byte-identical) from the PDK cell GDS; each used fixed LEF
//! VIA becomes one structure `VIA_<name>`; the top structure holds one SREF per
//! instance (N: none, FS: STRANS x-reflection at y + height), routed wires with
//! DEF semantics (layer default width, half-width end extensions) merged into
//! maximal collinear runs exactly as `export-def` writes them, routed and
//! supply via SREFs, same-net routing patches (the DEF routing `RECT`s) on
//! the layer's net purpose, supply rails/spines, pin shapes with TEXT labels
//! and the PR boundary. Dates are fixed, so identical inputs give identical bytes.
//!
//! Units: coordinates are integer LEF DBU. When the LEF grid is finer than the
//! cell-GDS grid (GF180: 2000/um vs 0.001 um), every coordinate is divided
//! exactly; an inexact division is the explicit error `GDS_ODD_COORDINATE`.
//! Library units are never rescaled.

use crate::pdk::PhysicalBundle;
use crate::{Error, Result, Stage};
use rustroad_core::connectivity::PinUse;
use rustroad_core::power::RailPolarity;
use rustroad_core::routing::{self, Wire};
use rustroad_core::{Limits, Orientation, Placed, Rect};
use rustroad_formats::gds::{self, GdsError, GdsLimits, GdsWriter, Strans};
use rustroad_formats::gdsmap::{GdsLayer, LayerEntry, LayerKind, LayerMap, Purpose};
use rustroad_formats::lef::{self, LefGeometry};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_RECORDS: u64 = 16_000_000;
/// BGNLIB/BGNSTR modification and access dates: 1970-01-01 00:00:00 (years
/// counted from 1900). Fixed so that output carries no timestamp.
pub const FIXED_DATES: [i16; 12] = [70, 1, 1, 0, 0, 0, 70, 1, 1, 0, 0, 0];
pub const GDS_VERSION: i16 = 600;
pub const VIA_CELL_PREFIX: &str = "VIA_";
const MAX_LAYER_MAP_BYTES: u64 = 1024 * 1024;
const MAX_CELL_LEF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LABEL_BYTES: usize = 512;
static TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn fail(code: &'static str, message: impl Into<String>) -> Error {
    Error::Gds {
        code,
        message: message.into(),
    }
}
impl From<GdsError> for Error {
    fn from(e: GdsError) -> Self {
        fail(e.code, format!("{} (byte offset {})", e.message, e.offset))
    }
}

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub cell_gds: PathBuf,
    pub layer_map: PathBuf,
    pub out: PathBuf,
    /// Override for the recorded cell-LEF path; its sha256 must still match.
    pub cell_lef: Option<PathBuf>,
    pub max_output_bytes: u64,
    pub max_output_records: u64,
    pub limits: GdsLimits,
}
impl ExportOptions {
    pub fn new(cell_gds: PathBuf, layer_map: PathBuf, out: PathBuf) -> Self {
        Self {
            cell_gds,
            layer_map,
            out,
            cell_lef: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_output_records: DEFAULT_MAX_OUTPUT_RECORDS,
            limits: GdsLimits::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct ElementCounts {
    pub instances: usize,
    /// Maximal collinear runs written (one rectangle per run and target).
    pub routed_wires: usize,
    /// Stored lattice wire records the runs were merged from.
    pub stored_wire_records: usize,
    pub routed_vias: usize,
    /// Same-net routing patches (DEF routing RECT).
    pub routed_patches: usize,
    pub supply_wires: usize,
    pub supply_vias: usize,
    pub pin_shapes: usize,
    pub labels: usize,
    pub boundary: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct GdsReport {
    pub status: &'static str,
    pub revision: String,
    pub top: String,
    pub output: FileIdentity,
    pub records: u64,
    pub cell_gds: FileIdentity,
    pub layer_map: FileIdentity,
    pub layer_map_pdk: String,
    pub cell_lef: FileIdentity,
    pub foreign_bindings_checked: usize,
    pub lef_dbu_per_micron: u32,
    pub gds_dbu_per_micron: u32,
    pub coordinate_divisor: i64,
    pub pdk_structures: Vec<String>,
    pub via_structures: Vec<String>,
    pub elements: ElementCounts,
    pub routing_status: &'static str,
    pub drc_status: &'static str,
    pub lvs_status: &'static str,
    pub scope: &'static str,
    pub workspace_mutated: bool,
}

fn identity(path: &Path, bytes: &[u8]) -> FileIdentity {
    FileIdentity {
        path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    }
}

/// Exact LEF-DBU -> GDS-DBU coordinate conversion.
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    pub divisor: i64,
}
impl Scale {
    pub fn coord(self, value: i64, what: &str) -> Result<i32> {
        if value % self.divisor != 0 {
            return Err(fail(
                "GDS_ODD_COORDINATE",
                format!(
                    "{what}: coordinate {value} LEF DBU is not a multiple of {}; \
                     the GDS grid would need a fractional database unit",
                    self.divisor
                ),
            ));
        }
        i32::try_from(value / self.divisor).map_err(|_| {
            fail(
                "GDS_COORDINATE_RANGE",
                format!("{what}: coordinate {value} exceeds the GDSII 32-bit range"),
            )
        })
    }
    pub fn point(self, p: [i64; 2], what: &str) -> Result<[i32; 2]> {
        Ok([self.coord(p[0], what)?, self.coord(p[1], what)?])
    }
    pub fn rect(self, r: Rect, what: &str) -> Result<[i32; 4]> {
        Ok([
            self.coord(r.lx, what)?,
            self.coord(r.ly, what)?,
            self.coord(r.ux, what)?,
            self.coord(r.uy, what)?,
        ])
    }
}

/// Hashes exactly the bytes accepted by the inner writer.
struct Hashing<W: Write> {
    inner: W,
    hasher: Sha256,
}
impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
/// Removes an unpublished temporary output on every failure path.
struct Pending {
    path: PathBuf,
    armed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn label_text(name: &str) -> Result<&str> {
    if name.is_empty()
        || name.len() > MAX_LABEL_BYTES
        || !name
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'\\')
    {
        return Err(fail(
            "GDS_LABEL_UNSUPPORTED",
            format!("pin name {name:?} has no unambiguous GDS label (escaped or non-printable)"),
        ));
    }
    Ok(name)
}

/// Destination checks: never inside the workspace, never onto an input or a
/// non-regular file. Returns the canonical destination path.
fn destination(workspace: &Path, out: &Path, inputs: &[&Path]) -> Result<PathBuf> {
    let name = out
        .file_name()
        .ok_or_else(|| fail("GDS_OUTPUT", "output path needs a file name"))?;
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let parent = fs::canonicalize(&parent)
        .map_err(|e| fail("GDS_OUTPUT", format!("output directory: {e}")))?;
    let workspace = fs::canonicalize(workspace)?;
    if parent.starts_with(&workspace) {
        return Err(fail(
            "GDS_OUTPUT_IN_WORKSPACE",
            "export-gds is read-only; write outside the workspace directory",
        ));
    }
    let target = parent.join(name);
    if let Ok(m) = fs::symlink_metadata(&target)
        && !m.is_file()
    {
        return Err(fail("GDS_OUTPUT", "existing output is not a regular file"));
    }
    let resolved = fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    for input in inputs {
        if fs::canonicalize(input).is_ok_and(|c| c == resolved) {
            return Err(fail("GDS_OUTPUT", "output path would overwrite an input"));
        }
    }
    Ok(target)
}

fn used_masters(bundle: &PhysicalBundle) -> Vec<&str> {
    let model = &bundle.connectivity;
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for &m in &model.instance_masters {
        let name = model.masters[m as usize].name.as_str();
        if seen.insert(name) {
            names.push(name);
        }
    }
    names
}

/// Re-read the recorded cell LEF (sha256-pinned) and require every used
/// master to bind to the same-named foreign cell at offset zero, orientation N.
fn check_foreign(
    bundle: &PhysicalBundle,
    override_path: Option<&Path>,
    dbu: u32,
    used: &[&str],
) -> Result<(FileIdentity, usize)> {
    let pin = bundle
        .sources
        .iter()
        .find(|s| s.role == "cell_lef")
        .ok_or_else(|| fail("GDS_SOURCE_MISSING", "workspace records no cell_lef source"))?;
    let path = override_path.unwrap_or(&pin.path);
    let bytes = crate::read_bounded(path, MAX_CELL_LEF_BYTES)?;
    let id = identity(path, &bytes);
    if id.sha256 != pin.sha256 {
        return Err(fail(
            "GDS_SOURCE_CHANGED",
            format!(
                "cell LEF {} sha256 {} differs from imported {}",
                path.display(),
                id.sha256,
                pin.sha256
            ),
        ));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| fail("GDS_CELL_LEF", "cell LEF is not UTF-8"))?;
    let document = lef::parse_lef(&text, dbu, &lef::LefLimits::default())
        .map_err(|e| fail("GDS_CELL_LEF", e.to_string()))?;
    let macros: HashMap<&str, &lef::LefMacro> = document
        .macros
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    let model = &bundle.connectivity;
    for name in used {
        let m = macros.get(name).ok_or_else(|| {
            fail(
                "GDS_FOREIGN_MISSING",
                format!("used master {name} is absent from the cell LEF"),
            )
        })?;
        let master = model
            .masters
            .iter()
            .find(|x| x.name == *name)
            .expect("used master");
        if m.size != master.size {
            return Err(fail(
                "GDS_SOURCE_CHANGED",
                format!("master {name} size differs from the imported LEF"),
            ));
        }
        match m.foreign.as_slice() {
            [f] if f.name == m.name
                && f.origin == [0, 0]
                && m.origin == [0, 0]
                && f.orientation.as_deref().is_none_or(|o| o == "N") => {}
            [] => {
                return Err(fail(
                    "GDS_FOREIGN_MISSING",
                    format!("master {name} has no FOREIGN binding to a GDS cell"),
                ));
            }
            _ => {
                return Err(fail(
                    "GDS_FOREIGN_MISMATCH",
                    format!(
                        "master {name} FOREIGN {:?} / ORIGIN {:?} is not the same-named cell at offset 0 N",
                        m.foreign, m.origin
                    ),
                ));
            }
        }
    }
    Ok((id, used.len()))
}

struct Layers<'a> {
    map: &'a LayerMap,
    technology: &'a crate::pdk::Technology,
}
impl<'a> Layers<'a> {
    fn entry(&self, name: &str) -> Result<&'a LayerEntry> {
        let entry = self.map.entry(name).ok_or_else(|| {
            fail(
                "GDS_LAYER_UNMAPPED",
                format!(
                    "LEF layer {name} has no entry in the {} layer map",
                    self.map.pdk
                ),
            )
        })?;
        let tech = self
            .technology
            .layers
            .iter()
            .find(|l| l.name == name)
            .ok_or_else(|| {
                fail(
                    "GDS_LAYER_UNMAPPED",
                    format!("{name} is not a technology layer"),
                )
            })?;
        let kind = match tech.kind.as_deref() {
            Some("ROUTING") => LayerKind::Routing,
            Some("CUT") => LayerKind::Cut,
            other => {
                return Err(fail(
                    "GDS_LAYER_KIND",
                    format!("{name} has LEF TYPE {other:?}; only ROUTING/CUT shapes stream"),
                ));
            }
        };
        if kind != entry.kind {
            return Err(fail(
                "GDS_LAYER_KIND",
                format!(
                    "{name} is {kind:?} in LEF but {:?} in the layer map",
                    entry.kind
                ),
            ));
        }
        Ok(entry)
    }
    fn targets(&self, name: &str, purpose: Purpose) -> Result<&'a [GdsLayer]> {
        let t = self.entry(name)?.targets(purpose);
        if t.is_empty() {
            return Err(fail(
                "GDS_LAYER_UNMAPPED",
                format!("{name} has no {purpose:?} target in the layer map"),
            ));
        }
        Ok(t)
    }
    fn width(&self, name: &str) -> Result<i64> {
        self.technology
            .layers
            .iter()
            .find(|l| l.name == name)
            .and_then(|l| l.width)
            .ok_or_else(|| fail("GDS_WIDTH", format!("{name} has no LEF default WIDTH")))
    }
}

fn rect_of(r: [i64; 4]) -> Rect {
    Rect {
        lx: r[0],
        ly: r[1],
        ux: r[2],
        uy: r[3],
    }
}
fn wire_rect(start: [i64; 2], end: [i64; 2], width: i64) -> Result<Rect> {
    routing::wire_rect(
        &Wire {
            net: 0,
            layer: 0,
            start,
            end,
        },
        width,
    )
    .map_err(|e| fail("GDS_GEOMETRY", e.to_string()))
}

/// Stream the current revision. Read-only: takes the shared workspace lock.
pub fn export_gds(workspace: &Path, options: &ExportOptions, limits: &Limits) -> Result<GdsReport> {
    let snapshot = crate::read(workspace, limits)?;
    let meta = snapshot.metadata();
    if meta.stage != Stage::Placed {
        return Err(fail("GDS_UNPLACED", "place the design before GDS export"));
    }
    let bundle = meta.physical.as_ref().ok_or_else(|| {
        fail(
            "GDS_UNSUPPORTED_WORKSPACE",
            "GDS export needs an imported PDK workspace",
        )
    })?;
    let (Some(_), Some(state)) = (bundle.config.as_ref(), bundle.state.as_ref()) else {
        return Err(fail(
            "GDS_UNSUPPORTED_WORKSPACE",
            "GDS export needs prepared physical I/O and supply geometry",
        ));
    };
    let placed: Vec<Placed> = snapshot.records().collect();
    let model = &bundle.connectivity;
    let design = &meta.design;

    let map_bytes = crate::read_bounded(&options.layer_map, MAX_LAYER_MAP_BYTES)?;
    let map_id = identity(&options.layer_map, &map_bytes);
    let map: LayerMap =
        serde_json::from_slice(&map_bytes).map_err(|e| fail("GDS_LAYER_MAP", e.to_string()))?;
    map.validate().map_err(|e| fail("GDS_LAYER_MAP", e))?;
    let layers = Layers {
        map: &map,
        technology: &bundle.technology,
    };

    let cell_bytes = crate::read_bounded(&options.cell_gds, options.limits.max_bytes as u64)?;
    let cell_id = identity(&options.cell_gds, &cell_bytes);
    let library = gds::parse_library(&cell_bytes, &options.limits, false)?;
    let gds_dbu = gds::dbu_per_micron(&library.units)?;
    let lef_dbu = design.units_per_micron;
    if lef_dbu < gds_dbu || lef_dbu % gds_dbu != 0 {
        return Err(fail(
            "GDS_UNIT_MISMATCH",
            format!("LEF {lef_dbu} DBU/um is not an integer multiple of cell-GDS {gds_dbu} DBU/um"),
        ));
    }
    let scale = Scale {
        divisor: i64::from(lef_dbu / gds_dbu),
    };

    let used = used_masters(bundle);
    let (lef_id, foreign) = check_foreign(bundle, options.cell_lef.as_deref(), lef_dbu, &used)?;
    let closure = library.closure(&used, &options.limits).map_err(|e| {
        if e.code == "GDS_MISSING_STRUCTURE" {
            fail("GDS_CELL_MISSING", e.message)
        } else {
            e.into()
        }
    })?;

    let mut vias: BTreeMap<&str, &lef::LefVia> = BTreeMap::new();
    let mut add_via = |name: &str| -> Result<()> {
        if vias.contains_key(name) {
            return Ok(());
        }
        let via = bundle
            .technology
            .vias
            .iter()
            .find(|v| v.name == name)
            .ok_or_else(|| {
                fail(
                    "GDS_VIA_MISSING",
                    format!("fixed via {name} is not in the technology"),
                )
            })?;
        vias.insert(via.name.as_str(), via);
        Ok(())
    };
    if let Some(r) = &bundle.routing {
        for v in &r.routes.vias {
            let name = r
                .via_names
                .get(v.kind as usize)
                .ok_or_else(|| fail("GDS_VIA_MISSING", "routed via kind out of range"))?;
            add_via(name)?;
        }
    }
    for v in &state.supply.vias {
        add_via(&v.name)?;
    }

    let top = design.name.as_str();
    let pdk_names: HashSet<&str> = library.structures.iter().map(|s| s.name.as_str()).collect();
    if pdk_names.contains(top) {
        return Err(fail(
            "GDS_NAME_COLLISION",
            format!("top cell {top} collides with a cell-GDS structure"),
        ));
    }
    let via_cells: BTreeMap<&str, String> = vias
        .keys()
        .map(|n| (*n, format!("{VIA_CELL_PREFIX}{n}")))
        .collect();
    for cell in via_cells.values() {
        if pdk_names.contains(cell.as_str()) || cell == top {
            return Err(fail(
                "GDS_NAME_COLLISION",
                format!("via structure {cell} collides with an existing structure"),
            ));
        }
    }

    let inputs: Vec<&Path> = vec![&options.cell_gds, &options.layer_map, lef_id.path.as_path()];
    let target = destination(workspace, &options.out, &inputs)?;
    let parent = target.parent().expect("canonical parent").to_path_buf();
    let file_name = target
        .file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned();
    let mut pending = None;
    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".{file_name}.rustroad-{}-{}.tmp",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                pending = Some((
                    file,
                    Pending {
                        path: candidate,
                        armed: true,
                    },
                ));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let (file, mut guard) =
        pending.ok_or_else(|| fail("GDS_OUTPUT", "could not create a temporary output"))?;
    let sink = BufWriter::with_capacity(
        256 * 1024,
        Hashing {
            inner: file,
            hasher: Sha256::new(),
        },
    );
    let mut w = GdsWriter::new(sink, options.max_output_bytes, options.max_output_records);
    let mut counts = ElementCounts::default();

    w.begin_library(GDS_VERSION, &FIXED_DATES, top, library.units)?;
    for &i in &closure {
        w.raw(&cell_bytes[library.structures[i].source.clone()])?;
    }
    for (name, via) in &vias {
        w.begin_structure(&via_cells[name], &FIXED_DATES)?;
        let what = format!("VIA {name}");
        for g in &via.geometries {
            match g {
                LefGeometry::Rect { layer, rect } => {
                    let r = scale.rect(rect_of(*rect), &what)?;
                    for t in layers.targets(layer, Purpose::Via)? {
                        w.rectangle(t.0, t.1, r)?;
                    }
                }
                LefGeometry::Polygon { layer, points } => {
                    let mut xy = points
                        .iter()
                        .map(|p| scale.point(*p, &what))
                        .collect::<Result<Vec<_>>>()?;
                    if xy.first() != xy.last() {
                        xy.push(xy[0]);
                    }
                    for t in layers.targets(layer, Purpose::Via)? {
                        w.boundary(t.0, t.1, &xy)?;
                    }
                }
            }
        }
        w.end_structure()?;
    }

    w.begin_structure(top, &FIXED_DATES)?;
    for (i, p) in placed.iter().enumerate() {
        let master = &model.masters[model.instance_masters[i] as usize];
        let what = format!("instance {}", design.cells[i].name);
        match rustroad_core::placed_orientation(design, p)? {
            Orientation::N => w.sref(&master.name, None, scale.point([p.x, p.y], &what)?)?,
            Orientation::FS => {
                let y =
                    p.y.checked_add(p.height)
                        .ok_or_else(|| fail("GDS_COORDINATE_RANGE", what.clone()))?;
                w.sref(
                    &master.name,
                    Some(&Strans::reflect_x()),
                    scale.point([p.x, y], &what)?,
                )?
            }
        }
        counts.instances += 1;
    }

    let routing_status = if let Some(r) = &bundle.routing {
        // Same export view as DEF: maximal collinear runs (identical metal
        // union to the stored lattice records), then vias, then the same-net
        // patches on the routing layer's net purpose.
        let runs = crate::routing::export_runs(bundle, r)
            .map_err(|e| fail("GDS_GEOMETRY", e.to_string()))?;
        let mut wires: HashMap<u32, Vec<&Wire>> = HashMap::new();
        let mut routed: HashMap<u32, Vec<&routing::Via>> = HashMap::new();
        let mut patches: HashMap<u32, Vec<&routing::Patch>> = HashMap::new();
        // Rail ties are drawn with their supply net (net purpose, as DEF NETS).
        let export = crate::routing::ExportNets::new(bundle);
        let net_of = |n: u32| {
            export
                .of(n)
                .map_err(|e| fail("GDS_GEOMETRY", e.to_string()))
        };
        for wire in &runs {
            wires.entry(net_of(wire.net)?).or_default().push(wire);
        }
        for via in &r.routes.vias {
            routed.entry(net_of(via.net)?).or_default().push(via);
        }
        for patch in &r.routes.patches {
            patches.entry(net_of(patch.net)?).or_default().push(patch);
        }
        counts.stored_wire_records = r.routes.wires.len();
        // Resolve each routing layer's targets and DEF default width once.
        let mut resolved: HashMap<u32, (&[GdsLayer], i64)> = HashMap::new();
        let mut resolve = |layer: u32| -> Result<(&[GdsLayer], i64)> {
            if let Some(v) = resolved.get(&layer) {
                return Ok(*v);
            }
            let name = r
                .layers
                .get(layer as usize)
                .ok_or_else(|| fail("GDS_LAYER_UNMAPPED", "routed layer out of range"))?;
            let v = (layers.targets(name, Purpose::Net)?, layers.width(name)?);
            resolved.insert(layer, v);
            Ok(v)
        };
        for (ni, net) in model.nets.iter().enumerate() {
            let what = format!("net {}", net.name);
            for wire in wires.get(&(ni as u32)).map_or(&[][..], Vec::as_slice) {
                let (targets, width) = resolve(wire.layer)?;
                let rect = scale.rect(wire_rect(wire.start, wire.end, width)?, &what)?;
                for t in targets {
                    w.rectangle(t.0, t.1, rect)?;
                }
                counts.routed_wires += 1;
            }
            for via in routed.get(&(ni as u32)).map_or(&[][..], Vec::as_slice) {
                let name = &r.via_names[via.kind as usize];
                w.sref(&via_cells[name.as_str()], None, scale.point(via.at, &what)?)?;
                counts.routed_vias += 1;
            }
            for patch in patches.get(&(ni as u32)).map_or(&[][..], Vec::as_slice) {
                let (targets, _) = resolve(patch.layer)?;
                if !(patch.rect.lx < patch.rect.ux && patch.rect.ly < patch.rect.uy) {
                    return Err(fail("GDS_GEOMETRY", format!("{what}: empty patch")));
                }
                let rect = scale.rect(patch.rect, &what)?;
                for t in targets {
                    w.rectangle(t.0, t.1, rect)?;
                }
                counts.routed_patches += 1;
            }
        }
        "routed"
    } else {
        "absent"
    };

    for net in model.nets.iter().filter(|n| n.use_.is_supply()) {
        let polarity = if net.use_ == PinUse::Power {
            RailPolarity::Power
        } else {
            RailPolarity::Ground
        };
        let what = format!("supply net {}", net.name);
        for wire in state.supply.wires.iter().filter(|x| x.polarity == polarity) {
            let rect = scale.rect(wire_rect(wire.start, wire.end, wire.width)?, &what)?;
            for t in layers.targets(&wire.layer, Purpose::SpecialNet)? {
                w.rectangle(t.0, t.1, rect)?;
            }
            counts.supply_wires += 1;
        }
        for via in state.supply.vias.iter().filter(|x| x.polarity == polarity) {
            w.sref(
                &via_cells[via.name.as_str()],
                None,
                scale.point(via.at, &what)?,
            )?;
            counts.supply_vias += 1;
        }
    }

    let io: HashMap<&str, (&str, Rect)> = state
        .io_pins
        .iter()
        .map(|p| (p.name.as_str(), (p.layer.as_str(), p.rect)))
        .collect();
    for port in &model.ports {
        for name in &port.bits {
            let (layer, rect) = if port.use_.is_supply() {
                let polarity = if port.use_ == PinUse::Power {
                    RailPolarity::Power
                } else {
                    RailPolarity::Ground
                };
                let s = state
                    .supply
                    .ports
                    .iter()
                    .find(|s| s.polarity == polarity)
                    .ok_or_else(|| {
                        fail(
                            "GDS_PIN_GEOMETRY",
                            format!("supply port {name} has no shape"),
                        )
                    })?;
                (s.layer.as_str(), s.rect)
            } else {
                *io.get(name.as_str())
                    .ok_or_else(|| fail("GDS_PIN_GEOMETRY", format!("I/O {name} has no shape")))?
            };
            let what = format!("pin {name}");
            let r = scale.rect(rect, &what)?;
            for t in layers.targets(layer, Purpose::Pin)? {
                w.rectangle(t.0, t.1, r)?;
                counts.pin_shapes += 1;
            }
            let label = layers.entry(layer)?.label.ok_or_else(|| {
                fail(
                    "GDS_LAYER_UNMAPPED",
                    format!("{layer} has no label target in the layer map"),
                )
            })?;
            let (cx, cy) = (rect.lx + rect.ux, rect.ly + rect.uy);
            if cx % 2 != 0 || cy % 2 != 0 {
                return Err(fail(
                    "GDS_ODD_COORDINATE",
                    format!("{what}: label centre is not on the LEF grid"),
                ));
            }
            let at = scale.point([cx / 2, cy / 2], &what)?;
            w.text(label.0, label.1, at, label_text(name)?)?;
            counts.labels += 1;
        }
    }
    let boundary = scale.rect(design.die_area, "die area")?;
    w.rectangle(map.boundary.layer.0, map.boundary.layer.1, boundary)?;
    counts.boundary = 1;
    w.end_structure()?;
    w.end_library()?;

    let records = w.records_written();
    let bytes = w.bytes_written();
    let sink = w.into_inner()?;
    let hashing = sink
        .into_inner()
        .map_err(|e| fail("GDS_IO", e.error().to_string()))?;
    hashing.inner.sync_all()?;
    let sha256 = format!("{:x}", hashing.hasher.finalize());
    drop(hashing.inner);
    fs::rename(&guard.path, &target)?;
    guard.armed = false;
    File::open(&parent)?.sync_all()?;

    Ok(GdsReport {
        status: "written",
        revision: meta.revision.clone(),
        top: top.to_string(),
        output: FileIdentity {
            path: target,
            sha256,
            bytes,
        },
        records,
        cell_gds: cell_id,
        layer_map: map_id,
        layer_map_pdk: map.pdk.clone(),
        cell_lef: lef_id,
        foreign_bindings_checked: foreign,
        lef_dbu_per_micron: lef_dbu,
        gds_dbu_per_micron: gds_dbu,
        coordinate_divisor: scale.divisor,
        pdk_structures: closure
            .iter()
            .map(|&i| library.structures[i].name.clone())
            .collect(),
        via_structures: via_cells.values().cloned().collect(),
        elements: counts,
        routing_status,
        drc_status: "unchecked",
        lvs_status: "unchecked",
        scope: "block_level_no_pad_or_seal_ring",
        workspace_mutated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordinates_divide_exactly_or_fail_explicitly() {
        let half = Scale { divisor: 2 };
        assert_eq!(half.point([118_270, -40], "w").unwrap(), [59_135, -20]);
        let error = half.coord(118_271, "wire").unwrap_err();
        assert!(
            error.to_string().starts_with("GDS_ODD_COORDINATE"),
            "{error}"
        );
        assert!(
            half.rect(rect_of([0, 0, 3, 4]), "via")
                .unwrap_err()
                .to_string()
                .contains("GDS_ODD_COORDINATE")
        );
        let unit = Scale { divisor: 1 };
        assert_eq!(unit.coord(-7, "x").unwrap(), -7);
        assert!(
            unit.coord(i64::from(i32::MAX) + 1, "x")
                .unwrap_err()
                .to_string()
                .starts_with("GDS_COORDINATE_RANGE")
        );
    }
    #[test]
    fn labels_must_be_unambiguous() {
        assert_eq!(label_text("a[1]").unwrap(), "a[1]");
        for bad in ["", "\\esc", "a b", "tab\t"] {
            assert!(label_text(bad).is_err(), "{bad:?}");
        }
    }
}
