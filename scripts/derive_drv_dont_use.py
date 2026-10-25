#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Derive the GF180 DRV-limited synthesis dont_use families from Liberty data.

usage: derive_drv_dont_use.py [--pdk gf180] [--out fixtures/flow/gf180-drv-dont-use.json] [--check]

Owner decision (2026-10-24, GF180 DRV / cell policy): the design SDC keeps
``set_max_transition 1.0``; GF180 synthesis gets a dont_use rule for every cell
family that cannot reach that output transition at ss_125C_3v00 even at minimum
load with a 0.2 ns input transition. This script measures it from the Liberty
file pinned (path and sha256) by fixtures/flow/signoff-pinned.json; nothing is
hand-written:

* every ``cell`` of the library is grouped into a family by its name without the
  drive suffix (``aoi211_1``, ``_2``, ``_4`` -> ``aoi211``);
* minimum load = the smallest input-pin capacitance of any cell synthesis may
  instantiate (one sink and no wire: the least a driven net can carry); cells
  of the PDK exclusion lists, the owner-reserved CTS/hold cells and cells
  without an output (antenna, fillers) are not sinks of a synthesized net.
  Where a table's smallest load index is larger (tri-state cells), that
  index is the minimum (no extrapolation).
  The value at the smallest load index of each table (a characterization
  point below any real sink) is recorded as well, as a sensitivity column;
* a member's transition is the WORST over all its output pins, timing arcs and
  ``rise_transition``/``fall_transition`` tables at (0.2 ns, minimum load),
  bilinearly interpolated, no extrapolation: any input may be driven, so every
  arc must meet the limit. ``three_state_disable`` arcs are skipped (the output
  is released to high impedance; no driven transition);
* a family is DRV-limited when even its best member exceeds the limit; cells
  without an output transition table are listed (``no_transition_table``) and
  never selected; families already wholly excluded are not repeated.

The output records the Liberty path, sha256 and units, the minimum-load sink,
every member's worst arc and value, and the family globs (``<prefix><family>_*``)
that scripts/cell_policy.py adds to the GF180 ``synth_dont_use`` (it recomputes
this file and refuses a stale one). ``--check`` recomputes and exits 1 when the
tracked file differs.
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
sys.path.insert(0, str(ROOT / "scripts/signoff"))
from liberty_lite import Library, Table, LibertyError  # noqa: E402
import manifest as manifest_mod  # noqa: E402

SCHEMA = "rustroad.drv-dont-use.v1"
PREFIX = {"gf180": "gf180mcu_fd_sc_mcu7t5v0__"}
CORNER = {"gf180": ("liberty_ss", "ss_125C_3v00")}
MAX_TRANSITION_NS = 1.0      # design SDC set_max_transition (unchanged, owner decision)
INPUT_TRANSITION_NS = 0.2    # owner decision
DEFAULT_OUT = ROOT / "fixtures/flow/gf180-drv-dont-use.json"
FAMILY = re.compile(r"^(?P<family>.+)_(?P<drive>\d+)$")


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def excluded_globs(m: dict, tag: str) -> list[str]:
    """PDK synthesis/PnR exclusion files and the owner-reserved CTS/hold cells (no DRV rule)."""
    import cell_policy
    _, synth, pnr = cell_policy.base_lists(m, tag)
    return synth + pnr + cell_policy.RESERVED[tag]


def lookup(table: Table, slew: float, load: float) -> float:
    return table.lookup(slew, load)


def output_tables(cell, lib: Library):
    for pin in cell.sub("pin"):
        if pin.attr("direction") != "output":
            continue
        for timing in pin.sub("timing"):
            if timing.attr("timing_type") == "three_state_disable":
                continue
            for kind in ("rise_transition", "fall_transition"):
                for g in timing.sub(kind):
                    yield pin, timing, kind, Table(g, lib)


def worst(cell, lib: Library, slew: float, load: float | None) -> dict | None:
    """Worst output transition over every arc at (slew, load).

    load None = each table's smallest index; a load below a table's smallest
    index is raised to it (tri-state tables start above the smallest sink).
    """
    out = None
    for pin, timing, kind, t in output_tables(cell, lib):
        axis = t.index_2 if t.variables[1] == "total_output_net_capacitance" else t.index_1
        at = min(axis) if load is None else max(load, min(axis))
        value = lookup(t, slew, at)
        if out is None or value > out["value"]:
            out = dict(value=value, pin=pin.name, related_pin=timing.attr("related_pin"),
                       timing_type=timing.attr("timing_type", "combinational"), table=kind, load=at)
    return out


def derive(tag: str, m: dict | None = None) -> dict:
    m = m or manifest_mod.load(manifest_mod.DEFAULT_MANIFEST)
    key, corner = CORNER[tag]
    path = manifest_mod.pdk_host_path(m, tag, key)
    pinned = m["pdks"][tag]["files"][key]["sha256"]
    digest = sha256(path)
    if digest != pinned:
        raise SystemExit(f"{path}: sha256 {digest} differs from the signoff manifest ({pinned})")
    if corner not in path.name:
        raise SystemExit(f"{path} is not the {corner} Liberty")
    lib = Library(path)
    t_unit, c_unit = lib.time_unit_ns(), lib.cap_unit_pf()
    slew = INPUT_TRANSITION_NS / t_unit
    prefix = PREFIX[tag]
    excluded = excluded_globs(m, tag)

    def is_excluded(name: str) -> bool:
        return any(fnmatch.fnmatchcase(name, g) for g in excluded)

    # Minimum load: smallest input pin of a cell synthesis may instantiate that has an output.
    sink = None
    for name in sorted(lib.cells):
        cell = lib.cells[name]
        if is_excluded(name) or not any(p.attr("direction") == "output" for p in cell.sub("pin")):
            continue
        for pin in cell.sub("pin"):
            if pin.attr("direction") == "input" and pin.attr("capacitance") is not None:
                cap = float(pin.attr("capacitance"))
                if sink is None or cap < sink["cap"]:
                    sink = dict(cap=cap, cell=name, pin=pin.name)
    if sink is None:
        raise SystemExit("no input pin capacitance found")
    families: dict[str, list] = {}
    no_table = []
    for name in sorted(lib.cells):
        if not name.startswith(prefix):
            raise SystemExit(f"cell {name} lacks the library prefix {prefix}")
        short = name[len(prefix):]
        match = FAMILY.match(short)
        family = match["family"] if match else short
        try:
            w = worst(lib.cells[name], lib, slew, sink["cap"])
            w_min = worst(lib.cells[name], lib, slew, None)
        except LibertyError as e:
            raise SystemExit(f"{name}: {e}")
        if w is None:
            no_table.append(name)
            continue
        families.setdefault(family, []).append(dict(
            cell=name, excluded_already=is_excluded(name),
            worst_transition_ns=round(w["value"] * t_unit, 6), pin=w["pin"], related_pin=w["related_pin"],
            timing_type=w["timing_type"], table=w["table"],
            at_table_min_load=dict(load_pf=round(w_min["load"] * c_unit, 9),
                                   worst_transition_ns=round(w_min["value"] * t_unit, 6),
                                   related_pin=w_min["related_pin"])))
    rows, limited = [], []
    for family, members in sorted(families.items()):
        usable = [x for x in members if not x["excluded_already"]]
        best = min(members, key=lambda x: x["worst_transition_ns"])
        best_min = min(x["at_table_min_load"]["worst_transition_ns"] for x in members)
        is_limited = best["worst_transition_ns"] > MAX_TRANSITION_NS
        rows.append(dict(family=family, best_member=best["cell"], best_ns=best["worst_transition_ns"],
                         best_ns_at_table_min_load=best_min, drv_limited=is_limited,
                         usable_members=len(usable), members=members))
        if is_limited and usable:
            limited.append(family)
    return dict(
        schema=SCHEMA, pdk=tag, corner=corner,
        liberty=dict(path=str(path), sha256=digest, bytes=path.stat().st_size, manifest_key=key,
                     time_unit_ns=t_unit, capacitive_load_unit_pf=c_unit),
        rule=dict(max_transition_ns=MAX_TRANSITION_NS, input_transition_ns=INPUT_TRANSITION_NS,
                  load=("smallest input-pin capacitance of a cell synthesis may instantiate (one sink, no wire); "
                        "PDK-excluded, owner-reserved and output-less cells are not sinks; raised to a table's smallest "
                        "load index where that is larger"),
                  member="worst over every output pin, timing arc and rise/fall transition table "
                         "(three_state_disable skipped), bilinear, no extrapolation",
                  family="DRV-limited when even its best member exceeds max_transition_ns",
                  source="owner decision 2026-10-24 (GF180 DRV / cell policy); SDC set_max_transition 1.0 unchanged"),
        minimum_load=dict(load_pf=round(sink["cap"] * c_unit, 9), cell=sink["cell"], pin=sink["pin"]),
        producer="scripts/derive_drv_dont_use.py",
        drv_limited_families=limited,
        dont_use=[f"{prefix}{f}_*" for f in limited],
        drv_limited_at_table_min_load=[r["family"] for r in rows
                                       if r["best_ns_at_table_min_load"] > MAX_TRANSITION_NS and r["usable_members"]],
        no_transition_table=no_table,
        families=rows)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--pdk", default="gf180", choices=sorted(PREFIX))
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--check", action="store_true", help="recompute and compare with --out")
    args = ap.parse_args(argv)
    result = derive(args.pdk)
    text = json.dumps(result, indent=1) + "\n"
    if args.check:
        same = args.out.is_file() and args.out.read_text() == text
        print(json.dumps(dict(check="identical" if same else "differs", out=str(args.out))))
        return 0 if same else 1
    args.out.write_text(text)
    print(json.dumps(dict(minimum_load=result["minimum_load"], families=result["drv_limited_families"],
                          best_ns={r["family"]: r["best_ns"] for r in result["families"] if r["drv_limited"]},
                          at_table_min_load=result["drv_limited_at_table_min_load"])))
    return 0


if __name__ == "__main__":
    sys.exit(main())
