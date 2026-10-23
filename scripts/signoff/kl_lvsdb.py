# SPDX-License-Identifier: Apache-2.0
# KLayout batch macro: klayout -b -r kl_lvsdb.py -rd db_path=X.lvsdb -rd out=Y.json
# Dumps the LVS cross-reference of a KLayout .lvsdb as JSON: every circuit
# pair with its status, every pin pair (layout name <-> schematic name) of
# every circuit pair, and (bounded) every non-matching net/pin/device/
# subcircuit pair. KLayout pairs pins by topology, so a symmetric design with
# swapped port labels still reports Match; the host harness therefore compares
# the paired pin NAMES itself. A verdict is computed by the host, not here.
import json
import pya

MAX_DETAILS = 400
MAX_PIN_PAIRS = 100000
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
    return ""


def count(circuit, method):
    if circuit is None:
        return None
    return sum(1 for _ in getattr(circuit, method)())


result = {"db_path": db_path, "circuits": [], "details": [], "details_truncated": False,
          "pin_pairs_truncated": False, "pin_pairs_dumped": True}
details = 0
pin_pairs = 0
for cp in xref.each_circuit_pair():
    a, b = cp.first(), cp.second()
    status = STATUS.get(cp.status(), str(cp.status()))
    entry = {"layout": a.name if a else None, "schematic": b.name if b else None, "status": status,
             "layout_devices": count(a, "each_device"), "schematic_devices": count(b, "each_device"),
             "layout_nets": count(a, "each_net"), "schematic_nets": count(b, "each_net"),
             "layout_pins": count(a, "each_pin"), "schematic_pins": count(b, "each_pin"),
             "layout_subcircuits": count(a, "each_subcircuit"),
             "schematic_subcircuits": count(b, "each_subcircuit"), "pin_pairs": []}
    for pair in xref.each_pin_pair(cp):
        pin_pairs += 1
        if pin_pairs > MAX_PIN_PAIRS:
            result["pin_pairs_truncated"] = True
            break
        entry["pin_pairs"].append({"layout": name(pair.first()), "schematic": name(pair.second()),
                                   "status": STATUS.get(pair.status(), str(pair.status()))})
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
