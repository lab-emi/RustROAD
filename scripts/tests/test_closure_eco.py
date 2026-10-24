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
