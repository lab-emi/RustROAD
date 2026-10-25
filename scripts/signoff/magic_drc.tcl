# SPDX-License-Identifier: Apache-2.0
# Magic batch DRC (black-box use of Magic 8.3.678 in the pinned image).
# usage: env GDS=<gds> CELL=<top> magic -dnull -noconsole -rcfile <pdk magicrc> magic_drc.tcl
# Prints RR_* markers parsed by scripts/signoff/parsers.py:parse_magic_drc.
# The DRC style drc(full) is selected explicitly and echoed back. Results are
# reported for the hierarchical top cell, for every cell checked standalone,
# and for a flattened copy of the top (context-correct count across all cells).
set gds $::env(GDS)
set cell $::env(CELL)
crashbackups stop
puts "RR_TECH [tech name]"
gds read $gds
set all [cellname list allcells]
puts "RR_CELLS [llength $all]"
# A wrong name would make Magic silently create an empty cell: refuse instead.
if {[lsearch -exact $all $cell] < 0} {
    puts "RR_TOP_MISSING $cell"
    quit -noprompt
}
load $cell
select top cell
puts "RR_BBOX [box values]"
drc euclidean on
drc style drc(full)
puts "RR_STYLE [drc list style]"
drc check
drc catchup
puts "RR_HIER_TOTAL [drc list count total]"
foreach {msg boxes} [drc listall why] { puts "RR_HIER_WHY [llength $boxes] :: $msg" }
foreach c $all {
    if {$c == $cell} { continue }
    load $c
    select top cell
    foreach {msg boxes} [drc listall why] { puts "RR_CELL_WHY $c [llength $boxes] :: $msg" }
}
load $cell
select top cell
flatten rr_flat_top
load rr_flat_top
select top cell
drc check
drc catchup
puts "RR_FLAT_TOTAL [drc list count total]"
foreach {msg boxes} [drc listall why] { puts "RR_FLAT_WHY [llength $boxes] :: $msg" }
puts "RR_DONE"
quit -noprompt
