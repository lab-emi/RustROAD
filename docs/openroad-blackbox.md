# OpenROAD: clean-room public-interface study with REA

Date: 2026-10-23. This is a bounded interface study, a small external placement
oracle, and a RustROAD design proposal. It is not a reproduction of OpenROAD
internals or evidence of RTL-to-GDS correctness.
No OpenROAD, Yosys, ABC or OpenSTA implementation source, disassembly or decompiler
output was inspected. REA was used only for its documented process-capture workflow.

## Observed executable and capture boundary

The host has no `openroad` command on PATH. Filename inventory found OpenROAD in
an existing local Podman image; no download, build, installation or registry pull
was needed. `REA 6.7.0` was already available in npm's runner cache and was invoked
by its absolute CLI path. Its version was checked before use.

The selected image has local content ID
`f152917ebc458bd015042e44d9dac5af69bcae9fb5657d5121cb4d7226ed0c67`.
Its OCI digests and the selected OpenROAD executable's complete SHA-256 are in
[`identities.json`](../evidence/rea/identities.json). The executable's `-version`
prints **`unknown`**. That is the observed response, not a usable semantic version;
reproduction must pin the captured image and binary identities. The container
path is `/OpenROAD-flow-scripts/tools/install/OpenROAD/bin/openroad`.

The outer process was `/usr/bin/podman`. Therefore REA's executable digest binds
Podman, not OpenROAD; the separate binary digest and image ID provide the latter's
identity. Neither file digest is a proof of an atomic operating-system image
binding. Commands inherit an unrecorded host environment in REA, while the target
container receives only its image defaults and selected arguments.

Three scenarios used the existing image with `--network=none --read-only`, a
64 MiB `/tmp` tmpfs, `--memory=2g --memory-swap=2g --cpus=2 --pids-limit=128` and
`--rm`. The parent process had an 8 GiB virtual-address ceiling and a 60-second
wall limit. Each REA scenario had a 30-second deadline, 256 KiB output cap and
128-sample process bound. The preflight estimate is in
[`resource-estimate.md`](../evidence/rea/resource-estimate.md).

| Probe | Observable outcome | Outer wall | Outer maximum RSS |
| --- | --- | --- | --- |
| `-version` | `unknown`, exit 0 | 1.21 s | 326,948 KiB |
| `-help` | Batch/GUI/web flags, exit 0 | 1.51 s | 318,204 KiB |
| Public Tcl `help` | 21 requested commands listed, exit 0 | 1.52 s | 320,276 KiB |

These numbers include REA/container startup and do not measure a physical-design
algorithm. GNU time does not establish a synchronized process-family or container
RSS peak. No placement/routing speedup is claimed. All captures were untruncated.
REA reported verified owned-process-group cleanup and removal of its temporary
root, but the help and Tcl captures also contain `unverified_processes` from
`EACCES` reading `/proc/.../environ`. Full descendant observation is therefore
not established. Network activity is not measured by REA; network isolation is a
selected container setting. No filesystem observation paths were selected.

The complete scenarios, original capture envelopes, progress logs, derived stdout
and time receipts are under [`evidence/rea`](../evidence/rea).
[`summary.json`](../evidence/rea/summary.json) indexes the evidence IDs and limits.

## Public contracts that were actually observed

Runtime `-help` supports a command file, `-exit`, `-no_init`, `-no_splash`,
`-threads`, JSON `-metrics`, and GUI/web options. RustROAD reference invocations
should be batch-only with a finite command file, `-no_init`, explicit thread count
and an outer timeout. The presence of GUI/server flags is not a reason to adopt a
resident service in RustROAD.

The self-authored [`public-contract.tcl`](../evidence/rea/public-contract.tcl)
only invokes `help` on selected public commands. No command definitions were
read and no design was loaded. Observed contracts include:

- Data interchange: `read_lef`, `read_def`, `read_verilog`, `read_liberty`,
  `read_sdc`, `write_def`, `write_verilog`, `write_spef`, `read_db`, `write_db`.
- Physical stages: `initialize_floorplan`, `global_placement`,
  `detailed_placement`, `check_placement`, `clock_tree_synthesis`,
  `global_route`, `detailed_route`, `extract_parasitics`.
- Reports: `report_worst_slack`, `report_tns`, `report_design_area`.

The [public API documentation](https://openroad.readthedocs.io/en/latest/main/src/README.html)
separately describes LEF/DEF/netlist interchange and saved database files.
RustROAD should use the standard interchange formats at reference boundaries;
it should own its disk-workspace representation rather than depend on `.odb`.
Matching command names alone proves neither data compatibility nor correctness.

## Flow lessons and our independent design

The [published flow overview](https://openroad.readthedocs.io/en/latest/main/README.html)
separates floorplanning, placement, clock synthesis, routing and final extraction/
verification. This motivates independently checkpointed RustROAD stages: a failed
route should preserve the last valid placement and its inputs rather than make
all earlier work opaque. This stage contract is our proposal, not an observation
of OpenROAD's internal implementation.

| RustROAD boundary | Owned data and output | Acceptance evidence |
| --- | --- | --- |
| Import / floorplan | Bounded LEF/DEF reader, integer DBU geometry, rows and sites | Explicit unsupported-feature diagnostics; independently checked units and bounds |
| Global placement | Separate net-pin CSR and placement SoA, bounded optimizer state | Density, wirelength and timing quality; deterministic reference mode |
| Legalization | Generational instance IDs, legal row/site positions | Independent geometry checker plus OpenROAD `check_placement` on exported DEF |
| CTS / repair | Versioned inserted-cell/netlist edits and clock constraints | Equivalence where applicable; clock/skew and setup/hold reports |
| Routing | Bounded routing grid/search frontier and wire/via shapes | Connectivity, route DRC, routed length and via counts |
| Extraction / finish | Corner-tagged SPEF, DEF/GDS exports and signoff manifests | Extracted timing plus independent DRC/LVS; missing checks remain unverified |

The [global-placement user guide](https://openroad.readthedocs.io/en/latest/main/src/gpl/README.html)
exposes timing and routability tradeoffs. RustROAD can define these as explicit
objectives and budgets without copying the reference's numerical method. Start
with a deterministic pure-Rust CPU reference, then measure SoA/CSR lowering,
iterations and synchronization before adding CUDA or Metal/Accelerate.

The [detailed-placement contract](https://openroad.readthedocs.io/en/latest/main/src/dpl/README.html)
describes site/row legality, displacement and incremental changes. A small
independent legalizer is a useful first vertical slice; it is not a complete
analytical placer. Our checker must not share the placement algorithm's geometry
assumptions as its sole oracle.

The [extraction contract](https://openroad.readthedocs.io/en/latest/main/src/rcx/README.html)
requires routed geometry and process/corner rules and can export SPEF. Consequently
pre-route estimated RC cannot establish post-route timing closure. RustSTA needs
a validated SPEF ingestion path before it can independently close that boundary.

## Performance, workspace and renderer implications

The following are RustROAD requirements derived from the project constitution,
not measured properties of OpenROAD:

- A mutating command locks `workspace.lock`, checks `--base-rev`, and atomically
  publishes immutable revision tables. Analysis uses read snapshots via `memmap2`.
- Keep mutable physical IR, timing IR and compute views distinct. Share immutable
  technology libraries, and bound every search frontier, iteration count and
  retained report. Never materialize all paths on reconvergent nets.
- A GUI reads immutable render snapshots without holding a workspace write lock.
  Tile/viewport filtering, level of detail and batched GPU instances should scale
  with visible geometry rather than total shapes. Native UI-framework selection
  needs an actual macOS/Linux workload benchmark; this REA study proves nothing
  about GPUI versus another renderer.
- Benchmark the whole command: lock/snapshot read, mmap faults, lowering, compute,
  host/device synchronization, revision publication and report generation.
  Compare wall time, peak RSS and QoR at the same corner, constraints, seed,
  thread count and input identities. Report p95 interaction latency separately
  from placement/routing throughput.

[ORFS flow variables](https://openroad-flow-scripts.readthedocs.io/en/latest/user/FlowVariables.html)
include explicit thread controls, timing/report costs and configurable formal
checks. RustROAD should make those choices visible in its run manifest. A process
exit code is insufficient for a successful stage: require the stage's expected
completion marker, valid artifacts and checked metrics. Publish `unchecked` or
`unverified` for missing DRC/LVS/formal/timing evidence, never numeric zero.

## Executed external placement oracle

The exported RustROAD fixture `.work/tiny.def` was checked using the pinned
OpenROAD's public `read_lef`, `read_def` and `check_placement` commands. The
self-authored [`placement-oracle.lef`](../fixtures/placement-oracle.lef) has one
0.5 by 1.0 micron site and three pinless cell shapes, with 1000 DBU per micron.
It is a geometry fixture, not a fabrication PDK. Inputs contain 24 placed
instances and six rows. The driver is
[`openroad_placement_oracle.sh`](../scripts/openroad_placement_oracle.sh).

| Case | External observation | Outer wall / maximum RSS |
| --- | --- | --- |
| Exported placement | Read 24 components; accepted; exit 0 | 1.17 s / 318,180 KiB |
| Negative control | Second instance moved onto the first; one overlap reported; rejected; exit 42 | 1.53 s / 318,756 KiB |

The negative control reported `u1 (CELL2) overlaps u0 (CELL4)` and diagnostic
`DPL-0033`. The harness requires this overlap-specific observation and a rejected
marker, not merely an arbitrary command failure. Both captures were untruncated.
The negative capture retains the same `/proc/.../environ` permission limitation
for some descendant observations; it does not invalidate the captured placement
diagnostic but prevents a complete process-tree claim.

Full input hashes, immutable input copies, Tcl scenarios, REA captures and time
receipts are in [`placement-oracle`](../evidence/rea/placement-oracle).
[`result.json`](../evidence/rea/placement-oracle/result.json) reports the bounded
geometry result as `pass`, with DRC/LVS/timing explicitly `unchecked`. A positive
placement check produces no violation report file; its successful exit and
completion marker are retained. The negative violation report is preserved.

This supplies an external oracle for the first placement slice; it does not
establish logical connectivity, routed DRC, timing or a complete flow. Both
sky130 and gf180mcu still need their own content-pinned full-library/constraint
campaigns. This investigation ran no PDK corpus, synthesis, simulation, CEC,
routing, extraction or GDS/DRC/LVS.

To rerun the external geometry oracle after exporting a fresh DEF:

```sh
scripts/openroad_placement_oracle.sh .work/tiny.def .work/openroad-placement-oracle
```

The driver refuses a missing pinned image or a non-6.7.0 REA runtime as
`unverified`; it never downloads a replacement. Inputs are limited to 16 MiB and
5000 instances. The preflight estimate accounts conservatively for possible
quadratic overlap-pair storage; both runs are sequential and use the resource
caps described above.

To reproduce a captured scenario with the observed cached REA runtime:

```sh
ulimit -v 8388608
/usr/bin/time -v timeout 60s \
  node /home/cgao/.npm/_npx/a1722018f28f9df2/node_modules/rea-agents/scripts/rea.mjs \
  capture-process evidence/rea/openroad-public-contract.scenario.json --json
```

That absolute npm-cache path is host-specific. Other hosts should use an explicitly
pinned `rea-agents@6.7.0` runtime and an image with the recorded digest, after a
memory preflight. Do not substitute `latest` and call the result reproduced.
