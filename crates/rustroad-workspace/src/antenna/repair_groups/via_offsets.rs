//! Typed pilot for a single contact replacement and short retained-wire links.
//! No routing search. Reject trials without publishing a geometry revision.

use super::Session;
use crate::antenna::{
    AntennaRequest, AntennaStats, CheckRow, MAX_REPORT_MEMORY_BYTES, NetIndex, Prepared,
    charge_work, evaluate_net, fail, net_index, new_gate_violation, violated_gates,
};
use crate::{Error, Result, pdk::read_input};
use rustroad_core::Limits;
use rustroad_core::routing::{
    LayerRect, Patch, RouteInput, Routes, Via, ViaPadGap, Wire, aligned_via_notch_patch,
    eager_aligned_via_notch_patch, shifted, validate_with_work_limit,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const SCHEMA: &str = "rustroad.antenna-via-offsets.v1";
const MAX_CANDIDATES: usize = 64;
const MAX_COMPONENT_RECORDS: usize = 16_384;
const MAX_PLAN_BYTES: u64 = 256 << 10;
const MAX_SPLIT_VERTICES: usize = 32;

type Point = [i64; 2];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViaOffsetPlan {
    schema: String,
    source_sha256: String,
    cut_area_model: PathBuf,
    #[serde(default)]
    fill_aligned_pad_notch: bool,
    #[serde(default)]
    move_lower_support_cut: bool,
    #[serde(default)]
    eager_lower_pad_gap_fill: bool,
    candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    net: String,
    via: String,
    old_at: Point,
    new_at: Point,
    links: Vec<Link>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Link {
    layer: String,
    start: Point,
    end: Point,
}

#[derive(Debug, Serialize)]
pub struct ViaOffsetSummary {
    schema: &'static str,
    base_revision: String,
    revision: String,
    source_before_sha256: String,
    source_after_sha256: String,
    model_cut_layer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_cut_layer: Option<String>,
    accepted: usize,
    native_work: usize,
    native_work_cap: usize,
    hard_checks: usize,
    comparisons_per_hard_check_cap: usize,
    retained_wire_split_cap_per_candidate: usize,
    trials: Vec<Trial>,
}

#[derive(Debug, Serialize)]
struct Trial {
    #[serde(flatten)]
    candidate: Candidate,
    outcome: &'static str,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_upper_cut: Option<NativeDelta>,
    #[serde(flatten)]
    edits: TrialEdits,
}

#[derive(Debug, Default, Serialize)]
struct TrialEdits {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    retained_wire_splits: Vec<WireSplit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notch_patch: Option<NotchPatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_upper_support: Option<RetainedUpperSupport>,
}

#[derive(Debug, Serialize)]
struct NotchPatch {
    retained_via: Via,
    patch: Patch,
    #[serde(skip_serializing_if = "Option::is_none")]
    unfilled_finding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eager_gap: Option<ViaPadGap>,
}

#[derive(Debug, Serialize)]
struct RetainedUpperSupport {
    retained_via: Via,
    before_geometric_weight: u8,
    after_geometric_weight: u8,
    #[serde(skip)]
    eager_unique: bool,
}

#[derive(Debug, Serialize)]
struct NativeDelta {
    before_ratio: f64,
    after_ratio: f64,
    before_excess: f64,
    after_excess: f64,
}

impl ViaOffsetPlan {
    fn validate(&self) -> Result<()> {
        if self.eager_lower_pad_gap_fill
            && !(self.move_lower_support_cut && self.fill_aligned_pad_notch)
        {
            return Err(fail(
                "ANTENNA_VIA_OFFSETS",
                "eager lower-pad fill needs both lower-cut movement and pad fill",
            ));
        }
        if self.schema != SCHEMA
            || self.source_sha256.len() != 64
            || !self.source_sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.cut_area_model.as_os_str().is_empty()
            || !(1..=MAX_CANDIDATES).contains(&self.candidates.len())
            || self.candidates.iter().any(|c| {
                c.net.is_empty()
                    || c.via.is_empty()
                    || c.links.is_empty()
                    || c.links.len() > 6
                    || c.links.iter().any(|link| link.layer.is_empty())
            })
        {
            return Err(fail(
                "ANTENNA_VIA_OFFSETS",
                "invalid plan identity or bounds",
            ));
        }
        Ok(())
    }
}

/// Read a bounded, fail-closed JSON plan.
///
/// # Errors
/// The plan exceeds its size cap, has an invalid identity, or is malformed.
pub fn read_via_offset_plan(path: &Path) -> Result<ViaOffsetPlan> {
    let (text, _) = read_input(path, MAX_PLAN_BYTES, "antenna_via_offsets")?;
    let plan: ViaOffsetPlan = serde_json::from_str(&text)?;
    plan.validate()?;
    Ok(plan)
}

fn len(a: Point, b: Point) -> u128 {
    u128::from(a[0].abs_diff(b[0])) + u128::from(a[1].abs_diff(b[1]))
}

fn on_wire(wire: &Wire, at: Point) -> bool {
    (0..2)
        .all(|i| (wire.start[i].min(wire.end[i])..=wire.start[i].max(wire.end[i])).contains(&at[i]))
}

#[derive(Debug, Serialize)]
struct WireSplit {
    slot: usize,
    original: Wire,
    vertices: Vec<Point>,
}

impl WireSplit {
    /// Put the first piece in the original slot. Collinear pieces of this
    /// width share endpoint metal, so their copper union and length are exact.
    fn apply(&self, wires: &mut Vec<Wire>) {
        wires[self.slot].end = self.vertices[0];
        let starts = self.vertices.iter().copied();
        let ends = starts
            .clone()
            .skip(1)
            .chain(std::iter::once(self.original.end));
        wires.extend(starts.zip(ends).map(|(start, end)| Wire {
            start,
            end,
            ..self.original.clone()
        }));
    }
}

fn split_retained_wires(
    wires: &[Wire],
    attachment_points: &BTreeSet<(u32, Point)>,
) -> Option<Vec<WireSplit>> {
    let mut splits = Vec::new();
    let mut count = 0;
    for (slot, wire) in wires.iter().enumerate() {
        let mut vertices: Vec<_> = attachment_points
            .iter()
            .filter_map(|&(layer, at)| {
                (wire.layer == layer && at != wire.start && at != wire.end && on_wire(wire, at))
                    .then_some(at)
            })
            .collect();
        if vertices.is_empty() {
            continue;
        }
        count += vertices.len();
        if count > MAX_SPLIT_VERTICES {
            return None;
        }
        vertices.sort_unstable_by_key(|&at| len(wire.start, at));
        splits.push(WireSplit {
            slot,
            original: wire.clone(),
            vertices,
        });
    }
    Some(splits)
}

fn on_track(input: &RouteInput, layer: u32, at: Point) -> bool {
    let direction = input.layers[layer as usize].vertical;
    let track = input
        .tracks
        .as_ref()
        .and_then(|plan| plan.tracks.get(layer as usize).copied().flatten());
    match (direction, track) {
        (Some(vertical), Some((pitch, offset))) => {
            let coordinate = at[usize::from(!vertical)];
            (i128::from(coordinate) - i128::from(offset)).rem_euclid(i128::from(pitch)) == 0
        }
        _ => false,
    }
}

fn path_links(
    prep: &Prepared,
    local: &Routes,
    old: &Via,
    candidate: &Candidate,
    cut: u32,
) -> std::result::Result<Vec<Wire>, &'static str> {
    let input = &prep.input;
    let kind = &input.vias[old.kind as usize];
    let [lo, hi] = [kind.lower, kind.upper];
    if prep.cut_adjacent.get(&cut) != Some(&(lo, hi))
        || kind
            .shapes
            .iter()
            .filter(|shape| shape.layer == cut)
            .count()
            != 1
        || ![lo, hi]
            .iter()
            .all(|&layer| on_track(input, layer, candidate.new_at))
        || !(1..=3 * u128::from(prep.dbu)).contains(&len(candidate.old_at, candidate.new_at))
    {
        return Err("candidate is outside the modeled cut, track, or offset scope");
    }
    let mut wires = Vec::with_capacity(candidate.links.len());
    for layer in [lo, hi] {
        let old_wire = |wire: &&Wire| wire.layer == layer && on_wire(wire, old.at);
        let retained = || local.wires.iter().filter(old_wire);
        let links = candidate
            .links
            .iter()
            .filter(|link| link.layer == input.layers[layer as usize].name);
        if links.clone().next().is_none() {
            if !retained().any(|wire| on_wire(wire, candidate.new_at)) {
                return Err("unlinked layer lacks one retained wire through both sites");
            }
            continue;
        }
        let old_via_landing = local.vias.iter().any(|via| {
            via != old && via.at == old.at && {
                let other = &input.vias[via.kind as usize];
                other.lower == layer || other.upper == layer
            }
        });
        if retained().next().is_none() && !old_via_landing {
            return Err("linked layer lacks a retained wire or via landing");
        }
        let mut last = old.at;
        let mut visited = vec![last];
        let mut axis = None;
        for link in links {
            let horizontal = link.start[1] == link.end[1];
            let planar = horizontal || link.start[0] == link.end[0];
            if link.start != last
                || !planar
                || visited.contains(&link.end)
                || visited.len() > 3
                || axis == Some(horizontal)
                || link
                    .end
                    .iter()
                    .any(|coordinate| coordinate % input.grid != 0)
            {
                return Err("links do not form one bounded on-grid orthogonal path");
            }
            axis = Some(horizontal);
            last = link.end;
            visited.push(last);
            wires.push(Wire {
                net: old.net,
                layer,
                start: link.start,
                end: link.end,
            });
        }
        if last != candidate.new_at {
            return Err("linked layer does not reach the replacement via");
        }
    }
    if wires.len() != candidate.links.len()
        || wires
            .iter()
            .map(|wire| len(wire.start, wire.end))
            .sum::<u128>()
            > 6 * u128::from(prep.dbu)
    {
        return Err("unrecognized layer or excessive total link length");
    }
    Ok(wires)
}

fn normalized(row: &CheckRow) -> f64 {
    row.ratio / row.effective_limit
}

fn no_increase_in_violated_gates(before: &[CheckRow], after: &[CheckRow]) -> bool {
    let mut old: BTreeMap<_, f64> = BTreeMap::new();
    for row in before.iter().filter(|row| row.violated) {
        for gate in &row.gates {
            old.entry((&row.layer, &row.kind, gate))
                .and_modify(|n| *n = n.max(normalized(row)))
                .or_insert_with(|| normalized(row));
        }
    }
    after.iter().filter(|row| row.violated).all(|row| {
        row.gates.iter().all(|gate| {
            old.get(&(&row.layer, &row.kind, gate))
                .is_some_and(|ratio| normalized(row) <= *ratio + 1e-9)
        })
    })
}

fn cut_metric(rows: &[CheckRow], cut: &str) -> (f64, f64) {
    rows.iter()
        .filter(|row| row.layer == cut && row.kind == "area")
        .map(|row| (row.ratio, (normalized(row) - 1.0).max(0.0)))
        .fold((0.0, 0.0), |(max, debt), (ratio, excess)| {
            (max.max(ratio), debt + excess)
        })
}

struct Evaluation<'a> {
    prep: &'a Prepared,
    request: AntennaRequest,
    stats: AntennaStats,
    memory: crate::antenna::report::ReportMemory,
    fill_aligned_pad_notch: bool,
    moved_cut: u32,
    eager_lower_pad_gap_fill: bool,
}

/// Screen the actual old and new lower cut against same-net original upper
/// cuts. The independent before/after antenna calculation decides receiver
/// ownership and improvement after building the edited metal component.
fn lower_support(
    evaluation: &mut Evaluation<'_>,
    local: &Routes,
    old: &Via,
    new_at: Point,
) -> Result<Option<RetainedUpperSupport>> {
    let prep = evaluation.prep;
    let model = prep
        .cut_area
        .as_ref()
        .ok_or_else(|| fail("ANTENNA_VIA_OFFSETS", "cut model required"))?;
    let spec = &prep.input.vias[old.kind as usize];
    let cut = spec
        .shapes
        .iter()
        .find(|shape| shape.layer == model.lower)
        .ok_or_else(|| fail("ANTENNA_VIA_OFFSETS", "modeled lower cut missing"))?;
    let before = shifted(cut.rect, old.at)?;
    let after = shifted(cut.rect, new_at)?;
    let mut selected: Option<RetainedUpperSupport> = None;
    for retained_via in &local.vias {
        let kind = &prep.input.vias[retained_via.kind as usize];
        charge_work(
            &mut evaluation.stats.work,
            kind.shapes.len() + 1,
            evaluation.request.max_work,
        )?;
        for shape in kind
            .shapes
            .iter()
            .filter(|shape| shape.layer == model.upper)
        {
            let upper = shifted(shape.rect, retained_via.at)?;
            let weights = (
                model.support_weight(before, upper),
                model.support_weight(after, upper),
            );
            if let (Some(before_geometric_weight), Some(after_geometric_weight)) = weights
                && before_geometric_weight > after_geometric_weight
            {
                if let Some(mut first) = selected {
                    first.eager_unique = false;
                    return Ok(Some(first));
                }
                let support = RetainedUpperSupport {
                    retained_via: retained_via.clone(),
                    before_geometric_weight,
                    after_geometric_weight,
                    eager_unique: true,
                };
                if !evaluation.eager_lower_pad_gap_fill {
                    return Ok(Some(support));
                }
                selected = Some(support);
            }
        }
    }
    Ok(selected)
}

fn native_change(before: &[CheckRow], after: &[CheckRow], cut: &str) -> (NativeDelta, bool) {
    let (before_ratio, before_excess) = cut_metric(before, cut);
    let (after_ratio, after_excess) = cut_metric(after, cut);
    let improves = after_excess + 1e-9 < before_excess
        && !new_gate_violation(after, &violated_gates(before))
        && no_increase_in_violated_gates(before, after);
    (
        NativeDelta {
            before_ratio,
            after_ratio,
            before_excess,
            after_excess,
        },
        improves,
    )
}

fn find_notch_patch(
    evaluation: &mut Evaluation<'_>,
    local: &Routes,
    moved: &Via,
    detail: &str,
    upper_support: Option<&RetainedUpperSupport>,
) -> Result<Option<NotchPatch>> {
    if !evaluation.fill_aligned_pad_notch || !detail.starts_with("ROUTING_RULE ") {
        return Ok(None);
    }
    let prep = evaluation.prep;
    let model = prep
        .cut_area
        .as_ref()
        .ok_or_else(|| fail("ANTENNA_VIA_OFFSETS", "cut model required"))?;
    if evaluation.moved_cut == model.lower {
        let Some(upper_support) = upper_support else {
            return Ok(None);
        };
        let retained = &upper_support.retained_via;
        charge_work(
            &mut evaluation.stats.work,
            prep.input.vias[moved.kind as usize].shapes.len()
                + prep.input.vias[retained.kind as usize].shapes.len()
                + 1,
            evaluation.request.max_work,
        )?;
        // The verifier takes the contacts in physical lower-to-upper order.
        // The upper-support witness is fixed throughout a lower-cut trial.
        return Ok(
            aligned_via_notch_patch(&prep.input, detail, moved, retained)?.map(|patch| {
                NotchPatch {
                    retained_via: retained.clone(),
                    patch,
                    unfilled_finding: Some(detail.into()),
                    eager_gap: None,
                }
            }),
        );
    }
    let mut patch = None;
    for retained in &local.vias {
        let kind = &prep.input.vias[retained.kind as usize];
        charge_work(
            &mut evaluation.stats.work,
            kind.shapes.len() + 1,
            evaluation.request.max_work,
        )?;
        if !kind.shapes.iter().any(|shape| shape.layer == model.lower) {
            continue;
        }
        if let Some(candidate) = aligned_via_notch_patch(&prep.input, detail, retained, moved)? {
            if patch.is_some() {
                return Ok(None);
            }
            patch = Some(NotchPatch {
                retained_via: retained.clone(),
                patch: candidate,
                unfilled_finding: Some(detail.into()),
                eager_gap: None,
            });
        }
    }
    Ok(patch)
}

fn eager_notch_patch(
    evaluation: &mut Evaluation<'_>,
    session: &Session,
    local: &Routes,
    moved: &Via,
    upper_support: Option<&RetainedUpperSupport>,
) -> Result<Option<NotchPatch>> {
    if !evaluation.eager_lower_pad_gap_fill {
        return Ok(None);
    }
    let Some(support) = upper_support.filter(|support| support.eager_unique) else {
        return Ok(None);
    };
    let retained = &support.retained_via;
    let prep = evaluation.prep;
    let shapes: Vec<_> = net_index::local_shapes(prep, &session.index, local, moved.net)?
        .into_iter()
        .map(|shape| LayerRect {
            layer: shape.layer,
            rect: shape.rect,
        })
        .collect();
    Ok(eager_aligned_via_notch_patch(
        &prep.input,
        moved,
        retained,
        &shapes,
        &mut evaluation.stats.work,
        evaluation.request.max_work,
    )?
    .map(|(patch, eager_gap)| NotchPatch {
        retained_via: retained.clone(),
        patch,
        unfilled_finding: None,
        eager_gap: Some(eager_gap),
    }))
}

struct GeometryEdit {
    old: Via,
    new_at: Point,
    links: Vec<Wire>,
    splits: Vec<WireSplit>,
    added_length: u64,
}

impl GeometryEdit {
    fn probe(
        &self,
        session: &mut Session,
        evaluation: &Evaluation<'_>,
        patch: Option<&Patch>,
    ) -> Result<Option<String>> {
        let prep = evaluation.prep;
        let state = session
            .metadata
            .physical
            .as_mut()
            .and_then(|p| p.routing.as_mut())
            .ok_or_else(|| fail("NO_ROUTING", "missing routing state"))?;
        let routes = &mut state.routes;
        let slot = routes
            .vias
            .iter()
            .position(|via| *via == self.old)
            .ok_or_else(|| fail("ANTENNA_INDEX", "indexed old contact is absent"))?;
        let last_wire = routes.wires.len();
        let last_patch = routes.patches.len();
        let old_length = routes.stats.wire_length_dbu;
        let new_length = old_length
            .checked_add(self.added_length)
            .ok_or_else(|| Error::Budget("ANTENNA_VIA_OFFSETS: route wire length".into()))?;
        routes.vias[slot].at = self.new_at;
        for split in &self.splits {
            split.apply(&mut routes.wires);
        }
        routes.wires.extend_from_slice(&self.links);
        routes.patches.extend(patch.cloned());
        routes.stats.wire_length_dbu = new_length;
        match validate_with_work_limit(
            &prep.input,
            routes,
            &state.config.limits,
            evaluation.request.max_work,
        ) {
            Ok(()) => {
                session.index = NetIndex::new(prep, routes)?;
                Ok(None)
            }
            Err(error) => {
                routes.vias[slot].at = self.old.at;
                routes.wires.truncate(last_wire);
                routes.patches.truncate(last_patch);
                for split in &self.splits {
                    routes.wires[split.slot] = split.original.clone();
                }
                routes.stats.wire_length_dbu = old_length;
                match error {
                    rustroad_core::Error::Invalid(detail) => Ok(Some(detail)),
                    other => Err(other.into()),
                }
            }
        }
    }
}

impl Evaluation<'_> {
    fn rows(
        &mut self,
        session: &Session,
        routes: &Routes,
        net: u32,
    ) -> Result<crate::antenna::report::Rows> {
        let shapes = net_index::local_shapes(self.prep, &session.index, routes, net)?;
        evaluate_net(
            self.prep,
            &session.metadata.design,
            session.model()?,
            net,
            &shapes,
            &self.request,
            &mut self.stats,
            &mut false,
            Some(&self.memory),
        )
    }
}

fn bind_candidate(
    session: &Session,
    prep: &Prepared,
    candidate: &Candidate,
) -> Result<Option<Via>> {
    let Some(net) = session
        .model()?
        .nets
        .iter()
        .position(|net| net.name == candidate.net)
    else {
        return Ok(None);
    };
    let net =
        u32::try_from(net).map_err(|_| Error::Budget("ANTENNA_VIA_OFFSETS: net id".into()))?;
    let Some(kind) = prep
        .input
        .vias
        .iter()
        .position(|via| via.name == candidate.via)
    else {
        return Ok(None);
    };
    let kind =
        u32::try_from(kind).map_err(|_| Error::Budget("ANTENNA_VIA_OFFSETS: via id".into()))?;
    let old = Via {
        net,
        kind,
        at: candidate.old_at,
    };
    let count = session
        .routes()?
        .vias
        .iter()
        .filter(|via| **via == old)
        .take(2)
        .count();
    Ok((count == 1).then_some(old))
}

fn try_candidate(
    session: &mut Session,
    evaluation: &mut Evaluation<'_>,
    candidate: &Candidate,
    old: Via,
    cut_name: &str,
    hard_checks: &mut usize,
    edits: &mut TrialEdits,
) -> Result<(&'static str, String, Option<NativeDelta>)> {
    let rejected = |detail: &str| Ok(("skipped", detail.into(), None));
    let prep = evaluation.prep;
    let Via { net, kind, .. } = old;
    let mut local = session.index.local(session.routes()?, net)?;
    if local.wires.len() + local.vias.len() + local.patches.len() + local.access.len()
        > MAX_COMPONENT_RECORDS
    {
        return rejected("net record limit reached");
    }
    let mut matches = local
        .vias
        .iter()
        .enumerate()
        .filter(|(_, via)| **via == old);
    let Some((local_slot, _)) = matches.next() else {
        return rejected("expected old via absent or already replaced");
    };
    if matches.next().is_some()
        || local
            .vias
            .iter()
            .any(|via| via.kind == kind && via.at == candidate.new_at)
    {
        return rejected("old or replacement via site is ambiguous");
    }
    let links = match path_links(prep, &local, &old, candidate, evaluation.moved_cut) {
        Ok(links) => links,
        Err(reason) => return rejected(reason),
    };
    let retained_upper_support = if prep
        .cut_area
        .as_ref()
        .is_some_and(|model| model.lower == evaluation.moved_cut)
    {
        let Some(retained) = lower_support(evaluation, &local, &old, candidate.new_at)? else {
            return rejected(
                "lower cut does not reduce calibrated overlap with a retained upper cut",
            );
        };
        Some(retained)
    } else {
        None
    };
    let via_spec = &prep.input.vias[kind as usize];
    let attachment_points = [via_spec.lower, via_spec.upper]
        .into_iter()
        .flat_map(|layer| [(layer, old.at), (layer, candidate.new_at)])
        .chain(
            links
                .iter()
                .flat_map(|wire| [(wire.layer, wire.start), (wire.layer, wire.end)]),
        )
        .collect::<BTreeSet<_>>();
    charge_work(
        &mut evaluation.stats.work,
        local.wires.len() * attachment_points.len(),
        evaluation.request.max_work,
    )?;
    let Some(mut splits) = split_retained_wires(&local.wires, &attachment_points) else {
        return rejected("retained-wire split vertex cap reached");
    };
    let before = evaluation.rows(session, &local, net)?;
    let (_, before_excess) = cut_metric(&before.data, cut_name);
    if before_excess == 0.0 {
        return rejected("no upper-cut excess under the pinned native model");
    }
    local.vias[local_slot].at = candidate.new_at;
    for split in &splits {
        split.apply(&mut local.wires);
    }
    local.wires.extend(links.iter().cloned());
    let after = evaluation.rows(session, &local, net)?;
    let (delta, improves) = native_change(&before.data, &after.data, cut_name);
    if !improves {
        return Ok((
            "native_rejected",
            "no safe native upper-cut improvement".into(),
            Some(delta),
        ));
    }
    let length = links.iter().map(|w| len(w.start, w.end)).sum::<u128>();
    let added = u64::try_from(length)
        .map_err(|_| Error::Budget("ANTENNA_VIA_OFFSETS: link length".into()))?;
    for split in &mut splits {
        split.slot = session
            .index
            .wire_slots(net)
            .get(split.slot)
            .copied()
            .map(|slot| slot as usize)
            .ok_or_else(|| fail("ANTENNA_INDEX", "retained wire has no global slot"))?;
    }
    let edit = GeometryEdit {
        old,
        new_at: candidate.new_at,
        links,
        splits,
        added_length: added,
    };
    if let Some(patch) = eager_notch_patch(
        evaluation,
        session,
        &local,
        &local.vias[local_slot],
        retained_upper_support.as_ref(),
    )? {
        let patch_count = local.patches.len();
        local.patches.push(patch.patch.clone());
        let filled = evaluation.rows(session, &local, net)?;
        local.patches.truncate(patch_count);
        let (eager_delta, improves) = native_change(&before.data, &filled.data, cut_name);
        if improves {
            *hard_checks += 1;
            if edit
                .probe(session, evaluation, Some(&patch.patch))?
                .is_none()
            {
                edits.retained_wire_splits = edit.splits;
                edits.notch_patch = Some(patch);
                edits.retained_upper_support = retained_upper_support;
                return Ok((
                    "accepted",
                    "full native route validation passed with one pad fill".into(),
                    Some(eager_delta),
                ));
            }
        }
    }
    *hard_checks += 1;
    let Some(detail) = edit.probe(session, evaluation, None)? else {
        edits.retained_wire_splits = edit.splits;
        edits.retained_upper_support = retained_upper_support;
        return Ok((
            "accepted",
            "full native route validation passed".into(),
            Some(delta),
        ));
    };
    let Some(patch) = find_notch_patch(
        evaluation,
        &local,
        &local.vias[local_slot],
        &detail,
        retained_upper_support.as_ref(),
    )?
    else {
        return Ok(("geometry_rejected", detail, Some(delta)));
    };
    local.patches.push(patch.patch.clone());
    let filled = evaluation.rows(session, &local, net)?;
    let (delta, improves) = native_change(&before.data, &filled.data, cut_name);
    if !improves {
        return Ok((
            "native_rejected",
            "pad fill has no safe native upper-cut improvement".into(),
            Some(delta),
        ));
    }
    *hard_checks += 1;
    if let Some(detail) = edit.probe(session, evaluation, Some(&patch.patch))? {
        return Ok((
            "geometry_rejected",
            format!("pad fill rejected: {detail}"),
            Some(delta),
        ));
    }
    edits.retained_wire_splits = edit.splits;
    edits.notch_patch = Some(patch);
    edits.retained_upper_support = retained_upper_support;
    Ok((
        "accepted",
        "full native route validation passed with one pad fill".into(),
        Some(delta),
    ))
}

/// Apply ranked contact replacements under an exclusive revision lock.
///
/// # Errors
/// The revision, route geometry, or model is stale, required inputs fail, or
/// a memory or work budget is reached. No candidate trials are committed.
pub fn repair_via_offsets(
    workspace: &Path,
    base_rev: &str,
    plan: &ViaOffsetPlan,
    limits: &Limits,
) -> Result<ViaOffsetSummary> {
    plan.validate()?;
    let (mut session, prep) = Session::open(
        workspace,
        base_rev,
        limits,
        Some((&plan.cut_area_model, &plan.source_sha256)),
    )?;
    let model = prep
        .cut_area
        .as_ref()
        .ok_or_else(|| fail("ANTENNA_VIA_OFFSETS", "cut model required"))?;
    let cut_name = &prep.input.layers[model.upper as usize].name;
    let mut evaluation = Evaluation {
        prep: &prep,
        request: AntennaRequest::default(),
        stats: AntennaStats::default(),
        memory: crate::antenna::report::ReportMemory::new(MAX_REPORT_MEMORY_BYTES),
        fill_aligned_pad_notch: plan.fill_aligned_pad_notch,
        moved_cut: if plan.move_lower_support_cut {
            model.lower
        } else {
            model.upper
        },
        eager_lower_pad_gap_fill: plan.eager_lower_pad_gap_fill,
    };
    let source_before_sha256 = session.source.clone();
    let old_sites = plan
        .candidates
        .iter()
        .map(|c| bind_candidate(&session, &prep, c))
        .collect::<Result<Vec<_>>>()?;
    let mut trials = Vec::with_capacity(plan.candidates.len());
    let mut accepted_sites = BTreeSet::new();
    let mut hard_checks = 0;
    for (candidate, old) in plan.candidates.iter().zip(old_sites) {
        let key = old.as_ref().map(|via| (via.net, via.kind, via.at));
        let mut edits = TrialEdits::default();
        let (outcome, detail, native_upper_cut) = match old {
            None => (
                "skipped",
                "candidate has no unique old via in the frozen base".into(),
                None,
            ),
            Some(_) if key.is_some_and(|key| accepted_sites.contains(&key)) => {
                ("skipped", "old via site already replaced".into(), None)
            }
            Some(old) => try_candidate(
                &mut session,
                &mut evaluation,
                candidate,
                old,
                cut_name,
                &mut hard_checks,
                &mut edits,
            )?,
        };
        if outcome == "accepted"
            && let Some(key) = key
        {
            accepted_sites.insert(key);
        }
        trials.push(Trial {
            candidate: candidate.clone(),
            outcome,
            detail,
            native_upper_cut,
            edits,
        });
    }
    let accepted = accepted_sites.len();
    if accepted > 0 {
        session.number = crate::next_free_revision(workspace, session.number)?;
        session.metadata.revision = format!("rev{}", session.number);
        session.logical = 1;
        session.source = session.digest.fingerprint(session.routes()?)?;
        session.commit(&prep)?;
    }
    Ok(ViaOffsetSummary {
        schema: SCHEMA,
        base_revision: base_rev.into(),
        revision: session.metadata.revision.clone(),
        source_before_sha256,
        source_after_sha256: session.source.clone(),
        model_cut_layer: cut_name.clone(),
        moved_cut_layer: plan.move_lower_support_cut.then(|| {
            prep.input.layers[evaluation.moved_cut as usize]
                .name
                .clone()
        }),
        accepted,
        native_work: evaluation.stats.work,
        native_work_cap: evaluation.request.max_work,
        hard_checks,
        comparisons_per_hard_check_cap: evaluation.request.max_work,
        retained_wire_split_cap_per_candidate: MAX_SPLIT_VERTICES,
        trials,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SPLIT_VERTICES, ViaOffsetPlan, Wire, len, repair_via_offsets, split_retained_wires,
    };
    use rustroad_core::routing::wire_rect;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn combined_lower_cut_plan_still_checks_bounds_before_direct_workspace_access() {
        let mut plan: ViaOffsetPlan = serde_json::from_value(serde_json::json!({
            "schema": super::SCHEMA,
            "source_sha256": "0".repeat(64),
            "cut_area_model": "model.json",
            "fill_aligned_pad_notch": true,
            "move_lower_support_cut": true,
            "candidates": [{
                "net": "native_electrical_291_n",
                "via": "M3M4_PR",
                "old_at": [936100, 328100],
                "new_at": [937020, 328100],
                "links": [{"layer": "met4", "start": [936100, 328100], "end": [937020, 328100]}]
            }],
        }))
        .unwrap();
        plan.validate().unwrap();
        plan.candidates.clear();
        let error = repair_via_offsets(
            Path::new("/dev/null"),
            "rev1",
            &plan,
            &rustroad_core::Limits::default(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid plan identity or bounds")
        );
    }

    #[test]
    fn moved_lower_pad_fills_only_the_reported_fixed_upper_pair() {
        use rustroad_core::Rect;
        use rustroad_core::routing::{
            LayerRect, RouteInput, RouteLayer, RouteVia, Via, aligned_via_notch_patch,
            eager_aligned_via_notch_patch,
        };

        let rect = |lx, ly, ux, uy| Rect { lx, ly, ux, uy };
        let met4 = RouteLayer {
            name: "met4".into(),
            width: 300,
            spacing: 300,
            vertical: Some(true),
            min_area: None,
            min_step: None,
            wide_spacing: None,
        };
        let via = |name: &str, lower, upper, h: i64| RouteVia {
            name: name.into(),
            lower,
            upper,
            shapes: vec![LayerRect {
                layer: 4,
                rect: rect(-h, -h, h, h),
            }],
        };
        let input = RouteInput {
            die: rect(0, 0, 1_505_120, 816_000),
            grid: 5,
            step: 5,
            access_layer: 0,
            layers: vec![met4; 6],
            vias: vec![via("M3M4_PR", 3, 4, 165), via("M4M5_PR", 4, 5, 590)],
            obstacles: Vec::new(),
            instances: None,
            terminals: Vec::new(),
            rules: true,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            held_bytes: 0,
        };
        let moved = Via {
            kind: 0,
            net: 14636,
            at: [937020, 328100],
        };
        let retained = Via {
            kind: 1,
            net: 14636,
            at: [936100, 328100],
        };
        let finding = "ROUTING_RULE same-net notch/spacing net 14636 layer 4 (935510,327510)-(936690,328690) (936855,327935)-(937185,328265) (1 total)";
        let patch = aligned_via_notch_patch(&input, finding, &moved, &retained)
            .unwrap()
            .unwrap();
        assert_eq!(
            (patch.net, patch.layer, patch.rect),
            (14636, 4, rect(936690, 327935, 936855, 328265))
        );
        let layer_rect = |rect| LayerRect { layer: 4, rect };
        let mut work = 0;
        let (eager, gap) = eager_aligned_via_notch_patch(
            &input,
            &moved,
            &retained,
            &[layer_rect(rect(936690, 327950, 936855, 328250))],
            &mut work,
            1000,
        )
        .unwrap()
        .unwrap();
        assert_eq!(eager, patch);
        assert_eq!(gap.facing_gap, patch.rect);
        assert!(
            eager_aligned_via_notch_patch(
                &input,
                &moved,
                &retained,
                &[layer_rect(patch.rect)],
                &mut work,
                1000,
            )
            .unwrap()
            .is_none()
        );
        let complex_covers =
            [327950, 327970, 327990, 328010].map(|y| layer_rect(rect(936690, y, 936855, y + 5)));
        assert!(
            eager_aligned_via_notch_patch(
                &input,
                &moved,
                &retained,
                &complex_covers,
                &mut work,
                1000,
            )
            .unwrap()
            .is_none()
        );
        let unrelated_retained_pad = Via {
            at: [936105, 328100],
            ..retained
        };
        assert!(
            aligned_via_notch_patch(&input, finding, &moved, &unrelated_retained_pad)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interior_junctions_keep_exact_retained_copper_and_length() {
        let trunk = Wire {
            net: 3,
            layer: 5,
            start: [1840, 460],
            end: [0, 460],
        };
        let untouched = Wire {
            layer: 4,
            ..trunk.clone()
        };
        let points = BTreeSet::from([
            (5, [0, 460]),
            (5, [460, 460]),
            (5, [1380, 460]),
            (5, [920, 0]),
        ]);
        let mut wires = vec![trunk.clone(), untouched.clone()];
        let splits = split_retained_wires(&wires, &points).unwrap();
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].vertices, [[1380, 460], [460, 460]]);
        splits[0].apply(&mut wires);
        assert_eq!(wires[1], untouched);

        let parts = [&wires[0], &wires[2], &wires[3]];
        assert_eq!(parts[0].start, trunk.start);
        assert_eq!(parts[2].end, trunk.end);
        assert_eq!(parts[0].end, parts[1].start);
        assert_eq!(parts[1].end, parts[2].start);
        assert_eq!(
            parts.iter().map(|w| len(w.start, w.end)).sum::<u128>(),
            len(trunk.start, trunk.end)
        );
        let original_rect = wire_rect(&trunk, 300).unwrap();
        let rects = parts.map(|w| wire_rect(w, 300).unwrap());
        assert_eq!(rects[0].ux, original_rect.ux);
        assert_eq!(rects[2].lx, original_rect.lx);
        assert!(rects[0].lx < rects[1].ux && rects[1].lx < rects[2].ux);
        assert!(rects.iter().all(|r| {
            r.ly == original_rect.ly
                && r.uy == original_rect.uy
                && r.lx >= original_rect.lx
                && r.ux <= original_rect.ux
        }));
        let excessive = vec![trunk; MAX_SPLIT_VERTICES];
        assert!(split_retained_wires(&excessive, &points).is_none());
    }
}
