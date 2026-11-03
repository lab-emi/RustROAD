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
- [Signoff scope](signoff.md)


## Ibex host campaign · 2026-11-02

In the RustEDA Ibex host campaign (IHP SG13G2 / EZ130 with two real SRAMs, on a
4-vCPU cloud host), RustROAD in the repository's published native recipe
(8 GiB memory cap) and OpenROAD 26Q3 in LibreLane 3.0.17 (10 GiB cap)
implemented the same prepared design. **Both final layouts pass foundry main
and antenna DRC with 0 violations and full-device LVS.** The native layout has
233,909.2 µm² standard-cell area, 1,679,076.6 µm routed signal wire and
301,099 vias, against 197,913.7 µm², 969,571.6 µm and 122,194 for OpenROAD:
**about 73 % more wire and about 2.5× the vias, on different netlists.** Its
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
at an evaluation horizon reports the reason explicitly; the default policy stays
strict. Repeated same-path routing near misses receive bounded repair.

[Source, evidence and remaining limits](development-status.md)
