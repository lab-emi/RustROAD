#!/usr/bin/env python3
"""Exercise the independent DEF checker against a real export and planted defects.

Uses a fresh evidence workspace only; all source files remain unchanged.
"""
import argparse
import fcntl
import hashlib
import json
from pathlib import Path
import re
import sys
from check_def import check, read_bounded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--def", dest="def_path", type=Path, required=True)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--base-rev", required=True)
    args = parser.parse_args()
    if args.base_rev != "empty":
        parser.error("REVISION_CONFLICT: expected --base-rev empty")
    args.workspace.mkdir(parents=True, exist_ok=False)
    with (args.workspace / "workspace.lock").open("wb") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        (args.workspace / "HEAD").write_text("rev0\n")
        original, design = read_bounded(args.def_path), read_bounded(args.input)
        source = args.workspace / "physical.json"
        source.write_text(design)
        cases = {"good": original}
        matches = list(re.finditer(r"(?m)^- \S+ \S+ \+ PLACED \( (-?\d+) (-?\d+) \) N ;$", original))
        if len(matches) < 2:
            parser.error("controls require at least two placed cells")
        first, second = matches[:2]
        replacement = original[second.start():second.end()]
        cases["overlap"] = original[:second.start()] + re.sub(r"\( -?\d+ -?\d+ \)", f"( {first[1]} {first[2]} )", replacement) + original[second.end():]
        cases["off_grid"] = original[:first.start(1)] + str(int(first[1]) + 1) + original[first.end(1):]
        cases["duplicate"] = original[:second.start()] + original[first.start():first.end()] + original[second.end():]
        cases["missing"] = original[:first.start()] + original[first.end():]
        cases["bad_master"] = original[:first.start()] + re.sub(r"^(- \S+ )\S+", r"\1UNRECOGNIZED", original[first.start():first.end()]) + original[first.end():]
        cases["unsupported"] = original.replace("END DESIGN", "NETS 0 ;\nEND NETS\nEND DESIGN")
        result = {"schema": "rustroad.def-controls.v1", "cases": [], "input_sha256": hashlib.sha256(design.encode()).hexdigest(),
                  "def_sha256": hashlib.sha256(original.encode()).hexdigest(), "drc_status": "unchecked", "lvs_status": "unchecked"}
        for name, text in cases.items():
            path = args.workspace / (name + ".def")
            path.write_text(text)
            try:
                observed = check(source, path)
                rejected = False
            except (ValueError, KeyError, TypeError) as error:
                observed = {"status": "failed", "diagnostic": str(error)}
                rejected = True
            result["cases"].append(dict(name=name, expected_rejection=name != "good", observed=observed,
                                        matched_expectation=rejected == (name != "good")))
        result["status"] = "passed" if all(c["matched_expectation"] for c in result["cases"]) else "failed"
        (args.workspace / "summary.json").write_text(json.dumps(result, indent=2) + "\n")
        (args.workspace / "HEAD").write_text("rev1\n")
        print(json.dumps(result, indent=2))
        return 0 if result["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
