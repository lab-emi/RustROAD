# SPDX-License-Identifier: Apache-2.0
# Magic antenna check (black-box use).
# usage: env GDS=<gds> CELL=<top> OUTDIR=<dir> magic -dnull -noconsole -rcfile <rc> magic_antenna.tcl
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
extract no all
extract all
antennacheck debug
antennacheck
puts "RR_ANT_DONE"
quit -noprompt
