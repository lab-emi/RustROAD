# Development status

Updated **2026-11-01**. This page describes the source published with the
RustEDA development update. Use the superproject’s exact gitlinks and the
repository lockfile when reproducing a run.

## Spacing-aware placement and bounded physical implementation

The development source combines bounded extraction indices and query workers,
single-session antenna repair, an opt-in staggered tap lattice, placement-stage
diagnostics and spacing-aware electrostatic density. The density model accounts
for the same cell spacing required by legalization. Optional checkpoint retention
at an evaluation horizon reports the reason explicitly; the default policy stays
strict. Repeated same-path routing near misses receive bounded repair.

In the qualified Ibex layout, standard-cell non-filler area is 204,053.9 µm²,
routed signal wire is 1,190,522 µm and the via count is 168,381. The complete
CPU flow with two real SRAMs records 472.2 s and 472.6 s, with 16/16 gates,
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
