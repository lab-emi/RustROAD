# Bounded reference Verilog header normalization

OpenROAD and Yosys can emit non-ANSI module headers. The existing physical
verification readers consume ANSI port declarations. The verification utility
`scripts/normalize_verilog_ports.py` translates the module/port prefix and keeps
intervening net declarations and the remaining body bytes verbatim.

```bash
python3 scripts/normalize_verilog_ports.py \
  --source reference-powered.v --out reference-powered-ansi.v
```

Keep the original producer file and the JSON normalization receipt. The receipt
binds both file hashes, body offsets and hashes, and any retained declarations.
An existing output is never overwritten; failures do not publish a partial file.

The accepted input is a non-parameterized mapped module with literal port ranges.
Generated net declarations may appear between port declarations. Later port-net
redeclarations must agree in type, signedness and range. Ambiguous inline or
multiline declarations, macros, unsupported types and semantic prefix pragmas
produce explicit diagnostics. Already-ANSI input is copied exactly. This utility
is a format adapter for generated netlists; it is not a general RTL frontend.

Bounds are1MiB per prefix statement,16MiB per declaration prefix,65,536 converted
ports and16GiB per file. Body data is processed in1MiB chunks. The64MiB streaming
probe used22,144KiB maximum process RSS under a256MiB guard.

`scripts/verify_verilog_ports.py` runs pinned black-box Yosys on bounded SKY130
and GF180 powered-netlist fixtures. The canonical output must be byte-identical
before and after normalization, and a changed port direction must be detected.
Its source fixtures are capped at4MiB each; reference calls use1GiB/120s limits.
The normalizer also has declaration, byte-budget and output-preservation tests.

Normalization establishes format preservation. Electrical equivalence, port-label
checks, device matching, DRC and the other physical acceptance criteria remain
separate requirements. In particular, it does not resolve the SKY130 `conb_1`
device/bulk-pin limitations observed with the pinned LVS configuration.
