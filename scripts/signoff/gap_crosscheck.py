#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Foundry-deck cross-check of the geometry checker's planted sub-minimum gap (W3).

usage: gap_crosscheck.py --summary FLOW/summary.json --gds REFERENCE_gf180.gds --out DIR
Plants the bump rectangle that scripts/check_routing_geometry.py built for
its GF180 planted 0.20 um Metal1 gap into the reference GDS (2000 DEF DBU/um
-> 0.001 um GDS), runs the gf180 KLayout DRC deck on the unplanted reference
and on the planted copy, and diffs the two report databases. The deck agrees
only if (1) the M1.2a item count increases, (2) a NEW M1.2a edge pair has one
edge on the bump's edge facing the target and the other on the target's facing
edge, separated by exactly the planted gap, and (3) the same predicate finds
nothing in the unplanted baseline (so a nearby pre-existing item cannot pass
it). Exit 0 iff the deck agrees.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import container  # noqa: E402
import parsers  # noqa: E402
import signoff  # noqa: E402


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--summary", type=Path, required=True)
    p.add_argument("--gds", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    summary = json.loads(args.summary.read_text())
    gf = next(x for x in summary["pdks"] if x["pdk"] == "gf180")
    gap = gf["geometry"]["negative_controls"]["planted_subminimum_gap"]
    if any(v % 2 for v in gap["bump"]):
        raise SystemExit("odd DEF coordinate cannot be represented at 0.001 um")
    rect = [v // 2 for v in gap["bump"]]
    container.run(["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_plant.py", "-rd", "gds=/in/design.gds",
                   "-rd", "out=/out/gf180_gap.gds", "-rd", "report=/out/gf180_gap.json", "-rd", "kind=rect",
                   "-rd", "metal=34/0", "-rd", "boundary=0/0", "-rd", "rect=" + ",".join(map(str, rect))],
                  out_dir=out, label=container.next_label(out, "plant_gap"),
                  ro=[(str(HERE), "/scripts"), (str(args.gds.resolve()), "/in/design.gds")],
                  memory="2g", cpus=1, threads=1, timeout=300)
    ns = argparse.Namespace(gds=out / "gf180_gap.gds", top="lane_smoke", pdk="gf180", out=out / "drc", netlist=None,
                            against=None, against_top=None, manifest=signoff.manifest_mod.DEFAULT_MANIFEST,
                            threads=4, memory="4g", timeout=900, plant=None)
    s = signoff.run_checks(ns, ["drc-klayout"])
    verdict = json.loads(Path(s["verdicts"]["drc-klayout"]).read_text())
    base_ns = argparse.Namespace(**dict(vars(ns), gds=args.gds.resolve(), out=out / "drc_baseline"))
    b = signoff.run_checks(base_ns, ["drc-klayout"])
    base_verdict = json.loads(Path(b["verdicts"]["drc-klayout"]).read_text())
    rule = "M1.2a"
    gap_um = gap["gap_dbu"] / 2000.0
    bump_um = [v / 2000.0 for v in gap["bump"]]
    target_um = [v / 2000.0 for v in gap["target"]]
    problems = []
    try:
        planted = parsers.lyrdb_items(verdict["reports"]["drc.lyrdb"], rule)
        base = parsers.lyrdb_items(base_verdict["reports"]["drc.lyrdb"], rule)
        base_values = {v for item in base for v in item["values"]}
        new_items = [item for item in planted if not set(item["values"]) & base_values]
        hits = parsers.planted_gap_hits(new_items, bump_um, target_um, gap_um)
        baseline_hits = parsers.planted_gap_hits(base, bump_um, target_um, gap_um)
    except (parsers.ParseError, KeyError) as error:
        problems.append(f"report evidence missing: {error}")
        planted, base, new_items, hits, baseline_hits = [], [], [], [], []
    if verdict["status"] == "unverified" or base_verdict["status"] == "unverified":
        problems.append("a DRC run is unverified")
    if len(planted) <= len(base):
        problems.append(f"{rule} count did not increase ({len(base)} -> {len(planted)})")
    if not hits:
        problems.append("no new edge pair measures the planted gap between bump and target")
    if baseline_hits:
        problems.append(f"predicate also matches the unplanted baseline {baseline_hits[:2]} (not discriminating)")
    agrees = not problems
    result = dict(gap=gap, gds_bump_um=bump_um, gds_target_um=target_um, gap_um=gap_um, rule=rule,
                  baseline_count=len(base), planted_count=len(planted),
                  deck_counts=(verdict.get("counts") or {}).get("by_rule"),
                  baseline_counts=(base_verdict.get("counts") or {}).get("by_rule"),
                  new_items=[v for item in new_items for v in item["values"]],
                  deck_items_at_planted_gap=hits, baseline_items_matching_predicate=baseline_hits,
                  problems=problems, agrees=agrees,
                  verdicts=dict(planted=s["verdicts"]["drc-klayout"], baseline=b["verdicts"]["drc-klayout"]))
    (out / "gap_crosscheck.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(dict(agrees=agrees, items=hits, counts=[len(base), len(planted)], problems=problems)))
    return 0 if agrees else 1


if __name__ == "__main__":
    sys.exit(main())
