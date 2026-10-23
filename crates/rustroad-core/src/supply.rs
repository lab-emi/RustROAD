//! Bounded supply geometry construction from a passing primary-rail report.
//!
//! Pre-implementation estimate: <=200k input rail spans, one geometry scope,
//! no per-thread copies. Sorted/deduplicated stripes (<32 MiB), <=200002 wires,
//! <=200k vias and two ports with <=128-byte names (<128 MiB including vector
//! capacity/name copies) keep derived state <256 MiB. Caller retains the input
//! report/master data separately. No paths, clauses, graph traversal or pairwise
//! intersection matrix is materialized. Sort work and all output counts are
//! budgeted. Tiny tests use <8 KiB. Run two workers under repository OS guards.
//!
//! Wires have square endcaps, so the inclusive bounding box extends half-width
//! beyond each endpoint. Construction/enclosure checks use that same convention.
//! This is a restricted two-spine rectangular topology, not foundry DRC or IR
//! analysis. Fixed-via technology geometry and independent checking remain vital.

use crate::power::{RailPolarity, RailReport};
use crate::{Rect, valid_object_name};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerSpec {
    pub name: String,
    /// Minimum requested width; widened/aligned to enclose upper via metal.
    pub width: i64,
    pub spacing: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViaSpec {
    pub name: String,
    /// Fixed via lower-metal rectangles, relative to its origin.
    pub lower: Vec<Rect>,
    /// Fixed via upper-metal rectangles, relative to its origin.
    pub upper: Vec<Rect>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyLimits {
    pub max_rails: usize,
    pub max_wires: usize,
    pub max_vias: usize,
    pub max_work: usize,
    pub max_via_shapes: usize,
}
impl Default for SupplyLimits {
    fn default() -> Self {
        Self {
            max_rails: 200_000,
            max_wires: 200_002,
            max_vias: 200_000,
            max_work: 20_000_000,
            max_via_shapes: 256,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyWire {
    pub polarity: RailPolarity,
    pub layer: String,
    pub width: i64,
    pub start: [i64; 2],
    pub end: [i64; 2],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyVia {
    pub polarity: RailPolarity,
    pub name: String,
    pub at: [i64; 2],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyPort {
    pub polarity: RailPolarity,
    pub layer: String,
    pub rect: Rect,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyGrid {
    pub wires: Vec<SupplyWire>,
    pub vias: Vec<SupplyVia>,
    pub ports: Vec<SupplyPort>,
    pub work_units: usize,
    pub scope: String,
    pub drc_status: String,
}
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct SupplyError {
    pub code: &'static str,
    pub message: String,
}
type Result<T> = std::result::Result<T, SupplyError>;
fn failure(code: &'static str, message: impl Into<String>) -> SupplyError {
    SupplyError {
        code,
        message: message.into(),
    }
}
fn invalid(message: impl Into<String>) -> SupplyError {
    failure("INVALID_SUPPLY_INPUT", message)
}
fn overflow() -> SupplyError {
    failure("SUPPLY_GEOMETRY_OVERFLOW", "integer coordinate overflow")
}
fn bounded_i64(value: i128) -> Result<i64> {
    i64::try_from(value).map_err(|_| overflow())
}
fn ceil_grid(value: i128, grid: i64) -> Result<i64> {
    let g = i128::from(grid);
    bounded_i64((value.div_euclid(g) + i128::from(value.rem_euclid(g) != 0)) * g)
}
fn floor_grid(value: i128, grid: i64) -> Result<i64> {
    let g = i128::from(grid);
    bounded_i64(value.div_euclid(g) * g)
}
fn aligned(value: i64, grid: i64) -> bool {
    value.rem_euclid(grid) == 0
}
fn rectangle_valid(rect: Rect) -> bool {
    rect.lx < rect.ux && rect.ly < rect.uy
}
fn contains(outer: Rect, inner: Rect) -> bool {
    outer.lx <= inner.lx && outer.ly <= inner.ly && outer.ux >= inner.ux && outer.uy >= inner.uy
}
fn in_die(die: Rect, rect: Rect) -> Result<()> {
    if !rectangle_valid(rect) || !contains(die, rect) {
        return Err(failure(
            "SUPPLY_OUTSIDE_DIE",
            "constructed metal or port is not entirely inside die",
        ));
    }
    Ok(())
}
fn translated(rect: Rect, at: [i64; 2]) -> Result<Rect> {
    Ok(Rect {
        lx: bounded_i64(i128::from(rect.lx) + i128::from(at[0]))?,
        ly: bounded_i64(i128::from(rect.ly) + i128::from(at[1]))?,
        ux: bounded_i64(i128::from(rect.ux) + i128::from(at[0]))?,
        uy: bounded_i64(i128::from(rect.uy) + i128::from(at[1]))?,
    })
}
/// Square-capped metal extent used both for construction and independent export.
pub fn wire_bounds(wire: &SupplyWire) -> Result<Rect> {
    if wire.width <= 0 || wire.width % 2 != 0 {
        return Err(invalid("wire width must be positive and even"));
    }
    if wire.start[0] != wire.end[0] && wire.start[1] != wire.end[1] {
        return Err(invalid("supply wire is not orthogonal"));
    }
    let half = i128::from(wire.width / 2);
    Ok(Rect {
        lx: bounded_i64(i128::from(wire.start[0].min(wire.end[0])) - half)?,
        ly: bounded_i64(i128::from(wire.start[1].min(wire.end[1])) - half)?,
        ux: bounded_i64(i128::from(wire.start[0].max(wire.end[0])) + half)?,
        uy: bounded_i64(i128::from(wire.start[1].max(wire.end[1])) + half)?,
    })
}
struct Work {
    used: usize,
    max: usize,
}
impl Work {
    fn tick(&mut self, n: usize) -> Result<()> {
        self.used = self
            .used
            .checked_add(n)
            .ok_or_else(|| failure("SUPPLY_BUDGET_EXCEEDED", "work counter overflow"))?;
        if self.used > self.max {
            return Err(failure("SUPPLY_BUDGET_EXCEEDED", "max_work exceeded"));
        }
        Ok(())
    }
    fn sort(&mut self, n: usize) -> Result<()> {
        let log = if n < 2 {
            0
        } else {
            usize::BITS as usize - n.leading_zeros() as usize
        };
        self.tick(
            n.checked_mul(log)
                .and_then(|v| v.checked_mul(2))
                .ok_or_else(|| failure("SUPPLY_BUDGET_EXCEEDED", "sort allowance overflow"))?,
        )
    }
}
#[derive(Clone, Copy)]
struct Stripe {
    polarity: RailPolarity,
    x0: i64,
    x1: i64,
    y0: i64,
    y1: i64,
}
#[derive(Clone, Copy)]
struct Route {
    polarity: RailPolarity,
    x0: i64,
    x1: i64,
    y: i64,
    width: i64,
}
fn via_bounds(shapes: &[Rect], grid: i64, work: &mut Work) -> Result<Rect> {
    let first = *shapes
        .first()
        .ok_or_else(|| invalid("fixed via must provide lower and upper metal geometry"))?;
    let mut bounds = first;
    for shape in shapes {
        work.tick(1)?;
        if !rectangle_valid(*shape)
            || [shape.lx, shape.ly, shape.ux, shape.uy]
                .iter()
                .any(|v| !aligned(*v, grid))
        {
            return Err(invalid(
                "fixed via metal rectangle is invalid or off manufacturing grid",
            ));
        }
        bounds.lx = bounds.lx.min(shape.lx);
        bounds.ly = bounds.ly.min(shape.ly);
        bounds.ux = bounds.ux.max(shape.ux);
        bounds.uy = bounds.uy.max(shape.uy);
    }
    Ok(bounds)
}

/// Construct a two-spine network in the left die/core margin. Power is the
/// left spine and ground the right. Both have connected bottom-edge ports.
/// The caller must serialize square wire endcaps consistently and must connect
/// logical supply endpoints separately. No DRC/signoff status is asserted.
#[allow(clippy::too_many_arguments)]
pub fn build_supply_grid(
    die: Rect,
    core: Rect,
    grid: i64,
    rails: &RailReport,
    rail_layer: &str,
    rail_min_width: i64,
    strap: &LayerSpec,
    via: &ViaSpec,
    limits: &SupplyLimits,
) -> Result<SupplyGrid> {
    if grid <= 0 || rail_min_width <= 0 || strap.width <= 0 || strap.spacing < 0 {
        return Err(invalid(
            "grid and widths must be positive; spacing must be nonnegative",
        ));
    }
    if !rectangle_valid(die) || !rectangle_valid(core) || !contains(die, core) {
        return Err(invalid("core must be a nonempty rectangle inside die"));
    }
    if [
        die.lx, die.ly, die.ux, die.uy, core.lx, core.ly, core.ux, core.uy,
    ]
    .iter()
    .any(|v| !aligned(*v, grid))
    {
        return Err(invalid("die/core edges must lie on manufacturing grid"));
    }
    for name in [rail_layer, strap.name.as_str(), via.name.as_str()] {
        if !valid_object_name(name) || name.len() > 128 {
            return Err(invalid(
                "layer/via name must be valid and at most 128 bytes",
            ));
        }
    }
    if strap.name == rail_layer {
        return Err(invalid("spine and rail must use distinct metal layers"));
    }
    if rails.status != "pass"
        || !rails.diagnostics.is_empty()
        || rails.rail_layer != rail_layer
        || rails.row_rails.is_empty()
    {
        return Err(invalid(
            "a nonempty passing report for the selected rail layer is required",
        ));
    }
    if rails.row_rails.len() > limits.max_rails
        || via
            .lower
            .len()
            .checked_add(via.upper.len())
            .is_none_or(|n| n > limits.max_via_shapes)
    {
        return Err(failure(
            "SUPPLY_BUDGET_EXCEEDED",
            "rail/via shape input cap exceeded",
        ));
    }
    if !rails
        .row_rails
        .iter()
        .any(|r| r.polarity == RailPolarity::Power)
        || !rails
            .row_rails
            .iter()
            .any(|r| r.polarity == RailPolarity::Ground)
    {
        return Err(invalid("both primary supply polarities must be present"));
    }
    let mut work = Work {
        used: 0,
        max: limits.max_work,
    };
    let lower = via_bounds(&via.lower, grid, &mut work)?;
    let upper = via_bounds(&via.upper, grid, &mut work)?;
    let mut sorted = Vec::with_capacity(rails.row_rails.len());
    for span in &rails.row_rails {
        work.tick(1)?;
        if span.x_start >= span.x_end
            || span.y_min >= span.y_max
            || span.x_start < core.lx
            || span.x_end > core.ux
            || !aligned(span.x_start, grid)
            || !aligned(span.x_end, grid)
        {
            return Err(invalid(
                "rail span is invalid, outside core x extent, or off-grid",
            ));
        }
        sorted.push(Stripe {
            polarity: span.polarity,
            x0: span.x_start,
            x1: span.x_end,
            y0: span.y_min,
            y1: span.y_max,
        });
    }
    work.sort(sorted.len())?;
    sorted.sort_unstable_by_key(|s| (s.y0, s.y1, s.polarity, s.x0, s.x1));
    let mut merged: Vec<Stripe> = Vec::with_capacity(sorted.len());
    let mut furthest: [Option<i64>; 2] = [None, None];
    for stripe in sorted {
        work.tick(1)?;
        let own = match stripe.polarity {
            RailPolarity::Power => 0,
            RailPolarity::Ground => 1,
        };
        if furthest[1 - own].is_some_and(|end| stripe.y0 <= end) {
            return Err(failure(
                "SUPPLY_POLARITY_OVERLAP",
                "different original supply bands overlap or touch",
            ));
        }
        furthest[own] = Some(furthest[own].map_or(stripe.y1, |end| end.max(stripe.y1)));
        if let Some(last) = merged.last_mut() {
            if stripe.y0 <= last.y1 && stripe.polarity != last.polarity {
                return Err(failure(
                    "SUPPLY_POLARITY_OVERLAP",
                    "different supply rail bands overlap or touch",
                ));
            }
            if stripe.y0 < last.y1 && stripe.polarity == last.polarity {
                // A single common physical strip must exist for every merged row.
                last.y0 = last.y0.max(stripe.y0);
                last.y1 = last.y1.min(stripe.y1);
                last.x0 = last.x0.min(stripe.x0);
                last.x1 = last.x1.max(stripe.x1);
                continue;
            }
        }
        merged.push(stripe);
    }
    if merged.len() > limits.max_vias
        || merged
            .len()
            .checked_add(2)
            .is_none_or(|n| n > limits.max_wires)
    {
        return Err(failure(
            "SUPPLY_BUDGET_EXCEEDED",
            "wire/via output count cap exceeded",
        ));
    }
    let mut routes = Vec::with_capacity(merged.len());
    let required_rail_half = ceil_grid(
        ((i128::from(rail_min_width) + 1) / 2)
            .max(-i128::from(lower.ly))
            .max(i128::from(lower.uy)),
        grid,
    )?;
    let strap_half = ceil_grid(
        ((i128::from(strap.width) + 1) / 2)
            .max(-i128::from(upper.lx))
            .max(i128::from(upper.ux)),
        grid,
    )?;
    let strap_width = bounded_i64(2 * i128::from(strap_half))?;
    // Distinct supply nets must not touch even when the caller's minimum is zero.
    let spacing = ceil_grid(i128::from(strap.spacing.max(grid)), grid)?;
    let mut max_rail_half = 0;
    for stripe in merged {
        work.tick(1)?;
        let low = ceil_grid(i128::from(stripe.y0), grid)?;
        let high = floor_grid(i128::from(stripe.y1), grid)?;
        let center_low = bounded_i64(i128::from(low) + i128::from(required_rail_half))?;
        let center_high = bounded_i64(i128::from(high) - i128::from(required_rail_half))?;
        if center_low > center_high {
            return Err(failure(
                "INSUFFICIENT_RAIL_BAND",
                "common rail strip cannot enclose the required width and via",
            ));
        }
        let midpoint = (i128::from(low) + i128::from(high)) / 2;
        let y = floor_grid(midpoint, grid)?.clamp(center_low, center_high);
        let half = (y - low).min(high - y);
        let width = bounded_i64(2 * i128::from(half))?;
        max_rail_half = max_rail_half.max(half);
        routes.push(Route {
            polarity: stripe.polarity,
            x0: stripe.x0,
            x1: stripe.x1,
            y,
            width,
        });
    }
    let left_extent = i128::from(strap_half)
        .max(i128::from(max_rail_half))
        .max(-i128::from(lower.lx))
        .max(-i128::from(upper.lx));
    let power_x = ceil_grid(i128::from(die.lx) + left_extent, grid)?;
    let ground_x =
        bounded_i64(i128::from(power_x) + i128::from(strap_width) + i128::from(spacing))?;
    if i128::from(ground_x) + i128::from(strap_half) > i128::from(core.lx) {
        return Err(failure(
            "INSUFFICIENT_SUPPLY_MARGIN",
            "two enclosed, separated spines do not fit in the left die/core margin",
        ));
    }
    let bottom = bounded_i64(i128::from(die.ly) + i128::from(strap_half))?;
    let upper_end = routes
        .iter()
        .map(|r| i128::from(r.y) + i128::from(upper.uy).max(0))
        .max()
        .unwrap();
    let top = ceil_grid(upper_end.max(i128::from(bottom)), grid)?;
    let mut wires = Vec::with_capacity(routes.len() + 2);
    let mut ports = Vec::with_capacity(2);
    for (polarity, x) in [
        (RailPolarity::Power, power_x),
        (RailPolarity::Ground, ground_x),
    ] {
        work.tick(1)?;
        let wire = SupplyWire {
            polarity,
            layer: strap.name.clone(),
            width: strap_width,
            start: [x, bottom],
            end: [x, top],
        };
        in_die(die, wire_bounds(&wire)?)?;
        wires.push(wire);
        let rect = Rect {
            lx: bounded_i64(i128::from(x) - i128::from(strap_half))?,
            ly: die.ly,
            ux: bounded_i64(i128::from(x) + i128::from(strap_half))?,
            uy: bounded_i64(i128::from(die.ly) + i128::from(strap_width))?,
        };
        in_die(die, rect)?;
        ports.push(SupplyPort {
            polarity,
            layer: strap.name.clone(),
            rect,
        });
    }
    let mut vias = Vec::with_capacity(routes.len());
    for route in routes {
        work.tick(1)?;
        let x = match route.polarity {
            RailPolarity::Power => power_x,
            RailPolarity::Ground => ground_x,
        };
        let half = i128::from(route.width / 2);
        let start = ceil_grid(
            i128::from(route.x0).min(i128::from(x) + (i128::from(lower.lx) + half).min(0)),
            grid,
        )?;
        let end = floor_grid(
            i128::from(route.x1).max(i128::from(x) + (i128::from(lower.ux) - half).max(0)),
            grid,
        )?;
        let wire = SupplyWire {
            polarity: route.polarity,
            layer: rail_layer.into(),
            width: route.width,
            start: [start, route.y],
            end: [end, route.y],
        };
        let bounds = wire_bounds(&wire)?;
        in_die(die, bounds)?;
        let at = [x, route.y];
        for rect in &via.lower {
            work.tick(1)?;
            let shape = translated(*rect, at)?;
            in_die(die, shape)?;
            if !contains(bounds, shape) {
                return Err(failure(
                    "VIA_ENCLOSURE_MISMATCH",
                    "lower via metal is outside its rail wire",
                ));
            }
        }
        let spine = &wires[match route.polarity {
            RailPolarity::Power => 0,
            RailPolarity::Ground => 1,
        }];
        let spine_bounds = wire_bounds(spine)?;
        for rect in &via.upper {
            work.tick(1)?;
            let shape = translated(*rect, at)?;
            in_die(die, shape)?;
            if !contains(spine_bounds, shape) {
                return Err(failure(
                    "VIA_ENCLOSURE_MISMATCH",
                    "upper via metal is outside its spine",
                ));
            }
        }
        wires.push(wire);
        vias.push(SupplyVia {
            polarity: route.polarity,
            name: via.name.clone(),
            at,
        });
    }
    Ok(SupplyGrid {
        wires,
        vias,
        ports,
        work_units: work.used,
        scope: "two_left_spines_and_primary_row_rails".into(),
        drc_status: "unchecked".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::{RailEdge, RailSpan};
    fn report() -> RailReport {
        let stripe = |row: u32, edge: RailEdge, polarity: RailPolarity, y: i64| RailSpan {
            row,
            edge,
            polarity,
            x_start: 100,
            x_end: 500,
            y_min: y - 10,
            y_max: y + 10,
        };
        RailReport {
            status: "pass".into(),
            scope: "test".into(),
            rail_layer: "M1".into(),
            occupied_rows: 2,
            shared_boundaries_checked: 1,
            intervals_checked: 8,
            work_units: 10,
            row_rails: vec![
                stripe(0, RailEdge::Bottom, RailPolarity::Ground, 100),
                stripe(0, RailEdge::Top, RailPolarity::Power, 200),
                stripe(1, RailEdge::Bottom, RailPolarity::Power, 200),
                stripe(1, RailEdge::Top, RailPolarity::Ground, 300),
            ],
            diagnostics: vec![],
            body_bias_status: "unchecked".into(),
            drc_status: "unchecked".into(),
            ir_drop_status: "unchecked".into(),
        }
    }
    fn via() -> ViaSpec {
        ViaSpec {
            name: "V12".into(),
            lower: vec![Rect {
                lx: -4,
                ly: -2,
                ux: 4,
                uy: 2,
            }],
            upper: vec![Rect {
                lx: -2,
                ly: -4,
                ux: 2,
                uy: 4,
            }],
        }
    }
    fn strap() -> LayerSpec {
        LayerSpec {
            name: "M2".into(),
            width: 8,
            spacing: 4,
        }
    }
    fn die() -> Rect {
        Rect {
            lx: 0,
            ly: 0,
            ux: 600,
            uy: 400,
        }
    }
    fn core() -> Rect {
        Rect {
            lx: 100,
            ly: 100,
            ux: 500,
            uy: 300,
        }
    }
    fn build(rails: &RailReport) -> Result<SupplyGrid> {
        build_supply_grid(
            die(),
            core(),
            2,
            rails,
            "M1",
            6,
            &strap(),
            &via(),
            &SupplyLimits::default(),
        )
    }
    #[test]
    fn deduplicates_shared_rails_and_connects_two_bottom_ports() {
        let grid = build(&report()).unwrap();
        assert_eq!(grid.wires.len(), 5);
        assert_eq!(grid.vias.len(), 3);
        assert_eq!(grid.ports.len(), 2);
        assert_eq!(
            grid.vias
                .iter()
                .filter(|v| v.polarity == RailPolarity::Power)
                .count(),
            1
        );
        for port in &grid.ports {
            assert_eq!(port.rect.ly, die().ly);
            assert!(contains(
                wire_bounds(
                    &grid.wires[if port.polarity == RailPolarity::Power {
                        0
                    } else {
                        1
                    }]
                )
                .unwrap(),
                port.rect
            ));
        }
        for wire in &grid.wires {
            assert!(contains(die(), wire_bounds(wire).unwrap()));
            assert_eq!(wire.width % 4, 0);
            assert!(wire.start.into_iter().chain(wire.end).all(|v| v % 2 == 0));
        }
        assert_eq!(grid.drc_status, "unchecked");
    }
    #[test]
    fn rejects_missing_margin_bad_report_and_opposite_polarity_overlap() {
        let shallow = Rect { lx: 20, ..core() };
        assert_eq!(
            build_supply_grid(
                die(),
                shallow,
                2,
                &report(),
                "M1",
                6,
                &strap(),
                &via(),
                &SupplyLimits::default()
            )
            .unwrap_err()
            .code,
            "INSUFFICIENT_SUPPLY_MARGIN"
        );
        let mut bad = report();
        bad.status = "fail".into();
        assert!(build(&bad).is_err());
        let mut bad = report();
        bad.row_rails[2].polarity = RailPolarity::Ground;
        assert_eq!(build(&bad).unwrap_err().code, "SUPPLY_POLARITY_OVERLAP");
    }
    #[test]
    fn upper_via_enclosure_widens_spines_and_spacing_is_preserved() {
        let mut via = via();
        via.upper[0].lx = -9;
        via.upper[0].ux = 9;
        // Move enclosure edges onto a one-unit manufacturing grid.
        let output = build_supply_grid(
            die(),
            core(),
            1,
            &report(),
            "M1",
            6,
            &strap(),
            &via,
            &SupplyLimits::default(),
        )
        .unwrap();
        assert_eq!(output.wires[0].width, 18);
        let power = wire_bounds(&output.wires[0]).unwrap();
        let ground = wire_bounds(&output.wires[1]).unwrap();
        assert!(ground.lx - power.ux >= 4);
    }
    #[test]
    fn rejects_insufficient_band_off_grid_and_outside_die() {
        let mut bad = report();
        bad.row_rails[0].y_min = 98;
        bad.row_rails[0].y_max = 102;
        assert_eq!(build(&bad).unwrap_err().code, "INSUFFICIENT_RAIL_BAND");
        let mut bad = report();
        bad.row_rails[0].x_start = 101;
        assert!(build(&bad).is_err());
        let mut bad = report();
        bad.row_rails.last_mut().unwrap().y_min = 394;
        bad.row_rails.last_mut().unwrap().y_max = 414;
        assert_eq!(build(&bad).unwrap_err().code, "SUPPLY_OUTSIDE_DIE");
    }
    #[test]
    fn rejects_work_and_output_overruns_without_partial_grid() {
        for limits in [
            SupplyLimits {
                max_work: 1,
                ..Default::default()
            },
            SupplyLimits {
                max_vias: 2,
                ..Default::default()
            },
            SupplyLimits {
                max_wires: 4,
                ..Default::default()
            },
            SupplyLimits {
                max_rails: 3,
                ..Default::default()
            },
        ] {
            assert_eq!(
                build_supply_grid(
                    die(),
                    core(),
                    2,
                    &report(),
                    "M1",
                    6,
                    &strap(),
                    &via(),
                    &limits
                )
                .unwrap_err()
                .code,
                "SUPPLY_BUDGET_EXCEEDED"
            );
        }
    }
    #[test]
    fn zero_requested_spacing_still_separates_supply_metals() {
        let mut spec = strap();
        spec.spacing = 0;
        let output = build_supply_grid(
            die(),
            core(),
            2,
            &report(),
            "M1",
            6,
            &spec,
            &via(),
            &SupplyLimits::default(),
        )
        .unwrap();
        let left = wire_bounds(&output.wires[0]).unwrap();
        let right = wire_bounds(&output.wires[1]).unwrap();
        assert!(right.lx - left.ux >= 2);
    }
}
