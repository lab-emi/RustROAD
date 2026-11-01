# Integration fixes and dual-PDK requalification on the native GDS

Stream b0, branch `feat/rtl2gds-b0` (from `feat/rtl2gds` b813c34), 2026-10-24.
Scope is block/macro level: no pad ring, no seal ring; IR/EM/SI/OCV/CPPR are
`unchecked`; density is reported separately and is never folded into DRC.
Third-party tools ran only as black boxes (signoff image `4c48415d8bd0`:
Magic 8.3.678, Netgen 1.5.323, KLayout 0.30.9; OpenRCX image `f152917ebc45`;
OpenSTA 3.1.0 host binary). No reference source was read.

Evidence (all tracked): `docs/evidence/requalification.json`,
`docs/evidence/gds-verification.json`, `docs/evidence/def-segmentation.json`,
`docs/evidence/extraction-requalification.json`,
`docs/evidence/timing-baseline.json`.

## What changed

1. **`export-def` writes maximal collinear runs.** The router stores one record
   per lattice edge; the DEF used to write one `NEW` path per record, each with
   its own half-width end extension. The new bounded helper
   `rustroad_core::wire_runs::collinear_runs` merges same-net, same-layer
   records that touch or overlap on one track line and splits a run only at a
   strictly interior via landing (lower or upper metal), pin-access point or
   same-layer branch. Vias stay singleton paths and patches stay `RECT`
   statements. Every via and branch remains a path end, which is what
   endpoint-based readers need.
2. **`export-gds` streams patches and writes the same runs.** `Routes.patches`
   were dropped; they are now rectangles on the routing layer's net purpose.
   Wires use the DEF runs. Every used LEF via variant already had its own
   `VIA_<name>` structure (Sky130 `VIA_M1M2_PR_MR`; GF180 `VIA_Via1_VH`,
   `VIA_Via2_HH`, `VIA_Via2_HV`, `VIA_Via2_VH`); the XOR confirms them.
   KLayout's DEF reader does draw routing `RECT`s in NETS, so the reference
   streamout covers patches without an overlay: on the same routed workspace,
   the b813c34 native GDS (no patches) differed from the reference only on the
   patch layers, and the new native GDS does not differ. `verify_gds.py` keeps
   this as a permanent control.
3. **`extract` includes patches.** A patch adds ground, plane and lateral
   coupling capacitance on all four edges of its pieces not already covered by
   same-net metal, attached to the node of the same-net conductor it overlaps
   (no new node or resistor). Details: `docs/extraction.md` (physical model
   step 5). Workspaces without patches extract byte-identically to b813c34.
4. **Harness fixes** found while requalifying:
   * `scripts/verify_gds.py` required the old router DRC baseline. It now
     requires Magic 0 and KLayout 0, adds a planted sub-minimum
     square (both decks must count it), checks that every DEF `RECT` patch is
     streamed on its net purpose, and deletes those rectangles as a control
     (the XOR must fail). Its basic KLayout LVS check stops at "Netlists
     match": deleting the wire of port `z[1]` still read `match` on both PDKs
     (a labelled net without devices is left out of that comparison). It now
     also runs both LVS checks of `scripts/signoff`, which catch it (top pin
     count 11 != 12, `z[1]` unpaired); the deleted-wire control requires
     Netgen and the signoff KLayout check to fail. The victim is the largest
     signal wire (a DEF NETS path), not a supply spine. The missing-cell
     control picks a cell the producer actually used. The odd-GF180-coordinate
     plant is now refused by the workspace reader (`INVALID_WORKSPACE`,
     LEF-rule via enclosure / supply via grid) before streaming; the control
     records which layer caught it and that no file was written.
   * `scripts/verify_router_signoff.py` assumed the first path of the first
     net is an access-layer wire; it now picks the first vertical access-layer
     wire of at least four widths.
   * `scripts/compare_spef.py merge-def` failed on DEF `RECT` lines; they are
     now kept verbatim (and checked identical). `campaign --sta-bin DIR`
     selects the RustSTA binaries.
   * New: `scripts/def_segmentation.py` (old vs new DEF proof),
     `scripts/timing_baseline.py` (multi-corner baseline),
     `scripts/requalification_evidence.py` (evidence assembly).

The stored routing state (`routing.rbin`, schema 5) is **not** compacted. The
lattice records carry the junction structure that `validate()` and the
extractor's graph use; merging at export gives the identical DEF/GDS metal
with no schema, mirror or old-revision risk.

## Geometry proof for the merged DEF

`scripts/def_segmentation.py` on the final flow workspaces compares OLD =
`export-def` of the b813c34 binary with NEW = the flow's `export-def` (same
routed revision). It checks that the text outside NETS, net headers, vias and
`RECT` patches is identical; that KLayout reference streamouts of both are
XOR-identical on all layers (`--layer-details`); that the text labels (layer,
string, position) are identical; that deleting the longest NEW path is
detected by the XOR (control); and that `merge-def` of OLD and NEW gives the
same paths. It also runs OpenRCX on both: the per-edge OLD DEF made the
OpenRCX reference depend on path segmentation, which is why the flow's DEF now
writes maximal runs. The receipt is `docs/evidence/def-segmentation.json`.

## Requalification (both PDKs, integrated tools)

Flow: `scripts/verify_routing.py`, policy producer (RustSys c5ec23c,
`3de13b3a…`) with `fixtures/flow/cell-policy.json`, signoff manifest, RustSTA
0.4 (`rusteda c8f71b63…`, `ruststa ac76f0f3…`), RustROAD `ff523a9d…` (built
from this branch). Pass 1 used `--signoff-reference`; pass 2 used
`--signoff-gds <pdk>=<pass 1 native export-gds>`. Both passes produced byte-identical DEF; pass 2's workspaces
export the same GDS bytes and the same native SPEF bytes (min/nom/max) as
pass 1.

Pass 2 also feeds `scripts/verify_router_signoff.py` (planted notch and
sub-area controls, swapped LVS pins, a placement-only copy, pin-access test
layouts) and `scripts/verify_gds.py` (read-only, byte-determinism including
an independent copy, XOR and labels, verbatim cell bytes, patches streamed,
planted DRC, four LVS verdicts on native and reference, negative controls).
The per-check verdicts of this stream are in
`docs/evidence/requalification.json` and `docs/evidence/gds-verification.json`,
kept as historical evidence for reproducibility, not as current results.

The Sky130 clock-input antenna item found in this stream was handed to the
antenna ECO stream (`docs/hold-closure.md`); its record is
`antenna_eco_handoff` in `docs/evidence/requalification.json`.

## Post-integration timing baseline

`scripts/timing_baseline.py` times pass 2 at tt+nom, ss+max and ff+min with
the unmodified PDK Liberty and the flow's SDC, on the native SPEF from
`rustroad extract` and on OpenRCX SPEF of the flow's merged DEF. One RustSTA
0.4 workspace per SPEF source holds all three corners
(`--require-wire-model spef_rc`, OpenSTA-compatible pulse rules); OpenSTA
3.1.0 runs the same inputs per corner. The parity gate is 1 ps per endpoint.
The per-endpoint `hold_fix` rows (required delay, net, driver, worst path
stages) for every corner and source are in
`docs/evidence/timing-baseline.json`; hold closure is `docs/hold-closure.md`.

## Resources (Rule 9)

Estimates before running: the native commands are small (export-gds
< 32 MiB); RustSTA three-corner import < 256 MiB;
containers capped at 4 GiB (signoff) / 2 GiB (OpenRCX), one at a time per
harness; no run above 4 GiB, so no scale lock.

## Reproduction

```sh
free -h
export PATH=/home/cgao/.cargo/bin:$PATH
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  timeout 1200s cargo build -j 4 --release --workspace
STA=/mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin
python3 scripts/verify_routing.py --workspace .work/runs/r1 --base-rev empty \
  --rustroad target/release/rustroad --rusteda $STA/rusteda --ruststa $STA/ruststa \
  --producer policy --signoff-reference
for p in sky130 gf180; do target/release/rustroad --workspace .work/runs/r1/${p}_physical \
  export-gds --cell-gds <pdk cell gds> --layer-map fixtures/pdk/$p-gds-map.json \
  --out .work/runs/r1-native/$p.gds; done
python3 scripts/verify_routing.py --workspace .work/runs/r2 --base-rev empty \
  --rustroad target/release/rustroad --rusteda $STA/rusteda --ruststa $STA/ruststa \
  --producer policy --signoff-gds sky130=.work/runs/r1-native/sky130.gds \
  --signoff-gds gf180=.work/runs/r1-native/gf180.gds
python3 scripts/verify_router_signoff.py --run-dir .work/runs/rsig2 --flow .work/runs/r2 \
  --rustroad target/release/rustroad
python3 scripts/verify_gds.py --run .work/runs/gds5 --source .work/runs/r2 \
  --rustroad target/release/rustroad --evidence docs/evidence/gds-verification.json
python3 scripts/compare_spef.py campaign --workspace .work/runs/spef2 --base-rev empty \
  --ready .work/runs/r2 --sta-bin $STA
python3 scripts/compare_spef.py evidence --summary .work/runs/spef2/summary.json \
  --out docs/evidence/extraction-requalification.json
# OLD DEF: export-def of the same workspaces with the b813c34 binary
python3 scripts/def_segmentation.py --run .work/runs/seg2 \
  --case sky130:OLD/sky130.def:.work/runs/r2/sky130.def:ROUTES/sky130_routes.json \
  --case gf180:OLD/gf180.def:.work/runs/r2/gf180.def:ROUTES/gf180_routes.json \
  --evidence docs/evidence/def-segmentation.json
python3 scripts/timing_baseline.py --flow .work/runs/r2 --out .work/runs/tb4 \
  --evidence docs/evidence/timing-baseline.json
python3 scripts/requalification_evidence.py --pass1 .work/runs/r1 --pass2 .work/runs/r2 \
  --native-gds .work/runs/r1-native --router-signoff .work/runs/rsig2 \
  --rustroad target/release/rustroad --scratch .work/runs/evidence-scratch \
  --out docs/evidence/requalification.json
```

## Open items at the end of this stream

* Hold closure and the Sky130 clock-input antenna item went to the hold and
  antenna ECO streams (`docs/hold-closure.md`).
* `verify_routing.py`/`flow_status.py` gated only tt+nom; the three-corner
  gate is `scripts/rtl2gds.py` (`docs/rtl2gds.md`).
* GF180 density needs fill; Sky130 windowed density needs a chip-level
  layout. Both are reported separately.
* Extraction accuracy items (PDK corner R data, omitted via pads and end
  caps) are described in `docs/extraction.md`.
