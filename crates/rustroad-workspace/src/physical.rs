//! Bounded physical preparation, with explicit PDK role bindings.
//! Resource/coverage contract: docs/power-io-budget.md.
use crate::pdk::{PhysicalBundle, Technology};
use crate::{Error, Result};
use rustroad_core::connectivity::{
    Connection, ConnectivityLimits, Direction, Net, PinOwner, PinUse, Port,
};
use rustroad_core::io::{IoConstraints, IoLimits, IoPin, place_boundary_io};
use rustroad_core::power::{
    PowerCell, PowerRow, RailConfig, RailLimits, RailPolarity, RailReport, validate_rail_continuity,
};
use rustroad_core::supply::{LayerSpec, SupplyGrid, SupplyLimits, ViaSpec, build_supply_grid};
use rustroad_core::{Cell, Design, DesignDb, Limits, Placed, Rect, def_name};
use rustroad_formats::{lef, verilog};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

fn default_added() -> usize {
    250_000
}
fn default_fanout() -> usize {
    8
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TieCell {
    pub cell: String,
    pub pin: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalConfig {
    pub power_net: String,
    pub ground_net: String,
    pub power_pin: String,
    pub ground_pin: String,
    pub body_power_pins: Vec<String>,
    pub body_ground_pins: Vec<String>,
    pub rail_layer: String,
    pub strap_layer: String,
    pub via: String,
    pub io_layer: String,
    pub tie_high: TieCell,
    pub tie_low: TieCell,
    pub filler_cells: Vec<String>,
    pub alternating_rows: bool,
    #[serde(default)]
    pub io_width: Option<i64>,
    #[serde(default)]
    pub io_spacing: Option<i64>,
    #[serde(default)]
    pub strap_width: Option<i64>,
    #[serde(default)]
    pub strap_spacing: Option<i64>,
    #[serde(default = "default_added")]
    pub max_added_cells: usize,
    #[serde(default = "default_fanout")]
    pub max_tie_fanout: usize,
}
impl PhysicalConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_added_cells > 250_000
            || self.max_tie_fanout == 0
            || self.max_tie_fanout > 10_000
            || self.filler_cells.is_empty()
            || self.filler_cells.len() > 64
            || self.body_power_pins.len() > 64
            || self.body_ground_pins.len() > 64
        {
            return Err(Error::Budget("physical configuration limit".into()));
        }
        for name in [
            &self.power_net,
            &self.ground_net,
            &self.power_pin,
            &self.ground_pin,
            &self.rail_layer,
            &self.strap_layer,
            &self.via,
            &self.io_layer,
            &self.tie_high.cell,
            &self.tie_high.pin,
            &self.tie_low.cell,
            &self.tie_low.pin,
        ]
        .into_iter()
        .chain(self.filler_cells.iter())
        .chain(self.body_power_pins.iter())
        .chain(self.body_ground_pins.iter())
        {
            if !rustroad_core::valid_object_name(name) {
                return Err(Error::Invalid("invalid physical configuration name".into()));
            }
        }
        if self.power_net == self.ground_net
            || self.power_pin == self.ground_pin
            || self.rail_layer == self.strap_layer
        {
            return Err(Error::Invalid(
                "power/ground or routing-layer roles must be distinct".into(),
            ));
        }
        if self.io_layer == self.rail_layer || self.io_layer == self.strap_layer {
            return Err(Error::Invalid(
                "I/O requires a layer separate from supply routing".into(),
            ));
        }
        if self.io_width.is_some_and(|v| v <= 0)
            || self.strap_width.is_some_and(|v| v <= 0)
            || self.io_spacing.is_some_and(|v| v < 0)
            || self.strap_spacing.is_some_and(|v| v < 0)
        {
            return Err(Error::Invalid("invalid physical width/spacing".into()));
        }
        let mut power = HashSet::new();
        power.insert(self.power_pin.as_str());
        for p in &self.body_power_pins {
            if !power.insert(p.as_str()) {
                return Err(Error::Invalid("duplicate power pin role".into()));
            }
        }
        let mut ground = HashSet::new();
        ground.insert(self.ground_pin.as_str());
        for p in &self.body_ground_pins {
            if !ground.insert(p.as_str()) {
                return Err(Error::Invalid("duplicate ground pin role".into()));
            }
        }
        if power.iter().any(|p| ground.contains(p)) {
            return Err(Error::Invalid("ambiguous power/ground pin role".into()));
        }
        if self.tie_high.cell == self.tie_low.cell && self.tie_high.pin == self.tie_low.pin {
            return Err(Error::Invalid(
                "high/low tie outputs must be distinct".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalState {
    pub io_pins: Vec<IoPin>,
    pub rails: RailReport,
    pub supply: SupplyGrid,
    pub fillers_added: usize,
}

pub(super) fn attach_supplies(
    masters: &[rustroad_core::connectivity::Master],
    instance_masters: &[u32],
    ports: &mut Vec<Port>,
    nets: &mut Vec<Net>,
    pins: &mut Vec<Connection>,
    cfg: &PhysicalConfig,
    limits: &ConnectivityLimits,
) -> Result<()> {
    cfg.validate()?;
    if nets.len() + 2 > limits.max_nets || pins.len() + 2 > limits.max_pins {
        return Err(Error::Budget("supply net/pin capacity".into()));
    }
    for name in [&cfg.power_net, &cfg.ground_net] {
        if nets.iter().any(|n| n.aliases.contains(name)) || ports.iter().any(|p| &p.name == name) {
            return Err(Error::Invalid(
                "supply name conflicts with a logical signal".into(),
            ));
        }
    }
    let first = nets.len() as u32;
    for (name, use_) in [
        (&cfg.power_net, PinUse::Power),
        (&cfg.ground_net, PinUse::Ground),
    ] {
        nets.push(Net {
            name: name.clone(),
            aliases: vec![name.clone()],
            use_,
        });
        let port = ports.len() as u32;
        ports.push(Port {
            name: name.clone(),
            direction: Direction::Inout,
            bits: vec![name.clone()],
            use_,
        });
        pins.push(Connection {
            owner: PinOwner::Port { port, bit: 0 },
            net: Some(nets.len() as u32 - 1),
        });
    }
    for pin in pins {
        if let PinOwner::Instance { instance, terminal } = pin.owner {
            let spec =
                &masters[instance_masters[instance as usize] as usize].pins[terminal as usize];
            if spec.use_.is_supply() {
                let supply = if spec.name == cfg.power_pin
                    || cfg.body_power_pins.contains(&spec.name)
                {
                    (PinUse::Power, first)
                } else if spec.name == cfg.ground_pin || cfg.body_ground_pins.contains(&spec.name) {
                    (PinUse::Ground, first + 1)
                } else {
                    return Err(Error::Invalid(format!("unmapped supply pin {}", spec.name)));
                };
                if spec.use_ != supply.0 {
                    return Err(Error::Invalid("supply role disagrees with LEF USE".into()));
                }
                pin.net = Some(supply.1);
            }
        }
    }
    Ok(())
}

pub(super) fn validate_ties(bundle: &PhysicalBundle) -> Result<()> {
    let (Some(cfg), Some(report)) = (&bundle.config, &bundle.ties) else {
        return Ok(());
    };
    if report.bindings.len() != report.new_cells || report.bindings.len() > cfg.max_added_cells {
        return Err(Error::Invalid("tie report count mismatch".into()));
    }
    if report.constant_bits != report.new_cells
        || report.high_cells.checked_add(report.low_cells) != Some(report.new_cells)
        || report.bindings.iter().filter(|b| b.value).count() != report.high_cells
    {
        return Err(Error::Invalid("tie report polarity/count mismatch".into()));
    }
    let model = &bundle.connectivity;
    let instances: HashMap<_, _> = bundle
        .netlist
        .instances
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();
    let mut net_sinks = vec![0usize; model.nets.len()];
    for pin in &model.pins {
        if let Some(net) = pin.net {
            let sink = match pin.owner {
                PinOwner::Instance { instance, terminal } => {
                    let p = &model.masters[model.instance_masters[instance as usize] as usize].pins
                        [terminal as usize];
                    !p.use_.is_supply() && p.direction == Some(Direction::Input)
                }
                PinOwner::Port { port, .. } => {
                    model.ports[port as usize].direction == Direction::Output
                }
            };
            if sink {
                net_sinks[net as usize] += 1;
            }
        }
    }
    let pin_nets: HashMap<_, _> = model.pins.iter().map(|p| (p.owner, p.net)).collect();
    let terminal_names: Vec<HashMap<_, _>> = model
        .masters
        .iter()
        .map(|m| {
            m.pins
                .iter()
                .enumerate()
                .map(|(i, p)| (p.name.as_str(), i))
                .collect()
        })
        .collect();
    let alias_nets: HashMap<_, _> = model
        .nets
        .iter()
        .enumerate()
        .flat_map(|(i, n)| n.aliases.iter().map(move |a| (a.as_str(), i as u32)))
        .collect();
    let mut binding_names = HashSet::new();
    for binding in &report.bindings {
        let &i = instances
            .get(binding.instance.as_str())
            .ok_or_else(|| Error::Invalid("missing tie instance".into()))?;
        if !binding_names.insert(binding.instance.as_str()) {
            return Err(Error::Invalid("duplicate tie report instance".into()));
        }
        let spec = if binding.value {
            &cfg.tie_high
        } else {
            &cfg.tie_low
        };
        if bundle.netlist.instances[i].cell != spec.cell || binding.pin != spec.pin {
            return Err(Error::Invalid("tie report binding mismatch".into()));
        }
        let terminal = *terminal_names[model.instance_masters[i] as usize]
            .get(binding.pin.as_str())
            .ok_or_else(|| Error::Invalid("tie output absent".into()))?;
        let net = pin_nets
            .get(&PinOwner::Instance {
                instance: i as u32,
                terminal: terminal as u32,
            })
            .copied()
            .flatten()
            .ok_or_else(|| Error::Invalid("tie output unconnected".into()))?;
        if alias_nets.get(binding.net.as_str()) != Some(&net)
            || net_sinks[net as usize] > cfg.max_tie_fanout
        {
            return Err(Error::Budget("tie net identity/fanout limit".into()));
        }
    }
    Ok(())
}

pub(super) fn clear_fillers(design: &mut Design, bundle: &mut PhysicalBundle) -> Result<()> {
    if bundle.generated_fillers.is_empty() {
        bundle.state = None;
        return Ok(());
    }
    let names: HashSet<_> = bundle
        .generated_fillers
        .iter()
        .map(String::as_str)
        .collect();
    design.cells.retain(|c| !names.contains(c.name.as_str()));
    bundle
        .netlist
        .instances
        .retain(|c| !names.contains(c.name.as_str()));
    bundle.generated_fillers.clear();
    bundle.state = None;
    relink(bundle)?;
    Ok(())
}
pub(super) fn relink(bundle: &mut PhysicalBundle) -> Result<()> {
    let lookup: HashMap<_, _> = bundle
        .connectivity
        .masters
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i as u32))
        .collect();
    let im = bundle
        .netlist
        .instances
        .iter()
        .map(|c| {
            lookup
                .get(c.cell.as_str())
                .copied()
                .ok_or_else(|| Error::Invalid("missing prepared master".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let (mut ports, mut nets, mut pins) = crate::pdk::link(
        &bundle.netlist,
        &bundle.connectivity.masters,
        &im,
        &ConnectivityLimits::default(),
    )?;
    if let Some(cfg) = &bundle.config {
        attach_supplies(
            &bundle.connectivity.masters,
            &im,
            &mut ports,
            &mut nets,
            &mut pins,
            cfg,
            &ConnectivityLimits::default(),
        )?;
    }
    bundle.connectivity.instance_masters = im;
    bundle.connectivity.ports = ports;
    bundle.connectivity.nets = nets;
    bundle.connectivity.pins = pins;
    validate_ties(bundle)
}

pub(super) fn prepare(
    design: &mut Design,
    bundle: &mut PhysicalBundle,
    mut placed: Vec<Placed>,
    limits: &Limits,
) -> Result<(Vec<Placed>, usize)> {
    let cfg = bundle
        .config
        .clone()
        .ok_or_else(|| Error::Invalid("physical config absent".into()))?;
    let by_name: HashMap<_, _> = bundle
        .connectivity
        .masters
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.clone(), i))
        .collect();
    let mut fillers = Vec::new();
    for name in &cfg.filler_cells {
        let m = &bundle.connectivity.masters[*by_name
            .get(name)
            .ok_or_else(|| Error::Invalid("filler master absent".into()))?];
        if m.pins.iter().any(|p| !p.use_.is_supply()) {
            return Err(Error::Invalid("filler has logical signal pins".into()));
        }
        fillers.push((m.size[0], m.name.clone()));
    }
    fillers.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    if fillers.last().map(|f| f.0) != Some(design.site.width) {
        return Err(Error::Invalid("filler list needs a one-site cell".into()));
    }
    let mut occupied = HashMap::<i64, Vec<(i64, i64)>>::new();
    for cell in &placed {
        occupied.entry(cell.y).or_default().push((
            cell.x,
            cell.x
                .checked_add(cell.width)
                .ok_or_else(|| Error::Invalid("cell extent overflow".into()))?,
        ));
    }
    let mut names: HashSet<String> = design.cells.iter().map(|c| c.name.clone()).collect();
    let mut new_positions = Vec::new();
    let mut sequence = 0usize;
    for row in &design.rows {
        let mut intervals = occupied.remove(&row.y).unwrap_or_default();
        intervals.sort_unstable();
        let mut cursor = row.x_start;
        for (a, b) in intervals
            .into_iter()
            .chain(std::iter::once((row.x_end, row.x_end)))
        {
            while cursor < a {
                let remaining = i128::from(a) - i128::from(cursor);
                let (width, master) = fillers
                    .iter()
                    .find(|f| i128::from(f.0) <= remaining)
                    .ok_or_else(|| Error::Invalid("unfillable row gap".into()))?;
                if design.cells.len() >= limits.max_cells
                    || bundle.generated_fillers.len()
                        + bundle.ties.as_ref().map_or(0, |t| t.new_cells)
                        >= cfg.max_added_cells
                {
                    return Err(Error::Budget("tie/filler instance limit".into()));
                }
                let name = loop {
                    if sequence > limits.max_cells.saturating_add(names.len()) {
                        return Err(Error::Budget("filler name search".into()));
                    }
                    let n = format!("rr_fill_{sequence}");
                    sequence += 1;
                    if names.insert(n.clone()) {
                        break n;
                    }
                };
                design.cells.push(Cell {
                    name: name.clone(),
                    master: master.clone(),
                    width: *width,
                    height: design.site.height,
                });
                bundle.netlist.instances.push(verilog::Instance {
                    name: name.clone(),
                    cell: master.clone(),
                    connections: Vec::new(),
                });
                bundle.generated_fillers.push(name);
                new_positions.push((cursor, row.y));
                cursor = cursor
                    .checked_add(*width)
                    .ok_or_else(|| Error::Invalid("filler coordinate overflow".into()))?;
            }
            if cursor > a {
                return Err(Error::Invalid("overlap before filling".into()));
            }
            cursor = b;
        }
    }
    let original_len = placed.len();
    let ids = DesignDb::new(design, limits)?.unplaced_records();
    for (i, p) in placed.iter_mut().enumerate() {
        p.id = ids[i].id;
    }
    for (i, (x, y)) in new_positions.into_iter().enumerate() {
        let mut p = ids[original_len + i];
        p.x = x;
        p.y = y;
        placed.push(p);
    }
    relink(bundle)?;
    bundle
        .connectivity
        .validate(design, &ConnectivityLimits::default())?;
    let state = layout(design, bundle, &placed)?;
    let expansions = bundle.generated_fillers.len() + design.rows.len();
    bundle.state = Some(state);
    bundle.diagnostics.retain(|d| {
        !matches!(
            d.code.as_str(),
            "POWER_GRID_UNCHECKED"
                | "IO_PLACEMENT_UNCHECKED"
                | "POWER_GRID_PENDING"
                | "IO_PLACEMENT_PENDING"
                | "PHYSICAL_SIGNOFF_UNCHECKED"
        )
    });
    bundle.diagnostics.push(crate::pdk::Diagnostic{code:"PHYSICAL_SIGNOFF_UNCHECKED".into(),source:"physical_preparation".into(),occurrences:1,message:"Primary metal rails/spines/vias and I/O geometry are generated and locally checked; well/tap continuity, DRC, IR/EM and extracted timing are not verified by this pass".into()});
    Ok((placed, expansions))
}

fn layer<'a>(technology: &'a Technology, name: &str) -> Result<&'a lef::LefLayer> {
    let layer = technology
        .layers
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| Error::Invalid(format!("unknown layer {name}")))?;
    if layer.kind.as_deref() != Some("ROUTING") {
        return Err(Error::Invalid(
            "physical placement layer must be ROUTING".into(),
        ));
    }
    Ok(layer)
}
pub(super) fn layout(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
) -> Result<PhysicalState> {
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| Error::Invalid("physical config absent".into()))?;
    rustroad_core::validate_placement(design, placed, &Limits::default())?;
    let mut intervals: Vec<_> = placed
        .iter()
        .map(|p| (p.y, p.x, p.x.checked_add(p.width).unwrap()))
        .collect();
    intervals.sort_unstable();
    let mut cursor = 0usize;
    for row in &design.rows {
        let mut x = row.x_start;
        while cursor < intervals.len() && intervals[cursor].0 == row.y {
            if intervals[cursor].1 != x {
                return Err(Error::Invalid("power row has a placement gap".into()));
            }
            x = intervals[cursor].2;
            cursor += 1;
        }
        if x != row.x_end {
            return Err(Error::Invalid("power row is not filled end to end".into()));
        }
    }
    let cells = placed
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Ok(PowerCell {
                instance: i as u32,
                master: bundle.connectivity.instance_masters[i],
                x: p.x,
                y: p.y,
                orientation: rustroad_core::placed_orientation(design, p)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    for cell in &cells {
        if cell.orientation == rustroad_core::Orientation::FS
            && !bundle.connectivity.masters[cell.master as usize]
                .symmetry
                .iter()
                .any(|s| s == "X")
        {
            return Err(Error::Invalid(
                "FS placement requires LEF SYMMETRY X".into(),
            ));
        }
    }
    let rows = design
        .rows
        .iter()
        .map(|r| PowerRow {
            x_start: r.x_start,
            x_end: r.x_end,
            y: r.y,
            height: design.site.height,
            orientation: r.orientation,
        })
        .collect::<Vec<_>>();
    let rails = validate_rail_continuity(
        &bundle.connectivity.masters,
        &cells,
        &rows,
        &RailConfig {
            power_pin: cfg.power_pin.clone(),
            ground_pin: cfg.ground_pin.clone(),
            rail_layer: cfg.rail_layer.clone(),
        },
        &RailLimits::default(),
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    if rails.status != "pass" {
        return Err(Error::Invalid(format!(
            "primary rail continuity failed: {:?}",
            rails.diagnostics
        )));
    }
    let grid = bundle.technology.manufacturing_grid.ok_or_else(|| {
        Error::Invalid("manufacturing grid required for physical preparation".into())
    })?;
    let io_layer = layer(&bundle.technology, &cfg.io_layer)?;
    let io_constraints = IoConstraints {
        die: design.die_area,
        core: design.core_area,
        grid,
        layer: cfg.io_layer.clone(),
        min_width: cfg
            .io_width
            .or(io_layer.width)
            .ok_or_else(|| Error::Invalid("I/O minimum width missing".into()))?
            .max(io_layer.width.unwrap_or(0)),
        min_spacing: cfg
            .io_spacing
            .or(io_layer.spacing)
            .ok_or_else(|| Error::Invalid("I/O minimum spacing missing".into()))?
            .max(io_layer.spacing.unwrap_or(0)),
    };
    let names = bundle
        .connectivity
        .ports
        .iter()
        .filter(|p| !p.use_.is_supply())
        .flat_map(|p| p.bits.iter().cloned())
        .collect::<Vec<_>>();
    let io_pins = place_boundary_io(&names, &io_constraints, &IoLimits::default())
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let rail_layer = layer(&bundle.technology, &cfg.rail_layer)?;
    let strap_layer = layer(&bundle.technology, &cfg.strap_layer)?;
    let via = bundle
        .technology
        .vias
        .iter()
        .find(|v| v.name == cfg.via)
        .ok_or_else(|| Error::Invalid("fixed supply via absent".into()))?;
    let rectangles = |name: &str| -> Result<Vec<Rect>> {
        via.geometries
            .iter()
            .filter_map(|g| match g {
                lef::LefGeometry::Rect { layer, rect } if layer == name => Some(Ok(Rect {
                    lx: rect[0],
                    ly: rect[1],
                    ux: rect[2],
                    uy: rect[3],
                })),
                lef::LefGeometry::Polygon { layer, .. } if layer == name => {
                    Some(Err(Error::Invalid("polygon supply via unsupported".into())))
                }
                _ => None,
            })
            .collect()
    };
    let supply = build_supply_grid(
        design.die_area,
        design.core_area,
        grid,
        &rails,
        &cfg.rail_layer,
        rail_layer
            .width
            .ok_or_else(|| Error::Invalid("rail minimum width absent".into()))?,
        &LayerSpec {
            name: cfg.strap_layer.clone(),
            width: cfg
                .strap_width
                .or(strap_layer.width)
                .ok_or_else(|| Error::Invalid("strap width absent".into()))?
                .max(strap_layer.width.unwrap_or(0)),
            spacing: cfg
                .strap_spacing
                .or(strap_layer.spacing)
                .ok_or_else(|| Error::Invalid("strap spacing absent".into()))?
                .max(strap_layer.spacing.unwrap_or(0)),
        },
        &ViaSpec {
            name: cfg.via.clone(),
            lower: rectangles(&cfg.rail_layer)?,
            upper: rectangles(&cfg.strap_layer)?,
        },
        &SupplyLimits::default(),
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(PhysicalState {
        io_pins,
        rails,
        supply,
        fillers_added: bundle.generated_fillers.len(),
    })
}

pub(super) fn validate_state(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
) -> Result<()> {
    let state = bundle
        .state
        .as_ref()
        .ok_or_else(|| Error::Invalid("prepared placement state absent".into()))?;
    let expected = layout(design, bundle, placed)?;
    if crate::serialize_bounded(state)? != crate::serialize_bounded(&expected)? {
        return Err(Error::Invalid(
            "prepared layout differs from validated geometry".into(),
        ));
    }
    Ok(())
}

pub(super) fn export_verilog(
    bundle: &PhysicalBundle,
    include_power: bool,
    output: &mut impl Write,
) -> Result<()> {
    let mut netlist = bundle.netlist.clone();
    if include_power {
        if let Some(cfg) = &bundle.config {
            for name in [&cfg.power_net, &cfg.ground_net] {
                netlist.ports.push(verilog::Port {
                    name: name.clone(),
                    direction: verilog::Direction::Inout,
                    range: None,
                });
            }
            for (i, inst) in netlist.instances.iter_mut().enumerate() {
                let m =
                    &bundle.connectivity.masters[bundle.connectivity.instance_masters[i] as usize];
                for pin in &m.pins {
                    if pin.use_.is_supply() {
                        inst.connections.push(verilog::Connection {
                            pin: pin.name.clone(),
                            signal: verilog::Expr::Ident(if pin.use_ == PinUse::Power {
                                cfg.power_net.clone()
                            } else {
                                cfg.ground_net.clone()
                            }),
                        });
                    }
                }
            }
        }
    } else {
        let names: HashSet<_> = bundle.generated_fillers.iter().collect();
        netlist.instances.retain(|i| !names.contains(&i.name));
    }
    verilog::write_verilog(&netlist, output).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(())
}

pub(super) fn export_def(
    design: &Design,
    placed: &[Placed],
    bundle: &PhysicalBundle,
    output: &mut impl Write,
) -> Result<()> {
    validate_state(design, bundle, placed)?;
    let cfg = bundle.config.as_ref().unwrap();
    let state = bundle.state.as_ref().unwrap();
    let model = &bundle.connectivity;
    rustroad_core::write_def_components(design, placed, output)?;
    let aliases: HashMap<_, _> = model
        .nets
        .iter()
        .enumerate()
        .flat_map(|(i, n)| n.aliases.iter().map(move |a| (a.as_str(), i)))
        .collect();
    let geometry: HashMap<_, _> = state
        .io_pins
        .iter()
        .map(|p| (p.name.as_str(), (p.layer.as_str(), p.rect)))
        .collect();
    writeln!(
        output,
        "PINS {} ;",
        model.ports.iter().map(|p| p.bits.len()).sum::<usize>()
    )?;
    for p in &model.ports {
        for name in &p.bits {
            let ni = *aliases
                .get(name.as_str())
                .ok_or_else(|| Error::Invalid("port net absent".into()))?;
            let (layer, rect) = if p.use_.is_supply() {
                let polarity = if p.use_ == PinUse::Power {
                    RailPolarity::Power
                } else {
                    RailPolarity::Ground
                };
                let s = state
                    .supply
                    .ports
                    .iter()
                    .find(|s| s.polarity == polarity)
                    .unwrap();
                (s.layer.as_str(), s.rect)
            } else {
                *geometry
                    .get(name.as_str())
                    .ok_or_else(|| Error::Invalid("I/O geometry absent".into()))?
            };
            writeln!(
                output,
                "- {} + NET {} + DIRECTION {} + USE {} + PORT + LAYER {} ( 0 0 ) ( {} {} ) + FIXED ( {} {} ) N ;",
                def_name(name),
                def_name(&model.nets[ni].name),
                p.direction.def(),
                p.use_.def(),
                def_name(layer),
                rect.ux - rect.lx,
                rect.uy - rect.ly,
                rect.lx,
                rect.ly
            )?;
        }
    }
    writeln!(output, "END PINS")?;
    let csr = model.lower_csr(design, &ConnectivityLimits::default())?;
    for special in [false, true] {
        let count = model
            .nets
            .iter()
            .filter(|n| n.use_.is_supply() == special)
            .count();
        let section = if special { "SPECIALNETS" } else { "NETS" };
        writeln!(output, "{section} {count} ;")?;
        for (ni, net) in model.nets.iter().enumerate() {
            if net.use_.is_supply() != special {
                continue;
            }
            write!(output, "- {}", def_name(&net.name))?;
            for &pi in &csr.pin_indices[csr.offsets[ni] as usize..csr.offsets[ni + 1] as usize] {
                match model.pins[pi as usize].owner {
                    PinOwner::Instance { instance, terminal } => write!(
                        output,
                        " ( {} {} )",
                        def_name(&design.cells[instance as usize].name),
                        def_name(
                            &model.masters[model.instance_masters[instance as usize] as usize].pins
                                [terminal as usize]
                                .name
                        )
                    )?,
                    PinOwner::Port { port, bit } => write!(
                        output,
                        " ( PIN {} )",
                        def_name(&model.ports[port as usize].bits[bit as usize])
                    )?,
                }
            }
            write!(output, " + USE {}", net.use_.def())?;
            if special {
                let polarity = if net.use_ == PinUse::Power {
                    RailPolarity::Power
                } else {
                    RailPolarity::Ground
                };
                for w in state.supply.wires.iter().filter(|w| w.polarity == polarity) {
                    write!(
                        output,
                        "\n  + ROUTED {} {} ( {} {} {} ) ( {} {} {} )",
                        def_name(&w.layer),
                        w.width,
                        w.start[0],
                        w.start[1],
                        w.width / 2,
                        w.end[0],
                        w.end[1],
                        w.width / 2
                    )?;
                }
                for v in state.supply.vias.iter().filter(|v| v.polarity == polarity) {
                    write!(
                        output,
                        "\n  + ROUTED {} 0 ( {} {} ) {}",
                        def_name(&cfg.rail_layer),
                        v.at[0],
                        v.at[1],
                        def_name(&v.name)
                    )?;
                }
            }
            writeln!(output, " ;")?;
        }
        writeln!(output, "END {section}")?;
    }
    writeln!(output, "END DESIGN")?;
    Ok(())
}

pub(super) fn table_bytes(state: &PhysicalState) -> Result<Vec<u8>> {
    let count = state
        .io_pins
        .len()
        .checked_add(state.supply.ports.len())
        .and_then(|n| n.checked_add(state.supply.wires.len()))
        .and_then(|n| n.checked_add(state.supply.vias.len()))
        .ok_or_else(|| Error::Budget("physical table count overflow".into()))?;
    let bytes = count
        .checked_mul(64)
        .ok_or_else(|| Error::Budget("physical table size overflow".into()))?;
    if bytes > crate::MAX_METADATA_BYTES as usize {
        return Err(Error::Budget("physical table byte cap".into()));
    }
    let mut out = Vec::with_capacity(bytes);
    let mut record = |values: [i64; 8]| {
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
    };
    for (i, p) in state.io_pins.iter().enumerate() {
        record([
            1, i as i64, p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy, 0, 0,
        ]);
    }
    for (i, p) in state.supply.ports.iter().enumerate() {
        record([
            2, i as i64, p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy, 0, 0,
        ]);
    }
    for (i, w) in state.supply.wires.iter().enumerate() {
        record([
            3, i as i64, w.start[0], w.start[1], w.end[0], w.end[1], w.width, 0,
        ]);
    }
    for (i, v) in state.supply.vias.iter().enumerate() {
        record([4, i as i64, v.at[0], v.at[1], 0, 0, 0, 0]);
    }
    Ok(out)
}
