//! Self-authored hold-ECO planner fixtures: a 4-register netlist, synthetic
//! RustSTA-shaped reports and a three-cell profile. Each test writes < 64 KiB;
//! no PDK, timing engine or path enumeration is involved. Real-PDK closure is
//! verified separately by scripts/hold_closure.py with extracted parasitics.

use rustroad_workspace::eco::{HoldEcoRequest, apply_plan, plan_hold};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const NETLIST: &str =
    "module top(input wire clk, input wire en, input wire [1:0] a, output wire [1:0] y);
  wire \\q[0] ;
  wire n0;
  wire n1;
  AND2 g0 (.A(a[0]), .B(en), .X(n0));
  AND2 g1 (.A(a[1]), .B(en), .X(n1));
  DFF r0 (.D(n0), .CLK(clk), .Q(\\q[0] ));
  DFF r1 (.D(n1), .CLK(clk), .Q(y[1]));
  DFF r2 (.D(a[0]), .CLK(clk), .Q(y[0]));
endmodule
";

static NEXT: AtomicU64 = AtomicU64::new(0);
fn dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../.work/hold-eco-tests/{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn buffer(name: &str, area: f64, delays: [f64; 3], function: &str) -> Value {
    let rf = |d: f64| json!({"rise": d, "fall": d * 1.1});
    json!({
        "name": name, "input_pin": "A", "output_pin": "X", "function": function,
        "function_check": "identity", "timing_sense": "positive_unate", "area_um2": area,
        "lef_size_um": [1.0, 2.0], "policy_class": "cts_hold",
        "input_cap_pf": {"tt": 0.002, "ss": 0.002, "ff": 0.002},
        "delay_ns": {"tt": rf(delays[0]), "ss": rf(delays[1]), "ff": rf(delays[2])},
        "output_slew_ns": {"tt": rf(0.05), "ss": rf(0.08), "ff": rf(0.03)}
    })
}
fn profile(buffers: Vec<Value>) -> Value {
    json!({
        "schema": "rustroad.delay-cells.v1", "pdk": "toy", "library": "toy", "generator": "test",
        "sources": [], "cell_policy": {"path": "p.json", "sha256": "00"},
        "corners": [{"name": "tt", "liberty": "tt", "rc": "nom"}],
        "representative": {"input_slew_ns": 0.05, "load_pf": 0.005, "reason": "test"},
        "selection": {}, "buffers": buffers,
        "antenna_cells": [{"name": "DIODE", "pin": "DIODE", "area_um2": 2.0, "lef_size_um": [1.0, 2.0],
                           "lef_diff_area_um2": 0.4, "lef_gate_area_um2": 0.0, "policy_class": "physical_only"}],
        "excluded": []
    })
}
fn default_profile() -> Value {
    profile(vec![
        buffer("BUF1", 4.0, [0.10, 0.17, 0.07], "A"),
        buffer("DLY1", 9.0, [0.20, 0.38, 0.14], "(A)"),
        buffer("DLY3", 10.0, [0.55, 1.06, 0.38], "A"),
    ])
}
fn hold_report(margin: f64, rows: &[(&str, &str, f64, f64)], wire: &str) -> Value {
    let mut corners = Vec::new();
    for c in ["tt", "ss", "ff"] {
        let endpoints: Vec<Value> = rows
            .iter()
            .filter(|r| r.0 == c)
            .map(|r| {
                json!({"endpoint": r.1, "hold_slack_ns": r.2, "setup_slack_ns": r.3,
                            "required_delay_ns": margin - r.2})
            })
            .collect();
        corners.push(json!({"corner": c, "hold_endpoints": 3, "hold_violating": endpoints.len(),
                            "listed": endpoints.len(), "shown": endpoints.len(), "endpoints": endpoints}));
    }
    json!({"schema": "rusteda.v1", "ok": true,
           "scope": {"corner": "tt", "wire_model": wire, "clock_model": "propagated"},
           "sections": {"hold_fix": {"target_margin_ns": margin, "corners": corners}}})
}
fn setup_report(corner: &str, rows: &[(&str, f64)]) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| json!({"endpoint": r.0, "setup_slack_ns": r.1, "hold_slack_ns": 1.0}))
        .collect();
    json!({"schema": "rusteda.v1", "ok": true,
           "scope": {"corner": corner, "wire_model": "spef_rc", "clock_model": "propagated"},
           "sections": {"endpoints": {"total": rows.len(), "matched": rows.len(), "shown": rows.len(), "rows": rows}}})
}

struct Case {
    d: PathBuf,
    req: HoldEcoRequest,
}
fn write(d: &Path, name: &str, v: &Value) -> PathBuf {
    let p = d.join(name);
    fs::write(&p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    p
}
fn case(rows: &[(&str, &str, f64, f64)], setup_ss: f64, profile: Value) -> Case {
    let d = dir();
    let netlist = d.join("in.v");
    fs::write(&netlist, NETLIST).unwrap();
    let hold = write(&d, "hold.json", &hold_report(0.05, rows, "spef_rc"));
    let ss = write(
        &d,
        "setup_ss.json",
        &setup_report(
            "ss",
            &[
                ("r0/D", setup_ss),
                ("r1/D", setup_ss),
                ("r2/D", setup_ss),
                ("y[0]", 5.0),
            ],
        ),
    );
    let tt = write(
        &d,
        "setup_tt.json",
        &setup_report("tt", &[("r0/D", 9.0), ("r1/D", 9.0), ("r2/D", 9.0)]),
    );
    let prof = write(&d, "profile.json", &profile);
    let req = HoldEcoRequest {
        netlist,
        hold_reports: vec![hold],
        setup_reports: vec![ss, tt],
        profile: prof,
        hold_corners: vec!["tt".into(), "ss".into(), "ff".into()],
        setup_corner: "ss".into(),
        target_margin_ns: 0.05,
        setup_margin_ns: 0.05,
        max_chain: 4,
        max_total_chain: 8,
        max_sinks: 64,
        max_combinations: 100_000,
        prefix: "rr_eco_hold_".into(),
        out: d.join("out.v"),
        plan_out: d.join("plan.json"),
    };
    Case { d, req }
}

#[test]
fn smallest_covering_cell_per_sink_and_deterministic_output() {
    // r0 needs 0.054/0.15/0 -> BUF1 (0.10/0.17) covers; r1 needs 0.30 at ss and
    // 0.04 at ff -> two BUF1 (0.34, area 8) beat one DLY1 (0.38, area 9).
    let rows = [
        ("tt", "r0/D", -0.004, 9.0),
        ("ss", "r0/D", -0.10, 8.5),
        ("ss", "r1/D", -0.25, 8.5),
        ("ff", "r1/D", 0.01, 9.3),
    ];
    let c = case(&rows, 8.5, default_profile());
    let plan = plan_hold(&c.req).unwrap();
    assert_eq!(plan.status, "planned");
    assert_eq!(plan.sinks.len(), 2);
    assert_eq!(plan.sinks[0].endpoint, "r0/D");
    assert_eq!(plan.sinks[0].cells, ["BUF1"]);
    assert_eq!(plan.sinks[1].cells, ["BUF1", "BUF1"]);
    assert!((plan.sinks[1].corners["ss"].need_ns - 0.30).abs() < 1e-12);
    assert!((plan.sinks[1].estimated_setup_slack_ns - (8.5 - 2.0 * 0.17 * 1.1)).abs() < 1e-9);
    let out = fs::read_to_string(&c.req.out).unwrap();
    assert!(
        out.contains("BUF1 rr_eco_hold_0 (.A(n0), .X(rr_eco_hold_0_n));"),
        "{out}"
    );
    assert!(
        out.contains("DFF r0 (.D(rr_eco_hold_0_n), .CLK(clk), .Q(\\q[0] ));"),
        "{out}"
    );
    assert!(out.contains("BUF1 rr_eco_hold_1 (.A(n1), .X(rr_eco_hold_1_n));"));
    assert!(out.contains("BUF1 rr_eco_hold_2 (.A(rr_eco_hold_1_n), .X(rr_eco_hold_2_n));"));
    // Same inputs, same bytes.
    let first = fs::read(&c.req.out).unwrap();
    let mut again = c.req.clone();
    again.out = c.d.join("out2.v");
    again.plan_out = c.d.join("plan2.json");
    plan_hold(&again).unwrap();
    assert_eq!(first, fs::read(&again.out).unwrap());
    let a: Value = serde_json::from_slice(&fs::read(&c.req.plan_out).unwrap()).unwrap();
    assert_eq!(a["schema"], "rustroad.hold-eco-plan.v1");
    assert_eq!(a["totals"]["cells"], 3);
}

#[test]
fn chain_when_no_single_cell_covers_and_setup_bounds_it() {
    // ss needs 1.6 ns: DLY3 (1.06) alone fails; DLY3 + DLY1 (1.44) fails; two DLY3 cover.
    let rows = [("ss", "r0/D", -1.55, 6.0)];
    let c = case(&rows, 6.0, default_profile());
    let plan = plan_hold(&c.req).unwrap();
    // Conflicting setup slack between reports is refused.
    let c2 = case(&rows, 5.0, default_profile());
    assert!(
        plan_hold(&c2.req)
            .unwrap_err()
            .to_string()
            .contains("conflicting setup")
    );
    assert_eq!(plan.sinks[0].cells, ["DLY3", "DLY3"]);
    let out = fs::read_to_string(&c.req.out).unwrap();
    assert!(
        out.contains("DLY3 rr_eco_hold_1 (.A(rr_eco_hold_0_n), .X(rr_eco_hold_1_n));"),
        "{out}"
    );
    // Every covering chain charges >= 1.1 x 1.6 ns of setup at ss (fall = 1.1 x rise),
    // so 1.7 ns of setup slack leaves no feasible chain above the 0.05 ns margin.
    let c = case(&[("ss", "r0/D", -1.55, 1.7)], 1.7, default_profile());
    let e = plan_hold(&c.req).unwrap_err().to_string();
    assert!(e.contains("HOLD_ECO_INFEASIBLE"), "{e}");
    assert!(!c.req.out.exists());
    let plan: Value = serde_json::from_slice(&fs::read(&c.req.plan_out).unwrap()).unwrap();
    assert_eq!(plan["status"], "infeasible");
    // A chain cap of 1 cell is also infeasible.
    let mut c = case(&rows, 6.0, default_profile());
    c.req.max_chain = 1;
    assert!(plan_hold(&c.req).is_err());
    // An exhausted search budget is an explicit budget error.
    let mut c = case(&rows, 6.0, default_profile());
    c.req.max_combinations = 3;
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("max_combinations")
    );
    let mut c = case(&rows, 6.0, default_profile());
    c.req.max_chain = 9;
    assert!(plan_hold(&c.req).is_err());
}

#[test]
fn existing_chain_is_extended_and_capped() {
    let rows = [("ss", "r0/D", -0.10, 8.0)];
    let c = case(&rows, 8.0, default_profile());
    plan_hold(&c.req).unwrap();
    // Second iteration on the ECO netlist: r0/D still short at ss.
    let mut second = c.req.clone();
    second.netlist = c.req.out.clone();
    second.out = c.d.join("out_b.v");
    second.plan_out = c.d.join("plan_b.json");
    let plan = plan_hold(&second).unwrap();
    assert_eq!(plan.sinks[0].existing_chain, ["rr_eco_hold_0"]);
    let out = fs::read_to_string(&second.out).unwrap();
    assert!(
        out.contains("BUF1 rr_eco_hold_1 (.A(rr_eco_hold_0_n), .X(rr_eco_hold_1_n));"),
        "{out}"
    );
    let mut third = second.clone();
    third.netlist = second.out.clone();
    third.out = c.d.join("out_c.v");
    third.plan_out = c.d.join("plan_c.json");
    third.max_total_chain = 2;
    let e = plan_hold(&third).unwrap_err().to_string();
    assert!(e.contains("existing chain 2"), "{e}");
}

#[test]
fn closed_design_is_unchanged_and_inputs_are_validated() {
    let c = case(&[("tt", "r0/D", 0.07, 9.0)], 8.0, default_profile());
    let plan = plan_hold(&c.req).unwrap();
    assert_eq!(plan.status, "closed");
    assert_eq!(fs::read_to_string(&c.req.out).unwrap(), {
        let n = rustroad_formats::verilog::parse_verilog(NETLIST, &Default::default()).unwrap();
        let mut v = Vec::new();
        rustroad_formats::verilog::write_verilog(&n, &mut v).unwrap();
        String::from_utf8(v).unwrap()
    });
    // Missing hold corner.
    let mut c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, default_profile());
    c.req.hold_corners.push("xx".into());
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("MISSING_CORNER")
    );
    // Report margin below the target margin.
    let c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, default_profile());
    write(
        &c.d,
        "hold.json",
        &hold_report(0.0, &[("tt", "r0/D", -0.01, 9.0)], "spef_rc"),
    );
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("target margin")
    );
    // Ideal-wire report is refused for decisions.
    write(
        &c.d,
        "hold.json",
        &hold_report(0.05, &[("tt", "r0/D", -0.01, 9.0)], "ideal"),
    );
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("spef_rc")
    );
    // Truncated report.
    let mut r = hold_report(0.05, &[("tt", "r0/D", -0.01, 9.0)], "spef_rc");
    r["sections"]["hold_fix"]["corners"][0]["listed"] = json!(2);
    write(&c.d, "hold.json", &r);
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("truncated")
    );
    // Stale endpoint.
    write(
        &c.d,
        "hold.json",
        &hold_report(0.05, &[("tt", "r9/D", -0.01, 9.0)], "spef_rc"),
    );
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("STALE_REPORT")
    );
    // Port endpoints are not edited.
    let c = case(&[("tt", "y[0]", -0.01, 9.0)], 8.0, default_profile());
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("top-level port")
    );
    // The output may not overwrite the input.
    let mut c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, default_profile());
    c.req.out = c.req.netlist.clone();
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("OUTPUT_PATH")
    );
    let mut c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, default_profile());
    c.req.plan_out = c.req.out.clone();
    assert!(
        plan_hold(&c.req)
            .unwrap_err()
            .to_string()
            .contains("--plan-out must differ")
    );
}

#[test]
fn inverting_profile_entries_are_refused() {
    let p = profile(vec![buffer("INV1", 3.0, [0.05, 0.08, 0.03], "(!A)")]);
    let c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, p);
    let e = plan_hold(&c.req).unwrap_err().to_string();
    assert!(e.contains("ECO_NOT_BUFFER"), "{e}");
    // The same refusal through apply-eco with a hand-written plan.
    let plan = write(
        &c.d,
        "eco.json",
        &json!({"schema": "rustroad.eco-plan.v1", "module": "top", "operations": [
            {"op": "insert_buffer", "cell": "INV1", "instance": "x0", "net": "x0_n",
             "sink": {"instance": "r0", "pin": "D"}}]}),
    );
    let e = apply_plan(&c.req.netlist, &plan, &c.req.profile, &c.d.join("x.v"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("ECO_NOT_BUFFER"), "{e}");
    assert!(!c.d.join("x.v").exists());
    // A diode plan through apply-eco on a port bit.
    let good = write(&c.d, "good_profile.json", &default_profile());
    let plan = write(
        &c.d,
        "eco_d.json",
        &json!({"schema": "rustroad.eco-plan.v1", "module": "top", "operations": [
            {"op": "insert_diode", "cell": "DIODE", "instance": "rr_eco_ant_0", "net": "a[1]"}]}),
    );
    let r = apply_plan(&c.req.netlist, &plan, &good, &c.d.join("d.v")).unwrap();
    assert_eq!(r.eco_report.diodes_inserted, 1);
    assert!(
        fs::read_to_string(c.d.join("d.v"))
            .unwrap()
            .contains("DIODE rr_eco_ant_0 (.DIODE(a[1]));")
    );
}

fn antenna_report(
    net: &str,
    metal: f64,
    gate: f64,
    diff: f64,
    limit_no_diff: Value,
    with_diff: Value,
    plus: Value,
) -> Value {
    json!({"schema": "rustroad.antenna-check.v1", "status": "violated", "checks": [{
        "net": net, "layer": "M3", "kind": "side_area", "gates": ["g0/A"], "diffusions": [],
        "gate_area_um2": gate, "diff_area_um2": diff, "metal_um2": metal, "denominator_um2": gate,
        "ratio": metal / gate, "limit": 400.0, "rule": "ANTENNADIFFSIDEAREARATIO",
        "rules": {"keyword_no_diff": "ANTENNASIDEAREARATIO", "no_diff": limit_no_diff,
                  "keyword_with_diff": "ANTENNADIFFSIDEAREARATIO", "with_diff": with_diff,
                  "gate_plus_diff": plus},
        "violated": true}]})
}

#[test]
fn antenna_eco_counts_diodes_from_the_lef_rule() {
    use rustroad_workspace::eco::{AntennaEcoRequest, plan_antenna};
    let d = dir();
    let netlist = d.join("in.v");
    fs::write(&netlist, NETLIST).unwrap();
    let prof = write(&d, "profile.json", &default_profile());
    let req = |report: &Path, name: &str| AntennaEcoRequest {
        netlist: netlist.clone(),
        report: report.to_path_buf(),
        profile: prof.clone(),
        cell: None,
        min_diodes: 1,
        max_diodes_per_net: 4,
        prefix: "rr_eco_ant_".into(),
        out: d.join(format!("{name}.v")),
        plan_out: d.join(format!("{name}.json")),
    };
    // GF180-like: diff rule 400 with gate-plus-diff 15. 1000 um^2 over 1 um^2 gate:
    // k diodes of 0.4 um^2 give 1000 / (1 + 6k) <= 400 at k = 1.
    let gf = write(
        &d,
        "gf.json",
        &antenna_report(
            "a[1]",
            1000.0,
            1.0,
            0.0,
            Value::Null,
            json!({"kind": "value", "ratio": 400.0}),
            json!(15.0),
        ),
    );
    let plan = plan_antenna(&req(&gf, "gf")).unwrap();
    assert_eq!(plan.nets[0].diodes, 1);
    assert!(
        fs::read_to_string(d.join("gf.v"))
            .unwrap()
            .contains("DIODE rr_eco_ant_0 (.DIODE(a[1]));")
    );
    // Without gate-plus-diff and a scalar limit, diodes cannot help: infeasible.
    let none = write(
        &d,
        "none.json",
        &antenna_report(
            "a[1]",
            1000.0,
            1.0,
            0.0,
            json!({"kind": "value", "ratio": 400.0}),
            Value::Null,
            Value::Null,
        ),
    );
    let e = plan_antenna(&req(&none, "none")).unwrap_err().to_string();
    assert!(e.contains("ANTENNA_ECO_INFEASIBLE"), "{e}");
    // A PWL limit in diffusion area (sky130-like) needs enough diodes to raise it:
    // ratio 3000; k = 1, 2, 3 give limits 2760, 2920, 3071 at 0.4 um^2 per diode.
    let pwl = json!({"kind": "pwl", "points": [[0.0, 400.0], [0.0225, 2609.0], [22.5, 11600.0]]});
    let sky = write(
        &d,
        "sky.json",
        &antenna_report("n0", 1500.0, 0.5, 0.0, Value::Null, pwl, Value::Null),
    );
    let plan = plan_antenna(&req(&sky, "sky")).unwrap();
    assert_eq!(plan.nets[0].diodes, 3);
    assert!(plan.nets[0].estimated_worst_ratio_over_limit <= 1.0);
    // Nets absent from the mapped netlist (clock-tree nets) are not repaired here.
    let clk = write(
        &d,
        "clk.json",
        &antenna_report(
            "rr_clock_0",
            1000.0,
            1.0,
            0.0,
            Value::Null,
            json!({"kind": "value", "ratio": 400.0}),
            json!(15.0),
        ),
    );
    assert!(
        plan_antenna(&req(&clk, "clk"))
            .unwrap_err()
            .to_string()
            .contains("not a net of the mapped netlist")
    );
}
