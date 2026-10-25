# Hold closure, CTS drive selection and antenna repair

This stream adds a typed netlist ECO, a hold-fix planner driven by RustSTA
0.4 reports, per-level clock-buffer masters, a native LEF process-antenna
check with diode repair, and a bounded closure loop. Everything was checked on
`fixtures/flow/pipeline.v` for SKY130 (`sky130_fd_sc_hd`) and GF180
(`gf180mcu_fd_sc_mcu7t5v0`). Timing decisions use native SPEF only.

**Result.** On both PDKs the loop closes hold at every corner with
hold >= +0.05 ns and setup >= 0. RustSTA and OpenSTA 3.1.0 agree within 1 ps on
the same native SPEF. The final post-ECO, post-CTS netlist passes RustSys
`check synth`, Yosys CEC and 512-vector simulation. Native-GDS foundry DRC is
no worse than before the ECO, and the planted antenna controls are flagged by
both the native check and the deck and are then repaired. This is
block-level evidence; the scope limits are listed below.

## Commands

All four new commands are bounded and deterministic. `hold-eco`, `antenna-eco`
and `apply-eco` read and write files only; they never open a workspace.
`check-antenna` opens the workspace read-only.

```sh
# Delay-cell profiles from tt/ss/ff Liberty, cell LEF and the cell policy (tracked)
python3 scripts/build_delay_cells.py            # --check verifies the tracked files

# Hold ECO from RustSTA 0.4 reports (target margin 0.05 ns; setup kept >= 0.05 ns at ss)
ruststa -w STA report --section hold_fix --target-margin 0.05 --limit 0 --format json > hold.json
ruststa -w STA report --section endpoints --corner ss --limit 0 --format json > setup_ss.json
rustroad hold-eco --netlist mapped.v --timed-netlist exported.v [--timed-netlist-sha256 HEX] \
  --hold-report hold.json --setup-report setup_ss.json [--setup-report setup_tt.json ...] \
  --hold-corner tt --hold-corner ss --hold-corner ff --setup-corner ss \
  --delay-cells fixtures/pdk/<pdk>-delay-cells.json [--cell-lef LEF] --out eco.v --plan-out plan.json

# Native antenna check (read-only); optional what-if shapes for planted controls
rustroad --workspace W check-antenna [--extra-shapes shapes.json] [--safety-margin 0.10] > antenna.json
rustroad antenna-eco --netlist mapped.v --antenna-report antenna.json \
  --delay-cells fixtures/pdk/<pdk>-delay-cells.json --out eco.v --plan-out plan.json

# Diode-free antenna repair of HEAD by layer jumpers (mutating; new revision)
rustroad --workspace W repair-antenna --base-rev revN [--antenna-report antenna.json] \
  [--safety-margin 0.10] [--max-jumpers-per-net 16] [--max-positions 512] [--max-candidates 32]

# Apply a given rustroad.eco-plan.v1 (used for the controls)
rustroad apply-eco --netlist mapped.v --plan plan.json --delay-cells profile.json [--cell-lef LEF] --out eco.v

# Closure loop and evidence (pinned RustSTA copy in .work/tools/sta, see below)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v -o run.time \
  timeout 5400s python3 scripts/hold_closure.py --run .work/evidence/final --base-rev empty \
  --signoff --antenna-controls --openrcx
python3 scripts/cts_drive_eval.py --run .work/evidence/cts-eval
python3 scripts/hold_evidence.py --summary .work/evidence/final/summary.json \
  --cts .work/evidence/cts-eval/cts_drive_eval.json --time run.time --out docs/evidence/hold-closure.json
```

**Pinned RustSTA binaries.** These are the RustSTA 0.4.0 fix-round-2 binaries
(RustSTA d1f61e6). Copy them from `.work/sta-signoff/bin/round2` into this
checkout's `.work/tools/sta/`:

- ruststa: `ac76f0f3765ce3c6d44f85257f7d836120a7e0bf6a7283a75357f37fd79aa838`
- rusteda: `c8f71b63124577174f8458870d4ec006ab14b35f3b249e6e9d52a1734e759baf`

The shared `bin/` directory is replaced by later rounds, so the driver checks
the hashes on every run.

## ECO primitive (`rustroad_formats::eco`)

`rustroad.eco-plan.v1` holds a list of typed operations:

- `insert_buffer {cell, instance, net, sink{instance,pin}}` moves exactly one
  sink pin behind a new buffer. The old net drives the buffer's input and keeps
  all its other sinks.
- `insert_diode {cell, instance, net}` attaches a diode. `net` is a scalar
  wire/port or a vector bit `name[i]`; a name that matches both forms is
  refused.

Names must be new, shell-safe handles `[A-Za-z][A-Za-z0-9_]*`. Existing
instances, wires and register Q nets are never renamed, so CEC state matching
by Q wire name is unaffected. A buffer cell is accepted only if its profiled
Liberty `function` (whitespace and enclosing parentheses removed) is exactly
its single input pin. An inverting, multi-input or unprofiled cell gives
`ECO_NOT_BUFFER`. Every operation is checked against a bound
(`max_operations` 4096) and the Verilog AST limits.

## Delay-cell profiles (`fixtures/pdk/<pdk>-delay-cells.json`)

`scripts/build_delay_cells.py` reads the three owner-fixed corner Liberty
files with a clean-room Liberty group reader (`scripts/liberty_lite.py`).

- Candidates are the cell policy's `post_synthesis_insertable.cts_hold` globs
  minus the clock-tree cells (`clkbuf*`, `clkinv*`), plus the logic buffers
  `buf_1` and `buf_2`.
- A cell is kept only if, in every corner, it has exactly one input and one
  output, an identity `function`, and a `positive_unate` combinational arc.
- Delays are bilinear lookups with no extrapolation, at sky130 0.05 ns / 5 fF
  and gf180 0.15 ns / 10 fF.
- Antenna cells are LEF `ANTENNACELL` masters allowed as `physical_only`
  (`diode_2`, gf180 `antenna`), stored with their LEF diffusion and gate areas.
- Each excluded cell is listed with its reason.

The result is 16 sky130 and 14 gf180 buffers. The sky130 values match the
earlier black-box recon tables, for example `dlygate4sd1_1` at tt/ss/ff:
0.205/0.239, 0.385/0.485, 0.138/0.158 ns.

## Hold planner (`rustroad hold-eco`)

**Report requirements.** The planner refuses a report if any of these fails:

- the wire model is `spef_rc` and the clock model is `propagated`;
- the report's `target_margin_ns` is at least the requested margin;
- rows are not truncated (`listed == shown`);
- every requested hold corner is covered;
- the setup corner has an endpoints report;
- slacks that appear in more than one report agree;
- each endpoint instance and pin exists in the netlist.

**Report binding (hardening, stream gate).** RustSTA reports name their
workspace and revision but carry no hash of the netlist they timed. That
record is in the timing engine's own revision metadata, so `hold-eco` binds
the reports to it and refuses a mismatch with `STALE_REPORT` (never
`INVALID_WORKSPACE`; the file-only commands report their own codes):

- **Provenance.** The revision the reports name
  (`<workspace>/revisions/rev<NNNN>/meta.json`, schema
  `rusteda.revision.v1`, read under a shared `flock` on the STA workspace's
  `workspace.lock`) must record exactly one netlist source, and its xxh3-64
  must equal that of the bytes `hold-eco` parses as `--timed-netlist`. The
  hash is the binding; the recorded path is reported
  (`source_path_matches`) but a copy with the same bytes is accepted. A
  missing, unreadable, malformed or ambiguous record (no or several netlist
  sources, a record of another revision, a revision handle other than
  `rev<N>`) fails `REPORT_PROVENANCE`; a writer holding the lock for more
  than about 2 s fails `STA_WORKSPACE_LOCKED`. Committed revisions are
  immutable; nothing in the STA workspace is written.
- `--timed-netlist` is the netlist the timing engine imported (the post-CTS
  export). With `--timed-netlist-sha256` (scripts/hold_closure.py records the
  hash when it hands the file to `rusteda import`) a different file is refused;
- every hold_fix row must match the timed netlist: endpoint instance and
  master, endpoint net, net driver and every worst-path stage pin/net
  (assign aliases are merged);
- every endpoints row must name a pin or port of the timed netlist, and all
  reports must come from one STA workspace revision;
- the edited netlist must be refined by the timed netlist: same instances and
  masters, and every changed connection reaches the original net through
  inserted buffers (clock tree) or tie cells. An ECO-prefixed instance present
  only in the timed netlist means the reports belong to a later iteration.

The phase-B1 counterexample (iteration-0 reports against the iteration-1
netlist) fails with `STALE_REPORT`. So does the round-1 verifier's
counterexample, which the content checks alone accepted: the iteration-1
reports (whose rows also exist in the iteration-0 netlist) with the
iteration-0 netlist and its own correct sha256. On the gate run's data:

```text
STALE_REPORT: the reports come from .../sky130_i1_sta_ws rev2, which timed
.../sky130_i1_exported.v (xxh3:ccbbad2cbcf4f9d8); --timed-netlist
.../sky130_i0_exported.v is xxh3:5e03463facbccb2b: the reports were not
produced from this netlist
```

(gf180: `xxh3:3a80821eca7dfeba` vs `xxh3:7fada0d8136b8ec9`.) The matching
pairing reproduces the gate's iteration-1 netlist byte for byte on both PDKs.
The plan records the binding (`inputs.binding`: `sta_record` with the
revision record, its sha256, the recorded netlist source and both xxh3
values; revision; rows and stages bound; inserted instances).

**Sink pins.** `insert_buffer` (hold-eco and apply-eco) accepts only a sink pin
whose cell-LEF `DIRECTION` is `INPUT`. The directions come from the profile's
`cell_lef` source, re-verified by sha256 (`--cell-lef` may only relocate the
same file). An output, inout or unknown pin fails with `ECO_INVALID`; the
phase-B1 `r0/Q` plan is now refused.

**Chain selection.** For each sink below the margin the planner needs
`margin - slack` at every hold corner where the sink is listed. It enumerates
multisets of up to `--max-chain` profiled cells, at most
`--max-combinations`. It picks the smallest total area, then the fewest
cells, then names. The hold side is credited with `min(rise, fall)` at each
corner. The setup side is charged `max(rise, fall)` at the setup corner, and
the estimated setup slack must stay at or above `--setup-margin`.

**Existing chains.** Earlier `rr_eco_hold_*` cells that already feed a sink
are found by walking back along the chain. New cells extend that chain, up to
`--max-total-chain`.

**Path-aware mode (`--pins-report`, closure stream).** With per-pin setup
slack reports of the same STA revision (bound like the other reports:
`pin_rows_bound`), the chain goes on the endpoint's worst hold path instead
of in front of the endpoint: on a sink pin of the path, or in front of every
input-pin sink of one small net (`--max-group-sinks`, e.g. an input port),
wherever the setup slack of every path through the point (and of the
delay already planned on the same path) leaves room at every setup corner;
a point shared by several violating endpoints is credited to all of them.
`--allow-partial` applies the feasible part (status `partial`) and lists
the infeasible endpoints with the setup budget of each point of their
path. Details and the ranking in `docs/closure.md`. Without
`--pins-report` the planner below is unchanged (same netlists; the plan
JSON carries three more parameter fields).

**Infeasible cases.** These write the plan with status `infeasible` and exit 2
with `HOLD_ECO_INFEASIBLE`:

- no chain meets the need without breaking the setup margin;
- the chain would exceed its length cap;
- the violating endpoint is a top-level port (endpoint planner; the
  path-aware planner can delay an upstream pin of its path instead).

The estimate is not a timing result. The loop re-times every ECO with new
parasitics.

## Per-level CTS masters

`clock.level_cells` in the physical profile is optional. Index 0 is the level
that drives the sinks; levels past the end of the list reuse its last entry.
Without the field nothing changes: tracked profiles and existing workspaces
are unaffected.

- Import keeps every configured master.
- Placement uses each master's own size.
- `validate_tree` checks each buffer's master against its level.
- SDC `set_max_fanout` still caps branching.

LEF `ANTENNACELL` diodes may stay on the clock source net as passive loads.
They still count toward the fanout limit.

### Evaluation (`scripts/cts_drive_eval.py`, iteration 0, RustSTA, ss+max)

The tables give the iteration-0 values at ss+max: clock-sink latency, clock
slew and worst hold slack.

**GF180**

| Variant (sinks, root) | Latency (ns) | Slew (ns) | Worst hold (ns) | Outcome |
|---|---|---|---|---|
| clkbuf_1 (tracked default) | 1.458 | 0.699 | -0.899 | closed in 2 iterations; 4 x dlya_1, 96.6 um2 |
| clkbuf_2 / clkbuf_4 | 1.339 | 0.413 | -0.675 | first plan 74.6 um2; ECO layout routing fails (per-sink search budget, net z[0]) |
| clkbuf_4 / clkbuf_8 | 1.279 | 0.311 | -0.554 | first plan 4 x buf_1, 52.7 um2; ECO layout routing fails (same) |
| clkbuf_4 | 1.352 | 0.311 | -0.653 | first plan 63.7 um2; ECO layout routing fails (same) |
| clkbuf_8 / 16, clkbuf_12 / 20 | — | — | — | CTS fails with PLACEMENT_CAPACITY between taps |

**SKY130**

| Variant (sinks, root) | Latency (ns) | Slew (ns) | Worst hold (ns) | Outcome |
|---|---|---|---|---|
| clkbuf_1 (tracked default) | 0.448 | 0.199 | -0.100 | closed in 3 iterations; 4 x buf_1, 15.0 um2 |
| clkbuf_2 / clkbuf_4 | 0.488 | 0.106 | -0.112 | closed in 2 iterations; 4 x buf_1, 15.0 um2 |
| clkbuf_4 / clkbuf_8 | 0.498 | 0.077 | -0.088 | closed in 2 iterations; 4 x buf_1, 15.0 um2 |
| clkbuf_4 | 0.525 | 0.081 | -0.134 | ECO layout routing fails (routing comparison limit) |
| clkbuf_8 / clkbuf_16 | — | — | — | clock routing fails (pin access candidates budget) |

**What the evaluation shows**

- On GF180, stronger clock buffers cut the ss clock slew by 55 % and the
  insertion delay by 12 %, and halve the hold-cell area in the first plan.
- Buffer upsizing alone does not close GF180: worst ss hold is still -0.554 ns.
- For every GF180 variant, the ECO layout hits the lattice router's per-sink
  search budget, which is already at its hard maximum.
- On SKY130, larger buffers add input load. Latency rises, the slew gain does
  not change the hold-cell count, and closure takes one iteration fewer.
- The tracked profiles keep `clkbuf_1`. The router budget item is handed to
  the router owner.

## Native antenna check (`rustroad check-antenna`)

**LEF data.** The LEF reader keeps the antenna statements exactly in
`LefDocument.antenna`:

- THICKNESS;
- the OXIDE1 AREA/SIDE and DIFF ratios, as scalars or PWL;
- GATEPLUSDIFF;
- the cumulative and AREAMINUSDIFF forms;
- pin ANTENNAGATEAREA and ANTENNADIFFAREA, with an optional LAYER.

Other oxide models, unknown forms and inexact values go to `unsupported`. They
never fail the import, and the import diagnostics are unchanged.

**Model.** The check re-reads the workspace's own LEF sources and verifies
them by sha256. The LEF/DEF 5.8 partial-net model is applied bottom-up over
the LEF layer stack:

- at a routing layer, the partial net is the conductors at or below it plus
  the cuts below it;
- at a cut layer, it is the routing layers below it plus the cuts up to it;
- the conductors are wires (with LEF width), vias, same-net patches and the
  routing-layer pin shapes.

For each component that touches a gate:

- ratio = (union side area, i.e. perimeter x THICKNESS, or union area) /
  (gate area + GATEPLUSDIFF x diffusion area);
- the limit is the DIFF rule evaluated at the connected diffusion area. With
  no diffusion, the non-DIFF rule applies, or the DIFF rule at zero when the
  LEF states only that one (sky130 metals, gf180 Metal2 and above). With
  diffusion but only a non-DIFF rule (gf180 Metal1), the non-DIFF rule
  applies, which ignores the diode's protection and is conservative.

**Verdict.** Cumulative or AREAMINUSDIFF rules, other oxides, or LAYER-qualified
pin areas make the verdict `partial`; none of these occurs in either PDK. Top
ports carry no gate or diffusion area. Cell-internal metal beyond the pin
shapes is not modeled.

**Safety margin (hardening, stream gate).** The native ratio reads low against
the deck: 445.6 vs Magic 470.866 on the natural sky130 clk/met3 violation
(-5.4 %), and the phase-B1 length sweep put the deck threshold between native
366.6 (Magic passes) and 379.5 (Magic fails), i.e. an under-read of 5.1-8.4 %.
A row is therefore `violated` when `ratio > (1 - m) x limit` with
`m = --safety-margin` (default 0.10, above the measured bound);
`exceeds_lef_limit` keeps the raw comparison, `effective_limit` the
threshold, and the report carries `safety_margin` and `lef_violations`.
`antenna-eco` sizes diodes against the same effective limit and carries a
`partial` input forward (status `partial`, `input_status`, diagnostic
`ANTENNA_INPUT_PARTIAL`) instead of reporting `clean`. The foundry deck stays
the signoff gate; scripts/rtl2gds.py re-measures the margin on a planted wire
in every run (native/(1-m) vs the Magic ratio, docs/rtl2gds.md).

**Budgets.** `max_shapes_per_net`, `max_pair_checks` and `max_union_cells`
bound the work. One run takes 0.03 s and about 13 MiB.

### Agreement with the foundry decks

- **Natural violation.** The iteration-0 sky130 layout has one antenna
  violation, the clock input `clk` on met3.
  - Native: 445.6 > 400.
  - Magic `antennacheck`: "Effective antenna ratio 470.866 > limit 400" on
    plane metal3.
  - After the hold ECO re-layout, both report 0.
  - On GF180, both report 0 on every layout.
- **Planted controls.** Each control is a wire of minimum I/O-layer width
  attached to input port `a[1]` and leaving the die. It is drawn in the
  native check through `--extra-shapes`, and in the native GDS through
  `kl_plant.py rect` on the layer map's net layer.

| PDK | Planted wire | Native check | Deck on planted GDS | Repair | After repair: native / deck |
|---|---|---|---|---|---|
| SKY130 | 600 um met3 | a[1] met3 793.2 > 400 | Magic: metal3 x 1 | 1 x diode_2 | 587.0 < 2773.9 (PWL at 0.43 um2 diffusion) / Magic 0 |
| GF180 | 2000 um Metal3 | a[1] Metal3 866.7 > 400 | KLayout: ANT.16_ii_ANT.4 x 8 | 1 x antenna | 254.5 < 400 / KLayout 0 |

The repair runs `antenna-eco`, then a fresh import, place, CTS and route, and
then the same plant. The repaired layouts still meet timing at every corner.

## Layer jumpers (`rustroad repair-antenna`)

**Why not diodes on sky130.** The antenna ECO's `sky130_fd_sc_hd__diode_2`
cannot pass the frozen LVS setup (E3 uart_tx and alu, root-caused with the
pinned image `4c48415d8bd0` on the E3 logs):

- the reference (powered) netlist is correct: the diode is present with the
  right connections (`DIODE` on the port net, `VGND/VNB` on VSS, `VPB/VPWR`
  on VDD), the XOR is clean and Netgen ends with `Circuits match uniquely`;
- Netgen prints `Cell sky130_fd_sc_hd__diode_2 (0|1) disconnected node:
  VGND/VPWR/VPB` inside the cell comparison, for both netlists: the PDK cell
  has one device (`XD0 VNB DIODE sky130_fd_pr__diode_pw2nd_05v5`) and three
  device-less supply pins. The frozen harness fails any disconnected node
  (the rule that catches a planted extra pin);
- the sky130 KLayout deck extracts no device from the `diode_2` layout
  (diff 65/20, nsdm 93/44 and areaid.diode 81/23 are all there; the deck's
  diode derivations do not recognise it): the extracted subckt is empty,
  the cell is `NoMatch` and the top is skipped.

Neither is a netlist error, so the deck/setup stays frozen and the repair
avoids diodes.

**Method.** The native check is per layer and non-cumulative (partial net:
the conductors that exist when the layer is etched). A jumper over layer L
cuts one straight L run of the violating partial net and bridges the gap on
the next routing layer up: two L/L+1 vias and a short L+1 wire (a
non-preferred-direction jog between two upper-layer track positions). While
L is etched the bridge does not exist, so the gates keep only the L metal
between them and the cut; when L+1 is etched the bridge is the only L+1
metal it adds. Netlist and placement are unchanged; only the violating
net's routes change.

- Per violating net, the lowest violating layer first. The net graph holds
  the layers up to L (wires split at every node, vias, access points and one
  virtual node per terminal joined to every node inside its pin shapes: the
  access point is often not on a via). Dijkstra from the gates of the
  violating partial nets gives the gate side of every straight run (interior
  nodes without via, access point or branch).
- Candidates: positions on the upper layer's tracks (lattice steps for the
  legacy router) inside the run, nearest the gate end first, bridges of one
  to three pitches; up to 512 positions per run. A candidate must clear
  every other-net shape (both vias on all their layers and the bridge,
  spatial index of the router's validation), leave no same-net pin or patch
  across the cut, and add no same-net rule violation (`route_rules`: notch
  spacing, minimum area, holes, floating patches) to the net.
- Strategy A cuts the longest runs; the first candidate that clears the
  net wins, else the one with the largest drop. Strategy B (multi-sink
  trees, where one cut can raise the other side's ratio) isolates every
  gate of the violating partial net on its nearest run in one batch. A
  choice is accepted only when the layer's worst ratio over the effective
  limit drops and no row of the net becomes newly violated; at most 16
  jumpers per net.
- The native margin is kept: acceptance uses the same `(1 - 0.10) x limit`
  as `check-antenna`. The edited routes pass the full `routing::validate`
  (other-net spacing, connectivity, same-net rules) before the commit and
  again inside it. `--antenna-report` binds a `check-antenna` report of the
  base revision (revision, margin and violated row set must equal the
  recomputed ones, else `STALE_REPORT`). With nothing violated, no revision
  is written (`status: clean`).
- Not repairable: a violation on the top routing layer (met4 / Metal4: no
  layer above in the routing profiles), cut-layer rules, runs whose every
  position is blocked. They are listed in `unrepaired` with the reason; the
  closure loop's diode ECO remains the fallback (sky130 LVS then fails, as
  before).

`scripts/hold_closure.py` runs `repair-antenna` right after routing on every
layout of the closure loop (legacy and track recipes), so the antenna check,
extraction, timing and signoff of that iteration see the jumpered routes.
Results (gate runs `.work/runs/A1-*`, `A2-*`): uart_tx sky130 `data_i[7]`
met3 361.2 -> 18.7, alu sky130 `a_i[3]` met3 396.4 -> 53.8, regfile sky130
`raddr_a[0]` met3 396.6 -> 205.1 (one jumper each); Magic antenna, Magic and
KLayout DRC, XOR and both LVS checks pass on those layouts.

Tests: `rustroad-core` `jumper::tests` (gate-side cut on a routed lattice
fixture that re-validates with unchanged wire length; other-net metal on the
bridge layer moves the jumper; the cut partial net has no further position;
bounds), `crates/rustroad-workspace/tests/antenna.rs`
(`layer_jumpers_repair_partial_net_violations_without_diodes`: violated ->
repaired with M2/M3 jumpers, `REVISION_CONFLICT`, `STALE_REPORT`, the
committed revision re-validates, check-antenna agrees, netlist unchanged).

## Closure loop (`scripts/hold_closure.py`)

**Loop.** At most 4 iterations, each recorded:

1. netlist -> import-pdk -> place -> clock-tree -> route clock -> route all
2. native extraction at min/nom/max
3. RustSTA 0.4 with three Liberty corners and three SPEF corners
   (`--require-wire-model spef_rc`)
4. `hold_fix` report with a 0.05 ns target margin, plus endpoints per corner
5. native `check-antenna`
6. `hold-eco` while timing is open; `antenna-eco` only on a timing-closed layout

Repairing antenna on a layout that the next hold ECO will replace was tried
first. On sky130 that perturbed the layout into the router budget failure
described above.

**Corners.** The corners are fixed by the owner decision:

- sky130: tt_025C_1v80+nom, ss_100C_1v60+max, ff_n40C_1v95+min
- gf180: tt_025C_3v30+nom, ss_125C_3v00+max, ff_n40C_3v60+min

The SDC is `cts.sdc` with the same post-route lines that `verify_routing.py`
appends. The Liberty files are the unmodified PDK files.

**Final gates per PDK:**

- RustSTA and OpenSTA hold >= margin and setup >= 0 at all three corners;
- per-endpoint parity <= 1 ps on the same native SPEF;
- RustSys `check synth` of the RTL against the final exported netlist;
- Yosys CEC, plus a negative control;
- 512-vector Verilator simulation;
- Yosys sat proofs of every inserted buffer master and every clock master at
  each corner Liberty, each with a wrong-polarity control;
- a native `ECO_NOT_BUFFER` refusal of a profile that lists `clkinv_1` with
  its real Liberty function `(!A)` / `(!I)`, plus a positive control;
- native antenna passed;
- with `--signoff`: native-GDS Magic/KLayout DRC and antenna no worse than
  iteration 0, and the deck antenna check passing;
- with `--antenna-controls`: the planted controls;
- the frozen hashes of 19 inputs unchanged: Liberty x3, RC profiles x3,
  OpenRCX rules x3, both SDCs, the delay-cell profile, the cell policy, the
  routing and physical profiles, the technology/cell LEF, the cell GDS and
  the GDS layer map.

### Results (`docs/evidence/hold-closure.json`)

The table gives the final iteration: worst setup and hold slack in RustSTA on
native SPEF, in ns.

| | SKY130 | GF180 |
|---|---|---|
| Iterations | 3 | 2 |
| Inserted cells | 4 x buf_1 (15.0 um2) | 4 x dlya_1 (96.6 um2) |
| Iteration 0 worst hold, tt / ss / ff | -0.0045 / -0.0998 / +0.0574 | -0.394 / -0.899 / -0.180 |
| Final tt+nom setup / hold | 9.093 / +0.0817 | 7.464 / +0.2445 |
| Final ss+max setup / hold | 8.502 / +0.0667 | 4.853 / +0.3707 |
| Final ff+min setup / hold | 9.331 / +0.1148 | 8.549 / +0.1962 |
| OpenSTA 3.1.0 worst hold tt / ss / ff | 0.0817 / 0.0665 / 0.1147 | 0.2445 / 0.3705 / 0.1962 |
| RustSTA vs OpenSTA parity (48 slacks) | 0.305 ps | 0.455 ps |
| RustSys / Yosys CEC, 512-vector simulation | equivalent / passed / passed | equivalent / passed / passed |
| Native-GDS Magic / KLayout DRC, before -> after | 15 -> 7 / 11 -> 7 | 2 -> 2 / 2 -> 2 |
| Deck antenna, before -> after | 1 -> 0 | 0 -> 0 |
| LVS (Netgen, KLayout) | pass, pass | pass, pass |
| Routing (nets, wires, vias, patches) | 23 nets, 6157 / 109 / 5 | 23 nets, 5622 / 83 / 1 |

**Reported, not gating: OpenRCX on the collinear-merged DEF.** The DEF's RECT
patch lines are dropped first; OpenRCX ignores them anyway (5 lines sky130,
1 gf180). Both tools still close at every corner:

- sky130 OpenSTA worst hold tt / ss / ff: 0.0758 / 0.0557 / 0.1115 ns; parity
  0.44 ps.
- gf180 OpenSTA worst hold tt / ss / ff: 0.2406 / 0.3615 / 0.1938 ns; parity
  2.72 ps.

The b0 stream owns DEF merging and patch export.

**Runs**

- Full run: 282 receipts, 2:26 wall time, rustroad sha256
  `a8e57fc64fa4c2925e13f839f12edd85899ab768bb2b92a39ab3df2ec655ba98`. The
  largest host process peaked at 290,084 KiB RSS (the Verilator build); the largest RustSTA import used
  231,844 KiB. The largest signoff container peaked at 475,460 KiB (sky130
  Netgen LVS). The harness fails any process above 2 GiB.
- A second full run in a different directory gave byte-identical netlists,
  exported/powered Verilog, DEF, routes and min/nom/max SPEF at every
  iteration.
- `cts_drive_eval.py`: 1:09 wall time, 290 MiB.
- Rust: 137 tests (8.2 s, 536 MiB), strict Clippy (4.9 s), release build
  (17.9 s, 642 MiB).
- Python: 45 unit tests.

### Why DRC is not clean

The native GDS writer and the native extractor predate `Routes.patches` and do
not emit them. That is a known integration gap owned by the b0 stream. The
remaining Magic/KLayout items are the same-net notch and min-area defects that
the patches fix: sky130 m1.2 / m2.2, gf180 M2.2a. The ECO does not add new
rule types. sky130 drops from 15/11 to 7/7, and the m3.2 item and the antenna
item disappear.

## Scope and limitations

- Block or macro level only. IR, EM, SI, OCV and CPPR are unchecked. Density
  is not part of this gate.
- The hold estimates use one representative slew/load point. Final acceptance
  is always a re-timed, re-extracted layout.
- Port-endpoint hold ECO is unsupported (explicit infeasible).
- Antenna diodes can be planned only on nets of the mapped netlist. Nets
  generated by CTS are reported as infeasible.
- Antenna model limits:
  - OXIDE1 partial-net only; the cumulative forms are not modeled (neither
    PDK states them);
  - ports carry no diffusion;
  - cell-internal metal beyond the pin shapes is not modeled;
  - the diode is assumed to join the violating partial net, and the re-layout
    plus both checks confirm it.
- Native extraction and native GDS still omit `Routes.patches` (b0 stream).
  The OpenRCX reference parity on GF180 is 2.7 ps on the merged DEF, which is
  reported only.
- The lattice router's hard per-sink search budget blocks the stronger GF180
  clock-buffer profiles and some layout perturbations (router owner, W15).
- No reference tool source was read. OpenSTA, Yosys, Verilator, Magic,
  KLayout, Netgen and OpenRCX ran only as pinned black-box executables.
