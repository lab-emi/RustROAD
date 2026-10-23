# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro: build the DRC-clean positive-control mini layout.
#   klayout -b -r kl_mini.py -rd pdk=sky130|gf180 -rd libgds=<PDK cell GDS> -rd out=<gds>
# Two rows (second row FS-mirrored) of PDK cells copied verbatim from the cell
# GDS (taps, three inverters, fillers), one routed net u1.out -> u2.in using the
# technology-LEF default via geometry, labelled top-level pins and the PR
# boundary. Used to prove that every DRC oracle can return "passed".
import pya

lib = pya.Layout()
lib.read(libgds)
ly = pya.Layout()
ly.dbu = lib.dbu
top = ly.create_cell("mini_top")
U = lambda v: int(round(v / ly.dbu))  # noqa: E731
copied = {}


def master(name):
    if name not in copied:
        c = ly.create_cell(name)
        c.copy_tree(lib.cell(name))
        copied[name] = c
    return copied[name]


def place(name, x, y, mirror=False):
    t = pya.Trans(pya.Trans.M0 if mirror else pya.Trans.R0, U(x), U(y))
    top.insert(pya.CellInstArray(master(name).cell_index(), t))


def box(l, d, x1, y1, x2, y2):
    top.shapes(ly.layer(l, d)).insert(pya.Box(U(x1), U(y1), U(x2), U(y2)))


def text(l, d, s, x, y):
    top.shapes(ly.layer(l, d)).insert(pya.Text(s, pya.Trans(U(x), U(y))))


def via(defn, cx, cy):
    for (l, d), (x1, y1, x2, y2) in defn:
        box(l, d, cx + x1, cy + y1, cx + x2, cy + y2)


if pdk == "sky130":
    P, H = "sky130_fd_sc_hd__", 2.72
    row0 = [("tapvpwrvgnd_1", .46), ("inv_1", 1.38), ("inv_1", 1.38), ("tapvpwrvgnd_1", .46)]
    row1 = [("tapvpwrvgnd_1", .46), ("inv_1", 1.38), ("fill_1", .46), ("fill_1", .46), ("fill_1", .46),
            ("tapvpwrvgnd_1", .46)]
    l1m1 = [((67, 44), (-.085, -.085, .085, .085)), ((67, 20), (-.085, -.085, .085, .085)),
            ((68, 20), (-.145, -.115, .145, .115))]
    a, b = (0.46 + 0.935, 1.20), (1.84 + 0.485, 1.195)
    via(l1m1, *a)
    via(l1m1, *b)
    box(68, 20, a[0] - .145, 1.085, b[0] + .145, 1.315)
    box(67, 16, 0.46 + .32, 1.075, 0.46 + .65, 1.315)
    text(67, 5, "in", 0.46 + .485, 1.195)
    box(67, 16, 1.84 + .82, 0.885, 1.84 + 1.05, 1.485)
    text(67, 5, "out", 1.84 + .935, 1.20)
    box(68, 16, 0, -.24, 3.68, .24)
    text(68, 5, "VGND", .23, 0)
    box(68, 16, 0, H - .24, 3.68, H + .24)
    text(68, 5, "VPWR", .23, H)
    boundary = (235, 4)
else:
    P, H = "gf180mcu_fd_sc_mcu7t5v0__", 3.92
    row0 = [("filltie", 1.12), ("inv_1", 2.24), ("inv_1", 2.24), ("filltie", 1.12)]
    row1 = [("filltie", 1.12), ("inv_1", 2.24), ("fill_1", .56), ("fill_1", .56), ("fill_1", .56),
            ("fill_1", .56), ("filltie", 1.12)]
    via1_vh = [((35, 0), (-.13, -.13, .13, .13)), ((34, 0), (-.13, -.19, .13, .19)),
               ((36, 0), (-.19, -.14, .19, .14))]
    a, b = (1.12 + 1.43, 1.70), (3.36 + 0.875, 1.70)
    via(via1_vh, *a)
    via(via1_vh, *b)
    box(36, 0, a[0] - .19, 1.56, b[0] + .19, 1.84)
    text(34, 10, "in", 1.12 + .875, 1.70)
    text(34, 10, "out", 3.36 + 1.43, 2.6)
    text(34, 10, "VSS", .56, 0)
    text(34, 10, "VDD", .56, H)
    boundary = (0, 0)
for row, ry, mir in [(row0, 0.0, False), (row1, 2 * H, True)]:
    x = 0.0
    for c, w in row:
        place(P + c, x, ry, mir)
        x += w
width = sum(w for _, w in row0)
box(boundary[0], boundary[1], 0, 0, width, 2 * H)
save = pya.SaveLayoutOptions()
save.gds2_write_timestamps = False
ly.write(out, save)
print("KL_MINI_DONE", width, 2 * H, ly.dbu)
