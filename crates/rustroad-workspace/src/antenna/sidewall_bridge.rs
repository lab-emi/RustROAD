//! Optional sidewall attribution for a localized, deck-calibrated contact pattern.
use super::{
    AntennaRequest, AntennaStats, ComponentAreas, Dsu, Prepared, Shape, charge_work, fail, overlap,
    touch,
};
use crate::{Error, Result};
use rustroad_core::Rect;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

const MAX_BRIDGE_SHAPES: usize = 4096;

/// Memory for portals and two directions of the unambiguous match table.
pub(super) fn scratch_bytes(shapes: usize) -> usize {
    shapes.min(MAX_BRIDGE_SHAPES) * 192
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    receiver_cut: String,
    portal_cut: String,
    cut_square_dbu: i64,
    measured_pad_width_dbu: i64,
    measured_pad_height_dbu: i64,
    bridge_pad_square_dbu: i64,
    bridge_width_dbu: i64,
    portal_spacing_min_dbu: i64,
    portal_spacing_max_dbu: i64,
    receiver_cut_offset_dbu: i64,
    thickness_dbu: i64,
}

pub(super) struct Model {
    receiver_cut: u32,
    portal_cut: u32,
    pub(super) measured_layer: u32,
    bridge_layer: u32,
    cut_square: i64,
    measured_pad_size: [i64; 2],
    bridge_pad_square: i64,
    bridge_width: i64,
    spacing: [i64; 2],
    receiver_offset: i64,
    pub(super) thickness_dbu: i64,
}

impl Profile {
    pub(super) fn resolve(self, prep: &Prepared) -> Result<Model> {
        let cap = i64::from(prep.dbu).saturating_mul(20);
        if [
            self.cut_square_dbu,
            self.measured_pad_width_dbu,
            self.measured_pad_height_dbu,
            self.bridge_pad_square_dbu,
            self.bridge_width_dbu,
            self.portal_spacing_min_dbu,
            self.portal_spacing_max_dbu,
            self.receiver_cut_offset_dbu,
            self.thickness_dbu,
        ]
        .into_iter()
        .any(|v| !(1..=cap).contains(&v))
            || self.cut_square_dbu % 2 != 0
            || self.bridge_pad_square_dbu % 2 != 0
            || self.portal_spacing_min_dbu > self.portal_spacing_max_dbu
            || self.portal_spacing_min_dbu <= self.measured_pad_width_dbu
            || self.receiver_cut_offset_dbu >= self.portal_spacing_min_dbu
        {
            return Err(fail(
                "ANTENNA_SIDEWALL_MODEL",
                "invalid local bridge geometry",
            ));
        }
        let cut = |name: &str| {
            prep.input
                .layers
                .iter()
                .position(|layer| layer.name == name && layer.vertical.is_none())
                .map(|id| id as u32)
                .filter(|id| prep.cut_adjacent.contains_key(id))
                .ok_or_else(|| {
                    fail(
                        "ANTENNA_SIDEWALL_MODEL",
                        format!("unknown cut layer {name}"),
                    )
                })
        };
        let receiver_cut = cut(&self.receiver_cut)?;
        let portal_cut = cut(&self.portal_cut)?;
        let (_, measured_layer) = prep.cut_adjacent[&receiver_cut];
        let (below, bridge_layer) = prep.cut_adjacent[&portal_cut];
        let rank = |id: u32| prep.positions[id as usize];
        if below != measured_layer
            || !(rank(receiver_cut) < rank(measured_layer)
                && rank(measured_layer) < rank(portal_cut)
                && rank(portal_cut) < rank(bridge_layer))
        {
            return Err(fail(
                "ANTENNA_SIDEWALL_MODEL",
                "cuts must share the measured routing layer",
            ));
        }
        Ok(Model {
            receiver_cut,
            portal_cut,
            measured_layer,
            bridge_layer,
            cut_square: self.cut_square_dbu,
            measured_pad_size: [self.measured_pad_width_dbu, self.measured_pad_height_dbu],
            bridge_pad_square: self.bridge_pad_square_dbu,
            bridge_width: self.bridge_width_dbu,
            spacing: [self.portal_spacing_min_dbu, self.portal_spacing_max_dbu],
            receiver_offset: self.receiver_cut_offset_dbu,
            thickness_dbu: self.thickness_dbu,
        })
    }
}

pub(super) struct Stage<'a> {
    pub(super) by_layer: &'a HashMap<u32, Vec<usize>>,
    pub(super) shapes: &'a [Shape],
    pub(super) components: &'a BTreeMap<usize, ComponentAreas>,
    pub(super) dsu: &'a mut Dsu,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Portal {
    center: [i128; 2],
    root: usize,
}

fn extent(rect: Rect) -> [i128; 2] {
    [
        i128::from(rect.ux) - i128::from(rect.lx),
        i128::from(rect.uy) - i128::from(rect.ly),
    ]
}

fn center(rect: Rect) -> [i128; 2] {
    [
        (i128::from(rect.lx) + i128::from(rect.ux)) / 2,
        (i128::from(rect.ly) + i128::from(rect.uy)) / 2,
    ]
}

fn charge(stats: &mut AntennaStats, request: &AntennaRequest, count: usize) -> Result<()> {
    stats.pair_checks = stats
        .pair_checks
        .checked_add(count)
        .filter(|&used| used <= request.max_pair_checks)
        .ok_or_else(|| Error::Budget("ANTENNA: max_pair_checks".into()))?;
    charge_work(&mut stats.work, count, request.max_work)
}

fn assign_unique(map: &mut BTreeMap<usize, Option<usize>>, key: usize, value: usize) {
    map.entry(key)
        .and_modify(|old| {
            if *old != Some(value) {
                *old = None;
            }
        })
        .or_insert(Some(value));
}

struct BridgeGeometry<'a> {
    shapes: &'a [Shape],
    metal: &'a [usize],
    portals: &'a [usize],
}

impl Model {
    // Only the observed two pads and one straight wire may touch this span.
    // Keep this local rule off T-junctions and the next via stack.
    fn isolated(
        &self,
        wire: Rect,
        points: [[i128; 2]; 2],
        geometry: &BridgeGeometry<'_>,
        request: &AntennaRequest,
        stats: &mut AntennaStats,
    ) -> Result<bool> {
        let half = i128::from(self.bridge_pad_square / 2);
        let lower_x = points[0][0] - half;
        let upper_x = points[1][0] + half;
        let lower_y = points[0][1] - half;
        let upper_y = points[0][1] + half;
        let mut pads = [false; 2];
        for &i in geometry.metal {
            let shape = &geometry.shapes[i];
            let rect = shape.rect;
            charge(stats, request, 1)?;
            if i128::from(rect.lx) > upper_x.max(i128::from(wire.ux)) {
                break;
            }
            let span_overlap = i128::from(rect.ux) >= lower_x
                && i128::from(rect.uy) >= lower_y
                && i128::from(rect.ly) <= upper_y
                && i128::from(rect.lx) <= upper_x;
            if !span_overlap && !touch(wire, rect) {
                continue;
            }
            if shape.terminal.is_some() {
                return Ok(false);
            }
            if rect == wire {
                continue;
            }
            let point = center(rect);
            if extent(rect) != [i128::from(self.bridge_pad_square); 2] {
                return Ok(false);
            }
            let Some(index) = points.iter().position(|&p| p == point) else {
                return Ok(false);
            };
            pads[index] = true;
        }
        if !pads.into_iter().all(|present| present) {
            return Ok(false);
        }
        for &i in geometry.portals {
            let rect = geometry.shapes[i].rect;
            charge(stats, request, 1)?;
            if i128::from(rect.lx) >= upper_x.max(i128::from(wire.ux)) {
                break;
            }
            if overlap(wire, rect) && !points.contains(&center(rect)) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn transfers(
        &self,
        Stage {
            by_layer,
            shapes,
            components,
            dsu,
        }: Stage<'_>,
        request: &AntennaRequest,
        stats: &mut AntennaStats,
    ) -> Result<BTreeMap<usize, usize>> {
        let indices = |layer| by_layer.get(&layer).map_or(&[][..], Vec::as_slice);
        let receiver_cuts = indices(self.receiver_cut);
        let portal_cuts = indices(self.portal_cut);
        let bridge_metal = indices(self.bridge_layer);
        let measured_metal = indices(self.measured_layer);
        if receiver_cuts.is_empty() || portal_cuts.len() < 2 || bridge_metal.is_empty() {
            return Ok(BTreeMap::new());
        }
        let bridge_geometry = BridgeGeometry {
            shapes,
            metal: bridge_metal,
            portals: portal_cuts,
        };
        let shape_count = receiver_cuts.len() + portal_cuts.len() + bridge_metal.len();
        if shape_count > MAX_BRIDGE_SHAPES {
            return Err(Error::Budget(
                "ANTENNA: sidewall bridge shapes per net".into(),
            ));
        }
        charge_work(&mut stats.work, shape_count, request.max_work)?;
        let cut_size = [i128::from(self.cut_square); 2];
        if receiver_cuts
            .iter()
            .chain(portal_cuts)
            .any(|&i| extent(shapes[i].rect) != cut_size)
        {
            return Err(fail(
                "ANTENNA_SIDEWALL_MODEL",
                "cut geometry differs from calibration",
            ));
        }

        let mut portals = Vec::with_capacity(portal_cuts.len());
        for &index in portal_cuts {
            let cut = shapes[index].rect;
            let point = center(cut);
            let mut root = None;
            let mut pad = false;
            let mut multiple = false;
            for &i in measured_metal {
                let metal = shapes[i].rect;
                charge(stats, request, 1)?;
                if metal.lx >= cut.ux {
                    break;
                }
                if overlap(cut, metal) {
                    let owner = dsu.find(i);
                    multiple |= root.is_some_and(|old| old != owner);
                    root = Some(owner);
                    pad |= extent(metal) == self.measured_pad_size.map(i128::from)
                        && center(metal) == point;
                }
            }
            if pad
                && !multiple
                && let Some(root) = root
            {
                portals.push(Portal {
                    center: point,
                    root,
                });
            }
        }
        // Accepted raw cuts have equal width. Their sorted input guarantees
        // monotonic x centers through filtering, including duplicate records.
        let mut from_receiver = BTreeMap::new();
        let mut from_donor = BTreeMap::new();
        for source in &portals {
            if !components
                .get(&source.root)
                .is_some_and(|c| c.gates.len() == 1 && c.diffusion == 0 && c.terminals == 1)
            {
                continue;
            }
            let [x, y] = source.center;
            let expected = x + i128::from(self.receiver_offset);
            let lower_at = receiver_cuts.partition_point(|&i| center(shapes[i].rect)[0] < expected);
            charge_work(
                &mut stats.work,
                receiver_cuts.len().ilog2() as usize + 3,
                request.max_work,
            )?;
            let mut has_lower_contact = false;
            for &i in &receiver_cuts[lower_at..] {
                charge(stats, request, 1)?;
                let [lo_x, lo_y] = center(shapes[i].rect);
                if lo_x != expected {
                    break;
                }
                if lo_y == y && dsu.find(i) == source.root {
                    has_lower_contact = true;
                    break;
                }
            }
            if !has_lower_contact {
                continue;
            }
            let left_min = x - i128::from(self.spacing[1]);
            let left_max = x - i128::from(self.spacing[0]);
            let left = portals.partition_point(|p| p.center[0] < left_min);
            charge_work(
                &mut stats.work,
                portals.len().ilog2() as usize + 32,
                request.max_work,
            )?;
            for donor in &portals[left..] {
                charge(stats, request, 1)?;
                let [dx, dy] = donor.center;
                if dx > left_max {
                    break;
                }
                if dy != y || donor.root == source.root || components.contains_key(&donor.root) {
                    continue;
                }
                for &i in bridge_metal {
                    charge(stats, request, 1)?;
                    let rect = shapes[i].rect;
                    let wire_half = i128::from(self.bridge_width / 2);
                    if i128::from(rect.lx) > dx {
                        break;
                    }
                    if extent(rect)[1] == i128::from(self.bridge_width)
                        && center(rect)[1] == y
                        && i128::from(rect.ux) >= x
                        && i128::from(rect.lx) >= dx - wire_half
                        && i128::from(rect.ux) <= x + wire_half
                        && self.isolated(
                            rect,
                            [donor.center, source.center],
                            &bridge_geometry,
                            request,
                            stats,
                        )?
                    {
                        assign_unique(&mut from_receiver, source.root, donor.root);
                        assign_unique(&mut from_donor, donor.root, source.root);
                        break;
                    }
                }
            }
        }
        Ok(from_receiver
            .into_iter()
            .filter_map(|(receiver, donor)| {
                let donor = donor?;
                (from_donor[&donor] == Some(receiver)).then_some((receiver, donor))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(layer: u32, [lx, ly, ux, uy]: [i64; 4]) -> Shape {
        Shape {
            layer,
            rect: Rect { lx, ly, ux, uy },
            terminal: None,
        }
    }

    #[test]
    fn sidewall_transfer_requires_the_calibrated_one_hop_geometry() {
        let model = Model {
            receiver_cut: 0,
            measured_layer: 1,
            portal_cut: 2,
            bridge_layer: 3,
            cut_square: 200,
            measured_pad_size: [380, 320],
            bridge_pad_square: 330,
            bridge_width: 300,
            spacing: [910, 930],
            receiver_offset: 230,
            thickness_dbu: 845,
        };
        // Dimensions from independent SKY130 fixtures. Include a valid second
        // bridge on the opposite side with no qualifying directed transition.
        for (pitch, offset, wire, branch, far_terminal, expected) in [
            (910, 230, true, false, false, true),
            (920, 230, true, false, false, true),
            (930, 230, true, false, false, true),
            (920, 690, true, false, false, false),
            (920, -230, true, false, false, false),
            (6000, 230, true, false, false, false),
            (920, 230, false, false, false, false),
            (920, 230, true, true, false, false),
            (920, 230, true, false, true, false),
        ] {
            let mut shapes = vec![
                shape(0, [offset - 100, -100, offset + 100, 100]),
                shape(1, [-190, -160, 190, 160]),
                shape(1, [-pitch - 190, -160, -pitch + 190, 160]),
                shape(2, [-100, -100, 100, 100]),
                shape(2, [-pitch - 100, -100, -pitch + 100, 100]),
                shape(3, [-165, -165, 165, 165]),
                shape(3, [-pitch - 165, -165, -pitch + 165, 165]),
                shape(1, [730, -160, 1110, 160]),
                shape(1, [1650, -160, 2030, 160]),
                shape(2, [820, -100, 1020, 100]),
                shape(2, [1740, -100, 1940, 100]),
                shape(3, [755, -165, 1085, 165]),
                shape(3, [1675, -165, 2005, 165]),
                shape(3, [770, -150, 1990, 150]),
            ];
            if wire {
                shapes.push(shape(3, [-pitch - 150, -150, 150, 150]));
            }
            if branch {
                shapes.push(shape(3, [-450, -150, -150, 1000]));
            }
            let mut by_layer = HashMap::<u32, Vec<usize>>::new();
            for (i, shape) in shapes.iter().enumerate() {
                by_layer.entry(shape.layer).or_default().push(i);
            }
            for list in by_layer.values_mut() {
                list.sort_by_key(|&i| (shapes[i].rect.lx, i));
            }
            let mut components = BTreeMap::from([(
                0,
                ComponentAreas {
                    gate: 159_000,
                    terminals: 1,
                    gates: vec![(0, "buf/A".into())],
                    ..ComponentAreas::default()
                },
            )]);
            if far_terminal {
                components.insert(
                    2,
                    ComponentAreas {
                        terminals: 1,
                        ..ComponentAreas::default()
                    },
                );
            }
            let mut dsu = Dsu((0..shapes.len()).collect());
            dsu.union(0, 1);
            let mut stats = AntennaStats::default();
            let transfers = model
                .transfers(
                    Stage {
                        by_layer: &by_layer,
                        shapes: &shapes,
                        components: &components,
                        dsu: &mut dsu,
                    },
                    &AntennaRequest::default(),
                    &mut stats,
                )
                .unwrap();
            assert_eq!(
                transfers.get(&0).copied(),
                expected.then_some(2),
                "{pitch}/{offset}/{wire}/{branch}/{far_terminal}"
            );
            assert_eq!(transfers.len(), usize::from(expected));
            assert_eq!(dsu.find(2), 2, "physical connectivity must be unchanged");
        }
    }
}
