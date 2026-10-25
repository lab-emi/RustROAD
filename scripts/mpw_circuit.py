# SPDX-License-Identifier: Apache-2.0
"""Bounded transistor-level experiments for a clock-net MPW discrepancy.

Only standard Liberty, Verilog, SPEF and SPICE data are used. Original PDK
files are read-only; characterization is a separately labelled experiment.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
import math
from pathlib import Path
import re
import sys

import compare_spef
import liberty_lite as LL
from rtl2gds_design import GateError, sha256_file
from verify_logic import bounded_text

# The main gate already imports scripts/signoff/signoff.py as ``signoff``.
# Use the same direct helper-module import as the other signoff callers.
sys.path.insert(0, str(Path(__file__).with_name("signoff")))
from manifest import tree_digest as model_tree_digest

MAX_NODES = 10000
MAX_ELEMENTS = 50000
MAX_CHARACTERIZATIONS = 128
MAX_BYTES = 64 * 1024 * 1024


def netlist_instances(path: Path) -> dict:
    result = {}
    for line in bounded_text(path, MAX_BYTES).splitlines():
        m = re.fullmatch(r"\s*(\S+)\s+(\S+)\s+\((.*)\);\s*", line)
        if m and m[1] not in ("module", "input", "output", "wire", "assign"):
            result[m[2].lstrip("\\")] = (
                m[1], {p: n.strip().lstrip("\\") for p, n in re.findall(r"\.(\w+)\(([^()]*)\)", m[3])})
    return result


def table_1d(pin: LL.Group, kind: str, library: LL.Library) -> tuple[list, list]:
    groups = [g for g in pin.sub("timing") if g.attr("timing_type") == "min_pulse_width"]
    if not groups:
        raise GateError("MPW oracle: no clock pulse group")
    tables = groups[-1].sub("rise_constraint" if kind.endswith("high") else "fall_constraint")
    if len(tables) != 1:
        raise GateError("MPW oracle: unsupported pulse table")
    t = tables[0]
    template = library.templates.get(t.name)
    if template is None or template.attr("variable_1") != "related_pin_transition":
        raise GateError("MPW oracle: pulse table must use related pin transition")
    indices = t.complex.get("index_1") or template.complex.get("index_1")
    if not indices or "index_2" in t.complex or "index_2" in template.complex:
        raise GateError("MPW oracle: only one-dimensional pulse tables are supported")
    xs = LL.numbers(",".join(indices[0]))
    ys = LL.numbers(",".join(t.complex["values"][0]))
    scale = library.time_unit_ns()
    if len(xs) != len(ys) or not 2 <= len(xs) <= 128:
        raise GateError("MPW oracle: invalid pulse table size")
    return [x * scale for x in xs], [y * scale for y in ys]


def interpolate(xs, ys, x):
    if not xs[0] <= x <= xs[-1]:
        raise GateError("MPW oracle: interpolation outside the characterized pulse table")
    for i in range(len(xs) - 1):
        if xs[i] <= x <= xs[i + 1]:
            return ys[i] + (ys[i + 1] - ys[i]) * (x - xs[i]) / (xs[i + 1] - xs[i])
    raise GateError("MPW oracle: malformed interpolation table")


@dataclass
class Circuit:
    pdk: dict
    library_path: Path
    netlist_path: Path
    spef_path: Path
    net_name: str
    input_slew: tuple[float, float]

    def __post_init__(self):
        self.instances = netlist_instances(self.netlist_path)
        parsed = compare_spef.parse_spef(self.spef_path)
        if self.net_name not in parsed["nets"]:
            raise GateError("MPW oracle: clock net absent from SPEF")
        self.net = parsed["nets"][self.net_name]
        drivers = [n for _, n, direction in self.net["conn"] if direction == "O"]
        if len(drivers) != 1 or ":" not in drivers[0]:
            raise GateError("MPW oracle: clock net needs one cell driver")
        self.driver_node = drivers[0]
        self.driver, self.output_pin = self.driver_node.rsplit(":", 1)
        self.master = self.instances[self.driver][0]
        wanted = {self.instances[n.rsplit(":", 1)[0]][0]
                  for _, n, _ in self.net["conn"] if ":" in n}
        if len(wanted) > 128:
            raise GateError("MPW oracle: cell-model budget exceeded")
        self.library = LL.Library(self.library_path, wanted)
        self.pin_models = {m: {p.name: p for p in c.sub("pin")}
                           for m, c in self.library.cells.items()}
        inputs = [p.name for p in self.library.cells[self.master].sub("pin")
                  if p.attr("direction") == "input"]
        if len(inputs) != 1:
            raise GateError("MPW oracle: only a single-input clock buffer/inverter is supported")
        self.input_pin = inputs[0]
        function = self.pin_models[self.master][self.output_pin].attr("function", "")
        function = re.sub(r"[\s()]", "", function)
        if function not in (self.input_pin, "!" + self.input_pin):
            raise GateError("MPW oracle: unsupported clock driver function")
        self.positive = function == self.input_pin
        self.nodes = set(self.net["ground"]) | {n for a, b, _ in self.net["res"] for n in (a, b)}
        self.nodes |= {n for _, n, _ in self.net["conn"]}
        if len(self.nodes) > MAX_NODES or len(self.net["res"]) > MAX_ELEMENTS:
            raise GateError("MPW oracle: RC circuit budget exceeded")
        if not self.net["res"] or not all(r >= 0 and math.isfinite(r) for _, _, r in self.net["res"]):
            raise GateError("MPW oracle: no valid resistance network")
        self.names = {name: f"n{i}" for i, name in enumerate(sorted(self.nodes))}
        self.pdk_root = Path(self.pdk["host_root"])
        self.cell_spice = self.pdk_root / self.pdk["files"]["cell_spice"]["path"]
        family = self.pdk["family"]
        if family != "sky130":
            raise GateError("MPW circuit declarations are qualified only for SKY130; keep the strict gate")
        self.model = self.pdk_root / "libs.tech/ngspice/sky130.lib.spice"
        if not self.model.is_file():
            raise GateError("MPW oracle: transistor corner model is unavailable")
        self.model_identity = self.pdk["trees"].get("ngspice_models", {})
        observed = model_tree_digest(self.pdk_root / self.model_identity.get("path", "missing"))
        if observed != (self.model_identity.get("sha256"), self.model_identity.get("files")):
            raise GateError("MPW oracle: transistor model tree is missing or differs from its pin")
        cell_text = bounded_text(self.cell_spice, MAX_BYTES)
        match = re.search(r"^\.subckt\s+" + re.escape(self.master) + r"\s+(.+)$", cell_text, re.M | re.I)
        if not match:
            raise GateError("MPW oracle: transistor driver cell is missing")
        self.spice_ports = match[1].split()
        self.voltage = float(self.library.header.attr("nom_voltage"))
        self.temperature = float(self.library.header.attr("nom_temperature"))
        operating = self.library.header.attr("default_operating_conditions", "")
        self.model_corner = operating.split("_", 1)[0]
        if self.model_corner not in ("tt", "ss", "ff"):
            raise GateError("MPW oracle: unsupported transistor corner")
        if not (0 < self.voltage < 10 and all(0 < s < 10 for s in self.input_slew)):
            raise GateError("MPW oracle: invalid voltage or input transition")

    def capacitances(self, transition: str) -> dict:
        caps = dict(self.net["ground"])
        for a, b, value in self.net["coupling"]:
            for name in (a, b):
                if name in self.nodes:
                    caps[name] = caps.get(name, 0.0) + value
        for _, node, direction in self.net["conn"]:
            if direction != "I":
                continue
            inst, pin = node.rsplit(":", 1)
            model = self.pin_models[self.instances[inst][0]][pin]
            value = model.attr(transition + "_capacitance") or model.attr("capacitance")
            if value is None:
                raise GateError("MPW oracle: sink input capacitance is unavailable")
            caps[node] = caps.get(node, 0.0) + float(value) * self.library.cap_unit_pf()
        if any(not math.isfinite(c) or c < 0 for c in caps.values()):
            raise GateError("MPW oracle: invalid capacitance")
        return caps

    def header(self, title: str) -> list:
        for path in (self.model, self.cell_spice):
            if any(c in str(path) for c in ('"', "\n", "\r")):
                raise GateError("MPW oracle: unsafe model path")
        return [title, f'.lib "{self.model}" {self.model_corner}', f'.include "{self.cell_spice}"',
                f".temp {self.temperature:.12g}",
                ".option reltol=1e-7 abstol=1e-14 vntol=1e-9",
                f"Vsupply vdd 0 {self.voltage:.12g}"]

    def driver_line(self, label, input_node, output_node):
        values = {self.input_pin: input_node, self.output_pin: output_node,
                  "VPWR": "vdd", "VPB": "vdd", "VGND": "0", "VNB": "0",
                  "VDD": "vdd", "VSS": "0", "VNW": "vdd", "VPW": "0"}
        if any(p not in values for p in self.spice_ports):
            raise GateError("MPW oracle: unknown transistor cell port")
        return "X" + label + " " + " ".join(values[p] for p in self.spice_ports) + " " + self.master

    def stimulus(self, label, node, slew):
        duration = slew / 0.6
        return f"V{label} {node} 0 PULSE(0 {self.voltage:.12g} 1n {duration:.15g}n {duration:.15g}n 2n 10n)"

    def measures(self, label, input_node, output_node):
        v = self.voltage
        input_rise, input_fall = ("RISE", "FALL") if self.positive else ("FALL", "RISE")
        return [
            f".meas tran {label}_rise TRIG v({output_node}) VAL={.2*v:.15g} RISE=1 TARG v({output_node}) VAL={.8*v:.15g} RISE=1",
            f".meas tran {label}_fall TRIG v({output_node}) VAL={.8*v:.15g} FALL=1 TARG v({output_node}) VAL={.2*v:.15g} FALL=1",
            f".meas tran {label}_delay_rise TRIG v({input_node}) VAL={.5*v:.15g} {input_rise}=1 TARG v({output_node}) VAL={.5*v:.15g} RISE=1",
            f".meas tran {label}_delay_fall TRIG v({input_node}) VAL={.5*v:.15g} {input_fall}=1 TARG v({output_node}) VAL={.5*v:.15g} FALL=1",
        ]

    @staticmethod
    def footer(step_ns):
        return [f".tran {step_ns:.15g}n 8n", ".control", "set numdgt=17", "run", "quit", ".endc", ".end"]

    def characterize(self, harness, tool: Path, label: str) -> dict:
        totals = [sum(self.capacitances(edge).values()) for edge in ("rise", "fall")]
        total = max(totals)
        # A broad lower-load set plus a dense grid near the real total load.
        loads = sorted(set([0.0, total*.25, total*.5, total*.75] +
                           [total * (.8 + .4*i/24) for i in range(25)]))
        slews = [min(self.input_slew)*.995, max(self.input_slew)*1.005]
        if len(loads)*len(slews) > MAX_CHARACTERIZATIONS:
            raise GateError("MPW oracle: characterization budget exceeded")
        lines = self.header("MPW driver characterization, separate from the timed library")
        labels = []
        for i, slew in enumerate(slews):
            for j, load in enumerate(loads):
                name, vin, vout = f"c{i}_{j}", f"i{i}_{j}", f"o{i}_{j}"
                lines += [self.stimulus(name, vin, slew), self.driver_line(name, vin, vout)]
                if load:
                    lines.append(f"C{name} {vout} 0 {load:.15g}p")
                lines += self.measures(name, vin, vout)
                labels.append(name)
        lines += self.footer(.0005)
        values, receipt = self.simulate(harness, tool, label, lines)
        tables = {key: [[values[f"c{i}_{j}_{key}"] for j in range(len(loads))]
                        for i in range(len(slews))] for key in ("rise", "fall", "delay_rise", "delay_fall")}
        training = [j for j, load in enumerate(loads)
                    if load < .8 * total or (j - 4) % 2 == 0 or j == len(loads)-1]
        validation = [j for j in range(len(loads)) if j not in training]
        error = 0.0
        for rows in tables.values():
            for row in rows:
                xs, ys = [loads[j] for j in training], [row[j] for j in training]
                error = max([error] + [abs(row[j] - interpolate(xs, ys, loads[j])) for j in validation])
        return dict(loads_pf=loads, input_slew_ns=slews, tables=tables,
                    training_indices=training, validation_indices=validation,
                    max_interpolation_error_ns=error,
                    receipt=receipt, model_sha256=sha256_file(self.model),
                    model_tree=self.model_identity,
                    cell_spice_sha256=sha256_file(self.cell_spice))

    def measure_network(self, harness, tool: Path, label: str, transition: str,
                        input_slew: float, targets: list[str], step_ns=.0005) -> dict:
        if len(targets) > 64 or any(pin not in self.names for pin in targets):
            raise GateError("MPW oracle: invalid measured pin list")
        lines = self.header("MPW actual extracted RC network")
        lines += [self.stimulus("input", "vin", input_slew),
                  self.driver_line("driver", "vin", self.names[self.driver_node])]
        for i, (a, b, value) in enumerate(self.net["res"]):
            lines.append(f"R{i} {self.names[a]} {self.names[b]} {max(value,1e-9):.15g}")
        for i, (name, value) in enumerate(self.capacitances(transition).items()):
            if value:
                lines.append(f"C{i} {self.names[name]} 0 {value:.15g}p")
        for i, name in enumerate(targets):
            lines += self.measures(f"p{i}", "vin", self.names[name])
        lines += self.footer(step_ns)
        values, receipt = self.simulate(harness, tool, label, lines)
        return dict(values={name: values[f"p{i}_{transition}"] for i, name in enumerate(targets)},
                    receipt=receipt, step_ns=step_ns)

    @staticmethod
    def simulate(harness, tool: Path, label: str, lines: list[str]):
        path = harness.root / (label + ".spice")
        path.write_text("\n".join(lines) + "\n")
        r = harness.run(label, [str(tool), "-n", "-b", str(path)], file_limit_bytes=16*1024*1024)
        text = bounded_text(Path(r["stdout"]), 16*1024*1024) + bounded_text(Path(r["stderr"]), 16*1024*1024)
        if r["exit_code"] or re.search(r"(?im)^(?:error|fatal)|failed to converge|timestep too small", text):
            raise GateError(f"MPW transistor experiment failed: {label}")
        values = {}
        for name, value in re.findall(r"(?m)^([a-z][a-z0-9_]*)\s*=\s*([-+0-9.eE]+)", text):
            value = float(value) * 1e9
            if not math.isfinite(value):
                raise GateError("MPW oracle: nonfinite measurement")
            values[name] = value
        required = [line.split()[2] for line in lines if line.startswith(".meas ")]
        if any(name not in values for name in required):
            raise GateError(f"MPW oracle: incomplete transient measurements in {label}")
        return values, dict(label=label, deck_sha256=sha256_file(path),
                            stdout_sha256=sha256_file(Path(r["stdout"])),
                            stderr_sha256=sha256_file(Path(r["stderr"])))

    def pulse_limit(self, pin: str, kind: str, slew_ns: float) -> float:
        inst, terminal = pin.rsplit("/", 1)
        model = self.pin_models[self.instances[inst][0]][terminal]
        xs, ys = table_1d(model, kind, self.library)
        return interpolate(xs, ys, slew_ns)

    def calibrated_library(self, characterization: dict, destination: Path) -> tuple[Path, Path]:
        """Create a labelled verification-only driver, retaining the original library data."""
        alias = "mpw_characterized_driver"
        slews, loads = characterization["input_slew_ns"], characterization["loads_pf"]
        rows = characterization["tables"]
        training = characterization.get("training_indices", list(range(len(loads))))
        loads = [loads[i] for i in training]
        rows = {key: [[row[i] for i in training] for row in table] for key, table in rows.items()}
        cap = self.pin_models[self.master][self.input_pin]
        cap_attrs = [f"{key} : {cap.attr(key)};" for key in
                     ("capacitance", "rise_capacitance", "fall_capacitance") if cap.attr(key) is not None]
        body = [
            "lu_table_template(mpw_characterized_table) {",
            "variable_1 : input_net_transition; variable_2 : total_output_net_capacitance;",
            "}",
            f"cell ({alias}) {{ area : 0;",
            f"pin ({self.input_pin}) {{ direction : input; {' '.join(cap_attrs)} }}",
            f'pin ({self.output_pin}) {{ direction : output; function : "{"" if self.positive else "!"}{self.input_pin}";',
            f'timing () {{ related_pin : "{self.input_pin}"; timing_type : combinational;',
            f'timing_sense : {"positive_unate" if self.positive else "negative_unate"};',
        ]
        scale = self.library.time_unit_ns()
        cap_scale = self.library.cap_unit_pf()
        for group, key in (("cell_rise","delay_rise"), ("cell_fall","delay_fall"),
                           ("rise_transition","rise"), ("fall_transition","fall")):
            body += [f"{group}(mpw_characterized_table) {{",
                     'index_1 ("'+", ".join(f"{x/scale:.15g}" for x in slews)+'");',
                     'index_2 ("'+", ".join(f"{x/cap_scale:.15g}" for x in loads)+'");',
                     'values ('+", ".join('"'+", ".join(f"{x/scale:.15g}" for x in row)+'"' for row in rows[key])+');',
                     "}"]
        body += ["}", "}", "}"]
        original = bounded_text(self.library_path, MAX_BYTES)
        end = original.rfind("}")
        if end < 0 or original[end+1:].strip():
            raise GateError("MPW oracle: malformed source library")
        destination.mkdir(parents=True, exist_ok=False)
        lib = destination / "characterized.lib"
        lib.write_text(original[:end] + "\n" + "\n".join(body) + "\n}\n")
        text = bounded_text(self.netlist_path, MAX_BYTES)
        pattern = r"(?m)^(\s*)" + re.escape(self.master) + r"(\s+" + re.escape(self.driver) + r"\s+\()"
        text, count = re.subn(pattern, lambda m: m[1] + alias + m[2], text)
        if count != 1:
            raise GateError("MPW oracle: driver instance rewrite is not unique")
        netlist = destination / "characterized.v"
        netlist.write_text(text)
        return lib, netlist
