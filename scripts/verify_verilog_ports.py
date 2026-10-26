#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check header normalization through pinned black-box Yosys on both PDK fixtures.

Two fixtures are capped at4MiB each. Normalization is streaming; canonicalization
and the port-direction negative controls run serially under1GiB/120s tool guards.
This verifies format preservation, not physical or timing correctness.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re

from normalize_verilog_ports import normalize
from verify_logic import Harness, bounded_text, sha256, yosys_path
from verify_physical import run_ok

MAX_FIXTURE = 4 << 20


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--yosys', type=Path, required=True)
    parser.add_argument('--yosys-sha256', required=True)
    parser.add_argument('--sky130', type=Path, required=True)
    parser.add_argument('--gf180', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    yosys = args.yosys.resolve()
    if not yosys.is_file() or sha256(yosys) != args.yosys_sha256:
        print(json.dumps(dict(status='unverified', reason='pinned Yosys missing or changed')))
        return 3
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    rows = []
    for tag in ['sky130', 'gf180']:
        source = getattr(args, tag).resolve()
        if source.stat().st_size > MAX_FIXTURE:
            raise ValueError('fixture byte limit')
        folder = out / tag
        folder.mkdir()
        harness = Harness(folder, argparse.Namespace(threads=1, memory_mib=1024, timeout_seconds=120))

        def canonical(label: str, original: Path) -> Path:
            script, target = folder / (label + '.ys'), folder / (label + '.v')
            script.write_text(f'read_verilog {yosys_path(original)}\nwrite_verilog -noattr {yosys_path(target)}\n')
            run_ok(harness, label, [yosys, '-Q', '-T', '-q', '-s', script])
            if not target.is_file() or not 1 <= target.stat().st_size <= MAX_FIXTURE:
                raise ValueError('canonical output byte limit')
            return target

        before = canonical('before', source)
        normalized = folder / 'normalized.v'
        receipt = normalize(before, normalized, MAX_FIXTURE)
        if receipt['status'] != 'normalized':
            raise ValueError('the reference fixture did not exercise non-ANSI conversion')
        after = canonical('after', normalized)
        if sha256(before) != sha256(after):
            raise ValueError('Yosys canonical circuit changed during normalization')
        negative = folder / 'wrong-direction.v'
        changed, count = re.subn(r'(?m)^  input\b', '  output', bounded_text(normalized, MAX_FIXTURE), count=1)
        if count != 1:
            raise ValueError('fixture has no input for the negative control')
        negative.write_text(changed)
        wrong = canonical('negative', negative)
        if sha256(wrong) == sha256(before):
            raise ValueError('canonical comparison missed the port-direction mutation')
        if sha256(yosys) != args.yosys_sha256:
            raise ValueError('reference executable changed')
        rows.append(dict(pdk=tag, status='passed', source_sha256=sha256(source),
                         normalization=receipt, canonical_sha256=sha256(before),
                         negative_sha256=sha256(wrong), negative_control='different port direction detected',
                         receipts=harness.receipts))
        print(tag + ': passed', flush=True)
    result = dict(status='passed', scope='header format preservation only; no physical or timing claim',
                  yosys=dict(path=str(yosys), sha256=args.yosys_sha256), cases=rows)
    (out / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
