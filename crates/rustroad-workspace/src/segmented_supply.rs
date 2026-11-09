//! Power rails split at macro halos, fed by explicit fixed-via stacks.
//!
//! Derived records and all scans use the existing SupplyLimits. No paths are
//! enumerated. Full-width rows connect the channel spines to the primary grid;
//! a design without those rows fails instead of publishing floating supplies.
use crate::pdk::PhysicalBundle;
use crate::{Error, Result};
use rustroad_core::power::{RailPolarity, RailReport, RailSpan};
use rustroad_core::supply::{
    LayerSpec, SupplyGrid, SupplyLimits, SupplyVia, SupplyWire, ViaSpec, build_supply_grid,
    wire_bounds,
};
use rustroad_core::{Design, Rect};
use std::collections::{BTreeMap, BTreeSet};

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
fn integer(n: i128) -> Result<i64> {
    i64::try_from(n).map_err(|_| invalid("segmented supply coordinate overflow"))
}
fn ceil(n: i128, grid: i64) -> Result<i64> {
    integer(
        n.div_euclid(i128::from(grid)) * i128::from(grid)
            + if n.rem_euclid(i128::from(grid)) == 0 {
                0
            } else {
                i128::from(grid)
            },
    )
}
fn shift(r: Rect, p: [i64; 2]) -> Result<Rect> {
    Ok(Rect {
        lx: integer(i128::from(r.lx) + i128::from(p[0]))?,
        ly: integer(i128::from(r.ly) + i128::from(p[1]))?,
        ux: integer(i128::from(r.ux) + i128::from(p[0]))?,
        uy: integer(i128::from(r.uy) + i128::from(p[1]))?,
    })
}
fn within(a: Rect, b: Rect) -> bool {
    a.lx <= b.lx && a.ly <= b.ly && a.ux >= b.ux && a.uy >= b.uy
}
struct Stack {
    names: Vec<String>,
    pads: Vec<BTreeMap<String, Vec<Rect>>>,
    landings: Vec<(String, i64)>,
    abstract_via: ViaSpec,
}
impl Stack {
    fn new(bundle: &PhysicalBundle, grid: i64) -> Result<Self> {
        let cfg = bundle
            .config
            .as_ref()
            .ok_or_else(|| invalid("missing power configuration"))?;
        let names = if cfg.rail_via_stack.is_empty() {
            vec![cfg.via.clone()]
        } else {
            cfg.rail_via_stack.clone()
        };
        if names.len() > 8 {
            return Err(Error::Budget("supply via stack length".into()));
        }
        let mut pads = Vec::new();
        let mut current = cfg.rail_layer.clone();
        let mut merged = BTreeMap::<String, Vec<Rect>>::new();
        for name in &names {
            let v = bundle
                .technology
                .vias
                .iter()
                .find(|v| &v.name == name)
                .ok_or_else(|| invalid(format!("missing supply via {name}")))?;
            let metals = crate::macro_supply::metals(&bundle.technology, v)?;
            if !metals.contains_key(&current) {
                return Err(invalid("non-contiguous supply via stack"));
            }
            current = metals
                .keys()
                .find(|s| **s != current)
                .ok_or_else(|| invalid("supply via layer pair"))?
                .clone();
            for (name, shapes) in &metals {
                merged.entry(name.clone()).or_default().extend(shapes);
            }
            pads.push(metals);
        }
        if current != cfg.strap_layer {
            return Err(invalid("supply via stack does not reach strap layer"));
        }
        let lower = pads
            .first()
            .and_then(|p| p.get(&cfg.rail_layer))
            .cloned()
            .ok_or_else(|| invalid("supply lower pad absent"))?;
        let upper = pads
            .last()
            .and_then(|p| p.get(&cfg.strap_layer))
            .cloned()
            .ok_or_else(|| invalid("supply upper pad absent"))?;
        let mut landings = Vec::new();
        for (name, shapes) in merged {
            if name == cfg.rail_layer || name == cfg.strap_layer {
                continue;
            }
            let mut half = shapes
                .iter()
                .flat_map(|r| [r.lx, r.ly, r.ux, r.uy])
                .map(|v| i128::from(v).abs())
                .max()
                .unwrap_or(0);
            let rule = bundle
                .technology
                .layers
                .iter()
                .find(|l| l.name == name)
                .ok_or_else(|| invalid("missing intermediate supply layer"))?;
            half = half.max(i128::from(rule.width.unwrap_or(0)).div_euclid(2));
            if let Some(area) = rule.min_area {
                let area =
                    u128::try_from(area).map_err(|_| invalid("negative supply landing area"))?;
                let side = area.isqrt();
                let side = side + u128::from(side * side < area);
                half = half.max(
                    i128::try_from(side.div_ceil(2))
                        .map_err(|_| invalid("landing area overflow"))?,
                );
            }
            landings.push((name, integer(2 * i128::from(ceil(half, grid)?))?));
        }
        Ok(Self {
            abstract_via: ViaSpec {
                name: names[0].clone(),
                lower,
                upper,
            },
            names,
            pads,
            landings,
        })
    }
}
struct Builder<'a> {
    out: SupplyGrid,
    die: Rect,
    grid: i64,
    work: usize,
    limits: &'a SupplyLimits,
}
impl Builder<'_> {
    fn tick(&mut self) -> Result<()> {
        self.work = self
            .work
            .checked_add(1)
            .ok_or_else(|| Error::Budget("supply work overflow".into()))?;
        if self.work > self.limits.max_work {
            return Err(Error::Budget("segmented supply work limit".into()));
        }
        Ok(())
    }
    fn wire(&mut self, w: SupplyWire) -> Result<()> {
        self.tick()?;
        if self.out.wires.len() >= self.limits.max_wires {
            return Err(Error::Budget("supply wire count".into()));
        }
        let r = wire_bounds(&w).map_err(|e| invalid(e.to_string()))?;
        if !within(self.die, r) {
            return Err(invalid("segmented supply wire outside die"));
        }
        self.out.wires.push(w);
        Ok(())
    }
    fn stack(&mut self, stack: &Stack, polarity: RailPolarity, at: [i64; 2]) -> Result<()> {
        for (name, pads) in stack.names.iter().zip(&stack.pads) {
            self.tick()?;
            if self.out.vias.len() >= self.limits.max_vias {
                return Err(Error::Budget("supply via count".into()));
            }
            for rect in pads.values().flatten() {
                if !within(self.die, shift(*rect, at)?) {
                    return Err(invalid("supply via outside die"));
                }
            }
            self.out.vias.push(SupplyVia {
                polarity,
                name: name.clone(),
                at,
            });
        }
        for (layer, width) in &stack.landings {
            self.wire(SupplyWire {
                polarity,
                layer: layer.clone(),
                width: *width,
                start: [at[0], integer(i128::from(at[1]) - i128::from(self.grid))?],
                end: [at[0], integer(i128::from(at[1]) + i128::from(self.grid))?],
            })?;
        }
        Ok(())
    }
}

pub(crate) fn build(
    design: &Design,
    bundle: &PhysicalBundle,
    rails: &RailReport,
) -> Result<SupplyGrid> {
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| invalid("power configuration absent"))?;
    let grid = bundle
        .technology
        .manufacturing_grid
        .filter(|g| *g > 0)
        .ok_or_else(|| invalid("power manufacturing grid absent"))?;
    let layer = |name: &str| {
        bundle
            .technology
            .layers
            .iter()
            .find(|l| l.name == name && l.kind.as_deref() == Some("ROUTING"))
            .ok_or_else(|| invalid(format!("power layer {name} absent")))
    };
    let rail = layer(&cfg.rail_layer)?;
    let strap = layer(&cfg.strap_layer)?;
    let limits = SupplyLimits::default();
    let stack = Stack::new(bundle, grid)?;
    let width = cfg
        .strap_width
        .or(strap.width)
        .ok_or_else(|| invalid("supply strap width absent"))?
        .max(strap.width.unwrap_or(0))
        .max(
            stack
                .landings
                .iter()
                .map(|(_, width)| *width)
                .max()
                .unwrap_or(0),
        );
    let spacing = cfg
        .strap_spacing
        .or(strap.spacing)
        .ok_or_else(|| invalid("supply strap spacing absent"))?
        .max(strap.spacing.unwrap_or(0));
    let mut full = rails.clone();
    full.row_rails
        .retain(|r| r.x_start == design.core_area.lx && r.x_end == design.core_area.ux);
    let mut out = build_supply_grid(
        design.die_area,
        design.core_area,
        grid,
        &full,
        &cfg.rail_layer,
        rail.width.ok_or_else(|| invalid("rail width absent"))?,
        &LayerSpec {
            name: cfg.strap_layer.clone(),
            width,
            spacing,
        },
        &stack.abstract_via,
        &limits,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let base_vias = std::mem::take(&mut out.vias);
    let base_wires = out.wires.clone();
    let mut b = Builder {
        out,
        die: design.die_area,
        grid,
        work: 0,
        limits: &limits,
    };
    for v in base_vias {
        b.stack(&stack, v.polarity, v.at)?;
    }
    let spans: Vec<&RailSpan> = rails
        .row_rails
        .iter()
        .filter(|r| r.x_start != design.core_area.lx || r.x_end != design.core_area.ux)
        .collect();
    if spans.is_empty() {
        b.out.work_units += b.work;
        return Ok(b.out);
    }
    let blocks = rustroad_core::fixed::envelopes(design)?;
    let mut ranges: Vec<(i64, i64)> = blocks
        .iter()
        .map(|r| (r.lx.max(design.core_area.lx), r.ux.min(design.core_area.ux)))
        .filter(|(a, z)| a < z)
        .collect();
    ranges.sort_unstable();
    let mut bands = Vec::new();
    let mut x = design.core_area.lx;
    for (a, z) in ranges {
        if x < a {
            bands.push((x, a));
        }
        x = x.max(z);
    }
    if x < design.core_area.ux {
        bands.push((x, design.core_area.ux));
    }
    let base: Vec<SupplyWire> = base_wires
        .iter()
        .filter(|w| w.layer == cfg.strap_layer)
        .cloned()
        .collect();
    if base.len() != 2 {
        return Err(invalid("expected two primary spines"));
    }
    let mut channels = Vec::new();
    for (lo, hi) in bands {
        b.tick()?;
        if lo == design.core_area.lx {
            channels.push((lo, hi, [base[0].start[0], base[1].start[0]]));
            continue;
        }
        let half = base[0].width / 2;
        let px = ceil(i128::from(lo) + i128::from(half), grid)?;
        let gx = integer(i128::from(px) + i128::from(base[0].width) + i128::from(spacing))?;
        if i128::from(gx) + i128::from(half) > i128::from(hi) {
            continue;
        }
        channels.push((lo, hi, [px, gx]));
        for (i, x) in [px, gx].into_iter().enumerate() {
            let mut w = base[i].clone();
            w.start[0] = x;
            w.end[0] = x;
            b.wire(w)?;
            for row in &base_wires {
                b.tick()?;
                if row.layer == cfg.rail_layer && row.polarity == base[i].polarity {
                    let bounds = wire_bounds(row).map_err(|e| invalid(e.to_string()))?;
                    if stack
                        .abstract_via
                        .lower
                        .iter()
                        .all(|r| shift(*r, [x, row.start[1]]).is_ok_and(|r| within(bounds, r)))
                    {
                        b.stack(&stack, base[i].polarity, [x, row.start[1]])?;
                    }
                }
            }
        }
    }
    for span in spans {
        b.tick()?;
        let channel = channels
            .iter()
            .find(|(lo, _, points)| {
                (*lo == design.core_area.lx && span.x_start == design.core_area.lx)
                    || (points[0] >= span.x_start && points[1] <= span.x_end)
            })
            .ok_or_else(|| invalid("segmented row has no macro-free power channel"))?;
        let i = usize::from(span.polarity == RailPolarity::Ground);
        let x = channel.2[i];
        let y = integer(
            ((i128::from(span.y_min) + i128::from(span.y_max)) / 2).div_euclid(i128::from(grid))
                * i128::from(grid),
        )?;
        let half = (y - span.y_min).min(span.y_max - y);
        let width = integer(2 * i128::from(half))?;
        let w = SupplyWire {
            polarity: span.polarity,
            layer: cfg.rail_layer.clone(),
            width,
            start: [span.x_start.min(x), y],
            end: [span.x_end.max(x), y],
        };
        let bounds = wire_bounds(&w).map_err(|e| invalid(e.to_string()))?;
        if !stack
            .abstract_via
            .lower
            .iter()
            .all(|r| shift(*r, [x, y]).is_ok_and(|r| within(bounds, r)))
        {
            return Err(invalid("segmented rail does not enclose stack landing"));
        }
        b.wire(w)?;
        b.stack(&stack, span.polarity, [x, y])?;
    }
    let mut seen = BTreeSet::new();
    b.out
        .vias
        .retain(|v| seen.insert((v.polarity, v.name.clone(), v.at)));
    let mut seen = BTreeSet::new();
    b.out
        .wires
        .retain(|w| seen.insert((w.polarity, w.layer.clone(), w.width, w.start, w.end)));
    b.out.work_units += b.work;
    b.out.scope = "segmented_row_rails_and_macro_free_channel_spines".into();
    Ok(b.out)
}
