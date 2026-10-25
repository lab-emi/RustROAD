module lane_smoke(
  input wire clk,
  input wire en,
  input wire [3:0] a,
  input wire [3:0] b,
  output wire [3:0] y,
  output wire [3:0] z,
  output wire high_bit,
  output wire low_bit
);
  wire \q[0] ;
  wire \q[1] ;
  wire \q[2] ;
  wire \q[3] ;
  wire n0;
  wire n1;
  wire n2;
  wire n3;
  wire n4;
  wire n5;
  wire n6;
  wire n7;
  wire n8;
  wire n9;
  wire n10;
  wire n11;
  wire n12;
  wire n13;
  wire n14;
  wire n15;
  wire n16;
  wire n17;
  gf180mcu_fd_sc_mcu7t5v0__and2_1 g0 (.A1(a[0]), .A2(b[0]), .Z(n0));
  gf180mcu_fd_sc_mcu7t5v0__nand2_4 g1 (.A1(a[0]), .A2(b[0]), .ZN(n1));
  gf180mcu_fd_sc_mcu7t5v0__and2_1 g2 (.A1(a[1]), .A2(b[1]), .Z(n2));
  gf180mcu_fd_sc_mcu7t5v0__nand2_4 g3 (.A1(a[1]), .A2(b[1]), .ZN(n3));
  gf180mcu_fd_sc_mcu7t5v0__and2_1 g4 (.A1(a[2]), .A2(b[2]), .Z(n4));
  gf180mcu_fd_sc_mcu7t5v0__nand2_4 g5 (.A1(a[2]), .A2(b[2]), .ZN(n5));
  gf180mcu_fd_sc_mcu7t5v0__and2_1 g6 (.A1(a[3]), .A2(b[3]), .Z(n6));
  gf180mcu_fd_sc_mcu7t5v0__nand2_4 g7 (.A1(a[3]), .A2(b[3]), .ZN(n7));
  gf180mcu_fd_sc_mcu7t5v0__or2_4 g8 (.A1(a[0]), .A2(b[0]), .Z(n8));
  gf180mcu_fd_sc_mcu7t5v0__and3_4 g9 (.A1(en), .A2(n1), .A3(n8), .Z(n9));
  gf180mcu_fd_sc_mcu7t5v0__or2_4 g10 (.A1(a[1]), .A2(b[1]), .Z(n10));
  gf180mcu_fd_sc_mcu7t5v0__and3_4 g11 (.A1(en), .A2(n3), .A3(n10), .Z(n11));
  gf180mcu_fd_sc_mcu7t5v0__or2_4 g12 (.A1(a[2]), .A2(b[2]), .Z(n12));
  gf180mcu_fd_sc_mcu7t5v0__and3_4 g13 (.A1(en), .A2(n5), .A3(n12), .Z(n13));
  gf180mcu_fd_sc_mcu7t5v0__or2_4 g14 (.A1(a[3]), .A2(b[3]), .Z(n14));
  gf180mcu_fd_sc_mcu7t5v0__and3_4 g15 (.A1(en), .A2(n7), .A3(n14), .Z(n15));
  assign n16 = 1'b1;

  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r0 (.D(n16), .CLK(clk), .Q(high_bit));
  assign n17 = 1'b0;

  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r1 (.D(n17), .CLK(clk), .Q(low_bit));
  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r2 (.D(n9), .CLK(clk), .Q(\q[0] ));
  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r3 (.D(n11), .CLK(clk), .Q(\q[1] ));
  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r4 (.D(n13), .CLK(clk), .Q(\q[2] ));
  gf180mcu_fd_sc_mcu7t5v0__dffq_1 r5 (.D(n15), .CLK(clk), .Q(\q[3] ));
  assign y[0] = \q[0] ;
  assign y[1] = \q[1] ;
  assign y[2] = \q[2] ;
  assign y[3] = \q[3] ;
  assign z[0] = n0;
  assign z[1] = n2;
  assign z[2] = n4;
  assign z[3] = n6;
endmodule
