# SPDX-License-Identifier: Apache-2.0
"""Result-file parsers for the signoff harness (pure Python, unit-tested).

Every parser returns facts; verdicts are derived from facts, never from tool
exit codes. A parser that cannot find the evidence it needs reports that as
missing (-> ``unverified``), never as zero violations.
"""
from __future__ import annotations

from collections import Counter
import json
import math
import re
import xml.etree.ElementTree as ET

MAX_ITEMS = 5_000_000


class ParseError(ValueError):
    """The evidence is absent or malformed."""


def _clean_category(text: str) -> str:
    text = (text or "").strip()
    if len(text) >= 2 and text[0] == text[-1] == "'":
        text = text[1:-1]
    return text


def parse_lyrdb(path) -> dict:
    """Count KLayout report-database items per rule and per cell (ALL cells)."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise ParseError(f"lyrdb unreadable: {error}") from error
    if root.tag != "report-database":
        raise ParseError(f"not a report database: <{root.tag}>")
    top = (root.findtext("top-cell") or "").strip()
    generator = (root.findtext("generator") or "").strip()
    categories = []
    def walk(node, prefix=""):
        for cat in node.findall("category"):
            name = (cat.findtext("name") or "").strip()
            full = prefix + name
            categories.append(full)
            sub = cat.find("categories")
            if sub is not None:
                walk(sub, full + ".")
    cats = root.find("categories")
    if cats is not None:
        walk(cats)
    by_rule, by_cell, weighted = Counter(), Counter(), Counter()
    total = 0
    items = root.find("items")
    samples = []
    for item in (items.findall("item") if items is not None else []):
        total += 1
        if total > MAX_ITEMS:
            raise ParseError("lyrdb item budget exceeded")
        rule = _clean_category(item.findtext("category"))
        cell = _clean_category(item.findtext("cell"))
        mult = item.findtext("multiplicity")
        mult = int(mult) if mult and mult.strip().isdigit() else 1
        by_rule[rule] += 1
        by_cell[cell] += 1
        weighted[rule] += mult
        if len(samples) < 50:
            values = [v.text for v in item.findall("values/value") if v.text]
            samples.append(dict(rule=rule, cell=cell, multiplicity=mult, value=values[:1]))
    return dict(top_cell=top, generator=generator, categories=len(categories), total=total,
                by_rule=dict(sorted(by_rule.items())), by_cell=dict(sorted(by_cell.items())),
                multiplicity_by_rule=dict(sorted(weighted.items())), samples=samples)


def parse_magic_drc(text: str) -> dict:
    """Parse the RR_* markers printed by scripts/signoff/magic_drc.tcl."""
    out = dict(top_missing=None, style=None, hier_total=None, flat_total=None, done=False,
               hier_why={}, flat_why={}, standalone_cell_why={}, bbox=None, cells=None)
    m = re.search(r"^RR_TOP_MISSING (.*)$", text, re.M)
    if m:
        out["top_missing"] = m[1].strip()
    m = re.search(r"^RR_STYLE (.*)$", text, re.M)
    out["style"] = m[1].strip() if m else None
    m = re.search(r"^RR_BBOX (.*)$", text, re.M)
    out["bbox"] = [int(v) for v in m[1].split()] if m else None
    m = re.search(r"^RR_CELLS (\d+)$", text, re.M)
    out["cells"] = int(m[1]) if m else None
    for key, marker in (("hier_total", "RR_HIER_TOTAL"), ("flat_total", "RR_FLAT_TOTAL")):
        m = re.search(rf"^{marker} (\d+)\s*$", text, re.M)
        out[key] = int(m[1]) if m else None
    def why(marker):
        rules = {}
        for m in re.finditer(rf"^{marker} (\d+) :: (.*)$", text, re.M):
            msg = m[2].strip()
            rm = re.search(r"\(([^()]+)\)\s*$", msg)
            rule = rm[1] if rm else msg
            rules[rule] = rules.get(rule, 0) + int(m[1])
        return rules
    out["hier_why"] = why("RR_HIER_WHY")
    out["flat_why"] = why("RR_FLAT_WHY")
    standalone = {}
    for m in re.finditer(r"^RR_CELL_WHY (\S+) (\d+) :: (.*)$", text, re.M):
        rm = re.search(r"\(([^()]+)\)\s*$", m[3].strip())
        rule = rm[1] if rm else m[3].strip()
        standalone.setdefault(m[1], {})
        standalone[m[1]][rule] = standalone[m[1]].get(rule, 0) + int(m[2])
    out["standalone_cell_why"] = standalone
    out["done"] = bool(re.search(r"^RR_DONE\s*$", text, re.M))
    return out


def parse_magic_antenna(text: str) -> dict:
    ran = "Running antenna checks." in text and "antennacheck finished." in text
    violations = []
    for m in re.finditer(r"Antenna violation detected at plane (\S+)\s*\n\s*Effective antenna ratio ([0-9.eE+-]+) > limit ([0-9.eE+-]+)", text):
        violations.append(dict(plane=m[1], ratio=float(m[2]), limit=float(m[3])))
    loose = len(re.findall(r"Antenna violation detected", text))
    return dict(ran=ran, violations=len(violations), unparsed_violation_lines=loose - len(violations),
                by_plane=dict(Counter(v["plane"] for v in violations)), samples=violations[:20],
                top_missing=bool(re.search(r"^RR_TOP_MISSING", text, re.M)),
                done=bool(re.search(r"^RR_ANT_DONE\s*$", text, re.M)))


def parse_netgen(comp_json_text: str, comp_out_text: str, top: str) -> dict:
    """Netgen pass = every circuit entry clean AND the final result text says unique match."""
    try:
        data = json.loads(comp_json_text)
    except json.JSONDecodeError as error:
        raise ParseError(f"netgen JSON unreadable: {error}") from error
    if not isinstance(data, list) or not data:
        raise ParseError("netgen JSON has no circuit entries")
    problems, circuits, top_seen = [], [], False
    for entry in data:
        name = entry.get("name")
        label = name[0] if isinstance(name, list) and name else "<pins-only>"
        if isinstance(name, list) and len(name) == 2 and name[0] == top and name[1] == top:
            top_seen = True
        devices = entry.get("devices")
        if devices is not None:
            a, b = (Counter({d[0]: d[1] for d in side}) for side in devices)
            if a != b:
                problems.append(f"{label}: device counts differ {dict(a)} vs {dict(b)}")
        nets = entry.get("nets")
        if nets is not None and len(set(nets)) != 1:
            problems.append(f"{label}: net counts differ {nets}")
        for key in ("badnets", "badelements"):
            if entry.get(key):
                problems.append(f"{label}: {len(entry[key])} {key}")
        pins = entry.get("pins")
        if pins is not None and (len(pins) != 2 or sorted(map(str, pins[0])) != sorted(map(str, pins[1]))):
            problems.append(f"{label}: pin lists differ")
        circuits.append(dict(name=label, devices=devices, nets=nets,
                             badnets=len(entry.get("badnets") or []),
                             badelements=len(entry.get("badelements") or [])))
    finals = re.findall(r"^Final result:\s*(.*)$", comp_out_text, re.M)
    final = finals[-1].strip() if finals else None
    if not top_seen:
        problems.append(f"top circuit {top} not compared")
    if final != "Circuits match uniquely.":
        problems.append(f"final result {final!r}")
    return dict(final_result=final, circuits=len(circuits), problems=problems,
                matched=not problems, details=circuits[-20:])


def parse_density_log(text: str) -> dict:
    values = {}
    bad = []
    for m in re.finditer(r"^(\S+)_ca_density is (\S+)\s*$", text, re.M):
        raw = m[2]
        try:
            value = float(raw)
        except ValueError:
            value = float("nan")
        values[m[1]] = raw
        if not math.isfinite(value) or not 0.0 <= value <= 1.0:
            bad.append(f"{m[1]}={raw}")
    return dict(densities=values, nonfinite=bad, finite=bool(values) and not bad)


def parse_strmxor(text: str) -> dict:
    identical = "No differences found" in text
    layers = {}
    # "  Layer   Output   Differences (shape count)" table; Output may be "-".
    for m in re.finditer(r"^\s*(\d+/\d+)\s+(?:\S+\s+)?(\d+)\s*$", text, re.M):
        layers[m[1]] = layers.get(m[1], 0) + int(m[2])
    return dict(identical=identical, layer_differences=layers,
                difference_count=sum(layers.values()) if layers else (0 if identical else None))


def lvsdb_verdict(xref: dict, top: str) -> dict:
    problems = []
    circuits = xref.get("circuits") or []
    if not circuits:
        problems.append("no circuit pairs in lvsdb")
    top_pair = [c for c in circuits if c.get("layout") == top or c.get("schematic") == top]
    if not top_pair:
        problems.append(f"top circuit {top} absent from cross-reference")
    if top_pair and top_pair[0].get("layout_pins") is not None and \
            top_pair[0].get("layout_pins") != top_pair[0].get("schematic_pins"):
        problems.append(f"top pin count layout {top_pair[0].get('layout_pins')} != schematic {top_pair[0].get('schematic_pins')}")
    for c in circuits:
        if c.get("status") != "Match":
            problems.append(f"circuit {c.get('layout')}|{c.get('schematic')} status {c.get('status')}")
    return dict(matched=not problems, problems=problems, circuits=len(circuits),
                top=top_pair[0] if top_pair else None, details=(xref.get("details") or [])[:40])


def count_def_odd_coordinates(text: str) -> dict:
    """Count integer coordinates inside DEF point lists '( x y [ext] )' that are odd."""
    checked = odd = 0
    examples = []
    for m in re.finditer(r"\(\s*(-?\d+|\*)\s+(-?\d+|\*)(?:\s+(-?\d+))?\s*\)", text):
        for value in m.groups():
            if value is None or value == "*":
                continue
            checked += 1
            if int(value) % 2:
                odd += 1
                if len(examples) < 10:
                    examples.append(m[0])
    widths = re.finditer(r"\b(?:ROUTED|NEW|FIXED|COVER)\s+[A-Za-z]\S*\s+(\d+)\s+(?:\+\s*SHAPE\s+\S+\s+)?\(", text)
    rows = re.finditer(r"^\s*ROW\s+\S+\s+\S+\s+(-?\d+)\s+(-?\d+)\s+\S+(?:\s+DO\s+\d+\s+BY\s+\d+\s+STEP\s+(\d+)\s+(\d+))?", text, re.M)
    for m in list(widths) + list(rows):
        for value in m.groups():
            if value is None:
                continue
            checked += 1
            if int(value) % 2:
                odd += 1
                if len(examples) < 10:
                    examples.append(m[0][:60])
    return dict(checked=checked, odd=odd, examples=examples)
