#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Write the scale design manifests (schema rustroad.design.v1).

usage: scale_manifests.py --rustroad BIN --rustsys BIN [--design NAME ...]

For every design: writes the clock SDC (fixtures/designs/<name>/cts.sdc),
synthesizes the RTL with the cell-policy RustSys arguments (or takes the
literal mapped netlist), runs `rustroad floorplan` with the recipe below into
fixtures/designs/<name>/<pdk>-{floorplan,physical}.json and records every
tracked file and frozen PDK input by SHA-256 in fixtures/designs/<name>.json.
scripts/scale_flow.py regenerates the floorplans and requires identical bytes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from scale_flow import SITES, CELL_GDS, deconflict  # noqa: E402

RUSTSYS = "fixtures/designs/rtl/PROVENANCE.md"
DESIGNS = {
    "counter": dict(top="counter", rtl=["counter/counter.sv"], clock="clk_i",
                    description="8-bit wrapping counter with enable, clear and async reset (RustSys fixture)."),
    "uart_tx": dict(top="uart_tx", rtl=["uart_tx/uart_tx.sv"], clock="clk_i",
                    description="UART transmitter FSM, 4 clocks per bit, async reset (RustSys fixture)."),
    "fifo": dict(top="fifo", rtl=["fifo/fifo.sv"], clock="clk_i",
                 description="4x8 synchronous FIFO with count/full/empty, async reset (RustSys fixture)."),
    "alu": dict(top="alu", rtl=["alu/wt_pkg.sv", "alu/alu.sv"], clock=None,
                description="8-bit combinational ALU, 8 operations (RustSys fixture); no clock."),
    "regfile": dict(top="regfile", rtl=["regfile/regfile.sv"], clock="clk",
                    description="8x8 register file, two read ports, async reset (RustSys fixture)."),
    "mult": dict(top="mult", rtl=["mult/mult.sv"], clock="clk",
                 description="6x6 signed registered and unsigned combinational multipliers (RustSys fixture)."),
    "dense": dict(top="lane_smoke", rtl=["dense/dense.sv"], clock="clk", literal=True,
                  description="Dense six-register fixture: literal mapped netlists per PDK (tap-release fixture)."),
    "pipeline_track": dict(top="lane_smoke", rtl_paths=["fixtures/flow/pipeline.v"], clock="clk",
                           description="fixtures/flow/pipeline.v through the scale flow (track router)."),
}
RECIPE = dict(utilization=0.5, aspect=1.0, margin_um=10.0, command="rustroad floorplan")
CORNERS = {
    "sky130": [dict(name="tt", liberty="tt_025C_1v80", rc="nom"),
               dict(name="ss", liberty="ss_100C_1v60", rc="max"),
               dict(name="ff", liberty="ff_n40C_1v95", rc="min")],
    "gf180": [dict(name="tt", liberty="tt_025C_3v30", rc="nom"),
              dict(name="ss", liberty="ss_125C_3v00", rc="max"),
              dict(name="ff", liberty="ff_n40C_3v60", rc="min")],
}
LIBRARY = {"sky130": "sky130_fd_sc_hd", "gf180": "gf180mcu_fd_sc_mcu7t5v0"}
VARIANT = {"sky130": "sky130A", "gf180": "gf180mcuD"}
# OpenRCX rule files the timing flow reads (sky130: Calibre-derived set).
RCX_SUFFIX = {"sky130": ".calibre", "gf180": ""}
SDC = """set_units -time ns -capacitance pF
create_clock -name core -period 10 -waveform {{0 5}} [get_ports {port}]
set_clock_transition 0.1 [get_clocks core]
set_clock_uncertainty 0.05 [get_clocks core]
set_max_capacitance 0.3 [current_design]
set_max_transition 1.0 [get_clocks core]
"""
IO_TEMPLATE = """set_input_delay 0.2 -clock ${clock} [get_ports {${data_inputs}}]
set_output_delay 0.2 -clock ${clock} [get_ports {${outputs}}]
set_input_transition 0.1 [get_ports {${inputs}}]
set_load 0.01 [get_ports {${outputs}}]
set_propagated_clock [get_clocks ${clock}]
"""


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def entry(rel: str) -> dict:
    return dict(path=rel, sha256=sha256(ROOT / rel))


def pdk(tag: str) -> dict:
    data = json.loads((ROOT / "fixtures/pdk/local-real-pdks.json").read_text())
    return [p for p in data["pdks"] if p["tag"] == tag][0]


def frozen(tag: str, physical: str, routing: str) -> dict:
    p = pdk(tag)
    host = Path(json.loads((ROOT / "fixtures/flow/signoff-pinned.json").read_text())["pdks"][tag]["host_root"])
    lib_dir = Path(p["liberty"]["path"]).parent
    out = dict(cell_gds=sha256(host / CELL_GDS[tag]), cell_lef=p["cell_lef"]["sha256"],
               technology_lef=p["technology_lef"]["sha256"],
               cell_policy=sha256(ROOT / "fixtures/flow/cell-policy.json"),
               delay_cells=sha256(ROOT / f"fixtures/pdk/{tag}-delay-cells.json"),
               gds_layer_map=sha256(ROOT / f"fixtures/pdk/{tag}-gds-map.json"),
               physical_config=sha256(ROOT / physical), routing_profile=sha256(ROOT / routing))
    for c in CORNERS[tag]:
        out[f"liberty_{c['name']}"] = sha256(lib_dir / f"{LIBRARY[tag]}__{c['liberty']}.lib")
        out[f"rc_profile_{c['name']}"] = sha256(ROOT / f"fixtures/pdk/{tag}-rc-{c['rc']}.json")
        out[f"openrcx_rules_{c['name']}"] = sha256(
            host / "libs.tech/librelane" / f"rules.openrcx.{VARIANT[tag]}.{c['rc']}{RCX_SUFFIX[tag]}")
    return dict(sorted(out.items()))


def build(name: str, spec: dict, args) -> dict:
    d = ROOT / "fixtures/designs" / name
    d.mkdir(parents=True, exist_ok=True)
    rtl = spec.get("rtl_paths") or [f"fixtures/designs/rtl/{x}" for x in spec["rtl"]]
    sdc_rel = None
    if spec["clock"]:
        (d / "cts.sdc").write_text(SDC.format(port=spec["clock"]))
        sdc_rel = f"fixtures/designs/{name}/cts.sdc"
    else:
        # Combinational design: a virtual clock for I/O constraints only.
        (d / "cts.sdc").write_text("set_units -time ns -capacitance pF\ncreate_clock -name core -period 10\n"
                                   "set_max_capacitance 0.3 [current_design]\n")
        sdc_rel = f"fixtures/designs/{name}/cts.sdc"
    tmpl = ROOT / "fixtures/designs/postroute-io.sdc.tmpl"
    if not tmpl.exists():
        tmpl.write_text(IO_TEMPLATE)
    pdks = {}
    policy = json.loads((ROOT / "fixtures/flow/cell-policy.json").read_text())
    for tag in ("sky130", "gf180"):
        p = pdk(tag)
        profile = f"fixtures/pdk/{tag}-scale-physical.json"
        routing = f"fixtures/pdk/{tag}-scale-routing.json"
        with tempfile.TemporaryDirectory(dir=ROOT / ".work") as tmp:
            tmp = Path(tmp)
            mapped = tmp / "mapped.v"
            if spec.get("literal"):
                mapped.write_bytes((ROOT / f"fixtures/designs/rtl/{name}/{tag}_mapped.v").read_bytes())
            else:
                subprocess.run([str(args.rustsys), "--threads", "2", "--progress", "off", "synth", "--backend",
                                "cpu", "--top", spec["top"], "--lib", p["liberty"]["path"],
                                *policy["pdks"][tag]["rustsys_args"], "--out", str(mapped), "--print", "json",
                                *[str(ROOT / r) for r in rtl]], check=True, capture_output=True)
            text, _ = deconflict(mapped.read_text())
            netlist = tmp / "netlist.v"
            netlist.write_text(text)
            clock = ["--no-clock"] if not spec["clock"] else ["--clock-port", spec["clock"]]
            floor, phys = d / f"{tag}-floorplan.json", d / f"{tag}-physical.json"
            for f in (floor, phys):
                if f.exists():
                    f.unlink()
            subprocess.run([str(args.rustroad), "floorplan", "--tech-lef", p["technology_lef"]["path"],
                            "--cell-lef", p["cell_lef"]["path"], "--netlist", str(netlist),
                            "--physical-config", str(ROOT / profile), "--routing-config", str(ROOT / routing),
                            "--site", SITES[tag], "--utilization", str(RECIPE["utilization"]),
                            "--aspect", str(RECIPE["aspect"]), "--margin-um", str(RECIPE["margin_um"]), *clock,
                            "--out-floorplan", str(floor), "--out-physical-config", str(phys)],
                           check=True, capture_output=True)
        physical = f"fixtures/designs/{name}/{tag}-physical.json"
        e = dict(floorplan=entry(f"fixtures/designs/{name}/{tag}-floorplan.json"),
                 physical_config=entry(physical), physical_profile=entry(profile),
                 routing_config=entry(routing), delay_cells=entry(f"fixtures/pdk/{tag}-delay-cells.json"),
                 gds_layer_map=entry(f"fixtures/pdk/{tag}-gds-map.json"),
                 rc_profiles={rc: entry(f"fixtures/pdk/{tag}-rc-{rc}.json") for rc in ("min", "nom", "max")},
                 corners=CORNERS[tag], frozen=frozen(tag, physical, routing))
        if spec.get("literal"):
            e["netlist"] = entry(f"fixtures/designs/rtl/{name}/{tag}_mapped.v")
        pdks[tag] = e
    return dict(
        schema="rustroad.design.v1", name=name, description=spec["description"], top=spec["top"],
        rtl=[entry(r) for r in rtl],
        clock=None if not spec["clock"] else dict(port=spec["clock"], name="core"),
        sdc=dict(clock=entry(sdc_rel), io_template=entry("fixtures/designs/postroute-io.sdc.tmpl"),
                 note="signoff SDC = clock SDC + io_template filled from the RTL port list; the CTS reads the "
                      "clock SDC" if spec["clock"] else "combinational: virtual clock for I/O constraints"),
        pdk_profiles=entry("fixtures/pdk/local-real-pdks.json"),
        cell_policy=entry("fixtures/flow/cell-policy.json"),
        signoff_manifest=entry("fixtures/flow/signoff-pinned.json"),
        tools=entry("fixtures/flow/local-pinned.json"),
        floorplan_recipe=RECIPE,
        flow=dict(harness="scripts/scale_flow.py", router="track", placer="analytic"),
        pdks=pdks,
        verification=dict(cosim_vectors=4096, cosim_seed="0x5eed2026c0ffee01", rustsys_budget=10000,
                          rustsys_model_budget=10000, rustsys_sweep_max_candidates=256, yosys_equiv_seq=2),
        budgets=dict(hold_margin_ns=0.05, setup_margin_ns=0.0, hold_eco_setup_margin_ns=0.05, parity_ns=0.001,
                     max_hold_iterations=4, process_timeout_s=600, process_memory_mib=2048, max_wall_s=3600,
                     container_memory="4g", container_timeout_s=1800),
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rustroad", type=Path, required=True)
    ap.add_argument("--rustsys", type=Path, required=True)
    ap.add_argument("--design", action="append")
    args = ap.parse_args()
    for name in args.design or list(DESIGNS):
        m = build(name, DESIGNS[name], args)
        (ROOT / "fixtures/designs" / f"{name}.json").write_text(json.dumps(m, indent=2) + "\n")
        print(name, "written")
    return 0


if __name__ == "__main__":
    sys.exit(main())
