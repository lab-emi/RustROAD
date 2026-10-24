# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the RC oracle helpers (plain unittest; no podman, no PDK tools).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

from pathlib import Path
import random
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPTS = HERE.parent
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(SCRIPTS / "signoff"))

import gds_records as gds  # noqa: E402
import magic_rc  # noqa: E402
import rc_oracles  # noqa: E402

EXT = """timestamp 0
version 8.3
tech sky130A
style ngspice()
scale 1000 1 500000
use sky130_fd_sc_hd__buf_1 sky130_fd_sc_hd__buf_1_0 1 0 2000 0 1 3000
use sky130_fd_sc_hd__buf_1 sky130_fd_sc_hd__buf_1_1 1 0 4000 0 -1 3544
node "a" 10 100.5 0 0 m1 0 0
node "b" 10 50.25 0 0 m1 0 0
node "VSS" 1 900 0 0 m1 0 0
equiv "a" "sky130_fd_sc_hd__buf_1_0.X"
cap "a" "b" 3.5
cap "b" "VSS" 1.25
cap "a" "VSS" 2
"""

RES_EXT = """scale 1000 1 500000
rnode "RRT_1_0/P" 0 0 100 200 0
rnode "n.n0" 0 0 150 200 0
rnode "n" 0 0 300 200 0
resist "RRT_1_0/P" "n.n0" 15.7
resist "n.n0" "n" 4.5
rnode "RRT_2_0/P" 0 0 900 900 0
rnode "m" 0 0 950 900 0
resist "RRT_2_0/P" "m" 1.25
"""

SPEF = """*SPEF "IEEE 1481-1999"
*DESIGN "t"
*T_UNIT 1 NS
*C_UNIT 1 PF
*R_UNIT 1 OHM
*L_UNIT 1 HENRY
*NAME_MAP
*1 n
*2 m
*3 u0
*4 u1
*D_NET *1 0.003
*CONN
*P a I
*I *3:A I *D buf
*CAP
1 a 0.001
2 *1:1 0.0015
3 *1:1 *2:1 0.0005
*RES
1 a *1:1 10
2 *1:1 *3:A 20
*END
*D_NET *2 0.0015
*CONN
*P b I
*I *4:A I *D buf
*CAP
1 b 0.001
2 *2:1 *1:1 0.0005
*RES
1 b *2:1 5
2 *2:1 *4:A 5
*END
"""


class GdsRecordsTest(unittest.TestCase):
    def test_round_trip_and_editing(self):
        with tempfile.TemporaryDirectory() as tmp:
            src, dst = Path(tmp) / "a.gds", Path(tmp) / "b.gds"
            lib = object.__new__(gds.Library)
            lib.head = [gds.encode(gds.HEADER, gds.INT16, (600).to_bytes(2, "big")),
                        gds.encode(gds.BGNLIB, gds.INT16, bytes(24)),
                        gds.encode(gds.LIBNAME, gds.ASCII, gds.ascii_payload("L"))]
            lib.structures = [gds.structure("CELL", [gds.boundary(68, 20, (0, 0, 10, 20))]),
                              gds.structure("TOP", [gds.sref("CELL", (5, 5)), gds.text(68, 5, (1, 1), "P"),
                                                    gds.boundary(69, 20, (-5, -5, 5, 5))])]
            lib.write(src)
            back = gds.Library(src)
            self.assertEqual(back.names(), ["CELL", "TOP"])
            top = back.structures[1]
            self.assertEqual([e["kind"] for e in top["elements"]], [gds.SREF, gds.TEXT, gds.BOUNDARY])
            self.assertEqual(top["elements"][0]["sname"], "CELL")
            back.write(dst)
            self.assertEqual(src.read_bytes(), dst.read_bytes())

    def test_truncated_stream_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bad.gds"
            path.write_bytes(gds.encode(gds.HEADER, gds.INT16, (600).to_bytes(2, "big"))[:3])
            with self.assertRaises(gds.GdsError):
                gds.Library(path)


class MagicParsingTest(unittest.TestCase):
    def ext(self, text):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "t.ext"
            path.write_text(text)
            return magic_rc.parse_ext(path)

    def test_node_totals_resolve_aliases_and_ignore_record_order(self):
        ext = self.ext(EXT)
        self.assertEqual(ext["style"], "ngspice()")
        totals, partners = magic_rc.node_totals(ext)
        self.assertAlmostEqual(totals["a"], 100.5 + 3.5 + 2.0)
        self.assertAlmostEqual(totals["b"], 50.25 + 3.5 + 1.25)
        self.assertAlmostEqual(partners["a"]["b"], 3.5)
        self.assertEqual(magic_rc.canonical_map(ext)["sky130_fd_sc_hd__buf_1_0.X"], "a")
        lines = EXT.splitlines()
        caps = [line for line in lines if line.startswith("cap")]
        rest = [line for line in lines if not line.startswith("cap")]
        random.Random(7).shuffle(caps)
        shuffled, _ = magic_rc.node_totals(self.ext("\n".join(rest + caps) + "\n"))
        self.assertEqual(shuffled, totals)

    def test_instances_map_by_placed_corner_including_flipped_rows(self):
        ext = self.ext(EXT)
        comps = {"u0": dict(master="sky130_fd_sc_hd__buf_1", x=10000, y=15000, orient="N"),
                 "u1": dict(master="sky130_fd_sc_hd__buf_1", x=20000, y=15000, orient="FS")}
        sizes = {"sky130_fd_sc_hd__buf_1": (1.38, 2.72)}
        got = magic_rc.map_instances(ext["uses"], comps, sizes, 1000)
        self.assertEqual(got, {"sky130_fd_sc_hd__buf_1_0": "u0", "sky130_fd_sc_hd__buf_1_1": "u1"})
        comps["u1"]["y"] = 15005
        with self.assertRaises(magic_rc.MagicError):
            magic_rc.map_instances(ext["uses"], comps, sizes, 1000)

    def test_resistance_blocks_keep_points(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "t.res.ext"
            path.write_text(RES_EXT)
            blocks = magic_rc.parse_res_ext(path)
        self.assertEqual(len(blocks), 2)
        self.assertEqual(blocks[0]["points"][0], ("RRT_1_0/P", 100, 200))
        self.assertAlmostEqual(sum(r for _, _, r in blocks[0]["resistors"]), 20.2)
        self.assertEqual(blocks[1]["resistors"], [("RRT_2_0/P", "m", 1.25)])


class BackannotationTest(unittest.TestCase):
    def test_per_net_totals_scale_native_trees(self):
        with tempfile.TemporaryDirectory() as tmp:
            src, dst = Path(tmp) / "n.spef", Path(tmp) / "o.spef"
            src.write_text(SPEF)
            target = {"n": dict(c_pf=0.006, r_ohm=15.0), "m": dict(c_pf=0.0015, r_ohm=20.0)}
            factors = magic_rc.backannotate(src, target, dst)
            self.assertAlmostEqual(factors["n"]["c"], 2.0)
            self.assertAlmostEqual(factors["n"]["r"], 0.5)
            got = rc_oracles.per_net_spef(dst)
            self.assertAlmostEqual(got["n"]["c_pf"], 0.006)
            self.assertAlmostEqual(got["n"]["r_ohm"], 15.0)
            self.assertAlmostEqual(got["m"]["r_ohm"], 20.0)
            self.assertEqual(got["n"]["coupling_pf"], 0.0)  # couplings grounded at their own node


class TargetsTest(unittest.TestCase):
    def test_targets_follow_the_fixed_bounds(self):
        native = {"a": dict(c_pf=0.0060, coupling_pf=0.0, r_ohm=30.0),
                  "b": dict(c_pf=0.0010, coupling_pf=0.0, r_ohm=10.0)}
        oracle = {"a": dict(c_pf=0.0050, coupling_pf=0.0, r_ohm=24.0),
                  "b": dict(c_pf=0.0020, coupling_pf=0.0, r_ohm=10.0)}
        t = rc_oracles.compare_totals(native, oracle)["targets"]
        self.assertEqual(t["net_c"]["eligible"], 1)          # only nets >= 5 fF
        self.assertEqual(t["net_c"]["status"], "passed")     # +20 % is on the bound
        self.assertEqual(t["net_r"]["eligible"], 1)          # only nets >= 20 ohm
        self.assertEqual(t["net_r"]["status"], "failed")     # +25 %
        self.assertEqual(t["coverage"]["status"], "passed")
        native.pop("b")
        self.assertEqual(rc_oracles.compare_totals(native, oracle)["coverage"]["status"], "failed")


if __name__ == "__main__":
    unittest.main()
