# Supply rails and tie cells: pinned PDK data

This investigation reads only LEF/Liberty data from the existing Ciel PDK tree
at revision `f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7`. It reads no reference
implementation. Sources are the same full `sky130_fd_sc_hd` and
`gf180mcu_fd_sc_mcu7t5v0` files recorded by `fixtures/pdk/local-real-pdks.json`.
All dimensions below are microns in the source; multiply exactly by the selected
DBU per micron. Existing workspaces use 1000 for Sky130 and 2000 for GF180.

| Data | Sky130 HD | GF180 MCU 7-track |
| --- | --- | --- |
| Site | `unithd`, 0.460 × 2.720 | `GF018hv5v_mcu_sc7`, 0.560 × 3.920 |
| Primary power pin / layer | `VPWR` / `met1` | `VDD` / `Metal1` |
| Primary ground pin / layer | `VGND` / `met1` | `VSS` / `Metal1` |
| N-oriented lower rail | ground: y −0.240 … +0.240 | ground: y −0.300 … +0.300 |
| N-oriented upper rail | power: y 2.480 … 2.960 | power: y 3.620 … 4.220 |
| Body power / nwell | `VPB` / `nwell` | `VNW` / `Nwell` |
| Body ground / pwell | `VNB` / `pwell` | `VPW` / `Pwell` |

The primary rail rectangles of the inspected ordinary, tie and filler masters
span x = 0 … master width. Some GF180 pins also have narrower fingers; those
fingers are not a replacement for the full-width rail. The Liberty `pg_type`
labels primary supply pins separately from nwell/pwell bias pins. Merely attaching
a bias pin to a net does not establish a well tie/contact or latch-up compliance.

Sky130's `sky130_fd_sc_hd__conb_1` is 1.380 × 2.720. Its Liberty output `HI`
has function `1`, and `LO` has function `0`. Both are ordinary signal outputs;
constant consumers should connect to their respective signal nets, while the
cell's supply pins connect to supply nets. GF180 uses two masters, each 2.240 ×
3.920: `gf180mcu_fd_sc_mcu7t5v0__tieh` has output `Z`, function `1`;
`gf180mcu_fd_sc_mcu7t5v0__tiel` has output `ZN`, function `0`. Maximum fanout,
capacitance, transition and physical tie-cell spacing remain separate constraints.

The smallest filler is `sky130_fd_sc_hd__fill_1` (0.460 wide) or
`gf180mcu_fd_sc_mcu7t5v0__fill_1` (0.560 wide). Sky130 also has 2/4/8-site fillers;
GF180 has 2/4/8/16/32/64-site fillers. Their rail geometry permits closing the
occupied row's horizontal rail gaps. Filler selection does not replace tapcells,
well-tie insertion, detailed routing or foundry DRC.

For FS placement, x is translated unchanged and a local y coordinate becomes
`placed_y + master_height - local_y`, where `placed_y` is the oriented bounding
box's lower edge. A rectangle's lower/upper y bounds exchange under that mirror.
Thus consecutive N/FS rows have matching primary polarity at their shared edge;
two adjacent N rows place different supply rails against each other. The external
OpenDB checker must verify the export's origin/location convention independently.

## Fixed vias for a later strap connection

The Sky130 fixed via `M1M2_PR` has cut layer `via` rectangle
(−0.075, −0.075) … (+0.075, +0.075), `met1`
(−0.160, −0.130) … (+0.160, +0.130), and `met2`
(−0.130, −0.160) … (+0.130, +0.160). GF180 fixed `Via1_HV` has cut layer
`Via1` ±0.130 square, `Metal1` (−0.190, −0.130) … (+0.190, +0.130), and
`Metal2` (−0.140, −0.190) … (+0.140, +0.190). The GF180 file records 4.500 ohms
for that via. These are geometry facts, not an extracted resistance model or DRC
approval for any proposed wire width/placement. The selected primary rail bands
contain the via's first-metal enclosure when centered at the row edge.

## Validator resource and coverage contract (before implementation)

The rail validator receives immutable selected masters, explicit N/FS cell
placements and declared rows. It transforms primary pin rectangles, forms sorted
horizontal intervals at the top/bottom row edges, checks their union over each
occupied row span and checks shared-boundary polarity. It reports exact gaps and
mismatches. It does not enumerate electrical paths, route a grid, or infer body
connections. Only rectangles strictly straddling the edge provide rail coverage;
polygons on the selected rail layer are explicitly unsupported in this slice.

Default hard caps: 250,000 cells/masters, 100,000 rows, 250,000 inspected primary
pin shapes, 1,000,000 transformed rail intervals, 64,000,000 bounded work units,
and 256 diagnostics. At roughly 64 bytes/interval with capacity growth, intervals
use <128 MB; cached master profiles and placement/row lookup buffers add <64 MB;
row rail summaries add <16 MB. One geometry scope, no timing corners, no per-worker
copies, and two test workers give a conservative <512 MiB run estimate. Tiny unit
fixtures retain <8 KiB each excluding runtime. Sorting is charged conservatively
before execution; scans count explicit work. Exceeding any cap returns a typed
budget diagnostic. Use the repository's 8 GiB virtual-address and 180-second
wall guards plus `/usr/bin/time -v` for tests. The external checker is independent
and must validate both PDKs and negative polarity/gap controls.

## Local validator verification

Five focused unit cases passed: alternating N/FS common rail bands; an N/N
shared-boundary polarity fault; an internal gap repaired by adding a filler;
a wrong orientation/missing supply pin; and explicit work/interval/shape budget
exhaustion. `cargo clippy -p rustroad-core --all-targets -- -D warnings` passed.
The guarded targeted test command took 0.74 s and reported 267,312 KiB maximum
RSS including Rust compilation (`.work/power-unit-tests.time`). These are small
synthetic tests, not independent PDK or signoff evidence. The separate external
checker owns the dual-PDK physical comparison and planted-defect acceptance.
