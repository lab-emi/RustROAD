# SPDX-License-Identifier: Apache-2.0
"""Final multi-corner timing of the RTL-to-GDS gate (RustSTA 0.4 + OpenSTA 3.1.0).

For each SPEF source (native ``rustroad extract`` and OpenRCX on the exported
DEF), per PDK:

* RustSTA: one workspace with the three analysis corners (tt+nom, ss+max,
  ff+min) and one SPEF per corner, ``run --require-wire-model spef_rc`` with
  the default pulse rules (``worst``/``all``: the gating MPW/min-period
  verdict), then a second run with the OpenSTA-compatible rules (``--pulse-groups
  last --pulse-pins clock-ends``) used only for parity. Reports: endpoints per
  corner, pulse_width, min_period, drv and limits.
* OpenSTA (black box) per corner on the same Liberty, netlist, SDC and SPEF:
  parasitic annotation, endpoint setup/hold slacks, min pulse width, minimum
  period, max slew / capacitance / fanout.

Statuses (each separately, both engines and both SPEF sources):
  timing_setup   setup slack >= setup margin at every gated setup corner (tt, ss)
  timing_hold    hold slack >= hold margin at tt, ss and ff
  mpw_min_period RustSTA default rules met with checked rows and no unchecked
                 pins except the asynchronous set/clear inputs of sequential
                 cells (their pulse comes from the reset source outside the
                 block: no clock reaches them, neither engine checks them; they
                 are matched pin for pin and listed as out of block scope),
                 OpenSTA met; minimum-period rows may be absent only when
                 BOTH engines report none (no Liberty minimum_period on the cells).
                 ``not_applicable`` only for a combinational design (virtual
                 I/O clock): RustSTA pulse_width_status unconstrained with zero
                 rows and both engines without any pulse-width/period row.
  drv            RustSTA drv met with every kind checked, OpenSTA no violator
  timing_accuracy RustSTA vs OpenSTA: endpoint sets equal and every endpoint
                 slack within the parity tolerance per corner per SPEF, MPW /
                 period rows (compatible rules) within tolerance, equal DRV
                 violator sets. Reported separately; it gates like any item.
Every SPEF is checked against its producer's SHA-256 before it is imported.
"""
from __future__ import annotations

import json
import math
from pathlib import Path
import re

from rtl2gds_design import GateError, item, sha256_file, worst

T0133 = re.compile(r"corner '(\w+)': (\d+) pin\(s\) with min_pulse_width or minimum_period checks are reached by no clock")
ROW = re.compile(r"^(\S+)(?:\s+\((high|low|[^)]*)\))?\s+(-?[0-9.]+)\s+(-?[0-9.]+)\s+(-?[0-9.]*)\s*\((MET|VIOLATED)\)")
SETUP_CORNERS = ("tt", "ss")
HOLD_CORNERS = ("tt", "ss", "ff")
DRV_KINDS = ("max_transition", "max_capacitance", "max_fanout")
COMPAT = ["--pulse-groups", "last", "--pulse-pins", "clock-ends"]


def run_json(h, label: str, argv: list, file_limit_bytes: int | None = None) -> dict:
    r = h.run(label, [str(a) for a in argv]) if file_limit_bytes is None else \
        h.run(label, [str(a) for a in argv], file_limit_bytes=file_limit_bytes)
    if r["exit_code"] != 0:
        try:
            message = (json.loads(Path(r["stdout"]).read_text()).get("error") or {}).get("message")
        except (OSError, ValueError, AttributeError):
            message = None
        raise GateError(f"{label}: exit {r['exit_code']}" + (f": {message}" if message else ""))
    value = json.loads(Path(r["stdout"]).read_text())
    if value.get("schema") == "rusteda.v1" and (value.get("ok") is not True or value.get("error")):
        raise GateError(f"{label}: unsuccessful rusteda.v1 envelope")
    return value


def spef_binding(spefs: dict[str, Path], expected: dict[str, str]) -> dict:
    """Each SPEF must still be the file its extractor wrote (sha256)."""
    rows, bad = {}, []
    for corner, path in spefs.items():
        actual = sha256_file(path) if Path(path).is_file() else None
        ok = actual is not None and actual == expected.get(corner)
        rows[corner] = dict(path=str(path), expected=expected.get(corner), actual=actual)
        if not ok:
            bad.append(corner)
    return item("passed" if not bad else "failed",
                None if not bad else f"SPEF differs from its extraction receipt at {bad}", files=rows)


# ----------------------------------------------------------------------------- RustSTA

def ruststa(h, tag: str, tools: dict, libs: dict, netlist: Path, sdc: Path, spefs: dict,
            clock_model: str = "propagated", rc_limits: list[str] | None = None, state_gib: str = "1",
            file_limit_bytes: int | None = None) -> dict:
    ws = h.root / f"{tag}_ws"
    common = ["--workspace", ws, "--threads", "2", "--format", "json", "--color", "never"]
    rusteda, sta = tools["rusteda"], tools["ruststa"]
    a = run_json(h, f"{tag}_init", [rusteda, *common, "init"])
    argv = [rusteda, *common, "import", "--base-rev", a["new_rev"]]
    for c in libs:
        argv += ["--lib", libs[c], "--corner", c]
    argv += ["--netlist", netlist, "--sdc", sdc]
    for c in libs:
        argv += ["--spef", spefs[c], "--spef-corner", c]
    argv += rc_limits or ["--max-rc-nodes", "200000", "--max-rc-elements", "200000"]
    imp = run_json(h, f"{tag}_import", argv, file_limit_bytes)
    out = dict(workspace=str(ws), import_metrics=imp.get("metrics"))
    base = imp["new_rev"]
    for mode, extra in (("gating", []), ("compat", COMPAT)):
        r = run_json(h, f"{tag}_run_{mode}", [sta, *common, "run", "--base-rev", base, "--backend", "cpu",
                                               "--precision", "f64", "--max-paths", "16", "--max-state-gib", state_gib,
                                               "--require-wire-model", "spef_rc", "-q", *extra], file_limit_bytes)
        rev = r["new_rev"]
        base = rev
        if r["scope"].get("wire_model") != "spef_rc" or r["scope"].get("clock_model") != clock_model or \
                (clock_model == "ideal" and r["scope"].get("physical_clock_domains") != 0):
            raise GateError(f"{tag} {mode}: wrong timing scope {r['scope']} (required clock model {clock_model})")
        unclocked = {}
        for d in r.get("diagnostics", []):
            m = T0133.search(d.get("message") or "") if d.get("code") == "T0133" else None
            if m:
                unclocked[m[1]] = int(m[2])
        entry = dict(revision=rev, scope=r["scope"], completeness=r["completeness"], metrics=r["metrics"],
                     diagnostics=sorted({d.get("code") for d in r.get("diagnostics", []) if d.get("code")}),
                     unclocked_pulse_pins=unclocked, endpoints={}, reports={})
        for c in libs:
            e = run_json(h, f"{tag}_{mode}_endpoints_{c}", [sta, *common, "report", "--rev", rev, "--section",
                                                            "endpoints", "--corner", c, "--limit", "0", "-q"])
            sec = e["sections"]["endpoints"]
            if sec["matched"] != sec["total"] or len(sec["rows"]) != sec["total"]:
                raise GateError(f"{tag}: endpoint report truncated at {c}")
            entry["endpoints"][c] = {row["endpoint"]: dict(setup=row["setup_slack_ns"], hold=row["hold_slack_ns"])
                                     for row in sec["rows"]}
        for section in ("pulse_width", "min_period", "drv", "limits"):
            d = run_json(h, f"{tag}_{mode}_{section}", [sta, *common, "report", "--rev", rev, "--section", section,
                                                        "--limit", "0", "-q"])
            sec = d["sections"][section]
            if sec.get("truncated") or len(sec.get("rows", [])) != sec.get("total"):
                raise GateError(f"{tag}: {section} report truncated")
            entry["reports"][section] = sec
        out[mode] = entry
    return out


def async_pulse_pins(netlist: Path, lib: Path) -> list[str]:
    """``instance/pin`` of every asynchronous set/clear input (Liberty ``ff`` /
    ``latch`` ``clear`` or ``preset``) that carries a min-pulse-width check."""
    import liberty_lite as LL
    inst = re.compile(r"^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$")
    conn = re.compile(r"\.(\w+)\(([^()]*)\)")
    rows = []
    for line in Path(netlist).read_text().splitlines():
        m = inst.match(line)
        if m and not m[1].startswith(("module", "input", "output", "wire", "assign")):
            rows.append((m[1], m[2].lstrip("\\"), [p for p, _ in conn.findall(m[3])]))
    masters = {r[0] for r in rows}
    library = LL.Library(lib, wanted=masters)
    async_pins = {}
    for name, cell in library.cells.items():
        pins = set()
        for g in cell.groups:
            if g.kind in ("ff", "latch"):
                for key in ("clear", "preset"):
                    pins |= set(re.findall(r"[A-Za-z_][A-Za-z0-9_]*", g.attr(key) or ""))
        checked = set()
        for pin in cell.sub("pin"):
            if pin.name in pins and (pin.attrs.get("min_pulse_width_low") or pin.attrs.get("min_pulse_width_high") or
                                     any(t.attr("timing_type") == "min_pulse_width" for t in pin.sub("timing"))):
                checked.add(pin.name)
        async_pins[name] = checked
    return sorted(f"{i}/{p}" for master, i, conns in rows for p in conns if p in async_pins.get(master, ()))


# ----------------------------------------------------------------------------- OpenSTA

def tcl(path: Path) -> str:
    if any(ch in str(path) for ch in "{}\n\r"):
        raise GateError("paths cannot contain braces or newlines")
    return "{" + str(path) + "}"


MARKS = ["RR_ANNOT", "RR_SETUP", "RR_HOLD", "RR_MPW", "RR_PER", "RR_SLEW", "RR_CAP", "RR_FAN", "RR_END"]


def parse_opensta(text: str) -> dict:
    if not re.search(r"(?mi)^\s*Time\s+1ns\s*$", text):
        raise GateError("OpenSTA units not reported as 1ns")
    if re.search(r"(?mi)^\s*Error(?:\s+\d+)?:", text):
        raise GateError("OpenSTA reported an error")
    for m in MARKS:
        if text.count(m + "\n") != 1:
            raise GateError(f"OpenSTA output marker {m} missing")
    sec = {a: text.split(a + "\n", 1)[1].split(b + "\n", 1)[0] for a, b in zip(MARKS, MARKS[1:])}
    ann = re.search(r"Found (\d+) unannotated drivers", sec["RR_ANNOT"])
    part = re.search(r"Found (\d+) partially unannotated drivers", sec["RR_ANNOT"])
    if ann is None or part is None:
        raise GateError("OpenSTA parasitic annotation report not recognised")
    res = dict(unannotated=int(ann[1]), partially_unannotated=int(part[1]), setup={}, hold={}, mpw={}, period={},
               max_transition={}, max_capacitance={}, max_fanout={})
    for key, mark in (("setup", "RR_SETUP"), ("hold", "RR_HOLD")):
        for line in sec[mark].splitlines():
            m = ROW.match(line)
            if m:
                res[key][m[1]] = float(m[5])
    for line in sec["RR_MPW"].splitlines():
        m = ROW.match(line)
        if m and m[2] in ("high", "low"):
            res["mpw"][f"{m[1]}:{m[2]}"] = dict(required=float(m[3]), actual=float(m[4]), slack=float(m[5]),
                                                 met=m[6] == "MET")
    for line in sec["RR_PER"].splitlines():
        m = ROW.match(line)
        if m:
            res["period"][m[1]] = dict(period=float(m[3]), min_period=float(m[4]), slack=float(m[5]), met=m[6] == "MET")
    for key, mark in (("max_transition", "RR_SLEW"), ("max_capacitance", "RR_CAP"), ("max_fanout", "RR_FAN")):
        for line in sec[mark].splitlines():
            m = ROW.match(line)
            if m:
                lim, val = float(m[3]), float(m[4])
                res[key][m[1]] = dict(limit=lim, value=val, slack=lim - val, violated=m[6] == "VIOLATED")
    return res


def opensta(h, label: str, opensta_bin: Path, lib: Path, netlist: Path, top: str, sdc: Path, spef: Path,
            driver_arc: tuple[str, str] | None = None) -> dict:
    script = h.root / f"{label}.tcl"
    checks = {"RR_MPW": "-min_pulse_width", "RR_PER": "-min_period", "RR_SLEW": "-max_slew",
              "RR_CAP": "-max_capacitance", "RR_FAN": "-max_fanout"}
    body = [f"read_liberty {tcl(lib)}", f"read_verilog {tcl(netlist)}", f"link_design {top}",
            f"read_sdc {tcl(sdc)}", f"read_spef {tcl(spef)}", "report_units",
            "puts RR_ANNOT", "report_parasitic_annotation",
            "puts RR_SETUP",
            "report_checks -path_delay max -group_path_count 100000 -endpoint_path_count 1 -format end -digits 12",
            "puts RR_HOLD",
            "report_checks -path_delay min -group_path_count 100000 -endpoint_path_count 1 -format end -digits 12"]
    for mark in ("RR_MPW", "RR_PER", "RR_SLEW", "RR_CAP", "RR_FAN"):
        body += [f"puts {mark}", f"report_check_types {checks[mark]} -max_count 100000 -digits 12"]
    body += ["puts RR_END"]
    if driver_arc:
        # Published CLI observation only; the ordinary gate does not request it.
        body += ["puts RR_DRIVER", "report_dcalc -from " + tcl(Path(driver_arc[0])) +
                 " -to " + tcl(Path(driver_arc[1])) + " -digits 12"]
    body += ["exit", ""]
    script.write_text("\n".join(body))
    r = h.run(label, [str(opensta_bin), "-no_init", "-no_splash", "-exit", str(script)])
    if r["exit_code"] != 0:
        raise GateError(f"{label}: OpenSTA exit {r['exit_code']}")
    text = Path(r["stdout"]).read_text(errors="replace") + Path(r["stderr"]).read_text(errors="replace")
    out = parse_opensta(text)
    out["receipt"] = label
    return out


# ----------------------------------------------------------------------------- evaluation

def worst_of(rows: dict, kind: str):
    values = [v[kind] for v in rows.values() if v.get(kind) is not None]
    return min(values) if values else None


def endpoint_parity(rust: dict, ref: dict, tolerance: float) -> dict:
    names = set(rust)
    ref_names = set(ref["setup"]) | set(ref["hold"])
    missing = sorted(names ^ ref_names) + sorted(e for e in names if e not in ref["setup"] or e not in ref["hold"])
    diffs = []
    for e in sorted(names & set(ref["setup"]) & set(ref["hold"])):
        for kind in ("setup", "hold"):
            a, b = rust[e][kind], ref[kind][e]
            if a is None or not (math.isfinite(a) and math.isfinite(b)):
                missing.append(f"{e}:{kind}")
                continue
            diffs.append(dict(endpoint=e, kind=kind, ruststa_ns=a, opensta_ns=b, difference_ns=abs(a - b)))
    worst_diff = max((d["difference_ns"] for d in diffs), default=None)
    ok = not missing and diffs and worst_diff <= tolerance
    return dict(status="passed" if ok else "failed", endpoints=len(names), comparisons=len(diffs),
                max_difference_ns=worst_diff, tolerance_ns=tolerance, missing=sorted(set(missing))[:20],
                worst_rows=sorted(diffs, key=lambda d: -d["difference_ns"])[:6])


def pulse_parity(compat: dict, ref: dict, corner: str, tolerance: float,
                 declarations: dict | None = None) -> dict:
    mine = {}
    for r in compat["reports"]["pulse_width"]["rows"]:
        if r["corner"] != corner:
            continue
        key = f"{r['pin']}:{'high' if r['kind'].endswith('high') else 'low'}"
        if key not in mine or r["slack"] < mine[key]["slack"]:
            mine[key] = r
    period = {r["pin"]: r for r in compat["reports"]["min_period"]["rows"] if r["corner"] == corner}
    missing = sorted(set(ref["mpw"]) ^ set(mine)) + sorted(set(ref["period"]) ^ set(period))
    rows = []
    for k in sorted(set(ref["mpw"]) & set(mine)):
        rows.append(dict(check=k, ruststa_slack_ns=mine[k]["slack"], opensta_slack_ns=ref["mpw"][k]["slack"],
                         ruststa_required_ns=mine[k]["limit"], opensta_required_ns=ref["mpw"][k]["required"],
                         difference_ns=max(abs(mine[k]["slack"] - ref["mpw"][k]["slack"]),
                                           abs(mine[k]["limit"] - ref["mpw"][k]["required"]))))
    for k in sorted(set(ref["period"]) & set(period)):
        rows.append(dict(check=f"{k}:min_period", ruststa_slack_ns=period[k]["slack"],
                         opensta_slack_ns=ref["period"][k]["slack"], ruststa_required_ns=period[k]["limit"],
                         opensta_required_ns=ref["period"][k]["min_period"],
                         difference_ns=max(abs(period[k]["slack"] - ref["period"][k]["slack"]),
                                           abs(period[k]["limit"] - ref["period"][k]["min_period"]))))
    diff = max((r["difference_ns"] for r in rows), default=0.0)
    declarations = declarations or {}
    declared, uncovered = [], []
    for row in rows:
        if row["difference_ns"] <= tolerance:
            continue
        proof = declarations.get(row["check"])
        if (row["check"].endswith((":high", ":low")) and isinstance(proof, dict)
                and proof.get("valid") is True):
            declared.append(dict(check=row["check"], difference_ns=row["difference_ns"], evidence=proof))
        else:
            uncovered.append(row["check"])
    status = "failed" if missing or uncovered else "deviation_declared" if declared else "passed"
    return dict(status=status, mpw_rows=len(mine),
                period_rows=len(period), max_difference_ns=diff, missing_or_extra=missing[:20],
                worst_rows=sorted(rows, key=lambda r: -r["difference_ns"])[:4],
                declared_deviations=declared, undeclared_mismatches=uncovered,
                tolerance_ns=tolerance)


def drv_parity(gating: dict, ref: dict, corner: str) -> dict:
    out = {}
    for kind in DRV_KINDS:
        rust_viol = sorted(r["pin"] for r in gating["reports"]["drv"]["rows"]
                           if r["corner"] == corner and r["kind"] == kind and r["slack"] < 0)
        ref_viol = sorted(p for p, v in ref[kind].items() if v["violated"])
        out[kind] = dict(ruststa_violators=rust_viol, opensta_violators=ref_viol, equal=rust_viol == ref_viol)
    out["status"] = "passed" if all(v["equal"] for v in out.values()) else "failed"
    return out


def evaluate_source(rust: dict, ref: dict, corners: list[str], budgets: dict, clocked: bool = True,
                    async_pins: list[str] = (), declarations: dict | None = None) -> dict:
    """Gating statuses of one SPEF source; `ref` maps corner -> OpenSTA result.

    ``clocked`` False (combinational design, virtual I/O clock): the pulse
    checks are ``not_applicable`` only when both engines prove there is
    nothing to check; any pulse-width/period row makes them gate as usual."""
    g, compat = rust["gating"], rust["compat"]
    hold_m, setup_m, tol = budgets["hold_margin_ns"], budgets["setup_margin_ns"], budgets["parity_ns"]
    matrix, setup_bad, hold_bad, unverified = {}, [], [], []
    for c in corners:
        r_rows, o = g["endpoints"][c], ref[c]
        cell = dict(ruststa=dict(worst_setup_ns=worst_of(r_rows, "setup"), worst_hold_ns=worst_of(r_rows, "hold")),
                    opensta=dict(worst_setup_ns=min(o["setup"].values(), default=None),
                                 worst_hold_ns=min(o["hold"].values(), default=None)))
        matrix[c] = cell
        for engine in ("ruststa", "opensta"):
            s, h_ = cell[engine]["worst_setup_ns"], cell[engine]["worst_hold_ns"]
            if c in SETUP_CORNERS:
                if s is None:
                    unverified.append(f"{engine}:{c}:setup")
                elif s < setup_m:
                    setup_bad.append(dict(engine=engine, corner=c, worst_setup_ns=s, margin_ns=setup_m))
            if c in HOLD_CORNERS:
                if h_ is None:
                    unverified.append(f"{engine}:{c}:hold")
                elif h_ < hold_m:
                    hold_bad.append(dict(engine=engine, corner=c, worst_hold_ns=h_, margin_ns=hold_m))
        if o["unannotated"] or o["partially_unannotated"]:
            unverified.append(f"opensta:{c}:unannotated drivers {o['unannotated']}/{o['partially_unannotated']}")
    setup = item("failed" if setup_bad else "unverified" if unverified else "passed",
                 None if not (setup_bad or unverified) else f"setup below {setup_m} ns: {setup_bad or unverified}",
                 violations=setup_bad, corners=list(SETUP_CORNERS))
    hold = item("failed" if hold_bad else "unverified" if unverified else "passed",
                None if not (hold_bad or unverified) else f"hold below {hold_m} ns: {hold_bad or unverified}",
                violations=hold_bad, corners=list(HOLD_CORNERS))
    # MPW / minimum period: RustSTA default (worst/all) rules gate; OpenSTA must agree on met.
    m, comp, lim = g["metrics"], g["completeness"], g["reports"]["limits"]["rows"]
    pw_rows = g["reports"]["pulse_width"]["rows"]
    per_rows = g["reports"]["min_period"]["rows"]
    problems = []
    # Unchecked pulse pins are accepted only when they are exactly the
    # asynchronous set/clear pins of the netlist at every corner and neither
    # engine has a pulse row for any of them.
    unchecked = comp.get("unchecked_pulse_width_pins") or 0
    per_corner = g.get("unclocked_pulse_pins") or {}
    checked_pins = {r["pin"] for r in pw_rows + per_rows} | {k.rsplit(":", 1)[0] for c in corners for k in ref[c]["mpw"]} \
        | {k for c in corners for k in ref[c]["period"]}
    async_ok = bool(async_pins) and unchecked == len(async_pins) and \
        all(per_corner.get(c) == len(async_pins) for c in corners) and not (set(async_pins) & checked_pins)
    out_of_scope = list(async_pins) if unchecked and async_ok else []
    if m.get("pulse_width_status") != "met" and not (m.get("pulse_width_status") == "partial" and out_of_scope):
        problems.append(f"RustSTA pulse_width_status {m.get('pulse_width_status')}")
    if not pw_rows:
        problems.append("RustSTA checked no min-pulse-width pin")
    if (unchecked and not out_of_scope) or comp.get("pass_through_pulse_pins") or \
            g["reports"]["pulse_width"].get("unevaluated") or g["reports"]["min_period"].get("unevaluated"):
        problems.append("RustSTA left pulse-width/period pins unchecked or unevaluated"
                        + (f" ({unchecked} unchecked, per corner {per_corner}; {len(async_pins)} async set/clear pins)"
                           if unchecked else ""))
    if any(r["slack"] < 0 for r in pw_rows + per_rows):
        problems.append("RustSTA pulse-width/period violation")
    ref_mpw = {c: ref[c]["mpw"] for c in corners}
    ref_per = {c: ref[c]["period"] for c in corners}
    if any(not v["met"] for c in corners for v in list(ref_mpw[c].values()) + list(ref_per[c].values())):
        problems.append("OpenSTA pulse-width/period violation")
    if not all(ref_mpw[c] for c in corners):
        problems.append("OpenSTA reported no min-pulse-width rows")
    period_rows = dict(ruststa=len(per_rows), opensta=sum(len(ref_per[c]) for c in corners))
    if (period_rows["ruststa"] == 0) != (period_rows["opensta"] == 0):
        problems.append(f"minimum-period coverage differs between engines {period_rows}")
    violated = any("violation" in p for p in problems)
    nothing = dict(ruststa_pulse_width_status=m.get("pulse_width_status"), ruststa_pulse_rows=len(pw_rows),
                   ruststa_period_rows=len(per_rows), opensta_mpw_rows=sum(len(ref_mpw[c]) for c in corners),
                   opensta_period_rows=period_rows["opensta"],
                   ruststa_unchecked_pulse_pins=comp.get("unchecked_pulse_width_pins"))
    combinational = (not clocked and m.get("pulse_width_status") == "unconstrained" and not pw_rows and not per_rows
                     and not nothing["opensta_mpw_rows"] and not nothing["opensta_period_rows"]
                     and not comp.get("unchecked_pulse_width_pins") and not comp.get("pass_through_pulse_pins"))
    status = "not_applicable" if combinational else "failed" if violated else "unverified" if problems else "passed"
    mpw = item(status, ("combinational design: no clocked cell; both engines report no pulse-width or period check "
                        f"{nothing}") if combinational else "; ".join(problems) or None,
               rule="RustSTA default pulse rules (groups=worst, pins=all) gate; OpenSTA must agree",
               pulse_width_rows=len(pw_rows), min_period_rows=period_rows,
               worst_min_pulse_width_slack_ns=m.get("worst_min_pulse_width_slack_ns"),
               async_set_clear_pins_unchecked=dict(
                   count=len(out_of_scope), pins=out_of_scope[:64],
                   scope="minimum pulse width of asynchronous set/clear inputs: the pulse is generated outside the "
                         "block (no clock reaches these pins; neither engine checks them); recovery/removal of the "
                         "same pins is timed (timing_setup/timing_hold)") if out_of_scope else None,
               worst_min_period_slack_ns=m.get("worst_min_period_slack_ns"),
               min_period_note=None if period_rows["ruststa"] else
               "no Liberty minimum_period constraint on the clocked cells (both engines report none)")
    # DRV: every kind checked at every corner by RustSTA; no violator in either engine.
    problems = []
    if m.get("drv_status") != "met":
        problems.append(f"RustSTA drv_status {m.get('drv_status')}")
    for c in corners:
        for kind in DRV_KINDS:
            rows = [x for x in lim if x["corner"] == c and x["kind"] == kind]
            if len(rows) != 1 or not rows[0]["checked"] or rows[0]["unevaluated"]:
                problems.append(f"RustSTA {kind} not fully checked at {c}")
            elif rows[0]["violating"]:
                problems.append(f"RustSTA {kind} violation at {c}")
            if any(v["violated"] for v in ref[c][kind].values()):
                problems.append(f"OpenSTA {kind} violation at {c}")
            if not ref[c][kind]:
                problems.append(f"OpenSTA checked no {kind} pin at {c}")
    for key in ("drv_untimed_pins", "drv_ideal_clock_pins"):
        if comp.get(key):
            problems.append(f"RustSTA {key} = {comp.get(key)}")
    violated = any("violation" in p for p in problems)
    drv = item("failed" if violated else "unverified" if problems else "passed", "; ".join(problems) or None,
               limits=[x for x in lim if x["kind"] in DRV_KINDS])
    # Accuracy (RustSTA vs OpenSTA).
    acc = {c: dict(endpoints=endpoint_parity(g["endpoints"][c], ref[c], tol),
                   pulse=pulse_parity(compat, ref[c], c, tol, (declarations or {}).get(c)),
                   drv=drv_parity(g, ref[c], c)) for c in corners}
    failing = [f"{c}:{k}" for c, v in acc.items() for k, x in v.items()
               if x["status"] != "passed" and not (k == "pulse" and x["status"] == "deviation_declared")]
    declared = [dict(corner=c, **row) for c, v in acc.items()
                for row in v["pulse"].get("declared_deviations", [])]
    status = "failed" if failing else "deviation_declared" if declared else "passed"
    accuracy = item(status, f"parity outside {tol * 1e3:g} ps: {failing}" if failing else
                    "owner-approved MPW deviations are listed explicitly" if declared else None,
                    corners=acc, failing=failing, declared_deviations=declared)
    return dict(timing_setup=setup, timing_hold=hold, mpw_min_period=mpw, drv=drv, timing_accuracy=accuracy,
                matrix=matrix)


def unverified_source(reason: str) -> dict:
    """Statuses of a SPEF source that could not be timed (engine refused it)."""
    out = {k: item("unverified", reason) for k in ("timing_setup", "timing_hold", "mpw_min_period", "drv",
                                                  "timing_accuracy")}
    out["timing_accuracy"]["corners"] = {}
    out["matrix"] = {}
    return out


def combine(sources: dict[str, dict]) -> dict:
    """Per-status worst over SPEF sources; details keep every source."""
    out = {}
    for key in ("timing_setup", "timing_hold", "mpw_min_period", "drv", "timing_accuracy"):
        parts = {s: v[key] for s, v in sources.items()}
        st = worst(p["status"] for p in parts.values())
        if "not_applicable" in [p["status"] for p in parts.values()] and st in ("passed", "deviation_declared"):
            # One source not applicable, another passed: the sources disagree.
            st = "unverified"
        reasons = [f"{s}: {p['reason']}" for s, p in parts.items() if p.get("reason")]
        out[key] = item(st, "; ".join(reasons) or None, by_spef=parts)
    return out
