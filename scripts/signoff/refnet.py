# SPDX-License-Identifier: Apache-2.0
"""Reference-netlist helpers for LVS (bounded; explicit diagnostics).

* ``parse_verilog``: flat structural Verilog as exported by RustROAD
  (``export-verilog --include-power``) or RustSys: one module, ANSI ports,
  ``wire`` declarations, ``assign <port bit> = <net>;`` aliases and named-port
  cell instances. Anything else is rejected with a diagnostic, never skipped.
* ``parse_subckts``: ``.subckt`` blocks of a PDK SPICE/CDL library.
* ``spice_reference``: a SPICE/CDL top-level reference for KLayout LVS built
  from the parsed Verilog plus the PDK cell subcircuits it uses. Device-less
  physical cells (tap/fill/endcap without any device line) are dropped from
  the schematic, because extraction produces no devices for them; the list of
  dropped instances is reported. Optional unit rescaling converts the PDK
  SPICE ``w=650000u`` convention to metres-with-``u`` (``w=0.65u``).
"""
from __future__ import annotations

from collections import Counter
import re

MAX_INSTANCES = 500_000
MAX_BITS = 1_000_000
IDENT = r"(?:\\\S+|[A-Za-z_][A-Za-z0-9_$]*)"
DEVICE_PREFIXES = ("m", "x", "d", "r", "c", "q", "j")


class NetlistError(ValueError):
    """Unsupported or malformed construct (explicit diagnostic)."""


def _norm(name: str) -> str:
    name = name.strip()
    if name.startswith("\\"):
        name = name[1:].strip()
    return name


def _strip_comments(text: str) -> str:
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    return re.sub(r"//[^\n]*", " ", text)


def parse_verilog(text: str) -> dict:
    text = _strip_comments(text)
    modules = re.findall(r"\bmodule\b", text)
    if len(modules) != 1:
        raise NetlistError(f"expected exactly one module, found {len(modules)}")
    header = re.search(r"\bmodule\s+(" + IDENT + r")\s*\((.*?)\)\s*;", text, re.S)
    if header is None:
        raise NetlistError("module header not recognised (ANSI port list required)")
    top = _norm(header[1])
    body_start = header.end()
    body_end = text.find("endmodule", body_start)
    if body_end < 0:
        raise NetlistError("missing endmodule")
    ports, directions = [], {}
    for decl in [d.strip() for d in header[2].split(",") if d.strip()]:
        m = re.fullmatch(r"(input|output|inout)\s+(?:wire\s+)?(?:\[(\d+):(\d+)\]\s*)?(" + IDENT + r")\s*", decl + " ")
        if m is None:
            raise NetlistError(f"unsupported port declaration: {decl!r}")
        name = _norm(m[4])
        if m[2] is not None:
            hi, lo = int(m[2]), int(m[3])
            step = -1 if hi >= lo else 1
            bits = [f"{name}[{i}]" for i in range(hi, lo + step, step)]
        else:
            bits = [name]
        for bit in bits:
            ports.append(bit)
            directions[bit] = m[1]
        if len(ports) > MAX_BITS:
            raise NetlistError("port bit budget exceeded")
    body = text[body_start:body_end]
    aliases: dict[str, str] = {}
    instances = []
    for stmt in [s.strip() for s in body.split(";")]:
        if not stmt:
            continue
        if re.match(r"wire\b", stmt):
            continue
        if re.match(r"(input|output|inout|reg|supply0|supply1|tri\w*)\b", stmt):
            raise NetlistError(f"unsupported body declaration: {stmt[:80]!r}")
        if stmt.startswith("assign"):
            m = re.fullmatch(r"assign\s+(" + IDENT + r"(?:\s*\[\d+\])?)\s*=\s*(" + IDENT + r"(?:\s*\[\d+\])?|\d+'[bh][01xz]+)\s*", stmt + " ")
            if m is None:
                raise NetlistError(f"unsupported assign: {stmt[:80]!r}")
            lhs, rhs = _norm(m[1]).replace(" ", ""), _norm(m[2]).replace(" ", "")
            if "'" in rhs:
                raise NetlistError(f"constant assign {stmt!r}: tie cells are required for LVS")
            if lhs not in directions:
                raise NetlistError(f"assign target {lhs!r} is not a port bit")
            aliases[lhs] = rhs
            continue
        m = re.fullmatch(r"(" + IDENT + r")\s+(" + IDENT + r")\s*\((.*)\)", stmt, re.S)
        if m is None:
            raise NetlistError(f"unsupported statement: {stmt[:80]!r}")
        pins = {}
        for pm in re.finditer(r"\.(\w+)\s*\(\s*([^()]*?)\s*\)", m[3]):
            net = _norm(pm[2]).replace(" ", "")
            if not net:
                raise NetlistError(f"unconnected pin {pm[1]} on {m[2]}")
            if "'" in net:
                raise NetlistError(f"constant on pin {pm[1]} of {m[2]}")
            pins[pm[1]] = net
        leftover = re.sub(r"\.(\w+)\s*\(\s*([^()]*?)\s*\)", "", m[3]).replace(",", "").strip()
        if leftover:
            raise NetlistError(f"positional or malformed connections on {m[2]}: {leftover[:60]!r}")
        instances.append(dict(cell=_norm(m[1]), name=_norm(m[2]), pins=pins))
        if len(instances) > MAX_INSTANCES:
            raise NetlistError("instance budget exceeded")
    # Map internal net -> port bit; two port bits on one net would short pins.
    net_to_port: dict[str, str] = {}
    for port, net in aliases.items():
        if net in directions:
            raise NetlistError(f"port {port} aliased to port {net}: physically shorted ports are unsupported")
        if net in net_to_port:
            raise NetlistError(f"ports {net_to_port[net]} and {port} alias net {net}: shorted ports are unsupported")
        net_to_port[net] = port
    return dict(top=top, ports=ports, directions=directions, aliases=aliases,
                net_to_port=net_to_port, instances=instances)


def parse_subckts(text: str) -> dict:
    """Return {name: dict(pins=[...], text=block, devices=count)} (case-insensitive keywords)."""
    out = {}
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        line = lines[i]
        m = re.match(r"\s*\.subckt\s+(\S+)\s*(.*)$", line, re.I)
        if not m:
            i += 1
            continue
        name = m[1]
        header = [line]
        pins = [p for p in m[2].split() if "=" not in p]
        j = i + 1
        while j < len(lines) and lines[j].lstrip().startswith("+"):
            header.append(lines[j])
            pins += [p for p in lines[j].lstrip()[1:].split() if "=" not in p]
            j += 1
        body = []
        while j < len(lines) and not re.match(r"\s*\.ends\b", lines[j], re.I):
            body.append(lines[j])
            j += 1
        if j >= len(lines):
            raise NetlistError(f"subckt {name} has no .ends")
        devices = sum(1 for b in body if b.strip() and b.strip()[0].lower() in DEVICE_PREFIXES)
        out[name] = dict(pins=pins, text="\n".join(header + body + [lines[j]]) + "\n", devices=devices)
        i = j + 1
    return out


def _rescale_units(block: str) -> str:
    def scale(match):
        value = float(match[2]) * 1e-6
        return f"{match[1]}={value:.6g}u"
    return re.sub(r"\b([wlWL])=([0-9.]+(?:[eE][+-]?\d+)?)u\b", scale, block)


def spice_reference(netlist: dict, library: dict, *, rescale_units: bool, cdl: bool) -> tuple[str, dict]:
    used, lines, dropped = [], [], Counter()
    def net_name(net):
        return netlist["net_to_port"].get(net, net)
    for inst in netlist["instances"]:
        sub = library.get(inst["cell"])
        if sub is None:
            raise NetlistError(f"cell {inst['cell']} not found in the PDK library netlist")
        if sub["devices"] == 0:
            dropped[inst["cell"]] += 1
            continue
        missing = [p for p in sub["pins"] if p not in inst["pins"]]
        extra = [p for p in inst["pins"] if p not in sub["pins"]]
        if missing or extra:
            raise NetlistError(f"{inst['name']} ({inst['cell']}): pins missing {missing} extra {extra}")
        nets = [net_name(inst["pins"][p]) for p in sub["pins"]]
        lines.append(f"X{inst['name']} {' '.join(nets)} {inst['cell']}")
        if inst["cell"] not in used:
            used.append(inst["cell"])
    blocks = []
    for cell in used:
        block = library[cell]["text"]
        if rescale_units:
            block = _rescale_units(block)
        blocks.append(block)
    kw_sub, kw_end = (".SUBCKT", ".ENDS") if cdl else (".subckt", ".ends")
    text = ["* LVS reference generated by scripts/signoff/refnet.py from structural Verilog",
            *blocks, f"{kw_sub} {netlist['top']} {' '.join(netlist['ports'])}", *lines,
            f"{kw_end} {netlist['top']}", ""]
    return "\n".join(text), dict(instances=len(lines), cells=used, dropped_deviceless=dict(dropped))
