# Opt-in bounded detailed-path bend pricing

The track router accepts optional `bend_cost` and `max_labels` fields:

```json
{"bend_cost": 840, "max_labels": 2000000}
```

Omitting `bend_cost`, or setting it to zero, retains the legacy point-only
search, queue IDs, cost saturation, and serialized plan encoding. A positive
price requires an aggregate discovered-label cap from 1 to 64,000,000; the price
must not exceed 2^40. The private pilot keeps original Metal3 access, via cost
840 and hard-search weight 4. It changes no default heuristic or geometry rule.

Each enabled search labels a graph node by its last planar heading: none,
east, west, north or south. A via preserves that heading, including through
multiple consecutive vias. A planar edge that changes a nonempty heading pays
one bend price; a U-turn counts as a change. The initial heading is empty.
This prices only the new detailed sink path. Fixed pin-escape junctions and
joins to an existing routed tree receive no additional junction fee.

Different headings at the same node need separate costs and closed flags. For
example, reaching a node at cost 5 from the north can lose to an east arrival at
cost 6 if the next eastward edge costs 1 and turning costs 10: completions cost
16 and 7. A single cheapest cost at that node would discard the better route.
The bounded queue and parents therefore reference label IDs; graph operations
resolve those labels to physical nodes. Reconstruction still emits graph edges.

The existing minimum-area checks walk the same bounded 64-edge parent context
using label parents, retaining run lengths, preceding via kinds and escape
checks. Geometric probes, cleanup and route validation remain mandatory. Heading
labels establish the bend-cost context; they do not prove optimal dominance for
every path-dependent rule. Weighted heuristic search is also incomplete. The
enabled router may return `NO_ROUTE` or a structured budget error; it must never
relax geometry to avoid either result.

## Resource estimate

Labels use a sparse `(node, heading)` index and a central arena, with one parent
ID per label. The cap counts all headings together in one search, is checked
before new allocation/insertion, and is reset between existing search stages.
There is no per-path vector or fivefold implicit-grid allocation. Existing
per-search/aggregate expansions, heap, reconstruction and spatial work caps
remain active. Enabled path additions fail explicitly on integer overflow.

Admission replaces the legacy search-record/frontier terms with at most
`min(max_labels, 5 * graph_nodes) * 128` bytes for labels and
`min(max_labels, 5 * graph_nodes, max_heap) * 128` bytes for queue/index growth.
An additional 4 KiB covers container headers and minimum allocation floors.
Other access, guide, history, routed and geometry terms remain unchanged. For
the private IHP pilot, 2M labels plus 2M frontier entries are conservatively
512 MB and replace the prior 2.36 GB record/frontier estimate. Expected total
admission is about 2.2 GiB; the original 4 GiB RAM / 1 GiB swap diagnostic guard
and all work limits remain unchanged. Label/heap type sizes and measured RSS
must remain below these conservative allowances.

Tiny fixtures use less than 1 MiB each. Builds/tests use two jobs/two test
threads, CPUs 24–31, explicit cgroup/time limits and the hash-bound GNU timer.
Private receipts are in superproject `.work/perf-20261030/qor-cost/`.

## Acceptance scope

Tests cover different-heading dominance, via chains, the aggregate cap before
insertion, cap reuse, overflow, minimum-area validation and zero-policy
encoding/output equality. Real SKY130/GF180 controls and a fresh IHP replay are
separate gates. Candidate physical QoR requires area, HPWL, wirelength and vias
all to avoid regression. Foundry DRC/LVS/antenna, CEC and timing remain required
before a full-flow acceptance claim.

The final bounded workspace suite passed 442 tests, with 11 existing ignored
tests, in 59.10 seconds at 805128 KiB peak RSS. Formatting, all-target/all-feature
Clippy with warnings denied, rustdoc, and release build passed; release took
83.87 seconds at 988772 KiB. The frozen private executable SHA-256 is
`5214d7e40f14defedd4ded25ffaef366333f68109e8f0ba9184b6dc23e4d43e7`.

The unchanged default dual-PDK oracle passed all 36 commands and six independent
DEF geometry comparisons. Against the fixed legacy executable, 12 complete
DEF/netlist files and six complete routing objects (including counters) match
exactly. Positive bend controls passed 28 commands on both PDKs with identical
2/16-thread outputs, historical netlists/nonrouting content preserved, and an
independent DEF parser. Enabled SKY130 routing intentionally differs from legacy;
the retained old-versus-enabled equality trial rejects that difference. The
positive controls establish native validity/determinism, not foundry signoff.
