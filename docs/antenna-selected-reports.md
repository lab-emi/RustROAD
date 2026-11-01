# Explicit selected antenna reports

The default `check-antenna` inventory and legacy `--antenna-report` repair binding
retain their previous JSON, cumulative allocation ceiling and failure behavior.
A complete inventory is never inferred from a selection or from a clock-only route.

For an explicit group, write a JSON array of distinct routed logical net names:

```sh
rustroad --workspace design check-antenna --selected-nets nets.json > group.json
rustroad --workspace design repair-antenna --base-rev rev4 \
  --selected-nets nets.json --selected-report group.json
```

Selections are bounded to4096 names and a2MiB selection input. Each name is at
most4096 bytes. Unknown, duplicate or unrouted identities fail. Report statuses
`subset_clean`, `subset_violated` and `subset_partial` refer only to that group.
The optional `selection` record contains canonical names, their SHA-256, a streamed
fingerprint of the actual source/placement/connectivity/routes, effective analysis
limits and an explicit observation for every requested name. No-geometry, no-gate,
no-applicable-rule and hierarchical partial states are retained, not omitted.
A group with no native gate/rule observation cannot serve as clean repair evidence.

Filtering takes place while collecting retained conductor shapes. The complete
geometry scan still consumes work. Source PDK files are reread and checked against
the imported hashes. Model work remains cumulative across binding, before rows,
candidates and after rows. Existing planner and final geometry-validation ledgers
retain their separate hard bounds; their counters must not be presented as a
single resettable allowance.

`--selected-report` is separate from legacy `--antenna-report`; passing one format
to the other binding path fails. Both flags together fail. Selected binding freshly
recomputes exactly the reported selection at the requested current revision. It
requires equal source identities/fingerprint, selection, margin, limits, per-net
coverage and complete check-row values with multiplicity. Incoming numeric ratios
never drive planning. Dropped, duplicated or forged rows and changed source,
selection, revision or margin fail before a revision is committed. What-if reports
cannot authorize repair. Existing hierarchical-model rejection remains enforced.

The repair's optional `--max-antenna-rows` inherits the selected report's exact bound when
omitted. An explicit conflict returns `REPORT_LIMIT_MISMATCH`. Other analysis
limits must equal the check's frozen limits; the current CLI exposes matching
`--max-work`, while geometry-limit overrides are available through the typed
request. The global `--max-rows` still limits workspace placement rows. The legacy omitted antenna-row argument retains its100k bound.

The new explicit path uses a hard128MiB simultaneously live report allocation
budget. Reservations follow owned row vectors, repeated names/PWLs, metadata,
parsed input, before/current/best candidate reports, output records and streaming
serialization scratch. Input rows are bound one at a time; the parser does not
materialize a full `serde_json::Value` or duplicate full typed row inventory.
The selected metadata field is separately bounded to2MiB. Geometry/model scratch
still has its existing shape/pair/union-grid and OS limits. A reservation is released
only with the owned buffer. It does not reset model work or cumulative allocation.

`stats.report_memory_bytes` retains its conservative cumulative per-row meaning.
The optional `report_memory` record distinguishes live bytes, their peak, cumulative
reservation bytes and cumulative row bytes. Cumulative bytes can exceed128MiB
when released buffers are reused; live bytes cannot. These are conservative logical
allocation reservations, not RSS. Resource-capped acceptance runs must also measure
actual RSS/cgroup peaks. Returned report clones created by callers are outside the
engine operation's allocation scope.

A flow may partition the canonical routed-net inventory into bounded disjoint
groups. After every mutation, earlier reports are stale. Once mutations stop, all
groups must be checked again at the same finalHEAD; exact union/disjointness and
explicit partial/no-observation coverage must be verified. The routing creation
report may be an ancestor, but final geometry/connectivity, exports, parasitics and
timing must bind the final revision. Original full foundry DRC, all process-antenna
rules, device-level LVS and extracted timing/DRV remain separate mandatory gates.
