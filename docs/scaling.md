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
4M obstacles, 2M segments, 4e8 expansions, 4e6 per search, 4e10
comparisons (`gf180-scale-routing-large.json`, used by rv_core only: 2e9
expansions and 2e11 comparisons, same memory caps). Every search counts
expansions per search and in total; spatial queries count comparisons;
every queue is capped.

Expected sizes by the formula above: the required designs have
N <= 0.5M track nodes, G <= 10k GCells and S <= 50k shapes, i.e. an
estimate below 20 MiB on top of the loaded workspace; SERV-size blocks
(7-8k nets, ~25k placed instances) about 4-6M nodes and ~0.2-0.3 GiB. The
measured values are in the results below (route RSS stays under 50 MiB for
every required design). The estimate covers the router state only; on
SERV/rv_core-size blocks the route process peaks at 2.2-2.8x the estimate
(see Stretch designs), so the cgroup cap stays in place.

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
`--pdk gf180` reruns one PDK, and the evidence merges run directories:

```sh
python3 scripts/scale_evidence.py --runs .work/scale/runs/x2 --runs .work/scale/runs/x3 \
  --design serv --design rv_core --out docs/evidence/scale-stretch.json
```

## Results (binary `a453e4ca…`, RustSys policy producer `3de13b3a…`)

`docs/evidence/scale.json` holds every row below with hashes and receipts
(`scripts/scale_evidence.py`). Signoff image `4c48415d8bd0` (Magic 8.3.678,
Netgen 1.5.323, KLayout 0.30.9). Every design ran twice per PDK from RTL;
DEF, GDS, Verilog and powered Verilog were byte-identical both times.
Native GDS == KLayout's streamout of the DEF (XOR 0) everywhere.

| design | PDK | logic cells (placed incl. taps/fillers) | routed nets | Magic | KLayout | antenna | Netgen LVS | KLayout LVS | CEC | HPWL / fixed order | route s / MiB | flow s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| counter | sky130 | 43 (264) | 53 | 0 | 0 | 0 | match | match | equivalent | 0.73 | 0.12 / 17 | 17 |
| counter | gf180 | 46 (306) | 51 | 0 | 0 | 0 | match | match | equivalent | 0.53 | 0.08 / 22 | 20 |
| uart_tx | sky130 | 89 (560) | 107 | 0 | 0 | 0 | match | match | equivalent | 0.55 | 0.26 / 19 | 23 |
| uart_tx | gf180 | 100 (600) | 114 | 0 | 0 | 0 | match | match | equivalent | 0.47 | 0.16 / 22 | 24 |
| fifo | sky130 | 81 (481) | 102 | 0 | 0 | 0 | match | match | equivalent | 0.44 | 0.22 / 18 | 25 |
| fifo | gf180 | 114 (820) | 132 | 0 | 0 | 0 | match | match | equivalent | 0.38 | 0.18 / 24 | 25 |
| alu | sky130 | 253 (1094) | 272 | 0 | 0 | 0 | match | match | equivalent | 0.54 | 0.50 / 28 | 44 |
| alu | gf180 | 252 (1001) | 271 | 0 | 0 | 0 | match | match | equivalent | 0.41 | 0.42 / 27 | 32 |
| regfile | sky130 | 372 (1479) | 425 | 0 | 0 | 0 | match | match | equivalent | 0.42 | 1.21 / 40 | 77 |
| regfile | gf180 | 358 (1737) | 387 | 0 | 0 | 0 | match | match | equivalent | 0.38 | 1.75 / 43 | 44 |
| mult | sky130 | 418 (1651) | 440 | 0 | 0 | 0 | match | match | equivalent | 0.41 | 0.72 / 37 | 62 |
| mult | gf180 | 414 (1793) | 436 | 0 | 0 | 0 | match | match | equivalent | 0.41 | 0.80 / 41 | 45 |
| dense | sky130 | 26 literal (258) | 31 | 0 | 0 | 0 | **failed** | **failed** | equivalent | 0.61 | 0.06 / 17 | 13 |
| dense | gf180 | 30 literal (160) | 35 | 0 | 0 | 0 | match | match | equivalent | 0.62 | 0.04 / 21 | 16 |
| pipeline_track | sky130 | 10 (77) | 17 | 0 | 0 | 0 | match | match | equivalent | 0.72 | 0.04 / 17 | 11 |
| pipeline_track | gf180 | 10 (132) | 17 | 0 | 0 | 0 | match | match | equivalent | 0.63 | 0.02 / 21 | 16 |

* Antenna: Magic `antennacheck` (sky130) and the KLayout antenna deck
  (GF180), plus the native `check-antenna` (0 violations on every required
  design, so no antenna ECO was needed).
* Density (separate, never gating): GF180 fails the whole-block coverage
  rules (7-8 items, no fill), sky130 windowed density is `unverified`
  (block smaller than the window), as for `pipeline.v` before.
* `route` = the `route --scope all` step (wall seconds / peak RSS of the
  whole process including the loaded workspace); `flow` = one PDK of the
  harness end to end: synthesis, floorplan, import, place, CTS, route,
  antenna check, exports, native GDS, reference streamout, all signoff
  checks, CEC and the full second run. Peak host-process RSS 61-105 MiB
  (RustSys synthesis); signoff containers 441-520 MiB.
* `dense` sky130: DRC 0/0, XOR, antenna and CEC pass. Both LVS checks fail
  on the two `sky130_fd_sc_hd__conb_1` tie cells the literal netlist needs
  (`high_bit <= 1`, `low_bit <= 0`): Netgen ends with "Circuits match
  uniquely." but the harness rejects the four disconnected `VPB`/`VNB`
  nodes that conb_1 (resistors only) has on both sides; KLayout's sky130
  deck extracts conb_1 as HI shorted to VPWR and LO to VGND (no resistor
  device) and reports the cell `NoMatch`, so the top is skipped. This is a
  property of the PDK tie cell and the pinned decks, independent of
  routing; the signoff status logic is frozen and was not changed. GF180
  (separate tieh/tiel cells) passes. Raw receipts:
  `.work/scale/runs/s5/dense/sky130/a/signoff/lvs_{netgen,klayout}.json`.
* The lattice router (`pipeline.v`, `scripts/verify_routing.py` with the
  integrated profiles) gives DEF `cb94ea92…` (sky130) and `3a07101c…`
  (GF180), identical Verilog and powered Verilog with this binary and with
  the e0d074d binary: the requalified DRC 0/0 and LVS match carry over.
* `scripts/hold_closure.py` (lattice-router profiles, native extraction,
  RustSTA round-2 binaries) still closes `pipeline.v` with this binary:
  `passed` on both PDKs (sky130 3 iterations, GF180 2), so the hold flow is
  unaffected.
* Tap gap control: GF180 counter with a 60 um tap gap gives KLayout
  DF.13_MV 31 + DF.14_MV 26 items (Magic 0); the 23 um scale gap gives 0.

Memory estimate versus measurement (route step):

| design | PDK | nodes N | GCells G | shapes S | estimate | route RSS |
|---|---|---|---|---|---|---|
| counter | sky130 | 110,032 | 529 | 5,866 | 3.4 MiB | 17.3 MiB |
| alu | sky130 | 302,176 | 1,444 | 24,727 | 10.8 MiB | 27.9 MiB |
| regfile | sky130 | 429,120 | 2,025 | 47,132 | 17.1 MiB | 39.6 MiB |
| regfile | gf180 | 463,752 | 9,506 | 38,593 | 16.9 MiB | 43.4 MiB |
| mult | gf180 | 463,752 | 9,506 | 35,947 | 16.5 MiB | 40.7 MiB |

The difference is the process baseline (binary, loaded design.json, LEF
masters, connectivity, output records: ~14-20 MiB for these sizes).
`cargo test --workspace`: 160 passed, 4 ignored; each test binary peaks
below 10 MiB RSS; the whole run 16 s.

## Stretch designs (same binary)

`docs/evidence/scale-stretch.json` (rows name their run directory: `x2`
for SERV and rv_core sky130, `x3` for rv_core GF180 with the large routing
profile). Both ran behind `flock .work/scale.lock` with a 14000 s flow
timeout.

| design | PDK | logic cells (placed) | routed nets | Magic | KLayout | antenna signoff / native | Netgen LVS | KLayout LVS | CEC | rerun | HPWL / fixed order | route s / MiB | flow s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| serv | sky130 | 7370 (24318) | 7959 | 0 | 0 | 29 / 102 | port errors | unverified | equivalent | identical | 0.19 | 108 / 514 | 1742 |
| serv | gf180 | 6221 (26145) | 6481 | 0 | 0 | 45 / 13 | port errors | unverified | equivalent | identical | 0.18 | 115 / 536 | 680 |
| rv_core | sky130 | 5047 (19290) | - | - | - | - | - | - | - | - | 0.24 | **failed** (337 s / 334) | - |
| rv_core | gf180 | 7915 (35042) | 8128 | 0 | 0 | 17 / 4 | port errors | unverified | **unverified** (timeout) | identical | 0.21 | 290 / 860 | 3174 (1800 CEC) |

* SERV is DRC-clean (Magic and KLayout 0, XOR 0) on both PDKs and its
  routed netlist is equivalent to the RTL. Netgen reports "Netlists match
  uniquely with port errors": SERV drives several output ports from one net
  (`o_ext_rs1[i]`/`o_dbus_adr[i]`, `o_ext_rs2[i]`/`o_dbus_dat[i]`) and leaves
  inputs unused (`i_ibus_rdt[1:0]`, `i_ext_rd`), so Magic flags shorted
  ports and the pin lists differ; the KLayout check and the port-label check
  are `unverified` because the reference netlist has aliased ports ("shorted
  ports are unsupported"). Sky130 additionally lists the conb_1 `VPB`/`VNB`
  nodes (as for `dense`). Fixing this needs a buffer per aliased output in
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
  structured, after 337 s). Untracked experiments on the same placement:
  5x work budgets stop at "net 1700 ripped up more than 24 times" (12 min);
  5x budgets with 64 rip-ups exceed 2e11 comparisons (18.5 min, 396 MiB);
  utilization 0.4 fails earlier with one pin that has "no legal access
  after local repair". The root cause (a congestion hot spot the
  negotiation cannot spread, or a pin-access corner case) is not isolated.
* rv_core GF180 uses `gf180-scale-physical-wide.json` (25.2 um tap gap for
  its 24.1 um cells) and `gf180-scale-routing-large.json`: with the default
  profile it stops at the 4e8 expansion budget (`x2`); it needs 4.3e8. It
  is then DRC-clean (Magic 0, KLayout 0 including the DF.13/DF.14 tap
  distance rules, XOR 0) and byte-identical on the rerun. Netgen reports
  port errors only (`dmem_addr_o[0]`/`[1]` share one net in the netlist);
  the native antenna check finds 4 violations, all on clock-tree nets, and
  KLayout 17 (`ANT.16_ii_ANT.4` 14, `ANT.5` 3). `rustsys check synth` did
  not finish within the harness' 1800 s (killed, 2.2 GiB peak), so CEC is
  `unverified`; the same check of the synthesized netlist before
  floorplanning also runs past 1800 s, so this is the checker on rv_core's
  logic, not the physical flow.

Route memory estimate versus measurement on these blocks:

| design | PDK | nodes N | GCells G | shapes S | estimate | route RSS |
|---|---|---|---|---|---|---|
| serv | sky130 | 4,756,800 | 22,350 | 798,177 | 232 MiB | 514 MiB |
| serv | gf180 | 5,769,588 | 117,992 | 611,294 | 231 MiB | 536 MiB |
| rv_core | sky130 | 4,278,176 | 20,164 | 606,639 | 191 MiB | 334 MiB (failed) |
| rv_core | gf180 | 7,879,224 | 161,202 | 754,441 | 303 MiB | 860 MiB |

The estimate covers the router state; per-net plans, spatial bins, the
loaded design and the output records add 1.2-1.8x on top at this size.
The 2048 MiB `memory_mib` default therefore bounds the process at roughly
5-6 GiB, which is why the harness also runs under a cgroup cap.

## Limits and open issues

* **Sky130 tie cells.** Designs that need constants on sky130 instantiate
  `conb_1`, which neither LVS check of the pinned harness accepts (see the
  dense row). A tie-cell policy (dual-output reuse, or deck/harness support
  for device-less body pins and conb_1's resistors) belongs to the signoff
  and gate streams; GF180's tieh/tiel pass.
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
* **Memory estimate.** The pre-allocation estimate covers the router state;
  the route process peaks at 2.2-2.8x of it on 20-35k-instance blocks. An
  overhead term (per-net plans, spatial bins, output records) is missing.
* **Extraction granularity.** Wires are stored as maximal runs split at
  junctions, so `extract` sees no interior nodes inside a straight run (the
  Elmore delay of a straight segment is unchanged; higher-order slew effects
  of very long runs are not split out).
* **Escaped names.** RustSys' `\p_q[0]` next to port `p_q` is renamed by the
  harness, not by RustROAD's importer, which still refuses the ambiguity.
