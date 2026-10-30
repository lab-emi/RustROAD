# Native RC extraction

`rustroad extract` turns the routed HEAD revision of a workspace into detailed
IEEE 1481 SPEF. It is a read-only command. It reads the stored routing records
(`routing.rbin`), the placed pin/obstruction geometry and the supply metal, and
a per-PDK, per-corner RC profile built from PDK data files. Two independent
black-box oracles check it: OpenRCX (`scripts/compare_spef.py`) and Magic
(`scripts/magic_rc.py`); `scripts/rc_oracles.py` runs all three together. A
finite-difference field solver on the PDK's process stack
(`scripts/field_solver.py`) measures what no 2.5-D extractor models: the
three-dimensional loading of wire crossings. No reference source was read,
and no native coefficient was fitted to either oracle's output.

Scope is block level. Supply nets are not extracted. Pin capacitance stays in
Liberty. The routed metal is extracted completely: wire bodies, the half-width
end extensions, the via landing pads and routing patches. A crossing is counted
from both conductors' cross-sections, including pins, obstructions and supply
metal that a routed wire crosses. Metal on another layer that overlaps a wire
face without forming a calibration plate (stacked, offset or barely wider
metal) couples over the overlapped area. Via cut capacitance, diagonal coupling
and the fringe of such non-plate overlaps are not modeled (see "Limitations").
SI, IR/EM, OCV and CPPR remain `unchecked`. DRC/LVS status is `unchecked`.

## Command

```sh
rustroad extract --workspace W --rc-profile fixtures/pdk/sky130-rc-max.json \
  --corner max --out /path/outside/W/routed_max.spef \
  [--max-spef-mib 32] [--max-segment-um 25] [--max-rc-nodes 500000] \
  [--max-coupling-caps 1000000] [--max-extract-work 200000000] \
  [--max-rc-elements 1000000] [--max-extract-candidates 4096] \
  [--extract-memory-mib 2048] [--max-extract-shapes 2000000] \
  [--max-extract-bin-entries 8000000]
```

* The workspace is opened under its shared lock and never written. `--out`
  inside the workspace fails with `OUTPUT_IN_WORKSPACE`. The SPEF is written to a
  temporary file in the target directory, fsynced, then renamed. A failure
  removes the temporary file and leaves no output.
* `--corner` must equal the profile's corner (`CORNER_MISMATCH` otherwise). The
  profile's `technology_lef_sha256` must equal the workspace's imported
  technology LEF. Every routing and cut layer the router used must be in the
  profile. A routed width different from the calibrated width is an explicit
  error (non-default widths are unsupported).
* The byte cap (`--max-spef-mib`, at most 16,384) is enforced while writing.
  Shape, node, element, coupling, candidate, bin and work budgets fail with
  `BUDGET_EXCEEDED`. Errors use the existing `{status, code, message}` stderr
  envelope and exit code 2.
* stdout is a JSON report (`rustroad.extract.v1`): output path, sha256 and
  bytes; profile identity; `scope` (corner, `wire_model: rc_tree_per_net`,
  `coupling: cross_net_spef` plus the reason, and how each structure is
  treated); `stats`; unannotated signal nets; and diagnostics. Nets with no
  pin (`pinless_signal_nets`) and nets with exactly one pin
  (`single_pin_signal_nets`: an unused input port or an unloaded output pin;
  the router routes nets of two or more terminals only) have nothing to
  extract and are listed separately, never as unannotated.
* Candidate shapes per wire run (`--max-extract-candidates`, default 4,096)
  were a fixed cap; they are a budget now. Per run the extractor holds the
  candidate list, interval events, ordered active set and plane list, with
  vector growth and hash/tree occupancy included: 256 B per candidate
  (`CANDIDATE_SCRATCH_BYTES`), one run at a time. The ceiling is 48 MiB of
  that scratch, 196,608 candidates (`MAX_CANDIDATES_CEILING`). An interval sweep
  skips disjoint candidates while keeping shape-ID accumulation order. Index
  work and active-candidate comparisons debit `--max-extract-work`. The report states `max_candidates_per_run`,
  `candidate_scratch_bytes` and the observed `stats.max_run_candidates`.
  `--max-rc-elements` (resistors + nodes + 2 x couplings, explicitly bounded by the
  whole-process memory allowance) is a command-line budget too. The track
  recipe of the gate passes 65,536 candidates (16 MiB scratch with the
  interval sweep) and 8 M elements.

Cell pin and obstruction geometry is shared per master and decoded only for
local queries. The default virtual-shape budget is 256 million and the separate
instance-index budget is 64 million entries; they can be set with
`--max-virtual-shapes` and `--max-instance-bin-entries`. Crossing conductors use
one marking bit per virtual shape and generate their contribution strips as
needed. They do not create a flat design-sized rectangle/strip array.

Before RC-tree construction, the extractor checks the loaded process footprint,
with 25% headroom, plus graph, retained-shape, coupling, spatial-index and scratch
allowances against `--extract-memory-mib`. Runtime peak RSS is checked during
extraction. Larger object limits still fail when their combined estimate exceeds
the memory allowance. `stats.memory` reports the estimate and cap. The CLI reads
the workspace once, converting the segment length under the same snapshot lock.
Owned-conductor bins use a shared insertion-ordered entry array and fixed
headers. The estimate accounts for entry-vector reallocation and bounds header
count by each routing layer's actual conductor extent. Virtual cell geometry
keeps its separate index. Candidate order and per-bin work counts are preserved.

SPEF formatting retains the numeric RC network once and generates labeled nets one
at a time. A compact coupling incidence table preserves record order; the name
map borrows instance names instead of copying them for every pin. The existing
byte and graph budgets still apply.

Repeated runs are byte-identical. The SPEF `*DATE` is a fixed string, and the
JSON report carries provenance.

Candidate queries use the lateral calibration window on the wire's own layer
and its exact footprint on every other modelled layer. All accepted shape IDs
and interval boundaries are retained, including shapes with no direct final
capacitance contribution; removing such boundaries can change f64 rounding.
Candidates are still processed in shape-ID order. The query change adds no
per-run storage or worker concurrency: the run16 replay estimate is 3.46 GiB,
including 16 MiB scratch (65,536 candidates at 256 bytes), one corner and one
extraction worker. Builds and tests use two workers under a 4 GiB OS cap.

After metal construction and crossing discovery, an immutable BVH indexes only
the retained shape IDs. The mutable conductor bins are released first; virtual
cell geometry stays separate. Admission takes the maximum of the old bin and
new BVH allowances, including a bounded construction/query stack. For four
million retained shapes and 15 layers the BVH allowance is about 112 MiB,
below the existing 192 MiB bin-entry allowance. Build/query probes consume the
original work budget; candidate and electrical accumulation order stay fixed.

## RC profile (`rustroad.rc-profile.v1`)

`scripts/build_rc_profile.py` writes `fixtures/pdk/{sky130,gf180}-rc-{min,nom,max}.json`.
`--check` fails if a tracked profile differs from a rebuild;
`--compare-ref e0d074d` fails unless every field that existed at that revision
is unchanged, apart from the intentional changes it lists with their PDK
evidence (round 3: the four GF180 max cut resistances, see "Data
decisions"); diagnostics may only gain entries. It reads PDK data files and
records each one's path, sha256 and size in `sources`:

| role | Sky130 | GF180 |
|---|---|---|
| capacitance calibration table | `libs.tech/librelane/rules.openrcx.sky130A.<c>.calibre` | `libs.tech/librelane/rules.openrcx.gf180mcuD.<c>` |
| nominal calibration table (corner ratio of the overlap data; min/max profiles) | `...sky130A.nom.calibre` | `...gf180mcuD.nom` |
| resistance, overlap data and cross-check | `libs.tech/magic/sky130A.tech` (extract section) | `libs.tech/magic/gf180mcuD.tech` |
| layer order and widths | `sky130_fd_sc_hd__nom.tlef` | `gf180mcu_fd_sc_mcu7t5v0__nom.tlef` |

Corners map as follows:

| profile | calibration table | Magic variant | used with Liberty |
|---|---|---|---|
| `min` | `*.min` | `(lrlc)`: low R, low C | ff (hold) |
| `nom` | `*.nom` | `()`: nominal | tt |
| `max` | `*.max` | `(hrhc)`: high R, high C | ss (setup and hold) |

Schema:

* `layers[]`: `name`, `rules_metal` (the 1-based calibration metal index,
  checked against the LEF routing-layer order and `LayerCount`), `width_um`
  (checked against LEF `WIDTH`), `sheet_resistance_ohm_per_square` (from the
  Magic `resist` line of the corner variant), `contexts[]`,
  `area_cap_pf_per_um2` and `overlap_area[]`. Each context is
  `{over, under, rows}`: `over` is the calibration index of the nearest
  conductor below (0 = substrate), and `under` is the index of the nearest
  conductor above or null. Each row is
  `[edge_spacing_um, coupling_pf_per_um, ground_pf_per_um_per_side]`. Each
  `overlap_area` entry is `{over, pf_per_um2, magic_nominal_af_per_um2,
  table_corner_ratio, table_corner_ratio_source}`.
* `cuts[]`: `name`, `lower`, `upper`, `resistance_ohm_per_cut` and `source`
  (the Magic `contact` value). The cut-to-contact mapping is checked against
  the Magic `contact`/`types` sections.
* Also recorded: `corner`, `pdk`, `library`, `pdk_version`,
  `technology_lef_sha256`, `units` (um/pF/ohm), `magic_variant`,
  `lef_parasitics`, `model` (text), `cross_checks`, `diagnostics` and
  `generator`.

A profile without `area_cap_pf_per_um2` (from before round 2) still loads;
the overlap-area term is then off and the report says so
(`OVERLAP_AREA_UNAVAILABLE`).

Data decisions:

* **Table rows are per side.** The isolated row read as "per side" (isolated
  total = 2 x last-row ground) agrees with the Magic area + perimeter model
  within -11 %..+28 % on all layers, corners and both PDKs. Read as "total",
  the disagreement would be -36 %..-56 %. Coupling at minimum spacing agrees
  with the Magic sidewall model within 8 % on every Sky130 layer at nominal.
  GF180 differs more (-40 %..+54 %); that is a disagreement between two PDK
  data sources and is recorded, not resolved (`cross_checks`).
* **Resistance is the foundry sheet resistance.** The fourth column of each
  calibration row is not used (its ratio to the sheet resistance differs per
  layer, so its unit cannot be established from data alone). Sky130 LEF
  `RPERSQ` agrees with the Magic nominal values and is recorded as a
  cross-check.
* **Non-plate overlap area comes from the Magic overlap rules.** The
  calibration tables only describe a wire over or under an infinite plate; they
  have no data for metal that covers part of a wire face. For that case the
  profile carries the PDK's own area capacitances: Magic nominal
  `defaultareacap` (face to substrate) and `defaultoverlap` (face over a lower
  routing layer). They are moved to each corner with the calibration table's
  own ratio, so every capacitance term follows the table's corners: the
  substrate row's isolated value (min/nom, max/nom) for the area capacitance,
  and the plate excess of the same layer pair (OVER-k minus OVER-0, isolated)
  for the overlap; where a distant plate's excess is not positive (Sky130 met4
  over li1) the substrate ratio is used and named in
  `table_corner_ratio_source`. Magic's own corner variants are not used for
  this term: Sky130 `(hrhc)` puts met2 over met1 at 313 aF/um^2 against
  133.9 at nominal (2.3x), the table's plate excess moves 1.57x.
* GF180's technology LEF states that its parasitic values are not correct. They
  are never read (`lef_parasitics: placeholder_excluded`).
* The GF180 Magic `(lrlc)` block lists every contact resistance as 0, which
  means no data. The min profile therefore uses the nominal 4.5 ohm and states
  this in `diagnostics` (`VIA_R_CORNER_UNAVAILABLE`).
* **The GF180 Magic `(hrhc)` contact block is a placeholder too (round 3).** It
  gives every one of its 20 contact names (diffusion, poly, via and MIM
  contacts) one value, 15 ohm, while the nominal block distinguishes them
  (4.5 to 8 ohm); the diffusion resistances of the same block are a blanket
  15 ohm/sq as well. A block that gives every contact type one value although
  the nominal block distinguishes them is not corner data, so the max profile
  also keeps the nominal 4.5 ohm per cut (equal to the technology LEF) and
  says so (`VIA_R_CORNER_UNAVAILABLE`). Sky130's corner blocks distinguish
  their contacts (and agree in direction with the Sky130 SPICE corner models)
  and are kept. The previous choice (15 ohm) remains available as the
  `round2_data` ablation variant; `rc_data_sources.py` tabulates every
  variant's block.
* `DIAGUNDER` tables and the `RESOVER` tables are not used.
* Which data source each quantity follows, and how far the PDK's sources
  disagree, is tabulated in "Data sources" (`docs/evidence/rc-data-sources.json`).

## Physical model

Each routed net is built from its stored records: wires, fixed vias and
pin-access points.

1. **Graph.** Nodes are `(layer, x, y)` in integer DBU: record ends, via
   landings and access points. A wire record is split at every node of its net
   and layer that lies inside it, and repeated metal (collinear overlapping
   records) counts once (`duplicate_wire_edges`). So lattice edges and compact
   segments of any length give the same graph. A node is a junction unless it
   is the interior point of a straight run on one layer: degree 2, collinear,
   no via and no pin. Each maximal straight run between junctions is one
   resistor, unless it is longer than `--max-segment-um`: then it is cut into
   `ceil(L / limit)` equal pieces at exact coordinates, and a cut point that is
   not a stored node becomes an internal node (`segment_cut_nodes`). The cut
   points depend only on the run, not on how its metal was stored (unit test
   `long_runs_split_at_the_same_coordinates_however_stored`). Coincident pins
   get a 0-ohm link; the timing engine contracts zero-ohm edges.
2. **Tree.** A breadth-first walk starts at the single driver, an output pin or
   an input port. Neighbours are visited in node-key order. A conductor that
   closes a loop keeps its capacitance but not its resistance, and is counted
   in `opened_loop_resistors` with an `RC_LOOP_OPENED` diagnostic; the timing
   engine rejects meshes. Disconnected nodes are an error.
3. **Resistance.** A run's resistance is `sheet * centerline_length / width`.
   A via's resistance is the per-cut value divided by the number of cuts. The
   centreline already runs to the via centre, so pads and end extensions add
   no resistance.
4. **Metal.** The routed metal of a net is tiled without overlap: the **body**
   of each run (its centreline extent times the width), then **pieces**: the
   half-width end extensions, every via landing pad (the LEF via geometry on
   routing layers) and every routing patch, each minus the same-net metal
   already present on its layer, cut into disjoint rectangles by exact integer
   subtraction (at most 256 per rectangle). The union of bodies and pieces is
   exactly the routed metal. Pieces add capacitance only. Each piece has two
   side strips, one along x and one along y, so all four of its edges take the
   per-side model below.
5. **Capacitance.** Every strip (a body or a side strip of a piece) is cut into
   intervals at the extents of nearby shapes: other bodies and pieces, instance
   pins, port pins, obstructions and supply metal on routing layers. For each
   interval and each of its two sides:
   * **Exposure.** A side touching or inside same-net metal (an inner corner,
     the net's own pin) carries no capacitance (`covered_side_intervals`).
   * **Lateral.** The nearest same-layer shape within the table range gives
     the row at that edge spacing (linear interpolation, clamped to the first
     row): coupling `cc` and ground `cg` per side. With no neighbour, the side
     takes the isolated last row. A same-net neighbour shields the side.
   * **Plates.** A conductor on another layer is the calibration plate below
     (above) where the metal of its layer covers the strip and reaches at least
     one strip width beyond both side edges: crossings and clearly wider
     shapes. The nearest such layer below and above selects the context
     `(over, under)`; the conductor under each side edge owns that side's plate
     share. A missing exact context falls back to `(over, -)`, then
     `(0, under)`, then `(0, -)`, and is counted.
   * **Split.** A plate below shields the substrate: the whole context value
     `cg` of the side goes to it. A plate above takes the excess over what goes
     down (the substrate share `cg_sub`, the OVER-0 row at the same spacing, or
     the OVER-only row when there is a plate below too). With no plate, `cg` is
     ground.
   * **Crossings count from both conductors.** The upper conductor's OVER cut
     covers the lower one's width; the lower conductor's UNDER cut covers the
     upper one's width and adds its excess as coupling to the upper one. This
     holds whoever owns the lower or upper metal: a pin, an obstruction or
     supply metal that a routed conductor on another layer overlaps gets two
     strips like a piece (`passive_strips`, `passive_intervals`) and counts its
     own cut toward the routed conductor that is its plate (to that net's
     ground, or as coupling for another net's pin). Its own ground and lateral
     sides are counted from the routed side, as before. The overlap area of a
     crossing is in both cuts, which overstates a crossing by about
     `area_cap * w_upper * w_lower` (2.6 aF for Sky130 met2/met1, against
     24-45 aF per crossing).
     This two-cut construction is a 2.5-D model: per the 3-D field solve
     ("Crossings in three dimensions") it is accurate for dense crossings,
     under-counts sparse crossings and over-counts crossings under a wire
     with close same-layer neighbours. That 3-D loading is not modeled.
   * **Non-plate overlap area.** Where the nearest metal of another layer covers
     part of a strip's face without being a plate (stacked, offset or barely
     wider metal, partial pad overlaps), the upper conductor's face couples to
     it over the covered width at the profile's `overlap_area` value (counted
     once, from the upper face; a passive conductor above a routed one is
     counted from the routed one), and the covered face loses its
     `area_cap_pf_per_um2` to the substrate (floored at zero per interval).
     Nearer layers shield farther ones across the face. The fringe of such
     overlaps is not modeled. `overlap_area_um2` and `nonspanning_plane_intervals`
     count it.
   * **Ownership of lateral pairs.** Every routed-net shape models its own
     exposed sides, so a lateral pair is counted half from each side. Pins,
     obstructions and supply metal are counted fully from the routed side. A
     partner with no signal net (supply, obstruction, unconnected pin) is
     ground. A partner pin is attached at that pin's tree node, a piece at its
     node, and a body at its end nearest the interval. On the strip's own side,
     each value is split between its two end nodes by position (pi model).
6. **Routing patches.** `Routes.patches` (DEF routing `RECT`) are pieces of step
   4 attached to the RC node of the same-net conductor they overlap or touch:
   largest overlap area first, then the lowest shape index. A floating patch, a
   patch on a net without an RC tree and a patch off a routing layer are
   explicit errors. Patches do not conduct in the native tree (see
   "Limitations").
7. **Coupling output.** Cross-net capacitors are written as SPEF coupling caps,
   listed in both nets' `*CAP` sections. So each net's declared total equals
   its ground plus coupling. RustSTA grounds coupling at factor 1, so for timing
   this equals a grounded model.

JSON `stats` added in round 2: `duplicate_wire_edges`, `segment_cut_nodes`,
`passive_strips`, `passive_intervals`, `overlap_area_um2`,
`overlap_pairs_without_data`. `scope` adds `passive_conductors`; `parallel_overlap`
and `cell_internal_metal` describe the overlap-area term.

## SPEF conventions

* Header: `*SPEF "IEEE 1481-1999"`, units `NS`, `PF`, `OHM`, `HENRY`, divider
  `/`, delimiter `:`, bus delimiter `[]`, `*DESIGN_FLOW "NAME_SCOPE LOCAL" "PIN_CAP NONE"`.
* `*NAME_MAP`: routed nets `*1..*N` in connectivity net order. Then come the
  instances, in order of first use.
* Nodes: internal junctions are `*<net>:<k>`, with k = 1.. in tree order from
  the driver; instance pins are `*<inst>:<PIN>`; ports are bare bit names
  (`a[0]`). Characters other than `[A-Za-z0-9_]` and the bus brackets are
  backslash-escaped.
* `*PORTS` lists routed signal ports. `*CONN` has `*P` for ports (`I`/`O`/`B`)
  and `*I` for pins with the LEF pin direction and `*D <master>`.
* `*CAP` lists ground caps for every node (zero allowed), then couplings. Each
  coupling names the net's own node first. `*RES` lists tree edges.
* Values have 6 significant digits. The `*D_NET` total is the sum of the
  rounded values listed, so RustSTA's total check (1e-4 relative) is exact.

## Resources (Rule 9)

Estimate made before implementation, at the default caps with one corner and
one thread (the extractor is serial and deterministic):

* 2 M shapes x 56 B = 112 MiB (bodies, pieces and passive shapes share this
  cap; a run adds at most two end pieces and a via at most one pad per routing
  layer before subtraction);
* passive strips: at most 2 per passive shape, bounded by 2 x the shape cap,
  64 B each = 256 MiB worst case;
* 8 M bin entries x 4 B, plus the map, < 128 MiB;
* 500 k nodes (virtual cut nodes included) with keys and adjacency, about
  64 MiB;
* 1 M couplings x about 64 B (BTreeMap) = 64 MiB;
* SPEF model strings < 64 MiB; the file is streamed through a 256 KiB buffer;
* the workspace read itself is under 16 MiB of `design.json`, about 200 MiB
  parsed.

That is under 1 GiB peak, under the 2 GiB per-test-binary limit. There are no
paths, cuts or cones. Each net is one linear walk; the neighbour search is
bounded by the window, `max_candidates` and `max_work`; the passive-strip
search and the face coverage are charged to `max_work`.

The estimate stays under 4 GiB, so no scale lock is needed. Builds and tests
use `CARGO_BUILD_JOBS=4`, `RUST_TEST_THREADS=4` and
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G`.

## Verification

* **Unit tests** (`rustroad-core::extract`, values derived by hand from the
  test rows):
  * an analytic straight wire (10 ohm; ground 2 x 4e-5 x 10 um plus the two end
    extensions);
  * a parallel pair (coupling counted once, per-side ground);
  * the exact union perimeter of an L corner and of a via with pads;
  * sides inside the net's own pin are not exposed;
  * a crossing counted from both cross-sections;
  * a crossed grounded rail counts its own cut (2 x 3e-5 x 0.2 um), and the
    same rail as a routed conductor gives the crossing wire the same total;
  * a non-plate overlap couples over the covered face once (3e-4 pF/um^2 x
    0.1 um x 10.1 um) and removes 1e-4 pF/um^2 of the face's substrate
    capacitance, leaving the lower wire unchanged;
  * narrower parallel metal below is not a plate;
  * compact segments extract exactly like lattice edges, and a run longer than
    the segment limit is cut at the same exact coordinates whether it is
    stored as lattice edges or as one record through a T junction plus an
    overlapping duplicate (resistors 2/2/3/3/3 ohm; two virtual cut nodes; two
    duplicate edges);
  * patches, floating/unrouted/cut-layer patches, branch/via/pin trees, loop
    opening, budgets, profile-width mismatch.
* **Integration tests** (`crates/rustroad-workspace/tests/extraction.rs`):
  byte-identical rerun; unchanged workspace tree; one D_NET and one tree per
  net; refusals that leave no output and no temporary file.
* **Python tests** (`scripts/tests/test_rc_oracles.py`): GDS records; Magic
  `.ext` parsing; instance mapping; back-annotation; the target bounds; legacy
  profiles drop only the new fields; LEF OBS stripping; signal-pin shrinking
  (supply pins and OBS untouched); report rows carry the bound their verdict
  uses (fidelity 1 ps); blanket corner contact blocks are detected and the
  tracked GF180 max profile keeps the nominal via resistance; the field
  solver's self-test and stack parsing; design totals and the
  between/below/above classification (69 tests).
* **Campaign self-checks and controls** (`scripts/rc_oracles.py`): native
  byte-identical rerun; read-only workspace; refusals; independent resistance
  accounting from the route records (collinear records merged; nets whose
  record graph has a loop are left out and listed); Magic determinism;
  back-annotation fidelity (OpenRCX's per-net totals on the native trees must
  time within 1 ps of OpenRCX's own SPEF); OpenRCX design total unchanged at
  `-coupling_threshold 0`; OpenRCX per-net C unchanged without the cell LEF
  OBS; no net loses more than 0.1 fF of OpenRCX capacitance with every signal
  pin of the cell LEF shrunk to a 10 nm square; negative controls (x3
  resistance, dropped D_NET, renamed net, dropped `*I`, planted open in the
  GDS).
* **Canonical structures** (`scripts/rc_structures.py`,
  `docs/evidence/rc-structures.json`): the native column is the extractor itself
  (`crates/rustroad-core/examples/extract_structures.rs` calls
  `extract_cpu_ref` with the tracked profile), not a formula. Round 3 adds a
  monotonicity check (adding conductors must not lower a wire's total) and an
  OpenRCX via probe.
* **Field solver** (`scripts/field_solver.py selftest`, also in the Python
  tests): parallel plates exact to 1e-9, symmetric capacitance matrix, monotone
  totals, the 2-D grid and the quarter-domain 3-D grid agree within 1 % on a
  long wire; grid and domain convergence of the crossing result is recorded in
  the evidence.

## Oracles

**OpenRCX** (pinned `openroad/orfs` image `f152917e`) reads the DEF the flow
writes (maximal collinear runs, geometry-identical routing to the native GDS),
the PDK technology and cell LEF and the calibration table of the corner, with
`-max_res 0 -no_merge_via_res` and either the flow's `-coupling_threshold 0.1`
or `-coupling_threshold 0`. The campaign checks per design that its nominal
SPEF at 0.1 equals the flow's own OpenRCX SPEF net for net
(`openrcx.flow_reference_nominal`). It reads the same
calibration tables as the native profile; its resistance comes from the
tables' `RESOVER` rows and the LEF via resistances (see "Data sources").

Black-box behaviour (inputs varied, outputs compared; no source read). Each
item is a campaign control or a canonical structure, re-run every campaign:

* **Cell pin metal is not a partner of any net.** The same DEF with every
  signal pin of the cell LEF shrunk to a 10 nm square lowers no net's C
  beyond the control threshold; it can raise C and change R, because OpenRCX
  merges a net's own wire metal over its own pin into the pin while the pin
  is full size. Native and Magic couple routed wires to neighbouring pin
  metal.
* **Cell obstructions are not extracted.** A cell LEF stripped of every OBS
  block gives identical per-net C.
* **Via landing metal is not extracted.** On a single wire, a via (or a
  two-via stack) adds exactly what splitting the wire path at that point
  adds (the extra end extension); the pads themselves add nothing. Native and
  Magic extract the pads.
* **At `-coupling_threshold 0.1` a small coupling is grounded in one net and
  dropped from the other.** The design total (`sum(ground) + sum(coupling)/2`)
  is identical to the threshold-0 run at every corner while per-net totals
  drop. The native SPEF writes every coupling in both nets, so the
  like-for-like OpenRCX configuration for per-net C and timing is threshold 0
  ("lossless"). Both are reported.
* **Capacitance monotonicity can be violated.** In one canonical GF180
  structure (a Metal3 wire with minimum-space neighbours on both sides, over
  Metal2 crossings at 2 um pitch) OpenRCX lowers the wire's total when the
  crossings are added. With every other conductor at 0 V a conductor's
  capacitance is an energy minimum over a set that adding conductors only
  shrinks, so it cannot decrease; native and Magic increase.
* **A crossing is two grounded increments.** Crossing couplings never appear
  in its SPEF; each wire gets its own ground increment, which does not grow
  with the lower conductor's width until that conductor is a wider plate.
* **Stacked parallel wires are full plates.** Magic and native use the PDK's
  overlap data for an aligned pair instead.
* OpenRCX can write two parallel resistors at a pin access; RustSTA rejects
  meshes, so those resistors are opened (counted in
  `openrcx.<corner>.loops_opened_for_sta`) before timing.

So OpenRCX is geometry-identical to the native extractor for the routed wires
and the supply metal, but not for the cell pins, cell obstructions and via
pads, which the native extractor and Magic both include.

**Magic** (pinned signoff image `4c48415d8bd0`, Magic 8.3.678) is independent
in both data and algorithm for capacitance (its own area, perimeter, sidewall,
overlap and sideoverlap rules per corner variant) and sees the real cell
layouts. The harness (`scripts/magic_rc.py`):

* exports the native GDS, DEF, routes and connectivity of the workspace
  (read-only commands);
* capacitance: reads the GDS with the GDS via structures flattened on input
  (`gds flatglob VIA_*`), extracts the hierarchy once (instance transforms and
  a standalone extraction of every cell master) and then the flattened design.
  A net's routing capacitance is its flat node capacitance (substrate plus
  every coupling record) minus the standalone capacitance of its cell pin
  nodes, which Liberty already holds. Values are read from the raw `.ext`
  records with exactly rounded sums;
* resistance: extracts a routing-only terminal layout (cell instances removed,
  one port cell at every routed pin access point) with `extract do resistance`
  and zero `extresist` thresholds; terminals are matched by position;
* maps every Magic node to a native net through the instance placement and the
  pin names, and refuses any open, short, merged or unmapped net;
* corner styles follow the profile's `magic_variant`.

Magic has no SPEF writer, so timing against Magic back-annotates its per-net
totals onto the native RC trees (every node capacitance and resistor of a net
scaled by one factor; couplings grounded at their own node). The fidelity
control does the same with OpenRCX's totals and compares with OpenRCX's own
SPEF (bound 1 ps).

## Oracle campaign

`scripts/rc_oracles.py campaign` extracts every routed workspace listed in
`CASES.json` natively at min/nom/max, runs OpenRCX at the flow and lossless
coupling thresholds and Magic at the profile's corner variants, times every
SPEF source in RustSTA (Magic by back-annotation) and writes the comparison:

```sh
python3 scripts/rc_oracles.py campaign --workspace RUN --base-rev empty --inputs CASES.json \
  --rustroad target/release/rustroad --sta-bin STA_DIR \
  --variant e0d074d=BASE_BIN:legacy --variant round1=ROUND1_BIN:legacy \
  --variant round2_data=target/release/rustroad:profiles=PROFILES_AT_66411bd
python3 scripts/rc_oracles.py evidence --summary RUN/summary.json --out docs/evidence/extraction-oracles-<design>.json
```

The targets are fixed: total C +/-10 %, per-net C +/-20 % for nets >= 5 fF,
per-net R +/-20 % for nets >= 20 ohm and an endpoint slack delta <= 10 ps
(the larger of the setup and hold delta), each evaluated against each oracle.
The back-annotation fidelity control must stay within 1 ps; when it fails the
campaign status is `failed`, and Magic timing deltas are bounded only to the
fidelity value. `--variant NAME=BIN[:legacy|:profiles=DIR]` adds ablation
binaries or profile sets to the same campaign. A missed comparison is
attributed net by net (one net at a time set to the oracle's per-net totals on
the native trees), capped by `--attribution-max-nets`.

Neither oracle is geometry-identical to the native extractor: OpenRCX
extracts the same routed wires and supply metal but none of the cell pins,
cell obstructions and via pads (controls above), and Magic extracts the real
cell layouts with its own data. A target missed against one oracle is
therefore reported with the net attribution and the oracle-to-oracle
difference, never closed by fitting a coefficient to either oracle.

Campaign receipts of earlier rounds (`docs/evidence/extraction-oracles-*.json`,
`extraction-comparison.json`, `extraction-requalification.json`) are
historical evidence kept for reproducibility, not current benchmarks.

## Crossings in three dimensions

The calibration tables are two-dimensional; every extractor here builds a
crossing from 2-D cross-sections (native: both wires' cuts; OpenRCX and Magic:
their own 2.5-D rules). `scripts/field_solver.py` solves the 3-D Laplace
problem of an upper wire crossed by perpendicular lower wires at a given pitch,
optionally with same-layer neighbours of the upper wire, on the PDK's own
process stack (Sky130 KLayout cross-section, GF180 KLayout 2.5-D file) in a
homogeneous dielectric, and compares the upper wire's increment per crossing
with the native two-cut construction built from the same solver's 2-D values
(`docs/evidence/rc-field-solver.json`, which also records grid and domain
convergence). In a homogeneous dielectric every capacitance scales with the
permittivity, so the ratio does not depend on it; the real stack's layered
dielectrics would shift it, which is why the ratio is reported and not applied.

* For dense crossings (minimum pitch, no neighbours) the two-cut construction
  agrees with the 3-D solve.
* Sparse crossings load the upper wire more than the two-cut model says: a
  grounded line just below a wire intercepts field that would reach the
  substrate farther below, over a length set by that height, not by the
  line's width.
* Close same-layer neighbours absorb that field: with minimum-space
  neighbours the 3-D increment is below the two-cut value.

So the narrow-crossing loading is an unmodeled native structure, and the
2.5-D oracles bound it only loosely. A 3-D crossing term is not adopted: it
needs the full 3-D environment (cell metal below, irregular pitches, finite
wires) that the idealised factors do not contain, and an oracle that models
it.

## Resistance against Magic

Magic uses the same Magic sheet and contact values as the native profile except
where the profile falls back to nominal values (GF180 min and max contacts), so
for resistance it checks geometry and topology. Its network starts at the
terminal square, so a terminal on a pin via adds the via's contact plus half a
square of the pin layer; that element is taken out on both sides before the
comparison. A routing patch that bridges two regions conducts in Magic but not
in the native tree (patches add capacitance only). At GF180 min and max,
Magic's contacts are its placeholder blocks while the native profile keeps the
nominal value, so that comparison reflects the data choice.

## Data sources

`scripts/rc_data_sources.py` (`docs/evidence/rc-data-sources.json`) tabulates
every interconnect value the PDK ships, per corner. Selected rows:

| PDK | quantity | corner | native | Magic | LEF | SPICE corner model | calibration table (reference) | other table variants |
|---|---|---|---|---|---|---|---|---|
| sky130 | met1/met2 sheet, ohm/sq | min | 0.105 | 0.105 | 0.125 | 0.111 | 0.131 (calibre) | 0.106 (magic), 0.105 (spef_extractor) |
| sky130 | met1/met2 sheet, ohm/sq | nom | 0.125 | 0.125 | 0.125 | 0.125 | 0.173 (calibre) | 0.126, 0.125 |
| sky130 | met1/met2 sheet, ohm/sq | max | 0.145 | 0.145 | 0.125 | 0.139 | 0.233 (calibre) | 0.147, 0.145 |
| sky130 | met3/met4 sheet, ohm/sq | nom | 0.047 | 0.047 | 0.047 | 0.047 | 0.051 | 0.047, 0.047 |
| sky130 | via (M1M2) ohm/cut | min / nom / max | 2.0 / 4.5 / 15 | 2.0 / 4.5 / 15 | 4.5 | 2.75 / 4.5 / 11.85 | - | - |
| sky130 | mcon ohm/cut | min / nom / max | 1.6 / 9.3 / 23 | 1.6 / 9.3 / 23 | 9.3 | 3.91 / 9.3 / 18.61 | - | - |
| gf180 | Metal1-4 sheet, ohm/sq | min / nom / max | 0.076 / 0.090 / 0.104 | 0.076 / 0.090 / 0.104 | 0.090 | 0.078 / 0.090 / 0.102 | 0.063 / 0.091 / 0.145 (plain) | 0.077 / 0.091 / 0.105 (magic) |
| gf180 | Via1-4 ohm/cut | min / nom / max | 4.5 / 4.5 / 4.5 | 0 / 4.5 / 15 (placeholder blocks) | 4.5 | - | - | - |

Magic contact blocks (`magic_contact_blocks` in the evidence): Sky130 gives 8
distinct values among 26 contact names in every variant; GF180 gives 4 at
nominal and one value for all 20 names in `(hrhc)`/`(hrlc)` (15 ohm) and
`(lrhc)`/`(lrlc)` (0).

The calibration table's resistance is its `RESOVER` row (ohm per unit length
at the table width); times the width it is the sheet value shown. The OpenRCX
reference uses that row, and the LEF via resistance at every corner (observed
in its SPEF: 9.3 / 4.5 / 3.41 ohm on Sky130, 4.5 on GF180).

**Which source the native profile follows, and why.** Resistance follows the
Magic extraction rules of the corner variant (`(lrlc)` / `()` / `(hrhc)`) where
they are corner data:

* at nominal they agree with the technology LEF, the SPICE interconnect model
  and two of the three shipped calibration variants (`magic`,
  `spef_extractor`). The Sky130 `calibre` variant, the one the OpenRCX flow
  reads, is the outlier: met1/met2 0.173 ohm/sq (+39 %) at nominal, 0.233
  (+61 % on Magic, +68 % on SPICE) at max. No other PDK source supports it, so
  the native profile does not adopt it;
* the Magic rules are a complete per-corner set, contacts included; the SPICE
  corner models are the other complete set and lie between Magic's extremes
  and the nominal value. GF180's SPICE sheet corners agree with Magic within
  2 %;
* GF180's Magic corner contact blocks are placeholders (one value for every
  contact type; zeros at the low corners), and no other GF180 source has via
  corner data, so GF180 vias keep the nominal 4.5 ohm at every corner.

Capacitance follows the calibration table (`calibre` for Sky130, the plain
table for GF180), the same table OpenRCX reads. The isolated-wire value of
that table against Magic's area + perimeter rules (aF/um at default width):

| PDK | corner | table / Magic - 1 over the routing layers |
|---|---|---|
| sky130 | min | +2.2 .. +25.0 % |
| sky130 | nom | -3.7 .. +14.5 % (Magic nominal equals the LEF CPERSQDIST/EDGECAPACITANCE) |
| sky130 | max | -11.1 .. +4.8 % |
| gf180 | min | +3.0 .. +26.7 % |
| gf180 | nom | +5.4 .. +27.3 % |
| gf180 | max | +4.2 .. +27.6 % |

The overlap-area term (non-plate overlaps) uses the Magic nominal overlap and
area capacitance, moved to the corner by the table. For a fully covered face it
adds 25-45 % of the table's full-plate excess for the same pair (area only, no
fringe), nominal values:

| PDK | upper over lower | Magic overlap aF/um^2 | Magic area cap aF/um^2 | covered-face increment aF/um (default width) | table full-plate excess aF/um | corner ratio min / max |
|---|---|---|---|---|---|---|
| sky130 | met1 over li1 | 114.2 | 25.78 | 12.4 | 25.4 | 0.73 / 1.36 |
| sky130 | met2 over met1 | 133.9 | 17.50 | 16.3 | 37.9 | 0.71 / 1.57 |
| sky130 | met3 over met1 | 34.5 | 12.37 | 6.7 | 16.1 | 0.90 / 1.23 |
| sky130 | met3 over met2 | 86.2 | 12.37 | 22.1 | 47.7 | 0.82 / 1.32 |
| gf180 | Metal2 over Metal1 | 59.0 | 15.02 | 12.3 | 56.6 | 0.80 / 1.28 |
| gf180 | Metal3 over Metal1 | 20.2 | 10.09 | 2.8 | 23.8 | 0.86 / 1.17 |
| gf180 | Metal3 over Metal2 | 59.0 | 10.09 | 13.7 | 61.3 | 0.81 / 1.26 |

## Canonical structures

`scripts/rc_structures.py` (`docs/evidence/rc-structures.json`) puts the same
canonical geometry through both oracles and the native extractor itself, at
the nominal corner with 40 um wires at default width: isolated wires,
minimum-space pairs, single crossings, crossings of rails and planes, wires
along rails, wires over crossing arrays with and without minimum-space
neighbours, and stacked and offset wires. It also checks monotonicity (adding
conductors must not lower a wire's total) and probes how OpenRCX treats vias.
The native column is the extractor, not a formula.

## Reproduction

```sh
free -h
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G timeout 900s \
  cargo build --release --bins --examples
python3 scripts/build_rc_profile.py --check
python3 scripts/build_rc_profile.py --compare-ref e0d074d   # lists the intentional GF180 max cut change
python3 scripts/extraction_ablation.py --out .work/abl --build     # ablation binaries
python3 scripts/verify_routing.py --workspace .work/runs/flowA --base-rev empty \
  --rustroad target/release/rustroad --rusteda STA/rusteda --ruststa STA/ruststa --producer policy
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v \
  timeout 3000s python3 scripts/rc_oracles.py campaign --workspace .work/runs/r3/pre \
  --base-rev empty --inputs CASES.json --rustroad target/release/rustroad --sta-bin STA \
  [--variant NAME=BIN[:legacy|:profiles=DIR] ...] [--attribution-max-nets 30]
python3 scripts/rc_oracles.py report --summary .work/runs/r3/pre/summary.json
python3 scripts/rc_oracles.py evidence --summary .work/runs/r3/pre/summary.json --out X.json [--compact]
python3 scripts/rc_structures.py --out .work/runs/structures \
  --native-bin target/release/examples/extract_structures --evidence docs/evidence/rc-structures.json
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G \
  python3 scripts/field_solver.py crossings --out docs/evidence/rc-field-solver.json --markdown
python3 scripts/magic_rc.py controls --pdk sky130 --out .work/runs/magic_controls/sky130
python3 scripts/rc_data_sources.py --out docs/evidence/rc-data-sources.json --markdown
python3 -m pytest -q scripts/tests
```

`CASES.json` lists, per PDK, the routed workspace, the post-route netlist and
SDC, and optionally the flow's OpenRCX nominal SPEF. For a schema-6 workspace
(scale stream) the campaign needs an extractor built from this branch merged
with `feat/rtl2gds-scale`.

## Limitations

* Only default-width wires and fixed single-layer-cut vias are supported. The
  calibration tables have one width per layer.
* Crossings are 2.5-D (two-cut). The 3-D loading of sparse crossings and its
  reduction next to close neighbours are not modeled (field-solver evidence).
* Via cut capacitance is not modeled; the pads are.
* Non-plate overlaps carry their area capacitance only; their edge fringe
  (Magic `sideoverlap`) is left out. A routed upper conductor that has a
  calibration plate farther below does not count a nearer non-plate overlap.
* Diagonal adjacent-layer coupling (`DIAGUNDER`) is not modeled.
* The overlap area of a crossing is in both cuts (about
  `area_cap * w_upper * w_lower`, 2.6 aF for Sky130 met2/met1).
* Piece and passive strips count area per strip, so their area term is
  approximate (under 5 % of a pad's or end's capacitance); the overlap-area
  term of a piece is counted on its x strip only.
* A lateral pair is split half-and-half; where one side's nearest shape
  differs from the other's, the pair is counted unevenly.
* Same-net metal is joined only through the stored record graph. Routing
  patches add capacitance but do not conduct.
* GF180 via resistance has no corner data in the PDK; nominal is used at min
  and max.
* Magic timing is a back-annotation of Magic's per-net totals onto the native
  trees; it is accurate only to the fidelity control, because an RC-driver
  model sees the distribution of R and C along a net, which one scale factor
  per net cannot reproduce.
* The field solver uses a homogeneous dielectric and idealised environments
  (infinite wires, periodic crossings); its factors are evidence, not model
  data.
* Serial `cpu_ref` only. There is no GPU path and none is claimed.
