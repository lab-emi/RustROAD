//! Deterministic, bounded boundary I/O geometry in integer database units.
//!
//! Upfront resource estimate: default N <= 100,000 pins, one corner and one
//! execution worker. Output records at <= 96 bytes/pin use < 10 MiB, cloned
//! name/layer text is capped at 32 MiB, side assignments use < 1 MiB, and
//! temporary validation references/hash tables use < 8 MiB. Peak working
//! allocation is conservatively < 64 MiB, excluding caller-owned inputs and
//! separately retained revisions. There are no paths, cuts, graphs or recursive
//! traversals. Assignment uses <= 4*N side probes; validation sorts at most N
//! references, O(N log N). Small unit fixtures consume < 64 KiB each; use the
//! repository's existing two-worker build/test and OS memory/time guards.
//!
//! This proves a conservative rectangular geometry contract, not foundry DRC:
//! square pins lie on the selected global grid, inside the die, outside the
//! core with clearance, and separated by at least the configured spacing in
//! an axis. Layer routing direction, obstructions, minimum area, antenna and
//! technology-specific end-of-line/enclosure rules need independent checking.

use crate::{Rect, valid_object_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoConstraints {
    pub die: Rect,
    pub core: Rect,
    pub layer: String,
    /// Global manufacturing grid, with integer origin (0, 0).
    pub grid: i64,
    pub min_width: i64,
    pub min_spacing: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoLimits {
    pub max_pins: usize,
    /// Maximum candidate-side probes during assignment, at most four per pin.
    pub max_expansions: usize,
    /// Aggregate cloned pin-name and layer-name bytes in the output.
    pub max_text_bytes: usize,
}
impl Default for IoLimits {
    fn default() -> Self {
        Self {
            max_pins: 100_000,
            max_expansions: 400_000,
            max_text_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoSide {
    West,
    East,
    South,
    North,
}
impl IoSide {
    fn index(self) -> usize {
        match self {
            Self::West => 0,
            Self::East => 1,
            Self::South => 2,
            Self::North => 3,
        }
    }
    fn vertical(self) -> bool {
        matches!(self, Self::West | Self::East)
    }
}
const SIDES: [IoSide; 4] = [IoSide::West, IoSide::East, IoSide::South, IoSide::North];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoPin {
    /// Exact logical bit/port name, including any literal escaped-name content.
    pub name: String,
    pub layer: String,
    pub side: IoSide,
    /// Absolute die coordinates. The caller decides the DEF local origin.
    pub rect: Rect,
}

#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("IO_PLACEMENT_INVALID: {0}")]
    Invalid(String),
    #[error("IO_PLACEMENT_BUDGET: {0}")]
    Budget(String),
    #[error(
        "IO_PLACEMENT_CAPACITY: requested {requested} pins, conservative capacity is {capacity}"
    )]
    Capacity { requested: usize, capacity: u128 },
}
pub type IoResult<T> = std::result::Result<T, IoError>;

fn invalid(message: impl Into<String>) -> IoError {
    IoError::Invalid(message.into())
}
fn budget(message: &str) -> IoError {
    IoError::Budget(message.into())
}
fn rect_valid(rect: Rect) -> bool {
    rect.lx < rect.ux && rect.ly < rect.uy
}
fn contains(outer: Rect, inner: Rect) -> bool {
    outer.lx <= inner.lx && outer.ly <= inner.ly && outer.ux >= inner.ux && outer.uy >= inner.uy
}
fn coord(value: i128) -> IoResult<i64> {
    i64::try_from(value).map_err(|_| invalid("coordinate/dimension exceeds i64 DBU"))
}
fn round_up(value: i64, grid: i64) -> IoResult<i64> {
    // Positive values and grid are checked before this helper is called.
    let value = i128::from(value);
    let grid = i128::from(grid);
    coord(((value + grid - 1) / grid) * grid)
}

impl IoConstraints {
    /// Square width and separation used by this conservative placement policy.
    /// Spacing is at least one grid step so distinct terminals never touch,
    /// including when the declared minimum spacing is zero.
    pub fn effective_dimensions(&self) -> IoResult<(i64, i64)> {
        if self.grid <= 0 || self.min_width <= 0 || self.min_spacing < 0 {
            return Err(invalid(
                "grid/width must be positive and spacing nonnegative",
            ));
        }
        Ok((
            round_up(self.min_width, self.grid)?,
            round_up(self.min_spacing.max(self.grid), self.grid)?,
        ))
    }
    fn validate(&self) -> IoResult<(i64, i64)> {
        let dimensions = self.effective_dimensions()?;
        if !valid_object_name(&self.layer) {
            return Err(invalid("invalid I/O layer name"));
        }
        if !rect_valid(self.die) || !rect_valid(self.core) || !contains(self.die, self.core) {
            return Err(invalid("nonempty core must lie inside a nonempty die"));
        }
        if [self.die.lx, self.die.ly, self.die.ux, self.die.uy]
            .iter()
            .any(|value| value % self.grid != 0)
        {
            return Err(invalid("die boundary must lie on the global I/O grid"));
        }
        Ok(dimensions)
    }
}

#[derive(Clone, Copy)]
struct Lane {
    /// Allowed lower coordinate along the side, excluding both corners.
    start: i128,
    end: i128,
    capacity: usize,
}

fn lanes(config: &IoConstraints, width: i64, gap: i64, max_pins: usize) -> [Lane; 4] {
    let d = config.die;
    let c = config.core;
    let w = i128::from(width);
    let s = i128::from(gap);
    let guard = w + s;
    let eligible = [
        i128::from(d.lx) + w + s <= i128::from(c.lx),
        i128::from(d.ux) - w - s >= i128::from(c.ux),
        i128::from(d.ly) + w + s <= i128::from(c.ly),
        i128::from(d.uy) - w - s >= i128::from(c.uy),
    ];
    std::array::from_fn(|index| {
        let side = SIDES[index];
        let (lower, upper) = if side.vertical() {
            (d.ly, d.uy)
        } else {
            (d.lx, d.ux)
        };
        let start = i128::from(lower) + guard;
        let end = i128::from(upper) - guard - w;
        let capacity = if eligible[index] && end >= start {
            // Cap stored capacities to the maximum accepted request before
            // converting to usize; do not allocate a vector of all slots.
            (((end - start) / (w + s) + 1) as u128).min(max_pins as u128) as usize
        } else {
            0
        };
        Lane {
            start,
            end,
            capacity,
        }
    })
}

fn check_text<'a>(
    names: impl Iterator<Item = &'a str>,
    layer: &str,
    limits: &IoLimits,
) -> IoResult<()> {
    let mut seen = HashSet::new();
    let mut bytes = 0usize;
    for (count, name) in names.enumerate() {
        if count >= limits.max_pins {
            return Err(budget("max_pins exceeded"));
        }
        if !valid_object_name(name) || !seen.insert(name) {
            return Err(invalid("invalid or duplicate logical I/O name"));
        }
        bytes = bytes
            .checked_add(name.len())
            .and_then(|n| n.checked_add(layer.len()))
            .ok_or_else(|| budget("I/O text size overflow"))?;
        if bytes > limits.max_text_bytes {
            return Err(budget("max_text_bytes exceeded"));
        }
    }
    Ok(())
}

fn at(config: &IoConstraints, side: IoSide, lower: i128, width: i64) -> IoResult<Rect> {
    let d = config.die;
    let w = i128::from(width);
    let (lx, ly) = match side {
        IoSide::West => (i128::from(d.lx), lower),
        IoSide::East => (i128::from(d.ux) - w, lower),
        IoSide::South => (lower, i128::from(d.ly)),
        IoSide::North => (lower, i128::from(d.uy) - w),
    };
    Ok(Rect {
        lx: coord(lx)?,
        ly: coord(ly)?,
        ux: coord(lx + w)?,
        uy: coord(ly + w)?,
    })
}

/// Place ordered logical terminal names. Output order is identical to input.
/// Counts differ by at most one among unsaturated eligible sides. Saturated
/// sides are skipped; no pin is silently omitted when capacity is exhausted.
pub fn place_boundary_io(
    names: &[String],
    config: &IoConstraints,
    limits: &IoLimits,
) -> IoResult<Vec<IoPin>> {
    let (width, gap) = config.validate()?;
    if names.len() > limits.max_pins {
        return Err(budget("max_pins exceeded"));
    }
    check_text(names.iter().map(String::as_str), &config.layer, limits)?;
    let lanes = lanes(config, width, gap, limits.max_pins);
    let capacity = lanes.iter().map(|lane| lane.capacity as u128).sum::<u128>();
    if names.len() as u128 > capacity {
        return Err(IoError::Capacity {
            requested: names.len(),
            capacity,
        });
    }
    let mut sides = Vec::with_capacity(names.len());
    let mut counts = [0usize; 4];
    let mut next = 0usize;
    let mut expansions = 0usize;
    for _ in names {
        let mut chosen = None;
        for _ in 0..4 {
            if expansions >= limits.max_expansions {
                return Err(budget("max_expansions exceeded"));
            }
            expansions += 1;
            let index = next;
            next = (next + 1) % 4;
            if counts[index] < lanes[index].capacity {
                chosen = Some(index);
                break;
            }
        }
        let index = chosen.ok_or_else(|| invalid("I/O capacity accounting is inconsistent"))?;
        counts[index] += 1;
        sides.push(SIDES[index]);
    }
    let mut used = [0usize; 4];
    let mut pins = Vec::with_capacity(names.len());
    for (name, side) in names.iter().zip(sides) {
        let index = side.index();
        let lane = lanes[index];
        let steps = (lane.end - lane.start) / i128::from(config.grid);
        let offset_steps = if counts[index] == 1 {
            steps / 2
        } else {
            steps
                .checked_mul(used[index] as i128)
                .ok_or_else(|| budget("slot interpolation overflow"))?
                / (counts[index] - 1) as i128
        };
        let lower = lane.start + offset_steps * i128::from(config.grid);
        pins.push(IoPin {
            name: name.clone(),
            layer: config.layer.clone(),
            side,
            rect: at(config, side, lower, width)?,
        });
        used[index] += 1;
    }
    // Check geometric invariants independently of the slot calculation.
    validate_boundary_io(&pins, config, limits)?;
    Ok(pins)
}

/// Explicit I/O assignment: ordered pin names per side (increasing coordinate
/// along the side) and optional along-edge track snapping `[pitch, offset]`
/// of pin centres (x on south/north, y on west/east).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IoPlan {
    pub west: Vec<String>,
    pub east: Vec<String>,
    pub south: Vec<String>,
    pub north: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_track: Option<[i64; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y_track: Option<[i64; 2]>,
}

/// Place pins by an explicit plan: evenly spread along each side in plan
/// order, centres snapped to the side's track grid, spacing preserved.
/// Output order is identical to `names`; every name must appear exactly once.
pub fn place_planned_io(
    names: &[String],
    plan: &IoPlan,
    config: &IoConstraints,
    limits: &IoLimits,
) -> IoResult<Vec<IoPin>> {
    let (width, gap) = config.validate()?;
    if names.len() > limits.max_pins {
        return Err(budget("max_pins exceeded"));
    }
    check_text(names.iter().map(String::as_str), &config.layer, limits)?;
    let lists = [&plan.west, &plan.east, &plan.south, &plan.north];
    let mut seen = HashSet::new();
    for list in lists {
        for n in list.iter() {
            if !seen.insert(n.as_str()) {
                return Err(invalid("I/O plan names a pin twice"));
            }
        }
    }
    if seen.len() != names.len() || names.iter().any(|n| !seen.contains(n.as_str())) {
        return Err(invalid(
            "I/O plan does not cover exactly the logical I/O pins",
        ));
    }
    for t in [plan.x_track, plan.y_track].into_iter().flatten() {
        if t[0] <= 0 || t[0] % config.grid != 0 || t[1] % config.grid != 0 {
            return Err(invalid("I/O track must be positive and on the grid"));
        }
    }
    let slots = planned_slots(config, plan, width, gap, limits)?;
    let mut placed = std::collections::HashMap::new();
    for (index, list) in lists.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let (first, step, count) = slots[index];
        if list.len() > count {
            return Err(IoError::Capacity {
                requested: list.len(),
                capacity: count as u128,
            });
        }
        let side = SIDES[index];
        for (k, name) in list.iter().enumerate() {
            let slot = if list.len() == 1 {
                (count - 1) / 2
            } else {
                k * (count - 1) / (list.len() - 1)
            };
            let lower = first + step * slot as i128;
            placed.insert(
                name.as_str(),
                IoPin {
                    name: name.clone(),
                    layer: config.layer.clone(),
                    side,
                    rect: at(config, side, lower, width)?,
                },
            );
        }
    }
    let pins: Vec<IoPin> = names.iter().map(|n| placed[n.as_str()].clone()).collect();
    validate_boundary_io(&pins, config, limits)?;
    Ok(pins)
}

/// Slot lattice of each side under a plan: (first lower edge, step, count).
/// With a track, pin centres sit on track lines and consecutive slots are
/// the fewest whole tracks apart that keep width plus spacing.
fn planned_slots(
    config: &IoConstraints,
    plan: &IoPlan,
    width: i64,
    gap: i64,
    limits: &IoLimits,
) -> IoResult<[(i128, i128, usize); 4]> {
    let lanes = lanes(config, width, gap, limits.max_pins);
    let g = i128::from(config.grid);
    let half = i128::from(width / 2 / config.grid * config.grid);
    let pitch = i128::from(width) + i128::from(gap);
    let mut out = [(0i128, 1i128, 0usize); 4];
    for (index, lane) in lanes.iter().enumerate() {
        if lane.capacity == 0 {
            continue;
        }
        let track = if SIDES[index].vertical() {
            plan.y_track
        } else {
            plan.x_track
        };
        let (first, step) = match track {
            Some([p, o]) => {
                let (p, o) = (i128::from(p), i128::from(o));
                let k = (lane.start + half - o + p - 1).div_euclid(p);
                let mut first = o + k * p - half;
                first = (first + g - 1).div_euclid(g) * g;
                let step = (pitch + p - 1).div_euclid(p) * p;
                (first, step)
            }
            None => (lane.start, pitch),
        };
        let count = if first > lane.end {
            0
        } else {
            (((lane.end - first) / step) + 1).min(limits.max_pins as i128) as usize
        };
        out[index] = (first, step, count);
    }
    Ok(out)
}

/// Pins each side can hold under the plan's track snapping (W, E, S, N).
pub fn planned_capacity(
    config: &IoConstraints,
    plan: &IoPlan,
    limits: &IoLimits,
) -> IoResult<[usize; 4]> {
    let (width, gap) = config.validate()?;
    let s = planned_slots(config, plan, width, gap, limits)?;
    Ok([s[0].2, s[1].2, s[2].2, s[3].2])
}

/// Conservative axis-separation test, stronger than Euclidean spacing for
/// diagonally adjacent rectangles. i128 prevents overflow near i64 endpoints.
fn separated(a: Rect, b: Rect, gap: i64) -> bool {
    let gap = i128::from(gap);
    i128::from(a.ux) + gap <= i128::from(b.lx)
        || i128::from(b.ux) + gap <= i128::from(a.lx)
        || i128::from(a.uy) + gap <= i128::from(b.ly)
        || i128::from(b.uy) + gap <= i128::from(a.ly)
}

/// Validate geometry without requiring it to follow this placer's particular
/// slot positions. Same-side intervals are swept in sorted order. For adjacent
/// orthogonal sides, only the two terminals closest to their shared corner can
/// have the smallest separation; check those four pairs, not every pin pair.
pub fn validate_boundary_io(
    pins: &[IoPin],
    config: &IoConstraints,
    limits: &IoLimits,
) -> IoResult<()> {
    let (width, gap) = config.validate()?;
    if pins.len() > limits.max_pins {
        return Err(budget("max_pins exceeded"));
    }
    check_text(
        pins.iter().map(|pin| pin.name.as_str()),
        &config.layer,
        limits,
    )?;
    let mut groups: [Vec<&IoPin>; 4] = std::array::from_fn(|_| Vec::new());
    for pin in pins {
        if pin.layer != config.layer {
            return Err(invalid("I/O pin layer does not match constraints"));
        }
        if !rect_valid(pin.rect)
            || !contains(config.die, pin.rect)
            || i128::from(pin.rect.ux) - i128::from(pin.rect.lx) != i128::from(width)
            || i128::from(pin.rect.uy) - i128::from(pin.rect.ly) != i128::from(width)
        {
            return Err(invalid(format!(
                "{}: pin must be the configured square inside the die",
                pin.name
            )));
        }
        if [pin.rect.lx, pin.rect.ly, pin.rect.ux, pin.rect.uy]
            .iter()
            .any(|v| v % config.grid != 0)
        {
            return Err(invalid(format!("{}: pin is off the global grid", pin.name)));
        }
        let d = config.die;
        let c = config.core;
        let boundary_and_core = match pin.side {
            IoSide::West => {
                pin.rect.lx == d.lx && i128::from(pin.rect.ux) + i128::from(gap) <= i128::from(c.lx)
            }
            IoSide::East => {
                pin.rect.ux == d.ux && i128::from(pin.rect.lx) - i128::from(gap) >= i128::from(c.ux)
            }
            IoSide::South => {
                pin.rect.ly == d.ly && i128::from(pin.rect.uy) + i128::from(gap) <= i128::from(c.ly)
            }
            IoSide::North => {
                pin.rect.uy == d.uy && i128::from(pin.rect.ly) - i128::from(gap) >= i128::from(c.uy)
            }
        };
        if !boundary_and_core || !separated(pin.rect, config.core, gap) {
            return Err(invalid(format!(
                "{}: pin is not on its boundary or lacks core clearance",
                pin.name
            )));
        }
        groups[pin.side.index()].push(pin);
    }
    for (index, group) in groups.iter_mut().enumerate() {
        group.sort_unstable_by_key(|pin| {
            if SIDES[index].vertical() {
                pin.rect.ly
            } else {
                pin.rect.lx
            }
        });
        for adjacent in group.windows(2) {
            if !separated(adjacent[0].rect, adjacent[1].rect, gap) {
                return Err(invalid("same-side I/O pins overlap or violate spacing"));
            }
        }
    }
    for (a, b) in [
        (groups[0].first(), groups[2].first()),
        (groups[0].last(), groups[3].first()),
        (groups[1].first(), groups[2].last()),
        (groups[1].last(), groups[3].last()),
    ] {
        if let (Some(a), Some(b)) = (a, b)
            && !separated(a.rect, b.rect, gap)
        {
            return Err(invalid("I/O pins violate spacing at a die corner"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn constraints() -> IoConstraints {
        IoConstraints {
            die: Rect {
                lx: 0,
                ly: 0,
                ux: 200,
                uy: 200,
            },
            core: Rect {
                lx: 40,
                ly: 40,
                ux: 160,
                uy: 160,
            },
            layer: "met3".into(),
            grid: 2,
            min_width: 3,
            min_spacing: 3,
        }
    }
    fn names(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("port[{i}]")).collect()
    }
    #[test]
    fn planned_pins_follow_order_side_and_track() {
        let c = constraints();
        let ports = names(6);
        let plan = IoPlan {
            west: vec!["port[2]".into(), "port[0]".into()],
            east: vec!["port[1]".into()],
            south: vec!["port[3]".into(), "port[4]".into()],
            north: vec!["port[5]".into()],
            x_track: Some([10, 4]),
            y_track: Some([8, 0]),
        };
        let pins = place_planned_io(&ports, &plan, &c, &IoLimits::default()).unwrap();
        assert_eq!(
            pins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ports
        );
        let side = |n: &str| pins.iter().find(|p| p.name == n).unwrap();
        assert_eq!(side("port[2]").side, IoSide::West);
        assert!(side("port[2]").rect.ly < side("port[0]").rect.ly);
        // Centres sit on the side's track (within one grid step).
        for p in &pins {
            let (centre, [pitch, offset]) = match p.side {
                IoSide::West | IoSide::East => ((p.rect.ly + p.rect.uy) / 2, [8, 0]),
                _ => ((p.rect.lx + p.rect.ux) / 2, [10, 4]),
            };
            let d = (centre - offset).rem_euclid(pitch);
            assert!(d <= 2 || pitch - d <= 2, "{p:?}");
        }
        assert_eq!(
            pins,
            place_planned_io(&ports, &plan, &c, &IoLimits::default()).unwrap()
        );
        // Every pin must be planned exactly once.
        let mut missing = plan.clone();
        missing.north.clear();
        assert!(place_planned_io(&ports, &missing, &c, &IoLimits::default()).is_err());
        let mut twice = plan.clone();
        twice.north.push("port[0]".into());
        assert!(place_planned_io(&ports, &twice, &c, &IoLimits::default()).is_err());
        let cap = planned_capacity(&c, &plan, &IoLimits::default()).unwrap();
        assert!(cap.iter().all(|n| *n >= 2));
    }
    #[test]
    fn balanced_deterministic_and_grid_rounded() {
        let c = constraints();
        let ports = names(16);
        let a = place_boundary_io(&ports, &c, &IoLimits::default()).unwrap();
        assert_eq!(
            a,
            place_boundary_io(&ports, &c, &IoLimits::default()).unwrap()
        );
        assert_eq!(c.effective_dimensions().unwrap(), (4, 4));
        assert_eq!(
            a.iter().map(|p| &p.name).collect::<Vec<_>>(),
            ports.iter().collect::<Vec<_>>()
        );
        for side in SIDES {
            assert_eq!(a.iter().filter(|p| p.side == side).count(), 4);
        }
        for p in &a {
            assert_eq!((p.rect.ux - p.rect.lx, p.rect.uy - p.rect.ly), (4, 4));
            assert!(
                [p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy]
                    .iter()
                    .all(|v| v % 2 == 0)
            );
        }
        validate_boundary_io(&a, &c, &IoLimits::default()).unwrap();
    }
    #[test]
    fn saturated_or_unavailable_sides_are_skipped() {
        let mut c = constraints();
        c.core.lx = 0;
        let a = place_boundary_io(&names(12), &c, &IoLimits::default()).unwrap();
        assert!(a.iter().all(|p| p.side != IoSide::West));
        for side in [IoSide::East, IoSide::South, IoSide::North] {
            assert_eq!(a.iter().filter(|p| p.side == side).count(), 4);
        }
        c.core = c.die;
        assert!(matches!(
            place_boundary_io(&names(1), &c, &IoLimits::default()),
            Err(IoError::Capacity { .. })
        ));
        assert!(
            place_boundary_io(&[], &c, &IoLimits::default())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn exact_small_capacity_and_insufficient_budget_fail() {
        let c = IoConstraints {
            die: Rect {
                lx: 0,
                ly: 0,
                ux: 24,
                uy: 24,
            },
            core: Rect {
                lx: 8,
                ly: 8,
                ux: 16,
                uy: 16,
            },
            min_width: 4,
            min_spacing: 2,
            ..constraints()
        };
        assert_eq!(
            place_boundary_io(&names(8), &c, &IoLimits::default())
                .unwrap()
                .len(),
            8
        );
        assert!(matches!(
            place_boundary_io(&names(9), &c, &IoLimits::default()),
            Err(IoError::Capacity {
                requested: 9,
                capacity: 8
            })
        ));
        let limited = IoLimits {
            max_expansions: 3,
            ..IoLimits::default()
        };
        assert!(matches!(
            place_boundary_io(&names(4), &c, &limited),
            Err(IoError::Budget(_))
        ));
        let limited = IoLimits {
            max_text_bytes: 1,
            ..IoLimits::default()
        };
        assert!(matches!(
            place_boundary_io(&names(1), &c, &limited),
            Err(IoError::Budget(_))
        ));
    }
    #[test]
    fn validator_detects_same_side_corner_and_grid_defects() {
        let c = constraints();
        let mut p = place_boundary_io(&names(8), &c, &IoLimits::default()).unwrap();
        p[4].rect = p[0].rect;
        assert!(validate_boundary_io(&p, &c, &IoLimits::default()).is_err());
        let corners = vec![
            IoPin {
                name: "w".into(),
                layer: c.layer.clone(),
                side: IoSide::West,
                rect: Rect {
                    lx: 0,
                    ly: 0,
                    ux: 4,
                    uy: 4,
                },
            },
            IoPin {
                name: "s".into(),
                layer: c.layer.clone(),
                side: IoSide::South,
                rect: Rect {
                    lx: 0,
                    ly: 0,
                    ux: 4,
                    uy: 4,
                },
            },
        ];
        assert!(validate_boundary_io(&corners, &c, &IoLimits::default()).is_err());
        let mut p = place_boundary_io(&names(1), &c, &IoLimits::default()).unwrap();
        p[0].rect.ly += 1;
        p[0].rect.uy += 1;
        assert!(validate_boundary_io(&p, &c, &IoLimits::default()).is_err());
    }
    #[test]
    fn negatives_extremes_and_invalid_constraints_are_checked() {
        let mut c = constraints();
        c.die = Rect {
            lx: -1_000_000_000_000,
            ly: -1_000_000_000_000,
            ux: -999_999_999_800,
            uy: -999_999_999_800,
        };
        c.core = Rect {
            lx: -999_999_999_960,
            ly: -999_999_999_960,
            ux: -999_999_999_840,
            uy: -999_999_999_840,
        };
        let p = place_boundary_io(&names(8), &c, &IoLimits::default()).unwrap();
        assert!(p.iter().all(|p| p.rect.ux < 0 && p.rect.uy < 0));
        c.grid = 0;
        assert!(place_boundary_io(&names(1), &c, &IoLimits::default()).is_err());
        c = constraints();
        c.min_width = i64::MAX;
        assert!(matches!(
            place_boundary_io(&names(1), &c, &IoLimits::default()),
            Err(IoError::Invalid(_))
        ));
        c = constraints();
        c.die.ux -= 1;
        assert!(place_boundary_io(&names(1), &c, &IoLimits::default()).is_err());
        let mut duplicate = names(2);
        duplicate[1] = duplicate[0].clone();
        assert!(place_boundary_io(&duplicate, &constraints(), &IoLimits::default()).is_err());
    }
}
