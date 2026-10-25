# SPDX-License-Identifier: Apache-2.0
"""Validate the owner-approved, narrowly scoped MPW deviation evidence."""
from __future__ import annotations

import math
from pathlib import Path

from rtl2gds_design import sha256_file

SCHEMA = "rustroad.mpw-deviation.v1"
NEARBY_TOLERANCE_NS = .0002
MAX_RELATIVE_DISTANCE = .2
MAX_BRACKET_WIDTH = .0001
MAX_CONTINUOUS_DELTA_NS = .00005


def number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def difference(row):
    keys = ("ruststa_required_ns", "opensta_required_ns", "ruststa_slack_ns", "opensta_slack_ns")
    if not isinstance(row, dict) or not all(number(row.get(k)) for k in keys):
        return None
    return max(abs(row["ruststa_required_ns"] - row["opensta_required_ns"]),
               abs(row["ruststa_slack_ns"] - row["opensta_slack_ns"]))


def validate(proof: dict, expected_binding: dict, actual: dict, work_root: Path) -> dict:
    """A missing, stale or inconclusive proof cannot convert a parity failure."""
    bad = []
    if not isinstance(proof, dict) or proof.get("schema") != SCHEMA:
        return dict(valid=False, reasons=["missing/unknown MPW evidence schema"])
    if (not all(isinstance(proof.get(k), dict) for k in ("binding", "original", "nearby", "step", "circuit"))
            or not isinstance(proof.get("artifacts"), list)
            or not all(isinstance(proof["step"].get(k), dict) for k in ("lo", "hi"))
            or not isinstance(proof["circuit"].get("points"), list)
            or any(not isinstance(p, dict) for p in proof["circuit"]["points"] + proof["artifacts"])
            or difference(actual) is None):
        return dict(valid=False, reasons=["malformed MPW evidence"])
    if proof.get("binding") != expected_binding:
        bad.append("MPW evidence does not bind the current pin, files and tools")
    original = proof.get("original") or {}
    for key, value in actual.items():
        if not number(original.get(key)) or abs(original[key] - value) > 1e-10:
            bad.append("original MPW measurements differ from the gate")
            break
    near = proof.get("nearby") or {}
    distance = abs(near.get("scale", 99) - 1) if number(near.get("scale")) else 99
    near_difference = difference(near)
    if not (0 < distance <= MAX_RELATIVE_DISTANCE and near_difference is not None
            and near_difference <= NEARBY_TOLERANCE_NS):
        bad.append("nearby sample across the step exceeds 0.2 ps or the bounded search radius")
    lo, hi = (proof.get("step") or {}).get("lo", {}), (proof.get("step") or {}).get("hi", {})
    scales = [x.get("scale") for x in (lo, hi)]
    step_ok = all(number(x) for x in scales)
    if step_ok:
        a, b = scales
        step_ok = (0 < a < b and b-a <= MAX_BRACKET_WIDTH
                   and min(1, near.get("scale", 99)) <= a < b <= max(1, near.get("scale", 99)))
    if step_ok and difference(lo) is not None and difference(hi) is not None:
        discontinuous = max(abs(hi[k] - lo[k]) for k in ("opensta_required_ns", "opensta_slack_ns"))
        continuous = max(abs(hi[k] - lo[k]) for k in ("ruststa_required_ns", "ruststa_slack_ns"))
        driver = [point.get("opensta_driver_slew_ns") for point in (lo, hi)]
        driver_step = abs(driver[1]-driver[0]) if all(number(v) for v in driver) else 0
        step_ok = (discontinuous >= NEARBY_TOLERANCE_NS and continuous <= MAX_CONTINUOUS_DELTA_NS
                   and discontinuous >= 5 * continuous
                   and driver_step >= .0002
                   and min(difference(lo), difference(hi)) <= NEARBY_TOLERANCE_NS
                   and max(difference(lo), difference(hi)) > NEARBY_TOLERANCE_NS)
    else:
        step_ok = False
    if not step_ok:
        bad.append("reference slew step is not isolated between the actual and nearby resistance")
    circuit = proof.get("circuit") or {}
    points = circuit.get("points") or []
    if (circuit.get("model") != "transistor_rc_with_independent_driver_characterization"
            or not 1 <= len(points) <= 16):
        bad.append("missing bounded transistor circuit comparison")
    else:
        for point in points:
            fields = ("ruststa_required_ns", "opensta_required_ns", "spice_required_ns", "uncertainty_ns")
            if not all(number(point.get(k)) for k in fields):
                bad.append("nonfinite circuit comparison")
                break
            uncertainty = point["uncertainty_ns"]
            ours = abs(point["ruststa_required_ns"] - point["spice_required_ns"])
            other = abs(point["opensta_required_ns"] - point["spice_required_ns"])
            if not 0 <= uncertainty <= NEARBY_TOLERANCE_NS or ours + uncertainty >= other - uncertainty:
                bad.append("circuit oracle does not unambiguously favor RustSTA")
                break
    artifacts = proof.get("artifacts") or []
    if not 5 <= len(artifacts) <= 64:
        bad.append("incomplete evidence artifact list")
    else:
        root = work_root.resolve()
        for entry in artifacts:
            if not isinstance(entry.get("path"), str):
                bad.append("malformed evidence path")
                break
            path = Path(entry["path"]).resolve()
            if (not path.is_relative_to(root) or not path.is_file() or path.stat().st_size > 64*1024**2
                    or sha256_file(path) != entry.get("sha256")):
                bad.append("evidence artifact is missing, changed, oversized or outside the run")
                break
    return dict(valid=not bad, reasons=bad, nearby_difference_ns=near_difference,
                policy="all three owner-approved MPW conditions; ordinary parity remains 1 ps")
