//! Bounded, immutable spatial queries over retained conductor shape IDs.
//!
//! Build only after the retained metal pieces have been added. Virtual cell
//! geometry stays in `InstanceGeometry`; this index never materializes it.
//! Traversal order has no electrical meaning: callers sort the accepted IDs.
use super::{ExtractLimits, Shape, Shapes, budget, invalid};
use crate::{Rect, Result};

const LEAF_SIZE: usize = 8;
const LEAF_FLAG: u32 = 1 << 31;
const NONE: u32 = u32::MAX;
// At most 32 layer partitions, followed by bounded quarter/three-quarter
// spatial partitions. The explicit cap also protects against future changes.
const MAX_DEPTH: usize = 128;
const BUILD_STACK_BYTES: usize = MAX_DEPTH * 1024;

#[derive(Clone, Copy)]
struct Node {
    bounds: Rect,
    layer_lo: u32,
    layer_hi: u32,
    // Leaf: [a, a + (b & !LEAF_FLAG)) in ids. Branch: child indices a and b.
    a: u32,
    b: u32,
}

/// A deterministic BVH whose leaves refer to existing retained shapes.
/// Shape IDs and geometry must remain unchanged between build and queries.
pub(super) struct ShapeIndex {
    ids: Vec<u32>,
    nodes: Vec<Node>,
    layer_count: usize,
}

fn charge(work: &mut usize, limits: &ExtractLimits) -> Result<()> {
    *work = work
        .checked_add(1)
        .filter(|&n| n <= limits.max_work)
        .ok_or_else(|| budget("extraction shape-index work"))?;
    Ok(())
}

// Strict endpoint inequalities deliberately accept a zero-width query when
// a positive-width shape straddles it, matching the extraction predicates.
fn intersects(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect {
        lx: a.lx.min(b.lx),
        ly: a.ly.min(b.ly),
        ux: a.ux.max(b.ux),
        uy: a.uy.max(b.uy),
    }
}

fn center(r: Rect, vertical: bool) -> i128 {
    // Doubled centers avoid both rounding and i64 coordinate overflow.
    if vertical {
        i128::from(r.ly) + i128::from(r.uy)
    } else {
        i128::from(r.lx) + i128::from(r.ux)
    }
}

impl ShapeIndex {
    fn node_bound(shapes: u64, layers: u64) -> u64 {
        if shapes == 0 {
            return 0;
        }
        // A single-layer leaf has >=4 IDs unless it is the layer's only
        // leaf. Splitting layer ranges adds at most layers-1 branches, so
        // leaves <= ceil(shapes/4)+layers, nodes < 2*leaves. Also every
        // binary tree with nonempty leaves has <=2*shapes-1 nodes.
        shapes
            .saturating_mul(2)
            .saturating_sub(1)
            .min(shapes.div_ceil(4).saturating_add(layers).saturating_mul(2))
    }

    /// Conservative additional peak allocation, including the fixed query
    /// stack and Vec headers. Construction partitions in place without sort
    /// buffers. Drop the old retained-shape bins before allocating this index.
    pub(super) fn estimated_bytes(max_shapes: usize, layer_count: usize) -> u64 {
        let n = max_shapes as u64;
        n.saturating_mul(size_of::<u32>() as u64)
            .saturating_add(
                Self::node_bound(n, layer_count as u64).saturating_mul(size_of::<Node>() as u64),
            )
            .saturating_add(
                (BUILD_STACK_BYTES + MAX_DEPTH * size_of::<u32>() + size_of::<Self>()) as u64,
            )
    }

    pub(super) fn build(
        shapes: &Shapes<'_>,
        layer_count: usize,
        limits: &ExtractLimits,
        work: &mut usize,
    ) -> Result<Self> {
        let count = shapes.retained_len();
        if count > limits.max_shapes || layer_count == 0 || layer_count > u32::MAX as usize {
            return Err(budget("extraction shape-index extent"));
        }
        let capacity = usize::try_from(Self::node_bound(count as u64, layer_count as u64))
            .map_err(|_| budget("extraction shape-index node capacity"))?;
        if count >= LEAF_FLAG as usize || capacity >= LEAF_FLAG as usize {
            return Err(budget("extraction shape-index ID capacity"));
        }
        let mut out = Self {
            ids: Vec::new(),
            nodes: Vec::new(),
            layer_count,
        };
        out.ids
            .try_reserve_exact(count)
            .map_err(|_| budget("extraction shape-index ID allocation"))?;
        out.nodes
            .try_reserve_exact(capacity)
            .map_err(|_| budget("extraction shape-index node allocation"))?;
        for (id, shape) in shapes.owned() {
            charge(work, limits)?;
            if shape.layer as usize >= layer_count || !shape.rect.valid() {
                return Err(invalid("invalid retained shape-index geometry"));
            }
            out.ids.push(id);
        }
        if count > 0 {
            out.build_node(shapes, 0, count, 0, capacity, limits, work)?;
        }
        Ok(out)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "recursion shares the immutable shapes, hard limits and charged work counter"
    )]
    fn build_node(
        &mut self,
        shapes: &Shapes<'_>,
        start: usize,
        end: usize,
        depth: usize,
        capacity: usize,
        limits: &ExtractLimits,
        work: &mut usize,
    ) -> Result<u32> {
        if depth >= MAX_DEPTH || self.nodes.len() >= capacity {
            return Err(budget("extraction shape-index tree extent"));
        }
        charge(work, limits)?;
        let first = shapes.get(self.ids[start]);
        let mut bounds = first.rect;
        let (mut layer_lo, mut layer_hi) = (first.layer, first.layer);
        let (mut center_lo, mut center_hi) = (
            [center(first.rect, false), center(first.rect, true)],
            [center(first.rect, false), center(first.rect, true)],
        );
        for &id in &self.ids[start..end] {
            charge(work, limits)?;
            let shape = shapes.get(id);
            bounds = union(bounds, shape.rect);
            layer_lo = layer_lo.min(shape.layer);
            layer_hi = layer_hi.max(shape.layer);
            for axis in 0..2 {
                let c = center(shape.rect, axis == 1);
                center_lo[axis] = center_lo[axis].min(c);
                center_hi[axis] = center_hi[axis].max(c);
            }
        }
        let at = self.nodes.len() as u32;
        self.nodes.push(Node {
            bounds,
            layer_lo,
            layer_hi,
            a: start as u32,
            b: (end - start) as u32 | LEAF_FLAG,
        });
        let count = end - start;
        if count <= LEAF_SIZE {
            return Ok(at);
        }
        let vertical = center_hi[1] - center_lo[1] > center_hi[0] - center_lo[0];
        let axis = usize::from(vertical);
        let pivot = center_lo[axis] + (center_hi[axis] - center_lo[axis]) / 2;
        let layer_pivot = layer_lo + (layer_hi - layer_lo) / 2;
        let mut split = start;
        if layer_lo != layer_hi || center_lo[axis] != center_hi[axis] {
            for i in start..end {
                charge(work, limits)?;
                let shape = shapes.get(self.ids[i]);
                let lower = if layer_lo != layer_hi {
                    shape.layer <= layer_pivot
                } else {
                    center(shape.rect, vertical) <= pivot
                };
                if lower {
                    self.ids.swap(i, split);
                    split += 1;
                }
            }
        }
        let minimum_partition = (count / 4).max(4);
        if layer_lo == layer_hi
            && (split - start < minimum_partition || end - split < minimum_partition)
        {
            // An outlier or coincident centers must not make a linear chain.
            split = start + count / 2;
        }
        if split == start || split == end {
            return Err(invalid("empty shape-index partition"));
        }
        let left = self.build_node(shapes, start, split, depth + 1, capacity, limits, work)?;
        let right = self.build_node(shapes, split, end, depth + 1, capacity, limits, work)?;
        self.nodes[at as usize].a = left;
        self.nodes[at as usize].b = right;
        Ok(at)
    }

    pub(super) fn visit(
        &self,
        layer: u32,
        query: Rect,
        shapes: &Shapes<'_>,
        limits: &ExtractLimits,
        work: &mut usize,
        mut visitor: impl FnMut(u32, Shape) -> Result<()>,
    ) -> Result<()> {
        if layer as usize >= self.layer_count || query.lx > query.ux || query.ly > query.uy {
            return Err(invalid("invalid shape-index query"));
        }
        if self.nodes.is_empty() {
            return Ok(());
        }
        let mut stack = [NONE; MAX_DEPTH];
        stack[0] = 0;
        let mut pending = 1;
        while pending > 0 {
            pending -= 1;
            let node = self.nodes[stack[pending] as usize];
            charge(work, limits)?;
            if layer < node.layer_lo || layer > node.layer_hi || !intersects(node.bounds, query) {
                continue;
            }
            if node.b & LEAF_FLAG != 0 {
                let start = node.a as usize;
                let end = start + (node.b & !LEAF_FLAG) as usize;
                for &id in &self.ids[start..end] {
                    charge(work, limits)?;
                    let shape = shapes.get(id);
                    if shape.layer == layer && intersects(shape.rect, query) {
                        visitor(id, shape)?;
                    }
                }
            } else {
                if pending + 2 > stack.len() {
                    return Err(budget("extraction shape-index query depth"));
                }
                stack[pending] = node.b;
                stack[pending + 1] = node.a;
                pending += 2;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Owner, PassiveOwner};

    fn shapes(rects: impl IntoIterator<Item = (u32, Rect)>) -> Shapes<'static> {
        Shapes::new(
            rects
                .into_iter()
                .map(|(layer, rect)| Shape {
                    layer,
                    rect,
                    owner: Owner::Passive(PassiveOwner::Ground),
                })
                .collect(),
            0,
            None,
            &ExtractLimits::default(),
        )
        .unwrap()
    }

    fn check(shapes: &Shapes<'_>, layers: usize, queries: &[(u32, Rect)]) -> ShapeIndex {
        let limits = ExtractLimits::default();
        let mut work = 0;
        let index = ShapeIndex::build(shapes, layers, &limits, &mut work).unwrap();
        let mut repeated_work = 0;
        let repeated = ShapeIndex::build(shapes, layers, &limits, &mut repeated_work).unwrap();
        assert_eq!(work, repeated_work);
        assert_eq!(index.ids, repeated.ids);
        assert!(
            index.nodes.len()
                <= ShapeIndex::node_bound(shapes.retained_len() as u64, layers as u64) as usize
        );
        for &(layer, query) in queries {
            // Independent oracle: direct endpoint comparisons, no BVH helper.
            let expected: Vec<u32> = shapes
                .owned()
                .filter(|(_, s)| {
                    s.layer == layer
                        && s.rect.ux > query.lx
                        && s.rect.lx < query.ux
                        && s.rect.uy > query.ly
                        && s.rect.ly < query.uy
                })
                .map(|(id, _)| id)
                .collect();
            let mut actual = Vec::new();
            index
                .visit(layer, query, shapes, &limits, &mut work, |id, _| {
                    actual.push(id);
                    Ok(())
                })
                .unwrap();
            actual.sort_unstable();
            assert_eq!(actual, expected, "layer {layer}, query {query:?}");
        }
        index
    }

    #[test]
    fn adversarial_boxes_match_brute_force_with_strict_boundaries() {
        let mut input = Vec::new();
        for i in 0..513i64 {
            // Long rails, clustered rectangles, equal centroids, diagonal
            // slivers and negative coordinates all coexist in each layer.
            let rect = match i % 5 {
                0 => Rect {
                    lx: -1_000_000,
                    ly: i - 260,
                    ux: 1_000_000,
                    uy: i - 259,
                },
                1 => Rect {
                    lx: i % 11 - 5,
                    ly: i % 7 - 3,
                    ux: i % 11 - 3,
                    uy: i % 7,
                },
                2 => Rect {
                    lx: -10,
                    ly: -10,
                    ux: 10,
                    uy: 10,
                },
                3 => Rect {
                    lx: i * 97,
                    ly: i * 83,
                    ux: i * 97 + 1,
                    uy: i * 83 + 1,
                },
                _ => Rect {
                    lx: i64::MAX - 1024,
                    ly: i64::MIN + 1,
                    ux: i64::MAX,
                    uy: i64::MIN + 1024,
                },
            };
            input.push(((i % 7) as u32, rect));
        }
        let shapes = shapes(input);
        let mut queries = Vec::new();
        for layer in 0..8 {
            for i in -32..32i64 {
                queries.push((
                    layer,
                    Rect {
                        lx: i * 5,
                        ly: i * 7,
                        ux: i * 5 + 10,
                        uy: i * 7 + 3,
                    },
                ));
            }
            for rect in [
                Rect {
                    lx: -10,
                    ly: -10,
                    ux: 10,
                    uy: 10,
                },
                Rect {
                    lx: 10,
                    ly: -10,
                    ux: 11,
                    uy: 10,
                },
                Rect {
                    lx: 0,
                    ly: -5,
                    ux: 0,
                    uy: 5,
                },
                Rect {
                    lx: -5,
                    ly: 0,
                    ux: 5,
                    uy: 0,
                },
                Rect {
                    lx: i64::MIN,
                    ly: i64::MIN,
                    ux: i64::MAX,
                    uy: i64::MAX,
                },
            ] {
                queries.push((layer, rect));
            }
        }
        check(&shapes, 8, &queries);
    }

    #[test]
    fn all_small_counts_and_skewed_layers_respect_allocation_bound() {
        for count in 0..257 {
            let shapes = shapes((0..count).map(|i| {
                let x = if i % 13 == 0 { i64::MAX - 2 } else { i as i64 };
                let layer = if i < 7 { i as u32 } else { 7 };
                (
                    layer,
                    Rect {
                        lx: x,
                        ly: -1,
                        ux: x + 1,
                        uy: 1,
                    },
                )
            }));
            let index = check(&shapes, 8, &[]);
            let allocated = index.ids.capacity() * size_of::<u32>()
                + index.nodes.capacity() * size_of::<Node>()
                + BUILD_STACK_BYTES
                + MAX_DEPTH * size_of::<u32>()
                + size_of::<ShapeIndex>();
            assert!(allocated as u64 <= ShapeIndex::estimated_bytes(count, 8));
        }
        assert_eq!(size_of::<Node>(), 48);
    }

    #[test]
    fn build_and_query_budgets_fail_without_visiting_unbounded_candidates() {
        let shapes = shapes((0..256).map(|i| {
            (
                0,
                Rect {
                    lx: i,
                    ly: 0,
                    ux: i + 1,
                    uy: 1,
                },
            )
        }));
        let mut limits = ExtractLimits {
            max_work: 1,
            ..Default::default()
        };
        assert!(ShapeIndex::build(&shapes, 1, &limits, &mut 0).is_err());
        limits.max_work = 100_000;
        let mut work = 0;
        let index = ShapeIndex::build(&shapes, 1, &limits, &mut work).unwrap();
        limits.max_work = work + 3;
        let mut visits = 0;
        assert!(
            index
                .visit(
                    0,
                    Rect {
                        lx: -1,
                        ly: -1,
                        ux: 300,
                        uy: 2
                    },
                    &shapes,
                    &limits,
                    &mut work,
                    |_, _| {
                        visits += 1;
                        Ok(())
                    }
                )
                .is_err()
        );
        assert!(visits <= 3);
        limits.max_shapes = 255;
        assert!(ShapeIndex::build(&shapes, 1, &limits, &mut 0).is_err());
    }

    #[test]
    fn malformed_geometry_queries_and_visitor_failures_propagate() {
        let bad = shapes([(
            1,
            Rect {
                lx: 0,
                ly: 0,
                ux: 1,
                uy: 1,
            },
        )]);
        let limits = ExtractLimits::default();
        assert!(ShapeIndex::build(&bad, 1, &limits, &mut 0).is_err());
        let bad = shapes([(
            0,
            Rect {
                lx: 1,
                ly: 0,
                ux: 0,
                uy: 1,
            },
        )]);
        assert!(ShapeIndex::build(&bad, 1, &limits, &mut 0).is_err());
        let shapes = shapes([(
            0,
            Rect {
                lx: -1,
                ly: -1,
                ux: 1,
                uy: 1,
            },
        )]);
        let index = ShapeIndex::build(&shapes, 1, &limits, &mut 0).unwrap();
        let query = Rect {
            lx: -2,
            ly: -2,
            ux: 2,
            uy: 2,
        };
        assert!(
            index
                .visit(1, query, &shapes, &limits, &mut 0, |_, _| Ok(()))
                .is_err()
        );
        assert!(
            index
                .visit(0, query, &shapes, &limits, &mut 0, |_, _| Err(budget(
                    "visitor control"
                )))
                .is_err()
        );
        assert!(
            index
                .visit(
                    0,
                    Rect { ux: -3, ..query },
                    &shapes,
                    &limits,
                    &mut 0,
                    |_, _| Ok(())
                )
                .is_err()
        );
    }

    #[test]
    fn local_queries_do_not_scan_the_whole_retained_shape_universe() {
        // Peak fixture allocation: 16,384 shapes (<1 MiB) plus <0.6 MiB
        // of BVH buffers and two test workers; no materialized graph paths.
        let shapes = shapes((0..16_384i64).map(|i| {
            let x = i % 128 * 100;
            let y = i / 128 * 100;
            (
                0,
                Rect {
                    lx: x,
                    ly: y,
                    ux: x + 1,
                    uy: y + 1,
                },
            )
        }));
        let limits = ExtractLimits::default();
        let index = ShapeIndex::build(&shapes, 1, &limits, &mut 0).unwrap();
        let mut work = 0;
        let mut found = Vec::new();
        index
            .visit(
                0,
                Rect {
                    lx: 6399,
                    ly: 6399,
                    ux: 6402,
                    uy: 6402,
                },
                &shapes,
                &limits,
                &mut work,
                |id, _| {
                    found.push(id);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(found, [64 * 128 + 64]);
        assert!(work < 128, "local query charged {work} probes");
    }

    #[test]
    fn virtual_shape_ids_are_skipped_without_materializing_their_geometry() {
        use crate::extract::InstancedPassive;
        use crate::instance_obstacles::{
            InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
        };
        let rect = Rect {
            lx: -1,
            ly: -1,
            ux: 1,
            uy: 1,
        };
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(
                    10,
                    0,
                    vec![TemplateObstacle {
                        layer: 0,
                        rect,
                        pin: None,
                    }],
                )
                .unwrap(),
            ],
            vec![ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [0, 0],
                orientation: crate::Orientation::FS,
            }],
            vec![],
        )
        .unwrap()
        .with_pin_ids(vec![])
        .unwrap();
        let limits = ExtractLimits::default();
        let shape = Shape {
            layer: 0,
            rect,
            owner: Owner::Passive(PassiveOwner::Ground),
        };
        let mut shapes = Shapes::new(
            vec![shape, shape],
            1,
            Some(InstancedPassive {
                geometry: &source,
                signal_nets: &[],
            }),
            &limits,
        )
        .unwrap();
        shapes.push(shape);
        assert_eq!(shapes.virtual_count(), 1);
        let index = check(&shapes, 1, &[(0, rect)]);
        assert_eq!(index.ids, [0, 2, 3]);
    }
}
