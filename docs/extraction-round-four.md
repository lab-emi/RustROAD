# Fourth-round extraction correctness (2026-11-14)

The saved fourth-round implementation is integrated with explicit admission for
its unit-order and completion arrays, corrected candidate-scratch accounting,
and revision-guarded validation-cache writes. Performance acceptance remains
pending; the measurements below came from correctness runs on a busy host.

The tested source is `3a85bbd` (the integration merge is `6e168d2`, preserving
saved WIP `62879ce`). [The receipt](evidence/extraction-r4-correctness-20261114.json)
pins its executable and records the input identities, comparisons and resource
limits. The `nom` IHP input is the restored `rev48` workspace whose six outputs
match the 2026-11-13 publication. A cold extraction with `5aecc5c` reproduced
the published SPEF `468051be9b8106541bdf3eee2e203a428cb65ab1e568a2f43f98a36c2b753932`.

| Comparison | Observed result |
| --- | --- |
| Candidate SPEF at 1, 2, 3, 4 threads | Identical: `acffe7e5d13b5170b7d9dd5614b4b376be1e4b6e26f2053be324095fa913bfe8` |
| Raw RC values/statistics digest at all four thread counts | Identical: `b00ce08974fe438fce81dde63ba5b3c1d9efccbf30f05c3bfc799626d356680f` |
| Structural/connection/unit changes against published SPEF | 0 |
| Resistance values changed | 0 of 294,805 |
| Net total values changed | 19 of 14,178; maximum 1e-9 pF absolute |
| Ground capacitance values changed | 22 of 308,983; maximum 1e-10 pF absolute |
| Coupling records changed | 2 of 170,956; maximum 1e-9 pF absolute |
| Largest relative printed capacitance change | 5.86105e-6 |

Capacitance entries use six significant digits; an f64 rounding change near a
formatting boundary can move the last printed digit. Couplings are listed in
both incident nets, so the table counts SPEF records. The streaming comparator
checks record identities and values without loading a design graph. Its control
tests reject changed connections, units, resistance, large capacitance changes,
non-finite values and exceeded input budgets.

All-target/all-feature Clippy, release builds, rustdoc and format checks passed.
Release workspace tests passed 609, ignored 11 and failed 0. The test command,
including compilation, peaked at 1,527,804 KiB under a 4 GiB cgroup with two build
jobs and two test threads. Ignored tests and external PDK oracles were not counted
as passed. The candidate extraction peaks were 249,960 / 230,848 / 241,760 /
239,688 KiB at 1 / 2 / 3 / 4 threads, respectively; these are individual
correctness samples, not a completed memory benchmark.

Candidate receipts were prepared only in an independent workspace copy using
`verify-workspace --write-receipt --base-rev rev48`. Missing and stale base
controls leave HEAD and the existing receipt unchanged. Full preparation
validation is recorded separately; future warm-cache extraction comparisons
must name that contract, while end-to-end RTL-to-GDS measurements include every
real validation step. Both compared extraction commands used a 4 GiB admission
allowance because the corrected scratch estimate is larger; their geometry,
candidate, node, coupling and work caps are unchanged.

The 15-rotation interleaved A/B against OpenRCX, fresh multi-PDK physical/timing
gates and final performance acceptance remain open. No speedup ratio is claimed
from these runs. Local commands, both original and final build logs, queue
cancellation record, receipt controls and per-thread outputs are retained under
`.work/three-pdk-20261114/extraction-r4/` in the superproject.
