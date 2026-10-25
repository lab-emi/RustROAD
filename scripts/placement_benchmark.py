#!/usr/bin/env python3
"""Bounded end-to-end CPU placement benchmark, including independent legality.

Run under /usr/bin/time -v and an OS memory/time guard; see docs/memory-budget.md.
Writes only to a new --output directory. No timing/DRC/signoff claim is inferred.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import time


def digest_file(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cells", type=int, default=50_000)
    args = parser.parse_args()
    if not 1 <= args.cells <= 50_000:
        parser.error("--cells must be in 1..50000")
    resource.setrlimit(resource.RLIMIT_AS, (4 * 1024**3, 4 * 1024**3))
    resource.setrlimit(resource.RLIMIT_CPU, (120, 120))
    resource.setrlimit(resource.RLIMIT_FSIZE, (64 * 1024**2, 64 * 1024**2))
    binary = args.binary.resolve(strict=True)
    root = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    rows = (args.cells + 19) // 20
    design = {
        "name": "scale_cpu_ref", "units_per_micron": 1000,
        "die_area": {"lx": 0, "ly": 0, "ux": 12000, "uy": (rows + 2) * 1000},
        "core_area": {"lx": 1000, "ly": 1000, "ux": 11000, "uy": (rows + 1) * 1000},
        "site": {"name": "rustroad_site", "width": 500, "height": 1000},
        "rows": [{"x_start": 1000, "x_end": 11000, "y": (i + 1) * 1000} for i in range(rows)],
        "cells": [{"name": f"u{i}", "master": "CELL1", "width": 500, "height": 1000} for i in range(args.cells)],
    }
    input_path = output / "input.json"
    input_path.write_text(json.dumps(design, separators=(",", ":")) + "\n")
    del design
    measurements = []
    for threads in (1, 2):
        work = output / f"threads{threads}"
        def_path = output / f"threads{threads}.def"
        common = [str(binary), "--threads", str(threads), "--workspace", str(work)]
        env = dict(os.environ, RAYON_NUM_THREADS=str(threads))
        start = time.perf_counter()
        for command in ([*common, "init", "--base-rev", "rev0", "--input", str(input_path)],
                        [*common, "place", "--base-rev", "rev1"]):
            name = f"threads{threads}-{command[len(common)]}"
            with (output / f"{name}.json").open("w") as stdout, (output / f"{name}.stderr").open("w") as stderr:
                subprocess.run(command, stdout=stdout, stderr=stderr, timeout=30, env=env, check=True)
        with def_path.open("w") as stream, (output / f"threads{threads}-export.stderr").open("w") as stderr:
            subprocess.run([*common, "export-def"], stdout=stream, stderr=stderr, timeout=30, env=env, check=True)
        checked_path = output / f"threads{threads}-check.json"
        with checked_path.open("w") as stdout, (output / f"threads{threads}-check.stderr").open("w") as stderr:
            subprocess.run(["python3", str(root / "scripts/check_def.py"), "--input", str(input_path), "--def", str(def_path)], stdout=stdout, stderr=stderr, timeout=30, check=True)
        duration = time.perf_counter() - start
        if checked_path.stat().st_size > 1024**2:
            raise RuntimeError("checker report exceeds 1 MiB")
        verdict = json.loads(checked_path.read_text())
        if verdict.get("status") != "passed":
            raise RuntimeError("independent legality did not pass")
        digest = digest_file(def_path)
        measurements.append({"threads": threads, "wall_seconds": duration, "def_sha256": digest,
                             "legality": verdict, "scope": "init + place + mmap/export + independent check"})
    if measurements[0]["def_sha256"] != measurements[1]["def_sha256"]:
        raise RuntimeError("one/two-thread output differs")
    result = {"schema": "rustroad.placement_benchmark.v1", "cells": args.cells, "rows": rows,
              "binary_sha256": digest_file(binary),
              "measurements": measurements, "deterministic_across_threads": True,
              "reference_performance_comparison": "not_run", "gpu": "not_run",
              "drc_status": "unchecked", "lvs_status": "unchecked", "routing_status": "not_run",
              "corner": "not_applicable", "wire_model": "none", "unconstrained_endpoints": None}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
