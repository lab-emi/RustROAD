#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate an MPW declaration experiment on explicit, immutable input files.

Run under a 2 GiB memory / 600 second wall-clock scope. This command writes a
fresh evidence directory; it never changes a workspace, PDK or input file.
"""
from __future__ import annotations

import argparse
import fcntl
import json
from pathlib import Path
import resource

import rtl2gds_design as D
import rtl2gds_timing as T
from rtl2gds_mpw import prove_deviations
from verify_physical import PhysicalHarness


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for option in ("netlist", "liberty", "sdc", "spef", "run"):
        p.add_argument("--"+option, type=Path, required=True)
    p.add_argument("--top", required=True)
    p.add_argument("--pdk", choices=("sky130", "gf180"), required=True)
    p.add_argument("--corner", choices=("tt", "ss", "ff"), required=True)
    p.add_argument("--tools", type=Path, default=D.ROOT/"fixtures/flow/rtl2gds-tools.json")
    p.add_argument("--manifest", type=Path, default=D.ROOT/"fixtures/flow/signoff-pinned.json")
    a = p.parse_args()
    root = a.run.resolve()
    root.mkdir(parents=True, exist_ok=False)
    with (root/"workspace.lock").open("wb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        resource.setrlimit(resource.RLIMIT_AS, (2*1024**3, 2*1024**3))
        result = dict(status="unproven", scope=dict(pdk=a.pdk, corner=a.corner), declarations={})
        try:
            entries = D.load_tools(a.tools, {})
            names = ("rusteda", "ruststa", "opensta", "ngspice")
            if any(entries.get(n, {}).get("status") != "passed" for n in names):
                raise D.GateError("MPW tools must match all four pinned executables")
            tools = {n:Path(entries[n]["path"]) for n in names}
            pdk = json.loads(a.manifest.read_text())["pdks"][a.pdk]
            if D.sha256_file(a.liberty) != pdk["files"]["liberty_"+a.corner]["sha256"]:
                raise D.GateError("MPW declaration needs the pinned PDK Liberty")
            h = PhysicalHarness(root, argparse.Namespace(threads=1, memory_mib=1536, timeout_seconds=150))
            h.run("memory_preflight", ["free", "-h"])
            rust = T.ruststa(h, "original", tools, {a.corner:a.liberty.resolve()}, a.netlist.resolve(),
                            a.sdc.resolve(), {a.corner:a.spef.resolve()})
            ref = {a.corner:T.opensta(h, "original_ref", tools["opensta"], a.liberty.resolve(),
                                     a.netlist.resolve(), a.top, a.sdc.resolve(), a.spef.resolve())}
            declarations, attempts = prove_deviations(
                h, "explain", tools, pdk, a.pdk, "explicit", [a.corner], a.top,
                a.netlist.resolve(), {a.corner:a.liberty.resolve()}, a.sdc.resolve(),
                {a.corner:a.spef.resolve()}, rust, ref, .001)
            parity = T.pulse_parity(rust["compat"], ref[a.corner], a.corner, .001,
                                    declarations.get(a.corner))
            result.update(status=parity["status"], parity=parity, declarations=declarations, attempts=attempts)
        except (D.GateError, OSError, KeyError, ValueError) as error:
            result["reason"] = str(error)
        (root/"explanation.json").write_text(json.dumps(result, indent=2, allow_nan=False)+"\n")
    print(json.dumps(dict(status=result["status"], evidence=str(root/"explanation.json"))))
    return 0 if result["status"] in ("passed", "deviation_declared") else 3


if __name__ == "__main__":
    raise SystemExit(main())
