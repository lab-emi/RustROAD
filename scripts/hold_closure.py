#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Bounded dual-PDK hold-closure loop on native extraction (RustROAD + RustSys + RustSTA).

Per PDK, iteration k (k = 0 .. --max-iterations-1):
  netlist_k -> import-pdk -> place -> clock-tree --sdc -> route clock -> route all
  -> native extract min/nom/max -> RustSTA 0.4 (3 Liberty corners x 3 SPEF corners)
  -> hold_fix (target margin) + endpoints per corner
  -> closed? stop : rustroad hold-eco -> netlist_{k+1}
netlist_0 is the pinned RustSys producer's mapped netlist with the cell policy.
Decisions use native SPEF only. Every iteration is recorded.

Final iteration checks (gating): hold >= margin and setup >= 0 at tt+nom,
ss+max, ff+min in RustSTA; OpenSTA 3.1.0 on the same native SPEF meets the
same thresholds with per-endpoint parity <= 1 ps; RustSys `check synth`
RTL vs the final post-ECO post-CTS netlist; Yosys CEC (+ negative control);
512-vector Verilator simulation; Yosys sat proofs that every inserted cell
is a non-inverting buffer (+ wrong-polarity control) and a native ECO
rejection of a profiled inverting cell; native GDS foundry DRC (Magic +
KLayout) on the final layout not worse than iteration 0; frozen Liberty,
SDC, RC-profile, delay-cell-profile and cell-policy hashes are unchanged.
Reported (not gating): OpenSTA on OpenRCX SPEF of the collinear-merged DEF.

Third-party tools run only as black-box executables. Exit 0 passed, 1 failed, 3 partial.

`Flow` is also the physical/closure engine of scripts/rtl2gds.py: an optional
`Design` (top, RTL files, tracked floorplan, rendered SDC) replaces the
pipeline.v defaults, and `Flow.close()` runs the bounded loop alone. Without a
`Design` every command and verdict of this script is unchanged.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
sys.path.insert(0, str(ROOT / 'scripts/signoff'))
from verify_physical import PhysicalHarness, run_ok, fingerprint, external_cec, require, IMAGE, OPENROAD  # noqa: E402
from verify_logic import bounded_text, sha256, write_json, tcl_path  # noqa: E402
from verify_taps import clock_function_proof  # noqa: E402
import verify_routing  # noqa: E402

# RustSTA 0.4.0 fix-round-2 binaries (RustSTA d1f61e6), copied unchanged from the
# timing stream's .work/sta-signoff/bin/round2 into this checkout's .work/tools/sta;
# the shared directory is replaced by later rounds, so identity is pinned by hash.
STA_BIN = ROOT / '.work/tools/sta'
STA_SHA = {'ruststa': 'ac76f0f3765ce3c6d44f85257f7d836120a7e0bf6a7283a75357f37fd79aa838',
           'rusteda': 'c8f71b63124577174f8458870d4ec006ab14b35f3b249e6e9d52a1734e759baf'}
CORNERS = {
    'sky130': [('tt', 'tt_025C_1v80', 'nom'), ('ss', 'ss_100C_1v60', 'max'), ('ff', 'ff_n40C_1v95', 'min')],
    'gf180': [('tt', 'tt_025C_3v30', 'nom'), ('ss', 'ss_125C_3v00', 'max'), ('ff', 'ff_n40C_3v60', 'min')],
}
OPENRCX_RULES = {'sky130': '.calibre', 'gf180': ''}
POSTROUTE_SDC_TAIL = '''set_input_delay 0.2 -clock core [get_ports {en a b}]
set_output_delay 0.2 -clock core [get_ports {y z}]
set_input_transition 0.1 [get_ports {clk en a b}]
set_load 0.01 [get_ports {y z}]
set_propagated_clock [get_clocks core]
'''
PARITY_NS = 1e-3
# Planted long-wire-to-gate antenna controls: a straight wire of the I/O layer's
# LEF minimum width, attached to an input port pin and leaving the die outward
# (same port and lengths as the signoff harness controls; GDS layer = the
# layer map's net layer for that LEF layer).
PLANT = {'sky130': dict(port='a[1]', length_um=600, gds_layer='70/20'),
         'gf180': dict(port='a[1]', length_um=2000, gds_layer='42/0')}
ROW = re.compile(r'^(\S+)\s+\([^\n]*?\)\s+(-?[0-9.eE+]+)\s+(-?[0-9.eE+]+)\s+(-?[0-9.eE+]+)\s+\((?:MET|VIOLATED)\)\s*$', re.M)


class Design:
    """What `Flow` builds. The default is this script's pipeline fixture.

    top: top module; rtl: RTL source files; floorplan: tracked floorplan JSON
    copied byte-for-byte (None: the generated pipeline floorplan); sdc_text:
    the complete post-route SDC (None: the CTS SDC + POSTROUTE_SDC_TAIL);
    control_sink: (instance, pin) the native ECO polarity controls buffer.
    """

    def __init__(self, top='lane_smoke', rtl=None, floorplan=None, sdc_text=None, control_sink=('r0', 'D')):
        self.top = top
        self.rtl = [Path(x) for x in (rtl or [ROOT / 'fixtures/flow/pipeline.v'])]
        self.floorplan = Path(floorplan) if floorplan else None
        self.sdc_text = sdc_text
        self.control_sink = tuple(control_sink)


def jdump(path: Path, value) -> Path:
    write_json(path, value)
    return path


class Flow:
    def __init__(self, h, args, tools, p, policy, design: Design | None = None):
        self.h, self.args, self.tools, self.p, self.policy = h, args, tools, p, policy
        self.design = design or Design()
        self.top = self.design.top
        self.tag = p['tag']
        lib_tt = Path(p['liberty']['path'])
        self.libs = {c: Path(str(lib_tt).replace(CORNERS[self.tag][0][1], name)) for c, name, _ in CORNERS[self.tag]}
        self.rc = {c: rc for c, _, rc in CORNERS[self.tag]}
        self.rc_profiles = {c: ROOT / f'fixtures/pdk/{self.tag}-rc-{rc}.json' for c, _, rc in CORNERS[self.tag]}
        self.delay_cells = ROOT / f'fixtures/pdk/{self.tag}-delay-cells.json'
        self.config = Path(args.physical_config.get(self.tag, p['config']))
        self.routing = Path(p['routing_config'])
        self.sink_pin = json.loads(self.config.read_text())['clock']['sink_pin']
        self.sdc = h.root / f'{self.tag}_postroute.sdc'
        self.sdc.write_text(self.design.sdc_text if self.design.sdc_text is not None
                            else Path(p['cts_sdc']).read_text() + POSTROUTE_SDC_TAIL)
        rules = Path(p['extraction_rules']['path'])
        self.openrcx_rules = {c: rules.with_name(f'rules.openrcx.{"sky130A" if self.tag == "sky130" else "gf180mcuD"}.{rc}{OPENRCX_RULES[self.tag]}')
                              for c, _, rc in CORNERS[self.tag]}

    # ------------------------------------------------------------ inputs
    def frozen(self) -> dict:
        files = {f'liberty_{c}': v for c, v in self.libs.items()}
        files.update({f'rc_profile_{c}': v for c, v in self.rc_profiles.items()})
        files.update(sdc_postroute=self.sdc, sdc_cts=Path(self.p['cts_sdc']), delay_cells=self.delay_cells,
                     cell_policy=self.args.cell_policy, routing_profile=self.routing, physical_config=self.config,
                     technology_lef=Path(self.p['technology_lef']['path']), cell_lef=Path(self.p['cell_lef']['path']),
                     cell_gds=Path(self.p['tap_reference']['gds']['path']),
                     gds_layer_map=ROOT / f'fixtures/pdk/{self.tag}-gds-map.json')
        files.update({f'openrcx_rules_{c}': v for c, v in self.openrcx_rules.items()})
        return {k: fingerprint(v) for k, v in sorted(files.items())}

    # ------------------------------------------------------------ physical
    def synth(self) -> Path:
        mapped = self.h.root / f'{self.tag}_mapped.v'
        args = self.policy['pdks'][self.tag]['rustsys_args']
        run_ok(self.h, f'{self.tag}_synth', [self.tools['rustsys'], '--threads', '2', '--progress', 'off', 'synth', '--backend', 'cpu',
                                            '--top', self.top, '--lib', self.libs['tt'], *args, '--out', mapped, '--print', 'json',
                                            *self.design.rtl])
        return mapped

    def floorplan(self) -> Path:
        sky = self.tag == 'sky130'
        u, sw, sh = (1000, 460, 2720) if sky else (2000, 1120, 7840)
        floor = self.h.root / f'{self.tag}_floor.json'
        if not floor.exists() and self.design.floorplan is not None:
            floor.write_bytes(self.design.floorplan.read_bytes())
        if not floor.exists():
            write_json(floor, dict(name='lane_smoke', units_per_micron=u, die_area=dict(lx=0, ly=0, ux=125 * sw, uy=18 * sh),
                                   core_area=dict(lx=20 * sw, ly=5 * sh, ux=120 * sw, uy=13 * sh),
                                   site=dict(name='unithd' if sky else 'GF018hv5v_mcu_sc7', width=sw, height=sh),
                                   rows=[dict(x_start=20 * sw, x_end=120 * sw, y=(5 + i) * sh) for i in range(8)]))
        return floor

    def physical(self, it: int, netlist: Path) -> dict:
        t, h = f'{self.tag}_i{it}', self.h
        ws = h.root / f'{t}_physical'
        road = [self.tools['rustroad'], '--workspace', ws, '--threads', '2', '--max-cells', '2000', '--max-rows', '128']
        rec = {}
        a = h.json_command(f'{t}_import', list(map(str, road + ['import-pdk', '--base-rev', 'rev0', '--tech-lef', self.p['technology_lef']['path'],
                                                                 '--cell-lef', self.p['cell_lef']['path'], '--netlist', netlist,
                                                                 '--floorplan', self.floorplan(), '--physical-config', self.config])))
        a = h.json_command(f'{t}_place', list(map(str, road + ['place', '--base-rev', a['revision']])))
        a = h.json_command(f'{t}_cts', list(map(str, road + ['clock-tree', '--base-rev', a['revision'], '--sdc', self.p['cts_sdc']])))
        rec['cts'] = a
        a = h.json_command(f'{t}_clock_route', list(map(str, road + ['route', '--base-rev', a['revision'], '--scope', 'clock', '--config', self.routing])))
        a = h.json_command(f'{t}_all_route', list(map(str, road + ['route', '--base-rev', a['revision'], '--scope', 'all', '--config', self.routing])))
        rec['routed_revision'] = a['revision']
        out = {}
        for label, extra, suffix in (('def', ['export-def'], '.def'), ('netlist', ['export-verilog'], '_exported.v'),
                                     ('powered', ['export-verilog', '--include-power'], '_powered.v')):
            r = run_ok(h, f'{t}_export_{label}', road + extra)
            out[label] = h.root / (t + suffix)
            out[label].write_bytes(Path(r['stdout']).read_bytes())
        routes_path = h.root / f'{t}_routes.json'
        routes_path.write_bytes(Path(run_ok(h, f'{t}_export_routes', road + ['export-routes'])['stdout']).read_bytes())
        routes = json.loads(bounded_text(routes_path))['routes']
        out['routes'] = routes_path
        rec.update(workspace=str(ws), road=[str(x) for x in road], files={k: fingerprint(v) for k, v in out.items()},
                   paths={k: str(v) for k, v in out.items()}, wires=len(routes['wires']), vias=len(routes['vias']),
                   patches=len(routes.get('patches', [])), routed_nets=len(routes['nets']))
        rec['spef'] = {}
        for c in self.libs:
            spef = h.root / f'{t}_{self.rc[c]}.spef'
            x = h.json_command(f'{t}_extract_{self.rc[c]}', list(map(str, road + ['extract', '--rc-profile', self.rc_profiles[c],
                                                                                    '--corner', self.rc[c], '--out', spef])))
            require(x['status'] == 'ok' and not x['unannotated_signal_nets'], f'{t}: native extraction incomplete')
            rec['spef'][c] = dict(path=str(spef), sha256=x['output']['sha256'], rc=self.rc[c], diagnostics=x['diagnostics'])
        return rec

    # ------------------------------------------------------------ timing
    def sta(self, it: int, phys: dict, label: str = 'sta', spefs: dict | None = None, netlist: Path | None = None) -> dict:
        t, h = f'{self.tag}_i{it}_{label}', self.h
        ws = h.root / f'{t}_ws'
        rusteda, ruststa = str(self.tools['rusteda']), str(self.tools['ruststa'])
        common = ['--workspace', str(ws), '--threads', '2', '--format', 'json', '--color', 'never']
        a = h.json_command(f'{t}_init', [rusteda, *common, 'init'], True)
        argv = [rusteda, *common, 'import', '--base-rev', a['new_rev']]
        for c in self.libs:
            argv += ['--lib', str(self.libs[c]), '--corner', c]
        timed = Path(netlist or phys['paths']['netlist'])
        timed_sha = sha256(timed)  # bound into hold-eco (--timed-netlist-sha256)
        argv += ['--netlist', str(timed), '--sdc', str(self.sdc)]
        for c in self.libs:
            argv += ['--spef', str((spefs or {}).get(c) or phys['spef'][c]['path']), '--spef-corner', c]
        argv += ['--max-rc-nodes', '200000', '--max-rc-elements', '200000']
        a = h.json_command(f'{t}_import', argv, True)
        r = h.json_command(f'{t}_run', [ruststa, *common, 'run', '--base-rev', a['new_rev'], '--backend', 'cpu', '--precision', 'f64',
                                        '--max-paths', '16', '--max-state-gib', '1', '--require-wire-model', 'spef_rc'], True)
        require(r['scope'].get('clock_model') == 'propagated' and r['scope'].get('wire_model') == 'spef_rc', f'{t}: wrong timing scope')
        require(r['completeness']['unconstrained_endpoints'] == 0, f'{t}: unconstrained endpoints')
        rev = r['new_rev']
        files = {}
        hold = h.root / f'{t}_hold_fix.json'
        rr = run_ok(h, f'{t}_hold_fix', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'hold_fix',
                                         '--target-margin', str(self.args.target_margin), '--limit', '0'])
        hold.write_bytes(Path(rr['stdout']).read_bytes())
        files['hold_fix'] = hold
        corners = {}
        for c in self.libs:
            ep = h.root / f'{t}_endpoints_{c}.json'
            rr = run_ok(h, f'{t}_endpoints_{c}', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'endpoints',
                                                  '--corner', c, '--limit', '0'])
            ep.write_bytes(Path(rr['stdout']).read_bytes())
            files[f'endpoints_{c}'] = ep
            rows = json.loads(bounded_text(ep))['sections']['endpoints']['rows']
            pins = json.loads(bounded_text(Path(run_ok(h, f'{t}_clock_pins_{c}', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'pins',
                                                                                  '--corner', c, '--filter', f'pin~/{self.sink_pin}', '--limit', '0'])['stdout'])))
            sinks = [x for x in pins['sections']['pins']['rows'] if x['pin'].endswith('/' + self.sink_pin)]
            require(sinks and all(x['arrival_late_ns'] is not None for x in sinks), f'{t}: clock sink arrivals missing at {c}')
            clock = dict(sinks=len(sinks), latency_max_ns=max(x['arrival_late_ns'] for x in sinks),
                         latency_min_ns=min(x['arrival_early_ns'] for x in sinks), slew_max_ns=max(x['slew_late_ns'] for x in sinks))
            corners[c] = dict(clock=clock, worst_setup_ns=min(x['setup_slack_ns'] for x in rows), worst_hold_ns=min(x['hold_slack_ns'] for x in rows),
                              setup_violations=sum(x['setup_slack_ns'] < 0 for x in rows),
                              hold_below_margin=sum(x['hold_slack_ns'] < self.args.target_margin for x in rows),
                              endpoints={x['endpoint']: dict(setup=x['setup_slack_ns'], hold=x['hold_slack_ns']) for x in rows})
        clocks = run_ok(h, f'{t}_clocks', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'clocks', '--limit', '0'])
        closed = all(v['worst_hold_ns'] >= self.args.target_margin and v['worst_setup_ns'] >= 0 for v in corners.values())
        return dict(workspace=str(ws), revision=rev, timed_netlist=str(timed), timed_netlist_sha256=timed_sha,
                    scope=r['scope'], metrics=r['metrics'], completeness=r['completeness'],
                    diagnostics=[d for d in r.get('diagnostics', []) if d.get('severity') != 'note'][:20],
                    corners=corners, closed=closed, files={k: str(v) for k, v in files.items()},
                    clocks_report=str(clocks['stdout']))

    def hold_eco(self, it: int, netlist: Path, sta: dict) -> tuple[Path, dict]:
        t = f'{self.tag}_i{it}'
        out, plan = self.h.root / f'{self.tag}_i{it + 1}_mapped.v', self.h.root / f'{t}_hold_plan.json'
        argv = [self.tools['rustroad'], 'hold-eco', '--netlist', netlist, '--timed-netlist', sta['timed_netlist'],
                '--timed-netlist-sha256', sta['timed_netlist_sha256'], '--hold-report', sta['files']['hold_fix'],
                '--delay-cells', self.delay_cells, '--setup-corner', 'ss', '--target-margin-ns', self.args.target_margin,
                '--setup-margin-ns', self.args.setup_margin, '--out', out, '--plan-out', plan]
        for c in self.libs:
            argv += ['--hold-corner', c, '--setup-report', sta['files'][f'endpoints_{c}']]
        r = self.h.run(f'{t}_hold_eco', [str(x) for x in argv])
        result = json.loads(bounded_text(plan)) if plan.exists() else None
        require(r['exit_code'] == 0, f'{t}: hold-eco failed: ' + bounded_text(Path(r['stderr']))[-600:])
        return out, result

    # ------------------------------------------------------------ antenna
    def check_antenna(self, label: str, workspace: str, extra: Path | None = None) -> dict:
        argv = [self.tools['rustroad'], '--workspace', workspace, 'check-antenna']
        if extra is not None:
            argv += ['--extra-shapes', extra]
        out = self.h.root / f'{label}_antenna.json'
        r = run_ok(self.h, f'{label}_check_antenna', argv)
        out.write_bytes(Path(r['stdout']).read_bytes())
        d = json.loads(bounded_text(out))
        require(d['schema'] == 'rustroad.antenna-check.v1', f'{label}: antenna report schema')
        return dict(status=d['status'], violations=d['violations'], report=str(out),
                    violated=[dict(net=c['net'], layer=c['layer'], kind=c['kind'], ratio=c['ratio'], limit=c['limit'], rule=c['rule'])
                              for c in d['checks'] if c['violated']][:20],
                    worst=[dict(net=c['net'], layer=c['layer'], ratio=round(c['ratio'], 3), limit=c['limit']) for c in d['checks'][:3]],
                    scope=d['scope'], stats=d['stats'])

    def antenna_eco(self, label: str, netlist: Path, report: str, out: Path) -> dict:
        plan = self.h.root / f'{label}_antenna_plan.json'
        r = self.h.run(f'{label}_antenna_eco', [str(x) for x in [self.tools['rustroad'], 'antenna-eco', '--netlist', netlist,
                                                                  '--antenna-report', report, '--delay-cells', self.delay_cells,
                                                                  '--out', out, '--plan-out', plan]])
        result = json.loads(bounded_text(plan)) if plan.exists() else None
        require(r['exit_code'] == 0, f'{label}: antenna-eco failed: ' + bounded_text(Path(r['stderr']))[-600:])
        return result

    def plant_shape(self, phys: dict, port: str | None = None) -> dict:
        cfg = dict(PLANT[self.tag], **({'port': port} if port else {}))
        text = bounded_text(Path(phys['paths']['def']), 64 * 1024 ** 2)
        die = re.search(r'(?m)^DIEAREA \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \) ;', text)
        pin = re.search(r'(?m)^- ' + re.escape(cfg['port']) + r' \+ NET \S+ .*?\+ LAYER (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \) '
                        r'\+ FIXED \( (-?\d+) (-?\d+) \) N ;', text)
        require(die is not None and pin is not None, f'{self.tag}: port {cfg["port"]} pin or DIEAREA not found in DEF')
        layer = pin[1]
        x, y = int(pin[6]), int(pin[7])
        lx, ly, ux, uy = x + int(pin[2]), y + int(pin[3]), x + int(pin[4]), y + int(pin[5])
        tlef = Path(self.p['technology_lef']['path']).read_text()
        w = re.search(r'(?ms)^LAYER ' + re.escape(layer) + r'\b.*?^\s*WIDTH\s+([0-9.]+)\s*;', tlef)
        require(w is not None, f'{self.tag}: LEF WIDTH of {layer} not found')
        dbu = 1000 if self.tag == 'sky130' else 2000
        width = round(float(w[1]) * dbu)
        length = cfg['length_um'] * dbu
        require(width <= min(ux - lx, uy - ly), f'{self.tag}: planted wire wider than the pin')
        d = [int(v) for v in die.groups()]
        if ly == d[1]:
            rect = [lx, ly - length, lx + width, uy]
        elif uy == d[3]:
            rect = [lx, ly, lx + width, uy + length]
        elif lx == d[0]:
            rect = [lx - length, ly, ux, ly + width]
        else:
            require(ux == d[2], f'{self.tag}: port pin is not on the die boundary')
            rect = [lx, ly, ux + length, ly + width]
        return dict(net=cfg['port'], layer=layer, rect=rect, length_um=cfg['length_um'], width_dbu=width, pin_rect=[lx, ly, ux, uy])

    def export_gds(self, label: str, phys: dict) -> tuple[Path, dict]:
        gds = self.h.root / f'{label}.gds'
        x = self.h.json_command(f'{label}_export_gds', [str(v) for v in [self.tools['rustroad'], '--workspace', phys['workspace'],
                                                                            'export-gds', '--cell-gds', self.p['tap_reference']['gds']['path'],
                                                                            '--layer-map', ROOT / f'fixtures/pdk/{self.tag}-gds-map.json', '--out', gds]])
        return gds, x

    def plant_gds(self, label: str, gds: Path, export: dict, shape: dict) -> Path:
        import container  # noqa: E402
        divisor = export['coordinate_divisor']
        require(all(v % divisor == 0 for v in shape['rect']), f'{label}: planted rectangle off the GDS grid')
        rect = ','.join(str(v // divisor) for v in shape['rect'])
        work = self.h.root / f'{label}_plant'
        out = work / 'planted.gds'
        cmd = ['/foss/tools/klayout/klayout', '-b', '-r', '/scripts/kl_plant.py']
        for k, v in dict(gds='/in/design.gds', out='/out/planted.gds', report='/out/planted.json', kind='rect',
                         metal=PLANT[self.tag]['gds_layer'], rect=rect).items():
            cmd += ['-rd', f'{k}={v}']
        r = container.run(cmd, out_dir=work, label=container.next_label(work, 'plant_rect'),
                          ro=[(str(ROOT / 'scripts/signoff'), '/scripts'), (str(gds.resolve()), '/in/design.gds')],
                          memory='2g', cpus=1, threads=1, timeout=300)
        require(r.get('exit_code') == 0 and out.is_file(), f'{label}: planting failed')
        return out

    def deck_antenna(self, label: str, gds: Path, phys: dict) -> dict:
        import signoff as signoff_harness  # noqa: E402
        ns = argparse.Namespace(gds=gds, top=self.top, pdk=self.tag, out=self.h.root / f'{label}_signoff',
                                netlist=Path(phys['paths']['powered']), against=None, against_top=None,
                                manifest=self.args.manifest, threads=2, memory='4g', timeout=900, plant=None)
        summary = signoff_harness.run_checks(ns, ['antenna'])
        jdump(ns.out / 'signoff.json', summary)
        return dict(status=summary['statuses']['antenna'], count=summary['counts'].get('antenna'),
                    by_rule=summary['by_rule'].get('antenna'), summary=str(ns.out / 'signoff.json'))

    def antenna_controls(self, it: int, netlist: Path, phys: dict) -> dict:
        """Planted long wire on an input port: native must flag it, the foundry
        deck must flag it, and after a diode ECO and a fresh layout both pass."""
        t = f'{self.tag}_antctl'
        shape = self.plant_shape(phys)
        extra = jdump(self.h.root / f'{t}_extra.json', [{k: shape[k] for k in ('net', 'layer', 'rect')}])
        native = self.check_antenna(f'{t}_planted', phys['workspace'], extra)
        flagged = native['status'] == 'violated' and any(v['net'] == shape['net'] for v in native['violated'])
        require(flagged, f'{t}: native check did not flag the planted wire')
        out = dict(shape=shape, native_planted=native)
        gds, export = self.export_gds(f'{t}_base', phys)
        out['deck_planted'] = self.deck_antenna(f'{t}_planted', self.plant_gds(f'{t}_planted', gds, export, shape), phys)
        repaired = self.h.root / f'{t}_repaired_mapped.v'
        plan = self.antenna_eco(f'{t}', netlist, native['report'], repaired)
        out['antenna_eco'] = dict(status=plan['status'], nets=plan['nets'], cell=plan['cell'])
        rphys = self.physical('antrep', repaired)
        rshape = self.plant_shape(rphys)
        rextra = jdump(self.h.root / f'{t}_repaired_extra.json', [{k: rshape[k] for k in ('net', 'layer', 'rect')}])
        out['repaired_shape'] = rshape
        out['native_repaired'] = self.check_antenna(f'{t}_repaired', rphys['workspace'], rextra)
        out['native_repaired_unplanted'] = self.check_antenna(f'{t}_repaired_unplanted', rphys['workspace'])
        rgds, rexport = self.export_gds(f'{t}_repaired', rphys)
        out['deck_repaired'] = self.deck_antenna(f'{t}_repaired', self.plant_gds(f'{t}_repaired', rgds, rexport, rshape), rphys)
        rsta = self.sta('antrep', rphys)
        out['repaired_timing'] = dict(closed=rsta['closed'], corners={c: dict(worst_setup_ns=d['worst_setup_ns'], worst_hold_ns=d['worst_hold_ns'])
                                                                       for c, d in rsta['corners'].items()})
        ok = (out['native_planted']['status'] == 'violated' and out['deck_planted']['status'] == 'failed'
              and out['native_repaired']['status'] == 'passed' and out['deck_repaired']['status'] == 'passed')
        out['status'] = 'passed' if ok else 'failed'
        return out

    # ------------------------------------------------------------ OpenSTA
    def opensta(self, label: str, corner: str, netlist: Path, spef: Path) -> dict:
        h = self.h
        script = h.root / f'{label}.tcl'
        script.write_text(f'''read_liberty {tcl_path(self.libs[corner])}
read_verilog {tcl_path(netlist)}
link_design {self.top}
read_sdc {tcl_path(self.sdc)}
read_spef {tcl_path(spef)}
report_units
puts RUSTROAD_SETUP_BEGIN
report_checks -path_delay max -group_path_count 1000 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_SETUP_END
puts RUSTROAD_HOLD_BEGIN
report_checks -path_delay min -group_path_count 1000 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_HOLD_END
puts RUSTROAD_LATENCY_BEGIN
report_clock_latency -digits 12
puts RUSTROAD_LATENCY_END
exit
''')
        r = run_ok(h, label, [self.tools['opensta'], '-no_init', '-no_splash', '-exit', script])
        text = bounded_text(Path(r['stdout'])) + bounded_text(Path(r['stderr']))
        require(not re.search(r'(?mi)^\s*Error(?:\s+\d+)?:', text), f'{label}: OpenSTA error')
        require(re.search(r'(?mi)^\s*Time\s+1ns\s*$', text), f'{label}: OpenSTA units')

        def section(name):
            a, b = f'RUSTROAD_{name}_BEGIN', f'RUSTROAD_{name}_END'
            require(text.count(a) == 1 and text.count(b) == 1, f'{label}: report markers')
            return text.split(a, 1)[1].split(b, 1)[0]
        values = {k: {m[1]: float(m[4]) for m in ROW.finditer(section(k.upper()))} for k in ('setup', 'hold')}
        lat = [float(x) for x in re.findall(r'^\s*(-?[0-9.]+)\s+network latency ', section('LATENCY'), re.M)]
        return dict(setup=values['setup'], hold=values['hold'], latency_ns=lat, receipt=label)

    def compare(self, label: str, rust: dict, ref: dict) -> dict:
        diffs = []
        for c, data in rust['corners'].items():
            names = set(data['endpoints'])
            r = ref[c]
            require(set(r['setup']) == names and set(r['hold']) == names, f'{label}: endpoint coverage differs at {c}')
            for e, v in data['endpoints'].items():
                for kind in ('setup', 'hold'):
                    a, b = v[kind], r[kind][e]
                    require(math.isfinite(a) and math.isfinite(b), f'{label}: nonfinite slack')
                    diffs.append(dict(corner=c, endpoint=e, kind=kind, ruststa_ns=a, opensta_ns=b, difference_ns=abs(a - b)))
        worst = max(d['difference_ns'] for d in diffs)
        ref_ok = all(min(ref[c]['hold'].values()) >= self.args.target_margin and min(ref[c]['setup'].values()) >= 0 for c in ref)
        return dict(status='passed' if worst <= PARITY_NS else 'different', tolerance_ns=PARITY_NS, max_difference_ns=worst,
                    opensta_closed=ref_ok,
                    opensta_worst={c: dict(setup=min(ref[c]['setup'].values()), hold=min(ref[c]['hold'].values())) for c in ref},
                    worst_rows=sorted(diffs, key=lambda d: -d['difference_ns'])[:8], comparisons=len(diffs))

    # ------------------------------------------------------------ reference RC (reported)
    def openrcx_merged(self, it: int, phys: dict) -> dict:
        """OpenRCX SPEF of the geometry-identical collinear-merged DEF (reported, not gating)."""
        h, t = self.h, f'{self.tag}_i{it}'
        merged = h.root / f'{t}_merged.def'
        routes = Path(phys['paths']['routes'])
        # OpenRCX ignores DEF routing RECT patches (router stream lesson) and the
        # merge tool predates them: drop the patch lines (keeping a net's final ';')
        # so the merged DEF carries exactly the wires and vias OpenRCX extracts.
        nopatch = h.root / f'{t}_nopatch.def'
        patch = re.compile(r'^(\s*)NEW \S+ \( -?\d+ -?\d+ \) RECT \( -?\d+ -?\d+ -?\d+ -?\d+ \)(\s*;)?\s*$')
        lines, dropped = [], 0
        for line in bounded_text(Path(phys['paths']['def']), 64 * 1024 ** 2).split('\n'):
            m = patch.match(line)
            if m:
                dropped += 1
                if m[2]:
                    lines.append(m[1] + ';')
                continue
            lines.append(line)
        nopatch.write_text('\n'.join(lines))
        require(dropped == phys['patches'], f'{t}: patch line count {dropped} != routed patches {phys["patches"]}')
        run_ok(h, f'{t}_merge_def', [sys.executable, '-I', ROOT / 'scripts/compare_spef.py', 'merge-def', '--def', nopatch,
                                      '--routes', routes, '--out', merged])
        spefs = {}
        for c in self.libs:
            spef = h.root / f'{t}_openrcx_{self.rc[c]}.spef'
            script = h.root / f'{t}_openrcx_{self.rc[c]}.tcl'
            script.write_text(f'''read_lef /technology.lef
read_lef /cells.lef
read_liberty /library.lib
read_def /run/{merged.name}
define_process_corner -ext_model_index 0 {self.rc[c]}
set_extraction_rules_file /extraction.rules
extract_parasitics -max_res 0 -coupling_threshold 0.1 -no_merge_via_res
write_spef /run/{spef.name}
puts EXTRACTION_COMPLETE
exit
''')
            command = ['podman', 'run', '--rm', '--pull=never', '--network=none', '--read-only', '--tmpfs=/tmp:rw,size=64m',
                       '--memory=2g', '--memory-swap=2g', '--cpus=2', '--pids-limit=128', f'--volume={h.root}:/run:rw',
                       f'--volume={self.p["technology_lef"]["path"]}:/technology.lef:ro', f'--volume={self.p["cell_lef"]["path"]}:/cells.lef:ro',
                       f'--volume={self.libs[c]}:/library.lib:ro', f'--volume={self.openrcx_rules[c]}:/extraction.rules:ro',
                       '--entrypoint=/usr/bin/timeout', IMAGE, '60s', OPENROAD, '-no_init', '-no_splash', '-threads', '2', '-exit', f'/run/{script.name}']
            r = run_ok(h, f'{t}_openrcx_{self.rc[c]}', command)
            require('EXTRACTION_COMPLETE' in bounded_text(Path(r['stdout'])) and spef.is_file(), f'{t}: OpenRCX failed')
            spefs[c] = spef
        rust = self.sta(it, phys, label='sta_openrcx', spefs=spefs)
        ref = {c: self.opensta(f'{t}_opensta_openrcx_{c}', c, Path(phys['paths']['netlist']), spefs[c]) for c in self.libs}
        cmp = self.compare(f'{t}_openrcx', rust, ref)
        return dict(status='reported', merged_def=fingerprint(merged), dropped_patch_lines=dropped, rules={c: str(v) for c, v in self.openrcx_rules.items()},
                    ruststa=dict(closed=rust['closed'], corners={c: {k: v for k, v in d.items() if k != 'endpoints'} for c, d in rust['corners'].items()}),
                    opensta=cmp, note='OpenRCX ignores DEF RECT patches (router stream); the b0 stream owns DEF merging')

    # ------------------------------------------------------------ proofs and controls
    def cell_proofs(self, netlist: Path) -> dict:
        """Yosys sat proofs for every inserted buffer master + a native wrong-polarity control."""
        profile = json.loads(self.delay_cells.read_text())
        text = netlist.read_text()
        used = sorted({b['name'] for b in profile['buffers'] if re.search(rf'(?m)^\s*{re.escape(b["name"])}\s', text)})
        proofs = {}
        for name in used:
            b = next(x for x in profile['buffers'] if x['name'] == name)
            for c in self.libs:
                proofs[f'{name}@{c}'] = clock_function_proof(self.h, f'{self.tag}_proof_{name}_{c}',
                                                             dict(cell=name, input_pin=b['input_pin'], output_pin=b['output_pin']),
                                                             self.libs[c], self.tools['yosys'])
        # Native control: a profile that lists an inverting cell with its real Liberty function is refused.
        inv_name = f'{profile["library"]}__clkinv_1'
        inv_fn = re.search(r'cell\s*\(\s*"?' + re.escape(inv_name) + r'"?\s*\)\s*\{.*?function\s*:\s*"([^"]+)"',
                           bounded_text(self.libs['tt'], 64 * 1024 ** 2), re.S)
        require(inv_fn is not None, 'inverter function not found for the control')
        bad = json.loads(json.dumps(profile))
        pins = ('A', 'Y') if self.tag == 'sky130' else ('I', 'ZN')
        bad['buffers'].append(dict(profile['buffers'][0], name=inv_name, input_pin=pins[0], output_pin=pins[1], function=inv_fn[1]))
        bad_profile = jdump(self.h.root / f'{self.tag}_control_inverting_profile.json', bad)
        plan = jdump(self.h.root / f'{self.tag}_control_inverting_plan.json', dict(
            schema='rustroad.eco-plan.v1', module=self.top,
            operations=[dict(op='insert_buffer', cell=inv_name, instance='rr_ctl_inv_0', net='rr_ctl_inv_0_n',
                             sink=dict(instance=self.design.control_sink[0], pin=self.design.control_sink[1]))]))
        out = self.h.root / f'{self.tag}_control_inverting.v'
        r = self.h.run(f'{self.tag}_control_inverting_eco', [str(self.tools['rustroad']), 'apply-eco', '--netlist', str(netlist), '--plan', str(plan),
                                                              '--delay-cells', str(bad_profile), '--out', str(out)])
        err = bounded_text(Path(r['stderr']))
        require(r['exit_code'] != 0 and 'ECO_NOT_BUFFER' in err and not out.exists(), 'inverting ECO cell was not rejected')
        # The same plan with a real profiled buffer is accepted (positive control).
        ok_plan = jdump(self.h.root / f'{self.tag}_control_buffer_plan.json', dict(
            schema='rustroad.eco-plan.v1', module=self.top,
            operations=[dict(op='insert_buffer', cell=profile['buffers'][0]['name'], instance='rr_ctl_buf_0', net='rr_ctl_buf_0_n',
                             sink=dict(instance=self.design.control_sink[0], pin=self.design.control_sink[1]))]))
        ok_out = self.h.root / f'{self.tag}_control_buffer.v'
        run_ok(self.h, f'{self.tag}_control_buffer_eco', [self.tools['rustroad'], 'apply-eco', '--netlist', netlist, '--plan', ok_plan,
                                                          '--delay-cells', self.delay_cells, '--out', ok_out])
        # Clock-tree masters actually instantiated (default cell and any level_cells).
        clock = json.loads(self.config.read_text())['clock']
        clock_cells = sorted({c for c in [clock['cell'], *clock.get('level_cells', [])]
                              if re.search(rf'(?m)^\s*{re.escape(c)}\s', text)})
        for name in clock_cells:
            for c in self.libs:
                proofs[f'{name}@{c}'] = clock_function_proof(self.h, f'{self.tag}_proof_{name}_{c}',
                                                             dict(cell=name, input_pin=clock['input_pin'], output_pin=clock['output_pin']),
                                                             self.libs[c], self.tools['yosys'])
        return dict(status='proved' if used else 'no_cells_inserted', cells=used, clock_cells=clock_cells, proofs=proofs,
                    native_wrong_polarity=dict(status='rejected', cell=inv_name, liberty_function=inv_fn[1], code='ECO_NOT_BUFFER'),
                    native_positive_control='accepted')

    def equivalence(self, it: int, final_netlist: Path) -> dict:
        h, t, rtl = self.h, f'{self.tag}_final', ROOT / 'fixtures/flow/pipeline.v'
        native = h.json_command(f'{t}_rustsys_check', list(map(str, [self.tools['rustsys'], '--threads', '2', '--progress', 'off', 'check', 'synth',
                                                                     '--top', 'lane_smoke', '--lib', self.libs['tt'], '--netlist', final_netlist,
                                                                     '--budget', '10000', '--model-budget', '10000', '--sweep-max-candidates', '256',
                                                                     '--print', 'json', rtl])))
        require(native.get('verdict') == 'equivalent', 'RustSys check synth: not equivalent')
        cec = external_cec(h, f'{t}_cec', self.tools['yosys'], self.libs['tt'], rtl, final_netlist)
        bad = h.root / f'{t}_bad.sv'
        bad.write_text(rtl.read_text().replace('assign z = a & b;', 'assign z = ~(a & b);'))
        external_cec(h, f'{t}_cec_negative', self.tools['yosys'], self.libs['tt'], bad, final_netlist, expected=False)
        sim = verify_routing.simulation(h, t, self.tools, self.libs['tt'], rtl, final_netlist)
        return dict(rustsys_check_synth=native.get('verdict'), yosys_cec=cec['status'], yosys_cec_negative='rejected', simulation=sim)

    def gds_drc(self, label: str, phys: dict) -> dict:
        """Native GDS + foundry DRC (Magic, KLayout) and antenna via the pinned signoff image."""
        import signoff as signoff_harness  # noqa: E402
        h = self.h
        gds = h.root / f'{label}.gds'
        x = h.json_command(f'{label}_export_gds', [str(v) for v in phys['road'] + ['export-gds', '--cell-gds', self.p['tap_reference']['gds']['path'],
                                                                                 '--layer-map', ROOT / f'fixtures/pdk/{self.tag}-gds-map.json', '--out', gds]])
        ns = argparse.Namespace(gds=gds, top=self.top, pdk=self.tag, out=h.root / f'{label}_signoff', netlist=Path(phys['paths']['powered']),
                                against=None, against_top=None, manifest=self.args.manifest, threads=2, memory='4g', timeout=900, plant=None)
        summary = signoff_harness.run_checks(ns, ['drc-magic', 'drc-klayout', 'antenna', 'lvs-netgen', 'lvs-klayout'])
        jdump(ns.out / 'signoff.json', summary)
        return dict(gds=x['output'], statuses=summary['statuses'], counts=summary['counts'], by_rule=summary['by_rule'],
                    summary=str(ns.out / 'signoff.json'), peak_rss_kib=summary['resources']['peak_rss_kib'])

    # ------------------------------------------------------------ driver
    def close(self, netlist: Path) -> tuple[list, tuple]:
        """The bounded closure loop from a mapped netlist: returns the
        per-iteration records and (iteration, netlist, physical, sta, antenna)
        of the final iteration."""
        iterations, final = [], None
        for it in range(self.args.max_iterations):
            phys = self.physical(it, netlist)
            sta = self.sta(it, phys)
            record = dict(iteration=it, netlist=fingerprint(netlist), physical={k: v for k, v in phys.items() if k not in ('road',)},
                          timing=dict(closed=sta['closed'], corners={c: {k: v for k, v in d.items() if k != 'endpoints'} for c, d in sta['corners'].items()},
                                      endpoints={c: d['endpoints'] for c, d in sta['corners'].items()}, files=sta['files'],
                                      metrics=sta['metrics'], diagnostics=sta['diagnostics']))
            antenna = self.check_antenna(f'{self.tag}_i{it}', phys['workspace'])
            record['antenna'] = antenna
            iterations.append(record)
            final = (it, netlist, phys, sta, antenna)
            # 'partial' (unmodeled LEF antenna statements) cannot be repaired here;
            # it ends the loop like 'passed' and fails the native_antenna gate.
            if sta['closed'] and antenna['status'] != 'violated':
                break
            if it + 1 == self.args.max_iterations:
                break
            # Antenna repair targets a timing-closed layout: while the hold ECO is
            # still changing the layout, a violation on a layout that is about to
            # be replaced is recorded but not repaired.
            if not sta['closed']:
                netlist, plan = self.hold_eco(it, netlist, sta)
                record['hold_eco'] = dict(status=plan['status'], totals=plan['totals'], sinks=plan['sinks'],
                                          plan=fingerprint(self.h.root / f'{self.tag}_i{it}_hold_plan.json'))
            elif antenna['status'] == 'violated':
                repaired = self.h.root / f'{self.tag}_i{it + 1}_mapped_ant.v'
                plan = self.antenna_eco(f'{self.tag}_i{it}', netlist, antenna['report'], repaired)
                record['antenna_eco'] = dict(status=plan['status'], nets=plan['nets'])
                netlist = repaired
        return iterations, final

    def run(self) -> dict:
        start = time.monotonic()
        frozen_before = self.frozen()
        iterations, final = self.close(self.synth())
        it, netlist, phys, sta, antenna = final
        result = dict(pdk=self.tag, iterations=iterations, final_iteration=it, closed=sta['closed'], antenna=antenna,
                      config=fingerprint(self.config), delay_cells=fingerprint(self.delay_cells))
        exported = Path(phys['paths']['netlist'])
        ref = {c: self.opensta(f'{self.tag}_final_opensta_{c}', c, exported, Path(phys['spef'][c]['path'])) for c in self.libs}
        result['opensta_native'] = self.compare(f'{self.tag}_final_native', sta, ref)
        result['opensta_native']['clock_latency_ns'] = {c: ref[c]['latency_ns'] for c in ref}
        result['equivalence'] = self.equivalence(it, exported)
        result['cell_proofs'] = self.cell_proofs(exported)
        if self.args.openrcx:
            try:
                result['openrcx_merged'] = self.openrcx_merged(it, phys)
            except (RuntimeError, OSError, KeyError, ValueError) as e:
                result['openrcx_merged'] = dict(status='unverified', reason=str(e))
        if self.args.antenna_controls:
            try:
                result['antenna_controls'] = self.antenna_controls(it, netlist, phys)
            except (RuntimeError, OSError, KeyError, ValueError) as e:
                result['antenna_controls'] = dict(status='failed', reason=str(e))
        if self.args.signoff:
            first = iterations[0]['physical']
            first_phys = dict(first, road=[str(x) for x in [self.tools['rustroad'], '--workspace', first['workspace'], '--threads', '2',
                                                             '--max-cells', '2000', '--max-rows', '128']])
            result['drc_before'] = self.gds_drc(f'{self.tag}_i0_native', first_phys)
            result['drc_after'] = self.gds_drc(f'{self.tag}_i{it}_native', phys) if it != 0 else result['drc_before']
            b, a = result['drc_before']['counts'], result['drc_after']['counts']
            worse = [k for k in ('drc-magic', 'drc-klayout', 'antenna') if a.get(k) is None or b.get(k) is None or a[k] > b[k]]
            result['drc_not_worse'] = dict(status='passed' if not worse else 'failed', worse_or_unverified=worse, before=b, after=a)
        frozen_after = self.frozen()
        changed = sorted(k for k in frozen_before if frozen_before[k]['sha256'] != frozen_after[k]['sha256'])
        result['frozen_inputs'] = dict(status='unchanged' if not changed else 'changed', changed=changed, files=frozen_before)
        gates = dict(
            ruststa_closed=sta['closed'],
            opensta_closed=result['opensta_native']['opensta_closed'],
            parity=result['opensta_native']['status'] == 'passed',
            rustsys_cec=result['equivalence']['rustsys_check_synth'] == 'equivalent',
            yosys_cec=result['equivalence']['yosys_cec'] == 'passed',
            simulation=result['equivalence']['simulation']['status'] == 'passed',
            cell_proofs=result['cell_proofs']['status'] in ('proved', 'no_cells_inserted'),
            native_antenna=antenna['status'] == 'passed',
            frozen_inputs=not changed)
        if self.args.signoff:
            gates['drc_not_worse'] = result['drc_not_worse']['status'] == 'passed'
            gates['deck_antenna'] = result['drc_after']['statuses'].get('antenna') == 'passed'
        if self.args.antenna_controls:
            gates['antenna_controls'] = result['antenna_controls']['status'] == 'passed'
        result['gates'] = gates
        result['status'] = 'passed' if all(gates.values()) else 'failed'
        result['wall_seconds'] = round(time.monotonic() - start, 3)
        return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--run', type=Path, required=True, help='new evidence directory')
    ap.add_argument('--base-rev', required=True)
    ap.add_argument('--pdk', choices=('sky130', 'gf180', 'all'), default='all')
    ap.add_argument('--profiles', type=Path, default=ROOT / 'fixtures/pdk/local-routing.json')
    ap.add_argument('--rustroad', type=Path, default=ROOT / 'target/release/rustroad')
    ap.add_argument('--sta-bin', type=Path, default=STA_BIN)
    ap.add_argument('--cell-policy', type=Path, default=ROOT / 'fixtures/flow/cell-policy.json')
    ap.add_argument('--manifest', type=Path, default=ROOT / 'fixtures/flow/signoff-pinned.json')
    ap.add_argument('--physical-config', action='append', default=[], metavar='PDK=JSON')
    ap.add_argument('--max-iterations', type=int, default=4, choices=range(1, 5))
    ap.add_argument('--target-margin', type=float, default=0.05)
    ap.add_argument('--setup-margin', type=float, default=0.05)
    ap.add_argument('--signoff', action='store_true', help='native GDS + foundry DRC/antenna/LVS before and after')
    ap.add_argument('--openrcx', action='store_true', help='report OpenSTA on OpenRCX SPEF of the merged DEF')
    ap.add_argument('--antenna-controls', action='store_true', help='planted long-wire antenna controls, native + foundry deck')
    args = ap.parse_args()
    args.physical_config = {k: str(Path(v).resolve()) for k, v in (x.split('=', 1) for x in args.physical_config)}
    args.manifest, args.cell_policy = args.manifest.resolve(), args.cell_policy.resolve()
    require(args.base_rev == 'empty', 'REVISION_CONFLICT: a fresh evidence directory requires --base-rev empty')
    root = args.run.resolve()
    root.mkdir(parents=True, exist_ok=False)
    lock = (root / 'workspace.lock').open('wb')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    (root / 'HEAD').write_text('rev0\n')
    resource.setrlimit(resource.RLIMIT_AS, (8192 * 1024 ** 2, 8192 * 1024 ** 2))
    os.environ.update(GOMAXPROCS='2', QT_QPA_PLATFORM='offscreen', OMP_NUM_THREADS='2')
    h = PhysicalHarness(root, argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=60, tolerance_ns=PARITY_NS))
    start = time.monotonic()
    old = json.loads((ROOT / 'docs/evidence/tap-results.json').read_text())
    policy = json.loads(bounded_text(args.cell_policy, 1024 ** 2))
    tools = {n: Path(old['tools'][n]['path']) for n in ('yosys', 'verilator', 'opensta')}
    tools.update(rustsys=Path(policy['producer']['binary']['path']), rustroad=args.rustroad.resolve(),
                 rusteda=args.sta_bin / 'rusteda', ruststa=args.sta_bin / 'ruststa')
    pinned = {n: old['tools'][n]['sha256'] for n in ('yosys', 'verilator', 'opensta')}
    pinned.update(rustsys=policy['producer']['binary']['sha256'], **STA_SHA)
    summary = dict(schema='rustroad.hold-closure.v1', status='unverified', tools={}, pdks=[],
                   parameters=dict(target_margin_ns=args.target_margin, setup_margin_ns=args.setup_margin,
                                   max_iterations=args.max_iterations, decision_spef='native rustroad extract',
                                   corners={k: [dict(corner=c, liberty=l, rc=rc) for c, l, rc in v] for k, v in CORNERS.items()},
                                   parity_ns=PARITY_NS),
                   scope=dict(level='block/macro (no pad ring, no seal ring)', ir_drop='unchecked', electromigration='unchecked',
                              signal_integrity='unchecked', ocv='unchecked', cppr='unchecked'))
    try:
        r = run_ok(h, 'memory_preflight', ['free', '-h'])
        summary['memory_preflight'] = bounded_text(Path(r['stdout']))
        for n, path in tools.items():
            require(path.is_file(), f'missing tool {n}')
            if n in pinned:
                require(sha256(path) == pinned[n], f'pinned tool changed: {n}')
            summary['tools'][n] = fingerprint(path)
        profiles = json.loads(bounded_text(args.profiles, 1024 ** 2))
        for p in profiles['pdks']:
            if args.pdk not in ('all', p['tag']):
                continue
            p = dict(p)
            for key in ('config', 'routing_config', 'cts_sdc'):
                p[key] = str((ROOT / p[key]).resolve())
            for key in ('technology_lef', 'cell_lef', 'liberty', 'extraction_rules'):
                require(sha256(Path(p[key]['path'])) == p[key]['sha256'], f'PDK identity mismatch {key}')
            try:
                summary['pdks'].append(Flow(h, args, tools, p, policy).run())
            except (RuntimeError, ValueError, KeyError, OSError) as error:
                summary['pdks'].append(dict(pdk=p['tag'], status='failed', reason=str(error)))
        expected = 2 if args.pdk == 'all' else 1
        ok = len(summary['pdks']) == expected and all(p['status'] == 'passed' for p in summary['pdks'])
        summary['status'] = 'passed' if ok else 'failed'
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary['reason'] = str(error)
        summary['status'] = 'failed'
    finally:
        summary['identity_stable'] = all(sha256(Path(t['path'])) == t['sha256'] for t in summary['tools'].values()) and bool(summary['tools'])
        if not summary['identity_stable']:
            summary['status'] = 'failed'
        summary['receipts'] = len(h.receipts)
        summary['wall_seconds'] = round(time.monotonic() - start, 3)
        summary['peak_rss_kib'] = max((r['peak_rss_kib'] for r in h.receipts if r['peak_rss_kib'] is not None), default=None)
        write_json(root / 'summary.json', summary)
        write_json(root / 'receipts.json', h.receipts)
        (root / 'HEAD').write_text('rev1\n')
    print(json.dumps(dict(status=summary['status'], summary=str(root / 'summary.json'),
                          pdks={p['pdk']: dict(status=p['status'], closed=p.get('closed'), iterations=len(p.get('iterations', [])),
                                               reason=p.get('reason')) for p in summary['pdks']})))
    return {'passed': 0, 'failed': 1}.get(summary['status'], 3)


if __name__ == '__main__':
    sys.exit(main())
