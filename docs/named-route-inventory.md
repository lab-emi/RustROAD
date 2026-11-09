# Named routed-net inventory

`rustroad --workspace DIR antenna-inventory` reads HEAD under a shared lock and
emits `rustroad.antenna-inventory.v1`. It lists the logical IDs actually declared
in `routes.nets`, independently of wire count, pin use, gate area or macro contact.
The output establishes identity coverage only. Antenna, DRC, LVS and timing remain
`unchecked`; an `all_signal_nets` scope is the stored routing scope, not a new
physical signoff result.

The single v1 status field is `status: "inventory_only"`.
`model_logical_net_count` counts the complete native connectivity model, including
logical power/ground entries. `router_declared_net_count` counts route IDs;
`routed_logical_net_count` counts the explicit named logical subset, and
`excluded_nonlogical_supply_tie_count` counts only known synthetic tie IDs. The
last two counts must sum to the declared route count. Prototype08 field aliases
are not part of this protocol.

`nets` is sorted by canonical name, then ID. Each entry serializes fields in the
order `net_id`, `canonical_name`. Known synthetic supply-tie IDs are validated
against their original domain, excluded from this logical list, and counted by
`excluded_nonlogical_supply_tie_count`. Duplicate/unknown IDs or duplicate names fail.

`source_sha256` uses exactly the selected-antenna source recipe and the same two
SHA-checked original LEF sources. `inventory_sha256` is SHA-256 of compact UTF-8
JSON for the tuple array `[workspace_revision, routing_scope, source_sha256, nets]`.
`routing_scope` is `all_signal_nets` or `clock_nets_only`. Python consumers can
independently reproduce the hash with `ensure_ascii=False`, separators `(',',':')`
and dictionaries inserted in the entry field order above.

The explicit command bounds are `--max-inventory-nets` (250000 logical model
nets), `--max-inventory-routed-ids` (1000000), `--max-inventory-name-mib` (16),
`--max-inventory-route-mib` (256), `--max-inventory-work` (10000000),
`--max-inventory-output-mib` (64), and `--inventory-memory-mib` (64). They have
separate argument identities from global CLI limits. Metadata keeps the existing
64 MiB ceiling; source-table/route bytes and route counts are admitted before
restoring or decoding those tables. The whole-snapshot conservative estimate is
capped at 2 GiB. Counted work covers source-object inventory counts, routed IDs
and bounded sorting; snapshot integrity checks and byte hashing also retain
explicit input byte limits. JSON output uses a counting pass and streaming writer.

Consumers must partition this explicit list, retain unobserved and macro-touch
nets, and check every group again at the final mutated HEAD. An early clean group
cannot be relabeled with a later revision. Independent foundry decks remain the
full-design antenna and manufacturing checks.
