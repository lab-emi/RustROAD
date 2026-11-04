#!/usr/bin/env python3
"""Replay a grouped repair from an independent copy of its initial revision.

Upfront estimate: serial native session <1 GiB; Python controller <512 MiB
(one report decoded at a time). Report/artifact count <=4096, report bytes
<=8 GiB; no graph/path enumeration. A 4 GiB cgroup and wall-clock timeout are
required. The session commits only its last logical revision (summary v2):
reports are compared with the original per-attempt-commit evidence modulo the
compact format (violated rows only) and its input-size work term.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

CHECK = re.compile(r'(?:round-\d+-group-\d+|repair-\d+-before|final-group-\d+)\.json')
REPAIR = re.compile(r'repair-(\d+)\.json')
OTHER = re.compile(r'(?:inventory-(?:initial|final)|model-(?:initial|final))\.json')


def identity(path):
    with path.open('rb') as stream:
        return dict(bytes=path.stat().st_size,
                    sha256=hashlib.file_digest(stream, 'sha256').hexdigest())


def revision_files(path):
    entries = list(path.iterdir())
    if len(entries) > 64 or any(not p.is_file() or p.is_symlink() for p in entries):
        raise ValueError('revision file/count bound')
    return {p.name: identity(p) for p in entries}


def load(path):
    if path.stat().st_size > 128 << 20:
        raise ValueError(f'report byte bound: {path.name}')
    return json.loads(path.read_bytes())


def compare_check(actual, expected, name):
    a, b = load(actual), load(expected)
    if a.get('check_rows') == 'violated':
        b['checks'] = [row for row in b['checks'] if row['violated']]
    elif a.get('check_rows') is not None:
        raise ValueError(f'unknown check_rows: {name}')
    a.pop('check_rows', None)
    for report in (a, b):
        report['stats'].pop('report_memory', None)
    if a != b:
        raise ValueError(f'check report differs: {name}')


def compare_repair(actual, expected, number, reports, baseline):
    a, b = load(actual), load(expected)
    work = []
    for report, directory in ((a, reports), (b, baseline)):
        report['stats'].pop('report_memory', None)
        size = (directory / f'repair-{number}-before.json').stat().st_size
        work.append(report['stats'].pop('antenna_work') - size)
    if a != b or work[0] != work[1]:
        raise ValueError(f'repair report differs: repair-{number}.json')
    return work[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--threads', default='2')
    args = parser.parse_args()
    baseline, binary = args.baseline.resolve(strict=True), args.binary.resolve(strict=True)
    lineage = json.loads((baseline / 'lineage.json').read_bytes())
    source, initial = Path(lineage['workspace']), lineage['initial_revision']
    if re.fullmatch(r'rev[1-9][0-9]{0,3}', initial) is None:
        raise ValueError('unsafe initial revision')
    if len(lineage['attempts']) > 64:
        raise ValueError('replay attempt bound')
    original_reports = baseline / 'native-reports'
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    workspace = output / 'workspace'
    (workspace / 'revisions').mkdir(parents=True)
    original = source / 'revisions' / initial
    shutil.copytree(original, workspace / 'revisions' / initial)
    if revision_files(original) != revision_files(workspace / 'revisions' / initial):
        raise ValueError('initial copied revision differs')
    (workspace / 'HEAD').write_text(initial + '\n')
    (workspace / 'workspace.lock').touch()
    plan = dict(schema='rustroad.antenna-group-repair.v1', output=str(output / 'reports'),
                max_rounds=3, deadline_seconds=1800,
                policy={key: lineage['policy'][key] for key in ('group_names', 'max_groups',
                    'max_attempts', 'max_total_jumpers', 'max_commands', 'max_report_bytes',
                    'max_cumulative_work')})
    plan_path = output / 'plan.json'
    plan_path.write_text(json.dumps(plan, indent=2) + '\n')
    command = [str(binary), '--workspace', str(workspace), '--threads', args.threads,
               '--max-expansions', '8000000', 'repair-antenna-groups', '--base-rev', initial,
               '--plan', str(plan_path)]
    tick = time.monotonic()
    with (output / 'stdout').open('xb') as stdout, (output / 'stderr').open('xb') as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=1800)
    wall_seconds = time.monotonic() - tick
    if result.returncode:
        raise ValueError(f'repair command failed: {result.returncode}; see {output / "stderr"}')
    reports = output / 'reports'
    summary = json.loads((reports / 'summary.json').read_bytes())
    if summary['schema'] != 'rustroad.antenna-group-repair-result.v2':
        raise ValueError('summary schema')
    for key in ('status', 'initial_revision', 'final_revision', 'scope', 'route_scope',
                'inventory_status', 'native_analysis_status', 'observations',
                'whole_layout_antenna_status', 'excluded_macro_touch', 'unresolved_supported_nets',
                'requires_full_revalidation', 'total_jumpers', 'stop_reason',
                'skipped_futile_groups', 'unreported_validation_work_upper_bound'):
        if summary[key] != lineage[key]:
            raise ValueError(f'group decision/accounting differs: {key}')
    if len(summary['attempts']) != len(lineage['attempts']):
        raise ValueError('attempt count differs')
    for actual, expected in zip(summary['attempts'], lineage['attempts'], strict=True):
        for key in ('round', 'group', 'base_revision', 'revision'):
            if actual[key] != expected[key]:
                raise ValueError(f'attempt differs: {key}')
        selected = json.loads(Path(expected['selected']['path']).read_bytes())
        if actual['selected_nets'] != selected:
            raise ValueError('selected repair decision differs')
    names = sorted(p.name for p in original_reports.iterdir()
                   if CHECK.fullmatch(p.name) or REPAIR.fullmatch(p.name) or OTHER.fullmatch(p.name))
    if not 1 <= len(names) <= 4096:
        raise ValueError('baseline report count bound')
    if sorted(p.name for p in reports.iterdir() if p.name != 'summary.json') != names:
        raise ValueError('report set differs')
    input_free_work = 0
    for name in names:
        actual, expected = reports / name, original_reports / name
        if CHECK.fullmatch(name):
            compare_check(actual, expected, name)
        elif match := REPAIR.fullmatch(name):
            input_free_work += compare_repair(actual, expected, int(match.group(1)), reports,
                                              original_reports)
        elif load(actual) != load(expected):
            raise ValueError(f'report differs: {name}')
    final = summary['final_revision']
    if (workspace / 'HEAD').read_text() != final + '\n' or summary['committed_revision'] not in (None, final):
        raise ValueError('final HEAD differs')
    committed = revision_files(workspace / 'revisions' / final)
    reference = revision_files(source / 'revisions' / final)
    for name, record in reference.items():
        if name != 'design.json' and committed.get(name) != record:
            raise ValueError(f'final revision table differs: {name}')
    design = [json.loads((root / 'revisions' / final / 'design.json').read_bytes())
              for root in (workspace, source)]
    parents = [d.pop('parent_revision') for d in design]
    if final != initial and (parents[0] != initial or design[0] != design[1]):
        raise ValueError('final design differs beyond its parent revision')
    receipt = dict(schema='rustroad.antenna-group-repair-replay.v2', status='passed',
                   scope='complete_native_antenna_orchestration_replay', command=command,
                   binary=identity(binary), wall_seconds=wall_seconds,
                   attempts=len(summary['attempts']), logical_revisions=summary['logical_revisions'],
                   groups=summary['groups'], reports=len(names), report_bytes=summary['report_bytes'],
                   committed_revision=committed,
                   final_inventory=identity(reports / 'inventory-final.json'),
                   final_model=identity(reports / 'model-final.json'),
                   whole_layout_antenna_status='unchecked', full_flow_status='unchecked')
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({key: receipt[key] for key in ('status', 'scope', 'wall_seconds', 'attempts',
                                                 'logical_revisions', 'groups')}))


if __name__ == '__main__':
    main()
