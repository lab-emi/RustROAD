# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the signoff/flow parsers (plain unittest; no podman, no PDK tools).

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

import check_cell_policy  # noqa: E402
import container  # noqa: E402
import flow_status  # noqa: E402
import manifest  # noqa: E402
import parsers  # noqa: E402
import refnet  # noqa: E402
import routing_units  # noqa: E402

LYRDB = """<?xml version="1.0" encoding="utf-8"?>
<report-database>
 <top-cell>lane_smoke</top-cell>
 <generator>drc: script='deck.drc'</generator>
 <categories>
  <category><name>M1.2a</name><categories></categories></category>
  <category><name>M2.3</name><categories>
    <category><name>sub</name><categories></categories></category></categories></category>
 </categories>
 <items>
  <item><category>'M1.2a'</category><cell>lane_smoke</cell><multiplicity>1</multiplicity>
   <values><value>edge-pair: (1,2;3,4)|(5,6;7,8)</value></values></item>
  <item><category>'M1.2a'</category><cell>gf180mcu_fd_sc_mcu7t5v0__clkbuf_1</cell><multiplicity>3</multiplicity></item>
  <item><category>M2.3</category><cell>lane_smoke</cell><multiplicity>1</multiplicity></item>
 </items>
</report-database>
"""

MAGIC_DRC = """RR_TECH sky130A
RR_CELLS 13
RR_BBOX 0 0 11500 9792
RR_STYLE drc(full)
RR_HIER_TOTAL 26
RR_HIER_WHY 16 :: Metal2 spacing < 0.14um (met2.2)
RR_HIER_WHY 25 :: Metal1 spacing < 0.14um (met1.2)
RR_CELL_WHY sky130_fd_sc_hd__inv_1 2 :: N-diff distance to P-tap must be < 15.0um (LU.2)
RR_FLAT_TOTAL 26
RR_FLAT_WHY 16 :: Metal2 spacing < 0.14um (met2.2)
RR_FLAT_WHY 25 :: Metal1 spacing < 0.14um (met1.2)
RR_DONE
"""

ANTENNA = """Running antenna checks.
Cell: sky130_fd_sc_hd__inv_1_2
Antenna violation detected at plane metal2
Effective antenna ratio 893.08 > limit 400
Gate rect (212 47) to (242 177)
antennacheck finished.
RR_ANT_DONE
"""

NETGEN_OK = [
    {"pins": [["1", "2"], ["2", "1"]]},
    {"name": ["inv", "inv"], "devices": [[["nfet", 1], ["pfet", 1]], [["pfet", 1], ["nfet", 1]]],
     "nets": [4, 4], "badnets": [], "badelements": [], "pins": [["A", "Y"], ["Y", "A"]]},
    {"name": ["top", "top"], "devices": [[["inv", 2]], [["inv", 2]]], "nets": [5, 5],
     "badnets": [], "badelements": [], "pins": [["a", "y", "VDD"], ["VDD", "a", "y"]]},
]

POWERED_V = r"""module lane_smoke (
  input wire clk,
  input wire [1:0] a,
  output wire [1:0] y,
  inout wire VDD,
  inout wire VSS
);
  wire \q[0] ;
  wire n0;
  assign y[0] = \q[0] ;
  assign y[1] = n0;
  sky130_fd_sc_hd__inv_1 g0 (.A(a[0]), .Y(n0), .VGND(VSS), .VNB(VSS), .VPB(VDD), .VPWR(VDD));
  sky130_fd_sc_hd__dfxtp_1 r0 (.D(a[1]), .CLK(clk), .Q(\q[0] ), .VGND(VSS), .VNB(VSS), .VPB(VDD), .VPWR(VDD));
  sky130_fd_sc_hd__tapvpwrvgnd_1 rr_tap_0 (.VGND(VSS), .VPWR(VDD));
endmodule
"""

LIBRARY = """.subckt sky130_fd_sc_hd__inv_1 A VGND VNB VPB VPWR Y
X0 VGND A Y VNB sky130_fd_pr__nfet_01v8 w=650000u l=150000u
X1 VPWR A Y VPB sky130_fd_pr__pfet_01v8_hvt w=1e+06u l=150000u
.ends
.subckt sky130_fd_sc_hd__dfxtp_1 CLK D VGND VNB VPB VPWR Q
X0 Q D VGND VNB sky130_fd_pr__nfet_01v8 w=420000u l=150000u
.ends
.subckt sky130_fd_sc_hd__tapvpwrvgnd_1 VGND VPWR
.ends
"""

LEF = """VERSION 5.7 ;
UNITS
  DATABASE MICRONS 2000 ;
END UNITS
LAYER Metal1
  TYPE ROUTING ;
  WIDTH 0.230 ;
  SPACING 0.230 ;
  SPACING 0.300 RANGE 10.005 999.00 ;
END Metal1
LAYER Via1
  TYPE CUT ;
  SPACING 0.26 ;
END Via1
LAYER Metal2
  TYPE ROUTING ;
  # SPACING 0.1 ;
  SPACINGTABLE
    PARALLELRUNLENGTH 0.00
    WIDTH 0 0.28
    WIDTH 3 0.4 ;
END Metal2
VIA Via1_HV DEFAULT
  LAYER Metal1 ;
    RECT -0.19 -0.13 0.19 0.13 ;
  LAYER Via1 ;
    RECT -0.13 -0.13 0.13 0.13 ;
  LAYER Metal2 ;
    RECT -0.14 -0.19 0.14 0.19 ;
END Via1_HV
"""


class LyrdbTest(unittest.TestCase):
    def test_counts_all_cells_and_nested_categories(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "x.lyrdb"
            path.write_text(LYRDB)
            r = parsers.parse_lyrdb(path)
        self.assertEqual(r["total"], 3)
        self.assertEqual(r["by_rule"], {"M1.2a": 2, "M2.3": 1})
        self.assertEqual(r["by_cell"], {"gf180mcu_fd_sc_mcu7t5v0__clkbuf_1": 1, "lane_smoke": 2})
        self.assertEqual(r["multiplicity_by_rule"]["M1.2a"], 4)
        self.assertEqual(r["top_cell"], "lane_smoke")
        self.assertEqual(r["categories"], 3)

    def test_missing_or_foreign_file_is_an_error_not_zero(self):
        with self.assertRaises(parsers.ParseError):
            parsers.parse_lyrdb("/nonexistent/report.lyrdb")
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "x.lyrdb"
            path.write_text("<html/>")
            with self.assertRaises(parsers.ParseError):
                parsers.parse_lyrdb(path)


class MagicTest(unittest.TestCase):
    def test_drc_markers(self):
        r = parsers.parse_magic_drc(MAGIC_DRC)
        self.assertTrue(r["done"])
        self.assertEqual(r["style"], "drc(full)")
        self.assertEqual((r["hier_total"], r["flat_total"]), (26, 26))
        self.assertEqual(r["flat_why"], {"met2.2": 16, "met1.2": 25})
        self.assertEqual(r["standalone_cell_why"], {"sky130_fd_sc_hd__inv_1": {"LU.2": 2}})

    def test_wrong_top_and_crash(self):
        r = parsers.parse_magic_drc("RR_CELLS 3\nRR_TOP_MISSING wrong_top\n")
        self.assertEqual(r["top_missing"], "wrong_top")
        self.assertIsNone(r["hier_total"])
        self.assertFalse(parsers.parse_magic_drc("RR_HIER_TOTAL 0\n")["done"])

    def test_antenna(self):
        r = parsers.parse_magic_antenna(ANTENNA)
        self.assertTrue(r["ran"] and r["done"])
        self.assertEqual(r["violations"], 1)
        self.assertEqual(r["by_plane"], {"metal2": 1})
        self.assertFalse(parsers.parse_magic_antenna("RR_ANT_DONE\n")["ran"])


class NetgenTest(unittest.TestCase):
    def test_match(self):
        r = parsers.parse_netgen(json.dumps(NETGEN_OK), "Final result: Circuits match uniquely.\n", "top")
        self.assertTrue(r["matched"], r["problems"])

    def test_final_result_and_badnets(self):
        r = parsers.parse_netgen(json.dumps(NETGEN_OK), "Final result: Top level cell failed pin matching.\n", "top")
        self.assertFalse(r["matched"])
        bad = json.loads(json.dumps(NETGEN_OK))
        bad[2]["badnets"] = [["n1"]]
        bad[2]["nets"] = [5, 6]
        r = parsers.parse_netgen(json.dumps(bad), "Final result: Circuits match uniquely.\n", "top")
        self.assertFalse(r["matched"])
        self.assertEqual(len(r["problems"]), 2)
        r = parsers.parse_netgen(json.dumps(NETGEN_OK), "Final result: Circuits match uniquely.\n", "other")
        self.assertIn("top circuit other not compared", r["problems"])
        with self.assertRaises(parsers.ParseError):
            parsers.parse_netgen("[]", "", "top")


class MiscParserTest(unittest.TestCase):
    def test_density_finite(self):
        ok = parsers.parse_density_log("li1_ca_density is 0.94\nm1_ca_density is 0.9\n")
        self.assertTrue(ok["finite"])
        bad = parsers.parse_density_log("li1_ca_density is -Infinity\nm4_ca_density is NaN\n")
        self.assertFalse(bad["finite"])
        self.assertEqual(len(bad["nonfinite"]), 2)
        self.assertFalse(parsers.parse_density_log("nothing printed")["finite"])

    def test_strmxor(self):
        self.assertTrue(parsers.parse_strmxor("No differences found\n")["identical"])
        r = parsers.parse_strmxor("Result summary\n  Layer      Output       Differences (shape count)\n"
                                  "  ------\n  68/20      68/20        2\n  70/20      -            3\n")
        self.assertFalse(r["identical"])
        self.assertEqual(r["layer_differences"], {"68/20": 2, "70/20": 3})
        self.assertEqual(r["difference_count"], 5)

    def test_lvsdb_verdict(self):
        good = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=3, schematic_pins=3),
                              dict(layout="inv", schematic="INV", status="Match")])
        self.assertTrue(parsers.lvsdb_verdict(good, "top")["matched"])
        skipped = dict(circuits=[dict(layout="top", schematic="TOP", status="Skipped"),
                                 dict(layout="fill", schematic=None, status="NoMatch")])
        self.assertFalse(parsers.lvsdb_verdict(skipped, "top")["matched"])
        pins = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=2, schematic_pins=3)])
        self.assertFalse(parsers.lvsdb_verdict(pins, "top")["matched"])
        self.assertFalse(parsers.lvsdb_verdict(dict(circuits=[]), "top")["matched"])

    def test_def_odd_coordinates(self):
        r = parsers.count_def_odd_coordinates(
            "ROW r s 1 2 N DO 3 BY 1 STEP 5 0 ;\n+ ROUTED Metal1 1201 ( 1 2 3 ) NEW Metal2 ( 4 6 )\n( g0 VDD )")
        self.assertEqual((r["checked"], r["odd"]), (10, 5))
        self.assertEqual(parsers.count_def_odd_coordinates("( 2 4 ) ( 6 * )")["odd"], 0)


class RefnetTest(unittest.TestCase):
    def test_parse_and_reference(self):
        net = refnet.parse_verilog(POWERED_V)
        self.assertEqual(net["top"], "lane_smoke")
        self.assertEqual(net["ports"], ["clk", "a[1]", "a[0]", "y[1]", "y[0]", "VDD", "VSS"])
        self.assertEqual(net["net_to_port"], {"q[0]": "y[0]", "n0": "y[1]"})
        lib = refnet.parse_subckts(LIBRARY)
        self.assertEqual(lib["sky130_fd_sc_hd__tapvpwrvgnd_1"]["devices"], 0)
        text, info = refnet.spice_reference(net, lib, rescale_units=True, cdl=False)
        self.assertEqual(info["dropped_deviceless"], {"sky130_fd_sc_hd__tapvpwrvgnd_1": 1})
        self.assertIn("Xg0 a[0] VSS VSS VDD VDD y[1] sky130_fd_sc_hd__inv_1", text)
        self.assertIn("w=0.65u", text)
        self.assertIn("w=1u", text)
        self.assertNotIn("650000u", text)

    def test_rejections(self):
        shorted = POWERED_V.replace("assign y[1] = n0;", "assign y[1] = \\q[0] ;")
        with self.assertRaises(refnet.NetlistError):
            refnet.parse_verilog(shorted)
        with self.assertRaises(refnet.NetlistError):
            refnet.parse_verilog(POWERED_V.replace("assign y[1] = n0;", "assign y[1] = 1'b0;"))
        with self.assertRaises(refnet.NetlistError):
            refnet.parse_verilog(POWERED_V.replace(".A(a[0]),", "a[0],"))
        net = refnet.parse_verilog(POWERED_V.replace("sky130_fd_sc_hd__inv_1 g0", "sky130_fd_sc_hd__nope g0"))
        with self.assertRaises(refnet.NetlistError):
            refnet.spice_reference(net, refnet.parse_subckts(LIBRARY), rescale_units=True, cdl=False)


class RoutingUnitsTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.lef = Path(self.dir.name) / "t.tlef"
        self.lef.write_text(LEF)

    def tearDown(self):
        self.dir.cleanup()

    def test_lef_rules(self):
        r = routing_units.lef_rules(LEF)
        self.assertEqual(r["dbu_per_um"], 2000)
        self.assertEqual(r["layers"]["Metal1"]["spacing_um"], 0.23)
        self.assertEqual(r["layers"]["Metal2"]["spacing_um"], 0.28)
        self.assertEqual(r["layers"]["Metal2"]["source"], "SPACINGTABLE WIDTH 0")
        self.assertEqual(r["vias"]["Via1_HV"], ["Metal1", "Via1", "Metal2"])

    def test_half_unit_profile_rejected(self):
        old = dict(layers=[dict(name="Metal1", spacing=230), dict(name="Metal2", spacing=280)],
                   vias=[dict(name="Via1_HV", cut_spacing=260)])
        with self.assertRaises(routing_units.UnitsError) as ctx:
            routing_units.assert_profile_units(old, str(self.lef))
        self.assertIn("Metal1: profile spacing 230 DBU < LEF SPACING 0.23 um x 2000 DBU/um = 460 DBU", str(ctx.exception))
        self.assertIn("Via1_HV", str(ctx.exception))
        new = dict(layers=[dict(name="Metal1", spacing=460), dict(name="Metal2", spacing=560)],
                   vias=[dict(name="Via1_HV", cut_spacing=520)])
        self.assertEqual(routing_units.assert_profile_units(new, str(self.lef))["status"], "passed")
        with self.assertRaises(routing_units.UnitsError):
            routing_units.assert_profile_units(new, str(self.lef), lef_sha256="0" * 64)

    def test_committed_gf180_profile_matches_lef_units(self):
        root = SCRIPTS.parent
        config = json.loads((root / "fixtures/pdk/gf180-routing.json").read_text())
        self.assertEqual([l["spacing"] for l in config["layers"]], [460, 560, 560, 560])
        self.assertEqual({v["cut_spacing"] for v in config["vias"]}, {520})


class FlowStatusTest(unittest.TestCase):
    def passed(self):
        return {k: flow_status.item("passed") for k in flow_status.REQUIRED}

    def test_missing_signoff_is_partial_even_when_timing_passes(self):
        statuses = self.passed()
        for k in ("gds", "drc_magic", "lvs_netgen"):
            statuses[k] = flow_status.item("missing")
        gate = flow_status.overall([dict(pdk="sky130", signoff_statuses=statuses)], "passed", True, True)
        self.assertEqual(gate["status"], "partial")
        self.assertEqual(flow_status.EXIT[gate["status"]], 3)
        self.assertEqual({b["item"] for b in gate["blocking"]}, {"gds", "drc_magic", "lvs_netgen"})
        gate = flow_status.overall([dict(pdk="sky130")], "passed", True, True)
        self.assertEqual(gate["status"], "partial")
        self.assertEqual(len(gate["blocking"]), len(flow_status.REQUIRED))

    def test_all_passed_and_failures(self):
        pdks = [dict(pdk=t, signoff_statuses=self.passed()) for t in ("sky130", "gf180")]
        self.assertEqual(flow_status.overall(pdks, "passed", True, True)["status"], "passed")
        self.assertEqual(flow_status.overall(pdks, "partial", True, True)["status"], "partial")
        self.assertEqual(flow_status.overall(pdks, "passed", False, True)["status"], "failed")
        self.assertEqual(flow_status.overall(pdks, "passed", True, False)["status"], "failed")
        self.assertEqual(flow_status.overall([], "passed", True, True)["status"], "partial")
        pdks[0]["signoff_statuses"]["density"] = flow_status.item("failed")
        self.assertEqual(flow_status.overall(pdks, "passed", True, True)["status"], "passed",
                         "density is reported separately and is not a required item")

    def test_mpw_and_drv_never_pass_without_evidence(self):
        self.assertEqual(flow_status.mpw_status({"unanalyzed_arcs": {"min_pulse_width": 4}})["status"], "unchecked")
        self.assertEqual(flow_status.mpw_status({"unanalyzed_arcs": {}})["status"], "unchecked")
        self.assertEqual(flow_status.mpw_status(None)["status"], "missing")
        self.assertEqual(flow_status.drv_status("passed", ["tt_nom"])["status"], "unchecked")


class CellPolicyTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        root = Path(self.dir.name)
        lib = root / "libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0"
        lib.mkdir(parents=True)
        (lib / "synth_exclude.cells").write_text("gf180mcu_fd_sc_mcu7t5v0__sdffq_1\n")
        (lib / "pnr_exclude.cells").write_text("# DRC\ngf180mcu_fd_sc_mcu7t5v0__mux2_1\n")
        self.root = root
        p = "gf180mcu_fd_sc_mcu7t5v0__"
        self.mapped = root / "mapped.v"
        self.mapped.write_text(f"module t(input a, output y);\n {p}and2_2 g0 (.A1(a), .A2(a), .Z(y));\nendmodule\n")
        self.final = root / "final.v"
        self.final.write_text(f"module t(input a, output y);\n {p}and2_2 g0 (.A1(a), .A2(a), .Z(y));\n"
                              f" {p}clkbuf_1 rr_clkbuf_0 (.I(a), .Z(n));\n {p}filltie rr_tap_0 ();\nendmodule\n")

    def tearDown(self):
        self.dir.cleanup()

    def test_synth_violation_and_pass(self):
        r = check_cell_policy.check("gf180", "synth", [self.mapped], None, None, self.root)
        self.assertEqual(r["status"], "passed", r)
        bad = self.root / "bad.v"
        bad.write_text(self.mapped.read_text().replace("and2_2", "mux2_1") +
                       "module u(); gf180mcu_fd_sc_mcu7t5v0__dlyb_1 d0 (.I(a), .Z(b)); endmodule\n")
        r = check_cell_policy.check("gf180", "synth", [bad], None, None, self.root)
        self.assertEqual(r["status"], "failed")
        self.assertEqual({v["rule"] for v in r["violations"]}, {"pnr_exclude", "synth_exclude_or_reserved"})

    def test_physical_stage_allows_inserted_cts_and_tap_cells_only(self):
        r = check_cell_policy.check("gf180", "physical", [self.final], self.mapped, None, self.root)
        self.assertEqual(r["status"], "passed", r)
        bad = self.root / "bad_final.v"
        bad.write_text(self.final.read_text().replace("clkbuf_1 rr_clkbuf_0", "sdffq_1 rr_extra_0"))
        r = check_cell_policy.check("gf180", "physical", [bad], self.mapped, None, self.root)
        self.assertEqual(r["violations"][0]["rule"], "excluded_cell_added_after_synthesis")
        r = check_cell_policy.check("gf180", "physical", [self.final], None, None, self.root)
        self.assertEqual(r["status"], "unverified")

    def test_committed_policy_is_consistent_with_owner_reservation(self):
        policy = json.loads((SCRIPTS.parent / "fixtures/flow/cell-policy.json").read_text())
        gf = policy["pdks"]["gf180"]
        for glob in ("gf180mcu_fd_sc_mcu7t5v0__dly*", "gf180mcu_fd_sc_mcu7t5v0__clkbuf*",
                     "gf180mcu_fd_sc_mcu7t5v0__clkinv*", "gf180mcu_fd_sc_mcu7t5v0__mux2_1"):
            self.assertIn(glob, gf["synth_dont_use"])
        self.assertEqual(gf["rustsys_args"][0], "--dont-use")
        self.assertIn("sky130_fd_sc_hd__clkinv*", policy["pdks"]["sky130"]["synth_dont_use"])


class ContainerAndManifestTest(unittest.TestCase):
    def test_mount_and_label_guards(self):
        with self.assertRaises(container.ContainerError):
            container.check_mount("/", "/in/x")
        with self.assertRaises(container.ContainerError):
            container.check_mount(str(HERE), "/foss/pdks")
        with self.assertRaises(container.ContainerError):
            container.check_mount("relative/path", "/in/x")
        self.assertEqual(container.check_mount(str(HERE), "/scripts")[1], "/scripts")
        with self.assertRaises(container.ContainerError):
            container.run(["true"], out_dir=Path("/tmp/x"), label="ok")
        with self.assertRaises(container.ContainerError):
            container.run(["true"], out_dir=HERE, label="bad label")
        env = container.deterministic_env(2)
        self.assertEqual((env["LC_ALL"], env["TZ"], env["PYTHONHASHSEED"]), ("C", "UTC", "0"))

    def test_time_file(self):
        t = container.parse_time_file("\tMaximum resident set size (kbytes): 4321\n\tExit status: 0\n")
        self.assertEqual((t["peak_rss_kib"], t["inner_exit_status"]), (4321, 0))

    def test_manifest_schema_and_pins(self):
        m = manifest.load(SCRIPTS.parent / "fixtures/flow/signoff-pinned.json")
        self.assertEqual(m["images"]["iic_osic_tools"]["id"], container.SIGNOFF_IMAGE)
        self.assertIn("not_for_signoff", m)
        self.assertTrue(all(t["not_for_signoff"] for t in m["not_for_signoff"]))
        for tag in ("sky130", "gf180"):
            for key in ("tech_lef", "cell_lef", "cell_gds", "liberty_tt", "liberty_ss", "liberty_ff",
                        "cell_spice", "cell_cdl", "magicrc", "magic_tech", "netgen_setup", "klayout_drc",
                        "klayout_lvs", "klayout_map", "klayout_lyp", "rcx_min", "rcx_nom", "rcx_max",
                        "synth_exclude", "pnr_exclude"):
                entry = m["pdks"][tag]["files"][key]
                self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")
                self.assertGreater(entry["bytes"], 0)
        with tempfile.TemporaryDirectory() as d:
            bad = Path(d) / "m.json"
            bad.write_text(json.dumps(dict(m, schema="other")))
            with self.assertRaises(ValueError):
                manifest.load(bad)


if __name__ == "__main__":
    unittest.main()
