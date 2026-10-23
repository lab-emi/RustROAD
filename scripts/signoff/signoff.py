#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Block-level foundry signoff harness (black-box tools in the pinned image).

usage: signoff.py CHECK --gds LAYOUT.gds --top CELL --pdk sky130|gf180 --out DIR
                  [--netlist POWERED.v] [--against OTHER.gds [--against-top CELL]]
                  [--manifest fixtures/flow/signoff-pinned.json] [--threads 4]
                  [--memory 4g] [--timeout 900]
CHECK: drc-magic | drc-klayout | antenna | density | lvs-netgen | lvs-klayout | xor | all

Writes DIR/<check>.json (one verdict per check, schema rustroad.signoff-verdict.v1)
and, for ``all``, DIR/signoff.json. Status is passed / failed / unverified and is
derived only from parsed result files plus preflight assertions, never from tool
exit codes (several tools exit 0 on failure). Rule counts are taken across ALL
cells of the hierarchy. Density is reported separately and is never folded into
the block DRC verdict. Exit: 0 passed, 1 failed, 3 unverified.

Scope: block/macro level only (no pad ring, no seal ring). IR/EM/SI/OCV/CPPR are
outside this harness and must be reported as unchecked by the caller.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import sys
import time
import traceback

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import container  # noqa: E402
import manifest as manifest_mod  # noqa: E402
import parsers  # noqa: E402
import refnet  # noqa: E402

SCHEMA = "rustroad.signoff-verdict.v1"
CHECKS = ("drc-magic", "drc-klayout", "antenna", "density", "lvs-netgen", "lvs-klayout", "xor")
TOOLS = {"drc-magic": ["magic"], "drc-klayout": ["klayout"], "antenna": ["magic", "klayout"],
         "density": ["magic", "klayout", "python3"], "lvs-netgen": ["magic", "netgen"],
         "lvs-klayout": ["klayout", "python3"], "xor": ["strmxor", "klayout"]}
SCOPE = dict(level="block/macro", pad_ring="not_applicable", seal_ring="not_applicable",
             density="reported separately (density_status); not part of the block DRC verdict",
             ir_drop="unchecked", electromigration="unchecked", signal_integrity="unchecked",
             ocv="unchecked", cppr="unchecked")
# Body/substrate terminals the PDK KLayout LVS decks synthesize on library cells
# (layout name, schematic name). Only these named child-cell pin pairs may differ.
LVS_BODY_PINS = {"sky130": (("sky130_gnd", "VNB"),), "gf180": (("SUB", "VPW"),)}
# Signoff deck options pinned here and echoed into every verdict.
KLAYOUT_DRC_OPTIONS = {
    "sky130": dict(feol="true", beol="true", offgrid="true", floating_met="false", seal="false"),
    "gf180": dict(variant="gf180mcuD", decks="all,-antenna,-density", run_mode="deep"),
}


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fingerprint(path: Path | None) -> dict | None:
    if path is None:
        return None
    path = Path(path)
    if not path.is_file():
        return dict(path=str(path), missing=True)
    return dict(path=str(path.resolve()), sha256=sha256(path), bytes=path.stat().st_size)


class Verdict:
    def __init__(self, check: str, session: "Session"):
        self.check, self.s = check, session
        self.failures: list[str] = []
        self.unverified: list[str] = []
        self.data: dict = dict(counts=None, result={}, reports={}, options={}, runs=[])
        self.started = time.monotonic()

    def fail(self, why: str):
        self.failures.append(why)

    def unknown(self, why: str):
        self.unverified.append(why)

    def run(self, receipt: dict):
        self.data["runs"].append(dict(label=receipt["label"], exit_code=receipt["exit_code"],
                                      timed_out=receipt["timed_out"], wall_seconds=receipt["wall_seconds"],
                                      peak_rss_kib=receipt["peak_rss_kib"],
                                      receipt=str(Path(receipt["stdout"]).parent / "receipt.json")))
        reason = run_incomplete(receipt)
        if reason:
            self.unknown(f"{receipt['label']}: {reason}")
        if receipt["peak_rss_kib"] is None:
            self.unknown(f"{receipt['label']}: no peak-RSS receipt")
        return receipt

    @property
    def status(self) -> str:
        if self.failures:
            return "failed"
        if self.unverified:
            return "unverified"
        return "passed"

    def finish(self) -> dict:
        s = self.s
        runs = self.data["runs"]
        rss = [r["peak_rss_kib"] for r in runs if r["peak_rss_kib"] is not None]
        # Rule 4: a count backed by incomplete evidence is never reported (as 0 or otherwise).
        self.data["counts"] = definitive_counts(self.data["counts"], self.unverified)
        tools = {}
        for t in TOOLS.get(self.check, []):
            entry = ((s.m or {}).get("tools") or {}).get(t)
            if isinstance(entry, dict):
                tools[t] = dict(version=entry.get("version_output"),
                                files={k: (v or {}).get("sha256") for k, v in (entry.get("files") or {}).items()})
        out = dict(schema=SCHEMA, check=self.check, pdk=s.pdk, status=self.status,
                   failures=self.failures, unverified_reasons=self.unverified,
                   input=dict(gds=fingerprint(s.gds), top_cell=s.top,
                              reference_netlist=fingerprint(s.netlist),
                              against=fingerprint(s.against), against_top=s.against_top),
                   preflight=s.preflight_summary(),
                   identity=dict(status=s.identity["status"], manifest=str(s.manifest_path),
                                 manifest_sha256=s.identity.get("manifest_sha256"),
                                 mismatches=s.identity["mismatches"], image=container.SIGNOFF_IMAGE),
                   tools=tools,
                   scope=SCOPE, **self.data,
                   resources=dict(wall_seconds=round(time.monotonic() - self.started, 3),
                                  tool_wall_seconds=round(sum(r["wall_seconds"] for r in runs), 3),
                                  peak_rss_kib=max(rss) if rss else None))
        path = s.out / f"{self.check.replace('-', '_')}.json"
        path.write_text(json.dumps(out, indent=2) + "\n")
        out["verdict_path"] = str(path)
        return out


def run_incomplete(receipt: dict) -> str | None:
    """Why a tool run is not evidence (None if it ran to completion).

    Exit codes never make a verdict pass; a run that was killed (signal, OOM:
    128+N), timed out (124) or never started (podman 125-127) is unverified.
    """
    code = receipt.get("exit_code")
    if code is None or receipt.get("timed_out"):
        return f"tool did not finish ({receipt.get('host_error') or 'timeout'})"
    if code >= 124:
        why = {124: "timeout", 125: "container failed to start", 126: "command not executable",
               127: "command not found"}.get(code, f"killed by signal {code - 128}" if code > 128 else "abnormal exit")
        return f"tool run incomplete: exit {code} ({why})"
    inner = (receipt.get("time") or {}).get("inner_exit_status")
    if isinstance(inner, int) and inner >= 124:
        return f"tool run incomplete: inner exit {inner}"
    return None


def definitive_counts(counts: dict | None, unverified: list[str]) -> dict | None:
    """Null every count of a verdict that has any unverified reason (never 0)."""
    if not unverified or counts is None:
        return counts
    out = {k: None for k in counts if k not in ("unit", "note")}
    out.update(total=None, by_rule=None, unit=counts.get("unit"),
               note="null: the evidence for this check is incomplete (see unverified_reasons)")
    return out


class Session:
    def __init__(self, args):
        self.args = args
        self.out = args.out.resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        self.manifest_path = args.manifest.resolve()
        self.pdk = args.pdk
        self.manifest_error = None
        try:
            self.m = manifest_mod.load(self.manifest_path)
            self.spec = self.m["pdks"][args.pdk]
            if not isinstance(self.spec, dict):
                raise ValueError(f"pdks.{args.pdk} is not an object")
        except (OSError, ValueError, KeyError, TypeError) as error:
            # Identity verification reports this as unverified; nothing is run.
            self.m, self.spec, self.manifest_error = None, None, f"manifest unusable: {error!r}"
        self.gds = args.gds.resolve()
        self.top = args.top
        self.netlist = args.netlist.resolve() if args.netlist else None
        self.against = args.against.resolve() if args.against else None
        self.against_top = args.against_top or args.top
        self.threads = args.threads
        self.identity = None
        self.preflight = None
        self.against_preflight = None
        self.plant = getattr(args, "plant", None)

    # -- shared steps ---------------------------------------------------------
    def ensure_identity(self, checks):
        tools = sorted({t for c in checks for t in TOOLS[c]} | {"coreutils"})
        self.identity = manifest_mod.verify(self.manifest_path, self.out, [self.pdk], tools,
                                            label=container.next_label(self.out, "identity"))
        if self.manifest_error and self.identity["status"] == "verified":
            self.identity = dict(self.identity, status="unverified",
                                 mismatches=self.identity["mismatches"] + [self.manifest_error])
        (self.out / "identity.json").write_text(json.dumps(self.identity, indent=2) + "\n")
        return self.identity

    def cpath(self, key: str) -> str:
        try:
            return manifest_mod.pdk_container_path(self.m, self.pdk, key)
        except (KeyError, TypeError):
            return f"<missing manifest entry pdks.{self.pdk}.files.{key}>"

    def host_pdk(self, key: str) -> Path:
        return manifest_mod.pdk_host_path(self.m, self.pdk, key)

    def mounts(self, extra=()):
        ro = [(str(HERE), "/scripts"), (str(self.gds), "/in/design.gds")]
        if self.netlist is not None and self.netlist.is_file():
            ro.append((str(self.netlist), "/in/reference.v"))
        if self.against is not None and self.against.is_file():
            ro.append((str(self.against), "/in/against.gds"))
        return ro + list(extra)

    def run(self, verdict: Verdict, prefix: str, command, *, env=None, memory=None, timeout=None,
            workdir="/out"):
        receipt = container.run([str(c) for c in command], out_dir=self.out,
                                label=container.next_label(self.out, prefix), ro=self.mounts(),
                                memory=memory or self.args.memory, cpus=max(self.threads, 1),
                                threads=self.threads, timeout=timeout or self.args.timeout,
                                env=env, workdir=workdir)
        return verdict.run(receipt)

    def probe(self, which: str = "design") -> dict | None:
        if which == "design" and self.preflight is not None:
            return self.preflight
        gds_in, top = ("/in/design.gds", self.top) if which == "design" else ("/in/against.gds", self.against_top)
        result = self.out / f"preflight_{which}.json"
        texts = self.out / f"preflight_{which}.texts.tsv"
        for stale in (result, texts):
            if stale.exists():
                stale.unlink()
        receipt = container.run(["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_probe.py",
                                 "-rd", f"gds={gds_in}", "-rd", f"top={top}", "-rd", f"out=/out/{result.name}",
                                 "-rd", f"texts_out=/out/{texts.name}",
                                 "-rd", "boundary=%d/%d" % tuple(self.spec["boundary_layer"])],
                                out_dir=self.out, label=container.next_label(self.out, f"preflight_{which}"),
                                ro=self.mounts(), memory="2g", cpus=1, threads=1, timeout=300)
        info = json.loads(result.read_text()) if result.is_file() and not run_incomplete(receipt) else None
        if info is not None:
            info["receipt"] = dict(label=receipt["label"], wall_seconds=receipt["wall_seconds"],
                                   peak_rss_kib=receipt["peak_rss_kib"])
        if which == "design":
            self.preflight = info
        else:
            self.against_preflight = info
        return info

    def preflight_summary(self) -> dict:
        info = self.preflight
        if info is None:
            return dict(ran=False)
        keys = ("dbu_um", "top_exists", "top_is_top_cell", "top_cells", "bbox_um", "bbox_area_um2",
                "boundary_layer", "boundary_shapes_in_top", "boundary_area_um2", "boundary_bbox_um",
                "geometry_outside_boundary", "top_instances", "cells", "top_texts_truncated")
        out = {k: info.get(k) for k in keys}
        out["top_labels"] = sorted({t[1] for t in info.get("top_texts") or []})[:200]
        out["assertions"] = self.assertions()
        return out

    def assertions(self) -> dict:
        info = self.preflight or {}
        dbu_ok = info.get("dbu_um") is not None and abs(info["dbu_um"] - self.spec["gds_dbu_um"]) < 1e-12
        return dict(top_cell_exists=bool(info.get("top_exists")),
                    top_is_a_top_cell=bool(info.get("top_is_top_cell")),
                    single_top_cell=len(info.get("top_cells") or []) == 1,
                    bbox_nonempty=bool(info.get("bbox_area_um2")) and bool(info.get("bbox_finite", True)),
                    boundary_present=bool(info.get("boundary_shapes_in_top")) and bool(info.get("boundary_area_um2")),
                    dbu_expected=dbu_ok)

    def apply_preflight(self, v: Verdict):
        if self.identity is None or self.identity["status"] != "verified":
            return  # tools are not trusted; gate() records the identity reason
        if self.preflight is None:
            v.unknown("preflight probe failed (GDS unreadable?)")
            return
        a = self.assertions()
        if not a["top_cell_exists"]:
            v.fail(f"top cell {self.top!r} not in GDS (top cells: {self.preflight.get('top_cells')})")
        elif not a["top_is_a_top_cell"]:
            v.fail(f"{self.top!r} is a child cell, not a top cell of the GDS (top cells: "
                   f"{self.preflight.get('top_cells')}); signing off a sub-cell is refused")
        elif not a["single_top_cell"]:
            v.fail(f"GDS has {len(self.preflight.get('top_cells') or [])} top cells "
                   f"{(self.preflight.get('top_cells') or [])[:6]}; a block GDS must hold exactly one, "
                   "otherwise the other top cells are never checked")
        elif not a["bbox_nonempty"]:
            v.fail("top cell bbox is empty")
        if a["top_cell_exists"] and not a["boundary_present"]:
            v.fail("PR boundary layer %d/%d missing from the top cell" % tuple(self.spec["boundary_layer"]))
        if not a["dbu_expected"]:
            v.fail(f"GDS dbu {self.preflight.get('dbu_um')} um != required {self.spec['gds_dbu_um']} um")

    def reference(self) -> tuple[dict | None, str | None]:
        """Parsed reference netlist (cached) or the reason it cannot be parsed."""
        if not hasattr(self, "_reference"):
            try:
                self._reference = (refnet.parse_verilog(self.netlist.read_text()), None)
            except (OSError, refnet.NetlistError) as error:
                self._reference = (None, f"reference netlist unsupported: {error}")
        return self._reference

    def check_reference_top(self, v: Verdict) -> None:
        """The reference netlist must define exactly the module being signed off."""
        modules = parsers.reference_module_names(self.netlist.read_text(errors="replace"))
        v.data["result"]["reference_modules"] = modules[:20]
        if self.top not in modules:
            v.fail(f"reference netlist defines no module {self.top!r} (modules: {modules[:6]}); "
                   "the LVS reference would silently become a library cell or another module")
        elif len(modules) != 1:
            v.unknown(f"reference netlist defines {len(modules)} modules; one flat module is required")

    def check_port_labels(self, v: Verdict) -> None:
        """Harness oracle independent of both LVS tools: GDS top labels == reference ports."""
        net, why = self.reference()
        info = self.preflight or {}
        if net is None:
            v.unknown(f"port-label check: {why}")
            return
        if info.get("top_texts") is None or info.get("top_texts_truncated"):
            v.unknown("port-label check: top-cell labels not probed")
            return
        result = parsers.port_label_check(info["top_texts"], net["ports"])
        v.data["result"]["port_labels"] = result
        for problem in result["problems"]:
            v.fail(f"port labels: {problem}")

    def fresh_dir(self, name: str) -> Path:
        d = self.out / name
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)
        return d

    def gate(self, v: Verdict) -> bool:
        """Return False when identity is unverified (tools are then not trusted/run)."""
        if self.identity["status"] != "verified":
            v.unknown("tool/PDK identity unverified: " + "; ".join(self.identity["mismatches"][:5]))
            return False
        return True


def log_errors(text: str) -> list[str]:
    bad = []
    for line in text.splitlines():
        if re.search(r"^(ERROR|Error)\b|\bERROR\s*:|Traceback \(most recent call last\)|Segmentation fault|Killed", line):
            bad.append(line.strip()[:300])
    return bad[:20]


def save_log(receipt: dict, dest: Path) -> str:
    text = container.read_capture(receipt["stdout"]) + "\n--- stderr ---\n" + container.read_capture(receipt["stderr"])
    dest.write_text(text)
    return text


# -- checks -----------------------------------------------------------------
def magic_env(s: Session, extra=None):
    env = dict(GDS="/in/design.gds", CELL=s.top)
    env.update(extra or {})
    return env


def check_drc_magic(s: Session) -> dict:
    v = Verdict("drc-magic", s)
    s.apply_preflight(v)
    v.data["options"] = dict(style="drc(full)", euclidean="on", flattened_recheck=True,
                             rcfile=s.cpath("magicrc"))
    if not s.gate(v):
        return v.finish()
    d = s.fresh_dir("drc_magic")
    r = s.run(v, "drc_magic", ["/foss/tools/bin/magic", "-dnull", "-noconsole", "-rcfile",
                               s.cpath("magicrc"), "/scripts/magic_drc.tcl"], env=magic_env(s))
    log = save_log(r, d / "magic_drc.log")
    v.data["reports"]["log"] = str(d / "magic_drc.log")
    p = parsers.parse_magic_drc(log)
    if p["top_missing"]:
        v.fail(f"Magic: top cell {p['top_missing']} not in GDS (would silently create an empty cell)")
        v.data["result"] = p
        return v.finish()
    if p["style"] != "drc(full)":
        v.unknown(f"DRC style not confirmed as drc(full): {p['style']!r}")
    if not p["done"] or p["hier_total"] is None or p["flat_total"] is None:
        v.unknown("Magic DRC did not complete (missing RR_DONE/totals)")
    else:
        hier_boxes, flat_boxes = sum(p["hier_why"].values()), sum(p["flat_why"].values())
        v.data["counts"] = dict(total=p["hier_total"], flattened_total=p["flat_total"],
                                by_rule=p["flat_why"], hierarchical_by_rule=p["hier_why"],
                                unit="Magic error tiles (total) / error boxes (by_rule)",
                                standalone_cell_errors=p["standalone_cell_why"])
        if p["hier_total"] or p["flat_total"] or hier_boxes or flat_boxes:
            v.fail(f"Magic DRC: {p['hier_total']} errors (hierarchical), {p['flat_total']} (flattened); "
                   f"rules {p['flat_why'] or p['hier_why']}")
    v.data["result"] = dict(bbox_internal_units=p["bbox"], cells=p["cells"],
                            note="standalone_cell_errors are each child cell checked without context "
                                 "(e.g. tap/latch-up or via-cell min-area); the flattened recheck is the "
                                 "context-correct count across all cells")
    return v.finish()


def klayout_drc(s: Session, v: Verdict, d: Path, decks: str | None, report_name: str) -> dict | None:
    """Run the PDK KLayout DRC deck; return parsed lyrdb or None (evidence missing)."""
    report = d / report_name
    if s.pdk == "sky130":
        opts = dict(KLAYOUT_DRC_OPTIONS["sky130"]) if decks is None else {}
        deck = s.cpath("klayout_drc") if decks is None else s.cpath("klayout_density")
        cmd = ["/foss/tools/klayout/klayout", "-b", "-r", deck, "-rd", "input=/in/design.gds"]
        rd = dict(opts, thr=str(s.threads), top_cell=s.top) if decks is None else {}
    else:
        opts = dict(KLAYOUT_DRC_OPTIONS["gf180"])
        if decks is not None:
            opts["decks"] = decks
        deck = s.cpath("klayout_drc")
        cmd = ["/foss/tools/klayout/klayout", "-b", "-r", deck, "-rd", "input=/in/design.gds"]
        rd = dict(opts, threads=str(s.threads), topcell=s.top)
    report_arg = f"/out/{d.name}/{report_name}"
    if s.plant == "relative-report":
        report_arg = report_name  # reproduces the trap: KLayout writes nothing and exits 0
    cmd += ["-rd", f"report={report_arg}"]
    for key, value in rd.items():
        cmd += ["-rd", f"{key}={value}"]
    v.data["options"].setdefault("decks", []).append(dict(deck=deck, options=rd, report=str(report)))
    start = time.time()
    r = s.run(v, f"{d.name}_klayout", cmd)
    log = save_log(r, d / (report_name + ".log"))
    v.data["reports"][report_name] = str(report)
    v.data["reports"][report_name + ".log"] = str(d / (report_name + ".log"))
    errors = log_errors(log)
    if errors:
        v.unknown(f"KLayout log errors: {errors[:3]}")
    if not report.is_file() or report.stat().st_mtime < start - 1:
        v.unknown(f"report {report} was not written (exit code {r['exit_code']} is not evidence)")
        return None
    try:
        parsed = parsers.parse_lyrdb(report)
    except parsers.ParseError as error:
        v.unknown(str(error))
        return None
    if parsed["top_cell"] != s.top:
        v.unknown(f"report top cell {parsed['top_cell']!r} != {s.top!r}")
    parsed["log_text"] = log
    return parsed


def check_drc_klayout(s: Session) -> dict:
    v = Verdict("drc-klayout", s)
    s.apply_preflight(v)
    if not s.gate(v):
        return v.finish()
    d = s.fresh_dir("drc_klayout")
    parsed = klayout_drc(s, v, d, None, "drc.lyrdb")
    if parsed is not None:
        log = parsed.pop("log_text")
        if s.pdk == "gf180":
            m = re.findall(r"DRC RESULT:\s*(.*)", log)
            v.data["result"]["deck_summary_line"] = m[-1].strip() if m else None
        v.data["counts"] = dict(total=parsed["total"], by_rule=parsed["by_rule"], by_cell=parsed["by_cell"],
                                multiplicity_by_rule=parsed["multiplicity_by_rule"], unit="lyrdb items, all cells")
        v.data["result"].update(categories=parsed["categories"], samples=parsed["samples"][:20])
        if parsed["total"]:
            v.fail(f"KLayout DRC: {parsed['total']} items {parsed['by_rule']}")
    return v.finish()


def magic_antenna(s: Session, v: Verdict, d: Path) -> dict:
    r = s.run(v, "antenna_magic", ["/foss/tools/bin/magic", "-dnull", "-noconsole", "-rcfile",
                                   s.cpath("magicrc"), "/scripts/magic_antenna.tcl"],
              env=magic_env(s, dict(OUTDIR=f"/out/{d.name}/magic")))
    log = save_log(r, d / "magic_antenna.log")
    v.data["reports"]["magic_antenna_log"] = str(d / "magic_antenna.log")
    return parsers.parse_magic_antenna(log)


def check_antenna(s: Session) -> dict:
    v = Verdict("antenna", s)
    s.apply_preflight(v)
    if not s.gate(v):
        return v.finish()
    d = s.fresh_dir("antenna")
    magic = magic_antenna(s, v, d)
    if s.pdk == "sky130":
        primary = dict(tool="magic antennacheck", **magic)
        secondary = dict(tool="klayout", status="not_available",
                         reason="the sky130 KLayout DRC decks contain no antenna rules")
        if magic["top_missing"]:
            v.fail("Magic: top cell missing")
        elif not magic["ran"] or not magic["done"]:
            v.unknown("Magic antennacheck did not run to completion")
        elif magic["unparsed_violation_lines"]:
            v.unknown("unparsed Magic antenna violation lines")
        elif magic["violations"]:
            v.fail(f"Magic antenna: {magic['violations']} violations {magic['by_plane']}")
        v.data["counts"] = dict(total=magic["violations"], by_rule=magic["by_plane"], unit="Magic violations (by plane)")
    else:
        parsed = klayout_drc(s, v, d, "antenna", "antenna.lyrdb")
        primary = dict(tool="klayout gf180mcu.drc decks=antenna")
        if parsed is not None:
            parsed.pop("log_text")
            primary.update(total=parsed["total"], by_rule=parsed["by_rule"], by_cell=parsed["by_cell"])
            v.data["counts"] = dict(total=parsed["total"], by_rule=parsed["by_rule"], by_cell=parsed["by_cell"],
                                    unit="lyrdb items, all cells")
            if parsed["total"]:
                v.fail(f"KLayout antenna: {parsed['total']} items {parsed['by_rule']}")
        secondary = dict(tool="magic antennacheck", status="secondary_incomplete_rules",
                         note="gf180mcuD.tech antenna section is marked incomplete; a clean result is not evidence",
                         **magic)
        if magic.get("violations"):
            v.fail(f"secondary Magic antenna check reports {magic['violations']} violations")
    v.data["result"] = dict(primary=primary, secondary=secondary)
    return v.finish()


def check_density(s: Session) -> dict:
    v = Verdict("density", s)
    s.apply_preflight(v)
    if not s.gate(v):
        return v.finish()
    d = s.fresh_dir("density")
    decks = []
    if s.pdk == "sky130":
        parsed = klayout_drc(s, v, d, "density", "density.lyrdb")
        entry = dict(deck="met_min_ca_density.lydrc (max density / min clear area)", status="unverified")
        if parsed is not None:
            dens = parsers.parse_density_log(parsed.pop("log_text"))
            entry.update(items=parsed["total"], by_rule=parsed["by_rule"], densities=dens["densities"])
            if not dens["finite"]:
                entry["reason"] = f"non-finite or missing densities {dens['nonfinite'] or 'none printed'} (needs 235/4 boundary)"
                v.unknown("density deck: " + entry["reason"])
            elif parsed["total"]:
                entry["status"] = "failed"
                v.fail(f"density deck: {parsed['total']} items")
            else:
                entry["status"] = "passed"
        decks.append(entry)
        # Windowed min/max density check shipped with the PDK (Magic). It refuses small layouts.
        md = d / "magic_window"
        md.mkdir()
        shutil.copyfile(s.gds, md / "design.gds")
        r = s.run(v, "density_magic", ["/usr/bin/python3", s.cpath("magic_check_density"), "design.gds"],
                  workdir=f"/out/{d.name}/magic_window")
        log = save_log(r, d / "magic_check_density.log")
        v.data["reports"]["magic_check_density_log"] = str(d / "magic_check_density.log")
        entry = dict(deck="libs.tech/magic/check_density.py (700 um windows, min and max density)")
        if "cannot run density checks" in log:
            entry.update(status="unverified", reason="layout smaller than the 700 um density window; "
                         "minimum-density (fill) compliance is chip-level and unchecked here")
            v.unknown("windowed density: " + entry["reason"])
        else:
            entry.update(status="unverified", reason="output not recognised; refusing to infer a pass",
                         tail=log[-2000:])
            v.unknown("windowed density output unrecognised")
        decks.append(entry)
    else:
        parsed = klayout_drc(s, v, d, "density", "density.lyrdb")
        entry = dict(deck="gf180mcu.drc decks=density (global coverage rules)", status="unverified")
        if parsed is not None:
            parsed.pop("log_text")
            entry.update(items=parsed["total"], by_rule=parsed["by_rule"], by_cell=parsed["by_cell"])
            entry["status"] = "failed" if parsed["total"] else "passed"
            if parsed["total"]:
                v.fail(f"density deck: {parsed['total']} items {parsed['by_rule']}")
        decks.append(entry)
    # Per-deck counts; the total is null (never 0) unless every deck produced a definitive count.
    by_rule, by_cell = {}, {}
    for entry in decks:
        for rule_name, n in (entry.get("by_rule") or {}).items():
            by_rule[rule_name] = by_rule.get(rule_name, 0) + n
        for cell, n in (entry.get("by_cell") or {}).items():
            by_cell[cell] = by_cell.get(cell, 0) + n
    complete = bool(decks) and all(e.get("status") in ("passed", "failed") and isinstance(e.get("items"), int)
                                   for e in decks)
    v.data["counts"] = dict(total=sum(e["items"] for e in decks) if complete else None,
                            by_deck={e["deck"]: dict(status=e.get("status"), items=e.get("items")) for e in decks},
                            by_rule=by_rule, by_cell=by_cell, unit="density deck items, all cells",
                            note=None if complete else "total is null: at least one density deck gave no definitive count")
    v.data["result"] = dict(decks=decks, note="density_status is separate from the block DRC verdict")
    return v.finish()


def check_lvs_netgen(s: Session) -> dict:
    v = Verdict("lvs-netgen", s)
    s.apply_preflight(v)
    if s.netlist is None or not s.netlist.is_file():
        v.unknown("no reference netlist given (--netlist)")
        return v.finish()
    if not s.gate(v):
        return v.finish()
    s.check_reference_top(v)
    s.check_port_labels(v)
    d = s.fresh_dir("lvs_netgen")
    r = s.run(v, "lvs_magic_extract", ["/foss/tools/bin/magic", "-dnull", "-noconsole", "-rcfile",
                                       s.cpath("magicrc"), "/scripts/magic_extract.tcl"],
              env=magic_env(s, dict(OUTDIR=f"/out/{d.name}/ext")))
    log = save_log(r, d / "magic_extract.log")
    layout = d / "ext" / f"{s.top}.spice"
    v.data["reports"].update(extract_log=str(d / "magic_extract.log"), layout_netlist=str(layout))
    if "RR_TOP_MISSING" in log:
        v.fail("Magic: top cell missing; nothing extracted")
        return v.finish()
    if "RR_EXT_DONE" not in log or not layout.is_file():
        v.unknown("Magic extraction did not produce a layout netlist")
        return v.finish()
    # Extraction evidence of the Netgen flow itself: shorted port names, and the
    # extracted top-level port list against the reference ports (both ways).
    ext = parsers.parse_magic_extract(log)
    extracted_ports = parsers.spice_subckt_ports(layout.read_text(errors="replace"), s.top)
    ref_net, _ = s.reference()
    ext.update(extracted_top_ports=extracted_ports, problems=[])
    if ext["shorted_ports"]:
        ext["problems"].append(f"ports electrically shorted {ext['shorted_ports'][:6]}")
    if extracted_ports is None:
        v.unknown(f"extracted layout netlist has no .subckt {s.top}")
    elif ref_net is not None:
        got, want = set(extracted_ports), set(ref_net["ports"])
        ext.update(layout_only_ports=sorted(got - want)[:50], reference_only_ports=sorted(want - got)[:50])
        if got != want:
            ext["problems"].append(f"extracted top ports differ from the reference (layout only "
                                   f"{sorted(got - want)[:6]}, reference only {sorted(want - got)[:6]})")
    for problem in ext["problems"]:
        v.fail(f"Magic extraction: {problem}")
    v.data["result"]["extraction"] = ext
    comp = d / "comp.out"
    r = s.run(v, "lvs_netgen", ["/foss/tools/bin/netgen", "-batch", "source", "/scripts/netgen_lvs.tcl"],
              env=dict(LAYOUT=f"/out/{d.name}/ext/{s.top}.spice", TOP=s.top, REFV="/in/reference.v",
                       STDSPICE=s.cpath("cell_spice"), SETUP=s.cpath("netgen_setup"),
                       OUT=f"/out/{d.name}/comp.out"))
    save_log(r, d / "netgen.log")
    comp_json = d / "comp.json"
    v.data["reports"].update(comp_out=str(comp), comp_json=str(comp_json), netgen_log=str(d / "netgen.log"))
    v.data["options"] = dict(setup=s.cpath("netgen_setup"), std_spice=s.cpath("cell_spice"),
                             reference="powered structural Verilog read on top of the PDK cell SPICE")
    if not comp.is_file() or not comp_json.is_file():
        v.unknown("netgen did not write comp.out/comp.json (exit code is always 0; not evidence)")
        return v.finish()
    try:
        p = parsers.parse_netgen(comp_json.read_text(), comp.read_text(errors="replace"), s.top)
    except parsers.ParseError as error:
        v.unknown(str(error))
        return v.finish()
    v.data["result"].update(p)
    top = [c for c in p["details"] if c["name"] == s.top]
    v.data["counts"] = dict(total=len(p["problems"]), by_rule={"mismatch": len(p["problems"])},
                            top=top[0] if top else None, unit="netgen problems")
    if not p["matched"]:
        v.fail("Netgen: " + "; ".join(p["problems"][:6]))
    return v.finish()


def check_lvs_klayout(s: Session) -> dict:
    v = Verdict("lvs-klayout", s)
    s.apply_preflight(v)
    if s.netlist is None or not s.netlist.is_file():
        v.unknown("no reference netlist given (--netlist)")
        return v.finish()
    if not s.gate(v):
        return v.finish()
    d = s.fresh_dir("lvs_klayout")
    cdl = s.pdk == "gf180"
    library_file = s.host_pdk("cell_cdl" if cdl else "cell_spice")
    s.check_port_labels(v)
    try:
        net = refnet.parse_verilog(s.netlist.read_text())
        lib = refnet.parse_subckts(library_file.read_text(errors="replace"))
        text, info = refnet.spice_reference(net, lib, rescale_units=not cdl, cdl=cdl)
    except refnet.NetlistError as error:
        v.unknown(f"reference netlist unsupported: {error}")
        return v.finish()
    if net["top"] != s.top:
        v.fail(f"reference netlist top {net['top']!r} != layout top {s.top!r}")
    ref = d / ("reference.cdl" if cdl else "reference.spice")
    ref.write_text(text)
    v.data["reports"]["schematic"] = str(ref)
    v.data["result"]["schematic"] = dict(info, library=str(library_file), source=str(s.netlist))
    lvsdb = d / ("run/design.lvsdb" if cdl else "lvs.lvsdb")
    start = time.time()
    if cdl:
        options = dict(variant="D", run_mode="deep", thr=str(s.threads), lvs_sub="default (gf180mcu_gnd)",
                       top_lvl_pins="true")
        r = s.run(v, "lvs_klayout", ["/usr/bin/python3", s.cpath("klayout_lvs_runner"), "--layout=/in/design.gds",
                                     f"--netlist=/out/{d.name}/{ref.name}", "--variant=D", f"--topcell={s.top}",
                                     "--run_mode=deep", f"--run_dir=/out/{d.name}/run", f"--thr={s.threads}",
                                     "--top_lvl_pins"])
    else:
        options = dict(run_mode="deep", convert_subckts="true", combine="true", schematic_simplify="true",
                       top_lvl_pins="true", thr=str(s.threads))
        r = s.run(v, "lvs_klayout", ["/foss/tools/klayout/klayout", "-b", "-r", s.cpath("klayout_lvs"),
                                     "-rd", "input=/in/design.gds", "-rd", f"report=/out/{d.name}/lvs.lvsdb",
                                     "-rd", f"schematic=/out/{d.name}/{ref.name}",
                                     "-rd", f"target_netlist=/out/{d.name}/extracted.cir",
                                     "-rd", f"top_cell={s.top}"] +
                  [x for k, val in options.items() for x in ("-rd", f"{k}={val}")])
    v.data["options"] = dict(deck=s.cpath("klayout_lvs_runner" if cdl else "klayout_lvs"),
                             pin_name_check="top pins: every layout<->schematic pin pair must have equal names "
                                            "(case-insensitive); library cells: equal unless a deck body pin",
                             net_name_check="every labelled top-level layout net must be paired (Match) with the "
                                            "equally named schematic net",
                             port_check="every reference port must be a paired top-level pin and vice versa",
                             body_pin_pairs=[list(x) for x in LVS_BODY_PINS[s.pdk]], **options)
    log = save_log(r, d / "klayout_lvs.log")
    v.data["reports"].update(lvsdb=str(lvsdb), log=str(d / "klayout_lvs.log"))
    if not lvsdb.is_file() or lvsdb.stat().st_mtime < start - 1:
        v.unknown("lvsdb not written (exit code is not evidence)")
        return v.finish()
    xref_path = d / "xref.json"
    r = s.run(v, "lvs_klayout_xref", ["/foss/tools/klayout/klayout", "-b", "-r", "/scripts/kl_lvsdb.py",
                                      "-rd", f"db_path=/out/{d.name}/{lvsdb.relative_to(d)}",
                                      "-rd", f"out=/out/{d.name}/xref.json"], memory="2g")
    if not xref_path.is_file():
        v.unknown("lvsdb cross-reference dump failed")
        return v.finish()
    verdict = parsers.lvsdb_verdict(json.loads(xref_path.read_text()), s.top, LVS_BODY_PINS[s.pdk],
                                    reference_ports=net["ports"])
    v.data["reports"]["xref"] = str(xref_path)
    if cdl:
        congrats = "Congratulations! Netlists match." in log
        verdict["deck_message_match"] = congrats
        if verdict["matched"] and not congrats:
            verdict["matched"] = False
            verdict["problems"].append("xref matched but the deck did not report a netlist match")
    v.data["result"].update(verdict)
    v.data["counts"] = dict(total=len(verdict["problems"]), by_rule={"mismatch": len(verdict["problems"])},
                            unit="non-matching circuit pairs/problems")
    if not verdict["matched"]:
        v.fail("KLayout LVS: " + "; ".join(verdict["problems"][:6]))
    return v.finish()


def check_xor(s: Session) -> dict:
    v = Verdict("xor", s)
    s.apply_preflight(v)
    if s.against is None or not s.against.is_file():
        v.unknown("no comparison layout given (--against)")
        return v.finish()
    if not s.gate(v):
        return v.finish()
    other = s.probe("against")
    if other is None:
        v.unknown(f"comparison layout {s.against} could not be probed")
    elif not other.get("top_exists"):
        v.fail(f"comparison top cell {s.against_top!r} not in {s.against}")
    elif not other.get("top_is_top_cell"):
        v.fail(f"comparison cell {s.against_top!r} is a child cell of {s.against}, not a top cell")
    d = s.fresh_dir("xor")
    r = s.run(v, "xor_strmxor", ["/foss/tools/klayout/strmxor", "--layer-details", f"--top-a={s.top}",
                                 f"--top-b={s.against_top}", "/in/design.gds", "/in/against.gds",
                                 f"/out/{d.name}/xor.gds"])
    log = save_log(r, d / "strmxor.log")
    p = parsers.parse_strmxor(log)
    v.data["reports"].update(log=str(d / "strmxor.log"), xor_gds=str(d / "xor.gds"))
    v.data["options"] = dict(layer_details=True, mode="flat geometric XOR, tolerance 0",
                             labels="text labels compared separately (layer, string, position in pm): top cell "
                                    "and flattened hierarchy; strmxor compares polygons only")
    mine = s.preflight or {}
    labels = compare_labels(s, mine, other or {})
    v.data["result"] = dict(p, labels=labels, hierarchy=dict(
        design_cells=mine.get("cells"), against_cells=(other or {}).get("cells"),
        design_top_instances=mine.get("top_instances"), against_top_instances=(other or {}).get("top_instances"),
        dbu=[mine.get("dbu_um"), (other or {}).get("dbu_um")]))
    if p["identical"] and r["exit_code"] == 0 and not p["layer_differences"]:
        pass
    elif "No differences found" not in log and not p["layer_differences"]:
        v.unknown("strmxor summary not recognised")
    else:
        v.fail(f"XOR differences: {p['layer_differences'] or 'reported'}")
    if labels["status"] == "unverified":
        v.unknown("label comparison: " + labels["reason"])
    elif labels["status"] == "failed":
        v.fail(f"label differences: {labels['flat']['only_in_design']} labels only in the design, "
               f"{labels['flat']['only_in_against']} only in the comparison layout "
               f"(e.g. {(labels['flat']['samples_only_in_design'] or labels['flat']['samples_only_in_against'])[:2]})")
    geometry = p["difference_count"] or 0 if p["identical"] else p["difference_count"]
    label_diffs = labels["flat"]["only_in_design"] + labels["flat"]["only_in_against"] \
        if labels["status"] in ("passed", "failed") else None
    by_rule = dict(p["layer_differences"])
    if label_diffs:
        by_rule["labels"] = label_diffs
    v.data["counts"] = dict(total=None if geometry is None or label_diffs is None else geometry + label_diffs,
                            geometry=geometry, labels=label_diffs, by_rule=by_rule,
                            unit="XOR difference shapes per layer + label records that differ")
    return v.finish()


def compare_labels(s: Session, mine: dict, other: dict) -> dict:
    """Exact multiset comparison of text labels (layer, string, x, y) of two layouts."""
    flat_a, flat_b = mine.get("flat_texts") or {}, other.get("flat_texts") or {}
    path_a = s.out / "preflight_design.texts.tsv"
    path_b = s.out / "preflight_against.texts.tsv"
    if mine.get("top_texts") is None or other.get("top_texts") is None or \
            mine.get("top_texts_truncated") or other.get("top_texts_truncated"):
        return dict(status="unverified", reason="top-cell labels not probed", flat=None, top=None)
    if not flat_a.get("sha256") or not flat_b.get("sha256") or not path_a.is_file() or not path_b.is_file():
        return dict(status="unverified", reason="flattened label lists missing or truncated", flat=None, top=None)
    for flat, path in ((flat_a, path_a), (flat_b, path_b)):
        if sha256(path) != flat["sha256"]:
            return dict(status="unverified", reason=f"{path} does not match its probe digest", flat=None, top=None)
    top = parsers.compare_texts(mine["top_texts"], other["top_texts"])
    if flat_a["sha256"] == flat_b["sha256"]:
        flat = dict(only_in_design=0, only_in_against=0, samples_only_in_design=[], samples_only_in_against=[])
    else:
        try:
            flat = parsers.diff_sorted_lines(path_a, path_b)
        except (parsers.ParseError, OSError, UnicodeDecodeError) as error:
            return dict(status="unverified", reason=f"label lists unreadable: {error}", flat=None, top=top)
    flat.update(design=flat_a["count"], against=flat_b["count"], design_sha256=flat_a["sha256"],
                against_sha256=flat_b["sha256"], design_list=str(path_a), against_list=str(path_b))
    identical = top["identical"] and flat["only_in_design"] == 0 and flat["only_in_against"] == 0
    return dict(status="passed" if identical else "failed", top=top, flat=flat)


RUNNERS = {"drc-magic": check_drc_magic, "drc-klayout": check_drc_klayout, "antenna": check_antenna,
           "density": check_density, "lvs-netgen": check_lvs_netgen, "lvs-klayout": check_lvs_klayout,
           "xor": check_xor}


def worst(*statuses):
    statuses = [s for s in statuses if s is not None]
    if not statuses:
        return "unverified"
    if "failed" in statuses:
        return "failed"
    if any(s != "passed" for s in statuses):
        return "unverified"
    return "passed"


def run_checks(args, checks) -> dict:
    s = Session(args)
    started = time.monotonic()
    s.ensure_identity(checks)
    if s.identity["status"] == "verified":
        s.probe("design")
    verdicts = {c: RUNNERS[c](s) for c in checks}
    st = {c.replace("-", "_"): v["status"] for c, v in verdicts.items()}
    summary = dict(schema="rustroad.signoff-summary.v1", pdk=s.pdk, top_cell=s.top,
                   gds=fingerprint(s.gds), reference_netlist=fingerprint(s.netlist),
                   identity=dict(status=s.identity["status"], mismatches=s.identity["mismatches"],
                                 manifest=str(s.manifest_path), manifest_sha256=s.identity.get("manifest_sha256")),
                   statuses=st,
                   drc_status=worst(st.get("drc_magic"), st.get("drc_klayout")) if {"drc_magic", "drc_klayout"} <= set(st) else None,
                   antenna_status=st.get("antenna"),
                   lvs_status=worst(st.get("lvs_netgen"), st.get("lvs_klayout")) if {"lvs_netgen", "lvs_klayout"} <= set(st) else None,
                   density_status=st.get("density"),
                   # A count is reported only for a definitive verdict; unverified -> null, never 0.
                   counts={c: (v.get("counts") or {}).get("total") if v["status"] in ("passed", "failed") else None
                           for c, v in verdicts.items()},
                   by_rule={c: (v.get("counts") or {}).get("by_rule") for c, v in verdicts.items()},
                   verdicts={c: v["verdict_path"] for c, v in verdicts.items()},
                   scope=SCOPE,
                   resources=dict(wall_seconds=round(time.monotonic() - started, 3),
                                  peak_rss_kib_per_check={c: v["resources"]["peak_rss_kib"] for c, v in verdicts.items()},
                                  peak_rss_kib=max([v["resources"]["peak_rss_kib"] or 0 for v in verdicts.values()] or [0])))
    block = [st[k] for k in st if k != "density"]
    summary["block_signoff_status"] = worst(*block) if block else None
    summary["status"] = summary["block_signoff_status"]
    return summary


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("check", choices=CHECKS + ("all",))
    parser.add_argument("--gds", type=Path, required=True)
    parser.add_argument("--top", required=True)
    parser.add_argument("--pdk", choices=("sky130", "gf180"), required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--netlist", type=Path, help="powered structural Verilog LVS reference")
    parser.add_argument("--against", type=Path, help="second GDS for xor")
    parser.add_argument("--against-top", help="top cell of --against (default: --top)")
    parser.add_argument("--manifest", type=Path, default=manifest_mod.DEFAULT_MANIFEST)
    parser.add_argument("--threads", type=int, default=4)
    parser.add_argument("--memory", default="4g")
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--plant", choices=("relative-report",), help=argparse.SUPPRESS)
    return parser


def crash_summary(args, checks, error: BaseException) -> dict:
    """A harness crash is never a verdict: every requested check becomes unverified (exit 3)."""
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    trace = traceback.format_exc()[-4000:]
    verdicts = {}
    for c in checks:
        path = out / f"{c.replace('-', '_')}.json"
        path.write_text(json.dumps(dict(schema=SCHEMA, check=c, pdk=args.pdk, status="unverified", failures=[],
                                        unverified_reasons=[f"harness error: {error!r}"], counts=None,
                                        traceback=trace, scope=SCOPE), indent=2) + "\n")
        verdicts[c] = str(path)
    st = {c.replace("-", "_"): "unverified" for c in checks}
    return dict(schema="rustroad.signoff-summary.v1", pdk=args.pdk, top_cell=args.top, status="unverified",
                statuses=st, block_signoff_status="unverified", density_status=st.get("density"),
                drc_status=None, lvs_status=None, antenna_status=st.get("antenna"),
                counts={c: None for c in checks}, verdicts=verdicts, error=repr(error), traceback=trace,
                scope=SCOPE)


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    if not re.fullmatch(r"[A-Za-z0-9_$.\[\]-]+", args.top):
        raise SystemExit(f"unsafe top cell name {args.top!r}")
    if not args.gds.is_file():
        raise SystemExit(f"GDS not found: {args.gds}")
    checks = list(CHECKS) if args.check == "all" else [args.check]
    if args.check == "all" and args.against is None:
        checks.remove("xor")
    try:
        summary = run_checks(args, checks)
    except Exception as error:  # noqa: BLE001 - any crash must map to unverified, never to exit 1
        print(traceback.format_exc(), file=sys.stderr)
        summary = crash_summary(args, checks, error)
    path = args.out.resolve() / ("signoff.json" if args.check == "all" else f"summary_{args.check.replace('-', '_')}.json")
    summary["summary_path"] = str(path)
    path.write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(dict(pdk=summary["pdk"], status=summary["status"], statuses=summary["statuses"],
                          density_status=summary["density_status"], counts=summary["counts"],
                          summary=str(path))))
    status = summary["status"] if args.check == "all" else summary["statuses"][args.check.replace("-", "_")]
    return {"passed": 0, "failed": 1}.get(status, 3)


if __name__ == "__main__":
    sys.exit(main())
