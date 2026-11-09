//! Explicit fixed-macro supply collection onto the existing two supply spines.
//!
//! At most 10k pin connections / 90k vias and 2M geometry comparisons. A pin
//! uses one enclosed, original-LEF access rectangle. No path enumeration or
//! inferred SRAM implementation. Estimated derived state is below 32 MiB.
use crate::pdk::{PhysicalBundle, Technology};
use crate::{Error, Result};
use rustroad_core::connectivity::{Geometry, PinUse};
use rustroad_core::power::RailPolarity;
use rustroad_core::supply::{SupplyGrid, SupplyVia, SupplyWire, wire_bounds};
use rustroad_core::{Design, Placed, Rect, transform_rect, valid_object_name};
use rustroad_formats::lef::{LefGeometry, LefVia};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub collection_layer: String,
    pub pin_layer: String,
    pub pin_via: String,
    /// In order from the standard-cell supply-spine layer to collection_layer.
    pub via_stack: Vec<String>,
    pub wire_width: i64,
}

fn invalid(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}
fn contains(a: Rect, b: Rect) -> bool {
    a.lx <= b.lx && a.ly <= b.ly && a.ux >= b.ux && a.uy >= b.uy
}
fn translate(r: Rect, at: [i64; 2]) -> Result<Rect> {
    let add = |a: i64, b: i64| {
        a.checked_add(b)
            .ok_or_else(|| invalid("macro supply coordinate overflow"))
    };
    Ok(Rect {
        lx: add(r.lx, at[0])?,
        ly: add(r.ly, at[1])?,
        ux: add(r.ux, at[0])?,
        uy: add(r.uy, at[1])?,
    })
}
fn via<'a>(technology: &'a Technology, name: &str) -> Result<&'a LefVia> {
    technology
        .vias
        .iter()
        .find(|v| v.name == name)
        .ok_or_else(|| invalid(format!("missing macro supply via {name}")))
}
pub(crate) fn metals(technology: &Technology, via: &LefVia) -> Result<BTreeMap<String, Vec<Rect>>> {
    if via.geometries.len() > 256 {
        return Err(Error::Budget("macro supply via shape count".into()));
    }
    let mut out = BTreeMap::<String, Vec<Rect>>::new();
    for geometry in &via.geometries {
        let LefGeometry::Rect { layer, rect } = geometry else {
            return Err(invalid("macro supply requires rectangular fixed vias"));
        };
        if technology
            .layers
            .iter()
            .any(|l| l.name == *layer && l.kind.as_deref() == Some("ROUTING"))
        {
            out.entry(layer.clone()).or_default().push(Rect {
                lx: rect[0],
                ly: rect[1],
                ux: rect[2],
                uy: rect[3],
            });
        }
    }
    if out.len() != 2 {
        return Err(invalid(
            "macro supply via must connect exactly two routing layers",
        ));
    }
    Ok(out)
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.wire_width <= 0 || self.via_stack.is_empty() || self.via_stack.len() > 8 {
            return Err(invalid("invalid macro supply width/via stack"));
        }
        for name in [&self.collection_layer, &self.pin_layer, &self.pin_via]
            .into_iter()
            .chain(&self.via_stack)
        {
            if !valid_object_name(name) {
                return Err(invalid("invalid macro supply layer/via name"));
            }
        }
        Ok(())
    }
}

pub(crate) fn collect(
    design: &Design,
    bundle: &PhysicalBundle,
    placed: &[Placed],
    supply: &mut SupplyGrid,
) -> Result<()> {
    if !design.cells.iter().any(|c| c.fixed.is_some()) {
        return Ok(());
    }
    let physical = bundle
        .config
        .as_ref()
        .ok_or_else(|| invalid("macro supply needs physical configuration"))?;
    let cfg = physical
        .macro_supply
        .as_ref()
        .ok_or_else(|| invalid("fixed macros require explicit macro_supply configuration"))?;
    cfg.validate()?;
    let tech = &bundle.technology;
    let grid = tech
        .manufacturing_grid
        .ok_or_else(|| invalid("macro supply needs manufacturing grid"))?;
    if grid <= 0 || grid.checked_mul(2).is_none_or(|g| cfg.wire_width % g != 0) {
        return Err(invalid(
            "macro collection width must be an even number of manufacturing grids",
        ));
    }
    let layer = tech
        .layers
        .iter()
        .find(|l| l.name == cfg.collection_layer && l.kind.as_deref() == Some("ROUTING"))
        .ok_or_else(|| invalid("macro collection layer absent"))?;
    if cfg.wire_width
        < layer
            .width
            .ok_or_else(|| invalid("macro collection minimum width absent"))?
    {
        return Err(invalid("macro collection width below technology minimum"));
    }
    let mut current = physical.strap_layer.clone();
    for name in &cfg.via_stack {
        let m = metals(tech, via(tech, name)?)?;
        if !m.contains_key(&current) {
            return Err(invalid("macro supply via stack is not contiguous"));
        }
        current = m.keys().find(|l| **l != current).unwrap().clone();
    }
    if current != cfg.collection_layer {
        return Err(invalid("macro via stack does not end at collection layer"));
    }
    let pin_via = metals(tech, via(tech, &cfg.pin_via)?)?;
    if !pin_via.contains_key(&cfg.pin_layer) || !pin_via.contains_key(&cfg.collection_layer) {
        return Err(invalid("macro pin via connects the wrong layers"));
    }
    let mut spines = BTreeMap::new();
    for (i, w) in supply.wires.iter().enumerate() {
        if w.layer == physical.strap_layer && w.start[0] == w.end[0] {
            spines.entry(w.polarity).or_insert(i);
        }
    }
    if spines.len() != 2 {
        return Err(invalid("macro collection requires both supply spines"));
    }
    let mut wires = Vec::new();
    let mut vias = Vec::new();
    let mut work = 0usize;
    for (i, cell) in design
        .cells
        .iter()
        .enumerate()
        .filter(|(_, c)| c.fixed.is_some())
    {
        let p = placed
            .get(i)
            .ok_or_else(|| invalid("macro placement missing"))?;
        let master = &bundle.connectivity.masters[bundle.connectivity.instance_masters[i] as usize];
        for pin in master.pins.iter().filter(|p| p.use_.is_supply()) {
            if wires.len() >= 10_000 {
                return Err(Error::Budget("macro supply pin connection count".into()));
            }
            let polarity = if pin.use_ == PinUse::Power {
                RailPolarity::Power
            } else {
                RailPolarity::Ground
            };
            let mut target = None;
            for shape in pin.ports.iter().flatten() {
                work += 1;
                if work > 2_000_000 {
                    return Err(Error::Budget("macro supply geometry work".into()));
                }
                let Geometry::Rect { layer, rect } = shape else {
                    continue;
                };
                if *layer != cfg.pin_layer {
                    continue;
                }
                let rect = transform_rect(
                    Rect {
                        lx: rect[0],
                        ly: rect[1],
                        ux: rect[2],
                        uy: rect[3],
                    },
                    p.x,
                    p.y,
                    p.height,
                    cell.fixed.unwrap().orientation,
                )?;
                let mid = |a: i64, b: i64| -> Result<i64> {
                    let n = ((i128::from(a) + i128::from(b)) / 2).div_euclid(i128::from(grid))
                        * i128::from(grid);
                    i64::try_from(n).map_err(|_| invalid("macro access coordinate overflow"))
                };
                let mut at = [mid(rect.lx, rect.ux)?, mid(rect.ly, rect.uy)?];
                // Put horizontal collectors between row-edge via stacks when
                // the pin permits it. Original pin geometry remains unchanged.
                if physical.rail_via_stack.len() > 1 {
                    let pitch = i128::from(design.site.height);
                    let origin = i128::from(design.rows[0].y) + pitch / 2;
                    let row_center = origin
                        + ((i128::from(at[1]) - origin + pitch / 2).div_euclid(pitch)) * pitch;
                    let candidate = [
                        at[0],
                        i64::try_from(row_center)
                            .map_err(|_| invalid("collector alignment overflow"))?,
                    ];
                    if pin_via[&cfg.pin_layer]
                        .iter()
                        .all(|r| translate(*r, candidate).is_ok_and(|r| contains(rect, r)))
                    {
                        at = candidate;
                    }
                }
                if pin_via[&cfg.pin_layer]
                    .iter()
                    .all(|r| translate(*r, at).is_ok_and(|r| contains(rect, r)))
                {
                    target = Some(at);
                    break;
                }
            }
            let at = target.ok_or_else(|| {
                invalid(format!(
                    "no enclosed macro supply via access on {}.{}",
                    cell.name, pin.name
                ))
            })?;
            let spine = &supply.wires[spines[&polarity]];
            let trunk = [spine.start[0], at[1]];
            let wire = SupplyWire {
                polarity,
                layer: cfg.collection_layer.clone(),
                width: cfg.wire_width,
                start: trunk,
                end: at,
            };
            let bounds = wire_bounds(&wire).map_err(|e| invalid(e.to_string()))?;
            if !contains(design.die_area, bounds) {
                return Err(invalid("macro supply wire outside die"));
            }
            for r in &pin_via[&cfg.collection_layer] {
                if !contains(bounds, translate(*r, at)?) {
                    return Err(invalid(
                        "macro supply pin via lacks collection metal enclosure",
                    ));
                }
            }
            vias.push(SupplyVia {
                polarity,
                name: cfg.pin_via.clone(),
                at,
            });
            for name in &cfg.via_stack {
                let v = via(tech, name)?;
                let vm = metals(tech, v)?;
                for (metal, rectangles) in vm {
                    let enclosure = if metal == physical.strap_layer {
                        let mut extended = spine.clone();
                        extended.end[1] = extended.end[1].max(
                            at[1]
                                .checked_add(cfg.wire_width)
                                .ok_or_else(|| invalid("macro spine extension overflow"))?,
                        );
                        wire_bounds(&extended).map_err(|e| invalid(e.to_string()))?
                    } else {
                        translate(
                            Rect {
                                lx: -cfg.wire_width / 2,
                                ly: -cfg.wire_width / 2,
                                ux: cfg.wire_width / 2,
                                uy: cfg.wire_width / 2,
                            },
                            trunk,
                        )?
                    };
                    for r in rectangles {
                        if !contains(enclosure, translate(r, trunk)?) {
                            return Err(invalid("macro supply via stack lacks metal enclosure"));
                        }
                    }
                    if metal != physical.strap_layer && metal != cfg.collection_layer {
                        wires.push(SupplyWire {
                            polarity,
                            layer: metal,
                            width: cfg.wire_width,
                            // The line exporter rejects zero-length wires.
                            // A two-grid segment supplies the enclosed landing
                            // metal with an explicit positive-length geometry.
                            start: [
                                trunk[0]
                                    .checked_sub(grid)
                                    .ok_or_else(|| invalid("macro landing overflow"))?,
                                trunk[1],
                            ],
                            end: [
                                trunk[0]
                                    .checked_add(grid)
                                    .ok_or_else(|| invalid("macro landing overflow"))?,
                                trunk[1],
                            ],
                        });
                    }
                }
                vias.push(SupplyVia {
                    polarity,
                    name: name.clone(),
                    at: trunk,
                });
            }
            let index = spines[&polarity];
            supply.wires[index].end[1] = supply.wires[index].end[1].max(
                at[1]
                    .checked_add(cfg.wire_width)
                    .ok_or_else(|| invalid("macro spine extension overflow"))?,
            );
            wires.push(wire);
        }
    }
    // Deduplicate coincident straps/vias (e.g. two identical macro banks).
    let mut via_keys = BTreeSet::new();
    vias.retain(|v| via_keys.insert((v.polarity, v.name.clone(), v.at)));
    for (i, a) in wires.iter().enumerate() {
        let a_bounds = wire_bounds(a).map_err(|e| invalid(e.to_string()))?;
        for b in &wires[..i] {
            work += 1;
            if work > 2_000_000 {
                return Err(Error::Budget("macro supply spacing work".into()));
            }
            if a.layer != b.layer || a.polarity == b.polarity {
                continue;
            }
            let spacing = tech
                .layers
                .iter()
                .find(|l| l.name == a.layer)
                .and_then(|l| l.spacing)
                .unwrap_or(0);
            let b = wire_bounds(b).map_err(|e| invalid(e.to_string()))?;
            let expanded = Rect {
                lx: b
                    .lx
                    .checked_sub(spacing)
                    .ok_or_else(|| invalid("macro spacing overflow"))?,
                ly: b
                    .ly
                    .checked_sub(spacing)
                    .ok_or_else(|| invalid("macro spacing overflow"))?,
                ux: b
                    .ux
                    .checked_add(spacing)
                    .ok_or_else(|| invalid("macro spacing overflow"))?,
                uy: b
                    .uy
                    .checked_add(spacing)
                    .ok_or_else(|| invalid("macro spacing overflow"))?,
            };
            if rustroad_core::fixed::overlaps(a_bounds, expanded) {
                return Err(invalid(
                    "opposite macro supply collection metals violate spacing",
                ));
            }
        }
    }
    for w in supply.wires.iter().chain(&wires) {
        if !contains(
            design.die_area,
            wire_bounds(w).map_err(|e| invalid(e.to_string()))?,
        ) {
            return Err(invalid("macro supply metal outside die"));
        }
    }
    supply.wires.extend(wires);
    supply.vias.extend(vias);
    supply.work_units = supply.work_units.saturating_add(work);
    supply.scope = "primary_row_rails_and_explicit_fixed_macro_collection".into();
    Ok(())
}
