#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Planted controls for the independent cell-policy checker (scripts/check_cell_policy.py).

usage: cellpol_controls.py --mapped-gf180 MAPPED.v --mapped-sky130 MAPPED.v --out DIR

Adds one PDK-excluded cell (gf180 mux2_1, a pnr_exclude cell) to a real mapped
netlist in every instance syntax the checker must understand (plain, with a
(* attribute *), with a #(parameter) list, as the second instance of a
multi-instance statement, with escaped names), plus statements it cannot
certify (behavioural ``always``, a ``define``) and instances of cells that are
neither library cells nor modules defined in the netlist (a black-box wrapper,
an upper-cased excluded cell name). Expected: excluded cell -> failed;
uncertifiable statement or unknown cell -> unverified; unmodified netlists ->
passed.
Writes DIR/cellpol.json; exit 0 iff every control behaved as expected.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_cell_policy  # noqa: E402

P = "gf180mcu_fd_sc_mcu7t5v0__"
FORMS = {
    "plain": f"  {P}mux2_1 xplain (.I0(a[0]), .I1(a[1]), .S(en), .Z(n0));",
    "attribute": f"  (* keep *) {P}mux2_1 xattr (.I0(a[0]), .I1(a[1]), .S(en), .Z(n0));",
    "parameter": f"  {P}mux2_1 #(.X(1)) xparam (.I0(a[0]), .I1(a[1]), .S(en), .Z(n0));",
    "multi_instance": f"  {P}mux2_1 xm0 (.I0(a[0]), .Z(n0)), xm1 (.I0(a[1]), .Z(n1));",
    "escaped": f"  \\{P}mux2_1  \\x$esc  (.I0(a[0]), .Z(n0));",
}
UNCERTIFIABLE = {
    "always_block": "  always @(posedge clk) n0 <= a[0];",
    "define": "`define CELL mux2_1",
    "undefined_module": "  hidden_wrapper u_hw (.A(a[0]), .X(n0));",
    "unknown_case_cell": f"  {P.upper()}MUX2_1 xupper (.I0(a[0]), .Z(n0));",
}


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--mapped-gf180", type=Path, required=True)
    p.add_argument("--mapped-sky130", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    args = p.parse_args(argv)
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    base = args.mapped_gf180.read_text()
    if base.count("endmodule") != 1:
        raise SystemExit("expected a single-module mapped netlist")
    results = []

    def record(name, pdk, path, expected):
        r = check_cell_policy.check(pdk, "synth", [path], None, None, None)
        results.append(dict(control=name, pdk=pdk, expected=expected, status=r["status"],
                            ok=r["status"] == expected, violations=[v["cell"] for v in r["violations"]][:4],
                            problems=r["problems"][:2]))

    record("unmodified", "gf180", args.mapped_gf180, "passed")
    record("unmodified", "sky130", args.mapped_sky130, "passed")
    for name, line in {**FORMS, **UNCERTIFIABLE}.items():
        path = out / f"gf180_{name}.v"
        path.write_text(base.replace("endmodule", line + "\nendmodule", 1))
        record(name, "gf180", path, "failed" if name in FORMS else "unverified")
    ok = all(x["ok"] for x in results)
    report = dict(schema="rustroad.cell-policy-controls.v1", all_expected=ok, controls=results,
                  inputs={k: str(v.resolve()) for k, v in (("gf180", args.mapped_gf180), ("sky130", args.mapped_sky130))})
    (out / "cellpol.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(dict(all_expected=ok, controls={f"{x['pdk']}:{x['control']}": x["status"] for x in results})))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
