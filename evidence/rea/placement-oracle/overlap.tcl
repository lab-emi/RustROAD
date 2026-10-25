if {[catch {
  read_lef /oracle/oracle.lef
  read_def /oracle/overlap.def
  check_placement -verbose -report_file_name /oracle/overlap.placement.json
} message]} {
  puts "RUSTROAD_PLACEMENT_REJECTED $message"
  exit 42
}
puts {RUSTROAD_PLACEMENT_ACCEPTED}
exit 0
