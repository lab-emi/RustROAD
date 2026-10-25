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
and charged at 160 bytes per admitted node/edge, independently of the design's
output segment budget. A large design can permit many completed routes without
reserving the entire output budget for one live search tree.

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
checked before allocating coverage, labels or the flood-fill frontier.
While routing, the process' peak RSS is checked at phase boundaries and during
search and access reservation. Every search, queue, repair round and rip-up
count remains bounded. Reaching a limit returns `BUDGET_EXCEEDED` or `NO_ROUTE`
and leaves HEAD unchanged.

Route records are streamed to `routes.rbin` (at most 8 GiB); the JSON envelope
remains capped at 64 MiB. Large schema-7 revisions externalize model and prepared
state into framed, hash-checked `model.rbin` records (16 GiB file bound,
1 MiB per frame) and fixed-width `physical.rbin` (2 GiB). These storage limits
do not certify completion or quality of a full-chip flow. Full OpenC910
RTL-to-GDS qualification remains in progress; see [scaling](scaling.md).
