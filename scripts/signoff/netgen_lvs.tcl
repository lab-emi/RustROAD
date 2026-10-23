# SPDX-License-Identifier: Apache-2.0
# Netgen LVS of a Magic-extracted layout against a powered structural Verilog
# reference read on top of the PDK standard-cell SPICE (black-box use).
# usage: env LAYOUT=<spice> TOP=<cell> REFV=<verilog> STDSPICE=<spice> SETUP=<setup.tcl>
#        OUT=<comp.out> netgen -batch source netgen_lvs.tcl
set c1 [readnet spice $::env(LAYOUT)]
set c2 [readnet spice $::env(STDSPICE)]
readnet verilog $::env(REFV) $c2
lvs "$c1 $::env(TOP)" "$c2 $::env(TOP)" $::env(SETUP) $::env(OUT) -json
puts "RR_NETGEN_DONE"
quit
