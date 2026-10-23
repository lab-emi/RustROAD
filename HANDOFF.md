# RustROAD handoff

## 2026-10-23 — v0.2.0 real-PDK connectivity (current)

- Implemented `import-pdk`, `export-verilog`, connected `export-def`, and
  `export-connectivity`. `rustroad-formats` adds exact integer-DBU LEF geometry
  and flat mapped-Verilog readers; warnings retain unsupported technology rules.
- Mapped vectors, escaped identifiers and assign aliases are linked with bounded
  bit expansion/union-find. Per-DB generational instance/master/net/pin arenas
  remain separate from the CSR representation. Constants needing tie cells,
  explicit supply wiring, bidirectional signal semantics, nonzero macro origins
  and multi-height cells are rejected explicitly in this milestone.
- Connected workspace schema 2 retains source hashes, used masters, logical AST
  and connectivity. `pins.rbin` is 16-byte u32 owner/net records;
  `net_offsets.rbin` contains u64 offsets with a final sentinel and
  `net_pins.rbin` contains u32 pin indices. Schema 1 remains readable.
- On read, bounded AST re-linking must equal stored ports/nets/pins; binary tables
  must match that model. A regression test changes only a logical pin connection
  to ensure Verilog and DEF cannot silently diverge. Public connectivity APIs
  have an instance cap as well as net/pin/shape budgets.
- 36 ordinary tests passed, plus both explicitly enabled real-LEF corpus tests;
  strict Clippy and release build passed. The corpus covers 437 Sky130 and 229
  GF180 macros plus 2,157 polygons from a GF180 standard LEF corpus. Final source
  hashes and build/RSS receipts are in `docs/evidence/pdk-validation.json`.
- The final RustSys -> RustROAD -> RustSTA campaign on true Ciel/open_pdks inputs
  passed with native/external CEC, 512-vector-per-PDK Verilator checks, OpenDB
  endpoint/direction/DBU/master-size/placement comparisons, and deliberate wrong
  pin connections in Verilog and DEF. Used LEF pin/OBS geometry is independently
  identical: Sky130 4 masters/30 pins/189 rectangles, GF180 5/36/169.
- Sky130: 16 cells, 26 nets, 74 signal endpoints, 138 stored pins; GF180: 20 cells,
  30 nets, 82 signal endpoints, 162 stored pins. Stored counts include unconnected
  LEF supply pins, whose geometry is preserved. Do not describe them as completed
  power networks. Top-level logical DEF pins are explicitly unplaced.
- Final 70-receipt campaign: 12.61 s complete wall time, 289,556 KiB maximum host
  process RSS. RustSTA/OpenSTA ideal-wire setup/hold differences <=6.029405e-7 ns
  (Sky130) /8.695039e-7 ns (GF180), tolerance 0.001 ns, zero unconstrained
  endpoints. Pulse-width/minimum-period warnings remain visible. Incremental
  checks are no-op updates only, not changed timing ECO validation.
- References: `docs/pdk-report.html`, `docs/pdk-import.md`,
  `docs/pdk-import-budget.md`, and `docs/evidence/pdk-import-results.json`.
  Full local receipts: `.work/pdk-import-geometry-final/`. This work did not alter
  or rebuild the sibling RustSys/RustSTA sources or frozen release targets.

### New lessons

- Sky130 source DBU is 1000/um and GF180 is 2000/um. Coordinates are decimal
  microns in LEF, not source-DBU integers. Reject nonintegral conversions instead
  of silently rounding. Validate selected site and each master separately.
- Verilog `alias` is a reserved keyword; use a valid or escaped identifier in
  fixtures. Default expression depth is 32 to fit nested serialized ASTs within
  the JSON recursion limit. Node/work budgets are separate from final pin count.
- Avoid O(ports * aliases): build the alias-to-net map once. Alias union uses size
  balancing/path compression and preserves a deterministic canonical name.
- OpenROAD's allocator and Go-based Podman can fail virtual reservations under a
  2 GiB AS cap even at low RSS. Keep the real container memory cap at 2 GiB/no swap,
  allow 8 GiB VAS for that reference process, and set GOMAXPROCS=2. The native
  Rust/timing tools keep their existing smaller limits. Preserve failed probes.
- I/O placement, power-grid construction, CTS, routing, extraction, GDS and DRC/LVS
  remain outstanding. The successful STA scope is ideal wire/clock, not extracted
  post-route timing. The earlier section below is historical v0.1 evidence.

## 2026-10-23 — initial physical workspace and verified subflows

- New clone of `git@github.com:lab-emi/RustROAD.git`, initial commit `625b5f4`;
  upstream initially contained only Apache-2.0 LICENSE.
- Parent `/mnt/data2/cgao/git/AGENTS.md` is authoritative. Read this file and
  `docs/memory-budget.md` before further work.
- Clean room: no OpenROAD/Yosys/ABC/OpenSTA implementation source, disassembly,
  decompiler output or algorithm porting. REA is restricted to black-box process
  capture and public documentation; it is not authorization to reverse engineer
  reference internals.
- Existing RustSys/RustSTA release binaries are frozen evidence. Do not rebuild
  into their targets or modify their working trees. Use Rust 1.97.1 through
  `/home/cgao/.cargo/bin/cargo`, not `/usr/bin/cargo`.
- The frozen RustSTA binary used for this baseline has no SPEF import. The sibling
  RustSTA source checkout already contains unrelated, uncommitted SPEF work on
  `feat/spef-postroute`; do not overwrite it or assume it has passed this baseline.
  Extracted timing and full RTL-to-GDS signoff remain unverified.
- `answer-me-with-html` is requested by parent preferences but is not an installed
  skill in this session; provide a local HTML engineering report directly.

### Implementation and validation

- Four crates: core (integer DBU, per-DB interner, typed arena, bounded fixed-order
  row packing), workspace (locks/revisions/immutable mmap tables), flow (bounded
  pinned-file assessment), CLI (`rustroad`). No GUI/Python dependency in core.
- Import format is intentionally a strict, small JSON geometry subset. No physical
  connectivity, LEF importer, global placer, CTS/router, extractor or GDS writer
  is implemented. `export-def` emits rows/components only and cannot prove signoff.
- 17 Rust tests pass, including concurrency, failed publication recovery and
  corruption detection. Clippy with warnings denied and release build pass.
- Independent DEF checker plus planted defect controls pass. OpenROAD accepts the
  24-cell geometry fixture and specifically rejects a forced overlap. Both use
  a hand-authored synthetic LEF, not fabrication PDKs.
- Dual-PDK XOR/register smoke passes RustSys/native CEC, independent Yosys CEC
  (including bad-design control), Verilator simulation (including bad-output
  control), RustSTA full/independent checks and same-scope OpenSTA differential.
  Original timed Liberty inputs are retained. A verification-only Sky130 copy
  removes wire-load metadata for the ideal-wire differential; this is not an RC
  model substitution for production. Incremental test is no-op only. Pulse-width
  and minimum-period unsupported diagnostics remain visible; no signoff claim.
- 50,000-cell/2,500-row end-to-end workload, including import, placement,
  publication, mmap/export and independent check: single observation 0.373 s
  (1 worker), 0.373 s (2 workers), same DEF SHA-256. Outer peak RSS 62,592 KiB.
  No OpenROAD speed comparison, GPU or macOS measurement was made.
- Evidence: `evidence/rea/`, `docs/evidence/`, and full local `.work/` receipts.
  Read `docs/report.html`, `docs/architecture.md`, `docs/flow-integration.md` and
  `docs/gui-architecture.md` for scope and next gates.

### Pitfalls caught during development

- Clap forbids a required global argument. Parse workspace as a global option,
  then explicitly reject missing workspace before any operation; CLI test covers
  the argument layout and mandatory mutation base revision.
- Seal invariant-bearing DB/snapshot fields. Publicly replacing the interner or
  metadata could invalidate a previously checked snapshot. Public DEF export
  still validates its complete input to avoid divide-by-zero and illegal output.
- Bound serialization while writing, not after building an arbitrarily large
  Vec. Check regular-file types before opening file inputs; flow identity reads
  additionally use nonblocking Unix opens against FIFO substitution races.
- Use buffered fixed-record and DEF output; one syscall per field would dominate
  a simple linear placer. This is included in the end-to-end benchmark.
- Keep `site.name` in the format and synchronize independent checker syntax with
  the declared identifier contract. Default `rustroad_site` is synthetic only.
- OpenROAD `-version` prints `unknown`. Pin OCI image and target binary SHA-256.
  REA's executable hash covers the podman wrapper, not its nested target. REA
  reports some /proc EACCES observations; retain that limitation in evidence.
- Workspace readers currently hold shared locks through snapshot lifetime.
  A GUI lease/GC protocol is future work; do not claim lock-free publication.
- Never rebuild the sibling benchmark targets. This task changed no RustSys or
  RustSTA source or version; future changes need their independent dual-PDK gates.

- HTML report syntax and local links were checked. Browser file: preview was
  blocked by URL policy; do not claim rendered visual QA or work around that block.
