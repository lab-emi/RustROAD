# Foundry-DRC-clean via landings and pin access

LEF-rule mode removes the same-net geometry defects that the foundry decks
flagged at via landings and pin access of the routed `fixtures/flow/pipeline.v`
block on both default PDKs: notches between via pads and their own wires or
pins, sub-AREA via-pad islands, and a via pad wider than a narrow pin leg.

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
  SPEF. A patch is at most a few tenths of a um^2, so the missing C is
  small, but it is not zero. A native extractor
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

A placement-only copy of the layout (routing stripped) is checked as well.

## Receipts

The run receipts of this change are in `docs/evidence/router-drc.json`, kept
as historical evidence for reproducibility, not as current results. The hard
caps are unchanged by the LEF-rule mode. Hold closure is a separate work item
and must be done on top of this router.

## Limits

- The lattice router's per-sink search budget limits it to small blocks: the
  denser six-register fixture (`.work/tap-release-final/*_literal.v`) ends
  with `BUDGET_EXCEEDED` (routing expansion limit, per-sink search) with or
  without `lef_rules`. Nothing here claims scaling.
- The native same-net rules cover:
  - same-net notches and spacing (Chebyshev, Euclidean at corners);
  - point contacts;
  - AREA;
  - MINSTEP at access landings only.

  Wide-metal spacing, end-of-line spacing, MINENCLOSEDAREA and LEF58 rules
  rely on the foundry decks alone.
- The LVS reference is RustROAD's powered export, which `verify_routing`
  links to the RTL by CEC.
