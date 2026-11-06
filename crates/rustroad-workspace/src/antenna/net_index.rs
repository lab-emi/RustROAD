//! Per-net record index of one routes state.
//!
//! The antenna check and the jumper repair look at a few selected nets at a
//! time, but scanned every wire, via, patch and terminal of the design for
//! each of them. This CSR index lists each net's records in their global
//! order, so per-net shape lists (and per-net route views) come out exactly
//! as the full scan's filter produced them. It is rebuilt (O(records)) after
//! the routes change.
use super::{Prepared, Shape, fail};
use crate::{Error, Result};
use rustroad_core::routing::{self as core_routing, LayerRect, Routes};
use std::collections::BTreeMap;

/// Items of each key in ascending global order.
struct Csr {
    start: Vec<u32>,
    items: Vec<u32>,
}
impl Csr {
    fn build(keys: impl Iterator<Item = u32> + Clone, slots: usize) -> Result<Self> {
        let mut start = vec![0u32; slots + 1];
        for key in keys.clone() {
            start[key as usize + 1] += 1;
        }
        for i in 0..slots {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut items = vec![0u32; start[slots] as usize];
        for (i, key) in keys.enumerate() {
            let at = &mut fill[key as usize];
            items[*at as usize] =
                u32::try_from(i).map_err(|_| Error::Budget("ANTENNA: route index".into()))?;
            *at += 1;
        }
        Ok(Self { start, items })
    }
    fn of(&self, key: u32) -> &[u32] {
        let k = key as usize;
        if k + 1 >= self.start.len() {
            return &[];
        }
        &self.items[self.start[k] as usize..self.start[k + 1] as usize]
    }
}

pub(super) struct NetIndex {
    wires: Csr,
    vias: Csr,
    patches: Csr,
    access: Csr,
    terminals: Csr,
    /// Record counts of the indexed routes (a stale index is refused).
    counts: [usize; 4],
}

impl NetIndex {
    pub(super) fn new(prep: &Prepared, routes: &Routes) -> Result<Self> {
        let slots = routes
            .wires
            .iter()
            .map(|w| w.net)
            .chain(routes.vias.iter().map(|v| v.net))
            .chain(routes.patches.iter().map(|p| p.net))
            .chain(routes.access.iter().map(|a| a.net))
            .chain(prep.input.terminals.iter().map(|t| t.net))
            .max()
            .map_or(0, |n| n as usize + 1);
        if slots > 16 << 20 {
            return Err(Error::Budget("ANTENNA: route index nets".into()));
        }
        Ok(Self {
            wires: Csr::build(routes.wires.iter().map(|w| w.net), slots)?,
            vias: Csr::build(routes.vias.iter().map(|v| v.net), slots)?,
            patches: Csr::build(routes.patches.iter().map(|p| p.net), slots)?,
            access: Csr::build(routes.access.iter().map(|a| a.net), slots)?,
            terminals: Csr::build(prep.input.terminals.iter().map(|t| t.net), slots)?,
            counts: Self::counts(routes),
        })
    }
    fn counts(routes: &Routes) -> [usize; 4] {
        [
            routes.wires.len(),
            routes.vias.len(),
            routes.patches.len(),
            routes.access.len(),
        ]
    }
    pub(super) fn check(&self, routes: &Routes) -> Result<()> {
        if self.counts != Self::counts(routes) {
            return Err(fail("ANTENNA_INDEX", "route index is stale"));
        }
        Ok(())
    }
    /// Terminal indices of `net` in input order.
    pub(super) fn terminals(&self, net: u32) -> &[u32] {
        self.terminals.of(net)
    }
    /// Shapes of one net in the full scan's order: wires, via shapes,
    /// patches, terminal shapes. `None` when the net has none.
    fn shapes(&self, prep: &Prepared, routes: &Routes, net: u32) -> Result<Vec<Shape>> {
        let input = &prep.input;
        let mut out = Vec::new();
        for &i in self.wires.of(net) {
            let w = &routes.wires[i as usize];
            out.push(Shape {
                layer: w.layer,
                rect: core_routing::wire_rect(w, input.layers[w.layer as usize].width)?,
                terminal: None,
            });
        }
        for &i in self.vias.of(net) {
            let v = &routes.vias[i as usize];
            let via = input
                .vias
                .get(v.kind as usize)
                .ok_or_else(|| fail("ANTENNA_ROUTES", "via kind outside the technology"))?;
            for LayerRect { layer, rect } in &via.shapes {
                out.push(Shape {
                    layer: *layer,
                    rect: core_routing::shifted(*rect, v.at)?,
                    terminal: None,
                });
            }
        }
        for &i in self.patches.of(net) {
            let p = &routes.patches[i as usize];
            out.push(Shape {
                layer: p.layer,
                rect: p.rect,
                terminal: None,
            });
        }
        for &ti in self.terminals.of(net) {
            for s in &input.terminals[ti as usize].shapes {
                out.push(Shape {
                    layer: s.layer,
                    rect: s.rect,
                    terminal: Some(ti as usize),
                });
            }
        }
        Ok(out)
    }
    /// Shape count of one net (as `shapes` would return).
    fn shape_count(&self, prep: &Prepared, routes: &Routes, net: u32) -> usize {
        let vias: usize = self
            .vias
            .of(net)
            .iter()
            .map(|&i| {
                prep.input
                    .vias
                    .get(routes.vias[i as usize].kind as usize)
                    .map_or(0, |v| v.shapes.len())
            })
            .sum();
        let terminals: usize = self
            .terminals
            .of(net)
            .iter()
            .map(|&ti| prep.input.terminals[ti as usize].shapes.len())
            .sum();
        self.wires.of(net).len() + vias + self.patches.of(net).len() + terminals
    }
    /// `net_shapes` restricted to `nets` (each routed): the same map, or
    /// `None` when a shape budget would trip (the caller then runs the full
    /// scan, which reports the budget exactly as before).
    pub(super) fn net_shapes(
        &self,
        prep: &Prepared,
        routes: &Routes,
        nets: &[u32],
        max_shapes_per_net: usize,
        max_total_shapes: usize,
    ) -> Result<Option<BTreeMap<u32, Vec<Shape>>>> {
        self.check(routes)?;
        let mut total = 0usize;
        for &net in nets {
            let n = self.shape_count(prep, routes, net);
            total = total.saturating_add(n);
            if n > max_shapes_per_net || total > max_total_shapes {
                return Ok(None);
            }
        }
        let mut out = BTreeMap::new();
        for &net in nets {
            let shapes = self.shapes(prep, routes, net)?;
            if !shapes.is_empty() {
                out.insert(net, shapes);
            }
        }
        Ok(Some(out))
    }
    /// The records of one net (rule checks and jumper planning only filter
    /// by net, so a view gives the same answers as the full routes).
    pub(super) fn local(&self, routes: &Routes, net: u32) -> Result<Routes> {
        self.check(routes)?;
        Ok(Routes {
            wires: self
                .wires
                .of(net)
                .iter()
                .map(|&i| routes.wires[i as usize].clone())
                .collect(),
            vias: self
                .vias
                .of(net)
                .iter()
                .map(|&i| routes.vias[i as usize].clone())
                .collect(),
            access: self
                .access
                .of(net)
                .iter()
                .map(|&i| routes.access[i as usize].clone())
                .collect(),
            patches: self
                .patches
                .of(net)
                .iter()
                .map(|&i| routes.patches[i as usize].clone())
                .collect(),
            nets: vec![net],
            stats: Default::default(),
        })
    }
}

/// Shapes of a per-net view in the full scan's order (its terminals from the index).
pub(super) fn local_shapes(
    prep: &Prepared,
    index: &NetIndex,
    local: &Routes,
    net: u32,
) -> Result<Vec<Shape>> {
    let input = &prep.input;
    let mut out = Vec::new();
    for w in &local.wires {
        out.push(Shape {
            layer: w.layer,
            rect: core_routing::wire_rect(w, input.layers[w.layer as usize].width)?,
            terminal: None,
        });
    }
    for v in &local.vias {
        let via = input
            .vias
            .get(v.kind as usize)
            .ok_or_else(|| fail("ANTENNA_ROUTES", "via kind outside the technology"))?;
        for LayerRect { layer, rect } in &via.shapes {
            out.push(Shape {
                layer: *layer,
                rect: core_routing::shifted(*rect, v.at)?,
                terminal: None,
            });
        }
    }
    for p in &local.patches {
        out.push(Shape {
            layer: p.layer,
            rect: p.rect,
            terminal: None,
        });
    }
    for &ti in index.terminals(net) {
        for s in &input.terminals[ti as usize].shapes {
            out.push(Shape {
                layer: s.layer,
                rect: s.rect,
                terminal: Some(ti as usize),
            });
        }
    }
    Ok(out)
}
