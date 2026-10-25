# Timing-closure ECOs of the track recipe

The closure loop of `scripts/rtl2gds.py` (`hold_closure.Flow.close`, track
recipe, `sizing_profile` set) repairs DRV, setup and hold on complete
layouts: every iteration is import, place, CTS, route, native extraction
and RustSTA (pinned 0.4.3, 26bd6eb) at tt+nom / ss+max / ff+min. This page
describes the ECOs added for the larger designs (buffer insertion,
`whatif`-verified sizing, path-aware hold fixing, seeded timing-driven
placement) and the loop order; results are in `docs/rtl2gds.md` (final2
table; evidence `docs/evidence/rtl2gds-final2.json`; earlier rounds
`docs/evidence/rtl2gds-closure.json`, `docs/evidence/critical-path.json`).

Nothing here changes an SDC, Liberty, RC profile, threshold, deck or status
rule. Every ECO is a typed `rustroad.eco-plan.v1` applied by
`rustroad apply-eco` (or written by `rustroad hold-eco`), every decision is
re-checked on the next complete layout, and RustSTA is the only timing
oracle inside the loop.

## Loop order and bounds (`Flow.sized_step`)

One decision per layout, at most `budgets.max_hold_iterations` (8) layouts:

1. **DRV / setup** (`closure_eco`): any max_transition / max_capacitance /
   max_fanout violation, or setup < 0 at tt/ss -> buffer insertion plus
   `whatif` sizing, applied as one plan; timing-driven net weights for the
   next placement while setup fails.
2. **Hold** (`hold_eco`, path-aware, partial plans allowed) when hold is
   below the ECO target. After the first DRV/setup ECO, a layout that needs
   both gets both: the hold plan first (it is bound to the timed netlist),
   then the DRV/setup plan applied on top of it, never re-buffering a sink a
   hold chain moved.
3. **Antenna** (unchanged) on a setup-, hold- and DRV-clean layout.

DRV/setup ECOs stop (the loop goes on to hold) after two consecutive
layouts on which the previous one improved neither the DRV count nor the
worst tt/ss setup slack by 0.05 ns (`SETUP_STAGNATION_*`). The loop ends
when everything is closed, when nothing applies, or at the layout bound.

From the second layout on, placement is **seeded** from the previous
layout (`place --seed`): unchanged cells keep their positions up to
legalization displacement, so a small ECO does not reshuffle every wire
(without it, every re-placement moved marginal DRV violations to new nets).

## Buffer insertion (`scripts/closure_eco.plan_buffering`)

Inputs: the RustSTA `drv` report (it lists every pin, so the driver's total
load per corner comes from its max_capacitance row and each pin's slew from
its max_transition row), the `pins` reports at ss and tt (per-pin setup
slack), the layout DEF (integer DBU positions) and the sizing profile's
Liberty drive data (below).

* **Nets planned:** a transition or capacitance above 0.95 x its limit
  (planning margin: the next layout moves every wire) or a fanout above
  its limit, at any corner, on a net driven by an instance of the mapped
  netlist or by an input port. Skipped, with the reason recorded: a driver
  whose Liberty model meets 0.8 x the limit at its own load (the slew comes
  from upstream), a small-fanout net a larger driver can fix (left to
  sizing), a net whose sinks are already ECO buffers (no stacking), CTS
  nets.
* **Clusters:** the movable sinks (instance input pins and output ports
  read through an `assign`) are split by recursive bisection on their
  positions (larger spread axis, ties by name) until each cluster's load
  (pin capacitances + wire capacitance per DBU of this net x cluster HPWL)
  is within what the strongest buffer drives at 0.6 x the transition limit
  and 16 sinks; a second level is built when more than 16 buffers would
  load the driver. Each cluster gets the buffer meeting the target with the
  smallest estimated delay (its own delay plus the driver's extra delay for
  its input capacitance) among those whose input capacitance fits the
  driver's budget. The analytic placer then pulls the buffer to its
  cluster.
* **Setup guard:** a sink with less than 0.75 ns of setup slack never goes
  behind a buffer; any other moves only if its estimated slack after the
  edit (driver delay at the new load + buffer delay + 0.35 x buffer output
  slew, against today's driver delay + 0.35 x slew) stays >= 0.15 ns. The
  estimated cost of each moved sink is charged to every pin downstream of
  it through combinational cells (max over paths, bounded walk), so two
  buffers of one plan cannot spend the same path's slack twice. Measured
  on the GF180 designs: without these rules every stage of uart_tx's
  critical path got a buffer (0.7-1.0 ns each at ss).
* **Input ports** are buffered the same way (port nets cannot be resized).
* **Critical-sink isolation:** on a net with failing setup, sinks well
  above the net's worst slack move behind one buffer (same guard) so the
  driver sees less load.
* Bounds: 512 buffers, 32 clusters per net, 16 sinks per cluster, two
  levels, 4,096 sinks per net, 2,000,000 pin visits of the guard walks.

`buffer_sinks` (`rustroad_formats::eco`) is the ECO primitive: all named
sinks of ONE net and the named output ports read through `assign` move
behind one new buffer (`rr_eco_buf_<k>`); the net keeps its driver and
every other sink. Buffer masters: the sizing profile's identity-function
families (Liberty-derived, cell policy applied), accepted only after the
same `BufferCell::check_identity` as hold cells; the gate's `cell_proofs`
proves every inserted master with Yosys at each corner Liberty.

## Sizing with RustSTA `whatif` (`Flow.whatif_sizing`)

A resize keeps every net, so `ruststa whatif` evaluates it exactly on the
layout's SPEF (bit-identical to a full analysis). First, one call evaluates
two **bulk** candidates: every DRV driver (worst first) one size up, and two
sizes up, within the tap gap and the utilization cap; the better one is
accepted as a whole only under the same rule as a single resize (below).
Then the single-resize rounds: candidates are drivers of (near-)violating
nets (one or two sizes up, or one down: the largest member is not always
the fastest edge at a light load, GF180 aoi22_4 vs aoi22_2), ECO buffers
loading such a net (one size down), and the cells on the worst setup paths
of up to 48 failing endpoints (one down, one and two up), within the tap gap
and a 74 % standard-cell utilization of the rows that also bounds buffer
insertion (a GF180 regfile netlist at 76 % failed CTS packing). Each round
evaluates every candidate on top of the resizes accepted so far and accepts
up to 12 improving ones on distinct instances: fewer DRV violations without
losing worst setup (or keeping it >= 0), a better worst setup slack, or an
equal worst and a better total; a resize may lower hold below the ECO target
by at most 0.5 ns (the hold ECO repairs it next). The next round's base
re-checks the accepted set as a whole and keeps only the single best edit of
a round that made it worse. Bounds: 6 rounds, 48 candidates per round, 128
candidates per layout and 640 per closure loop, both scaled by 200 / logic
instances within 0.5x..4x (counter 512 per layout, mult 64): counts, never
measured time, so the determinism rebuild evaluates exactly the same
candidates (measured cost per candidate on this host: 0.13 s on counter,
1.1 s on regfile, 1.9 s on mult).

The earlier sizing step (one size up on every setup-path cell, model-free)
is gone: on counter GF180 it took the netlist from +0.38 to -0.77 ns of
ideal-wire setup slack.

## Path-aware hold fixing (`rustroad hold-eco --pins-report ...`)

With pins reports (setup slack of every pin at ss and tt, same STA
revision, bound like the other reports), `hold-eco` places delay on the
worst hold path instead of in front of the endpoint:

* **Points** of a violating endpoint: every sink pin of its worst hold
  path (`net` stages; `launch` and clock pins never), and for each of them
  the whole net feeding it when it has at most 4 input-pin sinks (one chain
  in front of every sink, e.g. a small input port). A point is kept only if
  it lies on the worst path at every corner where the endpoint violates.
* **Setup room** of a point: the setup slack of the pin it delays every path
  through (the sink pin, or the net's driver / input port) at ss and tt,
  minus the delay already planned on points of the same path, minus the
  0.05 ns setup margin, against the chain's max(rise, fall) delay per
  corner.
* **Choice:** endpoints largest need first; among the feasible points the
  one on the most violating paths, then the most downstream (a point after
  a reconvergence delays every parallel branch at once; branch pins of a
  multi-input gate fixed one by one cost one layout each on regfile
  sky130), then the smallest chain (same multiset search as before). The
  loop plans with 0.03 ns above its ECO target (estimates at one
  representative load under-deliver). Delay planned on a
  shared point is credited to every later endpoint whose path crosses it.
* **Infeasible:** an endpoint none of whose points has the room is listed
  (`HOLD_ECO_INFEASIBLE`) with the remaining need per corner and the setup
  budget of each point as evidence. `--allow-partial` applies the feasible
  part (status `partial`); without it the plan fails as before. Without
  `--pins-report` the planner is the original endpoint planner (same
  edited netlists; the plan JSON has three more parameter fields), which
  the legacy recipe (pipeline) keeps using.

Net-level chains are emitted as one `buffer_sinks` (the sink-side cell)
plus `insert_buffer` on its input for the rest of the chain. The
`removed_hold_cell` control removes a chain wherever it sits (every pin it
drives returns to the original net).

## Timing-driven, seeded placement (`rustroad place`)

* `--net-weights FILE` (`rustroad.net-weights.v1`): per-net integer weights
  in milli-units (1000 = 1.0, at most 16,000) scale the bound-to-bound
  springs and the detailed-placement HPWL cost; unit weights reproduce the
  unweighted placement bit for bit. The loop writes them while setup fails:
  `1000 + 4000 x crit^2`, `crit = clamp((worst + 1 ns - slack) / 1 ns)` of
  the net driver's setup slack (worst of ss/tt). Unknown names are counted
  (`unmatched`), never guessed.
* `--seed FILE` (`rustroad.placement-seed.v1`, instance -> lower-left DBU):
  seeded cells target their seed, new cells the mean of their seeded or
  fixed net neighbours (4 propagation passes), then the same legalizer and
  detailed placement as global placement; no global solve. The report
  gives seeded / unseeded / unmatched counts and the displacement.
* Both are stored with the physical bundle, so the CTS re-placement uses
  them too; integer DBU in and out, deterministic, work-bounded.

## Liberty drive data (`scripts/build_sizing_profile.py`)

`cells[].drive.<corner>`: input pin capacitances and, for one-output
families, the worst delay and output transition (max of rise/fall, over
combinational and edge arcs) at 0.2 ns input slew and 5 / 50 fF, by
bilinear lookup inside each corner Liberty table (no extrapolation). The
planners interpolate linearly between the two points; estimates only
choose an edit, the next layout decides.

## Critical-path report (`scripts/critical_path.py`)

For finished gate runs: RustSTA without parasitics and with the ideal clock
on the synthesized and on the final closure netlist (pre-layout), and the
gate's own final native-SPEF analysis (post-route), worst setup at ss and
tt, the implied minimum period `period - worst slack`, and the worst path's
end points. GF180 without SPEF is `ideal` (no wire-load table), SKY130 is
`wireload_lumped` (Liberty default wire load, an upper bound of ideal).

## What limits GF180 now (final2, `docs/rtl2gds.md`)

With the owner rules (per-PDK clock period, GF180 DRV dont_use) and the GF180
floorplan at 0.35 utilization, counter, uart_tx and fifo close every GF180
gating item; alu, regfile and mult close setup and hold on both SPEFs and
keep only `drv` (3, 8 and 12 nets above 1.0 ns at ss on the selected
layout). What is left there, from the final layouts:

| Design | Remaining GF180 transition violations at ss | Why the loop did not remove them |
|---|---|---|
| alu | 3 nets (worst oai21_2 into one buf_2, 1.67 ns) | 8-layout bound reached while still improving (DRV 522 -> 6 pins) |
| regfile | 8 nets: nor2_4 into four ECO buffers (2.03 ns), seven dlyb_1 hold cells into buf_8 ECO buffers (1.08-1.65 ns) | the driver cannot grow (largest member, or a delay cell), the ECO buffers behind it shrink only one size per layout; an extra buffer level was tried and made alu/mult worse (reverted) |
| mult | 12 nets, aoi22_2/aoi22_4 into one ECO buffer (1.01-1.37 ns) | aoi22 is close to the limit at ss (best member 0.80 ns at the smallest sink with a 0.2 ns input, more with the real input slew); 8-layout bound |

The ten GF180 families that cannot reach 1.0 ns at all are excluded from
synthesis by the owner's dont_use rule (`scripts/derive_drv_dont_use.py`,
`fixtures/flow/gf180-drv-dont-use.json`). Earlier rounds (E6, 10 ns on every
design and no dont_use) are in `docs/evidence/rtl2gds-closure.json`.
