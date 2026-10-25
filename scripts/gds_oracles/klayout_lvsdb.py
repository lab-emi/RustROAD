# Machine-readable KLayout LVS cross-reference verdict (black box).
# klayout -b -r klayout_lvsdb.py -rd db_path=<lvsdb> -rd out=<json>
import json
import pya

lvs = pya.LayoutVsSchematic()
lvs.read(db_path)
xref = lvs.xref()
circuits = []
mismatches = []
for pair in xref.each_circuit_pair():
    a, b = pair.first(), pair.second()
    status = str(pair.status())
    circuits.append([a.name if a else None, b.name if b else None, status])
    if pair.status() != pya.NetlistCrossReference.Match:
        for kind, items in (('net', xref.each_net_pair(pair)), ('pin', xref.each_pin_pair(pair)),
                            ('device', xref.each_device_pair(pair)),
                            ('subcircuit', xref.each_subcircuit_pair(pair))):
            for item in items:
                if item.status() != pya.NetlistCrossReference.Match:
                    x, y = item.first(), item.second()
                    mismatches.append([kind, x.expanded_name() if x else None,
                                       y.expanded_name() if y else None, str(item.status())])
with open(out, 'w') as handle:
    json.dump(dict(circuits=circuits, mismatches=mismatches), handle, indent=1)
print('LVSDB_CIRCUITS', len(circuits), 'MISMATCHES', len(mismatches))
