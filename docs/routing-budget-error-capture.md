# Optional global-budget witness

The explicit replay policy can request one failure witness:

```json
{
  "path": "/absolute/new-artifact.json",
  "net": null,
  "round": null,
  "max_bytes": 536870912,
  "phase": "post_budget_error"
}
```

This observes `Error::Budget` from the repair hard search, owner stage, soft
search, alternate-root work or alternate soft search. The original error is
returned. No workspace revision is published and no comparison or expansion
ledger is reset. Earlier setup, initial access, final cleanup and validation
errors are outside this repair-stage witness scope.

The same explicit policy also retains the existing witness when final
`Outcome::Limited` is returned as a per-search budget error. Its actual snapshot
phase remains `post_failure`, its known `failed_pin` is retained, and it has no
`budget_error` context. A direct budget error instead has phase
`post_budget_error`, the partial-current-net context and a null unknown pin.
The loader enforces these different contexts; the requested policy is not used
to relabel the observed phase. A successful route writes neither witness.

The streamed, atomic, no-overwrite artifact retains the original source,
limits, current net/round, geometry flags, terminal order, guide, history and
actual ledgers. `failed_pin` is null when the error did not identify a pin.
`budget_error` records the explicit source stage/code, a UTF-8 message bounded
to4096bytes, a truncation flag, and `contains_uncommitted_current_net_state:true`.
`last_completed_round` describes earlier completed rounds; the captured geometry
and history can include partial work of the current round. This is not a
committed-round checkpoint or a replay of the original failed call.

Read-only replay preserves this phase/context and original remaining budgets.
A nearly exhausted budget can immediately refuse a new attempt. No per-search
override raises the captured aggregate ledger ceiling. A forged/missing context,
stale source or changed limits is refused. Normal pre-soft and post-limited
artifacts retain their existing encoding and behavior.

The existing512MiB output/4M dynamic shapes/4M history/100k terminal+rip counts
remain hard limits. The writer streams borrowed geometry and does not clone it.
If capture fails, a separate bounded `REPLAY_CAPTURE_FAILED` stderr diagnostic
preserves the capture failure; the command still returns its original budget
error. No failure becomes a success or a partial route revision.

For this broader policy, failed limited-state capture also preserves the primary
per-search error and emits `REPLAY_CAPTURE_FAILED` separately. The legacy
`post_failure` policy retains its previous capture/error behavior.
