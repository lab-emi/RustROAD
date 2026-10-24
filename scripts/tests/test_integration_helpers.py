# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the stream-b0 DEF/GDS helpers (plain unittest; no podman, no PDK tools).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

from pathlib import Path
import struct
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(SCRIPTS / "signoff"))

import compare_spef  # noqa: E402
import def_segmentation  # noqa: E402
import timing_baseline  # noqa: E402
import verify_gds  # noqa: E402
import verify_router_signoff  # noqa: E402

HEAD = "VERSION 5.8 ;\nDESIGN lane_smoke ;\nUNITS DISTANCE MICRONS 1000 ;\n"
TAIL = "SPECIALNETS 0 ;\nEND SPECIALNETS\nEND DESIGN\n"


def nets(body: str, count: int = 1) -> str:
    return HEAD + f"NETS {count} ;\n" + body + "\nEND NETS\n" + TAIL


EDGES = nets("""- n1 ( a X ) ( b A ) + USE SIGNAL
  + ROUTED met2 ( 100 0 ) ( 100 200 )
  NEW met2 ( 100 200 ) ( 100 400 )
  NEW met2 ( 100 400 ) ( 100 900 )
  NEW met1 ( 100 900 ) M1M2_PR
  NEW met2 ( 100 300 ) RECT ( 0 0 140 70 ) ;""")
RUNS = nets("""- n1 ( a X ) ( b A ) + USE SIGNAL
  + ROUTED met2 ( 100 0 ) ( 100 900 )
  NEW met1 ( 100 900 ) M1M2_PR
  NEW met2 ( 100 300 ) RECT ( 0 0 140 70 ) ;""")


class DefSegmentationTest(unittest.TestCase):
    def write(self, d: Path, name: str, text: str) -> Path:
        path = d / name
        path.write_text(text)
        return path

    def test_text_check_accepts_merged_paths_and_rejects_changed_vias_or_patches(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            old, new = self.write(d, "old.def", EDGES), self.write(d, "new.def", RUNS)
            r = def_segmentation.text_check(old, new)
            self.assertEqual(r["status"], "passed", r)
            self.assertEqual((r["old_wire_paths"], r["new_wire_paths"], r["vias"], r["rect_patches"]), (3, 1, 1, 1))
            moved = self.write(d, "moved.def", RUNS.replace("RECT ( 0 0 140 70 )", "RECT ( 0 0 140 80 )"))
            self.assertEqual(def_segmentation.text_check(old, moved)["status"], "failed")
            via = self.write(d, "via.def", RUNS.replace("M1M2_PR", "M1M2_PR_MR"))
            self.assertEqual(def_segmentation.text_check(old, via)["status"], "failed")
            header = self.write(d, "header.def", RUNS.replace("DESIGN lane_smoke", "DESIGN other"))
            self.assertEqual(def_segmentation.text_check(old, header)["status"], "failed")

    def test_delete_longest_path_keeps_a_valid_net_statement(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            src = self.write(d, "new.def", RUNS)
            dst = d / "bad.def"
            info = def_segmentation.delete_longest_path(src, dst)
            self.assertEqual(info["length_dbu"], 900)
            text = dst.read_text()
            self.assertNotIn("( 100 0 ) ( 100 900 )", text)
            self.assertIn("+ ROUTED met1 ( 100 900 ) M1M2_PR", text)
            self.assertTrue(text.split("\nEND NETS")[0].rstrip().endswith(";"))
            def_segmentation.parse_nets(def_segmentation.nets_section(text)[1])


class MergeDefTest(unittest.TestCase):
    def test_merge_def_keeps_rect_patches_and_merges_edges(self):
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            (d / "in.def").write_text(EDGES)
            routes = dict(layers=["met1", "met2"], routes=dict(nets=[0], access=[
                dict(net=0, layer=1, at=[100, 0], pin=0)]))
            (d / "routes.json").write_text(__import__("json").dumps(routes))
            r = compare_spef.merge_def(d / "in.def", d / "routes.json", d / "out.def")
            self.assertEqual((r["original_wire_paths"], r["merged_wire_paths"], r["rect_patches"]), (3, 1, 1))
            out = (d / "out.def").read_text()
            self.assertIn("NEW met2 ( 100 300 ) RECT ( 0 0 140 70 )", out)
            self.assertIn("met2 ( 100 0 ) ( 100 900 )", out)


class GdsHelpersTest(unittest.TestCase):
    def test_boundary_record_round_trips_through_the_independent_walker(self):
        rec = verify_gds.boundary_record(69, 20, (-5000, -5000, -4900, -4900))
        # Minimal library: HEADER, BGNLIB, LIBNAME, UNITS, BGNSTR, STRNAME, ENDSTR, ENDLIB.
        def r(t, payload=b""):
            return struct.pack(">HH", 4 + len(payload), t) + payload
        lib = (r(0x0002, struct.pack(">h", 600)) + r(0x0102, b"\0" * 24) + r(0x0206, b"lib\0")
               + r(0x0305, b"\0" * 16) + r(0x0502, b"\0" * 24) + r(0x0606, b"top\0") + r(0x0700) + r(0x0400))
        out = verify_gds.insert_into_top(lib, "top", rec)
        elements = verify_gds.top_elements(out, "top")
        self.assertEqual(len(elements), 1)
        e = elements[0]
        self.assertEqual((e["kind"], e["layer"], e["datatype"]), (0x08, 69, 20))
        self.assertEqual(e["xy"][0], (-5000, -5000))
        self.assertEqual(e["xy"][2], (-4900, -4900))
        self.assertEqual(list(verify_gds.structures(out)), ["top"])

    def test_def_patches_divides_exactly_or_refuses(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "p.def"
            path.write_text(RUNS.replace("( 100 300 ) RECT ( 0 0 140 70 )", "( 100 300 ) RECT ( 0 0 140 80 )"))
            self.assertEqual(verify_gds.def_patches(path, 1), [("met2", (100, 300, 240, 380))])
            self.assertEqual(verify_gds.def_patches(path, 2), [("met2", (50, 150, 120, 190))])
            path.write_text(RUNS.replace("RECT ( 0 0 140 70 )", "RECT ( 0 0 140 71 )"))
            with self.assertRaises(RuntimeError):
                verify_gds.def_patches(path, 2)


class RouterControlTest(unittest.TestCase):
    def test_planted_control_uses_a_vertical_access_wire_anywhere_in_the_net(self):
        text = nets("""- n1 ( a X ) ( b A ) + USE SIGNAL
  + ROUTED met1 ( 0 0 ) ( 5000 0 )
  NEW met2 ( 100 0 ) ( 100 900 ) ;""")
        bad = verify_router_signoff.planted(text, "sky130", "notch")
        # Island half a spacing right of the wire, inserted right after it.
        lines = bad.splitlines()
        i = next(k for k, line in enumerate(lines) if "RECT ( 0 0 140 560 )" in line)
        self.assertTrue(lines[i].startswith("  NEW met2 ( 240 0 ) RECT"))
        self.assertEqual(lines[i - 1].strip(), "NEW met2 ( 100 0 ) ( 100 900 )")
        self.assertTrue(bad.split("\nEND NETS")[0].rstrip().endswith(";"))
        short = nets("""- n1 ( a X ) ( b A ) + USE SIGNAL
  + ROUTED met2 ( 100 0 ) ( 100 300 ) ;""")
        with self.assertRaises(RuntimeError):
            verify_router_signoff.planted(short, "sky130", "notch")


class TimingSummaryTest(unittest.TestCase):
    def test_corner_summary_parity_margins_and_missing_endpoints(self):
        rust = {"r0/D": dict(setup=1.0, hold=-0.01), "r1/D": dict(setup=2.0, hold=0.04)}
        ref = {"r0/D": dict(setup=1.0004, hold=-0.0101), "r1/D": dict(setup=2.0, hold=0.0402)}
        s = timing_baseline.corner_summary(rust, ref)
        self.assertEqual(s["parity"], "passed")
        self.assertEqual(s["ruststa"]["hold_violating"], ["r0/D"])
        self.assertEqual(s["ruststa"]["hold_below_margin"], ["r0/D", "r1/D"])
        self.assertAlmostEqual(s["ruststa"]["worst_hold_ns"], -0.01)
        self.assertEqual(len(s["hold_endpoints"]), 2)
        ref["r1/D"]["hold"] = 0.0420
        self.assertEqual(timing_baseline.corner_summary(rust, ref)["parity"], "failed")
        del ref["r1/D"]
        s = timing_baseline.corner_summary(rust, ref)
        self.assertEqual((s["parity"], s["missing"]), ("failed", ["r1/D"]))


if __name__ == "__main__":
    unittest.main()
