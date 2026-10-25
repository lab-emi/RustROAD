#!/usr/bin/env python3
"""Bounded dual-PDK RustSys -> RustROAD -> RustSTA connectivity integration.

Reads real PDK data and pinned external executables as black boxes. This does
not route, extract parasitics, perform DRC/LVS or claim signoff.
"""
from __future__ import annotations
import argparse
from collections import defaultdict
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import resource
import shutil
import sys

from verify_logic import (Harness, bounded_text, executable, ideal_projection,
                          sha256, sta_flow, tcl_path, write_json, yosys_path, NUMBER)

RTL = '''module lane_smoke(input clk, input en, input [3:0] a, input [3:0] b, output [3:0] y, output [3:0] z);
  reg [3:0] q;
  wire [3:0] mix;
  assign mix = (a ^ b) & {4{en}};
  always @(posedge clk) q <= mix;
  assign y = q;
  assign z = a & b;
endmodule
'''
SDC = '''create_clock -name core -period 10 [get_ports clk]
set_input_delay 0.2 -clock core [get_ports {en a b}]
set_output_delay 0.2 -clock core [get_ports {y z}]
set_input_transition 0.1 [get_ports {clk en a b}]
set_clock_transition 0.1 [get_clocks core]
set_load 0.01 [get_ports {y z}]
'''
MAX_CONNECTIONS = 16000
MAX_CELLS = 2000
IMAGE = "f152917ebc458bd015042e44d9dac5af69bcae9fb5657d5121cb4d7226ed0c67"
OPENROAD = "/OpenROAD-flow-scripts/tools/install/OpenROAD/bin/openroad"


class PhysicalHarness(Harness):
    def run(self, label, command, **kwargs):
        # The reference allocator reserves GiB-sized regions even for tiny
        # designs. Physical RAM stays capped by the container's 2 GiB cgroup.
        original = self.args.memory_mib
        try:
            if str(command[0]) == "podman":
                self.args.memory_mib = 8192
            return super().run(label, command, **kwargs)
        finally:
            self.args.memory_mib = original

    def json_command(self, label, command, envelope=False):
        # The larger fixture has more than 8 endpoint checks; bound at 128.
        if "report" in command and "--limit" in command:
            command = list(command)
            command[command.index("--limit") + 1] = "128"
        return super().json_command(label, command, envelope)


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def run_ok(h, label, command):
    receipt = h.run(label, [str(x) for x in command])
    require(receipt["exit_code"] == 0, label + ": command failed; see receipt")
    return receipt


def fingerprint(path):
    return dict(path=str(path), sha256=sha256(path), bytes=path.stat().st_size)


def yosys_model(h, label, yosys, lib, netlist):
    script, output = h.root / (label + ".ys"), h.root / (label + ".json")
    script.write_text(f"read_liberty -lib {yosys_path(lib)}\nread_verilog {yosys_path(netlist)}\nhierarchy -top lane_smoke\nwrite_json {yosys_path(output)}\n")
    run_ok(h, label, [yosys, "-Q", "-T", "-s", script])
    module = json.loads(bounded_text(output))["modules"]["lane_smoke"]
    require(len(module["cells"]) <= MAX_CELLS, "cell budget exceeded")
    groups = defaultdict(list)
    ports, port_bits, cells, count = [], [], [], 0
    for name, port in module["ports"].items():
        bits = port["bits"]
        ports.append((name, port["direction"], len(bits), port.get("offset", 0), port.get("upto", 0)))
        for i, bit in enumerate(bits):
            index = i + port.get("offset", 0)
            if port.get("upto", 0):
                index = len(bits) - 1 - i + port.get("offset", 0)
            scalar = name if len(bits) == 1 else f"{name}[{index}]"
            port_bits.append((scalar, port["direction"]))
            groups[str(bit)].append(("P", scalar, ""))
            count += 1
    for name, cell in module["cells"].items():
        cells.append((name, cell["type"]))
        for pin, bits in cell["connections"].items():
            require(len(bits) == 1, "multi-bit standard-cell pin unsupported in oracle")
            bit = bits[0]
            groups[str(bit)].append(("I", name, pin))
            count += 1
    require(count <= MAX_CONNECTIONS, "connection budget exceeded")
    for constant in ("0", "1", "x", "z"):
        if constant in groups:
            groups[constant].append(("C", constant, ""))
    return dict(ports=sorted(ports), port_bits=sorted(port_bits), cells=sorted(cells),
                nets=sorted(tuple(sorted(endpoints)) for endpoints in groups.values()),
                endpoint_count=count)


def external_cec(h, tag, yosys, lib, reference_rtl, netlist, expected=True):
    script = h.root / (tag + ".ys")
    script.write_text(f'''read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(lib)}
read_verilog {yosys_path(netlist)}
hierarchy -top lane_smoke
proc
flatten
opt
rename lane_smoke gate
design -stash gate
design -reset
read_verilog -sv {yosys_path(reference_rtl)}
proc
opt
rename lane_smoke gold
design -stash gold
design -copy-from gold -as gold gold
design -copy-from gate -as gate gate
equiv_make gold gate equiv
hierarchy -top equiv
equiv_simple -seq 2 -undef
equiv_status -assert
''')
    receipt = h.run(tag, [str(yosys), "-Q", "-T", "-s", str(script)])
    stdout, stderr = bounded_text(Path(receipt["stdout"])), bounded_text(Path(receipt["stderr"]))
    actual = receipt["exit_code"] == 0 and "Equivalence successfully proven!" in stdout
    negative = receipt["exit_code"] == 1 and "unproven $equiv cells" in stderr
    require(actual if expected else negative, tag + ": CEC expectation not met")
    return dict(status="passed", expected_equivalent=expected, receipt=tag)



def simulate_wide(h, tag, tools, lib, rtl, netlist):
    if tools.get("verilator") is None:
        return dict(status="unverified", reason="Verilator unavailable")
    model, script, tb = (h.root / (tag + suffix) for suffix in ("_gate_sim.v", "_gate_sim.ys", "_sim_tb.sv"))
    script.write_text(f"read_liberty -ignore_miss_func -ignore_miss_dir -ignore_miss_data_latch {yosys_path(lib)}\nread_verilog {yosys_path(netlist)}\nhierarchy -top lane_smoke\nproc\nflatten\nrename lane_smoke gate\nwrite_verilog -noattr {yosys_path(model)}\n")
    run_ok(h, tag + "_sim_model", [tools["yosys"], "-Q", "-T", "-s", script])
    tb.write_text("""module test;
  reg clk=0, en=0; reg [3:0] a=0,b=0;
  wire [3:0] yg,zg,ym,zm;
  reg [3:0] expected_y;
  lane_smoke gold(clk,en,a,b,yg,zg);
  gate mapped(clk,en,a,b,ym,zm);
  initial begin
    for(integer i=0;i<512;i=i+1) begin
      a=i[3:0];b=i[7:4];en=i[8];#1;clk=1;#1;
      expected_y=(a^b)&{4{en}};
      if($test$plusargs("negative")) expected_y=expected_y^4'b0001;
      if(yg!==expected_y || ym!==expected_y || zg!==(a&b) || zm!==(a&b)) begin
        $display("SIM_MISMATCH case=%0d",i);$fatal(1,"mismatch");
      end
      clk=0;#1;
    end
    $display("RUSTROAD_SIM_ACCEPT cases=512");$finish;
  end
endmodule
""")
    build = h.root / (tag + "_sim_build")
    run_ok(h, tag + "_sim_build", [tools["verilator"], "--binary", "--timing", "-Wno-fatal", "-j", "2", "--top-module", "test", "--Mdir", build, rtl, model, tb])
    positive = run_ok(h, tag + "_sim_positive", [build / "Vtest"])
    require("RUSTROAD_SIM_ACCEPT cases=512" in bounded_text(Path(positive["stdout"])), "simulation did not exhaust cases")
    negative = h.run(tag + "_sim_negative", [str(build / "Vtest"), "+negative"])
    require(negative["exit_code"] != 0 and "SIM_MISMATCH" in bounded_text(Path(negative["stdout"])), "simulation negative control missed")
    return dict(status="passed", cases=512, initial_state="sampled after first clock only", negative_control="rejected", oracle="explicit gated-XOR register and AND output truth table")

def compare_sta(h, tag, opensta, lib, netlist, sdc, native):
    script = h.root / (tag + ".tcl")
    script.write_text(f'''read_liberty {tcl_path(lib)}
read_verilog {tcl_path(netlist)}
link_design lane_smoke
read_sdc {tcl_path(sdc)}
report_units
puts RUSTROAD_SETUP_BEGIN
report_checks -path_delay max -group_path_count 128 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_SETUP_END
puts RUSTROAD_HOLD_BEGIN
report_checks -path_delay min -group_path_count 128 -endpoint_path_count 1 -format end -digits 12
puts RUSTROAD_HOLD_END
exit
''')
    receipt = run_ok(h, tag, [opensta, "-no_init", "-no_splash", "-exit", script])
    text = bounded_text(Path(receipt["stdout"])) + bounded_text(Path(receipt["stderr"]))
    require(not re.search(r"(?mi)^\s*Error(?:\s+\d+)?:", text), "OpenSTA script error")
    require(re.search(r"(?mi)^\s*Time\s+1ns\s*$", text), "OpenSTA time unit not verified")
    rows = {}
    for kind in ("SETUP", "HOLD"):
        begin, end = "RUSTROAD_" + kind + "_BEGIN", "RUSTROAD_" + kind + "_END"
        require(text.count(begin) == 1 and text.count(end) == 1, "reference markers missing")
        section = text.split(begin, 1)[1].split(end, 1)[0]
        pattern = rf"^(\S+)\s+\([^\n]*?\)\s+({NUMBER})\s+({NUMBER})\s+({NUMBER})\s+\((?:MET|VIOLATED)\)\s*$"
        rows[kind.lower()] = {m[1]: float(m[4]) for m in re.finditer(pattern, section, re.M)}
    expected_names = {row["endpoint"] for row in native["endpoints"]}
    require(expected_names and all(set(v) == expected_names for v in rows.values()), "reference endpoint coverage differs")
    comparisons = []
    for row in native["endpoints"]:
        for kind in ("setup", "hold"):
            a, b = row[kind + "_slack_ns"], rows[kind][row["endpoint"]]
            require(math.isfinite(a) and math.isfinite(b), "nonfinite timing value")
            comparisons.append(dict(endpoint=row["endpoint"], kind=kind, ruststa_ns=a, opensta_ns=b, difference_ns=abs(a-b)))
    maximum = max(row["difference_ns"] for row in comparisons)
    warnings = [line for line in text.splitlines() if re.match(r"\s*Warning", line, re.I)]
    require(maximum <= h.args.tolerance_ns and not warnings, "same-scope timing mismatch or warning")
    return dict(status="passed", wire_model="ideal", corner="tt", parasitics="none",
                unconstrained_endpoints=native["completeness"]["unconstrained_endpoints"],
                tolerance_ns=h.args.tolerance_ns, max_difference_ns=maximum, comparisons=comparisons, receipt=tag)


def openroad_graph(h, tag, pdk, def_path, expected_graph, expected_design, expected_masters, expect_same=True):
    if shutil.which("podman") is None:
        return dict(status="unverified", reason="podman unavailable")
    check = h.run(tag + "_image", ["podman", "image", "exists", IMAGE])
    if check["exit_code"] != 0:
        return dict(status="unverified", reason="pinned OpenROAD image probe failed", receipt=tag+"_image")
    binary_probe = run_ok(h, tag + "_binary_identity", ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only", "--memory=2g", "--memory-swap=2g", "--entrypoint=/usr/bin/sha256sum", IMAGE, OPENROAD])
    binary_sha = bounded_text(Path(binary_probe["stdout"])).split()[0]
    require(re.fullmatch(r"[a-f0-9]{64}", binary_sha), "OpenROAD binary identity unavailable")
    script = h.root / (tag + ".tcl")
    script.write_text(f'''read_lef /technology.lef
read_lef /cells.lef
read_def /run/{def_path.name}
if {{[catch {{check_placement -verbose}} message]}} {{ puts "PLACEMENT_REJECTED $message"; exit 42 }}
puts RUSTROAD_OPENDB_BEGIN
set block [ord::get_db_block]
array set seenMaster {{}}
puts "U [$block getDbUnitsPerMicron]"
foreach term [$block getBTerms] {{ puts "D [binary encode hex [$term getName]] [$term getIoType]" }}
foreach inst [$block getInsts] {{
  set master [$inst getMaster]
  set xy [$inst getLocation]
  puts "G [binary encode hex [$inst getName]] [$master getWidth] [$master getHeight] [lindex $xy 0] [lindex $xy 1] [$inst getOrient] [binary encode hex [$master getName]]"
  set masterName [$master getName]
  if {{![info exists seenMaster($masterName)]}} {{
    set seenMaster($masterName) 1
    set mh [binary encode hex $masterName]
    foreach mt [$master getMTerms] {{
      set ph [binary encode hex [$mt getName]]
      puts "M $mh $ph [$mt getIoType] [$mt getSigType]"
      set group 0
      foreach mp [$mt getMPins] {{
        foreach box [$mp getGeometry] {{
          set layer [$box getTechLayer]
          if {{$layer == "NULL"}} {{ error "rectangle oracle received a non-layer shape" }}
          puts "R $mh $ph $group [binary encode hex [$layer getName]] [$box xMin] [$box yMin] [$box xMax] [$box yMax]"
        }}
        incr group
      }}
    }}
    foreach box [$master getObstructions] {{
      set layer [$box getTechLayer]
      if {{$layer == "NULL"}} {{ error "obstruction oracle received a non-layer shape" }}
      puts "O $mh [binary encode hex [$layer getName]] [$box xMin] [$box yMin] [$box xMax] [$box yMax]"
    }}
  }}
}}
foreach net [$block getNets] {{
  set name [binary encode hex [$net getName]]
  foreach term [$net getITerms] {{
    set inst [$term getInst]
    set pin [$term getMTerm]
    puts "I $name [binary encode hex [$inst getName]] [binary encode hex [$pin getName]]"
  }}
  foreach term [$net getBTerms] {{ puts "P $name [binary encode hex [$term getName]] -" }}
}}
puts RUSTROAD_OPENDB_END
exit 0
''')
    internal_time = h.root / (tag + "_internal.time")
    command = ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only", "--tmpfs=/tmp:rw,size=64m",
        "--memory=2g", "--memory-swap=2g", "--cpus=2", "--pids-limit=128", "--ulimit=fsize=67108864:67108864",
        "--ulimit=core=0:0", "--volume=" + str(h.root) + ":/run:rw",
        "--volume=" + pdk["technology_lef"]["path"] + ":/technology.lef:ro",
        "--volume=" + pdk["cell_lef"]["path"] + ":/cells.lef:ro", "--entrypoint=/usr/bin/timeout", IMAGE, "25s", "/usr/bin/time",
        "-v", "-o", "/run/" + internal_time.name, OPENROAD, "-no_init", "-no_splash", "-threads", "2", "-exit", "/run/" + script.name]
    receipt = run_ok(h, tag, command)
    output = bounded_text(Path(receipt["stdout"]))
    require(output.count("RUSTROAD_OPENDB_BEGIN") == 1 and output.count("RUSTROAD_OPENDB_END") == 1, "OpenDB graph markers missing")
    section = output.split("RUSTROAD_OPENDB_BEGIN")[1].split("RUSTROAD_OPENDB_END")[0]
    groups, geometry, port_directions, units = defaultdict(list), [], [], None
    master_pins, master_rects, master_obs = {}, defaultdict(lambda: defaultdict(list)), defaultdict(list)
    shape_count = 0
    for line in section.splitlines():
        fields = line.split()
        if not fields:
            continue
        if fields[0] == "M":
            key = (bytes.fromhex(fields[1]).decode(), bytes.fromhex(fields[2]).decode())
            master_pins[key] = (fields[3].lower(), fields[4].lower())
            require(len(master_pins) <= 512, "pin-definition oracle budget exceeded")
        elif fields[0] == "R":
            key = (bytes.fromhex(fields[1]).decode(), bytes.fromhex(fields[2]).decode())
            master_rects[key][int(fields[3])].append((bytes.fromhex(fields[4]).decode(), *map(int, fields[5:])))
            shape_count += 1
            require(shape_count <= 16000, "pin/obstruction geometry oracle budget exceeded")
        elif fields[0] == "O":
            master_obs[bytes.fromhex(fields[1]).decode()].append((bytes.fromhex(fields[2]).decode(), *map(int, fields[3:])))
            shape_count += 1
            require(shape_count <= 16000, "pin/obstruction geometry oracle budget exceeded")
        elif fields[0] == "U":
            units = int(fields[1])
        elif fields[0] == "D":
            port_directions.append((bytes.fromhex(fields[1]).decode(), fields[2].lower()))
        elif fields[0] == "G":
            geometry.append(dict(instance=bytes.fromhex(fields[1]).decode(), width=int(fields[2]),height=int(fields[3]),x=int(fields[4]),y=int(fields[5]),orientation=fields[6], master=bytes.fromhex(fields[7]).decode()))
        elif fields[0] in ("P", "I"):
            kind, net, one, two = fields
            groups[net].append((kind, bytes.fromhex(one).decode(), bytes.fromhex(two).decode() if kind == "I" else ""))
    require(shape_count <= 16000, "pin/obstruction geometry oracle budget exceeded")
    require(len(geometry) <= MAX_CELLS and sum(map(len, groups.values())) <= MAX_CONNECTIONS, "OpenDB oracle exceeded budgets")
    actual = sorted(tuple(sorted(group)) for group in groups.values())
    matches = actual == expected_graph["nets"]
    require(matches == expect_same, "DEF connectivity oracle expectation was not met")
    require(sorted(port_directions) == expected_graph["port_bits"], "DEF port directions differ")
    require(units == expected_design["units_per_micron"], "OpenDB database units differ")
    require(sorted((g["instance"], g["master"]) for g in geometry) == expected_graph["cells"], "DEF cell masters differ from mapped Verilog")
    expected_sizes = {cell["name"]:(cell["width"],cell["height"]) for cell in expected_design["cells"]}
    require({g["instance"]:(g["width"],g["height"]) for g in geometry} == expected_sizes, "imported master geometry differs from OpenDB")
    require(len(geometry) == len(expected_graph["cells"]), "OpenDB instance coverage differs")
    expected_pins, expected_rects, expected_obs = {}, {}, {}
    for master in expected_masters:
        name = master["name"]
        expected_obs[name] = []
        for rect in master["obstructions"]:
            require(rect["kind"] == "rect", "nonrectangular obstruction needs separate oracle")
            expected_obs[name].append((rect["layer"], *rect["rect"]))
        expected_obs[name].sort()
        for pin in master["pins"]:
            key = (name, pin["name"])
            expected_pins[key] = (pin["direction"], pin["use_"])
            ports = []
            for port in pin["ports"]:
                shapes = []
                for rect in port:
                    require(rect["kind"] == "rect", "nonrectangular MPin needs separate oracle")
                    shapes.append((rect["layer"], *rect["rect"]))
                ports.append(tuple(sorted(shapes)))
            expected_rects[key] = sorted(ports)
    actual_rects = {key: sorted(tuple(sorted(port)) for port in ports.values()) for key, ports in master_rects.items()}
    actual_obs = {master["name"]: sorted(master_obs[master["name"]]) for master in expected_masters}
    require(master_pins == expected_pins, "LEF signal/PG pin directions or uses differ from OpenDB")
    require(actual_rects == expected_rects, "LEF pin/MPin rectangles differ from OpenDB")
    require(actual_obs == expected_obs, "LEF obstruction rectangles differ from OpenDB")
    geometry_proof = dict(
        properties=sorted((master, pin, *value) for (master, pin), value in master_pins.items()),
        pin_ports=[dict(master=key[0], pin=key[1], ports=value) for key, value in sorted(actual_rects.items())],
        obstructions=[dict(master=key, rectangles=value) for key, value in sorted(actual_obs.items())])
    proof_path = h.root / (tag + "_master_geometry.json")
    write_json(proof_path, geometry_proof)
    require(all(g["orientation"] in ("R0", "N") for g in geometry), "unexpected row orientation")
    time_text = bounded_text(internal_time)
    peak = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", time_text)
    require(peak is not None and 0 < int(peak[1]) < 2 * 1024**2, "OpenROAD RSS missing or over budget")
    return dict(status="passed", expected_same_topology=expect_same, observed_same_topology=matches, method="OpenDB public API endpoint partition comparison", endpoint_count=sum(map(len, groups.values())),
                cell_geometry=geometry, master_geometry=dict(status="passed", masters=len(expected_masters), pins=len(master_pins), rectangles=shape_count, pin_rectangles=sum(len(port) for pins in actual_rects.values() for port in pins), obstruction_rectangles=sum(len(v) for v in actual_obs.values()), evidence=fingerprint(proof_path), comparison="all signal/PG directions, uses, grouped MPin rectangles and OBS"), image_id=IMAGE, binary_sha256=binary_sha, peak_rss_kib=int(peak[1]) if peak else None, receipt=tag)


def run_case(h, pdk, tools):
    tag = pdk["tag"]
    lib, tech, lef = (Path(pdk[key]["path"]) for key in ("liberty", "technology_lef", "cell_lef"))
    for key in ("liberty", "technology_lef", "cell_lef"):
        require(sha256(Path(pdk[key]["path"])) == pdk[key]["sha256"], "PDK pin mismatch: " + key)
    rtl, sdc = h.root / (tag + ".sv"), h.root / (tag + ".sdc")
    rtl.write_text(RTL)
    sdc.write_text(SDC)
    mapped, exported, definition = (h.root / (tag + suffix) for suffix in ("_mapped.v", "_exported.v", ".def"))
    base = [str(tools["rustsys"]), "--threads", "2", "--progress", "off"]
    synth = h.json_command(tag + "_synth", base + ["synth", "--backend", "cpu", "--top", "lane_smoke", "--lib", str(lib), "--out", str(mapped), "--print", "json", str(rtl)])
    original_graph = yosys_model(h, tag + "_mapped_graph", tools["yosys"], lib, mapped)
    require(2 < len(original_graph["cells"]) <= MAX_CELLS, "fixture did not exercise nontrivial mapped cells")
    # PDK data supplies exact site dimensions; do not infer a synthetic rectangle.
    sites = re.findall(r"(?ms)^SITE\s+(\S+)\s*\n(.*?)^END\s+\1", bounded_text(tech))
    require(sites, "technology LEF has no site")
    site_name, body = sites[0]
    size = re.search(r"SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)", body)
    require(size, "site SIZE absent")
    dbu_match = re.search(r"DATABASE\s+MICRONS\s+(\d+)", bounded_text(tech))
    require(dbu_match, "LEF database unit absent")
    from decimal import Decimal
    units = int(dbu_match[1])
    site_w, site_h = (int(Decimal(v) * units) for v in size.groups())
    x0, y0, x1, y1 = 5*site_w, 5*site_h, 105*site_w, 13*site_h
    floorplan = dict(name="lane_smoke", units_per_micron=units, die_area=dict(lx=0,ly=0,ux=x1+5*site_w,uy=y1+5*site_h),
        core_area=dict(lx=x0,ly=y0,ux=x1,uy=y1), site=dict(name=site_name,width=site_w,height=site_h),
        rows=[dict(x_start=x0,x_end=x1,y=y0+i*site_h) for i in range(8)])
    floor = h.root / (tag + "_floorplan.json")
    write_json(floor, floorplan)
    workspace = h.root / (tag + "_physical")
    road = [str(tools["rustroad"]), "--workspace", str(workspace), "--threads", "2", "--max-cells", "2000", "--max-rows", "128", "--max-expansions", "2128"]
    invalid_pin = h.root / (tag + "_unknown_pin.v")
    text = mapped.read_text()
    invalid_text, changes = re.subn(r"\.(?:A|A1|I)\(", ".UNKNOWN_INPUT(", text, count=1)
    require(changes == 1, "unknown-pin control unavailable")
    invalid_pin.write_text(invalid_text)
    rejected_ws = h.root / (tag + "_reject_unknown_pin")
    rejected = h.run(tag + "_reject_unknown_pin", [str(tools["rustroad"]), "--workspace", str(rejected_ws),
        "import-pdk", "--base-rev", "rev0", "--tech-lef", str(tech), "--cell-lef", str(lef),
        "--netlist", str(invalid_pin), "--floorplan", str(floor)])
    require(rejected["exit_code"] != 0 and not (rejected_ws / "HEAD").exists(), "unknown pin accepted or published a workspace")
    imported = h.json_command(tag + "_import", road + ["import-pdk", "--base-rev", "rev0", "--tech-lef", str(tech), "--cell-lef", str(lef), "--netlist", str(mapped), "--floorplan", str(floor)])
    placed = h.json_command(tag + "_place", road + ["place", "--base-rev", imported["revision"]])
    out = run_ok(h, tag + "_export_verilog", road + ["export-verilog"])
    exported.write_bytes(Path(out["stdout"]).read_bytes())
    out = run_ok(h, tag + "_export_def", road + ["export-def"])
    definition.write_bytes(Path(out["stdout"]).read_bytes())
    export_graph = yosys_model(h, tag + "_exported_graph", tools["yosys"], lib, exported)
    require(export_graph == original_graph, "exported Verilog topology differs from RustSys mapping")
    text = exported.read_text()
    bad = h.root / (tag + "_bad_pin.v")
    mutated, count = re.subn(r"(\.(?:A|A1|I)\()[^\)]+\)", r"\1clk)", text, count=1)
    require(count == 1 and mutated != text, "negative pin control could not be constructed")
    bad.write_text(mutated)
    bad_graph = yosys_model(h, tag + "_bad_graph", tools["yosys"], lib, bad)
    require(bad_graph != original_graph, "connectivity oracle missed rewired pin")
    cec = external_cec(h, tag + "_cec", tools["yosys"], lib, rtl, exported)
    negative = external_cec(h, tag + "_cec_bad", tools["yosys"], lib, rtl, bad, False)
    native_check = h.json_command(tag + "_native_cec", base + ["check", "synth", "--top", "lane_smoke", "--lib", str(lib), "--netlist", str(exported), "--budget", "10000", "--model-budget", "10000", "--sweep-max-candidates", "256", "--print", "json", str(rtl)])
    require(native_check.get("verdict") == "equivalent", "native CEC did not prove exported netlist")
    metadata = json.loads(bounded_text(workspace / "revisions" / placed["revision"] / "design.json"))
    simulation = simulate_wide(h, tag, tools, lib, rtl, exported)
    odb = openroad_graph(h, tag + "_opendb", pdk, definition, original_graph, metadata["design"], metadata["physical"]["connectivity"]["masters"])
    bad_def = h.root / (tag + "_bad_connection.def")
    def_lines = definition.read_text().splitlines()
    endpoint = None
    for i, line in enumerate(def_lines):
        candidate = re.search(r"\( (g[0-9]+) (A|A1|I) \)", line)
        if candidate:
            endpoint = candidate[0]
            def_lines[i] = line.replace(endpoint, "", 1)
            break
    require(endpoint is not None, "DEF negative endpoint not found")
    for i, line in enumerate(def_lines):
        if line.startswith("- clk ( PIN clk )"):
            def_lines[i] = line.replace("+ USE", endpoint + " + USE", 1)
            break
    else:
        raise RuntimeError("DEF negative target net not found")
    bad_def.write_text("\n".join(def_lines) + "\n")
    odb_negative = openroad_graph(h, tag + "_opendb_negative", pdk, bad_def, original_graph, metadata["design"], metadata["physical"]["connectivity"]["masters"], False)
    projected = h.root / (tag + "_ideal.lib")
    projection = ideal_projection(lib, projected)
    timing = sta_flow(h, tag + "_timing", tools, projected, exported, sdc)
    require(timing["scope"]["wire_model"] == "ideal", "ideal projection timing scope changed")
    comparison = compare_sta(h, tag + "_opensta", tools["opensta"], projected, exported, sdc, timing)
    for key in ("liberty", "technology_lef", "cell_lef"):
        require(sha256(Path(pdk[key]["path"])) == pdk[key]["sha256"], "PDK input changed during run")
    return dict(pdk=tag, status="passed" if all(x["status"] == "passed" for x in (odb, odb_negative, simulation)) else "unverified", synth_cells=synth["cells"],
        topology=dict(status="passed", cells=len(original_graph["cells"]), endpoints=original_graph["endpoint_count"], negative_pin_control="rejected", unknown_pin_import="rejected_before_publication"),
        imported=imported, placed=placed, native_cec=native_check["verdict"], external_cec=cec, cec_negative=negative,
        opendb=odb, opendb_negative=odb_negative, simulation=simulation, timing=timing, timing_comparison=comparison, projection=projection,
        artifacts={"mapped":fingerprint(mapped),"exported":fingerprint(exported),"def":fingerprint(definition),"floorplan":fingerprint(floor)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("workspace", "base-rev", "pdks", "rustroad", "rustsys", "rusteda", "ruststa", "yosys", "opensta"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--verilator", default="verilator")
    args = parser.parse_args()
    require(args.base_rev == "empty", "REVISION_CONFLICT: fresh evidence workspace requires empty")
    args.threads, args.memory_mib, args.timeout_seconds, args.tolerance_ns, args.max_conflicts = 2, 2048, 30, 0.001, 10000
    os.environ["GOMAXPROCS"] = "2"
    root = Path(args.workspace).resolve()
    root.mkdir(parents=True, exist_ok=False)
    lock = (root / "workspace.lock").open("wb")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    (root / "HEAD").write_text("rev0\n")
    resource.setrlimit(resource.RLIMIT_AS, (8192*1024**2,8192*1024**2))
    require(Path(args.pdks).is_file(), "PDK manifest must be a regular file")
    manifest = json.loads(bounded_text(Path(args.pdks), 1024**2))
    require(manifest.get("schema") == "rustroad.pdk-import-profile.v1" and len(manifest["pdks"]) == 2, "dual-PDK manifest required")
    require({p["tag"] for p in manifest["pdks"]} == {"sky130", "gf180"}, "both baseline PDK tags required")
    h = PhysicalHarness(root,args)
    summary = dict(schema="rustroad.pdk-import-verification.v1", status="unverified", pdks=[], tools={},
        source=fingerprint(Path(__file__).resolve()), helper_source=fingerprint(Path(__file__).with_name("verify_logic.py").resolve()), input_profile=fingerprint(Path(args.pdks).resolve()),
        analysis_scope=dict(corner="tt",wire_model="ideal",unconstrained_endpoints=None),
        physical_scope=dict(placement="attempted",power_grid="unchecked",routing="not_run",drc="unchecked",lvs="unchecked",extracted_sta="not_run"))
    tools = {name:executable(getattr(args,name)) for name in ("rustroad","rustsys","rusteda","ruststa","yosys","opensta","verilator")}
    summary["memory_estimate"] = dict(objects=2000,endpoints=16000,bytes_per_endpoint=256,bytes_per_cell=512,
        corners=1,threads=2,graph_and_tags_mib=8,parsed_liberty_mib=640,parsed_lef_mib=128,
        runtime_headroom_mib=256,external_tool_headroom_mib=256,podman_virtual_address_mib=8192,openroad_container_memory_mib=2048,max_paths=8,max_conflicts=10000,
        concurrent_children=1,all_paths_materialized=False)
    preflight = run_ok(h,"preflight_memory",["free","-h"])
    summary["host_memory"] = bounded_text(Path(preflight["stdout"]))
    write_json(root / "preflight.json",summary)
    try:
        for name, tool in tools.items():
            require(tool is not None, "unavailable tool " + name)
            receipt = run_ok(h,name + "_version",[tool,"-V" if name=="yosys" else "-version" if name=="opensta" else "--version"])
            summary["tools"][name] = {**fingerprint(tool),"version":bounded_text(Path(receipt["stdout"])).strip()}
        for pdk in manifest["pdks"]:
            if any(not Path(pdk[key]["path"]).is_file() for key in ("liberty", "technology_lef", "cell_lef")):
                summary["pdks"].append(dict(pdk=pdk["tag"],status="unverified",reason="missing PDK data file"))
                continue
            try:
                summary["pdks"].append(run_case(h,pdk,tools))
            except (RuntimeError,ValueError,KeyError,OSError) as error:
                summary["pdks"].append(dict(pdk=pdk["tag"],status="failed",reason=str(error)))
        summary["status"] = "passed" if all(p["status"]=="passed" for p in summary["pdks"]) else "failed" if any(p["status"]=="failed" for p in summary["pdks"]) else "unverified"
    except (RuntimeError, ValueError, KeyError, OSError) as error:
        summary["status"] = "unverified"
        summary["reason"] = str(error)
    finally:
        summary["identity_stable"] = all(sha256(Path(v["path"])) == v["sha256"] for v in summary["tools"].values())
        if not summary["identity_stable"]:
            summary["status"] = "failed"
        summary["receipts"] = h.receipts
        known_rss = [r["peak_rss_kib"] for r in h.receipts if r["peak_rss_kib"] is not None]
        summary["peak_host_process_rss_kib"] = max(known_rss, default=None)
        summary["rss_measurement_status"] = "complete" if len(known_rss) == len(h.receipts) else "partial"
        if summary["status"] == "passed" and summary["rss_measurement_status"] != "complete":
            summary["status"] = "unverified"
        summary["full_rtl_to_gds_status"] = "unverified"
        write_json(root/"summary.json",summary)
        (root/"HEAD").write_text("rev1\n")
    print(json.dumps(dict(status=summary["status"],summary=str(root/"summary.json"))))
    return {"passed":0,"unverified":3,"failed":1}[summary["status"]]


if __name__ == "__main__":
    sys.exit(main())
