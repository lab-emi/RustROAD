#!/usr/bin/env python3
"""Build per-PDK, per-corner RC extraction profiles from PDK data files only.

Inputs are PDK *data* files, read as data and fingerprinted (path + sha256):

* the PDK-supplied OpenRCX calibration tables (``rules.openrcx.*``): per routing
  layer and vertical context, rows ``spacing, coupling, ground`` per unit length;
* the Magic technology file ``extract`` section: sheet resistance and per-cut
  contact resistance for the nominal / high-R / low-R variants; the
  area/perimeter/sidewall capacitance as an independent cross-check of the
  calibration tables; and the nominal face-to-substrate area capacitance and
  layer-over-layer overlap capacitance, used only for metal on another layer
  that overlaps a wire face without forming a calibration plate (the tables
  have no data for that case). Those two are moved to each corner with the
  calibration table's own corner ratio (the substrate row for the area
  capacitance, the plate excess of the same layer pair for the overlap), so
  every capacitance term follows the table's corners;
* the technology LEF, used for layer names/order/widths and, where the PDK states
  the values are valid (Sky130 only), as a resistance/capacitance cross-check.
  The GF180 technology LEF states its parasitic values are placeholders; they
  are never read into a profile.

No coefficient is fitted to any extractor output. The fourth column of every
calibration row is not used: it is inconsistent with the foundry sheet
resistance of both other sources (see docs/extraction.md), so resistance comes
from the Magic extract section. The physical model is documented in
docs/extraction.md.

Usage:
  scripts/build_rc_profile.py [--pdk-root DIR] [--out-dir fixtures/pdk] [--check]
  scripts/build_rc_profile.py --compare-ref REV   # every field present at git REV is unchanged
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PDK_ROOT = Path(
    "/home/cgao/.local/share/containers/storage/overlay/"
    "a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel")
PDK_VERSION = "f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7"
MAX_INPUT_BYTES = 8 * 1024 * 1024
CORNERS = ("min", "nom", "max")
# Magic extract variants: nominal, high R + high C, low R + low C.
MAGIC_VARIANT = {"nom": "()", "max": "(hrhc)", "min": "(lrlc)"}

PDKS = {
    "sky130": dict(
        library="sky130_fd_sc_hd",
        base=f"sky130/versions/{PDK_VERSION}/sky130A",
        technology_lef="libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef",
        rules="libs.tech/librelane/rules.openrcx.sky130A.{corner}.calibre",
        magic="libs.tech/magic/sky130A.tech",
        routing=["li1", "met1", "met2", "met3", "met4", "met5"],
        magic_types=["allli", "allm1", "allm2", "allm3", "allm4", "allm5"],
        magic_planes=["locali", "metal1", "metal2", "metal3", "metal4", "metal5"],
        cuts={"mcon": "mcon", "via": "m2c", "via2": "m3c", "via3": "via3", "via4": "via4"},
        lef_rc_valid=True,
    ),
    "gf180": dict(
        library="gf180mcu_fd_sc_mcu7t5v0",
        base=f"gf180mcu/versions/{PDK_VERSION}/gf180mcuD",
        technology_lef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef",
        rules="libs.tech/librelane/rules.openrcx.gf180mcuD.{corner}",
        magic="libs.tech/magic/gf180mcuD.tech",
        routing=["Metal1", "Metal2", "Metal3", "Metal4", "Metal5"],
        magic_types=["allm1", "allm2", "allm3", "allm4", "allm5"],
        magic_planes=["metal1", "metal2", "metal3", "metal4", "metal5"],
        cuts={"Via1": "m2c", "Via2": "m3c", "Via3": "via3", "Via4": "via4"},
        lef_rc_valid=False,
    ),
}


def fail(message: str) -> None:
    raise SystemExit(f"build_rc_profile: {message}")


def read_text(path: Path) -> str:
    if not path.is_file():
        fail(f"missing PDK data file {path}")
    if path.stat().st_size > MAX_INPUT_BYTES:
        fail(f"input over byte cap: {path}")
    return path.read_text(encoding="ascii", errors="strict")


def fingerprint(role: str, path: Path) -> dict:
    data = path.read_bytes()
    return dict(role=role, path=str(path), sha256=hashlib.sha256(data).hexdigest(), bytes=len(data))


def parse_lef(text: str) -> dict:
    """Ordered layers with TYPE, WIDTH and RC statements (cross-check data)."""
    layers, current = [], None
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        m = re.match(r"^LAYER\s+(\S+)\s*$", line)
        if m and current is None:
            current = dict(name=m[1])
            continue
        if current is None:
            continue
        if line == f"END {current['name']}":
            layers.append(current)
            current = None
            continue
        tokens = line.rstrip(";").split()
        if tokens[:1] == ["TYPE"]:
            current["type"] = tokens[1]
        elif tokens[:1] == ["WIDTH"] and len(tokens) == 2:
            current["width"] = float(tokens[1])
        elif tokens[:2] == ["RESISTANCE", "RPERSQ"]:
            current["rpersq"] = float(tokens[2])
        elif tokens[:1] == ["RESISTANCE"] and len(tokens) == 2:
            current["resistance"] = float(tokens[1])
        elif tokens[:2] == ["CAPACITANCE", "CPERSQDIST"]:
            current["cpersqdist"] = float(tokens[2])
        elif tokens[:1] == ["EDGECAPACITANCE"]:
            current["edgecap"] = float(tokens[1])
    if current is not None:
        fail("unterminated LEF LAYER block")
    return dict(layers=layers, placeholder_parasitics="not correct Parasitic Information" in text)


def parse_rules(text: str) -> dict:
    """Return {'layer_count': n, 'tables': {(metal, over, under): rows}, 'ignored': [...]}."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "Extraction Rules for OpenRCX":
        fail("unexpected calibration-table header")
    count = None
    tables, widths, ignored = {}, {}, set()
    i = 0
    while i < len(lines):
        line = lines[i].strip()
        m = re.match(r"^LayerCount\s+(\d+)$", line)
        if m:
            count = int(m[1])
        m = re.match(r"^Metal\s+(\d+)\s+(RESOVER|OVER|UNDER|DIAGUNDER|OVERUNDER)$", line)
        if m:
            nxt = lines[i + 1].strip()
            w = re.match(r"^WIDTH Table (\d+) entries:\s*(.*)$", nxt)
            if not w:
                fail(f"missing width table after {line!r}")
            values = w[2].split()
            if int(w[1]) > 1 or len(values) > 1:
                fail(f"multi-width calibration table unsupported: {line!r}")
            if values:
                key = int(m[1])
                width = float(values[0])
                if widths.setdefault(key, width) != width:
                    fail(f"inconsistent table width for metal {key}")
            i += 2
            continue
        m = re.match(r"^Metal\s+(\d+)\s+(RESOVER|OVER|UNDER|DIAGUNDER)\s+(\d+)(?:\s+UNDER\s+(\d+))?$", line)
        if m:
            metal, kind, a, b = int(m[1]), m[2], int(m[3]), m[4]
            head = lines[i + 1].strip()
            d = re.match(r"^DIST count (\d+) width (\S+)$", head)
            if not d:
                fail(f"missing DIST header after {line!r}")
            n = int(d[1])
            rows = []
            for k in range(n):
                fields = lines[i + 2 + k].split()
                if len(fields) != 4:
                    fail(f"calibration row must have 4 fields: {line!r}")
                rows.append([float(x) for x in fields])
            if lines[i + 2 + n].strip() != "END DIST":
                fail(f"unterminated DIST table {line!r}")
            i += 3 + n
            if n == 0:
                continue
            if kind in ("RESOVER", "DIAGUNDER"):
                ignored.add(kind)
                continue
            if kind == "OVER":
                over, under = a, (int(b) if b is not None else None)
            else:  # UNDER j: over substrate, under plane j
                if b is not None:
                    fail("UNDER with second UNDER index")
                over, under = 0, a
            key = (metal, over, under)
            if key in tables:
                fail(f"duplicate calibration context {key}")
            prev = -1.0
            for r in rows:
                if not all(x == x and abs(x) != float("inf") for x in r) or min(r[1:3]) < 0:
                    fail(f"non-finite/negative calibration value in {line!r}")
                if r[0] <= prev:
                    fail(f"non-increasing spacing in {line!r}")
                prev = r[0]
            tables[key] = [r[:3] for r in rows]
            continue
        i += 1
    if count is None:
        fail("LayerCount missing")
    return dict(layer_count=count, widths=widths, tables=tables, ignored=sorted(ignored))


def parse_magic(text: str, variant: str, types: list[str], cut_types: list[str]) -> dict:
    """Read the extract section for one variant: resist/contact/area/perimeter/sidewall."""
    lines = text.splitlines()
    try:
        start = next(i for i, l in enumerate(lines) if l.strip() == "extract")
    except StopIteration:
        fail("magic extract section missing")
    active = {"*"}
    out = dict(resist={}, contact={}, areacap={}, perimeter={}, sidewall={}, overlap={}, all_contacts={})
    for raw in lines[start + 1:]:
        line = raw.split("#", 1)[0].strip()
        if line == "end":
            break
        if not line or line.startswith("style"):
            continue
        tokens = line.split()
        if tokens[0] == "variants":
            active = set(tokens[1].split(",")) if tokens[1] != "*" else {"*"}
            continue
        if variant not in active and "*" not in active:
            continue
        key = tokens[0]
        if key == "resist" and len(tokens) >= 3:
            m = re.match(r"^\(([^)]*)\)/(\S+)$", tokens[1])
            if m and m[1] in types:
                out["resist"][m[1]] = float(tokens[2])
        elif key == "contact" and len(tokens) == 3:
            # Every contact line of the variant (cuts, diffusion, poly, MIM):
            # a block that gives all of them one value is not characterized.
            out["all_contacts"][tokens[1]] = float(tokens[2])
            if tokens[1] in cut_types:
                out["contact"][tokens[1]] = float(tokens[2])
        elif key == "defaultareacap" and len(tokens) == 4 and tokens[1] in types:
            out["areacap"][tokens[1]] = float(tokens[3])
        elif key == "defaultperimeter" and len(tokens) == 4 and tokens[1] in types:
            out["perimeter"][tokens[1]] = float(tokens[3])
        elif key == "defaultoverlap" and len(tokens) == 6 and tokens[1] in types and tokens[3] in types:
            out["overlap"][(tokens[1], tokens[3])] = float(tokens[5])
        elif key == "defaultsidewall" and len(tokens) in (4, 5) and tokens[1] in types:
            out["sidewall"][tokens[1]] = [float(tokens[3]), float(tokens[4]) if len(tokens) == 5 else 0.0]
    return out


def check_magic_contacts(text: str, spec: dict) -> None:
    """The cut-to-contact-type map must agree with the Magic contact/types sections."""
    aliases = {}
    for line in text.splitlines():
        tokens = line.split()
        if len(tokens) == 2 and "," in tokens[1]:
            names = tokens[1].split(",")
            for alias in names:
                aliases.setdefault(alias, names[0])
    residues = {}
    in_contact = False
    for line in text.splitlines():
        s = line.strip()
        if s == "contact":
            in_contact = True
            continue
        if in_contact and s == "end":
            break
        tokens = s.split()
        if in_contact and len(tokens) == 3:
            residues[tokens[0]] = tokens[1:]
    planes = spec["magic_planes"]
    for index, (cut, ctype) in enumerate(spec["cuts"].items()):
        canonical = aliases.get(ctype, ctype)
        want = [planes[index], planes[index + 1]]
        if want not in (residues.get(ctype), residues.get(canonical)):
            fail(f"magic contact {ctype} ({canonical}) does not join {want} for cut {cut}")


def rel(a: float, b: float) -> float | None:
    return None if b == 0 else (a - b) / b


def build(tag: str, corner: str, pdk_root: Path) -> dict:
    spec = PDKS[tag]
    base = pdk_root / spec["base"]
    lef_path = base / spec["technology_lef"]
    rules_path = base / spec["rules"].format(corner=corner)
    magic_path = base / spec["magic"]
    lef = parse_lef(read_text(lef_path))
    rules = parse_rules(read_text(rules_path))
    nominal_rules_path = base / spec["rules"].format(corner="nom")
    rules_nom = rules if corner == "nom" else parse_rules(read_text(nominal_rules_path))
    magic_text = read_text(magic_path)
    check_magic_contacts(magic_text, spec)
    variant = MAGIC_VARIANT[corner]
    magic = parse_magic(magic_text, variant, spec["magic_types"], list(spec["cuts"].values()))
    nominal = parse_magic(magic_text, "()", spec["magic_types"], list(spec["cuts"].values()))
    if lef["placeholder_parasitics"] == spec["lef_rc_valid"]:
        fail(f"{tag}: LEF parasitic validity marker disagrees with the PDK profile table")
    routing = [l for l in lef["layers"] if l.get("type") == "ROUTING"]
    if [l["name"] for l in routing] != spec["routing"]:
        fail(f"{tag}: routing layer order {[l['name'] for l in routing]} differs from profile table")
    if rules["layer_count"] != len(routing):
        fail(f"{tag}: calibration LayerCount {rules['layer_count']} != {len(routing)} routing layers")
    by_name = {l["name"]: l for l in lef["layers"]}
    diagnostics = [
        "calibration column 4 is not used; resistance is taken from the Magic extract section",
        "DIAGUNDER (diagonal adjacent-layer) tables are not modeled",
        "via cut and via landing-pad self capacitance are not modeled",
    ]
    layers, checks = [], []
    for index, lef_layer in enumerate(routing, start=1):
        name = lef_layer["name"]
        width = rules["widths"].get(index)
        if width is None or abs(width - lef_layer["width"]) > 1e-9:
            fail(f"{tag}: calibration width for {name} ({width}) != LEF WIDTH {lef_layer['width']}")
        mtype = spec["magic_types"][index - 1]
        sheet = magic["resist"].get(mtype)
        if sheet is None:
            fail(f"{tag}: magic sheet resistance missing for {mtype} {variant}")
        contexts = []
        for (metal, over, under), rows in sorted(rules["tables"].items(), key=lambda kv: (kv[0][0], kv[0][1], -1 if kv[0][2] is None else kv[0][2])):
            if metal != index:
                continue
            contexts.append(dict(over=over, under=under, rows=rows))
        if not any(c["over"] == 0 and c["under"] is None for c in contexts):
            fail(f"{tag}: no OVER 0 calibration context for {name}")
        area_cap, overlap = overlap_area(tag, index, spec, rules, rules_nom, nominal)
        layers.append(dict(name=name, rules_metal=index, width_um=width,
                           sheet_resistance_ohm_per_square=sheet / 1000.0, contexts=contexts,
                           area_cap_pf_per_um2=area_cap, overlap_area=overlap))
        over0 = next(c for c in contexts if c["over"] == 0 and c["under"] is None)["rows"]
        isolated = 2.0 * over0[-1][2]
        s0, cc0 = over0[0][0], over0[0][1]
        check = dict(layer=name, rules_isolated_ground_pf_per_um=isolated,
                     rules_min_spacing_um=s0, rules_coupling_at_min_spacing_pf_per_um=cc0,
                     sheet_resistance_ohm_per_square=sheet / 1000.0)
        area, perim, side = magic["areacap"].get(mtype), magic["perimeter"].get(mtype), magic["sidewall"].get(mtype)
        if area is not None and perim is not None:
            m_iso = (area * width + 2.0 * perim) * 1e-6
            check.update(magic_isolated_ground_pf_per_um=m_iso,
                         isolated_ground_relative_difference=rel(isolated, m_iso))
        if side is not None:
            m_cc = side[0] / (s0 + side[1]) * 1e-6
            check.update(magic_coupling_at_min_spacing_pf_per_um=m_cc,
                         coupling_relative_difference=rel(cc0, m_cc))
        if spec["lef_rc_valid"]:
            if "rpersq" in lef_layer:
                check.update(lef_rpersq_ohm=lef_layer["rpersq"],
                             lef_vs_magic_nominal_resistance_relative_difference=rel(
                                 lef_layer["rpersq"], nominal["resist"][mtype] / 1000.0))
            if "cpersqdist" in lef_layer and "edgecap" in lef_layer:
                lef_iso = lef_layer["cpersqdist"] * width + 2.0 * lef_layer["edgecap"]
                check.update(lef_isolated_ground_pf_per_um_nominal=lef_iso)
        checks.append(check)
    cuts = []
    for index, (cut, ctype) in enumerate(spec["cuts"].items()):
        if cut not in by_name or by_name[cut].get("type") != "CUT":
            fail(f"{tag}: cut layer {cut} missing from LEF")
        order = [l["name"] for l in lef["layers"]]
        lower, upper = spec["routing"][index], spec["routing"][index + 1]
        if not order.index(lower) < order.index(cut) < order.index(upper):
            fail(f"{tag}: LEF order does not place {cut} between {lower} and {upper}")
        value = magic["contact"].get(ctype)
        source = f"magic contact {ctype} {variant}"
        if value is None:
            fail(f"{tag}: magic contact resistance missing for {ctype} {variant}")
        blanket = corner_contacts_blanket(magic, nominal)
        if value <= 0.0:
            fallback = nominal["contact"][ctype]
            diagnostics.append(
                f"VIA_R_CORNER_UNAVAILABLE: magic {variant} lists {ctype} contact resistance {value:g} mOhm "
                f"(all contacts are 0 in that block, i.e. no corner data); using nominal {fallback:g} mOhm")
            value, source = fallback, f"magic contact {ctype} () fallback"
        elif blanket is not None:
            fallback = nominal["contact"][ctype]
            diagnostics.append(
                f"VIA_R_CORNER_UNAVAILABLE: magic {variant} gives every one of its {blanket} contact types "
                f"(diffusion, poly, via and MIM contacts alike) the same {value:g} mOhm while the nominal "
                f"block distinguishes them, i.e. a placeholder, not corner data; using nominal "
                f"{fallback:g} mOhm for {ctype}")
            value, source = fallback, f"magic contact {ctype} () fallback (blanket {variant} block)"
        entry = dict(name=cut, lower=lower, upper=upper, resistance_ohm_per_cut=value / 1000.0, source=source)
        if spec["lef_rc_valid"] and "resistance" in by_name[cut]:
            entry["lef_resistance_ohm_nominal"] = by_name[cut]["resistance"]
        cuts.append(entry)
    sources = [fingerprint("capacitance_calibration_table", rules_path),
               fingerprint("resistance_and_crosscheck_tech", magic_path),
               fingerprint("technology_lef", lef_path)]
    if corner != "nom":
        sources.append(fingerprint("nominal_calibration_table_for_overlap_corner_ratio", nominal_rules_path))
    return dict(
        schema="rustroad.rc-profile.v1",
        pdk=tag,
        library=spec["library"],
        corner=corner,
        pdk_version=PDK_VERSION,
        technology_lef_sha256=sources[2]["sha256"],
        units=dict(length="um", capacitance="pF", resistance="ohm"),
        magic_variant=variant,
        lef_parasitics="cross_check_only" if spec["lef_rc_valid"] else "placeholder_excluded",
        sources=sources,
        model=dict(
            rows="[edge_spacing_um, coupling_pf_per_um_to_that_neighbour, ground_pf_per_um_for_that_side]",
            per_side="each wire side is looked up separately; an isolated wire has 2 * ground(last row)",
            beyond_last_row="coupling 0, ground = last-row ground (isolated side)",
            context="over = calibration metal index of the nearest conductor below (0 = substrate), under = nearest above",
            resistance="sheet_resistance * centerline_length / width; via = resistance_ohm_per_cut / cuts",
            overlap_area="metal of another layer that covers part of a wire face without forming a "
                         "calibration plate couples over the covered width at overlap_area.pf_per_um2 "
                         "(counted once, from the upper face), and the covered face loses "
                         "area_cap_pf_per_um2 to the substrate; values are the Magic nominal "
                         "defaultoverlap/defaultareacap times the calibration table's corner ratio "
                         "(table_corner_ratio: plate excess of the same pair; substrate row for area_cap)",
        ),
        layers=layers,
        cuts=cuts,
        cross_checks=checks,
        diagnostics=diagnostics,
        generator="scripts/build_rc_profile.py",
    )


def corner_contacts_blanket(magic: dict, nominal: dict) -> int | None:
    """Number of contact types when a corner variant gives every contact type
    one and the same value although the nominal block distinguishes them (a
    placeholder block, not corner data); None otherwise. A block of zeros is
    handled separately (no data at all)."""
    values = list(magic["all_contacts"].values())
    nominal_values = set(nominal["all_contacts"].values())
    if len(values) >= 3 and len(set(values)) == 1 and values[0] > 0.0 and len(nominal_values) > 1:
        return len(values)
    return None


def isolated_side(rules: dict, metal: int, over: int) -> float | None:
    rows = rules["tables"].get((metal, over, None))
    return None if rows is None else rows[-1][2]


def overlap_area(tag: str, index: int, spec: dict, rules: dict, rules_nom: dict, nominal: dict):
    """Face area capacitance to the substrate and over each lower routing layer
    (pF/um^2) for one routing layer: Magic nominal values moved to the corner
    by the calibration table's own ratio."""
    types = spec["magic_types"]
    mtype = types[index - 1]
    area = nominal["areacap"].get(mtype)
    sub, sub_nom = isolated_side(rules, index, 0), isolated_side(rules_nom, index, 0)
    if area is None or not sub or not sub_nom:
        fail(f"{tag}: magic nominal area capacitance or substrate row missing for {mtype}")
    area_cap = area * (sub / sub_nom) * 1e-6
    out = []
    for lower in range(1, index):
        ov = nominal["overlap"].get((mtype, types[lower - 1]))
        if ov is None:
            fail(f"{tag}: magic nominal overlap {mtype} over {types[lower - 1]} missing")
        plate, plate_nom = isolated_side(rules, index, lower), isolated_side(rules_nom, index, lower)
        if plate is None or plate_nom is None:
            fail(f"{tag}: calibration plate context metal {index} over {lower} missing")
        if plate - sub > 0.0 and plate_nom - sub_nom > 0.0:
            ratio, source = (plate - sub) / (plate_nom - sub_nom), "plate excess of the pair"
        else:
            # A distant plate the table rates at or below the substrate row
            # has no usable excess; the substrate row's ratio moves it.
            ratio, source = sub / sub_nom, "substrate row (pair plate excess not positive)"
        out.append(dict(over=lower, pf_per_um2=ov * ratio * 1e-6, magic_nominal_af_per_um2=ov,
                        table_corner_ratio=ratio, table_corner_ratio_source=source))
    return area_cap, out


def render(profile: dict) -> str:
    return json.dumps(profile, indent=1) + "\n"


# Field changes made on purpose since the reference revision, with the PDK
# evidence for each (a regex over the reported field path). Everything else
# must be unchanged; diagnostics may only gain entries.
INTENTIONAL_CHANGES = {
    r"^gf180-max\.cuts\[\d\]\.(resistance_ohm_per_cut|source)$":
        "GF180 Magic (hrhc) gives all 10 contact types (diffusion, poly, via, MIM) one blanket 15000 mOhm "
        "while the nominal block distinguishes them (and (lrlc) gives all of them 0): a placeholder, not "
        "corner via data, so the max profile keeps the nominal 4.5 ohm per cut like the min profile",
}


def compare_ref(rev: str, out_dir: Path) -> int:
    """Fail unless every field of the profiles at git ``rev`` is unchanged in
    the tracked profiles (fields may only have been added), apart from the
    listed intentional changes; diagnostics may only gain entries."""
    import subprocess

    def unchanged(old, new, where, bad):
        if isinstance(old, dict) and isinstance(new, dict):
            for k, v in old.items():
                if k not in new:
                    bad.append(f"{where}.{k} removed")
                else:
                    unchanged(v, new[k], f"{where}.{k}", bad)
        elif isinstance(old, list) and isinstance(new, list) and where.endswith(".sources"):
            if new[:len(old)] != old:
                bad.append(f"{where} changed")
        elif isinstance(old, list) and isinstance(new, list) and where.endswith(".diagnostics"):
            if any(d not in new for d in old):
                bad.append(f"{where} lost entries")
        elif isinstance(old, list) and isinstance(new, list) and len(old) == len(new):
            for i, (a, b) in enumerate(zip(old, new)):
                unchanged(a, b, f"{where}[{i}]", bad)
        elif old != new:
            bad.append(f"{where} changed")

    status = 0
    for tag in PDKS:
        for corner in CORNERS:
            rel_path = f"fixtures/pdk/{tag}-rc-{corner}.json"
            old = json.loads(subprocess.run(["git", "-C", str(ROOT), "show", f"{rev}:{rel_path}"],
                                            capture_output=True, check=True, text=True).stdout)
            new = json.loads((out_dir / f"{tag}-rc-{corner}.json").read_text())
            found: list[str] = []
            unchanged(old, new, tag + "-" + corner, found)
            intended = [b for b in found if any(re.match(k, b.rsplit(" ", 1)[0]) for k in INTENTIONAL_CHANGES)]
            bad = [b for b in found if b not in intended]
            added = sorted({k for layer in new["layers"] for k in layer} - {k for layer in old["layers"] for k in layer})
            print(f"{tag} {corner}: {'unchanged' if not bad else 'CHANGED ' + ', '.join(bad)}; added layer fields: "
                  f"{', '.join(added) or 'none'}"
                  + (f"; intentional: {', '.join(intended)}" if intended else ""))
            status |= bool(bad)
    return status


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pdk-root", type=Path, default=DEFAULT_PDK_ROOT)
    parser.add_argument("--out-dir", type=Path, default=ROOT / "fixtures/pdk")
    parser.add_argument("--check", action="store_true", help="fail if tracked profiles differ from a rebuild")
    parser.add_argument("--compare-ref", metavar="REV", help="fail unless all fields at git REV are unchanged")
    args = parser.parse_args()
    if args.compare_ref:
        return compare_ref(args.compare_ref, args.out_dir)
    stale = []
    for tag in PDKS:
        for corner in CORNERS:
            text = render(build(tag, corner, args.pdk_root))
            path = args.out_dir / f"{tag}-rc-{corner}.json"
            if args.check:
                if not path.is_file() or path.read_text() != text:
                    stale.append(str(path))
            else:
                path.write_text(text)
                print(f"wrote {path}")
    if stale:
        print("stale profiles: " + ", ".join(stale), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
