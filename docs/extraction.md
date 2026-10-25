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
  [--max-rc-elements 1000000] [--max-extract-candidates 4096]
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
  treated); `stats`; unannotated signal nets; and diagnostics. Nets with no
  pin (`pinless_signal_nets`) and nets with exactly one pin
  (`single_pin_signal_nets`: an unused input port or an unloaded output pin;
  the router routes nets of two or more terminals only) have nothing to
  extract and are listed separately, never as unannotated.
* Candidate shapes per wire run (`--max-extract-candidates`, default 4,096)
  were a fixed cap; they are a budget now. Per run the extractor holds the
  candidate list, two clamped cut points per candidate and at most one plane
  entry per candidate: 44 B per candidate (`CANDIDATE_SCRATCH_BYTES`), one run
  at a time. The ceiling is 48 MiB of that scratch, 1,143,901 candidates
  (`MAX_CANDIDATES_CEILING`); the quadratic intervals x candidates work stays
  charged to `--max-extract-work`. The report states `max_candidates_per_run`,
  `candidate_scratch_bytes` and the observed `stats.max_run_candidates`.
  `--max-rc-elements` (resistors + nodes + 2 x couplings, ceiling 8x the
  default) is a command-line budget too. SERV (scale-stream layout, sky130,
  7,959 nets): 8,054 candidates on its largest run, 4.3 M elements, work
  7.4e9, 20.6 s and 521 MB peak; the track recipe of the gate passes 65,536
  candidates (2.9 MB scratch) and 8 M elements.

Repeated runs are byte-identical. The SPEF `*DATE` is a fixed string, and the
JSON report carries provenance.

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
  and are kept. The previous choice (15 ohm) is measured as the `round2_data`
  ablation column; `rc_data_sources.py` tabulates every variant's block.
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

Measured (`docs/evidence/extraction-oracles-*.json`, round 3):

| run | wall | peak RSS |
|---|---|---|
| native extraction, pipeline.v, per corner | 0.017-0.063 s | 8.9-9.8 MiB |
| native extraction, regfile / mult (426-440 nets), per corner | 0.33-0.65 s | 29-35 MiB |
| OpenRCX container, pipeline.v, per corner | 0.31-0.43 s | 110-127 MiB |
| Magic capacitance / resistance run, pipeline.v, per corner | 0.46-0.86 s / 0.26-0.31 s | 67-74 MiB |
| `rc_oracles.py campaign`, pipeline.v, both PDKs, three ablation variants, lossless OpenRCX timing (553-809 guarded processes) | 40-52 s | 136 MiB (largest child) |
| `rc_oracles.py campaign`, pipeline.v, both PDKs, no ablation (RustSTA `c407c742`) | 41-48 s | 136 MiB |
| `rc_oracles.py campaign`, regfile / mult, both PDKs, one variant, attribution capped at 30 nets | 541-584 s | 137 MiB (largest child) |
| `rc_structures.py` (21 structures x 2 PDKs, three extractors, OpenRCX via probe) | 18.7 s | 44 MiB |
| `field_solver.py crossings` (100 quarter-domain 3-D solves of at most 125 k nodes, 2 BLAS threads) | 8 min 44 s | 2.2 GB |
| `cargo build --release --bins --examples` (4 jobs) | 17.1 s | 0.66 GiB |
| `cargo test --workspace --release` (4 threads, 155 tests) | 13.2 s | 0.52 GiB |

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
  profiles drop only the new fields; LEF OBS stripping; signal-pin shrinking
  (supply pins and OBS untouched); report rows carry the bound their verdict
  uses (fidelity 1 ps); blanket corner contact blocks are detected and the
  tracked GF180 max profile keeps the nominal via resistance; the field
  solver's self-test and stack parsing; design totals and the
  between/below/above classification (69 tests).
* **Campaign self-checks and controls** (`scripts/rc_oracles.py`): native
  byte-identical rerun; read-only workspace; refusals; independent resistance
  accounting from the route records (collinear records merged; nets whose
  record graph has a loop are left out and listed: none on pipeline.v, five on
  Sky130 regfile; max relative error 3.0e-6); Magic determinism; back-annotation
  fidelity (OpenRCX's per-net totals on the native trees must time within
  1 ps of OpenRCX's own SPEF; see "RustSTA version dependence" for where it
  fails); OpenRCX design total unchanged at `-coupling_threshold 0`; OpenRCX
  per-net C unchanged without the cell LEF OBS; no net loses more than 0.1 fF
  of OpenRCX capacitance with every signal pin of the cell LEF shrunk to a
  10 nm square (round 3; bit-identical on pipeline.v, increases only from a
  net's own wire over its own pin on regfile and mult); negative controls (x3 resistance, dropped D_NET, renamed net,
  dropped `*I`, planted open in the GDS).
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
or `-coupling_threshold 0`. Its nominal SPEF at 0.1 equals the flow's own
OpenRCX SPEF net for net on both pipeline.v designs. It reads the same
calibration tables as the native profile; its resistance comes from the
tables' `RESOVER` rows and the LEF via resistances (see "Data sources").

Black-box behaviour (inputs varied, outputs compared; no source read). Each
item is a campaign control or a canonical structure, re-run every campaign:

* **Cell pin metal is not a partner of any net (round 3).** The same DEF with
  every signal pin of the cell LEF shrunk to a 10 nm square (4688 Sky130 / 2600
  GF180 rectangles) gives bit-identical per-net C and R on pipeline.v; on
  regfile and mult no net loses more than 0.06 fF, while up to 250 nets gain
  (up to 3.7 fF) and R changes, because OpenRCX merges a net's own wire metal
  over its own pin into the pin while the pin is full size. Pins are real
  metal: on the
  pre-hold Sky130 design, net `n4` runs 0.24 um from the met1 `B` pin of an
  `xor2_2` (net `b[1]`) for 3 um; native couples them by 0.241 fF at max,
  Magic by 0.316 fF, OpenRCX by 0 even at threshold 0.
* **Cell obstructions are not extracted.** A cell LEF stripped of every OBS
  block (437 Sky130 / 190 GF180) gives identical per-net C.
* **Via landing metal is not extracted (round 3).** On a single wire, a via
  (or a two-via stack) adds exactly what splitting the wire path at that point
  adds (Sky130 +13.06 aF, GF180 +21.94 aF, the extra end extension); the pads
  themselves add 0.0 aF. Native and Magic extract the pads.
* **At `-coupling_threshold 0.1` a small coupling is grounded in one net and
  dropped from the other.** The design total (`sum(ground) + sum(coupling)/2`)
  is identical to the threshold-0 run at every corner while per-net totals
  drop (Sky130 per-net sum -1.2..-1.3 %; `n4` at max: 1.587 fF at 0.1 against
  1.724 fF at 0, -8 %). The native SPEF writes every coupling in both nets, so
  the like-for-like OpenRCX configuration for per-net C and timing is
  threshold 0 ("lossless"). Both are reported; the flow configuration alone
  moves OpenRCX's own ss+max slacks by up to 7.3 ps (pre Sky130) and 9.0 ps
  (hold GF180) against the lossless one.
* **Capacitance monotonicity is violated in one structure (round 3).** A
  GF180 Metal3 wire with minimum-space neighbours on both sides loses 178 aF
  of total capacitance when 18 Metal2 wires cross under it at 2 um pitch. With
  every other conductor at 0 V a conductor's capacitance is an energy minimum
  over a set that adding conductors only shrinks, so it cannot decrease;
  native (+321 aF) and Magic (+422 aF) increase.
* **A crossing is two grounded increments.** Crossing couplings never appear in
  its SPEF; each wire gets its own ground increment. The upper-wire increment
  for a crossing lower conductor 0.14 um and 0.48 um wide is the same
  (25.7 aF, Sky130 met2/met1); it grows only for wider plates.
* **Stacked parallel wires are full plates.** An aligned met2/met1 pair gets
  78 aF/um of coupling (Magic and native: 19 aF/um from the PDK's overlap data).
* On regfile, OpenRCX writes two parallel resistors at a pin access in two
  GF180 nets (0.11-0.12 ohm); RustSTA rejects meshes, so those resistors are
  opened (counted in `openrcx.<corner>.loops_opened_for_sta`) before timing.

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
SPEF (bound 1 ps). See "RustSTA version dependence" for where it fails.

## Three-way comparison (round 3, pipeline.v)

Extractor `6c5f267f…` (no Rust change this round; the profiles changed: GF180
max cut resistance). RustSTA: the program's current pin `ruststa 09bf9f85…`
(stream sta3, `.work/sta-signoff/bin`) is the primary; the previous pin
`c407c742…` is measured too. `rusteda c8f71b63…` for both.

```sh
python3 scripts/rc_oracles.py campaign --workspace RUN --base-rev empty --inputs CASES.json \
  --rustroad target/release/rustroad --sta-bin STA_DIR \
  --variant e0d074d=BASE_BIN:legacy --variant round1=ROUND1_BIN:legacy \
  --variant round2_data=target/release/rustroad:profiles=PROFILES_AT_66411bd
python3 scripts/rc_oracles.py evidence --summary RUN/summary.json --out docs/evidence/extraction-oracles-<design>.json
```

Designs (`pipeline.v`, policy producer, LEF-rule router):

* **pre**: the routed flow of `scripts/verify_routing.py` before hold closure
  (DEF byte-identical to the b0 requalification run);
  `docs/evidence/extraction-oracles-pre.json` (09bf9f85),
  `extraction-oracles-pre-c407c742.json`.
* **hold**: the hold-closed final design of the hold stream (four delay cells
  on each PDK), snapshot copies; `docs/evidence/extraction-oracles-hold.json`,
  `extraction-oracles-hold-c407c742.json`.

The targets are the fixed ones (total C +/-10 %, per-net C +/-20 % for nets
>= 5 fF, per-net R +/-20 % for nets >= 20 ohm, endpoint slack delta <= 10 ps),
evaluated against each oracle.

### Endpoint slack delta (max of the setup and hold delta, ps)

Bold: within 10 ps. "fidelity" is the back-annotation control (bound 1 ps).

| design | PDK | corner | flow OpenRCX (0.1 fF) | lossless OpenRCX (threshold 0) | OpenRCX 0.1 vs 0 | Magic (back-annotated) | OpenRCX vs Magic | fidelity | c407c742: flow / lossless | c407c742 fidelity |
|---|---|---|---|---|---|---|---|---|---|---|
| pre | sky130 | tt+nom | **5.1** | **4.1** | 3.7 | 8.9 | 14.1 | 0.03 | 5.1 / 4.1 | 0.03 |
| pre | sky130 | ss+max | 13.5 miss | **8.1** | 7.3 | 47.1 | 60.6 | 0.25 | 13.6 / 8.2 | 0.25 |
| pre | sky130 | ff+min | **2.8** | **2.1** | 2.4 | 6.2 | 8.2 | 0.16 | 2.8 / 2.1 | 0.16 |
| pre | gf180 | tt+nom | **5.2** | **5.2** | 0.1 | 41.9 | 37.7 | 0.06 | 5.2 / 5.2 | 0.06 |
| pre | gf180 | ss+max | 10.1 miss | 10.1 miss | 0.2 | 48.2 | 56.0 | 0.12 | 10.0 / 10.0 | 0.12 |
| pre | gf180 | ff+min | **3.0** | **3.0** | 0.0 | 16.7 | 13.9 | 0.03 | 3.0 / 3.0 | 0.03 |
| hold | sky130 | tt+nom | **5.1** | **5.4** | 2.1 | 9.1 | 5.8 | 0.04 | 5.1 / 5.4 | 0.04 |
| hold | sky130 | ss+max | **8.9** | **9.1** | 4.2 | 37.9 | 33.8 | 0.26 | 9.0 / 9.2 | 0.26 |
| hold | sky130 | ff+min | **3.0** | **3.4** | 1.3 | 8.0 | 6.1 | 0.16 | 3.0 / 3.4 | 0.16 |
| hold | gf180 | tt+nom | **4.0** | **3.9** | 3.9 | 26.6 | 26.0 | 0.05 | 4.0 / 3.9 | 0.05 |
| hold | gf180 | ss+max | **9.5** | **7.1** | 9.0 | 27.9 | 36.3 | 4.44 | 9.7 / 9.1 | 0.12 |
| hold | gf180 | ff+min | **2.1** | **1.9** | 2.0 | 9.1 | 8.1 | 0.03 | 2.1 / 1.9 | 0.03 |

* **Hold-closed design (signoff):** within 10 ps of OpenRCX at all six PDK x
  corner combinations, against both OpenRCX configurations and with both
  RustSTA pins; worst 9.5 / 9.1 ps (flow / lossless) with `09bf9f85` and
  9.7 / 9.2 ps with `c407c742`, so the margin is 0.3-0.9 ps.
* **Pre-hold design:** against lossless OpenRCX five of six pass; GF180 ss+max
  misses by 0.05 ps (10.05 ps, both configurations, both pins). Against the
  flow configuration Sky130 ss+max also misses (13.5 ps), by 3.5 ps, less than
  the 7.3 ps by which the flow configuration's own slacks differ from the
  lossless one's there.
* The hold gate holds on the native SPEF of the hold-closed design: worst hold
  +0.081 / +0.065 / +0.115 ns (Sky130 tt/ss/ff) and +0.242 / +0.362 / +0.194 ns
  (GF180), against +0.076 / +0.056 / +0.112 and +0.241 / +0.362 / +0.194 ns on
  flow OpenRCX (+0.0555 ns Sky130 ss on lossless OpenRCX); setup positive.

### Capacitance and resistance targets (RustSTA-independent)

| design | PDK | RC | total C vs OpenRCX (flow / lossless) | per-net C vs OpenRCX flow | per-net C vs lossless | total C vs Magic | per-net C vs Magic | per-net R vs OpenRCX | per-net R vs Magic |
|---|---|---|---|---|---|---|---|---|---|
| pre | sky130 | min | +0.8 % / +0.8 % | 7, -1.1 %..+6.2 % | 7, -2.8 %..+4.4 % | +13.8 % | 5, +1.2 %..+19.9 % | 19, -74.9 %..-36.3 % miss | 19, -65.7 %..-13.1 % miss |
| pre | sky130 | nom | +0.7 % / +0.7 % | 9, -1.2 %..+6.7 % | 9, -2.7 %..+6.2 % | +4.7 % | 10, -7.8 %..+8.3 % | 19, -20.9 %..-2.3 % miss | 19, -37.4 %..-5.5 % miss |
| pre | sky130 | max | +0.6 % / +0.6 % | 10, -0.3 %..+7.9 % | 10, -1.4 %..+5.7 % | -9.5 % | 10, -17.8 %..+1.8 % | 19, +7.4 %..+137.8 % miss | 19, -25.6 %..+11.5 % miss |
| pre | gf180 | min | -2.6 % / -2.6 % | 12, -1.2 %..+2.9 % | 12, -1.2 %..+2.9 % | +19.4 % | 12, +8.8 %..+18.9 % | 19, +0.9 %..+13.5 % | 7, +32.3 %..+110.1 % miss |
| pre | gf180 | nom | -3.3 % / -3.3 % | 13, -1.7 %..+2.8 % | 13, -1.7 %..+2.8 % | +22.6 % | 12, +11.0 %..+24.1 % miss | 19, -2.5 %..-1.3 % | 19, -1.2 %..+0.0 % |
| pre | gf180 | max | -4.1 % / -4.1 % | 13, -2.2 %..+2.7 % | 13, -2.2 %..+2.7 % | +7.8 % | 14, -10.2 %..+15.7 % | 19, -24.8 %..-7.1 % miss | 19, -66.7 %..-30.9 % miss |
| hold | sky130 | min | -1.2 % / -1.2 % | 6, -10.6 %..+6.5 % | 6, -10.6 %..+3.6 % | +12.4 % | 5, +3.0 %..+18.0 % | 23, -75.4 %..-36.3 % miss | 23, -66.4 %..-12.9 % miss |
| hold | sky130 | nom | -1.4 % / -1.4 % | 8, -11.1 %..+7.1 % | 8, -11.1 %..+5.0 % | +2.8 % | 12, -12.0 %..+6.8 % | 23, -20.9 %..-2.7 % miss | 23, -37.6 %..-5.5 % miss |
| hold | sky130 | max | -1.7 % / -1.7 % | 11, -9.7 %..+7.1 % | 12, -9.7 %..+6.3 % | -10.8 % | 15, -27.0 %..-3.2 % miss | 23, +6.7 %..+134.2 % miss | 23, -25.7 %..+11.5 % miss |
| hold | gf180 | min | -4.4 % / -4.4 % | 11, -2.5 %..+2.7 % | 11, -2.5 %..+2.4 % | +18.2 % | 11, +4.5 %..+18.3 % | 19, +0.4 %..+13.2 % | 6, +32.5 %..+110.1 % miss |
| hold | gf180 | nom | -5.0 % / -5.0 % | 11, -3.0 %..+2.6 % | 12, -3.0 %..+2.3 % | +20.8 % | 11, +8.2 %..+21.5 % miss | 19, -3.6 %..-1.3 % | 19, -1.2 %..+0.6 % |
| hold | gf180 | max | -6.0 % / -6.0 % | 13, -3.6 %..+2.5 % | 13, -3.6 %..+2.1 % | +9.3 % | 13, -9.2 %..+15.2 % | 20, -25.2 %..-8.1 % miss | 23, -66.7 %..-30.7 % miss |

Total and per-net C meet their targets against OpenRCX at every corner of both
designs (lossless: total -6.0..+0.8 %, per-net -11.1..+6.3 %). Against Magic
the misses follow the data (GF180: the tables' isolated-wire C is 5-27 % above
Magic's at nominal; Sky130: Magic's inter-layer data are 2.3x the tables' and
its max-corner isolated values up to 11 % above them). Per-net R follows the PDK data source of each tool (see "Data
sources"): at GF180 max, native now uses the nominal 4.5 ohm per cut, OpenRCX
the LEF 4.5 ohm and its table wire resistance (0.145 against 0.104 ohm/sq), and
Magic its blanket 15 ohm.

### Ablation (max of the setup and hold delta, ps; RustSTA 09bf9f85)

Columns: `e0d074d` (before stream extract2), `round1` (rebuilt from
`0851d03`), `round2_data` (this binary with the round-2 profiles, i.e. GF180
max via 15 ohm), `final`. Each cell: flow OpenRCX / lossless OpenRCX / Magic.

| design | PDK | corner | e0d074d | round1 | round2_data | final |
|---|---|---|---|---|---|---|
| pre | sky130 | tt+nom | 6.4 / 6.2 / 12.0 | 4.1 / 3.5 / 12.8 | 5.1 / 4.1 / 8.9 | 5.1 / 4.1 / 8.9 |
| pre | sky130 | ss+max | 12.7 / 10.8 / 53.3 | 7.7 / 5.7 / 57.5 | 13.5 / 8.1 / 47.1 | 13.5 / 8.1 / 47.1 |
| pre | sky130 | ff+min | 3.6 / 3.8 / 7.5 | 2.5 / 2.1 / 7.3 | 2.8 / 2.1 / 6.2 | 2.8 / 2.1 / 6.2 |
| pre | gf180 | tt+nom | 10.2 / 10.2 / 28.6 | 3.0 / 3.0 / 34.7 | 5.2 / 5.2 / 41.9 | 5.2 / 5.2 / 41.9 |
| pre | gf180 | ss+max | 24.1 / 24.1 / 80.1 | 10.6 / 10.6 / 54.7 | 6.1 / 6.1 / 52.2 | 10.1 / 10.1 / 48.2 |
| pre | gf180 | ff+min | 4.9 / 4.9 / 9.6 | 1.2 / 1.2 / 13.1 | 3.0 / 3.0 / 16.7 | 3.0 / 3.0 / 16.7 |
| hold | sky130 | tt+nom | 11.2 / 11.5 / 14.7 | 7.4 / 8.1 / 12.0 | 5.1 / 5.4 / 9.1 | 5.1 / 5.4 / 9.1 |
| hold | sky130 | ss+max | 21.1 / 21.5 / 48.6 | 12.2 / 13.2 / 46.0 | 8.9 / 9.1 / 37.9 | 8.9 / 9.1 / 37.9 |
| hold | sky130 | ff+min | 7.3 / 7.5 / 4.7 | 4.9 / 5.3 / 5.9 | 3.0 / 3.4 / 8.0 | 3.0 / 3.4 / 8.0 |
| hold | gf180 | tt+nom | 18.1 / 18.8 / 19.7 | 7.8 / 8.5 / 18.2 | 4.0 / 3.9 / 26.6 | 4.0 / 3.9 / 26.6 |
| hold | gf180 | ss+max | 37.8 / 39.4 / 34.9 | 17.4 / 19.0 / 46.5 | 10.3 / 11.5 / 29.6 | 9.5 / 7.1 / 27.9 |
| hold | gf180 | ff+min | 9.0 / 9.4 / 7.5 | 3.2 / 3.6 / 5.8 | 2.1 / 1.9 / 9.1 | 2.1 / 1.9 / 9.1 |

The GF180 max via resistance is the only change between `round2_data` and
`final`. It moves GF180 ss+max by up to 4.4 ps, in opposite directions on
the two designs (pre 6.1 -> 10.1 ps; hold 10.3 -> 9.5 ps against flow and
11.5 -> 7.1 ps against lossless OpenRCX): with the RC-driver model of
RustSTA `09bf9f85` per-net R matters at ss+max (decomposition with the
campaign's back-annotation, `magic_rc.backannotate`, on the native trees:
OpenRCX's per-net R alone moves hold GF180 ss by 7.4 ps with the 15 ohm vias
and 2.1 ps with 4.5 ohm), and on the pre-hold design the old via resistance
had cancelled a capacitance difference (OpenRCX's per-net C alone: 10.1 ps
either way). The data decision is made from the PDK files, not from this
table.

### Where the remaining deltas come from

Per-net attribution of each missed comparison (one net at a time set to the
oracle's per-net totals on the native trees; the moves add up to the measured
delta within 0.1 ps):

| design | PDK | corner | vs | endpoint | measured | largest moves: net (ps; native - oracle C fF; position against the two oracles) |
|---|---|---|---|---|---|---|
| pre | gf180 | ss+max | OpenRCX (both configurations) | r2/D setup | -10.05 ps | q[0] (+6.4; +0.19; between), n4 (+5.2; +0.29; between), rr_clock_1 (-5.0; +0.14; between), rr_clock_0 (+3.4; +0.10; between) |
| pre | sky130 | ss+max | OpenRCX flow | r2/D setup | -13.5 ps | n4 (+9.5; +0.49; between), q[0] (+4.3; +0.42; between) |

Every net behind a miss lies between the two oracles. The structures behind
them are identified:

* **Sky130 `n4`, `q[0]`** (over-the-cell met1/met2 nets): native counts the
  cells' pins and obstructions next to and under them (`n4`-`b[1]` pin
  coupling 0.241 fF, OBS 0.12 fF at max), which OpenRCX does not extract at
  all (pin and OBS controls). Magic, which sees the full cell layout, gives
  more still (`n4` 3.09 fF, native 2.08, OpenRCX 1.59 / 1.72 lossless). The
  flow configuration additionally drops 0.137 fF of `n4`'s couplings.
* **GF180 `q[0]`** is a 15.4 um Metal3 run next to `q[1]` at minimum space,
  crossing Metal2 wires. For that structure (canonical "wire with a
  minimum-space neighbour on one side over 2 um-pitch crossings") native adds
  506 aF, Magic 522 aF and OpenRCX 357 aF, and for the sandwiched variant
  OpenRCX violates monotonicity (above). The 3-D solve shows that close
  neighbours reduce crossing loading below the two-cut value (next section),
  so neither 2.5-D number is exact; native is within 3 % of Magic.

## Crossings in three dimensions

The calibration tables are two-dimensional; every extractor here builds a
crossing from 2-D cross-sections (native: both wires' cuts; OpenRCX and Magic:
their own 2.5-D rules). `scripts/field_solver.py` solves the 3-D Laplace
problem of an upper wire crossed by perpendicular lower wires at a given pitch,
optionally with same-layer neighbours of the upper wire, on the PDK's own
process stack (Sky130 KLayout cross-section, GF180 KLayout 2.5-D file) in a
homogeneous dielectric, and compares the upper wire's increment per crossing
with the native two-cut construction built from the same solver's 2-D values
(`docs/evidence/rc-field-solver.json`). In a homogeneous dielectric every
capacitance scales with the permittivity, so the ratio does not depend on it;
the real stack's layered dielectrics would shift it, which is why the ratio is
reported and not applied.

**sky130 met2 over met1** (two-cut increment without neighbours 4.25 aF at eps_r = 1)

| crossing pitch (um) | no neighbours | neighbours at 0.28 um | neighbours at 1 um | neighbours at 2 um | neighbours at 3 um |
|---|---|---|---|---|---|
| 0.28 | 1.04 | 0.26 | 0.76 | 0.93 | 0.97 |
| 1 | 2.75 | 0.55 | 1.74 | 2.31 | 2.49 |
| 2 | 3.78 | 0.65 | 2.08 | 2.94 | 3.28 |
| 4 | 4.42 | 0.69 | 2.18 | 3.16 | 3.62 |
| 8 | 4.65 | 0.69 | 2.16 | 3.18 | 3.67 |

**sky130 met3 over met2** (two-cut increment without neighbours 5.98 aF at eps_r = 1)

| crossing pitch (um) | no neighbours | neighbours at 0.6 um | neighbours at 1 um | neighbours at 2 um | neighbours at 3 um |
|---|---|---|---|---|---|
| 0.28 | 0.90 | 0.26 | 0.44 | 0.66 | 0.75 |
| 1 | 2.61 | 0.62 | 1.08 | 1.75 | 2.05 |
| 2 | 3.89 | 0.78 | 1.36 | 2.33 | 2.84 |
| 4 | 4.89 | 0.85 | 1.47 | 2.58 | 3.23 |
| 8 | 5.34 | 0.87 | 1.47 | 2.60 | 3.30 |

**gf180 Metal2 over Metal1** (two-cut increment without neighbours 4.70 aF at eps_r = 1)

| crossing pitch (um) | no neighbours | neighbours at 0.56 um | neighbours at 1 um | neighbours at 2 um | neighbours at 3 um |
|---|---|---|---|---|---|
| 0.46 | 1.02 | 0.28 | 0.52 | 0.78 | 0.88 |
| 1 | 1.91 | 0.49 | 0.90 | 1.41 | 1.61 |
| 2 | 2.82 | 0.64 | 1.18 | 1.92 | 2.26 |
| 4 | 3.37 | 0.71 | 1.27 | 2.11 | 2.54 |
| 8 | 3.53 | 0.74 | 1.27 | 2.12 | 2.57 |

**gf180 Metal3 over Metal2** (two-cut increment without neighbours 6.77 aF at eps_r = 1)

| crossing pitch (um) | no neighbours | neighbours at 0.56 um | neighbours at 1 um | neighbours at 2 um | neighbours at 3 um |
|---|---|---|---|---|---|
| 0.56 | 1.11 | 0.28 | 0.51 | 0.79 | 0.90 |
| 1 | 1.82 | 0.43 | 0.78 | 1.24 | 1.43 |
| 2 | 2.93 | 0.61 | 1.10 | 1.80 | 2.15 |
| 4 | 3.94 | 0.72 | 1.27 | 2.12 | 2.61 |
| 8 | 4.47 | 0.78 | 1.32 | 2.18 | 2.71 |

Grid convergence (Sky130 met2 over met1, 2 um pitch, no neighbours): coarse 16.20 aF (35190 nodes), default 16.05 aF (60690 nodes), fine 15.99 aF (105000 nodes), larger domain 15.36 aF (78120 nodes)

* For dense crossings (minimum pitch, no neighbours) the two-cut construction
  agrees with 3-D within 10 %.
* Sparse crossings (1-8 um pitch) load the upper wire 1.8-5.3x more than the
  two-cut model says:
  a grounded line 0.27-0.6 um below a wire intercepts field that would reach
  the substrate 2-3 um below over a length set by that height, not by the
  line's width.
* Close same-layer neighbours absorb that field: with minimum-space
  neighbours the 3-D increment is below the two-cut value (factor down to
  0.26 for dense crossings).

So the narrow-crossing loading is an unmodeled native structure, and the
oracles bound it only loosely. For an isolated crossing both oracles are above
native (Sky130 met2/met1 upper wire: OpenRCX 25.7 aF, Magic 25.3, native 11.2),
in the direction of the 3-D result, and the 3-D factor (3.8-4.4 at 2-4 um
pitch on the two-cut value) puts it above all three. Next to close neighbours
the 3-D result is below the two-cut value, and the oracles split: on GF180
OpenRCX is below native (and violates monotonicity when sandwiched), on Sky130
it is above; Magic is above native on both. Applying the 3-D factors to the
native two-cut increments (a scratch probe on routed-wire crossings of the two
studied Sky130 layer pairs, by local crossing pitch and neighbour distance)
moved native further from flow OpenRCX on all four Sky130 designs at ss+max
(pre 13.5 -> 19.7 ps, hold 8.9 -> 13.1, regfile 67 -> 93, mult 53 -> 61), so a
3-D crossing term is not adopted: it needs the full 3-D environment (cell
metal below, irregular pitches, finite wires) that the factors above do not
contain, and an oracle that models it.

## Larger designs: regfile and mult (scale stream)

`regfile` (426 nets) and `mult` (440 nets), routed by the scale stream's track
router into schema-6 workspaces (signoff DRC/LVS/CEC clean on both PDKs in
that stream's `s5` run), were measured with this branch merged with
`feat/rtl2gds-scale` `b363d0f` in a scratch clone (binary `d3726924…`; the
extractor code is identical to this branch's; nothing of the merge is
committed). Netlists come from `export-verilog`; the SDC has the pipeline
clock (10 ns) with 0.2 ns input/output delays on every data port. RustSTA
`09bf9f85`. Evidence: `docs/evidence/extraction-oracles-regfile.json`,
`extraction-oracles-mult.json` (compact: no per-net or per-endpoint tables).

| design | PDK | corner | flow OpenRCX | lossless OpenRCX | OpenRCX 0.1 vs 0 | Magic | OpenRCX vs Magic | fidelity | relative (lossless) |
|---|---|---|---|---|---|---|---|---|---|
| regfile | sky130 | tt+nom | 35.0 | 20.7 | 14.3 | 46.8 | 80.1 | 3.21 | 1.41 % |
| regfile | sky130 | ss+max | 66.9 | 42.5 | 24.3 | 211.3 | 277.8 | 17.18 | 1.45 % |
| regfile | sky130 | ff+min | 19.4 | 12.3 | 8.8 | 18.7 | 38.7 | 5.78 | 5.89 % |
| regfile | gf180 | tt+nom | 23.0 | 16.4 | 7.5 | 132.7 | 122.7 | 0.34 | 0.32 % |
| regfile | gf180 | ss+max | 38.8 | 26.9 | 15.7 | 285.2 | 302.6 | 4.33 | 0.26 % |
| regfile | gf180 | ff+min | 15.0 | 11.2 | 4.1 | 55.4 | 48.8 | 0.79 | 0.38 % |
| mult | sky130 | tt+nom | 25.4 | 22.0 | 7.9 | 32.6 | 56.9 | 0.93 | 2.53 % |
| mult | sky130 | ss+max | 52.8 | 40.8 | 14.3 | 170.1 | 188.6 | 5.34 | 2.47 % |
| mult | sky130 | ff+min | 15.5 | 12.3 | 4.9 | 16.4 | 30.5 | 4.12 | 1.91 % |
| mult | gf180 | tt+nom | 40.5 | 44.0 | 11.6 | 124.0 | 128.3 | 0.18 | 0.86 % |
| mult | gf180 | ss+max | 100.2 | 107.0 | 24.1 | 280.4 | 292.9 | 11.57 | 1.05 % |
| mult | gf180 | ff+min | 18.2 | 20.5 | 8.4 | 56.8 | 58.4 | 0.40 | 0.69 % |

(max of the setup and hold delta, ps; "relative" is the lossless delta at the
worst endpoint divided by that endpoint's setup arrival.)

* The target is missed at every corner against both oracles on both designs.
  The deltas are 0.3-2.5 % of the path arrival at the worst endpoint (5.9 %
  on one short reset path): these designs have paths of 1.5-16 ns, and per-net
  differences of a few percent add up along them.
* The OpenRCX configuration alone moves OpenRCX's slacks by up to 24 ps
  (regfile Sky130 ss+max, 0.1 fF against threshold 0), and OpenRCX and Magic
  disagree by up to 303 ps.
* Capacitance carries the deltas: on the native trees, OpenRCX's per-net C
  alone reproduces 93 / 98 ps of mult GF180 ss+max (94 / 100 ps measured, flow
  configuration); its per-net R alone moves 5.9 / 6.2 ps. On regfile the largest movers are clock-tree nets on which native
  lies between the oracles (Sky130 `rr_clock_3`: +2.1 fF over lossless
  OpenRCX, -4.1 fF under Magic at max). On mult GF180 the long input buses
  (30-110 fF) are 3-9 % below OpenRCX: long, crossing-rich nets, where
  OpenRCX's larger isolated-crossing increments (and, per the 3-D solve, the
  physics) give more than the native two-cut construction; the 3-D probe did
  not bring native closer, because the same factors raise the over-the-cell
  nets that are already above OpenRCX.
* Per-net C: regfile within +/-20 % of OpenRCX except one Sky130 net each at
  nom (+20.1 %) and max (+21.6 %); mult Sky130 -25.9..+17.2 % (fails), mult
  GF180 within -16.5..+11.8 %. The GF180 total-C metric misses (-10..-14 %)
  while the per-net sum is within -0.8..+3.3 %: OpenRCX writes each crossing
  as two ground increments, which the design total counts twice. On Sky130
  the per-net sum is +7..+9 %: the cell pins, obstructions and via pads that
  OpenRCX does not extract (a scratch run of the extractor without them gave
  -4.5..-6.5 % on regfile Sky130).
* The back-annotation fidelity control fails on these designs with RustSTA
  `09bf9f85` (up to 17 ps), so the Magic deltas are bounded only to that.
* The cell-pin control on these designs: shrinking every signal pin lowers no
  net's C by more than 0.06 fF but raises 38-250 nets (up to 3.7 fF) and
  changes R, because OpenRCX merges a net's own wire metal over its own pin
  into the pin; so pin metal is not a partner of any other net, as on
  pipeline.v, where the result is bit-identical.

## RustSTA version dependence

The program's RustSTA pin moved from `c407c742` to `09bf9f85` (stream sta3:
RC driver resistance and lumped pi). Both are measured on pipeline.v (table
above): against flow OpenRCX they differ by at most 0.2 ps, against lossless
OpenRCX by up to 2.0 ps (hold GF180 ss+max: 7.1 ps with `09bf9f85`, 9.1 ps
with `c407c742`), against Magic by up to 4.2 ps. Two consequences:

* per-net R now matters at ss+max (ablation above: the GF180 via resistance
  choice moves GF180 ss+max by up to 4.4 ps; OpenRCX's per-net R alone moves
  hold GF180 ss by 7.4 ps with the 15 ohm vias);
* the back-annotation fidelity control (OpenRCX's per-net totals scaled onto
  the native trees against OpenRCX's own SPEF, bound 1 ps) passes on
  pipeline.v with `c407c742` but fails with `09bf9f85` at hold GF180 ss+max
  (4.44 / 4.29 ps); on regfile and mult it fails with either pin (up to
  17 ps), because the driver model sees the
  distribution of R and C along a net, which one scale factor per net cannot
  reproduce. The campaign status is therefore `failed` for hold with
  `09bf9f85` (a control failed); the Magic comparisons there are bounded only
  to the fidelity value (Magic deltas are 28-50 ps, far larger).

## Resistance against Magic, element by element

Magic uses the same Magic sheet and contact values as the native profile except
where the profile falls back to nominal values (GF180 min and max contacts), so
for resistance it checks geometry and topology. Its network starts at the
terminal square, so a terminal on a pin via adds the via's contact plus half a
square of the pin layer (Sky130 li1: 15.70 ohm at nom against the native 9.3
ohm cut; 6.85 / 31.5 at min / max). With that element taken out on both sides,
every Sky130 net agrees within 2.0 % at every corner except `a[1]`, where a
routing patch bridges two met1 regions: Magic conducts through it, the native
tree does not (patches add capacitance only). GF180 agrees within 1.3 % at nom;
at min and max Magic's contacts are its placeholder blocks (0 and 15 ohm) while
the native profile keeps 4.5 ohm, so the GF180 min/max R comparison against
Magic fails by those values (-30.9..-66.7 % at max).

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
geometry through both oracles and the native extractor itself. Nominal corner,
40 um wires at default width:

| PDK | structure (40 um wires, nom) | native | OpenRCX | Magic |
|---|---|---|---|---|
| sky130 | isolated wire, total aF | 3519.4 | 3507.2 | 3140.3 |
| sky130 | pair at minimum space, coupling aF | 4194.1 | 4194.1 | 4561.4 |
| sky130 | crossing, upper wire increment aF | 11.2 | 25.7 | 25.3 |
| sky130 | crossing, lower wire increment aF | 23.5 | 19.5 | 33.9 |
| sky130 | crossing, coupling aF (Magic only; OpenRCX grounds it) | 23.5 | 0.0 | 33.9 |
| sky130 | crossing a grounded rail, increment aF | 24.1 | 25.7 | 55.0 |
| sky130 | crossing a 3x rail, increment aF | 60.5 | 64.4 | 138.9 |
| sky130 | crossing a 5 um plane, increment aF | 195.5 | 206.2 | 450.0 |
| sky130 | upper-layer wire along a rail, increment aF | 272.9 | 649.4 | 408.9 |
| sky130 | upper-layer wire along a 3x rail, increment aF | 654.2 | 649.4 | 1041.4 |
| sky130 | over 2 um-pitch crossings, increment aF | 249.5 | 525.8 | 654.5 |
| sky130 | over minimum-pitch crossings, increment aF | 1527.6 | 1733.6 | 2420.1 |
| sky130 | over 2 um-pitch crossings with a minimum-space neighbour on one side, increment aF | 218.9 | 391.8 | 598.3 |
| sky130 | over 2 um-pitch crossings with minimum-space neighbours on both sides, increment aF | 188.2 | 257.7 | 542.1 |
| sky130 | stacked aligned, coupling aF | 752.2 | 3112.6 | 752.2 |
| sky130 | stacked aligned, lower wire total aF | 4509.3 | 6856.6 | 4165.4 |
| sky130 | stacked offset, coupling aF | 117.8 | 1644.7 | 271.7 |
| gf180 | isolated wire, total aF | 3330.7 | 3307.7 | 2870.5 |
| gf180 | pair at minimum space, coupling aF | 5108.6 | 5108.6 | 3551.9 |
| gf180 | crossing, upper wire increment aF | 26.2 | 31.7 | 28.5 |
| gf180 | crossing, lower wire increment aF | 45.1 | 26.3 | 41.0 |
| gf180 | crossing, coupling aF (Magic only; OpenRCX grounds it) | 45.1 | 0.0 | 41.0 |
| gf180 | crossing a grounded rail, increment aF | 47.1 | 63.4 | 47.5 |
| gf180 | crossing a 3x rail, increment aF | 115.0 | 126.8 | 108.9 |
| gf180 | crossing a 5 um plane, increment aF | 296.2 | 317.0 | 272.6 |
| gf180 | upper-layer wire along a rail, increment aF | 121.1 | 957.2 | 192.2 |
| gf180 | upper-layer wire along a 3x rail, increment aF | 963.9 | 957.2 | 446.4 |
| gf180 | over 2 um-pitch crossings, increment aF | 691.7 | 892.3 | 621.7 |
| gf180 | over minimum-pitch crossings, increment aF | 1656.6 | 2197.8 | 1551.7 |
| gf180 | over 2 um-pitch crossings with a minimum-space neighbour on one side, increment aF | 506.2 | 356.9 | 522.0 |
| gf180 | over 2 um-pitch crossings with minimum-space neighbours on both sides, increment aF | 320.8 | -178.4 | 422.2 |
| gf180 | stacked aligned, coupling aF | 546.2 | 2051.9 | 594.9 |
| gf180 | stacked aligned, lower wire total aF | 4406.2 | 5890.0 | 4056.8 |
| gf180 | stacked offset, coupling aF | 69.2 | 972.8 | 160.8 |

| PDK | extractor | structures where adding conductors lowered the studied wire's total |
|---|---|---|
| sky130 | openrcx | none |
| sky130 | magic | none |
| sky130 | native | none |
| gf180 | openrcx | top_sandwiched_over_sparse_crossings (-178.4 aF) |
| gf180 | magic | none |
| gf180 | native | none |

| PDK | OpenRCX: wire split at its middle, aF | via at that point, aF | via stack, aF |
|---|---|---|---|
| sky130 | +13.06 | +0.00 | +0.00 |
| gf180 | +21.94 | +0.00 | +0.00 |

* Isolated wires and pairs: native and OpenRCX agree within 0.7 % (same table,
  same geometry). Magic differs by its data.
* Crossings over passive metal (rails, planes): native lies between the
  oracles or within 7 % of the nearer one.
* Wire along a slightly wider rail and stacked or offset wires: the
  overlap-area term gives Magic's area value and leaves out the edge fringe;
  OpenRCX makes every such overlap a full plate.
* **The narrow crossing is the structure where native differs most.** Upper
  wire: native 11.2 aF, OpenRCX 25.7, Magic 25.3 (Sky130); 26.2 / 31.7 / 28.5
  (GF180). Total over both wires: native 34.7 aF, OpenRCX 45.2, Magic 59.2
  (Sky130), below both. This is the unmodeled 3-D crossing loading of the
  previous section: native's two-cut construction is exact only for dense
  crossings, and the 3-D increment of an isolated crossing is above all three
  extractors. The oracles agree with each other here by different routes
  (OpenRCX's increment does not scale with the lower width; Magic's Sky130
  inter-layer data are 2.3x the table's), and they disagree once neighbours
  are present (neighbour on one side: native 219 / 506 aF, OpenRCX 392 /
  357 aF, Magic 598 / 522 aF; sandwiched: native 188 / 321, OpenRCX 258 /
  -178, Magic 542 / 422; Sky130 / GF180).

## Acceptance status (round 3)

* **Slack delta <= 10 ps against a geometry-identical oracle at every corner.**
  Neither oracle is fully geometry-identical: OpenRCX extracts the same routed
  wires and supply metal but none of the cell pins, cell obstructions and via
  pads (controls above), and Magic extracts the real cell layouts with its own
  data. Against OpenRCX:
  * hold-closed (signoff) pipeline.v: met at all six PDK x corner
    combinations, both OpenRCX configurations, both RustSTA pins (worst 9.5 /
    9.1 ps flow / lossless with `09bf9f85`, 9.7 / 9.2 ps with `c407c742`);
  * pre-hold pipeline.v: met at 11 of 12 against lossless OpenRCX; GF180
    ss+max is 10.05 ps (q[0], n4, clock nets; all between the oracles). Against
    the flow configuration Sky130 ss+max misses too (13.5 ps; the flow and the
    lossless OpenRCX configurations differ from each other by 7.3 ps there);
  * regfile and mult (deep paths, 426-440 nets): missed at every corner (see
    "Larger designs").
  **Not met as stated** for the pre-hold design and the larger designs.
* **Explained bound against the other oracle.** Against Magic the pipeline.v
  deltas are 28-48 ps at ss+max, the same size as the OpenRCX-to-Magic
  disagreement there (34-61 ps); every net behind a missed comparison lies
  between the oracles. The differences are identified: data (Magic's Sky130
  inter-layer values 2.3x the table's; the tables' GF180 isolated-wire values
  5-27 % above Magic's), cell geometry
  (OpenRCX blind to pins, OBS and via pads; Magic sees full layouts), the
  coupling threshold (up to 9.0 ps of OpenRCX against itself), OpenRCX's
  monotonicity violation and full-plate stacking, and the 3-D crossing loading
  that no extractor here models (field solver).
* **Per-net C targets** against OpenRCX are met at every corner on both
  pipeline.v designs (flow and lossless); total C too.
* **Resistance** discrepancies are explained per PDK data source (Sky130
  `calibre` RESOVER rows, OpenRCX's corner-invariant LEF vias, GF180's
  placeholder Magic corner contacts) and, against Magic, element by element.
* **Storage independence** is unchanged from round 2 (unit test; schema-6
  compact segments extract).

What would close the remaining misses is a choice for the owner, not a
coefficient: either restate the slack target for the signoff (hold-closed)
design, which meets it, or accept a 3-D crossing model (and an oracle that
models 3-D crossings to check it), which moves native away from both 2.5-D
oracles. Dropping cell pins, obstructions and via pads or adopting OpenRCX's
crossing treatment would match OpenRCX without being geometry-identical.

## History

* Round 2 (`66411bd`, binary `6c5f267f`): passive-conductor cuts, non-plate
  overlap area, storage-independent run splitting. Against flow OpenRCX with
  RustSTA `c407c742` 10 of 12 (misses: pre Sky130 ss+max 13.6 ps, pre GF180
  ss+max 10.3 ps); with `09bf9f85` 10 of 12 (pre Sky130 13.5, hold GF180
  10.3 ps).
* Round 1 (`0851d03`, binary `de7428b9`): via pads, end extensions, covered
  sides, crossings from both wires' cross-sections, plates must reach one strip
  width beyond both edges, Magic oracle. 9 of 12.
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
* Crossings are 2.5-D (two-cut). The 3-D loading of sparse crossings (2-5x the
  two-cut increment) and its reduction next to close neighbours are not
  modeled (field-solver evidence).
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
  patches add capacitance but do not conduct (Magic: `a[1]`, about 9 ohm at
  nom on Sky130).
* GF180 via resistance has no corner data in the PDK; nominal is used at min
  and max.
* Magic timing is a back-annotation of Magic's per-net totals onto the native
  trees; with RustSTA `09bf9f85` it is accurate only to the fidelity control
  (up to 4.4 ps on pipeline.v GF180 ss+max).
* The field solver uses a homogeneous dielectric and idealised environments
  (infinite wires, periodic crossings); its factors are evidence, not model
  data.
* Serial `cpu_ref` only. There is no GPU path and none is claimed.
