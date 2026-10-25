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
        tag = profile['tag']
        folder = out / tag
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
        source.write_text("module lane_smoke(output [16:0] y);\nwire zero;\nassign zero = 1'b0;\n" + cells + '\nendmodule\n')
        expected, wrong = folder / 'expected.v', folder / 'wrong.v'
        expected.write_text("module lane_smoke(output [16:0] y); assign y=17'b0; endmodule\n")
        wrong.write_text("module lane_smoke(output [16:0] y); assign y=17'b1; endmodule\n")
        workspace = folder / 'workspace'
        run_ok(harness, 'import', [road, '--workspace', workspace, 'import-pdk', '--base-rev', 'rev0',
                                 '--tech-lef', profile['technology_lef']['path'], '--cell-lef', profile['cell_lef']['path'],
                                 '--netlist', source, '--floorplan', floorplan, '--physical-config', config_path])
        revision = (workspace / 'HEAD').read_text().strip()
        metadata = json.loads((workspace / 'revisions' / revision / 'design.json').read_text())
        count = metadata['physical']['ties']['new_cells']
        if count != 3:
            raise RuntimeError(f'{tag}: expected 3 constant sources, got {count}')
        export = run_ok(harness, 'export', [road, '--workspace', workspace, 'export-verilog'])
        netlist = Path(export['stdout'])
        proof = external_cec(harness, 'constant_cec', yosys, Path(profile['liberty']['path']), expected, netlist)
        control = external_cec(harness, 'wrong_output_control', yosys, Path(profile['liberty']['path']), wrong, netlist, expected=False)
        rows.append(dict(pdk=tag, status='passed', constant_sources=count, max_fanout=8, proof=proof,
                         negative_control=control, receipts=harness.receipts))
    result = dict(schema='rustroad.tie-fanout-verification.v1', status='passed',
                  rustroad=dict(path=str(road), sha256=sha256(road)),
                  yosys=dict(path=str(yosys), sha256=sha256(yosys)), pdks=rows)
    (out / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(dict(status=result['status'], pdks=[r['pdk'] for r in rows], summary=str(out / 'summary.json'))))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
