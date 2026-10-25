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
