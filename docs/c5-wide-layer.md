# Wide-spacing queries on layers without wide parts

A layer can declare a wide-spacing rule while containing only narrow metal.
The wide-spacing owner index contains only actual wide parts, so such a layer
has no `reach` entry. A query on that layer has no same-layer owner candidates.
It must consume the empty-query work unit and return from that visit, rather
than indexing a missing entry and panicking. Queries on other layers, same-net
notch checks, and active-layer comparison counts stay unchanged.

The mixed-layer regression first reproduces the original panic: M1 has a real
wide-spacing violation, while M2 has a rule but only narrow wires, via pads,
patches and a pin. It then adds a wide M2 component and checks both distinct-net
and same-net violations. A low comparison cap must still fail explicitly.

## Upfront resource estimate

The fixture has four wires, at most three patches, three pins and one via on two
layers; its bounded rectangle/grid state is conservatively below 1 MiB per test
and adds no path enumeration. Existing engine comparison, grid and piece caps
remain in force. The new guard allocates nothing.

The fresh two-worker build was estimated below 3 GiB aggregate from prior
measurements. All checks use a 4 GiB RAM / 1 GiB swap cgroup, bounded wall time,
`CARGO_BUILD_JOBS=2`, `RUST_TEST_THREADS=2`, CPUs 24–31, and qualified GNU time.
The serial dual-PDK harness estimates below 1 GiB per native child and uses the
same route budgets, pinned historical executable and independent DEF geometry
comparison as the existing SKY130/GF180 controls. No high-memory scale lock is
needed for these estimates below 4 GiB.

The C5 diagnostic replay retains its original admission estimate of 3955 MiB
plus a 32 MiB controller, 16 requested route threads, 1200-second route timeout,
and 4 GiB / 1 GiB cgroup. It runs only in a newly copied private rev3 workspace.
Original failure evidence and qualified binaries remain immutable. Timings may
overlap GPU compilation and cannot establish controlled speedup.

## Validation

The focused regression failed at the original missing-key panic, then passed
after the guard. Workspace tests passed: 435 passed, 0 failed, 11 existing
ignored; fresh build plus tests took 91.26 seconds, peak RSS 972084 KiB.
Formatting, Clippy on all targets/features with warnings denied, and rustdoc
passed. Raw resource and command receipts are private under
`.work/perf-20261030/qor-c5-fix/` in the superproject.

The release build passed in 102.01 seconds with 1011632 KiB peak RSS. The
unmodified dual-PDK harness passed all 36 CLI commands, exact routes/netlists,
and six independently expanded DEF geometry comparisons against the pinned
historical executable. Both SKY130 and GF180 inputs were hash-rechecked.
The fixed private executable SHA-256 is
`2917b58c71006e40170f5aceeedb16abbae4666c516afa6417167e01d6d3afdb`.
This validates the focused guard and existing routing controls; IHP C5 QoR and
foundry DRC/LVS/timing remain separate diagnostic gates.
