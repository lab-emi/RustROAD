#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Black-box reference DEF -> GDS streamout (KLayout LEF/DEF reader, pinned image).

Used for signoff baselines and as the XOR oracle for RustROAD's native GDS
writer. Every standard-cell master is resolved to its structure in the PDK
cell GDS (macro_resolution_mode=2, never LEF geometry); the output keeps the
hierarchy (SREFs), uses a 0.001 um database unit and no timestamps. For a
2000-DBU/um DEF (GF180) every DEF coordinate must be even, otherwise the
0.001 um conversion would round silently and the run is refused.

usage: def2gds_reference.py --def ROUTED.def --pdk sky130|gf180 --out OUT.gds
           [--manifest fixtures/flow/signoff-pinned.json] [--timeout 600]
Writes OUT.gds and OUT.gds.json (receipt with identities, checks, resources).
Exit 0 only when the identity check and every output assertion pass.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import container  # noqa: E402
import manifest as manifest_mod  # noqa: E402
import parsers  # noqa: E402


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fingerprint(path: Path) -> dict:
    return dict(path=str(path.resolve()), sha256=sha256(path), bytes=path.stat().st_size)


def def_summary(text: str) -> dict:
    design = re.search(r"^\s*DESIGN\s+(\S+)\s*;", text, re.M)
    units = re.search(r"^\s*UNITS\s+DISTANCE\s+MICRONS\s+(\d+)\s*;", text, re.M)
    comps = re.search(r"^COMPONENTS\s+\d+\s*;(.*?)^END COMPONENTS", text, re.M | re.S)
    masters = sorted(set(re.findall(r"^\s*-\s+\S+\s+(\S+)", comps[1], re.M))) if comps else []
    return dict(design=design[1] if design else None, units=int(units[1]) if units else None,
                masters=masters)


def stream(def_path: Path, pdk: str, out: Path, manifest_path: Path, timeout: int = 600) -> dict:
    out = out.resolve()
    work = out.parent / (out.name + ".work")
    work.mkdir(parents=True, exist_ok=True)
    m = manifest_mod.load(manifest_path)
    spec = m["pdks"][pdk]
    identity = manifest_mod.verify(manifest_path, work, [pdk], ["klayout", "coreutils"],
                                   label=container.next_label(work, "identity"))
    receipt = dict(schema="rustroad.def2gds-reference.v1", pdk=pdk, input=fingerprint(def_path),
                   output=str(out), identity=dict(status=identity["status"], mismatches=identity["mismatches"],
                                                  manifest=str(manifest_path), manifest_sha256=identity.get("manifest_sha256")),
                   tool="KLayout 0.30.9 LEF/DEF reader (black box) in image " + container.SIGNOFF_IMAGE[:12],
                   status="unverified", problems=[])
    text = def_path.read_text(errors="replace")
    summary = def_summary(text)
    receipt["def"] = summary
    problems = receipt["problems"]
    if summary["units"] != spec["lef_dbu_per_um"]:
        problems.append(f"DEF UNITS {summary['units']} != PDK LEF DBU {spec['lef_dbu_per_um']}")
    if summary["units"] and summary["units"] != 1000:
        odd = parsers.count_def_odd_coordinates(text)
        receipt["odd_coordinate_check"] = odd
        factor = summary["units"] // 1000
        if summary["units"] % 1000 or factor != 2:
            problems.append(f"unsupported DEF units {summary['units']} for a 0.001 um GDS")
        elif odd["odd"]:
            problems.append(f"{odd['odd']} odd DEF coordinates cannot be represented at 0.001 um")
    if identity["status"] != "verified":
        problems.append("tool/PDK identity unverified")
    if problems:
        out.with_suffix(out.suffix + ".json").write_text(json.dumps(receipt, indent=2) + "\n")
        return receipt
    cpath = lambda key: manifest_mod.pdk_container_path(m, pdk, key)  # noqa: E731
    report = work / "streamout.json"
    if out.exists():
        out.unlink()
    label = container.next_label(out.parent, f"def2gds_{pdk}")
    run = container.run(["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_def2gds.py",
                         "-rd", "def_path=/in/design.def", "-rd", f"tlef={cpath('tech_lef')}",
                         "-rd", f"clef={cpath('cell_lef')}", "-rd", f"cgds={cpath('cell_gds')}",
                         "-rd", f"map_file={cpath('klayout_map')}", "-rd", f"out=/out/{out.name}",
                         "-rd", f"report=/out/{work.name}/{report.name}"],
                        out_dir=out.parent, label=label, ro=[(str(HERE), "/scripts"),
                                                             (str(def_path.resolve()), "/in/design.def")],
                        memory="4g", cpus=2, threads=2, timeout=timeout)
    receipt["resources"] = dict(wall_seconds=run["wall_seconds"], peak_rss_kib=run["peak_rss_kib"],
                                receipt=str(Path(run["stdout"]).parent / "receipt.json"))
    log = container.read_capture(run["stdout"]) + container.read_capture(run["stderr"])
    if "KL_DEF2GDS_DONE" not in log or not out.is_file() or not report.is_file():
        problems.append("streamout did not complete (missing marker/output/report)")
    else:
        info = json.loads(report.read_text())
        receipt["layout"] = info
        if info["top"] != summary["design"]:
            problems.append(f"top cell {info['top']} != DEF DESIGN {summary['design']}")
        if abs(info["dbu_um"] - spec["gds_dbu_um"]) > 1e-12:
            problems.append(f"GDS dbu {info['dbu_um']} != {spec['gds_dbu_um']}")
        unresolved = sorted(set(summary["masters"]) - set(info["pdk_cells_resolved"]))
        if unresolved:
            problems.append(f"masters not resolved to PDK GDS structures: {unresolved}")
        bl = "%d/%d" % tuple(spec["boundary_layer"])
        if not info["layer_shape_counts"].get(bl):
            problems.append(f"PR boundary layer {bl} absent")
        warnings = [line for line in log.splitlines() if re.search(r"\b(ERROR|Error)\b", line)]
        if warnings:
            problems.append(f"tool errors in log: {warnings[:5]}")
        receipt["output"] = fingerprint(out)
    receipt["status"] = "passed" if not problems else "failed"
    out.with_suffix(out.suffix + ".json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--def", dest="def_path", type=Path, required=True)
    parser.add_argument("--pdk", choices=("sky130", "gf180"), required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, default=manifest_mod.DEFAULT_MANIFEST)
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args(argv)
    receipt = stream(args.def_path.resolve(), args.pdk, args.out, args.manifest.resolve(), args.timeout)
    print(json.dumps(dict(status=receipt["status"], problems=receipt["problems"],
                          output=receipt.get("output"), receipt=str(args.out.resolve()) + ".json")))
    return {"passed": 0, "failed": 1}.get(receipt["status"], 3)


if __name__ == "__main__":
    sys.exit(main())
