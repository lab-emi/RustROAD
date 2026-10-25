//! Bounded level-balanced clock-buffer topology. No routed/skew claim.
//! State and work estimates were recorded in docs/tap-budget.md before this pass.
use crate::pdk::PhysicalBundle;
use crate::{Error, Result, Stage, Summary};
use rustroad_core::connectivity::{Direction, PinOwner};
use rustroad_core::{Cell, DesignDb, Limits, Placed};
use rustroad_formats::verilog;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConfig {
    pub port: String,
    pub cell: String,
    pub input_pin: String,
    pub output_pin: String,
    pub sink_pin: String,
    pub max_fanout: usize,
    pub max_buffers: usize,
    pub max_work: usize,
    /// Optional clock-buffer master per tree level. Index 0 is the level that
    /// drives the sinks, index k the level k steps closer to the root; levels
    /// beyond the list reuse its last entry. Empty (the default) uses `cell`
    /// everywhere, which is the previous behaviour. Every master must expose
    /// exactly `input_pin`/`output_pin` as its single input/output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub level_cells: Vec<String>,
}
impl ClockConfig {
    /// Master for a tree level counted from the sinks (0 = drives the sinks).
    pub fn cell_for_level(&self, level_from_sinks: usize) -> &str {
        match self.level_cells.len() {
            0 => &self.cell,
            n => &self.level_cells[level_from_sinks.min(n - 1)],
        }
    }
    /// Every master this configuration may instantiate (deduplicated, ordered).
    pub fn masters(&self) -> Vec<String> {
        let mut out = vec![self.cell.clone()];
        for c in &self.level_cells {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }
    pub fn validate(&self) -> Result<()> {
        if self.level_cells.len() > 32
            || self
                .level_cells
                .iter()
                .any(|n| !rustroad_core::valid_object_name(n))
        {
            return Err(Error::Invalid("invalid clock level_cells".into()));
        }
        if self.max_fanout < 2
            || self.max_fanout > 1024
            || self.max_buffers > 250_000
            || self.max_work > 10_000_000
            || self.max_work == 0
        {
            return Err(Error::Budget("clock configuration limit".into()));
        }
        if [
            &self.port,
            &self.cell,
            &self.input_pin,
            &self.output_pin,
            &self.sink_pin,
        ]
        .iter()
        .any(|n| !rustroad_core::valid_object_name(n))
            || self.input_pin == self.output_pin
        {
            return Err(Error::Invalid("invalid clock role".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockSink {
    pub instance: String,
    pub pin: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockReport {
    pub sinks: Vec<ClockSink>,
    pub buffers: Vec<String>,
    pub levels: usize,
    pub max_fanout: usize,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<ClockConstraintBinding>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConstraintBinding {
    pub source: crate::pdk::SourcePin,
    pub clock: rustroad_formats::sdc::ClockConstraints,
}
fn fail(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
fn root_net(bundle: &PhysicalBundle, cfg: &ClockConfig) -> Result<u32> {
    let p = bundle
        .connectivity
        .ports
        .iter()
        .find(|p| p.name == cfg.port)
        .ok_or_else(|| fail("clock port absent"))?;
    if p.direction != Direction::Input || p.bits != [cfg.port.clone()] {
        return Err(fail("clock must be one scalar input port"));
    }
    let root = bundle
        .connectivity
        .nets
        .iter()
        .position(|n| n.aliases.contains(&cfg.port))
        .map(|n| n as u32)
        .ok_or_else(|| fail("clock source net absent"))?;
    for pin in &bundle.connectivity.pins {
        if pin.net == Some(root)
            && let PinOwner::Port { port, .. } = pin.owner
            && bundle.connectivity.ports[port as usize].name != cfg.port
        {
            return Err(fail("clock source aliases another top-level port"));
        }
    }
    Ok(root)
}
fn master<'a>(
    bundle: &'a PhysicalBundle,
    cfg: &ClockConfig,
    name: &str,
) -> Result<&'a rustroad_core::connectivity::Master> {
    let m = bundle
        .connectivity
        .masters
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| fail("clock buffer master absent"))?;
    let inputs: Vec<_> = m
        .pins
        .iter()
        .filter(|p| !p.use_.is_supply() && p.direction == Some(Direction::Input))
        .collect();
    let outputs: Vec<_> = m
        .pins
        .iter()
        .filter(|p| !p.use_.is_supply() && p.direction == Some(Direction::Output))
        .collect();
    if inputs.len() != 1
        || outputs.len() != 1
        || inputs[0].name != cfg.input_pin
        || outputs[0].name != cfg.output_pin
    {
        return Err(fail(
            "clock buffer must have the configured one input and one output",
        ));
    }
    Ok(m)
}

/// A LEF `ANTENNACELL` master with exactly one non-supply pin, an input: an
/// antenna diode. It may stay on the clock source net as a passive load (it
/// still counts toward the net's fanout); it is never a clock sink.
pub(crate) fn is_passive_antenna(master: &rustroad_core::connectivity::Master) -> bool {
    let signal: Vec<_> = master.pins.iter().filter(|p| !p.use_.is_supply()).collect();
    master.class.as_deref().is_some_and(|c| {
        c.split_whitespace()
            .any(|w| w.eq_ignore_ascii_case("ANTENNACELL"))
    }) && signal.len() == 1
        && signal[0].direction == Some(Direction::Input)
}

pub(crate) fn validate_tree(bundle: &PhysicalBundle) -> Result<()> {
    let Some(report) = &bundle.clock_tree else {
        return Ok(());
    };
    let cfg = bundle
        .config
        .as_ref()
        .and_then(|c| c.clock.as_ref())
        .ok_or_else(|| fail("clock tree without configuration"))?;
    cfg.validate()?;
    for name in cfg.masters() {
        master(bundle, cfg, &name)?;
    }
    let fanout = if let Some(c) = &report.constraints {
        c.clock.validate().map_err(Error::Invalid)?;
        if c.clock.port != cfg.port
            || c.source.bytes > 1024 * 1024
            || c.source.sha256.len() != 64
            || !c.source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(fail("clock constraint source/port mismatch"));
        }
        cfg.max_fanout
            .min(c.clock.max_fanout.unwrap_or(cfg.max_fanout))
    } else {
        cfg.max_fanout
    };
    if report.buffers.is_empty()
        || report.buffers.len() > cfg.max_buffers
        || report.sinks.is_empty()
        || report.sinks.len() > 1_000_000
        || report.levels == 0
        || report.levels > 32
        || report.max_fanout != fanout
    {
        return Err(budget("clock report limit"));
    }
    let by_name: HashMap<_, _> = bundle
        .netlist
        .instances
        .iter()
        .enumerate()
        .map(|(i, x)| (x.name.as_str(), i))
        .collect();
    let buffers: HashSet<_> = report
        .buffers
        .iter()
        .map(|n| {
            by_name
                .get(n.as_str())
                .copied()
                .ok_or_else(|| fail("clock buffer missing"))
        })
        .collect::<Result<_>>()?;
    let leaves: HashSet<_> = report
        .sinks
        .iter()
        .map(|s| {
            Ok((
                *by_name
                    .get(s.instance.as_str())
                    .ok_or_else(|| fail("clock sink missing"))?,
                s.pin.as_str(),
            ))
        })
        .collect::<Result<_>>()?;
    if buffers.len() != report.buffers.len() || leaves.len() != report.sinks.len() {
        return Err(fail("duplicate clock report endpoint"));
    }
    let allowed = cfg.masters();
    for &i in &buffers {
        if !allowed.contains(&bundle.netlist.instances[i].cell) {
            return Err(fail("clock buffer type mismatch"));
        }
    }
    let model = &bundle.connectivity;
    let root = root_net(bundle, cfg)?;
    let mut receivers = vec![Vec::new(); model.nets.len()];
    let mut outputs = HashMap::new();
    for pin in &model.pins {
        if let (PinOwner::Instance { instance, terminal }, Some(net)) = (pin.owner, pin.net) {
            let spec = &model.masters[model.instance_masters[instance as usize] as usize].pins
                [terminal as usize];
            if spec.use_.is_supply() {
                continue;
            }
            if spec.direction == Some(Direction::Input) {
                receivers[net as usize].push((instance as usize, terminal as usize));
            } else if spec.direction == Some(Direction::Output)
                && buffers.contains(&(instance as usize))
                && spec.name == cfg.output_pin
            {
                outputs.insert(instance as usize, net);
            }
        }
    }
    let mut queue = VecDeque::from([(root, 0usize)]);
    let mut visited = HashSet::new();
    let mut seen_buffers = HashSet::new();
    let mut seen_leaves = HashSet::new();
    let mut work = 0usize;
    while let Some((net, depth)) = queue.pop_front() {
        if depth > 32 || !visited.insert(net) {
            return Err(fail("clock cycle or merged tree branch"));
        }
        if receivers[net as usize].is_empty() || receivers[net as usize].len() > fanout {
            return Err(fail("clock fanout exceeded"));
        }
        for &(instance, terminal) in &receivers[net as usize] {
            work += 1;
            if work > cfg.max_work {
                return Err(budget("clock validation max_work"));
            }
            let master = &model.masters[model.instance_masters[instance] as usize];
            let pin = &master.pins[terminal];
            if depth == 0 && !buffers.contains(&instance) && is_passive_antenna(master) {
                continue;
            }
            if buffers.contains(&instance) {
                if pin.name != cfg.input_pin || !seen_buffers.insert(instance) {
                    return Err(fail("clock buffer wiring error"));
                }
                // A buffer driven from BFS depth d sits report.levels - d
                // levels above the sinks (1-based), i.e. index levels-d-1.
                let level = report
                    .levels
                    .checked_sub(depth + 1)
                    .ok_or_else(|| fail("clock buffer deeper than the reported levels"))?;
                if bundle.netlist.instances[instance].cell != cfg.cell_for_level(level) {
                    return Err(fail("clock buffer master differs from its level policy"));
                }
                queue.push_back((
                    *outputs
                        .get(&instance)
                        .ok_or_else(|| fail("clock buffer output missing"))?,
                    depth + 1,
                ));
            } else {
                if pin.name != cfg.sink_pin
                    || !leaves.contains(&(instance, pin.name.as_str()))
                    || depth != report.levels
                {
                    return Err(fail("clock sink/depth mismatch"));
                }
                seen_leaves.insert((instance, pin.name.as_str()));
            }
        }
    }
    if seen_leaves.len() != leaves.len() || seen_buffers.len() != buffers.len() {
        return Err(fail("disconnected clock tree"));
    }
    Ok(())
}

pub fn synthesize(path: &Path, base_rev: &str, limits: &Limits) -> Result<Summary> {
    synthesize_with_sdc(path, base_rev, limits, None)
}

pub fn synthesize_with_sdc(
    path: &Path,
    base_rev: &str,
    limits: &Limits,
    sdc: Option<&Path>,
) -> Result<Summary> {
    let lock = crate::lock(path, false, true)?;
    let current = crate::head(path)?;
    crate::check_base(base_rev, &current)?;
    let snapshot = crate::load(path, lock, limits)?;
    if snapshot.metadata().stage != Stage::Placed {
        return Err(fail("place the design before clock synthesis"));
    }
    let mut metadata = snapshot.metadata().clone();
    let positions: Vec<Placed> = snapshot.records().collect();
    let bundle = metadata
        .physical
        .as_mut()
        .ok_or_else(|| fail("mapped design required"))?;
    if bundle.clock_tree.is_some() {
        return Err(fail(
            "clock tree already exists; reimport to choose a new topology",
        ));
    }
    let cfg = bundle
        .config
        .as_ref()
        .and_then(|c| c.clock.clone())
        .ok_or_else(|| fail("clock configuration required"))?;
    cfg.validate()?;
    let constraints = sdc
        .map(|p| {
            let (text, source) = crate::pdk::read_input(p, 1024 * 1024, "cts_sdc")?;
            let clock = rustroad_formats::sdc::parse(&text).map_err(Error::Invalid)?;
            if clock.port != cfg.port {
                return Err(fail(
                    "SDC clock port differs from physical clock configuration",
                ));
            }
            Ok(ClockConstraintBinding { source, clock })
        })
        .transpose()?;
    let fanout = cfg.max_fanout.min(
        constraints
            .as_ref()
            .and_then(|c| c.clock.max_fanout)
            .unwrap_or(cfg.max_fanout),
    );
    let mut sizes = HashMap::new();
    for name in cfg.masters() {
        sizes.insert(name.clone(), master(bundle, &cfg, &name)?.size);
    }
    let root = root_net(bundle, &cfg)?;
    let mut sinks = Vec::new();
    for pin in &bundle.connectivity.pins {
        if pin.net == Some(root)
            && let PinOwner::Instance { instance, terminal } = pin.owner
        {
            let m = &bundle.connectivity.masters
                [bundle.connectivity.instance_masters[instance as usize] as usize];
            if is_passive_antenna(m) {
                continue;
            }
            let p = &m.pins[terminal as usize];
            if p.name != cfg.sink_pin || p.direction != Some(Direction::Input) {
                return Err(fail("clock root drives an unsupported non-clock pin"));
            }
            sinks.push((
                positions[instance as usize].y,
                positions[instance as usize].x,
                bundle.netlist.instances[instance as usize].name.clone(),
                p.name.clone(),
            ));
        }
    }
    if sinks.is_empty() {
        return Err(fail("clock has no selected sinks"));
    }
    sinks.sort();
    let leaves: Vec<_> = sinks
        .iter()
        .map(|s| ClockSink {
            instance: s.2.clone(),
            pin: s.3.clone(),
        })
        .collect();
    crate::physical::clear_fillers(&mut metadata.design, bundle)?;
    let map: HashMap<_, _> = bundle
        .netlist
        .instances
        .iter()
        .enumerate()
        .map(|(i, x)| (x.name.clone(), i))
        .collect();
    let mut current: Vec<_> = sinks.into_iter().map(|s| (map[&s.2], s.3)).collect();
    let mut names: HashSet<String> = bundle
        .netlist
        .instances
        .iter()
        .map(|i| i.name.clone())
        .chain(bundle.netlist.wires.iter().map(|w| w.name.clone()))
        .chain(bundle.netlist.ports.iter().map(|p| p.name.clone()))
        .collect();
    let mut buffers = Vec::new();
    let available_buffers = bundle
        .config
        .as_ref()
        .unwrap()
        .max_added_cells
        .saturating_sub(bundle.ties.as_ref().map_or(0, |t| t.new_cells));
    let mut levels = 0usize;
    let mut work = 0usize;
    let mut sequence = 0usize;
    loop {
        levels += 1;
        if levels > 32 {
            return Err(budget("clock tree depth"));
        }
        let cell = cfg.cell_for_level(levels - 1).to_string();
        let size = sizes[&cell];
        let mut next = Vec::new();
        for group in current.chunks(fanout) {
            work = work
                .checked_add(group.len() + 1)
                .ok_or_else(|| budget("clock work overflow"))?;
            if work > cfg.max_work {
                return Err(budget("clock max_work"));
            }
            if buffers.len() >= cfg.max_buffers
                || buffers.len() >= available_buffers
                || metadata.design.cells.len() >= limits.max_cells
            {
                return Err(budget("clock buffer count"));
            }
            let (name, net) = loop {
                if sequence > cfg.max_work {
                    return Err(budget("clock name collision budget"));
                }
                let n = format!("rr_clkbuf_{sequence}");
                let w = format!("rr_clock_{sequence}");
                sequence += 1;
                if !names.contains(&n) && !names.contains(&w) {
                    names.insert(n.clone());
                    names.insert(w.clone());
                    break (n, w);
                }
            };
            for (instance, pin) in group {
                bundle.netlist.instances[*instance]
                    .connections
                    .iter_mut()
                    .find(|c| &c.pin == pin)
                    .ok_or_else(|| fail("clock sink connection absent"))?
                    .signal = verilog::Expr::Ident(net.clone());
            }
            let i = bundle.netlist.instances.len();
            bundle.netlist.instances.push(verilog::Instance {
                name: name.clone(),
                cell: cell.clone(),
                connections: vec![
                    verilog::Connection {
                        pin: cfg.input_pin.clone(),
                        signal: verilog::Expr::Ident(cfg.port.clone()),
                    },
                    verilog::Connection {
                        pin: cfg.output_pin.clone(),
                        signal: verilog::Expr::Ident(net.clone()),
                    },
                ],
            });
            bundle.netlist.wires.push(verilog::Wire {
                name: net,
                range: None,
            });
            metadata.design.cells.push(Cell {
                name: name.clone(),
                master: cell.clone(),
                width: size[0],
                height: size[1],
            });
            buffers.push(name);
            next.push((i, cfg.input_pin.clone()));
        }
        current = next;
        if current.len() == 1 {
            break;
        }
    }
    bundle.clock_tree = Some(ClockReport {
        sinks: leaves,
        buffers,
        levels,
        max_fanout: fanout,
        scope: "unrouted level-balanced topology; no skew/RC guarantee".into(),
        constraints,
    });
    crate::physical::relink(bundle)?;
    validate_tree(bundle)?;
    let initial = rustroad_core::place_cpu_ref(&DesignDb::new(&metadata.design, limits)?, limits)?;
    let (records, extra) =
        crate::physical::prepare(&mut metadata.design, bundle, initial.cells, limits)?;
    metadata.expansions = initial
        .expansions
        .checked_add(extra)
        .ok_or_else(|| budget("clock placement work overflow"))?;
    if metadata.expansions > limits.max_expansions {
        return Err(budget("clock placement expansion cap"));
    }
    metadata.parent_revision = crate::head(path)?;
    metadata.stage = Stage::Placed;
    crate::commit(path, metadata, &records)
}
