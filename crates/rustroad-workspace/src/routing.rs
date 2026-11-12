//! Revisioned routing bridge. Geometry/search live in the independent CPU view.
use crate::pdk::{PhysicalBundle, SourcePin};
use crate::{Error, Result, Stage, Summary};
use rustroad_core::connectivity::{Direction, Geometry, PinOwner, PinUse};
use rustroad_core::routing::{
    self, AccessClass, Blockage, InstanceObstacles, LayerRect, ObstacleInstance, ObstacleTemplate,
    RouteInput, RouteLayer, RouteLimits, RouteVia, Routes, TemplateObstacle, Terminal,
};
use rustroad_core::{Design, Limits, Placed, Rect};
use rustroad_formats::lef::LefGeometry;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerPolicy {
    pub name: String,
    pub spacing: i64,
    /// Wide-metal spacing row (LEF SPACINGTABLE PARALLELRUNLENGTH), DBU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wide_spacing: Option<rustroad_core::routing::WideSpacing>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViaPolicy {
    pub name: String,
    pub cut_spacing: i64,
}
/// One per-layer override of the LEF track PITCH/OFFSET (DBU at import).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackOverride {
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
}
fn default_global_iterations() -> usize {
    20
}
fn default_capacity_percent() -> u32 {
    80
}
fn default_guide_margin() -> usize {
    1
}
fn default_expand_margin() -> usize {
    4
}
fn default_ripups() -> usize {
    24
}
fn default_repair_rounds() -> usize {
    200_000
}
fn default_wrong_way() -> u64 {
    4
}
fn default_heuristic_weight() -> u64 {
    1
}
fn is_default_heuristic_weight(value: &u64) -> bool {
    *value == 1
}
fn default_memory_mib() -> u64 {
    2048
}
fn default_history_entries() -> usize {
    routing::DEFAULT_MAX_HISTORY_ENTRIES
}
fn default_history(value: &usize) -> bool {
    *value == default_history_entries()
}
/// Track router (`rustroad_core::routing::TrackPlan`): per-layer tracks from
/// LEF PITCH/OFFSET, GCell global routing, rip-up and reroute. Requires
/// `lef_rules`. Budgets: docs/scaling.md.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackRouterConfig {
    /// Lowest signal-routing layer. Lower layers remain usable by pin access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_routing_layer: Option<String>,
    /// Highest signal-routing layer; omitted uses the highest selected layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_routing_layer: Option<String>,
    /// GCell side in DBU.
    pub gcell: i64,
    /// Path cost of one via (DBU-equivalent).
    pub via_cost: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<TrackOverride>,
    #[serde(default = "default_global_iterations")]
    pub global_iterations: usize,
    #[serde(default = "default_capacity_percent")]
    pub capacity_percent: u32,
    #[serde(default = "default_guide_margin")]
    pub guide_margin: usize,
    #[serde(default = "default_expand_margin")]
    pub expand_margin: usize,
    #[serde(default = "default_ripups")]
    pub max_ripups_per_net: usize,
    #[serde(default = "default_repair_rounds")]
    pub max_repair_rounds: usize,
    #[serde(default = "default_wrong_way")]
    pub wrong_way_factor: u64,
    /// Detailed-search priority weight (1..=16); above one favors progress.
    #[serde(
        default = "default_heuristic_weight",
        skip_serializing_if = "is_default_heuristic_weight"
    )]
    pub heuristic_weight: u64,
    /// Optional soft-search priority weight; hard searches retain heuristic_weight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_heuristic_weight: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_owner_repair: Option<routing::SingleOwnerRepairConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_pin_index: Option<routing::OwnPinIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_index: Option<routing::InstanceIndexMode>,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(
        default = "default_history_entries",
        skip_serializing_if = "default_history"
    )]
    pub max_history_entries: usize,
    /// Routed stacks need the minimum-area run in every search.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub area_stacks: bool,
    /// Optional detailed-route quality controls; omitted keeps the legacy
    /// search (`rustroad_core::routing::RouteQor`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qor: Option<RouteQorConfig>,
}
/// Detailed-route quality controls by layer name (see
/// `rustroad_core::routing::RouteQor`). Unlisted layers keep 1000 milli.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteQorConfig {
    /// Initial detailed routing joins terminals in Prim (nearest-first) order.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prim_order: bool,
    /// Drop a routed net's pin-access via envelopes (search geometry only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub release_portals: bool,
    /// Drop every pin-access via envelope before detailed routing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub drop_portals: bool,
    /// Pin access of small nets leans toward the net's other terminals.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub net_aware_access: bool,
    /// Wire-step cost multiplier per routing layer, thousandths (>= 1000).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layer_cost_milli: BTreeMap<String, u32>,
    /// Via cost multiplier by the via's upper layer, thousandths (>= 1000).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub via_cost_milli: BTreeMap<String, u32>,
    /// Surcharge of a same-layer turn (DBU-equivalent).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bend_cost: u64,
    /// Hard searches of nets with at most this many terminals use
    /// `small_net_weight` (0 disables).
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub small_net_terminals: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub small_net_weight: u64,
    /// Same-net notch repair with the stair bridge, clipped bars and
    /// reserved fills (off keeps the legacy bars).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notch_bridge: bool,
    /// The wide-metal cycle cut of rule repair also runs for strict owners,
    /// at most three cuts per wide region.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_wide_cycles: bool,
    /// Post-route via reduction (`rustroad_core::routing::ViaReduction`);
    /// omitted keeps the routes of the repair loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_reduction: Option<rustroad_core::routing::ViaReduction>,
    /// One via per net, layer pair and point: pin escapes of one net share
    /// a via they land at one point, and the output writes one
    /// (`rustroad_core::routing::RouteQor::dedupe_vias`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dedupe_vias: bool,
    /// Post-route topology cleanup (`rustroad_core::routing::Straighten`);
    /// omitted keeps the routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub straighten: Option<rustroad_core::routing::Straighten>,
    /// `export-def` declares the track grid this router used: one `TRACKS`
    /// statement per axis and routing layer (`def_tracks`), as DEF 5.8 places
    /// them after the rows. Output only; the route is unchanged. Omitted
    /// keeps the DEF without track statements.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub def_tracks: bool,
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
fn is_zero_usize(value: &usize) -> bool {
    *value == 0
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
    /// Track router; absent keeps the lattice router (previous behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_router: Option<TrackRouterConfig>,
    /// Opt-in whole-net batches; completed metal stays fixed for later batches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_terminals: Option<usize>,
}
impl RoutingConfig {
    fn validate(&self) -> Result<()> {
        if self.batch_terminals.is_some_and(|n| {
            self.track_router.is_none()
                || !(2..=rustroad_core::route_batches::MAX_BATCH_TERMINALS).contains(&n)
        }) {
            return Err(Error::Invalid(
                "invalid routing batch terminal limit".into(),
            ));
        }
        if let Some(t) = &self.track_router {
            self.limits.validate_track()?;
            if !self.lef_rules
                || t.max_history_entries == 0
                || t.max_history_entries > 64_000_000
                || t.tracks.len() > 16
                || t.tracks
                    .iter()
                    .any(|o| !rustroad_core::valid_object_name(&o.layer))
            {
                return Err(Error::Invalid(
                    "track router requires lef_rules and valid track overrides".into(),
                ));
            }
        } else {
            self.limits.validate()?;
        }
        if self.layers.len() < 2
            || self.layers.len() > 8
            || self.vias.len() > 12
            || self.grid_step <= 0
            || !self.max_skew_ns.is_finite()
            || self.max_skew_ns <= 0.0
            || self.layers.iter().any(|l| {
                !rustroad_core::valid_object_name(&l.name)
                    || l.spacing < 0
                    || l.wide_spacing.is_some_and(|w| {
                        !(1..=1_000_000).contains(&w.width)
                            || !(0..=10_000_000).contains(&w.parallel_run)
                            || !(l.spacing..=1_000_000).contains(&w.spacing)
                    })
            })
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
    /// Schema 5: the full records. Schema 6: empty record lists (stats
    /// only); the records live in the authoritative `routes.rbin`.
    pub routes: Routes,
    /// Schema 6 only: identity of the authoritative `routes.rbin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<RoutesTable>,
}
/// Identity and record counts of `routes.rbin` (64-byte i64 LE records:
/// 1 wire, 2 via, 3 access, 4 patch, 5 routed net).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesTable {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    pub wires: usize,
    pub vias: usize,
    pub access: usize,
    pub patches: usize,
    pub nets: usize,
}
/// Byte cap of the streamed authoritative route table.
pub const MAX_ROUTE_TABLE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub(crate) const ROUTE_TABLE: &str = "routes.rbin";
/// Schema 6 route table: the schema-5 records followed by one net record each.
pub(crate) fn write_route_table(routes: &Routes, path: &Path) -> Result<RoutesTable> {
    use sha2::{Digest, Sha256};
    let count = [
        routes.wires.len(),
        routes.vias.len(),
        routes.access.len(),
        routes.patches.len(),
        routes.nets.len(),
    ]
    .into_iter()
    .try_fold(0usize, |sum, n| sum.checked_add(n))
    .ok_or_else(|| Error::Budget("route record count overflow".into()))?;
    let bytes = (count as u64)
        .checked_mul(64)
        .ok_or_else(|| Error::Budget("route table length overflow".into()))?;
    if bytes > MAX_ROUTE_TABLE_BYTES {
        return Err(Error::Budget("route table byte cap".into()));
    }
    struct Output {
        file: BufWriter<std::fs::File>,
        hash: Sha256,
    }
    impl Write for Output {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.file.write_all(data)?;
            self.hash.update(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    let mut output = Output {
        file: BufWriter::with_capacity(256 * 1024, file),
        hash: Sha256::new(),
    };
    records(routes, &mut output)?;
    for &net in &routes.nets {
        write_record([5, i64::from(net), 0, 0, 0, 0, 0, 0], &mut output)?;
    }
    output.flush()?;
    output.file.get_ref().sync_all()?;
    Ok(RoutesTable {
        file: ROUTE_TABLE.into(),
        bytes,
        sha256: format!("{:x}", output.hash.finalize()),
        wires: routes.wires.len(),
        vias: routes.vias.len(),
        access: routes.access.len(),
        patches: routes.patches.len(),
        nets: routes.nets.len(),
    })
}
/// Lowercase hex SHA-256 (the `RoutesTable::sha256` form).
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
/// How `decode_route_table_with` establishes the table's SHA-256 identity.
pub(crate) enum RouteSha {
    /// Hash the bytes now.
    Compute,
    /// SHA-256 already computed over exactly these bytes.
    Known(String),
    /// The bytes are bound by a matching validation receipt (XXH3-128 of the
    /// file that passed full validation, whose design.json pins the SHA-256).
    Receipt,
}
/// Decode a schema 6 route table; every count must match `table`. `sha`
/// says how the table's SHA-256 identity is established.
pub(crate) fn decode_route_table_with(
    bytes: &[u8],
    table: &RoutesTable,
    sha: RouteSha,
) -> Result<Routes> {
    let bad = |s: &str| Error::Invalid(format!("route table: {s}"));
    if bytes.len() as u64 != table.bytes
        || !bytes.len().is_multiple_of(64)
        || bytes.len() as u64 > MAX_ROUTE_TABLE_BYTES
    {
        return Err(bad("length"));
    }
    let digest = match sha {
        RouteSha::Compute => Some(sha256_hex(bytes)),
        RouteSha::Known(hex) => Some(hex),
        RouteSha::Receipt => None,
    };
    if digest.is_some_and(|hex| hex != table.sha256) {
        return Err(bad("sha256 differs from design.json"));
    }
    let count = [
        table.wires,
        table.vias,
        table.access,
        table.patches,
        table.nets,
    ]
    .into_iter()
    .try_fold(0usize, |sum, n| sum.checked_add(n))
    .ok_or_else(|| bad("record count overflow"))?;
    if count != bytes.len() / 64 {
        return Err(bad("record counts disagree with file size"));
    }
    let mut routes = Routes {
        wires: Vec::new(),
        vias: Vec::new(),
        access: Vec::new(),
        patches: Vec::new(),
        nets: Vec::new(),
        stats: Default::default(),
    };
    routes
        .wires
        .try_reserve_exact(table.wires)
        .map_err(|_| bad("wire allocation"))?;
    routes
        .vias
        .try_reserve_exact(table.vias)
        .map_err(|_| bad("via allocation"))?;
    routes
        .access
        .try_reserve_exact(table.access)
        .map_err(|_| bad("access allocation"))?;
    routes
        .patches
        .try_reserve_exact(table.patches)
        .map_err(|_| bad("patch allocation"))?;
    routes
        .nets
        .try_reserve_exact(table.nets)
        .map_err(|_| bad("net allocation"))?;
    let u32_of = |v: i64| u32::try_from(v).map_err(|_| bad("index out of range"));
    let mut last = 0i64;
    for chunk in bytes.chunks_exact(64) {
        let mut r = [0i64; 8];
        for (i, b) in chunk.chunks_exact(8).enumerate() {
            r[i] = i64::from_le_bytes(b.try_into().unwrap());
        }
        if r[0] < last || !(1..=5).contains(&r[0]) {
            return Err(bad("record kind/order"));
        }
        last = r[0];
        match r[0] {
            1 if routes.wires.len() >= table.wires => return Err(bad("too many wire records")),
            2 if routes.vias.len() >= table.vias => return Err(bad("too many via records")),
            3 if routes.access.len() >= table.access => return Err(bad("too many access records")),
            4 if routes.patches.len() >= table.patches => {
                return Err(bad("too many patch records"));
            }
            5 if routes.nets.len() >= table.nets => return Err(bad("too many net records")),
            1 => routes.wires.push(routing::Wire {
                net: u32_of(r[1])?,
                layer: u32_of(r[2])?,
                start: [r[3], r[4]],
                end: [r[5], r[6]],
            }),
            2 => routes.vias.push(routing::Via {
                net: u32_of(r[1])?,
                kind: u32_of(r[2])?,
                at: [r[3], r[4]],
            }),
            3 => routes.access.push(routing::Access {
                net: u32_of(r[1])?,
                pin: u32_of(r[2])?,
                layer: u32_of(r[3])?,
                at: [r[4], r[5]],
            }),
            4 => routes.patches.push(routing::Patch {
                net: u32_of(r[1])?,
                layer: u32_of(r[2])?,
                rect: Rect {
                    lx: r[3],
                    ly: r[4],
                    ux: r[5],
                    uy: r[6],
                },
            }),
            _ => routes.nets.push(u32_of(r[1])?),
        }
    }
    if routes.wires.len() != table.wires
        || routes.vias.len() != table.vias
        || routes.access.len() != table.access
        || routes.patches.len() != table.patches
        || routes.nets.len() != table.nets
    {
        return Err(bad("record counts differ from design.json"));
    }
    Ok(routes)
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
    let wanted: HashSet<&str> = report.buffers.iter().map(String::as_str).collect();
    let mut buffers = vec![false; bundle.netlist.instances.len()];
    let mut found = 0usize;
    for (i, instance) in bundle.netlist.instances.iter().enumerate() {
        if wanted.contains(instance.name.as_str()) {
            buffers[i] = true;
            found += 1;
        }
    }
    if found != report.buffers.len() {
        return Err(fail("clock buffer identity missing or repeated"));
    }
    let model = &bundle.connectivity;
    let mut nets = BTreeSet::new();
    // A selected gated root is also a clock net even though its driver is
    // an existing functional gate instead of an inserted clock buffer.
    nets.insert(crate::clock::root_net(bundle, cfg)?);
    for p in &model.pins {
        let source = match p.owner {
            PinOwner::Port { port, .. } => model.ports[port as usize].name == cfg.port,
            PinOwner::Instance { instance, terminal } => {
                buffers[instance as usize]
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
                wide_spacing: None,
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
            wide_spacing: policy.wide_spacing,
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
    build_input_impl(design, bundle, placed, config, scope, GeometryView::Routing)
        .map(|(input, _)| input)
}

/// Same routing input plus one owner per obstacle (parallel vector).
pub(crate) fn build_input_with_owners(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    config: &RoutingConfig,
    scope: RouteScope,
) -> Result<(RouteInput, Vec<ObstacleOwner>)> {
    build_input_impl(
        design,
        bundle,
        placed,
        config,
        scope,
        GeometryView::Explicit,
    )
}

/// Shared master geometry with global pin ownership, without expanding cells.
pub(crate) fn build_compact_input_with_owners(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    config: &RoutingConfig,
    scope: RouteScope,
) -> Result<(RouteInput, Vec<ObstacleOwner>)> {
    build_input_impl(
        design,
        bundle,
        placed,
        config,
        scope,
        GeometryView::Extraction,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GeometryView {
    Routing,
    Explicit,
    Extraction,
}

/// Keep the exact LEF geometry in the workspace. In the routing view only,
/// supplement the ordinary spacing by the positive difference to explicit
/// OBS spacing. A stricter configured layer spacing remains conservative.
fn obstruction_rect(
    master: &rustroad_core::connectivity::Master,
    geometry: &Geometry,
    layer: &RouteLayer,
) -> Result<Rect> {
    let (name, mut rect) = rectangle(geometry)?;
    let extra = master
        .obstruction_spacing
        .get(name)
        .copied()
        .unwrap_or(layer.spacing)
        .saturating_sub(layer.spacing)
        .max(0);
    rect.lx = rect
        .lx
        .checked_sub(extra)
        .ok_or_else(|| fail("OBS spacing overflow"))?;
    rect.ly = rect
        .ly
        .checked_sub(extra)
        .ok_or_else(|| fail("OBS spacing overflow"))?;
    rect.ux = rect
        .ux
        .checked_add(extra)
        .ok_or_else(|| fail("OBS spacing overflow"))?;
    rect.uy = rect
        .uy
        .checked_add(extra)
        .ok_or_else(|| fail("OBS spacing overflow"))?;
    Ok(rect)
}

fn obstacle_templates(
    masters: &[rustroad_core::connectivity::Master],
    indices: &HashMap<&str, u32>,
    layers: &[RouteLayer],
    block_obstructions: bool,
) -> Result<Vec<ObstacleTemplate>> {
    masters
        .iter()
        .map(|m| {
            let mut shapes = Vec::new();
            let mut add = |geometry: &Geometry, pin: Option<u32>| -> Result<()> {
                let name = match geometry {
                    Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
                };
                if let Some(&layer) = indices.get(name.as_str()) {
                    let rect = if pin.is_none() {
                        obstruction_rect(m, geometry, &layers[layer as usize])?
                    } else {
                        rectangle(geometry)?.1
                    };
                    shapes.push(TemplateObstacle { layer, rect, pin });
                }
                Ok(())
            };
            for (pin, p) in m.pins.iter().enumerate() {
                for g in p.ports.iter().flatten() {
                    add(g, Some(pin as u32))?;
                }
            }
            if block_obstructions
                || !m
                    .class
                    .as_deref()
                    .is_some_and(|c| c.split_whitespace().next() == Some("BLOCK"))
            {
                for g in &m.obstructions {
                    add(g, None)?;
                }
            }
            ObstacleTemplate::new(m.size[1], m.pins.len() as u32, shapes).map_err(Into::into)
        })
        .collect()
}

fn build_input_impl(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    config: &RoutingConfig,
    scope: RouteScope,
    view: GeometryView,
) -> Result<(RouteInput, Vec<ObstacleOwner>)> {
    let compact_instances = view != GeometryView::Explicit;
    let instance_owners = view == GeometryView::Extraction;
    let trace = std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some();
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
    let templates = if compact_instances {
        obstacle_templates(
            &model.masters,
            &indices,
            &layers,
            view != GeometryView::Extraction,
        )?
    } else {
        Vec::new()
    };
    let mut instance_records = Vec::new();
    let mut instance_pin_nets = Vec::new();
    let mut instance_pin_ids = Vec::new();
    if compact_instances {
        instance_records
            .try_reserve_exact(placed.len())
            .map_err(|_| Error::Budget("instance geometry allocation".into()))?;
        instance_pin_nets
            .try_reserve_exact(model.pins.len())
            .map_err(|_| Error::Budget("instance pin-net allocation".into()))?;
        if instance_owners {
            instance_pin_ids
                .try_reserve_exact(model.pins.len())
                .map_err(|_| Error::Budget("instance pin-identity allocation".into()))?;
        }
    }
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
    let pin_ids =
        rustroad_core::connectivity::PinIndex::new(model, &crate::pdk::connection_limits(design)?)?;
    if trace {
        rustroad_core::memory::trace("route bridge pin lookup");
    }
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
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| fail("physical config absent"))?;
    // Rail-input tie pins: router net nets.len() + k joins pin k to the
    // instance's own rail (all-net scope only).
    let tie_index: HashMap<u32, (u32, u32)> = if scope == RouteScope::All {
        crate::physical::rail_tie_pins(bundle)
            .into_iter()
            .enumerate()
            .map(|(k, (pin, supply))| (pin, (k as u32, supply)))
            .collect()
    } else {
        HashMap::new()
    };
    let rail_layer = indices.get(cfg.rail_layer.as_str()).copied();
    let mut ties = Vec::new();
    let mut tie_terminals = Vec::new();
    for (i, p) in placed.iter().enumerate() {
        if trace && i.is_multiple_of(1_000_000) {
            eprintln!(
                "[route] bridge instances {i}/{} terminals {}",
                placed.len(),
                terminals.len()
            );
        }
        let master = &model.masters[model.instance_masters[i] as usize];
        let orientation = rustroad_core::placed_orientation_for(design, &design.cells[i], p)?;
        let pin_start = u32::try_from(instance_pin_nets.len())
            .map_err(|_| fail("instance pin offset overflow"))?;
        let mut pending: Vec<(u32, u32)> = Vec::new();
        for (ti, pin) in master.pins.iter().enumerate() {
            let owner = PinOwner::Instance {
                instance: i as u32,
                terminal: ti as u32,
            };
            let pin_id = pin_ids.get(owner)?;
            let tie = tie_index.get(&pin_id).copied();
            let net = match tie {
                Some((k, _)) => Some(tie_net(model, k)?),
                None => model.pins[pin_id as usize].net,
            };
            if compact_instances {
                instance_pin_nets.push(net.unwrap_or(u32::MAX));
                if instance_owners {
                    instance_pin_ids.push(pin_id);
                }
            }
            let selected_pin = !pin.use_.is_supply()
                && net.is_some_and(|n| {
                    tie.is_some()
                        || if scope == RouteScope::All {
                            !model.nets[n as usize].use_.is_supply()
                        } else {
                            selected.contains(&n)
                        }
                });
            let mut shapes = Vec::new();
            for geometry in pin
                .ports
                .iter()
                .flatten()
                .filter(|_| !compact_instances || selected_pin)
            {
                let layer = match geometry {
                    Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
                };
                let Some(&li) = indices.get(layer.as_str()) else {
                    continue;
                };
                let (_, r) = rectangle(geometry)?;
                let r = rustroad_core::transform_rect(r, p.x, p.y, p.height, orientation)?;
                if !compact_instances {
                    push(li, r, net, ObstacleOwner::InstancePin(pin_id))?;
                }
                if layers[li as usize].vertical.is_some() {
                    shapes.push(LayerRect { layer: li, rect: r });
                }
            }
            if let Some((k, supply)) = tie {
                pending.push((k, supply));
            }
            if let Some(n) = net
                && selected_pin
            {
                let class = if config.lef_rules && view != GeometryView::Extraction {
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
                    pin: pin_id,
                    net: n,
                    driver: pin.direction == Some(Direction::Output),
                    shapes,
                    class,
                });
            }
        }
        if compact_instances {
            instance_records.push(ObstacleInstance {
                template: model.instance_masters[i],
                pin_start,
                at: [p.x, p.y],
                orientation,
            });
        }
        for geometry in master.obstructions.iter().filter(|_| !compact_instances) {
            let layer = match geometry {
                Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
            };
            if let Some(&li) = indices.get(layer.as_str()) {
                let r = obstruction_rect(master, geometry, &layers[li as usize])?;
                push(
                    li,
                    rustroad_core::transform_rect(r, p.x, p.y, p.height, orientation)?,
                    None,
                    ObstacleOwner::InstanceObstruction,
                )?;
            }
        }
        // Each tie pin's pseudo-driver: the largest rail-layer rectangle of
        // this instance's own supply pin of the tied polarity.
        for (k, supply) in pending {
            let rail = rail_layer.ok_or_else(|| fail("rail layer not selected for routing"))?;
            let want = if model.nets[supply as usize].use_ == PinUse::Power {
                &cfg.power_pin
            } else {
                &cfg.ground_pin
            };
            let mut zone: Option<Rect> = None;
            for pin in master.pins.iter().filter(|p| &p.name == want) {
                for geometry in pin.ports.iter().flatten() {
                    let (name, r) = rectangle(geometry)?;
                    if name != cfg.rail_layer {
                        continue;
                    }
                    let r = rustroad_core::transform_rect(r, p.x, p.y, p.height, orientation)?;
                    let area = |r: &Rect| i128::from(r.ux - r.lx) * i128::from(r.uy - r.ly);
                    if zone.is_none_or(|z| area(&r) > area(&z)) {
                        zone = Some(r);
                    }
                }
            }
            let zone =
                zone.ok_or_else(|| fail("tie instance has no rail-layer shape on its supply pin"))?;
            let net = tie_net(model, k)?;
            tie_terminals.push(Terminal {
                pin: u32::try_from(model.pins.len() + k as usize)
                    .map_err(|_| fail("tie pin id overflow"))?,
                net,
                driver: true,
                shapes: vec![LayerRect {
                    layer: rail,
                    rect: zone,
                }],
                class: None,
            });
            ties.push(routing::SupplyTie {
                net,
                supply,
                zone: LayerRect {
                    layer: rail,
                    rect: zone,
                },
            });
        }
    }
    if !ties.is_empty() && config.track_router.is_none() {
        return Err(fail("rail-input tie cells need the track router"));
    }
    terminals.extend(tie_terminals);
    ties.sort_by_key(|t| t.net);
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
            let pin_id = pin_ids.get(owner)?;
            let net = model.pins[pin_id as usize].net;
            push(layer, p.rect, net, ObstacleOwner::PortPin(pin_id))?;
            if let Some(net) = net
                && selected.contains(&net)
            {
                terminals.push(Terminal {
                    pin: pin_id,
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
    let find_supply = |name: &str| -> Result<u32> {
        model
            .nets
            .iter()
            .position(|n| n.name == name)
            .map(|i| i as u32)
            .ok_or_else(|| fail("supply net absent"))
    };
    let power_net = find_supply(&cfg.power_net)?;
    let ground_net = find_supply(&cfg.ground_net)?;
    let supply_net = |p: rustroad_core::power::RailPolarity| -> Result<u32> {
        Ok(if p == rustroad_core::power::RailPolarity::Power {
            power_net
        } else {
            ground_net
        })
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
    let tracks = config
        .track_router
        .as_ref()
        .map(|t| track_plan(bundle, t, &layers))
        .transpose()?;
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
            tracks,
            ties,
            instances: if compact_instances {
                let mut geometry =
                    InstanceObstacles::new(templates, instance_records, instance_pin_nets)?;
                if instance_owners {
                    geometry = geometry.with_pin_ids(instance_pin_ids)?;
                }
                Some(geometry)
            } else {
                None
            },
            held_bytes: 0,
        },
        owners,
    ))
}

/// Router net id of rail tie `k` (after every connectivity net).
pub(crate) fn tie_net(model: &rustroad_core::connectivity::ConnectedDesign, k: u32) -> Result<u32> {
    u32::try_from(model.nets.len() + k as usize).map_err(|_| fail("tie net id overflow"))
}

/// Exported net of every stored route net id: connectivity nets map to
/// themselves, rail tie `k` (id `nets + k`) to its supply net.
pub(crate) struct ExportNets {
    nets: usize,
    ties: Vec<u32>,
}
impl ExportNets {
    pub(crate) fn new(bundle: &PhysicalBundle) -> Self {
        Self {
            nets: bundle.connectivity.nets.len(),
            ties: crate::physical::rail_tie_pins(bundle)
                .into_iter()
                .map(|(_, supply)| supply)
                .collect(),
        }
    }
    pub(crate) fn of(&self, net: u32) -> Result<u32> {
        if (net as usize) < self.nets {
            return Ok(net);
        }
        self.ties
            .get(net as usize - self.nets)
            .copied()
            .ok_or_else(|| fail("route net outside connectivity and ties"))
    }
}
/// The DEF `TRACKS` statements of the track grid of `config` over `die`, in
/// routing-layer order, X then Y per layer: every coordinate
/// `offset + k * pitch` at which a default-width wire of the layer lies inside
/// the die, which is exactly the track set the track router builds along the
/// layer's preferred axis (`troute::layer_tracks`); the other axis uses the
/// same pitch and offset, as the DEF track grids of other tools do. Empty
/// without a track router.
pub(crate) fn def_tracks(
    bundle: &PhysicalBundle,
    config: &RoutingConfig,
    die: Rect,
) -> Result<Vec<String>> {
    let Some(track) = &config.track_router else {
        return Ok(Vec::new());
    };
    let (layers, _, _) = route_technology(bundle, config)?;
    let plan = track_plan(bundle, track, &layers)?;
    let mut out = Vec::new();
    for (layer, tracks) in layers.iter().zip(&plan.tracks) {
        let (Some(_), Some((pitch, offset))) = (layer.vertical, *tracks) else {
            continue;
        };
        if pitch <= 0 {
            return Err(fail("track pitch must be positive"));
        }
        let half = i128::from(layer.width / 2);
        let (pitch, offset) = (i128::from(pitch), i128::from(offset));
        for (axis, lo, hi) in [("X", die.lx, die.ux), ("Y", die.ly, die.uy)] {
            let (lo, hi) = (i128::from(lo), i128::from(hi));
            let k0 = (lo + half - offset + pitch - 1).div_euclid(pitch);
            let first = offset + k0 * pitch;
            if first + half > hi {
                continue;
            }
            let count = (hi - half - first).div_euclid(pitch) + 1;
            out.push(format!(
                "TRACKS {axis} {first} DO {count} STEP {pitch} LAYER {} ;",
                rustroad_core::def_name(&layer.name)
            ));
        }
    }
    Ok(out)
}

/// Track plan: LEF PITCH/OFFSET per routing layer (vertical: x component,
/// horizontal: y component), replaced by explicit profile overrides.
pub(crate) fn track_plan(
    bundle: &PhysicalBundle,
    config: &TrackRouterConfig,
    layers: &[RouteLayer],
) -> Result<rustroad_core::routing::TrackPlan> {
    let routing_layers = if config.min_routing_layer.is_some() || config.max_routing_layer.is_some()
    {
        let first = layers.iter().position(|l| l.vertical.is_some());
        let last = layers.iter().rposition(|l| l.vertical.is_some());
        let resolve = |name: Option<&str>, default: Option<usize>| {
            name.map_or(default, |name| {
                layers
                    .iter()
                    .position(|l| l.name == name && l.vertical.is_some())
            })
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| fail("signal-routing limit names no selected routing layer"))
        };
        Some([
            resolve(config.min_routing_layer.as_deref(), first)?,
            resolve(config.max_routing_layer.as_deref(), last)?,
        ])
    } else {
        None
    };
    for o in &config.tracks {
        if !layers
            .iter()
            .any(|l| l.name == o.layer && l.vertical.is_some())
        {
            return Err(fail("track override names no routing layer"));
        }
    }
    let mut tracks = Vec::with_capacity(layers.len());
    for l in layers {
        let Some(vertical) = l.vertical else {
            tracks.push(None);
            continue;
        };
        let tech = bundle
            .technology
            .layers
            .iter()
            .find(|t| t.name == l.name)
            .ok_or_else(|| fail("routing layer absent"))?;
        let axis = usize::from(!vertical);
        let over = config.tracks.iter().find(|o| o.layer == l.name);
        let pitch = over
            .and_then(|o| o.pitch)
            .or(tech.pitch.map(|p| p[axis]))
            .ok_or_else(|| fail("routing layer has no LEF PITCH or override"))?;
        let offset = over
            .and_then(|o| o.offset)
            .or(tech.offset.map(|p| p[axis]))
            .unwrap_or(0);
        tracks.push(Some((pitch, offset)));
    }
    Ok(rustroad_core::routing::TrackPlan {
        tracks,
        routing_layers,
        gcell: config.gcell,
        global_iterations: config.global_iterations,
        capacity_percent: config.capacity_percent,
        guide_margin: config.guide_margin,
        expand_margin: config.expand_margin,
        max_ripups_per_net: config.max_ripups_per_net,
        max_repair_rounds: config.max_repair_rounds,
        via_cost: config.via_cost,
        wrong_way_factor: config.wrong_way_factor,
        heuristic_weight: config.heuristic_weight,
        repair_heuristic_weight: config.repair_heuristic_weight,
        single_owner_repair: config.single_owner_repair.clone(),
        own_pin_index: config.own_pin_index,
        instance_index: config.instance_index,
        memory_mib: config.memory_mib,
        max_history_entries: config.max_history_entries,
        area_stacks: config.area_stacks,
        qor: config
            .qor
            .as_ref()
            .map(|q| route_qor(q, layers))
            .transpose()?,
    })
}
/// Layer-name quality controls resolved to input-layer indices.
fn route_qor(
    config: &RouteQorConfig,
    layers: &[RouteLayer],
) -> Result<rustroad_core::routing::RouteQor> {
    let resolve = |named: &BTreeMap<String, u32>| -> Result<Vec<u32>> {
        if named.is_empty() {
            return Ok(Vec::new());
        }
        for name in named.keys() {
            if !layers
                .iter()
                .any(|l| &l.name == name && l.vertical.is_some())
            {
                return Err(fail("route quality layer names no selected routing layer"));
            }
        }
        Ok(layers
            .iter()
            .map(|l| named.get(&l.name).copied().unwrap_or(1000))
            .collect())
    };
    Ok(rustroad_core::routing::RouteQor {
        prim_order: config.prim_order,
        release_portals: config.release_portals,
        drop_portals: config.drop_portals,
        net_aware_access: config.net_aware_access,
        layer_cost_milli: resolve(&config.layer_cost_milli)?,
        via_cost_milli: resolve(&config.via_cost_milli)?,
        bend_cost: config.bend_cost,
        small_net_terminals: config.small_net_terminals,
        small_net_weight: config.small_net_weight,
        notch_bridge: config.notch_bridge,
        strict_wide_cycles: config.strict_wide_cycles,
        via_reduction: config.via_reduction.clone(),
        dedupe_vias: config.dedupe_vias,
        straighten: config.straighten.clone(),
    })
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
    let mut add = |g: &Geometry, own: bool, obstruction: bool| -> Result<()> {
        let layer = match g {
            Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
        };
        if let Some(&li) = indices.get(layer.as_str()) {
            let r = if obstruction {
                obstruction_rect(master, g, &layers[li as usize])?
            } else {
                rectangle(g)?.1
            };
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
            add(g, ti == pin_index, false)?;
        }
    }
    for g in &master.obstructions {
        add(g, false, true)?;
    }
    Ok(class)
}

/// Upfront memory estimate of one track-router run (MiB, rounded up), the
/// cap it was checked against and its parts.
#[derive(Clone, Debug, Serialize)]
pub struct RouteMemory {
    pub estimate_mib: u64,
    pub memory_mib: u64,
    pub workspace_bytes: u64,
    /// Live resident + swapped bytes after decoding, before router allocation.
    /// None means the conservative serialized-size estimate was used.
    pub live_process_bytes: Option<u64>,
    pub held_mib: u64,
    pub search_mib: u64,
    pub spatial_mib: u64,
    pub routed_mib: u64,
    pub check_mib: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batches: Option<rustroad_core::route_batches::BatchReport>,
}
fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

/// An earlier routed workspace revision of the same design for `route_eco`.
#[derive(Clone, Debug)]
pub struct PriorSource<'a> {
    pub workspace: &'a Path,
    pub revision: &'a str,
}

/// What an ECO route offered the router. A net is offered when the prior
/// routed it and every terminal of it has the same driver flag and pin
/// geometry (absolute shapes) in both inputs; the router still re-checks
/// each offered net (escape tops reached, shapes clear) and keeps only those.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcoPrior {
    pub workspace: String,
    pub revision: String,
    pub prior_routed_nets: usize,
    pub offered_nets: usize,
    pub changed_nets: usize,
    pub new_nets: usize,
}

pub fn route(
    path: &Path,
    base_rev: &str,
    config_path: &Path,
    scope: RouteScope,
    limits: &Limits,
) -> Result<Summary> {
    route_eco(path, base_rev, config_path, scope, None, limits)
}

/// Terminals of each net as comparable geometry: (driver, sorted shapes),
/// sorted. Pin and access-class ids are input-local and are not compared.
type NetPins = Vec<(bool, Vec<(u32, i64, i64, i64, i64)>)>;
fn net_pins(input: &RouteInput) -> BTreeMap<u32, NetPins> {
    let mut out: BTreeMap<u32, NetPins> = BTreeMap::new();
    for t in &input.terminals {
        let mut shapes: Vec<_> = t
            .shapes
            .iter()
            .map(|s| (s.layer, s.rect.lx, s.rect.ly, s.rect.ux, s.rect.uy))
            .collect();
        shapes.sort_unstable();
        out.entry(t.net).or_default().push((t.driver, shapes));
    }
    for v in out.values_mut() {
        v.sort_unstable();
    }
    out
}

/// Prior geometry of every net of `input` whose pins are unchanged, keyed by
/// this input's net ids (matched by connectivity net name).
fn prior_routes(
    prior: &PriorSource<'_>,
    config: &RoutingConfig,
    scope: RouteScope,
    bundle: &PhysicalBundle,
    input: &RouteInput,
    limits: &Limits,
) -> Result<(rustroad_core::routing::PriorRoutes, EcoPrior)> {
    let snapshot = crate::read(prior.workspace, limits)?;
    crate::check_base(prior.revision, &crate::head(prior.workspace)?)?;
    let placed: Vec<_> = snapshot.records().collect();
    let metadata = snapshot.metadata();
    let old = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("prior workspace has no physical design"))?;
    let state = old
        .routing
        .as_ref()
        .ok_or_else(|| fail("prior workspace revision is not routed"))?;
    let old_input = build_input(&metadata.design, old, &placed, config, scope)?;
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
        || serde_json::to_value(&old_input.layers)? != serde_json::to_value(&input.layers)?
        || serde_json::to_value(&old_input.vias)? != serde_json::to_value(&input.vias)?
    {
        return Err(fail("prior routing uses another layer or via table"));
    }
    let name_of = |b: &PhysicalBundle, n: u32| -> Result<String> {
        b.connectivity
            .nets
            .get(n as usize)
            .map(|x| x.name.clone())
            .ok_or_else(|| fail("routing net outside connectivity"))
    };
    let mut old_ids = BTreeMap::new();
    for &n in &state.routes.nets {
        old_ids.insert(name_of(old, n)?, n);
    }
    let (new_pins, old_pins) = (net_pins(input), net_pins(&old_input));
    let mut keep: BTreeMap<u32, u32> = BTreeMap::new();
    let (mut changed, mut fresh) = (0usize, 0usize);
    for (&n, pins) in &new_pins {
        if pins.len() < 2 {
            continue;
        }
        match old_ids.get(&name_of(bundle, n)?) {
            Some(&o) if old_pins.get(&o) == Some(pins) => {
                keep.insert(o, n);
            }
            Some(_) => changed += 1,
            None => fresh += 1,
        }
    }
    let mut routes = rustroad_core::routing::PriorRoutes::default();
    for w in &state.routes.wires {
        if let Some(&n) = keep.get(&w.net) {
            let mut w = w.clone();
            w.net = n;
            routes.nets.entry(n).or_default().0.push(w);
        }
    }
    for v in &state.routes.vias {
        if let Some(&n) = keep.get(&v.net) {
            let mut v = v.clone();
            v.net = n;
            routes.nets.entry(n).or_default().1.push(v);
        }
    }
    let report = EcoPrior {
        workspace: prior.workspace.display().to_string(),
        revision: prior.revision.to_string(),
        prior_routed_nets: state.routes.nets.len(),
        offered_nets: routes.nets.len(),
        changed_nets: changed,
        new_nets: fresh,
    };
    Ok((routes, report))
}

/// `route`, optionally reusing the geometry of an earlier routed revision of
/// the same design for every net whose pins did not change (ECO routing).
pub fn route_eco(
    path: &Path,
    base_rev: &str,
    config_path: &Path,
    scope: RouteScope,
    prior: Option<&PriorSource<'_>>,
    limits: &Limits,
) -> Result<Summary> {
    let lock = crate::lock(path, false, true)?;
    let current = crate::head(path)?;
    crate::check_base(base_rev, &current)?;
    rustroad_core::memory::trace("start");
    let (text, source) = crate::pdk::read_input(config_path, 1024 * 1024, "routing_config")?;
    let config: RoutingConfig = serde_json::from_str(&text)?;
    // Track mode: the loaded workspace is part of the memory budget; refuse
    // before loading when it alone cannot fit.
    let workspace_bytes = revision_bytes(path, &current)?;
    if let Some(t) = &config.track_router {
        let load = routing::PROCESS_BASE_BYTES
            .saturating_add(workspace_bytes.saturating_mul(routing::WORKSPACE_LOAD_FACTOR));
        if load > t.memory_mib.saturating_mul(1 << 20) {
            return Err(Error::Budget(format!(
                "route memory: loading {} MiB of workspace needs about {} MiB, above memory_mib {}",
                workspace_bytes >> 20,
                load >> 20,
                t.memory_mib
            )));
        }
    }
    let snapshot = crate::load(path, lock, limits)?;
    if snapshot.metadata().stage != Stage::Placed {
        return Err(fail("place before routing"));
    }
    rustroad_core::memory::trace("loaded");
    let placed: Vec<_> = snapshot.records().collect();
    let (_lock, mut metadata) = snapshot.into_metadata();
    rustroad_core::memory::trace("metadata owned");
    let bundle = metadata
        .physical
        .as_mut()
        .ok_or_else(|| fail("physical design required"))?;
    let mut input = build_input(&metadata.design, bundle, &placed, &config, scope)?;
    let live_process_bytes = rustroad_core::memory::live_bytes();
    input.held_bytes = routing::loaded_held_estimate(workspace_bytes, &input, live_process_bytes);
    rustroad_core::memory::trace("input");
    let mut memory = if config.batch_terminals.is_some() {
        None
    } else {
        match (
            &config.track_router,
            routing::track_memory_estimate(&input, &config.limits)?,
        ) {
            (Some(t), Some(e)) => Some(RouteMemory {
                estimate_mib: mib(e.total()),
                memory_mib: t.memory_mib,
                workspace_bytes,
                live_process_bytes,
                held_mib: mib(e.held),
                search_mib: mib(e.search),
                spatial_mib: mib(e.spatial),
                routed_mib: mib(e.routed),
                check_mib: mib(e.check),
                batches: None,
            }),
            _ => None,
        }
    };
    let eco = match prior {
        Some(source) => {
            // A one-batch plan routes through the batch router's own view (its
            // terminal order and ties, as in the run that made the prior); a real
            // multi-batch plan is refused.
            if config.track_router.is_none()
                || config
                    .batch_terminals
                    .is_some_and(|b| input.terminals.len() > b)
            {
                return Err(fail(
                    "ECO routing needs the track router with every terminal in one batch",
                ));
            }
            Some(prior_routes(
                source, &config, scope, bundle, &input, limits,
            )?)
        }
        None => None,
    };
    let routes = if let (Some((prior, _)), None) = (&eco, config.batch_terminals) {
        routing::route_cpu_ref_with_prior(&input, &config.limits, prior)?
    } else if let Some(batch) = config.batch_terminals {
        let (routes, report) = rustroad_core::route_batches::route_with_prior(
            &input,
            &config.limits,
            batch,
            eco.as_ref().map(|(prior, _)| prior),
        )?;
        let estimate = report.peak_estimate;
        memory = Some(RouteMemory {
            estimate_mib: mib(estimate.total().max(report.final_validation_bytes)),
            memory_mib: input
                .tracks
                .as_ref()
                .expect("batch track plan checked")
                .memory_mib,
            workspace_bytes,
            live_process_bytes,
            held_mib: mib(estimate.held),
            search_mib: mib(estimate.search),
            spatial_mib: mib(estimate.spatial),
            routed_mib: mib(estimate.routed),
            check_mib: mib(estimate.check),
            batches: Some(report),
        });
        routes
    } else {
        routing::route_cpu_ref(&input, &config.limits)?
    };
    rustroad_core::memory::trace("routed");
    bundle.routing = Some(RoutingState {
        source,
        config,
        scope,
        layers: input.layers.iter().map(|l| l.name.clone()).collect(),
        via_names: input.vias.iter().map(|v| v.name.clone()).collect(),
        via_lower_layers: input.vias.iter().map(|v| v.lower).collect(),
        routes,
        table: None,
    });
    // The commit re-validates from its own input; free this one first.
    drop(input);
    metadata.format_version = 5;
    metadata.parent_revision = current;
    let mut summary = crate::commit(path, metadata, &placed)?;
    rustroad_core::memory::trace("committed");
    summary.route_memory = memory;
    summary.eco_prior = eco.map(|(_, report)| report);
    Ok(summary)
}
/// On-disk bytes of one workspace revision (metadata and binary tables).
pub(crate) fn revision_bytes(path: &Path, revision: &str) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(path.join("revisions").join(revision))? {
        let meta = entry?.metadata()?;
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
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
/// Receipt-backed read: the O(n) index bounds later code relies on (layer,
/// via kind). Geometry, connectivity and rules were validated at commit.
pub(crate) fn validate_bounds(state: &RoutingState) -> Result<()> {
    let layers = state.layers.len();
    let kinds = state.via_names.len();
    let r = &state.routes;
    if state.via_lower_layers.len() != kinds
        || state.via_lower_layers.iter().any(|&l| l as usize >= layers)
        || r.wires.iter().any(|w| w.layer as usize >= layers)
        || r.vias.iter().any(|v| v.kind as usize >= kinds)
        || r.access.iter().any(|a| a.layer as usize >= layers)
        || r.patches.iter().any(|p| p.layer as usize >= layers)
    {
        return Err(fail("stored route index out of range"));
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
    records(&state.routes, &mut data)?;
    Ok(data)
}
fn write_record(values: [i64; 8], output: &mut impl Write) -> std::io::Result<()> {
    let mut bytes = [0u8; 64];
    for (chunk, value) in bytes.chunks_exact_mut(8).zip(values) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    output.write_all(&bytes)
}
fn records(routes: &Routes, data: &mut impl Write) -> std::io::Result<()> {
    let mut record = |v: [i64; 8]| write_record(v, data);
    for w in &routes.wires {
        record([
            1,
            w.net as i64,
            w.layer as i64,
            w.start[0],
            w.start[1],
            w.end[0],
            w.end[1],
            0,
        ])?;
    }
    for v in &routes.vias {
        record([2, v.net as i64, v.kind as i64, v.at[0], v.at[1], 0, 0, 0])?;
    }
    for p in &routes.access {
        record([
            3,
            p.net as i64,
            p.pin as i64,
            p.layer as i64,
            p.at[0],
            p.at[1],
            0,
            0,
        ])?;
    }
    for p in &routes.patches {
        record([
            4,
            p.net as i64,
            p.layer as i64,
            p.rect.lx,
            p.rect.ly,
            p.rect.ux,
            p.rect.uy,
            0,
        ])?;
    }
    Ok(())
}
/// Per-net export scratch is bounded independently of the full route table.
/// At 512 bytes/edge (two tree-map vertices, adjacency, edge and visited state),
/// this cap admits at most 98 MiB; actual Ibex nets have at most 1,055 edges.
const MAX_DEF_PATH_EDGES: usize = 200_000;

pub(crate) struct DefPathTechnology<'a> {
    pub layers: &'a [String],
    pub via_names: &'a [String],
    pub via_layers: &'a [(u32, u32)],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DefVertex {
    layer: u32,
    at: [i64; 2],
}

#[derive(Clone, Copy)]
struct DefEdge {
    ends: [DefVertex; 2],
    via: Option<usize>,
}

pub(crate) fn write_def_paths(
    state: &RoutingState,
    via_layers: &[(u32, u32)],
    wires: &[&routing::Wire],
    vias: &[&routing::Via],
    patches: &[&routing::Patch],
    out: &mut impl Write,
) -> Result<()> {
    write_paths(
        DefPathTechnology {
            layers: &state.layers,
            via_names: &state.via_names,
            via_layers,
        },
        wires,
        vias,
        patches,
        &state.config.limits,
        out,
    )
}

/// Serialize connected trails, consuming every wire/via edge once, including
/// cycles. Independent NEW records for every edge can lose loop connections
/// when standard DEF consumers construct their route graph. No metal is added,
/// removed or merged here; collinear normalization happens in `export_runs`.
/// The visited edge set bounds the walk even on reconvergent or cyclic nets.
pub(crate) fn write_paths(
    technology: DefPathTechnology<'_>,
    wires: &[&routing::Wire],
    vias: &[&routing::Via],
    patches: &[&routing::Patch],
    limits: &RouteLimits,
    out: &mut impl Write,
) -> Result<()> {
    let count = wires
        .len()
        .checked_add(vias.len())
        .ok_or_else(|| Error::Budget("DEF path edge count overflow".into()))?;
    if count > limits.max_segments.min(MAX_DEF_PATH_EDGES) || patches.len() > limits.max_segments {
        return Err(Error::Budget("DEF connected path edge cap".into()));
    }
    // Admission includes sorting/map comparisons and linear edge/vertex visits;
    // tree depth is bounded by twice the admitted edge count. The walk itself
    // pops each adjacency entry once and emits each edge once.
    let depth = count.saturating_mul(2).max(1).ilog2() as usize + 1;
    let work = count
        .checked_mul(16 + 4 * depth)
        .and_then(|n| n.checked_add(patches.len()))
        .ok_or_else(|| Error::Budget("DEF path work overflow".into()))?;
    if work > limits.max_comparisons {
        return Err(Error::Budget("DEF connected path work cap".into()));
    }
    let DefPathTechnology {
        layers,
        via_names,
        via_layers,
    } = technology;
    if via_names.len() != via_layers.len() {
        return Err(fail("DEF via names and layer pairs differ"));
    }
    let mut edges = Vec::with_capacity(count);
    let mut adjacency = BTreeMap::<DefVertex, Vec<usize>>::new();
    let mut add = |ends: [DefVertex; 2], via| -> Result<()> {
        if ends.iter().any(|v| v.layer as usize >= layers.len()) {
            return Err(fail("DEF path layer outside routing technology"));
        }
        let index = edges.len();
        edges.push(DefEdge { ends, via });
        for vertex in ends {
            adjacency.entry(vertex).or_default().push(index);
        }
        Ok(())
    };
    for wire in wires {
        add(
            [
                DefVertex {
                    layer: wire.layer,
                    at: wire.start,
                },
                DefVertex {
                    layer: wire.layer,
                    at: wire.end,
                },
            ],
            None,
        )?;
    }
    for via in vias {
        let kind = via.kind as usize;
        let &(lower, upper) = via_layers
            .get(kind)
            .ok_or_else(|| fail("DEF via kind outside routing technology"))?;
        if lower == upper {
            return Err(fail("DEF via must join distinct routing layers"));
        }
        add(
            [
                DefVertex {
                    layer: lower,
                    at: via.at,
                },
                DefVertex {
                    layer: upper,
                    at: via.at,
                },
            ],
            Some(kind),
        )?;
    }
    let mut starts: Vec<_> = adjacency
        .iter()
        .map(|(&vertex, entries)| (entries.len() != 1, vertex))
        .collect();
    starts.sort_by(|(ad, a), (bd, b)| {
        (ad, &layers[a.layer as usize], a.at).cmp(&(bd, &layers[b.layer as usize], b.at))
    });
    let mut seen = vec![false; edges.len()];
    let mut emitted = 0;
    let mut first = true;
    for (_, start) in starts {
        while let Some(edge) = next_def_edge(&mut adjacency, &seen, start) {
            write!(
                out,
                "\n  {} {} ( {} {} )",
                if first { "+ ROUTED" } else { "NEW" },
                rustroad_core::def_name(&layers[start.layer as usize]),
                start.at[0],
                start.at[1]
            )?;
            first = false;
            let mut node = start;
            let mut next = Some(edge);
            while let Some(index) = next {
                let edge = edges[index];
                seen[index] = true;
                emitted += 1;
                node = if node == edge.ends[0] {
                    edge.ends[1]
                } else {
                    edge.ends[0]
                };
                if let Some(kind) = edge.via {
                    write!(out, " {}", rustroad_core::def_name(&via_names[kind]))?;
                } else {
                    write!(out, " ( {} {} )", node.at[0], node.at[1])?;
                }
                next = next_def_edge(&mut adjacency, &seen, node);
            }
        }
    }
    if emitted != edges.len() {
        return Err(fail("DEF path walk omitted an edge"));
    }
    // DEF 5.8 routing RECT, relative to its anchor point (the lower-left corner).
    for patch in patches {
        let layer = layers
            .get(patch.layer as usize)
            .ok_or_else(|| fail("DEF patch layer outside routing technology"))?;
        write!(
            out,
            "\n  {} {} ( {} {} ) RECT ( 0 0 {} {} )",
            if first { "+ ROUTED" } else { "NEW" },
            rustroad_core::def_name(layer),
            patch.rect.lx,
            patch.rect.ly,
            patch.rect.ux - patch.rect.lx,
            patch.rect.uy - patch.rect.ly
        )?;
        first = false;
    }
    Ok(())
}

fn next_def_edge(
    adjacency: &mut BTreeMap<DefVertex, Vec<usize>>,
    seen: &[bool],
    vertex: DefVertex,
) -> Option<usize> {
    let entries = adjacency.get_mut(&vertex)?;
    while let Some(index) = entries.pop() {
        if !seen[index] {
            return Some(index);
        }
    }
    None
}

/// Read-only replay from an exactly bound physical compute view.
///
/// # Errors
/// Rejects changed snapshot/config/workspace data and any admitted resource budget violation.
pub fn replay_route(
    path: &Path,
    config_path: &Path,
    snapshot_path: &Path,
    options: &rustroad_core::routing::ReplayOptions,
    limits: &Limits,
) -> Result<rustroad_core::routing::ReplayReport> {
    let snapshot = crate::load(path, crate::lock(path, false, false)?, limits)?;
    let metadata = snapshot.metadata();
    if metadata.stage != Stage::Placed {
        return Err(fail("place before route replay"));
    }
    let (text, _) = crate::pdk::read_input(config_path, 1024 * 1024, "routing_config")?;
    let config: RoutingConfig = serde_json::from_str(&text)?;
    let placed: Vec<_> = snapshot.records().collect();
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("physical design required"))?;
    let input = build_input(&metadata.design, bundle, &placed, &config, RouteScope::All)?;
    Ok(rustroad_core::routing::replay_route(
        &input,
        &config.limits,
        snapshot_path,
        options,
    )?)
}

/// Diagnostic only: geometric cleanup on a `RUSTROAD_RULE_DUMP` file against
/// this placed workspace's route input. Read-only; no revision is written.
pub fn cleanup_replay(
    path: &Path,
    config_path: &Path,
    dump_path: &Path,
    limits: &Limits,
) -> Result<rustroad_core::routing::CleanupReplayReport> {
    let snapshot = crate::load(path, crate::lock(path, false, false)?, limits)?;
    let metadata = snapshot.metadata();
    let (text, _) = crate::pdk::read_input(config_path, 1024 * 1024, "routing_config")?;
    let config: RoutingConfig = serde_json::from_str(&text)?;
    let placed: Vec<_> = snapshot.records().collect();
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("physical design required"))?;
    let input = build_input(&metadata.design, bundle, &placed, &config, RouteScope::All)?;
    Ok(rustroad_core::routing::cleanup_replay(
        &input,
        &config.limits,
        dump_path,
    )?)
}

#[cfg(test)]
mod obstruction_tests {
    use super::*;
    use rustroad_core::connectivity::Master;

    #[test]
    fn rc_macro_view_omits_abstract_obs_but_keeps_pins() {
        // One macro, one pin and one OBS rectangle: <16 KiB. Independent
        // OpenRCX controls establish that BLOCK OBS are not RC conductors.
        let shape = Geometry::Rect {
            layer: "M2".into(),
            rect: [0, 0, 100, 100],
        };
        let mut master = Master {
            buses: Default::default(),
            obstruction_spacing: Default::default(),
            name: "RAM".into(),
            class: Some("BLOCK".into()),
            symmetry: vec![],
            site: String::new(),
            size: [1000, 1000],
            pins: vec![rustroad_core::connectivity::MasterPin {
                name: "A".into(),
                direction: Some(Direction::Input),
                use_: PinUse::Signal,
                shape: None,
                ports: vec![vec![shape.clone()]],
            }],
            obstructions: vec![shape],
        };
        let layers = vec![RouteLayer {
            name: "M2".into(),
            width: 20,
            spacing: 20,
            vertical: Some(true),
            min_area: None,
            min_step: None,
            wide_spacing: None,
        }];
        let indices = HashMap::from([("M2", 0)]);
        let count = |m: &Master, obs| {
            let templates =
                obstacle_templates(std::slice::from_ref(m), &indices, &layers, obs).unwrap();
            InstanceObstacles::new(
                templates,
                vec![ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [0, 0],
                    orientation: rustroad_core::Orientation::N,
                }],
                vec![0],
            )
            .unwrap()
        };
        assert_eq!(count(&master, true).shape_count(0), Some(2));
        let extracted = count(&master, false);
        assert_eq!(extracted.shape_count(0), Some(1));
        assert_eq!(extracted.shape(0, 0).unwrap().0.net, Some(0));
        master.class = Some("CORE".into());
        assert_eq!(count(&master, false).shape_count(0), Some(2));
    }

    #[test]
    fn explicit_obs_spacing_is_not_added_twice_or_written_into_source_geometry() {
        // One rectangle and no graph: <8 KiB. Full real-macro pin access and
        // external DRC remain the independent integration checks.
        let geometry = Geometry::Rect {
            layer: "M2".into(),
            rect: [0, 0, 1000, 1000],
        };
        let mut master = Master {
            buses: Default::default(),
            obstruction_spacing: std::collections::BTreeMap::from([("M2".into(), 210)]),
            name: "RAM".into(),
            class: Some("BLOCK".into()),
            symmetry: Vec::new(),
            site: String::new(),
            size: [1000, 1000],
            pins: Vec::new(),
            obstructions: vec![geometry.clone()],
        };
        let layer = RouteLayer {
            name: "M2".into(),
            width: 200,
            spacing: 240,
            vertical: Some(true),
            min_area: None,
            min_step: None,
            wide_spacing: None,
        };
        assert_eq!(
            obstruction_rect(&master, &geometry, &layer).unwrap(),
            Rect {
                lx: 0,
                ly: 0,
                ux: 1000,
                uy: 1000
            }
        );
        master.obstruction_spacing.insert("M2".into(), 300);
        assert_eq!(
            obstruction_rect(&master, &geometry, &layer).unwrap(),
            Rect {
                lx: -60,
                ly: -60,
                ux: 1060,
                uy: 1060
            }
        );
        assert_eq!(
            master.obstructions[0], geometry,
            "the imported LEF view stays exact"
        );
    }
}

#[cfg(test)]
mod qor_config_tests {
    use super::*;

    fn layers() -> Vec<RouteLayer> {
        ["Metal1", "Via1", "Metal2"]
            .iter()
            .enumerate()
            .map(|(i, name)| RouteLayer {
                name: (*name).into(),
                width: 200,
                spacing: 210,
                vertical: (i != 1).then_some(i == 2),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            })
            .collect()
    }

    #[test]
    fn via_reduction_and_strict_wide_cycles_parse_and_default_off() {
        let off: RouteQorConfig = serde_json::from_str(r#"{"notch_bridge": true}"#).unwrap();
        let resolved = route_qor(&off, &layers()).unwrap();
        assert!(!resolved.strict_wide_cycles);
        assert_eq!(resolved.via_reduction, None);
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            r#"{"notch_bridge":true}"#
        );
        let on: RouteQorConfig = serde_json::from_str(
            r#"{"strict_wide_cycles": true, "via_reduction": {"passes": 1,
                "via_milli": 3000, "max_terminals": 10, "length_per_via": 1680,
                "net_expansions": 10000, "max_connection_length": 40000}}"#,
        )
        .unwrap();
        let resolved = route_qor(&on, &layers()).unwrap();
        assert!(resolved.strict_wide_cycles);
        let reduction = resolved.via_reduction.unwrap();
        assert_eq!(
            (reduction.passes, reduction.via_milli, reduction.weight),
            (1, 3000, 1)
        );
        assert_eq!(reduction.wrong_way_factor, 0);
        assert_eq!(reduction.margin, None);
        assert_eq!(reduction.min_vias, 1);
        assert_eq!(reduction.max_connection_length, Some(40000));
        // Unknown keys are refused, like every routing-config table.
        assert!(
            serde_json::from_str::<RouteQorConfig>(
                r#"{"via_reduction": {"passes": 1, "via_milli": 3000, "max_terminals": 10,
                    "length_per_via": 0, "net_expansions": 1, "bogus": 1}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn via_dedupe_and_straightening_parse_and_default_off() {
        let off: RouteQorConfig = serde_json::from_str(r#"{"notch_bridge": true}"#).unwrap();
        let resolved = route_qor(&off, &layers()).unwrap();
        assert!(!resolved.dedupe_vias);
        assert_eq!(resolved.straighten, None);
        // Off: the resolved controls serialize exactly as before.
        assert_eq!(
            serde_json::to_string(&resolved).unwrap(),
            r#"{"notch_bridge":true}"#
        );
        let on: RouteQorConfig =
            serde_json::from_str(r#"{"dedupe_vias": true, "straighten": {"passes": 2}}"#).unwrap();
        let resolved = route_qor(&on, &layers()).unwrap();
        assert!(resolved.dedupe_vias);
        assert_eq!(
            resolved.straighten,
            Some(rustroad_core::routing::Straighten {
                passes: 2,
                turn_at_ends: false,
                reroute: None
            })
        );
        assert!(
            serde_json::from_str::<RouteQorConfig>(r#"{"straighten": {"passes": 1, "bogus": 1}}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<RouteQorConfig>(r#"{"straighten": {}}"#).is_err());
        let rerouted: RouteQorConfig = serde_json::from_str(
            r#"{"straighten": {"passes": 1, "turn_at_ends": true, "reroute": {"bend_cost": 420,
                "max_terminals": 10, "net_expansions": 10000}}}"#,
        )
        .unwrap();
        let s = route_qor(&rerouted, &layers()).unwrap().straighten.unwrap();
        assert!(s.turn_at_ends);
        assert_eq!(s.reroute.unwrap().weight, 1);
    }
}

#[cfg(test)]
mod def_path_tests {
    use super::*;

    // Each fixture has at most six edges: <16 KiB including serialized text.
    // Real-layout metal identity and fresh OpenRCX annotation are the external oracle.
    fn emit(
        wires: &[routing::Wire],
        vias: &[routing::Via],
        limits: &RouteLimits,
    ) -> Result<String> {
        let layers = vec!["M1".into(), "unused_cut".into(), "M2".into()];
        let names = vec!["V12".into()];
        let mut text = Vec::new();
        write_paths(
            DefPathTechnology {
                layers: &layers,
                via_names: &names,
                via_layers: &[(0, 2)],
            },
            &wires.iter().collect::<Vec<_>>(),
            &vias.iter().collect::<Vec<_>>(),
            &[],
            limits,
            &mut text,
        )?;
        Ok(String::from_utf8(text).expect("DEF contains UTF-8 names"))
    }

    fn wire(layer: u32, start: [i64; 2], end: [i64; 2]) -> routing::Wire {
        routing::Wire {
            net: 0,
            layer,
            start,
            end,
        }
    }

    #[test]
    fn connected_def_traverses_actual_via_layers_in_both_directions() {
        let wires = [
            wire(0, [0, 0], [10, 0]),
            wire(2, [10, 0], [20, 0]),
            wire(0, [20, 0], [30, 0]),
        ];
        let vias = [
            routing::Via {
                net: 0,
                kind: 0,
                at: [10, 0],
            },
            routing::Via {
                net: 0,
                kind: 0,
                at: [20, 0],
            },
        ];
        assert_eq!(
            emit(&wires, &vias, &RouteLimits::default()).expect("valid path"),
            "\n  + ROUTED M1 ( 0 0 ) ( 10 0 ) V12 ( 20 0 ) V12 ( 30 0 )"
        );
    }

    #[test]
    fn connected_def_consumes_a_cycle_and_branch_once() {
        let wires = [
            wire(0, [0, 0], [10, 0]),
            wire(0, [10, 0], [10, 10]),
            wire(0, [10, 10], [0, 10]),
            wire(0, [0, 10], [0, 0]),
            wire(0, [-10, 0], [0, 0]),
            wire(0, [10, 10], [20, 10]),
        ];
        let text = emit(&wires, &[], &RouteLimits::default()).expect("bounded cyclic path");
        assert_eq!(
            text,
            "\n  + ROUTED M1 ( -10 0 ) ( 0 0 ) ( 0 10 ) ( 10 10 ) ( 20 10 )\n  NEW M1 ( 0 0 ) ( 10 0 ) ( 10 10 )"
        );
    }

    #[test]
    fn connected_def_keeps_disconnected_components_and_patch_geometry() {
        let layers = vec!["M1".into()];
        let wires = [wire(0, [0, 0], [10, 0]), wire(0, [20, 0], [30, 0])];
        let patch = routing::Patch {
            net: 0,
            layer: 0,
            rect: Rect {
                lx: 3,
                ly: 4,
                ux: 8,
                uy: 11,
            },
        };
        let mut bytes = Vec::new();
        write_paths(
            DefPathTechnology {
                layers: &layers,
                via_names: &[],
                via_layers: &[],
            },
            &wires.iter().collect::<Vec<_>>(),
            &[],
            &[&patch],
            &RouteLimits::default(),
            &mut bytes,
        )
        .expect("disconnected route fragments remain explicit");
        assert_eq!(
            String::from_utf8(bytes).expect("UTF-8"),
            "\n  + ROUTED M1 ( 0 0 ) ( 10 0 )\n  NEW M1 ( 20 0 ) ( 30 0 )\n  NEW M1 ( 3 4 ) RECT ( 0 0 5 7 )"
        );
    }

    #[test]
    fn connected_def_rejects_edge_and_work_budgets() {
        let wires = [wire(0, [0, 0], [10, 0])];
        for limits in [
            RouteLimits {
                max_segments: 0,
                ..RouteLimits::default()
            },
            RouteLimits {
                max_comparisons: 0,
                ..RouteLimits::default()
            },
        ] {
            assert!(matches!(emit(&wires, &[], &limits), Err(Error::Budget(_))));
        }
        let invalid = [routing::Via {
            net: 0,
            kind: 1,
            at: [0, 0],
        }];
        assert!(emit(&[], &invalid, &RouteLimits::default()).is_err());
    }
}
