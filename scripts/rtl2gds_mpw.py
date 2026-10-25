# SPDX-License-Identifier: Apache-2.0
"""Connect bounded MPW evidence generation to the strict timing gate."""
from __future__ import annotations

from collections import defaultdict
import json
from pathlib import Path

import mpw_probe
import rtl2gds_deviation as D
from rtl2gds_design import GateError, sha256_file

MAX_ROWS = 64


def prove_deviations(h, label, tools, pdk, tag, source, corners, top, netlist, libs,
                     sdc, spefs, rust, reference, tolerance):
    """Only measured failing MPW rows are candidates; no exception loosens a gate."""
    groups = defaultdict(list)
    originals = {}
    selected = {}
    for row in rust["compat"]["reports"]["pulse_width"]["rows"]:
        corner = row["corner"]
        if corner not in corners or row["kind"] not in ("min_pulse_width_high", "min_pulse_width_low"):
            continue
        key = row["pin"] + (":high" if row["kind"].endswith("high") else ":low")
        index = (corner, key)
        if index not in selected or row["slack"] < selected[index]["slack"]:
            selected[index] = row
    for (corner, key), row in selected.items():
        other = reference[corner]["mpw"].get(key)
        if not other:
            continue  # Coverage errors remain ordinary failures.
        values = dict(ruststa_required_ns=row["limit"], opensta_required_ns=other["required"],
                      ruststa_slack_ns=row["slack"], opensta_slack_ns=other["slack"])
        delta = D.difference(values)
        if delta is not None and delta > tolerance:
            originals[(corner, key)] = values
            groups[(corner, row["net"])].append(row)
    if len(groups) > mpw_probe.MAX_NETS or len(originals) > MAX_ROWS:
        raise GateError("MPW declaration net/row budget exceeded")
    results, attempts = {}, []
    for index, ((corner, net), rows) in enumerate(sorted(groups.items())):
        note = dict(corner=corner, net=net, rows=len(rows))
        try:
            probe = mpw_probe.Probe(h, f"{label}_mpw{index}", tools, pdk, tag, source, corner,
                                    top, netlist, libs[corner], sdc, spefs[corner], rows)
            candidates = probe.prove()
            accepted = results.setdefault(corner, {})
            for row in rows:
                key = row["pin"] + (":high" if row["kind"].endswith("high") else ":low")
                binding = dict(pdk=tag, corner=corner, spef_source=source, net=net, pin=row["pin"],
                               kind=row["kind"], netlist_sha256=sha256_file(netlist),
                               liberty_sha256=sha256_file(libs[corner]), sdc_sha256=sha256_file(sdc),
                               spef_sha256=sha256_file(spefs[corner]),
                               model_tree_sha256=pdk["trees"]["ngspice_models"]["sha256"],
                               tools={k:sha256_file(tools[k]) for k in ("ruststa", "rusteda", "opensta", "ngspice")},
                               probe_script_sha256=sha256_file(Path(mpw_probe.__file__)),
                               circuit_script_sha256=sha256_file(Path(mpw_probe.__file__).with_name("mpw_circuit.py")))
                proof = candidates[key]["proof"]
                # Revalidate against the GATE'S measurements, not the probe's fresh run.
                checked = D.validate(proof, binding, originals[(corner, key)], h.root)
                accepted[key] = dict(checked, proof=proof)
            record = h.root / f"{label}_mpw{index}_proofs.json"
            record.write_text(json.dumps({key:accepted[key] for key in candidates}, indent=2,
                                         allow_nan=False)+"\n")
            note.update(status="evidence_generated", valid=sum(
                bool(accepted[key]["valid"]) for key in candidates),
                proof_record=dict(path=str(record), sha256=sha256_file(record)),
                rejected_rows=[dict(check=key, reasons=accepted[key]["reasons"])
                               for key in candidates if not accepted[key]["valid"]])
        except (GateError, KeyError, OSError, ValueError) as error:
            note.update(status="unproven", reason=str(error))
        attempts.append(note)
    return results, attempts
