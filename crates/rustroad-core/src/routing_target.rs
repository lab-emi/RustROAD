//! Bounded routing-tree distance including unavoidable layer changes and directional costs.
use crate::routing::{budget, distance};
use crate::{Rect, Result};
use std::collections::VecDeque;

const MAX_SEGMENTS: usize = 4096;
pub(crate) const MEMORY_ALLOWANCE: u64 = 512 * 1024;

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

struct Target {
    layer: usize,
    rect: Rect,
}

pub(crate) struct TreeDistance {
    segments: Vec<Target>,
    bounds: Rect,
    fallback: bool,
}
impl TreeDistance {
    pub fn bounds_distance(&self, point: [i64; 2]) -> u64 {
        point[0]
            .abs_diff(point[0].clamp(self.bounds.lx, self.bounds.ux))
            .saturating_add(point[1].abs_diff(point[1].clamp(self.bounds.ly, self.bounds.uy)))
    }
    pub fn new(layer: usize, point: [i64; 2]) -> Self {
        let r = Rect {
            lx: point[0],
            ly: point[1],
            ux: point[0],
            uy: point[1],
        };
        Self {
            segments: vec![Target { layer, rect: r }],
            bounds: r,
            fallback: false,
        }
    }
    fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
        *work = work
            .checked_add(count)
            .filter(|&n| n <= cap)
            .ok_or_else(|| budget("routing target-distance work"))?;
        Ok(())
    }
    pub fn add(
        &mut self,
        layer: usize,
        a: [i64; 2],
        b: [i64; 2],
        work: &mut usize,
        cap: usize,
    ) -> Result<()> {
        let r = Rect {
            lx: a[0].min(b[0]),
            ly: a[1].min(b[1]),
            ux: a[0].max(b[0]),
            uy: a[1].max(b[1]),
        };
        self.bounds.lx = self.bounds.lx.min(r.lx);
        self.bounds.ly = self.bounds.ly.min(r.ly);
        self.bounds.ux = self.bounds.ux.max(r.ux);
        self.bounds.uy = self.bounds.uy.max(r.uy);
        if self.fallback {
            return Ok(());
        }
        Self::charge(work, self.segments.len(), cap)?;
        for target in &mut self.segments {
            if target.layer != layer {
                continue;
            }
            let old = &mut target.rect;
            let horizontal = old.ly == old.uy
                && r.ly == r.uy
                && old.ly == r.ly
                && old.lx <= r.ux
                && r.lx <= old.ux;
            let vertical = old.lx == old.ux
                && r.lx == r.ux
                && old.lx == r.lx
                && old.ly <= r.uy
                && r.ly <= old.uy;
            if horizontal || vertical {
                old.lx = old.lx.min(r.lx);
                old.ly = old.ly.min(r.ly);
                old.ux = old.ux.max(r.ux);
                old.uy = old.uy.max(r.uy);
                return Ok(());
            }
            if r.lx == r.ux
                && r.ly == r.uy
                && old.lx <= r.lx
                && r.lx <= old.ux
                && old.ly <= r.ly
                && r.ly <= old.uy
            {
                return Ok(());
            }
        }
        if self.segments.len() == MAX_SEGMENTS {
            self.segments = Vec::new();
            self.fallback = true;
        } else {
            self.segments.push(Target { layer, rect: r });
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
        if self.fallback {
            Self::charge(work, 1, cap)?;
            return distance(
                p,
                [
                    p[0].clamp(self.bounds.lx, self.bounds.ux),
                    p[1].clamp(self.bounds.ly, self.bounds.uy),
                ],
            );
        }
        Self::charge(work, self.segments.len().saturating_mul(3), cap)?;
        let best = self
            .segments
            .iter()
            .filter_map(|target| {
                let r = target.rect;
                model.estimate(
                    layer,
                    target.layer,
                    p[0].abs_diff(p[0].clamp(r.lx, r.ux)),
                    p[1].abs_diff(p[1].clamp(r.ly, r.uy)),
                )
            })
            .min();
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
        let mut tree = TreeDistance::new(0, [-5, -5]);
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
    fn segment_cap_falls_back_to_a_lower_bound_without_more_allocation() {
        let mut tree = TreeDistance::new(0, [0, 0]);
        let model = CostModel::new(&[false], &[], 80, 1).unwrap();
        let mut work = 0;
        for i in 1..=MAX_SEGMENTS {
            let n = i as i64 * 2;
            tree.add(0, [n, n], [n, n], &mut work, 100_000_000).unwrap();
        }
        assert!(tree.fallback);
        assert_eq!(tree.segments.capacity(), 0);
        assert_eq!(
            tree.distance(0, [1, 1], &model, &mut work, 100_000_000)
                .unwrap(),
            0
        );
    }
}
