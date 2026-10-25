//! Bounded primary-row-rail geometry verification. Read docs/power-pdk-notes.md
//! for the pre-implementation memory estimate, PDK facts and coverage boundary.
//! Geometry only: no routed DRC, IR drop, body-well continuity or electrical
//! path proof. All allocations and traversal/sorting work have explicit caps.

use crate::connectivity::{Geometry, Master, PinUse};
use crate::{Orientation, Rect, transform_rect, valid_object_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerCell {
    pub instance: u32,
    pub master: u32,
    /// Lower-left of the oriented cell bounding box, not an unadjusted origin.
    pub x: i64,
    pub y: i64,
    pub orientation: Orientation,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerRow {
    pub x_start: i64,
    pub x_end: i64,
    pub y: i64,
    pub height: i64,
    pub orientation: Orientation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailConfig {
    pub power_pin: String,
    pub ground_pin: String,
    pub rail_layer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailLimits {
    pub max_cells: usize,
    pub max_rows: usize,
    pub max_master_shapes: usize,
    pub max_intervals: usize,
    pub max_work: usize,
    pub max_diagnostics: usize,
}
impl Default for RailLimits {
    fn default() -> Self {
        Self {
            max_cells: 250_000,
            max_rows: 100_000,
            max_master_shapes: 250_000,
            max_intervals: 1_000_000,
            max_work: 64_000_000,
            max_diagnostics: 256,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RailEdge {
    Bottom,
    Top,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RailPolarity {
    Power,
    Ground,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailDiagnostic {
    pub code: String,
    pub row: Option<u32>,
    pub instance: Option<u32>,
    pub x_range: Option<[i64; 2]>,
    pub message: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailSpan {
    pub row: u32,
    pub edge: RailEdge,
    pub polarity: RailPolarity,
    pub x_start: i64,
    pub x_end: i64,
    /// A positive-width y strip common to every retained rail interval.
    pub y_min: i64,
    pub y_max: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailReport {
    pub status: String,
    pub scope: String,
    pub rail_layer: String,
    pub occupied_rows: usize,
    pub shared_boundaries_checked: usize,
    pub intervals_checked: usize,
    /// Actual scan units plus a conservative precharged sorting allowance.
    pub work_units: usize,
    pub row_rails: Vec<RailSpan>,
    pub diagnostics: Vec<RailDiagnostic>,
    pub body_bias_status: String,
    pub drc_status: String,
    pub ir_drop_status: String,
}
#[derive(Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RailError {
    pub code: &'static str,
    pub message: String,
}
type Result<T> = std::result::Result<T, RailError>;
fn invalid(message: impl Into<String>) -> RailError {
    RailError {
        code: "INVALID_POWER_INPUT",
        message: message.into(),
    }
}
fn overflow() -> RailError {
    RailError {
        code: "POWER_GEOMETRY_OVERFLOW",
        message: "integer coordinate overflow".into(),
    }
}
fn budget(message: impl Into<String>) -> RailError {
    RailError {
        code: "POWER_BUDGET_EXCEEDED",
        message: message.into(),
    }
}

struct Work<'a> {
    limits: &'a RailLimits,
    units: usize,
    shapes: usize,
    diagnostics: Vec<RailDiagnostic>,
}
impl Work<'_> {
    fn tick(&mut self, amount: usize) -> Result<()> {
        self.units = self
            .units
            .checked_add(amount)
            .ok_or_else(|| budget("work counter overflow"))?;
        if self.units > self.limits.max_work {
            return Err(budget("max_work exceeded"));
        }
        Ok(())
    }
    fn sorting(&mut self, count: usize) -> Result<()> {
        let log = if count < 2 {
            0
        } else {
            usize::BITS as usize - count.leading_zeros() as usize
        };
        let units = count
            .checked_mul(log)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| budget("sorting allowance overflow"))?;
        self.tick(units)
    }
    fn issue(
        &mut self,
        code: &str,
        row: Option<u32>,
        instance: Option<u32>,
        x_range: Option<[i64; 2]>,
        message: impl Into<String>,
    ) -> Result<()> {
        if self.diagnostics.len() >= self.limits.max_diagnostics {
            return Err(budget("max_diagnostics exceeded"));
        }
        self.diagnostics.push(RailDiagnostic {
            code: code.into(),
            row,
            instance,
            x_range,
            message: message.into(),
        });
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct LocalRail {
    polarity: RailPolarity,
    rect: Rect,
}
struct Profile {
    rails: Vec<LocalRail>,
}
#[derive(Clone, Copy)]
struct Interval {
    row: usize,
    edge: RailEdge,
    polarity: RailPolarity,
    instance: u32,
    x0: i64,
    x1: i64,
    y0: i64,
    y1: i64,
}
fn expected_polarity(orientation: Orientation, edge: RailEdge) -> RailPolarity {
    match (orientation, edge) {
        (Orientation::N, RailEdge::Bottom) | (Orientation::FS, RailEdge::Top) => {
            RailPolarity::Ground
        }
        (Orientation::N, RailEdge::Top) | (Orientation::FS, RailEdge::Bottom) => {
            RailPolarity::Power
        }
    }
}
fn crosses(rect: Rect, y: i64) -> bool {
    rect.ly < y && y < rect.uy
}

fn profile(
    master: &Master,
    instance: u32,
    config: &RailConfig,
    work: &mut Work<'_>,
) -> Result<Profile> {
    if master.size[0] <= 0 || master.size[1] <= 0 {
        return Err(invalid("master dimensions must be positive"));
    }
    let mut result = Profile { rails: Vec::new() };
    for (name, polarity, pin_use) in [
        (&config.power_pin, RailPolarity::Power, PinUse::Power),
        (&config.ground_pin, RailPolarity::Ground, PinUse::Ground),
    ] {
        let mut matched = 0;
        let mut edge_shapes = 0;
        for pin in &master.pins {
            work.tick(1)?;
            if &pin.name != name {
                continue;
            }
            matched += 1;
            if pin.use_ != pin_use {
                work.issue(
                    "SUPPLY_PIN_USE_MISMATCH",
                    None,
                    Some(instance),
                    None,
                    format!("{}.{} has the wrong declared supply use", master.name, name),
                )?;
            }
            for port in &pin.ports {
                work.tick(1)?;
                for shape in port {
                    work.tick(1)?;
                    if work.shapes >= work.limits.max_master_shapes {
                        return Err(budget("max_master_shapes exceeded"));
                    }
                    work.shapes += 1;
                    match shape {
                        Geometry::Rect { layer, rect } if layer == &config.rail_layer => {
                            let rect = Rect {
                                lx: rect[0],
                                ly: rect[1],
                                ux: rect[2],
                                uy: rect[3],
                            };
                            if rect.lx >= rect.ux || rect.ly >= rect.uy {
                                return Err(invalid("invalid primary rail rectangle"));
                            }
                            if crosses(rect, 0) || crosses(rect, master.size[1]) {
                                if result.rails.len() >= work.limits.max_master_shapes {
                                    return Err(budget("cached master rail count exceeded"));
                                }
                                result.rails.push(LocalRail { polarity, rect });
                                edge_shapes += 1;
                            }
                        }
                        Geometry::Polygon { layer, .. } if layer == &config.rail_layer => {
                            work.issue(
                                "UNSUPPORTED_RAIL_POLYGON",
                                None,
                                Some(instance),
                                None,
                                format!(
                                    "{}.{} has polygon geometry on the selected rail layer",
                                    master.name, name
                                ),
                            )?;
                        }
                        _ => {}
                    }
                }
            }
        }
        if matched != 1 {
            work.issue(
                "PRIMARY_SUPPLY_PIN_MISSING",
                None,
                Some(instance),
                None,
                format!(
                    "master {} must have exactly one named pin {name}",
                    master.name
                ),
            )?;
        } else if edge_shapes == 0 {
            work.issue(
                "PRIMARY_RAIL_MISSING",
                None,
                Some(instance),
                None,
                format!(
                    "{}.{} has no rectangle strictly crossing a row edge on {}",
                    master.name, name, config.rail_layer
                ),
            )?;
        }
    }
    Ok(result)
}

/// Verify the rectangle rail union on each occupied row span and polarity at
/// adjacent shared row edges. Empty row tails are outside the declared scope.
/// A passing report still does not prove the existence/connection of straps,
/// well contacts, supply sources, or foundry DRC/IR-drop compliance.
pub fn validate_rail_continuity(
    masters: &[Master],
    cells: &[PowerCell],
    rows: &[PowerRow],
    config: &RailConfig,
    limits: &RailLimits,
) -> Result<RailReport> {
    if cells.len() > limits.max_cells || masters.len() > limits.max_cells {
        return Err(budget("max_cells/masters exceeded"));
    }
    if rows.len() > limits.max_rows || rows.len() > u32::MAX as usize {
        return Err(budget("max_rows exceeded"));
    }
    if !valid_object_name(&config.power_pin)
        || !valid_object_name(&config.ground_pin)
        || !valid_object_name(&config.rail_layer)
        || config.power_pin == config.ground_pin
    {
        return Err(invalid("invalid or identical supply pin names/layer"));
    }
    let mut work = Work {
        limits,
        units: 0,
        shapes: 0,
        diagnostics: Vec::new(),
    };
    for (i, row) in rows.iter().enumerate() {
        work.tick(1)?;
        if row.x_start >= row.x_end || row.height <= 0 {
            return Err(invalid("invalid row extent"));
        }
        let top = row.y.checked_add(row.height).ok_or_else(overflow)?;
        if i > 0 {
            let previous_top = rows[i - 1]
                .y
                .checked_add(rows[i - 1].height)
                .ok_or_else(overflow)?;
            if previous_top > row.y {
                return Err(invalid("rows must be ordered and nonoverlapping"));
            }
        }
        if top <= row.y {
            return Err(invalid("invalid row height"));
        }
    }
    let mut profiles: Vec<Option<Profile>> = (0..masters.len()).map(|_| None).collect();
    let mut occupied: Vec<Option<[i64; 2]>> = vec![None; rows.len()];
    let mut ids = HashSet::with_capacity(cells.len());
    let mut intervals = Vec::new();
    for cell in cells {
        work.tick(1)?;
        if !ids.insert(cell.instance) {
            return Err(invalid("duplicate placed instance identity"));
        }
        let mi = cell.master as usize;
        let master = masters
            .get(mi)
            .ok_or_else(|| invalid("placed master index out of range"))?;
        work.tick(if rows.is_empty() {
            0
        } else {
            usize::BITS as usize - rows.len().leading_zeros() as usize
        })?;
        let Ok(ri) = rows.binary_search_by_key(&cell.y, |row| row.y) else {
            work.issue(
                "CELL_NOT_ON_ROW",
                None,
                Some(cell.instance),
                None,
                "placed lower edge does not match a declared row",
            )?;
            continue;
        };
        let row = &rows[ri];
        let right = cell.x.checked_add(master.size[0]).ok_or_else(overflow)?;
        let top = row.y.checked_add(row.height).ok_or_else(overflow)?;
        if master.size[1] != row.height
            || cell.x < row.x_start
            || right > row.x_end
            || master.size[0] <= 0
        {
            work.issue(
                "CELL_OUTSIDE_ROW",
                Some(ri as u32),
                Some(cell.instance),
                Some([cell.x, right]),
                "master size or placed extent is incompatible with row",
            )?;
            continue;
        }
        if cell.orientation != row.orientation {
            work.issue(
                "CELL_ROW_ORIENTATION_MISMATCH",
                Some(ri as u32),
                Some(cell.instance),
                Some([cell.x, right]),
                "cell orientation differs from its declared row",
            )?;
        }
        match &mut occupied[ri] {
            Some(span) => {
                span[0] = span[0].min(cell.x);
                span[1] = span[1].max(right);
            }
            slot @ None => *slot = Some([cell.x, right]),
        }
        if profiles[mi].is_none() {
            profiles[mi] = Some(profile(master, cell.instance, config, &mut work)?);
        }
        for local in &profiles[mi].as_ref().unwrap().rails {
            work.tick(1)?;
            let rect = transform_rect(local.rect, cell.x, cell.y, master.size[1], cell.orientation)
                .map_err(|_| overflow())?;
            for (edge, y) in [(RailEdge::Bottom, row.y), (RailEdge::Top, top)] {
                work.tick(1)?;
                if crosses(rect, y) {
                    if intervals.len() >= limits.max_intervals {
                        return Err(budget("max_intervals exceeded"));
                    }
                    intervals.push(Interval {
                        row: ri,
                        edge,
                        polarity: local.polarity,
                        instance: cell.instance,
                        x0: rect.lx,
                        x1: rect.ux,
                        y0: rect.ly,
                        y1: rect.uy,
                    });
                }
            }
        }
    }
    work.sorting(intervals.len())?;
    intervals.sort_unstable_by_key(|s| (s.row, s.edge, s.x0, s.x1, s.polarity, s.instance));
    let mut cursor = 0;
    let mut row_rails = Vec::new();
    let mut occupied_rows = 0;
    for (ri, span) in occupied.iter().enumerate() {
        work.tick(1)?;
        let Some([start, end]) = *span else {
            continue;
        };
        occupied_rows += 1;
        for edge in [RailEdge::Bottom, RailEdge::Top] {
            let expected = expected_polarity(rows[ri].orientation, edge);
            let mut covered = start;
            let mut y_min = i64::MIN;
            let mut y_max = i64::MAX;
            let mut found = false;
            let mut good = true;
            while cursor < intervals.len()
                && intervals[cursor].row == ri
                && intervals[cursor].edge == edge
            {
                work.tick(1)?;
                let interval = intervals[cursor];
                cursor += 1;
                if interval.polarity != expected {
                    good = false;
                    work.issue(
                        "ROW_RAIL_POLARITY_MISMATCH",
                        Some(ri as u32),
                        Some(interval.instance),
                        Some([interval.x0, interval.x1]),
                        format!(
                            "{edge:?} edge contains {:?}, expected {expected:?}",
                            interval.polarity
                        ),
                    )?;
                    continue;
                }
                let x0 = interval.x0.max(start);
                let x1 = interval.x1.min(end);
                if x0 >= x1 {
                    continue;
                }
                found = true;
                y_min = y_min.max(interval.y0);
                y_max = y_max.min(interval.y1);
                if x0 > covered {
                    good = false;
                    work.issue(
                        "ROW_RAIL_GAP",
                        Some(ri as u32),
                        None,
                        Some([covered, x0]),
                        format!("{edge:?} primary rail has no rectangle coverage"),
                    )?;
                }
                covered = covered.max(x1);
            }
            if covered < end {
                good = false;
                work.issue(
                    "ROW_RAIL_GAP",
                    Some(ri as u32),
                    None,
                    Some([covered, end]),
                    format!("{edge:?} primary rail does not cover occupied row span"),
                )?;
            }
            if good && found {
                if y_min >= y_max {
                    return Err(invalid("no common positive rail band"));
                }
                row_rails.push(RailSpan {
                    row: ri as u32,
                    edge,
                    polarity: expected,
                    x_start: start,
                    x_end: end,
                    y_min,
                    y_max,
                });
            }
        }
    }
    let mut shared_boundaries_checked = 0;
    for ri in 1..rows.len() {
        work.tick(1)?;
        if rows[ri - 1]
            .y
            .checked_add(rows[ri - 1].height)
            .ok_or_else(overflow)?
            != rows[ri].y
        {
            continue;
        }
        let (Some(lower), Some(upper)) = (occupied[ri - 1], occupied[ri]) else {
            continue;
        };
        let overlap = [lower[0].max(upper[0]), lower[1].min(upper[1])];
        if overlap[0] >= overlap[1] {
            continue;
        }
        shared_boundaries_checked += 1;
        if expected_polarity(rows[ri - 1].orientation, RailEdge::Top)
            != expected_polarity(rows[ri].orientation, RailEdge::Bottom)
        {
            work.issue(
                "SHARED_BOUNDARY_POLARITY_MISMATCH",
                Some(ri as u32),
                None,
                Some(overlap),
                "adjacent occupied row edges place different supply polarities together",
            )?;
        }
    }
    if occupied_rows == 0 {
        work.issue(
            "NO_OCCUPIED_ROWS",
            None,
            None,
            None,
            "no occupied row interval was verified",
        )?;
    }
    Ok(RailReport {
        status: if work.diagnostics.is_empty() {
            "pass"
        } else {
            "fail"
        }
        .into(),
        scope: "occupied_row_primary_rectangle_continuity".into(),
        rail_layer: config.rail_layer.clone(),
        occupied_rows,
        shared_boundaries_checked,
        intervals_checked: intervals.len(),
        work_units: work.units,
        row_rails,
        diagnostics: work.diagnostics,
        body_bias_status: "unchecked".into(),
        drc_status: "unchecked".into(),
        ir_drop_status: "unchecked".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectivity::MasterPin;
    fn config() -> RailConfig {
        RailConfig {
            power_pin: "VDD".into(),
            ground_pin: "VSS".into(),
            rail_layer: "M1".into(),
        }
    }
    fn master() -> Master {
        let pin = |name: &str, use_, rect| MasterPin {
            name: name.into(),
            direction: None,
            use_,
            shape: Some("ABUTMENT".into()),
            ports: vec![vec![Geometry::Rect {
                layer: "M1".into(),
                rect,
            }]],
        };
        Master {
            name: "CELL".into(),
            class: Some("CORE".into()),
            symmetry: vec!["X".into(), "Y".into()],
            site: "site".into(),
            size: [10, 20],
            pins: vec![
                pin("VDD", PinUse::Power, [0, 18, 10, 22]),
                pin("VSS", PinUse::Ground, [0, -2, 10, 2]),
            ],
            obstructions: Vec::new(),
        }
    }
    fn rows() -> Vec<PowerRow> {
        vec![
            PowerRow {
                x_start: 0,
                x_end: 40,
                y: 0,
                height: 20,
                orientation: Orientation::N,
            },
            PowerRow {
                x_start: 0,
                x_end: 40,
                y: 20,
                height: 20,
                orientation: Orientation::FS,
            },
        ]
    }
    fn cells() -> Vec<PowerCell> {
        vec![
            PowerCell {
                instance: 0,
                master: 0,
                x: 0,
                y: 0,
                orientation: Orientation::N,
            },
            PowerCell {
                instance: 1,
                master: 0,
                x: 10,
                y: 0,
                orientation: Orientation::N,
            },
            PowerCell {
                instance: 2,
                master: 0,
                x: 0,
                y: 20,
                orientation: Orientation::FS,
            },
            PowerCell {
                instance: 3,
                master: 0,
                x: 10,
                y: 20,
                orientation: Orientation::FS,
            },
        ]
    }
    #[test]
    fn alternating_rows_have_continuous_primary_rails() {
        let report = validate_rail_continuity(
            &[master()],
            &cells(),
            &rows(),
            &config(),
            &RailLimits::default(),
        )
        .unwrap();
        assert_eq!(report.status, "pass");
        assert_eq!(report.row_rails.len(), 4);
        assert_eq!(report.shared_boundaries_checked, 1);
        assert_eq!(report.row_rails[1].polarity, RailPolarity::Power);
        assert_eq!(report.row_rails[2].polarity, RailPolarity::Power);
        assert_eq!(
            (report.row_rails[2].y_min, report.row_rails[2].y_max),
            (18, 22)
        );
        assert_eq!(report.drc_status, "unchecked");
    }
    #[test]
    fn equal_row_orientations_detect_shared_boundary_short() {
        let mut rows = rows();
        rows[1].orientation = Orientation::N;
        let mut cells = cells();
        for cell in &mut cells[2..] {
            cell.orientation = Orientation::N;
        }
        let report = validate_rail_continuity(
            &[master()],
            &cells,
            &rows,
            &config(),
            &RailLimits::default(),
        )
        .unwrap();
        assert_eq!(report.status, "fail");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "SHARED_BOUNDARY_POLARITY_MISMATCH")
        );
    }
    #[test]
    fn gap_is_reported_and_filler_closes_it() {
        let rows = vec![rows()[0]];
        let mut cells = vec![
            cells()[0],
            PowerCell {
                instance: 1,
                master: 0,
                x: 20,
                y: 0,
                orientation: Orientation::N,
            },
        ];
        let report = validate_rail_continuity(
            &[master()],
            &cells,
            &rows,
            &config(),
            &RailLimits::default(),
        )
        .unwrap();
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "ROW_RAIL_GAP" && d.x_range == Some([10, 20]))
        );
        cells.push(PowerCell {
            instance: 2,
            master: 0,
            x: 10,
            y: 0,
            orientation: Orientation::N,
        });
        assert_eq!(
            validate_rail_continuity(
                &[master()],
                &cells,
                &rows,
                &config(),
                &RailLimits::default()
            )
            .unwrap()
            .status,
            "pass"
        );
    }
    #[test]
    fn misplaced_orientation_and_missing_rail_fail() {
        let mut cells = cells();
        cells[0].orientation = Orientation::FS;
        let report = validate_rail_continuity(
            &[master()],
            &cells,
            &rows(),
            &config(),
            &RailLimits::default(),
        )
        .unwrap();
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "CELL_ROW_ORIENTATION_MISMATCH")
        );
        let mut master = master();
        master.pins.pop();
        let report = validate_rail_continuity(
            &[master],
            &cells,
            &rows(),
            &config(),
            &RailLimits::default(),
        )
        .unwrap();
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "PRIMARY_SUPPLY_PIN_MISSING")
        );
    }
    #[test]
    fn work_and_interval_limits_are_hard_errors() {
        for limits in [
            RailLimits {
                max_work: 0,
                ..Default::default()
            },
            RailLimits {
                max_intervals: 7,
                ..Default::default()
            },
            RailLimits {
                max_master_shapes: 1,
                ..Default::default()
            },
        ] {
            assert_eq!(
                validate_rail_continuity(&[master()], &cells(), &rows(), &config(), &limits)
                    .unwrap_err()
                    .code,
                "POWER_BUDGET_EXCEEDED"
            );
        }
    }
}
