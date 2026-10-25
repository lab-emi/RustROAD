//! Locked, immutable disk snapshots. Only HEAD publication changes visible state.
use fs2::FileExt;
use memmap2::Mmap;
use rustroad_core::{Design, DesignDb, Limits, Placed, place_cpu_ref};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub mod access_test;
pub mod antenna;
pub mod clock;
pub mod eco;
pub mod extract;
pub mod floorplan;
pub mod gds;
pub mod pdk;
pub mod physical;
pub mod routing;
pub mod ties;
use rustroad_core::connectivity::{ConnectedDesign, ConnectivityLimits, PinOwner};

/// design.json cap (also bounds the schema-5 mirrors). 64 MiB holds the
/// netlist, connectivity and filler records of ~25k-instance blocks; route
/// records of schema 6 live in routes.rbin (`routing::MAX_ROUTE_TABLE_BYTES`).
pub const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;
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
    /// Coded stream-export failure (e.g. GDS_ODD_COORDINATE, GDS_CELL_MISSING).
    #[error("{code}: {message}")]
    Gds { code: &'static str, message: String },
    /// Coded failure of a file-only command (hold-eco, apply-eco,
    /// antenna-eco) or of a command's own input file: no workspace is
    /// involved, so the code is never INVALID_WORKSPACE (e.g. STALE_REPORT,
    /// ECO_APPLY, TIMING_REPORT, OUTPUT_PATH).
    #[error("{code}: {message}")]
    Input { code: &'static str, message: String },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical: Option<pdk::PhysicalBundle>,
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
    pub nets: Option<usize>,
    pub pins: Option<usize>,
    pub masters: Option<usize>,
    pub connectivity_status: &'static str,
    pub power_grid_status: &'static str,
    pub io_placement_status: &'static str,
    pub diagnostics: Vec<pdk::Diagnostic>,
    pub inserted_tie_cells: Option<usize>,
    pub filler_cells: Option<usize>,
    pub rail_continuity_status: &'static str,
    pub body_bias_status: &'static str,
    pub tap_cells: Option<usize>,
    pub tap_placement_status: &'static str,
    pub clock_buffers: Option<usize>,
    pub clock_topology_status: &'static str,
    pub clock_constraint_status: &'static str,
    pub routed_nets: Option<usize>,
    pub routing_segments: Option<usize>,
    pub routing_expansions: Option<usize>,
    /// Analytic placement record (absent for fixed-order packing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<physical::PlacementReport>,
    /// Track-router memory estimate of this route command (route only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_memory: Option<routing::RouteMemory>,
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
            routing_status: match self.physical.as_ref().and_then(|p| p.routing.as_ref()) {
                Some(r) if r.scope == routing::RouteScope::Clock => "clock_geometry_checked",
                Some(_) => "signal_geometry_checked",
                None => "not_run",
            },
            corner: "not_applicable",
            wire_model: "none",
            unconstrained_endpoints: None,
            nets: self.physical.as_ref().map(|p| p.connectivity.nets.len()),
            pins: self.physical.as_ref().map(|p| p.connectivity.pins.len()),
            masters: self.physical.as_ref().map(|p| p.connectivity.masters.len()),
            connectivity_status: if self.physical.is_some() {
                "linked_unverified"
            } else {
                "not_imported"
            },
            power_grid_status: if self.physical.as_ref().is_some_and(|p| p.state.is_some()) {
                "primary_grid_generated"
            } else {
                "unchecked"
            },
            io_placement_status: if self.physical.as_ref().is_some_and(|p| p.state.is_some()) {
                "placed_geometry_checked"
            } else {
                "not_placed"
            },
            diagnostics: self
                .physical
                .as_ref()
                .map_or_else(Vec::new, |p| p.diagnostics.clone()),
            inserted_tie_cells: self
                .physical
                .as_ref()
                .and_then(|p| p.ties.as_ref().map(|t| t.new_cells)),
            filler_cells: self
                .physical
                .as_ref()
                .and_then(|p| p.state.as_ref().map(|s| s.fillers_added)),
            rail_continuity_status: if self.physical.as_ref().is_some_and(|p| p.state.is_some()) {
                "geometry_checked"
            } else {
                "unchecked"
            },
            body_bias_status: if self
                .physical
                .as_ref()
                .is_some_and(|p| p.state.as_ref().is_some_and(|s| s.tap.is_some()))
            {
                "tap_inserted_contact_unverified"
            } else if self.physical.as_ref().is_some_and(|p| p.config.is_some()) {
                "logically_connected"
            } else {
                "unchecked"
            },
            tap_cells: self.physical.as_ref().map(|p| p.generated_taps.len()),
            tap_placement_status: if self
                .physical
                .as_ref()
                .is_some_and(|p| p.state.as_ref().is_some_and(|s| s.tap.is_some()))
            {
                "geometry_checked"
            } else {
                "unchecked"
            },
            clock_buffers: self
                .physical
                .as_ref()
                .and_then(|p| p.clock_tree.as_ref().map(|t| t.buffers.len())),
            clock_topology_status: if self
                .physical
                .as_ref()
                .is_some_and(|p| p.clock_tree.is_some())
            {
                if self.physical.as_ref().is_some_and(|p| p.routing.is_some()) {
                    "routed_geometry_checked"
                } else {
                    "unrouted_topology_checked"
                }
            } else {
                "not_run"
            },
            clock_constraint_status: if self
                .physical
                .as_ref()
                .and_then(|p| p.clock_tree.as_ref())
                .is_some_and(|c| c.constraints.is_some())
            {
                "explicit_sdc"
            } else {
                "unverified"
            },
            routed_nets: self
                .physical
                .as_ref()
                .and_then(|p| p.routing.as_ref().map(|r| r.routes.nets.len())),
            routing_segments: self.physical.as_ref().and_then(|p| {
                p.routing
                    .as_ref()
                    .map(|r| r.routes.wires.len() + r.routes.vias.len())
            }),
            routing_expansions: self
                .physical
                .as_ref()
                .and_then(|p| p.routing.as_ref().map(|r| r.routes.stats.expansions)),
            placement: self.physical.as_ref().and_then(|p| p.placement.clone()),
            route_memory: None,
        }
    }
}

pub struct Snapshot {
    // Held while mappings are alive; readers must never observe an unpublished snapshot.
    _lock: File,
    metadata: Metadata,
    map: Mmap,
    _connections: Option<ConnectionMaps>,
    _layout: Option<Mmap>,
    _routing: Option<Mmap>,
}
struct ConnectionMaps {
    _pins: Option<Mmap>,
    _net_offsets: Mmap,
    _net_pins: Option<Mmap>,
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
        if let Some(physical) = &self.metadata.physical {
            if physical.config.is_some() {
                crate::physical::export_def(&self.metadata.design, &records, physical, output)?;
            } else {
                rustroad_core::connectivity::write_connected_def(
                    &self.metadata.design,
                    &records,
                    &physical.connectivity,
                    &ConnectivityLimits::default(),
                    output,
                )?;
            }
        } else {
            rustroad_core::write_def(&self.metadata.design, &records, output)?;
        }
        Ok(())
    }
    pub fn export_verilog(&self, output: &mut impl Write) -> Result<()> {
        self.export_verilog_with_power(false, output)
    }
    pub fn export_verilog_with_power(
        &self,
        include_power: bool,
        output: &mut impl Write,
    ) -> Result<()> {
        let physical = self
            .metadata
            .physical
            .as_ref()
            .ok_or_else(|| Error::Invalid("no mapped netlist imported".into()))?;
        crate::physical::export_verilog(physical, include_power, output)?;
        Ok(())
    }
    pub fn connectivity(&self) -> Option<&ConnectedDesign> {
        self.metadata.physical.as_ref().map(|p| &p.connectivity)
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
        physical: None,
    };
    let result = commit(path, metadata, &records)?;
    drop(lock);
    Ok(result)
}

pub fn place(path: &Path, base_rev: &str, limits: &Limits) -> Result<Summary> {
    place_weighted(path, base_rev, limits, None)
}

pub fn place_weighted(
    path: &Path,
    base_rev: &str,
    limits: &Limits,
    weights: Option<&Path>,
) -> Result<Summary> {
    place_with(path, base_rev, limits, weights, None)
}

/// `place` with optional timing-driven net weights (`rustroad.net-weights.v1`).
/// The weights are stored with the physical bundle so that every later
/// re-placement of the workspace (clock-tree insertion) uses the same ones;
/// `None` keeps whatever the workspace already has.
/// `place` with optional timing-driven net weights and an ECO placement
/// seed (`rustroad.placement-seed.v1`, e.g. the previous layout); both are
/// stored with the physical bundle like the weights.
pub fn place_with(
    path: &Path,
    base_rev: &str,
    limits: &Limits,
    weights: Option<&Path>,
    seed: Option<&Path>,
) -> Result<Summary> {
    let seed = match seed {
        Some(file) => {
            let bytes = read_bounded(file, 64 << 20)?;
            let s: physical::PlacementSeed = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Invalid(format!("placement seed: {e}")))?;
            s.validate()?;
            Some(s)
        }
        None => None,
    };
    let weights = match weights {
        Some(file) => {
            let bytes = read_bounded(file, 64 << 20)?;
            let w: physical::NetWeights = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Invalid(format!("net weights: {e}")))?;
            w.validate()?;
            Some(w)
        }
        None => None,
    };
    let lock = lock(path, false, true)?;
    let current = head(path)?;
    check_base(base_rev, &current)?;
    let snapshot = load(path, lock, limits)?;
    let mut metadata = snapshot.metadata.clone();
    if let Some(w) = weights {
        let bundle = metadata.physical.as_mut().ok_or_else(|| {
            Error::Invalid("net weights need a physical (import-pdk) workspace".into())
        })?;
        if bundle.config.as_ref().is_none_or(|c| c.placer.is_none()) {
            return Err(Error::Invalid(
                "net weights need the analytic placer (physical config `placer`)".into(),
            ));
        }
        bundle.net_weights = Some(w);
    }
    if let Some(sd) = seed {
        let bundle = metadata.physical.as_mut().ok_or_else(|| {
            Error::Invalid("a placement seed needs a physical (import-pdk) workspace".into())
        })?;
        if bundle.config.as_ref().is_none_or(|c| c.placer.is_none()) {
            return Err(Error::Invalid(
                "a placement seed needs the analytic placer (physical config `placer`)".into(),
            ));
        }
        bundle.placement_seed = Some(sd);
    }
    if let Some(bundle) = &mut metadata.physical
        && bundle.config.is_some()
    {
        crate::physical::clear_fillers(&mut metadata.design, bundle)?;
    }
    let db = DesignDb::new(&metadata.design, limits)?;
    let placement = place_cpu_ref(&db, limits)?;
    let mut records = placement.cells;
    let mut expansions = placement.expansions;
    if let Some(bundle) = &mut metadata.physical
        && bundle.config.is_some()
    {
        let (p, e) = crate::physical::prepare(&mut metadata.design, bundle, records, limits)?;
        records = p;
        expansions = expansions
            .checked_add(e)
            .ok_or_else(|| Error::Budget("placement work overflow".into()))?;
    }
    if expansions > limits.max_expansions {
        return Err(Error::Budget(
            "physical placement expansions exceeded".into(),
        ));
    }
    metadata.parent_revision = current;
    metadata.stage = Stage::Placed;
    metadata.expansions = expansions;
    // Keep the exclusive lock for the entire calculation and publication.
    commit(path, metadata, &records)
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
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
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
    let mut metadata = metadata;
    if !matches!(metadata.format_version, 1..=6)
        || metadata.revision != current
        || revision_number(&metadata.parent_revision)? >= revision_number(&current)?
    {
        return Err(Error::Invalid("snapshot schema/revision mismatch".into()));
    }
    metadata.design.validate(limits)?;
    if (metadata.format_version >= 2) != metadata.physical.is_some()
        || (metadata.format_version >= 3)
            != metadata
                .physical
                .as_ref()
                .is_some_and(|p| p.config.is_some())
    {
        return Err(Error::Invalid("connected schema/payload mismatch".into()));
    }
    if (metadata.format_version >= 4)
        != metadata.physical.as_ref().is_some_and(|p| {
            p.routing.is_some()
                || p.config
                    .as_ref()
                    .is_some_and(|c| c.tap.is_some() || c.clock.is_some())
        })
    {
        return Err(Error::Invalid("tap schema/configuration mismatch".into()));
    }
    if (metadata.format_version >= 5)
        != metadata
            .physical
            .as_ref()
            .is_some_and(|p| p.routing.is_some())
        || (metadata.format_version == 6)
            != metadata
                .physical
                .as_ref()
                .and_then(|p| p.routing.as_ref())
                .is_some_and(|r| r.table.is_some() && r.config.track_router.is_some())
    {
        return Err(Error::Invalid("routing schema/payload mismatch".into()));
    }
    // Schema 6: the route records live in the authoritative routes.rbin.
    if metadata.format_version == 6 {
        let routing = metadata
            .physical
            .as_mut()
            .and_then(|p| p.routing.as_mut())
            .unwrap();
        let table = routing.table.clone().unwrap();
        let r = &routing.routes;
        if table.file != routing::ROUTE_TABLE
            || !r.wires.is_empty()
            || !r.vias.is_empty()
            || !r.access.is_empty()
            || !r.patches.is_empty()
            || !r.nets.is_empty()
        {
            return Err(Error::Invalid(
                "schema 6 routes must live only in routes.rbin".into(),
            ));
        }
        let bytes = read_bounded(
            &revision_dir.join(routing::ROUTE_TABLE),
            routing::MAX_ROUTE_TABLE_BYTES as u64,
        )?;
        let mut decoded = routing::decode_route_table(&bytes, &table)?;
        decoded.stats = routing.routes.stats.clone();
        routing.routes = decoded;
    }
    let connections = if let Some(physical) = &metadata.physical {
        pdk::validate_bundle(physical, &metadata.design, &ConnectivityLimits::default())?;
        Some(read_connection_tables(
            &revision_dir,
            &metadata.design,
            &physical.connectivity,
        )?)
    } else {
        None
    };
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
        _connections: connections,
        _layout: None,
        _routing: None,
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
    let mut snapshot = snapshot;
    if snapshot.metadata.stage == Stage::Placed {
        check_placed(&snapshot)?;
        if let Some(bundle) = &snapshot.metadata.physical
            && bundle.config.is_some()
        {
            let records = snapshot.records().collect::<Vec<_>>();
            crate::physical::validate_state(&snapshot.metadata.design, bundle, &records)?;
            let expected = crate::physical::table_bytes(bundle.state.as_ref().unwrap())?;
            let map = map_table(&revision_dir.join("physical.rbin"), expected.len())?
                .ok_or_else(|| Error::Invalid("empty prepared geometry table".into()))?;
            if map[..] != expected {
                return Err(Error::Invalid("prepared geometry table mismatch".into()));
            }
            snapshot._layout = Some(map);
            crate::routing::validate_state(&snapshot.metadata.design, bundle, &records)?;
            if let Some(routing) = bundle.routing.as_ref().filter(|r| r.table.is_none()) {
                let expected = crate::routing::table_bytes(routing)?;
                let map = map_table(&revision_dir.join("routing.rbin"), expected.len())?;
                if map.as_ref().map_or(&[][..], |m| &m[..]) != expected {
                    return Err(Error::Invalid("routing table mismatch".into()));
                }
                snapshot._routing = map;
            }
        }
    } else if snapshot
        .metadata
        .physical
        .as_ref()
        .is_some_and(|p| p.state.is_some())
    {
        return Err(Error::Invalid(
            "imported snapshot has prepared state".into(),
        ));
    }
    Ok(snapshot)
}

fn check_placed(snapshot: &Snapshot) -> Result<()> {
    let records = snapshot.records().collect::<Vec<_>>();
    rustroad_core::validate_placement(&snapshot.metadata.design, &records, &Limits::default())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

fn commit(path: &Path, mut metadata: Metadata, records: &[Placed]) -> Result<Summary> {
    if let Some(p) = &metadata.physical {
        metadata.format_version = if p
            .routing
            .as_ref()
            .is_some_and(|r| r.config.track_router.is_some())
        {
            6
        } else if p.routing.is_some() {
            5
        } else if p
            .config
            .as_ref()
            .is_some_and(|c| c.tap.is_some() || c.clock.is_some())
        {
            4
        } else if p.config.is_some() {
            3
        } else {
            2
        };
        crate::routing::validate_state(&metadata.design, p, records)?;
    }
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
    // Schema 6: move the route records out of design.json into routes.rbin.
    let mut held = None;
    let mut route_table = None;
    if metadata.format_version == 6 {
        let r = metadata
            .physical
            .as_mut()
            .and_then(|p| p.routing.as_mut())
            .unwrap();
        let bytes = routing::route_table_bytes(&r.routes)?;
        use sha2::{Digest, Sha256};
        r.table = Some(routing::RoutesTable {
            file: routing::ROUTE_TABLE.into(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
            wires: r.routes.wires.len(),
            vias: r.routes.vias.len(),
            access: r.routes.access.len(),
            patches: r.routes.patches.len(),
            nets: r.routes.nets.len(),
        });
        let empty = rustroad_core::routing::Routes {
            wires: Vec::new(),
            vias: Vec::new(),
            access: Vec::new(),
            patches: Vec::new(),
            nets: Vec::new(),
            stats: r.routes.stats.clone(),
        };
        held = Some(std::mem::replace(&mut r.routes, empty));
        route_table = Some(bytes);
    }
    let bytes = serialize_bounded(&metadata)?;
    if let Some(routes) = held
        && let Some(r) = metadata.physical.as_mut().and_then(|p| p.routing.as_mut())
    {
        r.routes = routes;
    }
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
    if let Some(physical) = &metadata.physical {
        pdk::validate_bundle(physical, &metadata.design, &ConnectivityLimits::default())?;
        write_connection_tables(&directory, &metadata.design, &physical.connectivity)?;
        if let Some(table) = &route_table {
            write_new(&directory.join(routing::ROUTE_TABLE), table)?;
        } else if let Some(routing) = &physical.routing {
            write_new(
                &directory.join("routing.rbin"),
                &crate::routing::table_bytes(routing)?,
            )?;
        }
        if let Some(state) = &physical.state {
            write_new(
                &directory.join("physical.rbin"),
                &crate::physical::table_bytes(state)?,
            )?;
        }
    }
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

fn owner_record(pin: &rustroad_core::connectivity::Connection) -> [u32; 4] {
    match pin.owner {
        PinOwner::Instance { instance, terminal } => {
            [0, instance, terminal, pin.net.unwrap_or(u32::MAX)]
        }
        PinOwner::Port { port, bit } => [1, port, bit, pin.net.unwrap_or(u32::MAX)],
    }
}
fn write_connection_tables(
    directory: &Path,
    design: &Design,
    model: &ConnectedDesign,
) -> Result<()> {
    let csr = model.lower_csr(design, &ConnectivityLimits::default())?;
    let write_table =
        |name: &str, produce: &mut dyn FnMut(&mut BufWriter<File>) -> Result<()>| -> Result<()> {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))?;
            let mut out = BufWriter::with_capacity(256 * 1024, file);
            produce(&mut out)?;
            out.flush()?;
            out.get_ref().sync_all()?;
            Ok(())
        };
    write_table("pins.rbin", &mut |out| {
        for pin in &model.pins {
            for field in owner_record(pin) {
                out.write_all(&field.to_le_bytes())?;
            }
        }
        Ok(())
    })?;
    write_table("net_offsets.rbin", &mut |out| {
        for offset in &csr.offsets {
            out.write_all(&offset.to_le_bytes())?;
        }
        Ok(())
    })?;
    write_table("net_pins.rbin", &mut |out| {
        for pin in &csr.pin_indices {
            out.write_all(&pin.to_le_bytes())?;
        }
        Ok(())
    })?;
    Ok(())
}
fn map_table(path: &Path, bytes: usize) -> Result<Option<Mmap>> {
    if !fs::metadata(path)?.is_file() {
        return Err(Error::Invalid("side table must be a regular file".into()));
    }
    let file = File::open(path)?;
    if file.metadata()?.len() != bytes as u64 {
        return Err(Error::Invalid(format!(
            "{}: connection table length mismatch",
            path.display()
        )));
    }
    if bytes == 0 {
        return Ok(None);
    }
    // SAFETY: immutable published files; snapshot retains the workspace lock.
    #[allow(unsafe_code)]
    let map = unsafe { Mmap::map(&file)? };
    Ok(Some(map))
}
fn read_connection_tables(
    directory: &Path,
    design: &Design,
    model: &ConnectedDesign,
) -> Result<ConnectionMaps> {
    let csr = model.lower_csr(design, &ConnectivityLimits::default())?;
    let pins = map_table(
        &directory.join("pins.rbin"),
        model
            .pins
            .len()
            .checked_mul(16)
            .ok_or_else(|| Error::Budget("pin table overflow".into()))?,
    )?;
    let offsets = map_table(
        &directory.join("net_offsets.rbin"),
        csr.offsets
            .len()
            .checked_mul(8)
            .ok_or_else(|| Error::Budget("offset table overflow".into()))?,
    )?
    .ok_or_else(|| Error::Invalid("missing offset sentinel".into()))?;
    let net_pins = map_table(
        &directory.join("net_pins.rbin"),
        csr.pin_indices
            .len()
            .checked_mul(4)
            .ok_or_else(|| Error::Budget("CSR table overflow".into()))?,
    )?;
    for (bytes, expected) in pins
        .as_deref()
        .unwrap_or(&[])
        .chunks_exact(16)
        .zip(&model.pins)
    {
        for (b, field) in bytes.chunks_exact(4).zip(owner_record(expected)) {
            if u32::from_le_bytes(b.try_into().unwrap()) != field {
                return Err(Error::Invalid(
                    "pin side table differs from metadata".into(),
                ));
            }
        }
    }
    for (bytes, expected) in offsets.chunks_exact(8).zip(csr.offsets) {
        if u64::from_le_bytes(bytes.try_into().unwrap()) != expected {
            return Err(Error::Invalid("net offsets differ from metadata".into()));
        }
    }
    for (bytes, expected) in net_pins
        .as_deref()
        .unwrap_or(&[])
        .chunks_exact(4)
        .zip(csr.pin_indices)
    {
        if u32::from_le_bytes(bytes.try_into().unwrap()) != expected {
            return Err(Error::Invalid("net CSR differs from metadata".into()));
        }
    }
    Ok(ConnectionMaps {
        _pins: pins,
        _net_offsets: offsets,
        _net_pins: net_pins,
    })
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
                orientation: rustroad_core::Orientation::N,
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
