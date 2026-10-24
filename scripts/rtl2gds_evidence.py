#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Condense finished RTL-to-GDS gate runs into one tracked evidence file.

usage: rtl2gds_evidence.py --out docs/evidence/rtl2gds-final.json RUN_DIR [RUN_DIR ...]

Reads each RUN_DIR/summary.json (schema rustroad.rtl2gds.v1) and the bound
artifacts it names (final netlist, DEF, binding record) and writes, per design
x PDK: the gate verdict and every gating status (status + short reason), the
worst setup/hold per corner, SPEF source and engine, RustSTA-vs-OpenSTA parity
per corner and SPEF, the closure iterations with the hold, sizing and antenna
ECO cells, cell counts by role, die/core/standard-cell area, signoff counts,
controls, determinism, and wall time / peak RSS per step. Paths are reduced
to file names; the run directories keep everything. Nothing is recomputed
from timing: values are those the gate recorded.
"""
from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "rustroad.rtl2gds-final.v1"
INSTANCE = re.compile(r"^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$")


def short(reason, n=400):
    if reason is None:
        return None
    text = re.sub(r"(/[^\s'\"\],]+)", lambda m: Path(m[1]).name, str(reason))
    return text if len(text) <= n else text[:n] + "..."


def lef_sizes(path: Path) -> dict[str, tuple[float, float]]:
    text = Path(path).read_text()
    return {m[1]: (float(m[2]), float(m[3])) for m in
            re.finditer(r"(?ms)^MACRO\s+(\S+)\s*$.*?^\s*SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)", text)}


def roles(netlist: Path, sizes: dict, physical: set[str]) -> dict:
    count, area = Counter(), Counter()
    for line in netlist.read_text().splitlines():
        m = INSTANCE.match(line)
        if not m or m[1] in ("module", "input", "output", "wire", "assign", "inout"):
            continue
        master, name = m[1], m[2].lstrip("\\")
        if name.startswith("rr_eco_ant"):
            role = "antenna_diode"
        elif master in physical:
            role = "physical_only"
        elif name.startswith("rr_clkbuf"):
            role = "clock_buffer"
        elif name.startswith("rr_eco_hold"):
            role = "hold_cell"
        elif name.startswith("rr_eco_buf"):
            role = "eco_buffer"
        elif name.startswith("rr_tie"):
            role = "tie"
        else:
            role = "logic"
        count[role] += 1
        w, h = sizes.get(master, (0.0, 0.0))
        area[role] += w * h
    return dict(count=dict(sorted(count.items())), total=sum(count.values()),
                area_um2={k: round(v, 3) for k, v in sorted(area.items())})


def die_area(def_path: Path) -> dict | None:
    text = def_path.read_text()
    units = re.search(r"UNITS DISTANCE MICRONS (\d+)", text)
    die = re.search(r"DIEAREA \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)", text)
    if not units or not die:
        return None
    u = int(units[1])
    w, h = (int(die[3]) - int(die[1])) / u, (int(die[4]) - int(die[2])) / u
    return dict(width_um=w, height_um=h, area_um2=round(w * h, 3))


def timing(pdk: dict) -> dict:
    matrix = pdk["details"].get("timing_matrix") or {}
    acc = (pdk["statuses"].get("timing_accuracy") or {}).get("by_spef") or {}
    parity = {}
    for src, v in acc.items():
        parity[src] = {c: dict(endpoints_ps=None if x["endpoints"].get("max_difference_ns") is None
                               else round(x["endpoints"]["max_difference_ns"] * 1e3, 4),
                               pulse_ps=round((x["pulse"].get("max_difference_ns") or 0) * 1e3, 4),
                               drv_equal=x["drv"]["status"] == "passed")
                       for c, x in (v.get("corners") or {}).items()}
        if not v.get("corners"):
            parity[src] = dict(status=v.get("status"), reason=short(v.get("reason")))
    return dict(worst=matrix, parity=parity)


def closure(pdk: dict) -> dict:
    h = pdk["statuses"].get("hold_closure") or {}
    rows = h.get("iterations") or []
    hold = sum((r.get("hold_eco") or {}).get("cells", 0) for r in rows if isinstance(r.get("hold_eco"), dict))
    diodes = sum(sum(n.get("diodes", 0) for n in (r.get("antenna_eco") or {}).get("nets") or []) for r in rows)
    resized = sum((r.get("size_eco") or {}).get("resized") or 0 for r in rows)
    buffers = sum((r.get("size_eco") or {}).get("buffers") or 0 for r in rows)
    return dict(status=h.get("status"), iterations=len(rows), max_iterations=h.get("max_iterations"),
                target_margin_ns=h.get("target_margin_ns"), sizing=h.get("sizing"),
                hold_cells_planned=hold, antenna_diodes_planned=diodes, cells_resized=resized,
                buffers_planned=buffers, final_iteration=h.get("final_iteration"), selected=h.get("selected"),
                trace=[dict(iteration=r["iteration"], worst=r["worst"], drv=(r.get("drv") or {}).get("violations"),
                            hold_eco=r.get("hold_eco") if not isinstance(r.get("hold_eco"), str) else short(r["hold_eco"], 300),
                            size_eco=r.get("size_eco"), antenna=r.get("antenna"), stopped=r.get("stopped"))
                       for r in rows])


def row(summary: dict, pdk: dict, run: Path, sizes: dict, policy: dict) -> dict:
    st = pdk["statuses"]
    gating = summary["gating"]
    binding = json.loads(Path(pdk["binding"]["path"]).read_text()) if pdk.get("binding") else None
    arts = (binding or {}).get("artifacts") or {}
    physical = set()
    glob_list = policy["pdks"][pdk["pdk"]]["post_synthesis_insertable"]["physical_only"]
    import fnmatch
    physical = {m for m in sizes if any(fnmatch.fnmatchcase(m, g) for g in glob_list)}
    cells = area = None
    if "netlist.powered" in arts and Path(arts["netlist.powered"]["path"]).is_file():
        cells = roles(Path(arts["netlist.powered"]["path"]), sizes, physical)
    if "def" in arts and Path(arts["def"]["path"]).is_file():
        area = die_area(Path(arts["def"]["path"]))
    signoff = {k: dict(status=st[k]["status"], count=st[k].get("count")) for k in
               ("gds_xor", "drc_magic", "drc_klayout", "antenna_deck", "lvs_netgen", "lvs_klayout", "density")
               if k in st}
    controls = {k: v.get("detected") for k, v in ((st.get("controls") or {}).get("controls") or {}).items()}
    steps = [dict(step=s["step"], wall_seconds=s["wall_seconds"], peak_rss_kib=s["peak_rss_kib"])
             for s in summary["resources"]["steps"] if s["pdk"] == pdk["pdk"]]
    failed = [k for k in gating if (st.get(k) or {}).get("status") == "failed"]
    unverified = [k for k in gating if (st.get(k) or {}).get("status") in ("unverified", "missing", None)]
    na = [k for k in gating if (st.get(k) or {}).get("status") == "not_applicable"]
    return dict(
        design=summary["design"]["name"], pdk=pdk["pdk"], recipe=summary["design"]["recipe"]["physical"],
        run=run.name, gate_status=pdk["status"], failed=failed, unverified=unverified, not_applicable=na,
        statuses={k: dict(status=(st.get(k) or {}).get("status"), reason=short((st.get(k) or {}).get("reason")))
                  for k in gating + summary["reported"]},
        timing=timing(pdk), closure=closure(pdk), cells=cells, die=area, signoff=signoff, controls=controls,
        determinism=dict(status=(st.get("determinism") or {}).get("status"),
                         artifacts=len(((st.get("determinism") or {}).get("artifacts") or {}))),
        artifact_binding=dict(status=(st.get("artifact_binding") or {}).get("status"),
                              artifacts=(pdk.get("binding") or {}).get("artifacts")),
        mpw_async_unchecked=((((st.get("mpw_min_period") or {}).get("by_spef") or {}).get("native") or {})
                             .get("async_set_clear_pins_unchecked") or {}).get("count"),
        wall_seconds=pdk.get("wall_seconds"), steps=steps,
        peak_rss_kib=max((s["peak_rss_kib"] for s in steps if s["peak_rss_kib"]), default=None))


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--extra", type=Path, action="append", default=[],
                    help="JSON object merged into the output (e.g. a cross-run determinism record)")
    ap.add_argument("runs", type=Path, nargs="+")
    args = ap.parse_args(argv)
    profiles = {p["tag"]: p for p in json.loads((ROOT / "fixtures/pdk/local-routing.json").read_text())["pdks"]}
    sizes = {t: lef_sizes(Path(p["cell_lef"]["path"])) for t, p in profiles.items()}
    policy = json.loads((ROOT / "fixtures/flow/cell-policy.json").read_text())
    rows, runs = [], []
    for run in args.runs:
        summary = json.loads((run / "summary.json").read_text())
        runs.append(dict(run=run.name, design=summary["design"]["name"], status=summary["status"],
                         exit_code=summary["exit_code"], failed_items=summary["failed_items"],
                         unverified_items=summary["unverified_items"],
                         not_applicable_items=summary.get("not_applicable_items"),
                         global_statuses={k: v["status"] for k, v in summary["global_statuses"].items()},
                         revision={k: v for k, v in summary["revision"].items() if k != "rustroad_path"},
                         wall_seconds=summary["resources"]["wall_seconds"],
                         peak_rss_kib=summary["resources"]["peak_rss_kib"],
                         processes=summary["resources"]["processes"],
                         tools={k: v.get("actual_sha256") or v.get("sha256") for k, v in summary["tools"].items()}))
        for pdk in summary["pdks"]:
            rows.append(row(summary, pdk, run, sizes[pdk["pdk"]], policy))
    out = dict(schema=SCHEMA, generator="scripts/rtl2gds_evidence.py",
               scope=json.loads((args.runs[0] / "summary.json").read_text())["scope"],
               runs=runs, rows=rows,
               table={f"{r['design']}/{r['pdk']}": dict(status=r["gate_status"], failed=r["failed"],
                                                         unverified=r["unverified"]) for r in rows})
    for path in args.extra:
        out.update(json.loads(path.read_text()))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=1, sort_keys=False, default=str) + "\n")
    print(json.dumps(out["table"], indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
