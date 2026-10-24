#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate fixtures/pdk/<pdk>-sizing.json (schema rustroad.sizing-profile.v1).

Drive-strength families for the gate-sizing ECO (`rustroad apply-eco
--sizing-profile`, planned by scripts/hold_closure.Flow.size_eco). Everything
comes from PDK data: the tt Liberty file of the owner-fixed corners, the cell
LEF (widths) and the tracked cell policy. Nothing is fitted to timing results.

A family is a set of library cells that are interchangeable in a netlist:
identical pin names and directions, identical normalized Liberty `function`
of every output pin, and identical sequential description (`ff`/`latch`
attributes). Only cells synthesis may use are listed (cell policy
`synth_dont_use` and `pnr_exclude` are excluded, which also excludes the
clock-tree and hold-delay cells reserved for CTS/hold ECO); tri-state,
clock-gating, state-table and function-less cells are excluded with a reason.
Members are ordered by drive: the output pin's `max_capacitance` (tt), then
area. Families with a single member are kept (the planner then knows the
instance cannot be resized).

Drive data (`cells[].drive`, planner input of the closure loop's buffering
and sizing ECOs): for each owner-fixed corner Liberty (tt/ss/ff), the input
pin capacitances and, for families with one output pin, the worst delay and
output transition over that pin's timing arcs (max of rise/fall) at one
input slew and two loads (`drive_model`), read by bilinear lookup inside the
characterized table (no extrapolation). The planner interpolates linearly in
load between the two points; it is an estimate, the loop re-times every ECO.

Usage: build_sizing_profile.py [--pdk sky130|gf180|all] [--out-dir fixtures/pdk] [--check]
"""
from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import liberty_lite as lib  # noqa: E402

SCHEMA = "rustroad.sizing-profile.v1"
PROFILES = ROOT / "fixtures/pdk/local-routing.json"
POLICY = ROOT / "fixtures/flow/cell-policy.json"
# Corner Liberty names (scripts/hold_closure.CORNERS): tt is the profile's own.
CORNER_LIBS = {"sky130": [("tt", "tt_025C_1v80"), ("ss", "ss_100C_1v60"), ("ff", "ff_n40C_1v95")],
               "gf180": [("tt", "tt_025C_3v30"), ("ss", "ss_125C_3v00"), ("ff", "ff_n40C_3v60")]}
DRIVE_INPUT_SLEW_NS = 0.2
DRIVE_LOADS_PF = (0.005, 0.05)
SEQUENTIAL_KEYS = ("clocked_on", "next_state", "clear", "preset", "clear_preset_var1", "clear_preset_var2",
                   "enable", "data_in")


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def source(role: str, path: Path) -> dict:
    path = Path(path)
    return dict(role=role, path=str(path), sha256=sha256(path), bytes=path.stat().st_size)


def norm(expr: str | None) -> str:
    return re.sub(r"\s+", "", (expr or "").strip('"'))


def lef_widths(path: Path, dbu: int) -> dict[str, int]:
    out = {}
    for m in re.finditer(r"(?ms)^MACRO\s+(\S+)\s*$(.*?)^END\s+\1\s*$", Path(path).read_text()):
        size = re.search(r"(?m)^\s*SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)\s*;", m[2])
        if size:
            width = float(size[1]) * dbu
            if abs(width - round(width)) > 1e-6:
                raise SystemExit(f"{m[1]}: width {size[1]} um is not an integer number of DBU")
            out[m[1]] = round(width)
    return out


def describe(cell) -> tuple[dict | None, str | None]:
    """(signature, None) for a sizable cell, else (None, reason)."""
    if cell.sub("statetable") or cell.attr("clock_gating_integrated_cell"):
        return None, "state table or clock-gating cell"
    pins, functions = {}, {}
    for pin in cell.sub("pin"):
        direction = pin.attr("direction")
        if direction not in ("input", "output"):
            return None, f"pin {pin.name} direction {direction}"
        pins[pin.name] = direction
        if direction == "output":
            if pin.attr("three_state"):
                return None, "three-state output"
            if not pin.attr("function"):
                return None, f"output {pin.name} without function"
            functions[pin.name] = norm(pin.attr("function"))
    if cell.sub("bus") or cell.sub("bundle"):
        return None, "bus or bundle pins"
    if not functions:
        return None, "no output with a function"
    seq = []
    for g in cell.groups:
        if g.kind in ("ff", "latch", "ff_bank", "latch_bank"):
            seq.append([g.kind, [lib.unquote(a) for a in g.args]] +
                       [[k, norm(g.attrs.get(k))] for k in SEQUENTIAL_KEYS if g.attrs.get(k)])
    return dict(pins=dict(sorted(pins.items())), functions=dict(sorted(functions.items())), sequential=seq), None


def drive_data(cell, library) -> dict | None:
    """Input caps and the worst output delay/transition at DRIVE_LOADS_PF."""
    t, c = library.time_unit_ns(), library.cap_unit_pf()
    caps = {}
    for pin in cell.sub("pin"):
        if pin.attr("direction") == "input":
            try:
                caps[pin.name] = round(float(pin.attr("capacitance")) * c, 9)
            except (TypeError, ValueError):
                return None
    outputs = [p for p in cell.sub("pin") if p.attr("direction") == "output"]
    if len(outputs) != 1:
        return dict(input_cap_pf=dict(sorted(caps.items())))
    delay, slew = [0.0, 0.0], [0.0, 0.0]
    arcs = 0
    for arc in outputs[0].sub("timing"):
        if arc.attr("timing_type", "combinational") not in ("combinational", "rising_edge", "falling_edge"):
            continue
        for k, load in enumerate(DRIVE_LOADS_PF):
            for kind, out in (("cell_rise", delay), ("cell_fall", delay), ("rise_transition", slew),
                              ("fall_transition", slew)):
                for g in arc.sub(kind):
                    try:
                        v = lib.Table(g, library).lookup(DRIVE_INPUT_SLEW_NS / t, load / c) * t
                    except lib.LibertyError:
                        return dict(input_cap_pf=dict(sorted(caps.items())))
                    out[k] = max(out[k], v)
        arcs += 1
    if not arcs:
        return dict(input_cap_pf=dict(sorted(caps.items())))
    return dict(input_cap_pf=dict(sorted(caps.items())), output_pin=outputs[0].name,
                delay_ns=[round(x, 6) for x in delay], slew_ns=[round(x, 6) for x in slew])


def build(tag: str) -> dict:
    profile = next(p for p in json.loads(PROFILES.read_text())["pdks"] if p["tag"] == tag)
    policy = json.loads(POLICY.read_text())["pdks"][tag]
    liberty, cell_lef = Path(profile["liberty"]["path"]), Path(profile["cell_lef"]["path"])
    dbu = 1000 if tag == "sky130" else 2000
    widths = lef_widths(cell_lef, dbu)
    library = lib.Library(liberty)
    blocked = list(policy["synth_dont_use"]) + list(policy["pnr_exclude"])
    names = set(library.cells)
    corner_paths = [(c, Path(str(liberty).replace(CORNER_LIBS[tag][0][1], n))) for c, n in CORNER_LIBS[tag]]
    corner_libs = [(c, library if c == "tt" else lib.Library(path, wanted=names)) for c, path in corner_paths]
    families, excluded = {}, []
    for name in sorted(library.cells):
        cell = library.cells[name]
        if any(fnmatch.fnmatchcase(name, g) for g in blocked):
            excluded.append(dict(name=name, reason="cell policy synth_dont_use / pnr_exclude"))
            continue
        if name not in widths:
            excluded.append(dict(name=name, reason="not in the cell LEF"))
            continue
        sig, reason = describe(cell)
        if sig is None:
            excluded.append(dict(name=name, reason=reason))
            continue
        out_pin = sorted(sig["functions"])[0]
        cap = next((p.attr("max_capacitance") for p in cell.sub("pin") if p.name == out_pin), None)
        try:
            area = float(cell.attr("area"))
            cap = float(cap) if cap is not None else None
        except (TypeError, ValueError):
            excluded.append(dict(name=name, reason="non-numeric area or max_capacitance"))
            continue
        key = json.dumps(sig, sort_keys=True)
        drive = {}
        for corner, lib_c in corner_libs:
            data = drive_data(lib_c.cells[name], lib_c) if name in lib_c.cells else None
            if data is not None:
                drive[corner] = data
        families.setdefault(key, dict(signature=sig, cells=[]))["cells"].append(
            dict(name=name, width_dbu=widths[name], area_um2=area, max_capacitance_pf=cap, drive=drive))
    out = []
    for fam in families.values():
        cells = sorted(fam["cells"], key=lambda c: (c["max_capacitance_pf"] if c["max_capacitance_pf"] is not None
                                                    else -1.0, c["area_um2"], c["name"]))
        if len({c["max_capacitance_pf"] for c in cells}) != len(cells) and len(cells) > 1:
            # Equal drive data: area decides; record that the order is by area.
            order = "max_capacitance_pf then area_um2 (ties present)"
        else:
            order = "max_capacitance_pf"
        base = re.sub(r"_\d+$", "", cells[0]["name"])
        out.append(dict(id=base, order=order, pins=fam["signature"]["pins"], functions=fam["signature"]["functions"],
                        sequential=fam["signature"]["sequential"], cells=cells))
    ids = [f["id"] for f in out]
    for i, f in enumerate(out):
        if ids.count(f["id"]) > 1:
            f["id"] = f"{f['id']}#{sum(1 for x in out[:i] if x['id'] == f['id'])}"
    out.sort(key=lambda f: f["id"])
    return dict(schema=SCHEMA, pdk=tag, library=profile["name"], generator="scripts/build_sizing_profile.py",
                sources=[source("liberty_tt", liberty), source("cell_lef", cell_lef), source("cell_policy", POLICY)] +
                        [source(f"liberty_{c}", path) for c, path in corner_paths if c != "tt"],
                dbu_per_um=dbu, rule="same pins, directions, output functions and sequential description; "
                                     "policy synth_dont_use and pnr_exclude removed; ordered by output max_capacitance",
                drive_model=dict(input_slew_ns=DRIVE_INPUT_SLEW_NS, loads_pf=list(DRIVE_LOADS_PF),
                                 corners=[c for c, _ in corner_paths],
                                 rule="per corner Liberty: input pin capacitance; worst (max rise/fall, max over "
                                      "combinational and edge arcs of the one output pin) delay and output "
                                      "transition at input_slew_ns and each of loads_pf, bilinear inside the "
                                      "table; linear in load between the points is the planner's estimate"),
                families=out, excluded=excluded)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--pdk", default="all", choices=("sky130", "gf180", "all"))
    ap.add_argument("--out-dir", type=Path, default=ROOT / "fixtures/pdk")
    ap.add_argument("--check", action="store_true", help="fail if a tracked profile differs from a rebuild")
    args = ap.parse_args()
    bad = []
    for tag in (("sky130", "gf180") if args.pdk == "all" else (args.pdk,)):
        built = build(tag)
        text = json.dumps(built, indent=1) + "\n"
        path = args.out_dir / f"{tag}-sizing.json"
        if args.check:
            # The cell-policy source path names the checkout it was built in;
            # its sha256 (and everything else) must match.
            def norm(d):
                return dict(d, sources=[dict(x, path=None) if x["role"] == "cell_policy" else x for x in d["sources"]])
            if not path.is_file() or norm(json.loads(path.read_text())) != norm(built):
                bad.append(str(path))
        else:
            path.write_text(text)
            print(path)
    if bad:
        print(f"differs from a rebuild: {bad}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
