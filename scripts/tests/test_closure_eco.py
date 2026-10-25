# SPDX-License-Identifier: Apache-2.0
"""Unit tests for scripts/closure_eco.py (plain unittest; no tools run).

Self-authored toy data: a 2-drive buffer family with linear drive data, a
LEF direction table and a small netlist/DEF. The real oracle of the closure
loop is the re-placed, re-routed, re-timed layout (scripts/rtl2gds.py).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

import json
from pathlib import Path
import sys
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import closure_eco as C  # noqa: E402

LEF = {
    "BUF1": {"A": dict(direction="INPUT"), "X": dict(direction="OUTPUT")},
    "BUF4": {"A": dict(direction="INPUT"), "X": dict(direction="OUTPUT")},
    "AND2": {"A": dict(direction="INPUT"), "B": dict(direction="INPUT"), "X": dict(direction="OUTPUT")},
    "DFF": {"D": dict(direction="INPUT"), "CLK": dict(direction="INPUT"), "Q": dict(direction="OUTPUT")},
}


def drive(cap, slew, delay):
    return dict(ss=dict(input_cap_pf=cap, output_pin="X", slew_ns=slew, delay_ns=delay))


PROFILE = dict(
    drive_model=dict(input_slew_ns=0.2, loads_pf=[0.005, 0.05]),
    families=[
        dict(id="BUF", pins=dict(A="input", X="output"), functions=dict(X="A"),
             cells=[dict(name="BUF1", width_dbu=200, drive=drive({"A": 0.002}, [0.2, 2.0], [0.3, 1.5])),
                    dict(name="BUF4", width_dbu=600, drive=drive({"A": 0.008}, [0.1, 0.4], [0.25, 0.4]))]),
        dict(id="AND2", pins=dict(A="input", B="input", X="output"), functions=dict(X="A&B"),
             cells=[dict(name="AND2", width_dbu=300, drive=drive({"A": 0.003, "B": 0.003}, [0.5, 3.0], [0.5, 2.5]))]),
        dict(id="DFF", pins=dict(D="input", CLK="input", Q="output"), functions=dict(Q="IQ"),
             cells=[dict(name="DFF", width_dbu=900, drive=dict(ss=dict(input_cap_pf={"D": 0.003, "CLK": 0.003},
                                                                        output_pin="Q", slew_ns=[0.4, 2.4],
                                                                        delay_ns=[2.0, 3.5])))]),
    ])

# r0/Q drives 12 AND2 inputs spread over two corners of the block and one
# output port through an assign.
SINKS = [f"g{i}" for i in range(12)]
NETLIST = "module t(input wire clk, input wire a, output wire y);\n  wire q;\n" + "".join(
    f"  wire n{i};\n" for i in range(12)) + "  DFF r0 (.D(a), .CLK(clk), .Q(q));\n" + "".join(
    f"  AND2 g{i} (.A(q), .B(a), .X(n{i}));\n" for i in range(12)) + "  assign y = q;\nendmodule\n"
DEF = "COMPONENTS 13 ;\n- r0 DFF + PLACED ( 1000 1000 ) N ;\n" + "".join(
    f"- g{i} AND2 + PLACED ( {1000 + (i % 6) * 400 if i < 6 else 90000 + (i % 6) * 400} 2000 ) N ;\n"
    for i in range(12)) + "END COMPONENTS\nPINS 3 ;\n" \
    "- y + NET y + DIRECTION OUTPUT + USE SIGNAL + PORT + LAYER M3 ( 0 0 ) ( 100 100 ) + FIXED ( 99000 0 ) N ;\n" \
    "- a + NET a + DIRECTION INPUT + USE SIGNAL + PORT + LAYER M3 ( 0 0 ) ( 100 100 ) + FIXED ( 0 0 ) N ;\n" \
    "END PINS\n"


def drv_rows(slew=2.0, cap=0.12):
    rows = [dict(pin="r0/Q", corner="ss", kind="max_transition", net="q", value=slew, limit=1.0, slack=1.0 - slew),
            dict(pin="r0/Q", corner="ss", kind="max_capacitance", net="q", value=cap, limit=0.3, slack=0.3 - cap),
            dict(pin="r0/Q", corner="ss", kind="max_fanout", net="q", value=13, limit=64, slack=51)]
    rows += [dict(pin=f"g{i}/A", corner="ss", kind="max_transition", net="q", value=slew, limit=1.0, slack=1.0 - slew)
             for i in range(12)]
    return rows


class Parse(unittest.TestCase):
    def test_netlist_and_def(self):
        n = C.parse_netlist(NETLIST, LEF)
        self.assertEqual(n.driver["q"], "r0/Q")
        self.assertEqual(len(n.sinks["q"]), 12)
        self.assertEqual(n.port_alias["q"], ["y"])
        self.assertEqual(n.driver["a"], "a")
        comps, ports = C.def_positions(DEF)
        self.assertEqual(comps["g7"], (90400, 2000))
        self.assertEqual(ports["y"], (99050, 50))


class Clusters(unittest.TestCase):
    def test_bisection_is_deterministic_and_bounded(self):
        items = [C.Item(f"g{i}/A", (i * 1000, (i % 3) * 500), 0.004, None) for i in range(40)]
        a = C.bisect_clusters(items, 0.02, 0.0, 16, [10 ** 6])
        b = C.bisect_clusters(list(reversed(items)), 0.02, 0.0, 16, [10 ** 6])
        self.assertEqual([[i.name for i in g] for g in a], [[i.name for i in g] for g in b])
        self.assertTrue(all(sum(i.cap for i in g) <= 0.02 + 1e-12 or len(g) == 1 for g in a))
        self.assertEqual(sorted(i.name for g in a for i in g), sorted(i.name for i in items))
        with self.assertRaises(RuntimeError):
            C.bisect_clusters(items, 0.02, 0.0, 16, [5])


class Buffering(unittest.TestCase):
    def plan(self, rows, setup_critical=False, pins=None):
        return C.plan_buffering(NETLIST, LEF, DEF, PROFILE, rows, pins or {"ss": {}}, "ss", None, 0, 1000,
                                {"DFF": 900, "AND2": 300, "BUF1": 200, "BUF4": 600}, setup_critical)

    def test_overloaded_driver_gets_located_clusters(self):
        res = self.plan(drv_rows())
        self.assertEqual(len(res["records"]), 1)
        rec = res["records"][0]
        self.assertEqual(rec["net"], "q")
        # The two far-apart halves are separate clusters; every sink moves.
        groups = [set(op["sinks"][k]["instance"] for k in range(len(op["sinks"]))) for op in res["ops"]]
        moved = set().union(*groups)
        self.assertEqual(moved, set(SINKS))
        self.assertGreaterEqual(len(res["ops"]), 2)
        for g in groups:
            self.assertTrue(g <= set(SINKS[:6]) or g <= set(SINKS[6:]), g)
        # The output port read through the assign moves with a cluster.
        self.assertEqual(sum(len(op.get("ports", [])) for op in res["ops"]), 1)
        self.assertTrue(all(op["op"] == "buffer_sinks" and op["instance"].startswith("rr_eco_buf_") for op in res["ops"]))
        self.assertLess(rec["driver_load_after_pf"], rec["load_before_pf"])
        self.assertEqual(res, self.plan(drv_rows()))

    def test_input_slew_propagation_and_clean_nets_are_left_alone(self):
        # 2.0 ns observed but the driver model gives < 0.8 ns at 0.005 pF.
        res = self.plan(drv_rows(slew=1.2, cap=0.005))
        self.assertEqual(res["ops"], [])
        self.assertIn("input-slew propagated", res["skipped"]["q"])
        clean = [dict(r, value=0.5, slack=0.5) if r["kind"] == "max_transition" else dict(r, slack=abs(r["slack"]))
                 for r in drv_rows()]
        self.assertEqual(self.plan(clean)["ops"], [])

    def test_setup_guard_decides_who_moves(self):
        # Overloaded DFF (0.12 pF): the driver gets faster (5.8 -> ~2.5 ns
        # estimated), but the critical g0 (< CRITICAL_SLACK_NS) never moves.
        pins = {"ss": {f"g{i}/A": (-0.5 if i == 0 else 3.0) for i in range(12)}}
        res = self.plan(drv_rows(), setup_critical=True, pins=pins)
        rec = res["records"][0]
        self.assertEqual(rec["kept_on_driver"], ["g0/A"])
        self.assertLess(rec["estimated_driver_delay_ns"]["after"], rec["estimated_driver_delay_ns"]["before"])
        # Lightly loaded (0.03 pF) and slow buffers: a buffer would cost the
        # critical sink its slack, so it stays; the rest moves.
        slow = json.loads(json.dumps(PROFILE))
        for c in slow["families"][0]["cells"]:
            c["drive"]["ss"]["delay_ns"] = [3.0, 4.0]
        res = C.plan_buffering(NETLIST, LEF, DEF, slow, drv_rows(slew=1.4, cap=0.03), pins, "ss", None, 0, 1000,
                               {"DFF": 900, "AND2": 300, "BUF1": 200, "BUF4": 600}, True)
        self.assertEqual(res["records"][0]["kept_on_driver"], ["g0/A"])
        moved = {s["instance"] for op in res["ops"] for s in op["sinks"]}
        self.assertNotIn("g0", moved)
        self.assertEqual(moved, set(SINKS[1:]))


class Weights(unittest.TestCase):
    def test_weights_follow_driver_slack(self):
        pins = {"ss": {"r0/Q": -0.4, **{f"g{i}/X": 2.0 for i in range(11)}, "g11/X": -0.2}, "tt": {"r0/Q": 3.0}}
        w = C.net_weights(NETLIST, LEF, pins)
        self.assertEqual(w["schema"], "rustroad.net-weights.v1")
        self.assertEqual(w["weights"]["q"], C.MAX_WEIGHT)
        self.assertGreater(w["weights"]["n11"], 1000)
        self.assertLess(w["weights"]["n11"], C.MAX_WEIGHT)
        self.assertNotIn("n0", w["weights"])
        self.assertTrue(all(isinstance(v, int) and 1000 < v <= C.MAX_WEIGHT for v in w["weights"].values()))


# A weak two-member driver family and a delay cell without a sizing family.
CO_LEF = dict(LEF, WEAK1={"A": dict(direction="INPUT"), "X": dict(direction="OUTPUT")},
              WEAK2={"A": dict(direction="INPUT"), "X": dict(direction="OUTPUT")},
              DLY={"A": dict(direction="INPUT"), "X": dict(direction="OUTPUT")})
CO_PROFILE = json.loads(json.dumps(PROFILE))
CO_PROFILE["families"].append(
    dict(id="WEAK", pins=dict(A="input", X="output"), functions=dict(X="!A"),
         cells=[dict(name="WEAK1", width_dbu=300, drive=drive({"A": 0.003}, [0.7, 4.0], [0.6, 3.0])),
                dict(name="WEAK2", width_dbu=500, drive=drive({"A": 0.005}, [0.5, 2.0], [0.5, 1.6]))]))
CO_WIDTHS = {"DFF": 900, "AND2": 300, "BUF1": 200, "BUF4": 600, "WEAK1": 300, "WEAK2": 500, "DLY": 400}


class CoSizing(unittest.TestCase):
    def prof(self):
        return C.Profile(CO_PROFILE, "ss", None)

    def test_fit_buffers_respects_budget_target_and_smallest(self):
        bufs = self.prof().buffers
        self.assertIsNone(C.fit_buffers(bufs, [0.04], 0.6, 0.0, 0.003))  # BUF1 too slow, BUF4 over budget
        self.assertEqual([b.name for b in C.fit_buffers(bufs, [0.04], 0.6, 0.0, 0.01)], ["BUF4"])
        self.assertEqual([b.name for b in C.fit_buffers(bufs, [0.006], 0.6, 0.0, 0.01, smallest=True)], ["BUF1"])

    def test_driver_model_headroom_and_hold_capacity(self):
        prof = self.prof()
        m = C.driver_model(prof, "WEAK1", 1.0, None, 0.001, None, 0.01, None)
        self.assertEqual([x[0] for x in m.members], ["WEAK1", "WEAK2"])
        self.assertFalse(m.headroom)
        self.assertAlmostEqual(m.target, C.DRIVER_TARGET)
        # Best member above DRIVER_TARGET even at the smallest arrangement load:
        # a target between that floor and the limit.
        slow = json.loads(json.dumps(CO_PROFILE))
        slow["families"][-1]["cells"][0]["drive"]["ss"]["slew_ns"] = [0.95, 4.0]
        slow["families"][-1]["cells"][1]["drive"]["ss"]["slew_ns"] = [0.9, 2.0]
        m = C.driver_model(C.Profile(slow, "ss", None), "WEAK1", 1.0, None, 0.001, None, 0.01, None)
        self.assertTrue(m.headroom)
        self.assertTrue(C.DRIVER_TARGET < m.target < 1.0)
        # Observed input-slew degradation lowers the target (bounded).
        m2 = C.driver_model(prof, "WEAK1", 1.0, None, 0.001, 5.0, 0.01, None)
        self.assertAlmostEqual(m2.degradation, C.MAX_SLEW_DEGRADATION)
        # Delay cells: conservative capacity through the characterized point.
        self.assertAlmostEqual(C.hold_capacity({"DLY": (0.005, 0.5)}, "DLY", 0.8), 0.008)
        h = C.driver_model(prof, "DLY", 1.0, None, 0.001, 1.6, 0.02, {"DLY": (0.005, 0.5)})
        self.assertEqual(h.members, [("DLY", 0.008)])
        self.assertIsNone(C.driver_model(prof, "DLY", 1.0, None, 0.001, 1.6, 0.02, None))

    def test_existing_buffer_moves_next_to_its_weak_driver(self):
        # WEAK1 w drives one ECO buffer 90 um away; the buffer drives 6 flops.
        net = ("module t(input wire a, input wire clk);\n  wire n0, n1;\n  WEAK1 w (.A(a), .X(n0));\n"
               "  BUF4 rr_eco_buf_0 (.A(n0), .X(n1));\n" +
               "".join(f"  DFF r{i} (.D(n1), .CLK(clk), .Q());\n" for i in range(6)) + "endmodule\n")
        deff = ("COMPONENTS 8 ;\n- w WEAK1 + PLACED ( 1000 1000 ) N ;\n- rr_eco_buf_0 BUF4 + PLACED ( 90000 1000 ) N ;\n" +
                "".join(f"- r{i} DFF + PLACED ( {91000 + 1000 * i} 1000 ) N ;\n" for i in range(6)) + "END COMPONENTS\n")
        rows = [dict(pin="w/X", corner="ss", kind="max_transition", net="n0", value=1.6, limit=1.0, slack=-0.6),
                dict(pin="rr_eco_buf_0/A", corner="ss", kind="max_transition", net="n0", value=1.6, limit=1.0, slack=-0.6),
                dict(pin="w/X", corner="ss", kind="max_capacitance", net="n0", value=0.025, limit=0.3, slack=0.275),
                dict(pin="rr_eco_buf_0/X", corner="ss", kind="max_capacitance", net="n1", value=0.02, limit=0.3,
                     slack=0.28)]
        pins = {"ss": {"rr_eco_buf_0/A": 5.0}}
        res = C.plan_buffering(net, CO_LEF, deff, CO_PROFILE, rows, pins, "ss", None, 1, 1000, CO_WIDTHS, False)
        rec = res["records"][0]
        self.assertEqual(rec["why"], "drv:rebalance ECO buffers")
        self.assertEqual(rec["arrangement"], "driver")
        # The driver grows (WEAK1 cannot take a buffer input plus wire), the
        # buffer is seeded next to it and its net gets the placement weight.
        self.assertEqual(res["resizes"]["w"][0], "WEAK2")
        self.assertIn("rr_eco_buf_0", res["positions"])
        self.assertLess(abs(res["positions"]["rr_eco_buf_0"][0] - 1000), 1000)
        self.assertEqual(res["weights"], {"n0": C.DRIVER_NET_WEIGHT})
        self.assertIn("rr_eco_buf_0", res["touched"])
        self.assertEqual(res, C.plan_buffering(net, CO_LEF, deff, CO_PROFILE, rows, pins, "ss", None, 1, 1000,
                                               CO_WIDTHS, False))
        # Setup room too small for the buffer's extra trunk delay: nothing moves.
        res = C.plan_buffering(net, CO_LEF, deff, CO_PROFILE, rows, {"ss": {"rr_eco_buf_0/A": 0.05}}, "ss", None, 1,
                               1000, CO_WIDTHS, False)
        self.assertEqual(res["positions"], {})
        self.assertIn("setup guard", res["skipped"]["n0"])

    def test_free_spot_uses_interval_capacity(self):
        # One row, a tap at 2000 splits it into [0, 2000) and [2300, 10000).
        deff = ("ROW r0 core 0 0 N DO 100 BY 1 STEP 100 0 ;\nROW r1 core 0 1000 FS DO 100 BY 1 STEP 100 0 ;\n"
                "COMPONENTS 4 ;\n- t x__filltie + PLACED ( 2000 0 ) N ;\n- a AND2 + PLACED ( 0 0 ) N ;\n"
                "- f x__fill_8 + PLACED ( 300 0 ) N ;\n- b BIG + PLACED ( 600 0 ) N ;\nEND COMPONENTS\n")
        occ = C.Occupancy(deff, {"AND2": 300, "x__fill_8": 300, "x__filltie": 300, "BIG": 1300})
        budget = [10 ** 6]
        # [0, 2000) holds 1600 of cells (the filler is free): 400 left there.
        self.assertEqual(occ.spot((500, 0), 300, "n1", budget), (300, 0))
        # Full now (100 left): the next one goes to the nearest other interval.
        x, y = occ.spot((500, 0), 300, "n2", budget)
        self.assertTrue((x, y) == (300, 1000) or (x >= 2300 and y == 0), (x, y))
        # A moved cell frees its own capacity first.
        occ.release("a")
        self.assertEqual(occ.spot((150, 0), 300, "a", budget), (0, 0))

    def test_hold_chain_gets_an_isolation_buffer(self):
        net = ("module t(input wire a, input wire clk);\n  wire n1;\n  DLY rr_eco_hold_0 (.A(a), .X(rr_eco_hold_0_n));\n"
               "  BUF4 rr_eco_buf_0 (.A(rr_eco_hold_0_n), .X(n1));\n  DFF r0 (.D(n1), .CLK(clk), .Q());\nendmodule\n")
        deff = ("COMPONENTS 2 ;\n- rr_eco_buf_0 BUF4 + PLACED ( 50000 1000 ) N ;\n- r0 DFF + PLACED ( 52000 1000 ) N ;\n"
                "END COMPONENTS\n")
        point = dict(name="rr_eco_buf_0/A", instances=["rr_eco_hold_0"], setup_slack_ns={"ss": 5.0},
                     added_setup_ns={"ss": 1.0})
        res = C.plan_hold_isolation(net, CO_LEF, deff, CO_PROFILE, [point], {"DLY": (0.005, 0.5)}, 1.0, 3, 1000,
                                    CO_WIDTHS, 1e-7, 0.05, corners=("ss",))
        self.assertEqual(len(res["ops"]), 1)
        op = res["ops"][0]
        self.assertEqual((op["op"], op["cell"], op["instance"]), ("buffer_sinks", "BUF1", "rr_eco_buf_3"))
        self.assertEqual(op["sinks"], [dict(instance="rr_eco_buf_0", pin="A")])
        self.assertEqual(set(res["positions"]), {"rr_eco_buf_3", "rr_eco_hold_0"})
        self.assertEqual(res["weights"], {"rr_eco_hold_0_n": C.DRIVER_NET_WEIGHT})
        self.assertEqual(res["next_seq"], 4)
        # No setup room for the buffer: left alone and reported.
        tight = dict(point, setup_slack_ns={"ss": 1.3})
        res = C.plan_hold_isolation(net, CO_LEF, deff, CO_PROFILE, [tight], {"DLY": (0.005, 0.5)}, 1.0, 3, 1000,
                                    CO_WIDTHS, 1e-7, 0.05, corners=("ss",))
        self.assertEqual(res["ops"], [])
        self.assertIn("setup room", res["skipped"]["rr_eco_buf_0/A"])
        # A light load (a flop pin) needs no isolation.
        light = net.replace("BUF4 rr_eco_buf_0 (.A(rr_eco_hold_0_n), .X(n1))", "DFF r9 (.D(rr_eco_hold_0_n), .CLK(clk), .Q())")
        res = C.plan_hold_isolation(light, CO_LEF, deff.replace("rr_eco_buf_0 BUF4", "r9 DFF"), CO_PROFILE,
                                    [dict(point, name="r9/D")], {"DLY": (0.005, 0.5)}, 1.0, 3, 1000, CO_WIDTHS, 1e-7,
                                    0.05, corners=("ss",))
        self.assertEqual(res["ops"], [])


if __name__ == "__main__":
    unittest.main()


class Consumption(unittest.TestCase):
    def test_spent_slack_follows_the_combinational_cone(self):
        n = C.parse_netlist(NETLIST, LEF)
        pl = C.Planner(n, {}, {}, C.Profile(PROFILE, "ss", None), {}, 1000, 0)
        pl.sequential = {"DFF"}
        pl.consume("g3/A", 0.4)
        self.assertEqual(pl.consumed["g3/A"], 0.4)
        # g3 drives n3, which has no sink: nothing else is charged; r0 is
        # sequential, so charging its D pin stops there.
        pl.consume("r0/D", 0.7)
        self.assertEqual(set(pl.consumed), {"g3/A", "r0/D"})
        # A larger charge replaces a smaller one, never the other way round.
        pl.consume("g3/A", 0.2)
        self.assertEqual(pl.consumed["g3/A"], 0.4)
        pl.walk_budget = [0]
        with self.assertRaises(RuntimeError):
            pl.consume("g4/A", 1.0)
