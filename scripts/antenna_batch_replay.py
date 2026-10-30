#!/usr/bin/env python3
"""Replay immutable final-group evidence through one selected-check batch.

Memory estimate: one <=128 MiB JSON group, its decoded Python value (<768 MiB),
one expected <=128 MiB report, and regex scratch (<256 MiB); conservatively <2 GiB.
The native child is serial (<1 GiB expected). Run under a 4 GiB cgroup and timeout.
No graph/path enumeration. This verifies same-revision checking, not B3 repair.
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

MAX_REPORT = 128 << 20
TOKEN = re.compile(rb'"(?:[^"\\]|\\.)*"|\s+')


def identity(path):
    with path.open('rb') as stream:
        return dict(bytes=path.stat().st_size,
                    sha256=hashlib.file_digest(stream, 'sha256').hexdigest())


def compact(raw):
    # Preserve every string and number token, field order and array order.
    return TOKEN.sub(lambda match: match[0] if match[0].startswith(b'"') else b'', raw)


def reports(path):
    decoder = json.JSONDecoder()
    with path.open() as stream:
        buffer = stream.read(1 << 20)
        if not buffer.startswith('['):
            raise ValueError('missing batch array')
        buffer = buffer[1:]
        while True:
            buffer = buffer.lstrip()
            if buffer.startswith(']'):
                if (buffer[1:] + stream.read()).strip():
                    raise ValueError('trailing batch bytes')
                return
            while True:
                try:
                    value, end = decoder.raw_decode(buffer)
                    del value
                    break
                except json.JSONDecodeError:
                    chunk = stream.read(1 << 20)
                    if not chunk or len(buffer.encode()) + len(chunk.encode()) > MAX_REPORT:
                        raise ValueError('batch report byte/parse limit')
                    buffer += chunk
            yield buffer[:end].encode()
            buffer = buffer[end:].lstrip()
            if not buffer:
                buffer = stream.read(1 << 20).lstrip()
            if buffer.startswith(','):
                buffer = buffer[1:]
            elif not buffer.startswith(']'):
                raise ValueError('invalid report delimiter')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True,
                        help='frozen antenna-repair output directory')
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    baseline, binary = args.baseline.resolve(strict=True), args.binary.resolve(strict=True)
    lineage = json.loads((baseline / 'lineage.json').read_bytes())
    revision = lineage['final_revision']
    source = Path(lineage['workspace'])
    expected = lineage['final_groups']
    if not 1 <= len(expected) <= 1024:
        raise ValueError('group count limit')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    workspace = out / 'workspace'
    (workspace / 'revisions').mkdir(parents=True)
    shutil.copytree(source / 'revisions' / revision, workspace / 'revisions' / revision)
    (workspace / 'HEAD').write_text(revision + '\n')
    (workspace / 'workspace.lock').touch()
    original = {p.name: identity(p) for p in (source / 'revisions' / revision).iterdir()}
    copied = {p.name: identity(p) for p in (workspace / 'revisions' / revision).iterdir()}
    if copied != original:
        raise ValueError('copied revision identity differs')
    groups = [json.loads(Path(row['selection']['path']).read_bytes()) for row in expected]
    first = json.loads(Path(expected[0]['header']['path']).read_bytes())
    plan = dict(schema='rustroad.antenna-check-batch.v1', revision=revision, groups=groups,
                analysis=first['selection']['limits'], safety_margin=0.1,
                max_work=200_000_000_000, max_output_bytes=8 << 30,
                snapshot_memory_bytes=2 << 30)
    plan_path = out / 'plan.json'
    plan_path.write_text(json.dumps(plan, indent=2) + '\n')
    command = [str(binary), '--workspace', str(workspace), '--threads', '2',
               '--max-expansions', '8000000', 'check-antenna-batch', '--plan', str(plan_path)]
    tick = time.monotonic()
    with (out / 'reports.json').open('xb') as stdout, (out / 'stderr').open('xb') as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=600)
    seconds = time.monotonic() - tick
    if result.returncode:
        raise ValueError(f'batch command failed: {result.returncode}; see {out / "stderr"}')
    count, bindings = 0, []
    for count, (actual, row) in enumerate(zip(reports(out / 'reports.json'), expected, strict=True), 1):
        path = Path(row['report']['path'])
        if identity(path) != {k: row['report'][k] for k in ('sha256', 'bytes')}:
            raise ValueError('baseline report identity changed')
        raw = path.read_bytes()
        if len(raw) > MAX_REPORT or compact(raw) != actual:
            raise ValueError(f'legacy report tokens differ: group {count - 1}')
        bindings.append(dict(group=count - 1, baseline=row['report']['sha256'],
                             compact_sha256=hashlib.sha256(actual).hexdigest(),
                             compact_bytes=len(actual)))
    after = {p.name: identity(p) for p in (workspace / 'revisions' / revision).iterdir()}
    if after != original or (workspace / 'HEAD').read_text() != revision + '\n':
        raise ValueError('read-only batch modified copied workspace')
    receipt = dict(schema='rustroad.antenna-batch-replay.v1', status='passed',
                   scope='same_revision_selected_checks_only', groups=count,
                   binary=identity(binary), command=command, wall_seconds=seconds,
                   reports=identity(out / 'reports.json'), revision_files=after,
                   exact_report_tokens=bindings, repair_orchestrator_status='unchecked',
                   whole_layout_antenna_status='unchecked')
    (out / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({k: receipt[k] for k in ('status', 'scope', 'groups', 'wall_seconds')}))


if __name__ == '__main__':
    main()
