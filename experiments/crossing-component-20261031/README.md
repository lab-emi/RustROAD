# Bounded crossing component evidence

Evidence-only record, created2026-10-31T00:42:12.059489+00:00. The compiled Rust source remains
`32ced98f399984e993a6e0fb28cb1273aa1e1a26` (source freeze2026-10-31T01:09:27+01:00); the later local
evidence commit adds only this experiment. Original frozen Rust and budget docs
are untouched. Their pending-validation statements describe the earlier freeze.

The frozen ELF is `ad618704b588ad7b5ef4db3668c70679d70dd764823e209eabb4d0d0a3a5e947` /58,882,552bytes.
Its actual source manifest contains570files; the
geometry-profile baseline has568files.
These counts were read and hashed, not inferred from older snapshots.

Component checks:454Rust tests passed;11existing environment/scale fixtures
remained explicitly ignored and are not passes. Fmt, all-target/all-feature
Clippy and docs passed. Fresh SKY130/GF180 controls completed36normal CLI
commands,6DEF geometry comparisons and12min/nom/max extraction calls. All6SPEF
pairs and normalized electrical/geometric statistics and legacy work match.

## One matched complete-extraction pair

Same ELF, global8threads, CPU16–23, original3584MiB algorithm cap and8GiB/2GiB
outer guard; crossing workers1versus8. No other algorithm knobs changed.

| Measurement | Workers1 | Workers8 |
| --- | ---: | ---: |
| Scope/command wall seconds | 126.621584 | 106.542557 |
| Copy/hash plus complete command seconds | 127.217263 | 107.194753 |
| Native GNUtime wall seconds | 126.02 | 105.74 |
| Native peakRSS KiB | 835804 | 834368 |
| User+system CPU seconds | 259.59 | 282.21 |
| Profiled virtual-crossing seconds | 27.410231 | 6.870432 |

Copy/hash-inclusive wall decreased15.74% in this
one pair. CPU time increased8.71%; this is a latency
tradeoff, not a CPU-efficiency improvement. End-extension/via-pad work remains
serial at27.859001s. Boundary timings do not
replace the complete-command measurements.

SPEF bytes are exact: rev56`cd835ce76826f6575cc84fe69602ca7089f5c007565ed2b4de5e98a1cf74d9e8`;
rev4`622f36c70f75f9c84e6375425b980e8cd01650bf0513f69cedc0e83d6d1795d3`,
with rev4candidate scope wall105.521496s.
Legacy work11,019,547,398is exact; additional control work
24,653,719gives total11,044,201,117under the same
100,000,000,000cap. Crossing scratch22,960,920B
and final query scratch32,110,976B have disjoint lifetimes:
the peak uses their maximum, not sum.

The initial large-replay01 location-field assertion failure and brief CPU
overlap remain preserved and are excluded from performance acceptance. Only
large-replay02 is the valid matched pair. Private fixture assembly/hash time
is included separately and together with the complete command.

## Scope and provenance

The independent evidence-only audit hashed2,021actual files /
1,106,488,497bytes, including source snapshots, binaries, raw
receipts/harnesses, both sides of small SPEF pairs and valid large/reference
artifacts. It ran CPU10,12/1GiB+512MiB/60s,0.65s/27,936KiB. It launched no build
or EDA process. See[evidence.json](evidence.json)and its root audit FileID for
the complete registry, exact commands, limits and exclusions.

This is component evidence, not a new full-flow result or default promotion.
The parent's existing accepted full flow remains1099.765s; no new whole-flow
time, physical signoff, QoR improvement or generalized speedup is established.
