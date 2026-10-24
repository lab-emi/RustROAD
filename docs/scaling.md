# Scaling RustROAD beyond the 13-cell fixture

Stream W14-W16 (`feat/rtl2gds-scale`). RustROAD now floorplans, places and
routes the RustSys fixtures (43-418 logic cells, up to 1.8k placed cells with
taps and fillers), the dense six-register fixture and `pipeline.v` on
sky130_fd_sc_hd and gf180mcu_fd_sc_mcu7t5v0, and every native GDS is checked
by the pinned foundry decks. Scope is block level: no pad ring, no seal ring;
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
| Caps | lattice defaults are hard caps | track mode: configurable up to absolute ceilings, checked against an upfront memory estimate |

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
   variants as in `lef_rules`); a same-net near miss costs one via. A via
   may not follow a via through a layer whose single pads are below `AREA`
   (sky130 met3, GF180 Metal2/3), so stacks never leave a min-area island.
   Collinear new edges become one spatial shape.
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
   patches notches and grows min-area stubs. In track mode the corner
   patch overlaps each shape by at least the layer width in both axes (the
   legacy bridge overlapped by a few DBU and made `met1.1`/`M3.1` necks), and
   holes enclosed by same-net metal smaller than 4x `AREA` (sky130 `m1.7`,
   `MINENCLOSEDAREA`) are reported and filled. Nets the pass cannot fix are
   ripped up and re-routed in strict mode (no near misses), up to four
   passes; then `ROUTING_RULE` fails the command and HEAD stays unchanged.
   `validate()` runs on the result, with the same telemetry checks as before
   (track-graph node count instead of the lattice size).

The lattice router, its caps and its output are unchanged: profiles without
`track_router` give byte-identical DEF, design.json (except input paths) and
powered Verilog to the binary before this stream (checked on `pipeline.v`
for both PDKs).

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

## Budgets and memory estimate (written before implementation, Rule 9)

Track router (one corner, one worker):

    bytes ~ 24 N + 48 G + 160 S
    N = sum over layers of tracks x stops   (cost u64, parent u32, via u32,
                                              epoch u32, history u16)
    G = GCells                               (A* state, guide marks, edges)
    S = obstacles + 16 x terminals + routed runs  (shape 48 B, marks/flags,
                                              bin references, map overhead)

`route` computes the first two terms plus `S = obstacles + 16 terminals`
before allocating and refuses a plan above `memory_mib` (default 2048).
Limits in track mode may exceed the lattice defaults up to
`RouteLimits::ceiling()` (64M nodes, 8M obstacles, 8M segments, 2e9
expansions, 2^40 comparisons, 64M heap); the scale profiles use 8M nodes,
4M obstacles, 2M segments, 4e8 expansions, 4e9 per search, 4e10
comparisons. Every search counts expansions per search and in total;
spatial queries count comparisons; every queue is capped.

| design (cells incl. taps/fillers) | track nodes N | estimate (N + G + S) | measured route RSS |
|---|---|---|---|
| regfile sky130 (1,479) | 429k | ~17 MiB | 41 MiB |
| regfile gf180 (1,737) | 464k | ~18 MiB | 46 MiB |
| SERV sky130 (stretch, see below) | see below | | |

The measured RSS includes the loaded workspace (design.json, LEF-derived
masters, connectivity) and the output records; route RSS stays under
50 MiB for every required design.

Placer: O(cells + pins): positions and targets (32 B/cell), the B2B system
(at most 2(2p-3) entries per p-pin net and axis, 16 B each) and the CSR copy;
work (CG multiply-adds, spreading and legalization steps) counted against
`max_work` (2e10). Floorplan: two placements of the same size.

Harness: host processes run with `RLIMIT_AS` 8 GiB; the whole harness under
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G`; signoff
containers at 4 GiB. Unit/integration tests stay far below 2 GiB (largest
test workspace < 2 MiB).

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

Design manifests (`fixtures/designs/<name>.json`, schema
`rustroad.design.v1`, the gate's schema) pin the RTL, the clock SDC, the
I/O SDC template, the profiles, the generated floorplan and physical
configuration per PDK, the frozen PDK inputs (cell GDS/LEF, technology LEF,
Liberty and OpenRCX rules per corner, RC profiles) and the
`floorplan_recipe` (utilization 0.5, aspect 1, margin 10 um).
`scripts/scale_manifests.py` regenerates them.

Results: see the tables at the end of this file (filled from
`docs/evidence/scale.json`).
