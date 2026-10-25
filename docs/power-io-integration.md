# Physical I/O, supply and tie-cell integration

RustROAD 0.3.0 now prepares physical I/O, inserts configured tie cells, fills
rows, alternates N/FS orientations and constructs the modeled primary supply
grid. The dual-PDK acceptance below passed. This resource estimate was written
before fixture generation and the external geometry checker.

The dual-PDK smoke will use at most 2,000 logical/tie/filler instances, 16,000
pin connections and 16,000 rectangular pin, obstruction, rail and I/O shapes.
Budget 512 bytes per instance, 256 bytes per connection and 128 bytes per
rectangle, plus sorted interval/union-find indexes: below 32 MiB checker state.
Use one corner and two worker threads; the pre-existing conservative Liberty
parser allowance is 640 MiB and runtime headroom is 256 MiB. No combinatorial
path enumeration or pairwise rectangle matrix is allowed. Geometry checks use
per-layer sorted interval sweeps and bounded connectivity unions; the work
budget is explicit, with negative controls on small fixtures.

One process runs at a time. Native tools have 2 GiB address-space, 30-second wall
and 64 MiB file limits. OpenROAD runs in the pinned image with 2 GiB cgroup
physical memory/no swap, 8 GiB virtual address allowance, two CPUs, bounded
files, and a 25-second inner timeout. Set GOMAXPROCS=2 for the container
controller. Check `free -h` before execution; capture outer and inner
`/usr/bin/time -v` RSS and wall receipts. Full-suite wall budget is 180 seconds.

Signal routing, CTS, parasitic extraction, DRC/LVS and GDS signoff remain outside this
phase. Logical PG attachment and geometric continuity of the explicitly modeled
rails do not establish electromigration, IR drop, via enclosure or foundry DRC.

## Verified pipeline

The final frozen RustROAD executable is version **0.3.0**, SHA-256
`fd5f827406a2e90bb6822dfe0c7f2ebdf2d335f4c490fcfe6ad08ed8d53f5654`.
[Results and receipt index](evidence/power-io-results.json),
[source/configuration pins](evidence/power-io-sources.json), and compact actual
layouts for [Sky130](evidence/power-io-layout-sky130.json) and
[GF180](evidence/power-io-layout-gf180.json) are tracked. No full PDK asset is
copied into these layout files.

| Check | Sky130 HD | GF180 MCU 7T5V0 |
|---|---:|---:|
| Physical cells | 97 | 44 |
| Inserted tie cells | 2 | 2 |
| Added filler cells | 77 | 20 |
| Mirrored FS cells, reported as MX by OpenDB | 46 | 19 |
| Connected PG pins, including body-bias pins | 388 | 176 |
| Placed signal / PG boundary terminals | 20 / 2 | 20 / 2 |
| Filled rows with intrinsic primary-rail continuity | 8 | 8 |
| Actual special wires / fixed vias | 11 / 9 | 11 / 9 |
| Connected geometric components per VDD/VSS | 1 / 1 | 1 / 1 |
| Conductor rectangles checked | 223 | 301 |
| Bounded intersection comparisons | 5,505 | 7,236 |
| Native and Yosys post-insertion CEC | equivalent / proved | equivalent / proved |
| Verilator input combinations | 512 passed | 512 passed |
| OpenROAD peak RSS | 88,640 KiB | 85,588 KiB |

The full serial run took **6.90 seconds** with **290,696 KiB** maximum host-process
RSS. This includes synthesis, import, physical preparation, exports, native and
external CEC, Verilator build/execution, OpenDB inspection, geometry checks and
STA consistency/differential checks. These are small integration measurements,
not placement/routing performance claims. Raw evidence is retained under
`.work/power-io-verified/`; the outer measurement is
`.work/power-io-verified.time`.

The test extends the prior four-bit registered datapath with two constant-D
flip-flops. Frozen RustSys already emits functional tie cells. To exercise the
new importer's literal-to-tie path honestly, the harness retains that native
netlist and creates an explicitly recorded, verification-only representation
using literal assignments in place of the two tie outputs. Yosys proves this
representation against the RTL before import. The importer then materializes
the configured tie cells, and both native CEC and Yosys prove the final logical
export. Six state bits are retained; simulation checks their behavior after the
first clock across all 512 data/enable combinations.

Tie polarity is independently established from the original Liberty models:
Sky130 `sky130_fd_sc_hd__conb_1` has HI=1 and LO=0; GF180 uses
`gf180mcu_fd_sc_mcu7t5v0__tieh.Z`=1 and `...__tiel.ZN`=0. Actual Yosys SAT proofs
pass, while the opposite constant claims are rejected. The values are not
inferred from cell names. The physical export contains the configured tie
outputs and no remaining literal cell-input connections.

## Independent physical evidence

OpenROAD is used only as a black-box parser/database oracle. Its public OpenDB
API reads real PDK LEFs and the produced DEF, including physical PINS and
SPECIALNETS. The checker independently verifies:

- Exact cell/master coverage and powered-Verilog signal/PG endpoint partitions,
  using bare Yosys parsing against OpenDB connectivity. This includes fillers,
  tie cells, body-bias pins and the two supply boundary ports.
- All I/O shapes are fixed/placed, inside the die and touch its boundary. Their
  direction/use and per-layer non-overlap are checked. OpenDB calls DEF `FIXED`
  pin placement `FIRM`; this alias was verified against the emitted DEF.
- Every PG pin is attached to its explicit configured role. Primary pin shapes
  are transformed independently using each observed N/FS placement; their
  bounding unions equal OpenDB's actual ITerm bounding boxes.
- Intrinsic cell-rail intervals cover every occupied, filled row. A separate
  interval union checks opposite top/bottom polarity and matching polarity at
  adjacent row boundaries, without relying on the added horizontal wires to
  hide a cell/filler gap.
- Actual SWire rectangles and actual fixed-via cut/metal enclosures form one
  connected conductor component per primary supply, with no geometric VDD/VSS
  shorts. The bounded sweep and union-find are independent of the production
  supply builder. Body-bias well continuity is not claimed.

Five negative controls modify copies of the actual OpenDB records: wrong PG
polarity, removal of a filler creating an intrinsic row gap, removal of the
power spine/vias, a superimposed opposite-polarity conductor, and an I/O shape
moved off the boundary. All five are rejected on both PDKs. These validate the
independent checker; they do not claim that the production importer rejects
arbitrary externally edited DEFs.

Two public-format conventions were verified directly. Component FS at the same
lower-left placement coordinates transforms local y into `H-y`; OpenDB reports
MX and matching bounds. DEF special-wire endpoints are flush by default, so
square endcaps require explicit extension `width/2` at each endpoint. Vias are
emitted on separate singleton paths, avoiding a via-induced layer switch on a
following segment. The preflight examples and their observed bounds remain in
`.work/power-fs-probe/` and `.work/power-cap-probe/`.

## Preserved timing chain

The final logical export also passes frozen RustSTA full, independent and no-op
incremental/full consistency checks. The same explicit ideal-wire library is
used for the OpenSTA comparison, with 0.001 ns tolerance. Maximum setup/hold
slack differences are 0.000000602940417593345 ns for Sky130 and
0.0000008695038307138248 ns for GF180.

Each PDK has **14 constrained endpoints and 2 no-data-arrival endpoints**. The
unconstrained records are exactly `r0/D` and `r1/D`, whose inputs are driven by
the independently proven constant tie outputs; the harness verifies that
relationship from actual connectivity. It does not replace this count with
zero. Native minimum-pulse-width warnings and GF180 minimum-period warnings
remain in the evidence. This is ideal-wire setup/hold calibration, not extracted
post-route timing or complete timing-rule coverage.

## Reproduce

The profiles in `fixtures/pdk/sky130-power-io.json` and
`fixtures/pdk/gf180-power-io.json` name the primary/bias roles, real tie/filler
cells, rail/strap/I/O layers and fixed vias. Integer dimensions are DBU. The
floorplan reserves a left channel of 20 sites for the two supply spines.
`fixtures/pdk/local-power-io.json` ties these profiles to the hashed local PDK
assets already used by the previous phase.

```sh
free -h
ulimit -v 8388608
/usr/bin/time -v -o .work/power-repeat.time timeout 180s \
python3 scripts/verify_power_io.py --workspace .work/power-repeat --base-rev empty \
  --profiles fixtures/pdk/local-power-io.json \
  --rustroad /absolute/path/to/rustroad --rustsys /absolute/path/to/rustsys \
  --yosys /absolute/path/to/yosys --verilator /absolute/path/to/verilator \
  --rusteda /absolute/path/to/RustSTA/rusteda --ruststa /absolute/path/to/ruststa \
  --opensta /absolute/path/to/sta
```

The evidence directory must be new. Core workflow: `import-pdk` accepts
`--physical-config`, then `place` constructs fillers, orientations, I/O and
supply geometry. `export-verilog` emits the logical design, while
`export-verilog --include-power` emits the connected physical cell set and
supply ports. No sibling engine is rebuilt or modified.
