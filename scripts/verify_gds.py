#!/usr/bin/env python3
"""Black-box acceptance of RustROAD native GDSII streamout on SKY130 and GF180.

For each PDK the routed RustROAD workspace of a finished flow run is COPIED
(the source run is only read, and its tree hash is re-checked at the end) and:

1. streamed twice with `rustroad export-gds` (read-only: HEAD and every file of
   the workspace copy must be unchanged; both streams must be byte-identical);
2. compared with an independent reference stream of the same routed DEF made
   by KLayout's LEF/DEF reader (strmxor must report no differences; top-level
   label sets and flattened text multisets must be equal);
3. checked for verbatim, byte-identical copies of the PDK cell structures and
   for every routing patch of the DEF (`RECT`) as a rectangle on its layer's
   net purpose; deleting those rectangles must make the XOR fail, which shows
   that the reference reader draws DEF routing RECTs;
4. checked by Magic drc(full) and the foundry KLayout DRC deck (counts per
   rule, all cells) on native and reference: both must report 0, and a planted
   sub-minimum square must raise both counts; the density decks are run and
   reported separately (owner decision: density is never folded into DRC);
5. checked by Magic extraction + Netgen LVS and by KLayout LVS against the
   workspace's powered netlist, on native and reference;
6. exercised with negative controls: deleted wire, wrong layer-map entry,
   missing cell structure (in the output and in the cell library) and an odd
   GF180 coordinate. The layer-map citations are re-verified against the PDK.

Third-party tools run only as black boxes inside the pinned signoff image.
Exit codes are never trusted alone: reports are parsed and positive controls
(planted DRC defect, deleted patches, missing-cell guards) are required. A missing tool or image
makes the status `unverified`, never `passed`. Resource contract:
docs/gds-budget.md. Scope: block level; no pad ring or seal ring.
"""
from verify_logic import gnu_time_program

import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
ORACLES = ROOT / 'scripts' / 'gds_oracles'
sys.path.insert(0, str(ROOT / 'scripts' / 'signoff'))
import signoff as signoff_harness  # noqa: E402
MANIFEST = ROOT / 'fixtures/flow/signoff-pinned.json'
IMAGE = '4c48415d8bd08c04033f4777b65a96299e22c1e94d12a8ea1d9a062697c006ee'
HOST_PDK = Path('/home/cgao/.local/share/containers/storage/overlay/'
                'a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel')
HOST_ROOTS = {'sky130A': HOST_PDK / 'sky130/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/sky130A',
              'gf180mcuD': HOST_PDK / 'gf180mcu/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/gf180mcuD'}
CONTAINER_PDK = '/foss/pdks'
TOP = 'lane_smoke'

PDKS = {
    'sky130': dict(
        variant='sky130A', library='sky130_fd_sc_hd', workspace='sky130_physical', def_name='sky130.def',
        cell_gds='libs.ref/sky130_fd_sc_hd/gds/sky130_fd_sc_hd.gds',
        tech_lef='libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef',
        cell_lef='libs.ref/sky130_fd_sc_hd/lef/sky130_fd_sc_hd.lef',
        klayout_map='libs.tech/klayout/tech/sky130A.map',
        magicrc='libs.tech/magic/sky130A.magicrc',
        netgen_setup='libs.tech/netgen/sky130A_setup.tcl',
        cell_spice='libs.ref/sky130_fd_sc_hd/spice/sky130_fd_sc_hd.spice',
        drc_deck='libs.tech/klayout/drc/sky130A_mr.drc',
        density_deck='libs.tech/klayout/drc/met_min_ca_density.lydrc',
        lvs_deck='libs.tech/klayout/lvs/sky130.lvs',
        layer_map=ROOT / 'fixtures/pdk/sky130-gds-map.json',
        wire_layer=(69, 20), wrong_map=('met2', 'net', [69, 21]),
        missing_cell='sky130_fd_sc_hd__and2_2'),
    'gf180': dict(
        variant='gf180mcuD', library='gf180mcu_fd_sc_mcu7t5v0', workspace='gf180_physical', def_name='gf180.def',
        cell_gds='libs.ref/gf180mcu_fd_sc_mcu7t5v0/gds/gf180mcu_fd_sc_mcu7t5v0.gds',
        tech_lef='libs.ref/gf180mcu_fd_sc_mcu7t5v0/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef',
        cell_lef='libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef',
        klayout_map='libs.tech/klayout/tech/gf180mcu.map',
        magicrc='libs.tech/magic/gf180mcuD.magicrc',
        netgen_setup='libs.tech/netgen/gf180mcuD_setup.tcl',
        cell_spice='libs.ref/gf180mcu_fd_sc_mcu7t5v0/spice/gf180mcu_fd_sc_mcu7t5v0.spice',
        cell_cdl='libs.ref/gf180mcu_fd_sc_mcu7t5v0/cdl/gf180mcu_fd_sc_mcu7t5v0.cdl',
        drc_deck='libs.tech/klayout/tech/drc/gf180mcu.drc',
        lvs_runner='libs.tech/klayout/tech/lvs/run_lvs.py',
        layer_map=ROOT / 'fixtures/pdk/gf180-gds-map.json',
        wire_layer=(36, 0), wrong_map=('Metal2', 'net', [36, 1]),
        missing_cell='gf180mcu_fd_sc_mcu7t5v0__and2_2'),
}


class Unverified(RuntimeError):
    """A required external tool or input is missing: never a pass."""


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as handle:
        for block in iter(lambda: handle.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


def tree_digest(root):
    """Order-independent digest of every file path and content under root."""
    h = hashlib.sha256()
    for path in sorted(p for p in Path(root).rglob('*') if p.is_file()):
        h.update(str(path.relative_to(root)).encode() + b'\0' + sha256(path).encode() + b'\n')
    return h.hexdigest()


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def parse_time(path):
    wall = rss = None
    try:
        text = Path(path).read_text(errors='replace')
    except OSError:
        return None, None
    m = re.search(r'Elapsed \(wall clock\) time \(h:mm:ss or m:ss\): (\S+)', text)
    if m:
        parts = [float(x) for x in m.group(1).split(':')]
        wall = sum(v * 60 ** i for i, v in enumerate(reversed(parts)))
    m = re.search(r'Maximum resident set size \(kbytes\): (\d+)', text)
    if m:
        rss = int(m.group(1))
    return wall, rss


class Harness:
    def __init__(self, run):
        self.run = run
        self.receipts = []

    def _record(self, name, argv, proc, started, time_file):
        directory = self.run / 'receipts' / name
        (directory / 'stdout.txt').write_bytes(proc.stdout)
        (directory / 'stderr.txt').write_bytes(proc.stderr)
        wall, rss = parse_time(time_file)
        receipt = dict(name=name, argv=[str(a) for a in argv], exit_code=proc.returncode,
                       wall_seconds=wall if wall is not None else time.monotonic() - started,
                       peak_rss_kib=rss)
        self.receipts.append(receipt)
        return receipt, proc.stdout.decode(errors='replace') + proc.stderr.decode(errors='replace')

    def host(self, name, argv, timeout=180):
        directory = self.run / 'receipts' / name
        directory.mkdir(parents=True, exist_ok=False)
        time_file = directory / 'time.txt'
        full = ['systemd-run', '--user', '--scope', '-q', '-p', 'MemoryMax=8G', '-p', 'MemorySwapMax=2G',
                'timeout', f'{timeout}s', gnu_time_program(), '-v', '-o', str(time_file), *map(str, argv)]
        started = time.monotonic()
        proc = subprocess.run(full, capture_output=True, timeout=timeout + 60)
        return self._record(name, argv, proc, started, time_file)

    def container(self, name, argv, timeout=600, env=None):
        directory = self.run / 'receipts' / name
        directory.mkdir(parents=True, exist_ok=False)
        time_file = directory / 'time.txt'
        environment = []
        for key, value in (env or {}).items():
            environment += ['-e', f'{key}={value}']
        full = ['podman', 'run', '--rm', '--pull=never', '--network=none', '--read-only',
                '--tmpfs=/tmp:rw,size=512m', '--user=0:0', '--memory=4g', '--memory-swap=4g', '--cpus=4',
                '--pids-limit=256', '-v', f'{self.run}:/work:rw', '-v', f'{ORACLES}:/oracles:ro', '-w', '/work',
                '-e', 'PDK_ROOT=/foss/pdks', '-e', 'HOME=/tmp', '-e', 'QT_QPA_PLATFORM=offscreen',
                '-e', 'PATH=/foss/tools/bin:/foss/tools/klayout:/usr/local/bin:/usr/bin:/bin', *environment,
                '--entrypoint=/usr/bin/timeout', IMAGE, str(timeout), '/usr/bin/time', '-v', '-o',
                '/work/' + str(time_file.relative_to(self.run)), *map(str, argv)]
        started = time.monotonic()
        proc = subprocess.run(full, capture_output=True, timeout=timeout + 120)
        return self._record(name, argv, proc, started, time_file)


def cpath(run, path):
    """Container path of a file under the run directory."""
    return '/work/' + str(Path(path).resolve().relative_to(run))


# ---------------------------------------------------------------- GDS bytes
# A deliberately small, independent record walker (no RustROAD code) used for
# verbatim-copy checks and for planting negative-control defects.

def records(data):
    at = 0
    while at < len(data):
        length, rtype = struct.unpack('>HB', data[at:at + 3])
        require(length >= 4 and at + length <= len(data), 'bad GDS framing at %d' % at)
        yield at, length, rtype
        at += length
        if rtype == 0x04:
            break


def structures(data):
    out = collections.OrderedDict()
    start = name = None
    for at, length, rtype in records(data):
        if rtype == 0x05:
            start = at
        elif rtype == 0x06:
            name = data[at + 4:at + length].rstrip(b'\0').decode()
        elif rtype == 0x07:
            require(name not in out, 'duplicate structure ' + name)
            out[name] = (start, at + length)
    return out


def boundary_record(layer, datatype, rect):
    """One GDS BOUNDARY element (closed 5-point rectangle) as raw records."""
    lx, ly, ux, uy = rect
    xy = struct.pack('>10i', lx, ly, ux, ly, ux, uy, lx, uy, lx, ly)
    return (struct.pack('>HH', 4, 0x0800) + struct.pack('>HHh', 6, 0x0D02, layer)
            + struct.pack('>HHh', 6, 0x0E02, datatype) + struct.pack('>HH', 4 + len(xy), 0x1003) + xy
            + struct.pack('>HH', 4, 0x1100))


def insert_into_top(data, top, record):
    """Bytes of `data` with `record` appended to structure `top` (before ENDSTR)."""
    begin, end = structures(data)[top]
    return data[:end - 4] + record + data[end - 4:]


def def_patches(definition, divisor):
    """Routing RECT statements of the DEF NETS section as (lef layer, GDS rect)."""
    text = Path(definition).read_text()
    body = text.split('\nNETS ', 1)[1].split('\nEND NETS', 1)[0]
    out = []
    for m in re.finditer(r'(?m)^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) RECT '
                         r'\( (-?\d+) (-?\d+) (-?\d+) (-?\d+) \)', body):
        x, y, a, b, c, d = map(int, m.groups()[1:])
        rect = (x + a, y + b, x + c, y + d)
        require(all(v % divisor == 0 for v in rect), 'DEF patch not on the GDS grid')
        out.append((m.group(1), tuple(v // divisor for v in rect)))
    return out


def top_elements(data, top):
    """(start, end, kind, layer, datatype, xy) for each element of `top`."""
    begin, end = structures(data)[top]
    out = []
    current = None
    for at, length, rtype in records(data[begin:end]):
        payload = data[begin + at + 4:begin + at + length]
        if rtype in (0x08, 0x09, 0x0A, 0x0B, 0x0C):
            current = dict(start=begin + at, kind=rtype, layer=None, datatype=None, xy=[])
        elif current is not None and rtype == 0x0D:
            current['layer'] = struct.unpack('>h', payload)[0]
        elif current is not None and rtype == 0x0E:
            current['datatype'] = struct.unpack('>h', payload)[0]
        elif current is not None and rtype == 0x10:
            values = struct.unpack('>%di' % (len(payload) // 4), payload)
            current['xy'] = list(zip(values[0::2], values[1::2]))
        elif current is not None and rtype == 0x11:
            current['end'] = begin + at + length
            out.append(current)
            current = None
    return out


# ------------------------------------------------------- layer-map citations

def parse_klayout_map(path):
    """KLayout LEF/DEF layer map -> {(lef, purpose): {(layer, datatype)}} with line numbers."""
    table = collections.defaultdict(set)
    lines = collections.defaultdict(set)
    for number, raw in enumerate(Path(path).read_text().splitlines(), 1):
        fields = raw.split()
        if not fields or fields[0].startswith('#'):
            continue
        require(len(fields) == 4, f'unexpected map line {number}: {raw!r}')
        target = (int(fields[2]), int(fields[3]))
        if fields[0] == 'NAME':
            for item in fields[1].split(','):
                lef, purpose = item.split('/')
                table[(lef, 'NAME_' + purpose)].add(target)
                lines[(lef, 'NAME_' + purpose)].add(number)
        else:
            for purpose in fields[1].split(','):
                table[(fields[0], purpose)].add(target)
                lines[(fields[0], purpose)].add(number)
    return table, lines


def verify_layer_map(map_path):
    """Re-derive every map entry from the cited PDK files; return the findings."""
    profile = json.loads(Path(map_path).read_text())
    sources = {s['id']: s for s in profile['sources']}
    problems = []
    for s in sources.values():
        if not Path(s['path']).is_file():
            raise Unverified('cited PDK file missing: ' + s['path'])
        if sha256(s['path']) != s['sha256'] or os.path.getsize(s['path']) != s['bytes']:
            problems.append('source %s sha256/bytes differ' % s['id'])
    table, lines = parse_klayout_map(sources['klayout_map']['path'])
    tech = Path(sources['magic_tech']['path']).read_text().splitlines()
    purposes = dict(net='NET', special_net='SPNET', via='VIA', pin='PIN')
    checked = 0
    for entry in profile['layers'] + [dict(lef='DIEAREA', boundary=profile['boundary'])]:
        cites = {c['source']: c for c in (entry.get('cite') or entry['boundary']['cite'])}
        for c in cites.values():
            if c['sha256'] != sources[c['source']]['sha256']:
                problems.append('%s citation sha256 mismatch' % entry['lef'])
        claims = []
        if 'boundary' in entry:
            claims.append(('ALL', {tuple(entry['boundary']['layer'])}))
        else:
            for key, purpose in purposes.items():
                if entry.get(key):
                    claims.append((purpose, {tuple(t) for t in entry[key]}))
            if entry.get('label'):
                claims.append(('NAME_LABEL', {tuple(entry['label'])}))
        for purpose, targets in claims:
            actual = table.get((entry['lef'], purpose), set())
            if actual != targets:
                problems.append(f"{entry['lef']} {purpose}: profile {sorted(targets)} != PDK map {sorted(actual)}")
            missing = lines.get((entry['lef'], purpose), set()) - set(cites['klayout_map']['lines'])
            if missing:
                problems.append(f"{entry['lef']} {purpose}: map lines {sorted(missing)} not cited")
            for layer, datatype in targets:
                calma = [n for n in cites['magic_tech']['lines']
                         if re.search(r'\bcalma\s+%d\s+%d\b' % (layer, datatype), tech[n - 1])]
                if not calma:
                    problems.append(f"{entry['lef']} {purpose}: no cited Magic calma {layer} {datatype}")
            checked += 1
    return dict(status='passed' if not problems else 'failed', claims_checked=checked, problems=problems,
                profile_sha256=sha256(map_path))


# --------------------------------------------------------------- parsers

def lyrdb_counts(path):
    text = Path(path).read_text(errors='replace')
    require('<report-database>' in text, 'not a KLayout report database: %s' % path)
    counts = collections.Counter()
    cells = collections.Counter()
    for item in re.findall(r'<item>(.*?)</item>', text, re.S):
        category = re.search(r'<category>(.*?)</category>', item, re.S)
        cell = re.search(r'<cell>(.*?)</cell>', item, re.S)
        counts[category.group(1).strip().strip("'") if category else '?'] += 1
        cells[cell.group(1).strip() if cell else '?'] += 1
    return dict(sorted(counts.items())), dict(sorted(cells.items()))


def magic_drc(text):
    require('CELL_PRESENT' in text and 'DRC_DONE' in text, 'Magic DRC did not complete on the top cell')
    total = int(re.search(r'^DRC_TOTAL (\d+)', text, re.M).group(1))
    why = collections.Counter()
    for count, message in re.findall(r'^DRC_WHY (\d+) :: (.*)$', text, re.M):
        why[message.strip()] += int(count)
    return total, dict(sorted(why.items()))


def netgen_verdict(json_path, out_path):
    entries = json.loads(Path(json_path).read_text())
    text = Path(out_path).read_text(errors='replace')
    def equal_pair(e, key):
        # Entries for device primitives (black-box placeholders) carry pins only.
        value = e.get(key)
        if value is None:
            return key != 'pins'
        if key == 'devices':
            return len(value) == 2 and sorted(map(tuple, value[0])) == sorted(map(tuple, value[1]))
        if key == 'pins':
            return len(value) == 2 and sorted(value[0]) == sorted(value[1])
        return len(value) == 2 and value[0] == value[1]
    clean = bool(entries) and any('name' in e for e in entries) and all(
        not e.get('badnets') and not e.get('badelements')
        and all(equal_pair(e, k) for k in ('pins', 'nets', 'devices')) for e in entries)
    final = re.findall(r'Final result: (.*)', text)
    match = clean and bool(final) and final[-1].strip().startswith('Circuits match uniquely')
    top = [e for e in entries if e.get('name', [None])[0] == TOP]
    return dict(verdict='match' if match else 'mismatch', final=final[-1].strip() if final else None,
                circuits=len(entries), top_nets=top[0]['nets'] if top else None,
                top_devices=top[0]['devices'][0] if top else None)


# --------------------------------------------------- LVS reference netlists

def powered_instances(verilog):
    """Instances and alias map from RustROAD's canonical powered Verilog."""
    text = re.sub(r'//.*', '', Path(verilog).read_text())
    header = re.search(r'module\s+(\S+)\s*\((.*?)\);', text, re.S)
    ports = []
    for decl in header.group(2).split(','):
        words = decl.split()
        rng = re.search(r'\[(\d+):(\d+)\]', decl)
        if rng:
            hi, lo = int(rng.group(1)), int(rng.group(2))
            ports += ['%s[%d]' % (words[-1], i) for i in range(hi, lo - 1, -1)]
        else:
            ports.append(words[-1])
    norm = lambda s: s.strip().lstrip('\\').strip()
    alias = {norm(b): norm(a) for a, b in re.findall(r'assign\s+(\S+)\s*=\s*(\\?\S+?)\s*;', text)}
    instances = []
    for cell, name, connections in re.findall(r'^\s*(\S+)\s+(\S+)\s*\((.*?)\);', text, re.S | re.M):
        if cell in ('module', 'input', 'output', 'inout', 'wire', 'assign'):
            continue
        pins = {p: alias.get(norm(n), norm(n)) for p, n in re.findall(r'\.(\w+)\s*\(\s*(\\?[^)]*?)\s*\)', connections)}
        instances.append((cell, name, pins))
    return header.group(1), ports, instances


def subcircuits(path):
    text = Path(path).read_text(errors='replace')
    out = {}
    for m in re.finditer(r'^\.subckt\s+(\S+)\s+([^\n]*)\n(.*?)^\.ends[^\n]*\n', text, re.S | re.M | re.I):
        body = [line for line in m.group(3).splitlines() if line.strip() and not line.startswith('*')]
        out[m.group(1)] = (m.group(2).split(), m.group(0), len(body))
    return out


def lvs_schematic(verilog, library, out, rescale_microns):
    """Powered netlist -> SPICE/CDL schematic. Device-less physical cells (taps,
    fillers) are dropped: they have no devices to compare. Sky130 SPICE uses
    the w=650000u convention; KLayout needs w=0.65u (rescale)."""
    top, ports, instances = powered_instances(verilog)
    cells = subcircuits(library)
    used, lines, dropped = [], [], collections.Counter()
    for cell, name, pins in instances:
        order, _, devices = cells[cell]
        if devices == 0:
            dropped[cell] += 1
            continue
        lines.append('X%s %s %s' % (name, ' '.join(pins[p] for p in order), cell))
        if cell not in used:
            used.append(cell)
    body = []
    for cell in used:
        text = cells[cell][1]
        if rescale_microns:
            text = re.sub(r'\b([wl])=([0-9.e+\-]+)u\b', lambda q: '%s=%gu' % (q.group(1), float(q.group(2)) * 1e-6), text)
        body.append(text)
    Path(out).write_text('* LVS schematic generated from %s\n%s\n.subckt %s %s\n%s\n.ends %s\n' % (
        Path(verilog).name, '\n'.join(body), top, ' '.join(ports), '\n'.join(lines), top))
    return dict(instances=len(lines), cells=used, dropped=dict(dropped))


# ------------------------------------------------------------- per-PDK run

def export(h, tag, binary, workspace, cell_gds, layer_map, out, name):
    receipt, output = h.host(name, [binary, '--workspace', workspace, 'export-gds', '--cell-gds', cell_gds,
                                    '--layer-map', layer_map, '--out', out])
    report = None
    if receipt['exit_code'] == 0:
        report = json.loads((h.run / 'receipts' / name / 'stdout.txt').read_text())
    return receipt, report, output


def klayout_drc(h, tag, p, gds, label, decks=None):
    run = h.run
    report = run / tag / f'{label}.lyrdb'
    root = CONTAINER_PDK + '/' + p['variant'] + '/'
    if tag == 'sky130':
        if decks == 'density':
            argv = ['klayout', '-b', '-r', root + p['density_deck'], '-rd', 'input=' + cpath(run, gds),
                    '-rd', 'report=' + cpath(run, report)]
        else:
            argv = ['klayout', '-b', '-r', root + p['drc_deck'], '-rd', 'input=' + cpath(run, gds),
                    '-rd', 'report=' + cpath(run, report), '-rd', 'feol=true', '-rd', 'beol=true',
                    '-rd', 'offgrid=true', '-rd', 'floating_met=false', '-rd', 'seal=false', '-rd', 'thr=4',
                    '-rd', 'top_cell=' + TOP]
    else:
        argv = ['klayout', '-b', '-r', root + p['drc_deck'], '-rd', 'input=' + cpath(run, gds),
                '-rd', 'report=' + cpath(run, report), '-rd', 'variant=gf180mcuD',
                '-rd', 'decks=' + (decks or 'all,-antenna,-density'), '-rd', 'run_mode=deep',
                '-rd', 'threads=4', '-rd', 'topcell=' + TOP]
    receipt, output = h.container(f'{tag}_{label}', argv, timeout=900)
    require(report.is_file(), f'{label}: KLayout wrote no report (exit {receipt["exit_code"]})')
    counts, cells = lyrdb_counts(report)
    result = dict(counts=counts, items=sum(counts.values()), cells=cells, exit_code=receipt['exit_code'])
    if decks == 'density':
        if tag == 'sky130':
            values = re.findall(r'^(\w+)_ca_density is (\S+)', output, re.M)
            result['clear_area_density'] = {k: v for k, v in values}
            result['non_finite'] = not values or bool(re.search(r'NaN|Infinity', output))
            result['scope'] = ('PDK KLayout density deck (maximum density / minimum clear area, normalised by '
                               'the 235/4 PR boundary); minimum density needs chip-level fill: unchecked')
        else:
            result['coverage_percent'] = {k: float(v) for k, v in re.findall(r'(\w+) ratio: ([0-9.]+) %', output)}
            result['scope'] = 'PDK KLayout density deck (whole-extent minimum coverage rules); no fill at block level'
        result['status'] = 'passed' if result['items'] == 0 and not result.get('non_finite') else 'failed'
    return result


def magic_drc_run(h, tag, p, gds, label):
    receipt, output = h.container(f'{tag}_{label}', ['magic', '-dnull', '-noconsole', '-rcfile',
                                                    CONTAINER_PDK + '/' + p['variant'] + '/' + p['magicrc'],
                                                    '/oracles/magic_drc.tcl'],
                                  timeout=600, env=dict(GDS=cpath(h.run, gds), CELL=TOP))
    total, why = magic_drc(output)
    return dict(total=total, reasons=why)


def netgen_lvs(h, tag, p, gds, powered, label):
    run = h.run
    outdir = run / tag / f'{label}_ext'
    root = CONTAINER_PDK + '/' + p['variant'] + '/'
    receipt, output = h.container(f'{tag}_{label}_extract', ['magic', '-dnull', '-noconsole', '-rcfile',
                                                            root + p['magicrc'], '/oracles/magic_ext.tcl'],
                                  timeout=600, env=dict(GDS=cpath(run, gds), CELL=TOP, OUTDIR=cpath(run, outdir)))
    require('EXT_DONE' in output and (outdir / f'{TOP}.spice').is_file(), f'{label}: Magic extraction failed')
    comp = run / tag / f'{label}_comp.out'
    receipt, output = h.container(f'{tag}_{label}_netgen', ['netgen', '-batch', 'source', '/oracles/netgen_lvs.tcl'],
                                  timeout=600, env=dict(LAYOUT=cpath(run, outdir / f'{TOP}.spice'), TOP=TOP,
                                                        REFV=cpath(run, powered), STDSPICE=root + p['cell_spice'],
                                                        SETUP=root + p['netgen_setup'], OUT=cpath(run, comp)))
    require(comp.is_file() and comp.with_suffix('.json').is_file(), f'{label}: Netgen wrote no report')
    return netgen_verdict(comp.with_suffix('.json'), comp)


def klayout_lvs(h, tag, p, gds, schematic, label):
    run = h.run
    root = CONTAINER_PDK + '/' + p['variant'] + '/'
    if tag == 'sky130':
        db = run / tag / f'{label}.lvsdb'
        receipt, output = h.container(f'{tag}_{label}', [
            'klayout', '-b', '-r', root + p['lvs_deck'], '-rd', 'input=' + cpath(run, gds),
            '-rd', 'report=' + cpath(run, db), '-rd', 'schematic=' + cpath(run, schematic),
            '-rd', 'target_netlist=' + cpath(run, run / tag / f'{label}.cir'), '-rd', 'thr=4',
            '-rd', 'run_mode=deep', '-rd', 'top_cell=' + TOP, '-rd', 'convert_subckts=true',
            '-rd', 'combine=true', '-rd', 'schematic_simplify=true'], timeout=900)
        log = output
    else:
        rundir = run / tag / f'{label}_run'
        rundir.mkdir()
        receipt, output = h.container(f'{tag}_{label}', [
            'python3', root + p['lvs_runner'], '--layout=' + cpath(run, gds), '--netlist=' + cpath(run, schematic),
            '--variant=D', '--topcell=' + TOP, '--run_mode=deep', '--run_dir=' + cpath(run, rundir), '--thr=4'],
            timeout=900)
        dbs = sorted(rundir.glob('*.lvsdb'))
        require(len(dbs) == 1, f'{label}: expected one lvsdb, found {len(dbs)}')
        db = dbs[0]
        log = output + ''.join(x.read_text(errors='replace') for x in rundir.glob('*.log'))
    require(db.is_file(), f'{label}: KLayout LVS wrote no database')
    summary = run / tag / f'{label}_xref.json'
    h.container(f'{tag}_{label}_xref', ['klayout', '-b', '-r', '/oracles/klayout_lvsdb.py', '-rd',
                                        'db_path=' + cpath(run, db), '-rd', 'out=' + cpath(run, summary)], timeout=300)
    xref = json.loads(summary.read_text())
    congratulated = 'Congratulations! Netlists match.' in log
    all_match = bool(xref['circuits']) and all(c[2] == 'Match' for c in xref['circuits'])
    return dict(verdict='match' if congratulated and all_match else 'mismatch', log_match=congratulated,
                circuits=xref['circuits'], mismatches=xref['mismatches'][:20], exit_code=receipt['exit_code'])


def signoff_lvs(h, tag, gds, powered, label):
    """Both LVS checks of scripts/signoff (pinned image): besides each tool's
    verdict they require every labelled top-level net to be paired with the
    same-named reference net, all reference ports to be paired top pins, and
    the label set to equal the port set. The basic KLayout check above stops at
    "Netlists match"; a labelled net that lost all its devices is left out of
    that comparison, so a deleted port wire can still read "match" there."""
    ns = argparse.Namespace(gds=gds, top=TOP, pdk=tag, out=h.run / tag / f'{label}_signoff', netlist=powered,
                            against=None, against_top=None, manifest=MANIFEST, threads=4, memory='4g', timeout=900,
                            plant=None)
    summary = signoff_harness.run_checks(ns, ['lvs-netgen', 'lvs-klayout'])
    out = {}
    for check in ('lvs-netgen', 'lvs-klayout'):
        v = json.loads(Path(summary['verdicts'][check]).read_text())
        out[check.replace('-', '_')] = dict(
            verdict={'passed': 'match', 'failed': 'mismatch'}.get(v['status'], 'unverified'),
            status=v['status'], failures=v.get('failures', [])[:10],
            unverified_reasons=v.get('unverified_reasons', [])[:10])
    return out


def strmxor(h, tag, a, b, label):
    receipt, output = h.container(f'{tag}_{label}', ['strmxor', cpath(h.run, a), cpath(h.run, b)], timeout=300)
    same = receipt['exit_code'] == 0 and 'No differences found' in output
    table = [line.strip() for line in output.splitlines() if re.match(r'^\s*\d+/\d+\s', line)]
    return dict(identical=same, exit_code=receipt['exit_code'], differences=table[:40],
                summary=[line.strip() for line in output.splitlines() if 'ifference' in line][:5])


def texts(h, tag, gds, label):
    out = h.run / tag / f'{label}_texts.json'
    h.container(f'{tag}_{label}_texts', ['klayout', '-b', '-r', '/oracles/klayout_texts.py', '-rd',
                                         'gds=' + cpath(h.run, gds), '-rd', 'out=' + cpath(h.run, out)], timeout=300)
    require(out.is_file(), f'{label}: text dump failed')
    return json.loads(out.read_text())


def case(h, tag, args):
    p = PDKS[tag]
    run = h.run
    work = run / tag
    work.mkdir()
    pdk_root = HOST_ROOTS[p['variant']]
    cell_gds = pdk_root / p['cell_gds']
    inputs = {key: pdk_root / p[key] for key in ('cell_gds', 'tech_lef', 'cell_lef', 'klayout_map', 'magicrc',
                                                   'netgen_setup', 'cell_spice') if key in p}
    for key, path in inputs.items():
        if not path.is_file():
            raise Unverified(f'{tag}: PDK input {key} missing at {path}')
    result = dict(pdk=tag, inputs={k: dict(path=str(v), sha256=sha256(v)) for k, v in inputs.items()})

    source = args.source / p['workspace']
    require((source / 'HEAD').is_file(), f'{tag}: source workspace missing: {source}')
    workspace = work / 'workspace'
    shutil.copytree(source, workspace, symlinks=False)
    result['source_workspace'] = dict(path=str(source), tree_sha256=tree_digest(source))
    before = tree_digest(workspace)
    head = (workspace / 'HEAD').read_text()
    require(before == result['source_workspace']['tree_sha256'], 'workspace copy differs from its source')

    # 1. Native stream, twice; read-only.
    native = work / 'native.gds'
    receipt, report, output = export(h, tag, args.rustroad, workspace, cell_gds, p['layer_map'], native,
                                     f'{tag}_export')
    require(report is not None and report['status'] == 'written', f'{tag}: export-gds failed: {output[-2000:]}')
    again = work / 'native_again.gds'
    receipt2, report2, _ = export(h, tag, args.rustroad, workspace, cell_gds, p['layer_map'], again,
                                  f'{tag}_export_again')
    require(report2 is not None, f'{tag}: second export failed')
    after = tree_digest(workspace)
    result['export'] = dict(report=report, wall_seconds=receipt['wall_seconds'], peak_rss_kib=receipt['peak_rss_kib'])
    result['read_only'] = dict(status='passed' if before == after and (workspace / 'HEAD').read_text() == head
                               else 'failed', head=head.strip(), tree_sha256_before=before, tree_sha256_after=after)
    result['determinism'] = dict(status='passed' if native.read_bytes() == again.read_bytes()
                                 and report['output']['sha256'] == sha256(native) == report2['output']['sha256']
                                 else 'failed', sha256=sha256(native), bytes=native.stat().st_size)
    # A second independent copy at another path must stream the same bytes.
    other = work / 'workspace_second_copy'
    shutil.copytree(source, other, symlinks=False)
    third = work / 'native_other_copy.gds'
    export(h, tag, args.rustroad, other, cell_gds, p['layer_map'], third, f'{tag}_export_other_copy')
    if third.read_bytes() != native.read_bytes():
        result['determinism']['status'] = 'failed'
    result['determinism']['independent_copy_identical'] = third.read_bytes() == native.read_bytes()
    shutil.rmtree(other)

    # 2. Reference stream of the same routed DEF (KLayout LEF/DEF reader).
    definition = work / 'routed.def'
    receipt, output = h.host(f'{tag}_export_def', [args.rustroad, '--workspace', workspace, 'export-def'])
    require(receipt['exit_code'] == 0, f'{tag}: export-def failed')
    definition.write_bytes((run / 'receipts' / f'{tag}_export_def' / 'stdout.txt').read_bytes())
    flow_def = args.source / p['def_name']
    result['routed_def'] = dict(sha256=sha256(definition), flow_def_sha256=sha256(flow_def) if flow_def.is_file() else None)
    reference = work / 'reference.gds'
    root = CONTAINER_PDK + '/' + p['variant'] + '/'
    receipt, output = h.container(f'{tag}_reference', ['klayout', '-b', '-r', '/oracles/klayout_def2gds.py'],
                                  timeout=600, env=dict(DEF=cpath(run, definition), TLEF=root + p['tech_lef'],
                                                        CLEF=root + p['cell_lef'], CGDS=root + p['cell_gds'],
                                                        MAP=root + p['klayout_map'], OUT=cpath(run, reference)))
    require('REFERENCE_WROTE' in output and reference.is_file(), f'{tag}: reference streamout failed')
    result['reference'] = dict(sha256=sha256(reference), bytes=reference.stat().st_size,
                               wall_seconds=receipt['wall_seconds'], peak_rss_kib=receipt['peak_rss_kib'])

    # 3. Geometry XOR, labels, hierarchy.
    xor = strmxor(h, tag, native, reference, 'strmxor')
    self_xor = strmxor(h, tag, native, again, 'strmxor_self')
    result['xor'] = dict(status='passed' if xor['identical'] and self_xor['identical'] else 'failed',
                         native_vs_reference=xor, native_vs_rerun=self_xor)
    nt, rt = texts(h, tag, native, 'native'), texts(h, tag, reference, 'reference')
    labels_equal = nt['labels'] == rt['labels'] and nt['flat_texts'] == rt['flat_texts']
    result['labels'] = dict(status='passed' if labels_equal and nt['labels'] else 'failed',
                            top_labels=len(nt['labels']), flat_texts=len(nt['flat_texts']),
                            native_only=[x for x in nt['labels'] if x not in rt['labels']],
                            reference_only=[x for x in rt['labels'] if x not in nt['labels']],
                            names=sorted(x[2] for x in nt['labels']), dbu=[nt['dbu'], rt['dbu']])
    result['hierarchy'] = dict(native_cells=sorted(nt['cells']), reference_cells=sorted(rt['cells']),
                               same_cell_names=sorted(nt['cells']) == sorted(rt['cells']),
                               native_top_instances=nt['cells'][TOP]['instances'],
                               reference_top_instances=rt['cells'][TOP]['instances'])

    # 4. Verbatim cell copies.
    pdk_bytes = cell_gds.read_bytes()
    native_bytes = native.read_bytes()
    pdk_structs, native_structs = structures(pdk_bytes), structures(native_bytes)
    copied = [n for n in native_structs if n in pdk_structs]
    identical = [n for n in copied if native_bytes[slice(*native_structs[n])] == pdk_bytes[slice(*pdk_structs[n])]]
    tampered = bytearray(native_bytes)
    s, e = native_structs[copied[0]]
    tampered[e - 8] ^= 0x01
    control = bytes(tampered[s:e]) != pdk_bytes[slice(*pdk_structs[copied[0]])]
    result['cell_bytes'] = dict(status='passed' if copied and identical == copied and set(copied) ==
                                set(report['pdk_structures']) and control else 'failed',
                                copied=copied, identical=len(identical), tamper_control_detected=control,
                                non_pdk_structures=[n for n in native_structs if n not in pdk_structs])

    # 4b. Routing patches (DEF RECT) streamed on the net purpose, and the
    # reference XOR's sensitivity to them.
    layer_map = json.loads(Path(p['layer_map']).read_text())
    net_targets = {e['lef']: [tuple(t) for t in e.get('net') or []] for e in layer_map['layers']}
    divisor = report['coordinate_divisor']
    wanted = def_patches(definition, divisor)
    elements = top_elements(native_bytes, TOP)
    boxes = {}
    for e in elements:
        if e['kind'] == 0x08 and len(e['xy']) == 5:
            xs, ys = [x for x, _ in e['xy']], [y for _, y in e['xy']]
            boxes.setdefault(((e['layer'], e['datatype']), (min(xs), min(ys), max(xs), max(ys))), []).append(e)
    found, missing = [], []
    for lef, rect in wanted:
        hits = [boxes.get((t, rect)) for t in net_targets.get(lef, [])]
        if hits and all(hits):
            found.extend(x[0] for x in hits)
        else:
            missing.append([lef, list(rect)])
    patch_check = dict(def_rects=len(wanted), streamed_elements=len(found), missing=missing,
                       report_routed_patches=report['elements'].get('routed_patches'))
    if wanted:
        stripped = native_bytes
        for e in sorted(found, key=lambda x: x['start'], reverse=True):
            stripped = stripped[:e['start']] + stripped[e['end']:]
        nopatch = work / 'control_patches_removed.gds'
        nopatch.write_bytes(stripped)
        x = strmxor(h, tag, nopatch, reference, 'control_patches_removed_xor')
        controls_patch = dict(detected=not x['identical'], removed=len(found), xor=x)
    else:
        controls_patch = dict(detected=None, removed=0, note='no DEF RECT patches in this layout')
    patch_check['status'] = ('passed' if not missing and report['elements'].get('routed_patches') == len(wanted)
                             else 'failed')
    result['patches'] = patch_check

    # 5. DRC (Magic + KLayout) and density, native vs reference.
    drc = {}
    for label, gds in (('native', native), ('reference', reference)):
        drc[label] = dict(magic=magic_drc_run(h, tag, p, gds, f'{label}_magic_drc'),
                          klayout=klayout_drc(h, tag, p, gds, f'{label}_klayout_drc'),
                          density=klayout_drc(h, tag, p, gds, f'{label}_density', decks='density'))
    equal = (drc['native']['magic'] == drc['reference']['magic']
             and drc['native']['klayout']['counts'] == drc['reference']['klayout']['counts']
             and drc['native']['density']['counts'] == drc['reference']['density']['counts'])
    # Positive control: a 0.1 x 0.1 um square on the wire layer, outside the
    # die, is below minimum width and area on both PDKs; both decks must see it.
    planted = work / 'control_planted_drc.gds'
    planted.write_bytes(insert_into_top(native.read_bytes(), TOP,
                                        boundary_record(*p['wire_layer'], (-5000, -5000, -4900, -4900))))
    plant = dict(magic=magic_drc_run(h, tag, p, planted, 'control_planted_magic_drc'),
                 klayout=klayout_drc(h, tag, p, planted, 'control_planted_klayout_drc'))
    plant_detected = (plant['magic']['total'] > drc['native']['magic']['total']
                      and plant['klayout']['items'] > drc['native']['klayout']['items'])
    clean = drc['native']['magic']['total'] == 0 and drc['native']['klayout']['items'] == 0
    result['drc'] = dict(status='equal_to_reference' if equal else 'differs', clean=clean,
                         native=drc['native'], reference=drc['reference'],
                         drc_verdict='passed' if clean else 'failed',
                         planted_control=dict(detected=plant_detected, layer=list(p['wire_layer']),
                                              rect_gds=[-5000, -5000, -4900, -4900],
                                              magic=plant['magic'], klayout=plant['klayout']['counts']))
    density = drc['native']['density']
    result['density_status'] = dict(status=density['status'], counts=density['counts'], items=density['items'],
                                    reference_counts=drc['reference']['density']['counts'], scope=density['scope'],
                                    values=density.get('clear_area_density') or density.get('coverage_percent'),
                                    note='reported separately; never folded into the block DRC verdict')

    # 6. LVS (Netgen + KLayout) against the workspace powered netlist.
    powered = work / 'powered.v'
    receipt, _ = h.host(f'{tag}_export_powered', [args.rustroad, '--workspace', workspace, 'export-verilog',
                                                  '--include-power'])
    require(receipt['exit_code'] == 0, f'{tag}: powered export failed')
    powered.write_bytes((run / 'receipts' / f'{tag}_export_powered' / 'stdout.txt').read_bytes())
    if tag == 'sky130':
        schematic = work / 'schematic.spice'
        generated = lvs_schematic(powered, inputs['cell_spice'], schematic, rescale_microns=True)
    else:
        schematic = work / 'schematic.cdl'
        generated = lvs_schematic(powered, pdk_root / p['cell_cdl'], schematic, rescale_microns=False)
    lvs = {}
    for label, gds in (('native', native), ('reference', reference)):
        lvs[label] = dict(netgen=netgen_lvs(h, tag, p, gds, powered, f'{label}_lvs'),
                          klayout=klayout_lvs(h, tag, p, gds, schematic, f'{label}_klayout_lvs'),
                          signoff=signoff_lvs(h, tag, gds, powered, f'{label}_lvs'))
    verdicts = {k: (v['netgen']['verdict'], v['klayout']['verdict'], v['signoff']['lvs_netgen']['verdict'],
                    v['signoff']['lvs_klayout']['verdict']) for k, v in lvs.items()}
    result['lvs'] = dict(status='equal_to_reference' if verdicts['native'] == verdicts['reference'] else 'differs',
                         lvs_verdict='match' if verdicts['native'] == ('match',) * 4 else 'mismatch',
                         verdict_order=['netgen_basic', 'klayout_basic', 'signoff_netgen', 'signoff_klayout'],
                         verdicts=verdicts, schematic=generated, native=lvs['native'], reference=lvs['reference'])

    # 7. Negative controls.
    controls = {}
    elements = top_elements(native_bytes, TOP)
    # Victim: the largest SIGNAL wire rectangle on the wire layer, i.e. one
    # that is a DEF NETS path (centre line equal to a path, any half width);
    # SPECIALNETS supply wires share the layer/datatype but have redundant
    # connections, so deleting one need not open a net.
    paths = set()
    body = definition.read_text().split('\nNETS ', 1)[1].split('\nEND NETS', 1)[0]
    for m in re.finditer(r'(?m)^\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)', body):
        if m.group(1) == p['wrong_map'][0]:
            x0, y0, x1, y1 = (int(v) // report['coordinate_divisor'] for v in m.groups()[1:])
            paths.add((min(x0, x1), min(y0, y1), max(x0, x1), max(y0, y1)))

    def signal(e):
        xs, ys = [x for x, _ in e['xy']], [y for _, y in e['xy']]
        lx, ly, ux, uy = min(xs), min(ys), max(xs), max(ys)
        hx, hy = (ux - lx) // 2, (uy - ly) // 2
        cy, cx = (ly + uy) // 2, (lx + ux) // 2
        return (lx + hy, cy, ux - hy, cy) in paths or (cx, ly + hx, cx, uy - hx) in paths
    wires = [e for e in elements if e['kind'] == 0x08 and (e['layer'], e['datatype']) == p['wire_layer']
             and len(e['xy']) == 5 and signal(e)]
    require(wires, 'no signal wire on the wire layer for the deleted-wire control')
    area = lambda e: (max(x for x, _ in e['xy']) - min(x for x, _ in e['xy'])) * (
        max(y for _, y in e['xy']) - min(y for _, y in e['xy']))
    victim = max(wires, key=area)
    nowire = work / 'control_deleted_wire.gds'
    nowire.write_bytes(native_bytes[:victim['start']] + native_bytes[victim['end']:])
    x = strmxor(h, tag, nowire, reference, 'control_deleted_wire_xor')
    lvs_bad = netgen_lvs(h, tag, p, nowire, powered, 'control_deleted_wire_lvs')
    kl_bad = klayout_lvs(h, tag, p, nowire, schematic, 'control_deleted_wire_klayout_lvs')
    so_bad = signoff_lvs(h, tag, nowire, powered, 'control_deleted_wire_lvs')
    # Both tools must reject on their own (Rule 8): Netgen (basic and signoff
    # parser) and KLayout through the signoff checks. The basic KLayout verdict
    # is recorded only; it is blind to a port net that lost its devices.
    controls['deleted_wire'] = dict(detected=not x['identical'] and lvs_bad['verdict'] == 'mismatch'
                                    and so_bad['lvs_netgen']['verdict'] == 'mismatch'
                                    and so_bad['lvs_klayout']['verdict'] == 'mismatch', xor=x,
                                    netgen=lvs_bad['verdict'], klayout_lvs_basic=kl_bad['verdict'],
                                    signoff_netgen=so_bad['lvs_netgen'], signoff_klayout=so_bad['lvs_klayout'],
                                    removed_rect=victim['xy'][0:3:2])

    wrong = json.loads(Path(p['layer_map']).read_text())
    layer, purpose, target = p['wrong_map']
    for entry in wrong['layers']:
        if entry['lef'] == layer:
            entry[purpose] = [target]
    wrong_map = work / 'control_wrong_map.json'
    wrong_map.write_text(json.dumps(wrong, indent=1))
    wrong_gds = work / 'control_wrong_map.gds'
    receipt, wrong_report, _ = export(h, tag, args.rustroad, workspace, cell_gds, wrong_map, wrong_gds,
                                      f'{tag}_control_wrong_map_export')
    x = strmxor(h, tag, wrong_gds, reference, 'control_wrong_map_xor') if wrong_report else dict(identical=False)
    citations = verify_layer_map(wrong_map)
    controls['wrong_map_entry'] = dict(detected=wrong_report is not None and not x['identical']
                                       and citations['status'] == 'failed', changed=p['wrong_map'],
                                       xor=x, citation_check=citations['problems'])

    # Prefer the configured cell; the cell set depends on the synthesis
    # producer, so fall back to the first copied PDK structure.
    missing = p['missing_cell'] if p['missing_cell'] in native_structs else report['pdk_structures'][0]
    nocell = work / 'control_missing_cell.gds'
    s, e = native_structs[missing]
    nocell.write_bytes(native_bytes[:s] + native_bytes[e:])
    x = strmxor(h, tag, nocell, reference, 'control_missing_cell_xor')
    library = work / 'control_cell_library_without_cell.gds'
    s, e = pdk_structs[missing]
    library.write_bytes(pdk_bytes[:s] + pdk_bytes[e:])
    receipt, bad_report, output = export(h, tag, args.rustroad, workspace, library, p['layer_map'],
                                         work / 'control_missing_cell_export.gds', f'{tag}_control_missing_cell_export')
    controls['missing_cell_structure'] = dict(
        detected=not x['identical'] and receipt['exit_code'] != 0 and 'GDS_CELL_MISSING' in output
        and not (work / 'control_missing_cell_export.gds').exists(),
        output_xor=x, export_exit=receipt['exit_code'], export_error=output.strip()[-400:], cell=missing)

    if tag == 'sky130':
        # Without the 235/4 PR boundary the density deck divides by zero and
        # reports no items: the parser must call that a failure, not a pass.
        boundary = [e for e in elements if e['kind'] == 0x08 and (e['layer'], e['datatype']) == (235, 4)]
        require(len(boundary) == 1, 'sky130 native GDS lacks exactly one 235/4 boundary')
        unbounded = work / 'control_no_boundary.gds'
        unbounded.write_bytes(native_bytes[:boundary[0]['start']] + native_bytes[boundary[0]['end']:])
        d = klayout_drc(h, tag, p, unbounded, 'control_no_boundary_density', decks='density')
        controls['density_without_boundary'] = dict(detected=d['status'] == 'failed' and d['non_finite'],
                                                    items=d['items'], values=d['clear_area_density'])
    if tag == 'gf180':
        odd = work / 'control_odd_workspace'
        shutil.copytree(workspace, odd, symlinks=False)
        revision = (odd / 'HEAD').read_text().strip()
        design = odd / 'revisions' / revision / 'design.json'
        data = json.loads(design.read_text())
        changed = None
        for via in data['physical']['technology']['vias']:
            if via['name'] == 'Via1_HV':
                for g in via['geometries']:
                    if g['layer'] == 'Via1':
                        g['rect'][0] -= 1
                        changed = g['rect']
        require(changed is not None, 'odd-coordinate control could not plant a defect')
        design.write_text(json.dumps(data, separators=(',', ':')))
        out = work / 'control_odd.gds'
        receipt, odd_report, output = export(h, tag, args.rustroad, odd, cell_gds, p['layer_map'], out,
                                             'gf180_control_odd_coordinate')
        # The workspace reader re-derives routing vias (LEF-rule enclosure) and
        # supply vias (manufacturing grid) from this geometry, so with routed
        # LEF-rule workspaces the tamper is refused on load (INVALID_WORKSPACE)
        # before streaming; GDS_ODD_COORDINATE itself is unit-tested. Either
        # way no output may be written.
        caught = ('GDS_ODD_COORDINATE' if 'GDS_ODD_COORDINATE' in output else
                  'INVALID_WORKSPACE' if 'INVALID_WORKSPACE' in output else None)
        controls['odd_gf180_coordinate'] = dict(detected=receipt['exit_code'] != 0 and caught is not None
                                                and not out.exists(), caught_by=caught,
                                                planted='Via1_HV cut rect %s (LEF DBU)' % changed,
                                                export_exit=receipt['exit_code'], export_error=output.strip()[-400:])
        shutil.rmtree(odd)
    if controls_patch['detected'] is not None:
        controls['patches_removed'] = controls_patch
    controls['planted_drc'] = dict(detected=result['drc']['planted_control']['detected'],
                                   magic=result['drc']['planted_control']['magic']['total'],
                                   klayout=result['drc']['planted_control']['klayout'])
    result['negative_controls'] = controls
    result['layer_map_citations'] = verify_layer_map(p['layer_map'])
    result['source_workspace']['tree_sha256_after'] = tree_digest(source)
    result['source_workspace']['unchanged'] = result['source_workspace']['tree_sha256_after'] == \
        result['source_workspace']['tree_sha256']

    checks = dict(read_only=result['read_only']['status'] == 'passed',
                  determinism=result['determinism']['status'] == 'passed',
                  xor=result['xor']['status'] == 'passed', labels=result['labels']['status'] == 'passed',
                  cell_bytes=result['cell_bytes']['status'] == 'passed',
                  patches=result['patches']['status'] == 'passed',
                  drc_equal=result['drc']['status'] == 'equal_to_reference',
                  drc_clean=result['drc']['clean'],
                  lvs_equal=result['lvs']['status'] == 'equal_to_reference',
                  lvs_match=result['lvs']['lvs_verdict'] == 'match',
                  controls=all(c['detected'] for c in controls.values()),
                  citations=result['layer_map_citations']['status'] == 'passed',
                  source_unchanged=result['source_workspace']['unchanged'])
    result['checks'] = checks
    result['status'] = 'passed' if all(checks.values()) else 'failed'
    return result


def evidence(d):
    """Compact, path-free view of a summary for docs/evidence (tracked)."""
    def control(v):
        x = v.get('xor') or v.get('output_xor')
        out = {k: w for k, w in v.items() if k not in ('xor', 'output_xor')}
        out.update(xor_identical=x['identical'] if x else None, xor_differences=x['differences'] if x else None)
        return out

    def pdk(p):
        if p['status'] != 'passed' and 'export' not in p:
            return p
        r = p['export']['report']
        side = lambda s: dict(magic=p['drc'][s]['magic'], klayout=p['drc'][s]['klayout']['counts'],
                              klayout_cells=p['drc'][s]['klayout']['cells'])
        return dict(
            pdk=p['pdk'], status=p['status'], checks=p['checks'],
            native=dict(sha256=r['output']['sha256'], bytes=r['output']['bytes'], records=r['records'],
                        coordinate_divisor=r['coordinate_divisor'], lef_dbu_per_micron=r['lef_dbu_per_micron'],
                        gds_dbu_per_micron=r['gds_dbu_per_micron'], elements=r['elements'],
                        pdk_structures=r['pdk_structures'], via_structures=r['via_structures'],
                        foreign_bindings_checked=r['foreign_bindings_checked'],
                        cell_gds_sha256=r['cell_gds']['sha256'], cell_lef_sha256=r['cell_lef']['sha256'],
                        layer_map_sha256=r['layer_map']['sha256'], wall_seconds=p['export']['wall_seconds'],
                        peak_rss_kib=p['export']['peak_rss_kib']),
            reference=dict(tool='KLayout LEF/DEF reader (black box)', sha256=p['reference']['sha256'],
                           wall_seconds=p['reference']['wall_seconds'], peak_rss_kib=p['reference']['peak_rss_kib']),
            routed_def_sha256=p['routed_def']['sha256'], flow_def_sha256=p['routed_def']['flow_def_sha256'],
            source_workspace=dict(tree_sha256=p['source_workspace']['tree_sha256'],
                                  unchanged=p['source_workspace']['unchanged']),
            read_only=p['read_only'], determinism=p['determinism'],
            strmxor=dict(native_vs_reference=p['xor']['native_vs_reference']['summary'],
                         native_vs_rerun=p['xor']['native_vs_rerun']['summary']),
            labels={k: p['labels'][k] for k in ('status', 'top_labels', 'flat_texts', 'names', 'native_only',
                                                 'reference_only', 'dbu')},
            hierarchy=p['hierarchy'], cell_bytes=p['cell_bytes'],
            drc=dict(status=p['drc']['status'], drc_verdict=p['drc']['drc_verdict'], clean=p['drc']['clean'],
                     planted_control={k: v for k, v in p['drc']['planted_control'].items()},
                     native=side('native'), reference=side('reference')),
            patches=p['patches'],
            density_status=p['density_status'],
            lvs=dict(status=p['lvs']['status'], lvs_verdict=p['lvs']['lvs_verdict'], verdicts=p['lvs']['verdicts'],
                     schematic=p['lvs']['schematic'],
                     netgen=dict(native=p['lvs']['native']['netgen'], reference=p['lvs']['reference']['netgen']),
                     klayout=dict(native=p['lvs']['native']['klayout']['circuits'],
                                  reference=p['lvs']['reference']['klayout']['circuits']),
                     signoff=dict(native=p['lvs']['native']['signoff'], reference=p['lvs']['reference']['signoff']),
                     verdict_order=p['lvs']['verdict_order']),
            negative_controls={k: control(v) for k, v in p['negative_controls'].items()},
            layer_map_citations=p['layer_map_citations'], pdk_inputs={k: v['sha256'] for k, v in p['inputs'].items()})
    return dict(schema='rustroad.gds-verification-evidence.v1', status=d['status'], scope=d['scope'],
                image=dict(id=d['image'], identity=d.get('image_identity')), tools=d.get('tools'),
                rustroad_sha256=d.get('rustroad', {}).get('sha256'), harness_sha256=d.get('harness_sha256'),
                oracle_scripts=d.get('oracle_scripts'), wall_seconds=round(d['wall_seconds'], 2),
                peak_external_rss_kib=d['peak_rss_kib'],
                receipts=[dict(name=r['name'], exit_code=r['exit_code'], wall_seconds=round(r['wall_seconds'], 3),
                               peak_rss_kib=r['peak_rss_kib']) for r in d['receipts']],
                pdks=[pdk(p) for p in d['pdks']])


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--run', type=Path, required=True, help='new evidence directory (must not exist)')
    parser.add_argument('--rustroad', type=Path, default=ROOT / 'target/release/rustroad')
    parser.add_argument('--source', type=Path, default=ROOT / '.work/rc-timing/ready',
                        help='finished flow run holding <pdk>_physical workspaces (read only)')
    parser.add_argument('--pdk', choices=sorted(PDKS), action='append')
    parser.add_argument('--evidence', type=Path, help='also write the compact tracked evidence JSON here')
    args = parser.parse_args()
    args.run = args.run.resolve()
    args.rustroad = args.rustroad.resolve()
    args.source = args.source.resolve()
    args.run.mkdir(parents=True, exist_ok=False)
    start = time.monotonic()
    h = Harness(args.run)
    summary = dict(schema='rustroad.gds-verification.v1', status='unverified',
                   scope=dict(level='block', pad_ring='not_applicable', seal_ring='not_applicable',
                              density='reported_separately', ir_em_si_ocv_cppr='unchecked'),
                   image=IMAGE, pdks=[])
    try:
        free = subprocess.run(['free', '-h'], capture_output=True, text=True)
        summary['memory_preflight'] = free.stdout
        inspect = subprocess.run(['podman', 'image', 'inspect', IMAGE, '--format', '{{.Id}} {{.RepoDigests}}'],
                                 capture_output=True, text=True)
        if inspect.returncode != 0:
            raise Unverified('pinned signoff image is not available')
        summary['image_identity'] = inspect.stdout.strip()
        if not args.rustroad.is_file():
            raise Unverified('rustroad binary missing')
        summary['rustroad'] = dict(path=str(args.rustroad), sha256=sha256(args.rustroad))
        _, version = h.container('klayout_version', ['klayout', '-v'], timeout=60)
        _, magic = h.container('magic_version', ['magic', '--version'], timeout=60)
        summary['tools'] = dict(klayout=version.strip().splitlines()[0] if version.strip() else None,
                                magic=magic.strip().splitlines()[0] if magic.strip() else None)
        summary['oracle_scripts'] = {p.name: sha256(p) for p in sorted(ORACLES.iterdir())}
        summary['harness_sha256'] = sha256(Path(__file__))
        for tag in args.pdk or sorted(PDKS, reverse=True):
            try:
                summary['pdks'].append(case(h, tag, args))
            except Unverified:
                raise
            except (RuntimeError, ValueError, KeyError, OSError, json.JSONDecodeError) as error:
                summary['pdks'].append(dict(pdk=tag, status='failed', reason=str(error)))
        statuses = [p['status'] for p in summary['pdks']]
        summary['status'] = 'passed' if statuses and all(s == 'passed' for s in statuses) and \
            len(statuses) == len(args.pdk or PDKS) else 'failed'
    except Unverified as error:
        summary['status'] = 'unverified'
        summary['reason'] = str(error)
    finally:
        summary['receipts'] = h.receipts
        for r in h.receipts:
            if r['name'].endswith('_netgen') and 'netgen' not in summary.get('tools', {}):
                text = (args.run / 'receipts' / r['name'] / 'stdout.txt').read_text(errors='replace')
                m = re.search(r'^(Netgen \S+)', text, re.M)
                summary.setdefault('tools', {})['netgen'] = m.group(1) if m else None
        summary['wall_seconds'] = time.monotonic() - start
        summary['peak_rss_kib'] = max((r['peak_rss_kib'] for r in h.receipts if r['peak_rss_kib']), default=None)
        (args.run / 'summary.json').write_text(json.dumps(summary, indent=1, sort_keys=True) + '\n')
        if args.evidence:
            args.evidence.write_text(json.dumps(evidence(summary), indent=1, sort_keys=True) + '\n')
    print(json.dumps(dict(status=summary['status'], summary=str(args.run / 'summary.json'),
                          pdks={p['pdk']: p['status'] for p in summary['pdks']})))
    return {'passed': 0, 'failed': 1, 'unverified': 3}[summary['status']]


if __name__ == '__main__':
    sys.exit(main())
