#!/usr/bin/env python3
"""Compose the tracked dual-PDK requalification evidence from finished runs.

Reads (never modifies) two verify_routing.py runs of the same inputs (--pass1
with --signoff-reference, --pass2 with --signoff-gds of pass 1's native
export-gds output), the verify_router_signoff.py run on pass 2, and the
directory holding pass 1's native GDS files. It re-derives the byte-
determinism facts (DEF, GDS and native SPEF of pass 1 vs pass 2) by exporting
again with the given rustroad binary into a scratch directory, and extracts
the antenna details from the Magic report. Nothing here decides a verdict
that a harness did not already decide; it only collects them.

usage: requalification_evidence.py --pass1 R1 --pass2 R2 --native-gds DIR
         --router-signoff RS --rustroad BIN --scratch DIR --out docs/evidence/requalification.json
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
PDK_ROOT = Path("/home/cgao/.local/share/containers/storage/overlay/"
                "a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel")
CELL_GDS = {
    "sky130": PDK_ROOT / "sky130/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/sky130A/libs.ref/"
                         "sky130_fd_sc_hd/gds/sky130_fd_sc_hd.gds",
    "gf180": PDK_ROOT / "gf180mcu/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/gf180mcuD/libs.ref/"
                        "gf180mcu_fd_sc_mcu7t5v0/gds/gf180mcu_fd_sc_mcu7t5v0.gds",
}


def sha256(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(argv: list) -> None:
    subprocess.run([str(a) for a in argv], check=True, capture_output=True, timeout=120)


def statuses(summary: dict) -> dict:
    out = {}
    for p in summary["pdks"]:
        s = {}
        for k, v in p.get("signoff_statuses", {}).items():
            s[k] = {x: y for x, y in v.items() if x in ("status", "count", "evidence_gds", "setup_violations",
                                                       "hold_violations", "worst_setup_ns", "worst_hold_ns")}
        out[p["pdk"]] = dict(flow_status=p["status"], statuses=s, by_rule=p.get("signoff", {}).get("by_rule"),
                             opensta_tt_parity=dict(status=p["opensta"]["status"],
                                                    max_difference_ns=p["opensta"]["max_difference_ns"],
                                                    clock_skew_rise_fall_ns=p["opensta"]["clock_skew_rise_fall_ns"]),
                             routed=dict(wires=p["geometry"]["wires"], vias=p["geometry"]["vias"],
                                         nets=p["geometry"]["routed_nets"], terminals=p["geometry"]["terminals"]),
                             cec=p["native_cec"], simulation=p["simulation"]["status"],
                             def_sha256=p["artifacts"]["def"]["sha256"])
    return out


def antenna(run_dir: Path, pdk: str) -> dict:
    verdict = json.loads((run_dir / f"{pdk}_signoff" / "antenna.json").read_text())
    out = dict(status=verdict["status"], failures=verdict["failures"], result=verdict["result"])
    log = run_dir / f"{pdk}_signoff" / "antenna" / "magic_antenna.log"
    if log.is_file():
        text = log.read_text(errors="replace")
        out["magic_violations"] = [
            dict(cell_use=m[1], plane=m[2], ratio=float(m[3]), limit=float(m[4]))
            for m in re.finditer(r"Cell: (\S+)\nAntenna violation detected at plane (\S+)\n"
                                 r"Effective antenna ratio ([0-9.]+) > limit ([0-9.]+)", text)]
    return out


def antenna_handoff(run_dir: Path, pdk: str, violations: list, cell_lef: Path) -> list:
    """Locate each Magic antenna violation in the routed DEF for the antenna
    ECO owners: the instance whose placement box contains the reported gate
    rectangle (Magic reports 5 nm database units after a 1 nm GDS read; the
    mapped point must fall inside the die, else the entry says so), that
    nets connected to that instance, and their routed wire length per layer."""
    text = (run_dir / f"{pdk}.def").read_text()
    dbu = int(re.search(r"UNITS DISTANCE MICRONS (\d+)", text)[1])
    sizes = {m[1]: (float(m[2]), float(m[3])) for m in
             re.finditer(r"(?ms)^MACRO (\S+)\n.*?^\s*SIZE ([0-9.]+) BY ([0-9.]+)", cell_lef.read_text())}
    comps = [(m[1], m[2], int(m[3]) / dbu, int(m[4]) / dbu) for m in
             re.finditer(r"(?m)^- (\S+) (\S+) \+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \)", text)]
    nets = {}
    body = text.split("\nNETS ", 1)[1].split("\nEND NETS", 1)[0]
    for block in re.split(r"\n(?=- )", body):
        if not block.startswith("- "):
            continue
        name = block.split()[1]
        pins = re.findall(r"\( (\S+) (\S+) \)", block.split("+ USE")[0])
        length = {}
        for m in re.finditer(r"(?:ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)", block):
            length[m[1]] = length.get(m[1], 0) + (abs(int(m[4]) - int(m[2])) + abs(int(m[5]) - int(m[3]))) / dbu
        nets[name] = dict(pins=pins, wire_um=length)
    out = []
    log = (run_dir / f"{pdk}_signoff" / "antenna" / "magic_antenna.log").read_text(errors="replace")
    for v, m in zip(violations, re.finditer(r"Gate rect \((-?\d+) (-?\d+)\) to \((-?\d+) (-?\d+)\)", log)):
        cx, cy = (int(m[1]) + int(m[3])) * 0.005 / 2, (int(m[2]) + int(m[4])) * 0.005 / 2
        hits = [(n, master) for n, master, x, y in comps if master in sizes
                and x <= cx <= x + sizes[master][0] and y <= cy <= y + sizes[master][1]
                and "fill" not in master and "tap" not in master]
        entry = dict(magic=v, gate_centre_um=[round(cx, 4), round(cy, 4)], instances=hits)
        if len(hits) == 1:
            inst = hits[0][0]
            entry["connected_nets"] = {n: dict(pins=[f"{a}/{b}" if a != "PIN" else f"port {b}" for a, b in d["pins"]],
                                          wire_um={k: round(x, 3) for k, x in d["wire_um"].items()})
                                   for n, d in nets.items() if any(a == inst for a, _ in d["pins"])
                                   and not n.startswith(("VDD", "VSS"))}
        out.append(entry)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ("pass1", "pass2", "native-gds", "router-signoff", "rustroad", "scratch", "out"):
        ap.add_argument("--" + name, type=Path, required=True)
    a = ap.parse_args()
    r1, r2 = json.loads((a.pass1 / "summary.json").read_text()), json.loads((a.pass2 / "summary.json").read_text())
    rs = json.loads((a.router_signoff / "summary.json").read_text())
    a.scratch.mkdir(parents=True, exist_ok=True)
    determinism = {}
    for pdk in ("sky130", "gf180"):
        d = dict(def_pass1=sha256(a.pass1 / f"{pdk}.def"), def_pass2=sha256(a.pass2 / f"{pdk}.def"),
                 gds_pass1_native=sha256(a.native_gds / f"{pdk}.gds"))
        gds = a.scratch / f"{pdk}_pass2.gds"
        run([a.rustroad, "--workspace", a.pass2 / f"{pdk}_physical", "export-gds", "--cell-gds", CELL_GDS[pdk],
             "--layer-map", ROOT / f"fixtures/pdk/{pdk}-gds-map.json", "--out", gds])
        d["gds_pass2_export"] = sha256(gds)
        for rc in ("min", "nom", "max"):
            pair = []
            for tag, src in (("pass1", a.pass1), ("pass2", a.pass2)):
                spef = a.scratch / f"{pdk}_{tag}_{rc}.spef"
                run([a.rustroad, "--workspace", src / f"{pdk}_physical", "extract", "--rc-profile",
                     ROOT / f"fixtures/pdk/{pdk}-rc-{rc}.json", "--corner", rc, "--out", spef])
                pair.append(sha256(spef))
            d[f"spef_{rc}"] = pair
        d["status"] = ("passed" if d["def_pass1"] == d["def_pass2"] and d["gds_pass1_native"] == d["gds_pass2_export"]
                       and all(d[f"spef_{rc}"][0] == d[f"spef_{rc}"][1] for rc in ("min", "nom", "max"))
                       else "failed")
        determinism[pdk] = d
    router = {}
    for tag, e in rs["pdks"].items():
        r = e["routed"]
        router[tag] = dict(drc=r["drc_status"], magic=r["magic"]["total"], klayout=r["klayout"]["items"],
                           antenna=dict(status=r["antenna_status"], items=r["antenna"]["items"],
                                        oracle=r["antenna"]["oracle"]),
                           density=dict(status=r["density_status"], items=r["density"]["items"]),
                           lvs=r["lvs"]["status"],
                           controls={k: v.get("status", dict(magic=v.get("magic"), klayout=v.get("klayout")))
                                     for k, v in e["controls"].items()},
                           access_test=dict(status=e["access_test"]["drc_status"], layouts=e["access_test"]["tests"]))
    evidence = dict(
        schema="rustroad.requalification.v1",
        scope=r2["scope"], producer=r2["producer"],
        tools={k: dict(sha256=v["sha256"], version=v.get("version")) for k, v in r2["tools"].items()},
        pass1=dict(status=r1["status"], functional_status=r1["functional_status"], wall_seconds=r1["wall_seconds"],
                   peak_rss_kib=r1["peak_rss_kib"], evidence_gds="reference_klayout_streamout",
                   pdks=statuses(r1)),
        pass2=dict(status=r2["status"], functional_status=r2["functional_status"], wall_seconds=r2["wall_seconds"],
                   peak_rss_kib=r2["peak_rss_kib"], evidence_gds="native export-gds of pass 1",
                   blocking=[f"{b['pdk']}:{b['item']}={b['status']}" for b in r2["blocking"]],
                   failed_items=r2["failed_items"], pdks=statuses(r2)),
        determinism=determinism,
        antenna={pdk: antenna(a.pass2, pdk) for pdk in ("sky130", "gf180")},
        antenna_eco_handoff={},
        router_signoff=dict(status=rs["status"], wall_seconds=rs["wall_seconds"], peak_rss_kib=rs["peak_rss_kib"],
                            pdks=router),
        related=dict(gds="docs/evidence/gds-verification.json", def_segmentation="docs/evidence/def-segmentation.json",
                     extraction="docs/evidence/extraction-requalification.json",
                     timing="docs/evidence/timing-baseline.json"))
    lefs = {"sky130": "libs.ref/sky130_fd_sc_hd/lef/sky130_fd_sc_hd.lef",
            "gf180": "libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef"}
    for pdk, info in evidence["antenna"].items():
        if info.get("magic_violations"):
            lef = CELL_GDS[pdk].parents[3] / lefs[pdk]
            evidence["antenna_eco_handoff"][pdk] = antenna_handoff(a.pass2, pdk, info["magic_violations"], lef)
    a.out.write_text(json.dumps(evidence, indent=1, sort_keys=True) + "\n")
    print(json.dumps(dict(out=str(a.out), determinism={k: v["status"] for k, v in determinism.items()})))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
