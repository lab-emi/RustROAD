# Native GDSII streamout (`export-gds`)

Status: implemented and black-box verified on the routed pipeline fixture for
SKY130 (`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`) by
`scripts/verify_gds.py` (below). Scope is a block/macro: no pad ring, no seal
ring; IR/EM/SI/OCV/CPPR are unchecked. `export-gds` itself reports
`drc_status: "unchecked"`; the foundry checks run in the harnesses below.
Resource contract: `docs/gds-budget.md`. Run receipts:
`docs/evidence/gds-verification.json` (historical evidence, kept for
reproducibility).

## Command

```sh
rustroad --workspace W export-gds \
  --cell-gds <PDK standard-cell .gds> \
  --layer-map fixtures/pdk/<pdk>-gds-map.json \
  --out <file.gds> \
  [--cell-lef <cell LEF; must match the imported sha256>] \
  [--max-gds-bytes 268435456] [--max-gds-records 16000000] \
  [--merge-shapes off|rectangles|polygons] [--vias cells|flat]
```

The defaults (`--merge-shapes off --vias cells`) write the legacy stream,
byte-identical to the export before shape merging existed; the structural
harnesses rely on it (see "Which representation"). `--merge-shapes rectangles
--vias flat` writes the same geometry as an exact per-net cover with flat vias,
the cheapest stream for the foundry DRC/antenna/LVS decks measured below; the
IHP Ibex native recipe passes these two options to its `be_gds` step.

- Read-only: takes the shared workspace lock, has no `--base-rev`, never writes
  inside the workspace (`GDS_OUTPUT_IN_WORKSPACE`), never overwrites an input or
  a non-regular file. HEAD and every revision file are unchanged (checked by
  tree hash in the harness and in `tests/gds_export.rs`).
- Atomic: streamed through a byte/record-capped hashing writer into a hidden
  temporary file in the destination directory, fsynced, renamed, directory
  fsynced. Any failure removes the temporary file; nothing partial is published.
- Deterministic: fixed BGNLIB/BGNSTR dates (1970-01-01 00:00:00), LIBNAME = design
  name, no paths or timestamps. Copies of a workspace at different paths stream
  identical bytes.
- Result (stdout JSON): `status: "written"`, `revision`, `top`, `output
  {path, sha256, bytes}`, `records`, `cell_gds`/`layer_map`/`cell_lef`
  identities, `foreign_bindings_checked`, `lef_dbu_per_micron`,
  `gds_dbu_per_micron`, `coordinate_divisor`, `pdk_structures`,
  `via_structures`, `elements {instances, routed_wires (merged runs),
  stored_wire_records, routed_vias, routed_patches, supply_wires,
  supply_vias, pin_shapes, labels, boundary, merged_rectangles,
  merged_polygons, merged_holed_pieces, merged_capped_pieces}`,
  `routing_status`, `drc_status: "unchecked"`, `lvs_status: "unchecked"`,
  `scope: "block_level_no_pad_or_seal_ring"`, `shape_merge` and `via_style`
  (the styles used), `workspace_mutated: false`.
- Errors exit 2 with `{"status":"error","code":...}`. GDS codes:
  `GDS_ODD_COORDINATE`, `GDS_COORDINATE_RANGE`, `GDS_UNIT_MISMATCH`,
  `GDS_UNITS_UNSUPPORTED`, `GDS_CELL_MISSING`, `GDS_NAME_COLLISION`,
  `GDS_LAYER_MAP`, `GDS_LAYER_UNMAPPED`, `GDS_LAYER_KIND`, `GDS_WIDTH`,
  `GDS_VIA_MISSING`, `GDS_PIN_GEOMETRY`, `GDS_LABEL_UNSUPPORTED`,
  `GDS_FOREIGN_MISSING`, `GDS_FOREIGN_MISMATCH`, `GDS_SOURCE_MISSING`,
  `GDS_SOURCE_CHANGED`, `GDS_CELL_LEF`, `GDS_OUTPUT`, `GDS_OUTPUT_IN_WORKSPACE`,
  `GDS_UNPLACED`, `GDS_UNSUPPORTED_WORKSPACE`, `GDS_BUDGET`, `GDS_IO`, plus the
  reader codes below for a malformed cell library.

## What is streamed

| Structure / element | Source and semantics |
|---|---|
| PDK cells | Every used master plus its SREF/AREF closure (iterative DFS, depth cap 64, cycle guard), copied **byte-identically** from the cell GDS in library order. Missing structure: `GDS_CELL_MISSING`. |
| `VIA_<name>` | `--vias cells` (default) only: one structure per used fixed LEF VIA (routed + supply), its RECT/POLYGON shapes on the map's `via` targets. Name collisions with any cell-GDS structure or the top: `GDS_NAME_COLLISION`. With `--vias flat` no via structure is written. |
| Instances | SREF per instance in instance order. Row `N`: no STRANS at (x, y). Row `FS`: STRANS x-reflection at (x, y + height). |
| Net metal (`--merge-shapes rectangles\|polygons` or `--vias flat`) | Per net and per GDS layer/datatype, the routed wires and patches below, for a supply net its supply rails/spines, and (flat vias) the LEF shapes of the net's vias at their via points are written as one exact cover of their union: maximal non-overlapping rectangles (`rectangles`) or one simple polygon per hole-free connected piece (`polygons`). See "Same-net shape merging". With `--vias cells` the net's routed (and supply) via SREFs follow its metal. The default `--merge-shapes off --vias cells` writes the rows below unchanged (byte-identical to the unmerged export). |
| Routed wires | DEF NETS semantics: layer default `WIDTH`, half-width extension at both ends (exact `wire_rect`). One BOUNDARY per maximal collinear same-net run on `net` targets, the same runs `export-def` writes (`rustroad_core::wire_runs`): stored lattice records that touch or overlap on one track line are merged, and a run is split only at a strictly interior via landing (lower or upper metal), pin-access point or same-layer branch. The metal union equals the stored records'. |
| Routed / supply vias | Default `--vias cells`: SREF to `VIA_<name>` at the via point. Every used LEF via variant (LEF-rule routing, e.g. `M1M2_PR_MR`, `Via1_VH`, `Via2_HH`) gets its own structure. `--vias flat`: the via's LEF RECT shapes at the via point, on each layer's `via` targets, merged with the net's metal (cut shapes stay one rectangle per cut unless cuts of one net touch); LEF POLYGON shapes are written unmerged (the router and the supply builder accept only rectangular vias today, so this path is covered by unit tests only). |
| Routing patches | Every `Routes.patches` rectangle (DEF routing `RECT`, same-net notch/corner fill) as one BOUNDARY on the routing layer's `net` targets. |
| Supply rails/spines | SPECIALNETS geometry (explicit width, half-width extension) on `special_net` targets. |
| Pins | I/O and supply pin rectangles on every `pin` target; one TEXT per pin at the rectangle centre on the `label` target. Bus bits are labelled `a[1]`; supply ports `VDD`/`VSS`. Escaped/non-printable names: `GDS_LABEL_UNSUPPORTED`. |
| PR boundary | Die area on the map's boundary layer (SKY130 235/4, GF180 0/0). |

## Same-net shape merging

The DEF view of a route is a set of maximal collinear runs (split at via
landings, pin-access points and branches) with half-width end extensions,
plus patches. Runs of one net overlap at every split and corner, and patches
or supply ties are often covered by runs. Every downstream geometry engine
merges this again: the foundry DRC, antenna and LVS decks treat a layer as
the union of its shapes. With `--merge-shapes rectangles|polygons`
`export-gds` writes each net's metal as an exact cover of the union
(`rustroad_workspace::gds_merge`):

1. Rectangles of one net on one GDS layer/datatype are sorted, deduplicated
   and grouped into connected pieces (positive-area overlap or a shared edge of
   positive length; corner contact does not connect, so no polygon touches
   itself).
2. Each piece is rasterized on its own compressed grid (its distinct edge
   coordinates; Bentley's rectangle-union treatment, Preparata & Shamos,
   *Computational Geometry*, 1985, sec. 8.4) with a 2-D difference array.
3. `rectangles`: the covered cells are decomposed into maximal slabs (runs of
   covered cells along one axis, merged across the other while identical), in
   both orientations; the decomposition with fewer rectangles is written
   (horizontal on ties). The rectangles do not overlap.
   `polygons`: the boundary of the covered cells is traced as directed edges
   (interior on the left; at a diagonal pinch the walk turns left, keeping the
   cell it borders). A piece with exactly one loop and no repeated vertex is
   written as one BOUNDARY; a piece with holes (a same-net loop) keeps its
   rectangle decomposition, since a GDS BOUNDARY cannot carry a hole.
4. Output is sorted, so the stream does not depend on input order.

A piece over 2^22 grid cells keeps its input rectangles (exact, unmerged;
counted as `merged_capped_pieces`), and a polygon over 8,190 vertices (one GDS
XY record) is written as its rectangle decomposition. The covered point set of
every net and layer is unchanged; instances, pins and labels are unchanged.

Vias: with `--vias flat` each routed and supply via contributes its
LEF shapes at the via point to its net's metal instead of an SREF to a via
structure. The flattened geometry equals the SREF expansion exactly. The
reason is downstream cost: KLayout's deep-mode DRC, antenna and LVS decks
process every via SREF as a cell instance with its own interaction context;
written as top-level shapes, the cuts and enclosures are plain flat geometry
(see "Downstream check cost" below).

Evidence (Ibex SG13G2/EZ130, opt/phys layout, 2026-11-04): the top cell goes
from 178,384 BOUNDARY elements and 189,396 SREFs (`--merge-shapes off --vias
cells`, the default, byte-identical to the previous export: 54,368 instance
SREFs plus 135,028 via SREFs, 130,729 routed and 4,299 supply) to 371,073
BOUNDARY elements and 54,368 SREFs (`--merge-shapes rectangles --vias flat`:
370,828 merged rectangles, 244 pin shapes, the PR boundary; instance SREFs
only), and the library from 245 to 227 structures. A KLayout layer-by-layer
XOR of the rectangles+flat stream, `--vias cells` with either merge style, and
`--merge-shapes polygons --vias flat` against the default stream is empty on
all 38 layer/datatype pairs, and the label sets are equal.
`tests/gds_export.rs` checks the same equality on the fixture by cell-centre
coverage of every routing and cut layer for all style combinations, on the
routed design and on the placed design before routing (where the merged
styles draw the supply nets in their own pass), and `gds_merge` unit tests
check exact, non-overlapping covers on 300 random rectilinear sets plus the
piece-cap and vertex-limit fallbacks. Every style is byte-identical across
runs and `--threads 1`/`2`/`4` (default `ca8b8a6d…`, rectangles+flat
`96cdad25…`, polygons+flat `f7e9af41…`), and a complete Ibex flow run with
the recipe's `--merge-shapes rectangles --vias flat` streamed the same bytes
as a replay of `export-gds` on the copied final workspace.

## Which representation

The default stream keeps the structure that the structural harnesses inspect,
so they must not pass the merged or flat styles:

- `scripts/verify_gds.py` requires every DEF `RECT` patch as its own 5-point
  boundary on its net layer (check 4b; on the Ibex rectangles+flat stream
  1,362 of 1,606 patches are absorbed into larger rectangles and the check
  fails; with `--merge-shapes rectangles --vias cells` 1,092; with the
  default 0), and its deleted-wire control looks for a top-level box whose
  centre line equals a DEF NETS path;
- the rtl2gds `removed_via` control (`scripts/rtl2gds_controls.py`, through
  `hold_closure.export_gds`) builds its via set from the report's
  `via_structures`, and `scripts/signoff/kl_plant.py` `missing_via` deletes
  one `VIA_<name>` SREF at the via point.

The merged/flat styles are for flows that only hand the stream to geometry
engines (foundry DRC, antenna, LVS, density, XOR). Their geometry is proven
equal to the default stream's by the coverage tests and the Ibex XOR above,
not by `verify_gds.py`.

Units: coordinates are integer LEF DBU. The cell-GDS UNITS are copied verbatim
and must be an exact decimal micron grid (`dbu_per_micron`: user unit 10^-k um,
meter unit 10^-(6+k), compared as correctly rounded binary64). When the LEF grid
is an integer multiple of the GDS grid every coordinate is divided exactly
(GF180: 2000 DBU/um -> 0.001 um, divisor 2); any remainder is
`GDS_ODD_COORDINATE`. Library units are never rescaled (the GF180 deck has an
explicit 0.001 um DBU rule).

FOREIGN: the cell LEF recorded at import (sha256-pinned; `--cell-lef` may
relocate it) is re-read; each used master must have exactly one `FOREIGN`
naming itself, offset 0 0, orientation N/absent, and `ORIGIN 0 0`. The LEF
reader now retains `FOREIGN name [x y [orient]]` as `LefMacro.foreign:
Vec<LefForeign>`; import still uses LEF geometry and records a diagnostic.

## Downstream check cost

The IHP foundry decks (KLayout deep mode) and the tap-coverage latch-up check
were run on three streams of the same Ibex layout and on LibreLane 3.0.17's
final stream of the same design, with the exact invocations of the campaign's
shared gates (`signoff_layout.py`), 3 interleaved repetitions per stream
(order alternated each repetition, one check at a time, 4 vCPUs shared with
other work at 1-minute load 1.7 to 3.0). Medians, wall / CPU
(user + system) seconds; peak RSS is GNU time's maximum resident set size:

| Check | LibreLane | default export | rectangles + flat | rectangles + flat / LibreLane | peak RSS KiB (LibreLane / default / rectangles + flat) |
|---|---|---|---|---|---|
| Foundry main DRC | 531.3 / 683.3 | 500.0 / 641.9 | 376.3 / 514.0 | 0.71 / 0.75 | 2,292,508 / 2,319,980 / 2,263,316 |
| Foundry antenna DRC | 255.4 / 251.7 | 265.2 / 263.0 | 193.8 / 190.9 | 0.76 / 0.76 | 1,693,768 / 1,721,132 / 1,614,236 |
| Full-device LVS | 187.5 / 185.4 | 172.5 / 170.4 | 131.5 / 130.0 | 0.70 / 0.70 | 865,992 / 813,412 / 629,192 |
| Latch-up tap coverage | 38.3 / 37.9 | 42.9 / 42.5 | 42.8 / 42.2 | 1.12 / 1.11 | 579,260 / 600,704 / 600,304 |

"Default export" is `--merge-shapes off --vias cells` (byte-identical to the
earlier writer); "rectangles + flat" is `--merge-shapes rectangles --vias
flat`. The main-DRC peak RSS was below LibreLane's in every paired
repetition, but only by 0.4 to 2.3 % (0.16 % between the highest
rectangles + flat run and the lowest LibreLane run), so it is parity rather
than a win: the peak is set by the FEOL filler tables, which the export does
not change.
The main-deck gain is in the deep-mode tables that see every via SREF as an
instance with its own context (3.2 angle, 3.1 offgrid, 5.17 Metaln); the
FEOL filler tables are unchanged. Merging alone (vias kept as cells) did not
change the antenna deck's CPU time in single-sample diagnostics; flattening
the vias did. The latch-up check reads well, diffusion and tie geometry, which
the export does not change. All checks pass on every stream.

## Layer map profile (`rustroad.gds-layer-map.v1`)

Tracked profiles: `fixtures/pdk/sky130-gds-map.json`,
`fixtures/pdk/gf180-gds-map.json`.

```json
{
  "schema": "rustroad.gds-layer-map.v1", "pdk": "sky130A", "library": "sky130_fd_sc_hd",
  "sources": [{"id": "klayout_map", "role": "...", "path": "...", "sha256": "<64 hex>", "bytes": 1676}],
  "layers": [
    {"lef": "met1", "kind": "routing",
     "net": [[68, 20]], "special_net": [[68, 20]], "via": [[68, 20]],
     "pin": [[68, 20], [68, 16]], "label": [68, 5],
     "cite": [{"source": "klayout_map", "sha256": "<same>", "lines": [28, 29, 31]},
              {"source": "magic_tech", "sha256": "...", "lines": [1203, 1204, 1206, 1208, 1210, 1212]}]},
    {"lef": "via", "kind": "cut", "via": [[68, 44]], "cite": [...]}
  ],
  "boundary": {"layer": [235, 4], "cite": [...]},
  "notes": ["..."]
}
```

Rules (`LayerMap::validate`, unknown fields rejected): routing layers need
non-empty `net`, `special_net`, `via`; cut layers only `via`; at most 8 distinct
targets per purpose; every entry and the boundary cite at least one listed
source with a matching sha256 and 1..32 line numbers. `export-gds` also checks
the LEF layer TYPE against `kind` (`GDS_LAYER_KIND`). `scripts/verify_gds.py`
re-derives every claim from the cited KLayout LEF/DEF map (purposes NET, SPNET,
VIA, PIN, NAME .../LABEL, DIEAREA, and that all relevant lines are cited) and
requires a cited Magic `calma <layer> <datatype>` line for each target.

## Library APIs

- `rustroad_formats::gds`: `Real8` (exact `from_f64`, correctly rounded
  `to_f64`, `parts`, `is_exact_f64`), `RecordReader`, `parse_library(bytes,
  &GdsLimits, retain_elements) -> Library`, `Library::{closure, index,
  libname}`, `GdsWriter::{begin_library, begin_structure, rectangle, boundary,
  sref, text, element, raw, end_structure, end_library, into_inner}`,
  `write_library`, `dbu_per_micron`. Reader codes: `GDS_TRUNCATED`,
  `GDS_SYNTAX`, `GDS_UNSUPPORTED`, `GDS_BUDGET`, `GDS_NAME`, `GDS_GEOMETRY`,
  `GDS_DUPLICATE_STRUCTURE`, `GDS_MISSING_STRUCTURE`, `GDS_REFERENCE_CYCLE`,
  `GDS_REAL`, `GDS_RECORD_LENGTH`.
- `rustroad_formats::gdsmap`: `LayerMap`, `LayerEntry`, `GdsLayer`, `Purpose`,
  `LayerKind`, `SCHEMA`.
- `rustroad_workspace::gds`: `export_gds(&Path, &ExportOptions, &Limits) ->
  Result<GdsReport>`, `ExportOptions::new(cell_gds, layer_map, out)`, `Scale`,
  `FIXED_DATES`, `GDS_VERSION` (600), `VIA_CELL_PREFIX` (`VIA_`).
- `rustroad_workspace::Error::Gds { code, message }` (new coded variant).

## Verification

```sh
python3 -I scripts/verify_gds.py --run .work/<new dir> \
  --source .work/rc-timing/ready --rustroad target/release/rustroad
```

The harness copies the routed workspaces (`<pdk>_physical`) of a finished
flow run and runs every third-party tool only as a black box in the pinned
image `4c48415d8bd0` (KLayout 0.30.9, Magic 8.3.678, Netgen 1.5.323). Exit
codes are not trusted: reports are parsed, the Magic top cell must exist, both
DRC decks must report 0 on the native GDS, and a planted 0.1 x 0.1 um square on
the wire layer must raise both counts (positive control). Every DEF `RECT`
patch must be streamed on its layer's net purpose, and deleting those
rectangles must make the XOR fail. LVS runs the basic Netgen/KLayout checks and
both LVS checks of `scripts/signoff` (named-net and port pairing).

Negative controls (each must be detected):

- deleted signal wire (the largest signal wire, a DEF NETS path): XOR
  difference, and Netgen and the signoff KLayout check fail; the basic
  KLayout check alone still says match, which is why the signoff checks are
  required;
- wrong map entry (met2/Metal2 `net` datatype changed): XOR differences on
  the drawing layer plus a spurious layer; the citation verifier rejects it;
- missing cell structure: removed from the output -> XOR differences on all
  cell layers; removed from the cell library -> `GDS_CELL_MISSING`, exit 2,
  no output file;
- patches removed: XOR differences only on the patch layers;
- planted sub-minimum square: both decks count it;
- SKY130 density without the 235/4 boundary -> non-finite densities,
  reported failed (not a silent pass);
- odd GF180 coordinate (a Via1_HV cut edge planted in a workspace copy):
  refused by the workspace reader (`INVALID_WORKSPACE`, LEF-rule via
  enclosure / supply via grid) before streaming, or by the exporter
  (`GDS_ODD_COORDINATE`); exit 2, no output file.

KLayout deep-mode items are counted over every cell. Density is reported
separately and is never part of the DRC verdict (owner decision). The
per-run tables of the stream b0 requalification and of the earlier phase-A
run are in `docs/evidence/gds-verification.json`, kept as historical
evidence for reproducibility, not as current results.

Rust tests cover the GDS codec round trip, exact real8 vectors incl. the PDK
UNITS bytes and round-to-even decoding, budgets, every-prefix truncation,
2,000 single-bit mutations, grammar violations, closure depth/cycles, unit
grids, layer-map validation, FOREIGN parsing, and a routed self-authored
workspace: determinism, read-only, FS/N transforms, labels, caps, missing
cells, collisions, wrong maps, FOREIGN mismatch, changed LEF, 2000-DBU exact
division and odd-coordinate rejection. Ignored real-PDK tests (run with
explicit paths) parse both PDK cell libraries and re-encode them
byte-identically through the typed model, check that every cell-LEF FOREIGN
binding names its own macro at offset 0, and stream verbatim cells from
copies of both routed workspaces. Strict Clippy (`-D warnings`) and rustfmt
are part of the checks.

## Limitations

- Only N and FS rows exist in RustROAD; other orientations are not emitted.
- The exporter does not repair DRC; it streams exactly what was routed.
- Labels for escaped Verilog identifiers are rejected rather than guessed.
- The workspace loader does not re-verify stored technology VIA geometry against
  its LEF byte for byte. With LEF-rule routing it does re-derive the routing
  vias (cut ENCLOSURE, variant families) and the supply vias (manufacturing
  grid), which now refuses the planted odd-coordinate via. Export re-verifies
  the cell LEF (FOREIGN) by sha256 only.
- Netgen/KLayout LVS references are RustROAD's own powered export; the
  independent RTL-to-powered CEC link is a separate work item (W10).
