set_units -time ns -capacitance pF
create_clock -name core -period 10
set_clock_uncertainty 0.05 [get_clocks core]
set_max_fanout 64 [current_design]
set_max_capacitance 0.3 [current_design]
set_max_transition 1.0 [current_design]
