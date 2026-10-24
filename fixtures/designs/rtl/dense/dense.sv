module lane_smoke(input clk, input en, input [3:0] a, input [3:0] b,
 output [3:0] y, output [3:0] z, output reg high_bit, output reg low_bit);
 reg [3:0] q;
 wire [3:0] mix;
 assign mix=(a^b)&{4{en}};
 always @(posedge clk) begin q<=mix;high_bit<=1'b1;low_bit<=1'b0;end
 assign y=q;
 assign z=a&b;
endmodule
