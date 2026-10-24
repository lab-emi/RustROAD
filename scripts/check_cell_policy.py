#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Independent standard-cell policy checker (reads the PDK exclusion files directly).

usage: check_cell_policy.py --pdk sky130|gf180 --stage synth|physical --netlist N.v [--netlist ...]
                            [--mapped MAPPED.v] [--policy fixtures/flow/cell-policy.json]
                            [--pdk-root DIR] [--out REPORT.json]

* synth: no instance may use a cell matching the PDK synthesis exclusion list,
  the PDK PnR/DRC exclusion list, the owner-reserved CTS/hold cells or (GF180)
  the owner's DRV-limited families (cannot meet 1.0 ns max_transition at ss).
* physical (CTS'd / final / powered netlists): no instance may use a PnR/DRC
  excluded cell; an instance that already exists in ``--mapped`` must still
  obey the synthesis rule; an instance added after synthesis may use an
  excluded cell only if it is a physical-only cell (tap/fill/decap/diode/...)
  or a clock/delay cell reserved for CTS and hold repair.

Every instantiated cell must be a library cell (a MACRO of the PDK cell LEF)
or a module defined in the scanned files; an instance of anything else (a
black-box wrapper, a misspelt or differently cased cell name) could hide an
excluded cell, so it makes the result ``unverified`` (Rule 7), never passed.

The PDK files are read from the Ciel PDK root (default: the host path pinned
in fixtures/flow/signoff-pinned.json), independently of the generated policy.
When ``--policy`` is given its glob lists and source hashes are cross-checked
against the PDK files. Exit: 0 passed, 1 failed, 3 unverified.
"""
from __future__ import annotations

import argparse
from collections import Counter
import fnmatch
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
FILES = {
    "sky130": dict(variant="sky130A", synth="libs.tech/librelane/sky130_fd_sc_hd/synth_excluded.cells",
                   pnr="libs.tech/librelane/sky130_fd_sc_hd/pnr_excluded.cells", prefix="sky130_fd_sc_hd__",
                   lef="libs.ref/sky130_fd_sc_hd/lef/sky130_fd_sc_hd.lef"),
    "gf180": dict(variant="gf180mcuD", synth="libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0/synth_exclude.cells",
                  pnr="libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0/pnr_exclude.cells", prefix="gf180mcu_fd_sc_mcu7t5v0__",
                  lef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef"),
}
MAX_MACROS = 100_000
# Owner decision (W4): GF180 delay/clock cells are reserved for CTS and hold repair.
OWNER_RESERVED = {"sky130": [], "gf180": ["dly*", "clkbuf*", "clkinv*"]}
# Owner decision (2026-10-24, GF180 DRV / cell policy): synthesis must not use the
# families that cannot meet the SDC 1.0 ns max_transition at ss_125C_3v00 even at
# minimum load with a 0.2 ns input. This is the owner's list as decided; the
# generator derives it from Liberty (scripts/derive_drv_dont_use.py) and a policy
# whose derived list differs is reported inconsistent (unverified).
OWNER_DRV_LIMITED = {"sky130": [],
                     "gf180": ["aoi211_*", "aoi221_*", "aoi222_*", "nor4_*", "oai22_*", "oai221_*", "oai222_*",
                               "oai31_*", "oai32_*", "oai33_*"]}
DRV_LIBERTY = {"gf180": "libs.ref/gf180mcu_fd_sc_mcu7t5v0/lib/gf180mcu_fd_sc_mcu7t5v0__ss_125C_3v00.lib"}
# Cells the physical flow may add after synthesis (suffixes after the library prefix).
PHYSICAL_ONLY = {"sky130": ["tap*", "decap*", "fill*", "diode*"],
                 "gf180": ["filltie", "fill_*", "fillcap_*", "endcap", "antenna"]}
CTS_HOLD = {"sky130": ["clkbuf*", "clkinv*", "clkinvlp*", "clkdlybuf*", "dlygate*", "dlymetal*"],
            "gf180": ["dly*", "clkbuf*", "clkinv*"]}
DECLARATIONS = {"input", "output", "inout", "wire", "reg", "tri", "wand", "wor", "tri0", "tri1",
                "supply0", "supply1", "assign", "parameter", "localparam", "defparam", "integer", "genvar"}
# Verilog keywords that can never be a cell name in a structural instance.
RESERVED = DECLARATIONS | {"module", "macromodule", "endmodule", "always", "initial", "generate",
                           "endgenerate", "begin", "end", "if", "else", "case", "endcase", "for",
                           "function", "endfunction", "task", "endtask", "specify", "endspecify",
                           "primitive", "endprimitive", "table", "endtable", "and", "or", "nand", "nor",
                           "xor", "xnor", "buf", "not", "bufif0", "bufif1", "notif0", "notif1"}
HARMLESS_DIRECTIVES = {"timescale", "default_nettype", "celldefine", "endcelldefine", "resetall"}
MAX_INSTANCES = 1_000_000
MAX_TOKENS = 50_000_000
MAX_REPORTED = 20
TOKEN = re.compile(r"""
    (?P<ws>\s+)
  | (?P<lcomment>//[^\n]*)
  | (?P<bcomment>/\*.*?\*/)
  | (?P<attr>\(\*(?!\s*\)).*?\*\))
  | (?P<directive>`[A-Za-z_]\w*[^\n]*)
  | (?P<escaped>\\\S+)
  | (?P<ident>[A-Za-z_][\w$]*)
  | (?P<number>\d[\w.]*(?:'[sS]?[bBoOdDhH][0-9a-fA-FxXzZ_?]+)?|'[sS]?[bBoOdDhH][0-9a-fA-FxXzZ_?]+)
  | (?P<string>"(?:[^"\\\n]|\\.)*")
  | (?P<punct>[()\[\]{},;.#:=+\-*/?!~&|^<>@%])
""", re.S | re.X)


class NetlistError(ValueError):
    """The netlist cannot be scanned completely."""


def tokens(text: str):
    """Lex structural Verilog; comments and (* attributes *) are dropped, escaped identifiers kept whole."""
    out, directives, pos, n = [], [], 0, len(text)
    while pos < n:
        m = TOKEN.match(text, pos)
        if m is None:
            raise NetlistError(f"unrecognised character {text[pos]!r} at offset {pos}")
        pos = m.end()
        kind = m.lastgroup
        if kind in ("ws", "lcomment", "bcomment", "attr"):
            continue
        if kind == "directive":
            directives.append(m[0].strip())
            continue
        out.append((kind, m[0]))
        if len(out) > MAX_TOKENS:
            raise NetlistError("token budget exceeded")
    return out, directives


def scan(text: str) -> dict:
    """Classify every module-level statement of a structural netlist.

    Returns instances [(cell, instance)], module names, and every statement
    or compiler directive that is not a recognised declaration or instance
    (``unrecognised``); a caller must not report a pass when that is non-empty,
    because an unparsed statement could hide an excluded cell (Rule 7).
    """
    try:
        toks, directives = tokens(text)
    except NetlistError as error:
        return dict(instances=[], modules=[], unrecognised=[str(error)], directives=[])
    instances_, modules, unrecognised = [], [], []
    for d in directives:
        word = re.match(r"`(\w+)", d)[1]
        if word not in HARMLESS_DIRECTIVES:
            unrecognised.append(f"compiler directive {d[:80]!r} can change the netlist")
    i, n = 0, len(toks)

    def balanced(j: int) -> int:
        """toks[j] is '(' ; return the index after its matching ')'."""
        depth = 0
        while j < n:
            if toks[j][1] == "(":
                depth += 1
            elif toks[j][1] == ")":
                depth -= 1
                if depth == 0:
                    return j + 1
            elif toks[j][1] == ";":
                break
            j += 1
        raise NetlistError("unbalanced parentheses")

    def skip_to_semicolon(j: int) -> int:
        while j < n and toks[j][1] != ";":
            j += 1
        return j + 1

    def name_tok(j: int) -> bool:
        return j < n and (toks[j][0] == "escaped" or (toks[j][0] == "ident" and toks[j][1] not in RESERVED))

    while i < n:
        kind, word = toks[i]
        try:
            if word == "endmodule":
                i += 1
                continue
            if word in ("module", "macromodule"):
                if i + 1 < n:
                    modules.append(toks[i + 1][1].lstrip("\\"))
                i = skip_to_semicolon(i)
                continue
            if word in DECLARATIONS:
                i = skip_to_semicolon(i)
                continue
            if word == ";":
                i += 1
                continue
            # CELL [#(params)] INST [range] (conns) {, INST [range] (conns)} ;
            if name_tok(i):
                cell = word.lstrip("\\")
                j = i + 1
                if j < n and toks[j][1] == "#":
                    if j + 1 < n and toks[j + 1][1] == "(":
                        j = balanced(j + 1)
                    else:
                        raise NetlistError("unsupported #delay/parameter form")
                found = []
                while True:
                    if not name_tok(j):
                        raise NetlistError("expected an instance name")
                    inst = toks[j][1].lstrip("\\")
                    j += 1
                    if j < n and toks[j][1] == "[":
                        while j < n and toks[j][1] != "]":
                            j += 1
                        j += 1
                    if j >= n or toks[j][1] != "(":
                        raise NetlistError("expected a port list")
                    j = balanced(j)
                    found.append((cell, inst))
                    if j < n and toks[j][1] == ",":
                        j += 1
                        continue
                    break
                if j >= n or toks[j][1] != ";":
                    raise NetlistError("instance not terminated by ';'")
                instances_ += found
                if len(instances_) > MAX_INSTANCES:
                    raise NetlistError("instance budget exceeded")
                i = j + 1
                continue
            raise NetlistError(f"unsupported statement starting with {word!r}")
        except NetlistError as error:
            end = skip_to_semicolon(i)
            if len(unrecognised) < MAX_REPORTED:
                snippet = " ".join(t[1] for t in toks[i:min(end, i + 12)])
                unrecognised.append(f"{error}: {snippet[:160]!r}")
            elif len(unrecognised) == MAX_REPORTED:
                unrecognised.append("further unrecognised statements omitted")
            i = max(end, i + 1)
    return dict(instances=instances_, modules=modules, unrecognised=unrecognised, directives=directives)


def instances(text: str) -> list[tuple[str, str]]:
    return scan(text)["instances"]


def read_list(path: Path) -> list[str]:
    return [line.split("#", 1)[0].strip() for line in path.read_text().splitlines()
            if line.split("#", 1)[0].strip()]


def lef_macros(path: Path) -> set[str]:
    """MACRO names of a cell LEF (bounded)."""
    names = set()
    with path.open(errors="replace") as handle:
        for line in handle:
            m = re.match(r"\s*MACRO\s+(\S+)", line)
            if m:
                names.add(m[1])
                if len(names) > MAX_MACROS:
                    raise NetlistError("LEF macro budget exceeded")
    return names


def match(cell: str, globs: list[str]) -> str | None:
    for g in globs:
        if fnmatch.fnmatchcase(cell, g):
            return g
    return None


def default_root(tag: str) -> Path:
    manifest = json.loads((ROOT / "fixtures/flow/signoff-pinned.json").read_text())
    return Path(manifest["pdks"][tag]["host_root"])


def check(tag: str, stage: str, netlists: list[Path], mapped: Path | None, policy: Path | None,
          pdk_root: Path | None) -> dict:
    spec = FILES[tag]
    root = pdk_root or default_root(tag)
    report = dict(schema="rustroad.cell-policy-check.v1", pdk=tag, stage=stage, status="unverified",
                  problems=[], violations=[], sources={})
    try:
        synth_path, pnr_path, lef_path = root / spec["synth"], root / spec["pnr"], root / spec["lef"]
        synth, pnr = read_list(synth_path), read_list(pnr_path)
        library = lef_macros(lef_path)
    except (OSError, NetlistError) as error:
        report["problems"].append(f"PDK exclusion file or cell LEF unreadable: {error}")
        return report
    if not library:
        report["problems"].append(f"no MACRO in the cell LEF {lef_path}")
        return report
    for key, path in (("synth_exclude", synth_path), ("pnr_exclude", pnr_path), ("cell_lef", lef_path)):
        report["sources"][key] = dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
    report["library_cells"] = len(library)
    reserved = [spec["prefix"] + g for g in OWNER_RESERVED[tag]]
    drv_limited = [spec["prefix"] + g for g in OWNER_DRV_LIMITED[tag]]
    physical_only = [spec["prefix"] + g for g in PHYSICAL_ONLY[tag]]
    cts_hold = [spec["prefix"] + g for g in CTS_HOLD[tag]]
    synth_forbidden = synth + pnr + reserved + drv_limited
    report["rules"] = dict(synth_forbidden=synth_forbidden, pnr_forbidden=pnr,
                           post_synthesis_allowed=physical_only + cts_hold)
    if policy is not None:
        data = json.loads(policy.read_text())["pdks"][tag]
        consistency = []
        if set(data["synth_dont_use"]) != set(synth_forbidden):
            consistency.append(f"policy synth_dont_use differs from PDK lists + owner reservation + DRV rule: "
                               f"missing {sorted(set(synth_forbidden) - set(data['synth_dont_use']))} "
                               f"extra {sorted(set(data['synth_dont_use']) - set(synth_forbidden))}")
        if set(data["pnr_exclude"]) != set(pnr):
            consistency.append("policy pnr_exclude differs from the PDK file")
        for src in data["sources"]:
            if src["role"] == "owner_drv_limit":
                if sorted(src.get("entries") or []) != sorted(drv_limited):
                    consistency.append(f"policy DRV-limited families {src.get('entries')} differ from the owner's "
                                       f"{drv_limited}")
                lib = root / DRV_LIBERTY.get(tag, "")
                got = hashlib.sha256(lib.read_bytes()).hexdigest() if tag in DRV_LIBERTY and lib.is_file() else None
                if got is None or got != src.get("liberty_sha256"):
                    consistency.append(f"policy DRV rule Liberty sha256 {str(src.get('liberty_sha256'))[:12]} != "
                                       f"PDK {str(got)[:12]}")
                continue
            got = report["sources"].get(src["role"], {}).get("sha256")
            if got != src["sha256"]:
                consistency.append(f"policy source {src['role']} sha256 {src['sha256'][:12]} != PDK {str(got)[:12]}")
        report["policy_consistency"] = consistency or "consistent"
        report["problems"] += consistency
    mapped_names = {}
    unknown_cells = Counter()
    if mapped is not None:
        scanned = scan(mapped.read_text())
        mapped_names = {inst: cell for cell, inst in scanned["instances"]}
        for why in scanned["unrecognised"]:
            report["problems"].append(f"{mapped}: unrecognised statement (cannot certify): {why}")
        for cell, _ in scanned["instances"]:
            if cell not in library and cell not in scanned["modules"]:
                unknown_cells[cell] += 1
    counts, files, scans = Counter(), [], []
    for path in netlists:
        if not path.is_file():
            report["problems"].append(f"netlist missing: {path}")
            continue
        scans.append((path, scan(path.read_text())))
    defined = {m for _, scanned in scans for m in scanned["modules"]}
    for path, scanned in scans:
        insts = scanned["instances"]
        for why in scanned["unrecognised"]:
            report["problems"].append(f"{path}: unrecognised statement (cannot certify): {why}")
        files.append(dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest(), instances=len(insts),
                          modules=scanned["modules"], unrecognised=len(scanned["unrecognised"])))
        for cell, _ in insts:
            if cell not in library and cell not in defined:
                unknown_cells[cell] += 1
        for cell, inst in insts:
            counts[cell] += 1
            g = match(cell, pnr)
            if g:
                report["violations"].append(dict(netlist=str(path), instance=inst, cell=cell, rule="pnr_exclude", glob=g))
                continue
            g = match(cell, synth_forbidden)
            if g is None:
                continue
            if stage == "synth" or inst in mapped_names:
                report["violations"].append(dict(netlist=str(path), instance=inst, cell=cell,
                                                 rule="synth_exclude_or_reserved", glob=g))
            elif not match(cell, physical_only + cts_hold):
                report["violations"].append(dict(netlist=str(path), instance=inst, cell=cell,
                                                 rule="excluded_cell_added_after_synthesis", glob=g))
    if unknown_cells:
        report["problems"].append(f"{sum(unknown_cells.values())} instances of {len(unknown_cells)} cells that are neither "
                                  f"library cells (cell LEF) nor modules defined in the scanned files (cannot certify "
                                  f"what they contain): {sorted(unknown_cells)[:10]}")
    report["unknown_cells"] = dict(sorted(unknown_cells.items())[:MAX_REPORTED])
    if stage == "physical" and mapped is None:
        report["problems"].append("physical stage needs --mapped to tell synthesized from inserted instances")
    report.update(netlists=files, cells=dict(sorted(counts.items())))
    if report["violations"]:
        report["status"] = "failed"
    elif report["problems"] or not files:
        report["status"] = "unverified"
    else:
        report["status"] = "passed"
    return report


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--pdk", choices=sorted(FILES), required=True)
    p.add_argument("--stage", choices=("synth", "physical"), required=True)
    p.add_argument("--netlist", type=Path, action="append", required=True)
    p.add_argument("--mapped", type=Path)
    p.add_argument("--policy", type=Path)
    p.add_argument("--pdk-root", type=Path)
    p.add_argument("--out", type=Path)
    args = p.parse_args(argv)
    report = check(args.pdk, args.stage, args.netlist, args.mapped, args.policy, args.pdk_root)
    if args.out:
        args.out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(dict(pdk=report["pdk"], stage=report["stage"], status=report["status"],
                          violations=len(report["violations"]), problems=report["problems"][:3])))
    return {"passed": 0, "failed": 1}.get(report["status"], 3)


if __name__ == "__main__":
    sys.exit(main())
