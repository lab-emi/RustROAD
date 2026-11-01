# SPDX-License-Identifier: Apache-2.0
"""Bounded external sorting compared with an independent in-memory oracle."""
import hashlib
from pathlib import Path
import random
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "signoff"))
from label_sort import write_sorted


class LabelSortTests(unittest.TestCase):
    def test_multiple_merge_levels_preserve_duplicates_unicode_and_empty_input(self):
        rng = random.Random(42)
        lines = [f'{rng.randrange(100)}\t{rng.choice(["pin", "π", "ä", "", "零"])}'
                 for _ in range(5000)]
        lines.extend(["", "\t", "a", "a\t", "a\t0", "a\t\t"])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "labels.tsv"
            for records in (lines, []):
                result = write_sorted(iter(records), path, buffer_bytes=256)
                expected = "".join(line + "\n" for line in sorted(records)).encode("utf-8")
                self.assertEqual(path.read_bytes(), expected)
                self.assertEqual(result["count"], len(records))
                self.assertEqual(result["sha256"], hashlib.sha256(expected).hexdigest())
                self.assertFalse(result["truncated"])
                self.assertEqual(list(Path(directory).iterdir()), [path])

    def test_limits_and_failed_input_preserve_the_previous_output(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "labels.tsv"
            path.write_bytes(b"previous\n")
            cases = [(iter(["a", "b"]), {"max_records": 1}),
                     (iter(["a", "b"]), {"max_bytes": 3}),
                     (iter(["a", "b"]), {"max_work": 1}),
                     (iter(["a\nb"]), {}),
                     (iter(["x" * 300]), {"buffer_bytes": 256}),
                     (iter(["x" * (1 << 20)]), {})]
            for records, limits in cases:
                with self.assertRaises(ValueError):
                    write_sorted(records, path, **limits)
                self.assertEqual(path.read_bytes(), b"previous\n")
                self.assertEqual(list(Path(directory).iterdir()), [path])

    def test_merge_work_exhaustion_leaves_no_partial_result(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "labels.tsv"
            with self.assertRaisesRegex(ValueError, "work budget"):
                write_sorted((str(i) for i in range(100)), path,
                             buffer_bytes=256, max_work=205)
            self.assertFalse(path.exists())
            self.assertEqual(list(Path(directory).iterdir()), [])
