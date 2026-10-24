# Native RC extraction

`rustroad extract` turns the routed HEAD revision of a workspace into detailed
IEEE 1481 SPEF. It is a read-only command. It reads the stored routing records
(`routing.rbin`), the placed pin/obstruction geometry and the supply metal, and
a per-PDK, per-corner RC profile built from PDK data files. OpenRCX is used only
as a black-box differential oracle (`scripts/compare_spef.py`). No reference
source was read, and no native coefficient was fitted to reference output.

Scope is block level. Supply nets are not extracted. Pin capacitance stays in
Liberty. Via cut and landing-pad self capacitance, wire end caps and diagonal
coupling are not modeled (see "Limitations"). SI, IR/EM, OCV and CPPR remain
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

## Physical model

Each routed net is built from its stored lattice records: wires, fixed vias and
pin-access points.

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
   A via's resistance is the per-cut value divided by the number of cuts.
4. **Capacitance.** Every run is cut into intervals along its axis at the
   extents of nearby shapes. Nearby shapes are other runs, via pads, instance
   pins, port pins, obstructions and supply metal on routing layers. For each
   interval:
   * The nearest conductor below and above selects the calibration context
     `(over, under)`. A missing exact context falls back to `(over, -)`, then
     to `(0, under)`, then to `(0, -)`, and is counted.
   * Each side looks up the nearest same-layer shape within the table range.
     The row at that edge spacing (linear interpolation, clamped to the first
     row) gives coupling `cc` and ground `cg`. With no neighbour, the side
     takes the isolated last row. A same-net neighbour shields the side (`cg`
     only). A touching same-net neighbour removes that side.
   * The substrate share of `cg` (the OVER-0 row at the same spacing) is
     grounded. The excess over it is the capacitance to the plane below or
     above, which is attributed to that plane shape's net.
   * Ownership: a lateral pair of runs is counted half from each side. A
     crossing between two runs is owned by the upper run's OVER context.
     Pins, pads, obstructions and supply metal are counted fully from the
     run's side.
   * A partner with no signal net (supply, obstruction, unconnected pin) is
     ground. A partner pin is attached at that pin's tree node, a pad at its
     via node, and a run at its end nearest the interval. On the run's own side,
     each value is split between its two end nodes by position (pi model).
5. **Routing patches.** `Routes.patches` (the router's same-net notch and
   corner fills, DEF routing `RECT`) add capacitance, never a node or a
   resistor. Each patch attaches to the RC node of the same-net conductor it
   overlaps or touches: largest overlap area first, then the lowest shape
   index; a wire run attaches at its end nearer the patch centre, a via pad at
   its via node, a pin at its pin node. The part of the patch not already
   covered by same-net metal on its layer (wire runs with end extensions, via
   pads, pin shapes, earlier patches) is cut into disjoint rectangles by exact
   integer subtraction (at most 256 pieces per patch). Every piece is a
   conductor shape with two side strips, one along x and one along y, so all
   four of its edges take the per-side model of step 4: ground with the
   context and plane split, and lateral coupling to the nearest other-net
   shape. Other nets' runs see the pieces as neighbours and planes. A strip
   facing a wire run's end cap owns that coupling (factor 1), because the
   wire's side model never counts its end caps; strip-to-run sides are split
   half and half as for two runs. A floating patch, a patch on a net without
   an RC tree and a patch off a routing layer are explicit errors. The JSON
   `stats` report `patches` and `patch_pieces`; `scope.routing_patches`
   states the model. Workspaces without patches extract byte-identically to
   the previous binary (checked at min/nom/max on both PDKs).
6. **Coupling output.** Cross-net capacitors are written as SPEF coupling caps,
   listed in both nets' `*CAP` sections. So each net's declared total equals
   its ground plus coupling, just as in the reference SPEF. RustSTA 0.3 grounds
   coupling at factor 1 (T0074), so for timing this equals a grounded model.
   Keeping coupling explicit preserves the cross-net information for a later
   crosstalk analysis and keeps per-net totals comparable.

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

* 2 M shapes x 56 B = 112 MiB;
* 8 M bin entries x 4 B, plus the map, < 128 MiB;
* 500 k nodes with keys and adjacency, about 64 MiB;
* 1 M couplings x about 64 B (BTreeMap) = 64 MiB;
* SPEF model strings < 64 MiB; the file is streamed through a 256 KiB buffer;
* the workspace read itself is under 16 MiB of `design.json`, about 200 MiB parsed.

That is about 0.7 GiB peak, under the 2 GiB per-test-binary limit. There are no
paths, cuts or cones. Each net is one linear walk, and the neighbour search is
bounded by the window, `max_candidates` and `max_work`.

Measured on the pipeline fixture:

* native extraction: 0.02-0.04 s and 8.0-8.2 MiB peak RSS per run (36 runs);
* full comparison campaign (169 guarded processes): 28.4 s wall clock, 136 MiB
  peak child RSS (RustSTA import);
* OpenRCX container: 109-125 MiB, under 1 s per run.

Builds and tests used `CARGO_BUILD_JOBS=4`, `RUST_TEST_THREADS=4` and
`systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G`.

## Verification

* **Unit tests.**
  * `rustroad-core::extract`: an analytic straight wire (10 ohm, 8e-4 pF, by
    hand); a parallel pair (coupling counted once, per-side ground); a branch,
    via and mid-run pin tree with exact resistor values; run splitting and loop
    opening; plane coupling with an explicit fallback and an exact hand total;
    budgets; and profile-width mismatch. Patches: a hand-computed patch beside
    a wire (the covered wire side drops out, the three exposed patch edges are
    added at the nearer end node, the tree is unchanged); an overlapping patch
    counts only its uncovered piece; a fully covered patch adds nothing; a
    patch couples once to a parallel net and owns the coupling to a facing
    wire end; floating, unrouted and cut-layer patches fail.
  * `rustroad-formats::spef`: syntax, the name map, escaping, exact totals,
    determinism, and rejection of invalid values.
* **Integration tests** (`crates/rustroad-workspace/tests/extraction.rs`, on a
  routed 7-net fixture):
  * byte-identical rerun;
  * unchanged workspace tree;
  * one D_NET and one tree per net;
  * refusals that leave no output and no temporary file: output inside the
    workspace, corner mismatch, technology LEF mismatch, missing layer, byte
    cap, and zero cap.
* **Independent resistance accounting** (campaign). For each net, the Python
  harness recomputes `sum(Rs*L/W) + vias` from the exported route records and
  the profile, then compares it with the SPEF resistors. The maximum relative
  error is 2.5e-6 on both PDKs at every corner.
* **Negative controls** (campaign, both PDKs, all detected):

  | control | detected by |
  |---|---|
  | x3 resistance profile | R target fails; C unchanged |
  | dropped D_NET | coverage fails; RustSTA `missing SPEF annotation` |
  | net renamed to an unknown name | coverage fails; RustSTA rejects |
  | removed `*I` sink connection | `*CONN` mismatch; RustSTA rejects |
  | output inside the workspace | refused |
  | corner mismatch | refused |

## Comparison against OpenRCX (black box)

`scripts/compare_spef.py campaign --workspace .work/extract-final --base-rev empty`
works on byte-checked copies of `.work/rc-timing/ready`. It runs:

* the native extractor at min, nom and max;
* OpenRCX in the pinned `openroad/orfs` image (`f152917e`), with the
  harness's own flags (`-max_res 0 -coupling_threshold 0.1 -no_merge_via_res`)
  and the same rules file per corner. It is run on two DEFs:
  * **harness DEF**: the DEF exactly as the flow writes it, one path per
    lattice edge. Its nominal output reproduces the ready run's SPEF byte for
    byte, apart from `*DATE`.
  * **merged DEF**: the same file with collinear lattice edges merged through
    points that are not a via, a pin access or a branch. Each edge already
    carries the DEF default half-width extension, so the metal union is
    identical. The script checks this net by net, layer by layer and track by
    track, and the via lists are identical.
* RustSTA 0.3 at tt+nom, ss+max and ff+min, using the Liberty from owner
  decision (f) with only its wire-load groups projected out, the frozen
  post-route SDC, and each SPEF in turn.

Targets were fixed before measuring:

* total design wire C within +/-10 %;
* per-net total C within +/-20 % for nets with reference C >= 5 fF;
* per-net total R within +/-20 % for nets with reference R >= 20 ohm;
* endpoint setup/hold slack delta <= 10 ps.

Results (native vs reference; the evidence is
`docs/evidence/extraction-comparison.json`):

| PDK | RC | reference DEF | total C | per-net C (n, min..max) | per-net R (n, min..max) |
|---|---|---|---|---|---|
| sky130 | min | merged | -5.9 % pass | 8, -18.5..+3.7 % pass | 19, -69.7..-36.4 % **fail** |
| sky130 | nom | merged | -6.2 % pass | 9, -18.5..+3.9 % pass | 19, -19.4..-2.6 % pass |
| sky130 | max | merged | -7.1 % pass | 11, -16.7..+5.8 % pass | 19, +7.0..+139 % **fail** |
| gf180 | min | merged | -7.1 % pass | 12, -7.2..+1.3 % pass | 19, +0.9..+13.4 % pass |
| gf180 | nom | merged | -7.9 % pass | 13, -8.1..+1.1 % pass | 19, -2.5..-1.3 % pass |
| gf180 | max | merged | -9.0 % pass | 13, -9.2..+0.6 % pass | 19, +15.2..+177 % **fail** |
| sky130 | min/nom/max | harness | -59.0/-59.3/-60.0 % **fail** | all **fail** (-73..-40 %) | all **fail** |
| gf180 | min/nom/max | harness | -60.3/-60.8/-61.4 % **fail** | all **fail** (-76..-52 %) | all **fail** |

Net coverage and every `*CONN` (pins, ports, directions) match the reference
in all 12 comparisons.

| PDK | STA | reference DEF | max abs setup / hold delta | native WNS / WHS | reference WNS / WHS |
|---|---|---|---|---|---|
| sky130 | tt+nom | merged | 5.76 / 5.28 ps pass | 9.0981 / +0.0369 ns | 9.0926 / +0.0362 ns |
| sky130 | ss+max | merged | 13.27 / 12.36 ps **fail** | 8.5080 / -0.0371 ns | 8.4951 / -0.0410 ns |
| sky130 | ff+min | merged | 3.39 / 3.06 ps pass | 9.3326 / +0.0819 ns | 9.3293 / +0.0818 ns |
| gf180 | tt+nom | merged | 9.53 / 7.60 ps pass | 7.7025 / -0.4182 ns | 7.6930 / -0.4213 ns |
| gf180 | ss+max | merged | 22.54 / 17.82 ps **fail** | 5.6614 / -0.9435 ns | 5.6389 / -0.9513 ns |
| gf180 | ff+min | merged | 4.59 / 3.67 ps pass | 8.5722 / -0.1955 ns | 8.5676 / -0.1967 ns |
| sky130 | tt/ss/ff | harness | 152 / 289 / 99 ps **fail** | | 8.9457 / +0.0002, 8.2193 / -0.1116, 9.2337 / +0.0606 ns |
| gf180 | tt/ss/ff | harness | 387 / 810 / 213 ps **fail** | | 7.3157 / -0.6355, 4.8515 / -1.4080, 8.3587 / -0.3103 ns |

### Dominant causes (measured, not tuned)

1. **The harness-DEF reference depends on path segmentation, not on metal.**
   On the identical metal, the reference's own result changes by -56.5 % to
   -57.6 % in total C. Its STA changes by up to 147 ps (Sky130) and 377 ps
   (GF180) at tt, and by 276 ps / 787 ps at ss, when only the DEF path
   segmentation changes from 4964/4982 lattice-edge paths to 92/74 merged
   paths. Every lattice edge carries its own half-width end extension, so the
   overlapping metal is counted again per edge. The native extractor builds
   runs from the connectivity graph and is independent of segmentation by
   construction.
   **Implication for the existing flow:** the current OpenRCX-based post-route
   timing (`docs/postroute-timing.md`) overstates wire C by about 2.3x and wire
   R by 1.2-2x. That includes the reported GF180 tt hold of -0.6355 ns; with the
   merged reference it is -0.4213 ns. The DEF writer, or the reference step,
   should merge collinear lattice edges before reference extraction. That
   belongs to the router/DEF owners; it is not changed here.
2. **Resistance at the min/max corners comes from different PDK corner data.**
   * The native model uses the Magic high/low variants. Contacts there move
     much more than metal: Sky130 m2c 2.0 / 4.5 / 15 ohm and GF180 via 4.5 /
     4.5 (fallback) / 15 ohm, against metal ±16 %.
   * The reference resistance is corner-invariant for vias: 9.3/4.5/3.41 ohm
     on Sky130 and 4.5 ohm on GF180, observed at min, nom and max in its SPEF.
     Its metal resistance scales like the calibration tables' fourth column
     (Sky130 met2 min/nom/max 17.76/23.46/31.55 ohm on the same run, i.e.
     0.757x/1.345x, matching the column ratios).
   * At nominal on Sky130, a black-box observation of the clk net shows the
     gap: an 18.815 um met2 run is 23.46 ohm in the reference and 16.80 ohm
     natively; a 23.365 um met3 run is 4.04 vs 3.66 ohm. The reference value
     corresponds to an effective 0.173 ohm/sq at 0.14 um width, against the
     PDK sheet resistance of 0.125 (Magic and LEF agree). Vias are identical.
   * GF180 nominal resistance agrees within -2.5 %.
   The native model keeps the foundry sheet and contact data.
3. **The ss+max slack delta is driven by capacitance, not resistance.** The
   sensitivity runs replace only the max-corner resistance by the nominal
   resistance, either all of it or the vias only. The deltas stay at
   13.6 / 13.5 ps (Sky130) and 23.0 / 22.9 ps (GF180), so the -7.1 % / -9.0 %
   total-C gap at the slow corner is the cause. The a priori size of the
   omitted metal is 10.7 / 6.5 um^2 of via-pad area outside the net's wires
   and 2.1 / 4.7 um^2 of end caps. Converted with the profile's isolated value,
   that is 7.9 fF (Sky130) and 3.9 fF (GF180), against measured gaps of
   6.1 fF and 9.5 fF. This is the most likely dominant omission on Sky130.
   On GF180 it explains about 40 % of the gap.

No coefficient was changed after these measurements. Modelling pads and end
caps is the next step. Because the reference deficit is now known, that change
should also be validated against a second independent oracle (for example
Magic extraction of the same metal) to avoid a correlated fit.

## Re-measurement after integration (stream b0, 2026-10-24)

`export-def` now writes maximal collinear runs (see `docs/requalification.md`),
so the flow's own DEF is already at the merged level. The campaign was re-run
on a fresh policy-producer flow (`--ready` = that run, RustSTA 0.4 binaries
via `--sta-bin`, router with LEF rules and patches, native patch extraction):

```sh
python3 scripts/compare_spef.py campaign --workspace .work/runs/spef2 --base-rev empty \
  --ready .work/runs/r2 --rustroad target/release/rustroad \
  --sta-bin /mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin
```

Evidence: `docs/evidence/extraction-requalification.json` (34.0 s wall, 136 MiB
peak child RSS, status `partial`: every control and self-check passes, not
every target). "harness" is now the flow DEF itself; `merge-def` of it differs
only where it merges through an upper-layer via landing (104 -> 100 Sky130,
77 -> 72 GF180 paths), and the reference's total C moves by less than 0.05 %.

| PDK | RC | total C | per-net C (n, min..max) | per-net R (n, min..max) |
|---|---|---|---|---|
| sky130 | min | -4.1 % pass | 7, -6.8..+0.3 % pass | 19, -74.9..-36.3 % **fail** |
| sky130 | nom | -4.2 % pass | 9, -6.4..+2.4 % pass | 19, -20.9..-2.3 % **fail** |
| sky130 | max | -4.8 % pass | 10, -5.2..+1.9 % pass | 19, +7.4..+137.8 % **fail** |
| gf180 | min | -7.0 % pass | 12, -6.3..-0.2 % pass | 19, +0.9..+13.5 % pass |
| gf180 | nom | -7.8 % pass | 13, -7.2..-0.2 % pass | 19, -2.5..-1.3 % pass |
| gf180 | max | -8.9 % pass | 13, -8.4..-0.4 % pass | 19, +8.7..+178.3 % **fail** |

| PDK | STA | max abs setup / hold delta | native WNS / WHS | reference WNS / WHS |
|---|---|---|---|---|
| sky130 | tt+nom | 4.91 / 6.42 ps pass | 9.1081 / -0.0045 ns | 9.1032 / -0.0086 ns |
| sky130 | ss+max | 9.63 / 12.74 ps **fail** | 8.5310 / -0.0998 ns | 8.5214 / -0.1060 ns |
| sky130 | ff+min | 3.06 / 3.59 ps pass | 9.3394 / +0.0574 ns | 9.3363 / +0.0548 ns |
| gf180 | tt+nom | 10.12 / 7.20 ps **fail** | 7.7020 / -0.3945 ns | 7.6929 / -0.3966 ns |
| gf180 | ss+max | 24.01 / 17.21 ps **fail** | 5.6606 / -0.8987 ns | 5.6387 / -0.9046 ns |
| gf180 | ff+min | 4.86 / 3.41 ps pass | 8.5719 / -0.1805 ns | 8.5675 / -0.1814 ns |

What changed and why (measured, nothing tuned): the routed design is now the
LEF-rule router output of the policy producer, so per-net numbers moved. The
Sky130 nominal per-net R floor moved from -19.4 % to -20.9 % (one net, the
known 0.125 vs implied 0.173 ohm/sq met2 difference), and the GF180 tt+nom
setup delta from 9.53 to 10.12 ps; both remain the capacitance and resistance
causes analysed above. Patch capacitance is 8 x a few aF on Sky130 and one
patch on GF180 (total-C change < 0.01 %). The omitted-metal estimate is now
6.2 fF (Sky130 nom) against a 3.1 fF measured gap, and 3.6 fF against 7.5 fF
on GF180. The controls (x3 R, dropped/renamed net, dropped connection) and the
independent resistance accounting (max rel. error 3.0e-6) all pass.

## Reproduction

```sh
free -h
export CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G timeout 900s \
  cargo build --release --workspace
python3 scripts/build_rc_profile.py --check
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G \
  /usr/bin/time -v timeout 900s python3 scripts/compare_spef.py campaign \
  --workspace .work/extract-final --base-rev empty
python3 scripts/compare_spef.py report --summary .work/extract-final/summary.json
python3 scripts/compare_spef.py spef --native A.spef --reference B.spef
python3 scripts/compare_spef.py merge-def --def IN.def --routes ROUTES.json --out OUT.def
```

The campaign exits 3 (`partial`): every self-check and control passes, but not
every target is met.

## Limitations

* Only default-width wires and fixed single-layer-cut vias are supported. The
  calibration tables have one width per layer.
* Via cuts and landing pads are neighbours and planes for other nets, but
  their own capacitance and wire end caps are omitted (estimated above).
* Diagonal adjacent-layer coupling is not modeled. A crossing is approximated
  by a full plane over the overlap interval, with no fringe beyond the
  footprint.
* A lateral pair is split half-and-half. Where one side sees a wider via pad,
  that region is counted about 1.5x.
* Same-net metal is joined only through the stored lattice graph, not through
  geometric overlap; routing patches are the exception (attached by overlap,
  capacitance only, see the physical model).
* OpenRCX reads DEF `RECT` patches without error but does not extract them, so
  the reference lacks the patch capacitance (sky130: 8 patches, gf180: 1, a few
  aF each). The native extractor includes it.
* Serial `cpu_ref` only. There is no GPU path and none is claimed.
