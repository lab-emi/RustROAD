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
  sky130_fd_sc_hd__and2_2 g0 (.A(a[0]), .B(b[0]), .X(n0));
  sky130_fd_sc_hd__nand2_4 g1 (.A(a[0]), .B(b[0]), .Y(n1));
  sky130_fd_sc_hd__and2_2 g2 (.A(a[1]), .B(b[1]), .X(n2));
  sky130_fd_sc_hd__nand2_4 g3 (.A(a[1]), .B(b[1]), .Y(n3));
  sky130_fd_sc_hd__and2_2 g4 (.A(a[2]), .B(b[2]), .X(n4));
  sky130_fd_sc_hd__nand2_4 g5 (.A(a[2]), .B(b[2]), .Y(n5));
  sky130_fd_sc_hd__and2_2 g6 (.A(a[3]), .B(b[3]), .X(n6));
  sky130_fd_sc_hd__nand2_4 g7 (.A(a[3]), .B(b[3]), .Y(n7));
  sky130_fd_sc_hd__o211a_4 g8 (.A1(a[0]), .A2(b[0]), .B1(en), .C1(n1), .X(n8));
  sky130_fd_sc_hd__o211a_4 g9 (.A1(a[1]), .A2(b[1]), .B1(en), .C1(n3), .X(n9));
  sky130_fd_sc_hd__o211a_4 g10 (.A1(a[2]), .A2(b[2]), .B1(en), .C1(n5), .X(n10));
  sky130_fd_sc_hd__o211a_4 g11 (.A1(a[3]), .A2(b[3]), .B1(en), .C1(n7), .X(n11));
  assign n12 = 1'b1;

  sky130_fd_sc_hd__dfxtp_1 r0 (.D(n12), .CLK(clk), .Q(high_bit));
  assign n13 = 1'b0;

  sky130_fd_sc_hd__dfxtp_1 r1 (.D(n13), .CLK(clk), .Q(low_bit));
  sky130_fd_sc_hd__dfxtp_1 r2 (.D(n8), .CLK(clk), .Q(\q[0] ));
  sky130_fd_sc_hd__dfxtp_1 r3 (.D(n9), .CLK(clk), .Q(\q[1] ));
  sky130_fd_sc_hd__dfxtp_1 r4 (.D(n10), .CLK(clk), .Q(\q[2] ));
  sky130_fd_sc_hd__dfxtp_1 r5 (.D(n11), .CLK(clk), .Q(\q[3] ));
  assign y[0] = \q[0] ;
  assign y[1] = \q[1] ;
  assign y[2] = \q[2] ;
  assign y[3] = \q[3] ;
  assign z[0] = n0;
  assign z[1] = n2;
  assign z[2] = n4;
  assign z[3] = n6;
endmodule
