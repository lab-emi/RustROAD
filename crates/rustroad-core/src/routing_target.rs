//! Admissible Manhattan distance to a bounded projected routing tree.
use crate::routing::{budget, distance};
use crate::{Rect, Result};

const MAX_SEGMENTS: usize = 4096;
pub(crate) const MEMORY_ALLOWANCE: u64 = 512 * 1024;

pub(crate) struct TreeDistance {
    segments: Vec<Rect>,
    bounds: Rect,
    fallback: bool,
}
impl TreeDistance {
    pub fn new(point: [i64; 2]) -> Self {
        let r = Rect {
            lx: point[0],
            ly: point[1],
            ux: point[0],
            uy: point[1],
        };
        Self {
            segments: vec![r],
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
    pub fn add(&mut self, a: [i64; 2], b: [i64; 2], work: &mut usize, cap: usize) -> Result<()> {
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
        for old in &mut self.segments {
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
            self.segments.push(r);
        }
        Ok(())
    }
    pub fn distance(&self, p: [i64; 2], work: &mut usize, cap: usize) -> Result<u64> {
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
        Self::charge(work, self.segments.len(), cap)?;
        self.segments.iter().try_fold(u64::MAX, |best, r| {
            Ok(best.min(distance(
                p,
                [p[0].clamp(r.lx, r.ux), p[1].clamp(r.ly, r.uy)],
            )?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn l_tree_matches_independent_lattice_targets_and_beats_box_interior() {
        let mut tree = TreeDistance::new([-5, -5]);
        let mut work = 0;
        tree.add([-5, -5], [5, -5], &mut work, 1_000_000).unwrap();
        tree.add([5, -5], [5, 5], &mut work, 1_000_000).unwrap();
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
                    tree.distance([x, y], &mut work, 1_000_000).unwrap(),
                    expected
                );
            }
        }
        assert_eq!(tree.distance([0, 0], &mut work, 1_000_000).unwrap(), 5);
        assert!(tree.distance([0, 0], &mut work, 0).is_err());
    }
    #[test]
    fn segment_cap_falls_back_to_a_lower_bound_without_more_allocation() {
        let mut tree = TreeDistance::new([0, 0]);
        let mut work = 0;
        for i in 1..=MAX_SEGMENTS {
            let n = i as i64 * 2;
            tree.add([n, n], [n, n], &mut work, 100_000_000).unwrap();
        }
        assert!(tree.fallback);
        assert_eq!(tree.segments.capacity(), 0);
        assert_eq!(tree.distance([1, 1], &mut work, 100_000_000).unwrap(), 0);
    }
}
