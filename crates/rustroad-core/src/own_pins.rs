//! Bounded same-net pin-metal overlap queries for detailed routing.
use crate::routing::OwnPinIndex;
use crate::routing::{budget, invalid, overlap};
use crate::{Rect, Result};

type PinShape = (u32, Rect, [i64; 2]);

pub(crate) struct OwnPins {
    shapes: Vec<PinShape>,
    order: Vec<u32>,
    widths: Vec<i64>,
    ranges: Option<Vec<LayerRange>>,
}
#[derive(Clone, Copy, Default)]
struct LayerRange {
    begin: usize,
    end: usize,
    bounds: Option<Rect>,
}

fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(count)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("same-net pin query work"))?;
    Ok(())
}

impl OwnPins {
    #[cfg(test)]
    pub fn new(shapes: Vec<PinShape>, layers: usize, work: &mut usize, cap: usize) -> Result<Self> {
        Self::with_index(shapes, layers, work, cap, None)
    }
    pub fn with_index(
        shapes: Vec<PinShape>,
        layers: usize,
        work: &mut usize,
        cap: usize,
        mode: Option<OwnPinIndex>,
    ) -> Result<Self> {
        if layers > 64 || shapes.len() > u32::MAX as usize {
            return Err(budget("same-net pin index count"));
        }
        charge(
            work,
            shapes
                .len()
                .saturating_mul(shapes.len().max(1).ilog2() as usize + 3),
            cap,
        )?;
        let mut widths = vec![0; layers];
        for &(layer, rect, _) in &shapes {
            let width = rect
                .ux
                .checked_sub(rect.lx)
                .filter(|&n| n > 0)
                .ok_or_else(|| invalid("same-net pin width"))?;
            let maximum = widths
                .get_mut(layer as usize)
                .ok_or_else(|| invalid("same-net pin layer"))?;
            *maximum = (*maximum).max(width);
        }
        let mut order = Vec::new();
        order
            .try_reserve_exact(shapes.len())
            .map_err(|_| budget("same-net pin index allocation"))?;
        order.extend(0..shapes.len() as u32);
        order.sort_unstable_by_key(|&i| (shapes[i as usize].0, shapes[i as usize].1.lx, i));
        let ranges = if mode.is_some() {
            charge(work, layers.saturating_add(order.len()), cap)?;
            let mut ranges = vec![LayerRange::default(); layers];
            for (position, &id) in order.iter().enumerate() {
                let (layer, rect, _) = shapes[id as usize];
                let range = &mut ranges[layer as usize];
                if range.bounds.is_none() {
                    range.begin = position;
                }
                range.end = position + 1;
                range.bounds = Some(match range.bounds {
                    None => rect,
                    Some(b) => Rect {
                        lx: b.lx.min(rect.lx),
                        ly: b.ly.min(rect.ly),
                        ux: b.ux.max(rect.ux),
                        uy: b.uy.max(rect.uy),
                    },
                });
            }
            Some(ranges)
        } else {
            None
        };
        Ok(Self {
            shapes,
            order,
            widths,
            ranges,
        })
    }

    pub fn blocks(
        &self,
        layer: u32,
        rect: Rect,
        allowed: &[[i64; 2]],
        work: &mut usize,
        cap: usize,
    ) -> Result<bool> {
        if allowed.len() > 2 {
            return Err(budget("same-net allowed endpoint count"));
        }
        let width = *self
            .widths
            .get(layer as usize)
            .ok_or_else(|| invalid("same-net query layer"))?;
        let order = if let Some(ranges) = &self.ranges {
            charge(work, 1, cap)?;
            let range = &ranges[layer as usize];
            if !range.bounds.is_some_and(|b| overlap(rect, b)) {
                return Ok(false);
            }
            &self.order[range.begin..range.end]
        } else {
            &self.order[..]
        };
        let lo = rect.lx.saturating_sub(width);
        let key = |&i: &u32| (self.shapes[i as usize].0, self.shapes[i as usize].1.lx);
        let begin = order.partition_point(|i| key(i) < (layer, lo));
        let end = order.partition_point(|i| key(i) <= (layer, rect.ux));
        charge(
            work,
            (order.len().max(1).ilog2() as usize + 1)
                .saturating_mul(2)
                .saturating_add(end - begin),
            cap,
        )?;
        Ok(order[begin..end].iter().any(|&i| {
            let (_, pin, top) = self.shapes[i as usize];
            overlap(rect, pin) && !allowed.contains(&top)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_ranges_match_linear_oracle_on_empty_cut_layers_and_touching_bounds() {
        let shapes = (0..4000)
            .map(|i| {
                let x = i * 11 - 25000;
                (
                    (i % 2 * 2) as u32,
                    Rect {
                        lx: x,
                        ly: -20,
                        ux: x + 9,
                        uy: 20,
                    },
                    [x + 4, 0],
                )
            })
            .collect::<Vec<_>>();
        let indexed = OwnPins::with_index(
            shapes.clone(),
            7,
            &mut 0,
            1_000_000,
            Some(OwnPinIndex::LayerRanges),
        )
        .unwrap();
        for layer in 0..7 {
            for x in (-25020..20000).step_by(137) {
                for (ly, uy) in [(-25, -20), (-20, -1), (0, 21), (20, 24)] {
                    let rect = Rect {
                        lx: x,
                        ux: x + 9,
                        ly,
                        uy,
                    };
                    for allowed in [vec![], vec![[x + 4, 0]], vec![[x + 4, 0], [x + 15, 0]]] {
                        let expected = shapes.iter().any(|(l, r, t)| {
                            *l == layer && overlap(rect, *r) && !allowed.contains(t)
                        });
                        assert_eq!(
                            indexed.blocks(layer, rect, &allowed, &mut 0, 200).unwrap(),
                            expected,
                            "layer {layer} rect {rect:?}"
                        );
                    }
                }
            }
        }
        let query = Rect {
            lx: 0,
            ly: 0,
            ux: 2,
            uy: 2,
        };
        let mut work = 0;
        assert!(!indexed.blocks(1, query, &[], &mut work, 1).unwrap());
        assert_eq!(work, 1, "empty-layer exact test remains charged");
        assert!(indexed.blocks(1, query, &[], &mut 0, 0).is_err());
        assert!(OwnPins::with_index(shapes, 7, &mut 0, 1, Some(OwnPinIndex::LayerRanges)).is_err());
    }

    #[test]
    fn queries_match_linear_overlap_and_escape_exceptions() {
        let shapes: Vec<_> = (0..7000)
            .map(|i| {
                let x = i as i64 * 17 - 50000;
                (
                    (i % 3) as u32,
                    Rect {
                        lx: x,
                        ly: -20,
                        ux: x + 9,
                        uy: 30,
                    },
                    [x + 3, 0],
                )
            })
            .collect();
        let indexed = OwnPins::new(shapes.clone(), 3, &mut 0, 1_000_000).unwrap();
        for i in (0..7000).step_by(7) {
            let (layer, pin, top) = shapes[i];
            for allowed in [vec![], vec![top], vec![top, [0, 0]]] {
                for dx in [-12, -1, 0, 8, 20] {
                    let query = Rect {
                        lx: pin.lx + dx,
                        ly: -5,
                        ux: pin.lx + dx + 2,
                        uy: 5,
                    };
                    let expected = shapes
                        .iter()
                        .any(|(l, r, t)| *l == layer && overlap(query, *r) && !allowed.contains(t));
                    let mut work = 0;
                    assert_eq!(
                        indexed
                            .blocks(layer, query, &allowed, &mut work, 100)
                            .unwrap(),
                        expected
                    );
                    assert!(work < 40, "query scanned {work} entries");
                }
            }
        }
        assert!(indexed.blocks(0, shapes[0].1, &[], &mut 0, 1).is_err());
    }
}
