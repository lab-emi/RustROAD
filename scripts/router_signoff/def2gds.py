# Reference DEF->GDS streamout with the KLayout LEF/DEF reader (black-box use).
# Run inside the pinned signoff image: klayout -b -r def2gds.py with env
# DEF TLEF CLEF CGDS MAP OUT TOP. Standard cells stay hierarchical (SREFs to the
# original PDK GDS structures); routing comes only from the DEF under test.
import os
import pya

opt = pya.LoadLayoutOptions()
cfg = opt.lefdef_config
cfg.map_file = os.environ['MAP']
cfg.lef_files = [os.environ['TLEF'], os.environ['CLEF']]
cfg.read_lef_with_def = False
cfg.macro_resolution_mode = 2  # never synthesize macro geometry from LEF
cells = pya.Layout()
cells.read(os.environ['CGDS'])
cfg.macro_layouts = [cells]
layout = pya.Layout()
layout.read(os.environ['DEF'], opt)
tops = [c.name for c in layout.top_cells()]
if tops != [os.environ['TOP']]:
    raise RuntimeError('unexpected top cells %r' % tops)
top = layout.top_cell()
if top.bbox().empty():
    raise RuntimeError('empty top cell')
print('TOP %s DBU %.6f CELLS %d BBOX %s' % (top.name, layout.dbu, layout.cells(), top.bbox()))
for index in layout.layer_indexes():
    info = layout.get_info(index)
    count = 0
    it = top.begin_shapes_rec(index)
    while not it.at_end():
        count += 1
        it.next()
    print('LAYER %d/%d shapes_rec=%d' % (info.layer, info.datatype, count))
layout.write(os.environ['OUT'])
print('WROTE', os.environ['OUT'])
