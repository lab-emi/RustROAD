//! Bounded tap reservation and row packing. No process rules are inferred here.
//! See docs/tap-budget.md. O(cells + rows + tap slots), no path/cut enumeration.
use crate::{Design, Error, Limits, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapLimits {
    pub max_taps: usize,
    pub max_work: usize,
}
impl Default for TapLimits {
    fn default() -> Self {
        Self {
            max_taps: 250_000,
            max_work: 1_000_000,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapSite {
    pub row: usize,
    pub x: i64,
    pub y: i64,
    pub width: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapReport {
    pub sites: Vec<TapSite>,
    pub rows_checked: usize,
    pub max_clear_gap: i64,
    pub requested_max_gap: i64,
    pub work_units: usize,
    pub contact_status: String,
}
pub struct TapPlan {
    pub cells: Vec<[i64; 2]>,
    pub report: TapReport,
}
fn budget(message: &str) -> Error {
    Error::Budget(message.into())
}
fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn value(v: i128) -> Result<i64> {
    i64::try_from(v).map_err(|_| invalid("tap coordinate overflow"))
}
fn tick(work: &mut usize, limit: usize) -> Result<()> {
    *work = work
        .checked_add(1)
        .ok_or_else(|| budget("tap work overflow"))?;
    if *work > limit {
        return Err(budget("tap max_work exceeded"));
    }
    Ok(())
}

/// Reserve a tap at both ends of each row and distribute bounded clear gaps
/// between them. Physical transistor/contact coverage is checked externally.
pub fn reserve(
    design: &Design,
    tap_width: i64,
    max_gap: i64,
    limits: &TapLimits,
) -> Result<TapReport> {
    design.validate(&Limits::default())?;
    let site = design.site.width;
    if tap_width <= 0 || tap_width % site != 0 || max_gap < 0 {
        return Err(invalid("tap width/gap is not compatible with site lattice"));
    }
    let gap_sites = max_gap / site;
    let width_sites = tap_width / site;
    let mut sites = Vec::new();
    let mut work = 0usize;
    let mut actual = 0i64;
    for (ri, row) in design.rows.iter().enumerate() {
        tick(&mut work, limits.max_work)?;
        let length = (i128::from(row.x_end) - i128::from(row.x_start)) / i128::from(site);
        if length < i128::from(width_sites) {
            return Err(invalid("row is narrower than tap cell"));
        }
        let count = if length == i128::from(width_sites) {
            1
        } else {
            let unit = i128::from(width_sites) + i128::from(gap_sites);
            let n = ((length + i128::from(gap_sites) + unit - 1) / unit).max(2);
            if n * i128::from(width_sites) > length {
                return Err(invalid("tap gap cannot be realized on row lattice"));
            }
            n
        };
        let count = usize::try_from(count).map_err(|_| budget("tap count overflow"))?;
        if count > limits.max_taps.saturating_sub(sites.len()) {
            return Err(budget("tap max_taps exceeded"));
        }
        let gaps = count.saturating_sub(1);
        let free = length - (count as i128) * i128::from(width_sites);
        let (base, extra) = if gaps == 0 {
            (0, 0)
        } else {
            (free / gaps as i128, free % gaps as i128)
        };
        let mut x = i128::from(row.x_start);
        for i in 0..count {
            tick(&mut work, limits.max_work)?;
            sites.push(TapSite {
                row: ri,
                x: value(x)?,
                y: row.y,
                width: tap_width,
            });
            x += i128::from(tap_width);
            if i < gaps {
                let gap = (base + i128::from((i as i128) < extra)) * i128::from(site);
                actual = actual.max(value(gap)?);
                x += gap;
            }
        }
        if x != i128::from(row.x_end) {
            return Err(invalid("tap reservation did not cover row edges"));
        }
    }
    Ok(TapReport {
        sites,
        rows_checked: design.rows.len(),
        max_clear_gap: actual,
        requested_max_gap: max_gap,
        work_units: work,
        contact_status: "requires_independent_GDS_verification".into(),
    })
}

pub fn place(design: &Design, tap_width: i64, max_gap: i64, limits: &TapLimits) -> Result<TapPlan> {
    place_with_spacing(design, tap_width, max_gap, 0, limits)
}
pub fn place_with_spacing(
    design: &Design,
    tap_width: i64,
    max_gap: i64,
    cell_gap: i64,
    limits: &TapLimits,
) -> Result<TapPlan> {
    if design.site.width <= 0 || cell_gap < 0 || cell_gap % design.site.width != 0 {
        return Err(invalid("cell spacing must follow site lattice"));
    }
    let mut report = reserve(design, tap_width, max_gap, limits)?;
    let mut intervals = Vec::with_capacity(report.sites.len());
    for pair in report.sites.windows(2) {
        if pair[0].row == pair[1].row {
            let x = pair[0]
                .x
                .checked_add(tap_width)
                .ok_or_else(|| invalid("tap extent overflow"))?;
            if x < pair[1].x {
                intervals.push((x, pair[1].x, pair[0].y));
            }
        }
    }
    let mut cells = Vec::with_capacity(design.cells.len());
    let mut interval = 0usize;
    let mut x = intervals.first().map_or(0, |p| p.0);
    for cell in &design.cells {
        loop {
            tick(&mut report.work_units, limits.max_work)?;
            let current = intervals.get(interval).ok_or_else(|| {
                Error::Capacity(format!(
                    "{}: insufficient free row intervals between taps",
                    cell.name
                ))
            })?;
            if let Some(right) = x.checked_add(cell.width).filter(|r| *r <= current.1) {
                cells.push([x, current.2]);
                x = right
                    .checked_add(cell_gap)
                    .ok_or_else(|| invalid("cell spacing overflow"))?;
                break;
            }
            interval += 1;
            x = intervals.get(interval).map_or(0, |p| p.0);
        }
    }
    Ok(TapPlan { cells, report })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Orientation, Rect, Row, Site};
    fn fixture() -> Design {
        Design {
            name: "d".into(),
            units_per_micron: 1000,
            die_area: Rect {
                lx: 0,
                ly: 0,
                ux: 200,
                uy: 80,
            },
            core_area: Rect {
                lx: 0,
                ly: 0,
                ux: 200,
                uy: 80,
            },
            site: Site {
                name: "s".into(),
                width: 10,
                height: 20,
            },
            rows: vec![
                Row {
                    x_start: 0,
                    x_end: 200,
                    y: 0,
                    orientation: Orientation::N,
                },
                Row {
                    x_start: 0,
                    x_end: 200,
                    y: 20,
                    orientation: Orientation::FS,
                },
            ],
            cells: (0..4)
                .map(|i| Cell {
                    name: format!("u{i}"),
                    master: "b".into(),
                    width: 30,
                    height: 20,
                })
                .collect(),
        }
    }
    #[test]
    fn reserves_edges_and_packs_deterministically() {
        let d = fixture();
        let a = place(&d, 10, 50, &TapLimits::default()).unwrap();
        let b = place(&d, 10, 50, &TapLimits::default()).unwrap();
        assert_eq!(a.cells, b.cells);
        assert_eq!(a.report, b.report);
        assert_eq!(a.report.sites.first().unwrap().x, 0);
        assert_eq!(a.report.sites.last().unwrap().x, 190);
        assert!(a.report.max_clear_gap <= 50);
        for [x, y] in a.cells {
            assert!(
                !a.report
                    .sites
                    .iter()
                    .any(|t| t.y == y && x < t.x + 10 && t.x < x + 30)
            );
        }
    }
    #[test]
    fn hard_caps_and_unplaceable_cells_fail() {
        let mut d = fixture();
        assert!(matches!(
            place(
                &d,
                10,
                50,
                &TapLimits {
                    max_taps: 2,
                    max_work: 1000
                }
            ),
            Err(Error::Budget(_))
        ));
        assert!(matches!(
            place(
                &d,
                10,
                50,
                &TapLimits {
                    max_taps: 100,
                    max_work: 1
                }
            ),
            Err(Error::Budget(_))
        ));
        d.cells[0].width = 100;
        d.cells[0].master = "wide".into();
        assert!(matches!(
            place(&d, 10, 50, &TapLimits::default()),
            Err(Error::Capacity(_))
        ));
    }
    #[test]
    fn lattice_and_overflow_are_checked() {
        let d = fixture();
        assert!(reserve(&d, 11, 50, &TapLimits::default()).is_err());
        assert!(reserve(&d, 10, -1, &TapLimits::default()).is_err());
        let r = reserve(&d, 10, 59, &TapLimits::default()).unwrap();
        assert!(r.max_clear_gap <= 50);
    }
}
