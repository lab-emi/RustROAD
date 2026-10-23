# RustROAD

Clean-room, native Rust physical-design development for the RustEDA toolchain.
Apache-2.0. This is an initial engineering baseline, **not a complete RTL-to-GDS
implementation**. Current version: 0.1.0.

Implemented:

- Strict bounded JSON design import, integer `i64` DBU coordinates, per-design
  string interning and generational instance IDs.
- Deterministic pure-Rust `cpu_ref` fixed-order single-height row placement.
  Runtime and work are bounded by cells + rows; this is not global optimization.
- Daemonless disk workspaces, `workspace.lock`, mandatory mutation `--base-rev`,
  atomic revision publication and read-only `memmap2` instance tables.
- Placement-only DEF export, independent Python legality checking, and a pinned
  external OpenROAD positive/negative placement oracle captured by REA.
- Read-only pinned-tool/dual-PDK flow readiness and a guarded logic validation
  harness using existing RustSys/RustSTA and independent external tools.

Not implemented: LEF/netlist connectivity import into the physical engine,
global placement optimization, CTS, routing, extraction, GDS streamout, DRC/LVS,
or GUI. Pinless geometry fixtures are not PDK signoff. Missing results always
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

The exported DEF contains geometry only: rows and placed components. It does not
invent pins, nets or routes. Its `placement_status` stays `placed_unverified`;
external validation receipts are separate evidence.

## Verification and research

- [REA/OpenROAD study and placement evidence](docs/openroad-blackbox.md)
- [Flow integration contract](docs/flow-integration.md)
- [GUI architecture and framework decision](docs/gui-architecture.md)
- [GUI benchmark acceptance plan](docs/gui-benchmark-plan.md)
- [Architecture and next implementation gates](docs/architecture.md)
- [Chinese engineering report](docs/report.html)

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
