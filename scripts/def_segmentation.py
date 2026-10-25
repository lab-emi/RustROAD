#!/usr/bin/env python3
"""Geometry and reference-extraction proof for export-def's merged wire runs.

For each PDK the per-lattice-edge DEF of an older RustROAD (OLD) and the
merged-run DEF of the current `export-def` (NEW) of the SAME routed workspace
are compared:

1. text: everything outside the NETS section is byte-identical; every net
   keeps its header, its via statements and its RECT patch statements; only
   wire paths differ;
2. geometry: KLayout LEF/DEF reference streamouts (pinned signoff image) of
   OLD and NEW are XOR-compared on every layer (strmxor --layer-details) with
   exact text-label comparison (scripts/signoff xor check). A copy of NEW with
   its longest wire path deleted must fail the same XOR (negative control);
3. merge-def: the independent merger of scripts/compare_spef.py is applied to
   OLD and NEW (path counts reported; it keeps upper-layer via landings inside
   a path, NEW splits there);
4. reference extraction: OpenRCX (black box, pinned image) on OLD, on
   merge-def(OLD) and on NEW at min/nom/max; design totals and per-net C/R of
   NEW against merge-def(OLD) and of OLD against merge-def(OLD).

usage: def_segmentation.py --run NEW_DIR --case PDK:OLD.def:NEW.def:ROUTES.json
                           [--case ...] [--evidence docs/evidence/def-segmentation.json]
Exit 0 when every geometry/text check and control passes, 1 otherwise, 3 when
a tool or input is unavailable (never a pass). Third-party tools only run as
black boxes; no reference source is read.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "signoff"))
import compare_spef  # noqa: E402
from verify_physical import PhysicalHarness  # noqa: E402
import def2gds_reference  # noqa: E402
import signoff as signoff_harness  # noqa: E402

ROOT = HERE.parent
MANIFEST = ROOT / "fixtures/flow/signoff-pinned.json"
PATH = re.compile(r"^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)\s*;?\s*$")
VIA = compare_spef.VIA
RECT = compare_spef.RECT


def nets_section(text: str) -> tuple[str, str, str]:
    head, sep, rest = text.partition("\nNETS ")
    if not sep:
        raise ValueError("DEF NETS section missing")
    body, sep, tail = rest.partition("\nEND NETS\n")
    if not sep:
        raise ValueError("DEF END NETS missing")
    return head, body, tail


def parse_nets(body: str) -> dict:
    nets, current = {}, None
    for line in body.split("\n")[1:]:
        if line.startswith("- "):
            current = dict(header=line, wires=[], vias=[], rects=[])
            nets[line.split()[1]] = current
            continue
        for key, pattern in (("rects", RECT), ("wires", PATH), ("vias", VIA)):
            m = pattern.match(line)
            if m and current is not None:
                current[key].append(tuple(m.groups()))
                break
        else:
            if current is None or line.strip() not in ("", ";"):
                raise ValueError(f"unexpected NETS line {line!r}")
    return nets


def text_check(old: Path, new: Path) -> dict:
    a, b = old.read_text(), new.read_text()
    (ha, ba, ta), (hb, bb, tb) = nets_section(a), nets_section(b)
    na, nb = parse_nets(ba), parse_nets(bb)
    problems = []
    if ha != hb or ta != tb:
        problems.append("text outside NETS differs")
    if ba.split("\n")[0] != bb.split("\n")[0]:
        problems.append("NETS count line differs")
    if list(na) != list(nb):
        problems.append("net names/order differ")
    for name in set(na) & set(nb):
        x, y = na[name], nb[name]
        if x["header"] != y["header"]:
            problems.append(f"{name}: header differs")
        if sorted(x["vias"]) != sorted(y["vias"]):
            problems.append(f"{name}: via statements differ")
        if sorted(x["rects"]) != sorted(y["rects"]):
            problems.append(f"{name}: RECT patches differ")
    count = lambda n, k: sum(len(v[k]) for v in n.values())
    return dict(status="passed" if not problems else "failed", problems=problems[:20], nets=len(nb),
                old_wire_paths=count(na, "wires"), new_wire_paths=count(nb, "wires"),
                vias=count(nb, "vias"), rect_patches=count(nb, "rects"))


def delete_longest_path(src: Path, dst: Path) -> dict:
    text = src.read_text()
    head, body, tail = nets_section(text)
    lines = body.split("\n")
    best = None
    for i, line in enumerate(lines):
        m = PATH.match(line)
        if m:
            x0, y0, x1, y1 = map(int, m.groups()[1:])
            length = abs(x1 - x0) + abs(y1 - y0)
            if best is None or length > best[0]:
                best = (length, i, line)
    if best is None:
        raise ValueError("no wire path to delete")
    _, i, line = best
    if line.lstrip().startswith("+ ROUTED"):
        # Promote the next path of the net to `+ ROUTED`.
        j = i + 1
        if j < len(lines) and lines[j].lstrip().startswith("NEW "):
            lines[j] = lines[j].replace("NEW ", "+ ROUTED ", 1)
        elif line.rstrip().endswith(";"):
            lines[i - 1] = lines[i - 1].rstrip() + " ;"
    elif line.rstrip().endswith(";"):
        lines[i - 1] = lines[i - 1].rstrip() + " ;"
    del lines[i]
    dst.write_text(head + "\nNETS " + "\n".join(lines) + "\nEND NETS\n" + tail)
    return dict(deleted=line.strip(), length_dbu=best[0])


def xor(gds: Path, against: Path, pdk: str, out: Path) -> dict:
    ns = argparse.Namespace(gds=gds, top="lane_smoke", pdk=pdk, out=out, netlist=None, against=against,
                            against_top="lane_smoke", manifest=MANIFEST, threads=2, memory="4g", timeout=600,
                            plant=None)
    summary = signoff_harness.run_checks(ns, ["xor"])
    verdict = json.loads(Path(summary["verdicts"]["xor"]).read_text())
    return dict(status=verdict["status"], counts=verdict.get("counts"), failures=verdict.get("failures"),
                unverified_reasons=verdict.get("unverified_reasons"),
                labels=(verdict.get("result") or {}).get("labels", {}).get("status"), verdict=summary["verdicts"]["xor"])


def stream(definition: Path, pdk: str, out: Path) -> dict:
    receipt = def2gds_reference.stream(definition, pdk, out, MANIFEST)
    if receipt["status"] != "passed":
        raise RuntimeError(f"reference streamout of {definition.name} failed: {receipt['problems']}")
    return dict(status=receipt["status"], sha256=def2gds_reference.sha256(out))


def extraction(h, pdk: str, defs: dict) -> dict:
    out = {}
    for corner in ("min", "nom", "max"):
        spefs, runs = {}, {}
        for kind, path in defs.items():
            spef, info = compare_spef.openrcx(h, f"{pdk}_{kind}", pdk, corner, path, f"{pdk}_{kind}_{corner}")
            spefs[kind], runs[kind] = spef, info
        totals = {k: compare_spef.compare_spefs(v, spefs["old_merged"])["design_native"] for k, v in spefs.items()}

        def brief(a, b):
            c = compare_spef.compare_spefs(spefs[a], spefs[b])
            rows = c["nets"]
            return dict(total_c_rel=c["total_c_rel"], coverage=c["coverage"]["status"],
                        max_abs_net_c_rel=max((abs(r["c_rel"]) for r in rows if r["c_rel"] is not None), default=None),
                        max_abs_net_r_rel=max((abs(r["r_rel"]) for r in rows if r["r_rel"] is not None), default=None),
                        targets={k: v["status"] for k, v in c["targets"].items()})
        out[corner] = dict(totals=totals, new_vs_old_merged=brief("new", "old_merged"),
                           old_vs_old_merged=brief("old", "old_merged"),
                           openrcx={k: dict(spef_sha256=v["spef"]["sha256"], peak_rss_kib=v["container_peak_rss_kib"])
                                    for k, v in runs.items()})
    return out


def case(h, run: Path, pdk: str, old: Path, new: Path, routes: Path) -> dict:
    work = run / pdk
    work.mkdir()
    result = dict(pdk=pdk, old=compare_spef.fingerprint(old), new=compare_spef.fingerprint(new))
    result["text"] = text_check(old, new)
    merged = {}
    for kind, d in (("old", old), ("new", new)):
        merged[kind] = compare_spef.merge_def(d, routes, work / f"{kind}_merge_def.def")
    result["merge_def"] = {k: {x: v[x] for x in ("original_wire_paths", "merged_wire_paths", "rect_patches")}
                           for k, v in merged.items()}
    refs = {}
    for kind, d in (("old", old), ("new", new)):
        refs[kind] = work / f"{kind}_reference.gds"
        result.setdefault("streamouts", {})[kind] = stream(d, pdk, refs[kind])
    result["xor"] = xor(refs["new"], refs["old"], pdk, work / "xor_new_vs_old")
    broken = work / "control_deleted_path.def"
    result["control"] = delete_longest_path(new, broken)
    broken_gds = work / "control_deleted_path.gds"
    stream(broken, pdk, broken_gds)
    result["control"]["xor"] = xor(broken_gds, refs["old"], pdk, work / "xor_control")
    result["control"]["status"] = "detected" if result["control"]["xor"]["status"] == "failed" else "missed"
    result["extraction"] = extraction(h, pdk, dict(old=old, old_merged=work / "old_merge_def.def", new=new))
    ok = (result["text"]["status"] == "passed" and result["xor"]["status"] == "passed"
          and result["xor"]["labels"] == "passed" and result["control"]["status"] == "detected")
    result["status"] = "passed" if ok else "failed"
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--run", type=Path, required=True, help="new evidence directory (must not exist)")
    ap.add_argument("--case", action="append", required=True, help="PDK:OLD.def:NEW.def:ROUTES.json")
    ap.add_argument("--evidence", type=Path)
    a = ap.parse_args()
    run = a.run.resolve()
    run.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    h = PhysicalHarness(run, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=30, tolerance_ns=0.001))
    summary = dict(schema="rustroad.def-segmentation.v1", status="unverified", cases=[],
                   scope="same routed workspace; OLD = per-lattice-edge paths, NEW = merged collinear runs")
    try:
        for spec in a.case:
            pdk, old, new, routes = spec.split(":")
            summary["cases"].append(case(h, run, pdk, Path(old).resolve(), Path(new).resolve(),
                                         Path(routes).resolve()))
        statuses = [c["status"] for c in summary["cases"]]
        summary["status"] = "passed" if statuses and all(s == "passed" for s in statuses) else "failed"
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary["status"] = "unverified" if "unverified" in str(error) else "failed"
        summary["reason"] = str(error)
    summary["wall_seconds"] = round(time.monotonic() - started, 2)
    summary["receipts"] = len(h.receipts)
    summary["peak_rss_kib"] = max((x["peak_rss_kib"] for x in h.receipts if x["peak_rss_kib"]), default=None)
    (run / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    if a.evidence:
        a.evidence.write_text(json.dumps(summary, indent=1, sort_keys=True) + "\n")
    print(json.dumps(dict(status=summary["status"], summary=str(run / "summary.json"), reason=summary.get("reason"))))
    return {"passed": 0, "failed": 1, "unverified": 3}[summary["status"]]


if __name__ == "__main__":
    sys.exit(main())
