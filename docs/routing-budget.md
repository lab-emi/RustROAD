# Constrained routing / extraction verification resource contract

Written before implementing the SDC reader, routing graph or new checker.
Geometry is i64 DBU; timing constraints use finite f64 in explicit ns units.
The first router is deterministic CPU reference code, not GPU code.

The router uses a finite, rectilinear coordinate lattice: manufacturing-aligned
regular coordinates plus bounded pin-access coordinates, <=512 per axis and
<=8 routing layers. A configurable `max_grid_nodes` (default 2,000,000) is
checked before allocating state. Distance, predecessor and generation arrays
cost <=16 bytes/node (~32 MiB); heap entries <=500,000 at ~32 bytes (~32 MiB).
Only one sink search runs at once. Per-search and total expansion/work limits
stop routing with a structured no-route/budget diagnostic, retaining the old
workspace HEAD. No simple paths, cuts or cones are materialized.

Pin/OBS/supply/previous-route obstacles use a bounded spatial bin index. At
most 200,000 rectangles and 1,000,000 bin references cost <80 MiB including
maps and allocation capacity. Queries have an independent 200,000,000-comparison budget.
Routed wire/via records are capped at 100,000. Immutable source geometry is
shared within a routing transaction; no per-corner/per-worker graph copies.
Conservative extra native state is <256 MiB (one corner, one routing worker).
The small dual-PDK fixture has <150 instances, <200 terminals and an expected
lattice <2,000,000 nodes; expected native RSS <256 MiB. Large designs that cannot
fit this first lattice must fail explicitly, not allocate an unbounded grid.

SDC input is <=1 MiB, <=10,000 bounded statements/tokens with no Tcl execution,
file sourcing, substitutions, loops or eval. Unsupported timing semantics are
explicit diagnostics/errors. Clock configuration, chosen layers/vias, physical
routes and source constraints are revisioned and checked when reloading.

Independent OpenDB/KLayout geometry checks and OpenRCX extraction run as external
executables on standard LEF/DEF/GDS/SPEF inputs. Each process has <=2 GiB physical
memory/no swap where a cgroup is available, <=30 s wall time and bounded output.
OpenROAD gets an 8 GiB virtual-address allowance for its allocator. Parsed SPEF
and clock reports are <=32 MiB, <=200,000 nodes/elements; validation uses bounded
linear graph work, not path enumeration. PDKs/corners run serially. Expected
external RSS is <512 MiB; any test >2 GiB or PDK run >8 GiB is a defect.

Use `free -h` before builds/suites, `CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`,
`RAYON_NUM_THREADS=2`, `ulimit -v 8388608`, `timeout 180s` and `/usr/bin/time -v`.
Serialize runs estimated above 4 GiB behind `.work/scale.lock`. Measure complete
import/CTS/routing/export/extraction/STA wall time and peak RSS, not only search
time. Pin executable and PDK-rule hashes. Missing tools/models yield unverified.

GF180's technology LEF warns that its parasitic values are unsuitable for
extraction. Use the PDK's dedicated OpenRCX nominal rule file, never promote
LEF placeholder RC to foundry extraction evidence. Both PDKs must retain the
distinction between routed connectivity, subset geometric checks, extracted
timing and complete foundry DRC/LVS. Clock skew acceptance must use explicit
limits and propagated clocks, never ideal-clock equality as a skew oracle.
