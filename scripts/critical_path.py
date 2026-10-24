#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Critical-path report of finished rtl2gds gate runs (report only, no verdict).

For every PDK row of each run directory (`scripts/rtl2gds.py --run DIR`):

* pre-layout: RustSTA on a netlist without parasitics and with the ideal
  clock (the rendered signoff SDC minus `set_propagated_clock`), at ss and
  tt, for (a) the synthesized mapped netlist and (b) the final closure
  netlist (after sizing/buffering/hold ECOs, before CTS). The wire model is
  whatever RustSTA selects without SPEF and is reported: `ideal` (no
  wire-load table, GF180) or `wireload_lumped` (the Liberty default wire
  load, SKY130) -- the latter is an upper bound of the ideal-wire delay;
* post-route: the gate's own RustSTA workspace on the final native SPEF
  (propagated clock through the RustROAD clock tree), at ss and tt.

For each: worst setup slack, the implied minimum period
`period - worst setup slack` (the clock period at which the worst path
would have zero slack with the same uncertainty, latency and setup times),
and the startpoint/endpoint/arrival of the worst path. `setup_feasible_at_
period` is false when the pre-layout worst slack of the synthesized netlist
is negative: no placement or routing can then meet the period.

Usage: critical_path.py --work DIR --out FILE.json RUN [RUN ...]
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from hold_closure import CORNERS  # noqa: E402

TIMEOUT_S = 600


def run(argv: list, timeout: int = TIMEOUT_S) -> dict:
    r = subprocess.run([str(a) for a in argv], capture_output=True, text=True, timeout=timeout)
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"{argv[0]} {argv[3:6]}: exit {r.returncode}: {r.stderr[-400:]}") from e


def worst_path(ruststa: Path, ws: Path, corner: str, rev: str = "HEAD") -> dict:
    d = run([ruststa, "-w", ws, "--format", "json", "report", "--rev", rev, "--section", "paths", "--kind", "setup",
             "--corner", corner, "--limit", "1", "-q"])
    rows = ((d.get("sections") or {}).get("paths") or {}).get("rows") or []
    if not rows:
        return {}
    r = rows[0]
    return dict(startpoint=r.get("startpoint"), endpoint=r.get("endpoint"), slack_ns=r.get("slack_ns"),
                arrival_ns=r.get("arrival_ns"), required_ns=r.get("required_ns"))


def period_of(sdc: str) -> float:
    m = re.search(r"create_clock\b[^\n]*-period\s+([0-9.]+)", sdc)
    if not m:
        raise RuntimeError("no create_clock -period in the SDC")
    return float(m[1])


def pre_layout(tools: dict, libs: dict, netlist: Path, sdc: Path, work: Path) -> dict:
    ws = work / "ws"
    run([tools["rusteda"], "-w", ws, "--format", "json", "init"])
    argv = [tools["rusteda"], "-w", ws, "--format", "json", "import", "--base-rev", "rev0", "-q"]
    for c in ("tt", "ss"):
        argv += ["--lib", libs[c], "--corner", c]
    a = run(argv + ["--netlist", netlist, "--sdc", sdc])
    if not a.get("ok"):
        raise RuntimeError(f"import failed: {a.get('error')}")
    r = run([tools["ruststa"], "-w", ws, "--format", "json", "run", "--base-rev", a["new_rev"], "--backend", "cpu",
             "--precision", "f64", "--max-paths", "16", "-q"])
    if not r.get("ok"):
        raise RuntimeError(f"ruststa run failed: {r.get('error')}")
    out = dict(wire_model=r["scope"].get("wire_model"), clock_model=r["scope"].get("clock_model"))
    for c in ("ss", "tt"):
        out[c] = worst_path(tools["ruststa"], ws, c, r["new_rev"])
    return out


def report(run_dir: Path, work: Path) -> list:
    summary = json.loads((run_dir / "summary.json").read_text())
    tools = {k: Path(summary["tools"][k]["path"]) for k in ("rusteda", "ruststa")}
    tools = {k: v if v.is_absolute() else ROOT / v for k, v in tools.items()}
    profiles = {p["tag"]: p for p in json.loads((ROOT / "fixtures/pdk/local-routing.json").read_text())["pdks"]}
    rows = []
    for pdk in summary["pdks"]:
        tag = pdk["pdk"]
        tt = Path(profiles[tag]["liberty"]["path"])
        libs = {c: Path(str(tt).replace(CORNERS[tag][0][1], name)) for c, name, _ in CORNERS[tag]}
        sdc_text = (run_dir / f"{tag}_postroute.sdc").read_text()
        period = period_of(sdc_text)
        w = work / run_dir.name / tag
        w.mkdir(parents=True, exist_ok=False)
        ideal = w / "ideal_clock.sdc"
        ideal.write_text("".join(x for x in sdc_text.splitlines(keepends=True) if "set_propagated_clock" not in x))
        row = dict(design=summary["design"]["name"], pdk=tag, run=str(run_dir), period_ns=period,
                   final_iteration=None)
        hc = pdk["statuses"].get("hold_closure") or {}
        closure = hc.get("iterations") or []
        # The loop's selected layout (best complete one), else the last.
        final_it = hc.get("final_iteration", len(closure) - 1)
        row["final_iteration"] = final_it
        final_mapped = None
        for cand in (f"{tag}_i{final_it}_mapped_sized.v", f"{tag}_i{final_it}_mapped.v", f"{tag}_i{final_it}_mapped_ant.v",
                     f"{tag}_i{final_it}_mapped_hold_sized.v"):
            if (run_dir / cand).is_file():
                final_mapped = run_dir / cand
        for label, netlist in (("prelayout_synth", run_dir / f"{tag}_mapped.v"), ("prelayout_final", final_mapped)):
            if netlist is None:
                row[label] = dict(status="not_applicable", reason="no closure ECO changed the netlist")
                continue
            sub = w / label
            sub.mkdir()
            try:
                row[label] = dict(netlist=str(netlist), **pre_layout(tools, libs, netlist, ideal, sub))
            except (RuntimeError, OSError, subprocess.TimeoutExpired) as e:
                row[label] = dict(status="unverified", reason=str(e)[-400:])
        ws = run_dir / f"{tag}_final_native_ws"
        if ws.is_dir():
            row["postroute"] = dict(spef="native", clock_model="propagated",
                                    **{c: worst_path(tools["ruststa"], ws, c) for c in ("ss", "tt")})
        else:
            row["postroute"] = dict(status="unverified", reason=f"{ws.name} absent")
        for label in ("prelayout_synth", "prelayout_final", "postroute"):
            for c in ("ss", "tt"):
                x = (row.get(label) or {}).get(c)
                if isinstance(x, dict) and x.get("slack_ns") is not None:
                    x["min_period_ns"] = round(period - x["slack_ns"], 4)
        pre = ((row.get("prelayout_synth") or {}).get("ss") or {}).get("slack_ns")
        row["setup_feasible_at_period"] = None if pre is None else pre >= 0
        rows.append(row)
    return rows


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--work", type=Path, required=True, help="new scratch directory (STA workspaces)")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("runs", type=Path, nargs="+")
    args = ap.parse_args()
    args.work.mkdir(parents=True, exist_ok=False)
    rows = []
    for r in args.runs:
        rows += report(r.resolve(), args.work.resolve())
    args.out.write_text(json.dumps(dict(schema="rustroad.critical-path.v1", rows=rows), indent=1) + "\n")
    for x in rows:
        def f(label, c):
            v = (x.get(label) or {}).get(c) or {}
            return "-" if v.get("slack_ns") is None else f"{v['slack_ns']:+.3f}/{v['min_period_ns']:.2f}"
        print(x["design"], x["pdk"], "pre(synth)", f("prelayout_synth", "ss"), (x.get("prelayout_synth") or {}).get("wire_model"),
              "pre(final)", f("prelayout_final", "ss"), "post", f("postroute", "ss"), "feasible", x["setup_feasible_at_period"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
