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
tie-cell insertion and explicit supply networks require a physical profile;
nonzero macro origins fail with diagnostics rather than guessed semantics.

Core connectivity has separate generational `InstId`, `CellId`, `NetId`, `PinId`
arenas and a derived CSR view. It expands signal bits with hard caps and unifies
wire aliases with bounded union/find. Driver and endpoint coverage checks do not
enumerate graph paths. Imported LEF supply geometry is retained; no supply wiring
or I/O physical placement is inferred without an explicit physical profile.

Version 0.3 adds bounded I/O, rail and supply-grid kernels. Profile-driven import
materializes literal constants into PDK tie cells; linked sink counts enforce
the tie fanout budget. Placement inserts physical-only fillers, validates LEF
symmetry for N/FS, and checks primary rails across each full row and shared row
boundaries. Horizontal rails join two independent vertical supply spines through
pinned fixed vias. I/O and supply geometry use separate routing layers.

Version 0.4 adds a separate bounded tap reservation/packing kernel. It reserves
both row ends and intermediate site-aligned tap slots before packing logical
cells and filling remaining gaps. The workspace validates the selected WELLTAP
master and supply/LEF geometry, but deliberately does not infer GDS contacts from
LEF labels. An independent checker reconstructs placed original-PDK GDS polygons
and verifies contact-to-metal paths plus active-region distance within each
well/substrate component. The profile's engineering distance policy is distinct
from full foundry latch-up/DRC/LVS acceptance.

The `clock-tree` mutation builds a bounded level-balanced buffer topology from
one configured scalar input to direct configured sink pins, then re-places the
logical cells and regenerates taps, fillers and primary supply geometry. A
bounded breadth-first native check enforces sink coverage, uniform depth,
fanout and no repeated nets. Independent OpenDB traces follow unique drivers
backwards; Liberty SAT, whole-design CEC, simulation and ideal-wire timing provide
separate semantic checks. This stage does not optimize skew or produce signal
wire geometry. Version 0.5 adds strict primary-clock SDC binding and a separate
bounded routing pass. Local pin-access searches reserve escape wires and fixed
vias; multi-terminal A* uses an explicit lattice, heap, expansion and comparison
budget. Wire/via records are separate from the netlist and stored in schema 5
`routing.rbin`. Placement invalidates routes; every reload checks geometry,
endpoint connectivity and table identity. Cell-spacing policy is explicit.

The external integration harness now feeds routed DEF to pinned OpenRCX with
original PDK extraction rules, then imports detailed SPEF into pinned postroute
RustSTA candidate tools and compares propagated-clock timing with OpenSTA.
This is an independently checked functional chain on a four-register pipeline.
The dense six-register case still exceeds this router's demonstrated coverage.
The unchanged 1 ps parity gate is not met (about 3.15/6.79 ps discrepancies), and
hold closure is incomplete. The harness returns partial, never signoff success.
See `cts-routing.md`; a native calibrated RC extractor remains future work.

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

Prepared physical profiles use version 3 and a further `physical.rbin` table:
fixed 64-byte records mirror I/O rectangles, supply port rectangles, wire segments
and via placements. Every read reruns bounded layout checks and compares the
table to the regenerated state. Repeated placement first removes generated
fillers and rebuilds the same deterministic result; logical cells and inserted
ties retain their ordering. Default Verilog export removes physical-only fillers,
while `--include-power` exposes the full PG-connected view for physical checking.

Tap/clock profiles use schema 4; schemas 1–3 remain readable. Schema 4 adds tap
position records to `physical.rbin`, plus bounded tap and clock reports in the
metadata. Taps are omitted from the logical Verilog view and included in the PG
view. Inserted clock buffers remain in both. Re-placement removes generated taps
and fillers, preserves the clock topology and original logical order, and
rebuilds the same geometry. Older profiles without either feature still publish
schema 3 snapshots.

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

The row-packing kernel remains linear, but complete snapshot legality and rail
checks sort bounded intervals and take O(N log N) in the general case. Filler
instances can follow logical instances in storage; geometry validation is
independent of that storage order.

## Next gates, in dependency order

1. **Broader technology and signoff requirements.** The LEF/mapped Verilog,
   boundary I/O, primary supply/tie network and N/FS rail-continuity paths are
   independently verified on the two baseline PDKs. Remaining scope includes DEF
   import, broader well/guard-ring policies, multi-height cells, richer orientations, complete
   design-rule interpretation, IR/EM and further PDK variants.
2. **Legalization and global placement.** Keep fixed-order packing as a baseline.
   Define objective/QoR, displacement and density constraints; estimate memory
   for pin CSR, gradient and density buffers first. Deterministic `cpu_ref` and
   independent placement checks precede CUDA or Metal/Accelerate acceleration.
3. **Clocking and routing.** The current clock topology has explicit port/cell
   roles and passes connectivity/formal checks; SDC-derived clock constraints,
   propagated-clock timing, skew-aware buffering and routing remain. Routing owns integer geometry, a bounded frontier,
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
