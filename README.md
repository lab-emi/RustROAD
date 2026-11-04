<p align="center">
  <img src="docs/assets/rustroad-wordmark.svg" alt="RustROAD" width="360">
</p>

<h1 align="center">Physical design for the next design iteration.</h1>

<p align="center">
  <a href="https://lab-emi.github.io/RustEDA/modules/rustroad/">Documentation</a> ·
  <a href="https://github.com/lab-emi/RustEDA">RustEDA</a> ·
  <a href="https://lab-emi.github.io/RustEDA/benchmarks/">Benchmarks</a> ·
  <a href="LICENSE">Apache-2.0</a>
</p>

<picture>
  <source media="(prefers-reduced-motion: reduce)" srcset="docs/assets/rustroad-banner.svg">
  <img src="docs/assets/rustroad-banner.gif" alt="RustROAD implements placement, clock trees, routing, extraction and GDS export" width="1200">
</picture>

RustROAD implements placement, clock trees, routing, extraction and GDS export. The recorded small-block baseline spans SKY130 HD and GF180 7T: **15 ordinary passes and one declared deviation**. OpenC910 physical implementation remains unverified.

Part of **[RustEDA](https://github.com/lab-emi/RustEDA)**, infrastructure for chip-design recursive self-improvement. Native tools shorten the work between inference steps; verified results guide the next iteration.

[Current qualification and limits](docs/closeout-2026-10-25.md) · [Flow guide](docs/rtl2gds.md)

## Latest development · 2026-11-02

The RustEDA Ibex host campaign (IHP SG13G2 / EZ130 with two real SRAMs, on a
4-vCPU Intel Xeon cloud host with 15.7 GiB RAM and no swap) ran RustROAD CLI
0.5.0 (commit [`07e1626`](https://github.com/lab-emi/RustROAD/commit/07e1626f220ef1b7f3029ef827fae4b1491c3373))
in the repository's published native recipe under an 8 GiB memory cap
(+2 GiB memsw), and OpenROAD 26Q3 in LibreLane 3.0.17 Classic under a 10 GiB
cap, from the same prepared RTL, SDC and floorplan. The same independent
scripts checked both final layouts. **Both pass foundry main and antenna DRC
with 0 violations and full-device LVS.** On the shared timing gate the native
layout has zero setup, hold, slew, capacitance and fanout violations at TT and
SS; the LibreLane layout keeps 28 (TT) and 24 (SS) capacitance and 30 fanout
violations. The two layouts come from different netlists; the table gives
their routed wire and vias side by side, and no wirelength or via ratio is
formed (the receipt's `wirelength_ratio` is null).

| Ibex final layout | RustROAD, native recipe | OpenROAD 26Q3, LibreLane Classic |
| --- | --- | --- |
| Standard-cell area | 233,909.2 µm² | 197,913.7 µm² |
| Routed signal wirelength | 1,679,076.6 µm | 969,571.6 µm |
| Vias | 301,099 | 122,194 |
| Foundry main / antenna DRC violations | 0 / 0 | 0 / 0 |
| Full-device LVS | `passed` | `passed` |
| Placement and CTS | 20.92 s, summed over four placement passes including import/export steps | 16.30 s placement + 20.17 s CTS |
| Routing | 389.63 s at 4 threads, global and detailed | 68.98 s global routing and antenna checks/repair + 1,796.83 s detailed |
| Repair after detailed routing | 179.00 s post-route + 409.35 s antenna repair and verification | None; its post-GRT repair runs before detailed routing |

Stage times come from one timed run of each flow. The netlists and the repair
around each stage differ, so they are not an identical-task comparison and no
quotient is formed; the
[row scopes](docs/development-status.md#place-cts-and-route-against-openroad)
are listed with the full table. Standard-cell area is a whole-flow result of
different netlists and repair. The native run is `unqualified`. Its producer
completed every physical step and then stopped at the RustSTA/OpenSTA
`timing_accuracy` gate (1.4052 ps against 1 ps); the remaining gates ran
separately with the recipe's scripts. Neither flow passes every gate, so the
receipt's end-to-end ratios (speed-up, standard-cell area, wirelength) are
null. Deviations include the nixpkgs OpenROAD 26Q3 build instead of
LibreLane's pinned OpenROAD `dcf36133`, LibreLane's own post-GRT design repair,
and a reconstructed EZ130 tech LEF and cell GDS (5 of 7 pinned EZ130 files are
byte-identical); see
[method and deviations](https://lab-emi.github.io/RustEDA/benchmarks/#host-campaign-method-and-deviations).

On the same final layout, RustROAD's extraction and GDS export are slower than
the reference tools. RustROAD reads its final workspace; OpenRCX and KLayout
read the exported DEF and LEFs, and the two extractors use their own RC
models. Native extraction took a median 158.823 s with the harness's
`RAYON_NUM_THREADS` at 2 or 4 (RustROAD's pool stays at its default
`--threads 2`, which the recipe's extraction command does not override),
against 17.649 s and 17.114 s for OpenRCX at 2 and 4 threads (9.0× and 9.3×),
with 810,632 KiB against 295,728 KiB worst peak RSS at setting 4. Its CPU time
tracks its wall time, so it is effectively single-threaded in this
configuration; the recipe leaves the opt-in candidate-query workers at their
serial default ([extraction guide](docs/extraction.md#bounded-cpu-candidate-queries)).
Over the same 16,329 nets its SPEF totals are 312.638 pF and 6,633.54 kΩ,
against 290.705 pF and 6,691.924 kΩ for OpenRCX. GDS export took 3.019 s
against 2.286 s for KLayout 0.30.12's LEF/DEF streamout (1.32×); KLayout's
time includes DEF and LEF parsing. The two GDS files are not byte-equivalent
and no geometric comparison was recorded, so the GDS result is a timing
comparison only. Medians are of three measured repetitions after one warm-up,
interleaved.

The published recipe, run with the reconstructed EZ130 tech LEF and cell GDS,
reproduces the run16-class layout of the
[2026-10-30 comparison](https://lab-emi.github.io/RustEDA/benchmarks/#historical-matched-comparison-2026-10-30):
the same 233,909.2 µm² standard-cell area, with 1,679,076.6 µm of wire and
301,099 vias against 1,678,598.6 µm and 301,859 in the 2026-10-30 receipt. Two
complete native runs on this host produced byte-identical GDS, DEF, netlists
and native SPEF. The
[R20/R21 recipe of 2026-11-01](https://lab-emi.github.io/RustEDA/benchmarks/#latest-native-development-results-2026-11-01)
(1,190,522 µm and 168,381 vias in its development receipt) used inputs that
are not published, so it was not reproduced.

The measured source is the 2026-11-01 development update. It combines bounded
extraction indices and query workers, single-session antenna repair, an opt-in
staggered tap lattice, placement-stage diagnostics and spacing-aware
electrostatic density. The density model accounts for the same cell spacing
required by legalization. Optional checkpoint retention at an evaluation
horizon reports the reason explicitly; in that update the default policy stayed
strict. Since then, keeping the checkpoint at the evaluation cap is the default
(`stop_at_evaluation_cap`, see
[electrostatic placement](docs/electrostatic-placement.md)). Repeated
same-path routing near misses receive bounded repair.

[Current source, scope and method](docs/development-status.md) ·
[Place, CTS and route vs OpenROAD](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-place-cts-and-route-vs-openroad) ·
[Extraction vs OpenRCX](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-extraction-vs-openrcx) ·
[GDS export vs KLayout](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-gds-export-vs-klayout) ·
[All benchmarks](https://lab-emi.github.io/RustEDA/benchmarks/)

## Build and try

Read the [memory budgets](docs/memory-budget.md) before running large builds.
Use the pinned Rust 1.97.1 toolchain; limit CPU concurrency and process memory:

```sh
mkdir -p .work
free -h
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
/usr/bin/time -v -o .work/tests.time timeout 180s cargo test --workspace --locked
cargo build --release --locked --bin rustroad

target/release/rustroad init --workspace .work/demo --base-rev rev0 \
  --input fixtures/physical-small.json
target/release/rustroad place --workspace .work/demo --base-rev rev1
target/release/rustroad inspect --workspace .work/demo
target/release/rustroad export-def --workspace .work/demo > .work/demo.def
python3 scripts/check_def.py --input fixtures/physical-small.json --def .work/demo.def
```

`rev0` means an unpublished workspace. Successful mutations return the new
revision. Reuse that value on the next mutation; stale values return
`REVISION_CONFLICT`. Failed placement never publishes a partial revision. Use a
new directory when repeating the example; `init` never resets an existing design.

`--threads`, `--max-cells`, `--max-rows` and `--max-expansions` are configurable.
The 16 MiB design/metadata cap remains in force. JSON rejects unknown fields;
unsupported multi-height cells, inconsistent masters, unaligned/overlapping rows,
duplicate names and overflowing coordinates fail explicitly. The optional
`site.name` defaults to `rustroad_site`; real LEF interoperability requires a
matching site and matching cell dimensions.

The JSON geometry example still exports rows/components only. Designs imported
with `import-pdk` also export logical PINS/NETS. With `--physical-config`, placement
creates physical I/O, fillers, alternating rows and a primary supply network.
Profiles with `tap` also reserve and insert well/substrate taps before filling.
Without that explicit profile, I/O and supply geometry remain unchecked. Exported Verilog
preserves the mapped logic and interfaces. `placement_status` remains
`placed_unverified`; external validation receipts are separate evidence.

## Real PDK and mapped-netlist import

```sh
target/release/rustroad import-pdk --workspace .work/connected --base-rev rev0 \
  --tech-lef /path/to/technology.tlef --cell-lef /path/to/cells.lef \
  --netlist /path/to/rustsys-mapped.v --floorplan /path/to/floorplan.json
target/release/rustroad place --workspace .work/connected --base-rev rev1
target/release/rustroad export-verilog --workspace .work/connected > .work/connected.v
target/release/rustroad export-def --workspace .work/connected > .work/connected.def
```

The floorplan JSON contains `name`, `units_per_micron`, `die_area`, `core_area`,
`site` and `rows` using the example's schema, without a `cells` field. Module and
site identities must match. The importer derives every cell and pin from the
mapped module and LEF, stores source hashes, rejects missing masters/pins,
multi-driver signals, nonzero macro origins, inconsistent widths/units and
physical constants or supply wiring without a suitable profile. Unsupported technology rules
are counted warnings rather than discarded signoff requirements. See
[real-PDK validation and limitations](docs/pdk-import.md) and
[import memory/work budgets](docs/pdk-import-budget.md).

## Physical I/O, ties and primary supply grid

Use a matching `fixtures/pdk/sky130-power-io.json` or `gf180-power-io.json` with
`import-pdk --physical-config <profile>`. The floorplan must leave space for the
two supply spines and I/O channels. Profiles bind exact PDK pin/master/via names,
minimum geometry and the insertion budget. `alternating_rows` generates N/FS
rows; FS cells must advertise LEF `SYMMETRY X`.

`place` fills row gaps, checks intrinsic primary-rail continuity, creates routed
SPECIALNETS and fixes I/O geometry on the boundary. `export-verilog` retains the
logical interface and inserted ties; generated physical-only fillers and taps
are omitted.
`export-verilog --include-power` includes fillers, supply ports and PG pin wiring
for physical connectivity checks. Both views are independently verified.

Profiles without a tap policy still connect body pins logically only. Complete
latch-up compliance, IR/EM and foundry DRC/LVS remain separate requirements. See
[physical preparation and verification](docs/power-io-integration.md) and
[resource limits](docs/power-io-budget.md).

## Well/substrate taps and clock topology

Use `fixtures/pdk/sky130-tap.json` or `gf180-tap.json` as the physical profile.
These bind actual tap and clock-buffer masters. Both configure a maximum 20 µm
horizontal gap between reserved tap-cell edges. The independent GDS checker
separately requires all inspected active polygons to be within 15 µm Manhattan
distance of a powered contact in the same body component. This is a conservative
engineering test policy, not a substitute for the complete foundry rule deck.

After `import-pdk` and `place`, add the configured clock-buffer topology:

```sh
target/release/rustroad clock-tree --workspace .work/connected --base-rev rev2
target/release/rustroad export-def --workspace .work/connected > .work/clock.def
```

The command accepts a scalar clock input and explicitly named sink pin. It
inserts bounded-fanout buffers, rebuilds physical placement and validates the
tree before publishing. Repeated `place` keeps the same result; changing the
clock topology requires a fresh import. That standalone command does not produce
propagated-clock, skew or RC results; the routing/extraction flow below adds those
checks. Read [the implementation guide](docs/tap-integration.md).

Run the combined, guarded, dual-PDK verification with installed dependencies:

```sh
free -h
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
/usr/bin/time -v -o .work/taps.time timeout 180s \
  python3 scripts/verify_taps.py --workspace .work/taps --base-rev empty
```

Local PDK paths and SHA256 pins are in `fixtures/pdk/local-taps.json`. Missing
tools/data cannot produce a passing result. KLayout is an external verification
dependency, not a Rust core dependency.

## Constrained CTS, physical routing and SPEF

`clock-tree --sdc fixtures/flow/cts.sdc` reads a strict primary-clock subset:
period/waveform, transition, uncertainty and explicit design limits.
Unsupported Tcl or timing semantics fail instead of being ignored. Routing uses
an explicit layer/via policy and finite search budgets:

```sh
rustroad clock-tree --workspace .work/chip --base-rev rev2 \
  --sdc fixtures/flow/cts.sdc
rustroad route --workspace .work/chip --base-rev rev3 --scope clock \
  --config fixtures/pdk/sky130-routing.json
rustroad route --workspace .work/chip --base-rev rev4 --scope all \
  --config fixtures/pdk/sky130-routing.json
rustroad export-def --workspace .work/chip > .work/routed.def
```

For the routed fixture, import with the matching `sky130-routed-physical.json`
or `gf180-routed-physical.json` profile. These leave two sites between logical
cells before filler insertion. This first router can fail on dense pin access
or its finite lattice; a failed route preserves HEAD. Re-placement invalidates
routes. Schema 5 stores checked geometry in `routing.rbin`.

The [CTS/routing report](docs/cts-routing.md) records actual results.
The small four-register pipeline routes all 19 signal nets on both PDKs, with
independent OpenDB/KLayout geometry, original-GDS tap checks, external OpenRCX
extraction, CEC and simulation. The six-register dense case remains outside this
router's demonstrated coverage. GF180 extraction uses its dedicated PDK rules,
not the technology LEF's explicitly unsuitable parasitic values.

Propagated-clock skew passes the configured limits, but the strict 1 ps
RustSTA/OpenSTA parity gate does not: differences and hold violations remain
visible. The verification command exits **3** for this partial result; it must
not be treated as complete timing closure. See
[the exact scope and reproduction steps](docs/cts-routing.md).

### GDSII streamout

`export-gds` is read-only and byte-deterministic. It copies the used PDK cell
structures verbatim, writes one structure per used fixed via and a top cell with
SREFs, DEF-semantics wires, supply geometry, labelled pins and the PR boundary.
`--merge-shapes rectangles|polygons` writes each net's metal per layer as an
exact cover of its union and `--vias flat` writes the vias' shapes with it
instead of via structures: the same geometry, cheaper for the foundry
DRC/antenna/LVS decks ([docs/gds-export.md](docs/gds-export.md)); the
defaults keep the legacy stream that `scripts/verify_gds.py` inspects:

```sh
rustroad export-gds --workspace .work/chip \
  --cell-gds <pdk>/libs.ref/sky130_fd_sc_hd/gds/sky130_fd_sc_hd.gds \
  --layer-map fixtures/pdk/sky130-gds-map.json --out .work/chip.gds
```

GF180 LEF coordinates (2000 DBU/um) are divided exactly onto the 0.001 um GDS
grid; an odd coordinate is an explicit error. On both PDKs the native stream is
XOR-identical to an independent KLayout LEF/DEF reference and gets the same
Magic/KLayout DRC counts and Netgen/KLayout LVS verdicts. Wires are written as
maximal collinear runs (the same runs `export-def` writes) and the router's
same-net patches are streamed. On the integrated LEF-rule flow both PDKs give
Magic 0 / KLayout 0 DRC items and LVS matches in both tools; the Sky130 block
still has one Magic metal3 antenna violation (clock input net) for the antenna
ECO. See [the GDS export guide](docs/gds-export.md) and
[the requalification report](docs/requalification.md).

## Native RC extraction

`extract` is read-only. It writes detailed SPEF for the routed HEAD revision,
atomically and outside the workspace, using a per-corner profile generated from
PDK data (`scripts/build_rc_profile.py`):

```sh
rustroad extract --workspace .work/chip --rc-profile fixtures/pdk/sky130-rc-max.json \
  --corner max --out .work/routed_max.spef
```

Routing patches add capacitance at the node of the conductor they overlap.
Pins, obstructions and supply metal that a routed wire crosses count their own
cut, and non-plate overlaps on other layers couple over their area. The SPEF
imports into RustSTA 0.3/0.4 (`wire_model=spef_rc`). The model, SPEF naming,
budgets and the three-way comparison (native, OpenRCX, Magic) at min/nom/max on
both PDKs are in [docs/extraction.md](docs/extraction.md), with a 3-D field
solver (`scripts/field_solver.py`) for wire crossings. The result is partial:
total and per-net C meet their targets against OpenRCX on pipeline.v; the
10 ps slack target is met on the hold-closed design at every corner (RustSTA
`09bf9f85` and `c407c742`, OpenRCX at its flow and lossless coupling
thresholds) but missed at GF180 ss+max on the pre-hold design (10.05 ps) and on
the larger regfile and mult designs; OpenRCX extracts no cell pin, obstruction
or via-pad metal, and min/max resistance follows different PDK data than
OpenRCX.

## Hold closure, CTS drive and antenna repair

`hold-eco`, `antenna-eco` and `apply-eco` are file-only typed netlist ECOs:
- a non-inverting buffer goes between one named sink pin and its net;
- an antenna diode is attached to a named net;
- new handles are shell-safe; no existing instance or Q-net is renamed.

`check-antenna` is a read-only LEF partial-net antenna check of the routed
HEAD. The clock profile may set per-level buffer masters (`clock.level_cells`).

`scripts/hold_closure.py` iterates up to four times:
ECO netlist -> place -> CTS -> route -> native extraction (min/nom/max) ->
RustSTA tt/ss/ff -> hold-eco.

On `pipeline.v` it closes hold >= +0.05 ns and setup >= 0 at tt+nom, ss+max
and ff+min on both PDKs:
- RustSTA/OpenSTA parity <= 1 ps on the native SPEF;
- the final post-ECO, post-CTS netlist passes RustSys CEC, Yosys CEC and
  512-vector simulation;
- native-GDS DRC is no worse than before the ECO;
- the planted antenna controls are flagged and then repaired, by both the
  native check and the deck.

See [docs/hold-closure.md](docs/hold-closure.md).

## Verification and research

- [REA/OpenROAD study and placement evidence](docs/openroad-blackbox.md)
- [Flow integration contract](docs/flow-integration.md)
- [Real-PDK connected flow](docs/pdk-import.md)
- [Architecture and next implementation gates](docs/architecture.md)
- [Chinese tap and clock-topology report](docs/tap-integration.md)
- [Chinese CTS/routing/extraction report](docs/cts-routing.md)
- [Native GDSII streamout and black-box acceptance](docs/gds-export.md)
- [Integration fixes and dual-PDK requalification on the native GDS](docs/requalification.md)
- [Hold closure, CTS drive selection and antenna repair](docs/hold-closure.md)
- [Chinese physical-preparation report](docs/power-io-integration.md)
- [Earlier PDK integration report](docs/pdk-import.md)
- [Earlier bootstrap report](docs/architecture.md)

Run the bounded placement workload after a release build:

```sh
/usr/bin/time -v -o .work/placement.time timeout 180s \
  python3 scripts/placement_benchmark.py --binary target/release/rustroad \
  --output .work/placement-benchmark --cells 50000
```

This measures import, placement, snapshot publication, mmap/export and the
independent checker. It compares one/two-thread output hashes; it does not claim
a speedup against OpenROAD. Real GPU/GUI and both baseline PDK physical flows
must pass their own gates before performance or signoff claims.

Reference tools are external executables over standard formats. No OpenROAD,
Yosys, ABC or OpenSTA implementation is copied, translated or inspected. REA
is used only for public-interface/process observations, with retained limitations.
