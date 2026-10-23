#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Foundry-deck cross-check of the geometry checker's planted sub-minimum gap (W3).

usage: gap_crosscheck.py --summary FLOW/summary.json --gds REFERENCE_gf180.gds --out DIR
Plants the bump rectangle that scripts/check_routing_geometry.py built for
its GF180 planted 0.20 um Metal1 gap into the reference GDS (2000 DEF DBU/um
-> 0.001 um GDS), runs the gf180 KLayout DRC deck and reports whether the deck
also flags a Metal1 spacing item at the planted location. Exit 0 iff it does.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import container  # noqa: E402
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
    x1, y1, x2, y2 = [v * 0.001 for v in rect]
    hits = []
    for item in verdict["result"].get("samples", []):
        if item["rule"] != "M1.2a" or not item["value"]:
            continue
        coords = [float(c) for c in item["value"][0].replace("edge-pair:", "").replace("(", " ").replace(")", " ")
                  .replace(";", " ").replace("|", " ").replace(",", " ").split()]
        xs, ys = coords[0::2], coords[1::2]
        if min(xs) <= x2 + 0.25 and max(xs) >= x1 - 0.25 and min(ys) <= y2 + 0.25 and max(ys) >= y1 - 0.25:
            hits.append(item["value"][0])
    result = dict(gap=gap, gds_rect_um=[x1, y1, x2, y2], deck_counts=verdict["counts"]["by_rule"],
                  deck_items_at_planted_gap=hits, agrees=bool(hits))
    (out / "gap_crosscheck.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(dict(agrees=bool(hits), items=hits, counts=verdict["counts"]["by_rule"])))
    return 0 if hits else 1


if __name__ == "__main__":
    sys.exit(main())
