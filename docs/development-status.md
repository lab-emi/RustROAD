# Development status

Updated **2026-11-05**. The 2026-11-05 section reports the RustEDA host campaign
of that date; the rest of this page describes the source published with the
RustEDA development update. Use the superproject’s exact gitlinks and the
repository lockfile when reproducing a run.

## Ibex host campaign · 2026-11-05

The RustEDA Ibex host campaign of 2026-11-05 (IHP SG13G2 / EZ130 with two real
SRAMs, on a 4-vCPU Intel Xeon cloud host with 15.7 GiB RAM and no swap; a
different and slower machine than on 2026-11-02) ran RustROAD CLI 0.5.0
(commit [`63d492d`](https://github.com/lab-emi/RustROAD/commit/63d492d6dbd0034e0ef96ae6145b519498db5aa7)) in the repository's current native recipe under an 8 GiB
memory cap (+2 GiB memsw), and OpenROAD 26Q3 in LibreLane 3.0.17 Classic under
a 10 GiB cap, from the same prepared RTL, SDC and floorplan. The same
independent scripts checked both final layouts. **Both pass foundry main and
antenna DRC with 0 violations, latch-up tap coverage and full-device LVS; on
the shared timing gate the native layout has zero setup, hold, slew,
capacitance and fanout violations at TT and SS, while LibreLane's keeps 28 (TT)
and 24 (SS) capacitance and 30 fanout violations.** The native layout has less
standard-cell area, less routed wire and fewer vias, from a different netlist;
no area, wire or via ratio is formed, because LibreLane's layout fails a gate
and the receipt's ratios are therefore null.

The native run is `unqualified` (the component-qualification evidence is not
published). Routing, post-route repair, antenna repair and extraction ran at
`--threads 4`, frontend placement and CTS at `--threads 2`, and GDS export on
the default 2-thread pool. The producer completed all 114 manifest steps,
including its own foundry checks; a second producer run through `be_timing`
produced byte-identical GDS, DEF, netlists and native SPEF (the OpenRCX SPEF, written by OpenROAD,
differs). OpenROAD, OpenRCX and KLayout ran as external executables over
standard-format files; no OpenROAD implementation is copied or inspected.

### Place, CTS and route against OpenROAD · 2026-11-05

| Ibex final layout | RustROAD, native recipe | OpenROAD 26Q3, LibreLane Classic |
| --- | --- | --- |
| Standard-cell area | 187,585.2 µm² | 197,913.7 µm² |
| Routed signal wirelength | 897,357.3 µm | 969,571.6 µm |
| Vias | 118,525 | 122,194 |
| Standard cells / antenna diodes / taps / fillers | 14,058 / 52 / 5,934 / 34,322 | 15,191 / 504 / 6,900 / 44,672 |
| Foundry main / antenna DRC violations | 0 / 0 | 0 / 0 |
| Latch-up tap coverage (LU.a, LU.b) and full-device LVS | `passed`, `passed` | `passed`, `passed` |
| Slew / capacitance / fanout violations, TT and SS | 0 / 0 / 0 | 0 / 28 / 30 TT, 0 / 24 / 30 SS |
| Placement and CTS | 28.00 s | 19.69 s placement + 27.36 s CTS |
| Routing | 302.24 s at 4 threads, global and detailed | 83.34 s global routing and antenna checks/repair + 2,486.85 s detailed (4 threads) |
| Repair after routing | 7.34 s post-route + 18.99 s antenna repair and verification | None; its post-GRT repair runs before detailed routing |

LibreLane's 2,486.85 s of detailed routing ran on the same 4 vCPUs; its
repair before detailed routing (52.36 s, four resizer steps) sits beside the
native frontend STA and ECO stage (26.04 s) on the benchmark page.

### Extraction against OpenRCX · 2026-11-05

The replay sets RustROAD's `--threads` to each setting on the recorded
`extract` command, which now passes `--threads 4`; OpenRCX ran with
`set_thread_count` 2 and 4. One warm-up and three measured repetitions,
interleaved.

| | RustROAD 0.5.0 | OpenRCX |
| --- | ---: | ---: |
| Median wall, 2 threads (fastest) | 10.121 s (9.783 s) | 5.988 s (5.970 s) |
| Median wall, 4 threads (fastest) | 6.260 s (5.920 s) | 5.944 s (5.912 s) |
| CPU time per run, 2 / 4 threads | 17.76–18.94 s / 17.79–19.05 s | 5.91–6.45 s / 5.88–6.24 s |
| Worst peak RSS, 2 / 4 threads | 299,628 / 308,656 KiB | 211,480 / 212,012 KiB |
| SPEF nets | 14,178 | 14,178 |
| SPEF total capacitance / resistance | 162.695 pF / 2,752.769 kΩ | 153.653 pF / 2,769.870 kΩ |

At 4 threads the two are level (the ranges overlap); at 2 threads OpenRCX is
faster (RustROAD 1.69×). RustROAD uses about three times OpenRCX's CPU time and
1.42–1.46× its peak RSS; its total capacitance is 5.9 % above OpenRCX's and its total
resistance 0.6 % below.

### GDS export against KLayout · 2026-11-05

| | RustROAD `export-gds` | KLayout LEF/DEF streamout |
| --- | ---: | ---: |
| Median wall (fastest) | 0.831 s (0.799 s) | 1.938 s (1.730 s) |
| Worst peak RSS | 105,316 KiB | 207,728 KiB |
| Output size | 26,682,872 B | 25,363,292 B |

**RustROAD is faster:** KLayout takes 2.33× its median wall time. The recipe
now streams merged rectangles and flat vias (`--merge-shapes rectangles --vias
flat`), which the foundry checks read faster; the two GDS files are not
byte-equivalent and no geometric comparison was recorded.

### Shared gates on the routed layouts · 2026-11-05

Two shared gates are slower on the native layout, for different reasons. **OpenRCX extraction is slower on the native DEF**: in
interleaved same-load runs the shared `extraction` gate took a median 6.57 s
against 4.84 s on LibreLane's DEF (1.36×; ranges 6.35–6.74 and 4.58–5.04 s,
5 runs each), and the flow's own `be_openrcx` took 6.79 s against 5.557 s
for LibreLane's RCX step. Its time is consistent with the wire pieces it
extracts, which the route topology sets: the native DEF has 192,328 pieces against 129,901
(`scripts/def_pieces.py`), with 42,789 staircase and 39,398 zigzag pieces
against 25,925 and 17,576, although it has less wire and fewer vias. The
latch-up tap-coverage gate is slower on the native layout: median 50.54 s against 45.47 s (1.11×, ranges 49.58–52.77 and 44.51–46.74 s, 5 interleaved runs each); its time follows the notched n-well
domains of the flip-flop masters that synthesis chose
([tap integration](tap-integration.md)). Foundry main DRC, antenna DRC and
LVS are faster on the native layout (benchmark page).

## Ibex host campaign · 2026-11-02

The RustEDA host campaign reran Ibex with two real SRAMs on IHP SG13G2 / EZ130
on a 4-vCPU Intel Xeon cloud host (15.7 GiB RAM, cgroup v1, no swap). RustROAD
CLI 0.5.0 at commit
[`07e1626`](https://github.com/lab-emi/RustROAD/commit/07e1626f220ef1b7f3029ef827fae4b1491c3373),
the source described below, ran in the repository's published native recipe
(run16-class) under an 8 GiB memory cap (+2 GiB memsw). Routing ran at
4 threads (`be_route --threads 4`). The recipe's extraction and GDS export
commands pass no `--threads`, so they used RustROAD's default 2-thread pool.
The reference is LibreLane 3.0.17 Classic through `Checker.XOR`, with its
own post-GRT design repair, `DRT_THREADS=4` and a 10 GiB cap, on the nixpkgs
OpenROAD 26Q3 build instead of LibreLane's pinned OpenROAD `dcf36133`. Both
flows read the same prepared RTL, SDC and floorplan contract, and the same
independent gate scripts checked both final layouts. Each flow has one timed
sample. OpenROAD, OpenRCX and KLayout ran as external executables over
standard-format files; no OpenROAD implementation is copied or inspected.

The native run is `unqualified`: the 16-gate composer needs
component-qualification evidence that is not published. Its producer completed
every physical step, including routing, repair, export and both extractions,
and stopped after 114 of 119 manifest steps at the `timing_accuracy` gate,
where RustSTA and OpenSTA differ by 1.4052 ps on identical SPEF against a 1 ps
tolerance. The remaining gates ran separately with the recipe's scripts. Neither
flow passes every gate, so the receipt's end-to-end comparative ratios
(speed-up, standard-cell area, wirelength) are null. The wire and via values
below are the two layouts' measured values from different netlists; no wire or
via ratio is formed.

### Place, CTS and route against OpenROAD

Times and layouts come from the two complete flows. The netlists differ
(RustSys against Yosys + ABC), and so does the repair around each stage, so this
is not an identical-task comparison and no quotient is formed.

| | RustROAD, native recipe | OpenROAD 26Q3, LibreLane Classic |
| --- | --- | --- |
| Floorplan, PDN, taps, IO | No separate stage recorded | 10.56 s |
| Placement and CTS | 20.92 s, summed over the recipe's four frontend placement passes (three with CTS), including import and export steps | 16.30 s placement (global + detailed) + 20.17 s CTS |
| Repair before detailed routing | 64.65 s frontend STA, ECO rounds and planning helpers (RustSTA, Python) | 39.92 s resizer repair |
| Routing | 389.63 s (`be_route`, global and detailed) | 68.98 s global routing with antenna checks and repair + 1,796.83 s detailed routing |
| Repair after detailed routing | 179.00 s post-route repair + 409.35 s antenna repair and verification | None; its post-GRT repair runs before detailed routing |
| Standard-cell area | 233,909.2 µm² | 197,913.7 µm² |
| Final non-filler density (cap 70 %) | 33.25 % | 28.14 % |
| Standard cells / antenna diodes / taps / fillers | 16,208 / 52 / 13,923 / 55,651 | 15,191 / 504 / 6,900 / 44,672 |
| Routed nets | 16,329 | 15,227 |
| Routed signal wirelength | 1,679,076.6 µm | 969,571.6 µm |
| Vias | 301,099 | 122,194 |
| Foundry main DRC | `passed`, 0 | `passed`, 0 |
| Foundry antenna DRC | `passed`, 0 | `passed`, 0 |
| Full-device LVS | `passed` (top-port identity, 2 macros verified) | `passed` (top-port identity, 2 macros verified) |
| Slew / capacitance / fanout violations, TT | 0 / 0 / 0 | 0 / 28 / 30 |
| Slew / capacitance / fanout violations, SS | 0 / 0 / 0 | 0 / 24 / 30 |

Row scopes differ. The native 64.65 s includes the frontend RustSTA runs,
while LibreLane's OpenSTA runs (114.35 s, pre-, mid- and post-PnR) are not in
the table. LibreLane's 39.92 s holds all of its resizer repair, including
22.88 s of post-GRT design and timing repair (12.091 s + 10.792 s), all before
detailed routing. Its 68.98 s includes a 6.788 s antenna check that runs after
detailed routing.

**Routed wire and vias are the main gap.** The native layout has
1,679,076.6 µm of routed signal wire and 301,099 vias, against 969,571.6 µm and
122,194; no wire or via ratio is formed. The two layouts come from different netlists and placements (16,208 against 15,191
standard cells, 16,329 against 15,227 routed nets), so the gap is not the
router's alone. Standard-cell area is likewise a whole-flow result of the
different synthesized netlists and repair, and no area ratio is formed.
RustROAD's routing step takes 389.63 s against 1,796.83 s for OpenROAD's
detailed routing alone; with post-route and antenna repair the native routing
phase takes 977.98 s, against 1,865.81 s for LibreLane's global routing with
antenna checks and repair plus detailed routing. Wirelength counts regular
NETS only (SPECIALNETS, RECT patches and VIRTUAL jumps excluded) as Manhattan
segments between consecutive path points; a per-layer split was not computed.
Main DRC runs with the deck's recommended rules disabled for both layouts.
Timing violations come from the shared gate: an independent nominal OpenRCX
extraction analysed by RustSTA and OpenSTA, keeping the worse value.

### Extraction against OpenRCX

Both extract the same final native layout at the nominal corner, each with its
own RC model. RustROAD reads its final workspace, from which the flow's DEF was
exported, with the recipe's IHP RC profile. OpenRCX reads that exported DEF with
the tech, cell and SRAM LEFs and IHP's `IHP_rcx_patterns.rules`. The harness
replayed each contender at thread settings 2 and 4: OpenRCX through
`set_thread_count`, RustROAD through `RAYON_NUM_THREADS`. RustROAD sizes its
pool from `--threads`, which the recipe's extraction command does not pass, so
both RustROAD rows ran with the default 2-thread pool. Each contender had one
warm-up and three measured repetitions, interleaved.

| | RustROAD 0.5.0 (2-thread pool) | OpenRCX (OpenROAD 26Q3) |
| --- | ---: | ---: |
| Median wall, setting 2 (fastest) | 158.823 s (157.605 s) | 17.649 s (17.237 s) |
| Median wall, setting 4 (fastest) | 158.823 s (157.790 s) | 17.114 s (15.904 s) |
| Wall time per run | 157.605–161.041 s | 15.904–17.889 s |
| CPU time per run | 155.73–158.90 s | 15.70–17.69 s |
| Worst peak RSS, setting 2 / 4 | 810,452 / 810,632 KiB | 295,860 / 295,728 KiB |
| SPEF nets | 16,329 | 16,329 |
| SPEF total capacitance | 312.638 pF | 290.705 pF |
| SPEF total resistance | 6,633.54 kΩ | 6,691.924 kΩ |
| TT setup / hold worst slack, RustSTA on the flow's SPEF from this extractor | 49.279521 / 0.147036 ns | 49.296423 / 0.142664 ns |
| SS setup / hold worst slack, RustSTA on the flow's SPEF from this extractor | 48.840157 / 0.268527 ns | 48.868532 / 0.265683 ns |

The slack rows come from the producer's `timing_accuracy` check on the SPEF
files written during the flow, not from the replayed outputs above; the OpenRCX
SPEF is not byte-identical across runs.

**OpenRCX is faster and uses less memory.** RustROAD takes 9.0× OpenRCX's
median wall time at setting 2 and 9.3× at setting 4, with about 2.7× its peak
RSS. RustROAD's CPU time (155.73–158.90 s) tracks its wall time
(157.605–161.041 s), so in the recipe's configuration extraction is
effectively single-threaded: electrical arithmetic runs serially, and the
recipe leaves the opt-in candidate-query workers at their serial default (see
[extraction guide](extraction.md#strips-and-parallel-computation)).
Neither RustROAD's thread scaling nor that parallel query path was measured
here. In the flow itself, native extraction took 161.48 s and OpenRCX
18.02 s. RustROAD's total capacitance is 7.5 % above OpenRCX's and its total
resistance 0.9 % below.

### GDS export against KLayout

Both write GDS for the same final native layout. RustROAD's `export-gds` reads
its final workspace with the combined cell GDS and the recipe's JSON layer map.
KLayout 0.30.12 runs `scripts/signoff/kl_def2gds.py` on the exported DEF with
the tech LEF, the combined EZ130 + SRAM cell LEF, the combined cell GDS and
IHP's layer map, so its time includes DEF and LEF parsing. One warm-up and
three measured repetitions, interleaved.

| | RustROAD `export-gds` | KLayout LEF/DEF streamout |
| --- | ---: | ---: |
| Median wall (fastest) | 3.019 s (2.942 s) | 2.286 s (2.226 s) |
| Worst peak RSS | 379,668 KiB | 276,124 KiB |
| Output size | 48,157,358 B | 59,716,694 B |

**KLayout is faster:** RustROAD takes 1.32× its median wall time. The two GDS
files are not byte-equivalent, and no geometric comparison of them was
recorded, so this is a timing comparison only. The first KLayout replay failed
before reading the DEF and was rerun with a `.def` path.

### Reproduction scope

- **Run16, not R20/R21.** The repository publishes the run16-class recipe. The
  host layout reproduces the 2026-10-30 run16 layout: the same 233,909.2 µm²
  standard-cell area and 33.25 % density, 1,679,076.6 µm of wire against
  1,678,598.6 µm and 301,099 vias against 301,859 (run16 values from the
  [2026-10-30 receipt](https://lab-emi.github.io/RustEDA/benchmarks/#historical-matched-comparison-2026-10-30)).
  The R20/R21 recipe inputs of 2026-11-01 are not published, so this host
  could not reproduce that layout (below).
- **Determinism.** Two complete native runs on this host, a bring-up under
  shared load and the final run, produced byte-identical GDS, DEF, netlists and
  native SPEF. The OpenRCX SPEF differs between them.
- **Inputs.** Five of the seven EZ130 files that the contract pins are
  byte-identical; the SG13G2 tech LEF and the cell GDS are reconstructed from
  public sources, so this campaign does not reproduce the 2026-10-30 inputs.
- **Hardware.** Times come from 4 vCPUs and are not comparable with the
  16-logical-CPU i9-13900HX results below.

[Place, CTS and route vs OpenROAD](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-place-cts-and-route-vs-openroad) ·
[Extraction vs OpenRCX](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-extraction-vs-openrcx) ·
[GDS export vs KLayout](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-gds-export-vs-klayout) ·
[Host campaign, method and deviations](https://lab-emi.github.io/RustEDA/benchmarks/#host-campaign-method-and-deviations)

## Spacing-aware placement and bounded physical implementation

The development source combines bounded extraction indices and query workers,
single-session antenna repair, an opt-in staggered tap lattice, placement-stage
diagnostics and spacing-aware electrostatic density. The density model accounts
for the same cell spacing required by legalization. When the evaluation cap is
reached after a strictly improved exact-target checkpoint, the placer keeps that
checkpoint by default and reports `evaluation_horizon_after_checkpoint`;
`"stop_at_evaluation_cap": false` restores the strict fallback
([electrostatic placement](electrostatic-placement.md)). Repeated same-path
routing near misses receive bounded repair.

In the qualified Ibex layout of the R20/R21 runs (2026-11-01, 16 logical CPUs
of an i9-13900HX), standard-cell non-filler area is 204,053.9 µm², routed
signal wire is 1,190,522 µm and the via count is 168,381. Their recipe inputs
are not published, so the host campaign above ran the run16-class recipe
instead. The complete CPU flow with two real SRAMs records 472.2 s and
472.6 s, with 16/16 gates,
complete foundry main/antenna checks, device LVS and TT/SS timing. The additional
maximal deck contains 272 categories including LU.a; the main deck checks LU.b.
The qualifier requires the early checked GDS to equal the final GDS byte for byte.

The spacing-aware placer and diagnostic settings are opt-in. Dual-PDK physical
smokes cover Sky130 and GF180. OpenC910 physical closure, general macro flows
and GPU-integrated extraction remain unverified. The latest measurements use a
pinned campaign recipe, not every default CLI configuration.

[Whole-flow evidence and measurement scope](https://lab-emi.github.io/RustEDA/benchmarks/) ·
[RustEDA engine status](https://lab-emi.github.io/RustEDA/engine-status/)

## Publication checks

The isolated source passes 472 regular Rust tests (11 ignored), formatting and
all-target/all-feature Clippy. These checks do not replace the retained independent
PDK/whole-flow receipts or qualify a new design.
