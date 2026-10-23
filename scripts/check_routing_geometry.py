"""Run in external KLayout. Bounded geometry oracle from OpenDB data only."""
import copy
import json
import os
from pathlib import Path
import pya
ROOT=Path(__file__).resolve().parents[1]


def require(value,message):
    if not value:raise ValueError(message)


def check(data,routes,profile,scope):
    config=json.loads((ROOT/profile['routing_config']).read_text())
    layers={x['name']:x['spacing'] for x in config['layers']}
    groups={};drawn={};cuts={};cut_spacings={};work=0
    def charge(n=1):
        nonlocal work
        work+=n;require(work<=5_000_000,'geometry comparison budget')
    def add(table,net,layer,rect):
        if layer in layers:table.setdefault((net,layer),pya.Region()).insert(pya.Box(*rect))
    def transform(rect,inst):
        a,b,c,d=rect
        if inst['orientation'] in ('MX','FS'):b,d=inst['height']-d,inst['height']-b
        else:require(inst['orientation'] in ('N','R0'),'orientation unsupported')
        return [a+inst['x'],b+inst['y'],c+inst['x'],d+inst['y']]
    terminals={};obstacles={}
    for inst in data['instances']:
        for pin in inst['pins']:
            if pin['net'] is None:continue
            boxes=[(s['layer'],transform(s['rect'],inst)) for s in pin['shapes'] if s['layer'] in layers]
            terminals[(inst['name'],pin['name'])]=(pin['net'],boxes)
            for l,r in boxes:add(groups,pin['net'],l,r)
        for shape in routes['obstructions'].get(inst['master'],[]):
            l=shape['layer']
            if l in layers:obstacles.setdefault(l,pya.Region()).insert(pya.Box(*transform(shape['rect'],inst)))
    for pin in data['io']:
        boxes=[(s['layer'],s['rect']) for s in pin['shapes'] if s['layer'] in layers]
        terminals[('PORT',pin['name'])]=(pin['net'],boxes)
        for l,r in boxes:add(groups,pin['net'],l,r)
    selected={n for n,c in routes['net_terminals'].items() if c>=2}
    if scope=='clock':
        cfg=json.loads((ROOT/profile['config']).read_text())['clock']
        selected={p['net'] for p in data['io'] if p['name']==cfg['port']}
        selected|={p['net'] for i in data['instances'] if i['master']==cfg['cell'] for p in i['pins'] if p['name']==cfg['output_pin'] and p['net'] is not None}
    all_vias=routes['vias']+data['vias']
    for wire in routes['wires']+data['wires']:
        require(len(wire['rect'])==4 and wire['rect'][0]<wire['rect'][2] and wire['rect'][1]<wire['rect'][3],'invalid routed rectangle')
        add(groups,wire['net'],wire['layer'],wire['rect'])
    for wire in routes['wires']:
        require(wire['net'] in selected,'unexpected routed net')
        box=wire['rect'];die=data['die']
        require(die[0]<=box[0]<box[2]<=die[2] and die[1]<=box[1]<box[3]<=die[3],'route leaves die')
        require(min(box[2]-box[0],box[3]-box[1])>=routes['layer_widths'][wire['layer']],'route below default width')
        add(drawn,wire['net'],wire['layer'],box)
    policies={v['name']:v['cut_spacing'] for v in config['vias']}
    # LEF-rule profiles also route single-cut LEF variants of a policy via;
    # such a variant takes its cut layer's policy spacing (else the largest).
    layer_policy={}
    for via in routes['vias']+data['vias']:
        if via['name'] in policies:
            for shape in via['shapes']:
                if shape['layer'] not in layers:
                    layer_policy[shape['layer']]=max(layer_policy.get(shape['layer'],0),policies[via['name']])
    for via,is_new in [(v,True) for v in routes['vias']]+[(v,False) for v in data['vias']]:
        for shape in via['shapes']:
            l,r=shape['layer'],shape['rect']
            if l in layers:
                add(groups,via['net'],l,r)
                if is_new:add(drawn,via['net'],l,r)
            else:
                cuts.setdefault(l,{})[tuple(r)]=via['net']
                require(via['name'] in policies or config.get('lef_rules'),'VIA policy missing')
                spacing=policies[via['name']] if via['name'] in policies else layer_policy.get(l,max(policies.values()))
                cut_spacings[l]=max(cut_spacings.get(l,0),spacing)
    for key in groups:groups[key]=groups[key].merged()
    for (net,layer),r in drawn.items():
        charge()
        require((r & obstacles.get(layer,pya.Region())).is_empty(),'routing intersects LEF obstruction')
        grown=r.sized(layers[layer])
        for (other,l),target in groups.items():
            if l==layer and other!=net:
                charge();require((grown & target).is_empty(),'different-net short or spacing violation')
    # Single-cut fixed vias in these profiles. Check same-net cuts as well.
    for layer,entries in cuts.items():
        cut_spacing=cut_spacings[layer]
        rects=list(entries)
        for i,a in enumerate(rects):
            for b in rects[:i]:
                charge();dx=max(a[0]-b[2],b[0]-a[2],0);dy=max(a[1]-b[3],b[1]-a[3],0)
                require(dx*dx+dy*dy>=cut_spacing*cut_spacing,'via cut spacing violation')
    parent=[];components={}
    def find(i):
        while parent[i]!=i:parent[i]=parent[parent[i]];i=parent[i]
        return i
    def union(ids):
        if ids:
            a=find(ids[0])
            for i in ids[1:]:parent[find(i)]=a
    for (net,layer),region in groups.items():
        if net not in selected:continue
        for poly in region.each():
            charge();require(len(parent)<100000,'component count budget')
            idx=len(parent);parent.append(idx);components.setdefault((net,layer),[]).append((idx,pya.Region(poly)))
    def touching(net,boxes):
        ids=[]
        for layer,rect in boxes:
            shape=pya.Region(pya.Box(*rect))
            for idx,poly in components.get((net,layer),[]):
                charge()
                if not (shape & poly).is_empty():ids.append(idx)
        return ids
    for via in all_vias:
        if via['net'] in selected:union(touching(via['net'],[(s['layer'],s['rect']) for s in via['shapes'] if s['layer'] in layers]))
    pin_count=0
    for net,boxes in terminals.values():
        if net not in selected:continue
        ids=touching(net,boxes);require(ids,'routed terminal not geometrically contacted');union(ids);pin_count+=1
    for net in selected:
        roots={find(idx) for (n,_),parts in components.items() if n==net for idx,_ in parts}
        require(len(roots)==1,'disconnected signal/clock conductor network')
    return dict(status='passed',routed_nets=len(selected),terminals=pin_count,wires=len(routes['wires']),vias=len(routes['vias']),
                comparison_work=work,components=len(parent),subset_drc_status='passed',full_drc_status='unchecked')


request=Path(os.environ['RUSTROAD_ROUTING_REQUEST'])
require(request.stat().st_size<8*1024**2,'request byte budget')
r=json.loads(request.read_text());result=check(r['geometry'],r['routes'],r['profile'],r['scope'])
controls={}
for mode in ('missing_net_route','supply_short'):
    bad=copy.deepcopy(r['routes']);net=next(w['net'] for w in bad['wires'])
    if mode=='missing_net_route':
        bad['wires']=[w for w in bad['wires'] if w['net']!=net];bad['vias']=[v for v in bad['vias'] if v['net']!=net]
    else:
        w=copy.deepcopy(r['geometry']['wires'][0]);w['net']=net;bad['wires'].append(w)
    try:check(r['geometry'],bad,r['profile'],r['scope'])
    except ValueError:controls[mode]='rejected'
    else:raise ValueError('negative routing control accepted: '+mode)
result['negative_controls']=controls
Path(r['output']).write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result))
