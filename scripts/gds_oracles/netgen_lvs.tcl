# Netgen LVS: extracted layout vs powered structural Verilog over PDK cell SPICE.
# env: LAYOUT TOP REFV STDSPICE SETUP OUT
set c1 [readnet spice $::env(LAYOUT)]
set c2 [readnet spice $::env(STDSPICE)]
readnet verilog $::env(REFV) $c2
lvs "$c1 $::env(TOP)" "$c2 $::env(TOP)" $::env(SETUP) $::env(OUT) -json
quit
