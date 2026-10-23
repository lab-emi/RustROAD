# Dump label/hierarchy facts of a GDS as JSON (KLayout black box).
# klayout -b -r klayout_texts.py -rd gds=<file> -rd out=<json>
import json
import pya

layout = pya.Layout()
layout.read(gds)
tops = [c.name for c in layout.top_cells()]
top = layout.top_cell()
labels = []
flat = []
for li in layout.layer_indexes():
    info = layout.get_info(li)
    for s in top.shapes(li).each():
        if s.is_text():
            t = s.text
            labels.append([info.layer, info.datatype, t.string, t.x, t.y,
                           str(t.trans.rot), bool(t.trans.is_mirror()), t.size,
                           int(t.halign), int(t.valign)])
    it = top.begin_shapes_rec(li)
    while not it.at_end():
        shape = it.shape()
        if shape.is_text():
            t = shape.text.transformed(it.trans())
            flat.append([info.layer, info.datatype, t.string, t.x, t.y])
        it.next()
cells = {}
for ci in range(layout.cells()):
    c = layout.cell(ci)
    cells[c.name] = dict(instances=c.child_instances(),
                         shapes=sum(c.shapes(li).size() for li in layout.layer_indexes()))
with open(out, 'w') as handle:
    json.dump(dict(dbu=layout.dbu, tops=tops, labels=sorted(labels), flat_texts=sorted(flat),
                   cells=cells), handle, indent=1, sort_keys=True)
print('TEXTS_WROTE', out, len(labels), len(flat))
