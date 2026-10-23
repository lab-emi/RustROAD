# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro: klayout -b -r kl_lvsdb.py -rd db_path=X.lvsdb -rd out=Y.json
# Dumps the LVS cross-reference of a KLayout .lvsdb as JSON: every circuit
# pair with its status, and (bounded) every non-matching net/pin/device/
# subcircuit pair. A verdict is computed by the host harness, not here.
import json
import pya

MAX_DETAILS = 400
lvs = pya.LayoutVsSchematic()
lvs.read(db_path)
xref = lvs.xref()
STATUS = {
    pya.NetlistCrossReference.Match: "Match",
    pya.NetlistCrossReference.MatchWithWarning: "MatchWithWarning",
    pya.NetlistCrossReference.NoMatch: "NoMatch",
    pya.NetlistCrossReference.Mismatch: "Mismatch",
    pya.NetlistCrossReference.Skipped: "Skipped",
    pya.NetlistCrossReference.None_: "None",
}


def name(obj):
    if obj is None:
        return None
    for attr in ("expanded_name", "name"):
        if hasattr(obj, attr):
            value = getattr(obj, attr)()
            if value:
                return value
    return str(obj)


def count(circuit, method):
    if circuit is None:
        return None
    return sum(1 for _ in getattr(circuit, method)())


result = {"db_path": db_path, "circuits": [], "details": [], "details_truncated": False}
details = 0
for cp in xref.each_circuit_pair():
    a, b = cp.first(), cp.second()
    status = STATUS.get(cp.status(), str(cp.status()))
    entry = {"layout": a.name if a else None, "schematic": b.name if b else None, "status": status,
             "layout_devices": count(a, "each_device"), "schematic_devices": count(b, "each_device"),
             "layout_nets": count(a, "each_net"), "schematic_nets": count(b, "each_net"),
             "layout_pins": count(a, "each_pin"), "schematic_pins": count(b, "each_pin"),
             "layout_subcircuits": count(a, "each_subcircuit"),
             "schematic_subcircuits": count(b, "each_subcircuit")}
    result["circuits"].append(entry)
    if status == "Match":
        continue
    for kind, iterator in (("net", xref.each_net_pair), ("pin", xref.each_pin_pair),
                           ("device", xref.each_device_pair), ("subcircuit", xref.each_subcircuit_pair)):
        for pair in iterator(cp):
            pstatus = STATUS.get(pair.status(), str(pair.status()))
            if pstatus == "Match":
                continue
            details += 1
            if details > MAX_DETAILS:
                result["details_truncated"] = True
                break
            result["details"].append({"circuit": entry["layout"] or entry["schematic"], "kind": kind,
                                      "layout": name(pair.first()), "schematic": name(pair.second()),
                                      "status": pstatus})
top_layout = lvs.internal_top_cell().name if lvs.internal_top_cell() else None
result["layout_top"] = top_layout
with open(out, "w") as handle:
    json.dump(result, handle, indent=1, sort_keys=True)
print("KL_LVSDB_DONE")
