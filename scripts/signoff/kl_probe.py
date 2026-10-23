# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro (klayout -b -r kl_probe.py -rd gds=... -rd top=... -rd out=...
#   -rd boundary=L/D). Writes a JSON description of the layout used by the
# signoff preflight assertions: dbu, top cells, requested top existence/bbox,
# the PR-boundary shapes placed directly in the top cell, and per-layer
# recursive shape counts. Uses only KLayout's public layout API.
import json
import math
import pya

info = {"gds": gds, "requested_top": top}
ly = pya.Layout()
ly.read(gds)
info["dbu_um"] = ly.dbu
info["cells"] = ly.cells()
info["top_cells"] = sorted(c.name for c in ly.top_cells())
info["top_exists"] = ly.has_cell(top)
bl, bd = [int(v) for v in boundary.split("/")]
info["boundary_layer"] = [bl, bd]
if info["top_exists"]:
    cell = ly.cell(top)
    box = cell.bbox()
    info["bbox_dbu"] = None if box.empty() else [box.left, box.bottom, box.right, box.top]
    info["bbox_um"] = None if box.empty() else [box.left * ly.dbu, box.bottom * ly.dbu,
                                                  box.right * ly.dbu, box.top * ly.dbu]
    info["bbox_area_um2"] = 0.0 if box.empty() else box.width() * box.height() * ly.dbu * ly.dbu
    li = ly.find_layer(bl, bd)
    direct, area = 0, 0
    bbox = pya.Box()
    if li is not None:
        for shape in cell.shapes(li).each():
            if shape.is_box() or shape.is_polygon() or shape.is_path():
                direct += 1
                poly = shape.polygon
                area += poly.area()
                bbox += poly.bbox()
    info["boundary_shapes_in_top"] = direct
    info["boundary_area_um2"] = area * ly.dbu * ly.dbu
    info["boundary_bbox_um"] = None if bbox.empty() else [bbox.left * ly.dbu, bbox.bottom * ly.dbu,
                                                          bbox.right * ly.dbu, bbox.top * ly.dbu]
    info["geometry_outside_boundary"] = None if bbox.empty() or box.empty() else \
        not (bbox.contains(box.p1) and bbox.contains(box.p2))
    layers = {}
    total = 0
    for index in ly.layer_indexes():
        lp = ly.get_info(index)
        it = cell.begin_shapes_rec(index)
        count = 0
        while not it.at_end():
            count += 1
            total += 1
            if total > 20000000:
                raise RuntimeError("shape probe budget exceeded")
            it.next()
        if count:
            layers[f"{lp.layer}/{lp.datatype}"] = count
    info["layer_shape_counts"] = layers
    instances = 0
    for inst in cell.each_inst():
        instances += inst.size() if inst.is_regular_array() else 1
    info["top_instances"] = instances
    finite = all(math.isfinite(v) for v in (info["bbox_um"] or []))
    info["bbox_finite"] = finite
with open(out, "w") as handle:
    json.dump(info, handle, indent=1, sort_keys=True)
print("KL_PROBE_DONE")
