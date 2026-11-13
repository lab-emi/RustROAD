# Tap insertion and bounded clock topology — v0.4.0

The real-PDK preparation path now reserves and inserts well/substrate tap cells,
then checks their physical contacts independently using original PDK GDS. A
separate `clock-tree` transaction inserts a level-balanced buffer topology and
rebuilds placement, taps, fillers and the primary supply network. This is an
unrouted CTS prototype. Signal routing, parasitic extraction, propagated-clock
timing and complete RTL-to-GDS signoff remain unfinished.

## Native implementation

`rustroad-core::tap` reserves a tap at both ends of every row and distributes
additional site-aligned slots until each clear horizontal gap satisfies the
configured limit. Logical cells pack into the free intervals in deterministic
order. The placer rejects a cell that cannot fit between taps; it does not
silently expand the permitted gap. Tap count, total instances, work and integer
coordinate arithmetic have explicit limits. See [the upfront resource
estimate](tap-budget.md).

`PhysicalConfig.tap` binds `cell`, `max_gap`, `well_layer` and `substrate_layer`.
The selected LEF master must be `CORE WELLTAP`, have only supply pins and expose
the configured primary rails and well/substrate pin geometry. The two checked
profiles use:

| PDK | Tap master | Width × height (µm) | LEF well/substrate |
|---|---|---|---|
| sky130_fd_sc_hd | sky130_fd_sc_hd__tapvpwrvgnd_1 | 0.46 × 2.72 | nwell / pwell |
| gf180mcu_fd_sc_mcu7t5v0 | gf180mcu_fd_sc_mcu7t5v0__filltie | 1.12 × 3.92 | Nwell / Pwell |

Both profiles allow at most 20 µm between tap-cell edges. LEF uses 1,000 DBU/µm
for Sky130 and 2,000 DBU/µm for GF180. GDS coordinates are converted exactly;
the checked source grids need factors 1 and 2 respectively. Nonintegral
conversion fails. LEF layer names are not used as proof of GDS contact presence.

Schema 4 stores generated tap identities and bounded reports, and mirrors tap
positions in the mmap physical table. Opening a snapshot regenerates the
expected placement policy and checks actual tap positions. Repeated `place`
removes generated taps/fillers and recreates the same layout. Schemas 1–3 remain
readable. Logical Verilog omits physical-only taps/fillers; powered Verilog and
DEF include them and their supply connections. Clock buffers remain in both
Verilog views.

### Experimental staggered rows

An explicit optional `tap.pattern` selects a staggered lattice. For example,
`"max_gap": 30000, "pattern": {"kind": "staggered", "phase": 15000}`
uses DBU, not microns. Absent `pattern` retains the original planner and JSON
output. No public PDK profile selects this experiment by default.

`max_gap` still bounds the **clear horizontal gap between tap-cell bounding
boxes**. The staggered mode also uses it as the nominal lattice pitch; pitch
and odd-row phase round down to whole sites. Phase is relative to the physical
row number from the core origin, never the macro-cut segment number. The
lattice's x origin is the core's left edge. Each row segment keeps taps at both
ends, including macro halo boundaries; a short segment that excludes every
lattice anchor receives a bounded interior repair when needed. Narrow segments
that cannot hold the boundary taps fail explicitly. Placement, free intervals,
persisted-policy validation and repeated placement all use this same policy.

The lattice relies on wells shared by row pairs: an `N` row and the `FS` row
that faces it share one well, and their two phases interleave there at half the
pitch. A row edge without such a partner (the top core row, a row bordering a
macro, its halo or a row gap) carries only its own anchors. The optional
`"unpaired"` field adds the other row parity's anchors on exactly those
stretches, widened by half a pitch at each end:

```json
"max_gap": 42000, "pattern": {"kind": "staggered", "phase": 21000, "unpaired": "cell_top"}
```

`cell_top` names the edge at the top of an unflipped cell (the top of an `N` row,
the bottom of an `FS` row; the n-well edge of a library whose power rail is at
the cell top); `both` checks both edges. A partner exists only where a row
segment of the opposite orientation abuts that edge. Added anchors never
replace or move the row's own anchors; one that would overlap a boundary or
repair tap is dropped, and `phase` and `pitch - phase` must each be at least
the tap width. Absent `unpaired` keeps the plain lattice and its JSON. The
planner still makes no coverage claim. A layout that uses it needs independent
latch-up evidence; for the IHP Ibex recipe that is the `tap_coverage.py`
certificate, a maximal-deck LU.a run and main-deck LU.b on the final GDS. Only
LU.b runs inside that flow (main-deck DRC); the tap-coverage certificate and
maximal-deck LU.a are required out-of-flow evidence, not gates. At a 42 um
pitch the densification is load-bearing: without `unpaired` the certificate
leaves three LU.a boxes in the top core row. The recipe therefore admits only
lattices with that evidence on its floorplan instead of any staggered pitch.

The native gap metric is distinct from foundry diffusion-to-corresponding-tie
distance. The pattern does not establish connected well/body domains, electrical
contact or latch-up compliance. Its report retains
`requires_independent_GDS_verification`. Candidate acceptance requires original
GDS geometry in connected domains and unchanged foundry rule runs. In the pinned
IHP decks, main emits LU.b and maximal emits LU.a; retain both full category
inventories and their missing-tie negative controls. Neither deck name alone
establishes both rule scopes. No full-flow or default promotion follows from
planner tests or a component placement replay.

### Row-pair wells stay separate

Each `N`/`FS` row pair shares one NWell strip, and the taps of that pair bias
it. RustROAD does not join the strips to each other. A GDS-export variant that
did (an NWell spine just outside one core side, with a stub to every strip that
reaches that side) was evaluated on the IHP Ibex layout and removed again
(commit `e106b5d`):

1. **It weakens the geometric latch-up certificate.** `tap_coverage.py` takes
   each merged NWell polygon as one domain and accepts any contacted n-tie of
   that domain within 20 µm straight-line L1 distance. Once the strips form one
   comb, the ties of the neighbouring strips, 6.72 µm away across the
   p-substrate gap, certify a strip that has none. With the n-tie contacts of
   all 54 taps of one full-width strip removed, the certificate passed the
   bridged copy and failed the unbridged copy (32 uncertified boxes, its report
   cap). The IHP LU.a definition grows the ties inside the NWell
   (`ext_enlarge_inside`), so that strip violates it in both copies.
2. **Its speed-up came from certifying less.** The certificate ran faster on
   the bridged layout (109 LU.a domains with targets instead of 251) only
   because it then certified 143 strips as one domain.
3. **It had no measured electrical benefit.** Each stub touched only the
   0.205 µm well overhang of its strip's edge WELLTAP, so a strip that loses
   that tap is cut off from the spine again, and the 0.84 µm wide, 956 µm long
   spine is a high-resistance path next to a tap. NW.b (minimum NWell space or
   notch of the same net, 0.62 µm) concerns wells closer than 0.62 µm; the
   strips are 2.92 µm apart.

For the record, the certificate's run time depended on the domain shapes: in
single diagnostic runs its per-domain intersection took roughly 80 ms for a
notched (non-rectangular) domain and about 2 ms for a rectangle. On the Ibex
layout 71 of the 251 LU.a domains are notched (24 of 197 on the LibreLane
reference layout). The notches belong to six masters placed 1,599 times,
mostly the mux-input reset flop `DFFMRQX2` (1,344), whose N+ diffusion lies
closer than NW.d (0.31 µm) to the notch, so the notch cannot be filled. That
cost followed from cell selection in synthesis, not from the well geometry
RustROAD draws.

Since 2026-11-12 the shared certificate no longer intersects each domain with
the whole target layer: it gathers every domain's targets and ties through a
bounding-box index, merges them again per domain, reads only the contacts that
touch tie diffusion and flattens the layers natively, so its time no longer
follows the notches. The checker changed for both layouts; neither layout did.
On both Ibex final layouts, and on seven tap-removal mutants of each, it
examines the same target boxes against the same tie points as before and fails
the same ones. In a notched domain LU.a now also needs a path inside the
n-well, as the foundry rule grows the ties inside NWell, so a comb of strips no
longer certifies a strip through the ties of its neighbours (point 1 above
describes the certificate of that time). In 6 interleaved runs at the same load
the latch-up gate took a median 5.70 s on this layout against 7.34 s on
LibreLane's (0.78×), where the previous checker took 39.02 s against 35.21 s
(1.11×); details are on the superproject's benchmark page.

## Independent physical verification

`verify_taps.py` invokes pinned OpenROAD/OpenDB as an external process to read
the exported DEF and the original technology/cell LEFs. It checks placement,
tap spacing, primary PG connectivity and powered-Verilog endpoint partitions.
It does not accept RustROAD's own success flag as the physical oracle.

`check_tap_gds.py` runs in external KLayout and reads the pinned original merged
GDS library and its layer-property map. Polygon Boolean operations verify:

1. The actual tap has n+ contacts inside nwell and p+ contacts in substrate/pwell.
2. Each contact reaches the correct primary rail. Sky130 follows tap → licon →
   li1 → mcon → met1; GF180 follows COMP → Contact → Metal1. A connection to the
   opposite rail fails.
3. Every assembled well/substrate component has a tap, including empty-row
   filler wells. Every inspected active polygon lies within the configured
   15 µm Manhattan contact coverage in the same component.
4. All N/FS instance transformations are applied to actual GDS polygons. Logic
   input cells must contain active geometry, preventing a vacuous distance pass.

The 15 µm limit is a conservative **engineering acceptance policy**. It is not
a complete foundry latch-up implementation. Sky130's [nwell.4 rule](https://skywater-pdk.readthedocs.io/en/main/rules/periphery.html#nwell)
requires a contacted tap in each nwell, with stated exceptions. GF180 documents
[conditional core latch-up limits](https://gf180mcu-pdk.readthedocs.io/en/latest/physical_verification/design_manual/drm_14_3_1.html)
and separate [COMP distance rules DF.13/14](https://gf180mcu-pdk.readthedocs.io/en/latest/physical_verification/design_manual/drm_07_06.html).
The checker does not encode every voltage, spacing, marker and exception in
those rule decks. Deep-well cases are explicitly rejected. I/O guard rings,
contact enclosure/area DRC, full LVS, IR drop and electromigration remain
unchecked.

Missing contact, missing taps, end-only taps with excessive interior distance
and wrong tap supply controls must all fail. The inherited power checker also
rejects row polarity, filler-gap, spine-gap, short and interior-I/O defects.

## Clock topology and its limits

`PhysicalConfig.clock` explicitly binds the source port, sink pin and a
single-input/single-output buffer master. `max_fanout`, `max_buffers` and
`max_work` cap insertion and checking. The current algorithm sorts sinks by
their existing position and groups them bottom-up into equal buffer depth.
The topology is re-linked and checked before an atomic workspace commit.
Unsupported clock source aliases, non-clock root loads, repeated synthesis,
stale base revisions and insufficient capacity fail explicitly.

The verification harness proves both Boolean input values of the actual Liberty
buffer with Yosys and rejects inverted polarity. Whole-design RTL equivalence
and 512-vector Verilator checks follow. A separate OpenDB checker traces each
sink backwards through unique drivers, checks all inserted buffers and fanout,
and rejects bypassed sinks and disconnected buffers. Post-clock tap/GDS/PG
checks run again. Repeated placement must produce byte-identical DEF.

The six-sink fixture on each PDK produces six buffers, three levels and fanout
at most two. `sky130_fd_sc_hd__clkbuf_1` uses A/X; GF180's corresponding master
uses I/Z. This is not a delay- or skew-optimized clock tree. The source port is
configured directly, not discovered from SDC. Gated/generated clocks, sink
capacitance limits, clock routing, propagated clock latency and skew closure are
future work. `clock_topology_status` therefore reports
`unrouted_topology_checked`, while routing remains `not_run`.

## Reproduction

Build with the repository's pinned Rust toolchain and resource limits. The local
profile pins Ciel/open_pdks source files; adjust paths to identical files on
another machine. Do not substitute guessed geometry or unpinned libraries.

```sh
free -h
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
/usr/bin/time -v -o .work/tap-build.time timeout 180s \
  cargo build --release --locked --offline --bin rustroad
/usr/bin/time -v -o .work/tap-verification.time timeout 180s \
  python3 scripts/verify_taps.py --workspace .work/tap-verification --base-rev empty
```

The harness uses a fresh evidence workspace, its own `workspace.lock` and
`--base-rev empty`. Each EDA mutation also uses the native revision contract.
Children receive 2 GiB memory/30 s guards; OpenROAD has an 8 GiB virtual-address
allowance but a 2 GiB/no-swap container limit. Both PDK cases execute serially.
KLayout and Python belong only to the external test harness, never the core.

For an imported physical design:

```sh
rustroad import-pdk --workspace .work/chip --base-rev rev0 \
  --tech-lef technology.tlef --cell-lef cells.lef --netlist mapped.v \
  --floorplan floor.json --physical-config fixtures/pdk/sky130-tap.json
rustroad place --workspace .work/chip --base-rev rev1
rustroad clock-tree --workspace .work/chip --base-rev rev2
rustroad export-def --workspace .work/chip
rustroad export-verilog --workspace .work/chip --include-power
```

Use matching GF180 paths and profile for that PDK. Return revisions, rather than
the example's fixed revision strings, should drive an automated flow.

## Evidence and next gates

The [HTML report](tap-integration.md), [complete compact result](evidence/tap-results.json)
and [build/test receipt](evidence/tap-validation.json) record exact counts, input
and executable hashes, timing scope, negative controls, wall time and peak RSS.
Full process logs remain in the recorded local evidence directory.

RustSTA/OpenSTA comparisons are TT, ideal wire and ideal clock. Both retain 14
constrained endpoints and two constant-driven D pins without data arrival;
only those independently identified endpoints are accepted. Pulse-width and
minimum-period limitations remain explicit. Incremental-vs-full testing is a
no-op update, not a changed-ECO test. The original RustSys/RustSTA binaries stay
frozen and neither sibling source tree was modified.

Next: derive clock constraints and transition/capacitance targets, legalize
under those constraints, implement bounded obstacle-aware signal/clock routing,
then extract corner-tagged RC/SPEF and validate propagated timing against an
independent tool. GDS streamout and full-deck DRC/LVS are still required to close
the complete RTL-to-GDS flow.
