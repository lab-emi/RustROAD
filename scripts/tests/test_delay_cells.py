# SPDX-License-Identifier: Apache-2.0
"""Unit tests for the Liberty reader and delay-cell profile derivation (no PDK needed).

Run: python3 -m unittest discover -s scripts/tests -v
"""
from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import build_delay_cells  # noqa: E402
import liberty_lite as lib  # noqa: E402

LIBERTY = r'''/* synthetic */
library ("toy") {
  time_unit : "1ns" ;
  capacitive_load_unit (1, pf) ;
  lu_table_template (del_2_2) {
    variable_1 : input_net_transition ;
    variable_2 : total_output_net_capacitance ;
    index_1 ("0.0, 1.0") ;
    index_2 ("0.0, 0.1") ;
  }
  cell ("BUF") {
    area : 4.0 ;
    pin ("A") { direction : input ; capacitance : 0.002 ; }
    pin ("X") {
      direction : output ;
      function : "(A)" ;
      timing () {
        related_pin : "A" ;
        timing_sense : positive_unate ;
        cell_rise (del_2_2) { values ("0.10, 0.30", \
                                       "0.20, 0.40") ; }
        cell_fall (del_2_2) { values ("0.12, 0.32", "0.22, 0.42") ; }
        rise_transition (del_2_2) { values ("0.01, 0.02", "0.03, 0.04") ; }
        fall_transition (del_2_2) { values ("0.01, 0.02", "0.03, 0.04") ; }
      }
    }
  }
  cell (INV) {
    area : 2.0 ;
    pin (A) { direction : input ; capacitance : 0.002 ; }
    pin (Y) { direction : output ; function : "!A" ;
      timing () { related_pin : "A" ; timing_sense : negative_unate ;
        cell_rise (del_2_2) { values ("0.1, 0.1", "0.1, 0.1") ; }
        cell_fall (del_2_2) { values ("0.1, 0.1", "0.1, 0.1") ; } } }
  }
  cell ("AND2") {
    area : 5.0 ;
    pin ("A") { direction : input ; }
    pin ("B") { direction : input ; }
    pin ("X") { direction : output ; function : "(A&B)" ; }
  }
}
'''


class LibertyLiteTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / 'toy.lib'
        self.path.write_text(LIBERTY)

    def tearDown(self):
        self.tmp.cleanup()

    def test_bilinear_inside_and_refuses_extrapolation(self):
        values = [[0.10, 0.30], [0.20, 0.40]]
        self.assertAlmostEqual(lib.bilinear([0.0, 1.0], [0.0, 0.1], values, 0.5, 0.05), 0.25)
        self.assertAlmostEqual(lib.bilinear([0.0, 1.0], [0.0, 0.1], values, 0.0, 0.1), 0.30)
        with self.assertRaises(lib.LibertyError):
            lib.bilinear([0.0, 1.0], [0.0, 0.1], values, 1.5, 0.05)

    def test_selected_cells_quoted_and_unquoted(self):
        library = lib.Library(self.path, wanted={'BUF', 'INV'})
        self.assertEqual(sorted(library.cells), ['BUF', 'INV'])
        self.assertEqual(library.all_cells, ['BUF', 'INV', 'AND2'])
        self.assertEqual(library.time_unit_ns(), 1.0)
        self.assertEqual(library.cap_unit_pf(), 1.0)

    def test_identity_buffer_is_profiled_and_others_are_refused(self):
        library = lib.Library(self.path, wanted={'BUF', 'INV', 'AND2'})
        rep = dict(input_slew_ns=0.5, load_pf=0.05)
        data, reason = build_delay_cells.buffer_data(library, 'BUF', rep)
        self.assertIsNone(reason)
        self.assertEqual((data['input_pin'], data['output_pin'], data['function']), ('A', 'X', '(A)'))
        self.assertAlmostEqual(data['delay_ns']['rise'], 0.25)
        self.assertAlmostEqual(data['delay_ns']['fall'], 0.27)
        data, reason = build_delay_cells.buffer_data(library, 'INV', rep)
        self.assertIsNone(data)
        self.assertIn('not the identity', reason)
        data, reason = build_delay_cells.buffer_data(library, 'AND2', rep)
        self.assertIsNone(data)
        self.assertIn('inputs', reason)
        self.assertIn('absent', build_delay_cells.buffer_data(library, 'NOPE', rep)[1])


if __name__ == '__main__':
    unittest.main()
