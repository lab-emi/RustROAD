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
`routes.rbin` byte-identical to the one of the RustROAD `485b5e6` merged head.

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
| `via_reduction` | Post-route via reduction (section below). Omitted keeps the routes of the repair loop. With `keep_pieces` a re-route must also not add DEF wire pieces ("Wire pieces against vias" below). |
| `dedupe_vias` | One via per net, layer pair and point: the pin escapes of a net that land vias at one point share one right after pin access, and the output writes one (section below). |
| `straighten` | Post-route topology cleanup: same-layer staircases and zigzags become straighter same-net geometry where the clearance and same-net rule checks allow (section below). Omitted keeps the routes. With `neighbors`, a chain walled in by other nets' routed metal is rewritten together with them ("Wire pieces against vias" below). |
| `def_tracks` | `export-def` declares the track grid of the router (`TRACKS`, one statement per axis and routing layer). Output only; omitted writes no track statements ("Wire pieces against vias" below). |

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

The wrong-way jogs cost the shared OpenRCX extraction gate, most of whose
work follows the DEF's wire pieces (`docs/extraction.md`, "Oracles";
`scripts/def_pieces.py`): runs split at every via landing or branch inside
them. The final DEF of the recipe has 198,916 pieces (5.02 per connection)
against 172,566 (4.36) with the previous configuration and LibreLane's
129,901 (3.21). In an interleaved A/B of that gate (five runs each, the
gate's own invocation, load 0.8 to 1.3) it took 6.79 s wall and 6.63 s CPU
against 5.99 s and 5.91 s, and 4.99 s and 4.75 s on LibreLane's DEF. Over
four interleaved A/Bs of the same three DEFs (the gate twice, the
producer's `be_openrcx` step, and the gate's OpenRCX script alone; five or
six runs each, load 0.4 to 1.5) the recipe took 1.36 to 1.49 times
LibreLane's wall time and 1.38 to 1.48 times its CPU time, and 12 % to 17 %
more wall time than the previous configuration. By cause (first match per
run), chains of three or more same-layer runs joined at plain corners hold
most of the increase: monotone staircases 51.2 k pieces against 32.4 k
before (+58 %) and 25.9 k for LibreLane, non-monotone zigzags 41.7 k
against 20.0 k (+108 %) and 17.6 k. The rest of the gap to LibreLane is
Metal1 pin-access runs (11.1 k; LibreLane's DEF routes no Metal1) and
Metal4 and above (9.2 k against 3.9 k).

Both native DEFs (previous configuration and recipe) also hold 24 redundant
via pairs: two Via1 instances of different variants (`Via1_YY_so` and
`Via1_XY_so`) at one point of one net, either of which makes the
connection. In the previous configuration's DEF OpenRCX puts both in
series on 10 of these nets, 40 ohm more on one driver-to-sink path than
their continuous Metal1 (`docs/extraction.md`, "Oracles"); on `n9210`,
with either via removed, it does not. Both vias of each pair are pin-access
escape vias; `dedupe_vias` keeps one of them right after pin access (a
geometry change, so rule repair and the signoff checks run on it again;
"Coincident same-net vias" below).

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

#### Coincident same-net vias (`track_router.qor.dedupe_vias`)

Both native Ibex DEFs of 2026-11-04 (phys configuration and recipe) hold 24
nets with two Via1 instances of different variants (`Via1_YY_so` and
`Via1_XY_so`) at one point, written between two Metal1 runs with a Metal2
stub ending there. A traced replay of the final configuration's `be_route`
input (each such group listed with the origin of every via) shows where all
24 come from: pin access. In each, two pins of one net lie on one Metal1 row,
and `net_aware_access` pulls each pin's escape towards the other's: both
escapes run on Metal1 to the same via point between the pins and up one
Metal2 stub to a shared top 840 DBU away, each through the variant its own
access search picked. Both vias are escape records: none is a routed via,
and neither is at its escape's top. On 8 of the nets (two terminals) the
route itself is empty, because both escapes end on the same top. `assemble`
removed identical records only, so it wrote both. Either via makes the
connection; OpenRCX put the pair in series on 10 of these nets (40 ohm on one
driver-to-sink path, `docs/extraction.md`).

With `dedupe_vias`, the escapes of one net share one via per layer pair and
point right after pin access, before any search (`share_escape_vias`): the
first variant, by kind, whose pads lie along the net's escape wires through
that point on each of its two layers where those wires share an axis (the
`orient_vias` rule), else the lowest kind, stays in the first plan (terminal
order) that holds it, and every other via of the group leaves its plan
together with its pads in the search index. So the searches, rule repair and
the output all see one via, and the index matches what is written. A pair at
an escape's own top stays in the plans (the escape's stacking and pad checks
read that via); for that case, and for a routed via that `orient_vias`
re-picks on top of its own escape via (the other way a pair could arise),
`assemble` still keeps one via per (net, layer pair, point), by the same rule
over all of the net's wires.

On the Ibex `be_route` input the trace reports 39 escape vias shared (one of
each of the 24 two-variant pairs, and 15 exact duplicates of another escape's
via, which the output already wrote once) and 0 removed by `assemble` in each
rule pass; the exported DEF has no coincident pair, and with the recipe's
straightening the route (`routes.rbin`) and DEF are byte-identical to those of
the earlier output-stage removal. With the removal alone exactly these 24
nets change, with 32 fewer Via1 (53,271 to 53,239): on 8 of them the pair had
joined the same two points twice, a loop that kept the Metal2 stub at the
shared top alive; with one via left, that via and stub lead to no access
point, and cleanup's dead-end pruning removes them, so the net becomes one
Metal1 run (on `n9210` the DEF entry shrinks from two Metal1 segments, both
vias, the stub and a patch to a single Metal1 segment).

Tests: `escape_vias_of_one_net_at_one_point_share_one_via` (the Ibex pattern:
the variant along the escape wires stays, the other leaves its plan and the
index with the plan's shape layout kept, a pair at the escapes' own tops
stays for `assemble`, which writes it once),
`coincident_vias_of_one_layer_pair_keep_the_variant_along_the_wires` (the
output rule: a via of another layer pair and a lone via elsewhere stay, the
lowest kind when no variant fits, budget charged),
`coincident_via_dedupe_is_off_by_default_and_never_changes_a_clean_route`.

#### Post-route topology cleanup (`track_router.qor.straighten`)

The shared OpenRCX extraction gate costs more on the native Ibex DEF than
on LibreLane's (above, and `docs/extraction.md`), and most of its work
follows the DEF's wire pieces: maximal collinear runs split at every via
landing or branch. Same-layer staircases and zigzags (chains of three or
more runs joined at plain corners) held 92.9 k of the 198.7 k pieces of the
`be_route` replay of the final configuration, against 43.5 k of LibreLane's
129.9 k. A staircase is a shortest path with more bends than it needs:
among the rectilinear shortest paths between two points, the minimum-bend
ones have the fewest (C. D. Yang, D. T. Lee and C. K. Wong, "Rectilinear
path problems among rectilinear obstacles revisited", SIAM J. Comput.
24(3), 1995). The cleanup rewrites a routed net's chains into fewer-bend
same-net geometry where the router's own checks accept it, against the
fixed metal of every other net, once negotiated repair and the via
reduction are done (first rule pass only):

1. **Chains.** On each routing layer, a chain is a maximal sequence of the
   net's routed runs joined at plain corners: exactly two runs end there and
   nothing else of the net is there (no via landing on that layer, no pin
   access point, no escape wire, no pin shape, no run passing through). Its
   two ends are junctions and stay. A chain through a run that meets the
   net's escape geometry inside it (an escape wire ending on it, an escape
   via or access point inside it) is left alone: the route's runs are not
   split at such points, so the run is a connection.
2. **Optional bend-priced re-route** (`reroute`; not in the Ibex recipe,
   below): before the rewrites, each
   net with at most `max_terminals` terminals that holds a chain is ripped
   up and searched again (the via-reduction machinery: hard search in its
   guide grown by `expand_margin`, every other net fixed, at most
   `net_expansions` expansions) with `bend_cost` added to every same-layer
   turn; the new route is kept only with fewer runs, no more routed vias and
   no more wire, else the old route's index shapes are restored unchanged.
3. **Rewrites.** Each chain of three or more runs is replaced by the first
   legal candidate with fewer runs, fewest runs first: the straight run
   between aligned junctions, the two Ls, the Zs whose middle leg lies on one
   of the chain's own corner lines, then the minimum-bend shortest path over
   the layer's track grid (tracks and stops) inside the junctions' box: a
   dynamic programme over the monotone grid with the arrival axis as state,
   ties to continuing straight. Without `turn_at_ends` a candidate leaves
   both junctions along the chain's own axis there; with it, that is a
   preference among candidates with as few runs.
4. **Checks.** Every candidate is a shortest path, so a rewrite never adds
   wire, and it touches no via (the optional re-route may remove vias and
   wire, never add them). Each new run must be clear of every other
   net's metal, fixed geometry and cell obstructions (`Spatial::probe`, the
   test of a search step) and of the wide-metal spacing of any shape of
   another net that is wide on its own (`Spatial::wide_clear`); it may touch
   same-net metal only where the chain's end run touched it at that end
   junction (no new branch, no loop through the net, no more overlap of its
   own pins); the net's same-net rule check on that layer
   (`route_rules::check_group`: notches, necks, holes, minimum area) must
   report no violation the layer did not have; and the rewritten net must
   pass the endpoint connectivity check of `validate`, else it keeps its
   route. Cleanup, the rule-repair passes and `validate` then run as usual.
5. `passes` repeats the rewrites over the new state (a pass that keeps
   nothing ends it). Nets, chains and candidates are taken in a fixed order,
   so the result is a function of the routed state. In an ECO route a net
   that still holds its prior geometry is left as it is.

Route replays of the Ibex `be_route` input of the final configuration
(superproject `e0773f6`: RustROAD `deb9705` recipe routing, the flow's
`--threads 4` invocation; QoR from the exported DEF with the receipt code,
pieces and causes with `scripts/def_pieces.py`):

| `be_route` replay | vias | wire µm | wrong-way µm | pieces | staircase / zigzag pieces |
| --- | ---: | ---: | ---: | ---: | ---: |
| final configuration | 118,424 | 897,717.5 | 26,213.8 | 198,730 | 51,235 / 41,665 |
| `dedupe_vias` (output-stage removal) | 118,392 | 897,713.7 | 26,213.8 | 198,714 | 51,235 / 41,665 |
| plus `straighten` (2 passes) | 118,393 | 897,660.8 | 26,179.7 | 195,300 | 48,060 / 40,635 |
| plus `turn_at_ends` | 118,331 | 897,357.3 | 26,110.3 | 192,134 | 42,774 / 39,411 |
| the same with the escape vias shared at pin access (the recipe) | 118,331 | 897,357.3 | 26,110.3 | 192,134 | 42,774 / 39,411 |
| plus `reroute` (bend 420, 10,000 expansions) | 117,886 | 895,577.7 | 25,385.6 | 189,040 | 41,807 / 37,482 |
| plus `reroute` (bend 420, 4,000 expansions; the earlier recipe) | 118,114 | 896,263.3 | 25,703.1 | 190,096 | 42,343 / 38,004 |
| `dedupe_vias`, the rewrites with `turn_at_ends` and main-search `bend_cost` 20 (no `reroute`) | 118,871 | 897,023.9 | 23,882.3 | 183,917 | 39,804 / 32,750 |
| the same with `bend_cost` 100 | 118,845 | 896,935.1 | 23,727.8 | 183,593 | 39,717 / 32,393 |

The row with the escape vias shared at pin access writes the same
`routes.rbin` and DEF as the row above it (output-stage removal).

The rewrites take about 2 s (1.6 to 1.9 s in every trace). On the recipe
line, 4,572 chains of 3,161 nets become 7,112 fewer runs and 157 µm less
wire, with no connectivity fallback. Most chains stay: in the first pass
4,537 of 10,735 chains were rewritten; candidates were refused 21,717 times
for other nets' metal, 1,230 times for same-net contact and 46 times by the
same-net rule check (never by the wide-metal test on Ibex; its unit test
exercises it), and the minimum-bend search found a legal shortest path in
3,934 of the 6,557 boxes it searched. The second pass rewrote 35 more.
These guarantees hold per rewrite, not per net of the final DEF: cleanup and
the rule-repair passes that follow re-route some nets, as they do without
the cleanup.

The bend-priced re-route (`reroute`) is not in the recipe. In every trace
it took 30.9 to 35.1 s (5,875 nets tried, 1,389 kept, 904 stopped at 4,000
expansions; 66 s at 10,000), and the route after the via reduction took
43.8 to 48.8 s with it against 13.0 to 21.0 s without it (trace times
within each process; the work before the via reduction is identical): 25.1
to 31.0 s more in each of four back-to-back pairs, 7 to 9 % of `be_route`.
For that it gave 2.0 k fewer
pieces than the rewrites alone (192,134 to 190,096, about 1 %), 217 fewer
vias, 1.1 mm less wire and 407 µm less wrong-way wire, and in interleaved
runs on the final DEFs OpenRCX took about 0.25 s less per run (below). That
is more than its 1 % of the pieces alone would explain, but a small fraction
of its route time. A bend surcharge in
the main search (the existing `bend_cost` control) removes more pieces but
adds about 450 vias, so the recipe leaves it off as well.

Full flow (the private campaign kit: superproject `e0773f6` with this routing
change, RustROAD `c7471d7`, the other binaries of the frozen final set): the
producer completed all 119 steps (exit 0; `timing_accuracy` largest
RustSTA/OpenSTA difference 0.1639 ps) and every shared gate passed (DRC,
antenna, latch-up, LVS, timing, simulation, both equivalences, density;
OpenRCX timing TT setup/hold worst slack 49.636/0.111 ns, SS 49.442/0.211 ns,
no setup, hold, slew, capacitance or fanout violation). `be_route` wrote the
`routes.rbin` of the replay above. The final DEF has 118,525 vias (Via1
53,239, Via2 49,179, Via3 13,170, Via4 2,701, TopVia1 232, TopVia2 4)
against 118,610, 897,357.3 µm of signal wire against 897,717.5, 26,180.9 µm
of wrong-way wire against 26,279.3 (LibreLane 17,301), 192,328 pieces against
198,916 (LibreLane 129,901; staircase and zigzag 42,789 and 39,398 against
51,249 and 41,652) and no coincident via pair; area, cells and every netlist
are unchanged. Per net against the previous final DEF, 3,232 nets changed;
9 of them got longer (28.1 µm in all, at most 9.6 µm) and 9 got more vias
(27 in all), through the rule passes after the cleanup, while the totals
fell by 360.2 µm and 85 vias. A second complete run wrote byte-identical
GDS, DEF, netlists and native SPEF (the OpenRCX SPEF file differs, every
timing value read from it is equal, and six reruns of the `be_openrcx` step
on this DEF wrote identical SPEF apart from its date line), and the final
binary with both controls off routes the replay input to the same
`routes.rbin` and DEF as the `deb9705` build.

OpenRCX on the final DEFs (interleaved runs, each holding the host's
heavy-job lock; medians, wall and CPU). The producer's `be_openrcx` step, in
two batches (nine runs per DEF, load 0.6 to 1.0): 7.65 and 7.36 s on the new
DEF, 8.02 and 7.86 s on the previous native DEF and 5.83 and 5.73 s on
LibreLane's, so 1.31 and 1.28 times LibreLane against 1.38 and 1.37 (median
per-run ratio 1.28 and 1.30); faster than on the previous DEF in 7 of 9
pairs (median 0.57 s wall, 0.55 s CPU). The shared extraction gate (six runs,
load 0.8 to 1.0): 7.15 and 7.04 s against 7.37 and 7.23 s and 5.24 and
5.06 s, so 1.37 and 1.39 times LibreLane against 1.41 and 1.43; the two
native DEFs are not separated there (median paired difference 0.00 s wall,
faster in 4 of 6 pairs). The same batches had a fourth arm, the final DEF
of the earlier recipe with the re-route (190,290 pieces): about 0.25 s
faster than the new DEF per run (`be_openrcx` 0.27 s, faster in 5 of 6
pairs; gate 0.25 s, 4 of 6), 1.27 and 1.26 times LibreLane for the step and
1.30 and 1.32 for the gate. Earlier batches on that DEF gave, for the step,
1.28 and 1.27 times LibreLane as a ratio of medians (five runs, load 0.8 to
1.0; 1.35 and 1.34 as the median per-run ratio) and 1.37 and 1.39 (eight
runs, load 1.1 to 1.4), with the previous native DEF at 1.38 to 1.51; and
for the gate 1.33 and 1.32 (six runs, load 0.85 to 1.1, faster than the
previous DEF in 6 of 6 pairs by a median 0.36 s) and, on a noisier host,
1.37 and 1.39 with the two native DEFs not separated. Across batches the
host moves these ratios by 0.1 or more, more than the cleanup does: it takes
about 0.4 to 0.6 s (5 to 7 %) off the step, and OpenRCX still takes about
1.3 to 1.4 times as long on the native DEF as on LibreLane's.

Route CPU, interleaved replays of the flow's `be_route` input with the final
binaries (three pairs, each run holding the heavy-job lock, load 1.0 to
1.2): 327.9 s median with the recipe's cleanup (311.0 to 338.5 s) against
316.9 s without it (`deb9705` build and final configuration; 316.1 to
341.2 s), paired differences -2.6, +11.8 and -5.9 s. The whole-run spread
comes from the routing before the via reduction, which is the same work in
both arms (296.7 to 323.2 s). Within each run, the rewrites took 1.6 to
1.9 s, and everything after the via reduction took 13.3 to 13.9 s with the
cleanup against 15.5 to 18.2 s without it, since the rewritten nets leave
rule repair less to do: the cleanup costs no measurable route time. In the
full flow `be_route` took 308.6 s CPU (310.4 s wall). LibreLane's published
routing (global routing with antenna repair plus detailed routing) took
1,865.8 s.

Tests: `candidates_keep_both_end_axes_and_are_never_longer` (candidate
shapes, order, end axes, length), `straightening_turns_a_staircase_into_a_z_on_its_own_corner_row`
(validated, deterministic, no via, equal length; with `turn_at_ends` the L),
`straightening_skips_a_candidate_another_nets_metal_blocks` (a blocked row
is skipped, the minimum-bend search finds another, a staircase walled in
on both sides stays), `straightening_reroute_replaces_a_staircase_by_a_bend_priced_search`,
`straightening_chains_stop_at_junctions_and_skip_runs_that_meet_escapes_inside`,
`straightening_refuses_a_candidate_inside_wide_metal_spacing` (a candidate
clear at the normal spacing but inside the wide-metal spacing of another
net's wide shape is refused, the other corner row is taken; without the wide
rule the refused row is taken),
`straightening_in_an_eco_route_leaves_nets_that_keep_their_prior_geometry`
(neither the re-route nor the rewrites touch a net that still holds its prior
geometry, both run on a net the ECO routed again, and an identity ECO of a
routed design with the cleanup on writes the routes of the same ECO without
it), `straightening_turns_at_a_via_junction_and_the_via_follows_the_new_arrival`
(with `turn_at_ends` a staircase ending at a routed via becomes an L that
arrives along the other axis, `orient_vias` re-picks the via variant whose pad
lies along it, and the result validates with the same vias),
`straightening_and_dedupe_are_off_by_default_and_reject_out_of_range_passes`.

#### Wire pieces against vias (prices, joint moves, escape jogs, track grid)

After the cleanup above, the final native DEF of the campaign of
2026-11-05 still held 192,328 wire pieces against LibreLane's 129,901, and
OpenRCX took 1.36 times as long on it in the shared gate. By layer and
direction (`scripts/def_pieces.py`, split by layer), almost all of the
difference is Metal2, the vertical access layer: 121,306 pieces against
82,748, of them 71,562 in same-layer chains (staircases and zigzags)
against 34,772. Metal3 holds 47,240 against 42,638, Metal4 and above
12,651 against 4,515, and Metal1 escape wires 11,131 (LibreLane writes no
Metal1 wire, but jogs on Metal2 at 10,117 Via1 landings instead). Two
causes stand out:

* **Escape stubs.** A native escape reaches the Metal2 area of a later
  stacked via with an 840 DBU stub from its Via1 (`area_stacks`); the
  route starts at the stub's top, and when it leaves that top along the
  other axis or back past the Via1, the stub and the route's first runs
  form a chain that the router never sees as one (the top is a junction of
  the route). 7,693 Metal2 chains are such Zs from a Via1 to a Via2
  (vertical, jog, vertical) against 1,911 in LibreLane's DEF, and 2,385 of
  them only jog to a track because the Via1 sits off the track.
  LibreLane's access stacks Via2 on Via1 at 11,035 pins and writes the
  Metal2 minimum-area metal there as `RECT` patches, which OpenRCX does not
  extract at all (`docs/extraction.md`, "RECT patches are not extracted"):
  those accesses hold no Metal2 piece for that reader. (Turning the
  router's area stubs into patches would change the extracted values, so
  it is not done; `area_stacks: false` leaves pins without legal access on
  this placement, `NO_ROUTE`.)
* **Prices.** With `wrong_way_factor` 3 a one-track Metal2 jog (1,260)
  costs a quarter of a via pair (2 x 2,520), so most track changes near
  the pins are jogs; each jog is a piece and splits the run it leaves.
  The previous configuration (factor 4, via cost 1,680) had 172,566 pieces
  but 130,729 vias.

Route replays of the campaign's `be_route` input (its `road-final` `rev3`
with the flow's `--threads 4 --max-expansions 80000000` invocation; the
first row writes the campaign's own `routes.rbin`), QoR from the exported
DEF with the receipt code, pieces and causes with `scripts/def_pieces.py`,
route CPU from GNU time (1-minute load 1.2 to 5.8, each replay holding the
heavy-job lock):

| `be_route` replay (changes to the recipe) | vias | Via2 / Via3 / Via4 | wire µm | wrong-way µm | pieces | staircase / zigzag pieces | route CPU s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| the campaign recipe (`wrong_way_factor` 3) | 118,331 | 49,169 / 13,050 / 2,645 | 897,357.3 | 26,110.3 | 192,134 | 42,774 / 39,411 | 243.2 |
| `bend_cost` 100 | 118,845 | 49,453 / 13,258 / 2,658 | 896,945.4 | 23,727.8 | 183,602 | 39,707 / 32,438 | 248.2 |
| no via reduction | 121,504 | 51,685 / 13,680 / 2,670 | 896,560.9 | 22,794.6 | 190,819 | 41,656 / 34,606 | 246.8 |
| joint moves (`neighbors`) | 118,171 | 49,056 / 13,003 / 2,638 | 896,795.2 | 25,797.5 | 190,618 | 42,480 / 38,042 | 269.1 |
| factor 4 | 120,627 | 51,409 / 13,156 / 2,570 | 897,758.9 | 17,820.5 | 177,349 | 34,555 / 32,044 | 244.7 |
| factor 4, `bend_cost` 100 | 120,952 | 51,502 / 13,298 / 2,651 | 897,420.9 | 16,839.3 | 171,959 | 32,171 / 28,726 | 238.4 |
| factor 5 | 122,571 | 52,663 / 13,781 / 2,613 | 896,763.4 | 14,041.4 | 168,797 | 30,031 / 25,819 | 227.0 |
| factor 5, `via_milli` 3000 | 120,690 | 51,129 / 13,452 / 2,597 | 898,026.1 | 16,461.4 | 170,658 | 30,782 / 29,586 | 218.2 |
| factor 5, via reduction B | 118,550 | 50,243 / 12,315 / 2,476 | 898,199.0 | 17,624.5 | 171,155 | 32,149 / 31,530 | 259.6 |
| factor 5, via reduction B with `keep_pieces` | 124,169 | 53,711 / 14,387 / 2,557 | 896,298.0 | 12,664.0 | 165,826 | 28,403 / 22,043 | 255.0 |
| factor 5, `bend_cost` 100, via reduction B | 119,040 | 50,188 / 12,616 / 2,691 | 898,295.8 | 16,963.4 | 166,543 | 29,784 / 28,496 | 262.3 |
| the same with joint moves | 118,964 | 50,136 / 12,596 / 2,689 | 897,928.3 | 16,795.9 | 165,591 | 29,372 / 27,565 | 274.7 |
| the same with joint moves and escape jogs | 118,975 | 50,142 / 12,599 / 2,691 | 895,863.4 | 16,815.3 | 162,920 | 26,896 / 23,377 | 266.6 |
| factor 6 | 125,108 | 54,016 / 14,624 / 2,902 | 896,424.7 | 11,489.9 | 164,435 | 27,132 / 21,202 | 248.9 |
| factor 6, `bend_cost` 100, via reduction B | 120,454 | 51,151 / 12,890 / 2,810 | 897,529.6 | 14,937.4 | 163,465 | 28,763 / 24,708 | 243.5 |
| the same with joint moves and escape jogs (the new recipe) | 120,377 | 51,086 / 12,888 / 2,800 | 895,279.2 | 14,793.7 | 159,861 | 25,365 / 19,805 | 244.1 |
| factor 7, `bend_cost` 100, via reduction B, joint moves, escape jogs | 122,280 | 52,319 / 13,485 / 2,855 | 895,686.1 | 11,562.4 | 154,953 | 23,791 / 15,366 | 270.9 |
| factor 7, `bend_cost` 100, via reduction B with `via_milli` 4000, joint moves, escape jogs | 120,415 | 50,897 / 13,074 / 2,822 | 897,127.5 | 14,011.1 | 156,406 | 24,844 / 18,461 | 271.5 |
| factor 7, `bend_cost` 100, via reduction C, joint moves, escape jogs | 118,539 | 50,352 / 12,134 / 2,485 | 896,824.0 | 14,782.8 | 156,255 | 25,940 / 19,429 | 379.3 |
| factor 8, the same | 119,613 | 50,950 / 12,457 / 2,595 | 896,400.0 | 13,440.7 | 154,656 | 24,717 / 17,584 | 354.8 |
| factor 10, `bend_cost` 100, via reduction D, joint moves, escape jogs | 119,005 | 50,879 / 12,122 / 2,431 | 897,597.3 | 13,362.8 | 152,543 | 23,762 / 17,184 | 402.0 |
| the same with Metal4 and Metal5 wire at 1.5 and TopMetal wire at 2 times its price (`layer_cost_milli`) | 128,054 | 61,512 / 11,102 / 2,124 | 908,859.1 | 12,812.5 | 162,328 | 25,033 / 16,780 | 1,108.0 |

Via reduction presets: A is the campaign recipe's (`via_milli` 2000, one
pass, nets up to 10 terminals, 10,000 expansions per net, connections up to
20 um); B `via_milli` 3000, two passes, 20 terminals, 20,000 expansions,
40 um; C 4000, three passes, 32 terminals, 40,000 expansions, 60 um; D as C
with `via_milli` 5000 and 80 um. Rows without a preset use A.

`wrong_way_factor` is the strongest lever: from 3 to 4 it removes 14.8
thousand pieces and adds 2.3 thousand vias, from 4 to 5 8.6 thousand and
1.9 thousand, from 5 to 6 4.4 thousand and 2.5 thousand, mostly Via2 (track
changes near the pins become via pairs or straight runs instead of jogs),
for about the same wire. The main search's `bend_cost` 100 removes 4.6 to
8.5 thousand pieces more for at most 0.5 thousand vias (with search states
that are nodes, the surcharge is a heuristic for fewer bends, not an exact
minimum; 400 makes rip-up diverge on this placement, net 6284 ripped up
more than 256 times). A stronger via reduction takes the vias back: at
factor 5 preset B removes 4.0 thousand more vias than A for 2.4 thousand
pieces and 1.4 mm of wire, and at factor 7 preset C 3.7 thousand more than
B for 1.3 thousand pieces and 1.1 mm. It costs route CPU: the pass with D
tries 34 thousand re-routes in three passes, and the route takes 402 s of
CPU against 243 s (route CPU in this table varies by about 10 % with the
host's load). Without any via reduction the campaign recipe has 1.3
thousand fewer pieces and 3.2 thousand more vias, so the pass itself costs
few pieces. `via_reduction.keep_pieces` (a re-route must not add pieces)
keeps only 1,419 of the 3,686 re-routes of preset B at factor 5 and leaves
124,169 vias, above LibreLane's 122,194, so it is not used. A repair search
weight of 1 instead of 2 (`repair_heuristic_weight`) changed the factor-5
line by 22 pieces.

**Joint moves** (`straighten.neighbors`, `routing::StraightenNeighbors`):
the rewrites refuse most remaining chains because another net's routed
metal lies on every shorter candidate (21,717 refusals in the campaign recipe's
replay). A walled-in chain whose candidate only the routed metal of at most
`max_victims` other nets refuses is rewritten together with those nets:
they are ripped up, the chain is rewritten with the rewrites' own checks,
and they are routed again as hard searches; the move is kept only when the
nets involved hold fewer DEF pieces in all (escape and routed wires merged
into maximal runs split at every junction), no more routed vias and at
most `length_per_piece` more wire per piece removed, else every route and
index shape is restored. This is incremental rip-up and reroute (R. Nair,
"A simple yet effective technique for global wiring", IEEE TCAD
6(2):165-172, 1987) in the form of the strong modifications of Mighty
(H. Shin and A. Sangiovanni-Vincentelli, "A detailed router based on
incremental routing modifications: Mighty", IEEE TCAD 6(6):942-955, 1987),
which move the nets in a connection's way instead of detouring around
them. On the campaign recipe's replay it tried 2,170 moves and kept 452 (530
found no rewrite or no route for a ripped net, 1,188 were not better):
1,525 pieces, 160 vias and 0.56 mm of wire fewer for 26 s more route CPU;
on the new recipe it tried 1,572 and kept 354, 890 pieces fewer in 12.7 s,
and on the factor-10 line 1,614 and 271, 580 pieces fewer. The gain
is small because most walls are fixed metal (escapes, pins, via pads of
access) or nets that cannot route elsewhere without as many pieces.

**Escape jogs** (`straighten.escape_jogs`): before the rewrites, an escape
whose access-layer stub runs straight from its via landing to its top,
where the route leaves with a jog to another track and then runs on along
the stub's axis, takes the jog at its via instead: the escape becomes the
jog and a stub of the old length on the route's track (to the old corner
when the route runs on away from the via, else from the via's row toward
the route), the route keeps the rest of its run and the turn at the top
disappears. That is one DEF piece fewer per escape, and where the route
turned back past the via (a U) twice the stub less wire. A move is kept
only when the new metal is clear of other nets and of wide-metal spacing,
touches no same-net metal but the via pad and the run it joins, adds no
same-net rule violation on the access layer, its new top is a graph node
and the net stays connected; the escape's top then moves to that node, so
later rule passes route from it. On the factor-5 line it examined 5,815
escapes and moved 2,904: 2.7 thousand pieces and 2.1 mm of wire fewer, 11
more vias (rule repair); on the new recipe 5,630 and 2,877, on the
factor-10 line 4,787 and 2,730.

**Track grid** (`def_tracks`): `export-def` declares the router's track
grid (`TRACKS`, one statement per axis and routing layer; on Ibex the same
14 statements as LibreLane's DEF). The route is unchanged. OpenRCX reads
the DEF faster and extracts a few nets differently with them
(`docs/extraction.md`, "Track statements change its work and a few
values"): on the campaign DEF the gate's OpenRCX script alone took a
median 4.46 s with the statements against 4.87 s without (five interleaved
runs each in one hold of the heavy-job lock, load 4.1 to 5.9; faster in all
five pairs, by a median 0.30 s; extraction phase 3,197 ms against 3,591 ms),
1.30 against 1.42 times its time on LibreLane's DEF in the same batch.

**OpenRCX time against pieces.** The gate's OpenRCX script alone, in one
hold of the heavy-job lock, five interleaved runs per DEF (load 4.1 to 5.9;
every DEF with the 14 track statements unless noted; the wires are the count
OpenRCX reports extracting):

| DEF | pieces | wires | wall s, median (range) | extraction phase ms | × LibreLane |
| --- | ---: | ---: | ---: | ---: | ---: |
| campaign native DEF, no track statements | 192,328 | 196,598 | 4.87 (4.50–5.00) | 3,591 | 1.42 |
| campaign native DEF | 192,328 | 196,598 | 4.46 (4.38–4.70) | 3,197 | 1.30 |
| replay: factor 5, `bend_cost` 100, via reduction B, joint moves | 165,591 | 169,929 | 3.66 (3.53–3.99) | 2,458 | 1.07 |
| replay: the new recipe | 159,861 | 164,205 | 3.84 (3.77–4.27) | 2,618 | 1.12 |
| replay: factor 7, via reduction B at `via_milli` 4000, joint moves, escape jogs | 156,406 | 160,730 | 3.61 (3.46–3.85) | 2,358 | 1.05 |
| replay: factor 7, via reduction C, joint moves, escape jogs | 156,255 | 160,591 | 3.60 (3.46–3.98) | 2,427 | 1.05 |
| replay: factor 7, via reduction B, joint moves, escape jogs | 154,953 | 159,260 | 3.90 (3.33–4.35) | 2,512 | 1.14 |
| full flow: factor 10, via reduction D, joint moves, escape jogs | 152,749 | 157,003 | 3.68 (3.50–3.94) | 2,495 | 1.07 |
| LibreLane | 129,901 | 133,158 | 3.43 (3.29–3.55) | 1,985 | 1.00 |

From 192 to 152–166 thousand pieces the run gets 0.6 to 0.9 s faster, but
inside that band the time does not follow the piece count: the extraction
phase costs 14.5 to 16.3 µs per reported wire on the native DEFs (14.9 on
LibreLane's), and the arms' ranges overlap. Reading the native DEF takes
about 0.2 s less than LibreLane's (0.4 s against 0.6 s: 12.5 MB against
21.3 MB; LibreLane's has 245,538 routing statements and 30,477 patches
against 44 to 46 thousand statements and under a thousand patches),
which offsets most of what remains of the extraction phase. At LibreLane's
per-wire cost the extraction phase would match LibreLane's at about 133
thousand wires; the native DEFs keep 23 to 30 thousand pieces more, and two
access choices account for most of them: 9.1 to 9.7 thousand Metal1 escape
pieces (LibreLane's DEF has no Metal1 wire) and the Metal2 stub of every
stacked access (LibreLane's 11,035 stacked accesses write that metal as
`RECT` patches, which OpenRCX does not extract). Both are honest metal of
the native layout; neither is hidden from the extractor.

**The recipe.** `wrong_way_factor` 6, `bend_cost` 100, via reduction B
(`via_milli` 3000, two passes, 20 terminals, 20,000 expansions, 40 µm), the
joint moves (`max_victims` 2, `max_terminals` 10, `net_expansions` 10000,
`length_per_piece` 420), the escape jogs and `def_tracks`. Factors 7 and 10
remove 3 to 7 thousand pieces more, but their route is slower (the via
reduction of the factor-10 line, preset D, takes the route to 402 s of CPU;
factor 7 with `via_milli` 4000 took 271.5 s in its replay and 264.1 s in a
full run against 243.1 s for this recipe), and the full run at factor 7
(without the joint moves) routed one net (`n132`, eight pins, 299.1 µm
against 281.9 µm in the campaign) to 0.965 of its driver's maximum
capacitance, so post-route repair resized that driver and took 47.3 s
instead of 5.1 s; its shared gates all passed, and OpenRCX on its layout
took 1.13 times LibreLane's time in the gate's script, 1.11 in the gate
and the `be_openrcx` step (five runs each, load 1.9 to 2.8).

Full flow (the private campaign kit: superproject `3cf7323` with the recipe
change, RustROAD `261d49c` for `rustroad`, the other binaries of the
2026-11-05 campaign), two complete runs: the producer completed all 114
steps of the kit manifest (exit 0; `timing_accuracy` largest RustSTA/OpenSTA
difference 0.2697 ps) and every shared gate passed (DRC, antenna, latch-up,
LVS, timing, simulation, both equivalences, density; OpenRCX timing TT
setup/hold worst slack 49.628/0.108 ns, SS 49.429/0.206 ns, no setup, hold,
slew, capacitance or fanout violation); post-route repair found nothing to
resize. The final DEF has 120,585 vias (Via1 53,239, Via2 51,104, Via3
13,020, Via4 2,846, TopVia1 374, TopVia2 2) against 118,525 (LibreLane
122,194), 895,279.2 µm of signal wire against 897,357.3 (LibreLane
969,571.6), 160,069 pieces against 192,328 (LibreLane 129,901; staircase and
zigzag 25,365 and 19,805 against 42,789 and 39,398) and the 14 track
statements; area, cells and the synthesis, final and powered netlists are
byte-identical to the campaign's. The second run wrote byte-identical GDS,
DEF, netlists and native SPEF; both OpenRCX SPEF files differ between the
runs, every timing value read from them is equal, and five reruns of the
`be_openrcx` step on this DEF wrote identical SPEF apart from its date
line. `be_route` took 243.4 and 221.1 s (243.1 and 220.2 s CPU; the
campaign's took 302.2 s at its load). In three interleaved pairs of route
replays (each run holding the heavy-job lock, load 0.9 to 3.1) the recipe
took a median 230.6 s of CPU (227.5 to 234.0 s) against 220.2 s (212.6 to
225.8 s) for the campaign recipe with the campaign's binary, 1.05 times
(paired differences +4.8, +14.9 and +13.8 s), and wrote the `routes.rbin`
of its row in the table above every time. The higher wrong-way price ends
negotiated repair 38 to 49 s earlier (155 to 160 s into the route against
193 to 206 s); the two via-reduction passes (44 to 46 s against 7 to 8 s)
and the rewrites, joint moves and rule passes after them (27 s against 9 to
11 s) spend that and 5 to 15 s more.

OpenRCX on the final DEF of the first run (new), the campaign's (old) and
LibreLane's, in one hold of the heavy-job lock right after that run (five
interleaved runs each, load 1.5 to 2.9; median wall seconds and range):

| | new | old | LibreLane | new / LibreLane | old / LibreLane |
| --- | ---: | ---: | ---: | ---: | ---: |
| shared `extraction` gate | 3.80 (3.63–4.03) | 5.10 (4.80–5.64) | 3.84 (3.44–4.23) | 0.99 | 1.33 |
| the same, CPU | 3.67 | 4.91 | 3.59 | 1.02 | 1.37 |
| the producer's `be_openrcx` step | 3.81 (3.77–4.18) | 5.02 (5.00–5.39) | 3.60 (3.45–3.63) | 1.06 | 1.39 |
| the gate's OpenRCX script alone | 3.32 (3.15–3.51) | 4.43 (4.42–4.64) | 3.29 (3.19–3.35) | 1.01 | 1.35 |
| its extraction phase (ms) | 2,190 | 3,330 | 1,946 | 1.13 | 1.71 |

A later batch of the script alone on an almost idle host (seven runs each,
load 0.9 to 1.0) gave 3.48 s (3.21–3.77) on the new DEF against 3.23 s
(2.93–3.31) on LibreLane's (1.08; slower in 6 of 7 pairs, by a median
0.27 s) and 4.51 s (1.40) on the campaign's; the final DEF of the factor-7
run took 3.47 s, not separated from the new one (the new one faster in 4
of 7 pairs, median paired difference 0.08 s). So OpenRCX takes 0.99 to 1.08
times LibreLane's time on the new DEF instead of 1.33 to 1.42: parity in
the shared gate within the run-to-run spread, not a lead, and the in-flow
step is still 6 % slower with ranges that do not overlap. What remains is
the extraction phase (1.13 to 1.17 times LibreLane's), offset by the faster
read of the smaller native DEF; the 23 to 30 thousand pieces more are
mostly the Metal1 escapes and the stacked accesses' Metal2 stubs described
above.

Tests: `def_tracks_declare_the_router_track_grid_and_change_nothing_else`
(`scale_flow`: the same route table, the six statements of a three-layer
grid before `COMPONENTS`, nothing else in the DEF changed),
`straightening_moves_the_net_that_walls_a_staircase_in` (a staircase
walled in by fixed metal and by another net's routed detour: without moves
it stays; with them the other net is ripped up, the staircase becomes a Z
and the other net routes straight, 4 pieces fewer, validated and
deterministic; out-of-range controls are refused),
`escape_jogs_move_the_turn_at_an_escape_top_to_its_via` (off, the escape
and the route keep their geometry; on, the escape jogs at its via and the
route keeps the rest of its run: one Metal2 piece fewer, the same vias,
deterministic), `via_reduction_with_keep_pieces_never_adds_wire_pieces`,
and `via_dedupe_and_straightening_parse_and_default_off` (the joint moves
and the escape jogs parse, default off, and an incomplete `neighbors` is
refused).

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
