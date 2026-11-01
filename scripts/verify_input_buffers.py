#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Bounded dual-PDK verification of nonclock primary-input buffer trees.

Each fixture has fewer than100 cells and1000 pins. Native import/export and
black-box Yosys CEC run serially under1GiB/120s limits. Expected outputs are
direct RTL equations; a wrong output and a configured inverter are rejected
by the external equivalence oracle. Physical signoff is a separate gate.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from verify_logic import bounded_text, sha256
from verify_physical import PhysicalHarness, external_cec, run_ok

ROOT = Path(__file__).resolve().parents[1]


def inspect(metadata, role, expected_buffers):
    physical = metadata['physical']
    report = physical['input_buffers']
    model = physical['connectivity']
    if len(model['pins']) > 1000 or len(model['instance_masters']) > 100:
        raise RuntimeError('verification fixture object budget exceeded')
    if len(report['buffers']) != expected_buffers or len(report['trees']) != 1:
        raise RuntimeError('unexpected input tree size')
    if report['skipped_clock_inputs'] != 1:
        raise RuntimeError('clock input was not explicitly left for CTS')
    buffers = {b['instance'] for b in report['buffers']}
    outputs, loads = set(), {}
    for pin in model['pins']:
        owner, net = pin['owner'], pin['net']
        if owner['kind'] != 'instance' or net is None:
            continue
        terminal = model['masters'][model['instance_masters'][owner['instance']]]['pins'][owner['terminal']]
        if terminal['direction'] == 'input':
            loads[net] = loads.get(net, 0) + 1
        if owner['instance'] in buffers and terminal['name'] == role['output_pin']:
            outputs.add(net)
    tree = report['trees'][0]
    port = next(i for i, p in enumerate(model['ports']) if p['name'] == tree['port'])
    root = next(p['net'] for p in model['pins']
                if p['owner'] == dict(kind='port', port=port, bit=tree['bit']))
    if len(outputs) != expected_buffers or any(not 1 <= loads.get(net, 0) <= 8 for net in outputs | {root}):
        raise RuntimeError('independent pin census found an invalid buffer fanout')
    return dict(buffers=expected_buffers, source_sinks=tree['sinks'], levels=tree['levels'],
                max_observed_fanout=max(loads[net] for net in outputs | {root}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rustroad', type=Path, required=True)
    parser.add_argument('--yosys', type=Path, default=ROOT / '.work/tools/yosys')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    road, yosys, out = args.rustroad.resolve(), args.yosys.resolve(), args.out.resolve()
    if not road.is_file() or not yosys.is_file():
        print(json.dumps(dict(status='unverified', reason='native or reference executable missing')))
        return 3
    out.mkdir(parents=True, exist_ok=False)
    profiles = json.loads((ROOT / 'fixtures/pdk/local-routing.json').read_text())['pdks']
    rows = []
    for profile in profiles:
        tag = profile['tag']
        for role in ['technology_lef', 'cell_lef', 'liberty']:
            pin = profile[role]
            if sha256(Path(pin['path'])) != pin['sha256']:
                raise RuntimeError(f'{tag}: {role} identity changed')
        for variant in ['scalar', 'bus_down', 'bus_up', 'inverting_role']:
            folder = out / (tag + '-' + variant)
            folder.mkdir()
            harness = PhysicalHarness(folder, argparse.Namespace(threads=2, memory_mib=1024,
                                                                 timeout_seconds=120))
            config = json.loads((ROOT / profile['config']).read_text())
            policy_path = ROOT / 'fixtures/pdk' / (tag + '-input-buffer.json')
            policy = json.loads(policy_path.read_text())
            data_buffer = dict(policy)
            count = 73 if variant == 'scalar' else 17
            bad = variant == 'inverting_role'
            if bad:
                policy['cell'] = policy['cell'].replace('__buf_4', '__inv_4')
                policy['output_pin'] = 'Y' if tag == 'sky130' else 'ZN'
            config.update(max_added_cells=1000, input_buffer=policy)
            config_path = folder / 'physical.json'
            config_path.write_text(json.dumps(config, indent=2) + '\n')
            bus = '[0:1]' if variant == 'bus_up' else '[1:0]'
            vector = variant.startswith('bus_')
            port = f'input {bus} a' if vector else 'input a'
            source_bit = 'a[0]' if vector else 'a'
            source = folder / 'mapped.v'
            clock_buffer = config['clock']
            statements = [f"{data_buffer['cell']} d{i} (.{data_buffer['input_pin']}({source_bit}), .{data_buffer['output_pin']}(y[{i}]));"
                          for i in range(count)]
            statements += [f"{clock_buffer['cell']} c{i} (.{clock_buffer['input_pin']}(clk), .{clock_buffer['output_pin']}(c[{i}]));"
                           for i in range(9)]
            source.write_text(f'module lane_smoke({port}, input clk, output [{count-1}:0] y, output [8:0] c);\n'
                              + '\n'.join(statements) + '\nendmodule\n')
            expected, wrong = folder / 'expected.v', folder / 'wrong.v'
            header = f'module lane_smoke({port}, input clk, output [{count-1}:0] y, output [8:0] c);\n'
            expected.write_text(header + f'assign y={{{count}{{{source_bit}}}}}; assign c={{9{{clk}}}}; endmodule\n')
            wrong.write_text(header + f"assign y={{{count}{{{source_bit}}}}} ^ {count}'b1; assign c={{9{{clk}}}}; endmodule\n")
            workspace = folder / 'workspace'
            run_ok(harness, 'import', [road, '--workspace', workspace, 'import-pdk', '--base-rev', 'rev0',
                                      '--tech-lef', profile['technology_lef']['path'],
                                      '--cell-lef', profile['cell_lef']['path'], '--netlist', source,
                                      '--floorplan', ROOT / 'fixtures/designs/pipeline' / f'{tag}-floorplan.json',
                                      '--physical-config', config_path])
            revision = (workspace / 'HEAD').read_text().strip()
            metadata_path = workspace / 'revisions' / revision / 'design.json'
            geometry = inspect(json.loads(bounded_text(metadata_path)), policy, 12 if count == 73 else 3)
            exported = Path(run_ok(harness, 'export', [road, '--workspace', workspace, 'export-verilog'])['stdout'])
            proof = external_cec(harness, 'equivalence', yosys, Path(profile['liberty']['path']),
                                 expected, exported, expected=not bad)
            control = None if bad else external_cec(harness, 'wrong_output', yosys,
                                                     Path(profile['liberty']['path']), wrong,
                                                     exported, expected=False)
            row = dict(pdk=tag, variant=variant, status='passed', native_pin_census=geometry,
                       equivalence=proof, negative_control=control,
                       policy_sha256=sha256(config_path), source_sha256=sha256(source),
                       exported_sha256=sha256(exported), receipts=harness.receipts)
            rows.append(row)
            (out / 'progress.json').write_text(json.dumps(rows, indent=2) + '\n')
            print(f'{tag}/{variant}: passed', flush=True)
    result = dict(schema='rustroad.input-buffer-verification.v1', status='passed',
                  scope='bounded native import and functional equivalence; physical signoff separate',
                  rustroad=dict(path=str(road), sha256=sha256(road)),
                  yosys=dict(path=str(yosys), sha256=sha256(yosys)), cases=rows)
    (out / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
