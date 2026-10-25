#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Evaluate per-level clock-buffer drive strength for hold closure (both PDKs).

For each variant the tracked routed physical profile is copied into the run
directory with only `clock.level_cells` changed (index 0 drives the sinks);
`base` is the unchanged profile (single clkbuf_1 master). Each variant runs the
bounded closure loop (scripts/hold_closure.py, native SPEF, tt/ss/ff) and the
table records, per corner, the iteration-0 clock-sink latency/slew and worst
hold slack (RustSTA) plus the delay cells, area and iterations the loop needed,
or the exact failure. Profiles in fixtures/ are not modified.

Usage: cts_drive_eval.py --run DIR [--pdk sky130|gf180|all] [hold_closure options...]
Exit 0 when every variant ran to a recorded outcome (closure or explicit failure).
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
VARIANTS = {
    'sky130': {'base': None, 'L24': ['clkbuf_2', 'clkbuf_4'], 'L48': ['clkbuf_4', 'clkbuf_8'],
               'L4': ['clkbuf_4'], 'L816': ['clkbuf_8', 'clkbuf_16']},
    'gf180': {'base': None, 'L24': ['clkbuf_2', 'clkbuf_4'], 'L48': ['clkbuf_4', 'clkbuf_8'],
              'L4': ['clkbuf_4'], 'L816': ['clkbuf_8', 'clkbuf_16'], 'L1220': ['clkbuf_12', 'clkbuf_20']},
}
PREFIX = {'sky130': 'sky130_fd_sc_hd__', 'gf180': 'gf180mcu_fd_sc_mcu7t5v0__'}


def receipt_json(run: Path, label: str):
    path = run / 'receipts' / label / 'stdout.txt'
    return json.loads(path.read_text()) if path.is_file() and path.stat().st_size else None


def iteration0(run: Path, pdk: str) -> dict:
    out = {}
    for c in ('tt', 'ss', 'ff'):
        pins = receipt_json(run, f'{pdk}_i0_sta_clock_pins_{c}')
        eps = receipt_json(run, f'{pdk}_i0_sta_endpoints_{c}')
        if pins is None or eps is None:
            continue
        rows = pins['sections']['pins']['rows']
        out[c] = dict(latency_max_ns=max(r['arrival_late_ns'] for r in rows), slew_max_ns=max(r['slew_late_ns'] for r in rows),
                      worst_hold_ns=min(r['hold_slack_ns'] for r in eps['sections']['endpoints']['rows']))
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--run', type=Path, required=True)
    ap.add_argument('--pdk', choices=('sky130', 'gf180', 'all'), default='all')
    ap.add_argument('--timeout', type=int, default=1200)
    args, extra = ap.parse_known_args()
    root = args.run.resolve()
    root.mkdir(parents=True, exist_ok=False)
    table = []
    for pdk in (('sky130', 'gf180') if args.pdk == 'all' else (args.pdk,)):
        base = json.loads((ROOT / f'fixtures/pdk/{pdk}-routed-physical.json').read_text())
        for name, cells in VARIANTS[pdk].items():
            cmd = [sys.executable, str(ROOT / 'scripts/hold_closure.py'), '--run', str(root / f'{pdk}-{name}'), '--base-rev', 'empty',
                   '--pdk', pdk, *extra]
            if cells is not None:
                cfg = json.loads(json.dumps(base))
                cfg['clock']['level_cells'] = [PREFIX[pdk] + c for c in cells]
                path = root / f'{pdk}-{name}.json'
                path.write_text(json.dumps(cfg, indent=2) + '\n')
                cmd += ['--physical-config', f'{pdk}={path}']
            start = time.monotonic()
            p = subprocess.run(cmd, capture_output=True, text=True, timeout=args.timeout)
            summary = json.loads((root / f'{pdk}-{name}' / 'summary.json').read_text())
            entry = dict(pdk=pdk, variant=name, level_cells=cells or [base['clock']['cell'].split('__')[-1]],
                         exit_code=p.returncode, wall_seconds=round(time.monotonic() - start, 2),
                         iteration0=iteration0(root / f'{pdk}-{name}', pdk))
            r = summary['pdks'][0] if summary['pdks'] else dict(status='failed', reason=summary.get('reason'))
            entry.update(status=r['status'], reason=r.get('reason'))
            failed = re.match(r'(\S+): process exited', r.get('reason') or '')
            if failed:
                err = root / f'{pdk}-{name}' / 'receipts' / failed[1] / 'stderr.txt'
                if err.is_file():
                    try:
                        e = json.loads(err.read_text().strip().splitlines()[-1])
                        entry.update(failure=dict(step=failed[1], code=e.get('code'), message=e.get('message')))
                    except (ValueError, IndexError):
                        entry.update(failure=dict(step=failed[1], stderr_tail=err.read_text()[-300:]))
            if r.get('iterations'):
                plans = [i['hold_eco'] for i in r['iterations'] if 'hold_eco' in i]
                entry.update(iterations=len(r['iterations']), closed=r['closed'],
                             hold_cells=sum(x['totals']['cells'] for x in plans),
                             hold_area_um2=round(sum(x['totals']['area_um2'] for x in plans), 3),
                             cell_types=sorted({c for x in plans for s in x['sinks'] for c in s['cells']}),
                             final={c: dict(worst_setup_ns=v['worst_setup_ns'], worst_hold_ns=v['worst_hold_ns'])
                                    for c, v in r['iterations'][-1]['timing']['corners'].items()})
            else:
                plan = root / f'{pdk}-{name}' / f'{pdk}_i0_hold_plan.json'
                if plan.is_file():
                    pl = json.loads(plan.read_text())
                    entry.update(first_plan=dict(cells=pl['totals']['cells'], area_um2=round(pl['totals']['area_um2'], 3),
                                                 cell_types=sorted({c for s in pl['sinks'] for c in s['cells']})))
            table.append(entry)
            print(json.dumps({k: entry.get(k) for k in ('pdk', 'variant', 'status', 'iterations', 'hold_cells', 'hold_area_um2', 'reason')}))
    (root / 'cts_drive_eval.json').write_text(json.dumps(dict(schema='rustroad.cts-drive-eval.v1', variants=table), indent=1) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
