#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Collect a compact, path-sanitised signoff evidence summary for docs/evidence.

usage: evidence.py --run DIR [--out docs/evidence/signoff-results.json]
DIR holds baseline/{signoff_sky130,signoff_gf180,controls_sky130,controls_gf180,ref}
and w4/{flow-frozen,flow-policy,flow-nativepath} as produced by the commands in
docs/signoff.md. Only verdict facts, counts, hashes and resources are kept.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load(path: Path):
    return json.loads(path.read_text())


def signoff_facts(d: Path) -> dict:
    s = load(d / "signoff.json")
    out = dict(status=s["status"], statuses=s["statuses"], density_status=s["density_status"],
               drc_status=s["drc_status"], lvs_status=s["lvs_status"], antenna_status=s["antenna_status"],
               counts=s["counts"], by_rule=s["by_rule"], gds_sha256=s["gds"]["sha256"],
               identity=s["identity"]["status"], manifest_sha256=s["identity"]["manifest_sha256"],
               wall_seconds=s["resources"]["wall_seconds"], peak_rss_kib_per_check=s["resources"]["peak_rss_kib_per_check"])
    for check, path in s["verdicts"].items():
        v = load(Path(path))
        out.setdefault("by_cell", {})[check] = (v.get("counts") or {}).get("by_cell")
        out.setdefault("tool_runs", {})[check] = [dict(label=r["label"], wall_seconds=r["wall_seconds"],
                                                       peak_rss_kib=r["peak_rss_kib"]) for r in v["runs"]]
    return out


def flow_facts(d: Path) -> dict:
    s = load(d / "summary.json")
    pdks = {}
    for p in s["pdks"]:
        entry = dict(status=p["status"])
        if p["status"] == "passed":
            entry.update(synth=p.get("synth_report"), timing=p["timing"]["metrics"],
                         unanalyzed_arcs=p["timing"]["completeness"].get("unanalyzed_arcs"),
                         opensta_max_difference_ns=p["opensta"]["max_difference_ns"],
                         opensta_status=p["opensta"]["status"], skew_ns=p["opensta"]["clock_skew_rise_fall_ns"],
                         cec=p["cec"]["status"] if isinstance(p["cec"], dict) else p["cec"],
                         native_cec=p["native_cec"], simulation=p["simulation"]["status"],
                         geometry={k: p["geometry"][k] for k in ("routed_nets", "terminals", "wires", "vias")},
                         planted_gap=p["geometry"]["negative_controls"].get("planted_subminimum_gap"),
                         artifacts={k: v["sha256"] for k, v in p["artifacts"].items()},
                         signoff_statuses={k: v["status"] for k, v in p["signoff_statuses"].items()},
                         signoff_counts=p["signoff"].get("counts"), signoff_by_rule=p["signoff"].get("by_rule"),
                         signoff_evidence_gds=p["signoff"].get("evidence_gds"),
                         cell_policy={k: c["status"] for k, c in p["cell_policy"].items()})
        else:
            entry["reason"] = p.get("reason")
        pdks[p["pdk"]] = entry
    return dict(status=s["status"], postroute_timing_status=s["postroute_timing_status"],
                functional_status=s["functional_status"], producer=s["producer"],
                blocking=[f"{b['pdk']}:{b['item']}={b['status']}" for b in s["blocking"]],
                wall_seconds=round(s["wall_seconds"], 3), peak_rss_kib=s["peak_rss_kib"], pdks=pdks)


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--run", type=Path, required=True)
    p.add_argument("--out", type=Path, default=ROOT / "docs/evidence/signoff-results.json")
    args = p.parse_args(argv)
    run = args.run.resolve()
    out = dict(schema="rustroad.signoff-evidence.v1",
               manifest=load(ROOT / "fixtures/flow/signoff-pinned.json")["images"],
               baseline={t: signoff_facts(run / f"baseline/signoff_{t}") for t in ("sky130", "gf180")},
               reference_streamout={t: {k: load(run / f"baseline/ref/{t}.gds.json")[k] for k in ("status", "input", "output", "resources")}
                                    for t in ("sky130", "gf180")},
               controls={}, flows={})
    for t in ("sky130", "gf180"):
        c = load(run / f"baseline/controls_{t}/controls.json")
        out["controls"][t] = dict(all_expected=c["all_expected"], resources=c["resources"],
                                  results={x["control"]: dict(kind=x["kind"], expected=x["expected"], detected=x["detected"])
                                           for x in c["controls"]})
    for name in ("flow-frozen", "flow-policy", "flow-nativepath"):
        if (run / "w4" / name / "summary.json").is_file():
            out["flows"][name] = flow_facts(run / "w4" / name)
    text = json.dumps(out, indent=1, sort_keys=False) + "\n"
    text = text.replace(str(run), "<run>").replace(str(ROOT), "<repo>")
    args.out.write_text(text)
    print(json.dumps(dict(out=str(args.out), bytes=len(text))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
