# Physical I/O, supply, tie and row preparation resource contract

Written before implementation. All geometry and derived coordinates use i64 DBU
with checked i128 intermediates. No timing paths, cuts or SAT clauses are created
by physical preparation. Independent CEC keeps its existing solver caps.

The existing caps remain authoritative: 250,000 total instances, 250,000 nets,
1,000,000 pins/expanded bits, 100,000 rows, 16 MiB published metadata. Added tie
and filler cells count toward the same total instance cap, not a second allowance.
Constant rewriting has a work/bit limit and depth <=32. One tie driver per load
is the initial policy; no unbounded high-fanout materialization is permitted.

I/O preparation is capped at 100,000 pins and 400,000 side probes with bounded
name bytes. It uses arithmetic side capacities and sorted validation, not an
all-pairs overlap matrix. Row packing and filler creation use at most a bounded
number of instances plus rows. Rail validation stores bounded intervals, sorts
them and checks union coverage/polarity. Generated supply wires/vias are O(rows),
with an explicit segment budget; the independent connectivity checker caps its
rectangle/via graph and sweep events.

Conservative additional peak memory beyond the 1.5 GiB import allowance: 128 B
per prepared instance (32 MB), 128 B per I/O pin (13 MB), 128 B per row/rail
interval plus bounded geometry references (64 MB), 256 MB temporary AST copies,
and 128 MB serialization/validation/mmap overhead. A 2 GiB runtime target covers
the default budgets with the metadata cap often rejecting a large case earlier.
Libraries and masters are shared immutable data; no per-cell PDK clones and no
per-thread graph replicas. Initial dual-PDK fixtures stay below 2,000 cells,
16,000 endpoints and 64 rows, expected native RSS <512 MiB.

Before builds/runs inspect `free -h`. Build jobs=2, test threads=2, Rayon=2.
Use /usr/bin/time -v and timeout 180s with an 8 GiB VAS ceiling for pure-CPU
builds. Native verification children retain 2 GiB VAS and 30s limits. The pinned
OpenROAD container retains 2 GiB physical/no-swap, 8 GiB VAS, two CPUs and an
inner deadline; do not confuse virtual reservations with actual RAM. Preserve
RSS receipts. Runs estimated above 4 GiB must use the constitution's scale lock.

Verification claims are separate: row/rail geometry and supply connectivity are
not foundry DRC, well/tap checking, IR drop or EM. An unexecuted check stays
unchecked. No CTS, signal routing or parasitic extraction is implied by this phase.
