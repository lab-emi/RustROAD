"""def_pieces.py: runs, pieces, trail bound and causes on a hand-checked DEF."""
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


if __name__ == '__main__':
    unittest.main()
