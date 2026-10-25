#!/usr/bin/env python3
"""Post-integration multi-corner timing baseline (reported, not closed).

For each PDK of a finished verify_routing.py run (--flow), on a copy of its
routed workspace:

* native SPEF: `rustroad extract` at min/nom/max, each written twice (byte
  determinism) and with the workspace tree unchanged;
* reference SPEF: OpenRCX (black box, pinned image) on the flow's routed DEF,
  which `export-def` now writes as merged collinear runs, at min/nom/max;
* RustSTA (pinned 0.4 binaries): one workspace per SPEF source holding the
  three analysis corners tt+nom, ss+max and ff+min (unmodified PDK Liberty,
  the flow's SDC), `run --require-wire-model spef_rc` with the
  OpenSTA-compatible pulse rules, per-corner endpoint slacks and the
  `hold_fix --target-margin 0.05` rows;
* OpenSTA 3.1.0 (black box) on the same files per corner: endpoint setup and
  hold slacks; parity with RustSTA per endpoint against the 1 ps gate.

Gates (owner decision f, never relaxed here): hold slack >= +0.05 ns at
tt+nom, ss+max and ff+min and setup slack >= 0 at ss+max, for both tools and
both SPEF sources. This is the starting point of hold closure: violations are
reported with their endpoints, nothing is fixed or hidden. Inputs are frozen
by sha256 in the output. Exit 0 when every gate is met, 3 when the run is
complete but a gate is violated (the expected baseline), 1 on a harness or
parity failure, which is never reported as a pass.

usage: timing_baseline.py --flow RUN --out NEW_DIR [--rustroad BIN]
                          [--sta-bin DIR] [--evidence docs/evidence/timing-baseline.json]
"""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import sys
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import compare_spef  # noqa: E402
from verify_logic import bounded_text, write_json  # noqa: E402
from verify_physical import PhysicalHarness, fingerprint, require  # noqa: E402

ROOT = HERE.parent
STA_BIN = Path("/mnt/data2/cgao/git/RustROAD/.work/sta-signoff/bin")
OPENSTA = Path("/home/cgao/.local/share/containers/storage/overlay/"
               "c2cc5a7e2baaa2c6bdac6951f759273edbb27a34c710ea4593e43ef22d00e4b8/diff/foss/tools/openroad/bin/sta")
CORNERS = dict(tt="nom", ss="max", ff="min")
HOLD_MARGIN_NS = 0.05
SETUP_MARGIN_NS = 0.0
PARITY_NS = 1e-3
GATES = [("tt", "hold"), ("ss", "hold"), ("ff", "hold"), ("ss", "setup")]
ROW = re.compile(r"^(\S+)(?:\s+\([^)]*\))?\s+(-?[0-9.]+)\s+(-?[0-9.]+)\s+(-?[0-9.]*)\s*\((MET|VIOLATED)\)")


def liberty(pdk: str, corner: str) -> Path:
    spec = compare_spef.PDKS[pdk]
    return compare_spef.PDK_ROOT / spec["base"] / spec["lib"].format(spec["libs"][corner])


def tree_digest(root: Path) -> str:
    return compare_spef.tree_digest(root)


def native_spefs(h, pdk: str, rustroad: Path, ws: Path) -> tuple[dict, dict]:
    before = tree_digest(ws)
    spefs, info = {}, {}
    for rc in ("min", "nom", "max"):
        profile = ROOT / f"fixtures/pdk/{pdk}-rc-{rc}.json"
        out = h.root / f"{pdk}_native_{rc}.spef"
        report, receipt = compare_spef.native_extract(h, f"{pdk}_native_{rc}", rustroad, ws, profile, rc, out)
        again = h.root / f"{pdk}_native_{rc}_rerun.spef"
        compare_spef.native_extract(h, f"{pdk}_native_{rc}_rerun", rustroad, ws, profile, rc, again)
        require(again.read_bytes() == out.read_bytes(), f"{pdk} {rc}: native SPEF reruns differ")
        spefs[rc] = out
        info[rc] = dict(spef=fingerprint(out), rerun_byte_identical=True, profile=fingerprint(profile),
                        stats={k: report["stats"][k] for k in ("nets", "wire_runs", "vias", "patches",
                                                                "patch_pieces", "ground_pf", "coupling_pf",
                                                                "resistance_ohm")},
                        elapsed_seconds=receipt["elapsed_seconds"], peak_rss_kib=receipt["peak_rss_kib"])
    require(tree_digest(ws) == before, f"{pdk}: native extraction modified the workspace")
    return spefs, info


def openrcx_spefs(h, pdk: str, definition: Path) -> tuple[dict, dict]:
    spefs, info = {}, {}
    for rc in ("min", "nom", "max"):
        spef, run = compare_spef.openrcx(h, f"{pdk}_openrcx", pdk, rc, definition, f"{pdk}_openrcx_{rc}")
        spefs[rc], info[rc] = spef, run
    return spefs, info


def ruststa(h, tag: str, pdk: str, netlist: Path, sdc: Path, spefs: dict, sta_bin: Path) -> dict:
    rusteda, sta = str(sta_bin / "rusteda"), str(sta_bin / "ruststa")
    ws = h.root / f"{tag}_sta"
    common = ["--workspace", str(ws), "--threads", "2", "--format", "json", "--color", "never"]
    init = h.json_command(f"{tag}_init", [rusteda, *common, "init"], True)
    argv = [rusteda, *common, "import", "--base-rev", init["new_rev"]]
    for corner in CORNERS:
        argv += ["--lib", str(liberty(pdk, corner)), "--corner", corner]
    argv += ["--netlist", str(netlist), "--sdc", str(sdc)]
    for corner, rc in CORNERS.items():
        argv += ["--spef", str(spefs[rc]), "--spef-corner", corner]
    argv += ["--max-rc-nodes", "200000", "--max-rc-elements", "200000"]
    imp = h.json_command(f"{tag}_import", argv, True)
    run = h.run(f"{tag}_run", [sta, *common, "run", "--base-rev", imp["new_rev"], "--backend", "cpu",
                               "--precision", "f64", "--max-paths", "16", "--max-state-gib", "1",
                               "--require-wire-model", "spef_rc", "--pulse-groups", "last",
                               "--pulse-pins", "clock-ends"])
    require(run["exit_code"] == 0, f"{tag}: RustSTA run failed")
    r = json.loads(bounded_text(Path(run["stdout"])))
    scope = r["scope"]
    require(scope.get("wire_model") == "spef_rc" and scope.get("clock_model") == "propagated",
            f"{tag}: wrong timing scope {scope}")
    require(r["completeness"]["unconstrained_endpoints"] == 0, f"{tag}: unconstrained endpoints")
    out = dict(scope=scope, completeness=r["completeness"], metrics=r["metrics"],
               diagnostics=sorted({d["code"] for d in r.get("diagnostics", [])}),
               peak_rss_kib=run["peak_rss_kib"], corners={})
    for corner in CORNERS:
        rep = h.run(f"{tag}_endpoints_{corner}", [sta, *common, "report", "--rev", r["new_rev"], "--section",
                                                  "endpoints", "--corner", corner, "--limit", "0"])
        require(rep["exit_code"] == 0, f"{tag}: endpoint report failed")
        rows = json.loads(bounded_text(Path(rep["stdout"])))["sections"]["endpoints"]["rows"]
        out["corners"][corner] = {row["endpoint"]: dict(setup=row["setup_slack_ns"], hold=row["hold_slack_ns"])
                                  for row in rows}
    hf = h.run(f"{tag}_hold_fix", [sta, *common, "report", "--rev", r["new_rev"], "--section", "hold_fix",
                                   "--target-margin", str(HOLD_MARGIN_NS), "--limit", "0"])
    require(hf["exit_code"] == 0, f"{tag}: hold_fix report failed")
    shutil.copyfile(hf["stdout"], h.root / f"{tag}_hold_fix.json")
    out["hold_fix"] = json.loads(bounded_text(Path(hf["stdout"])))["sections"]["hold_fix"]
    return out


def opensta(h, tag: str, lib: Path, netlist: Path, sdc: Path, spef: Path) -> dict:
    tcl = h.root / f"{tag}.tcl"
    tcl.write_text(f"""read_liberty {{{lib}}}
read_verilog {{{netlist}}}
link_design lane_smoke
read_sdc {{{sdc}}}
read_spef {{{spef}}}
report_units
puts MARK_SETUP
report_checks -path_delay max -group_path_count 100000 -endpoint_path_count 1 -format end -digits 12
puts MARK_HOLD
report_checks -path_delay min -group_path_count 100000 -endpoint_path_count 1 -format end -digits 12
puts MARK_DRV
report_check_types -max_slew -max_capacitance -max_fanout -violators -digits 12
puts MARK_END
exit
""")
    r = h.run(tag, [str(OPENSTA), "-no_init", "-no_splash", "-exit", str(tcl)])
    require(r["exit_code"] == 0, f"{tag}: OpenSTA failed")
    text = bounded_text(Path(r["stdout"])) + bounded_text(Path(r["stderr"]))
    require("MARK_END" in text and re.search(r"(?mi)^\s*Time\s+1ns\s*$", text), f"{tag}: OpenSTA output incomplete")
    require(not re.search(r"(?mi)^\s*Error(?:\s+\d+)?:", text), f"{tag}: OpenSTA command error")
    sections = {}
    for a, b in (("MARK_SETUP", "MARK_HOLD"), ("MARK_HOLD", "MARK_DRV"), ("MARK_DRV", "MARK_END")):
        sections[a] = text.split(a + "\n", 1)[1].split(b + "\n", 1)[0]
    out = {}
    for kind, mark in (("setup", "MARK_SETUP"), ("hold", "MARK_HOLD")):
        for line in sections[mark].splitlines():
            m = ROW.match(line)
            if m:
                out.setdefault(m[1], {})[kind] = float(m[4])
    return dict(endpoints=out, drv_violations=sections["MARK_DRV"].count("VIOLATED"),
                warnings=sorted({l.strip() for l in text.splitlines() if l.startswith("Warning")})[:20])


def corner_summary(rust: dict, ref: dict) -> dict:
    eps = sorted(set(rust) | set(ref))
    missing = [e for e in eps if e not in rust or e not in ref or set(ref[e]) != {"setup", "hold"}
               or rust[e]["setup"] is None or rust[e]["hold"] is None]
    diff = dict(setup=0.0, hold=0.0)
    for e in eps:
        if e in missing:
            continue
        for k in ("setup", "hold"):
            require(math.isfinite(rust[e][k]) and math.isfinite(ref[e][k]), "non-finite slack")
            diff[k] = max(diff[k], abs(rust[e][k] - ref[e][k]))

    def tool(rows):
        values = {k: [(v[k], e) for e, v in rows.items() if v.get(k) is not None] for k in ("setup", "hold")}
        return dict(worst_setup_ns=min(values["setup"])[0] if values["setup"] else None,
                    worst_hold_ns=min(values["hold"])[0] if values["hold"] else None,
                    setup_violating=sorted(e for s, e in values["setup"] if s < SETUP_MARGIN_NS),
                    hold_violating=sorted(e for s, e in values["hold"] if s < 0.0),
                    hold_below_margin=sorted(e for s, e in values["hold"] if s < HOLD_MARGIN_NS))
    return dict(endpoints=len(eps), missing=missing, max_abs_setup_diff_ns=diff["setup"],
                max_abs_hold_diff_ns=diff["hold"],
                parity="passed" if not missing and max(diff.values()) <= PARITY_NS else "failed",
                ruststa=tool(rust), opensta=tool(ref),
                hold_endpoints=[dict(endpoint=e, ruststa_ns=rust.get(e, {}).get("hold"),
                                     opensta_ns=ref.get(e, {}).get("hold"))
                                for e in eps if min(rust.get(e, {}).get("hold") or 0.0,
                                                    ref.get(e, {}).get("hold") or 0.0) < HOLD_MARGIN_NS])


def case(h, pdk: str, flow: Path, rustroad: Path, sta_bin: Path) -> dict:
    ws = h.root / f"{pdk}_physical"
    shutil.copytree(flow / f"{pdk}_physical", ws)
    require(tree_digest(ws) == tree_digest(flow / f"{pdk}_physical"), "workspace copy differs")
    inputs = {}
    for name in (f"{pdk}.def", f"{pdk}_exported.v", f"{pdk}_postroute.sdc"):
        shutil.copyfile(flow / name, h.root / name)
        inputs[name] = fingerprint(h.root / name)
    definition, netlist, sdc = (h.root / f"{pdk}.def", h.root / f"{pdk}_exported.v",
                                h.root / f"{pdk}_postroute.sdc")
    result = dict(pdk=pdk, inputs=inputs, liberty={c: fingerprint(liberty(pdk, c)) for c in CORNERS},
                  corners={c: dict(liberty=compare_spef.PDKS[pdk]["libs"][c], rc=rc) for c, rc in CORNERS.items()})
    native, result["native_extraction"] = native_spefs(h, pdk, rustroad, ws)
    reference, rcx = openrcx_spefs(h, pdk, definition)
    result["openrcx"] = {rc: dict(spef=v["spef"], rules=v["rules"], peak_rss_kib=v["container_peak_rss_kib"])
                         for rc, v in rcx.items()}
    result["sources"] = {}
    for source, spefs in (("native", native), ("openrcx", reference)):
        tag = f"{pdk}_{source}"
        rust = ruststa(h, tag, pdk, netlist, sdc, spefs, sta_bin)
        entry = dict(ruststa=dict(scope=rust["scope"], completeness=rust["completeness"],
                                  metrics=rust["metrics"], diagnostics=rust["diagnostics"],
                                  peak_rss_kib=rust["peak_rss_kib"]),
                     hold_fix=rust["hold_fix"], corners={})
        for corner, rc in CORNERS.items():
            ref = opensta(h, f"{tag}_{corner}_opensta", liberty(pdk, corner), netlist, sdc, spefs[rc])
            summary = corner_summary(rust["corners"][corner], ref["endpoints"])
            summary.update(opensta_drv_violations=ref["drv_violations"], opensta_warnings=ref["warnings"])
            entry["corners"][corner] = summary
        gates = {}
        for corner, kind in GATES:
            c = entry["corners"][corner]
            margin = HOLD_MARGIN_NS if kind == "hold" else SETUP_MARGIN_NS
            worst = {t: c[t][f"worst_{kind}_ns"] for t in ("ruststa", "opensta")}
            gates[f"{corner}_{kind}"] = dict(
                margin_ns=margin, worst_ns=worst,
                status="met" if all(v is not None and v >= margin for v in worst.values()) else "violated")
        entry["gates"] = gates
        entry["parity"] = "passed" if all(c["parity"] == "passed" for c in entry["corners"].values()) else "failed"
        result["sources"][source] = entry
    return result


def evidence(summary: dict) -> dict:
    """Path-free tracked view."""
    def strip(v):
        if isinstance(v, dict):
            return {k: strip(x) for k, x in v.items() if k not in ("path", "receipt")}
        if isinstance(v, list):
            return [strip(x) for x in v]
        return v
    out = strip({k: v for k, v in summary.items() if k != "receipts"})
    out["receipts"] = len(summary.get("receipts", []))
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--flow", type=Path, required=True, help="finished verify_routing.py run (read only)")
    ap.add_argument("--out", type=Path, required=True, help="new directory (must not exist)")
    ap.add_argument("--rustroad", type=Path, default=ROOT / "target/release/rustroad")
    ap.add_argument("--sta-bin", type=Path, default=STA_BIN)
    ap.add_argument("--pdk", action="append", choices=sorted(compare_spef.PDKS))
    ap.add_argument("--evidence", type=Path)
    a = ap.parse_args()
    root = a.out.resolve()
    root.mkdir(parents=True, exist_ok=False)
    resource.setrlimit(resource.RLIMIT_AS, (8192 * 1024 ** 2, 8192 * 1024 ** 2))
    os.environ.update(GOMAXPROCS="2", OMP_NUM_THREADS="2")
    h = PhysicalHarness(root, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=60,
                                                 tolerance_ns=PARITY_NS))
    started = time.monotonic()
    flow = a.flow.resolve()
    summary = dict(schema="rustroad.timing-baseline.v1", status="unverified",
                   gates=dict(hold_margin_ns=HOLD_MARGIN_NS, setup_margin_ns=SETUP_MARGIN_NS,
                              parity_ns=PARITY_NS, required=[f"{c}_{k}" for c, k in GATES],
                              corners={c: f"{c} Liberty + {rc} RC" for c, rc in CORNERS.items()}),
                   scope=dict(level="block/macro", clocks="propagated", cppr="unchecked", ocv="unchecked",
                              si="unchecked (coupling grounded at factor 1)", ir_em="unchecked",
                              note="baseline before hold closure; nothing fixed here"),
                   flow=str(flow), tools={}, pdks=[])
    try:
        r = h.run("memory_preflight", ["free", "-h"])
        summary["memory_preflight"] = bounded_text(Path(r["stdout"]))
        for name, path in (("rustroad", a.rustroad.resolve()), ("rusteda", a.sta_bin / "rusteda"),
                           ("ruststa", a.sta_bin / "ruststa"), ("opensta", OPENSTA)):
            require(path.is_file(), f"missing {name}")
            r = h.run(f"{name}_version", [str(path), "-version" if name == "opensta" else "--version"])
            summary["tools"][name] = {**fingerprint(path), "version": bounded_text(Path(r["stdout"])).strip()}
        for pdk in a.pdk or ("sky130", "gf180"):
            summary["pdks"].append(case(h, pdk, flow, a.rustroad.resolve(), a.sta_bin.resolve()))
        summary["parity_failures"] = [
            dict(pdk=p["pdk"], spef=src, corner=c, max_abs_setup_diff_ns=v["max_abs_setup_diff_ns"],
                 max_abs_hold_diff_ns=v["max_abs_hold_diff_ns"], missing=v["missing"])
            for p in summary["pdks"] for src, s in p["sources"].items() for c, v in s["corners"].items()
            if v["parity"] != "passed"]
        summary["gate_violations"] = [
            dict(pdk=p["pdk"], spef=src, gate=g, worst_ns=v["worst_ns"], margin_ns=v["margin_ns"])
            for p in summary["pdks"] for src, s in p["sources"].items() for g, v in s["gates"].items()
            if v["status"] != "met"]
        parity = all(s["parity"] == "passed" for p in summary["pdks"] for s in p["sources"].values())
        met = all(g["status"] == "met" for p in summary["pdks"] for s in p["sources"].values()
                  for g in s["gates"].values())
        summary["parity_status"] = "passed" if parity else "failed"
        summary["status"] = "passed" if parity and met else ("violated" if parity else "failed")
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary["status"] = "failed"
        summary["reason"] = str(error)
    finally:
        summary["receipts"] = h.receipts
        summary["wall_seconds"] = round(time.monotonic() - started, 2)
        summary["peak_rss_kib"] = max((x["peak_rss_kib"] for x in h.receipts if x["peak_rss_kib"]), default=None)
        write_json(root / "summary.json", summary)
        if a.evidence:
            a.evidence.write_text(json.dumps(evidence(summary), indent=1, sort_keys=True) + "\n")
    print(json.dumps(dict(status=summary["status"], summary=str(root / "summary.json"), reason=summary.get("reason"))))
    return {"passed": 0, "violated": 3, "failed": 1, "unverified": 3}[summary["status"]]


if __name__ == "__main__":
    sys.exit(main())
