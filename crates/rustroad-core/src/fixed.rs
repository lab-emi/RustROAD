//! Fixed hard macros and bounded subtraction of their halos from placement rows.
use crate::{Cell, Design, Error, Orientation, Rect, Result, Row};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedPlacement {
    pub x: i64,
    pub y: i64,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub halo: i64,
}

pub fn envelope(cell: &Cell) -> Result<Option<Rect>> {
    let Some(p) = cell.fixed else { return Ok(None) };
    if p.halo < 0 {
        return Err(Error::Invalid("negative fixed macro halo".into()));
    }
    let cv = |n: i128| {
        i64::try_from(n).map_err(|_| Error::Invalid("fixed macro coordinate overflow".into()))
    };
    Ok(Some(Rect {
        lx: cv(i128::from(p.x) - i128::from(p.halo))?,
        ly: cv(i128::from(p.y) - i128::from(p.halo))?,
        ux: cv(i128::from(p.x) + i128::from(cell.width) + i128::from(p.halo))?,
        uy: cv(i128::from(p.y) + i128::from(cell.height) + i128::from(p.halo))?,
    }))
}

pub fn overlaps(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

pub fn envelopes(design: &Design) -> Result<Vec<Rect>> {
    let mut out = Vec::new();
    for cell in &design.cells {
        if let Some(rect) = envelope(cell)? {
            if out.len() >= 4096 {
                return Err(Error::Budget("fixed macro count exceeds 4096".into()));
            }
            out.push(rect);
        }
    }
    Ok(out)
}

pub fn validate(design: &Design, max_work: usize) -> Result<()> {
    let envelopes = envelopes(design)?;
    let mut work = 0usize;
    for (i, &a) in envelopes.iter().enumerate() {
        if !design.core_area.contains(a) {
            return Err(Error::Invalid(
                "fixed macro and halo must lie inside core".into(),
            ));
        }
        for &b in &envelopes[..i] {
            work += 1;
            if work > max_work {
                return Err(Error::Budget("fixed macro overlap work".into()));
            }
            if overlaps(a, b) {
                return Err(Error::Invalid("fixed macro halos overlap".into()));
            }
        }
    }
    Ok(())
}

/// Rows remain the placement grid. Returned intervals exclude every macro halo,
/// rounding cuts outwards to whole sites without changing macro coordinates.
pub fn row_segments(design: &Design, max_work: usize) -> Result<Vec<Row>> {
    let blocked = envelopes(design)?;
    let mut out = Vec::new();
    let mut work = 0usize;
    let step = i128::from(design.site.width);
    if step <= 0 {
        return Err(Error::Invalid("nonpositive placement site".into()));
    }
    for row in &design.rows {
        let mut cuts = Vec::new();
        for b in &blocked {
            work += 1;
            if work > max_work {
                return Err(Error::Budget("fixed macro row subtraction work".into()));
            }
            if i128::from(row.y) < i128::from(b.uy)
                && i128::from(b.ly) < i128::from(row.y) + i128::from(design.site.height)
            {
                let origin = i128::from(row.x_start);
                let lo = origin + (i128::from(b.lx) - origin).div_euclid(step) * step;
                let hi = origin + (i128::from(b.ux) - origin + step - 1).div_euclid(step) * step;
                let lo = lo.max(origin);
                let hi = hi.min(i128::from(row.x_end));
                if lo < hi {
                    cuts.push((lo, hi));
                }
            }
        }
        cuts.sort_unstable();
        let mut x = i128::from(row.x_start);
        for (lo, hi) in cuts.into_iter().chain(std::iter::once((
            i128::from(row.x_end),
            i128::from(row.x_end),
        ))) {
            if x < lo {
                out.push(Row {
                    x_start: x as i64,
                    x_end: lo as i64,
                    ..*row
                });
            }
            x = x.max(hi);
        }
        if out.len() > max_work {
            return Err(Error::Budget("fixed macro row interval count".into()));
        }
    }
    Ok(out)
}
