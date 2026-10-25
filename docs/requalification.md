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
   its own half-width end extension (Sky130 4898, GF180 5014 paths). The new
   bounded helper `rustroad_core::wire_runs::collinear_runs` merges same-net,
   same-layer records that touch or overlap on one track line and splits a run
   only at a strictly interior via landing (lower or upper metal), pin-access
   point or same-layer branch. Vias stay singleton paths and patches stay
   `RECT` statements. The DEF now has 104 / 77 wire paths. Every via and
   branch remains a path end, which is what endpoint-based readers need.
2. **`export-gds` streams patches and writes the same runs.** `Routes.patches`
   were dropped; they are now rectangles on the routing layer's net purpose.
   Wires use the DEF runs (Sky130 104 rectangles from 4898 records; GF180 77
   from 5014). Every used LEF via variant already had its own `VIA_<name>`
   structure (Sky130 `VIA_M1M2_PR_MR`; GF180 `VIA_Via1_VH`, `VIA_Via2_HH`,
   `VIA_Via2_HV`, `VIA_Via2_VH`); the XOR below confirms them. The output is
   5.7x (Sky130, 371 -> 65 kB) and 7.8x (GF180, 362 -> 46 kB) smaller.
   KLayout's DEF reader does draw routing `RECT`s in NETS, so the reference
   streamout covers patches without an overlay: on the same routed workspace,
   the b813c34 native GDS (no patches) differed from the reference by XOR
   shapes only on the patch layers (Sky130 68/20: 3, 69/20: 5, 70/20: 1 for 8
   patches; GF180 36/0: 2 for 1 patch), and the new native GDS differs by 0.
   `verify_gds.py` keeps this as a permanent control.
3. **`extract` includes patches.** A patch adds ground, plane and lateral
   coupling capacitance on all four edges of its pieces not already covered by
   same-net metal, attached to the node of the same-net conductor it overlaps
   (no new node or resistor). Details: `docs/extraction.md` (physical model
   step 5). Workspaces without patches extract byte-identically to b813c34.
4. **Harness fixes** found while requalifying:
   * `scripts/verify_gds.py` required the old router DRC baseline (Magic 26
     etc.). It now requires Magic 0 and KLayout 0, adds a planted sub-minimum
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

`scripts/def_segmentation.py` on the final flow workspaces: OLD = `export-def`
of the b813c34 binary, NEW = the flow's `export-def` (same routed revision).

| check | Sky130 | GF180 |
|---|---|---|
| text outside NETS, net headers, vias, `RECT` patches | identical | identical |
| wire paths | 4898 -> 104 | 5014 -> 77 |
| KLayout reference streamouts, XOR (all layers, `--layer-details`) | 0 | 0 |
| text labels (layer, string, position) | identical | identical |
| control: longest NEW path deleted | XOR 70/20: 1 (detected) | XOR 42/0: 1 (detected) |
| `merge-def` of OLD / NEW | 100 / 100 paths | 72 / 72 paths |

OpenRCX total capacitance (fF) on the same metal:

| PDK | RC | OLD (per edge) | merge-def(OLD) | NEW (export-def) | NEW vs merge-def | OLD vs merge-def |
|---|---|---|---|---|---|---|
| sky130 | min | 163.01 | 67.43 | 67.45 | +0.03 % | +141.7 % |
| sky130 | nom | 178.64 | 73.70 | 73.73 | +0.03 % | +142.4 % |
| sky130 | max | 189.86 | 77.82 | 77.85 | +0.04 % | +144.0 % |
| gf180 | min | 206.53 | 88.54 | 88.54 | 0.00 % | +133.3 % |
| gf180 | nom | 226.28 | 96.69 | 96.69 | 0.00 % | +134.0 % |
| gf180 | max | 250.26 | 106.53 | 106.53 | 0.00 % | +134.9 % |

Per-net C of NEW vs merge-def(OLD) is within 0.85 % and per-net R within
0.31 % (Sky130); GF180 is identical. The residual Sky130 difference comes from
the four upper-layer via landings at which NEW ends a path and `merge-def`
does not.

## Requalification (both PDKs, integrated tools)

Flow: `scripts/verify_routing.py`, policy producer (RustSys c5ec23c,
`3de13b3a…`) with `fixtures/flow/cell-policy.json`, signoff manifest, RustSTA
0.4 (`rusteda c8f71b63…`, `ruststa ac76f0f3…`), RustROAD `ff523a9d…` (built
from this branch). Pass 1
used `--signoff-reference`; pass 2 used `--signoff-gds <pdk>=<pass 1 native
export-gds>`. Both passes produced byte-identical DEF; pass 2's workspaces
export the same GDS bytes and the same native SPEF bytes (min/nom/max) as
pass 1. A complete earlier campaign with a binary that differed only in the
`extract` JSON scope text gave the same DEF, GDS, SPEF and timing values.

| item (native GDS) | Sky130 | GF180 |
|---|---|---|
| XOR native vs reference streamout (geometry + labels) | passed (0) | passed (0) |
| Magic drc(full) | 0 | 0 |
| KLayout main deck | 0 | 0 |
| antenna, Magic `antennacheck` | **1 (metal3, ratio 470.9 > 400)** | 0 (secondary: GF180 Magic rules marked incomplete) |
| antenna, KLayout | not available (no antenna rules in the Sky130 decks) | 0 (primary, `gf180mcu.drc` antenna deck) |
| LVS Netgen | match | match |
| LVS KLayout | match | match |
| density (separate) | max density/clear area passed; windowed min density unverified (block < 700 um) | failed: 8 whole-block coverage rules (no fill) |
| CEC (native + Yosys) / 512-vector simulation | passed / passed | passed / passed |
| cell policy (synth + physical) | passed | passed |

`scripts/verify_router_signoff.py` on pass 2 (reference streamout): DRC 0/0,
LVS match on both PDKs; planted notch and sub-area controls and swapped LVS
pins are rejected; the placement-only copy is clean (0/0); pin-access test
layouts (28 Sky130, 22 GF180) are clean. Its status is `failed` only because
of the Sky130 antenna item.

`scripts/verify_gds.py` on pass 2: `passed` on both PDKs (read-only,
byte-determinism including an independent copy, XOR and labels, verbatim cell
bytes, 8 / 1 patches streamed, DRC 0 = reference, planted DRC detected, four
LVS verdicts match on native and reference, all negative controls detected).

The flow gate (`flow_status`) remains `failed`: besides the antenna item, the
ss/ff timing items, MPW/min-period and DRV are still placeholders in
`verify_routing.py` (it runs tt+nom only), and post-route timing is `partial`.
The multi-corner numbers are below.

### Antenna: root cause and handoff

The violation is real, not a GDS/DEF artifact (native and reference GDS are
XOR-identical and agree). Magic reports gate rect centre (53.81, 16.95) um,
inside `rr_clkbuf_2` (`sky130_fd_sc_hd__clkbuf_1`, placed (52.9, 16.32) FS).
That cell's input `A` is on net `clk`, routed from the met3 I/O pin at the
left die edge ((0, 1.29)-(0.49, 1.78) um) as one 53.765 um met3 run at
y = 1.41 um, then M2M3 at x = 54.01 um and 16.365 um of met2 up to the pin.
No met4 jump exists, so the whole met3 run is attached to the gate when met3
is etched. With `ANTENNAGATEAREA 0.1965` um^2 and met3 thickness 0.8 um
(LEF), side area ~ 92.5 um^2 gives the reported 470.9 against the LEF/Magic
sidewall ratio 400 (`ANTENNADIFFSIDEAREARATIO PWL ( ( 0 400 ) ( 0.0125 400 )
( 0.0225 2609 ) ...)`). This is an antenna-ratio problem of a long single-
layer route from an I/O pin to a minimum clock-buffer input, not a router,
GDS or DEF defect. It is handed to the antenna ECO stream (diode insertion,
e.g. a `sky130_fd_sc_hd__diode_2` on `clk` near `rr_clkbuf_2/A`; any diode
area >= 0.0225 um^2 raises the limit to 2609); a layer jump near the gate
would be the routing alternative. Exact record:
`docs/evidence/requalification.json` -> `antenna_eco_handoff`.

## Post-integration timing baseline (no hold fixing)

`scripts/timing_baseline.py` on pass 2. Corners per owner decision (f):
tt+nom, ss+max, ff+min with the unmodified PDK Liberty and the flow's SDC.
Native SPEF from `rustroad extract`; reference SPEF from OpenRCX on the flow's
merged DEF. One RustSTA 0.4 workspace per SPEF source holds all three corners
(`--require-wire-model spef_rc`, OpenSTA-compatible pulse rules); OpenSTA
3.1.0 runs the same inputs per corner. Parity gate 1 ps per endpoint.

| PDK | SPEF | corner | RustSTA WNS / WHS (ns) | OpenSTA WNS / WHS (ns) | parity dS / dH (ps) | hold < 0 | hold < +0.05 ns |
|---|---|---|---|---|---|---|---|
| sky130 | native | tt+nom | 9.1081 / -0.0045 | 9.1080 / -0.0045 | 0.12 / 0.14 | 1 | r0/D, r1/D |
| sky130 | native | ss+max | 8.5310 / -0.0998 | 8.5308 / -0.0999 | 0.28 / 0.28 | 2 | r0/D, r1/D, r3/D |
| sky130 | native | ff+min | 9.3394 / +0.0574 | 9.3393 / +0.0573 | 0.07 / 0.07 | 0 | - |
| sky130 | openrcx | tt+nom | 9.1032 / -0.0088 | 9.1030 / -0.0089 | 0.14 / 0.16 | 2 | r0/D, r1/D |
| sky130 | openrcx | ss+max | 8.5214 / -0.1063 | 8.5212 / -0.1064 | 0.19 / 0.20 | 2 | r0/D, r1/D, r2/D, r3/D |
| sky130 | openrcx | ff+min | 9.3363 / +0.0547 | 9.3362 / +0.0547 | 0.15 / 0.16 | 0 | - |
| gf180 | native | tt+nom | 7.7020 / -0.3945 | 7.7020 / -0.3944 | 0.10 / 0.09 | 4 | r0/D, r1/D, r2/D, r3/D |
| gf180 | native | ss+max | 5.6606 / -0.8987 | 5.6604 / -0.8984 | 0.42 / 0.34 | 4 | r0/D, r1/D, r2/D, r3/D |
| gf180 | native | ff+min | 8.5719 / -0.1805 | 8.5718 / -0.1805 | 0.08 / 0.08 | 4 | r0/D, r1/D, r2/D, r3/D |
| gf180 | openrcx | tt+nom | 7.6929 / -0.3966 | 7.6928 / -0.3965 | 0.16 / 0.13 | 4 | r0/D, r1/D, r2/D, r3/D |
| gf180 | openrcx | ss+max | 5.6387 / -0.9046 | 5.6361 / -0.9072 | **6.83 / 5.26** | 4 | r0/D, r1/D, r2/D, r3/D |
| gf180 | openrcx | ff+min | 8.5675 / -0.1814 | 8.5674 / -0.1813 | 0.08 / 0.08 | 4 | r0/D, r1/D, r2/D, r3/D |

Gates: setup >= 0 at ss+max is met everywhere (worst +5.64 ns, GF180). Hold
>= +0.05 ns is violated at tt+nom and ss+max on Sky130 and at every corner on
GF180, in both tools and on both SPEFs; Sky130 ff+min is met (+0.0547 ns
OpenRCX, +0.0574 ns native). All violating endpoints are register D pins fed
from input ports through one gate. RustSTA's `hold_fix --target-margin 0.05`
worst-corner requirement is always ss: Sky130 r0/D 0.150 (native) / 0.156 ns
(OpenRCX), r1/D 0.142 / 0.147 ns, r3/D and r2/D below 0.01 ns; GF180 r3/D
0.949 / 0.955 ns and r0..r2/D 0.83-0.87 ns. RustSTA reports `drv_status`
and `pulse_width_status` `met` on every run. The per-endpoint `hold_fix` rows
(required delay, net, driver, worst path stages) for every corner and source
are in `docs/evidence/timing-baseline.json`. With the merged DEF the OpenRCX
reference no longer overstates wire C: on the same routed design the per-edge
DEF of b813c34 gave tt+nom worst hold -0.114 ns (Sky130) and -0.614 ns (GF180)
in the flow's OpenRCX check; the merged DEF gives -0.009 / -0.397 ns.

**Parity deviation (for the RustSTA owners).** Only GF180 ss+max on the
OpenRCX SPEF misses the 1 ps gate. The data paths agree to 0.001 ps; the
difference is on the capture clock. At `rr_clkbuf_2/Z`
(`gf180mcu_fd_sc_mcu7t5v0__clkbuf_1` driving net `rr_clock_2`: 3 pins, SPEF
total 1.995 fF, R 34.5 ohm) OpenSTA's slew is 0.54239 ns, equal to its own
lumped-load slew at the same total (0.542394 ns with `set_load`), while
RustSTA gives 0.52789 ns. That 14.5 ps slew difference shifts the clock
arrival at `r0/CLK` by 2.6 ps and the hold check value by 2.7 ps. With the
native SPEF of the same net (R 94.9 ohm, 1.834 fF) both tools give 0.52134 ns.
Reordering `*CONN` (driver first) or the coupling node order, dropping
zero-valued couplings, or grounding the couplings changes neither tool's
result, so this is an effective-capacitance difference of the two delay
calculators on this RC network, not a SPEF-reading difference. Inputs are
frozen in the evidence by sha256.

## Extraction targets re-measured

`compare_spef.py campaign` on pass 2 (RustSTA 0.4): total C -4.1..-8.9 % and
per-net C pass everywhere; per-net R fails at Sky130 min/nom/max (nominal now
-20.9 % on one net, just over 20 %) and GF180 max; endpoint slack deltas exceed
10 ps at Sky130 ss+max (12.7 ps) and GF180 tt+nom (10.1 ps) and ss+max
(24.0 ps). The harness DEF now equals the merged level (segmentation
sensitivity < 0.05 % C, < 1 ps STA). The causes are the PDK corner-data and
omitted-metal items documented in `docs/extraction.md`; no coefficient,
tolerance or rule was changed.

## Resources (Rule 9)

Estimates before running: the native commands are small (export-gds < 32 MiB,
extract ~8 MiB measured before); RustSTA three-corner import < 256 MiB;
containers capped at 4 GiB (signoff) / 2 GiB (OpenRCX), one at a time per
harness; no run above 4 GiB, so no scale lock. Measured:

| run | wall | peak RSS |
|---|---|---|
| `cargo build --release` (4 jobs) | 46.5 s | 471 MiB |
| verify_routing pass 1 / pass 2 | 75.0 s / 78.7 s | 394 / 394 MiB (host processes) |
| export-gds (Sky130 / GF180) | 0.04 / 0.02 s | 16.6 / 10.6 MiB |
| extract (per corner) | 0.03-0.04 s | 8.2-8.6 MiB |
| verify_gds.py | 189.7 s | 464 MiB (largest container) |
| verify_router_signoff.py | 138.7 s | 465 MiB (largest container) |
| compare_spef.py campaign | 28.9 s | 133 MiB (largest child) |
| def_segmentation.py | 45.8 s | 45 MiB (outer) |
| timing_baseline.py | 13.4 s | 226 MiB (largest child) |
| whole evidence campaign (`.work/rerun.sh`) | 9 min 32 s | 394 MiB (host processes) |

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

Exit codes: verify_routing 1 (Sky130 antenna is a definitive failure; timing
items are partial), verify_router_signoff 1 (antenna), verify_gds 0,
compare_spef 3 (partial targets), def_segmentation 0, timing_baseline 1
(hold gates violated and the GF180 ss OpenRCX parity deviation).

## Open items

* Sky130 metal3 antenna on `clk` -> antenna ECO stream (diode or layer jump).
* Hold closure on both PDKs (table above) -> hold ECO stream. Re-route,
  re-extract and re-run both tools after any fix.
* GF180 ss+max OpenRCX-SPEF parity (6.8 ps) -> RustSTA effective-capacitance
  on low-R clock nets.
* `verify_routing.py`/`flow_status.py` still gate only tt+nom; the ss/ff,
  MPW/min-period and DRV items stay placeholders until the multi-corner STA
  is wired into the flow gate (the numbers exist in `timing_baseline.py`).
* GF180 density (8 coverage rules) needs fill; Sky130 windowed density needs a
  chip-level layout. Both are reported separately.
* Extraction accuracy targets that still miss (PDK corner R data, omitted via
  pads and end caps) are unchanged; see `docs/extraction.md`.
