//! Revisioned routing bridge. Geometry/search live in the independent CPU view.
use crate::pdk::{PhysicalBundle, SourcePin};
use crate::{Error, Result, Stage, Summary};
use rustroad_core::connectivity::{Direction, Geometry, PinOwner};
use rustroad_core::routing::{
    self, AccessClass, Blockage, LayerRect, RouteInput, RouteLayer, RouteLimits, RouteVia, Routes,
    Terminal,
};
use rustroad_core::{Design, Limits, Placed, Rect};
use rustroad_formats::lef::LefGeometry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerPolicy {
    pub name: String,
    pub spacing: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViaPolicy {
    pub name: String,
    pub cut_spacing: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    pub grid_step: i64,
    pub access_layer: String,
    pub layers: Vec<LayerPolicy>,
    pub vias: Vec<ViaPolicy>,
    #[serde(default)]
    pub limits: RouteLimits,
    pub max_skew_ns: f64,
    /// LEF-rule mode: AREA/MINSTEP/ENCLOSURE, every single-cut LEF fixed-via
    /// variant per policy via, the pin-access table and same-net cleanup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lef_rules: bool,
}
impl RoutingConfig {
    fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        if self.layers.len() < 2
            || self.layers.len() > 8
            || self.vias.len() > 12
            || self.grid_step <= 0
            || !self.max_skew_ns.is_finite()
            || self.max_skew_ns <= 0.0
            || self
                .layers
                .iter()
                .any(|l| !rustroad_core::valid_object_name(&l.name) || l.spacing < 0)
            || self
                .vias
                .iter()
                .any(|v| !rustroad_core::valid_object_name(&v.name) || v.cut_spacing < 0)
        {
            return Err(Error::Invalid("invalid routing configuration".into()));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteScope {
    Clock,
    All,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingState {
    pub source: SourcePin,
    pub config: RoutingConfig,
    pub scope: RouteScope,
    pub layers: Vec<String>,
    pub via_names: Vec<String>,
    pub via_lower_layers: Vec<u32>,
    pub routes: Routes,
}
fn fail(s: &str) -> Error {
    Error::Invalid(s.into())
}
fn rectangle(g: &Geometry) -> Result<(&str, Rect)> {
    match g {
        Geometry::Rect { layer, rect } => Ok((
            layer,
            Rect {
                lx: rect[0],
                ly: rect[1],
                ux: rect[2],
                uy: rect[3],
            },
        )),
        Geometry::Polygon { .. } => Err(fail(
            "routing polygon pin/OBS requires a polygon-aware access model",
        )),
    }
}
fn lef_rectangle(g: &LefGeometry) -> Result<(&str, Rect)> {
    match g {
        LefGeometry::Rect { layer, rect } => Ok((
            layer,
            Rect {
                lx: rect[0],
                ly: rect[1],
                ux: rect[2],
                uy: rect[3],
            },
        )),
        LefGeometry::Polygon { .. } => Err(fail("routing polygon VIA unsupported")),
    }
}
pub(crate) fn clock_nets(bundle: &PhysicalBundle) -> Result<BTreeSet<u32>> {
    let report = bundle
        .clock_tree
        .as_ref()
        .ok_or_else(|| fail("clock tree required before routing"))?;
    let cfg = bundle
        .config
        .as_ref()
        .and_then(|c| c.clock.as_ref())
        .ok_or_else(|| fail("clock config required"))?;
    let by_name: HashMap<_, _> = bundle
        .netlist
        .instances
        .iter()
        .enumerate()
        .map(|(i, x)| (x.name.as_str(), i))
        .collect();
    let buffers: BTreeSet<_> = report.buffers.iter().map(|n| by_name[n.as_str()]).collect();
    let model = &bundle.connectivity;
    let mut nets = BTreeSet::new();
    for p in &model.pins {
        let source = match p.owner {
            PinOwner::Port { port, .. } => model.ports[port as usize].name == cfg.port,
            PinOwner::Instance { instance, terminal } => {
                buffers.contains(&(instance as usize))
                    && model.masters[model.instance_masters[instance as usize] as usize].pins
                        [terminal as usize]
                        .name
                        == cfg.output_pin
            }
        };
        if source {
            nets.insert(p.net.ok_or_else(|| fail("unconnected clock source"))?);
        }
    }
    Ok(nets)
}

/// One LEF fixed via as routing shapes; a cut layer joins the routing view
/// with the policy cut spacing. The via must join two selected routing layers.
fn route_via<'a>(
    bundle: &'a PhysicalBundle,
    via: &'a rustroad_formats::lef::LefVia,
    policy: &ViaPolicy,
    layers: &mut Vec<RouteLayer>,
    indices: &mut HashMap<&'a str, u32>,
) -> Result<RouteVia> {
    let mut shapes = Vec::new();
    let mut metals = BTreeSet::new();
    for g in &via.geometries {
        let (name, r) = lef_rectangle(g)?;
        let tech = bundle
            .technology
            .layers
            .iter()
            .find(|l| l.name == name)
            .ok_or_else(|| fail("VIA layer absent"))?;
        let index = if let Some(i) = indices.get(name) {
            *i
        } else if tech.kind.as_deref() == Some("CUT") {
            let i = layers.len() as u32;
            layers.push(RouteLayer {
                name: name.into(),
                width: tech.width.unwrap_or(r.ux - r.lx),
                spacing: policy.cut_spacing.max(tech.spacing.unwrap_or(0)),
                vertical: None,
                min_area: None,
                min_step: None,
            });
            indices.insert(tech.name.as_str(), i);
            i
        } else {
            return Err(fail("VIA reaches a layer outside routing policy"));
        };
        if layers[index as usize].vertical.is_some() {
            metals.insert(index);
        }
        shapes.push(LayerRect {
            layer: index,
            rect: r,
        });
    }
    if metals.len() != 2 {
        return Err(fail("VIA must connect two selected routing layers"));
    }
    Ok(RouteVia {
        name: via.name.clone(),
        lower: *metals.first().unwrap(),
        upper: *metals.last().unwrap(),
        shapes,
    })
}
fn cuts(v: &RouteVia, layers: &[RouteLayer]) -> Vec<LayerRect> {
    v.shapes
        .iter()
        .filter(|s| layers[s.layer as usize].vertical.is_none())
        .cloned()
        .collect()
}
/// A single-cut variant of `base`: same cut rectangle, same two metal layers,
/// exactly one pad rectangle per metal layer.
fn variant_of(base: &RouteVia, other: &RouteVia, layers: &[RouteLayer]) -> bool {
    let (a, b) = (cuts(base, layers), cuts(other, layers));
    let pads = |v: &RouteVia, l: u32| v.shapes.iter().filter(|s| s.layer == l).count();
    a.len() == 1
        && b.len() == 1
        && a[0].layer == b[0].layer
        && a[0].rect == b[0].rect
        && other.lower == base.lower
        && other.upper == base.upper
        && pads(other, other.lower) == 1
        && pads(other, other.upper) == 1
        && other.shapes.len() == 3
}
fn same_shapes(a: &RouteVia, b: &RouteVia) -> bool {
    let key = |v: &RouteVia| {
        let mut k: Vec<_> = v
            .shapes
            .iter()
            .map(|s| (s.layer, s.rect.lx, s.rect.ly, s.rect.ux, s.rect.uy))
            .collect();
        k.sort_unstable();
        k
    };
    key(a) == key(b)
}
fn pad_area(v: &RouteVia, layers: &[RouteLayer]) -> i128 {
    v.shapes
        .iter()
        .filter(|s| layers[s.layer as usize].vertical.is_some())
        .map(|s| i128::from(s.rect.ux - s.rect.lx) * i128::from(s.rect.uy - s.rect.ly))
        .sum()
}
/// LEF cut ENCLOSURE BELOW/ABOVE: every pad on the lower/upper metal must
/// overhang each cut by (a, b) in either orientation. Absent rules pass.
fn encloses(bundle: &PhysicalBundle, v: &RouteVia, layers: &[RouteLayer]) -> Result<bool> {
    for cut in cuts(v, layers) {
        let tech = bundle
            .technology
            .layers
            .iter()
            .find(|l| l.name == layers[cut.layer as usize].name)
            .ok_or_else(|| fail("VIA cut layer absent"))?;
        for (metal, rule) in [
            (v.lower, tech.enclosure_below),
            (v.upper, tech.enclosure_above),
        ] {
            let Some([a, b]) = rule else {
                continue;
            };
            for pad in v.shapes.iter().filter(|s| s.layer == metal) {
                let ox = (cut.rect.lx - pad.rect.lx).min(pad.rect.ux - cut.rect.ux);
                let oy = (cut.rect.ly - pad.rect.ly).min(pad.rect.uy - cut.rect.uy);
                if !((ox >= a && oy >= b) || (ox >= b && oy >= a)) {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Routing layers (policy order, then cut layers) and fixed vias (policy via,
/// then its LEF variants in LEF-rule mode) with the name-to-index map.
pub(crate) type Technology<'a> = (Vec<RouteLayer>, Vec<RouteVia>, HashMap<&'a str, u32>);
pub(crate) fn route_technology<'a>(
    bundle: &'a PhysicalBundle,
    config: &RoutingConfig,
) -> Result<Technology<'a>> {
    let mut layers = Vec::new();
    let mut indices = HashMap::new();
    for policy in &config.layers {
        let l = bundle
            .technology
            .layers
            .iter()
            .find(|l| l.name == policy.name)
            .ok_or_else(|| fail("routing layer absent"))?;
        if l.kind.as_deref() != Some("ROUTING") || indices.contains_key(l.name.as_str()) {
            return Err(fail("routing layer kind or duplicate"));
        }
        let width = l
            .width
            .ok_or_else(|| fail("routing default width absent"))?;
        let vertical = match l.direction.as_deref() {
            Some("VERTICAL") => true,
            Some("HORIZONTAL") => false,
            _ => return Err(fail("routing direction absent")),
        };
        indices.insert(l.name.as_str(), layers.len() as u32);
        layers.push(RouteLayer {
            name: l.name.clone(),
            width,
            spacing: policy.spacing.max(l.spacing.unwrap_or(0)),
            vertical: Some(vertical),
            min_area: if config.lef_rules { l.min_area } else { None },
            min_step: if config.lef_rules { l.min_step } else { None },
        });
    }
    let mut vias: Vec<RouteVia> = Vec::new();
    for policy in &config.vias {
        let via = bundle
            .technology
            .vias
            .iter()
            .find(|v| v.name == policy.name)
            .ok_or_else(|| fail("routing VIA absent"))?;
        let configured = route_via(bundle, via, policy, &mut layers, &mut indices)?;
        if config.lef_rules && !encloses(bundle, &configured, &layers)? {
            return Err(fail("configured routing VIA violates LEF cut ENCLOSURE"));
        }
        let mut family = Vec::new();
        if config.lef_rules {
            for other in &bundle.technology.vias {
                if other.name == via.name
                    || other
                        .geometries
                        .iter()
                        .any(|g| matches!(g, LefGeometry::Polygon { .. }))
                {
                    continue;
                }
                let Ok(candidate) = route_via(bundle, other, policy, &mut layers, &mut indices)
                else {
                    continue;
                };
                if variant_of(&configured, &candidate, &layers)
                    && encloses(bundle, &candidate, &layers)?
                    && !family
                        .iter()
                        .chain(std::iter::once(&configured))
                        .any(|v: &RouteVia| same_shapes(v, &candidate))
                {
                    family.push(candidate);
                }
            }
            family.sort_by_key(|v| (pad_area(v, &layers), v.name.clone()));
        }
        vias.push(configured);
        vias.extend(family);
    }
    Ok((layers, vias, indices))
}

/// Electrical owner of one routing obstacle, kept for extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObstacleOwner {
    /// Instance pin shape, by connectivity pin index.
    InstancePin(u32),
    InstanceObstruction,
    /// Signal I/O pin rectangle, by connectivity pin index.
    PortPin(u32),
    Supply,
}

pub(crate) fn build_input(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    config: &RoutingConfig,
    scope: RouteScope,
) -> Result<RouteInput> {
    build_input_with_owners(design, bundle, placed, config, scope).map(|(input, _)| input)
}

/// Same routing input plus one owner per obstacle (parallel vector).
pub(crate) fn build_input_with_owners(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    config: &RoutingConfig,
    scope: RouteScope,
) -> Result<(RouteInput, Vec<ObstacleOwner>)> {
    config.validate()?;
    let state = bundle
        .state
        .as_ref()
        .ok_or_else(|| fail("prepared placement required"))?;
    let model = &bundle.connectivity;
    let grid = bundle
        .technology
        .manufacturing_grid
        .ok_or_else(|| fail("manufacturing grid missing"))?;
    let (layers, vias, indices) = route_technology(bundle, config)?;
    let selected: BTreeSet<_> = match scope {
        RouteScope::Clock => {
            if bundle
                .clock_tree
                .as_ref()
                .and_then(|c| c.constraints.as_ref())
                .is_none()
            {
                return Err(fail("clock routing requires explicit SDC constraints"));
            }
            clock_nets(bundle)?
        }
        RouteScope::All => model
            .nets
            .iter()
            .enumerate()
            .filter_map(|(i, n)| (!n.use_.is_supply()).then_some(i as u32))
            .collect(),
    };
    let pin_nets: HashMap<_, _> = model.pins.iter().map(|p| (p.owner, p.net)).collect();
    let pin_ids: HashMap<_, _> = model
        .pins
        .iter()
        .enumerate()
        .map(|(i, p)| (p.owner, i as u32))
        .collect();
    let mut obstacles = Vec::new();
    let mut owners = Vec::new();
    let mut terminals = Vec::new();
    let mut push = |layer: u32, rect: Rect, net: Option<u32>, owner: ObstacleOwner| -> Result<()> {
        if obstacles.len() >= config.limits.max_obstacles {
            return Err(Error::Budget("routing input obstacles".into()));
        }
        obstacles.push(Blockage { layer, rect, net });
        owners.push(owner);
        Ok(())
    };
    let mut classes = Vec::<AccessClass>::new();
    let mut class_ids = HashMap::<(u32, bool, usize), u32>::new();
    for (i, p) in placed.iter().enumerate() {
        let master = &model.masters[model.instance_masters[i] as usize];
        let orientation = rustroad_core::placed_orientation(design, p)?;
        for (ti, pin) in master.pins.iter().enumerate() {
            let owner = PinOwner::Instance {
                instance: i as u32,
                terminal: ti as u32,
            };
            let net = pin_nets[&owner];
            let mut shapes = Vec::new();
            for geometry in pin.ports.iter().flatten() {
                let layer = match geometry {
                    Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
                };
                let Some(&li) = indices.get(layer.as_str()) else {
                    continue;
                };
                let (_, r) = rectangle(geometry)?;
                let r = rustroad_core::transform_rect(r, p.x, p.y, p.height, orientation)?;
                push(li, r, net, ObstacleOwner::InstancePin(pin_ids[&owner]))?;
                if layers[li as usize].vertical.is_some() {
                    shapes.push(LayerRect { layer: li, rect: r });
                }
            }
            if let Some(n) = net
                && selected.contains(&n)
                && !pin.use_.is_supply()
            {
                let class = if config.lef_rules {
                    let key = (
                        model.instance_masters[i],
                        orientation == rustroad_core::Orientation::FS,
                        ti,
                    );
                    let id = match class_ids.get(&key) {
                        Some(id) => *id,
                        None => {
                            let id = classes.len() as u32;
                            classes.push(access_class(
                                master,
                                orientation,
                                p.height,
                                ti,
                                &indices,
                                &layers,
                            )?);
                            class_ids.insert(key, id);
                            id
                        }
                    };
                    Some((id, [p.x, p.y]))
                } else {
                    None
                };
                terminals.push(Terminal {
                    pin: pin_ids[&owner],
                    net: n,
                    driver: pin.direction == Some(Direction::Output),
                    shapes,
                    class,
                });
            }
        }
        for geometry in &master.obstructions {
            let layer = match geometry {
                Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
            };
            if let Some(&li) = indices.get(layer.as_str()) {
                let (_, r) = rectangle(geometry)?;
                push(
                    li,
                    rustroad_core::transform_rect(r, p.x, p.y, p.height, orientation)?,
                    None,
                    ObstacleOwner::InstanceObstruction,
                )?;
            }
        }
    }
    let io: HashMap<_, _> = state.io_pins.iter().map(|p| (p.name.as_str(), p)).collect();
    for (i, port) in model
        .ports
        .iter()
        .enumerate()
        .filter(|(_, p)| !p.use_.is_supply())
    {
        for (bit, name) in port.bits.iter().enumerate() {
            let p = io
                .get(name.as_str())
                .ok_or_else(|| fail("signal I/O placement absent"))?;
            let Some(&layer) = indices.get(p.layer.as_str()) else {
                return Err(fail("I/O layer not selected for routing"));
            };
            let owner = PinOwner::Port {
                port: i as u32,
                bit: bit as u32,
            };
            let net = pin_nets[&owner];
            push(layer, p.rect, net, ObstacleOwner::PortPin(pin_ids[&owner]))?;
            if let Some(net) = net
                && selected.contains(&net)
            {
                terminals.push(Terminal {
                    pin: pin_ids[&owner],
                    net,
                    driver: port.direction == Direction::Input,
                    shapes: vec![LayerRect {
                        layer,
                        rect: p.rect,
                    }],
                    class: None,
                });
            }
        }
    }
    let cfg = bundle.config.as_ref().unwrap();
    let supply_net = |p: rustroad_core::power::RailPolarity| -> Result<u32> {
        let name = if p == rustroad_core::power::RailPolarity::Power {
            &cfg.power_net
        } else {
            &cfg.ground_net
        };
        model
            .nets
            .iter()
            .position(|n| n.name == *name)
            .map(|i| i as u32)
            .ok_or_else(|| fail("supply net absent"))
    };
    for w in &state.supply.wires {
        if let Some(&l) = indices.get(w.layer.as_str()) {
            push(
                l,
                routing::wire_rect(
                    &routing::Wire {
                        net: 0,
                        layer: l,
                        start: w.start,
                        end: w.end,
                    },
                    w.width,
                )?,
                Some(supply_net(w.polarity)?),
                ObstacleOwner::Supply,
            )?;
        }
    }
    for v in &state.supply.vias {
        let via = bundle
            .technology
            .vias
            .iter()
            .find(|x| x.name == v.name)
            .ok_or_else(|| fail("supply VIA missing"))?;
        for g in &via.geometries {
            let (name, r) = lef_rectangle(g)?;
            if let Some(&l) = indices.get(name) {
                push(
                    l,
                    routing::shifted(r, v.at)?,
                    Some(supply_net(v.polarity)?),
                    ObstacleOwner::Supply,
                )?;
            }
        }
    }
    for p in &state.supply.ports {
        if let Some(&l) = indices.get(p.layer.as_str()) {
            push(
                l,
                p.rect,
                Some(supply_net(p.polarity)?),
                ObstacleOwner::Supply,
            )?;
        }
    }
    let access_layer = *indices
        .get(config.access_layer.as_str())
        .ok_or_else(|| fail("configured access layer absent"))?;
    Ok((
        RouteInput {
            die: design.die_area,
            grid,
            step: config.grid_step,
            access_layer,
            layers,
            vias,
            obstacles,
            terminals,
            rules: config.lef_rules,
            access_classes: classes,
        },
        owners,
    ))
}
/// Relative (origin 0,0) pin and same-master other-net geometry of one
/// (master, orientation, pin) access class on the selected layers.
pub(crate) fn access_class(
    master: &rustroad_core::connectivity::Master,
    orientation: rustroad_core::Orientation,
    height: i64,
    pin_index: usize,
    indices: &HashMap<&str, u32>,
    layers: &[RouteLayer],
) -> Result<AccessClass> {
    let mut class = AccessClass {
        shapes: Vec::new(),
        blockers: Vec::new(),
    };
    let mut add = |g: &Geometry, own: bool| -> Result<()> {
        let layer = match g {
            Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
        };
        if let Some(&li) = indices.get(layer.as_str()) {
            let (_, r) = rectangle(g)?;
            let rect = rustroad_core::transform_rect(r, 0, 0, height, orientation)?;
            let shape = LayerRect { layer: li, rect };
            if own {
                if layers[li as usize].vertical.is_some() {
                    class.shapes.push(shape);
                }
            } else {
                class.blockers.push(shape);
            }
        }
        Ok(())
    };
    for (ti, pin) in master.pins.iter().enumerate() {
        for g in pin.ports.iter().flatten() {
            add(g, ti == pin_index)?;
        }
    }
    for g in &master.obstructions {
        add(g, false)?;
    }
    Ok(class)
}

pub fn route(
    path: &Path,
    base_rev: &str,
    config_path: &Path,
    scope: RouteScope,
    limits: &Limits,
) -> Result<Summary> {
    let lock = crate::lock(path, false, true)?;
    let current = crate::head(path)?;
    crate::check_base(base_rev, &current)?;
    let snapshot = crate::load(path, lock, limits)?;
    if snapshot.metadata().stage != Stage::Placed {
        return Err(fail("place before routing"));
    }
    let mut metadata = snapshot.metadata().clone();
    let placed: Vec<_> = snapshot.records().collect();
    let bundle = metadata
        .physical
        .as_mut()
        .ok_or_else(|| fail("physical design required"))?;
    let (text, source) = crate::pdk::read_input(config_path, 1024 * 1024, "routing_config")?;
    let config: RoutingConfig = serde_json::from_str(&text)?;
    let input = build_input(&metadata.design, bundle, &placed, &config, scope)?;
    let routes = routing::route_cpu_ref(&input, &config.limits)?;
    bundle.routing = Some(RoutingState {
        source,
        config,
        scope,
        layers: input.layers.iter().map(|l| l.name.clone()).collect(),
        via_names: input.vias.iter().map(|v| v.name.clone()).collect(),
        via_lower_layers: input.vias.iter().map(|v| v.lower).collect(),
        routes,
    });
    metadata.format_version = 5;
    metadata.parent_revision = current;
    crate::commit(path, metadata, &placed)
}
pub(crate) fn validate_state(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
) -> Result<()> {
    if let Some(state) = &bundle.routing {
        if state.source.bytes > 1024 * 1024
            || state.source.sha256.len() != 64
            || !state.source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(fail("routing source pin invalid"));
        }
        let input = build_input(design, bundle, placed, &state.config, state.scope)?;
        if state.layers
            != input
                .layers
                .iter()
                .map(|l| l.name.clone())
                .collect::<Vec<_>>()
            || state.via_names
                != input
                    .vias
                    .iter()
                    .map(|v| v.name.clone())
                    .collect::<Vec<_>>()
            || state.via_lower_layers != input.vias.iter().map(|v| v.lower).collect::<Vec<_>>()
        {
            return Err(fail("routing technology names changed"));
        }
        routing::validate(&input, &state.routes, &state.config.limits)?;
    }
    Ok(())
}
/// Lower/upper routing-layer index of every stored via kind, re-derived from
/// the imported technology; the stored names and lower layers must agree.
pub(crate) fn via_layer_pairs(
    bundle: &PhysicalBundle,
    state: &RoutingState,
) -> Result<Vec<(u32, u32)>> {
    let (layers, vias, _) = route_technology(bundle, &state.config)?;
    if state.layers != layers.iter().map(|l| l.name.clone()).collect::<Vec<_>>()
        || state.via_names != vias.iter().map(|v| v.name.clone()).collect::<Vec<_>>()
        || state.via_lower_layers != vias.iter().map(|v| v.lower).collect::<Vec<_>>()
    {
        return Err(fail("routing technology names changed"));
    }
    Ok(vias.iter().map(|v| (v.lower, v.upper)).collect())
}
/// Export view of the stored wires: maximal collinear same-net same-layer runs,
/// split only at via landings, pin-access points and same-layer branches
/// (`rustroad_core::wire_runs`). The metal union equals the stored records'.
pub(crate) fn export_runs(
    bundle: &PhysicalBundle,
    state: &RoutingState,
) -> Result<Vec<routing::Wire>> {
    let pairs = via_layer_pairs(bundle, state)?;
    let routes = &state.routes;
    Ok(rustroad_core::wire_runs::collinear_runs(
        &routes.wires.iter().collect::<Vec<_>>(),
        &routes.vias.iter().collect::<Vec<_>>(),
        &pairs,
        &routes.access.iter().collect::<Vec<_>>(),
        state.config.limits.max_comparisons,
    )?)
}
pub(crate) fn table_bytes(state: &RoutingState) -> Result<Vec<u8>> {
    let n = state
        .routes
        .wires
        .len()
        .checked_add(state.routes.vias.len())
        .and_then(|n| n.checked_add(state.routes.access.len()))
        .and_then(|n| n.checked_add(state.routes.patches.len()))
        .and_then(|n| n.checked_mul(64))
        .ok_or_else(|| Error::Budget("routing table overflow".into()))?;
    if n > crate::MAX_METADATA_BYTES as usize {
        return Err(Error::Budget("routing table byte cap".into()));
    }
    let mut data = Vec::with_capacity(n);
    let mut record = |v: [i64; 8]| {
        for x in v {
            data.extend_from_slice(&x.to_le_bytes());
        }
    };
    for w in &state.routes.wires {
        record([
            1,
            w.net as i64,
            w.layer as i64,
            w.start[0],
            w.start[1],
            w.end[0],
            w.end[1],
            0,
        ]);
    }
    for v in &state.routes.vias {
        record([2, v.net as i64, v.kind as i64, v.at[0], v.at[1], 0, 0, 0]);
    }
    for p in &state.routes.access {
        record([
            3,
            p.net as i64,
            p.pin as i64,
            p.layer as i64,
            p.at[0],
            p.at[1],
            0,
            0,
        ]);
    }
    for p in &state.routes.patches {
        record([
            4,
            p.net as i64,
            p.layer as i64,
            p.rect.lx,
            p.rect.ly,
            p.rect.ux,
            p.rect.uy,
            0,
        ]);
    }
    Ok(data)
}
pub(crate) fn write_def_paths(
    state: &RoutingState,
    wires: &[&routing::Wire],
    vias: &[&routing::Via],
    patches: &[&routing::Patch],
    out: &mut impl Write,
) -> Result<()> {
    write_paths(
        &state.layers,
        &state.via_names,
        &state.via_lower_layers,
        wires,
        vias,
        patches,
        out,
    )
}
pub(crate) fn write_paths(
    layers: &[String],
    via_names: &[String],
    via_lower_layers: &[u32],
    wires: &[&routing::Wire],
    vias: &[&routing::Via],
    patches: &[&routing::Patch],
    out: &mut impl Write,
) -> Result<()> {
    let mut first = true;
    for w in wires {
        write!(
            out,
            "\n  {} {} ( {} {} ) ( {} {} )",
            if first { "+ ROUTED" } else { "NEW" },
            rustroad_core::def_name(&layers[w.layer as usize]),
            w.start[0],
            w.start[1],
            w.end[0],
            w.end[1]
        )?;
        first = false;
    }
    for v in vias {
        let name = &via_names[v.kind as usize];
        let lower = &layers[via_lower_layers[v.kind as usize] as usize];
        write!(
            out,
            "\n  {} {} ( {} {} ) {}",
            if first { "+ ROUTED" } else { "NEW" },
            rustroad_core::def_name(lower),
            v.at[0],
            v.at[1],
            rustroad_core::def_name(name)
        )?;
        first = false;
    }
    // DEF 5.8 routing RECT, relative to its anchor point (the lower-left corner).
    for p in patches {
        write!(
            out,
            "\n  {} {} ( {} {} ) RECT ( 0 0 {} {} )",
            if first { "+ ROUTED" } else { "NEW" },
            rustroad_core::def_name(&layers[p.layer as usize]),
            p.rect.lx,
            p.rect.ly,
            p.rect.ux - p.rect.lx,
            p.rect.uy - p.rect.ly
        )?;
        first = false;
    }
    Ok(())
}
