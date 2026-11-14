"""Output-comparison controls: tiny files, <32 MiB, no graph allocation."""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_extraction_reorder import compare


SPEF = '''*SPEF "IEEE 1481-1998"
*C_UNIT 1 PF
*R_UNIT 1 OHM
*NAME_MAP
*1 net
*2 gate
*D_NET *1 0.0020000000000000000
*CONN
*I *2:A I
*CAP
1 *1:1 0.0010000000000000000
2 *1:1 *2:A 0.0010000000000000000
*RES
1 *1:1 *2:A 10.0000000000000000
*END
'''


class ExtractionReorder(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.baseline = Path(self.directory.name) / "a.spef"
        self.candidate = Path(self.directory.name) / "b.spef"
        self.baseline.write_text(SPEF)

    def check(self, candidate, **limits):
        self.candidate.write_text(candidate)
        return compare(self.baseline, self.candidate, **limits)

    def test_low_order_changes_count_every_numeric_record(self):
        result = self.check(SPEF.replace("0.0010000000000000000", "0.0010000000000000002"))
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["values"]["ground_pf"]["changed"], 1)
        self.assertEqual(result["values"]["coupling_pf"]["changed"], 1)
        self.assertEqual(result["values"]["resistance_ohm"]["changed"], 0)
        self.assertEqual(result["values"]["ground_pf"]["max_absolute"], "2E-19")
        self.assertEqual(result["structural_differences"], 0)

    def test_changed_connection_record_or_units_is_not_a_numeric_reorder(self):
        for text in (SPEF.replace("*I *2:A I", "*I *2:B I"),
                     SPEF.replace("1 *1:1 0.001", "1 *1:2 0.001"),
                     SPEF.replace("*C_UNIT 1 PF", "*C_UNIT 1 FF"),
                     SPEF.replace("*END\n", "")):
            with self.subTest(text=text):
                result = self.check(text)
                self.assertEqual(result["status"], "failed")
                self.assertGreater(result["structural_differences"], 0)

    def test_resistance_changes_and_large_capacitance_changes_fail(self):
        for old, new in (("10.0000000000000000", "10.0000000000000001"),
                         ("0.0010000000000000000", "0.0020000000000000000")):
            self.assertEqual(self.check(SPEF.replace(old, new))["status"], "failed")

    def test_one_last_digit_of_the_six_digit_format_is_reported(self):
        result = self.check(SPEF.replace("0.0010000000000000000", "0.00100001"))
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["values"]["ground_pf"]["max_absolute"], "1.00000000000E-8")

    def test_empty_nonfinite_and_oversized_inputs_are_refused(self):
        for text in ("", SPEF.replace("0.0010000000000000000", "NaN"),
                     SPEF.replace("0.0010000000000000000", "Infinity")):
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    self.check(text)
        for limits in (dict(max_bytes=16), dict(max_lines=3)):
            with self.assertRaisesRegex(ValueError, "SPEF_(BYTES|BUDGET)"):
                self.check(SPEF, **limits)


if __name__ == "__main__":
    unittest.main()
