#!/usr/bin/env python3
"""Guarded dual-PDK CTS/routing/OpenRCX/SPEF integration. Partial != signoff."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import resource
import sys
import time

from verify_physical import PhysicalHarness, run_ok, fingerprint, external_cec, require, IMAGE, OPENROAD
from verify_logic import bounded_text, sha256, write_json, yosys_path, ideal_projection
from verify_power_io import capture_power_db, check_power_geometry, verify_powered_netlist
from verify_taps import check_clock_graph, clock_function_proof, gds_check
from routing_reference import extract, compare_extracted
import check_cell_policy
import flow_status

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts/signoff'))
import def2gds_reference  # noqa: E402
import signoff as signoff_harness  # noqa: E402


def simulation(h,tag,tools,lib,rtl,netlist):
    script=h.root/(tag+'_sim.ys');model=h.root/(tag+'_gate.v');test=h.root/(tag+'_tb.sv')
    script.write_text(f'read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(lib)}\nread_verilog {yosys_path(netlist)}\nhierarchy -top lane_smoke\nproc\nflatten\nrename lane_smoke gate\nwrite_verilog -noattr {yosys_path(model)}\n')
    run_ok(h,tag+'_sim_model',[tools['yosys'],'-Q','-T','-s',script])
    test.write_text('''module test;
reg clk=0,en=0;reg[1:0] a=0,b=0,previous_q=0;wire[1:0] yg,zg,ym,zm;reg[1:0] want;
lane_smoke gold(.clk(clk),.en(en),.a(a),.b(b),.y(yg),.z(zg));
gate mapped(.clk(clk),.en(en),.a(a),.b(b),.y(ym),.z(zm));
initial begin
for(integer i=0;i<512;i=i+1)begin
a=i[1:0];b=i[3:2];en=i[4];want=previous_q&{2{en}};#1;clk=1;#1;
if((i>0 && (yg!==want||ym!==yg)) || zg!==(a&b) || zm!==zg)begin $display("ROUTE_SIM_FAIL %0d",i);$fatal(1);end
previous_q=a^b;clk=0;#1;
end
$display("ROUTE_SIM_PASS vectors=512 checked_pipeline_cycles=511");$finish;
end endmodule
''')
    build=h.root/(tag+'_sim_build')
    run_ok(h,tag+'_sim_build',[tools['verilator'],'--binary','--timing','-Wno-fatal','-j','2','--top-module','test','--Mdir',build,rtl,model,test])
    r=run_ok(h,tag+'_sim_run',[build/'Vtest'])
    require('ROUTE_SIM_PASS' in bounded_text(Path(r['stdout'])),'pipeline simulation failed')
    return dict(status='passed',vectors=512,checked_pipeline_cycles=511,initial_state='unconstrained; y sampled after two edges')


def sta(h,tag,tools,lib,netlist,sdc,spef):
    ws=h.root/(tag+'_sta');common=['--workspace',str(ws),'--threads','2','--format','json','--color','never']
    a=h.json_command(tag+'_sta_init',[str(tools['rusteda']),*common,'init'],True)
    a=h.json_command(tag+'_sta_import',[str(tools['rusteda']),*common,'import','--base-rev',a['new_rev'],'--lib',str(lib),'--corner','tt','--netlist',str(netlist),'--sdc',str(sdc),'--spef',str(spef),'--spef-corner','tt','--max-rc-nodes','200000','--max-rc-elements','200000'],True)
    r=h.json_command(tag+'_sta_run',[str(tools['ruststa']),*common,'run','--base-rev',a['new_rev'],'--corner','tt','--backend','cpu','--precision','f64','--max-paths','8','--max-state-gib','1'],True)
    report=h.json_command(tag+'_sta_report',[str(tools['ruststa']),*common,'report','--rev',r['new_rev'],'--section','checks','--section','unconstrained','--section','clocks','--limit','128'],True)
    full=h.json_command(tag+'_sta_full',[str(tools['ruststa']),*common,'check','--base-rev',r['new_rev'],'--method','full','--tolerance','1e-9'],True)
    update=h.json_command(tag+'_sta_update',[str(tools['ruststa']),*common,'update','--base-rev',full['new_rev'],'--backend','cpu','--precision','f64','--max-paths','8','--max-state-gib','1'],True)
    check=h.json_command(tag+'_sta_updated_full',[str(tools['ruststa']),*common,'check','--base-rev',update['new_rev'],'--method','full','--tolerance','1e-9'],True)
    require(full.get('verdict')=='agree' and check.get('verdict')=='agree','full/incremental inconsistency')
    require(r['scope'].get('clock_model')=='propagated' and r['scope'].get('wire_model')=='spef_rc' and r['scope'].get('delay_calculator')=='nldm_ceff_ramp','wrong extracted timing scope')
    require(r['completeness']['unconstrained_endpoints']==0,'unconstrained pipeline endpoints')
    return dict(status='passed',scope=r['scope'],metrics=r['metrics'],completeness=r['completeness'],diagnostics=r['diagnostics'],
                endpoints=report['sections']['checks']['rows'],full=full,incremental_noop=check,workspace=str(ws))


def missing_annotation_control(h,tag,tools,lib,netlist,sdc,spef):
    text=bounded_text(spef,32*1024**2)
    bad=h.root/(tag+'_missing.spef')
    text,count=re.subn(r'(?ms)^\*D_NET .*?^\*END\s*\n','',text,count=1)
    require(count==1,'missing-annotation fixture absent');bad.write_text(text)
    common=['--workspace',str(h.root/(tag+'_missing_sta')),'--threads','2','--format','json','--color','never']
    a=h.json_command(tag+'_missing_init',[str(tools['rusteda']),*common,'init'],True)
    a=h.json_command(tag+'_missing_import',[str(tools['rusteda']),*common,'import','--base-rev',a['new_rev'],'--lib',str(lib),'--corner','tt','--netlist',str(netlist),'--sdc',str(sdc),'--spef',str(bad),'--spef-corner','tt'],True)
    r=h.run(tag+'_missing_run',[str(tools['ruststa']),*common,'run','--base-rev',a['new_rev'],'--corner','tt','--backend','cpu','--precision','f64','--max-paths','8','--max-state-gib','1'])
    require(r['exit_code']!=0 and 'missing SPEF annotation' in bounded_text(Path(r['stdout'])),'missing RC silently accepted')
    return 'rejected'


def skew_control(h,tag,opensta,lib,netlist,sdc,spef,native,limit):
    text=bounded_text(spef,32*1024**2)
    mapping=re.search(r'(?m)^\*(\d+) rr_clock_0[ \t]*$',text);require(mapping is not None,'clock branch not in SPEF')
    block=re.search(r'(?ms)^\*D_NET \*'+mapping[1]+r' .*?^\*END',text);require(block is not None,'clock branch RC absent')
    part=block[0];start=part.index('*RES\n')+len('*RES\n')
    resistors=part[start:]
    def scale(m):return m[1]+format(float(m[2])*10000,'.12g')+m[3]
    resistors,count=re.subn(r'(?m)^(\d+ \S+ \S+ )([0-9.eE+\-]+)([ \t]*)$',scale,resistors)
    require(count>0,'clock branch has no R to perturb')
    changed=part[:start]+resistors;bad=h.root/(tag+'_skew_bad.spef');bad.write_text(text[:block.start()]+changed+text[block.end():])
    result=compare_extracted(h,tag+'_skew_negative',opensta,lib,netlist,sdc,bad,native,limit)
    require(result['skew_status']=='failed','large clock resistance did not fail skew limit')
    return dict(status='rejected',skew_ns=result['clock_skew_rise_fall_ns'],resistance_scale=10000)


def run_signoff(h,tag,args,definition,powered):
    """Foundry signoff statuses for this PDK (black-box tools in the pinned image).

    gds: passed only for a native GDS (--signoff-gds) that XOR-matches the
    reference KLayout streamout of the same DEF. DRC/antenna/LVS come from the
    native GDS when given, otherwise from the reference streamout when
    --signoff-reference is set (evidence_gds says which), otherwise missing.
    """
    missing=lambda why:flow_status.item('missing',why)
    out=dict(statuses=dict(gds=missing('no native GDS supplied (--signoff-gds); the native writer is another stream'),
             **{k:missing('signoff not run (use --signoff-reference or --signoff-gds)') for k in ('drc_magic','drc_klayout','antenna','lvs_netgen','lvs_klayout')},
             density=missing('signoff not run')),evidence_gds=None)
    native=dict(x.split('=',1) for x in args.signoff_gds).get(tag)
    if not args.signoff_reference and native is None:return out
    ref=h.root/(tag+'_reference.gds')
    receipt=def2gds_reference.stream(definition,tag,ref,args.manifest)
    out['reference_streamout']=dict(status=receipt['status'],problems=receipt['problems'],output=receipt.get('output'),receipt=str(ref)+'.json')
    if receipt['status']!='passed':
        for k in ('drc_magic','drc_klayout','antenna','lvs_netgen','lvs_klayout','density'):out['statuses'][k]=flow_status.item('unverified','reference streamout failed')
        return out
    gds=Path(native).resolve() if native else ref
    ns=argparse.Namespace(gds=gds,top='lane_smoke',pdk=tag,out=h.root/(tag+'_signoff'),netlist=powered,against=ref if native else None,
                          against_top='lane_smoke' if native else None,manifest=args.manifest,threads=2,memory='4g',timeout=900,plant=None)
    checks=list(signoff_harness.CHECKS)
    if not native:checks.remove('xor')
    summary=signoff_harness.run_checks(ns,checks)
    (ns.out/'signoff.json').write_text(json.dumps(summary,indent=2)+'\n')
    out.update(evidence_gds='native' if native else 'reference_klayout_streamout',summary=str(ns.out/'signoff.json'),
               counts=summary['counts'],by_rule=summary['by_rule'],peak_rss_kib=summary['resources']['peak_rss_kib'])
    for key,value in summary['statuses'].items():
        if key=='xor':continue
        out['statuses'][key]=flow_status.item(value,None if value=='passed' else 'see '+summary['verdicts'][key.replace('_','-')],
                                             evidence_gds=out['evidence_gds'],count=summary['counts'].get(key.replace('_','-')))
    if native:
        x=summary['statuses'].get('xor')
        out['statuses']['gds']=flow_status.item('passed' if x=='passed' else 'failed' if x=='failed' else 'unverified',
                                               'native GDS XOR vs reference streamout: '+str(x),native=str(gds))
    return out


def case(h,p,tools,opts,policy):
    tag=p['tag'];lib=Path(p['liberty']['path']);rtl=ROOT/'fixtures/flow/pipeline.v';config=Path(p['config'])
    for key in ('technology_lef','cell_lef','liberty','extraction_rules'):
        require(sha256(Path(p[key]['path']))==p[key]['sha256'],'PDK identity mismatch '+key)
    mapped=h.root/(tag+'_mapped.v')
    dont_use=policy['pdks'][tag]['rustsys_args'] if policy is not None else []
    r=run_ok(h,tag+'_synth',[tools['rustsys'],'--threads','2','--progress','off','synth','--backend','cpu','--top','lane_smoke','--lib',lib,*dont_use,'--out',mapped,'--print','json',rtl])
    synth_report=json.loads(bounded_text(Path(r['stdout'])))
    cell_checks={'mapped':check_cell_policy.check(tag,'synth',[mapped],None,opts.cell_policy if policy is not None else None,None)}
    sky=tag=='sky130';u,sw,sh=(1000,460,2720) if sky else (2000,1120,7840)
    floor=h.root/(tag+'_floor.json')
    write_json(floor,dict(name='lane_smoke',units_per_micron=u,die_area=dict(lx=0,ly=0,ux=125*sw,uy=18*sh),core_area=dict(lx=20*sw,ly=5*sh,ux=120*sw,uy=13*sh),site=dict(name='unithd' if sky else 'GF018hv5v_mcu_sc7',width=sw,height=sh),rows=[dict(x_start=20*sw,x_end=120*sw,y=(5+i)*sh)for i in range(8)]))
    road=[tools['rustroad'],'--workspace',h.root/(tag+'_physical'),'--threads','2','--max-cells','2000','--max-rows','128']
    a=h.json_command(tag+'_import',list(map(str,road+['import-pdk','--base-rev','rev0','--tech-lef',p['technology_lef']['path'],'--cell-lef',p['cell_lef']['path'],'--netlist',mapped,'--floorplan',floor,'--physical-config',config])))
    a=h.json_command(tag+'_place',list(map(str,road+['place','--base-rev',a['revision']])))
    before_def=h.root/(tag+'_before.def');r=run_ok(h,tag+'_before_def',road+['export-def']);before_def.write_bytes(Path(r['stdout']).read_bytes())
    before=capture_power_db(h,tag+'_before',p,before_def)
    cfg=json.loads(config.read_text());proof=clock_function_proof(h,tag,cfg['clock'],lib,tools['yosys'])
    for k,extra in enumerate(c for c in cfg['clock'].get('level_cells',[]) if c!=cfg['clock']['cell']):proof.setdefault('level_masters',[]).append(clock_function_proof(h,f'{tag}_level{k}',dict(cfg['clock'],cell=extra),lib,tools['yosys']))
    a=h.json_command(tag+'_cts',list(map(str,road+['clock-tree','--base-rev',a['revision'],'--sdc',p['cts_sdc']])))
    cts=a
    clock=h.json_command(tag+'_clock_route',list(map(str,road+['route','--base-rev',a['revision'],'--scope','clock','--config',p['routing_config']])))
    a=h.json_command(tag+'_all_route',list(map(str,road+['route','--base-rev',clock['revision'],'--scope','all','--config',p['routing_config']])))
    definition,netlist,powered=[h.root/(tag+s) for s in ('.def','_exported.v','_powered.v')]
    for label,args,path in [('def',['export-def'],definition),('netlist',['export-verilog'],netlist),('powered',['export-verilog','--include-power'],powered)]:
        r=run_ok(h,tag+'_export_'+label,road+args);path.write_bytes(Path(r['stdout']).read_bytes())
    cell_checks['physical']=check_cell_policy.check(tag,'physical',[netlist,powered],mapped,opts.cell_policy if policy is not None else None,None)
    route_data=run_ok(h,tag+'_export_routes',road+['export-routes'])
    route_json=json.loads(bounded_text(Path(route_data['stdout'])))
    repeat=h.json_command(tag+'_reroute',list(map(str,road+['route','--base-rev',a['revision'],'--scope','all','--config',p['routing_config']])))
    r=run_ok(h,tag+'_repeat_def',road+['export-def']);require(sha256(Path(r['stdout']))==sha256(definition),'non-deterministic repeated routing')
    cec=external_cec(h,tag+'_cec',tools['yosys'],lib,rtl,netlist)
    bad=h.root/(tag+'_bad.sv');bad.write_text(rtl.read_text().replace('assign z = a & b;','assign z = ~(a & b);'))
    external_cec(h,tag+'_cec_negative',tools['yosys'],lib,bad,netlist,expected=False)
    native=h.json_command(tag+'_native_cec',list(map(str,[tools['rustsys'],'--threads','2','--progress','off','check','synth','--top','lane_smoke','--lib',lib,'--netlist',netlist,'--budget','10000','--model-budget','10000','--sweep-max-candidates','256','--print','json',rtl])))
    require(native.get('verdict')=='equivalent','native CEC failed')
    sim=simulation(h,tag,tools,lib,rtl,netlist)
    data=capture_power_db(h,tag+'_final',p,definition)
    d=json.loads(floor.read_text());data.update(expected_rows=d['rows'],site_width=sw,site_height=sh)
    power=check_power_geometry(data,p);topology=verify_powered_netlist(h,tag,tools['yosys'],powered,data)
    graph=check_clock_graph(before,data,cfg['clock']);gds=gds_check(h,tag,p,data,tools,u)
    routes,spef,reference=extract(h,tag,p,definition)
    request=h.root/(tag+'_routing_request.json');geometry_output=h.root/(tag+'_routing_check.json')
    write_json(request,dict(geometry=data,routes=routes,profile=p,scope='all',output=str(geometry_output)))
    os.environ['RUSTROAD_ROUTING_REQUEST']=str(request)
    run_ok(h,tag+'_geometry',[tools['klayout'],'-b','-r',ROOT/'scripts/check_routing_geometry.py'])
    geometry=json.loads(bounded_text(geometry_output))
    projected=h.root/(tag+'_ideal.lib');projection=ideal_projection(lib,projected)
    sdc=h.root/(tag+'_postroute.sdc');sdc.write_text(Path(p['cts_sdc']).read_text()+'''set_input_delay 0.2 -clock core [get_ports {en a b}]
set_output_delay 0.2 -clock core [get_ports {y z}]
set_input_transition 0.1 [get_ports {clk en a b}]
set_load 0.01 [get_ports {y z}]
set_propagated_clock [get_clocks core]
''')
    timing=sta(h,tag,tools,projected,netlist,sdc,spef)
    limit=json.loads(Path(p['routing_config']).read_text())['max_skew_ns']
    diff=compare_extracted(h,tag+'_opensta',tools['opensta'],projected,netlist,sdc,spef,timing,limit)
    require(diff['clock_sink_count']==graph['sinks'] and diff['skew_status']=='passed','clock skew requirement not met')
    missing=missing_annotation_control(h,tag,tools,projected,netlist,sdc,spef)
    skew_bad=skew_control(h,tag,tools['opensta'],projected,netlist,sdc,spef,timing,limit)
    text=bounded_text(spef,32*1024**2);nets=re.findall(r'^\*D_NET (\S+) (\S+)',text,re.M)
    require(len(nets)==geometry['routed_nets'] and all(float(v)>0 for _,v in nets),'SPEF coverage/positive capacitance failed')
    for key in ('technology_lef','cell_lef','liberty','extraction_rules'):
        require(sha256(Path(p[key]['path']))==p[key]['sha256'],'PDK changed during run')
    signoff=run_signoff(h,tag,opts,definition,powered)
    tt_ok=timing['metrics']['violating_endpoints']==0 and timing['metrics']['hold_violating_endpoints']==0 and diff['status']=='passed'
    statuses=dict(signoff['statuses'])
    statuses.update(
        timing_tt_nom=flow_status.item('passed' if tt_ok else 'failed','reported only (owner decision f); RustSTA vs OpenSTA parity '+diff['status'],
                                       setup_violations=timing['metrics']['violating_endpoints'],hold_violations=timing['metrics']['hold_violating_endpoints'],
                                       worst_setup_ns=timing['metrics'].get('worst_setup_slack_ns'),worst_hold_ns=timing['metrics'].get('worst_hold_slack_ns')),
        timing_ss_max_setup=flow_status.item('missing','ss corner (ss Liberty + max RC rules) not run by this harness'),
        timing_ss_max_hold=flow_status.item('missing','ss corner (ss Liberty + max RC rules) not run by this harness'),
        timing_ff_min_hold=flow_status.item('missing','ff corner (ff Liberty + min RC rules) not run by this harness'),
        mpw_min_period=flow_status.mpw_status(timing.get('completeness')),
        drv=flow_status.drv_status(diff.get('transition_capacitance_fanout_status'),['tt_nom']),
        cell_policy=flow_status.item('passed' if policy is not None and all(c['status']=='passed' for c in cell_checks.values()) else
                                     'failed' if any(c['status']=='failed' for c in cell_checks.values()) else 'unchecked',
                                     None if policy is not None else 'historical frozen producer run without --dont-use',
                                     checks={k:dict(status=c['status'],violations=c['violations'][:10]) for k,c in cell_checks.items()}))
    return dict(pdk=tag,status='passed',signoff_statuses=statuses,signoff=signoff,cell_policy=cell_checks,synth_report=dict(area=synth_report.get('area'),cells=synth_report.get('cells')),cts=cts,placed_routed=a,clock_route=clock,repeated_route=repeat['revision'],clock_graph=graph,buffer_function=proof,
        cec=cec,native_cec=native['verdict'],simulation=sim,power=power,powered_topology=topology,taps=gds,geometry=geometry,
        routes=route_json,extraction=dict(status='passed',provider='external OpenRCX',corner='nominal',nets=len(nets),rules=p['extraction_rules'],spef=fingerprint(spef),reference_geometry=reference,measurement=routes['measurement']),
        timing=timing,opensta=diff,missing_annotation_control=missing,skew_control=skew_bad,projection=projection,artifacts={name:fingerprint(path)for name,path in [('rtl',rtl),('mapped',mapped),('netlist',netlist),('powered',powered),('def',definition),('floor',floor),('sdc',sdc),('geometry',geometry_output)]},
        timing_closure='violated' if timing['metrics']['hold_violating_endpoints'] or timing['metrics']['violating_endpoints'] else 'met')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace',type=Path,required=True);parser.add_argument('--base-rev',required=True)
    parser.add_argument('--profiles',type=Path,default=ROOT/'fixtures/pdk/local-routing.json')
    parser.add_argument('--rustroad',type=Path,default=ROOT/'target/release/rustroad')
    parser.add_argument('--rusteda',type=Path,default=ROOT/'.work/routing-tools/rusteda');parser.add_argument('--ruststa',type=Path,default=ROOT/'.work/routing-tools/ruststa')
    parser.add_argument('--producer',choices=('policy','frozen'),default='policy',help='policy: pinned RustSys c5ec23c with PDK --dont-use cell policy; frozen: historical 9c83124e without --dont-use')
    parser.add_argument('--cell-policy',type=Path,default=ROOT/'fixtures/flow/cell-policy.json')
    parser.add_argument('--manifest',type=Path,default=ROOT/'fixtures/flow/signoff-pinned.json')
    parser.add_argument('--signoff-reference',action='store_true',help='stream the routed DEF with the reference KLayout reader and run foundry signoff on it')
    parser.add_argument('--signoff-gds',action='append',default=[],metavar='PDK=GDS',help='native GDS to sign off and XOR against the reference streamout')
    args=parser.parse_args();args.manifest=args.manifest.resolve();args.cell_policy=args.cell_policy.resolve();require(args.base_rev=='empty','REVISION_CONFLICT: fresh evidence workspace requires empty')
    root=args.workspace.resolve();root.mkdir(parents=True,exist_ok=False);lock=(root/'workspace.lock').open('wb');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB);(root/'HEAD').write_text('rev0\n')
    resource.setrlimit(resource.RLIMIT_AS,(8192*1024**2,8192*1024**2));os.environ.update(GOMAXPROCS='2',QT_QPA_PLATFORM='offscreen',OMP_NUM_THREADS='2')
    h=PhysicalHarness(root,argparse.Namespace(threads=2,memory_mib=2048,timeout_seconds=30,tolerance_ns=0.001));start=time.monotonic()
    old=json.loads((ROOT/'docs/evidence/tap-results.json').read_text())
    tools={n:Path(old['tools'][n]['path']) for n in ('rustsys','yosys','verilator','opensta')};tools.update(rustroad=args.rustroad.resolve(),rusteda=args.rusteda.resolve(),ruststa=args.ruststa.resolve(),klayout=Path('/usr/bin/klayout'))
    policy=None;pinned=dict(old['tools'])
    if args.producer=='policy':
        policy=json.loads(bounded_text(args.cell_policy,1024**2));require(policy.get('schema')=='rustroad.cell-policy.v1','wrong cell-policy schema')
        tools['rustsys']=Path(policy['producer']['binary']['path']);pinned['rustsys']=dict(sha256=policy['producer']['binary']['sha256'])
    summary=dict(schema='rustroad.routing-verification.v2',status='unverified',postroute_timing_status='unverified',functional_status='unverified',tools={},pdks=[],
        producer=dict(mode=args.producer,path=str(tools['rustsys']),sha256=pinned['rustsys']['sha256'],cell_policy=str(args.cell_policy) if policy else None),
        scope=dict(level='block/macro (no pad ring, no seal ring)',drc='foundry decks via scripts/signoff when requested, else missing',
                   lvs='Netgen + KLayout via scripts/signoff when requested, else missing',density='reported separately',
                   ir_drop='unchecked',electromigration='unchecked',signal_integrity='unchecked',ocv='unchecked',
                   timing_accuracy_gate_ns=0.001,cppr='unchecked'),
        required_items=list(flow_status.REQUIRED),reported_items=list(flow_status.REPORTED))
    try:
        r=run_ok(h,'memory_preflight',['free','-h']);summary['memory_preflight']=bounded_text(Path(r['stdout']))
        for n,p in tools.items():
            require(p.is_file(),'missing '+n)
            if n in ('rustsys','yosys','verilator','opensta'):require(sha256(p)==pinned[n]['sha256'],'pinned tool changed: '+n)
            r=run_ok(h,n+'_version',[p,'-V' if n=='yosys' else '-version' if n=='opensta' else '-v' if n=='klayout' else '--version'])
            summary['tools'][n]={**fingerprint(p),'version':bounded_text(Path(r['stdout'])).strip()}
            if n=='klayout':summary['tools'][n].update(signoff_role='not_for_signoff: host build used only by the RustROAD subset routing-geometry checker; signoff verdicts use the pinned image (fixtures/flow/signoff-pinned.json)')
        r=run_ok(h,'openroad_identity',['podman','run','--rm','--pull=never','--network=none','--read-only','--memory=2g','--memory-swap=2g','--entrypoint=/usr/bin/sha256sum',IMAGE,OPENROAD])
        require(bounded_text(Path(r['stdout'])).split()[0]==old['openroad_reference']['sha256'],'OpenROAD identity changed')
        summary['openroad_reference']=old['openroad_reference']
        profiles=json.loads(bounded_text(args.profiles,1024**2));require(profiles['schema']=='rustroad.routing-profile.v1','wrong profile schema')
        for p in profiles['pdks']:
            p=dict(p)
            for key in ('config','routing_config','cts_sdc'):p[key]=str((ROOT/p[key]).resolve())
            try:summary['pdks'].append(case(h,p,tools,args,policy))
            except (RuntimeError,ValueError,KeyError,OSError) as error:summary['pdks'].append(dict(pdk=p['tag'],status='failed',reason=str(error)))
        functional=len(summary['pdks'])==2 and all(p['status']=='passed' for p in summary['pdks'])
        summary['functional_status']='passed' if functional else 'failed'
        summary['timing_accuracy_status']='passed' if functional and all(p['opensta']['status']=='passed' for p in summary['pdks']) else 'failed'
        summary['postroute_timing_status']='passed' if functional and all(p['opensta']['status']=='passed' and p['timing_closure']=='met' for p in summary['pdks']) else 'partial' if functional else 'failed'
    except (RuntimeError,ValueError,KeyError,OSError) as error:summary['reason']=str(error)
    finally:
        summary['identity_stable']=all(sha256(Path(t['path']))==t['sha256'] for t in summary['tools'].values()) and bool(summary['tools'])
        gate=flow_status.overall(summary['pdks'],summary['postroute_timing_status'],summary['functional_status']=='passed',summary['identity_stable'])
        summary['status']=gate['status'];summary['blocking']=gate['blocking'];summary['failed_items']=gate.get('failed_items',[])
        summary['receipts']=h.receipts;summary['wall_seconds']=time.monotonic()-start;summary['peak_rss_kib']=max((r['peak_rss_kib'] for r in h.receipts if r['peak_rss_kib'] is not None),default=None)
        write_json(root/'summary.json',summary);(root/'HEAD').write_text('rev1\n')
    print(json.dumps(dict(status=summary['status'],postroute_timing_status=summary['postroute_timing_status'],functional_status=summary['functional_status'],
                          blocking=len(summary.get('blocking',[])),summary=str(root/'summary.json'))))
    return flow_status.EXIT[summary['status']]


if __name__=='__main__':sys.exit(main())
