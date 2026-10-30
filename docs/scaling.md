# Scaling RustROAD beyond the 13-cell fixture

Stream W14-W16 (`feat/rtl2gds-scale`) added the floorplanner, the analytic
placer and the track router used for the RustSys fixtures, the dense
six-register fixture and `pipeline.v` on sky130_fd_sc_hd and
gf180mcu_fd_sc_mcu7t5v0. Every native GDS goes through the pinned foundry
decks (Magic and KLayout DRC, XOR, antenna, Netgen and KLayout LVS), CEC with
a planted-mutation control and a byte-identical rerun. Scope is block level:
no pad ring, no seal ring; IR, EM, SI, OCV and CPPR are `unchecked`; density
is reported separately and never gates; timing (hold closure, multi-corner
STA) runs in the unified gate (`docs/rtl2gds.md`) and is not claimed here.

## Large-design implementation (unreleased)

The current branch extends storage and routing preparation for large flat
netlists. Full OpenC910 RTL-to-GDS qualification remains in progress; these
implementation changes are not an end-to-end speedup claim.

- Verilog is read through a bounded streaming lexer. The input byte, token,
  instance, pin and net limits are explicit; the ordinary CLI defaults remain
  small. `--max-input-mib` admits a larger file without retaining its source or
  a complete token vector alongside the netlist.
- Large revisions use workspace schema 7 and hash-bound `model.rbin` frames.
  Encoding `RROADB02` stores cells, netlist/connectivity records, clock sinks,
  tap sites and prepared geometry outside the JSON envelope. Readers also
  accept `RROADB01`. Every frame and array count is checked before decoding;
  physical and route tables are written and checked incrementally.
- Immutable placement records are constructed without a duplicate mutable
  string-interning database. The global placer parallelizes independent
  matrix rows while retaining arithmetic order within each row and reduction.
  Detailed placement uses indexed local intervals and gaps; high-fanout nets
  follow the same explicit cutoff in global and detailed optimization.
- Routing preparation retains cell geometry once per master, with instance
  positions/orientations and a compact pin-to-net column. Cell bounding boxes
  are indexed in pages; pin and obstruction rectangles are translated only
  when queried. Native RC extraction also uses shared shapes and global pin identities,
  generating local conductor contributions without flattening the geometry.
  The explicit path remains available to other consumers. Full-chip extraction
  qualification remains separate from these storage changes.
- Large routing grids use bounded sparse search state. Spatial bins allocate
  32 by 32 pages on populated large layouts. Fixed rectangles are shared;
  completed nets retain collinear runs rather than every track edge. Clock
  membership uses an indexed bit mask, and supply-net IDs are resolved once
  for the entire input build.
- The detailed frontier uses indexed priority updates, with one pending entry
  per node. Queue growth is independent of how often a better path reaches
  that node; its memory allowance is included in the route preflight.
- Target distance uses an exact bounded segment index, including layer changes
  and directional costs. Exceeding `max_target_segments` fails explicitly;
  large trees never silently collapse to one bounding box. Rip-up penalties
  use the displaced net's history once, without a multiplier from unrelated
  repairs or the current net's attempts.
- Bounded preferred-track spans reduce search through open channels while
  retaining ordinary moves, full-wire collision checks and intermediate guide
  checks. Large sparse hole checks use a bounded sweep rather than allocating
  the Cartesian product of x/y coordinates.
- `track_router.max_history_entries` independently bounds congestion history
  (default 4,000,000). Pin-access memoization has a bounded memory cache;
  eviction recomputes the same geometry. Object/work limits, the whole-process
  memory estimate and the runtime RSS guard remain active. Resource limits
  are independent of geometric and timing acceptance checks.
- After decoding, the routing estimate uses resident plus swapped process
  bytes with 25% headroom, while future search, index, route and checking
  allocations stay separately budgeted. Pre-load and unavailable-probe paths
  retain the conservative serialized-size estimate.
- Access tables share the exact relative landing geometry. Local escape repair
  queries a bounded interval index and keeps the original ascending-pin repair
  order. Both construction and candidate scans count toward the work cap.
- Optional `port_buffer` roles isolate output aliases and feedthroughs. SPEF
  formatting generates one labeled net at a time; it does not copy the complete
  extracted network into a second string-rich representation.
- Dead-end pruning and endpoint connectivity use compact source indices for
  the full design and build only one net's graph at a time. Per-net edge,
  endpoint and work limits are checked before graph allocation. Cycles and
  disconnected access points retain their previous validation semantics.
- Same-net pin-metal queries use a bounded spatial index. Equal-cost global
  searches prefer progress toward their target. Exhausted hard searches can
  use the existing bounded wider-guide and rip-up attempts; soft-search and
  aggregate budgets still fail explicitly.
- An explicit `batch_terminals` routing profile bounds the active whole-net
  group. Completed metal becomes fixed geometry for later groups; no net is
  split or omitted. The combined result passes the full-design validator
  before publication. Global work/output limits remain cumulative. Congestion
  that cannot be resolved within a group fails explicitly; earlier groups are
  not silently discarded. This mode still needs full-core qualification.
- Constant fanout repair covers inserted and in-place converted tie sources,
  including top-level output aliases. Mixed signal/constant buses preserve
  their declared bit order. Independent Yosys equivalence and wrong-output
  controls cover both baseline PDKs.
- Optional [primary-input trees](input-buffering.md) bound nonclock input
  fanout before placement, with explicit buffer, sink, work and memory limits.
  They use a fixed floorplan; automatic sizing refuses the policy explicitly.
- Independent GDS metadata counts use bounded hierarchy dynamic programming.
  Complete XOR label lists are sorted through bounded disk chunks and merges,
  retaining exact hashes and negative controls without a full label vector.

## What changed

| Area | Before | Now |
|---|---|---|
| Floorplan | hand-written JSON | `rustroad floorplan` (utilization, aspect, margin; I/O plan) |
| Placement | fixed netlist-order packing | `placer` config: analytic global placement, legalization, detailed placement (fixed order stays the default) |
| I/O | round-robin W/E/S/N in port order | optional `io_plan`: connectivity-aware sides and order, track-snapped centres, optional along-side targets |
| Router | one lattice with pin coordinates, greedy access, no rip-up, caps = defaults | `track_router` profile: per-layer tracks, conflict-aware access, GCell global routing with negotiated congestion, guide-restricted detailed routing with rip-up and reroute (lattice router unchanged and byte-identical) |
| Route storage | every lattice edge in design.json + mirror `routing.rbin` | schema 6: maximal runs in the authoritative `routes.rbin`, its sha256 and counts in design.json |
| Caps | lattice defaults are hard caps | track mode: configurable up to absolute ceilings, checked against an upfront whole-process memory estimate and a runtime RSS guard |
| Constants (sky130) | `conb_1` tie cells | rail-input ties: `inv_1` with its input routed to its own rail (no `conb_1`) |
| Spatial index | hash map of bins | dense head/tail grid per layer (same order, same results) |

These engines use deterministic CPU reference implementations and integer DBU
geometry. The placer can parallelize independent matrix rows; the router is
single-worker. The placer's solver uses f64 and rounds back to the site grid.

## Floorplan (`rustroad floorplan`)

File-only command (no workspace). Inputs: technology/cell LEF, mapped
netlist, physical profile, routing profile, site, `--utilization`
(default 0.5), `--aspect` (core height/width, 1.0), `--margin-um` (10).
Outputs: a floorplan JSON for `import-pdk` and a physical configuration
(profile + `io_width` + `io_plan` + `placer`), written atomically; a report
on stdout.

1. Tie cells are materialized exactly as import does; clock buffers are
   estimated with the CTS grouping arithmetic (`max_fanout`).
2. Rows are `tap + k (tap + clear)` long, `clear` = the tap profile's maximum
   clear gap rounded to sites, so the tap reservation lands exactly on its
   largest gap. A cell wider than `clear` fails with `FLOORPLAN_TAP_GAP`.
3. `k` grows until `rows * k * clear * utilization >= cell area` (rows follow
   the aspect ratio) and the die perimeter holds 1.25x the I/O pins.
4. Core and die edges sit on the site and row grids, so pins keep their
   track alignment.
5. I/O pins: square, at least the I/O layer `AREA`, and at least
   `max(I/O track pitch, largest gap between perpendicular stops) + wire
   width`, so a default-width landing always fits inside the pin (a wire that
   only partly covers a pin edge makes a sub-width neck).
6. I/O order: a bounded analytic placement of the netlist with round-robin
   pins gives the centroid of the cells each pin connects to; pins take the
   nearest side with capacity (strongest preference first) and are ordered
   by centroid along it; the placement is repeated once with that plan.
   `place_planned_io` spreads them over a slot lattice whose centres are on
   the side's track (`x_track`: the I/O layer's perpendicular neighbour,
   `y_track`: the I/O layer).
7. An `io_plan` may also carry `targets`: a pin-centre coordinate along the
   side (y on west/east, x on south/north) per pin. A targeted pin takes the
   lattice slot nearest its target. A forward pass keeps plan order and one
   pin per slot; a backward pass leaves room for the pins after it. Untargeted
   pins keep their evenly spread slot, so a plan without targets places
   exactly as before. An even spread puts a side's first pin at its corner,
   however far its loads are. The Ibex recipe derives targets from a
   pin-planning placement: run14's corner `clk_i` and `rst_ni` nets broke the
   1 ps timing parity gate and the reset input's max capacitance.

## Placement (`placer` in the physical configuration)

`rustroad_core::place` (`PlaceConfig`: global iterations 12, CG iterations
150, detailed passes 3, nets above 256 pins ignored by the model):

* **Global:** bound-to-bound quadratic wirelength over cell centres with the
  I/O pin centres fixed, solved per axis with Jacobi-preconditioned
  conjugate gradient; three plain solves, then 12 rounds of spreading plus
  re-solve with pseudo-net anchors whose weight grows quadratically.
* **Spreading:** recursive bisection over the free row intervals between
  reserved taps: the region is cut at its middle (columns when wider than
  tall), cells sorted along the cut are split so each side's area matches
  its capacity share, down to single-row segments where cells are spread in
  x with the whitespace shared evenly. Whole-cell rounding can over-fill a
  region; a side without capacity (covered by a macro) never receives cells,
  the overflow stays on the side that has rows and legalization resolves it.
* **Legalization:** widest cells first, every cell takes the interval of
  least estimated displacement (plus a fill penalty) that still has room;
  inside each interval an order-preserving cluster solve (overlapping
  neighbours merge and sit at their mean target, clamped and snapped to the
  site grid) places them. Capacity counts widths plus `cell_spacing_sites`,
  so every assignment fits.
* **Detailed:** bounded passes of equal-width neighbour swaps and moves into
  the best free gap near the median of the cell's nets, accepted only when
  HPWL drops.
* **Check:** an independent legality check (inside a free interval, on the
  site grid, no overlap, spacing kept between neighbours of one free
  interval; across a tap the tap itself separates the cells) runs before the
  result is used; taps and fillers are then added exactly as before.
* `place` and `clock-tree` both use it; `inspect`/command summaries carry a
  `placement` report: HPWL (centre to centre, DBU) of the result, of the
  global placement and of fixed-order packing of the same cells into the same
  intervals (`baseline_hpwl_dbu`).
* **Timing-driven weights** (`place --net-weights`, `rustroad.net-weights.v1`):
  integer per-net weights (1000 = 1.0, at most 16,000) scale the springs and
  the detailed-placement HPWL cost; unit weights are bit-identical to the
  unweighted placer (`net_weights` in the report: applied, unmatched,
  weighted HPWL).
* **Seeded ECO placement** (`place --seed`, `rustroad.placement-seed.v1`):
  seeded cells go straight to legalization from their seed, new cells start
  at the mean of their seeded/fixed net neighbours; no global solve
  (`seed` in the report: seeded/unseeded/unmatched, displacement). Weights
  and seed are stored with the physical bundle, so the CTS re-placement uses
  them too. The closure loop uses both (`docs/closure.md`).

## Router (`track_router` in the routing profile; needs `lef_rules`)

`track_router.min_routing_layer` and `max_routing_layer` optionally restrict
global capacity and detailed signal wires/vias by selected LEF layer name.
The access layer must lie inside that range. Lower layers remain available
for fixed pin escapes; for example, selecting Metal2 through Metal5 keeps
Metal1 cell-pin access while reserving TopMetal for the supply grid. Omitting
both fields retains the previous behavior for existing profiles. Invalid,
reversed or cut-layer limits are rejected.

1. **Track graph.** Every routing layer has its own tracks from the LEF
   `PITCH`/`OFFSET` (cross axis), replaced by profile `tracks` overrides.
   Along a track the nodes are the tracks of the via-paired perpendicular
   layers ("stops"), so every via lands on a node of both layers. Moves:
   along a track (cost = length), to a neighbouring track at the same stop
   (cost = length x `wrong_way_factor`, 4) and vias (`via_cost`). Node ids are
   dense per layer; no per-edge storage.
   * Sky130: li1/met1/met2/met3/met4 from the LEF (met3 and met4 are offset
     by half a pitch from met1/met2, which is why one shared lattice cannot
     work).
   * GF180: Metal1..Metal4 at 0.56 um; the LEF says `OFFSET 0`, but the 7t
     cells put their pins on x = 0.28 + 0.56k um: with the LEF offset 500 of
     608 used pin/orientation pairs have no on-track landing, so the profile
     shifts the vertical layers (Metal2, Metal4) by 0.28 um.
2. **Pin access (conflict aware).** Each terminal searches (A*) a local
   lattice made of its landing candidates and the nearby track/stop
   coordinates of the layers up to the access layer, and stops at the first
   on-graph node of the access layer (met2 / Metal2). The lower bound is the
   distance to that layer's track grid plus the minimum via-hop cost; the
   small layer-hop table is built once. Candidates are the
   on-graph points inside the pin plus the quartile points, checked once per
   (master, orientation, pin, exact relative landing points) by the
   existing LEF-rule access table (other-net blockers, same-pin notches,
   MINSTEP, pads inside the pin first); escapes keep the via-variant and
   same-pin checks. Terminals with the fewest candidates go first. A terminal
   with no legal escape rips up the escapes of terminals in its window, takes
   its own first and re-queues them (at most 3 repairs per terminal).
3. **Global routing.** GCells of `gcell` DBU (one row height). Edge capacity
   = tracks crossing the boundary that are free of fixed geometry, times
   `capacity_percent` (80). Nets (pins = escape tops) are routed tree-wise by
   A* on the GCell grid; for `global_iterations` (20) rounds every net using
   an overflowed edge is ripped up and re-routed with cost
   `(1 + history/2 + 4 * round * overflow)`, history growing on overflowed
   edges (negotiated congestion, clean-room). The dilated GCell set of each
   net (`guide_margin` 1) is its guide.
4. **Detailed routing.** Nets in order of guide size. Each sink: A* from its
   escape top to the net's tree. A bounded spatial index finds the distance
   to actual tree segments, including unavoidable layer changes and directional
   costs. `heuristic_weight` defaults to one; explicit values up to sixteen
   can favor progress without changing geometric checks or resource limits.
   Search is restricted to the guide, then to the guide box plus
   `expand_margin` (4) GCells. Every wire edge and via is checked by the
   spatial index against obstacles, pins, escapes and other nets (via
   variants as in `lef_rules`); a same-net near miss costs one via. On a
   layer with `AREA` constraints, initial routes defer path-dependent area
   checks to bounded geometric repair. Strict retries accept actual via-pad
   area certificates or require a sufficient intervening wire run. Final
   area validation is always required. A via taken right after a wire
   prefers the variant whose pad is elongated along that wire; after
   routing, variants are re-picked where exactly one wire ends at a via on a
   layer and the pad runs across it (LEF58 end-of-line enclosure, e.g.
   GF180 V1.3c, is not modelled natively), only when the new pad clears
   every other net without a same-net near miss. Collinear new edges become
   one spatial shape.
5. **Rip-up and reroute.** A net that fails is re-tried in a soft mode in a
   wider box: routed metal of other nets is crossable at
   `10 x via_cost x (1 + rip-ups of the crossed net)`; nets it crosses
   are ripped up and queued; contested nodes gain history cost for every
   later search. Bounded by `max_ripups_per_net` (24) and
   `max_repair_rounds` (200k); fixed geometry (pins, obstructions, supply,
   escapes) is never crossed (`NO_ROUTE` names the pin and net).
6. **Output and same-net pass.** Escapes and routed edges become wires merged
   into maximal collinear runs split at vias, access points and branches
   (`wire_runs`), then the existing bounded same-net pass prunes dead ends,
   patches notches and grows min-area stubs. In track mode a near pair
   whose facing span is narrower than the layer width (or a corner pair) is
   filled with the bounding box of the parts of both shapes within two
   widths of each other, which contains both facing corners (the legacy
   bridge overlapped by a few DBU and its widened gap fill left steps:
   `met1.1`/`M3.1` necks), and
   holes enclosed by same-net metal smaller than 4x `AREA` (sky130 `m1.7`,
   `MINENCLOSEDAREA`) are reported and filled. Nets the pass cannot fix are
   ripped up and re-routed in strict mode (no near misses), up to four
   passes; then `ROUTING_RULE` fails the command and HEAD stays unchanged.
   `validate()` runs on the result, with the same telemetry checks as before
   (track-graph node count instead of the lattice size).

The lattice router, its caps and its output are unchanged: profiles without
`track_router` give byte-identical DEF, design.json (except input paths) and
powered Verilog to the binary before this stream (checked on `pipeline.v`
for both PDKs with `scripts/verify_routing.py`).

## Constants: rail-input ties (sky130)

Sky130's only tie cell, `sky130_fd_sc_hd__conb_1`, is two resistors: its
`VPB`/`VNB` pins have no device (Netgen lists them as disconnected nodes,
which the frozen harness rejects) and the pinned KLayout deck extracts it as
HI/LO shorted to the rails (`NoMatch`). Neither deck nor status logic is
changed here; the flow stops using `conb_1` instead. A tie profile entry may
name a logic cell plus a `rail_input`:

```json
"tie_high": {"cell": "sky130_fd_sc_hd__inv_1", "pin": "Y", "rail_input": {"pin": "A", "supply": "ground"}},
"tie_low":  {"cell": "sky130_fd_sc_hd__inv_1", "pin": "Y", "rail_input": {"pin": "A", "supply": "power"}}
```

(`fixtures/pdk/sky130-scale-physical.json`; GF180 keeps `tieh`/`tiel`, which
both LVS checks accept.)

1. **Netlist.** RustSys maps constants on sky130 to `conb_1` itself (one
   instance, `LO` only, in SERV and rv_core); the profile's
   `constant_cells` (`{"cell": "sky130_fd_sc_hd__conb_1", "high": ["HI"],
   "low": ["LO"]}`) makes the import and `floorplan` rebuild each such
   instance first (`dissolved_cells` in the tie report): with one connected
   output it becomes the tie master for that value at the same name and
   netlist position (so placement sees the same cell order), with several
   connected outputs it becomes literal assignments. Every literal constant
   bit then gets its own tie instance as before (`ties.rs`); a tie
   master's rail input is connected to the reserved wire
   `rr_tie_rail_ground`/`rr_tie_rail_power`. An instance of the tie master
   whose rail pin already carries the matching literal (a re-imported
   export) is tied again rather than given a second tie cell. The reserved
   names are refused in input netlists.
2. **Connectivity.** The import folds the reserved wires into the supply
   nets: the tie input is a cell input on `VSS`/`VDD`. `ConnectedDesign`
   accepts cell inputs on supply nets (outputs and ports still fail).
3. **Routing.** In track mode each tie input becomes its own router net
   (id after the logical nets) with two terminals: the input pin (its
   access classes, via variants and patches as for any pin) and a
   pseudo-driver that is the instance's own rail rectangle on the rail
   layer (met1). `RouteInput::ties` declares the pairing; the spatial index
   treats supply shapes as the tie's own metal only where the tie's shape
   overlaps that zone and touches them (a near miss to supply metal still
   blocks, so no notch can form between the two groups). The lattice router
   refuses ties. `validate()` checks them like any net.
4. **Exports.** Logical Verilog writes the input as `1'b0`/`1'b1` (CEC, STA:
   RustSTA reports the inverter as a constant driver); the powered netlist
   writes `VSS`/`VDD`. DEF writes the tie wiring in a `NETS` entry of the
   supply net (which keeps its `SPECIALNETS` entry and pins); KLayout's DEF
   reader (the XOR reference) and OpenROAD's read it (OpenRCX extracts the
   signal nets and no supply net). GDS draws it
   with the supply net. `extract` treats tie metal as grounded passive
   conductors (coupling to signal nets is kept, no SPEF net); the native
   `check-antenna` skips tie nets and says so
   (`supply_tie_nets_unchecked`); the foundry antenna decks check them.

Rule 8: the Boolean value of a tie is never trusted from the profile:
`rustsys check synth` checks the final netlist (with the inverters and
their literal inputs) against the RTL.

## Storage (schema 6)

A workspace routed with `track_router` is committed as `format_version` 6:
design.json keeps the routing state with empty record lists and the stats,
plus `table = {file: "routes.rbin", sha256, bytes, wires, vias, access,
patches, nets}`. `routes.rbin` holds 64-byte i64 records (kinds 1-4 as in
`routing.rbin`, plus kind 5 = routed net), at most 512 MiB. Loading reads
it, checks length, sha256, record order and counts, and runs the full
routing validation. Schemas 1-5 load as before (`routing.rbin` stays a
mirror there). `Routes.wires` keeps its meaning (axis-parallel segments of
any length), so `export-def`, `export-gds`, `extract` and `check-antenna`
work unchanged. A tampered or truncated `routes.rbin` is rejected
(`tests/scale_flow.rs`).

## Budgets and memory estimate (Rule 9)

Track router, one corner, one worker. `route` refuses a run whose
whole-process estimate exceeds `track_router.memory_mib` (default 2048):

    total = held + search + spatial + routed + check            (bytes)
    held    = 8 MiB + 3 J + 2 I
    search  = 24 N + 48 G
    spatial = index(20 T)
    routed  = 4096 T + 8 N
    check   = index(2 R) + 128 R,      R = 10 T
    index(k) = 8 B L + 64 (O + k) + 16 (E + 4 k)

| symbol | meaning |
|---|---|
| J | bytes of the HEAD revision on disk (design.json and its tables); `3 J` covers the loaded metadata, its working copy and decoded tables |
| I | the routing input as built (obstacles + owner table, terminals and their shapes, access classes), counted twice for vector growth and the builder's lookup tables |
| N, G | track-graph nodes and GCells (cost u64, parent, via kind, epoch u32, history u16 per node) |
| O, T | obstacles and terminals |
| B, L | grid bins over the die and every obstacle (per layer), routing+cut layers |
| E | bin entries of the fixed obstacles, counted shape by shape without allocating |
| 20 T | shapes the router adds (escapes, routed runs and vias, ripped-up shapes that stay indexed) |
| R | output records (wires, vias, patches, access) |

`check` is the rule-repair / validation index over the fixed shapes and the
result (alive together with the router) plus the same-net groups and the
output records. The estimate is enforced twice before allocation: the
`held` load part alone before the workspace is read, and the total after
the input is built (`BUDGET_EXCEEDED`, the message lists the parts). While
routing, the router compares the process' peak RSS (`VmHWM`, Linux) with
`memory_mib` at every phase boundary and every 65536 expansions and stops
with `BUDGET_EXCEEDED` if it is above (where procfs is unreadable the guard
is inactive and only the estimate applies). The route command's JSON
reports `route_memory` (estimate, cap and parts, MiB).

The coefficients were fitted on route-step re-runs of finished workspaces
against the route step's own peak RSS (GNU time around the binary). Every
route receipt records `route_memory.estimate_mib` next to the measured peak
(`estimate_bounds_route_rss`); the sweep receipts are historical evidence
kept for reproducibility, not current benchmarks.

The spatial index stores bins in a dense per-layer grid (head/tail entry
index per bin, entries chained in insertion order) instead of a hash map of
vectors: iteration order and every work count are unchanged (DEF, GDS,
Verilog and `routes.json` byte-identical on counter/fifo/regfile/mult on
both PDKs and against the previous evidence).

Limits in track mode may exceed the lattice defaults up to
`RouteLimits::ceiling()` (4,294,967,294 implicit nodes, 256M retained obstacle
records, 512M bin entries, 128M segments, 2e9 expansions, 2^40 comparisons,
64M heap entries); the grid's bins over
all layers count against `max_bin_entries`. The scale profiles use 8M nodes,
4M obstacles, 32M bin entries, 2M segments, 4e8 expansions, 4e6 per search,
4e10 comparisons (`gf180-scale-routing-large.json`, used by rv_core only:
2e9 expansions and 2e11 comparisons, same memory caps). Every search counts
expansions per search and in total; spatial queries count comparisons;
every queue is capped.

Placer: O(cells + pins): positions and targets (32 B/cell), the B2B system
(at most 2(2p-3) entries per p-pin net and axis, 16 B each) and the CSR copy;
work (CG multiply-adds, spreading and legalization steps) counted against
`max_work` (2e10). Floorplan: two placements of the same size.

Harness: host tools run under `prlimit --as` 8 GiB and GNU time (their own
peak RSS; `wait4` on a child of the Python harness reports the interpreter's
resident pages, because Linux keeps the replaced address space's
high-water mark at exec; a step killed at its timeout is recorded as
unmeasured, not with the wrapper's numbers); the whole harness under
`systemd-run --user --scope -p MemoryMax=... -p MemorySwapMax=2G`; signoff
containers at 4 GiB.

## Verification

`scripts/scale_flow.py --design fixtures/designs/<name>.json` runs, per PDK:
tracked-input hashes; RustSys synthesis with the cell-policy arguments (or
the manifest's literal netlist); the escaped-name rename; `floorplan`
regeneration that must be byte-identical to the tracked floorplan and
physical configuration; import, place, CTS (clocked designs), route; the
native `check-antenna` (and `antenna-eco` plus a re-run if it reports
violations); DEF/Verilog/powered Verilog/native GDS export; the signoff
harness on the native GDS with KLayout's DEF streamout as XOR reference
(Magic `drc(full)`, KLayout main deck, antenna, density, Netgen and KLayout
LVS against the powered Verilog); `rustsys check synth` RTL versus the final
netlist; and a second complete run in a fresh directory that must give
byte-identical DEF, GDS, Verilog and powered Verilog. Statuses come from
parsed reports, never from exit codes; `failed` beats `unverified` beats
`passed`.

* **CEC verdicts.** `rustsys check synth` reports `equivalent`,
  `initial-state-refinement`, `not-equivalent`, `undecided` or `error`.
  `equivalent` without mismatches passes; `not-equivalent`, or any other
  verdict that carries a mismatch with a counterexample, fails; the rest
  (and timeouts) are `unverified`. A passing check is followed by a
  planted-mutation control: the first instance whose master has a dual
  with identical Liberty pins (AND2<->OR2, NAND2<->NOR2, same drive) is
  swapped in a copy of the final netlist and the same check must return
  `not-equivalent` (up to three candidates, in case a gate is redundant);
  otherwise the CEC item is `unverified`. `--no-cec-control` also makes it
  `unverified`.
* **Build-step failures.** A failing step marks its own gating item
  `failed` (`synth`; `floorplan` -> `floorplan_reproduced`;
  `import`/`place`/`clock-tree` -> `place`; `route`/`antenna` -> `route`;
  exports -> `export`), keeps the verdicts of the steps that finished, marks
  the rest `unverified` (not run) and the PDK `failed` (exit 1), with the
  receipt label as `failed_step`.

Design manifests (`fixtures/designs/<name>.json`, schema
`rustroad.design.v2`, the RTL-to-GDS gate's single schema, see
docs/rtl2gds.md) pin the RTL, the clock SDC, the I/O SDC template, the
profiles, the generated floorplan and physical configuration per PDK, the
frozen PDK inputs (cell GDS/LEF, technology LEF, Liberty and OpenRCX rules
per corner, RC profiles) and `recipe.floorplan` (utilization 0.5, aspect 1,
margin 10 um; GF180 overrides the utilization with 0.35 in
`pdks.gf180.floorplan_recipe` since final2, room for its closure ECOs) with
`recipe.physical: track`. `scripts/scale_manifests.py`
regenerates them (`--refresh-manifest` re-reads the hashes only). Since the
final round every track design also goes through `scripts/rtl2gds.py`
(timing, closure, controls); the GF180 scale clock buffer is now clkbuf_8.

## Reproduction

```sh
export PATH=/home/cgao/.cargo/bin:$PATH; free -h
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  env CARGO_BUILD_JOBS=4 cargo build --release
# manifests (only when RTL, profiles or the floorplan code change)
python3 scripts/scale_manifests.py --rustroad target/release/rustroad \
  --rustsys <RustROAD>/.work/tools/rustsys-target/release/rustsys
# one design, both PDKs (signoff image 4c48415d8bd0 via podman)
systemd-run --user --scope -p MemoryMax=12G -p MemorySwapMax=2G \
  /usr/bin/time -v timeout 5400 python3 scripts/scale_flow.py \
  --design fixtures/designs/regfile.json --rustroad target/release/rustroad \
  --rustsys <RustROAD>/.work/tools/rustsys-target/release/rustsys --out .work/scale/runs/regfile
python3 scripts/scale_evidence.py --runs .work/scale/runs --out docs/evidence/scale.json
```

Stretch designs (SERV, rv_core) run behind `flock .work/scale.lock` with a
14000 s flow timeout;
`--pdk gf180` reruns one PDK, and the evidence can merge run directories
(`--runs A --runs B`, later PDK entries win, all from one binary):

```sh
python3 scripts/scale_evidence.py --runs .work/scale/runs/x6 \
  --design serv --design rv_core --out docs/evidence/scale-stretch.json
```

Route-step memory per phase: `RUSTROAD_ROUTE_TRACE=1` (the harness sets it)
prints `[route]`/`[troute]` lines with the estimate inputs, the estimate
and the resident set after each phase.

## Controls and receipts

The harness controls above (CEC planted mutation, build-step failure
reporting, byte-identical rerun) run on every row. Two further controls
exercise the budgets and the tap profile:

* Memory controls (route step only): a `memory_mib` below the load estimate
  is refused before the workspace is loaded, one below the total estimate is
  refused before the router allocates (the message lists the parts), and the
  runtime guard is unit-tested with a 1 MiB cap.
* Tap gap control: GF180 counter with a 60 um tap gap must fail the KLayout
  DF.13_MV/DF.14_MV tap distance rules, which the 23 um scale gap passes.

The sweep receipts (`docs/evidence/scale.json`,
`docs/evidence/scale-stretch.json`) are historical evidence kept for
reproducibility, not current benchmarks.

## Limits and open issues

* **Sky130 constants (policy for the gate owners).** The flow ties each
  constant through an `inv_1` whose input is wired to its own rail; the
  consumers' gates never touch a rail, but each tie inverter's gate does
  (sky130_fd_sc_hd has no tie cell other than `conb_1`, which the pinned
  LVS decks reject). DRC, antenna and both LVS checks pass; whether a
  gate-on-rail tie is acceptable for the target's ESD/reliability rules is
  not checked by any deck here. GF180 keeps `tieh`/`tiel`.
* **GF180 tap gap.** The scale physical profile allows a 23 um clear tap gap
  (the 20 um of `gf180-routed-physical.json` cannot hold the 20.2-22.4 um
  flip-flops and AOI cells RustSys maps to); the KLayout DF.13/DF.14 tap
  distance rules pass at 23 um and fail at 60 um. rv_core needs a 24.1 um
  cell, which `floorplan` refuses (`FLOORPLAN_TAP_GAP`) with that profile;
  its manifest uses `gf180-scale-physical-wide.json` (25.2 um).
* **Antenna.** `antenna-eco` cannot repair clock-tree nets and refuses the
  whole plan when one violating net is a clock-tree net, so signal-net
  violations of the same design stay unrepaired too. A partial ECO (repair
  signal nets, report clock nets) or CTS-aware diodes are not implemented;
  diode-free layer jumpers are described in `docs/hold-closure.md`. On GF180
  the native check can report fewer violations than the KLayout deck has
  items; the deck decides.
* **Aliased output ports.** SERV and rv_core drive several output ports from
  one net (rv_core: `dmem_addr_o[0]`/`[1]`); Netgen then reports port errors
  and the KLayout LVS is `unverified`. A buffer per aliased port (synthesis
  or flow) is needed; the routing itself matches ("Netlists match uniquely").
* **Timing.** Hold closure and multi-corner timing on these designs run in
  the unified gate after merge. The CTS profile here uses clkbuf_2 with
  fan-out 8; skew/hold are not optimized by the placer or router.
* **Performance.** Detailed routing is one sequential A* per sink with
  spatial checks per edge (`cpu_ref` only, no parallel batches yet); the
  repair loop and same-net pass are bounded but not tuned.
* **Memory estimate.** The upfront estimate is calibrated against measured
  route peaks; its routed-state and
  record terms are per-terminal averages, so a pathological net mix could
  exceed it. The runtime guard (peak RSS against `memory_mib`) needs Linux
  procfs; elsewhere only the estimate applies, so the harness keeps its
  cgroup cap.
* **Extraction granularity.** Wires are stored as maximal runs split at
  junctions, so `extract` sees no interior nodes inside a straight run (the
  Elmore delay of a straight segment is unchanged; higher-order slew effects
  of very long runs are not split out).
* **Escaped names.** RustSys' `\p_q[0]` next to port `p_q` is renamed by the
  harness, not by RustROAD's importer, which still refuses the ambiguity.

The physical side-table format admits up to 32 million instances, including fillers, taps and clock buffers. Commands still require an explicit sufficient cell capacity and obey the 16 GiB table and 1 MiB record limits; raising a capacity never allocates absent rows.

The physical instance ceiling is shared by stored tables, retained-netlist validation, instance geometry, and extraction indices; per-command limits remain independent and may be lower.

Track profiles may set `heuristic_weight` from 1 to 16 (default 1). It scales
the detailed search's distance estimate when ordering pending nodes. Higher
values favor progress toward the target and may produce a longer route; they
do not change wire/via costs, collision checks, expansion limits or final
validation. Priority arithmetic saturates at `u64::MAX`. Qualify the chosen
weight on both PDKs and report measured routed quality along with run time.

Congestion repair starts from an internal driver when present, orders the
other internal escapes from their coordinate median outward, and attaches
I/O ports last when the net has internal pins.
A remote I/O driver therefore need not be the first geometric root
of the undirected route tree. Logical driver identities are unchanged. The
ordering reuses each net's terminal slice, takes bounded `O(N log N)` work,
and is charged to the comparison budget before sorting.

A soft search that reaches its per-search limit releases its partial metal
and may retry once from an alternate terminal root. Each attempt keeps the
same limit and both consume the existing aggregate work/expansion budget.
Exhausting both attempts fails explicitly; no partial result is published.

Detailed search also offers the track stops bracketing the target coordinate
as checked macro-wire endpoints. They use the same complete-span obstacle,
guide, history and topology checks as other long moves, then expand to bounded
unit edges for the stored route. To limit repeated full-span queries in dense
layouts, probes are sampled after the first 16 expansions and capped at 1,024
per sink. Unit moves remain available for detours.

Via junctions use the area of the actual via pads. A smaller unused variant
does not force a wire run when an incident pad already meets the layer's
minimum area. The conservative minimum-run restriction is used on strict
reroutes. Initial routes defer this path-dependent restriction to bounded
geometric repair, which adds area stubs before final validation; unresolved
violations trigger strict rerouting or an explicit failure. The pad table
has a one-million-entry cap, is included in memory admission, and is checked
before search; final geometry validation still checks the complete union.

Pin access reserves a clear upward via envelope before accepting a landing
below the access layer. If that exit is blocked, the bounded local search
continues to another landing. These envelopes protect subsequent access and
detailed routing, but are excluded from global conductor-capacity estimates
and from exported geometry. Their shape/index storage is included in memory
admission and released with the owning escape plan.
