//! Bounded one-owner selection before and after ordinary negotiated repair.
//! No owner subsets or per-path owner-set labels are materialized.
use super::{Move, NetRoute, Outcome, Plan, Router};
use crate::Result;
use crate::route_search::State;
use crate::routing::{SingleOwnerRepairConfig, Soft, Terminal, budget, invalid};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
/// Total work spent on the optional fast candidate, across every sink.
const FAST_CANDIDATE_EXPANSIONS: usize = 250_000;
/// Foreign routed owners touched by regular boundary geometry probes. These
/// probes alone do not establish that all path-dependent rules allow the edge.
#[derive(Debug, Serialize)]
pub struct BoundaryOwner {
    pub net: u32,
    pub blocking_candidates: usize,
    pub single_owner_candidates: usize,
}

pub(super) struct Request<'a> {
    pub config: &'a SingleOwnerRepairConfig,
    pub net: u32,
    pub terminals: &'a [&'a Terminal],
    pub plans: &'a HashMap<u32, Plan>,
    pub strict: bool,
    pub rips: &'a BTreeMap<u32, usize>,
    pub round: usize,
    pub failed_pin: u32,
}

/// One net's pre-owner and post-soft trials share this allocation, including
/// any alternate-root retry. Ordinary soft search still uses the global ledger.
pub(super) struct Allocation {
    remaining_expansions: usize,
    progress_trials: usize,
    tried_owners: Vec<u32>,
}
impl Allocation {
    pub(super) fn new(config: &SingleOwnerRepairConfig) -> Self {
        Self {
            remaining_expansions: config.max_total_expansions,
            progress_trials: 0,
            tried_owners: Vec::new(),
        }
    }
    fn consume(&mut self, expansions: usize) -> Result<()> {
        self.remaining_expansions = self
            .remaining_expansions
            .checked_sub(expansions)
            .ok_or_else(|| invalid("single-owner allocation overdrawn"))?;
        Ok(())
    }
}

enum TrialSource {
    ExhaustedFrontier(usize),
    CompletedSoft,
}

fn charge(router: &mut Router<'_>, work: usize) -> Result<()> {
    router.spatial.work = router
        .spatial
        .work
        .checked_add(work)
        .filter(|&n| n <= router.limits.max_comparisons)
        .ok_or_else(|| budget("single-owner repair comparison ledger"))?;
    Ok(())
}

pub(super) fn trial(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
    owner: u32,
    search: usize,
) -> Result<Outcome> {
    if router.allowed_victim.is_some()
        || router.deferred_history.is_some()
        || router.recorded_history.is_some()
        || router.trial_search_limit.is_some()
        || router.trial_expansion_limit.is_some()
    {
        return Err(invalid("nested single-owner trial"));
    }
    let before = router.expansions;
    let end = before.saturating_add(allocation.remaining_expansions);
    router.allowed_victim = Some(owner);
    router.trial_search_limit = Some(search);
    router.trial_expansion_limit = Some(end);
    router.deferred_history = Some(Vec::new());
    let previous_weight = router.heuristic_weight_override;
    if let Some(weight) = request.config.heuristic_weight {
        router.heuristic_weight_override = Some(weight);
        router.single_owner_stats.as_mut().unwrap().heuristic_weight = Some(weight);
    }
    let outcome = router.route_net(
        request.net,
        request.terminals,
        request.plans,
        true,
        request.strict,
        request.rips,
        request.round,
    );
    // Restore even when routing returned Budget/NoRoute. No fallible cleanup
    // or `?` may precede this restoration. Ordinary soft retains its weight.
    router.heuristic_weight_override = previous_weight;
    let deferred = router
        .deferred_history
        .take()
        .ok_or_else(|| invalid("missing deferred history"))?;
    router.allowed_victim = None;
    router.trial_search_limit = None;
    router.trial_expansion_limit = None;
    allocation.consume(router.expansions - before)?;
    router.single_owner_stats.as_mut().unwrap().attempts += 1;
    let outcome = outcome?;
    if let Outcome::Routed(_, victims) = &outcome {
        if victims.iter().any(|&v| v != owner) {
            return Err(invalid("single-owner trial displaced another owner"));
        }
        for &node in &deferred {
            router.history.increment(node)?;
        }
        router
            .single_owner_stats
            .as_mut()
            .unwrap()
            .committed_history_updates += deferred.len();
    }
    if std::env::var_os("RUSTROAD_ROUTE_TRACE").is_some() {
        let (status, failed_pin) = match &outcome {
            Outcome::Routed(_, _) => ("routed", None),
            Outcome::Failed(pin) => ("no_route", Some(*pin)),
            Outcome::Limited(pin) => ("limited", Some(*pin)),
        };
        eprintln!(
            "[troute] single-owner net {} owner {owner} cap {search} {status} failed_pin {failed_pin:?} exp {}",
            request.net,
            router.expansions - before
        );
    }
    Ok(outcome)
}

fn try_owners(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
    owners: &[u32],
    source: TrialSource,
) -> Result<Option<(NetRoute, Vec<u32>)>> {
    let config = request.config;
    let rank = |pin| {
        request
            .terminals
            .iter()
            .position(|t| t.pin == pin)
            .ok_or_else(|| invalid("single-owner failed pin outside net"))
    };
    for &owner in owners {
        if allocation.remaining_expansions == 0 {
            break;
        }
        if router.close_expanded {
            if allocation.tried_owners.contains(&owner) {
                continue;
            }
            allocation.tried_owners.push(owner);
        }
        let first_cap = if router.close_expanded {
            config.first_search_expansions.min(
                router
                    .limits
                    .scouts
                    .as_ref()
                    .map_or(12_000, |scouts| scouts.owner_first_expansions),
            )
        } else {
            config.first_search_expansions
        };
        let first = trial(router, request, allocation, owner, first_cap)?;
        let (failed, limited) = match first {
            Outcome::Routed(route, victims) => return Ok(Some((route, victims))),
            Outcome::Failed(pin) => (pin, false),
            Outcome::Limited(pin) => (pin, true),
        };
        let promote = if router.close_expanded {
            limited
                && match source {
                    TrialSource::ExhaustedFrontier(initial) => rank(failed)? > initial,
                    TrialSource::CompletedSoft => rank(failed)? > 1,
                }
        } else {
            match source {
                TrialSource::ExhaustedFrontier(initial) => rank(failed)? > initial,
                // A complete soft route identifies actual candidate owners even
                // when the first one-owner search runs out before its first sink.
                TrialSource::CompletedSoft => limited || rank(failed)? > 1,
            }
        };
        if promote && allocation.progress_trials < config.max_progress_candidates {
            if allocation.remaining_expansions == 0 {
                break;
            }
            allocation.progress_trials += 1;
            let progress_cap = if router.close_expanded {
                config.progress_search_expansions.min(
                    router
                        .limits
                        .scouts
                        .as_ref()
                        .map_or(24_000, |scouts| scouts.owner_progress_expansions),
                )
            } else {
                config.progress_search_expansions
            };
            if let Outcome::Routed(route, victims) =
                trial(router, request, allocation, owner, progress_cap)?
            {
                return Ok(Some((route, victims)));
            }
        }
    }
    if allocation.remaining_expansions == 0 || router.expansions >= router.limits.max_expansions {
        router
            .single_owner_stats
            .as_mut()
            .unwrap()
            .local_budget_exhaustions += 1;
    }
    Ok(None)
}

fn inner(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
) -> Result<Option<(NetRoute, Vec<u32>)>> {
    let config = request.config;
    let scan = if router.graph.len <= crate::route_search::DENSE_NODES {
        router.graph.len
    } else {
        0
    };
    charge(router, scan)?;
    if router.search.current_count() > config.max_frontier_nodes {
        router.single_owner_stats.as_mut().unwrap().frontier_skips += 1;
        return Ok(None);
    }
    let (states, work) = router.search.capture_bounded(config.max_frontier_nodes)?;
    charge(router, work)?;
    let Some(mut owners) = probe_boundary_owners(router, &states, request.net)? else {
        router.single_owner_stats.as_mut().unwrap().frontier_skips += 1;
        return Ok(None);
    };
    charge(
        router,
        owners
            .len()
            .saturating_mul(owners.len().max(1).ilog2() as usize + 1),
    )?;
    let self_rips = request.rips.get(&request.net).copied().unwrap_or(0);
    owners.sort_unstable_by_key(|o| {
        let o_rips = request.rips.get(&o.net).copied().unwrap_or(0);
        (
            router.close_expanded && o_rips > self_rips.saturating_add(1),
            std::cmp::Reverse(o.single_owner_candidates),
            std::cmp::Reverse(o.blocking_candidates),
            if router.close_expanded { o_rips } else { 0 },
            o.net,
        )
    });
    owners.truncate(config.max_candidates);
    router.single_owner_stats.as_mut().unwrap().candidate_owners += owners.len();
    let initial_rank = request
        .terminals
        .iter()
        .position(|t| t.pin == request.failed_pin)
        .ok_or_else(|| invalid("single-owner failed pin outside net"))?;
    let owners: Vec<_> = owners.into_iter().map(|o| o.net).collect();
    try_owners(
        router,
        request,
        allocation,
        &owners,
        TrialSource::ExhaustedFrontier(initial_rank),
    )
}

fn account(
    router: &mut Router<'_>,
    before: (usize, usize),
    result: &Result<Option<(NetRoute, Vec<u32>)>>,
) {
    let stats = router.single_owner_stats.as_mut().unwrap();
    stats.trial_expansions += router.expansions - before.0;
    stats.trial_comparisons += router.spatial.work - before.1;
    stats.successes += usize::from(result.as_ref().is_ok_and(|r| r.is_some()));
}

/// Returns a one-owner route or explicitly falls back to ordinary soft repair.
/// Failed trials retain consumed work/dead handles but no history.
pub(super) fn run(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
) -> Result<Option<(NetRoute, Vec<u32>)>> {
    request.config.validate(router.limits)?;
    router
        .single_owner_stats
        .get_or_insert_with(Default::default)
        .stages += 1;
    let before = (router.expansions, router.spatial.work);
    let result = inner(router, request, allocation);
    account(router, before, &result);
    result
}

fn refine(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
    owners: &[u32],
) -> Result<Option<(NetRoute, Vec<u32>)>> {
    let stats = router
        .single_owner_stats
        .get_or_insert_with(Default::default);
    stats.stages += 1;
    let post = stats
        .post_soft_refinement
        .get_or_insert_with(Default::default);
    post.stages += 1;
    if allocation.remaining_expansions == 0 {
        post.quota_skips += 1;
        return Ok(None);
    }
    // route_net returns a BTreeSet projection. Validate before using the first
    // bounded slice, rather than materializing or sorting owner combinations.
    charge(router, owners.len())?;
    if owners.windows(2).any(|v| v[0] >= v[1]) {
        return Err(invalid("soft candidate owners are not sorted and unique"));
    }
    let count = owners.len().min(request.config.max_candidates);
    let stats = router.single_owner_stats.as_mut().unwrap();
    stats.candidate_owners += count;
    stats
        .post_soft_refinement
        .as_mut()
        .unwrap()
        .candidate_owners += count;
    let attempts_before = stats.attempts;
    let before = (router.expansions, router.spatial.work);
    let mut sorted_owners: Vec<u32>;
    let candidate_slice = if router.close_expanded {
        sorted_owners = owners.to_vec();
        sorted_owners.sort_unstable_by_key(|&o| (request.rips.get(&o).copied().unwrap_or(0), o));
        &sorted_owners[..count]
    } else {
        &owners[..count]
    };
    let result = try_owners(
        router,
        request,
        allocation,
        candidate_slice,
        TrialSource::CompletedSoft,
    );
    account(router, before, &result);
    let stats = router.single_owner_stats.as_mut().unwrap();
    let post = stats.post_soft_refinement.as_mut().unwrap();
    post.attempts += stats.attempts - attempts_before;
    post.successes += usize::from(result.as_ref().is_ok_and(|r| r.is_some()));
    post.fallbacks += usize::from(result.as_ref().is_ok_and(|r| r.is_none()));
    result
}

/// First try a bounded non-growing candidate. Otherwise preserve ordinary
/// soft's history semantics and exact fallback during owner refinement.
/// Global budget errors abort; they cannot publish a partial route as success.
pub(super) fn soft(
    router: &mut Router<'_>,
    request: &Request<'_>,
    allocation: &mut Allocation,
) -> Result<Outcome> {
    if router.close_expanded
        && router.heuristic_weight_override.is_none()
        && router.detail_weight(true) < 3
        && request.config.post_soft_refinement.is_some()
        && let Some(route) = fast_non_growing(router, request)?
    {
        return Ok(route);
    }
    if request.config.post_soft_refinement.is_none() {
        return router.route_net(
            request.net,
            request.terminals,
            request.plans,
            true,
            request.strict,
            request.rips,
            request.round,
        );
    }
    if router.deferred_history.is_some() || router.recorded_history.is_some() {
        return Err(invalid("nested soft history transaction"));
    }
    let was_zero = router.history.is_zero();
    router.recorded_history = Some(Vec::new());
    let original = router.route_net(
        request.net,
        request.terminals,
        request.plans,
        true,
        request.strict,
        request.rips,
        request.round,
    );
    let updates = router
        .recorded_history
        .take()
        .ok_or_else(|| invalid("missing soft history journal"))?;
    let outcome = original?;
    let Outcome::Routed(route, victims) = outcome else {
        return Ok(outcome);
    };
    if victims.len() <= 1 || (router.close_expanded && victims.len() > 4) {
        return Ok(Outcome::Routed(route, victims));
    }
    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
        eprintln!(
            "[troute] post-soft net {} original_owners {} first {:?} remaining {}",
            request.net,
            victims.len(),
            &victims[..victims.len().min(16)],
            allocation.remaining_expansions
        );
    }
    charge(router, route.shapes.len().saturating_add(updates.len()))?;
    for &id in &route.shapes {
        router.spatial.remove(id);
    }
    router.history.restore_updates(&updates, was_zero)?;
    let result = refine(router, request, allocation, &victims)?;
    if let Some((route, victims)) = result {
        if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
            eprintln!(
                "[troute] post-soft net {} selected {:?}",
                request.net, victims
            );
        }
        return Ok(Outcome::Routed(route, victims));
    }
    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
        eprintln!(
            "[troute] post-soft net {} retained {} original owners",
            request.net,
            victims.len()
        );
    }
    charge(router, route.shapes.len().saturating_add(updates.len()))?;
    for &id in &route.shapes {
        router.spatial.restore(id)?;
    }
    for &(node, _) in &updates {
        router.history.increment(node)?;
    }
    Ok(Outcome::Routed(route, victims))
}

/// Try a bounded greedy route, accepting it only when the entire net cannot
/// grow the pending queue. Rejected attempts leave no geometry or history.
/// All work remains charged; this is not a replacement for quality search.
pub(super) fn fast_non_growing(
    router: &mut Router<'_>,
    request: &Request<'_>,
) -> Result<Option<Outcome>> {
    if router.deferred_history.is_some()
        || router.recorded_history.is_some()
        || router.trial_search_limit.is_some()
        || router.trial_expansion_limit.is_some()
        || router.allowed_victim.is_some()
    {
        return Err(invalid("nested fast routing candidate"));
    }
    let before = router.expansions;
    let was_zero = router.history.is_zero();
    let weight = router.heuristic_weight_override;
    router.heuristic_weight_override = Some(3);
    router.trial_search_limit = Some(
        request
            .config
            .progress_search_expansions
            .min(router.limits.max_search_expansions),
    );
    router.trial_expansion_limit = Some(
        before.saturating_add(
            request.config.max_total_expansions.min(
                router
                    .limits
                    .scouts
                    .as_ref()
                    .map_or(FAST_CANDIDATE_EXPANSIONS, |scouts| scouts.fast_expansions),
            ),
        ),
    );
    router.recorded_history = Some(Vec::new());
    let result = router.route_net(
        request.net,
        request.terminals,
        request.plans,
        true,
        request.strict,
        request.rips,
        request.round,
    );
    // Restore configuration before any fallible cleanup, including Budget errors.
    router.heuristic_weight_override = weight;
    router.trial_search_limit = None;
    router.trial_expansion_limit = None;
    let updates = router
        .recorded_history
        .take()
        .ok_or_else(|| invalid("missing fast history journal"))?;
    let outcome = result?;
    let accepted = matches!(&outcome, Outcome::Routed(_, victims) if victims.len() <= 1);
    if std::env::var("RUSTROAD_ROUTE_TRACE").is_ok_and(|v| v == "2") {
        eprintln!(
            "[troute-fast] {}",
            serde_json::json!({
                "round": request.round, "net": request.net, "accepted": accepted,
                "expansions": router.expansions - before,
                "victims": match &outcome { Outcome::Routed(_, v) => Some(v.len()), _ => None },
            })
        );
    }
    if accepted {
        return Ok(Some(outcome));
    }
    let shapes = match &outcome {
        Outcome::Routed(route, _) => route.shapes.len(),
        _ => 0,
    };
    charge(router, shapes.saturating_add(updates.len()))?;
    if let Outcome::Routed(route, _) = outcome {
        for id in route.shapes {
            router.spatial.remove(id);
        }
    }
    router.history.restore_updates(&updates, was_zero)?;
    Ok(None)
}

pub(super) fn probe_boundary_owners(
    router: &mut Router<'_>,
    nodes: &[(u32, State)],
    net: u32,
) -> Result<Option<Vec<BoundaryOwner>>> {
    let present: std::collections::BTreeSet<_> = nodes.iter().map(|n| n.0).collect();
    let mut owners = BTreeMap::<u32, (usize, usize)>::new();
    let mut neighbors = Vec::with_capacity(8);
    let mut queries = 0usize;
    for &(id, _) in nodes {
        let (node_layer, node_at) = router.graph.point(id);
        router.graph.neighbors(id, &mut neighbors);
        for &(next, movement) in &neighbors {
            let (layer, point) = router.graph.point(next);
            if present.contains(&next)
                || !router.plan.allows_routing_layer(layer)
                || router.gmark[router.cells.of(point) as usize] != router.gepoch
            {
                continue;
            }
            let variants = match movement {
                Move::Via(pair) => router.graph.pairs[pair].kinds.clone(),
                _ => vec![usize::MAX],
            };
            if variants.len() > 64 {
                return Err(budget("boundary probe via variant cap"));
            }
            for kind in variants {
                queries += 1;
                if queries > 4096 * 64 {
                    return Err(budget("boundary probe query cap"));
                }
                let probe = if kind == usize::MAX {
                    let rect = router.edge_rect(node_layer, node_at, point, net)?;
                    router.spatial.probe_soft(node_layer, rect, net)?
                } else {
                    router.spatial.via_probe_soft(kind, node_at, net)?
                };
                if let Soft::Clear { victims, .. } = probe {
                    // A protected partner is blocked for this net, never a trial owner.
                    for &owner in victims.iter().filter(|&&o| router.protected != Some(o)) {
                        if !owners.contains_key(&owner) && owners.len() >= 256 {
                            return Ok(None);
                        }
                        let entry = owners.entry(owner).or_default();
                        entry.0 += 1;
                        entry.1 += usize::from(victims.len() == 1);
                    }
                }
            }
        }
    }
    Ok(Some(
        owners
            .into_iter()
            .map(
                |(net, (blocking_candidates, single_owner_candidates))| BoundaryOwner {
                    net,
                    blocking_candidates,
                    single_owner_candidates,
                },
            )
            .collect(),
    ))
}
