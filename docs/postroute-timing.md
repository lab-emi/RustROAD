# Post-route timing feedback with RustSTA 0.3

The dual-PDK route/extract/timing harness now requires `wire_model=spef_rc`
and `delay_calculator=nldm_ceff_ramp`. It checks the finite-ramp driver/effective
load/slew model, with explicit propagated clocks, rather than the earlier
first-moment-only timing scope. The OpenSTA endpoint tolerance stays 0.001 ns.

A fresh run re-synthesizes the pipeline RTL, checks CEC and 512 simulation
vectors, imports and places real cells, builds CTS, routes all signals, exports
DEF and mapped Verilog, and invokes the pinned external extractor with actual
PDK rules. Original GDS/DEF geometry, supply/tap connectivity, clock skew and
negative controls remain in the acceptance flow. Both full timing recomputation
and no-op update consistency are checked; the latter is not a changed-ECO test.
RustSTA separately validates real cell replacement/restoration on each library
with fixed RC topology, independent transient circuits and CPU/CUDA propagation.

```sh
python3 scripts/verify_routing.py --workspace work/postroute --base-rev empty \
  --rusteda /path/to/rusteda-0.3 --ruststa /path/to/ruststa-0.3
```

Use the local pinned PDK profile and the usual process memory/time guards.
The new top-level `timing_accuracy_status` separates endpoint parity from the
existing `functional_status` and overall result. An absent tool never passes.

The [HTML report](postroute-timing.md) records measured timing and resources.
Sky130's nominal setup/hold checks meet the constraints, with only about 0.247 ps
hold margin; that margin is below the 1 ps comparison gate. GF180 still has four
hold violations, worst about -0.635535 ns. Therefore the full flow correctly
remains `partial` even though both accuracy gates and the functional chain pass.
The next physical optimization must improve the data/clock relationship and
re-route/re-extract before checking again. No violation is clipped or hidden.

Scope: TT/f64, factor-one grounded coupling, primary propagated clocks and
CPPR off; 19 signal nets, 46 terminals and eight timing endpoints per PDK.
This fixture is not industrial-scale throughput evidence. Complete DRC/LVS and
full RTL-to-GDS signoff remain unverified; the current native routing/geometry
checks cover an explicit subset. Reference programs are black-box executables;
no reference implementation source is read, compared or translated.
