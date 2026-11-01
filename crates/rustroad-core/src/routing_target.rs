//! Bounded routing-tree distance including unavoidable layer changes and directional costs.
use crate::routing::{budget, distance};
use crate::{Rect, Result};
use std::collections::VecDeque;

// The relaxed graph ignores obstacles and minimum-area rules. Its layer walks
// remember which preferred directions are available, so every physical route
// costs at least one of the three displacement modes below.
pub(crate) struct CostModel {
    hops: Vec<Vec<[u16; 4]>>,
    via: u64,
    wrong: u64,
}
impl CostModel {
    pub fn new(vertical: &[bool], edges: &[(usize, usize)], via: u64, wrong: u64) -> Result<Self> {
        let n = vertical.len();
        if n == 0 || n > 64 || edges.len() > 4096 || wrong == 0 || via == 0 {
            return Err(budget("routing lower-bound layer graph"));
        }
        let bit = |i: usize| if vertical[i] { 2usize } else { 1usize };
        let mut adjacent = vec![Vec::new(); n];
        for &(a, b) in edges {
            if a >= n || b >= n {
                return Err(budget("routing lower-bound layer index"));
            }
            adjacent[a].push(b);
            adjacent[b].push(a);
        }
        let mut hops = vec![vec![[u16::MAX; 4]; n]; n];
        for source in 0..n {
            let mut distance = vec![[u16::MAX; 4]; n];
            distance[source][bit(source)] = 0;
            let mut queue = VecDeque::from([(source, bit(source))]);
            while let Some((at, seen)) = queue.pop_front() {
                for &next in &adjacent[at] {
                    let mask = seen | bit(next);
                    if distance[next][mask] == u16::MAX {
                        distance[next][mask] = distance[at][seen] + 1;
                        queue.push_back((next, mask));
                    }
                }
            }
            for (target, values) in distance.iter().enumerate() {
                for (required, minimum) in hops[source][target].iter_mut().enumerate() {
                    *minimum = values
                        .iter()
                        .enumerate()
                        .filter(|(mask, _)| mask & required == required)
                        .map(|(_, &n)| n)
                        .min()
                        .unwrap_or(u16::MAX);
                }
            }
        }
        Ok(Self { hops, via, wrong })
    }
    fn estimate(&self, from: usize, to: usize, dx: u64, dy: u64) -> Option<u64> {
        let hops = self.hops.get(from)?.get(to)?;
        let cost = |mask: usize, x: u64, y: u64| {
            (hops[mask] != u16::MAX).then_some(())?;
            u64::from(hops[mask])
                .checked_mul(self.via)?
                .checked_add(x)?
                .checked_add(y)
        };
        match (dx == 0, dy == 0) {
            (true, true) => cost(0, 0, 0),
            (false, true) => [
                cost(1, dx, 0),
                dx.checked_mul(self.wrong).and_then(|x| cost(2, x, 0)),
            ]
            .into_iter()
            .flatten()
            .min(),
            (true, false) => [
                cost(2, 0, dy),
                dy.checked_mul(self.wrong).and_then(|y| cost(1, 0, y)),
            ]
            .into_iter()
            .flatten()
            .min(),
            (false, false) => [
                cost(3, dx, dy),
                dy.checked_mul(self.wrong).and_then(|y| cost(1, dx, y)),
                dx.checked_mul(self.wrong).and_then(|x| cost(2, x, dy)),
            ]
            .into_iter()
            .flatten()
            .min(),
        }
    }
}

#[derive(Clone, Copy)]
struct Target {
    layer: usize,
    rect: Rect,
}

const PENDING: usize = 32;
const BLOCK_LEVELS: usize = 32;
const NONE: u32 = u32::MAX;

/// Covers target buffers, two BVH nodes per target, merge relocation and stacks.
pub(crate) fn memory_bytes(targets: usize) -> u64 {
    (targets as u64)
        .saturating_mul(192)
        .saturating_add(64 * 1024)
}

fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(count)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("routing target-distance work"))?;
    Ok(())
}

fn merged(a: Rect, b: Rect) -> Rect {
    Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    }
}

struct Node {
    rect: Rect,
    layers: u64,
    left: u32,
    right: u32,
}

struct Block {
    targets: Vec<Target>,
    nodes: Vec<Node>,
    root: u32,
}

impl Block {
    fn new(mut targets: Vec<Target>, work: &mut usize, cap: usize) -> Result<Self> {
        let count = targets
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_sub(1))
            .ok_or_else(|| budget("routing target-index size"))?;
        let mut nodes = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| budget("routing target-index allocation"))?;
        fn build(
            targets: &mut [Target],
            nodes: &mut Vec<Node>,
            work: &mut usize,
            cap: usize,
        ) -> Result<u32> {
            charge(
                work,
                targets
                    .len()
                    .saturating_mul(targets.len().ilog2() as usize + 3),
                cap,
            )?;
            let rect = targets
                .iter()
                .skip(1)
                .fold(targets[0].rect, |a, t| merged(a, t.rect));
            let layers = targets.iter().fold(0u64, |m, t| m | (1u64 << t.layer));
            let (left, right) = if targets.len() == 1 {
                (NONE, NONE)
            } else {
                let x = i128::from(rect.ux) - i128::from(rect.lx);
                let y = i128::from(rect.uy) - i128::from(rect.ly);
                targets.sort_unstable_by_key(|t| {
                    let cx = i128::from(t.rect.lx) + i128::from(t.rect.ux);
                    let cy = i128::from(t.rect.ly) + i128::from(t.rect.uy);
                    (
                        if x >= y { cx } else { cy },
                        cx,
                        cy,
                        t.layer,
                        t.rect.lx,
                        t.rect.ly,
                        t.rect.ux,
                        t.rect.uy,
                    )
                });
                let middle = targets.len() / 2;
                let (a, b) = targets.split_at_mut(middle);
                (build(a, nodes, work, cap)?, build(b, nodes, work, cap)?)
            };
            let id = u32::try_from(nodes.len()).map_err(|_| budget("routing target-index id"))?;
            nodes.push(Node {
                rect,
                layers,
                left,
                right,
            });
            Ok(id)
        }
        let root = build(&mut targets, &mut nodes, work, cap)?;
        Ok(Self {
            targets,
            nodes,
            root,
        })
    }

    fn lower_bound(
        node: &Node,
        layer: usize,
        p: [i64; 2],
        model: &CostModel,
        work: &mut usize,
        cap: usize,
    ) -> Result<Option<u64>> {
        let dx = p[0].abs_diff(p[0].clamp(node.rect.lx, node.rect.ux));
        let dy = p[1].abs_diff(p[1].clamp(node.rect.ly, node.rect.uy));
        charge(work, node.layers.count_ones() as usize + 1, cap)?;
        let mut mask = node.layers;
        let mut best = None;
        while mask != 0 {
            let target = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if let Some(cost) = model.estimate(layer, target, dx, dy) {
                best = Some(best.map_or(cost, |b: u64| b.min(cost)));
            }
        }
        Ok(best)
    }

    fn distance(
        &self,
        layer: usize,
        p: [i64; 2],
        model: &CostModel,
        mut best: Option<u64>,
        work: &mut usize,
        cap: usize,
    ) -> Result<Option<u64>> {
        let Some(root_bound) =
            Self::lower_bound(&self.nodes[self.root as usize], layer, p, model, work, cap)?
        else {
            return Ok(best);
        };
        let mut stack = [(0u32, 0u64); 64];
        stack[0] = (self.root, root_bound);
        let mut len = 1;
        let mut visits = 0;
        while len != 0 {
            len -= 1;
            let (id, bound) = stack[len];
            visits += 1;
            if visits > self.nodes.len() {
                return Err(budget("routing target-index visit limit"));
            }
            if best.is_some_and(|b| bound >= b) {
                continue;
            }
            let node = &self.nodes[id as usize];
            if node.left == NONE {
                best = Some(bound);
                continue;
            }
            let mut children = [(node.left, u64::MAX), (node.right, u64::MAX)];
            for child in &mut children {
                child.1 =
                    Self::lower_bound(&self.nodes[child.0 as usize], layer, p, model, work, cap)?
                        .unwrap_or(u64::MAX);
            }
            children.sort_unstable_by_key(|&(id, cost)| (std::cmp::Reverse(cost), id));
            for (id, cost) in children {
                if cost == u64::MAX || best.is_some_and(|b| cost >= b) {
                    continue;
                }
                if len == stack.len() {
                    return Err(budget("routing target-index stack limit"));
                }
                stack[len] = (id, cost);
                len += 1;
            }
        }
        Ok(best)
    }
}

/// Logarithmic forest of immutable balanced BVHs; every stored segment stays exact.
pub(crate) struct TreeDistance {
    pending: Vec<Target>,
    blocks: Vec<Option<Block>>,
    bounds: Rect,
    count: usize,
    limit: usize,
}

impl TreeDistance {
    pub fn new(layer: usize, point: [i64; 2], limit: usize) -> Result<Self> {
        if layer >= 64 || limit == 0 || limit > 1_000_000 {
            return Err(budget("routing target-segment limit"));
        }
        let rect = Rect {
            lx: point[0],
            ly: point[1],
            ux: point[0],
            uy: point[1],
        };
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(PENDING)
            .map_err(|_| budget("routing target staging allocation"))?;
        pending.push(Target { layer, rect });
        Ok(Self {
            pending,
            blocks: (0..BLOCK_LEVELS).map(|_| None).collect(),
            bounds: rect,
            count: 1,
            limit,
        })
    }
    pub fn bounds_distance(&self, point: [i64; 2]) -> u64 {
        point[0]
            .abs_diff(point[0].clamp(self.bounds.lx, self.bounds.ux))
            .saturating_add(point[1].abs_diff(point[1].clamp(self.bounds.ly, self.bounds.uy)))
    }
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn add(
        &mut self,
        layer: usize,
        a: [i64; 2],
        b: [i64; 2],
        work: &mut usize,
        cap: usize,
    ) -> Result<()> {
        if layer >= 64 {
            return Err(budget("routing target layer"));
        }
        let rect = Rect {
            lx: a[0].min(b[0]),
            ly: a[1].min(b[1]),
            ux: a[0].max(b[0]),
            uy: a[1].max(b[1]),
        };
        if self
            .pending
            .last()
            .is_some_and(|t| t.layer == layer && t.rect == rect)
        {
            return Ok(());
        }
        if self.count == self.limit {
            return Err(budget("routing target-segment limit"));
        }
        charge(work, 1, cap)?;
        if self.pending.capacity() < PENDING {
            self.pending
                .try_reserve_exact(PENDING - self.pending.len())
                .map_err(|_| budget("routing target staging allocation"))?;
        }
        self.pending.push(Target { layer, rect });
        self.bounds = merged(self.bounds, rect);
        self.count += 1;
        if self.pending.len() == PENDING {
            let mut targets = std::mem::take(&mut self.pending);
            let mut level = 0;
            loop {
                if level == self.blocks.len() {
                    return Err(budget("routing target-index levels"));
                }
                let Some(old) = self.blocks[level].take() else {
                    break;
                };
                let Block {
                    targets: mut previous,
                    nodes,
                    ..
                } = old;
                drop(nodes);
                previous
                    .try_reserve_exact(targets.len())
                    .map_err(|_| budget("routing target merge allocation"))?;
                previous.append(&mut targets);
                targets = previous;
                level += 1;
            }
            self.blocks[level] = Some(Block::new(targets, work, cap)?);
        }
        Ok(())
    }

    pub fn distance(
        &self,
        layer: usize,
        p: [i64; 2],
        model: &CostModel,
        work: &mut usize,
        cap: usize,
    ) -> Result<u64> {
        let mut best = None;
        for target in &self.pending {
            charge(work, 3, cap)?;
            let r = target.rect;
            if let Some(cost) = model.estimate(
                layer,
                target.layer,
                p[0].abs_diff(p[0].clamp(r.lx, r.ux)),
                p[1].abs_diff(p[1].clamp(r.ly, r.uy)),
            ) {
                best = Some(best.map_or(cost, |b: u64| b.min(cost)));
            }
        }
        for block in self.blocks.iter().flatten() {
            best = block.distance(layer, p, model, best, work, cap)?;
        }
        best.map_or_else(
            || {
                distance(
                    p,
                    [
                        p[0].clamp(self.bounds.lx, self.bounds.ux),
                        p[1].clamp(self.bounds.ly, self.bounds.uy),
                    ],
                )
            },
            Ok,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layer_bound_matches_independent_grid_shortest_paths() {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;

        // At most 100 states and 600 directed edges; no route or path enumeration.
        let topologies = [
            (vec![false], vec![]),
            (vec![false, true, false], vec![(0, 1), (1, 2)]),
            (vec![false, false, true, true], vec![(0, 1), (1, 2)]),
        ];
        for (vertical, edges) in topologies {
            let states = vertical.len() * 25;
            for via in [1, 80, 1000] {
                for wrong in [1, 4] {
                    let model = CostModel::new(&vertical, &edges, via, wrong).unwrap();
                    for goal in 0..states {
                        let mut actual = vec![u64::MAX; states];
                        actual[goal] = 0;
                        let mut heap = BinaryHeap::from([Reverse((0, goal))]);
                        let mut pops = 0;
                        while let Some(Reverse((cost, id))) = heap.pop() {
                            pops += 1;
                            assert!(pops < states * 16, "bounded grid search");
                            if actual[id] != cost {
                                continue;
                            }
                            let layer = id / 25;
                            let x = id % 5;
                            let y = id % 25 / 5;
                            let mut next = Vec::with_capacity(8);
                            for (dx, dy) in [(-1isize, 0), (1, 0), (0, -1), (0, 1)] {
                                let Some(nx) = x.checked_add_signed(dx).filter(|&v| v < 5) else {
                                    continue;
                                };
                                let Some(ny) = y.checked_add_signed(dy).filter(|&v| v < 5) else {
                                    continue;
                                };
                                let preferred = vertical[layer] == (dy != 0);
                                next.push((
                                    layer * 25 + ny * 5 + nx,
                                    40 * if preferred { 1 } else { wrong },
                                ));
                            }
                            for &(a, b) in &edges {
                                if layer == a {
                                    next.push((b * 25 + y * 5 + x, via));
                                } else if layer == b {
                                    next.push((a * 25 + y * 5 + x, via));
                                }
                            }
                            for (id, step) in next {
                                let candidate = cost + step;
                                if candidate < actual[id] {
                                    actual[id] = candidate;
                                    heap.push(Reverse((candidate, id)));
                                }
                            }
                        }
                        for (start, expected) in actual.into_iter().enumerate() {
                            let bound = model.estimate(
                                start / 25,
                                goal / 25,
                                (start % 5).abs_diff(goal % 5) as u64 * 40,
                                (start % 25 / 5).abs_diff(goal % 25 / 5) as u64 * 40,
                            );
                            assert_eq!(
                                bound.unwrap_or(u64::MAX),
                                expected,
                                "layers={vertical:?}, via={via}, wrong={wrong}, start={start}, goal={goal}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn l_tree_matches_independent_lattice_targets_and_beats_box_interior() {
        let mut tree = TreeDistance::new(0, [-5, -5], 8192).unwrap();
        let model = CostModel::new(&[false], &[], 80, 1).unwrap();
        let mut work = 0;
        tree.add(0, [-5, -5], [5, -5], &mut work, 1_000_000)
            .unwrap();
        tree.add(0, [5, -5], [5, 5], &mut work, 1_000_000).unwrap();
        let targets: Vec<_> = (-5i64..=5)
            .map(|x| [x, -5])
            .chain((-5..=5).map(|y| [5, y]))
            .collect();
        for x in -8i64..=8 {
            for y in -8i64..=8 {
                let expected = targets
                    .iter()
                    .map(|p| x.abs_diff(p[0]) + y.abs_diff(p[1]))
                    .min()
                    .unwrap();
                assert_eq!(
                    tree.distance(0, [x, y], &model, &mut work, 1_000_000)
                        .unwrap(),
                    expected
                );
            }
        }
        assert_eq!(
            tree.distance(0, [0, 0], &model, &mut work, 1_000_000)
                .unwrap(),
            5
        );
        assert!(tree.distance(0, [0, 0], &model, &mut work, 0).is_err());
    }
    #[test]
    fn segment_budget_fails_without_discarding_target_geometry() {
        let mut tree = TreeDistance::new(0, [0, 0], 3).unwrap();
        let model = CostModel::new(&[false], &[], 80, 1).unwrap();
        let mut work = 0;
        tree.add(0, [2, 2], [2, 2], &mut work, 1000).unwrap();
        tree.add(0, [4, 4], [4, 4], &mut work, 1000).unwrap();
        assert!(tree.add(0, [6, 6], [6, 6], &mut work, 1000).is_err());
        assert_eq!(tree.len(), 3);
        assert_eq!(
            tree.distance(0, [1, 1], &model, &mut work, 1000).unwrap(),
            2
        );
    }

    #[test]
    fn large_index_matches_linear_distance_without_a_box_fallback() {
        let mut tree = TreeDistance::new(0, [0, 0], 20_000).unwrap();
        let mut points = vec![[0i64, 0i64]];
        let mut work = 0;
        for i in 1..10_000i64 {
            let p = [2 * i, 3 * i];
            tree.add(0, p, p, &mut work, 100_000_000).unwrap();
            points.push(p);
        }
        let model = CostModel::new(&[false], &[], 80, 1).unwrap();
        for q in [[10001i64, 14999], [500, 749], [-20, 100], [22000, 33000]] {
            let expected = points
                .iter()
                .map(|p| q[0].abs_diff(p[0]) + q[1].abs_diff(p[1]))
                .min()
                .unwrap();
            assert_eq!(
                tree.distance(0, q, &model, &mut work, 100_000_000).unwrap(),
                expected
            );
        }
        assert!(tree.distance(0, [10001, 14999], &model, &mut 0, 1).is_err());
    }

    #[test]
    fn merged_blocks_match_weighted_linear_segment_oracle() {
        // Fewer than 5,000 segments and 1.3M oracle comparisons; <2 MiB scratch.
        let model = CostModel::new(&[false, true], &[(0, 1)], 80, 4).unwrap();
        let mut tree = TreeDistance::new(0, [-5000, 5000], 5000).unwrap();
        let mut segments = vec![(0usize, [-5000i64, 5000], [-5000i64, 5000])];
        let mut random = 197u64;
        let mut draw = || {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((random >> 32) % 10001) as i64 - 5000
        };
        let mut work = 0;
        for index in 1..5000 {
            let a = [draw(), draw()];
            let mut b = a;
            b[index % 2] = draw();
            let layer = index % 2;
            tree.add(layer, a, b, &mut work, 100_000_000).unwrap();
            segments.push((layer, a, b));
            if index < 70 || index % 32 == 0 {
                let q = [draw(), draw()];
                let expected = segments
                    .iter()
                    .map(|&(to, a, b)| {
                        let dx = q[0].abs_diff(q[0].clamp(a[0].min(b[0]), a[0].max(b[0])));
                        let dy = q[1].abs_diff(q[1].clamp(a[1].min(b[1]), a[1].max(b[1])));
                        if to != layer {
                            dx + dy + 80
                        } else if layer == 0 {
                            (dx + 4 * dy).min(dx + dy + 160)
                        } else {
                            (4 * dx + dy).min(dx + dy + 160)
                        }
                    })
                    .min()
                    .unwrap();
                assert_eq!(
                    tree.distance(layer, q, &model, &mut work, 100_000_000)
                        .unwrap(),
                    expected,
                    "after {index} segments, querying {q:?} on layer {layer}"
                );
            }
        }
    }
}
