# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro (klayout -b -r kl_probe.py -rd gds=... -rd top=... -rd out=...
#   -rd boundary=L/D [-rd texts_out=FLAT.tsv]). Writes a JSON description of the
# layout used by the signoff preflight assertions: dbu, top cells, requested top
# existence/bbox, the PR-boundary shapes placed directly in the top cell,
# per-layer recursive shape counts, every text label placed directly in the top
# cell, and (with texts_out) every text of the flattened hierarchy as sorted
# TSV lines plus their count and sha256. Text positions are integer picometres
# (dbu-independent). Uses only KLayout's public layout API; all work is bounded.
import hashlib
import json
import math
import pya

MAX_SHAPES = 20000000
MAX_TOP_TEXTS = 200000
MAX_FLAT_TEXTS = 5000000

info = {"gds": gds, "requested_top": top}
ly = pya.Layout()
ly.read(gds)
info["dbu_um"] = ly.dbu
info["cells"] = ly.cells()
info["top_cells"] = sorted(c.name for c in ly.top_cells())
info["top_exists"] = ly.has_cell(top)
info["top_is_top_cell"] = top in info["top_cells"]
bl, bd = [int(v) for v in boundary.split("/")]
info["boundary_layer"] = [bl, bd]


def pm(value_dbu):
    return int(round(value_dbu * ly.dbu * 1e6))


def layer_name(index):
    lp = ly.get_info(index)
    return "%d/%d" % (lp.layer, lp.datatype)


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
        it = cell.begin_shapes_rec(index)
        count = 0
        while not it.at_end():
            count += 1
            total += 1
            if total > MAX_SHAPES:
                raise RuntimeError("shape probe budget exceeded")
            it.next()
        if count:
            layers[layer_name(index)] = count
    info["layer_shape_counts"] = layers
    # Text labels placed directly in the top cell (the block's port labels).
    top_texts, truncated = [], False
    for index in ly.layer_indexes():
        for shape in cell.shapes(index).each(pya.Shapes.STexts):
            if len(top_texts) >= MAX_TOP_TEXTS:
                truncated = True
                break
            t = shape.text
            top_texts.append([layer_name(index), t.string, pm(t.x), pm(t.y)])
    info["top_texts"] = sorted(top_texts)
    info["top_texts_truncated"] = truncated
    # Every text of the flattened hierarchy (for the XOR label comparison).
    try:
        texts_path = texts_out
    except NameError:
        texts_path = None
    if texts_path:
        lines, flat_truncated = [], False
        for index in ly.layer_indexes():
            name = layer_name(index)
            it = cell.begin_shapes_rec(index)
            it.shape_flags = pya.Shapes.STexts
            while not it.at_end():
                shape = it.shape()
                if shape.is_text():
                    if len(lines) >= MAX_FLAT_TEXTS:
                        flat_truncated = True
                        break
                    t = shape.text
                    p = it.trans() * pya.Point(t.x, t.y)
                    lines.append("%s\t%s\t%d\t%d" % (name, json.dumps(t.string), pm(p.x), pm(p.y)))
                it.next()
            if flat_truncated:
                break
        lines.sort()
        blob = ("\n".join(lines) + ("\n" if lines else "")).encode("utf-8")
        with open(texts_path, "wb") as handle:
            handle.write(blob)
        info["flat_texts"] = {"path": texts_path, "count": len(lines), "truncated": flat_truncated,
                              "sha256": None if flat_truncated else hashlib.sha256(blob).hexdigest()}
    instances = 0
    for inst in cell.each_inst():
        instances += inst.size() if inst.is_regular_array() else 1
    info["top_instances"] = instances
    finite = all(math.isfinite(v) for v in (info["bbox_um"] or []))
    info["bbox_finite"] = finite
with open(out, "w") as handle:
    json.dump(info, handle, indent=1, sort_keys=True)
print("KL_PROBE_DONE")
