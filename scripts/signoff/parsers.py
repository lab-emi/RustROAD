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


def lyrdb_items(path, rule: str | None = None) -> list[dict]:
    """Every item of a KLayout report database (optionally one rule), bounded by MAX_ITEMS."""
    try:
        root = ET.parse(path).getroot()
    except (OSError, ET.ParseError) as error:
        raise ParseError(f"lyrdb unreadable: {error}") from error
    if root.tag != "report-database":
        raise ParseError(f"not a report database: <{root.tag}>")
    out = []
    items = root.find("items")
    for index, item in enumerate(items.findall("item") if items is not None else []):
        if index >= MAX_ITEMS:
            raise ParseError("lyrdb item budget exceeded")
        name = _clean_category(item.findtext("category"))
        if rule is not None and name != rule:
            continue
        out.append(dict(rule=name, cell=_clean_category(item.findtext("cell")),
                        values=[v.text.strip() for v in item.findall("values/value") if v.text]))
    return out


def parse_edge_pair(value: str):
    """'edge-pair: (x1,y1;x2,y2)|(x3,y3;x4,y4)' (also '/' separator) -> two edges in um, else None."""
    m = re.fullmatch(r"\s*edge-pair:\s*\(([^)]*)\)\s*[|/]\s*\(([^)]*)\)\s*", value or "")
    if not m:
        return None
    edges = []
    for part in (m[1], m[2]):
        nums = re.split(r"[;,]", part)
        if len(nums) != 4:
            return None
        try:
            edges.append(tuple(float(x) for x in nums))
        except ValueError:
            return None
    return tuple(edges)


def planted_gap_hits(items: list[dict], bump_um, target_um, gap_um: float, tol_um: float = 0.0015) -> list[str]:
    """Edge-pair items that measure exactly the planted gap between the bump and the target.

    One edge must lie on the bump's edge facing the target and the other on the
    target's facing edge (both parallel, each overlapping its own rectangle's
    span), and their separation must equal ``gap_um`` within ``tol_um``. Nearby
    unrelated items (pre-existing notches, other neighbours) do not qualify.
    """
    bx1, by1, bx2, by2 = bump_um
    tx1, ty1, tx2, ty2 = target_um
    if tx1 >= bx2:
        axis, b_line, t_line = "x", bx2, tx1
    elif bx1 >= tx2:
        axis, b_line, t_line = "x", bx1, tx2
    elif ty1 >= by2:
        axis, b_line, t_line = "y", by2, ty1
    elif by1 >= ty2:
        axis, b_line, t_line = "y", by1, ty2
    else:
        raise ParseError("bump and target overlap; no gap to measure")
    if abs(abs(t_line - b_line) - gap_um) > tol_um:
        raise ParseError(f"geometry gap {abs(t_line - b_line):.4f} um != planted {gap_um} um")
    b_span = (by1, by2) if axis == "x" else (bx1, bx2)
    t_span = (ty1, ty2) if axis == "x" else (tx1, tx2)

    def on_line(edge, line, span):
        x1, y1, x2, y2 = edge
        const = (x1, x2) if axis == "x" else (y1, y2)
        along = (y1, y2) if axis == "x" else (x1, x2)
        if abs(const[0] - line) > tol_um or abs(const[1] - line) > tol_um:
            return False
        lo, hi = min(along), max(along)
        return hi > span[0] + tol_um and lo < span[1] - tol_um

    hits = []
    for item in items:
        for value in item.get("values", []):
            pair = parse_edge_pair(value)
            if pair is None:
                continue
            e1, e2 = pair
            if (on_line(e1, b_line, b_span) and on_line(e2, t_line, t_span)) or \
                    (on_line(e2, b_line, b_span) and on_line(e1, t_line, t_span)):
                hits.append(value)
    return hits


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
    """Netgen pass = every circuit entry clean AND the final result text says unique match.

    "Clean" includes device properties: Netgen reports a topology match with
    different device parameters (e.g. a drive-strength swap changes transistor
    widths) as ``Final result: Circuits match uniquely.`` followed by
    ``Property errors were found.``, and lists the mismatches under the
    circuit's ``properties`` key in comp.json. Either is a failure here.
    """
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
        props = entry.get("properties")
        if props:
            samples = []
            for pair in props[:3]:
                try:
                    samples.append(f"{pair[0][0]} {pair[0][1]} vs {pair[1][0]} {pair[1][1]}")
                except (IndexError, TypeError, KeyError):
                    samples.append(str(pair)[:160])
            problems.append(f"{label}: {len(props)} device property mismatches ({'; '.join(samples)})")
        pins = entry.get("pins")
        if pins is not None and (len(pins) != 2 or sorted(map(str, pins[0])) != sorted(map(str, pins[1]))):
            problems.append(f"{label}: pin lists differ")
        circuits.append(dict(name=label, devices=devices, nets=nets,
                             badnets=len(entry.get("badnets") or []),
                             badelements=len(entry.get("badelements") or []),
                             property_errors=len(props or [])))
    finals = re.findall(r"^Final result:\s*(.*)$", comp_out_text, re.M)
    final = finals[-1].strip() if finals else None
    if not top_seen:
        problems.append(f"top circuit {top} not compared")
    if final != "Circuits match uniquely.":
        problems.append(f"final result {final!r}")
    property_lines = re.findall(r"^.*\bproperty errors\b.*$", comp_out_text, re.M | re.I)
    if property_lines:
        problems.append(f"comp.out reports property errors ({len(property_lines)} lines, e.g. "
                        f"{property_lines[-1].strip()[:120]!r})")
    # Port errors: a pin on one side only (e.g. a labelled floating layout pin, or an
    # unconnected reference port) is reported as "Netlists match uniquely with port
    # errors." / "(no matching pin)" / "disconnected node", yet the final line still
    # says "Circuits match uniquely." and comp.json drops the pin from both lists.
    port_lines = re.findall(r"^.*\bport errors?\b.*$", comp_out_text, re.M | re.I)
    unmatched_pins = [_netgen_unmatched_pin(x) for x in
                      re.findall(r"^.*\(no matching pin\).*$", comp_out_text, re.M)]
    disconnected = re.findall(r"^Cell\s+(\S+)\s+\((\d+)\)\s+disconnected node:\s*(.*?)\s*$", comp_out_text, re.M)
    if port_lines:
        problems.append(f"comp.out reports port errors ({port_lines[-1].strip()[:120]!r})")
    if unmatched_pins:
        problems.append(f"{len(unmatched_pins)} pins without a matching pin: {unmatched_pins[:6]}")
    if disconnected:
        problems.append(f"{len(disconnected)} disconnected nodes: "
                        + ", ".join(f"{c}({side}):{n}" for c, side, n in disconnected[:6]))
    return dict(final_result=final, circuits=len(circuits), problems=problems,
                property_error_lines=[x.strip()[:200] for x in property_lines[:10]],
                property_errors=sum(c["property_errors"] for c in circuits),
                port_error_lines=[x.strip()[:200] for x in port_lines[:10]],
                unmatched_pins=unmatched_pins[:50],
                disconnected_nodes=[dict(cell=c, side=int(side), node=n) for c, side, n in disconnected[:50]],
                matched=not problems, details=circuits[-20:])


def _netgen_unmatched_pin(line: str) -> str:
    left, _, right = line.partition("|")
    left, right = left.strip(), right.strip()
    return f"layout-only {left}" if "(no matching pin)" in right else f"reference-only {right}"


def parse_magic_extract(text: str) -> dict:
    """Facts from a Magic extraction log: port pairs Magic reports as shorted together."""
    shorted = re.findall(r'Ports\s+"([^"]*)"\s+and\s+"([^"]*)"\s+are electrically shorted', text)
    return dict(shorted_ports=[list(x) for x in shorted[:50]], shorted_count=len(shorted))


def spice_subckt_ports(text: str, name: str) -> list[str] | None:
    """Port list of ``.subckt name`` in a SPICE netlist (continuation lines joined), or None."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = re.match(r"\s*\.subckt\s+(\S+)\s*(.*)$", line, re.I)
        if not m or m[1] != name:
            continue
        ports = [p for p in m[2].split() if "=" not in p]
        j = i + 1
        while j < len(lines) and lines[j].lstrip().startswith("+"):
            ports += [p for p in lines[j].lstrip()[1:].split() if "=" not in p]
            j += 1
        return ports
    return None


def reference_module_names(verilog_text: str) -> list[str]:
    """Names of every module declared in a Verilog file (comments stripped)."""
    text = re.sub(r"/\*.*?\*/", " ", verilog_text, flags=re.S)
    text = re.sub(r"//[^\n]*", " ", text)
    names = re.findall(r"\b(?:module|macromodule)\s+(\\\S+|[A-Za-z_][\w$]*)", text)
    return [n[1:] if n.startswith("\\") else n for n in names]


def port_label_check(top_texts: list, ports: list[str]) -> dict:
    """Compare the GDS top-cell text labels with the reference netlist ports, both ways.

    Every reference port must be labelled in the top cell, and every top-cell
    label must name a reference port (exact, case-sensitive: labels and ports
    come from the same names). Several labels of one port (e.g. power pins on
    two layers) are fine; a label that is not a port is a failure.
    """
    labels = sorted({t[1] for t in top_texts})
    port_set = set(ports)
    missing = sorted(port_set - set(labels))
    extra = sorted(set(labels) - port_set)
    problems = []
    if missing:
        problems.append(f"reference ports without a top-cell label: {missing[:10]}")
    if extra:
        problems.append(f"top-cell labels that are not reference ports: {extra[:10]}")
    return dict(matched=not problems, problems=problems, labels=len(labels), ports=len(port_set),
                missing_labels=missing[:50], extra_labels=extra[:50])


def compare_texts(design: list, against: list, limit: int = 50) -> dict:
    """Multiset comparison of text records [layer, string, x_pm, y_pm] (both directions)."""
    a, b = Counter(map(tuple, design)), Counter(map(tuple, against))
    only_a, only_b = a - b, b - a
    return dict(identical=not only_a and not only_b, design=sum(a.values()), against=sum(b.values()),
                only_in_design=sum(only_a.values()), only_in_against=sum(only_b.values()),
                samples_only_in_design=[list(k) for k in sorted(only_a)[:limit]],
                samples_only_in_against=[list(k) for k in sorted(only_b)[:limit]])


def diff_sorted_lines(path_a, path_b, limit: int = 50) -> dict:
    """Streaming multiset diff of two sorted text files (bounded samples, exact counts).

    Raises ParseError if either file is not sorted (a merge would be meaningless).
    """
    only_a, only_b, samples_a, samples_b = 0, 0, [], []
    with open(path_a, encoding="utf-8") as fa, open(path_b, encoding="utf-8") as fb:
        def reader(handle):
            last = None
            for raw in handle:
                line = raw.rstrip("\n")
                if last is not None and line < last:
                    raise ParseError(f"{handle.name} is not sorted")
                last = line
                yield line
        ia, ib = reader(fa), reader(fb)
        la, lb = next(ia, None), next(ib, None)
        while la is not None or lb is not None:
            if la is not None and (lb is None or la < lb):
                only_a += 1
                if len(samples_a) < limit:
                    samples_a.append(la)
                la = next(ia, None)
            elif lb is not None and (la is None or lb < la):
                only_b += 1
                if len(samples_b) < limit:
                    samples_b.append(lb)
                lb = next(ib, None)
            else:
                la, lb = next(ia, None), next(ib, None)
    return dict(only_in_design=only_a, only_in_against=only_b, samples_only_in_design=samples_a,
                samples_only_in_against=samples_b)


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


def _pin_name(value) -> str:
    return (value or "").strip()


def _unnamed(value) -> bool:
    text = _pin_name(value)
    return not text or text.startswith("$")


def lvsdb_verdict(xref: dict, top: str, body_pin_pairs=(), reference_ports=None) -> dict:
    """KLayout LVS verdict from the kl_lvsdb.py dump.

    KLayout pairs pins by topology, so a symmetric design with swapped port
    labels still reports ``Match``. Besides every circuit pair being ``Match``
    and the top pin counts agreeing, every top-level pin pair must therefore
    pair a layout pin with the schematic pin of the SAME name (case-insensitive,
    the SPICE reader upper-cases names) and every top pin must be named. In
    child (library) cells a named-vs-named pin pair must also agree, except the
    body/substrate terminals the PDK deck synthesizes (``body_pin_pairs``,
    e.g. sky130_gnd<->VNB); unnamed deck-generated pins ($n) are only reported.

    Two holes of the pairing are closed explicitly: (1) a NAMED layout net of
    the top circuit (i.e. one carrying a label) that is not paired, or paired
    with a differently named or non-matching schematic net, fails -- KLayout
    leaves a labelled floating net (an extra pin) out of the comparison and
    still reports Match; (2) with ``reference_ports`` (the port list of the
    reference netlist), every reference port must appear as a paired top-level
    schematic pin and vice versa -- schematic simplification silently drops an
    unconnected reference port, so equal post-simplify pin counts prove nothing.
    """
    problems = []
    body = {(a.casefold(), b.casefold()) for a, b in body_pin_pairs}
    circuits = xref.get("circuits") or []
    if not circuits:
        problems.append("no circuit pairs in lvsdb")
    top_pair = [c for c in circuits if c.get("layout") == top or c.get("schematic") == top]
    if not top_pair:
        problems.append(f"top circuit {top} absent from cross-reference")
    if top_pair and top_pair[0].get("layout_pins") is not None and \
            top_pair[0].get("layout_pins") != top_pair[0].get("schematic_pins"):
        problems.append(f"top pin count layout {top_pair[0].get('layout_pins')} != schematic {top_pair[0].get('schematic_pins')}")
    if xref.get("pin_pairs_truncated"):
        problems.append("pin-pair dump truncated; pin names not fully compared")
    for c in circuits:
        if c.get("status") != "Match":
            problems.append(f"circuit {c.get('layout')}|{c.get('schematic')} status {c.get('status')}")
    top_pins = dict(pairs=None, name_mismatches=[], unnamed=[])
    if top_pair:
        pairs = top_pair[0].get("pin_pairs")
        if pairs is None:
            problems.append("top pin pairs not dumped; pin names cannot be compared")
        else:
            top_pins["pairs"] = len(pairs)
            if top_pair[0].get("layout_pins") is not None and len(pairs) < top_pair[0]["layout_pins"]:
                problems.append(f"only {len(pairs)} of {top_pair[0]['layout_pins']} top pins paired")
            for side in ("layout", "schematic"):
                names = [_pin_name(p.get(side)).casefold() for p in pairs if not _unnamed(p.get(side))]
                dup = sorted({n for n in names if names.count(n) > 1})
                if dup:
                    problems.append(f"duplicate top {side} pin names {dup[:6]}")
            for p in pairs:
                a, b = p.get("layout"), p.get("schematic")
                if _unnamed(a) or _unnamed(b):
                    top_pins["unnamed"].append([a, b])
                elif _pin_name(a).casefold() != _pin_name(b).casefold():
                    top_pins["name_mismatches"].append([a, b])
                if p.get("status") not in (None, "Match"):
                    problems.append(f"top pin {a}<->{b} status {p.get('status')}")
            if top_pins["unnamed"]:
                problems.append(f"unnamed top pin pairs {top_pins['unnamed'][:6]}")
            if top_pins["name_mismatches"]:
                problems.append("top pin names swapped/mismatched (layout<->schematic) "
                                + ", ".join(f"{a}<->{b}" for a, b in top_pins["name_mismatches"][:6]))
    nets = dict(named=None, unpaired=[], mismatched=[])
    if top_pair:
        named = top_pair[0].get("named_nets")
        if named is None:
            problems.append("named top-level nets not dumped; labelled floating nets cannot be excluded")
        else:
            nets["named"] = len(named)
            for n in named:
                lname, sname = _pin_name(n.get("layout")), n.get("schematic")
                if not n.get("paired") or _unnamed(sname):
                    nets["unpaired"].append(lname)
                elif n.get("status") not in (None, "Match") or lname.casefold() != _pin_name(sname).casefold():
                    nets["mismatched"].append([lname, sname, n.get("status")])
            if nets["unpaired"]:
                problems.append(f"labelled top-level layout nets not in the schematic (extra pins/labels): "
                                f"{nets['unpaired'][:6]}")
            if nets["mismatched"]:
                problems.append(f"labelled top-level nets paired with a different or non-matching schematic net: "
                                f"{nets['mismatched'][:6]}")
    if xref.get("named_nets_truncated"):
        problems.append("named-net dump truncated; labelled nets not fully compared")
    ports = dict(reference=None, unpaired_reference_ports=[], schematic_pins_not_ports=[])
    if reference_ports is not None and top_pair and top_pair[0].get("pin_pairs") is not None:
        want = {p.casefold(): p for p in reference_ports}
        paired = {_pin_name(p.get("schematic")).casefold() for p in top_pair[0]["pin_pairs"]
                  if not _unnamed(p.get("schematic")) and not _unnamed(p.get("layout"))}
        ports["reference"] = len(want)
        ports["unpaired_reference_ports"] = sorted(want[k] for k in set(want) - paired)
        ports["schematic_pins_not_ports"] = sorted(paired - set(want))
        if ports["unpaired_reference_ports"]:
            problems.append(f"reference ports not paired with a layout pin: {ports['unpaired_reference_ports'][:6]}")
        if ports["schematic_pins_not_ports"]:
            problems.append(f"paired schematic pins that are not reference ports: {ports['schematic_pins_not_ports'][:6]}")
    child = dict(compared=0, name_mismatches=[], body_pairs=0, unnamed=0)
    for c in circuits:
        if top_pair and c is top_pair[0]:
            continue
        for p in c.get("pin_pairs") or []:
            a, b = p.get("layout"), p.get("schematic")
            if _unnamed(a) or _unnamed(b):
                child["unnamed"] += 1
                continue
            child["compared"] += 1
            key = (_pin_name(a).casefold(), _pin_name(b).casefold())
            if key[0] == key[1]:
                continue
            if key in body:
                child["body_pairs"] += 1
                continue
            child["name_mismatches"].append([c.get("layout"), a, b])
    if child["name_mismatches"]:
        problems.append("library-cell pin names mismatched "
                        + ", ".join(f"{cell}:{a}<->{b}" for cell, a, b in child["name_mismatches"][:6]))
    return dict(matched=not problems, problems=problems, circuits=len(circuits),
                top=({k: v for k, v in top_pair[0].items() if k not in ("pin_pairs", "named_nets")} if top_pair else None),
                top_pins=top_pins, top_nets=dict(nets, unpaired=nets["unpaired"][:50], mismatched=nets["mismatched"][:50]),
                reference_ports=dict(ports, unpaired_reference_ports=ports["unpaired_reference_ports"][:50],
                                     schematic_pins_not_ports=ports["schematic_pins_not_ports"][:50]),
                child_pins=dict(child, name_mismatches=child["name_mismatches"][:20]),
                details=(xref.get("details") or [])[:40])


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
