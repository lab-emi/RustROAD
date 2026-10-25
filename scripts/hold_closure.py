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
`Design` (top, RTL files, tracked floorplan, rendered SDC, physical recipe,
clocked or combinational, optional literal mapped netlist) replaces the
pipeline.v defaults, and `Flow.close()` runs the bounded loop alone. The
`legacy` recipe is this script's fixed-order placer + lattice router (clock
route then all nets); the `track` recipe is the analytic placer + track router
of scripts/scale_flow.py (route --scope all) with its larger workspace limits.
Without a `Design` every command and verdict of this script is unchanged.
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
from scale_flow import deconflict, buffer_port_aliases, port_buffer_cell, port_tie_cells, SITES  # noqa: E402

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


# Workspace limits (rustroad global options) per physical recipe. The legacy
# values are this script's original ones (pipeline.v); the track values are
# the scale flow's (scripts/scale_flow.py), sized for blocks up to SERV.
ROAD_LIMITS = {
    'legacy': ['--max-cells', '2000', '--max-rows', '128'],
    'track': ['--max-cells', '250000', '--max-rows', '100000', '--max-expansions', '100000000'],
}
# Native extraction work budget per recipe (comparisons; memory is bounded by
# the shape/node caps, not by this count). The default 2e8 is the legacy
# value; regfile-size track layouts need ~2e8 and SERV more than 1.6e9, so
# the track recipe allows 1.28e10 (the extractor's hard ceiling, 64x).
EXTRACT_LIMITS = {'legacy': [], 'track': ['--max-extract-work', '12800000000', '--max-coupling-caps', '8000000',
                                          '--max-rc-nodes', '4000000', '--max-spef-mib', '256',
                                          # SERV (scale-stream layout): 8,054 candidates on one wire run
                                          # (44 B scratch each); RC elements 4.3M of the 8M ceiling.
                                          '--max-extract-candidates', '65536', '--max-rc-elements', '8000000']}
# check-antenna rows kept in a report (all rows of every required design;
# a SERV-size report of every row exceeds the 64 MiB process output cap).
ANTENNA_REPORT_ROWS = '16384'


def spef_file_limit(argv: list[str]) -> int:
    """Process file-size limit of an extract step: its --max-spef-mib cap
    (default 32 MiB) plus 1 MiB, never below the harness log cap."""
    mib = int(argv[argv.index('--max-spef-mib') + 1]) if '--max-spef-mib' in argv else 32
    return max(64 * 1024 ** 2, (mib + 1) * 1024 ** 2)


# RustSTA SPEF import limits per recipe (legacy: this script's original
# values; track: SERV-size blocks, below the engine's own hard limits).
STA_RC_LIMITS = {'legacy': ['--max-rc-nodes', '200000', '--max-rc-elements', '200000'],
                 'track': ['--max-rc-nodes', '2000000', '--max-rc-elements', '4000000',
                           # SERV GF180: 94 MB SPEF per corner, names above 16 MiB
                           # (import 4.9 s, 533 MiB; the native cap is 256 MiB).
                           '--max-spef-mib', '256', '--max-rc-name-mib', '64']}
# RustSTA `run --max-state-gib` per recipe (it also bounds the RC numeric
# work: SERV GF180 needs 3.2e10 > 8.6e9 of 1 GiB; measured 13 s, 408 MiB).
STA_STATE_GIB = {'legacy': '1', 'track': '4'}
# File-size limit of RustSTA import/run (they write the workspace tables:
# SERV GF180 parasitics.rbin is 283 MB for three 94 MB SPEFs) and of the
# OpenRCX container (its SPEF); None keeps the harness's 64 MiB log cap.
STA_FILE_LIMIT = {'legacy': None, 'track': 1024 * 1024 ** 2}
# Gate-sizing ECO bounds (size_eco): resizes per iteration, worst setup
# endpoints traced per iteration, and the utilization every ECO (buffers and
# resizes) may reach in the fixed core (standard cells / row area; taps,
# fillers and CTS buffers excluded). Measured on GF180 regfile at 12 ns: a
# netlist at 0.76 no longer passed CTS packing ("baseline packing ran out of
# intervals"), and with 0.70 for resizes only, buffers alone reached 0.76
# while the sizing that fixes small-fanout nets (alu, 1-2 sites each) was
# refused. One cap for both, below the packing failure. Lowered from 0.74:
# GF180 alu netlists at 0.7255 and 0.7304 (many buf_8/buf_12, about one tap
# interval each) failed placement packing the same way (DRV co-sizing dev
# runs D1/D2, 2026-10-25).
SIZE_MAX_RESIZES = 256
SIZE_MAX_SETUP_ENDPOINTS = 48
SIZE_MAX_UTILIZATION = 0.70
# Closure loop (track recipe): a setup ECO that improved the worst setup
# slack by less than this over the previous layout counts as stagnant; the
# loop then moves on to hold repair instead of spending the remaining
# layouts on setup it cannot close (setup stays failed and reported).
SETUP_STAGNATION_NS = 0.05
# Hold ECO on the track recipe: path-aware placement (pins reports at these
# corners bound the setup room of each point) and partial plans.
PATH_AWARE_SETUP_CORNERS = ('ss', 'tt')
# A net-level hold point (one chain in front of every sink of a net) only on
# nets this small: delay cells are weak drivers.
HOLD_MAX_GROUP_SINKS = 4
# The track loop's hold ECO plans this much above its target margin.
HOLD_PLAN_EXTRA_NS = 0.03
# Gate sizing with RustSTA `whatif` as the oracle (exact on the layout's
# SPEF: a resize keeps every net): candidates per round, rounds, resizes
# accepted per round, and the wall-clock budget of one whatif call.
WHATIF_MAX_CANDIDATES = 48
WHATIF_ROUNDS = 6
WHATIF_ACCEPT_PER_ROUND = 12
WHATIF_TIMEOUT_S = 600
# Candidate budgets, as counts so the determinism rebuild evaluates exactly
# the same candidates (a whatif call that fails or hits WHATIF_TIMEOUT_S is a
# failed round, recorded): per layout and per closure loop (the gate runs
# the loop twice per PDK within its 3600 s wall budget). Both scale with 200
# / instances of the netlist, clamped to 0.5x..4x. Raised from 96/320 when
# violating drivers also try one step down (three candidates each): GF180
# alu/mult/regfile ran out of candidates with 20-40 violations left. Raised
# again from 128/640 (DRV co-sizing, 2026-10-25): with RustSTA 0.4.3 a
# candidate costs 0.19 s (alu), 0.40 s (regfile), 0.53 s (mult) on this host
# (was 1.1-1.9 s), and the 640 loop budget (mult 320) was spent by layout 5-6
# of 8, leaving the last layouts' marginal DRV nets without sizing.
WHATIF_LAYOUT_CANDIDATES = 192
WHATIF_LOOP_CANDIDATES = 1536
WHATIF_HOLD_LOSS_NS = 0.5
# A resize may not make the worst DRV pin worse by more than this fraction of
# its limit (severity guard; the count alone accepted alu g121 _2 -> _1 at an
# equal count while its own transition went from 1.84 to 2.62 ns).
WHATIF_DWS_TOLERANCE = 1e-3
# Chain co-sizing candidates (a violating driver and its upstream drivers one
# step up together) per whatif round, worst violators first.
WHATIF_CHAIN_CANDIDATES = 6
# DRV/setup ECOs stop (the loop goes on to hold) after this many consecutive
# layouts on which the previous ECO improved neither the DRV count nor the
# worst setup slack, nor repaired any violating net of the layout before.
SETUP_STAGNATION_ROUNDS = 2


class Design:
    """What `Flow` builds. The default is this script's pipeline fixture.

    top: top module; rtl: RTL source files; floorplan: tracked floorplan JSON
    copied byte-for-byte (None: the generated pipeline floorplan); sdc_text:
    the complete post-route SDC (None: the CTS SDC + POSTROUTE_SDC_TAIL);
    control_sink: (instance, pin) the native ECO polarity controls buffer;
    recipe: 'legacy' or 'track' (ROAD_LIMITS, physical()); clocked: False for
    a combinational design (virtual I/O clock: no CTS, ideal clock model);
    netlist: a literal mapped netlist replacing synthesis; physical_profile and
    floorplan_recipe: the track recipe's `rustroad floorplan` inputs;
    sizing_profile: enables the gate-sizing ECO for setup and DRV repair.
    """

    def __init__(self, top='lane_smoke', rtl=None, floorplan=None, sdc_text=None, control_sink=('r0', 'D'),
                 recipe='legacy', clocked=True, netlist=None, physical_profile=None, floorplan_recipe=None,
                 sizing_profile=None):
        if recipe not in ROAD_LIMITS:
            raise ValueError(f'unknown physical recipe {recipe!r}')
        self.top = top
        self.rtl = [Path(x) for x in (rtl or [ROOT / 'fixtures/flow/pipeline.v'])]
        self.floorplan = Path(floorplan) if floorplan else None
        self.sdc_text = sdc_text
        self.control_sink = tuple(control_sink)
        self.recipe = recipe
        self.clocked = bool(clocked)
        self.netlist = Path(netlist) if netlist else None
        self.physical_profile = Path(physical_profile) if physical_profile else None
        self.floorplan_recipe = floorplan_recipe
        # Gate-sizing ECO (size_eco) in the closure loop; None: the loop is
        # exactly the hold/antenna loop of this script.
        self.sizing_profile = Path(sizing_profile) if sizing_profile else None


INSTANCE_LINE = re.compile(r'^\s*(\S+)\s+(\S+)\s+\((.*)\);\s*$')
CONNECTION = re.compile(r'\.(\w+)\(([^()]*)\)')


def netlist_instances(text: str) -> dict:
    """instance -> (master, {pin: net}) of a flat mapped netlist (bounded by its text)."""
    out = {}
    for line in text.splitlines():
        m = INSTANCE_LINE.match(line)
        if m and m[1] not in ('module', 'input', 'output', 'wire', 'assign', 'inout'):
            out[m[2].lstrip('\\')] = (m[1], {p: n.strip() for p, n in CONNECTION.findall(m[3])})
    return out


def constant_pins(netlist: Path, config: Path) -> set[str]:
    """``instance/pin`` and output port bits connected to a constant: a
    tie-cell output net (physical configuration tie_high/tie_low; a rail-input
    tie only when its rail pin is a literal, so an ordinary inverter of the
    same master never counts; constant_cells outputs), a literal 1'b0/1'b1,
    or an assign alias of either. Independent of the timing engine (used to
    accept its no-arrival endpoints only when they are constant-driven)."""
    import scale_flow  # noqa: E402 - port declarations
    text = bounded_text(Path(netlist), 64 * 1024 ** 2)
    cfg = json.loads(Path(config).read_text())
    ties = {}
    for k in ('tie_high', 'tie_low'):
        if cfg.get(k):
            ties[(cfg[k]['cell'], cfg[k]['pin'])] = (cfg[k].get('rail_input') or {}).get('pin')
    for c in cfg.get('constant_cells') or []:
        for pin in c.get('high', []) + c.get('low', []):
            ties[(c['cell'], pin)] = None
    literal = {"1'b0", "1'b1"}

    def is_tie(master: str, pin: str, conns: dict) -> bool:
        if (master, pin) not in ties:
            return False
        rail = ties[(master, pin)]
        return rail is None or conns.get(rail, '').strip() in literal
    insts = netlist_instances(text)
    constant = {n.strip().lstrip('\\') for name, (master, conns) in insts.items() for p, n in conns.items()
                if is_tie(master, p, conns)}
    constant |= literal
    aliases = re.findall(r'(?m)^\s*assign\s+(\S+)\s*=\s*(\S+?)\s*;', text)
    for _ in range(len(aliases) + 1):
        grown = {a.lstrip('\\') for a, b in aliases if b.lstrip('\\') in constant and a.lstrip('\\') not in constant}
        if not grown:
            break
        constant |= grown
    pins = {f'{name}/{p}' for name, (master, conns) in insts.items() for p, n in conns.items()
            if n.strip().lstrip('\\') in constant and not is_tie(master, p, conns)}
    return pins | (scale_flow._port_bits(text, 'output') & constant)


def lef_widths(path: Path, dbu: int) -> dict[str, int]:
    """MACRO -> SIZE width in DBU (rounded; used only for a utilization bound)."""
    text = bounded_text(Path(path), 64 * 1024 ** 2)
    return {m[1]: round(float(m[2]) * dbu) for m in
            re.finditer(r'(?ms)^MACRO\s+(\S+)\s*$.*?^\s*SIZE\s+([0-9.]+)\s+BY', text)}


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
        self.sink_pin = (json.loads(self.config.read_text()).get('clock') or {}).get('sink_pin')
        if self.design.clocked and not self.sink_pin:
            raise ValueError(f'{self.tag}: a clocked design needs clock.sink_pin in {self.config}')
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
        if self.design.sizing_profile is not None:
            files['sizing_profile'] = self.design.sizing_profile
        return {k: fingerprint(v) for k, v in sorted(files.items())}

    # ------------------------------------------------------------ physical
    def synth(self) -> Path:
        """RustSys with the cell policy (or the design's literal mapped netlist).

        Escaped scalar wires that collide with a vector port bit (RustSys
        register outputs ``\\p_q[0]``) are renamed (scale_flow.deconflict);
        output ports that share a net are buffered (scale_flow.buffer_port_aliases);
        the raw producer output is kept as ``<tag>_rustsys.v`` when changed.
        Every CEC of the gate checks the renamed netlist against the RTL.
        """
        mapped = self.h.root / f'{self.tag}_mapped.v'
        if self.design.netlist is not None:
            mapped.write_bytes(self.design.netlist.read_bytes())
        else:
            args = self.policy['pdks'][self.tag]['rustsys_args']
            run_ok(self.h, f'{self.tag}_synth', [self.tools['rustsys'], '--threads', '2', '--progress', 'off', 'synth', '--backend', 'cpu',
                                                '--top', self.top, '--lib', self.libs['tt'], *args, '--out', mapped, '--print', 'json',
                                                *self.design.rtl])
        text = mapped.read_text()
        renamed_text, self.renamed_wires = deconflict(text)
        # Output ports sharing one net get identity buffers (LVS: one pin per
        # port in the layout); a no-op for netlists without such aliases.
        renamed_text, self.port_buffers = buffer_port_aliases(renamed_text, port_buffer_cell(self.tag),
                                                               port_tie_cells(self.tag))
        if self.renamed_wires or self.port_buffers:
            mapped.rename(self.h.root / f'{self.tag}_rustsys.v')
            mapped.write_text(renamed_text)
        return mapped

    def reproduce_floorplan(self, mapped: Path) -> dict:
        """Track recipe: `rustroad floorplan` from the recipe must regenerate the
        tracked floorplan and physical configuration byte for byte."""
        fp = self.design.floorplan_recipe
        floor, phys = self.h.root / f'{self.tag}_floorplan_check.json', self.h.root / f'{self.tag}_physical_check.json'
        clock = ['--clock-port', self.p['clock_port']] if self.design.clocked else ['--no-clock']
        rep = self.h.json_command(f'{self.tag}_floorplan', [str(x) for x in [
            self.tools['rustroad'], 'floorplan', '--tech-lef', self.p['technology_lef']['path'], '--cell-lef', self.p['cell_lef']['path'],
            '--netlist', mapped, '--physical-config', self.design.physical_profile, '--routing-config', self.routing,
            '--site', SITES[self.tag], '--utilization', fp['utilization'], '--aspect', fp['aspect'],
            '--margin-um', fp['margin_um'], *clock, '--out-floorplan', floor, '--out-physical-config', phys]])
        same_floor = floor.read_bytes() == self.design.floorplan.read_bytes()
        same_phys = phys.read_bytes() == self.config.read_bytes()
        return dict(status='passed' if same_floor and same_phys else 'failed', floorplan_identical=same_floor,
                    physical_config_identical=same_phys, recipe=fp,
                    report={k: rep.get(k) for k in ('die_area', 'core_area', 'rows', 'estimated_utilization', 'io_pins',
                                                    'clock_buffers_estimate', 'cell_area_um2')})

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

    def physical(self, it: int, netlist: Path, weights: Path | None = None, seed: Path | None = None) -> dict:
        """One complete layout. `weights`: timing-driven net weights for the
        analytic placer (`place --net-weights`); `seed`: ECO placement seed
        (`place --seed`, the previous layout); both are kept for the CTS
        re-placement."""
        t, h = f'{self.tag}_i{it}', self.h
        ws = h.root / f'{t}_physical'
        road = [self.tools['rustroad'], '--workspace', ws, '--threads', '2', *ROAD_LIMITS[self.design.recipe]]
        rec = dict(recipe=self.design.recipe)
        a = h.json_command(f'{t}_import', list(map(str, road + ['import-pdk', '--base-rev', 'rev0', '--tech-lef', self.p['technology_lef']['path'],
                                                                 '--cell-lef', self.p['cell_lef']['path'], '--netlist', netlist,
                                                                 '--floorplan', self.floorplan(), '--physical-config', self.config])))
        place = ['place', '--base-rev', a['revision']] + (['--net-weights', weights] if weights else []) + \
            (['--seed', seed] if seed else [])
        a = h.json_command(f'{t}_place', list(map(str, road + place)))
        rec['placement'] = a.get('placement')
        if weights:
            rec['net_weights'] = fingerprint(weights)
        if seed:
            rec['placement_seed'] = fingerprint(seed)
        if self.design.clocked:
            a = h.json_command(f'{t}_cts', list(map(str, road + ['clock-tree', '--base-rev', a['revision'], '--sdc', self.p['cts_sdc']])))
            rec['cts'] = a
        else:
            rec['cts'] = dict(status='not_applicable', reason='combinational design: no clock port, no clock tree')
        if self.design.recipe == 'legacy':
            a = h.json_command(f'{t}_clock_route', list(map(str, road + ['route', '--base-rev', a['revision'], '--scope', 'clock', '--config', self.routing])))
        a = h.json_command(f'{t}_all_route', list(map(str, road + ['route', '--base-rev', a['revision'], '--scope', 'all', '--config', self.routing])))
        rec['route'] = {k: a.get(k) for k in ('routed_nets', 'routing_segments', 'routing_expansions', 'cells', 'tap_cells',
                                              'filler_cells', 'route_memory') if k in a}
        rec['routed_revision'] = a['revision']
        # Diode-free antenna repair on the routed revision: the native check
        # inside repair-antenna finds the violating (net, layer) rows and the
        # router cuts the run nearest the gates and bridges it one layer up
        # (new revision only when a jumper was inserted; netlist, placement
        # and every other net unchanged). Clean layouts are left untouched.
        j = h.json_command(f'{t}_repair_antenna', list(map(str, road + ['repair-antenna', '--base-rev', a['revision']])))
        rec['antenna_repair'] = {k: j[k] for k in ('status', 'revision', 'before', 'after', 'jumpers', 'unrepaired', 'stats')}
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
            argv = list(map(str, road + ['extract', '--rc-profile', self.rc_profiles[c], '--corner', self.rc[c],
                                         '--out', spef, *EXTRACT_LIMITS[self.design.recipe]]))
            # The SPEF is capped by the extractor's own --max-spef-mib; the
            # process file-size limit must not be tighter than that cap.
            r = h.run(f'{t}_extract_{self.rc[c]}', argv, file_limit_bytes=spef_file_limit(argv))
            require(r['exit_code'] == 0, f'{t}_extract_{self.rc[c]}: process exited {r["exit_code"]}; see receipt')
            x = json.loads(bounded_text(Path(r['stdout'])))
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
        argv += STA_RC_LIMITS[self.design.recipe]
        a = self.sta_json(f'{t}_import', argv)
        r = self.sta_json(f'{t}_run', [ruststa, *common, 'run', '--base-rev', a['new_rev'], '--backend', 'cpu', '--precision', 'f64',
                                        '--max-paths', '16', '--max-state-gib', STA_STATE_GIB[self.design.recipe],
                                        '--require-wire-model', 'spef_rc'])
        require(r['scope'].get('clock_model') == self.clock_model() and r['scope'].get('wire_model') == 'spef_rc'
                and (self.design.clocked or r['scope'].get('physical_clock_domains') == 0), f'{t}: wrong timing scope')
        rev = r['new_rev']
        constant_endpoints = []
        if r['completeness']['unconstrained_endpoints']:
            rows = json.loads(bounded_text(Path(run_ok(h, f'{t}_unconstrained', [ruststa, *common[:-2], 'report', '--rev', rev,
                                                                                 '--section', 'unconstrained', '--limit', '0'])['stdout'])))
            rows = rows['sections']['unconstrained']['rows']
            constant = constant_pins(timed, self.config)
            bad = [x for x in rows if x['reason'] != 'no_arrival' or x['pin'] not in constant]
            require(len(rows) == r['completeness']['unconstrained_endpoints'] and not bad,
                    f'{t}: unconstrained endpoints that are not constant-driven: {bad[:5]}')
            constant_endpoints = sorted(x['pin'] for x in rows)
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
            rows = [x for x in json.loads(bounded_text(ep))['sections']['endpoints']['rows']
                    if x['endpoint'] not in constant_endpoints]
            if self.design.clocked:
                pins = json.loads(bounded_text(Path(run_ok(h, f'{t}_clock_pins_{c}', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'pins',
                                                                                      '--corner', c, '--filter', f'pin~/{self.sink_pin}', '--limit', '0'])['stdout'])))
                sinks = [x for x in pins['sections']['pins']['rows'] if x['pin'].endswith('/' + self.sink_pin)]
                require(sinks and all(x['arrival_late_ns'] is not None for x in sinks), f'{t}: clock sink arrivals missing at {c}')
                clock = dict(sinks=len(sinks), latency_max_ns=max(x['arrival_late_ns'] for x in sinks),
                             latency_min_ns=min(x['arrival_early_ns'] for x in sinks), slew_max_ns=max(x['slew_late_ns'] for x in sinks))
            else:
                clock = dict(sinks=0, model='virtual I/O clock (combinational design)')
            corners[c] = dict(clock=clock, worst_setup_ns=min(x['setup_slack_ns'] for x in rows), worst_hold_ns=min(x['hold_slack_ns'] for x in rows),
                              setup_violations=sum(x['setup_slack_ns'] < 0 for x in rows),
                              hold_below_margin=sum(x['hold_slack_ns'] < self.args.target_margin for x in rows),
                              endpoints={x['endpoint']: dict(setup=x['setup_slack_ns'], hold=x['hold_slack_ns']) for x in rows})
        clocks = run_ok(h, f'{t}_clocks', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'clocks', '--limit', '0'])
        closed = all(v['worst_hold_ns'] >= self.args.target_margin and v['worst_setup_ns'] >= 0 for v in corners.values())
        drv = None
        if self.design.sizing_profile is not None:
            path = h.root / f'{t}_drv.json'
            rr = run_ok(h, f'{t}_drv', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'drv', '--limit', '0'])
            path.write_bytes(Path(rr['stdout']).read_bytes())
            files['drv'] = path
            rows = json.loads(bounded_text(path, 64 * 1024 ** 2))['sections']['drv']['rows']
            bad = [x for x in rows if x['slack'] < 0]
            drv = dict(violations=len(bad), by_kind={k: sum(1 for x in bad if x['kind'] == k) for k in sorted({x['kind'] for x in bad})},
                       status=r['metrics'].get('drv_status'))
            # Per-pin setup slack (path-aware hold ECO, buffering, net weights).
            for c in PATH_AWARE_SETUP_CORNERS:
                pins = h.root / f'{t}_pins_{c}.json'
                rr = run_ok(h, f'{t}_pins_{c}', [ruststa, *common[:-2], 'report', '--rev', rev, '--section', 'pins',
                                                 '--corner', c, '--limit', '0'])
                pins.write_bytes(Path(rr['stdout']).read_bytes())
                files[f'pins_{c}'] = pins
        return dict(workspace=str(ws), revision=rev, timed_netlist=str(timed), timed_netlist_sha256=timed_sha,
                    scope=r['scope'], metrics=r['metrics'], completeness=r['completeness'],
                    diagnostics=[d for d in r.get('diagnostics', []) if d.get('severity') != 'note'][:20],
                    corners=corners, closed=closed, files={k: str(v) for k, v in files.items()},
                    clocks_report=str(clocks['stdout']), constant_endpoints=constant_endpoints, drv=drv)

    def sta_rc_limits(self) -> list[str]:
        return list(STA_RC_LIMITS[self.design.recipe])

    def sta_state_gib(self) -> str:
        return STA_STATE_GIB[self.design.recipe]

    def sta_file_limit(self) -> int | None:
        return STA_FILE_LIMIT[self.design.recipe]

    def sta_json(self, label: str, argv: list) -> dict:
        """A rusteda.v1 JSON command under the recipe's RustSTA file limit."""
        limit = self.sta_file_limit()
        if limit is None:
            return self.h.json_command(label, argv, True)
        r = self.h.run(label, [str(a) for a in argv], file_limit_bytes=limit)
        require(r['exit_code'] == 0, f'{label}: process exited {r["exit_code"]}; see receipt')
        value = json.loads(bounded_text(Path(r['stdout'])))
        require(value.get('schema') == 'rusteda.v1' and value.get('ok') is True and value.get('error') is None,
                f'{label}: unsuccessful rusteda.v1 envelope')
        return value

    def clock_model(self) -> str:
        """RustSTA scope the gate requires: propagated clocks through the
        RustROAD clock tree, or the ideal model of a virtual I/O clock."""
        return 'propagated' if self.design.clocked else 'ideal'

    def hold_eco(self, it: int, netlist: Path, sta: dict, target: float | None = None) -> tuple[Path, dict]:
        t = f'{self.tag}_i{it}'
        out, plan = self.h.root / f'{self.tag}_i{it + 1}_mapped.v', self.h.root / f'{t}_hold_plan.json'
        hold_report = sta['files']['hold_fix']
        if target is None:
            target = self.args.target_margin
        else:
            # A raised target (reference-SPEF guard band): the hold_fix report
            # of the same native STA revision, at that target.
            hold_report = self.h.root / f'{t}_hold_fix_{target:.4f}.json'
            rr = run_ok(self.h, f'{t}_hold_fix_raised', [self.tools['ruststa'], '--workspace', sta['workspace'], '--threads', '2',
                                                        '--format', 'json', 'report', '--rev', sta['revision'], '--section', 'hold_fix',
                                                        '--target-margin', str(target), '--limit', '0'])
            hold_report.write_bytes(Path(rr['stdout']).read_bytes())
        argv = [self.tools['rustroad'], 'hold-eco', '--netlist', netlist, '--timed-netlist', sta['timed_netlist'],
                '--timed-netlist-sha256', sta['timed_netlist_sha256'], '--hold-report', hold_report,
                '--delay-cells', self.delay_cells, '--setup-corner', 'ss', '--target-margin-ns', target,
                '--setup-margin-ns', self.args.setup_margin, '--out', out, '--plan-out', plan]
        for c in self.libs:
            argv += ['--hold-corner', c, '--setup-report', sta['files'][f'endpoints_{c}']]
        if self.design.sizing_profile is not None:
            # Track recipe: path-aware placement bounded by per-pin setup slack
            # at ss and tt, and partial plans (infeasible endpoints listed).
            for c in PATH_AWARE_SETUP_CORNERS:
                argv += ['--pins-report', sta['files'][f'pins_{c}']]
            argv += ['--allow-partial', '--max-group-sinks', HOLD_MAX_GROUP_SINKS]
        r = self.h.run(f'{t}_hold_eco', [str(x) for x in argv])
        result = json.loads(bounded_text(plan)) if plan.exists() else None
        require(r['exit_code'] == 0, f'{t}: hold-eco failed: ' + bounded_text(Path(r['stderr']))[-600:])
        return out, result

    # ------------------------------------------------------------ gate sizing
    def size_plan(self, it: int, netlist: Path, sta: dict, phys: dict, skip: set, extra_width: int = 0) -> tuple[list, dict]:
        """Plan one gate-sizing step (resize_cell, same Liberty family).

        Candidates: the driver of every net with a max_transition /
        max_capacitance violation (RustSTA drv report, any corner; one to three
        sizes up by the violation ratio), and every cell on the worst setup
        path of up to SIZE_MAX_SETUP_ENDPOINTS endpoints with setup slack < 0
        at tt/ss (one size up). Only instances of the mapped netlist are
        resized (not CTS, hold, tie or diode cells); the new width must fit the
        tap clear gap and the resized cells must keep the core utilization at
        or below SIZE_MAX_UTILIZATION. Bounded by SIZE_MAX_RESIZES. `skip`:
        instances the buffering planner already handles (drivers of buffered
        nets, its own resizes); `extra_width`: DBU of row width the planned
        buffers add. Returns (operations, summary).
        """
        import rtl2gds_logic as L  # noqa: E402 - gate helper (LEF pin reader)
        t, h = f'{self.tag}_i{it}', self.h
        prof = json.loads(self.design.sizing_profile.read_text())
        family, widths = {}, {}
        for f in prof['families']:
            for k, c in enumerate(f['cells']):
                family[c['name']] = (f['id'], k, [x['name'] for x in f['cells']])
                widths[c['name']] = c['width_dbu']
        cfg = json.loads(self.config.read_text())
        floor = json.loads(self.design.floorplan.read_text() if self.design.floorplan else self.floorplan().read_text())
        site_w = floor['site']['width']
        spacing = int(cfg.get('cell_spacing_sites') or 0) * site_w
        max_width = (cfg['tap']['max_gap'] // site_w) * site_w - spacing if cfg.get('tap') else None
        core = floor['core_area']
        rows_area = sum((r['x_end'] - r['x_start']) for r in floor['rows']) * floor['site']['height']
        lef = L.lef_pins(Path(self.p['cell_lef']['path']))
        mapped = netlist_instances(bounded_text(netlist, 64 * 1024 ** 2))
        timed = netlist_instances(bounded_text(Path(sta['timed_netlist']), 64 * 1024 ** 2))
        driver = {}
        for name, (master, conns) in timed.items():
            for pin, net in conns.items():
                if ((lef.get(master) or {}).get(pin) or {}).get('direction', '') and \
                        lef[master][pin]['direction'].startswith('OUTPUT'):
                    driver[net.lstrip('\\')] = name
        steps, reasons, skipped = {}, {}, {}

        def want(inst, n, why):
            if inst not in mapped:
                skipped[inst] = 'not in the mapped netlist (CTS/inserted cell)'
                return
            if inst.startswith(('rr_eco_hold_', 'rr_eco_ant_', 'rr_tie_', 'rr_clk')):
                skipped[inst] = 'ECO, tie or clock cell'
                return
            if inst in skip:
                skipped[inst] = 'handled by the buffering ECO'
                return
            steps[inst] = max(steps.get(inst, 0), n)
            reasons.setdefault(inst, set()).add(why)
        drv_rows = json.loads(bounded_text(Path(sta['files']['drv']), 64 * 1024 ** 2))['sections']['drv']['rows']
        for row in sorted(drv_rows, key=lambda x: x['slack']):
            if row['slack'] >= 0 or row['kind'] not in ('max_transition', 'max_capacitance'):
                continue
            inst = driver.get((row.get('net') or '').lstrip('\\'))
            if inst is None:
                skipped[row.get('net') or row['pin']] = 'driven by a port or unknown driver'
                continue
            ratio = row['value'] / row['limit'] if row['limit'] else 9.0
            want(inst, 1 if ratio < 1.5 else 2 if ratio < 3.0 else 3, row['kind'])
        failing = sorted(((v['setup'], c, e) for c in ('tt', 'ss') for e, v in sta['corners'][c]['endpoints'].items()
                          if v['setup'] is not None and v['setup'] < 0), key=lambda x: x[0])
        seen = set()
        traced = 0
        ruststa = str(self.tools['ruststa'])
        for slack, corner, ep in failing:
            if ep in seen or traced >= SIZE_MAX_SETUP_ENDPOINTS:
                continue
            seen.add(ep)
            traced += 1
            r = run_ok(h, f'{t}_size_path_{traced}', [ruststa, 'path', '-w', sta['workspace'], '--rev', sta['revision'],
                                                     '--to', ep, '--kind', 'setup', '--corner', corner, '--format', 'json', '-q'])
            data = json.loads(bounded_text(Path(r['stdout']), 64 * 1024 ** 2))
            for path in data.get('paths') or ([data['path']] if data.get('path') else []):
                for point in path.get('points') or []:
                    if point.get('arc') == 'cell' and point.get('inst'):
                        want(point['inst'], 1, 'setup')
        # Apply the steps within families, width and utilization bounds.
        height = floor['site']['height']
        sizes = lef_widths(Path(self.p['cell_lef']['path']), floor['units_per_micron'])
        used = (sum(sizes.get(m, 0) for m, _ in mapped.values()) + extra_width) * height
        ops, unsizable = [], {}
        order = sorted(steps, key=lambda i: (-steps[i], i))
        for inst in order:
            if len(ops) >= SIZE_MAX_RESIZES:
                unsizable[inst] = 'resize budget per iteration reached'
                continue
            master = mapped[inst][0]
            if master not in family:
                unsizable[inst] = f'{master} is in no sizing family'
                continue
            _, k, members = family[master]
            target = None
            for j in range(min(len(members) - 1, k + steps[inst]), k, -1):
                if max_width is None or widths[members[j]] <= max_width:
                    target = members[j]
                    break
            if target is None:
                unsizable[inst] = f'{master} is the largest member that fits the tap gap'
                continue
            grow = (widths[target] - widths[master]) * height
            if (used + grow) > SIZE_MAX_UTILIZATION * rows_area:
                unsizable[inst] = 'core utilization bound reached'
                continue
            used += grow
            ops.append(dict(op='resize_cell', instance=inst, cell=target))
        summary = dict(resized=len(ops), candidates=len(steps), traced_setup_endpoints=traced,
                       failing_setup_endpoints=len({e for _, _, e in failing}),
                       drv_violations=sum(1 for x in drv_rows if x['slack'] < 0),
                       by_reason={r: sum(1 for i in reasons if r in reasons[i] and any(o['instance'] == i for o in ops))
                                  for r in ('setup', 'max_transition', 'max_capacitance')},
                       unsizable=dict(sorted(unsizable.items())[:40]), skipped=dict(sorted(skipped.items())[:40]),
                       utilization_after=round(used / rows_area, 4), max_width_dbu=max_width)
        return ops, summary

    def whatif_sizing(self, it: int, netlist: Path, sta: dict, skip: set, extra_width: int = 0) -> tuple[list, dict]:
        """Gate sizing decided by RustSTA `whatif` on this layout (exact: a
        resize keeps every net and its SPEF parasitics).

        Candidates: the drivers of nets with max_transition/max_capacitance
        violations and the cells on the worst setup paths of up to
        SIZE_MAX_SETUP_ENDPOINTS failing endpoints (tt/ss), each with the
        neighbouring members of its sizing family (one down, one and two up,
        within the tap gap). Each round evaluates every candidate on top of
        the resizes accepted so far and accepts up to WHATIF_ACCEPT_PER_ROUND
        improving ones on distinct instances, best first. Improving: fewer
        DRV violations without losing worst tt/ss setup slack (or keeping it
        >= 0), or no more DRV violations and a better worst setup slack, or
        an equal worst and a better total negative slack. The next round's
        base re-checks the accepted set as a whole; a round whose combined
        base is worse than the previous one keeps only its single best edit.
        Bounded by WHATIF_ROUNDS x WHATIF_MAX_CANDIDATES evaluations and
        WHATIF_TIMEOUT_S per call. Returns (resize operations, summary).
        """
        t, h = f'{self.tag}_i{it}', self.h
        prof = json.loads(self.design.sizing_profile.read_text())
        family, widths = {}, {}
        for f in prof['families']:
            for k, c in enumerate(f['cells']):
                family[c['name']] = (k, [x['name'] for x in f['cells']])
                widths[c['name']] = c['width_dbu']
        cfg = json.loads(self.config.read_text())
        floor = json.loads(self.design.floorplan.read_text() if self.design.floorplan else self.floorplan().read_text())
        site_w = floor['site']['width']
        spacing = int(cfg.get('cell_spacing_sites') or 0) * site_w
        max_width = (cfg['tap']['max_gap'] // site_w) * site_w - spacing if cfg.get('tap') else None
        rows_area = sum((r['x_end'] - r['x_start']) for r in floor['rows']) * floor['site']['height']
        sizes = lef_widths(Path(self.p['cell_lef']['path']), floor['units_per_micron'])
        mapped = netlist_instances(bounded_text(netlist, 64 * 1024 ** 2))
        used = (sum(sizes.get(m, 0) for m, _ in mapped.values()) + extra_width) * floor['site']['height']
        import rtl2gds_logic as L  # noqa: E402
        lef = L.lef_pins(Path(self.p['cell_lef']['path']))
        timed = netlist_instances(bounded_text(Path(sta['timed_netlist']), 64 * 1024 ** 2))
        driver, sinks_of = {}, {}
        for name, (master, conns) in timed.items():
            for pin, net in conns.items():
                d = ((lef.get(master) or {}).get(pin) or {}).get('direction') or ''
                if d.startswith('OUTPUT'):
                    driver[net.lstrip('\\')] = name
                elif d == 'INPUT':
                    sinks_of.setdefault(net.lstrip('\\'), []).append(name)
        prio = {}

        def want(inst, value, why):
            if inst in mapped and inst not in skip and mapped[inst][0] in family and \
                    not inst.startswith(('rr_eco_hold_', 'rr_eco_ant_', 'rr_tie_', 'rr_clk')):
                old = prio.get(inst)
                if old is None or value < old[0]:
                    prio[inst] = (value, why)
        alias = {a.lstrip('\\'): b.lstrip('\\') for a, b in
                 re.findall(r'(?m)^\s*assign\s+(\S+)\s*=\s*(\S+?)\s*;', bounded_text(Path(sta['timed_netlist']), 64 * 1024 ** 2))}
        drv_rows = json.loads(bounded_text(Path(sta['files']['drv']), 64 * 1024 ** 2))['sections']['drv']['rows']
        import closure_eco as CE  # noqa: E402
        limits = {k: max((r['limit'] for r in drv_rows if r['kind'] == k), default=1.0)
                  for k in ('max_transition', 'max_capacitance')}
        # Inputs of each timed instance: the upstream drivers whose output
        # slew a violating driver sees (chain co-sizing).
        fanin = {}
        pins_on = {}
        for name, (master, conns) in timed.items():
            for net in conns.values():
                pins_on[net.lstrip('\\')] = pins_on.get(net.lstrip('\\'), 0) + 1
        # Small nets of each instance (clock and other wide nets excluded):
        # two edits of one round on one of them are coupled.
        inst_nets = {name: {n.lstrip('\\') for n in conns.values() if pins_on.get(n.lstrip('\\'), 0) <= 8}
                     for name, (master, conns) in timed.items()}
        for name, (master, conns) in timed.items():
            for pin, net in conns.items():
                if (((lef.get(master) or {}).get(pin) or {}).get('direction') or '') == 'INPUT':
                    up = driver.get(net.lstrip('\\'))
                    # ECO buffers are not grown for their sinks' sake: a larger
                    # buffer loads its own (often weak) driver, and wide cells
                    # pack badly between taps (GF180 alu: a 0.73 netlist with
                    # 32 buf_12 failed placement packing).
                    if up and up != name and not up.startswith('rr_eco_buf_'):
                        fanin.setdefault(name, set()).add(up)
        violators = {}
        for row in drv_rows:
            if row['kind'] in ('max_transition', 'max_capacitance') and row['value'] > CE.DRV_PLAN_MARGIN * row['limit']:
                net = (row.get('net') or '').lstrip('\\')
                net = net if net in driver else alias.get(net, net)
                inst = driver.get(net)
                if inst:
                    want(inst, -10.0 + row['slack'] / max(row['limit'], 1e-9), 'drv')
                    if row['slack'] < 0:
                        violators[inst] = min(violators.get(inst, 0.0), row['slack'] / max(row['limit'], 1e-9))
                # ECO buffers loading the net may shrink (less input capacitance);
                # on a violating net every sink may (GF180 mult: nor2_2 into one
                # nor2_4 input, 17.6 fF).
                for sink in sinks_of.get(net, []):
                    if sink.startswith('rr_eco_buf_') or row['slack'] < 0:
                        want(sink, -9.0 + row['slack'] / max(row['limit'], 1e-9), 'drv_sink')
        # The upstream drivers of violating drivers may grow (a sharper input
        # slew, and room for the violating driver's own upsizing).
        for inst, sev in sorted(violators.items(), key=lambda x: (x[1], x[0])):
            for up in sorted(fanin.get(inst, ())):
                want(up, -8.0 + sev, 'drv_up')
        failing = sorted(((v['setup'], c, e) for c in ('tt', 'ss') for e, v in sta['corners'][c]['endpoints'].items()
                          if v['setup'] is not None and v['setup'] < 0), key=lambda x: x[0])
        seen, traced = set(), 0
        for slack, corner, ep in failing:
            if ep in seen or traced >= SIZE_MAX_SETUP_ENDPOINTS:
                continue
            seen.add(ep)
            traced += 1
            r = run_ok(h, f'{t}_size_path_{traced}', [self.tools['ruststa'], 'path', '-w', sta['workspace'], '--rev', sta['revision'],
                                                     '--to', ep, '--kind', 'setup', '--corner', corner, '--format', 'json', '-q'])
            data = json.loads(bounded_text(Path(r['stdout']), 64 * 1024 ** 2))
            for path in data.get('paths') or ([data['path']] if data.get('path') else []):
                for point in path.get('points') or []:
                    if point.get('arc') == 'cell' and point.get('inst'):
                        want(point['inst'], slack, 'setup')
        order = sorted(prio, key=lambda i: (prio[i][0], i))

        def alternatives(inst):
            master = mapped[inst][0]
            k, members = family[master]
            out = []
            # A (near-)violating driver grows (or steps down once, below), an
            # ECO buffer loading such a net shrinks (one or two steps), an
            # upstream driver of a violating one grows.
            why = prio[inst][1]
            # A violating driver also tries one step down: the largest member is
            # not always the fastest edge at a light load (GF180 aoi22_4 vs _2).
            steps = {'drv': (k + 1, k + 2, k - 1), 'drv_sink': (k - 1, k - 2), 'drv_up': (k + 1,)}
            for j in steps.get(why, (k + 1, k - 1, k + 2)):
                if 0 <= j < len(members) and members[j] != master and \
                        (max_width is None or widths[members[j]] <= max_width):
                    out.append(members[j])
            return out

        def chain_candidate(inst):
            """A violating driver one step up together with each of its
            upstream drivers one step up (one candidate): growing the driver
            alone loads its inputs, which then violate (GF180 alu oai21)."""
            edits = []
            for i in [inst] + sorted(fanin.get(inst, ())):
                if i not in mapped or i in skip or mapped[i][0] not in family or i in accepted or \
                        i.startswith(('rr_eco_hold_', 'rr_eco_ant_', 'rr_tie_', 'rr_clk')):
                    continue
                k, members = family[mapped[i][0]]
                if k + 1 < len(members) and (max_width is None or widths[members[k + 1]] <= max_width):
                    edits.append((i, members[k + 1]))
            return edits if len(edits) >= 2 and edits[0][0] == inst else None

        def metrics(values):
            s = values['summary']
            corners = {x['corner']: x for x in values.get('corners') or []}
            wns = min(corners[c]['setup']['worst_slack_ns'] for c in ('tt', 'ss') if c in corners)
            tns = sum(corners[c]['setup']['tns_ns'] for c in ('tt', 'ss') if c in corners)
            drv = sum(int(s.get(k) or 0) for k in ('max_transition_violating', 'max_capacitance_violating',
                                                    'max_fanout_violating'))
            # Severity: the worst transition / capacitance slack over every pin,
            # as a fraction of its limit (counts alone accepted resizes that
            # made the worst net worse at an equal count: alu g121 _2 -> _1).
            dws = min([float(s[k]) / limits[lk] for k, lk in (('worst_max_transition_slack_ns', 'max_transition'),
                                                             ('worst_max_capacitance_slack_pf', 'max_capacitance'))
                       if s.get(k) is not None] or [0.0])
            return dict(drv=drv, wns=wns, tns=tns, whs=s.get('worst_hold_slack_ns'), dws=dws)

        target = self.args.target_margin

        def better(m, b, why='setup'):
            # Hold below the target is repaired by the hold ECO afterwards;
            # a resize may cost it at most WHATIF_HOLD_LOSS_NS there.
            if m['whs'] is not None and b['whs'] is not None and \
                    m['whs'] < min(b['whs'], target) - WHATIF_HOLD_LOSS_NS:
                return False
            setup_ok = m['wns'] >= min(b['wns'], 0.0) - 1e-9
            if m['drv'] < b['drv'] and setup_ok:
                return True
            # At an equal count a resize may not make the worst DRV pin worse
            # (WHATIF_DWS_TOLERANCE of its limit): the count alone accepted alu
            # g121 _2 -> _1 while its own transition went 1.84 -> 2.62 ns.
            if m['drv'] == b['drv'] and m['dws'] < b['dws'] - WHATIF_DWS_TOLERANCE and m['dws'] < 0:
                return False
            # Same count, a less severe worst violation.
            if m['drv'] <= b['drv'] and m['dws'] > b['dws'] + 1e-4 and b['dws'] < 0 and setup_ok:
                return True
            # Upsizing a near-violating driver (planning margin): kept unless
            # it adds a violation or costs setup below zero.
            # (Upstream growth for a violating driver's sake must improve.)
            if why in ('drv', 'drv_sink') and m['drv'] <= b['drv'] and setup_ok:
                return True
            if m['drv'] <= b['drv'] and m['wns'] > b['wns'] + 1e-4:
                return True
            return m['drv'] <= b['drv'] and m['wns'] >= b['wns'] - 1e-9 and m['tns'] > b['tns'] + 1e-4

        accepted, history, evaluated = {}, [], 0
        prev_base = None
        spent = 0
        scale = min(4.0, max(0.5, 200.0 / max(len(mapped), 1)))
        budget = min(int(WHATIF_LAYOUT_CANDIDATES * scale), int(WHATIF_LOOP_CANDIDATES * scale) - self.whatif_spent)
        bulk = self.whatif_bulk(t, sta, order, prio, mapped, family, widths, max_width, floor, rows_area, used,
                                budget - spent, metrics, better)
        if bulk is not None:
            spent += bulk['evaluated']
            self.whatif_spent += bulk['evaluated']
            for inst, cell in bulk.get('accepted') or []:
                accepted[inst] = cell
            used += bulk.get('grow') or 0
            evaluated += bulk['evaluated']
        for rnd in range(WHATIF_ROUNDS):
            room = budget - spent
            if room < 4:
                break
            cands = []
            cap_n = min(WHATIF_MAX_CANDIDATES, room)
            # Chain co-sizing candidates of the worst violators first (up to
            # WHATIF_CHAIN_CANDIDATES), then single resizes by priority.
            for inst, _ in sorted(violators.items(), key=lambda x: (x[1], x[0])):
                if sum(1 for c in cands if len(c[1]) > 1) >= WHATIF_CHAIN_CANDIDATES or len(cands) >= cap_n:
                    break
                if inst in accepted or inst not in prio:
                    continue
                ch = chain_candidate(inst)
                if ch:
                    cands.append((inst, ch, 'drv_chain'))
            for inst in order:
                if inst in accepted:
                    continue
                for cell in alternatives(inst):
                    if len(cands) < cap_n:
                        cands.append((inst, [(inst, cell)], prio[inst][1]))
            if not cands:
                break
            edits = dict(schema='rusteda.edits.v1',
                         base=[dict(op='resize', instance=i, cell=c) for i, c in sorted(accepted.items())],
                         candidates=[dict(id=f'c{k}', edits=[dict(op='resize', instance=i, cell=c) for i, c in e])
                                     for k, (_, e, _) in enumerate(cands)],
                         report=dict(endpoints=[], paths=[]))
            ef = jdump(h.root / f'{t}_whatif_{rnd}.json', edits)
            r = h.run(f'{t}_whatif_{rnd}', [str(x) for x in [
                self.tools['ruststa'], '-w', sta['workspace'], '--format', 'json', 'whatif', '--rev', sta['revision'],
                '--edits', ef, '--max-candidates', '1024', '--max-edits', '4096', '--timeout', WHATIF_TIMEOUT_S,
                '--threads', '2', '-q']])
            spent += len(cands)
            self.whatif_spent += len(cands)
            try:
                d = json.loads(bounded_text(Path(r['stdout']), 256 * 1024 ** 2))
            except (ValueError, OSError):
                d = {}
            if not d.get('base') or r['exit_code'] != 0:
                history.append(dict(round=rnd, status='failed', exit_code=r['exit_code'],
                                    stderr=bounded_text(Path(r['stderr']))[-400:]))
                break
            base = metrics(d['base'])
            if prev_base is not None and better(prev_base, base) and history and history[-1].get('accepted'):
                # The combined set is worse than before it: keep only the
                # single best edit of the last round.
                last = history[-1]['accepted']
                for i, _ in last[history[-1].get('first_size', 1):]:
                    accepted.pop(i, None)
                history[-1]['reverted'] = [i for i, _ in last[history[-1].get('first_size', 1):]]
                continue
            scored = []
            for c in d.get('candidates') or []:
                if c.get('status') != 'evaluated' or not c.get('values'):
                    continue
                evaluated += 1
                m = metrics(c['values'])
                k = int(c['id'][1:])
                if better(m, base, cands[k][2]):
                    scored.append(((m['drv'] - base['drv'], -(m['dws'] - base['dws']), -(m['wns'] - base['wns']),
                                    -(m['tns'] - base['tns']), k), cands[k], m))
            scored.sort()
            take, insts = [], set()
            nets_taken = set()
            for key_, (inst, cand_edits, why), m in scored:
                if any(i in insts for i, _ in cand_edits) or len(take) >= WHATIF_ACCEPT_PER_ROUND:
                    continue
                # Each candidate was evaluated alone: two on one net (a buffer
                # shrunk while its sink grows, GF180 alu) can violate together.
                touched_nets = set().union(*(inst_nets.get(i, set()) for i, _ in cand_edits))
                if touched_nets & nets_taken:
                    continue
                grow = sum((widths[cell] - widths[mapped[i][0]]) * floor['site']['height'] for i, cell in cand_edits)
                if grow > 0 and used + grow > SIZE_MAX_UTILIZATION * rows_area:
                    continue
                used += max(grow, 0)
                nets_taken |= touched_nets
                for i, cell in cand_edits:
                    insts.add(i)
                    take.append((i, cell))
                    accepted[i] = cell
            history.append(dict(round=rnd, candidates=len(cands), improving=len(scored), base=base,
                                accepted=take, first_size=len(scored[0][1][1]) if scored and take else 1,
                                exit_code=r['exit_code'], seconds=round(r.get('elapsed_seconds') or 0.0, 2)))
            prev_base = base
            if not take:
                break
        last = history[-1] if history else None
        if last and last.get('accepted') and len(last['accepted']) > last.get('first_size', 1) and \
                not last.get('reverted') and budget - spent >= 1:
            # The last round's edits were never timed together (the next
            # round's base does that): one candidate with all of them on top
            # of the base they were chosen on; worse than that base -> keep
            # only the round's best edit.
            newer = dict(last['accepted'])
            older = {i: c for i, c in accepted.items() if i not in newer}
            doc = dict(schema='rusteda.edits.v1', base=[dict(op='resize', instance=i, cell=c) for i, c in sorted(older.items())],
                       candidates=[dict(id='v0', edits=[dict(op='resize', instance=i, cell=c) for i, c in last['accepted']])],
                       report=dict(endpoints=[], paths=[]))
            vf = jdump(h.root / f'{t}_whatif_verify.json', doc)
            r = h.run(f'{t}_whatif_verify', [str(x) for x in [
                self.tools['ruststa'], '-w', sta['workspace'], '--format', 'json', 'whatif', '--rev', sta['revision'],
                '--edits', vf, '--max-candidates', '1024', '--max-edits', '4096', '--timeout', WHATIF_TIMEOUT_S,
                '--threads', '2', '-q']])
            spent += 1
            self.whatif_spent += 1
            try:
                d = json.loads(bounded_text(Path(r['stdout']), 256 * 1024 ** 2))
            except (ValueError, OSError):
                d = {}
            cand = (d.get('candidates') or [{}])[0]
            ok = r['exit_code'] == 0 and d.get('base') and cand.get('status') == 'evaluated' and cand.get('values')
            if ok:
                together, before = metrics(cand['values']), last['base']
                if not better(together, before, 'drv'):
                    for i, _ in last['accepted'][last.get('first_size', 1):]:
                        accepted.pop(i, None)
                    last['reverted'] = [i for i, _ in last['accepted'][last.get('first_size', 1):]]
                last['verified'] = dict(together=together, kept=not last.get('reverted'))
            else:
                last['verified'] = dict(status='failed', exit_code=r['exit_code'])
        ops = [dict(op='resize_cell', instance=i, cell=c) for i, c in sorted(accepted.items())]
        summary = dict(resized=len(ops), candidates=len(prio), traced_setup_endpoints=traced,
                       whatif_candidates=spent, whatif_loop_candidates=self.whatif_spent,
                       failing_setup_endpoints=len({e for _, _, e in failing}), evaluated=evaluated,
                       rounds=history, bulk=bulk, utilization_after=round(used / rows_area, 4), max_width_dbu=max_width,
                       oracle='ruststa whatif (resize on the layout SPEF)')
        return ops, summary

    def whatif_bulk(self, t, sta, order, prio, mapped, family, widths, max_width, floor, rows_area, used, room,
                    metrics, better) -> dict | None:
        """Bulk DRV upsizing, decided by one RustSTA `whatif` call.

        GF180 ss leaves many small-fanout nets marginally above the transition
        limit; the planner leaves them to sizing, and one-instance-at-a-time
        candidates exhaust the count budget long before they are all tried.
        Two combined candidates are evaluated first: every DRV driver (worst
        first) one step up, and two steps up, within the tap gap and the
        utilization cap. The better of them is accepted as a whole only when
        RustSTA says it improves (`better`, the same rule as single resizes:
        fewer violations without setup going below zero, hold within its
        allowance); the single-resize rounds then continue on top of it.
        Costs two candidates of the budget.
        """
        drivers = [i for i in order if prio[i][1] == 'drv']
        if not drivers or room < 2:
            return None
        h = self.h
        height = floor['site']['height']
        cap = SIZE_MAX_UTILIZATION * rows_area
        bulks = []
        for step in (1, 2):
            edits, grow = [], 0
            for inst in drivers:
                master = mapped[inst][0]
                k, members = family[master]
                j = min(k + step, len(members) - 1)
                while j > k and max_width is not None and widths[members[j]] > max_width:
                    j -= 1
                if j <= k:
                    continue
                g = (widths[members[j]] - widths[master]) * height
                if used + grow + g > cap:
                    continue
                grow += g
                edits.append((inst, members[j]))
            if edits and all(edits != b[0] for b in bulks):
                bulks.append((edits, grow))
        if not bulks:
            return dict(evaluated=0, candidates=0, accepted=[], grow=0, reason='no driver can grow within the caps')
        doc = dict(schema='rusteda.edits.v1', base=[],
                   candidates=[dict(id=f'b{k}', edits=[dict(op='resize', instance=i, cell=c) for i, c in e])
                               for k, (e, _) in enumerate(bulks)],
                   report=dict(endpoints=[], paths=[]))
        ef = jdump(h.root / f'{t}_whatif_bulk.json', doc)
        r = h.run(f'{t}_whatif_bulk', [str(x) for x in [
            self.tools['ruststa'], '-w', sta['workspace'], '--format', 'json', 'whatif', '--rev', sta['revision'],
            '--edits', ef, '--max-candidates', '1024', '--max-edits', '4096', '--timeout', WHATIF_TIMEOUT_S,
            '--threads', '2', '-q']])
        out = dict(evaluated=len(bulks), candidates=[len(e) for e, _ in bulks], accepted=[], grow=0,
                   exit_code=r['exit_code'])
        try:
            d = json.loads(bounded_text(Path(r['stdout']), 256 * 1024 ** 2))
        except (ValueError, OSError):
            d = {}
        if not d.get('base') or r['exit_code'] != 0:
            out['reason'] = 'whatif failed'
            return out
        base = metrics(d['base'])
        best = None
        for c in d.get('candidates') or []:
            if c.get('status') != 'evaluated' or not c.get('values'):
                continue
            m = metrics(c['values'])
            k = int(c['id'][1:])
            if better(m, base, 'drv'):
                score = (m['drv'], -m['wns'], -m['tns'], k)
                if best is None or score < best[0]:
                    best = (score, k, m)
        out['base'] = base
        if best is None:
            out['reason'] = 'no bulk candidate improves'
            return out
        _, k, m = best
        out.update(accepted=bulks[k][0], grow=bulks[k][1], step=k + 1, after=m)
        return out

    def closure_eco(self, it: int, netlist: Path, sta: dict, phys: dict, apply_to: Path | None = None,
                    frozen: frozenset = frozenset()) -> tuple[Path | None, dict]:
        """DRV + setup repair of one layout (track recipe): buffer insertion
        (scripts/closure_eco.plan_buffering: repeaters / port buffers /
        critical-sink isolation on the nets RustSTA reports with DRV
        violations, located by the layout's DEF) plus gate sizing
        (size_plan), applied as one typed plan by `rustroad apply-eco
        --sizing-profile`. `apply_to`: apply the plan to this netlist instead
        (the hold ECO's output of the same iteration; `frozen` are the sink
        pins its chains moved, never re-buffered). Returns (None, summary)
        when nothing applies."""
        import closure_eco as CE  # noqa: E402
        import rtl2gds_logic as L  # noqa: E402
        t, h = f'{self.tag}_i{it}', self.h
        prof = json.loads(self.design.sizing_profile.read_text())
        cfg = json.loads(self.config.read_text())
        floor = json.loads(self.design.floorplan.read_text() if self.design.floorplan else self.floorplan().read_text())
        site_w = floor['site']['width']
        spacing = int(cfg.get('cell_spacing_sites') or 0) * site_w
        max_width = (cfg['tap']['max_gap'] // site_w) * site_w - spacing if cfg.get('tap') else None
        lef = L.lef_pins(Path(self.p['cell_lef']['path']))
        widths = lef_widths(Path(self.p['cell_lef']['path']), floor['units_per_micron'])
        drv_rows = json.loads(bounded_text(Path(sta['files']['drv']), 64 * 1024 ** 2))['sections']['drv']['rows']
        pins = {c: {x['pin']: x['slack_ns'] for x in json.loads(bounded_text(Path(sta['files'][f'pins_{c}']), 64 * 1024 ** 2))
                    ['sections']['pins']['rows']} for c in PATH_AWARE_SETUP_CORNERS}
        setup_failing = any(sta['corners'][c]['worst_setup_ns'] < 0 for c in ('tt', 'ss'))
        text = bounded_text(netlist, 64 * 1024 ** 2)
        rows_area = sum((r['x_end'] - r['x_start']) for r in floor['rows']) * floor['site']['height']
        used = sum(widths.get(m, 0) for m, _ in netlist_instances(text).values()) * floor['site']['height']
        room = max(0, int((SIZE_MAX_UTILIZATION * rows_area - used) // floor['site']['height']))
        # Buffer names continue after every rr_eco_buf_ of the netlist the plan
        # is applied to (the hold ECO's output may hold isolation buffers).
        seq = max(CE.next_sequence(text), CE.next_sequence(bounded_text(apply_to, 64 * 1024 ** 2)) if apply_to else 0)
        buf = CE.plan_buffering(text, lef, bounded_text(Path(phys['paths']['def']), 64 * 1024 ** 2), prof, drv_rows, pins,
                                'ss', max_width, seq, floor['site']['height'], widths, setup_failing,
                                frozen, room, self.hold_drive(), spacing, getattr(self, 'left_to_sizing', frozenset()))
        # A small-fanout net that whatif sizing does not fix by the next layout
        # gets buffered then.
        self.left_to_sizing = frozenset(k for k, v in buf['skipped'].items() if v.startswith('fanout <= 2'))
        skip = set(buf['resizes']) | set(buf.get('touched') or [])
        n = CE.parse_netlist(text, lef)
        for rec in buf['records']:
            d = n.driver.get(rec['net'], '')
            if '/' in d:
                skip.add(d.rsplit('/', 1)[0])
        size_ops, summary = self.whatif_sizing(it, netlist, sta, skip, buf['added_width_dbu'])
        resizes = [dict(op='resize_cell', instance=i, cell=c) for i, (c, _) in sorted(buf['resizes'].items())]
        ops = buf['ops'] + resizes + size_ops
        summary.update(buffers=sum(1 for o in buf['ops'] if o['op'] == 'buffer_sinks'), buffer_nets=len(buf['records']),
                       driver_resizes=len(resizes),
                       buffering=dict(records=buf['records'][:60], skipped=dict(sorted(buf['skipped'].items())[:40]),
                                      added_width_dbu=buf['added_width_dbu'], nets_with_drv_rows=buf['nets_with_rows']),
                       library_limited=dict(sorted(buf.get('library_limited', {}).items())[:64]))
        summary['resized'] = len(size_ops) + len(resizes)
        if not ops:
            return None, summary
        plan = jdump(h.root / f'{t}_size_plan.json', dict(schema='rustroad.eco-plan.v1', module=self.top, operations=ops))
        out = h.root / f'{self.tag}_i{it + 1}_mapped_sized.v'
        if apply_to is not None:
            out = h.root / f'{self.tag}_i{it + 1}_mapped_hold_sized.v'
        run_ok(h, f'{t}_size_eco', [self.tools['rustroad'], 'apply-eco', '--netlist', apply_to or netlist, '--plan', plan,
                                    '--delay-cells', self.delay_cells, '--sizing-profile', self.design.sizing_profile,
                                    '--out', out])
        summary['plan'] = fingerprint(plan)
        summary['ops'] = len(ops)
        # Applied: the next layout seeds the co-sized buffers next to their
        # drivers and keeps those drivers' nets short.
        self.seed_positions.update(buf.get('positions') or {})
        self.drv_weights.update(buf.get('weights') or {})
        summary['co_sizing'] = dict(positions=len(buf.get('positions') or {}), weighted_nets=len(buf.get('weights') or {}),
                                    arrangements={m: sum(1 for r in buf['records'] if r.get('arrangement') == m)
                                                  for m in ('cluster', 'driver', 'root')})
        return out, summary

    def hold_isolation(self, it: int, out: Path, plan: dict, sta: dict, phys: dict) -> tuple[Path, dict | None]:
        """DRV-aware hold insertion (track recipe): a sizing-profile buffer
        behind every hold chain whose sink-side delay cell cannot drive its
        point's load within the transition target (closure_eco.
        plan_hold_isolation; setup-guarded per point), applied to the hold
        ECO's output `out`. Returns (netlist, summary); `out` unchanged when
        nothing applies."""
        if self.design.sizing_profile is None or not plan.get('points'):
            return out, None
        import closure_eco as CE  # noqa: E402
        import rtl2gds_logic as L  # noqa: E402
        t, h = f'{self.tag}_i{it}', self.h
        prof = json.loads(self.design.sizing_profile.read_text())
        cfg = json.loads(self.config.read_text())
        floor = json.loads(self.design.floorplan.read_text() if self.design.floorplan else self.floorplan().read_text())
        site_w, row_h = floor['site']['width'], floor['site']['height']
        spacing = int(cfg.get('cell_spacing_sites') or 0) * site_w
        max_width = (cfg['tap']['max_gap'] // site_w) * site_w - spacing if cfg.get('tap') else None
        lef = L.lef_pins(Path(self.p['cell_lef']['path']))
        widths = lef_widths(Path(self.p['cell_lef']['path']), floor['units_per_micron'])
        drv_rows = json.loads(bounded_text(Path(sta['files']['drv']), 64 * 1024 ** 2))['sections']['drv']['rows']
        limits = [r['limit'] for r in drv_rows if r['kind'] == 'max_transition']
        if not limits:
            return out, dict(status='skipped', reason='no max_transition limit in the drv report')
        def_text = bounded_text(Path(phys['paths']['def']), 64 * 1024 ** 2)
        cw = CE.layout_wire_cap(bounded_text(Path(sta['timed_netlist']), 64 * 1024 ** 2), lef, def_text, prof, drv_rows,
                                'ss', widths, row_h)
        text = bounded_text(out, 64 * 1024 ** 2)
        iso = CE.plan_hold_isolation(text, lef, def_text, prof, plan['points'], self.hold_drive(), min(limits),
                                     CE.next_sequence(text), row_h, widths, cw, float(self.args.setup_margin), max_width,
                                     spacing=spacing)
        summary = dict(status='none', records=iso['records'][:64], skipped=dict(sorted(iso['skipped'].items())[:32]),
                       wire_cap_pf_per_dbu=cw)
        if not iso['ops']:
            return out, summary
        ef = jdump(h.root / f'{t}_hold_iso_plan.json', dict(schema='rustroad.eco-plan.v1', module=self.top,
                                                            operations=iso['ops']))
        result = h.root / f'{self.tag}_i{it + 1}_mapped_hold_iso.v'
        run_ok(h, f'{t}_hold_iso_eco', [self.tools['rustroad'], 'apply-eco', '--netlist', out, '--plan', ef,
                                        '--delay-cells', self.delay_cells, '--sizing-profile', self.design.sizing_profile,
                                        '--out', result])
        self.seed_positions.update(iso['positions'])
        self.drv_weights.update(iso['weights'])
        summary.update(status='applied', buffers=len(iso['ops']), plan=fingerprint(ef))
        return result, summary

    def placement_seed(self, it: int, phys: dict) -> Path:
        """rustroad.placement-seed.v1 of this layout's cell positions (DEF
        COMPONENTS, lower-left DBU): the next ECO layout is legalized from it
        instead of re-running global placement (stable wires between ECOs)."""
        cells = {}
        for line in bounded_text(Path(phys['paths']['def']), 64 * 1024 ** 2).split('\nCOMPONENTS ', 1)[1].split('\nEND COMPONENTS', 1)[0].splitlines():
            m = re.match(r'- (\S+) \S+ .*?\+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \)', line)
            if m:
                cells[m[1].lstrip('\\')] = [int(m[2]), int(m[3])]
        source = {'def': fingerprint(Path(phys['paths']['def']))}
        planned = getattr(self, 'seed_positions', None) or {}
        if planned:
            # DRV co-sizing: buffers (new or moved) next to their weak driver,
            # hold chains and their isolation buffer next to their sinks.
            moved = sum(1 for k in planned if k in cells)
            for inst, (x, y) in sorted(planned.items()):
                cells[inst] = [int(x), int(y)]
            source['eco_positions'] = dict(new=len(planned) - moved, moved=moved)
            self.seed_positions = {}
        return jdump(self.h.root / f'{self.tag}_i{it + 1}_placement_seed.json',
                     dict(schema='rustroad.placement-seed.v1', source=source, cells=dict(sorted(cells.items()))))

    def layout_weights(self, it: int) -> Path | None:
        """Net weights of layout `it`: the timing-driven ones (while setup
        failed) merged (max) with the DRV co-sizing weights of weak drivers'
        nets; None when neither exists."""
        drv = getattr(self, 'drv_weights', None) or {}
        if not drv:
            return self.weights
        w = json.loads(self.weights.read_text()) if self.weights else \
            dict(schema='rustroad.net-weights.v1', source=dict(rule='no timing-driven weights'), weights={})
        merged = dict(w['weights'])
        for net, v in drv.items():
            merged[net] = max(merged.get(net, 1000), v)
        w['weights'] = dict(sorted(merged.items()))
        w['source'] = dict(w.get('source') or {}, drv_driver_nets=len(drv),
                           drv_rule='nets of weak drivers to the buffer or hold isolation placed next to them')
        return jdump(self.h.root / f'{self.tag}_i{it}_net_weights_layout.json', w)

    def hold_drive(self) -> dict:
        """Delay-cell master -> (characterized load pF, worst output
        transition ns at ss) from the delay-cell profile."""
        prof = json.loads(self.delay_cells.read_text())
        load = prof['representative']['load_pf']
        return {b['name']: (load, max(b['output_slew_ns']['ss'].values())) for b in prof['buffers']
                if 'ss' in b.get('output_slew_ns', {})}

    def net_weights(self, it: int, netlist: Path, sta: dict) -> Path | None:
        """Timing-driven placement weights from this layout's per-pin setup
        slack (scripts/closure_eco.net_weights); None when nothing is weighted."""
        import closure_eco as CE  # noqa: E402
        import rtl2gds_logic as L  # noqa: E402
        pins = {c: {x['pin']: x['slack_ns'] for x in json.loads(bounded_text(Path(sta['files'][f'pins_{c}']), 64 * 1024 ** 2))
                    ['sections']['pins']['rows']} for c in PATH_AWARE_SETUP_CORNERS}
        w = CE.net_weights(bounded_text(netlist, 64 * 1024 ** 2), L.lef_pins(Path(self.p['cell_lef']['path'])), pins)
        if not w['weights']:
            return None
        w['source'].update(sta_workspace=sta['workspace'], sta_revision=sta['revision'], netlist=fingerprint(netlist))
        return jdump(self.h.root / f'{self.tag}_i{it + 1}_net_weights.json', w)

    # ------------------------------------------------------------ antenna
    def check_antenna(self, label: str, workspace: str, extra: Path | None = None) -> dict:
        # Rows are worst-first; ANTENNA_REPORT_ROWS keeps every row of the
        # required designs (regfile 8,334) and bounds SERV-size reports.
        argv = [self.tools['rustroad'], '--workspace', workspace, 'check-antenna', '--max-rows', ANTENNA_REPORT_ROWS]
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

    def plant_shape(self, phys: dict, port: str | None = None, length_um: int | None = None) -> dict:
        cfg = dict(PLANT[self.tag], **({'port': port} if port else {}), **({'length_um': length_um} if length_um else {}))
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
        # The strip runs through the pin centre (where routes land on the
        # track floorplans' I/O pins), on a 10-DBU grid (GDS divisor 2).
        cx = ((lx + ux) // 2 - width // 2) // 10 * 10
        cy = ((ly + uy) // 2 - width // 2) // 10 * 10
        require(lx <= cx and cx + width <= ux and ly <= cy and cy + width <= uy,
                f'{self.tag}: planted strip does not fit the pin')
        if ly == d[1]:
            rect = [cx, ly - length, cx + width, uy]
        elif uy == d[3]:
            rect = [cx, ly, cx + width, uy + length]
        elif lx == d[0]:
            rect = [lx - length, cy, ux, cy + width]
        else:
            require(ux == d[2], f'{self.tag}: port pin is not on the die boundary')
            rect = [lx, cy, ux + length, cy + width]
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
        pins_of = {b['name']: (b['input_pin'], b['output_pin']) for b in profile['buffers']}
        used = {b['name'] for b in profile['buffers'] if re.search(rf'(?m)^\s*{re.escape(b["name"])}\s', text)}
        if self.design.sizing_profile is not None:
            # Drive-strength buffers the buffering ECO inserted (rr_eco_buf_*).
            fams = json.loads(self.design.sizing_profile.read_text())['families']
            for f in fams:
                ins = [k for k, d in f['pins'].items() if d == 'input']
                outs = [k for k, d in f['pins'].items() if d == 'output']
                if len(ins) == 1 and len(outs) == 1:
                    for c in f['cells']:
                        pins_of.setdefault(c['name'], (ins[0], outs[0]))
            for master, name in re.findall(r'(?m)^\s*(\S+)\s+\\?(rr_eco_buf_\d+)\s', text):
                require(master in pins_of, f'{name}: inserted master {master} is not a profiled buffer')
                used.add(master)
        used = sorted(used)
        proofs = {}
        for name in used:
            b = dict(name=name, input_pin=pins_of[name][0], output_pin=pins_of[name][1])
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
        clock = json.loads(self.config.read_text()).get('clock') or {}
        clock_cells = sorted({c for c in [clock.get('cell'), *clock.get('level_cells', [])]
                              if c and re.search(rf'(?m)^\s*{re.escape(c)}\s', text)})
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
        # Track-recipe loop state (reset per call: the determinism rebuild and
        # the controls run their own loops on fresh layouts).
        self.weights = None  # timing-driven net weights of the next layout
        # DRV co-sizing: placement seeds of cells that must sit next to their
        # driver (next layout only) and weights of those drivers' nets (kept).
        self.seed_positions, self.drv_weights = {}, {}
        self.left_to_sizing = frozenset()  # small-fanout DRV nets the last plan left to whatif
        self.last_setup_eco, self.no_progress, self.eco_blocked = None, 0, None
        self.whatif_spent = 0
        seed, best = None, None
        for it in range(self.args.max_iterations):
            try:
                phys = self.physical(it, netlist, self.layout_weights(it), seed)
                sta = self.sta(it, phys)
            except RuntimeError as error:
                # Track recipe: an ECO layout that cannot be built ends the
                # loop; the last complete layout stays final (closure failed
                # if it was not closed) and every oracle still runs on it.
                if final is None or self.design.sizing_profile is None:
                    raise
                iterations[-1]['next_iteration_failed'] = dict(iteration=it, netlist=fingerprint(netlist),
                                                              reason=str(error)[-1500:])
                break
            record = dict(iteration=it, netlist=fingerprint(netlist), physical={k: v for k, v in phys.items() if k not in ('road',)},
                          timing=dict(closed=sta['closed'], corners={c: {k: v for k, v in d.items() if k != 'endpoints'} for c, d in sta['corners'].items()},
                                      endpoints={c: d['endpoints'] for c, d in sta['corners'].items()}, files=sta['files'],
                                      metrics=sta['metrics'], diagnostics=sta['diagnostics']))
            antenna = self.check_antenna(f'{self.tag}_i{it}', phys['workspace'])
            record['antenna'] = antenna
            iterations.append(record)
            final = (it, netlist, phys, sta, antenna)
            if self.design.sizing_profile is not None:
                record['score'] = self.layout_score(sta, antenna)
                if best is None or record['score'] <= best[0]:
                    best = (record['score'], final)
                record['drv'] = sta['drv']
                netlist, stop = self.sized_step(it, netlist, sta, phys, antenna, record)
                # The full decisions of this step (the gate summary keeps counts).
                jdump(self.h.root / f'{self.tag}_i{it}_closure_decisions.json',
                      {k: record.get(k) for k in ('size_eco', 'hold_eco', 'hold_isolation', 'setup_stagnant', 'stopped')})
                if stop:
                    break
                # The next layout is an ECO of this one: seeded placement.
                seed = self.placement_seed(it, phys)
                continue
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
        if best is not None and best[1][0] != final[0]:
            # Track recipe: the gate checks the best complete layout of the
            # loop, not merely the last one (a late ECO can lose ground).
            iterations[-1]['selected_iteration'] = dict(
                iteration=best[1][0], score=best[0], last_score=iterations[-1].get('score'),
                rule='fewest failing categories (setup, hold, DRV, antenna), then the smallest '
                     'setup + hold deficit + 0.01 ns per DRV violation; ties: the later layout')
            final = best[1]
        return iterations, final

    def layout_score(self, sta: dict, antenna: dict) -> list:
        """[failing categories, deficit] of one layout (smaller is better)."""
        c = sta['corners']
        setup = sum(max(0.0, -c[x]['worst_setup_ns']) for x in ('tt', 'ss') if x in c)
        hold = sum(max(0.0, self.args.target_margin - v['worst_hold_ns']) for v in c.values())
        drv = (sta.get('drv') or {}).get('violations') or 0
        fails = int(setup > 0) + int(hold > 0) + int(drv > 0) + int(antenna['status'] == 'violated')
        return [fails, round(setup + hold + 0.01 * drv, 6)]

    def sized_step(self, it: int, netlist: Path, sta: dict, phys: dict, antenna: dict, record: dict) -> tuple[Path, bool]:
        """One decision of the closure loop with gate sizing (track recipe).

        Order (explicit bound: `max_iterations` layouts in all):
          1. DRV violated, or setup < 0 at tt/ss and the last setup ECO still
             helped -> DRV/setup ECO (`closure_eco`: buffering + sizing), and
             timing-driven net weights for the next placement while setup fails;
          2. hold below the target -> path-aware hold ECO (partial plans
             apply; infeasible endpoints are recorded with their evidence);
          3. antenna violated on a timing- and DRV-clean layout -> antenna ECO.
        DRV/setup count as stagnant (not repairable by this loop) after
        SETUP_STAGNATION_ROUNDS consecutive DRV/setup ECOs that improved
        neither the DRV count nor the worst tt/ss setup slack (by
        SETUP_STAGNATION_NS); the loop then goes on to hold. Returns (netlist for the next iteration, stop).
        """
        corners = sta['corners'].values()
        setup_ok = all(v['worst_setup_ns'] >= 0 for v in corners)
        hold_ok = all(v['worst_hold_ns'] >= self.args.target_margin for v in corners)
        drv_ok = not (sta['drv'] or {}).get('violations')
        raised = None
        if setup_ok and hold_ok and drv_ok and antenna['status'] != 'violated':
            ref = self.reference_hold(it, phys)
            record['reference_hold'] = ref
            short = ref.get('shortfall_ns')
            if not short:
                return netlist, True
            # Native closed but the reference SPEF is below the gate margin:
            # raise the native target by the measured shortfall (+10 ps).
            raised = round(self.args.target_margin + short + 0.01, 4)
            hold_ok = False
        if it + 1 == self.args.max_iterations:
            record['stopped'] = 'iteration bound reached'
            return netlist, True
        worst = min(sta['corners'][c]['worst_setup_ns'] for c in ('tt', 'ss'))
        drv_count = (sta['drv'] or {}).get('violations') or 0
        record['worst_setup_ns'] = worst
        prev = getattr(self, 'last_setup_eco', None)
        # Violating nets of this layout: an ECO that repaired some of them
        # made progress even when new marginal ones keep the count (GF180
        # mult: one net at 1.00-1.06 ns per layout, a different one each time).
        drv_nets = frozenset(r.get('net') or r['pin'] for r in json.loads(bounded_text(
            Path(sta['files']['drv']), 64 * 1024 ** 2))['sections']['drv']['rows'] if r['slack'] < 0)
        repaired = bool(prev is not None and len(prev) > 3 and prev[3] - drv_nets)
        if prev is not None and prev[0] == it - 1 and worst < prev[1] + SETUP_STAGNATION_NS and drv_count >= prev[2] \
                and not repaired:
            self.no_progress = getattr(self, 'no_progress', 0) + 1
        elif prev is not None and prev[0] == it - 1:
            self.no_progress = 0
        if getattr(self, 'no_progress', 0) >= SETUP_STAGNATION_ROUNDS and not getattr(self, 'eco_blocked', None):
            # The last DRV/setup ECOs helped neither: stop spending layouts on them.
            self.eco_blocked = dict(iteration=it, previous_worst_ns=prev[1], worst_ns=worst, previous_drv=prev[2],
                                    drv=drv_count, bound_ns=SETUP_STAGNATION_NS, rounds=SETUP_STAGNATION_ROUNDS)
        blocked = getattr(self, 'eco_blocked', None)
        if blocked:
            record['setup_stagnant'] = blocked
        if not setup_ok and not blocked:
            self.weights = self.net_weights(it, netlist, sta) or self.weights
            record['net_weights'] = fingerprint(self.weights) if self.weights else None
        want_eco = (not drv_ok or not setup_ok) and not blocked
        # The first DRV/setup ECO runs alone (it changes the most); later
        # ones share the layout with the hold ECO (one layout for both).
        combined = want_eco and not hold_ok and self.last_setup_eco is not None
        if want_eco and not combined:
            out, summary = self.closure_eco(it, netlist, sta, phys)
            record['size_eco'] = summary
            if out is not None:
                self.last_setup_eco = (it, worst, drv_count, drv_nets)
                return out, False
        if not hold_ok:
            if raised is None and self.design.sizing_profile is not None:
                # Plan above the target: chain delays at the profile's one
                # representative load under-deliver on routed layouts.
                raised = round(self.args.target_margin + HOLD_PLAN_EXTRA_NS, 4)
            try:
                out, plan = self.hold_eco(it, netlist, sta, target=raised)
            except RuntimeError as error:
                record['hold_eco'] = dict(status='failed', reason=str(error)[-1500:])
                written = self.h.root / f'{self.tag}_i{it}_hold_plan.json'
                if written.is_file():
                    # HOLD_ECO_INFEASIBLE: the plan lists every endpoint with
                    # the setup budget of each point of its worst hold path.
                    done = json.loads(bounded_text(written))
                    record['hold_eco'].update(plan_status=done.get('status'), infeasible=done.get('infeasible', [])[:64],
                                              plan=fingerprint(written))
                if combined:
                    out, summary = self.closure_eco(it, netlist, sta, phys)
                    record['size_eco'] = summary
                    if out is not None:
                        self.last_setup_eco = (it, worst, drv_count, drv_nets)
                        return out, False
                return netlist, True
            record['hold_eco'] = dict(status=plan['status'], totals=plan['totals'], sinks=plan['sinks'],
                                      points=plan.get('points', []), infeasible=plan.get('infeasible', []),
                                      plan=fingerprint(self.h.root / f'{self.tag}_i{it}_hold_plan.json'))
            if plan['status'] == 'closed':
                record['stopped'] = 'hold ECO planned nothing'
                return netlist, True
            out, iso = self.hold_isolation(it, out, plan, sta, phys)
            if iso is not None:
                record['hold_isolation'] = iso
            if combined:
                frozen = set()
                for op in plan['eco']['operations']:
                    if op['op'] == 'insert_buffer':
                        frozen.add(f"{op['sink']['instance']}/{op['sink']['pin']}")
                    elif op['op'] == 'buffer_sinks':
                        frozen |= {f"{x['instance']}/{x['pin']}" for x in op['sinks']}
                both, summary = self.closure_eco(it, netlist, sta, phys, apply_to=out, frozen=frozenset(frozen))
                record['size_eco'] = summary
                if both is not None:
                    self.last_setup_eco = (it, worst, drv_count, drv_nets)
                    return both, False
            return out, False
        if antenna['status'] == 'violated' and setup_ok and drv_ok:
            repaired = self.h.root / f'{self.tag}_i{it + 1}_mapped_ant.v'
            plan = self.antenna_eco(f'{self.tag}_i{it}', netlist, antenna['report'], repaired)
            record['antenna_eco'] = dict(status=plan['status'], nets=plan['nets'])
            return repaired, False
        record['stopped'] = 'no repair applies (setup/DRV not repairable further)'
        return netlist, True

    def reference_hold(self, it: int, phys: dict) -> dict:
        """Worst hold per corner on OpenRCX SPEF of the same DEF (RustSTA), and
        the shortfall below the gate's hold margin (None: none, or unknown)."""
        import compare_spef  # noqa: E402
        spefs = {}
        try:
            for c in self.libs:
                spefs[c], _ = compare_spef.openrcx(self.h, f'{self.tag}_i{it}_ref', self.tag, self.rc[c],
                                                   Path(phys['paths']['def']), f'{self.tag}_i{it}_ref_{self.rc[c]}',
                                                   timeout_s=max(25, self.h.args.timeout_seconds - 30),
                                                   file_limit_bytes=self.sta_file_limit())
            ref = self.sta(it, phys, label='sta_ref', spefs=spefs)
        except (RuntimeError, OSError, KeyError, ValueError) as error:
            return dict(status='unverified', reason=str(error)[-600:], shortfall_ns=None)
        worst = {c: v['worst_hold_ns'] for c, v in ref['corners'].items()}
        margin = getattr(self.args, 'gate_margin', self.args.target_margin)
        short = max(0.0, max(margin - h for h in worst.values()))
        return dict(status='checked', worst_hold_ns=worst, gate_margin_ns=margin,
                    shortfall_ns=round(short, 6) if short > 0 else None)

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
