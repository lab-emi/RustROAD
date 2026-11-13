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
- GDS export **2.8× faster than KLayout** with **2.0× less peak memory**. **RustROAD's own extraction is slower than OpenRCX**: 1.22× its time at 4 threads (4.664 s against 3.829 s; the ranges do not overlap) and 2.19× at 2 threads, with 1.60× its peak memory.
- Foundry DRC, antenna DRC, LVS and the latch-up check run **1.4×, 1.3×, 1.5× and 1.3× faster** on RustROAD's layout (same scripts, same load), and OpenRCX now extracts its DEF in the time it takes on LibreLane's (1.00×, level). The whole RustEDA flow finishes **4.4× sooner end to end** (751.54 s against 3,283.05 s).

LibreLane's layout fails the shared timing gate (112 maximum-capacitance and fanout violations at TT and SS) and its equivalence check does not complete, so the receipt keeps its own ratio fields null under its publication rule; the multiples here divide the recorded times. Stages are paired by phase, not as identical tasks. Against the three losses of 2026-11-05, in same-load runs: repair before global routing now takes 0.73× the time of LibreLane's two pre-route resizer steps (ranges do not overlap; 1.18× on 2026-11-05), OpenRCX on RustROAD's layout is level with its time on LibreLane's (1.00×, ranges overlap; 1.36× on 2026-11-05) and the latch-up check on RustROAD's layout takes 0.76× its time on LibreLane's (1.11× with the checker of that campaign). RustEDA is still not faster or not better everywhere: RustROAD's standalone extraction takes 1.22× OpenRCX's time at 4 threads (2.19× at 2), RustSTA's peak memory at 4 threads is 11.8 % above OpenSTA's, the timing gate is level (0.98×, ranges overlap) and RustROAD's layout keeps less hold margin (TT 0.108 ns against 0.172 ns; both meet hold). [All stages, gates and losses](https://lab-emi.github.io/RustEDA/benchmarks/#ibex-host-campaign-2026-11-13)

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
streamout (2.76×). **Its extraction is slower than OpenRCX**: 4.664 s at
4 threads against 3.829 s (1.22×; the ranges do not overlap) and 7.951 s at
2 threads against 3.625 s (2.19×), with about 3.7 times OpenRCX's CPU time and
1.60× its peak RSS; on 2026-11-05, on another host and layout, it was level at
4 threads (1.05×). In same-load runs OpenRCX now extracts the native DEF in
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
