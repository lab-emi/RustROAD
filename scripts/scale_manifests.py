#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Write the track-recipe design manifests (schema rustroad.design.v2).

usage: scale_manifests.py --rustroad BIN --rustsys BIN [--design NAME ...]
       scale_manifests.py --refresh-manifest [--design NAME ...]

For every design: writes the clock SDC (fixtures/designs/<name>/cts.sdc; a
virtual-clock SDC for a combinational design), synthesizes the RTL with the
cell-policy RustSys arguments (or takes the literal mapped netlist), runs
`rustroad floorplan` with the recipe below into
fixtures/designs/<name>/<pdk>-{floorplan,physical}.json and records every
tracked file and frozen PDK input by SHA-256 in fixtures/designs/<name>.json,
including the SHA-256 of the signoff SDC rendered from the RTL port list
(pinned Yosys, scripts/rtl2gds_design.render_sdc). scripts/rtl2gds.py and
scripts/scale_flow.py regenerate the floorplans and require identical bytes.

``--refresh-manifest`` rewrites only the SDC files and manifest JSON of
existing designs (floorplans and physical configurations are kept byte for
byte; their hashes are re-read). The reference clock SDC (10 ns) is fixed
here; the per-PDK clock SDCs carry the period of the owner rule and come from
scripts/derive_clock_period.py --write, which must run first (after any cell
policy change, since synthesis changes). The SDC is fixed before any timing
closure and is never edited to make timing pass.
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
from scale_flow import SITES, CELL_GDS, buffer_port_aliases, deconflict, port_buffer_cell, port_tie_cells  # noqa: E402
import rtl2gds_design as D  # noqa: E402

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
                    physical_profile={"sky130": "fixtures/designs/regfile/sky130-profile.json"},
                    description="8x8 register file, two read ports, async reset (RustSys fixture); "
                                "SKY130 clock leaves drive at most four sinks."),
    "mult": dict(top="mult", rtl=["mult/mult.sv"], clock="clk",
                 physical_profile={"sky130": "fixtures/designs/mult/sky130-profile.json"},
                 description="6x6 signed registered and unsigned combinational multipliers (RustSys fixture); "
                             "SKY130 clock leaves drive at most four sinks to reduce clock loading."),
    "dense": dict(top="lane_smoke", rtl=["dense/dense.sv"], clock="clk", literal=True,
                  description="Dense six-register fixture: literal mapped netlists per PDK (tap-release fixture)."),
    "pipeline_track": dict(top="lane_smoke", rtl_paths=["fixtures/flow/pipeline.v"], clock="clk",
                           description="fixtures/flow/pipeline.v through the scale flow (track router)."),
    # Stretch designs.
    "serv": dict(top="serv_rf_top", clock="clk", stretch=True,
                 rtl=[f"serv/serv_{n}.v" for n in (
                     "aligner", "alu", "bufreg", "bufreg2", "compdec", "csr", "ctrl", "debug", "decode",
                     "immdec", "mem_if", "rf_if", "rf_ram", "rf_ram_if", "rf_top", "state", "synth_wrapper",
                     "top")],
                 description="SERV bit-serial RV32I core with its register file (serv_rf_top, upstream "
                             "f200eb2, ISC); stretch."),
    "rv_core": dict(top="rv_core", rtl=["rv_core/rv_pkg.sv", "rv_core/rv_core.sv"], clock="clk_i",
                    stretch=True, physical_profile={"gf180": "fixtures/pdk/gf180-scale-physical-wide.json"},
                    routing_profile={"gf180": "fixtures/pdk/gf180-scale-routing-large.json"},
                    description="Single-cycle RV32I-subset core (RustSys fixture); stretch. GF180 uses a "
                                "25.2 um tap gap for its 24.1 um cells (checked by KLayout DF.13/DF.14) and "
                                "5x the routing work budgets (2e9 expansions, 2e11 comparisons)."),
}
RECIPE = dict(utilization=0.5, aspect=1.0, margin_um=10.0, command="rustroad floorplan")
# Per-PDK floorplan recipe (manifest pdks.<tag>.floorplan_recipe). GF180 at ss
# needs room for its closure ECOs: measured on regfile/alu/mult at the
# derived periods, DRV buffers, hold cells and resizes took the standard-cell
# utilization from 0.50 to the 0.74 cap (0.76 failed CTS packing) with
# violations left; from 0.35 they end at 0.49-0.63. SKY130 needs almost no
# ECO area (0-19 buffers per design) and keeps 0.5.
PDK_RECIPE = {"gf180": dict(utilization=0.35)}


def recipe_of(tag: str) -> dict:
    return dict(RECIPE, **PDK_RECIPE.get(tag, {}))
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
# Block-level constraints, fixed before closure for every track design: 100 MHz,
# 0.1 ns clock transition, 50 ps uncertainty; electrical DRV limits 0.3 pF and
# 1.0 ns (clock domain); set_max_fanout 64 is a structural bound (power of two,
# the same for every design, so the fanout check is active and gating; the
# largest fanout in the required set, regfile rst_n, has 64 sinks).
SDC = """set_units -time ns -capacitance pF
create_clock -name core -period 10 -waveform {{0 5}} [get_ports {port}]
set_clock_transition 0.1 [get_clocks core]
set_clock_uncertainty 0.05 [get_clocks core]
set_max_fanout 64 [current_design]
set_max_capacitance 0.3 [current_design]
set_max_transition 1.0 [get_clocks core]
"""
# Combinational design: a virtual clock only times the I/O (no clock port, no
# clock network, no CTS); same limits.
VIRTUAL_SDC = """set_units -time ns -capacitance pF
create_clock -name core -period 10
set_clock_uncertainty 0.05 [get_clocks core]
set_max_fanout 64 [current_design]
set_max_capacitance 0.3 [current_design]
set_max_transition 1.0 [current_design]
"""
IO_TEMPLATE = """set_input_delay 0.2 -clock ${clock} [get_ports {${data_inputs}}]
set_output_delay 0.2 -clock ${clock} [get_ports {${outputs}}]
set_input_transition 0.1 [get_ports {${inputs}}]
set_load 0.01 [get_ports {${outputs}}]
set_propagated_clock [get_clocks ${clock}]
"""
VIRTUAL_IO_TEMPLATE = """set_input_delay 0.2 -clock ${clock} [get_ports {${data_inputs}}]
set_output_delay 0.2 -clock ${clock} [get_ports {${outputs}}]
set_input_transition 0.1 [get_ports {${inputs}}]
set_load 0.01 [get_ports {${outputs}}]
"""
TOOLS = "fixtures/flow/rtl2gds-tools.json"
PROFILES = "fixtures/pdk/local-routing.json"


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
    write_sdc(name, spec)
    pdks = {}
    unavailable = {}
    policy = json.loads((ROOT / "fixtures/flow/cell-policy.json").read_text())
    for tag in ("sky130", "gf180"):
        p = pdk(tag)
        profile = (spec.get("physical_profile") or {}).get(tag, f"fixtures/pdk/{tag}-scale-physical.json")
        routing = (spec.get("routing_profile") or {}).get(tag, f"fixtures/pdk/{tag}-scale-routing.json")
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
            text, _ = buffer_port_aliases(text, port_buffer_cell(tag), port_tie_cells(tag))
            netlist = tmp / "netlist.v"
            netlist.write_text(text)
            clock = ["--no-clock"] if not spec["clock"] else ["--clock-port", spec["clock"]]
            floor, phys = d / f"{tag}-floorplan.json", d / f"{tag}-physical.json"
            for f in (floor, phys):
                if f.exists():
                    f.unlink()
            res = subprocess.run([str(args.rustroad), "floorplan", "--tech-lef", p["technology_lef"]["path"],
                                  "--cell-lef", p["cell_lef"]["path"], "--netlist", str(netlist),
                                  "--physical-config", str(ROOT / profile), "--routing-config",
                                  str(ROOT / routing), "--site", SITES[tag], "--utilization",
                                  str(recipe_of(tag)["utilization"]), "--aspect", str(recipe_of(tag)["aspect"]),
                                  "--margin-um", str(recipe_of(tag)["margin_um"]), *clock,
                                  "--out-floorplan", str(floor), "--out-physical-config", str(phys)],
                                 capture_output=True, text=True)
            if res.returncode != 0:
                if not spec.get("stretch"):
                    raise RuntimeError(f"{name} {tag}: floorplan failed: {res.stdout[-500:]}")
                unavailable[tag] = json.loads(res.stdout or "{}").get("message", res.stderr[-300:])
                continue
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
    m = dict(name=name, description=spec["description"], top=spec["top"], rtl=[entry(r) for r in rtl],
             stretch=bool(spec.get("stretch")), pdks_unavailable=unavailable, pdks=pdks)
    return finish(m, spec)


def write_sdc(name: str, spec: dict) -> None:
    d = ROOT / "fixtures/designs" / name
    d.mkdir(parents=True, exist_ok=True)
    (d / "cts.sdc").write_text(SDC.format(port=spec["clock"]) if spec["clock"] else VIRTUAL_SDC)
    for tmpl, text in (("postroute-io.sdc.tmpl", IO_TEMPLATE), ("postroute-io-virtual.sdc.tmpl", VIRTUAL_IO_TEMPLATE)):
        path = ROOT / "fixtures/designs" / tmpl
        if not path.exists():
            path.write_text(text)
        if path.read_text() != text:
            raise RuntimeError(f"{path} differs from the template in this script")


def rendered_sdc(m: dict, tag: str) -> str:
    """The signoff SDC of one PDK exactly as scripts/rtl2gds.py renders it (pinned
    Yosys port list, Verilator XML when Yosys cannot elaborate the RTL)."""
    import argparse as _argparse
    import rtl2gds_logic as L
    from verify_physical import PhysicalHarness
    tools = json.loads((ROOT / TOOLS).read_text())["tools"]
    paths = {}
    for name in ("yosys", "verilator"):
        paths[name] = D.resolve(tools[name]["path"])
        if sha256(paths[name]) != tools[name]["sha256"]:
            raise RuntimeError(f"pinned {name} differs from its sha256")
    with tempfile.TemporaryDirectory(dir=ROOT / ".work") as tmp:
        h = PhysicalHarness(Path(tmp), _argparse.Namespace(threads=2, memory_mib=2048, timeout_seconds=120,
                                                          tolerance_ns=0.001))
        ports, _ = L.rtl_ports(h, paths, [ROOT / r["path"] for r in m["rtl"]], m["top"])
    return D.render_sdc((ROOT / m["pdks"][tag]["sdc_clock"]["path"]).read_text(),
                        (ROOT / m["sdc"]["io_template"]["path"]).read_text(), ports,
                        m["clock"]["port"], m["clock"]["name"])


def period_files(name: str, tags) -> dict:
    """The owner clock-period rule's outputs (scripts/derive_clock_period.py --write)."""
    out = dict(period_derivation=f"fixtures/designs/{name}/clock-period.json")
    out.update({tag: f"fixtures/designs/{name}/{tag}-cts.sdc" for tag in tags})
    missing = [p for p in out.values() if not (ROOT / p).is_file()]
    if missing:
        raise RuntimeError(f"{name}: run scripts/derive_clock_period.py --write first (missing {missing})")
    return out


def finish(m: dict, spec: dict) -> dict:
    """Complete a track-recipe manifest: schema v2 fields, SDC and frozen hashes."""
    name = m["name"]
    sdc_rel = f"fixtures/designs/{name}/cts.sdc"
    tmpl = "fixtures/designs/postroute-io.sdc.tmpl" if spec["clock"] else "fixtures/designs/postroute-io-virtual.sdc.tmpl"
    periods = period_files(name, m["pdks"])
    out = dict(schema=D.DESIGN_SCHEMA, name=name, description=m["description"], top=m["top"], rtl=m["rtl"],
               clock=dict(port=spec["clock"] or None, name="core"),
               sdc=dict(reference_clock=entry(sdc_rel), io_template=entry(tmpl),
                        period_derivation=entry(periods["period_derivation"]),
                        note=("signoff SDC per PDK = pdks.<tag>.sdc_clock (reference_clock with the period of the "
                              "owner rule, period_derivation) + io_template filled from the RTL port list; the CTS "
                              "reads the clock SDC") if spec["clock"] else
                             ("combinational: a virtual clock times the I/O; no clock port, no CTS; per-PDK "
                              "period as above")),
               recipe=dict(physical="track", floorplan={k: RECIPE[k] for k in D.FLOORPLAN_RECIPE_KEYS}),
               stretch=bool(spec.get("stretch")), pdks_unavailable=m.get("pdks_unavailable") or {},
               pdk_profiles=entry(PROFILES), cell_policy=entry("fixtures/flow/cell-policy.json"),
               signoff_manifest=entry("fixtures/flow/signoff-pinned.json"), tools=entry(TOOLS))
    pdks = {}
    for tag, e in m["pdks"].items():
        e = {k: v for k, v in e.items()}
        e["sizing_profile"] = dict(path=f"fixtures/pdk/{tag}-sizing.json")
        for key in ("floorplan", "physical_config", "physical_profile", "routing_config", "delay_cells",
                    "gds_layer_map", "netlist", "sizing_profile"):
            if key in e:
                e[key] = entry(e[key]["path"])
        e["rc_profiles"] = {rc: entry(v["path"]) for rc, v in e["rc_profiles"].items()}
        e["sdc_clock"] = entry(periods[tag])
        if tag in PDK_RECIPE:
            e["floorplan_recipe"] = {k: recipe_of(tag)[k] for k in D.FLOORPLAN_RECIPE_KEYS}
        else:
            e.pop("floorplan_recipe", None)
        rendered = hashlib.sha256(rendered_sdc(dict(out, pdks={tag: e}), tag).encode()).hexdigest()
        e["frozen"] = dict(frozen(tag, e["physical_config"]["path"], e["routing_config"]["path"]),
                           sdc_cts=e["sdc_clock"]["sha256"], sdc_postroute=rendered,
                           sizing_profile=e["sizing_profile"]["sha256"])
        e["frozen"] = dict(sorted(e["frozen"].items()))
        pdks[tag] = e
    out["pdks"] = pdks
    out["verification"] = dict(cosim_vectors=4096, cosim_seed="0x5eed2026c0ffee01", rustsys_budget=10000,
                               rustsys_model_budget=10000, rustsys_sweep_max_candidates=256, yosys_equiv_seq=2)
    # Track designs: the closure loop also sizes gates, so it needs more
    # layouts (regfile gf180 took three hold rounds alone; 10 since the DRV
    # co-sizing round: GF180 alu and mult reached 1-2 nets at 1.00-1.06 ns
    # on layouts 7-8 of 8 while still improving); the ECO hold target
    # adds a 20 ps guard band for the native-vs-OpenRCX difference (the gate
    # still checks >= 0.05 ns on both SPEFs).
    out["budgets"] = dict(hold_margin_ns=0.05, hold_eco_target_margin_ns=0.07, setup_margin_ns=0.0,
                          hold_eco_setup_margin_ns=0.05, parity_ns=0.001,
                          max_hold_iterations=10, process_timeout_s=600, process_memory_mib=2048,
                          max_wall_s=14400 if spec.get("stretch") else 3600, container_memory="4g",
                          container_timeout_s=7200 if spec.get("stretch") else 1800)
    problems = D.manifest_problems(out)
    if problems:
        raise RuntimeError(f"{name}: {problems}")
    return out


def refresh_hashes(name: str) -> dict:
    """Legacy-recipe manifest (pipeline): re-read every tracked and frozen hash;
    nothing else changes (its reference SDC and floorplans are the fixtures'
    own). A v2 manifest is converted to v3 (per-PDK clock SDC of the owner
    clock-period rule)."""
    m = json.loads((ROOT / "fixtures/designs" / f"{name}.json").read_text())
    if m["recipe"]["physical"] != "legacy":
        raise RuntimeError(f"{name}: not a legacy-recipe manifest")
    periods = period_files(name, m["pdks"])
    if "clock" in m["sdc"]:
        old = m["sdc"]
        m["schema"] = D.DESIGN_SCHEMA
        m["sdc"] = dict(reference_clock=old["clock"], io_template=old["io_template"],
                        period_derivation=dict(path=periods["period_derivation"]),
                        note="signoff SDC per PDK = pdks.<tag>.sdc_clock (reference_clock with the period of the "
                             "owner rule, period_derivation) + io_template filled from the RTL port list; the CTS "
                             "reads the clock SDC")
        for tag, e in m["pdks"].items():
            e["sdc_clock"] = dict(path=periods[tag])
    for key, e in D.tracked_entries(m).items():
        e["sha256"] = sha256(ROOT / e["path"])
    for tag, e in m["pdks"].items():
        rendered = hashlib.sha256(rendered_sdc(m, tag).encode()).hexdigest()
        fresh = dict(frozen(tag, e["physical_config"]["path"], e["routing_config"]["path"]),
                     sdc_cts=e["sdc_clock"]["sha256"], sdc_postroute=rendered)
        if set(fresh) != set(e["frozen"]):
            raise RuntimeError(f"{name} {tag}: frozen keys differ {sorted(set(fresh) ^ set(e['frozen']))}")
        e["frozen"] = dict(sorted(fresh.items()))
    problems = D.manifest_problems(m)
    if problems:
        raise RuntimeError(f"{name}: {problems}")
    return m


def refresh(name: str, spec: dict) -> dict:
    """Rewrite the SDC and manifest of an existing design; floorplans are kept."""
    write_sdc(name, spec)
    old = json.loads((ROOT / "fixtures/designs" / f"{name}.json").read_text())
    return finish(dict(old, rtl=[entry(r["path"]) for r in old["rtl"]]), spec)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rustroad", type=Path)
    ap.add_argument("--rustsys", type=Path)
    ap.add_argument("--design", action="append")
    ap.add_argument("--refresh-manifest", action="store_true",
                    help="rewrite SDC + manifest only (keep floorplans and physical configurations)")
    args = ap.parse_args()
    if not args.refresh_manifest and (args.rustroad is None or args.rustsys is None):
        ap.error("--rustroad and --rustsys are required unless --refresh-manifest")
    for name in args.design or [d for d in DESIGNS if not DESIGNS[d].get("stretch")] + ["pipeline"]:
        if args.refresh_manifest and name not in DESIGNS:
            m = refresh_hashes(name)
        else:
            m = refresh(name, DESIGNS[name]) if args.refresh_manifest else build(name, DESIGNS[name], args)
        (ROOT / "fixtures/designs" / f"{name}.json").write_text(json.dumps(m, indent=2) + "\n")
        print(name, "written")
    return 0


if __name__ == "__main__":
    sys.exit(main())
