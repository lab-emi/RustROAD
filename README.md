# RustROAD

Clean-room, native Rust physical-design development for the RustEDA toolchain.
Apache-2.0. This is an initial engineering baseline, **not a complete RTL-to-GDS
implementation**. Current version: 0.4.0.

Implemented:

- Strict bounded JSON design import, integer `i64` DBU coordinates, per-design
  string interning and generational instance IDs.
- Deterministic pure-Rust `cpu_ref` fixed-order single-height row placement.
  Runtime and work are bounded by cells + rows; this is not global optimization.
- Daemonless disk workspaces, `workspace.lock`, mandatory mutation `--base-rev`,
  atomic revision publication and read-only `memmap2` instance tables.
- Placement-only DEF export, independent Python legality checking, and a pinned
  external OpenROAD positive/negative placement oracle captured by REA.
- Real technology/cell LEF import with exact DBU conversion, sites, layers,
  macros, pin/obstruction geometry, fixed vias and explicit unsupported-rule
  diagnostics. Retains only used cell masters in the design snapshot.
- Flat mapped Verilog import with buses, escaped names, bit/part selects,
  concatenations and assignment aliases; bounded linking and driver validation.
- Connected DEF PINS/NETS and canonical Verilog export; version-2 mmap pin/CSR
  tables with logical-to-physical consistency checks when reopening snapshots.
- An actual RustSys → RustROAD → RustSTA path on both real Sky130/GF180 PDKs,
  independently checked through Yosys, Verilator and OpenROAD/OpenSTA.
- Physical boundary I/O placement, constant-to-tie-cell materialization with
  bounded fanout, row fillers, N/FS row placement and primary power-rail checks.
- An explicit primary supply grid: horizontal rails, two vertical spines,
  fixed vias and supply ports, exported through DEF SPECIALNETS.
- Profile-driven well/substrate tap reservation, insertion and repeatable
  re-placement. Independent OpenDB + original-PDK GDS checks cover actual
  contacts, their connection to supply metal, and active-region tap distance.
- Bounded, level-balanced clock-buffer topology, with independent Liberty
  truth-table proofs, RTL equivalence, OpenDB connectivity and ideal-wire STA.
  This is an unrouted CTS prototype, without skew optimization.
- Read-only pinned-tool/dual-PDK flow readiness and a guarded logic validation
  harness using existing RustSys/RustSTA and independent external tools.

Not implemented: global placement optimization, routed/timing-driven CTS,
signal routing, extraction, GDS streamout, full-deck DRC/LVS, IR/EM analysis, or GUI. Real-PDK connectivity
and cell placement are not PDK signoff. Missing results always
remain `unverified`, `not_run` or `unchecked`.

## Build and try

Read [HANDOFF.md](HANDOFF.md) and [memory budgets](docs/memory-budget.md) first.
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
clock topology requires a fresh import. No propagated-clock, skew or RC result
is claimed. Read [the implementation and reproduction guide](docs/tap-integration.md).

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

## Verification and research

- [REA/OpenROAD study and placement evidence](docs/openroad-blackbox.md)
- [Flow integration contract](docs/flow-integration.md)
- [Real-PDK connected flow](docs/pdk-import.md)
- [GUI architecture and framework decision](docs/gui-architecture.md)
- [GUI benchmark acceptance plan](docs/gui-benchmark-plan.md)
- [Architecture and next implementation gates](docs/architecture.md)
- [Chinese tap and clock-topology report](docs/tap-report.html)
- [Chinese physical-preparation report](docs/power-io-report.html)
- [Earlier PDK integration report](docs/pdk-report.html)
- [Earlier bootstrap report](docs/report.html)

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
