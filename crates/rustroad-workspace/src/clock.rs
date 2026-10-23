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
}
impl ClockConfig {
    pub fn validate(&self) -> Result<()> {
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
) -> Result<&'a rustroad_core::connectivity::Master> {
    let m = bundle
        .connectivity
        .masters
        .iter()
        .find(|m| m.name == cfg.cell)
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
    master(bundle, cfg)?;
    if report.buffers.is_empty()
        || report.buffers.len() > cfg.max_buffers
        || report.sinks.is_empty()
        || report.sinks.len() > 1_000_000
        || report.levels == 0
        || report.levels > 32
        || report.max_fanout != cfg.max_fanout
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
    for &i in &buffers {
        if bundle.netlist.instances[i].cell != cfg.cell {
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
        if receivers[net as usize].len() > cfg.max_fanout {
            return Err(fail("clock fanout exceeded"));
        }
        for &(instance, terminal) in &receivers[net as usize] {
            work += 1;
            if work > cfg.max_work {
                return Err(budget("clock validation max_work"));
            }
            let pin = &model.masters[model.instance_masters[instance] as usize].pins[terminal];
            if buffers.contains(&instance) {
                if pin.name != cfg.input_pin || !seen_buffers.insert(instance) {
                    return Err(fail("clock buffer wiring error"));
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
    let m = master(bundle, &cfg)?;
    let size = m.size;
    let root = root_net(bundle, &cfg)?;
    let mut sinks = Vec::new();
    for pin in &bundle.connectivity.pins {
        if pin.net == Some(root)
            && let PinOwner::Instance { instance, terminal } = pin.owner
        {
            let p = &bundle.connectivity.masters
                [bundle.connectivity.instance_masters[instance as usize] as usize]
                .pins[terminal as usize];
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
        let mut next = Vec::new();
        for group in current.chunks(cfg.max_fanout) {
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
                cell: cfg.cell.clone(),
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
                master: cfg.cell.clone(),
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
        max_fanout: cfg.max_fanout,
        scope: "unrouted level-balanced topology; no skew/RC guarantee".into(),
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
