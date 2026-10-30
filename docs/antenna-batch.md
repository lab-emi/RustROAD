# Selected antenna check batches

`rustroad --workspace <dir> check-antenna-batch --plan <file>` streams a JSON
array of selected check reports. The plan pins a revision; the command acquires
one shared workspace lock, checks the revision, decodes the snapshot and prepares
the LEF model once. The lock remains held until the output writer flushes. A
revision conflict fails before emitting a report. The command creates no revision.

Each report has the same fields, ordering, rows, counters, source fingerprint and
scope as `check-antenna --selected-nets` with the corresponding analysis limits.
Whitespace differs from the legacy CLI's pretty-printed JSON. Reports remain native
pre-screening observations; foundry antenna signoff is separate and unchecked.

A plan has this form (all fields required):

```json
{
  "schema": "rustroad.antenna-check-batch.v1",
  "revision": "rev56",
  "groups": [["net0", "net1"], ["net2"]],
  "analysis": {
    "max_shapes_per_net": 200000,
    "max_total_shapes": 2000000,
    "max_pair_checks": 200000000,
    "max_union_cells": 4000000,
    "max_rows": 100000,
    "max_work": 200000000,
    "report_memory_bytes": 134217728
  },
  "safety_margin": 0.1,
  "max_work": 200000000000,
  "max_output_bytes": 8589934592,
  "snapshot_memory_bytes": 2147483648
}
```

Groups are processed serially in the supplied order. Each contains 1–128 names,
sorted and distinct. The plan permits at most 1024 groups, 100,000 total name
entries and 16 MiB of name bytes. Plan input is capped at 16 MiB. Repeated groups
are allowed. Each group keeps the existing work, shape, row and report memory
limits; the next check reserves its full per-group work bound against the aggregate
budget. A completed check charges its actual reported work. The snapshot memory
admission has a 4 GiB ceiling. One report is retained at a time, counted against the
output cap before its bytes are emitted, then released.

Failure may leave an incomplete array. Consumers must accept the output only when
the process exits successfully; a partial array is not completed verification.

`scripts/antenna_batch_replay.py` copies one final revision from existing grouped
runner evidence into a fresh directory and compares all report tokens exactly,
including every numeric token and field order. Its caller supplies the resource
scope, timeout and GNU-time measurement. It also verifies that all copied revision
files and HEAD remain unchanged.

This command is a prerequisite for the B3 repair orchestration work. It can replace
same-revision group checks, including the final coverage scan. It does not yet
reproduce the grouped runner's mutation rounds, futile-group policy or deadline
reserve policy, and does not establish the final repaired-design hash gate.

The upfront Ibex estimate was one 0.63 GiB decoded snapshot/model plus one bounded
128 MiB report and at most 16 MiB retained name bytes, conservatively below 2 GiB
with one worker. Fresh two-worker compilation was admitted at 8 GiB estimated peak
and serialized under the superproject scale lock (12 GiB RAM / 2 GiB swap cap).
The release build measured 70.77 s and 990,000 KiB maximum process RSS.

On a fresh copy of run16 rev56, all 128 final reports matched the frozen report
tokens exactly. The batch command took 7.735 s. Copying, executing and checking the
reports together took 17.67 s and 385,000 KiB peak RSS under a 4 GiB scope. The
historical 128 final checks took 218.9 s; a full-flow speedup has not been measured.
The copied `design.json` retained SHA-256
`f34b3205d32b1bfef521974e2b9c241970496187c29d7f5089566785348427a3`.
Evidence is in the superproject's `.work/perf-20261030/antenna/final-replay/receipt.json`.

The revision header is part of `design.json`'s bytes. A future repair session must
preserve real ancestry when reproducing that hash; fewer publications require a
separate storage/ancestry design. Never substitute invented parent metadata.
