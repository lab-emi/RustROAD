# Scaling RustROAD beyond the 13-cell fixture

Stream W14-W16 (`feat/rtl2gds-scale`). RustROAD now floorplans, places and
routes the RustSys fixtures (43-418 logic cells, up to 1.8k placed cells with
taps and fillers), the dense six-register fixture and `pipeline.v` on
sky130_fd_sc_hd and gf180mcu_fd_sc_mcu7t5v0, and every native GDS is checked
by the pinned foundry decks: all 16 design/PDK rows pass DRC (Magic and
KLayout), XOR, antenna, Netgen and KLayout LVS, CEC (with a planted-mutation
control) and a byte-identical rerun. Scope is block level: no pad ring, no seal ring;
IR, EM, SI, OCV and CPPR are `unchecked`; density is reported separately and
never gates; timing (hold closure, multi-corner STA) runs after merge in the
unified gate and is not claimed here.

## What changed

| Area | Before | Now |
|---|---|---|
| Floorplan | hand-written JSON | `rustroad floorplan` (utilization, aspect, margin; I/O plan) |
| Placement | fixed netlist-order packing | `placer` config: analytic global placement, legalization, detailed placement (fixed order stays the default) |
| I/O | round-robin W/E/S/N in port order | optional `io_plan`: connectivity-aware sides and order, track-snapped centres |
| Router | one lattice with pin coordinates, greedy access, no rip-up, caps = defaults | `track_router` profile: per-layer tracks, conflict-aware access, GCell global routing with negotiated congestion, guide-restricted detailed routing with rip-up and reroute (lattice router unchanged and byte-identical) |
| Route storage | every lattice edge in design.json + mirror `routing.rbin` | schema 6: maximal runs in the authoritative `routes.rbin`, its sha256 and counts in design.json |
| Caps | lattice defaults are hard caps | track mode: configurable up to absolute ceilings, checked against an upfront whole-process memory estimate and a runtime RSS guard |
| Constants (sky130) | `conb_1` tie cells | rail-input ties: `inv_1` with its input routed to its own rail (no `conb_1`) |
| Spatial index | hash map of bins | dense head/tail grid per layer (same order, same results, 15-30% less peak RSS) |

Everything is `cpu_ref`: deterministic, single worker, integer DBU geometry
(the placer's solver alone uses f64 and rounds back to the site grid).

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
  x with the whitespace shared evenly.
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
  site grid, no overlap, spacing kept) runs before the result is used; taps
  and fillers are then added exactly as before.
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
2. **Pin access (conflict aware).** Each terminal searches (Dijkstra) a local
   lattice made of its landing candidates and the nearby track/stop
   coordinates of the layers up to the access layer, and stops at the first
   on-graph node of the access layer (met2 / Metal2). Candidates are the
   on-graph points inside the pin plus the quartile points, checked once per
   (master, orientation, pin, origin modulo the track period) by the
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
   escape top to the net's tree (multi-target, heuristic = distance to the
   tree's bounding box), restricted to the guide, then to the guide box plus
   `expand_margin` (4) GCells. Every wire edge and via is checked by the
   spatial index against obstacles, pins, escapes and other nets (via
   variants as in `lef_rules`); a same-net near miss costs one via. On a
   layer whose single via pads are below `AREA` (sky130 li1/met1/met3/met4,
   GF180 Metal1-4) a via may leave only after a wire run of at least
   `AREA / width` since the via that entered the layer, and a path may not
   join the tree with a via at a tree node that already has one, so no
   pad-only min-area island is created. A via taken right after a wire
   prefers the variant whose pad is elongated along that wire; after
   routing, variants are re-picked where exactly one wire ends at a via on a
   layer and the pad runs across it (LEF58 end-of-line enclosure, e.g.
   GF180 V1.3c, is not modelled natively), only when the new pad clears
   every other net without a same-net near miss. Collinear new edges become
   one spatial shape.
5. **Rip-up and reroute.** A net that fails is re-tried in a soft mode in a
   wider box: routed metal of other nets is crossable at
   `10 x via_cost x (1 + round) x (1 + rips of that net)`; nets it crosses
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
   netlist position (so placement sees the same cell order: SERV sky130
   keeps the identical HPWL; appending a new tie cell instead perturbed the
   placement enough that the repair loop ran out of budget), with several
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
   reader (the XOR reference) and OpenROAD's read it (OpenRCX on dense
   sky130 extracts the 31 signal nets and no supply net). GDS draws it
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
| J | bytes of the HEAD revision on disk (design.json and its tables); loaded metadata, its working copy and decoded tables measured 1.9-2.5 J |
| I | the routing input as built (obstacles + owner table, terminals and their shapes, access classes), counted twice for vector growth and the builder's lookup tables |
| N, G | track-graph nodes and GCells (cost u64, parent, via kind, epoch u32, history u16 per node) |
| O, T | obstacles and terminals |
| B, L | grid bins over the die and every obstacle (per layer), routing+cut layers |
| E | bin entries of the fixed obstacles, counted shape by shape without allocating |
| 20 T | shapes the router adds (escapes, routed runs and vias, ripped-up shapes that stay indexed; measured 13-16 T) |
| R | output records (wires, vias, patches, access; measured 5.2-7.0 T) |

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
(counter, regfile and mult on both PDKs, SERV on both, rv_core GF180; peak
RSS by GNU time around the binary). The final sweeps (`s8`, `x6`) confirm
the bound on every routed design, from the flow's own route step:

| design | PDK | N | T | estimate MiB | peak RSS MiB | ratio |
|---|---|---|---|---|---|---|
| counter | sky130 | 110,032 | 194 | 16 | 12.4 | 1.29 |
| counter | gf180 | 116,232 | 189 | 17 | 12.8 | 1.32 |
| uart_tx | sky130 | 191,040 | 394 | 23 | 15.9 | 1.44 |
| uart_tx | gf180 | 181,440 | 403 | 24 | 16.6 | 1.44 |
| fifo | sky130 | 191,040 | 382 | 23 | 16.4 | 1.40 |
| fifo | gf180 | 261,096 | 475 | 29 | 19.7 | 1.47 |
| alu | sky130 | 302,176 | 921 | 35 | 23.6 | 1.48 |
| alu | gf180 | 261,096 | 950 | 34 | 23.7 | 1.43 |
| regfile | sky130 | 429,120 | 1,668 | 51 | 34.1 | 1.50 |
| regfile | gf180 | 463,752 | 1,478 | 52 | 34.3 | 1.52 |
| mult | sky130 | 429,120 | 1,520 | 49 | 32.8 | 1.50 |
| mult | gf180 | 463,752 | 1,438 | 51 | 33.5 | 1.52 |
| dense | sky130 | 110,032 | 90 | 15 | 10.9 | 1.37 |
| dense | gf180 | 65,472 | 94 | 13 | 10.1 | 1.29 |
| pipeline_track | sky130 | 51,200 | 42 | 11 | 9.2 | 1.20 |
| pipeline_track | gf180 | 65,472 | 42 | 12 | 9.4 | 1.28 |
| serv | sky130 | 4,756,800 | 29,739 | 635 | 430.5 | 1.47 |
| serv | gf180 | 5,769,588 | 24,685 | 623 | 473.1 | 1.32 |
| rv_core | sky130 | 4,278,176 | 21,404 | 504 | 289.0 | 1.74 (route failed: `BUDGET_EXCEEDED`, comparisons) |
| rv_core | gf180 | 7,879,224 | 30,548 | 809 | 593.7 | 1.36 |

(rv_core sky130's estimate is from the route log; the step fails before it
writes its summary.)

The same comparison for every run of the sweep is in the evidence
(`route_memory.estimate_mib` versus the route receipt's peak RSS,
`estimate_bounds_route_rss`).

The spatial index stores bins in a dense per-layer grid (head/tail entry
index per bin, entries chained in insertion order) instead of a hash map of
vectors: iteration order and every work count are unchanged (DEF, GDS,
Verilog and `routes.json` byte-identical on counter/fifo/regfile/mult on
both PDKs and against the previous evidence), and peak RSS drops: SERV
514 -> 428 MiB (sky130), 536 -> 455 MiB (GF180), rv_core GF180 843 ->
594 MiB.

Limits in track mode may exceed the lattice defaults up to
`RouteLimits::ceiling()` (64M nodes, 8M obstacles, 64M bin entries, 8M
segments, 2e9 expansions, 2^40 comparisons, 64M heap); the grid's bins over
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
resident pages, 15-25 MiB, because Linux keeps the replaced address space's
high-water mark at exec; a step killed at its timeout is recorded as
unmeasured, not with the wrapper's numbers); the whole harness under
`systemd-run --user --scope -p MemoryMax=... -p MemorySwapMax=2G`; signoff
containers at 4 GiB.
Test binaries peak at 10-16 MiB RSS (largest: `scale_flow`, 16 MiB).

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
14000 s flow timeout (Magic extraction for LVS alone takes ~25 min);
`--pdk gf180` reruns one PDK, and the evidence can merge run directories
(`--runs A --runs B`, later PDK entries win, all from one binary):

```sh
python3 scripts/scale_evidence.py --runs .work/scale/runs/x6 \
  --design serv --design rv_core --out docs/evidence/scale-stretch.json
```

Route-step memory per phase: `RUSTROAD_ROUTE_TRACE=1` (the harness sets it)
prints `[route]`/`[troute]` lines with the estimate inputs, the estimate
and the resident set after each phase.

## Results (binary `0988dce9…`, RustSys policy producer `3de13b3a…`)

`docs/evidence/scale.json` holds every row below with hashes and receipts
(`scripts/scale_evidence.py`, run directory `s8`). Signoff image
`4c48415d8bd0` (Magic 8.3.678, Netgen 1.5.323, KLayout 0.30.9). Every
design ran twice per PDK from RTL; DEF, GDS, Verilog and powered Verilog
were byte-identical both times. Native GDS == KLayout's streamout of the
DEF (XOR 0) everywhere. All 16 rows pass every gating item.

| design | PDK | logic cells (placed incl. taps/fillers) | routed nets | Magic | KLayout | antenna | Netgen LVS | KLayout LVS | CEC (planted control) | HPWL / fixed order | route s / peak MiB / estimate MiB | flow s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| counter | sky130 | 43 (264) | 53 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.73 | 0.12 / 12.4 / 16 | 17 |
| counter | gf180 | 46 (306) | 51 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.53 | 0.08 / 12.8 / 17 | 19 |
| uart_tx | sky130 | 89 (560) | 107 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.55 | 0.26 / 15.9 / 23 | 24 |
| uart_tx | gf180 | 100 (600) | 114 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.47 | 0.16 / 16.6 / 24 | 24 |
| fifo | sky130 | 81 (481) | 102 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.44 | 0.22 / 16.4 / 23 | 26 |
| fifo | gf180 | 114 (820) | 132 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.38 | 0.18 / 19.7 / 29 | 26 |
| alu | sky130 | 253 (1094) | 272 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.54 | 0.52 / 23.6 / 35 | 46 |
| alu | gf180 | 252 (1001) | 271 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.41 | 0.42 / 23.7 / 34 | 33 |
| regfile | sky130 | 372 (1479) | 425 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.42 | 1.20 / 34.1 / 51 | 79 |
| regfile | gf180 | 358 (1737) | 387 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.38 | 1.75 / 34.3 / 52 | 44 |
| mult | sky130 | 418 (1651) | 440 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.41 | 0.74 / 32.8 / 49 | 65 |
| mult | gf180 | 414 (1793) | 436 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.41 | 0.74 / 33.5 / 51 | 42 |
| dense | sky130 | literal (258) | 33 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.61 | 0.06 / 10.9 / 15 | 13 |
| dense | gf180 | literal (160) | 35 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.62 | 0.04 / 10.1 / 13 | 16 |
| pipeline_track | sky130 | 10 (77) | 17 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.72 | 0.04 / 9.2 / 11 | 12 |
| pipeline_track | gf180 | 10 (132) | 17 | 0 | 0 | 0 | match | match | equivalent (detected) | 0.63 | 0.02 / 9.4 / 12 | 16 |

* `dense` sky130 (constants `high_bit <= 1`, `low_bit <= 0`) now gets two
  rail-tied `inv_1` instead of `conb_1`: Netgen "Final result: Circuits
  match uniquely." with no disconnected node, KLayout "Congratulations!
  Netlists match.", DRC 0/0, antenna 0, CEC equivalent. Raw logs:
  `.work/scale/runs/s8/dense/sky130/a/signoff/lvs_{netgen,klayout}/`.
* Every other row's DEF, GDS, Verilog and powered Verilog are
  byte-identical to the previous evidence (binary `a453e4ca`, run `s5`):
  the dense spatial grid and the estimate change no routing decision, and
  designs without constants do not see the tie change. (Intermediate
  sweeps `s6` and `s7`, binaries `a4f5c17a` and `8603d1e4` from the
  development of this round, gave the same bytes on all 16 rows; their run
  directories were removed after the comparison.)
* CEC control: in every row the planted dual-master swap of the final
  netlist came back `not-equivalent` on the first candidate.
* Antenna: Magic `antennacheck` (sky130) and the KLayout antenna deck
  (GF180), plus the native `check-antenna` (0 violations on every required
  design, so no antenna ECO was needed).
* Density (separate, never gating): GF180 fails the whole-block coverage
  rules (7-8 items, no fill), sky130 windowed density is `unverified`
  (block smaller than the window), as for `pipeline.v` before.
* `route` = the `route --scope all` step: wall seconds, the tool's peak RSS
  (GNU time) and the router's upfront estimate; the estimate is above the
  measured peak in every row (`estimate_bounds_route_rss`). `flow` = one PDK
  of the harness end to end: synthesis, floorplan, import, place, CTS,
  route, antenna check, exports, native GDS, reference streamout, all
  signoff checks, CEC with its control and the full second run. Host tools
  peak at 60-105 MiB (RustSys synthesis); signoff containers 440-520 MiB.
* The lattice router (`pipeline.v`, `scripts/verify_routing.py` with the
  integrated profiles, `.work/scale/legacy/vr_v16`) gives DEF `cb94ea92…`
  (sky130) and `3a07101c…` (GF180) and the same exported and powered
  Verilog as the e0d074d binary (`vr_base`): the requalified DRC 0/0 and
  LVS match carry over (status `partial` as before: post-route timing).
* `scripts/hold_closure.py` (lattice-router profiles, native extraction,
  RustSTA round-2 binaries) closes `pipeline.v` with this binary: `passed`
  on both PDKs (sky130 3 iterations, GF180 2; `.work/scale/hold_v16`).
* Memory controls (regfile GF180, route step only): `memory_mib` 9 is
  refused before the workspace is loaded ("loading 1 MiB of workspace needs
  about 11 MiB"), 40 before the router allocates ("estimate 51 MiB ...
  held 14 search 11 spatial 7 routed 9 check 9 MiB"), 60 routes (peak
  33.8 MiB). The runtime guard is unit-tested with a 1 MiB cap.
* Tap gap control: GF180 counter with a 60 um tap gap gives KLayout
  DF.13_MV 31 + DF.14_MV 26 items (Magic 0); the 23 um scale gap gives 0.
* `cargo test --workspace`: 165 passed, 4 ignored; every test binary
  peaks at 10-16 MiB RSS. `python3 -m unittest discover -s scripts/tests`:
  59 tests.

## Stretch designs (same binary)

`docs/evidence/scale-stretch.json` (run directory `x6`), behind
`flock .work/scale.lock` with a 14000 s flow timeout.

| design | PDK | logic cells (placed) | routed nets | Magic | KLayout | antenna signoff / native | Netgen LVS | KLayout LVS | CEC | rerun | HPWL / fixed order | route s / peak MiB / estimate MiB | flow s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| serv | sky130 | 7370 (24318) | 7960 | 0 | 0 | 29 / 102 | port errors | unverified | equivalent (control detected) | identical | 0.19 | 101 / 431 / 635 | 1822 |
| serv | gf180 | 6221 (26145) | 6481 | 0 | 0 | 45 / 13 | port errors | unverified | equivalent (control detected) | identical | 0.18 | 98 / 473 / 623 | 657 |
| rv_core | sky130 | 5047 (19290) | - | - | - | - | - | - | - | - | 0.24 | **failed** (371 s / 288 / 504) | - |
| rv_core | gf180 | 7915 (35042) | 8128 | 0 | 0 | 17 / 4 | port errors | unverified | **unverified** (timeout) | identical | 0.21 | 277 / 593 / 809 | 2952 (1800 CEC) |

* SERV is DRC-clean (Magic and KLayout 0, XOR 0) on both PDKs, equivalent
  to the RTL (the planted-mutation control was rejected) and identical on
  the rerun. Its sky130 netlist from RustSys has one `conb_1` (`LO` only),
  now rebuilt in place as a rail-tied `inv_1`: the four `conb_1`
  `VPB`/`VNB` nodes are gone from Netgen's report, and placement, HPWL and
  routing stay what they were with `conb_1`. Netgen still reports "Netlists
  match uniquely with port errors": SERV drives several output ports from
  one net (`o_ext_rs1[i]`/`o_dbus_adr[i]`, `o_ext_rs2[i]`/`o_dbus_dat[i]`)
  and leaves inputs unused (`i_ibus_rdt[1:0]`, `i_ext_rd`: the 70
  remaining disconnected nodes), so Magic flags shorted ports and the pin
  lists differ; the KLayout check and the port-label check are
  `unverified` because the reference netlist has aliased ports ("shorted
  ports are unsupported"). Fixing this needs a buffer per aliased output in
  synthesis or the flow, not a routing change.
* Antenna: the native check finds 102 (sky130: 47 on clock-tree nets, 55
  on signal nets) and 13 (GF180: 1 clock, 12 signal) violations.
  `antenna-eco` refuses the whole plan as soon as one violating net is a
  clock-tree net (`ANTENNA_ECO_INFEASIBLE`: not a net of the mapped
  netlist), so the signal-net violations stay unrepaired as well. Signoff:
  sky130 Magic 29 (met3 23, met4 5, met1 1), GF180 KLayout
  `ANT.16_ii_ANT.4` 45 (Magic secondary 7). GF180 density fails (7 items,
  separate, no fill).
* rv_core sky130 does not route: 19 nets fail the first detailed pass and
  the repair loop exceeds the 4e10 comparison budget (`BUDGET_EXCEEDED`,
  structured, after 371 s at 288 MiB). The harness now reports it as
  `failed` with `failed_step: sky130_a_route0` (the steps before it
  `passed`, everything after it `unverified`). Earlier untracked
  experiments on the same placement: 5x work budgets stop at "net 1700
  ripped up more than 24 times" (12 min); 5x budgets with 64 rip-ups exceed
  2e11 comparisons (18.5 min); utilization 0.4 fails earlier with one pin
  that has "no legal access after local repair". The root cause (a
  congestion hot spot the negotiation cannot spread, or a pin-access corner
  case) is not isolated.
* rv_core GF180 uses `gf180-scale-physical-wide.json` (25.2 um tap gap for
  its 24.1 um cells) and `gf180-scale-routing-large.json` (it needs 4.3e8
  expansions, above the default 4e8). It is DRC-clean (Magic 0, KLayout 0
  including the DF.13/DF.14 tap distance rules, XOR 0). Netgen reports port
  errors only (`dmem_addr_o[0]`/`[1]` share one net in the netlist); the
  native antenna check finds 4 violations, all on clock-tree nets, and
  KLayout 17 (`ANT.16_ii_ANT.4` 14, `ANT.16_ii_ANT.5` 3). `rustsys check
  synth` did not finish within the harness' 1800 s, so CEC is `unverified`
  (the control only runs after a pass); the same check of the synthesized
  netlist before floorplanning also runs past 1800 s, so this is the
  checker on rv_core's logic, not the physical flow.
* Memory: every stretch route stays below its estimate (table in "Budgets
  and memory estimate"); the dense spatial grid cut the measured peaks from
  514/536/860 MiB (previous round) to 431/473/593 MiB.

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
  its manifest uses `gf180-scale-physical-wide.json` (25.2 um), which the
  KLayout main deck accepts on the routed rv_core (0 items).
* **Antenna.** No required design needs a diode. `antenna-eco` cannot repair
  clock-tree nets and refuses the whole plan when one violating net is a
  clock-tree net, so SERV keeps its signal-net violations too (rv_core's
  native violations are all on clock-tree nets). A partial ECO (repair
  signal nets, report clock nets), layer hopping or CTS-aware diodes are
  not implemented; on GF180 the native check reports fewer violations than
  the KLayout deck has items (13 vs 45 on SERV, 4 vs 17 on rv_core).
* **Aliased output ports.** SERV and rv_core drive several output ports from
  one net (rv_core: `dmem_addr_o[0]`/`[1]`); Netgen then reports port errors
  and the KLayout LVS is `unverified`. A buffer per aliased port (synthesis
  or flow) is needed; the routing itself matches ("Netlists match uniquely").
* **Timing.** Hold closure and multi-corner timing on these designs run in
  the unified gate after merge. The CTS profile here uses clkbuf_2 with
  fan-out 8; skew/hold are not optimized by the placer or router.
* **Performance.** Detailed routing is one sequential A* per sink with
  Spatial checks per edge (`cpu_ref` only, no parallel batches yet):
  ~1-2 s for the required designs, 2-5 min for SERV/rv_core-size blocks;
  the repair loop and same-net pass are bounded but not tuned, and rv_core
  on sky130 does not converge within any budget tried (see Stretch designs).
* **Memory estimate.** The upfront estimate is calibrated (1.20-1.52x
  the measured route peak on every routed design of the final sweeps); its routed-state and
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
