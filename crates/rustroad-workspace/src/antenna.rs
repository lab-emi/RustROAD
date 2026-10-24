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
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// Rail-tie router nets (a tie-cell input joined to its own supply
    /// rail): supply metal, not checked here; the foundry decks check them.
    #[serde(skip_serializing_if = "is_zero")]
    pub supply_tie_nets_unchecked: usize,
    pub shapes: usize,
    pub partial_nets_checked: usize,
    pub pair_checks: usize,
    pub union_cells: usize,
}
fn is_zero(n: &usize) -> bool {
    *n == 0
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

fn check_budgets(request: &AntennaRequest) -> Result<()> {
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
    Ok(())
}

/// The LEF antenna data, route technology and layer order of one routed
/// revision, read once per command.
struct Prepared {
    dbu: u32,
    dbu2: f64,
    sources: Vec<SourceCheck>,
    input: core_routing::RouteInput,
    order: Vec<u32>,
    cut_adjacent: HashMap<u32, (u32, u32)>,
    layer_rules: HashMap<String, LayerAntenna>,
    pin_area: HashMap<(String, String), (i64, i64, bool)>,
    checked_layers: Vec<String>,
    rule_free_layers: Vec<String>,
    diagnostics: Vec<String>,
    partial: bool,
    cumulative: String,
    unsupported: Vec<lef::AntennaUnsupported>,
}

fn prepare(
    design: &rustroad_core::Design,
    bundle: &crate::pdk::PhysicalBundle,
    placed: &[rustroad_core::Placed],
    state: &crate::routing::RoutingState,
) -> Result<Prepared> {
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
    let layer_rules: HashMap<String, LayerAntenna> = docs
        .iter()
        .flat_map(|d| d.antenna.layers.iter())
        .map(|a| (a.layer.clone(), a.clone()))
        .collect();
    let mut pin_area: HashMap<(String, String), (i64, i64, bool)> = HashMap::new();
    for d in &docs {
        for p in &d.antenna.pins {
            pin_area.insert(
                (p.macro_name.clone(), p.pin.clone()),
                (p.gate_area, p.diff_area, p.layer_qualified),
            );
        }
    }
    let unsupported: Vec<_> = docs
        .iter()
        .flat_map(|d| d.antenna.unsupported.iter().cloned())
        .collect();
    let (input, _owners) =
        build_input_with_owners(design, bundle, placed, &state.config, state.scope)?;
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
        let has_ratio = |a: &&LayerAntenna| {
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
    Ok(Prepared {
        dbu,
        dbu2,
        sources,
        input,
        order,
        cut_adjacent,
        layer_rules,
        pin_area,
        checked_layers,
        rule_free_layers,
        diagnostics,
        partial,
        cumulative,
        unsupported,
    })
}

/// Logical routed nets of `routes` (rail-tie router nets have ids past the
/// connectivity nets and are skipped).
fn logical_routed(routes: &core_routing::Routes, logical: u32) -> BTreeSet<u32> {
    routes
        .nets
        .iter()
        .copied()
        .filter(|&n| n < logical)
        .collect()
}

/// Conductor shapes of the nets in `only` (all logical routed nets when
/// `None`): wires (LEF width), vias, patches and routing-layer pin shapes.
fn net_shapes(
    prep: &Prepared,
    routes: &core_routing::Routes,
    routed: &BTreeSet<u32>,
    only: Option<u32>,
) -> Result<BTreeMap<u32, Vec<Shape>>> {
    let input = &prep.input;
    let keep = |n: u32| routed.contains(&n) && only.is_none_or(|o| o == n);
    let mut per_net: BTreeMap<u32, Vec<Shape>> = BTreeMap::new();
    for w in routes.wires.iter().filter(|w| keep(w.net)) {
        let width = input.layers[w.layer as usize].width;
        per_net.entry(w.net).or_default().push(Shape {
            layer: w.layer,
            rect: core_routing::wire_rect(w, width)?,
            terminal: None,
        });
    }
    for v in routes.vias.iter().filter(|v| keep(v.net)) {
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
    for p in routes.patches.iter().filter(|p| keep(p.net)) {
        per_net.entry(p.net).or_default().push(Shape {
            layer: p.layer,
            rect: p.rect,
            terminal: None,
        });
    }
    for (ti, t) in input.terminals.iter().enumerate() {
        if !keep(t.net) {
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
    Ok(per_net)
}

/// Terminal name, LEF gate and diffusion area (DBU^2) and LAYER qualification.
fn terminal_info(
    prep: &Prepared,
    design: &rustroad_core::Design,
    model: &rustroad_core::connectivity::ConnectedDesign,
    ti: usize,
) -> (String, i64, i64, bool) {
    let t = &prep.input.terminals[ti];
    match model.pins[t.pin as usize].owner {
        PinOwner::Instance { instance, terminal } => {
            let master = &model.masters[model.instance_masters[instance as usize] as usize];
            let pin = &master.pins[terminal as usize].name;
            let (g, d, q) = prep
                .pin_area
                .get(&(master.name.clone(), pin.clone()))
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
}

/// Partial-net rows of one net (every checked layer with gate area).
#[allow(clippy::too_many_arguments)]
fn evaluate_net(
    prep: &Prepared,
    design: &rustroad_core::Design,
    model: &rustroad_core::connectivity::ConnectedDesign,
    net: u32,
    shapes: &[Shape],
    request: &AntennaRequest,
    stats: &mut AntennaStats,
    qualified: &mut bool,
) -> Result<Vec<CheckRow>> {
    let input = &prep.input;
    let dbu2 = prep.dbu2;
    let keep = 1.0 - request.safety_margin;
    let mut rows = Vec::new();
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
    for &layer in &prep.order {
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
            prep.cut_adjacent
                .get(&layer)
                .map(|&(lo, hi)| vec![(layer, lo), (layer, hi)])
                .unwrap_or_default()
        } else {
            prep.cut_adjacent
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
        let Some(rule) = prep.layer_rules.get(lname) else {
            continue;
        };
        if rule.model.as_deref().is_some_and(|m| m != "OXIDE1") {
            continue;
        }
        // Components with gate area: gate/diff sums and pin names.
        let mut comp: BTreeMap<usize, (i64, i64, Vec<String>, Vec<String>)> = BTreeMap::new();
        for (&t, &first) in &first_of_terminal {
            let root = dsu.find(first);
            let (name, g, d, q) = terminal_info(prep, design, model, t);
            *qualified |= q;
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
    Ok(rows)
}

fn sort_rows(rows: &mut [CheckRow]) {
    rows.sort_by(|a, b| {
        (b.ratio / b.limit)
            .total_cmp(&(a.ratio / a.limit))
            .then_with(|| a.net.cmp(&b.net))
            .then_with(|| a.layer.cmp(&b.layer))
            .then_with(|| a.kind.cmp(&b.kind))
    });
}

/// Rows of every routed logical net (plus what-if shapes), unsorted, with
/// the per-net worst ratio over limit.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn evaluate_all(
    prep: &Prepared,
    design: &rustroad_core::Design,
    model: &rustroad_core::connectivity::ConnectedDesign,
    routes: &core_routing::Routes,
    request: &AntennaRequest,
    stats: &mut AntennaStats,
    qualified: &mut bool,
    extra: Vec<(u32, Shape)>,
) -> Result<(Vec<CheckRow>, BTreeMap<String, (f64, bool)>)> {
    let routed = logical_routed(routes, model.nets.len() as u32);
    let mut per_net = net_shapes(prep, routes, &routed, None)?;
    for (net, shape) in extra {
        per_net.entry(net).or_default().push(shape);
    }
    let mut rows = Vec::new();
    let mut worst: BTreeMap<String, (f64, bool)> = BTreeMap::new();
    for (&net, shapes) in &per_net {
        let net_rows = evaluate_net(prep, design, model, net, shapes, request, stats, qualified)?;
        for r in &net_rows {
            let w = worst.entry(r.net.clone()).or_insert((0.0, false));
            w.0 = w.0.max(r.ratio / r.limit);
            w.1 |= r.violated;
        }
        rows.extend(net_rows);
    }
    Ok((rows, worst))
}

pub fn check_antenna(
    workspace: &Path,
    request: &AntennaRequest,
    limits: &Limits,
) -> Result<AntennaReport> {
    check_budgets(request)?;
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
    let placed: Vec<_> = snapshot.records().collect();
    let prep = prepare(design, bundle, &placed, state)?;
    let model = &bundle.connectivity;
    let mut diagnostics = prep.diagnostics.clone();
    let mut partial = prep.partial;
    let logical = model.nets.len() as u32;
    let routed = logical_routed(&state.routes, logical);
    let supply_ties = state.routes.nets.len() - routed.len();
    if supply_ties > 0 {
        diagnostics.push(format!(
            "ANTENNA_SUPPLY_TIES: {supply_ties} rail-tie nets (tie-cell input to its own supply rail) are supply metal and not checked natively"
        ));
    }
    let mut extra_shapes = Vec::new();
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
            let layer = prep
                .input
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
            extra_shapes.push((
                net,
                Shape {
                    layer,
                    rect: Rect { lx, ly, ux, uy },
                    terminal: None,
                },
            ));
        }
    }
    let extra_count = extra_shapes.len();
    let mut stats = AntennaStats {
        supply_tie_nets_unchecked: supply_ties,
        ..AntennaStats::default()
    };
    let mut qualified = false;
    let (mut rows, worst) = evaluate_all(
        &prep,
        design,
        model,
        &state.routes,
        request,
        &mut stats,
        &mut qualified,
        extra_shapes,
    )?;
    if qualified {
        partial = true;
        diagnostics.push("ANTENNA_LAYER_QUALIFIED_PIN: some pin areas are LAYER-qualified and were summed over all layers".into());
    }
    sort_rows(&mut rows);
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
        dbu_per_micron: prep.dbu,
        sources: prep.sources,
        scope: AntennaScope {
            model: "LEF process-antenna partial-net ratio per routing/cut layer",
            oxide: "OXIDE1",
            cumulative: prep.cumulative,
            side_area: "union perimeter x LEF THICKNESS",
            block_ports: "top-level ports carry no gate or diffusion area",
            cell_internal_metal: "routing-layer pin port geometry counted; other cell-internal metal not modeled",
            checked_layers: prep.checked_layers,
            rule_free_layers: prep.rule_free_layers,
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
        unsupported: prep.unsupported,
        diagnostics,
    })
}

pub const REPAIR_SCHEMA: &str = "rustroad.antenna-repair.v1";

/// `repair-antenna`: diode-free repair of the native antenna violations of
/// HEAD by layer jumpers (`rustroad_core::jumper`).
#[derive(Clone, Debug)]
pub struct RepairRequest {
    /// Budgets and safety margin of the native check (no what-if shapes).
    pub antenna: AntennaRequest,
    /// `check-antenna` report of the base revision; when given, its revision,
    /// margin and violated rows must equal the recomputed ones.
    pub report: Option<PathBuf>,
    pub max_jumpers_per_net: usize,
    pub max_jumpers: usize,
    pub jumper: rustroad_core::jumper::JumperLimits,
}
impl Default for RepairRequest {
    fn default() -> Self {
        Self {
            antenna: AntennaRequest::default(),
            report: None,
            max_jumpers_per_net: 16,
            max_jumpers: 4096,
            jumper: rustroad_core::jumper::JumperLimits::default(),
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct JumperRecord {
    pub net: String,
    pub layer: String,
    pub upper: String,
    pub via: String,
    pub gate_side: [i64; 2],
    pub far_side: [i64; 2],
    pub gate_distance_um: f64,
    pub run_length_um: f64,
    /// Worst ratio of the net's rows on `layer` before and after (kind with
    /// the largest ratio over limit), and that row's effective limit.
    pub ratio_before: f64,
    pub ratio_after: f64,
    pub effective_limit: f64,
}
#[derive(Clone, Debug, Serialize)]
pub struct Unrepaired {
    pub net: String,
    pub layer: String,
    pub ratio: f64,
    pub effective_limit: f64,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RepairCount {
    pub violations: usize,
    pub lef_violations: usize,
    pub nets: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RepairStats {
    pub candidates_evaluated: usize,
    pub planner_work: usize,
    pub wire_length_dbu_before: u64,
    pub wire_length_dbu_after: u64,
    pub wires_before: usize,
    pub wires_after: usize,
    pub vias_before: usize,
    pub vias_after: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct RepairReport {
    pub schema: &'static str,
    /// clean (nothing violated), repaired (no native violation left),
    /// partial (some left; jumpers inserted), unrepaired (none inserted).
    pub status: &'static str,
    pub method: &'static str,
    pub design: String,
    pub base_revision: String,
    /// New revision when jumpers were committed, else the base revision.
    pub revision: String,
    pub safety_margin: f64,
    pub before: RepairCount,
    pub after: RepairCount,
    pub jumpers: Vec<JumperRecord>,
    pub unrepaired: Vec<Unrepaired>,
    pub stats: RepairStats,
    pub drc_status: &'static str,
    pub antenna_status: &'static str,
}

fn count(rows: &[CheckRow]) -> RepairCount {
    let nets: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.violated)
        .map(|r| r.net.clone())
        .collect();
    RepairCount {
        violations: rows.iter().filter(|r| r.violated).count(),
        lef_violations: rows.iter().filter(|r| r.exceeds_lef_limit).count(),
        nets: nets.into_iter().collect(),
    }
}

/// Worst (ratio / effective limit) per layer name of one net's rows.
fn excess_by_layer(rows: &[CheckRow]) -> BTreeMap<String, (f64, f64, f64)> {
    let mut out: BTreeMap<String, (f64, f64, f64)> = BTreeMap::new();
    for r in rows {
        let x = r.ratio / r.effective_limit;
        let e = out
            .entry(r.layer.clone())
            .or_insert((x, r.ratio, r.effective_limit));
        if x > e.0 {
            *e = (x, r.ratio, r.effective_limit);
        }
    }
    out
}

/// Check the caller's report against the recomputed rows (same revision,
/// margin and violated row set).
fn bind_report(path: &Path, revision: &str, margin: f64, rows: &[CheckRow]) -> Result<()> {
    let (text, _) = read_input(path, 512 * 1024 * 1024, "antenna_report")?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let stale = |m: &str| fail("STALE_REPORT", format!("{}: {m}", path.display()));
    if v["schema"] != ANTENNA_SCHEMA {
        return Err(stale("not a rustroad.antenna-check.v1 report"));
    }
    if v["workspace_revision"] != revision {
        return Err(stale("report revision differs from --base-rev"));
    }
    if v["safety_margin"]["fraction"].as_f64() != Some(margin) {
        return Err(stale("report safety margin differs"));
    }
    if v["scope"]["extra_shapes"].as_u64() != Some(0) {
        return Err(stale("report contains what-if shapes"));
    }
    let key = |net: &str, layer: &str, kind: &str| format!("{net}\u{0}{layer}\u{0}{kind}");
    let theirs: BTreeSet<String> = v["checks"]
        .as_array()
        .ok_or_else(|| stale("report has no checks"))?
        .iter()
        .filter(|c| c["violated"] == true)
        .map(|c| {
            key(
                c["net"].as_str().unwrap_or(""),
                c["layer"].as_str().unwrap_or(""),
                c["kind"].as_str().unwrap_or(""),
            )
        })
        .collect();
    let ours: BTreeSet<String> = rows
        .iter()
        .filter(|r| r.violated)
        .map(|r| key(&r.net, &r.layer, &r.kind))
        .collect();
    if v["violations"].as_u64() != Some(ours.len() as u64) || theirs != ours {
        return Err(stale("violated rows differ from the recomputed check"));
    }
    Ok(())
}

pub fn repair_antenna(
    path: &Path,
    base_rev: &str,
    request: &RepairRequest,
    limits: &Limits,
) -> Result<RepairReport> {
    use rustroad_core::jumper::{Jumper, JumperPlanner, apply};
    check_budgets(&request.antenna)?;
    if request.antenna.extra_shapes.is_some() {
        return Err(fail(
            "INVALID_ARGUMENT",
            "repair-antenna takes no what-if shapes",
        ));
    }
    if request.max_jumpers_per_net == 0
        || request.max_jumpers_per_net > 64
        || request.max_jumpers == 0
        || request.max_jumpers > 1_000_000
    {
        return Err(fail(
            "INVALID_ARGUMENT",
            "jumper budgets outside hard bounds",
        ));
    }
    request.jumper.validate()?;
    let lock = crate::lock(path, false, true)?;
    let current = crate::head(path)?;
    crate::check_base(base_rev, &current)?;
    let snapshot = crate::load(path, lock, limits)?;
    let mut metadata = snapshot.metadata().clone();
    let placed: Vec<_> = snapshot.records().collect();
    let design = metadata.design.clone();
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("NO_PHYSICAL", "import a PDK design first"))?;
    let state = bundle
        .routing
        .as_ref()
        .ok_or_else(|| fail("NO_ROUTING", "route before the antenna repair"))?;
    let prep = prepare(&design, bundle, &placed, state)?;
    let model = &bundle.connectivity;
    let route_limits = state.config.limits.clone();
    let mut stats = AntennaStats::default();
    let mut qualified = false;
    let (rows_before, _) = evaluate_all(
        &prep,
        &design,
        model,
        &state.routes,
        &request.antenna,
        &mut stats,
        &mut qualified,
        Vec::new(),
    )?;
    if let Some(report) = &request.report {
        bind_report(
            report,
            &current,
            request.antenna.safety_margin,
            &rows_before,
        )?;
    }
    let before = count(&rows_before);
    let mut routes = state.routes.clone();
    let wire_stats =
        |r: &core_routing::Routes| (r.stats.wire_length_dbu, r.wires.len(), r.vias.len());
    let (len0, wires0, vias0) = wire_stats(&routes);
    let mut jumpers = Vec::new();
    let mut unrepaired = Vec::new();
    let mut evaluated = 0usize;
    let mut planner_work = 0usize;
    if !before.nets.is_empty() {
        let input = &prep.input;
        let routed = logical_routed(&routes, model.nets.len() as u32);
        let mut planner = JumperPlanner::new(input, &routes, &route_limits)?;
        let trace = std::env::var_os("RUSTROAD_JUMPER_TRACE").is_some();
        let name_of = |l: u32| input.layers[l as usize].name.clone();
        let layer_id = |name: &str| -> Option<u32> {
            input
                .layers
                .iter()
                .position(|l| l.name == name)
                .map(|i| i as u32)
        };
        let rank: HashMap<u32, usize> = prep
            .order
            .iter()
            .enumerate()
            .map(|(i, &l)| (l, i))
            .collect();
        for net_name in &before.nets {
            let net = model
                .nets
                .iter()
                .position(|n| &n.name == net_name)
                .ok_or_else(|| fail("ANTENNA_REPAIR", format!("net {net_name} absent")))?
                as u32;
            let gate_names: Vec<(String, u32)> = input
                .terminals
                .iter()
                .enumerate()
                .filter(|(_, t)| t.net == net)
                .filter_map(|(ti, t)| {
                    let (name, gate, _, _) = terminal_info(&prep, &design, model, ti);
                    (gate > 0).then_some((name, t.pin))
                })
                .collect();
            let mut scratch = AntennaStats::default();
            let eval =
                |r: &core_routing::Routes, scratch: &mut AntennaStats| -> Result<Vec<CheckRow>> {
                    let shapes = net_shapes(&prep, r, &routed, Some(net))?;
                    let empty = Vec::new();
                    evaluate_net(
                        &prep,
                        &design,
                        model,
                        net,
                        shapes.get(&net).unwrap_or(&empty),
                        &request.antenna,
                        scratch,
                        &mut false,
                    )
                };
            let mut inserted = 0usize;
            loop {
                let rows = eval(&routes, &mut scratch)?;
                let bad: Vec<&CheckRow> = rows.iter().filter(|r| r.violated).collect();
                let Some(row) = bad
                    .iter()
                    .min_by_key(|r| layer_id(&r.layer).and_then(|l| rank.get(&l)).copied())
                    .copied()
                else {
                    break;
                };
                let stop = |reason: &str| Unrepaired {
                    net: net_name.clone(),
                    layer: row.layer.clone(),
                    ratio: row.ratio,
                    effective_limit: row.effective_limit,
                    reason: reason.into(),
                };
                if inserted >= request.max_jumpers_per_net || jumpers.len() >= request.max_jumpers {
                    unrepaired.push(stop("jumper budget reached"));
                    break;
                }
                let layer = layer_id(&row.layer)
                    .ok_or_else(|| fail("ANTENNA_REPAIR", "row layer is not a route layer"))?;
                if input.layers[layer as usize].vertical.is_none() {
                    unrepaired.push(stop(
                        "cut-layer rule: a jumper cannot shorten a via partial net",
                    ));
                    break;
                }
                if planner.upper_of(layer).is_none() {
                    unrepaired.push(stop("no routing layer above the violating layer"));
                    break;
                }
                let excess0 = excess_by_layer(&rows);
                let violated0: BTreeSet<String> = bad
                    .iter()
                    .map(|r| format!("{}\u{0}{}", r.layer, r.kind))
                    .collect();
                let x0 = excess0[&row.layer].0;
                let tried = evaluated;
                // (layer excess after, clean, edited routes, jumpers, rows)
                type Choice = (f64, bool, core_routing::Routes, Vec<Jumper>, Vec<CheckRow>);
                let judge = |trial: core_routing::Routes,
                             chosen: Vec<Jumper>,
                             scratch: &mut AntennaStats|
                 -> Result<Option<Choice>> {
                    let after = eval(&trial, scratch)?;
                    let fresh = after.iter().any(|r| {
                        r.violated && !violated0.contains(&format!("{}\u{0}{}", r.layer, r.kind))
                    });
                    let x = excess_by_layer(&after).get(&row.layer).map_or(0.0, |e| e.0);
                    if trace {
                        eprintln!(
                            "[jumper] {net_name} {} x0 {x0:.3} -> {x:.3} fresh {fresh} jumpers {:?}",
                            row.layer,
                            chosen
                                .iter()
                                .map(|c| (c.gate_side, c.far_side))
                                .collect::<Vec<_>>()
                        );
                    }
                    if fresh || x >= x0 {
                        return Ok(None);
                    }
                    let clean = !after.iter().any(|r| r.violated);
                    Ok(Some((x, clean, trial, chosen, after)))
                };
                let better = |a: &Choice, b: &Option<Choice>| -> bool {
                    b.as_ref()
                        .is_none_or(|b| (a.1 && !b.1) || (a.1 == b.1 && a.0 < b.0))
                };
                // A: one jumper on the longest runs of the gates' partial
                // net; the first that clears the net, else the largest drop.
                // Seeds: the gates of the violating partial nets of this layer.
                let row_gates: BTreeSet<&String> = bad
                    .iter()
                    .filter(|r| r.layer == row.layer)
                    .flat_map(|r| r.gates.iter())
                    .collect();
                let seeds: Vec<u32> = gate_names
                    .iter()
                    .filter(|(n, _)| row_gates.contains(n))
                    .map(|(_, p)| *p)
                    .collect();
                let candidates =
                    planner.candidates(&routes, net, layer, &seeds, &request.jumper)?;
                let mut best: Option<Choice> = None;
                for c in candidates {
                    evaluated += 1;
                    let trial = apply(input, &routes, &c)?;
                    if let Some(choice) = judge(trial, vec![c], &mut scratch)?
                        && better(&choice, &best)
                    {
                        let clean = choice.1;
                        best = Some(choice);
                        if clean {
                            break;
                        }
                    }
                }
                // B: isolate every gate of the violating partial net with a
                // jumper on its nearest run (multi-sink trees: the trunk
                // keeps no gate once each sink is cut off).
                if !best.as_ref().is_some_and(|b| b.1) {
                    let nearest = rustroad_core::jumper::JumperLimits {
                        nearest_first: true,
                        per_run: 1,
                        ..request.jumper.clone()
                    };
                    let mut trial = routes.clone();
                    let mut chosen = Vec::new();
                    for (name, pin) in &gate_names {
                        if !row_gates.contains(name)
                            || inserted + chosen.len() >= request.max_jumpers_per_net
                        {
                            continue;
                        }
                        let c = planner.candidates(&trial, net, layer, &[*pin], &nearest)?;
                        if let Some(c) = c.into_iter().next() {
                            evaluated += 1;
                            trial = apply(input, &trial, &c)?;
                            chosen.push(c);
                        }
                    }
                    if !chosen.is_empty()
                        && let Some(choice) = judge(trial, chosen, &mut scratch)?
                        && better(&choice, &best)
                    {
                        best = Some(choice);
                    }
                }
                let Some((_, _, trial, chosen, after)) = best else {
                    unrepaired.push(stop(if evaluated == tried {
                        "no legal jumper position on the gates' partial net"
                    } else {
                        "no jumper candidate lowers the partial-net ratio"
                    }));
                    break;
                };
                let e1 = excess_by_layer(&after);
                let (_, r0, lim) = excess0[&row.layer];
                for c in &chosen {
                    planner.commit(c)?;
                    jumpers.push(JumperRecord {
                        net: net_name.clone(),
                        layer: name_of(c.layer),
                        upper: name_of(c.upper),
                        via: input.vias[c.via as usize].name.clone(),
                        gate_side: c.gate_side,
                        far_side: c.far_side,
                        gate_distance_um: c.gate_distance as f64 / f64::from(prep.dbu),
                        run_length_um: c.run_length as f64 / f64::from(prep.dbu),
                        ratio_before: r0,
                        ratio_after: e1.get(&row.layer).map_or(0.0, |e| e.1),
                        effective_limit: lim,
                    });
                }
                routes = trial;
                inserted += chosen.len();
            }
        }
        planner_work = planner.work();
    }
    let mut stats_after = AntennaStats::default();
    let (rows_after, _) = evaluate_all(
        &prep,
        &design,
        model,
        &routes,
        &request.antenna,
        &mut stats_after,
        &mut qualified,
        Vec::new(),
    )?;
    let after = count(&rows_after);
    let (len1, wires1, vias1) = wire_stats(&routes);
    let status = if before.violations == 0 {
        "clean"
    } else if jumpers.is_empty() {
        "unrepaired"
    } else if after.violations == 0 {
        "repaired"
    } else {
        "partial"
    };
    let mut revision = current.clone();
    if !jumpers.is_empty() {
        core_routing::validate(&prep.input, &routes, &route_limits)?;
        let bundle = metadata.physical.as_mut().unwrap();
        bundle.routing.as_mut().unwrap().routes = routes;
        metadata.parent_revision = current.clone();
        drop(prep);
        revision = crate::commit(path, metadata, &placed)?.revision;
    }
    Ok(RepairReport {
        schema: REPAIR_SCHEMA,
        status,
        method: "layer jumpers: the violating layer's straight run nearest the gates is cut and \
                 bridged on the next routing layer up (two vias and a short wire); candidates \
                 are accepted only when the native partial-net ratio of the net drops without a \
                 new violated row; netlist and placement unchanged",
        design: design.name.clone(),
        base_revision: current,
        revision,
        safety_margin: request.antenna.safety_margin,
        before,
        after,
        jumpers,
        unrepaired,
        stats: RepairStats {
            candidates_evaluated: evaluated,
            planner_work,
            wire_length_dbu_before: len0,
            wire_length_dbu_after: len1,
            wires_before: wires0,
            wires_after: wires1,
            vias_before: vias0,
            vias_after: vias1,
        },
        drc_status: "unchecked",
        antenna_status: "native_only",
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
