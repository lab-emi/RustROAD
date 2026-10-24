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
      [--rustroad BIN] [--variant NAME=BIN[:legacy|:profiles=DIR] ...] --sta-bin DIR
  rc_oracles.py report --summary OUT/summary.json
  rc_oracles.py evidence --summary OUT/summary.json --out docs/evidence/X.json

A ``:legacy`` variant (an extractor from before the overlap-area profile
fields) is given profiles without those fields; every other profile field is
unchanged since e0d074d apart from the intentional changes that
``build_rc_profile.py --compare-ref e0d074d`` lists. A ``:profiles=DIR``
variant reads ``DIR/<pdk>-rc-<corner>.json`` instead of the tracked profiles
(for example the profiles of an earlier revision, to measure a data-source
decision with the same binary).

Black-box controls on OpenRCX: the same DEF with ``-coupling_threshold 0``
(every coupling explicit: does grounding sub-threshold couplings change the
design total?), with the cell LEF stripped of OBS (are cell obstructions
extracted?) and with every signal pin's metal shrunk to a 10 nm square (does
any net lose capacitance, i.e. is cell pin metal a partner of other nets?). The threshold-0 SPEF is also timed: it keeps each
coupling in both nets, as the native SPEF does, so it is the like-for-like
OpenRCX configuration for per-net C (the 0.1 fF flow setting grounds a small
coupling in one net and drops it from the other). Its verdicts are reported
next to the flow configuration's; the campaign status is unchanged and uses
the flow configuration.

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
# Cell-pin control: a coupling to another net's pin of the size the native
# extractor and Magic find next to routed wires (0.1-0.3 fF per net pair on
# pipeline.v) would show as that net losing capacitance when the pins shrink.
PIN_PARTNER_BOUND_PF = 1e-4
# Back-annotation fidelity control: OpenRCX's per-net totals on the native
# trees must time within this of OpenRCX's own SPEF.
FIDELITY_BOUND_NS = 0.001
# Attribution runs one STA per net; larger designs cap it at the nets with
# the largest |native - oracle| capacitance (--attribution-max-nets).
ATTRIBUTION_MAX_NETS = None


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


NEW_PROFILE_FIELDS = ("area_cap_pf_per_um2", "overlap_area")


def legacy_profile(src: Path, dst: Path) -> Path:
    """The profile without this round's overlap-area fields (for extractors
    that reject unknown fields); all other content is unchanged."""
    p = json.loads(bounded_text(src))
    for layer in p["layers"]:
        for key in NEW_PROFILE_FIELDS:
            layer.pop(key, None)
    p["model"].pop("overlap_area", None)
    p["sources"] = [x for x in p["sources"] if x["role"] != "nominal_calibration_table_for_overlap_corner_ratio"]
    dst.write_text(json.dumps(p, indent=1) + "\n")
    return dst


def strip_lef_obs(src: Path, dst: Path) -> int:
    """Copy a cell LEF without its OBS blocks; returns the number removed."""
    out, skip, removed = [], False, 0
    for line in bounded_text(src, 64 * 1024 ** 2).splitlines(keepends=True):
        token = line.strip()
        if not skip and token == "OBS":
            skip, removed = True, removed + 1
            continue
        if skip:
            skip = token != "END"
            continue
        out.append(line)
    dst.write_text("".join(out))
    return removed


def shrink_lef_pins(src: Path, dst: Path) -> int:
    """Copy a cell LEF with every RECT of every signal PIN (not POWER/GROUND)
    replaced by a 10 nm square at its centre; OBS and supply pins unchanged.
    Returns the number of rectangles shrunk."""
    out, in_pin, supply, shrunk = [], False, False, 0
    for line in bounded_text(src, 64 * 1024 ** 2).splitlines(keepends=True):
        t = line.split()
        if t[:1] == ["PIN"]:
            in_pin, supply = True, False
        elif in_pin and t[:1] == ["USE"] and len(t) > 1 and t[1].rstrip(";") in ("POWER", "GROUND"):
            supply = True
        elif in_pin and t[:1] == ["END"] and len(t) == 2:
            in_pin = False
        if in_pin and not supply and t[:1] == ["RECT"] and len(t) >= 5:
            x0, y0, x1, y1 = map(float, t[1:5])
            cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
            indent = line[: len(line) - len(line.lstrip())]
            line = f"{indent}RECT {cx - 0.005:.4f} {cy - 0.005:.4f} {cx + 0.005:.4f} {cy + 0.005:.4f} ;\n"
            shrunk += 1
        out.append(line)
    dst.write_text("".join(out))
    return shrunk


def open_loops(src: Path, dst: Path) -> dict:
    """Copy a SPEF for RustSTA (which rejects RC meshes), dropping every *RES
    record that closes a loop in its net (union-find in record order; the
    remaining records are renumbered). Capacitance records are unchanged."""
    out, parent, section, kept = [], {}, None, 0
    removed, nets = [], set()
    net = None

    def find(x):
        while parent.setdefault(x, x) != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x
    for line in bounded_text(src, cs.MAX_SPEF_BYTES).splitlines(keepends=True):
        tokens = line.split()
        key = tokens[0] if tokens else ""
        if key == "*D_NET":
            net, parent, section, kept = tokens[1], {}, None, 0
        elif key in ("*CONN", "*CAP", "*RES", "*END"):
            section = key
        elif section == "*RES" and len(tokens) == 4:
            a, b = find(tokens[1]), find(tokens[2])
            if a == b:
                removed.append(dict(net=net, a=tokens[1], b=tokens[2], ohm=float(tokens[3])))
                nets.add(net)
                continue
            parent[a] = b
            kept += 1
            if tokens[0] != str(kept):
                line = f"{kept} {tokens[1]} {tokens[2]} {tokens[3]}\n"
        out.append(line)
    dst.write_text("".join(out))
    return dict(resistors_removed=len(removed), nets=len(nets), removed=removed[:32],
                max_removed_ohm=max((r["ohm"] for r in removed), default=0.0))


def design_total(spef: dict) -> float:
    return math.fsum(v["c_pf"] - v["coupling_pf"] / 2.0 for v in spef.values())


def position(value: float, a: float, b: float) -> str:
    lo, hi = min(a, b), max(a, b)
    return "between" if lo <= value <= hi else "below_both" if value < lo else "above_both"


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


def sta(h, tag, lib, corner, netlist, sdc, spef, expect="ok") -> dict:
    """RustSTA run whose workspace (about 35 MiB of imported Liberty) is removed
    once its reports are parsed; the receipts keep every command and output."""
    try:
        return cs.sta_run(h, tag, lib, corner, netlist, sdc, spef, expect=expect)
    finally:
        shutil.rmtree(h.root / f"{tag}_sta", ignore_errors=True)


def sta_set(h, tag, lib, corner, netlist, sdc, spefs: dict) -> dict:
    return {label: sta(h, f"{tag}_{label}", lib, corner, netlist, sdc, spef) for label, spef in spefs.items()}


def strip_endpoints(delta: dict) -> dict:
    return {k: v for k, v in delta.items() if k != "endpoints"}


def campaign_case(h, case: dict, rustroad: Path, variants: dict, legacy: set, profile_dirs: dict) -> dict:
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
            profile = ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json"
            if vname in profile_dirs:
                profile = profile_dirs[vname] / f"{pdk}-rc-{corner}.json"
            if vname in legacy:
                profile = legacy_profile(profile, h.root / f"{pdk}-rc-{corner}-legacy.json")
            cs.native_extract(h, f"{tag}_variant_{vname}_{corner}", vbin, ws, profile, corner, out)
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
    # Black-box OpenRCX controls: threshold 0 (all couplings explicit) and a
    # cell LEF without OBS.
    rcx0 = {}
    result["openrcx_threshold0"] = {}
    for corner in CORNERS:
        spef, info = cs.openrcx(h, f"{tag}_rcx0", pdk, corner, paths["def_"], f"{pdk}_rcx0_{corner}",
                                threshold="0")
        rcx0[corner] = spef
        a, b = per_net_spef(rcx[corner]), per_net_spef(spef)
        ta, tb = design_total(a), design_total(b)
        result["openrcx_threshold0"][corner] = dict(
            spef=info["spef"], design_total_pf=dict(threshold_0_1=ta, threshold_0=tb), design_total_rel=cs.rel(ta, tb),
            per_net_sum_rel=cs.rel(math.fsum(v["c_pf"] for v in a.values()), math.fsum(v["c_pf"] for v in b.values())),
            max_abs_net_c_rel=max(abs(cs.rel(a[n]["c_pf"], b[n]["c_pf"]) or 0.0) for n in a),
            status="passed" if abs(cs.rel(ta, tb) or 0.0) <= 1e-6 else "failed",
            meaning="passed: grounding a sub-threshold coupling keeps the design total (it is grounded in one "
                    "net, not both)")
    stripped = h.root / f"{pdk}_cells_without_obs.lef"
    removed = strip_lef_obs(cs.PDK_ROOT / spec["base"] / spec["lef"], stripped)
    spef, _ = cs.openrcx(h, f"{tag}_rcx_noobs", pdk, "nom", paths["def_"], f"{pdk}_rcx_noobs_nom", cells_lef=stripped)
    a, b = per_net_spef(rcx["nom"]), per_net_spef(spef)
    moved = max(abs(a[n]["c_pf"] - b[n]["c_pf"]) for n in a)
    result["openrcx_obs_blind"] = dict(obs_blocks_removed=removed, max_abs_net_c_change_pf=moved,
                                       status="passed" if moved == 0.0 and removed > 0 else "failed",
                                       meaning="passed: OpenRCX gives identical per-net C without the cell OBS, "
                                               "i.e. it does not extract cell obstructions (native and Magic do)")
    dots = h.root / f"{pdk}_cells_pin_dots.lef"
    shrunk = shrink_lef_pins(cs.PDK_ROOT / spec["base"] / spec["lef"], dots)
    spef, _ = cs.openrcx(h, f"{tag}_rcx_pindots", pdk, "nom", paths["def_"], f"{pdk}_rcx_pindots_nom",
                         cells_lef=dots)
    b = per_net_spef(spef)
    lost = max((a[n]["c_pf"] - b[n]["c_pf"] for n in a), default=0.0)
    gained = max((b[n]["c_pf"] - a[n]["c_pf"] for n in a), default=0.0)
    moved_r = max(abs(a[n]["r_ohm"] - b[n]["r_ohm"]) for n in a)
    result["openrcx_pin_blind"] = dict(
        signal_pin_rects_shrunk=shrunk, max_net_c_decrease_pf=max(lost, 0.0), max_net_c_increase_pf=max(gained, 0.0),
        nets_c_decreased=sum(1 for n in a if b[n]["c_pf"] < a[n]["c_pf"]),
        nets_c_increased=sum(1 for n in a if b[n]["c_pf"] > a[n]["c_pf"]), max_abs_net_r_change_ohm=moved_r,
        bound_pf=PIN_PARTNER_BOUND_PF,
        status="passed" if lost <= PIN_PARTNER_BOUND_PF and shrunk > 0 else "failed",
        meaning="passed: no net loses more than the bound when every signal pin shrinks to a 10 nm square, i.e. "
                "OpenRCX does not count cell pin metal as a coupling or ground partner of any net (native and "
                "Magic do). Increases (and R changes) come from a net's own wire metal over its own pin, which "
                "OpenRCX merges into the pin while the pin is full size")
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
                     vs_openrcx_threshold0_totals=compare_totals(n, per_net_spef(rcx0[corner]))["targets"],
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
    # RustSTA rejects RC meshes: an OpenRCX SPEF with loop-closing resistors is
    # timed with those resistors opened (counted; capacitance unchanged).
    rcx_sta, rcx0_sta = {}, {}
    for corner in CORNERS:
        rcx_sta[corner] = h.root / f"{pdk}_rcx_tree_{corner}.spef"
        result["openrcx"][corner]["loops_opened_for_sta"] = open_loops(rcx[corner], rcx_sta[corner])
        rcx0_sta[corner] = h.root / f"{pdk}_rcx0_tree_{corner}.spef"
        result["openrcx_threshold0"][corner]["loops_opened_for_sta"] = open_loops(rcx0[corner], rcx0_sta[corner])
    for sta_corner, rc in cs.STA_CORNERS.items():
        spefs = dict(native=native[rc], openrcx=rcx_sta[rc], openrcx_threshold0=rcx0_sta[rc],
                     magic_ba=magic_ba[rc], openrcx_ba=rcx_ba[rc])
        for vname in old:
            spefs[f"variant_{vname}"] = old[vname][rc]
        runs = sta_set(h, f"{tag}_{sta_corner}", libs[sta_corner]["path"], sta_corner, netlist, sdc, spefs)
        entry = dict(liberty=libs[sta_corner]["source"], rc_corner=rc,
                     runs={k: {x: v[x] for x in ("scope", "metrics", "completeness", "diagnostics", "peak_rss_kib")}
                           for k, v in runs.items()},
                     vs_openrcx=cs.sta_delta(runs["native"], runs["openrcx"]),
                     vs_openrcx_threshold0=cs.sta_delta(runs["native"], runs["openrcx_threshold0"]),
                     openrcx_threshold_effect=cs.sta_delta(runs["openrcx"], runs["openrcx_threshold0"]),
                     vs_magic=cs.sta_delta(runs["native"], runs["magic_ba"]),
                     openrcx_vs_magic=cs.sta_delta(runs["openrcx_ba"], runs["magic_ba"]),
                     backannotation_fidelity=cs.sta_delta(runs["openrcx_ba"], runs["openrcx"]),
                     endpoints={k: v["endpoints"] for k, v in runs.items()})
        entry["variants"] = {vname: dict(vs_openrcx=strip_endpoints(cs.sta_delta(runs[f"variant_{vname}"],
                                                                                   runs["openrcx"])),
                                         vs_openrcx_threshold0=strip_endpoints(cs.sta_delta(
                                             runs[f"variant_{vname}"], runs["openrcx_threshold0"])),
                                         vs_magic=strip_endpoints(cs.sta_delta(runs[f"variant_{vname}"],
                                                                               runs["magic_ba"])))
                             for vname in old}
        # Attribution where the slack target is missed: set one net at a time
        # to the oracle's per-net totals on the native trees and record the
        # move of the worst endpoint (first-order, per net).
        oracle_nets = dict(vs_openrcx=per_net_spef(rcx[rc]), vs_openrcx_threshold0=per_net_spef(rcx0[rc]),
                           vs_magic=per_net_magic(magic[rc]))
        other = dict(vs_openrcx="vs_magic", vs_openrcx_threshold0="vs_magic", vs_magic="vs_openrcx")
        native_nets = per_net_spef(native[rc])
        for ref, delta in (("vs_openrcx", entry["vs_openrcx"]),
                           ("vs_openrcx_threshold0", entry["vs_openrcx_threshold0"]),
                           ("vs_magic", entry["vs_magic"])):
            if delta["status"] == "passed":
                continue
            worst = max(delta["endpoints"], key=lambda r: max(abs(r["setup_slack_delta_ns"]),
                                                               abs(r["hold_slack_delta_ns"])))
            kind = "setup" if abs(worst["setup_slack_delta_ns"]) >= abs(worst["hold_slack_delta_ns"]) else "hold"
            base_slack = runs["native"]["endpoints"][worst["endpoint"]][f"{kind}_slack_ns"]
            rows = []
            candidates = sorted(native_nets)
            if ATTRIBUTION_MAX_NETS is not None and len(candidates) > ATTRIBUTION_MAX_NETS:
                candidates = sorted(sorted(candidates, key=lambda n: -abs(native_nets[n]["c_pf"]
                                                                          - oracle_nets[ref][n]["c_pf"]))
                                    [:ATTRIBUTION_MAX_NETS])
            for net in candidates:
                one = {n: dict(native_nets[n]) for n in native_nets}
                one[net] = oracle_nets[ref][net]
                spef = h.root / f"{pdk}_attr_{sta_corner}_{ref}_{len(rows)}.spef"
                magic_rc.backannotate(native[rc], one, spef)
                run = sta(h, f"{tag}_attr_{sta_corner}_{ref}_{len(rows)}", libs[sta_corner]["path"],
                          sta_corner, netlist, sdc, spef)
                o1, o2 = oracle_nets[ref][net]["c_pf"], oracle_nets[other[ref]][net]["c_pf"]
                rows.append(dict(net=net, native_c_pf=native_nets[net]["c_pf"], oracle_c_pf=o1,
                                 other_oracle_c_pf=o2, position=position(native_nets[net]["c_pf"], o1, o2),
                                 slack_move_ns=run["endpoints"][worst["endpoint"]][f"{kind}_slack_ns"] - base_slack))
                spef.unlink()
            rows.sort(key=lambda r: -abs(r["slack_move_ns"]))
            entry[f"attribution_{ref}"] = dict(
                endpoint=worst["endpoint"], check=kind, attributed_nets=len(rows), nets_total=len(native_nets),
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
        rejected = sta(h, f"{tag}_control_{kind}", libs["tt"]["path"], "tt", netlist, sdc, bad,
                       expect="run_rejected")
        controls[kind] = dict(mutation=detail, magic_coverage=cov["status"],
                              ruststa=dict(status=rejected["status"], stage=rejected["stage"]),
                              status="detected" if (cov["status"] == "failed" or kind == "dropped_connection")
                              and rejected["status"] == "rejected" else "missed")
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
    global ATTRIBUTION_MAX_NETS
    ATTRIBUTION_MAX_NETS = args.attribution_max_nets
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
        variants, legacy, profile_dirs = {}, set(), {}
        for spec_ in args.variant:
            vname, _, vpath = spec_.partition("=")
            if vpath.endswith(":legacy"):
                vpath = vpath[: -len(":legacy")]
                legacy.add(vname)
            vpath, _, pdir = vpath.partition(":profiles=")
            if pdir:
                profile_dirs[vname] = Path(pdir).resolve()
                for pdk_ in cs.PDKS:
                    for corner_ in CORNERS:
                        require((profile_dirs[vname] / f"{pdk_}-rc-{corner_}.json").is_file(),
                                f"variant {vname}: profile {pdk_}-rc-{corner_}.json missing in {pdir}")
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
            summary["cases"].append(campaign_case(h, case, rustroad, variants, legacy, profile_dirs))
        summary["variant_profiles"] = {k: {f.name: fingerprint(f) for f in sorted(v.glob("*-rc-*.json"))}
                                       for k, v in profile_dirs.items()}
        verdicts = []
        for c in summary["cases"]:
            for corner in CORNERS:
                for ref in ("vs_openrcx_totals", "vs_magic"):
                    for k, v in c["spef"][corner][ref]["targets"].items():
                        verdicts.append((c["pdk"], corner, ref, k, v["status"]))
            for sc in cs.STA_CORNERS:
                for ref in ("vs_openrcx", "vs_openrcx_threshold0", "vs_magic"):
                    verdicts.append((c["pdk"], sc, ref, "sta_slack", c["sta"][sc][ref]["status"]))
                fid = c["sta"][sc]["backannotation_fidelity"]
                verdicts.append((c["pdk"], sc, "control", "backannotation_fidelity",
                                 "passed" if max(fid["max_abs_setup_delta_ns"], fid["max_abs_hold_delta_ns"])
                                 <= FIDELITY_BOUND_NS else "failed"))
            for corner, v in c["resistance_accounting"].items():
                verdicts.append((c["pdk"], corner, "independent", "resistance_accounting", v["status"]))
            verdicts.append((c["pdk"], "all", "self", "read_only", c["read_only"]["status"]))
            for corner, v in c["openrcx_threshold0"].items():
                verdicts.append((c["pdk"], corner, "independent", "openrcx_threshold0_design_total", v["status"]))
            verdicts.append((c["pdk"], "nom", "independent", "openrcx_obs_blind", c["openrcx_obs_blind"]["status"]))
            verdicts.append((c["pdk"], "nom", "independent", "openrcx_pin_blind", c["openrcx_pin_blind"]["status"]))
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


STA_ROWS = (("vs_openrcx", "OpenRCX (flow, 0.1 fF threshold)"),
            ("vs_openrcx_threshold0", "OpenRCX (threshold 0, lossless)"),
            ("openrcx_threshold_effect", "OpenRCX 0.1 vs threshold 0 (information)"),
            ("vs_magic", "Magic (back-annotated)"),
            ("openrcx_vs_magic", "OpenRCX vs Magic (both back-annotated, information)"),
            ("backannotation_fidelity", "fidelity: OpenRCX back-annotated vs own SPEF"))


def sta_rows(case: dict) -> list[str]:
    """Report rows of one case's slack comparisons; each row shows the bound
    its verdict uses (the fidelity control has its own, FIDELITY_BOUND_NS)."""
    ps = lambda v: f"{1e3 * v:.2f}"  # noqa: E731
    out = []
    for sc, v in case["sta"].items():
        for ref, label in STA_ROWS:
            if ref not in v:
                continue
            x = v[ref]
            worst = max(x["max_abs_setup_delta_ns"], x["max_abs_hold_delta_ns"])
            if ref == "backannotation_fidelity":
                bound, status = FIDELITY_BOUND_NS, "passed" if worst <= FIDELITY_BOUND_NS else "failed"
            elif ref in ("openrcx_threshold_effect", "openrcx_vs_magic"):
                bound, status = None, "information"
            else:
                bound, status = x["bound_ns"], x["status"]
            out.append(f"| {case['pdk']} | {sc}+{v['rc_corner']} | {label} | {ps(x['max_abs_setup_delta_ns'])} / "
                       f"{ps(x['max_abs_hold_delta_ns'])} | {'-' if bound is None else ps(bound)} | {status} |")
    return out


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
    out += ["", "| PDK | STA | vs | max abs setup / hold delta (ps) | bound (ps) | verdict |",
            "|---|---|---|---|---|---|"]
    for c in d["cases"]:
        out += sta_rows(c)
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
            for ref in ("vs_openrcx", "vs_openrcx_threshold0", "vs_magic"):
                a = v.get(f"attribution_{ref}")
                if not a:
                    continue
                top = ", ".join(f"{r['net']} ({1e3 * r['slack_move_ns']:+.1f}, {1e3 * (r['native_c_pf'] - r['oracle_c_pf']):+.2f}"
                                f"{', ' + r['position'].replace('_', ' ') if 'position' in r else ''})"
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
    if all("openrcx_threshold0" in c for c in d["cases"]):
        out += ["", "| PDK | RC | OpenRCX design total, threshold 0.1 / 0 fF (pF) | per-net sum change | "
                "total C vs OpenRCX threshold 0 | per-net C vs OpenRCX threshold 0 |", "|---|---|---|---|---|---|"]
        for c in d["cases"]:
            for corner in CORNERS:
                t0 = c["openrcx_threshold0"][corner]
                t = c["spef"][corner]["vs_openrcx_threshold0_totals"]
                dt = t0["design_total_pf"]
                out.append(f"| {c['pdk']} | {corner} | {dt['threshold_0_1']:.6f} / {dt['threshold_0']:.6f} "
                           f"({t0['status']}) | {pct(t0['per_net_sum_rel'])} | {pct(t['total_c']['rel'])} "
                           f"{t['total_c']['status']} | {pct((t['net_c']['distribution'] or {}).get('min'))}.."
                           f"{pct((t['net_c']['distribution'] or {}).get('max'))} {t['net_c']['status']} |")
    out += ["", "| PDK | control | result |", "|---|---|---|"]
    for c in d["cases"]:
        for k, v in c["controls"].items():
            out.append(f"| {c['pdk']} | {k} | {v['status']} |")
        if "openrcx_obs_blind" in c:
            v = c["openrcx_obs_blind"]
            out.append(f"| {c['pdk']} | OpenRCX without cell OBS ({v['obs_blocks_removed']} blocks): max per-net "
                       f"change {v['max_abs_net_c_change_pf']:.1e} pF | {v['status']} |")
        if "openrcx_pin_blind" in c:
            v = c["openrcx_pin_blind"]
            out.append(f"| {c['pdk']} | OpenRCX with {v['signal_pin_rects_shrunk']} signal pin rectangles shrunk to "
                       f"10 nm: largest per-net C decrease {v['max_net_c_decrease_pf']:.1e} pF "
                       f"({v['nets_c_decreased']} nets, bound {v['bound_pf']:.0e}), increase "
                       f"{v['max_net_c_increase_pf']:.1e} pF ({v['nets_c_increased']} nets) | {v['status']} |")
        for k, v in c["resistance_accounting"].items():
            out.append(f"| {c['pdk']} | independent R accounting {k} | {v['status']} ({v['max_rel_error']:.1e}) |")
        out.append(f"| {c['pdk']} | Magic determinism | {c['magic']['determinism']['status']} |")
    return "\n".join(out) + "\n"


def evidence(summary_path: Path, compact: bool = False) -> dict:
    """Evidence record of a campaign; ``compact`` leaves out the per-net and
    per-endpoint tables (larger designs)."""
    d = json.loads(bounded_text(summary_path, 256 * 1024 ** 2))
    cases = []
    for c in d["cases"]:
        spef = {}
        for corner, e in c["spef"].items():
            spef[corner] = {k: ({x: y for x, y in v.items() if x != "nets"} if isinstance(v, dict) else v)
                            for k, v in e.items()}
            if not compact:
                spef[corner]["vs_magic_nets"] = e["vs_magic"]["nets"]
                spef[corner]["vs_openrcx_nets"] = e["vs_openrcx_totals"]["nets"]
                spef[corner]["magic_r_terminal_nets"] = e["magic_r_terminals"]["nets"]
        cases.append(dict(pdk=c["pdk"], inputs=c["inputs"], native=c["native"], read_only=c["read_only"],
                          resistance_accounting=c["resistance_accounting"], openrcx=c["openrcx"],
                          magic=c["magic"], spef=spef,
                          openrcx_threshold0=c.get("openrcx_threshold0"),
                          openrcx_obs_blind=c.get("openrcx_obs_blind"),
                          openrcx_pin_blind=c.get("openrcx_pin_blind"),
                          sta={k: dict(rc_corner=v["rc_corner"], liberty=v["liberty"], runs=v["runs"],
                                       **{r: strip_endpoints(v[r]) for r in v if isinstance(v[r], dict)
                                          and "max_abs_setup_delta_ns" in v[r]},
                                       **({} if compact else dict(endpoints=v["endpoints"])))
                               for k, v in c["sta"].items()},
                          attribution={k: {r: v[r] for r in v if r.startswith("attribution_")}
                                       for k, v in c["sta"].items()},
                          controls=c["controls"]))
    return dict(schema="rustroad.rc-oracles-evidence.v1", compact=compact, design=d.get("design"), status=d["status"],
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
    c.add_argument("--variant", action="append", default=[], metavar="NAME=BIN[:legacy|:profiles=DIR]",
                   help="other extractor binary measured on the same inputs (ablation); :legacy gives it "
                        "profiles without the overlap-area fields")
    c.add_argument("--sta-bin", type=Path, required=True)
    c.add_argument("--attribution-max-nets", type=int, default=None,
                   help="attribute a missed slack target only over this many nets (largest C difference first)")
    r = sub.add_parser("report")
    r.add_argument("--summary", type=Path, required=True)
    e = sub.add_parser("evidence")
    e.add_argument("--summary", type=Path, required=True)
    e.add_argument("--out", type=Path, required=True)
    e.add_argument("--compact", action="store_true", help="leave out per-net and per-endpoint tables")
    args = parser.parse_args()
    if args.command == "report":
        print(report(args.summary), end="")
        return 0
    if args.command == "evidence":
        write_json(args.out, evidence(args.summary, args.compact))
        return 0
    return campaign(args)


if __name__ == "__main__":
    sys.exit(main())
