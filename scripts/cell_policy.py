#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate fixtures/flow/cell-policy.json from the PDK exclusion lists.

usage: cell_policy.py generate [--manifest fixtures/flow/signoff-pinned.json]
                               [--producer-receipt .work/tools/rustsys-c5ec23c.build.json]
                               [--out fixtures/flow/cell-policy.json]

The synthesis ``--dont-use`` globs per PDK are exactly the union of the PDK
files (pinned by sha256 in the signoff manifest) plus the owner's reserved
CTS/hold cells for GF180 (dly*, clkbuf*, clkinv*) plus, for GF180, the
owner's DRV-limited families derived from Liberty (fixtures/flow/
gf180-drv-dont-use.json, recomputed and required identical); sky130's
synthesis list already reserves its clock/delay cells. ``pnr_exclude`` cells must never
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
sys.path.insert(0, str(ROOT / "scripts"))
import manifest as manifest_mod  # noqa: E402

RESERVED = {
    "sky130": [],
    "gf180": ["gf180mcu_fd_sc_mcu7t5v0__dly*", "gf180mcu_fd_sc_mcu7t5v0__clkbuf*",
              "gf180mcu_fd_sc_mcu7t5v0__clkinv*"],
}
# Owner decision 2026-10-24 (GF180 DRV / cell policy): families that cannot
# meet the SDC max_transition at ss even at minimum load, derived from Liberty
# by scripts/derive_drv_dont_use.py (tracked result, recomputed here).
DRV_DONT_USE = {"gf180": "fixtures/flow/gf180-drv-dont-use.json"}
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


def base_lists(m: dict, tag: str) -> tuple[list, list, list]:
    """PDK exclusion files (verified against the signoff manifest) -> (sources, synth globs, pnr globs)."""
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
    return sources, synth, pnr


def drv_limited(m: dict, tag: str) -> tuple[dict | None, list]:
    """Owner DRV rule (GF180): the tracked Liberty derivation, recomputed and required identical."""
    if tag not in DRV_DONT_USE:
        return None, []
    import derive_drv_dont_use as drv
    path = ROOT / DRV_DONT_USE[tag]
    fresh = json.dumps(drv.derive(tag, m), indent=1) + "\n"
    if not path.is_file() or path.read_text() != fresh:
        raise SystemExit(f"{path} is missing or stale: rerun scripts/derive_drv_dont_use.py")
    data = json.loads(fresh)
    source = dict(role="owner_drv_limit", path=DRV_DONT_USE[tag],
                  sha256=hashlib.sha256(fresh.encode()).hexdigest(), entries=data["dont_use"],
                  liberty_sha256=data["liberty"]["sha256"], rule=data["rule"])
    return source, data["dont_use"]


def generate(manifest_path: Path, producer_receipt: Path | None) -> dict:
    m = manifest_mod.load(manifest_path)
    pdks = {}
    for tag in ("sky130", "gf180"):
        spec = m["pdks"][tag]
        sources, synth, pnr = base_lists(m, tag)
        drv_source, drv_globs = drv_limited(m, tag)
        if drv_source is not None:
            sources.append(drv_source)
        dont_use = []
        for glob in synth + pnr + RESERVED[tag] + drv_globs:
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
