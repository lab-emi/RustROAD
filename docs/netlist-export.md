# Bounded structural Verilog export

`export-verilog` streams declarations, assignments and one instance at a time.
Both logical and powered output avoid cloning the entire imported netlist.
Powered output resolves scalar aliases with a linear, cycle-guarded traversal;
open pins are named deterministically in two bounded passes. Logical output
omits tagged filler and tap instances.

The global `--max-cells`, `--max-nets`, `--max-pins` and expression limits also
apply to the exported view. The ordinary defaults remain appropriate for small
designs. Large exports must explicitly raise these limits and specify:

- `--max-output-mib`: emitted bytes, default16MiB, maximum16GiB.
- `--export-memory-mib`: whole-process allowance including the loaded snapshot,
  default2048MiB, maximum64GiB.
- `--max-export-work`: planning and emission work, default200million units,
  maximum2^40 units.

The admission estimate includes declaration/name maps, owned generated names,
alias resolution, physical tags and one transformed record. It reserves256bytes
per declaration and96bytes per instance name, plus owned payloads. Runtime RSS
checks supplement the count-based allowance. Run large exports under an OS or
cgroup memory limit as well, including snapshot loading.

An exhausted budget returns a structured error and leaves the workspace
unchanged. Standard output may be partial; promote an output file only after
successful completion. The library entry point is
`Snapshot::export_verilog_bounded`, with `SupplyMode` and `ExportLimits`.

Tests compare bytes with the established exporter across alias chains, cycles,
open pins and physical-cell filtering. A300,001-instance stream checks explicit
limits above the ordinary250,000-instance default; an errored record prevents
successful completion. Independent dual-PDK physical gates remain required.
