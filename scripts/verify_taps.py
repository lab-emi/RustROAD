#!/usr/bin/env python3
"""Bounded dual-PDK tap insertion, OpenDB and independent GDS verification."""
import argparse
import copy
import json
import os
from pathlib import Path
import resource
import shutil
import sys
import time
from collections import Counter, defaultdict
from verify_power_io import (run_power_case, check_power_geometry, require,
                             capture_power_db, semantic_simulation, verify_powered_netlist)
from verify_physical import PhysicalHarness, run_ok, fingerprint, external_cec, compare_sta, IMAGE, OPENROAD
from verify_logic import bounded_text, sha256, write_json, yosys_path, sta_flow


def gds_check(h, tag, profile, data, tools, dbu):
    output = h.root / (tag + '_gds_taps.json')
    request = h.root / (tag + '_tap_request.json')
    write_json(request, dict(profile=profile, geometry=data, dbu=dbu, output=str(output)))
    os.environ['RUSTROAD_TAP_REQUEST'] = str(request)
    run_ok(h, tag + '_gds_taps', [tools['klayout'], '-b', '-r', Path(__file__).with_name('check_tap_gds.py').resolve()])
    result = json.loads(bounded_text(output, 2 * 1024**2))
    require(result['status'] == 'passed', 'GDS tap verification failed')
    return dict(result=result, evidence=fingerprint(output))


def clock_function_proof(h, tag, cfg, liberty, yosys):
    """Prove the actual Liberty buffer truth table, including a wrong-polarity control."""
    receipts = []
    prefix = (f'read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(liberty)}\n'
              f'hierarchy -top {cfg["cell"]}\nproc\nflatten\n')
    for value in (0, 1):
        label = f'{tag}_clock_function_{value}'
        script = h.root / (label + '.ys')
        script.write_text(prefix + f'sat -set {cfg["input_pin"]} {value} -prove {cfg["output_pin"]} {value} -verify\n')
        result = run_ok(h, label, [yosys, '-Q', '-T', '-s', script])
        require('SAT proof finished - no model found: SUCCESS!' in bounded_text(Path(result['stdout'])), 'buffer function not proven')
        receipts.append(label)
    label = tag + '_clock_function_negative'
    script = h.root / (label + '.ys')
    script.write_text(prefix + f'sat -set {cfg["input_pin"]} 0 -prove {cfg["output_pin"]} 1 -verify\n')
    result = h.run(label, list(map(str, [yosys, '-Q', '-T', '-s', script])))
    require(result['exit_code'] != 0 and 'FAIL' in bounded_text(Path(result['stdout'])), 'wrong buffer polarity control accepted')
    return dict(status='proved', master=cfg['cell'], truth_table=[0, 1], wrong_polarity='rejected', receipts=receipts)


def check_clock_graph(before, after, cfg):
    """Trace unique drivers backwards from OpenDB sinks; no native tree report input."""
    require(len(after['instances']) <= 2000, 'clock checker instance budget')
    old_names = {i['name'] for i in before['instances']}
    masters = {cfg['cell'], *cfg.get('level_cells', [])}
    buffers = {i['name']: i for i in after['instances'] if i['name'] not in old_names and i['master'] in masters}
    require(0 < len(buffers) <= cfg['max_buffers'], 'clock checker buffer budget')
    source = [p for p in after['io'] if p['name'] == cfg['port'] and p['direction'] == 'input']
    old_source = [p for p in before['io'] if p['name'] == cfg['port'] and p['direction'] == 'input']
    require(len(source) == len(old_source) == 1, 'clock source port missing')
    expected = {(i['name'], p['name']) for i in before['instances'] for p in i['pins']
                if p['net'] == old_source[0]['net'] and p['direction'] == 'input'}
    require(expected and all(pin == cfg['sink_pin'] for _, pin in expected), 'unexpected original clock sinks')
    instances = {i['name']: i for i in after['instances']}
    drivers, receivers = defaultdict(list), defaultdict(list)
    work = 0
    for p in after['io']:
        if p['use'] not in ('power', 'ground'):
            (drivers if p['direction'] == 'input' else receivers)[p['net']].append(('port', p['name'], ''))
    for instance in after['instances']:
        for pin in instance['pins']:
            work += 1
            require(work <= cfg['max_work'], 'clock checker work budget')
            if pin['net'] is not None and pin['use'] not in ('power', 'ground'):
                require(pin['direction'] in ('input', 'output'), 'unsupported clock checker direction')
                (drivers if pin['direction'] == 'output' else receivers)[pin['net']].append(('inst', instance['name'], pin['name']))
    seen_buffers, clock_nets, depths = set(), set(), []
    for name, pin_name in sorted(expected):
        require(name in instances, 'clock sink removed')
        pins = [p for p in instances[name]['pins'] if p['name'] == pin_name]
        require(len(pins) == 1 and pins[0]['net'] is not None, 'clock sink disconnected')
        net, depth, path = pins[0]['net'], 0, set()
        while True:
            work += 1
            require(work <= cfg['max_work'] and depth <= 32, 'clock checker trace budget')
            require(net not in path, 'clock cycle')
            path.add(net)
            clock_nets.add(net)
            require(0 < len(receivers[net]) <= cfg['max_fanout'], 'clock net fanout')
            require(len(drivers[net]) == 1, 'clock net has missing/multiple drivers')
            driver = drivers[net][0]
            if driver == ('port', cfg['port'], ''):
                require(net == source[0]['net'], 'clock source mismatch')
                break
            require(driver[0] == 'inst' and driver[1] in buffers and driver[2] == cfg['output_pin'], 'clock driven by unexpected cell/pin')
            seen_buffers.add(driver[1])
            inputs = [p for p in buffers[driver[1]]['pins'] if p['name'] == cfg['input_pin']]
            require(len(inputs) == 1 and inputs[0]['net'] is not None, 'clock buffer input disconnected')
            net, depth = inputs[0]['net'], depth + 1
        depths.append(depth)
    require(len(set(depths)) == 1 and depths[0] > 0, 'clock leaves have unequal or zero buffer depth')
    require(seen_buffers == set(buffers), 'unused/disconnected clock buffer')
    allowed = {('inst', name, pin) for name, pin in expected} | {('inst', name, cfg['input_pin']) for name in buffers}
    require(all(endpoint in allowed for net in clock_nets for endpoint in receivers[net]), 'unexpected endpoint on clock tree')
    return dict(status='passed', sinks=len(expected), buffers=len(buffers), levels=depths[0],
                actual_max_fanout=max(len(receivers[n]) for n in clock_nets),
                depth_histogram=dict(Counter(depths)), work_units=work, wire_model='none',
                scope='logical topology from independent OpenDB endpoints; no routed skew claim')


def run_clock_case(h, profile, tools, before_result, before):
    tag = profile['tag'] + '_clock'
    cfg = json.loads(Path(profile['config']).read_text())['clock']
    liberty = Path(profile['liberty']['path'])
    proof = clock_function_proof(h, tag, cfg, liberty, tools['yosys'])
    road = [tools['rustroad'], '--workspace', h.root / (profile['tag'] + '_physical'),
            '--threads', '2', '--max-cells', '2000', '--max-rows', '128']
    placed = h.json_command(tag + '_insert', list(map(str, road + ['clock-tree', '--base-rev', before_result['placed']['revision']])))
    stale = h.run(tag + '_stale', list(map(str, road + ['clock-tree', '--base-rev', before_result['placed']['revision']])))
    require(stale['exit_code'] != 0 and 'REVISION_CONFLICT' in bounded_text(Path(stale['stderr'])), 'stale clock mutation accepted')
    exported, definition, powered = [h.root / (tag + suffix) for suffix in ('.v', '.def', '_powered.v')]
    for label, command, path in [('logical', ['export-verilog'], exported), ('def', ['export-def'], definition), ('powered', ['export-verilog', '--include-power'], powered)]:
        receipt = run_ok(h, tag + '_export_' + label, road + command)
        path.write_bytes(Path(receipt['stdout']).read_bytes())
    repeated = h.json_command(tag + '_replace', list(map(str, road + ['place', '--base-rev', placed['revision']])))
    repeated_def = run_ok(h, tag + '_replace_export', road + ['export-def'])
    require(sha256(Path(repeated_def['stdout'])) == sha256(definition), 'post-clock placement is not deterministic')
    rtl = Path(before_result['artifacts']['source']['path'])
    cec = external_cec(h, tag + '_cec', tools['yosys'], liberty, rtl, exported)
    simulation = semantic_simulation(h, tag, tools, profile, rtl, exported)
    data = capture_power_db(h, tag, profile, definition)
    for key in ('expected_rows', 'site_width', 'site_height'):
        data[key] = before[key]
    write_json(h.root / (tag + '_power_geometry.json'), data)
    graph = check_clock_graph(before, data, cfg)
    controls = {}
    for defect in ('bypass_sink', 'disconnect_buffer'):
        bad = copy.deepcopy(data)
        root = next(p['net'] for p in bad['io'] if p['name'] == cfg['port'])
        if defect == 'bypass_sink':
            sink = next(p for i in bad['instances'] for p in i['pins'] if p['name'] == cfg['sink_pin'] and p['direction'] == 'input')
            sink['net'] = root
        else:
            old_names = {i['name'] for i in before['instances']}
            buffer = next(i for i in bad['instances'] if i['master'] == cfg['cell'] and i['name'] not in old_names)
            next(p for p in buffer['pins'] if p['name'] == cfg['output_pin'])['net'] = None
        try:
            check_clock_graph(before, bad, cfg)
        except (RuntimeError, ValueError):
            controls[defect] = 'rejected'
        else:
            raise RuntimeError('clock defect control accepted: ' + defect)
    geometry = check_power_geometry(data, profile)
    powered_topology = verify_powered_netlist(h, tag, tools['yosys'], powered, data)
    floor = json.loads(Path(before_result['artifacts']['floorplan']['path']).read_text())
    gaps = spacing(data, profile)
    gds = gds_check(h, tag, profile, data, tools, floor['units_per_micron'])
    projected, sdc = h.root / (profile['tag'] + '_timing_ideal.lib'), h.root / (profile['tag'] + '_timing.sdc')
    timing = sta_flow(h, tag + '_timing', tools, projected, exported, sdc)
    report = json.loads(bounded_text(h.root / 'receipts' / (tag + '_timing_report') / 'stdout.txt'))
    uncovered = report['sections']['unconstrained']['rows']
    require({(p['pin'], p['reason']) for p in uncovered} == {(p['pin'], p['reason']) for p in before_result['timing']['unconstrained']},
            'post-clock unexpected unconstrained endpoints')
    differential = compare_sta(h, tag + '_opensta', tools['opensta'], projected, exported, sdc, timing)
    return dict(status='passed', native=placed, repeated_place=dict(revision=repeated['revision'], def_sha256=sha256(definition), status='identical'), buffer_function=proof, graph=graph, controls=controls,
                stale_revision='rejected', external_cec=cec, simulation=simulation, power_geometry=geometry,
                powered_topology=powered_topology, tap_rows=gaps, gds=gds,
                timing=dict(native=timing, opensta=differential, unconstrained=uncovered,
                            clock_model='ideal', wire_model='ideal', parasitics='none'),
                artifacts={name: fingerprint(path) for name, path in [('verilog', exported), ('powered', powered), ('def', definition), ('geometry', h.root / (tag + '_power_geometry.json'))]},
                routed_cts='not_run', skew='unverified')


def spacing(data, profile):
    cfg = json.loads(Path(profile['config']).read_text())['tap']
    master = cfg['cell']
    rows = data['expected_rows']
    result = []
    for row in rows:
        taps = sorted([i for i in data['instances'] if i['master'] == master and i['y'] == row['y']], key=lambda i:i['x'])
        require(taps, 'row without tap')
        require(taps[0]['x'] == row['x_start'] and taps[-1]['x']+taps[-1]['width'] == row['x_end'], 'untapped row boundary')
        gaps=[b['x']-a['x']-a['width'] for a,b in zip(taps,taps[1:])]
        require(all(0<=g<=cfg['max_gap'] for g in gaps), 'tap spacing exceeds configured policy')
        result.append(dict(y=row['y'],taps=len(taps),max_gap=max(gaps,default=0)))
    return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace',type=Path,required=True)
    parser.add_argument('--base-rev',required=True)
    parser.add_argument('--profiles',type=Path,default=Path('fixtures/pdk/local-taps.json'))
    parser.add_argument('--tool-profile',type=Path,default=Path('docs/evidence/power-io-results.json'))
    parser.add_argument('--rustroad',type=Path,default=Path('target/release/rustroad'))
    args=parser.parse_args()
    started = time.monotonic()
    require(args.base_rev=='empty','REVISION_CONFLICT: fresh evidence workspace requires empty')
    root=args.workspace.resolve();root.mkdir(parents=True,exist_ok=False)
    import fcntl
    lock=(root/'workspace.lock').open('wb');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    (root/'HEAD').write_text('rev0\n')
    resource.setrlimit(resource.RLIMIT_AS,(8192*1024**2,8192*1024**2))
    os.environ.update(GOMAXPROCS='2',QT_QPA_PLATFORM='offscreen',OMP_NUM_THREADS='2')
    args.threads,args.memory_mib,args.timeout_seconds,args.tolerance_ns=2,2048,30,0.001
    h=PhysicalHarness(root,args)
    profiles=json.loads(bounded_text(args.profiles,1024**2))
    require(profiles['schema'] == 'rustroad.tap-profile.v1' and {p['tag'] for p in profiles['pdks']} == {'sky130', 'gf180'}, 'two baseline PDKs required')
    old_report=json.loads(bounded_text(args.tool_profile,2*1024**2))
    old=old_report['tools']
    tools={name:Path(old[name]['path']).resolve() for name in ['rustsys','yosys','verilator','rusteda','ruststa','opensta']}
    tools['rustroad']=args.rustroad.resolve();tools['klayout']=Path(shutil.which('klayout') or '/missing/klayout')
    summary=dict(schema='rustroad.tap-verification.v1',status='unverified',pdks=[],tools={},source=fingerprint(Path(__file__).resolve()),
                 gds_checker=fingerprint(Path(__file__).with_name('check_tap_gds.py').resolve()),profiles=fingerprint(args.profiles.resolve()),
                 helper_sources=[fingerprint(Path(__file__).with_name(name).resolve()) for name in ('verify_power_io.py', 'verify_physical.py', 'verify_logic.py')],
                 scope={'latchup_signoff':'unchecked','drc':'unchecked','lvs':'unchecked','cts':'unverified','routing':'not_run','extraction':'not_run'})
    try:
        memory=run_ok(h,'memory_preflight',['free','-h'])
        summary['memory_preflight']=bounded_text(Path(memory['stdout']))
        for name,path in tools.items():
            require(path.is_file(),'missing tool '+name)
            if name not in ['rustroad','klayout']:require(sha256(path)==old[name]['sha256'],'frozen reference changed: '+name)
            receipt=run_ok(h,name+'_version',[path,'-V' if name=='yosys' else '-version' if name=='opensta' else '-v' if name=='klayout' else '--version'])
            summary['tools'][name]={**fingerprint(path),'version':bounded_text(Path(receipt['stdout'])).strip()}
        run_ok(h,'openroad_image',['podman','image','exists',IMAGE])
        binary=run_ok(h,'openroad_binary',['podman','run','--rm','--pull=never','--network=none','--read-only',
            '--memory=2g','--memory-swap=2g','--cpus=2','--pids-limit=128','--entrypoint=/usr/bin/sha256sum',IMAGE,OPENROAD])
        digest=bounded_text(Path(binary['stdout'])).split()[0]
        require(digest==old_report['openroad_reference']['sha256'],'pinned OpenROAD binary changed')
        summary['openroad_reference']=dict(image_id=IMAGE,executable=OPENROAD,sha256=digest)
        for profile in profiles['pdks']:
            tag=profile['tag']
            try:
                for pin in profile['tap_reference'].values():
                    if isinstance(pin,dict) and 'sha256'in pin:require(sha256(Path(pin['path']))==pin['sha256'],'tap reference data changed')
                result=run_power_case(h,profile,tools)
                require(result['status']=='passed','power/logic/timing predecessor did not pass')
                data=json.loads(bounded_text(Path(result['artifacts']['geometry']['path'])))
                cfg=json.loads(Path(profile['config']).read_text())
                # capture helper writes the expected rows into this standard data file.
                data['expected_rows']=json.loads(Path(result['artifacts']['floorplan']['path']).read_text())['rows']
                gaps=spacing(data,profile)
                negative=copy.deepcopy(data)
                first=next(i for i in negative['instances'] if i['master']==cfg['tap']['cell'])
                next(p for p in first['pins'] if p['name']==cfg['power_pin'])['net']=cfg['ground_net']
                try:check_power_geometry(negative,profile)
                except (RuntimeError,ValueError):wrong_net='rejected'
                else:raise RuntimeError('wrong tap supply control accepted')
                floor=json.loads(Path(result['artifacts']['floorplan']['path']).read_text())
                gds=gds_check(h,tag,profile,data,tools,floor['units_per_micron'])
                result['tap_verification']={'status':'passed','rows':gaps,'wrong_supply_control':wrong_net,'gds':gds['result'],'evidence':gds['evidence']}
                result['clock_tree']=run_clock_case(h,profile,tools,result,data)
                for pin in profile['tap_reference'].values():
                    if isinstance(pin,dict) and 'sha256'in pin:require(sha256(Path(pin['path']))==pin['sha256'],'tap reference changed during run')
                for key in ('technology_lef','cell_lef','liberty'):
                    require(sha256(Path(profile[key]['path']))==profile[key]['sha256'],'PDK changed during clock validation')
                require(sha256(Path(profile['config']))==result['artifacts']['config']['sha256'],'physical config changed during validation')
                summary['pdks'].append(result)
            except (RuntimeError,ValueError,KeyError,OSError) as error:
                summary['pdks'].append(dict(pdk=tag,status='failed',reason=str(error)))
        summary['status']='passed' if len(summary['pdks'])==2 and all(p['status']=='passed' for p in summary['pdks']) else 'failed'
        if summary['status'] == 'passed':
            summary['scope']['cts']='unrouted_topology_checked'
    except (RuntimeError,ValueError,KeyError,OSError) as error:
        summary['reason']=str(error)
    finally:
        summary['identity_stable']=all(sha256(Path(t['path']))==t['sha256'] for t in summary['tools'].values())
        if not summary['identity_stable']:summary['status']='failed'
        summary['receipts']=h.receipts
        summary['peak_rss_kib']=max((r['peak_rss_kib'] for r in h.receipts if r['peak_rss_kib']is not None),default=None)
        summary['wall_seconds']=time.monotonic()-started
        write_json(root/'summary.json',summary);(root/'HEAD').write_text('rev1\n')
    print(json.dumps(dict(status=summary['status'],summary=str(root/'summary.json'))))
    return 0 if summary['status']=='passed'else 3 if summary['status']=='unverified'else 1


if __name__=='__main__':sys.exit(main())
