#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Derive the clock period of every design x PDK from pre-layout timing (owner rule).

usage: derive_clock_period.py --work DIR [--design NAME ...] [--write | --check]

Owner decision (2026-10-24, clock period rule), applied before any physical
step, uniformly to every design and PDK:

    period = max(10 ns, round_up(1.25 x pre-layout ss+max critical-path delay, 1 ns))

Method (the same for every design; nothing is tuned per design):

1. The mapped netlist exactly as the gate builds it (``hold_closure.Flow.synth``:
   pinned RustSys with the cell-policy ``--dont-use`` arguments on the tt
   Liberty, or the design's literal netlist; then ``scale_flow.deconflict`` and
   ``buffer_port_aliases``).
2. The signoff SDC rendered from the RTL port list (``rtl2gds_design.render_sdc``)
   on the design's reference clock SDC (10 ns, ``sdc.reference_clock``) with
   ``set_propagated_clock`` removed: ideal clock, no CTS.
3. Pinned RustSTA on that netlist with the ss Liberty only (corner ``ss``) and
   no SPEF. The wire model is the one RustSTA selects without parasitics and
   is required, per PDK, with ``--require-wire-model``: GF180 ``ideal`` (its
   Liberty has no wire-load table), SKY130 ``wireload_lumped`` (its Liberty
   ``default_wire_load``, an upper bound of ideal wires; RustSTA has no switch to
   force ideal there). That is the documented pre-layout scope of
   ``scripts/critical_path.py``.
4. Critical-path delay ``d`` = reference period - worst ss setup slack: the
   period at which the worst path has zero slack with the same setup time,
   uncertainty, I/O delays and ideal clock (for a flop-to-flop path the data
   arrival plus setup time plus uncertainty; for an I/O path it includes the
   0.2 ns input/output delays). The worst path's start/end, arrival and
   required times are recorded.
5. ``period = max(10, ceil(1.25 d))`` in whole ns. The per-PDK clock SDC is the
   reference clock SDC with only ``-period`` (and the ``-waveform`` fall edge
   at period/2) replaced; it is written to ``fixtures/designs/<name>/<pdk>-cts.sdc``.
6. Self-check: RustSTA is re-run with the derived SDC; its worst slack must be
   ``period - d`` within 1 ps (the delay does not depend on the period).

The record (``fixtures/designs/<name>/clock-period.json``) keeps every input by
sha256 (RTL, reference SDC, I/O template, cell policy, Liberty, tools), the
netlist sha256, the measured path and the arithmetic. ``--check`` recomputes
everything and exits 1 if a record or SDC differs. Run it before
``scale_manifests.py`` (which freezes the SDC by hash in the manifest).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import rtl2gds_design as D  # noqa: E402
from scale_flow import buffer_port_aliases, deconflict, port_buffer_cell, port_tie_cells  # noqa: E402

SCHEMA = "rustroad.clock-period.v1"
RULE = "period = max(10 ns, round_up(1.25 x pre-layout ss+max critical-path delay, 1 ns))"
FLOOR_NS, FACTOR, STEP_NS = 10, 1.25, 1
WIRE_MODEL = {"gf180": "ideal", "sky130": "wireload_lumped"}
SS = {"sky130": "ss_100C_1v60", "gf180": "ss_125C_3v00"}
TT = {"sky130": "tt_025C_1v80", "gf180": "tt_025C_3v30"}
REQUIRED = ["pipeline", "dense", "counter", "uart_tx", "fifo", "alu", "regfile", "mult"]
STRETCH = ["pipeline_track", "serv", "rv_core"]
TOOLS = ROOT / "fixtures/flow/rtl2gds-tools.json"
TIMEOUT_S = 1800


def sha256(path: Path) -> str:
    return D.sha256_file(Path(path))


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def run_json(argv: list, timeout: int = TIMEOUT_S) -> dict:
    r = subprocess.run([str(a) for a in argv], capture_output=True, text=True, timeout=timeout)
    try:
        out = json.loads(r.stdout)
    except json.JSONDecodeError as e:
        raise RuntimeError(f"{Path(str(argv[0])).name} {argv[1:6]}: exit {r.returncode}: {r.stderr[-600:]}") from e
    return out


def period_text(value: float) -> str:
    return str(int(value)) if float(value).is_integer() else repr(float(value))


def set_period(clock_sdc: str, period: int) -> str:
    """The reference clock SDC with only -period (and the waveform fall edge) replaced."""
    lines, hits = [], 0
    for line in clock_sdc.splitlines(keepends=True):
        if line.lstrip().startswith("create_clock"):
            hits += 1
            new, n = re.subn(r"-period\s+[0-9.]+", f"-period {period_text(period)}", line)
            if n != 1:
                raise RuntimeError(f"create_clock without one -period: {line!r}")
            new, n = re.subn(r"-waveform\s+\{\s*0\s+[0-9.]+\s*\}", f"-waveform {{0 {period_text(period / 2)}}}", new)
            if n == 0 and "-waveform" in new:
                raise RuntimeError(f"unsupported -waveform: {line!r}")
            line = new
        lines.append(line)
    if hits != 1:
        raise RuntimeError(f"expected one create_clock, found {hits}")
    return "".join(lines)


def reference_period(clock_sdc: str) -> float:
    m = re.search(r"create_clock\b[^\n]*-period\s+([0-9.]+)", clock_sdc)
    if not m:
        raise RuntimeError("no create_clock -period")
    return float(m[1])


def tools() -> dict:
    data = json.loads(TOOLS.read_text())["tools"]
    out = {}
    for name in ("rustsys", "rusteda", "ruststa", "yosys", "verilator"):
        p = Path(data[name]["path"])
        p = p if p.is_absolute() else ROOT / p
        got = sha256(p) if p.is_file() else None
        if got != data[name]["sha256"]:
            raise SystemExit(f"pinned {name} {p}: sha256 {got} != {data[name]['sha256']}")
        out[name] = dict(path=p, sha256=got)
    return out


def liberty(tag: str, corner: str) -> Path:
    profiles = json.loads((ROOT / "fixtures/pdk/local-routing.json").read_text())
    tt = Path(next(p for p in profiles["pdks"] if p["tag"] == tag)["liberty"]["path"])
    return tt.with_name(tt.name.replace(TT[tag], corner))


def ports_of(m: dict, t: dict, work: Path) -> list:
    import rtl2gds_logic as L
    from verify_physical import PhysicalHarness
    (work / "ports").mkdir()
    h = PhysicalHarness(work / "ports", argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=600,
                                                            tolerance_ns=0.001))
    ports, _ = L.rtl_ports(h, {k: v["path"] for k, v in t.items()}, [D.resolve(r["path"]) for r in m["rtl"]],
                           m["top"])
    return ports


def mapped_netlist(m: dict, tag: str, t: dict, policy: dict, out: Path) -> dict:
    spec = m["pdks"][tag]
    raw = out.with_suffix(".rustsys.v")
    if spec.get("netlist"):
        raw.write_bytes(D.resolve(spec["netlist"]["path"]).read_bytes())
        source = dict(kind="literal", path=spec["netlist"]["path"])
    else:
        r = subprocess.run([str(t["rustsys"]["path"]), "--threads", "2", "--progress", "off", "synth", "--backend", "cpu",
                            "--top", m["top"], "--lib", str(liberty(tag, TT[tag])),
                            *policy["pdks"][tag]["rustsys_args"], "--out", str(raw), "--print", "json",
                            *[str(D.resolve(x["path"])) for x in m["rtl"]]],
                           capture_output=True, text=True, timeout=TIMEOUT_S)
        if r.returncode != 0:
            raise RuntimeError(f"{m['name']} {tag}: RustSys synth exit {r.returncode}: {r.stderr[-600:]}")
        rep = json.loads(r.stdout)
        source = dict(kind="rustsys", cells=sum(rep["cells"].values()), area=rep.get("area"))
    text, _ = deconflict(raw.read_text())
    text, _ = buffer_port_aliases(text, port_buffer_cell(tag), port_tie_cells(tag))
    out.write_text(text)
    return dict(source, sha256=sha256(out), instances=len(re.findall(r"(?m)^\s*\S+__\S+\s+\S+\s*\(", text)))


def sta(t: dict, tag: str, netlist: Path, sdc: Path, ws: Path) -> dict:
    if ws.exists():
        shutil.rmtree(ws)
    run_json([t["rusteda"]["path"], "-w", ws, "--format", "json", "init"])
    a = run_json([t["rusteda"]["path"], "-w", ws, "--format", "json", "import", "--base-rev", "rev0", "-q",
                  "--lib", liberty(tag, SS[tag]), "--corner", "ss", "--netlist", netlist, "--sdc", sdc])
    if not a.get("ok"):
        raise RuntimeError(f"import failed: {a.get('error')}")
    r = run_json([t["ruststa"]["path"], "-w", ws, "--format", "json", "run", "--base-rev", a["new_rev"],
                  "--backend", "cpu", "--precision", "f64", "--threads", "2", "--max-paths", "8",
                  "--require-wire-model", WIRE_MODEL[tag], "-q"])
    if not r.get("ok"):
        raise RuntimeError(f"ruststa run failed: {r.get('error')}")
    d = run_json([t["ruststa"]["path"], "-w", ws, "--format", "json", "report", "--rev", r["new_rev"], "--section",
                  "paths", "--kind", "setup", "--corner", "ss", "--limit", "1", "-q"])
    rows = ((d.get("sections") or {}).get("paths") or {}).get("rows") or []
    if not rows:
        raise RuntimeError("no setup path reported at ss")
    w = rows[0]
    scope = r.get("scope") or {}
    return dict(wire_model=scope.get("wire_model"), clock_model=scope.get("clock_model"),
                unconstrained_endpoints=scope.get("unconstrained_endpoints"),
                startpoint=w.get("startpoint"), endpoint=w.get("endpoint"), arrival_ns=w.get("arrival_ns"),
                required_ns=w.get("required_ns"), slack_ns=w.get("slack_ns"))


def derive_design(name: str, work: Path, t: dict) -> tuple[dict, dict[str, str]]:
    m = json.loads((ROOT / "fixtures/designs" / f"{name}.json").read_text())
    ref = m["sdc"].get("reference_clock") or m["sdc"]["clock"]  # v2 manifests: the 10 ns clock SDC
    ref_text = D.resolve(ref["path"]).read_text()
    if sha256_text(ref_text) != ref["sha256"]:
        raise SystemExit(f"{name}: reference clock SDC differs from the manifest")
    ref_period = reference_period(ref_text)
    io = D.resolve(m["sdc"]["io_template"]["path"]).read_text()
    policy_path = D.resolve(m["cell_policy"]["path"])
    policy = json.loads(policy_path.read_text())
    work = work / name
    work.mkdir(parents=True, exist_ok=False)
    ports = ports_of(m, t, work)
    record = dict(schema=SCHEMA, design=name, rule=RULE, owner_decision="2026-10-24 clock period rule",
                  method=dict(netlist="gate mapped netlist (RustSys + cell policy, deconflict, port-alias buffers)",
                              constraints="rendered signoff SDC on the reference clock SDC, set_propagated_clock "
                                          "removed (ideal clock)",
                              engine="RustSTA, ss Liberty only, no SPEF, --require-wire-model per PDK",
                              wire_model=WIRE_MODEL,
                              critical_path_delay="reference period - worst ss setup slack",
                              rounding="ceil to 1 ns, floor 10 ns"),
                  inputs=dict(rtl=[dict(path=r["path"], sha256=sha256(D.resolve(r["path"]))) for r in m["rtl"]],
                              reference_clock_sdc=dict(path=ref["path"], sha256=ref["sha256"], period_ns=ref_period),
                              io_template=dict(path=m["sdc"]["io_template"]["path"],
                                               sha256=sha256(D.resolve(m["sdc"]["io_template"]["path"]))),
                              cell_policy=dict(path=m["cell_policy"]["path"], sha256=sha256(policy_path)),
                              tools={k: v["sha256"] for k, v in t.items()}),
                  pdks={})
    sdcs = {}
    for tag in m["pdks"]:
        w = work / tag
        w.mkdir()
        netlist = w / "mapped.v"
        facts = mapped_netlist(m, tag, t, policy, netlist)
        clock_ref = ref_text
        ideal = D.render_sdc(clock_ref, io, ports, m["clock"]["port"], m["clock"]["name"])
        ideal = "".join(x for x in ideal.splitlines(keepends=True) if "set_propagated_clock" not in x)
        (w / "reference_ideal.sdc").write_text(ideal)
        pre = sta(t, tag, netlist, w / "reference_ideal.sdc", w / "ws_ref")
        if pre["wire_model"] != WIRE_MODEL[tag] or pre["clock_model"] not in (None, "ideal"):
            raise RuntimeError(f"{name} {tag}: scope {pre['wire_model']}/{pre['clock_model']} is not the pre-layout scope")
        delay = round(ref_period - pre["slack_ns"], 6)
        scaled = round(FACTOR * delay, 6)
        period = D.clock_period_rule(delay)
        if period != max(FLOOR_NS, STEP_NS * math.ceil(scaled / STEP_NS - 1e-9)):
            raise RuntimeError("rule arithmetic disagrees with rtl2gds_design.clock_period_rule")
        clock_text = set_period(clock_ref, period)
        sdcs[tag] = clock_text
        check_sdc = D.render_sdc(clock_text, io, ports, m["clock"]["port"], m["clock"]["name"])
        check_sdc = "".join(x for x in check_sdc.splitlines(keepends=True) if "set_propagated_clock" not in x)
        (w / "derived_ideal.sdc").write_text(check_sdc)
        post = pre if period == ref_period else sta(t, tag, netlist, w / "derived_ideal.sdc", w / "ws_derived")
        expect = round(period - delay, 6)
        ok = abs(post["slack_ns"] - expect) <= 0.001
        record["pdks"][tag] = dict(
            liberty_ss=dict(name=liberty(tag, SS[tag]).name, sha256=sha256(liberty(tag, SS[tag]))),
            netlist=facts, prelayout=pre, critical_path_delay_ns=delay, scaled_ns=scaled, period_ns=period,
            binding="floor 10 ns" if period == FLOOR_NS and scaled <= FLOOR_NS else "1.25 x critical path",
            clock_sdc=dict(path=f"fixtures/designs/{name}/{tag}-cts.sdc", sha256=sha256_text(clock_text)),
            self_check=dict(status="passed" if ok else "failed", worst_slack_at_period_ns=post["slack_ns"],
                            expected_ns=expect, endpoint=post["endpoint"]))
        if not ok:
            raise RuntimeError(f"{name} {tag}: slack at {period} ns is {post['slack_ns']}, expected {expect}")
    return record, sdcs


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--work", type=Path, required=True, help="new scratch directory")
    ap.add_argument("--design", action="append", help="default: the required set and the stretch designs")
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true", help="write the records and per-PDK clock SDCs")
    mode.add_argument("--check", action="store_true", help="recompute and compare with the tracked files")
    args = ap.parse_args(argv)
    args.work.mkdir(parents=True, exist_ok=False)
    t = tools()
    bad = []
    for name in args.design or REQUIRED + STRETCH:
        record, sdcs = derive_design(name, args.work.resolve(), t)
        files = {ROOT / "fixtures/designs" / name / "clock-period.json": json.dumps(record, indent=1) + "\n"}
        files.update({ROOT / "fixtures/designs" / name / f"{tag}-cts.sdc": text for tag, text in sdcs.items()})
        for path, text in files.items():
            if args.write:
                path.write_text(text)
            elif not path.is_file() or path.read_text() != text:
                bad.append(str(path.relative_to(ROOT)))
        print(json.dumps({name: {tag: dict(delay=r["critical_path_delay_ns"], x125=r["scaled_ns"],
                                            period=r["period_ns"], wire=r["prelayout"]["wire_model"],
                                            path=f"{r['prelayout']['startpoint']} -> {r['prelayout']['endpoint']}")
                                 for tag, r in record["pdks"].items()}}), flush=True)
    if args.check:
        print(json.dumps(dict(check="identical" if not bad else "differs", differs=bad)))
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
