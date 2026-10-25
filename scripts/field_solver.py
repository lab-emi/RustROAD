#!/usr/bin/env python3
"""Finite-difference electrostatic field solver for interconnect structures.

A ``cpu_ref`` reference for questions the 2-D calibration tables cannot answer
(how much a wire crossing loads each wire in three dimensions). It is not part
of the extractor and is not fitted to anything.

Model: conductors are axis-aligned boxes at fixed potential; z = 0 is the
grounded substrate; every other face of the domain is zero-flux (a mirror
plane: a box touching it continues as its mirror image, and a quarter-domain
solve with mirror planes at x = 0 and y = 0 is exactly one quarter of the
symmetric full structure). The dielectric is homogeneous (relative
permittivity 1). Every capacitance then scales with the permittivity, so the
ratios reported below do not depend on it; absolute values are in aF at
eps_r = 1.

Discretisation: a rectilinear grid with lines on every box face, graded
geometrically from ``hmin`` at the faces to ``hmax``; the 7-point (5-point in
2-D) finite-volume stencil; charges are the exact discrete flux out of each
conductor, so the capacitance matrix of the discrete system is symmetric.
Sparse direct solve (SuperLU) up to ``DIRECT_MAX_NODES`` free nodes,
diagonally preconditioned conjugate gradients above that.

Process stack (conductor z-ranges) from PDK data files only: the KLayout
cross-section script of Sky130 (``libs.tech/klayout/tech/xsect/sky130.xs``:
thickness variables and the ``*_bottom`` sums) and the KLayout 2.5-D view of
GF180 (``libs.tech/klayout/tech/d25/gf180mcu.lyd25``: ``zstart``/``height``).

``crossings`` measures, per PDK and adjacent routing-layer pair, the total
capacitance an upper wire gains from lower wires crossing it at a given pitch,
with or without same-layer neighbours of the upper wire (3-D, per crossing),
and the same quantity from the native two-cut construction applied to this
solver's own 2-D per-length values (upper wire over a plate times the lower
width, plus the lower wire's plate excess under the upper plate times the
upper width; the upper cross-section carries the same neighbours). Their
ratio is the factor by which a two-cut 2.5-D model misses (> 1) or overstates
(< 1) three-dimensional crossing loading in a homogeneous dielectric.

usage: field_solver.py crossings --out docs/evidence/rc-field-solver.json [--markdown]
       field_solver.py selftest
"""
from __future__ import annotations

import argparse
import ast
import os
import json
import operator
from pathlib import Path
import re
import resource
import sys
import time

# Bounded BLAS threads on a shared host (set before numpy loads).
for _var in ("OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS"):
    os.environ.setdefault(_var, "2")
import numpy as np  # noqa: E402
import scipy.sparse as sp  # noqa: E402
import scipy.sparse.linalg as spla  # noqa: E402

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
EPS0_AF_PER_UM = 8.854187817
DIRECT_MAX_NODES = 90_000
MAX_NODES = 400_000
PDK_ROOT = Path("/home/cgao/.local/share/containers/storage/overlay/"
                "a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel")
PDK_VERSION = "f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7"
STACK_FILES = {
    "sky130": f"sky130/versions/{PDK_VERSION}/sky130A/libs.tech/klayout/tech/xsect/sky130.xs",
    "gf180": f"gf180mcu/versions/{PDK_VERSION}/gf180mcuD/libs.tech/klayout/tech/d25/gf180mcu.lyd25",
}
# Routing layer -> calibration metal index (the native profiles' order).
LAYERS = {
    "sky130": ["li1", "met1", "met2", "met3", "met4", "met5"],
    "gf180": ["Metal1", "Metal2", "Metal3", "Metal4", "Metal5"],
}


# ----------------------------------------------------------------------------
# Process stack from PDK files
# ----------------------------------------------------------------------------
_OPS = {ast.Add: operator.add, ast.Sub: operator.sub, ast.Mult: operator.mul, ast.Div: operator.truediv}


def _arith(expr: str, names: dict) -> float:
    """Evaluate +,-,*,/ over numbers and known names (no calls, no attributes)."""
    def ev(node):
        if isinstance(node, ast.Expression):
            return ev(node.body)
        if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)):
            return float(node.value)
        if isinstance(node, ast.Name) and node.id in names:
            return names[node.id]
        if isinstance(node, ast.BinOp) and type(node.op) in _OPS:
            return _OPS[type(node.op)](ev(node.left), ev(node.right))
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.USub):
            return -ev(node.operand)
        raise ValueError(f"unsupported stack expression: {expr}")
    return ev(ast.parse(expr, mode="eval"))


def stack_sky130(text: str) -> dict:
    """Metal z-ranges (um) from the cross-section script's variables."""
    names = {}
    for line in text.splitlines():
        m = re.match(r"^\s*([a-z0-9_]+)\s*=\s*([0-9a-z_+\-*/. ()]+?)\s*(#.*)?$", line)
        if m and ("thickness" in m[1] or m[1].endswith("_bottom")):
            try:
                names[m[1]] = _arith(m[2], names)
            except ValueError:
                continue
    out = {}
    bottoms = dict(li1="li_bottom", met1="m1_bottom", met2="m2_bottom", met3="m3_bottom", met4="m4_bottom")
    thick = dict(li1="li_thickness", met1="m1_thickness", met2="m2_thickness", met3="m3_thickness",
                 met4="m4_thickness", met5="m5_thickness")
    for layer, b in bottoms.items():
        out[layer] = (names[b], names[b] + names[thick[layer]])
    # met5 sits on ild6 above met4 (the script deposits it without a *_bottom name).
    out["met5"] = (out["met4"][0] + names["ild6_thickness"], out["met4"][0] + names["ild6_thickness"]
                   + names["m5_thickness"])
    return out


def stack_gf180(text: str) -> dict:
    out = {}
    for m in re.finditer(r"z\((Metal\d), zstart: \(([^)]*)\)\.nm, height: ([0-9.]+)\.nm", text):
        z0 = _arith(m[2], {}) / 1000.0
        out[m[1]] = (z0, z0 + float(m[3]) / 1000.0)
    return out


def load_stack(pdk: str) -> tuple[dict, dict]:
    path = PDK_ROOT / STACK_FILES[pdk]
    text = path.read_text()
    import hashlib
    stack = stack_sky130(text) if pdk == "sky130" else stack_gf180(text)
    missing = [l for l in LAYERS[pdk] if l not in stack]
    if missing:
        raise SystemExit(f"field_solver: stack file lacks {missing}")
    return stack, dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())


# ----------------------------------------------------------------------------
# Solver
# ----------------------------------------------------------------------------
def grid_lines(lo: float, hi: float, marks, hmin: float, hmax: float, growth: float = 1.35) -> np.ndarray:
    """Grid on [lo, hi] through every mark, spacing hmin next to each mark,
    growing geometrically (factor ``growth``) up to hmax."""
    marks = sorted({lo, hi, *[m for m in marks if lo < m < hi]})
    pts = [marks[0]]
    for a, b in zip(marks[:-1], marks[1:]):
        length = b - a
        steps, s, h = [], 0.0, hmin
        while 2.0 * (s + h) < length:
            steps.append(h)
            s += h
            h = min(h * growth, hmax)
        mid = length - 2.0 * s
        n_mid = max(1, int(np.ceil(mid / hmax - 1e-9)))
        seq = steps + [mid / n_mid] * n_mid + steps[::-1]
        x = a
        for st in seq[:-1]:
            x += st
            pts.append(x)
        pts.append(b)
    return np.array(pts)


class Solver:
    """Discrete Laplace problem on a rectilinear grid (see module doc)."""

    def __init__(self, xs, ys, zs, boxes):
        self.xs, self.ys, self.zs = xs, ys, zs
        nx, ny, nz = len(xs), len(ys), len(zs)
        n = nx * ny * nz
        if n > MAX_NODES:
            raise ValueError(f"grid of {n} nodes over the {MAX_NODES} budget")
        idx = np.arange(n).reshape(nx, ny, nz)
        cond = np.full((nx, ny, nz), -1, dtype=np.int32)
        cond[:, :, 0] = 0
        self.names = ["substrate"] + [b[0] for b in boxes]
        tol = 1e-9
        for k, (_, x0, x1, y0, y1, z0, z1) in enumerate(boxes):
            ix = (xs >= x0 - tol) & (xs <= x1 + tol)
            iy = (ys >= y0 - tol) & (ys <= y1 + tol)
            iz = (zs >= z0 - tol) & (zs <= z1 + tol)
            if not (ix.any() and iy.any() and iz.any()):
                raise ValueError(f"box {boxes[k][0]} has no grid node")
            cond[np.ix_(ix, iy, iz)] = k + 1

        def dual(v):
            d = np.zeros_like(v)
            d[1:] += (v[1:] - v[:-1]) / 2
            d[:-1] += (v[1:] - v[:-1]) / 2
            return d
        dx, dy, dz = dual(xs), dual(ys), dual(zs)
        hx, hy, hz = np.diff(xs), np.diff(ys), np.diff(zs)
        a_list, b_list, g_list = [], [], []
        if nx > 1:
            g = dy[None, :, None] * dz[None, None, :] / hx[:, None, None]
            a_list.append(idx[:-1].ravel()); b_list.append(idx[1:].ravel())
            g_list.append(np.broadcast_to(g, (nx - 1, ny, nz)).ravel())
        if ny > 1:
            g = dx[:, None, None] * dz[None, None, :] / hy[None, :, None]
            a_list.append(idx[:, :-1].ravel()); b_list.append(idx[:, 1:].ravel())
            g_list.append(np.broadcast_to(g, (nx, ny - 1, nz)).ravel())
        g = dx[:, None, None] * dy[None, :, None] / hz[None, None, :]
        a_list.append(idx[:, :, :-1].ravel()); b_list.append(idx[:, :, 1:].ravel())
        g_list.append(np.broadcast_to(g, (nx, ny, nz - 1)).ravel())
        self.a = np.concatenate(a_list)
        self.b = np.concatenate(b_list)
        self.g = np.concatenate(g_list) * EPS0_AF_PER_UM
        c = cond.ravel()
        self.c = c
        free = c < 0
        self.free = free
        fidx = -np.ones(n, dtype=np.int64)
        fidx[free] = np.arange(int(free.sum()))
        self.fidx = fidx
        self.nf = int(free.sum())
        fa, fb = free[self.a], free[self.b]
        both = fa & fb
        diag = np.zeros(self.nf)
        np.add.at(diag, fidx[self.a[fa]], self.g[fa])
        np.add.at(diag, fidx[self.b[fb]], self.g[fb])
        rows = np.concatenate([fidx[self.a[both]], fidx[self.b[both]]])
        cols = np.concatenate([fidx[self.b[both]], fidx[self.a[both]]])
        vals = np.concatenate([-self.g[both], -self.g[both]])
        self.lap = (sp.coo_matrix((vals, (rows, cols)), shape=(self.nf, self.nf)) + sp.diags(diag)).tocsc()
        self._lu = None

    def solve(self, excite: int) -> np.ndarray:
        """Node potentials with conductor ``excite`` at 1 V, all others 0."""
        c, a, b, g, free, fidx = self.c, self.a, self.b, self.g, self.free, self.fidx
        v_fixed = (c == excite).astype(float)
        rhs = np.zeros(self.nf)
        m = free[a] & ~free[b]
        np.add.at(rhs, fidx[a[m]], g[m] * v_fixed[b[m]])
        m = free[b] & ~free[a]
        np.add.at(rhs, fidx[b[m]], g[m] * v_fixed[a[m]])
        if self.nf <= DIRECT_MAX_NODES:
            if self._lu is None:
                self._lu = spla.splu(self.lap)
            x = self._lu.solve(rhs)
        else:
            pre = sp.diags(1.0 / self.lap.diagonal())
            x, info = spla.cg(self.lap.tocsr(), rhs, M=pre, rtol=1e-10, maxiter=50_000)
            if info != 0:
                raise RuntimeError(f"conjugate gradients did not converge ({info})")
        v = v_fixed.copy()
        v[free] = x
        return v

    def charges(self, v: np.ndarray) -> np.ndarray:
        c, a, b, g = self.c, self.a, self.b, self.g
        flow = g * (v[a] - v[b])
        q = np.zeros(len(self.names))
        ca, cb = c[a], c[b]
        m = (ca >= 0) & (ca != cb)
        np.add.at(q, ca[m], flow[m])
        m = (cb >= 0) & (ca != cb)
        np.add.at(q, cb[m], -flow[m])
        return q

    def row(self, name: str) -> dict:
        """Charges on every conductor with ``name`` at 1 V (aF): the diagonal is
        the total capacitance, the others are minus the couplings."""
        q = self.charges(self.solve(self.names.index(name)))
        return {n: float(q[i]) for i, n in enumerate(self.names)}


def solver_2d(boxes_xz, half_width=20.0, height=20.0, hmin=0.01, hmax=0.5) -> Solver:
    """Per-unit-length cross-section (one cell in y between mirror planes)."""
    xm = [v for b in boxes_xz for v in (b[1], b[2])]
    zm = [v for b in boxes_xz for v in (b[3], b[4])]
    xs = grid_lines(-half_width, half_width, xm, hmin, hmax)
    zs = grid_lines(0.0, height, [0.0, *zm], hmin, hmax)
    boxes = [(n, max(x0, -half_width), min(x1, half_width), 0.0, 1.0, z0, z1) for n, x0, x1, z0, z1 in boxes_xz]
    return Solver(xs, np.array([0.0, 1.0]), zs, boxes)


def solver_quarter(boxes, half_x, half_y, height=10.0, hmin=0.015, hmax=0.5) -> Solver:
    """Quarter domain [0, half_x] x [0, half_y] (mirror planes at 0) of a
    structure symmetric in x and y; charges are a quarter of the full ones."""
    xm = [v for b in boxes for v in (b[1], b[2])]
    ym = [v for b in boxes for v in (b[3], b[4])]
    zm = [v for b in boxes for v in (b[5], b[6])]
    xs = grid_lines(0.0, half_x, xm, hmin, hmax)
    ys = grid_lines(0.0, half_y, ym, hmin, hmax)
    zs = grid_lines(0.0, height, [0.0, *zm], hmin, hmax)
    clipped = [(n, max(x0, 0.0), min(x1, half_x), max(y0, 0.0), min(y1, half_y), z0, z1)
               for n, x0, x1, y0, y1, z0, z1 in boxes]
    return Solver(xs, ys, zs, clipped)


# ----------------------------------------------------------------------------
# Crossing study
# ----------------------------------------------------------------------------
def profile_widths(pdk: str) -> dict:
    prof = json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-nom.json").read_text())
    out = {}
    for layer in prof["layers"]:
        ctx = next(c for c in layer["contexts"] if c["over"] == 0 and c["under"] is None)
        out[layer["name"]] = dict(width=layer["width_um"], min_space=ctx["rows"][0][0])
    return out


def two_cut(stack, upper, lower, wu, wl, neighbours=None) -> dict:
    """The native two-cut crossing increment of the upper wire from this
    solver's own 2-D values: (upper over a lower plate - upper alone) x wl +
    (lower under an upper plate - lower alone) x wu. With ``neighbours`` the
    upper wire's cross-section has same-layer neighbours at that centre
    distance on both sides (the lower wire's cut, under the upper wire only,
    does not contain them)."""
    zu, zl = stack[upper], stack[lower]
    beside = [] if not neighbours else [(f"N{k}", c - wu / 2, c + wu / 2, *zu)
                                        for k, c in enumerate((neighbours, -neighbours))]
    cu0 = solver_2d([("A", -wu / 2, wu / 2, *zu)] + beside).row("A")["A"]
    cuo = solver_2d([("A", -wu / 2, wu / 2, *zu), ("P", -100, 100, *zl)] + beside).row("A")["A"]
    cl0 = solver_2d([("A", -wl / 2, wl / 2, *zl)]).row("A")["A"]
    clu = solver_2d([("A", -wl / 2, wl / 2, *zl), ("P", -100, 100, *zu)]).row("A")["A"]
    return dict(upper_alone_per_um=cu0, upper_over_plate_per_um=cuo, lower_alone_per_um=cl0,
                lower_under_plate_per_um=clu, increment_per_crossing=(cuo - cu0) * wl + (clu - cl0) * wu)


def crossing_3d(stack, upper, lower, wu, wl, pitch, neighbours=None, half_y=6.0, **kw) -> dict:
    """Upper wire along x crossed by lower wires along y every ``pitch`` um
    (mirror planes at +-pitch/2), optionally with same-layer upper neighbours
    at centre distance ``neighbours``; increment per crossing of the upper
    wire's total capacitance and the coupling per crossing."""
    zu, zl = stack[upper], stack[lower]
    upper_boxes = [("U", -1e3, 1e3, -wu / 2, wu / 2, *zu)]
    if neighbours:
        upper_boxes.append(("N", -1e3, 1e3, neighbours - wu / 2, neighbours + wu / 2, *zu))
    low = ("L", -wl / 2, wl / 2, -1e3, 1e3, *zl)
    start = time.monotonic()
    with_l = solver_quarter(upper_boxes + [low], pitch / 2, half_y, **kw)
    r = with_l.row("U")
    alone = solver_quarter(upper_boxes, pitch / 2, half_y, **kw).row("U")
    return dict(pitch_um=pitch, neighbour_centre_um=neighbours, nodes=int(with_l.c.size),
                increment_per_crossing=4.0 * (r["U"] - alone["U"]), coupling_per_crossing=-4.0 * r["L"],
                upper_alone_per_um=4.0 * alone["U"] / pitch, seconds=time.monotonic() - start)


PAIRS = {"sky130": [("met2", "met1"), ("met3", "met2")], "gf180": [("Metal2", "Metal1"), ("Metal3", "Metal2")]}
PITCHES = (None, 1.0, 2.0, 4.0, 8.0)  # None = minimum pitch of the lower layer
NEIGHBOURS = (None, "min", 1.0, 2.0, 3.0)  # upper-wire neighbour centre distance; None = no neighbours


def crossings(out: Path | None) -> dict:
    """3-D / two-cut factor of the upper wire's crossing increment over a grid
    of crossing pitch and same-layer neighbour distance, per adjacent pair."""
    result = dict(schema="rustroad.rc-field-solver.v2", dielectric="homogeneous, eps_r = 1 (ratios are "
                  "independent of eps_r)", units="aF at eps_r = 1", pdks={})
    for pdk, pairs in PAIRS.items():
        stack, source = load_stack(pdk)
        widths = profile_widths(pdk)
        entry = dict(stack_source=source, stack_um={k: list(v) for k, v in stack.items()}, pairs=[])
        for upper, lower in pairs:
            wu, wl = widths[upper]["width"], widths[lower]["width"]
            n_min = round(wu + widths[upper]["min_space"], 6)
            p_min = round(wl + widths[lower]["min_space"], 6)
            neighbours = [n_min if n == "min" else n for n in NEIGHBOURS]
            pitches = [p_min if p is None else p for p in PITCHES]
            two = {str(n): two_cut(stack, upper, lower, wu, wl, neighbours=n) for n in neighbours}
            rows = []
            for p in pitches:
                for n in neighbours:
                    r = crossing_3d(stack, upper, lower, wu, wl, p, neighbours=n,
                                    half_y=max(6.0, (n or 0.0) + 4.0))
                    r["two_cut_increment_per_crossing"] = two[str(n)]["increment_per_crossing"]
                    r["ratio_to_two_cut"] = r["increment_per_crossing"] / r["two_cut_increment_per_crossing"]
                    rows.append(r)
            entry["pairs"].append(dict(upper=upper, lower=lower, upper_width_um=wu, lower_width_um=wl,
                                       minimum_pitch_um=p_min, minimum_neighbour_centre_um=n_min,
                                       two_cut=two, grid=rows))
        result["pdks"][pdk] = entry
    result["grid_convergence"] = convergence()
    result["peak_rss_kib"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if out:
        out.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    return result


def convergence() -> dict:
    """The Sky130 met2-over-met1 crossing at 2 um pitch on a coarser and a finer grid."""
    stack, _ = load_stack("sky130")
    w = profile_widths("sky130")
    rows = {}
    for label, kw in (("coarse", dict(hmin=0.025, hmax=0.7)), ("default", {}),
                      ("fine", dict(hmin=0.01, hmax=0.35)), ("larger domain", dict(half_y=9.0, height=14.0))):
        rows[label] = crossing_3d(stack, "met2", "met1", w["met2"]["width"], w["met1"]["width"], 2.0, **kw)
    return rows


def markdown(result: dict) -> str:
    """3-D / two-cut factor per pair: rows are crossing pitches, columns the
    upper wire's neighbour centre distance (none = no neighbours)."""
    out = []
    for pdk, e in result["pdks"].items():
        for p in e["pairs"]:
            neigh = sorted({r["neighbour_centre_um"] for r in p["grid"] if r["neighbour_centre_um"]})
            cols = [None] + neigh
            out += [f"**{pdk} {p['upper']} over {p['lower']}** (two-cut increment without neighbours "
                    f"{p['two_cut']['None']['increment_per_crossing']:.2f} aF at eps_r = 1)", "",
                    "| crossing pitch (um) | " + " | ".join("no neighbours" if c is None else f"neighbours at {c:g} um"
                                                         for c in cols) + " |",
                    "|---|" + "---|" * len(cols)]
            for pitch in sorted({r["pitch_um"] for r in p["grid"]}):
                cells = []
                for c in cols:
                    r = next(x for x in p["grid"] if x["pitch_um"] == pitch and x["neighbour_centre_um"] == c)
                    cells.append(f"{r['ratio_to_two_cut']:.2f}")
                out.append(f"| {pitch:g} | " + " | ".join(cells) + " |")
            out.append("")
    g = result["grid_convergence"]
    out += ["Grid convergence (Sky130 met2 over met1, 2 um pitch, no neighbours): " + ", ".join(
        f"{k} {v['increment_per_crossing']:.2f} aF ({v['nodes']} nodes)" for k, v in g.items())]
    return "\n".join(out) + "\n"


def selftest() -> int:
    """Analytic and structural checks (also run by scripts/tests)."""
    # Parallel plates spanning the mirror-plane domain: C = eps0 * A / d exactly.
    s = solver_quarter([("P", -10, 10, -10, 10, 1.0, 1.2)], 2.0, 3.0, height=1.5, hmin=0.05, hmax=0.2)
    c = s.row("P")["P"] * 4.0
    exact = EPS0_AF_PER_UM * (4.0 * 6.0) / 1.0
    # The plate's top faces a zero-flux lid, so only the gap below carries field.
    assert abs(c / exact - 1.0) < 1e-9, (c, exact)
    # Symmetric capacitance matrix and monotone totals.
    s = solver_quarter([("U", -100, 100, -0.07, 0.07, 2.0, 2.36), ("L", -0.07, 0.07, -100, 100, 1.37, 1.73)],
                       1.0, 2.0, height=6.0, hmin=0.03, hmax=0.4)
    ru, rl = s.row("U"), s.row("L")
    assert abs(ru["L"] - rl["U"]) <= 1e-9 * abs(ru["L"]), (ru["L"], rl["U"])
    alone = solver_quarter([("U", -100, 100, -0.07, 0.07, 2.0, 2.36)], 1.0, 2.0, height=6.0, hmin=0.03,
                           hmax=0.4).row("U")["U"]
    assert ru["U"] > alone, (ru["U"], alone)
    print("field_solver selftest: passed")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    c = sub.add_parser("crossings")
    c.add_argument("--out", type=Path)
    c.add_argument("--markdown", action="store_true")
    sub.add_parser("selftest")
    args = parser.parse_args()
    if args.command == "selftest":
        return selftest()
    result = crossings(args.out)
    if args.markdown or not args.out:
        print(markdown(result), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
