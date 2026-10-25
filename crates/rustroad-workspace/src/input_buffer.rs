//! Bounded nonclock primary-input fanout trees, with explicit physical cell roles.

use crate::pdk::PhysicalBundle;
use crate::{Error, Result};
use rustroad_core::connectivity::{ConnectedDesign, Direction, PinOwner, PinUse};
use rustroad_core::{Cell, Design, Limits};
use rustroad_formats::verilog;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::size_of;

const MAX_PRIMARY_BITS: usize = 1_000_000;
const MAX_LEVELS: usize = 64;

/// Identity-buffer role and independent resource bounds for primary-input fanout repair.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Noninverting cell master, certified separately against the functional library.
    pub cell: String,
    /// Signal input terminal of the configured master.
    pub input_pin: String,
    /// Signal output terminal of the configured master.
    pub output_pin: String,
    /// Maximum number of cell sinks on the source and each new buffer output.
    pub max_fanout: usize,
    /// Maximum buffers created across all selected primary inputs.
    pub max_buffers: usize,
    /// Maximum original sink pins moved into buffer trees.
    pub max_sinks: usize,
    /// Work limit for each construction or topology-validation pass.
    pub max_work: usize,
    /// Incremental scratch, appended AST and worst-case vector relocation allowance.
    pub memory_mib: u64,
}

impl Config {
    /// Validate supported fanout, work, memory and role bounds before allocating.
    ///
    /// # Errors
    /// Returns a diagnostic for unsupported bounds or malformed cell/pin names.
    pub fn validate(&self) -> Result<()> {
        if !(2..=128).contains(&self.max_fanout)
            || !(1..=250_000).contains(&self.max_buffers)
            || !(1..=2_000_000).contains(&self.max_sinks)
            || !(1..=1_000_000_000).contains(&self.max_work)
            || !(1..=4096).contains(&self.memory_mib)
        {
            return Err(budget("configuration limit"));
        }
        if self.input_pin == self.output_pin
            || [&self.cell, &self.input_pin, &self.output_pin]
                .iter()
                .any(|n| n.len() > 256 || !rustroad_core::valid_object_name(n))
        {
            return Err(invalid("invalid identity-buffer role"));
        }
        Ok(())
    }
}

/// Persistent identity of a generated buffer in the logical instance sequence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Buffer {
    /// Stable logical instance offset, before appended physical fillers and taps.
    pub instance: u32,
    /// Checked instance name at that offset.
    pub name: String,
}

/// One primary bit and the contiguous buffer range generated for its sinks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tree {
    /// Original primary input port name.
    pub port: String,
    /// Bit ordinal in the port's declared direction.
    pub bit: u32,
    /// Number of original functional cell input pins, excluding passive antenna loads.
    pub sinks: usize,
    /// Total physical input loads moved during import, including passive antenna cells.
    pub loads: usize,
    /// Start of this tree's range in [`Report::buffers`].
    pub first_buffer: usize,
    /// Number of generated buffers owned by this tree.
    pub buffers: usize,
    /// Number of buffer stages from the primary input to a leaf sink.
    pub levels: usize,
}

/// Bounded topology evidence; physical timing and functional equivalence are separate checks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// Source identities and complete, disjoint generated-buffer ranges.
    pub trees: Vec<Tree>,
    /// Generated logical instance identities.
    pub buffers: Vec<Buffer>,
    /// Fanout policy used for this import.
    pub max_fanout: usize,
    /// High-fanout clock inputs excluded during import and left for CTS.
    pub skipped_clock_inputs: usize,
}

fn budget(message: &str) -> Error {
    Error::Budget(format!("input buffering: {message}"))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(format!("input buffering: {message}"))
}

struct Work {
    used: usize,
    limit: usize,
}

impl Work {
    fn add(&mut self, amount: usize) -> Result<()> {
        self.used = self
            .used
            .checked_add(amount)
            .filter(|&n| n <= self.limit)
            .ok_or_else(|| budget("work limit"))?;
        Ok(())
    }
}

fn reserve<T>(count: usize) -> Result<Vec<T>> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(count)
        .map_err(|_| budget("allocation"))?;
    Ok(result)
}

fn filled<T: Clone>(count: usize, value: T) -> Result<Vec<T>> {
    let mut values = reserve(count)?;
    values.resize(count, value);
    Ok(values)
}

fn classification_bytes(model: &ConnectedDesign, work: &mut Work) -> Result<u64> {
    work.add(model.masters.len())?;
    Ok((model.masters.len() as u64).saturating_add(
        (model
            .masters
            .iter()
            .map(|m| m.pins.len())
            .max()
            .unwrap_or(0) as u64)
            .saturating_mul(16),
    ))
}

fn passive_masters(model: &ConnectedDesign, work: &mut Work) -> Result<Vec<bool>> {
    let mut passive = reserve(model.masters.len())?;
    for master in &model.masters {
        work.add(master.pins.len() + 1)?;
        passive.push(crate::clock::is_passive_antenna(master));
    }
    Ok(passive)
}

fn growth<T>(values: &Vec<T>, added: usize) -> u64 {
    let wanted = values.len().saturating_add(added);
    if wanted > values.capacity() {
        (wanted as u64).saturating_mul(size_of::<T>() as u64)
    } else {
        0
    }
}

fn tree_size(mut sinks: usize, fanout: usize) -> Result<(usize, usize)> {
    let mut count = 0usize;
    let mut levels = 0;
    while sinks > fanout {
        if levels == MAX_LEVELS {
            return Err(budget("tree depth"));
        }
        sinks = sinks.div_ceil(fanout);
        count = count
            .checked_add(sinks)
            .ok_or_else(|| budget("tree size overflow"))?;
        levels += 1;
    }
    Ok((count, levels))
}

#[derive(Clone, Copy)]
struct Sink {
    instance: u32,
    terminal: u32,
}

struct Root {
    net: u32,
    port: u32,
    bit: u32,
    count: usize,
    functional: usize,
    clock: bool,
    other_driver: bool,
    sinks: Vec<Sink>,
}

#[derive(Clone, Copy)]
enum Source {
    Root(usize),
    Buffer(usize),
}

struct Node {
    source: Source,
    name: String,
    wire: String,
}

fn expression(bundle: &PhysicalBundle, port: u32, bit: u32) -> Result<verilog::Expr> {
    let p = bundle
        .netlist
        .ports
        .get(port as usize)
        .ok_or_else(|| invalid("source port index"))?;
    Ok(match p.range {
        None if bit == 0 => verilog::Expr::Ident(p.name.clone()),
        None => return Err(invalid("scalar source port bit")),
        Some(range) => verilog::Expr::BitSelect {
            name: p.name.clone(),
            index: if range.msb <= range.lsb {
                range.msb.checked_add(i64::from(bit))
            } else {
                range.msb.checked_sub(i64::from(bit))
            }
            .ok_or_else(|| budget("source bit overflow"))?,
        },
    })
}

fn fresh(
    prefix: &str,
    names: &mut HashSet<String>,
    sequence: &mut usize,
    work: &mut Work,
) -> Result<String> {
    loop {
        work.add(1)?;
        let name = format!("{prefix}{sequence}");
        *sequence = sequence
            .checked_add(1)
            .ok_or_else(|| budget("name sequence"))?;
        if names.insert(name.clone()) {
            return Ok(name);
        }
    }
}

/// Insert bounded trees during import, before placement or clock-tree construction.
pub(crate) fn insert(
    design: &mut Design,
    bundle: &mut PhysicalBundle,
    limits: &Limits,
) -> Result<()> {
    let Some(physical) = &bundle.config else {
        return Ok(());
    };
    let Some(cfg) = physical.input_buffer.clone() else {
        return Ok(());
    };
    cfg.validate()?;
    if bundle.input_buffers.is_some() || bundle.clock_tree.is_some() || bundle.state.is_some() {
        return Err(invalid(
            "trees must be inserted once before physical preparation",
        ));
    }
    let mut work = Work {
        used: 0,
        limit: cfg.max_work,
    };
    let model = &bundle.connectivity;
    let allowance = cfg.memory_mib << 20;
    let classification = classification_bytes(model, &mut work)?;
    if classification > allowance {
        return Err(budget("master classification memory"));
    }
    let passive = passive_masters(model, &mut work)?;
    let master = model
        .masters
        .iter()
        .find(|m| m.name == cfg.cell)
        .ok_or_else(|| invalid("buffer master is absent"))?;
    let signal: Vec<_> = master.pins.iter().filter(|p| !p.use_.is_supply()).collect();
    if signal.len() != 2
        || !signal
            .iter()
            .any(|p| p.name == cfg.input_pin && p.direction == Some(Direction::Input))
        || !signal
            .iter()
            .any(|p| p.name == cfg.output_pin && p.direction == Some(Direction::Output))
    {
        return Err(invalid("buffer needs one configured input and output"));
    }
    drop(signal);
    let size = master.size;
    let primary_bits = model.ports.iter().try_fold(0usize, |n, p| {
        n.checked_add(if p.direction == Direction::Input && !p.use_.is_supply() {
            p.bits.len()
        } else {
            0
        })
        .filter(|&n| n <= MAX_PRIMARY_BITS)
        .ok_or_else(|| budget("primary interface size"))
    })?;
    if (primary_bits as u64)
        .saturating_mul(128)
        .saturating_add(classification)
        > allowance
    {
        return Err(budget("primary index memory"));
    }
    let mut roots: Vec<Root> = reserve(primary_bits)?;
    let mut by_net: HashMap<u32, usize> = HashMap::new();
    by_net
        .try_reserve(primary_bits)
        .map_err(|_| budget("primary index allocation"))?;
    work.add(model.pins.len())?;
    for pin in &model.pins {
        let (Some(net), PinOwner::Port { port, bit }) = (pin.net, pin.owner) else {
            continue;
        };
        let p = &model.ports[port as usize];
        if p.direction != Direction::Input || p.use_.is_supply() {
            continue;
        }
        if let Some(&index) = by_net.get(&net) {
            roots[index].other_driver = true;
            continue;
        }
        by_net.insert(net, roots.len());
        roots.push(Root {
            net,
            port,
            bit,
            count: 0,
            functional: 0,
            clock: physical.clock.as_ref().is_some_and(|c| c.port == p.name),
            other_driver: false,
            sinks: Vec::new(),
        });
    }
    work.add(model.pins.len())?;
    for pin in &model.pins {
        let (Some(net), PinOwner::Instance { instance, terminal }) = (pin.net, pin.owner) else {
            continue;
        };
        let Some(&index) = by_net.get(&net) else {
            continue;
        };
        let p = &model.masters[model.instance_masters[instance as usize] as usize].pins
            [terminal as usize];
        if p.use_.is_supply() {
            continue;
        }
        let root = &mut roots[index];
        if p.direction == Some(Direction::Input) {
            root.count += 1;
            if !passive[model.instance_masters[instance as usize] as usize] {
                root.functional += 1;
            }
            root.clock |= p.use_ == PinUse::Clock;
        } else {
            root.other_driver = true;
        }
    }
    let skipped_clock_inputs = roots
        .iter()
        .filter(|r| r.clock && r.count > cfg.max_fanout)
        .count();
    roots.retain(|r| r.count > cfg.max_fanout && !r.clock);
    roots.sort_unstable_by_key(|r| r.net);
    if roots.iter().any(|r| r.other_driver) {
        return Err(invalid(
            "selected input net has another driver or bidirectional terminal",
        ));
    }
    let mut sinks = 0usize;
    let mut buffers = 0usize;
    let mut source_bytes = 0u64;
    for root in &roots {
        sinks = sinks
            .checked_add(root.count)
            .filter(|&n| n <= cfg.max_sinks)
            .ok_or_else(|| budget("sink limit"))?;
        let (count, _) = tree_size(root.count, cfg.max_fanout)?;
        buffers = buffers
            .checked_add(count)
            .filter(|&n| n <= cfg.max_buffers)
            .ok_or_else(|| budget("buffer limit"))?;
        source_bytes = source_bytes.saturating_add(
            (model.ports[root.port as usize].name.len() as u64 + 64)
                .saturating_mul(cfg.max_fanout as u64 + 3),
        );
    }
    let remaining = physical
        .max_added_cells
        .saturating_sub(bundle.port_buffers.len())
        .saturating_sub(bundle.ties.as_ref().map_or(0, |t| t.new_cells))
        .min(limits.max_cells.saturating_sub(design.cells.len()));
    if buffers > remaining {
        return Err(budget("generated-cell limit"));
    }
    let source_names = || {
        bundle
            .netlist
            .instances
            .iter()
            .map(|i| i.name.as_str())
            .chain(bundle.netlist.wires.iter().map(|w| w.name.as_str()))
            .chain(bundle.netlist.ports.iter().map(|p| p.name.as_str()))
    };
    work.add(
        bundle.netlist.instances.len() + bundle.netlist.wires.len() + bundle.netlist.ports.len(),
    )?;
    let name_bytes = source_names()
        .filter(|n| n.starts_with("rr_input_"))
        .fold(0u64, |bytes, n| bytes.saturating_add(n.len() as u64 + 128));
    let estimate = (buffers as u64)
        .saturating_mul(4096)
        .saturating_add((sinks as u64).saturating_mul(96))
        .saturating_add((primary_bits as u64).saturating_mul(128))
        .saturating_add(source_bytes)
        .saturating_add(name_bytes)
        .saturating_add(classification)
        .saturating_add(growth(&design.cells, buffers))
        .saturating_add(growth(&bundle.netlist.instances, buffers))
        .saturating_add(growth(&bundle.netlist.wires, buffers));
    if estimate > allowance {
        return Err(budget("incremental memory estimate"));
    }
    work.add(usize::try_from(name_bytes).map_err(|_| budget("name bytes"))?)?;
    let mut names: HashSet<String> = source_names()
        .filter(|n| n.starts_with("rr_input_"))
        .map(str::to_owned)
        .collect();
    by_net.clear();
    for (i, root) in roots.iter_mut().enumerate() {
        root.sinks = reserve(root.count)?;
        by_net.insert(root.net, i);
    }
    work.add(model.pins.len())?;
    for pin in &model.pins {
        let (Some(net), PinOwner::Instance { instance, terminal }) = (pin.net, pin.owner) else {
            continue;
        };
        let Some(&index) = by_net.get(&net) else {
            continue;
        };
        let p = &model.masters[model.instance_masters[instance as usize] as usize].pins
            [terminal as usize];
        if p.direction == Some(Direction::Input) && !p.use_.is_supply() {
            roots[index].sinks.push(Sink { instance, terminal });
        }
    }
    let mut nodes: Vec<Node> = reserve(buffers)?;
    let mut rewrites: Vec<(Sink, usize)> = reserve(sinks)?;
    let mut report = Report {
        trees: reserve(roots.len())?,
        buffers: reserve(buffers)?,
        max_fanout: cfg.max_fanout,
        skipped_clock_inputs,
    };
    let mut sequence = 0;
    for (root_index, root) in roots.iter().enumerate() {
        let first = nodes.len();
        let mut heads = reserve(root.count.div_ceil(cfg.max_fanout))?;
        for chunk in root.sinks.chunks(cfg.max_fanout) {
            work.add(chunk.len() + 1)?;
            let id = nodes.len();
            nodes.push(Node {
                source: Source::Root(root_index),
                name: fresh("rr_input_buffer_", &mut names, &mut sequence, &mut work)?,
                wire: fresh("rr_input_net_", &mut names, &mut sequence, &mut work)?,
            });
            rewrites.extend(chunk.iter().map(|&sink| (sink, id)));
            heads.push(id);
        }
        let mut levels = 1;
        while heads.len() > cfg.max_fanout {
            if levels == MAX_LEVELS {
                return Err(budget("tree depth"));
            }
            let mut upper = reserve(heads.len().div_ceil(cfg.max_fanout))?;
            for chunk in heads.chunks(cfg.max_fanout) {
                work.add(chunk.len() + 1)?;
                let parent = nodes.len();
                nodes.push(Node {
                    source: Source::Root(root_index),
                    name: fresh("rr_input_buffer_", &mut names, &mut sequence, &mut work)?,
                    wire: fresh("rr_input_net_", &mut names, &mut sequence, &mut work)?,
                });
                for &child in chunk {
                    nodes[child].source = Source::Buffer(parent);
                }
                upper.push(parent);
            }
            heads = upper;
            levels += 1;
        }
        report.trees.push(Tree {
            port: model.ports[root.port as usize].name.clone(),
            bit: root.bit,
            sinks: root.functional,
            loads: root.count,
            first_buffer: first,
            buffers: nodes.len() - first,
            levels,
        });
    }
    if nodes.len() != buffers || rewrites.len() != sinks {
        return Err(invalid("tree planning count mismatch"));
    }
    let root_expressions = roots
        .iter()
        .map(|r| expression(bundle, r.port, r.bit))
        .collect::<Result<Vec<_>>>()?;
    design
        .cells
        .try_reserve_exact(buffers)
        .map_err(|_| budget("cell allocation"))?;
    bundle
        .netlist
        .instances
        .try_reserve_exact(buffers)
        .map_err(|_| budget("instance allocation"))?;
    bundle
        .netlist
        .wires
        .try_reserve_exact(buffers)
        .map_err(|_| budget("wire allocation"))?;
    for (sink, driver) in rewrites {
        let pin = &bundle.connectivity.masters
            [bundle.connectivity.instance_masters[sink.instance as usize] as usize]
            .pins[sink.terminal as usize]
            .name;
        let instance = &mut bundle.netlist.instances[sink.instance as usize];
        work.add(instance.connections.len())?;
        let connection = instance
            .connections
            .iter_mut()
            .find(|c| &c.pin == pin)
            .ok_or_else(|| invalid("sink connection is absent from the netlist"))?;
        connection.signal = verilog::Expr::Ident(nodes[driver].wire.clone());
    }
    for node in &nodes {
        work.add(1)?;
        let input = match node.source {
            Source::Root(i) => root_expressions[i].clone(),
            Source::Buffer(i) => verilog::Expr::Ident(nodes[i].wire.clone()),
        };
        let instance =
            u32::try_from(bundle.netlist.instances.len()).map_err(|_| budget("instance index"))?;
        bundle.netlist.instances.push(verilog::Instance {
            cell: cfg.cell.clone(),
            name: node.name.clone(),
            connections: vec![
                verilog::Connection {
                    pin: cfg.input_pin.clone(),
                    signal: input,
                },
                verilog::Connection {
                    pin: cfg.output_pin.clone(),
                    signal: verilog::Expr::Ident(node.wire.clone()),
                },
            ],
        });
        bundle.netlist.wires.push(verilog::Wire {
            name: node.wire.clone(),
            range: None,
        });
        design.cells.push(Cell {
            name: node.name.clone(),
            master: cfg.cell.clone(),
            width: size[0],
            height: size[1],
        });
        report.buffers.push(Buffer {
            instance,
            name: node.name.clone(),
        });
    }
    bundle.input_buffers = Some(report);
    if buffers != 0 {
        crate::physical::relink(bundle)?;
        bundle.diagnostics.push(crate::pdk::Diagnostic {
            code: "INPUT_BUFFER_EQUIVALENCE_UNCHECKED".into(),
            source: "physical_import".into(),
            occurrences: buffers,
            message: "Nonclock input trees use the declared identity-buffer role; independent equivalence verification is required".into(),
        });
    }
    // Import's final bundle validation checks this report against the rebuilt
    // connectivity; avoid an extra full pin-table scan before that check.
    Ok(())
}

/// Validate bounded fanout, root identity, generated instances and acyclic tree membership.
pub(crate) fn validate(bundle: &PhysicalBundle) -> Result<()> {
    let policy = bundle.config.as_ref().and_then(|p| p.input_buffer.as_ref());
    let Some(report) = &bundle.input_buffers else {
        return if policy.is_some() {
            Err(invalid("missing topology report"))
        } else {
            Ok(())
        };
    };
    let cfg = policy.ok_or_else(|| invalid("topology report without policy"))?;
    cfg.validate()?;
    let n = report.buffers.len();
    if n > cfg.max_buffers
        || report.trees.len() > MAX_PRIMARY_BITS
        || report.max_fanout != cfg.max_fanout
    {
        return Err(budget("stored topology count"));
    }
    let mut work = Work {
        used: 0,
        limit: cfg.max_work,
    };
    let classification = classification_bytes(&bundle.connectivity, &mut work)?;
    let primary_bits = bundle.connectivity.ports.iter().try_fold(0usize, |n, p| {
        n.checked_add(if p.direction == Direction::Input && !p.use_.is_supply() {
            p.bits.len()
        } else {
            0
        })
        .filter(|&n| n <= MAX_PRIMARY_BITS)
        .ok_or_else(|| budget("stored primary interface size"))
    })?;
    let scratch = (n as u64)
        .saturating_mul(384)
        .saturating_add((report.trees.len() as u64).saturating_mul(128))
        .saturating_add((bundle.connectivity.ports.len() as u64).saturating_mul(64))
        .saturating_add((primary_bits as u64).saturating_mul(128));
    if bundle.connectivity.ports.len() > MAX_PRIMARY_BITS
        || scratch.saturating_add(classification) > cfg.memory_mib << 20
    {
        return Err(budget("validation memory estimate"));
    }
    let mut ports = HashMap::new();
    ports
        .try_reserve(bundle.connectivity.ports.len())
        .map_err(|_| budget("port index allocation"))?;
    for (i, p) in bundle.connectivity.ports.iter().enumerate() {
        work.add(p.name.len() + 1)?;
        if ports.insert(p.name.as_str(), i).is_some() {
            return Err(invalid("duplicate source port"));
        }
    }
    let mut by_instance = HashMap::new();
    let mut names = HashSet::new();
    by_instance
        .try_reserve(n)
        .map_err(|_| budget("buffer index allocation"))?;
    names
        .try_reserve(n)
        .map_err(|_| budget("buffer name index allocation"))?;
    for (i, b) in report.buffers.iter().enumerate() {
        work.add(1)?;
        let instance = bundle
            .netlist
            .instances
            .get(b.instance as usize)
            .ok_or_else(|| invalid("generated instance index"))?;
        if b.name.len() > 64
            || instance.name != b.name
            || instance.cell != cfg.cell
            || !names.insert(b.name.as_str())
            || by_instance.insert(b.instance, i).is_some()
        {
            return Err(invalid("generated instance identity"));
        }
    }
    let mut wanted_roots = HashMap::new();
    let mut end = 0usize;
    let mut total_sinks = 0usize;
    for (i, tree) in report.trees.iter().enumerate() {
        work.add(1)?;
        let port = ports
            .get(tree.port.as_str())
            .copied()
            .ok_or_else(|| invalid("tree source port missing"))?;
        let p = &bundle.connectivity.ports[port];
        if p.direction != Direction::Input
            || p.use_.is_supply()
            || tree.bit as usize >= p.bits.len()
            || bundle
                .config
                .as_ref()
                .and_then(|p| p.clock.as_ref())
                .is_some_and(|c| c.port == tree.port)
            || wanted_roots.insert((port as u32, tree.bit), i).is_some()
            || tree.first_buffer != end
            || tree.buffers == 0
            || !(1..=MAX_LEVELS).contains(&tree.levels)
            || tree.loads <= cfg.max_fanout
            || tree.sinks > tree.loads
        {
            return Err(invalid("tree source or buffer range"));
        }
        end = end
            .checked_add(tree.buffers)
            .filter(|&x| x <= n)
            .ok_or_else(|| budget("tree buffer range"))?;
        total_sinks = total_sinks
            .checked_add(tree.loads)
            .filter(|&x| x <= cfg.max_sinks)
            .ok_or_else(|| budget("stored sink count"))?;
    }
    if end != n {
        return Err(invalid("unowned generated buffer"));
    }
    let mut inputs = filled(n, None)?;
    let mut outputs = filled(n, None)?;
    let mut roots = HashMap::new();
    let mut primary = HashMap::new();
    primary
        .try_reserve(primary_bits)
        .map_err(|_| budget("primary validation index"))?;
    let model = &bundle.connectivity;
    let passive = passive_masters(model, &mut work)?;
    work.add(model.pins.len())?;
    for p in &model.pins {
        let Some(net) = p.net else { continue };
        match p.owner {
            PinOwner::Port { port, bit } => {
                let p = &model.ports[port as usize];
                if p.direction == Direction::Input && !p.use_.is_supply() {
                    let is_clock = bundle
                        .config
                        .as_ref()
                        .and_then(|p| p.clock.as_ref())
                        .is_some_and(|c| c.port == p.name);
                    let entry = primary.entry(net).or_insert((0usize, false));
                    entry.1 |= is_clock;
                }
                if let Some(&tree) = wanted_roots.get(&(port, bit))
                    && roots.insert(net, tree).is_some()
                {
                    return Err(invalid("aliased source inputs"));
                }
            }
            PinOwner::Instance { instance, terminal } => {
                if let Some(&i) = by_instance.get(&instance) {
                    let pin = &model.masters[model.instance_masters[instance as usize] as usize]
                        .pins[terminal as usize];
                    let slot = if pin.name == cfg.input_pin {
                        &mut inputs[i]
                    } else if pin.name == cfg.output_pin {
                        &mut outputs[i]
                    } else {
                        continue;
                    };
                    if slot.replace(net).is_some() {
                        return Err(invalid("duplicate buffer pin"));
                    }
                }
            }
        }
    }
    if roots.len() != report.trees.len() {
        return Err(invalid("unconnected source input"));
    }
    let mut output_owner = HashMap::new();
    for (i, output) in outputs.into_iter().enumerate() {
        let net = output.ok_or_else(|| invalid("unconnected buffer output"))?;
        if output_owner.insert(net, i).is_some() || roots.contains_key(&net) {
            return Err(invalid("shared buffer output"));
        }
    }
    let mut children = filled(n, Vec::new())?;
    let mut labels = filled(n, None)?;
    let mut depths = filled(n, 0usize)?;
    let mut ready = VecDeque::new();
    ready
        .try_reserve(n)
        .map_err(|_| budget("tree frontier allocation"))?;
    for (i, input) in inputs.into_iter().enumerate() {
        work.add(1)?;
        let net = input.ok_or_else(|| invalid("unconnected buffer input"))?;
        if let Some(&root) = roots.get(&net) {
            labels[i] = Some(root);
            depths[i] = 1;
            ready.push_back(i);
        } else if let Some(&parent) = output_owner.get(&net) {
            children[parent].push(i);
        } else {
            return Err(invalid("buffer input is outside the declared trees"));
        }
    }
    let mut visited = 0;
    while let Some(parent) = ready.pop_front() {
        work.add(1)?;
        visited += 1;
        if visited > n || depths[parent] > MAX_LEVELS {
            return Err(budget("tree traversal"));
        }
        for &child in &children[parent] {
            if labels[child].is_some() {
                return Err(invalid("cyclic or multiply rooted tree"));
            }
            labels[child] = labels[parent];
            depths[child] = depths[parent] + 1;
            ready.push_back(child);
        }
    }
    if visited != n {
        return Err(invalid("cyclic buffer tree"));
    }
    let mut fanout = filled(n, 0usize)?;
    let mut root_fanout = filled(report.trees.len(), 0usize)?;
    let mut leaves = filled(report.trees.len(), 0usize)?;
    work.add(model.pins.len())?;
    for p in &model.pins {
        let Some(net) = p.net else { continue };
        let PinOwner::Instance { instance, terminal } = p.owner else {
            if output_owner.contains_key(&net) {
                return Err(invalid("internal buffer output aliases a port"));
            }
            continue;
        };
        let pin = &model.masters[model.instance_masters[instance as usize] as usize].pins
            [terminal as usize];
        if pin.use_.is_supply() {
            continue;
        }
        if pin.direction == Some(Direction::Input)
            && let Some((count, clock)) = primary.get_mut(&net)
        {
            *count += 1;
            *clock |= pin.use_ == PinUse::Clock;
        }
        if let Some(&owner) = output_owner.get(&net) {
            if pin.direction == Some(Direction::Input) {
                fanout[owner] += 1;
                if !by_instance.contains_key(&instance)
                    && !passive[model.instance_masters[instance as usize] as usize]
                {
                    if pin.use_ == PinUse::Clock {
                        return Err(invalid("input tree moved a clock sink"));
                    }
                    leaves[labels[owner].ok_or_else(|| invalid("missing root label"))?] += 1;
                }
            } else if by_instance.get(&instance) != Some(&owner) {
                return Err(invalid("another driver on a buffer output"));
            }
        } else if let Some(&tree) = roots.get(&net) {
            if pin.direction != Some(Direction::Input)
                || (!by_instance.contains_key(&instance)
                    && !passive[model.instance_masters[instance as usize] as usize])
            {
                return Err(invalid("source has an unbuffered sink or another driver"));
            }
            root_fanout[tree] += 1;
        }
    }
    if fanout
        .iter()
        .chain(&root_fanout)
        .any(|&n| n == 0 || n > cfg.max_fanout)
    {
        return Err(invalid("generated fanout exceeds policy"));
    }
    if primary
        .values()
        .any(|&(count, clock)| !clock && count > cfg.max_fanout)
    {
        return Err(invalid("unbuffered primary input exceeds fanout policy"));
    }
    for (i, tree) in report.trees.iter().enumerate() {
        let end = tree.first_buffer + tree.buffers;
        if leaves[i] != tree.sinks
            || labels[tree.first_buffer..end]
                .iter()
                .any(|&root| root != Some(i))
            || depths[tree.first_buffer..end].iter().max().copied() != Some(tree.levels)
        {
            return Err(invalid("tree coverage or depth mismatch"));
        }
    }
    Ok(())
}
