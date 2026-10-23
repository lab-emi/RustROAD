"""Black-box routed DEF/OpenRCX/OpenSTA adapters; no reference source inspection."""
import json
import math
import re
from pathlib import Path
from verify_physical import IMAGE, OPENROAD, run_ok, require, fingerprint
from verify_logic import bounded_text, tcl_path, NUMBER, write_json


def extract(h, tag, profile, definition):
    output = h.root / (tag + '.spef')
    geometry = h.root / (tag + '_routes.json')
    measurement = h.root / (tag + '_openroad.time')
    script = h.root / (tag + '_extract.tcl')
    script.write_text('''read_lef /technology.lef
read_lef /cells.lef
read_liberty /library.lib
read_def /run/''' + definition.name + '''
set block [ord::get_db_block]
set iterator [odb::dbWirePathItr]
set path [odb::dbWirePath]
set shape [odb::dbWirePathShape]
set count 0
set via_id 0
foreach layer [[$block getTech] getLayers] {
 puts "L [binary encode hex [$layer getName]] [$layer getWidth]"
}
foreach net [$block getNets] {
 if {[$net getSigType] in {POWER GROUND}} {continue}
 set nh [binary encode hex [$net getName]]
 puts "N $nh [expr {[llength [$net getITerms]]+[llength [$net getBTerms]]}]"
 set wire [$net getWire]
 if {$wire == "NULL"} {continue}
 $iterator begin $wire
 while {[$iterator getNextPath $path]} {
  while {[$iterator getNextShape $shape]} {
   incr count
   if {$count > 100000} {error "route shape budget"}
   set s [odb::dbWirePathShape_shape_get $shape]
   if {[$s isVia]} {
    set via [$s getTechVia]
    if {$via == "NULL"} {error "only fixed technology vias supported"}
    set xy [$s getViaXY]
    foreach box [$via getBoxes] {
     puts "V $nh $via_id [binary encode hex [$via getName]] [binary encode hex [[$box getTechLayer] getName]] [expr {[lindex $xy 0]+[$box xMin]}] [expr {[lindex $xy 1]+[$box yMin]}] [expr {[lindex $xy 0]+[$box xMax]}] [expr {[lindex $xy 1]+[$box yMax]}]"
    }
    incr via_id
   } else {
    puts "W $nh [binary encode hex [[$s getTechLayer] getName]] [$s xMin] [$s yMin] [$s xMax] [$s yMax]"
   }
  }
 }
}
array set seen {}
foreach inst [$block getInsts] {
 set master [$inst getMaster]
 set mh [binary encode hex [$master getName]]
 if {[info exists seen($mh)]} {continue}
 set seen($mh) 1
 foreach obs [$master getObstructions] {
  puts "O $mh [binary encode hex [[$obs getTechLayer] getName]] [$obs xMin] [$obs yMin] [$obs xMax] [$obs yMax]"
 }
}
puts ROUTE_PROBE_COMPLETE
define_process_corner -ext_model_index 0 nominal
set_extraction_rules_file /extraction.rules
extract_parasitics -max_res 0 -coupling_threshold 0.1 -no_merge_via_res
write_spef /run/''' + output.name + '''
puts EXTRACTION_COMPLETE
exit
''')
    command = ['podman','run','--rm','--pull=never','--network=none','--read-only',
        '--tmpfs=/tmp:rw,size=64m','--memory=2g','--memory-swap=2g','--cpus=2','--pids-limit=128',
        '--volume='+str(h.root)+':/run:rw', '--volume='+profile['technology_lef']['path']+':/technology.lef:ro',
        '--volume='+profile['cell_lef']['path']+':/cells.lef:ro','--volume='+profile['liberty']['path']+':/library.lib:ro',
        '--volume='+profile['extraction_rules']['path']+':/extraction.rules:ro','--entrypoint=/usr/bin/timeout',
        IMAGE,'25s','/usr/bin/time','-v','-o','/run/'+measurement.name,OPENROAD,'-no_init','-no_splash','-threads','2','-exit','/run/'+script.name]
    receipt=run_ok(h, tag+'_extract', command)
    text=bounded_text(Path(receipt['stdout']))
    require('EXTRACTION_COMPLETE' in text and 'ROUTE_PROBE_COMPLETE' in text,'reference extraction did not finish')
    decode=lambda x:bytes.fromhex(x).decode()
    data=dict(wires=[],vias=[],obstructions={},net_terminals={},layer_widths={})
    vias={}
    for line in text.splitlines():
        f=line.split()
        if not f:continue
        if f[0]=='W':data['wires'].append(dict(net=decode(f[1]),layer=decode(f[2]),rect=list(map(int,f[3:]))))
        elif f[0]=='V':
            v=vias.setdefault(int(f[2]),dict(net=decode(f[1]),name=decode(f[3]),shapes=[]))
            v['shapes'].append(dict(layer=decode(f[4]),rect=list(map(int,f[5:]))))
        elif f[0]=='O':data['obstructions'].setdefault(decode(f[1]),[]).append(dict(layer=decode(f[2]),rect=list(map(int,f[3:]))))
        elif f[0]=='N':data['net_terminals'][decode(f[1])]=int(f[2])
        elif f[0]=='L':data['layer_widths'][decode(f[1])]=int(f[2])
        require(len(data['wires'])+len(vias)<=100000,'reference shape budget')
    data['vias']=list(vias.values())
    m=bounded_text(measurement)
    rss=re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)',m)
    require(rss is not None and int(rss[1])<2*1024**2,'extraction RSS missing/over budget')
    data['measurement']=dict(peak_rss_kib=int(rss[1]),receipt=tag+'_extract')
    write_json(geometry,data)
    require(output.stat().st_size<=32*1024**2,'SPEF size budget')
    return data, output, fingerprint(geometry)


def compare_extracted(h, tag, opensta, lib, netlist, sdc, spef, native, max_skew_ns):
    script=h.root/(tag+'.tcl')
    script.write_text(f'''read_liberty {tcl_path(lib)}
read_verilog {tcl_path(netlist)}
link_design lane_smoke
read_sdc {tcl_path(sdc)}
read_spef {tcl_path(spef)}
report_units
puts RUSTROAD_CLOCK_BEGIN
puts "CLOCK_SINKS [llength [all_registers -clock_pins]]"
report_clock_latency -digits 12
puts RUSTROAD_CLOCK_END
puts RUSTROAD_LIMITS_BEGIN
report_check_types -max_slew -max_capacitance -max_fanout -violators -digits 12
puts RUSTROAD_LIMITS_END
puts RUSTROAD_SETUP_BEGIN
report_checks -path_delay max -group_path_count 128 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_SETUP_END
puts RUSTROAD_HOLD_BEGIN
report_checks -path_delay min -group_path_count 128 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_HOLD_END
exit
''')
    receipt=run_ok(h,tag,[opensta,'-no_init','-no_splash','-exit',script])
    text=bounded_text(Path(receipt['stdout']))+bounded_text(Path(receipt['stderr']))
    require(not re.search(r'(?mi)^\s*Error(?:\s+\d+)?:',text),'OpenSTA command error')
    require(re.search(r'(?mi)^\s*Time\s+1ns\s*$',text),'OpenSTA units')
    def section(name):
        a,b='RUSTROAD_'+name+'_BEGIN','RUSTROAD_'+name+'_END'
        require(text.count(a)==1 and text.count(b)==1,'reference report markers absent')
        return text.split(a,1)[1].split(b,1)[0]
    values={}
    for kind in ('SETUP','HOLD'):
        pattern=rf'^(\S+)\s+\([^\n]*?\)\s+({NUMBER})\s+({NUMBER})\s+({NUMBER})\s+\((?:MET|VIOLATED)\)\s*$'
        values[kind.lower()]={m[1]:float(m[4]) for m in re.finditer(pattern,section(kind),re.M)}
    names={r['endpoint'] for r in native['endpoints']}
    require(names and all(set(v)==names for v in values.values()),'extracted timing endpoint coverage mismatch')
    differences=[]
    for row in native['endpoints']:
        for kind in ('setup','hold'):
            a,b=row[kind+'_slack_ns'],values[kind][row['endpoint']]
            require(math.isfinite(a) and math.isfinite(b),'nonfinite extracted timing')
            differences.append(dict(endpoint=row['endpoint'],kind=kind,ruststa_ns=a,opensta_ns=b,difference_ns=abs(a-b)))
    maximum=max(r['difference_ns'] for r in differences)
    clocks=section('CLOCK');skews=[float(x) for x in re.findall(rf'^\s*({NUMBER})\s+skew\s*$',clocks,re.M)]
    latencies=[float(x) for x in re.findall(rf'^\s*({NUMBER})\s+network latency ',clocks,re.M)]
    sinks=re.search(r'CLOCK_SINKS (\d+)',clocks)
    require(sinks is not None and len(skews)==2 and len(latencies)==4 and min(latencies)>0,'propagated clock latency coverage missing')
    limits=section('LIMITS')
    return dict(status='passed' if maximum<=h.args.tolerance_ns else 'different',corner='tt',rc_corner='nominal',wire_input='detailed_spef',clock_model='propagated',
        native_delay_calculator=native.get('scope',{}).get('delay_calculator','unknown'),reference_delay_calculator='OpenSTA default',
        comparison_scope='same Liberty, SDC and SPEF inputs; delay calculators are not asserted identical',
        tolerance_ns=h.args.tolerance_ns,max_difference_ns=maximum,comparisons=differences,
        clock_sink_count=int(sinks[1]),clock_latencies_ns=latencies,clock_skew_rise_fall_ns=skews,
        skew_limit_ns=max_skew_ns,skew_status='passed' if max(skews)<=max_skew_ns else 'failed',
        transition_capacitance_fanout_status='failed' if 'VIOLATED' in limits else 'passed',
        limits_report=limits.strip(),receipt=tag)
