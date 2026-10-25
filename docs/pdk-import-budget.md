# PDK import resource contract (written before implementation)

This milestone imports standard technology/cell LEF data and one flat mapped
Verilog module. No reference implementation source is consulted. There is no
timing/path/cut/SAT exploration in the importer.

Inputs are limited to 64 MiB per LEF file, 16 MiB Verilog/floorplan, and 144 MiB
aggregate source bytes. Each parser has explicit token/object/geometry/depth
limits. Geometry uses exact decimal-to-integer conversion; off-grid values fail.
Mapped-expression depth defaults to 32, leaving room for the enclosing JSON
snapshot within serde_json's recursion limit. Deeper syntax is rejected explicitly.
The linking budget is 250,000 nets, 1,000,000 pins/expanded bits, 500,000 alias
operations, 250,000 retained geometry shapes and 250,000 instances. All indices
must fit u32. Work uses bounded signal expansion and union/find, never simple-path
enumeration; array lengths are checked before allocation.

Peak working estimate for the default linked-design caps: nets at 256 bytes =
64 MB; pin/bit entries, endpoint maps, CSR indices, typed arena and validation
state at 512 bytes * 1 million = 512 MB; input buffers + parser state <= 384 MB;
retained library geometry <= 128 MB; bounded metadata serialization, old/new
snapshots, a temporary AST-to-connection reconciliation and mmap pages <= 256 MB. One corner and two workers (no per-worker
library/database replicas) give a conservative 1.5 GiB runtime allowance.
The existing 16 MiB published metadata limit can reject a large linked design
earlier. The first dual-PDK run has <1,000 connected pins and <10 MiB combined
LEF data, with an expected <512 MiB runtime. Immutable parsed technology data is
shared and only used masters are retained, not a deep library copy per instance.

New connection tables use fixed-size little-endian records; counts and byte
lengths are checked before mmap. Validation and CSR lowering are O(nets + pins)
with explicit caps. Union/find with size balancing and path compression is
amortized O((bits + aliases) alpha(bits)); alias operations are capped separately. Cycles of aliases
do not recurse. Driver/coverage checks use counts, not enumerated paths.

Five-crate builds use CARGO_BUILD_JOBS=2, RUST_TEST_THREADS=2, RAYON_NUM_THREADS=2,
an 8 GiB virtual-address limit, /usr/bin/time -v and timeout 180s. Inspect free -h
before builds and dual-PDK suites. External engines retain their existing 2 GiB
address-space and 30-second command budgets. OpenROAD's allocator requires an
8 GiB virtual-address allowance, but stays inside a 2 GiB/no-swap container memory
limit; this is not a larger physical-RAM allowance. Run the two PDKs serially. Record
end-to-end time including parsing, linking, publication/mmap, export and oracles;
do not report parser-only time as complete-flow performance. A suite/process
above the constitution's RSS limit is a defect, not a reason to lift the limit.
