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

### Detailed-route quality controls (`track_router.qor`)

All optional; omitted (or all defaults) routes exactly as before and leaves
the serialized plan unchanged: `default_quality_controls_reproduce_the_legacy_route`
compares `qor` omitted with all defaults, and on Ibex the dense-placement
replay of the legacy Metal3-access configuration (no `qor`) writes a
`routes.rbin` byte-identical to the one of the RustROAD `baff0ba` merged head.

| Key | Effect |
| --- | --- |
| `prim_order` | The initial detailed route joins a net's terminals in Prim order (Prim 1957): the driver first, then repeatedly the terminal whose escape top is nearest (L1) to an already joined one, ties by pin id. O(k^2) work per net, charged. Repair keeps its centre-out order. |
| `release_portals` | Once a net is routed, its pin escapes' upward-via envelopes (search geometry only) are removed, once per terminal. |
| `drop_portals` | Every envelope is removed after pin access and global routing, before the first detailed search. Each envelope reserved a Via2 landing (Metal2 and Metal3 pads) above every pin for the whole detailed phase and walled other nets out of the lower layers. |
| `net_aware_access` | Pin access of nets with at most 8 terminals picks, among legal escape tops within 840 DBU of cost of the cheapest, the one nearest (twice the L1 distance) to the net's other terminals: their escape tops once reserved, else their pin boxes. Two pins that can share a Metal2 track land on it, and the net needs no Metal3 jog. A terminal whose escape search failed, and every escape it displaced, take the cheapest top from then on; this removes the observed re-block livelock, but the other terminals keep their hinted tops, so the repair does not retrace the legacy one. If the hinted pass still leaves a pin without a legal escape (or hits the repair-round cap), pin access is redone from the same start state without hints, which gives exactly the legacy escapes: the control never makes pin access fail where the legacy access succeeds. |
| `small_net_terminals`, `small_net_weight` | Hard detailed searches of nets with at most this many terminals use this weight (1 = admissible A*) instead of `heuristic_weight`; soft rip-up searches keep `repair_heuristic_weight`. |
| `layer_cost_milli`, `via_cost_milli` | Per routing layer, wire-step multiplier and via multiplier (by the via's upper layer), thousandths, 1000..=64000. |
| `bend_cost` | Surcharge of a same-layer wire step that turns from the axis of the step that reached the node. |
| `notch_bridge` | Same-net notch repair for facing shapes: cleanup's bridges try the stair bridge and bars clipped to two widths past the gap before the legacy full-length bars, and a notch that repeats after a re-route with every fill blocked gets a reserved fill (repair rung 5 below). Off keeps the legacy bars and repair. Needed with `access_layer` Metal2 on Ibex. |
| `strict_wide_cycles` | The bounded detail-cycle cut of rule repair (rung 3 below) also runs for an owner whose strict re-route succeeded, before the halos, from the second sighting of a wide pair, at most three cuts per wide region (`MAX_REGION_CUTS`), and several cuts can happen in one pass. Off keeps the cut for relaxed owners only, once per net, after both halos. |
| `via_reduction` | Post-route via reduction (section below). Omitted keeps the routes of the repair loop. |

#### Post-route via reduction (`track_router.qor.via_reduction`)

Global routing here is two-dimensional and the detailed search picks layers
step by step, so a net routed early in a congested region often climbs to
Metal4 and above, and keeps those layer changes after the space it avoided
has been freed or used by others. In the final DEFs of the Ibex phys
configuration and of LibreLane (receipt code), the native routing had 8.5 k
more vias (130,729 against 122,194) although it used 2.3 k fewer Via1 and
1.7 k fewer Via2: Via3 and above carried 12.5 k more (Via3 +9.9 k, Via4
+2.2 k, TopVia1 and TopVia2 +0.4 k), mostly in multi-terminal nets (25.2 vias
per 6-10-terminal net against 20.4; the two netlists differ, so the classes
do not hold the same nets). Re-assigning a net's layers against fixed
neighbours for via count is the classic layer-assignment-for-via-minimization
step after global routing (constrained via minimization; congestion-constrained layer
assignment, T.-H. Lee and T.-C. Wang, IEEE TCAD 27(9):1643-1656, 2008;
negotiation-based layer assignment for via count, W.-H. Liu and Y.-L. Li,
ASP-DAC 2011, which re-assigns one net at a time against the others). Here
it is re-expressed as a single-net re-route on the detailed track graph,
which also lets a net straighten its topology:

1. Once negotiated repair has routed every net (first rule pass only), the
   candidates are the nets with at most `max_terminals` terminals and at
   least `min_vias` routed vias (escape vias of the pin access plans are
   never counted), in order of decreasing routed via count, then net id.
   In an ECO route (a prior workspace, as post-route repair uses), a net
   whose route still equals its prior record is no candidate: the nets the
   ECO keeps stay as they were, and only the nets it routed are re-assigned.
   An identity ECO of the Ibex route below (the routed workspace as its own
   prior) skips all 14,143 nets with at most 10 terminals and writes the
   same routes as the ECO without the pass (23 s CPU); before, the pass
   tried 8,606 of them and changed 489 (37 s CPU).
2. Each candidate in turn is ripped up and searched again as a hard
   (non-rip-up) search in its global guide grown by `margin` GCells (default:
   the plan's `expand_margin`), with every other net's metal fixed, the via
   steps priced at `via_milli` thousandths of their normal cost and
   wrong-way steps at `wrong_way_factor` (0 keeps the plan's), with search
   weight `weight` (default 1) and at most `net_expansions` expansions for
   the net.
3. The new route is kept only when it has fewer routed vias and its wire
   length grows by at most `length_per_via` DBU per via removed. Otherwise,
   or when the search fails or hits its cap, the old route's index shapes
   are restored unchanged (no index compaction runs in between, so every
   removed shape id is still its own); the order and every decision are
   functions of the routed state alone (deterministic).
4. `passes` repeats this over the new state (a pass that keeps nothing
   ends it); cleanup and the rule-repair passes then run as usual on the
   result, so every route still passes the same checks.

The search lower bound is rebuilt from the same prices before the first
re-route and restored afterwards (`routing_target::CostModel`), so with
weight 1 the bound stays admissible and consistent for the pass's step
costs. Every new route is a legal hard-search result: no other net is
displaced. The pass charges the comparison budget for its ordering, the
ECO prior comparison and rip-ups, and reports per terminal-count class the
nets tried, kept, vias removed and search expansions in the `via reduction`
trace line, then a total line (tried, kept, capped, failed, vias removed and
ECO nets kept from the prior).

On the Ibex phys-config placement (route replays of the flow's `be_route`
input, routed vias and wire from the exported DEF with the receipt code), the
cost of the pass is dominated by long connections: with weight 1, via steps
at three times their price and no filter it removed 11.4 k vias for 42.5 M
expansions, but nets with at most 20 µm of routed wire per connection gave
64 % of that gain for 4 % of the expansions, and the median net needed 49
expansions against 87 k at the 99th percentile.

The recipe (`via_cost` 2520 and `wrong_way_factor` 3 instead of 1680 and 4,
`strict_wide_cycles`, and the pass with `via_milli` 2000, `max_terminals` 10,
`max_connection_length` 20000, `net_expansions` 10000 and `length_per_via`
1680) takes the route from 130,527 vias and 892,781.5 µm to 118,424 vias and
897,717.5 µm at `be_route`. Most of that comes from the prices, not from the
pass:

| `be_route`, Ibex phys-config placement | vias | Via2 / Via3 / Via4 | wire µm | wrong-way µm (Metal2) |
| --- | ---: | ---: | ---: | ---: |
| previous prices (1680, 4) | 130,527 | 57,390 / 16,271 / 3,231 | 892,781.5 | 10,271.7 (7,527.9) |
| new prices and `strict_wide_cycles` | 121,603 | 51,711 / 13,714 / 2,677 | 896,868.4 | 22,934.1 (16,293.9) |
| plus `via_reduction` (the recipe) | 118,424 | 49,188 / 13,082 / 2,653 | 897,717.5 | 26,213.8 (19,625.6) |

So the prices remove 8.9 k vias and the pass 3.2 k more (its trace: 1,600
nets kept of 9,060 tried, 25 capped, 1 failed). Both trade vias for
wrong-way wire: wrong-way length grows about 2.5 times, to 26.3 k µm in the final
DEF against LibreLane's 17.3 k µm (Metal2 19.6 k against 14.7 k), and the
same-net patches (RECT) grow from 1,606 to 2,869 (LibreLane's final DEF has
30,477). The acceptance test of the pass counts length, not direction. The
route time goes the same way: in interleaved replays negotiated repair ends
about 48 s later with the new prices, the pass takes about 10 s, and the
rule passes about 10 s less (the A/B replays of the flow's `be_route` input:
231 to 241 s wall before, 279 to 294 s after).

Against LibreLane the result is parity per connection, not a lead. In the
final DEFs the native routing has 118,610 vias against 122,194, but 39,608
connections (pins minus one per net) against 40,405: 2.995 vias per
connection against 3.024, 1.650 against 1.650 without Via1 (pin access),
and 2.205 per terminal against 2.197. The raw difference comes from the
smaller netlist and 2.3 k fewer Via1; Via3 and Via4 remain 6.7 k and 1.6 k
above LibreLane's.

The wrong-way jogs cost the shared OpenRCX extraction gate, whose work
follows the DEF's wire pieces (`docs/extraction.md`, "Oracles";
`scripts/def_pieces.py`): runs split at every via landing or branch inside
them. The final DEF of the recipe has 198,916 pieces (5.02 per connection)
against 172,566 (4.36) with the previous configuration and LibreLane's
129,901 (3.21), and in an interleaved A/B of that gate (five runs each, the
gate's own invocation, load 0.8 to 1.3) it took 6.79 s wall and 6.63 s CPU
against 5.99 s and 5.91 s, and 4.99 s and 4.75 s on LibreLane's DEF. By
cause (first match per run), chains of three or more same-layer runs joined
at plain corners hold most of the increase: monotone staircases 51.2 k
pieces against 32.4 k before and 25.9 k for LibreLane, non-monotone zigzags
41.7 k against 20.0 k and 17.6 k. The rest of the gap to LibreLane is
Metal1 pin-access runs (11.1 k; LibreLane's DEF routes no Metal1) and
Metal4 and above (9.2 k against 3.9 k).

Rule repair converges in 3 passes on this placement with both the previous
configuration and the recipe (17, 6, 0 and 48, 13, 0 remaining violations;
the new prices without the pass need 5), and in 4 with the recipe on the
placement of the RustSys netlist published on 2026-11-02 (47, 8, 1, 0; the
previous configuration needs 2), of the 9 that `MAX_RULE_PASS` allows. On
that placement the recipe gives 126,138 vias against 138,730 with the
previous configuration, and the new prices without `strict_wide_cycles` end
unresolved (net 14325, rung 3 below). On the published 2026-11-02 placement
(its own `be_route` input, routed with these two configurations) both need
3 passes (10, 1, 0 and 32, 7, 0), and the recipe gives 127,991 vias against
139,975 for 1,475,608.2 µm of wire against 1,471,216.8 µm (wrong-way
25.2 k µm against 10.6 k µm; 445 s CPU against 422 s).

Tests: `via_reduction_trades_routed_vias_for_a_wrong_way_jog` (two routed
vias become a Metal2 jog at equal length),
`via_reduction_keeps_routes_it_cannot_improve_unchanged` (normal prices: the
output equals the route without the pass, so rejected re-routes restore
exactly), `via_reduction_restores_routes_whose_re_route_hits_the_expansion_cap`
(a capped search restores the old route),
`via_reduction_restores_routes_whose_re_route_fails_in_its_region` (a failed
search restores the route, its shape ids and its blocking shapes),
`via_reduction_skips_nets_above_max_terminals`,
`via_reduction_in_an_eco_route_keeps_prior_nets_and_re_assigns_rerouted_ones`,
`via_reduction_on_crossing_nets_is_valid_deterministic_and_never_adds_vias`,
`via_reduction_rejects_out_of_range_controls`.

The search lower bound (`routing_target::CostModel`) prices each via pair at
its own cost (Dijkstra over layer and covered directions instead of a hop
count) and each axis at the cheapest layer multiplier, scaling whole distances
and rounding down, while the search rounds each scaled wire step up
(`troute::scaled`). So the bound never exceeds the cost of a path and drops by
at most a step's cost per step (admissible and consistent), also for steps
whose scaled cost is fractional; with unit wire multipliers it is exact on the
obstacle-free relaxation
(`weighted_bound_is_admissible_consistent_and_exact_for_unit_wires`). The bend
surcharge depends on the step that reached a node while search states are
nodes, so with `bend_cost` the bound is still a lower bound but even weight 1
is not guaranteed to find the cheapest path.

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
# Explicit staged work budgets

Track routing accepts an optional `limits.scouts` object with
`wide_hard_expansions`, `soft_expansions`, `reuse_expansions`,
`fast_expansions`, `owner_first_expansions` and `owner_progress_expansions`.
Each must be positive and no larger than `max_search_expansions`. These settings
raise the staged work allowances without raising active-node, heap, geometry or
process-memory limits. Owner trials still share their explicit allocation and
all attempts charge the global ledger. Omission preserves the existing staged
limits and serialized legacy profiles. Wider work quotas do not qualify a route;
the complete geometry, DRC/LVS and timing checks remain mandatory.

`limits.eager_repair_expansions` optionally caps one immediate hard-search attempt
for a single displaced, previously routed net. It is a whole-net allowance and
must be positive and no greater than `max_search_expansions`; the existing hard
per-pin allowance also applies. Success retains both connections. Failure leaves
the displaced net on the normal repair queue, without recursive displacement.
All work remains charged globally and no extra completed-route copy is retained.
`limits.eager_repair_max_victims` extends this to a bounded group of one through
sixteen displaced nets (omission preserves one). All old affected routes are
removed before reconnect attempts. Each reconnect remains a hard search with
the same per-net work allowance; unsuccessful nets enter the ordinary queue.
The group retains only bounded net IDs, never copies of the removed routes.
`limits.eager_repair_search_expansions` optionally sets the per-pin hard-search
cap inside one eager reconnect (positive, at most the whole-net allowance).
Omission keeps the smaller of the allowance and `max_hard_search_expansions`.

### Stacked-pin access

Pin access is reserved terminal by terminal. When a terminal finds no legal
escape because earlier reservations (for example two pins of one cell on the
same vertical track) block it, the reservation retries once with the blockers'
footprints on the escape layer, grown by that layer's spacing, marked as
protected for the blocked terminal only. The footprint list is bounded by
`max_active_nodes` and each overlap test is charged to the access work budget.
On the Ibex/IHP run this turned `NO_ROUTE: pin 19921 net 5374 has no legal
access` into a legal reservation without changing any rule.


## Same-net wide-metal spacing

Wide-metal checking retains the original rectangles of each component with wide
metal to subtract covered area from same-net gap corridors before projecting
remaining voids onto the facing side. A partial-width tongue does not remove
its height from the run; a full bridge does. A remote bridge never exempts a
long notch in the same polygon. At most 64 empty rectangles are retained per
pair, and every subtraction charges work. Retention is capped by
`max_obstacles` and costs 32 bytes per retained rectangle plus component metadata;
sorts and every membership probe charge the existing comparison budget. The
router memory estimate adds 96 bytes per candidate rule shape (capped by
`max_obstacles`) for rectangles, component vectors and their capacity growth.
There are no paths, cuts, clauses, corners or per-thread copies. The Ibex control
has 543,038 wires, 318,590 via stacks and 3,531 patches before this repair; actual
RSS is measured under a 4 GiB guard.

Cleanup may bridge an illegal wide-spacing gap only within the same
electrical net, after checking foreign geometry (including existing patches). It
uses the existing eight-round loop and work/spatial budgets, and caps added patches
by `max_segments`. All local rules run again after a bridge; blocked or remaining
violations go to strict re-routing. Same-net pairs never receive owner halos,
which cannot exclude their own net. The foundry deck remains the independent
geometry oracle; this local check is not signoff.

A wide pair that survives strict re-routing and blocked halo repairs may come
from a detail edge closing a loop through fixed pin escapes. Once per net, the
router checks at most four short local detail wires. Fixed plans are borrowed
and retained, and candidates sharing a collinear interval with a fixed escape
are excluded. A candidate is removed only when normalized wires, vias and every
access point remain in one endpoint-graph component both before and after it.
The next ordinary cleanup checks area, notches and wide spacing again.

The local plan/detail/access total is capped at 8,192 records; larger nets fall
back to existing repair. Normalized output has the same cap. All scans, sorting,
normalization and graph traversal share the original comparison ledger. There
are no path lists or graph replicas per thread. The estimate includes 4 MiB
local scratch and 64 bytes per terminal for the bounded attempted-net set.

The local wide-cycle repair admits at most 8192 route records and 8192
perpendicular wire pairs before normalization. Only four detail-wire candidates
per net are considered. Fixed pin escapes remain unchanged. Its conservative
16 MiB scratch allowance includes the two junction indexes; all work also
charges the enclosing routing budget. Successful cuts restart geometric
validation before any stale violation can requeue the changed owner.

## Rule-repair escalation: a repeated outcome must change

Rule repair is deterministic, so a net re-routed into unchanged surroundings
rebuilds the same geometry. Each rung of the ladder below changes the outcome
when the same thing happens again. When the ladder runs out, the command fails
with a structured diagnostic instead of looping silently:

1. A violating net is re-routed strictly (no same-net near misses). If the
   strict search finds no path, the net is relaxed once.
2. If the same violation geometry repeats on a relaxed net, that net gets a
   *notch zone* around the violation. Inside the zone it is searched strictly;
   elsewhere it stays relaxed. A zone that walls the net in is dropped. There
   are at most 4,096 zones in total.
3. A foreign wide-metal pair that survives a repair first gets a halo around
   the owner, then a halo around the neighbour's facing metal. After that, a
   bounded cut of the owner's local detail cycle is tried once per net (for
   a relaxed owner). With `qor.strict_wide_cycles` the cut comes first, from
   the second sighting, for every owner, at most three times per wide region,
   and an owner cut in a pass waits for the next pass's check instead of
   being re-routed on a stale entry (other violations of the pass are still
   handled). A strict search refuses same-net near misses only, and a detail
   loop through the owner's own fixed escape at full spacing is none: the
   ring's hole is filled by cleanup into wide metal facing a neighbour's
   fixed escape, a halo cannot move that escape, and every strict re-route
   rebuilt the ring (Ibex, placement of the RustSys netlist published on
   2026-11-02, `via_cost` 2520 and `wrong_way_factor` 3: net 14325 at pin D
   of two AOI221X1 cells, 220 nm from the facing pin-C escapes, unresolved
   after the last pass).
4. Two nets can keep ripping each other because their rip prices grow
   together. Once both directions of a pair reach 8 rips, the net ripped last
   *yields*: its soft search treats the partner's metal as blocked. A yield
   that walls the net in is dropped, and each direction is tried once. At most
   2^18 directed pairs are tracked, about 40 bytes each. Converging Ibex
   routes peak at 3–5 mutual rips, so they do not change.
5. With `qor.notch_bridge` only: a same-net notch that cleanup could not
   fill (the robust box and every bridge touch other nets' metal) and that
   repeats after a re-route gets a *reserved fill*: the first local fill
   candidate (robust box, stair bridge, then bars clipped to two widths past
   the gap) that only routed metal of other nets blocks is added as fixed
   metal of the net, and the nets in its way are re-routed, so the next
   cleanup can place the patch
   (`notch_fill_reservation_takes_the_first_fill_only_routed_metal_blocks`).
   A candidate blocked by fixed geometry is skipped; reservations are
   order-stable, placed once each and share the 4,096-zone cap. With the flag,
   cleanup's bridges also include the stair bridge: for two shapes that face
   each other over less than a width, one bar over both shapes' extents (at
   most one width past the facing span), at least a width thick across the
   gap, so the union is a Z whose inner corners are a width apart. Fill
   clearance (cleanup and reservation) checks the base spacing; a wide-metal
   conflict of a fill is left to the rule check and repair.
6. The rip-up cap (`max_ripups_per_net`), the repair-round cap and the 8
   rule passes bound everything. Exceeding any of them fails with `NO_ROUTE`
   or `ROUTING_RULE` and names the net and the violation.

These are the livelocks seen on Ibex and the rung that removes each one:

| Case | Symptom | Rung |
| --- | --- | --- |
| run07 | 2,898 min-area and notch leftovers after cleanup; the strict re-route died at a sub-area pad | area-aware stacks, min-area stub split (`min_area_cleanup_splits_the_stub_when_no_side_alone_has_room`, `vertical_access_pad_does_not_force_an_unrepairable_pin_notch`) |
| n199/n7552 | a foreign wide pair at fixed pin escapes, widened identically on every pass | failed-pin-first strict retry, wide-pair halos (`wide_metal_partial_width_tongue_keeps_the_facing_run`) |
| net 15647 | a same-net notch on a relaxed long net, identical for 8 passes | notch zone (`notch_zone_forbids_same_net_near_misses_only_inside_it`) |
| fe18c 15954/7015 | cleanup merged a same-net notch into wide metal facing a fixed escape (235 nm < 240 nm) | notch zone |
| fe22b 14972/13203 | two nets ripped each other 256 times in pass 1 | yield (`yielding_net_never_rips_its_protected_partner`); the full replay converges |
| Metal2 access, net 12085 | a Metal2 pad 175 nm above its own escape, facing it over 80 nm; robust box and bars touched other nets for 8 passes | `notch_bridge`: stair bridge (`captured_metal2_stair_notch_gets_a_neck_free_bridge`, `stair_notch_is_bridged_when_the_robust_fill_touches_other_nets`) and reserved fill (`notch_fill_reservation_takes_the_first_fill_only_routed_metal_blocks`); the published-placement replay with Metal2 access completes |
| flow placement, net 7237 pin 30742 | with `net_aware_access`, a failed pin's repair re-blocked it with the same net-nearer top of a neighbour 4 times (`has no legal access` after 14 s) | hinted terminals fall back to the cheapest top once they fail or are displaced (`net_aware_access_falls_back_to_the_cheapest_tops_after_a_failed_escape`: a row that livelocks without the fallback); a hinted pass that still fails is redone without hints (`failed_net_aware_access_is_redone_without_hints`) |
| placement of the 2026-11-02 RustSys netlist, `via_cost` 2520 and `wrong_way_factor` 3, net 14325 | a strict owner's detail loop through its own pin-D escape (two Metal2 jogs one track over) is filled by cleanup into a 620 nm wide block 220 nm from the neighbours' fixed pin-C escapes (240 nm wide spacing); the halos cannot move a fixed escape, the strict re-route rebuilds the loop, 2 pairs unresolved after the last pass | `strict_wide_cycles`: the cycle cut runs for strict owners first, from the second sighting, at most three cuts per region (`strict_wide_cycle_entry_cuts_a_redundant_loop_wire_once_per_pass_and_region`, `strict_wide_cycle_cuts_are_bounded_per_region`); the replay then completes in rule pass 3 after 5 cuts. No small fixture rebuilds the ring by itself: the end-to-end evidence is this replay, which fails without the option and completes with it |
