puts {RUSTROAD_PUBLIC_CONTRACT_START}
foreach command {read_lef read_def read_verilog read_liberty read_sdc write_def write_verilog write_spef read_db write_db initialize_floorplan global_placement detailed_placement check_placement clock_tree_synthesis global_route detailed_route extract_parasitics report_worst_slack report_tns report_design_area} {
  puts "RUSTROAD_COMMAND $command"
  if {[catch {help $command} message]} { puts "RUSTROAD_UNAVAILABLE $message" }
}
puts {RUSTROAD_PUBLIC_CONTRACT_END}
