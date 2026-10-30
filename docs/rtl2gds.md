# RTL-to-GDS on SKY130 and GF180 (`scripts/rtl2gds.py`)

Current small-block qualification verdicts: [2026-10-25 closeout](closeout-2026-10-25.md).
Receipts of earlier rounds (`docs/evidence/rtl2gds-*.json`) are historical
evidence kept for reproducibility, not current benchmarks.

One command takes a design manifest from RTL to GDS on SKY130
(`sky130_fd_sc_hd`) and GF180 (`gf180mcu_fd_sc_mcu7t5v0`) and checks the
result with independent oracles: RustSys synthesis, RustROAD floorplan,
placement, CTS, routing and closure ECOs (DRV buffering, `whatif`-verified
sizing, path-aware hold, diode-free antenna jumpers), native and OpenRCX
extraction, three-corner timing in RustSTA and OpenSTA on both SPEF sources,
foundry DRC/LVS/antenna on the native GDS, equivalence and co-simulation
against the RTL, artifact binding, a full determinism rebuild and eleven
negative controls. Third-party tools run only as pinned black-box
executables. This page describes the flow of the joint RustROAD / RustSys /
RustSTA integration; the detailed engine pages are `docs/closure.md`,
`docs/hold-closure.md`, `docs/scaling.md`, `docs/signoff.md` and
`docs/extraction.md`.

## Recipe notes

- **Owner decisions applied before any physical step** (next section):
  per-PDK clock periods and the GF180 DRV dont_use.
- **RustSTA 0.4.3** solves resistor loops and joins disconnected RC
  components, so OpenRCX SPEF with routed loops is timed.
- **GF180 floorplan headroom** (`pdks.gf180.floorplan_recipe`, utilization
  0.35; SKY130 keeps 0.5 and its floorplans are unchanged) leaves row room
  for the GF180 closure ECOs.
- **Closure loop** (`docs/closure.md`): bulk DRV upsizing decided by one
  `whatif` call, violating drivers may also step down, one 0.74 utilization
  cap for buffers and resizes, and `whatif` count budgets of 128 per layout
  and 640 per loop, scaled as described there.

No SDC value beyond the period rule, no Liberty, deck, RC profile,
threshold or status rule is changed by the flow.

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
   so SKY130 delays are an upper bound of ideal);
4. critical-path delay `d = 10 ns - worst ss setup slack` (the period at
   which the worst path has zero slack with the same setup time, 50 ps
   uncertainty, 0.2 ns I/O delays and ideal clock);
5. `period = max(10, ceil(1.25 d))`; the per-PDK clock SDC is the reference
   with only `-period` and the `-waveform` fall edge replaced
   (`fixtures/designs/<d>/<pdk>-cts.sdc`), and the record
   (`fixtures/designs/<d>/clock-period.json`) keeps every input hash, the
   netlist sha256, the measured path and the arithmetic;
6. self-check: RustSTA re-run with the derived SDC must report worst slack
   `period - d` within 1 ps.

The manifest freezes the clock SDC and the record by sha256; the gate's
global `clock_period` status re-checks the arithmetic, that each clock SDC
carries the record's period and differs from the reference only in
`-period`/`-waveform`, and that the record was made with the pinned RustSys,
RustSTA and the manifest's cell policy. `--check` re-runs the timing and
requires identical files.

The frozen periods (ns):

| Design | SKY130 (`wireload_lumped`) | GF180 (`ideal`) |
|---|---|---|
| pipeline | 10 | 10 |
| dense | 10 | 10 |
| counter | 10 | 13 |
| uart_tx | 10 | 12 |
| fifo | 10 | 12 |
| alu | 10 | 18 |
| regfile | 10 | 12 |
| mult | 10 | 30 |
| serv (stretch) | 14 | 42 |
| rv_core (stretch, not run) | 29 | 93 |

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
policy (GF180: those 10 families removed).

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

`git archive <rev>` into a directory outside every worktree, `git init` and
commit there (the tree must equal `<rev>^{tree}`, so manifest provenance
holds), a release build with its own `CARGO_TARGET_DIR`, the RustSTA pair
copied again from the shared directory and checked against `SHA256SUMS` and
the pin file, then the selected designs re-run on both PDKs with controls and
determinism. `scripts/compare_runs.py` compares verdicts, global and per-PDK
statuses and every bound artifact byte for byte. The documented
normalizations are the RustROAD binary (embedded paths), the OpenRCX SPEFs
(`*DATE` line) and `routes.json` (records the routing-profile path of its
checkout). The comparison is stored in `independent_check` of the evidence.

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
  `physical_only` globs and has no LEF `OUTPUT` pin (taps and fillers) are
  removed and listed. A policy glob matching a cell with an output pin is
  refused, not filtered. RustSys keeps the supply connections and gets the
  supply ports; Yosys gets an unpowered view (supply-pin connections and the
  two supply ports removed, listed).
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
| `antenna_planted` | a long wire on an input pin without a diode, or on an existing gate-connected routed segment of that input net; bounded candidate and length sweeps | native check-antenna and the foundry deck |

`inverted_output` replaces the earlier RTL-wrapper mutant: the wrapper moved
every register under a hierarchy prefix, so name-based state matching in
RustSys and Yosys could not start (it was `unverified`, not rejected).
An input pin that leaves upward can have no gate on its I/O-layer partial
net. In that case the antenna control extends an existing routed segment
on a gate-connected layer toward the nearest die edge. Both the native
checker and the independent foundry deck must reject the resulting GDS.
The candidate sweep is bounded, with at most three foundry-deck attempts.


## Closure loop (summary)

Each iteration is a complete layout (import, place, CTS, route, antenna
jumpers, native extraction, RustSTA at tt/ss/ff on the native SPEF, native
antenna check); the decisions use native SPEF only, OpenRCX is the
reference that raises the hold target when it is short. Track recipe, at
most 8 layouts: (1) DRV/setup ECO - buffer insertion with a setup guard,
bulk and single `whatif`-verified resizes, timing-driven seeded placement;
(2) path-aware hold ECO; (3) antenna repair by layer jumpers (diode-free).
The loop keeps its best complete layout. Details and bounds:
`docs/closure.md` and `docs/hold-closure.md`.

## Stretch designs

SERV and rv_core have manifests and derived periods but are not claimed on
either PDK. The GF180 scale routing profile (`gf180-scale-routing.json`)
allows 8 M track-graph nodes; a larger graph fails with `BUDGET_EXCEEDED`.
SERV LVS needs an owner decision on unused input ports, and Yosys aborts on
SERV, so it has no second CEC oracle.

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
largest step: a signoff container (capped at 4 GiB), then
RustSTA/Verilator/whatif (< 0.4 GiB); eight gates in parallel are about
4 GiB of 42 GiB available. Each gate runs under `systemd-run`
MemoryMax=8G, each tool under a 2 GiB address-space limit and a 600 s
timeout. `ruststa whatif` work is bounded by candidate counts, not time.

## Open issues

- The SKY130 min-pulse-width parity and GF180 DRV items of earlier rounds
  are handled by the [2026-10-25 closeout](closeout-2026-10-25.md) (MPW
  declaration policy, driver/buffer co-sizing).
- SERV and rv_core are not claimed (see "Stretch designs").
- Antenna violations on the top routing layer cannot take a jumper.
- The period rule's SKY130 pre-layout delays use the Liberty wire load
  (RustSTA has no switch to force ideal wires).

## Earlier rounds

E3 (`docs/evidence/rtl2gds-final.json`), the antenna round
(`docs/evidence/rtl2gds-antenna.json`, `rtl2gds-antenna-serv.json`), E6
closure (`docs/evidence/rtl2gds-closure.json`, critical paths in
`docs/evidence/critical-path.json`), final2 (`docs/evidence/rtl2gds-final2.json`)
and the first gate round (`docs/evidence/rtl2gds.json`) used older RustSTA
pins. Their receipts are historical evidence kept for reproducibility, not
current benchmarks; their tables are in the git history of this file. The
hold-ECO and antenna-ECO hardening (report binding, sink-pin checks, safety
margins) is documented in `docs/hold-closure.md`.
