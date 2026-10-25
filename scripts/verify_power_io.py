#!/usr/bin/env python3
"""Bounded independent OpenDB checks of physical I/O, supply metal and tie cells.

Reference tools are used only through documented commands and public database
APIs. No routing, RC, DRC/LVS, IR/EM or foundry signoff is claimed.
"""
from __future__ import annotations
import argparse
from collections import defaultdict
import copy
import json
import math
import os
from pathlib import Path
import re
import shutil
import sys

from verify_physical import PhysicalHarness, run_ok, fingerprint, IMAGE, OPENROAD, compare_sta
from verify_logic import bounded_text, sha256, write_json, yosys_path, sta_flow, ideal_projection

MAX_SHAPES = 16000
MAX_PINS = 16000
MAX_COMPARISONS = 100000


class Unverified(RuntimeError):
    pass


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def overlap(a, b):
    return a[0] <= b[2] and b[0] <= a[2] and a[1] <= b[3] and b[1] <= a[3]


def transformed(rect, inst):
    x0, y0, x1, y1 = rect
    if inst['orientation'] in ('FS', 'MX'):
        y0, y1 = inst['height'] - y1, inst['height'] - y0
    else:
        require(inst['orientation'] in ('N', 'R0'), 'unsupported orientation')
    return (inst['x'] + x0, inst['y'] + y0, inst['x'] + x1, inst['y'] + y1)


class Sets:
    def __init__(self, n):
        self.parent = list(range(n))
        self.size = [1] * n

    def root(self, i):
        while i != self.parent[i]:
            self.parent[i] = self.parent[self.parent[i]]
            i = self.parent[i]
        return i

    def join(self, a, b):
        a, b = self.root(a), self.root(b)
        if a == b:
            return
        if self.size[a] < self.size[b]:
            a, b = b, a
        self.parent[b] = a
        self.size[a] += self.size[b]


def check_power_geometry(data, profile):
    """Independent bounded rectangle sweep; production kernel is not reused."""
    roles = profile['pin_roles']
    primary = set(profile['primary_pins'])
    supplies = set(roles.values())
    require(data['net_uses'].get(profile['power_net'])=='power' and data['net_uses'].get(profile['ground_net'])=='ground','supply net USE mismatch')
    conductors = []
    signal_io = []
    checked_pins = 0
    flips = 0
    for inst in data['instances']:
        flips += inst['orientation'] in ('FS', 'MX')
        for pin in inst['pins']:
            if pin['name'] not in roles:
                continue
            checked_pins += 1
            require(checked_pins <= MAX_PINS, 'PG pin budget exceeded')
            require(pin['net'] == roles[pin['name']], f"wrong supply polarity {inst['name']}/{pin['name']}")
            if pin['name'] in primary:
                boxes = [transformed(shape['rect'], inst) for shape in pin['shapes'] if shape['layer'] == profile['rail_layer']]
                require(boxes, 'primary PG pin has no rail-layer shape')
                # A tap primary pin can include well and metal geometry in one
                # PORT. OpenDB's ITerm bbox covers all those shapes, whereas
                # only routing-layer shapes belong in the conductor graph.
                all_boxes = [transformed(shape['rect'], inst) for shape in pin['shapes']]
                bounding = (min(r[0] for r in all_boxes),min(r[1] for r in all_boxes),max(r[2] for r in all_boxes),max(r[3] for r in all_boxes))
                if pin.get('bbox') is not None:
                    require(tuple(pin['bbox']) == bounding, 'N/FS transform differs from OpenDB ITerm bbox')
                for rect in boxes:
                    conductors.append(dict(net=pin['net'],layer=profile['rail_layer'],rect=rect,kind='pin'))
    require(flips > 0, 'no FS instances exercised')
    def cover(intervals, lo, hi):
        cursor=lo
        for a,b in sorted(intervals):
            if b<=cursor:continue
            require(a<=cursor,'intrinsic cell-rail interval gap')
            cursor=max(cursor,b)
        require(cursor>=hi,'intrinsic cell-rail does not cover row')
    require(0<len(data['expected_rows'])<=128,'row budget exceeded')
    members_by_y=defaultdict(list)
    for inst in data['instances']:members_by_y[inst['y']].append(inst)
    require(set(members_by_y)=={r['y']for r in data['expected_rows']},'instance outside expected rows')
    row_edges=[]
    for row in data['expected_rows']:
        members=members_by_y[row['y']]
        end=row['x_start']
        for inst in sorted(members,key=lambda i:i['x']):
            require(inst['height']==data['site_height'] and inst['width']>0,'unsupported cell height/width')
            require(inst['x']>=end and inst['x']+inst['width']<=row['x_end'],'cell overlap or out-of-row placement')
            require((inst['x']-row['x_start'])%data['site_width']==0,'cell off site grid')
            end=inst['x']+inst['width']
        require(members,'empty prepared row')
        cover([(i['x'],i['x']+i['width'])for i in members],row['x_start'],row['x_end'])
        edge_polarities=[]
        for edge in (row['y'],row['y']+data['site_height']):
            intervals=defaultdict(list)
            for inst in members:
                for pin in inst['pins']:
                    if pin['name']not in primary:continue
                    for shape in pin['shapes']:
                        if shape['layer']!=profile['rail_layer']:continue
                        r=transformed(shape['rect'],inst)
                        if r[1]<=edge<=r[3]:intervals[pin['net']].append((r[0],r[2]))
            require(len(intervals)==1,'mixed or absent power polarity along row edge')
            net,spans=next(iter(intervals.items()));cover(spans,row['x_start'],row['x_end']);edge_polarities.append(net)
        require(edge_polarities[0]!=edge_polarities[1],'row top and bottom supply polarity equal')
        row_edges.append((row['y'],*edge_polarities))
    for previous,current in zip(sorted(row_edges),sorted(row_edges)[1:]):
        if previous[0]+data['site_height']==current[0]:require(previous[2]==current[1],'adjacent row power polarity conflict')
    for wire in data['wires']:
        require(wire['net'] in supplies, 'unexpected special-net conductor')
        if wire['layer'] in profile['conductor_layers']:
            conductors.append(wire)
    for via in data['vias']:
        require(via['net'] in supplies, 'unexpected via supply')
        require(via['name']==profile['via'],'unexpected fixed via')
        metals=[s for s in via['shapes'] if s['layer'] in profile['conductor_layers']]
        cuts=[s for s in via['shapes'] if s['layer'] not in profile['conductor_layers']]
        require(len({s['layer']for s in metals})==2 and cuts,'via lacks both metals or cut')
        require(all(any(overlap(m['rect'],c['rect'])for c in cuts)for m in metals),'via cut does not intersect enclosure metal')
        # Metal enclosures of a single actual via are electrically connected.
        for shape in via['shapes']:
            if shape['layer'] in profile['conductor_layers']:
                conductors.append(dict(net=via['net'],layer=shape['layer'],rect=shape['rect'],kind='via',via=via['id']))
    require(supplies <= {p['net'] for p in data['io']},'supply boundary port missing')
    for pin in data['io']:
        require(pin['status'] in ('PLACED','FIXED','FIRM'), 'unplaced I/O pin')
        require(pin['shapes'], 'I/O pin has no geometry')
        if pin['net'] in supplies:
            expected_use='power' if pin['net']==roles[profile['primary_pins'][0]] else 'ground'
            require(pin['use']==expected_use and pin['direction']=='inout','PG top-level terminal properties mismatch')
        else: require(pin['use']=='signal','signal I/O terminal use mismatch')
        die = data['die']
        for shape in pin['shapes']:
            r=shape['rect']
            require(die[0] <= r[0] <= r[2] <= die[2] and die[1] <= r[1] <= r[3] <= die[3], 'I/O shape outside die')
            require(r[0] == die[0] or r[1] == die[1] or r[2] == die[2] or r[3] == die[3], 'I/O shape does not touch die boundary')
        if pin['net'] not in supplies:
            signal_io.append(pin)
    require(signal_io, 'no placed signal I/O exercised')
    io_shapes=[(p['net'],s['layer'],s['rect']) for p in data['io'] for s in p['shapes']]
    require(len(io_shapes)<=MAX_SHAPES,'I/O shape budget exceeded')
    tests=0
    for i,(net,layer,rect) in enumerate(io_shapes):
        for j in range(i):
            other,other_layer,other_rect=io_shapes[j]
            tests+=1;require(tests<=MAX_COMPARISONS,'I/O comparison budget exceeded')
            require(net==other or layer!=other_layer or not overlap(rect,other_rect),'I/O terminals overlap on one metal layer')
    require(0 < len(conductors) <= MAX_SHAPES, 'conductor shape budget exceeded')
    sets = Sets(len(conductors))
    layer_indices = defaultdict(list)
    via_indices = defaultdict(list)
    for i, shape in enumerate(conductors):
        rect=shape['rect']
        require(len(rect)==4 and all(type(v) is int and -(1<<63)<=v<(1<<63) for v in rect) and rect[0]<rect[2] and rect[1]<rect[3], 'invalid integer conductor rectangle')
        die=data['die'];require(die[0]<=rect[0] and die[1]<=rect[1] and rect[2]<=die[2] and rect[3]<=die[3],'supply conductor lies outside die')
        layer_indices[shape['layer']].append(i)
        if 'via' in shape:
            via_indices[(shape['net'],shape['via'])].append(i)
    for indices in via_indices.values():
        require(len(indices)>=2, 'via has fewer than two conducting layers')
        for i in indices[1:]:
            sets.join(indices[0],i)
    comparisons=0
    for indices in layer_indices.values():
        active=[]
        for i in sorted(indices,key=lambda i:conductors[i]['rect'][0]):
            current=conductors[i]
            active=[j for j in active if conductors[j]['rect'][2]>=current['rect'][0]]
            for j in active:
                comparisons+=1
                require(comparisons<=MAX_COMPARISONS, 'rectangle comparison budget exceeded')
                previous=conductors[j]
                if overlap(current['rect'],previous['rect']):
                    require(current['net']==previous['net'], 'power/ground conductor short')
                    sets.join(i,j)
            active.append(i)
    components=defaultdict(set)
    for i,shape in enumerate(conductors):
        components[shape['net']].add(sets.root(i))
    require(set(components)==supplies, 'missing physical supply metal')
    require(all(len(c)==1 for c in components.values()), 'disconnected rail/spine/pin component')
    return dict(status='passed',pg_pins=checked_pins,flipped_instances=flips,signal_io=len(signal_io),
        conductor_rectangles=len(conductors),intersection_comparisons=comparisons,rows_with_intrinsic_rail_continuity=len(row_edges),
        components={n:len(c) for n,c in components.items()},body_bias_continuity='unchecked',
        drc_status='unchecked',ir_drop_status='unchecked',electromigration_status='unchecked')


def tie_function_proof(h, profile, liberty, yosys):
    proofs=[]
    by_cell=defaultdict(list)
    for role in ('tie_high','tie_low'):
        item=profile[role]
        by_cell[item['master']].append((item['pin'],1 if role=='tie_high' else 0))
    for i,(master,claims) in enumerate(sorted(by_cell.items())):
        script=h.root/f"{profile['tag']}_tie_function_{i}.ys"
        script.write_text(f"read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(liberty)}\nhierarchy -top {master}\nproc\nflatten\nsat "+' '.join(f'-prove {pin} {value}' for pin,value in claims)+' -verify\n')
        result=run_ok(h,f"{profile['tag']}_tie_function_{i}",[yosys,'-Q','-T','-s',script])
        require('SAT proof finished - no model found: SUCCESS!' in bounded_text(Path(result['stdout'])),'tie function not proven')
        negative=script.with_name(script.stem+'_negative.ys')
        negative.write_text(script.read_text().replace('sat '+ ' '.join(f'-prove {pin} {value}' for pin,value in claims), 'sat '+ ' '.join(f'-prove {pin} {1-value}' for pin,value in claims)))
        rejected=h.run(f"{profile['tag']}_tie_function_{i}_negative",[str(yosys),'-Q','-T','-s',str(negative)])
        require(rejected['exit_code']!=0 and 'FAIL' in bounded_text(Path(rejected['stdout'])),'tie polarity negative control missed')
        proofs.append(dict(master=master,pins=claims,status='proved',receipt=result['label'],wrong_polarity='rejected'))
    return proofs




def capture_power_db(h, tag, pdk, definition):
    script=h.root/(tag+'_power_probe.tcl')
    script.write_text(f'''read_lef /technology.lef
read_lef /cells.lef
read_def /run/{definition.name}
if {{[catch {{check_placement -verbose}} message]}} {{ puts "PLACEMENT_REJECTED $message"; exit 42 }}
set block [ord::get_db_block]
set die [$block getDieArea]
puts "D [$die xMin] [$die yMin] [$die xMax] [$die yMax]"
array set seen {{}}
foreach inst [$block getInsts] {{
  set name [binary encode hex [$inst getName]]
  set master [$inst getMaster]
  set mh [binary encode hex [$master getName]]
  set xy [$inst getLocation]
  puts "G $name $mh [$master getWidth] [$master getHeight] [lindex $xy 0] [lindex $xy 1] [$inst getOrient]"
  if {{![info exists seen($mh)]}} {{
    set seen($mh) 1
    foreach mt [$master getMTerms] {{
      set ph [binary encode hex [$mt getName]]
      puts "M $mh $ph [$mt getIoType] [$mt getSigType]"
      foreach mp [$mt getMPins] {{
        foreach box [$mp getGeometry] {{
          set layer [$box getTechLayer]
          puts "P $mh $ph [binary encode hex [$layer getName]] [$box xMin] [$box yMin] [$box xMax] [$box yMax]"
        }}
      }}
    }}
  }}
  foreach it [$inst getITerms] {{
    set mt [$it getMTerm]
    set net [$it getNet]
    set nh -
    if {{$net != "NULL"}} {{ set nh [binary encode hex [$net getName]] }}
    set bbox [$it getBBox]
    puts "I $name [binary encode hex [$mt getName]] $nh [$bbox xMin] [$bbox yMin] [$bbox xMax] [$bbox yMax]"
  }}
}}
foreach bt [$block getBTerms] {{
  set nh [binary encode hex [[$bt getNet] getName]]
  puts "Q [binary encode hex [$bt getName]] $nh [$bt getIoType] [$bt getSigType]"
  foreach bp [$bt getBPins] {{
    foreach box [$bp getBoxes] {{
      puts "B [binary encode hex [$bt getName]] $nh [$bp getPlacementStatus] [$bt getIoType] [$bt getSigType] [binary encode hex [[$box getTechLayer] getName]] [$box xMin] [$box yMin] [$box xMax] [$box yMax]"
    }}
  }}
}}
set viaId 0
foreach net [$block getNets] {{
  set nh [binary encode hex [$net getName]]
  if {{[llength [$net getSWires]] > 0}} {{ puts "N $nh [$net getSigType]" }}
  foreach sw [$net getSWires] {{
    foreach box [$sw getWires] {{
      if {{[$box isVia]}} {{
        set via [$box getTechVia]
        if {{$via == "NULL"}} {{ error "block vias not supported by this probe" }}
        set xy [$box getViaXY]
        foreach vb [$via getBoxes] {{
          set layer [$vb getTechLayer]
          puts "V $nh $viaId [binary encode hex [$via getName]] [binary encode hex [$layer getName]] [expr {{[lindex $xy 0]+[$vb xMin]}}] [expr {{[lindex $xy 1]+[$vb yMin]}}] [expr {{[lindex $xy 0]+[$vb xMax]}}] [expr {{[lindex $xy 1]+[$vb yMax]}}]"
        }}
        incr viaId
      }} else {{
        set layer [$box getTechLayer]
        puts "W $nh [binary encode hex [$layer getName]] [$box xMin] [$box yMin] [$box xMax] [$box yMax]"
      }}
    }}
  }}
}}
puts POWER_PROBE_COMPLETE
exit 0
''')
    time_path=h.root/(tag+'_power_probe.time')
    command=['podman','run','--rm','--pull=never','--network=none','--read-only','--tmpfs=/tmp:rw,size=64m',
        '--memory=2g','--memory-swap=2g','--cpus=2','--pids-limit=128',
        '--volume='+str(h.root)+':/run:rw','--volume='+pdk['technology_lef']['path']+':/technology.lef:ro',
        '--volume='+pdk['cell_lef']['path']+':/cells.lef:ro','--entrypoint=/usr/bin/timeout',IMAGE,'25s','/usr/bin/time','-v','-o','/run/'+time_path.name,
        OPENROAD,'-no_init','-no_splash','-threads','2','-exit','/run/'+script.name]
    receipt=run_ok(h,tag+'_power_probe',command)
    text=bounded_text(Path(receipt['stdout']))
    require('POWER_PROBE_COMPLETE' in text,'OpenDB power probe API failed')
    decode=lambda x: bytes.fromhex(x).decode()
    masters,instances,ports,via_groups={}, {}, {}, {}
    data=dict(instances=[],io=[],wires=[],vias=[],net_uses={})
    for line in text.splitlines():
        fields=line.split()
        if not fields:continue
        kind=fields[0]
        if kind=='D': data['die']=list(map(int,fields[1:]))
        elif kind=='G':
            name,master=decode(fields[1]),decode(fields[2])
            instances[name]=dict(name=name,master=master,width=int(fields[3]),height=int(fields[4]),x=int(fields[5]),y=int(fields[6]),orientation=fields[7],pins=[])
        elif kind=='M':
            masters[(decode(fields[1]),decode(fields[2]))]=dict(direction=fields[3].lower(),use=fields[4].lower(),shapes=[])
        elif kind=='P':
            masters[(decode(fields[1]),decode(fields[2]))]['shapes'].append(dict(layer=decode(fields[3]),rect=list(map(int,fields[4:]))))
        elif kind=='I':
            inst,pin=decode(fields[1]),decode(fields[2]);detail=copy.deepcopy(masters[(instances[inst]['master'],pin)])
            detail.update(name=pin,net=decode(fields[3]) if fields[3]!='-' else None,bbox=list(map(int,fields[4:])))
            instances[inst]['pins'].append(detail)
        elif kind=='Q':
            name=decode(fields[1]);ports[name]=dict(name=name,net=decode(fields[2]),status='UNPLACED',direction=fields[3].lower(),use=fields[4].lower(),shapes=[])
        elif kind=='B':
            name=decode(fields[1]);obj=ports[name];obj['status']=fields[3]
            obj['shapes'].append(dict(layer=decode(fields[6]),rect=list(map(int,fields[7:]))))
        elif kind=='N':data['net_uses'][decode(fields[1])]=fields[2].lower()
        elif kind=='W':data['wires'].append(dict(net=decode(fields[1]),layer=decode(fields[2]),rect=list(map(int,fields[3:])),kind='wire'))
        elif kind=='V':
            key=(decode(fields[1]),int(fields[2]));via=via_groups.setdefault(key,dict(net=key[0],id=key[1],name=decode(fields[3]),shapes=[]))
            via['shapes'].append(dict(layer=decode(fields[4]),rect=list(map(int,fields[5:]))))
        require(len(instances)<=2000 and len(data['wires'])<=MAX_SHAPES and len(via_groups)<=MAX_SHAPES,'OpenDB capture budget exceeded')
    timing=bounded_text(time_path)
    peak=re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)',timing)
    require(peak is not None and 0<int(peak[1])<2*1024**2,'OpenROAD RSS missing or over budget')
    data.update(instances=list(instances.values()),io=list(ports.values()),vias=list(via_groups.values()),measurement=dict(peak_rss_kib=int(peak[1]),time=fingerprint(time_path)))
    write_json(h.root/(tag+'_power_geometry.json'),data)
    return data


POWER_RTL = '''module lane_smoke(input clk, input en, input [3:0] a, input [3:0] b,
 output [3:0] y, output [3:0] z, output reg high_bit, output reg low_bit);
 reg [3:0] q;
 wire [3:0] mix;
 assign mix=(a^b)&{4{en}};
 always @(posedge clk) begin q<=mix;high_bit<=1'b1;low_bit<=1'b0;end
 assign y=q;
 assign z=a&b;
endmodule
'''


def semantic_simulation(h, tag, tools, profile, rtl, exported):
    model=h.root/(tag+'_gate_sim.v');ys=h.root/(tag+'_gate_sim.ys');tb=h.root/(tag+'_tb.sv')
    ys.write_text(f"read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(Path(profile['liberty']['path']))}\nread_verilog {yosys_path(exported)}\nhierarchy -top lane_smoke\nproc\nflatten\nrename lane_smoke gate\nwrite_verilog -noattr {yosys_path(model)}\n")
    run_ok(h,tag+'_sim_model',[tools['yosys'],'-Q','-T','-s',ys])
    tb.write_text('''module test;
 reg clk=0,en=0;reg[3:0]a=0,b=0;wire[3:0]yg,zg,ym,zm;wire hg,lg,hm,lm;
 lane_smoke gold(.clk(clk),.en(en),.a(a),.b(b),.y(yg),.z(zg),.high_bit(hg),.low_bit(lg));
 gate mapped(.clk(clk),.en(en),.a(a),.b(b),.y(ym),.z(zm),.high_bit(hm),.low_bit(lm));
 initial begin
 for(integer i=0;i<512;i=i+1)begin
 a=i[3:0];b=i[7:4];en=i[8];#1;clk=1;#1;
 if(yg!==((a^b)&{4{en}})||ym!==yg||zg!==(a&b)||zm!==zg||hg!==1'b1||hm!==hg||lg!==1'b0||lm!==lg)begin
 $display("POWER_SIM_MISMATCH case=%0d",i);$fatal(1);end
 clk=0;#1;end
 $display("POWER_SIM_ACCEPT cases=512");$finish;end
 endmodule
''')
    build=h.root/(tag+'_sim_build')
    run_ok(h,tag+'_sim_build',[tools['verilator'],'--binary','--timing','-Wno-fatal','-j','2','--top-module','test','--Mdir',build,rtl,model,tb])
    result=run_ok(h,tag+'_sim_run',[build/'Vtest'])
    require('POWER_SIM_ACCEPT cases=512' in bounded_text(Path(result['stdout'])),'simulation coverage marker absent')
    return dict(status='passed',cases=512,initial_state='sampled after first edge',constant_outputs=dict(high_bit=1,low_bit=0))


def geometry_controls(data,profile):
    cases=[]
    for name in ('polarity','row_gap','strap_gap','short','io_interior'):
        bad=copy.deepcopy(data)
        if name=='polarity':
            changed=False
            for inst in bad['instances']:
                for pin in inst['pins']:
                    if pin['name']==profile['primary_pins'][0]:
                        pin['net']=profile['pin_roles'][profile['primary_pins'][1]];changed=True;break
                if changed:break
            require(changed,'polarity control unavailable')
        elif name=='row_gap':
            filler=next((i for i in bad['instances'] if 'fill_' in i['master']),None)
            require(filler is not None,'row-gap control requires filler')
            bad['instances'].remove(filler)
        elif name=='strap_gap':
            supply=profile['pin_roles'][profile['primary_pins'][0]]
            before=len(bad['wires'])
            bad['wires']=[w for w in bad['wires'] if not(w['net']==supply and w['layer']==profile['conductor_layers'][1])]
            bad['vias']=[v for v in bad['vias'] if v['net']!=supply]
            require(before!=len(bad['wires']),'strap control unavailable')
        elif name=='short':
            badwire=copy.deepcopy(next(w for w in bad['wires'] if w['net']==profile['pin_roles'][profile['primary_pins'][0]]))
            badwire['net']=profile['pin_roles'][profile['primary_pins'][1]];bad['wires'].append(badwire)
        else:
            pin=next(p for p in bad['io'] if p['net'] not in set(profile['pin_roles'].values()))
            for shape in pin['shapes']:
                r=shape['rect'];w=r[2]-r[0];h=r[3]-r[1];shape['rect']=[r[0]+w+1,r[1]+h+1,r[2]+w+1,r[3]+h+1]
        try:
            check_power_geometry(bad,profile)
        except RuntimeError as error:
            cases.append(dict(control=name,status='rejected',diagnostic=str(error)))
        else:raise RuntimeError('geometry oracle missed negative control '+name)
    return dict(status='passed',scope='mutated copies of actual OpenDB records; physical workspace unchanged',cases=cases)



def verify_powered_netlist(h, tag, yosys, netlist, data):
    script=h.root/(tag+'_powered_json.ys');output=h.root/(tag+'_powered_json.json')
    script.write_text(f"read_verilog {yosys_path(netlist)}\nwrite_json {yosys_path(output)}\n")
    run_ok(h,tag+'_powered_json',[yosys,'-Q','-T','-s',script])
    module=json.loads(bounded_text(output))['modules']['lane_smoke']
    groups=defaultdict(list)
    declared=[]
    for name,port in module['ports'].items():
        for i,bit in enumerate(port['bits']):
            index=i+port.get('offset',0)
            if port.get('upto',0):index=len(port['bits'])-1-i+port.get('offset',0)
            scalar=name if len(port['bits'])==1 else f"{name}[{index}]"
            groups[str(bit)].append(('P',scalar,''));declared.append((scalar,port['direction']))
    cells=[];connections=0
    for name,cell in module['cells'].items():
        cells.append((name,cell['type']))
        for pin,bits in cell['connections'].items():
            require(len(bits)<=1,'non-scalar physical library pin')
            if bits:
                require(type(bits[0]) is int,'unmaterialized literal remains on a physical cell pin')
                groups[str(bits[0])].append(('I',name,pin));connections+=1
    require(connections<=MAX_PINS and len(cells)<=2000,'powered netlist oracle budget')
    actual=defaultdict(list)
    for inst in data['instances']:
        for pin in inst['pins']:
            if pin['net'] is not None:actual[pin['net']].append(('I',inst['name'],pin['name']))
    for port in data['io']:actual[port['net']].append(('P',port['name'],''))
    require(sorted(cells)==sorted((i['name'],i['master'])for i in data['instances']),'powered Verilog cell/master coverage differs from OpenDB')
    require(sorted(declared)==sorted((p['name'],p['direction'])for p in data['io']),'powered Verilog port/direction differs from OpenDB')
    normalize=lambda d:sorted(tuple(sorted(v))for v in d.values())
    require(normalize(groups)==normalize(actual),'powered Verilog signal/PG endpoint partitions differ from OpenDB')
    return dict(status='passed',cells=len(cells),connections=connections,ports=len(declared),method='independent bare-Yosys parsing versus public OpenDB endpoint partitions')

def run_power_case(h, profile, tools):
    from verify_physical import external_cec
    tag=profile['tag'];lib=Path(profile['liberty']['path']);config=Path(profile['config']).resolve()
    for key in ('liberty','technology_lef','cell_lef'):
        if not Path(profile[key]['path']).is_file():raise Unverified('missing PDK '+key)
        require(sha256(Path(profile[key]['path']))==profile[key]['sha256'],'PDK input hash mismatch: '+key)
    rtl=h.root/(tag+'.sv');rtl.write_text(POWER_RTL)
    mapped=h.root/(tag+'_mapped.v');literal=h.root/(tag+'_literal.v')
    base=[tools['rustsys'],'--threads','2','--progress','off']
    synthesis=h.json_command(tag+'_synth',list(map(str,base+['synth','--backend','cpu','--top','lane_smoke','--lib',lib,'--out',mapped,'--print','json',rtl])))
    proofs=tie_function_proof(h,profile,lib,tools['yosys'])
    text=mapped.read_text();removed=[]
    # This verification-only representation change explicitly exposes literal
    # nets so the importer's tie-insertion path is exercised. CEC checks it.
    for role,value in [('tie_high',1),('tie_low',0)]:
        tie=profile[role]
        pattern=re.compile(r'(?m)^\s*'+re.escape(tie['master'])+r'\s+\S+\s*\(\s*\.'+re.escape(tie['pin'])+r'\(([^)]+)\)\s*\);\s*$')
        match=pattern.search(text)
        require(match is not None,'expected native tie cell was not found')
        wire=match[1].strip();removed.append(dict(master=tie['master'],pin=tie['pin'],net=wire,value=value))
        text=text[:match.start()]+"  assign "+wire+" = 1'b"+str(value)+";\n"+text[match.end():]
    literal.write_text(text)
    external_cec(h,tag+'_literal_cec',tools['yosys'],lib,rtl,literal)
    # Site and units are asserted by the native LEF binder; use exact PDK values.
    sky=tag=='sky130';u=1000 if sky else 2000;sw=460 if sky else 1120;sh=2720 if sky else 7840
    site='unithd' if sky else 'GF018hv5v_mcu_sc7'
    floor=h.root/(tag+'_floor.json')
    write_json(floor,dict(name='lane_smoke',units_per_micron=u,die_area=dict(lx=0,ly=0,ux=125*sw,uy=18*sh),core_area=dict(lx=20*sw,ly=5*sh,ux=120*sw,uy=13*sh),site=dict(name=site,width=sw,height=sh),rows=[dict(x_start=20*sw,x_end=120*sw,y=(5+i)*sh)for i in range(8)]))
    ws=h.root/(tag+'_physical');road=[tools['rustroad'],'--workspace',ws,'--threads','2','--max-cells','2000','--max-rows','128']
    imported=h.json_command(tag+'_import',list(map(str,road+['import-pdk','--base-rev','rev0','--tech-lef',Path(profile['technology_lef']['path']),'--cell-lef',Path(profile['cell_lef']['path']),'--netlist',literal,'--floorplan',floor,'--physical-config',config])))
    placed=h.json_command(tag+'_place',list(map(str,road+['place','--base-rev',imported['revision']])))
    exported=h.root/(tag+'_exported.v');definition=h.root/(tag+'.def');physical=h.root/(tag+'_powered.v')
    for label,args,path in [('logical',['export-verilog'],exported),('def',['export-def'],definition),('powered',['export-verilog','--include-power'],physical)]:
        receipt=run_ok(h,tag+'_export_'+label,road+args);path.write_bytes(Path(receipt['stdout']).read_bytes())
    cec=external_cec(h,tag+'_post_cec',tools['yosys'],lib,rtl,exported)
    native=h.json_command(tag+'_native_cec',list(map(str,base+['check','synth','--top','lane_smoke','--lib',lib,'--netlist',exported,'--budget','10000','--model-budget','10000','--sweep-max-candidates','256','--print','json',rtl])))
    require(native.get('verdict')=='equivalent','post-physical native CEC did not prove')
    simulation=semantic_simulation(h,tag,tools,profile,rtl,exported)
    data=capture_power_db(h,tag,profile,definition)
    design=json.loads(floor.read_text());data['expected_rows']=design['rows'];data['site_height']=design['site']['height'];data['site_width']=design['site']['width']
    write_json(h.root/(tag+'_power_geometry.json'),data)
    tie_counts={}
    for role in ('tie_high','tie_low'):
        choice=profile[role]
        linked=[i for i in data['instances'] if i['master']==choice['master'] and any(p['name']==choice['pin'] and p['net'] is not None for p in i['pins'])]
        require(linked,'configured '+role+' cell output not physically instantiated')
        tie_counts[role]=len(linked)
    powered_topology=verify_powered_netlist(h,tag,tools['yosys'],physical,data)
    geometry=check_power_geometry(data,profile)
    require(geometry['signal_io']==20 and len(data['io'])==22,'logical/PG I/O coverage differs from source RTL and config')
    controls=geometry_controls(data,profile)
    try:
        projected=h.root/(tag+'_timing_ideal.lib')
        projection=ideal_projection(lib,projected)
        sdc=h.root/(tag+'_timing.sdc')
        sdc.write_text("create_clock -name core -period 10 [get_ports clk]\nset_input_delay 0.2 -clock core [get_ports {en a b}]\nset_output_delay 0.2 -clock core [get_ports {y z high_bit low_bit}]\nset_input_transition 0.1 [get_ports {clk en a b}]\nset_clock_transition 0.1 [get_clocks core]\nset_load 0.01 [get_ports {y z high_bit low_bit}]\n")
        timing=sta_flow(h,tag+'_timing',tools,projected,exported,sdc)
        raw_report=json.loads(bounded_text(h.root/'receipts'/(tag+'_timing_report')/'stdout.txt'))
        uncovered=raw_report['sections']['unconstrained']['rows']
        tie_nets={p['net'] for i in data['instances'] for p in i['pins'] if p['net']is not None and any(i['master']==profile[r]['master'] and p['name']==profile[r]['pin']for r in ('tie_high','tie_low'))}
        constant_inputs={i['name']+'/'+p['name']for i in data['instances']for p in i['pins']if p['direction']=='input' and p['net']in tie_nets}
        require({p['pin']for p in uncovered}==constant_inputs and all(p['reason']=='no_arrival'for p in uncovered),'unexpected unconstrained timing endpoint')
        differential=compare_sta(h,tag+'_opensta',tools['opensta'],projected,exported,sdc,timing)
        timing_result=dict(status='passed',native=timing,opensta=differential,projection=projection,unconstrained=uncovered,scope_note='constant-driven data pins have no data arrival; all other tested endpoints are constrained')
    except (RuntimeError,ValueError,TypeError,KeyError,OSError) as error:
        timing_result=dict(status='unverified',reason=str(error),scope='post-prepare logical netlist with tie cells; no extracted parasitics')
    for key in ('liberty','technology_lef','cell_lef'):
        require(sha256(Path(profile[key]['path']))==profile[key]['sha256'],'PDK input changed: '+key)
    return dict(pdk=tag,status='passed' if timing_result['status']=='passed' else 'unverified',synthesis=synthesis,tie_function_proofs=proofs,literal_projection=removed,imported=imported,placed=placed,
        native_cec=native['verdict'],external_cec=cec,simulation=simulation,tie_counts=tie_counts,powered_topology=powered_topology,power_geometry=geometry,geometry_controls=controls,reference_measurement=data['measurement'],timing=timing_result,
        artifacts={name:fingerprint(path)for name,path in [('source',rtl),('native_mapped',mapped),('literal_mapped',literal),('exported',exported),('powered',physical),('def',definition),('config',config),('floorplan',floor),('geometry',h.root/(tag+'_power_geometry.json'))]})


def main():
    import fcntl
    import resource
    from verify_logic import executable
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('workspace','base-rev','profiles','rustroad','rustsys','yosys','verilator','rusteda','ruststa','opensta'):
        parser.add_argument('--'+name,required=True)
    args=parser.parse_args()
    require(args.base_rev=='empty','REVISION_CONFLICT: fresh evidence workspace requires empty')
    args.threads,args.memory_mib,args.timeout_seconds,args.tolerance_ns=2,2048,30,0.001
    os.environ['GOMAXPROCS']='2'
    resource.setrlimit(resource.RLIMIT_AS,(8192*1024**2,8192*1024**2))
    root=Path(args.workspace).resolve();root.mkdir(parents=True,exist_ok=False)
    lock=(root/'workspace.lock').open('wb');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
    (root/'HEAD').write_text('rev0\n')
    h=PhysicalHarness(root,args)
    profiles=json.loads(bounded_text(Path(args.profiles),1024**2))
    require(profiles['schema']=='rustroad.power-io-profile.v1' and {p['tag']for p in profiles['pdks']}=={'sky130','gf180'},'dual-PDK profiles required')
    tools={n:executable(getattr(args,n))for n in ('rustroad','rustsys','yosys','verilator','rusteda','ruststa','opensta')}
    summary=dict(schema='rustroad.power-io-verification.v1',status='unverified',source=fingerprint(Path(__file__).resolve()),profiles=fingerprint(Path(args.profiles).resolve()),tools={},pdks=[],
        analysis_scope=dict(corner=None,wire_model=None,unconstrained_endpoints=None),scope=dict(cts='not_run',routing='not_run',extracted_sta='not_run',drc='unchecked',lvs='unchecked',ir_drop='unchecked',electromigration='unchecked'))
    memory=run_ok(h,'memory_preflight',['free','-h']);summary['memory_preflight']=bounded_text(Path(memory['stdout']))
    write_json(root/'preflight.json',summary)
    try:
        for name,tool in tools.items():
            require(tool is not None,'missing tool '+name)
            result=run_ok(h,name+'_version',[tool,'-V' if name=='yosys' else '-version' if name=='opensta' else '--version'])
            summary['tools'][name]={**fingerprint(tool),'version':bounded_text(Path(result['stdout'])).strip()}
        if shutil.which('podman') is None:raise Unverified('podman unavailable')
        image=h.run('openroad_image',['podman','image','exists',IMAGE])
        if image['exit_code']!=0:raise Unverified('pinned OpenROAD image probe failed')
        ref=run_ok(h,'openroad_binary',['podman','run','--rm','--pull=never','--network=none','--read-only','--memory=2g','--memory-swap=2g','--entrypoint=/usr/bin/sha256sum',IMAGE,OPENROAD])
        digest=bounded_text(Path(ref['stdout'])).split()[0]
        require(re.fullmatch('[a-f0-9]{64}',digest),'missing OpenROAD binary digest')
        summary['openroad_reference']=dict(image_id=IMAGE,executable=OPENROAD,sha256=digest)
        for profile in profiles['pdks']:
            try:summary['pdks'].append(run_power_case(h,profile,tools))
            except Unverified as error:summary['pdks'].append(dict(pdk=profile['tag'],status='unverified',reason=str(error)))
            except (RuntimeError,ValueError,KeyError,OSError) as error:summary['pdks'].append(dict(pdk=profile['tag'],status='failed',reason=str(error)))
        states=[p['status']for p in summary['pdks']]
        summary['status']='failed' if 'failed'in states else 'unverified' if 'unverified'in states else 'passed'
    except (RuntimeError,ValueError,KeyError,OSError) as error:summary['reason']=str(error)
    finally:
        summary['identity_stable']=all(sha256(Path(t['path']))==t['sha256']for t in summary['tools'].values())
        if not summary['identity_stable']:summary['status']='failed'
        summary['receipts']=h.receipts
        known=[r['peak_rss_kib']for r in h.receipts if r['peak_rss_kib']is not None]
        summary['peak_host_process_rss_kib']=max(known,default=None)
        write_json(root/'summary.json',summary);(root/'HEAD').write_text('rev1\n')
    print(json.dumps(dict(status=summary['status'],summary=str(root/'summary.json'))))
    return {'passed':0,'failed':1,'unverified':3}[summary['status']]


if __name__=='__main__':
    sys.exit(main())
