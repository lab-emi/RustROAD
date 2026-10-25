# SPDX-License-Identifier: Apache-2.0
"""Logic oracles of the RTL-to-GDS gate (W10): CEC and generic co-simulation.

* RustSys ``check synth`` (native) and Yosys ``equiv_*`` (second oracle) prove
  the RTL equivalent to a netlist: the mapped, final (post-CTS/ECO) and powered
  netlists. For the powered netlist the LEF-only physical cells
  (cell-policy ``physical_only`` masters with no output pin in the cell LEF:
  taps, fillers, decaps, diodes) are removed and listed, RustSys gets the
  supply ports with ``--supply-port`` and Yosys gets an unpowered view (supply
  pin connections and supply ports removed, also listed).
* A generic random co-simulation testbench is generated from the RTL port list
  (Yosys JSON): every non-clock input is driven from a fixed-seed xorshift64
  sequence, the clock port is toggled, and every output bit of the RTL and of
  the gate model (Yosys model of the netlist with the Liberty functions) is
  compared with ``!==`` before and after every clock edge, for N cycles. Both
  models start from Verilator's zero-initialised state; nothing in it is
  specific to a fixture.
* When the pinned Yosys cannot elaborate the RTL (its frontend lacks e.g. a
  package import in a module header) the port list comes from Verilator's
  XML elaboration instead (recorded as ``port_source``), and for a
  combinational design with at most ``EXHAUSTIVE_MAX_INPUT_BITS`` input bits
  the second CEC oracle is an exhaustive co-simulation of the RTL against the
  netlist (every input combination, complete for combinational logic). A
  clocked design keeps the Yosys verdict (then ``unverified``).
* ``bad_rtl`` builds a generic mutant: the top module is renamed in copies of
  the RTL and a wrapper with the original name inverts bit 0 of the first
  output port. Both CEC oracles must reject it.
"""
from __future__ import annotations

from collections import Counter
import fnmatch
import json
from pathlib import Path
import re

from rtl2gds_design import GateError, item

MAX_LEF_BYTES = 64 * 1024 * 1024
MAX_NETLIST_BYTES = 64 * 1024 * 1024
EXHAUSTIVE_MAX_INPUT_BITS = 22  # 4,194,304 vectors at most


def _text(path: Path, limit: int) -> str:
    path = Path(path)
    if path.stat().st_size > limit:
        raise GateError(f"{path} exceeds {limit} bytes")
    return path.read_text(errors="replace")


def ystr(path: Path) -> str:
    return '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'


COSIM_BUILD_FILE_BYTES = 512 * 1024 * 1024  # Verilator precompiled headers exceed the 64 MiB log cap


def run(h, label: str, argv: list, **kw) -> dict:
    return h.run(label, [str(a) for a in argv], **kw)


def out_text(receipt: dict) -> str:
    return _text(Path(receipt["stdout"]), 64 * 1024 ** 2) + _text(Path(receipt["stderr"]), 64 * 1024 ** 2)


# ----------------------------------------------------------------------------- LEF pins

def lef_pins(path: Path) -> dict[str, dict[str, dict]]:
    """MACRO -> PIN -> {direction, use} from a cell LEF (names only)."""
    text = _text(path, MAX_LEF_BYTES)
    out: dict[str, dict[str, dict]] = {}
    macro = pin = None
    for line in text.splitlines():
        words = line.split()
        if not words:
            continue
        if words[0] == "MACRO" and len(words) > 1:
            macro, pin = words[1], None
            out[macro] = {}
        elif macro and words[0] == "PIN" and len(words) > 1:
            pin = words[1]
            out[macro][pin] = dict(direction=None, use=None)
        elif macro and pin and words[0] == "DIRECTION" and len(words) > 1:
            out[macro][pin]["direction"] = " ".join(w for w in words[1:] if w != ";")
        elif macro and pin and words[0] == "USE" and len(words) > 1:
            out[macro][pin]["use"] = words[1].rstrip(";")
        elif words[0] == "END" and len(words) > 1:
            if pin and words[1] == pin:
                pin = None
            elif macro and words[1] == macro:
                macro = pin = None
    return out


# ----------------------------------------------------------------------------- netlist views

INSTANCE = re.compile(r"^(\s*)(\S+)\s+(\S+)\s+\((.*)\);\s*$")
CONN = re.compile(r"\.(\w+)\(([^()]*)\)")


def physical_only_view(text: str, physical_globs: list[str], pins: dict) -> tuple[str, dict]:
    """Remove instances of LEF-only physical masters (policy globs AND no output pin)."""
    kept, removed, refused = [], Counter(), []
    for line in text.splitlines(keepends=True):
        m = INSTANCE.match(line)
        if m and any(fnmatch.fnmatchcase(m[2], g) for g in physical_globs):
            outputs = [p for p, d in (pins.get(m[2]) or {}).items() if (d["direction"] or "").startswith("OUTPUT")]
            if m[2] not in pins or outputs:
                refused.append(f"{m[3]} ({m[2]}): {'not in the cell LEF' if m[2] not in pins else 'has output pins ' + str(outputs)}")
                kept.append(line)
                continue
            removed[m[2]] += 1
            continue
        kept.append(line)
    if refused:
        raise GateError(f"physical-only filter refused: {refused[:5]}")
    return "".join(kept), dict(removed_instances=sum(removed.values()), by_master=dict(sorted(removed.items())),
                              rule="cell-policy physical_only masters without any LEF OUTPUT pin")


def rail_constant_view(text: str, pins: dict, supply_ports: list[str]) -> tuple[str, dict]:
    """Signal pins (LEF USE not POWER/GROUND) wired to a supply rail get the
    rail's logic constant: power -> 1'b1, ground -> 1'b0 (rail-input tie
    cells). The supply pins themselves keep their rails. Listed."""
    const = {supply_ports[0]: "1'b1", supply_ports[1]: "1'b0"}
    lines, tied = [], []
    for line in text.splitlines(keepends=True):
        m = INSTANCE.match(line)
        if m:
            master = pins.get(m[2]) or {}
            conns = CONN.findall(m[4])
            new = []
            for p, sig in conns:
                if sig.strip() in const and (master.get(p) or {}).get("use") not in ("POWER", "GROUND"):
                    tied.append(f"{m[3]}/{p}={sig.strip()}")
                    sig = const[sig.strip()]
                new.append((p, sig))
            if new != conns:
                line = f"{m[1]}{m[2]} {m[3]} (" + ", ".join(f".{p}({s_})" for p, s_ in new) + ");\n"
        lines.append(line)
    return "".join(lines), dict(rail_tied_signal_pins=tied, rule="signal pin on a supply rail -> that rail's constant")


def unpowered_view(text: str, pins: dict, supply_ports: list[str]) -> tuple[str, dict]:
    """Drop supply-pin connections (LEF USE POWER/GROUND) and the supply ports."""
    lines, dropped, ports_removed = [], 0, []
    for line in text.splitlines(keepends=True):
        m = INSTANCE.match(line)
        if m:
            master = pins.get(m[2]) or {}
            conns = CONN.findall(m[4])
            keep = [(p, s) for p, s in conns if (master.get(p) or {}).get("use") not in ("POWER", "GROUND")]
            dropped += len(conns) - len(keep)
            lines.append(f"{m[1]}{m[2]} {m[3]} (" + ", ".join(f".{p}({s})" for p, s in keep) + ");\n")
            continue
        decl = re.match(r"^\s*inout\s+wire\s+(\S+?)\s*,?\s*$", line)
        if decl and decl[1] in supply_ports:
            ports_removed.append(decl[1])
            continue
        lines.append(line)
    out = "".join(lines)
    # The port list may now end with a comma before ');'.
    out = re.sub(r",(\s*\n\);)", r"\1", out, count=1)
    if sorted(ports_removed) != sorted(supply_ports):
        raise GateError(f"supply ports {supply_ports} not all found (removed {ports_removed})")
    return out, dict(supply_connections_removed=dropped, supply_ports_removed=ports_removed)


# ----------------------------------------------------------------------------- RTL ports and mutants

def rtl_module(h, label: str, yosys: Path, rtl: list[Path], top: str) -> dict:
    out = h.root / f"{label}.json"
    script = h.root / f"{label}.ys"
    script.write_text("".join(f"read_verilog -sv {ystr(p)}\n" for p in rtl) +
                      f"hierarchy -check -top {top}\nproc\nwrite_json {ystr(out)}\n")
    r = run(h, label, [yosys, "-Q", "-T", "-s", script])
    if r["exit_code"] != 0 or not out.is_file():
        raise GateError(f"{label}: Yosys could not elaborate the RTL")
    return json.loads(_text(out, 64 * 1024 ** 2))["modules"][top]


def verilator_ports(xml_text: str, top: str) -> list[dict]:
    """Top-module ports (declaration order) from ``verilator --xml-only`` output."""
    import xml.etree.ElementTree as ET
    root = ET.fromstring(xml_text)
    dtypes = {}
    for el in root.iter():
        if el.get("id") is not None and el.tag.endswith("dtype"):
            dtypes[el.get("id")] = el
    module = next((m for m in root.iter("module") if m.get("name") == top and m.get("topModule") == "1"), None)
    if module is None:
        raise GateError(f"Verilator XML has no top module {top}")
    ports = []
    for var in module.findall("var"):
        direction = var.get("dir")
        if direction not in ("input", "output", "inout"):
            continue
        dt, hops = dtypes.get(var.get("dtype_id")), 0
        while dt is not None and dt.get("left") is None and dt.get("sub_dtype_id") and hops < 16:
            dt, hops = dtypes.get(dt.get("sub_dtype_id")), hops + 1
        if dt is None or dt.tag not in ("basicdtype", "refdtype", "enumdtype"):
            raise GateError(f"port {var.get('name')}: unsupported Verilator dtype")
        if dt.get("left") is None:
            msb = lsb = 0
            vector = False
        else:
            msb, lsb = int(dt.get("left")), int(dt.get("right"))
            vector = True
        lo, hi = min(msb, lsb), max(msb, lsb)
        width = hi - lo + 1
        vector = vector and width > 1
        ports.append(dict(name=var.get("name"), direction=direction, width=width, msb=msb if vector else 0,
                          lsb=lsb if vector else 0, vector=vector,
                          bits=[f"{var.get('name')}[{i}]" for i in range(lo, hi + 1)] if vector else [var.get("name")],
                          pin_index=int(var.get("pinIndex") or 0)))
    ports.sort(key=lambda p: p.pop("pin_index"))
    return ports


def verilator_json_ports(text: str, top: str, max_nodes: int = 200000,
                         max_depth: int = 128, max_bits: int = 1000000) -> list[dict]:
    """Read only the public JSON output's top-level port declarations.

    Verilator 5.046+ removed XML output. This adapter is bounded and rejects
    unsupported aggregate types rather than guessing their interface width.
    """
    try:
        data = json.loads(text)
    except (ValueError, RecursionError) as error:
        raise GateError(f"invalid Verilator JSON: {error}") from error
    if not isinstance(data, dict) or data.get("type") != "NETLIST":
        raise GateError("Verilator JSON root is not a NETLIST")
    stack, count, types, modules = [(data, 0)], 0, {}, []
    while stack:
        node, depth = stack.pop()
        count += 1
        if count > max_nodes or depth > max_depth:
            raise GateError("Verilator JSON node/depth budget exceeded")
        if isinstance(node, dict):
            kind = str(node.get("type", ""))
            if kind.endswith("DTYPE") and node.get("addr"):
                if node["addr"] in types:
                    raise GateError("duplicate Verilator JSON type identifier")
                types[node["addr"]] = node
            if kind == "MODULE" and node.get("name") == top:
                modules.append(node)
            stack.extend((v, depth + 1) for v in reversed(list(node.values()))
                         if isinstance(v, (dict, list)))
        elif isinstance(node, list):
            stack.extend((v, depth + 1) for v in reversed(node) if isinstance(v, (dict, list)))
    if len(modules) != 1:
        raise GateError(f"Verilator JSON has no unique top module {top}")
    ports, names, bits = [], set(), 0
    for var in modules[0].get("stmtsp", []):
        if not isinstance(var, dict):
            raise GateError("invalid Verilator JSON statement")
        if var.get("type") != "VAR" or var.get("isPrimaryIO") is not True:
            continue
        name = var.get("origName") or var.get("name")
        direction = str(var.get("direction", "")).lower()
        if not name or name in names or direction not in ("input", "output", "inout"):
            raise GateError("invalid or duplicate Verilator JSON port")
        names.add(name)
        ref, seen = var.get("dtypep"), set()
        while ref in types and types[ref].get("type") in ("REFDTYPE", "ENUMDTYPE"):
            if ref in seen or len(seen) >= 16:
                raise GateError(f"port {name}: type-reference cycle/budget")
            seen.add(ref)
            ref = types[ref].get("refDTypep")
        dtype = types.get(ref)
        if not dtype or dtype.get("type") != "BASICDTYPE":
            raise GateError(f"port {name}: unsupported Verilator JSON dtype")
        declared_range = dtype.get("range")
        if declared_range is None:
            if dtype.get("keyword") not in ("logic", "bit", "reg", "wire"):
                raise GateError(f"port {name}: unsupported scalar dtype")
            msb = lsb = 0
        else:
            match = re.fullmatch(r"(-?\d+):(-?\d+)", str(declared_range))
            if not match:
                raise GateError(f"port {name}: invalid range")
            msb, lsb = map(int, match.groups())
        width = abs(msb - lsb) + 1
        bits += width
        if bits > max_bits:
            raise GateError("Verilator JSON port-bit budget exceeded")
        vector = width > 1
        ports.append(dict(name=name, direction=direction, width=width,
                          msb=msb if vector else 0, lsb=lsb if vector else 0, vector=vector,
                          bits=[f"{name}[{i}]" for i in range(min(msb, lsb), max(msb, lsb) + 1)]
                          if vector else [name]))
    if not ports:
        raise GateError(f"Verilator JSON top {top} has no ports")
    return ports


def rtl_ports(h, tools: dict, rtl: list[Path], top: str) -> tuple[list[dict], dict]:
    """Ports of the RTL top: Yosys, then the public Verilator XML/JSON interface."""
    from rtl2gds_design import ports_from_yosys_json
    try:
        return ports_from_yosys_json(rtl_module(h, "rtl_ports", tools["yosys"], rtl, top)), dict(tool="yosys")
    except GateError as error:
        yosys_error = str(error)
    out = h.root / "rtl_ports_verilator"
    r = run(h, "rtl_ports_verilator", [tools["verilator"], "--xml-only", "-Wno-fatal", "--Mdir", out,
                                       "--top-module", top, *rtl])
    xml = out / f"V{top}.xml"
    if r["exit_code"] == 0 and xml.is_file():
        ports, interface = verilator_ports(_text(xml, 64 * 1024 ** 2), top), "xml"
    else:
        result = run(h, "rtl_ports_verilator_json", [tools["verilator"], "--json-only", "-Wno-fatal",
                     "--Mdir", out, "--top-module", top, *rtl])
        tree = out / f"V{top}.tree.json"
        if result["exit_code"] != 0 or not tree.is_file():
            raise GateError(f"neither Yosys nor Verilator could elaborate the RTL ({yosys_error})")
        ports, interface = verilator_json_ports(_text(tree, 64 * 1024 ** 2), top), "json"
    return ports, dict(tool=f"verilator --{interface}-only", yosys=yosys_error,
                       yosys_frontend=yosys_frontend_error(h, tools["yosys"], rtl, top))


def yosys_frontend_error(h, yosys: Path, rtl: list[Path], top: str, label: str = "rtl_frontend_probe") -> str | None:
    """The first Yosys frontend ERROR line when it cannot read the RTL (None: it can)."""
    script = h.root / f"{label}.ys"
    script.write_text("".join(f"read_verilog -sv {ystr(p)}\n" for p in rtl) + f"hierarchy -check -top {top}\n")
    r = run(h, label, [yosys, "-Q", "-T", "-s", script])
    if r["exit_code"] == 0:
        return None
    m = re.search(r"(?m)^.*ERROR:.*$", out_text(r))
    return (m[0] if m else f"exit {r['exit_code']}")[:300]


def first_input_pin(netlist: Path, pins: dict) -> str | None:
    """``instance/pin`` of the first cell-LEF INPUT pin (non-supply) in a netlist."""
    for line in _text(netlist, MAX_NETLIST_BYTES).splitlines():
        m = INSTANCE.match(line)
        if not m or m[2] not in pins:
            continue
        for pin, _net in CONN.findall(m[4]):
            d = pins[m[2]].get(pin) or {}
            if (d.get("direction") or "").startswith("INPUT") and d.get("use") not in ("POWER", "GROUND"):
                return f"{m[3].lstrip(chr(92))}/{pin}"
    return None


def bad_rtl(rtl: list[Path], top: str, ports: list[dict], out_dir: Path) -> tuple[list[Path], dict]:
    """Renamed-top copies + a wrapper that inverts bit 0 of the first output."""
    out_dir.mkdir(parents=True, exist_ok=True)
    renamed, files = 0, []
    pattern = re.compile(r"\bmodule(\s+)" + re.escape(top) + r"\b")
    for i, src in enumerate(rtl):
        text, n = pattern.subn(r"module\g<1>" + top + "__rr_orig", _text(src, 16 * 1024 ** 2))
        renamed += n
        dst = out_dir / f"{i}_{src.name}"
        dst.write_text(text)
        files.append(dst)
    if renamed != 1:
        raise GateError(f"bad-RTL control: module {top} defined {renamed} times")
    outputs = [p for p in ports if p["direction"] == "output"]
    if not outputs:
        raise GateError("bad-RTL control: the top has no output")
    victim = outputs[0]

    def decl(p):
        rng = f" [{p['msb']}:{p['lsb']}]" if p["vector"] else ""
        return f"  {p['direction']} wire{rng} {p['name']}"
    body = [f"module {top}(\n" + ",\n".join(decl(p) for p in ports) + "\n);"]
    rng = f" [{victim['msb']}:{victim['lsb']}]" if victim["vector"] else ""
    body.append(f"  wire{rng} rr_mutant_{victim['name']};")
    conns = ", ".join(f".{p['name']}({'rr_mutant_' + p['name'] if p is victim else p['name']})" for p in ports)
    body.append(f"  {top}__rr_orig rr_orig ({conns});")
    if victim["vector"]:
        lsb = min(victim["msb"], victim["lsb"])
        body.append(f"  assign {victim['name']} = rr_mutant_{victim['name']} ^ {victim['width']}'d1;")
        flipped = f"{victim['name']}[{lsb}]"
    else:
        body.append(f"  assign {victim['name']} = ~rr_mutant_{victim['name']};")
        flipped = victim["name"]
    body.append("endmodule\n")
    wrapper = out_dir / "rr_mutant_top.v"
    wrapper.write_text("\n".join(body))
    return files + [wrapper], dict(mutation=f"output bit {flipped} inverted by a wrapper around the renamed top",
                                   files=[str(f) for f in files + [wrapper]])


# ----------------------------------------------------------------------------- CEC oracles

def rustsys_check(h, label: str, rustsys: Path, lib: Path, rtl: list[Path], top: str, netlist: Path,
                  verification: dict, supply_ports: list[str] = ()) -> dict:
    argv = [rustsys, "--threads", "2", "--progress", "off", "check", "synth", "--top", top, "--lib", lib,
            "--netlist", netlist, "--budget", verification["rustsys_budget"],
            "--model-budget", verification["rustsys_model_budget"],
            "--sweep-max-candidates", verification["rustsys_sweep_max_candidates"], "--print", "json"]
    for p in supply_ports:
        argv += ["--supply-port", p]
    r = run(h, label, argv + list(rtl))
    text = _text(Path(r["stdout"]), 64 * 1024 ** 2)
    try:
        verdict = json.loads(text).get("verdict")
    except json.JSONDecodeError:
        verdict = None
    status = {"equivalent": "passed", "not-equivalent": "failed"}.get(verdict, "unverified")
    return item(status, None if status == "passed" else f"RustSys check synth verdict {verdict!r} (exit {r['exit_code']})",
                verdict=verdict, exit_code=r["exit_code"], receipt=label, supply_ports=list(supply_ports))


def yosys_equiv(h, label: str, yosys: Path, lib: Path, rtl: list[Path], top: str, netlist: Path, seq: int) -> dict:
    script = h.root / f"{label}.ys"
    script.write_text(
        f"read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {ystr(lib)}\n"
        f"read_verilog {ystr(netlist)}\nhierarchy -top {top}\nproc\nflatten\nopt\nrename {top} gate\n"
        "design -stash gate\ndesign -reset\n" +
        "".join(f"read_verilog -sv {ystr(p)}\n" for p in rtl) +
        # splitnets: the netlist names register bits ``\\q[0]``; splitting the
        # RTL's multi-bit wires gives the same names, so equiv_make pairs the
        # state bits and equiv_induct can prove them by induction.
        f"hierarchy -top {top}\nproc\nflatten\nmemory\nopt\nsplitnets\nrename {top} gold\ndesign -stash gold\n"
        "design -copy-from gold -as gold gold\ndesign -copy-from gate -as gate gate\n"
        # async2sync on both sides: the SAT models have no asynchronous
        # set/reset flip-flop; the same transformation of gold and gate models
        # the reset as an immediate override.
        "async2sync\n"
        f"equiv_make gold gate equiv\nhierarchy -top equiv\nequiv_simple -seq {int(seq)} -undef\n"
        f"equiv_induct -undef -seq {int(seq)}\nequiv_status -assert\n")
    r = run(h, label, [yosys, "-Q", "-T", "-s", script])
    stdout, stderr = _text(Path(r["stdout"]), 64 * 1024 ** 2), _text(Path(r["stderr"]), 64 * 1024 ** 2)
    if r["exit_code"] == 0 and "Equivalence successfully proven!" in stdout:
        return item("passed", receipt=label)
    if r["exit_code"] == 1 and "unproven $equiv cells" in stderr:
        m = re.search(r"Found (\d+) unproven \$equiv cells", stderr)
        return item("failed", f"Yosys equiv: {m[1] if m else '?'} unproven $equiv cells", receipt=label)
    return item("unverified", f"Yosys equiv did not reach a verdict (exit {r['exit_code']})", receipt=label)


def cec_pair(h, tag: str, tools: dict, lib: Path, rtl: list[Path], top: str, netlist: Path, verification: dict,
             supply_ports: list[str] = (), yosys_netlist: Path | None = None, exhaustive: dict | None = None) -> dict:
    """RustSys + Yosys; ``exhaustive`` (combinational designs only) adds the
    exhaustive co-simulation as the second oracle when Yosys cannot read the RTL."""
    out = dict(rustsys=rustsys_check(h, f"{tag}_rustsys", tools["rustsys"], lib, rtl, top, netlist, verification,
                                     supply_ports),
               yosys=yosys_equiv(h, f"{tag}_yosys", tools["yosys"], lib, rtl, top, yosys_netlist or netlist,
                                 verification["yosys_equiv_seq"]))
    if out["yosys"]["status"] == "unverified" and exhaustive is not None:
        frontend = yosys_frontend_error(h, tools["yosys"], rtl, top, f"{tag}_frontend_probe")
        if frontend is not None:
            out["yosys"]["reason"] = f"Yosys frontend cannot elaborate the RTL: {frontend}"
            out["exhaustive"] = cosim(h, f"{tag}_exhaustive", tools, lib, rtl, top, yosys_netlist or netlist,
                                      exhaustive["ports"], None, 1 << exhaustive["input_bits"], exhaustive["seed"],
                                      exhaustive=True)
    return out


def second_oracle(pair: dict) -> dict:
    """The independent (non-RustSys) CEC verdict of a ``cec_pair`` result."""
    if pair.get("exhaustive") is not None:
        e = pair["exhaustive"]
        return dict(oracle=f"exhaustive co-simulation ({(e.get('expected') or {}).get('cycles')} vectors, "
                           "Yosys cannot read the RTL)", status=e["status"])
    return dict(oracle="Yosys equiv_simple", status=pair["yosys"]["status"])


# ----------------------------------------------------------------------------- co-simulation

def xorshift_tb(top: str, ports: list[dict], clock: str | None, vectors: int, seed: int,
                exhaustive: bool = False) -> str:
    """Generic random RTL-vs-gate testbench (SystemVerilog, Verilator --timing).

    Every variable is initialised in its declaration and the loop is a
    ``repeat``: Verilator 5.032 with its default optimisation constant-folds
    variables that an initial block assigns before its first delay (observed:
    a counter incremented 4096 times reads 0 afterwards), which would make a
    comparison vacuous. The testbench therefore also prints how many cycles
    ran, how many port comparisons it made and the final simulation time; the
    harness requires all three to match (and builds with -O0).

    ``exhaustive`` (combinational, no clock): the inputs are an index counter
    instead of the random sequence, ``vectors`` must be 2**(input bits), and
    every input combination is applied once.
    """
    if any(p["direction"] == "inout" for p in ports):
        raise GateError("co-simulation: inout ports are not supported")
    ins = [p for p in ports if p["direction"] == "input" and p["name"] != clock]
    outs = [p for p in ports if p["direction"] == "output"]
    if not outs:
        raise GateError("co-simulation: the top has no output")
    rng = lambda p: f"[{p['width'] - 1}:0] " if p["width"] > 1 else ""  # noqa: E731
    lines = ["module rr_cosim_tb;"]
    if clock:
        lines.append(f"  reg {clock} = 1'b0;")
    lines += [f"  reg {rng(p)}{p['name']} = '0;" for p in ins]
    lines += [f"  wire {rng(p)}gold_{p['name']}, gate_{p['name']};" for p in outs]

    def conns(prefix):
        return ", ".join(f".{p['name']}({prefix + p['name'] if p['direction'] == 'output' else p['name']})"
                         for p in ports)
    lines.append(f"  {top} gold ({conns('gold_')});")
    lines.append(f"  gate dut ({conns('gate_')});")
    lines += [f"  reg [63:0] s = 64'h{seed:016x};", "  integer errors = 0;", "  integer checks = 0;",
              "  integer cycle = 0;", "  integer phase = 0;"]
    compare = []
    for p in outs:
        compare.append(f"      checks = checks + 1; if (gold_{p['name']} !== gate_{p['name']}) begin errors = errors + 1; "
                       f"if (errors <= 8) $display(\"RR_COSIM_MISMATCH cycle=%0d phase=%0d port={p['name']} "
                       f"gold=%b gate=%b\", cycle, phase, gold_{p['name']}, gate_{p['name']}); end")
    if exhaustive:
        bits = sum(p["width"] for p in ins)
        if clock or vectors != 1 << bits or bits == 0:
            raise GateError("exhaustive co-simulation needs a combinational design and 2**inputs vectors")
        lines.append(f"  reg [{bits}:0] idx = '0;")
    lines += ["  initial begin", f"    repeat ({int(vectors)}) begin"]
    if exhaustive:
        lines.append("      {" + ", ".join(p["name"] for p in ins) + f"}} = idx[{bits - 1}:0]; idx = idx + 1;")
    for p in ([] if exhaustive else ins):
        for k in range((p["width"] + 63) // 64):
            hi = min(p["width"], (k + 1) * 64) - 1
            lo = k * 64
            sel = f"[{hi}:{lo}]" if p["width"] > 1 else ""
            lines.append("      s = s ^ (s << 13); s = s ^ (s >> 7); s = s ^ (s << 17);")
            lines.append(f"      {p['name']}{sel} = s[{hi - lo}:0];")
    lines.append("      #1; phase = 0;")
    lines += compare
    if clock:
        lines.append(f"      {clock} = 1'b1; #1; phase = 1;")
        lines += compare
        lines.append(f"      {clock} = 1'b0; #1;")
    lines += ["      cycle = cycle + 1;", "    end",
              "    $display(\"RR_COSIM_DONE cycles=%0d checks=%0d errors=%0d time=%0t\", cycle, checks, errors, $time);",
              "    if (errors == 0) $display(\"RR_COSIM_PASS\");", "    else $display(\"RR_COSIM_FAIL\");",
              "    $finish;", "  end", "endmodule", ""]
    return "\n".join(lines)


def expected_counts(ports: list[dict], clock: str | None, vectors: int) -> dict:
    outs = sum(1 for p in ports if p["direction"] == "output")
    phases = 2 if clock else 1
    return dict(cycles=vectors, checks=vectors * phases * outs, time=vectors * (3 if clock else 1))


def cosim(h, tag: str, tools: dict, lib: Path, rtl: list[Path], top: str, netlist: Path, ports: list[dict],
          clock: str | None, vectors: int, seed: int, exhaustive: bool = False) -> dict:
    model = h.root / f"{tag}_gate_model.v"
    script = h.root / f"{tag}_gate_model.ys"
    script.write_text(f"read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {ystr(lib)}\n"
                      f"read_verilog {ystr(netlist)}\nhierarchy -top {top}\nproc\nflatten\nrename {top} gate\n"
                      f"write_verilog -noattr {ystr(model)}\n")
    r = run(h, f"{tag}_gate_model", [tools["yosys"], "-Q", "-T", "-s", script])
    if r["exit_code"] != 0:
        return item("unverified", "Yosys gate model failed")
    tb = h.root / f"{tag}_cosim_tb.sv"
    tb.write_text(xorshift_tb(top, ports, clock, vectors, seed, exhaustive))
    build = h.root / f"{tag}_cosim_build"
    r = run(h, f"{tag}_cosim_build", [tools["verilator"], "--binary", "--timing", "-O0", "-Wno-fatal", "-j", "2",
                                      "--top-module", "rr_cosim_tb", "--Mdir", build, *rtl, model, tb],
            file_limit_bytes=COSIM_BUILD_FILE_BYTES)
    if r["exit_code"] != 0:
        return item("unverified", "Verilator build failed")
    r = run(h, f"{tag}_cosim_run", [build / "Vrr_cosim_tb"])
    text = out_text(r)
    want = expected_counts(ports, clock, int(vectors))
    done = re.search(r"RR_COSIM_DONE cycles=(\d+) checks=(\d+) errors=(\d+) time=(\d+)", text)
    got = dict(cycles=int(done[1]), checks=int(done[2]), time=int(done[4])) if done else None
    common = dict(vectors=int(vectors), seed=None if exhaustive else f"0x{seed:016x}",
                  stimulus="exhaustive (every input combination)" if exhaustive else "xorshift64 random",
                  expected=want, observed=got,
                  compared_outputs=[p["name"] for p in ports if p["direction"] == "output"],
                  initial_state="Verilator zero-initialised state in both models", testbench=str(tb))
    if r["exit_code"] != 0 or got != want:
        return item("unverified", f"co-simulation incomplete: ran {got}, expected {want} (exit {r['exit_code']})",
                    **common)
    if int(done[3]) == 0 and "RR_COSIM_PASS" in text:
        return item("passed", **common)
    return item("failed", f"RTL and gate outputs differ in {done[3]} comparisons",
                mismatches=re.findall(r"RR_COSIM_MISMATCH.*", text)[:8], **common)
