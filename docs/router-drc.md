# Foundry-DRC-clean via landings and pin access

The routed `fixtures/flow/pipeline.v` block now has zero Magic `drc(full)` items
and zero KLayout main-deck items on both default PDKs. Before this change the
same harness found sky130 Magic 26 / KLayout 9 (m1.2 x5, m2.2 x4) and gf180
Magic 13 / KLayout 7 (M1.2a x4, M2.3 x3). All of them were same-net geometry
at via landings and pin access: notches between via pads and their own wires or
pins, sub-AREA via-pad islands, and a Via1_HV pad 5 nm wider than the clkbuf_1
`I` pin leg.

Scope is block level: no pad ring, no seal ring. IR, EM, SI, OCV and CPPR are
unchecked. Density is reported on its own and is never part of the block DRC
verdict. Only the decks below decide DRC; the native checks are a self-check
(Rule 8).

## What changed

- The LEF reader keeps the unconditional `AREA` value (exact DBU^2), `MINSTEP`
  (DBU) and cut-layer `ENCLOSURE BELOW|ABOVE a b` (DBU). Any other form of these
  statements is still reported as `LEF_SEMANTICS_UNMODELED`, and so is a second
  occurrence on the same layer. `MINSTEP` also gets a diagnostic, because the
  router enforces it only on pin-access landing pads. Neither default PDK
  defines `MINSTEP`. `MINENCLOSEDAREA`, `SPACINGTABLE`, `SPACING RANGE` and
  `LEF58_*` remain diagnostics.
- Setting `"lef_rules": true` in the routing profile turns the mode on. In this
  mode:
  - **Via variants.** For each policy via, every single-cut LEF fixed via on
    the same cut layer and metal pair is added, provided it passes the LEF
    cut ENCLOSURE rules. The policy via is tried first, then the others by
    pad area and then name. A search move takes the first variant that clears
    other nets. Where possible it avoids one whose pad would come within
    spacing of the net's own metal.
  - **Pin-access table.** It is built once for each (master, orientation,
    pin). Each candidate landing is checked against the master's other-net
    pins and OBS (spacing), and against notches with its own pin shapes.
    MINSTEP is also checked when the pad protrudes. Pads that lie entirely
    inside a pin rectangle are preferred. Escape wires and vias during the
    access search are checked for notches against their own pin too.
  - **Same-net pass after routing (bounded).** It works per (net, layer) on
    merged metal that includes the net's pin shapes. It runs in four steps:
    1. Prune dead-end wires and vias that lead to no access point.
    2. Fill notches, same-net gaps and point contacts narrower than the layer
       spacing (Euclidean at corners) with patch rectangles.
    3. Grow components below `AREA` with the shortest default-width stub from
       one of their via or wire nodes.

    Every patch and stub must clear other nets and must not create a new
    notch. After eight rounds, any violation left fails the command with
    `ROUTING_RULE` and leaves HEAD unchanged.
  - **Validation.** `validate()` (on route and on every reload) rejects
    same-net notches and gaps, point contacts, floating patches and sub-AREA
    islands. Unit tests plant a notch, a min-area island and a floating patch.
    The legacy checker accepts all three.
- Setting `"io_min_area": true` in the physical config grows square signal I/O
  pins to the I/O layer's `AREA`. On sky130 met3 this changes 0.40 um to
  0.49 um.
- Profiles without these flags behave exactly as before. Re-exporting existing
  workspaces and re-routing with the old profiles gives byte-identical DEF to
  the previous binary.

## Interfaces

| Item | Format |
|---|---|
| Routing profile | `lef_rules: bool` (default false; serialized only when true) |
| Physical config | `io_min_area: bool` (default false) |
| Technology layer (design.json) | optional `min_area` (DBU^2), `min_step` (DBU), `enclosure_below` / `enclosure_above` (`[a, b]` DBU); absent when the LEF has none |
| `RouteLayer` | optional `min_area`, `min_step` |
| `Routes` / export-routes | optional `patches: [{net, layer, rect: {lx, ly, ux, uy}}]`, omitted when empty |
| `routing.rbin` | new record kind 4: `[4, net, layer, lx, ly, ux, uy, 0]` (i64 LE) |
| DEF NETS | patch = `NEW <layer> ( lx ly ) RECT ( 0 0 w h )` (DEF 5.8 routing RECT) |
| Via names | `RoutingState.via_names` may now include LEF variants, e.g. `M2M3_PR_MR`, `Via1_VH`, `Via2_HH`, `Via2_VH`; a GDS writer must emit each used variant from the LEF |
| CLI | `rustroad --workspace W export-access-test --config PROFILE > access.def` (read-only) |

The workspace `format_version` stays 5. New fields are optional, so old
payloads still load. A binary from before this change rejects a payload that
contains the new fields.

Notes for other streams:
- OpenRCX reads DEF `RECT` patches without error but leaves them out of the
  SPEF. A patch is at most a few tenths of a um^2 (sky130: 9 patches; gf180:
  1), so the missing C is negligible, but it is not zero. A native extractor
  should include `routes.patches`. (Done in stream b0: `extract` and
  `export-gds` include patches, and `export-def` writes merged runs; see
  `docs/requalification.md`.)
- The OpenDB route probe ignores RECT shapes.

## Exact invocations

```sh
free -h; ulimit -v 8388608
/usr/bin/time -v timeout 300s python3 scripts/verify_routing.py \
  --workspace .work/ev_a --base-rev empty --rustroad target/release/rustroad \
  --rusteda <RustSTA>/.work/rc-wave/target/release/rusteda \
  --ruststa <RustSTA>/.work/rc-wave/target/release/ruststa
/usr/bin/time -v timeout 3600s python3 scripts/verify_router_signoff.py \
  --run-dir .work/ev_signoff --flow .work/ev_a --rustroad target/release/rustroad
```

`verify_router_signoff.py` works on the routed DEF in four steps:
1. Streams it to GDS with KLayout's LEF/DEF reader. This reference streamout
   uses the PDK `.map` and keeps the PDK cell GDS as SREFs.
2. Runs DRC inside the pinned image `hpretl/iic-osic-tools` (id `4c48415d8bd0`:
   Magic 8.3.678, Netgen 1.5.323, KLayout 0.30.9):
   - Magic `drc(full)` with Euclidean distances;
   - KLayout `sky130A_mr.drc` with feol, beol and offgrid, or `gf180mcu.drc`
     with `all,-antenna,-density`;
   - the antenna oracle: Magic `antennacheck` for sky130, the KLayout antenna
     deck for gf180;
   - the density decks.
3. Runs LVS: Magic extraction compared by Netgen, and KLayout LVS, each
   against the powered Verilog.
4. Exports and checks the pin-access test layouts, for both the LEF-rule
   profile and a legacy copy.

It reads every verdict from the report files, never from exit codes. It
counts KLayout items over all cells and rejects a missing top cell, an empty
bbox, a missing report or a NaN density.

The controls catch planted defects:
- a same-net island at half spacing: DRC count rises in both decks;
- a sub-AREA square: both decks flag it;
- one input swapped between two cells: both LVS tools report a mismatch.

A placement-only copy of the layout (routing stripped) gives 0/0 on both PDKs.

## Results (final binary, fresh runs)

| | sky130_fd_sc_hd | gf180mcu_fd_sc_mcu7t5v0 |
|---|---|---|
| Magic drc(full) / KLayout main | 0 / 0 (was 26 / 9) | 0 / 0 (was 13 / 7) |
| Antenna | 0 (Magic) | 0 (KLayout deck) |
| Density, separate | 0 items, passed | 8 global rules fail (DCF.1b, PL.8, M1-M5.4, MT.3), failed |
| LVS Netgen / KLayout | match / match | match / match |
| Access test: masters x orientations x pins | 22 layouts, 0 / 0 | 22 layouts, 0 / 0 |
| Same access test with `lef_rules=false` | 0 / 0 | Magic 5, KLayout 14 (M1.2a, M2.2a) |
| Wires / vias / patches | 4940 / 95 / 9 | 5002 / 76 / 1 |
| Expansions / comparisons (cap 2M / 200M) | 474,443 / 59.4M | 566,373 / 33.4M |
| route --scope all | 0.58 s, 15.9 MiB | 0.57 s, 14.3 MiB |

Further results:
- **Determinism.** Two independent flow runs give byte-identical DEF, Verilog
  and `export-routes` output. The SPEF differs only in the extractor's
  `*DATE` line. The in-flow re-route is also DEF-identical.
- **Full flow.** 28.1 s wall time, 403,648 KiB peak host RSS.
- **Signoff harness.** 154 s wall time, peak container RSS 474,824 KiB.
- **Hard caps.** All are unchanged.

Timing side effect, at tt with nominal RC:
- **sky130.** Before this change hold was +0.25 ps, already below the 1 ps
  parity gate. It is now -16.1 ps on one endpoint. The routed clock skew rose
  from 43.5 ps to 61.9 ps.
- **gf180.** Worst hold is -0.6336 ns; it was -0.6355 ns.
- **Parity.** RustSTA/OpenSTA parity is still under 1 ps: 0.545 ps on sky130,
  0.300 ps on gf180.

Hold closure is a separate work item and must be done on top of this router.

## Limits

- The denser six-register fixture (`.work/tap-release-final/*_literal.v`) still
  fails on both PDKs. The error is
  `BUDGET_EXCEEDED: routing expansion limit: per-sink search, pin 49 net 7,
  7 nets done`, and it is identical with `lef_rules=false`. This is the
  existing lattice router's scaling limit; nothing here claims scaling.
- The native same-net rules cover:
  - same-net notches and spacing (Chebyshev, Euclidean at corners);
  - point contacts;
  - AREA;
  - MINSTEP at access landings only.

  Wide-metal spacing, end-of-line spacing, MINENCLOSEDAREA and LEF58 rules
  rely on the foundry decks alone.
- The LVS reference is RustROAD's powered export, which `verify_routing`
  links to the RTL by CEC.
