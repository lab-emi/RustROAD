//! Shared, indexed passive cell geometry for extraction. The numeric route
//! shapes stay owned; the far larger cell shape universe stays virtual.
use super::{ExtractLimits, InstancedPassive, Owner, PassiveOwner, Shape};
use crate::routing::{InstanceIndex, InstanceQueryScratch, RouteLimits};
use crate::{Rect, Result};

pub(super) struct Shapes<'a> {
    values: Vec<Shape>,
    front: usize,
    pub(super) source: Option<InstancedPassive<'a>>,
    first: Vec<u32>,
    virtual_count: usize,
}
impl<'a> Shapes<'a> {
    pub(super) fn new(
        values: Vec<Shape>,
        front: usize,
        source: Option<InstancedPassive<'a>>,
        limits: &ExtractLimits,
    ) -> Result<Self> {
        let mut first = Vec::new();
        let mut count = 0usize;
        if let Some(input) = source {
            if !input.geometry.has_pin_ids() {
                return Err(super::invalid(
                    "instance geometry lacks logical pin identities",
                ));
            }
            first
                .try_reserve_exact(input.geometry.len() + 1)
                .map_err(|_| super::budget("instance shape prefix"))?;
            first.push(0);
            for i in 0..input.geometry.len() {
                // Every constituent rectangle is inside the transformed bounds;
                // reject coordinate overflow before exposing infallible lookup.
                input.geometry.bounds(i)?;
                count = count
                    .checked_add(input.geometry.shape_count(i).unwrap())
                    .filter(|&n| n <= limits.max_virtual_shapes)
                    .ok_or_else(|| super::budget("virtual extraction shapes"))?;
                first.push(
                    u32::try_from(count)
                        .map_err(|_| super::budget("virtual extraction shape ids"))?,
                );
            }
        }
        if count.saturating_add(limits.max_shapes) >= u32::MAX as usize {
            return Err(super::budget("extraction shape-id capacity"));
        }
        Ok(Self {
            values,
            front,
            source,
            first,
            virtual_count: count,
        })
    }
    pub(super) fn len(&self) -> usize {
        self.values.len() + self.virtual_count
    }
    pub(super) fn retained_len(&self) -> usize {
        self.values.len()
    }
    pub(super) fn push(&mut self, shape: Shape) {
        self.values.push(shape);
    }
    pub(super) fn virtual_count(&self) -> usize {
        self.virtual_count
    }
    pub(super) fn virtual_offset(&self) -> usize {
        self.front
    }
    pub(super) fn virtual_id(&self, instance: usize, ordinal: usize) -> u32 {
        (self.front + self.first[instance] as usize + ordinal) as u32
    }
    pub(super) fn owned(&self) -> impl Iterator<Item = (u32, &Shape)> {
        self.values.iter().enumerate().map(|(i, s)| {
            (
                (if i < self.front {
                    i
                } else {
                    i + self.virtual_count
                }) as u32,
                s,
            )
        })
    }
    pub(super) fn passive_shape(
        input: InstancedPassive<'_>,
        blockage: crate::routing::Blockage,
        pin: Option<u32>,
    ) -> Shape {
        let owner = match (blockage.net, pin) {
            (Some(net), Some(pin)) if input.signal_nets.get(net as usize) == Some(&true) => {
                PassiveOwner::Pin { net, pin }
            }
            _ => PassiveOwner::Ground,
        };
        Shape {
            layer: blockage.layer,
            rect: blockage.rect,
            owner: Owner::Passive(owner),
        }
    }
    pub(super) fn get(&self, id: u32) -> Shape {
        let id = id as usize;
        if id < self.front {
            return self.values[id];
        }
        let offset = id - self.front;
        if offset >= self.virtual_count {
            return self.values[id - self.virtual_count];
        }
        let instance = self.first.partition_point(|&p| p as usize <= offset) - 1;
        let ordinal = offset - self.first[instance] as usize;
        let input = self.source.expect("virtual shape has a source");
        let (shape, pin) = input
            .geometry
            .shape(instance, ordinal)
            .expect("validated virtual shape geometry");
        Self::passive_shape(input, shape, pin)
    }
}

pub(super) struct InstanceGeometry<'a> {
    index: InstanceIndex<'a>,
    limits: RouteLimits,
}
impl<'a> InstanceGeometry<'a> {
    pub(super) fn new(
        shapes: &Shapes<'a>,
        bin: i64,
        layer_count: usize,
        limits: &ExtractLimits,
    ) -> Result<Option<Self>> {
        let Some(input) = shapes.source else {
            return Ok(None);
        };
        let Some((die, bounds)) = Self::parameters(input, limits)? else {
            return Ok(None);
        };
        let index = InstanceIndex::for_geometry(input.geometry, die, bin, layer_count, &bounds)?;
        Ok(Some(Self {
            index,
            limits: bounds,
        }))
    }
    fn parameters(
        input: InstancedPassive<'_>,
        limits: &ExtractLimits,
    ) -> Result<Option<(Rect, RouteLimits)>> {
        let mut die = None;
        for i in 0..input.geometry.len() {
            if let Some(r) = input.geometry.bounds(i)? {
                die = Some(die.map_or(r, |b: Rect| Rect {
                    lx: b.lx.min(r.lx),
                    ly: b.ly.min(r.ly),
                    ux: b.ux.max(r.ux),
                    uy: b.uy.max(r.uy),
                }));
            }
        }
        let Some(die) = die else { return Ok(None) };
        let bounds = RouteLimits {
            max_obstacles: crate::MAX_PHYSICAL_INSTANCES,
            max_bin_entries: limits.max_instance_bin_entries,
            max_comparisons: limits.max_work,
            ..Default::default()
        };
        Ok(Some((die, bounds)))
    }
    pub(super) fn estimated_bytes(
        input: Option<InstancedPassive<'_>>,
        bin: i64,
        layers: usize,
        limits: &ExtractLimits,
    ) -> Result<u64> {
        let Some(input) = input else { return Ok(0) };
        let Some((die, bounds)) = Self::parameters(input, limits)? else {
            return Ok(0);
        };
        Ok(
            crate::routing::instance_geometry_bytes(input.geometry, die, bin, layers, &bounds)?
                .saturating_add((input.geometry.len() as u64 + 1).saturating_mul(4))
                .saturating_add(
                    input
                        .geometry
                        .virtual_shapes()
                        .div_ceil(64)
                        .saturating_mul(8),
                ),
        )
    }
    pub(super) fn visit(
        &mut self,
        shapes: &Shapes<'_>,
        layer: u32,
        query: Rect,
        work: &mut usize,
        mut visitor: impl FnMut(u32, Shape) -> Result<bool>,
    ) -> Result<bool> {
        let input = shapes.source.expect("indexed geometry has a source");
        let query = Self::expanded_query(query)?;
        self.index
            .visit_cells(layer, query, work, &self.limits, |instance, work| {
                input.geometry.visit_near(
                    instance,
                    layer,
                    query,
                    || {
                        *work = work
                            .checked_add(1)
                            .filter(|&n| n <= self.limits.max_comparisons)
                            .ok_or_else(|| super::budget("instance extraction work"))?;
                        Ok(())
                    },
                    |ordinal, blockage, pin| {
                        visitor(
                            shapes.virtual_id(instance, ordinal),
                            Shapes::passive_shape(input, blockage, pin),
                        )
                    },
                )
            })
    }
    fn expanded_query(query: Rect) -> Result<Rect> {
        Ok(Rect {
            lx: query
                .lx
                .checked_sub(1)
                .ok_or_else(|| super::budget("instance query coordinate"))?,
            ly: query
                .ly
                .checked_sub(1)
                .ok_or_else(|| super::budget("instance query coordinate"))?,
            ux: query
                .ux
                .checked_add(1)
                .ok_or_else(|| super::budget("instance query coordinate"))?,
            uy: query
                .uy
                .checked_add(1)
                .ok_or_else(|| super::budget("instance query coordinate"))?,
        })
    }
    pub(super) fn query_work_bound(
        &self,
        shapes: &Shapes<'_>,
        layer: u32,
        query: Rect,
    ) -> Option<usize> {
        self.index.query_work_bound(
            layer,
            Self::expanded_query(query).ok()?,
            shapes.virtual_count(),
        )
    }
    pub(super) fn visit_shared(
        &self,
        scratch: &mut InstanceQueryScratch,
        shapes: &Shapes<'_>,
        layer: u32,
        query: Rect,
        work: &mut usize,
        mut visitor: impl FnMut(u32, Shape) -> Result<bool>,
    ) -> Result<bool> {
        let input = shapes.source.expect("indexed geometry has a source");
        let query = Self::expanded_query(query)?;
        self.index.visit_cells_with_scratch(
            scratch,
            layer,
            query,
            work,
            &self.limits,
            |instance, work| {
                input.geometry.visit_near(
                    instance,
                    layer,
                    query,
                    || {
                        *work = work
                            .checked_add(1)
                            .filter(|&n| n <= self.limits.max_comparisons)
                            .ok_or_else(|| super::budget("instance extraction work"))?;
                        Ok(())
                    },
                    |ordinal, blockage, pin| {
                        visitor(
                            shapes.virtual_id(instance, ordinal),
                            Shapes::passive_shape(input, blockage, pin),
                        )
                    },
                )
            },
        )
    }
}
