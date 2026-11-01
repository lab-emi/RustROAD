# Optional exact routing queries

Two optional `track_router` fields change the query representation without
changing conductor geometry, spacing, preferred direction or the search policy:

```json
{
  "own_pin_index": "layer_ranges",
  "instance_index": "template_layers"
}
```

Omitting either field retains its legacy implementation and serialization. Own
pin ranges restrict the existing sorted order to a layer and reject only exact
empty-layer or integer-bounding-box disjoint queries. Allowed escape endpoints
and the strict overlap predicate are unchanged. Static instance bins use each
template's actual layer presence while retaining the original world bbox,
instance insertion order and template obstacle ordinal order. Cut layers and
FS transforms use the same integer geometry as the legacy queries.

Both indices retain the original 24M bin-entry limit and the configured whole
router memory/comparison/expansion ceilings. Admission counts layer replication,
page/dense grids, marks and construction space. New index construction work is
charged to the existing cumulative comparison ledger. Restoring an old replay
never resets that ledger or charges legacy reconstruction as new index work.
Dynamic shape IDs, tombstones and remove/restore semantics are untouched.

For a saved state, `replay-route --query-indices-config FILE` applies this exact
representation only after validating the original captured source and limits.
Changing the original routing config instead remains stale input. The explicit
report includes `query_indices`, `query_index_build_work` and
`query_index_estimated_bytes`; normal legacy reports omit them. None of these
read-only local experiments publishes a workspace revision or proves full-design
routing, antenna, DRC, LVS or timing closure.
