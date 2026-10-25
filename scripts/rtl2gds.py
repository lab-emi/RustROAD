#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Unified RTL-to-GDS gate: one design manifest, both PDKs, every oracle.

usage: rtl2gds.py --design fixtures/designs/pipeline.json --pdk sky130,gf180
                  --run NEW_DIR --base-rev empty [--rustroad BIN]
                  [--tools fixtures/flow/rtl2gds-tools.json] [--tool NAME=PATH]...
                  [--no-controls] [--no-determinism] [--preflight-only]
                  [--evidence docs/evidence/rtl2gds.json]

Per PDK (docs/rtl2gds.md): RustSys synthesis with the cell policy -> pre-layout
CEC (RustSys check synth + Yosys) -> RustROAD import/place/CTS/route, native
check-antenna + antenna ECO and bounded hold closure (scripts/hold_closure.py
Flow, native SPEF decisions) -> native extraction min/nom/max + OpenRCX on the
exported DEF -> RustSTA 0.4 (three corners, --require-wire-model spef_rc) and
OpenSTA 3.1.0 on both SPEF sources -> native GDS -> XOR (geometry + labels)
against the KLayout DEF reference, Magic + KLayout DRC, antenna, density
(reported), Netgen + KLayout LVS against the powered netlist (scripts/signoff)
-> RTL vs final / powered netlist CEC (RustSys + Yosys) and a generic random
co-simulation -> artifact binding (SHA-256, RustROAD revision) re-verified at
the end -> determinism (a second full build compared byte for byte) ->
negative controls (each must be rejected by its own oracle).

Writes NEW_DIR/summary.json (schema rustroad.rtl2gds.v1) with every status
separately, the scope exclusions, end-to-end wall time and per-step peak RSS.
Exit 0 only when every gating status passed; 1 when any gating status failed;
3 when nothing failed but something is unverified (a missing tool, an
incomplete oracle). Third-party tools run only as black-box executables.
"""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import sys
import time
import traceback

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "scripts/signoff"))
from verify_physical import PhysicalHarness, IMAGE, OPENROAD  # noqa: E402
from verify_logic import bounded_text  # noqa: E402
import check_cell_policy  # noqa: E402
import compare_spef  # noqa: E402
import def2gds_reference  # noqa: E402
from hold_closure import CORNERS, Design, Flow  # noqa: E402
import rtl2gds_binding as B  # noqa: E402
import rtl2gds_controls as C  # noqa: E402
import rtl2gds_design as D  # noqa: E402
from rtl2gds_design import GateError, item  # noqa: E402
import rtl2gds_logic as L  # noqa: E402
import rtl2gds_timing as T  # noqa: E402
import signoff as signoff_harness  # noqa: E402

GATING = ["inputs_frozen", "implementation", "cell_policy", "cec_prelayout", "hold_closure", "antenna_native",
          "extraction", "timing_setup", "timing_hold", "mpw_min_period", "drv", "timing_accuracy", "gds_xor",
          "drc_magic", "drc_klayout", "antenna_deck", "lvs_netgen", "lvs_klayout", "cec_final", "cec_powered",
          "simulation", "cell_proofs", "artifact_binding", "determinism", "controls"]
REPORTED = ["density"]
SCOPE = dict(level="block/macro", pad_ring="not_applicable (block level, no pad ring)",
             seal_ring="not_applicable (block level, no seal ring)",
             density="reported separately (density_status); not part of the gate (owner decision)",
             ir_drop="unchecked", electromigration="unchecked", signal_integrity="unchecked (coupling grounded)",
             ocv="unchecked", cppr="unchecked", clocks="propagated, RustROAD CTS",
             async_reset_pulse_width="unchecked: minimum pulse width of asynchronous set/clear inputs is a "
                                     "requirement on the reset source outside the block (pins listed per PDK in "
                                     "mpw_min_period); their recovery/removal checks are timed",
             antenna="native check-antenna with a 0.10 safety margin + foundry deck (deck is the signoff gate)")
SIGNOFF_CHECKS = ["drc-magic", "drc-klayout", "antenna", "density", "lvs-netgen", "lvs-klayout", "xor"]


class Steps:
    """Wall time and peak RSS per step (process receipts + container receipts).

    A step that would start after the manifest's wall-clock budget is refused
    (GateError), so an overrunning gate ends unverified instead of hanging.
    """

    def __init__(self, deadline: float | None = None):
        self.rows: list[dict] = []
        self.deadline = deadline

    @contextmanager
    def __call__(self, h, pdk: str, name: str):
        if self.deadline is not None and time.monotonic() > self.deadline:
            raise GateError(f"wall-clock budget exhausted before step {pdk}:{name}")
        start, t0, extra = len(h.receipts), time.monotonic(), []
        try:
            yield extra
        finally:
            rss = [r["peak_rss_kib"] for r in h.receipts[start:] if r.get("peak_rss_kib") is not None]
            rss += [x for x in extra if x]
            self.rows.append(dict(pdk=pdk, step=name, wall_seconds=round(time.monotonic() - t0, 3),
                                  peak_rss_kib=max(rss, default=None), processes=len(h.receipts) - start,
                                  nested_peaks=len([x for x in extra if x])))


class Context:
    def __init__(self, args, h, manifest, tools, policy, ports, sdc_texts):
        self.args, self.h, self.manifest, self.tools, self.policy = args, h, manifest, tools, policy
        self.ports, self.sdc_texts = ports, sdc_texts  # signoff SDC per PDK (per-PDK clock period)
        self.steps = Steps(args.started + float(manifest["budgets"]["max_wall_s"]))
        self.make_harness = harness
        self.top = manifest["top"]
        self.rtl = [D.resolve(r["path"]) for r in manifest["rtl"]]
        self.budgets, self.verification = manifest["budgets"], manifest["verification"]
        self.clock = manifest["clock"]
        self.clocked = self.clock["port"] is not None
        # Combinational design whose RTL the pinned Yosys cannot elaborate:
        # the second CEC oracle is an exhaustive co-simulation (complete for
        # combinational logic) when the input space is small enough.
        self.exhaustive = None
        if not self.clocked:
            bits = sum(p["width"] for p in ports if p["direction"] == "input")
            if bits <= L.EXHAUSTIVE_MAX_INPUT_BITS:
                self.exhaustive = dict(ports=ports, input_bits=bits, seed=int(str(self.verification["cosim_seed"]), 0))
        self.tool_paths = {n: Path(t["path"]) for n, t in tools.items() if "path" in t}
        self.tool_paths["rustroad"] = args.rustroad
        self.signoff_manifest = D.resolve(manifest["signoff_manifest"]["path"])
        self.flow_args = argparse.Namespace(
            max_iterations=int(self.budgets["max_hold_iterations"]), target_margin=self.budgets["hold_eco_target_margin_ns"],
            setup_margin=self.budgets["hold_eco_setup_margin_ns"], physical_config={},
            gate_margin=self.budgets["hold_margin_ns"],
            cell_policy=D.resolve(manifest["cell_policy"]["path"]), manifest=self.signoff_manifest,
            openrcx=False, antenna_controls=False, signoff=False)


def harness(root: Path, budgets: dict) -> PhysicalHarness:
    return PhysicalHarness(root, argparse.Namespace(threads=2, memory_mib=int(budgets["process_memory_mib"]),
                                                    timeout_seconds=int(budgets["process_timeout_s"]),
                                                    tolerance_ns=budgets["parity_ns"]))


def guard(statuses: dict, keys: list[str], status: str, reason: str):
    for k in keys:
        statuses.setdefault(k, item(status, reason))


class PdkGate:
    def __init__(self, ctx: Context, tag: str, profile: dict):
        self.ctx, self.tag = ctx, tag
        self.spec = ctx.manifest["pdks"][tag]
        p = dict(profile)
        p["config"] = str(D.resolve(self.spec["physical_config"]["path"]))
        p["routing_config"] = str(D.resolve(self.spec["routing_config"]["path"]))
        p["cts_sdc"] = str(D.resolve(self.spec["sdc_clock"]["path"]))
        p["clock_port"] = ctx.clock["port"]
        self.p = p
        self.recipe = ctx.manifest["recipe"]["physical"]
        self.statuses: dict[str, dict] = {}
        self.details: dict = {}
        self.container_peaks: list = []  # peak RSS of every signoff container run (steps)
        want = [(c["name"], c["liberty"], c["rc"]) for c in self.spec["corners"]]
        if want != CORNERS[tag]:
            raise GateError(f"{tag}: manifest corners {want} differ from the closure engine {CORNERS[tag]}")
        self.corners = [c for c, _, _ in want]
        self.rc = {c: rc for c, _, rc in want}

    def flow(self, h) -> Flow:
        ctx = self.ctx
        literal = self.spec.get("netlist")
        profile = self.spec.get("physical_profile")
        sizing = self.spec.get("sizing_profile")
        design = Design(top=ctx.top, rtl=ctx.rtl, floorplan=D.resolve(self.spec["floorplan"]["path"]),
                        sdc_text=ctx.sdc_texts[self.tag], recipe=self.recipe, clocked=ctx.clocked,
                        netlist=D.resolve(literal["path"]) if literal else None,
                        physical_profile=D.resolve(profile["path"]) if profile else None,
                        floorplan_recipe=D.floorplan_recipe(ctx.manifest, self.tag),
                        sizing_profile=D.resolve(sizing["path"]) if sizing else None)
        tools = {k: ctx.tool_paths[k] for k in ("yosys", "verilator", "opensta", "rustsys", "rustroad",
                                                  "rusteda", "ruststa")}
        return Flow(h, ctx.flow_args, tools, self.p, ctx.policy, design)

    # ------------------------------------------------------------------ build
    def build(self, h, label: str = "") -> dict:
        tag, steps, ctx = self.tag, self.ctx.steps, self.ctx
        flow = self.flow(h)
        out = dict(flow=flow, frozen_before=flow.frozen())
        with steps(h, tag, f"{label}synthesis"):
            out["mapped"] = flow.synth()
            out["renamed_wires"] = flow.renamed_wires
            out["port_buffers"] = flow.port_buffers
        if self.recipe == "track":
            with steps(h, tag, f"{label}floorplan"):
                out["floorplan_check"] = flow.reproduce_floorplan(out["mapped"])
        with steps(h, tag, f"{label}place_cts_route_closure"):
            out["iterations"], final = flow.close(out["mapped"])
        it, netlist, phys, sta, antenna = final
        out.update(final_iteration=it, final_mapped=netlist, phys=phys, sta=sta, antenna=antenna)
        with steps(h, tag, f"{label}openrcx") as extra:
            out["openrcx"] = {}
            for c in self.corners:
                rc = self.rc[c]
                spef, info = compare_spef.openrcx(h, f"{tag}_final", tag, rc, Path(phys["paths"]["def"]),
                                                  f"{tag}_openrcx_{rc}",
                                                  timeout_s=max(25, int(ctx.budgets["process_timeout_s"]) - 30),
                                                  file_limit_bytes=flow.sta_file_limit())
                out["openrcx"][c] = dict(path=spef, sha256=info["spef"]["sha256"], rules=info["rules"])
                extra.append(info["container_peak_rss_kib"])
        with steps(h, tag, f"{label}gds_export"):
            out["gds"], out["gds_export"] = flow.export_gds(f"{tag}_final_native", phys)
        art = {f"rtl.{i}": p for i, p in enumerate(self.ctx.rtl)}
        art.update({"sdc.rendered": flow.sdc, "netlist.mapped": out["mapped"], "netlist.final_mapped": netlist,
                    "netlist.final": Path(phys["paths"]["netlist"]), "netlist.powered": Path(phys["paths"]["powered"]),
                    "def": Path(phys["paths"]["def"]), "routes": Path(phys["paths"]["routes"]), "gds": out["gds"]})
        for rec in out["iterations"]:
            art[f"netlist.i{rec['iteration']}"] = Path(rec["netlist"]["path"])
        for c in self.corners:
            art[f"spef.native.{c}"] = Path(phys["spef"][c]["path"])
            art[f"spef.openrcx.{c}"] = out["openrcx"][c]["path"]
        out["artifacts"] = art
        return out

    # ------------------------------------------------------------------ verification
    def verify(self, b: dict):
        ctx, tag, h, st, steps = self.ctx, self.tag, self.ctx.h, self.statuses, self.ctx.steps
        flow, phys, sta = b["flow"], b["phys"], b["sta"]
        lib = flow.libs["tt"]
        final, powered = Path(phys["paths"]["netlist"]), Path(phys["paths"]["powered"])
        st["inputs_frozen"] = D.compare_frozen(self.spec["frozen"], b["frozen_before"])
        fpc = b.get("floorplan_check")
        impl_ok = fpc is None or fpc["status"] == "passed"
        st["implementation"] = item("passed" if impl_ok else "failed",
                                    None if impl_ok else f"track recipe: regenerated floorplan differs from the tracked one {fpc}",
                                    recipe=self.recipe, iterations=len(b["iterations"]), final_iteration=b["final_iteration"],
                                    routed_nets=phys["routed_nets"], wires=phys["wires"], vias=phys["vias"],
                                    patches=phys["patches"], floorplan_reproduced=fpc, renamed_escaped_wires=b["renamed_wires"],
                                    port_buffers=b.get("port_buffers"),
                                    clock_tree=phys.get("cts"), placement=phys.get("placement"), route=phys.get("route"),
                                    antenna_repair=phys.get("antenna_repair"))
        policy = ctx.flow_args.cell_policy
        with steps(h, tag, "cell_policy"):
            checks = {"synth": check_cell_policy.check(tag, "synth", [b["mapped"]], None, policy, None),
                      "physical": check_cell_policy.check(tag, "physical", [final, powered], b["mapped"], policy, None)}
            s = D.worst(c["status"] for c in checks.values())
            st["cell_policy"] = item(s, None if s == "passed" else str({k: (c["status"], c["violations"][:5], c["problems"][:5])
                                                                        for k, c in checks.items()}),
                                     checks={k: dict(status=c["status"], violations=c["violations"][:10]) for k, c in checks.items()})
        with steps(h, tag, "cec_prelayout"):
            pair = L.cec_pair(h, f"{tag}_cec_mapped", ctx.tool_paths, lib, ctx.rtl, ctx.top, b["mapped"], ctx.verification,
                              exhaustive=ctx.exhaustive)
            st["cec_prelayout"] = self.pair_status(pair, "RTL vs mapped netlist")
        st["hold_closure"] = self.closure_status(b)
        a = b["antenna"]
        st["antenna_native"] = item({"passed": "passed", "violated": "failed"}.get(a["status"], "unverified"),
                                    None if a["status"] == "passed" else f"check-antenna {a['status']}: {a['violated'][:3]}",
                                    worst=a["worst"], violations=a["violations"],
                                    margin=json.loads(Path(a["report"]).read_text())["safety_margin"])
        # Extraction completeness and SPEF identity (bound to the producers' receipts).
        native = {c: Path(phys["spef"][c]["path"]) for c in self.corners}
        rcx = {c: b["openrcx"][c]["path"] for c in self.corners}
        nb = T.spef_binding(native, {c: phys["spef"][c]["sha256"] for c in self.corners})
        rb = T.spef_binding(rcx, {c: b["openrcx"][c]["sha256"] for c in self.corners})
        st["extraction"] = item(D.worst([nb["status"], rb["status"]]), nb.get("reason") or rb.get("reason"),
                                native=dict(binding=nb, diagnostics={c: phys["spef"][c]["diagnostics"] for c in self.corners},
                                            rc_profiles={c: self.rc[c] for c in self.corners}),
                                openrcx=dict(binding=rb, rules={c: b["openrcx"][c]["rules"] for c in self.corners}))
        # Timing on both SPEF sources.
        with steps(h, tag, "timing"):
            async_pins = T.async_pulse_pins(final, flow.libs["tt"])
            sources = {}
            for src, spefs in (("native", native), ("openrcx", rcx)):
                try:
                    rust = T.ruststa(h, f"{tag}_final_{src}", ctx.tool_paths, flow.libs, final, flow.sdc, spefs,
                                     clock_model=flow.clock_model(), rc_limits=flow.sta_rc_limits(),
                         state_gib=flow.sta_state_gib(), file_limit_bytes=flow.sta_file_limit())
                    ref = {c: T.opensta(h, f"{tag}_final_{src}_opensta_{c}", ctx.tool_paths["opensta"], flow.libs[c],
                                        final, ctx.top, flow.sdc, spefs[c]) for c in self.corners}
                except GateError as error:
                    # One engine could not time this SPEF source: every timing
                    # status of the source is unverified (never passed); the
                    # other source and the remaining oracles still run.
                    sources[src] = T.unverified_source(f"{src} SPEF: {error}"[:600])
                    sources[src]["ruststa_metrics"] = sources[src]["ruststa_scope"] = None
                    continue
                sources[src] = T.evaluate_source(rust, ref, self.corners, ctx.budgets, clocked=ctx.clocked,
                                                 async_pins=async_pins)
                accuracy = sources[src]["timing_accuracy"]
                if (accuracy["status"] == "failed" and sources[src]["mpw_min_period"]["status"] == "passed"
                        and all(x[k]["status"] == "passed" for x in accuracy["corners"].values()
                                for k in ("endpoints", "drv"))):
                    from rtl2gds_mpw import prove_deviations
                    try:
                        declarations, attempts = prove_deviations(
                            h, f"{tag}_{src}", ctx.tool_paths,
                            json.loads(ctx.signoff_manifest.read_text())["pdks"][tag], tag, src, self.corners,
                            ctx.top, final, flow.libs, flow.sdc, spefs, rust, ref, ctx.budgets["parity_ns"])
                        sources[src] = T.evaluate_source(rust, ref, self.corners, ctx.budgets,
                                                         clocked=ctx.clocked, async_pins=async_pins,
                                                         declarations=declarations)
                        sources[src]["timing_accuracy"]["deviation_attempts"] = attempts
                    except GateError as error:
                        accuracy["deviation_attempts"] = [dict(status="unproven", reason=str(error))]
                sources[src]["ruststa_metrics"] = rust["gating"]["metrics"]
                sources[src]["ruststa_scope"] = rust["gating"]["scope"]
            combined = T.combine(sources)
            st.update(combined)
            self.details["timing_matrix"] = {s: v["matrix"] for s, v in sources.items()}
            self.details["timing_sources"] = {s: dict(metrics=v["ruststa_metrics"], scope=v["ruststa_scope"])
                                              for s, v in sources.items()}
        # Native GDS signoff against the KLayout reference streamout of the same DEF.
        with steps(h, tag, "signoff") as extra:
            ref = h.root / f"{tag}_final_reference.gds"
            receipt = def2gds_reference.stream(Path(phys["paths"]["def"]), tag, ref, ctx.signoff_manifest,
                                               timeout=int(ctx.budgets["container_timeout_s"]))
            extra.append((receipt.get("resources") or {}).get("peak_rss_kib"))
            self.details["reference_streamout"] = dict(status=receipt["status"], problems=receipt["problems"],
                                                       output=receipt.get("output"))
            if receipt["status"] != "passed":
                raise GateError(f"reference streamout {receipt['status']}: {receipt['problems']}")
            summary = self.signoff(b["gds"], powered, ref, f"{tag}_final_signoff", SIGNOFF_CHECKS)
            extra.append(summary["resources"]["peak_rss_kib"])
            self.reference_gds = ref
            m = {"xor": "gds_xor", "drc_magic": "drc_magic", "drc_klayout": "drc_klayout", "antenna": "antenna_deck",
                 "lvs_netgen": "lvs_netgen", "lvs_klayout": "lvs_klayout", "density": "density"}
            for key, name in m.items():
                s = summary["statuses"].get(key, "unverified")
                st[name] = item(s, None if s == "passed" else f"see {summary['verdicts'].get(key.replace('_', '-'))}",
                                count=summary["counts"].get(key.replace("_", "-")),
                                by_rule=summary["by_rule"].get(key.replace("_", "-")),
                                verdict=summary["verdicts"].get(key.replace("_", "-")))
            st["gds_xor"]["native_gds"] = dict(path=str(b["gds"]), export=b["gds_export"].get("output"))
            st["antenna_deck"]["tools"] = antenna_tools(summary)
            self.details["signoff_summary"] = summary.get("summary_path")
        with steps(h, tag, "cec_final"):
            st["cec_final"] = self.pair_status(L.cec_pair(h, f"{tag}_cec_final", ctx.tool_paths, lib, ctx.rtl, ctx.top,
                                                          final, ctx.verification, exhaustive=ctx.exhaustive),
                                               "RTL vs final netlist")
        with steps(h, tag, "cec_powered"):
            st["cec_powered"] = self.powered_cec(powered, lib)
        with steps(h, tag, "simulation"):
            st["simulation"] = L.cosim(h, f"{tag}_final", ctx.tool_paths, lib, ctx.rtl, ctx.top, final, ctx.ports,
                                       ctx.clock["port"], int(ctx.verification["cosim_vectors"]),
                                       int(str(ctx.verification["cosim_seed"]), 0))
        with steps(h, tag, "cell_proofs"):
            sink = next((e for e in sorted(sta["corners"]["tt"]["endpoints"]) if "/" in e), None)
            if sink is None:
                # Combinational design: the endpoints are output ports; take the
                # first cell-LEF INPUT pin of the final netlist instead.
                sink = L.first_input_pin(final, L.lef_pins(Path(self.p["cell_lef"]["path"])))
            if sink is None:
                raise GateError("no instance input pin for the ECO polarity control")
            flow.design.control_sink = tuple(sink.rsplit("/", 1))
            proofs = flow.cell_proofs(final)
            ok = proofs["status"] in ("proved", "no_cells_inserted") and all(
                v.get("status") == "proved" and v.get("wrong_polarity") == "rejected" for v in proofs["proofs"].values())
            st["cell_proofs"] = item("passed" if ok else "failed", None if ok else "an inserted cell is not a proven buffer",
                                     cells=proofs["cells"], clock_cells=proofs["clock_cells"],
                                     proofs={k: v.get("status") for k, v in proofs["proofs"].items()},
                                     native_wrong_polarity=proofs["native_wrong_polarity"])

    def pair_status(self, pair: dict, what: str) -> dict:
        second = L.second_oracle(pair)
        s = D.worst([pair["rustsys"]["status"], second["status"]])
        return item(s, None if s == "passed" else f"{what}: RustSys {pair['rustsys']['status']}, "
                                                  f"{second['oracle']} {second['status']}",
                    rustsys=pair["rustsys"], yosys=pair["yosys"], exhaustive=pair.get("exhaustive"),
                    second_oracle=second["oracle"])

    def powered_cec(self, powered: Path, lib: Path) -> dict:
        ctx, h, tag = self.ctx, self.ctx.h, self.tag
        pins = L.lef_pins(Path(self.p["cell_lef"]["path"]))
        globs = ctx.policy["pdks"][tag]["post_synthesis_insertable"]["physical_only"]
        supply = [self.p["power_net"], self.p["ground_net"]]
        text = bounded_text(powered)
        filtered, removed = L.physical_only_view(text, globs, pins)
        filtered, rails = L.rail_constant_view(filtered, pins, supply)
        f1 = h.root / f"{tag}_powered_logic.v"
        f1.write_text(filtered)
        unpowered, stripped = L.unpowered_view(filtered, pins, supply)
        f2 = h.root / f"{tag}_powered_unpowered.v"
        f2.write_text(unpowered)
        pair = L.cec_pair(h, f"{tag}_cec_powered", ctx.tool_paths, lib, ctx.rtl, ctx.top, f1, ctx.verification,
                          supply_ports=supply, yosys_netlist=f2, exhaustive=ctx.exhaustive)
        out = self.pair_status(pair, "RTL vs powered netlist")
        out.update(filtering=dict(physical_only=removed, rail_constants=rails, unpowered_view=stripped, supply_ports=supply,
                                  rustsys_input=str(f1), yosys_input=str(f2)))
        return out

    def closure_status(self, b: dict) -> dict:
        rows = []
        for rec in b["iterations"]:
            t = rec["timing"]
            he = rec.get("hold_eco") or {}
            se = rec.get("size_eco") or {}
            ar = (rec.get("physical") or {}).get("antenna_repair") or {}
            rows.append(dict(iteration=rec["iteration"], closed=t["closed"],
                             worst={c: dict(setup_ns=v["worst_setup_ns"], hold_ns=v["worst_hold_ns"])
                                    for c, v in t["corners"].items()},
                             antenna=rec["antenna"]["status"], hold_eco=he.get("totals") or he.get("reason"),
                             antenna_eco=rec.get("antenna_eco"), drv=rec.get("drv"),
                             antenna_jumpers=dict(status=ar.get("status"), jumpers=len(ar.get("jumpers") or []),
                                                  before=(ar.get("before") or {}).get("violations"),
                                                  after=(ar.get("after") or {}).get("violations"),
                                                  unrepaired=ar.get("unrepaired")) if ar else None,
                             size_eco={k: se.get(k) for k in ("resized", "candidates", "by_reason", "failing_setup_endpoints",
                                                              "drv_violations", "utilization_after", "buffers",
                                                              "buffer_nets", "driver_resizes", "library_limited",
                                                              "co_sizing")} if se else None,
                             hold_isolation={k: (rec.get("hold_isolation") or {}).get(k) for k in ("status", "buffers")}
                             if rec.get("hold_isolation") else None,
                             hold_points=len(he.get("points") or []) or None,
                             hold_infeasible=(he.get("infeasible") or [])[:8] or None,
                             net_weights=bool(rec.get("net_weights")), setup_stagnant=rec.get("setup_stagnant"),
                             score=rec.get("score"),
                             stopped=rec.get("stopped")))
        closed = b["sta"]["closed"]
        return item("passed" if closed else "failed",
                    None if closed else f"native RustSTA hold/setup not closed after {len(rows)} iteration(s)",
                    iterations=rows, max_iterations=self.ctx.flow_args.max_iterations,
                    final_iteration=b["final_iteration"],
                    selected=(b["iterations"][-1].get("selected_iteration") if b["iterations"] else None),
                    decision_spef="native rustroad extract", target_margin_ns=self.ctx.flow_args.target_margin,
                    gate_hold_margin_ns=self.ctx.budgets["hold_margin_ns"],
                    sizing=self.spec.get("sizing_profile") is not None)

    def signoff(self, gds: Path, powered: Path, against: Path | None, label: str, checks: list[str]) -> dict:
        ctx = self.ctx
        ns = argparse.Namespace(gds=gds, top=ctx.top, pdk=self.tag, out=ctx.h.root / label, netlist=powered,
                                against=against, against_top=ctx.top if against else None,
                                manifest=ctx.signoff_manifest, threads=2, memory=ctx.budgets["container_memory"],
                                timeout=int(ctx.budgets["container_timeout_s"]), plant=None)
        summary = signoff_harness.run_checks(ns, checks)
        self.container_peaks.append(summary["resources"]["peak_rss_kib"])
        path = ns.out / "signoff.json"
        summary["summary_path"] = str(path)
        path.write_text(json.dumps(summary, indent=2) + "\n")
        return summary


def antenna_tools(summary: dict) -> dict:
    """Per-tool antenna results: the PDK's primary deck gates, the other is reported."""
    try:
        v = json.loads(Path(summary["verdicts"]["antenna"]).read_text())
    except (KeyError, OSError, json.JSONDecodeError):
        return dict(status="unverified", reason="antenna verdict unreadable")
    out = {}
    for role in ("primary", "secondary"):
        r = (v.get("result") or {}).get(role) or {}
        out[role] = {k: r.get(k) for k in ("tool", "status", "violations", "total", "by_plane", "by_rule", "reason", "note")
                     if r.get(k) is not None}
    return out


def accuracy_evidence(pdks: list[dict]) -> list[dict]:
    """Exact rows behind every RustSTA-vs-OpenSTA parity failure."""
    rows = []
    for p in pdks:
        acc = p["statuses"].get("timing_accuracy") or {}
        for src, v in (acc.get("by_spef") or {}).items():
            for corner, x in (v.get("corners") or {}).items():
                for kind in ("endpoints", "pulse", "drv"):
                    if x[kind]["status"] not in ("passed", "deviation_declared"):
                        rows.append(dict(pdk=p["pdk"], spef=src, corner=corner, check=kind,
                                         max_difference_ns=x[kind].get("max_difference_ns"),
                                         tolerance_ns=x["endpoints"]["tolerance_ns"],
                                         worst_rows=x[kind].get("worst_rows"),
                                         missing=x[kind].get("missing") or x[kind].get("missing_or_extra")))
    return rows


# ---------------------------------------------------------------------------- driver

def revision(rustroad: Path) -> dict:
    out = B.git_identity()
    out["rustroad_sha256"] = D.sha256_file(rustroad) if rustroad.is_file() else None
    out["rustroad_path"] = str(rustroad)
    return out


def binding_inputs(ctx: Context, gate: PdkGate, b: dict) -> dict:
    art = dict(b["artifacts"])
    for key, fp in b["frozen_before"].items():
        art[f"input.{key}"] = Path(fp["path"])
    for name, path in ctx.tool_paths.items():
        art[f"tool.{name}"] = path
    art["design_manifest"] = ctx.args.design
    art["tools_manifest"] = ctx.args.tools
    for script in sorted((ROOT / "scripts").glob("rtl2gds*.py")) + [ROOT / "scripts/hold_closure.py",
                                                                     ROOT / "scripts/scale_flow.py",
                                                                     ROOT / "scripts/mpw_probe.py",
                                                                     ROOT / "scripts/mpw_circuit.py"]:
        art[f"harness.{script.name}"] = script
    return art


def run_pdk(ctx: Context, tag: str, profile: dict) -> dict:
    gate = PdkGate(ctx, tag, profile)
    st, h = gate.statuses, ctx.h
    started = time.monotonic()
    result = dict(pdk=tag)
    built = None
    try:
        built = gate.build(h)
    except Exception as error:  # noqa: BLE001 - the flow itself did not complete: a definitive failure
        st["implementation"] = item("failed", f"flow did not complete: {error}", traceback=traceback.format_exc()[-3000:])
    if built is not None:
        # Bind every artifact to the RustROAD revision before any oracle reads it.
        rev = revision(ctx.args.rustroad)
        record = B.bind(binding_inputs(ctx, gate, built), rev)
        (h.root / f"{tag}_binding.json").write_text(json.dumps(record, indent=1) + "\n")
        result["binding"] = dict(path=str(h.root / f"{tag}_binding.json"), digest=record["digest"],
                                 artifacts=len(record["artifacts"]), revision=rev)
        try:
            gate.verify(built)
        except Exception as error:  # noqa: BLE001 - an oracle that crashed proves nothing
            result["verification_error"] = dict(error=repr(error), traceback=traceback.format_exc()[-3000:])
        if not ctx.args.no_determinism:
            with ctx.steps(h, tag, "determinism_rerun") as extra:
                try:
                    (h.root / f"{tag}_rerun").mkdir()
                    h2 = harness(h.root / f"{tag}_rerun", ctx.budgets)
                    again = gate.build(h2, label="rerun_")
                    kinds = {k: "openrcx_spef" if k.startswith("spef.openrcx") else "bytes" for k in built["artifacts"]}
                    produced = {k: v for k, v in built["artifacts"].items() if not k.startswith(("rtl.", "sdc."))}
                    other = {k: v for k, v in again["artifacts"].items() if not k.startswith(("rtl.", "sdc."))}
                    st["determinism"] = B.compare_runs(produced, other, kinds)
                    st["determinism"]["rerun_dir"] = str(h2.root)
                    extra += [r["peak_rss_kib"] for r in h2.receipts if r.get("peak_rss_kib")]
                    result["rerun_receipts"] = len(h2.receipts)
                except Exception as error:  # noqa: BLE001
                    st["determinism"] = item("unverified", f"rerun did not complete: {error}",
                                             traceback=traceback.format_exc()[-2000:])
        if not ctx.args.no_controls:
            with ctx.steps(h, tag, "controls") as extra:
                first = len(gate.container_peaks)
                try:
                    st["controls"] = C.run_all(ctx, gate, built, record)
                except Exception as error:  # noqa: BLE001
                    st["controls"] = item("unverified", f"controls did not complete: {error}",
                                          traceback=traceback.format_exc()[-2000:])
                extra += gate.container_peaks[first:]
                extra += [c.get("peak_rss_kib") for c in (st["controls"].get("controls") or {}).values()]
        # Frozen inputs and every bound artifact re-verified at the very end.
        frozen_after = D.compare_frozen(gate.spec["frozen"], built["flow"].frozen())
        if "inputs_frozen" in st:
            st["inputs_frozen"] = item(D.worst([st["inputs_frozen"]["status"], frozen_after["status"]]),
                                       st["inputs_frozen"].get("reason") or frozen_after.get("reason"),
                                       before=st["inputs_frozen"], after=frozen_after)
        st["artifact_binding"] = B.verify(record, revision(ctx.args.rustroad))
    if "verification_error" in result:
        reason = "not reached: verification stopped at " + result["verification_error"]["error"]
    else:
        reason = "not reached: " + ((st.get("implementation") or {}).get("reason") or "earlier step did not complete")
    guard(st, GATING + REPORTED, "unverified", reason[:400])
    if ctx.args.no_controls:
        st["controls"] = item("unverified", "controls disabled (--no-controls)")
    if ctx.args.no_determinism:
        st["determinism"] = item("unverified", "determinism rerun disabled (--no-determinism)")
    gating = [st[k]["status"] if not (st[k]["status"] == "not_applicable" and k in D.NOT_APPLICABLE) else "passed"
              for k in GATING]
    result.update(status=D.worst("unverified" if x == "not_applicable" else x for x in gating), statuses=st,
                  details=gate.details,
                  wall_seconds=round(time.monotonic() - started, 3))
    return result


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0], formatter_class=argparse.RawDescriptionHelpFormatter,
                                 epilog=__doc__)
    ap.add_argument("--design", type=Path)
    ap.add_argument("--pdk", default="sky130,gf180", help="comma-separated PDK tags of the manifest")
    ap.add_argument("--run", type=Path, help="new evidence directory (must not exist)")
    ap.add_argument("--base-rev")
    ap.add_argument("--rustroad", type=Path, default=ROOT / "target/release/rustroad")
    ap.add_argument("--tools", type=Path, default=None, help="tool pin file (default: the manifest's)")
    ap.add_argument("--tool", action="append", default=[], metavar="NAME=PATH", help="override one pinned tool path")
    ap.add_argument("--no-controls", action="store_true")
    ap.add_argument("--no-determinism", action="store_true")
    ap.add_argument("--preflight-only", action="store_true")
    ap.add_argument("--evidence", type=Path, help="also write the compact tracked evidence JSON")
    ap.add_argument("--evidence-from", type=Path, metavar="SUMMARY",
                    help="only rewrite --evidence from an existing summary.json (no run)")
    args = ap.parse_args(argv)
    if args.evidence_from:
        if args.evidence is None:
            ap.error("--evidence-from needs --evidence")
        data = json.loads(args.evidence_from.read_text())
        args.evidence.write_text(json.dumps(evidence(data), indent=1, sort_keys=True, default=str) + "\n")
        return 0
    if args.design is None or args.run is None or args.base_rev is None:
        ap.error("--design, --run and --base-rev are required")
    if args.base_rev != "empty":
        print(json.dumps(dict(status="error", code="REVISION_CONFLICT",
                              message="a fresh gate run directory requires --base-rev empty")))
        return 2
    args.design, args.rustroad = args.design.resolve(), args.rustroad.resolve()
    root = args.run.resolve()
    root.mkdir(parents=True, exist_ok=False)
    lock = (root / "workspace.lock").open("wb")
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    (root / "HEAD").write_text("rev0\n")
    resource.setrlimit(resource.RLIMIT_AS, (8192 * 1024 ** 2, 8192 * 1024 ** 2))
    os.environ.update(GOMAXPROCS="2", QT_QPA_PLATFORM="offscreen", OMP_NUM_THREADS="2")
    started = args.started = time.monotonic()
    summary = dict(schema=D.SUMMARY_SCHEMA, status="unverified", exit_code=3, scope=SCOPE, gating=GATING,
                   reported=REPORTED, global_statuses={}, pdks=[])
    h = None
    ctx = None
    try:
        manifest = D.load_manifest(args.design)
        summary["design"] = dict(name=manifest["name"], top=manifest["top"], manifest=str(args.design),
                                 manifest_sha256=D.sha256_file(args.design), recipe=manifest["recipe"],
                                 stretch=manifest["stretch"], pdks_unavailable=manifest["pdks_unavailable"],
                                 clock=manifest["clock"])
        if manifest["clock"]["port"] is None:
            summary["scope"] = dict(SCOPE, clocks=f"virtual I/O clock {manifest['clock']['name']} only "
                                                  "(combinational design: no clock port, no clock tree, ideal clock model)")
        args.tools = (args.tools or D.resolve(manifest["tools"]["path"])).resolve()
        budgets = manifest["budgets"]
        h = harness(root, budgets)
        r = h.run("memory_preflight", ["free", "-h"])
        summary["memory_preflight"] = bounded_text(Path(r["stdout"]))
        summary["revision"] = revision(args.rustroad)
        overrides = dict(x.split("=", 1) for x in args.tool)
        tools = D.load_tools(args.tools, overrides)
        if not args.rustroad.is_file():
            tools["rustroad"] = dict(path=str(args.rustroad), status="missing", reason="rustroad binary missing")
        else:
            tools["rustroad"] = dict(path=str(args.rustroad), status="passed", actual_sha256=D.sha256_file(args.rustroad),
                                     note="the binary under test: bound by sha256 with the git revision, not pinned")
        g = summary["global_statuses"]
        g["tools_identity"] = D.tools_status(tools)
        pins = D.tool_pins_frozen(args.tools, manifest)
        if pins["status"] != "passed":
            g["tools_identity"] = item("failed", pins["reason"], tools=g["tools_identity"].get("tools"),
                                       pin_file=pins)
        if tools.get("openroad", {}).get("status") == "passed":
            r = h.run("openroad_identity", ["podman", "run", "--rm", "--pull=never", "--network=none", "--read-only",
                                            "--memory=2g", "--memory-swap=2g", "--entrypoint=/usr/bin/sha256sum", IMAGE, OPENROAD])
            got = bounded_text(Path(r["stdout"])).split()[0] if r["exit_code"] == 0 and bounded_text(Path(r["stdout"])).strip() else None
            want = tools["openroad"]
            ok = r["exit_code"] == 0 and got == want["sha256"] and want["image"] == IMAGE and want["executable"] == OPENROAD
            g["openroad_image"] = item("passed" if ok else "unverified" if got is None else "failed",
                                       None if ok else f"OpenROAD/OpenRCX identity {got} vs {want['sha256']}", image=IMAGE)
        else:
            g["openroad_image"] = item("unverified", "OpenROAD image entry missing from the tool pins")
        g["design_manifest"] = D.verify_tracked(manifest)
        g["budget_floors"] = D.budget_floors(manifest["budgets"], manifest["verification"])
        summary["tools"] = tools
        policy = json.loads(D.resolve(manifest["cell_policy"]["path"]).read_text())
        if policy["producer"]["binary"]["sha256"] != tools.get("rustsys", {}).get("expected_sha256"):
            g["design_manifest"] = item("failed", "cell-policy producer sha256 differs from the rustsys tool pin")
        ready = all(v["status"] == "passed" for v in g.values())
        # An uncommitted or untracked manifest still runs (development), but
        # its verdict can never be passed.
        g["manifest_provenance"] = D.manifest_provenance(args.design)
        if ready and not args.preflight_only:
            tool_paths = {n: Path(t["path"]) for n, t in tools.items() if "path" in t}
            rtl_files = [D.resolve(r["path"]) for r in manifest["rtl"]]
            ports, port_source = L.rtl_ports(h, tool_paths, rtl_files, manifest["top"])
            summary["port_source"] = port_source
            # One signoff SDC per PDK: the clock SDC carries the per-PDK period
            # of the owner rule (scripts/derive_clock_period.py), frozen by hash.
            sdc_texts, rendering = {}, {}
            for tag, spec in manifest["pdks"].items():
                sdc_texts[tag] = D.render_sdc(D.resolve(spec["sdc_clock"]["path"]).read_text(),
                                              D.resolve(manifest["sdc"]["io_template"]["path"]).read_text(), ports,
                                              manifest["clock"]["port"], manifest["clock"]["name"])
                got = hashlib.sha256(sdc_texts[tag].encode()).hexdigest()
                want = spec["frozen"]["sdc_postroute"]
                rendering[tag] = dict(sha256=got, frozen=want, period_ns=D.sdc_period(sdc_texts[tag]),
                                      status="passed" if got == want else "failed")
            bad = [t for t, r in rendering.items() if r["status"] != "passed"]
            g["sdc_rendering"] = item("passed" if not bad else "failed",
                                      None if not bad else f"rendered SDC differs from the frozen sha256: {bad}",
                                      pdks=rendering)
            g["clock_period"] = D.clock_period_status(manifest, tools)
            summary["ports"] = ports
            ctx = Context(args, h, manifest, tools, policy, ports, sdc_texts)
            profiles = json.loads(D.resolve(manifest["pdk_profiles"]["path"]).read_text())
            wanted = [t for t in args.pdk.split(",") if t]
            unknown = [t for t in wanted if t not in manifest["pdks"]]
            if unknown:
                raise GateError(f"--pdk names PDKs absent from the manifest: {unknown}")
            for tag in wanted:
                p = next(x for x in profiles["pdks"] if x["tag"] == tag)
                if g["sdc_rendering"]["status"] != "passed":
                    break
                summary["pdks"].append(run_pdk(ctx, tag, p))
        summary["requested_pdks"] = [t for t in args.pdk.split(",") if t]
    except Exception as error:  # noqa: BLE001 - a harness crash is never a pass
        summary["error"] = dict(error=repr(error), traceback=traceback.format_exc()[-4000:])
        summary["global_statuses"].setdefault("harness", item("unverified", f"harness error: {error!r}"))
    finally:
        per_pdk = {p["pdk"]: p["statuses"] for p in summary["pdks"]}
        verdict = D.overall(per_pdk, GATING, summary["global_statuses"],
                            summary.get("requested_pdks") or [t for t in args.pdk.split(",") if t])
        if args.preflight_only and verdict["status"] == "passed":
            verdict = dict(verdict, status="unverified", exit_code=3, unverified_items=["*:preflight_only"])
        summary.update(status=verdict["status"], exit_code=verdict["exit_code"], failed_items=verdict["failed_items"],
                       unverified_items=verdict["unverified_items"],
                       declared_deviations=verdict.get("declared_deviations", []),
                       not_applicable_items=verdict.get("not_applicable_items", []),
                       not_applicable_rules={k: D.NOT_APPLICABLE[k] for k in D.NOT_APPLICABLE})
        summary["timing_accuracy_status"] = D.worst((p["statuses"].get("timing_accuracy") or {}).get("status", "unverified")
                                                    for p in summary["pdks"]) if summary["pdks"] else "unverified"
        summary["timing_accuracy_failures"] = accuracy_evidence(summary["pdks"])
        summary["density_status"] = {p["pdk"]: p["statuses"].get("density", {}).get("status") for p in summary["pdks"]}
        receipts = h.receipts if h is not None else []
        steps = ctx.steps.rows if ctx is not None else []
        summary["resources"] = dict(
            wall_seconds=round(time.monotonic() - started, 3),
            peak_rss_kib=max([r["peak_rss_kib"] for r in receipts if r.get("peak_rss_kib")] +
                             [s["peak_rss_kib"] for s in steps if s.get("peak_rss_kib")], default=None),
            processes=len(receipts), steps=steps,
            guards=dict(gate_process_address_space_mib=8192, tool_process_address_space_mib=(
                ctx.budgets["process_memory_mib"] if ctx else None), tool_timeout_s=(
                ctx.budgets["process_timeout_s"] if ctx else None), single_tool_process_rss_limit_kib=2 * 1024 ** 2))
        (root / "summary.json").write_text(json.dumps(summary, indent=1, default=str) + "\n")
        (root / "HEAD").write_text("rev1\n")
        if args.evidence:
            args.evidence.parent.mkdir(parents=True, exist_ok=True)
            args.evidence.write_text(json.dumps(evidence(summary), indent=1, sort_keys=True, default=str) + "\n")
    print(json.dumps(dict(status=summary["status"], exit_code=summary["exit_code"],
                          failed=summary.get("failed_items"), unverified=summary.get("unverified_items")[:20],
                          not_applicable=summary.get("not_applicable_items"),
                          declared_deviations=summary.get("declared_deviations", []),
                          timing_accuracy_status=summary.get("timing_accuracy_status"),
                          summary=str(root / "summary.json"))))
    return summary["exit_code"]


def evidence(summary: dict) -> dict:
    """Compact, path-free view for docs/evidence (tracked): absolute paths are
    reduced to their file names; the run's summary.json keeps everything."""
    def strip(v):
        if isinstance(v, dict):
            return {k: strip(x) for k, x in v.items() if k not in ("path", "receipt", "traceback", "testbench",
                                                                  "rustsys_input", "yosys_input", "verdict",
                                                                  "rerun_dir", "rustroad_path")}
        if isinstance(v, list):
            return [strip(x) for x in v]
        if isinstance(v, str):
            return re.sub(r"(^|[\s'\"(=\[])(/[^\s'\"\],]+)", lambda m: m[1] + Path(m[2]).name, v)
        return v
    return strip({k: v for k, v in summary.items() if k not in ("memory_preflight",)})


if __name__ == "__main__":
    sys.exit(main())
