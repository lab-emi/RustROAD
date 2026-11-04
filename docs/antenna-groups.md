# Single-process grouped antenna repair

`repair-antenna-groups` runs the native selected-check/jumper policy against one
exclusively locked, decoded workspace. It reuses static placement, PDN, LEF and
routing configuration and refreshes the source fingerprint whenever routes change.
Repairs edit the routes in memory. Each repair that inserts jumpers creates the
next logical revision, under the name a per-attempt commit would have published
(`rev5`, `rev6`, ...: the first slot after its parent with no directory, so an
orphan directory left by an interrupted commit is skipped exactly as a
per-attempt commit skips it); every report carries these names and the
physical-source fingerprint of its state. Only the last logical revision is
materialized: the final routes get the repair's own routing validation (its
explicit-geometry input under the antenna work bound) and one commit, which
runs the full read-time validation before it publishes, with the session's
base revision as parent. Before each repair, it rechecks the exact eligible
selection and binds it to that check (in memory, rows and identities compared).

```sh
rustroad --workspace road --threads 2 --max-expansions 8000000 \
  repair-antenna-groups --base-rev rev4 --plan repair-plan.json
```

```json
{
  "schema": "rustroad.antenna-group-repair.v1",
  "output": "/absolute/path/to/new-reports",
  "max_rounds": 3,
  "deadline_seconds": 1800,
  "policy": {
    "group_names": 128,
    "max_groups": 1024,
    "max_attempts": 64,
    "max_total_jumpers": 4096,
    "max_commands": 4096,
    "max_report_bytes": 8589934592,
    "max_cumulative_work": 200000000000
  }
}
```

The output directory must not exist. All fields except `full_rows` are required. Budgets can be
reduced but cannot exceed the shown hard ceilings. Per-operation checking and
repair use the original 0.1 safety margin, 200-million work bound, 100,000 rows,
128 MiB live report budget, 16 jumpers per net, 512 positions and 32 candidates.
`max_commands` limits model/inventory/check/repair operations; the final summary
has one separate bounded metadata allowance. Report bytes include all raw report
files; summary bytes also remain subject to the aggregate cap.

Names come from the actual routed inventory and are sorted into canonical groups.
The order of rounds, groups, eligible names and jumper decisions is preserved.
Nets touching BLOCK instances and partial-model observations remain excluded from
repair eligibility. An unchanged eligible selection from a no-op attempt is not
retried. Attempt/jumper/work exhaustion and the measured final-check time reserve
stop repair; final inventory, model and complete group coverage still run. A check
that cannot fit its existing work bound fails with an explicit budget diagnostic.
The final model fingerprint and routed membership must match the initial state.

Reports are compact JSON. A check report holds every violated row (and so every
row above the raw LEF limit) plus the per-net worst ratios and the complete
selection coverage, whose `rows` still counts all evaluated rows; `check_rows:
"violated"` marks it. The optional plan field `"full_rows": true` writes every
row. A repair binds its selected check in memory and charges that report's
written bytes as its input work, so `antenna_work` and the totals derived from it
(`reported_work`, `work_budget_charge`) differ from the legacy runner by exactly
the report-size term; report-memory telemetry differs likewise. `model-final` is a
hard link of `model-initial` once their fingerprints match. The summary schema is
`rustroad.antenna-group-repair-result.v2`: it adds `logical_revisions`,
`committed_revision` and `check_rows`. An unresolved native result exits
successfully with `status: "unresolved"`, matching the legacy pre-screen policy.
Foundry antenna, DRC, LVS and timing signoff remain separate checks.

Failure commits nothing; partial reports remain and a successful summary is
required to accept the run. There is no background process or stdin protocol.
The exclusive workspace lock covers decisions, the revision publication and the
final evidence flush. A commit writes every side table as its own file (no hard
links to the parent's identical tables) and its validation receipt to
`receipts/<rev>.json`, outside the revision directory. An orphan revision
directory needs no manual cleanup before a rerun: it is skipped, and its slot
is never reused.

Session state persists across attempts without changing any decision. The jumper
planner's spatial index is built once; after every attempt it is rebased (cut
wires removed, pieces, bridges and vias inserted at their fresh-build positions in
every bin chain) so that probe order, and with it every work charge and the
planner budget, equals a fresh per-attempt planner. Checks and candidate
evaluation read per-net record lists instead of scanning all routes, and charge
the same work. The source fingerprint keeps the SHA-256 state of its
routes-independent prefix and serializes only the routes per change.
`RUSTROAD_ANTENNA_PARANOID=1` re-runs, at every step, the full routing
validation, a fresh planner comparison, the full shape scan and the fingerprint
from scratch; it is a slow diagnostic (minutes, about 4 GiB peak) and must give
identical outputs. `RUSTROAD_ANTENNA_TRACE=1` prints phase timings.

`scripts/antenna_group_repair_replay.py` copies the initial revision from
grouped-runner evidence into a new workspace, executes the command and compares
every report with the original: check reports must equal the original with only
its violated rows kept, repair reports must be equal apart from the input-size
work term, the summary attempts must match exactly, and the committed revision's
route table and side tables must equal the original final revision's (its
`design.json` differs only in `parent_revision`).

Ibex SG13G2 host replay (2026-11-03, rev4 of the native-final postroute
workspace, `--threads 4`): all 64 attempts, 77 jumpers, the 225 unresolved nets
and every repair's jumper list match the producer run; all 404 check, 64 repair,
inventory and model reports compare equal as described (every `planner_work`
included); the committed `rev52` `routes.rbin` has SHA-256
`695e2205a7e0cbfd2bcf5a14cb6d58cf1273671ce0437d344a11da9bbbdfa040` and
`export-gds` of it reproduces the reference GDS. The command took 8.9-9.9 s wall
and 11.2-13.2 s user CPU at 0.81-0.88 GB peak RSS and wrote 126 MB (measured
before the commit regained its read-time routing validation and stopped
hard-linking side tables; see the re-measurement below), against
360 s, 345 s user and 5.79 GB written for the per-attempt-commit runner (the
lower user times are with a receipt-stamped base revision, whose read skips the
full validation). Through the antenna_session wrapper the step took 11.7 s (from
388 s) and its independent verifier 1.9 s (from 21.4 s). Reports are 31 MB
instead of 780 MB, and the workspace grows by one revision instead of 48
(4.7 GB). The paranoid run gave identical reports and summary.

Re-measurement after the commit regained its read-time routing validation,
writes independent side tables and keeps its receipt outside the revision
(same host, interleaved with the previous build, unstamped base, load 4.5-6.3):
11.8-12.7 s wall and 13.8-14.0 s user CPU at 0.79-0.84 GB peak RSS, 136 MB
written, against 11.1-11.8 s, 12.6-13.1 s user and 126 MB for the previous
build. All reports, the summary attempts, the committed `rev52` side tables and
`routes.rbin` still match the producer run as described above. With an orphan
`rev52` directory left in place, the rerun names its last logical revision
`rev53` (exactly what the per-attempt runner would have published), commits it
with the same routes and exits successfully.
