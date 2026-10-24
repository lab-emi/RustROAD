#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Scale flow: RTL -> RustSys -> RustROAD floorplan/place/CTS/route -> native GDS
-> foundry DRC/LVS (black-box, pinned image) -> CEC -> byte-identical rerun.

usage: scale_flow.py --design fixtures/designs/NAME.json --rustroad BIN --rustsys BIN
                     --out DIR [--pdk sky130] [--pdk gf180] [--no-rerun] [--no-signoff]
                     [--refresh-floorplan]

Per PDK the flow
  1. checks every tracked input of the design manifest by SHA-256;
  2. synthesizes the RTL with the cell-policy RustSys arguments (or takes the
     manifest's literal mapped netlist) and renames escaped scalar wires that
     collide with a port bit name (``\\p_q[0]`` -> ``\\rr_p_q[0]``; the CEC below
     checks the renamed netlist against the RTL);
  3. regenerates the floorplan and physical configuration with
     ``rustroad floorplan`` from the manifest recipe and requires them to be
     byte-identical to the tracked files (``--refresh-floorplan`` rewrites them);
  4. runs import-pdk, place, clock-tree (when clocked), route --scope all,
     the native check-antenna, and (if needed) antenna-eco plus a re-run;
  5. exports DEF, Verilog, powered Verilog and the native GDS;
  6. runs scripts/signoff on the native GDS against KLayout's reference
     streamout of the same DEF (XOR), Magic drc(full), the KLayout main deck,
     antenna, density (reported, never gating), Netgen and KLayout LVS;
  7. proves RTL == final netlist with ``rustsys check synth``;
  8. repeats steps 2-5 in a fresh directory and compares DEF, GDS and both
     Verilog exports byte for byte.

Every host command gets a receipt (argv, exit, wall seconds, peak RSS from
wait4). Verdicts come from parsed outputs, never from exit codes alone.
Statuses: passed / failed / unverified; exit 0 / 1 / 3.
Scope: block level; IR/EM/SI/OCV/CPPR unchecked; density reported separately.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts" / "signoff"))
import def2gds_reference  # noqa: E402
import signoff as signoff_harness  # noqa: E402

SCHEMA = "rustroad.scale-flow.v1"
GATING = ("synth", "floorplan_reproduced", "route", "gds_xor", "drc_magic", "drc_klayout",
          "antenna", "lvs_netgen", "lvs_klayout", "cec", "determinism")
HOST_AS_BYTES = 8 * 1024 ** 3
SITES = {"sky130": "unithd", "gf180": "GF018hv5v_mcu_sc7"}
CELL_GDS = {"sky130": "libs.ref/sky130_fd_sc_hd/gds/sky130_fd_sc_hd.gds",
            "gf180": "libs.ref/gf180mcu_fd_sc_mcu7t5v0/gds/gf180mcu_fd_sc_mcu7t5v0.gds"}


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def resolve(p: str | Path) -> Path:
    p = Path(p)
    return p if p.is_absolute() else ROOT / p


def item(status: str, reason: str | None = None, **extra) -> dict:
    assert status in ("passed", "failed", "unverified")
    out = dict(status=status)
    if reason:
        out["reason"] = reason
    out.update(extra)
    return out


def worst(statuses) -> str:
    s = list(statuses)
    if not s:
        return "unverified"
    if "failed" in s:
        return "failed"
    return "passed" if all(x == "passed" for x in s) else "unverified"


class Runner:
    """Host commands with receipts (wall time, peak RSS via wait4)."""

    def __init__(self, root: Path):
        self.root = root
        self.receipts: list[dict] = []
        (root / "receipts").mkdir(parents=True, exist_ok=True)

    def run(self, label: str, argv: list, timeout: int = 1800, env: dict | None = None) -> dict:
        argv = [str(a) for a in argv]
        d = self.root / "receipts" / label
        d.mkdir(parents=True, exist_ok=True)
        out, err = d / "stdout.txt", d / "stderr.txt"

        def limit():
            resource.setrlimit(resource.RLIMIT_AS, (HOST_AS_BYTES, HOST_AS_BYTES))

        start = time.monotonic()
        with out.open("wb") as o, err.open("wb") as e:
            proc = subprocess.Popen(argv, stdout=o, stderr=e, preexec_fn=limit,
                                    env={**os.environ, **(env or {})})
            timed_out = False
            deadline = start + timeout
            status, usage = None, None
            while True:
                pid, status, usage = os.wait4(proc.pid, os.WNOHANG)
                if pid:
                    break
                if time.monotonic() > deadline:
                    proc.kill()
                    timed_out = True
                    _, status, usage = os.wait4(proc.pid, 0)
                    break
                time.sleep(0.02)
        wall = time.monotonic() - start
        code = os.waitstatus_to_exitcode(status)
        receipt = dict(label=label, argv=argv, exit_code=code, timed_out=timed_out,
                       wall_seconds=round(wall, 3), peak_rss_kib=usage.ru_maxrss,
                       stdout=str(out), stderr=str(err))
        (d / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        self.receipts.append(receipt)
        return receipt

    def ok(self, label: str, argv: list, **kw) -> dict:
        r = self.run(label, argv, **kw)
        if r["exit_code"] != 0:
            tail = Path(r["stderr"]).read_text(errors="replace")[-800:]
            raise StepError(f"{label}: exit {r['exit_code']}: {tail}")
        return r

    def json(self, label: str, argv: list, **kw) -> dict:
        r = self.ok(label, argv, **kw)
        return json.loads(Path(r["stdout"]).read_text())


class StepError(RuntimeError):
    pass


def deconflict(text: str) -> tuple[str, int]:
    """Rename escaped scalar wires that equal a vector port bit name.

    RustSys writes register outputs as ``\\p_q[0] `` next to the port ``p_q``;
    the physical importer refuses the ambiguous DEF name. The renamed wire is
    ``\\rr_p_q[0] ``; ports and instance names are untouched."""
    vectors = set(re.findall(r"^\s*(?:input|output|inout)\s+wire\s+\[\s*-?\d+\s*:\s*-?\d+\s*\]\s+(\w+)",
                             text, re.M))
    count = 0

    def sub(m):
        nonlocal count
        if m[1] in vectors:
            count += 1
            return "\\rr_" + m[1] + "[" + m[2] + "] "
        return m[0]
    return re.sub(r"\\(\w+)\[(\d+)\] ", sub, text), count


def pdk_entry(tag: str) -> dict:
    data = json.loads((ROOT / "fixtures/pdk/local-real-pdks.json").read_text())
    return [p for p in data["pdks"] if p["tag"] == tag][0]


def verify_tracked(m: dict, tag: str) -> dict:
    rows, bad = {}, []
    entries = {f"rtl.{i}": r for i, r in enumerate(m["rtl"])}
    entries["sdc.clock"] = m["sdc"]["clock"]
    entries["cell_policy"] = m["cell_policy"]
    spec = m["pdks"][tag]
    for key in ("floorplan", "physical_config", "routing_config", "gds_layer_map"):
        entries[f"pdks.{tag}.{key}"] = spec[key]
    for key in ("netlist", "physical_profile"):
        if key in spec:
            entries[f"pdks.{tag}.{key}"] = spec[key]
    for key, e in entries.items():
        p = resolve(e["path"])
        actual = sha256(p) if p.is_file() else None
        rows[key] = dict(path=str(p), expected=e["sha256"], actual=actual)
        if actual != e["sha256"]:
            bad.append(key)
    return item("passed" if not bad else "failed", None if not bad else f"changed: {bad}", files=rows)


def build(r: Runner, run_dir: Path, m: dict, tag: str, args, label: str) -> dict:
    """Steps 2-5 in run_dir; returns artifact paths and step facts."""
    spec = m["pdks"][tag]
    p = pdk_entry(tag)
    lib = Path(p["liberty"]["path"])
    run_dir.mkdir(parents=True, exist_ok=True)
    facts: dict = {}
    mapped = run_dir / "mapped.v"
    if "netlist" in spec:
        shutil.copyfile(resolve(spec["netlist"]["path"]), mapped)
        facts["synth"] = dict(source="literal manifest netlist", path=spec["netlist"]["path"])
    else:
        policy = json.loads(resolve(m["cell_policy"]["path"]).read_text())
        rtl = [resolve(x["path"]) for x in m["rtl"]]
        rep = r.ok(f"{label}_synth", [args.rustsys, "--threads", "2", "--progress", "off", "synth",
                                      "--backend", "cpu", "--top", m["top"], "--lib", lib,
                                      *policy["pdks"][tag]["rustsys_args"], "--out", mapped,
                                      "--print", "json", *rtl])
        report = json.loads(Path(rep["stdout"]).read_text())
        facts["synth"] = dict(cells=sum(report["cells"].values()), area=report.get("area"),
                              sequential=report.get("sequential_cells"))
    text, renamed = deconflict(mapped.read_text())
    netlist = run_dir / "netlist.v"
    netlist.write_text(text)
    facts["renamed_escaped_wires"] = renamed
    fp = m["floorplan_recipe"]
    floor, phys = run_dir / "floorplan.json", run_dir / "physical.json"
    road = [args.rustroad]
    clock = ["--no-clock"] if m["clock"] is None else ["--clock-port", m["clock"]["port"]]
    rep = r.json(f"{label}_floorplan", road + [
        "floorplan", "--tech-lef", p["technology_lef"]["path"], "--cell-lef", p["cell_lef"]["path"],
        "--netlist", netlist, "--physical-config", resolve(spec["physical_profile"]["path"]),
        "--routing-config", resolve(spec["routing_config"]["path"]), "--site", SITES[tag],
        "--utilization", str(fp["utilization"]), "--aspect", str(fp["aspect"]),
        "--margin-um", str(fp["margin_um"]), *clock,
        "--out-floorplan", floor, "--out-physical-config", phys])
    facts["floorplan"] = {k: rep[k] for k in ("die_area", "core_area", "rows", "estimated_utilization",
                                             "io_pins", "io_side_counts", "io_width_dbu",
                                             "clock_buffers_estimate", "cell_area_um2")}
    tracked_floor, tracked_phys = resolve(spec["floorplan"]["path"]), resolve(spec["physical_config"]["path"])
    if args.refresh_floorplan and label.endswith("a"):
        shutil.copyfile(floor, tracked_floor)
        shutil.copyfile(phys, tracked_phys)
    facts["floorplan_reproduced"] = (floor.read_bytes() == tracked_floor.read_bytes()
                                     and phys.read_bytes() == tracked_phys.read_bytes())
    ws = run_dir / "ws"
    rr = [args.rustroad, "--workspace", ws, "--threads", "2", "--max-cells", "250000",
          "--max-rows", "100000", "--max-expansions", "100000000"]
    eco = 0
    current = netlist
    while True:
        if ws.exists():
            shutil.rmtree(ws)
        a = r.json(f"{label}_import{eco}", rr + ["import-pdk", "--base-rev", "rev0", "--tech-lef",
                                                 p["technology_lef"]["path"], "--cell-lef", p["cell_lef"]["path"],
                                                 "--netlist", current, "--floorplan", tracked_floor,
                                                 "--physical-config", tracked_phys])
        a = r.json(f"{label}_place{eco}", rr + ["place", "--base-rev", a["revision"]])
        facts["placement"] = a.get("placement")
        if m["clock"] is not None:
            a = r.json(f"{label}_cts{eco}", rr + ["clock-tree", "--base-rev", a["revision"], "--sdc",
                                                  resolve(m["sdc"]["clock"]["path"])])
            facts["placement_after_cts"] = a.get("placement")
            facts["clock_buffers"] = a.get("clock_buffers")
        a = r.json(f"{label}_route{eco}", rr + ["route", "--base-rev", a["revision"], "--scope", "all",
                                                "--config", resolve(spec["routing_config"]["path"])],
                   env={"RUSTROAD_ROUTE_TRACE": "1"})
        facts["route"] = {k: a.get(k) for k in ("routed_nets", "routing_segments", "routing_expansions",
                                                 "cells", "tap_cells", "filler_cells")}
        ant = r.json(f"{label}_antenna{eco}", rr + ["check-antenna"])
        facts.setdefault("native_antenna", []).append(dict(
            status=ant.get("status"), violations=ant.get("violations"),
            checks=(ant.get("stats") or {}).get("checks")))
        if not ant.get("violations") or eco >= 2:
            break
        eco += 1
        report = run_dir / f"antenna{eco}.json"
        report.write_text(Path(r.receipts[-1]["stdout"]).read_text())
        fixed = run_dir / f"netlist_eco{eco}.v"
        r.json(f"{label}_antenna_eco{eco}", [args.rustroad, "antenna-eco", "--netlist", current,
                                             "--antenna-report", report,
                                             "--delay-cells", resolve(spec["delay_cells"]["path"]),
                                             "--out", fixed, "--plan-out", run_dir / f"antenna_plan{eco}.json"])
        current = fixed
    facts["antenna_eco_iterations"] = eco
    facts["final_netlist"] = str(current)
    outs = {}
    for name, extra in (("def", ["export-def"]), ("verilog", ["export-verilog"]),
                        ("powered", ["export-verilog", "--include-power"]),
                        ("routes", ["export-routes"])):
        rec = r.ok(f"{label}_export_{name}", rr + extra)
        path = run_dir / {"def": "final.def", "verilog": "final.v", "powered": "powered.v",
                          "routes": "routes.json"}[name]
        shutil.copyfile(rec["stdout"], path)
        outs[name] = path
    gds = run_dir / "final.gds"
    if gds.exists():
        gds.unlink()
    hr = json.loads((ROOT / "fixtures/flow/signoff-pinned.json").read_text())["pdks"][tag]["host_root"]
    r.json(f"{label}_export_gds", rr + ["export-gds", "--cell-gds", Path(hr) / CELL_GDS[tag],
                                        "--layer-map", resolve(spec["gds_layer_map"]["path"]),
                                        "--out", gds])
    outs["gds"] = gds
    routes = json.loads(outs["routes"].read_text())
    stats = (routes.get("routes") or routes).get("stats", {}) if isinstance(routes, dict) else {}
    facts["route_stats"] = stats
    return dict(outputs=outs, facts=facts, workspace=ws)


def signoff_checks(run_dir: Path, tag: str, top: str, outs: dict) -> dict:
    ref = run_dir / "reference.gds"
    receipt = def2gds_reference.stream(outs["def"], tag, ref, ROOT / "fixtures/flow/signoff-pinned.json")
    if receipt["status"] != "passed":
        return dict(statuses={k: item("unverified", "reference streamout failed") for k in
                              ("gds_xor", "drc_magic", "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout")},
                    reference=receipt)
    ns = argparse.Namespace(gds=outs["gds"], top=top, pdk=tag, out=run_dir / "signoff", netlist=outs["powered"],
                            against=ref, against_top=top, manifest=ROOT / "fixtures/flow/signoff-pinned.json",
                            threads=4, memory="4g", timeout=1800, plant=None)
    summary = signoff_harness.run_checks(ns, list(signoff_harness.CHECKS))
    (run_dir / "signoff").mkdir(exist_ok=True)
    (run_dir / "signoff" / "signoff.json").write_text(json.dumps(summary, indent=2) + "\n")
    st = summary["statuses"]
    out = {}
    for key, name in (("gds_xor", "xor"), ("drc_magic", "drc_magic"), ("drc_klayout", "drc_klayout"),
                      ("antenna", "antenna"), ("lvs_netgen", "lvs_netgen"), ("lvs_klayout", "lvs_klayout"),
                      ("density", "density")):
        verdict = summary["verdicts"].get(name.replace("_", "-"))
        out[key] = item(st.get(name, "unverified"), None if st.get(name) == "passed" else f"see {verdict}",
                        count=summary["counts"].get(name.replace("_", "-")),
                        by_rule=summary["by_rule"].get(name.replace("_", "-")))
    return dict(statuses=out, summary=str(run_dir / "signoff" / "signoff.json"),
                container_peak_rss_kib=summary["resources"]["peak_rss_kib"],
                container_wall_seconds=summary["resources"].get("wall_seconds"))


def cec(r: Runner, m: dict, tag: str, netlist: Path, args) -> dict:
    lib = Path(pdk_entry(tag)["liberty"]["path"])
    rtl = [resolve(x["path"]) for x in m["rtl"]]
    v = m["verification"]
    res = r.run(f"{tag}_cec", [args.rustsys, "--threads", "2", "--progress", "off", "check", "synth", "--top",
                               m["top"], "--lib", lib, "--netlist", netlist, "--budget", v["rustsys_budget"],
                               "--model-budget", v["rustsys_model_budget"], "--sweep-max-candidates",
                               v["rustsys_sweep_max_candidates"], "--print", "json", *rtl])
    try:
        verdict = json.loads(Path(res["stdout"]).read_text()).get("verdict")
    except (json.JSONDecodeError, OSError):
        verdict = None
    if verdict == "equivalent":
        return item("passed", verdict=verdict)
    if verdict in ("not_equivalent", "counterexample", "different"):
        return item("failed", f"verdict {verdict}", verdict=verdict)
    return item("unverified", f"no definitive verdict ({verdict}, exit {res['exit_code']})", verdict=verdict)


def flow(m: dict, tag: str, args, out: Path) -> dict:
    root = out / tag
    r = Runner(root)
    started = time.monotonic()
    result: dict = dict(pdk=tag, statuses={})
    st = result["statuses"]
    st["inputs"] = verify_tracked(m, tag)
    try:
        a = build(r, root / "a", m, tag, args, f"{tag}_a")
    except StepError as e:
        for k in GATING:
            st.setdefault(k, item("failed" if k == "route" else "unverified", str(e)[:2000]))
        result["error"] = str(e)[:4000]
        result["receipts"] = r.receipts
        return result
    facts = a["facts"]
    result["facts"] = facts
    st["synth"] = item("passed", **({} if "cells" not in facts["synth"] else facts["synth"]))
    st["floorplan_reproduced"] = item("passed" if facts["floorplan_reproduced"] else "failed",
                                      None if facts["floorplan_reproduced"] else
                                      "regenerated floorplan/physical config differ from the tracked files")
    st["route"] = item("passed", **facts["route"])
    if not args.no_signoff:
        s = signoff_checks(root / "a", tag, m["top"], a["outputs"])
        st.update(s["statuses"])
        result["signoff"] = {k: v for k, v in s.items() if k != "statuses"}
    else:
        for k in ("gds_xor", "drc_magic", "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout", "density"):
            st[k] = item("unverified", "signoff not run (--no-signoff)")
    st["cec"] = cec(r, m, tag, a["outputs"]["verilog"], args)
    if not args.no_rerun:
        try:
            b = build(r, root / "b", m, tag, args, f"{tag}_b")
            same = {k: sha256(a["outputs"][k]) == sha256(b["outputs"][k])
                    for k in ("def", "gds", "verilog", "powered")}
            st["determinism"] = item("passed" if all(same.values()) else "failed",
                                     None if all(same.values()) else f"differs: {same}",
                                     sha256={k: sha256(a["outputs"][k]) for k in same})
        except StepError as e:
            st["determinism"] = item("failed", f"rerun failed: {e}"[:2000])
    else:
        st["determinism"] = item("unverified", "rerun not requested")
    host = [x for x in r.receipts]
    result["resources"] = dict(
        wall_seconds=round(time.monotonic() - started, 3),
        host_peak_rss_kib=max(x["peak_rss_kib"] for x in host),
        steps={x["label"]: dict(wall_seconds=x["wall_seconds"], peak_rss_kib=x["peak_rss_kib"]) for x in host},
        container_peak_rss_kib=(result.get("signoff") or {}).get("container_peak_rss_kib"))
    result["artifacts"] = {k: dict(path=str(v), sha256=sha256(v)) for k, v in a["outputs"].items()}
    result["status"] = worst(st[k]["status"] for k in GATING)
    result["receipts"] = [dict(label=x["label"], exit_code=x["exit_code"], wall_seconds=x["wall_seconds"],
                               peak_rss_kib=x["peak_rss_kib"]) for x in r.receipts]
    return result


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--design", type=Path, required=True)
    ap.add_argument("--rustroad", type=Path, required=True)
    ap.add_argument("--rustsys", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--pdk", action="append", choices=("sky130", "gf180"))
    ap.add_argument("--no-rerun", action="store_true")
    ap.add_argument("--no-signoff", action="store_true")
    ap.add_argument("--refresh-floorplan", action="store_true")
    args = ap.parse_args(argv)
    m = json.loads(resolve(args.design).read_text())
    if m.get("schema") != "rustroad.design.v1":
        raise SystemExit("design manifest schema must be rustroad.design.v1")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    pdks = args.pdk or list(m["pdks"])
    per = {tag: flow(m, tag, args, args.out) for tag in pdks}
    status = worst(p.get("status", "unverified") for p in per.values())
    summary = dict(schema=SCHEMA, design=m["name"], top=m["top"], status=status,
                   gating=list(GATING), rustroad=dict(path=str(args.rustroad), sha256=sha256(args.rustroad)),
                   rustsys=dict(path=str(args.rustsys), sha256=sha256(args.rustsys)),
                   manifest=dict(path=str(args.design), sha256=sha256(resolve(args.design))),
                   scope=dict(level="block", ir_drop="unchecked", electromigration="unchecked",
                              signal_integrity="unchecked", ocv="unchecked", cppr="unchecked",
                              density="reported separately, not gating", timing="not part of this flow"),
                   wall_seconds=round(time.monotonic() - started, 3), pdks=per)
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({"design": m["name"], "status": status,
                      **{t: {k: v["status"] for k, v in p["statuses"].items()} for t, p in per.items()}}))
    return {"passed": 0, "failed": 1, "unverified": 3}[status]


if __name__ == "__main__":
    sys.exit(main())
