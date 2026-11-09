# Constrained routing / extraction verification resource contract

The optional `track_router.single_owner_repair` policy adds a bounded stage
after a hard frontier is exhausted and before ordinary soft repair. None keeps
legacy behavior/encoding. Its explicit limits are at most16 candidate owners,
4096 frontier nodes,50k expansions per first search, at most4 progress searches
of200k per sink, and2M total trial expansions per net. Existing global work,
expansion, heap and memory limits remain authoritative and never reset.

The optional nested `single_owner_repair.heuristic_weight` (1..16) applies
only while a constrained owner trial executes. Ordinary soft repair keeps
`repair_heuristic_weight`; hard search keeps `heuristic_weight`. An explicit
owner weight is more specific than `replay-route --heuristic-weight` inside
that trial. The prior override is restored immediately after routing returns,
including Budget/NoRoute, before any fallible cleanup. Omission preserves the
previous weight, JSON bytes and source fingerprint. This adds no per-node,
per-corner or per-thread heap allocation; explicit owner-weight telemetry is
omitted during legacy operation. It does not change shared trial quotas,
history rollback, geometry, or global budgets.

Boundary owners are sorted by descending single-owner geometry candidates,
descending total blocking candidates, then ascending net ID. A candidate may
displace only that owner. Advancing beyond the original failed terminal permits
one larger attempt immediately, up to the progress count. Selection stops at
the first full one-owner connection. Large frontiers, too many boundary owners,
local allocation exhaustion or no successful candidate fall back to ordinary
soft repair; a global budget error still fails the command explicitly.

Failed candidate geometry uses the existing removal path and its retained
dead records still consume the actual spatial budget. Congestion history is
deferred in a vector capped by `max_active_nodes`, committed only for a kept
route and discarded for failed trials. The memory preflight adds256 bytes per
frontier node,8 bytes per maximum active node (vector growth allowance), plus
64KiB. No path owner sets, subsets, graph clones or parallel scratch copies are
created. Route statistics report actual stage/attempt/success, ledger usage,
fallback and committed-history counters separately.

The optional `post_soft_refinement: "single_owner"` selection also examines a
completed soft candidate with more than one displaced owner. It tries a stable,
bounded prefix of that candidate's sorted owners, one at a time. A search that
reaches its first-search cap may use one of the bounded progress trials even
when it has not reached the first sink. The pre-owner and post-soft stages,
including alternate-root retries, share one `max_total_expansions` allocation
and one `max_progress_candidates` count per repaired net. No owner subsets are
materialized, and all ordinary soft and trial work remains in the original
aggregate ledger.

Ordinary soft history is updated immediately while a bounded journal records
its previous values. Before replacement trials the original geometry is made
inactive and the history updates are reversed. If no replacement completes,
the original live handles and history are restored exactly. Failed trials keep
their work and dead-handle accounting but do not commit congestion history.
Global budget errors abort rather than accepting an incomplete replacement.
The memory preflight includes the retained fallback, shape handles and history
journal alongside the existing search scratch. Omitting the new field retains
the preceding behavior and serialized snapshot fingerprint.

Actual saved-state controls cover an eight-owner candidate reduced to one and
a five-owner candidate reduced to one, with failed partial trials, shared-quota
fallbacks and identical two/sixteen-thread reports. These controls establish
local behavior only; complete routing and independent physical checks are
still required.

`replay-route --single-owner-repair-config POLICY.json` applies that same stage
as an explicit read-only experiment after validating the captured source. It
first reconstructs the hard frontier, so its measured ledger includes that
diagnostic hard call. The JSON is capped4096 bytes. It conflicts with the
individual hard/owner/probe modes and never publishes a workspace revision.

`replay-route --hard-search --max-search-expansions N` is a read-only
no-rip-up diagnostic from the captured state. It validates the original view
and aggregate ledger before applying the explicit hard-search bound (same
50M per-search ceiling). It cannot combine with terminal probes. Reports
distinguish `hard_no_ripup` from `soft_ripup`; neither is a full route verdict.
The option adds one bool and no graph copy. A small all-layer routed barrier
control is soft-routable by displacing its owner but hard-unroutable; the
snapshot is unchanged after both requests.

Adding `--probe-search-frontier` to a hard diagnostic records at most4096
current-epoch discovered states of the final sink, sorted by node ID. Exceeding
that bound rejects the diagnostic. It does not enumerate paths or imply that
a budget-limited frontier is the whole reachable component. Extra scratch is
bounded by1MiB and included in preflight; bounded dense scans and record sorting
are charged to the captured comparison ledger. Reports separate that work.
Hard per-search exhaustion is reported as `budget_exceeded`; an actually empty
frontier remains `no_route`.

The frontier diagnostic also probes regular boundary wire/via geometry and
reports at most256 foreign routed owners (at most64 via variants per node and
262144 queries). It does not claim every such edge satisfies path-dependent
rules. `--allowed-victim NET --max-search-expansions N` runs a separate read-only
counterfactual soft search permitting only that existing foreign owner to be
displaced; fixed geometry, pin escapes and every other routed owner stay hard.
No route is deleted in advance or revision published. The option conflicts with
hard/terminal-probe modes. DefaultNone keeps normal routing unchanged. Campaigns
must separately bound candidate count and cumulative expansions; no owner
subset enumeration is performed.

The explicit diagnostic `limits.replay` may select `phase: "post_failure"`,
`net: null`, `round: null`, an absolute new `path`, and `max_bytes <= 536870912`.
Only the final limited soft call after bounded alternate-root retries captures
that state. It includes actual live/dead occupancy, root order, history and
consumed expansion/work counters. Replaying it starts a **new attempt from the
post-failure state**; it does not reproduce the inputs to the original failed
call. Reports retain `source_phase` and `source_failed_pin`. Default `pre_soft`
continues to require one explicit net; old snapshots without a phase are read
as pre-soft. Capture remains streaming and never overwrites an existing file.
Extra capture RAM is <=48 MiB (bounded 4M sorted history plus selected plans);
spatial geometry and immutable instance templates remain borrowed/shared.

Written before implementing the SDC reader, routing graph or new checker.
Geometry is i64 DBU; timing constraints use finite f64 in explicit ns units.
The first router is deterministic CPU reference code, not GPU code.

The router uses a finite, rectilinear coordinate lattice: manufacturing-aligned
regular coordinates plus bounded pin-access coordinates, <=1024 per axis and
<=8 routing layers. A configurable `max_grid_nodes` (default 2,000,000) is
checked before allocating state. Distance, predecessor and generation arrays
cost <=16 bytes/node (~32 MiB); heap entries <=500,000 at ~32 bytes (~32 MiB).
Only one sink search runs at once. Per-search and total expansion/work limits
stop routing with a structured no-route/budget diagnostic, retaining the old
workspace HEAD. No simple paths, cuts or cones are materialized.

Pin/OBS/supply/previous-route obstacles use a bounded spatial bin index. At
most 200,000 rectangles and 1,000,000 bin references cost <80 MiB including
maps and allocation capacity. Queries have an independent 200,000,000-comparison budget.
Routed wire/via records are capped at 100,000. Immutable source geometry is
shared within a routing transaction; no per-corner/per-worker graph copies.
Conservative extra native state is <256 MiB (one corner, one routing worker).
The small dual-PDK fixture has <150 instances, <200 terminals and an expected
lattice <2,000,000 nodes; expected native RSS <256 MiB. Large designs that cannot
fit this first lattice must fail explicitly, not allocate an unbounded grid.

SDC input is <=1 MiB, <=10,000 bounded statements/tokens with no Tcl execution,
file sourcing, substitutions, loops or eval. Unsupported timing semantics are
explicit diagnostics/errors. Clock configuration, chosen layers/vias, physical
routes and source constraints are revisioned and checked when reloading.

Independent OpenDB/KLayout geometry checks and OpenRCX extraction run as external
executables on standard LEF/DEF/GDS/SPEF inputs. Each process has <=2 GiB physical
memory/no swap where a cgroup is available, <=30 s wall time and bounded output.
OpenROAD gets an 8 GiB virtual-address allowance for its allocator. Parsed SPEF
and clock reports are <=32 MiB, <=200,000 nodes/elements; validation uses bounded
linear graph work, not path enumeration. PDKs/corners run serially. Expected
external RSS is <512 MiB; any test >2 GiB or PDK run >8 GiB is a defect.

Use `free -h` before builds/suites, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`,
`RAYON_NUM_THREADS=2`, `ulimit -v 8388608`, `timeout 180s` and `/usr/bin/time -v`.
Serialize runs estimated above 4 GiB behind `.work/scale.lock`. Measure complete
import/CTS/routing/export/extraction/STA wall time and peak RSS, not only search
time. Pin executable and PDK-rule hashes. Missing tools/models yield unverified.

GF180's technology LEF warns that its parasitic values are unsuitable for
extraction. Use the PDK's dedicated OpenRCX nominal rule file, never promote
LEF placeholder RC to foundry extraction evidence. Both PDKs must retain the
distinction between routed connectivity, subset geometric checks, extracted
timing and complete foundry DRC/LVS. Clock skew acceptance must use explicit
limits and propagated clocks, never ideal-clock equality as a skew oracle.

## LEF-rule routing mode (`lef_rules: true`)

Written before implementing the via-variant, pin-access-table and same-net
cleanup changes. All caps above are unchanged and still cannot be raised.

- Via variants: per policy via at most the single-cut LEF fixed vias on the same
  cut layer (sky130 <=5, gf180 <=4 per layer pair); the config cap of 12 policy
  vias bounds the expanded list to <100 entries. Each search move tries the
  variants in order and stops at the first clear one, so query work grows by at
  most the variant count on via moves only; it is charged to `max_comparisons`.
- Access classes: one per distinct (master, orientation, pin), at most one per
  terminal (<=16,000). Aggregate class rectangles (pin shapes plus same-master
  blockers) share the 200,000-rectangle obstacle cap (~8 MiB). A class table has
  <=128 candidate points, each with the via kinds of one layer pair (<1 MiB per
  class). Tables are built once per class, not per instance.
- Same-net pass: one rebuilt spatial index (same 200,000-shape/1,000,000-bin
  caps as `validate`), shapes grouped per (net, layer) after merging collinear
  wires (<= segments + via pads + patches + terminal rectangles, ~50 B each),
  union-find arrays of the same size, at most eight repair rounds. Pair sweeps,
  coverage subtraction (<=64 pieces), area slabs (O(k^2) per component) and stub
  searches (binary search, <=65,536 steps) are all charged to one counter that
  is capped by `max_comparisons`; patches count against `max_segments`.
- Measured on the dual-PDK pipeline fixture: route step 0.58 s / 15.9 MiB RSS
  (sky130) and 0.57 s / 14.3 MiB (gf180) versus 0.45 s / 14.8 MiB and
  0.41 s / 12.7 MiB before; 59.4M / 33.4M comparisons of the 200M cap.

Unresolvable same-net violations fail the command with `ROUTING_RULE` and HEAD
stays unchanged. The native same-net check is a self-check; foundry decks are
the DRC oracle (see router-drc.md).

## Track-router mode (`track_router`, schema 6)

Written before implementing the track router (stream W14-W16); details in
`docs/scaling.md`. The lattice caps above stay hard caps for the lattice
router. A profile with `track_router` may raise every `RouteLimits` field up
to `RouteLimits::ceiling()` (4,294,967,294 implicit graph nodes, 256M
retained obstacle records, 512M bin entries, 128M segments, 2e9 expansions,
5e7 per search, 2^40 comparisons and 64M heap entries). These are independent
ceilings, not a promise that all maxima fit in memory at once.

`max_active_nodes` separately bounds the raw edges and nodes of one detailed
net (default 200,000; hard ceiling 50 million). It is checked before insertion
and charged at 164 bytes per admitted node/edge, independently of the design's
output segment budget. A large design can permit many completed routes without
reserving the entire output budget for one live search tree.

The detailed-search frontier retains one entry per open node. Updating a
node's cost adjusts that entry in place; stale duplicates cannot consume the
frontier allowance. `max_heap` limits unique pending nodes. Its estimate reserves
128 bytes per possible pending node for priorities, lookup positions and
allocation headroom, separately from the visited search state.

`route` checks a whole-process estimate against `track_router.memory_mib`
(default 2048, at most 65536). Before loading it uses the process baseline plus
3x the revision's serialized size. Once the complete routing input is decoded,
it uses the live resident plus swapped footprint with 25% headroom and a floor
at the structural input estimate. Platforms without that probe retain the
conservative serialized-size estimate. Future allocations are counted
separately: bounded search state, GCells, spatial indexes, retained routes and
the rule-checking workspace. Large implicit grids use sparse search state;
shared master geometry is translated on query instead of being flattened.

Spatial bins use dense grids for small extents and 32x32 pages for populated
large extents. Completed routes retain collinear runs. Access memoization is
capped at 64 MiB, and `max_history_entries` separately bounds congestion history.
Local pin-repair interval queries charge candidate scans to the work budget.
Wire normalization retains line maps and junction sets for one net at a time;
assembly emits normalized runs directly, without a second full raw-wire copy.
Completed pin escapes retain exact-length boxed slices for their wires, vias
and shape handles, rather than the spare capacity of growable vectors.
Pruning and connectivity validation retain four-byte source indices and one
net's endpoint graph, instead of maps of every endpoint in the design. Both
edge and unique-node counts must fit `max_active_nodes`; sort, construction,
leaf-removal and connectivity work are bounded by `max_comparisons`. An
oversized net fails before constructing an unbounded graph. Validation also
uses a compact pin index and an explicitly bounded terminal-presence array.
The work counter covers all nets and the output cap is checked before appending.
Same-net rule checks keep compact source indices and collect one net's shapes
at a time. Each group is bounded by the obstacle cap and sixteen times the
active-node cap. The per-check diagnostic limit defaults to 100,000 and has a
one-million ceiling; minimum-area diagnostic payloads also have a separate
sixteen-shapes-per-diagnostic aggregate bound. Exceeding a bound is an error,
never a clean DRC result. Area repair keeps the original round's geometry while
including newly appended wire/via anchors.
The compressed hole grid has its own `max_rule_grid_cells` bound (one million
cells by default, four million maximum). Multiplication and this limit are
checked before allocating coverage, labels or the flood-fill frontier. An
oversized sparse plane uses an exact sweep of free-space components instead;
its explicitly estimated workspace is capped at 64 bytes per grid-budget unit.
The sweep preserves dense flood-fill component boxes and ordering, and its
coverage, union and output work is charged to the same comparison limit.
While routing, the process' peak RSS is checked at phase boundaries and during
search and access reservation. Every search, queue, repair round and rip-up
count remains bounded. Reaching a limit returns `BUDGET_EXCEEDED` or `NO_ROUTE`
and leaves HEAD unchanged.

`batch_terminals` optionally selects groups of 2 through 1,000,000 terminals,
without splitting a net. There are at most 8,192 groups. Immutable cell and pin
columns are shared between groups. Each group must pass the existing memory
estimate using the live process footprint, including accumulated output and
fixed metal from earlier groups. Output growth is checked before allocation;
expansion and work counters are cumulative. After group state is released,
the complete result is checked against all original terminals and obstacles.
An oversized net, congested group, memory refusal or final check failure leaves
the workspace revision unchanged. The full-flow harness can exercise this mode
with `--route-batch-terminals N` on both PDKs.

Same-net pin-metal queries use a bounded index by layer and x interval; exact
overlap and escape-point exceptions are preserved. Global A* prefers greater
progress when total estimated costs tie and counts pin-access work against its
total budget. A hard detailed-search attempt that reaches its per-search cap
removes its partial metal and proceeds to the existing wider-guide/rip-up
attempts. Soft-search and aggregate resource limits remain terminal failures.
`max_hard_search_expansions` can hand off earlier; it defaults to the full
per-search limit and cannot exceed it. Lowering this optional limit does not
increase soft-search, total-work, frontier or memory allowances.
Soft crossing penalties escalate with the crossed net's rip-up history. Neither
the current net's attempts nor the total unrelated repair count multiplies that
penalty again. Spatial congestion history remains active. The repair counter
still enforces the global work bound, without making the same local conflict
arbitrarily costly as independent nets are added to a batch.
Only unit wire segments that actually cross another routed net gain spatial
history. A crossing on one part of a merged line does not mark its clear or
disconnected portions. Collinear merging retains endpoint IDs and processes
runs in place, without separate endpoint and merged-run copies. Exact conflict
queries use the existing comparison bound and history entry limit.

Preferred-track moves can span 16, 64 or 256 stops. Each full wire is checked
against geometry, and every intermediate point must obey the guide and tree
constraints; congestion history is included along the whole span. Ordinary
single-step moves remain available. Chosen spans are expanded into unit edges
before topology/output limits and final geometry validation.
Routing-tree distance retains exact segments in a bounded forest of balanced
spatial indexes. `max_target_segments` defaults to 8,192 and has a hard ceiling
of one million; admission reserves 192 bytes per target plus 64 KiB for index
construction, relocation and traversal. Inserts, construction and distance
queries charge the shared comparison budget. Traversal uses a fixed stack
and a node-visit cap. Exceeding the target limit returns a budget diagnostic;
it never discards segments or silently substitutes the tree's bounding box.
Outside the tree's bounds, spans must approach its bounding region. Inside,
they must approach an actual segment according to the exact target index;
a zero box distance must not disable every long move. Non-saturated queue
scores provide the existing target distance without another index query.
Empty history and single-root cases avoid unnecessary per-point lookup work; guide
coverage is checked over the complete crossed GCell interval.
Minimum-area checks accept a component as soon as one of its rectangles is
large enough; exact union area is retained for every reported area violation.

Route records are streamed to `routes.rbin` (at most 8 GiB); the JSON envelope
remains capped at 64 MiB. Large schema-7 revisions externalize model and prepared
state into framed, hash-checked `model.rbin` records (16 GiB file bound,
1 MiB per frame) and fixed-width `physical.rbin` (2 GiB). These storage limits
do not certify completion or quality of a full-chip flow. Full OpenC910
RTL-to-GDS qualification remains in progress; see [scaling](scaling.md).


## Preserving pin access across batches

A completed route can cover the only legal upper-layer escape of a later pin,
even when it clears that pin's original lower-layer metal. Batched routing now
returns a typed pin-access failure with its bounded search region and consumed
work. The caller may reopen completed nets overlapping that region, retain the
original cell and supply obstacles, and route those nets with the current batch.
Every original terminal remains in the final full-design validation.

Repair has explicit `RouteLimits` bounds:

| Limit | Default | Hard ceiling |
| --- | ---: | ---: |
| `max_batch_retries` |16 |128 |
| `max_batch_displaced_nets` |1,024 |4,096 |
| `max_batch_repair_terminals` |1,000,000 |1,000,000 |

Zero retries disables recovery. `batch_terminals` still limits the primary
selection; the separate repair-terminal limit caps growth when whole earlier
nets must join it. Every retry recomputes process-memory admission before search.
Victim selection costs at most192bytes per configured displaced net, plus exactly
counted terminal clones and temporary vector reallocation. Completed geometry is
scanned only on a pin-access failure, under the aggregate comparison budget.
Failed searches consume the aggregate expansion and comparison limits too.
No global all-pin escape-plan collection or path enumeration is introduced.

The regression encloses a lower-layer pin in fixed geometry. A prior upper-layer
wire blocks its only escape; joint routing provides a legal witness. Bounded
repair must produce a deterministic, fully connected route, and insufficient
retry, terminal or aggregate search budgets must fail. Real-PDK independent
physical gates remain necessary before full-design qualification.


## Read-only detailed-repair replay

`RouteLimits.replay` optionally writes exactly one pre-soft-search state:
`{"path":"/absolute/state.json","net":376,"round":671,"max_bytes":268435456}`.
It is diagnostic output under the existing revision-bound route transaction;
there is no default capture and existing files are never overwritten. A failed
route still publishes no partial workspace revision. The bounded snapshot retains
original dynamic shape IDs (including dead/rip-up records), alive/routed flags,
work counters, exact guide marks, sorted history, rip-up counts, terminal order
and the selected net's pin escape plans. The immutable source compute view is
fingerprinted, including lazy original instance geometry/PG/pins; terminal order
in that fingerprint is canonical so batching does not change identity.

The source templates remain shared. Streaming uses 64 KiB; extra history scratch
is at most 4M entries * 8 B, and canonical terminal references at most 1M * 8 B.
The encoded file has a 512 MiB hard ceiling, dynamic shapes 4M, selected terminals
100k and history 4M. The independent read-only command checks a conservative
8 * encoded-bytes + 2 * input-heap + 80 * search-state-limit + 128 * heap-limit
against 4 GiB before parsing. The caller must additionally apply an OS memory and
wall-clock guard. No search path set is materialized.

`rustroad --workspace WS replay-route --config ROUTING.json --snapshot state.json`
reconstructs the same bounded spatial index and calls exactly one existing soft
search. Optional `--max-search-expansions N` remains within the unchanged hard
ceiling; `--guide-bounds LX LY UX UY` is an explicit integer-DBU experiment inside
the original die. Technology rules, layers, pin escapes and fixed/dynamic geometry
are not relaxed. The report separates source/used guide bounds and includes work,
expansions, failed pin, victim nets and any candidate wires/vias. `routed` describes
that local soft attempt only: it is not a committed all-net route, DRC, LVS or
whole-design acceptance. Changed views and malformed/oversized state reject.

Batched orchestration subtracts earlier aggregate comparisons/expansions before
calling the router. Replay accepts captured aggregate allowances only when they
are no greater than the supplied outer limits, then uses those exact smaller
values. Every other limit must match, apart from explicit per-search overrides.
A real fresh04 capture deducted2,154,040 comparisons before its first batch; this
is not a reason to widen budgets or modify the captured snapshot.
### Read-only search diagnosis

The optional `track_router.repair_heuristic_weight` selects a separate 1..=16
priority weight only for soft rip-up searches. Hard initial and repair attempts
continue to use `heuristic_weight`. Missing values preserve the original behavior
and are omitted from serialization, preserving legacy replay fingerprints.
An explicit replay `--heuristic-weight` takes precedence over both profile values.
This changes search order alone; geometry, layer ranges and resource ceilings
remain unchanged. Each full run must still complete routing and independent
physical/timing checks before any end-to-end performance claim.

`replay-route --probe-terminals` checks immediate unit-wire and via geometry at
the captured escape tops, with at most 32 terminals and 64 variants per terminal.
It reports fixed blockage versus clear or rip-up-able geometry and whether the
neighbor lies inside the current guide and allowed layer range. It performs no
path search and publishes no revision; clear neighbors do not establish a route.

`replay-route --heuristic-weight 1` explicitly overrides only the search ordering
after the source fingerprint and captured resource ledger have been validated.
Both configured and used weights appear in the report; the accepted range is
1 through 16. The ordinary route command continues to use its TrackPlan value.
Neither option changes PDK geometry, spacing, layer limits or signoff scope.
