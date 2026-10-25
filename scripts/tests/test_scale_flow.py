# SPDX-License-Identifier: Apache-2.0
"""Unit tests for scripts/scale_flow.py status logic (plain unittest; no tools run).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import scale_flow  # noqa: E402

CELLS = {
    "sky130_fd_sc_hd__and2_2": frozenset({"A", "B", "X"}),
    "sky130_fd_sc_hd__or2_2": frozenset({"A", "B", "X"}),
    "sky130_fd_sc_hd__nand2_4": frozenset({"A", "B", "Y"}),
    "sky130_fd_sc_hd__nor2_4": frozenset({"A", "B", "Y"}),
    "gf180mcu_fd_sc_mcu7t5v0__nand2_1": frozenset({"A1", "A2", "ZN"}),
    "gf180mcu_fd_sc_mcu7t5v0__nor2_1": frozenset({"A1", "A2", "ZN"}),
}
NETLIST = """module m(input wire a, input wire b, output wire y, output wire z);
  wire n0;
  sky130_fd_sc_hd__inv_1 g0 (.A(a), .Y(n0));
  sky130_fd_sc_hd__nand2_4 g1 (.A(n0), .B(b), .Y(y));
  sky130_fd_sc_hd__and2_2 \\g2[0]  (.A(a), .B(b), .X(z));
endmodule
"""


def receipt(tmp: Path, report, exit_code=0, timed_out=False) -> dict:
    out = tmp / f"r{len(list(tmp.iterdir()))}.json"
    out.write_text(report if isinstance(report, str) else json.dumps(report))
    return dict(stdout=str(out), exit_code=exit_code, timed_out=timed_out)


class CecVerdict(unittest.TestCase):
    def test_classification(self):
        with tempfile.TemporaryDirectory() as d:
            tmp = Path(d)
            cex = [dict(kind="output", name="y", counterexample=[["a", True]])]
            cases = [
                (dict(verdict="equivalent", mismatches=[]), 0, False, "passed"),
                # RustSys spells the negative verdict with a hyphen (exit 3).
                (dict(verdict="not-equivalent", mismatches=cex), 3, False, "failed"),
                (dict(verdict="not-equivalent"), 3, False, "failed"),
                (dict(verdict="undecided", mismatches=cex), 3, False, "failed"),
                (dict(verdict="undecided", mismatches=[]), 3, False, "unverified"),
                (dict(verdict="error"), 2, False, "unverified"),
                (dict(verdict="initial-state-refinement", mismatches=[]), 0, False, "unverified"),
                (dict(verdict="equivalent", mismatches=[]), 0, True, "unverified"),
                ("not json", 2, False, "unverified"),
            ]
            for report, code, timed_out, want in cases:
                got = scale_flow.cec_verdict(receipt(tmp, report, code, timed_out))
                self.assertEqual(got["status"], want, (report, code, timed_out, got))


class Mutation(unittest.TestCase):
    def test_swaps_first_eligible_master_with_identical_pins(self):
        mutated, what = scale_flow.plant_mutation(NETLIST, CELLS)
        self.assertEqual(what, dict(instance="g1", master="sky130_fd_sc_hd__nand2_4",
                                    mutated_to="sky130_fd_sc_hd__nor2_4"))
        self.assertIn("sky130_fd_sc_hd__nor2_4 g1 (.A(n0), .B(b), .Y(y));", mutated)
        self.assertEqual(mutated.count("\n"), NETLIST.count("\n"))
        mutated, what = scale_flow.plant_mutation(NETLIST, CELLS, skip=1)
        self.assertEqual(what["instance"], "\\g2[0]")
        self.assertEqual(what["mutated_to"], "sky130_fd_sc_hd__or2_2")
        self.assertIsNone(scale_flow.plant_mutation(NETLIST, CELLS, skip=2))

    def test_requires_identical_liberty_pins(self):
        cells = dict(CELLS, **{"sky130_fd_sc_hd__nor2_4": frozenset({"A", "B", "X"})})
        _, what = scale_flow.plant_mutation(NETLIST, cells)
        self.assertEqual(what["instance"], "\\g2[0]")
        gf = "  gf180mcu_fd_sc_mcu7t5v0__nand2_1 u (.A1(a), .A2(b), .ZN(y));\n"
        _, what = scale_flow.plant_mutation(gf, CELLS)
        self.assertEqual(what["mutated_to"], "gf180mcu_fd_sc_mcu7t5v0__nor2_1")


class PortAliases(unittest.TestCase):
    CELL = ("BUF", "A", "X")
    NETLIST = """module top(
  input wire a,
  input wire [1:0] b,
  output wire y,
  output wire [2:0] z,
  output wire w
);
  wire n1;
  wire \\u.q[0] ;
  INV g0 (.A(a), .Y(n1));
  INV g1 (.A(b[0]), .Y(\\u.q[0] ));
  assign y = n1;
  assign z[0] = n1;
  assign z[1] = \\u.q[0] ;
  assign z[2] = b[1];
  assign w = y;
endmodule
"""

    def test_each_output_port_gets_its_own_net(self):
        out, ins = scale_flow.buffer_port_aliases(self.NETLIST, self.CELL)
        self.assertEqual([(x["port"], x["net"]) for x in ins],
                         [("z[0]", "n1"), ("z[2]", "b[1]"), ("w", "y")])
        self.assertIn("  assign y = n1;", out)
        self.assertIn("  assign z[1] = \\u.q[0] ;", out)
        self.assertIn("  BUF rr_portbuf_0 (.A(n1), .X(z[0]));", out)
        self.assertIn("  BUF rr_portbuf_1 (.A(b[1]), .X(z[2]));", out)
        self.assertIn("  BUF rr_portbuf_2 (.A(y), .X(w));", out)
        self.assertNotIn("assign z[0]", out)
        # Idempotent: the result has no aliased output left.
        self.assertEqual(scale_flow.buffer_port_aliases(out, self.CELL), (out, []))

    def test_constant_ports_get_their_own_tie_copy(self):
        text = self.NETLIST.replace("  wire n1;\n", "  wire n1;\n  wire c0;\n  TIEL t0 (.ZN(c0));\n") \
            .replace("assign y = n1;", "assign y = c0;").replace("assign z[0] = n1;", "assign z[0] = c0;")
        out, ins = scale_flow.buffer_port_aliases(text, self.CELL, {"TIEL": {"ZN"}})
        self.assertIn("  TIEL rr_porttie_0 (.ZN(z[0]));", out)
        self.assertEqual(ins[0], dict(instance="rr_porttie_0", net="c0", port="z[0]", cell="TIEL"))
        self.assertIn("  assign y = c0;", out)
        self.assertEqual(scale_flow.port_tie_cells("gf180"), {"gf180mcu_fd_sc_mcu7t5v0__tieh": {"Z"},
                                                              "gf180mcu_fd_sc_mcu7t5v0__tiel": {"ZN"}})
        self.assertEqual(scale_flow.port_tie_cells("sky130"), {"sky130_fd_sc_hd__conb_1": {"HI", "LO"}})

    def test_netlist_without_aliases_is_unchanged(self):
        text = self.NETLIST.replace("  assign z[0] = n1;\n", "").replace("  assign z[2] = b[1];\n", "") \
            .replace("  assign w = y;\n", "")
        self.assertEqual(scale_flow.buffer_port_aliases(text, self.CELL), (text, []))

    def test_profile_buffers_are_checked_identities(self):
        self.assertEqual(scale_flow.port_buffer_cell("sky130"), ("sky130_fd_sc_hd__buf_1", "A", "X"))
        self.assertEqual(scale_flow.port_buffer_cell("gf180"), ("gf180mcu_fd_sc_mcu7t5v0__buf_1", "I", "Z"))


class Receipts(unittest.TestCase):
    def test_stricter_parent_memory_cap_is_preserved(self):
        with tempfile.TemporaryDirectory() as d:
            runner = scale_flow.Runner(Path(d))
            with mock.patch.object(scale_flow.resource, "getrlimit", return_value=(256*1024**2, 512*1024**2)):
                result = runner.run("bounded", ["true"])
            self.assertEqual(result["exit_code"], 0)
            self.assertEqual(result["memory_limit_bytes"], 256*1024**2)

    def test_peak_rss_is_the_tool_and_timeouts_are_unmeasured(self):
        with tempfile.TemporaryDirectory() as d:
            r = scale_flow.Runner(Path(d))
            ok = r.run("true", ["true"])
            self.assertEqual(ok["exit_code"], 0)
            if scale_flow.GNU_TIME:
                self.assertEqual(ok["rss_source"], "gnu-time")
                self.assertLess(ok["peak_rss_kib"], 10 * 1024)
            slow = r.run("sleep", ["sleep", "5"], timeout=1)
            self.assertTrue(slow["timed_out"])
            self.assertIsNone(slow["peak_rss_kib"])
            self.assertEqual(slow["rss_source"], "unmeasured (timed out)")


class StepFailure(unittest.TestCase):
    def test_step_items(self):
        for label, key in (("sky130_a_synth", "synth"), ("gf180_a_floorplan", "floorplan_reproduced"),
                           ("sky130_a_import0", "place"), ("sky130_a_place0", "place"),
                           ("sky130_a_cts1", "place"), ("sky130_a_route0", "route"),
                           ("sky130_a_antenna_eco1", "route"), ("gf180_a_export_gds", "export")):
            self.assertEqual(scale_flow.step_item(label), key)

    def test_failed_build_step_fails_the_pdk_and_names_the_step(self):
        saved = scale_flow.build, scale_flow.verify_tracked

        def failing_build(r, run_dir, m, tag, args, label, facts=None):
            facts["synth"] = dict(cells=5, area=1.0, sequential=0)
            facts["completed"] = ["synth", "floorplan_reproduced", "place"]
            facts["floorplan_reproduced"] = True
            raise scale_flow.StepError(f"{label}_route0", f"{label}_route0: exit 2: BUDGET_EXCEEDED")

        try:
            scale_flow.build = failing_build
            scale_flow.verify_tracked = lambda m, tag: scale_flow.item("passed")
            with tempfile.TemporaryDirectory() as d:
                res = scale_flow.flow(dict(), "sky130", argparse.Namespace(), Path(d))
        finally:
            scale_flow.build, scale_flow.verify_tracked = saved
        st = {k: v["status"] for k, v in res["statuses"].items()}
        self.assertEqual(res["status"], "failed")
        self.assertEqual(res["failed_step"], "sky130_a_route0")
        self.assertEqual(st["route"], "failed")
        self.assertEqual((st["synth"], st["floorplan_reproduced"], st["place"]), ("passed",) * 3)
        for k in ("export", "gds_xor", "lvs_netgen", "cec", "determinism"):
            self.assertEqual(st[k], "unverified", k)
        self.assertEqual(set(st), set(scale_flow.GATING))

    def test_synth_failure_is_attributed_to_synth(self):
        saved = scale_flow.build, scale_flow.verify_tracked

        def failing_build(r, run_dir, m, tag, args, label, facts=None):
            raise scale_flow.StepError(f"{label}_synth", "synth failed")

        try:
            scale_flow.build = failing_build
            scale_flow.verify_tracked = lambda m, tag: scale_flow.item("passed")
            with tempfile.TemporaryDirectory() as d:
                res = scale_flow.flow(dict(), "gf180", argparse.Namespace(), Path(d))
        finally:
            scale_flow.build, scale_flow.verify_tracked = saved
        self.assertEqual(res["status"], "failed")
        self.assertEqual(res["statuses"]["synth"]["status"], "failed")
        self.assertEqual(res["statuses"]["route"]["status"], "unverified")


if __name__ == "__main__":
    unittest.main()
