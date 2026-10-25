#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare two complete rtl2gds gate runs of the same design (independent re-run check).

usage: compare_runs.py --out FILE.json REFERENCE_RUN=OTHER_RUN [...]

For each pair: the overall verdict, every global and per-PDK status, and every
artifact the gate bound by SHA-256 (``<pdk>_binding.json``) must be identical.
Documented normalizations, nothing else: the RustROAD binary (``tool.rustroad``,
built separately: embedded paths differ), OpenRCX SPEF (the wall-clock
``*DATE`` line is removed) and JSON artifacts that record an absolute input
path (the checkout prefix of each run is removed before comparing). The output
is a JSON object ``{"independent_check": ...}`` for ``rtl2gds_evidence.py --extra``.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]


def checkout(run: Path) -> str:
    """The checkout a run used (the scripts path recorded in its summary)."""
    s = json.loads((run / "summary.json").read_text())
    return str(Path(s["design"]["manifest"]).resolve().parents[2]) + "/"


def normalized(path: Path, key: str, prefix: str) -> str:
    data = path.read_bytes()
    if key.startswith("spef.openrcx"):
        data = b"\n".join(x for x in data.split(b"\n") if not x.startswith(b"*DATE"))
    elif path.suffix == ".json":
        data = data.replace(prefix.encode(), b"<checkout>/")
    return hashlib.sha256(data).hexdigest()


def compare(a: Path, b: Path) -> dict:
    sa, sb = (json.loads((x / "summary.json").read_text()) for x in (a, b))
    pa, pb = checkout(a), checkout(b)
    out = dict(reference=a.name, rerun=b.name, design=sa["design"]["name"],
               verdict=[sa["status"], sb["status"]], verdict_identical=sa["status"] == sb["status"],
               global_identical={k: v["status"] for k, v in sa["global_statuses"].items()} ==
               {k: v["status"] for k, v in sb["global_statuses"].items()},
               revisions=[sa["revision"].get("head"), sb["revision"].get("head")], pdks={})
    for x, y in zip(sa["pdks"], sb["pdks"]):
        tag = x["pdk"]
        st = {k: v["status"] for k, v in x["statuses"].items()} == {k: v["status"] for k, v in y["statuses"].items()}
        ba = json.loads((a / f"{tag}_binding.json").read_text())["artifacts"]
        bb = json.loads((b / f"{tag}_binding.json").read_text())["artifacts"]
        raw_same, normalized_same, differ = [], [], []
        for key in sorted(set(ba) | set(bb)):
            if key not in ba or key not in bb:
                differ.append(key)
            elif ba[key]["sha256"] == bb[key]["sha256"]:
                raw_same.append(key)
            elif key == "tool.rustroad":
                normalized_same.append(key)
            elif normalized(Path(ba[key]["path"]), key, pa) == normalized(Path(bb[key]["path"]), key, pb):
                normalized_same.append(key)
            else:
                differ.append(key)
        out["pdks"][tag] = dict(statuses_identical=st, artifacts=len(ba), byte_identical=len(raw_same),
                                identical_after_normalization=normalized_same, different=differ,
                                status="passed" if st and not differ else "failed")
    out["status"] = "passed" if out["verdict_identical"] and out["global_identical"] and \
        all(p["status"] == "passed" for p in out["pdks"].values()) else "failed"
    return out


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("pairs", nargs="+", help="REFERENCE_RUN=OTHER_RUN")
    args = ap.parse_args(argv)
    rows = [compare(Path(a).resolve(), Path(b).resolve()) for a, b in (p.split("=", 1) for p in args.pairs)]
    res = dict(independent_check=dict(rule=__doc__.split("\n\n")[1].replace("\n", " "), pairs=rows,
                                      status="passed" if all(r["status"] == "passed" for r in rows) else "failed"))
    args.out.write_text(json.dumps(res, indent=1) + "\n")
    print(json.dumps({r["design"]: r["status"] for r in rows}))
    return 0 if res["independent_check"]["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
