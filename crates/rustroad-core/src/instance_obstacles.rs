//! Shared immutable cell geometry for large routing inputs.
use crate::routing::{Blockage, RouteLimits, budget, invalid};
use crate::{Orientation, Rect, Result, transform_rect};
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

const MAX_INSTANCES: usize = 16_000_000;
const MAX_PINS: usize = 256_000_000;
const NO_NET: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct TemplateObstacle {
    pub layer: u32,
    pub rect: Rect,
    /// A pin ordinal, or None for an obstruction independent of connectivity.
    pub pin: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct ObstacleTemplate {
    height: i64,
    pin_count: u32,
    bounds: Option<Rect>,
    layers: BTreeMap<u32, Vec<TemplateObstacle>>,
}
impl ObstacleTemplate {
    pub fn new(height: i64, pin_count: u32, shapes: Vec<TemplateObstacle>) -> Result<Self> {
        if height <= 0 || shapes.len() > 4_000_000 {
            return Err(budget("cell obstacle template size"));
        }
        let mut bounds: Option<Rect> = None;
        let mut layers: BTreeMap<u32, Vec<TemplateObstacle>> = BTreeMap::new();
        for shape in shapes {
            if !shape.rect.valid() || shape.pin.is_some_and(|p| p >= pin_count) {
                return Err(invalid("cell obstacle template geometry/pin"));
            }
            bounds = Some(match bounds {
                None => shape.rect,
                Some(b) => Rect {
                    lx: b.lx.min(shape.rect.lx),
                    ly: b.ly.min(shape.rect.ly),
                    ux: b.ux.max(shape.rect.ux),
                    uy: b.uy.max(shape.rect.uy),
                },
            });
            layers.entry(shape.layer).or_default().push(shape);
        }
        Ok(Self {
            height,
            pin_count,
            bounds,
            layers,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ObstacleInstance {
    pub template: u32,
    pub pin_start: u32,
    pub at: [i64; 2],
    pub orientation: Orientation,
}

#[derive(Clone, Debug)]
pub struct InstanceObstacles {
    templates: Vec<ObstacleTemplate>,
    instances: Vec<ObstacleInstance>,
    pin_nets: Vec<u32>,
    virtual_shapes: u64,
    layers: BTreeSet<u32>,
}
impl InstanceObstacles {
    pub fn new(
        templates: Vec<ObstacleTemplate>,
        instances: Vec<ObstacleInstance>,
        pin_nets: Vec<u32>,
    ) -> Result<Self> {
        if templates.len() > 65_536 || instances.len() > MAX_INSTANCES || pin_nets.len() > MAX_PINS
        {
            return Err(budget("instanced obstacle table counts"));
        }
        let mut virtual_shapes = 0u64;
        for cell in &instances {
            let master = templates
                .get(cell.template as usize)
                .ok_or_else(|| invalid("obstacle template index"))?;
            if cell.pin_start as usize + master.pin_count as usize > pin_nets.len() {
                return Err(invalid("obstacle pin range"));
            }
            let shapes: usize = master.layers.values().map(Vec::len).sum();
            virtual_shapes = virtual_shapes
                .checked_add(shapes as u64)
                .ok_or_else(|| budget("virtual obstacle count"))?;
        }
        let layers = templates
            .iter()
            .flat_map(|t| t.layers.keys().copied())
            .collect();
        Ok(Self {
            templates,
            instances,
            pin_nets,
            virtual_shapes,
            layers,
        })
    }

    pub fn len(&self) -> usize {
        self.instances.len()
    }
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }
    pub fn virtual_shapes(&self) -> u64 {
        self.virtual_shapes
    }
    pub fn heap_bytes(&self) -> u64 {
        let shapes: usize = self
            .templates
            .iter()
            .flat_map(|t| t.layers.values())
            .map(|v| v.capacity() * size_of::<TemplateObstacle>())
            .sum();
        (self.instances.capacity() * size_of::<ObstacleInstance>()
            + self.pin_nets.capacity() * 4
            + shapes
            + self.templates.len() * 4096) as u64
    }
    pub(crate) fn validate(&self, layer_count: usize, limits: &RouteLimits) -> Result<()> {
        let count: usize = self
            .templates
            .iter()
            .flat_map(|t| t.layers.values())
            .map(Vec::len)
            .sum();
        if count > limits.max_obstacles
            || self
                .templates
                .iter()
                .any(|t| t.layers.keys().any(|&l| l as usize >= layer_count))
        {
            return Err(budget("instanced obstacle geometry budget/layer"));
        }
        Ok(())
    }
    pub(crate) fn has_layer(&self, layer: u32) -> bool {
        self.layers.contains(&layer)
    }
    pub(crate) fn bounds(&self, index: usize) -> Result<Option<Rect>> {
        let cell = self.instances[index];
        let t = &self.templates[cell.template as usize];
        t.bounds
            .map(|b| transform_rect(b, cell.at[0], cell.at[1], t.height, cell.orientation))
            .transpose()
    }
    pub(crate) fn visit(
        &self,
        index: usize,
        layer: u32,
        mut visitor: impl FnMut(Blockage) -> Result<bool>,
    ) -> Result<bool> {
        let cell = self.instances[index];
        let t = &self.templates[cell.template as usize];
        if let Some(shapes) = t.layers.get(&layer) {
            for shape in shapes {
                let net = shape
                    .pin
                    .map(|pin| self.pin_nets[(cell.pin_start + pin) as usize])
                    .filter(|&n| n != NO_NET);
                let rect = transform_rect(
                    shape.rect,
                    cell.at[0],
                    cell.at[1],
                    t.height,
                    cell.orientation,
                )?;
                if !visitor(Blockage { layer, rect, net })? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
