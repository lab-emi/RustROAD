module lane_smoke(input clk, input en, input [1:0] a, b, output [1:0] y, z);
  reg [1:0] q, r;
  always @(posedge clk) begin
    q <= a ^ b;
    r <= q & {2{en}};
  end
  assign y = r;
  assign z = a & b;
endmodule
