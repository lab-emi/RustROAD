#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate fixtures/pdk/<pdk>-delay-cells.json (schema rustroad.delay-cells.v1).

Everything comes from PDK data: the three corner Liberty files fixed by the
owner decision (sky130 tt_025C_1v80 / ss_100C_1v60 / ff_n40C_1v95; gf180
tt_025C_3v30 / ss_125C_3v00 / ff_n40C_3v60), the cell LEF and the tracked
cell policy (fixtures/flow/cell-policy.json). Nothing is fitted to timing
results.

Selection:
- buffers: cells allowed after synthesis for hold ECO by the cell policy
  (post_synthesis_insertable.cts_hold) except clock-tree cells (clkbuf*,
  clkinv*), plus the logic buffers buf_1/buf_2 (allowed everywhere). A cell is
  kept only if, in every corner, it has exactly one signal input and one
  output whose Liberty `function` is that input (identity) and whose timing
  arc is positive_unate combinational. Others are listed in `excluded` with
  the reason. pnr_exclude cells are never selected.
- antenna_cells: LEF `CLASS CORE ANTENNACELL` macros allowed as
  physical_only by the policy, with one signal pin and LEF ANTENNADIFFAREA.

Delays are bilinear lookups of cell_rise/cell_fall at one representative
(input transition, load) point per PDK, without extrapolation.

Usage: build_delay_cells.py [--pdk sky130|gf180|all] [--out-dir fixtures/pdk] [--check]
"""
from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import liberty_lite as lib  # noqa: E402

SCHEMA = 'rustroad.delay-cells.v1'
CORNERS = {
    'sky130': [('tt', 'tt_025C_1v80', 'nom'), ('ss', 'ss_100C_1v60', 'max'), ('ff', 'ff_n40C_1v95', 'min')],
    'gf180': [('tt', 'tt_025C_3v30', 'nom'), ('ss', 'ss_125C_3v00', 'max'), ('ff', 'ff_n40C_3v60', 'min')],
}
# Representative operating point: a single-gate data driver into one flop D
# pin plus a short wire. Recorded in the profile; the closure loop re-times.
REPRESENTATIVE = {
    'sky130': dict(input_slew_ns=0.05, load_pf=0.005),
    'gf180': dict(input_slew_ns=0.15, load_pf=0.01),
}
CLOCK_RESERVED = ('*__clkbuf_*', '*__clkinv*', '*__clkinvlp*')
LOGIC_BUFFERS = ('*__buf_1', '*__buf_2')


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1 << 20), b''):
            h.update(chunk)
    return h.hexdigest()


def source(role: str, path: Path) -> dict:
    return dict(role=role, path=str(path), sha256=sha256(path), bytes=path.stat().st_size)


def matches(name: str, globs) -> bool:
    return any(fnmatch.fnmatchcase(name, g) for g in globs)


def lef_macros(path: Path) -> dict:
    """CLASS, SIZE and per-pin direction/use/antenna areas of every MACRO."""
    text = path.read_text()
    out = {}
    for m in re.finditer(r'(?ms)^MACRO\s+(\S+)\s*$(.*?)^END\s+\1\s*$', text):
        body = m[2]
        cls = re.search(r'(?m)^\s*CLASS\s+([^;]+);', body)
        size = re.search(r'(?m)^\s*SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)\s*;', body)
        pins = {}
        for p in re.finditer(r'(?ms)^\s*PIN\s+(\S+)\s*$(.*?)^\s*END\s+\1\s*$', body):
            pb = p[2]
            d = re.search(r'DIRECTION\s+(\w+)', pb)
            u = re.search(r'USE\s+(\w+)', pb)
            gate = re.findall(r'ANTENNAGATEAREA\s+([0-9.]+)', pb)
            diff = re.findall(r'ANTENNADIFFAREA\s+([0-9.]+)', pb)
            pins[p[1]] = dict(direction=d[1] if d else None, use=u[1] if u else 'SIGNAL',
                              gate_area_um2=sum(map(float, gate)) if gate else None,
                              diff_area_um2=sum(map(float, diff)) if diff else None)
        out[m[1]] = dict(cls=' '.join(cls[1].split()) if cls else None,
                         size_um=[float(size[1]), float(size[2])] if size else None, pins=pins)
    return out


def buffer_data(library: lib.Library, name: str, rep: dict):
    """Return (data, None) for a proven non-inverting buffer, else (None, reason)."""
    cell = library.cells.get(name)
    if cell is None:
        return None, 'absent from this corner Liberty'
    if cell.attr('dont_use', 'false').lower() == 'true':
        return None, 'Liberty dont_use'
    pins = cell.sub('pin')
    inputs = [p for p in pins if p.attr('direction') == 'input']
    outputs = [p for p in pins if p.attr('direction') == 'output']
    if len(inputs) != 1 or len(outputs) != 1 or len(pins) != 2:
        return None, f'{len(inputs)} inputs / {len(outputs)} outputs (needs exactly one of each)'
    a, z = inputs[0], outputs[0]
    function = z.attr('function')
    if function is None:
        return None, 'output has no Liberty function'
    if re.sub(r'[\s()"]', '', function) != a.name:
        return None, f'Liberty function {function!r} is not the identity of {a.name}'
    arcs = [t for t in z.sub('timing') if t.attr('related_pin') == a.name]
    if len(arcs) != 1:
        return None, f'{len(arcs)} timing arcs from {a.name}'
    arc = arcs[0]
    sense, kind = arc.attr('timing_sense'), arc.attr('timing_type', 'combinational')
    if sense != 'positive_unate' or kind != 'combinational':
        return None, f'timing arc {sense}/{kind} is not positive_unate combinational'
    t, c = library.time_unit_ns(), library.cap_unit_pf()
    slew, load = rep['input_slew_ns'] / t, rep['load_pf'] / c

    def look(kind):
        groups = arc.sub(kind)
        if len(groups) != 1:
            raise lib.LibertyError(f'{name}: {kind} table count {len(groups)}')
        return round(lib.Table(groups[0], library).lookup(slew, load) * t, 9)
    cap = a.attr('capacitance')
    area = cell.attr('area')
    return dict(input_pin=a.name, output_pin=z.name, function=function, area_um2=float(area),
                input_cap_pf=round(float(cap) * c, 9), timing_sense=sense,
                delay_ns=dict(rise=look('cell_rise'), fall=look('cell_fall')),
                output_slew_ns=dict(rise=look('rise_transition'), fall=look('fall_transition'))), None


def build(pdk: str, profiles: dict, policy: dict, policy_path: Path) -> dict:
    p = next(x for x in profiles['pdks'] if x['tag'] == pdk)
    tt = Path(p['liberty']['path'])
    if sha256(tt) != p['liberty']['sha256']:
        raise SystemExit(f'{pdk}: tt Liberty identity mismatch')
    lef = Path(p['cell_lef']['path'])
    if sha256(lef) != p['cell_lef']['sha256']:
        raise SystemExit(f'{pdk}: cell LEF identity mismatch')
    pol = policy['pdks'][pdk]
    hold_globs = pol['post_synthesis_insertable']['cts_hold']
    physical = pol['post_synthesis_insertable']['physical_only']
    pnr_exclude = pol['pnr_exclude']
    corner_paths = []
    for corner, libname, rc in CORNERS[pdk]:
        path = Path(str(tt).replace(CORNERS[pdk][0][1], libname))
        if not path.is_file():
            raise SystemExit(f'{pdk}: corner Liberty {path} missing')
        corner_paths.append((corner, libname, rc, path))
    macros = lef_macros(lef)
    first = lib.Library(corner_paths[0][3], wanted=set())
    names = sorted(first.all_cells)
    candidates = [n for n in names if (matches(n, hold_globs) or matches(n, LOGIC_BUFFERS))
                  and not matches(n, CLOCK_RESERVED)]
    antenna = [n for n in names if matches(n, physical) and macros.get(n, {}).get('cls')
               and 'ANTENNACELL' in macros[n]['cls'].upper()]
    rep = REPRESENTATIVE[pdk]
    libraries = {c: lib.Library(path, wanted=set(candidates) | set(antenna)) for c, _, _, path in corner_paths}
    excluded = []
    for n in names:
        if matches(n, hold_globs) and matches(n, CLOCK_RESERVED):
            excluded.append(dict(name=n, reason='reserved for the clock tree (selection policy)'))
    buffers = []
    for n in candidates:
        if matches(n, pnr_exclude):
            excluded.append(dict(name=n, reason='pnr_exclude in the cell policy'))
            continue
        if n not in macros:
            excluded.append(dict(name=n, reason='absent from the cell LEF'))
            continue
        per, reason = {}, None
        for c, _, _, _ in corner_paths:
            data, reason = buffer_data(libraries[c], n, rep)
            if data is None:
                reason = f'{c}: {reason}'
                break
            per[c] = data
        if reason:
            excluded.append(dict(name=n, reason=reason))
            continue
        ref = per['tt']
        for c, d in per.items():
            if (d['input_pin'], d['output_pin'], d['area_um2']) != (ref['input_pin'], ref['output_pin'], ref['area_um2']):
                raise SystemExit(f'{n}: pins/area differ between corners')
        lp = macros[n]['pins']
        if set(k for k, v in lp.items() if v['use'] == 'SIGNAL') != {ref['input_pin'], ref['output_pin']}:
            excluded.append(dict(name=n, reason='LEF signal pins differ from Liberty'))
            continue
        buffers.append(dict(
            name=n, input_pin=ref['input_pin'], output_pin=ref['output_pin'], function=ref['function'],
            function_check='identity: Liberty function equals the only input pin in every corner',
            timing_sense='positive_unate', area_um2=ref['area_um2'], lef_size_um=macros[n]['size_um'],
            policy_class='logic' if matches(n, LOGIC_BUFFERS) else 'cts_hold',
            input_cap_pf={c: d['input_cap_pf'] for c, d in per.items()},
            delay_ns={c: d['delay_ns'] for c, d in per.items()},
            output_slew_ns={c: d['output_slew_ns'] for c, d in per.items()}))
    cells = []
    for n in antenna:
        signal = {k: v for k, v in macros[n]['pins'].items() if v['use'] == 'SIGNAL'}
        if len(signal) != 1:
            excluded.append(dict(name=n, reason='antenna cell needs exactly one signal pin'))
            continue
        pin, data = next(iter(signal.items()))
        if data['direction'] != 'INPUT' or not data['diff_area_um2']:
            excluded.append(dict(name=n, reason='antenna pin is not an INPUT with ANTENNADIFFAREA'))
            continue
        lib_cell = libraries['tt'].cells.get(n)
        lib_pins = [g for g in lib_cell.sub('pin')] if lib_cell else []
        if lib_cell is None or [g.name for g in lib_pins] != [pin] or any(g.attr('function') for g in lib_pins):
            excluded.append(dict(name=n, reason='Liberty pin set differs or has an output function'))
            continue
        cells.append(dict(name=n, pin=pin, area_um2=float(lib_cell.attr('area')), lef_size_um=macros[n]['size_um'],
                          lef_diff_area_um2=data['diff_area_um2'], lef_gate_area_um2=data['gate_area_um2'] or 0.0,
                          policy_class='physical_only'))
    excluded.sort(key=lambda e: e['name'])
    return dict(
        schema=SCHEMA, pdk=pdk, library=p['name'],
        generator='scripts/build_delay_cells.py',
        sources=[source(f'liberty_{c}', path) for c, _, _, path in corner_paths] + [source('cell_lef', lef)],
        cell_policy=dict(path=str(policy_path.relative_to(ROOT)), sha256=sha256(policy_path)),
        corners=[dict(name=c, liberty=libname, rc=rc) for c, libname, rc, _ in corner_paths],
        representative=dict(rep, reason='one gate driving one flop D pin plus a short wire; '
                            'the closure loop re-times every ECO with extracted parasitics'),
        selection=dict(hold_globs=hold_globs, logic_buffers=list(LOGIC_BUFFERS),
                       clock_reserved=list(CLOCK_RESERVED), pnr_exclude=pnr_exclude,
                       antenna_globs=physical),
        buffers=buffers, antenna_cells=cells, excluded=excluded)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--pdk', choices=('sky130', 'gf180', 'all'), default='all')
    ap.add_argument('--profiles', type=Path, default=ROOT / 'fixtures/pdk/local-routing.json')
    ap.add_argument('--cell-policy', type=Path, default=ROOT / 'fixtures/flow/cell-policy.json')
    ap.add_argument('--out-dir', type=Path, default=ROOT / 'fixtures/pdk')
    ap.add_argument('--check', action='store_true', help='fail if the tracked files differ')
    a = ap.parse_args()
    profiles = json.loads(a.profiles.read_text())
    policy = json.loads(a.cell_policy.read_text())
    status = 0
    for pdk in (('sky130', 'gf180') if a.pdk == 'all' else (a.pdk,)):
        data = build(pdk, profiles, policy, a.cell_policy.resolve())
        text = json.dumps(data, indent=1, sort_keys=False) + '\n'
        out = a.out_dir / f'{pdk}-delay-cells.json'
        if a.check:
            same = out.is_file() and out.read_text() == text
            print(f'{out}: {"identical" if same else "DIFFERS"}')
            status |= 0 if same else 1
        else:
            out.write_text(text)
            print(f'{out}: {len(data["buffers"])} buffers, {len(data["antenna_cells"])} antenna cells, '
                  f'{len(data["excluded"])} excluded')
    return status


if __name__ == '__main__':
    sys.exit(main())
