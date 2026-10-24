//! Read-only native process-antenna check of a routed revision.
//!
//! Model (LEF/DEF 5.8 process-antenna semantics, OXIDE1 only): for every
//! routing or cut layer that carries an antenna rule, the routed net is
//! split into partial nets made of the conductors that exist when that layer
//! is fabricated (routing layers at or below it and the cuts below it; for a
//! cut layer, the routing layers below it and the cuts up to it). For each
//! partial net touching gate area the ratio
//!     metal / (gate + ANTENNAGATEPLUSDIFF x diffusion)
//! is compared with the layer limit. Metal is the union side area
//! (perimeter x LEF THICKNESS) for SIDE rules and the union area for AREA
//! rules. The diffusion-dependent rule (scalar or PWL in the connected
//! diffusion area) applies when diffusion is connected; otherwise the
//! non-diffusion rule applies, or the diffusion rule evaluated at zero when
//! the LEF states only that one. Cumulative rules, other oxide models and
//! AREAMINUSDIFF are reported as unsupported (status partial), never ignored.
//!
//! Conservative verdict: the native model leaves out cell-internal metal and
//! the foundry decks extract gate area from the layout, so the native ratio
//! reads low against the deck (sky130 clk/met3: 445.6 native vs 470.866
//! Magic, i.e. 5.4 % low; the phase-B1 length sweep bounds the under-read
//! below 8.4 %). A row is therefore `violated` when its ratio exceeds
//! `(1 - safety_margin) x limit` (default margin `DEFAULT_SAFETY_MARGIN` =
//! 0.10, above the measured bound); `exceeds_lef_limit` keeps the raw LEF
//! comparison. The foundry deck stays the signoff gate (docs/rtl2gds.md).
//!
//! Inputs are the workspace's own technology/cell LEF sources, re-read and
//! verified by SHA-256, and the routed geometry: wires (LEF width), vias,
//! same-net patches and the routing-layer pin shapes of every terminal.
//! Optional what-if shapes (planted controls) are merged into named nets.
//!
//! Memory/work preflight (Rule 9): per net at most `max_shapes_per_net`
//! shapes (default 200,000; 64 B each with union-find state), pair checks
//! capped by `max_pair_checks` (2e8 total), and per component/layer union
//! grids capped by `max_union_cells` (4M cells, 1 B each). The pipeline
//! fixture uses < 1 MiB. No path, cut or cone is enumerated.
use crate::pdk::read_input;
use crate::routing::build_input_with_owners;
use crate::{Error, Result};
use rustroad_core::connectivity::PinOwner;
use rustroad_core::routing::{self as core_routing, LayerRect};
use rustroad_core::{Limits, Rect};
use rustroad_formats::lef::{self, AntennaRatio, LayerAntenna, RATIO_SCALE};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

pub const ANTENNA_SCHEMA: &str = "rustroad.antenna-check.v1";
/// Fraction of the LEF limit kept as margin for the native verdict; derived
/// from the measured native-vs-deck under-read (module documentation).
pub const DEFAULT_SAFETY_MARGIN: f64 = 0.10;
pub const SAFETY_MARGIN_DERIVATION: &str = "native partial-net ratios read low against the \
foundry deck: sky130 clk/met3 445.6 native vs 470.866 Magic (-5.4 %); planted met3 sweep: \
native 379.5 already fails Magic, native 366.6 passes (-5.1 % .. -8.4 %). Margin 0.10 exceeds \
the measured bound; the foundry deck remains the signoff gate.";

fn fail(code: &str, m: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("{code}: {m}"))
}

#[derive(Clone, Debug)]
pub struct AntennaRequest {
    pub extra_shapes: Option<PathBuf>,
    pub max_shapes_per_net: usize,
    pub max_pair_checks: usize,
    pub max_union_cells: usize,
    pub max_rows: usize,
    /// Fraction of each limit kept as margin (0 ..= 0.5).
    pub safety_margin: f64,
}
impl Default for AntennaRequest {
    fn default() -> Self {
        Self {
            extra_shapes: None,
            max_shapes_per_net: 200_000,
            max_pair_checks: 200_000_000,
            max_union_cells: 4_000_000,
            max_rows: 100_000,
            safety_margin: DEFAULT_SAFETY_MARGIN,
        }
    }
}

/// The margin a report was produced with (also read back by antenna-eco).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyMargin {
    pub fraction: f64,
    pub rule: String,
    pub derivation: String,
}

/// What-if conductor added to a named net (planted control geometry), DBU.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraShape {
    pub net: String,
    pub layer: String,
    pub rect: [i64; 4],
}

/// A limit in physical units: scalar or PWL (diffusion um^2, ratio).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Limit {
    Value { ratio: f64 },
    Pwl { points: Vec<[f64; 2]> },
}
impl Limit {
    fn from_lef(r: &AntennaRatio, dbu2: f64) -> Self {
        let s = RATIO_SCALE as f64;
        match r {
            AntennaRatio::Value(v) => Limit::Value {
                ratio: *v as f64 / s,
            },
            AntennaRatio::Pwl(p) => Limit::Pwl {
                points: p
                    .iter()
                    .map(|q| [q[0] as f64 / dbu2, q[1] as f64 / s])
                    .collect(),
            },
        }
    }
    /// PWL is linear between points and constant beyond the end points.
    pub fn at(&self, diff_um2: f64) -> f64 {
        match self {
            Limit::Value { ratio } => *ratio,
            Limit::Pwl { points } => {
                if diff_um2 <= points[0][0] {
                    return points[0][1];
                }
                for w in points.windows(2) {
                    if diff_um2 <= w[1][0] {
                        let t = (diff_um2 - w[0][0]) / (w[1][0] - w[0][0]);
                        return w[0][1] + t * (w[1][1] - w[0][1]);
                    }
                }
                points[points.len() - 1][1]
            }
        }
    }
}
/// The two LEF statements a check may use and the gate-plus-diff factor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulePair {
    pub keyword_no_diff: String,
    pub no_diff: Option<Limit>,
    pub keyword_with_diff: String,
    pub with_diff: Option<Limit>,
    pub gate_plus_diff: Option<f64>,
}
impl RulePair {
    /// (limit, keyword) for a partial net with `diff_um2` diffusion.
    pub fn limit(&self, diff_um2: f64) -> Option<(f64, String)> {
        let (first, second) = if diff_um2 > 0.0 {
            (
                (&self.with_diff, &self.keyword_with_diff),
                (&self.no_diff, &self.keyword_no_diff),
            )
        } else {
            (
                (&self.no_diff, &self.keyword_no_diff),
                (&self.with_diff, &self.keyword_with_diff),
            )
        };
        first
            .0
            .as_ref()
            .map(|l| (l.at(diff_um2), first.1.clone()))
            .or_else(|| {
                second
                    .0
                    .as_ref()
                    .map(|l| (l.at(diff_um2), second.1.clone()))
            })
    }
    pub fn denominator(&self, gate_um2: f64, diff_um2: f64) -> f64 {
        gate_um2
            + if diff_um2 > 0.0 {
                self.gate_plus_diff.unwrap_or(0.0) * diff_um2
            } else {
                0.0
            }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckRow {
    pub net: String,
    pub layer: String,
    /// side_area | area
    pub kind: String,
    pub gates: Vec<String>,
    pub diffusions: Vec<String>,
    pub gate_area_um2: f64,
    pub diff_area_um2: f64,
    pub metal_um2: f64,
    pub denominator_um2: f64,
    pub ratio: f64,
    pub limit: f64,
    /// `(1 - safety_margin) x limit`: the native verdict's threshold.
    pub effective_limit: f64,
    pub rule: String,
    pub rules: RulePair,
    /// Conservative native verdict: `ratio > effective_limit`.
    pub violated: bool,
    /// Raw LEF comparison: `ratio > limit`.
    pub exceeds_lef_limit: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceCheck {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct AntennaScope {
    pub model: &'static str,
    pub oxide: &'static str,
    pub cumulative: String,
    pub side_area: &'static str,
    pub block_ports: &'static str,
    pub cell_internal_metal: &'static str,
    pub checked_layers: Vec<String>,
    pub rule_free_layers: Vec<String>,
    pub extra_shapes: usize,
    pub drc_status: &'static str,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct AntennaStats {
    pub nets: usize,
    pub shapes: usize,
    pub partial_nets_checked: usize,
    pub pair_checks: usize,
    pub union_cells: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct NetWorst {
    pub net: String,
    pub worst_ratio_over_limit: f64,
    pub violated: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct AntennaReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub safety_margin: SafetyMargin,
    /// Rows above the raw LEF limit (subset of `violations`).
    pub lef_violations: usize,
    pub workspace_revision: String,
    pub design: String,
    pub dbu_per_micron: u32,
    pub sources: Vec<SourceCheck>,
    pub scope: AntennaScope,
    pub stats: AntennaStats,
    pub violations: usize,
    pub nets: Vec<NetWorst>,
    pub checks: Vec<CheckRow>,
    pub unsupported: Vec<lef::AntennaUnsupported>,
    pub diagnostics: Vec<String>,
}

struct Dsu(Vec<usize>);
impl Dsu {
    fn find(&mut self, mut a: usize) -> usize {
        while self.0[a] != a {
            self.0[a] = self.0[self.0[a]];
            a = self.0[a];
        }
        a
    }
    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            self.0[hi] = lo;
        }
    }
}
fn touch(a: Rect, b: Rect) -> bool {
    a.lx <= b.ux && b.lx <= a.ux && a.ly <= b.uy && b.ly <= a.uy
}
fn overlap(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

/// Union area (DBU^2) and perimeter (DBU) of rectangles by coordinate compression.
fn union_area_perimeter(
    rects: &[Rect],
    max_cells: usize,
    used: &mut usize,
) -> Result<(i128, i128)> {
    let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r.lx, r.ux]).collect();
    let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r.ly, r.uy]).collect();
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    let (nx, ny) = (xs.len().saturating_sub(1), ys.len().saturating_sub(1));
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| Error::Budget("antenna union grid".into()))?;
    if cells > max_cells {
        return Err(Error::Budget(format!(
            "ANTENNA: union grid {cells} cells exceeds max_union_cells {max_cells}"
        )));
    }
    *used += cells;
    let mut grid = vec![false; cells];
    for r in rects {
        let x0 = xs.partition_point(|&x| x < r.lx);
        let x1 = xs.partition_point(|&x| x < r.ux);
        let y0 = ys.partition_point(|&y| y < r.ly);
        let y1 = ys.partition_point(|&y| y < r.uy);
        for x in x0..x1 {
            for y in y0..y1 {
                grid[x * ny + y] = true;
            }
        }
    }
    let at = |x: isize, y: isize| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < nx
            && (y as usize) < ny
            && grid[x as usize * ny + y as usize]
    };
    let (mut area, mut perimeter) = (0i128, 0i128);
    for x in 0..nx {
        for y in 0..ny {
            if !grid[x * ny + y] {
                continue;
            }
            let dx = i128::from(xs[x + 1] - xs[x]);
            let dy = i128::from(ys[y + 1] - ys[y]);
            area += dx * dy;
            let (xi, yi) = (x as isize, y as isize);
            if !at(xi - 1, yi) {
                perimeter += dy;
            }
            if !at(xi + 1, yi) {
                perimeter += dy;
            }
            if !at(xi, yi - 1) {
                perimeter += dx;
            }
            if !at(xi, yi + 1) {
                perimeter += dx;
            }
        }
    }
    Ok((area, perimeter))
}

struct Shape {
    layer: u32,
    rect: Rect,
    terminal: Option<usize>,
}

fn rules_for(a: &LayerAntenna, dbu2: f64, side: bool) -> RulePair {
    let s = RATIO_SCALE as f64;
    let scalar = |v: &Option<i64>| {
        v.map(|x| Limit::Value {
            ratio: x as f64 / s,
        })
    };
    let ratio = |v: &Option<AntennaRatio>| v.as_ref().map(|r| Limit::from_lef(r, dbu2));
    RulePair {
        keyword_no_diff: if side {
            "ANTENNASIDEAREARATIO"
        } else {
            "ANTENNAAREARATIO"
        }
        .into(),
        no_diff: if side {
            scalar(&a.side_area_ratio)
        } else {
            scalar(&a.area_ratio)
        },
        keyword_with_diff: if side {
            "ANTENNADIFFSIDEAREARATIO"
        } else {
            "ANTENNADIFFAREARATIO"
        }
        .into(),
        with_diff: if side {
            ratio(&a.diff_side_area_ratio)
        } else {
            ratio(&a.diff_area_ratio)
        },
        gate_plus_diff: a.gate_plus_diff.map(|v| v as f64 / s),
    }
}

pub fn check_antenna(
    workspace: &Path,
    request: &AntennaRequest,
    limits: &Limits,
) -> Result<AntennaReport> {
    if request.max_shapes_per_net == 0
        || request.max_shapes_per_net > 1_000_000
        || request.max_pair_checks == 0
        || request.max_pair_checks > 2_000_000_000
        || request.max_union_cells == 0
        || request.max_union_cells > 64_000_000
        || request.max_rows == 0
        || request.max_rows > 1_000_000
    {
        return Err(fail(
            "INVALID_ARGUMENT",
            "antenna budgets outside hard bounds",
        ));
    }
    if !(request.safety_margin.is_finite() && (0.0..=0.5).contains(&request.safety_margin)) {
        return Err(fail(
            "INVALID_ARGUMENT",
            "--safety-margin must be within 0 ..= 0.5",
        ));
    }
    let keep = 1.0 - request.safety_margin;
    let snapshot = crate::read(workspace, limits)?;
    let metadata = snapshot.metadata();
    let design = &metadata.design;
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("NO_PHYSICAL", "import a PDK design first"))?;
    let state = bundle
        .routing
        .as_ref()
        .ok_or_else(|| fail("NO_ROUTING", "route before the antenna check"))?;
    // Re-read both LEF sources and require the imported identity.
    let dbu = design.units_per_micron;
    let dbu2 = f64::from(dbu) * f64::from(dbu);
    let mut docs = Vec::new();
    let mut sources = Vec::new();
    for role in ["technology_lef", "cell_lef"] {
        let pin = bundle
            .sources
            .iter()
            .find(|s| s.role == role)
            .ok_or_else(|| {
                fail(
                    "ANTENNA_SOURCE",
                    format!("workspace has no {role} identity"),
                )
            })?;
        let (text, read) = read_input(&pin.path, 64 * 1024 * 1024, role)?;
        if read.sha256 != pin.sha256 {
            return Err(fail(
                "ANTENNA_SOURCE_CHANGED",
                format!(
                    "{} no longer matches the imported sha256",
                    pin.path.display()
                ),
            ));
        }
        let doc = lef::parse_lef(
            &text,
            dbu,
            &lef::LefLimits {
                max_input_bytes: 64 * 1024 * 1024,
                ..Default::default()
            },
        )
        .map_err(|e| fail("ANTENNA_LEF", e))?;
        sources.push(SourceCheck {
            role: role.into(),
            path: pin.path.clone(),
            sha256: read.sha256,
        });
        docs.push(doc);
    }
    let tech = &docs[0];
    let stack: HashMap<&str, usize> = tech
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| (l.name.as_str(), i))
        .collect();
    let layer_rules: HashMap<&str, &LayerAntenna> = docs
        .iter()
        .flat_map(|d| d.antenna.layers.iter())
        .map(|a| (a.layer.as_str(), a))
        .collect();
    let mut pin_area: HashMap<(&str, &str), (i64, i64, bool)> = HashMap::new();
    for d in &docs {
        for p in &d.antenna.pins {
            pin_area.insert(
                (p.macro_name.as_str(), p.pin.as_str()),
                (p.gate_area, p.diff_area, p.layer_qualified),
            );
        }
    }
    let unsupported: Vec<_> = docs
        .iter()
        .flat_map(|d| d.antenna.unsupported.iter().cloned())
        .collect();
    let placed: Vec<_> = snapshot.records().collect();
    let (input, _owners) =
        build_input_with_owners(design, bundle, &placed, &state.config, state.scope)?;
    let model = &bundle.connectivity;
    // Stack position of every route layer and the cut adjacency from the vias.
    let mut position = Vec::with_capacity(input.layers.len());
    for l in &input.layers {
        position.push(*stack.get(l.name.as_str()).ok_or_else(|| {
            fail(
                "ANTENNA_LEF",
                format!("route layer {} absent from the technology LEF", l.name),
            )
        })?);
    }
    let mut cut_adjacent: HashMap<u32, (u32, u32)> = HashMap::new();
    for v in &input.vias {
        for s in &v.shapes {
            if input.layers[s.layer as usize].vertical.is_none()
                && let Some(old) = cut_adjacent.insert(s.layer, (v.lower, v.upper))
                && old != (v.lower, v.upper)
            {
                return Err(fail(
                    "ANTENNA_LEF",
                    "a cut layer joins different layer pairs",
                ));
            }
        }
    }
    // Layers to check, bottom-up.
    let mut order: Vec<u32> = (0..input.layers.len() as u32).collect();
    order.sort_by_key(|&i| position[i as usize]);
    let mut checked_layers = Vec::new();
    let mut rule_free_layers = Vec::new();
    let mut diagnostics = Vec::new();
    let mut partial = false;
    for &i in &order {
        let name = input.layers[i as usize].name.as_str();
        let has_ratio = |a: &&&LayerAntenna| {
            a.area_ratio.is_some()
                || a.diff_area_ratio.is_some()
                || a.side_area_ratio.is_some()
                || a.diff_side_area_ratio.is_some()
        };
        match layer_rules.get(name).filter(has_ratio) {
            None => rule_free_layers.push(name.to_string()),
            Some(a) => {
                let side = a.side_area_ratio.is_some() || a.diff_side_area_ratio.is_some();
                if side && a.thickness.is_none() {
                    diagnostics.push(format!("ANTENNA_NO_THICKNESS: {name} has a side-area rule but no THICKNESS; side checks skipped"));
                    partial = true;
                }
                if a.cum_area_ratio.is_some()
                    || a.cum_diff_area_ratio.is_some()
                    || a.cum_side_area_ratio.is_some()
                    || a.cum_diff_side_area_ratio.is_some()
                    || a.area_minus_diff.is_some()
                {
                    diagnostics.push(format!(
                        "ANTENNA_UNMODELED: {name} states a cumulative or AREAMINUSDIFF rule"
                    ));
                    partial = true;
                }
                if a.model.as_deref().is_some_and(|m| m != "OXIDE1") {
                    partial = true;
                }
                checked_layers.push(name.to_string());
            }
        }
    }
    let cumulative = if docs.iter().flat_map(|d| &d.antenna.layers).any(|a| {
        a.cum_area_ratio.is_some()
            || a.cum_diff_area_ratio.is_some()
            || a.cum_side_area_ratio.is_some()
            || a.cum_diff_side_area_ratio.is_some()
    }) {
        "present_in_lef_unsupported".to_string()
    } else {
        "not_present_in_lef".to_string()
    };
    if !unsupported.is_empty() {
        partial = true;
        diagnostics.push(format!(
            "ANTENNA_UNSUPPORTED: {} LEF antenna statements are not modeled (see unsupported)",
            unsupported.len()
        ));
    }
    // Per-net shapes.
    let mut per_net: BTreeMap<u32, Vec<Shape>> = BTreeMap::new();
    let routed: std::collections::BTreeSet<u32> = state.routes.nets.iter().copied().collect();
    for w in &state.routes.wires {
        let width = input.layers[w.layer as usize].width;
        per_net.entry(w.net).or_default().push(Shape {
            layer: w.layer,
            rect: core_routing::wire_rect(w, width)?,
            terminal: None,
        });
    }
    for v in &state.routes.vias {
        let via = input
            .vias
            .get(v.kind as usize)
            .ok_or_else(|| fail("ANTENNA_ROUTES", "via kind outside the technology"))?;
        for LayerRect { layer, rect } in &via.shapes {
            per_net.entry(v.net).or_default().push(Shape {
                layer: *layer,
                rect: core_routing::shifted(*rect, v.at)?,
                terminal: None,
            });
        }
    }
    for p in &state.routes.patches {
        per_net.entry(p.net).or_default().push(Shape {
            layer: p.layer,
            rect: p.rect,
            terminal: None,
        });
    }
    for (ti, t) in input.terminals.iter().enumerate() {
        if !routed.contains(&t.net) {
            continue;
        }
        for s in &t.shapes {
            per_net.entry(t.net).or_default().push(Shape {
                layer: s.layer,
                rect: s.rect,
                terminal: Some(ti),
            });
        }
    }
    let mut extra_count = 0;
    if let Some(path) = &request.extra_shapes {
        let (text, _) = read_input(path, 16 * 1024 * 1024, "extra_shapes")?;
        let extra: Vec<ExtraShape> = serde_json::from_str(&text)?;
        if extra.len() > 10_000 {
            return Err(Error::Budget(
                "ANTENNA: more than 10000 extra shapes".into(),
            ));
        }
        for e in extra {
            let net = model
                .nets
                .iter()
                .position(|n| n.name == e.net)
                .ok_or_else(|| fail("ANTENNA_EXTRA", format!("net {} absent", e.net)))?
                as u32;
            let layer = input
                .layers
                .iter()
                .position(|l| l.name == e.layer && l.vertical.is_some())
                .ok_or_else(|| {
                    fail(
                        "ANTENNA_EXTRA",
                        format!("{} is not a routing layer", e.layer),
                    )
                })? as u32;
            let [lx, ly, ux, uy] = e.rect;
            if lx >= ux || ly >= uy {
                return Err(fail(
                    "ANTENNA_EXTRA",
                    "extra rectangle must have positive extents",
                ));
            }
            if !routed.contains(&net) {
                return Err(fail(
                    "ANTENNA_EXTRA",
                    format!("net {} is not routed", e.net),
                ));
            }
            per_net.entry(net).or_default().push(Shape {
                layer,
                rect: Rect { lx, ly, ux, uy },
                terminal: None,
            });
            extra_count += 1;
        }
    }
    // Terminal names and antenna areas.
    let terminal_info = |ti: usize| -> (String, i64, i64, bool) {
        let t = &input.terminals[ti];
        match model.pins[t.pin as usize].owner {
            PinOwner::Instance { instance, terminal } => {
                let master = &model.masters[model.instance_masters[instance as usize] as usize];
                let pin = &master.pins[terminal as usize].name;
                let (g, d, q) = pin_area
                    .get(&(master.name.as_str(), pin.as_str()))
                    .copied()
                    .unwrap_or((0, 0, false));
                (
                    format!("{}/{}", design.cells[instance as usize].name, pin),
                    g,
                    d,
                    q,
                )
            }
            PinOwner::Port { port, bit } => (
                model.ports[port as usize].bits[bit as usize].clone(),
                0,
                0,
                false,
            ),
        }
    };
    let mut stats = AntennaStats::default();
    let mut rows: Vec<CheckRow> = Vec::new();
    let mut worst: BTreeMap<String, (f64, bool)> = BTreeMap::new();
    let mut qualified = false;
    for (&net, shapes) in &per_net {
        stats.nets += 1;
        stats.shapes += shapes.len();
        if shapes.len() > request.max_shapes_per_net {
            return Err(Error::Budget(format!(
                "ANTENNA: net {} has {} shapes (max_shapes_per_net {})",
                model.nets[net as usize].name,
                shapes.len(),
                request.max_shapes_per_net
            )));
        }
        let net_name = model.nets[net as usize].name.clone();
        let mut by_layer: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, s) in shapes.iter().enumerate() {
            by_layer.entry(s.layer).or_default().push(i);
        }
        for list in by_layer.values_mut() {
            list.sort_by_key(|&i| (shapes[i].rect.lx, i));
        }
        let mut dsu = Dsu((0..shapes.len()).collect());
        let mut first_of_terminal: HashMap<usize, usize> = HashMap::new();
        let mut added = vec![false; shapes.len()];
        for &layer in &order {
            // No metal of this net on this layer: nothing to add or check.
            let Some(list) = by_layer.get(&layer) else {
                continue;
            };
            for &i in list {
                added[i] = true;
                if let Some(t) = shapes[i].terminal {
                    match first_of_terminal.get(&t) {
                        Some(&f) => dsu.union(f, i),
                        None => {
                            first_of_terminal.insert(t, i);
                        }
                    }
                }
            }
            // Same-layer touching, swept by lx.
            for (k, &i) in list.iter().enumerate() {
                for &j in &list[k + 1..] {
                    if shapes[j].rect.lx > shapes[i].rect.ux {
                        break;
                    }
                    stats.pair_checks += 1;
                    if stats.pair_checks > request.max_pair_checks {
                        return Err(Error::Budget("ANTENNA: max_pair_checks".into()));
                    }
                    if touch(shapes[i].rect, shapes[j].rect) {
                        dsu.union(i, j);
                    }
                }
            }
            // Cut <-> adjacent routing layers (both already added when present).
            let joins: Vec<(u32, u32)> = if input.layers[layer as usize].vertical.is_none() {
                cut_adjacent
                    .get(&layer)
                    .map(|&(lo, hi)| vec![(layer, lo), (layer, hi)])
                    .unwrap_or_default()
            } else {
                cut_adjacent
                    .iter()
                    .filter(|(_, (lo, hi))| *lo == layer || *hi == layer)
                    .map(|(&c, _)| (c, layer))
                    .collect()
            };
            for (cut, metal) in joins {
                let (Some(cs), Some(ms)) = (by_layer.get(&cut), by_layer.get(&metal)) else {
                    continue;
                };
                for &c in cs {
                    if !added[c] {
                        continue;
                    }
                    for &m in ms {
                        if !added[m] {
                            continue;
                        }
                        stats.pair_checks += 1;
                        if stats.pair_checks > request.max_pair_checks {
                            return Err(Error::Budget("ANTENNA: max_pair_checks".into()));
                        }
                        if overlap(shapes[c].rect, shapes[m].rect) {
                            dsu.union(c, m);
                        }
                    }
                }
            }
            let lname = input.layers[layer as usize].name.as_str();
            let Some(rule) = layer_rules.get(lname) else {
                continue;
            };
            if rule.model.as_deref().is_some_and(|m| m != "OXIDE1") {
                continue;
            }
            // Components with gate area: gate/diff sums and pin names.
            let mut comp: BTreeMap<usize, (i64, i64, Vec<String>, Vec<String>)> = BTreeMap::new();
            for (&t, &first) in &first_of_terminal {
                let root = dsu.find(first);
                let (name, g, d, q) = terminal_info(t);
                qualified |= q;
                let e = comp.entry(root).or_default();
                e.0 += g;
                e.1 += d;
                if g > 0 {
                    e.2.push(name.clone());
                }
                if d > 0 {
                    e.3.push(name);
                }
            }
            let mut metal: BTreeMap<usize, Vec<Rect>> = BTreeMap::new();
            for &i in list {
                metal.entry(dsu.find(i)).or_default().push(shapes[i].rect);
            }
            let is_cut = input.layers[layer as usize].vertical.is_none();
            for (root, rects) in metal {
                let Some((gate, diff, gates, diffs)) = comp.get(&root) else {
                    continue;
                };
                if *gate <= 0 {
                    continue;
                }
                stats.partial_nets_checked += 1;
                let (area, perimeter) =
                    union_area_perimeter(&rects, request.max_union_cells, &mut stats.union_cells)?;
                let gate_um2 = *gate as f64 / dbu2;
                let diff_um2 = *diff as f64 / dbu2;
                let mut kinds = vec![(false, area as f64 / dbu2)];
                if !is_cut && let Some(t) = rule.thickness {
                    kinds.push((true, (perimeter * i128::from(t)) as f64 / dbu2));
                }
                for (side, metal_um2) in kinds {
                    let rules = rules_for(rule, dbu2, side);
                    let Some((limit, keyword)) = rules.limit(diff_um2) else {
                        continue;
                    };
                    let denominator = rules.denominator(gate_um2, diff_um2);
                    let ratio = metal_um2 / denominator;
                    let effective_limit = limit * keep;
                    let violated = ratio > effective_limit;
                    let w = worst.entry(net_name.clone()).or_insert((0.0, false));
                    w.0 = w.0.max(ratio / limit);
                    w.1 |= violated;
                    let mut g = gates.clone();
                    g.sort();
                    let mut d = diffs.clone();
                    d.sort();
                    rows.push(CheckRow {
                        net: net_name.clone(),
                        layer: lname.to_string(),
                        kind: if side { "side_area" } else { "area" }.into(),
                        gates: g,
                        diffusions: d,
                        gate_area_um2: gate_um2,
                        diff_area_um2: diff_um2,
                        metal_um2,
                        denominator_um2: denominator,
                        ratio,
                        limit,
                        effective_limit,
                        rule: if diff_um2 > 0.0 && rules.with_diff.is_some() {
                            format!("{keyword}@diffusion {diff_um2:.6}um2")
                        } else {
                            keyword
                        },
                        rules,
                        violated,
                        exceeds_lef_limit: ratio > limit,
                    });
                }
            }
        }
    }
    if qualified {
        partial = true;
        diagnostics.push("ANTENNA_LAYER_QUALIFIED_PIN: some pin areas are LAYER-qualified and were summed over all layers".into());
    }
    rows.sort_by(|a, b| {
        (b.ratio / b.limit)
            .total_cmp(&(a.ratio / a.limit))
            .then_with(|| a.net.cmp(&b.net))
            .then_with(|| a.layer.cmp(&b.layer))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    let violations = rows.iter().filter(|r| r.violated).count();
    let lef_violations = rows.iter().filter(|r| r.exceeds_lef_limit).count();
    if violations > request.max_rows {
        return Err(Error::Budget(format!(
            "ANTENNA: {violations} violations exceed max_rows"
        )));
    }
    rows.truncate(request.max_rows);
    let status = if violations > 0 {
        "violated"
    } else if partial {
        "partial"
    } else {
        "passed"
    };
    if extra_count > 0 {
        diagnostics.push(format!("ANTENNA_WHAT_IF: {extra_count} extra shapes from --extra-shapes are merged into their nets"));
    }
    Ok(AntennaReport {
        schema: ANTENNA_SCHEMA,
        status,
        safety_margin: SafetyMargin {
            fraction: request.safety_margin,
            rule: "violated when ratio > (1 - fraction) x LEF limit; exceeds_lef_limit is the raw comparison".into(),
            derivation: SAFETY_MARGIN_DERIVATION.into(),
        },
        lef_violations,
        workspace_revision: metadata.revision.clone(),
        design: design.name.clone(),
        dbu_per_micron: dbu,
        sources,
        scope: AntennaScope {
            model: "LEF process-antenna partial-net ratio per routing/cut layer",
            oxide: "OXIDE1",
            cumulative,
            side_area: "union perimeter x LEF THICKNESS",
            block_ports: "top-level ports carry no gate or diffusion area",
            cell_internal_metal: "routing-layer pin port geometry counted; other cell-internal metal not modeled",
            checked_layers,
            rule_free_layers,
            extra_shapes: extra_count,
            drc_status: "unchecked",
        },
        stats,
        violations,
        nets: worst
            .into_iter()
            .map(|(net, (r, v))| NetWorst {
                net,
                worst_ratio_over_limit: r,
                violated: v,
            })
            .collect(),
        checks: rows,
        unsupported,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    #[test]
    fn union_area_and_perimeter_merge_overlaps() {
        let mut used = 0;
        // Two overlapping 10x2 bars forming an L: area 10*2 + 2*8 = 36; perimeter 2*(10+10) = 40.
        let (a, p) =
            union_area_perimeter(&[r(0, 0, 10, 2), r(0, 0, 2, 10)], 100, &mut used).unwrap();
        assert_eq!((a, p), (36, 40));
        // Collinear lattice edges with overlapping end extensions = one bar.
        let (a, p) = union_area_perimeter(
            &[r(0, 0, 6, 2), r(4, 0, 10, 2), r(8, 0, 14, 2)],
            100,
            &mut used,
        )
        .unwrap();
        assert_eq!((a, p), (28, 32));
        // Disjoint shapes add up; touching edges merge.
        let (a, p) = union_area_perimeter(&[r(0, 0, 1, 1), r(1, 0, 2, 1)], 100, &mut used).unwrap();
        assert_eq!((a, p), (2, 6));
        assert!(
            union_area_perimeter(
                &[r(0, 0, 10, 2), r(1, 1, 3, 9), r(5, 5, 7, 7)],
                4,
                &mut used
            )
            .is_err()
        );
    }
    #[test]
    fn rule_selection_and_pwl() {
        let pwl = Limit::Pwl {
            points: vec![
                [0.0, 400.0],
                [0.0125, 400.0],
                [0.0225, 2609.0],
                [22.5, 11600.0],
            ],
        };
        assert_eq!(pwl.at(0.0), 400.0);
        assert!((pwl.at(0.4347) - (2609.0 + (0.4347 - 0.0225) / 22.4775 * 8991.0)).abs() < 1e-9);
        assert_eq!(pwl.at(100.0), 11600.0);
        // Diffusion-only rule evaluated at zero diffusion for gate-only nets.
        let sky = RulePair {
            keyword_no_diff: "ANTENNASIDEAREARATIO".into(),
            no_diff: None,
            keyword_with_diff: "ANTENNADIFFSIDEAREARATIO".into(),
            with_diff: Some(pwl),
            gate_plus_diff: None,
        };
        assert_eq!(sky.limit(0.0).unwrap().0, 400.0);
        assert!(sky.limit(1.0).unwrap().0 > 2609.0);
        assert_eq!(sky.denominator(0.5, 1.0), 0.5);
        // GF180 Metal2+: diff rule with gate-plus-diff; no non-diff rule.
        let gf = RulePair {
            keyword_no_diff: "ANTENNASIDEAREARATIO".into(),
            no_diff: None,
            keyword_with_diff: "ANTENNADIFFSIDEAREARATIO".into(),
            with_diff: Some(Limit::Value { ratio: 400.0 }),
            gate_plus_diff: Some(15.0),
        };
        assert_eq!(gf.denominator(2.6, 0.41), 2.6 + 15.0 * 0.41);
        assert_eq!(gf.denominator(2.6, 0.0), 2.6);
        // Metal1: only the non-diff rule exists and applies with diffusion too.
        let m1 = RulePair {
            keyword_no_diff: "ANTENNASIDEAREARATIO".into(),
            no_diff: Some(Limit::Value { ratio: 400.0 }),
            keyword_with_diff: "ANTENNADIFFSIDEAREARATIO".into(),
            with_diff: None,
            gate_plus_diff: None,
        };
        assert_eq!(
            m1.limit(0.3).unwrap(),
            (400.0, "ANTENNASIDEAREARATIO".to_string())
        );
        let none = RulePair {
            no_diff: None,
            with_diff: None,
            ..m1
        };
        assert!(none.limit(0.0).is_none());
    }
}
