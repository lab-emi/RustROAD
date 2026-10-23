"""Run with KLayout -b -r. Independent bounded GDS contact/well oracle.

Input path comes from RUSTROAD_TAP_REQUEST. All inspected objects are standard
PDK layout data and public KLayout APIs, not reference-tool implementation.
Resource estimate and scope: docs/tap-budget.md.
"""
from decimal import Decimal
import json
import os
import re
from pathlib import Path
import xml.etree.ElementTree as ET
import pya

MAX_SHAPES = 100_000
MAX_WORK = 2_000_000


def require(ok, message):
    if not ok:
        raise ValueError(message)


def main():
    path = Path(os.environ["RUSTROAD_TAP_REQUEST"])
    require(path.stat().st_size <= 2 * 1024**2, "request byte limit")
    request = json.loads(path.read_text())
    profile, data = request["profile"], request["geometry"]
    reference = profile["tap_reference"]
    names = reference["layers"]
    properties = ET.parse(reference["layer_properties"]["path"])
    mapping = {}
    for entry in properties.iter():
        source, name = entry.findtext("source"), entry.findtext("name")
        if source:
            match=re.search(r"(?:^|\s)(\d+)/(\d+)@",source)
            if match:
                name=(name or source[:match.start()]).strip().split(" - ")[0]
                if name:mapping[name]=(int(match[1]),int(match[2]))
    layers = {role: mapping[name] if name is not None else None for role, name in names.items()}
    layout = pya.Layout()
    layout.read(reference["gds"]["path"])
    ratio = Decimal(str(layout.dbu)) * request["dbu"]
    require(ratio == ratio.to_integral_value() and ratio > 0, "GDS grid cannot convert exactly")
    scale = int(ratio)
    work, shapes = 0, 0
    cache = {}

    def charge(n=1):
        nonlocal work
        work += n
        require(work <= MAX_WORK, "GDS comparison/work limit")

    def region(name, role):
        nonlocal shapes
        key = (name, role)
        if key in cache:
            return cache[key]
        cell = layout.cell(name)
        require(cell is not None, "GDS cell missing: " + name)
        result = pya.Region()
        layer = layers[role]
        if layer is not None:
            li = layout.find_layer(*layer)
            if li is not None and li >= 0:
                it = cell.begin_shapes_rec(li)
                while not it.at_end():
                    shapes += 1
                    require(shapes <= MAX_SHAPES, "GDS shape budget")
                    shape = it.shape()
                    poly = pya.Polygon(shape.box) if shape.is_box() else shape.polygon if shape.is_polygon() else shape.path.polygon() if shape.is_path() else None
                    if poly is not None:
                        result.insert(poly.transformed(it.trans()))
                    it.next()
        result = result.transformed(pya.ICplxTrans(scale, 0, False, 0, 0)).merged()
        cache[key] = result
        return result

    def selected_components(all_region, touching):
        result = pya.Region()
        for poly in all_region.each():
            charge()
            component = pya.Region(poly)
            if not (component & touching).is_empty():
                result += component
        return result

    tap_name = reference["master"]
    tap_instances = [i for i in data["instances"] if i["master"] == tap_name]
    require(tap_instances, "no actual tap instances")
    exemplar = tap_instances[0]
    power = pya.Region()
    ground = pya.Region()
    for pin in exemplar["pins"]:
        if pin["name"] not in profile["primary_pins"]:
            continue
        target = power if profile["pin_roles"][pin["name"]] == profile["power_net"] else ground
        for shape in pin["shapes"]:
            if shape["layer"] == profile["rail_layer"]:
                target.insert(pya.Box(*shape["rect"]))
    require(not power.is_empty() and not ground.is_empty(), "tap primary rails absent")
    well, active, contact = region(tap_name, "well"), region(tap_name, "tap"), region(tap_name, "contact")
    nactive = active & well & region(tap_name, "nimplant")
    pactive = (active - well) & region(tap_name, "pimplant")
    if layers["pwell"] is not None:
        pactive &= region(tap_name, "pwell")

    def prove_contacts(contact_region):
        contacts = {"nwell": contact_region & nactive, "substrate": contact_region & pactive}
        counts = {}
        for kind, selected in contacts.items():
            require(not selected.is_empty(), "missing " + kind + " tap contacts")
            count = 0
            for polygon in selected.each():
                charge()
                count += 1
                initial = pya.Region(polygon)
                if layers["local"] is not None:
                    local = selected_components(region(tap_name, "local"), initial)
                    cut = region(tap_name, "mcon") & local
                    require(not cut.is_empty(), "tap lacks local-to-metal contact")
                    metal = selected_components(region(tap_name, "metal"), cut)
                else:
                    metal = selected_components(region(tap_name, "metal"), initial)
                wanted, wrong = (power, ground) if kind == "nwell" else (ground, power)
                require(not (metal & wanted).is_empty(), "tap contact is not connected to required metal rail")
                require((metal & wrong).is_empty(), "tap contact reaches opposite supply rail")
            counts[kind] = count
        return contacts, counts

    contacts, contact_counts = prove_contacts(contact)
    try:
        prove_contacts(pya.Region())
    except ValueError:
        contact_negative = "rejected"
    else:
        raise ValueError("missing-contact control was accepted")

    def transform(instance):
        require(instance["orientation"] in ("N", "R0", "FS", "MX"), "unsupported GDS orientation")
        mirror = instance["orientation"] in ("FS", "MX")
        return pya.ICplxTrans(1, 0, mirror, instance["x"], instance["y"] + (instance["height"] if mirror else 0))

    all_well, all_pwell, all_active = pya.Region(), pya.Region(), pya.Region()
    active_input_cells=0
    for instance in data["instances"]:
        charge()
        name, trans = instance["master"], transform(instance)
        require(region(name, "deepwell").is_empty(), "deep-well case requires a separate contact policy")
        all_well += region(name, "well").transformed(trans)
        if layers["pwell"] is not None:
            all_pwell += region(name, "pwell").transformed(trans)
        cell_active=region(name,"active")
        if any(p["direction"]=="input" and p["use"] not in ("power","ground") for p in instance["pins"]):
            require(not cell_active.is_empty(),"logic cell has no inspected active GDS geometry")
            active_input_cells+=1
        all_active += cell_active.transformed(trans)
    require(active_input_cells>0,"no active logic cells inspected")
    all_well, all_active = all_well.merged(), all_active.merged()
    if layers["pwell"] is None:
        all_pwell = pya.Region(pya.Box(*data["die"])) - all_well
    else:
        all_pwell = all_pwell.merged()
    require(((all_active - all_well) - all_pwell).is_empty(), "active diffusion outside modeled substrate/well")

    def coverage(instances, distance):
        global_contacts = {kind: pya.Region() for kind in contacts}
        for instance in instances:
            charge()
            # Explicit PG endpoint identity is independently checked by OpenDB.
            trans = transform(instance)
            for kind in contacts:
                global_contacts[kind] += contacts[kind].transformed(trans)
        components = {}
        for kind, body, devices in [("nwell", all_well, all_active & all_well), ("substrate", all_pwell, all_active - all_well)]:
            count = 0
            for polygon in body.each():
                charge()
                component = pya.Region(polygon)
                active_here = devices & component
                taps_here = global_contacts[kind] & component
                require(not taps_here.is_empty(), "uncontacted " + kind + " component")
                count+=1
                if active_here.is_empty():continue
                cover = pya.Region()
                for contact_poly in taps_here.each():
                    charge()
                    box = contact_poly.bbox()
                    require(contact_poly.area() == box.area(), "nonrectangular contact needs another distance checker")
                    l, b, r, t = box.left, box.bottom, box.right, box.top
                    d = distance
                    cover.insert(pya.Polygon([pya.Point(l-d,b),pya.Point(l,b-d),pya.Point(r,b-d),pya.Point(r+d,b),pya.Point(r+d,t),pya.Point(r,t+d),pya.Point(l,t+d),pya.Point(l-d,t)]))
                require((active_here - cover.merged()).is_empty(), kind + " active diffusion exceeds configured Manhattan tap distance")
            components[kind] = count
        return components

    components = coverage(tap_instances, reference["max_distance_dbu"])
    try:
        coverage([], reference["max_distance_dbu"])
    except ValueError:
        missing_taps = "rejected"
    else:
        raise ValueError("missing-tap control accepted")
    sparse = []
    by_row = {}
    for instance in tap_instances:
        by_row.setdefault(instance["y"], []).append(instance)
    for row in by_row.values():
        row.sort(key=lambda i: i["x"])
        sparse += [row[0], row[-1]] if len(row) > 1 else row
    try:
        coverage(sparse, reference["max_distance_dbu"])
    except ValueError:
        sparse_control = "rejected"
    else:
        raise ValueError("sparse end-only tap control accepted")
    result = {"schema":"rustroad.tap-gds.v1","status":"passed","tap_master":tap_name,
              "tap_instances":len(tap_instances),"contact_counts_per_tap":contact_counts,
              "body_components_contacted":components,"distance_metric":"Manhattan contact-rectangle coverage of all active polygons within each body component",
              "max_distance_dbu":reference["max_distance_dbu"],"dbu_per_micron":request["dbu"],"gds_scale":scale,
              "source_shapes":shapes,"active_input_cells_checked":active_input_cells,"work_units":work,"negative_controls":{"missing_contact":contact_negative,"missing_taps":missing_taps,"end_only_taps":sparse_control},
              "latchup_signoff":"unchecked","drc_status":"unchecked","scope":"standard single-well core cells; no deep-well, IO guard-ring, IR/EM or full-deck claim"}
    Path(request["output"]).write_text(json.dumps(result, indent=2)+"\n")
    print(json.dumps(result))


main()
