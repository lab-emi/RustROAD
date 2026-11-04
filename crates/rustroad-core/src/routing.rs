//! Deterministic, bounded rectilinear routing on a finite coordinate lattice.
//! This compute view is separate from mutable netlist arenas. All geometry is
//! integer DBU. Resource preflight: docs/routing-budget.md.
//!
//! With `RouteInput::rules` set, the router also uses every supplied fixed-via
//! variant per layer pair, a per-(master, orientation, pin) access table and a
//! bounded post-route same-net pass (dangling-stub pruning, notch patches and
//! minimum-area stubs, see `route_rules`). Without it, behavior is unchanged.
pub use crate::instance_obstacles::{
    InstanceObstacles, ObstacleInstance, ObstacleTemplate, TemplateObstacle,
};
use crate::route_rules;
pub(crate) use crate::route_rules::Violation;
pub use crate::troute::replay::{
    BudgetErrorContext, BudgetErrorStage, ReplayCapture, ReplayOptions, ReplayPhase, ReplayReport,
};
use crate::{Error, Rect, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLimits {
    /// Optional bounded diagnostic snapshot; routing semantics are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay: Option<ReplayCapture>,
    /// Explicit staged search work allowances. Omission preserves legacy scouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scouts: Option<ScoutLimits>,
    /// Whole-net budget for immediately reconnecting one displaced routed net.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_repair_expansions: Option<usize>,
    /// Maximum displaced nets eligible for eager reconnect; omission means one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_repair_max_victims: Option<usize>,
    /// Per-pin search cap inside one eager reconnect. Omission keeps the
    /// smaller of the whole-net allowance and the ordinary hard-search cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_repair_search_expansions: Option<usize>,
    /// Bound on the length of a net re-routed after rip-up, relative to its
    /// first routed length. Omission preserves legacy acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_detour: Option<DetourBound>,
    pub max_axis: usize,
    pub max_grid_nodes: usize,
    pub max_obstacles: usize,
    pub max_bin_entries: usize,
    pub max_segments: usize,
    pub max_expansions: usize,
    pub max_search_expansions: usize,
    /// Optional earlier handoff from hard search to bounded routing repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hard_search_expansions: Option<usize>,
    /// Nodes or raw edges retained for one detailed net, independently of the design output size.
    #[serde(default = "default_active_nodes")]
    pub max_active_nodes: usize,
    /// Exact routing-tree segments retained by the bounded distance index.
    #[serde(default = "default_target_segments")]
    pub max_target_segments: usize,
    /// Retries that may reopen earlier nets after a later pin loses access.
    #[serde(default = "default_batch_retries")]
    pub max_batch_retries: usize,
    /// Distinct completed nets reopened during one batch.
    #[serde(default = "default_batch_displaced_nets")]
    pub max_batch_displaced_nets: usize,
    /// Total terminals, including reopened nets, admitted to a repair batch.
    #[serde(default = "default_batch_repair_terminals")]
    pub max_batch_repair_terminals: usize,
    /// Retained same-net violations per check; exceeding the limit is an error.
    #[serde(default = "default_rule_violations")]
    pub max_rule_violations: usize,
    /// Cells in one coordinate-compressed hole-detection grid.
    #[serde(default = "default_rule_grid_cells")]
    pub max_rule_grid_cells: usize,
    pub max_comparisons: usize,
    pub max_heap: usize,
}

/// Repair-time detour bound. A hard re-route of a net that already had a
/// route is rejected when its wire length exceeds `percent`% of the first
/// routed length plus `slack` DBU; the net then negotiates through soft
/// rip-up instead. Rejections are bounded per net and in total, after which
/// re-routes are accepted unbounded, so routability never depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetourBound {
    pub percent: u64,
    pub slack: u64,
    pub max_rejections_per_net: usize,
    pub max_rejections: usize,
}
impl DetourBound {
    fn validate(&self) -> Result<()> {
        if !(100..=1000).contains(&self.percent)
            || self.slack > 1_000_000_000
            || !(1..=64).contains(&self.max_rejections_per_net)
            || !(1..=10_000_000).contains(&self.max_rejections)
        {
            return Err(invalid("repair detour bound"));
        }
        Ok(())
    }
    /// Largest admitted length for a net first routed with `first` DBU.
    pub fn allowed(&self, first: u64) -> u64 {
        first
            .saturating_mul(self.percent)
            .saturating_div(100)
            .saturating_add(self.slack)
    }
}

/// Work budgets for staged routing attempts; retained storage stays separately bounded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoutLimits {
    /// Expanded-guide attempt that does not displace routed owners.
    pub wide_hard_expansions: usize,
    /// First ordinary soft-search stage per sink.
    pub soft_expansions: usize,
    /// Clear-prefix search that may reuse already selected owners only.
    pub reuse_expansions: usize,
    /// Whole-net allowance for the optional non-growing fast candidate.
    pub fast_expansions: usize,
    /// First constrained owner attempt, also capped by its owner allocation.
    pub owner_first_expansions: usize,
    /// Promoted constrained owner attempt, also capped by its owner allocation.
    pub owner_progress_expansions: usize,
}
fn default_rule_grid_cells() -> usize {
    1_000_000
}

fn default_rule_violations() -> usize {
    100_000
}

fn default_active_nodes() -> usize {
    200_000
}
fn default_target_segments() -> usize {
    8192
}
fn default_batch_retries() -> usize {
    16
}
fn default_batch_displaced_nets() -> usize {
    1024
}
fn default_batch_repair_terminals() -> usize {
    1_000_000
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            replay: None,
            scouts: None,
            eager_repair_expansions: None,
            eager_repair_max_victims: None,
            eager_repair_search_expansions: None,
            repair_detour: None,
            max_axis: 1024,
            max_grid_nodes: 2_000_000,
            max_obstacles: 200_000,
            max_bin_entries: 1_000_000,
            max_segments: 100_000,
            max_expansions: 2_000_000,
            max_search_expansions: 200_000,
            max_hard_search_expansions: None,
            max_active_nodes: default_active_nodes(),
            max_target_segments: default_target_segments(),
            max_batch_retries: default_batch_retries(),
            max_batch_displaced_nets: default_batch_displaced_nets(),
            max_batch_repair_terminals: default_batch_repair_terminals(),
            max_rule_violations: default_rule_violations(),
            max_rule_grid_cells: default_rule_grid_cells(),
            max_comparisons: 200_000_000,
            max_heap: 500_000,
        }
    }
}
impl RouteLimits {
    /// Absolute ceilings for the track router (`RouteInput::tracks`). The
    /// lattice router keeps its defaults as hard caps. The router also checks
    /// its upfront memory estimate against `TrackPlan::memory_mib`.
    pub fn ceiling() -> Self {
        Self {
            replay: None,
            scouts: None,
            eager_repair_expansions: None,
            eager_repair_max_victims: None,
            eager_repair_search_expansions: None,
            repair_detour: None,
            max_axis: 1 << 20,
            max_grid_nodes: u32::MAX as usize - 1,
            max_obstacles: 256_000_000,
            max_bin_entries: 512_000_000,
            max_segments: 128_000_000,
            // Work quotas do not size retained search storage. Memory admission
            // and the independent active-node/heap limits still bound allocation.
            max_expansions: 1usize << 40,
            max_search_expansions: 500_000_000,
            max_hard_search_expansions: None,
            max_active_nodes: 50_000_000,
            max_target_segments: 1_000_000,
            max_batch_retries: 128,
            max_batch_displaced_nets: 4096,
            max_batch_repair_terminals: 1_000_000,
            max_rule_violations: 1_000_000,
            max_rule_grid_cells: 4_000_000,
            max_comparisons: 1usize << 50,
            max_heap: 64_000_000,
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_against(&Self::default())
    }
    /// Track-mode bounds: every limit must be positive and at most `ceiling()`.
    pub fn validate_track(&self) -> Result<()> {
        self.validate_against(&Self::ceiling())
    }
    fn validate_against(&self, hard: &Self) -> Result<()> {
        if let Some(capture) = &self.replay {
            capture.validate()?;
        }
        if let Some(bound) = &self.repair_detour {
            bound.validate()?;
        }
        if self.max_batch_retries > hard.max_batch_retries {
            return Err(budget("batch retry limit outside hard bounds"));
        }
        if self
            .max_hard_search_expansions
            .is_some_and(|n| n == 0 || n > self.max_search_expansions)
        {
            return Err(budget("hard search limit exceeds per-search bounds"));
        }
        if let Some(scouts) = &self.scouts {
            for work in [
                scouts.wide_hard_expansions,
                scouts.soft_expansions,
                scouts.reuse_expansions,
                scouts.fast_expansions,
                scouts.owner_first_expansions,
                scouts.owner_progress_expansions,
            ] {
                if work == 0 || work > self.max_search_expansions {
                    return Err(budget("scout limit exceeds per-search bounds"));
                }
            }
        }
        if self
            .eager_repair_expansions
            .is_some_and(|n| n == 0 || n > self.max_search_expansions)
        {
            return Err(budget("eager repair limit exceeds per-search bounds"));
        }
        if self
            .eager_repair_max_victims
            .is_some_and(|n| !(1..=16).contains(&n) || self.eager_repair_expansions.is_none())
        {
            return Err(budget(
                "eager victim limit requires work allowance and 1..16 victims",
            ));
        }
        if self.eager_repair_search_expansions.is_some_and(|n| {
            n == 0
                || self
                    .eager_repair_expansions
                    .is_none_or(|allowance| n > allowance)
        }) {
            return Err(budget(
                "eager search limit requires work allowance and must not exceed it",
            ));
        }
        for (a, b) in [
            (self.max_axis, hard.max_axis),
            (self.max_grid_nodes, hard.max_grid_nodes),
            (self.max_obstacles, hard.max_obstacles),
            (self.max_bin_entries, hard.max_bin_entries),
            (self.max_segments, hard.max_segments),
            (self.max_expansions, hard.max_expansions),
            (self.max_search_expansions, hard.max_search_expansions),
            (self.max_active_nodes, hard.max_active_nodes),
            (self.max_target_segments, hard.max_target_segments),
            (self.max_batch_displaced_nets, hard.max_batch_displaced_nets),
            (
                self.max_batch_repair_terminals,
                hard.max_batch_repair_terminals,
            ),
            (self.max_rule_violations, hard.max_rule_violations),
            (self.max_rule_grid_cells, hard.max_rule_grid_cells),
            (self.max_comparisons, hard.max_comparisons),
            (self.max_heap, hard.max_heap),
        ] {
            if a == 0 || a > b {
                return Err(budget("routing limit outside hard bounds"));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteLayer {
    pub name: String,
    pub width: i64,
    pub spacing: i64,
    /// None is a cut layer; true is preferred vertical, false horizontal.
    pub vertical: Option<bool>,
    /// LEF AREA in DBU^2 (rules mode only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area: Option<i64>,
    /// LEF MINSTEP in DBU (rules mode, pin-access landing pads only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_step: Option<i64>,
    /// Wide-metal row of the LEF SPACINGTABLE (configured; checked after cleanup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wide_spacing: Option<WideSpacing>,
}
/// Metal wider than `width` needs `spacing` to other nets wherever the two
/// run parallel for more than `parallel_run` (DBU).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WideSpacing {
    pub width: i64,
    pub parallel_run: i64,
    pub spacing: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerRect {
    pub layer: u32,
    pub rect: Rect,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteVia {
    pub name: String,
    pub lower: u32,
    pub upper: u32,
    pub shapes: Vec<LayerRect>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Blockage {
    pub layer: u32,
    pub rect: Rect,
    pub net: Option<u32>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Terminal {
    pub pin: u32,
    pub net: u32,
    pub driver: bool,
    pub shapes: Vec<LayerRect>,
    /// Access-class index and instance origin; shapes = class shapes + origin.
    pub class: Option<(u32, [i64; 2])>,
}
/// One (master, orientation, pin) access geometry relative to the origin.
#[derive(Clone, Debug, Serialize)]
pub struct AccessClass {
    pub shapes: Vec<LayerRect>,
    /// Other-net geometry of the same master (other pins, OBS) on selected layers.
    pub blockers: Vec<LayerRect>,
}
/// Per-layer track grid and router parameters of the track router.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrackPlan {
    /// Indexed like `RouteInput::layers`: `(pitch, offset)` of the cross-axis
    /// track coordinates (vertical layer: x = offset + k * pitch); `None` for
    /// cut layers.
    pub tracks: Vec<Option<(i64, i64)>>,
    /// Inclusive signal-routing layer indices; lower layers remain available
    /// to fixed pin escapes. `None` retains all selected routing layers.
    pub routing_layers: Option<[u32; 2]>,
    /// Square GCell side in DBU for global routing.
    pub gcell: i64,
    pub global_iterations: usize,
    /// Fraction (percent) of free tracks a GCell edge offers global routing.
    pub capacity_percent: u32,
    /// GCells added around every global route when building guides.
    pub guide_margin: usize,
    /// Extra GCells for the second detailed attempt outside the guide.
    pub expand_margin: usize,
    pub max_ripups_per_net: usize,
    pub max_repair_rounds: usize,
    /// Path cost of one via, DBU-equivalent.
    pub via_cost: u64,
    /// Cost multiplier of non-preferred-direction wire edges.
    pub wrong_way_factor: u64,
    /// Integer multiplier of the detailed-search lower bound (1..=16).
    /// Values above one favor progress over shortest-path optimality.
    pub heuristic_weight: u64,
    /// Optional priority weight for soft rip-up searches only (1..=16).
    /// Omitted values retain the normal detailed-search weight and encoding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair_heuristic_weight: Option<u64>,
    /// Optional bounded one-owner trials before ordinary soft rip-up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_owner_repair: Option<SingleOwnerRepairConfig>,
    /// Exact per-layer own-pin range and bounding-box queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_pin_index: Option<OwnPinIndex>,
    /// Exact static-instance bins restricted to template layer presence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_index: Option<InstanceIndexMode>,
    /// Upfront memory estimate ceiling for graph, guides and search state.
    pub memory_mib: u64,
    /// Separate cap for cumulative congestion-history entries.
    pub max_history_entries: usize,
    /// Require the minimum-area run before a routed via stacks through a
    /// layer in every search, not only in strict re-routes after cleanup.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub area_stacks: bool,
}

/// Explicit bounds for one-owner trials. None in TrackPlan preserves legacy
/// routing and snapshot encoding; all trials share the router's real ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleOwnerRepairConfig {
    pub max_candidates: usize,
    pub max_frontier_nodes: usize,
    pub first_search_expansions: usize,
    pub max_progress_candidates: usize,
    pub progress_search_expansions: usize,
    pub max_total_expansions: usize,
    /// Refine a completed multi-owner soft candidate under the same local
    /// allocation. Omitted values preserve the prior route and encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_soft_refinement: Option<PostSoftRefinement>,
    /// Optional priority weight for constrained one-owner trials only.
    /// More specific than a replay-wide weight; omission preserves its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heuristic_weight: Option<u64>,
}

/// Additional selection after ordinary soft routing has supplied real owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostSoftRefinement {
    /// Try one observed owner at a time, keeping the original route on failure.
    SingleOwner,
}
impl SingleOwnerRepairConfig {
    /// Validate explicit local caps without enlarging the original global cap.
    pub fn validate(&self, limits: &RouteLimits) -> Result<()> {
        if !(1..=16).contains(&self.max_candidates)
            || !(1..=4096).contains(&self.max_frontier_nodes)
            || !(1..=500_000).contains(&self.first_search_expansions)
            || self.max_progress_candidates > 4
            || self.progress_search_expansions < self.first_search_expansions
            || self.progress_search_expansions > 2_000_000
            || self.progress_search_expansions > limits.max_search_expansions
            || self.max_total_expansions == 0
            || self.max_total_expansions > 20_000_000
            || self
                .heuristic_weight
                .is_some_and(|weight| !(1..=16).contains(&weight))
        {
            return Err(invalid("single-owner repair limits"));
        }
        Ok(())
    }

    pub(crate) fn working_bytes(&self, limits: &RouteLimits, max_via_shapes: usize) -> u64 {
        let base = (self.max_frontier_nodes as u64)
            .saturating_mul(256)
            .saturating_add((limits.max_active_nodes as u64).saturating_mul(8))
            .saturating_add(64 << 10);
        if self.post_soft_refinement.is_none() {
            return base;
        }
        // One retained fallback, its shape handles and reversible history
        // coexist with the existing trial scratch. Account for Vec growth.
        let per_node = (std::mem::size_of::<Wire>() as u64)
            .saturating_add(std::mem::size_of::<Via>() as u64)
            .saturating_add((max_via_shapes.max(1) as u64).saturating_mul(8))
            .saturating_add(std::mem::size_of::<(u32, u16)>() as u64)
            .saturating_mul(2);
        // A rejected fast candidate can leave dead spatial slots until the
        // next compaction. Its wire/via vectors are dropped before refinement.
        let fast_staging = if self.heuristic_weight.is_some() {
            (limits.max_active_nodes as u64)
                .saturating_mul((max_via_shapes as u64).saturating_add(1))
                .saturating_mul(80)
        } else {
            0
        };
        base.saturating_add((limits.max_active_nodes as u64).saturating_mul(per_node))
            .saturating_add(fast_staging)
    }
}

/// Measurements of refinement over an already completed soft candidate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostSoftRefinementStats {
    pub stages: usize,
    pub candidate_owners: usize,
    pub attempts: usize,
    pub successes: usize,
    pub fallbacks: usize,
    pub quota_skips: usize,
}
impl PostSoftRefinementStats {
    fn merge(&mut self, other: &Self) -> Result<()> {
        let sum = |a: usize, b: usize| {
            a.checked_add(b)
                .ok_or_else(|| budget("post-soft telemetry overflow"))
        };
        *self = Self {
            stages: sum(self.stages, other.stages)?,
            candidate_owners: sum(self.candidate_owners, other.candidate_owners)?,
            attempts: sum(self.attempts, other.attempts)?,
            successes: sum(self.successes, other.successes)?,
            fallbacks: sum(self.fallbacks, other.fallbacks)?,
            quota_skips: sum(self.quota_skips, other.quota_skips)?,
        };
        Ok(())
    }
}

/// Measured repair-stage counters; absent when the stage was not enabled.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleOwnerRepairStats {
    pub stages: usize,
    pub candidate_owners: usize,
    pub attempts: usize,
    pub successes: usize,
    pub frontier_skips: usize,
    pub local_budget_exhaustions: usize,
    pub trial_expansions: usize,
    pub trial_comparisons: usize,
    pub committed_history_updates: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_soft_refinement: Option<PostSoftRefinementStats>,
    /// Explicit one-owner override actually used; absent in legacy operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heuristic_weight: Option<u64>,
}
impl SingleOwnerRepairStats {
    pub(crate) fn merge(&mut self, other: &Self) -> Result<()> {
        let sum = |a: usize, b: usize| {
            a.checked_add(b)
                .ok_or_else(|| budget("single-owner telemetry overflow"))
        };
        let mut post = self.post_soft_refinement.clone();
        let heuristic_weight = match (self.heuristic_weight, other.heuristic_weight) {
            (Some(a), Some(b)) if a != b => {
                return Err(invalid("inconsistent owner priority telemetry"));
            }
            (a, b) => a.or(b),
        };
        if let Some(value) = &other.post_soft_refinement {
            post.get_or_insert_with(Default::default).merge(value)?;
        }
        let next = Self {
            stages: sum(self.stages, other.stages)?,
            candidate_owners: sum(self.candidate_owners, other.candidate_owners)?,
            attempts: sum(self.attempts, other.attempts)?,
            successes: sum(self.successes, other.successes)?,
            frontier_skips: sum(self.frontier_skips, other.frontier_skips)?,
            local_budget_exhaustions: sum(
                self.local_budget_exhaustions,
                other.local_budget_exhaustions,
            )?,
            trial_expansions: sum(self.trial_expansions, other.trial_expansions)?,
            trial_comparisons: sum(self.trial_comparisons, other.trial_comparisons)?,
            committed_history_updates: sum(
                self.committed_history_updates,
                other.committed_history_updates,
            )?,
            post_soft_refinement: post,
            heuristic_weight,
        };
        *self = next;
        Ok(())
    }
}
impl TrackPlan {
    pub(crate) fn allows_routing_layer(&self, layer: u32) -> bool {
        self.routing_layers
            .is_none_or(|[lower, upper]| lower <= layer && layer <= upper)
    }
}
pub const DEFAULT_MAX_HISTORY_ENTRIES: usize = 4_000_000;
/// Optional exact own-pin broad phase; omission retains the legacy query ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnPinIndex {
    LayerRanges,
}

/// Optional static-index representation; obstacle geometry is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceIndexMode {
    TemplateLayers,
}

/// Explicit replay-only query representation, applied after source validation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryIndices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_pin_index: Option<OwnPinIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_index: Option<InstanceIndexMode>,
}
/// Process baseline counted in every route memory estimate (binary, libc,
/// stacks, thread pool).
pub const PROCESS_BASE_BYTES: u64 = 8 << 20;
/// Conservative pre-load allowance per on-disk workspace byte. This is also
/// the fallback on platforms without a process-footprint probe.
pub const WORKSPACE_LOAD_FACTOR: u64 = 3;

/// Bytes the caller holds while the router runs: the process baseline, the
/// loaded workspace (`workspace_bytes` on disk) and this input, counted
/// twice (vector growth and the builder's transient lookup tables). Used
/// before loading or when the platform cannot report the live footprint.
pub fn held_estimate(workspace_bytes: u64, input: &RouteInput) -> u64 {
    PROCESS_BASE_BYTES
        .saturating_add(workspace_bytes.saturating_mul(WORKSPACE_LOAD_FACTOR))
        .saturating_add(input_heap_estimate(input).saturating_mul(2))
}

/// After the complete input has been decoded, account for the live process
/// rather than charging it for already-freed builder maps and metadata clones.
/// Add 25% allocator/page headroom; never fall below the input's structural
/// allowance. Callers must include swapped pages in `live_process_bytes`.
/// Future router allocations are separately covered by MemoryEstimate.
pub fn loaded_held_estimate(
    workspace_bytes: u64,
    input: &RouteInput,
    live_process_bytes: Option<u64>,
) -> u64 {
    match live_process_bytes.filter(|&n| n > 0) {
        Some(bytes) => bytes
            .saturating_add(bytes.div_ceil(4))
            .max(PROCESS_BASE_BYTES.saturating_add(input_heap_estimate(input).saturating_mul(2))),
        None => held_estimate(workspace_bytes, input),
    }
}

pub(crate) fn input_heap_estimate(input: &RouteInput) -> u64 {
    use std::mem::size_of;
    let shapes = |v: &[LayerRect]| std::mem::size_of_val(v) as u64;
    let terminals: u64 = input
        .terminals
        .iter()
        .map(|t| size_of::<Terminal>() as u64 + shapes(&t.shapes))
        .sum();
    let classes: u64 = input
        .access_classes
        .iter()
        .map(|c| size_of::<AccessClass>() as u64 + shapes(&c.shapes) + shapes(&c.blockers))
        .sum();
    // Obstacles plus the caller's parallel owner table (8 bytes each).
    let obstacles = (input.obstacles.len() * (size_of::<Blockage>() + 8)) as u64;
    let ties = (input.ties.len() * size_of::<SupplyTie>()) as u64;
    let instances = input
        .instances
        .as_ref()
        .map_or(0, InstanceObstacles::heap_bytes);
    obstacles
        .saturating_add(terminals)
        .saturating_add(classes)
        .saturating_add(ties)
        .saturating_add(instances)
}

/// Supply tie (rail-input tie cells): router net `net` holds a cell input
/// pin and a pseudo-driver on the cell's own rail rectangle `zone`; it is
/// not a logical net. A routed shape of `net` that overlaps `zone` may touch
/// (never merely approach) fixed shapes of the supply net `supply`.
#[derive(Clone, Debug, Serialize)]
pub struct SupplyTie {
    pub net: u32,
    pub supply: u32,
    pub zone: LayerRect,
}
#[derive(Serialize)]
pub struct RouteInput {
    pub die: Rect,
    pub grid: i64,
    pub step: i64,
    pub access_layer: u32,
    pub layers: Vec<RouteLayer>,
    pub vias: Vec<RouteVia>,
    pub obstacles: Vec<Blockage>,
    /// Shared cell templates and connectivity; translated only during queries.
    pub instances: Option<InstanceObstacles>,
    pub terminals: Vec<Terminal>,
    /// Enable LEF-rule mode (variants, access table, same-net cleanup/checks).
    pub rules: bool,
    pub access_classes: Vec<AccessClass>,
    /// Track router (per-layer track graph, global guides, rip-up and
    /// reroute). `None` keeps the lattice router.
    pub tracks: Option<TrackPlan>,
    /// Supply ties (track router only), sorted by `net`.
    pub ties: Vec<SupplyTie>,
    /// Bytes the caller already holds for this route (loaded workspace and
    /// this input), counted with the router's own estimate against the
    /// track plan's `memory_mib`.
    #[serde(skip)]
    pub held_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wire {
    pub net: u32,
    pub layer: u32,
    pub start: [i64; 2],
    pub end: [i64; 2],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Via {
    pub net: u32,
    pub kind: u32,
    pub at: [i64; 2],
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub pin: u32,
    pub net: u32,
    pub layer: u32,
    pub at: [i64; 2],
}
/// Same-net fill metal (DEF routing RECT); rules mode only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub net: u32,
    pub layer: u32,
    pub rect: Rect,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStats {
    pub grid_nodes: usize,
    pub expansions: usize,
    pub comparisons: usize,
    pub wire_length_dbu: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub single_owner_repair: Option<SingleOwnerRepairStats>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    pub wires: Vec<Wire>,
    pub vias: Vec<Via>,
    pub access: Vec<Access>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<Patch>,
    pub nets: Vec<u32>,
    pub stats: RouteStats,
}
/// Replay a single captured repair without changing a workspace or claiming signoff.
///
/// # Errors
/// Returns a diagnostic for stale views, invalid geometry and bounded work/memory failures.
pub fn replay_route(
    input: &RouteInput,
    limits: &RouteLimits,
    snapshot: &std::path::Path,
    options: &ReplayOptions,
) -> Result<ReplayReport> {
    crate::troute::replay::replay(input, limits, snapshot, options)
}

/// One `RUSTROAD_RULE_DUMP` file (see `troute::dump_rule_state`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleDump {
    schema: String,
    pass: usize,
    violations: Vec<serde_json::Value>,
    routes: Routes,
}

/// What a diagnostic cleanup replay left.
#[derive(Debug, Serialize)]
pub struct CleanupReplayReport {
    pub pass: usize,
    pub violations_before: usize,
    pub violations_after: usize,
    pub patches_added: usize,
    pub wires_added: usize,
    pub work: usize,
    /// Remaining violations by kind and layer.
    pub remaining: BTreeMap<String, usize>,
    pub first: Vec<String>,
}

/// Diagnostic only: re-run the bounded geometric cleanup on the routes of a
/// rule dump and report what remains. Never writes a routing revision.
///
/// # Errors
/// Returns a diagnostic for unreadable or oversized dumps and bounded work failures.
pub fn cleanup_replay(
    input: &RouteInput,
    limits: &RouteLimits,
    dump: &std::path::Path,
) -> Result<CleanupReplayReport> {
    let file = std::fs::File::open(dump).map_err(|e| invalid(&format!("rule dump open: {e}")))?;
    let bytes = file
        .metadata()
        .map_err(|e| invalid(&format!("rule dump metadata: {e}")))?
        .len();
    if bytes == 0 || bytes > 1 << 30 {
        return Err(budget("rule dump byte limit"));
    }
    let dump: RuleDump = serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|e| invalid(&format!("rule dump decode: {e}")))?;
    if dump.schema != "rustroad.rule-dump.v1" {
        return Err(invalid("rule dump schema"));
    }
    let mut routes = dump.routes;
    let (patches, wires) = (routes.patches.len(), routes.wires.len());
    let (work, left) = cleanup_report(input, &mut routes, limits)?;
    let mut remaining = BTreeMap::new();
    for v in &left {
        *remaining
            .entry(format!("{} layer {}", v.kind(), v.layer()))
            .or_insert(0) += 1;
    }
    Ok(CleanupReplayReport {
        pass: dump.pass,
        violations_before: dump.violations.len(),
        violations_after: left.len(),
        patches_added: routes.patches.len().saturating_sub(patches),
        wires_added: routes.wires.len().saturating_sub(wires),
        work,
        remaining,
        first: left.iter().take(64).map(|v| v.describe()).collect(),
    })
}

mod spatial_order;

pub(crate) fn budget(s: &str) -> Error {
    Error::Budget(s.into())
}
pub(crate) fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
pub(crate) fn no_route(s: &str) -> Error {
    Error::Invalid(format!("NO_ROUTE: {s}"))
}
pub(crate) fn int(v: i128) -> Result<i64> {
    i64::try_from(v).map_err(|_| invalid("routing coordinate overflow"))
}
pub(crate) fn expand(r: Rect, amount: i64) -> Result<Rect> {
    Ok(Rect {
        lx: int(i128::from(r.lx) - i128::from(amount))?,
        ly: int(i128::from(r.ly) - i128::from(amount))?,
        ux: int(i128::from(r.ux) + i128::from(amount))?,
        uy: int(i128::from(r.uy) + i128::from(amount))?,
    })
}
pub fn wire_rect(w: &Wire, width: i64) -> Result<Rect> {
    if width <= 0
        || width % 2 != 0
        || w.start == w.end
        || (w.start[0] != w.end[0] && w.start[1] != w.end[1])
    {
        return Err(invalid("non-Manhattan/zero wire or odd width"));
    }
    let h = i128::from(width / 2);
    Ok(Rect {
        lx: int(i128::from(w.start[0].min(w.end[0])) - h)?,
        ux: int(i128::from(w.start[0].max(w.end[0])) + h)?,
        ly: int(i128::from(w.start[1].min(w.end[1])) - h)?,
        uy: int(i128::from(w.start[1].max(w.end[1])) + h)?,
    })
}
pub fn shifted(r: Rect, at: [i64; 2]) -> Result<Rect> {
    Ok(Rect {
        lx: int(i128::from(r.lx) + i128::from(at[0]))?,
        ly: int(i128::from(r.ly) + i128::from(at[1]))?,
        ux: int(i128::from(r.ux) + i128::from(at[0]))?,
        uy: int(i128::from(r.uy) + i128::from(at[1]))?,
    })
}
pub(crate) fn contains(r: Rect, p: [i64; 2]) -> bool {
    r.lx <= p[0] && p[0] <= r.ux && r.ly <= p[1] && p[1] <= r.uy
}
pub(crate) fn overlap(a: Rect, b: Rect) -> bool {
    a.lx < b.ux && b.lx < a.ux && a.ly < b.uy && b.ly < a.uy
}
/// The rectangular envelope is a broad-phase test. Routing-metal corners
/// use the same integer minimum distance as native rule validation. Cut
/// spacing keeps its existing conservative envelope.
fn routing_separated(rule: &RouteLayer, a: Rect, b: Rect) -> bool {
    rule.vertical.is_some()
        && route_rules::relation(a, b, rule.spacing) == route_rules::Relation::Far
}

/// Deterministic multiplicative hasher for spatial bin keys (lookups only;
/// no iteration order depends on it).
#[derive(Clone, Copy, Default)]
pub(crate) struct BuildFx;
pub(crate) struct Fx(u64);
impl std::hash::BuildHasher for BuildFx {
    type Hasher = Fx;
    fn build_hasher(&self) -> Fx {
        Fx(0)
    }
}
impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 =
                (self.0.rotate_left(5) ^ u64::from(*b)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_u32(&mut self, v: u32) {
        self.0 = (self.0.rotate_left(5) ^ u64::from(v)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_i64(&mut self, v: i64) {
        self.0 = (self.0.rotate_left(5) ^ v as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}
const NIL: u32 = u32::MAX;
pub(crate) const BIN_PAGE_SIDE: i64 = 32;
const BIN_PAGE_CELLS: usize = (BIN_PAGE_SIDE * BIN_PAGE_SIDE) as usize;
type BinPageKey = (u32, i64, i64);
type BinPage = Box<[[u32; 2]; BIN_PAGE_CELLS]>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinMode {
    Dense,
    Paged,
    Sparse,
}
pub(crate) fn bin_mode(input: &RouteInput, cells: u64) -> BinMode {
    if cells.saturating_mul(input.layers.len() as u64) <= 1_000_000 {
        BinMode::Dense
    } else if input.obstacles.len() + input.instances.as_ref().map_or(0, InstanceObstacles::len)
        >= 1024
    {
        BinMode::Paged
    } else {
        BinMode::Sparse
    }
}
/// Spatial bins: one dense head/tail grid per layer over the die and every
/// fixed shape, entries chained in insertion order (so iteration order, and
/// with it every work count, matches a per-bin list). A layer's grid is
/// allocated on its first shape: 8 bytes per bin plus 8 bytes per entry.
/// Bins outside the grid (never produced by router inputs) use a hash map.
struct BinGrid {
    mode: BinMode,
    bx0: i64,
    by0: i64,
    nx: i64,
    ny: i64,
    /// Per layer: `[head, tail]` entry index per bin, or empty.
    cells: Vec<Vec<[u32; 2]>>,
    /// `[shape id, next entry]`.
    entries: Vec<[u32; 2]>,
    pages: HashMap<BinPageKey, BinPage, BuildFx>,
    overflow: HashMap<(u32, i64, i64), Vec<usize>, BuildFx>,
}
enum BinIter<'g> {
    Chain { entries: &'g [[u32; 2]], at: u32 },
    List(std::slice::Iter<'g, usize>),
}
impl Iterator for BinIter<'_> {
    type Item = usize;
    fn next(&mut self) -> Option<usize> {
        match self {
            BinIter::Chain { entries, at } => {
                if *at == NIL {
                    return None;
                }
                let [id, next] = entries[*at as usize];
                *at = next;
                Some(id as usize)
            }
            BinIter::List(it) => it.next().copied(),
        }
    }
}
impl BinGrid {
    fn slot(&self, x: i64, y: i64) -> Option<usize> {
        let (dx, dy) = (x - self.bx0, y - self.by0);
        (dx >= 0 && dy >= 0 && dx < self.nx && dy < self.ny).then(|| (dy * self.nx + dx) as usize)
    }
    fn push(&mut self, layer: u32, x: i64, y: i64, id: usize) -> Result<()> {
        if self.mode == BinMode::Sparse
            || (self.mode == BinMode::Dense && self.slot(x, y).is_none())
        {
            self.overflow
                .try_reserve(1)
                .map_err(|_| budget("sparse routing bin allocation"))?;
            let bin = self.overflow.entry((layer, x, y)).or_default();
            bin.try_reserve(1)
                .map_err(|_| budget("sparse routing bin entries"))?;
            bin.push(id);
            return Ok(());
        }
        let e = u32::try_from(self.entries.len())
            .ok()
            .filter(|&e| e != NIL)
            .ok_or_else(|| budget("routing spatial-bin count"))?;
        let id = u32::try_from(id).map_err(|_| budget("routing obstacle count"))?;
        let cell = if self.mode == BinMode::Paged {
            let key = (
                layer,
                x.div_euclid(BIN_PAGE_SIDE),
                y.div_euclid(BIN_PAGE_SIDE),
            );
            self.pages
                .try_reserve(1)
                .map_err(|_| budget("routing bin-page allocation"))?;
            let page = self
                .pages
                .entry(key)
                .or_insert_with(|| Box::new([[NIL, NIL]; BIN_PAGE_CELLS]));
            let slot = (y.rem_euclid(BIN_PAGE_SIDE) * BIN_PAGE_SIDE + x.rem_euclid(BIN_PAGE_SIDE))
                as usize;
            &mut page[slot]
        } else {
            let k = self.slot(x, y).expect("dense bin is inside the grid");
            let grid = &mut self.cells[layer as usize];
            if grid.is_empty() {
                grid.resize((self.nx * self.ny) as usize, [NIL, NIL]);
            }
            &mut grid[k]
        };
        if cell[1] == NIL {
            cell[0] = e;
        } else {
            self.entries[cell[1] as usize][1] = e;
        }
        cell[1] = e;
        self.entries.push([id, NIL]);
        Ok(())
    }
    fn iter(&self, layer: u32, x: i64, y: i64) -> BinIter<'_> {
        if self.mode == BinMode::Paged {
            let key = (
                layer,
                x.div_euclid(BIN_PAGE_SIDE),
                y.div_euclid(BIN_PAGE_SIDE),
            );
            let slot = (y.rem_euclid(BIN_PAGE_SIDE) * BIN_PAGE_SIDE + x.rem_euclid(BIN_PAGE_SIDE))
                as usize;
            return BinIter::Chain {
                entries: &self.entries,
                at: self.pages.get(&key).map_or(NIL, |p| p[slot][0]),
            };
        }
        match self.slot(x, y).filter(|_| self.mode == BinMode::Dense) {
            Some(k) => BinIter::Chain {
                entries: &self.entries,
                at: self.cells[layer as usize].get(k).map_or(NIL, |c| c[0]),
            },
            None => BinIter::List(
                self.overflow
                    .get(&(layer, x, y))
                    .map_or([].iter(), |v| v.iter()),
            ),
        }
    }
    /// Grid bins allocated over all layers.
    fn allocated(&self) -> usize {
        self.cells.iter().map(Vec::len).sum::<usize>()
            + self.overflow.len()
            + self.pages.len() * BIN_PAGE_CELLS
    }
    fn clear(&mut self) {
        for grid in &mut self.cells {
            if !grid.is_empty() {
                grid.fill([NIL, NIL]);
            }
        }
        self.entries.clear();
        for page in self.pages.values_mut() {
            page.fill([NIL, NIL]);
        }
        self.overflow.clear();
    }
}
/// Bin index range `[x0, x1] x [y0, y1]` covering the die and every fixed
/// obstacle, and the bins one layer's grid holds.
pub(crate) fn bin_extent(input: &RouteInput, bin: i64) -> (i64, i64, i64, i64, u64) {
    let mut r = input.die;
    for b in &input.obstacles {
        r.lx = r.lx.min(b.rect.lx);
        r.ly = r.ly.min(b.rect.ly);
        r.ux = r.ux.max(b.rect.ux);
        r.uy = r.uy.max(b.rect.uy);
    }
    let (x0, x1, y0, y1) = (
        r.lx.div_euclid(bin),
        r.ux.div_euclid(bin),
        r.ly.div_euclid(bin),
        r.uy.div_euclid(bin),
    );
    let cells = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
    (x0, x1, y0, y1, u64::try_from(cells).unwrap_or(u64::MAX))
}
/// Spatial bin side of the router (track mode: two access steps).
pub(crate) fn bin_side(input: &RouteInput) -> Result<i64> {
    input
        .step
        .checked_mul(if input.tracks.is_some() { 2 } else { 8 })
        .filter(|&side| side > 0)
        .ok_or_else(|| invalid("bin overflow"))
}

pub(crate) struct InstanceIndex<'a> {
    source: &'a InstanceObstacles,
    bin: i64,
    bins: BinGrid,
    marks: Vec<u64>,
    generation: u64,
    entries: usize,
    mode: Option<InstanceIndexMode>,
    /// Extraction-only per-instance world bounds and layer masks, computed
    /// once at construction. Router indices keep `None` and their original
    /// per-visit bounds computation.
    cache: Option<InstanceCache>,
}

/// Immutable per-instance query filters. A layer mask bit is set for every
/// template layer below 64; any layer at or above 64 sets every bit, so the
/// mask never rejects an instance that has shapes on the queried layer.
struct InstanceCache {
    bounds: Vec<Option<Rect>>,
    layers: Vec<u64>,
}
impl InstanceCache {
    fn new(source: &InstanceObstacles) -> Result<Self> {
        let mut bounds = Vec::new();
        let mut layers = Vec::new();
        bounds
            .try_reserve_exact(source.len())
            .map_err(|_| budget("instance bounds cache allocation"))?;
        layers
            .try_reserve_exact(source.len())
            .map_err(|_| budget("instance layer-mask allocation"))?;
        for i in 0..source.len() {
            bounds.push(source.bounds(i)?);
            layers.push(
                source
                    .instance_layers(i)
                    .fold(0u64, |mask, layer| mask | layer_bit(layer)),
            );
        }
        Ok(Self { bounds, layers })
    }
}
fn layer_bit(layer: u32) -> u64 {
    if layer < 64 { 1 << layer } else { u64::MAX }
}
/// Bytes of the extraction-only instance cache (bounds plus layer mask).
pub(crate) fn instance_cache_bytes(instances: usize) -> u64 {
    (instances as u64).saturating_mul((size_of::<Option<Rect>>() + size_of::<u64>()) as u64)
}

/// Bounded worker-private deduplication state for immutable instance queries.
pub(crate) struct InstanceQueryScratch {
    marks: Vec<u64>,
    generation: u64,
}
impl InstanceQueryScratch {
    pub(crate) fn new(instances: usize) -> Result<Self> {
        let mut marks = Vec::new();
        marks
            .try_reserve_exact(instances)
            .map_err(|_| budget("instance query scratch allocation"))?;
        marks.resize(instances, 0);
        Ok(Self {
            marks,
            generation: 0,
        })
    }
}

fn instance_extent(
    die: Rect,
    bin: i64,
    source: &InstanceObstacles,
    mode: Option<InstanceIndexMode>,
) -> Result<(i64, Rect, usize)> {
    if bin <= 0 {
        return Err(invalid("instance bin size"));
    }
    let mut bounds = die;
    let mut entries = 0usize;
    for i in 0..source.len() {
        if let Some(r) = source.bounds(i)? {
            bounds.lx = bounds.lx.min(r.lx);
            bounds.ly = bounds.ly.min(r.ly);
            bounds.ux = bounds.ux.max(r.ux);
            bounds.uy = bounds.uy.max(r.uy);
            let nx = i128::from(r.ux.div_euclid(bin)) - i128::from(r.lx.div_euclid(bin)) + 1;
            let ny = i128::from(r.uy.div_euclid(bin)) - i128::from(r.ly.div_euclid(bin)) + 1;
            let n = usize::try_from(nx * ny).map_err(|_| budget("instance bin count"))?;
            entries = entries
                .checked_add(
                    n.checked_mul(if mode.is_some() {
                        source.instance_layers(i).count()
                    } else {
                        1
                    })
                    .ok_or_else(|| budget("instance layer-bin count"))?,
                )
                .ok_or_else(|| budget("instance bin count"))?;
        }
    }
    Ok((bin, bounds, entries))
}

pub(crate) fn instance_index_bytes(input: &RouteInput, limits: &RouteLimits) -> Result<u64> {
    instance_index_override_bytes(
        input,
        limits,
        input.tracks.as_ref().and_then(|p| p.instance_index),
    )
}
pub(crate) fn instance_index_override_bytes(
    input: &RouteInput,
    limits: &RouteLimits,
    mode: Option<InstanceIndexMode>,
) -> Result<u64> {
    let Some(source) = &input.instances else {
        return Ok(0);
    };
    instance_geometry_bytes_mode(
        source,
        input.die,
        input
            .step
            .checked_mul(16)
            .ok_or_else(|| budget("instance bin size"))?,
        input.layers.len(),
        limits,
        mode,
    )
}

pub(crate) fn instance_geometry_bytes(
    source: &InstanceObstacles,
    die: Rect,
    bin: i64,
    layer_count: usize,
    limits: &RouteLimits,
) -> Result<u64> {
    instance_geometry_bytes_mode(source, die, bin, layer_count, limits, None)
}
fn instance_geometry_bytes_mode(
    source: &InstanceObstacles,
    die: Rect,
    bin: i64,
    layer_count: usize,
    limits: &RouteLimits,
    mode: Option<InstanceIndexMode>,
) -> Result<u64> {
    source.validate(layer_count, limits)?;
    let (bin, b, entries) = instance_extent(die, bin, source, mode)?;
    if entries > limits.max_bin_entries {
        return Err(budget("instance bin-entry limit"));
    }
    let (x0, x1, y0, y1) = (
        b.lx.div_euclid(bin),
        b.ux.div_euclid(bin),
        b.ly.div_euclid(bin),
        b.uy.div_euclid(bin),
    );
    let slots = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
    let pages = (i128::from(x1.div_euclid(BIN_PAGE_SIDE))
        - i128::from(x0.div_euclid(BIN_PAGE_SIDE))
        + 1)
        * (i128::from(y1.div_euclid(BIN_PAGE_SIDE)) - i128::from(y0.div_euclid(BIN_PAGE_SIDE)) + 1);
    let indexed_layers = if mode.is_some() {
        source.layer_count() as u64
    } else {
        1
    };
    let grid = if slots.saturating_mul(i128::from(indexed_layers)) <= 1_000_000 {
        (slots as u64)
            .saturating_mul(indexed_layers)
            .saturating_mul(8)
    } else if source.len() >= 1024 {
        u64::try_from(pages)
            .unwrap_or(u64::MAX)
            .saturating_mul(indexed_layers)
            .min(entries as u64)
            .saturating_mul(8320)
    } else {
        u64::try_from(slots)
            .unwrap_or(u64::MAX)
            .saturating_mul(indexed_layers)
            .min(entries as u64)
            .saturating_mul(96)
    };
    Ok(grid
        .saturating_add(entries as u64 * 16)
        .saturating_add(source.len() as u64 * 8)
        .saturating_add(if mode.is_some() {
            layer_count as u64 * 24
        } else {
            0
        }))
}

impl<'a> InstanceIndex<'a> {
    fn build_work(
        source: &InstanceObstacles,
        die: Rect,
        bin: i64,
        mode: Option<InstanceIndexMode>,
    ) -> Result<usize> {
        if mode.is_none() {
            return Ok(0);
        }
        let (_, _, entries) = instance_extent(die, bin, source, mode)?;
        entries
            .checked_add(source.len())
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(|| budget("instance layer-index construction work"))
    }
    fn new(
        input: &RouteInput,
        source: &'a InstanceObstacles,
        limits: &RouteLimits,
    ) -> Result<Self> {
        Self::for_geometry_mode(
            source,
            input.die,
            input
                .step
                .checked_mul(16)
                .ok_or_else(|| budget("instance bin size"))?,
            input.layers.len(),
            limits,
            input.tracks.as_ref().and_then(|p| p.instance_index),
        )
    }
    /// Extraction index: shares the router's bins and work accounting, plus
    /// cached instance bounds and a layer mask that skip instances without
    /// metal on the queried layer before any template work is charged.
    pub(crate) fn for_geometry(
        source: &'a InstanceObstacles,
        die: Rect,
        bin: i64,
        layer_count: usize,
        limits: &RouteLimits,
    ) -> Result<Self> {
        let mut index = Self::for_geometry_mode(source, die, bin, layer_count, limits, None)?;
        index.cache = Some(InstanceCache::new(source)?);
        Ok(index)
    }
    fn for_geometry_mode(
        source: &'a InstanceObstacles,
        die: Rect,
        bin: i64,
        layer_count: usize,
        limits: &RouteLimits,
        mode: Option<InstanceIndexMode>,
    ) -> Result<Self> {
        source.validate(layer_count, limits)?;
        let (bin, b, entries) = instance_extent(die, bin, source, mode)?;
        if entries > limits.max_bin_entries {
            return Err(budget("instance bin-entry limit"));
        }
        let (x0, x1, y0, y1) = (
            b.lx.div_euclid(bin),
            b.ux.div_euclid(bin),
            b.ly.div_euclid(bin),
            b.uy.div_euclid(bin),
        );
        let cells = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        let bin_mode = if cells.saturating_mul(if mode.is_some() {
            source.layer_count() as i128
        } else {
            1
        }) <= 1_000_000
        {
            BinMode::Dense
        } else if source.len() >= 1024 {
            BinMode::Paged
        } else {
            BinMode::Sparse
        };
        let mut bins = BinGrid {
            mode: bin_mode,
            bx0: x0,
            by0: y0,
            nx: x1 - x0 + 1,
            ny: y1 - y0 + 1,
            cells: vec![Vec::new(); if mode.is_some() { layer_count } else { 1 }],
            entries: Vec::new(),
            pages: HashMap::with_hasher(BuildFx),
            overflow: HashMap::with_hasher(BuildFx),
        };
        for i in 0..source.len() {
            if let Some(r) = source.bounds(i)? {
                for x in r.lx.div_euclid(bin)..=r.ux.div_euclid(bin) {
                    for y in r.ly.div_euclid(bin)..=r.uy.div_euclid(bin) {
                        if mode.is_some() {
                            for layer in source.instance_layers(i) {
                                bins.push(layer, x, y, i)?;
                            }
                        } else {
                            bins.push(0, x, y, i)?;
                        }
                    }
                }
            }
        }
        let mut marks = Vec::new();
        marks
            .try_reserve_exact(source.len())
            .map_err(|_| budget("instance query-mark allocation"))?;
        marks.resize(source.len(), 0);
        Ok(Self {
            source,
            bin,
            bins,
            marks,
            generation: 0,
            entries,
            mode,
            cache: None,
        })
    }
    fn visit(
        &mut self,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        mut visitor: impl FnMut(Blockage) -> Result<bool>,
    ) -> Result<bool> {
        let source = self.source;
        self.visit_cells(layer, query, work, limits, |i, work| {
            source.visit_near(
                i,
                layer,
                query,
                || {
                    *work = work
                        .checked_add(1)
                        .filter(|&n| n <= limits.max_comparisons)
                        .ok_or_else(|| budget("instance geometry work"))?;
                    Ok(())
                },
                |_, b, _| visitor(b),
            )
        })
    }
    pub(crate) fn visit_cells(
        &mut self,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        visitor: impl FnMut(usize, &mut usize) -> Result<bool>,
    ) -> Result<bool> {
        Self::visit_cells_inner(
            self.source,
            self.bin,
            &self.bins,
            self.mode,
            self.cache.as_ref(),
            &mut self.marks,
            &mut self.generation,
            layer,
            query,
            work,
            limits,
            visitor,
        )
    }
    /// The spatial bins remain shared; each concurrent query owns its marks.
    pub(crate) fn visit_cells_with_scratch(
        &self,
        scratch: &mut InstanceQueryScratch,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        visitor: impl FnMut(usize, &mut usize) -> Result<bool>,
    ) -> Result<bool> {
        if scratch.marks.len() != self.source.len() {
            return Err(budget("instance query scratch extent"));
        }
        Self::visit_cells_inner(
            self.source,
            self.bin,
            &self.bins,
            self.mode,
            self.cache.as_ref(),
            &mut scratch.marks,
            &mut scratch.generation,
            layer,
            query,
            work,
            limits,
            visitor,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "split immutable index fields from private query marks"
    )]
    fn visit_cells_inner(
        source: &InstanceObstacles,
        bin: i64,
        bins: &BinGrid,
        mode: Option<InstanceIndexMode>,
        cache: Option<&InstanceCache>,
        marks: &mut [u64],
        generation: &mut u64,
        layer: u32,
        query: Rect,
        work: &mut usize,
        limits: &RouteLimits,
        mut visitor: impl FnMut(usize, &mut usize) -> Result<bool>,
    ) -> Result<bool> {
        if !source.has_layer(layer) {
            return Ok(true);
        }
        *generation = generation
            .checked_add(1)
            .ok_or_else(|| budget("instance query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(bin),
            query.ux.div_euclid(bin),
            query.ly.div_euclid(bin),
            query.uy.div_euclid(bin),
        );
        let extent = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if extent > limits.max_bin_entries as i128 {
            return Err(budget("instance query extent"));
        }
        let bit = layer_bit(layer);
        for x in x0..=x1 {
            for y in y0..=y1 {
                *work = work
                    .checked_add(1)
                    .filter(|&n| n <= limits.max_comparisons)
                    .ok_or_else(|| budget("instance query work"))?;
                for i in bins.iter(if mode.is_some() { layer } else { 0 }, x, y) {
                    if marks[i] == *generation {
                        continue;
                    }
                    marks[i] = *generation;
                    *work = work
                        .checked_add(1)
                        .filter(|&n| n <= limits.max_comparisons)
                        .ok_or_else(|| budget("instance query work"))?;
                    // Work is charged before either filter, so the cache and
                    // layer mask change no work count: an instance without
                    // the layer visits no template shape.
                    if let Some(cache) = cache {
                        if cache.layers[i] & bit == 0
                            || !cache.bounds[i].is_some_and(|b| overlap(query, b))
                        {
                            continue;
                        }
                    } else if !source.bounds(i)?.is_some_and(|b| overlap(query, b)) {
                        continue;
                    }
                    let clear = visitor(i, work)?;
                    if !clear {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

#[derive(Serialize)]
pub(crate) struct SpatialCapture<'a> {
    shapes: &'a [Blockage],
    alive: &'a [bool],
    routed: &'a [bool],
    work: usize,
    bin: i64,
    entries: usize,
    generation: u64,
    instance_generation: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpatialState {
    shapes: Vec<Blockage>,
    alive: Vec<bool>,
    routed: Vec<bool>,
    pub(crate) work: usize,
    bin: i64,
    entries: usize,
    generation: u64,
    instance_generation: Option<u64>,
}
pub(crate) struct Spatial<'a> {
    pub(crate) input: &'a RouteInput,
    limits: &'a RouteLimits,
    bin: i64,
    /// Added geometry only; immutable fixed shapes remain in `input.obstacles`.
    shapes: Vec<Blockage>,
    bins: BinGrid,
    marks: Vec<u64>,
    generation: u64,
    entries: usize,
    pub(crate) work: usize,
    /// False once removed by rip-up (track router); ids stay in their bins.
    alive: Vec<bool>,
    /// True for rip-up-able routed shapes (track router); obstacles, pins and
    /// reserved pin access are fixed.
    routed: Vec<bool>,
    /// Reclaimed `shapes` offsets un-indexed by `compact_dead`.
    free_offsets: Vec<usize>,
    /// Count of `!alive` shapes still present in `bins`.
    dead_count: usize,
    instances: Option<InstanceIndex<'a>>,
    /// Supply ties by router net: (supply net, rail zone).
    ties: HashMap<u32, (u32, LayerRect)>,
}
/// Result of a soft probe: fixed blockage, or clear except for routed shapes
/// of the listed other nets (sorted, deduplicated).
pub(crate) enum Soft {
    Blocked,
    Clear { near: bool, victims: Vec<u32> },
}
fn same_net_shapes(
    ties: &HashMap<u32, (u32, LayerRect)>,
    net: u32,
    layer: u32,
    rect: Rect,
    b: &Blockage,
) -> bool {
    if b.net == Some(net) {
        return true;
    }
    match (ties.get(&net), b.net) {
        (Some((supply, zone)), Some(n)) if n == *supply => {
            zone.layer == layer
                && overlap(rect, zone.rect)
                && route_rules::relation(rect, b.rect, 0) == route_rules::Relation::Connected
        }
        _ => false,
    }
}

impl<'a> Spatial<'a> {
    pub(crate) fn capture(&self) -> Result<SpatialCapture<'_>> {
        if self.shapes.len() > 4_000_000 {
            return Err(budget("replay shape capture limit"));
        }
        Ok(SpatialCapture {
            shapes: &self.shapes,
            alive: &self.alive,
            routed: &self.routed,
            work: self.work,
            bin: self.bin,
            entries: self.entries,
            generation: self.generation,
            instance_generation: self.instances.as_ref().map(|v| v.generation),
        })
    }
    pub(crate) fn restore_capture(
        input: &'a RouteInput,
        limits: &'a RouteLimits,
        state: SpatialState,
    ) -> Result<Self> {
        let n = state.shapes.len();
        if n > 4_000_000
            || n != state.alive.len()
            || n != state.routed.len()
            || state.work > limits.max_comparisons
            || state.entries > limits.max_bin_entries
        {
            return Err(budget("replay spatial state limits"));
        }
        let mut result = Self::new(input, limits)?;
        for (i, shape) in state.shapes.into_iter().enumerate() {
            if shape.layer as usize >= input.layers.len() || !shape.rect.valid() {
                return Err(invalid("replay obstacle geometry"));
            }
            let id = if state.routed[i] {
                result.add_routed(shape)?
            } else {
                result.add_fixed(shape)?
            };
            if !state.alive[i] {
                result.remove(id);
            }
        }
        if result.bin != state.bin || result.entries != state.entries {
            return Err(invalid("replay spatial index differs"));
        }
        result.work = state
            .work
            .checked_add(result.instance_index_build_work()?)
            .filter(|&n| n <= limits.max_comparisons)
            .ok_or_else(|| budget("replay instance-index construction work"))?;
        result.generation = state.generation;
        match (&mut result.instances, state.instance_generation) {
            (Some(index), Some(generation)) => index.generation = generation,
            (None, None) => (),
            _ => return Err(invalid("replay instance index differs")),
        }
        Ok(result)
    }

    pub(crate) fn new(input: &'a RouteInput, limits: &'a RouteLimits) -> Result<Self> {
        if input
            .obstacles
            .len()
            .saturating_add(input.instances.as_ref().map_or(0, InstanceObstacles::len))
            > limits.max_obstacles
        {
            return Err(budget("routing obstacle count"));
        }
        let instance_work = input.instances.as_ref().map_or(Ok(0), |source| {
            InstanceIndex::build_work(
                source,
                input.die,
                input
                    .step
                    .checked_mul(16)
                    .ok_or_else(|| budget("instance bin size"))?,
                input.tracks.as_ref().and_then(|p| p.instance_index),
            )
        })?;
        if instance_work > limits.max_comparisons {
            return Err(budget("instance-index construction work"));
        }
        let instances = input
            .instances
            .as_ref()
            .map(|s| InstanceIndex::new(input, s, limits))
            .transpose()?;
        let instance_entries = instances.as_ref().map_or(0, |i| i.entries);
        // The track router queries far more often with short edges; it
        // uses bins of two access steps (the lattice router keeps eight).
        let bin = bin_side(input)?;
        let (x0, x1, y0, y1, cells) = bin_extent(input, bin);
        let mut s = Self {
            input,
            limits,
            bin,
            shapes: Vec::new(),
            bins: BinGrid {
                mode: bin_mode(input, cells),
                bx0: x0,
                by0: y0,
                nx: x1 - x0 + 1,
                ny: y1 - y0 + 1,
                cells: vec![Vec::new(); input.layers.len()],
                entries: Vec::new(),
                pages: HashMap::with_hasher(BuildFx),
                overflow: HashMap::with_hasher(BuildFx),
            },
            marks: vec![0; input.obstacles.len()],
            generation: 0,
            entries: instance_entries,
            work: instance_work,
            alive: Vec::new(),
            routed: Vec::new(),
            free_offsets: Vec::new(),
            dead_count: 0,
            instances,
            ties: input
                .ties
                .iter()
                .map(|t| (t.net, (t.supply, t.zone.clone())))
                .collect(),
        };
        for (id, b) in input.obstacles.iter().enumerate() {
            s.index(id, b)?;
        }
        Ok(s)
    }
    pub(crate) fn replay_instance_index(
        &mut self,
        mode: Option<InstanceIndexMode>,
    ) -> Result<usize> {
        if self.instances.as_ref().and_then(|i| i.mode) == mode {
            return Ok(0);
        }
        let Some(source) = self.input.instances.as_ref() else {
            return Ok(0);
        };
        let bin = self
            .input
            .step
            .checked_mul(16)
            .ok_or_else(|| budget("instance bin size"))?;
        let bytes = instance_geometry_bytes_mode(
            source,
            self.input.die,
            bin,
            self.input.layers.len(),
            self.limits,
            mode,
        )?;
        if bytes
            > self
                .input
                .tracks
                .as_ref()
                .map_or(0, |p| p.memory_mib)
                .saturating_mul(1 << 20)
        {
            return Err(budget("replay instance-index memory admission"));
        }
        let charged = InstanceIndex::build_work(source, self.input.die, bin, mode)?;
        self.work = self
            .work
            .checked_add(charged)
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("replay instance-index construction work"))?;
        let previous_entries = self.instances.as_ref().map_or(0, |i| i.entries);
        let generation = self.instances.as_ref().map_or(0, |i| i.generation);
        // No query may retain this representation across a replay boundary.
        // Dynamic shape handles and tombstones are deliberately untouched.
        drop(self.instances.take());
        let mut index = InstanceIndex::for_geometry_mode(
            source,
            self.input.die,
            bin,
            self.input.layers.len(),
            self.limits,
            mode,
        )?;
        self.entries = self
            .entries
            .checked_sub(previous_entries)
            .and_then(|n| n.checked_add(index.entries))
            .filter(|&n| n <= self.limits.max_bin_entries)
            .ok_or_else(|| budget("replay combined bin-entry limit"))?;
        index.generation = generation;
        self.instances = Some(index);
        Ok(charged)
    }
    pub(crate) fn instance_index_build_work(&self) -> Result<usize> {
        match &self.instances {
            Some(index) if index.mode.is_some() => index
                .entries
                .checked_add(index.source.len())
                .and_then(|n| n.checked_mul(2))
                .ok_or_else(|| budget("instance layer-index construction work")),
            _ => Ok(0),
        }
    }
    /// Is stored shape `b` the query net's own metal? A supply tie also owns
    /// supply shapes that its query touches inside its rail zone.
    fn same_net(&self, net: u32, layer: u32, rect: Rect, b: &Blockage) -> bool {
        same_net_shapes(&self.ties, net, layer, rect, b)
    }
    fn probe_instances(
        &mut self,
        layer: u32,
        rect: Rect,
        query: Rect,
        net: u32,
    ) -> Result<Option<bool>> {
        let Some(instances) = &mut self.instances else {
            return Ok(Some(false));
        };
        let ties = &self.ties;
        let routing = self.input.layers[layer as usize].vertical.is_some();
        let rules = self.input.rules;
        let spacing = self.input.layers[layer as usize].spacing;
        let mut near = false;
        let clear = instances.visit(layer, query, &mut self.work, self.limits, |b| {
            if !overlap(query, b.rect) {
                return Ok(true);
            }
            if rules && routing_separated(&self.input.layers[layer as usize], rect, b.rect) {
                return Ok(true);
            }
            let shared = same_net_shapes(ties, net, layer, rect, &b) && (routing || rect == b.rect);
            if !shared {
                return Ok(false);
            }
            if rules
                && routing
                && route_rules::relation(rect, b.rect, spacing) == route_rules::Relation::Near
            {
                near = true;
            }
            Ok(true)
        })?;
        Ok(clear.then_some(near))
    }
    /// Add a rip-up-able routed shape; returns its id.
    pub(crate) fn add_routed(&mut self, b: Blockage) -> Result<usize> {
        self.add_slot(b, true)
    }
    /// Add a fixed shape; returns its id.
    pub(crate) fn add_fixed(&mut self, b: Blockage) -> Result<usize> {
        self.add_slot(b, false)
    }
    pub(crate) fn remove(&mut self, id: usize) {
        let offset = id - self.input.obstacles.len();
        if self.alive[offset] {
            self.alive[offset] = false;
            self.dead_count += 1;
        }
    }
    pub(crate) fn restore(&mut self, id: usize) -> Result<()> {
        let offset = id
            .checked_sub(self.input.obstacles.len())
            .ok_or_else(|| invalid("cannot restore an input obstacle"))?;
        let alive = self
            .alive
            .get_mut(offset)
            .ok_or_else(|| invalid("invalid restored obstacle"))?;
        if !*alive {
            *alive = true;
            self.dead_count = self.dead_count.saturating_sub(1);
        }
        Ok(())
    }
    pub(crate) fn should_compact(&self) -> bool {
        self.dead_count >= 262_144 && self.free_offsets.len() < 65_536
    }
    pub(crate) fn compact_dead(&mut self) -> Result<()> {
        self.bins.clear();
        self.entries = self.instances.as_ref().map_or(0, |i| i.entries);
        self.free_offsets.clear();
        self.dead_count = 0;
        for (id, b) in self.input.obstacles.iter().enumerate() {
            self.index(id, b)?;
        }
        let base = self.input.obstacles.len();
        for offset in 0..self.shapes.len() {
            if self.alive[offset] {
                let b = self.shapes[offset].clone();
                self.index(base + offset, &b)?;
            }
        }
        for offset in (0..self.shapes.len()).rev() {
            if !self.alive[offset] {
                self.free_offsets.push(offset);
            }
        }
        Ok(())
    }
    /// (allocated grid bins, bin entries, shapes, bin side) for telemetry.
    pub(crate) fn footprint(&self) -> (usize, usize, usize, i64) {
        (
            self.bins.allocated() + self.instances.as_ref().map_or(0, |i| i.bins.allocated()),
            self.entries,
            self.len()
                + self
                    .input
                    .instances
                    .as_ref()
                    .map_or(0, InstanceObstacles::len),
            self.bin,
        )
    }
    pub(crate) fn add(&mut self, b: Blockage) -> Result<()> {
        self.add_slot(b, false).map(|_| ())
    }
    fn add_slot(&mut self, b: Blockage, routed: bool) -> Result<usize> {
        if let Some(&offset) = self.free_offsets.last() {
            let id = self.input.obstacles.len() + offset;
            self.index(id, &b)?;
            self.free_offsets.pop();
            self.shapes[offset] = b;
            self.marks[id] = 0;
            self.alive[offset] = true;
            self.routed[offset] = routed;
            return Ok(id);
        }
        if self.len().saturating_add(
            self.input
                .instances
                .as_ref()
                .map_or(0, InstanceObstacles::len),
        ) >= self.limits.max_obstacles
        {
            return Err(budget(&format!(
                "routing obstacle count: {} stored plus {} instance shapes, limit {}; {} removed stored shapes",
                self.len(),
                self.input
                    .instances
                    .as_ref()
                    .map_or(0, InstanceObstacles::len),
                self.limits.max_obstacles,
                self.alive.iter().filter(|alive| !**alive).count()
            )));
        }
        let id = self.len();
        self.index(id, &b)?;
        self.shapes.push(b);
        self.marks.push(0);
        self.alive.push(true);
        self.routed.push(routed);
        Ok(id)
    }
    pub(crate) fn len(&self) -> usize {
        self.input.obstacles.len() + self.shapes.len()
    }
    fn shape(&self, id: usize) -> &Blockage {
        if id < self.input.obstacles.len() {
            &self.input.obstacles[id]
        } else {
            &self.shapes[id - self.input.obstacles.len()]
        }
    }
    fn alive(&self, id: usize) -> bool {
        id < self.input.obstacles.len() || self.alive[id - self.input.obstacles.len()]
    }
    fn routed(&self, id: usize) -> bool {
        id >= self.input.obstacles.len() && self.routed[id - self.input.obstacles.len()]
    }
    fn index(&mut self, id: usize, b: &Blockage) -> Result<()> {
        if b.layer as usize >= self.input.layers.len() || !b.rect.valid() {
            return Err(invalid("routing obstacle"));
        }
        let (x0, x1, y0, y1) = (
            b.rect.lx.div_euclid(self.bin),
            b.rect.ux.div_euclid(self.bin),
            b.rect.ly.div_euclid(self.bin),
            b.rect.uy.div_euclid(self.bin),
        );
        let n = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if n < 0 || n > self.limits.max_bin_entries.saturating_sub(self.entries) as i128 {
            return Err(budget("routing spatial-bin count"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.bins.push(b.layer, x, y, id)?;
                self.entries += 1;
            }
        }
        Ok(())
    }
    pub(crate) fn clear(&mut self, layer: u32, rect: Rect, net: u32) -> Result<bool> {
        Ok(self.probe(layer, rect, net)?.is_some())
    }
    /// Exact clearance for a long capacity span. Sparse fixtures can have
    /// fewer stored shapes than empty bins along the span; scan those shapes
    /// directly instead of charging work proportional to empty chip area.
    pub(crate) fn span_clear(&mut self, layer: u32, rect: Rect, net: u32) -> Result<bool> {
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        let columns = i128::from(query.ux.div_euclid(self.bin))
            - i128::from(query.lx.div_euclid(self.bin))
            + 1;
        let rows = i128::from(query.uy.div_euclid(self.bin))
            - i128::from(query.ly.div_euclid(self.bin))
            + 1;
        if self.instances.is_some() || columns.saturating_mul(rows) <= self.len() as i128 {
            return self.clear(layer, rect, net);
        }
        if !self.input.die.contains(rect) {
            return Ok(false);
        }
        self.work = self
            .work
            .checked_add(self.len().saturating_add(1))
            .filter(|&n| n <= self.limits.max_comparisons)
            .ok_or_else(|| budget("capacity span comparison limit"))?;
        let routing = self.input.layers[layer as usize].vertical.is_some();
        for id in 0..self.len() {
            let shape = self.shape(id);
            if self.alive(id)
                && shape.layer == layer
                && overlap(query, shape.rect)
                && !(self.input.rules
                    && routing_separated(&self.input.layers[layer as usize], rect, shape.rect))
                && !(self.same_net(net, layer, rect, shape) && (routing || rect == shape.rect))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Bounded diagnostic for a stored shape rejected by the independent
    /// validator. Instance-template collisions remain identified by layer.
    pub(crate) fn stored_collision(&self, layer: u32, rect: Rect, net: u32) -> Result<String> {
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        let routing = self.input.layers[layer as usize].vertical.is_some();
        for id in 0..self.len() {
            if !self.alive(id) {
                continue;
            }
            let b = self.shape(id);
            if b.layer == layer
                && overlap(query, b.rect)
                && !(self.input.rules
                    && routing_separated(&self.input.layers[layer as usize], rect, b.rect))
                && !(self.same_net(net, layer, rect, b) && (routing || rect == b.rect))
            {
                return Ok(format!("against net {:?} rect {:?}", b.net, b.rect));
            }
        }
        Ok("against instance geometry or die boundary".into())
    }
    /// Like `probe`, but routed shapes of other nets do not block: their nets
    /// are returned as victims. Fixed shapes of other nets still block.
    pub(crate) fn probe_soft(&mut self, layer: u32, rect: Rect, net: u32) -> Result<Soft> {
        if !self.input.die.contains(rect) {
            return Ok(Soft::Blocked);
        }
        let mut near = false;
        let mut victims = Vec::new();
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| budget("routing query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let bins = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("routing query extent"));
        }
        let routing = self.input.layers[layer as usize].vertical.is_some();
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.work += 1;
                if self.work > self.limits.max_comparisons {
                    return Err(budget("routing comparison limit"));
                }
                {
                    for id in self.bins.iter(layer, x, y) {
                        if self.marks[id] == self.generation {
                            continue;
                        }
                        self.marks[id] = self.generation;
                        self.work += 1;
                        if self.work > self.limits.max_comparisons {
                            return Err(budget("routing comparison limit"));
                        }
                        if !self.alive(id) {
                            continue;
                        }
                        let b = self.shape(id);
                        if !overlap(query, b.rect) {
                            continue;
                        }
                        if self.input.rules
                            && routing_separated(&self.input.layers[layer as usize], rect, b.rect)
                        {
                            continue;
                        }
                        let shared =
                            self.same_net(net, layer, rect, b) && (routing || rect == b.rect);
                        if !shared {
                            match b.net {
                                Some(other) if self.routed(id) && other != net => {
                                    victims.push(other)
                                }
                                _ => return Ok(Soft::Blocked),
                            }
                            continue;
                        }
                        if self.input.rules
                            && routing
                            && route_rules::relation(
                                rect,
                                b.rect,
                                self.input.layers[layer as usize].spacing,
                            ) == route_rules::Relation::Near
                        {
                            near = true;
                        }
                    }
                }
            }
        }
        match self.probe_instances(layer, rect, query, net)? {
            Some(n) => near |= n,
            None => return Ok(Soft::Blocked),
        }
        victims.sort_unstable();
        victims.dedup();
        Ok(Soft::Clear { near, victims })
    }
    pub(crate) fn via_probe_soft(&mut self, kind: usize, at: [i64; 2], net: u32) -> Result<Soft> {
        let mut near = false;
        let mut victims = Vec::new();
        for shape in &self.input.vias[kind].shapes {
            match self.probe_soft(shape.layer, shifted(shape.rect, at)?, net)? {
                Soft::Blocked => return Ok(Soft::Blocked),
                Soft::Clear {
                    near: n,
                    victims: v,
                } => {
                    near |= n;
                    victims.extend(v);
                }
            }
        }
        victims.sort_unstable();
        victims.dedup();
        Ok(Soft::Clear { near, victims })
    }
    /// None when blocked; otherwise whether a same-net routing-layer shape is
    /// within spacing without connecting (a potential notch; rules mode only).
    pub(crate) fn probe(&mut self, layer: u32, rect: Rect, net: u32) -> Result<Option<bool>> {
        if !self.input.die.contains(rect) {
            return Ok(None);
        }
        let mut near = false;
        let query = expand(rect, self.input.layers[layer as usize].spacing)?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| budget("routing query generation"))?;
        let (x0, x1, y0, y1) = (
            query.lx.div_euclid(self.bin),
            query.ux.div_euclid(self.bin),
            query.ly.div_euclid(self.bin),
            query.uy.div_euclid(self.bin),
        );
        let bins = (i128::from(x1) - i128::from(x0) + 1) * (i128::from(y1) - i128::from(y0) + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("routing query extent"));
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                self.work += 1;
                if self.work > self.limits.max_comparisons {
                    return Err(budget("routing comparison limit"));
                }
                {
                    for id in self.bins.iter(layer, x, y) {
                        if self.marks[id] == self.generation {
                            continue;
                        }
                        self.marks[id] = self.generation;
                        self.work += 1;
                        if self.work > self.limits.max_comparisons {
                            return Err(budget("routing comparison limit"));
                        }
                        if !self.alive(id) {
                            continue;
                        }
                        let b = self.shape(id);
                        let routing = self.input.layers[layer as usize].vertical.is_some();
                        if !overlap(query, b.rect) {
                            continue;
                        }
                        if self.input.rules
                            && routing_separated(&self.input.layers[layer as usize], rect, b.rect)
                        {
                            continue;
                        }
                        let shared =
                            self.same_net(net, layer, rect, b) && (routing || rect == b.rect);
                        if !shared {
                            return Ok(None);
                        }
                        if self.input.rules
                            && routing
                            && route_rules::relation(
                                rect,
                                b.rect,
                                self.input.layers[layer as usize].spacing,
                            ) == route_rules::Relation::Near
                        {
                            near = true;
                        }
                    }
                }
            }
        }
        match self.probe_instances(layer, rect, query, net)? {
            Some(n) => near |= n,
            None => return Ok(None),
        }
        Ok(Some(near))
    }
    pub(crate) fn via_clear(&mut self, kind: usize, at: [i64; 2], net: u32) -> Result<bool> {
        Ok(self.via_probe(kind, at, net)?.is_some())
    }
    pub(crate) fn via_probe(
        &mut self,
        kind: usize,
        at: [i64; 2],
        net: u32,
    ) -> Result<Option<bool>> {
        let mut near = false;
        for shape in &self.input.vias[kind].shapes {
            match self.probe(shape.layer, shifted(shape.rect, at)?, net)? {
                None => return Ok(None),
                Some(n) => near |= n,
            }
        }
        Ok(Some(near))
    }
    /// First clear kind in order; in rules mode prefer one without a near miss.
    pub(crate) fn choose(
        &mut self,
        kinds: &[usize],
        at: [i64; 2],
        net: u32,
    ) -> Result<Option<usize>> {
        let mut fallback = None;
        for &k in kinds {
            match self.via_probe(k, at, net)? {
                None => {}
                Some(near) if !near || !self.input.rules => return Ok(Some(k)),
                Some(_) => {
                    fallback = fallback.or(Some(k));
                }
            }
        }
        Ok(fallback)
    }
    pub(crate) fn add_wire(&mut self, w: &Wire) -> Result<()> {
        self.add(Blockage {
            layer: w.layer,
            rect: wire_rect(w, self.input.layers[w.layer as usize].width)?,
            net: Some(w.net),
        })
    }
    pub(crate) fn add_via(&mut self, v: &Via) -> Result<()> {
        for s in &self.input.vias[v.kind as usize].shapes {
            self.add(Blockage {
                layer: s.layer,
                rect: shifted(s.rect, v.at)?,
                net: Some(v.net),
            })?;
        }
        Ok(())
    }
}

pub(crate) fn preflight(input: &RouteInput, limits: &RouteLimits) -> Result<()> {
    if let Some(plan) = &input.tracks {
        limits.validate_track()?;
        crate::troute::check_plan(input, plan)?;
        if let Some(config) = &plan.single_owner_repair {
            config.validate(limits)?;
        }
    } else {
        limits.validate()?;
    }
    if !input.die.valid()
        || input.grid <= 0
        || input.step < input.grid
        || input.step % input.grid != 0
        || input.layers.len() > 24
        || input.layers.is_empty()
    {
        return Err(invalid("routing domain/grid/layers"));
    }
    if input.access_layer as usize >= input.layers.len()
        || input.layers[input.access_layer as usize].vertical.is_none()
    {
        return Err(invalid("routing access layer"));
    }
    for l in &input.layers {
        if l.width <= 0
            || l.width % 2 != 0
            || i128::from(l.width) % (2 * i128::from(input.grid)) != 0
            || l.spacing < 0
            || !crate::valid_object_name(&l.name)
        {
            return Err(invalid("routing layer width/spacing/name"));
        }
    }
    let mut pairs = HashSet::new();
    for v in &input.vias {
        if v.lower == v.upper
            || v.lower as usize >= input.layers.len()
            || v.upper as usize >= input.layers.len()
            || input.layers[v.lower as usize].vertical.is_none()
            || input.layers[v.upper as usize].vertical.is_none()
            || (!pairs.insert((v.lower.min(v.upper), v.lower.max(v.upper))) && !input.rules)
            || v.lower > v.upper
            || v.shapes.is_empty()
            || v.shapes.len() > 64
            || v.shapes
                .iter()
                .any(|s| s.layer as usize >= input.layers.len() || !s.rect.valid())
        {
            return Err(invalid("routing via definition"));
        }
        if !v.shapes.iter().any(|s| s.layer == v.lower)
            || !v.shapes.iter().any(|s| s.layer == v.upper)
        {
            return Err(invalid("via lacks routing enclosure"));
        }
    }
    if let Some(instances) = &input.instances {
        instances.validate(input.layers.len(), limits)?;
        if instances.len().saturating_add(input.obstacles.len()) > limits.max_obstacles {
            return Err(budget("retained instance-obstacle records"));
        }
    }
    let mut pins = HashSet::new();
    // The track router bounds terminals and classes by the obstacle cap.
    let max_terminals = if input.tracks.is_some() {
        limits.max_obstacles
    } else {
        16_000
    };
    if input.terminals.len() > max_terminals
        || input.obstacles.len() > limits.max_obstacles
        || input.access_classes.len() > max_terminals
        || (!input.rules && !input.access_classes.is_empty())
    {
        return Err(budget("routing input count"));
    }
    // Aggregate class geometry shares the obstacle cap (<= 200k rectangles).
    if input
        .access_classes
        .iter()
        .map(|c| c.shapes.len() + c.blockers.len())
        .sum::<usize>()
        > limits.max_obstacles
    {
        return Err(budget("routing access class geometry"));
    }
    for c in &input.access_classes {
        if c.shapes.is_empty()
            || c.shapes.len() > 64
            || c.blockers.len() > 4096
            || c.shapes
                .iter()
                .chain(&c.blockers)
                .any(|s| s.layer as usize >= input.layers.len() || !s.rect.valid())
        {
            return Err(invalid("routing access class"));
        }
    }
    for l in &input.layers {
        if l.min_area.is_some_and(|a| a <= 0) || l.min_step.is_some_and(|s| s <= 0) {
            return Err(invalid("routing layer area/step rule"));
        }
    }
    for t in &input.terminals {
        if let Some((class, origin)) = t.class {
            let c = input
                .access_classes
                .get(class as usize)
                .ok_or_else(|| invalid("routing terminal access class"))?;
            let moved: Vec<_> = c
                .shapes
                .iter()
                .map(|s| shifted(s.rect, origin).map(|r| (s.layer, r)))
                .collect::<Result<_>>()?;
            if moved.len() != t.shapes.len()
                || moved
                    .iter()
                    .zip(&t.shapes)
                    .any(|(m, s)| m.0 != s.layer || m.1 != s.rect)
            {
                return Err(invalid("routing terminal differs from its access class"));
            }
        }
        if !pins.insert(t.pin)
            || t.shapes.is_empty()
            || t.shapes.len() > 64
            || t.shapes.iter().any(|s| {
                s.layer as usize >= input.layers.len()
                    || !s.rect.valid()
                    || input.layers[s.layer as usize].vertical.is_none()
            })
        {
            return Err(invalid("routing terminal"));
        }
    }
    if !input.ties.is_empty() {
        if input.tracks.is_none() || !input.rules {
            return Err(invalid("supply ties need the track router"));
        }
        let mut per_net = HashMap::<u32, (usize, usize)>::new();
        for t in &input.terminals {
            let e = per_net.entry(t.net).or_default();
            e.0 += 1;
            e.1 += usize::from(t.driver);
        }
        for (i, tie) in input.ties.iter().enumerate() {
            if (i > 0 && input.ties[i - 1].net >= tie.net)
                || tie.net == tie.supply
                || per_net.get(&tie.net) != Some(&(2, 1))
                || per_net.contains_key(&tie.supply)
                || input
                    .ties
                    .binary_search_by_key(&tie.supply, |t| t.net)
                    .is_ok()
                || tie.zone.layer as usize >= input.layers.len()
                || input.layers[tie.zone.layer as usize].vertical.is_none()
                || !tie.zone.rect.valid()
                || !input.terminals.iter().any(|t| {
                    t.net == tie.net
                        && t.driver
                        && t.class.is_none()
                        && t.shapes.len() == 1
                        && t.shapes[0].layer == tie.zone.layer
                        && t.shapes[0].rect == tie.zone.rect
                })
            {
                return Err(invalid("supply tie"));
            }
        }
    }
    Ok(())
}
pub(crate) fn candidates(t: &Terminal, input: &RouteInput) -> Result<Vec<(u32, [i64; 2])>> {
    quartile_points(&t.shapes, input.grid, Some(input.die))
}
/// Fixed-via kinds grouped by connected layer pair, in first-use order.
pub(crate) struct ViaPair {
    pub(crate) lower: u32,
    pub(crate) upper: u32,
    pub(crate) kinds: Vec<usize>,
}
/// Via pairs in first-use order (shared by the lattice and track routers).
pub(crate) fn via_pairs(input: &RouteInput) -> Vec<ViaPair> {
    let mut pairs: Vec<ViaPair> = Vec::new();
    for (k, v) in input.vias.iter().enumerate() {
        if let Some(p) = pairs
            .iter_mut()
            .find(|p| p.lower == v.lower && p.upper == v.upper)
        {
            p.kinds.push(k);
        } else {
            pairs.push(ViaPair {
                lower: v.lower,
                upper: v.upper,
                kinds: vec![k],
            });
        }
    }
    pairs
}
struct Lattice {
    xs: Vec<i64>,
    ys: Vec<i64>,
    layers: Vec<u32>,
    slots: HashMap<u32, usize>,
    pairs: Vec<ViaPair>,
    len: usize,
}
impl Lattice {
    fn new(input: &RouteInput, limits: &RouteLimits) -> Result<Self> {
        let axis = |lo: i64, hi: i64| -> Result<BTreeSet<i64>> {
            let first = i128::from(lo).div_euclid(i128::from(input.grid)) * i128::from(input.grid);
            let count = (i128::from(hi) - first) / i128::from(input.step) + 1;
            if count > limits.max_axis as i128 {
                return Err(budget("routing axis count"));
            }
            let mut out = BTreeSet::new();
            for i in 0..count {
                let v = int(first + i * i128::from(input.step))?;
                if v >= lo && v <= hi {
                    out.insert(v);
                }
            }
            Ok(out)
        };
        let mut xs = axis(input.die.lx, input.die.ux)?;
        let mut ys = axis(input.die.ly, input.die.uy)?;
        for t in &input.terminals {
            for (_, p) in candidates(t, input)? {
                xs.insert(p[0]);
                ys.insert(p[1]);
            }
        }
        if xs.len() > limits.max_axis || ys.len() > limits.max_axis {
            return Err(budget("routing pin-coordinate axis count"));
        }
        let layers: Vec<_> = input
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.vertical.map(|_| i as u32))
            .collect();
        if layers.is_empty() || layers.len() > 8 {
            return Err(budget("routing grid layer count"));
        }
        let len = xs
            .len()
            .checked_mul(ys.len())
            .and_then(|n| n.checked_mul(layers.len()))
            .ok_or_else(|| budget("routing grid size overflow"))?;
        if len == 0 || len > limits.max_grid_nodes {
            return Err(budget("routing grid node count"));
        }
        let slots = layers.iter().enumerate().map(|(i, l)| (*l, i)).collect();
        let pairs = via_pairs(input);
        Ok(Self {
            xs: xs.into_iter().collect(),
            ys: ys.into_iter().collect(),
            layers,
            slots,
            pairs,
            len,
        })
    }
    fn id(&self, layer: u32, p: [i64; 2]) -> u32 {
        let x = self.xs.binary_search(&p[0]).unwrap();
        let y = self.ys.binary_search(&p[1]).unwrap();
        (self.slots[&layer] * self.xs.len() * self.ys.len() + y * self.xs.len() + x) as u32
    }
    fn point(&self, id: u32) -> (u32, [i64; 2]) {
        let i = id as usize;
        let plane = self.xs.len() * self.ys.len();
        let k = i % plane;
        (
            self.layers[i / plane],
            [self.xs[k % self.xs.len()], self.ys[k / self.xs.len()]],
        )
    }
    fn neighbors(&self, id: u32) -> Vec<(u32, Option<usize>)> {
        let i = id as usize;
        let nx = self.xs.len();
        let ny = self.ys.len();
        let plane = nx * ny;
        let x = i % nx;
        let y = (i % plane) / nx;
        let mut n = Vec::with_capacity(8);
        if x > 0 {
            n.push((id - 1, None));
        }
        if x + 1 < nx {
            n.push((id + 1, None));
        }
        if y > 0 {
            n.push((id - nx as u32, None));
        }
        if y + 1 < ny {
            n.push((id + nx as u32, None));
        }
        let (l, p) = self.point(id);
        for (k, v) in self.pairs.iter().enumerate() {
            if v.lower == l {
                n.push((self.id(v.upper, p), Some(k)));
            } else if v.upper == l {
                n.push((self.id(v.lower, p), Some(k)));
            }
        }
        n
    }
}
pub(crate) fn distance(a: [i64; 2], b: [i64; 2]) -> Result<u64> {
    let d =
        (i128::from(a[0]) - i128::from(b[0])).abs() + (i128::from(a[1]) - i128::from(b[1])).abs();
    u64::try_from(d).map_err(|_| invalid("routing distance overflow"))
}

/// A reserved pin escape: lattice edges with via kind (`u32::MAX` = wire).
struct AccessPlan {
    access: Access,
    top: u32,
    edges: Vec<((u32, u32), u32)>,
}
pub(crate) const WIRE: u32 = u32::MAX;

/// One precomputed landing for a (master, orientation, pin) class: relative
/// point, legal upward via kinds (pads inside the pin first) and whether any
/// legal pad lies entirely inside a pin rectangle.
pub(crate) struct AccessEntry {
    pub(crate) layer: u32,
    pub(crate) at: [i64; 2],
    pub(crate) kinds: Vec<usize>,
    pub(crate) fits: bool,
}

pub(crate) fn quartile_points(
    shapes: &[LayerRect],
    grid: i64,
    die: Option<Rect>,
) -> Result<Vec<(u32, [i64; 2])>> {
    let mut seen = BTreeSet::new();
    let mut points = Vec::new();
    for s in shapes {
        for i in [2, 1, 3] {
            for j in [2, 1, 3] {
                let x =
                    i128::from(s.rect.lx) + (i128::from(s.rect.ux) - i128::from(s.rect.lx)) * i / 4;
                let y =
                    i128::from(s.rect.ly) + (i128::from(s.rect.uy) - i128::from(s.rect.ly)) * j / 4;
                let snap = |v: i128| {
                    int((v + i128::from(grid) / 2).div_euclid(i128::from(grid)) * i128::from(grid))
                };
                let p = [snap(x)?, snap(y)?];
                if contains(s.rect, p)
                    && die.is_none_or(|d| contains(d, p))
                    && seen.insert((s.layer, p))
                {
                    points.push((s.layer, p));
                }
            }
        }
    }
    if points.len() > 128 {
        return Err(budget("pin access candidates"));
    }
    Ok(points)
}

/// Build the access table of one class once: each candidate via is checked
/// against the master's own other-net geometry (spacing), against same-pin
/// notches, and against LEF MINSTEP when its pad protrudes from the pin.
fn access_table(
    input: &RouteInput,
    grid: &Lattice,
    class: &AccessClass,
    work: &mut usize,
    limits: &RouteLimits,
) -> Result<Vec<AccessEntry>> {
    let points = quartile_points(&class.shapes, input.grid, None)?;
    access_table_at(
        input,
        &grid.slots,
        &grid.pairs,
        class,
        &points,
        work,
        limits,
    )
}
/// Access table over the given relative landing points (see `access_table`).
pub(crate) fn access_table_at(
    input: &RouteInput,
    slots: &HashMap<u32, usize>,
    pairs: &[ViaPair],
    class: &AccessClass,
    points: &[(u32, [i64; 2])],
    work: &mut usize,
    limits: &RouteLimits,
) -> Result<Vec<AccessEntry>> {
    let cap = limits.max_comparisons;
    let access = slots[&input.access_layer];
    let mut entries = Vec::new();
    for &(layer, at) in points {
        let pins = |l: u32| -> Vec<Rect> {
            class
                .shapes
                .iter()
                .filter(|s| s.layer == l)
                .map(|s| s.rect)
                .collect()
        };
        let own = pins(layer);
        let (mut fit, mut other) = (Vec::new(), Vec::new());
        for pair in pairs {
            if pair.lower != layer || slots.get(&pair.upper).is_none_or(|s| *s > access) {
                continue;
            }
            'kind: for &k in &pair.kinds {
                let mut fits = false;
                for s in &input.vias[k].shapes {
                    let rect = shifted(s.rect, at)?;
                    let rule = &input.layers[s.layer as usize];
                    let halo = expand(rect, rule.spacing)?;
                    for b in class.blockers.iter().filter(|b| b.layer == s.layer) {
                        *work += 1;
                        if overlap(halo, b.rect) && !routing_separated(rule, rect, b.rect) {
                            continue 'kind;
                        }
                    }
                    if rule.vertical.is_none() {
                        continue;
                    }
                    let same = pins(s.layer);
                    if !same.is_empty()
                        && !route_rules::local_ok(rect, &same, rule.spacing, work, cap)?
                    {
                        continue 'kind;
                    }
                    if s.layer == layer {
                        fits = own.iter().any(|p| p.contains(rect));
                        if !fits
                            && let Some(step) = rule.min_step
                            && let Some(p) = own.iter().find(|p| contains(**p, at))
                        {
                            let sides = [
                                p.lx - rect.lx,
                                rect.ux - p.ux,
                                p.ly - rect.ly,
                                rect.uy - p.uy,
                            ];
                            if sides.iter().any(|d| *d > 0 && *d < step) {
                                continue 'kind;
                            }
                        }
                    }
                }
                if fits {
                    fit.push(k);
                } else {
                    other.push(k);
                }
            }
        }
        entries.push(AccessEntry {
            layer,
            at,
            fits: !fit.is_empty(),
            kinds: fit.into_iter().chain(other).collect(),
        });
        if *work > cap {
            return Err(budget("pin access table work"));
        }
    }
    entries.sort_by_key(|e| !e.fits);
    Ok(entries)
}

/// Access search start: layer, point, initial cost and table via kinds.
type Start = (u32, [i64; 2], u64, Option<Vec<usize>>);

fn reserve_access(
    input: &RouteInput,
    grid: &Lattice,
    spatial: &mut Spatial<'_>,
    terminals: &BTreeMap<u32, Vec<&Terminal>>,
    limits: &RouteLimits,
) -> Result<(HashMap<u32, AccessPlan>, usize)> {
    let mut plans = HashMap::new();
    let mut total = 0usize;
    let mut tables = BTreeMap::<u32, Vec<AccessEntry>>::new();
    for group in terminals.values().filter(|g| g.len() >= 2) {
        for t in group {
            let generic = candidates(t, input)?;
            // (layer, point) -> (start cost, table via kinds)
            let mut starts: Vec<Start> = Vec::new();
            if input.rules
                && let Some((class, origin)) = t.class
            {
                if let std::collections::btree_map::Entry::Vacant(slot) = tables.entry(class) {
                    let mut work = 0usize;
                    let table = access_table(
                        input,
                        grid,
                        &input.access_classes[class as usize],
                        &mut work,
                        limits,
                    )?;
                    spatial.work = spatial
                        .work
                        .checked_add(work)
                        .ok_or_else(|| budget("routing comparison limit"))?;
                    slot.insert(table);
                }
                let known: BTreeSet<_> = generic.iter().copied().collect();
                for e in &tables[&class] {
                    let p = [
                        int(i128::from(e.at[0]) + i128::from(origin[0]))?,
                        int(i128::from(e.at[1]) + i128::from(origin[1]))?,
                    ];
                    if known.contains(&(e.layer, p)) {
                        let cost = if e.fits { 0 } else { input.step as u64 };
                        starts.push((e.layer, p, cost, Some(e.kinds.clone())));
                    }
                }
            }
            if starts.is_empty() {
                starts = generic.iter().map(|&(l, p)| (l, p, 0, None)).collect();
            }
            if starts.is_empty() {
                return Err(no_route("no access candidates"));
            }
            let pin_shapes = |l: u32| -> Vec<Rect> {
                t.shapes
                    .iter()
                    .filter(|s| s.layer == l)
                    .map(|s| s.rect)
                    .collect()
            };
            let table_kinds: HashMap<(u32, [i64; 2]), Vec<usize>> = starts
                .iter()
                .filter_map(|(l, p, _, k)| k.clone().map(|k| ((*l, *p), k)))
                .collect();
            let reach = input
                .step
                .checked_mul(4)
                .ok_or_else(|| invalid("access radius overflow"))?;
            let bounds = expand(
                Rect {
                    lx: starts.iter().map(|p| p.1[0]).min().unwrap(),
                    ly: starts.iter().map(|p| p.1[1]).min().unwrap(),
                    ux: starts.iter().map(|p| p.1[0]).max().unwrap(),
                    uy: starts.iter().map(|p| p.1[1]).max().unwrap(),
                },
                reach,
            )?;
            let mut heap = BinaryHeap::new();
            // node -> (cost, parent, via kind of the parent edge)
            let mut visited = HashMap::<u32, (u64, u32, u32)>::new();
            for (layer, at, cost, _) in &starts {
                let (layer, at, cost) = (*layer, *at, *cost);
                let half = input.layers[layer as usize].width / 2;
                let r = Rect {
                    lx: int(i128::from(at[0]) - i128::from(half))?,
                    ly: int(i128::from(at[1]) - i128::from(half))?,
                    ux: int(i128::from(at[0]) + i128::from(half))?,
                    uy: int(i128::from(at[1]) + i128::from(half))?,
                };
                if spatial.clear(layer, r, t.net)? {
                    let id = grid.id(layer, at);
                    if visited.get(&id).is_none_or(|v| v.0 > cost) {
                        visited.insert(id, (cost, u32::MAX, WIRE));
                        heap.push(Reverse((cost, id)));
                    }
                }
            }
            let mut found = None;
            let mut expanded = 0usize;
            while let Some(Reverse((cost, id))) = heap.pop() {
                if visited[&id].0 != cost {
                    continue;
                }
                let (layer, at) = grid.point(id);
                if grid.slots[&layer] >= grid.slots[&input.access_layer] {
                    found = Some(id);
                    break;
                }
                expanded += 1;
                total += 1;
                if expanded > limits.max_search_expansions || total > limits.max_expansions {
                    return Err(budget("pin access expansion limit"));
                }
                for (next, via) in grid.neighbors(id) {
                    let (nl, q) = grid.point(next);
                    if grid.slots[&nl] > grid.slots[&input.access_layer] || !contains(bounds, q) {
                        continue;
                    }
                    let length = if via.is_some() {
                        input.step as u64
                    } else {
                        distance(at, q)?
                    };
                    let nc = cost
                        .checked_add(length)
                        .ok_or_else(|| budget("access cost overflow"))?;
                    if visited.get(&next).is_some_and(|v| v.0 <= nc) {
                        continue;
                    }
                    let kind = if let Some(pi) = via {
                        let pair = &grid.pairs[pi];
                        let listed: Vec<usize> = match table_kinds.get(&(layer, at)) {
                            Some(ks) if pair.lower == layer => ks
                                .iter()
                                .copied()
                                .filter(|k| pair.kinds.contains(k))
                                .collect(),
                            _ => pair.kinds.clone(),
                        };
                        let mut chosen = None;
                        for k in listed {
                            if input.rules {
                                let mut ok = true;
                                for s in &input.vias[k].shapes {
                                    let rule = &input.layers[s.layer as usize];
                                    let own = pin_shapes(s.layer);
                                    if rule.vertical.is_some()
                                        && !own.is_empty()
                                        && !route_rules::local_ok(
                                            shifted(s.rect, at)?,
                                            &own,
                                            rule.spacing,
                                            &mut spatial.work,
                                            limits.max_comparisons,
                                        )?
                                    {
                                        ok = false;
                                        break;
                                    }
                                }
                                if !ok {
                                    continue;
                                }
                            }
                            if spatial.via_clear(k, at, t.net)? {
                                chosen = Some(k as u32);
                                break;
                            }
                        }
                        match chosen {
                            Some(k) => k,
                            None => continue,
                        }
                    } else {
                        let rect = wire_rect(
                            &Wire {
                                net: t.net,
                                layer,
                                start: at,
                                end: q,
                            },
                            input.layers[layer as usize].width,
                        )?;
                        if input.rules {
                            let own = pin_shapes(layer);
                            if !own.is_empty()
                                && !route_rules::local_ok(
                                    rect,
                                    &own,
                                    input.layers[layer as usize].spacing,
                                    &mut spatial.work,
                                    limits.max_comparisons,
                                )?
                            {
                                continue;
                            }
                        }
                        if !spatial.clear(layer, rect, t.net)? {
                            continue;
                        }
                        WIRE
                    };
                    if heap.len() >= limits.max_heap || visited.len() >= limits.max_grid_nodes {
                        return Err(budget("pin access frontier"));
                    }
                    visited.insert(next, (nc, id, kind));
                    heap.push(Reverse((nc, next)));
                }
            }
            let top = found
                .ok_or_else(|| no_route(&format!("pin {} has no legal local escape", t.pin)))?;
            let mut current = top;
            let mut edges = Vec::new();
            while visited[&current].1 != u32::MAX {
                let (_, next, kind) = visited[&current];
                edges.push(((current.min(next), current.max(next)), kind));
                current = next;
                if edges.len() > limits.max_segments {
                    return Err(budget("access path limit"));
                }
            }
            let (layer, at) = grid.point(current);
            for &((a, b), kind) in &edges {
                let (l, p) = grid.point(a);
                let (other, q) = grid.point(b);
                if l == other {
                    spatial.add_wire(&Wire {
                        net: t.net,
                        layer: l,
                        start: p,
                        end: q,
                    })?;
                } else {
                    spatial.add_via(&Via {
                        net: t.net,
                        kind,
                        at: p,
                    })?;
                }
            }
            plans.insert(
                t.pin,
                AccessPlan {
                    access: Access {
                        pin: t.pin,
                        net: t.net,
                        layer,
                        at,
                    },
                    top,
                    edges,
                },
            );
        }
    }
    Ok((plans, total))
}

/// Whole-route memory estimate of the track router for this input (`None`
/// for the lattice router), as `route_cpu_ref` enforces it.
pub fn track_memory_estimate(
    input: &RouteInput,
    limits: &RouteLimits,
) -> Result<Option<crate::troute::MemoryEstimate>> {
    crate::troute::estimate_for(input, limits)
}

pub use crate::troute::PriorRoutes;

/// Track-router ECO: keep the still-valid prior geometry (see
/// `troute::route_with_prior`); the lattice router has no ECO mode.
pub fn route_cpu_ref_with_prior(
    input: &RouteInput,
    limits: &RouteLimits,
    prior: &PriorRoutes,
) -> Result<Routes> {
    preflight(input, limits)?;
    if input.tracks.is_none() {
        return Err(invalid(
            "ECO routing with a prior route needs the track router",
        ));
    }
    crate::troute::route_with_prior(input, limits, Some(prior))
}

pub fn route_cpu_ref(input: &RouteInput, limits: &RouteLimits) -> Result<Routes> {
    preflight(input, limits)?;
    if input.tracks.is_some() {
        return crate::troute::route(input, limits);
    }
    let grid = Lattice::new(input, limits)?;
    let mut spatial = Spatial::new(input, limits)?;
    let mut groups = BTreeMap::<u32, Vec<&Terminal>>::new();
    for t in &input.terminals {
        groups.entry(t.net).or_default().push(t);
    }
    let (plans, access_expansions) = reserve_access(input, &grid, &mut spatial, &groups, limits)?;
    let mut out = Routes {
        wires: Vec::new(),
        vias: Vec::new(),
        access: Vec::new(),
        patches: Vec::new(),
        nets: Vec::new(),
        stats: RouteStats {
            grid_nodes: grid.len,
            expansions: access_expansions,
            ..Default::default()
        },
    };
    let mut costs = vec![u64::MAX; grid.len];
    let mut parents = vec![u32::MAX; grid.len];
    let mut parent_kinds = vec![WIRE; grid.len];
    let mut tags = vec![0u32; grid.len];
    let mut epoch = 0u32;
    for (net, mut terminals) in groups {
        if terminals.len() < 2 {
            continue;
        }
        terminals.sort_by_key(|p| (!p.driver, p.pin));
        if terminals.iter().filter(|p| p.driver).count() != 1 {
            return Err(invalid("routed net needs one driver"));
        }
        out.nets.push(net);
        let root = &plans[&terminals[0].pin];
        out.access.push(root.access.clone());
        let mut tree = HashSet::from([root.top]);
        let mut edges: BTreeMap<(u32, u32), u32> = root.edges.iter().copied().collect();
        for &((a, b), _) in &root.edges {
            tree.insert(a);
            tree.insert(b);
        }
        for terminal in &terminals[1..] {
            let plan = &plans[&terminal.pin];
            out.access.push(plan.access.clone());
            let starts = vec![plan.top];
            epoch = epoch
                .checked_add(1)
                .ok_or_else(|| budget("routing search count"))?;
            let mut heap = BinaryHeap::new();
            let points: Vec<_> = tree.iter().map(|id| grid.point(*id).1).collect();
            let bbox = Rect {
                lx: points.iter().map(|p| p[0]).min().unwrap(),
                ly: points.iter().map(|p| p[1]).min().unwrap(),
                ux: points.iter().map(|p| p[0]).max().unwrap(),
                uy: points.iter().map(|p| p[1]).max().unwrap(),
            };
            let heuristic = |p: [i64; 2]| {
                distance(
                    p,
                    [p[0].clamp(bbox.lx, bbox.ux), p[1].clamp(bbox.ly, bbox.uy)],
                )
            };
            for &id in &starts {
                costs[id as usize] = 0;
                parents[id as usize] = u32::MAX;
                parent_kinds[id as usize] = WIRE;
                tags[id as usize] = epoch;
                heap.push(Reverse((heuristic(grid.point(id).1)?, 0u64, id)));
            }
            let mut found = None;
            let mut expanded = 0;
            while let Some(Reverse((_, cost, id))) = heap.pop() {
                if tags[id as usize] != epoch || costs[id as usize] != cost {
                    continue;
                }
                if tree.contains(&id) {
                    found = Some(id);
                    break;
                }
                expanded += 1;
                out.stats.expansions += 1;
                if expanded > limits.max_search_expansions {
                    return Err(budget(&format!(
                        "routing expansion limit: per-sink search, pin {} net {net}, {} nets done",
                        terminal.pin,
                        out.nets.len() - 1
                    )));
                }
                if out.stats.expansions > limits.max_expansions {
                    return Err(budget(&format!(
                        "routing expansion limit: total, pin {} net {net}, {} nets done",
                        terminal.pin,
                        out.nets.len() - 1
                    )));
                }
                let (layer, p) = grid.point(id);
                for (next, via) in grid.neighbors(id) {
                    let (_, q) = grid.point(next);
                    let length = if via.is_some() {
                        u64::try_from(input.step)
                            .unwrap()
                            .checked_mul(3)
                            .ok_or_else(|| budget("via cost overflow"))?
                    } else {
                        distance(p, q)?
                    };
                    let preferred = input.layers[layer as usize].vertical.unwrap();
                    let multiplier = if via.is_none()
                        && ((preferred && p[1] == q[1]) || (!preferred && p[0] == q[0]))
                    {
                        2
                    } else {
                        1
                    };
                    let next_cost = cost
                        .checked_add(
                            length
                                .checked_mul(multiplier)
                                .ok_or_else(|| budget("routing cost"))?,
                        )
                        .ok_or_else(|| budget("routing path cost"))?;
                    if tags[next as usize] == epoch && costs[next as usize] <= next_cost {
                        continue;
                    }
                    let kind = if let Some(pi) = via {
                        match spatial.choose(&grid.pairs[pi].kinds, p, net)? {
                            Some(k) => k as u32,
                            None => continue,
                        }
                    } else {
                        if !spatial.clear(
                            layer,
                            wire_rect(
                                &Wire {
                                    net,
                                    layer,
                                    start: p,
                                    end: q,
                                },
                                input.layers[layer as usize].width,
                            )?,
                            net,
                        )? {
                            continue;
                        }
                        WIRE
                    };
                    tags[next as usize] = epoch;
                    costs[next as usize] = next_cost;
                    parents[next as usize] = id;
                    parent_kinds[next as usize] = kind;
                    if heap.len() >= limits.max_heap {
                        return Err(budget("routing frontier limit"));
                    }
                    heap.push(Reverse((
                        next_cost
                            .checked_add(heuristic(q)?)
                            .ok_or_else(|| budget("routing estimate overflow"))?,
                        next_cost,
                        next,
                    )));
                }
            }
            let mut current = found.ok_or_else(|| {
                no_route(&format!("pin {} exhausted finite lattice", terminal.pin))
            })?;
            let mut length = 0usize;
            while parents[current as usize] != u32::MAX {
                let next = parents[current as usize];
                edges
                    .entry((current.min(next), current.max(next)))
                    .or_insert(parent_kinds[current as usize]);
                tree.insert(current);
                current = next;
                length += 1;
                if length > limits.max_search_expansions || edges.len() > limits.max_segments {
                    return Err(budget("routing trace/segment limit"));
                }
            }
            tree.insert(current);
            for &((a, b), kind) in &plan.edges {
                edges.insert((a, b), kind);
                tree.insert(a);
                tree.insert(b);
            }
        }
        for ((a, b), kind) in edges {
            if out.wires.len() + out.vias.len() >= limits.max_segments {
                return Err(budget("routing output segment limit"));
            }
            let (l, p) = grid.point(a);
            let (other, q) = grid.point(b);
            if l == other {
                let wire = Wire {
                    net,
                    layer: l,
                    start: p,
                    end: q,
                };
                out.stats.wire_length_dbu = out
                    .stats
                    .wire_length_dbu
                    .checked_add(distance(p, q)?)
                    .ok_or_else(|| budget("total wire length"))?;
                spatial.add_wire(&wire)?;
                out.wires.push(wire);
            } else {
                if kind == WIRE
                    || input.vias.get(kind as usize).is_none_or(|v| {
                        !((v.lower == l && v.upper == other) || (v.upper == l && v.lower == other))
                    })
                {
                    return Err(invalid("missing path via"));
                }
                let v = Via { net, kind, at: p };
                spatial.add_via(&v)?;
                out.vias.push(v);
            }
        }
    }
    out.stats.comparisons = spatial.work;
    if input.rules {
        let work = cleanup(input, &mut out, limits)?;
        out.stats.comparisons = out
            .stats
            .comparisons
            .checked_add(work)
            .ok_or_else(|| budget("routing comparison limit"))?;
        if out.stats.comparisons > limits.max_comparisons {
            return Err(budget("routing comparison limit"));
        }
        out.stats.wire_length_dbu = out.wires.iter().try_fold(0u64, |n, w| {
            n.checked_add(distance(w.start, w.end)?)
                .ok_or_else(|| budget("total wire length"))
        })?;
    }
    validate(input, &out, limits)?;
    Ok(out)
}

#[cfg(test)]
type Node = (u32, u32, i64, i64);

/// Remove dead-end wires/vias that lead to no access point (bounded by edges).
#[cfg(test)]
fn prune_legacy(input: &RouteInput, out: &mut Routes) {
    let anchors: BTreeSet<Node> = out
        .access
        .iter()
        .map(|a| (a.net, a.layer, a.at[0], a.at[1]))
        .collect();
    let nw = out.wires.len();
    let ends = |i: usize| -> (Node, Node) {
        if i < nw {
            let w = &out.wires[i];
            (
                (w.net, w.layer, w.start[0], w.start[1]),
                (w.net, w.layer, w.end[0], w.end[1]),
            )
        } else {
            let v = &out.vias[i - nw];
            let s = &input.vias[v.kind as usize];
            (
                (v.net, s.lower, v.at[0], v.at[1]),
                (v.net, s.upper, v.at[0], v.at[1]),
            )
        }
    };
    let total = nw + out.vias.len();
    let mut incident = BTreeMap::<Node, Vec<usize>>::new();
    for i in 0..total {
        let (a, b) = ends(i);
        incident.entry(a).or_default().push(i);
        incident.entry(b).or_default().push(i);
    }
    let mut degree: BTreeMap<Node, usize> = incident.iter().map(|(k, v)| (*k, v.len())).collect();
    let mut alive = vec![true; total];
    let mut queue: BTreeSet<Node> = degree
        .iter()
        .filter(|(n, d)| **d == 1 && !anchors.contains(n))
        .map(|(n, _)| *n)
        .collect();
    while let Some(node) = queue.pop_first() {
        if degree[&node] != 1 {
            continue;
        }
        let Some(&e) = incident[&node].iter().find(|&&e| alive[e]) else {
            continue;
        };
        alive[e] = false;
        let (a, b) = ends(e);
        for end in [a, b] {
            let d = degree.get_mut(&end).unwrap();
            *d -= 1;
            if *d == 1 && !anchors.contains(&end) {
                queue.insert(end);
            }
        }
    }
    let mut i = 0;
    out.wires.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    let mut j = nw;
    out.vias.retain(|_| {
        j += 1;
        alive[j - 1]
    });
}

/// Prune one endpoint graph at a time; output order and cyclic metal remain unchanged.
fn prune(input: &RouteInput, out: &mut Routes, limits: &RouteLimits) -> Result<usize> {
    let mut work = 0;
    let index = crate::route_graph::NetIndex::new(out, limits, &mut work)?;
    let nw = out.wires.len();
    let mut alive = vec![true; nw + out.vias.len()];
    for &net in index.nets() {
        let sources = index.sources(out, net);
        let graph = crate::route_graph::Graph::new(input, out, sources, limits, &mut work)?;
        let kept = graph.pruned(&mut work, limits.max_comparisons)?;
        for (&i, &keep) in sources.wires.iter().zip(&kept) {
            alive[i as usize] = keep;
        }
        for (&i, &keep) in sources.vias.iter().zip(&kept[sources.wires.len()..]) {
            alive[nw + i as usize] = keep;
        }
    }
    let mut i = 0;
    out.wires.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    out.vias.retain(|_| {
        i += 1;
        alive[i - 1]
    });
    Ok(work)
}

/// Post-route same-net legalization: prune dead ends, then up to eight rounds
/// of notch/point-contact patches, minimum-area stubs and same-net wide-gap
/// bridges, each checked against other nets. Unresolved violations fail explicitly
/// (HEAD stays unchanged).
pub(crate) fn cleanup(input: &RouteInput, out: &mut Routes, limits: &RouteLimits) -> Result<usize> {
    let (work, remaining) = cleanup_report(input, out, limits)?;
    if let Some(v) = remaining.first() {
        return Err(no_route(&format!(
            "ROUTING_RULE unresolved {} ({} total)",
            v.describe(),
            remaining.len()
        )));
    }
    Ok(work)
}
/// `cleanup` without the final failure: returns the work and every violation
/// left after the bounded repair rounds (the track router re-routes them).
pub(crate) fn cleanup_report(
    input: &RouteInput,
    out: &mut Routes,
    limits: &RouteLimits,
) -> Result<(usize, Vec<Violation>)> {
    let mut work = prune(input, out, limits)?;
    let mut spatial = Spatial::new(input, limits)?;
    for w in &out.wires {
        spatial.add_wire(w)?;
    }
    for v in &out.vias {
        spatial.add_via(v)?;
    }
    for p in &out.patches {
        spatial.add(Blockage {
            layer: p.layer,
            rect: p.rect,
            net: Some(p.net),
        })?;
    }
    let mut tried = BTreeSet::new();
    for _ in 0..8 {
        let mut violations = route_rules::check(input, out, limits, &mut work)?;
        if violations.is_empty() {
            violations = route_rules::wide_check(input, out, limits, &mut work)?
                .into_iter()
                .filter(|v| matches!(v, Violation::Wide { owner, other, .. } if owner == other))
                .collect();
            if violations.is_empty() {
                break;
            }
        }
        let mut changed = false;
        let cache = if input.tracks.is_some()
            && violations
                .iter()
                .any(|v| matches!(v, Violation::Area { .. }))
        {
            Some(crate::rule_index::RuleIndex::new(
                input,
                out,
                &mut work,
                limits.max_comparisons,
            )?)
        } else {
            None
        };
        for v in &violations {
            match v {
                Violation::Spacing { net, layer, a, b } => {
                    let width = input.layers[*layer as usize].width;
                    let fill = if input.tracks.is_some() {
                        route_rules::fill_for_robust(*a, *b, width, input.grid)
                    } else {
                        route_rules::fill_for(*a, *b, width, input.grid)
                    };
                    let fresh = tried.insert((*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy));
                    let fills = if fresh && spatial.clear(*layer, fill, *net)? {
                        vec![fill]
                    } else if fresh && input.tracks.is_some() {
                        // The robust box reaches another net: try the smaller bridges.
                        let mut chosen = Vec::new();
                        for rects in route_rules::bridges(*a, *b, width, input.grid) {
                            let mut clear = true;
                            for r in &rects {
                                clear = clear && spatial.clear(*layer, *r, *net)?;
                            }
                            if clear {
                                chosen = rects;
                                break;
                            }
                        }
                        chosen
                    } else {
                        Vec::new()
                    };
                    for rect in fills {
                        spatial.add(Blockage {
                            layer: *layer,
                            rect,
                            net: Some(*net),
                        })?;
                        out.patches.push(Patch {
                            net: *net,
                            layer: *layer,
                            rect,
                        });
                        changed = true;
                    }
                }
                Violation::Area {
                    net, layer, shapes, ..
                } => {
                    if let Some(stubs) = area_stub(
                        input,
                        out,
                        &mut spatial,
                        *net,
                        *layer,
                        shapes,
                        limits,
                        &mut work,
                        cache.as_ref(),
                    )? {
                        for w in stubs {
                            spatial.add_wire(&w)?;
                            out.wires.push(w);
                        }
                        changed = true;
                    }
                }
                Violation::Hole { net, layer, rect } => {
                    let fill = Rect {
                        lx: rect.lx.div_euclid(input.grid) * input.grid,
                        ly: rect.ly.div_euclid(input.grid) * input.grid,
                        ux: -((-rect.ux).div_euclid(input.grid) * input.grid),
                        uy: -((-rect.uy).div_euclid(input.grid) * input.grid),
                    };
                    if tried.insert((*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy))
                        && spatial.clear(*layer, fill, *net)?
                    {
                        spatial.add(Blockage {
                            layer: *layer,
                            rect: fill,
                            net: Some(*net),
                        })?;
                        out.patches.push(Patch {
                            net: *net,
                            layer: *layer,
                            rect: fill,
                        });
                        changed = true;
                    }
                }
                Violation::Neck { net, layer, a, b } => {
                    let width = input.layers[*layer as usize].width;
                    let Some((_, mut fills)) = route_rules::neck(*a, *b, width) else {
                        continue;
                    };
                    fills.sort_by_key(|f| i128::from(f.ux - f.lx) * i128::from(f.uy - f.ly));
                    for fill in fills {
                        if tried.insert((*net, *layer, fill.lx, fill.ly, fill.ux, fill.uy))
                            && spatial.clear(*layer, fill, *net)?
                        {
                            spatial.add(Blockage {
                                layer: *layer,
                                rect: fill,
                                net: Some(*net),
                            })?;
                            out.patches.push(Patch {
                                net: *net,
                                layer: *layer,
                                rect: fill,
                            });
                            changed = true;
                            break;
                        }
                    }
                }
                Violation::Wide {
                    owner,
                    other,
                    layer,
                    wide,
                    near,
                    ..
                } if owner == other => {
                    // Fill an open gap of the same electrical net across
                    // their illegal wide-spacing gap. All rules are checked
                    // again; a blocked bridge is left for strict re-routing.
                    let width = input.layers[*layer as usize].width;
                    let fill = route_rules::fill_for_robust(*wide, *near, width, input.grid);
                    if tried.insert((*owner, *layer, fill.lx, fill.ly, fill.ux, fill.uy))
                        && spatial.clear(*layer, fill, *owner)?
                    {
                        if out.patches.len() >= limits.max_segments {
                            return Err(budget("wide-metal patch count"));
                        }
                        spatial.add(Blockage {
                            layer: *layer,
                            rect: fill,
                            net: Some(*owner),
                        })?;
                        out.patches.push(Patch {
                            net: *owner,
                            layer: *layer,
                            rect: fill,
                        });
                        changed = true;
                    }
                }
                Violation::Floating { .. } | Violation::Wide { .. } => {}
            }
        }
        if !changed {
            break;
        }
    }
    let mut remaining = route_rules::check(input, out, limits, &mut work)?;
    // Remaining wide pairs require re-routing the owner or a routed neighbour.
    remaining.extend(route_rules::wide_check(input, out, limits, &mut work)?);
    let work = work
        .checked_add(spatial.work)
        .ok_or_else(|| budget("routing comparison limit"))?;
    Ok((work, remaining))
}

/// Shortest default-width stub from a via/wire node of the component that
/// lifts its merged area to the LEF minimum without new same-net notches.
/// When no single direction has room (a pad between two neighbours on its
/// track), the extension is split into two stubs on opposite sides.
#[allow(clippy::too_many_arguments)]
fn area_stub(
    input: &RouteInput,
    out: &Routes,
    spatial: &mut Spatial<'_>,
    net: u32,
    layer: u32,
    shapes: &[Rect],
    limits: &RouteLimits,
    work: &mut usize,
    cache: Option<&crate::rule_index::RuleIndex>,
) -> Result<Option<Vec<Wire>>> {
    let rule = &input.layers[layer as usize];
    let Some(min) = rule.min_area else {
        return Ok(None);
    };
    let cap = limits.max_comparisons;
    let owned;
    let index = if let Some(index) = cache {
        index
    } else {
        owned = crate::rule_index::RuleIndex::new(input, out, work, cap)?;
        &owned
    };
    let inside = |p: [i64; 2]| shapes.iter().any(|s| contains(*s, p));
    let mut anchors: Vec<[i64; 2]> = Vec::new();
    let vias: BTreeSet<[i64; 2]> = index
        .vias(out, net)?
        .filter(|v| {
            v.net == net
                && input.vias[v.kind as usize]
                    .shapes
                    .iter()
                    .any(|s| s.layer == layer)
                && inside(v.at)
        })
        .map(|v| v.at)
        .collect();
    anchors.extend(vias.iter().copied());
    let ends: BTreeSet<[i64; 2]> = index
        .wires(out, net)?
        .filter(|w| w.net == net && w.layer == layer)
        .flat_map(|w| [w.start, w.end])
        .filter(|p| inside(*p))
        .collect();
    anchors.extend(ends.into_iter().filter(|p| !vias.contains(p)));
    let others: Vec<Rect> = index
        .collect(input, out, net, limits, work)?
        .remove(&(net, layer))
        .unwrap_or_default()
        .into_iter()
        .map(|shape| shape.rect)
        .collect();
    let span = shapes
        .iter()
        .map(|s| (s.ux - s.lx).max(s.uy - s.ly))
        .max()
        .unwrap_or(0);
    let steps = ((min / rule.width.max(1)) + 2 * span + 2 * input.grid) / input.grid + 1;
    let steps = steps.min(1 << 16);
    let dirs: [[i64; 2]; 4] = if rule.vertical == Some(true) {
        [[0, 1], [0, -1], [1, 0], [-1, 0]]
    } else {
        [[1, 0], [-1, 0], [0, 1], [0, -1]]
    };
    let stub = |a: [i64; 2], d: [i64; 2], n: i64| -> Result<(Wire, Rect)> {
        let end = [
            int(i128::from(a[0]) + i128::from(d[0]) * i128::from(n) * i128::from(input.grid))?,
            int(i128::from(a[1]) + i128::from(d[1]) * i128::from(n) * i128::from(input.grid))?,
        ];
        let w = Wire {
            net,
            layer,
            start: a,
            end,
        };
        let r = wire_rect(&w, rule.width)?;
        Ok((w, r))
    };
    // Shortest n in 1..=steps whose stub (plus `extra`) reaches the minimum.
    let shortest =
        |a: [i64; 2], d: [i64; 2], extra: Option<Rect>, work: &mut usize| -> Result<Option<i64>> {
            let area = |n: i64, work: &mut usize| -> Result<i128> {
                let (_, r) = stub(a, d, n)?;
                let mut all = shapes.to_vec();
                all.extend(extra);
                all.push(r);
                route_rules::union_area(&all, work, cap)
            };
            if area(steps, work)? < i128::from(min) {
                return Ok(None);
            }
            let (mut lo, mut hi) = (1i64, steps);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if area(mid, work)? >= i128::from(min) {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            Ok(Some(lo))
        };
    for &a in &anchors {
        for d in dirs {
            let Some(n) = shortest(a, d, None, work)? else {
                continue;
            };
            let (w, r) = stub(a, d, n)?;
            if !spatial.clear(layer, r, net)?
                || !route_rules::local_ok(r, &others, rule.spacing, work, cap)?
            {
                continue;
            }
            return Ok(Some(vec![w]));
        }
    }
    for &a in &anchors {
        for d in [dirs[0], dirs[2]] {
            let back = [-d[0], -d[1]];
            // A longer back stub only covers more, so the first blocked one ends the scan.
            for n1 in 1..steps {
                let (w1, r1) = stub(a, back, n1)?;
                if !spatial.clear(layer, r1, net)? {
                    break;
                }
                let Some(n2) = shortest(a, d, Some(r1), work)? else {
                    continue;
                };
                let (w2, r2) = stub(a, d, n2)?;
                if spatial.clear(layer, r2, net)?
                    && route_rules::local_ok(r1, &others, rule.spacing, work, cap)?
                    && route_rules::local_ok(r2, &others, rule.spacing, work, cap)?
                {
                    return Ok(Some(vec![w1, w2]));
                }
            }
        }
    }
    // Diagnostic only: what blocks the shortest one-sided stubs.
    if std::env::var_os("RUSTROAD_AREA_TRACE").is_some() {
        for &a in anchors.iter().take(4) {
            for d in dirs {
                let reason = match shortest(a, d, None, work)? {
                    None => "no length reaches the area".to_string(),
                    Some(n) => {
                        let (_, r) = stub(a, d, n)?;
                        if !spatial.clear(layer, r, net)? {
                            spatial.stored_collision(layer, r, net)?
                        } else {
                            "same-net notch".to_string()
                        }
                    }
                };
                eprintln!("[area-trace] net {net} layer {layer} anchor {a:?} dir {d:?}: {reason}");
            }
        }
    }
    Ok(None)
}

/// Independent of search history: check geometry against blockages and bounded
/// endpoint-graph connectivity. External OpenDB/KLayout are the physical oracle.
pub fn validate(input: &RouteInput, routes: &Routes, limits: &RouteLimits) -> Result<()> {
    validate_geometry(input, routes, limits, None)
}

/// Validate stored routing under its original telemetry limits, with a separate
/// cap for the comparisons performed by this geometry validation. A previous
/// search may legitimately have used more work than this read-only check.
pub fn validate_with_work_limit(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    max_validation_comparisons: usize,
) -> Result<()> {
    if max_validation_comparisons == 0 {
        return Err(budget(
            "geometry validation comparison limit must be nonzero",
        ));
    }
    validate_geometry(input, routes, limits, Some(max_validation_comparisons))
}

fn validate_geometry(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    validation_work: Option<usize>,
) -> Result<()> {
    preflight(input, limits)?;
    let length = routes.wires.iter().try_fold(0u64, |n, w| {
        n.checked_add(distance(w.start, w.end)?)
            .ok_or_else(|| budget("wire length overflow"))
    })?;
    let nodes = if input.tracks.is_some() {
        crate::troute::graph_nodes(input, limits)?
    } else {
        Lattice::new(input, limits)?.len
    };
    if routes.stats.grid_nodes != nodes
        || routes.stats.expansions > limits.max_expansions
        || routes.stats.comparisons > limits.max_comparisons
        || routes.stats.wire_length_dbu != length
    {
        return Err(invalid("routing telemetry/geometry mismatch"));
    }
    let validation_limits = RouteLimits {
        max_comparisons: limits
            .max_comparisons
            .min(validation_work.unwrap_or(limits.max_comparisons)),
        ..limits.clone()
    };
    let limits = &validation_limits;
    if routes.wires.len() + routes.vias.len() + routes.patches.len() > limits.max_segments
        || routes.access.len() > input.terminals.len()
    {
        return Err(budget("stored routing size"));
    }
    if !input.rules && !routes.patches.is_empty() {
        return Err(invalid("routing patches require LEF-rule mode"));
    }
    if rayon::current_num_threads() > 1 && proven_valid(input, routes, limits, validation_work) {
        return Ok(());
    }
    let mut spatial = Spatial::new(input, limits)?;
    let selected = routed_net_set(input);
    if selected.as_ref() != routes.nets {
        return Err(invalid("stored routed-net coverage"));
    }
    for w in &routes.wires {
        if selected.binary_search(&w.net).is_err()
            || w.layer as usize >= input.layers.len()
            || input.layers[w.layer as usize].vertical.is_none()
        {
            return Err(invalid("stored wire identity"));
        }
        if [w.start[0], w.start[1], w.end[0], w.end[1]]
            .iter()
            .any(|x| x % input.grid != 0)
        {
            return Err(invalid("off-grid routing"));
        }
        let rect = wire_rect(w, input.layers[w.layer as usize].width)?;
        if !spatial.clear(w.layer, rect, w.net)? {
            return Err(invalid("wire collides with blockage/other net"));
        }
        spatial.add_wire(w)?;
    }
    for v in &routes.vias {
        if selected.binary_search(&v.net).is_err()
            || v.kind as usize >= input.vias.len()
            || v.at.iter().any(|x| x % input.grid != 0)
        {
            return Err(invalid("stored via identity/grid"));
        }
        if !spatial.via_clear(v.kind as usize, v.at, v.net)? {
            let kind = &input.vias[v.kind as usize];
            let mut blocked = Vec::new();
            for shape in &kind.shapes {
                let rect = shifted(shape.rect, v.at)?;
                if !spatial.clear(shape.layer, rect, v.net)? {
                    let collision = spatial.stored_collision(shape.layer, rect, v.net)?;
                    blocked.push(format!(
                        "{} [{},{},{},{}] {collision}",
                        input.layers[shape.layer as usize].name, rect.lx, rect.ly, rect.ux, rect.uy
                    ));
                }
            }
            return Err(invalid(&format!(
                "via {} net {} at {:?} collides with blockage/other net on {}",
                kind.name,
                v.net,
                v.at,
                blocked.join(", ")
            )));
        }
        spatial.add_via(v)?;
    }
    for p in &routes.patches {
        if selected.binary_search(&p.net).is_err()
            || p.layer as usize >= input.layers.len()
            || input.layers[p.layer as usize].vertical.is_none()
            || !p.rect.valid()
            || [p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy]
                .iter()
                .any(|x| x % input.grid != 0)
        {
            return Err(invalid("stored patch identity/grid"));
        }
        if !spatial.clear(p.layer, p.rect, p.net)? {
            return Err(invalid("patch collides with blockage/other net"));
        }
        spatial.add(Blockage {
            layer: p.layer,
            rect: p.rect,
            net: Some(p.net),
        })?;
    }
    let mut work = validation_work.map_or(0, |_| spatial.work);
    check_access(input, routes, &selected, limits, &mut work)?;
    let index = crate::route_graph::NetIndex::new(routes, limits, &mut work)?;
    for &net in index.nets() {
        let graph = crate::route_graph::Graph::new(
            input,
            routes,
            index.sources(routes, net),
            limits,
            &mut work,
        )?;
        if !graph.connected(&mut work, limits.max_comparisons)? {
            return Err(invalid("disconnected routed net"));
        }
    }
    drop(index);
    if input.rules {
        if validation_work.is_none() {
            work = 0;
        }
        let violations = route_rules::check(input, routes, limits, &mut work)?;
        if let Some(v) = violations.first() {
            return Err(invalid(&format!(
                "ROUTING_RULE {} ({} total)",
                v.describe(),
                violations.len()
            )));
        }
    }
    Ok(())
}

/// Nets with at least two terminals (the routed set), sorted.
fn routed_net_set(input: &RouteInput) -> Box<[u32]> {
    let mut selected: Vec<u32> = input.terminals.iter().map(|t| t.net).collect();
    selected.sort_unstable();
    let (mut read, mut write) = (0, 0);
    while read < selected.len() {
        let net = selected[read];
        let start = read;
        while read < selected.len() && selected[read] == net {
            read += 1;
        }
        if read - start >= 2 {
            selected[write] = net;
            write += 1;
        }
    }
    selected.truncate(write);
    selected.into_boxed_slice()
}

/// Every access record touches its own pin once; every routed terminal has one.
fn check_access(
    input: &RouteInput,
    routes: &Routes,
    selected: &[u32],
    limits: &RouteLimits,
    work: &mut usize,
) -> Result<()> {
    let terminals = crate::rule_index::order(
        input.terminals.len(),
        |i| input.terminals[i].pin,
        work,
        limits.max_comparisons,
    )?;
    let mut seen = vec![false; input.terminals.len()];
    for a in &routes.access {
        let slot = terminals
            .binary_search_by_key(&a.pin, |&i| input.terminals[i as usize].pin)
            .map_err(|_| invalid("routing access pin absent"))?;
        let i = terminals[slot] as usize;
        let t = &input.terminals[i];
        if selected.binary_search(&a.net).is_err()
            || t.net != a.net
            || seen[i]
            || !t
                .shapes
                .iter()
                .any(|s| s.layer == a.layer && contains(s.rect, a.at))
        {
            return Err(invalid("routing access does not touch its pin"));
        }
        seen[i] = true;
    }
    if input
        .terminals
        .iter()
        .enumerate()
        .any(|(i, t)| selected.binary_search(&t.net).is_ok() && !seen[i])
    {
        return Err(invalid("missing routed terminal"));
    }
    Ok(())
}

/// Parallel proof that the sequential pass of `validate_geometry` accepts
/// `routes`. `false` only means "not proven": the caller then runs the
/// sequential pass, so every verdict and every error (text and choice) is the
/// sequential one. The proof never accepts what the sequential pass rejects:
///
/// * Shapes: each wire, via shape and patch is probed against the complete,
///   frozen index (every fixed and routed shape) rather than only the shapes
///   inserted before it. A superset can only add blockers, so a clear probe
///   here is clear in insertion order. The work counted per probe (bins plus
///   distinct candidates, deduplicated by their first bin in scan order) is
///   an upper bound of the sequential charge, which sees the same bins and a
///   subset of the candidates.
/// * Access coverage, endpoint-graph connectivity and same-net rules are
///   per-net, order-independent checks; their work is counted exactly.
/// * Every budget check of the sequential pass is "running total <= cap", so
///   totals (upper bounds for the probes) within the cap rule out a budget
///   error at any point of the sequential order.
fn proven_valid(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    validation_work: Option<usize>,
) -> bool {
    let selected = routed_net_set(input);
    if selected.as_ref() != routes.nets {
        return false;
    }
    let ((shapes, access), (graphs, rules)) = rayon::join(
        || {
            rayon::join(
                || frozen_probe_work(input, routes, limits, &selected),
                || {
                    let mut work = 0;
                    check_access(input, routes, &selected, limits, &mut work)
                        .ok()
                        .map(|()| work)
                },
            )
        },
        || {
            rayon::join(
                || graph_work(input, routes, limits),
                || {
                    if input.rules {
                        route_rules::clean_parallel(input, routes, limits, 0)
                    } else {
                        Some(0)
                    }
                },
            )
        },
    );
    let (Some(shapes), Some(access), Some(graphs), Some(rules)) = (shapes, access, graphs, rules)
    else {
        return false;
    };
    let cap = limits.max_comparisons;
    let within = |parts: &[usize]| {
        parts
            .iter()
            .try_fold(0usize, |sum, &w| sum.checked_add(w))
            .is_some_and(|total| total <= cap)
    };
    if validation_work.is_some() {
        // One counter runs through probes, access, graphs and rules.
        within(&[shapes, access, graphs, rules])
    } else {
        within(&[shapes]) && within(&[access, graphs]) && within(&[rules])
    }
}

/// Endpoint-graph connectivity of every routed net on the rayon pool; the
/// exact work of `NetIndex::new` plus every net's graph, or `None` when a net
/// is disconnected or a per-net check fails.
fn graph_work(input: &RouteInput, routes: &Routes, limits: &RouteLimits) -> Option<usize> {
    use rayon::prelude::*;
    let mut work = 0;
    let index = crate::route_graph::NetIndex::new(routes, limits, &mut work).ok()?;
    let nets = index.nets();
    let chunk = nets.len().div_ceil(rayon::current_num_threads() * 8).max(1);
    let parts: Option<Vec<usize>> = nets
        .par_chunks(chunk)
        .map(|chunk| {
            let mut total = 0usize;
            for &net in chunk {
                let mut w = 0;
                let graph = crate::route_graph::Graph::new(
                    input,
                    routes,
                    index.sources(routes, net),
                    limits,
                    &mut w,
                )
                .ok()?;
                if !graph.connected(&mut w, limits.max_comparisons).ok()? {
                    return None;
                }
                total = total.checked_add(w)?;
            }
            Some(total)
        })
        .collect();
    parts?
        .into_iter()
        .try_fold(work, |sum, w| sum.checked_add(w))
}

/// One indexed shape of `FrozenIndex`: integer rectangle, owning net
/// (`u32::MAX` for none) and its insertion id (fixed obstacles first, then
/// routed shapes in the sequential insertion order).
#[derive(Clone, Copy)]
struct FrozenShape {
    rect: [i32; 4],
    net: u32,
    id: u32,
}
impl FrozenShape {
    fn rect(&self) -> Rect {
        Rect {
            lx: i64::from(self.rect[0]),
            ly: i64::from(self.rect[1]),
            ux: i64::from(self.rect[2]),
            uy: i64::from(self.rect[3]),
        }
    }
}

/// Immutable per-layer grids (compressed rows) over every fixed obstacle and
/// stored routed shape. Cells are whole multiples of the router's spatial
/// bin, so every shape a bin of a query holds is listed in a scanned cell.
struct FrozenIndex {
    side: i64,
    x0: i64,
    y0: i64,
    nx: i64,
    ny: i64,
    /// Per layer: cell offsets (`nx * ny + 1`, empty for an unused layer)
    /// into that layer's entries.
    layers: Vec<(Vec<u32>, Vec<FrozenShape>)>,
}
impl FrozenIndex {
    fn cells(&self, rect: Rect) -> (i64, i64, i64, i64) {
        (
            (rect.lx.div_euclid(self.side) - self.x0).clamp(0, self.nx - 1),
            (rect.ux.div_euclid(self.side) - self.x0).clamp(0, self.nx - 1),
            (rect.ly.div_euclid(self.side) - self.y0).clamp(0, self.ny - 1),
            (rect.uy.div_euclid(self.side) - self.y0).clamp(0, self.ny - 1),
        )
    }
    fn cell(&self, layer: u32, x: i64, y: i64) -> &[FrozenShape] {
        let (offsets, entries) = &self.layers[layer as usize];
        if offsets.is_empty() {
            return &[];
        }
        let k = (y * self.nx + x) as usize;
        &entries[offsets[k] as usize..offsets[k + 1] as usize]
    }
    /// `None` when a shape does not fit the compact form (the caller then
    /// leaves the decision to the sequential pass).
    fn build(
        input: &RouteInput,
        bin: i64,
        shapes: &[(u32, Rect, u32, u32)],
        routed_base: u32,
    ) -> Option<Self> {
        use rayon::prelude::*;
        let mut extent = input.die;
        for b in &input.obstacles {
            extent.lx = extent.lx.min(b.rect.lx);
            extent.ly = extent.ly.min(b.rect.ly);
            extent.ux = extent.ux.max(b.rect.ux);
            extent.uy = extent.uy.max(b.rect.uy);
        }
        let span = |side: i64| {
            (
                extent.lx.div_euclid(side),
                extent.ux.div_euclid(side) - extent.lx.div_euclid(side) + 1,
                extent.ly.div_euclid(side),
                extent.uy.div_euclid(side) - extent.ly.div_euclid(side) + 1,
            )
        };
        // Cells of a whole number of bins, at most 2^18 per layer.
        let mut side = bin;
        while {
            let (_, nx, _, ny) = span(side);
            nx.saturating_mul(ny) > 1 << 18
        } {
            side = side.checked_add(bin)?;
        }
        let (x0, nx, y0, ny) = span(side);
        let mut index = Self {
            side,
            x0,
            y0,
            nx,
            ny,
            layers: Vec::new(),
        };
        // Shape ids: fixed obstacles first, then the routed shapes.
        let fixed = u32::try_from(input.obstacles.len()).ok()?;
        routed_base.checked_add(u32::try_from(shapes.len()).ok()?)?;
        let shape = |i: u32| -> (u32, Rect, u32) {
            if i < fixed {
                let b = &input.obstacles[i as usize];
                (b.layer, b.rect, b.net.unwrap_or(u32::MAX))
            } else {
                let (layer, rect, net, _) = shapes[(i - fixed) as usize];
                (layer, rect, net)
            }
        };
        let total = fixed.checked_add(u32::try_from(shapes.len()).ok()?)?;
        let mut by_layer: Vec<Vec<u32>> = vec![Vec::new(); input.layers.len()];
        for i in 0..total {
            let (layer, rect, _) = shape(i);
            if layer as usize >= input.layers.len() || !extent.contains(rect) {
                return None;
            }
            by_layer[layer as usize].push(i);
        }
        let cells = usize::try_from(nx.checked_mul(ny)?).ok()?;
        let layers: Option<Vec<(Vec<u32>, Vec<FrozenShape>)>> = by_layer
            .into_par_iter()
            .map(|ids| {
                if ids.is_empty() {
                    return Some((Vec::new(), Vec::new()));
                }
                let mut offsets = vec![0u32; cells + 1];
                for &i in &ids {
                    let (cx0, cx1, cy0, cy1) = index.cells(shape(i).1);
                    for y in cy0..=cy1 {
                        for x in cx0..=cx1 {
                            let k = (y * nx + x) as usize + 1;
                            offsets[k] = offsets[k].checked_add(1)?;
                        }
                    }
                }
                for k in 1..offsets.len() {
                    offsets[k] = offsets[k].checked_add(offsets[k - 1])?;
                }
                let mut cursor = offsets[..cells].to_vec();
                let blank = FrozenShape {
                    rect: [0; 4],
                    net: 0,
                    id: 0,
                };
                let mut entries = vec![blank; offsets[cells] as usize];
                for &i in &ids {
                    let (_, rect, net) = shape(i);
                    let entry = FrozenShape {
                        rect: [
                            i32::try_from(rect.lx).ok()?,
                            i32::try_from(rect.ly).ok()?,
                            i32::try_from(rect.ux).ok()?,
                            i32::try_from(rect.uy).ok()?,
                        ],
                        net,
                        id: if i < fixed {
                            i
                        } else {
                            routed_base + (i - fixed)
                        },
                    };
                    let (cx0, cx1, cy0, cy1) = index.cells(rect);
                    for y in cy0..=cy1 {
                        for x in cx0..=cx1 {
                            let k = (y * nx + x) as usize;
                            entries[cursor[k] as usize] = entry;
                            cursor[k] += 1;
                        }
                    }
                }
                Some((offsets, entries))
            })
            .collect();
        index.layers = layers?;
        Some(index)
    }
}

/// Identity checks, the insertion budgets and frozen-index probes of every
/// stored shape (see `proven_valid`); an upper bound of the sequential
/// spatial work, or `None`.
fn frozen_probe_work(
    input: &RouteInput,
    routes: &Routes,
    limits: &RouteLimits,
    selected: &[u32],
) -> Option<usize> {
    use rayon::prelude::*;
    let on_grid = |values: &[i64]| values.iter().all(|x| x % input.grid == 0);
    let routing = |layer: u32| {
        input
            .layers
            .get(layer as usize)
            .is_some_and(|l| l.vertical.is_some())
    };
    let identity = routes.wires.par_iter().all(|w| {
        selected.binary_search(&w.net).is_ok()
            && routing(w.layer)
            && on_grid(&[w.start[0], w.start[1], w.end[0], w.end[1]])
    }) && routes.vias.par_iter().all(|v| {
        selected.binary_search(&v.net).is_ok()
            && (v.kind as usize) < input.vias.len()
            && on_grid(&v.at)
    }) && routes.patches.iter().all(|p| {
        selected.binary_search(&p.net).is_ok()
            && routing(p.layer)
            && p.rect.valid()
            && on_grid(&[p.rect.lx, p.rect.ly, p.rect.ux, p.rect.uy])
    });
    if !identity {
        return None;
    }
    // The routed shapes in the sequential insertion order, each with the
    // position before which its probe sees shapes: its own, or for a via
    // shape the via's first (all shapes of a via are probed, then added).
    // Spatial::new (its own budget checks; the instance index) runs beside.
    let list = || -> Option<Vec<(u32, Rect, u32, u32)>> {
        let mut first = Vec::new();
        first.try_reserve_exact(routes.vias.len()).ok()?;
        let mut at = u32::try_from(routes.wires.len()).ok()?;
        for v in &routes.vias {
            first.push(at);
            at = at.checked_add(u32::try_from(input.vias[v.kind as usize].shapes.len()).ok()?)?;
        }
        at.checked_add(u32::try_from(routes.patches.len()).ok()?)?;
        let wires = routes.wires.par_iter().enumerate().map(|(i, w)| {
            let rect = wire_rect(w, input.layers[w.layer as usize].width).ok()?;
            Some((w.layer, rect, w.net, i as u32))
        });
        let vias = routes
            .vias
            .par_iter()
            .zip(&first)
            .flat_map_iter(|(v, &first)| {
                input.vias[v.kind as usize].shapes.iter().map(move |shape| {
                    Some((shape.layer, shifted(shape.rect, v.at).ok()?, v.net, first))
                })
            });
        let base = at - u32::try_from(routes.patches.len()).ok()?;
        let patches = routes
            .patches
            .par_iter()
            .enumerate()
            .map(|(i, p)| Some((p.layer, p.rect, p.net, base + i as u32)));
        wires.chain(vias).chain(patches).collect()
    };
    let (shapes, spatial) = rayon::join(list, || Spatial::new(input, limits).ok());
    let (shapes, spatial) = (shapes?, spatial?);
    let bin = spatial.bin;
    let instance_shapes = input.instances.as_ref().map_or(0, InstanceObstacles::len);
    let base = spatial.len();
    let entries = shapes
        .par_iter()
        .map(|(layer, rect, _, _)| {
            if *layer as usize >= input.layers.len() || !rect.valid() {
                return None;
            }
            let n = (i128::from(rect.ux.div_euclid(bin)) - i128::from(rect.lx.div_euclid(bin)) + 1)
                * (i128::from(rect.uy.div_euclid(bin)) - i128::from(rect.ly.div_euclid(bin)) + 1);
            usize::try_from(n).ok()
        })
        .try_reduce(|| 0, |a, b| a.checked_add(b))?
        .checked_add(spatial.entries)?;
    if entries > limits.max_bin_entries
        || entries >= NIL as usize
        || base
            .checked_add(shapes.len())?
            .checked_add(instance_shapes)?
            > limits.max_obstacles
    {
        return None;
    }
    let index = FrozenIndex::build(input, bin, &shapes, u32::try_from(base).ok()?)?;
    let base = base as u32;
    let parts: Option<Vec<usize>> = shapes
        .par_chunks(4096)
        .map_init(
            || InstanceQueryScratch::new(instance_shapes),
            |scratch, slice| {
                let scratch = scratch.as_mut().ok()?;
                slice
                    .iter()
                    .try_fold(0usize, |sum, &(layer, rect, net, before)| {
                        let work = spatial
                            .probe_frozen(&index, scratch, base + before, layer, rect, net)
                            .ok()??;
                        sum.checked_add(work)
                    })
            },
        )
        .collect();
    parts?
        .into_iter()
        .try_fold(spatial.work, |sum, w| sum.checked_add(w))
}

impl Spatial<'_> {
    /// `probe` of a stored shape against the shapes inserted before id
    /// `before` (exactly the sequential candidates), read-only for `proven_valid`:
    /// `None` when blocked, otherwise an upper bound of the work `probe`
    /// charges (its bins, every scanned grid entry, and the instance work,
    /// which uses the caller's private query marks).
    fn probe_frozen(
        &self,
        index: &FrozenIndex,
        scratch: &mut InstanceQueryScratch,
        before: u32,
        layer: u32,
        rect: Rect,
        net: u32,
    ) -> Result<Option<usize>> {
        if !self.input.die.contains(rect) {
            return Ok(None);
        }
        let rule = &self.input.layers[layer as usize];
        let query = expand(rect, rule.spacing)?;
        let bins = (i128::from(query.ux.div_euclid(self.bin))
            - i128::from(query.lx.div_euclid(self.bin))
            + 1)
            * (i128::from(query.uy.div_euclid(self.bin))
                - i128::from(query.ly.div_euclid(self.bin))
                + 1);
        if bins > self.limits.max_bin_entries as i128 {
            return Err(budget("routing query extent"));
        }
        let routing = rule.vertical.is_some();
        let mut work = bins as usize;
        let (cx0, cx1, cy0, cy1) = index.cells(query);
        for y in cy0..=cy1 {
            for x in cx0..=cx1 {
                let cell = index.cell(layer, x, y);
                work += cell.len();
                for e in cell {
                    if e.id >= before {
                        continue;
                    }
                    let other = e.rect();
                    if !overlap(query, other) {
                        continue;
                    }
                    if self.input.rules && routing_separated(rule, rect, other) {
                        continue;
                    }
                    let b = Blockage {
                        layer,
                        rect: other,
                        net: (e.net != u32::MAX).then_some(e.net),
                    };
                    if !(self.same_net(net, layer, rect, &b) && (routing || rect == other)) {
                        return Ok(None);
                    }
                }
            }
        }
        let Some(instances) = &self.instances else {
            return Ok(Some(work));
        };
        let source = instances.source;
        let ties = &self.ties;
        let rules = self.input.rules;
        let limits = self.limits;
        let clear = instances.visit_cells_with_scratch(
            scratch,
            layer,
            query,
            &mut work,
            limits,
            |i, work| {
                source.visit_near(
                    i,
                    layer,
                    query,
                    || {
                        *work = work
                            .checked_add(1)
                            .filter(|&n| n <= limits.max_comparisons)
                            .ok_or_else(|| budget("instance geometry work"))?;
                        Ok(())
                    },
                    |_, b, _| {
                        if !overlap(query, b.rect) {
                            return Ok(true);
                        }
                        if rules && routing_separated(rule, rect, b.rect) {
                            return Ok(true);
                        }
                        Ok(same_net_shapes(ties, net, layer, rect, &b)
                            && (routing || rect == b.rect))
                    },
                )
            },
        )?;
        Ok(clear.then_some(work))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_layer_bins_match_linear_integer_geometry_and_callback_order() {
        use crate::Orientation;
        let shape = |layer, r| TemplateObstacle {
            layer,
            rect: r,
            pin: None,
        };
        let source = InstanceObstacles::new(
            vec![
                ObstacleTemplate::new(
                    20,
                    0,
                    vec![shape(
                        0,
                        Rect {
                            lx: 0,
                            ly: 0,
                            ux: 10,
                            uy: 20,
                        },
                    )],
                )
                .unwrap(),
                ObstacleTemplate::new(
                    40,
                    0,
                    vec![
                        shape(
                            2,
                            Rect {
                                lx: 0,
                                ly: 0,
                                ux: 90,
                                uy: 8,
                            },
                        ),
                        shape(
                            4,
                            Rect {
                                lx: 10,
                                ly: 10,
                                ux: 80,
                                uy: 20,
                            },
                        ),
                        shape(
                            6,
                            Rect {
                                lx: 0,
                                ly: 32,
                                ux: 90,
                                uy: 40,
                            },
                        ),
                        shape(
                            4,
                            Rect {
                                lx: 20,
                                ly: 20,
                                ux: 70,
                                uy: 32,
                            },
                        ),
                    ],
                )
                .unwrap(),
            ],
            (0..12)
                .map(|i| ObstacleInstance {
                    template: (i % 3 == 0) as u32,
                    pin_start: 0,
                    at: [i * 20 - 80, if i % 2 == 0 { -20 } else { 0 }],
                    orientation: if i % 2 == 0 {
                        Orientation::N
                    } else {
                        Orientation::FS
                    },
                })
                .collect(),
            vec![],
        )
        .unwrap();
        let die = Rect {
            lx: -200,
            ly: -100,
            ux: 300,
            uy: 100,
        };
        let limits = RouteLimits::default();
        for bin in [8, 64, 1000] {
            // The extraction index (cached bounds + layer mask) must visit the
            // same instances and charge the same work as the router form.
            let mut cached = InstanceIndex::for_geometry(&source, die, bin, 8, &limits).unwrap();
            let mut plain =
                InstanceIndex::for_geometry_mode(&source, die, bin, 8, &limits, None).unwrap();
            assert!(cached.cache.is_some() && plain.cache.is_none());
            for layer in 0..9 {
                for x in (-100..200).step_by(7) {
                    let query = Rect {
                        lx: x,
                        ly: -10,
                        ux: x + 23,
                        uy: 20,
                    };
                    let gather = |index: &mut InstanceIndex<'_>| {
                        let (mut work, mut hits) = (0, Vec::new());
                        index
                            .visit(layer, query, &mut work, &limits, |b| {
                                hits.push((b.layer, b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy));
                                Ok(true)
                            })
                            .unwrap();
                        (work, hits)
                    };
                    assert_eq!(gather(&mut cached), gather(&mut plain));
                }
            }
            let mut old = InstanceIndex::for_geometry(&source, die, bin, 8, &limits).unwrap();
            let mut new = InstanceIndex::for_geometry_mode(
                &source,
                die,
                bin,
                8,
                &limits,
                Some(InstanceIndexMode::TemplateLayers),
            )
            .unwrap();
            for layer in 0..8 {
                for x in (-100..200).step_by(13) {
                    let query = Rect {
                        lx: x,
                        ly: -10,
                        ux: x + 31,
                        uy: 20,
                    };
                    let gather = |index: &mut InstanceIndex<'_>| {
                        let mut hits = Vec::new();
                        index
                            .visit(layer, query, &mut 0, &limits, |b| {
                                hits.push((b.layer, b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy));
                                Ok(true)
                            })
                            .unwrap();
                        hits
                    };
                    let old_hits = gather(&mut old);
                    let new_hits = gather(&mut new);
                    assert_eq!(
                        old_hits, new_hits,
                        "callback order layer {layer} query {query:?}"
                    );
                    let mut oracle = Vec::new();
                    for i in 0..source.len() {
                        for ordinal in 0..source.shape_count(i).unwrap() {
                            let (b, _) = source.shape(i, ordinal).unwrap();
                            if b.layer == layer && overlap(query, b.rect) {
                                oracle.push((b.layer, b.rect.lx, b.rect.ly, b.rect.ux, b.rect.uy));
                            }
                        }
                    }
                    let mut actual = new_hits;
                    actual.sort();
                    oracle.sort();
                    assert_eq!(
                        actual, oracle,
                        "linear oracle layer {layer} query {query:?}"
                    );
                }
            }
            assert!(new.entries > old.entries);
        }
        let tight = RouteLimits {
            max_bin_entries: 1,
            ..limits.clone()
        };
        assert!(
            InstanceIndex::for_geometry_mode(
                &source,
                die,
                8,
                8,
                &tight,
                Some(InstanceIndexMode::TemplateLayers)
            )
            .is_err()
        );
    }

    #[test]
    fn optional_post_soft_stats_merge_is_checked_and_atomic() {
        let mut aggregate = SingleOwnerRepairStats::default();
        assert!(
            serde_json::to_value(&aggregate)
                .unwrap()
                .get("post_soft_refinement")
                .is_none()
        );
        let partial = SingleOwnerRepairStats {
            attempts: 2,
            trial_expansions: 123,
            post_soft_refinement: Some(PostSoftRefinementStats {
                stages: 1,
                attempts: 2,
                successes: 1,
                ..Default::default()
            }),
            ..Default::default()
        };
        aggregate.merge(&partial).unwrap();
        aggregate.merge(&partial).unwrap();
        assert_eq!(aggregate.attempts, 4);
        assert_eq!(aggregate.trial_expansions, 246);
        assert_eq!(
            aggregate.post_soft_refinement.as_ref().unwrap().successes,
            2
        );
        let before = serde_json::to_value(&aggregate).unwrap();
        let overflow = SingleOwnerRepairStats {
            post_soft_refinement: Some(PostSoftRefinementStats {
                stages: usize::MAX,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(aggregate.merge(&overflow).is_err());
        assert_eq!(serde_json::to_value(&aggregate).unwrap(), before);
    }

    #[test]
    fn validation_work_budget_is_separate_from_historical_search_statistics() {
        let input = fixture();
        let limits = RouteLimits::default();
        let mut routes = route_cpu_ref(&input, &limits).unwrap();
        // Synthetic boundary telemetry for this format/control test. Actual
        // Ibex evidence retains the original4.53B comparison count unchanged.
        routes.stats.comparisons = limits.max_comparisons;
        validate_with_work_limit(&input, &routes, &limits, 1_000_000).unwrap();
        assert_eq!(routes.stats.comparisons, limits.max_comparisons);
        let error = validate_with_work_limit(&input, &routes, &limits, 1).unwrap_err();
        assert!(matches!(error, Error::Budget(_)), "{error}");
        let mut invalid = routes.clone();
        invalid.stats.comparisons += 1;
        let error = validate_with_work_limit(&input, &invalid, &limits, 1_000_000).unwrap_err();
        assert!(error.to_string().contains("telemetry/geometry mismatch"));
    }

    #[test]
    fn per_net_pruning_matches_whole_graph_with_cycles_branches_and_vias() {
        let input = fixture();
        let mut state = 0x5a17u32;
        for case in 0..100 {
            let mut next = || {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                state ^ (state >> 16)
            };
            let mut original = Routes {
                wires: Vec::new(),
                vias: Vec::new(),
                access: Vec::new(),
                patches: Vec::new(),
                nets: (0..8).collect(),
                stats: RouteStats::default(),
            };
            for net in 0..8 {
                for _ in 0..24 {
                    let a = i64::from(next() % 8);
                    let b = i64::from(next() % 8);
                    original.wires.push(Wire {
                        net,
                        layer: next() % 2,
                        start: [a, 0],
                        end: [b, 0],
                    });
                }
                for _ in 0..4 {
                    original.vias.push(Via {
                        net,
                        kind: 0,
                        at: [i64::from(next() % 8), 0],
                    });
                }
                for pin in 0..4 {
                    original.access.push(Access {
                        pin: net * 4 + pin,
                        net,
                        layer: next() % 2,
                        at: [i64::from(next() % 8), 0],
                    });
                }
            }
            original.wires.reverse();
            original.vias.reverse();
            let mut expected = original.clone();
            prune_legacy(&input, &mut expected);
            prune(&input, &mut original, &RouteLimits::default()).unwrap();
            assert_eq!(original.wires, expected.wires, "wire pruning case {case}");
            assert_eq!(original.vias, expected.vias, "via pruning case {case}");
        }
    }

    #[test]
    fn endpoint_graph_rejects_disconnected_access_and_enforces_per_net_budget() {
        let input = fixture();
        let mut out = Routes {
            wires: vec![Wire {
                net: 0,
                layer: 0,
                start: [0, 0],
                end: [10, 0],
            }],
            vias: Vec::new(),
            access: vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 0,
                    at: [10, 0],
                },
            ],
            patches: Vec::new(),
            nets: vec![0],
            stats: RouteStats::default(),
        };
        let limits = RouteLimits::default();
        for connected in [true, false] {
            if !connected {
                out.access[1].at = [20, 0];
            }
            let mut work = 0;
            let index = crate::route_graph::NetIndex::new(&out, &limits, &mut work).unwrap();
            let graph = crate::route_graph::Graph::new(
                &input,
                &out,
                index.sources(&out, 0),
                &limits,
                &mut work,
            )
            .unwrap();
            assert_eq!(
                graph.connected(&mut work, limits.max_comparisons).unwrap(),
                connected
            );
            let tiny = RouteLimits {
                max_active_nodes: 1,
                ..limits.clone()
            };
            assert!(matches!(
                crate::route_graph::Graph::new(
                    &input,
                    &out,
                    index.sources(&out, 0),
                    &tiny,
                    &mut work
                ),
                Err(Error::Budget(_))
            ));
            assert!(graph.connected(&mut 0, 1).is_err());
        }
    }

    #[test]
    #[ignore = "explicit bounded whole-graph versus per-net RSS comparison"]
    fn endpoint_pruning_memory_fixture() {
        let input = fixture();
        let nets = 100_001u32;
        let mut out = Routes {
            wires: Vec::new(),
            vias: Vec::new(),
            access: Vec::new(),
            patches: Vec::new(),
            nets: (0..nets).collect(),
            stats: RouteStats::default(),
        };
        for net in 0..nets {
            for x in [0, 10, 20] {
                out.wires.push(Wire {
                    net,
                    layer: 0,
                    start: [x, 0],
                    end: [x + 10, 0],
                });
            }
            out.access.extend([
                Access {
                    pin: net * 2,
                    net,
                    layer: 0,
                    at: [0, 0],
                },
                Access {
                    pin: net * 2 + 1,
                    net,
                    layer: 0,
                    at: [20, 0],
                },
            ]);
        }
        if std::env::var_os("RUSTROAD_LEGACY_PRUNE").is_some() {
            prune_legacy(&input, &mut out);
        } else {
            prune(
                &input,
                &mut out,
                &RouteLimits {
                    max_segments: 400_004,
                    max_comparisons: 100_000_000,
                    ..RouteLimits::ceiling()
                },
            )
            .unwrap();
        }
        assert_eq!(out.wires.len(), 2 * nets as usize);
        assert!(out.wires.iter().all(|w| w.end[0] <= 20));
        std::hint::black_box(out);
    }

    #[test]
    fn instanced_wall_routes_identically_to_explicit_geometry() {
        let flat = fixture();
        let mut instanced = fixture();
        instanced.obstacles.clear();
        instanced.instances = Some(
            InstanceObstacles::new(
                vec![
                    ObstacleTemplate::new(
                        100,
                        0,
                        vec![TemplateObstacle {
                            layer: 0,
                            rect: Rect {
                                lx: 0,
                                ly: 0,
                                ux: 10,
                                uy: 100,
                            },
                            pin: None,
                        }],
                    )
                    .unwrap(),
                ],
                vec![ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [45, 0],
                    orientation: crate::Orientation::N,
                }],
                Vec::new(),
            )
            .unwrap(),
        );
        let limits = RouteLimits::default();
        let expected = route_cpu_ref(&flat, &limits).unwrap();
        let actual = route_cpu_ref(&instanced, &limits).unwrap();
        assert_eq!(actual.wires, expected.wires);
        assert_eq!(actual.vias, expected.vias);
        assert_eq!(actual.access, expected.access);
        validate(&instanced, &actual, &limits).unwrap();
    }

    #[test]
    fn instanced_flipped_pin_matches_explicit_negative_coordinates() {
        let mut flat = fixture();
        flat.die = Rect {
            lx: -100,
            ly: -100,
            ux: 100,
            uy: 100,
        };
        flat.obstacles = vec![Blockage {
            layer: 0,
            rect: Rect {
                lx: -45,
                ly: -40,
                ux: -35,
                uy: -30,
            },
            net: Some(7),
        }];
        let mut instanced = fixture();
        instanced.die = flat.die;
        instanced.obstacles.clear();
        instanced.instances = Some(
            InstanceObstacles::new(
                vec![
                    ObstacleTemplate::new(
                        30,
                        1,
                        vec![TemplateObstacle {
                            layer: 0,
                            rect: Rect {
                                lx: 0,
                                ly: 10,
                                ux: 10,
                                uy: 20,
                            },
                            pin: Some(0),
                        }],
                    )
                    .unwrap(),
                ],
                vec![ObstacleInstance {
                    template: 0,
                    pin_start: 0,
                    at: [-45, -50],
                    orientation: crate::Orientation::FS,
                }],
                vec![7],
            )
            .unwrap(),
        );
        let limits = RouteLimits::default();
        let mut a = Spatial::new(&flat, &limits).unwrap();
        let mut b = Spatial::new(&instanced, &limits).unwrap();
        for net in [7, 8] {
            for x in (-60..0).step_by(5) {
                for y in (-60..0).step_by(5) {
                    let rect = Rect {
                        lx: x,
                        ly: y,
                        ux: x + 2,
                        uy: y + 2,
                    };
                    assert_eq!(
                        a.probe(0, rect, net).unwrap(),
                        b.probe(0, rect, net).unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn millions_of_virtual_obstacles_keep_compact_storage() {
        let mut input = fixture();
        input.die = Rect {
            lx: 0,
            ly: 0,
            ux: 20_000,
            uy: 200_000,
        };
        input.obstacles.clear();
        let template = ObstacleTemplate::new(
            100,
            0,
            (0..102)
                .map(|_| TemplateObstacle {
                    layer: 0,
                    rect: Rect {
                        lx: 1,
                        ly: 1,
                        ux: 3,
                        uy: 3,
                    },
                    pin: None,
                })
                .collect(),
        )
        .unwrap();
        let instances = (0..100_000)
            .map(|i| ObstacleInstance {
                template: 0,
                pin_start: 0,
                at: [(i % 1000) * 20, (i / 1000) * 100],
                orientation: crate::Orientation::N,
            })
            .collect();
        let provider = InstanceObstacles::new(vec![template], instances, Vec::new()).unwrap();
        assert_eq!(provider.virtual_shapes(), 10_200_000);
        assert!(provider.heap_bytes() < 8 * 1024 * 1024);
        input.instances = Some(provider);
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(
            !spatial
                .clear(
                    0,
                    Rect {
                        lx: 1,
                        ly: 1,
                        ux: 2,
                        uy: 2
                    },
                    5
                )
                .unwrap()
        );
        assert!(spatial.shapes.is_empty());
    }

    #[test]
    fn every_bin_mode_matches_an_independent_ordered_bucket_oracle() {
        let mut reference = BTreeMap::<(u32, i64, i64), Vec<usize>>::new();
        let mut sequence = Vec::new();
        let mut random = 0x25bd_7c91_ae02_1843u64;
        for id in 0..4096usize {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let x = ((random >> 16) % 201) as i64 - 100;
            let y = ((random >> 32) % 201) as i64 - 100;
            let layer = (random & 1) as u32;
            sequence.push((layer, x, y, id));
            reference.entry((layer, x, y)).or_default().push(id);
        }
        for mode in [BinMode::Dense, BinMode::Sparse, BinMode::Paged] {
            let mut grid = BinGrid {
                mode,
                bx0: -64,
                by0: -64,
                nx: 129,
                ny: 129,
                cells: vec![Vec::new(); 2],
                entries: Vec::new(),
                pages: HashMap::with_hasher(BuildFx),
                overflow: HashMap::with_hasher(BuildFx),
            };
            for &(layer, x, y, id) in &sequence {
                grid.push(layer, x, y, id).unwrap();
            }
            for (&(layer, x, y), expected) in &reference {
                assert_eq!(grid.iter(layer, x, y).collect::<Vec<_>>(), *expected);
            }
            assert_eq!(grid.iter(0, 300, -300).count(), 0);
        }
    }

    #[test]
    fn immutable_obstacles_are_shared_and_survive_route_ripup() {
        let input = fixture();
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(spatial.shapes.is_empty());
        assert!(std::ptr::eq(spatial.shape(0), &input.obstacles[0]));
        let fixed = Rect {
            lx: 46,
            ly: 20,
            ux: 54,
            uy: 30,
        };
        assert!(!spatial.clear(0, fixed, 8).unwrap());
        let routed = Rect {
            lx: 10,
            ly: 10,
            ux: 20,
            uy: 20,
        };
        let id = spatial
            .add_routed(Blockage {
                layer: 0,
                rect: routed,
                net: Some(7),
            })
            .unwrap();
        assert_eq!(id, input.obstacles.len());
        assert!(!spatial.clear(0, routed, 8).unwrap());
        assert!(matches!(spatial.probe_soft(0, routed, 8).unwrap(),
            Soft::Clear { victims, .. } if victims == [7]));
        spatial.remove(id);
        assert!(spatial.clear(0, routed, 8).unwrap());
        assert!(!spatial.clear(0, fixed, 8).unwrap());
    }

    #[test]
    fn large_spatial_extent_allocates_only_touched_pages() {
        let mut input = fixture();
        input.die = Rect {
            lx: -100_000,
            ly: -100_000,
            ux: 100_000,
            uy: 100_000,
        };
        input.obstacles = vec![input.obstacles[0].clone(); 1024];
        let limits = RouteLimits::default();
        let mut spatial = Spatial::new(&input, &limits).unwrap();
        assert!(spatial.bins.mode == BinMode::Paged);
        assert_eq!(spatial.bins.pages.len(), 1);
        let fixed = Rect {
            lx: 46,
            ly: 20,
            ux: 54,
            uy: 30,
        };
        assert!(!spatial.clear(0, fixed, 8).unwrap());
        let negative = Rect {
            lx: -100,
            ly: -100,
            ux: -90,
            uy: -90,
        };
        let id = spatial
            .add_routed(Blockage {
                layer: 0,
                rect: negative,
                net: Some(7),
            })
            .unwrap();
        assert_eq!(spatial.bins.pages.len(), 2);
        assert!(!spatial.clear(0, negative, 8).unwrap());
        spatial.remove(id);
        assert!(spatial.clear(0, negative, 8).unwrap());
    }

    fn fixture() -> RouteInput {
        let layers = vec![
            RouteLayer {
                name: "M1".into(),
                width: 2,
                spacing: 2,
                vertical: Some(false),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
            RouteLayer {
                name: "M2".into(),
                width: 2,
                spacing: 2,
                vertical: Some(true),
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
            RouteLayer {
                name: "V1".into(),
                width: 2,
                spacing: 2,
                vertical: None,
                min_area: None,
                min_step: None,
                wide_spacing: None,
            },
        ];
        RouteInput {
            die: Rect {
                lx: 0,
                ly: 0,
                ux: 100,
                uy: 100,
            },
            grid: 1,
            step: 10,
            access_layer: 1,
            layers,
            vias: vec![RouteVia {
                name: "V12".into(),
                lower: 0,
                upper: 1,
                shapes: (0..3)
                    .map(|layer| LayerRect {
                        layer,
                        rect: Rect {
                            lx: -1,
                            ly: -1,
                            ux: 1,
                            uy: 1,
                        },
                    })
                    .collect(),
            }],
            obstacles: vec![Blockage {
                layer: 0,
                rect: Rect {
                    lx: 45,
                    ly: 0,
                    ux: 55,
                    uy: 100,
                },
                net: None,
            }],
            terminals: vec![
                Terminal {
                    pin: 0,
                    net: 0,
                    driver: true,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: Rect {
                            lx: 9,
                            ly: 9,
                            ux: 11,
                            uy: 11,
                        },
                    }],
                    class: None,
                },
                Terminal {
                    pin: 1,
                    net: 0,
                    driver: false,
                    shapes: vec![LayerRect {
                        layer: 0,
                        rect: Rect {
                            lx: 89,
                            ly: 89,
                            ux: 91,
                            uy: 91,
                        },
                    }],
                    class: None,
                },
            ],
            rules: false,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }

    fn rect(lx: i64, ly: i64, ux: i64, uy: i64) -> Rect {
        Rect { lx, ly, ux, uy }
    }
    fn layer(name: &str, vertical: Option<bool>, spacing: i64) -> RouteLayer {
        RouteLayer {
            name: name.into(),
            width: 2,
            spacing,
            vertical,
            min_area: None,
            min_step: None,
            wide_spacing: None,
        }
    }
    fn via(name: &str, m1: Rect) -> RouteVia {
        RouteVia {
            name: name.into(),
            lower: 0,
            upper: 1,
            shapes: vec![
                LayerRect { layer: 0, rect: m1 },
                LayerRect {
                    layer: 2,
                    rect: rect(-1, -1, 1, 1),
                },
                LayerRect {
                    layer: 1,
                    rect: rect(-1, -1, 1, 1),
                },
            ],
        }
    }
    fn terminal(pin: u32, driver: bool, shapes: Vec<Rect>) -> Terminal {
        Terminal {
            pin,
            net: 0,
            driver,
            shapes: shapes
                .into_iter()
                .map(|rect| LayerRect { layer: 0, rect })
                .collect(),
            class: None,
        }
    }
    /// Two M1 pins of one net, no obstacles; M1 spacing 4, cut spacing 0.
    fn rules_input() -> RouteInput {
        RouteInput {
            die: rect(0, 0, 100, 100),
            grid: 1,
            step: 10,
            access_layer: 1,
            layers: vec![
                layer("M1", Some(false), 4),
                layer("M2", Some(true), 4),
                layer("V1", None, 0),
            ],
            vias: vec![via("V12", rect(-1, -1, 1, 1))],
            obstacles: Vec::new(),
            terminals: vec![
                terminal(0, true, vec![rect(9, 9, 11, 11)]),
                terminal(1, false, vec![rect(9, 24, 11, 26)]),
            ],
            rules: true,
            access_classes: Vec::new(),
            tracks: None,
            ties: Vec::new(),
            instances: None,
            held_bytes: 0,
        }
    }
    fn manual(
        input: &RouteInput,
        wires: &[([i64; 2], [i64; 2], u32)],
        vias: &[[i64; 2]],
    ) -> Routes {
        let wires: Vec<_> = wires
            .iter()
            .map(|&(start, end, layer)| Wire {
                net: 0,
                layer,
                start,
                end,
            })
            .collect();
        Routes {
            stats: RouteStats {
                grid_nodes: Lattice::new(input, &RouteLimits::default()).unwrap().len,
                expansions: 0,
                comparisons: 0,
                wire_length_dbu: wires
                    .iter()
                    .map(|w| distance(w.start, w.end).unwrap())
                    .sum(),
                single_owner_repair: None,
            },
            wires,
            vias: vias
                .iter()
                .map(|&at| Via {
                    net: 0,
                    kind: 0,
                    at,
                })
                .collect(),
            access: vec![
                Access {
                    pin: 0,
                    net: 0,
                    layer: 0,
                    at: [10, 10],
                },
                Access {
                    pin: 1,
                    net: 0,
                    layer: 0,
                    at: [10, 25],
                },
            ],
            patches: Vec::new(),
            nets: vec![0],
        }
    }
    #[test]
    fn wide_metal_spacing_needs_width_gap_and_parallel_run() {
        let mut input = rules_input();
        input.layers[0].wide_spacing = Some(WideSpacing {
            width: 3,
            parallel_run: 10,
            spacing: 5,
        });
        let check = |input: &RouteInput, patch: Rect, foreign_x: i64| {
            let mut routes = manual(input, &[([10, 10], [10, 40], 0)], &[]);
            routes.patches.push(Patch {
                net: 0,
                layer: 0,
                rect: patch,
            });
            routes.wires.push(Wire {
                net: 1,
                layer: 0,
                start: [foreign_x, 10],
                end: [foreign_x, 40],
            });
            routes.nets = vec![0, 1];
            route_rules::wide_check(input, &routes, &RouteLimits::default(), &mut 0).unwrap()
        };
        // Wire 9..11 plus patch 11..13 is 4 wide (> 3); net 1 at 17..19 is 4
        // away (>= spacing 4, < wide spacing 5) along 20 > 10.
        let found = check(&input, rect(11, 15, 13, 35), 18);
        assert_eq!(found.len(), 1, "{found:?}");
        match &found[0] {
            Violation::Wide {
                net,
                owner,
                other,
                run,
                wide,
                near,
                ..
            } => {
                assert_eq!((*net, *owner, *other, *run), (0, 0, 1, 20));
                // Only the patch part faces net 1 closer than the wide spacing;
                // `near` bounds net 1's facing wire.
                assert_eq!(*wide, rect(11, 15, 13, 35));
                assert_eq!(*near, rect(17, 9, 19, 41));
            }
            v => panic!("unexpected {}", v.describe()),
        }
        // Distinct polygons of one net have the same foundry spacing rule.
        let mut routes = manual(
            &input,
            &[([10, 10], [10, 40], 0), ([18, 10], [18, 40], 0)],
            &[],
        );
        routes.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(11, 15, 13, 35),
        });
        let same =
            route_rules::wide_check(&input, &routes, &RouteLimits::default(), &mut 0).unwrap();
        assert!(
            matches!(
                same.as_slice(),
                [Violation::Wide {
                    owner: 0,
                    other: 0,
                    run: 20,
                    ..
                }]
            ),
            "{same:?}"
        );
        routes.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(11, 36, 17, 38),
        });
        let joined =
            route_rules::wide_check(&input, &routes, &RouteLimits::default(), &mut 0).unwrap();
        assert!(
            matches!(
                joined.as_slice(),
                [Violation::Wide {
                    owner: 0,
                    other: 0,
                    run: 20,
                    ..
                }]
            ),
            "a remote bridge leaves the long notch: {joined:?}"
        );
        routes.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(13, 20, 17, 22),
        });
        let partial =
            route_rules::wide_check(&input, &routes, &RouteLimits::default(), &mut 0).unwrap();
        assert!(
            partial
                .iter()
                .any(|v| matches!(v, Violation::Wide { run: 13, .. })),
            "only the covered facing interval is removed: {partial:?}"
        );
        routes.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(13, 15, 17, 35),
        });
        let filled =
            route_rules::wide_check(&input, &routes, &RouteLimits::default(), &mut 0).unwrap();
        assert!(
            filled.is_empty(),
            "the full facing gap is filled: {filled:?}"
        );
        // Parallel run 9 is not more than 10; gap 5 is enough.
        assert!(check(&input, rect(11, 15, 13, 24), 18).is_empty());
        assert!(check(&input, rect(11, 15, 13, 35), 19).is_empty());
        // Without the row nothing is checked.
        input.layers[0].wide_spacing = None;
        assert!(check(&input, rect(11, 15, 13, 35), 18).is_empty());
    }

    #[test]
    fn wide_metal_absent_layer_parts_preserve_other_layer_violations() {
        // Two declared rules do not imply wide geometry on both layers.
        // The fixture is <1 MiB: a few integer rectangles, no path enumeration.
        let mut input = rules_input();
        let rule = WideSpacing {
            width: 3,
            parallel_run: 10,
            spacing: 5,
        };
        input.layers[0].wide_spacing = Some(rule);
        input.layers[1].wide_spacing = Some(rule);
        input.terminals.push(Terminal {
            pin: 2,
            net: 2,
            driver: true,
            shapes: vec![LayerRect {
                layer: 1,
                rect: rect(59, 9, 61, 11),
            }],
            class: None,
        });
        let mut routes = manual(
            &input,
            &[([10, 10], [10, 40], 0), ([60, 10], [60, 40], 1)],
            &[[80, 80]],
        );
        routes.wires[1].net = 2;
        routes.wires.extend([
            Wire {
                net: 1,
                layer: 0,
                start: [18, 10],
                end: [18, 40],
            },
            Wire {
                net: 3,
                layer: 1,
                start: [68, 10],
                end: [68, 40],
            },
        ]);
        routes.patches.extend([
            Patch {
                net: 0,
                layer: 0,
                rect: rect(11, 15, 13, 35),
            },
            Patch {
                net: 2,
                layer: 1,
                rect: rect(59, 15, 61, 17),
            },
        ]);
        routes.nets = vec![0, 1, 2, 3];
        let check = |routes: &Routes| {
            route_rules::wide_check(&input, routes, &RouteLimits::default(), &mut 0).unwrap()
        };
        let first = check(&routes);
        assert!(
            matches!(
                first.as_slice(),
                [Violation::Wide {
                    owner: 0,
                    other: 1,
                    layer: 0,
                    run: 20,
                    ..
                }]
            ),
            "narrow M2 shapes must not hide the real M1 violation: {first:?}"
        );
        // Give M2 an actual wide component; its own pair must now be checked.
        routes.patches.push(Patch {
            net: 2,
            layer: 1,
            rect: rect(61, 15, 63, 35),
        });
        let both = check(&routes);
        assert_eq!(both.len(), 2, "both layer violations must remain: {both:?}");
        assert!(
            both.iter().any(|v| matches!(
                v,
                Violation::Wide {
                    owner: 2,
                    other: 3,
                    layer: 1,
                    run: 20,
                    ..
                }
            )),
            "wide M2 must still be checked: {both:?}"
        );
        // Same-net disjoint polygons retain the same spacing obligation.
        routes.wires[3].net = 2;
        routes.nets = vec![0, 1, 2];
        let same = check(&routes);
        assert!(
            same.iter().any(|v| matches!(
                v,
                Violation::Wide {
                    owner: 2,
                    other: 2,
                    layer: 1,
                    run: 20,
                    ..
                }
            )),
            "same-net wide notch must not be skipped: {same:?}"
        );
        let limits = RouteLimits {
            max_comparisons: 1,
            ..RouteLimits::default()
        };
        assert!(
            route_rules::wide_check(&input, &routes, &limits, &mut 0)
                .unwrap_err()
                .to_string()
                .contains("BUDGET_EXCEEDED")
        );
    }

    #[test]
    fn wide_metal_partial_width_tongue_keeps_the_facing_run() {
        let mut input = rules_input();
        input.die = rect(0, 0, 4000, 4000);
        input.step = 420;
        input.terminals.clear();
        input.layers[0].width = 200;
        input.layers[0].spacing = 210;
        input.layers[0].wide_spacing = Some(WideSpacing {
            width: 390,
            parallel_run: 1000,
            spacing: 240,
        });
        let mut original = manual(
            &input,
            &[([100, 100], [100, 1380], 0), ([520, 100], [520, 1380], 0)],
            &[],
        );
        original.access.clear();
        original.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(620, 0, 840, 1480),
        });
        original.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(410, 400, 420, 600),
        });
        // The complete IHP main deck retains M2.e for this 10 nm tongue.
        // Check both orientations; intruding metal must not hide the gap.
        for rotated in [false, true] {
            let mut routes = original.clone();
            if rotated {
                for w in &mut routes.wires {
                    w.start.swap(0, 1);
                    w.end.swap(0, 1);
                }
                for p in &mut routes.patches {
                    p.rect = rect(p.rect.ly, p.rect.lx, p.rect.uy, p.rect.ux);
                }
            }
            let found =
                route_rules::wide_check(&input, &routes, &RouteLimits::default(), &mut 0).unwrap();
            assert!(
                found
                    .iter()
                    .any(|v| matches!(v, Violation::Wide { run: 1480, .. })),
                "rotation {rotated}: {found:?}"
            );
        }
        // Two adjacent bridge rectangles jointly cover the gap at its
        // midpoint. The remaining 640 nm runs are below the 1000 nm rule.
        original.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(200, 640, 310, 840),
        });
        original.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(310, 640, 420, 840),
        });
        let found =
            route_rules::wide_check(&input, &original, &RouteLimits::default(), &mut 0).unwrap();
        assert!(found.is_empty(), "a full bridge splits the run: {found:?}");
    }

    #[test]
    fn cleanup_joins_same_net_wide_polygons_only_when_the_gap_is_clear() {
        let mut input = rules_input();
        input.layers[0].wide_spacing = Some(WideSpacing {
            width: 3,
            parallel_run: 10,
            spacing: 5,
        });
        let endpoints = [[10, 10], [10, 40], [18, 10], [18, 40]];
        input.terminals = endpoints
            .iter()
            .enumerate()
            .map(|(pin, &[x, y])| {
                terminal(pin as u32, pin == 0, vec![rect(x - 1, y - 1, x + 1, y + 1)])
            })
            .collect();
        let mut original = manual(
            &input,
            &[
                ([10, 10], [10, 40], 0),
                ([18, 10], [18, 40], 0),
                ([10, 10], [18, 10], 1),
            ],
            &[[10, 10], [18, 10]],
        );
        original.access = endpoints
            .iter()
            .enumerate()
            .map(|(pin, &at)| Access {
                pin: pin as u32,
                net: 0,
                layer: 0,
                at,
            })
            .collect();
        original.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(11, 15, 13, 35),
        });
        let limits = RouteLimits::default();
        let mut clear = original.clone();
        let (_, remaining) = cleanup_report(&input, &mut clear, &limits).unwrap();
        assert!(remaining.is_empty(), "{remaining:?}");
        assert_eq!(clear.patches.len(), 2, "exactly one legal bridge");
        assert_eq!(clear.patches[1].rect, rect(13, 15, 17, 35));
        validate(&input, &clear, &limits).unwrap();
        let (_, again) = cleanup_report(&input, &mut clear, &limits).unwrap();
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(clear.patches.len(), 2, "cleanup must be idempotent");

        // A foreign shape in the gap must keep the violation unresolved;
        // the cleanup must not make a short to clear a wide-spacing marker.
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(14, 20, 16, 22),
            net: Some(1),
        });
        let (_, blocked) = cleanup_report(&input, &mut original, &limits).unwrap();
        assert!(
            matches!(
                blocked.as_slice(),
                [Violation::Wide {
                    owner: 0,
                    other: 0,
                    ..
                }]
            ),
            "{blocked:?}"
        );
        assert_eq!(original.patches.len(), 1, "foreign gap stays unpatched");

        let mut bounded = limits;
        bounded.max_comparisons = 1;
        assert!(
            cleanup_report(&input, &mut original, &bounded).is_err(),
            "spent work budget cannot pass"
        );
    }

    #[test]
    fn planted_same_net_notch_is_rejected_by_native_validate() {
        let mut input = rules_input();
        // Straight connection plus a dangling hook 2 DBU from it (< spacing 4).
        let routes = manual(
            &input,
            &[
                ([10, 10], [10, 25], 0),
                ([10, 25], [14, 25], 0),
                ([14, 25], [14, 12], 0),
            ],
            &[],
        );
        let err = validate(&input, &routes, &RouteLimits::default()).unwrap_err();
        assert!(
            err.to_string().contains("ROUTING_RULE same-net notch"),
            "{err}"
        );
        // The legacy checker exempts same-net shapes and accepts the defect.
        input.rules = false;
        validate(&input, &routes, &RouteLimits::default()).unwrap();
        // Without the hook the same geometry is clean under the rules.
        input.rules = true;
        let clean = manual(&input, &[([10, 10], [10, 25], 0)], &[]);
        validate(&input, &clean, &RouteLimits::default()).unwrap();
    }
    #[test]
    fn planted_min_area_island_and_floating_patch_are_rejected() {
        let mut input = rules_input();
        input.layers[1].min_area = Some(50);
        // Up and immediately down: the M2 island is 2 x 4 = 8 DBU^2 < 50.
        let routes = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let err = validate(&input, &routes, &RouteLimits::default()).unwrap_err();
        assert!(err.to_string().contains("minimum area"), "{err}");
        input.rules = false;
        validate(&input, &routes, &RouteLimits::default()).unwrap();
        // A patch must touch same-net routed metal.
        input.rules = true;
        let mut floating = manual(&input, &[([10, 10], [10, 25], 0)], &[]);
        floating.patches.push(Patch {
            net: 0,
            layer: 0,
            rect: rect(40, 40, 44, 44),
        });
        let err = validate(&input, &floating, &RouteLimits::default()).unwrap_err();
        assert!(err.to_string().contains("floating patch"), "{err}");
        // Patches are never legal in legacy mode.
        input.rules = false;
        assert!(validate(&input, &floating, &RouteLimits::default()).is_err());
    }
    #[test]
    fn min_area_cleanup_splits_the_stub_when_no_side_alone_has_room() {
        // M2 island 2 x 4 at y 9..13 needs height 25 (area 50). Fixed M2
        // metal leaves 9 DBU below (die edge) and 14 above (spacing 4), and
        // walls on both sides forbid sideways stubs: no single stub fits.
        let mut input = rules_input();
        input.layers[1].min_area = Some(50);
        for r in [
            rect(0, 0, 4, 100),
            rect(16, 0, 20, 100),
            rect(5, 31, 15, 40),
        ] {
            input.obstacles.push(Blockage {
                layer: 1,
                rect: r,
                net: None,
            });
        }
        let limits = RouteLimits::default();
        let mut routes = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let (_, remaining) = cleanup_report(&input, &mut routes, &limits).unwrap();
        assert!(
            remaining.is_empty(),
            "{:?}",
            remaining.iter().map(|v| v.describe()).collect::<Vec<_>>()
        );
        let m2: Vec<_> = routes.wires.iter().filter(|w| w.layer == 1).collect();
        assert_eq!(m2.len(), 3, "{m2:?}");
        routes.stats.wire_length_dbu = routes
            .wires
            .iter()
            .map(|w| distance(w.start, w.end).unwrap())
            .sum();
        validate(&input, &routes, &limits).unwrap();
        // One more DBU of wall above leaves too little room even when split.
        input.obstacles[2].rect.ly = 27;
        let mut routes = manual(
            &input,
            &[([10, 10], [10, 12], 1), ([10, 12], [10, 25], 0)],
            &[[10, 10], [10, 12]],
        );
        let (_, remaining) = cleanup_report(&input, &mut routes, &limits).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn rules_mode_adds_min_area_stub_and_is_deterministic() {
        // A full-height M1 wall forces an M2 hop that is below its area rule.
        let mut input = rules_input();
        input.terminals = vec![
            terminal(0, true, vec![rect(9, 49, 11, 51)]),
            terminal(1, false, vec![rect(89, 49, 91, 51)]),
        ];
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(49, 0, 51, 100),
            net: None,
        });
        input.layers[1].min_area = Some(120);
        let limits = RouteLimits::default();
        let a = route_cpu_ref(&input, &limits).unwrap();
        let b = route_cpu_ref(&input, &limits).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert_eq!(a.patches, b.patches);
        validate(&input, &a, &limits).unwrap();
        let mut work = 0;
        assert!(
            route_rules::check(&input, &a, &limits, &mut work)
                .unwrap()
                .is_empty()
        );
        // Without the area rule the same hop routes with less M2 metal.
        input.layers[1].min_area = None;
        let plain = route_cpu_ref(&input, &limits).unwrap();
        let m2 = |r: &Routes| -> u64 {
            r.wires
                .iter()
                .filter(|w| w.layer == 1)
                .map(|w| distance(w.start, w.end).unwrap())
                .sum()
        };
        assert!(m2(&a) > m2(&plain), "{} <= {}", m2(&a), m2(&plain));
        // Stored routes from the area-free run violate the stricter rule.
        input.layers[1].min_area = Some(120);
        assert!(validate(&input, &plain, &limits).is_err());
    }
    #[test]
    fn routing_corner_spacing_accepts_a_covered_pad_and_rejects_a_closer_corner() {
        // Integer geometry of the EZ130 AOI211X2 C/B access corner. The lower
        // pad is wholly inside C:125^2+175^2 >180^2, although its square
        // spacing envelope overlaps B. Moving B closer reverses that result.
        let mut input = rules_input();
        input.die = rect(0, 0, 2000, 2000);
        input.layers[0].spacing = 180;
        input.vias[0].shapes[0].rect = rect(-105, -145, 105, 145);
        input.vias[0].shapes[1].rect = rect(-100, -145, 100, 145);
        input.vias[0].shapes[2].rect = rect(-95, -95, 95, 95);
        let pin = rect(1275, 1535, 1575, 1840);
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(765, 1155, 1195, 1365),
            net: Some(1),
        });
        let mut class = AccessClass {
            shapes: vec![LayerRect {
                layer: 0,
                rect: pin,
            }],
            blockers: vec![LayerRect {
                layer: 0,
                rect: input.obstacles[0].rect,
            }],
        };
        let slots = HashMap::from([(0, 0), (1, 1)]);
        let pairs = via_pairs(&input);
        let limits = RouteLimits::default();
        let at = [1425, 1685];
        let entries =
            access_table_at(&input, &slots, &pairs, &class, &[(0, at)], &mut 0, &limits).unwrap();
        assert_eq!(
            entries[0].kinds,
            [0],
            "the original pin already contains the entire pad"
        );
        assert!(
            Spatial::new(&input, &limits)
                .unwrap()
                .via_clear(0, at, 0)
                .unwrap()
        );
        input.obstacles[0].rect.ux = 1280;
        class.blockers[0].rect.ux = 1280;
        let entries =
            access_table_at(&input, &slots, &pairs, &class, &[(0, at)], &mut 0, &limits).unwrap();
        assert!(entries[0].kinds.is_empty());
        assert!(
            !Spatial::new(&input, &limits)
                .unwrap()
                .via_clear(0, at, 0)
                .unwrap()
        );
    }

    #[test]
    fn access_table_picks_the_variant_whose_pad_fits_the_pin() {
        let mut input = rules_input();
        input.layers[0].spacing = 3;
        input.layers[1].spacing = 3;
        // Wide pad (6 x 2) versus rotated pad (2 x 6) on a 4-wide pin leg.
        input.vias = vec![
            via("V12", rect(-3, -1, 3, 1)),
            via("V12_R", rect(-1, -3, 1, 3)),
        ];
        let leg = rect(8, 0, 12, 30);
        let bar = rect(8, 30, 30, 34);
        input.access_classes = vec![AccessClass {
            shapes: vec![
                LayerRect {
                    layer: 0,
                    rect: leg,
                },
                LayerRect {
                    layer: 0,
                    rect: bar,
                },
            ],
            blockers: Vec::new(),
        }];
        let mut t0 = terminal(0, true, vec![leg, bar]);
        t0.class = Some((0, [0, 0]));
        input.terminals = vec![t0, terminal(1, false, vec![rect(79, 79, 81, 81)])];
        // An M1 wall keeps the landing via in the final route (no pruning).
        input.obstacles.push(Blockage {
            layer: 0,
            rect: rect(49, 0, 51, 100),
            net: None,
        });
        let limits = RouteLimits::default();
        let grid = Lattice::new(&input, &limits).unwrap();
        let mut work = 0;
        let table =
            access_table(&input, &grid, &input.access_classes[0], &mut work, &limits).unwrap();
        let name = |k: usize| input.vias[k].name.as_str();
        // Leg points land the rotated pad inside the pin; bar points the wide one.
        let on = |r: Rect, e: &AccessEntry| contains(r, e.at) && !contains(bar, e.at);
        assert!(table.iter().any(|e| on(leg, e) && e.fits));
        for e in &table {
            if on(leg, e) && e.fits {
                assert_eq!(name(e.kinds[0]), "V12_R");
            }
            if contains(bar, e.at) && !contains(leg, e.at) && e.fits {
                assert_eq!(name(e.kinds[0]), "V12");
            }
        }
        // Table order puts fitting landings first.
        let first_misfit = table.iter().position(|e| !e.fits).unwrap_or(table.len());
        assert!(table[first_misfit..].iter().all(|e| !e.fits));
        let routes = route_cpu_ref(&input, &limits).unwrap();
        validate(&input, &routes, &limits).unwrap();
        let access = routes.access.iter().find(|a| a.pin == 0).unwrap();
        if let Some(landing) = routes.vias.iter().find(|v| v.at == access.at) {
            assert_eq!(name(landing.kind as usize), "V12_R");
        }
        assert_eq!(
            route_cpu_ref(&input, &limits).unwrap().wires,
            routes.wires,
            "rules-mode routing must be repeatable"
        );
        // A class whose shapes disagree with the terminal is rejected.
        input.terminals[0].class = Some((0, [1, 0]));
        assert!(route_cpu_ref(&input, &limits).is_err());
        // Legacy mode refuses multiple vias for one layer pair.
        input.terminals[0].class = None;
        input.access_classes.clear();
        input.rules = false;
        assert!(route_cpu_ref(&input, &limits).is_err());
    }
    /// The parallel proof never accepts what the sequential pass rejects:
    /// on routed fixtures and on every single-record mutation (moved wire,
    /// wire handed to another net, dropped via, moved via, dropped access,
    /// moved patch) and under tight chained work limits, a four-thread pool
    /// returns exactly the one-thread (sequential) verdict and error text.
    #[test]
    fn parallel_validation_matches_the_sequential_pass_on_mutations() {
        let one = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let four = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let mut area = rules_input();
        area.terminals = vec![
            terminal(0, true, vec![rect(9, 49, 11, 51)]),
            terminal(1, false, vec![rect(89, 49, 91, 51)]),
        ];
        area.obstacles.push(Blockage {
            layer: 0,
            rect: rect(49, 0, 51, 100),
            net: None,
        });
        area.layers[1].min_area = Some(120);
        let mut two_nets = rules_input();
        two_nets.terminals = vec![
            terminal(0, true, vec![rect(9, 9, 11, 11)]),
            terminal(1, false, vec![rect(89, 9, 91, 11)]),
            terminal(2, true, vec![rect(9, 89, 11, 91)]),
            terminal(3, false, vec![rect(89, 89, 91, 91)]),
        ];
        two_nets.terminals[2].net = 1;
        two_nets.terminals[3].net = 1;
        let mut proven = 0;
        for input in [fixture(), rules_input(), area, two_nets] {
            let limits = RouteLimits::default();
            let routes = route_cpu_ref(&input, &limits).unwrap();
            let mut cases = vec![routes.clone()];
            let step = input.grid.max(1);
            for i in 0..routes.wires.len() {
                for d in [[step, 0], [0, step], [-step, 0]] {
                    let mut r = routes.clone();
                    let w = &mut r.wires[i];
                    for p in [&mut w.start, &mut w.end] {
                        p[0] += d[0];
                        p[1] += d[1];
                    }
                    cases.push(r);
                }
                if let Some(&other) = routes.nets.iter().find(|&&n| n != routes.wires[i].net) {
                    let mut r = routes.clone();
                    r.wires[i].net = other;
                    cases.push(r);
                }
            }
            for i in 0..routes.vias.len() {
                let mut r = routes.clone();
                r.vias.remove(i);
                cases.push(r);
                let mut r = routes.clone();
                r.vias[i].at[0] += step;
                cases.push(r);
            }
            for i in 0..routes.access.len() {
                let mut r = routes.clone();
                r.access.remove(i);
                cases.push(r);
            }
            for i in 0..routes.patches.len() {
                let mut r = routes.clone();
                r.patches[i].rect.lx -= step;
                r.patches[i].rect.ux -= step;
                cases.push(r);
            }
            for (k, case) in cases.iter().enumerate() {
                let sequential = one.install(|| validate(&input, case, &limits));
                let parallel = four.install(|| validate(&input, case, &limits));
                assert_eq!(
                    format!("{sequential:?}"),
                    format!("{parallel:?}"),
                    "case {k}"
                );
                let proof = four.install(|| proven_valid(&input, case, &limits, None));
                assert!(sequential.is_ok() || !proof, "case {k}: proven but invalid");
                proven += usize::from(proof);
                for cap in [1, 50, 400, 2_000, 20_000] {
                    let a = one.install(|| validate_with_work_limit(&input, case, &limits, cap));
                    let b = four.install(|| validate_with_work_limit(&input, case, &limits, cap));
                    assert_eq!(format!("{a:?}"), format!("{b:?}"), "case {k} cap {cap}");
                }
            }
        }
        assert!(proven >= 4, "valid fixtures take the parallel proof");
    }
    #[test]
    fn routes_around_blockage_using_vias_and_is_deterministic() {
        let i = fixture();
        let a = route_cpu_ref(&i, &RouteLimits::default()).unwrap();
        let b = route_cpu_ref(&i, &RouteLimits::default()).unwrap();
        assert_eq!(a.wires, b.wires);
        assert_eq!(a.vias, b.vias);
        assert!(a.vias.len() >= 2);
        validate(&i, &a, &RouteLimits::default()).unwrap();
        let mut bad = a.clone();
        bad.vias.clear();
        assert!(validate(&i, &bad, &RouteLimits::default()).is_err());
    }
    #[test]
    fn routing_work_and_memory_caps_fail_explicitly() {
        let i = fixture();
        let l = RouteLimits {
            max_grid_nodes: 1,
            ..Default::default()
        };
        assert!(matches!(route_cpu_ref(&i, &l), Err(Error::Budget(_))));
        let l = RouteLimits {
            max_search_expansions: 1,
            ..Default::default()
        };
        assert!(matches!(route_cpu_ref(&i, &l), Err(Error::Budget(_))));
        let mut i = fixture();
        i.vias.clear();
        assert!(route_cpu_ref(&i, &RouteLimits::default()).is_err());
    }
    #[test]
    fn loaded_footprint_keeps_headroom_and_conservative_fallback() {
        let input = rules_input();
        let fallback = held_estimate(1 << 30, &input);
        assert_eq!(loaded_held_estimate(1 << 30, &input, None), fallback);
        assert_eq!(loaded_held_estimate(1 << 30, &input, Some(0)), fallback);
        assert_eq!(
            loaded_held_estimate(1 << 30, &input, Some(1 << 30)),
            5 << 28
        );
        assert!(loaded_held_estimate(0, &input, Some(1)) >= PROCESS_BASE_BYTES);
        assert_eq!(loaded_held_estimate(0, &input, Some(u64::MAX)), u64::MAX);
    }
}
