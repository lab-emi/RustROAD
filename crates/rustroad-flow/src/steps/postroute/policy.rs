//! The repair policy: equal-footprint sizing families and the choice of an
//! upsize for each driver whose net is at or over its limit.
//!
//! A family is a set of at least two cells that the two Liberty corners
//! describe with the same pins, directions and output functions, that are
//! not sequential, bus, clock-gating or `dont_use` cells, and that have the
//! same LEF width and height: swapping one for another changes no placement
//! and no connectivity. Members are ordered by `max_capacitance` (the smaller
//! of the two corners), then by name.
//!
//! Repair rows are the `max_capacitance`/`max_transition` rows whose value
//! reaches `near_limit_ratio` of the limit or whose slack is negative. For a
//! transition row the strongest member is taken; for capacitance rows the
//! weakest member whose limit times `target_ratio` covers the load, else the
//! strongest if it at least covers the load. Rows without an upsize are
//! listed as residual (violating) or skipped (near the limit only).
use super::super::text::{float, is_space};
use super::super::{StepResult, fail};
use super::liberty::Library;
use super::netlist::{Instance, base};
use rustroad_pyjson::{Dict, Value, list};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

const CODE: &str = "POSTROUTE";
pub const CORNERS: [&str; 2] = ["tt", "ss"];

#[derive(Clone, Debug, PartialEq)]
pub struct FamilyCell {
    pub name: String,
    pub width_dbu: i64,
    pub area_um2: f64,
    pub max_capacitance_pf: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub id: String,
    pub pins: Vec<(String, String)>,
    pub functions: Vec<(String, String)>,
    pub cells: Vec<FamilyCell>,
}

impl Family {
    pub fn to_value(&self) -> Value {
        let pairs = |items: &[(String, String)]| {
            Value::Dict(
                items
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::Str(v.clone())))
                    .collect(),
            )
        };
        Value::Dict(
            Dict::new()
                .with("id", self.id.as_str())
                .with(
                    "order",
                    "max_capacitance_pf (min over TT/SS), equal LEF width/height",
                )
                .with("pins", pairs(&self.pins))
                .with("functions", pairs(&self.functions))
                .with("sequential", Value::List(Vec::new()))
                .with(
                    "cells",
                    list(self.cells.iter().map(|cell| {
                        Dict::new()
                            .with("name", cell.name.as_str())
                            .with("width_dbu", cell.width_dbu)
                            .with("area_um2", cell.area_um2)
                            .with("max_capacitance_pf", cell.max_capacitance_pf)
                    })),
                ),
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Families {
    pub list: Vec<Family>,
    member: HashMap<String, usize>,
}
impl Families {
    pub fn of(&self, cell: &str) -> Option<&Family> {
        self.member.get(cell).map(|at| &self.list[*at])
    }
}

type Shape = (Vec<(String, String)>, Vec<(String, String)>);
/// A sizing class: pins and functions plus the LEF footprint.
type ClassKey = (Shape, (i64, i64));

fn shape(libs: &[Library; 2], name: &str) -> Option<Shape> {
    let mut rows: Vec<Shape> = Vec::new();
    for lib in libs {
        let cell = lib.get(name)?;
        if cell.unsupported || cell.dont_use || cell.pins.is_empty() {
            return None;
        }
        let mut pins = Vec::new();
        let mut functions = Vec::new();
        for (pin, attributes) in &cell.pins.0 {
            let direction = attributes.direction.as_deref()?;
            if direction != "input" && direction != "output" {
                return None;
            }
            pins.push((pin.clone(), direction.to_owned()));
            if direction == "output" {
                let function: String = attributes
                    .function
                    .as_deref()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !is_space(*c))
                    .collect();
                if function.is_empty() {
                    return None;
                }
                functions.push((pin.clone(), function));
            }
        }
        if functions.is_empty() {
            return None;
        }
        pins.sort();
        functions.sort();
        rows.push((pins, functions));
    }
    (rows[0] == rows[1]).then(|| rows.swap_remove(0))
}

fn max_capacitance(libs: &[Library; 2], name: &str) -> StepResult<Option<f64>> {
    let mut least: Option<f64> = None;
    for lib in libs {
        let Some(cell) = lib.get(name) else { continue };
        for (_, pin) in &cell.pins.0 {
            if pin.direction.as_deref() != Some("output") {
                continue;
            }
            let Some(text) = pin.max_capacitance.as_deref() else {
                continue;
            };
            let value = float(text)
                .ok_or_else(|| fail(CODE, format!("max_capacitance is not a number: {name}")))?;
            // Python's `min`: the first of equal values, NaN never replaces.
            least = Some(match least {
                Some(known) if value.partial_cmp(&known) != Some(Ordering::Less) => known,
                _ => value,
            });
        }
    }
    Ok(least)
}

/// Equal-footprint sizing classes from both corners' Liberty (`[tt, ss]`).
///
/// # Errors
/// `POSTROUTE` if a `max_capacitance` attribute is not a number.
pub fn families(libs: &[Library; 2], sizes: &[(String, (i64, i64))]) -> StepResult<Families> {
    let size_of: HashMap<&str, (i64, i64)> = sizes.iter().map(|(n, s)| (n.as_str(), *s)).collect();
    let mut names: Vec<&str> = libs[0]
        .cells
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    names.sort_unstable();
    let mut groups: Vec<(ClassKey, Vec<&str>)> = Vec::new();
    let mut slot: HashMap<ClassKey, usize> = HashMap::new();
    for name in names {
        let (Some(shape), Some(size)) = (shape(libs, name), size_of.get(name)) else {
            continue;
        };
        let key = (shape, *size);
        match slot.get(&key) {
            Some(at) => groups[*at].1.push(name),
            None => {
                slot.insert(key.clone(), groups.len());
                groups.push((key, vec![name]));
            }
        }
    }
    groups.sort_by(|a, b| a.1[0].cmp(b.1[0]));
    let mut out = Families::default();
    for ((shape, size), names) in groups {
        if names.len() < 2 {
            continue;
        }
        let mut cells = Vec::new();
        for name in names {
            let cap = max_capacitance(libs, name)?;
            cells.push((cap, name));
        }
        cells.sort_by(|a, b| {
            let (x, y) = (a.0.unwrap_or(0.0) + 0.0, b.0.unwrap_or(0.0) + 0.0);
            x.partial_cmp(&y)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.1.cmp(b.1))
        });
        let family = Family {
            id: format!("EQW_{}", cells[0].1),
            pins: shape.0,
            functions: shape.1,
            cells: cells
                .iter()
                .map(|(cap, name)| FamilyCell {
                    name: (*name).to_owned(),
                    width_dbu: size.0,
                    area_um2: match libs[0].get(name).and_then(|cell| cell.area) {
                        Some(area) if area != 0.0 => area,
                        _ => 0.0,
                    },
                    max_capacitance_pf: *cap,
                })
                .collect(),
        };
        for cell in &family.cells {
            out.member.insert(cell.name.clone(), out.list.len());
        }
        out.list.push(family);
    }
    Ok(out)
}

/// What one screening round decided.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Resize operations (`instance`, `cell`, the rows that asked for them).
    pub ops: Vec<Dict>,
    /// Violating rows without an equal-width upsize.
    pub residual: Vec<Dict>,
    /// Near-limit rows without an equal-width upsize.
    pub skipped: Vec<Dict>,
}

/// Thresholds of `policy` in the step inputs.
#[derive(Clone, Copy, Debug)]
pub struct Thresholds {
    pub near_limit_ratio: f64,
    pub target_ratio: f64,
    pub max_resizes: usize,
}

fn number(row: &Dict, name: &str) -> StepResult<f64> {
    row.get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| fail(CODE, format!("DRV row field '{name}' is not a number")))
}

/// `round(value, 4)`: correctly rounded on the binary value, ties to even.
fn round4(value: f64) -> f64 {
    format!("{value:.4}").parse().unwrap_or(value)
}

struct Need {
    net: String,
    driver: Option<(String, String)>,
    violating: bool,
    rows: Vec<Value>,
    kinds: Vec<(bool, f64)>,
}

/// Equal-width upsizes for rows at/over `near_limit_ratio`; residual rows
/// that have none. `corners` holds each corner's DRV rows in report order;
/// `resized` the cells chosen in earlier rounds.
///
/// # Errors
/// `POSTROUTE` on a malformed DRV row or when the cumulative resizes exceed
/// `max_resizes`.
pub fn plan(
    corners: &[(&str, &[Value])],
    instances: &[(String, Instance)],
    prects: &HashSet<&str>,
    libs: &[Library; 2],
    families: &Families,
    resized: &Dict,
    thresholds: &Thresholds,
) -> StepResult<Plan> {
    let by_name: HashMap<&str, &Instance> =
        instances.iter().map(|(n, i)| (n.as_str(), i)).collect();
    // The first output pin on a net, in instance order, drives it.
    let mut driver: HashMap<&str, (&str, &str)> = HashMap::new();
    for (name, instance) in instances {
        let Some(cell) = libs[0].get(&instance.master) else {
            continue;
        };
        for (pin, net) in &instance.pins.0 {
            if cell.pins.get(pin).and_then(|p| p.direction.as_deref()) == Some("output") {
                driver.entry(net.as_str()).or_insert((name, pin));
            }
        }
    }
    let mut need: Vec<(String, Need)> = Vec::new();
    let mut slot: HashMap<String, usize> = HashMap::new();
    for (corner, rows) in corners {
        for row in *rows {
            let row = row
                .as_dict()
                .ok_or_else(|| fail(CODE, "DRV row is not an object"))?;
            let kind = row
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(CODE, "DRV row without a kind"))?;
            let limit = row
                .get("limit")
                .ok_or_else(|| fail(CODE, "DRV row without a limit"))?;
            if !matches!(kind, "max_capacitance" | "max_transition") || !limit.truthy() {
                continue;
            }
            let limit = number(row, "limit")?;
            if limit <= 0.0 {
                continue;
            }
            let (value, slack) = (number(row, "value")?, number(row, "slack")?);
            let ratio = value / limit;
            if ratio < thresholds.near_limit_ratio && slack >= 0.0 {
                continue;
            }
            let net = base(
                row.get("net")
                    .and_then(Value::as_str)
                    .ok_or_else(|| fail(CODE, "DRV row without a net"))?,
            );
            let at = driver.get(net).copied();
            let key = at.map_or_else(|| format!("net:{net}"), |(instance, _)| instance.to_owned());
            let index = *slot.entry(key.clone()).or_insert_with(|| {
                need.push((
                    key,
                    Need {
                        net: net.to_owned(),
                        driver: at.map(|(i, p)| (i.to_owned(), p.to_owned())),
                        violating: false,
                        rows: Vec::new(),
                        kinds: Vec::new(),
                    },
                ));
                need.len() - 1
            });
            let current = &mut need[index].1;
            current.violating |= slack < 0.0;
            current.kinds.push((kind == "max_capacitance", value));
            let field = |name: &str| row.get(name).cloned().unwrap_or(Value::Null);
            current.rows.push(Value::Dict(
                Dict::new()
                    .with("corner", *corner)
                    .with("kind", kind)
                    .with("pin", field("pin"))
                    .with("value", field("value"))
                    .with("limit", field("limit"))
                    .with("slack", field("slack"))
                    .with("ratio", round4(ratio)),
            ));
            if row.get("pin").is_none() {
                return Err(fail(CODE, "DRV row without a pin"));
            }
        }
    }
    need.sort_by(|a, b| a.0.cmp(&b.0));
    let mut plan = Plan::default();
    for (key, item) in need {
        let instance = (!key.starts_with("net:")).then_some(key.as_str());
        let master = instance.and_then(|name| by_name.get(name).map(|i| i.master.as_str()));
        let current = match instance.and_then(|name| resized.get(name)) {
            Some(cell) => cell.as_str(),
            None => master,
        };
        let family = current.and_then(|cell| families.of(cell));
        let mut reason = match (instance, family) {
            (None, _) => Some("net driven by a port, macro or undriven".to_owned()),
            (Some(name), _) if !prects.contains(name) => {
                Some("driver is not in the pre-CTS netlist (clock tree)".to_owned())
            }
            (_, None) => Some(format!(
                "{} has no equal-width sizing family",
                current.unwrap_or("None")
            )),
            _ => None,
        };
        let mut choice = None;
        if let (None, Some(family), Some(current)) = (&reason, family, current) {
            let now = family
                .cells
                .iter()
                .find(|cell| cell.name == current)
                .ok_or_else(|| fail(CODE, "sizing family lost its member"))?;
            let cap_need = item
                .kinds
                .iter()
                .filter(|(capacitance, _)| *capacitance)
                .map(|(_, value)| *value)
                .fold(None, |best: Option<f64>, value| match best {
                    Some(known) if value.partial_cmp(&known) != Some(Ordering::Greater) => {
                        Some(known)
                    }
                    _ => Some(value),
                })
                .unwrap_or(0.0);
            let slew = item.kinds.iter().any(|(capacitance, _)| !capacitance);
            let strength = |cell: &FamilyCell| cell.max_capacitance_pf.unwrap_or(0.0);
            let stronger: Vec<&FamilyCell> = family
                .cells
                .iter()
                .filter(|cell| strength(cell) > strength(now))
                .collect();
            choice = if slew {
                stronger.last().copied()
            } else {
                // A stronger member has a limit: it exceeds the current one's.
                stronger
                    .iter()
                    .find(|cell| strength(cell) * thresholds.target_ratio >= cap_need)
                    .copied()
                    .or_else(|| {
                        stronger
                            .last()
                            .filter(|cell| strength(cell) > cap_need)
                            .copied()
                    })
            };
            if choice.is_none() {
                reason = Some(format!(
                    "no stronger equal-width member of {} than {current}",
                    family.id
                ));
            }
        }
        let entry = Dict::new()
            .with("net", item.net.as_str())
            .with(
                "driver",
                item.driver.as_ref().map(|(i, p)| format!("{i}/{p}")),
            )
            .with("violating", item.violating)
            .with("rows", Value::List(item.rows))
            .with("instance", instance)
            .with("master", current);
        match (choice, family) {
            (Some(cell), Some(family)) => plan.ops.push(
                entry
                    .with("cell", cell.name.as_str())
                    .with("family", family.id.as_str()),
            ),
            _ if item.violating => plan.residual.push(entry.with("reason", reason)),
            _ => plan.skipped.push(entry.with("reason", reason)),
        }
    }
    if resized.len() + plan.ops.len() > thresholds.max_resizes {
        return Err(fail(CODE, "post-route resize budget"));
    }
    Ok(plan)
}
