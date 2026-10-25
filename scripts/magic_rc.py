#!/usr/bin/env python3
"""Magic as a second, independent black-box RC oracle (pinned signoff image).

For one routed workspace and one RC corner this script:

1. exports the native GDS, the DEF, the routes and the connectivity of HEAD
   (read-only RustROAD commands);
2. capacitance run: Magic reads the native GDS with the GDS via structures
   flattened on input (``gds flatglob VIA_*``), extracts the hierarchy once
   (the ``use`` transforms and a standalone extraction of every cell master)
   and then the flattened design. A net's routing capacitance is its flat
   node capacitance (substrate plus every coupling record) minus the
   standalone capacitance of its cell pin nodes, which Liberty already holds;
3. resistance run: Magic extracts a routing-only layout derived from the same
   GDS (cell instances removed, one small port terminal cell placed at every
   routed pin access point) with ``extract do resistance`` and zero
   thresholds. A net's resistance is the sum of its network resistors;
4. maps every Magic node to a native net through the cell instance placement
   (``use`` transform against the DEF component) and the pin names, and
   refuses on any unmapped, ambiguous or merged net;
5. writes ``magic_nets.json`` (per-net totals) and, on request, a SPEF that
   back-annotates the Magic per-net capacitance and resistance onto the
   native RC trees (topology from the native SPEF, every node capacitance and
   resistor of a net scaled by one factor). The back-annotation is how timing
   is compared against Magic; ``scripts/compare_spef.py`` shows with the
   OpenRCX SPEF that per-net totals carry the timing on these designs.

Corner styles follow the native profile's ``magic_variant`` ((lrlc) min, ()
nom, (hrhc) max), so Magic's capacitance comes from its own extraction rules
and is independent of the calibration tables the native extractor and OpenRCX
read; its resistance uses the same Magic sheet and contact values the native
profile records, so for resistance it checks geometry and topology only.

Usage:
  magic_rc.py extract --workspace WS --rustroad BIN --pdk sky130 --corner nom \
      --native-spef NATIVE.spef --out DIR [--backannotate OUT.spef]
  magic_rc.py controls --pdk sky130 --out DIR      (synthetic structures)

Magic, PDK files and the native outputs are used strictly as black boxes and
data. No reference source is read; nothing native is fitted to Magic.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE / "signoff"))
import container  # noqa: E402
import gds_records as gds  # noqa: E402

ROOT = HERE.parent
MAX_TEXT = 64 * 1024 * 1024
MAX_EXT_RECORDS = 4_000_000
PDKS = {
    "sky130": dict(tech="sky130A", prefix="sky130_fd_sc_hd__",
                   cell_lef="libs.ref/sky130_fd_sc_hd/lef/sky130_fd_sc_hd.lef"),
    "gf180": dict(tech="gf180mcuD", prefix="gf180mcu_fd_sc_mcu7t5v0__",
                  cell_lef="libs.ref/gf180mcu_fd_sc_mcu7t5v0/lef/gf180mcu_fd_sc_mcu7t5v0.lef"),
}
# Magic internal unit on both PDKs (checked against the DEF on every run).
MAGIC_UNIT_UM = 0.005

C_TCL = r"""
crashbackups stop
drc off
gds flatglob VIA_*
gds read /in/design.gds
load $::env(CELL)
select top cell
expand
set od $::env(OUTDIR)
file mkdir $od
cd $od
extract style $::env(STYLE)
extract do local
extract do capacitance
extract do coupling
extract no resistance
extract do adjust
extract all
flatten rr_flat_top
load rr_flat_top
extract all
puts "RR_C_DONE"
quit -noprompt
"""

R_TCL = r"""
crashbackups stop
drc off
gds flatglob VIA_*
gds read /in/design.gds
load $::env(CELL)
select top cell
expand
set od $::env(OUTDIR)
file mkdir $od
cd $od
extract style $::env(STYLE)
extract do local
extract do capacitance
extract no coupling
extract do resistance
extract do adjust
extresist threshold 0
extresist mindelay 0
extresist minresist 0
extresist simplify off
extresist blackbox on
extract all
puts "RR_FEEDBACK [feedback count]"
puts "RR_R_DONE"
quit -noprompt
"""


class MagicError(RuntimeError):
    pass


def require(ok: bool, message: str) -> None:
    if not ok:
        raise MagicError(message)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def text(path: Path) -> str:
    require(Path(path).is_file() and Path(path).stat().st_size <= MAX_TEXT, f"missing or oversized: {path}")
    return Path(path).read_text(errors="replace")


def manifest_pdk(pdk: str) -> dict:
    m = json.loads(text(ROOT / "fixtures/flow/signoff-pinned.json"))
    return m["pdks"][pdk]


# ----------------------------------------------------------------------------
# .ext parsing (Magic's documented extraction file format; bounded)
# ----------------------------------------------------------------------------
def parse_ext(path: Path) -> dict:
    nodes, caps, equiv, uses = {}, [], [], []
    substrate = style = None
    scale = None
    count = 0
    for line in text(path).splitlines():
        count += 1
        require(count <= MAX_EXT_RECORDS, f"{path}: record budget")
        t = line.split()
        if not t:
            continue
        key = t[0]
        if key == "scale":
            scale = [float(x) for x in t[1:4]]
        elif key == "style":
            style = t[1]
        elif key == "node":
            nodes[t[1].strip('"')] = dict(r=float(t[2]), c_af=float(t[3]))
        elif key == "substrate":
            substrate = t[1].strip('"')
        elif key == "cap":
            caps.append((t[1].strip('"'), t[2].strip('"'), float(t[3])))
        elif key == "equiv":
            equiv.append((t[1].strip('"'), t[2].strip('"')))
        elif key == "use":
            uses.append(dict(cell=t[1], id=t[2], transform=[int(x) for x in t[3:9]]))
    require(scale is not None, f"{path}: no scale record")
    require(scale[1] == 1.0, f"{path}: unexpected capacitance scale {scale}")
    return dict(nodes=nodes, caps=caps, equiv=equiv, uses=uses, substrate=substrate, scale=scale, style=style)


def canonical_map(ext: dict) -> dict:
    """alias -> canonical node name (equiv chains resolved)."""
    parent = {}

    def find(x):
        while parent.get(x, x) != x:
            x = parent[x]
        return x
    for a, b in ext["equiv"]:
        ra, rb = find(a), find(b)
        if ra != rb:
            # Prefer the name that carries a node record.
            if rb in ext["nodes"] and ra not in ext["nodes"]:
                ra, rb = rb, ra
            parent[rb] = ra
    return {x: find(x) for x in list(parent) + list(ext["nodes"])}


def node_totals(ext: dict) -> tuple[dict, dict]:
    """Per canonical node: total capacitance (aF) and per-partner coupling.
    Sums are exactly rounded (math.fsum), so Magic's record order (which can
    differ between identical runs) does not change any value."""
    canon = canonical_map(ext)
    c = lambda n: canon.get(n, n)  # noqa: E731
    total = defaultdict(list)
    partners = defaultdict(lambda: defaultdict(list))
    for name, rec in ext["nodes"].items():
        total[c(name)].append(rec["c_af"])
        partners[c(name)]["__substrate__"].append(rec["c_af"])
    for a, b, v in ext["caps"]:
        a, b = c(a), c(b)
        if a == b:
            continue
        total[a].append(v)
        total[b].append(v)
        partners[a][b].append(v)
        partners[b][a].append(v)
    return ({k: math.fsum(v) for k, v in total.items()},
            {k: {x: math.fsum(y) for x, y in sorted(v.items())} for k, v in partners.items()})


def parse_res_ext(path: Path) -> list[dict]:
    """Network blocks of a .res.ext: [{nodes: [names], points: [(name, x, y)],
    resistors: [(a, b, ohm)]}] (coordinates in Magic internal units)."""
    blocks, current, last = [], None, None
    count = 0
    for line in text(path).splitlines():
        count += 1
        require(count <= MAX_EXT_RECORDS, f"{path}: record budget")
        t = line.split('"')
        head = line.split()[0] if line.split() else ""
        if head == "rnode":
            if current is None or last == "resist":
                current = dict(nodes=[], points=[], resistors=[])
                blocks.append(current)
            current["nodes"].append(t[1])
            rest = t[2].split()
            current["points"].append((t[1], int(rest[2]), int(rest[3])))
            last = "rnode"
        elif head == "resist":
            require(current is not None, f"{path}: resist before rnode")
            current["resistors"].append((t[1], t[3], float(t[4].split()[0])))
            last = "resist"
    return blocks


# ----------------------------------------------------------------------------
# Inputs from the workspace (read-only commands)
# ----------------------------------------------------------------------------
def rustroad(binary: Path, workspace: Path, args: list[str], out: Path) -> None:
    command = [str(binary), "--workspace", str(workspace), "--threads", "2", "--max-cells", "250000",
               "--max-rows", "100000", *args]
    with out.open("wb") as handle:
        result = subprocess.run(command, stdout=handle, stderr=subprocess.PIPE, timeout=600, check=False)
    require(result.returncode == 0, f"rustroad {args[0]} failed: {result.stderr.decode()[-2000:]}")


def export_inputs(binary: Path, workspace: Path, out: Path, pdk: str) -> dict:
    out.mkdir(parents=True, exist_ok=True)
    paths = dict(gds=out / "design.gds", def_=out / "design.def", routes=out / "routes.json",
                 conn=out / "connectivity.json")
    spec = manifest_pdk(pdk)
    cell_gds = Path(spec["host_root"]) / spec["files"]["cell_gds"]["path"]
    require(sha256(cell_gds) == spec["files"]["cell_gds"]["sha256"], "cell GDS differs from the manifest")
    rustroad(binary, workspace, ["export-gds", "--cell-gds", str(cell_gds), "--layer-map",
                                 str(ROOT / f"fixtures/pdk/{pdk}-gds-map.json"), "--out", str(paths["gds"])],
             out / "export-gds.json")
    rustroad(binary, workspace, ["export-def"], paths["def_"])
    rustroad(binary, workspace, ["export-routes"], paths["routes"])
    rustroad(binary, workspace, ["export-connectivity"], paths["conn"])
    head = (workspace / "HEAD").read_text().strip()
    paths["design"] = workspace / "revisions" / head / "design.json"
    return paths


def def_components(def_text: str) -> tuple[int, dict]:
    units = int(re.search(r"UNITS DISTANCE MICRONS (\d+)", def_text)[1])
    body = def_text.split("\nCOMPONENTS ", 1)[1].split("\nEND COMPONENTS", 1)[0]
    comps = {}
    for m in re.finditer(r"^\s*- (\S+) (\S+)\s+\+ (?:PLACED|FIXED) \( (-?\d+) (-?\d+) \) (\S+)", body, re.M):
        comps[m[1]] = dict(master=m[2], x=int(m[3]), y=int(m[4]), orient=m[5])
    return units, comps


def lef_sizes(lef_text: str) -> dict:
    sizes, current = {}, None
    for line in lef_text.splitlines():
        t = line.split()
        if t[:1] == ["MACRO"]:
            current = t[1]
        elif t[:1] == ["SIZE"] and current:
            sizes[current] = (float(t[1]), float(t[3]))
    return sizes


def map_instances(uses: list[dict], comps: dict, sizes: dict, dbu: int) -> dict:
    """Magic instance id -> DEF component, by master and placed lower-left corner."""
    by_corner = {}
    for name, c in comps.items():
        key = (c["master"], c["x"], c["y"])
        require(key not in by_corner, f"two DEF components at one corner: {key}")
        by_corner[key] = name
    out = {}
    for u in uses:
        if u["cell"] not in sizes:
            continue
        w, h = sizes[u["cell"]]
        wd, hd = w / MAGIC_UNIT_UM, h / MAGIC_UNIT_UM
        a, b, c, d, e, f = u["transform"]
        xs = [a * px + b * py + c for px, py in ((0, 0), (wd, 0), (0, hd), (wd, hd))]
        ys = [d * px + e * py + f for px, py in ((0, 0), (wd, 0), (0, hd), (wd, hd))]
        key = (u["cell"], round(min(xs) * MAGIC_UNIT_UM * dbu), round(min(ys) * MAGIC_UNIT_UM * dbu))
        require(key in by_corner, f"Magic instance {u['id']} has no DEF component at {key}")
        out[u["id"]] = by_corner[key]
    require(len(set(out.values())) == len(out), "two Magic instances map to one DEF component")
    return out


def pin_table(conn: dict, design: dict) -> dict:
    """connectivity pin index -> (net name, instance or None, pin or port name)."""
    cells = design["design"]["cells"]
    out = {}
    for k, p in enumerate(conn["pins"]):
        net = conn["nets"][p["net"]]["name"]
        owner = p["owner"]
        if owner["kind"] == "port":
            port = conn["ports"][owner["port"]]
            out[k] = (net, None, port["bits"][owner["bit"]])
        else:
            i = owner["instance"]
            master = conn["masters"][conn["instance_masters"][i]]
            out[k] = (net, cells[i]["name"], master["pins"][owner["terminal"]]["name"])
    return out


# ----------------------------------------------------------------------------
# Routing-only terminal layout for the resistance run
# ----------------------------------------------------------------------------
def terminal_gds(src: Path, dst: Path, pdk: str, routes: dict, design: dict, profile: dict) -> dict:
    lib = gds.Library(src)
    prefix = PDKS[pdk]["prefix"]
    top = design["design"]["name"]
    dbu = int(design["design"]["units_per_micron"])
    require(dbu % 1000 == 0, "DEF units must be a multiple of the 0.001 um GDS unit")
    scale = dbu // 1000
    gmap = json.loads(text(ROOT / f"fixtures/pdk/{pdk}-gds-map.json"))
    layer_map = {entry["lef"]: entry for entry in gmap["layers"]}
    width = {l["name"]: round(l["width_um"] * 1000) for l in profile["layers"]}
    keep = [s for s in lib.structures if not s["name"].startswith(prefix)]
    tops = [s for s in keep if s["name"] == top]
    require(len(tops) == 1, f"top structure {top} not found once")
    t = tops[0]
    before = len(t["elements"])
    t["elements"] = [e for e in t["elements"]
                     if not (e["kind"] in (gds.SREF, gds.AREF) and e["sname"].startswith(prefix))]
    removed = before - len(t["elements"])
    names = set(lib.names())
    terminals = {}
    for a in routes["routes"]["access"]:
        layer = routes["layers"][a["layer"]]
        entry = layer_map[layer]
        half = width[layer] // 2
        elements = [gds.boundary(entry["net"][0][0], entry["net"][0][1], (-half, -half, half, half))]
        for lay, dt in entry["pin"]:
            elements.append(gds.boundary(lay, dt, (-half, -half, half, half)))
        elements.append(gds.text(entry["label"][0], entry["label"][1], (0, 0), "P"))
        name = f"RRT_{a['pin']}"
        require(name not in names, f"terminal structure name collides: {name}")
        require(a["at"][0] % scale == 0 and a["at"][1] % scale == 0, "access point off the GDS grid")
        keep.append(gds.structure(name, elements))
        t["elements"].append(gds.sref(name, (a["at"][0] // scale, a["at"][1] // scale)))
        terminals[name] = a["pin"]
    lib.structures = keep
    lib.write(dst)
    return dict(terminals=terminals, removed_cell_instances=removed, gds=str(dst), sha256=sha256(dst))


# ----------------------------------------------------------------------------
# Magic runs (pinned signoff image, guarded container)
# ----------------------------------------------------------------------------
def run_magic(out: Path, label: str, pdk: str, gds_path: Path | None, cell: str, style: str, tcl: str,
              timeout: int = 900) -> tuple[dict, Path]:
    scripts = out / "tcl"
    scripts.mkdir(parents=True, exist_ok=True)
    (scripts / f"{label}.tcl").write_text(tcl)
    tech = PDKS[pdk]["tech"]
    work = out / label
    if work.exists():
        shutil.rmtree(work)
    receipt = container.run(["/foss/tools/bin/magic", "-dnull", "-noconsole", "-rcfile",
                             f"/foss/pdks/{tech}/libs.tech/magic/{tech}.magicrc", f"/scripts/{label}.tcl"],
                            out_dir=out, label=container.next_label(out, label),
                            ro=[(str(scripts), "/scripts")] + ([(str(gds_path), "/in/design.gds")] if gds_path else []),
                            memory="4g", cpus=2, threads=2, timeout=timeout,
                            env=dict(OUTDIR=f"/out/{label}", CELL=cell, STYLE=style))
    log = container.read_capture(receipt["stdout"])
    marker = "RR_C_DONE" if "RR_C_DONE" in tcl else "RR_R_DONE"
    require(receipt["exit_code"] == 0 and marker in log, f"Magic {label} did not finish; see receipt")
    return receipt, work


# ----------------------------------------------------------------------------
# Extraction
# ----------------------------------------------------------------------------
def native_nets(spef_path: Path) -> dict:
    sys.path.insert(0, str(HERE))
    from compare_spef import parse_spef  # noqa: PLC0415
    spef = parse_spef(spef_path)
    out = {}
    for name, net in spef["nets"].items():
        pins = [tuple(n.rsplit(":", 1)) for kind, n, d in net["conn"] if kind == "I"]
        ports = [n for kind, n, d in net["conn"] if kind == "P"]
        out[name] = dict(pins=pins, ports=ports)
    return out


def magic_variant(profile: dict) -> str:
    v = profile["magic_variant"]
    v = v if v.startswith("(") else f"({v})"
    return f"ngspice{v}"


def run_corner(inputs: dict, pdk: str, corner: str, native_spef: Path, out: Path,
               backannotate_to: Path | None = None, gds_override: Path | None = None,
               resistance: bool = True) -> dict:
    """Magic C (and R) of one corner from exported inputs (see export_inputs)."""
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    profile_path = ROOT / f"fixtures/pdk/{pdk}-rc-{corner}.json"
    profile = json.loads(text(profile_path))
    require(profile["corner"] == corner, "profile corner mismatch")
    style = magic_variant(profile)
    spec = manifest_pdk(pdk)
    host_pdk = Path(spec["host_root"])
    design = json.loads(text(inputs["design"]))
    top = design["design"]["name"]
    dbu, comps = def_components(text(inputs["def_"]))
    sizes = lef_sizes(text(host_pdk / PDKS[pdk]["cell_lef"]))
    routes = json.loads(text(inputs["routes"]))
    conn = json.loads(text(inputs["conn"]))
    pins = pin_table(conn, design)
    nets = native_nets(native_spef)
    source_gds = gds_override or inputs["gds"]
    record = dict(schema="rustroad.magic-rc.v1", pdk=pdk, corner=corner, style=style, top=top,
                  inputs={k: dict(path=str(v), sha256=sha256(v)) for k, v in inputs.items()},
                  gds=dict(path=str(source_gds), sha256=sha256(source_gds)),
                  native_spef=dict(path=str(native_spef), sha256=sha256(native_spef)),
                  profile=dict(path=str(profile_path), sha256=sha256(profile_path)),
                  image=container.SIGNOFF_IMAGE, receipts=[])
    # Capacitance run on the native GDS.
    receipt, cdir = run_magic(out, f"c_{corner}", pdk, source_gds, top, style, C_TCL)
    record["receipts"].append(dict(label=receipt["label"], wall_seconds=receipt["wall_seconds"],
                                   peak_rss_kib=receipt["peak_rss_kib"]))
    hier = parse_ext(cdir / f"{top}.ext")
    flat = parse_ext(cdir / "rr_flat_top.ext")
    require(hier["style"] == style and flat["style"] == style, f"Magic did not apply style {style}")
    imap = map_instances(hier["uses"], comps, sizes, dbu)
    inv = {v: k for k, v in imap.items()}
    require(set(inv) == set(comps), f"{len(set(comps) - set(inv))} DEF components without a Magic instance")
    flat_total, flat_partner = node_totals(flat)
    canon = canonical_map(flat)
    cn = lambda n: canon.get(n, n)  # noqa: E731
    # Standalone pin capacitance of every used master (Liberty already holds it).
    standalone = {}
    for master in sorted({c["master"] for c in comps.values()}):
        ext = parse_ext(cdir / f"{master}.ext")
        totals, _ = node_totals(ext)
        cmap = canonical_map(ext)
        standalone[master] = {pin: totals.get(cmap.get(pin, pin), 0.0) for pin in
                              {p for (_, _, p) in pins.values()} | set(cmap)}
    net_node = {}
    for name, info in nets.items():
        cands = {cn(p) for p in info["ports"]} | {cn(f"{inv[i]}.{p}") for i, p in info["pins"]}
        cands = {x for x in cands if x in flat_total}
        require(len(cands) == 1, f"net {name}: Magic nodes {sorted(cands)} (open, short or unmapped)")
        net_node[name] = cands.pop()
    require(len(set(net_node.values())) == len(net_node), "two native nets share one Magic node (short)")
    node_net = {v: k for k, v in net_node.items()}
    per_net = {}
    for name, node in net_node.items():
        partners = flat_partner.get(node, {})
        coupling = math.fsum(v for k, v in partners.items() if k in node_net)
        ground = flat_total[node] - coupling
        internal = math.fsum(standalone[comps[i]["master"]].get(p, 0.0) for i, p in nets[name]["pins"])
        per_net[name] = dict(flat_af=flat_total[node], flat_coupling_af=coupling, flat_ground_af=ground,
                             pin_internal_af=internal, c_pf=(flat_total[node] - internal) * 1e-6,
                             coupling_pf=coupling * 1e-6, magic_node=node,
                             couplings_pf={node_net[k]: v * 1e-6 for k, v in sorted(partners.items())
                                           if k in node_net})
    record["instances_mapped"] = len(imap)
    if resistance:
        # Resistance run on the routing-only terminal layout.
        tgds = out / f"terminal_{corner}.gds"
        term = terminal_gds(source_gds, tgds, pdk, routes, design, profile)
        record["terminal_layout"] = {k: v for k, v in term.items() if k != "terminals"}
        receipt, rdir = run_magic(out, f"r_{corner}", pdk, tgds, top, style, R_TCL)
        record["receipts"].append(dict(label=receipt["label"], wall_seconds=receipt["wall_seconds"],
                                       peak_rss_kib=receipt["peak_rss_kib"]))
        require(parse_ext(rdir / f"{top}.ext")["style"] == style, f"Magic did not apply style {style}")
        blocks = parse_res_ext(rdir / f"{top}.res.ext")
        # A terminal on a top-level port takes the port's label name in Magic.
        term_name = {k: (pins[k][2] if pins[k][1] is None else f"{name}_0/P")
                     for name, k in term["terminals"].items()}
        access_layer = {a["pin"]: routes["layers"][a["layer"]] for a in routes["routes"]["access"]}
        pin_net = {term_name[k]: pins[k][0] for k in term_name}
        name_pin = {v: k for k, v in term_name.items()}
        by_net = defaultdict(list)
        for b in blocks:
            owners = {pin_net[n] for n in b["nodes"] if n in pin_net}
            if not owners:
                continue
            require(len(owners) == 1, f"one Magic network spans nets {sorted(owners)}")
            by_net[owners.pop()].append(b)
        dbu = int(design["design"]["units_per_micron"])
        half = {l["name"]: l["width_um"] / 2.0 / MAGIC_UNIT_UM for l in profile["layers"]}
        access = {a["pin"]: a for a in routes["routes"]["access"]}

        def at_terminal(k: int, x: int, y: int) -> bool:
            a = access[k]
            cx, cy = a["at"][0] / dbu / MAGIC_UNIT_UM, a["at"][1] / dbu / MAGIC_UNIT_UM
            h = half[access_layer[k]] + 1.0
            return abs(x - cx) <= h and abs(y - cy) <= h
        for name in nets:
            found = by_net.get(name, [])
            require(found, f"net {name}: no Magic resistance network")
            sums = sorted(round(math.fsum(r for _, _, r in b["resistors"]), 6) for b in found)
            require(len(set(sums)) == 1, f"net {name}: differing duplicate networks {sums}")
            b = found[0]
            # Every routed pin of the net has a network node on its terminal
            # square (Magic may give that node the net's name instead).
            mine = sorted(k for k, v in pins.items() if v[0] == name and k in term_name)
            missing = [k for k in mine if not any(at_terminal(k, x, y) for _, x, y in b["points"])]
            require(not missing, f"net {name}: no Magic network node at the terminals of pins {missing}")
            counts = defaultdict(int)
            for n in b["nodes"]:
                counts[n] += 1
            elements = {}
            for k in mine:
                names = {n for n, x, y in b["points"] if at_terminal(k, x, y)}
                unique = [n for n in names if counts[n] == 1 and n == term_name[k]]
                if unique:
                    incident = [r for a, c, r in b["resistors"] if unique[0] in (a, c)]
                    elements[term_name[k]] = dict(pin=k, layer=access_layer[k], port=pins[k][1] is None,
                                                  incident_ohm=math.fsum(incident),
                                                  incident_resistors=len(incident))
                else:
                    elements[term_name[k]] = dict(pin=k, layer=access_layer[k], port=pins[k][1] is None,
                                                  incident_ohm=None, ambiguous_names=sorted(names))
            per_net[name].update(r_ohm=math.fsum(r for _, _, r in b["resistors"]), resistors=len(b["resistors"]),
                                 rnodes=len(b["nodes"]), duplicate_networks=len(found) - 1,
                                 terminals=elements)
    record["nets"] = per_net
    record["totals"] = dict(c_pf=sum(n["c_pf"] for n in per_net.values()),
                            coupling_pf=sum(n["coupling_pf"] for n in per_net.values()),
                            pin_internal_pf=sum(n["pin_internal_af"] for n in per_net.values()) * 1e-6,
                            r_ohm=sum(n.get("r_ohm", 0.0) for n in per_net.values()))
    if backannotate_to is not None:
        require(resistance, "back-annotation needs the resistance run")
        factors = backannotate(native_spef, per_net, backannotate_to)
        record["backannotated_spef"] = dict(path=str(backannotate_to), sha256=sha256(backannotate_to),
                                            factor_range=dict(
                                                c=[min(f["c"] for f in factors.values()),
                                                   max(f["c"] for f in factors.values())],
                                                r=[min(f["r"] for f in factors.values()),
                                                   max(f["r"] for f in factors.values())]))
    (out / f"magic_nets_{corner}.json").write_text(json.dumps(record, indent=1, sort_keys=True) + "\n")
    return record


def plant_open(src: Path, dst: Path, def_text: str, pdk: str, profile: dict) -> dict:
    """Negative control: delete the GDS rectangle of the longest signal wire
    (a DEF NETS path, with its half-width end extensions) from the top cell."""
    units = int(re.search(r"UNITS DISTANCE MICRONS (\d+)", def_text)[1])
    scale = units // 1000
    body = def_text.split("\nNETS ", 1)[1].split("\nEND NETS", 1)[0]
    best = None
    current = None
    for line in body.split("\n"):
        if line.startswith("- "):
            current = line.split()[1]
        m = re.match(r"\s*(?:\+ ROUTED|NEW) (\S+) \( (-?\d+) (-?\d+) \) \( (-?\d+) (-?\d+) \)\s*;?$", line)
        if m and current:
            length = abs(int(m[2]) - int(m[4])) + abs(int(m[3]) - int(m[5]))
            if best is None or length > best[0]:
                best = (length, current, m[1], [int(m[i]) for i in range(2, 6)])
    require(best is not None, "no signal wire to delete")
    _, net, layer, (x0, y0, x1, y1) = best
    width = {l["name"]: round(l["width_um"] * units) for l in profile["layers"]}[layer]
    h = width // 2
    rect = [min(x0, x1) - h, min(y0, y1) - h, max(x0, x1) + h, max(y0, y1) + h]
    rect = [v // scale for v in rect]
    gmap = {e["lef"]: e for e in json.loads(text(ROOT / f"fixtures/pdk/{pdk}-gds-map.json"))["layers"]}
    lay, dt = gmap[layer]["net"][0]
    xy = gds.int32s([rect[0], rect[1], rect[2], rect[1], rect[2], rect[3], rect[0], rect[3], rect[0], rect[1]])
    lib = gds.Library(src)
    removed = 0
    for s in lib.structures:
        keep = []
        for e in s["elements"]:
            raw = b"".join(e["records"])
            hit = (e["kind"] == gds.BOUNDARY and gds.encode(gds.XY, gds.INT32, xy) in raw
                   and gds.encode(gds.LAYER, gds.INT16, lay.to_bytes(2, "big")) in raw
                   and gds.encode(gds.DATATYPE, gds.INT16, dt.to_bytes(2, "big")) in raw)
            if hit:
                removed += 1
            else:
                keep.append(e)
        s["elements"] = keep
    require(removed == 1, f"planted open: expected one wire rectangle, removed {removed}")
    lib.write(dst)
    return dict(net=net, layer=layer, rect_gds=rect, removed=removed, gds=str(dst), sha256=sha256(dst))


def extract(args) -> dict:
    inputs = export_inputs(args.rustroad.resolve(), args.workspace.resolve(), args.out.resolve() / "inputs",
                           args.pdk)
    return run_corner(inputs, args.pdk, args.corner, args.native_spef.resolve(), args.out,
                      args.backannotate.resolve() if args.backannotate else None)


# ----------------------------------------------------------------------------
# Back-annotation of per-net totals onto native RC trees
# ----------------------------------------------------------------------------
def backannotate(native_spef: Path, per_net: dict, out: Path, use_r: bool = True) -> dict:
    """Scale each native net's node capacitances (couplings grounded at their
    own node, factor 1, as the timing engine does) to the target per-net C and
    every resistor to the target per-net R. Topology and the *CONN of the
    native SPEF are kept; totals are the sums of the rounded values."""
    lines = text(native_spef).splitlines()
    names = {}
    for line in lines:
        m = re.match(r"^(\*\d+) (\S+)$", line)
        if m:
            names[m[1]] = m[2]
    out_lines, i, factors = [], 0, {}
    while i < len(lines):
        line = lines[i]
        if not line.startswith("*D_NET"):
            out_lines.append(line)
            i += 1
            continue
        block = []
        while not lines[i].startswith("*END"):
            block.append(lines[i])
            i += 1
        block.append(lines[i])
        i += 1
        token = block[0].split()[1]
        name = names.get(token, token)
        target = per_net[name]
        section, caps, res, head, order = None, defaultdict(float), [], [], []
        for row in block[1:]:
            t = row.split()
            if row.startswith("*CAP"):
                section = "cap"
                continue
            if row.startswith("*RES"):
                section = "res"
                continue
            if row.startswith("*END"):
                continue
            if section == "cap":
                node, value = t[1], float(t[-1])
                if node not in caps:
                    order.append(node)
                caps[node] += value
            elif section == "res":
                res.append((t[1], t[2], float(t[3])))
            else:
                head.append(row)
        native_c = sum(caps.values())
        native_r = sum(r for _, _, r in res)
        fc = target["c_pf"] / native_c if native_c > 0 else 1.0
        fr = (target["r_ohm"] / native_r if native_r > 0 else 1.0) if use_r else 1.0
        factors[name] = dict(c=fc, r=fr)
        cap_rows = [(n, float("%.6g" % (caps[n] * fc))) for n in order]
        total = sum(v for _, v in cap_rows)
        out_lines.append(f"*D_NET {token} {total:.9e}")
        out_lines += head
        out_lines.append("*CAP")
        out_lines += [f"{k + 1} {n} {v:.6g}" for k, (n, v) in enumerate(cap_rows)]
        out_lines.append("*RES")
        out_lines += [f"{k + 1} {a} {b} {float('%.6g' % (r * fr)):.6g}" for k, (a, b, r) in enumerate(res)]
        out_lines.append("*END")
    Path(out).write_text("\n".join(out_lines) + "\n")
    return factors


# ----------------------------------------------------------------------------
# Controls on synthetic structures (Magic against Magic's own rules)
# ----------------------------------------------------------------------------
def controls(args) -> dict:
    """Isolated wires against Magic's own area/perimeter rules, a pair, a
    crossing, and resistance of a straight wire against sheet * L / W."""
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    pdk = args.pdk
    tech_name = PDKS[pdk]["tech"]
    spec = manifest_pdk(pdk)
    tech = text(Path(spec["host_root"]) / f"libs.tech/magic/{tech_name}.tech")
    layers = dict(sky130=("m1", "m2", 0.14, 0.14, 0.14), gf180=("m1", "m2", 0.23, 0.28, 0.28))[pdk]
    lo, hi, wl, wh, sp = layers
    cases = {
        "iso_hi": f"wire {hi} 0 0 40 {wh} A",
        "iso_lo": f"wire {lo} 0 0 40 {wl} A",
        "pair_hi": f"wire {hi} 0 0 40 {wh} A; wire {hi} 0 {wh + sp} 40 {2 * wh + sp} B",
        "cross": f"wire {hi} 10 0 {10 + wh} 20 A; wire {lo} 0 10 20 {10 + wl} B",
        "hi_v20": f"wire {hi} 10 0 {10 + wh} 20 A",
        "lo_h20": f"wire {lo} 0 10 20 {10 + wl} B",
    }
    body = ["crashbackups stop", "drc off", "set od $::env(OUTDIR)", "file mkdir $od", "cd $od",
            "extract style $::env(STYLE)",
            "proc wire {layer x0 y0 x1 y1 name} {",
            "  box ${x0}um ${y0}um ${x1}um ${y1}um", "  paint $layer",
            "  box ${x0}um ${y0}um ${x0}um ${y0}um", "  label $name c $layer", "}"]
    for cell, spec_ in cases.items():
        body += [f"cellname create {cell}", f"load {cell}"] + [s.strip() for s in spec_.split(";")]
        body += ["extract do capacitance", "extract do coupling", "extract no resistance", "extract all"]
    body += ["puts RR_C_DONE", "quit -noprompt"]
    receipt, work = run_magic(out, "controls", pdk, None, "none", "ngspice()", "\n".join(body) + "\n")
    res = {}
    for cell in cases:
        ext = parse_ext(work / f"{cell}.ext")
        totals, partners = node_totals(ext)
        res[cell] = dict(nodes={n: dict(c_af=v, lumped_r=ext["nodes"].get(n, {}).get("r")) for n, v in totals.items()},
                         caps=[list(c) for c in ext["caps"]])
    # Magic's own rules for an isolated wire (nominal style, aF): area * areacap + perimeter * perimc.
    def rule(kind, layer_alias):
        m = re.search(rf"^\s*{kind}\s+{layer_alias}\s+\S+\s+([0-9.]+)", tech, re.M)
        return float(m[1]) if m else None
    alias = dict(m1="allm1", m2="allm2")
    checks = {}
    for cell, (layer, w) in (("iso_hi", (hi, wh)), ("iso_lo", (lo, wl))):
        area, perim = rule("defaultareacap", alias[layer]), rule("defaultperimeter", alias[layer])
        want = area * 40 * w + perim * 2 * (40 + w)
        got = res[cell]["nodes"]["A"]["c_af"]
        checks[cell] = dict(magic_af=got, rules_af=want, rel=(got - want) / want,
                            status="passed" if abs(got - want) / want < 0.02 else "failed")
    cross = res["cross"]["nodes"]
    checks["crossing_increment_af"] = dict(
        upper=cross["A"]["c_af"] - res["hi_v20"]["nodes"]["A"]["c_af"],
        lower=cross["B"]["c_af"] - res["lo_h20"]["nodes"]["B"]["c_af"])
    report = dict(schema="rustroad.magic-controls.v1", pdk=pdk, cases=cases, results=res, checks=checks,
                  receipt=dict(label=receipt["label"], wall_seconds=receipt["wall_seconds"],
                               peak_rss_kib=receipt["peak_rss_kib"]))
    (out / "controls.json").write_text(json.dumps(report, indent=1, sort_keys=True) + "\n")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    e = sub.add_parser("extract")
    e.add_argument("--workspace", type=Path, required=True)
    e.add_argument("--rustroad", type=Path, default=ROOT / "target/release/rustroad")
    e.add_argument("--pdk", choices=sorted(PDKS), required=True)
    e.add_argument("--corner", choices=("min", "nom", "max"), required=True)
    e.add_argument("--native-spef", type=Path, required=True)
    e.add_argument("--out", type=Path, required=True)
    e.add_argument("--backannotate", type=Path)
    c = sub.add_parser("controls")
    c.add_argument("--pdk", choices=sorted(PDKS), required=True)
    c.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = extract(args) if args.command == "extract" else controls(args)
    except (MagicError, gds.GdsError, container.ContainerError, OSError, KeyError, ValueError) as error:
        print(json.dumps(dict(status="failed", reason=str(error))))
        return 1
    print(json.dumps(dict(status="ok", totals=result.get("totals"), checks=result.get("checks"))))
    return 0


if __name__ == "__main__":
    sys.exit(main())
