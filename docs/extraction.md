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
end extensions and the via landing pads. Via cut capacitance, diagonal coupling
and the overlap coupling of parallel wires on other layers that do not form a
plate are not modeled (see "Limitations"). SI, IR/EM, OCV and CPPR remain
`unchecked`. DRC/LVS status is `unchecked`.

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
  `coupling: cross_net_spef` plus the reason, and the unmodeled items); `stats`
  (nets, lattice wires, wire runs, vias, nodes, resistors, opened loops,
  couplings, shapes, intervals, work, context fallbacks, foreign overlaps,
  wire length, ground/coupling pF, total ohm); unannotated signal nets; and
  diagnostics.

Repeated runs are byte-identical. The SPEF `*DATE` is a fixed string, and the
JSON report carries provenance.

## RC profile (`rustroad.rc-profile.v1`)

`scripts/build_rc_profile.py` writes `fixtures/pdk/{sky130,gf180}-rc-{min,nom,max}.json`.
`--check` fails if a tracked profile differs from a rebuild. It reads three PDK
data files and records each one's path, sha256 and size in `sources`:

| role | Sky130 | GF180 |
|---|---|---|
| capacitance calibration table | `libs.tech/librelane/rules.openrcx.sky130A.<c>.calibre` | `libs.tech/librelane/rules.openrcx.gf180mcuD.<c>` |
| resistance and cross-check | `libs.tech/magic/sky130A.tech` (extract section) | `libs.tech/magic/gf180mcuD.tech` |
| layer order and widths | `sky130_fd_sc_hd__nom.tlef` | `gf180mcu_fd_sc_mcu7t5v0__nom.tlef` |

These are the same calibration-table variants the existing harness used
(`fixtures/pdk/local-routing.json`). Corners map as follows:

| profile | calibration table | Magic variant | used with Liberty |
|---|---|---|---|
| `min` | `*.min` | `(lrlc)`: low R, low C | ff (hold) |
| `nom` | `*.nom` | `()`: nominal | tt |
| `max` | `*.max` | `(hrhc)`: high R, high C | ss (setup and hold) |

Schema:

* `layers[]`: `name`, `rules_metal` (the 1-based calibration metal index,
  checked against the LEF routing-layer order and `LayerCount`), `width_um`
  (checked against LEF `WIDTH`), `sheet_resistance_ohm_per_square` (from the
  Magic `resist` line of the corner variant), and `contexts[]`. Each context is
  `{over, under, rows}`: `over` is the calibration index of the nearest
  conductor below (0 = substrate), and `under` is the index of the nearest
  conductor above or null. Each row is
  `[edge_spacing_um, coupling_pf_per_um, ground_pf_per_um_per_side]`.
* `cuts[]`: `name`, `lower`, `upper`, `resistance_ohm_per_cut` and `source`
  (the Magic `contact` value). The cut-to-contact mapping is checked against
  the Magic `contact`/`types` sections.
* Also recorded: `corner`, `pdk`, `library`, `pdk_version`,
  `technology_lef_sha256`, `units` (um/pF/ohm), `magic_variant`,
  `lef_parasitics`, `model` (text), `cross_checks`, `diagnostics` and
  `generator`.

Data decisions:

* **Table rows are per side.** The isolated row read as "per side" (isolated
  total = 2 x last-row ground) agrees with the Magic area + perimeter model
  within -11 %..+28 % on all layers, corners and both PDKs. Read as "total",
  the disagreement would be -36 %..-56 %. Coupling at minimum spacing agrees
  with the Magic sidewall model within 8 % on every Sky130 layer at nominal.
  GF180 differs more (-40 %..+54 %); that is a disagreement between two PDK
  data sources and is recorded, not resolved. This is a data-to-data check,
  recorded in `cross_checks`.
* **Resistance is the foundry sheet resistance.** The fourth column of each
  calibration row is not used. Its ratio to the sheet resistance differs per
  layer (2200 for li1, 1443 for met1), so its unit cannot be established from
  data alone. Sky130 LEF `RPERSQ` agrees with the Magic nominal values and is
  recorded as a cross-check.
* GF180's technology LEF states that its parasitic values are not correct. They
  are never read (`lef_parasitics: placeholder_excluded`).
* The GF180 Magic `(lrlc)` block lists every contact resistance, diffusion
  contacts included, as 0, which means no data. The min profile therefore uses
  the nominal 4.5 ohm and states this in `diagnostics`
  (`VIA_R_CORNER_UNAVAILABLE`).
* `DIAGUNDER` tables and the `RESOVER` tables are not used.
* Which data source each quantity follows, and how far the PDK's sources
  disagree, is measured in "Data sources" below
  (`docs/evidence/rc-data-sources.json`).

## Physical model

Each routed net is built from its stored lattice records: wires, fixed vias and
pin-access points. Records may be lattice edges or longer collinear segments;
the graph, the runs and the extracted network are the same either way (unit
test `compact_segments_extract_like_lattice_edges`).

1. **Graph.** Nodes are `(layer, x, y)` in integer DBU. Wires and vias are the
   edges. A node is a junction unless it is the interior point of a straight
   run on one layer: degree 2, collinear, no via and no pin. Each maximal
   straight run between junctions becomes one resistor. Runs longer than
   `--max-segment-um` are split at existing lattice nodes. Coincident pins get
   a 0-ohm link; the timing engine contracts zero-ohm edges.
2. **Tree.** A breadth-first walk starts at the single driver, an output pin or
   an input port. Neighbours are visited in node-key order. A conductor that
   closes a loop keeps its capacitance but not its resistance, and is counted
   in `opened_loop_resistors` with an `RC_LOOP_OPENED` diagnostic; the timing
   engine rejects meshes. Disconnected nodes are an error.
3. **Resistance.** A run's resistance is `sheet * centerline_length / width`.
   A via's resistance is the per-cut value divided by the number of cuts. The
   centreline already runs to the via centre, so pads and end extensions add
   no resistance.
4. **Metal.** The routed metal of a net is tiled without overlap:
   * the **body** of each run: its centreline extent times the width;
   * **pieces**: the half-width end extension at both ends of every run (DEF
     default extension; the GDS and DEF writers draw the same), then every via
     landing pad (the LEF via geometry on routing layers), then every routing
     patch. Each rectangle minus the same-net metal already present on its
     layer (bodies, the net's own pins, earlier pieces) is cut into disjoint
     rectangles by exact integer subtraction (at most 256 per rectangle). An
     end extension attaches at its run's end node, a pad at its via's node, a
     patch at the node of the same-net conductor it overlaps most (step 6).

   The union of bodies and pieces is exactly the routed metal (the L corner and
   via-pad unit tests check the exact union perimeter). Pieces add capacitance
   only, never a node or a resistor. Each piece has two side strips, one along
   x and one along y, so all four of its edges take the per-side model below.
5. **Capacitance.** Every strip (a body or one side strip of a piece) is cut
   into intervals along its axis at the extents of nearby shapes: other bodies
   and pieces, instance pins, port pins, obstructions and supply metal on
   routing layers. For each interval and each of the two sides:
   * **Exposure.** A side touching same-net metal, or lying inside it (a
     same-net shape spans the edge, for example an inner corner or the net's
     own pin), is not exposed and carries no capacitance
     (`covered_side_intervals`).
   * **Lateral.** The nearest same-layer shape within the table range gives
     the row at that edge spacing (linear interpolation, clamped to the first
     row): coupling `cc` and ground `cg` per side. With no neighbour, the side
     takes the isolated last row. A same-net neighbour shields the side (`cg`
     only).
   * **Plates.** The calibration contexts describe a plate. A conductor on
     another layer is the plate below (above) only where the metal of its
     layer covers the strip and reaches at least one strip width beyond both
     side edges: crossings and clearly wider shapes. The nearest such layer
     below and above selects the context `(over, under)`; the conductor under
     each side edge owns that side's plate share. A missing exact context
     falls back to `(over, -)`, then `(0, under)`, then `(0, -)`, and is
     counted. Narrower, offset or barely wider metal on other layers is not a
     plate; it is counted in `nonspanning_plane_intervals` and its overlap
     coupling is not modeled.
   * **Split.** A plate below shields the substrate: the whole context value
     `cg` of the side goes to it. A plate above takes the excess over what
     goes down (the substrate share `cg_sub`, the OVER-0 row at the same
     spacing, or the OVER-only row when there is a plate below too). With no
     plate, `cg` is ground.
   * **Crossings count from both wires.** The upper wire's OVER cut covers the
     lower wire's width; the lower wire's UNDER cut covers the upper wire's
     width and adds its excess as coupling to the upper wire. These are
     different field regions (the 2.5D two-cut construction); the area under
     the overlap is in both, which overstates a crossing by about
     `area_cap * w_upper * w_lower`.
   * **Ownership of lateral pairs.** Every routed-net shape models its own
     exposed sides, so a lateral pair is counted half from each side. Pins,
     obstructions and supply metal do not model themselves and are counted
     fully from the routed side. A partner with no signal net (supply,
     obstruction, unconnected pin) is ground. A partner pin is attached at
     that pin's tree node, a piece at its node, and a body at its end nearest
     the interval. On the strip's own side, each value is split between its
     two end nodes by position (pi model).
6. **Routing patches.** `Routes.patches` (the router's same-net notch and
   corner fills, DEF routing `RECT`) are pieces of step 4 attached to the RC
   node of the same-net conductor they overlap or touch: largest overlap area
   first, then the lowest shape index; a body attaches at its end nearer the
   patch centre, a piece at its node, a pin at its pin node. A floating patch,
   a patch on a net without an RC tree and a patch off a routing layer are
   explicit errors. Patches do not conduct in the native tree (see
   "Limitations").
7. **Coupling output.** Cross-net capacitors are written as SPEF coupling caps,
   listed in both nets' `*CAP` sections. So each net's declared total equals
   its ground plus coupling, just as in the reference SPEF. RustSTA grounds
   coupling at factor 1, so for timing this equals a grounded model.

The JSON `stats` add `end_pieces`, `pad_pieces`, `covered_side_intervals` and
`nonspanning_plane_intervals`; `scope` states `wire_ends`, `metal_union`,
`via_capacitance`, `crossing_fringe` and `diagonal_coupling`.

## SPEF conventions

* Header: `*SPEF "IEEE 1481-1999"`, units `NS`, `PF`, `OHM`, `HENRY`, divider
  `/`, delimiter `:`, bus delimiter `[]`, `*DESIGN_FLOW "NAME_SCOPE LOCAL" "PIN_CAP NONE"`.
* `*NAME_MAP`: routed nets `*1..*N` in connectivity net order. Then come the
  instances, in order of first use.
* Nodes:
  * internal junctions are `*<net>:<k>`, with k = 1.. in tree order from the
    driver (the task's `net:N`);
  * instance pins are `*<inst>:<PIN>`;
  * ports are bare bit names (`a[0]`).
  Characters other than `[A-Za-z0-9_]` and the bus brackets are
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
* 8 M bin entries x 4 B, plus the map, < 128 MiB;
* 500 k nodes with keys and adjacency, about 64 MiB;
* 1 M couplings x about 64 B (BTreeMap) = 64 MiB;
* SPEF model strings < 64 MiB; the file is streamed through a 256 KiB buffer;
* the workspace read itself is under 16 MiB of `design.json`, about 200 MiB parsed.

That is about 0.7 GiB peak, under the 2 GiB per-test-binary limit. There are no
paths, cuts or cones. Each net is one linear walk, the neighbour search is
bounded by the window, `max_candidates` and `max_work`, and each piece
subtraction by `max_candidates` and 256 pieces.

Measured on the pipeline designs (`docs/evidence/extraction-oracles-*.json`):

| run | wall | peak RSS |
|---|---|---|
| native extraction, per corner | 0.015-0.020 s | 8.5-9.4 MiB |
| OpenRCX container, per corner | 0.32-0.45 s | 110-127 MiB |
| Magic capacitance run (hierarchical + flat), per corner | 0.46-0.87 s | 67-73 MiB |
| Magic resistance run (terminal layout), per corner | 0.26-0.36 s | 67-73 MiB |
| `rc_oracles.py campaign`, one design, both PDKs, with four ablation binaries (730-830 guarded processes) | 44-48 s | 136 MiB (largest child, RustSTA import) |
| `cargo build --release --bins` (4 jobs) | 10-12 s | 0.69 GiB |
| `cargo test --workspace` (4 threads) | 4.4 s | 0.57 GiB |
| ablation builds (three variants, 4 jobs each) | 50 s | 0.66 GiB |

No run exceeds 4 GiB, so no scale lock is needed. Builds and tests used
`CARGO_BUILD_JOBS=4`, `RUST_TEST_THREADS=4` and
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G`.

## Verification

* **Unit tests** (`rustroad-core::extract`, all values derived by hand from
  the test rows):
  * an analytic straight wire (10 ohm; ground 2 x 4e-5 x 10 um plus two end
    extensions of 0.2 um exposed edge each);
  * a parallel pair (coupling counted once over the bodies and the facing end
    extensions, per-side ground);
  * the exact union perimeter of an L corner (20.4 um; inner-corner sides are
    covered, the outer corner is one piece);
  * a via with 0.2 x 0.2 um pads: M1 union perimeter 10.7 um and M2 10.8 um,
    five M1 pad pieces, a neighbour 0.45 um from the pad edge couples once;
  * sides inside the net's own pin are not exposed;
  * a crossing counted from both cross-sections (upper OVER cut with
    substrate shielding, lower UNDER excess);
  * narrower parallel metal below is not a plate;
  * compact segments extract exactly like lattice edges;
  * patch beside a wire, overlapping patch, fully covered patch, patch facing
    a parallel net and a wire end, floating/unrouted/cut-layer patches;
  * a branch, via and mid-run pin tree with exact resistor values; run
    splitting and loop opening; budgets; profile-width mismatch.
* **Integration tests** (`crates/rustroad-workspace/tests/extraction.rs`, a
  routed 7-net fixture): byte-identical rerun; unchanged workspace tree; one
  D_NET and one tree per net; refusals that leave no output and no temporary
  file (output inside the workspace, corner mismatch, technology LEF mismatch,
  missing layer, byte cap, zero cap).
* **Python tests** (`scripts/tests/test_rc_oracles.py`): GDS record round trip
  and truncation refusal; Magic `.ext` parsing with alias resolution and
  record-order independence; instance mapping including flipped rows and a
  wrong corner; `.res.ext` blocks; back-annotation scaling; the target bounds.
* **Campaign self-checks and controls** (`scripts/rc_oracles.py`, both
  designs and both PDKs, all passed): native byte-identical rerun; read-only
  workspace; refusal of output inside the workspace and of a corner mismatch;
  independent resistance accounting from the route records (max relative
  error 3.0e-6); Magic determinism (two runs, exact per-net values);
  back-annotation fidelity (OpenRCX's per-net totals on the native trees time
  within 0.31 ps of OpenRCX's own SPEF at every corner); negative controls:
  x3 resistance profile (fails the R target against both oracles), dropped
  D_NET, renamed net, dropped `*I` connection (coverage fails; RustSTA
  rejects), planted open in the GDS (Magic harness refuses the split net).
* **Magic rule controls** (`scripts/magic_rc.py controls`,
  `docs/evidence/rc-magic-controls.json`): isolated met2/met1 (Sky130) and
  Metal2/Metal1 (GF180) wires extract to Magic's own
  `areacap * area + perimeter * perimc` within 1.4e-6, so the `.ext` parsing,
  units and style selection are right.

## Oracles

**OpenRCX** (pinned `openroad/orfs` image `f152917e`) reads the DEF the flow
writes (maximal collinear runs, geometry-identical to the native GDS), the PDK
technology and cell LEF and the calibration table of the corner, with
`-max_res 0 -coupling_threshold 0.1 -no_merge_via_res`. Its nominal SPEF equals
the flow's own OpenRCX SPEF net for net on both designs. It reads the same
calibration tables as the native profile, so for capacitance it checks the
geometric model; its resistance comes from the tables' `RESOVER` rows and the
LEF via resistances (see "Data sources").

**Magic** (pinned signoff image `4c48415d8bd0`, Magic 8.3.678) is independent
in both data and algorithm for capacitance (its own area, perimeter, sidewall
and overlap rules per corner variant) and sees the real cell layouts. The
harness (`scripts/magic_rc.py`):

* exports the native GDS, DEF, routes and connectivity of the workspace
  (read-only commands);
* capacitance: reads the GDS with the GDS via structures flattened on input
  (`gds flatglob VIA_*`; hierarchical via cells lose substrate capacitance in
  Magic's hierarchical adjustment), extracts the hierarchy once (instance
  transforms and a standalone extraction of every cell master) and then the
  flattened design. A net's routing capacitance is its flat node capacitance
  (substrate plus every coupling record) minus the standalone capacitance of
  its cell pin nodes, which Liberty already holds. Values are read from the
  raw `.ext` records with exactly rounded sums (`ext2spice` was not used:
  in flat mode it drops some substrate capacitance);
* resistance: extracts a routing-only terminal layout (cell instances
  removed, one port cell at every routed pin access point) with
  `extract do resistance` and zero `extresist` thresholds. A net's resistance
  is the sum of its network's resistors; terminals are matched by position
  (Magic sometimes names a terminal node after the net);
* maps every Magic node to a native net through the instance placement
  (transform against the DEF component corner) and the pin names, and refuses
  any open, short, merged or unmapped net;
* corner styles follow the profile's `magic_variant`: `ngspice(lrlc)`,
  `ngspice()`, `ngspice(hrhc)`.

Magic has no SPEF writer, so timing against Magic back-annotates its per-net
totals onto the native RC trees (every node capacitance and resistor of a net
scaled by one factor; couplings grounded at their own node, factor 1, as
RustSTA does). The fidelity control shows this is sound on these designs: the
same back-annotation of OpenRCX's totals reproduces OpenRCX's own timing
within 0.31 ps at every corner. Per-net totals carry the timing; resistance and
the distribution of capacitance along a net do not move slack measurably here.

## Three-way comparison (stream extract2, 2026-10-24)

Commands (final binary `de7428b9…`, RustSTA 0.4 `rusteda c8f71b63…`,
`ruststa c407c742…`):

```sh
python3 scripts/rc_oracles.py campaign --workspace RUN --base-rev empty --inputs CASES.json \
  --rustroad target/release/rustroad --sta-bin STA_BIN \
  --variant e0d074d=BASE_BIN --variant pads_ends=... --variant any_overlap_plates=... \
  --variant strict_plates=...
python3 scripts/rc_oracles.py evidence --summary RUN/summary.json --out docs/evidence/extraction-oracles-<design>.json
```

Designs (`pipeline.v`, policy producer, LEF-rule router):

* **pre**: the routed flow of `scripts/verify_routing.py` before hold closure
  (DEF byte-identical to the b0 requalification run);
  `docs/evidence/extraction-oracles-pre.json`.
* **hold**: the hold-closed final design of the hold stream (four delay cells
  on each PDK), read-only copies; `docs/evidence/extraction-oracles-hold.json`.

No larger routed design existed in `.work/wt/scale` (that stream had not
produced one), so none was measured.

The targets are the fixed ones (total C +/-10 %, per-net C +/-20 % for nets
>= 5 fF, per-net R +/-20 % for nets >= 20 ohm, endpoint slack delta <= 10 ps),
evaluated against each oracle. Campaign status is `partial` on both designs:
every control passes, not every target.

### Endpoint slack delta (max abs setup / hold, ps)

| design | PDK | corner | vs OpenRCX | vs Magic (back-annotated) | OpenRCX vs Magic |
|---|---|---|---|---|---|
| pre | sky130 | tt+nom | 1.6 / 4.2 **pass** | 12.8 / 6.2 | 14.1 / 6.4 |
| pre | sky130 | ss+max | 3.2 / 7.8 **pass** | 57.2 / 23.7 | 60.4 / 22.3 |
| pre | sky130 | ff+min | 0.9 / 2.5 **pass** | 7.3 / 3.7 **pass** | 8.2 / 6.1 |
| pre | gf180 | tt+nom | 3.0 / 2.0 **pass** | 34.7 / 29.6 | 37.7 / 31.5 |
| pre | gf180 | ss+max | 10.2 / 7.4 fail | 58.6 / 25.2 | 60.1 / 22.0 |
| pre | gf180 | ff+min | 1.2 / 0.6 **pass** | 15.2 / 12.9 | 16.0 / 13.3 |
| hold | sky130 | tt+nom | 7.4 / 6.3 **pass** | 12.0 / 4.9 | 5.8 / 5.8 |
| hold | sky130 | ss+max | 12.1 / 11.2 fail | 45.9 / 21.5 | 33.7 / 21.3 |
| hold | sky130 | ff+min | 4.8 / 3.5 **pass** | 5.9 / 4.1 **pass** | 6.0 / 6.0 |
| hold | gf180 | tt+nom | 7.8 / 6.7 **pass** | 18.2 / 13.1 | 26.0 / 19.8 |
| hold | gf180 | ss+max | 21.1 / 18.3 fail | 48.1 / 32.6 | 36.1 / 27.5 |
| hold | gf180 | ff+min | 3.2 / 2.7 **pass** | 6.9 / 4.6 **pass** | 10.0 / 7.3 |

### Capacitance and resistance targets

| design | PDK | RC | total C vs OpenRCX | per-net C vs OpenRCX | total C vs Magic | per-net C vs Magic | per-net R vs OpenRCX | per-net R vs Magic |
|---|---|---|---|---|---|---|---|---|
| pre | sky130 | min | -2.0 % | 7, -4.2..+2.7 % pass | +10.7 % fail | 5, -2.0..+17.5 % pass | -74.9..-36.3 % fail | -65.7..-13.1 % fail |
| pre | sky130 | nom | -2.3 % | 9, -4.1..+2.6 % pass | +1.6 % | 10, -10.6..+7.7 % pass | -20.9..-2.3 % fail | -37.4..-5.5 % fail |
| pre | sky130 | max | -3.4 % | 10, -3.8..+3.6 % pass | -13.1 % fail | 10, -22.3..+1.0 % fail | +7.4..+137.8 % fail | -25.6..+11.5 % fail |
| pre | gf180 | min | -5.9 % | 12, -4.2..+0.8 % pass | +15.5 % fail | 12, +3.9..+16.2 % pass | +0.9..+13.5 % pass | +32.3..+110.1 % fail |
| pre | gf180 | nom | -6.8 % | 13, -5.0..+0.5 % pass | +18.0 % fail | 12, +5.3..+21.8 % fail | -2.5..-1.3 % pass | -1.2..0.0 % pass |
| pre | gf180 | max | -8.2 % | 13, -6.1..+0.2 % pass | +3.2 % | 14, -12.6..+11.4 % pass | +8.7..+178.3 % fail | -0.8..0.0 % pass |
| hold | sky130 | min | -4.3 % | 6, -12.9..+2.7 % pass | +8.9 % | 5, -0.7..+15.9 % pass | -75.4..-36.3 % fail | -66.4..-12.9 % fail |
| hold | sky130 | nom | -4.7 % | 8, -12.9..+2.5 % pass | -0.6 % | 12, -16.0..+4.2 % pass | -20.9..-2.7 % fail | -37.6..-5.5 % fail |
| hold | sky130 | max | -6.1 % | 11, -12.0..+1.6 % pass | -14.7 % fail | 15, -30.9..-5.7 % fail | +6.7..+134.2 % fail | -25.7..+11.5 % fail |
| hold | gf180 | min | -8.6 % | 11, -5.6..+0.7 % pass | +13.0 % fail | 11, -1.2..+14.4 % pass | +0.4..+13.2 % pass | +32.5..+110.1 % fail |
| hold | gf180 | nom | -9.7 % | 11, -6.7..+0.5 % pass | +14.8 % fail | 11, +1.3..+19.4 % pass | -3.6..-1.3 % pass | -1.2..+0.6 % pass |
| hold | gf180 | max | -11.3 % fail | 13, -8.1..+0.1 % pass | +3.1 % | 13, -14.7..+11.3 % pass | +8.0..+175.9 % fail | -0.8..+0.5 % pass |

Total C is `sum(ground) + sum(coupling) / 2`. OpenRCX grounds couplings below
its 0.1 fF threshold in both nets, so its total counts those twice; per-net
totals (what timing sees) are the fairer comparison and are within the
per-net target everywhere against OpenRCX. The one total-C miss (hold, GF180
max, -11.3 %) is on that metric.

### Ablation (max abs slack delta in ps; total C)

Each column is an extractor binary on the same workspaces and oracles:
`e0d074d` (before this stream), `pads_ends` (pads, end extensions and covered
sides only), `any_overlap_plates` (plus substrate shielding and both
cross-sections, plates as before: any overlap on the nearest layer),
`strict_plates` (plates must span the strip), `final` (plates must reach one
strip width beyond both edges). The patches are produced by
`scripts/extraction_ablation.py`.

| design | PDK | corner | e0d074d | pads_ends | any_overlap_plates | strict_plates | final |
|---|---|---|---|---|---|---|---|
| pre | sky130 | tt+nom | 6.4 / 12.0 | 4.3 / 9.8 | 11.1 / 8.2 | 5.7 / 11.3 | 4.1 / 12.8 |
| pre | sky130 | ss+max | 12.7 / 53.0 | 12.9 / 47.5 | 26.4 / 42.2 | 10.8 / 53.0 | 7.8 / 57.2 |
| pre | sky130 | ff+min | 3.6 / 7.5 | 2.3 / 5.9 | 6.2 / 4.5 | 3.3 / 6.6 | 2.5 / 7.3 |
| pre | gf180 | tt+nom | 10.1 / 28.6 | 6.5 / 36.9 | 9.4 / 28.3 | 8.3 / 29.4 | 3.0 / 34.7 |
| pre | gf180 | ss+max | 24.0 / 84.2 | 16.4 / 76.6 | 20.0 / 72.9 | 19.6 / 63.3 | 10.2 / 58.6 |
| pre | gf180 | ff+min | 4.9 / 11.6 | 3.0 / 16.3 | 4.9 / 11.1 | 4.0 / 12.0 | 1.2 / 15.2 |
| hold | sky130 | tt+nom | 11.3 / 14.3 | 7.2 / 10.2 | 6.4 / 9.0 | 7.3 / 11.4 | 7.4 / 12.0 |
| hold | sky130 | ss+max | 21.2 / 48.5 | 12.9 / 39.8 | 10.7 / 37.4 | 12.5 / 43.3 | 12.1 / 45.9 |
| hold | sky130 | ff+min | 7.3 / 4.7 | 4.4 / 8.4 | 4.0 / 6.6 | 4.4 / 7.1 | 4.8 / 5.9 |
| hold | gf180 | tt+nom | 18.1 / 19.5 | 9.5 / 24.0 | 13.9 / 33.0 | 10.3 / 17.1 | 7.8 / 18.2 |
| hold | gf180 | ss+max | 41.4 / 36.6 | 24.3 / 24.8 | 36.4 / 33.6 | 25.5 / 42.6 | 21.1 / 48.1 |
| hold | gf180 | ff+min | 9.0 / 7.8 | 4.2 / 10.3 | 7.1 / 14.8 | 4.8 / 6.7 | 3.2 / 6.9 |

(each cell: against OpenRCX / against Magic). Every rule was motivated by a
canonical structure measured against both oracles (next section), not by these
numbers, and no coefficient changed. The final model improves on `e0d074d`
against OpenRCX at every corner of both designs (largest: GF180 ss+max
24.0 -> 10.2 ps and 41.4 -> 21.1 ps). Against Magic it moves both ways,
because Magic and the calibration tables disagree on the data itself.

### Per-net attribution of the missed corners

For the worst endpoint of each missed comparison, one net at a time is set to
the oracle's per-net totals on the native trees; the moves add up to the
measured delta within 0.2 ps (first order holds).

| design | PDK | corner | vs | endpoint | measured | largest moves: net (ps; native - oracle C, fF) |
|---|---|---|---|---|---|---|
| pre | gf180 | ss+max | OpenRCX | y[0] setup | -10.2 ps | y[0] (-6.8; -0.28), rr_clock_1 (-2.6; -0.08), rr_clock_2 (-0.7; -0.02) |
| hold | sky130 | ss+max | OpenRCX | r3/D setup | -12.1 ps | rr_clock_0 (-10.7; -0.51), n5 (-5.5; -0.33), rr_eco_hold_2_n (+1.7; +0.12) |
| hold | gf180 | ss+max | OpenRCX | y[0] setup | -21.1 ps | rr_clock_1 (-8.8; -0.26), y[0] (-7.1; -0.29), rr_clock_2 (-5.2; -0.16) |
| pre | sky130 | ss+max | Magic | r2/D setup | -57.2 ps | rr_clock_0 (-33.4; -1.58), n4 (-19.9; -1.20), q[0] (-17.1; -1.64) |
| pre | gf180 | ss+max | Magic | r3/D setup | -58.6 ps | q[1] (-43.6; -1.34), rr_clock_0 (-24.0; -0.69), rr_clock_1 (+15.5; -0.44) |
| hold | gf180 | ss+max | Magic | r3/D setup | -48.1 ps | rr_clock_0 (-31.5; -0.91), rr_clock_1 (+16.7; -0.47), q[1] (-15.6; -0.48) |

The slow corner is where the cells are weakest: a clock-buffer or flop output
moves the endpoint by 20-35 ps per fF at ss (GF180 `clkbuf_1`, `dffq_1`,
`dlya_1`; Sky130 `clkbuf_1`). A 10 ps budget therefore needs per-net agreement
of about 0.1-0.3 fF (2-5 %) on the few clock and launch nets of a path.

### Resistance against Magic, element by element

Magic uses the same Magic sheet and contact values as the native profile, so
for resistance it checks geometry and topology. Its network starts at the
terminal square, so a terminal on a pin via adds the via's contact plus half a
square of the pin layer (Sky130 li1: 15.70 ohm at nom against the native 9.3
ohm cut; 6.85 / 31.5 at min / max). With that element taken out on both sides,
every Sky130 net agrees within 2.0 % at every corner except `a[1]`, where a
routing patch bridges two met1 regions: Magic conducts through it and skips two
M1M2 vias (9 ohm at nom), the native tree does not (patches add capacitance
only). GF180 agrees within 1.3 % at nom and max on every net; at min Magic's
`(lrlc)` contacts are 0 (no data) while the native profile keeps 4.5 ohm.

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
  (+61 % on Magic, +68 % on SPICE) at max. Its met3/met4 is +9 %, li1 -9 %.
  A width-dependent effective resistance would explain such a value, but no
  other PDK source states one, so the native profile does not adopt it;
* the Magic rules are a complete per-corner set, contacts included; the SPICE
  corner models are the other complete set and lie between Magic's extremes
  and the nominal value (Sky130 M1M2 max 11.85 against 15 ohm). GF180's
  SPICE corners agree with Magic within 2 %;
* the reference's corner-invariant via resistance is the LEF nominal value,
  which no corner source supports.

So the per-net R gap against OpenRCX is a data-source difference: at Sky130
nominal it is the `calibre` met1/met2 row (one net misses -20 % by 0.9 %); at
the min/max corners of both PDKs it is the calibration tables' wider metal
spread and the corner-invariant LEF via values. Against Magic, which uses the
same data, resistance agrees element by element (previous section).

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

On Sky130 Magic's capacitance corners spread -14 % / +22 % around nominal
(met2 perimeter rule), the calibration table's -5 % / +4 %; on GF180 both
spread about +/-5 %. This data disagreement explains most of the per-net
capacitance difference against Magic: GF180 per-net C is above Magic on
almost every net at min and nom (up to +22 %), as the isolated-wire data
predict; Sky130 is above Magic at min and below it at max.

## Canonical structures

`scripts/rc_structures.py` (`docs/evidence/rc-structures.json`) puts the same
geometry through both oracles and evaluates the native model on it (the
formulas of the unit tests, with the profile's rows). Nominal corner, 40 um
wires at default width:

| PDK | structure | quantity | native | OpenRCX | Magic |
|---|---|---|---|---|---|
| sky130 | isolated met2 | total aF | 3519 | 3507 | 3140 |
| sky130 | met2 pair at minimum space | total / coupling aF | 6299 / 4194 | 6286 / 4194 | 6233 / 4561 |
| sky130 | met2 crossing met1 | upper / lower increment aF | 11.2 / 23.5 | 25.7 / 19.5 | 25.3 / 33.9 |
| sky130 | met2 over met1, aligned | coupling aF | 0 | 3113 | 752 |
| sky130 | met3 over met1, 0.085 um overlap | coupling aF | 0 | 1645 | 272 |
| gf180 | isolated Metal2 | total aF | 3331 | 3308 | 2870 |
| gf180 | Metal2 pair at minimum space | total / coupling aF | 6788 / 5109 | 6765 / 5109 | 5153 / 3552 |
| gf180 | Metal2 crossing Metal1 | upper / lower increment aF | 26.2 / 45.1 | 31.7 / 26.3 | 28.5 / 41.0 |
| gf180 | Metal2 over Metal1, aligned | coupling aF | 0 | 2052 | 595 |
| gf180 | Metal3 over Metal1, 0.085 um overlap | coupling aF | 0 | 973 | 161 |

* Native and OpenRCX agree on isolated wires and pairs within 0.7 % (the
  remainder is the native end faces): same table, same geometry. Magic is
  10-13 % lower on isolated wires and differs by +9 % (Sky130) and -30 %
  (GF180) on pair coupling: different data.
* Crossings: before this stream the native model gave the upper wire 5.3 aF
  (Sky130) and the lower wire nothing; both oracles give each wire about
  20-41 aF. The two-cut model gives 23-45 aF, except the Sky130 upper wire
  (11.2 aF, about half of both oracles).
* Stacked parallel wires: the oracles differ by a factor of 4 to 6 (OpenRCX
  treats the lower wire like a plate; Magic counts the overlap area). The
  calibration tables have no data for this case, so the native model counts
  nothing and reports the overlap as `nonspanning_plane_intervals`; Magic's
  area value (about 15-19 aF/um aligned) is the size of what is left out.

## Acceptance status

* **Slack delta <= 10 ps against a geometry-identical oracle at every corner:
  not met.** Against OpenRCX the final model meets it at 9 of the 12
  design x PDK x corner combinations; it misses GF180 ss+max on the pre-hold
  design by 0.17 ps (10.17 ps) and Sky130 and GF180 ss+max on the hold-closed
  design (12.1 and 21.1 ps). The attribution names the nets: clock and output
  nets that are 0.1-0.5 fF (2-6 %) off OpenRCX, at 20-35 ps per fF at ss.
* **Explained bound against the other oracle.** Against Magic the deltas are
  up to 58.6 ps at ss+max and 34.7 ps at tt. The two oracles disagree with
  each other by the same amount (OpenRCX vs Magic, both back-annotated: up to
  60.4 ps at ss+max, 37.7 ps at tt). The cause is data, not geometry: on an
  isolated wire the calibration tables are 11-14 % above Magic at nominal, the
  corner spreads differ (tables +/-6 %, Magic -14 % / +22 %), and crossings and
  stacked wires are modeled differently (canonical structures). Per net, the
  attribution lists the nets and capacitance differences that make up each
  missed delta.
* **Why no further change was made.** Closing the remaining ss+max deltas
  against OpenRCX needs per-net agreement of about 2-5 % on a few nets, which
  is below the spread of the PDK's own data sources and below the difference
  between the two oracles' structure models. Getting there would mean adopting
  one oracle's treatment of crossings or stacked wires (fitting), which this
  stream does not do.
* Per-net C targets against OpenRCX are met at every corner on both designs.
  Total C against OpenRCX is met except hold GF180 max (-11.3 %, a metric that
  double counts OpenRCX's grounded couplings).
* Resistance discrepancies are explained by data source (calibration `RESOVER`
  rows and corner-invariant LEF vias against Magic/SPICE corner data) and,
  against Magic, element by element (terminal element, one patch-bridged
  net, GF180 `(lrlc)` contacts without data).
* The hold-closed design keeps its hold margin with the new extractor: worst
  hold on the native SPEF is +0.082 / +0.067 / +0.115 ns (Sky130 tt/ss/ff) and
  +0.244 / +0.372 / +0.196 ns (GF180), against +0.076 / +0.056 / +0.112 and
  +0.241 / +0.364 / +0.194 ns on OpenRCX. Setup stays positive everywhere.

## Earlier comparisons (history)

* Before this stream (`docs/evidence/extraction-comparison.json`,
  `docs/evidence/extraction-requalification.json`): native was -4..-9 % low in
  total C against OpenRCX, slack deltas 12.7 ps (Sky130 ss), 10.1 and 24.0 ps
  (GF180 tt, ss) on the pre-hold design. Those campaigns attributed the gap to
  pads and end caps; measured now (ablation `pads_ends`), pads and end caps
  close most of the total-C gap (Sky130 max -4.8 % -> -0.1 %) and part of the
  GF180 slack delta (ss+max 24.0 -> 16.4 ps); the rest came from the crossing
  model and the plate rule (16.4 -> 10.2 ps; Sky130 ss+max 12.9 -> 7.8 ps).
* The per-edge DEF of earlier flows made the OpenRCX reference depend on path
  segmentation (+134..+144 % C on the same metal); the flow's DEF now writes
  maximal runs and OpenRCX on it equals the merged level. The campaign checks
  this per design (`openrcx.flow_reference_nominal`: 0.0 % difference).

## Reproduction

```sh
free -h
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G timeout 900s \
  cargo build --release --bins
python3 scripts/build_rc_profile.py --check
python3 scripts/extraction_ablation.py --out .work/abl --build     # optional ablation binaries
python3 scripts/verify_routing.py --workspace .work/runs/flowA --base-rev empty \
  --rustroad target/release/rustroad --rusteda STA/rusteda --ruststa STA/ruststa --producer policy
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v \
  timeout 3000s python3 scripts/rc_oracles.py campaign --workspace .work/runs/final/pre \
  --base-rev empty --inputs CASES.json --rustroad target/release/rustroad --sta-bin STA \
  [--variant NAME=BIN ...]
python3 scripts/rc_oracles.py report --summary .work/runs/final/pre/summary.json
python3 scripts/rc_structures.py --out .work/runs/structures --evidence docs/evidence/rc-structures.json
python3 scripts/magic_rc.py controls --pdk sky130 --out .work/runs/magic_controls/sky130
python3 scripts/rc_data_sources.py --out docs/evidence/rc-data-sources.json --markdown
python3 -m pytest -q scripts/tests
```

`CASES.json` lists, per PDK, the routed workspace, the post-route netlist and
SDC, and optionally the flow's OpenRCX nominal SPEF (see the script's help).
The historical OpenRCX-only campaign is `scripts/compare_spef.py campaign`.

## Limitations

* Only default-width wires and fixed single-layer-cut vias are supported. The
  calibration tables have one width per layer.
* Via cut capacitance is not modeled; the pads are.
* Metal on other layers that does not form a plate (narrower, offset or
  barely wider parallel wires, partial pad overlaps) adds no coupling. Magic's
  overlap-area value bounds what is left out (about 15-19 aF/um for aligned
  stacked minimum wires).
* Diagonal adjacent-layer coupling (`DIAGUNDER`) is not modeled.
* A crossing is counted from both cross-sections; the overlap area itself is
  in both (about `area_cap * w_upper * w_lower`, 2.6 aF for Sky130 met2/met1,
  against 23-35 aF per crossing).
* Piece strips count area per strip, so a piece's area term is approximate
  (under 5 % of a pad's or end's capacitance).
* A lateral pair is split half-and-half; where one side's nearest shape
  differs from the other's, the pair is counted unevenly.
* Same-net metal is joined only through the stored lattice graph. Routing
  patches add capacitance but do not conduct, so a patch that bridges two
  same-net regions is not a parallel resistance path (Magic: `a[1]`, about
  9 ohm at nom on Sky130).
* OpenRCX reads DEF `RECT` patches without error but does not extract them
  (Sky130: 8 patches, GF180: 1, a few aF each). The native extractor and Magic
  include them.
* Magic timing is a back-annotation of Magic's per-net totals onto the native
  trees, not a Magic RC network (its `.res.ext` node names are not unique, so
  the network cannot be rebuilt reliably); the fidelity control bounds the
  effect on these designs to 0.31 ps.
* Serial `cpu_ref` only. There is no GPU path and none is claimed.
