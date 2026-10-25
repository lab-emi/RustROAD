"""The reference streamout must not retain a whole full-chip DEF."""
from pathlib import Path
import sys
import tempfile
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'signoff'))
import def2gds_reference as ref
import parsers

WORK = Path(__file__).resolve().parents[2] / '.work' / 'def-metadata-tests'
WORK.mkdir(parents=True, exist_ok=True)

DEF = '''VERSION 5.8 ;
DESIGN t ;
PROPERTYDEFINITIONS
DESIGN FE_CORE_X REAL ;
DESIGN FE_CORE_Y REAL ;
END PROPERTYDEFINITIONS
UNITS DISTANCE MICRONS 2000 ;
ROW r s 1 2 N DO 3 BY 1 STEP 5 0 ;
COMPONENTS 2 ;
- a NAND
 + PLACED ( 2 4 ) N ;
- b BUF + FIXED ( 6 8 ) N ;
END COMPONENTS
NETS 1 ;
- n ( a A ) ( b Z )
 + ROUTED Metal1 1201 ( 1 2 3 ) NEW Metal2 ( 4 6 ) ;
END NETS
END DESIGN
'''

class DefMetadataTests(unittest.TestCase):
    def test_counts_match_whole_text_oracle(self):
        with tempfile.TemporaryDirectory(dir=WORK) as d:
            path=Path(d)/'a.def';path.write_text(DEF)
            meta, odd=ref.def_file_summary(path, True)
            self.assertEqual(meta, ref.def_summary(DEF))
            expected=parsers.count_def_odd_coordinates(DEF)
            self.assertEqual((odd['checked'],odd['odd']), (expected['checked'],expected['odd']))
    def test_truncated_and_oversized_records_fail(self):
        with tempfile.TemporaryDirectory(dir=WORK) as d:
            path=Path(d)/'a.def'
            for text in [DEF.replace('COMPONENTS 2','COMPONENTS 3'),DEF.replace('END COMPONENTS\n',''),DEF+'- unterminated']:
                path.write_text(text)
                with self.assertRaises(ValueError):ref.def_file_summary(path)
            path.write_text('x'*1000)
            with self.assertRaises(ValueError):list(ref.def_records(path,100))

if __name__=='__main__':unittest.main()
