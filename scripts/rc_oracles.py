#!/usr/bin/env python3
"""Native RC extraction against two independent black-box oracles.

For every routed case (one PDK workspace, its post-route netlist and SDC) and
every RC corner (min, nom, max) this campaign runs:

* the native extractor twice (byte determinism; read-only; refusals);
* OpenRCX (pinned ``openroad/orfs`` image) on the DEF the flow writes;
* Magic (pinned signoff image) through ``scripts/magic_rc.py`` on the native
  GDS of the same workspace: per-net capacitance from a flat extraction minus
  the standalone cell-pin capacitance, per-net resistance from a routing-only
  terminal layout;
* RustSTA at tt+nom, ss+max and ff+min with each SPEF. Magic has no SPEF
  writer, so its per-net totals are back-annotated onto the native RC trees.
  The OpenRCX totals are back-annotated the same way as a fidelity control:
  that SPEF must time like OpenRCX's own SPEF.

Targets are the fixed ones of docs/extraction.md, evaluated against each
oracle: total wire C within +/-10 %; per-net C within +/-20 % for nets with
oracle C >= 5 fF; per-net R within +/-20 % for nets with oracle R >= 20 ohm;
endpoint setup/hold slack delta <= 10 ps. Nothing native is fitted to either
oracle; the oracle-to-oracle disagreement is measured the same way.

usage:
  rc_oracles.py campaign --workspace OUT --base-rev empty --inputs CASES.json \
      [--rustroad BIN] [--variant NAME=BIN ...] --sta-bin DIR
  rc_oracles.py report --summary OUT/summary.json
  rc_oracles.py evidence --summary OUT/summary.json --out docs/evidence/X.json

CASES.json: {"design": "...", "cases": [{"pdk": "sky130", "workspace": DIR,
  "netlist": V, "sdc": SDC, "flow_spef": optional OpenRCX nominal SPEF of the flow}]}
"""
from __future__ import annotations

import argparse
import fcntl
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
import compare_spef as cs  # noqa: E402
import magic_rc  # noqa: E402
from verify_logic import bounded_text, ideal_projection, sha256, write_json  # noqa: E402
from verify_physical import PhysicalHarness, fingerprint, require  # noqa: E402

ROOT = HERE.parent
TARGETS = cs.TARGETS
CORNERS = ("min", "nom", "max")


def per_net_spef(path: Path) -> dict:
    spef = cs.parse_spef(path)
    out = {}
    for name, net in spef["nets"].items():
        m = cs.net_metrics(name, net)
        out[name] = dict(c_pf=m["total_c_pf"], coupling_pf=m["coupling_pf"], r_ohm=m["total_r_ohm"])
    return out


def per_net_magic(record: dict) -> dict:
    return {n: dict(c_pf=v["c_pf"], coupling_pf=v["coupling_pf"], r_ohm=v.get("r_ohm"))
            for n, v in record["nets"].items()}


def compare_totals(native: dict, oracle: dict) -> dict:
    """Targets on per-net totals of two extractions (net name -> c_pf, coupling_pf, r_ohm)."""
    t = TARGETS
    common = sorted(set(native) & set(oracle))
    coverage = dict(status="passed" if set(native) == set(oracle) else "failed",
                    only_native=sorted(set(native) - set(oracle)), only_oracle=sorted(set(oracle) - set(native)))
    design = lambda d: sum(d[n]["c_pf"] - d[n]["coupling_pf"] / 2.0 for n in common)  # noqa: E731
    total_rel = cs.rel(design(native), design(oracle))
    per_net_sum_rel = cs.rel(sum(native[n]["c_pf"] for n in common), sum(oracle[n]["c_pf"] for n in common))
    c_rows = [n for n in common if oracle[n]["c_pf"] >= t["net_c_min_pf"]]
    r_rows = [n for n in common if (oracle[n]["r_ohm"] or 0.0) >= t["net_r_min_ohm"]]
    crel = {n: cs.rel(native[n]["c_pf"], oracle[n]["c_pf"]) for n in common}
    rrel = {n: cs.rel(native[n]["r_ohm"], oracle[n]["r_ohm"]) for n in common if oracle[n]["r_ohm"]}

    def dist(values):
        values = sorted(v for v in values if v is not None)
        return None if not values else dict(min=values[0], median=values[len(values) // 2], max=values[-1],
                                            count=len(values))
    return dict(
        coverage=coverage, total_c_rel=total_rel, per_net_sum_c_rel=per_net_sum_rel,
        targets=dict(
            total_c=dict(status="passed" if total_rel is not None and abs(total_rel) <= t["total_c_rel"]
                         else "failed", rel=total_rel, bound=t["total_c_rel"]),
            net_c=dict(status="passed" if c_rows and all(abs(crel[n]) <= t["net_c_rel"] for n in c_rows)
                       else "failed", eligible=len(c_rows),
                       failing=[n for n in c_rows if abs(crel[n]) > t["net_c_rel"]],
                       distribution=dist(crel[n] for n in c_rows), bound=t["net_c_rel"]),
            net_r=dict(status="passed" if r_rows and all(abs(rrel[n]) <= t["net_r_rel"] for n in r_rows)
                       else "failed", eligible=len(r_rows),
                       failing=[n for n in r_rows if abs(rrel[n]) > t["net_r_rel"]],
                       distribution=dist(rrel[n] for n in r_rows), bound=t["net_r_rel"]),
            coverage=dict(status=coverage["status"])),
        nets={n: dict(native_c_pf=native[n]["c_pf"], oracle_c_pf=oracle[n]["c_pf"], c_rel=crel[n],
                      native_r_ohm=native[n]["r_ohm"], oracle_r_ohm=oracle[n]["r_ohm"], r_rel=rrel.get(n))
              for n in common})


def terminal_decomposition(record: dict, native_spef: Path, profile: dict, routes: dict) -> dict:
    """Split Magic's per-net R into the element at each pin-via terminal (the
    resistor between a terminal on the lowest routing layer and the network)
    and the rest, and compare the rest with the native R minus the native via
    resistance at those terminals (the cut resistance of that via)."""
    layers = routes["layers"]
    lowest = layers[0]
    lower_cut = {}
    for v in profile["cuts"]:
        lower_cut[v["lower"]] = v["resistance_ohm_per_cut"]
    vias_at = {(routes["via_lower_layers"][v["kind"]], v["at"][0], v["at"][1]) for v in routes["routes"]["vias"]}
    access = {a["pin"]: a for a in routes["routes"]["access"]}
    native = per_net_spef(native_spef)
    rows = {}
    for name, net in record["nets"].items():
        magic_terminal = native_via = 0.0
        count = ambiguous = 0
        for t in net.get("terminals", {}).values():
            a = access[t["pin"]]
            if t["layer"] != lowest or (a["layer"], a["at"][0], a["at"][1]) not in vias_at:
                continue
            if t.get("incident_ohm") is None:
                ambiguous += 1
                continue
            magic_terminal += t["incident_ohm"]
            native_via += lower_cut[lowest]
            count += 1
        rest_magic = net["r_ohm"] - magic_terminal
        rest_native = native[name]["r_ohm"] - native_via
        rows[name] = dict(pin_via_terminals=count, ambiguous_terminals=ambiguous, magic_terminal_ohm=magic_terminal,
                          native_via_ohm=native_via, magic_rest_ohm=rest_magic, native_rest_ohm=rest_native,
                          rest_rel=cs.rel(rest_native, rest_magic))
    eligible = [r for r in rows.values() if r["magic_rest_ohm"] >= TARGETS["net_r_min_ohm"]
                and not r["ambiguous_terminals"]]
    worst = max((abs(r["rest_rel"]) for r in eligible), default=None)
    per_terminal = sorted(r["magic_terminal_ohm"] / r["pin_via_terminals"] for r in rows.values()
                          if r["pin_via_terminals"])
    return dict(lowest_layer=lowest, native_via_ohm_per_terminal=lower_cut.get(lowest),
                magic_terminal_ohm_per_terminal=dict(min=per_terminal[0], max=per_terminal[-1])
                if per_terminal else None,
                rest=dict(eligible=len(eligible), max_abs_rel=worst,
                          status="passed" if worst is not None and worst <= TARGETS["net_r_rel"] else "failed"),
                nets=rows)


def sta_set(h, tag, lib, corner, netlist, sdc, spefs: dict) -> dict:
    return {label: cs.sta_run(h, f"{tag}_{label}", lib, corner, netlist, sdc, spef) for label, spef in spefs.items()}


def strip_endpoints(delta: dict) -> dict:
    return {k: v for k, v in delta.items() if k != "endpoints"}


def campaign_case(h, case: dict, rustroad: Path, variants: dict) -> dict:
    pdk = case["pdk"]
    spec = cs.PDKS[pdk]
    base = cs.PDK_ROOT / spec["base"]
    tag = pdk
    result = dict(pdk=pdk)
    ws = h.root / f"{pdk}_physical"
    shutil.copytree(Path(case["workspace"]), ws)
    require(cs.tree_digest(ws) == cs.tree_digest(Path(case["workspace"])), "workspace copy differs")
    netlist, sdc = h.root / f"{pdk}_netlist.v", h.root / f"{pdk}_postroute.sdc"
    shutil.copyfile(case["netlist"], netlist)
    shutil.copyfile(case["sdc"], sdc)
    result["inputs"] = dict(workspace=dict(source=case["workspace"], tree_sha256=cs.tree_digest(ws)),
                            netlist=fingerprint(netlist), sdc=fingerprint(sdc))
    before = cs.tree_digest(ws)
    # Exports for OpenRCX and Magic (read-only commands).
    exports = h.root / f"{pdk}_exports"
    exports.mkdir()
    paths = dict(gds=exports / "design.gds", def_=exports / "design.def", routes=exports / "routes.json",
                 conn=exports / "connectivity.json")
    mspec = magic_rc.manifest_pdk(pdk)
    cell_gds = Path(mspec["host_root"]) / mspec["files"]["cell_gds"]["path"]
    common = [str(rustroad), "--workspace", str(ws), "--max-cells", "250000", "--max-rows", "100000"]
    r = h.run(f"{tag}_export_gds", common + ["export-gds", "--cell-gds", str(cell_gds), "--layer-map",
                                             str(ROOT / f"fixtures/pdk/{pdk}-gds-map.json"), "--out",
                                             str(paths["gds"])])
    require(r["exit_code"] == 0, "export-gds failed")
    for label, command, key in (("export_def", "export-def", "def_"), ("export_routes", "export-routes", "routes"),
                                ("export_connectivity", "export-connectivity", "conn")):
        r = h.run(f"{tag}_{label}", common + [command])
        require(r["exit_code"] == 0, f"{command} failed")
        shutil.copyfile(r["stdout"], paths[key])
    paths["design"] = ws / "revisions" / (ws / "HEAD").read_text().strip() / "design.json"
    routes = json.loads(bounded_text(paths["routes"], 64 * 1024 ** 2))
    # Native extraction, twice per corner.
    native, reports = {}, {}
    for corner in CORNERS:
        profile = ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json"
        out = h.root / f"{pdk}_native_{corner}.spef"
        report, receipt = cs.native_extract(h, f"{tag}_native_{corner}", rustroad, ws, profile, corner, out)
        again = h.root / f"{pdk}_native_{corner}_rerun.spef"
        cs.native_extract(h, f"{tag}_native_{corner}_rerun", rustroad, ws, profile, corner, again)
        require(again.read_bytes() == out.read_bytes(), f"{pdk} {corner}: native SPEF not byte-identical")
        native[corner] = out
        reports[corner] = dict(output=report["output"], stats=report["stats"], scope=report["scope"],
                               byte_identical_rerun=True, elapsed_seconds=receipt["elapsed_seconds"],
                               peak_rss_kib=receipt["peak_rss_kib"])
    inside = cs.native_extract(h, f"{tag}_native_inside_workspace", rustroad, ws,
                               ROOT / f"fixtures/pdk/{pdk}-rc-nom.json", "nom", ws / "x.spef", expect_ok=False)
    mismatch = cs.native_extract(h, f"{tag}_native_corner_mismatch", rustroad, ws,
                                 ROOT / f"fixtures/pdk/{pdk}-rc-max.json", "nom", h.root / "mismatch.spef",
                                 expect_ok=False)
    require(inside["exit_code"] != 0 and "OUTPUT_IN_WORKSPACE" in bounded_text(Path(inside["stderr"])),
            "in-workspace output not refused")
    require(mismatch["exit_code"] != 0 and "CORNER_MISMATCH" in bounded_text(Path(mismatch["stderr"])),
            "corner mismatch not refused")
    require(cs.tree_digest(ws) == before and not (ws / "x.spef").exists(), "native extraction changed the workspace")
    result["native"] = reports
    result["read_only"] = dict(status="passed", workspace_tree_sha256=before)
    dbu = float(json.loads(bounded_text(paths["design"], 16 * 1024 ** 2))["design"]["units_per_micron"])
    result["resistance_accounting"] = {
        c: cs.resistance_accounting(paths["routes"], paths["conn"], ROOT / f"fixtures/pdk/{pdk}-rc-{c}.json",
                                    native[c], dbu) for c in CORNERS}
    # Ablation: other extractor binaries on the same workspace.
    old = {}
    for vname, vbin in variants.items():
        old[vname] = {}
        for corner in CORNERS:
            out = h.root / f"{pdk}_variant_{vname}_{corner}.spef"
            cs.native_extract(h, f"{tag}_variant_{vname}_{corner}", vbin, ws,
                              ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json", corner, out)
            old[vname][corner] = out
    # OpenRCX on the flow's DEF.
    rcx, result["openrcx"] = {}, {}
    for corner in CORNERS:
        spef, info = cs.openrcx(h, f"{tag}_rcx", pdk, corner, paths["def_"], f"{pdk}_rcx_{corner}")
        rcx[corner] = spef
        result["openrcx"][corner] = info
    if case.get("flow_spef"):
        flow = per_net_spef(Path(case["flow_spef"]))
        mine = per_net_spef(rcx["nom"])
        result["openrcx"]["flow_reference_nominal"] = dict(
            source=fingerprint(Path(case["flow_spef"])),
            total_c_rel=cs.rel(sum(v["c_pf"] for v in mine.values()), sum(v["c_pf"] for v in flow.values())),
            max_abs_net_c_rel=max(abs(cs.rel(mine[n]["c_pf"], flow[n]["c_pf"]) or 0.0) for n in mine))
    # Magic.
    magic, magic_ba, result["magic"] = {}, {}, {}
    for corner in CORNERS:
        ba = h.root / f"{pdk}_magic_ba_{corner}.spef"
        record = magic_rc.run_corner(paths, pdk, corner, native[corner], h.root / f"{pdk}_magic", ba)
        magic[corner] = record
        magic_ba[corner] = ba
        result["magic"][corner] = dict(style=record["style"], totals=record["totals"],
                                       receipts=record["receipts"], terminal_layout=record["terminal_layout"],
                                       backannotated_spef=record["backannotated_spef"],
                                       instances_mapped=record["instances_mapped"])
    again = magic_rc.run_corner(paths, pdk, "nom", native["nom"], h.root / f"{pdk}_magic_rerun", None)
    same = all(magic["nom"]["nets"][n][k] == again["nets"][n][k] for n in again["nets"]
               for k in ("c_pf", "coupling_pf", "r_ohm"))
    result["magic"]["determinism"] = dict(status="passed" if same else "failed",
                                          compared="per-net c_pf, coupling_pf, r_ohm at nom (two runs)")
    # Back-annotation fidelity control: OpenRCX totals onto native trees.
    rcx_ba = {}
    for corner in CORNERS:
        out = h.root / f"{pdk}_rcx_ba_{corner}.spef"
        magic_rc.backannotate(native[corner], per_net_spef(rcx[corner]), out)
        rcx_ba[corner] = out
    # SPEF comparisons.
    result["spef"] = {}
    for corner in CORNERS:
        n = per_net_spef(native[corner])
        x = per_net_spef(rcx[corner])
        m = per_net_magic(magic[corner])
        entry = dict(vs_openrcx=cs.compare_spefs(native[corner], rcx[corner]),
                     vs_openrcx_totals=compare_totals(n, x),
                     vs_magic=compare_totals(n, m),
                     openrcx_vs_magic=compare_totals(x, m),
                     magic_r_terminals=terminal_decomposition(
                         magic[corner], native[corner], json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json")
                                                                   .read_text()), routes))
        entry["variants"] = {}
        for vname in old:
            o = per_net_spef(old[vname][corner])
            entry["variants"][vname] = dict(vs_openrcx=compare_totals(o, x)["targets"],
                                            vs_magic=compare_totals(o, m)["targets"])
        result["spef"][corner] = entry
    # STA.
    libs = {}
    for c, name in spec["libs"].items():
        source = base / spec["lib"].format(name)
        projected = h.root / f"{pdk}_{c}_ideal.lib"
        libs[c] = dict(projection=ideal_projection(source, projected), path=projected, source=fingerprint(source))
    result["sta"] = {}
    for sta_corner, rc in cs.STA_CORNERS.items():
        spefs = dict(native=native[rc], openrcx=rcx[rc], magic_ba=magic_ba[rc], openrcx_ba=rcx_ba[rc])
        for vname in old:
            spefs[f"variant_{vname}"] = old[vname][rc]
        runs = sta_set(h, f"{tag}_{sta_corner}", libs[sta_corner]["path"], sta_corner, netlist, sdc, spefs)
        entry = dict(liberty=libs[sta_corner]["source"], rc_corner=rc,
                     runs={k: {x: v[x] for x in ("scope", "metrics", "completeness", "diagnostics", "peak_rss_kib")}
                           for k, v in runs.items()},
                     vs_openrcx=cs.sta_delta(runs["native"], runs["openrcx"]),
                     vs_magic=cs.sta_delta(runs["native"], runs["magic_ba"]),
                     openrcx_vs_magic=cs.sta_delta(runs["openrcx_ba"], runs["magic_ba"]),
                     backannotation_fidelity=cs.sta_delta(runs["openrcx_ba"], runs["openrcx"]),
                     endpoints={k: v["endpoints"] for k, v in runs.items()})
        entry["variants"] = {vname: dict(vs_openrcx=strip_endpoints(cs.sta_delta(runs[f"variant_{vname}"],
                                                                                   runs["openrcx"])),
                                         vs_magic=strip_endpoints(cs.sta_delta(runs[f"variant_{vname}"],
                                                                               runs["magic_ba"])))
                             for vname in old}
        # Attribution where the slack target is missed: set one net at a time
        # to the oracle's per-net totals on the native trees and record the
        # move of the worst endpoint (first-order, per net).
        oracle_nets = dict(vs_openrcx=per_net_spef(rcx[rc]), vs_magic=per_net_magic(magic[rc]))
        native_nets = per_net_spef(native[rc])
        for ref, delta in (("vs_openrcx", entry["vs_openrcx"]), ("vs_magic", entry["vs_magic"])):
            if delta["status"] == "passed":
                continue
            worst = max(delta["endpoints"], key=lambda r: max(abs(r["setup_slack_delta_ns"]),
                                                               abs(r["hold_slack_delta_ns"])))
            kind = "setup" if abs(worst["setup_slack_delta_ns"]) >= abs(worst["hold_slack_delta_ns"]) else "hold"
            base_slack = runs["native"]["endpoints"][worst["endpoint"]][f"{kind}_slack_ns"]
            rows = []
            for net in sorted(native_nets):
                one = {n: dict(native_nets[n]) for n in native_nets}
                one[net] = oracle_nets[ref][net]
                spef = h.root / f"{pdk}_attr_{sta_corner}_{ref}_{len(rows)}.spef"
                magic_rc.backannotate(native[rc], one, spef)
                run = cs.sta_run(h, f"{tag}_attr_{sta_corner}_{ref}_{len(rows)}", libs[sta_corner]["path"],
                                 sta_corner, netlist, sdc, spef)
                rows.append(dict(net=net, native_c_pf=native_nets[net]["c_pf"],
                                 oracle_c_pf=oracle_nets[ref][net]["c_pf"],
                                 slack_move_ns=run["endpoints"][worst["endpoint"]][f"{kind}_slack_ns"] - base_slack))
                spef.unlink()
            rows.sort(key=lambda r: -abs(r["slack_move_ns"]))
            entry[f"attribution_{ref}"] = dict(
                endpoint=worst["endpoint"], check=kind,
                measured_delta_ns=-worst[f"{kind}_slack_delta_ns"],
                sum_of_moves_ns=math.fsum(r["slack_move_ns"] for r in rows),
                sum_abs_moves_ns=math.fsum(abs(r["slack_move_ns"]) for r in rows), nets=rows)
        result["sta"][sta_corner] = entry
    # Negative controls.
    controls = {}
    scaled = h.root / f"{pdk}-rc-nom-scaledR.json"
    cs.scaled_profile(ROOT / f"fixtures/pdk/{pdk}-rc-nom.json", scaled, 3.0)
    scaled_spef = h.root / f"{pdk}_native_nom_scaledR.spef"
    cs.native_extract(h, f"{tag}_control_scaled_r", rustroad, ws, scaled, "nom", scaled_spef)
    sn = per_net_spef(scaled_spef)
    vx = compare_totals(sn, per_net_spef(rcx["nom"]))["targets"]["net_r"]
    vm = compare_totals(sn, per_net_magic(magic["nom"]))["targets"]["net_r"]
    controls["scaled_r_x3"] = dict(status="detected" if vx["status"] == "failed" and vm["status"] == "failed"
                                   else "missed", vs_openrcx=vx["distribution"], vs_magic=vm["distribution"])
    for kind in ("dropped_net", "missing_net", "dropped_connection"):
        bad = h.root / f"{pdk}_native_nom_{kind}.spef"
        detail = cs.mutate_spef(native["nom"], bad, kind)
        cov = compare_totals(per_net_spef(bad), per_net_magic(magic["nom"]))["coverage"]
        sta = cs.sta_run(h, f"{tag}_control_{kind}", libs["tt"]["path"], "tt", netlist, sdc, bad,
                         expect="run_rejected")
        controls[kind] = dict(mutation=detail, magic_coverage=cov["status"],
                              ruststa=dict(status=sta["status"], stage=sta["stage"]),
                              status="detected" if (cov["status"] == "failed" or kind == "dropped_connection")
                              and sta["status"] == "rejected" else "missed")
    planted = h.root / f"{pdk}_planted_open.gds"
    plant = magic_rc.plant_open(paths["gds"], planted, bounded_text(paths["def_"], 64 * 1024 ** 2), pdk,
                                json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-nom.json").read_text()))
    try:
        magic_rc.run_corner(paths, pdk, "nom", native["nom"], h.root / f"{pdk}_magic_planted", None,
                            gds_override=planted, resistance=False)
        verdict, reason = "missed", None
    except magic_rc.MagicError as error:
        verdict, reason = ("detected" if plant["net"] in str(error) else "missed"), str(error)
    controls["magic_planted_open"] = dict(plant=plant, status=verdict, refusal=reason)
    result["controls"] = controls
    return result


def campaign(args) -> int:
    require(args.base_rev == "empty", "REVISION_CONFLICT: a fresh campaign directory requires --base-rev empty")
    root = args.workspace.resolve()
    root.mkdir(parents=True, exist_ok=False)
    lock = (root / "workspace.lock").open("wb")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    (root / "HEAD").write_text("rev0\n")
    resource.setrlimit(resource.RLIMIT_AS, (8192 * 1024 ** 2, 8192 * 1024 ** 2))
    os.environ.update(GOMAXPROCS="2", OMP_NUM_THREADS="2")
    cs.RUSTSTA_BIN = args.sta_bin.resolve()
    h = PhysicalHarness(root, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=60,
                                                 tolerance_ns=0.001))
    start = time.monotonic()
    rustroad = args.rustroad.resolve()
    inputs = json.loads(bounded_text(args.inputs))
    summary = dict(schema="rustroad.rc-oracles.v1", status="unverified", design=inputs.get("design"),
                   targets=TARGETS, sta_corners={k: dict(liberty=k, rc=v) for k, v in cs.STA_CORNERS.items()},
                   scope=dict(block_level=True, coupling="grounded at factor 1 in timing", si="unchecked",
                              ir_em="unchecked", ocv="unchecked", cppr="off", drc="unchecked", lvs="unchecked",
                              magic_timing="per-net Magic totals back-annotated onto native RC trees"),
                   tools={}, cases=[])
    try:
        r = h.run("memory_preflight", ["free", "-h"])
        summary["memory_preflight"] = bounded_text(Path(r["stdout"]))
        bins = [("rustroad", rustroad), ("rusteda", cs.RUSTSTA_BIN / "rusteda"),
                ("ruststa", cs.RUSTSTA_BIN / "ruststa")]
        variants = {}
        for spec_ in args.variant:
            vname, _, vpath = spec_.partition("=")
            require(re.fullmatch(r"[A-Za-z0-9_-]+", vname) is not None and vpath, f"bad --variant {spec_}")
            variants[vname] = Path(vpath).resolve()
            bins.append((f"rustroad_variant_{vname}", variants[vname]))
        for name, path in bins:
            require(path.is_file(), f"missing {name}")
            r = h.run(f"{name}_version", [str(path), "--version"])
            summary["tools"][name] = {**fingerprint(path), "version": bounded_text(Path(r["stdout"])).strip()}
        summary["tools"]["openroad_image"] = cs.IMAGE
        summary["tools"]["magic_image"] = magic_rc.container.SIGNOFF_IMAGE
        for case in inputs["cases"]:
            summary["cases"].append(campaign_case(h, case, rustroad, variants))
        verdicts = []
        for c in summary["cases"]:
            for corner in CORNERS:
                for ref in ("vs_openrcx_totals", "vs_magic"):
                    for k, v in c["spef"][corner][ref]["targets"].items():
                        verdicts.append((c["pdk"], corner, ref, k, v["status"]))
            for sc in cs.STA_CORNERS:
                for ref in ("vs_openrcx", "vs_magic"):
                    verdicts.append((c["pdk"], sc, ref, "sta_slack", c["sta"][sc][ref]["status"]))
                fid = c["sta"][sc]["backannotation_fidelity"]
                verdicts.append((c["pdk"], sc, "control", "backannotation_fidelity",
                                 "passed" if max(fid["max_abs_setup_delta_ns"], fid["max_abs_hold_delta_ns"]) <= 0.001
                                 else "failed"))
            for corner, v in c["resistance_accounting"].items():
                verdicts.append((c["pdk"], corner, "independent", "resistance_accounting", v["status"]))
            verdicts.append((c["pdk"], "all", "self", "read_only", c["read_only"]["status"]))
            verdicts.append((c["pdk"], "nom", "self", "magic_determinism", c["magic"]["determinism"]["status"]))
            for k, v in c["controls"].items():
                verdicts.append((c["pdk"], "control", k, "detected",
                                 "passed" if v["status"] == "detected" else "failed"))
        summary["verdicts"] = [dict(pdk=a, corner=b, reference=r_, target=d, status=e)
                               for a, b, r_, d, e in verdicts]
        controls_ok = all(v[4] == "passed" for v in verdicts if v[2] in ("independent", "self", "control")
                          or v[1] == "control")
        targets_ok = all(v[4] == "passed" for v in verdicts if v[2] in ("vs_openrcx_totals", "vs_openrcx"))
        summary["status"] = "passed" if controls_ok and targets_ok else "partial" if controls_ok else "failed"
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary["status"] = "failed"
        summary["reason"] = f"{type(error).__name__}: {error}"
    finally:
        summary["receipts"] = h.receipts
        summary["wall_seconds"] = time.monotonic() - start
        summary["peak_rss_kib"] = max((x["peak_rss_kib"] for x in h.receipts if x["peak_rss_kib"]), default=None)
        write_json(root / "summary.json", summary)
        (root / "HEAD").write_text("rev1\n")
    print(json.dumps(dict(status=summary["status"], summary=str(root / "summary.json"),
                          reason=summary.get("reason"))))
    return {"passed": 0, "partial": 3, "unverified": 3, "failed": 1}[summary["status"]]


def report(summary_path: Path) -> str:
    d = json.loads(bounded_text(summary_path, 256 * 1024 ** 2))
    pct = lambda v: "n/a" if v is None else f"{100 * v:+.1f}%"  # noqa: E731
    ps = lambda v: f"{1e3 * v:.2f}"  # noqa: E731
    out = [f"Campaign `{d.get('design')}`: **{d['status']}**, wall {d['wall_seconds']:.1f} s, "
           f"peak child RSS {d['peak_rss_kib']} KiB.", ""]
    out += ["| PDK | RC | vs | total C | per-net C (n, min..max) | per-net R (n, min..max) |", "|---|---|---|---|---|---|"]
    for c in d["cases"]:
        for corner in CORNERS:
            for ref, label in (("vs_openrcx_totals", "OpenRCX"), ("vs_magic", "Magic"),
                               ("openrcx_vs_magic", "OpenRCX vs Magic")):
                if ref not in c["spef"][corner]:
                    continue
                t = c["spef"][corner][ref]["targets"]
                nc, nr = t["net_c"], t["net_r"]
                fmt = lambda x: f"{x['eligible']}, {pct((x['distribution'] or {}).get('min'))}..{pct((x['distribution'] or {}).get('max'))} {x['status']}"  # noqa: E731,E501
                out.append(f"| {c['pdk']} | {corner} | {label} | {pct(t['total_c']['rel'])} {t['total_c']['status']} | "
                           f"{fmt(nc)} | {fmt(nr)} |")
    out += ["", "| PDK | STA | vs | max abs setup / hold delta (ps) | verdict |", "|---|---|---|---|---|"]
    for c in d["cases"]:
        for sc, v in c["sta"].items():
            for ref, label in (("vs_openrcx", "OpenRCX"), ("vs_magic", "Magic (back-annotated)"),
                               ("openrcx_vs_magic", "OpenRCX vs Magic (both back-annotated)"),
                               ("backannotation_fidelity", "fidelity: OpenRCX back-annotated vs own SPEF")):
                if ref not in v:
                    continue
                x = v[ref]
                out.append(f"| {c['pdk']} | {sc}+{v['rc_corner']} | {label} | {ps(x['max_abs_setup_delta_ns'])} / "
                           f"{ps(x['max_abs_hold_delta_ns'])} | {x['status']} |")
    names = sorted({v for c in d["cases"] for x in c["sta"].values() for v in x.get("variants", {})})
    if names:
        out += ["", "Ablation (max abs setup/hold slack delta in ps against OpenRCX | Magic; total C against OpenRCX | Magic):",
                "", "| PDK | corner | " + " | ".join(f"{n}: OpenRCX | {n}: Magic" for n in names + ["final"]) + " |",
                "|---|---|" + "---|---|" * (len(names) + 1)]
        for c in d["cases"]:
            for sc, v in c["sta"].items():
                cell = lambda x, y: f"{1e3 * max(x['max_abs_setup_delta_ns'], x['max_abs_hold_delta_ns']):.1f} | {1e3 * max(y['max_abs_setup_delta_ns'], y['max_abs_hold_delta_ns']):.1f}"  # noqa: E731,E501
                row = [cell(v["variants"][n]["vs_openrcx"], v["variants"][n]["vs_magic"]) for n in names]
                row.append(cell(v["vs_openrcx"], v["vs_magic"]))
                out.append(f"| {c['pdk']} | {sc}+{v['rc_corner']} | " + " | ".join(row) + " |")
            for corner in CORNERS:
                e = c["spef"][corner]
                cell = lambda x, y: f"{pct(x['total_c']['rel'])} | {pct(y['total_c']['rel'])}"  # noqa: E731
                row = [cell(e["variants"][n]["vs_openrcx"], e["variants"][n]["vs_magic"]) for n in names]
                row.append(cell(e["vs_openrcx_totals"]["targets"], e["vs_magic"]["targets"]))
                out.append(f"| {c['pdk']} | C {corner} | " + " | ".join(row) + " |")
    out += ["", "| PDK | STA | vs | endpoint (check) | measured / sum of per-net moves (ps) | largest per-net moves: net (ps, native - oracle C fF) |",
            "|---|---|---|---|---|---|"]
    for c in d["cases"]:
        for sc, v in c["sta"].items():
            for ref in ("vs_openrcx", "vs_magic"):
                a = v.get(f"attribution_{ref}")
                if not a:
                    continue
                top = ", ".join(f"{r['net']} ({1e3 * r['slack_move_ns']:+.1f}, {1e3 * (r['native_c_pf'] - r['oracle_c_pf']):+.2f})"
                                for r in a["nets"][:4])
                out.append(f"| {c['pdk']} | {sc}+{v['rc_corner']} | {ref[3:]} | {a['endpoint']} ({a['check']}) | "
                           f"{1e3 * a['measured_delta_ns']:+.1f} / {1e3 * a['sum_of_moves_ns']:+.1f} | {top} |")
    out += ["", "| PDK | RC | Magic terminal element (ohm) | native pin via (ohm) | R without that element: max abs rel (n) |",
            "|---|---|---|---|---|"]
    for c in d["cases"]:
        for corner in CORNERS:
            t = c["spef"][corner]["magic_r_terminals"]
            mt = t["magic_terminal_ohm_per_terminal"]
            out.append(f"| {c['pdk']} | {corner} | {mt and f'{mt['min']:.2f}..{mt['max']:.2f}'} | "
                       f"{t['native_via_ohm_per_terminal']} | {pct(t['rest']['max_abs_rel'])} ({t['rest']['eligible']}) "
                       f"{t['rest']['status']} |")
    out += ["", "| PDK | control | result |", "|---|---|---|"]
    for c in d["cases"]:
        for k, v in c["controls"].items():
            out.append(f"| {c['pdk']} | {k} | {v['status']} |")
        for k, v in c["resistance_accounting"].items():
            out.append(f"| {c['pdk']} | independent R accounting {k} | {v['status']} ({v['max_rel_error']:.1e}) |")
        out.append(f"| {c['pdk']} | Magic determinism | {c['magic']['determinism']['status']} |")
    return "\n".join(out) + "\n"


def evidence(summary_path: Path) -> dict:
    d = json.loads(bounded_text(summary_path, 256 * 1024 ** 2))
    cases = []
    for c in d["cases"]:
        spef = {}
        for corner, e in c["spef"].items():
            spef[corner] = {k: ({x: y for x, y in v.items() if x != "nets"} if isinstance(v, dict) else v)
                            for k, v in e.items()}
            spef[corner]["vs_magic_nets"] = e["vs_magic"]["nets"]
            spef[corner]["vs_openrcx_nets"] = e["vs_openrcx_totals"]["nets"]
            spef[corner]["magic_r_terminal_nets"] = e["magic_r_terminals"]["nets"]
        cases.append(dict(pdk=c["pdk"], inputs=c["inputs"], native=c["native"], read_only=c["read_only"],
                          resistance_accounting=c["resistance_accounting"], openrcx=c["openrcx"],
                          magic=c["magic"], spef=spef,
                          sta={k: dict(rc_corner=v["rc_corner"], liberty=v["liberty"], runs=v["runs"],
                                       **{r: strip_endpoints(v[r]) for r in v if isinstance(v[r], dict)
                                          and "max_abs_setup_delta_ns" in v[r]},
                                       endpoints=v["endpoints"])
                               for k, v in c["sta"].items()},
                          controls=c["controls"]))
    return dict(schema="rustroad.rc-oracles-evidence.v1", design=d.get("design"), status=d["status"],
                reason=d.get("reason"), targets=d["targets"], sta_corners=d["sta_corners"], scope=d["scope"],
                tools=d["tools"], verdicts=d.get("verdicts"), wall_seconds=d["wall_seconds"],
                peak_child_rss_kib=d["peak_rss_kib"], receipts=len(d["receipts"]), cases=cases)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    c = sub.add_parser("campaign")
    c.add_argument("--workspace", type=Path, required=True)
    c.add_argument("--base-rev", required=True)
    c.add_argument("--inputs", type=Path, required=True)
    c.add_argument("--rustroad", type=Path, default=ROOT / "target/release/rustroad")
    c.add_argument("--variant", action="append", default=[], metavar="NAME=BIN",
                   help="other extractor binary measured on the same inputs (ablation)")
    c.add_argument("--sta-bin", type=Path, required=True)
    r = sub.add_parser("report")
    r.add_argument("--summary", type=Path, required=True)
    e = sub.add_parser("evidence")
    e.add_argument("--summary", type=Path, required=True)
    e.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "report":
        print(report(args.summary), end="")
        return 0
    if args.command == "evidence":
        write_json(args.out, evidence(args.summary))
        return 0
    return campaign(args)


if __name__ == "__main__":
    sys.exit(main())
