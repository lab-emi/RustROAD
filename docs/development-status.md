# Development status

Updated **2026-11-13**. The first section reports the extraction change made
after the RustEDA host campaign of 2026-11-13, measured against the campaign's
binary in the same runs; the next reports that campaign and the 2026-11-05
section the campaign before it; the rest of this page describes the source
published with the RustEDA development update. Use the superproject’s exact
gitlinks and the repository lockfile when reproducing a run.

## Faster extraction · 2026-11-13

After the 2026-11-13 campaign, RustROAD
[`fe4b286`](https://github.com/lab-emi/RustROAD/commit/fe4b28610fa1d2ce728fa5456e0b1fa891d821f7)
(eight commits, `52bc4e9..fe4b286`) extracts the campaign's final Ibex layout
in **0.83× the campaign binary's time at 4 threads and 0.84× at 2**, with the
same SPEF. That makes it **level with OpenRCX at 4 threads**, the recipe's
setting (3.226 s against 3.014 s, 1.07×; the ranges overlap, so it is not
faster), but **still 1.85× slower at 2 threads** (5.315 s against 2.874 s),
with 3.4 times OpenRCX's CPU time and 1.6× its peak RSS (median 324,552
against 201,696 KiB at 4 threads). In the campaign it was 1.22× slower at
4 threads and 2.19× at 2 ([below](#extraction-against-openrcx-2026-11-13)).

**What changed.** Extraction was the one head-to-head RustROAD lost in the
campaign, so the eight commits take repeated work out of the extractor's
phases without changing its output. Grid cells by exact reciprocal division
(Lemire, Kaser and Kurz, 2019), compact grid entries and one fill pass instead
of twelve decode passes; chunk records grouped in one pass instead of a stable
sort per shard; candidate roles decided once per strip and kept in sets
updated at events, with 64-bit packed events; metal pieces planned per net in
parallel; interval state reused while it is unchanged; SPEF values formatted
without an allocation per value. The strips (coupling search and RC
accumulation) went from 2.83 s to 2.24 s of wall time at 4 threads in single
profiled runs.

| Same-load A/B, 7 runs after a warm-up, interleaved | Campaign binary | New binary | Competitor |
| --- | ---: | ---: | ---: |
| **Extraction** (final routed layout, nominal corner) | RustROAD `f90f2dd` | RustROAD `fe4b286` | OpenRCX |
| Median wall, 4 threads (range) | 3.870 (3.645–4.031) s | **3.226 (3.062–3.397) s** | 3.014 (2.838–3.333) s |
| Median wall, 2 threads (range) | 6.333 (6.063–6.595) s | **5.315 (5.164–5.461) s** | 2.874 (2.796–2.979) s |
| Median CPU, 4 threads (range) | 11.73 (11.15–12.15) s | 10.12 (9.58–10.62) s | 2.99 (2.81–3.30) s |
| Median CPU, 2 threads (range) | 11.47 (10.99–11.90) s | 9.71 (9.47–9.99) s | 2.86 (2.78–2.95) s |
| Median peak RSS, 4 / 2 threads | 319,616 / 310,932 KiB | 324,552 / 288,896 KiB | 201,696 / 201,276 KiB |

The new binary's wall ranges do not overlap with the campaign binary's at
either thread count; at 4 threads they overlap with OpenRCX's, so it is level
there, not faster. On this host the campaign binary takes 1.28× OpenRCX's
time at 4 threads (1.22× in the campaign, on the 2.10 GHz VM).

**How it was measured.** One rotation per repetition and thread count
(campaign binary, new binary, OpenRCX) on the 4-vCPU VM of the same-load gate
reruns (a Xeon reporting 2.30 GHz), after a further container restart, with
the host's measurement and build locks held for the whole run so that nothing
else ran. Each contender replays the command its flow recorded, under GNU
time. The campaign binary ran on the campaign's producer output, the new one
on the output of a full native run made with the new binaries (below). Each
RustROAD ran on a private copy of its own routed workspace, so that each
trusts its own validation receipt: a rebuilt extractor re-validates a
workspace whose receipt another build wrote, about 0.3 s at 4 threads, which
the flow never does because its own routing writes the receipt.

**Same results.** Every RustROAD run of both binaries wrote the same SPEF
(SHA-256 `468051be…`, the campaign's), and a new digest over every bit of the
extracted network (printed by `RUSTROAD_EXTRACT_PROFILE=1`) is the same at 1
to 4 threads. With the new RustROAD and RustSTA binaries, a full native run
(114 of 114 steps, the campaign's prepared inputs, recipe and executor)
produced a GDS, DEF, native SPEF, netlists and mapped netlist byte-identical
to the campaign's, and all nine shared gates passed with the campaign's
verdicts.

**What remains.** RustROAD's extraction is still slower than OpenRCX at
2 threads (1.85×) and not faster at 4: it spends 3.4 times OpenRCX's CPU time
(about 9.4 s single-threaded against 3.0 s), and at 4 threads about 1 s of its
3.3 s is spent outside the parallel strip phase (workspace load, metal
construction, grid index, trees, virtual crossings, finalization, SPEF write).
Its peak RSS is 1.6× OpenRCX's. The SPEF is unchanged, so its total
capacitance stays 6.0 % above OpenRCX's.

[RustSTA peak memory and RustROAD extraction](https://lab-emi.github.io/RustEDA/benchmarks/#ruststa-peak-memory-and-rustroad-extraction-2026-11-13) ·
[Runs and summary](https://lab-emi.github.io/RustEDA/benchmarks/evidence/ibex-host-20261113-followup/summary.json)

## Ibex host campaign · 2026-11-13

The RustEDA Ibex host campaign of 2026-11-13 (IHP SG13G2 / EZ130 with two real
SRAMs, on a 4-vCPU Intel Xeon cloud host reporting 2.10 GHz, with 15.7 GiB RAM
and no swap; a different virtual machine from 2026-11-05's) ran RustROAD CLI
0.5.0 (commit [`f90f2dd`](https://github.com/lab-emi/RustROAD/commit/f90f2dd805cbad1ba03dcc4fb84c2d36dfb4d5f7),
built with Rust 1.100.0) in the repository's current native recipe
(superproject `820ffc8`) under an 8 GiB memory cap (+2 GiB memsw), and
OpenROAD 26Q3 in LibreLane 3.0.17 Classic under a 10 GiB cap, from the same
prepared RTL, SDC and floorplan. The same independent scripts checked both
final layouts. **Both pass foundry main and antenna DRC with 0 violations,
latch-up tap coverage and full-device LVS; on the shared timing gate the
native layout has zero setup, hold, slew, capacitance and fanout violations at
TT and SS, while LibreLane's keeps 28 (TT) and 24 (SS) capacitance and
30 fanout violations.** The native layout has less standard-cell area, less
routed wire and fewer vias, from a different netlist; the receipt keeps its
ratio fields null because LibreLane's layout fails a gate; divided, the native
layout has 5.2 % less area, 7.7 % less wire and 1.3 % fewer vias. It keeps
less hold margin than LibreLane's (TT 0.108 ns against 0.172 ns); both meet
hold at both corners.

The native run is `unqualified` (the component-qualification evidence is not
published). As on 2026-11-05, routing, post-route repair, antenna repair and
extraction ran at `--threads 4`, frontend placement and CTS at `--threads 2`,
and GDS export on the default 2-thread pool. The producer's step graph ran
for the first time under the Rust executor `rusteda flow exec` instead of the
Python `producer.py` (the same manifest, steps and output files) and completed
all 114 manifest steps, including its own foundry checks; a second producer
run through `be_timing` produced byte-identical GDS, DEF, netlists and native
SPEF (the OpenRCX SPEF, written by OpenROAD, differs). OpenROAD, OpenRCX and
KLayout ran as external executables over standard-format files; no OpenROAD
implementation is copied or inspected.

What changed in RustROAD since 2026-11-05: the router's topology controls
(RustROAD [`d010664`](https://github.com/lab-emi/RustROAD/commit/d0106647b548a83917c91391a8a105bb78ea238b),
superproject `3cd3061`) price wrong-way wire higher (`wrong_way_factor` 6
instead of 3) with a bend surcharge (`bend_cost` 100), run a stronger
two-pass via reduction, rewrite walled-in same-layer chains jointly with the
nets that wall them in, move the turn at an escape top to the escape's via,
and declare the router's track grid in the DEF (`TRACKS` statements). The
placement and cells are 2026-11-05's; the route changed. The routed DEF went
from about 192 thousand wire pieces to 160 thousand, at the cost of 2,060
more vias (120,585 against 118,525), with 895,279.2 µm of wire against
897,357.3 µm. In that change's interleaved A/B, the shared OpenRCX gate on
the native layout went from 1.33× to 0.99× its time on LibreLane's
([routing budget](routing-budget.md#wire-pieces-against-vias-prices-joint-moves-escape-jogs-track-grid)).
The four Python backend helpers of the recipe (physical inputs, antenna
session, its review, post-route repair) became `rustroad` commands
([native flow steps](flow-steps.md)).

This host is a different virtual machine from 2026-11-05's (a Xeon reporting
2.10 GHz instead of 2.80 GHz). With the same LibreLane configuration, tool
builds and inputs, every LibreLane stage took less time on it (its flow
2,766.392 s against 3,409.108 s, detailed routing 0.81×), and LibreLane's DEF
and netlists are byte-identical to 2026-11-05's. Compare the two flows within
this campaign; the 2026-11-05 values quoted below are history, not a speedup.

### Place, CTS and route against OpenROAD · 2026-11-13

| Ibex final layout | RustROAD, native recipe | OpenROAD 26Q3, LibreLane Classic |
| --- | --- | --- |
| Standard-cell area | 187,585.2 µm² | 197,913.7 µm² |
| Routed nets | 14,178 | 15,227 |
| Routed signal wirelength | 895,279.2 µm | 969,571.6 µm |
| Vias | 120,585 | 122,194 |
| Standard cells / antenna diodes / taps / fillers | 14,058 / 52 / 5,934 / 34,322 | 15,191 / 504 / 6,900 / 44,672 |
| Foundry main / antenna DRC violations | 0 / 0 | 0 / 0 |
| Latch-up tap coverage (LU.a, LU.b) and full-device LVS | `passed`, `passed` | `passed`, `passed` |
| Slew / capacitance / fanout violations, TT and SS | 0 / 0 / 0 | 0 / 28 / 30 TT, 0 / 24 / 30 SS |
| Hold worst slack, TT / SS | 0.108 / 0.206 ns | 0.172 / 0.312 ns |
| Placement and CTS | 19.56 s | 17.88 s placement + 23.33 s CTS |
| Routing | 266.71 s at 4 threads, global and detailed | 75.68 s global routing and antenna checks/repair + 2,007.72 s detailed (4 threads) |
| Repair after routing | 4.74 s post-route + 8.93 s antenna repair and verification | None; its post-GRT repair runs before detailed routing |

Paired by phase, routing (global and detailed) takes 266.71 s against
2,083.40 s (7.8×) and placement and clock tree 19.56 s against 41.21 s
(2.1×); the netlists and the repair around each stage differ, so these are
not identical-task quotients. LibreLane's 2,007.72 s of detailed routing ran
on the same 4 vCPUs; its repair before detailed routing (48.36 s, four
resizer steps) sits beside the native frontend STA and ECO stage (13.85 s) on
the benchmark page. As history, on the other host of 2026-11-05: native
routing 302.24 s and placement and CTS 28.00 s, LibreLane's detailed routing
2,486.85 s.

### Extraction against OpenRCX · 2026-11-13

Both extract the final native routed DEF at the nominal corner. The replay
sets RustROAD's `--threads` to each setting on the recorded `extract` command
(the recipe passes `--threads 4`); OpenRCX ran with `set_thread_count` 2 and
4. One warm-up and three measured repetitions, interleaved.

| | RustROAD 0.5.0 | OpenRCX |
| --- | ---: | ---: |
| Median wall, 2 threads (fastest) | 7.951 s (7.662 s) | 3.625 s (3.460 s) |
| Median wall, 4 threads (fastest) | 4.664 s (4.644 s) | 3.829 s (3.641 s) |
| CPU time per run, 2 / 4 threads | 13.76–14.38 s / 13.78–14.04 s | 3.44–3.90 s / 3.60–3.89 s |
| Worst peak RSS, 2 / 4 threads | 287,428 / 324,408 KiB | 202,832 / 203,308 KiB |
| SPEF nets | 14,178 | 14,178 |
| SPEF total capacitance / resistance | 162.713 pF / 2,788.416 kΩ | 153.557 pF / 2,802.709 kΩ |

**In the campaign RustROAD's extraction was slower than OpenRCX on this
layout**: it took 1.22× OpenRCX's median wall time at 4 threads, the recipe's
setting, where the ranges do not overlap, and 2.19× at 2 threads, with about
3.7 times OpenRCX's CPU time and 1.60× its peak RSS. Its total capacitance is
6.0 % above OpenRCX's and its total resistance 0.5 % below. In the flow, the
native `be_extract` step took 5.27 s. On 2026-11-05, on another host and
layout, the two were level at 4 threads (1.05×, overlapping ranges); since
then OpenRCX's median fell from 5.944 s to 3.829 s (0.64×) and RustROAD's from
6.260 s to 4.664 s (0.75×). Since the campaign, RustROAD `fe4b286` takes
0.83× the campaign binary's time at 4 threads and 0.84× at 2, with the same
SPEF, in same-load runs on another VM: level with OpenRCX at 4 threads (1.07×,
ranges overlap) and still 1.85× slower at 2
([faster extraction](#faster-extraction-2026-11-13)).

### GDS export against KLayout · 2026-11-13

Both stream the final native DEF to GDS: RustROAD `export-gds` (the flow's
recorded `be_gds` command, merged rectangles and flat vias, on its default
2-thread pool) against KLayout 0.30.12 running
`scripts/signoff/kl_def2gds.py` with the tech LEF, cell LEF, combined cell GDS
and IHP layer map. One warm-up and three measured repetitions, interleaved.

| | RustROAD `export-gds` | KLayout LEF/DEF streamout |
| --- | ---: | ---: |
| Median wall (fastest) | 0.498 s (0.460 s) | 1.375 s (1.255 s) |
| Worst peak RSS | 104,212 KiB | 204,424 KiB |
| Output size | 24,992,568 B | 22,493,830 B |

**RustROAD is faster:** KLayout takes 2.76× its median wall time (2.33× on
2026-11-05, 0.831 s against 1.938 s, on the other host and layout) and about
twice its peak RSS. The two GDS files are not byte-equivalent and the receipt
records no geometric comparison, so this is a timing comparison only. The
native flow's GDS is larger than LibreLane's (24,992,568 B against
17,717,408 B), because the recipe streams merged rectangles and flat vias,
which the foundry checks read faster.

### Shared gates on the routed layouts · 2026-11-13

The receipt's gate times are one sample each, taken in the campaign. After
the campaign, both layouts went through each gate again, interleaved and at
the same load, through the campaign's own `run_signoff.sh --gates`. The
container was restarted during the first of these repetitions and came back
on another 4-vCPU VM (a Xeon reporting 2.30 GHz), so every same-load run
below ran there, both layouts alike, and its seconds are not the campaign's.
The first repetition of each gate and layout after the restart was a
cold-cache warm-up and is excluded. A verdict is `win` when the two ranges do
not overlap and `parity` when they do.

| Shared gate | Native layout, campaign s | LibreLane layout, campaign s | Same-load median, native / LibreLane s (n) | Ranges, native / LibreLane s | Native ÷ LibreLane | Verdict |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Foundry main DRC | 386.80 | 458.88 | 277.72 / 378.53 (3) | 271.75–282.41 / 356.66–397.08 | 0.73× | win |
| Foundry antenna DRC | 171.37 | 224.40 | 126.82 / 169.12 (3) | 122.11–141.58 / 164.88–193.08 | 0.75× | win |
| Full-device LVS | 120.42 | 168.51 | 93.31 / 135.40 (3) | 92.31–101.36 / 135.35–147.41 | 0.69× | win |
| Latch-up tap coverage | 5.94 | 7.75 | 4.88 / 6.39 (5) | 4.73–4.88 / 6.24–6.59 | 0.76× | win |
| OpenRCX extraction | 4.24 | 3.68 | 3.37 / 3.38 (5) | 3.17–3.98 / 3.02–3.43 | 1.00× | parity |
| RustSTA + OpenSTA timing | 21.46 | 20.93 | 17.02 / 17.32 (5) | 16.31–17.26 / 17.26–18.08 | 0.98× | parity |

Vendor-model simulation (0.73×) and final density (0.78×) are also faster on
the native layout; the equivalence gate has no valid comparison, because
LibreLane's synthesis and final equivalence do not complete (`unverified`).

**OpenRCX extraction of the native DEF is now level with LibreLane's**
(1.00×), where the shared gate took 1.36× on 2026-11-05 (6.57 s against
4.84 s): the native DEF holds fewer wire pieces since the route change above.
In the campaign's single samples it took 4.24 s against 3.68 s, and the flow's
own `be_openrcx` took 4.23 s against 4.541 s for LibreLane's RCX step (6.79 s
against 5.557 s on 2026-11-05). The gate gives no pass/fail verdict on either
layout (`extracted_not_qualified`); its row compares run time only. The
timing gate is level, not faster beyond noise. The latch-up gate ran the
shared checker of 2026-11-12, which changed for both layouts; with the
checker of 2026-11-05 it took 1.11× LibreLane's time on the native layout
([tap integration](tap-integration.md)).

[Place, CTS and route vs OpenROAD](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-place-cts-and-route-vs-openroad-2026-11-13) ·
[Extraction vs OpenRCX](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-extraction-vs-openrcx-2026-11-13) ·
[GDS export vs KLayout](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-gds-export-vs-klayout-2026-11-13) ·
[Shared gates](https://lab-emi.github.io/RustEDA/benchmarks/#shared-gates-2026-11-13) ·
[Where RustEDA still loses or is at parity](https://lab-emi.github.io/RustEDA/benchmarks/#where-rusteda-still-loses-or-is-at-parity-2026-11-13) ·
[Method and deviations](https://lab-emi.github.io/RustEDA/benchmarks/#host-campaign-method-and-deviations-2026-11-13)

## Ibex host campaign · 2026-11-05

The RustEDA Ibex host campaign of 2026-11-05 (IHP SG13G2 / EZ130 with two real
SRAMs, on a 4-vCPU Intel Xeon cloud host with 15.7 GiB RAM and no swap; a
different and slower machine than on 2026-11-02) ran RustROAD CLI 0.5.0
(commit [`1921481`](https://github.com/lab-emi/RustROAD/commit/1921481c5f0fde21b62971a3a21e245065c9bf90)) in the repository's current native recipe under an 8 GiB
memory cap (+2 GiB memsw), and OpenROAD 26Q3 in LibreLane 3.0.17 Classic under
a 10 GiB cap, from the same prepared RTL, SDC and floorplan. The same
independent scripts checked both final layouts. **Both pass foundry main and
antenna DRC with 0 violations, latch-up tap coverage and full-device LVS; on
the shared timing gate the native layout has zero setup, hold, slew,
capacitance and fanout violations at TT and SS, while LibreLane's keeps 28 (TT)
and 24 (SS) capacitance and 30 fanout violations.** The native layout has less
standard-cell area, less routed wire and fewer vias, from a different netlist;
the receipt keeps its ratio fields null because LibreLane's layout fails a gate;
divided, the native layout has 5.2 % less area, 7.4 % less wire and 3.0 % fewer vias.

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
latch-up tap-coverage gate was slower on the native layout in the campaign (median 50.54 s against 45.47 s, 1.11×),
because the checker's time followed the notched n-well domains of the flip-flop masters that synthesis chose;
with the shared checker of 2026-11-12, changed for both layouts, which certifies the same boxes and adds an
in-well LU.a path, it is faster on the native layout: median 5.70 s against 7.34 s (0.78×, ranges 5.51–6.04
and 7.05–7.61 s, 6 interleaved runs each; [tap integration](tap-integration.md)). Foundry main DRC, antenna DRC and
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
