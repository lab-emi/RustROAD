# SPDX-License-Identifier: Apache-2.0
"""Top-level RTL-to-GDS status gating for scripts/verify_routing.py (pure, unit-tested).

The overall status is ``passed`` only when the flow ran functionally on every
PDK, tool identities stayed stable, the post-route timing comparison passed
and EVERY required signoff item below passed on every PDK. A definitive
failure of any required item (e.g. DRC items found, LVS mismatch, native GDS
XOR difference), a functional flow failure or an identity change gives
``failed`` (exit 1). Otherwise any missing, unverified or unchecked required
item, or a post-route timing status other than passed, gives ``partial``
(exit 3); ``partial`` therefore always means "not proven", never "proven
bad". Density is reported separately and is not a required item (owner
decision d). RustSTA design-rule (DRV) counts are not evidence (the engine
records but does not analyze max_transition/max_capacitance/max_fanout), and
unanalyzed min-pulse-width or minimum-period arcs are never a pass.
"""
from __future__ import annotations

REQUIRED = ("gds", "drc_magic", "drc_klayout", "antenna", "lvs_netgen", "lvs_klayout",
            "timing_ss_max_setup", "timing_ss_max_hold", "timing_ff_min_hold",
            "mpw_min_period", "drv", "cell_policy")
REPORTED = ("timing_tt_nom", "density")
# Both baseline PDKs must be present: a single-PDK summary is never a pass.
REQUIRED_PDKS = ("sky130", "gf180")
EXIT = {"passed": 0, "partial": 3, "unverified": 3, "failed": 1}


def item(status: str, reason: str | None = None, **extra) -> dict:
    out = dict(status=status)
    if reason:
        out["reason"] = reason
    out.update(extra)
    return out


def mpw_status(completeness: dict | None) -> dict:
    if not completeness:
        return item("missing", "no timing completeness report")
    arcs = {k: v for k, v in (completeness.get("unanalyzed_arcs") or {}).items() if v}
    wanted = {"min_pulse_width", "minimum_period"}
    if arcs:
        return item("unchecked", f"unanalyzed arcs {arcs}: min-pulse-width/min-period are not passed", unanalyzed_arcs=arcs)
    if "unanalyzed_arcs" not in completeness:
        return item("missing", "timing engine did not report unanalyzed arcs")
    return item("unchecked", f"no engine evidence that {sorted(wanted)} checks were analyzed")


def drv_status(opensta_limits: str | None, corners_checked: list[str]) -> dict:
    reason = ("RustSTA records set_max_fanout/set_max_capacitance/set_max_transition but does not analyze them "
              "(diagnostic D0021); its zero counts are not evidence")
    return item("unchecked", reason, ruststa="not_analyzed", opensta=opensta_limits,
                opensta_corners=corners_checked, required_corners=["ss_max"])


def overall(pdks: list[dict], postroute_timing_status: str, functional: bool, identity_stable: bool) -> dict:
    """Return dict(status, blocking[...]) from per-PDK ``signoff_statuses`` dicts."""
    blocking = []
    if not functional:
        return dict(status="failed", blocking=[dict(pdk="*", item="functional", status="failed")],
                    failed_items=["*:functional"])
    if not identity_stable:
        return dict(status="failed", blocking=[dict(pdk="*", item="identity", status="failed")],
                    failed_items=["*:identity"])
    if postroute_timing_status != "passed":
        blocking.append(dict(pdk="*", item="postroute_timing", status=postroute_timing_status))
    seen = [p.get("pdk") for p in pdks]
    for tag in REQUIRED_PDKS:
        if seen.count(tag) != 1:
            blocking.append(dict(pdk=tag, item="pdk_run", status="missing",
                                 reason=f"{seen.count(tag)} result entries for a required PDK (need exactly 1)"))
    for p in pdks:
        statuses = p.get("signoff_statuses") or {}
        for key in REQUIRED:
            entry = statuses.get(key) or item("missing", "not produced by this run")
            if entry.get("status") != "passed":
                blocking.append(dict(pdk=p.get("pdk"), item=key, status=entry.get("status", "missing"),
                                     reason=entry.get("reason")))
    failed = [b for b in blocking if b["status"] == "failed" and b["item"] != "postroute_timing"]
    if failed:
        return dict(status="failed", blocking=blocking, failed_items=[f"{b['pdk']}:{b['item']}" for b in failed])
    return dict(status="passed" if not blocking and pdks else "partial", blocking=blocking, failed_items=[])


def main(argv=None) -> int:
    """Recompute the gate from a verify_routing.py summary (optionally assuming all timing passed)."""
    import argparse
    import json
    from pathlib import Path
    parser = argparse.ArgumentParser(description="recompute the RTL-to-GDS gate from summary.json")
    parser.add_argument("summary", type=Path)
    parser.add_argument("--assume-timing-passed", action="store_true",
                        help="what-if: mark post-route timing and every timing corner item passed")
    args = parser.parse_args(argv)
    data = json.loads(args.summary.read_text())
    pdks = data.get("pdks", [])
    timing = data.get("postroute_timing_status", "missing")
    if args.assume_timing_passed:
        timing = "passed"
        for p in pdks:
            for key in ("timing_ss_max_setup", "timing_ss_max_hold", "timing_ff_min_hold", "timing_tt_nom"):
                (p.setdefault("signoff_statuses", {}))[key] = item("passed", "assumed (what-if)")
    gate = overall(pdks, timing, data.get("functional_status") == "passed", bool(data.get("identity_stable")))
    print(json.dumps(dict(status=gate["status"], exit=EXIT[gate["status"]], assume_timing_passed=args.assume_timing_passed,
                          failed_items=gate.get("failed_items", []),
                          blocking=[f"{b['pdk']}:{b['item']}={b['status']}" for b in gate["blocking"]])))
    return EXIT[gate["status"]]


if __name__ == "__main__":
    import sys
    sys.exit(main())
