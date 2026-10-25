#!/usr/bin/env python3
"""Router DRC/LVS acceptance on a black-box reference streamout.

The routed DEF under test is streamed to GDS by KLayout's LEF/DEF reader
(reference only), then checked with the foundry decks of the pinned
iic-osic-tools image: Magic drc(full), KLayout sky130A_mr.drc (feol, beol,
offgrid) or gf180mcu.drc (main deck), the antenna oracle of each PDK, the
density decks (reported separately, never folded into DRC), and LVS with Netgen
(Magic extraction) and KLayout. Results come from parsed report files, never
from exit codes. Missing tools, images or reports are failures, never passes.
Planted defects (negative controls) must be caught; a placement-only copy of
the layout is the positive control for the DRC decks.

Resource contract: one container at a time, <=4 GiB cgroup memory and no
extra swap, <=4 CPUs, 256 pids, bounded wall time per step. Measured peaks for
this 13-logic-cell block are ~0.45 GiB per KLayout run; nothing here exceeds
4 GiB, so no scale lock is needed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
HERE = Path(__file__).resolve().parent / 'router_signoff'
IMAGE = '4c48415d8bd08c04033f4777b65a96299e22c1e94d12a8ea1d9a062697c006ee'
HOST_PDK = Path('/home/cgao/.local/share/containers/storage/overlay/'
                'a2565b13ab35f5571d6aab9328dc383594b36360107bd8dd9c5516a02dc64674/diff/foss/pdks/ciel')
VERSION = 'f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7'


def pdk(tag):
    if tag == 'sky130':
        r = '/foss/pdks/sky130A'
        lib = r + '/libs.ref/sky130_fd_sc_hd'
        return dict(tag=tag, root=r, host=HOST_PDK / 'sky130/versions' / VERSION / 'sky130A',
                    tlef=lib + '/techlef/sky130_fd_sc_hd__nom.tlef', clef=lib + '/lef/sky130_fd_sc_hd.lef',
                    gds=lib + '/gds/sky130_fd_sc_hd.gds', spice=lib + '/spice/sky130_fd_sc_hd.spice',
                    map=r + '/libs.tech/klayout/tech/sky130A.map', magicrc=r + '/libs.tech/magic/sky130A.magicrc',
                    netgen=r + '/libs.tech/netgen/sky130A_setup.tcl',
                    drc=r + '/libs.tech/klayout/drc/sky130A_mr.drc',
                    density=r + '/libs.tech/klayout/drc/met_min_ca_density.lydrc',
                    lvs=r + '/libs.tech/klayout/lvs/sky130.lvs')
    r = '/foss/pdks/gf180mcuD'
    lib = r + '/libs.ref/gf180mcu_fd_sc_mcu7t5v0'
    return dict(tag=tag, root=r, host=HOST_PDK / 'gf180mcu/versions' / VERSION / 'gf180mcuD',
                tlef=lib + '/techlef/gf180mcu_fd_sc_mcu7t5v0__nom.tlef', clef=lib + '/lef/gf180mcu_fd_sc_mcu7t5v0.lef',
                gds=lib + '/gds/gf180mcu_fd_sc_mcu7t5v0.gds', spice=lib + '/spice/gf180mcu_fd_sc_mcu7t5v0.spice',
                cdl=lib + '/cdl/gf180mcu_fd_sc_mcu7t5v0.cdl',
                map=r + '/libs.tech/klayout/tech/gf180mcu.map', magicrc=r + '/libs.tech/magic/gf180mcuD.magicrc',
                netgen=r + '/libs.tech/netgen/gf180mcuD_setup.tcl',
                drc=r + '/libs.tech/klayout/tech/drc/gf180mcu.drc',
                lvs=r + '/libs.tech/klayout/tech/lvs/run_lvs.py')


def host(p, path):
    return p['host'] / path[len(p['root']) + 1:]


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


class Runner:
    def __init__(self, root):
        self.root = root
        self.receipts = []

    def container(self, label, timeout, command, env=None):
        out = self.root / 'receipts' / label
        out.mkdir(parents=True, exist_ok=False)
        args = ['podman', 'run', '--rm', '--pull=never', '--network=none', '--read-only',
                '--tmpfs=/tmp:rw,size=512m', '--user=0:0', '--memory=4g', '--memory-swap=4g',
                '--cpus=4', '--pids-limit=256', '-v', f'{self.root}:/work:rw', '-v', f'{HERE}:/tools:ro',
                '-w', '/work', '-e', 'PDK_ROOT=/foss/pdks', '-e', 'HOME=/tmp',
                '-e', 'PATH=/foss/tools/bin:/foss/tools/klayout:/usr/local/bin:/usr/bin:/bin']
        for k, v in (env or {}).items():
            args += ['-e', f'{k}={v}']
        args += ['--entrypoint=/usr/bin/timeout', IMAGE, str(timeout), '/usr/bin/time', '-v',
                 '-o', f'/work/receipts/{label}/time.txt', *command]
        start = time.monotonic()
        with (out / 'stdout.txt').open('wb') as so, (out / 'stderr.txt').open('wb') as se:
            rc = subprocess.run(args, stdin=subprocess.DEVNULL, stdout=so, stderr=se,
                                timeout=timeout + 60, check=False).returncode
        timing = (out / 'time.txt').read_text() if (out / 'time.txt').exists() else ''
        m = re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)', timing)
        receipt = dict(label=label, command=command, exit_code=rc, elapsed_seconds=time.monotonic() - start,
                       peak_rss_kib=int(m[1]) if m else None, stdout=str(out / 'stdout.txt'),
                       stderr=str(out / 'stderr.txt'))
        (out / 'receipt.json').write_text(json.dumps(receipt, indent=1))
        self.receipts.append(receipt)
        return receipt

    def text(self, receipt):
        return Path(receipt['stdout']).read_text(errors='replace') + Path(receipt['stderr']).read_text(errors='replace')


def lyrdb_items(path):
    require(path.is_file(), f'missing report {path}')
    text = path.read_text()
    require('<report-database>' in text, f'not a report database: {path}')
    counts = {}
    cells = {}
    for item in re.findall(r'<item>.*?</item>', text, re.S):
        m = re.search(r'<category>(.*?)</category>', item, re.S)
        c = re.search(r'<cell>(.*?)</cell>', item, re.S)
        key = m[1].strip().strip("'") if m else '?'
        counts[key] = counts.get(key, 0) + 1
        cell = c[1].strip() if c else '?'
        cells[cell] = cells.get(cell, 0) + 1
    return dict(items=sum(counts.values()), categories=counts, cells=cells)


def stream(r, p, label, def_path, top):
    """Copy the DEF into the run tree and stream it with the reference reader."""
    local = r.root / (label + '.def')
    if def_path.resolve() != local.resolve():
        shutil.copyfile(def_path, local)
    gds = r.root / (label + '.gds')
    rec = r.container(label + '_stream', 300, ['/foss/tools/klayout/klayout', '-b', '-r', '/tools/def2gds.py'],
                      dict(DEF='/work/' + local.name, TLEF=p['tlef'], CLEF=p['clef'], CGDS=p['gds'], MAP=p['map'],
                           OUT='/work/' + gds.name, TOP=top))
    text = r.text(rec)
    require(rec['exit_code'] == 0 and 'WROTE' in text and gds.is_file(), label + ': reference streamout failed')
    return gds, dict(def_sha256=sha256(local), gds_sha256=sha256(gds), log=re.findall(r'^(TOP .*|LAYER .*)$', text, re.M))


def magic_drc(r, p, label, gds, top):
    rec = r.container(label + '_magic_drc', 600, ['magic', '-dnull', '-noconsole', '-rcfile', p['magicrc'], '/tools/magic_drc.tcl'],
                      dict(GDS='/work/' + gds.name, CELL=top))
    text = r.text(rec)
    require('MAGIC_TOP_MISSING' not in text and 'Creating new cell' not in text, label + ': Magic top cell missing')
    require('MAGIC_DRC_DONE' in text and 'MAGIC_DRC_STYLE drc(full)' in text, label + ': Magic DRC incomplete')
    bbox = re.search(r'^MAGIC_BBOX (-?\d+) (-?\d+) (-?\d+) (-?\d+)', text, re.M)
    require(bbox and int(bbox[3]) > int(bbox[1]) and int(bbox[4]) > int(bbox[2]), label + ': Magic empty cell')
    total = int(re.search(r'^MAGIC_DRC_TOTAL (\d+)', text, re.M)[1])
    why = {m[2].strip(): int(m[1]) for m in re.finditer(r'^MAGIC_DRC_WHY (\d+) :: (.*)$', text, re.M)}
    boxes = [m[1] for m in re.finditer(r'^MAGIC_DRC_BOX (.*)$', text, re.M)][:200]
    return dict(total=total, rules=why, boxes=boxes, tech=re.search(r'^MAGIC_TECH (.*)$', text, re.M)[1],
                version=re.search(r'^MAGIC_VERSION (.*)$', text, re.M)[1], seconds=rec['elapsed_seconds'],
                peak_rss_kib=rec['peak_rss_kib'])


def klayout_drc(r, p, label, gds, top, deck):
    report = r.root / f'{label}_{deck}.lyrdb'
    if p['tag'] == 'sky130':
        if deck == 'main':
            cmd = ['/foss/tools/klayout/klayout', '-b', '-r', p['drc'], '-rd', 'input=/work/' + gds.name,
                   '-rd', 'report=/work/' + report.name, '-rd', 'feol=true', '-rd', 'beol=true', '-rd', 'offgrid=true',
                   '-rd', 'floating_met=false', '-rd', 'seal=false', '-rd', 'thr=4', '-rd', 'top_cell=' + top]
        else:
            cmd = ['/foss/tools/klayout/klayout', '-b', '-r', p['density'], '-rd', 'input=/work/' + gds.name,
                   '-rd', 'report=/work/' + report.name]
    else:
        decks = dict(main='all,-antenna,-density', antenna='antenna', density='density')[deck]
        cmd = ['/foss/tools/klayout/klayout', '-b', '-r', p['drc'], '-rd', 'input=/work/' + gds.name,
               '-rd', 'report=/work/' + report.name, '-rd', 'variant=gf180mcuD', '-rd', 'decks=' + decks,
               '-rd', 'run_mode=deep', '-rd', 'threads=4', '-rd', 'topcell=' + top]
    rec = r.container(f'{label}_klayout_{deck}', 900, cmd)
    text = r.text(rec)
    require(rec['exit_code'] == 0, f'{label}: KLayout {deck} deck failed')
    require(not re.search(r'(?m)^ERROR|Unable to open', text), f'{label}: KLayout {deck} reported an error')
    if p['tag'] == 'sky130' and deck == 'density':
        require(not re.search(r'NaN|Infinity', text), label + ': density deck saw no PR boundary')
    result = lyrdb_items(report)
    result.update(seconds=rec['elapsed_seconds'], peak_rss_kib=rec['peak_rss_kib'], report=str(report))
    return result


def magic_antenna(r, p, label, gds, top):
    rec = r.container(label + '_magic_antenna', 600, ['magic', '-dnull', '-noconsole', '-rcfile', p['magicrc'], '/tools/magic_antenna.tcl'],
                      dict(GDS='/work/' + gds.name, CELL=top, OUTDIR=f'/work/{label}_antenna'))
    text = r.text(rec)
    require('MAGIC_TOP_MISSING' not in text and 'Running antenna checks.' in text and 'antennacheck finished.' in text
            and 'MAGIC_ANTENNA_DONE' in text, label + ': Magic antenna check incomplete')
    return dict(items=len(re.findall(r'Antenna violation detected', text)), seconds=rec['elapsed_seconds'],
                peak_rss_kib=rec['peak_rss_kib'])


def subckts(text):
    blocks = {}
    for m in re.finditer(r'^\.subckt\s+(\S+)\s+([^\n]*)\n(.*?)^\.ends[^\n]*\n', text, re.S | re.M | re.I):
        body = [l for l in m[3].splitlines() if l.strip() and not l.lstrip().startswith('*')]
        blocks[m[1]] = dict(pins=m[2].split(), text=m[0], devices=sum(1 for l in body if l[0].upper() in 'MXDRCQ'))
    return blocks


def schematic(powered, library, out, rescale):
    """Structural powered Verilog -> SPICE/CDL top, keeping device-bearing cells."""
    v = re.sub(r'//.*', '', powered.read_text())
    m = re.search(r'module\s+(\S+)\s*\((.*?)\);', v, re.S)
    top = m[1]
    ports = []
    body_decls = v[m.end():]
    for name in [x.strip() for x in m[2].split(',')]:
        rng = re.search(r'(?:input|output|inout)\s*(?:wire\s*)?\[(\d+):(\d+)\]\s*' + re.escape(name) + r'\s*[;,]', body_decls) \
            or re.search(r'(?:input|output|inout)\s*(?:wire\s*)?\[(\d+):(\d+)\]\s*' + re.escape(name) + r'\b', m[2])
        if rng:
            hi, lo = int(rng[1]), int(rng[2])
            step = -1 if hi >= lo else 1
            ports += [f'{name}[{i}]' for i in range(hi, lo + step, step)]
        else:
            ports.append(name.split()[-1])
    norm = lambda s: s.strip().lstrip('\\').strip()
    alias = {norm(b): norm(a) for a, b in re.findall(r'assign\s+(\S+)\s*=\s*(\\?\S+?)\s*;', v)}
    lib = subckts(library.read_text())
    used, lines, dropped = [], [], 0
    for cell, inst, conns in re.findall(r'^\s*(\S+)\s+(\S+)\s*\((.*?)\);', v, re.S | re.M):
        if cell in ('module', 'input', 'output', 'inout', 'wire', 'assign', 'supply0', 'supply1'):
            continue
        require(cell in lib, 'schematic cell absent from PDK netlist: ' + cell)
        if lib[cell]['devices'] == 0:
            dropped += 1
            continue
        pins = dict((pn, norm(n)) for pn, n in re.findall(r'\.(\w+)\s*\(\s*(\\?[^)]*?)\s*\)', conns))
        nets = [alias.get(pins[x], pins[x]) for x in lib[cell]['pins']]
        lines.append('X%s %s %s' % (norm(inst), ' '.join(nets), cell))
        if cell not in used:
            used.append(cell)
    texts = []
    for c in used:
        t = lib[c]['text']
        if rescale:
            t = re.sub(r'\b([wlWL])=([0-9.eE+\-]+)u\b', lambda q: '%s=%gu' % (q[1], float(q[2]) * 1e-6), t)
        texts.append(t)
    out.write_text('* generated by verify_router_signoff.py from %s\n%s\n.subckt %s %s\n%s\n.ends %s\n' % (
        powered.name, '\n'.join(texts), top, ' '.join(ports), '\n'.join(lines), top))
    return dict(instances=len(lines), dropped_physical_only=dropped, cells=used)


def lvs(r, p, label, gds, top, powered):
    shutil.copyfile(powered, r.root / (label + '_powered.v'))
    rec = r.container(label + '_magic_ext', 600, ['magic', '-dnull', '-noconsole', '-rcfile', p['magicrc'], '/tools/magic_ext.tcl'],
                      dict(GDS='/work/' + gds.name, CELL=top, OUTDIR=f'/work/{label}_ext'))
    require('MAGIC_EXT_DONE' in r.text(rec) and (r.root / f'{label}_ext/{top}.spice').is_file(), label + ': extraction failed')
    out = r.root / (label + '_netgen.out')
    rec = r.container(label + '_netgen', 600, ['netgen', '-batch', 'source', '/tools/netgen_lvs.tcl'],
                      dict(LAYOUT=f'/work/{label}_ext/{top}.spice', TOP=top, REFV=f'/work/{label}_powered.v',
                           STDSPICE=p['spice'], SETUP=p['netgen'], OUT='/work/' + out.name))
    report = out.read_text() if out.is_file() else ''
    jpath = out.with_suffix('.json')
    require(jpath.is_file(), label + ': netgen JSON missing')
    entries = json.loads(jpath.read_text())
    def entry_ok(e):
        if sorted(e['pins'][0]) != sorted(e['pins'][1]):
            return False
        if 'name' not in e:  # pin-only record of a flattened device-less cell
            return True
        return (not e.get('badnets') and not e.get('badelements') and e['nets'][0] == e['nets'][1]
                and sorted(map(str, e['devices'][0])) == sorted(map(str, e['devices'][1])))
    ok = ('Final result: Circuits match uniquely.' in report and any(e.get('name', [None])[0] == top for e in entries)
          and all(entry_ok(e) for e in entries))
    netgen = dict(status='match' if ok else 'mismatch', final=re.findall(r'Final result: (.*)', report),
                  top=[e for e in entries if e.get('name', [None])[0] == top][:1], seconds=rec['elapsed_seconds'],
                  peak_rss_kib=rec['peak_rss_kib'])
    # KLayout LVS against a SPICE/CDL schematic built from the same powered netlist.
    if p['tag'] == 'sky130':
        ref = r.root / (label + '_schematic.spice')
        info = schematic(powered, host(p, p['spice']), ref, True)
        db = r.root / (label + '_klayout.lvsdb')
        rec = r.container(label + '_klayout_lvs', 900, ['/foss/tools/klayout/klayout', '-b', '-r', p['lvs'],
                          '-rd', 'input=/work/' + gds.name, '-rd', 'report=/work/' + db.name,
                          '-rd', 'schematic=/work/' + ref.name, '-rd', f'target_netlist=/work/{label}_klayout.cir',
                          '-rd', 'thr=4', '-rd', 'run_mode=deep', '-rd', 'top_cell=' + top, '-rd', 'convert_subckts=true',
                          '-rd', 'combine=true', '-rd', 'schematic_simplify=true'])
        text = r.text(rec)
        kl_ok = rec['exit_code'] == 0 and 'Congratulations! Netlists match.' in text and db.is_file()
    else:
        ref = r.root / (label + '_schematic.cdl')
        info = schematic(powered, host(p, p['cdl']), ref, False)
        run_dir = r.root / (label + '_klayout_lvs')
        rec = r.container(label + '_klayout_lvs', 900, ['python3', p['lvs'], '--layout=/work/' + gds.name,
                          '--netlist=/work/' + ref.name, '--variant=D', '--topcell=' + top, '--run_mode=deep',
                          '--run_dir=/work/' + run_dir.name, '--thr=4'])
        text = r.text(rec)
        kl_ok = 'Congratulations! Netlists match.' in text and "Netlists don't match" not in text
    klayout = dict(status='match' if kl_ok else 'mismatch', schematic=info, seconds=rec['elapsed_seconds'],
                   peak_rss_kib=rec['peak_rss_kib'])
    return dict(status='match' if ok and kl_ok else 'mismatch', netgen=netgen, klayout=klayout)


def planted(def_text, tag, kind):
    """Insert one deliberate defect into a copy of routed DEF NETS.

    The defect is attached to the first vertical access-layer (met2/Metal2)
    wire path of the NETS section that is at least four widths long; it is
    inserted right after that path, so it belongs to the same net. Path order
    inside a net is not assumed (export-def writes merged runs by layer).
    """
    sky = tag == 'sky130'
    w, s = (140, 140) if sky else (560, 560)
    access = 'met2' if sky else 'Metal2'
    nets = def_text.index('\nNETS ')
    m = None
    for c in re.finditer(r'(?m)^  (?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)', def_text[nets:]):
        x0, y0, x1, y1 = map(int, c.groups()[1:])
        if c[1] == access and x0 == x1 and abs(y1 - y0) >= 4 * w:
            m = c
            break
    require(m is not None, 'no vertical access-layer wire for planted control')
    layer, x0, y0, x1, y1 = m[1], *map(int, m.groups()[1:])
    if kind == 'notch':
        # A same-net island parallel to the wire, half a spacing away.
        rect = (x0 + w // 2 + s // 2, min(y0, y1), x0 + w // 2 + s // 2 + w, min(y0, y1) + 4 * w)
    else:
        # A lone same-net square below the layer minimum area.
        rect = (x0 + 10 * w, min(y0, y1), x0 + 10 * w + w, min(y0, y1) + w)
    patch = f'  NEW {layer} ( {rect[0]} {rect[1]} ) RECT ( 0 0 {rect[2] - rect[0]} {rect[3] - rect[1]} )\n'
    end = nets + m.end()
    return def_text[:end] + '\n' + patch.rstrip('\n') + def_text[end:]


def placement_only(def_text):
    """Strip signal routing (positive control: cells, PG, pins only)."""
    out, inside = [], False
    for line in def_text.splitlines():
        if line.startswith('NETS '):
            inside = True
        elif line.startswith('END NETS'):
            inside = False
        if inside and (line.startswith('  + ROUTED') or line.startswith('  NEW')):
            if line.rstrip().endswith(';'):
                out[-1] = out[-1].rstrip() + ' ;'
            continue
        out.append(line)
    return '\n'.join(out) + '\n'


def drc_suite(r, p, label, def_path, top, antenna=True, density=True):
    gds, stream_info = stream(r, p, label, def_path, top)
    res = dict(stream=stream_info, magic=magic_drc(r, p, label, gds, top), klayout=klayout_drc(r, p, label, gds, top, 'main'))
    if antenna:
        res['antenna'] = magic_antenna(r, p, label, gds, top) if p['tag'] == 'sky130' else klayout_drc(r, p, label, gds, top, 'antenna')
        res['antenna']['oracle'] = 'magic_antennacheck' if p['tag'] == 'sky130' else 'klayout_gf180mcu_antenna_deck'
    if density:
        res['density'] = klayout_drc(r, p, label, gds, top, 'density')
    res['drc_status'] = 'passed' if res['magic']['total'] == 0 and res['klayout']['items'] == 0 else 'failed'
    if antenna:
        res['antenna_status'] = 'passed' if res['antenna']['items'] == 0 else 'failed'
    if density:
        res['density_status'] = 'passed' if res['density']['items'] == 0 else 'failed'
    return res, gds


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--run-dir', type=Path, required=True)
    ap.add_argument('--flow', type=Path, help='verify_routing.py workspace with <tag>.def and <tag>_powered.v')
    ap.add_argument('--pdk', default='sky130,gf180')
    ap.add_argument('--access-def', action='append', default=[], help='tag=path of a pin-access test DEF')
    ap.add_argument('--rustroad', type=Path, help='binary used to export pin-access test DEFs from <flow>/<tag>_physical')
    ap.add_argument('--top', default='lane_smoke')
    ap.add_argument('--no-controls', action='store_true')
    a = ap.parse_args()
    root = a.run_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    r = Runner(root)
    started = time.monotonic()
    summary = dict(schema='rustroad.router-signoff.v1', image=IMAGE, status='unverified', pdks={},
                   scope=dict(level='block', pad_ring='absent', seal_ring='absent', ir_em_si_ocv_cppr='unchecked',
                              streamout='klayout_lefdef_reference', density='reported_separately'))
    try:
        inspect = subprocess.run(['podman', 'image', 'inspect', '--format', '{{.Id}}', IMAGE], capture_output=True, text=True, timeout=60)
        require(inspect.returncode == 0 and inspect.stdout.strip() == IMAGE, 'pinned signoff image missing')
        rec = r.container('tool_versions', 60, ['sh', '-c', 'magic --version; netgen -batch quit 2>&1 | head -3; /foss/tools/klayout/klayout -v'])
        summary['tool_versions'] = r.text(rec).split('Command being timed')[0].strip().splitlines()[:8]
        accesses = dict(x.split('=', 1) for x in a.access_def)
        for tag in a.pdk.split(','):
            p = pdk(tag)
            entry = dict(inputs={})
            for key in ('tlef', 'clef', 'gds', 'map', 'magicrc', 'netgen', 'drc', 'lvs', 'spice') + (('cdl',) if tag == 'gf180' else ('density',)):
                entry['inputs'][key] = dict(path=p[key], sha256=sha256(host(p, p[key])))
            if a.flow:
                def_path = a.flow / (tag + '.def')
                res, gds = drc_suite(r, p, tag, def_path, a.top)
                res['lvs'] = lvs(r, p, tag, gds, a.top, a.flow / (tag + '_powered.v'))
                entry['routed'] = res
                if not a.no_controls:
                    text = def_path.read_text()
                    controls = {}
                    for kind in ('notch', 'min_area'):
                        bad = root / f'{tag}_bad_{kind}.def'
                        bad.write_text(planted(text, tag, kind))
                        c, _ = drc_suite(r, p, f'{tag}_bad_{kind}', bad, a.top, antenna=False, density=False)
                        controls[kind] = dict(magic=c['magic']['total'], klayout=c['klayout']['items'],
                                              magic_rules=c['magic']['rules'], klayout_rules=c['klayout']['categories'],
                                              status='rejected' if c['magic']['total'] > res['magic']['total']
                                              and c['klayout']['items'] > res['klayout']['items'] else 'missed')
                    clean = root / f'{tag}_placement_only.def'
                    clean.write_text(placement_only(text))
                    c, _ = drc_suite(r, p, f'{tag}_placement_only', clean, a.top, antenna=False, density=False)
                    controls['placement_only'] = dict(magic=c['magic']['total'], klayout=c['klayout']['items'],
                                                      magic_rules=c['magic']['rules'], klayout_rules=c['klayout']['categories'])
                    swapped = root / f'{tag}_swap_reference.v'
                    pv = (a.flow / (tag + '_powered.v')).read_text()
                    # Swap one input net between two different logic instances.
                    pa = '.A(' if tag == 'sky130' else '.A1('
                    i = pv.index(pa)
                    j = pv.index(pa, pv.index(';', i))
                    na = pv[i + len(pa):pv.index(')', i)]
                    nb = pv[j + len(pa):pv.index(')', j)]
                    require(na.strip() != nb.strip(), 'swap control needs distinct nets')
                    pv = pv[:i] + pa + nb + pv[pv.index(')', i):j] + pa + na + pv[pv.index(')', j):]
                    swapped.write_text(pv)
                    bad_lvs = lvs(r, p, f'{tag}_swapped', gds, a.top, swapped)
                    controls['lvs_swapped_pins'] = dict(netgen=bad_lvs['netgen']['status'], klayout=bad_lvs['klayout']['status'],
                                                        status='rejected' if bad_lvs['netgen']['status'] == 'mismatch'
                                                        and bad_lvs['klayout']['status'] == 'mismatch' else 'missed')
                    entry['controls'] = controls
            if a.rustroad and a.flow and tag not in accesses:
                # Production profile plus a legacy-mode copy as a detection control.
                profile = ROOT / 'fixtures/pdk' / (tag + '-routing.json')
                legacy = root / (tag + '-legacy-routing.json')
                cfg = json.loads(profile.read_text())
                cfg['lef_rules'] = False
                legacy.write_text(json.dumps(cfg))
                for name, prof in ((tag + '_access.def', profile), (tag + '_access_legacy.def', legacy)):
                    with (root / name).open('wb') as f:
                        rc = subprocess.run([str(a.rustroad), '--workspace', str(a.flow / (tag + '_physical')), 'export-access-test',
                                             '--config', str(prof)], stdout=f, stderr=subprocess.PIPE, timeout=120).returncode
                    require(rc == 0, 'export-access-test failed for ' + name)
                accesses[tag] = str(root / (tag + '_access.def'))
                c, _ = drc_suite(r, p, tag + '_access_legacy', root / (tag + '_access_legacy.def'), 'rustroad_access_test',
                                 antenna=False, density=False)
                entry['access_legacy_control'] = dict(magic=c['magic']['total'], klayout=c['klayout']['items'],
                                                      magic_rules=c['magic']['rules'], klayout_rules=c['klayout']['categories'],
                                                      note='informational: same layout routed with lef_rules=false')
            if tag in accesses:
                res, _ = drc_suite(r, p, tag + '_access', Path(accesses[tag]), 'rustroad_access_test', antenna=False, density=False)
                res['tests'] = len(re.findall(r'(?m)^- acc\d+ \(', Path(accesses[tag]).read_text()))
                res['def_sha256'] = sha256(Path(accesses[tag]))
                entry['access_test'] = res
            summary['pdks'][tag] = entry
        ok = True
        for tag, e in summary['pdks'].items():
            if 'routed' in e:
                rt = e['routed']
                ok &= rt['drc_status'] == 'passed' and rt['antenna_status'] == 'passed' and rt['lvs']['status'] == 'match'
                if 'controls' in e:
                    c = e['controls']
                    ok &= all(c[k]['status'] == 'rejected' for k in ('notch', 'min_area', 'lvs_swapped_pins'))
                    ok &= c['placement_only']['magic'] == 0 and c['placement_only']['klayout'] == 0
            if 'access_test' in e:
                ok &= e['access_test']['drc_status'] == 'passed'
        summary['status'] = 'passed' if ok else 'failed'
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        summary['status'] = 'failed'
        summary['reason'] = str(error)
    summary['wall_seconds'] = time.monotonic() - started
    summary['peak_rss_kib'] = max((x['peak_rss_kib'] or 0 for x in r.receipts), default=None)
    summary['receipts'] = r.receipts
    (root / 'summary.json').write_text(json.dumps(summary, indent=1))
    brief = {t: {k: (v.get('drc_status'), v.get('magic', {}).get('total'), v.get('klayout', {}).get('items'),
                     v.get('antenna', {}).get('items'), v.get('density', {}).get('items'),
                     v.get('lvs', {}).get('status')) for k, v in e.items() if isinstance(v, dict) and isinstance(v.get('magic'), dict)}
             for t, e in summary['pdks'].items()}
    print(json.dumps(dict(status=summary['status'], reason=summary.get('reason'), brief=brief,
                          summary=str(root / 'summary.json'))))
    return 0 if summary['status'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
