set_units -time ns -capacitance pF
create_clock -name core -period 93 -waveform {0 46.5} [get_ports clk_i]
set_clock_transition 0.1 [get_clocks core]
set_clock_uncertainty 0.05 [get_clocks core]
set_max_fanout 64 [current_design]
set_max_capacitance 0.3 [current_design]
set_max_transition 1.0 [get_clocks core]
