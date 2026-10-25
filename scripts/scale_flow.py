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
  7. proves RTL == final netlist with ``rustsys check synth``; a planted
     mutation of the same netlist (one 2-input AND/OR or NAND/NOR master
     swapped for its dual with identical pins) must come back
     ``not-equivalent``, otherwise the CEC verdict is ``unverified``;
  8. repeats steps 2-5 in a fresh directory and compares DEF, GDS and both
     Verilog exports byte for byte.

A failing build step marks its own gating item ``failed`` (synth, floorplan
-> floorplan_reproduced, import/place/clock-tree -> place, route/antenna ->
route, exports -> export); the items after it are ``unverified`` (not run)
and the PDK status is ``failed``.

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
GATING = ("inputs", "synth", "floorplan_reproduced", "place", "route", "export", "gds_xor", "drc_magic",
          "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout", "cec", "determinism")
# Build-step label (``<pdk>_<a|b>_<step><n>``) -> gating item that owns it.
STEP_ITEMS = (("synth", "synth"), ("floorplan", "floorplan_reproduced"), ("import", "place"),
              ("place", "place"), ("cts", "place"), ("route", "route"), ("antenna", "route"),
              ("export", "export"))
SIGNOFF_ITEMS = ("gds_xor", "drc_magic", "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout")
# rustsys check synth verdicts (JSON ``verdict``): only these two are definitive.
CEC_PASS, CEC_FAIL = "equivalent", "not-equivalent"
# Planted-mutation control: dual masters with identical pin names.
MUTATIONS = (("and2", "or2"), ("or2", "and2"), ("nand2", "nor2"), ("nor2", "nand2"))
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


PRLIMIT = shutil.which("prlimit")
GNU_TIME = os.environ.get("RUSTEDA_GNU_TIME") or shutil.which("time")


class Runner:
    """Host commands with receipts (wall time, peak RSS).

    Peak RSS comes from GNU time wrapped around the tool (``%M`` of its own
    child): ``wait4`` on a child spawned from this interpreter records the
    interpreter's resident pages at exec (Linux keeps the old address
    space's high-water mark), ~15-25 MiB that the tool never used. The
    address-space cap is applied by ``prlimit``. Without GNU time the
    receipt says ``rss_source: wait4`` (an upper bound)."""

    def __init__(self, root: Path):
        self.root = root
        self.receipts: list[dict] = []
        (root / "receipts").mkdir(parents=True, exist_ok=True)

    def run(self, label: str, argv: list, timeout: int = 1800, env: dict | None = None) -> dict:
        argv = [str(a) for a in argv]
        d = self.root / "receipts" / label
        d.mkdir(parents=True, exist_ok=True)
        out, err = d / "stdout.txt", d / "stderr.txt"
        maxrss = d / "maxrss.txt"
        if maxrss.exists():
            maxrss.unlink()

        inherited = resource.getrlimit(resource.RLIMIT_AS)
        memory_limit = min([HOST_AS_BYTES] + [x for x in inherited if x != resource.RLIM_INFINITY])

        def limit():
            resource.setrlimit(resource.RLIMIT_AS, (memory_limit, memory_limit))

        spawn = dict(preexec_fn=limit) if PRLIMIT is None else {}
        command = argv if PRLIMIT is None else [PRLIMIT, f"--as={memory_limit}", "--", *argv]
        if GNU_TIME:
            command = [GNU_TIME, "-f", "%M", "-o", str(maxrss), *command]
        start = time.monotonic()
        with out.open("wb") as o, err.open("wb") as e:
            proc = subprocess.Popen(command, stdout=o, stderr=e, env={**os.environ, **(env or {})},
                                    start_new_session=True, **spawn)
            timed_out = False
            deadline = start + timeout
            status, usage = None, None
            while True:
                pid, status, usage = os.wait4(proc.pid, os.WNOHANG)
                if pid:
                    break
                if time.monotonic() > deadline:
                    os.killpg(proc.pid, 9)
                    timed_out = True
                    _, status, usage = os.wait4(proc.pid, 0)
                    break
                time.sleep(0.02)
        wall = time.monotonic() - start
        code = os.waitstatus_to_exitcode(status)
        proc.returncode = code  # wait4 already reaped it; Popen must not wait again.
        peak, source = usage.ru_maxrss, "wait4"
        if timed_out:
            # The killed wrapper's own rusage says nothing about the tool.
            peak, source = None, "unmeasured (timed out)"
        elif GNU_TIME:
            try:
                peak, source = int(maxrss.read_text().split()[-1]), "gnu-time"
            except (OSError, ValueError, IndexError):
                pass
        receipt = dict(label=label, argv=argv, exit_code=code, timed_out=timed_out,
                       memory_limit_bytes=memory_limit,
                       wall_seconds=round(wall, 3), peak_rss_kib=peak, rss_source=source,
                       stdout=str(out), stderr=str(err))
        (d / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        self.receipts.append(receipt)
        return receipt

    def ok(self, label: str, argv: list, **kw) -> dict:
        r = self.run(label, argv, **kw)
        if r["exit_code"] != 0:
            tail = Path(r["stderr"]).read_text(errors="replace")[-800:]
            raise StepError(label, f"{label}: exit {r['exit_code']}: {tail}")
        return r

    def json(self, label: str, argv: list, **kw) -> dict:
        r = self.ok(label, argv, **kw)
        return json.loads(Path(r["stdout"]).read_text())


class StepError(RuntimeError):
    """A build step failed; ``label`` is its receipt label."""

    def __init__(self, label: str, message: str):
        super().__init__(message)
        self.label = label


def step_item(label: str) -> str:
    """Gating item of a receipt label such as ``sky130_a_route0``."""
    step = label.split("_", 2)[2] if label.count("_") >= 2 else label
    for prefix, key in STEP_ITEMS:
        if step.startswith(prefix):
            return key
    return "route"


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


PORT_BUFFER_PREFIX = "rr_portbuf_"
PORT_TIE_PREFIX = "rr_porttie_"


def port_tie_cells(tag: str) -> dict[str, set[str]]:
    """Tie masters of the PDK's scale physical profile and their constant
    outputs (tie_high/tie_low without a rail input, constant_cells)."""
    cfg = json.loads((ROOT / f"fixtures/pdk/{tag}-scale-physical.json").read_text())
    out: dict[str, set[str]] = {}
    for k in ("tie_high", "tie_low"):
        t = cfg.get(k)
        if t and not t.get("rail_input"):
            out.setdefault(t["cell"], set()).add(t["pin"])
    for c in cfg.get("constant_cells") or []:
        out.setdefault(c["cell"], set()).update(c.get("high", []) + c.get("low", []))
    return out


def port_buffer_cell(tag: str) -> tuple[str, str, str]:
    """(cell, input pin, output pin) of the smallest Liberty-checked identity
    buffer of the PDK's delay-cell profile (sky130 buf_1 A->X, gf180 buf_1 I->Z)."""
    prof = json.loads((ROOT / f"fixtures/pdk/{tag}-delay-cells.json").read_text())
    b = prof["buffers"][0]
    if not b["function_check"].startswith("identity"):
        raise ValueError(f"{tag}: first profile buffer {b['name']} is not a checked identity")
    return b["name"], b["input_pin"], b["output_pin"]


def _port_bits(text: str, direction: str) -> set[str]:
    bits = set()
    for m in re.finditer(r"(?m)^\s*" + direction + r"\s+wire\s+(?:\[\s*(-?\d+)\s*:\s*(-?\d+)\s*\]\s+)?(\w+)", text):
        if m[1] is not None:
            hi, lo = int(m[1]), int(m[2])
            bits.update(f"{m[3]}[{i}]" for i in range(min(hi, lo), max(hi, lo) + 1))
        else:
            bits.add(m[3])
    return bits


def buffer_port_aliases(text: str, cell: tuple[str, str, str],
                        ties: dict[str, set[str]] | None = None) -> tuple[str, list[dict]]:
    """Give every output port bit its own net (OpenROAD ``buffer_ports`` /
    OpenLane ``insbuf`` practice for LVS).

    Two output ports assigned the same net (SERV: ``o_ext_rs1 = o_dbus_adr``,
    constant bits shared by several ports) are one node in the layout, so the
    extracted top cell has one pin where the reference has two (Netgen pin
    mismatch, Magic "ports electrically shorted"). For each group of
    ``assign PORT = NET;`` with the same NET, the first assign (in netlist
    order) is kept and every other port gets an identity buffer
    ``CELL rr_portbuf_<k> (.IN(NET), .OUT(PORT));`` in place of its assign; an
    output assigned from an input port (feedthrough) or from another output
    port is buffered too. A constant (a net driven by a tie cell of ``ties``:
    master -> constant output pins) is not buffered: the extra port gets its
    own copy of the tie cell ``MASTER rr_porttie_<k> (.PIN(PORT));`` so it
    stays a tie-cell output (a constant-driven endpoint). A netlist without
    such aliases is returned unchanged. The CEC oracles check the result
    against the RTL."""
    name, pin_in, pin_out = cell
    outs, ins = _port_bits(text, "output"), _port_bits(text, "input")
    assign = re.compile(r"(?m)^(\s*)assign\s+(\\\S+\s|[\w\[\]]+)\s*=\s*(\\\S+\s|[\w\[\]']+)\s*;[ \t]*$")
    seen: set[str] = set()
    inserted: list[dict] = []
    tie_of: dict[str, tuple[str, str]] = {}
    for m in re.finditer(r"(?m)^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$", text):
        for pin, net in re.findall(r"\.(\w+)\(([^()]*)\)", m[3]):
            if pin in (ties or {}).get(m[1], ()):
                tie_of[net.strip().lstrip("\\")] = (m[1], pin)

    def ident(x: str) -> str:
        return x.strip().lstrip("\\")

    def sub(m):
        lhs, rhs = ident(m[2]), ident(m[3])
        if lhs not in outs:
            return m[0]
        if rhs not in ins and rhs not in outs and rhs not in seen:
            seen.add(rhs)
            return m[0]
        k = len(inserted)
        src = m[3].strip() + (" " if m[3].startswith("\\") else "")
        dst = m[2].strip() + (" " if m[2].startswith("\\") else "")
        if rhs in tie_of:
            master, pin = tie_of[rhs]
            inserted.append(dict(instance=f"{PORT_TIE_PREFIX}{k}", net=rhs, port=lhs, cell=master))
            return f"{m[1]}{master} {PORT_TIE_PREFIX}{k} (.{pin}({dst}));"
        inserted.append(dict(instance=f"{PORT_BUFFER_PREFIX}{k}", net=rhs, port=lhs, cell=name))
        return f"{m[1]}{name} {PORT_BUFFER_PREFIX}{k} (.{pin_in}({src}), .{pin_out}({dst}));"
    out = assign.sub(sub, text)
    return out, inserted


def pdk_entry(tag: str) -> dict:
    data = json.loads((ROOT / "fixtures/pdk/local-real-pdks.json").read_text())
    return [p for p in data["pdks"] if p["tag"] == tag][0]


def verify_tracked(m: dict, tag: str) -> dict:
    rows, bad = {}, []
    entries = {f"rtl.{i}": r for i, r in enumerate(m["rtl"])}
    entries["cell_policy"] = m["cell_policy"]
    spec = m["pdks"][tag]
    for key in ("floorplan", "physical_config", "routing_config", "gds_layer_map", "sdc_clock"):
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


def build(r: Runner, run_dir: Path, m: dict, tag: str, args, label: str, facts: dict | None = None) -> dict:
    """Steps 2-5 in run_dir; returns artifact paths and step facts.

    ``facts`` (filled in place) keeps what finished before a StepError;
    ``facts["completed"]`` lists the finished gating items in order."""
    spec = m["pdks"][tag]
    p = pdk_entry(tag)
    lib = Path(p["liberty"]["path"])
    run_dir.mkdir(parents=True, exist_ok=True)
    facts = {} if facts is None else facts
    done = facts.setdefault("completed", [])
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
    text, port_buffers = buffer_port_aliases(text, port_buffer_cell(tag), port_tie_cells(tag))
    netlist = run_dir / "netlist.v"
    netlist.write_text(text)
    facts["renamed_escaped_wires"] = renamed
    facts["port_buffers"] = len(port_buffers)
    done.append("synth")
    fp = m["pdks"][tag].get("floorplan_recipe") or m["recipe"]["floorplan"]
    floor, phys = run_dir / "floorplan.json", run_dir / "physical.json"
    road = [args.rustroad]
    clock = ["--no-clock"] if m["clock"]["port"] is None else ["--clock-port", m["clock"]["port"]]
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
    done.append("floorplan_reproduced")
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
        if m["clock"]["port"] is not None:
            a = r.json(f"{label}_cts{eco}", rr + ["clock-tree", "--base-rev", a["revision"], "--sdc",
                                                  resolve(spec["sdc_clock"]["path"])])
            facts["placement_after_cts"] = a.get("placement")
            facts["clock_buffers"] = a.get("clock_buffers")
        if eco == 0:
            done.append("place")
        a = r.json(f"{label}_route{eco}", rr + ["route", "--base-rev", a["revision"], "--scope", "all",
                                                "--config", resolve(spec["routing_config"]["path"])],
                   env={"RUSTROAD_ROUTE_TRACE": "1"})
        facts["route"] = {k: a.get(k) for k in ("routed_nets", "routing_segments", "routing_expansions",
                                                 "cells", "tap_cells", "filler_cells", "route_memory")}
        route_receipt = r.receipts[-1]
        facts["route"]["peak_rss_kib"] = route_receipt["peak_rss_kib"]
        facts["route"]["rss_source"] = route_receipt.get("rss_source")
        # Rows are sorted worst first and every violated row is kept; the
        # cap only bounds the report size for large blocks.
        ant = r.json(f"{label}_antenna{eco}", rr + ["check-antenna", "--max-rows", "5000"])
        facts.setdefault("native_antenna", []).append(dict(
            status=ant.get("status"), violations=ant.get("violations"),
            checks=(ant.get("stats") or {}).get("checks")))
        if not ant.get("violations") or eco >= 2:
            break
        report = run_dir / f"antenna{eco + 1}.json"
        report.write_text(Path(r.receipts[-1]["stdout"]).read_text())
        fixed = run_dir / f"netlist_eco{eco + 1}.v"
        try:
            r.json(f"{label}_antenna_eco{eco + 1}", [
                args.rustroad, "antenna-eco", "--netlist", current, "--antenna-report", report,
                "--delay-cells", resolve(spec["delay_cells"]["path"]), "--out", fixed,
                "--plan-out", run_dir / f"antenna_plan{eco + 1}.json"])
        except StepError as e:
            # e.g. violations on clock-tree nets, which antenna-eco does not
            # repair: keep the routed result; the foundry antenna check reports.
            facts["antenna_eco_error"] = str(e)[:1500]
            break
        eco += 1
        current = fixed
    facts["antenna_eco_iterations"] = eco
    facts["final_netlist"] = str(current)
    done.append("route")
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
    done.append("export")
    return dict(outputs=outs, facts=facts, workspace=ws)


def signoff_checks(run_dir: Path, tag: str, top: str, outs: dict, timeout: int = 1800) -> dict:
    ref = run_dir / "reference.gds"
    receipt = def2gds_reference.stream(outs["def"], tag, ref, ROOT / "fixtures/flow/signoff-pinned.json")
    if receipt["status"] != "passed":
        return dict(statuses={k: item("unverified", "reference streamout failed") for k in
                              ("gds_xor", "drc_magic", "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout")},
                    reference=receipt)
    ns = argparse.Namespace(gds=outs["gds"], top=top, pdk=tag, out=run_dir / "signoff", netlist=outs["powered"],
                            against=ref, against_top=top, manifest=ROOT / "fixtures/flow/signoff-pinned.json",
                            threads=4, memory="4g", timeout=timeout, plant=None)
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


def cec_verdict(res: dict) -> dict:
    """Classify one ``rustsys check synth --print json`` receipt.

    ``equivalent`` passes; ``not-equivalent`` - or any other verdict that
    carries a mismatch with a counterexample - fails; everything else
    (undecided, error, initial-state-refinement, timeout, unreadable JSON)
    is unverified."""
    try:
        report = json.loads(Path(res["stdout"]).read_text())
    except (json.JSONDecodeError, OSError):
        report = {}
    verdict = report.get("verdict") if isinstance(report, dict) else None
    mismatches = report.get("mismatches") if isinstance(report, dict) else None
    mismatches = mismatches if isinstance(mismatches, list) else []
    extra = dict(verdict=verdict, exit_code=res["exit_code"], timed_out=res["timed_out"],
                 mismatches=len(mismatches),
                 first_mismatch=({k: mismatches[0].get(k) for k in ("kind", "name")}
                                 if mismatches and isinstance(mismatches[0], dict) else None))
    if verdict == CEC_PASS and not mismatches and not res["timed_out"]:
        return item("passed", **extra)
    if verdict == CEC_FAIL or (verdict != CEC_PASS and mismatches):
        return item("failed", f"verdict {verdict} with {len(mismatches)} mismatch(es)", **extra)
    return item("unverified", f"no definitive verdict ({verdict}, exit {res['exit_code']}"
                + (", timed out" if res["timed_out"] else "") + ")", **extra)


def liberty_pins(lib: Path) -> dict[str, frozenset]:
    """Cell name -> pin names of a Liberty file (for the mutation table only)."""
    out, cell, pins = {}, None, set()
    head = re.compile(r'^\s*cell\s*\(\s*"?([\w$]+)"?\s*\)')
    pin = re.compile(r'^\s*pin\s*\(\s*"?([\w$\[\]]+)"?\s*\)')
    with lib.open(errors="replace") as f:
        for line in f:
            m = head.match(line)
            if m:
                if cell is not None:
                    out[cell] = frozenset(pins)
                cell, pins = m[1], set()
                continue
            m = pin.match(line)
            if m and cell is not None:
                pins.add(m[1])
    if cell is not None:
        out[cell] = frozenset(pins)
    return out


def plant_mutation(text: str, cells: dict[str, frozenset], skip: int = 0) -> tuple[str, dict] | None:
    """Swap the master of the ``skip``-th eligible instance for its dual
    (AND2<->OR2, NAND2<->NOR2, same drive and identical Liberty pins)."""
    seen = 0
    for m in re.finditer(r"^(\s*)([\w$]+)(\s+)(\\?\S+)(\s*\()", text, re.M):
        master = m[2]
        for a, b in MUTATIONS:
            hit = re.fullmatch(r"(.*__)" + a + r"(_\d+)", master)
            if not hit:
                continue
            dual = hit[1] + b + hit[2]
            if master not in cells or cells.get(dual) != cells[master]:
                continue
            if seen < skip:
                seen += 1
                break
            mutated = text[:m.start(2)] + dual + text[m.end(2):]
            return mutated, dict(instance=m[4], master=master, mutated_to=dual)
    return None


def cec(r: Runner, m: dict, tag: str, netlist: Path, args) -> dict:
    lib = Path(pdk_entry(tag)["liberty"]["path"])
    rtl = [resolve(x["path"]) for x in m["rtl"]]
    v = m["verification"]

    def check(label: str, path: Path) -> dict:
        return r.run(label, [args.rustsys, "--threads", "2", "--progress", "off", "check", "synth", "--top",
                             m["top"], "--lib", lib, "--netlist", path, "--budget", v["rustsys_budget"],
                             "--model-budget", v["rustsys_model_budget"], "--sweep-max-candidates",
                             v["rustsys_sweep_max_candidates"], "--print", "json", *rtl])

    out = cec_verdict(check(f"{tag}_cec", netlist))
    if out["status"] != "passed":
        return out
    if args.no_cec_control:
        return item("unverified", "equivalent, but the planted-mutation control was skipped",
                    verdict=out.get("verdict"), control=item("unverified", "not requested"))
    # Planted-mutation control: the same check must reject a netlist with one
    # dual master; up to three candidates (a swapped gate can be redundant).
    cells = liberty_pins(lib)
    text = netlist.read_text()
    tries = []
    for k in range(3):
        planted = plant_mutation(text, cells, k)
        if planted is None:
            break
        mutated, what = planted
        path = netlist.parent / f"cec_control{k}.v"
        path.write_text(mutated)
        verdict = cec_verdict(check(f"{tag}_cec_control{k}", path))
        tries.append(dict(what, status=verdict["status"], verdict=verdict.get("verdict")))
        if verdict["status"] == "failed":
            out["control"] = item("passed", "planted mutation detected", tries=tries)
            return out
        if verdict["status"] == "unverified":
            break
    out["control"] = item("failed" if tries else "unverified",
                          "planted mutation not detected" if tries else "no eligible AND2/OR2/NAND2/NOR2 instance",
                          tries=tries)
    return item("unverified", f"CEC control {out['control']['status']}: the verdict is not trusted",
                verdict=out.get("verdict"), control=out["control"])


def flow(m: dict, tag: str, args, out: Path) -> dict:
    root = out / tag
    r = Runner(root)
    started = time.monotonic()
    result: dict = dict(pdk=tag, statuses={})
    st = result["statuses"]
    st["inputs"] = verify_tracked(m, tag)
    facts: dict = {}
    result["facts"] = facts
    try:
        a = build(r, root / "a", m, tag, args, f"{tag}_a", facts)
    except StepError as e:
        # The failing step owns the failure; finished steps keep their
        # verdicts; everything after it was not run.
        failed = step_item(e.label)
        done = facts.get("completed", [])
        if "synth" in done:
            st["synth"] = item("passed", **({} if "cells" not in facts.get("synth", {}) else facts["synth"]))
        if "floorplan_reproduced" in done:
            st["floorplan_reproduced"] = item("passed" if facts["floorplan_reproduced"] else "failed")
        for k in ("place", "route", "export"):
            if k in done:
                st[k] = item("passed")
        st[failed] = item("failed", f"step {e.label} failed: " + str(e)[:2000], failed_step=e.label)
        for k in GATING:
            st.setdefault(k, item("unverified", f"not run: step {e.label} failed"))
        result["error"] = str(e)[:4000]
        result["failed_step"] = e.label
        result["status"] = worst(st[k]["status"] for k in GATING)
        result["resources"] = dict(wall_seconds=round(time.monotonic() - started, 3),
                                   host_peak_rss_kib=max((x["peak_rss_kib"] for x in r.receipts
                                                          if x["peak_rss_kib"] is not None), default=None),
                                   unmeasured_steps=[x["label"] for x in r.receipts if x["peak_rss_kib"] is None],
                                   steps={x["label"]: dict(wall_seconds=x["wall_seconds"],
                                                           peak_rss_kib=x["peak_rss_kib"]) for x in r.receipts})
        result["receipts"] = [dict(label=x["label"], exit_code=x["exit_code"], wall_seconds=x["wall_seconds"],
                                   peak_rss_kib=x["peak_rss_kib"]) for x in r.receipts]
        return result
    st["synth"] = item("passed", **({} if "cells" not in facts["synth"] else facts["synth"]))
    st["floorplan_reproduced"] = item("passed" if facts["floorplan_reproduced"] else "failed",
                                      None if facts["floorplan_reproduced"] else
                                      "regenerated floorplan/physical config differ from the tracked files")
    st["place"] = item("passed", placement=facts.get("placement_after_cts") or facts.get("placement"))
    st["route"] = item("passed", **facts["route"])
    st["export"] = item("passed")
    if not args.no_signoff:
        s = signoff_checks(root / "a", tag, m["top"], a["outputs"],
                           int(m["budgets"].get("container_timeout_s", 1800)))
        st.update(s["statuses"])
        result["signoff"] = {k: v for k, v in s.items() if k != "statuses"}
    else:
        for k in SIGNOFF_ITEMS + ("density",):
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
            st["determinism"] = item("failed", f"rerun failed at {e.label}: {e}"[:2000], failed_step=e.label)
    else:
        st["determinism"] = item("unverified", "rerun not requested")
    host = [x for x in r.receipts]
    result["resources"] = dict(
        wall_seconds=round(time.monotonic() - started, 3),
        host_peak_rss_kib=max((x["peak_rss_kib"] for x in host if x["peak_rss_kib"] is not None), default=None),
        unmeasured_steps=[x["label"] for x in host if x["peak_rss_kib"] is None],
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
    ap.add_argument("--no-cec-control", action="store_true",
                    help="skip the planted-mutation control (the CEC item then stays unverified)")
    args = ap.parse_args(argv)
    m = json.loads(resolve(args.design).read_text())
    if m.get("schema") != "rustroad.design.v2" or (m.get("recipe") or {}).get("physical") != "track":
        raise SystemExit("design manifest must be rustroad.design.v2 with the track recipe")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    pdks = args.pdk or list(m["pdks"])
    per = {tag: flow(m, tag, args, args.out) for tag in pdks}
    status = worst(p["status"] for p in per.values())
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
