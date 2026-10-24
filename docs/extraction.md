# Native RC extraction

`rustroad extract` turns the routed HEAD revision of a workspace into detailed
IEEE 1481 SPEF. It is a read-only command. It reads the stored routing records
(`routing.rbin`), the placed pin/obstruction geometry and the supply metal, and
a per-PDK, per-corner RC profile built from PDK data files. Two independent
black-box oracles check it: OpenRCX (`scripts/compare_spef.py`) and Magic
(`scripts/magic_rc.py`); `scripts/rc_oracles.py` runs all three together. No
reference source was read, and no native coefficient was fitted to either
oracle's output.

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
  [--max-coupling-caps 1000000] [--max-extract-work 200000000]
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
* The byte cap (`--max-spef-mib`, at most 256) is enforced while writing.
  Shape, node, element, coupling, candidate, bin and work budgets fail with
  `BUDGET_EXCEEDED`. Errors use the existing `{status, code, message}` stderr
  envelope and exit code 2.
* stdout is a JSON report (`rustroad.extract.v1`): output path, sha256 and
  bytes; profile identity; `scope` (corner, `wire_model: rc_tree_per_net`,
  `coupling: cross_net_spef` plus the reason, and how each structure is
  treated); `stats`; unannotated signal nets; and diagnostics.

Repeated runs are byte-identical. The SPEF `*DATE` is a fixed string, and the
JSON report carries provenance.

## RC profile (`rustroad.rc-profile.v1`)

`scripts/build_rc_profile.py` writes `fixtures/pdk/{sky130,gf180}-rc-{min,nom,max}.json`.
`--check` fails if a tracked profile differs from a rebuild;
`--compare-ref e0d074d` fails unless every field that existed at that revision
is unchanged (this round only added fields). It reads PDK data files and
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

A profile without `area_cap_pf_per_um2` (from before this round) still loads;
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
* `DIAGUNDER` tables and the `RESOVER` tables are not used.
* Which data source each quantity follows, and how far the PDK's sources
  disagree, is measured in "Data sources" (`docs/evidence/rc-data-sources.json`).

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

JSON `stats` added this round: `duplicate_wire_edges`, `segment_cut_nodes`,
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

Measured (`docs/evidence/extraction-oracles-*.json`):

| run | wall | peak RSS |
|---|---|---|
| native extraction, pipeline.v, per corner | 0.017-0.025 s | 8.8-9.3 MiB |
| native extraction, regfile (426 nets, 9.5 k RC nodes), per corner | 0.30-0.60 s | 29-34 MiB |
| OpenRCX container, pipeline.v, per corner | 0.32-0.43 s | 110-127 MiB |
| Magic capacitance / resistance run, pipeline.v, per corner | 0.46-0.82 s / 0.26-0.31 s | 67-73 MiB |
| `rc_oracles.py campaign`, pipeline.v, both PDKs, five ablation binaries (681-769 guarded processes) | 45-48 s | 136 MiB (largest child) |
| `rc_oracles.py campaign`, pipeline.v, both PDKs, no ablation (RustSTA 09bf9f85 rerun) | 38-40 s | 136 MiB |
| `rc_oracles.py campaign`, regfile, both PDKs, three ablation binaries | 446 s | 137 MiB (largest child) |
| `rc_structures.py` (15 structures x 2 PDKs, three extractors) | 11.9 s | 44 MiB |
| `cargo build --release --bins --examples` (4 jobs) | 11.4 s | 0.65 GiB |
| `cargo test --workspace --release` (4 threads, 155 tests) | 22.2 s | 0.65 GiB |
| ablation builds (four variants, 4 jobs each) | 63 s | 0.66 GiB |

No run exceeds 4 GiB, so no scale lock is needed. Builds and tests used
`CARGO_BUILD_JOBS=4`, `RUST_TEST_THREADS=4` and
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
  profiles drop only the new fields; LEF OBS stripping; design totals and the
  between/below/above classification.
* **Campaign self-checks and controls** (`scripts/rc_oracles.py`, all passed on
  pipeline.v, both designs, both PDKs): native byte-identical rerun; read-only
  workspace; refusals; independent resistance accounting from the route
  records (collinear records merged; nets whose record graph has a loop are
  left out and listed: none on pipeline.v, five on Sky130 regfile; max relative
  error 3.0e-6); Magic determinism; back-annotation fidelity (OpenRCX's per-net
  totals on the native trees time within 0.31 ps of OpenRCX's own SPEF);
  OpenRCX design total unchanged at `-coupling_threshold 0`; OpenRCX per-net C
  unchanged without the cell LEF OBS; negative controls (x3 resistance, dropped
  D_NET, renamed net, dropped `*I`, planted open in the GDS).
* **Canonical structures** (`scripts/rc_structures.py`,
  `docs/evidence/rc-structures.json`): the native column is the extractor itself
  (`crates/rustroad-core/examples/extract_structures.rs` calls
  `extract_cpu_ref` with the tracked profile), not a formula.

## Oracles

**OpenRCX** (pinned `openroad/orfs` image `f152917e`) reads the DEF the flow
writes (maximal collinear runs, geometry-identical to the native GDS), the PDK
technology and cell LEF and the calibration table of the corner, with
`-max_res 0 -coupling_threshold 0.1 -no_merge_via_res`. Its nominal SPEF equals
the flow's own OpenRCX SPEF net for net on both pipeline.v designs. It reads the
same calibration tables as the native profile; its resistance comes from the
tables' `RESOVER` rows and the LEF via resistances (see "Data sources").

Black-box behaviour measured this round (inputs varied, outputs compared; no
source read):

* **Cell obstructions are not extracted.** The same DEF with a cell LEF
  stripped of every OBS block (437 Sky130 / 190 GF180 blocks) gives identical
  per-net C (difference 0.0 pF, every net, both designs and regfile). OpenRCX
  sees routing, special nets and pins, not the cells' internal metal. The
  native extractor and Magic both do.
* **Sub-threshold couplings are grounded in one net.** At
  `-coupling_threshold 0` the design total (`sum(ground) + sum(coupling)/2`)
  is identical to the 0.1 fF run at every corner, while the per-net sum is
  0.1-1.5 % higher. So a grounded small coupling stays in one net's total and
  leaves the other. (Round 1 explained the total-C miss as a double count of
  these couplings; that was wrong, see "Acceptance status".)
* **A crossing is two grounded increments.** Crossing couplings never appear in
  its SPEF; each wire gets its own ground increment. The upper-wire increment
  for a crossing lower conductor 0.14 um and 0.48 um wide is the same
  (25.7 aF, Sky130 met2/met1); it grows only for wider plates (64.4 aF at
  1.44 um, 206 aF at 5 um, where native gives 195.5 aF).
* **Stacked parallel wires are full plates.** An aligned met2/met1 pair gets
  78 aF/um of coupling on top of each wire's unchanged ground (Magic: 19 aF/um;
  native: 19 aF/um from the PDK's overlap data).
* On regfile, OpenRCX writes two parallel resistors at a pin access in two
  GF180 nets (0.11-0.12 ohm); RustSTA rejects meshes, so those resistors are
  opened (counted in `openrcx.<corner>.loops_opened_for_sta`) before timing.
  Capacitance is unchanged; on pipeline.v nothing is opened.

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
SPEF: within 0.31 ps on pipeline.v with RustSTA `c407c742`, but up to 17 ps on
regfile and 4.6 ps on pipeline.v with RustSTA `09bf9f85` (see "RustSTA version
dependence"). Where it fails, Magic slack deltas are bounded only to that
accuracy.

## Three-way comparison (round 2, pipeline.v)

Commands (extractor `6c5f267f…` from `f2a6126`/`abbf4b5`, RustSTA 0.4
`rusteda c8f71b63…`, `ruststa c407c742…`):

```sh
python3 scripts/rc_oracles.py campaign --workspace RUN --base-rev empty --inputs CASES.json \
  --rustroad target/release/rustroad --sta-bin STA_BIN \
  --variant e0d074d=BASE_BIN:legacy --variant round1=ROUND1_BIN:legacy \
  --variant split_only=... --variant no_passive_cuts=... --variant no_overlap_area=...
python3 scripts/rc_oracles.py evidence --summary RUN/summary.json --out docs/evidence/extraction-oracles-<design>.json
```

Designs (`pipeline.v`, policy producer, LEF-rule router):

* **pre**: the routed flow of `scripts/verify_routing.py` before hold closure
  (DEF byte-identical to the b0 requalification run);
  `docs/evidence/extraction-oracles-pre.json`.
* **hold**: the hold-closed final design of the hold stream (four delay cells
  on each PDK), read-only copies; `docs/evidence/extraction-oracles-hold.json`.

The targets are the fixed ones (total C +/-10 %, per-net C +/-20 % for nets
>= 5 fF, per-net R +/-20 % for nets >= 20 ohm, endpoint slack delta <= 10 ps),
evaluated against each oracle. Every control passes on both designs.

### Endpoint slack delta (max abs setup / hold, ps)

| design | PDK | corner | vs OpenRCX | vs Magic (back-annotated) | OpenRCX vs Magic | vs OpenRCX, RustSTA 09bf9f85 | fidelity, RustSTA 09bf9f85 |
|---|---|---|---|---|---|---|---|
| pre | sky130 | tt+nom | 5.1 / 4.9 **pass** | 9.0 / 4.5 | 14.1 / 6.4 | 5.1 | 0.03 |
| pre | sky130 | ss+max | 13.6 / 10.3 fail | 46.8 / 20.0 | 60.4 / 22.3 | 13.5 | 0.25 |
| pre | sky130 | ff+min | 2.8 / 2.5 **pass** | 6.2 / 4.0 | 8.1 / 6.1 | 2.8 | 0.16 |
| pre | gf180 | tt+nom | 5.2 / 4.3 **pass** | 41.9 / 35.6 | 37.7 / 31.5 | 5.2 | 0.06 |
| pre | gf180 | ss+max | 10.3 / 6.9 fail | 52.1 / 18.8 | 60.1 / 22.0 | 6.1 | 0.30 |
| pre | gf180 | ff+min | 3.0 / 2.8 **pass** | 18.8 / 15.9 | 16.0 / 13.3 | 3.0 | 0.03 |
| hold | sky130 | tt+nom | 4.9 / 5.1 **pass** | 9.1 / 4.3 | 5.8 / 5.8 | 5.1 | 0.04 |
| hold | sky130 | ss+max | 9.0 / 9.0 **pass** | 37.8 / 16.4 | 33.7 / 21.3 | 8.9 | 0.26 |
| hold | sky130 | ff+min | 3.0 / 3.0 **pass** | 8.0 / 5.9 | 6.0 / 6.0 | 3.0 | 0.16 |
| hold | gf180 | tt+nom | 4.0 / 2.6 **pass** | 26.6 / 20.3 | 26.0 / 19.8 | 4.0 | 0.05 |
| hold | gf180 | ss+max | 9.9 / 7.7 **pass** | 29.6 / 27.3 | 36.1 / 27.5 | 10.3 | 4.56 |
| hold | gf180 | ff+min | 2.1 / 1.2 **pass** | 11.0 / 8.2 | 10.0 / 7.3 | 2.1 | 0.03 |

The hold-closed design meets the slack target against OpenRCX at every corner
on both PDKs; the pre-hold design misses it at ss+max on both PDKs (13.6 and
10.3 ps). The hold-closed design keeps its hold margin with this extractor:
worst hold on the native SPEF +0.081 / +0.065 / +0.115 ns (Sky130 tt/ss/ff) and
+0.242 / +0.367 / +0.194 ns (GF180), against +0.076 / +0.056 / +0.112 and
+0.241 / +0.364 / +0.194 ns on OpenRCX; setup stays positive.

### Capacitance and resistance targets

| design | PDK | RC | total C vs OpenRCX | per-net C vs OpenRCX | total C vs Magic | per-net C vs Magic | per-net R vs OpenRCX | per-net R vs Magic |
|---|---|---|---|---|---|---|---|---|
| pre | sky130 | min | +0.8 % | 7, -1.1..+6.2 % pass | +13.8 % | 5, +1.2..+19.9 % pass | -74.9..-36.3 % fail | -65.7..-13.1 % fail |
| pre | sky130 | nom | +0.7 % | 9, -1.2..+6.7 % pass | +4.7 % | 10, -7.8..+8.3 % pass | -20.9..-2.3 % fail | -37.4..-5.5 % fail |
| pre | sky130 | max | +0.6 % | 10, -0.3..+7.9 % pass | -9.5 % | 10, -17.8..+1.8 % pass | +7.4..+137.8 % fail | -25.6..+11.5 % fail |
| pre | gf180 | min | -2.6 % | 12, -1.2..+2.9 % pass | +19.4 % | 12, +8.8..+18.9 % pass | +0.9..+13.5 % pass | +32.3..+110.1 % fail |
| pre | gf180 | nom | -3.3 % | 13, -1.7..+2.8 % pass | +22.6 % | 12, +11.0..+24.1 % fail | -2.5..-1.3 % pass | -1.2..+0.0 % pass |
| pre | gf180 | max | -4.1 % | 13, -2.2..+2.7 % pass | +7.8 % | 14, -10.2..+15.7 % pass | +8.7..+178.3 % fail | -0.8..+0.0 % pass |
| hold | sky130 | min | -1.2 % | 6, -10.6..+6.5 % pass | +12.4 % | 5, +3.0..+18.0 % pass | -75.4..-36.3 % fail | -66.4..-12.9 % fail |
| hold | sky130 | nom | -1.4 % | 8, -11.1..+7.1 % pass | +2.8 % | 12, -12.0..+6.8 % pass | -20.9..-2.7 % fail | -37.6..-5.5 % fail |
| hold | sky130 | max | -1.7 % | 11, -9.7..+7.1 % pass | -10.8 % | 15, -27.0..-3.2 % fail | +6.7..+134.2 % fail | -25.7..+11.5 % fail |
| hold | gf180 | min | -4.4 % | 11, -2.5..+2.7 % pass | +18.2 % | 11, +4.5..+18.3 % pass | +0.4..+13.2 % pass | +32.5..+110.1 % fail |
| hold | gf180 | nom | -5.0 % | 11, -3.0..+2.6 % pass | +20.8 % | 11, +8.2..+21.5 % fail | -3.6..-1.3 % pass | -1.2..+0.6 % pass |
| hold | gf180 | max | -6.0 % | 13, -3.6..+2.5 % pass | +9.3 % | 13, -9.2..+15.2 % pass | +8.0..+175.9 % fail | -0.8..+0.5 % pass |

Total and per-net C now meet their targets against OpenRCX at every corner of
both designs (round 1: hold GF180 max total C -11.3 %). The same holds against
OpenRCX at `-coupling_threshold 0` (every coupling explicit; total C -6.0..+0.8 %,
per-net C within -11.1..+7.1 %). Against Magic the misses follow the data
(isolated-wire C from the tables is 11-14 % above Magic at nominal on GF180;
Magic's Sky130 max corner is 15-30 % above the tables).

### Ablation (max abs slack delta in ps, against OpenRCX / against Magic)

Each column is an extractor binary on the same workspaces and oracles:
`e0d074d` (before stream extract2), `round1` (the previous round, rebuilt
from `0851d03`), `split_only` (round 1 plus this round's storage-independent
splitting), `no_passive_cuts` (overlap area on), `no_overlap_area` (passive
cuts on), `final` (both). Built by `scripts/extraction_ablation.py`; the two
older binaries read profiles without the new fields (`:legacy`).

| design | PDK | corner | e0d074d | round1 | split_only | no_passive_cuts | no_overlap_area | final |
|---|---|---|---|---|---|---|---|---|
| pre | sky130 | tt+nom | 6.4 / 12.0 | 4.1 / 12.8 | 4.1 / 12.8 | 4.9 / 10.5 | 4.1 / 11.3 | 5.1 / 9.0 |
| pre | sky130 | ss+max | 12.7 / 53.0 | 7.8 / 57.2 | 7.8 / 57.2 | 10.0 / 50.8 | 8.1 / 53.1 | 13.6 / 46.8 |
| pre | sky130 | ff+min | 3.6 / 7.5 | 2.5 / 7.3 | 2.5 / 7.3 | 2.5 / 6.2 | 2.4 / 6.5 | 2.8 / 6.2 |
| pre | gf180 | tt+nom | 10.1 / 28.6 | 3.0 / 34.7 | 3.0 / 34.7 | 2.1 / 35.6 | 4.3 / 41.0 | 5.2 / 41.9 |
| pre | gf180 | ss+max | 24.0 / 84.2 | 10.2 / 58.6 | 10.2 / 58.4 | 7.7 / 57.5 | 8.1 / 53.1 | 10.3 / 52.1 |
| pre | gf180 | ff+min | 4.9 / 11.6 | 1.2 / 15.2 | 1.2 / 15.2 | 1.4 / 15.6 | 2.4 / 18.4 | 3.0 / 18.8 |
| hold | sky130 | tt+nom | 11.3 / 14.3 | 7.4 / 12.0 | 7.4 / 12.0 | 6.1 / 10.7 | 6.7 / 10.4 | 4.9 / 9.1 |
| hold | sky130 | ss+max | 21.2 / 48.5 | 12.1 / 45.9 | 12.1 / 45.9 | 8.9 / 41.9 | 12.5 / 41.7 | 9.0 / 37.8 |
| hold | sky130 | ff+min | 7.3 / 4.7 | 4.8 / 5.9 | 4.8 / 5.9 | 3.9 / 7.3 | 3.9 / 6.7 | 3.0 / 8.0 |
| hold | gf180 | tt+nom | 18.1 / 19.5 | 7.8 / 18.2 | 7.7 / 18.2 | 7.1 / 18.9 | 2.4 / 26.0 | 4.0 / 26.6 |
| hold | gf180 | ss+max | 41.4 / 36.6 | 21.1 / 48.1 | 21.1 / 47.6 | 19.3 / 40.3 | 7.2 / 37.0 | 9.9 / 29.6 |
| hold | gf180 | ff+min | 9.0 / 7.8 | 3.2 / 6.9 | 3.1 / 6.9 | 2.9 / 7.4 | 1.1 / 10.7 | 2.1 / 11.0 |

Both terms were chosen before these numbers, from the canonical structures and
the two-cut construction, and neither has a free coefficient. The ablation
shows what is at stake at ss+max: every variant from round 1 on passes
against OpenRCX at tt and ff on both designs, and none passes all four ss+max
cases. `no_overlap_area` passes the pre-hold design and misses hold-closed
Sky130 (12.5 ps); `no_passive_cuts` passes the pre-hold design (9.95 / 7.7 ps)
and misses hold-closed GF180 (19.3 ps); `final` passes the hold-closed design
and misses pre-hold Sky130 (13.6 ps) and GF180 (10.3 ps). The final model is
the closest to Magic at ss+max in all four cases.

### Where the remaining deltas come from

Per-net attribution of each missed comparison (one net at a time set to the
oracle's per-net totals on the native trees; the moves add up to the measured
delta within 0.1 ps). Each listed net is classified by where its native C lies
relative to the two oracles:

| design | PDK | corner | vs | endpoint | measured | largest moves: net (ps; native - oracle C fF; native / OpenRCX / Magic fF) |
|---|---|---|---|---|---|---|
| pre | sky130 | ss+max | OpenRCX | r2/D setup | -13.6 ps | n4 (+9.6; +0.49; 2.08 / 1.59 / 3.09, between), q[0] (+4.3; +0.42; 3.51 / 3.09 / 4.86, between) |
| pre | gf180 | ss+max | OpenRCX | r2/D setup | -10.3 ps | q[0] (+6.5; +0.19; 3.46 / 3.27 / 3.63, between), n4 (+5.4; +0.29; 11.07 / 10.78 / 12.32, between), rr_clock_1 (-5.0; +0.14, between), rr_clock_0 (+3.5; +0.10, between) |

Every net behind a remaining miss lies between the two oracles: native is above
OpenRCX and below Magic. These are short nets over cells (n4, q[0]): the
native extractor counts the cells' li1/Metal1 obstructions as plates, crossing
partners and overlap partners, OpenRCX does not see them at all (OBS control
above), and Magic sees the complete cell layout and gives more still.

Across all nets at max, native lies between the oracles for 18/23 (hold
Sky130), 15/19 (pre Sky130), 11/23 (hold GF180) and 12/19 (pre GF180) nets.
Where it is outside, it is above both on GF180 port nets and `clk` (Magic's
GF180 data are lower) or below both by 0.3-9.7 % against OpenRCX (largest: hold
Sky130 `rr_eco_hold_1_n` -9.7 %, `n5` -7.0 %; hold GF180 `rr_clock_2` -4.4 %).
Round 1's systematic deficit is gone: hold GF180 had 22 of 23 nets below
OpenRCX at max (per-net sum -4.0 %), now 8 of 23 (+0.6 %); the per-net sums at
max are +0.6..+4.1 % on the four cases (round 1: -4.0..+0.2 %).

## Larger design: regfile (scale stream)

`regfile` (426 nets, 1479 / 1737 components on Sky130 / GF180), routed by the
scale stream's track router into schema-6 workspaces (signoff DRC/LVS clean on
both PDKs in that stream's `s5` run), was measured with this branch merged with
`feat/rtl2gds-scale` `b363d0f` in a scratch clone (nothing of the merge is
committed here). Netlists come from `export-verilog` (identical to that run's
`final.v`); the SDC has the pipeline clock (10 ns) with 0.2 ns input/output
delays on every data port. Evidence: `docs/evidence/extraction-oracles-regfile.json`
(compact: no per-net or per-endpoint tables).

| PDK | corner | vs OpenRCX | vs Magic | OpenRCX vs Magic | fidelity | split_only / no_passive_cuts / no_overlap_area vs OpenRCX |
|---|---|---|---|---|---|---|
| sky130 | tt+nom | 33.8 | 45.8 | 78.9 | 3.2 | 18.1 / 22.2 / 29.6 |
| sky130 | ss+max | 66.9 | 211.3 | 277.8 | 17.2 | 28.6 / 37.7 / 56.3 |
| sky130 | ff+min | 19.4 | 18.7 | 38.7 | 5.8 | 13.2 / 14.7 / 17.9 |
| gf180 | tt+nom | 23.0 | 132.7 | 122.7 | 0.3 | 23.3 / 16.9 / 15.7 |
| gf180 | ss+max | 35.0 | 285.0 | 302.4 | 13.1 | 78.1 / 66.7 / 37.2 |
| gf180 | ff+min | 14.9 | 56.5 | 49.9 | 0.8 | 9.4 / 7.0 / 11.4 |

(max abs of setup and hold delta, ps.) The slack target is missed against both
oracles at every corner on both PDKs, by every variant; the oracles disagree
with each other by 39-302 ps. Decomposition on the native trees (per-net totals
back-annotated): replacing only per-net C by OpenRCX's reproduces the delta
(Sky130 tt 34.2, ss 65.7 ps; GF180 tt 22.9, ss 34.5 ps), replacing only per-net
R moves it by 3.5 / 13.9 ps (Sky130 tt / ss) and 0.4 / 24.7 ps (GF180 tt / ss,
the corner-invariant LEF via R). So capacitance carries it, mostly on the
clock-tree nets: Sky130 `rr_clock_3` is +3.5 fF above OpenRCX and 4.1 fF below
Magic (between); GF180's large read-address nets (90-150 fF) are 3-5 % above
OpenRCX and 13 % above Magic. Per-net C meets +/-20 % against OpenRCX except
one Sky130 net each at nom (`n8`, +20.1 %) and max (`regs_q[2][6]`, +21.6 %);
total C against OpenRCX passes on
Sky130 (-1.6..-5.0 %) and misses on GF180 (-10.1..-13.3 %) while the per-net sum
is +1.7..+3.3 %: OpenRCX grounds each crossing as two separate increments (one
per wire), which the design total counts twice, while the native crossing is
one coupling capacitor counted once. The back-annotation fidelity control fails
on regfile (Sky130 3.2-17.2 ps; GF180 ss 13.1 ps), so on this design the Magic
deltas are only bounded to that accuracy. The independent resistance
accounting passes on 420 / 387 nets; five Sky130 nets whose route records form
loops (six opened loop resistors) are left out and listed.

## RustSTA version dependence

All acceptance numbers use the program's pinned RustSTA 0.4 (`rusteda
c8f71b63…`, `ruststa c407c742…`, `.work/tools/sta`). The top-level pinned
`ruststa` has since moved to `09bf9f85` (stream sta3: RC driver resistance and
lumped pi). Re-run with it (`docs/evidence/extraction-oracles-{pre,hold}-sta3.json`):
against OpenRCX the pre-hold design improves at GF180 ss+max (10.3 -> 6.1 ps)
and Sky130 ss+max stays at 13.5 ps; the hold-closed design moves to 10.3 ps at
GF180 ss+max (hold check). The back-annotation fidelity control fails with
`09bf9f85` on the hold-closed GF180 ss+max (4.56 / 3.04 ps against the 1 ps
bound): with driver resistance and the pi load, the distribution of R and C
along a net matters more, so per-net totals on native trees no longer
reproduce OpenRCX's own timing. The back-annotated Magic timing method has to
be requalified when the program moves to `09bf9f85`; until then its numbers
hold for `c407c742` only.

## Resistance against Magic, element by element

Magic uses the same Magic sheet and contact values as the native profile, so
for resistance it checks geometry and topology. Its network starts at the
terminal square, so a terminal on a pin via adds the via's contact plus half a
square of the pin layer (Sky130 li1: 15.70 ohm at nom against the native 9.3
ohm cut; 6.85 / 31.5 at min / max). With that element taken out on both sides,
every Sky130 net agrees within 2.0 % at every corner except `a[1]`, where a
routing patch bridges two met1 regions: Magic conducts through it, the native
tree does not (patches add capacitance only). GF180 agrees within 1.3 % at nom
and max on every net; at min Magic's `(lrlc)` contacts are 0 (no data) while
the native profile keeps 4.5 ohm.

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
| gf180 | Via1-4 ohm/cut | min / nom / max | 4.5 / 4.5 / 15 | 0 (no data) / 4.5 / 15 | 4.5 | - | - | - |

The calibration table's resistance is its `RESOVER` row (ohm per unit length
at the table width); times the width it is the sheet value shown. The OpenRCX
reference uses that row, and the LEF via resistance at every corner (observed
in its SPEF: 9.3 / 4.5 / 3.41 ohm on Sky130, 4.5 on GF180).

**Which source the native profile follows, and why.** Resistance follows the
Magic extraction rules of the corner variant (`(lrlc)` / `()` / `(hrhc)`):

* at nominal they agree with the technology LEF, the SPICE interconnect model
  and two of the three shipped calibration variants (`magic`,
  `spef_extractor`). The Sky130 `calibre` variant, the one the OpenRCX flow
  reads, is the outlier: met1/met2 0.173 ohm/sq (+39 %) at nominal, 0.233
  (+61 % on Magic, +68 % on SPICE) at max. No other PDK source supports it, so
  the native profile does not adopt it;
* the Magic rules are a complete per-corner set, contacts included; the SPICE
  corner models are the other complete set and lie between Magic's extremes
  and the nominal value. GF180's SPICE corners agree with Magic within 2 %;
* the reference's corner-invariant via resistance is the LEF nominal value,
  which no corner source supports.

So the per-net R gap against OpenRCX is a data-source difference (the Sky130
`calibre` met1/met2 row; the tables' wider metal spread and the
corner-invariant LEF vias at min/max on both PDKs). Against Magic, which uses
the same data, resistance agrees element by element (previous section). On
pipeline.v per-net R does not move slack measurably (round 1: OpenRCX's R on
the native trees changes nothing at ss+max); on regfile it moves GF180 ss+max by
up to 24.7 ps.

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
geometry through both oracles and the native extractor itself. Nominal corner,
40 um wires at default width; "round 1" is the previous model
(`split_only` binary):

| PDK | structure | round 1 | native | OpenRCX | Magic |
|---|---|---|---|---|---|
| sky130 | isolated met2, total aF | 3519 | 3519 | 3507 | 3140 |
| sky130 | met2 pair at minimum space, coupling aF | 4194 | 4194 | 4194 | 4561 |
| sky130 | met2 crossing met1: upper / lower increment aF | 11.2 / 23.5 | 11.2 / 23.5 | 25.7 / 19.5 | 25.3 / 33.9 |
| sky130 | met2 crossing a 0.48 um grounded met1 rail, increment aF | 18.2 | 24.1 | 25.7 | 55.0 |
| sky130 | met2 crossing a 5 um grounded plane, increment aF | 189.5 | 195.5 | 206.2 | 450.0 |
| sky130 | met3 along a 0.48 um rail, increment aF | 4.8 | 272.9 | 649.4 | 408.9 |
| sky130 | met3 over met2 crossings at 2 um pitch, increment aF | 249.5 | 249.5 | 525.8 | 654.5 |
| sky130 | met3 over minimum-pitch met2 crossings, increment aF | 1528 | 1528 | 1734 | 2420 |
| sky130 | met2 over met1 aligned, coupling aF | 0 | 752 | 3113 | 752 |
| sky130 | met3 over met1, 0.085 um overlap, coupling aF | 0 | 118 | 1645 | 272 |
| gf180 | isolated Metal2, total aF | 3331 | 3331 | 3308 | 2870 |
| gf180 | Metal2 pair at minimum space, coupling aF | 5109 | 5109 | 5109 | 3552 |
| gf180 | Metal2 crossing Metal1: upper / lower increment aF | 26.2 / 45.1 | 26.2 / 45.1 | 31.7 / 26.3 | 28.5 / 41.0 |
| gf180 | Metal2 crossing a 0.6 um grounded Metal1 rail, increment aF | 34.0 | 47.1 | 63.4 | 47.5 |
| gf180 | Metal2 crossing a 5 um grounded plane, increment aF | 283.1 | 296.2 | 317.0 | 272.6 |
| gf180 | Metal3 along a 0.6 um rail, increment aF | 6.7 | 121.1 | 957.2 | 192.2 |
| gf180 | Metal3 over Metal2 crossings at 2 um pitch, increment aF | 692 | 692 | 892 | 622 |
| gf180 | Metal3 over minimum-pitch Metal2 crossings, increment aF | 1657 | 1657 | 2198 | 1552 |
| gf180 | Metal2 over Metal1 aligned, coupling aF | 0 | 546 | 2052 | 595 |
| gf180 | Metal3 over Metal1, 0.085 um overlap, coupling aF | 0 | 69 | 973 | 161 |

* Isolated wires and pairs: native and OpenRCX agree within 0.7 % (same table,
  same geometry). Magic differs by its data.
* Crossings over passive metal (rails, planes): round 1 lacked the rail's own
  cut. Now native lies between the oracles (GF180 3x rail and plane) or within
  7 % of the nearer one (Sky130: 5-7 % below OpenRCX, Magic's data are 2.3x;
  GF180 0.6 um rail: 1 % below Magic).
* Wire along a slightly wider rail and stacked or offset wires: round 1 gave
  nothing; the overlap-area term gives Magic's area value (identical for the
  aligned case, by the shared data) and leaves out the edge fringe (Magic
  sideoverlap): native is below Magic by that fringe and far below OpenRCX,
  which makes every such overlap a full plate.
* The Sky130 routed crossing (upper wire 11.2 aF against 25.7 / 25.3) is the one
  case where both oracles are about twice native, and they get there by
  different routes. OpenRCX gives 25.7 aF to the upper wire for a 0.14 um and a
  0.48 um lower conductor alike (it does not scale with the overlapped width),
  and its rail and plane values are 5-7 % above native. Magic's Sky130
  inter-layer data are 2.3x the calibration table's for the same pair (the
  5 um plane: 450 against 195-206 aF). The crossing coupling itself is 23.5 aF
  native and 33.9 aF Magic; the native upper-wire increment is that coupling
  minus the substrate it shields (12.2 aF), which Magic shields by 8.6 aF.
  Neither difference points at missing native structure; adopting either would
  mean adopting that oracle's model or data.

## Acceptance status (round 2)

* **Slack delta <= 10 ps against a geometry-identical oracle at every corner:
  met on the hold-closed design, not on the pre-hold design.** Against OpenRCX
  the hold-closed (signoff) design passes at all six PDK x corner combinations
  (worst 9.9 ps, GF180 ss+max); the pre-hold design misses at ss+max on both
  PDKs (Sky130 13.6 ps, GF180 10.3 ps), 10 of 12 overall. With RustSTA
  `09bf9f85` it is also 10 of 12 (pre Sky130 ss+max 13.5 ps, hold GF180
  ss+max 10.3 ps). On the larger regfile design the target is missed at every
  corner against both oracles (OpenRCX 15-67 ps).
* **Explained bound against the other oracle.** Against Magic the deltas are
  up to 52 ps on pipeline.v at ss+max, the same size as the OpenRCX-to-Magic
  disagreement (up to 60 ps). Every net behind the remaining OpenRCX misses lies
  between the two oracles (attribution table). The difference against Magic is
  data (isolated-wire C 11-14 % apart at nominal, inter-layer data 2.3x on
  Sky130, corner spreads -14 % / +22 % against +/-5 %), plus Magic's complete
  cell geometry and edge fringe of overlaps; against OpenRCX it is OpenRCX's
  treatment of cells (no OBS), crossings (two grounded increments, not scaled
  by the overlapped width) and stacked metal (full plates). The ablation shows
  that the ss+max result moves by several ps with each physically motivated
  term and that no variant meets 10 ps at ss+max on both designs: the target
  is below the spread between the two oracles and between defensible model
  choices for metal the calibration tables do not describe. Closing the last
  pre-hold misses would mean ignoring cell obstructions or treating stacked
  metal as full plates, i.e. adopting OpenRCX's view, which this stream does
  not do.
* **Per-net C targets** against OpenRCX are met at every corner on both
  pipeline.v designs, and total C now too (round 1's GF180 max -11.3 % is
  -6.0 %). Round 1's explanation of that miss (sub-threshold couplings counted
  twice) was wrong: OpenRCX's design total is identical at threshold 0. The
  real cause is the ground/coupling split: OpenRCX writes a crossing as two
  ground increments that the design total counts separately, native writes one
  coupling capacitor counted once; with the passive cuts and overlap area the
  pipeline totals are within 6 %, and on GF180 regfile the metric still misses
  (-13.3 %) with the per-net sum at +1.7 %.
* **Resistance** discrepancies are explained by data source (calibration
  `RESOVER` rows and corner-invariant LEF vias against Magic/SPICE corner data)
  and, against Magic, element by element (terminal element, one patch-bridged
  net, GF180 `(lrlc)` contacts without data).
* **Storage independence.** Runs longer than the segment limit are cut at the
  same exact coordinates for any record length (unit test with lattice edges,
  one compact record through a T junction and an overlapping duplicate);
  regfile's schema-6 compact segments extract with 67 / 255 virtual cut nodes.

## History

* Round 1 of this stream (`0851d03`, binary `de7428b9`): via pads, end
  extensions, covered sides, crossings from both wires' cross-sections, plates
  must reach one strip width beyond both edges, Magic oracle. Against OpenRCX
  9 of 12 (misses: pre GF180 ss+max 10.17 ps, hold Sky130 ss+max 12.1 ps, hold
  GF180 ss+max 21.1 ps); the misses came from nets below both oracles.
* Before stream extract2 (`docs/evidence/extraction-comparison.json`,
  `docs/evidence/extraction-requalification.json`): native -4..-9 % low in total
  C against OpenRCX, slack deltas 12.7 ps (Sky130 ss), 10.1 and 24.0 ps (GF180
  tt, ss) on the pre-hold design.
* The per-edge DEF of earlier flows made the OpenRCX reference depend on path
  segmentation (+134..+144 % C on the same metal); the flow's DEF now writes
  maximal runs, and the campaign checks per design that its OpenRCX equals the
  flow's (`openrcx.flow_reference_nominal`: 0.0 %).

## Reproduction

```sh
free -h
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G timeout 900s \
  cargo build --release --bins --examples
python3 scripts/build_rc_profile.py --check
python3 scripts/build_rc_profile.py --compare-ref e0d074d
python3 scripts/extraction_ablation.py --out .work/abl --build     # ablation binaries
python3 scripts/verify_routing.py --workspace .work/runs/flowA --base-rev empty \
  --rustroad target/release/rustroad --rusteda STA/rusteda --ruststa STA/ruststa --producer policy
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v \
  timeout 3000s python3 scripts/rc_oracles.py campaign --workspace .work/runs/r2/pre \
  --base-rev empty --inputs CASES.json --rustroad target/release/rustroad --sta-bin STA \
  [--variant NAME=BIN[:legacy] ...] [--attribution-max-nets 30]
python3 scripts/rc_oracles.py report --summary .work/runs/r2/pre/summary.json
python3 scripts/rc_oracles.py evidence --summary .work/runs/r2/pre/summary.json --out X.json [--compact]
python3 scripts/rc_structures.py --out .work/runs/structures \
  --native-bin target/release/examples/extract_structures --evidence docs/evidence/rc-structures.json
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
* Via cut capacitance is not modeled; the pads are.
* Non-plate overlaps carry their area capacitance only; their edge fringe
  (Magic `sideoverlap`) is left out. Magic's values bound it (canonical
  structures: Sky130 met3 along a rail 409 aF against 273 aF native per 40 um).
  A routed upper conductor that has a calibration plate farther below does not
  count a nearer non-plate overlap.
* Diagonal adjacent-layer coupling (`DIAGUNDER`) is not modeled.
* A crossing is counted from both cross-sections; the overlap area itself is
  in both (about `area_cap * w_upper * w_lower`, 2.6 aF for Sky130 met2/met1).
  Three-dimensional corner fringe of narrow crossings is not modeled.
* Piece and passive strips count area per strip, so their area term is
  approximate (under 5 % of a pad's or end's capacitance); the overlap-area
  term of a piece is counted on its x strip only.
* A lateral pair is split half-and-half; where one side's nearest shape
  differs from the other's, the pair is counted unevenly.
* Same-net metal is joined only through the stored record graph. Routing
  patches add capacitance but do not conduct, so a patch that bridges two
  same-net regions is not a parallel resistance path (Magic: `a[1]`, about
  9 ohm at nom on Sky130).
* OpenRCX reads DEF `RECT` patches without error but does not extract them,
  and does not extract cell obstructions; the native extractor and Magic include
  both.
* Magic timing is a back-annotation of Magic's per-net totals onto the native
  trees; it is qualified (fidelity <= 0.31 ps) for pipeline.v with RustSTA
  `c407c742` only, not for regfile or RustSTA `09bf9f85`.
* Serial `cpu_ref` only. There is no GPU path and none is claimed.
