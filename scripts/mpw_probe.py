# SPDX-License-Identifier: Apache-2.0
"""Generate fresh, bounded evidence for a possible MPW reference-step deviation."""
from __future__ import annotations

import json
import math
from pathlib import Path
import re

from mpw_circuit import Circuit, table_1d
import rtl2gds_timing as T
import rtl2gds_deviation as D
from rtl2gds_design import GateError, sha256_file
from verify_logic import bounded_text

MAX_PROBES = 64
MAX_NETS = 8


def driver_waveforms(text: str) -> dict:
    """Read observable driver waveforms from the reference's public report."""
    result = {}
    for block in re.finditer(r"(?ms)^\S+ [v^] -> \S+ ([v^])\n(.*?)(?=^\.+\n|\Z)", text):
        inp = re.search(r"input_net_transition = ([\d.eE+-]+)", block[2])
        slew = re.search(r"Driver waveform slew = ([\d.eE+-]+)", block[2])
        if inp and slew:
            edge = "rise" if block[1] == "^" else "fall"
            result[edge] = dict(input_slew_ns=float(inp[1]), slew_ns=float(slew[1]))
    if set(result) != {"rise", "fall"} or any(
            not D.number(v) or v <= 0 for row in result.values() for v in row.values()):
        raise GateError("MPW probe could not read both reference driver waveforms")
    return result


def scale_resistance(source: Path, destination: Path, net_name: str, factor: float) -> None:
    if not math.isfinite(factor) or not .8 <= factor <= 1.2:
        raise GateError("MPW probe resistance is outside the nearby search bound")
    names, section, selected, changed, lines = {}, None, False, 0, []
    for line in bounded_text(source, 32*1024**2).splitlines():
        words = line.split()
        if not words:
            lines.append(line)
            continue
        key = words[0]
        if key == "*NAME_MAP":
            section = "names"
        elif key == "*D_NET":
            token = words[1]
            selected = names.get(token, token) == net_name
            section = "net"
        elif key == "*RES":
            section = "res"
        elif key in ("*CAP", "*CONN", "*END"):
            section = key
        elif section == "names" and key.startswith("*") and key[1:].isdigit() and len(words) == 2:
            names[key] = words[1]
        elif selected and section == "res" and len(words) == 4:
            value = float(words[3])
            if not math.isfinite(value) or value < 0:
                raise GateError("MPW probe encountered invalid resistance")
            words[3] = f"{value*factor:.15g}"
            line = " ".join(words)
            changed += 1
        lines.append(line)
    if not changed:
        raise GateError("MPW probe did not find the selected resistance network")
    destination.write_text("\n".join(lines) + "\n")


def pairs(rust: dict, reference: dict, corner: str, wanted: set[str]) -> dict:
    result = {}
    for row in rust["compat"]["reports"]["pulse_width"]["rows"]:
        if row["corner"] != corner:
            continue
        key = row["pin"] + (":high" if row["kind"].endswith("high") else ":low")
        if key not in wanted:
            continue
        other = reference["mpw"].get(key)
        if other is None:
            raise GateError("MPW probe reference omitted a target pin")
        value = dict(ruststa_required_ns=row["limit"], opensta_required_ns=other["required"],
                     ruststa_slack_ns=row["slack"], opensta_slack_ns=other["slack"])
        if key not in result or row["slack"] < result[key]["ruststa_slack_ns"]:
            result[key] = value
    if set(result) != wanted or any(D.difference(row) is None for row in result.values()):
        raise GateError("MPW probe target coverage is incomplete")
    return result


class Probe:
    def __init__(self, h, label, tools, pdk, tag, source, corner, top, netlist, lib, sdc, spef, rows):
        self.h, self.label, self.tools, self.pdk = h, label, tools, pdk
        self.tag, self.source, self.corner, self.top = tag, source, corner, top
        self.netlist, self.lib, self.sdc, self.spef = map(Path, (netlist, lib, sdc, spef))
        self.rows = rows
        self.wanted = {r["pin"] + (":high" if r["kind"].endswith("high") else ":low") for r in rows}
        self.net = rows[0]["net"]
        self.case = Circuit(self.pdk, self.lib, self.netlist, self.spef, self.net, (.1, .1))
        self.cache = {}
        self.records = []
        self.calibration = None

    def evaluate(self, factor=1.0, lib=None, netlist=None, name=None):
        key = round(factor, 12)
        if lib is None and key in self.cache:
            return self.cache[key]
        if len(self.records) >= MAX_PROBES:
            raise GateError("MPW probe work budget exceeded")
        name = name or f"{self.label}_q{len(self.records)}"
        spef = self.spef
        if factor != 1:
            spef = self.h.root / (name + ".spef")
            scale_resistance(self.spef, spef, self.net, factor)
        lib, netlist = Path(lib or self.lib), Path(netlist or self.netlist)
        rust = T.ruststa(self.h, name, self.tools, {self.corner: lib}, netlist, self.sdc,
                        {self.corner: spef})
        ref = T.opensta(self.h, name+"_ref", self.tools["opensta"], lib, netlist,
                       self.top, self.sdc, spef,
                       driver_arc=(self.case.driver+"/"+self.case.input_pin,
                                   self.case.driver+"/"+self.case.output_pin))
        observed = driver_waveforms(bounded_text(
            self.h.root/"receipts"/(name+"_ref")/"stdout.txt", 16*1024**2))
        value = dict(scale=factor, rows=pairs(rust, ref, self.corner, self.wanted),
                     reference_driver=observed,
                     workspace=rust["workspace"], source_spef=str(spef),
                     spef_sha256=sha256_file(spef),
                     raw_files=[str(self.h.root/"receipts"/(name+"_compat_pulse_width")/"stdout.txt"),
                                str(self.h.root/"receipts"/(name+"_ref")/"stdout.txt")])
        record = self.h.root / (name + ".json")
        record.write_text(json.dumps(value, indent=2, allow_nan=False)+"\n")
        value["record"] = str(record)
        self.records.append(record)
        if lib == self.lib:
            self.cache[key] = value
        return value

    def artifacts(self, prefix, selected):
        paths = [Path(p["record"]) for p in selected]
        paths += [Path(path) for p in selected for path in p["raw_files"]]
        paths += sorted(self.h.root.glob(prefix+"*.spice"))
        for kind in ("_char", "_rise", "_fall"):
            paths += [p for p in sorted(self.h.root.glob("receipts/"+prefix+kind+"*/*"))
                      if p.name in ("stdout.txt", "stderr.txt")]
        paths = sorted(set(paths))
        # Input SPEFs and full command receipts remain in the run. Keep bounded
        # observation and circuit records in each row's manifest.
        if len(paths) > 64:
            raise GateError("MPW evidence artifact budget exceeded")
        return [dict(path=str(p), name=p.name, sha256=sha256_file(p)) for p in paths]

    def prove(self):
        actual = self.evaluate()
        good = None
        for factor in (.99, 1.01, .97, 1.03, .95, 1.05, .9, 1.1, .85, 1.15, .8, 1.2):
            trial = self.evaluate(factor)
            if all(D.difference(r) <= D.NEARBY_TOLERANCE_NS for r in trial["rows"].values()):
                good = trial
                break
        if good is None:
            raise GateError("MPW probe found no nearby raw agreement within 0.2 ps")
        # Rise and fall driver steps need not occur at the same resistance.
        # Locate every affected row's own bracket; cached observations are shared.
        brackets = {}
        for key in sorted(self.wanted):
            good_point, bad_point = good, actual
            for _ in range(12):
                if abs(good_point["scale"]-bad_point["scale"]) <= D.MAX_BRACKET_WIDTH:
                    break
                trial = self.evaluate((good_point["scale"]+bad_point["scale"])/2)
                if D.difference(trial["rows"][key]) <= D.NEARBY_TOLERANCE_NS:
                    good_point = trial
                else:
                    bad_point = trial
            brackets[key] = sorted((good_point, bad_point), key=lambda p:p["scale"])
        case = self.case
        pin_report = T.run_json(self.h, self.label+"_input",
            [self.tools["ruststa"], "-w", actual["workspace"], "report", "--section", "pins",
             "--corner", self.corner, "--limit", "0", "--format", "json", "--threads", "1"])
        section = pin_report["sections"]["pins"]
        if section.get("truncated") or len(section["rows"]) != section["total"]:
            raise GateError("MPW driver-input report is incomplete")
        driver_pin = case.driver + "/" + case.input_pin
        found = [p for p in section["rows"] if p["pin"] == driver_pin]
        if len(found) != 1:
            raise GateError("MPW driver-input transition is unavailable")
        case.input_slew = (found[0]["slew_early_ns"], found[0]["slew_late_ns"])
        if not all(D.number(x) and x > 0 for x in case.input_slew):
            raise GateError("MPW input transition is invalid")
        char = case.characterize(self.h, self.tools["ngspice"], self.label+"_char")
        charfile = self.h.root / (self.label+"_characterization.json")
        charfile.write_text(json.dumps(char, indent=2, allow_nan=False)+"\n")
        self.records.append(charfile)
        lib, netlist = case.calibrated_library(char, self.h.root/(self.label+"_model"))
        self.records.extend((lib, netlist))
        calibrated = self.evaluate(lib=lib, netlist=netlist, name=self.label+"_calibrated")
        def node_name(pin):
            inst, terminal = pin.rsplit("/", 1)
            return inst + ":" + terminal
        wanted = sorted({node_name(row["pin"]) for row in self.rows})
        measurements = {}
        for edge in sorted({"rise" if row["kind"].endswith("high") else "fall" for row in self.rows}):
            measurements[edge] = []
            for index, slew in enumerate(case.input_slew):
                coarse = case.measure_network(self.h, self.tools["ngspice"],
                                              f"{self.label}_{edge}{index}_coarse", edge, slew, wanted)
                fine = case.measure_network(self.h, self.tools["ngspice"],
                                            f"{self.label}_{edge}{index}_fine", edge, slew, wanted, .00025)
                measurements[edge].append((coarse, fine))
        binding = dict(pdk=self.tag, corner=self.corner, spef_source=self.source, net=self.net,
                       netlist_sha256=sha256_file(self.netlist), liberty_sha256=sha256_file(self.lib),
                       sdc_sha256=sha256_file(self.sdc), spef_sha256=sha256_file(self.spef),
                       model_tree_sha256=case.model_identity["sha256"],
                       tools={k:sha256_file(Path(self.tools[k])) for k in
                              ("ruststa","rusteda","opensta","ngspice")},
                       probe_script_sha256=sha256_file(Path(__file__)),
                       circuit_script_sha256=sha256_file(Path(__file__).with_name("mpw_circuit.py")))
        out = {}
        for row in self.rows:
            key = row["pin"] + (":high" if row["kind"].endswith("high") else ":low")
            edge = "rise" if row["kind"].endswith("high") else "fall"
            lo, hi = brackets[key]
            artifacts = self.artifacts(self.label, (actual, good, lo, hi, calibrated))
            artifacts += [dict(path=str(p), name=p.name, sha256=sha256_file(p)) for p in (charfile, lib, netlist)]
            points = []
            terminal = node_name(row["pin"])
            inst, pin = row["pin"].rsplit("/", 1)
            xs, ys = table_1d(case.pin_models[case.instances[inst][0]][pin], row["kind"], case.library)
            slope = max(abs((ys[i+1]-ys[i])/(xs[i+1]-xs[i])) for i in range(len(xs)-1))
            for coarse, fine in measurements[edge]:
                measured = case.pulse_limit(row["pin"], row["kind"], fine["values"][terminal])
                coarse_limit = case.pulse_limit(row["pin"], row["kind"], coarse["values"][terminal])
                uncertainty = abs(measured-coarse_limit) + 2*slope*char["max_interpolation_error_ns"] + .00001
                points.append(dict(ruststa_required_ns=calibrated["rows"][key]["ruststa_required_ns"],
                                   opensta_required_ns=calibrated["rows"][key]["opensta_required_ns"],
                                   spice_required_ns=measured, uncertainty_ns=uncertainty))
            row_binding = dict(binding, pin=row["pin"], kind=row["kind"])
            proof = dict(schema=D.SCHEMA, binding=row_binding, original=actual["rows"][key],
                         nearby=dict(scale=good["scale"], **good["rows"][key]),
                         step=dict(lo=dict(scale=lo["scale"],**lo["rows"][key],
                                           opensta_driver_slew_ns=lo["reference_driver"][edge]["slew_ns"]),
                                   hi=dict(scale=hi["scale"],**hi["rows"][key],
                                           opensta_driver_slew_ns=hi["reference_driver"][edge]["slew_ns"])),
                         circuit=dict(model="transistor_rc_with_independent_driver_characterization",
                                      calibration_max_interpolation_error_ns=char["max_interpolation_error_ns"],
                                      points=points), artifacts=artifacts)
            checked = D.validate(proof, row_binding, actual["rows"][key], self.h.root)
            out[key] = dict(checked, proof=proof)
        return out
