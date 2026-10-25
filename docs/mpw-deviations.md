# Minimum-pulse-width deviation evidence

The ordinary timing-accuracy limit remains **1 ps**. The owner permits a
separate `deviation_declared` result for a pulse-width row only when all three
conditions are established on the same input files and pinned tools:

1. A bounded resistance sweep brackets a discontinuity in the reference
   clock driver's reported waveform slew between the real network and a
   nearby sample. Rise and fall steps are located separately.
2. At the nearby sample on the opposite side, the **raw** difference in both
   required pulse width and slack is at most **0.2 ps**. No step is subtracted
   from the original mismatch.
3. An independent transistor/RC simulation favors RustSTA, with numerical
   uncertainty smaller than the observed advantage.

The gate records the original mismatch, nearby sample, step bracket, circuit
results, commands, raw reports and input/tool/script SHA-256 identities. It
validates the fresh probe against the original gate measurements before using
it. Missing, stale, incomplete or inconclusive evidence leaves the ordinary
failure intact. Endpoint, minimum-period, DRV, setup and hold checks receive no
exception. A declared deviation propagates to the top-level status and is
listed in `declared_deviations`; it is never reported as plain `passed`.

## Circuit experiment

`scripts/mpw_circuit.py` reads standard Liberty, Verilog and SPEF data. It
simulates the original extracted RC network and the driver's PDK transistor
model with Ngspice. The characterized driver is placed in a separate copy of
the library/netlist to cancel a common cell-characterization offset. Original
PDK and design inputs remain unchanged. The resulting comparison is evidence
about the RC/waveform discrepancy, not foundry timing signoff.

The characterization holds out alternating load samples to estimate
interpolation error. The circuit comparison covers both reported input-slew
bounds and repeats transient measurements at 0.5 ps and 0.25 ps time steps.
Its margin includes the observed convergence difference and twice the
held-out interpolation error mapped through the pulse table. Both input
bounds must favor RustSTA. The qualified transistor experiment currently
covers SKY130 single-input clock buffers/inverters and one-dimensional pulse
tables. Other cases remain strict failures unless separately qualified.

Limits: eight affected nets and 64 affected rows per source; 64 timing probes
per net; resistance factors within 20% of the original; step bracket width
at most 0.0001 in resistance scale; 10,000 circuit nodes; 50,000 resistors;
128 characterization cells; 32 MiB input SPEF; 16 MiB subprocess logs.
Subprocess memory/time guards remain active. The FIFO prototype's measured
maximum child RSS was 524,064 KiB.

## Reproduction

`scripts/rtl2gds.py` generates evidence automatically for eligible failing
MPW rows after ordinary timing checks. An isolated experiment can also be run
with `scripts/mpw_explain.py`:

```sh
systemd-run --user --scope -p MemoryMax=2G -p MemorySwapMax=512M \
  timeout 600s python3 scripts/mpw_explain.py \
  --pdk sky130 --corner ss --top fifo \
  --netlist RUN/sky130_i1_exported.v \
  --liberty PDK/libs.ref/sky130_fd_sc_hd/lib/sky130_fd_sc_hd__ss_100C_1v60.lib \
  --sdc RUN/sky130_postroute.sdc --spef RUN/sky130_i1_max.spef \
  --run NEW_EVIDENCE_DIRECTORY
```

Set `RUSTEDA_GNU_TIME` to the pinned GNU time executable when it is absent
from PATH. Tool locations and identities come from
`fixtures/flow/rtl2gds-tools.json`; the transistor model tree is pinned in
`fixtures/flow/signoff-pinned.json`. The output includes `explanation.json`,
raw tool receipts and SPICE decks. The command only evaluates MPW parity;
full acceptance still requires the complete RTL-to-GDS gate.
