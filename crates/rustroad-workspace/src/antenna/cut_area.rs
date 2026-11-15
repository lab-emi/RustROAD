//! Optional cut-area weights calibrated by a process deck, applied to the
//! lower-side component that exists when the upper cut is formed.
use super::{AntennaRequest, AntennaStats, Dsu, Prepared, Shape, SourceCheck, charge_work, fail};
use crate::pdk::read_input;
use crate::{Error, Result};
use rustroad_core::Rect;
use serde::Deserialize;
use std::path::Path;

const SCHEMA: &str = "rustroad.antenna-cut-area.v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CutLayer {
    layer: String,
    square_dbu: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    schema: String,
    technology_lef_sha256: String,
    dbu_per_micron: u32,
    lower_cut: CutLayer,
    upper_cut: CutLayer,
    containment_dbu: i64,
    overlap_dbu: i64,
}

pub(super) struct Model {
    pub(super) lower: u32,
    pub(super) upper: u32,
    lower_size: i64,
    upper_size: i64,
    containment: i64,
    overlap: i64,
}

pub(super) fn read(path: &Path, prep: &Prepared) -> Result<(Model, SourceCheck)> {
    let (doc, source) = read_input(path, 16 * 1024, "antenna_cut_area")?;
    let profile: Profile = serde_json::from_str(&doc)?;
    if profile.schema != SCHEMA
        || prep.dbu != profile.dbu_per_micron
        || !prep.sources.iter().any(|source| {
            source.role == "technology_lef" && source.sha256 == profile.technology_lef_sha256
        })
        || !(0 < profile.containment_dbu && profile.containment_dbu < profile.overlap_dbu)
        || [&profile.lower_cut, &profile.upper_cut]
            .iter()
            .any(|cut| cut.square_dbu <= 0 || cut.square_dbu % 2 != 0)
    {
        return Err(fail(
            "ANTENNA_CUT_MODEL",
            "profile or technology identity differs",
        ));
    }
    let layer = |cut: &CutLayer| -> Result<u32> {
        prep.input
            .layers
            .iter()
            .position(|layer| layer.name == cut.layer && layer.vertical.is_none())
            .map(|n| n as u32)
            .ok_or_else(|| {
                fail(
                    "ANTENNA_CUT_MODEL",
                    format!("unknown cut layer {}", cut.layer),
                )
            })
    };
    let lower = layer(&profile.lower_cut)?;
    let upper = layer(&profile.upper_cut)?;
    let (_, middle) = prep.cut_adjacent.get(&lower).copied().ok_or_else(|| {
        fail(
            "ANTENNA_CUT_MODEL",
            "no routing-layer adjacency for lower cut",
        )
    })?;
    let (below, above) = prep.cut_adjacent.get(&upper).copied().ok_or_else(|| {
        fail(
            "ANTENNA_CUT_MODEL",
            "no routing-layer adjacency for upper cut",
        )
    })?;
    let rank = |id| prep.positions[id as usize];
    if middle != below
        || !(rank(lower) < rank(middle) && rank(middle) < rank(upper) && rank(upper) < rank(above))
    {
        return Err(fail(
            "ANTENNA_CUT_MODEL",
            "cut layers must bound the same intermediate routing layer",
        ));
    }
    Ok((
        Model {
            lower,
            upper,
            lower_size: profile.lower_cut.square_dbu,
            upper_size: profile.upper_cut.square_dbu,
            containment: profile.containment_dbu,
            overlap: profile.overlap_dbu,
        },
        SourceCheck {
            role: source.role,
            path: source.path,
            sha256: source.sha256,
        },
    ))
}

/// Only changed weights are stored. At most 8 bytes per upper cut (1.53 MiB
/// at the default per-net shape cap, 7.63 MiB at the absolute request cap).
#[derive(Debug)]
pub(super) struct Weight {
    shape: u32,
    scale: u8,
}

fn cut_key(rect: &Rect) -> [i64; 2] {
    [rect.lx, rect.ly]
}

pub(super) fn lookup(weights: &[Weight], shapes: &[Shape], rect: &Rect) -> u8 {
    weights
        .binary_search_by_key(&cut_key(rect), |w| cut_key(&shapes[w.shape as usize].rect))
        .map_or(1, |at| weights[at].scale)
}

impl Model {
    fn weight(&self, dx: i128, dy: i128) -> u8 {
        if dx >= i128::from(self.overlap) || dy >= i128::from(self.overlap) {
            return 1;
        }
        4 - u8::from(dx >= i128::from(self.containment))
            - u8::from(dy >= i128::from(self.containment))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn weights(
        &self,
        lower: &[usize],
        upper: &[usize],
        shapes: &[Shape],
        dsu: &mut Dsu,
        request: &AntennaRequest,
        stats: &mut AntennaStats,
    ) -> Result<Vec<Weight>> {
        let square = |i: usize, size: i64| {
            let r = shapes[i].rect;
            i128::from(r.ux) - i128::from(r.lx) == i128::from(size)
                && i128::from(r.uy) - i128::from(r.ly) == i128::from(size)
        };
        // Equal input widths preserve the original ordering by lower x edge,
        // and make (lx, ly) a complete key for each physical upper cut.
        charge_work(&mut stats.work, lower.len() + upper.len(), request.max_work)?;
        if !lower.iter().all(|&i| square(i, self.lower_size))
            || !upper.iter().all(|&i| square(i, self.upper_size))
        {
            return Err(fail(
                "ANTENNA_CUT_MODEL",
                "raw cut dimensions differ from the calibration",
            ));
        }
        if lower.is_empty() || upper.is_empty() {
            return Ok(Vec::new());
        }

        let mut weights = Vec::with_capacity(upper.len());
        let overlap = i128::from(self.overlap);
        let lower_half = i128::from(self.lower_size / 2);
        let upper_half = i128::from(self.upper_size / 2);
        // Two x-window binary searches per upper cut plus the final sort.
        let overhead = upper.len().saturating_mul(
            2 * (lower.len().ilog2() as usize + 1) + upper.len().ilog2() as usize + 1,
        );
        charge_work(&mut stats.work, overhead, request.max_work)?;
        for &i in upper {
            let here = shapes[i].rect;
            let x = i128::from(here.lx) + upper_half;
            let y = i128::from(here.ly) + upper_half;
            let first = lower
                .partition_point(|&j| i128::from(shapes[j].rect.lx) + lower_half <= x - overlap);
            let last = lower
                .partition_point(|&j| i128::from(shapes[j].rect.lx) + lower_half < x + overlap);
            let candidates = &lower[first..last];
            stats.pair_checks = stats
                .pair_checks
                .checked_add(candidates.len())
                .filter(|&used| used <= request.max_pair_checks)
                .ok_or_else(|| Error::Budget("ANTENNA: max_pair_checks".into()))?;
            charge_work(&mut stats.work, candidates.len(), request.max_work)?;
            let root = dsu.find(i);
            let mut neighbor = None;
            for &j in candidates {
                let cut = shapes[j].rect;
                let dx = (i128::from(cut.lx) + lower_half - x).abs();
                let dy = (i128::from(cut.ly) + lower_half - y).abs();
                let scale = self.weight(dx, dy);
                if scale == 1 || dsu.find(j) != root {
                    continue;
                }
                // A repeated physical cut is one neighbor. Distinct lower
                // neighbors in one window were outside the calibration.
                if neighbor.is_some_and(|(prior, _)| prior != cut) {
                    return Err(fail(
                        "ANTENNA_CUT_MODEL",
                        "multiple lower contacts within one upper-cut window",
                    ));
                }
                neighbor = Some((cut, scale));
            }
            if let Some((_, scale)) = neighbor {
                weights.push(Weight {
                    shape: i as u32,
                    scale,
                });
            }
        }
        weights.sort_unstable_by_key(|w| cut_key(&shapes[w.shape as usize].rect));
        Ok(weights)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model {
            lower: 1,
            upper: 3,
            lower_size: 200,
            upper_size: 800,
            containment: 430,
            overlap: 750,
        }
    }

    fn cut(layer: u32, [x, y]: [i64; 2], half: i64) -> Shape {
        Shape {
            layer,
            rect: Rect {
                lx: x - half,
                ly: y - half,
                ux: x + half,
                uy: y + half,
            },
            terminal: None,
        }
    }

    #[test]
    fn calibrated_four_three_two_and_one_cut_weights() {
        let m = model();
        for (dx, dy, expected) in [
            (0, 0, 4),
            (425, 425, 4),
            (430, 425, 3),
            (425, 430, 3),
            (430, 430, 2),
            (745, 745, 2),
            (750, 745, 1),
            (745, 750, 1),
        ] {
            assert_eq!(m.weight(dx, dy), expected);
        }
    }

    #[test]
    fn only_a_lower_stage_shared_component_carries_the_weight() {
        let shapes = [
            cut(1, [0, 0], 100),
            cut(3, [0, 0], 400),
            cut(1, [2000, 0], 100),
            cut(3, [2430, 0], 400),
        ];
        let m = model();
        let request = AntennaRequest::default();
        let mut stats = AntennaStats::default();
        let mut dsu = Dsu((0..shapes.len()).collect());
        dsu.union(0, 1);
        let at_upper_cut = m
            .weights(&[0, 2], &[1, 3], &shapes, &mut dsu, &request, &mut stats)
            .unwrap();
        assert_eq!(lookup(&at_upper_cut, &shapes, &shapes[1].rect), 4);
        assert_eq!(lookup(&at_upper_cut, &shapes, &shapes[3].rect), 1);
        dsu.union(2, 3);
        let after_upper_metal = m
            .weights(&[0, 2], &[1, 3], &shapes, &mut dsu, &request, &mut stats)
            .unwrap();
        assert_eq!(lookup(&after_upper_metal, &shapes, &shapes[3].rect), 3);
    }
}
