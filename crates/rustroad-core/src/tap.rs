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
/// Explicit experimental row pattern. It makes no claim about body coverage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TapPattern {
    /// A global lattice with nominal pitch `max_gap`, shifted on odd physical
    /// rows. Both pitch and phase are rounded down to the site lattice.
    Staggered {
        phase: i64,
        /// Optional edge densification. Where the row across the selected
        /// edge is absent, or does not face this row with the opposite
        /// orientation (so no well is shared across that edge), the stretch
        /// also receives the other row parity's lattice anchors. It then
        /// carries alone the interleaved half-pitch spacing that a shared well
        /// gets from both of its rows: the top core row, rows that border a
        /// macro or a row gap. Absent keeps the plain lattice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unpaired: Option<UnpairedEdge>,
    },
}
/// Row edge whose missing partner row triggers [`TapPattern::Staggered`]
/// densification. Edges are named in the unflipped (`N`) cell frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnpairedEdge {
    /// The edge at the top of an unflipped cell: the top edge of an `N` row
    /// and the bottom edge of an `FS` row. For a library whose power rail
    /// (and n-well) is at the cell top this is the n-well edge.
    CellTop,
    /// Both row edges.
    Both,
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
    // The caller's work budget also bounds validation of the admitted design.
    // A large placement must not be rejected by an unrelated small-fixture cap.
    design.validate(&Limits {
        max_cells: limits.max_work,
        max_rows: limits.max_work,
        max_expansions: limits.max_work,
    })?;
    let site = design.site.width;
    if tap_width <= 0 || tap_width % site != 0 || max_gap < 0 {
        return Err(invalid("tap width/gap is not compatible with site lattice"));
    }
    let gap_sites = max_gap / site;
    let width_sites = tap_width / site;
    let mut sites = Vec::new();
    let mut work = 0usize;
    let mut actual = 0i64;
    let segments = crate::fixed::row_segments(design, limits.max_work)?;
    for (ri, row) in segments.iter().enumerate() {
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
        rows_checked: segments.len(),
        max_clear_gap: actual,
        requested_max_gap: max_gap,
        work_units: work,
        contact_status: "requires_independent_GDS_verification".into(),
    })
}

/// Reserve an opt-in pattern, or preserve the original uniform algorithm when
/// absent. `max_gap` always bounds clear gaps between tap-cell bounding boxes;
/// it is not a diffusion-to-contact distance or a foundry signoff verdict.
pub fn reserve_with_pattern(
    design: &Design,
    tap_width: i64,
    max_gap: i64,
    pattern: Option<&TapPattern>,
    limits: &TapLimits,
) -> Result<TapReport> {
    let Some(TapPattern::Staggered { phase, unpaired }) = pattern else {
        return reserve(design, tap_width, max_gap, limits);
    };
    design.validate(&Limits {
        max_cells: limits.max_work,
        max_rows: limits.max_work,
        max_expansions: limits.max_work,
    })?;
    let site = design.site.width;
    if tap_width <= 0
        || tap_width % site != 0
        || max_gap <= tap_width
        || *phase < 0
        || *phase >= max_gap
    {
        return Err(invalid("invalid staggered tap width, pitch or phase"));
    }
    let pitch = i128::from(max_gap / site) * i128::from(site);
    let phase = i128::from(*phase / site) * i128::from(site);
    if pitch <= i128::from(tap_width) || phase >= pitch {
        return Err(invalid("staggered pitch/phase collapse on site lattice"));
    }
    // Both lattices share one row only when their anchors cannot overlap.
    if unpaired.is_some()
        && (phase < i128::from(tap_width) || pitch - phase < i128::from(tap_width))
    {
        return Err(invalid("unpaired densification needs phase >= tap width"));
    }
    let segments = crate::fixed::row_segments(design, limits.max_work)?;
    let mut work = 0;
    // Segments by row coordinate, in x order, for the partner-row lookup.
    let mut by_y: std::collections::BTreeMap<i64, Vec<(i64, i64, crate::Orientation)>> =
        std::collections::BTreeMap::new();
    if unpaired.is_some() {
        for row in &segments {
            tick(&mut work, limits.max_work)?;
            by_y.entry(row.y)
                .or_default()
                .push((row.x_start, row.x_end, row.orientation));
        }
        for list in by_y.values_mut() {
            list.sort_unstable_by_key(|s| (s.0, s.1));
        }
    }
    let mut sites = Vec::new();
    let mut actual = 0;
    let mut extra = Vec::new();
    for (ri, row) in segments.iter().enumerate() {
        tick(&mut work, limits.max_work)?;
        let start = i128::from(row.x_start);
        let end = i128::from(row.x_end) - i128::from(tap_width);
        if start > end {
            return Err(invalid("row is narrower than tap cell"));
        }
        if start != end && end - start < i128::from(tap_width) {
            return Err(invalid("row cannot hold both boundary taps"));
        }
        // Derive parity from physical row coordinates: a macro may split one
        // row into multiple segments and must never flip the phase mid-row.
        let row_number =
            (i128::from(row.y) - i128::from(design.core_area.ly)) / i128::from(design.site.height);
        let core = i128::from(design.core_area.lx);
        let origin = core + (row_number % 2) * phase;
        let target = start + i128::from(tap_width);
        let anchor = |origin: i128, from: i128| {
            let first = origin + (from - origin).div_euclid(pitch) * pitch;
            if first < from { first + pitch } else { first }
        };
        let first = anchor(origin, target);
        let last = end - i128::from(tap_width);
        let interior = if first <= last {
            (last - first) / pitch + 1
        } else {
            0
        };
        // For a short segment the two boundary exclusion zones can remove
        // every nominal anchor. Repair that gap explicitly; do not assume a
        // neighbouring row or a body connection covers it.
        let repair = (interior == 0 && end - start - i128::from(tap_width) > i128::from(max_gap))
            .then(|| {
                start
                    + i128::from(tap_width)
                    + ((end - start - 2 * i128::from(tap_width)) / 2 / i128::from(site))
                        * i128::from(site)
            });
        // Partner-lattice anchors on stretches whose well partner is missing,
        // widened by half a pitch so the stretch ends overlap the paired rows.
        extra.clear();
        if let Some(edge) = unpaired {
            let partner = core + ((row_number + 1) % 2) * phase;
            let above = row.orientation == crate::Orientation::N;
            let sides: &[bool] = match edge {
                UnpairedEdge::CellTop => &[above],
                UnpairedEdge::Both => &[true, false],
            };
            for &up in sides {
                let y = if up {
                    i64::try_from(i128::from(row.y) + i128::from(design.site.height))
                } else {
                    i64::try_from(i128::from(row.y) - i128::from(design.site.height))
                }
                .map_err(|_| invalid("tap row coordinate overflow"))?;
                let mut covered = start;
                let mut open = Vec::new();
                for &(lo, hi, orientation) in by_y.get(&y).map_or(&[][..], Vec::as_slice) {
                    tick(&mut work, limits.max_work)?;
                    if orientation == row.orientation {
                        continue;
                    }
                    let (lo, hi) = (i128::from(lo), i128::from(hi));
                    if hi <= covered || lo >= i128::from(row.x_end) {
                        continue;
                    }
                    if lo > covered {
                        open.push((covered, lo));
                    }
                    covered = covered.max(hi);
                }
                if covered < i128::from(row.x_end) {
                    open.push((covered, i128::from(row.x_end)));
                }
                for (a, b) in open {
                    let from = (a - pitch / 2).max(target);
                    let to = (b + pitch / 2).min(last);
                    let mut x = anchor(partner, from);
                    while x <= to {
                        tick(&mut work, limits.max_work)?;
                        extra.push(x);
                        x += pitch;
                    }
                }
            }
            extra.sort_unstable();
            extra.dedup();
        }
        let base =
            usize::try_from(interior + 1 + i128::from(start != end) + i128::from(repair.is_some()))
                .map_err(|_| budget("staggered tap count overflow"))?;
        let count = base
            .checked_add(extra.len())
            .ok_or_else(|| budget("staggered tap count overflow"))?;
        if count > limits.max_taps.saturating_sub(sites.len()) {
            return Err(budget("tap max_taps exceeded"));
        }
        if count > limits.max_work.saturating_sub(work) {
            return Err(budget("tap max_work exceeded"));
        }
        if sites.capacity() - sites.len() < count {
            let capacity = sites
                .capacity()
                .max(8)
                .saturating_mul(2)
                .max(sites.len() + count)
                .min(limits.max_taps.min(limits.max_work));
            sites
                .try_reserve_exact(capacity - sites.len())
                .map_err(|_| budget("staggered tap allocation failed"))?;
        }
        // Merge the row's own anchors with the partner anchors in x order. A
        // partner anchor that would overlap a boundary or repair tap is
        // dropped: the own lattice keeps priority.
        let mut own = std::iter::once(start)
            .chain((0..interior).map(|i| first + i * pitch))
            .chain(repair)
            .chain((start != end).then_some(end))
            .peekable();
        let mut more = extra.iter().copied().peekable();
        let mut previous: Option<i128> = None;
        loop {
            let x = match (own.peek(), more.peek()) {
                (Some(&a), Some(&b)) if b < a => {
                    more.next();
                    if previous.is_some_and(|p| b - p < i128::from(tap_width))
                        || b + i128::from(tap_width) > a
                    {
                        continue;
                    }
                    b
                }
                (Some(&a), _) => {
                    own.next();
                    a
                }
                (None, Some(&b)) => {
                    more.next();
                    if previous.is_some_and(|p| b - p < i128::from(tap_width)) {
                        continue;
                    }
                    b
                }
                (None, None) => break,
            };
            tick(&mut work, limits.max_work)?;
            if let Some(previous) = previous {
                let clear = x - previous - i128::from(tap_width);
                if clear < 0 || clear > i128::from(max_gap) {
                    return Err(invalid("staggered tap clear gap exceeds bound"));
                }
                actual = actual.max(value(clear)?);
            }
            sites.push(TapSite {
                row: ri,
                x: value(x)?,
                y: row.y,
                width: tap_width,
            });
            previous = Some(x);
        }
    }
    Ok(TapReport {
        sites,
        rows_checked: segments.len(),
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
    place_with_pattern(design, tap_width, max_gap, cell_gap, None, limits)
}

/// Pack cells between taps of the explicitly selected pattern.
pub fn place_with_pattern(
    design: &Design,
    tap_width: i64,
    max_gap: i64,
    cell_gap: i64,
    pattern: Option<&TapPattern>,
    limits: &TapLimits,
) -> Result<TapPlan> {
    if design.site.width <= 0 || cell_gap < 0 || cell_gap % design.site.width != 0 {
        return Err(invalid("cell spacing must follow site lattice"));
    }
    let mut report = reserve_with_pattern(design, tap_width, max_gap, pattern, limits)?;
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
        if let Some(fixed) = cell.fixed {
            cells.push([fixed.x, fixed.y]);
            continue;
        }
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
                    fixed: None,
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

    #[test]
    fn staggered_global_lattice_and_edges_have_explicit_positions() {
        let d = fixture();
        let pattern = TapPattern::Staggered {
            phase: 30,
            unpaired: None,
        };
        let a = reserve_with_pattern(&d, 10, 60, Some(&pattern), &TapLimits::default()).unwrap();
        let positions = |y| {
            a.sites
                .iter()
                .filter(|s| s.y == y)
                .map(|s| s.x)
                .collect::<Vec<_>>()
        };
        assert_eq!(positions(0), [0, 60, 120, 180, 190]);
        assert_eq!(positions(20), [0, 30, 90, 150, 190]);
        assert_eq!(a.max_clear_gap, 50);
        assert_eq!(a.requested_max_gap, 60);
        assert_eq!(a.contact_status, "requires_independent_GDS_verification");
        assert_eq!(
            a,
            reserve_with_pattern(&d, 10, 60, Some(&pattern), &TapLimits::default()).unwrap()
        );
        assert_eq!(
            reserve(&d, 10, 60, &TapLimits::default()).unwrap(),
            reserve_with_pattern(&d, 10, 60, None, &TapLimits::default()).unwrap()
        );
    }

    #[test]
    fn split_rows_keep_physical_phase_and_both_macro_boundary_taps() {
        let mut d = fixture();
        d.cells.push(crate::Cell {
            name: "macro".into(),
            master: "macro".into(),
            width: 40,
            height: 20,
            fixed: Some(crate::fixed::FixedPlacement {
                x: 80,
                y: 0,
                orientation: crate::Orientation::N,
                halo: 0,
            }),
        });
        let r = reserve_with_pattern(
            &d,
            10,
            60,
            Some(&TapPattern::Staggered {
                phase: 30,
                unpaired: None,
            }),
            &TapLimits::default(),
        )
        .unwrap();
        assert_eq!(r.rows_checked, 3);
        assert_eq!(
            r.sites
                .iter()
                .filter(|s| s.y == 0)
                .map(|s| s.x)
                .collect::<Vec<_>>(),
            [0, 60, 70, 120, 180, 190]
        );
        assert!(
            r.sites
                .iter()
                .all(|s| s.y != 0 || s.x + s.width <= 80 || s.x >= 120)
        );
        d.rows.remove(0);
        let r = reserve_with_pattern(
            &d,
            10,
            60,
            Some(&TapPattern::Staggered {
                phase: 30,
                unpaired: None,
            }),
            &TapLimits::default(),
        )
        .unwrap();
        assert_eq!(
            r.sites.iter().map(|s| s.x).collect::<Vec<_>>(),
            [0, 30, 90, 150, 190]
        );
    }

    #[test]
    fn staggered_adversarial_lattices_obey_cell_edge_gap_bound() {
        let mut d = fixture();
        d.cells.truncate(1);
        d.die_area.ux = 300;
        d.core_area.ux = 300;
        // Exhaust small independent lattice cases: short segments, tap overlap
        // at either boundary, nonaligned nominal values and every phase.
        for length in 1..=30 {
            for width in 1..=4 {
                for nominal in (width + 1)..=9 {
                    for phase in 0..nominal {
                        d.rows[0].x_end = length * 10;
                        d.rows[1].x_end = length * 10;
                        let r = reserve_with_pattern(
                            &d,
                            width * 10,
                            nominal * 10 + 9,
                            Some(&TapPattern::Staggered {
                                phase: phase * 10 + 1,
                                unpaired: None,
                            }),
                            &TapLimits::default(),
                        );
                        if length < width || (length > width && length < 2 * width) {
                            assert!(r.is_err());
                            continue;
                        }
                        let r = r.unwrap();
                        for row in &d.rows {
                            let slots = r.sites.iter().filter(|s| s.y == row.y).collect::<Vec<_>>();
                            assert_eq!(slots.first().unwrap().x, 0);
                            assert_eq!(slots.last().unwrap().x + width * 10, length * 10);
                            for pair in slots.windows(2) {
                                let gap = pair[1].x - pair[0].x - width * 10;
                                assert!((0..=nominal * 10 + 9).contains(&gap));
                            }
                            for slot in slots {
                                assert_eq!(slot.x % 10, 0);
                            }
                        }
                    }
                }
            }
        }
    }

    fn three_rows() -> Design {
        let mut d = fixture();
        d.core_area.uy = 60;
        d.rows.push(Row {
            x_start: 0,
            x_end: 200,
            y: 40,
            orientation: Orientation::N,
        });
        d
    }
    fn row_x(r: &TapReport, y: i64) -> Vec<i64> {
        r.sites.iter().filter(|s| s.y == y).map(|s| s.x).collect()
    }

    #[test]
    fn unpaired_edges_receive_the_partner_lattice() {
        let d = three_rows();
        let plain = TapPattern::Staggered {
            phase: 30,
            unpaired: None,
        };
        let top = TapPattern::Staggered {
            phase: 30,
            unpaired: Some(UnpairedEdge::CellTop),
        };
        let both = TapPattern::Staggered {
            phase: 30,
            unpaired: Some(UnpairedEdge::Both),
        };
        let limits = TapLimits::default();
        let p = reserve_with_pattern(&d, 10, 60, Some(&plain), &limits).unwrap();
        assert_eq!(row_x(&p, 0), [0, 60, 120, 180, 190]);
        assert_eq!(row_x(&p, 20), [0, 30, 90, 150, 190]);
        assert_eq!(row_x(&p, 40), [0, 60, 120, 180, 190]);
        // N rows carry the cell top at the row top: only the top core row's
        // n-well edge has no facing FS partner.
        let t = reserve_with_pattern(&d, 10, 60, Some(&top), &limits).unwrap();
        assert_eq!(row_x(&t, 0), row_x(&p, 0));
        assert_eq!(row_x(&t, 20), row_x(&p, 20));
        assert_eq!(row_x(&t, 40), [0, 30, 60, 90, 120, 150, 180, 190]);
        assert_eq!(t.max_clear_gap, 50);
        // Both edges: the bottom core row is unpaired as well.
        let b = reserve_with_pattern(&d, 10, 60, Some(&both), &limits).unwrap();
        assert_eq!(row_x(&b, 0), [0, 30, 60, 90, 120, 150, 180, 190]);
        assert_eq!(row_x(&b, 20), row_x(&p, 20));
        assert_eq!(row_x(&b, 40), row_x(&t, 40));
        assert_eq!(
            b,
            reserve_with_pattern(&d, 10, 60, Some(&both), &limits).unwrap()
        );
        // Rows of one orientation never share a well across an edge.
        let mut same = d.clone();
        for row in &mut same.rows {
            row.orientation = Orientation::N;
        }
        let s = reserve_with_pattern(&same, 10, 60, Some(&top), &limits).unwrap();
        assert_eq!(row_x(&s, 0), [0, 30, 60, 90, 120, 150, 180, 190]);
        assert_eq!(row_x(&s, 20), [0, 30, 60, 90, 120, 150, 180, 190]);
        // Partner anchors must not be able to overlap the own lattice.
        for phase in [0, 5] {
            let bad = TapPattern::Staggered {
                phase,
                unpaired: Some(UnpairedEdge::CellTop),
            };
            assert!(reserve_with_pattern(&d, 10, 60, Some(&bad), &limits).is_err());
            let ok = TapPattern::Staggered {
                phase,
                unpaired: None,
            };
            assert!(reserve_with_pattern(&d, 10, 60, Some(&ok), &limits).is_ok());
        }
        // Budgets count the added anchors.
        let n = t.sites.len();
        assert!(matches!(
            reserve_with_pattern(
                &d,
                10,
                60,
                Some(&top),
                &TapLimits {
                    max_taps: n - 1,
                    max_work: 1000
                }
            ),
            Err(Error::Budget(_))
        ));
    }

    #[test]
    fn macro_cut_partner_row_densifies_only_the_cut_stretch() {
        let mut d = three_rows();
        // A macro occupying the FS row over x 80..120 removes the n-well
        // partner of the N row below it there.
        d.cells.push(crate::Cell {
            name: "macro".into(),
            master: "macro".into(),
            width: 40,
            height: 20,
            fixed: Some(crate::fixed::FixedPlacement {
                x: 80,
                y: 20,
                orientation: crate::Orientation::N,
                halo: 0,
            }),
        });
        let top = TapPattern::Staggered {
            phase: 30,
            unpaired: Some(UnpairedEdge::CellTop),
        };
        let r = reserve_with_pattern(&d, 10, 60, Some(&top), &TapLimits::default()).unwrap();
        // Stretch 80..120 widened by half a pitch to 50..150: partner anchors
        // 90 and 150 join the own lattice of the N row.
        assert_eq!(row_x(&r, 0), [0, 60, 90, 120, 150, 180, 190]);
        // The FS row's segments face an N row below: paired, unchanged.
        assert_eq!(row_x(&r, 20), [0, 30, 70, 120, 150, 190]);
        // The top N row is unpaired over its full width.
        assert_eq!(row_x(&r, 40), [0, 30, 60, 90, 120, 150, 180, 190]);
        for pair in r.sites.windows(2) {
            if pair[0].row == pair[1].row {
                let gap = pair[1].x - pair[0].x - 10;
                assert!((0..=60).contains(&gap), "{pair:?}");
            }
        }
    }

    #[test]
    fn unpaired_adversarial_lattices_obey_cell_edge_gap_bound() {
        let mut d = three_rows();
        d.cells.truncate(1);
        d.die_area.ux = 300;
        d.core_area.ux = 300;
        for length in 1..=30 {
            for width in 1..=3 {
                for nominal in (width + 1)..=9 {
                    for phase in 0..nominal {
                        for row in &mut d.rows {
                            row.x_end = length * 10;
                        }
                        d.rows[1].x_start = (length / 3) * 10;
                        for edge in [UnpairedEdge::CellTop, UnpairedEdge::Both] {
                            let r = reserve_with_pattern(
                                &d,
                                width * 10,
                                nominal * 10 + 9,
                                Some(&TapPattern::Staggered {
                                    phase: phase * 10 + 1,
                                    unpaired: Some(edge),
                                }),
                                &TapLimits::default(),
                            );
                            let Ok(r) = r else { continue };
                            let mut rows = r.sites.clone();
                            rows.dedup_by_key(|s| s.row);
                            for row in rows.iter().map(|s| s.row) {
                                let slots: Vec<_> =
                                    r.sites.iter().filter(|s| s.row == row).collect();
                                for pair in slots.windows(2) {
                                    let gap = pair[1].x - pair[0].x - width * 10;
                                    assert!((0..=nominal * 10 + 9).contains(&gap));
                                }
                                for slot in slots {
                                    assert_eq!(slot.x % 10, 0);
                                }
                            }
                            // Densification only adds taps to the plain lattice.
                            let plain = reserve_with_pattern(
                                &d,
                                width * 10,
                                nominal * 10 + 9,
                                Some(&TapPattern::Staggered {
                                    phase: phase * 10 + 1,
                                    unpaired: None,
                                }),
                                &TapLimits::default(),
                            )
                            .unwrap();
                            for s in &plain.sites {
                                assert!(r.sites.contains(s));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unpaired_pattern_json_is_backward_compatible() {
        let legacy: TapPattern =
            serde_json::from_str(r#"{"kind":"staggered","phase":14490}"#).unwrap();
        assert_eq!(
            legacy,
            TapPattern::Staggered {
                phase: 14490,
                unpaired: None
            }
        );
        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            r#"{"kind":"staggered","phase":14490}"#
        );
        let text = r#"{"kind":"staggered","phase":14490,"unpaired":"cell_top"}"#;
        let edge: TapPattern = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&edge).unwrap(), text);
        assert!(
            serde_json::from_str::<TapPattern>(
                r#"{"kind":"staggered","phase":1,"unpaired":"left"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn staggered_caps_and_invalid_phase_fail_without_partial_plan() {
        let d = fixture();
        let pattern = TapPattern::Staggered {
            phase: 30,
            unpaired: None,
        };
        for limits in [
            TapLimits {
                max_taps: 1,
                max_work: 1000,
            },
            TapLimits {
                max_taps: 100,
                max_work: 1,
            },
        ] {
            assert!(matches!(
                reserve_with_pattern(&d, 10, 60, Some(&pattern), &limits),
                Err(Error::Budget(_))
            ));
        }
        for (width, pitch, phase) in [
            (11, 60, 30),
            (10, 10, 0),
            (10, 60, -1),
            (10, 60, 60),
            (10, 61, 60),
        ] {
            assert!(
                reserve_with_pattern(
                    &d,
                    width,
                    pitch,
                    Some(&TapPattern::Staggered {
                        phase,
                        unpaired: None
                    }),
                    &TapLimits::default()
                )
                .is_err()
            );
        }
        let mut d = d;
        d.site.width = 1;
        d.die_area.lx = i64::MIN;
        d.die_area.ux = i64::MAX;
        d.core_area = d.die_area;
        for row in &mut d.rows {
            row.x_start = i64::MIN;
            row.x_end = i64::MAX;
        }
        let r = reserve_with_pattern(
            &d,
            2,
            i64::MAX,
            Some(&TapPattern::Staggered {
                phase: i64::MAX / 2,
                unpaired: None,
            }),
            &TapLimits::default(),
        )
        .unwrap();
        assert!(r.sites.len() < 16);
    }
}
