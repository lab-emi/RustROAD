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
    /// A logic cell used as the constant source (e.g. sky130 `inv_1` with
    /// `A` on ground for a logic 1) instead of a dedicated tie master: its
    /// one input is joined to the cell's own `supply` rail by the track
    /// router. Absent: `cell` has only outputs and supply pins (`conb_1`,
    /// `tieh`/`tiel`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rail_input: Option<TieRailInput>,
}
/// A dedicated constant cell the input netlist may contain (e.g. sky130
/// `conb_1` from synthesis): the import dissolves its instances into
/// literals, which the `tie_high`/`tie_low` policy then implements.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConstantCell {
    pub cell: String,
    #[serde(default)]
    pub high: Vec<String>,
    #[serde(default)]
    pub low: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TieRailInput {
    pub pin: String,
    pub supply: RailPolarity,
}
impl TieCell {
    /// Rail input pin and whether it is tied to power.
    pub fn rail(&self) -> Option<(&str, bool)> {
        self.rail_input
            .as_ref()
            .map(|r| (r.pin.as_str(), r.supply == RailPolarity::Power))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapConfig {
    pub cell: String,
    /// Maximum clear horizontal interval between reserved tap-cell edges.
    pub max_gap: i64,
    pub well_layer: String,
    pub substrate_layer: String,
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
    /// Constant cells of the input netlist to rebuild with the tie policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constant_cells: Vec<ConstantCell>,
    pub filler_cells: Vec<String>,
    pub alternating_rows: bool,
    #[serde(default)]
    pub cell_spacing_sites: u32,
    #[serde(default)]
    pub io_width: Option<i64>,
    /// Grow square signal I/O pins to at least the I/O layer LEF AREA.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub io_min_area: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap: Option<TapConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<crate::clock::ClockConfig>,
    /// Analytic placer (`rustroad_core::place`); absent keeps fixed-order
    /// packing, the deterministic baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placer: Option<rustroad_core::place::PlaceConfig>,
    /// Explicit I/O assignment (connectivity-aware order from `floorplan`);
    /// absent keeps round-robin side assignment in port order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_plan: Option<rustroad_core::io::IoPlan>,
}
/// Schema of a net-weight file (`place --net-weights`).
pub const NET_WEIGHTS_SCHEMA: &str = "rustroad.net-weights.v1";
/// Largest number of weighted nets in one file.
pub const MAX_WEIGHTED_NETS: usize = 1_000_000;
/// Timing-driven placement weights: net name -> weight in milli-units
/// (1000 = 1.0, at most `rustroad_core::place::MAX_NET_WEIGHT`). Nets not
/// listed keep 1000.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetWeights {
    pub schema: String,
    /// Free-form provenance (the STA revision and rule that produced them).
    #[serde(default)]
    pub source: serde_json::Value,
    pub weights: std::collections::BTreeMap<String, u32>,
}
impl NetWeights {
    pub fn validate(&self) -> Result<()> {
        if self.schema != NET_WEIGHTS_SCHEMA {
            return Err(Error::Invalid(format!(
                "net weights schema {} is not {NET_WEIGHTS_SCHEMA}",
                self.schema
            )));
        }
        if self.weights.len() > MAX_WEIGHTED_NETS {
            return Err(Error::Budget("net weights: too many nets".into()));
        }
        if let Some((n, w)) = self
            .weights
            .iter()
            .find(|(_, w)| **w == 0 || **w > rustroad_core::place::MAX_NET_WEIGHT)
        {
            return Err(Error::Invalid(format!(
                "net weight {w} of {n} is outside 1..={}",
                rustroad_core::place::MAX_NET_WEIGHT
            )));
        }
        Ok(())
    }
}
/// Schema of a placement seed file (`place --seed`).
pub const PLACEMENT_SEED_SCHEMA: &str = "rustroad.placement-seed.v1";
/// ECO placement seed: instance name -> lower-left (DBU), typically the
/// previous layout of the same design. Seeded cells are legalized from their
/// seed; new cells start at their net neighbours (no global placement).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSeed {
    pub schema: String,
    #[serde(default)]
    pub source: serde_json::Value,
    pub cells: std::collections::BTreeMap<String, [i64; 2]>,
}
impl PlacementSeed {
    pub fn validate(&self) -> Result<()> {
        if self.schema != PLACEMENT_SEED_SCHEMA {
            return Err(Error::Invalid(format!(
                "placement seed schema {} is not {PLACEMENT_SEED_SCHEMA}",
                self.schema
            )));
        }
        if self.cells.len() > MAX_WEIGHTED_NETS {
            return Err(Error::Budget("placement seed: too many cells".into()));
        }
        Ok(())
    }
}
/// What the seeded placer did.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeedUse {
    /// Cells placed from their seed.
    pub seeded: usize,
    /// Cells without a seed (new ECO cells, renamed clock buffers).
    pub unseeded: usize,
    /// Seed entries no current cell has (removed or renamed cells).
    pub unmatched: usize,
    /// Legalization displacement of the seeded cells (Manhattan, DBU).
    pub max_displacement_dbu: i64,
    pub total_displacement_dbu: i64,
}
/// Wirelength record of the last placement (centre-to-centre HPWL, DBU).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementReport {
    pub mode: String,
    pub hpwl_dbu: u64,
    /// Fixed-order packing of the same cells into the same intervals.
    pub baseline_hpwl_dbu: u64,
    pub global_hpwl_dbu: u64,
    pub work: u64,
    pub status: String,
    /// Timing-driven weights applied (absent: unweighted placement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_weights: Option<WeightUse>,
    /// Seeded ECO placement (absent: global placement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SeedUse>,
}
/// What the placer did with `NetWeights`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightUse {
    /// Listed nets found in the placement model and weighted.
    pub applied: usize,
    /// Listed nets the model does not have (supply, unconnected or renamed
    /// nets); reported, never guessed.
    pub unmatched: usize,
    pub max_weight: u32,
    /// Weighted HPWL / 1000 of the result (same nets as `hpwl_dbu`).
    pub weighted_hpwl_dbu: u64,
}
impl PhysicalConfig {
    pub fn validate(&self) -> Result<()> {
        if self.cell_spacing_sites > 64 || (self.cell_spacing_sites > 0 && self.tap.is_none()) {
            return Err(Error::Invalid(
                "cell spacing needs tap-reserved rows and <=64 sites".into(),
            ));
        }
        if let Some(c) = &self.clock {
            c.validate()?;
        }
        if let Some(t) = &self.tap
            && (t.max_gap < 0
                || t.well_layer == t.substrate_layer
                || [&t.cell, &t.well_layer, &t.substrate_layer]
                    .iter()
                    .any(|n| !rustroad_core::valid_object_name(n)))
        {
            return Err(Error::Invalid("invalid tap policy".into()));
        }
        if self.max_added_cells > 250_000
            || self.constant_cells.len() > 16
            || self.constant_cells.iter().any(|c| {
                c.high.len() + c.low.len() == 0
                    || c.high.len() + c.low.len() > 64
                    || c.cell == self.tie_high.cell
                    || c.cell == self.tie_low.cell
            })
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
        .chain(
            [&self.tie_high, &self.tie_low]
                .into_iter()
                .filter_map(|t| t.rail_input.as_ref().map(|r| &r.pin)),
        )
        .chain(
            self.constant_cells
                .iter()
                .flat_map(|c| std::iter::once(&c.cell).chain(&c.high).chain(&c.low)),
        )
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
        // One master may serve both values only through rail inputs of
        // opposite supplies (same input pin, different rail).
        if self.tie_high.cell == self.tie_low.cell
            && self.tie_high.pin == self.tie_low.pin
            && !matches!(
                (&self.tie_high.rail_input, &self.tie_low.rail_input),
                (Some(h), Some(l)) if h.pin == l.pin && h.supply != l.supply
            )
        {
            return Err(Error::Invalid(
                "high/low tie outputs must be distinct".into(),
            ));
        }
        for t in [&self.tie_high, &self.tie_low] {
            if t.rail_input.as_ref().is_some_and(|r| r.pin == t.pin) {
                return Err(Error::Invalid(
                    "tie rail input and output must be different pins".into(),
                ));
            }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tap: Option<rustroad_core::tap::TapReport>,
}

pub(super) fn validate_tap_master(bundle: &PhysicalBundle) -> Result<()> {
    let Some(cfg) = bundle.config.as_ref() else {
        return Ok(());
    };
    let Some(tap) = &cfg.tap else {
        if !bundle.generated_taps.is_empty() {
            return Err(Error::Invalid("tap instances without policy".into()));
        }
        return Ok(());
    };
    let m = bundle
        .connectivity
        .masters
        .iter()
        .find(|m| m.name == tap.cell)
        .ok_or_else(|| Error::Invalid("tap master absent".into()))?;
    if !m.class.as_deref().is_some_and(|c| {
        c.split_whitespace()
            .any(|s| s.eq_ignore_ascii_case("WELLTAP"))
    }) || m.pins.iter().any(|p| !p.use_.is_supply())
    {
        return Err(Error::Invalid(
            "tap master must be a signal-free LEF WELLTAP".into(),
        ));
    }
    for (name, usage, body) in [
        (&cfg.power_pin, PinUse::Power, &tap.well_layer),
        (&cfg.ground_pin, PinUse::Ground, &tap.substrate_layer),
    ] {
        let p = m
            .pins
            .iter()
            .find(|p| &p.name == name && p.use_ == usage)
            .ok_or_else(|| Error::Invalid("tap primary pin missing or wrong use".into()))?;
        let has = |name: &str| {
            p.ports.iter().flatten().any(|g| match g {
                rustroad_core::connectivity::Geometry::Rect { layer, .. }
                | rustroad_core::connectivity::Geometry::Polygon { layer, .. } => layer == name,
            })
        };
        if !has(body) || !has(&cfg.rail_layer) {
            return Err(Error::Invalid(
                "tap must expose both body and primary rail geometry on its supply pin".into(),
            ));
        }
    }
    Ok(())
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
    for pin in pins.iter_mut() {
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
    fold_tie_rails(masters, instance_masters, nets, pins, first)
}

/// The reserved rail-input tie wires (`ties::RAIL_*_WIRE`) are the supply
/// nets: their pins (tie-cell inputs only) move to the power/ground net and
/// the wire nets are removed (net indices above them shift down).
fn fold_tie_rails(
    masters: &[rustroad_core::connectivity::Master],
    instance_masters: &[u32],
    nets: &mut Vec<Net>,
    pins: &mut [Connection],
    first: u32,
) -> Result<()> {
    let mut fold: Vec<(u32, u32)> = Vec::new();
    for (wire, target) in [
        (crate::ties::RAIL_POWER_WIRE, first),
        (crate::ties::RAIL_GROUND_WIRE, first + 1),
    ] {
        if let Some(i) = nets
            .iter()
            .position(|n| n.aliases.iter().any(|a| a == wire))
        {
            if nets[i].aliases.len() != 1 || i as u32 >= first {
                return Err(Error::Invalid(format!("tie rail wire {wire} is aliased")));
            }
            fold.push((i as u32, target));
        }
    }
    if fold.is_empty() {
        return Ok(());
    }
    for pin in pins.iter_mut() {
        let Some(n) = pin.net else { continue };
        let Some(&(_, target)) = fold.iter().find(|(r, _)| *r == n) else {
            continue;
        };
        let input = match pin.owner {
            PinOwner::Instance { instance, terminal } => {
                let spec =
                    &masters[instance_masters[instance as usize] as usize].pins[terminal as usize];
                !spec.use_.is_supply() && spec.direction == Some(Direction::Input)
            }
            PinOwner::Port { .. } => false,
        };
        if !input {
            return Err(Error::Invalid(
                "tie rail wire drives something other than a cell input".into(),
            ));
        }
        pin.net = Some(target);
    }
    let mut removed: Vec<u32> = fold.iter().map(|(r, _)| *r).collect();
    removed.sort_unstable();
    let mut index = 0u32;
    nets.retain(|_| {
        let keep = removed.binary_search(&index).is_err();
        index += 1;
        keep
    });
    for pin in pins.iter_mut() {
        if let Some(n) = pin.net {
            pin.net = Some(n - removed.partition_point(|&r| r < n) as u32);
        }
    }
    Ok(())
}

/// Instance signal pins tied to a supply net (rail-input tie cells), in
/// connectivity pin order: (pin index, supply net). The track router joins
/// each to the instance's own rail; exports report them on the supply net.
pub(crate) fn rail_tie_pins(bundle: &PhysicalBundle) -> Vec<(u32, u32)> {
    let model = &bundle.connectivity;
    model
        .pins
        .iter()
        .enumerate()
        .filter_map(|(i, p)| {
            let net = p.net?;
            let PinOwner::Instance { instance, terminal } = p.owner else {
                return None;
            };
            let spec = &model.masters[model.instance_masters[instance as usize] as usize].pins
                [terminal as usize];
            (model.nets[net as usize].use_.is_supply() && !spec.use_.is_supply())
                .then_some((i as u32, net))
        })
        .collect()
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
        if let Some((rail_pin, power)) = spec.rail() {
            let terminal = *terminal_names[model.instance_masters[i] as usize]
                .get(rail_pin)
                .ok_or_else(|| Error::Invalid("tie rail input absent".into()))?;
            let net = pin_nets
                .get(&PinOwner::Instance {
                    instance: i as u32,
                    terminal: terminal as u32,
                })
                .copied()
                .flatten()
                .ok_or_else(|| Error::Invalid("tie rail input unconnected".into()))?;
            let want = if power {
                &cfg.power_net
            } else {
                &cfg.ground_net
            };
            if &model.nets[net as usize].name != want {
                return Err(Error::Invalid("tie rail input on the wrong supply".into()));
            }
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
    bundle.routing = None;
    if bundle.generated_fillers.is_empty() && bundle.generated_taps.is_empty() {
        bundle.state = None;
        return Ok(());
    }
    let names: HashSet<_> = bundle
        .generated_fillers
        .iter()
        .chain(bundle.generated_taps.iter())
        .map(String::as_str)
        .collect();
    design.cells.retain(|c| !names.contains(c.name.as_str()));
    bundle
        .netlist
        .instances
        .retain(|c| !names.contains(c.name.as_str()));
    bundle.generated_fillers.clear();
    bundle.generated_taps.clear();
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
    validate_tap_master(bundle)?;
    let mut tap_work = 0usize;
    let gap = i64::from(cfg.cell_spacing_sites)
        .checked_mul(design.site.width)
        .ok_or_else(|| Error::Budget("cell spacing overflow".into()))?;
    bundle.placement = if cfg.placer.is_some() {
        Some(analytic(design, bundle, &mut placed, gap, limits)?)
    } else {
        None
    };
    if let Some(tap) = &cfg.tap {
        let m = bundle
            .connectivity
            .masters
            .iter()
            .find(|m| m.name == tap.cell)
            .unwrap();
        let width = m.size[0];
        let remaining = cfg
            .max_added_cells
            .saturating_sub(bundle.ties.as_ref().map_or(0, |t| t.new_cells))
            .saturating_sub(bundle.clock_tree.as_ref().map_or(0, |t| t.buffers.len()))
            .min(limits.max_cells.saturating_sub(design.cells.len()));
        let plan = rustroad_core::tap::place_with_spacing(
            design,
            width,
            tap.max_gap,
            gap,
            &rustroad_core::tap::TapLimits {
                max_taps: remaining,
                max_work: limits.max_expansions,
            },
        )?;
        tap_work = plan.report.work_units;
        if cfg.placer.is_none() {
            for (p, xy) in placed.iter_mut().zip(plan.cells) {
                p.x = xy[0];
                p.y = xy[1];
            }
        }
        let mut used: HashSet<_> = design.cells.iter().map(|c| c.name.clone()).collect();
        let mut next = 0usize;
        for slot in plan.report.sites {
            let name = loop {
                if next > limits.max_cells.saturating_add(used.len()) {
                    return Err(Error::Budget("tap name search exceeded".into()));
                }
                let n = format!("rr_tap_{next}");
                next += 1;
                if used.insert(n.clone()) {
                    break n;
                }
            };
            design.cells.push(Cell {
                name: name.clone(),
                master: tap.cell.clone(),
                width,
                height: design.site.height,
            });
            bundle.netlist.instances.push(verilog::Instance {
                name: name.clone(),
                cell: tap.cell.clone(),
                connections: Vec::new(),
            });
            bundle.generated_taps.push(name);
            placed.push(Placed {
                id: 0,
                x: slot.x,
                y: slot.y,
                width,
                height: design.site.height,
            });
        }
    }
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
                        + bundle.generated_taps.len()
                        + bundle.clock_tree.as_ref().map_or(0, |t| t.buffers.len())
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
    let expansions = bundle
        .generated_fillers
        .len()
        .checked_add(design.rows.len())
        .and_then(|v| v.checked_add(tap_work))
        .ok_or_else(|| Error::Budget("physical work overflow".into()))?;
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
    validate_tap_master(bundle)?;
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
    let io_pins = io_pins(design, bundle)?;
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
    let tap_report = if let Some(tap) = &cfg.tap {
        let m = bundle
            .connectivity
            .masters
            .iter()
            .find(|m| m.name == tap.cell)
            .unwrap();
        let report = rustroad_core::tap::reserve(
            design,
            m.size[0],
            tap.max_gap,
            &rustroad_core::tap::TapLimits::default(),
        )?;
        let indices: HashMap<_, _> = design
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.as_str(), i))
            .collect();
        let mut actual = Vec::with_capacity(bundle.generated_taps.len());
        for name in &bundle.generated_taps {
            let i = *indices
                .get(name.as_str())
                .ok_or_else(|| Error::Invalid("tap instance missing".into()))?;
            if design.cells[i].master != tap.cell {
                return Err(Error::Invalid("tap master mismatch".into()));
            }
            actual.push((placed[i].y, placed[i].x));
        }
        actual.sort_unstable();
        let mut expected: Vec<_> = report.sites.iter().map(|s| (s.y, s.x)).collect();
        expected.sort_unstable();
        if actual != expected {
            return Err(Error::Invalid(
                "tap placement/coverage differs from reserved policy".into(),
            ));
        }
        Some(report)
    } else {
        None
    };
    Ok(PhysicalState {
        io_pins,
        rails,
        supply,
        fillers_added: bundle.generated_fillers.len(),
        tap: tap_report,
    })
}

/// Signal I/O pin geometry (round-robin, or the explicit `io_plan`).
pub(crate) fn io_pins(design: &Design, bundle: &PhysicalBundle) -> Result<Vec<IoPin>> {
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| Error::Invalid("physical config absent".into()))?;
    let grid = bundle.technology.manufacturing_grid.ok_or_else(|| {
        Error::Invalid("manufacturing grid required for physical preparation".into())
    })?;
    let io_layer = layer(&bundle.technology, &cfg.io_layer)?;
    // Smallest grid-aligned square side whose area meets the LEF AREA rule.
    let area_side = match (cfg.io_min_area, io_layer.min_area) {
        (true, Some(area)) => {
            let area =
                u128::try_from(area).map_err(|_| Error::Invalid("I/O layer area rule".into()))?;
            let mut side = area.isqrt();
            if side * side < area {
                side += 1;
            }
            let g = u128::try_from(grid).map_err(|_| Error::Invalid("grid".into()))?;
            i64::try_from(side.div_ceil(g) * g)
                .map_err(|_| Error::Invalid("I/O area side overflow".into()))?
        }
        (true, None) => {
            return Err(Error::Invalid(
                "io_min_area requires a LEF AREA rule on the I/O layer".into(),
            ));
        }
        _ => 0,
    };
    let io_constraints = IoConstraints {
        die: design.die_area,
        core: design.core_area,
        grid,
        layer: cfg.io_layer.clone(),
        min_width: cfg
            .io_width
            .or(io_layer.width)
            .ok_or_else(|| Error::Invalid("I/O minimum width missing".into()))?
            .max(io_layer.width.unwrap_or(0))
            .max(area_side),
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
    match &cfg.io_plan {
        Some(plan) => {
            rustroad_core::io::place_planned_io(&names, plan, &io_constraints, &IoLimits::default())
        }
        None => place_boundary_io(&names, &io_constraints, &IoLimits::default()),
    }
    .map_err(|e| Error::Invalid(e.to_string()))
}

/// Free row intervals between reserved taps (whole rows without taps).
pub(crate) fn free_intervals(
    design: &Design,
    bundle: &PhysicalBundle,
) -> Result<Vec<rustroad_core::place::Interval>> {
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| Error::Invalid("physical config absent".into()))?;
    let mut out = Vec::new();
    if let Some(tap) = &cfg.tap {
        let m = bundle
            .connectivity
            .masters
            .iter()
            .find(|m| m.name == tap.cell)
            .ok_or_else(|| Error::Invalid("tap master absent".into()))?;
        let report = rustroad_core::tap::reserve(
            design,
            m.size[0],
            tap.max_gap,
            &rustroad_core::tap::TapLimits::default(),
        )?;
        for pair in report.sites.windows(2) {
            if pair[0].row == pair[1].row {
                let x = pair[0].x + pair[0].width;
                if x < pair[1].x {
                    out.push(rustroad_core::place::Interval {
                        y: pair[0].y,
                        lx: x,
                        ux: pair[1].x,
                    });
                }
            }
        }
    } else {
        for r in &design.rows {
            out.push(rustroad_core::place::Interval {
                y: r.y,
                lx: r.x_start,
                ux: r.x_end,
            });
        }
    }
    out.sort_unstable_by_key(|v| (v.y, v.lx));
    Ok(out)
}

/// Analytic placement of every current instance (logic, ties and clock
/// buffers; taps and fillers are added afterwards) plus the fixed-order
/// baseline on the same intervals for the HPWL report.
fn analytic(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &mut [Placed],
    gap: i64,
    limits: &Limits,
) -> Result<PlacementReport> {
    let cfg = bundle.config.as_ref().unwrap();
    let pc = cfg.placer.as_ref().unwrap();
    let model = &bundle.connectivity;
    let io = io_pins(design, bundle)?;
    let centre: HashMap<&str, [i64; 2]> = io
        .iter()
        .map(|p| {
            (
                p.name.as_str(),
                [(p.rect.lx + p.rect.ux) / 2, (p.rect.ly + p.rect.uy) / 2],
            )
        })
        .collect();
    let mut nets: Vec<Vec<rustroad_core::place::PinRef>> = vec![Vec::new(); model.nets.len()];
    // Timing-driven weights by net name (aliases too); unlisted nets 1000.
    let mut weights: Vec<u32> = Vec::new();
    let mut weight_use = None;
    if let Some(nw) = &bundle.net_weights {
        nw.validate()?;
        let mut applied = 0usize;
        weights = model
            .nets
            .iter()
            .map(|n| {
                let w = std::iter::once(&n.name)
                    .chain(&n.aliases)
                    .find_map(|name| nw.weights.get(name).copied());
                if w.is_some() {
                    applied += 1;
                }
                w.unwrap_or(1000)
            })
            .collect();
        weight_use = Some(WeightUse {
            applied,
            unmatched: nw.weights.len().saturating_sub(applied),
            max_weight: weights.iter().copied().max().unwrap_or(1000),
            weighted_hpwl_dbu: 0,
        });
    }
    for pin in &model.pins {
        let Some(n) = pin.net else { continue };
        if model.nets[n as usize].use_.is_supply() {
            continue;
        }
        match pin.owner {
            PinOwner::Instance { instance, terminal } => {
                let m = &model.masters[model.instance_masters[instance as usize] as usize];
                if m.pins[terminal as usize].use_.is_supply() {
                    continue;
                }
                if (instance as usize) < placed.len() {
                    nets[n as usize].push(rustroad_core::place::PinRef::Cell(instance));
                }
            }
            PinOwner::Port { port, bit } => {
                let name = &model.ports[port as usize].bits[bit as usize];
                if let Some(c) = centre.get(name.as_str()) {
                    nets[n as usize].push(rustroad_core::place::PinRef::Fixed(*c));
                }
            }
        }
    }
    if !weights.is_empty() {
        weights = weights
            .into_iter()
            .zip(&nets)
            .filter(|(_, n)| n.len() >= 2)
            .map(|(w, _)| w)
            .collect();
    }
    nets.retain(|n| n.len() >= 2);
    let problem = rustroad_core::place::PlaceProblem {
        weights,
        widths: placed.iter().map(|p| p.width).collect(),
        row_height: design.site.height,
        site: design.site.width,
        gap,
        nets,
        intervals: free_intervals(design, bundle)?,
    };
    let place_limits = rustroad_core::place::PlaceLimits {
        max_cells: limits.max_cells,
        ..Default::default()
    };
    let mut seed_use = None;
    let mut seeds = Vec::new();
    let result = match &bundle.placement_seed {
        Some(seed) => {
            seed.validate()?;
            seeds = design
                .cells
                .iter()
                .take(placed.len())
                .map(|c| seed.cells.get(&c.name).copied())
                .collect::<Vec<_>>();
            let seeded = seeds.iter().filter(|s| s.is_some()).count();
            seed_use = Some(SeedUse {
                seeded,
                unseeded: seeds.len() - seeded,
                unmatched: seed.cells.len().saturating_sub(seeded),
                max_displacement_dbu: 0,
                total_displacement_dbu: 0,
            });
            rustroad_core::place::place_seeded(&problem, &seeds, pc, &place_limits)?
        }
        None => rustroad_core::place::place(&problem, pc, &place_limits)?,
    };
    if let Some(u) = &mut seed_use {
        for (s, p) in seeds.iter().zip(&result.positions) {
            if let Some(s) = s {
                let d = (s[0] - p[0]).abs() + (s[1] - p[1]).abs();
                u.max_displacement_dbu = u.max_displacement_dbu.max(d);
                u.total_displacement_dbu = u.total_displacement_dbu.saturating_add(d);
            }
        }
    }
    for (p, xy) in placed.iter_mut().zip(&result.positions) {
        p.x = xy[0];
        p.y = xy[1];
    }
    // Baseline: fixed netlist-order packing into the same free intervals.
    let mut baseline = Vec::with_capacity(placed.len());
    let mut iv = 0usize;
    let mut x = problem.intervals[0].lx;
    for w in &problem.widths {
        loop {
            let v = problem
                .intervals
                .get(iv)
                .ok_or_else(|| Error::Invalid("baseline packing ran out of intervals".into()))?;
            if x + w <= v.ux {
                baseline.push([x, v.y]);
                x += w + gap;
                break;
            }
            iv += 1;
            x = problem.intervals.get(iv).map_or(0, |v| v.lx);
        }
    }
    Ok(PlacementReport {
        mode: if seed_use.is_some() {
            "seeded_eco_legalize".into()
        } else {
            "analytic_b2b_bisection".into()
        },
        seed: seed_use,
        hpwl_dbu: result.hpwl,
        baseline_hpwl_dbu: rustroad_core::place::hpwl(&problem, &baseline),
        global_hpwl_dbu: result.global_hpwl,
        work: result.work,
        status: "legal_checked".into(),
        net_weights: weight_use.map(|u| WeightUse {
            weighted_hpwl_dbu: rustroad_core::place::weighted_hpwl(&problem, &result.positions)
                / 1000,
            ..u
        }),
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

/// LVS netlist only: an `assign w = x;` between an internal scalar wire `w`
/// (driven by nothing else) and a scalar signal `x` is folded away by
/// renaming every use of `w` to `x` (e.g. tie-cell nets). Port-bit targets
/// and vector aliases are kept, so netlists without such assigns are
/// unchanged.
fn fold_internal_aliases(netlist: &mut verilog::Netlist) {
    let ports: HashSet<&str> = netlist.ports.iter().map(|p| p.name.as_str()).collect();
    let scalar_ports: HashSet<&str> = netlist
        .ports
        .iter()
        .filter(|p| p.range.is_none())
        .map(|p| p.name.as_str())
        .collect();
    let scalar_wires: HashSet<&str> = netlist
        .wires
        .iter()
        .filter(|w| w.range.is_none())
        .map(|w| w.name.as_str())
        .collect();
    let mut lhs_count: HashMap<&str, usize> = HashMap::new();
    for a in &netlist.assigns {
        if let verilog::Expr::Ident(l) = &a.lhs {
            *lhs_count.entry(l.as_str()).or_default() += 1;
        }
    }
    let mut map: HashMap<String, String> = HashMap::new();
    for a in &netlist.assigns {
        if let (verilog::Expr::Ident(l), verilog::Expr::Ident(r)) = (&a.lhs, &a.rhs)
            && scalar_wires.contains(l.as_str())
            && !ports.contains(l.as_str())
            && lhs_count[l.as_str()] == 1
            && (scalar_wires.contains(r.as_str()) || scalar_ports.contains(r.as_str()))
            && l != r
        {
            map.insert(l.clone(), r.clone());
        }
    }
    if map.is_empty() {
        return;
    }
    let resolve = |name: &str| -> String {
        let mut cur = name.to_string();
        for _ in 0..=map.len() {
            match map.get(&cur) {
                Some(next) if next != name => cur = next.clone(),
                _ => break,
            }
        }
        cur
    };
    // A cycle of aliases resolves to itself; keep those assigns.
    let folded: HashMap<String, String> = map
        .keys()
        .map(|k| (k.clone(), resolve(k)))
        .filter(|(k, v)| k != v && !map.contains_key(v))
        .collect();
    netlist.assigns.retain(|a| match &a.lhs {
        verilog::Expr::Ident(l) => !folded.contains_key(l),
        _ => true,
    });
    let rename = |e: &mut verilog::Expr| {
        if let verilog::Expr::Ident(n) = e
            && let Some(r) = folded.get(n)
        {
            *n = r.clone();
        }
    };
    for a in &mut netlist.assigns {
        rename(&mut a.rhs);
        if let verilog::Expr::Concat(items) = &mut a.rhs {
            items.iter_mut().for_each(rename);
        }
    }
    for inst in &mut netlist.instances {
        for c in &mut inst.connections {
            rename(&mut c.signal);
            if let verilog::Expr::Concat(items) = &mut c.signal {
                items.iter_mut().for_each(rename);
            }
        }
    }
    netlist.wires.retain(|w| !folded.contains_key(&w.name));
}

/// Replace the reserved tie-rail wires by `power`/`ground` (supply port
/// names in the powered export, `1'b1`/`1'b0` in the logical one) and drop
/// their declarations.
fn bind_tie_rails(netlist: &mut verilog::Netlist, power: &verilog::Expr, ground: &verilog::Expr) {
    use crate::ties::{RAIL_GROUND_WIRE, RAIL_POWER_WIRE};
    let bind = |e: &mut verilog::Expr| {
        if let verilog::Expr::Ident(name) = e {
            if name == RAIL_POWER_WIRE {
                *e = power.clone();
            } else if name == RAIL_GROUND_WIRE {
                *e = ground.clone();
            }
        }
    };
    for inst in &mut netlist.instances {
        for c in &mut inst.connections {
            bind(&mut c.signal);
        }
    }
    netlist
        .wires
        .retain(|w| w.name != RAIL_POWER_WIRE && w.name != RAIL_GROUND_WIRE);
}

pub(super) fn export_verilog(
    bundle: &PhysicalBundle,
    include_power: bool,
    output: &mut impl Write,
) -> Result<()> {
    let mut netlist = bundle.netlist.clone();
    if let Some(cfg) = bundle.config.as_ref().filter(|_| include_power) {
        bind_tie_rails(
            &mut netlist,
            &verilog::Expr::Ident(cfg.power_net.clone()),
            &verilog::Expr::Ident(cfg.ground_net.clone()),
        );
    } else {
        bind_tie_rails(
            &mut netlist,
            &verilog::Expr::Constant("1'b1".into()),
            &verilog::Expr::Constant("1'b0".into()),
        );
    }
    if include_power {
        fold_internal_aliases(&mut netlist);
        // An open signal pin (`.PIN()` or omitted, e.g. the unused output of
        // a two-output tie cell) gets its own one-pin net `rr_open_N`: the
        // same connectivity, in the form every LVS reader takes.
        let mut used: HashSet<String> = netlist
            .ports
            .iter()
            .map(|p| p.name.clone())
            .chain(netlist.wires.iter().map(|w| w.name.clone()))
            .collect();
        let mut next = 0usize;
        let mut opened = Vec::new();
        for (i, inst) in netlist.instances.iter_mut().enumerate() {
            let m = &bundle.connectivity.masters[bundle.connectivity.instance_masters[i] as usize];
            for pin in m.pins.iter().filter(|p| !p.use_.is_supply()) {
                let open = match inst.connections.iter().position(|c| c.pin == pin.name) {
                    Some(k) => matches!(inst.connections[k].signal, verilog::Expr::Unconnected)
                        .then_some(k),
                    None => {
                        inst.connections.push(verilog::Connection {
                            pin: pin.name.clone(),
                            signal: verilog::Expr::Unconnected,
                        });
                        Some(inst.connections.len() - 1)
                    }
                };
                if let Some(k) = open {
                    let name = loop {
                        let n = format!("rr_open_{next}");
                        next += 1;
                        if used.insert(n.clone()) {
                            break n;
                        }
                    };
                    inst.connections[k].signal = verilog::Expr::Ident(name.clone());
                    opened.push(verilog::Wire { name, range: None });
                }
            }
        }
        netlist.wires.extend(opened);
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
        let names: HashSet<_> = bundle
            .generated_fillers
            .iter()
            .chain(bundle.generated_taps.iter())
            .collect();
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
    let mut routed_wires = HashMap::<u32, Vec<_>>::new();
    let mut routed_vias = HashMap::<u32, Vec<_>>::new();
    let mut routed_patches = HashMap::<u32, Vec<_>>::new();
    // One DEF path per maximal collinear run (identical metal union), not one
    // per stored lattice edge: per-edge paths each carry their own end
    // extension, which reference extractors count again.
    let runs = match &bundle.routing {
        Some(routing) => {
            crate::routing::validate_state(design, bundle, placed)?;
            crate::routing::export_runs(bundle, routing)?
        }
        None => Vec::new(),
    };
    // Rail ties are supply metal: their regular wiring is written under
    // the supply net's own NETS entry (the net is in both sections).
    let export = crate::routing::ExportNets::new(bundle);
    if let Some(routing) = &bundle.routing {
        for wire in &runs {
            routed_wires
                .entry(export.of(wire.net)?)
                .or_default()
                .push(wire);
        }
        for via in &routing.routes.vias {
            routed_vias
                .entry(export.of(via.net)?)
                .or_default()
                .push(via);
        }
        for patch in &routing.routes.patches {
            routed_patches
                .entry(export.of(patch.net)?)
                .or_default()
                .push(patch);
        }
    }
    let tied: HashSet<u32> = routed_wires
        .keys()
        .chain(routed_vias.keys())
        .copied()
        .filter(|&n| model.nets[n as usize].use_.is_supply())
        .collect();
    let signal_pin = |pi: u32| match model.pins[pi as usize].owner {
        PinOwner::Instance { instance, terminal } => !model.masters
            [model.instance_masters[instance as usize] as usize]
            .pins[terminal as usize]
            .use_
            .is_supply(),
        PinOwner::Port { .. } => false,
    };
    for special in [false, true] {
        let count = model
            .nets
            .iter()
            .enumerate()
            .filter(|(i, n)| {
                n.use_.is_supply() == special || (!special && tied.contains(&(*i as u32)))
            })
            .count();
        let section = if special { "SPECIALNETS" } else { "NETS" };
        writeln!(output, "{section} {count} ;")?;
        for (ni, net) in model.nets.iter().enumerate() {
            let tie_entry = !special && tied.contains(&(ni as u32));
            if net.use_.is_supply() != special && !tie_entry {
                continue;
            }
            write!(output, "- {}", def_name(&net.name))?;
            for &pi in &csr.pin_indices[csr.offsets[ni] as usize..csr.offsets[ni + 1] as usize] {
                if tie_entry && !signal_pin(pi) {
                    continue;
                }
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
            if !special && let Some(routing) = &bundle.routing {
                crate::routing::write_def_paths(
                    routing,
                    routed_wires.get(&(ni as u32)).map_or(&[], Vec::as_slice),
                    routed_vias.get(&(ni as u32)).map_or(&[], Vec::as_slice),
                    routed_patches.get(&(ni as u32)).map_or(&[], Vec::as_slice),
                    output,
                )?;
            }
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
        .and_then(|n| n.checked_add(state.tap.as_ref().map_or(0, |t| t.sites.len())))
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
    if let Some(tap) = &state.tap {
        for (i, t) in tap.sites.iter().enumerate() {
            record([5, i as i64, t.x, t.y, t.width, t.row as i64, 0, 0]);
        }
    }
    Ok(out)
}
