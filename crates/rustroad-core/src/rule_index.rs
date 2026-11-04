//! Compact source indices for one-net-at-a-time geometry checking.
use crate::Result;
use crate::route_rules::{self, Shape};
use crate::routing::{RouteInput, RouteLimits, Routes, Via, Wire, budget};
use std::collections::BTreeMap;

fn charge(work: &mut usize, amount: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(amount)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("same-net index work"))?;
    Ok(())
}

pub(crate) fn order(
    n: usize,
    net: impl Fn(usize) -> u32,
    work: &mut usize,
    cap: usize,
) -> Result<Vec<u32>> {
    if n > u32::MAX as usize {
        return Err(budget("same-net source index count"));
    }
    charge(work, n.saturating_mul(2), cap)?;
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(n)
        .map_err(|_| budget("same-net index allocation"))?;
    indices.extend(0..n as u32);
    if !(1..n).all(|i| net(i - 1) <= net(i)) {
        charge(
            work,
            n.saturating_mul(n.max(1).ilog2() as usize + 1)
                .saturating_mul(2),
            cap,
        )?;
        indices.sort_unstable_by_key(|&i| (net(i as usize), i));
    }
    Ok(indices)
}

/// The net order of an input's terminals, as `order` computes it, kept for
/// many rule checks of that input (the jumper planner checks one net at a
/// time, and ordering ~230k terminals dominated each check). Using it
/// charges exactly the work `order` charges, so budgets see the same counts.
pub(crate) struct TerminalOrder {
    indices: Vec<u32>,
    presorted: bool,
}
impl TerminalOrder {
    pub(crate) fn new(input: &RouteInput) -> Result<Self> {
        let net = |i: usize| input.terminals[i].net;
        let n = input.terminals.len();
        let indices = order(n, net, &mut 0, usize::MAX)?;
        Ok(Self {
            indices,
            presorted: (1..n).all(|i| net(i - 1) <= net(i)),
        })
    }
    /// `order` over these terminals: the same charges, then the indices.
    fn order(&self, input: &RouteInput, work: &mut usize, cap: usize) -> Result<Vec<u32>> {
        let n = input.terminals.len();
        if n != self.indices.len() {
            return Err(budget("terminal order of another input"));
        }
        charge(work, n.saturating_mul(2), cap)?;
        if !self.presorted {
            charge(
                work,
                n.saturating_mul(n.max(1).ilog2() as usize + 1)
                    .saturating_mul(2),
                cap,
            )?;
        }
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(n)
            .map_err(|_| budget("same-net index allocation"))?;
        indices.extend_from_slice(&self.indices);
        Ok(indices)
    }
}

pub(crate) fn range(indices: &[u32], net: u32, source_net: impl Fn(u32) -> u32) -> &[u32] {
    let lo = indices.partition_point(|&i| source_net(i) < net);
    let end = lo + indices[lo..].partition_point(|&i| source_net(i) == net);
    &indices[lo..end]
}

pub(crate) struct RuleIndex {
    wires: Vec<u32>,
    vias: Vec<u32>,
    patches: Vec<u32>,
    terminals: Vec<u32>,
    selected: Vec<u32>,
    nets: Vec<u32>,
}

impl RuleIndex {
    pub fn new(input: &RouteInput, out: &Routes, work: &mut usize, cap: usize) -> Result<Self> {
        Self::build(input, out, work, cap, None)
    }
    /// `new` with the input's terminal order computed once beforehand.
    pub fn with_terminals(
        input: &RouteInput,
        out: &Routes,
        work: &mut usize,
        cap: usize,
        terminals: &TerminalOrder,
    ) -> Result<Self> {
        Self::build(input, out, work, cap, Some(terminals))
    }
    fn build(
        input: &RouteInput,
        out: &Routes,
        work: &mut usize,
        cap: usize,
        cached: Option<&TerminalOrder>,
    ) -> Result<Self> {
        let wires = order(out.wires.len(), |i| out.wires[i].net, work, cap)?;
        let vias = order(out.vias.len(), |i| out.vias[i].net, work, cap)?;
        let patches = order(out.patches.len(), |i| out.patches[i].net, work, cap)?;
        let terminals = match cached {
            Some(cached) => cached.order(input, work, cap)?,
            None => order(input.terminals.len(), |i| input.terminals[i].net, work, cap)?,
        };
        charge(work, out.nets.len().saturating_mul(32), cap)?;
        let mut selected = out.nets.clone();
        selected.sort_unstable();
        selected.dedup();
        let mut nets = Vec::new();
        for (ids, source) in [(&wires, 0u8), (&vias, 1), (&patches, 2)] {
            let mut previous = None;
            for &id in ids {
                let net = match source {
                    0 => out.wires[id as usize].net,
                    1 => out.vias[id as usize].net,
                    _ => out.patches[id as usize].net,
                };
                if previous != Some(net) {
                    nets.push(net);
                    previous = Some(net);
                }
            }
        }
        charge(work, nets.len().saturating_mul(32), cap)?;
        nets.sort_unstable();
        nets.dedup();
        Ok(Self {
            wires,
            vias,
            patches,
            terminals,
            selected,
            nets,
        })
    }

    pub fn nets(&self) -> &[u32] {
        &self.nets
    }

    fn check(&self, out: &Routes) -> Result<()> {
        if out.wires.len() < self.wires.len()
            || out.vias.len() < self.vias.len()
            || out.patches.len() < self.patches.len()
        {
            return Err(budget("same-net source snapshot was shortened"));
        }
        Ok(())
    }

    pub fn collect(
        &self,
        input: &RouteInput,
        out: &Routes,
        net: u32,
        limits: &RouteLimits,
        work: &mut usize,
    ) -> Result<BTreeMap<(u32, u32), Vec<Shape>>> {
        self.check(out)?;
        if input.terminals.len() < self.terminals.len() {
            return Err(budget("same-net terminal snapshot was shortened"));
        }
        charge(work, 256, limits.max_comparisons)?;
        let wires = range(&self.wires, net, |i| out.wires[i as usize].net);
        let vias = range(&self.vias, net, |i| out.vias[i as usize].net);
        let patches = range(&self.patches, net, |i| out.patches[i as usize].net);
        let terminals = range(&self.terminals, net, |i| input.terminals[i as usize].net);
        let mut count = wires.len().saturating_add(patches.len());
        for &id in vias {
            let via = &out.vias[id as usize];
            count = count.saturating_add(
                input
                    .vias
                    .get(via.kind as usize)
                    .ok_or_else(|| budget("same-net via kind"))?
                    .shapes
                    .len(),
            );
        }
        if self.selected.binary_search(&net).is_ok() {
            for &id in terminals {
                count = count.saturating_add(input.terminals[id as usize].shapes.len());
            }
        }
        if count
            > limits
                .max_obstacles
                .min(limits.max_active_nodes.saturating_mul(16))
        {
            return Err(budget("same-net geometry group size"));
        }
        charge(work, count, limits.max_comparisons)?;
        route_rules::collect_parts(
            input,
            wires.iter().map(|&i| &out.wires[i as usize]),
            vias.iter().map(|&i| &out.vias[i as usize]),
            patches.iter().map(|&i| &out.patches[i as usize]),
            terminals.iter().map(|&i| &input.terminals[i as usize]),
            &self.selected,
        )
    }

    pub fn wires<'a>(
        &'a self,
        out: &'a Routes,
        net: u32,
    ) -> Result<impl Iterator<Item = &'a Wire>> {
        self.check(out)?;
        let ids = range(&self.wires, net, |i| out.wires[i as usize].net);
        Ok(ids.iter().map(|&i| &out.wires[i as usize]).chain(
            out.wires[self.wires.len()..]
                .iter()
                .filter(move |w| w.net == net),
        ))
    }

    pub fn vias<'a>(&'a self, out: &'a Routes, net: u32) -> Result<impl Iterator<Item = &'a Via>> {
        self.check(out)?;
        let ids = range(&self.vias, net, |i| out.vias[i as usize].net);
        Ok(ids.iter().map(|&i| &out.vias[i as usize]).chain(
            out.vias[self.vias.len()..]
                .iter()
                .filter(move |v| v.net == net),
        ))
    }
}
