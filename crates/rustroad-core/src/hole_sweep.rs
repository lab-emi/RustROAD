//! Bounded sweep-line complement components for sparse rectangle unions.
use crate::routing::{budget, invalid};
use crate::{Rect, Result};

fn charge(work: &mut usize, n: usize, cap: usize) -> Result<()> {
    *work = work
        .checked_add(n)
        .filter(|&v| v <= cap)
        .ok_or_else(|| budget("same-net hole sweep work"))?;
    Ok(())
}

#[derive(Clone, Copy)]
struct Component {
    bounds: Rect,
    seed: (i64, i64),
    outside: bool,
}
#[derive(Clone, Copy)]
struct Span {
    lo: usize,
    hi: usize,
    component: usize,
}
#[derive(Clone, Copy)]
struct Event {
    x: usize,
    lo: usize,
    hi: usize,
    delta: i32,
}

struct Cover {
    count: Vec<i32>,
    covered: Vec<usize>,
}
impl Cover {
    fn update(
        &mut self,
        node: usize,
        lo: usize,
        hi: usize,
        event: Event,
        work: &mut usize,
        cap: usize,
    ) -> Result<()> {
        charge(work, 1, cap)?;
        if event.lo <= lo && hi <= event.hi {
            self.count[node] += event.delta;
            if self.count[node] < 0 {
                return Err(invalid("negative hole sweep coverage"));
            }
        } else {
            let mid = lo + (hi - lo) / 2;
            if event.lo < mid {
                self.update(node * 2, lo, mid, event, work, cap)?;
            }
            if event.hi > mid {
                self.update(node * 2 + 1, mid, hi, event, work, cap)?;
            }
        }
        self.covered[node] = if self.count[node] != 0 {
            hi - lo
        } else if hi - lo == 1 {
            0
        } else {
            self.covered[node * 2] + self.covered[node * 2 + 1]
        };
        Ok(())
    }
    fn free(
        &self,
        node: usize,
        lo: usize,
        hi: usize,
        out: &mut Vec<(usize, usize)>,
        work: &mut usize,
        cap: usize,
    ) -> Result<()> {
        charge(work, 1, cap)?;
        if self.covered[node] == hi - lo {
            return Ok(());
        }
        if self.covered[node] == 0 {
            if let Some(last) = out.last_mut()
                && last.1 == lo
            {
                last.1 = hi;
            } else {
                out.push((lo, hi));
            }
        } else {
            let mid = lo + (hi - lo) / 2;
            self.free(node * 2, lo, mid, out, work, cap)?;
            self.free(node * 2 + 1, mid, hi, out, work, cap)?;
        }
        Ok(())
    }
}

fn root(mut id: usize, parent: &mut [usize], work: &mut usize, cap: usize) -> Result<usize> {
    while parent[id] != id {
        charge(work, 1, cap)?;
        parent[id] = parent[parent[id]];
        id = parent[id];
    }
    Ok(id)
}
fn join(
    a: usize,
    b: usize,
    parent: &mut [usize],
    ranks: &mut [u8],
    data: &mut [Component],
    work: &mut usize,
    cap: usize,
) -> Result<()> {
    let (mut a, mut b) = (root(a, parent, work, cap)?, root(b, parent, work, cap)?);
    if a == b {
        return Ok(());
    }
    if ranks[a] < ranks[b] {
        std::mem::swap(&mut a, &mut b);
    }
    parent[b] = a;
    if ranks[a] == ranks[b] {
        ranks[a] += 1;
    }
    let x = data[b];
    let y = &mut data[a];
    y.bounds.lx = y.bounds.lx.min(x.bounds.lx);
    y.bounds.ly = y.bounds.ly.min(x.bounds.ly);
    y.bounds.ux = y.bounds.ux.max(x.bounds.ux);
    y.bounds.uy = y.bounds.uy.max(x.bounds.uy);
    y.seed = y.seed.min(x.seed);
    y.outside |= x.outside;
    Ok(())
}

/// Return exactly the bounded free components in dense flood-fill seed order.
pub(crate) fn find(
    rects: &[Rect],
    xs: &[i64],
    ys: &[i64],
    work: &mut usize,
    cap: usize,
    max_cells: usize,
) -> Result<Vec<Rect>> {
    let limit = (max_cells as u64).saturating_mul(64);
    let fixed = (rects.len() as u64)
        .saturating_mul(160)
        .saturating_add((ys.len() as u64).saturating_mul(512))
        .saturating_add((xs.len() as u64).saturating_mul(64));
    if fixed > limit {
        return Err(budget("same-net hole sweep memory"));
    }
    charge(
        work,
        rects
            .len()
            .saturating_mul((xs.len().max(1).ilog2() + ys.len().max(1).ilog2() + 4) as usize),
        cap,
    )?;
    let mut events = Vec::new();
    events
        .try_reserve_exact(
            rects
                .len()
                .checked_mul(2)
                .ok_or_else(|| budget("hole sweep events"))?,
        )
        .map_err(|_| budget("hole sweep events allocation"))?;
    for r in rects {
        if !r.valid() {
            return Err(invalid("hole sweep rectangle"));
        }
        let lo = ys
            .binary_search(&r.ly)
            .map_err(|_| invalid("hole sweep y coordinate"))?;
        let hi = ys
            .binary_search(&r.uy)
            .map_err(|_| invalid("hole sweep y coordinate"))?;
        events.push(Event {
            x: xs
                .binary_search(&r.lx)
                .map_err(|_| invalid("hole sweep x coordinate"))?,
            lo,
            hi,
            delta: 1,
        });
        events.push(Event {
            x: xs
                .binary_search(&r.ux)
                .map_err(|_| invalid("hole sweep x coordinate"))?,
            lo,
            hi,
            delta: -1,
        });
    }
    charge(
        work,
        events
            .len()
            .saturating_mul(events.len().max(1).ilog2() as usize + 1),
        cap,
    )?;
    events.sort_unstable_by_key(|e| (e.x, e.delta, e.lo, e.hi));
    let ny = ys.len() - 1;
    let slots = ny
        .checked_mul(4)
        .and_then(|n| n.checked_add(8))
        .ok_or_else(|| budget("hole sweep tree size"))?;
    let mut cover = Cover {
        count: vec![0; slots],
        covered: vec![0; slots],
    };
    let mut previous = Vec::<Span>::new();
    let mut components = Vec::<Component>::new();
    let mut free = Vec::new();
    free.try_reserve_exact(ny)
        .map_err(|_| budget("hole sweep frontier allocation"))?;
    let mut holes = Vec::<Component>::new();
    let mut event = 0;
    for x in 0..xs.len() - 1 {
        charge(work, 1, cap)?;
        while event < events.len() && events[event].x == x {
            cover.update(1, 0, ny, events[event], work, cap)?;
            event += 1;
        }
        free.clear();
        cover.free(1, 0, ny, &mut free, work, cap)?;
        let old = components.len();
        charge(
            work,
            old.saturating_mul(2)
                .saturating_add(free.len().saturating_mul(6)),
            cap,
        )?;
        let mut data = components.clone();
        data.try_reserve_exact(free.len())
            .map_err(|_| budget("hole sweep component allocation"))?;
        let mut current = Vec::new();
        current
            .try_reserve_exact(free.len())
            .map_err(|_| budget("hole sweep interval allocation"))?;
        for &(lo, hi) in &free {
            let id = data.len();
            data.push(Component {
                bounds: Rect {
                    lx: xs[x],
                    ly: ys[lo],
                    ux: xs[x + 1],
                    uy: ys[hi],
                },
                seed: (ys[lo], xs[x]),
                outside: x == 0 || x + 2 == xs.len() || lo == 0 || hi == ny,
            });
            current.push(Span {
                lo,
                hi,
                component: id,
            });
        }
        let mut parent: Vec<usize> = (0..data.len()).collect();
        let mut ranks = vec![0u8; data.len()];
        let (mut a, mut b) = (0, 0);
        while a < previous.len() && b < current.len() {
            charge(work, 1, cap)?;
            let (p, q) = (previous[a], current[b]);
            if p.lo < q.hi && q.lo < p.hi {
                join(
                    p.component,
                    q.component,
                    &mut parent,
                    &mut ranks,
                    &mut data,
                    work,
                    cap,
                )?;
            }
            if p.hi <= q.hi {
                a += 1;
            }
            if q.hi <= p.hi {
                b += 1;
            }
        }
        let mut live = vec![false; data.len()];
        for span in &mut current {
            span.component = root(span.component, &mut parent, work, cap)?;
            live[span.component] = true;
        }
        for i in 0..old {
            if parent[i] == i && !live[i] && !data[i].outside {
                if holes.len() >= max_cells.min(1_000_000) {
                    return Err(budget("hole sweep output count"));
                }
                if holes.len() == holes.capacity() {
                    let next = holes
                        .capacity()
                        .max(8)
                        .saturating_mul(2)
                        .min(max_cells.min(1_000_000));
                    let growth = (holes.capacity() as u64)
                        .saturating_mul(56)
                        .saturating_add((next as u64).saturating_mul(96));
                    if fixed.saturating_add(growth) > limit {
                        return Err(budget("hole sweep output memory"));
                    }
                    holes
                        .try_reserve_exact(next - holes.len())
                        .map_err(|_| budget("hole sweep output allocation"))?;
                }
                holes.push(data[i]);
            }
        }
        let mut mapping = vec![usize::MAX; data.len()];
        let mut next = Vec::new();
        next.try_reserve_exact(current.len())
            .map_err(|_| budget("hole sweep live component allocation"))?;
        for span in &mut current {
            let id = span.component;
            if mapping[id] == usize::MAX {
                mapping[id] = next.len();
                next.push(data[id]);
            }
            span.component = mapping[id];
        }
        previous = current;
        components = next;
    }
    charge(
        work,
        holes
            .len()
            .saturating_mul(holes.len().max(1).ilog2() as usize + 1),
        cap,
    )?;
    holes.sort_unstable_by_key(|c| c.seed);
    Ok(holes.into_iter().map(|c| c.bounds).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sweep(rectangles: &[Rect], cap: usize, cells: usize) -> Result<Vec<Rect>> {
        let mut xs: Vec<_> = rectangles.iter().flat_map(|r| [r.lx, r.ux]).collect();
        let mut ys: Vec<_> = rectangles.iter().flat_map(|r| [r.ly, r.uy]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        find(rectangles, &xs, &ys, &mut 0, cap, cells)
    }
    #[test]
    fn sweep_matches_the_independent_dense_flood_for_overlaps_and_contacts() {
        let mut random = 731u32;
        for case in 0..1000 {
            let mut next = || {
                random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                random ^ (random >> 16)
            };
            let mut rectangles = Vec::new();
            for _ in 0..1 + case % 12 {
                let x = i64::from(next() % 10) - 5;
                let y = i64::from(next() % 10) - 5;
                rectangles.push(Rect {
                    lx: x,
                    ly: y,
                    ux: x + 1 + i64::from(next() % 4),
                    uy: y + 1 + i64::from(next() % 4),
                });
            }
            let expected = crate::route_rules::holes(&rectangles, &mut 0, 1_000_000, 1000).unwrap();
            assert_eq!(
                sweep(&rectangles, 1_000_000, 1000).unwrap(),
                expected,
                "case {case}"
            );
        }
    }
    #[test]
    fn ring_hole_and_budget_failures_are_explicit() {
        let ring = [
            Rect {
                lx: 0,
                ly: 0,
                ux: 6,
                uy: 2,
            },
            Rect {
                lx: 0,
                ly: 4,
                ux: 6,
                uy: 6,
            },
            Rect {
                lx: 0,
                ly: 2,
                ux: 2,
                uy: 4,
            },
            Rect {
                lx: 4,
                ly: 2,
                ux: 6,
                uy: 4,
            },
        ];
        assert_eq!(
            sweep(&ring, 10000, 1000).unwrap(),
            [Rect {
                lx: 2,
                ly: 2,
                ux: 4,
                uy: 4
            }]
        );
        assert!(sweep(&ring, 1, 1000).is_err());
        assert!(sweep(&ring, 10000, 1).is_err());
        assert!(sweep(&ring[..3], 10000, 1000).unwrap().is_empty());
    }

    #[test]
    fn crossing_stripes_and_point_contacts_match_dense_components() {
        let mut rectangles = Vec::new();
        for n in [0, 3, 6, 9] {
            rectangles.push(Rect {
                lx: n,
                ly: 0,
                ux: n + 1,
                uy: 10,
            });
            rectangles.push(Rect {
                lx: 0,
                ly: n,
                ux: 10,
                uy: n + 1,
            });
        }
        let expected = crate::route_rules::holes(&rectangles, &mut 0, 1_000_000, 1000).unwrap();
        assert_eq!(expected.len(), 9);
        assert_eq!(sweep(&rectangles, 1_000_000, 1000).unwrap(), expected);
        rectangles.push(Rect {
            lx: 1,
            ly: 1,
            ux: 2,
            uy: 2,
        });
        rectangles.push(Rect {
            lx: 2,
            ly: 2,
            ux: 3,
            uy: 3,
        });
        assert_eq!(
            sweep(&rectangles, 1_000_000, 1000).unwrap(),
            crate::route_rules::holes(&rectangles, &mut 0, 1_000_000, 1000).unwrap()
        );
    }
}
