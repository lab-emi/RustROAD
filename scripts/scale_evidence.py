#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Assemble docs/evidence/scale.json and the results table from scale_flow runs.

usage: scale_evidence.py --runs DIR [--runs DIR2 ...] [--design NAME ...] [--out docs/evidence/scale.json]
DIR/<design>/summary.json are scripts/scale_flow.py outputs. With several
--runs, a later directory's PDK entry replaces an earlier one (a rerun of one
PDK with `--pdk`); each row names its run directory. Absolute paths are
reduced to names; hashes, counts, verdicts and resources are kept.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
ORDER = ["counter", "uart_tx", "fifo", "alu", "regfile", "mult", "dense", "pipeline_track"]


def detail(reason: str | None) -> str:
    """Inline a signoff verdict's failures instead of its absolute path."""
    reason = reason or ""
    if reason.startswith("see "):
        path = Path(reason[4:].strip())
        try:
            v = json.loads(path.read_text())
            reason = "; ".join(v.get("failures", []) + v.get("unverified_reasons", [])) or reason
        except (OSError, json.JSONDecodeError):
            pass
        rel = path.relative_to(ROOT) if path.is_relative_to(ROOT) else path.name
        reason = f"{reason} [{rel}]"
    return reason.replace(str(ROOT) + "/", "")


def measured_host_peak(p: dict, run_dir: Path) -> tuple[int | None, list]:
    """Host peak RSS over the steps whose peak was measured.

    A step killed at its timeout has no measurement (the wrapper's rusage
    is not the tool's); older runs recorded wait4 there, so the receipt
    files' `timed_out` flag is honoured here too."""
    peaks, unmeasured = [], []
    for x in p.get("receipts", []):
        receipt = run_dir / p["pdk"] / "receipts" / x["label"] / "receipt.json"
        timed_out = False
        if receipt.is_file():
            timed_out = json.loads(receipt.read_text()).get("timed_out", False)
        if timed_out or x.get("peak_rss_kib") is None:
            unmeasured.append(x["label"])
        else:
            peaks.append(x["peak_rss_kib"])
    return (max(peaks) if peaks else None), unmeasured


def row(design: str, tag: str, p: dict, run: str, run_dir: Path | None = None) -> dict:
    f = p.get("facts", {})
    st = {k: v["status"] for k, v in p["statuses"].items()}
    counts = {k: v.get("count") for k, v in p["statuses"].items() if "count" in v}
    pl = f.get("placement_after_cts") or f.get("placement") or {}
    steps = p.get("resources", {}).get("steps", {})
    route = next((v for k, v in steps.items() if k.endswith("_a_route0")), {})
    reasons = {k: detail(v.get("reason")) for k, v in p["statuses"].items() if v["status"] != "passed"}
    memory = (f.get("route") or {}).get("route_memory") or {}
    route_kib = route.get("peak_rss_kib")
    cec = p["statuses"].get("cec", {})
    host_peak, unmeasured = (measured_host_peak(p, run_dir) if run_dir else
                             (p.get("resources", {}).get("host_peak_rss_kib"), []))
    return dict(
        design=design, pdk=tag, run=run, status=p.get("status"), statuses=st, counts=counts,
        non_passed_reasons={k: r[:600] for k, r in reasons.items()},
        synth_cells=(f.get("synth") or {}).get("cells"),
        placed_cells=(f.get("route") or {}).get("cells"),
        routed_nets=(f.get("route") or {}).get("routed_nets"),
        routing_segments=(f.get("route") or {}).get("routing_segments"),
        floorplan=f.get("floorplan"),
        hpwl_dbu=pl.get("hpwl_dbu"), baseline_hpwl_dbu=pl.get("baseline_hpwl_dbu"),
        route_stats=f.get("route_stats"),
        native_antenna=f.get("native_antenna"), antenna_eco_iterations=f.get("antenna_eco_iterations"),
        renamed_escaped_wires=f.get("renamed_escaped_wires"),
        route_wall_seconds=route.get("wall_seconds"), route_peak_rss_kib=route_kib,
        route_rss_source=(f.get("route") or {}).get("rss_source"),
        route_memory=memory or None,
        estimate_bounds_route_rss=(memory.get("estimate_mib", 0) * 1024 >= route_kib
                                   if memory and route_kib else None),
        cec_verdict=cec.get("verdict"), cec_control=(cec.get("control") or {}).get("status"),
        cec_control_tries=(cec.get("control") or {}).get("tries"),
        failed_step=p.get("failed_step"),
        wall_seconds=p.get("resources", {}).get("wall_seconds"),
        host_peak_rss_kib=host_peak, unmeasured_steps=unmeasured,
        container_peak_rss_kib=p.get("resources", {}).get("container_peak_rss_kib"),
        artifacts={k: v["sha256"] for k, v in p.get("artifacts", {}).items()},
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=Path, action="append", required=True)
    ap.add_argument("--design", action="append")
    ap.add_argument("--out", type=Path, default=ROOT / "docs/evidence/scale.json")
    args = ap.parse_args()
    rows, tools = [], {}
    for d in args.design or ORDER:
        merged = {}
        for runs in args.runs:
            path = runs / d / "summary.json"
            if not path.is_file():
                continue
            s = json.loads(path.read_text())
            used = dict(rustroad=s["rustroad"]["sha256"], rustsys=s["rustsys"]["sha256"])
            if tools and used != tools:
                raise SystemExit(f"{path}: binaries differ from the other runs: {used} != {tools}")
            tools = used
            for tag, p in s["pdks"].items():
                merged[tag] = (p, runs.name, runs / d)
        if not merged:
            rows.append(dict(design=d, status="missing"))
        for tag, (p, run, run_dir) in merged.items():
            rows.append(row(d, tag, p, run, run_dir))
    out = dict(schema="rustroad.scale-evidence.v1", tools=tools,
               scope=dict(level="block", density="reported separately, not gating",
                          ir_drop="unchecked", electromigration="unchecked", signal_integrity="unchecked",
                          ocv="unchecked", cppr="unchecked", timing="not part of this flow"),
               rows=rows)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(out, indent=2) + "\n")
    print("| design | PDK | cells (placed) | nets | XOR | Magic | KLayout | antenna | Netgen | KLayout LVS | density | CEC (control) | rerun | HPWL vs fixed order | route s / peak MiB / estimate MiB | flow s |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for r in rows:
        if r.get("status") == "missing":
            print(f"| {r['design']} | - | missing |")
            continue
        st, c = r["statuses"], r["counts"]

        def cell(k, n=None):
            s = st.get(k, "-")
            return f"{s} ({c[n]})" if n and c.get(n) is not None else s
        ratio = (f"{r['hpwl_dbu'] / r['baseline_hpwl_dbu']:.2f}x" if r.get("hpwl_dbu") and r.get("baseline_hpwl_dbu")
                 else "-")
        print(f"| {r['design']} | {r['pdk']} | {r['synth_cells']} ({r['placed_cells']}) | {r['routed_nets']} | "
              f"{cell('gds_xor')} | {cell('drc_magic', 'drc_magic')} | {cell('drc_klayout', 'drc_klayout')} | "
              f"{cell('antenna', 'antenna')} | {cell('lvs_netgen')} | {cell('lvs_klayout')} | "
              f"{cell('density', 'density')} | {cell('cec')} ({r['cec_control']}) | {cell('determinism')} | "
              f"{ratio} | {r['route_wall_seconds']} / {round((r['route_peak_rss_kib'] or 0) / 1024)} / "
              f"{(r['route_memory'] or {}).get('estimate_mib')} | {r['wall_seconds']} |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
