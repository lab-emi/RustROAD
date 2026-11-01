# Single-process grouped antenna repair

`repair-antenna-groups` runs the native selected-check/jumper policy against one
exclusively locked, decoded workspace. It reuses static placement, PDN, LEF and
routing configuration and refreshes the source fingerprint whenever routes change.
Every successful jumper attempt publishes the same real revision as the legacy
runner, using the existing validation and commit paths. Before each repair, it
rechecks the exact eligible selection and binds the complete legacy report.

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

The output directory must not exist. All fields are required. Budgets can be
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

Raw check/repair reports use exactly the legacy CLI formatting. This preserves the
selected-report parser's byte-based work charge and provides directly comparable
artifacts. A compact `summary.json` records actual attempts, budgets, unresolved
nets and explicit unchecked signoff scope. An unresolved native result exits
successfully with `status: "unresolved"`, matching the legacy pre-screen policy.
Foundry antenna, DRC, LVS and timing signoff remain separate checks.

Failure preserves completed real revisions and partial artifacts; a successful
summary is required to accept the run. There is no background process or stdin
protocol. The exclusive workspace lock covers decisions, revision publication
and final evidence flush. Existing revision ancestry is retained instead of
inventing headers to force a final design hash.

`scripts/antenna_group_repair_replay.py` copies the initial real ancestry from
immutable grouped-runner evidence into a new workspace, executes the command,
and independently verifies every original operation report byte, selected
attempt/order/work outcome, changed revision side-table, and final design,
inventory and model. Run it under a 4 GiB resource scope, timeout and GNU time.
The initial estimate is <1.5 GiB: one decoded/prepared state, original bounded
planner copies, one 128 MiB report and <128 MiB bounded name/attempt/futile tables.

This command introduces a new compact receipt protocol. Existing product flow
recipes and their lineage qualifiers must explicitly adopt it; a standalone byte
replay does not establish new public full-flow qualification or a speedup claim.

The frozen run16 replay passed with the final candidate: 64 attempts, 52 changed
real revisions and 128 final groups. All 439 original raw operation reports,
every changed revision side-table, final design, inventory and connectivity model
matched byte for byte. Outcome, selection order, work charges and the
`attempt_budget` stop reason also matched. The native command measured 181.257 s;
copying and independent complete-file verification together measured 188.47 s,
with 733,392 KiB peak process RSS in a 4 GiB scope. An earlier pre-polish candidate
measured 218.349 s; these runs overlapped other work and are not a controlled
speedup comparison. The historical antenna step was 858.0 s. The <=60 s target
and new public full-flow qualification remain open.

Final replay evidence is in the superproject's
`.work/perf-20261030/antenna/full-replay-final/receipt.json`. The final candidate
binary is retained by SHA-256 under
`.work/perf-20261030/antenna/bin/47caec009e3a7121128e857c06660909b50bcd7e0e7c79072cb27bd0f380b468/rustroad`.
The unchanged final design, inventory and model hashes are respectively
`f34b3205d32b1bfef521974e2b9c241970496187c29d7f5089566785348427a3`,
`88e02b6970310e58a3c05ef01f445bdf08fec5cff515c02dcdc818ef94c3d1cc`, and
`8f8c9e89d2fc53944a707dc3f8cf2787c197750a7a9c05073f204d9ceaf38648`.

Validation also passed 28 antenna integration tests, 46 existing workspace/storage
unit tests, all-target workspace/CLI Clippy with warnings denied, and formatting.
Independent review caught the reduced exact-fit operation-budget edge for summary
publication; a regression test confirms that final lineage metadata consumes a
separate bounded allowance, matching the legacy policy.
