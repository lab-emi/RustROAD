#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Signoff tool/PDK identity manifest: generator and verifying loader.

``generate`` records podman image ids and RepoDigests, in-container tool paths,
file/tree sha256 and probed versions, and the sha256 + byte size of every PDK
file the RTL-to-GDS flow uses (hashed both inside the pinned image and at the
host overlay path; the two must agree). ``verify`` re-probes all of it and
returns ``verified`` only when every identity matches; any mismatch, missing
image, missing tool or missing file yields ``unverified`` (never a pass).

Host Magic/KLayout/Netgen are recorded as ``not_for_signoff``.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
import container  # noqa: E402

DEFAULT_MANIFEST = ROOT / "fixtures/flow/signoff-pinned.json"
SCHEMA = "rustroad.signoff-manifest.v1"
ORFS_IMAGE = "f152917ebc458bd015042e44d9dac5af69bcae9fb5657d5121cb4d7226ed0c67"
ORFS_OPENROAD = "/OpenROAD-flow-scripts/tools/install/OpenROAD/bin/openroad"
CIEL_HOST = ("/home/cgao/.local/share/containers/storage/overlay/"
             "a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel")
PDK_VERSION = "f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7"
OPENSTA_HOST = ("/home/cgao/.local/share/containers/storage/overlay/"
                "c2cc5a7e2baaa2c6bdac6951f759273edbb27a34c710ea4593e43ef22d00e4b8/diff/foss/tools/openroad/bin/sta")

SKY = "libs.ref/sky130_fd_sc_hd"
GF = "libs.ref/gf180mcu_fd_sc_mcu7t5v0"
PDK_SPEC = {
    "sky130": dict(
        library="sky130_fd_sc_hd", variant="sky130A", family="sky130",
        container_root="/foss/pdks/sky130A",
        host_root=f"{CIEL_HOST}/sky130/versions/{PDK_VERSION}/sky130A",
        gds_dbu_um=0.001, lef_dbu_per_um=1000, boundary_layer=[235, 4], manufacturing_grid_um=0.005,
        corners={"tt": dict(liberty="liberty_tt", rc="rcx_nom", role="typical, reported"),
                 "ss": dict(liberty="liberty_ss", rc="rcx_max", role="setup and hold"),
                 "ff": dict(liberty="liberty_ff", rc="rcx_min", role="hold")},
        files={
            "tech_lef": f"{SKY}/techlef/sky130_fd_sc_hd__nom.tlef",
            "cell_lef": f"{SKY}/lef/sky130_fd_sc_hd.lef",
            "cell_gds": f"{SKY}/gds/sky130_fd_sc_hd.gds",
            "liberty_tt": f"{SKY}/lib/sky130_fd_sc_hd__tt_025C_1v80.lib",
            "liberty_ss": f"{SKY}/lib/sky130_fd_sc_hd__ss_100C_1v60.lib",
            "liberty_ff": f"{SKY}/lib/sky130_fd_sc_hd__ff_n40C_1v95.lib",
            "cell_spice": f"{SKY}/spice/sky130_fd_sc_hd.spice",
            "cell_cdl": f"{SKY}/cdl/sky130_fd_sc_hd.cdl",
            "magicrc": "libs.tech/magic/sky130A.magicrc",
            "magic_tech": "libs.tech/magic/sky130A.tech",
            "magic_tcl": "libs.tech/magic/sky130A.tcl",
            "magic_check_density": "libs.tech/magic/check_density.py",
            "netgen_setup": "libs.tech/netgen/sky130A_setup.tcl",
            "klayout_drc": "libs.tech/klayout/drc/sky130A_mr.drc",
            "klayout_density": "libs.tech/klayout/drc/met_min_ca_density.lydrc",
            "klayout_lvs": "libs.tech/klayout/lvs/sky130.lvs",
            "klayout_map": "libs.tech/klayout/tech/sky130A.map",
            "klayout_lyp": "libs.tech/klayout/tech/sky130A.lyp",
            "klayout_lyt": "libs.tech/klayout/tech/sky130A.lyt",
            "rcx_min": "libs.tech/librelane/rules.openrcx.sky130A.min.calibre",
            "rcx_nom": "libs.tech/librelane/rules.openrcx.sky130A.nom.calibre",
            "rcx_max": "libs.tech/librelane/rules.openrcx.sky130A.max.calibre",
            "synth_exclude": "libs.tech/librelane/sky130_fd_sc_hd/synth_excluded.cells",
            "pnr_exclude": "libs.tech/librelane/sky130_fd_sc_hd/pnr_excluded.cells",
            "librelane_pdk_config": "libs.tech/librelane/config.tcl",
            "librelane_scl_config": "libs.tech/librelane/sky130_fd_sc_hd/config.tcl",
        },
        trees={"ngspice_models": "libs.tech/ngspice"},
    ),
    "gf180": dict(
        library="gf180mcu_fd_sc_mcu7t5v0", variant="gf180mcuD", family="gf180mcu",
        container_root="/foss/pdks/gf180mcuD",
        host_root=f"{CIEL_HOST}/gf180mcu/versions/{PDK_VERSION}/gf180mcuD",
        gds_dbu_um=0.001, lef_dbu_per_um=2000, boundary_layer=[0, 0], manufacturing_grid_um=0.005,
        corners={"tt": dict(liberty="liberty_tt", rc="rcx_nom", role="typical, reported"),
                 "ss": dict(liberty="liberty_ss", rc="rcx_max", role="setup and hold"),
                 "ff": dict(liberty="liberty_ff", rc="rcx_min", role="hold")},
        files={
            "tech_lef": f"{GF}/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef",
            "cell_lef": f"{GF}/lef/gf180mcu_fd_sc_mcu7t5v0.lef",
            "cell_gds": f"{GF}/gds/gf180mcu_fd_sc_mcu7t5v0.gds",
            "liberty_tt": f"{GF}/lib/gf180mcu_fd_sc_mcu7t5v0__tt_025C_3v30.lib",
            "liberty_ss": f"{GF}/lib/gf180mcu_fd_sc_mcu7t5v0__ss_125C_3v00.lib",
            "liberty_ff": f"{GF}/lib/gf180mcu_fd_sc_mcu7t5v0__ff_n40C_3v60.lib",
            "cell_spice": f"{GF}/spice/gf180mcu_fd_sc_mcu7t5v0.spice",
            "cell_cdl": f"{GF}/cdl/gf180mcu_fd_sc_mcu7t5v0.cdl",
            "magicrc": "libs.tech/magic/gf180mcuD.magicrc",
            "magic_tech": "libs.tech/magic/gf180mcuD.tech",
            "magic_tcl": "libs.tech/magic/gf180mcuD.tcl",
            "netgen_setup": "libs.tech/netgen/gf180mcuD_setup.tcl",
            "klayout_drc": "libs.tech/klayout/tech/drc/gf180mcu.drc",
            "klayout_lvs": "libs.tech/klayout/tech/lvs/gf180mcu.lvs",
            "klayout_lvs_runner": "libs.tech/klayout/tech/lvs/run_lvs.py",
            "klayout_map": "libs.tech/klayout/tech/gf180mcu.map",
            "klayout_lyp": "libs.tech/klayout/tech/gf180mcu.lyp",
            "klayout_lyt": "libs.tech/klayout/tech/gf180mcu.lyt",
            "rcx_min": "libs.tech/librelane/rules.openrcx.gf180mcuD.min",
            "rcx_nom": "libs.tech/librelane/rules.openrcx.gf180mcuD.nom",
            "rcx_max": "libs.tech/librelane/rules.openrcx.gf180mcuD.max",
            "synth_exclude": "libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0/synth_exclude.cells",
            "pnr_exclude": "libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0/pnr_exclude.cells",
            "librelane_pdk_config": "libs.tech/librelane/config.tcl",
            "librelane_scl_config": "libs.tech/librelane/gf180mcu_fd_sc_mcu7t5v0/config.tcl",
        },
        trees={"klayout_drc_tree": "libs.tech/klayout/tech/drc",
               "klayout_lvs_tree": "libs.tech/klayout/tech/lvs"},
    ),
}

TOOL_SPEC = {
    "magic": dict(files=["/foss/tools/bin/magic", "/foss/tools/magic/bin/magic",
                         "/foss/tools/magic/lib/magic/tcl/magicexec",
                         "/foss/tools/magic/lib/magic/tcl/magicdnull",
                         "/foss/tools/magic/lib/magic/tcl/tclmagic.so"],
                  trees=["/foss/tools/magic/lib/magic"],
                  version=["/foss/tools/bin/magic", "--version"], expect=r"^\s*8\.3\.678\s*$"),
    "netgen": dict(files=["/foss/tools/bin/netgen", "/foss/tools/netgen/bin/netgen",
                          "/foss/tools/netgen/lib/netgen/tcl/netgenexec",
                          "/foss/tools/netgen/lib/netgen/tcl/tclnetgen.so"],
                   trees=["/foss/tools/netgen/lib/netgen"],
                   version=["/foss/tools/bin/netgen", "-batch", "quit"], expect=r"Netgen 1\.5\.323 "),
    "klayout": dict(files=["/foss/tools/klayout/klayout"], trees=["/foss/tools/klayout"],
                    version=["/foss/tools/klayout/klayout", "-v"], expect=r"^KLayout 0\.30\.9\s*$"),
    "strmxor": dict(files=["/foss/tools/klayout/strmxor"], trees=[],
                    version=["/foss/tools/klayout/strmxor", "--version"], expect=r"^0\.30\.9 "),
    "python3": dict(files=["/usr/bin/python3.12",
                           "/usr/local/lib/python3.12/dist-packages/docopt.py"],
                    trees=["/usr/local/lib/python3.12/dist-packages/klayout"],
                    version=["/usr/bin/python3", "-c",
                             "import sys,docopt,klayout.db as k;print(sys.version.split()[0],docopt.__version__,k.__version__)"],
                    expect=r"^3\.12\.3 0\.6\.2 0\.30\.9"),
    "coreutils": dict(files=["/usr/bin/time", "/usr/bin/timeout", "/usr/bin/sha256sum"], trees=[],
                      version=["/usr/bin/timeout", "--version"], expect=r"timeout \(GNU coreutils\) 9\.4"),
}
NOT_FOR_SIGNOFF = [
    dict(id="magic", path="/usr/bin/magic", version="8.3.105",
         reason="PDK tech files require magic-8.3.411 or newer; host Magic segfaults (rc=139) loading sky130A.tech/gf180mcuD.tech"),
    dict(id="klayout", path="/usr/bin/klayout", version="KLayout 0.30.0",
         reason="fails the gf180mcu.drc deck (deep: 'Heap lost: DeepShapeStore'; flat: no netlist extractor)"),
    dict(id="netgen", path="/usr/lib/netgen/bin/netgen", version="Netgen 1.5.133",
         reason="off-PATH wrapper; different report format; not the pinned 1.5.323"),
]


def sha256_file(path: Path) -> tuple[str, int]:
    h = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            h.update(block)
            size += len(block)
    return h.hexdigest(), size


def tree_digest(root: Path) -> tuple[str, int]:
    entries = []
    for base, dirs, files in os.walk(root):
        dirs.sort()
        for name in sorted(files):
            full = Path(base) / name
            if full.is_symlink() or not full.is_file():
                continue
            entries.append((str(full.relative_to(root)), sha256_file(full)[0]))
    h = hashlib.sha256()
    for rel, sha in sorted(entries):
        h.update(f"{rel}\0{sha}\n".encode())
    return h.hexdigest(), len(entries)


def probe_request(pdks, tools) -> dict:
    files, trees, versions = [], [], {}
    for name in tools:
        spec = TOOL_SPEC[name]
        files += spec["files"]
        trees += spec["trees"]
        versions[name] = spec["version"]
    for tag in pdks:
        spec = PDK_SPEC[tag]
        files += [f"{spec['container_root']}/{rel}" for rel in spec["files"].values()]
        trees += [f"{spec['container_root']}/{rel}" for rel in spec["trees"].values()]
    return dict(files=sorted(set(files)), trees=sorted(set(trees)), versions=versions)


def run_probe(request: dict, out_dir: Path, label: str, image: str = container.SIGNOFF_IMAGE) -> tuple[dict | None, dict]:
    """Run identity_probe.py inside ``image``; return (probe result or None, receipt)."""
    out_dir.mkdir(parents=True, exist_ok=True)
    req = out_dir / f"{label}.request.json"
    req.write_text(json.dumps(request, indent=1) + "\n")
    receipt = container.run(["/usr/bin/python3", "/scripts/identity_probe.py",
                             f"/out/{req.name}", f"/out/{label}.result.json"],
                            out_dir=out_dir, label=label, image=image,
                            ro=[(str(HERE), "/scripts")], memory="1g", cpus=2, timeout=300)
    result_path = out_dir / f"{label}.result.json"
    if receipt["exit_code"] != 0 or not result_path.is_file():
        return None, receipt
    return json.loads(result_path.read_text()), receipt


def generate(path: Path, work: Path) -> dict:
    images = {}
    for key, image in (("iic_osic_tools", container.SIGNOFF_IMAGE), ("orfs", ORFS_IMAGE)):
        ident = container.image_identity(image)
        if not ident.get("present"):
            raise SystemExit(f"image {image} not present: {ident}")
        images[key] = dict(id=ident["id"], repo_digests=ident["repo_digests"],
                           repo_tags=ident["repo_tags"], created=ident["created"], env=ident["env"])
    request = probe_request(list(PDK_SPEC), list(TOOL_SPEC))
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S")
    probe, receipt = run_probe(request, work, f"manifest_probe_{stamp}")
    if probe is None:
        raise SystemExit(f"identity probe failed: {receipt['stderr']}")
    tools = {}
    for name, spec in TOOL_SPEC.items():
        version = probe["versions"][name]
        if not re.search(spec["expect"], version["output"], re.M):
            raise SystemExit(f"{name} version probe {version!r} does not match {spec['expect']}")
        tools[name] = dict(image="iic_osic_tools", version_command=spec["version"],
                           version_regex=spec["expect"],
                           version_output=version["output"].strip().splitlines()[0],
                           files={f: {k: probe["files"][f][k] for k in ("sha256", "bytes", "realpath")}
                                  for f in spec["files"]},
                           trees={t: probe["trees"][t] for t in spec["trees"]})
    orfs, orfs_receipt = run_probe(dict(files=[ORFS_OPENROAD], trees=[], versions={}), work,
                                   f"manifest_orfs_{stamp}", image=ORFS_IMAGE)
    if orfs is None:
        # The orfs image has python3 under a different path on some builds; fall back to sha256sum.
        raise SystemExit(f"orfs identity probe failed: {orfs_receipt['stderr']}")
    tools["openroad"] = dict(image="orfs", files={ORFS_OPENROAD: orfs["files"][ORFS_OPENROAD]},
                             trees={}, version_output="unknown (OpenROAD -version prints 'unknown'; pinned by image id and sha256)")
    sha, size = sha256_file(Path(OPENSTA_HOST))
    tools["opensta"] = dict(host=True, path=OPENSTA_HOST, sha256=sha, bytes=size, version="3.1.0",
                            note="host overlay path of the OpenSTA binary used by the timing harness")
    pdks = {}
    for tag, spec in PDK_SPEC.items():
        files = {}
        for key, rel in spec["files"].items():
            cpath = f"{spec['container_root']}/{rel}"
            probed = probe["files"][cpath]
            if probed.get("missing"):
                raise SystemExit(f"PDK file missing in image: {cpath}")
            hsha, hsize = sha256_file(Path(spec["host_root"]) / rel)
            if (hsha, hsize) != (probed["sha256"], probed["bytes"]):
                raise SystemExit(f"host/image PDK mismatch for {rel}")
            files[key] = dict(path=rel, sha256=hsha, bytes=hsize)
        trees = {}
        for key, rel in spec["trees"].items():
            cpath = f"{spec['container_root']}/{rel}"
            probed = probe["trees"][cpath]
            hsha, count = tree_digest(Path(spec["host_root"]) / rel)
            if (hsha, count) != (probed["sha256"], probed["files"]):
                raise SystemExit(f"host/image PDK tree mismatch for {rel}")
            trees[key] = dict(path=rel, sha256=hsha, files=count)
        pdks[tag] = {k: v for k, v in spec.items() if k not in ("files", "trees")}
        pdks[tag].update(distribution="Ciel/open_pdks", version=PDK_VERSION, files=files, trees=trees)
    host_tools = []
    for entry in NOT_FOR_SIGNOFF:
        item = dict(entry, not_for_signoff=True)
        p = Path(entry["path"])
        if p.is_file():
            item["sha256"], item["bytes"] = sha256_file(p)
        else:
            item["missing"] = True
        host_tools.append(item)
    manifest = dict(schema=SCHEMA, generated_utc=stamp, generator="scripts/signoff/manifest.py generate",
                    policy=dict(verdict_rule="any identity mismatch, missing image/tool/file => unverified",
                                tool_execution="only inside the pinned iic_osic_tools image via scripts/signoff/container.py",
                                clean_room="decks and tools are invoked as black boxes; GPLv3 sky130A_mr.drc is invoked, never copied"),
                    images=images, tools=tools, not_for_signoff=host_tools, pdks=pdks)
    path.write_text(json.dumps(manifest, indent=2, sort_keys=False) + "\n")
    return manifest


def load(path: Path = DEFAULT_MANIFEST) -> dict:
    data = json.loads(path.read_text())
    if data.get("schema") != SCHEMA:
        raise ValueError(f"{path}: wrong manifest schema {data.get('schema')!r}")
    return data


EXTRA_TOOLS = {"openroad": dict(image="orfs", files=[ORFS_OPENROAD]),
               "opensta": dict(host=True, path=OPENSTA_HOST)}


def structure_mismatches(manifest, pdks: list[str], tools: list[str]) -> list[str]:
    """Every entry the harness relies on must be present and equal to the generator's spec.

    A manifest that lacks a required file/tree/tool entry, or whose paths, roots
    or images differ from what the harness runs, cannot vouch for the run: the
    caller turns any problem listed here into ``unverified``.
    """
    out: list[str] = []
    if not isinstance(manifest, dict):
        return ["manifest is not a JSON object"]
    images = manifest.get("images")
    if not isinstance(images, dict):
        return ["manifest has no images table"]
    for key, want in (("iic_osic_tools", container.SIGNOFF_IMAGE), ("orfs", ORFS_IMAGE)):
        got = images.get(key)
        if not isinstance(got, dict) or got.get("id") != want:
            out.append(f"image {key} entry missing or not the image the harness runs ({want[:12]})")
        elif not isinstance(got.get("repo_digests"), list):
            out.append(f"image {key} has no repo_digests list")
    table = manifest.get("tools")
    if not isinstance(table, dict):
        return out + ["manifest has no tools table"]
    for name in tools:
        entry = table.get(name)
        if not isinstance(entry, dict):
            out.append(f"tool {name} entry missing")
            continue
        if name in TOOL_SPEC:
            spec = TOOL_SPEC[name]
            if entry.get("image") != "iic_osic_tools":
                out.append(f"tool {name} is not pinned to the iic_osic_tools image")
            files, trees = entry.get("files"), entry.get("trees")
            if not isinstance(files, dict) or not isinstance(trees, dict):
                out.append(f"tool {name} files/trees tables missing")
                continue
            out += [f"tool {name} file entry {f} missing" for f in spec["files"]
                    if not isinstance(files.get(f), dict) or "sha256" not in files[f] or "bytes" not in files[f]]
            out += [f"tool {name} tree entry {t} missing" for t in spec["trees"]
                    if not isinstance(trees.get(t), dict) or "sha256" not in trees[t] or "files" not in trees[t]]
            if entry.get("version_command") != spec["version"] or entry.get("version_regex") != spec["expect"]:
                out.append(f"tool {name} version probe differs from the harness spec")
        elif name in EXTRA_TOOLS:
            spec = EXTRA_TOOLS[name]
            if spec.get("host"):
                if entry.get("path") != spec["path"] or not entry.get("sha256"):
                    out.append(f"host tool {name} path/sha256 entry missing or changed")
            elif entry.get("image") != spec["image"] or any(
                    not isinstance((entry.get("files") or {}).get(f), dict) or
                    not (entry.get("files") or {})[f].get("sha256") for f in spec["files"]):
                out.append(f"tool {name} image/file entries missing")
        else:
            out.append(f"tool {name} is not a known signoff tool")
    pdk_table = manifest.get("pdks")
    if not isinstance(pdk_table, dict):
        return out + ["manifest has no pdks table"]
    for tag in pdks:
        spec, entry = PDK_SPEC.get(tag), pdk_table.get(tag)
        if spec is None:
            out.append(f"pdk {tag} is not a known PDK")
            continue
        if not isinstance(entry, dict):
            out.append(f"pdk {tag} not in manifest")
            continue
        for key, want in spec.items():
            if key in ("files", "trees"):
                continue
            if entry.get(key) != want:
                out.append(f"pdk {tag} {key} missing or differs from the harness spec")
        for table_key, check_field in (("files", "bytes"), ("trees", "files")):
            have = entry.get(table_key)
            if not isinstance(have, dict):
                out.append(f"pdk {tag} {table_key} table missing")
                continue
            for key, rel in spec[table_key].items():
                got = have.get(key)
                if not isinstance(got, dict) or got.get("path") != rel or not got.get("sha256") or check_field not in got:
                    out.append(f"pdk {tag} {table_key[:-1]} entry {key} ({rel}) missing or changed")
    return out


def verify(path: Path, out_dir: Path, pdks: list[str], tools: list[str], label: str = "identity",
           host_files: bool = True) -> dict:
    """Re-verify identities. Never raises for identity problems: returns status=unverified."""
    mismatches: list[str] = []
    receipts = []
    try:
        manifest = load(path)
        manifest_sha = sha256_file(path)[0]
    except (OSError, ValueError, json.JSONDecodeError) as error:
        return dict(status="unverified", mismatches=[f"manifest unreadable: {error}"], receipts=[])
    structural = structure_mismatches(manifest, pdks, tools)
    if structural:
        # Nothing below can be trusted (or even indexed) without the required entries.
        return dict(status="unverified", manifest=str(path), manifest_sha256=manifest_sha, pdks=pdks, tools=tools,
                    mismatches=["manifest incomplete: " + m for m in structural], receipts=[],
                    checked=dict(files=0, trees=0, versions=0))
    images_needed = {"iic_osic_tools"} | {manifest["tools"][t].get("image") for t in tools
                                          if t in manifest["tools"] and manifest["tools"][t].get("image")}
    for key in sorted(images_needed):
        pinned = manifest["images"].get(key)
        if pinned is None:
            mismatches.append(f"image {key} not in manifest")
            continue
        ident = container.image_identity(pinned["id"])
        if not ident.get("present"):
            mismatches.append(f"image {key} {pinned['id'][:12]} missing: {ident.get('error')}")
        elif ident["id"] != pinned["id"] or not set(pinned["repo_digests"]) <= set(ident["repo_digests"]):
            mismatches.append(f"image {key} identity changed: {ident['id']} {ident['repo_digests']}")
    unknown = [t for t in tools if t not in manifest["tools"]]
    mismatches += [f"tool {t} not in manifest" for t in unknown]
    iic_tools = [t for t in tools if t in manifest["tools"] and manifest["tools"][t].get("image") == "iic_osic_tools"]
    request = dict(files=[], trees=[], versions={})
    for t in iic_tools:
        entry = manifest["tools"][t]
        request["files"] += list(entry["files"])
        request["trees"] += list(entry["trees"])
        request["versions"][t] = entry["version_command"]
    for tag in pdks:
        spec = manifest["pdks"].get(tag)
        if spec is None:
            mismatches.append(f"pdk {tag} not in manifest")
            continue
        request["files"] += [f"{spec['container_root']}/{f['path']}" for f in spec["files"].values()]
        request["trees"] += [f"{spec['container_root']}/{t['path']}" for t in spec["trees"].values()]
    probe = None
    if not any(m.startswith("image iic_osic_tools") for m in mismatches):
        probe, receipt = run_probe(request, out_dir, label)
        receipts.append(receipt)
        if probe is None:
            mismatches.append("in-container identity probe failed")
    if probe is not None:
        for t in iic_tools:
            entry = manifest["tools"][t]
            for f, want in entry["files"].items():
                got = probe["files"].get(f, {})
                if got.get("missing") or got.get("sha256") != want["sha256"] or got.get("bytes") != want["bytes"]:
                    mismatches.append(f"tool {t} file {f} changed or missing")
            for d, want in entry["trees"].items():
                got = probe["trees"].get(d, {})
                if got.get("missing") or got.get("sha256") != want["sha256"] or got.get("files") != want["files"]:
                    mismatches.append(f"tool {t} tree {d} changed or missing")
            out = probe["versions"].get(t, {}).get("output", "")
            if not re.search(entry["version_regex"], out, re.M):
                mismatches.append(f"tool {t} version probe mismatch: {out[:200]!r}")
        for tag in pdks:
            spec = manifest["pdks"].get(tag)
            if spec is None:
                continue
            for key, want in spec["files"].items():
                got = probe["files"].get(f"{spec['container_root']}/{want['path']}", {})
                if got.get("missing") or got.get("sha256") != want["sha256"] or got.get("bytes") != want["bytes"]:
                    mismatches.append(f"pdk {tag} {key} ({want['path']}) changed or missing in image")
            for key, want in spec["trees"].items():
                got = probe["trees"].get(f"{spec['container_root']}/{want['path']}", {})
                if got.get("missing") or got.get("sha256") != want["sha256"] or got.get("files") != want["files"]:
                    mismatches.append(f"pdk {tag} tree {key} changed or missing in image")
    orfs_tools = [t for t in tools if t in manifest["tools"] and manifest["tools"][t].get("image") == "orfs"]
    if orfs_tools and not any(m.startswith("image orfs") for m in mismatches):
        files = sorted({f for t in orfs_tools for f in manifest["tools"][t]["files"]})
        orfs, receipt = run_probe(dict(files=files, trees=[], versions={}), out_dir, label + "_orfs",
                                  image=manifest["images"]["orfs"]["id"])
        receipts.append(receipt)
        if orfs is None:
            mismatches.append("orfs identity probe failed")
        else:
            for t in orfs_tools:
                for f, want in manifest["tools"][t]["files"].items():
                    got = orfs["files"].get(f, {})
                    if got.get("missing") or got.get("sha256") != want["sha256"]:
                        mismatches.append(f"tool {t} file {f} changed or missing")
    if host_files:
        for tag in pdks:
            spec = manifest["pdks"].get(tag)
            if spec is None:
                continue
            for key, want in spec["files"].items():
                p = Path(spec["host_root"]) / want["path"]
                if not p.is_file() or sha256_file(p) != (want["sha256"], want["bytes"]):
                    mismatches.append(f"pdk {tag} {key} host copy changed or missing: {p}")
    for t in tools:
        entry = manifest["tools"].get(t, {})
        if entry.get("host"):
            p = Path(entry["path"])
            if not p.is_file() or sha256_file(p)[0] != entry["sha256"]:
                mismatches.append(f"host tool {t} changed or missing: {p}")
    return dict(status="verified" if not mismatches else "unverified", manifest=str(path),
                manifest_sha256=manifest_sha, pdks=pdks, tools=tools, mismatches=mismatches,
                checked=dict(files=len(request["files"]), trees=len(request["trees"]),
                             versions=len(request["versions"])),
                receipts=[dict(label=r["label"], exit_code=r["exit_code"], wall_seconds=r["wall_seconds"],
                               peak_rss_kib=r["peak_rss_kib"]) for r in receipts])


def pdk_container_path(manifest: dict, tag: str, key: str) -> str:
    spec = manifest["pdks"][tag]
    return f"{spec['container_root']}/{spec['files'][key]['path']}"


def pdk_host_path(manifest: dict, tag: str, key: str) -> Path:
    spec = manifest["pdks"][tag]
    return Path(spec["host_root"]) / spec["files"][key]["path"]


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="action", required=True)
    g = sub.add_parser("generate")
    g.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    g.add_argument("--work", type=Path, required=True)
    v = sub.add_parser("verify")
    v.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    v.add_argument("--out", type=Path, required=True)
    v.add_argument("--pdk", action="append", choices=sorted(PDK_SPEC), default=None)
    v.add_argument("--tool", action="append", default=None)
    args = parser.parse_args(argv)
    if args.action == "generate":
        if shutil.which("podman") is None:
            raise SystemExit("podman missing")
        m = generate(args.manifest, args.work.resolve())
        print(json.dumps(dict(manifest=str(args.manifest), images=list(m["images"]),
                              tools=list(m["tools"]), pdks=list(m["pdks"]))))
        return 0
    result = verify(args.manifest, args.out.resolve(), args.pdk or sorted(PDK_SPEC),
                    args.tool or sorted(TOOL_SPEC) + ["openroad", "opensta"])
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "identity.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(dict(status=result["status"], mismatches=result["mismatches"])))
    return 0 if result["status"] == "verified" else 3


if __name__ == "__main__":
    sys.exit(main())
