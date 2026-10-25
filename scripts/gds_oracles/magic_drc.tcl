# Magic batch DRC, full style. env: GDS CELL. Fails closed on a missing cell:
# Magic would otherwise silently create an empty cell and report zero errors.
set gds $::env(GDS)
set cell $::env(CELL)
puts "MAGIC_VERSION [magic::version]"
puts "TECH [tech name]"
gds read $gds
if {[lsearch -exact [cellname list allcells] $cell] < 0} {
    puts "CELL_MISSING $cell"
    quit -noprompt
}
puts "CELL_PRESENT $cell"
load $cell
select top cell
drc euclidean on
drc style drc(full)
drc check
drc catchup
puts "DRC_TOTAL [drc list count total]"
foreach {msg boxes} [drc listall why] { puts "DRC_WHY [llength $boxes] :: $msg" }
puts "DRC_DONE"
quit -noprompt
