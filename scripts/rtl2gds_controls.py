# SPDX-License-Identifier: Apache-2.0
"""Negative controls of the RTL-to-GDS gate (scripts/rtl2gds.py).

Each control plants one defect into a copy of a final artifact (never into a
bound artifact) and runs the oracle that must catch it; the control is
``detected`` only when that oracle fails on its own result:

  drc_spacing       two metal rectangles at below-minimum spacing in the native
                    GDS -> Magic DRC AND KLayout DRC fail
  label_swap        two input-port labels exchanged -> Netgen LVS AND KLayout
                    LVS fail (own results) AND the XOR label comparison fails
  removed_via       one routing via SREF of a two-terminal signal net deleted
                    -> Netgen LVS AND KLayout LVS fail
  removed_hold_cell the hold-buffer chain of the worst iteration-0 hold
                    endpoint removed from the final netlist, fresh layout and
                    extraction -> RustSTA AND OpenSTA hold below the margin
  tampered_sdc      the rendered SDC edited -> frozen-input hash check fails
  tampered_spef     resistances of the native ss SPEF scaled x1000 -> SPEF
                    identity (extraction receipt) fails; re-timing reported
  bad_rtl           RTL mutant (an output bit inverted) -> RustSys check
                    synth AND Yosys equiv reject it against the final netlist
                    AND the random co-simulation reports mismatches
  missing_tool      the gate run with a pinned tool path that does not exist
                    -> exit 3, tools_identity unverified (never passed)
  tampered_artifact a bound GDS byte flipped, and separately a recorded hash
                    edited -> artifact binding re-verification fails
  antenna_planted   a long wire on an input pin -> native check-antenna and
                    the foundry deck both flag it (margin evidence recorded)
"""
from __future__ import annotations

import json
from pathlib import Path
import re
import shutil
import subprocess
import sys

import rtl2gds_binding as B
import rtl2gds_logic as L
import rtl2gds_timing as T
from rtl2gds_design import GateError, ROOT, item, sha256_file

sys.path.insert(0, str(ROOT / "scripts/signoff"))
import container  # noqa: E402
import controls as signoff_controls  # noqa: E402


def control(oracle: str, detected: bool, **evidence) -> dict:
    return dict(oracle=oracle, expected="failed", detected=bool(detected), **evidence)


def plant(tag: str, gds: Path, work: Path, kind: str, **extra) -> tuple[Path, dict]:
    lay = signoff_controls.LAYERS[tag]
    work.mkdir(parents=True, exist_ok=True)
    out, report = work / f"{kind}.gds", work / f"{kind}.plant.json"
    rd = dict(gds="/in/design.gds", out=f"/out/{out.name}", report=f"/out/{report.name}", kind=kind,
              metal=lay["metal"], pin_layer=lay["pin_layer"], pin_text=lay["pin_text"], draw_layer=lay["draw_layer"],
              boundary=lay["boundary"], spacing_um=lay["spacing_um"], pins=lay["short_pins"],
              antenna_pin=lay["antenna_pin"], swap=lay["swap_pins"], antenna_length_um=lay["antenna_length_um"])
    rd.update(extra)
    cmd = ["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_plant.py"]
    for k, v in rd.items():
        cmd += ["-rd", f"{k}={v}"]
    r = container.run(cmd, out_dir=work, label=container.next_label(work, f"plant_{kind}"),
                      ro=[(str(ROOT / "scripts/signoff"), "/scripts"), (str(Path(gds).resolve()), "/in/design.gds")],
                      memory="2g", cpus=1, threads=1, timeout=300)
    if not out.is_file() or not report.is_file():
        raise GateError(f"planting {kind} failed: {container.read_capture(r['stdout'])[-400:]}")
    return out, json.loads(report.read_text())


def verdict(summary: dict, check: str) -> dict:
    return json.loads(Path(summary["verdicts"][check]).read_text())


# ----------------------------------------------------------------------------- layout controls

def drc_spacing(ctx, gate, b, work: Path) -> dict:
    planted, info = plant(gate.tag, b["gds"], work, "spacing")
    s = gate.signoff(planted, Path(b["phys"]["paths"]["powered"]), None, f"{gate.tag}_ctl_spacing_signoff",
                     ["drc-magic", "drc-klayout"])
    st = s["statuses"]
    return control("Magic DRC and KLayout DRC", st.get("drc_magic") == "failed" and st.get("drc_klayout") == "failed",
                   statuses=st, counts=s["counts"], by_rule=s["by_rule"], planted=info.get("shapes"),
                   gap_um=info.get("gap_um"))


def input_bits(ctx) -> list[str]:
    return [bit for p in ctx.ports if p["direction"] == "input" and p["name"] != ctx.clock["port"] for bit in p["bits"]]


def label_swap(ctx, gate, b, work: Path) -> dict:
    bits = input_bits(ctx)
    if len(bits) < 2:
        raise GateError("label_swap needs two input port bits")
    swap = f"{bits[0]},{bits[1]}"
    planted, info = plant(gate.tag, b["gds"], work, "label_swap", swap=swap)
    s = gate.signoff(planted, Path(b["phys"]["paths"]["powered"]), gate.reference_gds,
                     f"{gate.tag}_ctl_label_swap_signoff", ["lvs-netgen", "lvs-klayout", "xor"])
    st = s["statuses"]
    ng, kl = verdict(s, "lvs-netgen"), verdict(s, "lvs-klayout")
    own = dict(netgen=signoff_controls.lvs_own(ng)[:4], klayout=signoff_controls.lvs_own(kl)[:4])
    detected = (st.get("lvs_netgen") == "failed" and st.get("lvs_klayout") == "failed" and st.get("xor") == "failed"
                and bool(own["netgen"]) and bool(own["klayout"]))
    return control("Netgen LVS, KLayout LVS (own results) and the XOR label comparison", detected, statuses=st,
                   swapped=swap, own_problems=own, labels_swapped=info.get("swapped"))


def two_terminal_via(def_text: str, vias: set[str], divisor: int, clock_nets: set[str]) -> tuple[str, str, int, int]:
    """First two-terminal signal net (DEF order) with a routing via: (net, via, x, y) in GDS units."""
    body = def_text.split("\nNETS ", 1)[1].split("\nEND NETS", 1)[0]
    for block in re.split(r"(?m)^\s*-\s+", body)[1:]:
        name = block.split(None, 1)[0]
        head = block.split("+", 1)[0]
        terminals = re.findall(r"\(\s*(\S+)\s+(\S+)\s*\)", head)
        if len(terminals) != 2 or name in clock_nets or "+ USE CLOCK" in block:
            continue
        for m in re.finditer(r"\(\s*(-?\d+)\s+(-?\d+)(?:\s+-?\d+)?\s*\)\s+(\w+)\s*(?:;)?\s*$", block, re.M):
            if m[3] in vias:
                x, y = int(m[1]), int(m[2])
                if x % divisor or y % divisor:
                    continue
                return name, m[3], x // divisor, y // divisor
    raise GateError("no two-terminal signal net with a routing via")


def removed_via(ctx, gate, b, work: Path) -> dict:
    exp = b["gds_export"]
    vias = {v[len("VIA_"):] for v in exp["via_structures"]}
    text = Path(b["phys"]["paths"]["def"]).read_text()
    clock = {n for n in re.findall(r"(?m)^\s*-\s+(\S+)", text.split("\nNETS ", 1)[1]) if n.startswith("rr_clock")}
    clock.add(ctx.clock["port"])
    net, via, x, y = two_terminal_via(text, vias, exp["coordinate_divisor"], clock)
    planted, info = plant(gate.tag, b["gds"], work, "missing_via", via_cell=f"VIA_{via}", via_x=x, via_y=y)
    s = gate.signoff(planted, Path(b["phys"]["paths"]["powered"]), None, f"{gate.tag}_ctl_removed_via_signoff",
                     ["lvs-netgen", "lvs-klayout"])
    st = s["statuses"]
    ng, kl = verdict(s, "lvs-netgen"), verdict(s, "lvs-klayout")
    own = dict(netgen=signoff_controls.lvs_own(ng)[:4], klayout=signoff_controls.lvs_own(kl)[:4])
    detected = st.get("lvs_netgen") == "failed" and st.get("lvs_klayout") == "failed" and own["netgen"] and own["klayout"]
    return control("Netgen LVS and KLayout LVS (own results)", detected, statuses=st, net=net, via=via, at_gds=[x, y],
                   removed=info.get("removed"), own_problems=own)


# ----------------------------------------------------------------------------- timing / identity controls

INSTANCE = re.compile(r"^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$")


def remove_chain(text: str, sink: str, prefix: str, buffers: dict[str, tuple[str, str]]) -> tuple[str, list[str]]:
    """Remove the ECO buffer chain feeding `inst/pin` and reconnect the original net.

    `buffers` maps each profiled delay-cell master to its (input, output) pins.
    """
    inst, pin = sink.rsplit("/", 1)
    lines = text.splitlines(keepends=True)
    insts = {}
    for i, line in enumerate(lines):
        m = INSTANCE.match(line)
        if m:
            insts[m[2]] = (i, m[1], dict(L.CONN.findall(m[3])))
    if inst not in insts or pin not in insts[inst][2]:
        raise GateError(f"sink {sink} not in the final netlist")
    net = insts[inst][2][pin].strip()
    removed, nets = [], []
    driver_of = {}
    for name, (_, master, conns) in insts.items():
        if name.startswith(prefix) and master in buffers and buffers[master][1] in conns:
            driver_of[conns[buffers[master][1]].strip()] = name
    while net in driver_of and len(removed) < 64:
        name = driver_of[net]
        master, conns = insts[name][1], insts[name][2]
        removed.append(name)
        nets.append(net)
        net = conns[buffers[master][0]].strip()
    if not removed:
        raise GateError(f"no {prefix} chain feeds {sink}")
    out = []
    for i, line in enumerate(lines):
        m = INSTANCE.match(line)
        if m and m[2] in removed:
            continue
        if any(re.fullmatch(r"\s*wire\s+" + re.escape(n) + r"\s*;\s*", line) for n in nets):
            continue
        if m and m[2] == inst:
            line = line.replace(f".{pin}({nets[0]})", f".{pin}({net})")
        out.append(line)
    return "".join(out), removed


def removed_hold_cell(ctx, gate, b, work: Path) -> dict:
    it0 = b["iterations"][0]["timing"]["endpoints"]
    worst = min(((v["hold"], c, e) for c, rows in it0.items() for e, v in rows.items() if "/" in e), default=None)
    if worst is None or not any(r.get("hold_eco") for r in b["iterations"]):
        raise GateError("no hold ECO was needed; the control cannot be planted")
    profile = json.loads(Path(b["flow"].delay_cells).read_text())
    buffers = {x["name"]: (x["input_pin"], x["output_pin"]) for x in profile["buffers"]}
    text, removed = remove_chain(Path(b["final_mapped"]).read_text(), worst[2], "rr_eco_hold_", buffers)
    work.mkdir(parents=True, exist_ok=True)
    netlist = work / "no_hold_cell_mapped.v"
    netlist.write_text(text)
    hc = ctx.make_harness(work, ctx.budgets)
    flow = gate.flow(hc)
    phys = flow.physical(0, netlist)
    sta = flow.sta(0, phys)
    margin = ctx.budgets["hold_margin_ns"]
    rust = {c: v["worst_hold_ns"] for c, v in sta["corners"].items()}
    corner = min(rust, key=rust.get)
    ref = T.opensta(hc, f"{gate.tag}_ctl_hold_opensta_{corner}", ctx.tool_paths["opensta"], flow.libs[corner],
                    Path(phys["paths"]["netlist"]), ctx.top, flow.sdc, Path(phys["spef"][corner]["path"]))
    ref_worst = min(ref["hold"].values())
    return control("RustSTA and OpenSTA hold gate on a fresh layout", rust[corner] < margin and ref_worst < margin,
                   removed=removed, sink=worst[2], iteration0_hold_ns=worst[0], worst_corner=corner,
                   ruststa_worst_hold_ns=rust, opensta_worst_hold_ns=ref_worst, margin_ns=margin)


def tampered_sdc(ctx, gate, b, work: Path) -> dict:
    work.mkdir(parents=True, exist_ok=True)
    tampered = work / "tampered_postroute.sdc"
    text = Path(b["flow"].sdc).read_text()
    new = re.sub(r"set_input_delay 0\.2", "set_input_delay 0.6", text, count=1)
    if new == text:
        new = text + "set_input_delay 0.6 -clock core [all_inputs]\n"
    tampered.write_text(new)
    fps = {k: dict(v) for k, v in b["frozen_before"].items()}
    fps["sdc_postroute"] = dict(path=str(tampered), sha256=sha256_file(tampered))
    from rtl2gds_design import compare_frozen
    res = compare_frozen(gate.spec["frozen"], fps)
    return control("frozen-input SHA-256 check", res["status"] == "failed" and res["changed"] == ["sdc_postroute"],
                   changed=res["changed"], expected_sha256=gate.spec["frozen"]["sdc_postroute"],
                   tampered_sha256=fps["sdc_postroute"]["sha256"])


def scale_spef_r(src: Path, dst: Path, factor: float) -> int:
    out, section, count = [], None, 0
    for line in src.read_text().splitlines(keepends=True):
        s = line.strip()
        if s.startswith("*"):
            section = s.split()[0]
        m = re.match(r"^(\d+\s+\S+\s+\S+\s+)([0-9.eE+-]+)(\s*)$", line)
        if section == "*RES" and m:
            line = f"{m[1]}{float(m[2]) * factor:.6g}{m[3]}"
            count += 1
        out.append(line)
    dst.write_text("".join(out))
    return count


def tampered_spef(ctx, gate, b, work: Path) -> dict:
    work.mkdir(parents=True, exist_ok=True)
    phys = b["phys"]
    src = Path(phys["spef"]["ss"]["path"])
    bad = work / "tampered_max.spef"
    resistors = scale_spef_r(src, bad, 1000.0)
    binding = T.spef_binding({"ss": bad}, {"ss": phys["spef"]["ss"]["sha256"]})
    spefs = {c: Path(phys["spef"][c]["path"]) for c in gate.corners}
    spefs["ss"] = bad
    hc = ctx.make_harness(work, ctx.budgets)
    try:
        rust = T.ruststa(hc, f"{gate.tag}_ctl_spef", ctx.tool_paths, b["flow"].libs, Path(phys["paths"]["netlist"]),
                         b["flow"].sdc, spefs)
        ss = rust["gating"]["endpoints"]["ss"]
        effect = dict(worst_setup_ns=T.worst_of(ss, "setup"), worst_hold_ns=T.worst_of(ss, "hold"),
                      bound_worst_setup_ns=b["sta"]["corners"]["ss"]["worst_setup_ns"],
                      bound_worst_hold_ns=b["sta"]["corners"]["ss"]["worst_hold_ns"])
    except Exception as error:  # noqa: BLE001 - recorded, the identity check is the oracle
        effect = dict(ruststa="did not time the tampered SPEF", error=repr(error)[:400])
    return control("SPEF identity against the extraction receipt", binding["status"] == "failed",
                   resistors_scaled=resistors, factor=1000.0, binding=binding["files"], timing_effect=effect)


def bad_rtl(ctx, gate, b, work: Path) -> dict:
    files, info = L.bad_rtl(ctx.rtl, ctx.top, ctx.ports, work)
    final = Path(b["phys"]["paths"]["netlist"])
    lib = b["flow"].libs["tt"]
    pair = L.cec_pair(ctx.h, f"{gate.tag}_ctl_bad_rtl", ctx.tool_paths, lib, files, ctx.top, final, ctx.verification)
    sim = L.cosim(ctx.h, f"{gate.tag}_ctl_bad_rtl", ctx.tool_paths, lib, files, ctx.top, final, ctx.ports,
                  ctx.clock["port"], int(ctx.verification["cosim_vectors"]), int(str(ctx.verification["cosim_seed"]), 0))
    return control("RustSys check synth, Yosys equiv and the random co-simulation",
                   pair["rustsys"]["status"] == "failed" and pair["yosys"]["status"] == "failed"
                   and sim["status"] == "failed", rustsys=pair["rustsys"], yosys=pair["yosys"],
                   cosim=dict(status=sim["status"], mismatches=(sim.get("mismatches") or [])[:3]), **info)


def missing_tool(ctx, gate, b, work: Path) -> dict:
    run = work / "run"
    missing = work / "absent" / "yosys"
    cmd = [sys.executable, str(ROOT / "scripts/rtl2gds.py"), "--design", str(ctx.args.design), "--pdk", gate.tag,
           "--run", str(run), "--base-rev", "empty", "--rustroad", str(ctx.args.rustroad), "--tools", str(ctx.args.tools),
           "--tool", f"yosys={missing}", "--no-controls", "--no-determinism"]
    work.mkdir(parents=True, exist_ok=True)
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=600, cwd=work)
    (work / "stdout.txt").write_text(p.stdout)
    (work / "stderr.txt").write_text(p.stderr)
    summary = json.loads((run / "summary.json").read_text()) if (run / "summary.json").is_file() else {}
    tools = (summary.get("global_statuses") or {}).get("tools_identity") or {}
    detected = (p.returncode == 3 and summary.get("status") == "unverified" and tools.get("status") == "unverified"
                and "yosys" in (tools.get("reason") or ""))
    return control("tool identity preflight (missing -> unverified, exit 3)", detected, exit_code=p.returncode,
                   status=summary.get("status"), tools_identity=tools, pdk_items_run=len(summary.get("pdks", [])))


def tampered_artifact(ctx, gate, b, record: dict, work: Path) -> dict:
    work.mkdir(parents=True, exist_ok=True)
    copy = work / "native.gds"
    shutil.copyfile(b["gds"], copy)
    data = bytearray(copy.read_bytes())
    data[len(data) // 2] ^= 0x01
    copy.write_bytes(bytes(data))
    rec = json.loads(json.dumps(record))
    rec["artifacts"]["gds"]["path"] = str(copy)
    revision = record["revision"]
    flipped = B.verify(rec, revision)
    rec2 = json.loads(json.dumps(record))
    rec2["artifacts"]["def"]["sha256"] = "0" * 64
    edited = B.verify(rec2, revision)
    untouched = B.verify(record, revision)
    detected = (flipped["status"] == "failed" and [c["artifact"] for c in flipped["changed"]] == ["gds"]
                and edited["status"] == "failed" and untouched["status"] == "passed")
    return control("artifact binding re-verification", detected, flipped_byte=dict(status=flipped["status"],
                   reason=flipped.get("reason")), edited_hash=dict(status=edited["status"], reason=edited.get("reason")),
                   untouched_record=untouched["status"])


def antenna_planted(ctx, gate, b, work: Path) -> dict:
    flow, phys = b["flow"], b["phys"]
    bits = input_bits(ctx)
    shape = flow.plant_shape(phys, port=bits[-1])
    extra = work / "extra.json"
    work.mkdir(parents=True, exist_ok=True)
    extra.write_text(json.dumps([{k: shape[k] for k in ("net", "layer", "rect")}]))
    native = flow.check_antenna(f"{gate.tag}_ctl_antenna", phys["workspace"], extra)
    planted = flow.plant_gds(f"{gate.tag}_ctl_antenna", b["gds"], b["gds_export"], shape)
    deck = flow.deck_antenna(f"{gate.tag}_ctl_antenna", planted, phys)
    report = json.loads(Path(native["report"]).read_text())
    rows = [c for c in report["checks"] if c["net"] == shape["net"] and c["violated"]]
    v = json.loads(Path(json.loads(Path(deck["summary"]).read_text())["verdicts"]["antenna"]).read_text())
    res = v.get("result") or {}
    magic = (res.get("primary") if gate.tag == "sky130" else res.get("secondary")) or {}
    margin = report["safety_margin"]["fraction"]
    native_max = max((c["ratio"] for c in rows), default=None)
    samples = [s["ratio"] for s in magic.get("samples") or []]
    check = None
    if native_max is not None and samples:
        check = dict(native_ratio=native_max, native_ratio_over_keep=native_max / (1 - margin),
                     magic_max_ratio=max(samples), conservative=native_max / (1 - margin) >= max(samples))
    return control("native check-antenna and the foundry antenna deck",
                   native["status"] == "violated" and bool(rows) and deck["status"] == "failed",
                   port=shape["net"], layer=shape["layer"], length_um=shape["length_um"],
                   native=dict(status=native["status"], violated=native["violated"][:3]),
                   deck=dict(status=deck["status"], count=deck["count"], by_rule=deck["by_rule"]),
                   margin_check=check)


def run_all(ctx, gate, b, record) -> dict:
    root = ctx.h.root / f"{gate.tag}_controls"
    root.mkdir(exist_ok=True)
    plan = [("drc_spacing", drc_spacing), ("label_swap", label_swap), ("removed_via", removed_via),
            ("removed_hold_cell", removed_hold_cell), ("tampered_sdc", tampered_sdc), ("tampered_spef", tampered_spef),
            ("bad_rtl", bad_rtl), ("missing_tool", missing_tool), ("antenna_planted", antenna_planted)]
    results = {}
    for name, fn in plan:
        try:
            results[name] = fn(ctx, gate, b, root / name)
        except Exception as error:  # noqa: BLE001 - a control that could not run proves nothing
            results[name] = dict(detected=None, error=repr(error)[:600])
    try:
        results["tampered_artifact"] = tampered_artifact(ctx, gate, b, record, root / "tampered_artifact")
    except Exception as error:  # noqa: BLE001
        results["tampered_artifact"] = dict(detected=None, error=repr(error)[:600])
    missed = [n for n, r in results.items() if r.get("detected") is False]
    crashed = [n for n, r in results.items() if r.get("detected") is None]
    status = "failed" if missed else "unverified" if crashed else "passed"
    return item(status, None if status == "passed" else f"not detected: {missed}; did not run: {crashed}",
                controls=results)
