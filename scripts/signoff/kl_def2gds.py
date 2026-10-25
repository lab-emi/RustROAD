# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro (black-box use of KLayout's public LEF/DEF reader):
#   klayout -b -r kl_def2gds.py -rd def_path=... -rd tlef=... -rd clef=... -rd cgds=...
#       -rd map_file=... -rd out=... -rd report=...
# Reads the routed DEF with the PDK technology/cell LEF and the PDK layer map,
# resolves every macro to the PDK standard-cell GDS structure (never to LEF
# geometry), and writes a hierarchical GDS at 0.001 um without timestamps.
import json
import pya

opt = pya.LoadLayoutOptions()
cfg = opt.lefdef_config
cfg.dbu = 0.001
cfg.map_file = map_file
cfg.lef_files = [tlef, clef]
cfg.read_lef_with_def = False
cfg.macro_resolution_mode = 2  # 2 = always use the GDS layout given in macro_layouts
cells = pya.Layout()
cells.read(cgds)
cfg.macro_layouts = [cells]
ly = pya.Layout()
ly.read(def_path, opt)
tops = ly.top_cells()
if len(tops) != 1:
    raise RuntimeError("expected exactly one top cell, got %d" % len(tops))
top = tops[0]
layers = {}
for index in ly.layer_indexes():
    lp = ly.get_info(index)
    it = top.begin_shapes_rec(index)
    count = 0
    while not it.at_end():
        count += 1
        it.next()
    if count:
        layers["%d/%d" % (lp.layer, lp.datatype)] = count
lib_cells = set(c.name for c in cells.each_cell())
resolved = sorted(set(c.name for c in ly.each_cell() if c.name in lib_cells))
save = pya.SaveLayoutOptions()
save.format = "GDS2"
save.gds2_write_timestamps = False
save.gds2_libname = "RUSTROAD_REF"
ly.write(out, save)
with open(report, "w") as handle:
    json.dump({"top": top.name, "dbu_um": ly.dbu, "cells": ly.cells(),
               "pdk_cells_resolved": resolved, "layer_shape_counts": layers,
               "bbox_um": [top.dbbox().left, top.dbbox().bottom, top.dbbox().right, top.dbbox().top]},
              handle, indent=1, sort_keys=True)
print("KL_DEF2GDS_DONE", top.name, ly.dbu)
