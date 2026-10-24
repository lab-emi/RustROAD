# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the RTL-to-GDS gate's pure helpers (no tools, no PDK files).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(SCRIPTS / "signoff"))

import rtl2gds_binding as B  # noqa: E402
import rtl2gds_controls as C  # noqa: E402
import rtl2gds_design as D  # noqa: E402
import rtl2gds_logic as L  # noqa: E402
import rtl2gds_timing as T  # noqa: E402

PORTS_JSON = {"ports": {
    "clk": {"direction": "input", "bits": [2]},
    "en": {"direction": "input", "bits": [3]},
    "a": {"direction": "input", "bits": [4, 5]},
    "b": {"direction": "input", "bits": [6, 7]},
    "y": {"direction": "output", "bits": [8, 9]},
    "z": {"direction": "output", "bits": [10, 11]},
}}
PIPELINE_TAIL = ("set_input_delay 0.2 -clock core [get_ports {en a b}]\n"
                 "set_output_delay 0.2 -clock core [get_ports {y z}]\n"
                 "set_input_transition 0.1 [get_ports {clk en a b}]\n"
                 "set_load 0.01 [get_ports {y z}]\n"
                 "set_propagated_clock [get_clocks core]\n")


class Statuses(unittest.TestCase):
    def test_worst_and_overall(self):
        self.assertEqual(D.worst([]), "unverified")
        self.assertEqual(D.worst(["passed", "missing"]), "unverified")
        self.assertEqual(D.worst(["passed", "unverified", "failed"]), "failed")
        per = {"sky130": {"a": D.item("passed"), "b": D.item("passed")}}
        v = D.overall(per, ["a", "b"], {"tools": D.item("passed")}, ["sky130"])
        self.assertEqual((v["status"], v["exit_code"]), ("passed", 0))
        # A required PDK without results, or a gating item never produced, is never a pass.
        v = D.overall(per, ["a", "b"], {}, ["sky130", "gf180"])
        self.assertEqual((v["status"], v["exit_code"]), ("unverified", 3))
        v = D.overall(per, ["a", "b", "c"], {}, ["sky130"])
        self.assertEqual(v["unverified_items"], ["sky130:c"])
        per["sky130"]["b"] = D.item("failed", "x")
        v = D.overall(per, ["a", "b"], {"tools": D.item("unverified")}, ["sky130"])
        self.assertEqual((v["status"], v["exit_code"], v["failed_items"]), ("failed", 1, ["sky130:b"]))
        with self.assertRaises(ValueError):
            D.item("ok")


class SdcAndPorts(unittest.TestCase):
    def test_ports_and_rendering_reproduce_the_pipeline_sdc(self):
        ports = D.ports_from_yosys_json(PORTS_JSON)
        self.assertEqual([p["name"] for p in ports], ["clk", "en", "a", "b", "y", "z"])
        self.assertEqual(ports[2]["bits"], ["a[0]", "a[1]"])
        self.assertEqual((ports[2]["msb"], ports[2]["lsb"], ports[1]["vector"]), (1, 0, False))
        tmpl = (SCRIPTS.parent / "fixtures/designs/postroute-io.sdc.tmpl").read_text()
        out = D.render_sdc("HEAD\n", tmpl, ports, "clk", "core")
        self.assertEqual(out, "HEAD\n" + PIPELINE_TAIL)
        with self.assertRaises(D.GateError):
            D.render_sdc("", tmpl, ports, "clock", "core")

    def test_frozen_comparison_names_every_change(self):
        fps = {"a": {"sha256": "1", "path": "x"}, "b": {"sha256": "2", "path": "y"}}
        self.assertEqual(D.compare_frozen({"a": "1", "b": "2"}, fps)["status"], "passed")
        r = D.compare_frozen({"a": "1", "b": "3", "c": "4"}, fps)
        self.assertEqual((r["status"], r["changed"]), ("failed", ["b", "c"]))


class Binding(unittest.TestCase):
    def test_bind_verify_and_tamper(self):
        with tempfile.TemporaryDirectory(dir=SCRIPTS.parent / ".work") if (SCRIPTS.parent / ".work").is_dir() \
                else tempfile.TemporaryDirectory() as d:
            a, b = Path(d) / "a.gds", Path(d) / "b.def"
            a.write_bytes(b"abc")
            b.write_bytes(b"def")
            rev = dict(head="h", dirty_digest="d", rustroad_sha256="r")
            rec = B.bind({"gds": a, "def": b}, rev)
            self.assertEqual(B.verify(rec, rev)["status"], "passed")
            self.assertEqual(B.verify(rec, dict(rev, head="h2"))["status"], "failed")
            a.write_bytes(b"abd")
            v = B.verify(rec, rev)
            self.assertEqual((v["status"], [c["artifact"] for c in v["changed"]]), ("failed", ["gds"]))
            a.write_bytes(b"abc")
            forged = json.loads(json.dumps(rec))
            forged["artifacts"]["def"]["sha256"] = "0" * 64
            self.assertIn("digest", B.verify(forged, rev)["reason"])

    def test_compare_runs_normalises_only_the_openrcx_date(self):
        with tempfile.TemporaryDirectory() as d:
            x, y = Path(d) / "x.spef", Path(d) / "y.spef"
            x.write_bytes(b'*SPEF\n*DATE "1"\n*D_NET a 1\n')
            y.write_bytes(b'*SPEF\n*DATE "2"\n*D_NET a 1\n')
            r = B.compare_runs({"s": x}, {"s": y}, {"s": "openrcx_spef"})
            self.assertEqual(r["status"], "passed")
            self.assertTrue(r["artifacts"]["s"]["normalization"])
            self.assertEqual(B.compare_runs({"s": x}, {"s": y}, {})["status"], "failed")
            self.assertEqual(B.compare_runs({"s": x}, {}, {})["status"], "failed")


class Logic(unittest.TestCase):
    PINS = {"FILL": {"VDD": dict(direction="INOUT", use="POWER"), "VSS": dict(direction="INOUT", use="GROUND")},
            "DIODE": {"D": dict(direction="INPUT", use="SIGNAL"), "VSS": dict(direction="INOUT", use="GROUND")},
            "AND2": {"A": dict(direction="INPUT", use="SIGNAL"), "B": dict(direction="INPUT", use="SIGNAL"),
                     "X": dict(direction="OUTPUT", use="SIGNAL"), "VPWR": dict(direction="INOUT", use="POWER"),
                     "VGND": dict(direction="INOUT", use="GROUND")}}
    NET = ("module t (\n  input wire a,\n  output wire x,\n  inout wire VDD,\n  inout wire VSS\n);\n"
           "  AND2 g0 (.A(a), .B(a), .X(x), .VGND(VSS), .VPWR(VDD));\n"
           "  FILL f0 (.VDD(VDD), .VSS(VSS));\n  DIODE d0 (.D(a), .VSS(VSS));\nendmodule\n")

    def test_powered_views(self):
        text, log = L.physical_only_view(self.NET, ["FILL", "DIODE"], self.PINS)
        self.assertNotIn("FILL", text)
        self.assertNotIn("DIODE", text)
        self.assertEqual(log["by_master"], {"DIODE": 1, "FILL": 1})
        # A policy glob that matches a cell with an output pin is refused, not filtered.
        with self.assertRaises(D.GateError):
            L.physical_only_view(self.NET, ["AND*"], self.PINS)
        up, log = L.unpowered_view(text, self.PINS, ["VDD", "VSS"])
        self.assertIn("AND2 g0 (.A(a), .B(a), .X(x));", up)
        self.assertIn("output wire x\n);", up)
        self.assertNotIn("VDD", up)
        self.assertEqual(log["supply_connections_removed"], 2)

    def test_bad_rtl_wrapper_and_testbench_are_generic(self):
        ports = D.ports_from_yosys_json(PORTS_JSON)
        with tempfile.TemporaryDirectory() as d:
            src = Path(d) / "top.v"
            src.write_text("module lane_smoke(input clk); endmodule\nmodule other; endmodule\n")
            files, info = L.bad_rtl([src], "lane_smoke", ports, Path(d) / "bad")
            self.assertIn("module lane_smoke__rr_orig(", files[0].read_text())
            wrapper = files[-1].read_text()
            self.assertIn("assign y = rr_mutant_y ^ 2'd1;", wrapper)
            self.assertIn("y[0]", info["mutation"])
            src.write_text("module x; endmodule\n")
            with self.assertRaises(D.GateError):
                L.bad_rtl([src], "lane_smoke", ports, Path(d) / "bad2")
        tb = L.xorshift_tb("lane_smoke", ports, "clk", 16, 0x1234)
        for needle in ("lane_smoke gold (", "gate dut (", "gold_y !== gate_y", "gold_z !== gate_z",
                       "a[1:0] = s[1:0]", "RR_COSIM_DONE", "clk = 1'b1", "repeat (16)",
                       "reg [63:0] s = 64'h0000000000001234;", "integer errors = 0;"):
            self.assertIn(needle, tb)
        self.assertNotIn("clk[", tb)
        # No assignment before the first delay (Verilator 5.032 constant-folds those).
        body = tb.split("initial begin", 1)[1].split("#1", 1)[0]
        self.assertNotIn("errors =", body)
        self.assertEqual(L.expected_counts(ports, "clk", 16), dict(cycles=16, checks=64, time=48))
        self.assertEqual(L.expected_counts(ports, None, 16), dict(cycles=16, checks=32, time=16))

    def test_lef_pins(self):
        with tempfile.TemporaryDirectory() as d:
            lef = Path(d) / "c.lef"
            lef.write_text("MACRO X\n PIN A\n  DIRECTION INPUT ;\n  USE SIGNAL ;\n END A\n"
                           " PIN Y\n  DIRECTION OUTPUT TRISTATE ;\n END Y\nEND X\n")
            self.assertEqual(L.lef_pins(lef), {"X": {"A": dict(direction="INPUT", use="SIGNAL"),
                                                     "Y": dict(direction="OUTPUT TRISTATE", use=None)}})


OPENSTA = """ time 1ns
RR_ANNOT
Found 0 unannotated drivers.
Found 0 partially unannotated drivers.
RR_SETUP
r3/D (dffq_1)   10.638705253601    5.785943984985    4.852761268616 (MET)
y[0] (output)                           9.750000000000    4.005033969879    5.744966030121 (MET)
RR_HOLD
r3/D (dffq_1)    1.571764230728    1.991392254829    0.419628024101 (MET)
y[0] (output)                          -0.149999991059    3.578323364258    3.728322982788 (MET)
RR_MPW
r3/CLK (high)                           1.473999977112    4.965814590454    3.491814374924 (MET)
r3/CLK (low)                            1.253999948502    5.031849861145    3.777849674225 (MET)
RR_PER
r3/CLK                                 10.000000000000    3.101000070572    6.899000167847 (MET)
RR_SLEW
y[0]                                    1.000000000000    0.928796589375    0.071203432977 (MET)
RR_CAP
a[0]                                    0.299999982119    0.023284759372    0.276715219021 (MET)
RR_FAN
a[0]                                    2.000000000000    2.000000000000    0.000000000000 (MET)
RR_END
"""


class Timing(unittest.TestCase):
    def test_opensta_parser_and_parity(self):
        o = T.parse_opensta(OPENSTA)
        self.assertEqual(o["setup"]["r3/D"], 4.852761268616)
        self.assertEqual(o["hold"]["y[0]"], 3.728322982788)
        self.assertEqual(set(o["mpw"]), {"r3/CLK:high", "r3/CLK:low"})
        self.assertEqual(o["period"]["r3/CLK"]["min_period"], 3.101000070572)
        self.assertEqual(o["max_fanout"]["a[0]"]["slack"], 0.0)
        rust = {"r3/D": dict(setup=4.8530, hold=0.4196), "y[0]": dict(setup=5.744966, hold=3.728323)}
        p = T.endpoint_parity(rust, o, 1e-3)
        self.assertEqual(p["status"], "passed")
        rust["r3/D"]["setup"] = 4.8540
        self.assertEqual(T.endpoint_parity(rust, o, 1e-3)["status"], "failed")
        del rust["y[0]"]
        self.assertIn("y[0]", T.endpoint_parity(rust, o, 1e-3)["missing"])
        with self.assertRaises(D.GateError):
            T.parse_opensta(OPENSTA.replace("RR_PER\n", ""))
        with self.assertRaises(D.GateError):
            T.parse_opensta(OPENSTA.replace("Found 0 unannotated", "Found x unannotated"))

    def test_spef_binding(self):
        with tempfile.TemporaryDirectory() as d:
            s = Path(d) / "a.spef"
            s.write_text("x")
            good = D.sha256_file(s)
            self.assertEqual(T.spef_binding({"tt": s}, {"tt": good})["status"], "passed")
            s.write_text("y")
            self.assertEqual(T.spef_binding({"tt": s}, {"tt": good})["status"], "failed")


class Controls(unittest.TestCase):
    def test_remove_chain_and_two_terminal_via(self):
        text = ("module t (input wire a, output wire y);\n  wire n2;\n  wire rr_eco_hold_0_n;\n  wire rr_eco_hold_1_n;\n"
                "  AND2 g2 (.A(a), .B(a), .X(n2));\n  BUF rr_eco_hold_0 (.A(n2), .X(rr_eco_hold_0_n));\n"
                "  BUF rr_eco_hold_1 (.A(rr_eco_hold_0_n), .X(rr_eco_hold_1_n));\n"
                "  DFF r0 (.CLK(a), .D(rr_eco_hold_1_n), .Q(y));\nendmodule\n")
        out, removed = C.remove_chain(text, "r0/D", "rr_eco_hold_", {"BUF": ("A", "X")})
        self.assertEqual(removed, ["rr_eco_hold_1", "rr_eco_hold_0"])
        self.assertIn("DFF r0 (.CLK(a), .D(n2), .Q(y));", out)
        self.assertNotIn("rr_eco_hold", out)
        with self.assertRaises(D.GateError):
            C.remove_chain(out, "r0/D", "rr_eco_hold_", {"BUF": ("A", "X")})
        definition = ("NETS 3 ;\n- clk ( r0 CLK ) ( PIN clk ) + USE CLOCK\n  + ROUTED met1 ( 0 0 ) ( 10 0 ) M1M2_PR ;\n"
                      "- n1 ( g0 X ) ( r0 D ) ( r1 D ) + USE SIGNAL\n  + ROUTED met1 ( 0 0 ) ( 10 0 ) M1M2_PR ;\n"
                      "- z ( g1 X ) ( PIN z ) + USE SIGNAL\n  + ROUTED met1 ( 20 40 ) ( 20 60 )\n"
                      "  NEW met2 ( 20 60 ) M1M2_PR ;\nEND NETS\n")
        self.assertEqual(C.two_terminal_via("x\n" + definition, {"M1M2_PR"}, 2, {"clk"}), ("z", "M1M2_PR", 10, 30))

    def test_scaled_spef_resistance(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d) / "a.spef", Path(d) / "b.spef"
            src.write_text("*D_NET n 1.0\n*CONN\n*I a:X O\n*CAP\n1 n:1 0.5\n*RES\n1 n:1 a:X 2.5\n*END\n")
            self.assertEqual(C.scale_spef_r(src, dst, 1000.0), 1)
            self.assertIn("1 n:1 a:X 2500\n", dst.read_text())
            self.assertIn("1 n:1 0.5\n", dst.read_text())


if __name__ == "__main__":
    unittest.main()
