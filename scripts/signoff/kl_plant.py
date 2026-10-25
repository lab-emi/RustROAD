# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro that plants one signoff negative control into a copy of a GDS:
#   klayout -b -r kl_plant.py -rd gds=IN -rd out=OUT -rd report=JSON -rd kind=KIND
#       -rd metal=L/D -rd pin_layer=L/D -rd pin_text=L/D -rd draw_layer=L/D -rd boundary=L/D
#       [-rd via_cell=NAME -rd via_x=DBU -rd via_y=DBU] [-rd pins=A,B] [-rd antenna_pin=NAME]
#       [-rd swap=A,B]
# KIND: spacing | offgrid | short | missing_via | antenna | rect | no_boundary | dbu_half | label_swap
#       | missing_label | extra_pin | rename_label | alias_label | extra_top
# (label_swap: exchange the strings of every top-cell text labelled A or B, on all layers;
#  missing_label: delete every top-cell text labelled A, i.e. the first name of -rd swap;
#  extra_pin: a floating 1x1 um draw+pin rectangle labelled "extra" on the pin-label layer in a
#  free region (a top-level port that the reference netlist does not have);
#  rename_label: rename every top-cell text labelled A to A + "x", same layer and position;
#  alias_label: add a copy of every top-cell text labelled A, named A + "_alias", at the same
#  layer and position (a second, non-reference port name on an existing port net);
#  extra_top: add an uninstantiated cell "stray_top" holding one metal rectangle, i.e. a
#  second top cell next to the block)
# (rect: -rd rect=x1,y1,x2,y2 in GDS dbu on -rd metal=L/D)
# Geometry for spacing/offgrid is placed in a region with no shapes on any layer
# (except the PR boundary) within a 2 um margin, so the planted defect is the
# only new interaction. All coordinates stay integer database units.
import json
import pya

ly = pya.Layout()
ly.read(gds)
top = ly.top_cell()
dbu = ly.dbu
U = lambda um: int(round(um / dbu))  # noqa: E731
info = {"kind": kind, "top": top.name, "dbu_um": dbu}


def layer(spec):
    l, d = [int(v) for v in spec.split("/")]
    return ly.layer(l, d)


def occupied():
    bl, bd = [int(v) for v in boundary.split("/")]
    region = pya.Region()
    for index in ly.layer_indexes():
        lp = ly.get_info(index)
        if (lp.layer, lp.datatype) == (bl, bd):
            continue
        region.insert(top.begin_shapes_rec(index))
    return region.merged()


def free_box(width_um, height_um, margin_um=2.0):
    used = occupied()
    die = top.bbox()
    step = U(1.0)
    w, h, m = U(width_um), U(height_um), U(margin_um)
    y = die.bottom + m
    while y + h + m <= die.top:
        x = die.left + m
        while x + w + m <= die.right:
            probe = pya.Box(x - m, y - m, x + w + m, y + h + m)
            if (used & pya.Region(probe)).is_empty():
                return pya.Box(x, y, x + w, y + h)
            x += step
        y += step
    raise RuntimeError("no free region for the planted control")


def grid(v, g=5):
    return v - (v % g)


metal_li = layer(metal)
if kind == "spacing":
    gap = float(spacing_um)
    box = free_box(0.6 + gap + 0.6, 1.0)
    x0, y0 = grid(box.left), grid(box.bottom)
    a = pya.Box(x0, y0, x0 + U(0.6), y0 + U(1.0))
    b = pya.Box(a.right + U(gap), y0, a.right + U(gap) + U(0.6), y0 + U(1.0))
    top.shapes(metal_li).insert(a)
    top.shapes(metal_li).insert(b)
    info.update(shapes=[str(a), str(b)], gap_um=gap)
elif kind == "offgrid":
    box = free_box(1.0, 1.0)
    x0, y0 = grid(box.left), grid(box.bottom)
    a = pya.Box(x0, y0, x0 + U(0.6) + 2, y0 + U(1.0))  # right edge 2 dbu off the 5 nm grid
    top.shapes(metal_li).insert(a)
    info.update(shapes=[str(a)], off_grid_dbu=2)
elif kind in ("short", "antenna"):
    pl, tl = layer(pin_layer), layer(pin_text)
    rects = {}
    for shape in top.shapes(tl).each():
        if shape.is_text():
            t = shape.text
            for s2 in top.shapes(pl).each_touching(pya.Box(t.x - 1, t.y - 1, t.x + 1, t.y + 1)):
                if not s2.is_text() and s2.bbox().area() > 0:
                    rects[t.string] = s2.bbox()
    if kind == "short":
        first, second = pins.split(",")
        a, b = rects[first], rects[second]
        bridge = a + b
        top.shapes(layer(draw_layer)).insert(bridge)
        info.update(pins=[first, second], bridge=str(bridge))
    else:
        r = rects[antenna_pin]
        die = top.bbox()
        length = U(float(antenna_length_um))
        width = r.width() if r.width() <= r.height() else r.height()
        if r.bottom == die.bottom:
            wire = pya.Box(r.left, r.bottom - length, r.left + width, r.top)
        elif r.top == die.top:
            wire = pya.Box(r.left, r.bottom, r.left + width, r.top + length)
        elif r.left == die.left:
            wire = pya.Box(r.left - length, r.bottom, r.right, r.bottom + width)
        else:
            wire = pya.Box(r.left, r.bottom, r.right + length, r.bottom + width)
        top.shapes(layer(draw_layer)).insert(wire)
        info.update(pin=antenna_pin, wire=str(wire), length_um=float(antenna_length_um))
elif kind == "missing_via":
    target = pya.Point(int(via_x), int(via_y))
    victims = [inst for inst in top.each_inst()
               if inst.cell.name == via_cell and inst.trans.disp == target]
    if len(victims) != 1:
        raise RuntimeError("expected exactly one %s at %s, found %d" % (via_cell, target, len(victims)))
    victims[0].delete()
    info.update(removed=via_cell, at=[target.x, target.y])
elif kind == "label_swap":
    first, second = swap.split(",")
    swapped = {first: 0, second: 0}
    for index in ly.layer_indexes():
        labels = [sh for sh in top.shapes(index).each() if sh.is_text() and sh.text.string in swapped]
        for shape in labels:
            t = shape.text
            swapped[t.string] += 1
            t.string = second if t.string == first else first
            shape.text = t
    if not all(swapped.values()):
        raise RuntimeError("label_swap: labels not found %r" % swapped)
    info.update(swapped=swapped, pins=[first, second])
elif kind == "missing_label":
    victim = swap.split(",")[0]
    deleted = 0
    for index in ly.layer_indexes():
        for shape in [sh for sh in top.shapes(index).each() if sh.is_text() and sh.text.string == victim]:
            shape.delete()
            deleted += 1
    if not deleted:
        raise RuntimeError("missing_label: label %r not found" % victim)
    info.update(deleted_label=victim, deleted=deleted)
elif kind == "extra_pin":
    box = free_box(1.0, 1.0)
    x0, y0 = grid(box.left), grid(box.bottom)
    r = pya.Box(x0, y0, x0 + U(1.0), y0 + U(1.0))
    top.shapes(layer(draw_layer)).insert(r)
    if pin_layer != pin_text:
        top.shapes(layer(pin_layer)).insert(r)
    c = r.center()
    top.shapes(layer(pin_text)).insert(pya.Text("extra", pya.Trans(c.x, c.y)))
    info.update(rect=str(r), label="extra", layers=sorted({draw_layer, pin_layer, pin_text}))
elif kind == "rename_label":
    victim = swap.split(",")[0]
    renamed = 0
    for index in ly.layer_indexes():
        for shape in [sh for sh in top.shapes(index).each() if sh.is_text() and sh.text.string == victim]:
            t = shape.text
            t.string = victim + "x"
            shape.text = t
            renamed += 1
    if not renamed:
        raise RuntimeError("rename_label: label %r not found" % victim)
    info.update(renamed_label=victim, new_label=victim + "x", renamed=renamed)
elif kind == "alias_label":
    victim = swap.split(",")[0]
    added = 0
    for index in ly.layer_indexes():
        for shape in [sh for sh in top.shapes(index).each() if sh.is_text() and sh.text.string == victim]:
            t = shape.text
            top.shapes(index).insert(pya.Text(victim + "_alias", t.trans))
            added += 1
    if not added:
        raise RuntimeError("alias_label: label %r not found" % victim)
    info.update(aliased_label=victim, alias=victim + "_alias", added=added)
elif kind == "extra_top":
    stray = ly.create_cell("stray_top")
    stray.shapes(metal_li).insert(pya.Box(0, 0, U(1.0), U(1.0)))
    info.update(extra_top="stray_top", layer=metal)
elif kind == "rect":
    r = pya.Box(*[int(v) for v in rect.split(",")])
    top.shapes(metal_li).insert(r)
    info.update(shapes=[str(r)], layer=metal)
elif kind == "no_boundary":
    bl, bd = [int(v) for v in boundary.split("/")]
    li = ly.find_layer(bl, bd)
    removed = top.shapes(li).size() if li is not None else 0
    if li is not None:
        top.shapes(li).clear()
    info.update(removed_boundary_shapes=removed)
elif kind == "dbu_half":
    ly.transform(pya.ICplxTrans(2.0, 0, False, 0, 0))
    ly.dbu = dbu / 2.0
    info.update(new_dbu_um=ly.dbu)
else:
    raise RuntimeError("unknown control kind " + kind)
save = pya.SaveLayoutOptions()
save.gds2_write_timestamps = False
ly.write(out, save)
with open(report, "w") as handle:
    json.dump(info, handle, indent=1, sort_keys=True)
print("KL_PLANT_DONE", kind)
