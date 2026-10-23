# Resource contract

For real technology/netlist import in version 0.2, also read
[the PDK import budget](pdk-import-budget.md), which includes parser, alias,
connectivity, geometry and snapshot reconciliation state.

Written before implementing the first physical pass and its verification fixtures.

The initial pass is a single-height, fixed-order row packer, not an analytical
placer or router. At the default cap of 250,000 instances, 100,000 rows, one
corner and two Rayon threads, conservatively budget 2 KiB per instance for
input strings, the per-design interner, generational arena, validation maps and
placement state (488 MiB), 256 bytes per row (24.4 MiB), and 16 MiB of bounded JSON
input plus parsing overhead (up to 64 MiB). Allow a further 128 MiB for snapshot
metadata, immutable mmap pages and publication. The per-instance allowance includes
the source design, duplicated interner keys, a publication metadata copy, arena,
SoA view and output records. Planned runtime RSS is below 1 GiB. The input byte
cap further constrains the number of long names. The estimator deliberately
overcounts shared state rather than assuming
one private design copy per worker. Parallel width classification uses bounded
indexed output, not private graph replicas.

There are no timing nodes/arcs, tag tables, cuts, clauses, paths or cones in this
milestone. There is no graph traversal or exponential materialization. Placement
uses at most instances + rows candidate expansions, enforced by a configurable
`--max-expansions` bound. A later graph algorithm must add its own estimate before
implementation. The independent DEF checker has its own stated budget.

Default CLI limits: 16 MiB input/metadata (also enforced by a bounded serialization
writer for programmatic calls), 250,000 cells, 100,000 rows, 350,000
placement expansions. Names are bounded to 128 bytes. Binary tables have fixed
40-byte records and are length-checked before mapping. Revisions are immutable;
temporary and orphan revisions are retained, with 128 bounded recovery slots.
Revision storage accumulates on disk; garbage collection is not implemented.
Workspace users must honor the lock and immutable-table contract; modifying an
already mapped file from outside RustROAD is unsupported. A future renderer must
define a reader lease/garbage-collection protocol before reclaiming snapshots.

Build estimate: five small crates and their Rust dependencies, two concurrent
compiler jobs, expected below 2 GiB. Before builds run `free -h`. Use:

```sh
mkdir -p .work
ulimit -v 8388608
export CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2
/usr/bin/time -v -o .work/test.time timeout 180s \
  /home/cgao/.cargo/bin/cargo test --workspace
```

This is a CPU-only 8 GiB virtual-address guard. Do not reuse it for CUDA/Metal
drivers. High-memory (>4 GiB estimated RSS) campaigns must additionally hold
`.work/scale.lock` and use the parent AGENTS.md cgroup policy. Run generated test
workspaces on this disk under `.work`, not RAM-backed `/tmp`. Record actual peak
RSS and wall time after the first run. Missing reference tools never count as a
successful validation.

## Initial end-to-end placement workload

The bounded generator accepts at most 50,000 cells and emits at most 2,500 rows.
At 2 KiB per cell the Rust allowance is 98 MiB plus the fixed allowances above.
The Python independent checker estimates below 512 MiB (objects, event lists,
sort keys and parsed JSON/DEF), with one interpreter. The generator does not
materialize paths, graphs, cuts or clauses. Run one pipeline at a time; compare
one and two worker results by DEF SHA-256. Timings include process startup,
JSON parsing, validation, arena/interner construction, SoA lowering, row placement,
snapshot writes/fsync, mmap reads, DEF export and the independent checker. They
are not OpenROAD performance comparisons and do not measure routing or GPU work.
