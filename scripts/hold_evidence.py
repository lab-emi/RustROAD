#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Condense a hold_closure.py run (+ optional cts_drive_eval.py run) into
docs/evidence/hold-closure.json (schema rustroad.hold-closure-evidence.v1).

Only numbers and verdicts from the run files are copied; nothing is recomputed.
Usage: hold_evidence.py --summary RUN/summary.json [--cts CTS/cts_drive_eval.json]
                        [--time RUN.time] --out docs/evidence/hold-closure.json
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re


def rounded(v, n=6):
    return round(v, n) if isinstance(v, float) else v


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--summary', type=Path, required=True)
    ap.add_argument('--cts', type=Path)
    ap.add_argument('--time', type=Path)
    ap.add_argument('--out', type=Path, required=True)
    a = ap.parse_args()
    s = json.loads(a.summary.read_text())
    out = dict(schema='rustroad.hold-closure-evidence.v1', status=s['status'], run=str(a.summary.parent),
               parameters=s['parameters'], scope=s['scope'], identity_stable=s['identity_stable'],
               tools={k: dict(sha256=v['sha256'], path=v['path']) for k, v in s['tools'].items()},
               receipts=s['receipts'], wall_seconds=s['wall_seconds'], peak_child_rss_kib=s['peak_rss_kib'], pdks=[])
    if a.time and a.time.is_file():
        t = a.time.read_text()
        out['driver'] = dict(wall=re.search(r'Elapsed \(wall clock\) time.*?: (\S+)', t)[1],
                             peak_rss_kib=int(re.search(r'Maximum resident set size \(kbytes\): (\d+)', t)[1]))
    for p in s['pdks']:
        e = dict(pdk=p['pdk'], status=p['status'], gates=p.get('gates'), closed=p.get('closed'), iterations=[])
        for it in p.get('iterations', []):
            r = dict(iteration=it['iteration'], netlist_sha256=it['netlist']['sha256'],
                     def_sha256=it['physical']['files']['def']['sha256'],
                     routing=dict(wires=it['physical']['wires'], vias=it['physical']['vias'], patches=it['physical']['patches'],
                                  routed_nets=it['physical']['routed_nets']),
                     timing={c: dict(clock_latency_max_ns=rounded(v['clock']['latency_max_ns']), clock_slew_max_ns=rounded(v['clock']['slew_max_ns']),
                                     worst_setup_ns=rounded(v['worst_setup_ns']), worst_hold_ns=rounded(v['worst_hold_ns']),
                                     hold_below_margin=v['hold_below_margin'], setup_violations=v['setup_violations'])
                             for c, v in it['timing']['corners'].items()},
                     drv_status=it['timing']['metrics'].get('drv_status'),
                     antenna=dict(status=it['antenna']['status'], violations=it['antenna']['violations'], violated=it['antenna']['violated'],
                                  worst=it['antenna']['worst'][:1]))
            if 'hold_eco' in it:
                r['hold_eco'] = dict(totals=it['hold_eco']['totals'],
                                     sinks=[dict(endpoint=x['endpoint'], cells=x['cells'], existing_chain=x['existing_chain'],
                                                 need_ns={c: rounded(v['need_ns']) for c, v in x['corners'].items()},
                                                 estimated_hold_slack_ns={c: rounded(v['estimated_hold_slack_ns']) for c, v in x['corners'].items()},
                                                 estimated_setup_slack_ns=rounded(x['estimated_setup_slack_ns'])) for x in it['hold_eco']['sinks']])
            if 'antenna_eco' in it:
                r['antenna_eco'] = it['antenna_eco']
            e['iterations'].append(r)
        if 'opensta_native' in p:
            o = p['opensta_native']
            e['opensta_native'] = dict(status=o['status'], max_difference_ps=rounded(o['max_difference_ns'] * 1e3, 4),
                                       comparisons=o['comparisons'], opensta_closed=o['opensta_closed'],
                                       opensta_worst={c: {k: rounded(v) for k, v in w.items()} for c, w in o['opensta_worst'].items()},
                                       clock_latency_ns=o.get('clock_latency_ns'))
        for k in ('equivalence', 'frozen_inputs', 'drc_not_worse'):
            if k in p:
                e[k] = p[k] if k != 'frozen_inputs' else dict(status=p[k]['status'], changed=p[k]['changed'],
                                                               files={n: v['sha256'] for n, v in p[k]['files'].items()})
        if 'cell_proofs' in p:
            c = p['cell_proofs']
            e['cell_proofs'] = dict(status=c['status'], cells=c['cells'], clock_cells=c.get('clock_cells'),
                                    proofs={k: dict(truth_table=v['truth_table'], wrong_polarity=v['wrong_polarity']) for k, v in c['proofs'].items()},
                                    native_wrong_polarity=c['native_wrong_polarity'], native_positive_control=c['native_positive_control'])
        for k in ('drc_before', 'drc_after'):
            if k in p:
                e[k] = dict(statuses=p[k]['statuses'], counts=p[k]['counts'], by_rule=p[k]['by_rule'], gds_sha256=p[k]['gds']['sha256'])
        if 'antenna_controls' in p:
            c = p['antenna_controls']
            e['antenna_controls'] = dict(status=c['status'], reason=c.get('reason'))
            if 'shape' in c:
                e['antenna_controls'].update(
                    planted=dict(net=c['shape']['net'], layer=c['shape']['layer'], rect_dbu=c['shape']['rect'], length_um=c['shape']['length_um']),
                    native_planted=dict(status=c['native_planted']['status'], violated=c['native_planted']['violated']),
                    deck_planted=c['deck_planted'], antenna_eco=c['antenna_eco'],
                    native_repaired=dict(status=c['native_repaired']['status'], worst=c['native_repaired']['worst']),
                    native_repaired_unplanted=dict(status=c['native_repaired_unplanted']['status']),
                    deck_repaired=c['deck_repaired'], repaired_timing=c['repaired_timing'])
        if 'openrcx_merged' in p:
            x = p['openrcx_merged']
            e['openrcx_merged_reported'] = dict(status=x['status'], reason=x.get('reason'))
            if 'opensta' in x:
                e['openrcx_merged_reported'].update(
                    ruststa_closed=x['ruststa']['closed'], opensta_closed=x['opensta']['opensta_closed'],
                    opensta_worst={c: {k: rounded(v) for k, v in w.items()} for c, w in x['opensta']['opensta_worst'].items()},
                    parity_ps=rounded(x['opensta']['max_difference_ns'] * 1e3, 4), dropped_patch_lines=x['dropped_patch_lines'], note=x['note'])
        e['wall_seconds'] = p.get('wall_seconds')
        e['reason'] = p.get('reason')
        out['pdks'].append(e)
    if a.cts:
        c = json.loads(a.cts.read_text())
        out['cts_drive_evaluation'] = [dict(pdk=v['pdk'], variant=v['variant'], level_cells=v['level_cells'], status=v['status'],
                                            iteration0={k: {m: rounded(x) for m, x in w.items()} for k, w in v['iteration0'].items()},
                                            iterations=v.get('iterations'), hold_cells=v.get('hold_cells'), hold_area_um2=v.get('hold_area_um2'),
                                            cell_types=v.get('cell_types'), first_plan=v.get('first_plan'), reason=v.get('reason'),
                                            failure=v.get('failure'))
                                       for v in c['variants']]
    a.out.write_text(json.dumps(out, indent=1) + '\n')
    print(a.out)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
