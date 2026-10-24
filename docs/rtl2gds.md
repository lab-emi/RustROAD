# RTL-to-GDS gate (`scripts/rtl2gds.py`)

One command takes a design manifest from RTL to GDS on SKY130
(`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`) and checks the
result with independent oracles: synthesis, layout, gate sizing, hold and
antenna repair, extraction, multi-corner timing in two engines on two SPEF
sources, foundry DRC/LVS on the native GDS, equivalence and simulation
against the RTL, artifact identity, determinism and negative controls. Every
design of `fixtures/designs/*.json` goes through the same gate; the manifest
selects the physical recipe (`legacy` or `track`). It reuses the existing
engines (`hold_closure.Flow`, `compare_spef.openrcx`, `scripts/signoff`,
`check_cell_policy`, `scale_flow.deconflict`) instead of copying them.
Third-party tools run only as black-box executables.

## Scope (read before the results)

Block level only: no pad ring, no seal ring. IR drop, electromigration,
signal integrity (couplings are grounded at factor 1), OCV and CPPR are
**unchecked**. Density is reported separately (`density_status`, owner
decision) and is not part of the verdict: GF180 fails its coverage rules
without fill, the SKY130 windowed deck cannot evaluate blocks smaller than
its window. The minimum pulse width of asynchronous set/clear inputs is a
requirement on the reset source outside the block (no clock reaches those
pins; neither engine checks them): it is unchecked, the pins are listed per
design, their recovery/removal checks are timed. Corners: tt+nom, ss+max,
ff+min. A design passes only on the PDK rows marked `passed` below; nothing
else is claimed.

## Results (evidence `docs/evidence/rtl2gds-closure.json`)

Gate runs `.work/runs/E6-<design>` (worktree `.work/wt/closure-dev2`) at
RustROAD `039bfe5` (clean tree, release binary `4ec81093`), the same RustSTA
pin `06b1c9f` (ruststa `3e80fde6`, rusteda `13050846`) and RustSys policy
producer `3de13b3a` as the E3 round below, both PDKs, controls and the
determinism rebuild included, eight gates in parallel. The closure ECOs of
`docs/closure.md` (buffer insertion, `whatif`-verified sizing, path-aware
hold, seeded timing-driven placement) are what changed:

| Design | PDK | Gate | Failed gating items | Unverified | Final cells: hold / ECO buf / diodes (resized) | Worst setup ss / hold ss (RustSTA, native) | DRV viol. | Layouts (final) | Wall s | Peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| pipeline | sky130 | passed | - | - | 4 / 0 / 0 (0) | +8.491 / +0.065 | - | 3 (2) | 57 | 464 |
| pipeline | gf180 | passed | - | - | 4 / 0 / 0 (0) | +4.844 / +0.362 | - | 2 (1) | 66 | 434 |
| dense | sky130 | passed | - | - | 0 / 0 / 0 (0) | +8.664 / +0.185 | 0 | 1 (0) | 56 | 464 |
| dense | gf180 | passed | - | - | 1 / 1 / 0 (0) | +6.636 / +0.186 | 0 | 3 (2) | 75 | 437 |
| counter | sky130 | failed | timing_accuracy | - | 13 / 0 / 0 (0) | +6.407 / +0.082 | 0 | 2 (1) | 80 | 490 |
| counter | gf180 | passed | - | - | 18 / 15 / 0 (5) | +0.056 / +0.102 | 0 | 4 (3) | 105 | 441 |
| uart_tx | sky130 | failed | timing_accuracy | - | 32 / 0 / 0 (0) | +6.952 / +0.095 | 0 | 2 (1) | 109 | 493 |
| uart_tx | gf180 | failed | drv, timing_accuracy | - | 17 / 58 / 0 (44) | +0.263 / +0.278 | 38 | 6 (5) | 340 | 444 |
| fifo | sky130 | failed | timing_accuracy, lvs_netgen, lvs_klayout | - | 34 / 0 / 1 (0) | +6.506 / +0.249 | 0 | 3 (2) | 118 | 491 |
| fifo | gf180 | failed | drv, timing_accuracy | - | 17 / 43 / 0 (20) | +0.108 / +0.213 | 9 | 5 (4) | 217 | 446 |
| alu | sky130 | failed | timing_accuracy, lvs_netgen, lvs_klayout | timing_setup, timing_hold, mpw_min_period, drv | 0 / 1 / 1 (1) | +5.746 / +0.883 | 0 | 3 (2) | 201 | 519 |
| alu | gf180 | failed | hold_closure, timing_setup, drv, timing_accuracy | - | 0 / 85 / 0 (19) | -4.578 / +1.321 | 381 | 6 (3) | 438 | 453 |
| regfile | sky130 | failed | timing_accuracy, lvs_netgen, lvs_klayout | - | 53 / 19 / 1 (4) | +6.899 / +0.111 | 0 | 6 (5) | 398 | 498 |
| regfile | gf180 | failed | drv, timing_accuracy | timing_setup, timing_hold, mpw_min_period | 64 / 106 / 0 (44) | +0.044 / +0.447 | 114 | 6 (5) | 602 | 465 |
| mult | sky130 | failed | timing_accuracy | - | 3 / 0 / 0 (0) | +4.225 / +0.114 | 0 | 2 (1) | 253 | 520 |
| mult | gf180 | failed | hold_closure, timing_setup, drv, timing_accuracy | - | 2 / 23 / 0 (34) | -10.588 / +0.550 | 579 | 4 (3) | 806 | 458 |

"Final cells" are instances of the final (selected) layout: hold cells,
`rr_eco_buf_*` buffers and antenna diodes, with the resizes the loop
applied over all layouts in parentheses. Every row passes controls (11/11),
determinism (full second build, byte-identical) and artifact binding;
`density` is reported only.

Against the E3 round (table below): GF180 counter now passes every gating
status; GF180 uart_tx, fifo and regfile close setup and hold on both SPEFs
and keep only `drv` (+ `timing_accuracy`); regfile sky130 closes its port
max-capacitance (`drv`) and `antenna_native`. Not closed:

1. **`timing_accuracy` (every track design but dense):** the RustSTA
   effective-capacitance residual (owner RustSTA, unchanged pin).
2. **GF180 `drv` (uart_tx 18, fifo 3, regfile 39 violating nets at the
   selected layout; alu, mult more).** 10 GF180 7t families cannot meet the
   1.0 ns `set_max_transition` at ss even at 5 fF with a 0.2 ns input
   (Liberty, best member of each: aoi211 1.15, aoi221 1.57, aoi222 1.76,
   nor4 1.31, oai22 1.21, oai221 1.56, oai222 1.98, oai31 1.11, oai32 1.96,
   oai33 2.42 ns); RustSys maps 10 of them into uart_tx, 2 into fifo, 14 into
   alu, 1 into regfile, 8 into mult, and their nets are reported
   `library_limited` (10 of uart_tx's 18, 2 of fifo's 3). Only synthesis
   (a GF180 dont_use for this limit, RustSys/cell-policy owner) or the
   limit owner can remove them. The rest are nets where a buffer would cost
   a setup-critical sink its slack (setup guard), transitions propagated
   from those nets, and resizes the `whatif` budget did not reach.
3. **GF180 alu and mult `timing_setup`/`hold_closure`:** setup-infeasible at
   10 ns at ss even with ideal wires (critical-path table below; mult also
   fails tt by -0.83 ns ideal). Hold closes on both (mult +0.550 ns ss).
4. **sky130 `lvs_netgen`/`lvs_klayout` (fifo, alu, regfile):** the antenna
   ECO's `diode_2` (owner: the antenna/diode LVS stream). fifo and regfile
   need a diode only now: their layouts changed (fifo: `data_i[3]` met3 run
   1.04 x the limit after the path-aware hold layout; regfile: the loop now
   gets past DRV to the antenna ECO).
5. **OpenRCX timing `unverified` on two rows:** RustSTA refuses the OpenRCX
   SPEF of alu sky130 (`disconnected RC network in a_i[0]`) and regfile
   GF180 (`RC mesh/parallel edge in waddr[1]`, a Metal4 jumper next to the
   I/O pin). Native-SPEF timing is complete on both; the routes are the
   track router's (owner: router/extraction).

### Critical paths (`scripts/critical_path.py`, evidence `docs/evidence/critical-path.json`)

Worst setup slack and the implied minimum period (`period - slack`) at ss:
pre-layout = RustSTA without parasitics and with the ideal clock (GF180:
`ideal` wires; SKY130: the Liberty default wire load, an upper bound of
ideal) on the synthesized netlist and on the final closure netlist;
post-route = the gate's native-SPEF analysis with the propagated clock. No
period was changed (the owner decides on the 10 ns of `cts.sdc`).

| Design | PDK | Period ns | Pre-layout synth: ss slack / min period (wire model) | Pre-layout final netlist ss | Post-route ss (native SPEF) | Post-route tt | Worst ss path | Setup-feasible at 10 ns with ideal wires |
|---|---|---|---|---|---|---|---|---|
| pipeline | sky130 | 10 | +8.877 / 1.12 (wireload_lumped) | +8.728 / 1.27 | +8.491 / 1.51 | +9.088 / 0.91 | r2/CLK -> y[0] | yes |
| pipeline | gf180 | 10 | +6.396 / 3.60 (ideal) | +5.205 / 4.80 | +4.844 / 5.16 | +7.461 / 2.54 | r1/CLK -> r3/D | yes |
| dense | sky130 | 10 | +9.035 / 0.97 (wireload_lumped) | - | +8.664 / 1.34 | +9.172 / 0.83 | r1/CLK -> low_bit | yes |
| dense | gf180 | 10 | +7.338 / 2.66 (ideal) | +6.816 / 3.18 | +6.636 / 3.36 | +8.194 / 1.81 | r5/CLK -> y[3] | yes |
| counter | sky130 | 10 | +6.804 / 3.20 (wireload_lumped) | +6.658 / 3.34 | +6.407 / 3.59 | +8.124 / 1.88 | r1/CLK -> r4/D | yes |
| counter | gf180 | 10 | +0.382 / 9.62 (ideal) | +0.541 / 9.46 | +0.056 / 9.94 | +5.093 / 4.91 | r2/CLK -> r7/D | yes |
| uart_tx | sky130 | 10 | +7.406 / 2.59 (wireload_lumped) | +7.406 / 2.59 | +6.952 / 3.05 | +8.475 / 1.53 | r4/CLK -> r5/D | yes |
| uart_tx | gf180 | 10 | +0.178 / 9.82 (ideal) | +1.006 / 8.99 | +0.263 / 9.74 | +5.201 / 4.80 | r6/CLK -> r0/D | yes |
| fifo | sky130 | 10 | +7.144 / 2.86 (wireload_lumped) | +6.946 / 3.05 | +6.506 / 3.49 | +8.145 / 1.86 | r6/CLK -> r6/D | yes |
| fifo | gf180 | 10 | +0.503 / 9.50 (ideal) | +0.615 / 9.39 | +0.108 / 9.89 | +5.102 / 4.90 | r5/CLK -> r5/D | yes |
| alu | sky130 | 10 | +6.125 / 3.88 (wireload_lumped) | +6.125 / 3.87 | +5.746 / 4.25 | +7.555 / 2.44 | b_i[1] -> zero_o | yes |
| alu | gf180 | 10 | -3.071 / 13.07 (ideal) | -3.041 / 13.04 | -4.578 / 14.58 | +2.652 / 7.35 | b_i[0] -> zero_o | no |
| regfile | sky130 | 10 | +8.020 / 1.98 (wireload_lumped) | +7.537 / 2.46 | +6.899 / 3.10 | +8.305 / 1.69 | r47/CLK -> rdata_a[7] | yes |
| regfile | gf180 | 10 | +3.037 / 6.96 (ideal) | +2.297 / 7.70 | +0.044 / 9.96 | +4.966 / 5.03 | r42/CLK -> rdata_b[2] | yes |
| mult | sky130 | 10 | +4.161 / 5.84 (wireload_lumped) | +4.161 / 5.84 | +4.225 / 5.77 | +6.867 / 3.13 | b[4] -> r11/D | yes |
| mult | gf180 | 10 | -12.053 / 22.05 (ideal) | -11.187 / 21.19 | -10.588 / 20.59 | -0.187 / 10.19 | b[1] -> r11/D | no |

alu and mult are setup-infeasible at 10 ns on GF180 ss with ideal wires
(13.07 / 22.05 ns minimum period for the synthesized netlists; mult's path
is an xor3 chain, 3.3-4.4 ns per xor3 at ss); every other design meets
10 ns at ss after layout.

### Earlier round (E3, before the closure ECOs)


Gate runs `.work/runs/E3-<design>` at RustROAD `0acb28a` (clean tree,
release binary `2911f28b`), RustSTA `06b1c9f` (ruststa `3e80fde6`, rusteda
`13050846`; stream sta3 `ebc7037` plus the shared convergence `6af9936`),
RustSys policy producer `3de13b3a`, both PDKs, controls and the determinism
rebuild included:

| Design | PDK | Recipe | Gate | Failed gating items | Hold cells / resized / diodes | Worst setup ss / hold ss (RustSTA, native) | Max parity native / OpenRCX (ps) | Wall s | Peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|
| pipeline | sky130 | legacy | passed | - | 4 / 0 / 0 | +8.491 / +0.065 | 0.31 / 0.32 | 51 | 464 |
| pipeline | gf180 | legacy | passed | - | 4 / 0 / 0 | +4.844 / +0.362 | 0.10 / 0.14 | 61 | 437 |
| dense | sky130 | track | passed | - | 0 / 0 / 0 | +8.664 / +0.185 | 0.97 / 0.86 | 52 | 465 |
| dense | gf180 | track | passed | - | 4 / 0 / 0 | +6.595 / +0.553 | 0.09 / 0.10 | 66 | 439 |
| counter | sky130 | track | failed | timing_accuracy | 10 / 0 / 0 | +6.428 / +0.093 | 1.24 / 0.85 | 71 | 491 |
| counter | gf180 | track | failed | hold_closure, timing_setup, timing_hold, drv, timing_accuracy | 0 / 12 / 0 | -0.469 / -1.539 | 1.13 / 1.37 | 77 | 445 |
| uart_tx | sky130 | track | failed | timing_accuracy, lvs_netgen, lvs_klayout | 42 / 0 / 1 | +7.022 / +0.072 | 3.09 / 1.87 | 112 | 495 |
| uart_tx | gf180 | track | failed | hold_closure, timing_setup, timing_hold, drv, timing_accuracy | 0 / 65 / 0 | -1.358 / -2.226 | 4.36 / 5.21 | 109 | 445 |
| fifo | sky130 | track | failed | timing_accuracy | 121 / 0 / 0 | +6.492 / +0.104 | 2.13 / 1.19 | 143 | 489 |
| fifo | gf180 | track | failed | hold_closure, timing_setup, timing_hold, drv, timing_accuracy | 0 / 8 / 0 | -0.943 / -2.438 | 1.49 / 1.75 | 103 | 444 |
| alu | sky130 | track | failed | timing_accuracy, lvs_netgen, lvs_klayout | 0 / 2 / 1 | +5.718 / +0.888 | 3.66 / 2.24 | 188 | 518 |
| alu | gf180 | track | failed | hold_closure, timing_setup, drv, timing_accuracy | 0 / 188 / 0 | -3.946 / +1.267 | 2.11 / 2.56 | 152 | 453 |
| regfile | sky130 | track | failed | antenna_native, drv, timing_accuracy | 287 / 0 / 0 | +6.770 / +0.109 | 2.99 / 2.13 | 430 | 496 |
| regfile | gf180 | track | failed | hold_closure, timing_hold, drv, timing_accuracy | 0 / 102 / 0 | +0.310 / -2.846 | 3.12 / 3.98 | 223 | 462 |
| mult | sky130 | track | failed | timing_accuracy | 3 / 0 / 0 | +4.171 / +0.098 | 3.65 / 2.14 | 255 | 521 |
| mult | gf180 | track | failed | hold_closure, timing_setup, drv, timing_accuracy | 2 / 313 / 0 | -12.538 / +0.120 | 1.32 / 1.57 | 237 | 461 |

`passed` = every gating status passed (exit 0 for pipeline and dense, both
PDKs). Every other row exits 1. Not listed as failing means passed: every
failing row still passes XOR, Magic and KLayout DRC, the antenna deck (except
where named), CEC pre-layout/final/powered (RustSys and the second oracle),
4096-cycle co-simulation, cell proofs, artifact binding, the full
determinism rebuild and every control (`planted_hold_uncertainty` stands in
for `removed_hold_cell` where no hold cell exists). "Hold cells / resized /
diodes" are the cells the hold, sizing and antenna ECOs planned over the
closure loop (GF180 rows with 0 hold cells: the hold ECO was infeasible or
never reached).

Root causes of the E3 failures (items 2 and 4 are what `docs/closure.md`
addresses; see the E6 table above). No threshold, Liberty, deck, harness
status logic or SDC value was changed to address any of them (the SDC
additions in "SDC" below were made before closure was run):

1. **`timing_accuracy` (all track designs except dense, both PDKs, 0.85-5.2
   ps; gate 1 ps).** RustSTA computes slightly smaller driver delays into
   long RC nets than OpenSTA with matching input slews. fifo sky130 ss,
   `r2/CLK -> data_o[7]` setup: clock path 0.55 ps, `r2/Q` (dfrtp_2 into
   83.7 fF of routed load) 1.05 ps, `g12` mux4 0.53 ps, total 2.13 ps. This is
   the high-resistance effective-capacitance residual the RustSTA stream
   declared after `ebc7037`; the merged pin `06b1c9f` is the build used here.
   Pipeline and dense (short nets) stay below 1 ps. Owner: RustSTA.
2. **GF180 `hold_closure`, `timing_setup`, `timing_hold`, `drv` (counter,
   uart_tx, fifo, alu, regfile, mult).** At ss (125 C, 3.0 V) the 10 ns
   constraint set is not met: setup down to -0.47 (counter), -1.36 (uart),
   -0.94 (fifo), -3.95 (alu), -12.5 ns (mult); input-to-register hold and
   reset removal need 0.5-2.9 ns of delay at ss (removal time about 1 ns
   behind a 0.72 ns clock latency against the 0.2 ns input delay) while
   several of the same endpoints have less than 0.4 ns of ss setup slack, so
   delay insertion at the sink is infeasible (`HOLD_ECO_INFEASIBLE`) or the
   loop reaches its 8-layout bound. Max-transition (1.0 ns) and
   max-capacitance violations come from weak drivers into long analytic-
   placement wires (`dffrnq` clk-to-Q 3.6 ns at ss with 1.1 ns output slew).
   The new gate-sizing ECO lowers DRV counts (mult 752 -> 334, regfile
   578 -> 330 violations) but upsizing also raises the load of the previous
   stage; it cannot close these blocks. Needs timing-driven placement,
   buffering and path-based hold fixing (not implemented).
3. **sky130 `lvs_netgen`, `lvs_klayout` (uart_tx, alu).** The antenna ECO
   inserts `sky130_fd_sc_hd__diode_2` on input-port nets (met3 runs from the
   edge I/O pins, ratio 1.05-1.10 of 400). The frozen LVS setup cannot match
   that cell: Netgen reports its device-less VGND/VPWR/VPB pins as
   disconnected nodes, the KLayout deck gives `diode_2 NoMatch` and skips the
   top (the same class as `conb_1`, which the scale stream removed). A
   diode-free repair (router jumper to an upper layer near the gate) or an
   owner decision on the deck is needed. DRC, XOR and both CECs pass on
   these layouts.
4. **regfile sky130 `antenna_native`, `drv`.** `raddr_a[0]` reaches 396.6 of
   the 400 limit (native safety margin 0.10, so it is flagged; the Magic deck
   passes), and the 57-64-sink input ports (`raddr_*`, `rst_n`) exceed
   `set_max_capacitance 0.3` pF: port nets cannot be resized; port
   buffering is not implemented. The loop stops when no repair applies.

Cross-run determinism: pipeline, dense, counter and alu (both PDKs) give
byte-identical netlists, DEF, routes, GDS and native SPEF in two independent
complete runs (E2 at `a469666`, E3 at `0acb28a`; no physical code between
them); OpenRCX SPEF differ only in `*DATE`. Every row's own determinism
status (a second full build in the same run) passed.

Stretch (SERV, rv_core): see "Stretch designs" below.

## How to run

```sh
# Release binary (rule 9: about 0.7 GiB, 4 jobs)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  --setenv=CARGO_BUILD_JOBS=4 cargo build --release --locked --bins
# Private copy of the pinned RustSTA pair (the shared directory is replaced by other rounds)
mkdir -p .work/tools/sta-gate
cp /mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin/{rusteda,ruststa} .work/tools/sta-gate/
sha256sum .work/tools/sta-gate/*   # must equal fixtures/flow/rtl2gds-tools.json
# One design (both PDKs, controls and determinism rerun included)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G /usr/bin/time -v -o gate.time \
  timeout 5400s python3 scripts/rtl2gds.py --design fixtures/designs/<name>.json \
  --pdk sky130,gf180 --run .work/runs/NEW-<name> --base-rev empty
# The tracked evidence from finished runs
python3 scripts/rtl2gds_evidence.py --out docs/evidence/rtl2gds-final.json .work/runs/NEW-*
```

Run the eight designs four at a time (measured below); never edit the
worktree while a run is in flight: the artifact binding records the git
status including untracked files, and a changed tree fails
`artifact_binding` (it did, in the first evidence round).

Options: `--rustroad BIN` (default `target/release/rustroad`), `--tools FILE`
(must be byte-identical to the manifest's frozen pin file, else
`tools_identity` fails), `--tool NAME=PATH` (override one pinned path; a
missing or different binary is never a pass), `--no-controls`,
`--no-determinism` (both then report `unverified`), `--preflight-only`,
`--evidence`/`--evidence-from` (single-run evidence). `--run` must not exist
and `--base-rev` must be `empty` (`REVISION_CONFLICT` otherwise). Exit 0 =
every gating status passed (or is an allowed `not_applicable`), 1 = at least
one failed, 3 = none failed but at least one is unverified.

The podman images are the pinned signoff image (`fixtures/flow/signoff-pinned.json`,
`4c48415d8bd0`) and the OpenROAD/OpenRCX image `f152917ebc45`.

## Design manifest (`rustroad.design.v2`)

One schema for every design (`fixtures/designs/<name>.json`, checked by
`rtl2gds_design.manifest_problems`; version 2 because the field set changed
when the track-recipe designs joined the gate). Every tracked file carries
its sha256.

| Field | Meaning |
|---|---|
| `schema`, `name`, `description`, `top`, `rtl[]` | identity and sources |
| `clock` | `{name, port}`; `port: null` = combinational design: `name` is a virtual clock that only times the I/O (no clock port, no CTS, RustSTA must report the ideal clock model with zero physical clock domains) |
| `sdc` | `clock` (primary or virtual-clock SDC; CTS reads it), `io_template` (`postroute-io.sdc.tmpl`, or `postroute-io-virtual.sdc.tmpl` without `set_propagated_clock`), `rendered_sha256` (the signoff SDC rendered from the RTL port list) |
| `recipe.physical` | `legacy`: tracked floorplan, fixed-order placer, lattice router (clock route, then all nets), the original workspace limits; `track`: analytic placer and track router (`route --scope all`) with the scale limits |
| `recipe.floorplan` | track only: `{utilization, aspect, margin_um}`; the gate re-runs `rustroad floorplan` on the mapped netlist and requires byte-identical floorplan and physical configuration (part of `implementation`); `null` for legacy |
| `stretch`, `pdks_unavailable` | stretch designs; PDKs whose floorplan cannot be derived (with the reason) |
| `pdk_profiles`, `cell_policy`, `signoff_manifest`, `tools` | `fixtures/pdk/local-routing.json`, the cell policy, the signoff image pins, the frozen tool pin file |
| `pdks.<tag>` | `floorplan`, `physical_config`, `routing_config`, `delay_cells`, `gds_layer_map`, `rc_profiles.{min,nom,max}`, `corners` (tt+nom / ss+max / ff+min), `frozen` (sha256 of every file the engine derives); optional `physical_profile` (track), `netlist` (literal mapped netlist instead of synthesis: dense), `sizing_profile` (enables the gate-sizing ECO) |
| `verification` | co-simulation vectors and seed, RustSys budgets, Yosys sequence depth |
| `budgets` | `hold_margin_ns` (gate), `hold_eco_target_margin_ns` (closure target, >= the gate margin), `setup_margin_ns`, `hold_eco_setup_margin_ns`, `parity_ns`, `max_hold_iterations` (closure layouts, every ECO kind), process/container time and memory |

The owner floors are unchanged and fixed in code (`OWNER_FLOORS`, `GATE_FLOORS`);
`hold_eco_target_margin_ns` below `hold_margin_ns` fails `budget_floors`.
`scripts/scale_manifests.py` writes the track manifests (`--refresh-manifest`
re-reads every hash without regenerating floorplans; it also refreshes the
legacy pipeline manifest). Budgets: pipeline 4 closure layouts, target =
gate margin (its earlier outcome is unchanged); track designs 8 layouts (the
loop also sizes gates; regfile GF180 alone took three hold rounds) and a
0.07 ns ECO target (20 ps guard band; the gate still checks 0.05 ns on both
SPEFs).

### SDC (fixed before closure)

All track designs: 10 ns clock, 0.1 ns clock transition, 50 ps uncertainty,
`set_max_capacitance 0.3`, `set_max_transition 1.0` (clock domain),
`set_max_fanout 64` (added so the fanout DRV is checked at all: neither
Liberty has `default_max_fanout`; 64 is one power-of-two bound for every
design, the largest fanout in the set, regfile `rst_n`, is exactly 64), I/O
delays 0.2 ns, input transition 0.1 ns, output load 0.01 pF. The ALU uses a
virtual clock with the same limits. No SDC value was changed after closure
started; the GF180 timing failures below are reported against these
constraints.

## Steps and statuses

Every status is `passed`, `failed`, `unverified` or `not_applicable` and is
reported separately per PDK (`pdks[].statuses`). `not_applicable` is
accepted only for the items `rtl2gds_design.NOT_APPLICABLE` names (fixed in
code: `mpw_min_period` of a combinational design, when RustSTA reports
`pulse_width_status unconstrained` with zero rows and OpenSTA has no
pulse-width or period row at any corner) and is listed in
`not_applicable_items`; it is never folded into `passed` silently. Gating
statuses:

| Status | Producer | Independent oracle / check |
|---|---|---|
| `inputs_frozen` | manifest hashes before and after the run | sha256 |
| `implementation` | RustSys synth (or the literal netlist), RustROAD floorplan check (track), import/place/CTS/route, exports, OpenRCX, export-gds | the flow completed; track: the regenerated floorplan and physical configuration are byte-identical to the tracked ones |
| `cell_policy` | mapped, final and powered netlists | `check_cell_policy.py` against the PDK exclusion lists |
| `cec_prelayout` | RTL vs mapped netlist | RustSys `check synth` and Yosys (`equiv_simple -seq 2`, `equiv_induct`); exhaustive co-simulation when Yosys cannot read the RTL of a combinational design |
| `hold_closure` | `hold_closure.Flow.close()`, native SPEF decisions, <= `max_hold_iterations` layouts (pipeline 4, track 8); track: gate sizing, hold ECO with a reference-SPEF guard band, antenna ECO | RustSTA hold >= the ECO target and setup >= 0 at tt/ss/ff (hold-eco keeps an estimated ss setup margin of 0.05 ns) |
| `antenna_native` | `check-antenna` on the final layout | conservative native model (margin 0.10) |
| `extraction` | native min/nom/max + OpenRCX on the exported DEF | each SPEF sha256 equals its producer's receipt |
| `timing_setup` | RustSTA and OpenSTA, native and OpenRCX SPEF | setup >= 0 at tt and ss |
| `timing_hold` | same | hold >= +0.05 ns at tt, ss and ff |
| `mpw_min_period` | RustSTA default rules (`groups=worst, pins=all`) gate | OpenSTA met; min-period rows may be absent only if both engines have none; unchecked pins accepted only when they are exactly the asynchronous set/clear pins (count per corner, no row in either engine; listed); `not_applicable` for a combinational design as above |
| `drv` | RustSTA `drv_status` and limits (every kind checked at every corner) | OpenSTA `report_check_types`: no violator |
| `timing_accuracy` | RustSTA vs OpenSTA per corner per SPEF | endpoint slacks <= 1 ps, same endpoints; MPW/period (`--pulse-groups last --pulse-pins clock-ends`) <= 1 ps; equal DRV violator sets |
| `gds_xor` | native GDS vs KLayout DEF reference streamout | strmxor (geometry) + label lists |
| `drc_magic`, `drc_klayout` | native GDS | Magic `drc(full)`, KLayout main deck: 0 items |
| `antenna_deck` | native GDS | sky130 Magic, gf180 KLayout (the other tool is reported) |
| `lvs_netgen`, `lvs_klayout` | native GDS vs powered netlist | Netgen, KLayout (own results, port names checked) |
| `cec_final` | RTL vs final (post-CTS/ECO) netlist | RustSys and Yosys |
| `cec_powered` | RTL vs powered netlist | RustSys with `--supply-port VDD/VSS` after removing the listed physical-only cells; Yosys on the unpowered view |
| `simulation` | generic random co-simulation (Verilator) | cycles, comparisons and end time must equal the expected counts |
| `cell_proofs` | inserted hold and clock cells | Yosys sat proof per corner, wrong polarity rejected, native ECO refuses an inverter |
| `artifact_binding` | sha256 of every artifact, tool and input + RustROAD revision | re-hashed after every oracle and control |
| `determinism` | a second full build in `<pdk>_rerun/` | byte compare (OpenRCX `*DATE` line removed, listed) |
| `controls` | eleven planted defects | each rejected by its own oracle (`removed_hold_cell` is replaced by `planted_hold_uncertainty` when no hold cell exists) |

Reported only: `density` (`density_status`; owner decision: not part of the
block DRC verdict). Global statuses: `tools_identity` (pinned binaries and the
frozen pin file), `openroad_image`, `design_manifest` (every tracked input by
sha256), `budget_floors`, `manifest_provenance`, `sdc_rendering`. `timing_accuracy_status` is also given at
the top level together with `timing_accuracy_failures` (every failing
corner/check with its worst rows).

A timing engine that refuses a SPEF source makes every timing status of that
source `unverified` (never passed); the other source and every other oracle
still run. Endpoints without a data arrival are accepted only when the pin
is constant-driven (tie-cell output, literal or an alias of one, traced in
the netlist independently of the engine; dense has two). Scope (`scope` in
the summary): as stated at the top; clocks are propagated through the
RustROAD clock tree, a combinational design uses the ideal model of its
virtual clock (zero physical clock domains required).

## Closure loop (`hold_closure.Flow.close`)

Each iteration is a complete layout (import, place, CTS, route, native
extraction, RustSTA on native SPEF, native antenna check). Legacy recipe:
unchanged (hold ECO while not closed, then antenna ECO). Track recipe
(`sizing_profile` set; details, estimates and bounds in `docs/closure.md`):

1. setup < 0 or any DRV violation (RustSTA `drv` report) -> **DRV/setup
   ECO** (`closure_eco`): buffer insertion (`buffer_sinks`: repeaters on
   overloaded or long nets, input-port buffering, critical-sink isolation;
   sinks clustered by their DEF positions, a setup guard keeps critical
   sinks on their driver) plus gate sizing decided by RustSTA `whatif` on
   the layout's SPEF (resizes are exact there), applied as one plan by
   `rustroad apply-eco --sizing-profile`; while setup fails, timing-driven
   net weights for the next placement. Buffer masters and families come
   from `fixtures/pdk/<pdk>-sizing.json` (Liberty-derived, cell policy
   applied, now with per-corner drive data). CTS, hold, tie and diode cells
   are never resized.
2. hold below `hold_eco_target_margin_ns` -> **path-aware hold ECO**
   (`hold-eco --pins-report`, partial plans): delay goes on the worst hold
   path where the setup slack of every path through the point allows it
   (a branch pin after the divergence, or one chain in front of all sinks
   of a small net such as an input port), planned 0.03 ns above the target.
   When native SPEF is closed, OpenRCX on the same DEF is timed by RustSTA;
   a reference hold below the gate margin raises the native target by the
   shortfall + 10 ps for the next hold ECO (uart sky130 was 0.046 ns on
   OpenRCX with 0.078 ns native). After the first DRV/setup ECO, a layout
   that needs both gets both (hold plan first, then the DRV/setup plan on
   top of it).
3. antenna violated on a setup- and DRV-clean layout -> antenna ECO.

From the second layout on, placement is seeded from the previous one
(`place --seed`: legalization from the previous positions, no global
solve). DRV/setup ECOs stop after two consecutive layouts without progress.
The loop's final layout is its best complete layout (fewest failing
categories, then the smallest deficit; `selected_iteration` in the record
when that is not the last one).

A hold ECO with no feasible point at all, or an ECO layout that cannot be
built, ends the loop (recorded, with the infeasible endpoints and the setup
budget of each point of their hold paths); every oracle still runs on the
selected layout and `hold_closure` reports the failure. Native extraction lists declared
nets without any pin (`pinless_signal_nets`, dangling RustSys wires)
separately from unannotated nets.

Track-router change found by the gate: a route path that ran over, or
dropped a via onto, its own pin away from the escape, and escape stubs lying
wholly inside the pin (L-shaped pins covered by a union test), connected the
pin twice. OpenRCX models the pin as a node, so these became parallel
resistors and RustSTA refused the SPEF (`T0070 RC mesh`) on every GF180
track design. Own pins now block such edges and vias; in-pin stubs are
trimmed. The native extractor never saw these loops.

### Logic oracles

- **Powered netlist.** Instances whose master matches the cell policy's
  `physical_only` globs and has no LEF `OUTPUT` pin are removed and listed
  (sky130: 123 = 32 `tapvpwrvgnd_1` + 91 fillers; gf180: 119 = 32 `filltie` +
  87 fillers). A policy glob matching a cell with an output pin is refused,
  not filtered. RustSys keeps the supply connections and gets the supply
  ports; Yosys gets an unpowered view (68 supply-pin connections and the two
  supply ports removed, listed).
- **Co-simulation.** The testbench is generated from the RTL port list: all
  non-clock inputs come from a fixed-seed xorshift64, the clock port toggles,
  and every output is compared with `!==` before and after each edge (4096
  cycles). Verilator 5.032's default optimisation constant-folds variables
  that an `initial` block assigns before its first delay (a counter read 0
  after 4096 increments), which made a first version vacuous. The testbench
  now initialises everything in declarations, is built with `-O0`, prints
  cycles/comparisons/time, and the gate requires the expected counts; the
  bad-RTL control must make it report mismatches.

- **Yosys oracle.** Gold is `proc; flatten; memory; opt; splitnets`
  (RustSys names register bits `\\q[0]`, splitnets gives the RTL the same
  names so `equiv_make` pairs the state), both sides get `async2sync`
  (asynchronous reset as an immediate override), then `equiv_simple -seq 2`
  and `equiv_induct`. Without the register pairing nothing sequential was
  proven (uart_tx); without `memory` fifo was rejected.
- **RTL Yosys cannot read.** Yosys 0.67 rejects a package import in a module
  header (alu, rv_core). Ports then come from `verilator --xml-only`
  (`port_source`), and for a combinational design with at most 22 input bits
  the second CEC oracle is an exhaustive co-simulation of the RTL against the
  netlist (every input combination: 524,288 for the ALU); a clocked design
  would keep Yosys `unverified`.
- **Rail ties.** Signal pins wired to a supply rail (the sky130 rail-input
  tie inverters) get that rail's constant in both powered views (listed in
  `filtering.rail_constants`); RustSys refuses a supply port read as logic.

### Negative controls (both PDKs, every design)

| Control | Planted defect (copy, never a bound artifact) | Oracle that must reject it |
|---|---|---|
| `drc_spacing` | two metal1 rectangles below minimum spacing | Magic and KLayout DRC |
| `label_swap` | two input-port labels exchanged | Netgen, KLayout LVS (own results) and the XOR label comparison |
| `removed_via` | one routing via of a two-terminal signal net deleted | Netgen and KLayout LVS |
| `removed_hold_cell` | the hold chain of the worst iteration-0 endpoint that a chain feeds, fresh layout | RustSTA and OpenSTA hold gate |
| `planted_hold_uncertainty` | (only when no hold cell exists) `set_clock_uncertainty -hold 2.0` in a copy of the SDC | RustSTA and OpenSTA hold gate |
| `tampered_sdc` | `set_input_delay 0.2` -> `0.6` in the rendered SDC | frozen-input sha256 |
| `tampered_spef` | every R of the native ss SPEF x1000 | SPEF sha256 vs the extraction receipt (re-timing reported) |
| `inverted_output` | bit 0 of the first output driven through a PDK inverter in a copy of the final netlist (names kept) | RustSys `not-equivalent`, the second CEC oracle, co-simulation mismatches |
| `missing_tool` | `--tool yosys=<absent>` | exit 3, `tools_identity` unverified |
| `relaxed_manifest` | untracked manifest copy with `parity_ns` 0.003 | exit 1, `budget_floors` failed, `manifest_provenance` unverified |
| `tampered_artifact` | one GDS byte flipped; separately one recorded hash edited | binding re-verification |
| `antenna_planted` | a long wire on an input pin without a diode (fewest sinks first; up to 64 pins x 600/1200/2400 um sky130, 2000/4000/8000 um gf180) whose I/O-layer partial net reaches a gate | native check-antenna and the foundry deck |

`inverted_output` replaces the earlier RTL-wrapper mutant: the wrapper moved
every register under a hierarchy prefix, so name-based state matching in
RustSys and Yosys could not start (it was `unverified`, not rejected).
`antenna_planted` needs a pin whose route stays on the I/O layer: a route
that leaves the pin upward has no gate at that layer's etch. All controls
were rejected by their oracle on every row of the evidence run.

## Hold and antenna ECO hardening

Detailed in `docs/hold-closure.md` (sections "Report binding", "Sink pins",
"Safety margin"); summary:

- `apply-eco`/`hold-eco` refuse an `insert_buffer` sink that is not a cell-LEF
  `INPUT` pin (`ECO_INVALID`); the phase-B1 `r0/Q` plan is refused.
- `hold-eco` binds reports to `--timed-netlist` through the timing engine's
  own record: the STA revision the reports name must list that file (by
  xxh3-64) as its netlist source (`REPORT_PROVENANCE` when the record cannot
  be read). It also checks the recorded sha256, one STA revision, every row
  and path stage, and that the edited netlist is refined by the timed
  netlist; stale or foreign reports fail `STALE_REPORT`.
- `check-antenna` keeps a derived 0.10 margin and `antenna-eco` sizes diodes
  against it; a `partial` input is carried forward as `partial`.
- File-only commands report their own codes (`STALE_REPORT`, `ECO_APPLY`,
  `TIMING_REPORT`, `OUTPUT_PATH`, ...), never `INVALID_WORKSPACE`.

Tests: `crates/rustroad-formats/src/eco.rs`
(`output_inout_or_unknown_sink_pins_are_refused`),
`crates/rustroad-workspace/tests/hold_eco.rs`
(`apply_eco_refuses_an_output_pin_sink_and_a_foreign_cell_lef`,
`stale_reports_are_refused_with_their_own_code`,
`later_reports_on_an_earlier_netlist_are_refused_by_the_sta_record`,
`unreadable_or_ambiguous_sta_records_are_refused`,
`cts_buffered_timed_netlist_refines_the_mapped_netlist`,
`antenna_eco_carries_a_partial_input_status_forward`,
`antenna_eco_sizes_diodes_against_the_safety_margin`) and
`crates/rustroad-workspace/tests/antenna.rs`
(`safety_margin_flags_ratios_just_below_the_lef_limit`). A regression run of
`scripts/hold_closure.py --signoff --openrcx --antenna-controls` passes and
reproduces every netlist, DEF and native SPEF of the run before the change
byte for byte.

Checks on the branch: `cargo test --release --workspace` 157 passed, 4
ignored (19 s, 702 MiB); `cargo clippy --all-targets -- -D warnings` and
`cargo fmt --check` clean; `python3 -m unittest discover -s scripts/tests`
68 tests (`scripts/tests/test_rtl2gds.py` covers the status algebra, budget
floors, manifest provenance, the frozen tool pin file, SDC rendering,
binding, determinism compare, powered-netlist views, bad-RTL wrapper,
testbench generator, OpenSTA parser, parity and the controls'
netlist/DEF/SPEF edits). The `hold_closure.py --signoff --openrcx
--antenna-controls` regression with the new binary passes (1:12.3, 289,748
KiB) and reproduces all 88 bound files of the previous round's run; only the
three hold plans differ (they now carry `inputs.binding.sta_record`).

## Stretch designs

**SERV** (`serv_rf_top`, track recipe, regenerated GF180 floorplan for the
clkbuf_8 clock tree) does not complete the gate on either PDK: synthesis,
the floorplan check, placement, CTS and routing finish (sky130 185 s, GF180
241 s, peak 470 MiB), then native extraction of the first layout stops on a
hard bound. Two bounds were raised with justification on the way (work
ceiling 64x the default, couplings 8M, RC nodes 4M, SPEF 256 MiB, RustSTA
import 2M nodes / 4M elements); the third, 4,096 candidate shapes per wire
run, is a hard per-run cap without a command-line option and was left as is
(run `.work/runs/keep-S3`, gate wall 432 s, exit 1, `implementation`
failed with `BUDGET_EXCEEDED: extraction candidates per wire run`). The
previously known SERV problems stand and were not attempted further: the
antenna ECO cannot repair clock-tree nets (29 sky130 / 45 GF180 deck
violations in the scale stream), sky130 repairs would use `diode_2` (LVS,
see above), and Netgen reports the unused inputs (`i_ibus_rdt[1:0]`,
`i_ext_rd[31:0]`, ...) as disconnected nodes and the aliased outputs as a
pin-list difference, which the frozen harness counts as failures (needs
output buffering for aliased ports in the flow, and an owner decision on
unused inputs). **rv_core** (optional) was not run.

## Resources (rule 9)

Memory estimate before the runs: the largest designs here have about 2,000
instances (regfile/mult with fillers); every tool runs alone inside one gate
(one process at a time), so a gate's peak is its largest process: a signoff
container (Magic/KLayout/Netgen, capped at 4 GiB, measured about 0.5 GiB),
then RustSTA/Verilator (< 0.3 GiB). Four gates ran in parallel (about 2 GiB
in all, 40 GiB available); each gate under `systemd-run` MemoryMax=8G, each
tool under a 2 GiB address-space limit and a 600 s (pipeline 120 s) timeout.

Measured (E6, both PDKs, controls and determinism included, eight gates in
parallel): 17 min 40 s from the first start to the last finish (mult
GF180: 806 s of its 1,059 s; regfile 1,001 s; the small designs 2-8 min);
the largest host-side process tree under `/usr/bin/time` around a gate is
370,944 KiB (mult), the largest step peak 532,892 KiB (a signoff container);
`ruststa whatif` is the closure loop's main cost (1.1 s per candidate on
regfile, 1.9 s on mult; bounded by candidate counts). Scratch after the
runs: about 22 GB for the eight E6 runs before trimming.

Measured (E3, both PDKs, controls and determinism included): see `runs[]` of
the evidence (wall time, peak RSS, process count per design) and `rows[].steps`
(wall time and peak RSS per step). Totals: the eight
designs took 2,334 s of gate wall time (four at a time: 10 min 54 s from the
first start to the last finish), 3,447 guarded processes; the longest gate is
regfile (654 s), the shortest pipeline (112 s). Peak RSS of any step is
533,000 KiB (a signoff container on mult); the largest host-side process
tree measured by `/usr/bin/time` around a whole gate is 356,296 KiB (mult;
containers run under podman and are counted in the step peaks).

## Open issues

- RustSTA vs OpenSTA effective-capacitance residual on long RC nets (1-5 ps
  per endpoint on the track designs): owner RustSTA.
- GF180 at ss with the 10 ns constraint set: alu and mult are
  setup-infeasible even with ideal wires (owner decision on the period);
  `drv` remains on uart_tx, fifo, regfile, alu and mult, partly from 10 cell
  families that cannot meet 1.0 ns at ss at any load (synthesis dont_use or
  owner decision on the limit), partly where a buffer would break setup.
- sky130 antenna repair without `diode_2` (router jumpers) or a deck/harness
  decision for `diode_2` (same class as `conb_1`): now fifo, alu, regfile.
- RustSTA refuses RC meshes and disconnected RC networks in SPEF: OpenRCX
  timing is `unverified` on alu sky130 (`a_i[0]`) and regfile GF180
  (`waddr[1]`) because of the track router's metal there.
- The closure loop's DRV repair is bounded by its `whatif` budget (counts,
  for determinism) and its 8 layouts; regfile GF180 ends with nets the
  budget did not reach.
- Density is reported only; co-simulation starts from Verilator's zero
  state; one-bit vector ports are treated as scalars.
