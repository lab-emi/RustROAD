#!/usr/bin/env python3
"""Canonical interconnect structures through both black-box oracles and the
native model.

Each structure is a few straight wires (DEF default width and half-width end
extensions). OpenRCX reads it as a DEF with the PDK technology LEF (no cells);
Magic paints the identical rectangles (wire plus extensions) and extracts
with its own rules. The native column evaluates the native extractor's model
on the same geometry from the tracked RC profile (calibration-table rows):

* isolated wire: 2 cg_iso (L + 2 w)  (two sides over the body and the end
  extensions, plus both end faces);
* parallel pair at spacing s: (L + w) (cg(s) + cg_iso) + 2 w cg_iso ground and
  (L + w) cc(s) coupling per wire;
* crossing of an upper wire A over a lower wire B: both cross-sections count.
  The plate B shields A's substrate over B's width, so the coupling is
  2 gP_A w_B + 2 (gU_B - g0_B) w_A; A's total rises by 2 (gP_A - g0_A) w_B +
  2 (gU_B - g0_B) w_A and B's by the coupling;
* parallel stacked (aligned, or offset) wires on other layers: neither wire
  extends one strip width beyond the other's edges, so no calibration plate
  applies and the native model adds nothing (documented as unmodelled).

The point is a calibration-free comparison of the three models on structures
where the geometry is unambiguous. Nothing is fitted.

usage: rc_structures.py --out DIR [--evidence docs/evidence/rc-structures.json]
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import compare_spef as cs  # noqa: E402
import magic_rc  # noqa: E402
from verify_physical import PhysicalHarness  # noqa: E402

ROOT = HERE.parents[0]
LENGTH = 40.0
SPEC = {
    "sky130": dict(dbu=1000, lo="met1", hi="met2", top="met3", magic=dict(met1="m1", met2="m2", met3="m3"),
                   space=0.14),
    "gf180": dict(dbu=2000, lo="Metal1", hi="Metal2", top="Metal3", magic=dict(Metal1="m1", Metal2="m2", Metal3="m3"),
                  space=0.28),
}


def profile(pdk: str) -> dict:
    p = json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-nom.json").read_text())
    return {l["name"]: l for l in p["layers"]}


def rows(layer: dict, over: int, under) -> list:
    for c in layer["contexts"]:
        if c["over"] == over and c["under"] == under:
            return c["rows"]
    raise KeyError((layer["name"], over, under))


def sample(rows_, spacing):
    last = rows_[-1]
    if spacing is None or spacing >= last[0]:
        return last[1], last[2]
    if spacing <= rows_[0][0]:
        return rows_[0][1], rows_[0][2]
    for a, b in zip(rows_, rows_[1:]):
        if a[0] <= spacing <= b[0]:
            t = (spacing - a[0]) / (b[0] - a[0])
            return a[1] + t * (b[1] - a[1]), a[2] + t * (b[2] - a[2])
    raise ValueError(spacing)


def structures(pdk: str) -> dict:
    """name -> list of (net, layer, x0, y0, x1, y1) centre lines in um."""
    s = SPEC[pdk]
    L = profile(pdk)
    w_hi, w_lo = L[s["hi"]]["width_um"], L[s["lo"]]["width_um"]
    w_top = L[s["top"]]["width_um"]
    pitch = w_hi + s["space"]
    return {
        "isolated": [("A", s["hi"], 10, 10, 10 + LENGTH, 10)],
        "pair_min_space": [("A", s["hi"], 10, 10, 10 + LENGTH, 10), ("B", s["hi"], 10, 10 + pitch, 10 + LENGTH, 10 + pitch)],
        "upper_alone": [("A", s["hi"], 30, 10, 30, 30)],
        "lower_alone": [("B", s["lo"], 20, 20, 40, 20)],
        "crossing": [("A", s["hi"], 30, 10, 30, 30), ("B", s["lo"], 20, 20, 40, 20)],
        "stacked_aligned": [("A", s["hi"], 10, 10, 10 + LENGTH, 10), ("B", s["lo"], 10, 10, 10 + LENGTH, 10)],
        "stacked_offset": [("A", s["top"], 10, 10, 10 + LENGTH, 10),
                           ("B", s["lo"], 10, 10 + (w_top + w_lo) / 2 - 0.085, 10 + LENGTH,
                            10 + (w_top + w_lo) / 2 - 0.085)],
    }


def rects(pdk: str, wires) -> list:
    L = profile(pdk)
    out = []
    for net, layer, x0, y0, x1, y1 in wires:
        h = L[layer]["width_um"] / 2
        out.append((net, layer, min(x0, x1) - h, min(y0, y1) - h, max(x0, x1) + h, max(y0, y1) + h))
    return out


def native(pdk: str) -> dict:
    s = SPEC[pdk]
    L = profile(pdk)
    hi, lo = L[s["hi"]], L[s["lo"]]
    w_hi, w_lo = hi["width_um"], lo["width_um"]
    iso = lambda layer: sample(rows(layer, 0, None), None)[1]  # noqa: E731
    out = {}
    out["isolated"] = dict(A=dict(total_af=2 * iso(hi) * (LENGTH + 2 * w_hi) * 1e6))
    cc, cg = sample(rows(hi, 0, None), s["space"])
    ground = (LENGTH + w_hi) * (cg + iso(hi)) + 2 * w_hi * iso(hi)
    coupling = (LENGTH + w_hi) * cc
    out["pair_min_space"] = {n: dict(total_af=(ground + coupling) * 1e6, coupling_af=coupling * 1e6) for n in "AB"}
    g0_a, gp_a = iso(hi), sample(rows(hi, lo["rules_metal"], None), None)[1]
    g0_b, gu_b = iso(lo), sample(rows(lo, 0, hi["rules_metal"]), None)[1]
    coupling = 2 * gp_a * w_lo + 2 * (gu_b - g0_b) * w_hi
    out["crossing_increment"] = dict(A=dict(total_af=(2 * (gp_a - g0_a) * w_lo + 2 * (gu_b - g0_b) * w_hi) * 1e6),
                                     B=dict(total_af=coupling * 1e6), coupling_af=coupling * 1e6)
    out["stacked_aligned"] = dict(coupling_af=0.0)
    out["stacked_offset"] = dict(coupling_af=0.0)
    return out


def def_text(pdk: str, wires) -> str:
    d = SPEC[pdk]["dbu"]
    nets = sorted({w[0] for w in wires})
    lines = ["VERSION 5.8 ;", 'DIVIDERCHAR "/" ;', 'BUSBITCHARS "[]" ;', "DESIGN syn ;",
             f"UNITS DISTANCE MICRONS {d} ;", f"DIEAREA ( 0 0 ) ( {100 * d} {100 * d} ) ;", f"PINS {len(nets)} ;"]
    for n in nets:
        net, layer, x0, y0 = next(w for w in wires if w[0] == n)[:4]
        lines.append(f"- {n}_p + NET {n} + DIRECTION INPUT + USE SIGNAL + PORT + LAYER {layer} ( -10 -10 ) ( 10 10 )"
                     f" + FIXED ( {round(x0 * d)} {round(y0 * d)} ) N ;")
    lines += ["END PINS", f"NETS {len(nets)} ;"]
    for n in nets:
        lines.append(f"- {n} ( PIN {n}_p ) + USE SIGNAL")
        segs = [w for w in wires if w[0] == n]
        for k, (_, layer, x0, y0, x1, y1) in enumerate(segs):
            kw = "  + ROUTED" if k == 0 else "  NEW"
            lines.append(f"{kw} {layer} ( {round(x0 * d)} {round(y0 * d)} ) ( {round(x1 * d)} {round(y1 * d)} )")
        lines[-1] += " ;"
    lines += ["END NETS", "END DESIGN"]
    return "\n".join(lines) + "\n"


def magic_tcl(pdk: str, cases: dict) -> str:
    body = ["crashbackups stop", "drc off", "set od $::env(OUTDIR)", "file mkdir $od", "cd $od",
            "extract style $::env(STYLE)"]
    for name, wires in cases.items():
        body += [f"cellname create {name}", f"load {name}"]
        for net, layer, lx, ly, ux, uy in rects(pdk, wires):
            m = SPEC[pdk]["magic"][layer]
            body += [f"box {lx:.4f}um {ly:.4f}um {ux:.4f}um {uy:.4f}um", f"paint {m}",
                     f"box {lx:.4f}um {ly:.4f}um {lx:.4f}um {ly:.4f}um", f"label {net} c {m}"]
        body += ["extract do capacitance", "extract do coupling", "extract no resistance", "extract all"]
    body += ["puts RR_C_DONE", "quit -noprompt"]
    return "\n".join(body) + "\n"


def run(out: Path) -> dict:
    out.mkdir(parents=True, exist_ok=False)
    h = PhysicalHarness(out, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=60, tolerance_ns=0.001))
    result = dict(schema="rustroad.rc-structures.v1", length_um=LENGTH, corner="nom", pdks={})
    for pdk in SPEC:
        cases = structures(pdk)
        rcx = {}
        for name, wires in cases.items():
            path = out / f"{pdk}_{name}.def"
            path.write_text(def_text(pdk, wires))
            spef, _ = cs.openrcx(h, f"{pdk}_{name}", pdk, "nom", path, f"{pdk}_{name}_nom")
            parsed = cs.parse_spef(spef)
            rcx[name] = {n: dict(total_af=(sum(v["ground"].values()) + sum(c for _, _, c in v["coupling"])) * 1e6,
                                 coupling_af=sum(c for _, _, c in v["coupling"]) * 1e6)
                         for n, v in parsed["nets"].items()}
        receipt, work = magic_rc.run_magic(out, f"magic_{pdk}", pdk, None, "none", "ngspice()",
                                           magic_tcl(pdk, cases))
        mag = {}
        for name in cases:
            ext = magic_rc.parse_ext(work / f"{name}.ext")
            totals, partners = magic_rc.node_totals(ext)
            mag[name] = {n: dict(total_af=v, coupling_af=sum(c for k, c in partners[n].items()
                                                             if k in ("A", "B") and k != n))
                         for n, v in totals.items() if n in ("A", "B")}
        for table in (rcx, mag):
            table["crossing_increment"] = dict(
                A=dict(total_af=table["crossing"]["A"]["total_af"] - table["upper_alone"]["A"]["total_af"]),
                B=dict(total_af=table["crossing"]["B"]["total_af"] - table["lower_alone"]["B"]["total_af"]))
        result["pdks"][pdk] = dict(structures={k: [list(w) for w in v] for k, v in cases.items()},
                                   openrcx=rcx, magic=mag, native=native(pdk))
    (out / "structures.json").write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    return result


def markdown(result: dict) -> str:
    out = ["| PDK | structure | quantity | native | OpenRCX | Magic |", "|---|---|---|---|---|---|"]
    for pdk, d in result["pdks"].items():
        n, x, m = d["native"], d["openrcx"], d["magic"]
        out.append(f"| {pdk} | isolated {LENGTH:g} um | total aF | {n['isolated']['A']['total_af']:.0f} | "
                   f"{x['isolated']['A']['total_af']:.0f} | {m['isolated']['A']['total_af']:.0f} |")
        out.append(f"| {pdk} | pair at minimum space | total / coupling aF | {n['pair_min_space']['A']['total_af']:.0f} / "
                   f"{n['pair_min_space']['A']['coupling_af']:.0f} | {x['pair_min_space']['A']['total_af']:.0f} / "
                   f"{x['pair_min_space']['A']['coupling_af']:.0f} | {m['pair_min_space']['A']['total_af']:.0f} / "
                   f"{m['pair_min_space']['A']['coupling_af']:.0f} |")
        out.append(f"| {pdk} | crossing | upper / lower increment aF | {n['crossing_increment']['A']['total_af']:.1f} / "
                   f"{n['crossing_increment']['B']['total_af']:.1f} | {x['crossing_increment']['A']['total_af']:.1f} / "
                   f"{x['crossing_increment']['B']['total_af']:.1f} | {m['crossing_increment']['A']['total_af']:.1f} / "
                   f"{m['crossing_increment']['B']['total_af']:.1f} |")
        for name in ("stacked_aligned", "stacked_offset"):
            out.append(f"| {pdk} | {name.replace('_', ' ')} {LENGTH:g} um | coupling aF | {n[name]['coupling_af']:.0f} | "
                       f"{x[name]['A']['coupling_af']:.0f} | {m[name]['A']['coupling_af']:.0f} |")
    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    result = run(args.out.resolve())
    if args.evidence:
        args.evidence.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(markdown(result), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
