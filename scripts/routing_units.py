# SPDX-License-Identifier: Apache-2.0
"""Routing-profile unit checks against the technology LEF (pure Python).

The subset geometry checker grows routed shapes by the profile spacing in
DEF database units. A profile written in the wrong units (for example GF180
values in 1000-DBU/um "nanometres" while the LEF uses 2000 DBU/um) silently
checks only part of the rule. ``assert_profile_units`` rejects every profile
spacing that is smaller than the LEF minimum spacing converted to DBU.
"""
from __future__ import annotations

import hashlib
from pathlib import Path
import re

MAX_LEF_BYTES = 16 * 1024 * 1024


class UnitsError(ValueError):
    """Profile spacing inconsistent with the technology LEF."""


def _strip(text: str) -> str:
    return re.sub(r"#[^\n]*", "", text)


def lef_rules(text: str) -> dict:
    """Return dbu_per_um, per-layer type/min spacing (um) and per-VIA layer lists."""
    text = _strip(text)
    m = re.search(r"\bDATABASE\s+MICRONS\s+(\d+)\s*;", text)
    if m is None:
        raise UnitsError("technology LEF has no UNITS DATABASE MICRONS")
    layers = {}
    for lm in re.finditer(r"^\s*LAYER\s+(\S+)\s*\n(.*?)^\s*END\s+\1\s*$", text, re.M | re.S):
        name, body = lm[1], lm[2]
        kind = re.search(r"^\s*TYPE\s+(\w+)", body, re.M)
        values = [float(v) for v in re.findall(r"^\s*SPACING\s+([0-9.]+)\s*;", body, re.M)]
        spacing = min(values) if values else None
        source = "SPACING" if values else None
        if spacing is None:
            table = re.search(r"SPACINGTABLE\s+PARALLELRUNLENGTH[^;]*?WIDTH\s+0(?:\.0*)?\s+([0-9.]+)", body, re.S)
            if table:
                spacing, source = float(table[1]), "SPACINGTABLE WIDTH 0"
        layers[name] = dict(type=kind[1] if kind else None, spacing_um=spacing, source=source)
    vias = {}
    for vm in re.finditer(r"^\s*VIA\s+(\S+)(?:\s+DEFAULT)?\s*\n(.*?)^\s*END\s+\1\s*$", text, re.M | re.S):
        vias[vm[1]] = re.findall(r"^\s*LAYER\s+(\S+)\s*;", vm[2], re.M)
    return dict(dbu_per_um=int(m[1]), layers=layers, vias=vias)


def sha256(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def assert_profile_units(config: dict, lef_path: str, lef_sha256: str | None = None) -> dict:
    """Raise UnitsError when any profile spacing is below the LEF rule in DBU."""
    path = Path(lef_path)
    if not path.is_file() or path.stat().st_size > MAX_LEF_BYTES:
        raise UnitsError(f"technology LEF missing or too large: {lef_path}")
    digest = sha256(path)
    if lef_sha256 is not None and digest != lef_sha256:
        raise UnitsError(f"technology LEF identity changed: {digest} != {lef_sha256}")
    rules = lef_rules(path.read_text(errors="replace"))
    dbu = rules["dbu_per_um"]
    problems, checked = [], []
    for entry in config.get("layers", []):
        rule = rules["layers"].get(entry["name"])
        if rule is None or rule["spacing_um"] is None:
            problems.append(f"layer {entry['name']}: no LEF minimum spacing to check the profile against")
            continue
        need = round(rule["spacing_um"] * dbu)
        checked.append(dict(layer=entry["name"], profile_dbu=entry["spacing"], lef_um=rule["spacing_um"],
                            lef_dbu=need, source=rule["source"]))
        if entry["spacing"] < need:
            problems.append(
                f"layer {entry['name']}: profile spacing {entry['spacing']} DBU < LEF {rule['source']} "
                f"{rule['spacing_um']} um x {dbu} DBU/um = {need} DBU (profile value is "
                f"{entry['spacing'] / dbu:g} um at {dbu} DBU/um; wrong units?)")
    for entry in config.get("vias", []):
        layers = rules["vias"].get(entry["name"])
        if layers is None:
            problems.append(f"via {entry['name']}: not defined in the technology LEF")
            continue
        cuts = [l for l in layers if (rules["layers"].get(l) or {}).get("type") == "CUT"]
        spacings = [rules["layers"][c]["spacing_um"] for c in cuts if rules["layers"][c]["spacing_um"] is not None]
        if not spacings:
            problems.append(f"via {entry['name']}: no LEF cut spacing")
            continue
        need = max(round(s * dbu) for s in spacings)
        checked.append(dict(via=entry["name"], cut_layers=cuts, profile_dbu=entry["cut_spacing"], lef_dbu=need))
        if entry["cut_spacing"] < need:
            problems.append(f"via {entry['name']}: profile cut_spacing {entry['cut_spacing']} DBU < LEF cut "
                            f"SPACING {max(spacings)} um x {dbu} DBU/um = {need} DBU (wrong units?)")
    if problems:
        raise UnitsError("routing profile units: " + "; ".join(problems))
    return dict(status="passed", dbu_per_um=dbu, lef=str(path), lef_sha256=digest, checked=checked)
