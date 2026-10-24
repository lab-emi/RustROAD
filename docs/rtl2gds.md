# RTL-to-GDS gate (`scripts/rtl2gds.py`)

One command takes a design manifest from RTL to signed-off GDS on SKY130
(`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`) and checks the
result with independent oracles: synthesis, layout, hold and antenna repair,
extraction, multi-corner timing in two engines on two SPEF sources, foundry
DRC/LVS on the native GDS, equivalence and simulation against the RTL,
artifact identity, determinism and negative controls. It reuses the existing
engines (`hold_closure.Flow`, `compare_spef.openrcx`, `scripts/signoff`,
`check_cell_policy`) instead of copying them. Third-party tools run only as
black-box executables.

## Result

`fixtures/designs/pipeline.json` (`fixtures/flow/pipeline.v`, four flip-flops),
the RustROAD commit recorded in the evidence (`revision.head`, clean tree), RustSTA `c407c742` / rusteda `c8f71b63`
(RustSTA `78ea7cc`), evidence `docs/evidence/rtl2gds.json`:

| | SKY130 | GF180 |
|---|---|---|
| Gate | passed | **failed**: `timing_accuracy` only |
| Hold closure (native SPEF) | 3 iterations, 4 x `buf_1` | 2 iterations, 4 x `dlya_1` |
| Worst hold tt/ss/ff (RustSTA, native) | +0.0817 / +0.0667 / +0.1148 ns | +0.2445 / +0.3707 / +0.1962 ns |
| Worst hold tt/ss/ff (OpenSTA, OpenRCX) | +0.0758 / +0.0557 / +0.1115 ns | +0.2406 / +0.3615 / +0.1938 ns |
| Worst setup ss (both engines, both SPEFs) | >= 8.494 ns | >= 4.852 ns |
| MPW / min period | met, 24 MPW rows, no Liberty min period (0 rows in both engines) | met, 24 MPW + 12 period rows in each engine |
| DRV (transition / cap / fanout) | met at every corner, both engines | met at every corner, both engines |
| RustSTA vs OpenSTA, native SPEF | <= 0.31 ps | <= 0.46 ps |
| RustSTA vs OpenSTA, OpenRCX SPEF | <= 0.44 ps (MPW <= 0.57 ps) | tt/ff <= 0.38 ps; **ss+max 2.72 ps endpoints, 2.51 ps MPW** |
| XOR vs KLayout DEF reference | 0 (geometry and labels) | 0 |
| DRC Magic / KLayout | 0 / 0 | 0 / 0 |
| Antenna | Magic 0 (KLayout: no sky130 antenna deck) | KLayout 0 (Magic secondary 0) |
| LVS Netgen / KLayout (powered netlist) | match / match | match / match |
| CEC RTL vs mapped / final / powered | RustSys equivalent, Yosys proven (all three) | same |
| Co-simulation | 4096 cycles, 16384 port comparisons, 0 mismatches | same |
| Density (reported, not gated) | unverified (block smaller than the windowed deck) | failed (8 coverage rules, no fill) |
| Artifact binding / determinism | 53 files re-verified / 16 artifacts byte-identical | 52 / 15 |
| Negative controls | 10 of 10 rejected by their oracle | 10 of 10 |

The gate exits 1 because the only failing gating status is RustSTA-vs-OpenSTA
parity at GF180 ss+max on the OpenRCX SPEF. The worst rows are `y[0]` hold
(RustSTA 3.764303 ns, OpenSTA 3.767026 ns) and setup, `y[1]`, `r0/D` and
`r3/D` hold, all 2.6-2.7 ps, and the min-pulse-width slack of the flops'
clock pins (2.51 ps). This is the known `rr_clkbuf_2` slew difference
(RustSTA 0.5279 ns vs OpenSTA 0.5424 ns on net `rr_clock_2`) owned by the
RustSTA stream; nothing is special-cased for it. Every other gating status
passes on both PDKs, and the same layout timed with the native SPEF agrees
within 0.46 ps. The exact rows are in `timing_accuracy_failures` of the
summary.

Three complete gate runs (`.work/runs/final-c` is the evidence run; the
summaries and binding records of the other two are kept in
`.work/runs/keep-final-a` and `keep-final`), each with a `git archive` build of
the committed tree, gave byte-identical netlists, DEF, GDS and native SPEF to
each other and to a run with the worktree build; the OpenRCX SPEF differ only
in OpenRCX's wall-clock `*DATE` line.

## How to run

```sh
# Release binary (rule 9: about 0.7 GiB, 4 jobs)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  --setenv=CARGO_BUILD_JOBS=4 cargo build --release --locked --bins
# Private copy of the pinned RustSTA pair (the shared directory is replaced by other rounds)
mkdir -p .work/tools/sta-gate
cp /mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin/{rusteda,ruststa} .work/tools/sta-gate/
sha256sum .work/tools/sta-gate/*   # must equal fixtures/flow/rtl2gds-tools.json
# The gate (both PDKs, controls and determinism rerun included)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v -o gate.time \
  timeout 3600s python3 scripts/rtl2gds.py --design fixtures/designs/pipeline.json \
  --pdk sky130,gf180 --run .work/runs/NEW --base-rev empty [--evidence docs/evidence/rtl2gds.json]
# Rewrite the tracked evidence from a finished run only
python3 scripts/rtl2gds.py --evidence-from .work/runs/NEW/summary.json --evidence docs/evidence/rtl2gds.json
```

Options: `--rustroad BIN` (default `target/release/rustroad`), `--tools FILE`,
`--tool NAME=PATH` (override one pinned path; a missing or different binary
is never a pass), `--no-controls`, `--no-determinism` (both then report
`unverified`), `--preflight-only`. `--run` must not exist and `--base-rev`
must be `empty` (`REVISION_CONFLICT` otherwise); the run directory is locked
and its HEAD moves from `rev0` to `rev1`. Exit 0 = every gating status passed,
1 = at least one failed, 3 = none failed but at least one is unverified.

The podman images are the pinned signoff image (`fixtures/flow/signoff-pinned.json`,
`4c48415d8bd0`, verified by `scripts/signoff`) and the OpenROAD/OpenRCX image
`f152917ebc45` (the `openroad` executable is hashed inside the image at
preflight).

## Design manifest (`rustroad.design.v1`)

`fixtures/designs/pipeline.json` holds, each tracked file with its sha256:

- `top`, `rtl[]`, `clock.{port,name}`;
- `sdc.clock` (primary-clock SDC, also read by CTS), `sdc.io_template`
  (`fixtures/designs/postroute-io.sdc.tmpl`, filled from the RTL port list
  read by Yosys: `${inputs}`, `${data_inputs}`, `${outputs}`, `${clock}`) and
  `sdc.rendered_sha256` (the rendered signoff SDC is frozen; for the pipeline
  it is byte-identical to the SDC every earlier stream used);
- `pdk_profiles`, `cell_policy`, `signoff_manifest`, `tools`;
- per PDK: `floorplan` (tracked JSON), `physical_config`, `routing_config`,
  `delay_cells`, `gds_layer_map`, `rc_profiles.{min,nom,max}`, `corners`
  (must equal the closure engine's tt+nom / ss+max / ff+min) and `frozen`:
  the sha256 of every file the engine derives (Liberty per corner, LEFs, cell
  GDS, OpenRCX rules, profiles, rendered SDC). These are checked before and
  after the run (`inputs_frozen`);
- `verification`: co-simulation vectors and seed, RustSys budgets, Yosys
  `equiv_simple -seq`;
- `budgets`: hold margin 0.05 ns, setup margin 0, hold-ECO setup margin
  0.05 ns, parity 1 ps, at most 4 hold iterations, 120 s / 2 GiB address
  space per tool process, 4 GiB / 900 s per signoff container, 3600 s wall.

`fixtures/flow/rtl2gds-tools.json` pins RustSys (policy producer), rusteda,
ruststa, Yosys, Verilator, OpenSTA and the OpenROAD image by sha256.

## Steps and statuses

Every status is `passed`, `failed` or `unverified` and is reported
separately per PDK (`pdks[].statuses`). Gating statuses:

| Status | Producer | Independent oracle / check |
|---|---|---|
| `inputs_frozen` | manifest hashes before and after the run | sha256 |
| `implementation` | RustSys synth, RustROAD import/place/CTS/route, exports, OpenRCX, export-gds | the flow completed |
| `cell_policy` | mapped, final and powered netlists | `check_cell_policy.py` against the PDK exclusion lists |
| `cec_prelayout` | RTL vs mapped netlist | RustSys `check synth` and Yosys `equiv_simple -seq 2` |
| `hold_closure` | `hold_closure.Flow.close()`, native SPEF decisions, <= 4 iterations | RustSTA hold >= 0.05 ns and setup >= 0 at tt/ss/ff (hold-eco keeps an estimated ss setup margin of 0.05 ns) |
| `antenna_native` | `check-antenna` on the final layout | conservative native model (margin 0.10) |
| `extraction` | native min/nom/max + OpenRCX on the exported DEF | each SPEF sha256 equals its producer's receipt |
| `timing_setup` | RustSTA and OpenSTA, native and OpenRCX SPEF | setup >= 0 at tt and ss |
| `timing_hold` | same | hold >= +0.05 ns at tt, ss and ff |
| `mpw_min_period` | RustSTA default rules (`groups=worst, pins=all`) gate | OpenSTA met; min-period rows may be absent only if both engines have none |
| `drv` | RustSTA `drv_status` and limits (every kind checked at every corner) | OpenSTA `report_check_types`: no violator |
| `timing_accuracy` | RustSTA vs OpenSTA per corner per SPEF | endpoint slacks <= 1 ps, same endpoints; MPW/period (`--pulse-groups last --pulse-pins clock-ends`) <= 1 ps; equal DRV violator sets |
| `gds_xor` | native GDS vs KLayout DEF reference streamout | strmxor (geometry) + label lists |
| `drc_magic`, `drc_klayout` | native GDS | Magic `drc(full)`, KLayout main deck: 0 items |
| `antenna_deck` | native GDS | sky130 Magic, gf180 KLayout (the other tool is reported) |
| `lvs_netgen`, `lvs_klayout` | native GDS vs powered netlist | Netgen, KLayout (own results, port names checked) |
| `cec_final` | RTL vs final (post-CTS/ECO) netlist | RustSys and Yosys |
| `cec_powered` | RTL vs powered netlist | RustSys with `--supply-port VDD/VSS` after removing the listed physical-only cells; Yosys on the unpowered view |
| `simulation` | generic random co-simulation (Verilator) | cycles, comparisons and end time must equal the expected counts |
| `cell_proofs` | inserted hold and clock cells | Yosys sat proof per corner, wrong polarity rejected, native ECO refuses an inverter |
| `artifact_binding` | sha256 of every artifact, tool and input + RustROAD revision | re-hashed after every oracle and control |
| `determinism` | a second full build in `<pdk>_rerun/` | byte compare (OpenRCX `*DATE` line removed, listed) |
| `controls` | ten planted defects | each rejected by its own oracle |

Reported only: `density` (`density_status`; owner decision: not part of the
block DRC verdict). Global statuses: `tools_identity`, `openroad_image`,
`design_manifest`, `sdc_rendering`. `timing_accuracy_status` is also given at
the top level together with `timing_accuracy_failures` (every failing
corner/check with its worst rows).

Scope (`scope` in the summary): block/macro level, no pad or seal ring; IR
drop, electromigration, signal integrity (couplings grounded), OCV and CPPR
are `unchecked`; density reported separately; clocks propagated through the
RustROAD clock tree.

### Logic oracles (W10)

- **Powered netlist.** Instances whose master matches the cell policy's
  `physical_only` globs and has no LEF `OUTPUT` pin are removed and listed
  (sky130: 123 = 32 `tapvpwrvgnd_1` + 91 fillers; gf180: 119 = 32 `filltie` +
  87 fillers). A policy glob matching a cell with an output pin is refused,
  not filtered. RustSys keeps the supply connections and gets the supply
  ports; Yosys gets an unpowered view (68 supply-pin connections and the two
  supply ports removed, listed).
- **Co-simulation.** The testbench is generated from the RTL port list: all
  non-clock inputs come from a fixed-seed xorshift64, the clock port toggles,
  and every output is compared with `!==` before and after each edge (4096
  cycles). Verilator 5.032's default optimisation constant-folds variables
  that an `initial` block assigns before its first delay (a counter read 0
  after 4096 increments), which made a first version vacuous. The testbench
  now initialises everything in declarations, is built with `-O0`, prints
  cycles/comparisons/time, and the gate requires the expected counts; the
  bad-RTL control must make it report mismatches.

### Negative controls (both PDKs, final run)

| Control | Planted defect (copy, never a bound artifact) | Oracle that rejected it |
|---|---|---|
| `drc_spacing` | two met1/Metal1 rectangles at 0.10/0.12 um | Magic (met1.2 x2 / M1.2a) and KLayout (m1.2 / M1.2a) |
| `label_swap` | labels `en` and `a[0]` exchanged | Netgen (pin matching), KLayout (swapped pin names), XOR labels |
| `removed_via` | the `L1M1_PR` / `Via1_HV` SREF on two-terminal net `y[1]` deleted | Netgen and KLayout: `y[1]` unpaired |
| `removed_hold_cell` | the hold chain of the worst iteration-0 sink removed, fresh layout | RustSTA/OpenSTA ss hold -0.0898 / -0.0899 ns (sky130), -0.7944 / -0.7945 ns (gf180) |
| `tampered_sdc` | `set_input_delay 0.2` -> `0.6` in the rendered SDC | frozen-input sha256 (`sdc_postroute`) |
| `tampered_spef` | every R of the native ss SPEF x1000 (229 / 210 resistors) | SPEF sha256 vs extraction receipt; re-timing moves ss setup 8.50 -> 7.02 ns (sky130) |
| `bad_rtl` | wrapper inverting `y[0]` around the renamed top | RustSys `not-equivalent`, Yosys 1 unproven `$equiv`, co-simulation mismatches |
| `missing_tool` | `--tool yosys=<absent>` | exit 3, `tools_identity` unverified (`yosys: missing`) |
| `tampered_artifact` | one GDS byte flipped; separately one recorded hash edited | binding re-verification names `gds`; digest mismatch |
| `antenna_planted` | 600 um met3 / 2000 um Metal3 wire on `b[1]` | native 810.5 / 867.2 > 360; Magic metal3 x1 / KLayout ANT.16_ii_ANT.4 x6 |

The planted antenna wire also re-measures the native margin: native/(1-0.10)
= 900.6 vs Magic 856.2 (sky130) and 963.5 vs Magic 883.1 (gf180), so the
conservative native verdict covers the deck on both.

## Hold and antenna ECO hardening

Detailed in `docs/hold-closure.md` (sections "Report binding", "Sink pins",
"Safety margin"); summary:

- `apply-eco`/`hold-eco` refuse an `insert_buffer` sink that is not a cell-LEF
  `INPUT` pin (`ECO_INVALID`); the phase-B1 `r0/Q` plan is refused.
- `hold-eco` binds reports to `--timed-netlist` (and its recorded sha256),
  to one STA revision, row by row and stage by stage, and checks that the
  edited netlist is refined by the timed netlist; stale reports fail
  `STALE_REPORT`.
- `check-antenna` keeps a derived 0.10 margin and `antenna-eco` sizes diodes
  against it; a `partial` input is carried forward as `partial`.
- File-only commands report their own codes (`STALE_REPORT`, `ECO_APPLY`,
  `TIMING_REPORT`, `OUTPUT_PATH`, ...), never `INVALID_WORKSPACE`.

Tests: `crates/rustroad-formats/src/eco.rs`
(`output_inout_or_unknown_sink_pins_are_refused`),
`crates/rustroad-workspace/tests/hold_eco.rs`
(`apply_eco_refuses_an_output_pin_sink_and_a_foreign_cell_lef`,
`stale_reports_are_refused_with_their_own_code`,
`cts_buffered_timed_netlist_refines_the_mapped_netlist`,
`antenna_eco_carries_a_partial_input_status_forward`,
`antenna_eco_sizes_diodes_against_the_safety_margin`) and
`crates/rustroad-workspace/tests/antenna.rs`
(`safety_margin_flags_ratios_just_below_the_lef_limit`). A regression run of
`scripts/hold_closure.py --signoff --openrcx --antenna-controls` passes and
reproduces every netlist, DEF and native SPEF of the run before the change
byte for byte.

Checks on the branch: `cargo test --release --workspace` 153 passed, 4
ignored (22 s, 730 MiB); `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean; `python3 -m unittest discover -s scripts/tests`
65 tests (`scripts/tests/test_rtl2gds.py` covers the status algebra, SDC
rendering, binding, determinism compare, powered-netlist views, bad-RTL
wrapper, testbench generator, OpenSTA parser, parity and the controls'
netlist/DEF/SPEF edits).

## Resources (rule 9)

Memory estimate before the runs: the pipeline has < 150 instances and 25
nets; every tool runs alone (one process at a time), so the peak is the
largest single process: a signoff container (KLayout/Magic/Netgen, capped at
4 GiB, measured < 0.5 GiB). Tool processes run under a 2 GiB address-space
limit and a 120 s timeout, the gate itself under 8 GiB, the whole command in
a `systemd-run` scope with MemoryMax=8G.

Measured (evidence run `.work/runs/final-c`, both PDKs): **121.2 s** end to
end (`/usr/bin/time`: 2:01.3), 290 guarded processes, peak RSS **475,012 KiB**
(a sky130 signoff container in the controls); the largest host process was
the Verilator co-simulation build (288,080 KiB). The two earlier complete runs
took 109.8 s and 128.4 s with the same results. Per step (wall s / peak RSS
KiB):

| Step | SKY130 | GF180 |
|---|---|---|
| synthesis | 0.14 / 61,632 | 0.28 / 103,904 |
| place, CTS, route, closure | 3.33 / 120,760 | 2.34 / 231,020 |
| OpenRCX (3 corners) | 1.26 / 126,684 | 1.54 / 136,012 |
| GDS export | 0.03 / 17,744 | 0.02 / 11,628 |
| CEC pre-layout / final / powered | 0.17 / 0.16 / 0.18 (<= 62,788) | 0.25 / 0.25 / 0.25 (<= 107,452) |
| timing (2 SPEFs x RustSTA + 6 OpenSTA) | 1.98 / 120,532 | 3.09 / 231,068 |
| signoff (reference streamout + 7 checks) | 13.21 / 474,888 | 16.73 / 446,292 |
| co-simulation | 1.82 / 287,884 | 1.70 / 288,080 |
| cell proofs | 0.74 / 61,500 | 0.93 / 79,344 |
| determinism rerun | 4.70 / 120,740 | 4.08 / 230,880 |
| controls (10) | 27.11 / 475,012 | 33.65 / 448,104 |

## Open issues

- RustSTA vs OpenSTA at GF180 ss+max on the OpenRCX SPEF: 2.72 ps endpoint,
  2.51 ps MPW (RustSTA stream). The gate fails on it by design.
- Density is reported only: GF180 fails 8 coverage rules without fill; the
  sky130 windowed deck cannot evaluate a block smaller than its window.
- The co-simulation starts both models from Verilator's zero state; a mapping
  that encodes a register inverted would be reported as a mismatch (fail
  safe), not proven. Sequential equivalence is proven by the CEC oracles.
- One-bit vector ports (`[0:0] x`) are treated as scalars by the port reader.
