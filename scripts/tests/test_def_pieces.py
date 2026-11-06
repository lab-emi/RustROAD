"""def_pieces.py: runs, pieces, crossings, trail bound and causes on hand-checked DEFs."""
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import def_pieces as dp

LEF = '''LAYER M1
  TYPE ROUTING ;
  DIRECTION HORIZONTAL ;
  PITCH 0.42 ;
  WIDTH 0.16 ;
END M1
LAYER V1
  TYPE CUT ;
END V1
LAYER M2
  TYPE ROUTING ;
  DIRECTION VERTICAL ;
  PITCH 0.42 ;
  WIDTH 0.2 ;
END M2
LAYER V2
  TYPE CUT ;
END V2
LAYER M3
  TYPE ROUTING ;
  DIRECTION HORIZONTAL ;
  PITCH 0.42 ;
  WIDTH 0.2 ;
END M3
LAYER V3
  TYPE CUT ;
END V3
LAYER M4
  TYPE ROUTING ;
  DIRECTION VERTICAL ;
  PITCH 0.42 ;
  WIDTH 0.2 ;
END M4
VIA V12 DEFAULT
  LAYER M1 ;
    RECT -0.1 -0.1 0.1 0.1 ;
  LAYER V1 ;
    RECT -0.05 -0.05 0.05 0.05 ;
  LAYER M2 ;
    RECT -0.1 -0.1 0.1 0.1 ;
END V12
'''

# a: an M2 run from 0 to 4200 written as two segments split at a via landing (one run, two
#    pieces), plus a straight M2 run with a via landing strictly inside it, written in its own
#    statement (one run, two pieces); b: an M2 staircase of five runs (vertical, jog, vertical,
#    jog, vertical) with a via at each end; c: unrouted.
DEF = '''VERSION 5.8 ;
DESIGN t ;
UNITS DISTANCE MICRONS 1000 ;
NETS 3 ;
- a ( u1 A ) ( u2 Y ) ( u3 B ) + USE SIGNAL
  + ROUTED M1 ( 0 0 ) V12 ( 0 2100 )
  NEW M2 ( 0 2100 ) ( 0 4200 )
  NEW M1 ( 0 2100 ) V12
  NEW M2 ( 840 0 ) ( 840 4200 )
  NEW M1 ( 840 2100 ) V12
  NEW M1 ( 0 2100 ) RECT ( 0 0 100 100 ) ;
- b ( u4 A ) ( u5 Y ) + USE SIGNAL
  + ROUTED M1 ( 2100 0 ) V12 ( 2100 840 ) ( 2520 * ) ( * 1680 ) ( 2940 * ) ( * 2520 ) V12 ;
- c ( u6 A ) ( u7 Y ) + USE SIGNAL ;
END NETS
END DESIGN
'''


# One net per cause and DEF construct, written LibreLane-style (indented entries, '*'
# coordinates). z: an M2 zigzag of four runs (x monotone, y up then down); m: a Metal1 run;
# j: a preferred-direction M2 run of one pitch between two via landings; u: an M4 run; t: a
# T-branch (the branch's end lies inside the trunk: one interior junction); x: two M3 runs crossing
# (a crossing, not a junction); e: TAPER and extension values; o: a via with an orientation;
# d / r: vias of the DEF VIAS section (RECT form and LAYERS form); c: unrouted.
DEF_CAUSES = '''VERSION 5.8 ;
DESIGN causes ;
UNITS DISTANCE MICRONS 1000 ;
VIAS 2 ;
    - myvia + RECT M2 ( -100 -100 ) ( 100 100 ) + RECT V2 ( -50 -50 ) ( 50 50 )
      + RECT M3 ( -100 -100 ) ( 100 100 ) ;
    - rv + VIARULE V34rule + CUTSIZE 100 100 + LAYERS M3 V3 M4 + CUTSPACING 100 100
      + ENCLOSURE 50 50 50 50 ;
END VIAS
NETS 11 ;
    - z ( u1 A ) ( u2 Y ) + USE SIGNAL
      + ROUTED M2 ( 0 10000 ) ( * 10840 ) ( 420 * ) ( * 10000 ) ( 840 * ) ;
    - m ( u3 A ) ( u4 Y ) + USE SIGNAL
      + ROUTED M1 ( 0 20000 ) ( 2000 * ) ;
    - j ( u5 A ) ( u6 Y ) + USE SIGNAL
      + ROUTED M1 ( 5000 20000 ) V12 ( * 20420 ) V12 ;
    - u ( u7 A ) ( u8 Y ) + USE SIGNAL
      + ROUTED M4 ( 9000 0 ) ( * 4200 ) ;
    - t ( u9 A ) ( u10 Y ) + USE SIGNAL
      + ROUTED M2 ( 0 30000 ) ( * 31000 )
      NEW M2 ( 0 30500 ) ( 800 * ) ;
    - x ( u11 A ) ( u12 Y ) + USE SIGNAL
      + ROUTED M3 ( 0 40000 ) ( 1000 * )
      NEW M3 ( 500 39500 ) ( * 40800 ) ;
    - e ( u13 A ) ( u14 Y ) + USE SIGNAL
      + ROUTED M2 TAPER ( 2000 50000 0 ) ( * 51000 0 ) ;
    - o ( u15 A ) ( u16 Y ) + USE SIGNAL
      + ROUTED M1 ( 3000 50000 ) V12 N ( * 50840 ) ;
    - d ( u17 A ) ( u18 Y ) + USE SIGNAL
      + ROUTED M2 ( 4000 60000 ) myvia ( 4420 * ) ;
    - r ( u19 A ) ( u20 Y ) + USE SIGNAL
      + ROUTED M3 ( 6000 60000 ) rv ( * 60840 ) ;
    - c ( u21 A ) ( u22 Y ) + USE SIGNAL ;
END NETS
END DESIGN
'''


class DefPiecesTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        d = Path(self.dir.name)
        (d / 't.lef').write_text(LEF)
        (d / 't.def').write_text(DEF)
        self.tech = dp.technology(LEF)
        self.path = d / 't.def'

    def tearDown(self):
        self.dir.cleanup()

    def test_counts(self):
        r = dp.analyse(self.path, self.tech)
        self.assertEqual(r['nets'], 3)
        self.assertEqual(r['routed_nets'], 2)
        self.assertEqual(r['connections'], 2 + 1)
        # a: 2 + 1 segments, b: 5 segments
        self.assertEqual(r['segments'], 8)
        self.assertEqual(r['vias'], 3 + 2)
        self.assertEqual(r['patches'], 1)
        self.assertEqual(r['statements'], 5 + 1)
        # a: two M2 runs; b: five runs
        self.assertEqual(r['runs'], 7)
        # a: each M2 run has one via landing inside it
        self.assertEqual(r['interior_junctions'], 2)
        self.assertEqual(r['pieces'], 9)
        self.assertEqual(r['pieces_by_cause'], {'other': 4, 'staircase': 5})
        self.assertEqual(r['runs_by_layer'], {'M2': {'preferred': 5, 'wrong_way': 2}})

    def test_trail_bound(self):
        r = dp.analyse(self.path, self.tech)
        # a: the first path with its two vias has four odd vertices (two trails); the second
        # run and the via inside it share no vertex (one trail each); b: one open trail
        self.assertEqual(r['trail_bound'], 2 + 1 + 1 + 1)

    def test_unsupported_and_inconsistent_input_fail(self):
        for bad in (DEF.replace('NEW M2 ( 840 0 )', 'NEW M2 MASK 1 ( 840 0 )'),
                    DEF.replace('NETS 3', 'NETS 4'),
                    DEF.replace('( 840 4200 )', '( 900 4200 )')):
            self.path.write_text(bad)
            with self.assertRaises(ValueError):
                dp.analyse(self.path, self.tech)

    def test_command_line_exit_codes(self):
        lef = self.path.with_name('t.lef')
        self.assertEqual(dp.main(['--tech-lef', str(lef), str(self.path)]), 0)
        self.path.write_text(DEF.replace('V12 ( 0 2100 )', 'V99 ( 0 2100 )'))
        self.assertEqual(dp.main(['--tech-lef', str(lef), str(self.path)]), 2)


    def test_every_cause_and_construct(self):
        self.path.write_text(DEF_CAUSES)
        r = dp.analyse(self.path, self.tech)
        self.assertEqual(r['nets'], 11)
        self.assertEqual(r['routed_nets'], 10)
        self.assertEqual(r['connections'], 10)
        # z 4, t 2, x 2, one each for the other six nets
        self.assertEqual(r['segments'], 15)
        self.assertEqual(r['vias'], 2 + 1 + 1 + 1)
        self.assertEqual(r['patches'], 0)
        self.assertEqual(r['statements'], 12)
        self.assertEqual(r['runs'], 15)
        # only the T-branch meets a run's interior; the crossing is reported apart
        self.assertEqual(r['interior_junctions'], 1)
        self.assertEqual(r['crossings'], 1)
        self.assertEqual(r['pieces'], 16)
        self.assertEqual(r['pieces_by_cause'], {'metal1': 1, 'other': 3 + 2 + 1 + 1 + 1, 'upper': 2,
                                                'via_pair_jog': 1, 'zigzag': 4})
        self.assertEqual(r['runs_by_layer'], {'M1': {'preferred': 1},
                                              'M2': {'preferred': 6, 'wrong_way': 3},
                                              'M3': {'preferred': 2, 'wrong_way': 1},
                                              'M4': {'preferred': 2}})
        # the T and the crossing are two separate paths each as written
        self.assertEqual(r['trail_bound'], 12)

    def test_unrouted_def(self):
        self.path.write_text(DEF.replace('NETS 3', 'NETS 1').split('- a ')[0]
                             + '- c ( u6 A ) ( u7 Y ) + USE SIGNAL ;\nEND NETS\nEND DESIGN\n')
        r = dp.analyse(self.path, self.tech)
        self.assertEqual((r['nets'], r['routed_nets'], r['pieces'], r['pieces_per_connection']), (1, 0, 0, 0))
        self.assertEqual(r['pieces_by_cause'], {})

    def test_rect_after_wires_and_missing_sections_fail(self):
        for bad in (DEF.replace('NEW M2 ( 840 0 ) ( 840 4200 )', 'NEW M2 ( 840 0 ) ( 840 4200 ) RECT ( 0 0 100 100 )'),
                    DEF.replace('NETS 3 ;', 'NETZ 3 ;'),
                    DEF_CAUSES.replace('V12 N', 'V12 Q')):
            self.path.write_text(bad)
            with self.assertRaises(ValueError):
                dp.analyse(self.path, self.tech)


if __name__ == '__main__':
    unittest.main()
