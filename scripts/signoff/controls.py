#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Planted positive/negative controls for the signoff harness.

usage: controls.py --pdk sky130|gf180 --gds REFERENCE.gds --top CELL --netlist POWERED.v
                   --def ROUTED.def --baseline SIGNOFF_DIR --out DIR [--manifest M]

Plants each defect into a copy of the reference GDS (KLayout macro in the
pinned image), runs the relevant signoff checks and records whether the
harness detected it, by comparing rule counts against the unmodified
baseline (``SIGNOFF_DIR/signoff.json`` from ``signoff.py all``). Controls:
spacing, offgrid, short, missing_via, antenna, wrong_top, missing_report,
no_boundary (sky130), dbu_half (gf180); positive controls: the clean mini
layout must pass both DRC oracles, the reference must XOR-identical to itself
re-streamed, and a planted variant must XOR-differ. Exit 0 iff every control
behaved as expected.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import container  # noqa: E402
import manifest as manifest_mod  # noqa: E402
import signoff  # noqa: E402

LAYERS = {
    "sky130": dict(metal="68/20", pin_layer="70/16", pin_text="70/5", draw_layer="70/20", boundary="235/4",
                   spacing_um="0.10", spacing_rule_kl="m1.2", spacing_rule_magic="met1.2",
                   short_pins="a[1],clk", antenna_pin="a[1]", antenna_length_um="600", via="M1M2_PR",
                   via_net="a[0]"),
    "gf180": dict(metal="34/0", pin_layer="42/10", pin_text="42/10", draw_layer="42/0", boundary="0/0",
                  spacing_um="0.12", spacing_rule_kl="M1.2a", spacing_rule_magic="M1.2a",
                  short_pins="a[1],clk", antenna_pin="a[1]", antenna_length_um="2000", via="Via1_HV",
                  via_net="a[0]"),
}


def first_via(def_text: str, net: str, via: str) -> tuple[int, int]:
    nets = def_text.split("\nNETS", 1)[1]
    block = re.search(r"^-\s+" + re.escape(net) + r"\s.*?;\s*$", nets, re.M | re.S)
    if block is None:
        raise SystemExit(f"net {net} not in DEF")
    for m in re.finditer(r"\(\s*(-?\d+)\s+(-?\d+)(?:\s+-?\d+)?\s*\)\s+(\w+)\s*$", block[0], re.M):
        if m[3] == via:
            return int(m[1]), int(m[2])
    raise SystemExit(f"no {via} on net {net}")


def plant(args, kind: str, work: Path, **extra) -> tuple[Path, dict]:
    lay = LAYERS[args.pdk]
    out = work / f"{kind}.gds"
    report = work / f"{kind}.plant.json"
    rd = dict(gds="/in/design.gds", out=f"/out/{out.name}", report=f"/out/{report.name}", kind=kind,
              metal=lay["metal"], pin_layer=lay["pin_layer"], pin_text=lay["pin_text"],
              draw_layer=lay["draw_layer"], boundary=lay["boundary"], spacing_um=lay["spacing_um"],
              pins=lay["short_pins"], antenna_pin=lay["antenna_pin"],
              antenna_length_um=lay["antenna_length_um"], **extra)
    cmd = ["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_plant.py"]
    for k, v in rd.items():
        cmd += ["-rd", f"{k}={v}"]
    r = container.run(cmd, out_dir=work, label=container.next_label(work, f"plant_{kind}"),
                      ro=[(str(HERE), "/scripts"), (str(args.gds.resolve()), "/in/design.gds")],
                      memory="2g", cpus=1, threads=1, timeout=300)
    if not out.is_file() or not report.is_file():
        raise SystemExit(f"planting {kind} failed: {container.read_capture(r['stdout'])[-800:]}")
    return out, json.loads(report.read_text())


def run(args, gds: Path, checks, out: Path, top: str | None = None, plant_flag=None, against=None) -> dict:
    ns = argparse.Namespace(gds=gds, top=top or args.top, pdk=args.pdk, out=out, netlist=args.netlist,
                            against=against, against_top=args.top if against else None,
                            manifest=args.manifest, threads=args.threads, memory="4g", timeout=900,
                            plant=plant_flag)
    summary = signoff.run_checks(ns, checks)
    (out / "signoff.json").write_text(json.dumps(summary, indent=2) + "\n")
    verdicts = {c: json.loads(Path(p).read_text()) for c, p in summary["verdicts"].items()}
    return dict(summary=summary, verdicts=verdicts)


def rule(verdict: dict, name: str) -> int:
    return ((verdict.get("counts") or {}).get("by_rule") or {}).get(name, 0)


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--pdk", choices=sorted(LAYERS), required=True)
    p.add_argument("--gds", type=Path, required=True)
    p.add_argument("--top", required=True)
    p.add_argument("--netlist", type=Path, required=True)
    p.add_argument("--def", dest="def_path", type=Path, required=True)
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--manifest", type=Path, default=manifest_mod.DEFAULT_MANIFEST)
    p.add_argument("--threads", type=int, default=4)
    args = p.parse_args(argv)
    args.gds, args.netlist, args.manifest = args.gds.resolve(), args.netlist.resolve(), args.manifest.resolve()
    work = args.out.resolve()
    work.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    base = json.loads((args.baseline / "signoff.json").read_text())
    base_v = {c: json.loads(Path(v).read_text()) for c, v in base["verdicts"].items()}
    lay = LAYERS[args.pdk]
    results = []

    def record(name, expected, detected, evidence, kind="negative"):
        results.append(dict(control=name, kind=kind, expected=expected, detected=bool(detected), evidence=evidence))
        print(json.dumps(dict(control=name, detected=bool(detected))), flush=True)

    # Negative: sub-minimum metal1 spacing.
    gds, info = plant(args, "spacing", work)
    r = run(args, gds, ["drc-klayout", "drc-magic", "xor"], work / "spacing", against=args.gds)
    kl, mg, xo = r["verdicts"]["drc-klayout"], r["verdicts"]["drc-magic"], r["verdicts"]["xor"]
    d_kl = rule(kl, lay["spacing_rule_kl"]) - rule(base_v["drc-klayout"], lay["spacing_rule_kl"])
    d_mg = rule(mg, lay["spacing_rule_magic"]) - rule(base_v["drc-magic"], lay["spacing_rule_magic"])
    record("spacing", f"{lay['spacing_rule_kl']} (KLayout) and {lay['spacing_rule_magic']} (Magic) increase",
           kl["status"] == "failed" and d_kl > 0 and mg["status"] == "failed" and d_mg > 0,
           dict(plant=info, klayout_delta=d_kl, magic_box_delta=d_mg, klayout=kl["counts"], magic=mg["counts"]))
    record("xor_negative", "XOR reports differences for the planted variant",
           xo["status"] == "failed", dict(xor=xo["result"].get("layer_differences"), status=xo["status"]))
    # Negative: off-grid edge (KLayout only; Magic snaps off-grid input).
    gds, info = plant(args, "offgrid", work)
    r = run(args, gds, ["drc-klayout", "drc-magic"], work / "offgrid")
    kl, mg = r["verdicts"]["drc-klayout"], r["verdicts"]["drc-magic"]
    off = {k: v for k, v in (kl["counts"] or {}).get("by_rule", {}).items() if "offgrid" in k.lower()}
    record("offgrid", "KLayout *_OFFGRID item", kl["status"] == "failed" and sum(off.values()) > 0,
           dict(plant=info, offgrid_rules=off, magic_status=mg["status"], magic_counts=mg["counts"],
                note="Magic snaps off-grid GDS input to its grid; offgrid detection relies on KLayout"))
    # Negative: metal3 short between two top-level pins.
    gds, info = plant(args, "short", work)
    r = run(args, gds, ["lvs-netgen", "lvs-klayout"], work / "short")
    ng, kv = r["verdicts"]["lvs-netgen"], r["verdicts"]["lvs-klayout"]
    record("short", "both LVS tools report a mismatch", ng["status"] == "failed" and kv["status"] == "failed",
           dict(plant=info, netgen=ng["failures"][:2], klayout=kv["failures"][:2]))
    # Negative: one signal via removed (open).
    text = args.def_path.read_text()
    units = int(re.search(r"UNITS DISTANCE MICRONS (\d+)", text)[1])
    x, y = first_via(text, lay["via_net"], lay["via"])
    factor = units // 1000
    gds, info = plant(args, "missing_via", work, via_cell="VIA_" + lay["via"], via_x=x // factor, via_y=y // factor)
    info.update(net=lay["via_net"], def_point=[x, y])
    r = run(args, gds, ["lvs-netgen", "lvs-klayout"], work / "missing_via")
    ng, kv = r["verdicts"]["lvs-netgen"], r["verdicts"]["lvs-klayout"]
    record("missing_via", "both LVS tools report a mismatch", ng["status"] == "failed" and kv["status"] == "failed",
           dict(plant=info, netgen=ng["failures"][:2], klayout=kv["failures"][:2]))
    # Negative: long gate-only metal3 wire on an input pin.
    gds, info = plant(args, "antenna", work)
    r = run(args, gds, ["antenna"], work / "antenna")
    an = r["verdicts"]["antenna"]
    record("antenna", "primary antenna oracle reports violations",
           an["status"] == "failed" and (an["counts"] or {}).get("total", 0) > 0,
           dict(plant=info, counts=an["counts"], secondary=an["result"].get("secondary")))
    # Negative: wrong top-cell name (Magic would silently create an empty cell).
    checks = ["drc-magic", "drc-klayout", "antenna", "density", "lvs-netgen", "lvs-klayout"]
    r = run(args, args.gds, checks, work / "wrong_top", top="wrong_top")
    statuses = {c: v["status"] for c, v in r["verdicts"].items()}
    record("wrong_top", "no check passes", all(s != "passed" for s in statuses.values()),
           dict(statuses=statuses, magic=r["verdicts"]["drc-magic"]["failures"][:2]))
    # Negative: missing report (relative KLayout report path writes nothing, exit 0).
    r = run(args, args.gds, ["drc-klayout"], work / "missing_report", plant_flag="relative-report")
    kl = r["verdicts"]["drc-klayout"]
    record("missing_report", "drc-klayout unverified because the report is absent",
           kl["status"] == "unverified" and any("not written" in u for u in kl["unverified_reasons"]),
           dict(status=kl["status"], reasons=kl["unverified_reasons"], exit_codes=[x["exit_code"] for x in kl["runs"]]))
    if args.pdk == "sky130":
        gds, info = plant(args, "no_boundary", work)
        r = run(args, gds, ["density", "drc-klayout"], work / "no_boundary")
        de, kl = r["verdicts"]["density"], r["verdicts"]["drc-klayout"]
        dens = de["result"]["decks"][0]
        record("no_boundary", "density non-finite -> not passed; preflight boundary assertion fails",
               de["status"] != "passed" and "non-finite" in (dens.get("reason") or "") and
               not de["preflight"]["assertions"]["boundary_present"] and kl["status"] == "failed",
               dict(plant=info, density=dens, density_status=de["status"], drc_klayout_failures=kl["failures"][:2]))
    else:
        gds, info = plant(args, "dbu_half", work)
        r = run(args, gds, ["drc-klayout"], work / "dbu_half")
        kl = r["verdicts"]["drc-klayout"]
        record("dbu_half", "deck DBU rule item and dbu preflight assertion",
               kl["status"] == "failed" and rule(kl, "DBU") > 0 and not kl["preflight"]["assertions"]["dbu_expected"],
               dict(plant=info, dbu_items=rule(kl, "DBU"), failures=kl["failures"][:3]))
    # Positive: mini layout must be DRC clean in both oracles.
    mini = work / "mini.gds"
    m = manifest_mod.load(args.manifest)
    rm = container.run(["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_mini.py", "-rd", f"pdk={args.pdk}",
                        "-rd", f"libgds={manifest_mod.pdk_container_path(m, args.pdk, 'cell_gds')}",
                        "-rd", f"out=/out/{mini.name}"], out_dir=work, label=container.next_label(work, "mini"),
                       ro=[(str(HERE), "/scripts")], memory="2g", cpus=1, threads=1, timeout=300)
    if not mini.is_file():
        raise SystemExit("mini layout build failed: " + container.read_capture(rm["stdout"])[-500:])
    r = run(args, mini, ["drc-magic", "drc-klayout"], work / "mini", top="mini_top")
    statuses = {c: v["status"] for c, v in r["verdicts"].items()}
    record("mini_drc_clean", "both DRC oracles pass on the clean mini layout", all(s == "passed" for s in statuses.values()),
           dict(statuses=statuses, counts={c: v["counts"] for c, v in r["verdicts"].items()}), kind="positive")
    # Positive: reference XOR-identical to an independent re-streamout of the same DEF.
    import def2gds_reference
    again = work / "restream.gds"
    rr = def2gds_reference.stream(args.def_path.resolve(), args.pdk, again, args.manifest)
    r = run(args, args.gds, ["xor"], work / "xor_positive", against=again)
    xo = r["verdicts"]["xor"]
    record("xor_positive", "re-streamed reference is XOR-identical", xo["status"] == "passed",
           dict(status=xo["status"], restream_sha256=rr.get("output", {}).get("sha256")), kind="positive")
    record("lvs_positive", "baseline LVS passes in both tools",
           base_v["lvs-netgen"]["status"] == "passed" and base_v["lvs-klayout"]["status"] == "passed",
           dict(netgen=base_v["lvs-netgen"]["status"], klayout=base_v["lvs-klayout"]["status"]), kind="positive")
    ok = all(x["detected"] for x in results)
    rss = []
    for receipt in work.rglob("receipt.json"):
        value = json.loads(receipt.read_text()).get("peak_rss_kib")
        if value:
            rss.append(value)
    out = dict(schema="rustroad.signoff-controls.v1", pdk=args.pdk, reference=signoff.fingerprint(args.gds),
               baseline=str(args.baseline.resolve() / "signoff.json"), all_expected=ok, controls=results,
               resources=dict(wall_seconds=round(time.monotonic() - started, 3), tool_runs=len(rss),
                              max_peak_rss_kib=max(rss) if rss else None))
    (work / "controls.json").write_text(json.dumps(out, indent=2) + "\n")
    print(json.dumps(dict(pdk=args.pdk, all_expected=ok, controls={x["control"]: x["detected"] for x in results},
                          report=str(work / "controls.json"))))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
