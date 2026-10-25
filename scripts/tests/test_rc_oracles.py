# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the RC oracle helpers (plain unittest; no podman, no PDK tools).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

import json
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


class CampaignHelpersTest(unittest.TestCase):
    def test_legacy_profile_drops_only_the_new_fields(self):
        root = Path(__file__).resolve().parents[2]
        src = root / "fixtures/pdk/sky130-rc-max.json"
        with tempfile.TemporaryDirectory() as tmp:
            dst = rc_oracles.legacy_profile(src, Path(tmp) / "legacy.json")
            new, old = json.loads(src.read_text()), json.loads(dst.read_text())
        self.assertTrue(all("overlap_area" in l and "area_cap_pf_per_um2" in l for l in new["layers"]))
        self.assertFalse(any("overlap_area" in l or "area_cap_pf_per_um2" in l for l in old["layers"]))
        self.assertNotIn("overlap_area", old["model"])
        for a, b in zip(old["layers"], new["layers"]):
            self.assertEqual(a["contexts"], b["contexts"])
            self.assertEqual(a["sheet_resistance_ohm_per_square"], b["sheet_resistance_ohm_per_square"])
        self.assertEqual(old["cuts"], new["cuts"])

    def test_lef_obs_blocks_are_removed_and_counted(self):
        lef = ("MACRO a\n PIN A\n  PORT\n   LAYER li1 ;\n  END\n END A\n OBS\n  LAYER li1 ;\n"
               "  RECT 0 0 1 1 ;\n END\nEND a\nMACRO b\n OBS\n  LAYER met1 ;\n END\nEND b\n")
        with tempfile.TemporaryDirectory() as tmp:
            src, dst = Path(tmp) / "in.lef", Path(tmp) / "out.lef"
            src.write_text(lef)
            self.assertEqual(rc_oracles.strip_lef_obs(src, dst), 2)
            text = dst.read_text()
        self.assertNotIn("OBS", text)
        self.assertNotIn("RECT 0 0 1 1", text)
        self.assertIn(" PIN A", text)
        self.assertIn("END a", text)
        self.assertIn("END b", text)

    def test_design_total_counts_each_coupling_once_and_positions(self):
        nets = {"a": dict(c_pf=3.0, coupling_pf=2.0), "b": dict(c_pf=5.0, coupling_pf=2.0)}
        self.assertAlmostEqual(rc_oracles.design_total(nets), 6.0)
        self.assertEqual(rc_oracles.position(2.0, 1.0, 3.0), "between")
        self.assertEqual(rc_oracles.position(0.5, 3.0, 1.0), "below_both")
        self.assertEqual(rc_oracles.position(4.0, 3.0, 1.0), "above_both")

    def test_signal_pins_shrink_to_dots_and_supply_and_obs_stay(self):
        lef = """MACRO a
  PIN A
    DIRECTION INPUT ;
    USE SIGNAL ;
    PORT
      LAYER li1 ;
        RECT 0.000 0.000 1.000 0.200 ;
    END
  END A
  PIN VGND
    USE GROUND ;
    PORT
      LAYER met1 ;
        RECT 0.000 -0.240 5.000 0.240 ;
    END
  END VGND
  OBS
    LAYER li1 ;
      RECT 2.000 2.000 3.000 3.000 ;
  END
END a
"""
        with tempfile.TemporaryDirectory() as tmp:
            src, dst = Path(tmp) / "in.lef", Path(tmp) / "out.lef"
            src.write_text(lef)
            self.assertEqual(rc_oracles.shrink_lef_pins(src, dst), 1)
            text = dst.read_text()
        self.assertIn("RECT 0.4950 0.0950 0.5050 0.1050 ;", text)
        self.assertIn("RECT 0.000 -0.240 5.000 0.240 ;", text)
        self.assertIn("RECT 2.000 2.000 3.000 3.000 ;", text)
        self.assertNotIn("RECT 0.000 0.000 1.000 0.200", text)

    def test_report_renders_fidelity_with_its_own_bound(self):
        delta = lambda s, h, status="passed": dict(max_abs_setup_delta_ns=s, max_abs_hold_delta_ns=h,  # noqa: E731
                                                    status=status, bound_ns=0.01)
        sta = dict(rc_corner="max", vs_openrcx=delta(0.004, 0.003), vs_openrcx_threshold0=delta(0.002, 0.001),
                   openrcx_threshold_effect=delta(0.003, 0.002), vs_magic=delta(0.03, 0.02, "failed"),
                   openrcx_vs_magic=delta(0.03, 0.02, "failed"), backannotation_fidelity=delta(0.00456, 0.003))
        text = "\n".join(rc_oracles.sta_rows(dict(pdk="gf180", sta=dict(ss=sta))))
        rows = [l for l in text.splitlines() if "fidelity" in l]
        self.assertTrue(rows and rows[0].endswith("| 4.56 / 3.00 | 1.00 | failed |"), rows)
        rows = [l for l in text.splitlines() if "threshold 0, lossless" in l]
        self.assertTrue(rows and rows[0].endswith("| 2.00 / 1.00 | 10.00 | passed |"), rows)


class ProfileDataTest(unittest.TestCase):
    def test_blanket_corner_contact_block_is_not_corner_data(self):
        import build_rc_profile as brp
        nominal = dict(all_contacts=dict(ndc=6300.0, pc=8000.0, m2c=4500.0, m3c=4500.0))
        blanket = dict(all_contacts=dict(ndc=15000.0, pc=15000.0, m2c=15000.0, m3c=15000.0))
        distinct = dict(all_contacts=dict(ndc=280000.0, pc=252000.0, m2c=15000.0, m3c=8000.0))
        zeros = dict(all_contacts=dict(ndc=0.0, pc=0.0, m2c=0.0, m3c=0.0))
        self.assertEqual(brp.corner_contacts_blanket(blanket, nominal), 4)
        self.assertIsNone(brp.corner_contacts_blanket(distinct, nominal))
        self.assertIsNone(brp.corner_contacts_blanket(zeros, nominal))  # handled as "no data" separately

    def test_tracked_gf180_max_keeps_nominal_via_resistance(self):
        root = SCRIPTS.parent / "fixtures/pdk"
        mx = json.loads((root / "gf180-rc-max.json").read_text())
        nom = json.loads((root / "gf180-rc-nom.json").read_text())
        self.assertEqual([c["resistance_ohm_per_cut"] for c in mx["cuts"]],
                         [c["resistance_ohm_per_cut"] for c in nom["cuts"]])
        self.assertTrue(any("blanket" in c["source"] for c in mx["cuts"]))
        sky = json.loads((root / "sky130-rc-max.json").read_text())
        self.assertFalse(any("fallback" in c["source"] for c in sky["cuts"]))


class FieldSolverTest(unittest.TestCase):
    def test_parallel_plate_symmetry_and_monotone_totals(self):
        import field_solver
        self.assertEqual(field_solver.selftest(), 0)

    def test_stacks_parse_from_pdk_files(self):
        import field_solver
        if not field_solver.PDK_ROOT.is_dir():
            self.skipTest("PDK not installed")
        sky, _ = field_solver.load_stack("sky130")
        for got, want in ((sky["met1"], (1.37, 1.73)), (sky["met2"], (2.0, 2.36))):
            self.assertAlmostEqual(got[0], want[0])
            self.assertAlmostEqual(got[1], want[1])
        gf, _ = field_solver.load_stack("gf180")
        self.assertAlmostEqual(gf["Metal2"][0], 2.01)
        self.assertAlmostEqual(gf["Metal2"][1], 2.56)

    def test_two_d_grid_matches_quarter_three_d_for_a_long_wire(self):
        import field_solver as f
        w = ("A", -0.07, 0.07, 1.37, 1.73)
        c2 = f.solver_2d([w], half_width=4.0, height=6.0, hmin=0.02, hmax=0.4).row("A")["A"]
        s3 = f.solver_quarter([("A", -100, 100, -0.07, 0.07, 1.37, 1.73)], 1.0, 4.0, height=6.0,
                              hmin=0.02, hmax=0.4)
        c3 = 4.0 * s3.row("A")["A"] / 2.0
        self.assertLess(abs(c3 / c2 - 1.0), 0.01, (c2, c3))


if __name__ == "__main__":
    unittest.main()
