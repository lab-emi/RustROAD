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
                    endpoint removed from the final netlist (a chain in front
                    of the endpoint, else one on a recorded path-aware point
                    of its path, else every hold cell), fresh layout and
                    extraction -> RustSTA AND OpenSTA hold below the margin;
                    when the closure loop's DRV buffers still keep hold met
                    without it, every hold cell and ECO buffer is removed
                    (recorded as `escalated`). If both engines still prove hold
                    met after removal, this is a non-faulty mutation; retain its
                    measured result and require planted_hold_uncertainty instead.
  planted_hold_uncertainty
                    when no hold cell was inserted, or removal is proved to
                    remain hold-clean in both engines: a copy of the SDC with
                    ``set_clock_uncertainty -hold 2.0`` re-times the bound final
                    layout (native SPEF) -> RustSTA AND OpenSTA hold gate fail
  tampered_sdc      the rendered SDC edited -> frozen-input hash check fails
  tampered_spef     resistances of the native ss SPEF scaled x1000 -> SPEF
                    identity (extraction receipt) fails; re-timing reported
  inverted_output   the final netlist with bit 0 of the first output port
                    driven through an inserted PDK inverter (every other
                    reader keeps the original value; instance and register
                    names unchanged) -> RustSys check synth AND the second
                    oracle (Yosys equiv, or the exhaustive co-simulation)
                    reject it against the RTL AND the random co-simulation
                    reports mismatches. (An RTL wrapper mutant moves every
                    register under a hierarchy prefix, so name-based state
                    matching cannot even start: not a usable control.)
  missing_tool      the gate run with a pinned tool path that does not exist
                    -> exit 3, tools_identity unverified (never passed)
  relaxed_manifest  an untracked copy of the design manifest with the parity
                    tolerance relaxed to 3 ps -> exit 1, budget_floors failed
                    (owner floor) AND manifest_provenance unverified
  tampered_artifact a bound GDS byte flipped, and separately a recorded hash
                    edited -> artifact binding re-verification fails
  antenna_planted   a long wire on an input port pin without an antenna
                    diode (fewest sinks first; the first port/length, up to
                    64 ports x 3 lengths, the native check flags: a pin whose
                    route leaves upward has no gate on the I/O layer) ->
                    native check-antenna and the foundry deck both flag it
                    (margin evidence recorded). A plant the conservative
                    native check flags but the deck passes is recorded in
                    `deck_plants` and the next candidate is planted (at
                    most MAX_DECK_PLANTS deck runs)
"""
from __future__ import annotations

from verify_logic import gnu_time_program

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
    info = json.loads(report.read_text())
    info["plant_peak_rss_kib"] = r.get("peak_rss_kib")
    return out, info


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
                   gap_um=info.get("gap_um"), peak_rss_kib=info.get("plant_peak_rss_kib"))


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
                   swapped=swap, own_problems=own, labels_swapped=info.get("swapped"),
                   peak_rss_kib=info.get("plant_peak_rss_kib"))


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
                   removed=info.get("removed"), own_problems=own, peak_rss_kib=info.get("plant_peak_rss_kib"))


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
        # Every pin the chain drove (one sink, or every sink of a net-level
        # path-aware point) goes back to the original net.
        if m:
            line = line.replace(f"({nets[0]})", f"({net})")
        out.append(line)
    return "".join(out), removed


def remove_all_chains(text: str, prefix: str | tuple, buffers: dict[str, tuple[str, str]]) -> tuple[str, list[str]]:
    """Remove every `prefix` buffer of the netlist and reconnect each pin it
    drove (directly or through further removed buffers) to the net in front
    of the chain. Used when no single chain feeds an iteration-0 endpoint
    (path-aware hold chains whose output a DRV buffer re-drives)."""
    lines = text.splitlines(keepends=True)
    hops = {}
    removed = []
    for line in lines:
        m = INSTANCE.match(line)
        if m and m[2].startswith(prefix) and m[1] in buffers:
            conns = dict(L.CONN.findall(m[3]))
            i, o = buffers[m[1]]
            if i in conns and o in conns:
                hops[conns[o].strip()] = conns[i].strip()
                removed.append(m[2])
    if not removed:
        raise GateError(f"no {prefix} cell in the netlist")

    def root(net):
        for _ in range(len(hops) + 1):
            if net not in hops:
                return net
            net = hops[net]
        raise GateError("cyclic ECO buffer chain")
    out = []
    for line in lines:
        m = INSTANCE.match(line)
        if m and m[2] in removed:
            continue
        if any(re.fullmatch(r"\s*wire\s+" + re.escape(n) + r"\s*;\s*", line) for n in hops):
            continue
        if m or line.lstrip().startswith("assign"):
            for n in hops:
                line = line.replace(f"({n})", f"({root(n)})").replace(f"= {n};", f"= {root(n)};")
        out.append(line)
    return "".join(out), removed


def removed_hold_cell(ctx, gate, b, work: Path) -> dict:
    it0 = b["iterations"][0]["timing"]["endpoints"]
    profile = json.loads(Path(b["flow"].delay_cells).read_text())
    buffers = {x["name"]: (x["input_pin"], x["output_pin"]) for x in profile["buffers"]}
    source = Path(b["final_mapped"]).read_text()
    # The worst iteration-0 hold endpoint whose sink a hold chain feeds now.
    worst, text, removed = None, None, None
    for hold, corner, ep in sorted((v["hold"], c, e) for c, rows in it0.items() for e, v in rows.items() if "/" in e):
        try:
            text, removed = remove_chain(source, ep, "rr_eco_hold_", buffers)
        except GateError:
            continue
        worst = (hold, corner, ep)
        break
    if worst is None:
        # Path-aware hold ECO: the chain sits on the endpoint's worst hold
        # path (a sink pin or a whole net upstream), recorded with the points.
        points = [pt for rec in b["iterations"] for pt in ((rec.get("hold_eco") or {}).get("points") or [])
                  if isinstance(rec.get("hold_eco"), dict)]
        for hold, corner, ep in sorted((v["hold"], c, e) for c, rows in it0.items() for e, v in rows.items() if "/" in e):
            for pt in points:
                if ep not in pt.get("endpoints", []):
                    continue
                for sink in pt.get("sinks", []):
                    try:
                        text, removed = remove_chain(source, sink, "rr_eco_hold_", buffers)
                    except GateError:
                        continue
                    worst = (hold, corner, ep)
                    break
                if worst:
                    break
            if worst:
                break
    if worst is None:
        # No chain can be traced from an endpoint (e.g. a DRV buffer now
        # drives the chain's output): remove every hold cell instead.
        text, removed = remove_all_chains(source, "rr_eco_hold_", buffers)
        worst = min((v["hold"], c, e) for c, rows in it0.items() for e, v in rows.items() if "/" in e)
    work.mkdir(parents=True, exist_ok=True)
    margin = ctx.budgets["hold_margin_ns"]

    def plant(label: str, text: str) -> dict:
        sub = work / label
        sub.mkdir(parents=True, exist_ok=True)
        netlist = sub / "no_hold_cell_mapped.v"
        netlist.write_text(text)
        hc = ctx.make_harness(sub, ctx.budgets)
        flow = gate.flow(hc)
        phys = flow.physical(0, netlist)
        sta = flow.sta(0, phys)
        rust = {c: v["worst_hold_ns"] for c, v in sta["corners"].items()}
        corner = min(rust, key=rust.get)
        refs = {c:T.opensta(hc, f"{gate.tag}_ctl_hold_opensta_{c}", ctx.tool_paths["opensta"], flow.libs[c],
                           Path(phys["paths"]["netlist"]), ctx.top, flow.sdc, Path(phys["spef"][c]["path"]))
                for c in gate.corners}
        if any(v["unannotated"] or v["partially_unannotated"] for v in refs.values()):
            raise GateError("hold-removal control has incomplete parasitic annotation")
        ref_hold = {c:min(v["hold"].values()) for c,v in refs.items()}
        return dict(rust=rust, corner=corner, ref_worst=min(ref_hold.values()), ref_hold=ref_hold,
                    processes=len(hc.receipts),
                    peak=max((r["peak_rss_kib"] for r in hc.receipts if r.get("peak_rss_kib")), default=None))
    first = plant("chain", text)
    escalated = None
    if not (first["rust"][first["corner"]] < margin and first["ref_worst"] < margin):
        # The closure loop's DRV buffers also delay the hold paths (a buffer
        # behind a hold chain keeps hold met without it): remove every delay
        # insertion of the loop -- hold cells and ECO buffers; resizes stay.
        both = dict(buffers)
        both.update(eco_buffer_pins(b))
        text2, removed2 = remove_all_chains(source, ("rr_eco_hold_", "rr_eco_buf_"), both)
        escalated = dict(removed=removed2, first=dict(ruststa_worst_hold_ns=first["rust"],
                                                      opensta_worst_hold_ns=first["ref_worst"], removed=removed))
        first = plant("all_insertions", text2)
        removed = removed2
    rust, corner, ref_worst = first["rust"], first["corner"], first["ref_worst"]
    return control("RustSTA and OpenSTA hold gate on a fresh layout", rust[corner] < margin and ref_worst < margin,
                   removed=removed, sink=worst[2], iteration0_hold_ns=worst[0], worst_corner=corner,
                   ruststa_worst_hold_ns=rust, opensta_worst_hold_ns=ref_worst, margin_ns=margin,
                   opensta_worst_hold_by_corner_ns=first["ref_hold"],
                   mutation_is_fault=not (all(v >= margin for v in rust.values())
                                          and all(v >= margin for v in first["ref_hold"].values())),
                   escalated=escalated, processes=first["processes"], peak_rss_kib=first["peak"])


def eco_buffer_pins(b) -> dict:
    """(input, output) pins of every identity-function sizing-profile master
    (the masters the closure loop's buffer insertion uses)."""
    sizing = b["flow"].design.sizing_profile
    if sizing is None:
        return {}
    out = {}
    for f in json.loads(Path(sizing).read_text())["families"]:
        ins = [p for p, d in f["pins"].items() if d == "input"]
        outs = [p for p, d in f["pins"].items() if d == "output"]
        if len(ins) == 1 and len(outs) == 1 and len(f["pins"]) == 2:
            for c in f["cells"]:
                out[c["name"]] = (ins[0], outs[0])
    return out


def hold_cells_inserted(b) -> bool:
    """Hold cells are in the final netlist (a failed or empty hold ECO counts not)."""
    return bool(re.search(r"(?m)^\s*\S+\s+\\?rr_eco_hold_\d+", Path(b["final_mapped"]).read_text()))


def planted_hold_uncertainty(ctx, gate, b, work: Path) -> dict:
    """No hold cell to remove: plant a hold violation in the constraints of a
    copy instead and require both engines' hold gate to reject the bound layout."""
    work.mkdir(parents=True, exist_ok=True)
    sdc = work / "planted_hold.sdc"
    sdc.write_text(Path(b["flow"].sdc).read_text() + f"set_clock_uncertainty -hold 2.0 [get_clocks {ctx.clock['name']}]\n")
    phys, flow = b["phys"], b["flow"]
    spefs = {c: Path(phys["spef"][c]["path"]) for c in gate.corners}
    netlist = Path(phys["paths"]["netlist"])
    hc = ctx.make_harness(work, ctx.budgets)
    rust = T.ruststa(hc, f"{gate.tag}_ctl_hold_unc", ctx.tool_paths, flow.libs, netlist, sdc, spefs,
                     clock_model=flow.clock_model(), rc_limits=flow.sta_rc_limits(),
                         state_gib=flow.sta_state_gib(), file_limit_bytes=flow.sta_file_limit())
    ref = {c: T.opensta(hc, f"{gate.tag}_ctl_hold_unc_opensta_{c}", ctx.tool_paths["opensta"], flow.libs[c], netlist,
                        ctx.top, sdc, spefs[c]) for c in gate.corners}
    res = T.evaluate_source(rust, ref, gate.corners, ctx.budgets, clocked=ctx.clocked)["timing_hold"]
    engines = {v["engine"] for v in res.get("violations") or []}
    return control("RustSTA and OpenSTA hold gate on the bound layout with a planted 2 ns hold uncertainty",
                   res["status"] == "failed" and engines == {"ruststa", "opensta"}, planted="set_clock_uncertainty -hold 2.0",
                   violations=(res.get("violations") or [])[:6], margin_ns=ctx.budgets["hold_margin_ns"],
                   processes=len(hc.receipts), peak_rss_kib=max((r["peak_rss_kib"] for r in hc.receipts
                                                                if r.get("peak_rss_kib")), default=None))


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
                         b["flow"].sdc, spefs, clock_model=b["flow"].clock_model(), rc_limits=b["flow"].sta_rc_limits(),
                         state_gib=b["flow"].sta_state_gib(),
                         file_limit_bytes=b["flow"].sta_file_limit())
        ss = rust["gating"]["endpoints"]["ss"]
        effect = dict(worst_setup_ns=T.worst_of(ss, "setup"), worst_hold_ns=T.worst_of(ss, "hold"),
                      bound_worst_setup_ns=b["sta"]["corners"]["ss"]["worst_setup_ns"],
                      bound_worst_hold_ns=b["sta"]["corners"]["ss"]["worst_hold_ns"])
    except Exception as error:  # noqa: BLE001 - recorded, the identity check is the oracle
        effect = dict(ruststa="did not time the tampered SPEF", error=repr(error)[:400])
    return control("SPEF identity against the extraction receipt", binding["status"] == "failed",
                   resistors_scaled=resistors, factor=1000.0, binding=binding["files"], timing_effect=effect,
                   peak_rss_kib=max((r["peak_rss_kib"] for r in hc.receipts if r.get("peak_rss_kib")), default=None))


def invert_output(text: str, bit: str, cell: str, pins: tuple[str, str]) -> str:
    """Drive output port bit `bit` through an inverter; all other uses of the
    bit (its driver and internal readers) move to a new wire."""
    esc = re.escape(bit)
    new = "rr_mut_n"
    if re.search(r"\b" + re.escape(new) + r"\b", text):
        raise GateError("netlist already contains rr_mut_n")
    count = 0

    def conn(m):
        nonlocal count
        count += 1
        return f".{m[1]}({new})"
    out = re.sub(r"\.(\w+)\(\s*" + esc + r"\s*\)", conn, text)
    out, n1 = re.subn(r"(?m)^(\s*assign\s+)" + esc + r"(\s*=)", r"\g<1>" + new + r"\2", out)
    out, n2 = re.subn(r"(?m)^(\s*assign\s+\S+\s*=\s*)" + esc + r"(\s*;)", r"\g<1>" + new + r"\2", out)
    if count + n1 + n2 == 0:
        raise GateError(f"output bit {bit} is neither connected nor assigned in the netlist")
    out, n = re.subn(r"(?m)^endmodule\s*$", f"  wire {new};\n  {cell} rr_mut ( .{pins[0]}({new}), .{pins[1]}({bit}) );\n"
                     "endmodule", out, count=1)
    if n != 1:
        raise GateError("no endmodule in the netlist")
    return out


def inverted_output(ctx, gate, b, work: Path) -> dict:
    work.mkdir(parents=True, exist_ok=True)
    outputs = [p for p in ctx.ports if p["direction"] == "output"]
    if not outputs:
        raise GateError("the top has no output")
    bit = outputs[0]["bits"][0]
    profile = json.loads(Path(b["flow"].delay_cells).read_text())
    cell = f"{profile['library']}__inv_1"
    pins = ("A", "Y") if gate.tag == "sky130" else ("I", "ZN")
    final = Path(b["phys"]["paths"]["netlist"])
    mutant = work / "inverted_output.v"
    mutant.write_text(invert_output(final.read_text(), bit, cell, pins))
    lib = b["flow"].libs["tt"]
    pair = L.cec_pair(ctx.h, f"{gate.tag}_ctl_inverted_output", ctx.tool_paths, lib, ctx.rtl, ctx.top, mutant,
                      ctx.verification, exhaustive=ctx.exhaustive)
    sim = L.cosim(ctx.h, f"{gate.tag}_ctl_inverted_output", ctx.tool_paths, lib, ctx.rtl, ctx.top, mutant, ctx.ports,
                  ctx.clock["port"], int(ctx.verification["cosim_vectors"]), int(str(ctx.verification["cosim_seed"]), 0))
    second = L.second_oracle(pair)
    return control(f"RustSys check synth, {second['oracle']} and the random co-simulation",
                   pair["rustsys"]["status"] == "failed" and second["status"] == "failed"
                   and sim["status"] == "failed", rustsys=pair["rustsys"], yosys=pair["yosys"],
                   exhaustive=pair.get("exhaustive"),
                   cosim=dict(status=sim["status"], mismatches=(sim.get("mismatches") or [])[:3]),
                   mutation=f"output bit {bit} driven through {cell} rr_mut")


def sub_gate(ctx, gate, work: Path, design: Path, *extra: str) -> tuple[int, dict, int | None]:
    """Run the gate itself (no controls, no determinism) into work/run."""
    run = work / "run"
    cmd = [sys.executable, str(ROOT / "scripts/rtl2gds.py"), "--design", str(design), "--pdk", gate.tag,
           "--run", str(run), "--base-rev", "empty", "--rustroad", str(ctx.args.rustroad), "--tools", str(ctx.args.tools),
           "--no-controls", "--no-determinism", *extra]
    work.mkdir(parents=True, exist_ok=True)
    timing = work / "time.txt"
    p = subprocess.run([gnu_time_program(), "-v", "-o", str(timing), *cmd], capture_output=True, text=True,
                       timeout=600, cwd=work)
    (work / "stdout.txt").write_text(p.stdout)
    (work / "stderr.txt").write_text(p.stderr)
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", timing.read_text()) if timing.is_file() else None
    summary = json.loads((run / "summary.json").read_text()) if (run / "summary.json").is_file() else {}
    return p.returncode, summary, int(m[1]) if m else None


def missing_tool(ctx, gate, b, work: Path) -> dict:
    missing = work / "absent" / "yosys"
    code, summary, rss = sub_gate(ctx, gate, work, ctx.args.design, "--tool", f"yosys={missing}")
    tools = (summary.get("global_statuses") or {}).get("tools_identity") or {}
    detected = (code == 3 and summary.get("status") == "unverified" and tools.get("status") == "unverified"
                and "yosys" in (tools.get("reason") or ""))
    return control("tool identity preflight (missing -> unverified, exit 3)", detected, exit_code=code,
                   status=summary.get("status"), tools_identity=tools, pdk_items_run=len(summary.get("pdks", [])),
                   peak_rss_kib=rss)


def relaxed_manifest(ctx, gate, b, work: Path) -> dict:
    """Round-1 verifier F2: a relaxed, untracked manifest copy must not pass."""
    work.mkdir(parents=True, exist_ok=True)
    m = json.loads(Path(ctx.args.design).read_text())
    m["budgets"]["parity_ns"] = 0.003
    copy = work / "relaxed-design.json"
    copy.write_text(json.dumps(m, indent=1) + "\n")
    code, summary, rss = sub_gate(ctx, gate, work, copy, "--preflight-only")
    g = summary.get("global_statuses") or {}
    floors, prov = g.get("budget_floors") or {}, g.get("manifest_provenance") or {}
    detected = (code == 1 and summary.get("status") == "failed" and floors.get("status") == "failed"
                and "budgets.parity_ns" in (floors.get("reason") or "") and prov.get("status") == "unverified")
    return control("budget floors (owner thresholds) and manifest provenance (git-tracked at HEAD)", detected,
                   exit_code=code, status=summary.get("status"), failed_items=summary.get("failed_items"),
                   budget_floors=dict(status=floors.get("status"), reason=floors.get("reason")),
                   manifest_provenance=dict(status=prov.get("status"), reason=prov.get("reason")),
                   pdk_items_run=len(summary.get("pdks", [])), peak_rss_kib=rss)


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


def antenna_candidates(netlist_text: str, bits: list[str]) -> list[str]:
    """Input port bits ordered by sink count (fewest first), without the bits
    that carry an antenna diode (their deck verdict would be the diode's)."""
    loads, diode = {b: 0 for b in bits}, set()
    for line in netlist_text.splitlines():
        m = INSTANCE.match(line)
        if not m:
            continue
        for _pin, net in L.CONN.findall(m[3]):
            net = net.strip()
            if net in loads:
                loads[net] += 1
                if m[2].lstrip("\\").startswith("rr_eco_ant"):
                    diode.add(net)
    return sorted((b for b in bits if b not in diode and loads[b] > 0), key=lambda b: (loads[b], bits.index(b)))


# Deck runs of the antenna control (each a full foundry antenna check).
MAX_DECK_PLANTS = 3


def antenna_planted(ctx, gate, b, work: Path) -> dict:
    flow, phys = b["flow"], b["phys"]
    work.mkdir(parents=True, exist_ok=True)
    candidates = antenna_candidates(Path(phys["paths"]["netlist"]).read_text(), input_bits(ctx))
    if not candidates:
        raise GateError("no input port bit without an antenna diode")
    # The strip must join a partial net that reaches a gate on the I/O layer
    # (a route that leaves the pin upward has no gate at that layer's etch):
    # the first candidate/length the native check flags is planted.
    base = flow.plant_shape(phys, port=candidates[0])["length_um"]
    k, native, shape, hit = 0, None, None, False
    tried = []
    deck = None
    for port in candidates[:64]:
        for length in (base, 2 * base, 4 * base):
            shape = flow.plant_shape(phys, port=port, length_um=length)
            extra = work / f"extra{k}.json"
            extra.write_text(json.dumps([{kk: shape[kk] for kk in ("net", "layer", "rect")}]))
            native = flow.check_antenna(f"{gate.tag}_ctl_antenna{k}", phys["workspace"], extra)
            k += 1
            # Only a violation on the planted net counts (another net may
            # already violate; that is antenna_native's verdict).
            hit = any(v["net"] == shape["net"] for v in native["violated"])
            if hit:
                break
        if not hit:
            continue
        planted = flow.plant_gds(f"{gate.tag}_ctl_antenna{len(tried) or ''}", b["gds"], b["gds_export"], shape)
        deck = flow.deck_antenna(f"{gate.tag}_ctl_antenna{len(tried) or ''}", planted, phys)
        tried.append(dict(port=shape["net"], length_um=shape["length_um"], deck=deck["status"]))
        # The native model is conservative (safety margin, LEF gate areas):
        # a plant it flags but the deck passes is recorded and the next
        # candidate is tried, at most MAX_DECK_PLANTS deck runs.
        if deck["status"] == "failed" or len(tried) >= MAX_DECK_PLANTS:
            break
        hit = False
    if deck is None:
        raise GateError(f"no input pin whose I/O-layer partial net reaches a gate ({k} plants tried): every route "
                        "leaves its pin upward, so the planted strip has no gate at that layer's etch")
    report = json.loads(Path(native["report"]).read_text())
    rows = [c for c in report["checks"] if c["net"] == shape["net"] and c["violated"]]
    deck_summary = json.loads(Path(deck["summary"]).read_text())
    v = json.loads(Path(deck_summary["verdicts"]["antenna"]).read_text())
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
                   candidates=candidates[:4], deck_plants=tried,
                   native=dict(status=native["status"], violated=native["violated"][:3]),
                   deck=dict(status=deck["status"], count=deck["count"], by_rule=deck["by_rule"]),
                   margin_check=check, peak_rss_kib=deck_summary["resources"]["peak_rss_kib"])


def run_all(ctx, gate, b, record) -> dict:
    root = ctx.h.root / f"{gate.tag}_controls"
    root.mkdir(exist_ok=True)
    hold_control = ("removed_hold_cell", removed_hold_cell) if hold_cells_inserted(b) else \
        ("planted_hold_uncertainty", planted_hold_uncertainty)
    plan = [("drc_spacing", drc_spacing), ("label_swap", label_swap), ("removed_via", removed_via),
            hold_control, ("tampered_sdc", tampered_sdc), ("tampered_spef", tampered_spef),
            ("inverted_output", inverted_output), ("missing_tool", missing_tool), ("relaxed_manifest", relaxed_manifest),
            ("antenna_planted", antenna_planted)]
    results = {}
    if not hold_cells_inserted(b):
        results["removed_hold_cell"] = dict(detected="not_applicable", oracle="RustSTA and OpenSTA hold gate",
                                            reason="no hold cell was inserted (closed at iteration "
                                                   f"{b['final_iteration']} without a hold ECO); replaced by "
                                                   "planted_hold_uncertainty")
    for name, fn in plan:
        try:
            results[name] = fn(ctx, gate, b, root / name)
        except Exception as error:  # noqa: BLE001 - a control that could not run proves nothing
            results[name] = dict(detected=None, error=repr(error)[:600])
    removal = results.get("removed_hold_cell", {})
    if removal.get("detected") is False and removal.get("mutation_is_fault") is False:
        # A timing oracle must not reject a layout both engines prove meets hold.
        # Keep the non-fault experiment visible, and require a genuine planted
        # violation. A failed or incomplete removal experiment never gets this path.
        results["removed_hold_cell"] = dict(
            detected="not_applicable", experiment=removal,
            reason="removal still meets hold in both engines at every corner; "
                   "a separate planted_hold_uncertainty must be detected")
        try:
            results["planted_hold_uncertainty"] = planted_hold_uncertainty(
                ctx, gate, b, root/"planted_hold_uncertainty")
        except Exception as error:  # noqa: BLE001
            results["planted_hold_uncertainty"] = dict(detected=None, error=repr(error)[:600])
    try:
        results["tampered_artifact"] = tampered_artifact(ctx, gate, b, record, root / "tampered_artifact")
    except Exception as error:  # noqa: BLE001
        results["tampered_artifact"] = dict(detected=None, error=repr(error)[:600])
    missed = [n for n, r in results.items() if r.get("detected") is False]
    crashed = [n for n, r in results.items() if r.get("detected") is None]
    if results.get("removed_hold_cell", {}).get("detected") == "not_applicable" and \
            results.get("planted_hold_uncertainty", {}).get("detected") is not True:
        crashed.append("removed_hold_cell (its replacement was not detected)")
    status = "failed" if missed else "unverified" if crashed else "passed"
    return item(status, None if status == "passed" else f"not detected: {missed}; did not run: {crashed}",
                controls=results)
