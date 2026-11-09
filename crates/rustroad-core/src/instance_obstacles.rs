//! Shared immutable cell geometry for large routing inputs.
use crate::routing::{Blockage, RouteLimits, budget, invalid};
use crate::{Orientation, Rect, Result, transform_rect};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::sync::Arc;

const MAX_INSTANCES: usize = crate::MAX_PHYSICAL_INSTANCES;
const MAX_PINS: usize = 256_000_000;
const NO_NET: u32 = u32::MAX;

#[derive(Clone, Debug, Serialize)]
pub struct TemplateObstacle {
    pub layer: u32,
    pub rect: Rect,
    /// A pin ordinal, or None for an obstruction independent of connectivity.
    pub pin: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ObstacleTemplate {
    height: i64,
    pin_count: u32,
    bounds: Option<Rect>,
    layers: BTreeMap<u32, Vec<(usize, TemplateObstacle)>>,
    order: Vec<(u32, usize)>,
    shape_count: usize,
}
impl ObstacleTemplate {
    pub fn new(height: i64, pin_count: u32, shapes: Vec<TemplateObstacle>) -> Result<Self> {
        if height <= 0 || shapes.len() > 4_000_000 {
            return Err(budget("cell obstacle template size"));
        }
        let shape_count = shapes.len();
        let mut bounds: Option<Rect> = None;
        let mut layers: BTreeMap<u32, Vec<(usize, TemplateObstacle)>> = BTreeMap::new();
        let mut order = Vec::with_capacity(shape_count);
        for (ordinal, shape) in shapes.into_iter().enumerate() {
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
            let layer = layers.entry(shape.layer).or_default();
            order.push((shape.layer, layer.len()));
            layer.push((ordinal, shape));
        }
        Ok(Self {
            height,
            pin_count,
            bounds,
            layers,
            shape_count,
            order,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ObstacleInstance {
    pub template: u32,
    pub pin_start: u32,
    pub at: [i64; 2],
    pub orientation: Orientation,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstanceObstacles {
    templates: Arc<[ObstacleTemplate]>,
    instances: Arc<[ObstacleInstance]>,
    pin_nets: Arc<[u32]>,
    /// Optional logical pin identities for extraction. Routing needs only nets.
    pin_ids: Option<Arc<[u32]>>,
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
            virtual_shapes = virtual_shapes
                .checked_add(master.shape_count as u64)
                .ok_or_else(|| budget("virtual obstacle count"))?;
        }
        let layers = templates
            .iter()
            .flat_map(|t| t.layers.keys().copied())
            .collect();
        Ok(Self {
            templates: templates.into(),
            instances: instances.into(),
            pin_nets: pin_nets.into(),
            pin_ids: None,
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
    pub fn with_pin_ids(mut self, pins: Vec<u32>) -> Result<Self> {
        if pins.len() != self.pin_nets.len() || pins.iter().any(|&p| p as usize >= MAX_PINS) {
            return Err(invalid("instance logical-pin identity column"));
        }
        self.pin_ids = Some(pins.into());
        Ok(self)
    }
    pub fn has_pin_ids(&self) -> bool {
        self.pin_ids.is_some()
    }
    pub fn shape_count(&self, instance: usize) -> Option<usize> {
        self.instances
            .get(instance)
            .map(|i| self.templates[i.template as usize].shape_count)
    }
    /// One rectangle in original template order, with its global pin identity
    /// when the extraction column is present. No other instance is expanded.
    pub fn shape(&self, instance: usize, ordinal: usize) -> Result<(Blockage, Option<u32>)> {
        let cell = self
            .instances
            .get(instance)
            .ok_or_else(|| invalid("instance shape owner"))?;
        let master = &self.templates[cell.template as usize];
        let &(layer, local) = master
            .order
            .get(ordinal)
            .ok_or_else(|| invalid("instance shape ordinal"))?;
        let shape = &master.layers[&layer][local].1;
        let pin = shape.pin.map(|p| (cell.pin_start + p) as usize);
        let net = pin.map(|p| self.pin_nets[p]).filter(|&n| n != NO_NET);
        let owner = pin.and_then(|p| self.pin_ids.as_ref().map(|ids| ids[p]));
        Ok((
            Blockage {
                layer,
                rect: transform_rect(
                    shape.rect,
                    cell.at[0],
                    cell.at[1],
                    master.height,
                    cell.orientation,
                )?,
                net,
            },
            owner,
        ))
    }
    /// Visit one layer with the same ordinals as `shape`. Ordinals remain
    /// stable when another instance queries the shared master template.
    pub fn visit_indexed(
        &self,
        instance: usize,
        layer: u32,
        mut visitor: impl FnMut(usize, Blockage, Option<u32>) -> Result<bool>,
    ) -> Result<bool> {
        let cell = self
            .instances
            .get(instance)
            .ok_or_else(|| invalid("instance shape owner"))?;
        let master = &self.templates[cell.template as usize];
        if let Some(shapes) = master.layers.get(&layer) {
            for &(ordinal, _) in shapes {
                let (shape, pin) = self.shape(instance, ordinal)?;
                if !visitor(ordinal, shape, pin)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    /// Same shape order as `visit_indexed`, but test against an exact local
    /// query before doing world-coordinate transforms. `probe` charges each
    /// comparison, including rejected rectangles.
    pub fn visit_near(
        &self,
        instance: usize,
        layer: u32,
        query: Rect,
        mut probe: impl FnMut() -> Result<()>,
        mut visitor: impl FnMut(usize, Blockage, Option<u32>) -> Result<bool>,
    ) -> Result<bool> {
        if !query.valid() {
            return Err(invalid("invalid instance query rectangle"));
        }
        let cell = self
            .instances
            .get(instance)
            .ok_or_else(|| invalid("instance query owner"))?;
        let master = &self.templates[cell.template as usize];
        let (lx, ux) = (
            i128::from(query.lx) - i128::from(cell.at[0]),
            i128::from(query.ux) - i128::from(cell.at[0]),
        );
        let (ly, uy) = match cell.orientation {
            Orientation::N => (
                i128::from(query.ly) - i128::from(cell.at[1]),
                i128::from(query.uy) - i128::from(cell.at[1]),
            ),
            Orientation::FS => (
                i128::from(master.height) + i128::from(cell.at[1]) - i128::from(query.uy),
                i128::from(master.height) + i128::from(cell.at[1]) - i128::from(query.ly),
            ),
        };
        if let Some(shapes) = master.layers.get(&layer) {
            for &(ordinal, ref shape) in shapes {
                probe()?;
                let r = shape.rect;
                if i128::from(r.lx) < ux
                    && lx < i128::from(r.ux)
                    && i128::from(r.ly) < uy
                    && ly < i128::from(r.uy)
                {
                    let (shape, pin) = self.shape(instance, ordinal)?;
                    if !visitor(ordinal, shape, pin)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
    pub fn heap_bytes(&self) -> u64 {
        let shapes: usize = self
            .templates
            .iter()
            .flat_map(|t| t.layers.values())
            .map(|v| v.capacity() * size_of::<(usize, TemplateObstacle)>())
            .sum();
        (self.instances.len() * size_of::<ObstacleInstance>()
            + self.pin_nets.len() * 4
            + self.pin_ids.as_ref().map_or(0, |v| v.len() * 4)
            + shapes
            + self
                .templates
                .iter()
                .map(|t| t.order.capacity() * size_of::<(u32, usize)>())
                .sum::<usize>()
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
    pub(crate) fn layer_count(&self) -> usize {
        self.layers.len()
    }
    pub(crate) fn instance_layers(&self, instance: usize) -> impl Iterator<Item = u32> + '_ {
        self.templates[self.instances[instance].template as usize]
            .layers
            .keys()
            .copied()
    }
    pub(crate) fn bounds(&self, index: usize) -> Result<Option<Rect>> {
        let cell = self.instances[index];
        let t = &self.templates[cell.template as usize];
        t.bounds
            .map(|b| transform_rect(b, cell.at[0], cell.at[1], t.height, cell.orientation))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexed_shapes_keep_pin_identity_and_transformed_integer_geometry() {
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(
                    6,
                    1,
                    vec![
                        TemplateObstacle {
                            layer: 0,
                            rect: Rect {
                                lx: 1,
                                ly: 1,
                                ux: 3,
                                uy: 2,
                            },
                            pin: Some(0),
                        },
                        TemplateObstacle {
                            layer: 2,
                            rect: Rect {
                                lx: 0,
                                ly: 3,
                                ux: 4,
                                uy: 4,
                            },
                            pin: None,
                        },
                    ],
                )
                .unwrap(),
            ],
            vec![
                ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [-10, -20],
                    orientation: Orientation::N,
                },
                ObstacleInstance {
                    template: 0,
                    pin_start: 1,
                    at: [20, 30],
                    orientation: Orientation::FS,
                },
            ],
            vec![3, 8],
        )
        .unwrap();
        assert!(!source.has_pin_ids());
        assert!(source.clone().with_pin_ids(vec![99]).is_err());
        let source = source.with_pin_ids(vec![99, 102]).unwrap();
        assert_eq!(source.shape_count(0), Some(2));
        assert_eq!(source.shape_count(2), None);
        let (normal, owner) = source.shape(0, 0).unwrap();
        assert_eq!(
            normal.rect,
            Rect {
                lx: -9,
                ly: -19,
                ux: -7,
                uy: -18
            }
        );
        assert_eq!((normal.net, owner), (Some(3), Some(99)));
        let (flipped, owner) = source.shape(1, 0).unwrap();
        assert_eq!(
            flipped.rect,
            Rect {
                lx: 21,
                ly: 34,
                ux: 23,
                uy: 35
            }
        );
        assert_eq!((flipped.net, owner), (Some(8), Some(102)));
        let (obstruction, owner) = source.shape(1, 1).unwrap();
        assert_eq!(
            obstruction.rect,
            Rect {
                lx: 20,
                ly: 32,
                ux: 24,
                uy: 33
            }
        );
        assert_eq!((obstruction.net, owner), (None, None));
        assert!(source.shape(1, 2).is_err());
        assert!(source.shape(2, 0).is_err());
        for instance in 0..2 {
            for layer in [0, 2] {
                for query in [
                    Rect {
                        lx: -10,
                        ly: -20,
                        ux: -6,
                        uy: -17,
                    },
                    Rect {
                        lx: 20,
                        ly: 33,
                        ux: 24,
                        uy: 36,
                    },
                    Rect {
                        lx: 100,
                        ly: 100,
                        ux: 102,
                        uy: 102,
                    },
                ] {
                    let mut expected = Vec::new();
                    for ordinal in 0..source.shape_count(instance).unwrap() {
                        let (shape, owner) = source.shape(instance, ordinal).unwrap();
                        let r = shape.rect;
                        if shape.layer == layer
                            && r.lx < query.ux
                            && query.lx < r.ux
                            && r.ly < query.uy
                            && query.ly < r.uy
                        {
                            expected.push((ordinal, r, shape.net, owner));
                        }
                    }
                    let mut actual = Vec::new();
                    let mut work = 0;
                    source
                        .visit_near(
                            instance,
                            layer,
                            query,
                            || {
                                work += 1;
                                Ok(())
                            },
                            |i, shape, owner| {
                                actual.push((i, shape.rect, shape.net, owner));
                                Ok(true)
                            },
                        )
                        .unwrap();
                    assert_eq!(actual, expected);
                    assert_eq!(work, 1);
                }
            }
        }
    }
}
