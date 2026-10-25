# RustSys / RustSTA integration and validation

RustROAD has a working, bounded **logic and pre-layout timing** integration on a
small two-input, one-register circuit with both required baseline libraries. It
does not yet implement routing, extraction, real-PDK stream-out, DRC or LVS.
`full_rtl_to_gds_status` remains `unverified`. A placement-only DEF is a separate
physical milestone, not a closed RTL-to-GDS flow.

## Reproduced evidence

The tracked [logic evidence](evidence/logic-verification.json) contains executable
version/hash pins, original Liberty hashes, native scope/diagnostics, every
comparison, negative controls, and an index of 48 local process receipts. The
full raw run is `.work/logic-suite-v2/`; the suite took 10.09 seconds and peaked
at 290,124 KiB RSS on this host. These are integration smoke measurements, not
chip-scale performance results. The outer measurement covers the complete
invocation, including subprocess startup, imports, mmap files, checks and the
Verilator build. RSS is maximum process RSS, not aggregate process-family memory.

| Check | Sky130 HD TT 1.80 V / 25 C | GF180 MCU 7T5V0 TT 3.30 V / 25 C |
|---|---|---|
| RustSys mapped netlist | 2 cells, native CEC `equivalent` | 2 cells, native CEC `equivalent` |
| External Yosys CEC | correct circuit proves; XOR→AND control rejects | same |
| Verilator simulation | all 4 input combinations agree in RTL and mapped model; wrong oracle rejects | same |
| RustSTA full and independent checks | `agree` | `agree` |
| RustSTA no-op incremental update vs fresh full check | `agree`, 0 ns difference | `agree`, 0 ns difference |
| RustSTA vs OpenSTA setup/hold, 2 endpoints | max difference 0.0000010211960717043667 ns | max difference 0.0000005134069436252275 ns |

The differential tolerance is 0.001 ns. Both tools use the **same explicitly
hashed verification-only ideal-wire library** for this comparison. The Sky130
original has `wireload_lumped`; its wire-load metadata is removed only in a
separate verification projection. Original functions/NLDM tables are unchanged;
projection byte spans and hashes are recorded. The production Liberty is never
modified. GF180's original already selects ideal wires. Both original-library
native runs are also recorded.

The SDC has a 10 ns ideal clock, 0.2 ns input/output delays, 0.1 ns slew and
0.01 pF output load. Both endpoints are constrained. RustSTA explicitly reports
unsupported `min_pulse_width` arcs (both PDKs) and `minimum_period` arcs (GF180).
These warnings remain in the evidence. This is setup/hold calibration within the
recorded scope, **not complete Liberty support or signoff**. The incremental test
is deliberately a no-op update; it does not validate an ECO with changed timing
arcs. Simulation samples after the first clock and does not establish arbitrary
initial-state equivalence; CEC is a separate check without a reset sequence.

The [tool probe](evidence/tool-probe.json) also found KLayout 0.30.0, Magic
8.3.105, Netgen 1.5.133 and ngspice 45.2. Their startup/version checks do not
establish a valid physical or analog flow. Icarus was absent from the checked
PATH; the actual simulation uses Verilator 5.032. The probe pins entrypoints,
not all shared libraries, container contents or children of wrapper scripts.
No reference implementation source was inspected.

## Running the logic verification

Python is a verification dependency, never a dependency of a Rust core crate.
The harness adapts the Apache-2.0 RustLane black-box engine harness and adds
RustROAD simulation, incremental no-op verification and explicit evidence
workspace revision/lock handling. It neither rebuilds nor modifies sibling
RustSys/RustSTA repositories. It creates a **fresh dedicated evidence workspace**;
its simple `HEAD` is not a RustROAD physical workspace and cannot be reused by
`rustroad init`.

Before the suite, inspect `free -h`. The upfront budget is one active child,
128 objects plus 256 arcs at a conservative 256 bytes each, 1 corner, 2 threads,
2–4 MiB graph/tag state, up to 640 MiB for parsed libraries, 256 MiB projection
and 256 MiB runtime overhead. Paths are capped at 8, native SAT obligations at
10,000 conflicts and sweep candidates at 256. The external solver has a hard
30 second CPU/wall limit and 2 GiB address-space cap because its public command
has no conflict-limit option. Files/logs are capped at 64 MiB; identities at
256 MiB. Processes run serially; libraries are not cloned into parallel workers.

```sh
free -h
ulimit -v 2097152
CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 RAYON_NUM_THREADS=2 \
/usr/bin/time -v -o .work/logic-repeat.time timeout 180s \
python3 scripts/verify_logic.py \
  --workspace .work/logic-repeat --base-rev empty \
  --rustsys /absolute/path/to/rustsys \
  --rusteda /absolute/path/to/RustSTA/rusteda \
  --ruststa /absolute/path/to/ruststa \
  --sky130-lib /absolute/path/to/sky130_fd_sc_hd__tt_025C_1v80.lib \
  --gf180-lib /absolute/path/to/gf180mcu_fd_sc_mcu7t5v0__tt_025C_3v30.lib \
  --yosys /absolute/path/to/yosys --opensta /absolute/path/to/sta \
  --verilator /absolute/path/to/verilator --threads 2 --timeout-seconds 30
```

The directory must not exist. Outputs include captured Verilog/SDC, synthesis
and checker reports, independent reference scripts, stdout/stderr, full
`/usr/bin/time -v` receipts, versions and hashes. Missing tools/libraries produce
`unverified` (exit 3), failed evidence exit 1, successful scoped evidence exit 0.
A process exit alone is never a proof: CEC verdicts, simulator markers, negative
controls, timing scope, finite values and tolerance are all checked. Outer
`timeout` bounds the whole suite; preserve evidence from interruption. Child
limits are process limits, not a filesystem/network sandbox or an aggregate
memory cgroup; use the parent AGENTS.md cgroup policy for larger campaigns.

## Read-only readiness API

`rustroad-flow::assess_path(&Path)` reads a `rustroad.flow.v1` manifest. The CLI
runs it under the physical workspace's shared lock:

```sh
rustroad flow-check --workspace .work/design \
  --manifest fixtures/flow/local-pinned.json
```

`local-pinned.json` is the measured host's explicit profile, not a portable
installer. `unconfigured.json` is a portable empty starting profile and keeps
both required PDK rows visible. Each configured file requires an absolute path
and lowercase SHA-256. The manifest is limited to 1 MiB, 16 tools, 8 PDKs and 64
file identities. Hashing streams a 64 KiB buffer, with a 256 MiB per-file cap;
regular-file checks and nonblocking opens reject FIFOs. No subprocess is started
by the Rust API. Expected RSS is below 16 MiB excluding runtime/CLI state; no
per-node graphs, tag tables, cuts, clauses or paths are created.

A tool entry may carry `"not_for_signoff": "<reason>"`. Such a tool's identity
is reported as `not_for_signoff` and every stage that depends on it stays
`unverified` with `tool_<id>_not_for_signoff` in `missing`. The host Magic
8.3.105, KLayout 0.30.0 and Netgen 1.5.133 in `local-pinned.json` are marked
this way; foundry signoff uses the pinned image in
`fixtures/flow/signoff-pinned.json` (see [signoff.md](signoff.md)).

`ready_to_attempt` only means required pinned files matched. Every stage still
has `verification_status: "not_run"`; missing or mismatched inputs are
`unverified`. Unknown timing scope fields are `null`, never invented zeroes.
A missing routing implementation and absent extracted-parasitics adapter keep
full-loop readiness unverified even if all executables are installed. The SPEF
limitation is tied to the frozen binary/adapter used in this run, not a claim
about future RustSTA revisions.

`python3 scripts/flow_probe.py --workspace .work/design --manifest
fixtures/flow/local-pinned.json` executes allowlisted version commands under a
shared lock, a 1 GiB address-space cap, a 3 second deadline per tool and a 64 KiB
output cap, emitting only JSON to stdout. It does not mutate the workspace.

## Independent placement legality

[DEF control evidence](evidence/def-controls.json) validates the actual 24-cell,
6-row CLI export with an independent Python checker. It rejects injected cell
overlap, off-grid placement, duplicate/missing cells, master changes and
unsupported statements. This checker does not reproduce the row-packer: it
uses an x-axis sweep with a Fenwick index to reject overlapping rows, a lattice
lookup and sorted cell intervals to check each placement.

```sh
ulimit -v 1048576
/usr/bin/time -v -o .work/def-controls.time timeout 30s \
python3 scripts/verify_def.py --input fixtures/physical-small.json \
  --def .work/physical-small.def --workspace .work/def-controls-repeat --base-rev empty
```

Inputs are bounded to 16 MiB, 250,000 cells and 100,000 rows. The independent
checker uses O((cells + rows) log(cells + rows)) work and bounded linear storage;
no pairwise overlap matrix or path materialization. A conservative 1 GiB cap
covers JSON objects, dictionaries, coordinate-compressed row events and sorting.
The 24-cell check/control run measured 20,236 KiB RSS including initialization,
placement and export; this is not a large-design memory claim. It handles only
the documented single-height, N-oriented, one-statement-per-line emitted DEF
subset. DRC/LVS and connectivity remain `unchecked`.

## Next implementation gate

Import real technology/cell LEF plus mapped-Verilog connectivity with explicit
unsupported-construct diagnostics and consistent integer DBU. Join this with
RustROAD placement, then add bounded net-aware placement and legal detailed
routing. For both PDKs pin original LEF/GDS, extraction rules, DRC and LVS decks;
stream out real referenced cell geometry and independently reopen the GDS.
Run DRC and extracted-netlist LVS with positive/negative controls. Add a RustSTA
extracted-parasitics adapter and changed-ECO incremental/full comparisons before
calling any future end-to-end result signoff. Do not substitute rectangles,
missing decks or ideal wires for these requirements.
