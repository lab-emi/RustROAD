# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the RTL-to-GDS gate's pure helpers (no tools, no PDK files).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

import json
import re
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
    def test_not_applicable_is_explicit_and_restricted(self):
        self.assertEqual(D.worst(["not_applicable", "passed"]), "passed")
        self.assertEqual(D.worst(["not_applicable", "not_applicable"]), "not_applicable")
        self.assertEqual(D.worst(["not_applicable", "unverified"]), "unverified")
        per = {"sky130": {"mpw_min_period": D.item("not_applicable", "combinational"), "drv": D.item("passed")}}
        v = D.overall(per, ["mpw_min_period", "drv"], {}, ["sky130"])
        self.assertEqual((v["status"], v["not_applicable_items"]), ("passed", ["sky130:mpw_min_period"]))
        # Only the items NOT_APPLICABLE names may be not_applicable.
        per["sky130"]["drv"] = D.item("not_applicable", "x")
        v = D.overall(per, ["mpw_min_period", "drv"], {}, ["sky130"])
        self.assertEqual((v["status"], v["unverified_items"]), ("unverified", ["sky130:drv"]))

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


def work_tmp():
    work = SCRIPTS.parent / ".work"
    return tempfile.TemporaryDirectory(dir=work) if work.is_dir() else tempfile.TemporaryDirectory()


class ManifestFreeze(unittest.TestCase):
    """Round-1 verifier F2: a manifest cannot loosen the fixed thresholds, and
    only the git-tracked manifest unchanged at HEAD can yield a pass."""

    def test_budget_floors(self):
        m = D.load_manifest(D.ROOT / "fixtures/designs/pipeline.json")
        self.assertEqual(D.budget_floors(m["budgets"], m["verification"])["status"], "passed")
        tight = dict(m["budgets"], hold_margin_ns=0.08, hold_eco_target_margin_ns=0.1, setup_margin_ns=0.01,
                     parity_ns=0.0005)
        self.assertEqual(D.budget_floors(tight, m["verification"])["status"], "passed")
        # The ECO hold target may add a guard band, never undercut the gate margin.
        r = D.budget_floors(dict(m["budgets"], hold_eco_target_margin_ns=0.04), m["verification"])
        self.assertEqual(r["status"], "failed")
        self.assertIn("hold_eco_target_margin_ns", r["reason"])
        for key, value in (("parity_ns", 0.003), ("hold_margin_ns", 0.049), ("setup_margin_ns", -0.01),
                           ("parity_ns", float("nan")), ("hold_margin_ns", "0.05"), ("parity_ns", None),
                           ("hold_margin_ns", True)):
            r = D.budget_floors(dict(m["budgets"], **{key: value}), m["verification"])
            self.assertEqual(r["status"], "failed", (key, value))
            self.assertIn(f"budgets.{key}", r["reason"])
        r = D.budget_floors(m["budgets"], dict(m["verification"], cosim_vectors=16))
        self.assertEqual((r["status"], r["floors"]["verification.cosim_vectors"]["status"]), ("failed", "failed"))
        missing = {k: v for k, v in m["budgets"].items() if k != "parity_ns"}
        self.assertEqual(D.budget_floors(missing, m["verification"])["status"], "failed")

    def test_manifest_provenance(self):
        with work_tmp() as d:
            repo = Path(d) / "repo"
            (repo / "fixtures").mkdir(parents=True)
            design = repo / "fixtures" / "design.json"
            design.write_text('{"a": 1}\n')
            git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@example.invalid"]
            import subprocess
            subprocess.run([*git, "init", "-q"], check=True)
            # Untracked (never committed) -> unverified, never passed.
            self.assertEqual(D.manifest_provenance(design, repo)["status"], "unverified")
            subprocess.run([*git, "add", "fixtures/design.json"], check=True)
            subprocess.run([*git, "commit", "-q", "-m", "x"], check=True)
            r = D.manifest_provenance(design, repo)
            self.assertEqual(r["status"], "passed", r)
            self.assertEqual(r["head_blob"], r["worktree_blob"])
            design.write_text('{"a": 2}\n')
            self.assertIn("differs from HEAD", D.manifest_provenance(design, repo)["reason"])
            subprocess.run([*git, "add", "fixtures/design.json"], check=True)  # staged edit is not HEAD either
            self.assertEqual(D.manifest_provenance(design, repo)["status"], "unverified")
            copy = repo / "copy.json"
            copy.write_text('{"a": 1}\n')
            self.assertIn("not tracked", D.manifest_provenance(copy, repo)["reason"])
            outside = Path(d) / "outside.json"
            outside.write_text("{}")
            self.assertIn("outside the repository", D.manifest_provenance(outside, repo)["reason"])

    def test_tool_pin_file_must_be_the_frozen_one(self):
        m = D.load_manifest(D.ROOT / "fixtures/designs/pipeline.json")
        pins = D.resolve(m["tools"]["path"])
        self.assertEqual(D.tool_pins_frozen(pins, m)["status"], "passed")
        with work_tmp() as d:
            other = Path(d) / "tools.json"
            data = json.loads(pins.read_text())
            data["tools"]["ruststa"]["sha256"] = "0" * 64
            other.write_text(json.dumps(data))
            self.assertEqual(D.tool_pins_frozen(other, m)["status"], "failed")
            self.assertEqual(D.tool_pins_frozen(Path(d) / "absent.json", m)["status"], "failed")


class ManifestSchema(unittest.TestCase):
    def test_every_tracked_manifest_is_v3_and_valid(self):
        names = sorted(p.stem for p in (SCRIPTS.parent / "fixtures/designs").glob("*.json"))
        self.assertIn("pipeline", names)
        for name in names:
            m = json.loads((SCRIPTS.parent / "fixtures/designs" / f"{name}.json").read_text())
            self.assertEqual(D.manifest_problems(m), [], name)
            self.assertEqual(m["schema"], D.DESIGN_SCHEMA)
        legacy = json.loads((SCRIPTS.parent / "fixtures/designs/pipeline.json").read_text())
        self.assertEqual((legacy["recipe"]["physical"], legacy["recipe"]["floorplan"]), ("legacy", None))

    def test_schema_rules(self):
        m = json.loads((SCRIPTS.parent / "fixtures/designs/counter.json").read_text())
        bad = json.loads(json.dumps(m))
        bad["recipe"]["physical"] = "maze"
        self.assertTrue(any("recipe.physical" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        bad["recipe"]["floorplan"] = None
        self.assertTrue(any("track recipe" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        del bad["pdks"]["sky130"]["physical_profile"]
        self.assertTrue(any("physical_profile" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        bad["pdks"]["sky130"]["surprise"] = {}
        self.assertTrue(any("unknown keys" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        bad["clock"] = None
        self.assertTrue(any("clock must be" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        bad["sdc"]["clock"] = bad["sdc"]["reference_clock"]  # v2 single clock SDC
        self.assertTrue(any("per PDK" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        del bad["pdks"]["gf180"]["sdc_clock"]
        self.assertTrue(any("sdc_clock" in x for x in D.manifest_problems(bad)))
        self.assertEqual(D.floorplan_recipe(m, "gf180")["utilization"], 0.35)  # per-PDK override
        self.assertEqual(D.floorplan_recipe(m, "sky130"), m["recipe"]["floorplan"])
        bad = json.loads(json.dumps(m))
        bad["pdks"]["gf180"]["floorplan_recipe"] = {"utilization": "dense"}
        self.assertTrue(any("floorplan_recipe" in x for x in D.manifest_problems(bad)))
        bad = json.loads(json.dumps(m))
        bad["pdks_unavailable"] = {"sky130": "x"}
        self.assertTrue(any("both available and unavailable" in x for x in D.manifest_problems(bad)))
        del bad["budgets"]["hold_eco_target_margin_ns"]
        self.assertTrue(any("hold_eco_target_margin_ns" in x for x in D.manifest_problems(bad)))


class ClockPeriodRule(unittest.TestCase):
    """Owner rule 2026-10-24: max(10 ns, ceil(1.25 x pre-layout ss critical-path delay, 1 ns))."""

    def test_rule_arithmetic(self):
        self.assertEqual(D.clock_period_rule(3.6), 10)
        self.assertEqual(D.clock_period_rule(8.0), 10)       # 10.0 exactly stays 10
        self.assertEqual(D.clock_period_rule(8.0001), 11)
        self.assertEqual(D.clock_period_rule(9.624356), 13)  # 12.03 rounds up
        self.assertEqual(D.clock_period_rule(9.6), 12)       # 12.0 exactly (no float creep)

    def test_tracked_records_match_their_manifests(self):
        tools = {k: dict(sha256=v["sha256"]) for k, v in
                 json.loads((SCRIPTS.parent / "fixtures/flow/rtl2gds-tools.json").read_text())["tools"].items()}
        for name in ("pipeline", "counter", "alu", "mult"):
            m = D.load_manifest(D.ROOT / f"fixtures/designs/{name}.json")
            r = D.clock_period_status(m, tools)
            self.assertEqual(r["status"], "passed", (name, r))
            for tag, row in r["pdks"].items():
                self.assertEqual(row["sdc_period_ns"], row["period_ns"])
                self.assertGreaterEqual(row["period_ns"], 10)

    def test_a_tampered_period_or_foreign_tool_fails(self):
        tools = {k: dict(sha256=v["sha256"]) for k, v in
                 json.loads((SCRIPTS.parent / "fixtures/flow/rtl2gds-tools.json").read_text())["tools"].items()}
        m = D.load_manifest(D.ROOT / "fixtures/designs/counter.json")
        with work_tmp() as d:
            sdc = Path(d) / "gf180-cts.sdc"
            text = D.resolve(m["pdks"]["gf180"]["sdc_clock"]["path"]).read_text()
            sdc.write_text(re.sub(r"-period [0-9.]+", "-period 11", text))
            bad = json.loads(json.dumps(m))
            bad["pdks"]["gf180"]["sdc_clock"] = dict(path=str(sdc), sha256=D.sha256_file(sdc))
            self.assertEqual(D.clock_period_status(bad, tools)["status"], "failed")
        other = json.loads(json.dumps(tools))
        other["ruststa"]["sha256"] = "0" * 64
        self.assertEqual(D.clock_period_status(m, other)["status"], "failed")


class SdcAndPorts(unittest.TestCase):
    def test_virtual_clock_rendering(self):
        ports = [p for p in D.ports_from_yosys_json(PORTS_JSON) if p["name"] != "clk"]
        tmpl = (SCRIPTS.parent / "fixtures/designs/postroute-io-virtual.sdc.tmpl").read_text()
        out = D.render_sdc("H\n", tmpl, ports, None, "core")
        self.assertIn("set_input_delay 0.2 -clock core [get_ports {en a b}]", out)
        self.assertNotIn("set_propagated_clock", out)

    def test_verilator_xml_ports(self):
        xml = """<verilator_xml><netlist><module name="alu" topModule="1">
<var name="op_i" dtype_id="1" dir="input" pinIndex="1"/><var name="a_i" dtype_id="2" dir="input" pinIndex="2"/>
<var name="z" dtype_id="3" dir="output" pinIndex="3"/><var name="sum" dtype_id="2"/></module></netlist>
<typetable><refdtype id="1" name="alu_op_e" sub_dtype_id="4"/><basicdtype id="4" name="logic" left="2" right="0"/>
<basicdtype id="2" name="logic" left="7" right="0"/><basicdtype id="3" name="logic"/></typetable></verilator_xml>"""
        ports = L.verilator_ports(xml, "alu")
        self.assertEqual([(p["name"], p["direction"], p["width"], p["vector"]) for p in ports],
                         [("op_i", "input", 3, True), ("a_i", "input", 8, True), ("z", "output", 1, False)])
        self.assertEqual(ports[0]["bits"], ["op_i[0]", "op_i[1]", "op_i[2]"])
        with self.assertRaises(D.GateError):
            L.verilator_ports(xml, "other")

    def test_verilator_json_ports_and_budgets(self):
        data = dict(type="NETLIST", types=[
            dict(type="REFDTYPE", addr="ref", refDTypep="enum"),
            dict(type="ENUMDTYPE", addr="enum", refDTypep="wide"),
            dict(type="BASICDTYPE", addr="wide", keyword="logic", range="2:0"),
            dict(type="BASICDTYPE", addr="bit", keyword="logic")],
            modules=[dict(type="MODULE", name="alu", stmtsp=[
                dict(type="VAR", name="op", isPrimaryIO=True, direction="INPUT", dtypep="ref"),
                dict(type="VAR", name="z", isPrimaryIO=True, direction="OUTPUT", dtypep="bit"),
                dict(type="VAR", name="internal", direction="INPUT", dtypep="bit")])])
        ports = L.verilator_json_ports(json.dumps(data), "alu")
        self.assertEqual([(p["name"], p["width"]) for p in ports], [("op", 3), ("z", 1)])
        self.assertEqual(ports[0]["bits"], ["op[0]", "op[1]", "op[2]"])
        with self.assertRaises(D.GateError):
            L.verilator_json_ports(json.dumps(data), "alu", max_nodes=2)
        with self.assertRaises(D.GateError):
            L.verilator_json_ports(json.dumps(data), "alu", max_bits=2)
        data["types"][0]["refDTypep"] = "ref"
        with self.assertRaises(D.GateError):
            L.verilator_json_ports(json.dumps(data), "alu")

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

    def test_rail_constants_and_exhaustive_testbench(self):
        pins = dict(self.PINS, INV={"A": dict(direction="INPUT", use="SIGNAL"),
                                    "Y": dict(direction="OUTPUT", use="SIGNAL"),
                                    "VPWR": dict(direction="INOUT", use="POWER")})
        text = "  INV t0 (.Y(n0), .A(VSS), .VPWR(VDD));\n  INV t1 (.Y(n1), .A(VDD), .VPWR(VDD));\n"
        out, log = L.rail_constant_view(text, pins, ["VDD", "VSS"])
        self.assertIn("INV t0 (.Y(n0), .A(1'b0), .VPWR(VDD));", out)
        self.assertIn("INV t1 (.Y(n1), .A(1'b1), .VPWR(VDD));", out)
        self.assertEqual(log["rail_tied_signal_pins"], ["t0/A=VSS", "t1/A=VDD"])
        ports = [p for p in D.ports_from_yosys_json(PORTS_JSON) if p["name"] != "clk"]
        tb = L.xorshift_tb("top", ports, None, 32, 1, exhaustive=True)
        self.assertIn("{en, a, b} = idx[4:0]; idx = idx + 1;", tb)
        self.assertNotIn("s = s ^", tb)
        with self.assertRaises(D.GateError):
            L.xorshift_tb("top", ports, None, 16, 1, exhaustive=True)
        with self.assertRaises(D.GateError):
            L.xorshift_tb("top", D.ports_from_yosys_json(PORTS_JSON), "clk", 64, 1, exhaustive=True)
        self.assertEqual(L.second_oracle(dict(yosys=D.item("unverified"), exhaustive=D.item("passed",
                         expected=dict(cycles=32))))["status"], "passed")
        self.assertEqual(L.second_oracle(dict(yosys=D.item("failed")))["oracle"], "Yosys equiv_simple")

    def test_first_input_pin(self):
        with tempfile.TemporaryDirectory() as d:
            n = Path(d) / "n.v"
            n.write_text(self.NET)
            self.assertEqual(L.first_input_pin(n, self.PINS), "g0/A")

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


class PulseScope(unittest.TestCase):
    def source(self, unchecked, per_corner, rows, status="partial", ref_mpw=None):
        lim = [dict(corner=c, kind=k, checked=True, unevaluated=False, violating=0)
               for c in ("tt",) for k in T.DRV_KINDS]
        g = dict(metrics=dict(pulse_width_status=status, drv_status="met"),
                 completeness=dict(unchecked_pulse_width_pins=unchecked),
                 unclocked_pulse_pins=per_corner, endpoints={"tt": {"r0/D": dict(setup=1.0, hold=0.2)}},
                 reports=dict(pulse_width=dict(rows=rows), min_period=dict(rows=[]), drv=dict(rows=[]),
                              limits=dict(rows=lim)))
        ref = {"tt": dict(setup={"r0/D": 1.0}, hold={"r0/D": 0.2}, mpw=ref_mpw or {}, period={},
                          max_transition={"x": dict(violated=False)}, max_capacitance={"x": dict(violated=False)},
                          max_fanout={"x": dict(violated=False)}, unannotated=0, partially_unannotated=0)}
        return dict(gating=g, compat=g), ref

    def test_async_set_clear_pins_are_out_of_scope_only_when_matched(self):
        budgets = dict(hold_margin_ns=0.05, setup_margin_ns=0.0, parity_ns=0.001)
        row = dict(pin="r0/CLK", corner="tt", kind="min_pulse_width_high", slack=1.0, limit=0.5)
        mpw = {"r0/CLK:high": dict(required=0.5, actual=1.5, slack=1.0, met=True)}
        rust, ref = self.source(1, {"tt": 1}, [row], ref_mpw=mpw)
        r = T.evaluate_source(rust, ref, ["tt"], budgets, async_pins=["r0/RESET_B"])["mpw_min_period"]
        self.assertEqual(r["status"], "passed", r)
        self.assertEqual(r["async_set_clear_pins_unchecked"]["pins"], ["r0/RESET_B"])
        # A count that does not match the async pins stays unverified.
        rust, ref = self.source(2, {"tt": 2}, [row], ref_mpw=mpw)
        r = T.evaluate_source(rust, ref, ["tt"], budgets, async_pins=["r0/RESET_B"])["mpw_min_period"]
        self.assertEqual(r["status"], "unverified")
        # Combinational design: nothing to check in either engine -> not_applicable.
        rust, ref = self.source(0, {}, [], status="unconstrained")
        r = T.evaluate_source(rust, ref, ["tt"], budgets, clocked=False)["mpw_min_period"]
        self.assertEqual(r["status"], "not_applicable")
        r = T.evaluate_source(rust, ref, ["tt"], budgets, clocked=True)["mpw_min_period"]
        self.assertEqual(r["status"], "unverified")
        u = T.unverified_source("mesh")
        self.assertEqual({v["status"] for k, v in u.items() if k != "matrix"}, {"unverified"})


class Evidence(unittest.TestCase):
    def test_absolute_paths_are_reduced_to_names(self):
        import rtl2gds
        out = rtl2gds.evidence(dict(revision=dict(branch="feat/rtl2gds-gate"), memory_preflight="x",
                                    pdks=[dict(reason="see /mnt/a/b/density.json now", path="/x/y",
                                               files=["/a/b.v", "rel/c.v"])]))
        self.assertEqual(out["revision"]["branch"], "feat/rtl2gds-gate")
        self.assertEqual(out["pdks"][0], dict(reason="see density.json now", files=["b.v", "rel/c.v"]))
        self.assertNotIn("memory_preflight", out)


class ClosureHelpers(unittest.TestCase):
    def test_constant_pins_follow_ties_and_aliases(self):
        import hold_closure as H
        with tempfile.TemporaryDirectory() as d:
            n, cfg = Path(d) / "n.v", Path(d) / "c.json"
            n.write_text("module t (input wire clk, output wire q);\n  wire n1;\n  wire rr_tie_net_0;\n"
                         "  assign n1 = rr_tie_net_0;\n  TIEH rr_tie_cell_0 (.Z(rr_tie_net_0));\n"
                         "  DFF r0 (.D(n1), .CLK(clk), .Q(q));\n  DFF r1 (.D(1'b0), .CLK(clk), .Q());\nendmodule\n")
            cfg.write_text(json.dumps(dict(tie_high=dict(cell="TIEH", pin="Z"), tie_low=None)))
            self.assertEqual(H.constant_pins(n, cfg), {"r0/D", "r1/D"})

    def test_constant_pins_ports_rail_ties_and_inverters(self):
        import hold_closure as H
        with tempfile.TemporaryDirectory() as d:
            n, cfg = Path(d) / "n.v", Path(d) / "c.json"
            # A rail tie (INV with a literal rail input) is a constant; the same
            # master as an ordinary inverter is not. Ports aliased to a tie
            # net or driven by a tie copy are constant-driven endpoints.
            n.write_text("module t (\n  input wire a,\n  output wire [1:0] z,\n  output wire w,\n  output wire v\n);\n"
                         "  wire rr_tie_net_0;\n  wire n2;\n"
                         "  INV rr_tie_cell_0 (.Y(rr_tie_net_0), .A(1'b0));\n  INV g1 (.A(a), .Y(n2));\n"
                         "  assign z[0] = rr_tie_net_0;\n  CONB rr_porttie_1 (.LO(z[1]));\n"
                         "  assign w = n2;\n  DFF r0 (.D(n2), .CLK(a), .Q(v));\nendmodule\n")
            cfg.write_text(json.dumps(dict(tie_high=dict(cell="INV", pin="Y", rail_input=dict(pin="A", supply="ground")),
                                           tie_low=None, constant_cells=[dict(cell="CONB", high=["HI"], low=["LO"])])))
            self.assertEqual(H.constant_pins(n, cfg), {"rr_tie_cell_0/A", "z[0]", "z[1]"})


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

    def test_remove_all_chains_bypasses_every_hold_cell(self):
        text = ("module t (input wire a, output wire y);\n  wire rr_eco_hold_0_n;\n  wire rr_eco_hold_1_n;\n"
                "  wire rr_eco_buf_2_n;\n  wire n1;\n"
                "  BUF rr_eco_hold_1 (.A(a), .X(rr_eco_hold_1_n));\n"
                "  BUF rr_eco_hold_0 (.A(rr_eco_hold_1_n), .X(rr_eco_hold_0_n));\n"
                "  BUF4 rr_eco_buf_2 (.A(rr_eco_hold_0_n), .X(rr_eco_buf_2_n));\n"
                "  AND2 g0 (.A(rr_eco_buf_2_n), .B(rr_eco_hold_0_n), .X(n1));\n"
                "  DFF r0 (.CLK(a), .D(n1), .Q(y));\nendmodule\n")
        out, removed = C.remove_all_chains(text, "rr_eco_hold_", {"BUF": ("A", "X")})
        self.assertEqual(sorted(removed), ["rr_eco_hold_0", "rr_eco_hold_1"])
        self.assertIn("BUF4 rr_eco_buf_2 (.A(a), .X(rr_eco_buf_2_n));", out)
        self.assertIn("AND2 g0 (.A(rr_eco_buf_2_n), .B(a), .X(n1));", out)
        self.assertNotIn("rr_eco_hold", out)
        with self.assertRaises(D.GateError):
            C.remove_all_chains(out, "rr_eco_hold_", {"BUF": ("A", "X")})
        # Escalated: ECO buffers too, an output port assign included.
        out, removed = C.remove_all_chains(text + "  assign y2 = rr_eco_buf_2_n;\n", ("rr_eco_hold_", "rr_eco_buf_"),
                                           {"BUF": ("A", "X"), "BUF4": ("A", "X")})
        self.assertEqual(sorted(removed), ["rr_eco_buf_2", "rr_eco_hold_0", "rr_eco_hold_1"])
        self.assertIn("AND2 g0 (.A(a), .B(a), .X(n1));", out)
        self.assertIn("assign y2 = a;", out)

    def test_inverted_output_and_antenna_candidates(self):
        text = ("module t (input wire a, output wire [1:0] y, output wire z);\n  wire n1;\n"
                "  assign y[1] = n1;\n  DFF r0 (.D(a), .CLK(a), .Q(y[0]));\n  AND2 g1 (.A(y[0]), .B(a), .X(n1));\n"
                "  BUF g2 (.A(a), .X(z));\n  DIODE rr_eco_ant_0 (.D(a));\nendmodule\n")
        out = C.invert_output(text, "y[0]", "INV", ("A", "Y"))
        self.assertIn("DFF r0 (.D(a), .CLK(a), .Q(rr_mut_n));", out)
        self.assertIn("AND2 g1 (.A(rr_mut_n), .B(a), .X(n1));", out)
        self.assertIn("INV rr_mut ( .A(rr_mut_n), .Y(y[0]) );", out)
        out = C.invert_output(text, "y[1]", "INV", ("A", "Y"))
        self.assertIn("assign rr_mut_n = n1;", out)
        with self.assertRaises(D.GateError):
            C.invert_output(text, "q", "INV", ("A", "Y"))
        self.assertEqual(C.antenna_candidates(text + "  BUF g3 (.A(b), .X());\n", ["a", "b"]), ["b"])

    def test_scaled_spef_resistance(self):
        with tempfile.TemporaryDirectory() as d:
            src, dst = Path(d) / "a.spef", Path(d) / "b.spef"
            src.write_text("*D_NET n 1.0\n*CONN\n*I a:X O\n*CAP\n1 n:1 0.5\n*RES\n1 n:1 a:X 2.5\n*END\n")
            self.assertEqual(C.scale_spef_r(src, dst, 1000.0), 1)
            self.assertIn("1 n:1 a:X 2500\n", dst.read_text())
            self.assertIn("1 n:1 0.5\n", dst.read_text())


if __name__ == "__main__":
    unittest.main()


class CompareRuns(unittest.TestCase):
    """Independent re-run check: only the documented normalizations are applied."""

    def test_normalizations(self):
        import compare_runs
        with work_tmp() as d:
            a, b = Path(d) / "a.spef", Path(d) / "b.spef"
            a.write_bytes(b'*SPEF "x"\n*DATE "Mon"\n*D_NET n 1\n')
            b.write_bytes(b'*SPEF "x"\n*DATE "Tue"\n*D_NET n 1\n')
            self.assertEqual(compare_runs.normalized(a, "spef.openrcx.ss", "/p/"),
                             compare_runs.normalized(b, "spef.openrcx.ss", "/q/"))
            self.assertNotEqual(compare_runs.normalized(a, "spef.native.ss", "/p/"),
                                compare_runs.normalized(b, "spef.native.ss", "/q/"))
            j, k = Path(d) / "a.json", Path(d) / "b.json"
            j.write_text('{"path": "/p/fixtures/x.json", "n": 1}')
            k.write_text('{"path": "/q/fixtures/x.json", "n": 1}')
            self.assertEqual(compare_runs.normalized(j, "routes", "/p/"), compare_runs.normalized(k, "routes", "/q/"))
            k.write_text('{"path": "/q/fixtures/x.json", "n": 2}')
            self.assertNotEqual(compare_runs.normalized(j, "routes", "/p/"), compare_runs.normalized(k, "routes", "/q/"))


class HoldMutationControls(unittest.TestCase):
    def evaluate(self, removal, replacement=True):
        from types import SimpleNamespace
        from unittest import mock
        names = ("drc_spacing", "label_swap", "removed_via", "tampered_sdc", "tampered_spef",
                 "inverted_output", "missing_tool", "relaxed_manifest", "antenna_planted", "tampered_artifact")
        patches = {n:mock.Mock(return_value=dict(detected=True)) for n in names}
        patches.update(hold_cells_inserted=mock.Mock(return_value=True),
                       removed_hold_cell=mock.Mock(return_value=removal),
                       planted_hold_uncertainty=mock.Mock(return_value=dict(detected=replacement)))
        with work_tmp() as tmp, mock.patch.multiple(C, **patches):
            ctx = SimpleNamespace(h=SimpleNamespace(root=Path(tmp)))
            return C.run_all(ctx, SimpleNamespace(tag="gf180"), {"final_iteration":1}, {})

    def test_non_faulty_removal_needs_a_detected_replacement(self):
        old = dict(detected=False, mutation_is_fault=False)
        good = self.evaluate(old)
        self.assertEqual(good["status"], "passed")
        self.assertEqual(good["controls"]["removed_hold_cell"]["detected"], "not_applicable")
        self.assertIs(good["controls"]["removed_hold_cell"]["experiment"]["detected"], False)
        self.assertEqual(self.evaluate(old, False)["status"], "failed")
        self.assertEqual(self.evaluate(old, None)["status"], "unverified")

    def test_real_misses_and_incomplete_removals_cannot_be_excused(self):
        miss = self.evaluate(dict(detected=False, mutation_is_fault=True))
        self.assertEqual(miss["status"], "failed")
        self.assertNotIn("planted_hold_uncertainty", miss["controls"])
        missing = self.evaluate(dict(detected=None, mutation_is_fault=False))
        self.assertEqual(missing["status"], "unverified")
        self.assertNotIn("planted_hold_uncertainty", missing["controls"])
