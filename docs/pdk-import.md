# Real-PDK import and connectivity verification

RustROAD 0.2.0 now imports real-PDK LEF and mapped Verilog, preserves signal
connectivity through placement and exports connected DEF plus equivalent
Verilog. The integrated dual-PDK acceptance below passed against the frozen
release. The resource budget was written before fixture generation and runs.

The first campaign uses the real Ciel/open_pdks `sky130_fd_sc_hd` and
`gf180mcu_fd_sc_mcu7t5v0` technology LEFs, cell LEFs and TT Liberty data. Files
are read-only host paths pinned by SHA-256; no reference tool implementation is
read or translated. Original PDK data is not rewritten.

## Upfront resource budget

One four-bit registered datapath case is run serially per PDK. Cap each at 2,000 mapped cells and 16,000 cell/port connections.
At 256 bytes per endpoint and 512 bytes per instance, canonical connectivity
maps and placement/check state occupy less than 8 MiB per case. One timing
corner, two threads and at most 8 retained paths add conservatively 8 MiB graph
and tag state; no cuts or startpoint-to-endpoint paths are materialized. Allow
640 MiB for a parsed Liberty, 128 MiB for technology/cell LEF state, 256 MiB
runtime headroom and 256 MiB for the independent black-box tool. Expected child
RSS remains below 2 GiB. Run one child at a time with two build/test workers.

Native SAT obligations use a 10,000-conflict cap and 256 sweep candidates. Native
subprocesses have a 30 second wall deadline, a 2 GiB virtual-address cap and a
64 MiB file cap. Podman/reference processes receive an 8 GiB virtual-address
allowance while the reference container is limited to 2 GiB physical memory
and no swap; this permits the allocator’s virtual reservations without
relaxing the physical-memory bound. The complete campaign gets a 180 second external deadline.
Connectivity comparison uses sorted endpoint tuples O(E log E), with an
explicit 16,000-endpoint bound, not path enumeration. Check `free -h` before
execution and retain `/usr/bin/time -v` RSS/wall receipts. Larger (>4 GiB)
campaigns must use the parent cgroup/scale.lock policy instead of these defaults.

No routing, extracted parasitics, full-chip DRC/LVS or GDS signoff is implied by
an import, a connected DEF, successful CEC or legal standard-cell placement.

## Reproduction contract

`fixtures/pdk/local-real-pdks.json` pins original Ciel data in the installed
container store. It is a local profile, not a distributable PDK package or a
claim that the store path is portable. The version-directory token is
`f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7`; hashes, byte counts and exact paths are
recorded separately for both technology LEFs, cell LEFs and Liberty files.
Sky130 selects `unithd` (0.46 × 2.72 µm, 1000 DBU/µm). GF180 selects
`GF018hv5v_mcu_sc7` (0.56 × 3.92 µm, 2000 DBU/µm). The test uses N-oriented
rows and checks integer DBU dimensions against an independent OpenDB reader.
Power-rail row alternation is outside this placement-only scope.

The physical import is a fresh workspace operation:

```sh
rustroad --workspace .work/physical import-pdk --base-rev rev0 \
  --tech-lef /absolute/path/to/technology.tlef \
  --cell-lef /absolute/path/to/cells.lef \
  --netlist /absolute/path/to/mapped.v --floorplan /absolute/path/to/floorplan.json
rustroad --workspace .work/physical place --base-rev rev1
rustroad --workspace .work/physical export-verilog > .work/exported.v
rustroad --workspace .work/physical export-def > .work/connected.def
```

A floorplan defines `name`, integer `units_per_micron`, `die_area`, `core_area`,
`site {name,width,height}` and `rows [{x_start,x_end,y}]`; cells and their pin
connectivity come from mapped Verilog and real LEF. Netlist buses, escaped wire
identifiers, bit-selects and assignment aliases are exercised. Exported Verilog
preserves logical connectivity. DEF top-level pins are logically connected but
**unplaced**, with explicit `io_placement_status: not_placed`; no IO geometry is
invented. Power pins stay separate from signal pins and the power grid remains
unchecked.

The acceptance datapath has four registered bits, four AND outputs, XOR/data
gating and output aliases. It has no reset or initial-state assumptions and no
implicit constant rails requiring tie-cell insertion. The independent Yosys
reader canonicalizes the endpoint partition of each net rather than comparing
net names. It compares RustSys output, RustROAD exported Verilog and real-PDK
OpenDB DEF imports. Native and external CEC plus 512-case RTL/mapped Verilator
simulation test behavior. A valid but rewired input is rejected by the
connectivity comparator and CEC; an unknown pin must be rejected by the import
before publishing workspace `HEAD`.

The enabled-feedback prototype exposed a limitation of the selected Yosys
`equiv_simple -seq 2` setup: it did not establish initial-state correspondence
for the register feedback path. This was an inconclusive checker result, not a
counterexample. Its raw probe is retained in `.work/pdk-preparation/cec-preflight`.
Acceptance uses a feed-forward registered datapath that the independent checker
can prove without reset assumptions. Extending acceptance to changed-feedback
ECOs requires a sound independently validated state-mapping contract.

```sh
free -h
ulimit -v 8388608
/usr/bin/time -v -o .work/physical-repeat.time timeout 180s \
python3 scripts/verify_physical.py --workspace .work/physical-repeat --base-rev empty \
  --pdks fixtures/pdk/local-real-pdks.json \
  --rustroad /absolute/path/to/rustroad --rustsys /absolute/path/to/rustsys \
  --rusteda /absolute/path/to/RustSTA/rusteda --ruststa /absolute/path/to/ruststa \
  --yosys /absolute/path/to/yosys --opensta /absolute/path/to/sta \
  --verilator /absolute/path/to/verilator
```

The script's evidence workspace must be new; its lock and `HEAD` are independent
of child RustROAD/RustSTA workspaces. All tool paths are pinned by executable
hash at runtime. OpenROAD runs from an immutable existing OCI image, with its
actual executable hash recorded, no network, a read-only root, 2 GiB cgroup
memory cap, two CPUs, bounded files, and an inner 25 second timeout. The internal
`/usr/bin/time` receipt records actual OpenROAD RSS; host `podman` RSS is not
presented as OpenROAD memory. No reference implementation is inspected.

## Verified RustROAD 0.2.0 results

[Tracked results](evidence/pdk-import-results.json) contain the complete scoped
findings and 70 hashed process receipts. [Source pins](evidence/pdk-import-sources.json)
identify the harness, its helper, the real-PDK manifest and all six PDK files.
The frozen RustROAD executable SHA-256 is
`807e90f4b6cae9d0a120961a05690499afccbe2d987dd2e108256713e70342ee`.
All engine entrypoint hashes were unchanged from start to finish.

| Acceptance check | Sky130 HD | GF180 MCU 7T5V0 |
|---|---:|---:|
| Mapped standard cells / retained state bits | 16 / 4 | 20 / 4 |
| Logical signal nets | 26 | 30 |
| Connected signal endpoints | 74 | 82 |
| Stored pins, including preserved PG pins | 138 | 162 |
| Top-level terminal directions verified in OpenDB | 18 | 18 |
| Native / external CEC | equivalent / proved | equivalent / proved |
| Verilator input combinations | 512 passed | 512 passed |
| Imported master identity and integer dimensions vs OpenDB | passed | passed |
| Signal/PG pin definitions, including direction and use | 30 matched | 36 matched |
| MPin rectangle geometry, layers and port grouping | 65 rectangles matched | 114 rectangles matched |
| Obstruction rectangle geometry and layers | 124 rectangles matched | 55 rectangles matched |
| OpenDB placement and signal-net partition comparison | passed | passed |
| Deliberately rewired DEF remains geometrically legal but differs logically | detected | detected |
| Unknown cell pin import, before HEAD publication | rejected | rejected |
| Deliberately rewired Verilog, topology and CEC | rejected | rejected |
| RustSTA full, independent and no-op incremental/full checks | agree | agree |
| Maximum setup/hold difference vs OpenSTA | 0.000000602940417593345 ns | 0.0000008695038307138248 ns |
| Constrained / unconstrained timing endpoints | 12 / 0 | 12 / 0 |
| OpenROAD positive-case peak RSS | 88,904 KiB | 86,096 KiB |

The full serial campaign took **12.61 seconds**, with **289,556 KiB** maximum
host-process RSS. It includes synthesis, LEF/link import, placement, immutable
workspace publication/readback, exports, graph comparison, CEC, Verilator
compilation/execution and timing checks. It is a small integration workload,
not a chip-scale performance comparison or a claim that placement causes the
observed ideal-wire timing numbers. Raw receipts remain under
`.work/pdk-import-geometry-final/`; the outer `/usr/bin/time -v` receipt is
`.work/pdk-import-geometry-final.time`. Container-internal time receipts measure
OpenROAD itself separately from `podman`.

The timing comparison uses TT Liberty, ideal clocks and explicitly matched
ideal wires, with a tolerance of 0.001 ns. The Sky130 verification-only
projection removes wire-load metadata without modifying original cells or
NLDM data. The production PDK files remain unchanged. Native timing reports
retain unsupported `min_pulse_width` and `minimum_period` diagnostics; the LEF
reader similarly preserves diagnostics for unmodeled electrical, spacing,
antenna and routing rules. These passed **setup/hold and import/connectivity**
checks do not certify those unimplemented rule classes.

The workflow has two separate and honest physical limitations: I/O pins are
connected but not physically placed; power/ground pin geometry is retained but
power rails and power connectivity are not built. No route, extracted wire RC,
DRC, LVS or GDS stream-out is performed. `full_rtl_to_gds_status` is therefore
still `unverified`, and those analyses remain `not_run` or `unchecked`.

Two setup pitfalls are recorded in the raw preparation runs. Podman's Go
runtime needed `GOMAXPROCS=2` under the process guard on this many-core host.
OpenROAD's allocator needed more than a 2 GiB virtual-address allowance before
performing useful work, despite low RSS; the final run uses 8 GiB virtual
address space for its controller/reference process and a separate 2 GiB
physical-memory cgroup. The unsuccessful preparation receipts are preserved,
and neither was mistaken for an EDA correctness failure or a passing result.

### Additional pin/obstruction geometry oracle budget

Before extending the external oracle, budget at most 16,000 rectangular shapes
and 512 pin definitions across the used masters. At 128 bytes per shape plus
pin/MPin grouping and sort keys, this adds conservatively under 8 MiB per
interpreter. The actual smoke uses 4 Sky130 and 5 GF180 masters. The comparison
sorts bounded rectangle lists; it never enumerates overlapping pairs. All
signal and PG pin directions/uses, MPin grouping, layer names, integer box
coordinates and obstruction boxes are compared with public OpenDB data. All
189 Sky130 and 169 GF180 rectangles matched exactly. Detailed imported-geometry
certificates are tracked alongside the results: [Sky130](evidence/pdk-import-sky130-opendb_master_geometry.json)
and [GF180](evidence/pdk-import-gf180-opendb_master_geometry.json).
