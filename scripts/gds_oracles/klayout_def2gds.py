# Reference streamout: KLayout's LEF/DEF reader used strictly as a black box.
# Runs inside the pinned signoff image: klayout -b -r klayout_def2gds.py
# env: DEF TLEF CLEF CGDS MAP OUT. Standard cells come from the PDK cell GDS
# (macro_resolution_mode=2: never synthesize cell geometry from LEF).
import os
import pya

opt = pya.LoadLayoutOptions()
cfg = opt.lefdef_config
cfg.map_file = os.environ['MAP']
cfg.lef_files = [os.environ['TLEF'], os.environ['CLEF']]
cfg.read_lef_with_def = False
cfg.macro_resolution_mode = 2
cells = pya.Layout()
cells.read(os.environ['CGDS'])
cfg.macro_layouts = [cells]
layout = pya.Layout()
layout.read(os.environ['DEF'], opt)
top = layout.top_cell()
print('REFERENCE_TOP %s dbu %.6g cells %d' % (top.name, layout.dbu, layout.cells()))
layout.write(os.environ['OUT'])
print('REFERENCE_WROTE %s' % os.environ['OUT'])
