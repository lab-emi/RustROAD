# SPDX-License-Identifier: Apache-2.0
from pathlib import Path
import hashlib
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import normalize_verilog_ports as N


class NormalizePorts(unittest.TestCase):
    def run_case(self, text):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        source, target = root / 'source.v', root / 'normalized.v'
        source.write_bytes(text)
        result = N.normalize(source, target)
        original, normalized = source.read_bytes(), target.read_bytes()
        self.assertEqual(original[result['body_source_offset']:],
                         normalized[result['body_target_offset']:])
        self.assertEqual(result['source_sha256'], hashlib.sha256(original).hexdigest())
        self.assertEqual(result['output_sha256'], hashlib.sha256(normalized).hexdigest())
        return normalized, result

    def test_non_ansi_preserves_order_ranges_and_opaque_body(self):
        text = b'/* license */\nmodule m(y,a,VDD);\ninput [0:3] a;\noutput [3:0] y;\ninout VDD;\n/* body */ assign y=a; endmodule\n'
        normalized, result = self.run_case(text)
        self.assertEqual(result['ports'], 3)
        self.assertTrue(normalized.startswith(b'/* license */\nmodule m (\n  output [3:0] y,\n  input [0:3] a,\n  inout VDD\n);'))

    def test_escaped_names_comments_and_grouped_declarations(self):
        normalized, result = self.run_case(b'module \\m.x (\\a,b , y, z);\n// declarations\ninput wire \\a,b ; output wire y,z;\nassign y=\\a,b ;\nassign z=y; endmodule\n')
        self.assertEqual(result['ports'], 3)
        self.assertIn(b'input wire \\a,b ', normalized)
        self.assertIn(b'output wire z', normalized)

    def test_ansi_is_copied_exactly(self):
        text = b'module m(input wire a, output wire y); assign y=a; endmodule\n'
        normalized, result = self.run_case(text)
        self.assertEqual(normalized, text)
        self.assertEqual(result['status'], 'copied_ansi')
        self.assertIsNone(result['ports'])

    def test_generated_interleaved_wire_declarations_are_retained(self):
        normalized, result = self.run_case(b'module m(a,y);\ninput [3:0] a;\nwire [3:0] a;\nwire internal;\noutput y;\nwire y;\nassign y=a[0]; endmodule\n')
        self.assertGreater(result['retained_declaration_bytes'], 0)
        self.assertIn(b'wire [3:0] a;', normalized)
        self.assertIn(b'wire internal;', normalized)

    def test_long_leading_comments_cross_chunk_boundaries_without_body_changes(self):
        text = b'module m(a,y); input a; output y;\n/*' + b'x' * 13 + b'*/\n// wire [99:0] a; comment only\nassign y=a;\nendmodule\n'
        with mock.patch.object(N, 'CHUNK', 16):
            normalized, result = self.run_case(text)
        self.assertIn(b'/*' + b'x' * 13 + b'*/', normalized)
        self.assertEqual(result['ports'], 2)

    def test_inconsistent_and_unsupported_prefixes_publish_nothing(self):
        cases = [
            b'module m(a,a); input a; endmodule',
            b'module m(a,b); input a; output a; endmodule',
            b'module m(a); input missing; endmodule',
            b'module m(a,b); input a; assign b=a; output b; endmodule',
            b'module m(a); input [WIDTH:0] a; endmodule',
            b'module m #(parameter W=2)(a); input a; endmodule',
            b'// synthesis translate_off\nmodule m(a); input a; endmodule',
            b'module m(a);',
            b'module m(a); input a;\nwire [3:0] a;\nendmodule\n',
            b'module m(a); input a;\nwire internal;\nwire signed a;\nendmodule\n',
            b'module m(a); input a;\nassign a=1; wire [3:0] a;\nendmodule\n',
            b'module m(a); input a;\n`define WIDTH 8\nendmodule\n',
            b'module m(a); input a;\ninteger a;\nendmodule\n',
            b'module m(a); input a;\ncustom_type a;\nendmodule\n',
            b'module m(a); input a;\nassign a=1; custom_type a;\nendmodule\n',
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for i, text in enumerate(cases):
                with self.subTest(text=text):
                    source, target = root / f'in{i}.v', root / f'out{i}.v'
                    source.write_bytes(text)
                    with self.assertRaises(N.PrefixError):
                        N.normalize(source, target)
                    self.assertFalse(target.exists())
                    self.assertFalse(list(root.glob('*.partial-*')))

    def test_limits_and_existing_files_are_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, target = root / 'in.v', root / 'out.v'
            source.write_bytes(b'module m(a); input a; endmodule')
            with self.assertRaises(N.PrefixError):
                N.normalize(source, target, maximum=4)
            with mock.patch.object(N, 'MAX_STATEMENT', 8), self.assertRaises(N.PrefixError):
                N.normalize(source, target)
            temporary = target.with_name(target.name + f'.partial-{os.getpid()}')
            temporary.write_bytes(b'preserve')
            with self.assertRaises(FileExistsError):
                N.normalize(source, target)
            self.assertEqual(temporary.read_bytes(), b'preserve')
            target.write_bytes(b'existing')
            with self.assertRaises(FileExistsError):
                N.normalize(source, target)
            self.assertEqual(target.read_bytes(), b'existing')


if __name__ == '__main__':
    unittest.main()
