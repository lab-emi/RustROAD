# RTL-to-GDS on SKY130 and GF180 (`scripts/rtl2gds.py`)

Current closeout results: [2026-10-25 qualification](closeout-2026-10-25.md). The historical measurements below are preserved separately.

One command takes a design manifest from RTL to GDS on SKY130
(`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`) and checks the
result with independent oracles: RustSys synthesis, RustROAD floorplan,
placement, CTS, routing and closure ECOs (DRV buffering, `whatif`-verified
sizing, path-aware hold, diode-free antenna jumpers), native and OpenRCX
extraction, three-corner timing in RustSTA and OpenSTA on both SPEF sources,
foundry DRC/LVS/antenna on the native GDS, equivalence and co-simulation
against the RTL, artifact binding, a full determinism rebuild and eleven
negative controls. Third-party tools run only as pinned black-box
executables. This page is the final report of the joint RustROAD / RustSys /
RustSTA integration; the detailed engine pages are `docs/closure.md`,
`docs/hold-closure.md`, `docs/scaling.md`, `docs/signoff.md` and
`docs/extraction.md`.

## Historical integration result

Final runs `E-<design>` (worktree `.work/wt/final2`, RustROAD `aafcbaa`,
clean tree, release binary `36a47edb`; RustSTA 0.4.3 `26bd6eb`, ruststa
`472de8e8`, rusteda `c39dbde4`; RustSys policy producer `3de13b3a`; signoff
image `4c48415d8bd0`; both PDKs, controls and the determinism rebuild
included). Evidence: `docs/evidence/rtl2gds-final2.json`.

| | SKY130 | GF180 |
|---|---|---|
| every gating item passed | pipeline, dense, counter, uart_tx, alu | pipeline, dense, counter, uart_tx, fifo |
| only `timing_accuracy` failed (RustSTA vs OpenSTA min-pulse-width parity, 1.28-1.86 ps) | fifo, regfile, mult | - |
| only `drv` failed (3-12 nets above 1.0 ns at ss) | - | alu, regfile, mult |

10 of 16 design x PDK rows pass every gating status (the original headline incorrectly said 11). On all 16 rows setup
(tt, ss) and hold (tt, ss, ff) close on native and OpenRCX SPEF in both
engines, the native GDS has 0 XOR, 0 Magic and 0 KLayout DRC items, 0
antenna-deck violations, Netgen and KLayout LVS match, RTL equivalence
(pre-layout, final, powered netlist) and the 4096-cycle co-simulation pass,
all 11 controls are rejected by their oracles, the second full build is byte
identical, and every bound artifact re-verifies. An independent re-run of
counter and fifo from a `git archive` build in a separate directory gives
identical verdicts and identical artifacts (below).

### Per design x PDK (RustSTA, native SPEF, worst over all endpoints; ns)

| Design | PDK | Period ns (pre-layout d) | Gate | Failed items | Setup tt / ss | Hold tt / ss / ff | DRV ss (nets, worst slew) | Max parity native / OpenRCX ps | Cells logic / ECO buf / hold | Die um2 | Layouts (sel.) | Wall s | Peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| pipeline | sky130 | 10 (1.12) | passed | - | +9.087 / +8.491 | +0.081 / +0.064 / +0.115 | 0 | 0.06 / 0.42 | 10 / 0 / 4 | 2815 | 3 (2) | 57 | 464 |
| pipeline | gf180 | 10 (3.60) | passed | - | +7.461 / +4.844 | +0.242 / +0.362 / +0.194 | 0 | 0.03 / 0.03 | 10 / 0 / 4 | 4939 | 2 (1) | 71 | 434 |
| dense | sky130 | 10 (0.97) | passed | - | +9.172 / +8.664 | +0.141 / +0.185 / +0.125 | 0 | 0.03 / 0.03 | 18 / 0 / 0 | 3827 | 1 (0) | 58 | 464 |
| dense | gf180 | 10 (2.66) | passed | - | +8.192 / +6.633 | +0.150 / +0.192 / +0.130 | 0 | 0.02 / 0.03 | 22 / 1 / 1 | 9165 | 3 (2) | 78 | 439 |
| counter | sky130 | 10 (3.20) | passed | - | +8.124 / +6.407 | +0.080 / +0.081 / +0.098 | 0 | 0.61 / 0.59 | 43 / 0 / 13 | 3827 | 2 (1) | 81 | 491 |
| counter | gf180 | 13 (9.62) | passed | - | +7.241 / +1.260 | +0.127 / +0.142 / +0.123 | 0 | 0.07 / 0.08 | 47 / 14 / 15 | 14291 | 4 (3) | 99 | 442 |
| uart_tx | sky130 | 10 (2.59) | passed | - | +8.474 / +6.950 | +0.098 / +0.095 / +0.107 | 0 | 0.60 / 0.62 | 89 / 0 / 32 | 6644 | 2 (1) | 107 | 494 |
| uart_tx | gf180 | 12 (8.99) | passed | - | +7.254 / +2.433 | +0.261 / +0.457 / +0.171 | 0 | 0.16 / 0.20 | 106 / 36 / 17 | 20549 | 4 (3) | 131 | 443 |
| fifo | sky130 | 10 (2.86) | failed | timing_accuracy | +8.145 / +6.503 | +0.138 / +0.218 / +0.127 | 0 | 1.28 / 0.60 | 81 / 0 / 34 | 6644 | 2 (1) | 115 | 489 |
| fifo | gf180 | 12 (9.57) | passed | - | +6.747 / +1.238 | +0.160 / +0.290 / +0.110 | 0 | 0.26 / 0.34 | 115 / 36 / 16 | 27941 | 4 (3) | 137 | 449 |
| alu | sky130 | 10 (3.87) | passed | - | +7.546 / +5.728 | +0.631 / +0.884 / +0.524 | 0 | 0.84 / 0.60 | 253 / 1 / 0 | 10508 | 2 (1) | 195 | 517 |
| alu | gf180 | 18 (13.96) | failed | drv | +9.294 / +0.766 | +1.319 / +2.244 / +0.923 | 3 nets, 1.67 ns | 0.29 / 0.44 | 244 / 146 / 0 | 27941 | 8 (7) | 314 | 449 |
| regfile | sky130 | 10 (1.98) | failed | timing_accuracy | +8.304 / +6.895 | +0.100 / +0.110 / +0.114 | 0 | 1.86 / 0.80 | 372 / 19 / 53 | 14921 | 5 (4) | 371 | 496 |
| regfile | gf180 | 12 (8.90) | failed | drv | +6.767 / +1.656 | +0.262 / +0.404 / +0.201 | 8 nets, 2.03 ns | 0.60 / 0.76 | 341 / 173 / 65 | 57844 | 8 (4) | 511 | 473 |
| mult | sky130 | 10 (5.84) | failed | timing_accuracy | +6.866 / +4.221 | +0.101 / +0.113 / +0.130 | 0 | 1.28 / 0.58 | 418 / 0 / 3 | 14921 | 2 (1) | 248 | 520 |
| mult | gf180 | 30 (23.41) | failed | drv | +16.885 / +3.566 | +0.178 / +0.253 / +0.146 | 12 nets, 1.37 ns | 0.22 / 0.21 | 367 / 137 / 2 | 46121 | 8 (7) | 568 | 462 |

Period: the owner rule's period and, in parentheses, the pre-layout ss
critical-path delay it came from. Setup/hold: RustSTA on the native SPEF of
the selected layout (OpenSTA and the OpenRCX SPEF agree within the parity
column; both are gated). DRV: nets with a violating pin at ss on the
selected layout (both SPEFs, both engines). Parity: largest RustSTA-OpenSTA
difference over endpoints, min-pulse-width and DRV rows at any corner. Cells:
logic instances of the final netlist, ECO buffers, hold delay cells. Layouts:
closure layouts built and the selected one. Wall time and peak RSS per PDK
row (the peak is a signoff container).

### Failing items and root causes

1. **SKY130 `timing_accuracy` (fifo, regfile, mult): RustSTA vs OpenSTA
   min-pulse-width required time at ss on the native SPEF.** Endpoint setup
   and hold parity is <= 0.89 ps on every row and both SPEFs; the pulse-width check of the
   flops' CLK pins differs by up to 1.279 ps (fifo `r6/CLK` high: required
   0.69166 vs 0.69040 ns), 1.864 ps (regfile `r29/CLK`) and 1.284 ps (mult
   `r7/CLK` low). The Liberty `min_pulse_width` of `sky130_fd_sc_hd` flops is
   a table of the clock-pin transition, so a sub-picosecond clock-slew
   difference on the routed CTS nets shows up here (OpenSTA: `r6/CLK` 0.23 /
   0.18 ns rise/fall). Same residual class as the effective-capacitance one
   the RustSTA stream declared; 0.4.3 did not change it. Owner: RustSTA. The
   1 ps gate and the harness were not changed. The same three designs pass
   parity on GF180 and on OpenRCX SPEF.
2. **GF180 `drv` (alu 3 nets, regfile 8, mult 12; transitions 1.01-2.03 ns
   at ss against `set_max_transition 1.0`).** Setup and hold are closed on
   these rows. Remaining nets on the selected layouts: alu `oai21_2` into one
   ECO buffer (1.67 ns); regfile a `nor2_4` (largest member) into four ECO
   buffers (2.03 ns) and seven `dlyb_1` hold cells into `buf_8` ECO buffers
   (1.08-1.65 ns); mult `aoi22_2`/`aoi22_4` into one ECO buffer (1.01-1.37 ns;
   aoi22's best member reaches only 0.80 ns at the smallest sink with a 0.2
   ns input, more with the real input slew). The closure loop reached its
   8-layout bound on all three while still improving (alu 522 -> 6 violating
   pins). Owner: closure stream (driver/buffer co-sizing; the loop resizes
   one family step per layout and never resizes delay cells). An extra
   buffer level in front of ECO buffers was tried and made alu and mult
   worse; it is not in the final code.

No SDC value beyond the period rule, no Liberty, deck, RC profile,
threshold or status rule was changed for any of these.

### What changed in this round (and why)

- **Owner decisions applied before any physical step** (next section):
  per-PDK clock periods (GF180 counter 13, uart_tx 12, fifo 12, alu 18,
  regfile 12, mult 30 ns; every SKY130 design and GF180 pipeline/dense
  10 ns) and the GF180 DRV dont_use. Before them (E6, 10 ns everywhere,
  RustSTA 06b1c9f) only pipeline and dense passed on both PDKs and GF180
  counter alone among the track designs; alu and mult were setup-infeasible
  at 10 ns with ideal wires.
- **RustSTA 0.4.3** solves resistor loops and joins disconnected RC
  components, so OpenRCX SPEF with routed loops is timed (two rows were
  `unverified` in E6); 0.4.2's one-step effective capacitance brought
  endpoint parity under 1 ps on every row.
- **GF180 floorplan headroom** (`pdks.gf180.floorplan_recipe`, utilization
  0.35; SKY130 keeps 0.5 and its floorplans are unchanged): at 0.5 the GF180
  closure ECOs filled the rows (regfile: a 0.76 netlist failed CTS packing,
  `baseline packing ran out of intervals`; alu: resizes refused at the 0.70
  cap with 116-128 violating pins left). From 0.35 the alu, regfile and mult
  loops end at 0.49-0.63.
- **Closure loop** (`docs/closure.md`): bulk DRV upsizing decided by one
  `whatif` call, violating drivers may also step down, one 0.74 utilization
  cap for buffers and resizes, larger `whatif` count budgets (128 per layout,
  640 per loop, scaled as before). Dev runs: mult GF180 DRV 180 -> 18
  violating pins at 0.5 utilization from the bulk step alone.

## Owner decisions (2026-10-24) and their derivations

### Clock period per design x PDK

`period = max(10 ns, round_up(1.25 x pre-layout ss+max critical-path delay, 1 ns))`,
applied uniformly to every design and PDK before any physical step.
`scripts/derive_clock_period.py --work DIR --write` (tracked) computes it:

1. the mapped netlist exactly as the gate builds it (pinned RustSys with the
   cell policy, after the GF180 dont_use below; `deconflict`,
   `buffer_port_aliases`), or the literal netlist (dense);
2. the signoff SDC rendered from the RTL ports on the 10 ns reference clock
   SDC (`sdc.reference_clock`), `set_propagated_clock` removed (ideal clock);
3. pinned RustSTA, ss Liberty only, no SPEF, `--require-wire-model`: GF180
   `ideal` (its Liberty has no wire-load table), SKY130 `wireload_lumped`
   (its Liberty `default_wire_load`; RustSTA cannot force ideal wires there,
   so SKY130 delays are an upper bound of ideal; no SKY130 period is near
   the 10 ns floor's 8 ns threshold, the largest is mult at 5.84 ns);
4. critical-path delay `d = 10 ns - worst ss setup slack` (the period at
   which the worst path has zero slack with the same setup time, 50 ps
   uncertainty, 0.2 ns I/O delays and ideal clock);
5. `period = max(10, ceil(1.25 d))`; the per-PDK clock SDC is the reference
   with only `-period` and the `-waveform` fall edge replaced
   (`fixtures/designs/<d>/<pdk>-cts.sdc`), and the record
   (`fixtures/designs/<d>/clock-period.json`) keeps every input hash, the
   netlist sha256, the measured path and the arithmetic;
6. self-check: RustSTA re-run with the derived SDC reports worst slack
   `period - d` within 1 ps (passed on every row).

The manifest freezes the clock SDC and the record by sha256; the gate's
global `clock_period` status re-checks the arithmetic, that each clock SDC
carries the record's period and differs from the reference only in
`-period`/`-waveform`, and that the record was made with the pinned RustSys,
RustSTA and the manifest's cell policy. `--check` re-runs the timing and
requires identical files. RustSTA 0.4.2 and 0.4.3 give identical delays (to
1e-6 ns).

| Design | PDK | Wire model | Worst pre-layout path | Slack at 10 ns | d | 1.25 d | Period |
|---|---|---|---|---|---|---|---|
| pipeline | sky130 | wireload_lumped | r0/CLK -> r2/D | +8.877 | 1.123 | 1.404 | 10 |
| pipeline | gf180 | ideal | r0/CLK -> r2/D | +6.396 | 3.604 | 4.504 | 10 |
| dense | sky130 | wireload_lumped | r5/CLK -> y[3] | +9.035 | 0.965 | 1.207 | 10 |
| dense | gf180 | ideal | b[0] -> r2/D | +7.338 | 2.662 | 3.327 | 10 |
| counter | sky130 | wireload_lumped | r1/CLK -> r4/D | +6.804 | 3.196 | 3.995 | 10 |
| counter | gf180 | ideal | r0/CLK -> r7/D | +0.376 | 9.624 | 12.030 | **13** |
| uart_tx | sky130 | wireload_lumped | r1/CLK -> r5/D | +7.406 | 2.594 | 3.243 | 10 |
| uart_tx | gf180 | ideal | r1/CLK -> r6/D | +1.009 | 8.991 | 11.239 | **12** |
| fifo | sky130 | wireload_lumped | r4/CLK -> r4/D | +7.144 | 2.856 | 3.570 | 10 |
| fifo | gf180 | ideal | r6/CLK -> r6/D | +0.430 | 9.570 | 11.963 | **12** |
| alu | sky130 | wireload_lumped | a_i[3] -> zero_o | +6.125 | 3.875 | 4.844 | 10 |
| alu | gf180 | ideal | b_i[1] -> zero_o | -3.958 | 13.958 | 17.448 | **18** |
| regfile | sky130 | wireload_lumped | r47/CLK -> rdata_a[7] | +8.020 | 1.980 | 2.475 | 10 |
| regfile | gf180 | ideal | raddr_b[2] -> rdata_b[7] | +1.098 | 8.902 | 11.127 | **12** |
| mult | sky130 | wireload_lumped | b[4] -> r11/D | +4.161 | 5.839 | 7.299 | 10 |
| mult | gf180 | ideal | b[0] -> r10/D | -13.407 | 23.407 | 29.259 | **30** |
| serv (stretch) | sky130 | wireload_lumped | r9/CLK -> r1171/D | -0.850 | 10.850 | 13.562 | **14** |
| serv (stretch) | gf180 | ideal | r1192/CLK -> r1171/D | -23.549 | 33.549 | 41.936 | **42** |
| rv_core (stretch, not run) | sky130 / gf180 | | | | 22.505 / 73.939 | | 29 / 93 |

As expected, most rows stay at 10 ns; the GF180 track designs do not (their
ss critical paths are 9-23 ns after the dont_use). The dont_use made the
GF180 netlists slower than before (alu 13.07 -> 13.96 ns, mult 22.05 ->
23.41, regfile 6.96 -> 8.90).

### GF180 DRV dont_use

Keep `set_max_transition 1.0`; GF180 synthesis must not use cell families
that cannot reach 1.0 ns output transition at ss_125C_3v00 even at minimum
load with a 0.2 ns input. `scripts/derive_drv_dont_use.py` (tracked) derives
it from the Liberty pinned by `fixtures/flow/signoff-pinned.json`
(`gf180mcu_fd_sc_mcu7t5v0__ss_125C_3v00.lib`, sha256 `5d2f37ba...`) into
`fixtures/flow/gf180-drv-dont-use.json`:

- minimum load: the smallest input-pin capacitance of a cell synthesis may
  instantiate (one sink, no wire): 2.283 fF (`dffrsnq_1/D`); raised to a
  table's smallest index where that is larger (tri-state cells);
- a member's transition: the worst over every output pin, timing arc and
  rise/fall table, bilinear, no extrapolation (`three_state_disable`
  skipped); a family is DRV-limited when even its best member exceeds 1.0 ns.

| Family | Best member | Transition at 2.283 fF (ns) | At the table's 1 fF index (ns) |
|---|---|---|---|
| aoi211 | aoi211_4 | 1.056 | 1.013 |
| aoi221 | aoi221_4 | 1.474 | 1.304 |
| aoi222 | aoi222_4 | 1.665 | 1.622 |
| nor4 | nor4_2 | 1.180 | 1.069 |
| oai22 | oai22_4 | 1.142 | 1.108 |
| oai221 | oai221_4 | 1.487 | 1.454 |
| oai222 | oai222_4 | 1.912 | 1.881 |
| oai31 | oai31_4 | 1.012 | 0.964 |
| oai32 | oai32_4 | 1.860 | 1.767 |
| oai33 | oai33_4 | 2.316 | 2.266 |

The derived list equals the owner's. `oai31` is marginal: at the table's 1
fF characterization point (below any real sink) its `_4` member reaches
0.964 ns; at the smallest real sink it does not. The next families are
aoi22 (0.80 ns) and nand4 (0.72 ns). `scripts/cell_policy.py` recomputes the
derivation (a stale file is refused) and adds the globs
(`gf180mcu_fd_sc_mcu7t5v0__<family>_*`) to the GF180 `synth_dont_use` and
`rustsys_args`; `scripts/check_cell_policy.py` holds the owner's list
independently, fails any such instance in a synthesized netlist and reports
a policy whose derived list or Liberty hash differs as inconsistent. The
sizing profiles (`fixtures/pdk/<pdk>-sizing.json`) were rebuilt from the new
policy (GF180: those 10 families removed). Before the rule RustSys mapped 10
such instances into uart_tx, 2 into fifo, 14 into alu, 1 into regfile and 8
into mult (E6 netlists); now none.

## How to run

```sh
export PATH=/home/cgao/.cargo/bin:$PATH
# Release binary (Rule 9: about 0.9 GiB, 4 jobs)
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  --setenv=CARGO_BUILD_JOBS=4 cargo build --release --locked --bins
# Private copy of the pinned RustSTA pair (the shared directory is replaced by other streams)
mkdir -p .work/tools/sta-gate
cp /mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin/{rusteda,ruststa,SHA256SUMS} .work/tools/sta-gate/
(cd .work/tools/sta-gate && sha256sum -c SHA256SUMS)   # and equal to fixtures/flow/rtl2gds-tools.json
# Owner-rule inputs (only after a Liberty, cell-policy, RTL or RustSTA-pin change; in this order)
python3 scripts/derive_drv_dont_use.py && python3 scripts/cell_policy.py generate \
  --producer-receipt .work/tools/rustsys-c5ec23c.build.json && python3 scripts/build_sizing_profile.py
python3 scripts/derive_clock_period.py --work .work/period-new --write     # --check to verify
python3 scripts/scale_manifests.py --rustroad target/release/rustroad \
  --rustsys /mnt/data2/cgao/git/RustROAD/.work/tools/rustsys-target/release/rustsys --design <name> ...
python3 scripts/scale_manifests.py --refresh-manifest --design pipeline
git commit ...                                 # manifests must be at HEAD (manifest_provenance)
# One design, both PDKs, controls and determinism rebuild included
systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=2G --setenv=PATH=$PATH \
  /usr/bin/time -v -o .work/runs/E-<name>.time timeout 7200s \
  python3 scripts/rtl2gds.py --design fixtures/designs/<name>.json --pdk sky130,gf180 \
  --run .work/runs/E-<name> --base-rev empty
# Evidence and the independent re-run comparison
python3 scripts/compare_runs.py --out .work/independent.json REF_RUN=OTHER_RUN ...
python3 scripts/rtl2gds_evidence.py --out docs/evidence/rtl2gds-final2.json \
  --extra .work/independent.json .work/runs/E-*
```

Never edit the worktree while a run is in flight: the artifact binding
records the git status including untracked files. Exit 0 = every gating
status passed (or an allowed `not_applicable`), 1 = something failed, 3 =
nothing failed but something is unverified. Options: `--rustroad BIN`,
`--tools FILE` (must hash to the manifest's frozen pin file), `--tool
NAME=PATH`, `--no-controls`, `--no-determinism` (both then `unverified`),
`--preflight-only`. `--run` must not exist, `--base-rev` must be `empty`
(`REVISION_CONFLICT` otherwise).

### Independent re-run

`git archive aafcbaa` into `.work/indep-final2/src` (outside every
worktree), `git init` + commit there (tree `28ce4f8d`, identical to
`aafcbaa^{tree}`, so manifest provenance holds), its own `CARGO_TARGET_DIR`
release build (binary `28e80364`, differs from `36a47edb` only by embedded
paths), the RustSTA pair copied again from the shared directory and checked
against `SHA256SUMS` and the pin file; counter and fifo re-run on both PDKs
with controls and determinism. `scripts/compare_runs.py`: identical verdicts
(counter passed, fifo failed on `sky130:timing_accuracy` only), identical
global and per-PDK statuses, and of 55 (SKY130) / 57 (GF180) bound artifacts
50 / 52 byte-identical; the rest are identical after the documented
normalizations: the RustROAD binary, the three OpenRCX SPEFs (`*DATE` line)
and `routes.json` (records the routing-profile path of its checkout).
Result in `independent_check` of the evidence.

## Design set

| Design | Source | Recipe | Notes |
|---|---|---|---|
| pipeline | `fixtures/flow/pipeline.v` (4 registers) | legacy (tracked floorplan, fixed-order placer, lattice router) | |
| dense | literal mapped netlists per PDK (6 registers) | track | |
| counter | 8-bit counter, enable, clear, async reset | track | |
| uart_tx | UART transmitter FSM | track | |
| fifo | 4x8 synchronous FIFO | track | |
| alu | 8-bit combinational ALU (virtual clock) | track | Yosys cannot read its package import: exhaustive co-simulation is the second CEC oracle |
| regfile | 8x8 register file, two read ports | track | 64-sink ports |
| mult | 6x6 registered and combinational multipliers | track | xor3 chains |
| serv (stretch) | SERV `serv_rf_top` | track | below |
| rv_core (stretch) | RV32I subset | track | not run |

## Design manifest (`rustroad.design.v3`)

One schema for every design (`fixtures/designs/<name>.json`, checked by
`rtl2gds_design.manifest_problems`); every tracked file carries its sha256.
v3 (this round) moves the clock SDC per PDK:

| Field | Meaning |
|---|---|
| `schema`, `name`, `description`, `top`, `rtl[]` | identity and sources |
| `clock` | `{name, port}`; `port: null` = combinational design with a virtual clock (no CTS) |
| `sdc` | `reference_clock` (the 10 ns clock SDC the period derivation starts from), `io_template`, `period_derivation` (`clock-period.json`) |
| `recipe` | `physical` (`legacy` or `track`), `floorplan` (`utilization`, `aspect`, `margin_um`; null for legacy) |
| `pdks.<tag>` | `sdc_clock` (per-PDK clock SDC with the derived period; CTS reads it), `floorplan`, `physical_config`, `routing_config`, `delay_cells`, `gds_layer_map`, `rc_profiles`, `corners` (tt+nom / ss+max / ff+min), `frozen` (sha256 of every derived input, incl. `sdc_cts` and the rendered `sdc_postroute`); optional `physical_profile`, `netlist` (literal), `sizing_profile`, `floorplan_recipe` (per-PDK override; GF180: utilization 0.35) |
| `pdk_profiles`, `cell_policy`, `signoff_manifest`, `tools` | profiles, cell policy, signoff image pins, frozen tool pin file |
| `verification`, `budgets` | co-simulation and RustSys/Yosys budgets; owner floors (`hold_margin_ns` 0.05, `setup_margin_ns` 0, `parity_ns` 0.001, fixed in code), closure budgets (8 layouts on the track recipe, ECO hold target 0.07 ns), process/container limits |

SDC of every design (fixed before closure; only the period follows the
owner rule): 0.1 ns clock transition, 50 ps uncertainty,
`set_max_capacitance 0.3`, `set_max_transition 1.0`, `set_max_fanout 64`
(pipeline: 2), I/O delays 0.2 ns, input transition 0.1 ns, output load
0.01 pF.

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
| `cell_policy` | mapped, final and powered netlists | `check_cell_policy.py` against the PDK exclusion lists, the owner-reserved CTS/hold cells and (GF180) the owner's DRV-limited families |
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
| `controls` | eleven planted defects | each rejected by its own oracle; a hold-removal experiment that is still timing-clean is recorded and replaced by an explicit hold violation |

Reported only: `density` (`density_status`; owner decision: not part of the
block DRC verdict). Global statuses: `tools_identity` (pinned binaries and the
frozen pin file), `openroad_image`, `design_manifest` (every tracked input by
sha256), `budget_floors`, `manifest_provenance`, `sdc_rendering` (one signoff SDC per PDK, each
equal to its frozen sha256) and `clock_period` (the owner period rule, below). `timing_accuracy_status` is also given at
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

## Logic oracles

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

## Negative controls (both PDKs, every design)

| Control | Planted defect (copy, never a bound artifact) | Oracle that must reject it |
|---|---|---|
| `drc_spacing` | two metal1 rectangles below minimum spacing | Magic and KLayout DRC |
| `label_swap` | two input-port labels exchanged | Netgen, KLayout LVS (own results) and the XOR label comparison |
| `removed_via` | one routing via of a two-terminal signal net deleted | Netgen and KLayout LVS |
| `removed_hold_cell` | the hold chain of the worst iteration-0 endpoint that a chain feeds, fresh layout | RustSTA and OpenSTA hold gate |
| `planted_hold_uncertainty` | when no hold cell exists, or both engines prove removal still meets hold: `set_clock_uncertainty -hold 2.0` in a copy of the SDC | RustSTA and OpenSTA hold gate |
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


## Closure loop (summary)

Each iteration is a complete layout (import, place, CTS, route, antenna
jumpers, native extraction, RustSTA at tt/ss/ff on the native SPEF, native
antenna check); the decisions use native SPEF only, OpenRCX is the
reference that raises the hold target when it is short. Track recipe, at
most 8 layouts: (1) DRV/setup ECO - buffer insertion with a setup guard,
bulk and single `whatif`-verified resizes, timing-driven seeded placement;
(2) path-aware hold ECO; (3) antenna repair by layer jumpers (diode-free).
The loop keeps its best complete layout. Details, bounds and measurements:
`docs/closure.md` and `docs/hold-closure.md`.

## Stretch designs

**SERV GF180** at the derived 42 ns did not get through implementation in
this round: on the regenerated 0.35-utilization GF180 floorplan the track
router stops at once with `BUDGET_EXCEEDED: routing grid node count`
(`gf180-scale-routing.json` allows 8 M track-graph nodes). No profile was
changed for it. The earlier round (0.5 floorplan, 10 ns,
`docs/evidence/rtl2gds-antenna-serv.json`) completed the GF180 gate with
LVS failing only on its 35 unused input ports (Netgen disconnected nodes in
both netlists, KLayout unnamed pins; owner decision on unused ports needed),
`cec_*` unverified because Yosys aborts on SERV, and timing not closed.
**SERV SKY130** did not route with buffered ports (router budget; scale
stream). **rv_core** was not run (its periods are derived: 29 / 93 ns).

## Scope: what is and is not claimed

Claimed, for the rows marked passed: block-level RTL-to-GDS on the two
open PDKs at the stated corners (tt+nom, ss+max, ff+min) under the stated
SDC, with every gating status above passing, independently of RustROAD's
own models where the table says so.

Not claimed:

- **Block level only**: no pad ring, no seal ring, no chip assembly.
- **Unchecked**: IR drop, electromigration, signal integrity (couplings are
  grounded at factor 1), OCV/AOCV/POCV, CPPR, the minimum pulse width of
  asynchronous set/clear inputs (a requirement on the reset source; pins
  listed, recovery/removal timed).
- **Density** is reported separately (`density_status`, owner decision):
  GF180 fails its coverage rules without fill, the SKY130 windowed deck
  cannot evaluate blocks smaller than its window.
- Timing is only as good as the Liberty corners and the two RC sources
  (native extraction and OpenRCX on the exported DEF); no silicon
  correlation, no SPICE-level signoff of the blocks.
- The failing rows are failures, not passes with exceptions; SERV and
  rv_core are not claimed on either PDK.
- Co-simulation starts from Verilator's zero state; one-bit vector ports
  are treated as scalars.

## Resources (Rule 9)

Estimate before the runs: about 2,000 instances at most (regfile/mult with
fillers); one tool process at a time inside a gate, so a gate's peak is its
largest step: a signoff container (capped at 4 GiB, measured about 0.5 GiB),
then RustSTA/Verilator/whatif (< 0.4 GiB); eight gates in parallel are about
4 GiB of 42 GiB available. Each gate ran under `systemd-run` MemoryMax=8G,
each tool under a 2 GiB address-space limit and a 600 s timeout.

Measured (E runs, eight gates in parallel, both PDKs, controls and the
determinism rebuild included): 14 min 43 s from the first start to the last
finish (regfile 883 s of gate wall time, mult 817 s, alu 510 s, the others
129-254 s); largest step peak 532,872 KiB (a signoff container, mult); the
largest host-side process tree under `/usr/bin/time` 366,476 KiB (regfile).
`ruststa whatif` dominates the GF180 closure loops (counts, not time, bound
it). Derivations: `derive_clock_period.py` 10 s / 120 MiB for all 11
designs; `derive_drv_dont_use.py` 1 s. Independent re-run: counter 3:00,
fifo 4:11; release build 22 s, 850 MiB.

## Open issues

- RustSTA vs OpenSTA min-pulse-width parity on SKY130 CTS clock pins at ss
  (1.1-1.9 ps): owner RustSTA.
- GF180 DRV on alu, regfile, mult (3-12 nets): driver/buffer co-sizing, a
  smarter treatment of hold delay cells that feed ECO buffers, and the
  8-layout bound (closure stream).
- SERV GF180 exceeds the track router's 8 M grid-node budget on the 0.35
  floorplan; SERV LVS needs an owner decision on unused input ports; Yosys
  aborts on SERV (no second CEC oracle).
- Antenna violations on the top routing layer cannot take a jumper (none on
  the required designs).
- The period rule's SKY130 pre-layout delays use the Liberty wire load
  (RustSTA has no switch to force ideal wires); stated per row.

## Earlier rounds

E3 (`docs/evidence/rtl2gds-final.json`), the antenna round
(`docs/evidence/rtl2gds-antenna.json`, `rtl2gds-antenna-serv.json`), E6
closure (`docs/evidence/rtl2gds-closure.json`, critical paths in
`docs/evidence/critical-path.json`) and the first gate round
(`docs/evidence/rtl2gds.json`) ran at 10 ns everywhere with older RustSTA
pins; their tables are in the git history of this file. The hold-ECO and
antenna-ECO hardening (report binding, sink-pin checks, safety margins) is
documented in `docs/hold-closure.md`.
