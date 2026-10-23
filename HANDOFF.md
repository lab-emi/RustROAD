# RustROAD handoff

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
