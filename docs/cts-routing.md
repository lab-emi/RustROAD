# Constrained CTS, routing and extracted timing — v0.5.0

The four-register `fixtures/flow/pipeline.v` now passes through real RustSys
mapping, RustROAD placement and clock buffers, clock/signal routing, external
OpenRCX extraction, SPEF import and propagated-clock RustSTA/OpenSTA analysis
on both baseline PDKs. The functional integration passes. It is **not a closed
timing or signoff result**: the strict 1 ps timing parity gate fails, and hold
violations remain. The harness returns exit 3 for this partial result.

## Constraints and physical algorithms

`clock-tree --sdc FILE` reads a strict, bounded primary-clock SDC subset. It
supports `create_clock` name/period/waveform, clock/input transition, uncertainty,
design-wide fanout/capacitance, maximum transition, propagated selection and
explicit ns/pF units. It accepts literal collections, not a Tcl interpreter.
Unsupported constraints, generated clocks, substitutions, scopes and command
forms fail explicitly. Source SHA256 and parsed constraints survive reload.
The clock fanout is the smaller of the physical profile and SDC limit. Clock
period/uncertainty and other limits are retained for independent timing checks;
the topology algorithm does not claim timing-driven optimization.

`route --scope clock|all --config FILE` builds a deterministic finite lattice
from manufacturing-aligned coordinates and bounded pin candidates. A bounded
local search first reserves pin access to the configured metal, including fixed
vias. Then bounded A* joins each sink to its net's existing conductor tree.
The router respects LEF pin/OBS rectangles, supply geometry and earlier nets.
It checks cut spacing even between cuts of the same net. No paths/cuts are
enumerated exponentially. See [the upfront resource budget](routing-budget.md).

The routed profiles leave two sites between logical cells. Fillers preserve
row power continuity. This is an explicit spacing policy, not global placement
optimization. The larger previous six-register fixture still triggers finite
search/access limits and remains outside the demonstrated routing coverage.
Do not infer large-design scalability from the four-register case.

Routing output uses integer DBU and the technology's default widths. Standard
DEF NETS contain ROUTED/NEW paths and fixed vias. Schema 5 adds a checked
`routing.rbin`; loading validates geometry, endpoint connectivity and table
identity. Failed routing leaves HEAD unchanged. Repeated routing is byte-stable;
new placement discards routes and returns to the appropriate pre-route schema.

## Independent physical and RC checks

The reference adapter uses public OpenDB APIs to read the exported DEF, enumerate
actual wire/via rectangles and obtain original LEF pins/obstructions. KLayout
polygon operations independently check continuity, selected width/spacing rules,
pin coverage, obstacles and shorts. Missing-net geometry and a planted supply
short are rejected. Existing original-GDS tap and primary PG checks run again.
This is a subset geometry check; full foundry DRC, antenna, LVS and IR/EM are
still unchecked.

OpenRCX runs as a pinned external executable on the routed DEF and the PDK's
dedicated nominal extraction rules. It emits detailed R/C SPEF with coupling.
The adapter uses documented extraction commands from the
[OpenRCX manual](https://openroad.readthedocs.io/en/latest/main/src/rcx/README.html).
GF180's technology LEF explicitly warns that its electrical values are not
appropriate for extraction; the dedicated `rules.openrcx.gf180mcuD.nom` is used.
No LEF placeholder RC is promoted to calibrated extraction evidence. The native
RustROAD library does not yet implement a calibrated RC field extractor.

The files are imported by explicitly pinned RustSTA/RustEDA **postroute candidate
binaries**, copied to `.work/routing-tools/`. They differ from the old frozen
release tools that lack SPEF support. Their hashes are retained; neither sibling
source nor its target is rebuilt or modified. They use f64 lumped NLDM plus
Elmore, grounded coupling factor 1 and propagated primary clocks, with CPPR off.
Full-vs-update checks pass. Deleting an annotated clock net must fail with missing
SPEF annotation rather than falling back to ideal wires.

## What passes, and what does not

- All 19 connected signal nets in the four-register example route on each PDK.
  CEC, 512-vector simulation (511 pipeline outputs after initialization), PG/tap
  coverage, geometric checks, repeated-routing identity and SPEF import pass.
- OpenSTA reports nonzero propagated latency and rise/fall skew for all four
  clock sinks. The configured skew limits are 0.5 ns (Sky130) and 1 ns (GF180).
  A 10,000× resistance perturbation in one clock branch fails this skew gate.
- Maximum RustSTA/OpenSTA endpoint differences are about **3.15 ps / 6.79 ps**,
  beyond the unchanged **1 ps** gate. These are recorded as `different`, not a
  pass. OpenSTA uses its default delay calculator; identical delay algorithms
  are not asserted. No reference-tool implementation was inspected to explain
  or mask the differences.
- Native hold reports have violations; small Sky130 differences can change a
  near-zero hold verdict. Both native and reference numbers are retained. This
  demands timing-model parity and hold repair before closure can be claimed.
- Pulse-width/minimum-period and unimplemented design-rule diagnostics remain
  explicit. The example has eight constrained endpoints and no uncovered ones.

## Reproduce

Use the pinned Rust toolchain and installed black-box dependencies. The PDK
paths/hashes are in `fixtures/pdk/local-routing.json`. Supply explicit candidate
binaries with the required SPEF/propagated-clock capability if the local copies
are unavailable; an absent dependency is unverified, not a pass.

```sh
free -h
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
cargo build --release --locked --offline --bin rustroad
/usr/bin/time -v -o .work/routing.time timeout 180s \
  python3 scripts/verify_routing.py --workspace .work/routing-check --base-rev empty \
  --rusteda /path/to/postroute/rusteda --ruststa /path/to/postroute/ruststa
```

Expected current result: `functional_status=passed`, `status=partial`, exit 3.
The [HTML report](cts-routing.md) and
[retained results](evidence/routing-results.json) include exact hashes, resource
measurements, scope and negative controls. The raw local logs are linked there.

Next gates: independently resolve the observed timing-model differences, repair
hold without changing the intended constraints, improve pin access/rip-up for
denser designs, then GDS streamout and complete foundry DRC/LVS. A routed DEF and
an extracted SPEF alone do not finish RTL-to-GDS signoff.
