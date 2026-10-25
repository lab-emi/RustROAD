//! Bounded structural export views; no second full netlist AST is constructed.
use crate::pdk::PhysicalBundle;
use crate::ties::{RAIL_GROUND_WIRE, RAIL_POWER_WIRE};
use crate::{Error, Result};
use rustroad_formats::verilog::{self, Direction, Expr, VerilogLimits};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[cfg(test)]
#[path = "netlist_export_tests.rs"]
mod tests;

/// Whether generated supply ports and physical-only instances are emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupplyMode {
    Logical,
    Powered,
}

/// Object, output, whole-process memory and preparation/emission work limits.
#[derive(Clone, Debug)]
pub struct ExportLimits {
    /// Structural limits; `max_input_bytes` is the maximum emitted byte count.
    pub verilog: VerilogLimits,
    /// Whole-process allowance, including the loaded immutable snapshot.
    pub memory_mib: u64,
    /// Aggregate bounded planning and emission work.
    pub max_work: usize,
}

impl Default for ExportLimits {
    fn default() -> Self {
        Self {
            verilog: VerilogLimits::default(),
            memory_mib: 2048,
            max_work: 200_000_000,
        }
    }
}

fn budget(message: &str) -> Error {
    Error::Budget(format!("Verilog export: {message}"))
}
fn format_error(error: verilog::VerilogError) -> Error {
    if matches!(error, verilog::VerilogError::Budget(_))
        || error.to_string().contains("Verilog export memory budget")
    {
        budget(&error.to_string())
    } else {
        Error::Invalid(error.to_string())
    }
}

fn charge(work: &mut usize, amount: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(amount)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("work limit"))?;
    Ok(())
}

fn rail(name: &str) -> bool {
    name == RAIL_POWER_WIRE || name == RAIL_GROUND_WIRE
}

fn aliases<'a>(
    bundle: &'a PhysicalBundle,
    work: &mut usize,
    cap: usize,
) -> Result<HashMap<&'a str, &'a str>> {
    let netlist = &bundle.netlist;
    charge(
        work,
        netlist.assigns.len().saturating_mul(4) + netlist.ports.len() + netlist.wires.len(),
        cap,
    )?;
    let ports: HashMap<_, _> = netlist
        .ports
        .iter()
        .map(|p| (p.name.as_str(), p.range))
        .collect();
    let mut lhs_count = HashMap::<&str, usize>::new();
    let mut wanted = HashSet::new();
    for a in &netlist.assigns {
        if let Expr::Ident(l) = &a.lhs {
            *lhs_count.entry(l).or_default() += 1;
            wanted.insert(l.as_str());
        }
        if let Expr::Ident(r) = &a.rhs {
            wanted.insert(r.as_str());
        }
    }
    let scalar: HashSet<_> = netlist
        .wires
        .iter()
        .filter(|w| w.range.is_none() && !rail(&w.name) && wanted.contains(w.name.as_str()))
        .map(|w| w.name.as_str())
        .collect();
    let mut nodes = Vec::new();
    let mut index = HashMap::new();
    for a in &netlist.assigns {
        if let (Expr::Ident(l), Expr::Ident(r)) = (&a.lhs, &a.rhs)
            && scalar.contains(l.as_str())
            && !ports.contains_key(l.as_str())
            && lhs_count[l.as_str()] == 1
            && (scalar.contains(r.as_str()) || ports.get(r.as_str()) == Some(&None))
            && l != r
        {
            index.insert(l.as_str(), nodes.len());
            nodes.push((l.as_str(), r.as_str()));
        }
    }
    drop(ports);
    drop(lhs_count);
    drop(wanted);
    drop(scalar);
    // Functional graph: resolve every node once. Cycles and tails into cycles
    // remain as assignments, matching the established powered-netlist contract.
    let mut done = vec![false; nodes.len()];
    let mut visiting = vec![false; nodes.len()];
    let mut resolved = vec![None; nodes.len()];
    let mut path = Vec::new();
    for start in 0..nodes.len() {
        if done[start] {
            continue;
        }
        path.clear();
        let mut current = start;
        let target = loop {
            charge(work, 1, cap)?;
            if done[current] {
                break resolved[current];
            }
            if visiting[current] {
                break None;
            }
            if path.len() >= nodes.len() {
                return Err(budget("alias traversal"));
            }
            visiting[current] = true;
            path.push(current);
            match index.get(nodes[current].1) {
                Some(&next) => current = next,
                None => break Some(nodes[current].1),
            }
        };
        for &i in &path {
            charge(work, 1, cap)?;
            done[i] = true;
            visiting[i] = false;
            resolved[i] = target;
        }
    }
    Ok(nodes
        .into_iter()
        .zip(resolved)
        .filter_map(|((name, _), root)| root.map(|r| (name, r)))
        .collect())
}

fn transformed<'a>(
    expr: &'a Expr,
    folded: &HashMap<&str, &str>,
    supplies: Option<(&str, &str)>,
    bind: bool,
) -> Cow<'a, Expr> {
    let replacement = |name: &str| -> Option<Expr> {
        if bind && rail(name) {
            let power = name == RAIL_POWER_WIRE;
            return Some(match supplies {
                Some((p, g)) => Expr::Ident(if power { p } else { g }.to_owned()),
                None => Expr::Constant(if power { "1'b1" } else { "1'b0" }.into()),
            });
        }
        folded
            .get(name)
            .map(|target| Expr::Ident((*target).to_owned()))
    };
    if let Expr::Ident(name) = expr
        && let Some(mut value) = replacement(name)
    {
        if let Expr::Ident(n) = &mut value
            && let Some(target) = folded.get(n.as_str())
        {
            *n = (*target).to_owned();
        }
        return Cow::Owned(value);
    }
    if let Expr::Concat(parts) = expr
        && parts
            .iter()
            .any(|e| matches!(e, Expr::Ident(n) if folded.contains_key(n.as_str())))
    {
        let mut parts = parts.clone();
        for e in &mut parts {
            if let Expr::Ident(name) = e
                && let Some(target) = folded.get(name.as_str())
            {
                *name = (*target).to_owned();
            }
        }
        return Cow::Owned(Expr::Concat(parts));
    }
    Cow::Borrowed(expr)
}

struct Open<'a> {
    pin: &'a str,
    name: String,
    missing: bool,
}

fn open_pins<'a>(
    bundle: &'a PhysicalBundle,
    index: usize,
    sequence: &mut usize,
    used: &HashSet<&str>,
    work: &mut usize,
    limits: &ExportLimits,
    work_limit: usize,
) -> Result<Vec<Open<'a>>> {
    let instance = &bundle.netlist.instances[index];
    let master = &bundle.connectivity.masters[bundle.connectivity.instance_masters[index] as usize];
    charge(
        work,
        master
            .pins
            .len()
            .saturating_mul(instance.connections.len() + 1),
        work_limit,
    )?;
    let mut opened = Vec::new();
    for pin in master.pins.iter().filter(|p| !p.use_.is_supply()) {
        let connection = instance.connections.iter().find(|c| c.pin == pin.name);
        if connection.is_some_and(|c| !matches!(c.signal, Expr::Unconnected)) {
            continue;
        }
        let name = loop {
            charge(work, 1, work_limit)?;
            let name = format!("rr_open_{sequence}");
            *sequence = sequence
                .checked_add(1)
                .ok_or_else(|| budget("open-pin name sequence"))?;
            if !used.contains(name.as_str()) {
                break name;
            }
        };
        if opened.len() >= limits.verilog.max_connections {
            return Err(budget("open-pin count"));
        }
        opened.push(Open {
            pin: &pin.name,
            name,
            missing: connection.is_none(),
        });
    }
    Ok(opened)
}

struct Guarded<'a, W> {
    writer: &'a mut W,
    bytes: u64,
    next_check: u64,
    memory_bytes: u64,
}
impl<W: Write> Write for Guarded<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes >= self.next_check {
            self.next_check = self.bytes.saturating_add(1 << 20);
            if rustroad_core::memory::peak_rss_bytes().is_some_and(|n| n > self.memory_bytes) {
                return Err(std::io::Error::other("Verilog export memory budget"));
            }
        }
        let n = self.writer.write(bytes)?;
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// Stream a validated snapshot through logical or powered transformations.
pub(crate) fn write(
    bundle: &PhysicalBundle,
    mode: SupplyMode,
    limits: &ExportLimits,
    output: &mut impl Write,
) -> Result<()> {
    let nl = &bundle.netlist;
    let syntax = &limits.verilog;
    if limits.memory_mib == 0
        || limits.memory_mib > 65536
        || limits.max_work == 0
        || limits.max_work > 1usize << 40
        || syntax.max_input_bytes > 16usize << 30
        || syntax.max_instances > crate::storage::MAX_CELLS
        || syntax.max_nets > 64_000_000
        || syntax.max_connections > 256_000_000
        || syntax.max_expr_nodes > 256_000_000
        || syntax.max_depth > 128
    {
        return Err(budget("unsupported limits"));
    }
    let powered = mode == SupplyMode::Powered;
    let tags = if powered {
        0
    } else {
        bundle.generated_fillers.len() + bundle.generated_taps.len()
    };
    if nl.instances.len().saturating_sub(tags) > syntax.max_instances
        || nl.ports.len() > syntax.max_ports
    {
        return Err(budget("output object count"));
    }
    let mut work = 0;
    charge(
        &mut work,
        nl.instances.len()
            + nl.wires.len()
            + nl.ports.len()
            + bundle.connectivity.masters.len()
            + tags,
        limits.max_work,
    )?;
    let cap = limits.memory_mib << 20;
    let held = rustroad_core::memory::live_bytes().unwrap_or_else(|| {
        bundle
            .sources
            .iter()
            .map(|s| s.bytes as u64)
            .sum::<u64>()
            .saturating_mul(16)
    });
    let open_prefix_names = if powered {
        nl.ports
            .iter()
            .map(|p| p.name.as_str())
            .chain(nl.wires.iter().map(|w| w.name.as_str()))
            .filter(|n| n.starts_with("rr_open_"))
            .count()
    } else {
        0
    };
    let maximum_record = nl
        .instances
        .iter()
        .map(|i| i.connections.len())
        .max()
        .unwrap_or(0)
        .max(
            bundle
                .connectivity
                .masters
                .iter()
                .map(|m| m.pins.len())
                .max()
                .unwrap_or(0),
        );
    let planning = (tags as u64)
        .saturating_mul(64)
        .saturating_add((nl.ports.len() as u64).saturating_mul(128))
        .saturating_add((open_prefix_names as u64).saturating_mul(64))
        .saturating_add((nl.assigns.len() as u64).saturating_mul(512))
        .saturating_add(
            (maximum_record as u64).saturating_mul(
                (syntax.max_identifier_bytes as u64)
                    .saturating_mul(4)
                    .saturating_add(256),
            ),
        )
        // An inline revision is capped at64MiB; framed instances at1MiB.
        // Include a conservative8x allowance for one transformed powered record.
        .saturating_add(if powered { 512 << 20 } else { 1 << 20 });
    let writer_state = ((nl.ports.len() + nl.wires.len()) as u64)
        .saturating_mul(256)
        .saturating_add((nl.instances.len() as u64).saturating_mul(96))
        .saturating_add(1 << 20);
    if held.saturating_add(planning).saturating_add(writer_state) > cap {
        return Err(budget("whole-process memory estimate"));
    }
    // Snapshot loading already validated the original AST. The record writer
    // validates the exact emitted view, including generated declarations.
    let folded = if powered {
        aliases(bundle, &mut work, limits.max_work)?
    } else {
        HashMap::new()
    };
    let omitted: HashSet<&str> = if powered {
        HashSet::new()
    } else {
        bundle
            .generated_fillers
            .iter()
            .chain(&bundle.generated_taps)
            .map(String::as_str)
            .collect()
    };
    let used: HashSet<&str> = if powered {
        nl.ports
            .iter()
            .map(|p| p.name.as_str())
            .chain(nl.wires.iter().map(|w| w.name.as_str()))
            .filter(|n| n.starts_with("rr_open_") && !folded.contains_key(*n))
            .collect()
    } else {
        HashSet::new()
    };
    let supplies = bundle
        .config
        .as_ref()
        .filter(|_| powered)
        .map(|c| (c.power_net.as_str(), c.ground_net.as_str()));
    let mut guarded = Guarded {
        writer: output,
        bytes: 0,
        next_check: 0,
        memory_bytes: cap,
    };
    let mut writer = verilog::StreamWriter::new(
        &mut guarded,
        syntax,
        cap.saturating_sub(held).saturating_sub(planning),
        limits.max_work,
    );
    writer.charge_work(work).map_err(format_error)?;
    let ports = nl
        .ports
        .iter()
        .map(|p| (p.name.as_str(), p.direction, p.range))
        .chain(
            supplies
                .into_iter()
                .flat_map(|(p, g)| [(p, Direction::Inout, None), (g, Direction::Inout, None)]),
        );
    writer.module(&nl.module, ports).map_err(format_error)?;
    for wire in &nl.wires {
        if !rail(&wire.name) && !folded.contains_key(wire.name.as_str()) {
            writer
                .wire(Cow::Borrowed(&wire.name), wire.range)
                .map_err(format_error)?;
        }
    }
    if powered {
        let mut sequence = 0;
        for i in 0..nl.instances.len() {
            let before = work;
            let work_limit = work.saturating_add(writer.remaining_work());
            for open in open_pins(
                bundle,
                i,
                &mut sequence,
                &used,
                &mut work,
                limits,
                work_limit,
            )? {
                writer
                    .wire(Cow::Owned(open.name), None)
                    .map_err(format_error)?;
            }
            writer.charge_work(work - before).map_err(format_error)?;
        }
    }
    for assign in &nl.assigns {
        if matches!(&assign.lhs, Expr::Ident(n) if folded.contains_key(n.as_str())) {
            continue;
        }
        let rhs = transformed(&assign.rhs, &folded, supplies, false);
        writer.assign(&assign.lhs, &rhs).map_err(format_error)?;
    }
    let mut sequence = 0;
    for (i, instance) in nl.instances.iter().enumerate() {
        if omitted.contains(instance.name.as_str()) {
            continue;
        }
        let before = work;
        let work_limit = work.saturating_add(writer.remaining_work());
        let opened = if powered {
            open_pins(
                bundle,
                i,
                &mut sequence,
                &used,
                &mut work,
                limits,
                work_limit,
            )?
        } else {
            Vec::new()
        };
        writer.charge_work(work - before).map_err(format_error)?;
        let master = &bundle.connectivity.masters[bundle.connectivity.instance_masters[i] as usize];
        let mut connections = Vec::new();
        for c in &instance.connections {
            let value = if let Some(open) = opened.iter().find(|o| o.pin == c.pin) {
                Cow::Owned(Expr::Ident(open.name.clone()))
            } else {
                transformed(&c.signal, &folded, supplies, true)
            };
            connections.push((c.pin.as_str(), value));
        }
        connections.extend(
            opened
                .iter()
                .filter(|o| o.missing)
                .map(|o| (o.pin, Cow::Owned(Expr::Ident(o.name.clone())))),
        );
        if let Some((power, ground)) = supplies {
            connections.extend(master.pins.iter().filter(|p| p.use_.is_supply()).map(|p| {
                (
                    p.name.as_str(),
                    Cow::Owned(Expr::Ident(
                        if p.use_ == rustroad_core::connectivity::PinUse::Power {
                            power
                        } else {
                            ground
                        }
                        .to_owned(),
                    )),
                )
            }));
        }
        writer
            .instance(&instance.cell, Cow::Borrowed(&instance.name), connections)
            .map_err(format_error)?;
    }
    writer.finish().map_err(format_error)
}
