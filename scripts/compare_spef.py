#!/usr/bin/env python3
"""Native RustROAD extraction vs OpenRCX (black box) at min/nom/max on both PDKs.

The three-way campaign (native, OpenRCX and Magic, with ablation and per-net
attribution) is scripts/rc_oracles.py; it reuses the SPEF reader, the OpenRCX
and RustSTA runners and the controls defined here. ``unmodeled_metal_estimate``
below predates the via-pad and end-extension model and is kept only for the
historical campaign.

Subcommands:
  spef      --native A.spef --reference B.spef        per-net C/R and totals vs targets
  merge-def --def IN.def --routes ROUTES.json --out OUT.def
            geometry-identical DEF whose collinear lattice edges are merged
  campaign  --workspace DIR --base-rev empty [--inputs DIR]
            full guarded run: native extraction (determinism, read-only), OpenRCX
            on the harness DEF and on the merged DEF, RustSTA endpoint slack with
            each SPEF at tt+nom, ss+max and ff+min, and negative controls.

Targets are fixed before measuring (docs/extraction.md):
  total design wire C within +/-10 %; per-net total C within +/-20 % for nets
  with reference C >= 5 fF; per-net total R within +/-20 % for nets with
  reference R >= 20 ohm; RustSTA endpoint setup/hold slack delta <= 10 ps.

OpenRCX, OpenSTA-free RustSTA and the PDK files are used strictly as black-box
executables and data. No reference source is read; no coefficient of the
native model is derived from reference output.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
import fcntl
import hashlib
import heapq
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from verify_logic import bounded_text, ideal_projection, sha256, write_json  # noqa: E402
from verify_physical import IMAGE, OPENROAD, PhysicalHarness, fingerprint, require  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
PDK_ROOT = Path("/home/cgao/.local/share/containers/storage/overlay/"
                "a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel")
VERSION = "f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7"
import subprocess  # noqa: E402


def main_checkout() -> Path:
    """Root of the primary checkout (worktrees share its .work evidence)."""
    common = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--path-format=absolute",
                             "--git-common-dir"], check=True, capture_output=True, text=True).stdout.strip()
    return Path(common).parent


READY_DEFAULT = main_checkout() / ".work/rc-timing/ready"
RUSTSTA_BIN = Path("/mnt/data2/cgao/git/RustSTA/.work/rc-wave/target/release")
STA_REPORT_LIMIT = 100_000
MAX_SPEF_BYTES = 32 * 1024 * 1024
MAX_SPEF_NETS = 100_000
MAX_SPEF_LINES = 4_000_000
TARGETS = dict(total_c_rel=0.10, net_c_rel=0.20, net_c_min_pf=0.005, net_r_rel=0.20,
               net_r_min_ohm=20.0, slack_delta_ns=0.010)
# STA corner -> (Liberty corner, RC corner)
STA_CORNERS = dict(tt="nom", ss="max", ff="min")
PDKS = {
    "sky130": dict(
        base=f"sky130/versions/{VERSION}/sky130A",
        tlef="libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef",
        lef="libs.ref/sky130_fd_sc_hd/lef/sky130_fd_sc_hd.lef",
        lib="libs.ref/sky130_fd_sc_hd/lib/sky130_fd_sc_hd__{}.lib",
        libs=dict(tt="tt_025C_1v80", ss="ss_100C_1v60", ff="ff_n40C_1v95"),
        rules="libs.tech/librelane/rules.openrcx.sky130A.{}.calibre",
    ),
    "gf180": dict(
        base=f"gf180mcu/versions/{VERSION}/gf180mcuD",
        tlef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef",
        lef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef",
        lib="libs.ref/gf180mcu_fd_sc_mcu7t5v0/lib/gf180mcu_fd_sc_mcu7t5v0__{}.lib",
        libs=dict(tt="tt_025C_3v30", ss="ss_125C_3v00", ff="ff_n40C_3v60"),
        rules="libs.tech/librelane/rules.openrcx.gf180mcuD.{}",
    ),
}


# ----------------------------------------------------------------------------
# SPEF reading (bounded, detailed nets only)
# ----------------------------------------------------------------------------
def parse_spef(path: Path) -> dict:
    require(path.is_file() and path.stat().st_size <= MAX_SPEF_BYTES, f"SPEF missing or over cap: {path}")
    lines = bounded_text(path, MAX_SPEF_BYTES).splitlines()
    require(len(lines) <= MAX_SPEF_LINES, "SPEF line budget")
    cscale = rscale = None
    names, ports, nets = {}, {}, {}
    section, net = None, None

    def resolve(token: str) -> str:
        if token.startswith("*"):
            head, sep, tail = token.partition(":")
            require(head in names, f"unmapped SPEF name {head}")
            return names[head] + (sep + tail if sep else "")
        return token

    for raw in lines:
        line = raw.strip()
        if not line or line.startswith("//"):
            continue
        tokens = line.split()
        key = tokens[0]
        if key == "*C_UNIT":
            cscale = float(tokens[1]) * {"PF": 1.0, "FF": 1e-3}[tokens[2]]
        elif key == "*R_UNIT":
            rscale = float(tokens[1]) * {"OHM": 1.0, "KOHM": 1e3}[tokens[2]]
        elif key in ("*NAME_MAP", "*PORTS"):
            section = key
        elif key == "*D_NET":
            require(cscale is not None and rscale is not None, "SPEF units missing")
            require(len(nets) < MAX_SPEF_NETS, "SPEF net budget")
            name = resolve(tokens[1])
            require(name not in nets, f"duplicate D_NET {name}")
            net = dict(declared=float(tokens[2]) * cscale, conn=[], ground={}, coupling=[], res=[])
            nets[name] = net
            section = "*D_NET"
        elif key in ("*CONN", "*CAP", "*RES"):
            section = key
        elif key == "*END":
            net, section = None, None
        elif section == "*NAME_MAP" and key.startswith("*"):
            names[key] = tokens[1]
        elif section == "*PORTS" and not key.startswith("*"):
            ports[tokens[0]] = tokens[1]
        elif section == "*CONN" and key in ("*P", "*I"):
            net["conn"].append((key[1], resolve(tokens[1]), tokens[2]))
        elif section == "*CAP":
            if len(tokens) == 3:
                node = resolve(tokens[1])
                net["ground"][node] = net["ground"].get(node, 0.0) + float(tokens[2]) * cscale
            elif len(tokens) == 4:
                net["coupling"].append((resolve(tokens[1]), resolve(tokens[2]), float(tokens[3]) * cscale))
            else:
                require(False, f"unsupported *CAP record: {line}")
        elif section == "*RES":
            require(len(tokens) == 4, f"unsupported *RES record: {line}")
            net["res"].append((resolve(tokens[1]), resolve(tokens[2]), float(tokens[3]) * rscale))
    return dict(nets=nets, ports=ports)


def net_metrics(name: str, net: dict) -> dict:
    ground = sum(net["ground"].values())
    coupling = sum(c for _, _, c in net["coupling"])
    total_r = sum(r for _, _, r in net["res"])
    graph = defaultdict(list)
    for a, b, r in net["res"]:
        graph[a].append((b, r))
        graph[b].append((a, r))
    drivers = [n for kind, n, d in net["conn"] if (kind == "P" and d == "I") or (kind == "I" and d == "O")]
    sinks = sorted(n for kind, n, d in net["conn"] if n not in drivers)
    path_r = {}
    if len(drivers) == 1:
        dist = {drivers[0]: 0.0}
        heap = [(0.0, drivers[0])]
        while heap:
            d, u = heapq.heappop(heap)
            if d > dist.get(u, math.inf):
                continue
            for v, r in graph[u]:
                if d + r < dist.get(v, math.inf):
                    dist[v] = d + r
                    heapq.heappush(heap, (d + r, v))
        path_r = {s: dist.get(s) for s in sinks}
    nodes = set(net["ground"]) | set(graph)
    return dict(net=name, total_c_pf=ground + coupling, ground_pf=ground, coupling_pf=coupling,
                declared_pf=net["declared"], total_r_ohm=total_r, nodes=len(nodes),
                resistors=len(net["res"]), mesh=len(net["res"]) >= len(nodes) and len(nodes) > 0,
                drivers=drivers, sink_path_r_ohm=path_r,
                conn=sorted((kind, n, d) for kind, n, d in net["conn"]))


def rel(a: float, b: float) -> float | None:
    return None if b == 0 else (a - b) / b


def compare_spefs(native_path: Path, reference_path: Path) -> dict:
    a, b = parse_spef(native_path), parse_spef(reference_path)
    na = {k: net_metrics(k, v) for k, v in a["nets"].items()}
    nb = {k: net_metrics(k, v) for k, v in b["nets"].items()}
    only_native, only_reference = sorted(set(na) - set(nb)), sorted(set(nb) - set(na))
    rows, conn_mismatch = [], []
    for name in sorted(set(na) & set(nb)):
        x, y = na[name], nb[name]
        if x["conn"] != y["conn"]:
            conn_mismatch.append(name)
        sink_rel = [rel(x["sink_path_r_ohm"].get(s) or 0.0, r) for s, r in y["sink_path_r_ohm"].items()
                    if r and x["sink_path_r_ohm"].get(s) is not None]
        rows.append(dict(net=name, native_c_pf=x["total_c_pf"], reference_c_pf=y["total_c_pf"],
                         c_rel=rel(x["total_c_pf"], y["total_c_pf"]),
                         native_ground_pf=x["ground_pf"], reference_ground_pf=y["ground_pf"],
                         native_coupling_pf=x["coupling_pf"], reference_coupling_pf=y["coupling_pf"],
                         native_r_ohm=x["total_r_ohm"], reference_r_ohm=y["total_r_ohm"],
                         r_rel=rel(x["total_r_ohm"], y["total_r_ohm"]),
                         worst_sink_path_r_rel=max(sink_rel, key=abs) if sink_rel else None,
                         native_nodes=x["nodes"], reference_nodes=y["nodes"],
                         native_mesh=x["mesh"], reference_mesh=y["mesh"]))

    def design(nets):
        ground = sum(m["ground_pf"] for m in nets.values())
        coupling = sum(m["coupling_pf"] for m in nets.values()) / 2.0
        return dict(ground_pf=ground, coupling_pf=coupling, total_pf=ground + coupling,
                    resistance_ohm=sum(m["total_r_ohm"] for m in nets.values()))
    da, db = design(na), design(nb)
    t = TARGETS
    c_rows = [r for r in rows if r["reference_c_pf"] >= t["net_c_min_pf"]]
    r_rows = [r for r in rows if r["reference_r_ohm"] >= t["net_r_min_ohm"]]
    c_fail = [r["net"] for r in c_rows if abs(r["c_rel"]) > t["net_c_rel"]]
    r_fail = [r["net"] for r in r_rows if abs(r["r_rel"]) > t["net_r_rel"]]
    coverage_ok = not only_native and not only_reference and not conn_mismatch
    total_rel = rel(da["total_pf"], db["total_pf"])

    def dist(values):
        values = sorted(values)
        if not values:
            return None
        return dict(min=values[0], median=values[len(values) // 2], max=values[-1],
                    mean=sum(values) / len(values), count=len(values))
    return dict(
        native=str(native_path), reference=str(reference_path),
        native_sha256=sha256(native_path), reference_sha256=sha256(reference_path),
        coverage=dict(status="passed" if coverage_ok else "failed", only_native=only_native,
                      only_reference=only_reference, connection_mismatch=conn_mismatch,
                      nets=len(rows)),
        design_native=da, design_reference=db, total_c_rel=total_rel,
        targets=dict(
            total_c=dict(status="passed" if total_rel is not None and abs(total_rel) <= t["total_c_rel"] else "failed",
                         rel=total_rel, bound=t["total_c_rel"]),
            net_c=dict(status="passed" if c_rows and not c_fail else "failed", eligible=len(c_rows),
                       failing=c_fail, bound=t["net_c_rel"], min_reference_pf=t["net_c_min_pf"],
                       distribution=dist([r["c_rel"] for r in c_rows])),
            net_r=dict(status="passed" if r_rows and not r_fail else "failed", eligible=len(r_rows),
                       failing=r_fail, bound=t["net_r_rel"], min_reference_ohm=t["net_r_min_ohm"],
                       distribution=dist([r["r_rel"] for r in r_rows])),
            coverage=dict(status="passed" if coverage_ok else "failed"),
        ),
        nets=rows,
    )


# ----------------------------------------------------------------------------
# Geometry-identical merged DEF
# ----------------------------------------------------------------------------
WIRE = re.compile(r"^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)\s*;?\s*$")
VIA = re.compile(r"^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) (\S+)\s*;?\s*$")
# DEF 5.8 routing RECT (the router's same-net patches), kept verbatim.
RECT = re.compile(r"^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) RECT \( (-?\d+) (-?\d+) (-?\d+) (-?\d+) \)"
                  r"\s*;?\s*$")


def _line_union(intervals):
    out = []
    for lo, hi in sorted(intervals):
        if out and lo <= out[-1][1]:
            out[-1][1] = max(out[-1][1], hi)
        else:
            out.append([lo, hi])
    return [tuple(x) for x in out]


def _union_by_line(wires):
    lines = defaultdict(list)
    for layer, a, b in wires:
        if a[0] == b[0]:
            lines[(layer, "v", a[0])].append((min(a[1], b[1]), max(a[1], b[1])))
        else:
            require(a[1] == b[1], "non-Manhattan DEF wire")
            lines[(layer, "h", a[1])].append((min(a[0], b[0]), max(a[0], b[0])))
    return {k: _line_union(v) for k, v in lines.items()}


def merge_def(def_path: Path, routes_path: Path, out_path: Path) -> dict:
    """Merge collinear same-layer lattice edges through points that are not a
    via, a pin access point or a branch. Each original edge carries the DEF
    default half-width extension, so the metal union is unchanged; the result
    is checked line by line, and via lists must be identical."""
    routes = json.loads(bounded_text(routes_path))
    layers = routes["layers"]
    access = defaultdict(set)
    for a in routes["routes"]["access"]:
        access[a["net"]].add((layers[a["layer"]], a["at"][0], a["at"][1]))
    text = bounded_text(def_path, MAX_SPEF_BYTES)
    head, sep, rest = text.partition("\nNETS ")
    require(sep, "DEF NETS section missing")
    body, sep, tail = rest.partition("\nEND NETS\n")
    require(sep, "DEF END NETS missing")
    lines = body.split("\n")
    out, nets, current = [lines[0]], [], None
    for line in lines[1:]:
        if line.startswith("- "):
            current = dict(header=line, wires=[], vias=[], rects=[], trailer=[])
            nets.append(current)
            continue
        m = RECT.match(line)
        if m and current is not None:
            current["rects"].append((m[1], int(m[2]), int(m[3]), int(m[4]), int(m[5]), int(m[6]), int(m[7])))
            continue
        m = WIRE.match(line)
        if m and current is not None:
            current["wires"].append((m[1], (int(m[2]), int(m[3])), (int(m[4]), int(m[5]))))
            continue
        m = VIA.match(line)
        if m and current is not None:
            current["vias"].append((m[1], int(m[2]), int(m[3]), m[4]))
            continue
        require(current is not None and line.strip() in ("", ";"), f"unexpected DEF NETS line: {line!r}")
        current["trailer"].append(line)
    require(len(nets) == len(routes["routes"]["nets"]), "DEF/route net count mismatch")
    before_edges = after_edges = 0
    for index, net in enumerate(nets):
        special = set(access[routes["routes"]["nets"][index]])
        special |= {(l, x, y) for l, x, y, _ in net["vias"]}
        incident = defaultdict(list)
        for k, (l, a, b) in enumerate(net["wires"]):
            incident[(l,) + a].append(k)
            incident[(l,) + b].append(k)

        def interior(p):
            ks = incident[p]
            if len(ks) != 2 or p in special:
                return False
            pts = [q for k in ks for q in net["wires"][k][1:]]
            return len({q[0] for q in pts}) == 1 or len({q[1] for q in pts}) == 1
        used = [False] * len(net["wires"])
        merged = []
        for k, (l, a, b) in enumerate(net["wires"]):
            if used[k]:
                continue
            used[k] = True
            ends = [a, b]
            for side in (0, 1):
                p, prev = ends[side], k
                while interior((l,) + p):
                    nxt = [x for x in incident[(l,) + p] if x != prev][0]
                    if used[nxt]:
                        break
                    used[nxt] = True
                    s = net["wires"][nxt]
                    p = s[2] if s[1] == p else s[1]
                    prev = nxt
                ends[side] = p
            merged.append((l, ends[0], ends[1]))
        require(_union_by_line(merged) == _union_by_line(net["wires"]), "merged DEF metal union differs")
        before_edges += len(net["wires"])
        after_edges += len(merged)
        out.append(net["header"])
        first = True
        for l, a, b in merged:
            out.append(("  + ROUTED " if first else "  NEW ") + f"{l} ( {a[0]} {a[1]} ) ( {b[0]} {b[1]} )")
            first = False
        for l, x, y, name in net["vias"]:
            out.append(("  + ROUTED " if first else "  NEW ") + f"{l} ( {x} {y} ) {name}")
            first = False
        for l, x, y, a, b, c, d in net["rects"]:
            out.append(("  + ROUTED " if first else "  NEW ") + f"{l} ( {x} {y} ) RECT ( {a} {b} {c} {d} )")
            first = False
        if not out[-1].rstrip().endswith(";"):
            out[-1] += " ;"
    out_path.write_text(head + "\nNETS " + "\n".join(out) + "\nEND NETS\n" + tail)
    return dict(source=fingerprint(def_path), merged=fingerprint(out_path), nets=len(nets),
                original_wire_paths=before_edges, merged_wire_paths=after_edges,
                rect_patches=sum(len(n["rects"]) for n in nets),
                metal_union="identical per net, layer and track line", vias="identical", rects="identical")


# ----------------------------------------------------------------------------
# Campaign
# ----------------------------------------------------------------------------
def tree_digest(root: Path) -> str:
    h = hashlib.sha256()
    for p in sorted(root.rglob("*")):
        if p.is_file():
            h.update(str(p.relative_to(root)).encode() + b"\0" + sha256(p).encode() + b"\n")
    return h.hexdigest()


def openrcx(h, tag, pdk, corner, def_path, label, threshold="0.1", cells_lef=None, timeout_s=25,
            file_limit_bytes=None):
    """OpenRCX on a DEF. ``threshold`` is ``-coupling_threshold`` (fF);
    ``cells_lef`` replaces the PDK cell LEF (black-box controls only);
    ``timeout_s`` bounds the container (larger blocks need more than 25 s);
    ``file_limit_bytes`` raises the process file-size limit (SERV-size SPEF)."""
    spec = PDKS[pdk]
    base = PDK_ROOT / spec["base"]
    run_dir = h.root / f"{tag}_rcx"
    run_dir.mkdir(exist_ok=True)
    local_def = run_dir / def_path.name
    if not local_def.exists():
        shutil.copyfile(def_path, local_def)
    require(re.fullmatch(r"[0-9]+(\.[0-9]+)?", threshold) is not None, "bad coupling threshold")
    lef = base / spec["lef"] if cells_lef is None else Path(cells_lef)
    out = run_dir / f"{label}.spef"
    script = run_dir / f"{label}.tcl"
    process = "nominal" if corner == "nom" else corner
    script.write_text(f"""read_lef /technology.lef
read_lef /cells.lef
read_liberty /library.lib
read_def /run/{local_def.name}
define_process_corner -ext_model_index 0 {process}
set_extraction_rules_file /extraction.rules
extract_parasitics -max_res 0 -coupling_threshold {threshold} -no_merge_via_res
write_spef /run/{out.name}
puts EXTRACTION_COMPLETE
exit
""")
    rules = base / spec["rules"].format(corner)
    command = ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only",
               "--tmpfs=/tmp:rw,size=64m", "--memory=2g", "--memory-swap=2g", "--cpus=2", "--pids-limit=128",
               f"--volume={run_dir}:/run:rw", f"--volume={base / spec['tlef']}:/technology.lef:ro",
               f"--volume={lef}:/cells.lef:ro",
               f"--volume={base / spec['lib'].format(spec['libs']['tt'])}:/library.lib:ro",
               f"--volume={rules}:/extraction.rules:ro", "--entrypoint=/usr/bin/timeout", IMAGE, f"{int(timeout_s)}s",
               "/usr/bin/time", "-v", "-o", f"/run/{label}.time", OPENROAD, "-no_init", "-no_splash",
               "-threads", "2", "-exit", f"/run/{script.name}"]
    receipt = h.run(label, command) if file_limit_bytes is None else \
        h.run(label, command, file_limit_bytes=file_limit_bytes)
    require(receipt["exit_code"] == 0, f"{label}: OpenRCX failed")
    require("EXTRACTION_COMPLETE" in bounded_text(Path(receipt["stdout"])), "OpenRCX did not finish")
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", bounded_text(run_dir / f"{label}.time"))
    require(m is not None and int(m[1]) < 2 * 1024 ** 2, "OpenRCX RSS missing/over budget")
    return out, dict(rules=fingerprint(rules), spef=fingerprint(out), container_peak_rss_kib=int(m[1]),
                     elapsed_seconds=receipt["elapsed_seconds"], receipt=receipt["label"])


def native_extract(h, label, rustroad, workspace, profile, corner, out, expect_ok=True):
    command = [str(rustroad), "--workspace", str(workspace), "--threads", "2", "--max-cells", "2000",
               "--max-rows", "128", "extract", "--rc-profile", str(profile), "--corner", corner,
               "--out", str(out)]
    receipt = h.run(label, command)
    if not expect_ok:
        return receipt
    require(receipt["exit_code"] == 0, f"{label}: native extraction failed")
    report = json.loads(bounded_text(Path(receipt["stdout"])))
    require(report["output"]["sha256"] == sha256(out), "extract JSON sha256 differs from file")
    return report, receipt


def sta_run(h, tag, lib, corner, netlist, sdc, spef, expect="ok"):
    ws = h.root / f"{tag}_sta"
    common = ["--workspace", str(ws), "--threads", "2", "--format", "json", "--color", "never"]
    rusteda, ruststa = str(RUSTSTA_BIN / "rusteda"), str(RUSTSTA_BIN / "ruststa")
    a = h.json_command(f"{tag}_init", [rusteda, *common, "init"], True)
    imp = h.run(f"{tag}_import", [rusteda, *common, "import", "--base-rev", a["new_rev"], "--lib", str(lib),
                                  "--corner", corner, "--netlist", str(netlist), "--sdc", str(sdc),
                                  "--spef", str(spef), "--spef-corner", corner, "--max-rc-nodes", "200000",
                                  "--max-rc-elements", "200000"])
    if expect != "ok" and imp["exit_code"] != 0:
        return dict(status="rejected", stage="import", missing_annotation=None,
                    message=bounded_text(Path(imp["stdout"]))[-2000:])
    if expect == "import_rejected":
        return dict(status="accepted", stage="import", missing_annotation=None, message="")
    require(imp["exit_code"] == 0, f"{tag}: RustSTA import failed")
    rev = json.loads(bounded_text(Path(imp["stdout"])))["new_rev"]
    run = h.run(f"{tag}_run", [ruststa, *common, "run", "--base-rev", rev, "--corner", corner, "--backend",
                               "cpu", "--precision", "f64", "--max-paths", "16", "--max-state-gib", "1"])
    if expect == "run_rejected":
        text = bounded_text(Path(run["stdout"]))
        return dict(status="rejected" if run["exit_code"] != 0 else "accepted", stage="run",
                    missing_annotation="missing SPEF annotation" in text, message=text[-2000:])
    require(run["exit_code"] == 0, f"{tag}: RustSTA run failed")
    r = json.loads(bounded_text(Path(run["stdout"])))
    scope = r["scope"]
    require(scope.get("wire_model") == "spef_rc" and scope.get("delay_calculator") == "nldm_ceff_ramp"
            and scope.get("clock_model") == "propagated" and scope.get("corner") == corner,
            f"{tag}: wrong extracted timing scope {scope}")
    require(r["completeness"]["unconstrained_endpoints"] == 0, f"{tag}: unconstrained endpoints")
    # Every endpoint row (the harness's json_command would cap --limit at 128,
    # which truncates larger designs differently per SPEF).
    report = h.run(f"{tag}_report", [ruststa, *common, "report", "--rev", r["new_rev"], "--section", "checks",
                                     "--section", "paths", "--limit", str(STA_REPORT_LIMIT)])
    require(report["exit_code"] == 0, f"{tag}: RustSTA report failed")
    rep = json.loads(bounded_text(Path(report["stdout"]), 256 * 1024 ** 2))
    require(rep.get("schema") == "rusteda.v1" and rep.get("ok") is True and rep.get("error") is None,
            f"{tag}: unsuccessful rusteda.v1 envelope")
    require(len(rep["sections"]["checks"]["rows"]) < STA_REPORT_LIMIT, f"{tag}: endpoint rows reached the limit")
    paths = {p["handle"]: p for p in rep["sections"]["paths"]["rows"]}
    endpoints = {}
    for row in rep["sections"]["checks"]["rows"]:
        setup, hold = paths.get(row["worst_setup_handle"]), paths.get(row["worst_hold_handle"])
        endpoints[row["endpoint"]] = dict(setup_slack_ns=row["setup_slack_ns"], hold_slack_ns=row["hold_slack_ns"],
                                          setup_arrival_ns=setup and setup["arrival_ns"],
                                          hold_arrival_ns=hold and hold["arrival_ns"])
    return dict(status="ok", scope=scope, metrics=r["metrics"], completeness=r["completeness"],
                diagnostics=sorted({d.get("code") for d in r.get("diagnostics", [])} - {None}),
                endpoints=endpoints, peak_rss_kib=max(x["peak_rss_kib"] or 0 for x in (imp, run)))


def sta_delta(native, reference):
    require(set(native["endpoints"]) == set(reference["endpoints"]), "STA endpoint coverage differs")
    rows, worst = [], dict(setup=0.0, hold=0.0, setup_arrival=0.0, hold_arrival=0.0)
    for name in sorted(native["endpoints"]):
        a, b = native["endpoints"][name], reference["endpoints"][name]
        row = dict(endpoint=name)
        for k in ("setup", "hold"):
            d = a[k + "_slack_ns"] - b[k + "_slack_ns"]
            require(math.isfinite(d), "nonfinite slack")
            row[k + "_slack_delta_ns"] = d
            worst[k] = max(worst[k], abs(d))
            if a[k + "_arrival_ns"] is not None and b[k + "_arrival_ns"] is not None:
                da = a[k + "_arrival_ns"] - b[k + "_arrival_ns"]
                row[k + "_arrival_delta_ns"] = da
                worst[k + "_arrival"] = max(worst[k + "_arrival"], abs(da))
        rows.append(row)
    ok = max(worst["setup"], worst["hold"]) <= TARGETS["slack_delta_ns"]
    return dict(status="passed" if ok else "failed", bound_ns=TARGETS["slack_delta_ns"],
                max_abs_setup_delta_ns=worst["setup"], max_abs_hold_delta_ns=worst["hold"],
                max_abs_setup_arrival_delta_ns=worst["setup_arrival"],
                max_abs_hold_arrival_delta_ns=worst["hold_arrival"],
                native_worst=dict(setup=native["metrics"]["worst_setup_slack_ns"],
                                  hold=native["metrics"]["worst_hold_slack_ns"]),
                reference_worst=dict(setup=reference["metrics"]["worst_setup_slack_ns"],
                                     hold=reference["metrics"]["worst_hold_slack_ns"]),
                endpoints=rows)


def resistance_accounting(routes_path: Path, connectivity_path: Path, profile_path: Path,
                          spef_path: Path, dbu: float) -> dict:
    """Independent re-computation of each net's total wire + via resistance from
    the stored route records and the profile (not from the extractor's tree).

    Collinear records of a net are merged per layer and line, so repeated
    metal counts once. A net whose record graph has a loop is left out and
    counted: the extractor keeps a tree and opens the loop-closing conductor,
    so its SPEF total is lower by design."""
    routes = json.loads(bounded_text(routes_path))
    names = [n["name"] for n in json.loads(bounded_text(connectivity_path))["nets"]]
    profile = json.loads(bounded_text(profile_path))
    layers = {l["name"]: l for l in profile["layers"]}
    cuts = {c["lower"]: c["resistance_ohm_per_cut"] for c in profile["cuts"]}
    route_layers = routes["layers"]
    lines = defaultdict(list)  # (net, layer, horizontal, fixed) -> [(lo, hi)]
    keys = defaultdict(set)    # net -> {(layer, x, y)}
    via_edges = defaultdict(list)
    for w in routes["routes"]["wires"]:
        horizontal = w["start"][1] == w["end"][1]
        fixed, a, b = (w["start"][1], w["start"][0], w["end"][0]) if horizontal else \
            (w["start"][0], w["start"][1], w["end"][1])
        lines[(w["net"], w["layer"], horizontal, fixed)].append((min(a, b), max(a, b)))
        keys[w["net"]].update({(w["layer"], *w["start"]), (w["layer"], *w["end"])})
    expected = defaultdict(float)
    for v in routes["routes"]["vias"]:
        expected[names[v["net"]]] += cuts[route_layers[routes["via_lower_layers"][v["kind"]]]]
        lo, hi = routes["via_lower_layers"][v["kind"]], None
        # Upper layer of the via: the next routing layer in the routes' order.
        hi = route_layers.index(route_layers[lo]) + 1
        keys[v["net"]].update({(lo, *v["at"]), (hi, *v["at"])})
        via_edges[v["net"]].append(((lo, *v["at"]), (hi, *v["at"])))
    for a in routes["routes"]["access"]:
        keys[a["net"]].add((a["layer"], *a["at"]))
    edges = defaultdict(list)
    for (net, layer, horizontal, fixed), spans in lines.items():
        spans.sort()
        merged = []
        for lo, hi in spans:
            if merged and lo <= merged[-1][1]:
                merged[-1][1] = max(merged[-1][1], hi)
            else:
                merged.append([lo, hi])
        model = layers[route_layers[layer]]
        for lo, hi in merged:
            expected[names[net]] += model["sheet_resistance_ohm_per_square"] * ((hi - lo) / dbu) / model["width_um"]
            stops = sorted({(k[1] if horizontal else k[2]) for k in keys[net] if k[0] == layer
                            and (k[2] if horizontal else k[1]) == fixed
                            and lo <= (k[1] if horizontal else k[2]) <= hi})
            for c0, c1 in zip(stops, stops[1:]):
                p0 = (layer, c0, fixed) if horizontal else (layer, fixed, c0)
                p1 = (layer, c1, fixed) if horizontal else (layer, fixed, c1)
                edges[net].append((p0, p1))
    loops = set()
    for net in keys:
        parent = {}

        def find(x):
            while parent.setdefault(x, x) != x:
                parent[x] = parent[parent[x]]
                x = parent[x]
            return x
        for a, b in edges[net] + via_edges[net]:
            ra, rb = find(a), find(b)
            if ra == rb:
                loops.add(names[net])
                break
            parent[ra] = rb
    spef = parse_spef(spef_path)
    require(set(spef["nets"]) == set(expected), "accounting net coverage")
    errors = {n: abs(sum(r for _, _, r in spef["nets"][n]["res"]) - want) / want
              for n, want in expected.items() if n not in loops}
    worst = max(errors.values())
    return dict(status="passed" if worst < 1e-5 else "failed", nets=len(errors), nets_with_loops=sorted(loops),
                max_rel_error=worst,
                method="per net: merged stored wires Rs*L/W plus vias, against the SPEF resistors; nets whose "
                       "record graph has a loop are left out (the extractor opens the loop)")


def unmodeled_metal_estimate(definition: Path, design_json: Path, profile_path: Path) -> dict:
    """A priori size of the metal the native model omits (via landing-pad area
    outside the net's own wires, and the half-width end caps of wire runs),
    converted to capacitance with the profile's own isolated-wire value per um
    of default-width wire. Uses no reference output."""
    tech = json.loads(bounded_text(design_json, 16 * 1024 ** 2))["physical"]["technology"]
    profile = json.loads(bounded_text(profile_path))
    model = {l["name"]: l for l in profile["layers"]}
    dbu = float(tech["dbu_per_micron"])
    vias = {v["name"]: [g for g in v["geometries"] if g["layer"] in model] for v in tech["vias"]}
    text = bounded_text(definition, MAX_SPEF_BYTES)
    body = text.split("\nNETS ", 1)[1].split("\nEND NETS\n", 1)[0]
    nets, current = [], None
    for line in body.split("\n")[1:]:
        if line.startswith("- "):
            current = dict(wires=[], vias=[])
            nets.append(current)
        elif (m := WIRE.match(line)):
            current["wires"].append((m[1], (int(m[2]), int(m[3])), (int(m[4]), int(m[5]))))
        elif (m := VIA.match(line)):
            current["vias"].append((m[1], int(m[2]), int(m[3]), m[4]))
    step = 5  # DBU raster for pad coverage
    pad_area = pad_excess = end_area = 0.0
    estimate = 0.0
    for net in nets:
        rects = defaultdict(list)
        degree = defaultdict(int)
        for layer, a, b in net["wires"]:
            half = round(model[layer]["width_um"] * dbu) // 2
            rects[layer].append((min(a[0], b[0]) - half, min(a[1], b[1]) - half,
                                 max(a[0], b[0]) + half, max(a[1], b[1]) + half))
            degree[(layer,) + a] += 1
            degree[(layer,) + b] += 1
        for (layer, _x, _y), n in degree.items():
            if n == 1:
                w = model[layer]["width_um"]
                area = w * w / 2.0
                end_area += area
                estimate += area / w * 2.0 * model[layer]["contexts"][0]["rows"][-1][2]
        for _lower, x, y, name in net["vias"]:
            for g in vias[name]:
                layer = g["layer"]
                lx, ly, ux, uy = g["rect"][0] + x, g["rect"][1] + y, g["rect"][2] + x, g["rect"][3] + y
                total = covered = 0
                for px in range(lx + step // 2, ux, step):
                    for py in range(ly + step // 2, uy, step):
                        total += 1
                        if any(r[0] <= px < r[2] and r[1] <= py < r[3] for r in rects[layer]):
                            covered += 1
                cell = (ux - lx) * (uy - ly) / dbu ** 2 / max(total, 1)
                pad_area += total * cell
                excess = (total - covered) * cell
                pad_excess += excess
                w = model[layer]["width_um"]
                estimate += excess / w * 2.0 * model[layer]["contexts"][0]["rows"][-1][2]
    return dict(via_pad_area_um2=pad_area, via_pad_area_outside_wires_um2=pad_excess,
                wire_end_cap_area_um2=end_area, estimated_capacitance_pf=estimate,
                method="excess area / default width * isolated two-sided ground per um (substrate context)")


def mutate_spef(src: Path, dst: Path, kind: str) -> str:
    text = bounded_text(src, MAX_SPEF_BYTES)
    if kind == "dropped_net":
        blocks = list(re.finditer(r"(?ms)^\*D_NET (\S+) .*?^\*END\n", text))
        require(len(blocks) > 1, "no net to drop")
        victim = blocks[len(blocks) // 2]
        text = text[:victim.start()] + text[victim.end():]
        detail = victim[1]
    elif kind == "missing_net":
        m = re.search(r"(?m)^(\*\d+) (n\d+|q\[\d\]|rr_clock_\d)$", text)
        require(m is not None, "no internal net to rename")
        text = text[:m.start()] + f"{m[1]} rr_missing_net_{m[2].replace('[', '_').replace(']', '')}" + text[m.end():]
        detail = m[2]
    elif kind == "dropped_connection":
        m = re.search(r"(?m)^\*I \S+ I(?: \*D \S+)?\n", text)
        require(m is not None, "no sink connection")
        text = text[:m.start()] + text[m.end():]
        detail = m[0].strip()
    else:
        raise ValueError(kind)
    dst.write_text(text)
    return detail


def scaled_profile(src: Path, dst: Path, factor: float) -> None:
    p = json.loads(src.read_text())
    for layer in p["layers"]:
        layer["sheet_resistance_ohm_per_square"] *= factor
    for cut in p["cuts"]:
        cut["resistance_ohm_per_cut"] *= factor
    p["diagnostics"].append(f"NEGATIVE_CONTROL: all resistances scaled by {factor}")
    dst.write_text(json.dumps(p, indent=1) + "\n")


def campaign_pdk(h, pdk, ready, rustroad):
    spec = PDKS[pdk]
    base = PDK_ROOT / spec["base"]
    tag = pdk
    result = dict(pdk=pdk)
    # Inputs: copies of the latest ready run, verified byte-identical.
    ws = h.root / f"{pdk}_physical"
    shutil.copytree(ready / f"{pdk}_physical", ws)
    copies = {}
    for name in (f"{pdk}.def", f"{pdk}_exported.v", f"{pdk}_postroute.sdc", f"{pdk}.spef"):
        shutil.copyfile(ready / name, h.root / name)
        copies[name] = dict(source=str(ready / name), sha256=sha256(h.root / name))
        require(copies[name]["sha256"] == sha256(ready / name), f"input copy differs from ready run: {name}")
    require(tree_digest(ws) == tree_digest(ready / f"{pdk}_physical"), "workspace copy differs from ready run")
    result["inputs"] = copies
    definition, netlist, sdc = h.root / f"{pdk}.def", h.root / f"{pdk}_exported.v", h.root / f"{pdk}_postroute.sdc"
    before = tree_digest(ws)
    # Native extraction at each RC corner, twice (byte determinism).
    native, native_reports = {}, {}
    for corner in ("min", "nom", "max"):
        profile = ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json"
        out = h.root / f"{pdk}_native_{corner}.spef"
        report, receipt = native_extract(h, f"{tag}_native_{corner}", rustroad, ws, profile, corner, out)
        again = h.root / f"{pdk}_native_{corner}_rerun.spef"
        report2, _ = native_extract(h, f"{tag}_native_{corner}_rerun", rustroad, ws, profile, corner, again)
        require(again.read_bytes() == out.read_bytes(), f"{pdk} {corner}: native SPEF not byte-identical")
        native[corner] = out
        native_reports[corner] = dict(report=report, rerun_sha256=report2["output"]["sha256"],
                                      byte_identical=True, elapsed_seconds=receipt["elapsed_seconds"],
                                      peak_rss_kib=receipt["peak_rss_kib"])
    after = tree_digest(ws)
    require(before == after, f"{pdk}: native extraction modified the workspace")
    inside = native_extract(h, f"{tag}_native_inside_workspace", rustroad, ws,
                            ROOT / f"fixtures/pdk/{pdk}-rc-nom.json", "nom", ws / "x.spef", expect_ok=False)
    mismatch = native_extract(h, f"{tag}_native_corner_mismatch", rustroad, ws,
                              ROOT / f"fixtures/pdk/{pdk}-rc-max.json", "nom", h.root / "mismatch.spef",
                              expect_ok=False)
    require(inside["exit_code"] != 0 and "OUTPUT_IN_WORKSPACE" in bounded_text(Path(inside["stderr"])),
            "in-workspace output not refused")
    require(mismatch["exit_code"] != 0 and "CORNER_MISMATCH" in bounded_text(Path(mismatch["stderr"])),
            "corner mismatch not refused")
    require(tree_digest(ws) == before and not (ws / "x.spef").exists(), "refused extraction changed workspace")
    result["native"] = native_reports
    exported = {}
    for command in ("export-routes", "export-connectivity"):
        r = h.run(f"{tag}_{command}", [str(rustroad), "--workspace", str(ws), "--max-cells", "2000",
                                       "--max-rows", "128", command])
        require(r["exit_code"] == 0, f"{command} failed")
        exported[command] = h.root / f"{pdk}_{command}.json"
        shutil.copyfile(r["stdout"], exported[command])
    dbu = float(json.loads(bounded_text(ws / "revisions" / (ws / "HEAD").read_text().strip() / "design.json",
                                        16 * 1024 ** 2))["design"]["units_per_micron"])
    result["resistance_accounting"] = {
        corner: resistance_accounting(exported["export-routes"], exported["export-connectivity"],
                                      ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json", native[corner], dbu)
        for corner in ("min", "nom", "max")}

    result["read_only"] = dict(status="passed", workspace_tree_sha256=before,
                               output_inside_workspace="refused", corner_mismatch="refused")
    # Reference: harness DEF exactly as the flow wrote it, and the merged DEF.
    routes = exported["export-routes"]
    merged_def = h.root / f"{pdk}_merged.def"
    result["merged_def"] = merge_def(definition, routes, merged_def)
    reference = dict(harness={}, merged={})
    result["reference_runs"] = dict(harness={}, merged={})
    for corner in ("min", "nom", "max"):
        for kind, d in (("harness", definition), ("merged", merged_def)):
            spef, info = openrcx(h, f"{tag}_{kind}", pdk, corner, d, f"{pdk}_{kind}_{corner}")
            reference[kind][corner] = spef
            result["reference_runs"][kind][corner] = info
    strip = lambda p: [l for l in bounded_text(p, MAX_SPEF_BYTES).splitlines() if not l.startswith("*DATE")]
    result["reference_identity"] = dict(
        nominal_harness_equals_ready_run_spef=strip(reference["harness"]["nom"]) == strip(h.root / f"{pdk}.spef"))
    require(result["reference_identity"]["nominal_harness_equals_ready_run_spef"],
            "reference nominal SPEF no longer reproduces the ready run")
    # SPEF comparisons.
    result["spef"] = {}
    for corner in ("min", "nom", "max"):
        result["spef"][corner] = dict(
            vs_harness_def=compare_spefs(native[corner], reference["harness"][corner]),
            vs_merged_def=compare_spefs(native[corner], reference["merged"][corner]),
            reference_segmentation=compare_spefs(reference["merged"][corner], reference["harness"][corner]))
    # STA with each SPEF at each analysis corner.
    libs = {}
    for c, name in spec["libs"].items():
        source = base / spec["lib"].format(name)
        projected = h.root / f"{pdk}_{c}_ideal.lib"
        libs[c] = dict(projection=ideal_projection(source, projected), path=projected, source=fingerprint(source))
    result["sta"] = {}
    for sta_corner, rc in STA_CORNERS.items():
        runs = {}
        for label, spef in (("native", native[rc]), ("harness", reference["harness"][rc]),
                            ("merged", reference["merged"][rc])):
            runs[label] = sta_run(h, f"{tag}_{label}_{sta_corner}", libs[sta_corner]["path"], sta_corner,
                                  netlist, sdc, spef)
        result["sta"][sta_corner] = dict(
            liberty=libs[sta_corner]["source"], rc_corner=rc,
            runs={k: {x: v[x] for x in ("scope", "metrics", "completeness", "diagnostics", "peak_rss_kib")}
                  for k, v in runs.items()},
            vs_harness_def=sta_delta(runs["native"], runs["harness"]),
            vs_merged_def=sta_delta(runs["native"], runs["merged"]),
            reference_segmentation=sta_delta(runs["merged"], runs["harness"]),
            endpoints={k: v["endpoints"] for k, v in runs.items()})
    result["unmodeled_metal"] = {
        corner: unmodeled_metal_estimate(merged_def, ws / "revisions" / (ws / "HEAD").read_text().strip()
                                         / "design.json", ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json")
        for corner in ("min", "nom", "max")}
    # Sensitivity diagnostics (not model changes): how much of the min/max
    # corner difference comes from the resistance corner data alone.
    diagnostics = {}
    nominal_profile = json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-nom.json").read_text())
    for sta_corner, rc in (("ss", "max"), ("ff", "min")):
        for variant in ("nominal_resistance", "nominal_via_resistance"):
            prof = json.loads((ROOT / f"fixtures/pdk/{pdk}-rc-{rc}.json").read_text())
            if variant == "nominal_resistance":
                for layer, nominal in zip(prof["layers"], nominal_profile["layers"]):
                    layer["sheet_resistance_ohm_per_square"] = nominal["sheet_resistance_ohm_per_square"]
            for cut, nominal in zip(prof["cuts"], nominal_profile["cuts"]):
                cut["resistance_ohm_per_cut"] = nominal["resistance_ohm_per_cut"]
            prof["diagnostics"].append(f"SENSITIVITY_VARIANT: {variant}")
            path = h.root / f"{pdk}-rc-{rc}-{variant}.json"
            path.write_text(json.dumps(prof, indent=1) + "\n")
            spef = h.root / f"{pdk}_native_{rc}_{variant}.spef"
            native_extract(h, f"{tag}_sensitivity_{rc}_{variant}", rustroad, ws, path, rc, spef)
            run = sta_run(h, f"{tag}_sensitivity_{sta_corner}_{variant}", libs[sta_corner]["path"], sta_corner,
                          netlist, sdc, spef)
            reference_run = result["sta"][sta_corner]
            merged_endpoints = dict(endpoints=reference_run["endpoints"]["merged"],
                                    metrics=reference_run["runs"]["merged"]["metrics"])
            delta = sta_delta(run, merged_endpoints)
            cmp = compare_spefs(spef, reference["merged"][rc])
            diagnostics[f"{sta_corner}_{rc}_{variant}"] = dict(
                sta_vs_merged=dict((k, delta[k]) for k in ("status", "max_abs_setup_delta_ns",
                                                           "max_abs_hold_delta_ns")),
                net_r=cmp["targets"]["net_r"], total_c_rel=cmp["total_c_rel"])
    result["sensitivity"] = diagnostics
    # Negative controls.
    controls = {}
    scaled = h.root / f"{pdk}-rc-nom-scaledR.json"
    scaled_profile(ROOT / f"fixtures/pdk/{pdk}-rc-nom.json", scaled, 3.0)
    scaled_spef = h.root / f"{pdk}_native_nom_scaledR.spef"
    native_extract(h, f"{tag}_control_scaled_r", rustroad, ws, scaled, "nom", scaled_spef)
    cmp = compare_spefs(scaled_spef, reference["merged"]["nom"])
    same_c = compare_spefs(scaled_spef, native["nom"])
    controls["scaled_r_x3"] = dict(
        status="detected" if cmp["targets"]["net_r"]["status"] == "failed" else "missed",
        net_r=cmp["targets"]["net_r"], capacitance_unchanged=abs(same_c["total_c_rel"]) < 1e-9)
    for kind in ("dropped_net", "missing_net", "dropped_connection"):
        bad = h.root / f"{pdk}_native_nom_{kind}.spef"
        detail = mutate_spef(native["nom"], bad, kind)
        cmp = compare_spefs(bad, reference["merged"]["nom"])
        sta = sta_run(h, f"{tag}_control_{kind}", libs["tt"]["path"], "tt", netlist, sdc, bad,
                      expect="run_rejected")
        controls[kind] = dict(mutation=detail, comparison_coverage=cmp["coverage"],
                              ruststa=dict(status=sta["status"], stage=sta["stage"],
                                           missing_annotation=sta.get("missing_annotation")),
                              status="detected" if cmp["coverage"]["status"] == "failed"
                              and sta["status"] == "rejected" else "missed")
    result["controls"] = controls
    return result


def campaign(args) -> int:
    global RUSTSTA_BIN
    RUSTSTA_BIN = args.sta_bin.resolve()
    require(args.base_rev == "empty", "REVISION_CONFLICT: a fresh campaign directory requires --base-rev empty")
    root = args.workspace.resolve()
    root.mkdir(parents=True, exist_ok=False)
    lock = (root / "workspace.lock").open("wb")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    (root / "HEAD").write_text("rev0\n")
    resource.setrlimit(resource.RLIMIT_AS, (8192 * 1024 ** 2, 8192 * 1024 ** 2))
    os.environ.update(GOMAXPROCS="2", OMP_NUM_THREADS="2")
    h = PhysicalHarness(root, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=30,
                                                 tolerance_ns=0.001))
    start = time.monotonic()
    rustroad = args.rustroad.resolve()
    summary = dict(schema="rustroad.extraction-comparison.v1", status="unverified", targets=TARGETS,
                   sta_corners={k: dict(liberty=k, rc=v) for k, v in STA_CORNERS.items()},
                   scope=dict(block_level=True, coupling="native: cross-net SPEF caps; timing grounds at factor 1",
                              si="unchecked", ir_em="unchecked", ocv="unchecked", cppr="off",
                              drc="unchecked", lvs="unchecked"),
                   tools={}, pdks=[])
    try:
        r = h.run("memory_preflight", ["free", "-h"])
        summary["memory_preflight"] = bounded_text(Path(r["stdout"]))
        for name, path in (("rustroad", rustroad), ("rusteda", RUSTSTA_BIN / "rusteda"),
                           ("ruststa", RUSTSTA_BIN / "ruststa")):
            require(path.is_file(), f"missing {name}")
            r = h.run(f"{name}_version", [str(path), "--version"])
            summary["tools"][name] = {**fingerprint(path), "version": bounded_text(Path(r["stdout"])).strip()}
        r = h.run("openroad_identity", ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only",
                                        "--memory=2g", "--memory-swap=2g", "--entrypoint=/usr/bin/sha256sum",
                                        IMAGE, OPENROAD])
        require(r["exit_code"] == 0, "OpenROAD identity probe failed")
        summary["tools"]["openroad"] = dict(image=IMAGE, binary=OPENROAD,
                                            sha256=bounded_text(Path(r["stdout"])).split()[0])
        for pdk in PDKS:
            summary["pdks"].append(campaign_pdk(h, pdk, args.ready.resolve(), rustroad))
        verdicts = []
        for p in summary["pdks"]:
            for corner in ("min", "nom", "max"):
                for ref in ("vs_harness_def", "vs_merged_def"):
                    for k, v in p["spef"][corner][ref]["targets"].items():
                        verdicts.append((p["pdk"], corner, ref, k, v["status"]))
            for c in STA_CORNERS:
                for ref in ("vs_harness_def", "vs_merged_def"):
                    verdicts.append((p["pdk"], c, ref, "sta_slack", p["sta"][c][ref]["status"]))
            for corner, v in p["resistance_accounting"].items():
                verdicts.append((p["pdk"], corner, "independent", "resistance_accounting", v["status"]))
            verdicts.append((p["pdk"], "all", "self", "read_only", p["read_only"]["status"]))
            for k, v in p["controls"].items():
                verdicts.append((p["pdk"], "control", k, "detected", "passed" if v["status"] == "detected" else "failed"))
        summary["verdicts"] = [dict(pdk=a, corner=b, reference=c, target=d, status=e) for a, b, c, d, e in verdicts]
        controls_ok = all(v[4] == "passed" for v in verdicts if v[1] == "control" or v[2] in ("independent", "self"))
        merged_ok = all(v[4] == "passed" for v in verdicts if v[2] == "vs_merged_def")
        harness_ok = all(v[4] == "passed" for v in verdicts if v[2] == "vs_harness_def")
        summary["status"] = ("passed" if controls_ok and merged_ok and harness_ok else
                             "partial" if controls_ok else "failed")
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary["status"] = "failed"
        summary["reason"] = str(error)
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
    """Markdown tables from a campaign summary (used for docs/extraction.md)."""
    d = json.loads(bounded_text(summary_path))
    pct = lambda v: "n/a" if v is None else f"{100 * v:+.1f}%"
    out = [f"Campaign status: **{d['status']}**, wall {d['wall_seconds']:.1f} s, "
           f"peak child RSS {d['peak_rss_kib']} KiB.", ""]
    out += ["| PDK | RC | reference DEF | total C | per-net C (n, min..max) | per-net R (n, min..max) | coverage |",
            "|---|---|---|---|---|---|---|"]
    for p in d["pdks"]:
        for c in ("min", "nom", "max"):
            for ref, label in (("vs_merged_def", "merged"), ("vs_harness_def", "harness")):
                t = p["spef"][c][ref]["targets"]
                nc, nr = t["net_c"], t["net_r"]
                out.append(f"| {p['pdk']} | {c} | {label} | {pct(t['total_c']['rel'])} {t['total_c']['status']} | "
                           f"{nc['eligible']}, {pct(nc['distribution']['min'])}..{pct(nc['distribution']['max'])} "
                           f"{nc['status']} | {nr['eligible']}, {pct(nr['distribution']['min'])}.."
                           f"{pct(nr['distribution']['max'])} {nr['status']} | {t['coverage']['status']} |")
    out += ["", "| PDK | STA corner | reference DEF | max abs setup delta | max abs hold delta | native WNS/WHS | reference WNS/WHS | verdict |",
            "|---|---|---|---|---|---|---|---|"]
    for p in d["pdks"]:
        for c, v in p["sta"].items():
            for ref, label in (("vs_merged_def", "merged"), ("vs_harness_def", "harness")):
                x = v[ref]
                out.append(f"| {p['pdk']} | {c}+{v['rc_corner']} | {label} | {1e3 * x['max_abs_setup_delta_ns']:.2f} ps | "
                           f"{1e3 * x['max_abs_hold_delta_ns']:.2f} ps | {x['native_worst']['setup']:.4f} / "
                           f"{x['native_worst']['hold']:+.4f} ns | {x['reference_worst']['setup']:.4f} / "
                           f"{x['reference_worst']['hold']:+.4f} ns | {x['status']} |")
    out += ["", "| PDK | reference segmentation sensitivity (same metal, harness vs merged DEF) | total C | worst STA delta |",
            "|---|---|---|---|"]
    for p in d["pdks"]:
        for c in ("min", "nom", "max"):
            seg = p["spef"][c]["reference_segmentation"]
            sta = next(v for v in p["sta"].values() if v["rc_corner"] == c)["reference_segmentation"]
            out.append(f"| {p['pdk']} | {c}: merged/harness | {pct(seg['total_c_rel'])} | "
                       f"{1e3 * max(sta['max_abs_setup_delta_ns'], sta['max_abs_hold_delta_ns']):.1f} ps |")
    out += ["", "| PDK | control | result |", "|---|---|---|"]
    for p in d["pdks"]:
        for k, v in p["controls"].items():
            out.append(f"| {p['pdk']} | {k} | {v['status']} |")
        for k, v in p["resistance_accounting"].items():
            out.append(f"| {p['pdk']} | independent R accounting {k} | {v['status']} (max rel err {v['max_rel_error']:.1e}) |")
    out += ["", "| PDK | sensitivity variant | STA max delta vs merged | per-net R range vs merged |",
            "|---|---|---|---|"]
    for p in d["pdks"]:
        for k, v in p["sensitivity"].items():
            x = v["sta_vs_merged"]
            r = v["net_r"]["distribution"]
            out.append(f"| {p['pdk']} | {k} | {1e3 * max(x['max_abs_setup_delta_ns'], x['max_abs_hold_delta_ns']):.2f} ps | "
                       f"{pct(r['min'])}..{pct(r['max'])} |")
    out += ["", "| PDK | RC | via pad area outside wires | end-cap area | a priori C of omitted metal | measured gap (merged - native) |",
            "|---|---|---|---|---|---|"]
    for p in d["pdks"]:
        for c in ("min", "nom", "max"):
            u = p["unmodeled_metal"][c]
            cmp = p["spef"][c]["vs_merged_def"]
            gap = cmp["design_reference"]["total_pf"] - cmp["design_native"]["total_pf"]
            out.append(f"| {p['pdk']} | {c} | {u['via_pad_area_outside_wires_um2']:.2f} um2 | "
                       f"{u['wire_end_cap_area_um2']:.2f} um2 | {1e3 * u['estimated_capacitance_pf']:.2f} fF | "
                       f"{1e3 * gap:.2f} fF |")
    return "\n".join(out) + "\n"


def evidence(summary_path: Path) -> dict:
    """Sanitized tracked evidence: verdicts, distributions and resources only."""
    d = json.loads(bounded_text(summary_path))
    keep = lambda t: {k: v for k, v in t.items() if k != "nets"}
    pdks = []
    for p in d["pdks"]:
        pdks.append(dict(
            pdk=p["pdk"], inputs=p["inputs"], read_only=p["read_only"],
            reference_identity=p["reference_identity"], merged_def=p["merged_def"],
            native={c: dict(output=v["report"]["output"], rc_profile=v["report"]["rc_profile"],
                            stats=v["report"]["stats"], scope=v["report"]["scope"],
                            byte_identical_rerun=v["byte_identical"], elapsed_seconds=v["elapsed_seconds"],
                            peak_rss_kib=v["peak_rss_kib"]) for c, v in p["native"].items()},
            reference_runs=p["reference_runs"],
            spef={c: {k: keep(v) for k, v in x.items()} for c, x in p["spef"].items()},
            spef_nets_nominal_vs_merged=p["spef"]["nom"]["vs_merged_def"]["nets"],
            sta={c: dict(liberty=v["liberty"], rc_corner=v["rc_corner"], runs=v["runs"],
                         **{k: {x: y for x, y in v[k].items() if x != "endpoints"}
                            for k in ("vs_merged_def", "vs_harness_def", "reference_segmentation")})
                 for c, v in p["sta"].items()},
            resistance_accounting=p["resistance_accounting"], unmodeled_metal=p["unmodeled_metal"],
            sensitivity=p["sensitivity"],
            controls={k: {x: y for x, y in v.items() if x != "net_r"} for k, v in p["controls"].items()}))
    return dict(schema="rustroad.extraction-evidence.v1", status=d["status"], targets=d["targets"],
                sta_corners=d["sta_corners"], scope=d["scope"], tools=d["tools"], verdicts=d["verdicts"],
                wall_seconds=d["wall_seconds"], peak_child_rss_kib=d["peak_rss_kib"],
                receipts=len(d["receipts"]), pdks=pdks)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    s = sub.add_parser("spef")
    s.add_argument("--native", type=Path, required=True)
    s.add_argument("--reference", type=Path, required=True)
    m = sub.add_parser("merge-def")
    m.add_argument("--def", dest="definition", type=Path, required=True)
    m.add_argument("--routes", type=Path, required=True)
    m.add_argument("--out", type=Path, required=True)
    r = sub.add_parser("report")
    r.add_argument("--summary", type=Path, required=True)
    e = sub.add_parser("evidence")
    e.add_argument("--summary", type=Path, required=True)
    e.add_argument("--out", type=Path, required=True)
    c = sub.add_parser("campaign")
    c.add_argument("--workspace", type=Path, required=True)
    c.add_argument("--base-rev", required=True)
    c.add_argument("--ready", type=Path, default=READY_DEFAULT,
                   help="latest routed run to copy (never modified)")
    c.add_argument("--rustroad", type=Path, default=ROOT / "target/release/rustroad")
    c.add_argument("--sta-bin", type=Path, default=RUSTSTA_BIN,
                   help="directory holding the rusteda and ruststa binaries to use")
    args = parser.parse_args()
    if args.command == "spef":
        result = compare_spefs(args.native, args.reference)
        print(json.dumps({k: v for k, v in result.items() if k != "nets"}, indent=1))
        return 0 if all(t["status"] == "passed" for t in result["targets"].values()) else 3
    if args.command == "report":
        print(report(args.summary), end="")
        return 0
    if args.command == "evidence":
        write_json(args.out, evidence(args.summary))
        return 0
    if args.command == "merge-def":
        print(json.dumps(merge_def(args.definition, args.routes, args.out), indent=1))
        return 0
    return campaign(args)


if __name__ == "__main__":
    sys.exit(main())
