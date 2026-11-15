//! Optional two-stage pin access for verified additive diode ECOs.
use super::{Plan, PriorRoutes, Router, Terminal, budget, invalid, trace};
use crate::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};

const MAX_ADDED_PINS: usize = 512;
const MAX_UNCHANGED_NETS: usize = 20_000;
const MAX_PROJECTION_RECORDS: usize = 512_000;
const MAX_PROJECTION_SHAPES: usize = 1_200_000;
// Include temporary slot handles, owned shapes, vector capacity and indexing
// slack. The spatial index still enforces its ordinary shape and bin caps.
const PROJECTION_BYTES_PER_SHAPE: u64 = 112;

pub(super) fn charge(router: &mut Router<'_>, work: usize) -> Result<()> {
    router.spatial.work = router
        .spatial
        .work
        .checked_add(work)
        .filter(|&used| used <= router.limits.max_comparisons)
        .ok_or_else(|| budget("prior escape preservation comparison limit"))?;
    Ok(())
}

pub(super) fn reserve<'a>(
    router: &mut Router<'a>,
    groups: &BTreeMap<u32, Vec<&'a Terminal>>,
    terminals: &[&'a Terminal],
    prior: &PriorRoutes,
    base_bytes: u64,
) -> Result<HashMap<u32, Plan>> {
    let started = std::time::Instant::now();
    if prior.preserve_escape_pins.len() > MAX_ADDED_PINS {
        return Err(budget("prior escape preservation added-pin limit"));
    }
    let (added, stable): (Vec<_>, Vec<_>) = terminals
        .iter()
        .copied()
        .partition(|t| prior.preserve_escape_pins.contains(&t.pin));
    if added.len() != prior.preserve_escape_pins.len() || added.iter().any(|t| t.driver) {
        return Err(invalid(
            "prior escape preservation needs verified added input terminals",
        ));
    }
    let affected: BTreeSet<_> = added.iter().map(|t| t.net).collect();
    if affected.len() != added.len() || affected.iter().any(|net| !prior.nets.contains_key(net)) {
        return Err(invalid(
            "prior escape preservation needs one verified addition per offered net",
        ));
    }
    charge(router, terminals.len().saturating_add(prior.nets.len()))?;
    let mut unchanged = Vec::new();
    for &net in prior
        .nets
        .keys()
        .filter(|net| groups.contains_key(net) && !affected.contains(net))
    {
        if unchanged.len() == MAX_UNCHANGED_NETS {
            return Err(budget("prior escape preservation unchanged-net limit"));
        }
        unchanged.push(net);
    }
    let mut records = 0usize;
    let mut shapes = 0usize;
    for net in &unchanged {
        let (wires, vias) = &prior.nets[net];
        records = records
            .saturating_add(wires.len())
            .saturating_add(vias.len());
        if records > MAX_PROJECTION_RECORDS.min(router.limits.max_segments) {
            return Err(budget("prior escape preservation projected records"));
        }
        shapes = shapes.saturating_add(wires.len());
        for via in vias {
            let template = router
                .input
                .vias
                .get(via.kind as usize)
                .ok_or_else(|| invalid("prior escape preservation via kind"))?;
            shapes = shapes.saturating_add(template.shapes.len());
        }
        if shapes > MAX_PROJECTION_SHAPES {
            return Err(budget("prior escape preservation projected shapes"));
        }
    }
    charge(router, records)?;
    let extra_bytes = (shapes as u64)
        .saturating_mul(PROJECTION_BYTES_PER_SHAPE)
        .saturating_add((terminals.len() as u64).saturating_mul(16));
    if base_bytes.saturating_add(extra_bytes) > router.plan.memory_mib.saturating_mul(1 << 20) {
        return Err(budget("prior escape preservation memory estimate"));
    }

    let mut plans = router.reserve(&stable)?;
    router.guard("prior escape preservation initial access")?;
    let retained_records = plans.values().fold(0usize, |count, plan| {
        count
            .saturating_add(plan.wires.len())
            .saturating_add(plan.vias.len())
    });
    // Determine which previously unchanged nets can still be imported with
    // only the original terminals reserved.
    let stable_routes = project(router, groups, &plans, prior, &unchanged, shapes)?;
    let stable_admits = stable_routes.nets;
    retire(router, stable_routes.slots)?;

    plans.extend(router.reserve_after(&added, retained_records)?);
    let first_attempt = project(router, groups, &plans, prior, &stable_admits, shapes)?;
    let stable_count = stable_admits.len();
    let unprotected_count = first_attempt.nets.len();
    let checks_per_net = unprotected_count.checked_ilog2().unwrap_or(0) as usize + 1;
    charge(router, stable_count.saturating_mul(checks_per_net))?;
    let lost: Vec<_> = stable_admits
        .into_iter()
        .filter(|net| first_attempt.nets.binary_search(net).is_err())
        .collect();
    let mut protected_shapes = 0;
    if lost.is_empty() {
        retire(router, first_attempt.slots)?;
    } else {
        // Remove only the fresh pins' first attempt. Their old peers stay
        // fixed, even if a later pin requests access repair.
        for terminal in &added {
            let plan = plans
                .remove(&terminal.pin)
                .ok_or_else(|| invalid("prior escape preservation missing fresh plan"))?;
            charge(router, plan.shapes.len())?;
            for slot in plan.shapes {
                router.spatial.remove(slot);
            }
        }
        retire(router, first_attempt.slots)?;
        let protected = project(router, groups, &plans, prior, &lost, shapes)?;
        if protected.nets != lost {
            return Err(invalid(
                "prior escape preservation changed a stable route admission",
            ));
        }
        protected_shapes = protected.slots.len();
        plans.extend(router.reserve_after(&added, retained_records)?);
        retire(router, protected.slots)?;
    }
    router.guard("prior escape preservation cleanup")?;
    trace(
        "prior escape preservation",
        started,
        format_args!(
            "staged {} added pins, {stable_count} prior nets before new pins, {unprotected_count} after unprotected access, protected {} prior nets {protected_shapes} shapes",
            added.len(),
            lost.len(),
        ),
    );
    Ok(plans)
}

struct Projection {
    nets: Vec<u32>,
    slots: Vec<usize>,
}

fn project(
    router: &mut Router<'_>,
    groups: &BTreeMap<u32, Vec<&Terminal>>,
    plans: &HashMap<u32, Plan>,
    prior: &PriorRoutes,
    selected: &[u32],
    max_shapes: usize,
) -> Result<Projection> {
    if selected.is_empty() {
        return Ok(Projection {
            nets: Vec::new(),
            slots: Vec::new(),
        });
    }
    charge(router, plans.len().saturating_add(selected.len()))?;
    // Match the normal prior import, which sees these search envelopes
    // unless the routing plan drops them before detailed routing.
    let portals: Vec<_> = if router.plan.qor.as_ref().is_some_and(|q| q.drop_portals) {
        plans
            .values()
            .flat_map(|plan| {
                plan.shapes
                    .iter()
                    .rev()
                    .take(usize::from(plan.portal_shapes))
                    .copied()
            })
            .collect()
    } else {
        Vec::new()
    };
    charge(router, portals.len().saturating_mul(2))?;
    for &slot in &portals {
        router.spatial.remove(slot);
    }
    let mut out = Projection {
        nets: Vec::with_capacity(selected.len()),
        slots: Vec::new(),
    };
    let report = router.prior_rejects.take();
    for &net in selected {
        if let Some(route) = router.import_prior(net, &groups[&net], plans, prior)? {
            out.nets.push(net);
            if out.slots.len().saturating_add(route.shapes.len()) > max_shapes {
                return Err(budget("prior escape preservation projected shape slots"));
            }
            out.slots.extend(route.shapes);
        }
    }
    router.prior_rejects = report;
    for slot in portals {
        router.spatial.restore(slot)?;
    }
    router.guard("prior escape preservation temporary routes")?;
    Ok(out)
}

fn retire(router: &mut Router<'_>, slots: Vec<usize>) -> Result<()> {
    charge(router, slots.len())?;
    for slot in slots {
        router.spatial.remove(slot);
    }
    // Reclaim removed bins and let later searches reuse retired slots. Live
    // escape and portal handles keep their original IDs.
    let (_, entries, allocated_shapes, _) = router.spatial.footprint();
    charge(router, entries.saturating_add(allocated_shapes))?;
    router.spatial.compact_dead()
}
