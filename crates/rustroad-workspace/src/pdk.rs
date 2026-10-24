//! Bounded technology/netlist import. See docs/pdk-import-budget.md before changes.
use crate::{
    Error, Metadata, Result, Stage, Summary, check_base, commit, head, lock, read_bounded,
};
use rustroad_core::connectivity::{
    ConnectedDesign, Connection, ConnectivityDb, ConnectivityLimits, Direction, Geometry, Master,
    MasterPin, Net, PinOwner, PinUse, Port,
};
use rustroad_core::{Cell, Design, DesignDb, Limits, Rect, Row, Site};
use rustroad_formats::{lef, verilog};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_LEF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NETLIST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_AGGREGATE_BYTES: u64 = 144 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportSources {
    pub technology_lef: PathBuf,
    pub cell_lef: PathBuf,
    pub netlist: PathBuf,
    pub floorplan: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePin {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    pub code: String,
    pub source: String,
    pub message: String,
    pub occurrences: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Technology {
    pub dbu_per_micron: u32,
    pub manufacturing_grid: Option<i64>,
    pub layers: Vec<lef::LefLayer>,
    pub sites: Vec<lef::LefSite>,
    pub vias: Vec<lef::LefVia>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalBundle {
    pub import_version: u32,
    pub technology: Technology,
    pub netlist: verilog::Netlist,
    pub connectivity: ConnectedDesign,
    pub sources: Vec<SourcePin>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub config: Option<crate::physical::PhysicalConfig>,
    #[serde(default)]
    pub ties: Option<crate::ties::TieReport>,
    #[serde(default)]
    pub generated_fillers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_taps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_tree: Option<crate::clock::ClockReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<crate::routing::RoutingState>,
    #[serde(default)]
    pub state: Option<crate::physical::PhysicalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<crate::physical::PlacementReport>,
    /// Timing-driven net weights (`place --net-weights`); every later
    /// placement of this workspace (clock-tree re-placement) uses them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_weights: Option<crate::physical::NetWeights>,
    /// ECO placement seed (`place --seed`); kept for the clock-tree re-placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_seed: Option<crate::physical::PlacementSeed>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Floorplan {
    pub(crate) name: String,
    pub(crate) units_per_micron: u32,
    pub(crate) die_area: Rect,
    pub(crate) core_area: Rect,
    pub(crate) site: Site,
    pub(crate) rows: Vec<Row>,
}
pub(crate) type FloorplanSpec = Floorplan;
/// `select_technology` for the file-only floorplan command.
pub(crate) fn select_technology_for(
    tech: &lef::LefDocument,
    cells: &lef::LefDocument,
    floor: &Floorplan,
    netlist: &verilog::Netlist,
    extra: &[String],
) -> Result<(Technology, Vec<Master>)> {
    select_technology(tech, cells, floor, netlist, extra)
}

pub(crate) fn read_input(path: &Path, limit: u64, role: &str) -> Result<(String, SourcePin)> {
    let bytes = read_bounded(path, limit)?;
    let pin = SourcePin {
        role: role.into(),
        path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes: bytes.len(),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Invalid(format!("{role}: input is not UTF-8")))?;
    Ok((text, pin))
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
fn budget(message: impl Into<String>) -> Error {
    Error::Budget(message.into())
}

fn input_preflight(sources: &ImportSources) -> Result<()> {
    let mut total = 0u64;
    for (path, limit) in [
        (&sources.technology_lef, MAX_LEF_BYTES),
        (&sources.cell_lef, MAX_LEF_BYTES),
        (&sources.netlist, MAX_NETLIST_BYTES),
        (&sources.floorplan, MAX_NETLIST_BYTES),
    ] {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(invalid("PDK import requires regular input files"));
        }
        if metadata.len() > limit {
            return Err(budget("PDK input file byte budget exceeded"));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| budget("PDK input byte count overflow"))?;
        if total > MAX_AGGREGATE_BYTES {
            return Err(budget("PDK aggregate source budget exceeded"));
        }
    }
    Ok(())
}

pub(crate) fn validate_bundle(
    bundle: &PhysicalBundle,
    design: &Design,
    limits: &ConnectivityLimits,
) -> Result<()> {
    if bundle.import_version != 1
        || bundle.netlist.module != design.name
        || bundle.technology.dbu_per_micron != design.units_per_micron
        || bundle.netlist.instances.len() != design.cells.len()
        || bundle.sources.len() != if bundle.config.is_some() { 5 } else { 4 }
        || bundle.diagnostics.len() > 1024
    {
        return Err(invalid("invalid physical import metadata"));
    }
    if bundle.config.is_some() != bundle.ties.is_some() {
        return Err(invalid("physical configuration/tie report mismatch"));
    }
    for source in &bundle.sources {
        if source.sha256.len() != 64
            || !source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || source.bytes > MAX_LEF_BYTES as usize
        {
            return Err(invalid("invalid source pin"));
        }
    }
    if bundle.sources.iter().map(|p| p.bytes as u64).sum::<u64>() > MAX_AGGREGATE_BYTES {
        return Err(budget("stored PDK source aggregate limit exceeded"));
    }
    verilog::validate_netlist(&bundle.netlist, &verilog::VerilogLimits::default())
        .map_err(|e| invalid(e.to_string()))?;
    for (instance, cell) in bundle.netlist.instances.iter().zip(&design.cells) {
        if instance.name != cell.name || instance.cell != cell.master {
            return Err(invalid("netlist instances and geometry differ"));
        }
    }
    if bundle
        .technology
        .layers
        .iter()
        .map(|l| &l.name)
        .ne(bundle.connectivity.layers.iter())
    {
        return Err(invalid("technology/connection layers differ"));
    }
    bundle.connectivity.validate(design, limits)?;
    // Individually valid AST and connection tables must describe the same design.
    let (mut ports, mut nets, mut pins) = link(
        &bundle.netlist,
        &bundle.connectivity.masters,
        &bundle.connectivity.instance_masters,
        limits,
    )?;
    if let Some(cfg) = &bundle.config {
        let generated = bundle
            .generated_fillers
            .len()
            .checked_add(bundle.generated_taps.len())
            .and_then(|n| n.checked_add(bundle.clock_tree.as_ref().map_or(0, |t| t.buffers.len())))
            .and_then(|n| n.checked_add(bundle.ties.as_ref().map_or(0, |t| t.new_cells)))
            .ok_or_else(|| budget("generated physical-cell count overflow"))?;
        if generated > cfg.max_added_cells {
            return Err(budget("total generated physical-cell budget exceeded"));
        }
        crate::physical::attach_supplies(
            &bundle.connectivity.masters,
            &bundle.connectivity.instance_masters,
            &mut ports,
            &mut nets,
            &mut pins,
            cfg,
            limits,
        )?;
        let instance_names: HashMap<_, _> = bundle
            .netlist
            .instances
            .iter()
            .map(|i| (i.name.as_str(), i))
            .collect();
        let mut names = HashSet::new();
        for name in &bundle.generated_fillers {
            if !names.insert(name) {
                return Err(invalid("duplicate generated filler"));
            }
            let inst = instance_names
                .get(name.as_str())
                .ok_or_else(|| invalid("generated filler missing"))?;
            if !cfg.filler_cells.contains(&inst.cell) || !inst.connections.is_empty() {
                return Err(invalid("invalid generated filler"));
            }
        }
        crate::physical::validate_ties(bundle)?;
        crate::physical::validate_tap_master(bundle)?;
        crate::clock::validate_tree(bundle)?;
        for name in &bundle.generated_taps {
            if !names.insert(name) {
                return Err(invalid("duplicate generated physical instance"));
            }
            let inst = instance_names
                .get(name.as_str())
                .ok_or_else(|| invalid("generated tap absent"))?;
            if Some(inst.cell.as_str()) != cfg.tap.as_ref().map(|t| t.cell.as_str())
                || !inst.connections.is_empty()
            {
                return Err(invalid("invalid generated tap"));
            }
        }
    } else if bundle.ties.is_some()
        || bundle.clock_tree.is_some()
        || bundle.routing.is_some()
        || bundle.state.is_some()
        || !bundle.generated_fillers.is_empty()
        || !bundle.generated_taps.is_empty()
    {
        return Err(invalid("physical state without configuration"));
    }
    if ports != bundle.connectivity.ports
        || nets != bundle.connectivity.nets
        || pins != bundle.connectivity.pins
    {
        return Err(invalid("logical netlist and connection tables differ"));
    }
    Ok(())
}

pub fn import_pdk(
    path: &Path,
    base_rev: &str,
    sources: &ImportSources,
    limits: &Limits,
    connection_limits: &ConnectivityLimits,
) -> Result<Summary> {
    import_pdk_with_config(path, base_rev, sources, limits, connection_limits, None)
}

pub fn import_pdk_with_config(
    path: &Path,
    base_rev: &str,
    sources: &ImportSources,
    limits: &Limits,
    connection_limits: &ConnectivityLimits,
    config_path: Option<&Path>,
) -> Result<Summary> {
    let _lock = lock(path, true, true)?;
    let current = head(path)?;
    check_base(base_rev, &current)?;
    input_preflight(sources)?;
    let (floor_text, floor_pin) = read_input(&sources.floorplan, MAX_NETLIST_BYTES, "floorplan")?;
    let floor: Floorplan = serde_json::from_str(&floor_text)?;
    if floor.rows.len() > limits.max_rows {
        return Err(budget("floorplan max_rows exceeded"));
    }
    let vlimits = verilog::VerilogLimits {
        max_instances: limits.max_cells,
        max_connections: connection_limits.max_pins,
        max_nets: connection_limits.max_nets,
        max_expr_nodes: connection_limits.max_pins,
        ..Default::default()
    };
    let (verilog_text, verilog_pin) =
        read_input(&sources.netlist, MAX_NETLIST_BYTES, "mapped_verilog")?;
    let mut netlist =
        verilog::parse_verilog(&verilog_text, &vlimits).map_err(|e| invalid(e.to_string()))?;
    drop(verilog_text);
    if floor.name != netlist.module {
        return Err(invalid("floorplan name does not match the mapped module"));
    }
    let llef = lef::LefLimits {
        max_input_bytes: MAX_LEF_BYTES as usize,
        ..Default::default()
    };
    let (tech_text, tech_pin) =
        read_input(&sources.technology_lef, MAX_LEF_BYTES, "technology_lef")?;
    let tech = lef::parse_lef(&tech_text, floor.units_per_micron, &llef)
        .map_err(|e| invalid(e.to_string()))?;
    drop(tech_text);
    let (cell_text, cell_pin) = read_input(&sources.cell_lef, MAX_LEF_BYTES, "cell_lef")?;
    let actual_bytes = [&floor_pin, &verilog_pin, &tech_pin, &cell_pin]
        .iter()
        .map(|p| p.bytes as u64)
        .sum::<u64>();
    if actual_bytes > MAX_AGGREGATE_BYTES {
        return Err(budget("PDK aggregate source budget exceeded during read"));
    }
    let cells = lef::parse_lef(&cell_text, floor.units_per_micron, &llef)
        .map_err(|e| invalid(e.to_string()))?;
    drop(cell_text);
    let (config, config_pin) = if let Some(path) = config_path {
        let (text, pin) = read_input(path, 1024 * 1024, "physical_config")?;
        let cfg: crate::physical::PhysicalConfig = serde_json::from_str(&text)?;
        cfg.validate()?;
        (Some(cfg), Some(pin))
    } else {
        (None, None)
    };
    let mut tie_report = None;
    if let Some(cfg) = &config {
        let find = |name: &str| {
            tech.macros
                .iter()
                .chain(&cells.macros)
                .find(|m| m.name == name)
                .ok_or_else(|| invalid(format!("missing tie master {name}")))
        };
        let specs = cfg
            .constant_cells
            .iter()
            .map(|c| {
                Ok(crate::ties::ConstantCellSpec {
                    cell: find(&c.cell)?,
                    high: &c.high,
                    low: &c.low,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let high = crate::ties::TieSpec {
            cell: find(&cfg.tie_high.cell)?,
            pin: &cfg.tie_high.pin,
            rail: cfg.tie_high.rail(),
        };
        let low = crate::ties::TieSpec {
            cell: find(&cfg.tie_low.cell)?,
            pin: &cfg.tie_low.pin,
            rail: cfg.tie_low.rail(),
        };
        let (dissolved, count) = crate::ties::dissolve_constant_cells(
            &netlist,
            &specs,
            high,
            low,
            &crate::ties::TieLimits::default(),
        )
        .map_err(|e| invalid(e.to_string()))?;
        let (updated, mut report) = crate::ties::materialize_constants_with(
            &dissolved,
            high,
            low,
            &crate::ties::TieLimits {
                max_new_cells: cfg
                    .max_added_cells
                    .min(limits.max_cells.saturating_sub(netlist.instances.len())),
                ..Default::default()
            },
        )
        .map_err(|e| invalid(e.to_string()))?;
        report.dissolved_cells = count;
        netlist = updated;
        tie_report = Some(report);
    }
    let mut diagnostics = Vec::new();
    for (source, doc) in [("technology_lef", &tech), ("cell_lef", &cells)] {
        for d in &doc.diagnostics {
            diagnostics.push(Diagnostic {
                code: d.code.clone(),
                source: source.into(),
                occurrences: d.occurrences,
                message: format!(
                    "{} {} (first line {}): {}",
                    d.scope, d.keyword, d.first_line, d.message
                ),
            });
        }
    }
    let extra = config.as_ref().map_or_else(Vec::new, |c| {
        let mut v = c.filler_cells.clone();
        v.push(c.tie_high.cell.clone());
        v.push(c.tie_low.cell.clone());
        if let Some(t) = &c.tap {
            v.push(t.cell.clone());
        }
        if let Some(t) = &c.clock {
            v.extend(t.masters());
        }
        v
    });
    let (technology, masters) = select_technology(&tech, &cells, &floor, &netlist, &extra)?;
    drop(tech);
    drop(cells);
    let mut design = Design {
        name: floor.name,
        units_per_micron: floor.units_per_micron,
        die_area: floor.die_area,
        core_area: floor.core_area,
        site: floor.site,
        rows: floor.rows,
        cells: Vec::with_capacity(netlist.instances.len()),
    };
    if config.as_ref().is_some_and(|c| c.alternating_rows) {
        for (i, row) in design.rows.iter_mut().enumerate() {
            row.orientation = if i % 2 == 0 {
                rustroad_core::Orientation::N
            } else {
                rustroad_core::Orientation::FS
            };
        }
    }
    let mut master_indices = HashMap::new();
    for (i, m) in masters.iter().enumerate() {
        master_indices.insert(m.name.as_str(), i as u32);
    }
    let mut instance_masters = Vec::with_capacity(netlist.instances.len());
    for inst in &netlist.instances {
        let id = *master_indices
            .get(inst.cell.as_str())
            .ok_or_else(|| invalid(format!("missing LEF master {}", inst.cell)))?;
        let m = &masters[id as usize];
        design.cells.push(Cell {
            name: inst.name.clone(),
            master: inst.cell.clone(),
            width: m.size[0],
            height: m.size[1],
        });
        instance_masters.push(id);
    }
    design.validate(limits)?;
    let (mut ports, mut nets, mut pins) =
        link(&netlist, &masters, &instance_masters, connection_limits)?;
    if let Some(cfg) = &config {
        crate::physical::attach_supplies(
            &masters,
            &instance_masters,
            &mut ports,
            &mut nets,
            &mut pins,
            cfg,
            connection_limits,
        )?;
    }
    let connectivity = ConnectedDesign {
        layers: technology.layers.iter().map(|l| l.name.clone()).collect(),
        masters,
        instance_masters,
        ports,
        nets,
        pins,
    };
    let db = ConnectivityDb::new(&connectivity, &design, connection_limits)?;
    let (_, _, pin_count) = db.counts();
    drop(db);
    let supplies = connectivity
        .pins
        .iter()
        .filter(|p| match p.owner {
            PinOwner::Instance { instance, terminal } => connectivity.masters
                [connectivity.instance_masters[instance as usize] as usize]
                .pins[terminal as usize]
                .use_
                .is_supply(),
            _ => false,
        })
        .count();
    diagnostics.push(Diagnostic {code:if config.is_some(){"POWER_GRID_PENDING"}else{"POWER_GRID_UNCHECKED"}.into(),source:"physical_import".into(),occurrences:supplies,
        message:if config.is_some(){"Supply pins are logically connected; primary metal network construction and rail verification occur during placement"}else{"LEF power/ground pin geometry is preserved; supply nets and row power continuity have not been constructed or verified"}.into()});
    diagnostics.push(Diagnostic {code:if config.is_some(){"IO_PLACEMENT_PENDING"}else{"IO_PLACEMENT_UNCHECKED"}.into(),source:"physical_import".into(),occurrences:connectivity.ports.iter().map(|p|p.bits.len()).sum(),
        message:if config.is_some(){"Top-level terminals are preserved; boundary I/O placement occurs during placement"}else{"Top-level logical terminals are preserved in DEF PINS; no I/O placement or fabricated port geometry is emitted"}.into()});
    if pin_count > connection_limits.max_pins {
        return Err(budget("max_pins exceeded"));
    }
    let mut source_pins = vec![floor_pin, verilog_pin, tech_pin, cell_pin];
    if let Some(pin) = config_pin {
        source_pins.push(pin);
    }
    if source_pins.iter().map(|p| p.bytes as u64).sum::<u64>() > MAX_AGGREGATE_BYTES {
        return Err(budget("physical import aggregate byte budget"));
    }
    let physical = PhysicalBundle {
        import_version: 1,
        technology,
        netlist,
        connectivity,
        sources: source_pins,
        diagnostics,
        config,
        ties: tie_report,
        generated_fillers: Vec::new(),
        generated_taps: Vec::new(),
        clock_tree: None,
        routing: None,
        state: None,
        placement: None,
        net_weights: None,
        placement_seed: None,
    };
    validate_bundle(&physical, &design, connection_limits)?;
    let records = DesignDb::new(&design, limits)?.unplaced_records();
    let metadata = Metadata {
        format_version: if physical
            .config
            .as_ref()
            .is_some_and(|c| c.tap.is_some() || c.clock.is_some())
        {
            4
        } else if physical.config.is_some() {
            3
        } else {
            2
        },
        revision: String::new(),
        parent_revision: current,
        stage: Stage::Imported,
        design,
        expansions: 0,
        physical: Some(physical),
    };
    commit(path, metadata, &records)
}

fn select_technology(
    tech: &lef::LefDocument,
    cells: &lef::LefDocument,
    floor: &Floorplan,
    netlist: &verilog::Netlist,
    extra: &[String],
) -> Result<(Technology, Vec<Master>)> {
    let mut layer_map = BTreeMap::new();
    for l in tech.layers.iter().chain(&cells.layers) {
        if let Some(old) = layer_map.insert(l.name.clone(), l.clone())
            && old != *l
        {
            return Err(invalid(format!("conflicting LEF layer {}", l.name)));
        }
    }
    let mut site_map = BTreeMap::new();
    for s in tech.sites.iter().chain(&cells.sites) {
        if let Some(old) = site_map.insert(s.name.clone(), s.clone())
            && old != *s
        {
            return Err(invalid(format!("conflicting LEF site {}", s.name)));
        }
    }
    let site = site_map.get(&floor.site.name).ok_or_else(|| {
        invalid(format!(
            "floorplan site {} absent from LEF",
            floor.site.name
        ))
    })?;
    if site.size != [floor.site.width, floor.site.height] {
        return Err(invalid("floorplan site dimensions differ from LEF"));
    }
    let mut library = HashMap::new();
    for m in tech.macros.iter().chain(&cells.macros) {
        if library.insert(m.name.as_str(), m).is_some() {
            return Err(invalid(format!("duplicate LEF macro {}", m.name)));
        }
    }
    let mut seen = HashSet::new();
    let mut masters = Vec::new();
    for name in netlist
        .instances
        .iter()
        .map(|i| i.cell.as_str())
        .chain(extra.iter().map(String::as_str))
    {
        if !seen.insert(name) {
            continue;
        }
        let m = library
            .get(name)
            .ok_or_else(|| invalid(format!("missing LEF master {name}")))?;
        if m.origin != [0, 0] {
            return Err(invalid(format!(
                "unsupported nonzero LEF origin for {}",
                m.name
            )));
        }
        if m.site.as_deref() != Some(floor.site.name.as_str()) {
            return Err(invalid(format!(
                "master {} does not use selected site",
                m.name
            )));
        }
        let pins = m
            .pins
            .iter()
            .map(|p| -> Result<MasterPin> {
                let direction = p.direction.as_deref().map(direction).transpose()?;
                let use_ = match p.use_.as_deref().unwrap_or("SIGNAL") {
                    "SIGNAL" => PinUse::Signal,
                    "CLOCK" => PinUse::Clock,
                    "POWER" => PinUse::Power,
                    "GROUND" => PinUse::Ground,
                    other => return Err(invalid(format!("unsupported LEF pin USE {other}"))),
                };
                Ok(MasterPin {
                    name: p.name.clone(),
                    direction,
                    use_,
                    shape: p.shape.clone(),
                    ports: p
                        .ports
                        .iter()
                        .map(|p| p.geometries.iter().map(geometry).collect())
                        .collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        masters.push(Master {
            name: m.name.clone(),
            class: m.class.clone(),
            symmetry: m.symmetry.clone(),
            site: floor.site.name.clone(),
            size: m.size,
            pins,
            obstructions: m.obstructions.iter().map(geometry).collect(),
        });
    }
    let manufacturing_grid = match (tech.manufacturing_grid, cells.manufacturing_grid) {
        (Some(a), Some(b)) if a != b => return Err(invalid("LEF manufacturing grid mismatch")),
        (a, b) => a.or(b),
    };
    let mut via_names = BTreeMap::new();
    for via in tech.vias.iter().chain(&cells.vias) {
        if let Some(old) = via_names.insert(via.name.clone(), via.clone())
            && old != *via
        {
            return Err(invalid(format!("conflicting fixed VIA {}", via.name)));
        }
    }
    let vias = via_names.into_values().collect();
    Ok((
        Technology {
            dbu_per_micron: floor.units_per_micron,
            manufacturing_grid,
            layers: layer_map.into_values().collect(),
            sites: site_map.into_values().collect(),
            vias,
        },
        masters,
    ))
}
fn direction(value: &str) -> Result<Direction> {
    match value {
        "INPUT" => Ok(Direction::Input),
        "OUTPUT" => Ok(Direction::Output),
        "INOUT" => Ok(Direction::Inout),
        _ => Err(invalid(format!("unsupported LEF pin direction {value}"))),
    }
}
fn geometry(value: &lef::LefGeometry) -> Geometry {
    match value {
        lef::LefGeometry::Rect { layer, rect } => Geometry::Rect {
            layer: layer.clone(),
            rect: *rect,
        },
        lef::LefGeometry::Polygon { layer, points } => Geometry::Polygon {
            layer: layer.clone(),
            points: points.clone(),
        },
    }
}
fn vdirection(value: verilog::Direction) -> Direction {
    match value {
        verilog::Direction::Input => Direction::Input,
        verilog::Direction::Output => Direction::Output,
        verilog::Direction::Inout => Direction::Inout,
    }
}

struct Signals {
    declared: HashMap<String, (Option<verilog::Range>, Vec<usize>)>,
    names: Vec<String>,
    parent: Vec<usize>,
    size: Vec<usize>,
    canonical: Vec<usize>,
}
impl Signals {
    fn add(&mut self, name: &str, range: Option<verilog::Range>, max: usize) -> Result<()> {
        if let Some((old, _)) = self.declared.get(name) {
            return if *old == range {
                Ok(())
            } else {
                Err(invalid("conflicting signal declarations"))
            };
        }
        let width = range.map_or(Ok(1), |r| range_width(r, max))?;
        if self.names.len().checked_add(width).is_none_or(|n| n > max) {
            return Err(budget("expanded signal bit budget exceeded"));
        }
        let mut bits = Vec::with_capacity(width);
        for i in 0..width {
            let index = self.names.len();
            let bit_name = match range {
                None => name.to_owned(),
                Some(r) => format!(
                    "{name}[{}]",
                    i128::from(r.msb)
                        + if r.msb <= r.lsb {
                            i as i128
                        } else {
                            -(i as i128)
                        }
                ),
            };
            self.names.push(bit_name);
            self.parent.push(index);
            self.size.push(1);
            self.canonical.push(index);
            bits.push(index);
        }
        self.declared.insert(name.into(), (range, bits));
        Ok(())
    }
    fn root(&mut self, mut i: usize) -> usize {
        // Union-by-size forest with path halving, no recursive walk.
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }
    fn join(&mut self, a: usize, b: usize) {
        let mut a = self.root(a);
        let mut b = self.root(b);
        if a == b {
            return;
        }
        if self.size[a] < self.size[b] || (self.size[a] == self.size[b] && a > b) {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        self.size[a] += self.size[b];
        self.canonical[a] = self.canonical[a].min(self.canonical[b]);
    }
    fn expand(&self, expr: &verilog::Expr, max: usize) -> Result<Vec<usize>> {
        let mut result = Vec::new();
        let mut stack = vec![(expr, 0usize)];
        let mut expansions = 0usize;
        while let Some((expr, depth)) = stack.pop() {
            expansions += 1;
            if expansions > max || depth > 128 {
                return Err(budget("expression expansion/depth limit exceeded"));
            }
            match expr {
                verilog::Expr::Ident(name) => {
                    let bits = &self
                        .declared
                        .get(name)
                        .ok_or_else(|| invalid(format!("undeclared signal {name}")))?
                        .1;
                    if bits.len() > max.saturating_sub(result.len()) {
                        return Err(budget("bit expansion limit exceeded"));
                    }
                    result.extend_from_slice(bits);
                }
                verilog::Expr::BitSelect { name, index } => result.push(self.bit(name, *index)?),
                verilog::Expr::PartSelect { name, range } => {
                    let width = range_width(*range, max.saturating_sub(result.len()))?;
                    for i in 0..width {
                        let index = (i128::from(range.msb)
                            + if range.msb <= range.lsb {
                                i as i128
                            } else {
                                -(i as i128)
                            }) as i64;
                        result.push(self.bit(name, index)?);
                    }
                }
                verilog::Expr::Concat(parts) => {
                    if parts.len() > max.saturating_sub(stack.len()) {
                        return Err(budget("concat work limit exceeded"));
                    }
                    for part in parts.iter().rev() {
                        stack.push((part, depth + 1));
                    }
                }
                verilog::Expr::Unconnected => {}
                verilog::Expr::Constant(_) => {
                    return Err(invalid(
                        "physical constants require explicit mapped tie cells; constant connections/aliases are unsupported",
                    ));
                }
            }
            if result.len() > max {
                return Err(budget("bit expansion limit exceeded"));
            }
        }
        Ok(result)
    }
    fn bit(&self, name: &str, index: i64) -> Result<usize> {
        let (range, bits) = self
            .declared
            .get(name)
            .ok_or_else(|| invalid("undeclared selected signal"))?;
        let range = range.ok_or_else(|| invalid("bit select on scalar signal"))?;
        if index < range.msb.min(range.lsb) || index > range.msb.max(range.lsb) {
            return Err(invalid("bit select outside declared range"));
        }
        let offset = (i128::from(index) - i128::from(range.msb)).unsigned_abs() as usize;
        Ok(bits[offset])
    }
}
fn range_width(range: verilog::Range, max: usize) -> Result<usize> {
    let width = (i128::from(range.msb) - i128::from(range.lsb)).unsigned_abs() + 1;
    if width > max as u128 {
        return Err(budget("vector bit-width limit exceeded"));
    }
    Ok(width as usize)
}

pub(super) fn link(
    netlist: &verilog::Netlist,
    masters: &[Master],
    instance_masters: &[u32],
    limits: &ConnectivityLimits,
) -> Result<(Vec<Port>, Vec<Net>, Vec<Connection>)> {
    let mut signals = Signals {
        declared: HashMap::new(),
        names: Vec::new(),
        parent: Vec::new(),
        size: Vec::new(),
        canonical: Vec::new(),
    };
    for port in &netlist.ports {
        signals.add(&port.name, port.range, limits.max_aliases)?;
    }
    for wire in &netlist.wires {
        signals.add(&wire.name, wire.range, limits.max_aliases)?;
    }
    let mut unique = HashSet::new();
    for name in &signals.names {
        if !unique.insert(name.as_str()) {
            return Err(invalid(
                "scalar escaped identifier collides with a vector bit name in DEF; rename before physical import",
            ));
        }
    }
    drop(unique);
    let mut alias_work = 0usize;
    for assign in &netlist.assigns {
        let a = signals.expand(&assign.lhs, limits.max_aliases)?;
        let b = signals.expand(&assign.rhs, limits.max_aliases)?;
        if a.len() != b.len() {
            return Err(invalid(
                "assign width mismatch; implicit extension/truncation is unsupported",
            ));
        }
        alias_work = alias_work
            .checked_add(a.len())
            .ok_or_else(|| budget("alias work overflow"))?;
        if alias_work > limits.max_aliases {
            return Err(budget("alias union work limit exceeded"));
        }
        for (a, b) in a.into_iter().zip(b) {
            signals.join(a, b);
        }
    }
    let mut net_of = HashMap::new();
    let mut nets: Vec<Net> = Vec::new();
    let mut signal_nets = Vec::with_capacity(signals.names.len());
    for i in 0..signals.names.len() {
        let root = signals.root(i);
        let ni = if let Some(&n) = net_of.get(&root) {
            n
        } else {
            if nets.len() >= limits.max_nets {
                return Err(budget("max_nets exceeded"));
            }
            let n = nets.len() as u32;
            net_of.insert(root, n);
            nets.push(Net {
                name: signals.names[signals.canonical[root]].clone(),
                aliases: Vec::new(),
                use_: PinUse::Signal,
            });
            n
        };
        nets[ni as usize].aliases.push(signals.names[i].clone());
        signal_nets.push(ni);
    }
    let mut ports = Vec::new();
    let mut pins = Vec::new();
    for (pi, port) in netlist.ports.iter().enumerate() {
        let bits = &signals.declared[&port.name].1;
        if bits.len() > limits.max_pins.saturating_sub(pins.len()) {
            return Err(budget("max_pins exceeded"));
        }
        ports.push(Port {
            name: port.name.clone(),
            direction: vdirection(port.direction),
            bits: bits.iter().map(|&i| signals.names[i].clone()).collect(),
            use_: PinUse::Signal,
        });
        for (bit, &i) in bits.iter().enumerate() {
            pins.push(Connection {
                owner: PinOwner::Port {
                    port: pi as u32,
                    bit: bit as u32,
                },
                net: Some(signal_nets[i]),
            });
        }
    }
    let mut connection_work = 0usize;
    for (ii, instance) in netlist.instances.iter().enumerate() {
        let master = &masters[instance_masters[ii] as usize];
        if master.pins.len() > limits.max_pins.saturating_sub(pins.len()) {
            return Err(budget("max_pins exceeded"));
        }
        let mut named = HashMap::new();
        for connection in &instance.connections {
            if named
                .insert(connection.pin.as_str(), &connection.signal)
                .is_some()
            {
                return Err(invalid("duplicate cell pin connection"));
            }
        }
        for (terminal, spec) in master.pins.iter().enumerate() {
            let expr = named.remove(spec.name.as_str());
            let net = if spec.use_.is_supply() {
                if expr.is_some() {
                    return Err(invalid(
                        "explicit supply pin wiring requires a supply-network importer",
                    ));
                }
                None
            } else if expr.is_none() && spec.direction == Some(Direction::Output) {
                // An omitted output pin is unconnected (e.g. the unused output
                // of a two-output tie cell); omitted inputs stay an error.
                None
            } else {
                let expr = expr.ok_or_else(|| {
                    invalid(format!(
                        "missing connection {}.{}",
                        instance.name, spec.name
                    ))
                })?;
                let bits = signals.expand(expr, limits.max_pins)?;
                connection_work = connection_work
                    .checked_add(bits.len())
                    .ok_or_else(|| budget("connection work overflow"))?;
                if connection_work > limits.max_pins {
                    return Err(budget("connection expansion work limit exceeded"));
                }
                match bits.as_slice() {
                    [] => None,
                    [i] => Some(signal_nets[*i]),
                    _ => {
                        return Err(invalid(format!(
                            "{}.{}, LEF pin is scalar but expression is a vector",
                            instance.name, spec.name
                        )));
                    }
                }
            };
            pins.push(Connection {
                owner: PinOwner::Instance {
                    instance: ii as u32,
                    terminal: terminal as u32,
                },
                net,
            });
        }
        if let Some(pin) = named.keys().next() {
            return Err(invalid(format!("unknown LEF pin {}.{pin}", instance.name)));
        }
    }
    Ok((ports, nets, pins))
}
