# SPDX-License-Identifier: Apache-2.0
"""Design manifest, frozen inputs, tool pins and status algebra of the RTL-to-GDS gate.

A design manifest (schema ``rustroad.design.v1``, ``fixtures/designs/<name>.json``)
names the RTL, the top module and clock port, the SDC (primary-clock SDC plus an
I/O template rendered from the RTL port list), per-PDK floorplan / physical /
routing / delay-cell / RC-profile references, the verification parameters and
the budgets. Every tracked file is pinned by SHA-256; the rendered SDC and every
PDK file the closure engine derives (Liberty per corner, LEFs, cell GDS, OpenRCX
rules) are frozen by hash as well. Tool binaries are pinned in a separate tool
file (``fixtures/flow/rtl2gds-tools.json``).

Statuses are ``passed`` / ``failed`` / ``unverified`` (``missing`` counts as
unverified). A definitive failure of any gating item fails the gate (exit 1);
otherwise any unproven gating item leaves it unverified (exit 3); exit 0 only
when every gating item passed. Pure functions here are unit-tested
(scripts/tests/test_rtl2gds.py).
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
from string import Template

ROOT = Path(__file__).resolve().parents[1]
DESIGN_SCHEMA = "rustroad.design.v1"
TOOLS_SCHEMA = "rustroad.rtl2gds-tools.v1"
SUMMARY_SCHEMA = "rustroad.rtl2gds.v1"
EXIT = {"passed": 0, "failed": 1, "unverified": 3}
MAX_MANIFEST_BYTES = 1024 * 1024


class GateError(RuntimeError):
    """A gate step could not produce a verdict (maps to unverified)."""


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def resolve(path: str | Path) -> Path:
    p = Path(path)
    return p if p.is_absolute() else (ROOT / p)


# ----------------------------------------------------------------------------- statuses

def item(status: str, reason: str | None = None, **extra) -> dict:
    if status not in ("passed", "failed", "unverified", "missing"):
        raise ValueError(f"unknown status {status!r}")
    out = dict(status=status)
    if reason:
        out["reason"] = reason
    out.update(extra)
    return out


def worst(statuses) -> str:
    """failed > unverified (incl. missing) > passed; an empty list is unverified."""
    statuses = list(statuses)
    if not statuses:
        return "unverified"
    if "failed" in statuses:
        return "failed"
    if any(s != "passed" for s in statuses):
        return "unverified"
    return "passed"


def overall(per_pdk: dict[str, dict[str, dict]], gating: list[str], global_items: dict[str, dict],
            required_pdks: list[str]) -> dict:
    """Combine per-PDK gating statuses and global items into the gate verdict.

    Every required PDK must be present with every gating item; a missing item
    is unverified, never passed.
    """
    failed, unverified = [], []
    for tag in required_pdks:
        statuses = per_pdk.get(tag)
        if statuses is None:
            unverified.append(f"{tag}:pdk_run")
            continue
        for key in gating:
            st = (statuses.get(key) or {}).get("status", "missing")
            if st == "failed":
                failed.append(f"{tag}:{key}")
            elif st != "passed":
                unverified.append(f"{tag}:{key}")
    for key, value in global_items.items():
        st = (value or {}).get("status", "missing")
        if st == "failed":
            failed.append(f"*:{key}")
        elif st != "passed":
            unverified.append(f"*:{key}")
    status = "failed" if failed else "unverified" if unverified else "passed"
    return dict(status=status, exit_code=EXIT[status], failed_items=failed, unverified_items=unverified)


# ----------------------------------------------------------------------------- manifest

REQUIRED_KEYS = ("schema", "name", "top", "rtl", "clock", "sdc", "pdk_profiles", "cell_policy",
                 "signoff_manifest", "tools", "pdks", "verification", "budgets")
PDK_KEYS = ("floorplan", "physical_config", "routing_config", "delay_cells", "gds_layer_map",
            "rc_profiles", "corners", "frozen")
BUDGET_KEYS = ("hold_margin_ns", "setup_margin_ns", "hold_eco_setup_margin_ns", "parity_ns",
               "max_hold_iterations", "process_timeout_s", "process_memory_mib", "max_wall_s",
               "container_memory", "container_timeout_s")
VERIFICATION_KEYS = ("cosim_vectors", "cosim_seed", "rustsys_budget", "rustsys_model_budget",
                     "rustsys_sweep_max_candidates", "yosys_equiv_seq")


def load_manifest(path: Path) -> dict:
    path = Path(path)
    if path.stat().st_size > MAX_MANIFEST_BYTES:
        raise GateError(f"design manifest {path} exceeds {MAX_MANIFEST_BYTES} bytes")
    m = json.loads(path.read_text())
    problems = [f"missing key {k}" for k in REQUIRED_KEYS if k not in m]
    if m.get("schema") != DESIGN_SCHEMA:
        problems.append(f"schema {m.get('schema')!r} is not {DESIGN_SCHEMA}")
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_$]*", str(m.get("top", ""))):
        problems.append("top is not a simple Verilog identifier")
    for tag, spec in (m.get("pdks") or {}).items():
        problems += [f"pdks.{tag}: missing key {k}" for k in PDK_KEYS if k not in spec]
    problems += [f"budgets: missing {k}" for k in BUDGET_KEYS if k not in m.get("budgets", {})]
    problems += [f"verification: missing {k}" for k in VERIFICATION_KEYS if k not in m.get("verification", {})]
    if problems:
        raise GateError(f"design manifest {path}: " + "; ".join(problems))
    return m


def tracked_entries(m: dict) -> dict[str, dict]:
    """Every {path, sha256} entry of the manifest, keyed by a dotted name."""
    out = {}
    for i, r in enumerate(m["rtl"]):
        out[f"rtl.{i}"] = r
    out["sdc.clock"] = m["sdc"]["clock"]
    out["sdc.io_template"] = m["sdc"]["io_template"]
    for key in ("pdk_profiles", "cell_policy", "signoff_manifest", "tools"):
        out[key] = m[key]
    for tag, spec in m["pdks"].items():
        for key in ("floorplan", "physical_config", "routing_config", "delay_cells", "gds_layer_map"):
            out[f"pdks.{tag}.{key}"] = spec[key]
        for rc, entry in spec["rc_profiles"].items():
            out[f"pdks.{tag}.rc_profiles.{rc}"] = entry
    return out


def verify_tracked(m: dict) -> dict:
    """Hash every tracked manifest file; any difference is a failed identity."""
    rows, bad = {}, []
    for key, entry in tracked_entries(m).items():
        p = resolve(entry["path"])
        actual = sha256_file(p) if p.is_file() else None
        ok = actual == entry["sha256"]
        rows[key] = dict(path=str(p), expected=entry["sha256"], actual=actual, status="passed" if ok else "failed")
        if not ok:
            bad.append(key)
    return item("passed" if not bad else "failed", None if not bad else f"changed or missing: {bad}",
                files=rows, changed=bad)


def compare_frozen(expected: dict[str, str], fingerprints: dict[str, dict]) -> dict:
    """Closure-engine frozen inputs (Flow.frozen()) against the manifest's hashes."""
    rows, bad = {}, []
    for key in sorted(set(expected) | set(fingerprints)):
        want = expected.get(key)
        got = (fingerprints.get(key) or {}).get("sha256")
        ok = want is not None and got == want
        rows[key] = dict(expected=want, actual=got, path=(fingerprints.get(key) or {}).get("path"),
                         status="passed" if ok else "failed")
        if not ok:
            bad.append(key)
    return item("passed" if not bad else "failed", None if not bad else f"frozen inputs differ: {bad}",
                files=rows, changed=bad)


# ----------------------------------------------------------------------------- ports and SDC

def ports_from_yosys_json(module: dict) -> list[dict]:
    """Ports in declaration order: name, direction, width, msb, lsb, bits (scalar names).

    Yosys JSON does not distinguish ``x`` from ``[0:0] x``; a one-bit port is
    treated as a scalar (a one-bit vector port is not supported).
    """
    out = []
    for name, port in module["ports"].items():
        width = len(port["bits"])
        offset = port.get("offset", 0)
        upto = bool(port.get("upto", 0))
        lo, hi = offset, offset + width - 1
        msb, lsb = (lo, hi) if upto else (hi, lo)
        vector = width > 1
        bits = [f"{name}[{i}]" for i in range(lo, hi + 1)] if vector else [name]
        out.append(dict(name=name, direction=port["direction"], width=width, msb=msb, lsb=lsb,
                        vector=vector, bits=bits))
    return out


def render_sdc(clock_sdc: str, template: str, ports: list[dict], clock_port: str, clock_name: str) -> str:
    """Primary-clock SDC + the I/O template filled from the RTL port list.

    ${clock}: clock name; ${inputs}: every input port (declaration order);
    ${data_inputs}: inputs except the clock port; ${outputs}: every output port.
    """
    names = [p["name"] for p in ports]
    if clock_port not in names:
        raise GateError(f"clock port {clock_port!r} is not a port of the RTL top")
    if any(p["direction"] == "inout" for p in ports):
        raise GateError("inout ports are not supported by the I/O SDC template")
    inputs = [p["name"] for p in ports if p["direction"] == "input"]
    outputs = [p["name"] for p in ports if p["direction"] == "output"]
    values = dict(clock=clock_name, inputs=" ".join(inputs), outputs=" ".join(outputs),
                  data_inputs=" ".join(n for n in inputs if n != clock_port))
    return clock_sdc + Template(template).substitute(values)


# ----------------------------------------------------------------------------- tools

def load_tools(path: Path, overrides: dict[str, str]) -> dict[str, dict]:
    """Pinned tool binaries with their identity status (missing/changed never pass)."""
    data = json.loads(Path(path).read_text())
    if data.get("schema") != TOOLS_SCHEMA:
        raise GateError(f"tool file {path}: schema {data.get('schema')!r} is not {TOOLS_SCHEMA}")
    out = {}
    for name, spec in data["tools"].items():
        if "path" not in spec:
            out[name] = dict(spec, status="passed", note="image identity verified by its own harness")
            continue
        p = Path(overrides[name]) if name in overrides else resolve(spec["path"])
        entry = dict(spec, path=str(p), expected_sha256=spec["sha256"], overridden=name in overrides)
        if not p.is_file():
            entry.update(status="missing", actual_sha256=None, reason=f"{name} not found at {p}")
        else:
            actual = sha256_file(p)
            entry.update(actual_sha256=actual, status="passed" if actual == spec["sha256"] else "failed",
                         reason=None if actual == spec["sha256"] else f"{name} sha256 {actual} != pinned {spec['sha256']}")
        out[name] = entry
    unknown = sorted(set(overrides) - set(out))
    if unknown:
        raise GateError(f"--tool overrides name unknown tools: {unknown}")
    return out


def tools_status(tools: dict[str, dict]) -> dict:
    statuses = {n: t["status"] for n, t in tools.items()}
    st = worst(["unverified" if s == "missing" else s for s in statuses.values()])
    bad = {n: s for n, s in statuses.items() if s != "passed"}
    return item(st, None if not bad else f"tool identity: {bad}", tools=statuses)
