//! Bounded snapshots of one detailed repair attempt, never a routing revision.
use super::owner_repair::{self, BoundaryOwner};
use super::{Cells, Graph, Move, Outcome, Plan, Router};
use crate::route_search::History;
use crate::routing::{
    RouteInput, RouteLimits, SingleOwnerRepairConfig, SingleOwnerRepairStats, Soft, Spatial,
    SpatialCapture, SpatialState, Terminal, Via, Wire, budget, int, invalid,
};
use crate::{Rect, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

const MAX_BYTES: u64 = 512 << 20;
const MAX_HISTORY: usize = 4_000_000;
const MAX_TERMINALS: usize = 100_000;

/// The exact point at which the captured state was observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPhase {
    #[default]
    PreSoft,
    /// After the final limited call, including consumed work and alternate root order.
    PostFailure,
}

/// Explicit diagnostic output for one selected pre-search or failed state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCapture {
    pub path: PathBuf,
    pub net: Option<u32>,
    pub round: Option<usize>,
    pub max_bytes: u64,
    #[serde(default)]
    pub phase: ReplayPhase,
}
impl ReplayCapture {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(1024..=MAX_BYTES).contains(&self.max_bytes)
            || !self.path.is_absolute()
            || self
                .path
                .components()
                .any(|p| matches!(p, Component::ParentDir))
            || self.round == Some(0)
            || (self.phase == ReplayPhase::PreSoft && self.net.is_none())
        {
            return Err(budget("replay capture path/byte/round bounds"));
        }
        Ok(())
    }
}

/// Read-only experiments preserve technology rules and the original state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayOptions {
    /// Optional per-search expansion override; all absolute ceilings remain.
    pub max_search_expansions: Option<usize>,
    /// Optional replacement GCell rectangle, in integer DBU and inside the die.
    pub guide_bounds: Option<Rect>,
    /// Check only immediate wire/via geometry at at most 32 captured terminals.
    #[serde(default)]
    pub probe_terminals: bool,
    /// Search-only override, bounded to the same 1..=16 range as TrackPlan.
    pub heuristic_weight: Option<u64>,
    /// Explicit diagnostic without ripping any existing route. Requires a
    /// per-search bound; source geometry and aggregate ledger stay unchanged.
    #[serde(default)]
    pub hard_search: bool,
    /// At most4096 current-epoch discovered states after a hard diagnostic.
    #[serde(default)]
    pub probe_search_frontier: bool,
    /// Replay-only experiment: this owner may be displaced, all others stay hard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_victim: Option<u32>,
    /// Explicit local experiment after source validation; production uses TrackPlan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_owner_repair: Option<SingleOwnerRepairConfig>,
}

/// Immediate graph edge geometry, without claiming route reachability or DRC.
#[derive(Debug, Serialize)]
pub struct NeighborProbe {
    pub layer: u32,
    pub at: [i64; 2],
    pub movement: &'static str,
    pub via_kind: Option<usize>,
    pub in_guide: bool,
    pub routing_layer_allowed: bool,
    pub geometry: &'static str,
    pub victim_count: usize,
}

/// A fixed terminal escape's immediate on-graph neighbors.
#[derive(Debug, Serialize)]
pub struct TerminalProbe {
    pub pin: u32,
    pub layer: u32,
    pub at: [i64; 2],
    pub top_via: bool,
    pub neighbors: Vec<NeighborProbe>,
}

/// One local search result; it never establishes whole-design physical signoff.
#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub schema: &'static str,
    pub status: &'static str,
    pub scope: &'static str,
    pub search_mode: &'static str,
    pub source_phase: ReplayPhase,
    pub source_failed_pin: Option<u32>,
    pub input_sha256: String,
    pub net: u32,
    pub round: usize,
    pub failed_pin: Option<u32>,
    pub terminal_order: Vec<u32>,
    pub source_guide_bounds: Option<Rect>,
    pub used_guide_bounds: Option<Rect>,
    pub used_guide_cells: usize,
    pub configured_heuristic_weight: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configured_repair_heuristic_weight: Option<u64>,
    pub used_heuristic_weight: u64,
    pub terminal_probes: Option<Vec<TerminalProbe>>,
    pub expansions_before: usize,
    pub expansions_after: usize,
    pub comparisons_before: usize,
    pub comparisons_after: usize,
    pub wires: Option<Vec<Wire>>,
    pub vias: Option<Vec<Via>>,
    pub victims: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_frontier: Option<Vec<SearchFrontierNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_frontier_work: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_owners: Option<Vec<BoundaryOwner>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_victim: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_owner_repair: Option<SingleOwnerRepairStats>,
}

/// Discovered states of the final sink search, not enumerated paths.
#[derive(Debug, Serialize)]
pub struct SearchFrontierNode {
    pub node: u32,
    pub layer: u32,
    pub at: [i64; 2],
    pub cost: u64,
    pub parent: Option<u32>,
    pub incoming_kind: u32,
}

fn probe_terminals(
    router: &mut Router<'_>,
    plans: &BTreeMap<u32, Plan>,
    net: u32,
) -> Result<Vec<TerminalProbe>> {
    if plans.len() > 32 {
        return Err(budget("terminal probe limit: at most 32 terminals"));
    }
    let mut probes = Vec::with_capacity(plans.len());
    let mut neighbors = Vec::with_capacity(16);
    for (&pin, plan) in plans {
        let (layer, at) = router.graph.point(plan.top);
        router.graph.neighbors(plan.top, &mut neighbors);
        if neighbors.len() > 64 {
            return Err(budget("terminal probe neighbor limit"));
        }
        let mut rows = Vec::new();
        for &(next, movement) in &neighbors {
            if movement == Move::Along && plan.top.abs_diff(next) != 1 {
                continue;
            }
            let (next_layer, next_at) = router.graph.point(next);
            let (name, kinds) = match movement {
                Move::Along => ("preferred_wire", None),
                Move::Wrong => ("wrong_way_wire", None),
                Move::Via(pair) => {
                    let kinds = &router.graph.pairs[pair].kinds;
                    if kinds.len() > 64 {
                        return Err(budget("terminal probe via-kind limit"));
                    }
                    ("via", Some(kinds.clone()))
                }
            };
            let variants: &[usize] = kinds.as_deref().unwrap_or(&[usize::MAX]);
            for &kind in variants {
                if rows.len() >= 64 {
                    return Err(budget("terminal probe variant limit"));
                }
                let geometry = if kind == usize::MAX {
                    let rect = router.edge_rect(layer, at, next_at, net)?;
                    router.spatial.probe_soft(layer, rect, net)?
                } else {
                    router.spatial.via_probe_soft(kind, at, net)?
                };
                let (geometry, victim_count) = match geometry {
                    Soft::Blocked => ("fixed_blocked", 0),
                    Soft::Clear { victims, .. } => ("clear_or_rippable", victims.len()),
                };
                rows.push(NeighborProbe {
                    layer: next_layer,
                    at: next_at,
                    movement: name,
                    via_kind: (kind != usize::MAX).then_some(kind),
                    in_guide: router.gmark[router.cells.of(next_at) as usize] == router.gepoch,
                    routing_layer_allowed: router.plan.allows_routing_layer(next_layer),
                    geometry,
                    victim_count,
                });
            }
        }
        probes.push(TerminalProbe {
            pin,
            layer,
            at,
            top_via: plan.top_via,
            neighbors: rows,
        });
    }
    Ok(probes)
}

#[derive(Serialize)]
struct Capture<'a> {
    schema: &'static str,
    phase: ReplayPhase,
    failed_pin: Option<u32>,
    input_sha256: String,
    limits: &'a RouteLimits,
    net: u32,
    round: usize,
    strict: bool,
    terminal_order: Vec<u32>,
    plans: BTreeMap<u32, &'a Plan>,
    rips: &'a BTreeMap<u32, usize>,
    spatial: SpatialCapture<'a>,
    history: Vec<(u32, u16)>,
    guide_marks: &'a [u32],
    guide_epoch: u32,
    search_epoch: u32,
    expansions: usize,
    access_entries: usize,
    access_cache_bytes: u64,
    guide_bounds: Option<Rect>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema: String,
    #[serde(default)]
    phase: ReplayPhase,
    #[serde(default)]
    failed_pin: Option<u32>,
    input_sha256: String,
    limits: RouteLimits,
    net: u32,
    round: usize,
    strict: bool,
    terminal_order: Vec<u32>,
    plans: BTreeMap<u32, Plan>,
    rips: BTreeMap<u32, usize>,
    spatial: SpatialState,
    history: Vec<(u32, u16)>,
    guide_marks: Vec<u32>,
    guide_epoch: u32,
    search_epoch: u32,
    expansions: usize,
    access_entries: usize,
    access_cache_bytes: u64,
    guide_bounds: Option<Rect>,
}

struct BoundedWriter<W> {
    writer: W,
    remaining: u64,
}
impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("replay encoded-byte budget exceeded"));
        }
        let n = self.writer.write(bytes)?;
        self.remaining -= n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn input_digest(input: &RouteInput) -> Result<String> {
    let mut writer = BoundedWriter {
        writer: HashWriter(Sha256::new()),
        remaining: MAX_BYTES,
    };
    #[derive(Serialize)]
    struct View<'a> {
        die: Rect,
        grid: i64,
        step: i64,
        access_layer: u32,
        layers: &'a [crate::routing::RouteLayer],
        vias: &'a [crate::routing::RouteVia],
        obstacles: &'a [crate::routing::Blockage],
        instances: &'a Option<crate::routing::InstanceObstacles>,
        terminals: Vec<&'a Terminal>,
        rules: bool,
        access_classes: &'a [crate::routing::AccessClass],
        tracks: &'a Option<crate::routing::TrackPlan>,
        ties: &'a [crate::routing::SupplyTie],
    }
    if input.terminals.len() > 1_000_000 {
        return Err(budget("replay input terminal limit"));
    }
    let mut terminals: Vec<_> = input.terminals.iter().collect();
    terminals.sort_unstable_by_key(|t| t.pin);
    if terminals.windows(2).any(|v| v[0].pin == v[1].pin) {
        return Err(invalid("duplicate replay input terminal"));
    }
    let view = View {
        die: input.die,
        grid: input.grid,
        step: input.step,
        access_layer: input.access_layer,
        layers: &input.layers,
        vias: &input.vias,
        obstacles: &input.obstacles,
        instances: &input.instances,
        terminals,
        rules: input.rules,
        access_classes: &input.access_classes,
        tracks: &input.tracks,
        ties: &input.ties,
    };
    serde_json::to_writer(&mut writer, &view)
        .map_err(|e| invalid(&format!("replay input fingerprint: {e}")))?;
    Ok(format!("{:x}", writer.writer.0.finalize()))
}
fn cell_rect(cells: &Cells, cell: usize, die: Rect) -> Result<Rect> {
    let (x, y) = cells.xy(cell as u32);
    let lx = int(i128::from(cells.lx) + x as i128 * i128::from(cells.g))?;
    let ly = int(i128::from(cells.ly) + y as i128 * i128::from(cells.g))?;
    Ok(Rect {
        lx,
        ly,
        ux: lx.saturating_add(cells.g).min(die.ux),
        uy: ly.saturating_add(cells.g).min(die.uy),
    })
}
fn guide_bounds(router: &Router<'_>) -> Result<(Option<Rect>, usize)> {
    let mut result: Option<Rect> = None;
    let mut count = 0;
    for (id, &mark) in router.gmark.iter().enumerate() {
        if mark != router.gepoch {
            continue;
        }
        let r = cell_rect(&router.cells, id, router.input.die)?;
        result = Some(match result {
            None => r,
            Some(old) => Rect {
                lx: old.lx.min(r.lx),
                ly: old.ly.min(r.ly),
                ux: old.ux.max(r.ux),
                uy: old.uy.max(r.uy),
            },
        });
        count += 1;
    }
    Ok((result, count))
}

pub(super) fn capture_if_requested(
    router: &Router<'_>,
    net: u32,
    terminals: &[&Terminal],
    plans: &HashMap<u32, Plan>,
    strict: bool,
    rips: &BTreeMap<u32, usize>,
    round: usize,
) -> Result<()> {
    capture_at_phase(
        router,
        net,
        terminals,
        plans,
        strict,
        rips,
        CaptureMoment {
            round,
            phase: ReplayPhase::PreSoft,
            failed_pin: None,
        },
    )
}

pub(super) struct CaptureMoment {
    round: usize,
    phase: ReplayPhase,
    failed_pin: Option<u32>,
}
impl CaptureMoment {
    pub(super) fn post_failure(round: usize, failed_pin: u32) -> Self {
        Self {
            round,
            phase: ReplayPhase::PostFailure,
            failed_pin: Some(failed_pin),
        }
    }
}

pub(super) fn capture_at_phase(
    router: &Router<'_>,
    net: u32,
    terminals: &[&Terminal],
    plans: &HashMap<u32, Plan>,
    strict: bool,
    rips: &BTreeMap<u32, usize>,
    moment: CaptureMoment,
) -> Result<()> {
    let CaptureMoment {
        round,
        phase,
        failed_pin,
    } = moment;
    let Some(policy) = &router.limits.replay else {
        return Ok(());
    };
    if policy.phase != phase
        || policy.net.is_some_and(|value| value != net)
        || policy.round.is_some_and(|value| value != round)
    {
        return Ok(());
    }
    policy.validate()?;
    if terminals.len() > MAX_TERMINALS || rips.len() > MAX_TERMINALS {
        return Err(budget("replay terminal/ripup state count"));
    }
    let selected = terminals
        .iter()
        .map(|t| {
            plans
                .get(&t.pin)
                .map(|p| (t.pin, p))
                .ok_or_else(|| invalid("replay terminal plan missing"))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let capture = Capture {
        schema: "rustroad.route-replay.v1",
        phase,
        failed_pin,
        input_sha256: input_digest(router.input)?,
        limits: router.limits,
        net,
        round,
        strict,
        terminal_order: terminals.iter().map(|t| t.pin).collect(),
        plans: selected,
        rips,
        spatial: router.spatial.capture()?,
        history: router.history.capture()?,
        guide_marks: &router.gmark,
        guide_epoch: router.gepoch,
        search_epoch: router.epoch,
        expansions: router.expansions,
        access_entries: router.access_entries,
        access_cache_bytes: router.access_cache_bytes,
        guide_bounds: guide_bounds(router)?.0,
    };
    let parent = policy
        .path
        .parent()
        .ok_or_else(|| invalid("replay output parent"))?;
    std::fs::create_dir_all(parent).map_err(|e| invalid(&format!("replay directory: {e}")))?;
    let temporary = policy
        .path
        .with_extension(format!("replay-{}.tmp", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| invalid(&format!("replay temporary file: {e}")))?;
    let mut writer = BoundedWriter {
        writer: BufWriter::with_capacity(65536, file),
        remaining: policy.max_bytes,
    };
    let written = serde_json::to_writer(&mut writer, &capture)
        .map_err(|e| budget(&format!("replay encoding: {e}")))
        .and_then(|()| {
            writer
                .flush()
                .map_err(|e| invalid(&format!("replay flush: {e}")))
        })
        .and_then(|()| {
            writer
                .writer
                .get_ref()
                .sync_all()
                .map_err(|e| invalid(&format!("replay sync: {e}")))
        });
    drop(writer);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    // A hard link publishes atomically without overwriting an existing artifact.
    let published = std::fs::hard_link(&temporary, &policy.path)
        .map_err(|e| invalid(&format!("replay publication (no overwrite): {e}")));
    let _ = std::fs::remove_file(&temporary);
    published?;
    eprintln!(
        "[troute] captured {phase:?} state net {net} round {round} failed_pin {failed_pin:?} guide {:?} at {}",
        capture.guide_bounds,
        policy.path.display()
    );
    Ok(())
}

/// Rebuild exactly one captured search without publishing a workspace revision.
///
/// # Errors
/// Rejects stale input views, malformed/budget-exceeding state and unsupported overrides.
pub fn replay(
    input: &RouteInput,
    limits: &RouteLimits,
    path: &Path,
    options: &ReplayOptions,
) -> Result<ReplayReport> {
    let file = File::open(path).map_err(|e| invalid(&format!("replay open: {e}")))?;
    let bytes = file
        .metadata()
        .map_err(|e| invalid(&format!("replay metadata: {e}")))?
        .len();
    if bytes == 0 || bytes > MAX_BYTES {
        return Err(budget("replay input byte limit"));
    }
    // 8x encoded state admits JSON allocation headroom before constructing the index/search.
    let requested_search = options
        .max_search_expansions
        .unwrap_or(limits.max_search_expansions);
    if options.probe_search_frontier && !options.hard_search {
        return Err(invalid(
            "search frontier diagnostics require hard-search mode",
        ));
    }
    if options.allowed_victim.is_some()
        && (options.max_search_expansions.is_none()
            || options.hard_search
            || options.probe_terminals)
    {
        return Err(invalid(
            "single-owner replay needs an explicit bound and soft search, without terminal probes",
        ));
    }
    if options.hard_search && (options.max_search_expansions.is_none() || options.probe_terminals) {
        return Err(invalid(
            "hard replay requires an explicit per-search bound and cannot probe terminals",
        ));
    }
    if let Some(config) = &options.single_owner_repair {
        config.validate(limits)?;
        if options.allowed_victim.is_some()
            || options.hard_search
            || options.probe_terminals
            || options.probe_search_frontier
        {
            return Err(invalid(
                "single-owner repair stage conflicts with another replay mode",
            ));
        }
    }
    if requested_search == 0 || requested_search > RouteLimits::ceiling().max_search_expansions {
        return Err(budget("replay per-search override outside hard ceiling"));
    }
    if options
        .heuristic_weight
        .is_some_and(|weight| !(1..=16).contains(&weight))
    {
        return Err(invalid("replay heuristic weight must be in 1..=16"));
    }
    let estimate = bytes
        .saturating_mul(8)
        .saturating_add(crate::routing::input_heap_estimate(input).saturating_mul(2))
        .saturating_add((requested_search as u64).saturating_mul(80))
        .saturating_add((limits.max_heap as u64).saturating_mul(128))
        .saturating_add(if options.probe_search_frontier {
            1 << 20
        } else {
            0
        })
        .saturating_add(options.single_owner_repair.as_ref().map_or(0, |config| {
            config.working_bytes(
                limits,
                input.vias.iter().map(|v| v.shapes.len()).max().unwrap_or(1),
            )
        }));
    if estimate > 4 << 30 {
        return Err(budget("replay decode/search estimate exceeds 4 GiB"));
    }
    let state: Snapshot = serde_json::from_reader(BufReader::new(file.take(MAX_BYTES)))
        .map_err(|e| invalid(&format!("replay decoding: {e}")))?;
    if state.schema != "rustroad.route-replay.v1" || state.input_sha256 != input_digest(input)? {
        return Err(invalid("REPLAY_INPUT_CHANGED: source compute view differs"));
    }
    if options.allowed_victim.is_some_and(|owner| {
        owner == state.net || !input.terminals.iter().any(|terminal| terminal.net == owner)
    }) {
        return Err(invalid(
            "single-owner replay requires a different existing net",
        ));
    }
    if (state.phase == ReplayPhase::PreSoft && state.failed_pin.is_some())
        || (state.phase == ReplayPhase::PostFailure && state.failed_pin.is_none())
    {
        return Err(invalid("replay phase/failure binding differs"));
    }
    let mut original = state.limits.clone();
    original.replay = None;
    let mut current = limits.clone();
    current.replay = None;
    // Batch orchestration deducts already-consumed aggregate work before
    // entering this router. Reuse exactly that smaller remaining allowance.
    if original.max_comparisons > current.max_comparisons
        || original.max_expansions > current.max_expansions
    {
        return Err(invalid(
            "REPLAY_LIMITS_CHANGED: captured aggregate budget exceeds supplied outer limit",
        ));
    }
    current.max_comparisons = original.max_comparisons;
    current.max_expansions = original.max_expansions;
    if serde_json::to_value(&original).map_err(|e| invalid(&e.to_string()))?
        != serde_json::to_value(&current).map_err(|e| invalid(&e.to_string()))?
    {
        return Err(invalid(
            "REPLAY_LIMITS_CHANGED: use explicit bounded overrides",
        ));
    }
    if let Some(maximum) = options.max_search_expansions {
        current.max_search_expansions = maximum;
        if options.hard_search {
            current.max_hard_search_expansions = Some(maximum);
        }
    }
    current.validate_track()?;
    let plan = input
        .tracks
        .as_ref()
        .ok_or_else(|| invalid("track replay requires track plan"))?;
    let graph = Graph::new(input, plan, &current)?;
    let cells = Cells::new(input.die, plan.gcell)?;
    let nodes = graph.len;
    if state.guide_marks.len() != cells.len()
        || state.guide_epoch == 0
        || state.history.len() > MAX_HISTORY
        || state.terminal_order.len() < 2
        || state.terminal_order.len() > MAX_TERMINALS
        || state.plans.len() != state.terminal_order.len()
        || state.rips.len() > MAX_TERMINALS
        || state.expansions > current.max_expansions
    {
        return Err(budget("replay source state counts"));
    }
    let mut router = Router::prepared(input, &current, graph, cells)?;
    router.heuristic_weight_override = options.heuristic_weight;
    router.allowed_victim = options.allowed_victim;
    router.spatial = Spatial::restore_capture(input, &current, state.spatial)?;
    router.history = History::restore_capture(
        nodes,
        current
            .max_expansions
            .min(nodes)
            .min(plan.max_history_entries),
        &state.history,
    )?;
    router.gmark = state.guide_marks;
    router.gepoch = state.guide_epoch;
    router.epoch = state.search_epoch;
    router.expansions = state.expansions;
    router.access_entries = state.access_entries;
    router.access_cache_bytes = state.access_cache_bytes;
    let by_pin: BTreeMap<_, _> = input.terminals.iter().map(|t| (t.pin, t)).collect();
    let unique: std::collections::BTreeSet<_> = state.terminal_order.iter().copied().collect();
    if unique.len() != state.terminal_order.len()
        || input
            .terminals
            .iter()
            .filter(|t| t.net == state.net)
            .count()
            != unique.len()
    {
        return Err(invalid("replay terminal set differs from complete net"));
    }
    let mut terminals = Vec::new();
    for pin in &state.terminal_order {
        let terminal = by_pin
            .get(pin)
            .copied()
            .ok_or_else(|| invalid("replay terminal missing"))?;
        let escape = state
            .plans
            .get(pin)
            .ok_or_else(|| invalid("replay escape missing"))?;
        if terminal.net != state.net
            || escape.access.net != state.net
            || escape.access.pin != *pin
            || escape.top as usize >= nodes
        {
            return Err(invalid("replay terminal/escape binding"));
        }
        if escape.portal_shapes as usize > escape.shapes.len()
            || escape.access.layer as usize >= input.layers.len()
            || escape
                .wires
                .len()
                .saturating_add(escape.vias.len())
                .saturating_add(escape.shapes.len())
                > current.max_active_nodes
            || escape
                .wires
                .iter()
                .any(|w| w.net != state.net || w.layer as usize >= input.layers.len())
            || escape
                .vias
                .iter()
                .any(|v| v.net != state.net || v.kind as usize >= input.vias.len())
            || escape
                .shapes
                .iter()
                .any(|&id| id < input.obstacles.len() || id >= router.spatial.len())
        {
            return Err(invalid("replay escape geometry/handles"));
        }
        terminals.push(terminal);
    }
    if let Some(bounds) = options.guide_bounds {
        if !bounds.valid()
            || bounds.lx < input.die.lx
            || bounds.ly < input.die.ly
            || bounds.ux > input.die.ux
            || bounds.uy > input.die.uy
        {
            return Err(invalid("replay guide rectangle outside die"));
        }
        router.gmark.fill(0);
        router.gepoch = 1;
        for i in 0..router.gmark.len() {
            let cell = cell_rect(&router.cells, i, input.die)?;
            if cell.lx < bounds.ux
                && bounds.lx < cell.ux
                && cell.ly < bounds.uy
                && bounds.ly < cell.uy
            {
                router.gmark[i] = 1;
            }
        }
    }
    let (used_guide_bounds, used_guide_cells) = guide_bounds(&router)?;
    let work = router.spatial.work;
    let terminal_probes = options
        .probe_terminals
        .then(|| probe_terminals(&mut router, &state.plans, state.net))
        .transpose()?;
    let plans: HashMap<_, _> = state.plans.into_iter().collect();
    let (status, failed_pin, wires, vias, victims) = if options.probe_terminals {
        ("geometry_probed", None, None, None, Vec::new())
    } else {
        let mut owner_allocation = options
            .single_owner_repair
            .as_ref()
            .map(owner_repair::Allocation::new);
        let staged = if let Some(config) = &options.single_owner_repair {
            match router.route_net(
                state.net,
                &terminals,
                &plans,
                false,
                state.strict,
                &state.rips,
                state.round,
            )? {
                Outcome::Routed(route, victims) => Some((route, victims)),
                Outcome::Limited(_) => None,
                Outcome::Failed(failed_pin) => owner_repair::run(
                    &mut router,
                    &owner_repair::Request {
                        config,
                        net: state.net,
                        terminals: &terminals,
                        plans: &plans,
                        strict: state.strict,
                        rips: &state.rips,
                        round: state.round,
                        failed_pin,
                    },
                    owner_allocation.as_mut().expect("owner allocation"),
                )?,
            }
        } else {
            None
        };
        let outcome = if let Some((route, victims)) = staged {
            Outcome::Routed(route, victims)
        } else if let Some(config) = &options.single_owner_repair {
            owner_repair::soft(
                &mut router,
                &owner_repair::Request {
                    config,
                    net: state.net,
                    terminals: &terminals,
                    plans: &plans,
                    strict: state.strict,
                    rips: &state.rips,
                    round: state.round,
                    failed_pin: terminals[0].pin,
                },
                owner_allocation.as_mut().expect("owner allocation"),
            )?
        } else {
            router.route_net(
                state.net,
                &terminals,
                &plans,
                !options.hard_search,
                state.strict,
                &state.rips,
                state.round,
            )?
        };
        match outcome {
            Outcome::Limited(pin) => ("budget_exceeded", Some(pin), None, None, Vec::new()),
            Outcome::Failed(pin) => ("no_route", Some(pin), None, None, Vec::new()),
            Outcome::Routed(route, victims) => {
                ("routed", None, Some(route.wires), Some(route.vias), victims)
            }
        }
    };
    if options
        .allowed_victim
        .is_some_and(|allowed| victims.iter().any(|&net| net != allowed))
    {
        return Err(invalid("single-owner replay displaced a forbidden owner"));
    }
    let (search_frontier, search_frontier_work, boundary_owners) = if options.probe_search_frontier
    {
        let work_before = router.spatial.work;
        let (states, work) = router.search.capture_bounded(4096)?;
        router.spatial.work = router
            .spatial
            .work
            .checked_add(work)
            .filter(|&n| n <= current.max_comparisons)
            .ok_or_else(|| budget("search frontier diagnostic work"))?;
        let owners = owner_repair::probe_boundary_owners(&mut router, &states, state.net)?
            .ok_or_else(|| budget("boundary owner report cap"))?;
        let records: Vec<_> = states
            .into_iter()
            .map(|(node, state)| {
                let (layer, at) = router.graph.point(node);
                SearchFrontierNode {
                    node,
                    layer,
                    at,
                    cost: state.cost,
                    parent: (state.parent != u32::MAX).then_some(state.parent),
                    incoming_kind: state.kind,
                }
            })
            .collect();
        let work = router.spatial.work - work_before;
        (Some(records), Some(work), Some(owners))
    } else {
        (None, None, None)
    };
    Ok(ReplayReport {
        schema: "rustroad.route-replay-result.v1",
        status,
        search_mode: if options.probe_terminals {
            "terminal_probe"
        } else if options.hard_search {
            "hard_no_ripup"
        } else if options.allowed_victim.is_some() {
            "soft_single_owner"
        } else if options
            .single_owner_repair
            .as_ref()
            .is_some_and(|c| c.post_soft_refinement.is_some())
        {
            "bounded_single_owner_soft_refinement"
        } else if options.single_owner_repair.is_some() {
            "bounded_single_owner_then_soft"
        } else {
            "soft_ripup"
        },
        scope: match (options.probe_terminals, options.hard_search, state.phase) {
            (true, _, _) => {
                "immediate terminal wire/via geometry only; no path search, revision or signoff"
            }
            (false, true, _) => {
                "one hard search from captured state; no revision or full-design signoff"
            }
            (false, false, _) if options.allowed_victim.is_some() => {
                "counterfactual search permitting one routed owner; no revision or full-design signoff"
            }
            (false, false, _) if options.single_owner_repair.is_some() => {
                "bounded repair selection from captured state; no revision or full-design signoff"
            }
            (false, false, ReplayPhase::PreSoft) => {
                "one captured pre-soft search; no revision or full-design signoff"
            }
            (false, false, ReplayPhase::PostFailure) => {
                "new attempt from post-failure state; original failed call is not replayed; no revision or signoff"
            }
        },
        source_phase: state.phase,
        source_failed_pin: state.failed_pin,
        input_sha256: state.input_sha256,
        net: state.net,
        round: state.round,
        failed_pin,
        terminal_order: state.terminal_order,
        source_guide_bounds: state.guide_bounds,
        used_guide_bounds,
        used_guide_cells,
        configured_heuristic_weight: plan.heuristic_weight,
        configured_repair_heuristic_weight: plan.repair_heuristic_weight,
        used_heuristic_weight: router.detail_weight(true),
        terminal_probes,
        expansions_before: state.expansions,
        expansions_after: router.expansions,
        comparisons_before: work,
        comparisons_after: router.spatial.work,
        wires,
        vias,
        victims,
        search_frontier,
        search_frontier_work,
        boundary_owners,
        allowed_victim: options.allowed_victim,
        single_owner_repair: router.single_owner_stats.clone(),
    })
}
