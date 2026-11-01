//! Bounded endpoint graphs shared by route pruning and connectivity validation.
use crate::Result;
use crate::routing::{RouteInput, RouteLimits, Routes, budget, invalid};
use crate::rule_index::{order, range};
use std::collections::VecDeque;

type Key = (u32, i64, i64);

fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(count)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("route endpoint graph work"))?;
    Ok(())
}

fn reserve<T>(count: usize) -> Result<Vec<T>> {
    let mut items = Vec::new();
    items
        .try_reserve_exact(count)
        .map_err(|_| budget("route endpoint graph allocation"))?;
    Ok(items)
}

/// Global storage is one compact source index per wire, via and access point.
pub(crate) struct NetIndex {
    wires: Vec<u32>,
    vias: Vec<u32>,
    access: Vec<u32>,
    nets: Vec<u32>,
}

#[derive(Clone, Copy)]
pub(crate) struct Sources<'a> {
    pub wires: &'a [u32],
    pub vias: &'a [u32],
    access: &'a [u32],
}

impl NetIndex {
    pub fn new(out: &Routes, limits: &RouteLimits, work: &mut usize) -> Result<Self> {
        if out.wires.len().saturating_add(out.vias.len()) > limits.max_segments {
            return Err(budget("route endpoint source count"));
        }
        let cap = limits.max_comparisons;
        let wires = order(out.wires.len(), |i| out.wires[i].net, work, cap)?;
        let vias = order(out.vias.len(), |i| out.vias[i].net, work, cap)?;
        let access = order(out.access.len(), |i| out.access[i].net, work, cap)?;
        let mut nets = Vec::new();
        for (indices, source) in [(&wires, 0), (&vias, 1), (&access, 2)] {
            let mut previous = None;
            for &i in indices {
                let net = match source {
                    0 => out.wires[i as usize].net,
                    1 => out.vias[i as usize].net,
                    _ => out.access[i as usize].net,
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
            access,
            nets,
        })
    }

    pub fn nets(&self) -> &[u32] {
        &self.nets
    }

    pub fn sources<'a>(&'a self, out: &Routes, net: u32) -> Sources<'a> {
        Sources {
            wires: range(&self.wires, net, |i| out.wires[i as usize].net),
            vias: range(&self.vias, net, |i| out.vias[i as usize].net),
            access: range(&self.access, net, |i| out.access[i as usize].net),
        }
    }
}

/// Only the current net's integer endpoints and packed incidence remain live.
pub(crate) struct Graph {
    offsets: Vec<u32>,
    incidence: Vec<u32>,
    ends: Vec<(u32, u32)>,
    anchors: Vec<bool>,
}

impl Graph {
    pub fn new(
        input: &RouteInput,
        out: &Routes,
        sources: Sources<'_>,
        limits: &RouteLimits,
        work: &mut usize,
    ) -> Result<Self> {
        let edges = sources.wires.len().saturating_add(sources.vias.len());
        if edges > limits.max_active_nodes || sources.access.len() > limits.max_active_nodes {
            return Err(budget("route endpoint graph per-net size"));
        }
        let count = edges
            .checked_mul(2)
            .and_then(|n| n.checked_add(sources.access.len()))
            .filter(|&n| n <= u32::MAX as usize)
            .ok_or_else(|| budget("route endpoint graph node count"))?;
        charge(
            work,
            count.saturating_mul((count.max(1).ilog2() as usize + 1) * 2 + 8),
            limits.max_comparisons,
        )?;
        let mut keys: Vec<Key> = reserve(count)?;
        let mut ends = reserve(edges)?;
        for &i in sources.wires {
            let w = &out.wires[i as usize];
            keys.push((w.layer, w.start[0], w.start[1]));
            keys.push((w.layer, w.end[0], w.end[1]));
        }
        for &i in sources.vias {
            let v = &out.vias[i as usize];
            let spec = input
                .vias
                .get(v.kind as usize)
                .ok_or_else(|| invalid("stored via identity"))?;
            keys.push((spec.lower, v.at[0], v.at[1]));
            keys.push((spec.upper, v.at[0], v.at[1]));
        }
        for &i in sources.access {
            let a = &out.access[i as usize];
            keys.push((a.layer, a.at[0], a.at[1]));
        }
        keys.sort_unstable();
        keys.dedup();
        if keys.len() > limits.max_active_nodes {
            return Err(budget("route endpoint graph active node count"));
        }
        let id = |key: Key| -> u32 {
            keys.binary_search(&key)
                .expect("collected route endpoint exists") as u32
        };
        let mut offsets = vec![0u32; keys.len() + 1];
        for &i in sources.wires {
            let w = &out.wires[i as usize];
            ends.push((
                id((w.layer, w.start[0], w.start[1])),
                id((w.layer, w.end[0], w.end[1])),
            ));
        }
        for &i in sources.vias {
            let v = &out.vias[i as usize];
            let spec = &input.vias[v.kind as usize];
            ends.push((
                id((spec.lower, v.at[0], v.at[1])),
                id((spec.upper, v.at[0], v.at[1])),
            ));
        }
        for &(a, b) in &ends {
            offsets[a as usize + 1] += 1;
            offsets[b as usize + 1] += 1;
        }
        for i in 1..offsets.len() {
            offsets[i] += offsets[i - 1];
        }
        let mut incidence = vec![0; edges * 2];
        let mut cursor = offsets[..keys.len()].to_vec();
        for (i, &(a, b)) in ends.iter().enumerate() {
            for n in [a, b] {
                incidence[cursor[n as usize] as usize] = i as u32;
                cursor[n as usize] += 1;
            }
        }
        let mut anchors = vec![false; keys.len()];
        for &i in sources.access {
            let a = &out.access[i as usize];
            anchors[id((a.layer, a.at[0], a.at[1])) as usize] = true;
        }
        Ok(Self {
            offsets,
            incidence,
            ends,
            anchors,
        })
    }

    pub fn pruned(&self, work: &mut usize, cap: usize) -> Result<Vec<bool>> {
        let mut alive = vec![true; self.ends.len()];
        let mut degree: Vec<u32> = self.offsets.windows(2).map(|w| w[1] - w[0]).collect();
        let mut queue: VecDeque<u32> = degree
            .iter()
            .enumerate()
            .filter(|&(i, d)| *d == 1 && !self.anchors[i])
            .map(|(i, _)| i as u32)
            .collect();
        while let Some(node) = queue.pop_front() {
            charge(work, 1, cap)?;
            if degree[node as usize] != 1 {
                continue;
            }
            let mut edge = None;
            for &i in &self.incidence
                [self.offsets[node as usize] as usize..self.offsets[node as usize + 1] as usize]
            {
                charge(work, 1, cap)?;
                if alive[i as usize] {
                    edge = Some(i as usize);
                    break;
                }
            }
            let Some(edge) = edge else {
                continue;
            };
            alive[edge] = false;
            let (a, b) = self.ends[edge];
            for n in [a, b] {
                degree[n as usize] -= 1;
                if degree[n as usize] == 1 && !self.anchors[n as usize] {
                    queue.push_back(n);
                }
            }
        }
        Ok(alive)
    }

    pub fn connected(&self, work: &mut usize, cap: usize) -> Result<bool> {
        if self.anchors.is_empty() {
            return Ok(true);
        }
        let mut visited = vec![false; self.anchors.len()];
        let mut queue = VecDeque::from([0u32]);
        visited[0] = true;
        let mut count = 0usize;
        while let Some(node) = queue.pop_front() {
            count += 1;
            for &edge in &self.incidence
                [self.offsets[node as usize] as usize..self.offsets[node as usize + 1] as usize]
            {
                charge(work, 1, cap)?;
                let (a, b) = self.ends[edge as usize];
                let next = if a == node { b } else { a };
                if !visited[next as usize] {
                    visited[next as usize] = true;
                    queue.push_back(next);
                }
            }
        }
        Ok(count == visited.len())
    }
}
