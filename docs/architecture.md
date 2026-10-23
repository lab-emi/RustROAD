# RustROAD architecture and implementation gates

## Current boundaries

`rustroad-core` owns the physical geometry model, its per-DB interner and
generational instance arena. A separate contiguous CPU view supports the initial
fixed-order row packer. Neither core nor workspace depends on Python, GUI, or an
external EDA tool. Native Rust code is the authority for physical state.

`rustroad-formats` provides bounded, separately typed LEF and structural-Verilog
readers. The LEF AST preserves integer geometry, pin use/direction, sites, layers
and fixed vias; unsupported rules are counted diagnostics. The mapped AST keeps
vectors, escaped names and aliases. `rustroad-workspace::pdk` joins these inputs,
checks exact site/master units, and retains only used masters. Constants requiring
tie-cell insertion, explicit supply networks and nonzero macro origins fail with
diagnostics rather than guessed physical semantics.

Core connectivity has separate generational `InstId`, `CellId`, `NetId`, `PinId`
arenas and a derived CSR view. It expands signal bits with hard caps and unifies
wire aliases with bounded union/find. Driver and endpoint coverage checks do not
enumerate graph paths. Imported LEF supply geometry is retained; no supply wiring
or I/O physical placement is inferred.

`rustroad-workspace` owns lock acquisition, strict schema validation, immutable
revision directories and atomic HEAD replacement. The fixed 40-byte little-endian
instance record is `(id: u64, x: i64, y: i64, width: i64, height: i64)`. Versioned
JSON stores bounded design metadata and row/site geometry. Every reader validates
format, table length and relevant invariants before using a read-only mmap. Files
are created with exclusive creation and synced before HEAD is replaced.

Connected snapshots use format version 2; legacy geometry-only version 1 remains
readable. `pins.rbin` holds 16-byte `(owner_kind, owner_index, terminal_index,
net_index)` u32 records (`u32::MAX` means unconnected), `net_offsets.rbin` holds
u64 CSR offsets including the final sentinel, and `net_pins.rbin` holds u32 pin
indices. The reader checks lengths and fields against bounded metadata. It also
relinks the stored logical AST and requires the same ports, nets and pin mapping
before allowing Verilog or DEF export. A corrupted AST cannot silently produce
a second circuit while the binary pin table still describes the first.

Readers hold shared locks for the lifetime of their mapped snapshot; a writer
holds an exclusive lock through calculation and publication. This intentionally
simple first version serializes publication behind active readers. A production
GUI needs immutable snapshot leases and a compatible reclamation protocol before
shortening lock lifetimes. There is no garbage collector: revisions and up to 128
skipped orphan slots are retained for recovery, not automatically deleted.

`rustroad-flow` assesses file identities and dependencies. Availability never
means that a stage succeeded. Verification scripts invoke external binaries with
bounded time, memory, logs and process concurrency. Existing RustSys/RustSTA
release binaries remain frozen; neither sibling target directory is rebuilt.

`rustroad-cli` exposes these interfaces with shell-safe revision handles. All
mutations require `--workspace` and `--base-rev`. Export writes to stdout so the
workspace stays read-only. Geometry-only placement metrics include absent timing,
DRC/LVS and routing scope rather than fabricated zeros.

## Why start with row packing

The baseline exercises representation, lock/revision semantics, persistence,
format interoperability and independent verification before introducing an
optimization objective. It runs in O(N + R), stores O(N + R), and caps candidate
expansions. It preserves input instance order and packs from left to right while
advancing monotonically through rows. It can reject a design that another packing
could fit; it makes no minimum wirelength, density, routability or timing claim.

## Next gates, in dependency order

1. **Broader technology and physical connectivity.** The initial LEF/mapped
   Verilog path is independently verified on real `sky130_fd_sc_hd` and
   `gf180mcu_fd_sc_mcu7t5v0` data. Extend it with DEF import, physical I/O placement,
   supply/tie networks, row orientation and power continuity, multi-height cells,
   complete required design rules and further PDK variants.
2. **Legalization and global placement.** Keep fixed-order packing as a baseline.
   Define objective/QoR, displacement and density constraints; estimate memory
   for pin CSR, gradient and density buffers first. Deterministic `cpu_ref` and
   independent placement checks precede CUDA or Metal/Accelerate acceleration.
3. **Clocking and routing.** Clock-aware edits require explicit SDC scope and
   connectivity/formal checks. Routing owns integer geometry, a bounded frontier,
   configurable expansion/memory caps and explicit no-route diagnostics.
4. **Extraction and timing closure.** Corner-tagged SPEF must preserve net/pin
   identities, capacitance/resistance units and extracted topology. RustSTA needs
   independent OpenSTA comparisons and incremental/full consistency on both
   baseline PDKs before being used for post-route acceptance. Uncommitted work in
   another RustSTA checkout is not release evidence.
5. **GDS and signoff.** Compare the resulting GDS/DEF/netlists with independent
   connectivity, DRC and LVS checks against pinned decks. Run RTL/gate simulation,
   CEC, post-route timing and physical checks as separate required verdicts. A
   generated GDS alone does not close the loop.
6. **Viewport and acceleration.** Follow the GUI benchmark plan: bounded scene
   preparation, LOD, virtualized inspection and batched geometry first. Measure
   real Linux and macOS behavior before selecting the final shell. UI graphics
   and EDA compute have separate backends and verification obligations.

Each gate requires deterministic inputs, version/hash evidence, measured
end-to-end wall time and peak RSS, and a negative control for its independent
checker. No graph/path/cut/SAT search may be added without an explicit budget.
