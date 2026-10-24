#!/usr/bin/env python3
"""Interconnect R and C as stated by every PDK data source, per corner.

Reads only PDK data files (pinned by sha256 in the output) and the tracked
native RC profiles, and tabulates side by side:

* sheet resistance per routing layer: Magic extraction rules (the variant
  each native profile corner uses), technology LEF ``RPERSQ``, the SPICE
  interconnect corner models, and the per-length ``RESOVER`` value of every
  calibration-table variant shipped with the PDK, converted to ohm/sq at the
  table width (R_per_um * width);
* contact (via) resistance per cut layer from the same sources, and per Magic
  variant how many distinct values its contact block gives (a block that
  gives every contact type one value is a placeholder, not corner data);
* isolated-wire capacitance per um per routing layer: the calibration table
  the native extractor and the OpenRCX reference read (2 x the last OVER-0
  ground row, per side) against Magic's area + perimeter rules and the LEF
  CPERSQDIST/EDGECAPACITANCE values.

No reference tool output is read. The point is to show which data source
the native profile follows and how far the sources disagree.

usage: rc_data_sources.py [--out docs/evidence/rc-data-sources.json] [--markdown]
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
MAX_BYTES = 32 * 1024 * 1024
CORNER_VARIANT = dict(min="(lrlc)", nom="()", max="(hrhc)")
PDK = {
    "sky130": dict(
        root="sky130A",
        magic="libs.tech/magic/sky130A.tech",
        lef="libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef",
        tables={v: f"libs.tech/librelane/rules.openrcx.sky130A.{{c}}.{v}" for v in ("calibre", "magic", "spef_extractor")},
        reference_table="calibre",
        spice={"min": "libs.tech/ngspice/r+c/res_low__cap_low.spice",
               "nom": "libs.tech/ngspice/r+c/res_typical__cap_typical.spice",
               "max": "libs.tech/ngspice/r+c/res_high__cap_high.spice"},
        layers=[("li1", 1, "allli", "locali", "rl1"), ("met1", 2, "allm1", "metal1", "rm1"),
                ("met2", 3, "allm2", "metal2", "rm2"), ("met3", 4, "allm3", "metal3", "rm3"),
                ("met4", 5, "allm4", "metal4", "rm4"), ("met5", 6, "allm5", "metal5", "rm5")],
        cuts=[("mcon", "mcon", "rcl1"), ("via", "m2c", "rcvia"), ("via2", "m3c", "rcvia2"),
              ("via3", "via3", "rcvia3"), ("via4", "via4", "rcvia4")],
    ),
    "gf180": dict(
        root="gf180mcuD",
        magic="libs.tech/magic/gf180mcuD.tech",
        lef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef",
        tables={"plain": "libs.tech/librelane/rules.openrcx.gf180mcuD.{c}",
                "magic": "libs.tech/librelane/rules.openrcx.gf180mcuD.{c}.magic"},
        reference_table="plain",
        spice={"min": ("libs.tech/ngspice/sm141064.ngspice", "res_ff"),
               "nom": ("libs.tech/ngspice/sm141064.ngspice", "res_typical"),
               "max": ("libs.tech/ngspice/sm141064.ngspice", "res_ss")},
        layers=[("Metal1", 1, "allm1", "metal1", "rsh_rm1"), ("Metal2", 2, "allm2", "metal2", "rsh_rm2"),
                ("Metal3", 3, "allm3", "metal3", "rsh_rm3"), ("Metal4", 4, "allm4", "metal4", "rsh_rm4"),
                ("Metal5", 5, "allm5", "metal5", "rsh_tm11k")],
        cuts=[("Via1", "m2c", None), ("Via2", "m3c", None), ("Via3", "via3", None), ("Via4", "via4", None)],
    ),
}


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read(path: Path, sources: dict) -> str:
    if not path.is_file() or path.stat().st_size > MAX_BYTES:
        raise SystemExit(f"missing or oversized data file: {path}")
    sources[str(path)] = sha256(path)
    return path.read_text(errors="replace")


def pdk_root(pdk: str) -> Path:
    manifest = json.loads((ROOT / "fixtures/flow/signoff-pinned.json").read_text())
    return Path(manifest["pdks"][pdk]["host_root"])


def magic_rules(tech: str) -> dict:
    """variant -> {('resist', alias): mohm/sq, ('contact', name): mohm, ('areacap', alias): aF/um2,
    ('perimeter', alias): aF/um} from the extract section's ngspice style."""
    section = tech.split("\nextract", 1)[1].split("\nend", 1)[0]
    lines = section.splitlines()
    all_variants = None
    for line in lines:
        m = re.match(r"\s*style\s+ngspice\s+variants\s+(.*)$", line)
        if m:
            all_variants = [v.strip() for v in m[1].split(",")]
            break
    if not all_variants:
        raise SystemExit("ngspice extraction style not found")
    out = {v: {} for v in all_variants}
    active = set(all_variants)
    in_style = False
    for line in lines:
        t = line.split()
        if not t or t[0].startswith("#"):
            continue
        if t[0] == "style":
            in_style = len(t) > 1 and t[1] == "ngspice"
            active = set(all_variants)
            continue
        if not in_style:
            continue
        if t[0] == "variants":
            spec = " ".join(t[1:])
            active = set(all_variants) if spec == "*" else {v.strip() for v in spec.split(",")}
            continue
        key = None
        if t[0] == "resist" and len(t) >= 3:
            m = re.match(r"\(?([^)/]+)\)?/(\S+)", t[1])
            if m:
                key, value = ("resist", m[1]), float(t[2])
        elif t[0] == "contact" and len(t) >= 3:
            for name in t[1].split(","):
                for v in active:
                    out.setdefault(v, {})[("contact", name)] = float(t[-1])
            continue
        elif t[0] in ("defaultareacap", "defaultperimeter") and len(t) == 4:
            key, value = (t[0].replace("default", ""), t[1]), float(t[3])
        if key:
            for v in active:
                out.setdefault(v, {})[key] = value
    return out


def table(text: str) -> dict:
    """metal -> dict(width, resover_ohm_per_um, iso_ground_pf_per_um_per_side)."""
    out = {}
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = re.match(r"Metal (\d+) RESOVER 0$", line)
        if m:
            width = float(lines[i + 1].split()[-1])
            values = set()
            j = i + 2
            while not lines[j].startswith("END"):
                values.add(float(lines[j].split()[3]))
                j += 1
            out.setdefault(int(m[1]), {}).update(width=width, resover_ohm_per_nm=sorted(values))
        m = re.match(r"Metal (\d+) OVER 0$", line)
        if m:
            j = i + 2
            last = None
            while not lines[j].startswith("END"):
                last = [float(x) for x in lines[j].split()]
                j += 1
            out.setdefault(int(m[1]), {}).update(iso_ground_pf_per_um_per_side=last[2],
                                                  width=float(lines[i + 1].split()[-1]))
    return out


def lef_values(text: str) -> dict:
    out = {}
    for block in re.finditer(r"(?ms)^LAYER (\S+)(.*?)^END \1", text):
        body = block[2]
        entry = {}
        for key, pattern in (("rpersq", r"RESISTANCE RPERSQ ([0-9.Ee+-]+)"),
                             ("cut_ohm", r"^\s*RESISTANCE ([0-9.Ee+-]+)\s*;"),
                             ("cpersqdist_pf", r"CAPACITANCE CPERSQDIST ([0-9.Ee+-]+)"),
                             ("edge_pf", r"EDGECAPACITANCE ([0-9.Ee+-]+)"), ("width", r"^\s*WIDTH ([0-9.]+)")):
            m = re.search(pattern, body, re.M)
            if m:
                entry[key] = float(m[1])
        out[block[1]] = entry
    return out


def spice_params(text: str, lib: str | None) -> dict:
    if lib:
        m = re.search(rf"(?ms)^\.lib {lib}\s*$(.*?)^\.endl {lib}", text)
        text = m[1] if m else ""
    params = {}
    for name, value in re.findall(r"(?m)(?:^\+|\.param)\s*(\w+)\s*=\s*'?([0-9.eE+-]+(?:[+-][0-9.eE+-]+)?)'?", text):
        try:
            params[name] = eval(value, {"__builtins__": {}})  # noqa: S307 - numeric literal sums only
        except (SyntaxError, NameError):
            continue
    return params


def collect(pdk: str) -> dict:
    spec = PDK[pdk]
    root = pdk_root(pdk)
    sources = {}
    magic = magic_rules(read(root / spec["magic"], sources))
    lef = lef_values(read(root / spec["lef"], sources))
    tables = {v: {c: table(read(root / p.format(c=c), sources)) for c in ("min", "nom", "max")}
              for v, p in spec["tables"].items()}
    spice = {}
    for corner, entry in spec["spice"].items():
        path, lib = (entry, None) if isinstance(entry, str) else entry
        spice[corner] = spice_params(read(root / path, sources), lib)
    profiles = {c: json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-{c}.json").read_text()) for c in ("min", "nom", "max")}
    for c in profiles:
        sources[str(ROOT / f"fixtures/pdk/{pdk}-rc-{c}.json")] = sha256(ROOT / f"fixtures/pdk/{pdk}-rc-{c}.json")
    rows_r, rows_v, rows_c = [], [], []
    for corner in ("min", "nom", "max"):
        variant = CORNER_VARIANT[corner]
        prof = {l["name"]: l for l in profiles[corner]["layers"]}
        cuts = {c["name"]: c for c in profiles[corner]["cuts"]}
        for name, metal, alias, plane, spice_name in spec["layers"]:
            magic_r = magic.get(variant, {}).get(("resist", alias))
            row = dict(pdk=pdk, corner=corner, layer=name, native=prof.get(name, {}).get("sheet_resistance_ohm_per_square"),
                       magic=magic_r / 1000.0 if magic_r is not None else None,
                       lef_nominal=lef.get(name, {}).get("rpersq"),
                       spice=spice[corner].get(spice_name))
            for v, per_corner in tables.items():
                t = per_corner[corner].get(metal)
                row[f"table_{v}"] = (t["resover_ohm_per_nm"][0] * 1000.0 * t["width"]
                                     if t and t.get("resover_ohm_per_nm") else None)
            rows_r.append(row)
            area = magic.get(variant, {}).get(("areacap", alias))
            perim = magic.get(variant, {}).get(("perimeter", alias))
            w = prof.get(name, {}).get("width_um")
            ref = tables[spec["reference_table"]][corner].get(metal, {})
            iso_table = 2.0 * ref["iso_ground_pf_per_um_per_side"] * 1e6 if ref.get("iso_ground_pf_per_um_per_side") else None
            iso_magic = area * w + 2.0 * perim if area is not None and perim is not None and w else None
            l = lef.get(name, {})
            iso_lef = ((l["cpersqdist_pf"] * w + 2.0 * l["edge_pf"]) * 1e6
                       if "cpersqdist_pf" in l and "edge_pf" in l and w else None)
            rows_c.append(dict(pdk=pdk, corner=corner, layer=name, width_um=w, table_af_per_um=iso_table,
                               magic_af_per_um=iso_magic, lef_nominal_af_per_um=iso_lef,
                               table_vs_magic=(iso_table / iso_magic - 1.0) if iso_table and iso_magic else None))
        for name, magic_name, spice_name in spec["cuts"]:
            mv = magic.get(variant, {}).get(("contact", magic_name))
            rows_v.append(dict(pdk=pdk, corner=corner, cut=name, native=cuts.get(name, {}).get("resistance_ohm_per_cut"),
                               magic=mv / 1000.0 if mv is not None else None,
                               lef_nominal=lef.get(name, {}).get("cut_ohm"),
                               spice=spice[corner].get(spice_name) if spice_name else None))
    blocks = {}
    for variant, entries in sorted(magic.items()):
        values = {k[1]: v / 1000.0 for k, v in entries.items() if k[0] == "contact"}
        if values:
            blocks[variant] = dict(contact_types=len(values), distinct_ohm=sorted(set(values.values())),
                                   blanket=len(values) >= 3 and len(set(values.values())) == 1)
    return dict(pdk=pdk, sources=sources, sheet_resistance=rows_r, via_resistance=rows_v,
                isolated_capacitance=rows_c, reference_table_variant=spec["reference_table"],
                magic_contact_blocks=blocks)


def markdown(data: dict) -> str:
    f = lambda v, d=4: "-" if v is None else f"{v:.{d}g}"  # noqa: E731
    out = []
    for p in data["pdks"]:
        variants = [k for k in p["sheet_resistance"][0] if k.startswith("table_")]
        out += [f"**{p['pdk']}: sheet resistance (ohm/sq)**", "",
                "| corner | layer | native | Magic | LEF (nom) | SPICE | " + " | ".join(variants) + " |",
                "|---|---|---|---|---|---|" + "---|" * len(variants)]
        for r in p["sheet_resistance"]:
            out.append(f"| {r['corner']} | {r['layer']} | {f(r['native'])} | {f(r['magic'])} | {f(r['lef_nominal'])} | "
                       f"{f(r['spice'])} | " + " | ".join(f(r[v]) for v in variants) + " |")
        out += ["", f"**{p['pdk']}: contact resistance (ohm per cut)**", "",
                "| corner | cut | native | Magic | LEF (nom) | SPICE |", "|---|---|---|---|---|---|"]
        for r in p["via_resistance"]:
            out.append(f"| {r['corner']} | {r['cut']} | {f(r['native'])} | {f(r['magic'])} | {f(r['lef_nominal'])} | {f(r['spice'])} |")
        out += ["", f"**{p['pdk']}: Magic contact blocks per variant**", "",
                "| variant | contact types | distinct values (ohm) | one value for all |", "|---|---|---|---|"]
        for v, b in p["magic_contact_blocks"].items():
            vals = ", ".join(f(x) for x in b["distinct_ohm"][:8]) + (" ..." if len(b["distinct_ohm"]) > 8 else "")
            out.append(f"| {v} | {b['contact_types']} | {vals} | {'yes' if b['blanket'] else 'no'} |")
        out += ["", f"**{p['pdk']}: isolated wire capacitance (aF/um, default width)**", "",
                "| corner | layer | calibration table | Magic rules | LEF (nom) | table / Magic - 1 |", "|---|---|---|---|---|---|"]
        for r in p["isolated_capacitance"]:
            rel = "-" if r["table_vs_magic"] is None else f"{100 * r['table_vs_magic']:+.1f}%"
            out.append(f"| {r['corner']} | {r['layer']} | {f(r['table_af_per_um'])} | {f(r['magic_af_per_um'])} | "
                       f"{f(r['lef_nominal_af_per_um'])} | {rel} |")
        out.append("")
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path)
    parser.add_argument("--markdown", action="store_true")
    args = parser.parse_args()
    data = dict(schema="rustroad.rc-data-sources.v1", corner_variant=CORNER_VARIANT,
                pdks=[collect(p) for p in ("sky130", "gf180")])
    if args.out:
        args.out.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n")
    if args.markdown or not args.out:
        print(markdown(data))
    return 0


if __name__ == "__main__":
    sys.exit(main())
