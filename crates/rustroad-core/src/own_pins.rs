//! Bounded same-net pin-metal overlap queries for detailed routing.
use crate::routing::{budget, invalid, overlap};
use crate::{Rect, Result};

type PinShape = (u32, Rect, [i64; 2]);

pub(crate) struct OwnPins {
    shapes: Vec<PinShape>,
    order: Vec<u32>,
    widths: Vec<i64>,
}

fn charge(work: &mut usize, count: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(count)
        .filter(|&n| n <= cap)
        .ok_or_else(|| budget("same-net pin query work"))?;
    Ok(())
}

impl OwnPins {
    pub fn new(shapes: Vec<PinShape>, layers: usize, work: &mut usize, cap: usize) -> Result<Self> {
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
        Ok(Self {
            shapes,
            order,
            widths,
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
        let lo = rect.lx.saturating_sub(width);
        let key = |&i: &u32| (self.shapes[i as usize].0, self.shapes[i as usize].1.lx);
        let begin = self.order.partition_point(|i| key(i) < (layer, lo));
        let end = self.order.partition_point(|i| key(i) <= (layer, rect.ux));
        charge(
            work,
            (self.order.len().max(1).ilog2() as usize + 1)
                .saturating_mul(2)
                .saturating_add(end - begin),
            cap,
        )?;
        Ok(self.order[begin..end].iter().any(|&i| {
            let (_, pin, top) = self.shapes[i as usize];
            overlap(rect, pin) && !allowed.contains(&top)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
