# Constrained routing / extraction verification resource contract

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
| `max_batch_blocked_pins` |64 |4,096 |
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


A failed pin-reservation pass collects up to `max_batch_blocked_pins` independent
blocked regions before returning. It never sends partial plans to detailed
routing. Reopening all nearby completed nets together avoids rebuilding the
routing index for each failed pin. The bounded error payload is boxed; its
collection allowance is128bytes per configured record. The caller reserves
256bytes per record for region expansion and an ordered interval index, then
scans completed obstacles once. Every query and the bounded index sort consume
the existing comparison budget. One-at-a-time diagnostics remain selectable
with a limit of1.

The grouped regression has two independent enclosed pins and two earlier nets.
Their joint legal route is retained as the witness. One allowed batch retry
must recover both blockers, produce deterministic complete geometry, and still
refuse the same case when only one failure may be collected per pass. The
rectangle-query index is checked against exhaustive overlap independently of
its ordering, including exhaustion of its work limit.


Unresolved same-net spacing pairs can also request bounded cross-batch recovery.
Their original rule descriptions remain errors; at most the existing recovery
region limit is retained. The caller may reopen nearby completed nets, then must
pass every ordinary rule and whole-design check. Non-spacing failures keep their
ordinary error path. A region without a completed net to reopen fails immediately.
Retries share the same cap as blocked-pin recovery.

Every assembly and cleanup pass now consumes the remaining comparison allowance.
Rule checks, geometry queries and area-stub searches share one cleanup work
counter. Patch and stub insertion checks the output-record limit before appending.
Repeated unsuccessful cleanup passes cannot reset either resource budget. A tiny
area-repair fixture verifies refusal at both the measured work boundary and a
full output-record budget. The actual large-core spacing case still requires a
fresh guarded run and independent physical validation.
