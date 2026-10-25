//! Physical connectivity snapshots and a separate generational mutable arena.
//!
//! Resource contract: docs/pdk-import-budget.md. All passes are O(nets + pins +
//! retained geometry), bounded by ConnectivityLimits before allocation. No
//! timing, path enumeration, graph search, or electrically guessed connections.
use crate::{
    Design, Error, InstId, Placed, Result, Symbol, SymbolInterner, def_name, valid_object_name,
};
use serde::{Deserialize, Serialize};
use slotmap::{SlotMap, new_key_type};
use std::collections::HashSet;
use std::io::Write;

new_key_type! { pub struct NetId; pub struct PinId; pub struct CellId; }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectivityLimits {
    pub max_instances: usize,
    pub max_nets: usize,
    pub max_pins: usize,
    pub max_aliases: usize,
    pub max_shapes: usize,
    pub max_vertices: usize,
}
impl Default for ConnectivityLimits {
    fn default() -> Self {
        Self {
            max_instances: 250_000,
            max_nets: 250_000,
            max_pins: 1_000_000,
            max_aliases: 500_000,
            max_shapes: 250_000,
            max_vertices: 1_000_000,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,
    Output,
    Inout,
}
impl Direction {
    pub fn def(self) -> &'static str {
        match self {
            Self::Input => "INPUT",
            Self::Output => "OUTPUT",
            Self::Inout => "INOUT",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinUse {
    #[default]
    Signal,
    Clock,
    Power,
    Ground,
}
impl PinUse {
    pub fn is_supply(self) -> bool {
        matches!(self, Self::Power | Self::Ground)
    }
    pub fn def(self) -> &'static str {
        match self {
            Self::Signal => "SIGNAL",
            Self::Clock => "CLOCK",
            Self::Power => "POWER",
            Self::Ground => "GROUND",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Geometry {
    Rect {
        layer: String,
        rect: [i64; 4],
    },
    Polygon {
        layer: String,
        points: Vec<[i64; 2]>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MasterPin {
    pub name: String,
    pub direction: Option<Direction>,
    pub use_: PinUse,
    pub shape: Option<String>,
    pub ports: Vec<Vec<Geometry>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Master {
    pub name: String,
    pub class: Option<String>,
    pub symmetry: Vec<String>,
    pub site: String,
    pub size: [i64; 2],
    pub pins: Vec<MasterPin>,
    pub obstructions: Vec<Geometry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Net {
    pub name: String,
    pub aliases: Vec<String>,
    #[serde(default)]
    pub use_: PinUse,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub name: String,
    pub direction: Direction,
    pub bits: Vec<String>,
    #[serde(default)]
    pub use_: PinUse,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PinOwner {
    Instance { instance: u32, terminal: u32 },
    Port { port: u32, bit: u32 },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub owner: PinOwner,
    pub net: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectedDesign {
    pub layers: Vec<String>,
    pub masters: Vec<Master>,
    pub instance_masters: Vec<u32>,
    pub ports: Vec<Port>,
    pub nets: Vec<Net>,
    pub pins: Vec<Connection>,
}

/// Compact owner-to-pin lookup. Contiguous owner groups need only their
/// ranges; valid reordered tables use one bounded u32 permutation per pin.
pub struct PinIndex {
    instances: Vec<PinRange>,
    ports: Vec<PinRange>,
    permutation: Option<Vec<u32>>,
}

struct PinRange {
    start: u32,
    len: u32,
}

impl PinIndex {
    pub fn new(model: &ConnectedDesign, limits: &ConnectivityLimits) -> Result<Self> {
        if model.instance_masters.len() > limits.max_instances
            || model.pins.len() > limits.max_pins
            || model.pins.len() >= u32::MAX as usize
            || model.ports.len() > limits.max_pins
        {
            return Err(Error::Budget("pin index object limit".into()));
        }
        let mut count = 0usize;
        let mut range = |len: usize| -> Result<PinRange> {
            count = count
                .checked_add(len)
                .filter(|&n| n <= limits.max_pins && n < u32::MAX as usize)
                .ok_or_else(|| Error::Budget("pin index count limit".into()))?;
            Ok(PinRange {
                start: u32::MAX,
                len: len as u32,
            })
        };
        let mut instances = Vec::with_capacity(model.instance_masters.len());
        for &master in &model.instance_masters {
            let master = model
                .masters
                .get(master as usize)
                .ok_or_else(|| Error::Invalid("pin index master out of range".into()))?;
            instances.push(range(master.pins.len())?);
        }
        let mut ports = Vec::with_capacity(model.ports.len());
        for port in &model.ports {
            ports.push(range(port.bits.len())?);
        }
        if count != model.pins.len() {
            return Err(Error::Invalid("pin index incomplete owner table".into()));
        }
        let mut index = Self {
            instances,
            ports,
            permutation: None,
        };
        let mut contiguous = true;
        for (i, pin) in model.pins.iter().enumerate() {
            let (range, terminal) = index.range_mut(pin.owner)?;
            if range.start == u32::MAX {
                range.start = (i as u32).checked_sub(terminal).unwrap_or(u32::MAX);
            }
            contiguous &= range.start.checked_add(terminal) == Some(i as u32);
        }
        if !contiguous {
            let mut start = 0;
            for range in index.instances.iter_mut().chain(&mut index.ports) {
                range.start = start;
                start += range.len;
            }
            let mut permutation = Vec::new();
            permutation
                .try_reserve_exact(count)
                .map_err(|_| Error::Budget("pin permutation allocation".into()))?;
            permutation.resize(count, u32::MAX);
            for (i, pin) in model.pins.iter().enumerate() {
                let slot = index.slot(pin.owner)? as usize;
                if permutation[slot] != u32::MAX {
                    return Err(Error::Invalid("pin index duplicate owner".into()));
                }
                permutation[slot] = i as u32;
            }
            index.permutation = Some(permutation);
        }
        Ok(index)
    }

    fn range_mut(&mut self, owner: PinOwner) -> Result<(&mut PinRange, u32)> {
        let (ranges, owner, terminal) = match owner {
            PinOwner::Instance { instance, terminal } => (&mut self.instances, instance, terminal),
            PinOwner::Port { port, bit } => (&mut self.ports, port, bit),
        };
        match ranges.get_mut(owner as usize) {
            Some(range) if terminal < range.len => Ok((range, terminal)),
            _ => Err(Error::Invalid("pin index owner out of range".into())),
        }
    }

    fn slot(&self, owner: PinOwner) -> Result<u32> {
        let (ranges, owner, terminal) = match owner {
            PinOwner::Instance { instance, terminal } => (&self.instances, instance, terminal),
            PinOwner::Port { port, bit } => (&self.ports, port, bit),
        };
        match ranges.get(owner as usize) {
            Some(range) if terminal < range.len => Ok(range.start + terminal),
            _ => Err(Error::Invalid("pin index owner out of range".into())),
        }
    }

    pub fn get(&self, owner: PinOwner) -> Result<u32> {
        let slot = self.slot(owner)?;
        Ok(self.permutation.as_ref().map_or(slot, |p| p[slot as usize]))
    }
}

impl ConnectedDesign {
    pub fn validate(&self, design: &Design, limits: &ConnectivityLimits) -> Result<()> {
        design.validate(&crate::Limits {
            max_cells: limits.max_instances,
            max_rows: crate::Limits::default().max_rows,
            max_expansions: 0,
        })?;
        if design.cells.len() >= u32::MAX as usize
            || self.nets.len() > limits.max_nets
            || self.pins.len() > limits.max_pins
            || self.nets.len() >= u32::MAX as usize
            || self.pins.len() >= u32::MAX as usize
            || self.masters.len() > limits.max_instances
            || self.ports.len() > limits.max_pins
            || self.layers.len() > 4096
            || self.instance_masters.len() != design.cells.len()
        {
            return Err(Error::Budget("connected design table capacity".into()));
        }
        let mut aliases = 0usize;
        let mut names = std::collections::HashMap::new();
        for (net_index, net) in self.nets.iter().enumerate() {
            aliases = aliases
                .checked_add(net.aliases.len())
                .ok_or_else(|| Error::Budget("alias count overflow".into()))?;
            if aliases > limits.max_aliases {
                return Err(Error::Budget("max_aliases exceeded".into()));
            }
            if !valid_object_name(&net.name)
                || net.aliases.is_empty()
                || !net.aliases.contains(&net.name)
            {
                return Err(Error::Invalid("invalid net name/aliases".into()));
            }
            for alias in &net.aliases {
                if !valid_object_name(alias)
                    || names.insert(alias.as_str(), net_index as u32).is_some()
                {
                    return Err(Error::Invalid("invalid or repeated net alias".into()));
                }
            }
        }
        let layer_names: HashSet<_> = self.layers.iter().map(String::as_str).collect();
        if layer_names.len() != self.layers.len()
            || self.layers.iter().any(|l| !valid_object_name(l))
        {
            return Err(Error::Invalid("duplicate/invalid technology layer".into()));
        }
        let mut shapes = 0usize;
        let mut vertices = 0usize;
        let mut master_names = HashSet::new();
        let mut master_pins = 0usize;
        for master in &self.masters {
            master_pins = master_pins
                .checked_add(master.pins.len())
                .ok_or_else(|| Error::Budget("master pin count overflow".into()))?;
            if master_pins > limits.max_pins {
                return Err(Error::Budget("retained master pin budget exceeded".into()));
            }
            if !valid_object_name(&master.name)
                || !master_names.insert(master.name.as_str())
                || master.site != design.site.name
                || master.size[0] <= 0
                || master.size[1] != design.site.height
                || master.size[0] % design.site.width != 0
                || master.pins.len() > limits.max_pins
            {
                return Err(Error::Invalid("invalid or incompatible LEF master".into()));
            }
            let mut pin_names = HashSet::new();
            for pin in &master.pins {
                if !valid_object_name(&pin.name)
                    || !pin_names.insert(pin.name.as_str())
                    || (!pin.use_.is_supply() && pin.direction.is_none())
                {
                    return Err(Error::Invalid(
                        "invalid/duplicate LEF pin or absent signal direction".into(),
                    ));
                }
                if pin.ports.len() > limits.max_shapes {
                    return Err(Error::Budget(
                        "pin PORT count exceeds geometry limit".into(),
                    ));
                }
            }
            for shape in master
                .pins
                .iter()
                .flat_map(|p| p.ports.iter().flatten())
                .chain(&master.obstructions)
            {
                shapes += 1;
                if shapes > limits.max_shapes {
                    return Err(Error::Budget("max_shapes exceeded".into()));
                }
                let layer = match shape {
                    Geometry::Rect { layer, rect } => {
                        if rect[0] >= rect[2] || rect[1] >= rect[3] {
                            return Err(Error::Invalid("invalid LEF rectangle".into()));
                        }
                        vertices += 2;
                        layer
                    }
                    Geometry::Polygon { layer, points } => {
                        if points.len() < 3 {
                            return Err(Error::Invalid("invalid LEF polygon".into()));
                        }
                        vertices = vertices
                            .checked_add(points.len())
                            .ok_or_else(|| Error::Budget("vertex count overflow".into()))?;
                        layer
                    }
                };
                if vertices > limits.max_vertices {
                    return Err(Error::Budget("max_vertices exceeded".into()));
                }
                if !layer_names.contains(layer.as_str()) {
                    return Err(Error::Invalid(format!(
                        "unknown pin/obstruction layer {layer}"
                    )));
                }
            }
        }
        let mut expected_pins = 0usize;
        for (index, master_index) in self.instance_masters.iter().enumerate() {
            let master = self
                .masters
                .get(*master_index as usize)
                .ok_or_else(|| Error::Invalid("invalid master index".into()))?;
            let cell = &design.cells[index];
            if cell.master != master.name || [cell.width, cell.height] != master.size {
                return Err(Error::Invalid("instance and LEF master mismatch".into()));
            }
            expected_pins = expected_pins
                .checked_add(master.pins.len())
                .ok_or_else(|| Error::Budget("pin count overflow".into()))?;
            if expected_pins > limits.max_pins {
                return Err(Error::Budget("max_pins exceeded".into()));
            }
        }
        let mut port_names = HashSet::new();
        let mut bit_names = HashSet::new();
        for port in &self.ports {
            if !valid_object_name(&port.name)
                || !port_names.insert(port.name.as_str())
                || port.bits.is_empty()
            {
                return Err(Error::Invalid("invalid top-level port".into()));
            }
            expected_pins = expected_pins
                .checked_add(port.bits.len())
                .ok_or_else(|| Error::Budget("port count overflow".into()))?;
            if expected_pins > limits.max_pins {
                return Err(Error::Budget("max_pins exceeded".into()));
            }
            for bit in &port.bits {
                if !valid_object_name(bit) || !bit_names.insert(bit.as_str()) {
                    return Err(Error::Invalid("invalid/duplicate port bit".into()));
                }
            }
        }
        if expected_pins != self.pins.len() {
            return Err(Error::Invalid("pin coverage count mismatch".into()));
        }
        // Coverage/uniqueness needs no hash table for every physical pin.
        // The common contiguous representation is checked with owner ranges;
        // valid permutations retain the same bounded validation semantics.
        drop(PinIndex::new(self, limits)?);
        let mut drivers = vec![0u32; self.nets.len()];
        let mut sinks = vec![0u32; self.nets.len()];
        for pin in &self.pins {
            let (direction, use_, is_port) = match pin.owner {
                PinOwner::Instance { instance, terminal } => {
                    let mi = self
                        .instance_masters
                        .get(instance as usize)
                        .ok_or_else(|| Error::Invalid("pin instance index".into()))?;
                    let spec = self.masters[*mi as usize]
                        .pins
                        .get(terminal as usize)
                        .ok_or_else(|| Error::Invalid("master pin index".into()))?;
                    (spec.direction, spec.use_, false)
                }
                PinOwner::Port { port, bit } => {
                    let spec = self
                        .ports
                        .get(port as usize)
                        .ok_or_else(|| Error::Invalid("port index".into()))?;
                    let name = spec
                        .bits
                        .get(bit as usize)
                        .ok_or_else(|| Error::Invalid("port bit index".into()))?;
                    if let Some(net) = pin.net
                        && names.get(name.as_str()) != Some(&net)
                    {
                        return Err(Error::Invalid(
                            "port bit is assigned to the wrong net".into(),
                        ));
                    }
                    (Some(spec.direction), spec.use_, true)
                }
            };
            if use_.is_supply() {
                if let Some(net) = pin.net
                    && self.nets.get(net as usize).map(|n| n.use_) != Some(use_)
                {
                    return Err(Error::Invalid("supply pin net polarity mismatch".into()));
                }
                continue;
            }
            let Some(net_index) = pin.net else {
                if direction != Some(Direction::Output) || is_port {
                    return Err(Error::Invalid("unconnected input/top-level pin".into()));
                }
                continue;
            };
            let net_index = net_index as usize;
            if net_index >= self.nets.len() {
                return Err(Error::Invalid("pin net index".into()));
            }
            if self.nets[net_index].use_.is_supply() {
                // A cell input on a supply net is a rail tie (the input of a
                // rail-input tie cell); outputs and ports never are.
                if is_port || direction != Some(Direction::Input) {
                    return Err(Error::Invalid(
                        "signal pin is shorted to supply without a tie cell".into(),
                    ));
                }
                continue;
            }
            let driver = match direction {
                Some(Direction::Input) => is_port,
                Some(Direction::Output) => !is_port,
                _ => {
                    return Err(Error::Invalid(
                        "bidirectional/unknown signal direction is unsupported".into(),
                    ));
                }
            };
            if driver {
                drivers[net_index] += 1;
            } else {
                sinks[net_index] += 1;
            }
        }
        for (index, (&drivers, &sinks)) in drivers.iter().zip(&sinks).enumerate() {
            if drivers > 1 || (sinks > 0 && drivers == 0) {
                return Err(Error::Invalid(format!(
                    "net {} has {drivers} drivers and {sinks} sinks",
                    self.nets[index].name
                )));
            }
        }
        Ok(())
    }

    pub fn lower_csr(
        &self,
        design: &Design,
        limits: &ConnectivityLimits,
    ) -> Result<ConnectivityCsr> {
        self.validate(design, limits)?;
        let mut offsets = vec![0u64; self.nets.len() + 1];
        for pin in &self.pins {
            if let Some(n) = pin.net {
                offsets[n as usize + 1] += 1;
            }
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut cursor = offsets[..self.nets.len()].to_vec();
        let mut pin_indices = vec![0u32; *offsets.last().unwrap_or(&0) as usize];
        for (i, pin) in self.pins.iter().enumerate() {
            if let Some(n) = pin.net {
                pin_indices[cursor[n as usize] as usize] = i as u32;
                cursor[n as usize] += 1;
            }
        }
        Ok(ConnectivityCsr {
            offsets,
            pin_indices,
        })
    }
}

/// A derived contiguous read/compute representation, never the mutable IR.
pub struct ConnectivityCsr {
    pub offsets: Vec<u64>,
    pub pin_indices: Vec<u32>,
}
pub struct ArenaNet {
    pub name: Symbol,
}
pub struct ArenaPin {
    pub net: Option<NetId>,
    pub owner: ArenaPinOwner,
}
pub enum ArenaPinOwner {
    Instance { instance: InstId, terminal: u32 },
    Port { port: u32, bit: u32 },
}
pub struct ArenaCell {
    pub name: Symbol,
}
pub struct ArenaInstance {
    pub name: Symbol,
    pub cell: CellId,
}
pub struct ConnectivityDb {
    symbols: SymbolInterner,
    nets: SlotMap<NetId, ArenaNet>,
    pins: SlotMap<PinId, ArenaPin>,
    cells: SlotMap<CellId, ArenaCell>,
    instances: SlotMap<InstId, ArenaInstance>,
}
impl ConnectivityDb {
    pub fn new(
        model: &ConnectedDesign,
        design: &Design,
        limits: &ConnectivityLimits,
    ) -> Result<Self> {
        model.validate(design, limits)?;
        let mut db = Self {
            symbols: SymbolInterner::default(),
            nets: SlotMap::with_key(),
            pins: SlotMap::with_key(),
            cells: SlotMap::with_key(),
            instances: SlotMap::with_key(),
        };
        let mut net_ids = Vec::with_capacity(model.nets.len());
        for net in &model.nets {
            let name = db.symbols.intern(&net.name);
            net_ids.push(db.nets.insert(ArenaNet { name }));
        }
        let mut cell_ids = Vec::with_capacity(model.masters.len());
        for cell in &model.masters {
            let name = db.symbols.intern(&cell.name);
            cell_ids.push(db.cells.insert(ArenaCell { name }));
        }
        let mut instance_ids = Vec::with_capacity(design.cells.len());
        for (index, instance) in design.cells.iter().enumerate() {
            let name = db.symbols.intern(&instance.name);
            instance_ids.push(db.instances.insert(ArenaInstance {
                name,
                cell: cell_ids[model.instance_masters[index] as usize],
            }));
        }
        for pin in &model.pins {
            db.pins.insert(ArenaPin {
                net: pin.net.map(|n| net_ids[n as usize]),
                owner: match pin.owner {
                    PinOwner::Instance { instance, terminal } => ArenaPinOwner::Instance {
                        instance: instance_ids[instance as usize],
                        terminal,
                    },
                    PinOwner::Port { port, bit } => ArenaPinOwner::Port { port, bit },
                },
            });
        }
        Ok(db)
    }
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.instances.len(), self.nets.len(), self.pins.len())
    }
}

pub fn write_connected_def(
    design: &Design,
    cells: &[Placed],
    model: &ConnectedDesign,
    limits: &ConnectivityLimits,
    output: &mut impl Write,
) -> std::io::Result<()> {
    model
        .validate(design, limits)
        .map_err(std::io::Error::other)?;
    let csr = model
        .lower_csr(design, limits)
        .map_err(std::io::Error::other)?;
    crate::write_def_components(design, cells, output)?;
    let port_pins = model.ports.iter().map(|p| p.bits.len()).sum::<usize>();
    writeln!(output, "PINS {port_pins} ;")?;
    for pin in &model.pins {
        if let PinOwner::Port { port, bit } = pin.owner {
            let spec = &model.ports[port as usize];
            let net = pin.net.expect("validated connected port");
            writeln!(
                output,
                "- {} + NET {} + DIRECTION {} + USE SIGNAL ;",
                def_name(&spec.bits[bit as usize]),
                def_name(&model.nets[net as usize].name),
                spec.direction.def()
            )?;
        }
    }
    writeln!(output, "END PINS\nNETS {} ;", model.nets.len())?;
    for (ni, net) in model.nets.iter().enumerate() {
        write!(output, "- {}", def_name(&net.name))?;
        for &pi in &csr.pin_indices[csr.offsets[ni] as usize..csr.offsets[ni + 1] as usize] {
            match model.pins[pi as usize].owner {
                PinOwner::Instance { instance, terminal } => {
                    let master = &model.masters[model.instance_masters[instance as usize] as usize];
                    write!(
                        output,
                        " ( {} {} )",
                        def_name(&design.cells[instance as usize].name),
                        def_name(&master.pins[terminal as usize].name)
                    )?;
                }
                PinOwner::Port { port, bit } => write!(
                    output,
                    " ( PIN {} )",
                    def_name(&model.ports[port as usize].bits[bit as usize])
                )?,
            }
        }
        writeln!(output, " + USE SIGNAL ;")?;
    }
    writeln!(output, "END NETS\nEND DESIGN")
}

#[cfg(test)]
mod pin_index_tests {
    use super::*;

    fn model() -> ConnectedDesign {
        let master = Master {
            name: "buffer".into(),
            class: None,
            symmetry: Vec::new(),
            site: "core".into(),
            size: [10, 10],
            pins: ["A", "Y"]
                .into_iter()
                .map(|name| MasterPin {
                    name: name.into(),
                    direction: None,
                    use_: PinUse::Signal,
                    shape: None,
                    ports: Vec::new(),
                })
                .collect(),
            obstructions: Vec::new(),
        };
        // Ordinary ports precede instances; an added supply port follows them.
        let owners = [
            PinOwner::Port { port: 0, bit: 0 },
            PinOwner::Port { port: 0, bit: 1 },
            PinOwner::Instance {
                instance: 0,
                terminal: 0,
            },
            PinOwner::Instance {
                instance: 0,
                terminal: 1,
            },
            PinOwner::Instance {
                instance: 1,
                terminal: 0,
            },
            PinOwner::Instance {
                instance: 1,
                terminal: 1,
            },
            PinOwner::Port { port: 1, bit: 0 },
        ];
        ConnectedDesign {
            layers: Vec::new(),
            masters: vec![master],
            instance_masters: vec![0, 0],
            ports: vec![
                Port {
                    name: "a".into(),
                    direction: Direction::Input,
                    bits: vec!["a[0]".into(), "a[1]".into()],
                    use_: PinUse::Signal,
                },
                Port {
                    name: "VDD".into(),
                    direction: Direction::Inout,
                    bits: vec!["VDD".into()],
                    use_: PinUse::Power,
                },
            ],
            nets: Vec::new(),
            pins: owners
                .into_iter()
                .map(|owner| Connection { owner, net: None })
                .collect(),
        }
    }

    #[test]
    fn index_matches_independent_owner_search_across_pin_orders() {
        let original = model();
        for reverse in [false, true] {
            for rotation in 0..original.pins.len() {
                let mut model = original.clone();
                model.pins.rotate_left(rotation);
                if reverse {
                    model.pins.reverse();
                }
                let index = PinIndex::new(&model, &ConnectivityLimits::default()).unwrap();
                for pin in &original.pins {
                    let expected = model
                        .pins
                        .iter()
                        .position(|p| p.owner == pin.owner)
                        .unwrap();
                    assert_eq!(index.get(pin.owner).unwrap() as usize, expected);
                }
                if rotation == 0 && !reverse {
                    assert!(
                        index.permutation.is_none(),
                        "contiguous groups need no pin-sized table"
                    );
                }
            }
        }
    }

    #[test]
    fn duplicate_missing_invalid_and_over_budget_owners_fail() {
        let original = model();
        let mut duplicate = original.clone();
        duplicate.pins[1] = duplicate.pins[0];
        assert!(PinIndex::new(&duplicate, &ConnectivityLimits::default()).is_err());
        let mut missing = original.clone();
        missing.pins.pop();
        assert!(PinIndex::new(&missing, &ConnectivityLimits::default()).is_err());
        let mut invalid = original.clone();
        invalid.pins[1].owner = PinOwner::Instance {
            instance: 0,
            terminal: 99,
        };
        assert!(PinIndex::new(&invalid, &ConnectivityLimits::default()).is_err());
        assert!(
            PinIndex::new(
                &original,
                &ConnectivityLimits {
                    max_pins: 6,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}
