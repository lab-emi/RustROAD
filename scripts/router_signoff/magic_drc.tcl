# Magic batch DRC, full style, Euclidean. env: GDS CELL. Prints parseable lines.
set gds $::env(GDS)
set cell $::env(CELL)
puts "MAGIC_VERSION [magic::version]"
puts "MAGIC_TECH [tech name]"
crashbackups stop
gds read $gds
if {[lsearch -exact [cellname list allcells] $cell] < 0} {
    puts "MAGIC_TOP_MISSING $cell"
    quit -noprompt
}
load $cell
select top cell
puts "MAGIC_BBOX [box values]"
drc euclidean on
drc style drc(full)
puts "MAGIC_DRC_STYLE [drc list style]"
drc check
drc catchup
puts "MAGIC_DRC_TOTAL [drc list count total]"
foreach {msg boxes} [drc listall why] {
    puts "MAGIC_DRC_WHY [llength $boxes] :: $msg"
    foreach b $boxes { puts "MAGIC_DRC_BOX $msg :: $b" }
}
puts "MAGIC_DRC_DONE"
quit -noprompt
