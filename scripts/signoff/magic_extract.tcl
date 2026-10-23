# SPDX-License-Identifier: Apache-2.0
# Magic hierarchical extraction to an LVS netlist (black-box use).
# usage: env GDS=<gds> CELL=<top> OUTDIR=<dir> magic -dnull -noconsole -rcfile <rc> magic_extract.tcl
set gds $::env(GDS)
set cell $::env(CELL)
set od $::env(OUTDIR)
crashbackups stop
drc off
gds read $gds
set all [cellname list allcells]
if {[lsearch -exact $all $cell] < 0} {
    puts "RR_TOP_MISSING $cell"
    quit -noprompt
}
load $cell
select top cell
expand
file mkdir $od
cd $od
extract do local
extract all
ext2spice lvs
ext2spice -o $od/$cell.spice
puts "RR_EXT_DONE $od/$cell.spice"
quit -noprompt
