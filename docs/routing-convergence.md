# Bounded routing convergence diagnosis

The repair objective is the actual queue balance: one completed net removes one
entry and each newly queued victim adds one. A faster search is useful only if
the resulting routes reduce this balance without violating geometry or budgets.

This candidate follows the Source15 integration prerequisite and the four-file
Source16 corridor/search/spatial experiment. These prerequisites are separate
from this change. The original module and frozen experiments remain intact.

## Cost and search changes

Each sink of a soft retry now first attempts a route that introduces no new
owner. It may cross owners already committed by earlier sinks. This scout uses
the existing search storage and is capped at the minimum of 10,000, the hard
search allowance and the current per-search allowance. A limited or exhausted
scout falls through to ordinary soft search. This preserves clear sink prefixes
when only a later sink required repair, instead of re-optimizing those clear
sinks into cheaper but destructive crossings. The objective is greedy per sink,
not an unbounded search over whole-net victim combinations.

- In the bounded corridor policy, an owner selected by an earlier sink of the
  same net costs one via to cross again. The spatial obstacle stays present until
  the net is committed; this changes preference, not collision validation.
- The current candidate preserves the existing eighty-via new-owner price
  and rip multiplier. A twenty-via experiment without that multiplier was
  rejected: after 400 real rounds its queue grew from 1,029 to 1,312, versus
  924 remaining with the baseline. Eliminating scout-cap hits alone was not
  evidence of convergence. Local history remains unchanged.
- A single-owner trial charges four vias and honors its configured heuristic
  weight. Previously the corridor path silently replaced that weight with three.
- Both soft stages honor the configured per-search ceiling. The first stage is
  bounded by the lesser of 250,000 and that ceiling, so small configured limits
  return a limited outcome instead of reaching the lower-level fatal guard.
- Router preparation selects the same policy for production and read-only replay.
  Previously production enabled the policy after preparation, leaving replay on
  the old path. Legacy policy selection and small designs remain compatible.

This is still an additive geometric search heuristic. It does not solve an exact
minimum-distinct-owner problem within one sink's path. Wire/via envelopes and
macro moves can charge the same owner differently. No unbounded owner-set labels,
path enumeration, or owner subset search is introduced.

The one-owner trial remains a whole-net trial. A failed trial does not prove the
net needs multiple owners, and a capped hard search does not prove its frontier
is exhausted. A low-cost frontier sampling experiment was rejected: on the real net3862
snapshot it selected four unhelpful owners, increasing expansions from 380,745
to 468,745 with the same victim. Limited frontiers retain the existing skip
behavior; simply turning that skip into speculative whole-net trials is not a
verified fix.

## Quality-gated fast candidate

Before ordinary soft refinement, the bounded corridor policy may try weight3
with the configured progress-search bound and at most min(250,000,
max_total_expansions) expansions for the entire net. It accepts only a complete
route with zero or one distinct victim. A multi-owner or incomplete candidate
has all tentative geometry removed and its history journal restored before the
original quality search. Consumed work is never reset. Explicit replay weights
and configurations without post-soft refinement retain their requested behavior.

This acceptance rule prevents the fast candidate from increasing the immediate
queue length. It does not prove minimum wire length, a zero-victim alternative's
absence, or eventual global convergence. Unnecessary repetition of an identical
weight3 search with an identical cap is removed. Dead spatial staging is included
in owner-stage memory admission; no unbounded owner sets are added.


## Evidence and stop rules

Small geometric regressions reproduce the earlier failure: an avoidable
second victim, a needless victim instead of a short clear detour, a clear prefix spoiled when a later sink is blocked, and incorrect
control flow at a one-expansion soft-search limit. The first also independently
checks that no route segment crosses the additional victim's rectangle.

`RUSTROAD_ROUTE_TRACE=2` now emits `troute-search` and `troute-round` JSON lines.
Search rows report actual weight, stage, cap, expansions and outcome. Round rows
include hard successes, queue before/after, newly queued owners, all victims,
work, wire length and vias. Wire/via totals in this ledger describe attempted
routes, not the final layout's QoR. `scripts/analyze_route_trace.py` checks queue
conservation and search limits while streaming at most 64 MiB and 200,000 rounds.
Its legacy parser explicitly reports missing hard-success/enqueue information.

Start with the same physical revision, same round cap and same CPU allocation.
Reject a candidate if faster searches create worse queue growth. A short early
window does not establish escape from a late plateau. Only extend a surviving
candidate with an explicit additional work/time cap, then use its recorded hard
cases for local replay. Completing routing still requires independent DRC/LVS,
antenna and extracted timing checks before any end-to-end performance claim.

## Resource contract

A matched effective-policy comparison explicitly sets the owner weight to three,
matching what the old binary actually executed despite its configured value one.
The new binary honors either value and reports the real weight.

The initial estimate for the real 26.5M-node implicit graph is approximately
1.9 GiB, using the existing bounded sparse search, heap, history, obstacle and
route tables. Diagnostics use one routing worker, 4 GiB RAM/1 GiB swap, explicit
round and wall-clock caps. Builds use two workers and an 8 GiB cgroup behind the
superproject scale lock. Test execution uses two workers; each test executable
must remain below 2 GiB. GNU time measures the whole command and peak process RSS.
No new library parse or per-node owner-set table is allocated by this change.
The rejected sampler is preserved only in the frozen v4 experiment. The fast
candidate adds conservative admission for max_active_nodes times
(max_via_shapes + 1) times 80 bytes, in addition to the existing history journal
and refinement scratch. Candidate vectors are dropped before quality search.
