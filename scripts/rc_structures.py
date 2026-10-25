#!/usr/bin/env python3
"""Canonical interconnect structures through both black-box oracles and the
native model.

Each structure is a few straight wires at DEF default width (half-width end
extensions), optionally with grounded supply metal of a given width (DEF
SPECIALNETS for OpenRCX, painted and labelled for Magic, a passive ground
conductor for the native model). OpenRCX reads it as a DEF with the PDK
technology LEF (no cells); Magic paints the identical rectangles and extracts
with its own rules; the native column runs the native extractor itself
(``extract_cpu_ref`` through the ``extract_structures`` example binary) with
the tracked RC profile of the corner, so it is the model the flow uses, not a
formula.

Structures (A is the upper or studied wire):

* isolated wire, pair at minimum spacing;
* crossing of an upper wire A over a lower wire B (and each alone);
* crossings over grounded rails 0.6/0.48 um and 3x that, and a wide plane;
* an upper-layer wire running along such a rail (aligned, the rail slightly
  wider) and along a 3x wider rail;
* a wire over dense crossings (minimum pitch) and over sparse crossings;
* the same sparse crossings under a wire with a minimum-space neighbour on
  one side, and on both sides (a sandwiched wire);
* parallel stacked wires on other layers (aligned; offset by most of a width).

Every structure that only adds conductors next to the studied wire must not
lower that wire's total capacitance (with all other conductors at 0 V, the
capacitance is an energy minimum over a smaller admissible set when a
conductor is added); ``monotonicity`` lists any extractor that does.

OpenRCX-only probes (DEF level, one net): a wire, the same wire split into two
paths at its middle, and with a via or a via stack at that point; the via
metal's capacitance is the difference to the split wire.

The point is a calibration-free comparison of the three models on structures
whose geometry is unambiguous. Nothing is fitted.

usage: rc_structures.py --out DIR --native-bin target/release/examples/extract_structures
                        [--evidence docs/evidence/rc-structures.json]
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import compare_spef as cs  # noqa: E402
import magic_rc  # noqa: E402
from verify_physical import PhysicalHarness  # noqa: E402

ROOT = HERE.parents[0]
LENGTH = 40.0
CORNER = "nom"
SPEC = {
    "sky130": dict(dbu=1000, lo="met1", hi="met2", top="met3", magic=dict(met1="m1", met2="m2", met3="m3"),
                   space=0.14, space_top=0.3, rail=0.48, vertical={"met2", "met4"},
                   via=dict(single="M1M2_PR", stack="M2M3_PR")),
    "gf180": dict(dbu=2000, lo="Metal1", hi="Metal2", top="Metal3", magic=dict(Metal1="m1", Metal2="m2", Metal3="m3"),
                  space=0.28, space_top=0.28, rail=0.6, vertical={"Metal2", "Metal4"},
                  via=dict(single="Via1_HV", stack="Via2_VH")),
}
GROUND = "VSS"


def profile(pdk: str) -> dict:
    return json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-{CORNER}.json").read_text())


def layers(pdk: str) -> dict:
    return {l["name"]: l for l in profile(pdk)["layers"]}


def structures(pdk: str) -> dict:
    """name -> list of (net, layer, x0, y0, x1, y1[, width]) centre lines in um;
    the ground net carries an explicit width."""
    s = SPEC[pdk]
    L = layers(pdk)
    hi, lo, top = s["hi"], s["lo"], s["top"]
    w_hi, w_lo, w_top = L[hi]["width_um"], L[lo]["width_um"], L[top]["width_um"]
    pitch_hi = w_hi + s["space"]
    rail = s["rail"]
    T = LENGTH
    dense = [(f"X{i}", hi, 12 + pitch_hi * i, 5, 12 + pitch_hi * i, 15) for i in range(int(36 / pitch_hi))]
    sparse = [(f"Y{i}", hi, 12 + 2.0 * i, 5, 12 + 2.0 * i, 15) for i in range(18)]
    p_top = w_top + s["space_top"]
    beside = [("B", top, 10, 10 + p_top, 10 + T, 10 + p_top), ("C", top, 10, 10 - p_top, 10 + T, 10 - p_top)]
    return {
        "isolated": [("A", hi, 10, 10, 10 + T, 10)],
        "pair_min_space": [("A", hi, 10, 10, 10 + T, 10), ("B", hi, 10, 10 + pitch_hi, 10 + T, 10 + pitch_hi)],
        "upper_alone": [("A", hi, 30, 10, 30, 30)],
        "lower_alone": [("B", lo, 20, 20, 40, 20)],
        "crossing": [("A", hi, 30, 10, 30, 30), ("B", lo, 20, 20, 40, 20)],
        "crossing_rail": [("A", hi, 30, 10, 30, 30), (GROUND, lo, 20, 20, 40, 20, rail)],
        "crossing_rail_3x": [("A", hi, 30, 10, 30, 30), (GROUND, lo, 20, 20, 40, 20, 3 * rail)],
        "crossing_plane": [("A", hi, 30, 10, 30, 30), (GROUND, lo, 20, 20, 40, 20, 5.0)],
        "top_alone": [("A", top, 10, 30, 10 + T, 30)],
        "top_along_rail": [("A", top, 10, 30, 10 + T, 30), (GROUND, lo, 5, 30, 15 + T, 30, rail)],
        "top_along_rail_3x": [("A", top, 10, 30, 10 + T, 30), (GROUND, lo, 5, 30, 15 + T, 30, 3 * rail)],
        "top_over_sparse_crossings": [("A", top, 10, 10, 10 + T, 10)] + sparse,
        "top_over_dense_crossings": [("A", top, 10, 10, 10 + T, 10)] + dense,
        "top_beside": [("A", top, 10, 10, 10 + T, 10)] + beside[:1],
        "top_beside_over_sparse_crossings": [("A", top, 10, 10, 10 + T, 10)] + beside[:1] + sparse,
        "top_sandwiched": [("A", top, 10, 10, 10 + T, 10)] + beside,
        "top_sandwiched_over_sparse_crossings": [("A", top, 10, 10, 10 + T, 10)] + beside + sparse,
        "stacked_aligned": [("A", hi, 10, 10, 10 + T, 10), ("B", lo, 10, 10, 10 + T, 10)],
        "stacked_offset": [("A", top, 10, 10, 10 + T, 10),
                           ("B", lo, 10, 10 + (w_top + w_lo) / 2 - 0.085, 10 + T,
                            10 + (w_top + w_lo) / 2 - 0.085)],
    }


def rect(pdk: str, wire) -> tuple:
    """(net, layer, lx, ly, ux, uy) in um: signal wires get half-width end
    extensions (DEF default); ground metal is drawn exactly as specified."""
    net, layer, x0, y0, x1, y1 = wire[:6]
    if net == GROUND:
        h, ext = wire[6] / 2, 0.0
    else:
        h = ext = layers(pdk)[layer]["width_um"] / 2
    lx, ux, ly, uy = min(x0, x1), max(x0, x1), min(y0, y1), max(y0, y1)
    if y0 == y1:
        return net, layer, lx - ext, ly - h, ux + ext, uy + h
    return net, layer, lx - h, ly - ext, ux + h, uy + ext


def def_text(pdk: str, wires) -> str:
    d = SPEC[pdk]["dbu"]
    signal = [w for w in wires if w[0] != GROUND]
    ground = [w for w in wires if w[0] == GROUND]
    nets = sorted({w[0] for w in signal})
    lines = ["VERSION 5.8 ;", 'DIVIDERCHAR "/" ;', 'BUSBITCHARS "[]" ;', "DESIGN syn ;",
             f"UNITS DISTANCE MICRONS {d} ;", f"DIEAREA ( 0 0 ) ( {100 * d} {100 * d} ) ;", f"PINS {len(nets)} ;"]
    for n in nets:
        net, layer, x0, y0 = next(w for w in signal if w[0] == n)[:4]
        lines.append(f"- {n}_p + NET {n} + DIRECTION INPUT + USE SIGNAL + PORT + LAYER {layer} ( -10 -10 ) ( 10 10 )"
                     f" + FIXED ( {round(x0 * d)} {round(y0 * d)} ) N ;")
    lines.append("END PINS")
    if ground:
        lines += ["SPECIALNETS 1 ;", f"- {GROUND}"]
        for k, (_, layer, x0, y0, x1, y1, width) in enumerate(ground):
            kw = "  + ROUTED" if k == 0 else "  NEW"
            lines.append(f"{kw} {layer} {round(width * d)} ( {round(x0 * d)} {round(y0 * d)} 0 ) "
                         f"( {round(x1 * d)} {round(y1 * d)} 0 )")
        lines[-1] += " + USE GROUND ;"
        lines.append("END SPECIALNETS")
    lines.append(f"NETS {len(nets)} ;")
    for n in nets:
        lines.append(f"- {n} ( PIN {n}_p ) + USE SIGNAL")
        for k, (_, layer, x0, y0, x1, y1) in enumerate(w for w in signal if w[0] == n):
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
        for wire in wires:
            net, layer, lx, ly, ux, uy = rect(pdk, wire)
            m = SPEC[pdk]["magic"][layer]
            body += [f"box {lx:.4f}um {ly:.4f}um {ux:.4f}um {uy:.4f}um", f"paint {m}",
                     f"box {lx:.4f}um {ly:.4f}um {lx:.4f}um {ly:.4f}um", f"label {net} c {m}"]
        body += ["extract do capacitance", "extract do coupling", "extract no resistance", "extract all"]
    body += ["puts RR_C_DONE", "quit -noprompt"]
    return "\n".join(body) + "\n"


def native_input(pdk: str, wires) -> str:
    """Profile and geometry for the extract_structures example (integer DBU)."""
    s = SPEC[pdk]
    d = s["dbu"]
    prof = profile(pdk)
    index = {l["name"]: i for i, l in enumerate(prof["layers"])}
    out = [f"D {d}"]
    for l in prof["layers"]:
        area = l.get("area_cap_pf_per_um2")
        out.append(f"L {l['name']} {l['rules_metal']} {round(l['width_um'] * d)} "
                   f"{1 if l['name'] in s['vertical'] else 0} {l['sheet_resistance_ohm_per_square']} "
                   f"{'-' if area is None else repr(area)}")
        for o in l.get("overlap_area", []):
            out.append(f"O {o['over']} {o['pf_per_um2']!r}")
        for c in l["contexts"]:
            out.append(f"C {c['over']} {-1 if c['under'] is None else c['under']} {len(c['rows'])}")
            out += [" ".join(repr(v) for v in row) for row in c["rows"]]
    nets = sorted({w[0] for w in wires if w[0] != GROUND})
    for wire in wires:
        if wire[0] == GROUND:
            _, layer, lx, ly, ux, uy = rect(pdk, wire)
            out.append(f"G {index[layer]} {round(lx * d)} {round(ly * d)} {round(ux * d)} {round(uy * d)}")
        else:
            net, layer, x0, y0, x1, y1 = wire
            out.append(f"W {nets.index(net)} {index[layer]} {round(x0 * d)} {round(y0 * d)} "
                       f"{round(x1 * d)} {round(y1 * d)}")
    return "\n".join(out) + "\n", nets


def native(pdk: str, cases: dict, binary: Path, out: Path) -> dict:
    result = {}
    for name, wires in cases.items():
        text, nets = native_input(pdk, wires)
        spec = out / f"native_{pdk}_{name}.txt"
        spec.write_text(text)
        run = subprocess.run([str(binary), str(spec)], capture_output=True, text=True, timeout=60)
        if run.returncode != 0:
            raise RuntimeError(f"native {pdk} {name}: {run.stderr.strip()}")
        result[name] = {}
        for line in run.stdout.splitlines():
            _, k, g, c = line.split()
            result[name][nets[int(k)]] = dict(total_af=(float(g) + float(c)) * 1e6, coupling_af=float(c) * 1e6)
    return result


def increments(table: dict) -> None:
    """Derived quantities: what each structure adds to the studied wire(s)."""
    def total(name, net):
        return table[name][net]["total_af"]
    table["crossing_increment"] = dict(A=dict(total_af=total("crossing", "A") - total("upper_alone", "A")),
                                       B=dict(total_af=total("crossing", "B") - total("lower_alone", "B")))
    for name in ("crossing_rail", "crossing_rail_3x", "crossing_plane"):
        table[f"{name}_increment"] = dict(A=dict(total_af=total(name, "A") - total("upper_alone", "A")))
    for name in ("top_along_rail", "top_along_rail_3x", "top_over_sparse_crossings", "top_over_dense_crossings"):
        table[f"{name}_increment"] = dict(A=dict(total_af=total(name, "A") - total("top_alone", "A")))
    table["top_beside_over_sparse_crossings_increment"] = dict(A=dict(
        total_af=total("top_beside_over_sparse_crossings", "A") - total("top_beside", "A")))
    table["top_sandwiched_over_sparse_crossings_increment"] = dict(A=dict(
        total_af=total("top_sandwiched_over_sparse_crossings", "A") - total("top_sandwiched", "A")))


# (structure with added conductors, reference structure, studied wire)
MONOTONE = [("crossing", "upper_alone", "A"), ("crossing", "lower_alone", "B"),
            ("crossing_rail", "upper_alone", "A"), ("crossing_rail_3x", "upper_alone", "A"),
            ("crossing_plane", "upper_alone", "A"), ("top_along_rail", "top_alone", "A"),
            ("top_along_rail_3x", "top_alone", "A"), ("top_over_sparse_crossings", "top_alone", "A"),
            ("top_over_dense_crossings", "top_alone", "A"), ("top_beside", "top_alone", "A"),
            ("top_beside_over_sparse_crossings", "top_beside", "A"), ("top_sandwiched", "top_alone", "A"),
            ("top_sandwiched_over_sparse_crossings", "top_sandwiched", "A")]


def monotonicity(table: dict) -> list:
    """Structures where adding conductors lowered the studied wire's total."""
    out = []
    for added, ref, net in MONOTONE:
        d = table[added][net]["total_af"] - table[ref][net]["total_af"]
        if d < -1e-6:
            out.append(dict(structure=added, reference=ref, wire=net, change_af=d))
    return out


def via_probe_def(pdk: str, lines: list[str]) -> str:
    d = SPEC[pdk]["dbu"]
    lo = SPEC[pdk]["lo"]
    return "\n".join(["VERSION 5.8 ;", 'DIVIDERCHAR "/" ;', 'BUSBITCHARS "[]" ;', "DESIGN syn ;",
                      f"UNITS DISTANCE MICRONS {d} ;", f"DIEAREA ( 0 0 ) ( {100 * d} {100 * d} ) ;", "PINS 1 ;",
                      f"- A_p + NET A + DIRECTION INPUT + USE SIGNAL + PORT + LAYER {lo} ( -10 -10 ) ( 10 10 )"
                      f" + FIXED ( {10 * d} {10 * d} ) N ;", "END PINS", "NETS 1 ;", "- A ( PIN A_p ) + USE SIGNAL"]
                     + lines + ["END NETS", "END DESIGN"]) + "\n"


def openrcx_via_probe(h, out: Path, pdk: str) -> dict:
    """OpenRCX per-net C of one lower-layer wire, the same wire split at its
    middle, and with a via / a via stack at that point (DEF level)."""
    s, d = SPEC[pdk], SPEC[pdk]["dbu"]
    lo, hi = s["lo"], s["hi"]
    a, m, b, y = 10 * d, 30 * d, 50 * d, 10 * d
    cases = {
        "wire": [f"  + ROUTED {lo} ( {a} {y} ) ( {b} {y} ) ;"],
        "split": [f"  + ROUTED {lo} ( {a} {y} ) ( {m} {y} )", f"  NEW {lo} ( {m} {y} ) ( {b} {y} ) ;"],
        "via": [f"  + ROUTED {lo} ( {a} {y} ) ( {b} {y} )", f"  NEW {lo} ( {m} {y} ) {s['via']['single']} ;"],
        "via_stack": [f"  + ROUTED {lo} ( {a} {y} ) ( {b} {y} )", f"  NEW {lo} ( {m} {y} ) {s['via']['single']}",
                      f"  NEW {hi} ( {m} {y} ) {s['via']['stack']} ;"],
    }
    result = {}
    for name, lines in cases.items():
        path = out / f"{pdk}_via_probe_{name}.def"
        path.write_text(via_probe_def(pdk, lines))
        spef, _ = cs.openrcx(h, f"{pdk}_via_probe_{name}", pdk, CORNER, path, f"{pdk}_via_probe_{name}_{CORNER}",
                             threshold="0")
        net = cs.parse_spef(spef)["nets"]["A"]
        result[name] = dict(total_af=(sum(net["ground"].values()) + sum(c for _, _, c in net["coupling"])) * 1e6,
                            r_ohm=sum(r for _, _, r in net["res"]))
    result["via_metal_af"] = result["via"]["total_af"] - result["split"]["total_af"]
    result["via_stack_metal_af"] = result["via_stack"]["total_af"] - result["split"]["total_af"]
    result["split_af"] = result["split"]["total_af"] - result["wire"]["total_af"]
    return result


def run(out: Path, binary: Path) -> dict:
    out.mkdir(parents=True, exist_ok=False)
    h = PhysicalHarness(out, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=60, tolerance_ns=0.001))
    result = dict(schema="rustroad.rc-structures.v3", length_um=LENGTH, corner=CORNER,
                  native_model="extract_cpu_ref via crates/rustroad-core/examples/extract_structures.rs",
                  pdks={})
    for pdk in SPEC:
        cases = structures(pdk)
        rcx = {}
        for name, wires in cases.items():
            path = out / f"{pdk}_{name}.def"
            path.write_text(def_text(pdk, wires))
            spef, _ = cs.openrcx(h, f"{pdk}_{name}", pdk, CORNER, path, f"{pdk}_{name}_{CORNER}")
            parsed = cs.parse_spef(spef)
            rcx[name] = {n: dict(total_af=(sum(v["ground"].values()) + sum(c for _, _, c in v["coupling"])) * 1e6,
                                 coupling_af=sum(c for _, _, c in v["coupling"]) * 1e6)
                         for n, v in parsed["nets"].items()}
        receipt, work = magic_rc.run_magic(out, f"magic_{pdk}", pdk, None, "none", "ngspice()",
                                           magic_tcl(pdk, cases))
        mag = {}
        for name, wires in cases.items():
            signal = {w[0] for w in wires if w[0] != GROUND}
            ext = magic_rc.parse_ext(work / f"{name}.ext")
            totals, partners = magic_rc.node_totals(ext)
            mag[name] = {n: dict(total_af=v, coupling_af=sum(c for k, c in partners[n].items()
                                                             if k in signal and k != n))
                         for n, v in totals.items() if n in signal}
        nat = native(pdk, cases, binary, out)
        for table in (rcx, mag, nat):
            increments(table)
        result["pdks"][pdk] = dict(structures={k: [list(w) for w in v] for k, v in cases.items()},
                                   openrcx=rcx, magic=mag, native=nat,
                                   monotonicity={t: monotonicity(x) for t, x in
                                                 (("openrcx", rcx), ("magic", mag), ("native", nat))},
                                   openrcx_via_probe=openrcx_via_probe(h, out, pdk))
    (out / "structures.json").write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    return result


ROWS = [
    ("isolated", "A", "total_af", "isolated wire, total aF"),
    ("pair_min_space", "A", "coupling_af", "pair at minimum space, coupling aF"),
    ("crossing_increment", "A", "total_af", "crossing, upper wire increment aF"),
    ("crossing_increment", "B", "total_af", "crossing, lower wire increment aF"),
    ("crossing", "A", "coupling_af", "crossing, coupling aF (Magic only; OpenRCX grounds it)"),
    ("crossing_rail_increment", "A", "total_af", "crossing a grounded rail, increment aF"),
    ("crossing_rail_3x_increment", "A", "total_af", "crossing a 3x rail, increment aF"),
    ("crossing_plane_increment", "A", "total_af", "crossing a 5 um plane, increment aF"),
    ("top_along_rail_increment", "A", "total_af", "upper-layer wire along a rail, increment aF"),
    ("top_along_rail_3x_increment", "A", "total_af", "upper-layer wire along a 3x rail, increment aF"),
    ("top_over_sparse_crossings_increment", "A", "total_af", "over 2 um-pitch crossings, increment aF"),
    ("top_over_dense_crossings_increment", "A", "total_af", "over minimum-pitch crossings, increment aF"),
    ("top_beside_over_sparse_crossings_increment", "A", "total_af",
     "over 2 um-pitch crossings with a minimum-space neighbour on one side, increment aF"),
    ("top_sandwiched_over_sparse_crossings_increment", "A", "total_af",
     "over 2 um-pitch crossings with minimum-space neighbours on both sides, increment aF"),
    ("stacked_aligned", "A", "coupling_af", "stacked aligned, coupling aF"),
    ("stacked_aligned", "B", "total_af", "stacked aligned, lower wire total aF"),
    ("stacked_offset", "A", "coupling_af", "stacked offset, coupling aF"),
]


def markdown(result: dict) -> str:
    out = [f"| PDK | structure ({LENGTH:g} um wires, {CORNER}) | native | OpenRCX | Magic |", "|---|---|---|---|---|"]
    for pdk, d in result["pdks"].items():
        for name, net, key, label in ROWS:
            vals = [d[t].get(name, {}).get(net, {}).get(key) for t in ("native", "openrcx", "magic")]
            out.append(f"| {pdk} | {label} | " + " | ".join("-" if v is None else f"{v:.1f}" for v in vals) + " |")
    out += ["", "| PDK | extractor | structures where adding conductors lowered the studied wire's total |",
            "|---|---|---|"]
    for pdk, d in result["pdks"].items():
        for tool, rows in d["monotonicity"].items():
            text = "; ".join(f"{r['structure']} ({r['change_af']:+.1f} aF)" for r in rows) or "none"
            out.append(f"| {pdk} | {tool} | {text} |")
    out += ["", "| PDK | OpenRCX: wire split at its middle, aF | via at that point, aF | via stack, aF |",
            "|---|---|---|---|"]
    for pdk, d in result["pdks"].items():
        v = d["openrcx_via_probe"]
        out.append(f"| {pdk} | {v['split_af']:+.2f} | {v['via_metal_af']:+.2f} | {v['via_stack_metal_af']:+.2f} |")
    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--native-bin", type=Path, required=True,
                        help="cargo build --release --examples -p rustroad-core; target/release/examples/extract_structures")
    parser.add_argument("--evidence", type=Path)
    args = parser.parse_args()
    result = run(args.out.resolve(), args.native_bin.resolve())
    if args.evidence:
        args.evidence.write_text(json.dumps(result, indent=1, sort_keys=True) + "\n")
    print(markdown(result), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
