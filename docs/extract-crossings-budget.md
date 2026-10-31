# Bounded virtual-crossing CPU prototype

Base3cc2994 contains the qualified six-boundary profiler. Its single profiled
extraction observed25.1206 s in virtual crossings and26.7492 s in end/pad pieces.
SPEF, electrical statistics and legacy logical work matched; live-RSS-derived
held/total admission varied, with the original3584 MiB limit unchanged. No25 s
saving, chosen architecture or new full-flow pass follows from that observation.

This prototype adds explicit crossing workers (default1, maximum16) and a
crossing scratch allowance (default32 MiB, maximum32 MiB). Default1 immediately
calls the original serial implementation and emits no new parallel telemetry.
An opted-in library call outside an already established Rayon pool also uses
serial execution; it must not accidentally create an unbudgeted global pool.
CLI acceleration remains bounded by actual global --threads.

## Memory admission and lifetimes

Let I be the actual source instance count and V the actual virtual-shape count.
N=ceil(V/64) u64 words are the original canonical activation bitset. Workers
borrow the immutable coarse index/templates/shapes. Each owns I u64 marks and a
persistent N-word private bitset. The first bitset occupies the original output
allocation; only W-1 bitsets are additional. Keep exactly64 query slots, never a
whole-design query/candidate/contribution list. Merge private bitsets once at the
end, retaining the first allocation as the result.

Before allocations, conservatively admit:

`W*(8I+2MiB stacks/headroom) + (W-1)*8N + 64*128 + 64*256 + 4096` bytes.

The256 B/slot separate reserve covers bounded Error string capacity; inline slot
size must be at most128 B. Runtime Vec capacities, headers and actual scratch
allocation are checked before any query launches. Allocations are fallible.
Allocation/capacity failure after initialization returns a structured budget
error with no successful receipt, rather than retrying and claiming control0.
Current reachable query/transform errors use short static strings. Unexpected
larger error payloads fail with a structured budget diagnostic in their original
query position; that future guard does not prevent an upstream error constructor
from allocating first, so an input-sized diagnostic must not be added without
new admission. There is no unsafe conversion or atomic bitset.

The existing fixture has I=85,836 and a receipt-derived V upper bound768,669
(observation only, not generic admission). W8 then needs at most22,972,008 B,
below selected final query scratch32,110,976 B. Crossing scratch must be dropped
before immutable-index replacement/final candidate Batch allocation. The whole
admission is baseline+max(crossing_scratch,query_scratch), retaining original
3584 MiB and existing query scratch settings. Runtime admission uses actual I,
V, lengths and capacities and the minimum of available original memory and the
new <=32 MiB allowance. Source-only work has no large-design allocation.

## Deterministic work ledger

stats.work remains the original logical bin/template/query/interval units.
New optional parallel-crossing telemetry distinguishes reserved_control_upper
from actually charged control_work and exposes full-extraction legacy_work,
total_work and original max_work. Root's original aggregate cap governs
legacy_work+control_work through every later stage.

New charged units are defined, not claimed as CPU instruction counts:

- one per newly zeroed worker-instance mark;
- one per extra private-bitset word initialized;
- one per worker/slot initialized;
- one per parallel query dispatched and one per ordered result consumed;
- one per launched parallel tile;
- one per private bitset word OR merged.

Fixed bounded admission/upper-bound probing is separately reported and bounded;
serial fallback/control0 does not mean no CPU work occurred. Queries are streamed
in exact owned-shape then other-modeled-layer order. Admission may inspect only
a fixed64-query tile, without executing its geometry. A first-tile rejection
falls back before worker initialization or speculative geometry. Initial control
reservation is an O(1) upper bound from retained_shape_count*modeled_layer_count,
plus worker initialization and mandatory merge. Reserve all mandatory merge work
before initializing workers. Each parallel tile additionally reserves its full
original query-work bound before dispatch; an invalid/overflowing/unaffordable
bound selects serial processing before that tile's geometry executes.

Later serial fallback retains completed private results and processes current
and remaining queries once in original order, protecting the already reserved
final merge. No failed geometry is silently retried for free. Worker-local legacy
work/errors are consumed in original query order; the first original query error
wins. All probe expansion, strict final intersection, all-layer visits and
canonical IDs are unchanged, including electrically zero-effect conductors.

Initial probing visits at most64 owned shapes and64 query descriptors. If that
prefix cannot supply a complete first tile and more owned shapes remain, use
the original serial implementation rather than scan a whole design merely for
admission. `preflight_owned_shapes` and `bound_probes` make this scope explicit.
After admission, the one streaming owned-shape traversal replaces the original
loop; no second global scan/tape is retained. Instance dedup generations differ
internally but are fresh per query and bounded by the original work ceiling;
private queries never mutate the original mark vector. A subsequent serial
query increments past all generations in that original vector, so no stale mark
can suppress an instance.

After crossing, all remaining extraction/index/query/interval limits use
original_max_work-actual_control_work, including the retained InstanceGeometry
query bound. At finalization total_work is checked again and reported honestly.
Extra control work can exhaust a tight original aggregate cap; it never creates
new allowance or changes the original individual f64 addition order.

## Validation plan and current scope

Tiny source controls will compare original serial crossing bits against explicit
brute-force world rectangles, including bit indices63/64, tangency, transformed
instances, dense repeated hits, all routing layers, serial/thread/memory fallback,
tight work and error ordering. Tiny complete extractions will compare exact
electrical output and legacy work, with the new ledger distinguished.
No fixture may enumerate unbounded paths or allocate whole-design pairs.

Expected tiny runtime state<64 MiB, one/two test workers, bounded fixtures. Tiny
tests may use CPU10,12 under1 GiB/60 s/GNU time; heavy Cargo compilation and any
large replay await root source review. Actual correctness and performance remain
unverified until those checks. No GPU code, production/default dispatch change,
cache, tap insertion or P1 source is included.

The source freeze contains the implementation and the proposed tiny Rust controls.
Nine focused Rust controls and one complete tiny extraction control are present.
Only Rust formatting/parser checks and source review have run at this point,
plus 54,075 independent scalar cases counting reserved versus charged control
units for launched prefixes and a byte comparison of the unchanged serial body.
That external arithmetic/source check took 0.03 s and 21,028 KiB peak RSS;
rustfmt took 0.09 s and 48,568 KiB, under CPU10,12/1 GiB/60 s guards.
There has been no Cargo build/typecheck/test, no large-design replay, and no
performance or dual-PDK qualification. The next step is independent source review,
then a separately authorized bounded build and execution of these controls.
