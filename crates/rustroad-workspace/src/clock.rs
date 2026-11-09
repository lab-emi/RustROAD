//! Bounded level-balanced clock-buffer topology. No routed/skew claim.
//! State and work estimates were recorded in docs/tap-budget.md before this pass.
use crate::pdk::PhysicalBundle;
use crate::{Error, Result, Stage, Summary};
use rustroad_core::connectivity::{Direction, PinOwner};
use rustroad_core::{Cell, Limits, Placed};
use rustroad_formats::verilog;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConfig {
    pub port: String,
    /// Optional already-gated internal clock net. `port` still identifies the
    /// source clock in SDC; buffering this net preserves the gating logic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_net: Option<String>,
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
    /// Order in which sinks, then each level's buffers, are cut into groups of
    /// `max_fanout`. Omission keeps the legacy row-major (y, x) order.
    #[serde(default, skip_serializing_if = "ClockClustering::is_raster")]
    pub clustering: ClockClustering,
    /// Instance-name prefixes of non-inverting buffer trees (`<label>_<depth>_<index>`,
    /// depth 0 drives the original loads) whose same-depth sink sets are
    /// re-partitioned by Hilbert order before CTS. Group sizes are kept.
    /// Requires Hilbert clustering; empty (the default) leaves trees as built.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regroup_prefixes: Vec<String>,
}
/// Sink grouping policy. Row-major order puts a whole placement row into one
/// group, so a leaf net can span the die; the Hilbert order keeps consecutive
/// groups spatially compact at every level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockClustering {
    #[default]
    Raster,
    Hilbert,
}
impl ClockClustering {
    fn is_raster(&self) -> bool {
        *self == ClockClustering::Raster
    }
}
/// Hilbert-curve index of a point on a 2^16 x 2^16 lattice (O(16) integer work).
fn hilbert_index(mut x: u32, mut y: u32) -> u64 {
    const N: u32 = 1 << 16;
    let mut d = 0u64;
    let mut s = N / 2;
    while s > 0 {
        let rx = u32::from(x & s != 0);
        let ry = u32::from(y & s != 0);
        d += u64::from(s) * u64::from(s) * u64::from((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                x = N - 1 - x;
                y = N - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}
/// Hilbert keys for integer DBU points, scaled by the common (square) span.
pub fn hilbert_keys(points: &[[i64; 2]]) -> Vec<u64> {
    let lo = |k: usize| points.iter().map(|p| p[k]).min().unwrap_or(0);
    let (x0, y0) = (lo(0), lo(1));
    let span = points
        .iter()
        .map(|p| (p[0] - x0).max(p[1] - y0))
        .max()
        .unwrap_or(0)
        .max(1) as i128;
    let scale = |v: i64| ((v as i128) * i128::from((1u32 << 16) - 1) / span) as u32;
    points
        .iter()
        .map(|p| hilbert_index(scale(p[0] - x0), scale(p[1] - y0)))
        .collect()
}
/// Upper bound on sink connections a regroup pass may touch.
const MAX_REGROUP_SINKS: usize = 4_000_000;

/// Re-partition the sinks of each named buffer-tree level by Hilbert order of
/// their positions, keeping every buffer's sink count. Every buffer at one
/// depth of a tree carries the same non-inverted signal, so only the physical
/// grouping changes. A level whose nets touch a port, an assignment or a
/// non-scalar connection is left unchanged. A level whose buffers `seed`
/// partitions exactly into the current sinks, at the current group sizes,
/// keeps that partition (an earlier regroup of the same design). Returns the
/// regrouped buffers and those of them that kept the seed partition.
fn regroup_trees(
    bundle: &mut PhysicalBundle,
    positions: &[Placed],
    prefixes: &[String],
    seed: &std::collections::BTreeMap<String, Vec<[String; 2]>>,
) -> Result<(Vec<String>, Vec<String>)> {
    if prefixes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // label -> depth -> buffer instance indices (netlist order).
    let mut trees: std::collections::BTreeMap<&str, std::collections::BTreeMap<u32, Vec<usize>>> =
        std::collections::BTreeMap::new();
    for (i, instance) in bundle.netlist.instances.iter().enumerate() {
        if !prefixes
            .iter()
            .any(|p| instance.name.starts_with(p.as_str()))
        {
            continue;
        }
        let mut parts = instance.name.rsplitn(3, '_');
        let (Some(index), Some(depth), Some(label)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(_), Ok(depth)) = (index.parse::<u32>(), depth.parse::<u32>()) else {
            continue;
        };
        trees
            .entry(label)
            .or_default()
            .entry(depth)
            .or_default()
            .push(i);
    }
    let trees: Vec<Vec<Vec<usize>>> = trees
        .into_values()
        .map(|levels| levels.into_values().collect())
        .collect();
    // Scalar net -> (instance, connection) sinks; ports and assignments pin a net.
    let mut users: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut touched = 0usize;
    for (i, instance) in bundle.netlist.instances.iter().enumerate() {
        for (c, connection) in instance.connections.iter().enumerate() {
            if let verilog::Expr::Ident(net) = &connection.signal {
                touched += 1;
                if touched > MAX_REGROUP_SINKS {
                    return Err(budget("buffer regroup connection count"));
                }
                users.entry(net.clone()).or_default().push((i, c));
            }
        }
    }
    let mut pinned: HashSet<String> = bundle
        .netlist
        .ports
        .iter()
        .map(|p| p.name.clone())
        .collect();
    fn names(e: &verilog::Expr, out: &mut HashSet<String>) {
        match e {
            verilog::Expr::Ident(n) => {
                out.insert(n.clone());
            }
            verilog::Expr::BitSelect { name, .. } | verilog::Expr::PartSelect { name, .. } => {
                out.insert(name.clone());
            }
            verilog::Expr::Concat(items) => items.iter().for_each(|i| names(i, out)),
            _ => {}
        }
    }
    for a in &bundle.netlist.assigns {
        names(&a.lhs, &mut pinned);
        names(&a.rhs, &mut pinned);
    }
    for instance in &bundle.netlist.instances {
        for connection in &instance.connections {
            if !matches!(connection.signal, verilog::Expr::Ident(_)) {
                names(&connection.signal, &mut pinned);
            }
        }
    }
    let output_pin = |bundle: &PhysicalBundle, i: usize| -> Option<String> {
        let m = &bundle.connectivity.masters[bundle.connectivity.instance_masters[i] as usize];
        let mut outputs = m
            .pins
            .iter()
            .filter(|p| p.direction == Some(Direction::Output));
        match (outputs.next(), outputs.next()) {
            (Some(p), None) => Some(p.name.clone()),
            _ => None,
        }
    };
    let centre = |p: &Placed| [p.x + p.width / 2, p.y + p.height / 2];
    let mut moved: HashMap<usize, [i64; 2]> = HashMap::new();
    let (mut regrouped, mut kept) = (Vec::new(), Vec::new());
    for levels in trees {
        for buffers in levels {
            // (net, sinks) per buffer; any unsupported shape skips the level.
            let mut level = Vec::with_capacity(buffers.len());
            for &b in &buffers {
                let Some(pin) = output_pin(bundle, b) else {
                    level.clear();
                    break;
                };
                let Some(verilog::Expr::Ident(net)) = bundle.netlist.instances[b]
                    .connections
                    .iter()
                    .find(|c| c.pin == pin)
                    .map(|c| &c.signal)
                else {
                    level.clear();
                    break;
                };
                if pinned.contains(net) {
                    level.clear();
                    break;
                }
                let sinks: Vec<(usize, usize)> = users[net]
                    .iter()
                    .copied()
                    .filter(|&(i, _)| i != b)
                    .collect();
                level.push((net.clone(), sinks));
            }
            if level.len() != buffers.len() || level.len() < 2 {
                continue;
            }
            let sinks: Vec<(usize, usize)> =
                level.iter().flat_map(|l| l.1.iter().copied()).collect();
            let points: Vec<[i64; 2]> = sinks
                .iter()
                .map(|&(i, _)| {
                    moved
                        .get(&i)
                        .copied()
                        .unwrap_or_else(|| centre(&positions[i]))
                })
                .collect();
            let index: HashMap<(&str, &str), usize> = sinks
                .iter()
                .enumerate()
                .map(|(k, &(i, c))| {
                    let instance = &bundle.netlist.instances[i];
                    (
                        (instance.name.as_str(), instance.connections[c].pin.as_str()),
                        k,
                    )
                })
                .collect();
            let mut used = vec![false; sinks.len()];
            let seeded: Option<Vec<Vec<usize>>> = buffers
                .iter()
                .zip(&level)
                .map(|(&b, (_, group))| {
                    let names = seed.get(&bundle.netlist.instances[b].name)?;
                    if names.len() != group.len() {
                        return None;
                    }
                    names
                        .iter()
                        .map(|[i, p]| {
                            let k = *index.get(&(i.as_str(), p.as_str()))?;
                            (!std::mem::replace(&mut used[k], true)).then_some(k)
                        })
                        .collect()
                })
                .collect();
            drop(index);
            let reused = seeded.is_some();
            let chunks = seeded.unwrap_or_else(|| {
                let keys = hilbert_keys(&points);
                let mut order: Vec<usize> = (0..sinks.len()).collect();
                order.sort_unstable_by_key(|&k| (keys[k], points[k], sinks[k]));
                let mut at = 0usize;
                level
                    .iter()
                    .map(|(_, group)| {
                        at += group.len();
                        order[at - group.len()..at].to_vec()
                    })
                    .collect()
            });
            for ((&b, (net, _)), chunk) in buffers.iter().zip(&level).zip(&chunks) {
                let (mut sx, mut sy) = (0i128, 0i128);
                for &k in chunk {
                    let (i, c) = sinks[k];
                    bundle.netlist.instances[i].connections[c].signal =
                        verilog::Expr::Ident(net.clone());
                    sx += i128::from(points[k][0]);
                    sy += i128::from(points[k][1]);
                }
                let n = chunk.len().max(1) as i128;
                moved.insert(b, [(sx / n) as i64, (sy / n) as i64]);
                regrouped.push(bundle.netlist.instances[b].name.clone());
                if reused {
                    kept.push(bundle.netlist.instances[b].name.clone());
                }
            }
            for (net, _) in &level {
                users.remove(net);
            }
            // Later (upper) levels see the new sink membership.
            for &(i, c) in &sinks {
                if let verilog::Expr::Ident(net) =
                    &bundle.netlist.instances[i].connections[c].signal
                {
                    users.entry(net.clone()).or_default().push((i, c));
                }
            }
        }
    }
    Ok((regrouped, kept))
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
        if self
            .root_net
            .as_ref()
            .is_some_and(|n| !rustroad_core::valid_object_name(n))
        {
            return Err(Error::Invalid("invalid internal clock root name".into()));
        }
        if self.level_cells.len() > 32
            || self
                .level_cells
                .iter()
                .any(|n| !rustroad_core::valid_object_name(n))
        {
            return Err(Error::Invalid("invalid clock level_cells".into()));
        }
        if self.regroup_prefixes.len() > 16
            || self
                .regroup_prefixes
                .iter()
                .any(|p| !rustroad_core::valid_object_name(p))
            || (!self.regroup_prefixes.is_empty() && self.clustering != ClockClustering::Hilbert)
        {
            return Err(Error::Invalid(
                "invalid clock regroup_prefixes (needs Hilbert clustering)".into(),
            ));
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
    /// Buffers of `regroup_prefixes` trees whose sinks were reassigned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regrouped: Vec<String>,
    /// The leaf order came from the placement seed (`clock_sinks`) instead
    /// of the configured clustering.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seed_order: bool,
    /// Regrouped buffers that kept the placement seed's partition (`regroup`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seed_regrouped: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockConstraintBinding {
    pub source: crate::pdk::SourcePin,
    pub clock: rustroad_formats::sdc::ClockConstraints,
}
fn is_zero(n: &usize) -> bool {
    *n == 0
}
fn fail(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
pub(crate) fn root_net(bundle: &PhysicalBundle, cfg: &ClockConfig) -> Result<u32> {
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
    let Some(name) = &cfg.root_net else {
        return Ok(root);
    };
    let internal = bundle
        .connectivity
        .nets
        .iter()
        .position(|n| n.aliases.contains(name))
        .map(|n| n as u32)
        .ok_or_else(|| fail("configured internal clock net absent"))?;
    if internal == root {
        return Ok(root);
    }
    let mut drivers = 0;
    for pin in &bundle.connectivity.pins {
        if pin.net != Some(internal) {
            continue;
        }
        let PinOwner::Instance { instance, terminal } = pin.owner else {
            return Err(fail("internal clock root aliases a top-level port"));
        };
        let master = &bundle.connectivity.masters
            [bundle.connectivity.instance_masters[instance as usize] as usize];
        let terminal = &master.pins[terminal as usize];
        if terminal.direction == Some(Direction::Output) && !terminal.use_.is_supply() {
            drivers += 1;
        }
    }
    if drivers != 1 {
        return Err(fail("internal clock root must have exactly one driver"));
    }
    Ok(internal)
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
        || report.sinks.len() > cfg.max_work
        || report.sinks.len() > cfg.max_buffers.saturating_mul(fanout)
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
    // Only clock-tree nets need receiver lists. Collecting every data-net
    // receiver duplicated the complete design graph during clock validation.
    let mut receivers = HashMap::with_capacity(buffers.len() + 1);
    receivers.insert(root, Vec::new());
    let mut outputs = HashMap::new();
    for pin in &model.pins {
        if let (PinOwner::Instance { instance, terminal }, Some(net)) = (pin.owner, pin.net) {
            let spec = &model.masters[model.instance_masters[instance as usize] as usize].pins
                [terminal as usize];
            if spec.use_.is_supply() {
                continue;
            }
            if spec.direction == Some(Direction::Output)
                && buffers.contains(&(instance as usize))
                && spec.name == cfg.output_pin
            {
                outputs.insert(instance as usize, net);
                if receivers.insert(net, Vec::new()).is_some() {
                    return Err(fail("merged clock-buffer output net"));
                }
            }
        }
    }
    let mut receiver_count = 0usize;
    for pin in &model.pins {
        if let (PinOwner::Instance { instance, terminal }, Some(net)) = (pin.owner, pin.net)
            && let Some(list) = receivers.get_mut(&net)
        {
            let spec = &model.masters[model.instance_masters[instance as usize] as usize].pins
                [terminal as usize];
            if !spec.use_.is_supply() && spec.direction == Some(Direction::Input) {
                receiver_count += 1;
                if receiver_count > cfg.max_work || list.len() >= fanout {
                    return Err(budget("clock receiver work/fanout limit"));
                }
                list.push((instance as usize, terminal as usize));
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
        let list = receivers
            .get(&net)
            .ok_or_else(|| fail("clock output net missing"))?;
        if list.is_empty() || list.len() > fanout {
            return Err(fail("clock fanout exceeded"));
        }
        for &(instance, terminal) in list {
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
    let positions: Vec<Placed> = snapshot.records().collect();
    let (_lock, mut metadata) = snapshot.into_metadata();
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
    let regroup_seed = bundle
        .placement_seed
        .as_ref()
        .map(|s| s.regroup.clone())
        .unwrap_or_default();
    let (regrouped, kept) =
        regroup_trees(bundle, &positions, &cfg.regroup_prefixes, &regroup_seed)?;
    drop(regroup_seed);
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
            if p.direction == Some(Direction::Output) {
                continue;
            }
            if p.name != cfg.sink_pin || p.direction != Some(Direction::Input) {
                return Err(fail("clock root drives an unsupported non-clock pin"));
            }
            sinks.push((
                positions[instance as usize].y,
                positions[instance as usize].x,
                instance as usize,
            ));
            if sinks.len() > cfg.max_work || sinks.len() > cfg.max_buffers.saturating_mul(fanout) {
                return Err(budget("clock sink count"));
            }
        }
    }
    if sinks.is_empty() {
        return Err(fail("clock has no selected sinks"));
    }
    // Chunking follows the sink order, so the leaf order of an earlier tree
    // over the same sinks (placement seed) reproduces its whole topology.
    let seeded: Option<Vec<u64>> = bundle.placement_seed.as_ref().and_then(|seed| {
        if seed.clock_sinks.len() != sinks.len() {
            return None;
        }
        let order: HashMap<&str, u64> = seed
            .clock_sinks
            .iter()
            .zip(0u64..)
            .map(|(name, k)| (name.as_str(), k))
            .collect();
        sinks
            .iter()
            .map(|s| {
                order
                    .get(bundle.netlist.instances[s.2].name.as_str())
                    .copied()
            })
            .collect()
    });
    let seed_order = seeded.is_some();
    let keys = match (seeded, cfg.clustering) {
        (Some(keys), _) => keys,
        (None, ClockClustering::Raster) => vec![0; sinks.len()],
        (None, ClockClustering::Hilbert) => {
            hilbert_keys(&sinks.iter().map(|s| [s.1, s.0]).collect::<Vec<_>>())
        }
    };
    let mut sinks: Vec<_> = keys.into_iter().zip(sinks).collect();
    sinks.sort_unstable_by(|(ka, a), (kb, b)| {
        (ka, a.0, a.1).cmp(&(kb, b.0, b.1)).then_with(|| {
            bundle.netlist.instances[a.2]
                .name
                .cmp(&bundle.netlist.instances[b.2].name)
        })
    });
    let sinks: Vec<_> = sinks.into_iter().map(|(_, s)| s).collect();
    let leaves: Vec<_> = sinks
        .iter()
        .map(|s| ClockSink {
            instance: bundle.netlist.instances[s.2].name.clone(),
            pin: cfg.sink_pin.clone(),
        })
        .collect();
    let mut current: Vec<_> = sinks.into_iter().map(|s| s.2).collect();
    // clear_fillers retains the logical instance order. Remap the sorted sink
    // indices even when a valid loaded revision interleaves generated cells.
    let removed: Vec<usize> = {
        let generated: HashSet<&str> = bundle
            .generated_fillers
            .iter()
            .chain(&bundle.generated_taps)
            .map(String::as_str)
            .collect();
        bundle
            .netlist
            .instances
            .iter()
            .enumerate()
            .filter_map(|(i, instance)| generated.contains(instance.name.as_str()).then_some(i))
            .collect()
    };
    for index in &mut current {
        *index -= removed.partition_point(|removed| removed < index);
    }
    drop(removed);
    crate::physical::clear_fillers(&mut metadata.design, bundle)?;
    let mut names: HashSet<String> = bundle
        .netlist
        .instances
        .iter()
        .map(|i| i.name.as_str())
        .chain(bundle.netlist.wires.iter().map(|w| w.name.as_str()))
        .chain(bundle.netlist.ports.iter().map(|p| p.name.as_str()))
        .filter(|n| n.starts_with("rr_clkbuf_") || n.starts_with("rr_clock_"))
        .map(str::to_owned)
        .collect();
    let mut buffers = Vec::new();
    let available_buffers = bundle
        .config
        .as_ref()
        .unwrap()
        .max_added_cells
        .saturating_sub(bundle.port_buffers.len())
        .saturating_sub(bundle.input_buffers.as_ref().map_or(0, |r| r.buffers.len()))
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
            let pin = if levels == 1 {
                &cfg.sink_pin
            } else {
                &cfg.input_pin
            };
            for instance in group {
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
                        signal: verilog::Expr::Ident(
                            cfg.root_net.as_ref().unwrap_or(&cfg.port).clone(),
                        ),
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
                fixed: None,
                name: name.clone(),
                master: cell.clone(),
                width: size[0],
                height: size[1],
            });
            buffers.push(name);
            next.push(i);
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
        regrouped,
        seed_order,
        seed_regrouped: kept.len(),
    });
    if cfg.clustering == ClockClustering::Hilbert
        && let Some(seed) = bundle.placement_seed.as_mut()
    {
        // A seed recorded for an earlier tree names different groups; new
        // buffers start from their own pins' centroid instead. A reused leaf
        // order rebuilds the same groups under the same names.
        let tree = bundle.clock_tree.as_ref().unwrap();
        let rebuilt = if seed_order {
            &[][..]
        } else {
            &tree.buffers[..]
        };
        let kept: HashSet<&String> = kept.iter().collect();
        for name in rebuilt
            .iter()
            .chain(tree.regrouped.iter().filter(|n| !kept.contains(n)))
        {
            seed.cells.remove(name);
        }
    }
    crate::physical::relink(bundle)?;
    validate_tree(bundle)?;
    let initial = rustroad_core::place_design_cpu_ref(&metadata.design, limits)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hilbert_prefix_fills_a_square_with_unit_steps() {
        // The first 4^6 curve cells are exactly the 64x64 corner square and
        // consecutive cells are lattice neighbours (16 KiB of state).
        let mut at = vec![None; 64 * 64];
        for x in 0..64u32 {
            for y in 0..64u32 {
                let d = hilbert_index(x, y) as usize;
                assert!(d < at.len() && at[d].is_none());
                at[d] = Some((x, y));
            }
        }
        for w in at.windows(2) {
            let (a, b) = (w[0].unwrap(), w[1].unwrap());
            assert_eq!(a.0.abs_diff(b.0) + a.1.abs_diff(b.1), 1);
        }
        assert_eq!(hilbert_index(0, 0), 0);
        assert!(hilbert_index(u16::MAX as u32, 0) > hilbert_index(0, u16::MAX as u32));
    }

    #[test]
    fn hilbert_groups_are_compact_where_raster_groups_span_rows() {
        // 8 rows x 32 columns of sinks, groups of 8: a raster group is a
        // whole-width row segment, a Hilbert group stays near-square.
        let points: Vec<[i64; 2]> = (0..8)
            .flat_map(|y| (0..32).map(move |x| [x * 10_000, y * 3_360]))
            .collect();
        let half_perimeter = |order: &[usize]| {
            order
                .chunks(8)
                .map(|g| {
                    let xs = g.iter().map(|&i| points[i][0]);
                    let ys = g.iter().map(|&i| points[i][1]);
                    (xs.clone().max().unwrap() - xs.min().unwrap())
                        + (ys.clone().max().unwrap() - ys.min().unwrap())
                })
                .max()
                .unwrap()
        };
        let mut raster: Vec<usize> = (0..points.len()).collect();
        raster.sort_by_key(|&i| (points[i][1], points[i][0]));
        let keys = hilbert_keys(&points);
        let mut hilbert: Vec<usize> = (0..points.len()).collect();
        hilbert.sort_by_key(|&i| (keys[i], points[i][1], points[i][0]));
        assert_eq!(half_perimeter(&raster), 70_000);
        assert!(
            half_perimeter(&hilbert) <= 40_000,
            "{}",
            half_perimeter(&hilbert)
        );
        // Degenerate inputs stay defined: one point and coincident points.
        assert_eq!(hilbert_keys(&[[5, 5]]), vec![0]);
        assert_eq!(hilbert_keys(&[[7, 7], [7, 7]]), vec![0, 0]);
        let config: ClockConfig = serde_json::from_value(serde_json::json!({
            "port": "clk", "cell": "BUF", "input_pin": "A", "output_pin": "Y",
            "sink_pin": "CK", "max_fanout": 8, "max_buffers": 8, "max_work": 64
        }))
        .unwrap();
        assert_eq!(config.clustering, ClockClustering::Raster);
        assert!(
            serde_json::to_value(&config)
                .unwrap()
                .get("clustering")
                .is_none()
        );
    }
}
