#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate fixtures/flow/cell-policy.json from the PDK exclusion lists.

usage: cell_policy.py generate [--manifest fixtures/flow/signoff-pinned.json]
                               [--producer-receipt .work/tools/rustsys-c5ec23c.build.json]
                               [--out fixtures/flow/cell-policy.json]

The synthesis ``--dont-use`` globs per PDK are exactly the union of the PDK
files (pinned by sha256 in the signoff manifest) plus the owner's reserved
CTS/hold cells for GF180 (dly*, clkbuf*, clkinv*); sky130's synthesis list
already reserves its clock/delay cells. ``pnr_exclude`` cells must never
appear in any netlist. The independent checker is scripts/check_cell_policy.py.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts/signoff"))
import manifest as manifest_mod  # noqa: E402

RESERVED = {
    "sky130": [],
    "gf180": ["gf180mcu_fd_sc_mcu7t5v0__dly*", "gf180mcu_fd_sc_mcu7t5v0__clkbuf*",
              "gf180mcu_fd_sc_mcu7t5v0__clkinv*"],
}
# Cells RustROAD itself may insert after synthesis (never chosen by synthesis).
POST_SYNTHESIS_INSERTABLE = {
    "sky130": dict(physical_only=["sky130_fd_sc_hd__tap*", "sky130_fd_sc_hd__decap*", "sky130_fd_sc_hd__fill*",
                                  "sky130_fd_sc_hd__diode*"],
                   cts_hold=["sky130_fd_sc_hd__clkbuf*", "sky130_fd_sc_hd__clkinv*", "sky130_fd_sc_hd__clkinvlp*",
                             "sky130_fd_sc_hd__clkdlybuf*", "sky130_fd_sc_hd__dlygate*", "sky130_fd_sc_hd__dlymetal*"]),
    "gf180": dict(physical_only=["gf180mcu_fd_sc_mcu7t5v0__filltie", "gf180mcu_fd_sc_mcu7t5v0__fill_*",
                                 "gf180mcu_fd_sc_mcu7t5v0__fillcap_*", "gf180mcu_fd_sc_mcu7t5v0__endcap",
                                 "gf180mcu_fd_sc_mcu7t5v0__antenna"],
                  cts_hold=list(RESERVED["gf180"])),
}


def read_cells(path: Path) -> list[str]:
    out = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            out.append(line)
    return out


def generate(manifest_path: Path, producer_receipt: Path | None) -> dict:
    m = manifest_mod.load(manifest_path)
    pdks = {}
    for tag in ("sky130", "gf180"):
        spec = m["pdks"][tag]
        sources, synth, pnr = [], [], []
        for key in ("synth_exclude", "pnr_exclude"):
            path = manifest_mod.pdk_host_path(m, tag, key)
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            if digest != spec["files"][key]["sha256"]:
                raise SystemExit(f"{path} differs from the signoff manifest")
            cells = read_cells(path)
            sources.append(dict(role=key, path=spec["files"][key]["path"], sha256=digest, entries=cells))
            (synth if key == "synth_exclude" else pnr).extend(cells)
        dont_use = []
        for glob in synth + pnr + RESERVED[tag]:
            if glob not in dont_use:
                dont_use.append(glob)
        pdks[tag] = dict(library=spec["library"], pdk_version=spec["version"], sources=sources,
                         reserved_for_cts_hold=RESERVED[tag], synth_dont_use=dont_use, pnr_exclude=pnr,
                         post_synthesis_insertable=POST_SYNTHESIS_INSERTABLE[tag],
                         rustsys_args=[x for g in dont_use for x in ("--dont-use", g)])
    policy = dict(schema="rustroad.cell-policy.v1",
                  rules=dict(synthesis="mapped netlists contain no cell matching synth_dont_use",
                             physical="no netlist contains a pnr_exclude cell; synth_dont_use cells may appear only "
                                      "on instances added after synthesis and only if post_synthesis_insertable",
                             glob="'*' and '?' as in RustSys --dont-use, matched against the full cell name"),
                  manifest=str(manifest_path.relative_to(ROOT)) if manifest_path.is_relative_to(ROOT) else str(manifest_path),
                  pdks=pdks)
    if producer_receipt is not None:
        policy["producer"] = json.loads(producer_receipt.read_text())
    policy["frozen_producer"] = dict(
        path="/mnt/data2/cgao/git/RustSys/target/release/rustsys",
        sha256="9c83124e7bcf1a9ced5675672f3587099afec796de3230679fbff638c0b09e50",
        note="historical frozen producer without --dont-use; reproduce with verify_routing.py --producer frozen")
    return policy


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="action", required=True)
    g = sub.add_parser("generate")
    g.add_argument("--manifest", type=Path, default=manifest_mod.DEFAULT_MANIFEST)
    g.add_argument("--producer-receipt", type=Path)
    g.add_argument("--out", type=Path, default=ROOT / "fixtures/flow/cell-policy.json")
    args = p.parse_args(argv)
    policy = generate(args.manifest.resolve(), args.producer_receipt)
    args.out.write_text(json.dumps(policy, indent=2) + "\n")
    print(json.dumps({t: len(v["synth_dont_use"]) for t, v in policy["pdks"].items()}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
