#!/usr/bin/env python3
"""Replay the frozen grouped repair from an independent copied initial workspace.

Upfront estimate: serial native session <1.5 GiB; Python controller <256 MiB
while native runs, streaming SHA verification afterwards. Report/artifact count
<=4096, report bytes <=8 GiB; no graph/path enumeration. A 4 GiB cgroup and
wall-clock timeout are required. Preserve the original full revision ancestry.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import time

REPORT = re.compile(r'(?:round-\d+-group-\d+|repair-\d+(?:-before)?|final-group-\d+|inventory-(?:initial|final)|model-(?:initial|final))\.json')


def identity(path):
    with path.open('rb') as stream:
        return dict(bytes=path.stat().st_size,
                    sha256=hashlib.file_digest(stream, 'sha256').hexdigest())


def revision_files(path):
    entries = list(path.iterdir())
    if len(entries) > 64 or any(not p.is_file() or p.is_symlink() for p in entries):
        raise ValueError('revision file/count bound')
    return {p.name: identity(p) for p in entries}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', required=True, type=Path)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    baseline, binary = args.baseline.resolve(strict=True), args.binary.resolve(strict=True)
    lineage = json.loads((baseline / 'lineage.json').read_bytes())
    source, initial = Path(lineage['workspace']), lineage['initial_revision']
    if re.fullmatch(r'rev[1-9][0-9]{0,3}', initial) is None:
        raise ValueError('unsafe initial revision')
    initial_number = int(initial[3:])
    if initial_number > 128 or len(lineage['attempts']) > 64:
        raise ValueError('replay revision/attempt bound')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    workspace = output / 'workspace'
    (workspace / 'revisions').mkdir(parents=True)
    for number in range(1, initial_number + 1):
        revision = f'rev{number}'
        original = source / 'revisions' / revision
        target = workspace / 'revisions' / revision
        shutil.copytree(original, target)
        if revision_files(original) != revision_files(target):
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
    command = [str(binary), '--workspace', str(workspace), '--threads', '2',
               '--max-expansions', '8000000', 'repair-antenna-groups', '--base-rev', initial,
               '--plan', str(plan_path)]
    tick = time.monotonic()
    with (output / 'stdout').open('xb') as stdout, (output / 'stderr').open('xb') as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=1800)
    wall_seconds = time.monotonic() - tick
    if result.returncode:
        raise ValueError(f'repair command failed: {result.returncode}; see {output / "stderr"}')
    summary = json.loads((output / 'reports/summary.json').read_bytes())
    for key in ('status', 'initial_revision', 'final_revision', 'scope', 'route_scope',
                'inventory_status', 'native_analysis_status', 'observations',
                'whole_layout_antenna_status', 'excluded_macro_touch', 'unresolved_supported_nets',
                'requires_full_revalidation', 'total_jumpers', 'stop_reason',
                'skipped_futile_groups', 'reported_work', 'unreported_validation_work_upper_bound',
                'work_budget_charge'):
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
    paths = sorted(path for path in baseline.iterdir() if REPORT.fullmatch(path.name))
    if not 1 <= len(paths) <= 4096 or sum(path.stat().st_size for path in paths) > 8 << 30:
        raise ValueError('baseline report count/byte bound')
    report_bindings = {}
    for expected in paths:
        actual = output / 'reports' / expected.name
        report_bindings[expected.name] = identity(expected)
        if identity(actual) != report_bindings[expected.name]:
            raise ValueError(f'complete report bytes differ: {expected.name}')
    revisions = {}
    for revision in sorted({row['revision'] for row in summary['attempts']}):
        actual = revision_files(workspace / 'revisions' / revision)
        if actual != revision_files(source / 'revisions' / revision):
            raise ValueError(f'revision side-table bytes differ: {revision}')
        revisions[revision] = actual
    if (workspace / 'HEAD').read_text() != lineage['final_revision'] + '\n':
        raise ValueError('final HEAD differs')
    receipt = dict(schema='rustroad.antenna-group-repair-replay.v1', status='passed',
                   scope='complete_native_antenna_orchestration_replay', command=command,
                   binary=identity(binary), wall_seconds=wall_seconds,
                   attempts=len(summary['attempts']), logical_changed_revisions=len(revisions),
                   groups=summary['groups'], reports=report_bindings, revisions=revisions,
                   final_design=identity(workspace / 'revisions' / lineage['final_revision'] / 'design.json'),
                   final_inventory=identity(output / 'reports/inventory-final.json'),
                   final_model=identity(output / 'reports/model-final.json'),
                   whole_layout_antenna_status='unchecked', full_flow_status='unchecked')
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({key: receipt[key] for key in ('status', 'scope', 'wall_seconds', 'attempts',
                                                 'logical_changed_revisions', 'groups')}))


if __name__ == '__main__':
    main()
