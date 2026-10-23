# Magic hierarchical extraction to an LVS SPICE netlist. env: GDS CELL OUTDIR.
set gds $::env(GDS); set cell $::env(CELL); set od $::env(OUTDIR)
file mkdir $od
cd $od
crashbackups stop
drc off
gds read $gds
if {[lsearch -exact [cellname list allcells] $cell] < 0} {
    puts "CELL_MISSING $cell"
    quit -noprompt
}
load $cell
select top cell
expand
extract do local
extract all
ext2spice lvs
ext2spice -o ${cell}.spice
puts "EXT_DONE ${od}/${cell}.spice"
quit -noprompt
