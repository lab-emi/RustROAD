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
import signoff  # noqa: E402

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

    def test_property_errors_fail_even_when_topology_matches(self):
        # Drive-strength swap: same topology, different transistor widths (F1).
        bad = json.loads(json.dumps(NETGEN_OK))
        bad[2]["properties"] = [[["and2_2_3/nfet:4", [["w", "1.3"]]], ["g0/nfet:3", [["w", "2.6"]]]]]
        out = "Netlists match uniquely with property errors.\nFinal result: Circuits match uniquely.\n" \
              "Property errors were found.\n"
        r = parsers.parse_netgen(json.dumps(bad), out, "top")
        self.assertFalse(r["matched"])
        self.assertEqual(r["property_errors"], 1)
        self.assertTrue(any("device property mismatches" in x for x in r["problems"]), r["problems"])
        # Either signal alone is enough.
        r = parsers.parse_netgen(json.dumps(bad), "Final result: Circuits match uniquely.\n", "top")
        self.assertFalse(r["matched"])
        r = parsers.parse_netgen(json.dumps(NETGEN_OK), out, "top")
        self.assertFalse(r["matched"])
        self.assertTrue(any("comp.out reports property errors" in x for x in r["problems"]))
        # An empty properties list is clean.
        ok = json.loads(json.dumps(NETGEN_OK))
        ok[2]["properties"] = []
        self.assertTrue(parsers.parse_netgen(json.dumps(ok), "Final result: Circuits match uniquely.\n", "top")["matched"])


    def test_port_errors_fail_even_when_final_line_matches(self):
        # Labelled floating layout pin (round-2 F1): comp.json drops it from both pin lists.
        out = ("Cell lane_smoke (0) disconnected node: extra\nNetlists match uniquely with port errors.\n"
               "extra                                      |(no matching pin)\n"
               "Final result: Circuits match uniquely.\n")
        r = parsers.parse_netgen(json.dumps(NETGEN_OK), out, "top")
        self.assertFalse(r["matched"])
        self.assertEqual(r["unmatched_pins"], ["layout-only extra"])
        self.assertEqual(r["disconnected_nodes"], [dict(cell="lane_smoke", side=0, node="extra")])
        self.assertEqual(len(r["problems"]), 3, r["problems"])
        for line in ("Netlists match uniquely with port errors.\n", "(no matching pin)   |extra\n",
                     "Cell sky130_fd_sc_hd__and2_2 (0) disconnected node: A\n"):
            r = parsers.parse_netgen(json.dumps(NETGEN_OK), line + "Final result: Circuits match uniquely.\n", "top")
            self.assertFalse(r["matched"], line)
        self.assertEqual(parsers.parse_netgen(json.dumps(NETGEN_OK), "(no matching pin)   |extra\n", "top")
                         ["unmatched_pins"], ["reference-only extra"])

    def test_magic_extract_facts(self):
        log = 'Warning:  Ports "en" and "en_alias" are electrically shorted.\nRR_EXT_DONE x\n'
        self.assertEqual(parsers.parse_magic_extract(log)["shorted_ports"], [["en", "en_alias"]])
        self.assertEqual(parsers.parse_magic_extract("RR_EXT_DONE x\n")["shorted_count"], 0)
        spice = ".subckt inv A Y\n.ends\n.subckt top VDD VSS a\n+ y[0] y[1]\nX1 a y inv\n.ends\n"
        self.assertEqual(parsers.spice_subckt_ports(spice, "top"), ["VDD", "VSS", "a", "y[0]", "y[1]"])
        self.assertIsNone(parsers.spice_subckt_ports(spice, "other"))

    def test_reference_module_names(self):
        text = "// module fake\n/* module hidden */ module \\top$1 (a);\nendmodule\nmodule lane_smoke(input a); endmodule\n"
        self.assertEqual(parsers.reference_module_names(text), ["top$1", "lane_smoke"])


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

    @staticmethod
    def pins(*pairs, status="Match"):
        return [dict(layout=a, schematic=b, status=status) for a, b in pairs]

    def test_lvsdb_verdict(self):
        top_pins = self.pins(("a[0]", "A[0]"), ("a[1]", "A[1]"), ("y", "Y"))
        named = [dict(layout=a, paired=True, schematic=b, status="Match") for a, b in
                 (("a[0]", "A[0]"), ("a[1]", "A[1]"), ("y", "Y"))]
        good = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=3, schematic_pins=3,
                                   pin_pairs=top_pins, named_nets=named),
                              dict(layout="inv", schematic="INV", status="Match",
                                   pin_pairs=self.pins(("A", "A"), ("sky130_gnd", "VNB"), ("$4", None)))])
        r = parsers.lvsdb_verdict(good, "top", (("sky130_gnd", "VNB"),))
        self.assertTrue(r["matched"], r["problems"])
        self.assertEqual((r["child_pins"]["body_pairs"], r["child_pins"]["unnamed"]), (1, 1))
        self.assertFalse(parsers.lvsdb_verdict(good, "top")["matched"], "body pins differ unless allowlisted")
        no_pairs = json.loads(json.dumps(good))
        del no_pairs["circuits"][0]["pin_pairs"]
        self.assertFalse(parsers.lvsdb_verdict(no_pairs, "top", (("sky130_gnd", "VNB"),))["matched"])
        skipped = dict(circuits=[dict(layout="top", schematic="TOP", status="Skipped"),
                                 dict(layout="fill", schematic=None, status="NoMatch")])
        self.assertFalse(parsers.lvsdb_verdict(skipped, "top")["matched"])
        pins = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=2, schematic_pins=3)])
        self.assertFalse(parsers.lvsdb_verdict(pins, "top")["matched"])
        self.assertFalse(parsers.lvsdb_verdict(dict(circuits=[]), "top")["matched"])

    def test_lvsdb_swapped_top_labels_fail_despite_match(self):
        # KLayout pairs symmetric ports by topology and reports Match (F2).
        swapped = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=3, schematic_pins=3,
                                      pin_pairs=self.pins(("a[0]", "A[1]"), ("a[1]", "A[0]"), ("y", "Y")))])
        r = parsers.lvsdb_verdict(swapped, "top")
        self.assertFalse(r["matched"])
        self.assertEqual(r["top_pins"]["name_mismatches"], [["a[0]", "A[1]"], ["a[1]", "A[0]"]])
        unnamed = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=2, schematic_pins=2,
                                      pin_pairs=self.pins(("$1", "A"), ("y", "Y")))])
        self.assertFalse(parsers.lvsdb_verdict(unnamed, "top")["matched"])
        dup = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=2, schematic_pins=2,
                                  pin_pairs=self.pins(("y", "Y"), ("Y", "y")))])
        self.assertFalse(parsers.lvsdb_verdict(dup, "top")["matched"])
        child = dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=1, schematic_pins=1,
                                    pin_pairs=self.pins(("y", "Y"))),
                               dict(layout="nand2", schematic="NAND2", status="Match",
                                    pin_pairs=self.pins(("A", "B"), ("B", "A")))])
        self.assertFalse(parsers.lvsdb_verdict(child, "top")["matched"])
        truncated = dict(good_top := dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=1,
                                                         schematic_pins=1, pin_pairs=self.pins(("y", "Y")),
                                                         named_nets=[dict(layout="y", paired=True, schematic="Y",
                                                                          status="Match")])]),
                         pin_pairs_truncated=True)
        self.assertTrue(parsers.lvsdb_verdict(good_top, "top")["matched"])
        self.assertFalse(parsers.lvsdb_verdict(truncated, "top")["matched"])

    def test_lvsdb_unpaired_labelled_net_and_reference_ports(self):
        pins = self.pins(("a", "A"), ("y", "Y"))
        def top(named, layout_pins=2, schematic_pins=2, pin_pairs=pins):
            return dict(circuits=[dict(layout="top", schematic="TOP", status="Match", layout_pins=layout_pins,
                                       schematic_pins=schematic_pins, pin_pairs=pin_pairs, named_nets=named)])
        clean = [dict(layout="a", paired=True, schematic="A", status="Match"),
                 dict(layout="y", paired=True, schematic="Y", status="Match")]
        self.assertTrue(parsers.lvsdb_verdict(top(clean), "top", reference_ports=["a", "y"])["matched"])
        # Extra labelled floating layout net: left out of the pairing, circuit still Match (round-2 F1).
        extra = clean + [dict(layout="extra", paired=False, schematic=None, status=None)]
        r = parsers.lvsdb_verdict(top(extra), "top", reference_ports=["a", "y"])
        self.assertFalse(r["matched"])
        self.assertEqual(r["top_nets"]["unpaired"], ["extra"])
        renamed = [dict(layout="a", paired=True, schematic="Y", status="Match")]
        self.assertFalse(parsers.lvsdb_verdict(top(renamed), "top")["matched"])
        self.assertFalse(parsers.lvsdb_verdict(top(None), "top")["matched"], "named nets must be dumped")
        # Reference port dropped by schematic simplification (round-2 F2): 2 == 2 pins, still a failure.
        r = parsers.lvsdb_verdict(top(clean), "top", reference_ports=["a", "y", "extra"])
        self.assertFalse(r["matched"])
        self.assertEqual(r["reference_ports"]["unpaired_reference_ports"], ["extra"])
        r = parsers.lvsdb_verdict(top(clean), "top", reference_ports=["a"])
        self.assertEqual(r["reference_ports"]["schematic_pins_not_ports"], ["y"])
        self.assertTrue(parsers.lvsdb_verdict(top(clean), "top", reference_ports=["A", "Y"])["matched"],
                        "the SPICE reader upper-cases names; port comparison is case-insensitive")

    def test_port_labels_and_text_comparison(self):
        texts = [["70/5", "a", 1000, 2000], ["69/5", "VDD", 0, 0], ["69/16", "VDD", 5, 5]]
        self.assertTrue(parsers.port_label_check(texts, ["a", "VDD"])["matched"])
        r = parsers.port_label_check(texts + [["70/5", "extra", 7, 7]], ["a", "VDD"])
        self.assertEqual((r["matched"], r["extra_labels"]), (False, ["extra"]))
        r = parsers.port_label_check(texts, ["a", "VDD", "en"])
        self.assertEqual((r["matched"], r["missing_labels"]), (False, ["en"]))
        self.assertTrue(parsers.compare_texts(texts, list(reversed(texts)))["identical"])
        r = parsers.compare_texts(texts, [["70/5", "ax", 1000, 2000]] + texts[1:])
        self.assertEqual((r["identical"], r["only_in_design"], r["only_in_against"]), (False, 1, 1))
        self.assertFalse(parsers.compare_texts(texts, texts + [texts[0]])["identical"], "multiset, not set")
        with tempfile.TemporaryDirectory() as d:
            a, b = Path(d) / "a.tsv", Path(d) / "b.tsv"
            a.write_text("1\tx\n2\ty\n3\tz\n")
            b.write_text("1\tx\n2\tyy\n3\tz\n3\tz\n")
            r = parsers.diff_sorted_lines(a, b)
            self.assertEqual((r["only_in_design"], r["only_in_against"]), (1, 2))
            self.assertEqual(r["samples_only_in_design"], ["2\ty"])
            b.write_text("2\tx\n1\tx\n")
            with self.assertRaises(parsers.ParseError):
                parsers.diff_sorted_lines(a, b)

    def test_planted_gap_edge_pair(self):
        bump, target = [58.945, 28.7, 60.38, 28.96], [60.58, 28.005, 61.17, 29.045]
        exact = dict(values=["edge-pair: (60.58,28.586;60.58,29.045)|(60.38,28.96;60.38,28.7)"])
        notch = dict(values=["edge-pair: (59.32,28.96;59.325,28.96)|(59.392,29.18;59.32,29.18)"])
        above = dict(values=["edge-pair: (59.32,28.96;59.897,28.96)|(59.83,29.18;59.32,29.18)"])
        self.assertEqual(parsers.planted_gap_hits([exact, notch, above], bump, target, 0.2), exact["values"])
        self.assertEqual(parsers.planted_gap_hits([notch, above], bump, target, 0.2), [])
        with self.assertRaises(parsers.ParseError):
            parsers.planted_gap_hits([exact], bump, target, 0.3)
        self.assertIsNone(parsers.parse_edge_pair("polygon: (1,2;3,4;5,6)"))

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
        both = [dict(pdk="sky130", signoff_statuses=statuses), dict(pdk="gf180", signoff_statuses=self.passed())]
        gate = flow_status.overall(both, "passed", True, True)
        self.assertEqual(gate["status"], "partial")
        self.assertEqual(flow_status.EXIT[gate["status"]], 3)
        self.assertEqual({b["item"] for b in gate["blocking"]}, {"gds", "drc_magic", "lvs_netgen"})
        gate = flow_status.overall([dict(pdk="sky130"), dict(pdk="gf180", signoff_statuses=self.passed())],
                                   "passed", True, True)
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
        self.assertEqual(flow_status.overall(pdks[:1], "passed", True, True)["status"], "partial",
                         "a single-PDK summary is never a pass")
        self.assertEqual(flow_status.overall(pdks + pdks[:1], "passed", True, True)["status"], "partial")

    def test_definitive_required_failure_is_failed(self):
        statuses = self.passed()
        statuses["drc_klayout"] = flow_status.item("failed")
        statuses["timing_ss_max_setup"] = flow_status.item("missing")
        gate = flow_status.overall([dict(pdk="gf180", signoff_statuses=statuses)], "passed", True, True)
        self.assertEqual(gate["status"], "failed")
        self.assertEqual(flow_status.EXIT[gate["status"]], 1)
        self.assertEqual(gate["failed_items"], ["gf180:drc_klayout"])
        statuses["drc_klayout"] = flow_status.item("unverified")
        gate = flow_status.overall([dict(pdk="gf180", signoff_statuses=statuses)], "passed", True, True)
        self.assertEqual(gate["status"], "partial")
        statuses = self.passed()
        statuses["timing_tt_nom"] = flow_status.item("failed")
        gate = flow_status.overall([dict(pdk="gf180", signoff_statuses=statuses)], "partial", True, True)
        self.assertEqual(gate["status"], "partial", "TT is reported only; post-route timing partial is not proof")

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
        lef = root / "libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef"
        lef.mkdir(parents=True)
        (lef / "gf180mcu_fd_sc_mcu7t5v0.lef").write_text("VERSION 5.7 ;\n" + "".join(
            f"MACRO gf180mcu_fd_sc_mcu7t5v0__{c}\n  CLASS CORE ;\nEND gf180mcu_fd_sc_mcu7t5v0__{c}\n"
            for c in ("and2_2", "and2_4", "clkbuf_1", "filltie", "mux2_1", "sdffq_1", "dlyb_1", "oai31_4", "oai211_1"))
            + "END LIBRARY\n")
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

    def test_gf180_drv_limited_families_are_synthesis_dont_use(self):
        # Owner decision 2026-10-24: oai31 (and nine other families) cannot meet 1.0 ns at ss.
        drv = self.root / "drv.v"
        drv.write_text(self.mapped.read_text().replace("and2_2", "oai31_4"))
        r = check_cell_policy.check("gf180", "synth", [drv], None, None, self.root)
        self.assertEqual(r["status"], "failed", r)
        self.assertEqual(r["violations"][0]["glob"], "gf180mcu_fd_sc_mcu7t5v0__oai31_*")
        ok = self.root / "oai211.v"  # oai211 is not DRV-limited (no glob may over-match)
        ok.write_text(self.mapped.read_text().replace("and2_2", "oai211_1"))
        self.assertEqual(check_cell_policy.check("gf180", "synth", [ok], None, None, self.root)["status"], "passed")
        self.assertEqual(check_cell_policy.OWNER_DRV_LIMITED["sky130"], [])

    def test_physical_stage_allows_inserted_cts_and_tap_cells_only(self):
        r = check_cell_policy.check("gf180", "physical", [self.final], self.mapped, None, self.root)
        self.assertEqual(r["status"], "passed", r)
        bad = self.root / "bad_final.v"
        bad.write_text(self.final.read_text().replace("clkbuf_1 rr_clkbuf_0", "sdffq_1 rr_extra_0"))
        r = check_cell_policy.check("gf180", "physical", [bad], self.mapped, None, self.root)
        self.assertEqual(r["violations"][0]["rule"], "excluded_cell_added_after_synthesis")
        r = check_cell_policy.check("gf180", "physical", [self.final], None, None, self.root)
        self.assertEqual(r["status"], "unverified")

    def test_attribute_and_parameter_instances_are_seen(self):
        p = "gf180mcu_fd_sc_mcu7t5v0__"
        for form in (f"(* keep *) {p}mux2_1 xattr (.I0(a), .Z(y));",
                     f"{p}mux2_1 #(.X(1)) xparam (.I0(a), .Z(y));",
                     f"{p}mux2_1 xmulti0 (.I0(a)), xmulti [1:0] (.I0(a));",
                     f"\\{p}mux2_1  \\x;esc  (.I0(a), .Z(y));"):
            bad = self.root / "form.v"
            bad.write_text(f"module t(input a, output y);\n{form}\nendmodule\n")
            r = check_cell_policy.check("gf180", "synth", [bad], None, None, self.root)
            self.assertEqual(r["status"], "failed", (form, r))
            self.assertEqual(r["violations"][0]["cell"], f"{p}mux2_1")

    def test_unrecognised_statements_are_unverified_not_passed(self):
        p = "gf180mcu_fd_sc_mcu7t5v0__"
        for extra in ("always @(posedge a) y <= a;", "`define CELL mux2_1", f"{p}and2_2 (.A1(a));",
                      "generate for (i = 0; i < 2; i = i + 1) begin end endgenerate"):
            odd = self.root / "odd.v"
            odd.write_text(self.mapped.read_text().replace("endmodule", extra + "\nendmodule"))
            r = check_cell_policy.check("gf180", "synth", [odd], None, None, self.root)
            self.assertEqual(r["status"], "unverified", (extra, r))
            self.assertTrue(any("unrecognised statement" in x for x in r["problems"]))
        ok = self.root / "ok.v"
        ok.write_text("`timescale 1ns/1ps\n/* c */ module t(input a, output y); // x\n wire \\q[0] ;\n"
                      f" assign y = a;\n {p}and2_2 g0 (.A1(a), .A2(1'b0), .Z(\\q[0] ));\nendmodule\n")
        r = check_cell_policy.check("gf180", "synth", [ok], None, None, self.root)
        self.assertEqual(r["status"], "passed", r)
        self.assertEqual(check_cell_policy.scan(ok.read_text())["instances"], [(f"{p}and2_2", "g0")])

    def test_unknown_cells_are_unverified_not_passed(self):
        # Round-2 F5: a black-box wrapper or an unknown (e.g. upper-case) cell name could hide anything.
        p = "gf180mcu_fd_sc_mcu7t5v0__"
        for extra in ("hidden_wrapper u_hw (.A(a), .X());", f"{p.upper()}MUX2_1 u0 (.I0(a), .Z(y));"):
            odd = self.root / "unknown.v"
            odd.write_text(self.mapped.read_text().replace("endmodule", extra + "\nendmodule"))
            r = check_cell_policy.check("gf180", "synth", [odd], None, None, self.root)
            self.assertEqual(r["status"], "unverified", (extra, r))
            self.assertTrue(any("neither library cells" in x for x in r["problems"]), r["problems"])
        # A submodule defined in the scanned files is fine (and is scanned itself).
        hier = self.root / "hier.v"
        hier.write_text(f"module leaf(input a, output y);\n {p}and2_2 g (.A1(a), .A2(a), .Z(y));\nendmodule\n"
                        "module t(input a, output y);\n leaf u (.a(a), .y(y));\nendmodule\n")
        self.assertEqual(check_cell_policy.check("gf180", "synth", [hier], None, None, self.root)["status"], "passed")
        (self.root / "libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef").unlink()
        r = check_cell_policy.check("gf180", "synth", [self.mapped], None, None, self.root)
        self.assertEqual(r["status"], "unverified", "no cell LEF: nothing can be certified")

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

    def test_incomplete_manifest_is_unverified_without_running_anything(self):
        # Round-2 F7: a missing required entry must give unverified, never a KeyError/crash.
        m = manifest.load(SCRIPTS.parent / "fixtures/flow/signoff-pinned.json")
        tools = sorted(manifest.TOOL_SPEC) + ["openroad", "opensta"]
        self.assertEqual(manifest.structure_mismatches(m, ["sky130", "gf180"], tools), [])
        cases = []
        for mutate in (lambda x: x["pdks"]["sky130"]["files"].pop("klayout_lvs"),
                       lambda x: x["pdks"]["gf180"]["trees"].pop("klayout_drc_tree"),
                       lambda x: x["pdks"]["gf180"].__setitem__("container_root", "/elsewhere"),
                       lambda x: x["pdks"]["sky130"]["files"]["magicrc"].__setitem__("path", "other.magicrc"),
                       lambda x: x["tools"]["magic"]["files"].pop("/foss/tools/bin/magic"),
                       lambda x: x["tools"].pop("netgen"),
                       lambda x: x["images"]["iic_osic_tools"].__setitem__("id", "0" * 64),
                       lambda x: x.pop("pdks")):
            copy = json.loads(json.dumps(m))
            mutate(copy)
            cases.append(manifest.structure_mismatches(copy, ["sky130", "gf180"], tools))
        self.assertTrue(all(cases), cases)
        with tempfile.TemporaryDirectory() as d:
            copy = json.loads(json.dumps(m))
            del copy["pdks"]["sky130"]["files"]["klayout_lvs"]
            path = Path(d) / "nolvs.json"
            path.write_text(json.dumps(copy))
            r = manifest.verify(path, Path(d) / "out", ["sky130"], ["klayout"])
            self.assertEqual(r["status"], "unverified")
            self.assertTrue(r["mismatches"][0].startswith("manifest incomplete"))
            self.assertEqual(r["receipts"], [], "nothing is probed for an incomplete manifest")


class SignoffHarnessTest(unittest.TestCase):
    """Harness behaviour that needs no container: identity failures and tool-run failures."""

    def test_run_incomplete_codes(self):
        base = dict(exit_code=0, timed_out=False, host_error=None, time={"inner_exit_status": 0})
        self.assertIsNone(signoff.run_incomplete(base))
        self.assertIsNone(signoff.run_incomplete(dict(base, exit_code=1)), "exit codes never pass a verdict; 1 is parsed")
        for code in (124, 125, 127, 137, 139):
            self.assertIn("incomplete", signoff.run_incomplete(dict(base, exit_code=code)))
        self.assertIn("signal 9", signoff.run_incomplete(dict(base, exit_code=137)))
        self.assertIn("did not finish", signoff.run_incomplete(dict(base, exit_code=None, host_error="x")))

    def test_oom_killed_tool_reports_null_counts_not_zero(self):
        # Round-2 F4: an unverified verdict must never carry counts.total = 0.
        with tempfile.TemporaryDirectory() as d:
            session = type("S", (), {})()
            session.pdk, session.gds, session.top, session.netlist = "sky130", None, "top", None
            session.against, session.against_top, session.out = None, None, Path(d)
            session.manifest_path, session.m = Path(d) / "m.json", None
            session.identity = dict(status="verified", mismatches=[])
            session.preflight_summary = lambda: dict(ran=False)
            v = signoff.Verdict("antenna", session)
            v.run(dict(label="antenna_magic_0", exit_code=137, timed_out=False, host_error=None, wall_seconds=0.5,
                       peak_rss_kib=24000, stdout=str(Path(d) / "r" / "stdout.txt")))
            v.data["counts"] = dict(total=0, by_rule={}, unit="Magic violations (by plane)")
            out = v.finish()
            self.assertEqual(out["status"], "unverified")
            self.assertIsNone(out["counts"]["total"])
            self.assertIsNone(out["counts"]["by_rule"])
            on_disk = json.loads((Path(d) / "antenna.json").read_text())
            self.assertIsNone(on_disk["counts"]["total"])
            ok = signoff.Verdict("drc-magic", session)
            ok.data["counts"] = dict(total=0, by_rule={})
            self.assertEqual(ok.finish()["counts"]["total"], 0, "a definitive clean result keeps its 0")

    def test_bad_or_incomplete_manifest_gives_unverified_exit_3(self):
        m = manifest.load(SCRIPTS.parent / "fixtures/flow/signoff-pinned.json")
        with tempfile.TemporaryDirectory() as d:
            gds = Path(d) / "x.gds"
            gds.write_bytes(b"\0" * 16)
            copy = json.loads(json.dumps(m))
            del copy["pdks"]["sky130"]["files"]["klayout_lvs"]
            (Path(d) / "nolvs.json").write_text(json.dumps(copy))
            for name in ("missing.json", "nolvs.json"):
                out = Path(d) / name.replace(".", "_")
                code = signoff.main(["all", "--gds", str(gds), "--top", "lane_smoke", "--pdk", "sky130",
                                     "--out", str(out), "--manifest", str(Path(d) / name)])
                self.assertEqual(code, 3, name)
                for check in ("drc_magic", "drc_klayout", "antenna", "density", "lvs_netgen", "lvs_klayout"):
                    verdict = json.loads((out / f"{check}.json").read_text())
                    self.assertEqual(verdict["status"], "unverified", (name, check))
                    self.assertIsNone((verdict["counts"] or {}).get("total"))

    def test_harness_crash_is_unverified_not_failed(self):
        with tempfile.TemporaryDirectory() as d:
            gds = Path(d) / "x.gds"
            gds.write_bytes(b"\0" * 16)
            real = signoff.run_checks
            def boom(*_a, **_k):
                raise KeyError("simulated")
            signoff.run_checks = boom
            try:
                code = signoff.main(["drc-klayout", "--gds", str(gds), "--top", "t", "--pdk", "gf180", "--out", d])
            finally:
                signoff.run_checks = real
            self.assertEqual(code, 3)
            self.assertEqual(json.loads((Path(d) / "drc_klayout.json").read_text())["status"], "unverified")


if __name__ == "__main__":
    unittest.main()
