# Block-level foundry signoff harness

This harness runs foundry DRC, antenna, density and LVS for RustROAD layouts on
SkyWater 130 (`sky130_fd_sc_hd`, sky130A) and GlobalFoundries 180
(`gf180mcu_fd_sc_mcu7t5v0`, gf180mcuD). The tools run as black boxes in one
pinned podman image. A verdict comes only from parsed result files and
preflight assertions. Tool exit codes are never evidence: KLayout DRC, the
gf180 `run_lvs.py` and Netgen all exit 0 on failure.

## Scope

- Only block/macro-level signoff. There is no pad ring and no seal ring;
  `seal=false` is passed to the sky130 deck.
- Density is reported on its own as `density_status`. It is never folded into
  the block DRC verdict, and it reads `passed` only when every density deck
  really passes. At block size:
  - the sky130 windowed (700 µm) min/max density check refuses to run, so the
    result is `unverified`;
  - the gf180 global coverage rules fail, so the result is `failed` with the
    real item counts.
- IR drop, EM, SI, OCV and CPPR are outside this harness, and the flow reports
  them as `unchecked`.
- The LVS reference is RustROAD's own powered Verilog export (`export-verilog
  --include-power`). Two CEC checks in `verify_routing.py` tie it to the RTL:
  Yosys `equiv_*` and `rustsys check synth`. It is still not an independently
  produced netlist.
- The GDS used by the baseline comes from KLayout's LEF/DEF reader. It is a
  black-box reference streamout, not the native RustROAD GDS writer (another
  stream builds that). `verify_routing.py --signoff-gds` gates a native GDS on
  an XOR match against this reference.

## Tool and PDK pinning (W1)

`fixtures/flow/signoff-pinned.json` (schema `rustroad.signoff-manifest.v1`)
records the following.

- Podman images, each with its id and RepoDigests:
  - `docker.io/hpretl/iic-osic-tools`, id
    `4c48415d8bd08c04033f4777b65a96299e22c1e94d12a8ea1d9a062697c006ee`,
    digests `sha256:344fadc6…` and `sha256:5d6adf1f…`,
    `IIC_OSIC_TOOLS_VERSION=2026.07`.
  - `docker.io/openroad/orfs`, id `f152917ebc45…`, used for OpenRCX/OpenROAD.
- In-container tools, each with its executable, library sha256, a tree digest
  and the probed version:
  - Magic 8.3.678
  - Netgen 1.5.323
  - KLayout 0.30.9 and strmxor
  - python 3.12.3 with docopt 0.6.2 and the klayout module 0.30.9
  - coreutils `time`/`timeout`
- OpenROAD, by sha256 inside the orfs image.
- OpenSTA 3.1.0, by its host path and sha256.
- The sha256 and byte size of every PDK file the flow uses, for both PDKs:
  - tech LEF, cell LEF and cell GDS;
  - Liberty for tt/ss/ff (sky130 `tt_025C_1v80`, `ss_100C_1v60`,
    `ff_n40C_1v95`; gf180 `tt_025C_3v30`, `ss_125C_3v00`, `ff_n40C_3v60`);
  - SPICE and CDL;
  - magicrc, Magic tech and tcl, and the Netgen setup;
  - the KLayout DRC, density and LVS decks, plus the gf180 deck trees;
  - `.map`, `.lyp` and `.lyt`;
  - the OpenRCX min/nom/max rules (sky130 uses the `.calibre` variants, as the
    existing harness does);
  - the synthesis and PnR exclusion lists, and the LibreLane config files.

  Every file is hashed inside the image and at the host overlay path, and the
  two must agree.
- The host tools `/usr/bin/magic` 8.3.105, `/usr/bin/klayout` 0.30.0 and
  `/usr/lib/netgen/bin/netgen` 1.5.133, marked `not_for_signoff` with the
  reason. The same three entries in `fixtures/flow/local-pinned.json` carry a
  `not_for_signoff` reason too, so `rustroad flow-check` reports them as
  `not_for_signoff` and keeps the gds_streamout, drc and lvs stages
  `unverified` (`tool_<id>_not_for_signoff`). Host KLayout is still used by
  `verify_routing.py`, but only for the RustROAD subset routing-geometry
  checker, which is not a signoff verdict; the summary labels it that way.

```sh
python3 scripts/signoff/manifest.py generate --work .work/manifest-gen   # regenerate (review the diff)
python3 scripts/signoff/manifest.py verify --out .work/identity           # exit 0 verified, 3 unverified
```

`manifest.verify()` runs at the start of every signoff invocation. These
problems make every verdict `unverified` and stop the tools from running:

- any mismatch;
- a missing image, tool file or PDK file;
- a changed RepoDigest;
- a failed version probe.

The two demonstrations below both give exit 3:

- a tampered gf180 DRC-deck hash makes all six checks `unverified`;
- a renamed Magic executable plus an unknown orfs image id makes the check
  `unverified`.

## Container wrapper

`scripts/signoff/container.py run --out DIR --label L [--ro HOST:CONTAINER]... [--memory 4g] [--cpus 4] [--timeout 900] -- CMD...`

Also `container.py inspect [--image ID]`, which prints the podman identity, and
`container.py probe --out DIR [--pdk sky130|gf180]`, which runs the in-image
identity probe against the manifest and exits 0 when verified, 3 when
unverified.

Each run is wrapped as follows:

- **Network and filesystem:** `--pull=never --network=none --read-only`, with
  a `/tmp` tmpfs of 512 MiB.
- **Privileges:** `--user=0:0` (rootless; files land owned by the host user),
  `--cap-drop=all` and `no-new-privileges`.
- **Resource caps:** `--memory=--memory-swap` (no extra swap),
  `--cpus`, `--pids-limit` and podman `--timeout`.
- **Time and memory receipt:** the command runs under an inner coreutils
  `timeout` and `/usr/bin/time -v`, which gives the peak-RSS receipt. A
  host-side subprocess timeout, followed by `podman rm -f`, is the backstop.
- **Fixed environment:** `LC_ALL=C TZ=UTC HOME=/tmp PDK_ROOT=/foss/pdks
  PYTHONHASHSEED=0 SOURCE_DATE_EPOCH=0 QT_QPA_PLATFORM=offscreen` and a fixed
  PATH. No host variables are passed in.
- **Mounts:** only explicitly listed read-only paths plus one writable
  `/out`. Broad host directories, `/tmp` output and mounts that shadow image
  paths are refused.

Each receipt `receipts/<label>/receipt.json` records:

- the podman argv and mounts;
- the limits;
- the exit code;
- wall time and peak RSS;
- the stdout/stderr paths.

## Signoff checks (W2)

```sh
python3 scripts/signoff/signoff.py CHECK --gds LAYOUT.gds --top CELL --pdk sky130|gf180 --out DIR \
    [--netlist POWERED.v] [--against OTHER.gds [--against-top CELL]] \
    [--manifest fixtures/flow/signoff-pinned.json] [--threads 4] [--memory 4g] [--timeout 900]
# CHECK: drc-magic | drc-klayout | antenna | density | lvs-netgen | lvs-klayout | xor | all
```

**Preflight (all checks).** A KLayout probe (`kl_probe.py`) checks the
following. A violation sets the status to `failed`.

- The requested top cell exists. Magic would otherwise silently create an
  empty cell.
- The requested cell is a top cell of the GDS, and the GDS has exactly one top
  cell. A library cell inside the GDS given as `--top` (e.g. `and2_2`, which
  in gf180 even carries its own 0/0 boundary) used to pass Magic DRC, antenna
  and Netgen LVS; a stray second top cell would never be checked at all.
- The top cell's bbox is non-empty.
- The PR boundary is drawn in the top cell itself: sky130 235/4, gf180 0/0.
  This matters because gf180 standard cells carry their own 0/0.
- The GDS dbu is 0.001 µm.

The probe also lists every text label placed directly in the top cell
(`top_texts`: layer, string, position in integer picometres) and writes every
text of the flattened hierarchy to a sorted list with its sha256 (used by the
XOR label comparison). Geometry outside the PR boundary is reported
(`geometry_outside_boundary`) but not failed, because standard-cell wells
legitimately overhang the row boundary.

**Port-label oracle (both LVS checks).** Independently of either LVS tool, the
set of top-cell label strings must equal the set of reference-netlist ports,
both ways (`result.port_labels`): every port labelled, every top label a port.
Its failures are prefixed `port labels:` so they stay distinguishable from the
tool's own findings.

### Checks

**drc-magic**

- Tool and settings: `magic -dnull -noconsole -rcfile <pdk magicrc>
  magic_drc.tcl`. The tcl sets `drc euclidean on; drc style drc(full)` and
  echoes the style back, which must read `drc(full)`.
- Reported counts:
  - the hierarchical `drc list count total`;
  - error boxes per rule;
  - a recheck on a flattened copy of the top cell, which gives the
    context-correct count across all cells;
  - each child cell checked on its own, kept as `standalone_cell_errors`
    (informational).
- Failed when either total is greater than 0. `unverified` when `RR_DONE` or
  the totals are missing.

**drc-klayout**

- Tool and settings:
  - sky130: `sky130A_mr.drc` with `feol beol offgrid = true`, `floating_met
    seal = false` and `top_cell`. The deck is GPLv3 and is only invoked.
  - gf180: `gf180mcu.drc` with `variant=gf180mcuD decks=all,-antenna,-density
    run_mode=deep topcell`.
- Report path: always absolute.
- Pass/fail: lyrdb items are counted per rule and per cell across all cells,
  because deep mode attributes some items to child cells. `unverified` when
  the report is missing or stale, the log has errors, or the report's top
  cell is wrong.

**antenna**

- sky130: Magic `antennacheck` is the primary oracle, after `extract do local;
  extract no all; extract all`. KLayout is `not_available`, since the sky130
  KLayout decks contain no antenna rules.
- gf180: the KLayout `decks=antenna` deck is the primary oracle. Magic is the
  secondary; its gf180 rules are marked incomplete, so a clean Magic result
  is not evidence.
- Fails on any violation from either tool.

**density**

- sky130:
  - `met_min_ca_density.lydrc`: items, plus the printed densities, which must
    be finite and in [0,1]. NaN or -Infinity appear without 235/4.
  - The PDK `check_density.py` 700 µm windowed check. Below window size it
    refuses, which gives `unverified`.
- gf180: `gf180mcu.drc decks=density`, counting the global coverage items.
- Counts are per deck (`counts.by_deck`). `counts.total`, and the summary
  count, are `null` unless every density deck gave a definitive count, so the
  sky130 result is never reported as 0. More generally, every summary count is
  `null` for an `unverified` verdict.

**lvs-netgen**

- Step 1: Magic `extract do local; extract all; ext2spice lvs`, run
  hierarchically.
- Step 2: `netgen -batch` reads the PDK cell SPICE, then the powered Verilog
  on top of it, with the PDK setup file.
- The reference Verilog must define exactly one module, named `--top`.
  Otherwise Netgen silently compares against whatever else carries that name
  (for a child-cell top, the PDK SPICE subckt of the library cell).
- Pass requires every comp.json entry clean (`badnets`, `badelements`,
  `properties`, device multisets, net counts, pin sets), the top circuit
  compared, the final result exactly `Circuits match uniquely.`, and none of
  these lines anywhere in comp.out: `property errors`, `port errors`,
  `(no matching pin)`, `disconnected node`.
- Port errors matter. A labelled floating metal pin that is not in the
  reference ends with `Netlists match uniquely with port errors.`, the pin
  table shows `extra |(no matching pin)`, `Cell lane_smoke (0) disconnected
  node: extra` is printed, and the final line still says `Circuits match
  uniquely.`; comp.json drops the pin from both lists. That is a failure here.
- Magic extraction is evidence of the same flow: Magic's `Ports "A" and "B"
  are electrically shorted` warning fails the check, and the extracted
  top-level `.subckt` port list must equal the reference port set. On gf180 a
  second label on an existing port net (`en_alias` on `en`) is merged by
  Magic, so Netgen itself matches; the shorted-port warning catches it.
- Device properties matter. A drive-strength swap with the same topology
  (sky130 `and2_2` -> `and2_4`) ends with `Final result: Circuits match
  uniquely.` followed by `Property errors were found.`, and comp.json lists
  the width mismatches under `properties`. That is a failure here.

**lvs-klayout**

- Reference: `refnet.py` builds a SPICE (sky130) or CDL-subset (gf180)
  reference from the powered Verilog plus the PDK cell subcircuits.
  Device-less tap/fill cells are dropped, and sky130 `w=650000u` becomes
  `0.65u`.
- sky130: `sky130.lvs` with `convert_subckts combine schematic_simplify
  top_lvl_pins = true`.
- gf180: `run_lvs.py --variant=D --run_mode=deep --top_lvl_pins`.
- Pass requires every circuit pair in the `.lvsdb` cross-reference to be
  `Match`, the top-pin counts to be equal, and, for gf180, `Congratulations!
  Netlists match.` in the log.
- Pin names are compared by the harness. KLayout pairs pins by topology, so
  swapped labels on the symmetric `a[0]`/`a[1]` bits still report `Match`
  (and the gf180 deck still prints its congratulations). `kl_lvsdb.py` dumps
  every pin pair. On the top circuit every pair must join a layout pin and a
  schematic pin of the same name (case-insensitive, since the SPICE reader
  upper-cases), every top pin must be named, and names must be unique. In
  library cells a named pin pair must also agree, except the body pins the
  decks synthesize (`LVS_BODY_PINS`: sky130 `sky130_gnd`<->`VNB`, gf180
  `SUB`<->`VPW`); unnamed deck pins (`$n`) are only counted.
- Labelled nets are compared by the harness. KLayout leaves a labelled
  floating net (an extra pin label not in the schematic) out of the
  comparison and still reports `Match` with 12/12 pins. `kl_lvsdb.py` dumps
  every named (non-`$`) net of the layout top circuit with its pairing; each
  must be paired, `Match`, with the equally named schematic net.
- Reference ports are compared by the harness. sky130 `schematic_simplify`
  drops an unconnected reference port, so the post-simplify pin counts are
  equal (12 = 12) although the reference has 13 ports. Every port of the
  reference Verilog must be a paired top-level schematic pin, and every paired
  schematic pin a reference port (case-insensitive).

**xor**

- Tool: `strmxor --layer-details --top-a --top-b a.gds b.gds xor.gds`.
  Without `--layer-details`, layers present in only one file are ignored,
  which is a false pass.
- Pass requires both `No differences found` and an empty difference table.
- strmxor compares polygons only; text labels are compared separately and
  exactly: the top-cell labels as a multiset of (layer, string, x, y), and
  the flattened label lists of both layouts by sha256 with a bounded streaming
  diff when they differ (`result.labels`). A renamed port label (`en` ->
  `enx`, geometry identical) is therefore an XOR failure, `counts.labels` 2.
- Hierarchy facts (cell counts, top instances) are reported.

`all` runs every check except `xor` (which needs `--against`) and writes
`signoff.json` with these fields:

- the per-check `statuses`;
- `drc_status`, the worse of Magic and KLayout DRC;
- `lvs_status`;
- `antenna_status`;
- the separate `density_status`;
- `block_signoff_status`, which is DRC + antenna + LVS (+ XOR) and excludes
  density;
- `counts`, `by_rule` and per-check peak RSS.

### Verdict JSON (`rustroad.signoff-verdict.v1`, one file per check)

```json
{"schema": "rustroad.signoff-verdict.v1", "check": "drc-klayout", "pdk": "gf180",
 "status": "passed|failed|unverified", "failures": ["..."], "unverified_reasons": ["..."],
 "input": {"gds": {"path": "/abs", "sha256": "...", "bytes": 1}, "top_cell": "lane_smoke",
           "reference_netlist": null, "against": null, "against_top": null},
 "preflight": {"dbu_um": 0.001, "bbox_um": [0, 0, 70, 70.56], "boundary_layer": [0, 0],
               "boundary_shapes_in_top": 1, "top_cells": ["lane_smoke"], "top_labels": ["VDD", "..."],
               "assertions": {"top_cell_exists": true, "top_is_a_top_cell": true, "single_top_cell": true,
               "bbox_nonempty": true, "boundary_present": true, "dbu_expected": true}},
 "identity": {"status": "verified", "manifest": "...", "manifest_sha256": "...", "mismatches": []},
 "tools": {"klayout": {"version": "KLayout 0.30.9", "files": {"/foss/tools/klayout/klayout": "sha256"}}},
 "options": {"decks": [{"deck": "/foss/pdks/...", "options": {"variant": "gf180mcuD"}}]},
 "counts": {"total": 7, "by_rule": {"M1.2a": 4, "M2.3": 3}, "by_cell": {"lane_smoke": 6,
            "gf180mcu_fd_sc_mcu7t5v0__clkbuf_1": 1}, "unit": "lyrdb items, all cells"},
 "result": {}, "reports": {"drc.lyrdb": "/abs/.../drc.lyrdb", "drc.lyrdb.log": "/abs/..."},
 "runs": [{"label": "drc_klayout_klayout_0", "exit_code": 0, "wall_seconds": 2.8, "peak_rss_kib": 445596,
           "receipt": "/abs/.../receipt.json"}],
 "scope": {"level": "block/macro", "density": "reported separately ...", "ir_drop": "unchecked"},
 "resources": {"wall_seconds": 3.0, "tool_wall_seconds": 2.9, "peak_rss_kib": 445596}}
```

Check-specific `result` fields:

- `lvs-netgen`: `final_result`, `problems` (Netgen's own findings),
  `property_errors`, `port_error_lines`, `unmatched_pins`,
  `disconnected_nodes`, `extraction {shorted_ports, extracted_top_ports,
  layout_only_ports, reference_only_ports, problems}`, `reference_modules`,
  `port_labels`.
- `lvs-klayout`: `problems` (from the lvsdb cross-reference), `top_pins
  {pairs, name_mismatches, unnamed}`, `top_nets {named, unpaired,
  mismatched}`, `reference_ports {reference, unpaired_reference_ports,
  schematic_pins_not_ports}`, `child_pins`, `port_labels`.
- `xor`: `layer_differences`, `labels {status, top, flat {only_in_design,
  only_in_against, samples_*, design_sha256, against_sha256}}`; `counts` has
  `geometry` and `labels`.

The status is `failed` when there is any definite failure: a rule item, a
mismatch or a preflight violation. Otherwise it is `unverified` when any
evidence is missing: identity, report, completion marker, finite densities.
Otherwise it is `passed`. Exit codes are 0 for passed, 1 for failed and 3 for
unverified.

- **Counts.** Whenever a verdict has any unverified reason, every count in
  `counts` is `null` (never 0), in the per-check file as well as in the
  summary. A clean, complete run keeps its 0.
- **Killed tools.** A tool run that exited 124 (timeout), 125-127 (podman
  could not start it) or 128+N (killed by signal N, e.g. 137 for the OOM
  killer) is not evidence: the verdict is `unverified` with the reason, even
  when partial output exists. Exit codes 0/1 are recorded, never trusted.
- **Manifest completeness.** Before anything is probed, the manifest must hold
  every file, tree, tool and image entry the harness uses, with paths and
  roots equal to the generator's spec, and the image ids the harness actually
  runs. A missing entry gives `unverified` (exit 3) for every check.
- **Harness errors.** Any unexpected exception writes an `unverified` verdict
  for each requested check (with the traceback) and exits 3; a crash never
  produces exit 1, which means `failed`.

## Reference streamout

```sh
python3 scripts/signoff/def2gds_reference.py --def ROUTED.def --pdk sky130|gf180 --out OUT.gds
```

This runs KLayout's LEF/DEF reader in the pinned image with:

- the PDK tech LEF, cell LEF and `.map`;
- `macro_resolution_mode=2`, so every master is resolved to its PDK GDS
  structure and the hierarchy (SREFs) is kept;
- dbu 0.001 µm;
- no GDS timestamps, so repeated runs are byte-identical.

For gf180, the 2000 DBU/µm DEF is first checked for odd coordinates (none in
20,562 checked values). Odd coordinates cannot be represented exactly and
would be rounded silently. After the run, the script checks that:

- the top cell equals DEF `DESIGN`;
- the dbu is 0.001;
- every DEF master resolved to a PDK structure;
- the boundary layer is present.

The output is XOR-identical to the earlier host-KLayout 0.30.0 streamout. It
is the XOR oracle for the native writer.

## Baseline (copies of `.work/rc-timing/ready`)

The recon baseline was reproduced exactly.

| | Sky130 | GF180 |
|---|---|---|
| KLayout DRC | 9 items: m1.2 ×5, m2.2 ×4 (all in `lane_smoke`) | 7 items: M1.2a ×4 (1 in `clkbuf_1`), M2.3 ×3 |
| Magic DRC (`drc(full)`) | 26 errors (hierarchical = flattened); boxes met1.2 25, met2.2 16 | 13 errors (hierarchical = flattened); boxes M2.3 10, M1.2a 6 |
| Antenna | Magic 0 (KLayout: no sky130 rules) | KLayout 0 (Magic secondary 0) |
| Density (separate) | `unverified`: max-density deck 0 items, finite (li1 0.94, m1 0.91, m2 0.98); 700 µm window check not runnable | `failed`: 8 global items (DCF.1b, PL.8, M1.4, M2.4, M3.4, M4.4, M5.4, MT.3) |
| Netgen LVS | `Circuits match uniquely.` (13 logic cells, 21 nets) | match |
| KLayout LVS | all circuit pairs Match, 12/12 top pins | Match, `Congratulations! Netlists match.` |

Magic's "26" and "13" are its error-tile totals. The per-rule numbers are
error boxes, so they add up to more than the total. Each child cell checked
on its own also shows standalone errors: sky130 LU.2, LU.3 and nwell.4 (the
cells have no taps), and min-area on the via cells. The flattened recheck
gives the same 26/13 with the same rules, which shows these standalone errors
are satisfied in context.

## Planted controls

Each control is planted with `kl_plant.py` into a copy of the reference GDS.
Every one is detected on both PDKs (`controls.py`; all 23 entries per PDK are
as expected). Each LVS control must be caught by each LVS flow on its own
(Netgen: comp.out/comp.json and its Magic extraction; KLayout: the lvsdb
cross-reference), and for the port/label controls the harness port-label
oracle must fail as well; the flow gate needing both tools is not counted as
detection.

| Control | What is planted | Detection |
|---|---|---|
| spacing | two metal1 rects 0.10 µm (sky130) / 0.12 µm (gf180) apart in an empty region | KLayout m1.2 / M1.2a +1 and Magic met1.2 / M1.2a boxes increase |
| xor (negative) | the spacing variant vs the reference | strmxor shows 2 shapes on 68/20 / 34/0 → `failed` |
| offgrid | a metal1 edge 2 nm off the 5 nm grid | KLayout `m1_OFFGRID` / `metal1_OFFGRID` ×2. Magic snaps the input and does not flag it |
| short | met3 bridge between the `a[1]` and `clk` pins | Netgen `Top level cell failed pin matching`; KLayout top `NoMatch`, pins 11 ≠ 12 |
| missing via | delete the first M1M2_PR / Via1_HV of net `a[0]` (located from the DEF) | Netgen `Netlists do not match`; KLayout `NoMatch` |
| antenna | gate-only met3 wire on the `a[1]` pin (sky130 600 µm, gf180 2000 µm) | sky130 Magic metal3 ×1; gf180 KLayout ANT.16_ii_ANT.4 ×8, and Magic also reports ratio 882 > 400 |
| wrong top cell | `--top wrong_top` | all six checks not passed; Magic tcl reports `RR_TOP_MISSING` |
| missing report | relative KLayout report path (KLayout exits 0, "Unable to open file") | drc-klayout `unverified` (report not written) |
| missing 235/4 (sky130) | remove the PR boundary | densities −Infinity/NaN → density not passed; boundary assertion fails |
| GF180 dbu 0.0005 | geometry-preserving rescale | deck `DBU` ×1 plus the dbu assertion |
| label swap | exchange the `a[0]` and `a[1]` port labels in the top cell | Netgen `Netlists do not match`; KLayout circuit status is still `Match`, but the pin-name check reports `a[0]<->A[1], a[1]<->A[0]` → `failed` |
| missing label | delete the `a[0]` port label | Netgen pin matching fails; KLayout pins 11 ≠ 12 and an unnamed top pin pair |
| cell swap | reference netlist `g0` and2_2 → and2_4 (same function, other drive) | sky130 Netgen: topology matches, 6 width property errors → `failed`; gf180 Netgen pin matching fails; KLayout `NoMatch` on both |
| extra pin | floating 1×1 µm met3/Metal3 rectangle labelled `extra` (sky130 also on 70/16) | Netgen `port errors`, `extra (no matching pin)`, `disconnected node: extra` (final line still "match uniquely"); KLayout labelled net `extra` unpaired (status still `Match`, 12/12 pins); port-label oracle: `extra` is not a port |
| extra port | reference netlist gets `input wire extra` (no layout counterpart) | Netgen pin lists differ; KLayout sky130 reference port `extra` not paired (simplify dropped it, 12 = 12 pins), gf180 13 ≠ 12 pins; port-label oracle: port without label |
| rename label | port text `en` → `enx`, geometry unchanged | XOR geometry 0 but labels differ (2 records) → `failed`; Netgen pin matching fails; KLayout pin/net names differ |
| alias label | a second label `en_alias` at the `en` label position | sky130 Netgen pin matching fails; gf180 Netgen matches (Magic merges the label) but Magic reports `Ports "en" and "en_alias" are electrically shorted` and the extracted ports differ; KLayout net `en,en_alias`; port-label oracle |
| child cell as top | `--top` = a library cell inside the GDS (`and2_2`) | every check `failed`: not a top cell; Netgen also: reference defines no such module |
| extra top cell | uninstantiated `stray_top` cell with one metal rectangle | every check `failed`: GDS has 2 top cells |
| OOM kill | antenna, drc-klayout and lvs-netgen with a 24 MiB container cap | all `unverified` (exit 137, killed by signal 9) with `counts.total` null |
| manifest missing entry | `pdks.<pdk>.files.klayout_lvs` deleted from a manifest copy | `lvs-klayout` exit 3 `unverified` ("manifest incomplete"), verdict written, no crash |
| mini (positive) | clean 2-row PDK-cell layout | Magic and KLayout DRC both `passed` (0) |
| xor (positive) / LVS (positive) | re-streamed reference; baseline LVS | `passed` |

A 600 µm gf180 wire on `a[1]` is below the 400 ratio. The four `a[1]` gates
total about 2.6 µm², so the side-area ratio is about 250. Both tools agree on
this, so the gf180 control uses 2000 µm.

## Oracle and status integrity (W3)

**GF180 profile units.** `fixtures/pdk/gf180-routing.json` previously held
Metal1 230, Metal2–4 280 and cut 260 DBU. At 2000 DBU/µm those mean 0.115,
0.14 and 0.13 µm. The tech LEF says 0.23, 0.28 and 0.26 µm. The profile now
holds 460, 560, 560, 560 and cut 520.

**Units assertion.** `check_routing_geometry.py` calls
`routing_units.assert_profile_units()` before any check. It compares each
profile spacing against the LEF value: scalar `SPACING`, or the
`SPACINGTABLE` WIDTH 0 entry, times `DATABASE MICRONS`. Any value below the
LEF value fails with a diagnostic, for example `Metal1: profile spacing 230
DBU < LEF SPACING 0.23 um x 2000 DBU/um = 460 DBU (profile value is 0.115 um
...; wrong units?)`.

**Router unaffected.** The router uses max(profile, LEF), so the rerun with
the frozen producer and the fixed profile gives byte-identical DEF, mapped
and powered netlists, and identical timing. The SPEF differs only in its
`*DATE` line.

**Planted gap.** The checker's new `planted_subminimum_gap` control builds a
GF180 Metal1 bump. The bump overlaps net `clk`'s Via1 pad and leaves exactly
0.20 µm (400 DBU) to `rr_clock_2`, while staying clear of every other net:

- with the LEF-consistent spacing it is rejected (`different-net short or
  spacing violation`);
- with the old half-unit values it is accepted.

The sky130 equivalent, a 0.11 µm gap, is rejected.

**Independent oracle.** `gap_crosscheck.py` plants the same bump into the
reference GDS and runs the gf180 foundry deck on the planted copy and on the
unplanted reference, then diffs the two report databases. It agrees only if
the M1.2a count increases and a NEW M1.2a edge pair has one edge on the
bump's edge facing the target and the other on the target's facing edge,
exactly 0.20 µm apart; the same predicate must find nothing in the unplanted
baseline. The deck reports that edge pair, `(60.58,28.586;60.58,29.045)|
(60.38,28.96;60.38,28.7)`, so it agrees with the corrected subset checker.
(The earlier predicate, any M1.2a item within 0.25 µm, also matched a
pre-existing notch item and did not discriminate.)

**Flow status in `verify_routing.py`.**

- The previous overall verdict is now `postroute_timing_status`.
- The top-level `status` (`scripts/flow_status.py`) requires all of these, on
  both PDKs (exactly one sky130 and one gf180 result; a single-PDK summary is
  `partial`), plus `postroute_timing_status == passed` and a functional,
  identity-stable flow:
  - `gds`, `drc_magic`, `drc_klayout`, `antenna`, `lvs_netgen`, `lvs_klayout`;
  - `timing_ss_max_setup`, `timing_ss_max_hold`, `timing_ff_min_hold`;
  - `mpw_min_period`, `drv`, `cell_policy`.
- A definitive failure of a required item (DRC items, antenna violations,
  an LVS mismatch, a native-GDS XOR difference, a cell-policy violation), a
  functional failure or an identity change gives `failed` (exit 1) and lists
  `failed_items`. Otherwise a required item that is missing, unverified or
  unchecked, or a post-route timing status other than `passed`, gives
  `partial` (exit 3). `partial` means "not proven", never "proven bad".
- The scope block reports `cppr: unchecked` together with IR, EM, SI and OCV.
- `timing_tt_nom` and `density` are reported but not required.
- RustSTA DRV is always `unchecked`. RustSTA records `set_max_*` without
  analysing it (D0021), so its zero counts are not evidence. OpenSTA's TT
  limit report is attached.
- `mpw_min_period` is `unchecked` while RustSTA reports unanalyzed
  `min_pulse_width`/`minimum_period` arcs: 4 on Sky130, 8 + 8 on GF180.
- ss/ff timing items stay `missing` until the multi-corner extraction and
  timing stream supplies them.

`python3 scripts/flow_status.py SUMMARY.json --assume-timing-passed` replays
the gate with every timing item forced to `passed`. On the frozen-producer
summary (no signoff run) it still gives `partial`/exit 3, because the signoff
items are missing. The runs that do sign off the reference streamout give
`failed`/exit 1, because DRC really fails.

## Producer and cell policy (W4)

**Producer build.** RustSys `c5ec23c` (tree `a25b2efb…`) was exported with
`git archive` to `.work/tools/rustsys-src-c5ec23c` (archive sha256
`29c83bbc…`). It was built with this command:

```
CARGO_TARGET_DIR=/mnt/data2/cgao/git/RustROAD/.work/tools/rustsys-target cargo build --locked --offline --release -p rustsys-cli
```

The build ran under `systemd-run MemoryMax=8G`, with 4 jobs, in 53 s, with a
peak of 702 MiB. Neither the RustSys tree nor its targets were touched.

**Producer pin.** The binary is
`/mnt/data2/cgao/git/RustROAD/.work/tools/rustsys-target/release/rustsys`,
sha256 `3de13b3a87deb103f89a3c7456ce9196ccdbf52fd145eab546c1c6e4732be4ae`,
70,962,888 bytes. Compared with the gf180-audit binary `cded9f8b…`:

- its sha differs, because the git-archive build path is embedded;
- the help text of all 8 command levels is identical;
- the `pipeline.v` netlists are byte-identical on both PDKs.

**Cell policy.** `fixtures/flow/cell-policy.json` (`rustroad.cell-policy.v1`)
holds:

- the exact `--dont-use` globs per PDK: the PDK `synth_exclude` list, plus
  the `pnr_exclude` list, plus, for GF180, `dly*`, `clkbuf*` and `clkinv*`
  reserved for CTS/hold, and (owner decision 2026-10-24) the GF180 families
  that cannot reach 1.0 ns output transition at ss (aoi211, aoi221, aoi222,
  nor4, oai22, oai221, oai222, oai31, oai32, oai33; derived from Liberty by
  `scripts/derive_drv_dont_use.py`, see `docs/rtl2gds.md`). Sky130's
  synthesis list already reserves its clock/delay cells. Every source file
  is recorded with sha256;
- the `pnr_exclude` list;
- the cells that may be inserted after synthesis (physical-only and CTS/hold);
- the producer build receipt;
- the frozen producer.

It is regenerated with:

```sh
python3 scripts/cell_policy.py generate --producer-receipt .work/tools/rustsys-c5ec23c.build.json
```

**Independent checker.**

```sh
python3 scripts/check_cell_policy.py --pdk gf180 --stage synth --netlist mapped.v --policy fixtures/flow/cell-policy.json
python3 scripts/check_cell_policy.py --pdk gf180 --stage physical --mapped mapped.v --netlist final.v --netlist powered.v
```

The checker reads the PDK exclusion files directly from the pinned Ciel path,
restates the owner reservation, and cross-checks the policy's globs and
source hashes. Netlists are read by a small structural-Verilog tokenizer and
statement classifier: comments and `(* attributes *)` are dropped, escaped
identifiers are kept whole, and `#(...)` parameter lists, instance arrays and
multi-instance statements are understood. Any statement that is neither a
declaration nor a recognised instance (for example `always`, `generate`, a
primitive without a name) and any compiler directive other than
`timescale`/`default_nettype`/`celldefine`/`resetall` makes the result
`unverified`, never `passed`. Every instantiated cell must be a MACRO of the
PDK cell LEF (read from the same pinned root; its sha256 is reported) or a
module defined in the scanned files. Anything else, such as a black-box
wrapper whose body is not supplied or an upper-cased excluded cell name, could
hide an excluded cell, so it gives `unverified` (`unknown_cells`). It applies
these rules:

- `pnr_exclude` cells are forbidden everywhere.
- Instances from synthesis must obey the synthesis rule.
- Instances added later may only be physical-only or CTS/hold cells.

`scripts/signoff/cellpol_controls.py` adds a gf180 `mux2_1` (a `pnr_exclude`
cell) to the real mapped netlist in five syntaxes (plain, attribute,
parameter list, second instance of a statement, escaped names): all five are
`failed`. An added `always` block, a `define`, an instance of an undefined
`hidden_wrapper` module or an upper-cased `GF180MCU_FD_SC_MCU7T5V0__MUX2_1`
gives `unverified`, and the unmodified netlists give `passed`.

As a control on the sys scout's `probe.v` with `--timing-period 2`:

- without the policy it finds sky130 `sdfrbp_1` (scan) and gf180
  `clkinv_1/2/3` and `sdffrnq_1`;
- with the policy it finds none.

**Flow switches.** `verify_routing.py --producer policy` is the default and
uses the pinned producer with `--dont-use`. `--producer frozen` reproduces the
historical 9c83124e run, and that rerun is byte-identical.

### Flow re-run, frozen producer vs pinned producer + policy

Both runs use the same RustROAD binary (`07788f84…`) and RustSTA 0.3.

| | Sky130 frozen | Sky130 policy | GF180 frozen | GF180 policy |
|---|---|---|---|---|
| mapped cells | and2_2×4, xor2_4×2, dfxtp_1×4 (area 165.16) | and2_0×2, and2_4×2, xor2_2×2, dfxtp_1×4 (142.64) | and2_2×4, xor2_2×2, dffq_1×4 (412.70) | same cells; and2 pin order swapped |
| placed cells | 131 | 134 | 134 | 134 |
| routed nets / terminals | 19 / 46 | 19 / 46 | 19 / 46 | 19 / 46 |
| wires / vias | 4964 / 101 | 4885 / 99 | 4982 / 77 | 4997 / 77 |
| TT worst setup / hold (ns) | 8.946 / +0.000247 | 8.956 / **−0.113** (2 endpoints) | 7.316 / −0.636 (4) | 7.314 / −0.616 (4) |
| RustSTA–OpenSTA max difference | 0.55 ps | 0.48 ps | 0.30 ps | 0.30 ps |
| Yosys CEC / RustSys CEC / 512-vector simulation | pass / equivalent / pass | pass / equivalent / pass | pass / equivalent / pass | pass / equivalent / pass |
| cell policy (mapped, physical) | clean (unchecked: no policy) | passed | clean (unchecked) | passed |
| reference-GDS DRC: KLayout / Magic | 9 / 26 | 9 (m1.2 4, m2.2 4, **m3.2 1**) / 25 | 7 / 13 | 7 / 13 |
| antenna | 0 | **Magic metal3 ratio 469.5 > 400** | 0 | 0 |
| LVS Netgen / KLayout | match / match | match / match | match / match | match / match |
| top-level status (frozen: no signoff run; policy: reference signoff) | partial (exit 3) | failed (exit 1): DRC + antenna | | failed (exit 1): DRC |

On `pipeline.v` the policy changes nothing: the mapped netlists are
byte-identical with and without `--dont-use`. Every difference comes from the
newer producer's default load-aware cleanup. It picks smaller sky130 cells,
which:

- moves Sky130 TT hold from +0.25 ps to −0.113 ns;
- adds a met3 spacing violation and a sky130 metal3 antenna violation.

These are real regressions for the routing and hold streams to close. They
are not hidden.

The fix-round reruns (`.work/final2`, `.work/final3`) used the worktree's own
RustROAD build (`fb910e71…`). Their DEF, mapped and powered netlists are
byte-identical to the earlier run with the main-tree binary, and every timing
and signoff number is unchanged; only the sky130 density count is now `null`
instead of 0. In `.work/final3` every flow artifact and timing number is
byte-identical to `.work/final2`. The policy-run netlists pass both LVS tools
under the property, pin-name, named-net, reference-port, extraction and
port-label checks. A fourth flow variant feeds the planted spacing GDS as the
"native" GDS: XOR fails, `gds` is `failed`, and the run is `failed` (exit 1).

## Reproduction and resources

```sh
scripts/signoff/reproduce.sh .work/final     # copies READY inputs, never mutates them
```

The script covers:

- identity, reference streamout, `signoff.py all` and controls on both PDKs;
- the tampered manifest;
- the four flow variants (frozen, policy + reference signoff, native path,
  bad native GDS);
- the cell-policy scanner controls (`cellpol_controls.py`);
- the GF180 gap cross-check;
- the gate what-if;
- the unit tests (`python3 -m unittest discover -s scripts/tests`, 42 tests);
- `docs/evidence/signoff-results.json`, written by `evidence.py`.

Measured on this host:

| Step (container wall incl. podman start) | Sky130 | GF180 |
|---|---|---|
| Preflight probe (incl. label lists: 12 top labels; 677 / 1056 flattened) | 1.6 s, 402 MiB | 1.7 s, 401 MiB |
| Magic DRC (hierarchical + flattened recheck) | 1.0 s, 87 MiB | 0.9 s, 82 MiB |
| KLayout DRC deck | 3.3 s, 427 MiB | 9.3 s, 435 MiB |
| Antenna (Magic; gf180 + KLayout deck) | 0.7 s, 71 MiB | 0.6 s + 2.3 s, 416 MiB |
| Density decks | 1.8 s + 0.7 s, 403 MiB | 2.1 s, 413 MiB |
| Magic extraction + Netgen | 1.7 s + 0.5 s, 462 MiB | 0.9 s + 0.5 s, 251 MiB |
| KLayout LVS + xref/pin-pair/named-net dump | 3.0 s + 1.7 s, 424 MiB | 8.5 s + 1.6 s, 419 MiB |
| reference streamout | 2.5 s (host step) | 2.5 s (host step) |
| `signoff.py all` (incl. identity + preflight) | 16.7 s | 29.2 s |
| `controls.py` (23 controls) | 200 s, 136 tool runs | 313 s, 136 tool runs |
| `verify_routing.py` frozen / policy + reference signoff / native path / bad native | 28 s / 82 s / 87 s / 86 s, 405,696 KiB host process | (both PDKs per run) |
| `gap_crosscheck.py` (planted + baseline deck runs) | | 26 s |

The fix-round-2 `reproduce.sh` run (`.work/final3`) took 14:37 wall with a
host peak of 405,696 KiB. It made 754 container tool runs; the largest peak
RSS was 474,844 KiB (Netgen reading the full sky130 cell SPICE) and none
exceeded 1 GiB. All container runs used a 4 GiB cap (2 GiB for probes; the
OOM control deliberately uses 24 MiB). The largest host process was
`verify_routing.py` at 405,696 KiB. The Rust worktree checks (`cargo build --release`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo test --workspace`) run with
`PATH=/home/cgao/.cargo/bin` first (the host `/usr/bin/rustc` 1.93.1 fails
with E0514) under `systemd-run MemoryMax=8G`, see `.work/build-check.sh`.

## Traps handled

- Magic silently creates a missing top cell. The tcl refuses with
  `RR_TOP_MISSING`, and the preflight fails as well.
- Magic snaps off-grid input. Off-grid detection relies on KLayout.
- A relative KLayout report path resolves against the input GDS directory,
  which is read-only here. KLayout then writes nothing and exits 0, so only
  absolute paths are used and the report's existence and age are asserted.
- The sky130 density deck needs 235/4. Without it, it prints NaN/−Infinity
  and reports 0 items.
- KLayout deep mode reports some items in child cells. Counts cover all
  cells.
- strmxor without `--layer-details` ignores layers missing in one file.
- KLayout LVS without `top_lvl_pins` compares 0 top pins. With it, KLayout
  still pairs pins by topology, so swapped port labels on symmetric bits
  still match; only the harness's pin-name comparison catches them.
- Netgen reports a same-topology device-parameter mismatch as `Circuits match
  uniquely.` plus `Property errors were found.`.
- The gf180 `run_lvs.py` and the KLayout DRC decks exit 0 on failure, and
  Netgen always exits 0.
- KLayout's GDS writer stamps the time unless `gds2_write_timestamps=False`.
- The PDK CDL is not parseable as a whole for gf180 (line 598). Only the
  needed `.SUBCKT`s are extracted.
- An extra top-level pin that is not in the reference (a labelled floating
  metal rectangle) passes both tools on both PDKs unless their outputs are
  read closely: Netgen prints `port errors`/`(no matching pin)`/`disconnected
  node` but still `Circuits match uniquely.`; KLayout drops the floating
  labelled net from the comparison and reports `Match`. Because the reference
  streamout comes from the same DEF, a DEF-level extra pin would also pass the
  XOR. The harness now fails on the Netgen lines, on unpaired labelled
  KLayout nets, and on any top label that is not a reference port.
- sky130 `schematic_simplify` silently drops an unconnected reference port, so
  equal top pin counts prove nothing; the reference port list is compared.
- Netgen compares the layout cell against any reference circuit of the same
  name, including a PDK library subckt; the reference module name is checked.
- strmxor ignores text labels; labels are compared separately.
- Magic (gf180) merges a second label at the same position into the existing
  port and only warns `Ports ... are electrically shorted`.
