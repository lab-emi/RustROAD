# Magic antenna check. env: GDS CELL OUTDIR.
set gds $::env(GDS); set cell $::env(CELL); set od $::env(OUTDIR)
file mkdir $od
cd $od
crashbackups stop
drc off
gds read $gds
if {[lsearch -exact [cellname list allcells] $cell] < 0} {
    puts "MAGIC_TOP_MISSING $cell"
    quit -noprompt
}
load $cell
select top cell
expand
extract do local
extract no all
extract all
antennacheck debug
antennacheck
puts "MAGIC_ANTENNA_DONE"
quit -noprompt
