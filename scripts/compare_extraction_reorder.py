#!/usr/bin/env python3
"""Bounded comparison of native SPEFs whose accumulation order may differ.

Record identities, order, units and connectivity must stay identical. Only
D_NET totals and CAP values may differ within the requested tolerance; RES
values must stay numerically identical. Native SPEF uses six significant digits:
crossing a rounding boundary can change its last digit (up to 1e-5 relative)
even when the underlying f64 change is far smaller. This compares two outputs,
not an EDA oracle. Counts refer to records (couplings occur in both incident nets).

Memory estimate: two lines <=64 KiB each, hashes, four counters and at most 16
examples; <32 MiB including Python. Each input is capped at 512 MiB and ten
million lines. No design graph or collection of paths is materialized.
"""
from __future__ import annotations

import argparse
from decimal import Decimal, InvalidOperation, localcontext
import hashlib
import itertools
import json
from pathlib import Path

MAX_BYTES = 512 << 20
MAX_LINES = 10_000_000
MAX_LINE_BYTES = 64 << 10
MAX_EXAMPLES = 16


class SpefLines:
    def __init__(self, path: Path, max_bytes: int, max_lines: int):
        self.path = path
        self.max_bytes = max_bytes
        self.max_lines = max_lines
        self.bytes = 0
        self.lines = 0
        self.sha = hashlib.sha256()

    def __iter__(self):
        if self.path.stat().st_size > self.max_bytes:
            raise ValueError(f"SPEF_BYTES: {self.path}")
        with self.path.open("rb") as stream:
            while raw := stream.readline(MAX_LINE_BYTES + 1):
                self.bytes += len(raw)
                self.lines += 1
                if len(raw) > MAX_LINE_BYTES:
                    raise ValueError(f"SPEF_LINE_BYTES: {self.path}:{self.lines}")
                if self.bytes > self.max_bytes or self.lines > self.max_lines:
                    raise ValueError(f"SPEF_BUDGET: {self.path}:{self.lines}")
                self.sha.update(raw)
                yield raw.decode("utf-8").split()

    def identity(self):
        return dict(path=str(self.path.resolve()), bytes=self.bytes, lines=self.lines,
                    sha256=self.sha.hexdigest())


def numeric_record(tokens, section):
    if not tokens:
        return section, None
    head = tokens[0]
    if head == "*D_NET":
        if len(tokens) != 3:
            raise ValueError("SPEF_FORMAT: malformed D_NET")
        return None, "net_total_pf"
    if head in ("*CAP", "*RES"):
        return head, None
    if head.startswith("*"):
        return None, None
    if section in ("*CAP", "*RES"):
        if not head.isdecimal() or len(tokens) not in ((3, 4) if section == "*CAP" else (4,)):
            raise ValueError(f"SPEF_FORMAT: unsupported {section} record")
        if section == "*RES":
            return section, "resistance_ohm"
        return section, "ground_pf" if len(tokens) == 3 else "coupling_pf"
    return section, None


def value(token):
    if len(token) > 64:
        raise ValueError("SPEF_VALUE: oversized numeric token")
    try:
        result = Decimal(token)
    except InvalidOperation as error:
        raise ValueError("SPEF_VALUE: invalid numeric token") from error
    if not result.is_finite() or abs(result.adjusted()) > 100:
        raise ValueError("SPEF_VALUE: non-finite or out-of-range value")
    return result


def compare(baseline: Path, candidate: Path, *, relative=Decimal("1e-5"),
            absolute_pf=Decimal("1e-14"), max_bytes=MAX_BYTES, max_lines=MAX_LINES):
    if not relative.is_finite() or not absolute_pf.is_finite() or relative < 0 or absolute_pf < 0:
        raise ValueError("TOLERANCE: finite non-negative tolerances required")
    if not 0 < max_bytes <= MAX_BYTES or not 0 < max_lines <= MAX_LINES:
        raise ValueError("SPEF_BUDGET: limits outside hard bounds")
    sources = [SpefLines(p, max_bytes, max_lines) for p in (baseline, candidate)]
    kinds = ("net_total_pf", "ground_pf", "coupling_pf", "resistance_ohm")
    stats = {kind: dict(compared=0, changed=0, outside_tolerance=0,
                        max_absolute=Decimal(0), max_relative=Decimal(0)) for kind in kinds}
    structural = 0
    examples = []
    sections = [None, None]
    with localcontext() as context:
        context.prec = 80
        for line, pair in enumerate(itertools.zip_longest(*sources), 1):
            kinds_at = []
            for side, tokens in enumerate(pair):
                sections[side], kind = numeric_record(tokens, sections[side])
                kinds_at.append(kind)
            left, right = pair
            kind = kinds_at[0]
            same_record = (left is not None and right is not None and kind is not None
                           and kinds_at[1] == kind and left[:-1] == right[:-1])
            if not same_record:
                if left != right or kinds_at[0] != kinds_at[1]:
                    structural += 1
                    if len(examples) < MAX_EXAMPLES:
                        examples.append(dict(line=line, kind="structure", baseline=left, candidate=right))
                continue
            a, b = value(left[-1]), value(right[-1])
            row = stats[kind]
            row["compared"] += 1
            delta = abs(a - b)
            scale = max(abs(a), abs(b))
            rel = delta / scale if scale else Decimal(0)
            row["max_absolute"] = max(row["max_absolute"], delta)
            row["max_relative"] = max(row["max_relative"], rel)
            if delta == 0:
                continue
            row["changed"] += 1
            allowed = Decimal(0) if kind == "resistance_ohm" else absolute_pf + relative * scale
            row["outside_tolerance"] += int(delta > allowed)
            if len(examples) < MAX_EXAMPLES:
                examples.append(dict(line=line, kind=kind, identity=left[:-1],
                                     baseline=left[-1], candidate=right[-1],
                                     absolute=str(delta), relative=str(rel)))
    if stats["net_total_pf"]["compared"] == 0:
        raise ValueError("SPEF_FORMAT: no paired D_NET records")
    for row in stats.values():
        row["max_absolute"] = str(row["max_absolute"])
        row["max_relative"] = str(row["max_relative"])
    passed = structural == 0 and not any(r["outside_tolerance"] for r in stats.values())
    return dict(schema="rustroad.extraction-reorder-comparison.v1", status="passed" if passed else "failed",
                scope="same-native-format output comparison; not physical-model qualification",
                baseline=sources[0].identity(), candidate=sources[1].identity(),
                tolerance=dict(relative=str(relative), absolute_capacitance_pf=str(absolute_pf),
                               resistance_ohm="0"),
                structural_differences=structural, values=stats, examples=examples,
                limits=dict(max_bytes=max_bytes, max_lines=max_lines, max_line_bytes=MAX_LINE_BYTES,
                            max_examples=MAX_EXAMPLES))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--relative", type=Decimal, default=Decimal("1e-5"))
    parser.add_argument("--absolute-pf", type=Decimal, default=Decimal("1e-14"))
    parser.add_argument("--max-bytes", type=int, default=MAX_BYTES)
    parser.add_argument("--max-lines", type=int, default=MAX_LINES)
    args = parser.parse_args()
    if args.out.resolve() in (args.baseline.resolve(), args.candidate.resolve()):
        parser.error("output must be separate from the compared inputs")
    try:
        report = compare(args.baseline, args.candidate, relative=args.relative,
                         absolute_pf=args.absolute_pf, max_bytes=args.max_bytes, max_lines=args.max_lines)
    except (OSError, UnicodeError, ValueError) as error:
        parser.exit(2, f"{error}\n")
    args.out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: report[key] for key in ("status", "structural_differences", "values")}))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
