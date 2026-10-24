# Native GDSII streamout (`export-gds`)

Status: implemented and black-box verified on the routed pipeline fixture for
SKY130 (`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`). Scope is a
block/macro: no pad ring, no seal ring; IR/EM/SI/OCV/CPPR are unchecked.
`export-gds` itself reports `drc_status: "unchecked"`; the foundry checks run
in the harnesses below. On the integrated LEF-rule flow the native GDS is DRC
clean (Magic 0, KLayout 0) and LVS-matched on both PDKs; Sky130 has one Magic
antenna item (see `docs/requalification.md`). Resource contract:
`docs/gds-budget.md`. Evidence: `docs/evidence/gds-verification.json`.

## Command

```sh
rustroad --workspace W export-gds \
  --cell-gds <PDK standard-cell .gds> \
  --layer-map fixtures/pdk/<pdk>-gds-map.json \
  --out <file.gds> \
  [--cell-lef <cell LEF; must match the imported sha256>] \
  [--max-gds-bytes 268435456] [--max-gds-records 16000000]
```

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
  supply_vias, pin_shapes, labels, boundary}`, `routing_status`,
  `drc_status: "unchecked"`, `lvs_status: "unchecked"`, `scope:
  "block_level_no_pad_or_seal_ring"`, `workspace_mutated: false`.
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
| `VIA_<name>` | One structure per used fixed LEF VIA (routed + supply), its RECT/POLYGON shapes on the map's `via` targets. Name collisions with any cell-GDS structure or the top: `GDS_NAME_COLLISION`. |
| Instances | SREF per instance in instance order. Row `N`: no STRANS at (x, y). Row `FS`: STRANS x-reflection at (x, y + height). |
| Routed wires | DEF NETS semantics: layer default `WIDTH`, half-width extension at both ends (exact `wire_rect`). One BOUNDARY per maximal collinear same-net run on `net` targets, the same runs `export-def` writes (`rustroad_core::wire_runs`): stored lattice records that touch or overlap on one track line are merged, and a run is split only at a strictly interior via landing (lower or upper metal), pin-access point or same-layer branch. The metal union equals the stored records'. |
| Routed / supply vias | SREF to `VIA_<name>` at the via point. Every used LEF via variant (LEF-rule routing, e.g. `M1M2_PR_MR`, `Via1_VH`, `Via2_HH`) gets its own structure. |
| Routing patches | Every `Routes.patches` rectangle (DEF routing `RECT`, same-net notch/corner fill) as one BOUNDARY on the routing layer's `net` targets. |
| Supply rails/spines | SPECIALNETS geometry (explicit width, half-width extension) on `special_net` targets. |
| Pins | I/O and supply pin rectangles on every `pin` target; one TEXT per pin at the rectangle centre on the `label` target. Bus bits are labelled `a[1]`; supply ports `VDD`/`VSS`. Escaped/non-printable names: `GDS_LABEL_UNSUPPORTED`. |
| PR boundary | Die area on the map's boundary layer (SKY130 235/4, GF180 0/0). |

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

Requalification run (stream b0, 2026-10-24; `.work/runs/gds5` on the
policy-producer LEF-rule flow `.work/runs/r2`, binary `ff523a9d...`, 189.7 s
wall, largest container 464 MiB):

| Check | SKY130 | GF180 |
|---|---|---|
| strmxor native vs KLayout LEF/DEF reference | No differences found | No differences found |
| Top labels / flattened texts equal | 12 / 668 equal | 12 / 1056 equal |
| PDK cell structures byte-identical | 10 / 10 | 10 / 10 |
| Re-export and independent-copy export identical | yes (`48cdfd7e...`, 65,110 B, 5,176 records) | yes (`d2c20a1e...`, 46,368 B, 3,768 records) |
| Routing patches streamed (DEF `RECT`) | 8 / 8 | 1 / 1 |
| Magic drc(full), KLayout main deck (native = reference) | 0, 0 | 0, 0 |
| Planted sub-minimum square detected (Magic / KLayout) | 2 / m2.1 x2, m2.6 x1 | 2 / M2.1 x2, M2.3 x1 |
| Density (separate) | passed, 0 items | failed, 8 items (no fill) |
| LVS: Netgen, KLayout, signoff Netgen, signoff KLayout | match x4 (native and reference) | match x4 (native and reference) |
| Layer-map citations | 36 claims, 0 problems | 30 claims, 0 problems |

Controls (all detected): deleted signal wire (port `z[1]`; XOR 1 shape; Netgen
and the signoff KLayout check fail, the basic KLayout check still says match,
which is why the signoff checks are now required), wrong map entry, missing
cell structure, patches removed (XOR only on the patch layers: Sky130
68/20 x3, 69/20 x5, 70/20 x1; GF180 36/0 x2), planted DRC square, Sky130
density without the boundary, GF180 odd coordinate (now refused by the
workspace reader, `INVALID_WORKSPACE`, before streaming; no output file).

Phase-A run on the earlier router (historical: `.work/gds/final`, binary sha256 `7c34a6d3...acce`, 109.6 s wall
for both PDKs, largest external step 474 MB RSS; evidence written with
`--evidence docs/evidence/gds-verification.json`):

| Check | SKY130 | GF180 |
|---|---|---|
| strmxor native vs KLayout LEF/DEF reference | No differences found | No differences found |
| Top labels / flattened texts equal | 12 / 677 equal | 12 / 1056 equal |
| PDK cell structures byte-identical | 8 / 8 | 10 / 10 |
| Re-export and independent-copy export identical | yes (sha256 `b714fdf1...`) | yes (sha256 `e877c9f6...`) |
| Workspace read-only (HEAD rev6 + tree hash) | unchanged | unchanged |
| Magic drc(full) total, native = reference | 26 = 26 | 13 = 13 |
| KLayout main deck, native = reference | m1.2 x5, m2.2 x4 | M1.2a x4, M2.3 x3 |
| Density deck (separate `density_status`) | passed: 0 items, finite (li1 0.94 ... m5 1.0) | failed: 8 items (DCF.1b, PL.8, M1.4-M5.4, MT.3) |
| Netgen LVS native / reference | match / match | match / match |
| KLayout LVS native / reference | match / match | match / match |
| Layer-map citations re-derived | 36 claims, 0 problems | 30 claims, 0 problems |

The KLayout GF180 M1.2a items are attributed 3 to `lane_smoke` and 1 to the
`clkbuf_1` child cell; counts use every cell. Density is reported separately and
is never part of the DRC verdict (owner decision); the GF180 block has no fill.

Negative controls (all detected):

- deleted wire (largest met2/Metal2 segment removed): strmxor 1 difference on
  69/20 (36/0), Netgen and KLayout LVS mismatch;
- wrong map entry (met2/Metal2 `net` datatype 21/1): strmxor 27 (37) differences
  on the drawing layer plus a spurious layer; citation verifier rejects it;
- missing cell structure: removed from the output -> strmxor differences on all
  cell layers; removed from the cell library -> `GDS_CELL_MISSING`, exit 2, no
  output file;
- odd GF180 coordinate (Via1_HV cut edge -261 LEF DBU planted in a workspace
  copy) -> `GDS_ODD_COORDINATE`, exit 2, no output file;
- SKY130 density without the 235/4 boundary -> non-finite densities, reported
  failed (not a silent pass).

Native export cost: SKY130 0.04 s / 16.0 MiB peak RSS, GF180 0.02 s / 10.1 MiB
(374,534 / 359,540 bytes; 29,286 records for SKY130). These are single small
fixtures, not a scaling claim.

Rust tests: 97 ordinary tests pass (18 new: GDS codec round trip, exact real8
vectors incl. the PDK UNITS bytes and round-to-even decoding, budgets,
every-prefix truncation, 2,000 single-bit mutations, grammar violations,
closure depth/cycles, unit grids, layer-map validation, FOREIGN parsing, and a
routed self-authored workspace: determinism, read-only, FS/N transforms,
labels, caps, missing cells, collisions, wrong maps, FOREIGN mismatch, changed
LEF, 2000-DBU exact division and odd-coordinate rejection). Ignored real-PDK
tests (run with explicit paths) also pass: both PDK cell libraries parse and
re-encode byte-identically through the typed model, all 437/229 cell-LEF
FOREIGN bindings name their own macro at offset 0, and copies of both routed
workspaces stream verbatim cells. Strict Clippy (`-D warnings`) and rustfmt are
clean. Full test run: 5.9 s, 461 MB peak (cargo + tests).

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
