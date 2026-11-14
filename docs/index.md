# RustROAD

RustROAD implements placement, clock trees, routing, extraction and GDS export. The recorded small-block baseline spans SKY130 HD and GF180 7T: **15 ordinary passes and one declared deviation**. OpenC910 physical implementation remains unverified.

[All RustEDA modules](https://lab-emi.github.io/RustEDA/)

<picture>
<source media="(prefers-reduced-motion: reduce)" srcset="assets/rustroad-banner.svg">
<img src="assets/rustroad-banner.gif" alt="RustROAD implements placement, clock trees, routing, extraction and GDS export. The recorded small-block baseline spans SKY130 HD and GF180 7T: **15 ordinary passes and one declared deviation**. OpenC910 physical implementation remains unverified." width="1200">
</picture>

- [Physical flow](rtl2gds.md)
- [Architecture](architecture.md)
- [Current qualification](closeout-2026-10-25.md)
- [PDK import](pdk-import.md)
- [Timing and extraction](postroute-timing.md)
- [Native flow steps](flow-steps.md)
- [Signoff scope](signoff.md)


## RustROAD vs OpenROAD at a glance

Ibex RTL-to-GDS on IHP SG13G2 with EZ130 cells and two SRAM macros, 2026-11-13: RustEDA and LibreLane 3.0.17 (OpenROAD, Yosys, KLayout, Magic, Netgen) ran one after the other on the same 4-vCPU host from the same prepared inputs, and the same signoff scripts checked both layouts. Every RustEDA engine was built with Rust 1.100.0, and RustEDA's step graph ran under its Rust executor, `rusteda flow exec`.

- **Routing 7.8× faster than OpenROAD** (266.71 s against 2,083.40 s, global and detailed), **placement and clock tree 2.1× faster** (19.56 s against 41.21 s), and **export 159× faster** than LibreLane's streamout steps (1.67 s against 266.29 s).
- **5.2 % less standard-cell area, 7.7 % less routed wire and 1.3 % fewer vias**, with 52 antenna diodes against 504 and **zero timing violations** against LibreLane's 112.
- GDS export **2.8× faster than KLayout** with **2.0× less peak memory**. RustROAD's own extraction, after the campaign (RustROAD [`8d29a61`](https://github.com/lab-emi/RustROAD/commit/8d29a61f82b47c9cb26726134d8eec24823ae359)), was **faster than OpenRCX in all 15 runs at 4 threads** (median 2.776 s against 3.047 s, 0.91×) but the ranges overlap, so it is **level there, not faster beyond noise**, and it is **still 1.64× slower at 2 threads** (4.991 s against 3.049 s), with 1.3× its peak memory and the same SPEF (in the campaign: 1.22× slower at 4 threads, 2.19× at 2).
- Foundry DRC, antenna DRC, LVS and the latch-up check run **1.4×, 1.3×, 1.5× and 1.3× faster** on RustROAD's layout (same scripts, same load), and OpenRCX now extracts its DEF in the time it takes on LibreLane's (1.00×, level). The whole RustEDA flow finishes **4.4× sooner end to end** (751.54 s against 3,283.05 s).

LibreLane's layout fails the shared timing gate (112 maximum-capacitance and fanout violations at TT and SS) and its equivalence check does not complete, so the receipt keeps its own ratio fields null under its publication rule; the multiples here divide the recorded times. Stages are paired by phase, not as identical tasks. Against the three losses of 2026-11-05, in same-load runs: repair before global routing now takes 0.73× the time of LibreLane's two pre-route resizer steps (ranges do not overlap; 1.18× on 2026-11-05), OpenRCX on RustROAD's layout is level with its time on LibreLane's (1.00×, ranges overlap; 1.36× on 2026-11-05) and the latch-up check on RustROAD's layout takes 0.76× its time on LibreLane's (1.11× with the checker of that campaign). RustEDA is still not faster or not better everywhere: RustROAD's standalone extraction took 1.22× OpenRCX's time at 4 threads in the campaign (2.19× at 2) and, since RustROAD `8d29a61` after the campaign, is level at 4 threads (0.91×, faster in all 15 runs but with overlapping ranges) and 1.64× slower at 2; the timing gate is level (0.98×, ranges overlap) and RustROAD's layout keeps less hold margin (TT 0.108 ns against 0.172 ns; both meet hold). RustSTA's peak memory at 4 threads, 11.8 % above OpenSTA's in the campaign, is 12.6 % below it since RustSTA `de210a6` ([after the campaign](https://lab-emi.github.io/RustEDA/benchmarks/#ruststa-peak-memory-and-rustroad-extraction-2026-11-13)). [All stages, gates and losses](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-13)

## PULPissimo benchmark scope

The [PULPissimo benchmark](https://lab-emi.github.io/RustEDA/benchmarks/pulpissimo/) is synthesis-only and does not time RustROAD. It compares the PULPissimo Ibex core with a bounded Yosys 0.70/ABC reference on SKY130 HD; no placement, clock tree, routing, extraction or GDS stage is part of it. On SKY130 the full PULPissimo SoC still exceeds a 2 h synthesis budget with behavioral L2 memory; an IHP SG13G2 / EZ130 synthesis-only test with 80 official SRAM macros completes in 426 s. Neither is a physical signoff result.

## Faster extraction, second round · 2026-11-14

After the 2026-11-13 follow-up, RustROAD
[`8d29a61`](https://github.com/lab-emi/RustROAD/commit/8d29a61f82b47c9cb26726134d8eec24823ae359)
(seven extraction commits and a docs commit, `fe4b286..8d29a61`) extracts the
campaign's final Ibex layout in **0.71× the campaign binary's time at 4 threads
and 0.75× at 2** (0.85× and 0.90× of `fe4b286`'s), with **18.6 % less peak
memory than `fe4b286`** at 4 threads and the same SPEF. Against OpenRCX it was
**faster in all 15 runs at 4 threads** (median 2.776 s against 3.047 s, 0.91×),
but the ranges overlap (2.665–3.086 s against 2.861–3.741 s), so it is level
there, not faster beyond noise; at 2 threads it is **still 1.64× slower**
(4.991 s against 3.049 s), with 3.2 times OpenRCX's CPU time and 1.31× its peak
RSS.

**What changed.** The seven commits cut serial work around the parallel
strip phase and release memory earlier, without changing the output. In single
profiled runs at 4 threads: the grid index is built by row bands in parallel, a
parallel counting sort (Zagha and Blelloch, 1991) in which each band owns
disjoint slices of the offsets and entries (0.22–0.27 s to 0.07 s); entries in
a grid cell ascend by lower edge, so a scan leaves the cell at the first entry
above the query (user CPU −3 %); small couplings are grounded through one flat
node array (finalization 0.09–0.11 s to 0.07 s); net-tree records are sorted in
parallel and metal pieces go only to the lists read later (metal construction
0.17 s to 0.12 s); crossed supply rails and other retained passive conductors
weigh 16 units in the strip chunks, which removes a serial barrier of
0.08–0.09 s at the end of the strip phase; and once the extraction input is
built, the workspace snapshot is freed except for the routes, the design's
cells and the connectivity, which takes the peak RSS from 294–332 MB to
263–270 MB and makes it steady.

| Same-load A/B, 15 runs after a warm-up, order rotated | Campaign binary | Previous | New binary | Competitor |
| --- | ---: | ---: | ---: | ---: |
| **Extraction** (final routed layout, nominal corner) | RustROAD `f90f2dd` | RustROAD `fe4b286` | RustROAD `8d29a61` | OpenRCX |
| Median wall, 4 threads (range) | 3.894 (3.639–4.313) s | 3.267 (3.089–3.699) s | **2.776 (2.665–3.086) s** | 3.047 (2.861–3.741) s |
| Median wall, 2 threads (range) | 6.659 (6.297–7.180) s | 5.553 (5.337–5.862) s | **4.991 (4.786–5.415) s** | 3.049 (2.902–3.370) s |
| Median CPU, 4 threads (range) | 11.84 (11.08–13.31) s | 10.32 (9.68–11.20) s | 9.76 (9.28–10.65) s | 3.03 (2.84–3.71) s |
| Median CPU, 2 threads (range) | 12.01 (11.37–13.00) s | 10.23 (9.78–10.78) s | 9.50 (9.14–10.29) s | 3.03 (2.89–3.35) s |
| Median peak RSS, 4 threads (range) | 322,356 (294,380–325,824) KiB | 325,740 (298,844–331,192) KiB | **265,084 (262,936–270,052) KiB** | 201,736 (201,560–202,204) KiB |
| Median peak RSS, 2 threads (range) | 312,836 (287,540–314,364) KiB | 288,312 (286,504–290,560) KiB | **251,464 (250,592–252,804) KiB** | 201,400 (201,052–202,024) KiB |
| Runs in which `8d29a61` was faster, 4 / 2 threads | 15 / 15 of 15 | 15 / 14 of 15 | — | 15 / 0 of 15 |

The new binary's wall ranges do not overlap with the campaign binary's at
either thread count, nor with `fe4b286`'s at 4 threads (by 3 ms); at 2 threads
they overlap with `fe4b286`'s, which was faster in one run. Its peak RSS ranges
are apart from both. On this host the campaign binary takes 1.28× OpenRCX's
time at 4 threads, as in the previous follow-up.

**What remains.** RustROAD's extraction still spends 3.2 times OpenRCX's CPU
time (9.5 to 9.8 s against 3.0 s), so it is 1.64× slower at 2 threads and only
level at 4. At 4 threads about 2.1 s of its 2.8 s is the parallel strip phase,
evenly loaded, with a flat profile (interval sweep 31 %, candidate queries and
gathering 17 %, record grouping 6.5 %), and about 0.75 s is spent outside it
(workspace read 0.12 s, net trees 0.13 s, metal construction 0.12 s, virtual
crossings 0.08–0.15 s, and smaller phases). Its peak RSS is 1.31× OpenRCX's at
4 threads; at the peak about 73 MiB are snapshot remainder and input, 55 MiB
coupling accumulators, 49 MiB grid, 45 MiB net trees and 35 MiB retained
shapes. The SPEF is unchanged, so its total capacitance stays 6.0 % above
OpenRCX's.

[Method and identity](development-status.md#faster-extraction-second-round-2026-11-14) ·
[RustROAD extraction, second round](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-extraction-second-round-2026-11-14)

## Faster extraction · 2026-11-13

After the 2026-11-13 campaign, RustROAD
[`fe4b286`](https://github.com/lab-emi/RustROAD/commit/fe4b28610fa1d2ce728fa5456e0b1fa891d821f7)
(eight commits, `52bc4e9..fe4b286`) extracts the campaign's final Ibex layout
in **0.83× the campaign binary's time at 4 threads and 0.84× at 2**, with the
same SPEF. That makes it **level with OpenRCX at 4 threads**, the recipe's
setting (3.226 s against 3.014 s, 1.07×; the ranges overlap, so it is not
faster), but **still 1.85× slower at 2 threads** (5.315 s against 2.874 s),
with 3.4 times OpenRCX's CPU time and 1.6× its peak RSS. In the campaign it
was 1.22× slower at 4 threads and 2.19× at 2 (below). The eight commits take
repeated work out of the extractor's phases (grid cells and grid build, chunk
grouping, candidate roles per strip, metal pieces planned per net in
parallel, reused interval state, SPEF value formatting); the strips went from
2.83 s to 2.24 s of wall time at 4 threads in single profiled runs.

| Same-load A/B, 7 runs after a warm-up, interleaved | Campaign binary | New binary | Competitor |
| --- | ---: | ---: | ---: |
| **Extraction** (final routed layout, nominal corner) | RustROAD `f90f2dd` | RustROAD `fe4b286` | OpenRCX |
| Median wall, 4 threads (range) | 3.870 (3.645–4.031) s | **3.226 (3.062–3.397) s** | 3.014 (2.838–3.333) s |
| Median wall, 2 threads (range) | 6.333 (6.063–6.595) s | **5.315 (5.164–5.461) s** | 2.874 (2.796–2.979) s |
| Median CPU, 4 threads (range) | 11.73 (11.15–12.15) s | 10.12 (9.58–10.62) s | 2.99 (2.81–3.30) s |
| Median CPU, 2 threads (range) | 11.47 (10.99–11.90) s | 9.71 (9.47–9.99) s | 2.86 (2.78–2.95) s |
| Median peak RSS, 4 / 2 threads | 319,616 / 310,932 KiB | 324,552 / 288,896 KiB | 201,696 / 201,276 KiB |

Every run of both binaries wrote the same SPEF (the campaign's), a new digest
over every bit of the extracted network is the same at 1 to 4 threads, and a
full native run with the new RustROAD and RustSTA binaries produced a GDS,
DEF, native SPEF and netlists byte-identical to the campaign's, with all nine
shared gates passing with the campaign's verdicts. **What remains:** it is
still 1.85× slower than OpenRCX at 2 threads and not faster at 4; it spends
3.4 times OpenRCX's CPU time, about 1 s of its 3.3 s at 4 threads is spent
outside the parallel strip phase, and its peak RSS is 1.6× OpenRCX's.

[Tables, method and identity](development-status.md#faster-extraction-2026-11-13) ·
[RustSTA peak memory and RustROAD extraction](https://lab-emi.github.io/RustEDA/benchmarks/#ruststa-peak-memory-and-rustroad-extraction-2026-11-13)

## Ibex host campaign · 2026-11-13

The RustEDA Ibex host campaign of 2026-11-13 (IHP SG13G2 / EZ130 with two real
SRAMs, on a 4-vCPU Intel Xeon cloud host reporting 2.10 GHz, with 15.7 GiB RAM
and no swap; a different virtual machine from 2026-11-05's) ran RustROAD CLI
0.5.0 (commit [`f90f2dd`](https://github.com/lab-emi/RustROAD/commit/f90f2dd805cbad1ba03dcc4fb84c2d36dfb4d5f7),
built with Rust 1.100.0) in the repository's current native recipe under an
8 GiB memory cap (+2 GiB memsw), with the step graph run by the Rust executor
`rusteda flow exec`, and OpenROAD 26Q3 in LibreLane 3.0.17 Classic under a
10 GiB cap, from the same prepared RTL, SDC and floorplan. The same
independent scripts checked both final layouts. **Both pass foundry main and
antenna DRC with 0 violations, latch-up tap coverage and full-device LVS; on
the shared timing gate the native layout has zero setup, hold, slew,
capacitance and fanout violations at TT and SS, while LibreLane's keeps 28 (TT)
and 24 (SS) capacitance and 30 fanout violations.** Divided (the receipt keeps
its ratio fields null because LibreLane's layout fails a gate), the native
layout has 5.2 % less area, 7.7 % less wire and 1.3 % fewer vias, from a
different netlist.

Since 2026-11-05 the router's topology controls (RustROAD
[`d010664`](https://github.com/lab-emi/RustROAD/commit/d0106647b548a83917c91391a8a105bb78ea238b),
superproject `3cd3061`: `wrong_way_factor` 6 with a bend surcharge, a stronger two-pass via
reduction, joint rewrites of walled-in chains, escape jogs and `TRACKS`
statements in the DEF) cut the routed DEF from about 192 thousand wire pieces
to 160 thousand, at the cost of 2,060 more vias; placement and cells are
unchanged. In that change's interleaved A/B the shared OpenRCX gate on the
native layout went from 1.33× to 0.99× LibreLane's time
([routing budget](routing-budget.md#wire-pieces-against-vias-prices-joint-moves-escape-jogs-track-grid)).
The recipe's four Python backend helpers (physical inputs, antenna session,
its review, post-route repair) became `rustroad` commands
([native flow steps](flow-steps.md)).

| Ibex final layout | RustROAD, native recipe | OpenROAD 26Q3, LibreLane Classic |
| --- | --- | --- |
| Standard-cell area | 187,585.2 µm² | 197,913.7 µm² |
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

RustROAD's GDS export took 0.498 s against 1.375 s for KLayout's DEF-to-GDS
streamout (2.76×). **In the campaign its extraction was slower than
OpenRCX**: 4.664 s at 4 threads against 3.829 s (1.22×; the ranges do not
overlap) and 7.951 s at 2 threads against 3.625 s (2.19×), with about 3.7 times
OpenRCX's CPU time and 1.60× its peak RSS; on 2026-11-05, on another host and
layout, it was level at 4 threads (1.05×). Since the campaign, RustROAD
`fe4b286` is level with OpenRCX at 4 threads (1.07×, ranges overlap) and still
1.85× slower at 2, with the same SPEF ([faster extraction](#faster-extraction-2026-11-13)),
and RustROAD `8d29a61` is level at 4 threads (0.91×, ranges overlap) and 1.64×
slower at 2 ([second round](#faster-extraction-second-round-2026-11-14)).
In same-load runs OpenRCX now extracts the native DEF in
the time it takes on LibreLane's (median 3.37 s against 3.38 s, 1.00×), and
foundry main DRC, antenna DRC, LVS and latch-up run in 0.73×, 0.75×, 0.69× and
0.76× of their time on LibreLane's layout. The host differs from 2026-11-05's
(every LibreLane stage took less time on it), so the two campaigns' times are
history, not a before/after.

[Scope, tables and method](development-status.md#ibex-host-campaign-2026-11-13) ·
[Place, CTS and route vs OpenROAD](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-place-cts-and-route-vs-openroad-2026-11-13) ·
[Extraction vs OpenRCX](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-extraction-vs-openrcx-2026-11-13) ·
[GDS export vs KLayout](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-gds-export-vs-klayout-2026-11-13) ·
[Shared gates](https://lab-emi.github.io/RustEDA/benchmarks/#shared-gates-2026-11-13) ·
[Host campaign 2026-11-13](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-13)

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

RustROAD's GDS export took 0.831 s against 1.938 s for KLayout's LEF/DEF
streamout; its extraction took 6.260 s at 4 threads against 5.944 s for OpenRCX
(ranges overlap) and 10.121 s at 2 threads against 5.988 s. OpenRCX extraction
of the native DEF is slower than of LibreLane's (1.36× in same-load runs),
consistent with its route topology's larger number of wire pieces (192,328
against 129,901).

[Scope, tables and method](development-status.md#ibex-host-campaign-2026-11-05) ·
[Host campaign 2026-11-05](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-05)

## Ibex host campaign · 2026-11-02

In the RustEDA Ibex host campaign (IHP SG13G2 / EZ130 with two real SRAMs, on a
4-vCPU cloud host), RustROAD in the repository's published native recipe
(8 GiB memory cap) and OpenROAD 26Q3 in LibreLane 3.0.17 (10 GiB cap)
implemented the same prepared design. **Both final layouts pass foundry main
and antenna DRC with 0 violations and full-device LVS.** The native layout has
233,909.2 µm² standard-cell area, 1,679,076.6 µm routed signal wire and
301,099 vias, against 197,913.7 µm², 969,571.6 µm and 122,194 for OpenROAD,
on different netlists. Its
routing step took 389.63 s for global and detailed routing, followed by
588.35 s of post-route and antenna repair; OpenROAD's detailed routing alone
took 1,796.83 s. Each flow is a single timed sample and the netlists differ,
so no quotient is formed.

On the same final layout (medians of three runs), native extraction took
158.823 s with `RAYON_NUM_THREADS` at 2 or 4 (RustROAD's pool fixed at its
default of 2 threads), against 17.649 s and 17.114 s for OpenRCX at 2 and
4 threads; in the recipe's serial-query configuration it is effectively
single-threaded. GDS export took 3.019 s against 2.286 s for KLayout's LEF/DEF
streamout (timing only; the outputs are not byte-equivalent). The native run
is `unqualified` and stopped at the RustSTA/OpenSTA `timing_accuracy` gate;
neither flow passes every gate, so no end-to-end ratio is published. The
published recipe reproduces the run16-class layout; the R20/R21 recipe inputs
are not published. Deviations include the nixpkgs OpenROAD build, a
reconstructed EZ130 tech LEF and cell GDS, and LibreLane's own post-GRT
repair.

[Scope, tables and method](development-status.md) ·
[Place, CTS and route vs OpenROAD](https://lab-emi.github.io/RustEDA/benchmarks/#rustroad-place-cts-and-route-vs-openroad) ·
[Method and deviations](https://lab-emi.github.io/RustEDA/benchmarks/#host-campaign-method-and-deviations) ·
[All benchmarks](https://lab-emi.github.io/RustEDA/benchmarks/)

## Development update · 2026-11-01

The development source combines bounded extraction indices and query workers,
single-session antenna repair, an opt-in staggered tap lattice, placement-stage
diagnostics and spacing-aware electrostatic density. The density model accounts
for the same cell spacing required by legalization. Optional checkpoint retention
at an evaluation horizon reports the reason explicitly; in that update the default
policy stayed strict. Since then, keeping the checkpoint at the evaluation cap is
the default (`stop_at_evaluation_cap`, see
[electrostatic placement](electrostatic-placement.md)). Repeated same-path
routing near misses receive bounded repair.

[Source, evidence and remaining limits](development-status.md)
