//! `floorplan`: derive die, core, rows and an I/O plan from cell area,
//! target utilization and aspect ratio (files only; no workspace).
//!
//! * Rows are as long as `tap + k * (tap + gap)` so the tap reservation lands
//!   exactly on its maximum clear gap, which must hold the widest cell.
//! * `k` grows until the free (non-tap) row area times the target utilization
//!   holds the logic, tie and estimated clock-buffer area (plus the configured
//!   cell spacing); the row count follows the aspect ratio.
//! * Core and die edges sit on the site and row grids, so cell pins keep their
//!   track alignment. Margins hold the I/O pins, their routing channel and
//!   the supply spines.
//! * I/O pins are square, at least the I/O layer AREA, and large enough to
//!   contain an on-track landing node of the I/O layer; their centres are
//!   snapped to the tracks along each side.
//! * The I/O order is connectivity aware: a bounded analytic placement of the
//!   netlist (I/O pins first spread round-robin) gives the centroid of the
//!   cells each pin connects to; pins go to the nearest side with capacity,
//!   ordered by their centroid along the side, and the placement is repeated
//!   once with that plan.
//!
//! Work is O(cells + pins) plus two bounded placements (`rustroad_core::place`).
use crate::eco::{Artifact, write_atomic};
use crate::pdk::read_input;
use crate::physical::PhysicalConfig;
use crate::routing::RoutingConfig;
use crate::{Error, Result};
use rustroad_core::connectivity::{ConnectivityLimits, PinOwner};
use rustroad_core::io::{IoConstraints, IoLimits, IoPlan};
use rustroad_core::place::{Interval, PinRef, PlaceConfig, PlaceLimits, PlaceProblem};
use rustroad_core::{Rect, Row, Site};
use rustroad_formats::{lef, verilog};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

const MAX_LEF_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NETLIST_BYTES: u64 = 16 * 1024 * 1024;

pub struct FloorplanRequest {
    pub tech_lef: PathBuf,
    pub cell_lef: PathBuf,
    pub netlist: PathBuf,
    pub physical_config: PathBuf,
    pub routing_config: PathBuf,
    pub site: String,
    pub utilization: f64,
    pub aspect: f64,
    pub margin_um: f64,
    /// Clock port override; `None` keeps the profile's port.
    pub clock_port: Option<String>,
    /// Remove the clock configuration (combinational designs).
    pub no_clock: bool,
    /// Keep the fixed-order placer instead of enabling the analytic one.
    pub fixed_order: bool,
    pub out_floorplan: PathBuf,
    pub out_physical_config: PathBuf,
}

#[derive(Serialize)]
pub struct FloorplanReport {
    pub schema: &'static str,
    pub design: String,
    pub units_per_micron: u32,
    pub die_area: Rect,
    pub core_area: Rect,
    pub rows: usize,
    pub row_length_dbu: i64,
    pub tap_period: Option<usize>,
    pub logic_cells: usize,
    pub tie_cells: usize,
    pub clock_buffers_estimate: usize,
    pub cell_area_um2: f64,
    pub free_row_area_um2: f64,
    pub target_utilization: f64,
    pub estimated_utilization: f64,
    pub io_pins: usize,
    pub io_side_counts: [usize; 4],
    pub io_width_dbu: i64,
    pub io_order: &'static str,
    pub placement_hpwl_dbu: u64,
    pub outputs: Vec<Artifact>,
    pub status: &'static str,
}

fn fail(code: &'static str, message: impl Into<String>) -> Error {
    Error::Gds {
        code,
        message: message.into(),
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// `UNITS DATABASE MICRONS n` of a technology LEF.
fn database_units(text: &str) -> Result<u32> {
    let tokens: Vec<&str> = text.split_whitespace().take(20_000).collect();
    for w in tokens.windows(3) {
        if w[0] == "DATABASE" && w[1] == "MICRONS" {
            return w[2]
                .trim_end_matches(';')
                .parse::<u32>()
                .ok()
                .filter(|v| *v > 0 && *v <= 100_000)
                .ok_or_else(|| invalid("invalid LEF DATABASE MICRONS"));
        }
    }
    Err(invalid("technology LEF has no UNITS DATABASE MICRONS"))
}

fn round_up(v: i64, g: i64) -> i64 {
    (v + g - 1).div_euclid(g) * g
}

/// Track `(pitch, offset)` of one routing layer: LEF PITCH/OFFSET along the
/// cross axis, replaced by the routing profile's override.
fn layer_track(
    layers: &[lef::LefLayer],
    routing: &RoutingConfig,
    name: &str,
) -> Result<(i64, i64, bool, i64)> {
    let l = layers
        .iter()
        .find(|l| l.name == name)
        .ok_or_else(|| invalid(format!("routing layer {name} absent from LEF")))?;
    let vertical = match l.direction.as_deref() {
        Some("VERTICAL") => true,
        Some("HORIZONTAL") => false,
        _ => return Err(invalid(format!("layer {name} has no direction"))),
    };
    let axis = usize::from(!vertical);
    let over = routing
        .track_router
        .as_ref()
        .and_then(|t| t.tracks.iter().find(|o| o.layer == name));
    let pitch = over
        .and_then(|o| o.pitch)
        .or(l.pitch.map(|p| p[axis]))
        .ok_or_else(|| invalid(format!("layer {name} has no PITCH")))?;
    let offset = over
        .and_then(|o| o.offset)
        .or(l.offset.map(|p| p[axis]))
        .unwrap_or(0);
    Ok((pitch, offset, vertical, l.width.unwrap_or(0)))
}

/// Largest gap between consecutive coordinates of the union of track sets.
fn max_gap(tracks: &[(i64, i64)]) -> i64 {
    let period = tracks.iter().fold(1i64, |acc, (p, _)| {
        let g = gcd(acc, *p);
        (acc / g).saturating_mul(*p).min(1 << 30)
    });
    let mut coords: Vec<i64> = Vec::new();
    for &(p, o) in tracks {
        let mut c = o.rem_euclid(p);
        while c < 2 * period && coords.len() < 1 << 16 {
            coords.push(c);
            c += p;
        }
    }
    coords.sort_unstable();
    coords.dedup();
    coords.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0)
}
fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.abs().max(1)
}

pub fn floorplan(req: &FloorplanRequest) -> Result<FloorplanReport> {
    if !(req.utilization > 0.05 && req.utilization <= 0.95)
        || !(req.aspect >= 0.1 && req.aspect <= 10.0)
        || !(req.margin_um >= 0.0 && req.margin_um <= 10_000.0)
        || !rustroad_core::valid_object_name(&req.site)
    {
        return Err(invalid(
            "floorplan utilization/aspect/margin/site out of range",
        ));
    }
    for out in [&req.out_floorplan, &req.out_physical_config] {
        for input in [
            &req.tech_lef,
            &req.cell_lef,
            &req.netlist,
            &req.physical_config,
            &req.routing_config,
        ] {
            if out == input {
                return Err(fail(
                    "OUTPUT_PATH",
                    "floorplan output would overwrite an input",
                ));
            }
        }
    }
    if req.out_floorplan == req.out_physical_config {
        return Err(fail("OUTPUT_PATH", "floorplan outputs must differ"));
    }
    let (tech_text, _) = read_input(&req.tech_lef, MAX_LEF_BYTES, "technology_lef")?;
    let dbu = database_units(&tech_text)?;
    let llef = lef::LefLimits {
        max_input_bytes: MAX_LEF_BYTES as usize,
        ..Default::default()
    };
    let tech = lef::parse_lef(&tech_text, dbu, &llef).map_err(|e| invalid(e.to_string()))?;
    drop(tech_text);
    let (cell_text, _) = read_input(&req.cell_lef, MAX_LEF_BYTES, "cell_lef")?;
    let cells = lef::parse_lef(&cell_text, dbu, &llef).map_err(|e| invalid(e.to_string()))?;
    drop(cell_text);
    let (cfg_text, _) = read_input(&req.physical_config, 1024 * 1024, "physical_config")?;
    let mut cfg: PhysicalConfig = serde_json::from_str(&cfg_text)?;
    let (route_text, _) = read_input(&req.routing_config, 1024 * 1024, "routing_config")?;
    let routing: RoutingConfig = serde_json::from_str(&route_text)?;
    if req.no_clock {
        cfg.clock = None;
    } else if let (Some(port), Some(clock)) = (&req.clock_port, cfg.clock.as_mut()) {
        clock.port = port.clone();
    }
    if !req.fixed_order && cfg.placer.is_none() {
        cfg.placer = Some(PlaceConfig::default());
    }
    cfg.io_plan = None;
    cfg.validate()?;
    let (net_text, _) = read_input(&req.netlist, MAX_NETLIST_BYTES, "mapped_verilog")?;
    let netlist = verilog::parse_verilog(&net_text, &verilog::VerilogLimits::default())
        .map_err(|e| invalid(e.to_string()))?;
    drop(net_text);
    let find = |name: &str| {
        tech.macros
            .iter()
            .chain(&cells.macros)
            .find(|m| m.name == name)
            .ok_or_else(|| invalid(format!("missing LEF master {name}")))
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
    let (netlist, _) = crate::ties::dissolve_constant_cells(
        &netlist,
        &specs,
        high,
        low,
        &crate::ties::TieLimits::default(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let (netlist, ties) = crate::ties::materialize_constants_with(
        &netlist,
        high,
        low,
        &crate::ties::TieLimits::default(),
    )
    .map_err(|e| invalid(e.to_string()))?;
    let site = tech
        .sites
        .iter()
        .chain(&cells.sites)
        .find(|s| s.name == req.site)
        .ok_or_else(|| invalid(format!("site {} absent from LEF", req.site)))?;
    let (sw, sh) = (site.size[0], site.size[1]);
    let gap = i64::from(cfg.cell_spacing_sites) * sw;
    let mut widest = 0i64;
    let mut area = 0i128;
    for inst in &netlist.instances {
        let w = find(&inst.cell)?.size[0];
        widest = widest.max(w);
        area += i128::from(w + gap);
    }
    // Clock buffers the bounded CTS will add (same grouping arithmetic).
    let mut buffers = 0usize;
    if let Some(clock) = &cfg.clock {
        let sinks = netlist
            .instances
            .iter()
            .filter(|i| {
                i.connections.iter().any(|c| {
                    c.pin == clock.sink_pin
                        && matches!(&c.signal, verilog::Expr::Ident(n) if *n == clock.port)
                })
            })
            .count();
        let mut current = sinks;
        while current > 0 {
            let groups = current.div_ceil(clock.max_fanout.max(2));
            buffers += groups;
            if groups <= 1 {
                break;
            }
            current = groups;
        }
        let bw = clock
            .masters()
            .iter()
            .map(|m| find(m).map(|m| m.size[0]))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        widest = widest.max(bw);
        area += i128::from(bw + gap) * buffers as i128;
    }
    let area = i64::try_from(area).map_err(|_| Error::Budget("cell area overflow".into()))?;
    // Row length: tap + k * (tap + clear gap).
    let (tap_w, clear) = match &cfg.tap {
        Some(t) => {
            let tw = find(&t.cell)?.size[0];
            let clear = t.max_gap / sw * sw;
            if clear < widest + gap {
                return Err(fail(
                    "FLOORPLAN_TAP_GAP",
                    format!(
                        "widest cell {widest} DBU (+ spacing {gap}) exceeds the tap clear gap {clear} DBU; widen tap.max_gap in the physical profile"
                    ),
                ));
            }
            (tw, clear)
        }
        None => (0, 16 * sw),
    };
    // I/O pin size: AREA-legal square holding an on-track landing node.
    let grid = tech
        .manufacturing_grid
        .or(cells.manufacturing_grid)
        .ok_or_else(|| invalid("manufacturing grid missing"))?;
    let layers: Vec<lef::LefLayer> = tech.layers.iter().chain(&cells.layers).cloned().collect();
    let names: Vec<&str> = routing.layers.iter().map(|l| l.name.as_str()).collect();
    let io_index = names
        .iter()
        .position(|n| *n == cfg.io_layer)
        .ok_or_else(|| invalid("I/O layer is not a routing-profile layer"))?;
    let (io_pitch, io_offset, io_vertical, io_wire) =
        layer_track(&layers, &routing, &cfg.io_layer)?;
    let mut cross = Vec::new();
    for j in [io_index.wrapping_sub(1), io_index + 1] {
        if let Some(n) = names.get(j) {
            let t = layer_track(&layers, &routing, n)?;
            if t.2 != io_vertical {
                cross.push(t);
            }
        }
    }
    let neighbour = *cross
        .first()
        .ok_or_else(|| invalid("I/O layer has no perpendicular neighbour layer"))?;
    let stop_gap = max_gap(&cross.iter().map(|t| (t.0, t.1)).collect::<Vec<_>>());
    let landing = round_up(io_pitch.max(stop_gap) + io_wire + 2 * grid, 2 * grid);
    let io_layer = layers
        .iter()
        .find(|l| l.name == cfg.io_layer)
        .ok_or_else(|| invalid("I/O layer absent"))?;
    let io_width = cfg
        .io_width
        .unwrap_or(0)
        .max(io_layer.width.unwrap_or(0))
        .max(landing);
    cfg.io_width = Some(io_width);
    let (x_track, y_track) = if io_vertical {
        ([io_pitch, io_offset], [neighbour.0, neighbour.1])
    } else {
        ([neighbour.0, neighbour.1], [io_pitch, io_offset])
    };
    let area_side = match (cfg.io_min_area, io_layer.min_area) {
        (true, Some(a)) => {
            let a = a as u128;
            let mut side = a.isqrt();
            if side * side < a {
                side += 1;
            }
            round_up(side as i64, grid)
        }
        _ => 0,
    };
    let io_spacing = cfg
        .io_spacing
        .or(io_layer.spacing)
        .ok_or_else(|| invalid("I/O spacing missing"))?
        .max(io_layer.spacing.unwrap_or(0));
    let io_bits: usize = netlist
        .ports
        .iter()
        .map(|p| {
            p.range
                .map_or(1, |r| (r.msb - r.lsb).unsigned_abs() as usize + 1)
        })
        .sum();
    let probe = IoPlan {
        west: Vec::new(),
        east: Vec::new(),
        south: Vec::new(),
        north: Vec::new(),
        x_track: Some(x_track),
        y_track: Some(y_track),
    };
    let margin = (req.margin_um * f64::from(dbu)).round() as i64;
    let (mx, my) = (round_up(margin.max(sw), sw), round_up(margin.max(sh), sh));
    // Smallest k (row length) whose rows hold the cells at the target
    // utilization and whose die perimeter holds every I/O pin with a
    // quarter spare for the nearest-side assignment.
    let mut k = 1usize;
    let (length, rows, core, die, constraints) = loop {
        let length = tap_w + k as i64 * (tap_w + clear);
        let rows = ((req.aspect * length as f64 / sh as f64).round() as usize).max(1);
        let free = rows as f64 * k as f64 * clear as f64;
        k += 1;
        if k > 100_000 || rows > 100_000 {
            return Err(Error::Budget("floorplan row search".into()));
        }
        if free * req.utilization < area as f64 {
            continue;
        }
        let core = Rect {
            lx: mx,
            ly: my,
            ux: mx + length,
            uy: my + rows as i64 * sh,
        };
        let die = Rect {
            lx: 0,
            ly: 0,
            ux: core.ux + mx,
            uy: core.uy + my,
        };
        let constraints = IoConstraints {
            die,
            core,
            grid,
            layer: cfg.io_layer.clone(),
            min_width: io_width.max(area_side),
            min_spacing: io_spacing,
        };
        let caps = rustroad_core::io::planned_capacity(&constraints, &probe, &IoLimits::default())
            .map_err(|e| invalid(e.to_string()))?;
        if caps.iter().sum::<usize>() * 4 >= io_bits * 5 {
            break (length, rows, core, die, constraints);
        }
    };
    let k = k - 1;
    let floor_rows: Vec<Row> = (0..rows)
        .map(|i| Row {
            x_start: core.lx,
            x_end: core.ux,
            y: core.ly + i as i64 * sh,
            orientation: rustroad_core::Orientation::N,
        })
        .collect();
    // Connectivity: link the netlist against the LEF masters.
    let design_floor = crate::pdk::FloorplanSpec {
        name: netlist.module.clone(),
        units_per_micron: dbu,
        die_area: die,
        core_area: core,
        site: Site {
            name: req.site.clone(),
            width: sw,
            height: sh,
        },
        rows: floor_rows.clone(),
    };
    let (_, masters) =
        crate::pdk::select_technology_for(&tech, &cells, &design_floor, &netlist, &[])?;
    let index: HashMap<&str, u32> = masters
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i as u32))
        .collect();
    let instance_masters: Vec<u32> = netlist
        .instances
        .iter()
        .map(|i| index[i.cell.as_str()])
        .collect();
    let (ports, nets, pins) = crate::pdk::link(
        &netlist,
        &masters,
        &instance_masters,
        &ConnectivityLimits::default(),
    )?;
    let io_names: Vec<String> = ports.iter().flat_map(|p| p.bits.iter().cloned()).collect();
    let mut net_pins: Vec<Vec<(Option<u32>, Option<usize>)>> = vec![Vec::new(); nets.len()];
    let bit_index: HashMap<&str, usize> = io_names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    for pin in &pins {
        let Some(n) = pin.net else { continue };
        match pin.owner {
            PinOwner::Instance { instance, terminal } => {
                let m = &masters[instance_masters[instance as usize] as usize];
                if !m.pins[terminal as usize].use_.is_supply() {
                    net_pins[n as usize].push((Some(instance), None));
                }
            }
            PinOwner::Port { port, bit } => {
                let name = &ports[port as usize].bits[bit as usize];
                net_pins[n as usize].push((None, Some(bit_index[name.as_str()])));
            }
        }
    }
    let intervals: Vec<Interval> = floor_rows
        .iter()
        .map(|r| Interval {
            y: r.y,
            lx: r.x_start,
            ux: r.x_end,
        })
        .collect();
    let widths: Vec<i64> = netlist
        .instances
        .iter()
        .map(|i| find(&i.cell).map(|m| m.size[0]))
        .collect::<Result<_>>()?;
    let pc = cfg.placer.clone().unwrap_or_default();
    let place_with = |io: &[rustroad_core::io::IoPin]| -> Result<(Vec<[i64; 2]>, u64)> {
        let centre: HashMap<&str, [i64; 2]> = io
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    [(p.rect.lx + p.rect.ux) / 2, (p.rect.ly + p.rect.uy) / 2],
                )
            })
            .collect();
        let nets: Vec<Vec<PinRef>> = net_pins
            .iter()
            .map(|n| {
                n.iter()
                    .map(|(c, b)| match (c, b) {
                        (Some(c), _) => PinRef::Cell(*c),
                        (None, Some(b)) => PinRef::Fixed(centre[io_names[*b].as_str()]),
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|n| n.len() >= 2)
            .collect();
        let problem = PlaceProblem {
            weights: Vec::new(),
            widths: widths.clone(),
            row_height: sh,
            site: sw,
            gap,
            nets,
            intervals: intervals.clone(),
        };
        let r = rustroad_core::place::place(&problem, &pc, &PlaceLimits::default())?;
        Ok((r.positions, r.hpwl))
    };
    let mut io =
        rustroad_core::io::place_boundary_io(&io_names, &constraints, &IoLimits::default())
            .map_err(|e| invalid(e.to_string()))?;
    let mut plan = None;
    let mut hpwl = 0;
    for _ in 0..2 {
        let (positions, h) = place_with(&io)?;
        hpwl = h;
        let p = io_plan(
            &io_names,
            &net_pins,
            &positions,
            &widths,
            sh,
            core,
            &constraints,
            x_track,
            y_track,
        )?;
        io = rustroad_core::io::place_planned_io(&io_names, &p, &constraints, &IoLimits::default())
            .map_err(|e| invalid(e.to_string()))?;
        plan = Some(p);
    }
    let plan = plan.unwrap();
    let side_counts = [
        plan.west.len(),
        plan.east.len(),
        plan.south.len(),
        plan.north.len(),
    ];
    cfg.io_plan = Some(plan);
    cfg.validate()?;
    let floor_json = serde_json::json!({
        "name": netlist.module,
        "units_per_micron": dbu,
        "die_area": die,
        "core_area": core,
        "site": {"name": req.site, "width": sw, "height": sh},
        "rows": floor_rows.iter().map(|r| serde_json::json!({"x_start": r.x_start, "x_end": r.x_end, "y": r.y})).collect::<Vec<_>>(),
    });
    let mut outputs = Vec::new();
    let mut bytes = serde_json::to_vec_pretty(&floor_json)?;
    bytes.push(b'\n');
    outputs.push(write_atomic(&req.out_floorplan, &bytes)?);
    let mut bytes = serde_json::to_vec_pretty(&cfg)?;
    bytes.push(b'\n');
    outputs.push(write_atomic(&req.out_physical_config, &bytes)?);
    let free_length = if cfg.tap.is_some() {
        k as i64 * clear
    } else {
        length
    };
    let free = rows as f64 * free_length as f64;
    let um2 = |v: f64| v / f64::from(dbu) / f64::from(dbu);
    Ok(FloorplanReport {
        schema: "rustroad.floorplan-report.v1",
        design: netlist.module.clone(),
        units_per_micron: dbu,
        die_area: die,
        core_area: core,
        rows,
        row_length_dbu: length,
        tap_period: cfg.tap.as_ref().map(|_| k),
        logic_cells: netlist.instances.len() - ties.new_cells,
        tie_cells: ties.new_cells,
        clock_buffers_estimate: buffers,
        cell_area_um2: um2(area as f64 * sh as f64),
        free_row_area_um2: um2(free * sh as f64),
        target_utilization: req.utilization,
        estimated_utilization: area as f64 / free.max(1.0),
        io_pins: io_names.len(),
        io_side_counts: side_counts,
        io_width_dbu: io_width,
        io_order: "connectivity_centroid_nearest_side",
        placement_hpwl_dbu: hpwl,
        outputs,
        status: "written",
    })
}

/// Nearest-side assignment with capacity, ordered along each side by the
/// centroid of the connected cells.
#[allow(clippy::too_many_arguments)]
fn io_plan(
    names: &[String],
    net_pins: &[Vec<(Option<u32>, Option<usize>)>],
    positions: &[[i64; 2]],
    widths: &[i64],
    row_height: i64,
    core: Rect,
    constraints: &IoConstraints,
    x_track: [i64; 2],
    y_track: [i64; 2],
) -> Result<IoPlan> {
    let mut sum = vec![[0i128; 2]; names.len()];
    let mut count = vec![0i128; names.len()];
    for net in net_pins {
        let bits: Vec<usize> = net.iter().filter_map(|p| p.1).collect();
        if bits.is_empty() {
            continue;
        }
        for (c, _) in net {
            if let Some(c) = c {
                let c = *c as usize;
                let centre = [
                    positions[c][0] + widths[c] / 2,
                    positions[c][1] + row_height / 2,
                ];
                for &b in &bits {
                    sum[b][0] += i128::from(centre[0]);
                    sum[b][1] += i128::from(centre[1]);
                    count[b] += 1;
                }
            }
        }
    }
    let middle = [(core.lx + core.ux) / 2, (core.ly + core.uy) / 2];
    let centroid: Vec<[i64; 2]> = (0..names.len())
        .map(|b| {
            if count[b] == 0 {
                middle
            } else {
                [(sum[b][0] / count[b]) as i64, (sum[b][1] / count[b]) as i64]
            }
        })
        .collect();
    let probe = IoPlan {
        west: Vec::new(),
        east: Vec::new(),
        south: Vec::new(),
        north: Vec::new(),
        x_track: Some(x_track),
        y_track: Some(y_track),
    };
    let caps = rustroad_core::io::planned_capacity(constraints, &probe, &IoLimits::default())
        .map_err(|e| invalid(e.to_string()))?;
    // Distance of each centroid to each core side (W, E, S, N).
    let dist = |p: [i64; 2]| -> [i64; 4] {
        [
            p[0] - core.lx,
            core.ux - p[0],
            p[1] - core.ly,
            core.uy - p[1],
        ]
    };
    let mut order: Vec<usize> = (0..names.len()).collect();
    // Strongest preference (largest gap between best and second side) first.
    order.sort_by_key(|&b| {
        let mut v = dist(centroid[b]);
        v.sort_unstable();
        (std::cmp::Reverse(v[1] - v[0]), b)
    });
    let mut sides: Vec<Vec<usize>> = vec![Vec::new(); 4];
    for b in order {
        let dv = dist(centroid[b]);
        let mut ranked = [0usize, 1, 2, 3];
        ranked.sort_by_key(|&s| (dv[s], s));
        let s = ranked
            .into_iter()
            .find(|&s| sides[s].len() < caps[s])
            .ok_or_else(|| fail("FLOORPLAN_IO_CAPACITY", "I/O pins exceed the die perimeter"))?;
        sides[s].push(b);
    }
    let mut lists: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (s, list) in sides.iter_mut().enumerate() {
        let vertical = s < 2;
        list.sort_by_key(|&b| (centroid[b][usize::from(vertical)], b));
        lists.insert(s, list.iter().map(|&b| names[b].clone()).collect());
    }
    Ok(IoPlan {
        west: lists.remove(&0).unwrap_or_default(),
        east: lists.remove(&1).unwrap_or_default(),
        south: lists.remove(&2).unwrap_or_default(),
        north: lists.remove(&3).unwrap_or_default(),
        x_track: Some(x_track),
        y_track: Some(y_track),
    })
}
