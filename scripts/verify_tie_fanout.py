#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Independent dual-PDK CEC for native constant-source replication.

Seventeen buffer inputs share one constant source, with a maximum physical
fanout of eight. The imported netlist must contain three constant sources;
black-box Yosys proves all outputs are zero and rejects a wrong-output control.
Each process uses the existing 1 GiB / 120-second verification guard. No
reference-tool implementation is imported or inspected.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from verify_physical import PhysicalHarness, external_cec, run_ok
from verify_logic import sha256

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rustroad', type=Path, required=True)
    parser.add_argument('--yosys', type=Path, default=ROOT / '.work/tools/yosys')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    road, yosys = args.rustroad.resolve(), args.yosys.resolve()
    profiles = json.loads((ROOT / 'fixtures/pdk/local-routing.json').read_text())['pdks']
    rows = []
    for profile in profiles:
        for variant in ['literal', 'reused', 'ports', 'ports-up']:
            tag = profile['tag']
            folder = out / (tag + '-' + variant)
            folder.mkdir()
            harness = PhysicalHarness(folder, argparse.Namespace(threads=2, memory_mib=1024, timeout_seconds=120))
            for role in ['technology_lef', 'cell_lef', 'liberty']:
                pin = profile[role]
                if sha256(Path(pin['path'])) != pin['sha256']:
                    raise RuntimeError(f'{tag}: {role} identity mismatch')
            config = json.loads((ROOT / profile['config']).read_text())
            buffer = config['clock']
            config.update(clock=None, max_tie_fanout=8, max_added_cells=1000)
            config_path = folder / 'physical.json'
            config_path.write_text(json.dumps(config, indent=2) + '\n')
            floorplan = ROOT / 'fixtures/designs/pipeline' / f'{tag}-floorplan.json'
            source = folder / 'mapped.v'
            cells = '\n'.join(f"  {buffer['cell']} b{i} (.{buffer['input_pin']}(zero), .{buffer['output_pin']}(y[{i}]));" for i in range(17))
            if variant == 'literal':
                driver = "assign zero = 1'b0;"
            else:
                low = config['tie_low']
                connections = [f".{low['pin']}(zero)"]
                if low.get('rail_input'):
                    rail = low['rail_input']
                    literal = "1'b1" if rail['supply'] == 'power' else "1'b0"
                    connections.append(f".{rail['pin']}({literal})")
                driver = f"{low['cell']} original_source ({', '.join(connections)});"
            expected, wrong = folder / 'expected.v', folder / 'wrong.v'
            if variant.startswith('ports'):
                bus = '[0:16]' if variant == 'ports-up' else '[16:0]'
                fields = ['zero'] * 17
                fields[5] = 'a'
                source.write_text(f"module lane_smoke(input a, output {bus} y); wire zero; assign zero=1'b0; assign y={{{','.join(fields)}}}; endmodule\n")
                fields = ["1'b0"] * 17
                fields[5] = 'a'
                expected.write_text(f"module lane_smoke(input a, output {bus} y); assign y={{{','.join(fields)}}}; endmodule\n")
                fields[0] = "1'b1"
                wrong.write_text(f"module lane_smoke(input a, output {bus} y); assign y={{{','.join(fields)}}}; endmodule\n")
            else:
                source.write_text("module lane_smoke(output [16:0] y);\nwire zero;\n" + driver + '\n' + cells + '\nendmodule\n')
                expected.write_text("module lane_smoke(output [16:0] y); assign y=17'b0; endmodule\n")
                wrong.write_text("module lane_smoke(output [16:0] y); assign y=17'b1; endmodule\n")
            workspace = folder / 'workspace'
            run_ok(harness, 'import', [road, '--workspace', workspace, 'import-pdk', '--base-rev', 'rev0',
                                     '--tech-lef', profile['technology_lef']['path'], '--cell-lef', profile['cell_lef']['path'],
                                     '--netlist', source, '--floorplan', floorplan, '--physical-config', config_path])
            revision = (workspace / 'HEAD').read_text().strip()
            metadata = json.loads((workspace / 'revisions' / revision / 'design.json').read_text())
            count = metadata['physical']['ties']['new_cells']
            expected_new = 3 if variant == 'literal' else 2
            if count != expected_new:
                raise RuntimeError(f'{tag}/{variant}: expected {expected_new} inserted sources, got {count}')
            export = run_ok(harness, 'export', [road, '--workspace', workspace, 'export-verilog'])
            netlist = Path(export['stdout'])
            proof = external_cec(harness, 'constant_cec', yosys, Path(profile['liberty']['path']), expected, netlist)
            control = external_cec(harness, 'wrong_output_control', yosys, Path(profile['liberty']['path']), wrong, netlist, expected=False)
            rows.append(dict(pdk=tag, variant=variant, status='passed', inserted_sources=count, constant_sources=2 if variant.startswith('ports') else 3, max_fanout=8, proof=proof,
                             negative_control=control, receipts=harness.receipts))
    result = dict(schema='rustroad.tie-fanout-verification.v1', status='passed',
                  rustroad=dict(path=str(road), sha256=sha256(road)),
                  yosys=dict(path=str(yosys), sha256=sha256(yosys)), pdks=rows)
    (out / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(dict(status=result['status'], cases=[r['pdk'] + ':' + r['variant'] for r in rows], summary=str(out / 'summary.json'))))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
