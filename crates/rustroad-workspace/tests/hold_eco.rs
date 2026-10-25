//! Self-authored hold-ECO planner fixtures: a 4-register netlist, synthetic
//! RustSTA-shaped reports with the revision records of a synthetic STA
//! workspace (`sta_ws/revisions/rev<NNNN>/meta.json`, the shape the timing
//! engine writes), a toy cell LEF (pin directions) and a three-cell profile.
//! Each test writes < 64 KiB; no PDK, timing engine or path enumeration is
//! involved. Real-PDK closure (and the provenance binding against records the
//! timing engine itself wrote) is verified separately by scripts/hold_closure.py
//! and scripts/rtl2gds.py with extracted parasitics.

use rustroad_workspace::eco::{HoldEcoRequest, apply_plan, plan_hold};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Pin directions of every toy master (geometry is irrelevant here).
const CELL_LEF: &str = "VERSION 5.8 ;
MACRO AND2 CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE SIGNAL ; END A
 PIN B DIRECTION INPUT ; USE SIGNAL ; END B
 PIN X DIRECTION OUTPUT ; USE SIGNAL ; END X
END AND2
MACRO DFF CLASS CORE ; SIZE 1 BY 2 ;
 PIN D DIRECTION INPUT ; USE SIGNAL ; END D
 PIN CLK DIRECTION INPUT ; USE CLOCK ; END CLK
 PIN Q DIRECTION OUTPUT ; USE SIGNAL ; END Q
END DFF
MACRO CKBUF CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE CLOCK ; END A
 PIN X DIRECTION OUTPUT ; USE CLOCK ; END X
END CKBUF
MACRO BUF1 CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE SIGNAL ; END A
 PIN X DIRECTION OUTPUT ; USE SIGNAL ; END X
END BUF1
MACRO DLY1 CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE SIGNAL ; END A
 PIN X DIRECTION OUTPUT ; USE SIGNAL ; END X
END DLY1
MACRO DLY3 CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE SIGNAL ; END A
 PIN X DIRECTION OUTPUT ; USE SIGNAL ; END X
END DLY3
MACRO INV1 CLASS CORE ; SIZE 1 BY 2 ;
 PIN A DIRECTION INPUT ; USE SIGNAL ; END A
 PIN X DIRECTION OUTPUT ; USE SIGNAL ; END X
END INV1
MACRO DIODE CLASS CORE ANTENNACELL ; SIZE 1 BY 2 ;
 PIN DIODE DIRECTION INPUT ; USE SIGNAL ; END DIODE
END DIODE
END LIBRARY
";

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
/// Master, data net and net driver of each endpoint of NETLIST, as RustSTA
/// reports them in hold_fix rows.
fn binding_of(endpoint: &str) -> (&'static str, &'static str, &'static str) {
    match endpoint {
        "r0/D" => ("DFF", "n0", "g0/X"),
        "r1/D" => ("DFF", "n1", "g1/X"),
        "r2/D" => ("DFF", "a[0]", "a[0]"),
        _ => ("PORT", "", ""),
    }
}
fn hold_row(endpoint: &str, hold: f64, setup: f64, margin: f64) -> Value {
    let (cell, net, driver) = binding_of(endpoint);
    let mut row = json!({"endpoint": endpoint, "hold_slack_ns": hold, "setup_slack_ns": setup,
                         "required_delay_ns": margin - hold});
    if let Some((inst, _)) = endpoint.split_once('/') {
        row["inst"] = json!(inst);
        row["cell"] = json!(cell);
        row["net"] = json!(net);
        row["net_driver"] = json!(driver);
        row["path"] =
            json!({"stages": [{"pin": driver, "net": net}, {"pin": endpoint, "net": net}]});
    }
    row
}
/// Builds one hold_fix row from (endpoint, hold slack, setup slack).
type RowFn<'a> = &'a dyn Fn(&str, f64, f64) -> Value;
fn hold_report_rev(
    margin: f64,
    rows: &[(&str, &str, f64, f64)],
    wire: &str,
    rev: &str,
    custom: Option<RowFn>,
) -> Value {
    let mut corners = Vec::new();
    for c in ["tt", "ss", "ff"] {
        let endpoints: Vec<Value> = rows
            .iter()
            .filter(|r| r.0 == c)
            .map(|r| match custom {
                Some(f) => f(r.1, r.2, r.3),
                None => hold_row(r.1, r.2, r.3, margin),
            })
            .collect();
        corners.push(json!({"corner": c, "hold_endpoints": 3, "hold_violating": endpoints.len(),
                            "listed": endpoints.len(), "shown": endpoints.len(), "endpoints": endpoints}));
    }
    json!({"schema": "rusteda.v1", "ok": true, "workspace": "/sta/ws", "rev": rev,
           "scope": {"corner": "tt", "wire_model": wire, "clock_model": "propagated"},
           "sections": {"hold_fix": {"target_margin_ns": margin, "corners": corners}}})
}
fn hold_report(margin: f64, rows: &[(&str, &str, f64, f64)], wire: &str) -> Value {
    hold_report_rev(margin, rows, wire, "rev2", None)
}
fn setup_report_rev(corner: &str, rows: &[(&str, f64)], rev: &str) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| json!({"endpoint": r.0, "setup_slack_ns": r.1, "hold_slack_ns": 1.0}))
        .collect();
    json!({"schema": "rusteda.v1", "ok": true, "workspace": "/sta/ws", "rev": rev,
           "scope": {"corner": corner, "wire_model": "spef_rc", "clock_model": "propagated"},
           "sections": {"endpoints": {"total": rows.len(), "matched": rows.len(), "shown": rows.len(), "rows": rows}}})
}
fn setup_report(corner: &str, rows: &[(&str, f64)]) -> Value {
    setup_report_rev(corner, rows, "rev2")
}
/// The profile with a `cell_lef` source written next to it (pin directions).
fn with_cell_lef(d: &Path, mut profile: Value) -> Value {
    let lef = d.join("cells.lef");
    fs::write(&lef, CELL_LEF).unwrap();
    profile["sources"] = json!([{"role": "cell_lef", "path": lef,
        "sha256": format!("{:x}", Sha256::digest(CELL_LEF.as_bytes())), "bytes": CELL_LEF.len()}]);
    profile
}

struct Case {
    d: PathBuf,
    req: HoldEcoRequest,
}
/// Reports name the synthetic STA workspace of their test directory.
const STA_WS: &str = "/sta/ws";
fn write(d: &Path, name: &str, v: &Value) -> PathBuf {
    let p = d.join(name);
    let mut v = v.clone();
    if v["workspace"] == STA_WS {
        v["workspace"] = json!(d.join("sta_ws"));
    }
    fs::write(&p, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    p
}
fn xxh3(path: &Path) -> String {
    format!(
        "xxh3:{:016x}",
        xxhash_rust::xxh3::xxh3_64(&fs::read(path).unwrap())
    )
}
/// The revision record the timing engine writes for `rev`: `sources` lists
/// each netlist source (path, xxh3-64) of the netlist table.
fn record_sources(d: &Path, rev: &str, meta_rev: &str, netlists: &[&Path]) {
    let ws = d.join("sta_ws");
    let n: u32 = rev.trim_start_matches("rev").parse().unwrap();
    let dir = ws.join("revisions").join(format!("rev{n:04}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(ws.join("workspace.lock"), b"").unwrap();
    let sources: Vec<Value> = netlists
        .iter()
        .map(|p| json!({"table": "netlist", "path": p, "hash": xxh3(p)}))
        .chain([json!({"table": "constraints", "path": d.join("x.sdc"), "hash": "xxh3:0000000000000001"})])
        .collect();
    let meta = json!({"schema": "rusteda.revision.v1", "rev": meta_rev, "parent": "rev1",
        "tables": {"netlist": {"path": "netlist.rbin", "hash": "xxh3:00000000000000aa", "upstream": {}}},
        "sources": sources});
    fs::write(
        dir.join("meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap(),
    )
    .unwrap();
}
/// The timing engine timed `netlist` in revision `rev` of the test's STA workspace.
fn record(d: &Path, rev: &str, netlist: &Path) {
    record_sources(d, rev, rev, &[netlist]);
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
    let prof = write(&d, "profile.json", &with_cell_lef(&d, profile));
    record(&d, "rev2", &netlist);
    let req = HoldEcoRequest {
        timed_netlist: netlist.clone(),
        timed_netlist_sha256: None,
        cell_lef: None,
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
        pins_reports: Vec::new(),
        allow_partial: false,
        max_group_sinks: 8,
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
    // Second iteration on the ECO netlist, re-timed: r0/D still short at ss.
    // The new reports name the ECO net and driver at r0/D.
    let rebound = |rev: &str, net: &'static str, driver: &'static str| {
        let row = move |e: &str, h: f64, st: f64| {
            json!({"endpoint": e, "hold_slack_ns": h, "setup_slack_ns": st, "inst": "r0",
                   "cell": "DFF", "net": net, "net_driver": driver, "required_delay_ns": 0.05 - h})
        };
        hold_report_rev(0.05, &rows, "spef_rc", rev, Some(&row))
    };
    let mut second = c.req.clone();
    second.netlist = c.req.out.clone();
    second.timed_netlist = c.req.out.clone();
    second.hold_reports = vec![write(
        &c.d,
        "hold_b.json",
        &rebound("rev4", "rr_eco_hold_0_n", "rr_eco_hold_0/X"),
    )];
    second.setup_reports = vec![
        write(
            &c.d,
            "ss_b.json",
            &setup_report_rev("ss", &[("r0/D", 8.0)], "rev4"),
        ),
        write(
            &c.d,
            "tt_b.json",
            &setup_report_rev("tt", &[("r0/D", 9.0)], "rev4"),
        ),
    ];
    second.out = c.d.join("out_b.v");
    second.plan_out = c.d.join("plan_b.json");
    record(&c.d, "rev4", &second.timed_netlist);
    let plan = plan_hold(&second).unwrap();
    assert_eq!(plan.sinks[0].existing_chain, ["rr_eco_hold_0"]);
    let out = fs::read_to_string(&second.out).unwrap();
    assert!(
        out.contains("BUF1 rr_eco_hold_1 (.A(rr_eco_hold_0_n), .X(rr_eco_hold_1_n));"),
        "{out}"
    );
    let mut third = second.clone();
    third.netlist = second.out.clone();
    third.timed_netlist = second.out.clone();
    third.hold_reports = vec![write(
        &c.d,
        "hold_c.json",
        &rebound("rev6", "rr_eco_hold_1_n", "rr_eco_hold_1/X"),
    )];
    third.setup_reports = vec![
        write(
            &c.d,
            "ss_c.json",
            &setup_report_rev("ss", &[("r0/D", 8.0)], "rev6"),
        ),
        write(
            &c.d,
            "tt_c.json",
            &setup_report_rev("tt", &[("r0/D", 9.0)], "rev6"),
        ),
    ];
    third.out = c.d.join("out_c.v");
    third.plan_out = c.d.join("plan_c.json");
    third.max_total_chain = 2;
    record(&c.d, "rev6", &third.timed_netlist);
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
    let e = apply_plan(
        &c.req.netlist,
        &plan,
        &c.req.profile,
        None,
        None,
        &c.d.join("x.v"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("ECO_NOT_BUFFER"), "{e}");
    assert!(!c.d.join("x.v").exists());
    // A diode plan through apply-eco on a port bit.
    let good = write(
        &c.d,
        "good_profile.json",
        &with_cell_lef(&c.d, default_profile()),
    );
    let plan = write(
        &c.d,
        "eco_d.json",
        &json!({"schema": "rustroad.eco-plan.v1", "module": "top", "operations": [
            {"op": "insert_diode", "cell": "DIODE", "instance": "rr_eco_ant_0", "net": "a[1]"}]}),
    );
    let r = apply_plan(&c.req.netlist, &plan, &good, None, None, &c.d.join("d.v")).unwrap();
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
    json!({"schema": "rustroad.antenna-check.v1", "status": "violated",
        "safety_margin": {"fraction": 0.10, "rule": "test", "derivation": "test"},
        "checks": [{
        "net": net, "layer": "M3", "kind": "side_area", "gates": ["g0/A"], "diffusions": [],
        "gate_area_um2": gate, "diff_area_um2": diff, "metal_um2": metal, "denominator_um2": gate,
        "ratio": metal / gate, "limit": 400.0, "effective_limit": 360.0,
        "rule": "ANTENNADIFFSIDEAREARATIO",
        "rules": {"keyword_no_diff": "ANTENNASIDEAREARATIO", "no_diff": limit_no_diff,
                  "keyword_with_diff": "ANTENNADIFFSIDEAREARATIO", "with_diff": with_diff,
                  "gate_plus_diff": plus},
        "violated": true, "exceeds_lef_limit": true}]})
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
    // (Raw LEF limit: safety margin 0 for this arithmetic.)
    let pwl = json!({"kind": "pwl", "points": [[0.0, 400.0], [0.0225, 2609.0], [22.5, 11600.0]]});
    let mut sky = antenna_report("n0", 1500.0, 0.5, 0.0, Value::Null, pwl, Value::Null);
    sky["safety_margin"]["fraction"] = json!(0.0);
    let sky = write(&d, "sky.json", &sky);
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

#[test]
fn apply_eco_refuses_an_output_pin_sink_and_a_foreign_cell_lef() {
    // Phase-B1 verifier NB1: a sink on r0/Q used to be accepted and left two
    // drivers on the new net.
    let c = case(&[("tt", "r0/D", -0.01, 9.0)], 8.0, default_profile());
    let plan = write(
        &c.d,
        "q_sink.json",
        &json!({"schema": "rustroad.eco-plan.v1", "module": "top", "operations": [
            {"op": "insert_buffer", "cell": "BUF1", "instance": "vx0", "net": "vx0_n",
             "sink": {"instance": "r0", "pin": "Q"}}]}),
    );
    let out = c.d.join("q.v");
    let e = apply_plan(&c.req.netlist, &plan, &c.req.profile, None, None, &out)
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("ECO_APPLY:") && e.contains("OUTPUT pin of DFF"),
        "{e}"
    );
    assert!(!out.exists());
    // The same plan on the input pin works.
    let ok = write(
        &c.d,
        "d_sink.json",
        &json!({"schema": "rustroad.eco-plan.v1", "module": "top", "operations": [
            {"op": "insert_buffer", "cell": "BUF1", "instance": "vx0", "net": "vx0_n",
             "sink": {"instance": "r0", "pin": "D"}}]}),
    );
    apply_plan(&c.req.netlist, &ok, &c.req.profile, None, None, &out).unwrap();
    // A cell LEF other than the one the profile was built from is refused.
    let other = c.d.join("other.lef");
    fs::write(&other, CELL_LEF.replace("SIZE 1 BY 2", "SIZE 2 BY 2")).unwrap();
    let e = apply_plan(
        &c.req.netlist,
        &ok,
        &c.req.profile,
        Some(&other),
        None,
        &c.d.join("o.v"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.starts_with("ECO_PIN_DIRECTIONS:"), "{e}");
    // A profile without a cell LEF cannot prove sink directions.
    let bare = write(&c.d, "bare.json", &default_profile());
    let e = apply_plan(&c.req.netlist, &ok, &bare, None, None, &c.d.join("b.v"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no cell_lef source"), "{e}");
}

#[test]
fn stale_reports_are_refused_with_their_own_code() {
    let rows = [("ss", "r0/D", -0.10, 8.0)];
    let c = case(&rows, 8.0, default_profile());
    plan_hold(&c.req).unwrap();
    let eco = c.req.out.clone();
    let expect_stale = |req: &HoldEcoRequest, what: &str| {
        let e = plan_hold(req).unwrap_err().to_string();
        assert!(e.starts_with("STALE_REPORT:"), "{what}: {e}");
        assert!(!e.contains("INVALID_WORKSPACE"), "{what}: {e}");
        assert!(!req.out.exists(), "{what}: output written");
    };
    let fresh = |name: &str| {
        let mut r = c.req.clone();
        r.out = c.d.join(format!("{name}.v"));
        r.plan_out = c.d.join(format!("{name}.json"));
        r
    };
    // Iteration-0 reports against the iteration-1 netlist (verifier NB4):
    // the STA record of rev2 names in.v, not the ECO netlist.
    let mut r = fresh("stale_i1");
    r.netlist = eco.clone();
    r.timed_netlist = eco.clone();
    expect_stale(&r, "old reports, new netlist");
    let e = plan_hold(&r).unwrap_err().to_string();
    assert!(e.contains("were not produced from this netlist"), "{e}");
    // The same pairing with a record forged to name the ECO netlist is still
    // refused by the content binding (r0/D sits on another net there).
    record(&c.d, "rev3", &eco);
    let mut r = fresh("stale_i1_forged");
    r.netlist = eco.clone();
    r.timed_netlist = eco.clone();
    r.hold_reports = vec![write(
        &c.d,
        "hold_rev3.json",
        &hold_report_rev(0.05, &rows, "spef_rc", "rev3", None),
    )];
    r.setup_reports = vec![
        write(
            &c.d,
            "ss_rev3.json",
            &setup_report_rev("ss", &[("r0/D", 8.0)], "rev3"),
        ),
        write(
            &c.d,
            "tt_rev3.json",
            &setup_report_rev("tt", &[("r0/D", 9.0)], "rev3"),
        ),
    ];
    expect_stale(&r, "forged record, old rows");
    let e = plan_hold(&r).unwrap_err().to_string();
    assert!(e.contains("report net n0"), "{e}");
    // New netlist paired with the old timed netlist.
    let mut r = fresh("stale_pair");
    r.netlist = eco.clone();
    expect_stale(&r, "timed netlist older than the edited netlist");
    // Old netlist paired with a timed netlist that already has the ECO cell.
    let mut r = fresh("stale_later");
    r.timed_netlist = eco.clone();
    expect_stale(&r, "timed netlist newer than the edited netlist");
    // A timed-netlist hash other than the one recorded at timing time.
    let mut r = fresh("stale_hash");
    r.timed_netlist_sha256 = Some("00".repeat(32));
    expect_stale(&r, "hash");
    // Reports of two different STA revisions.
    let mut r = fresh("stale_rev");
    r.setup_reports[0] = write(
        &c.d,
        "setup_other_rev.json",
        &setup_report_rev(
            "ss",
            &[("r0/D", 8.0), ("r1/D", 8.0), ("r2/D", 8.0), ("y[0]", 5.0)],
            "rev9",
        ),
    );
    expect_stale(&r, "mixed revisions");
    // A row naming another master, or a path stage that does not exist.
    let wrong_cell = |e: &str, h: f64, st: f64| {
        let mut v = hold_row(e, h, st, 0.05);
        v["cell"] = json!("AND2");
        v
    };
    let mut r = fresh("stale_cell");
    r.hold_reports = vec![write(
        &c.d,
        "hold_wrong_cell.json",
        &hold_report_rev(0.05, &rows, "spef_rc", "rev2", Some(&wrong_cell)),
    )];
    expect_stale(&r, "master");
    let wrong_stage = |e: &str, h: f64, st: f64| {
        let mut v = hold_row(e, h, st, 0.05);
        v["path"]["stages"][0]["pin"] = json!("g1/X");
        v
    };
    let mut r = fresh("stale_stage");
    r.hold_reports = vec![write(
        &c.d,
        "hold_wrong_stage.json",
        &hold_report_rev(0.05, &rows, "spef_rc", "rev2", Some(&wrong_stage)),
    )];
    expect_stale(&r, "path stage");
    // An endpoints row of a pin that the timed netlist lacks.
    let mut r = fresh("stale_endpoint");
    r.setup_reports[1] = write(
        &c.d,
        "tt_extra.json",
        &setup_report("tt", &[("r0/D", 9.0), ("r7/D", 9.0)]),
    );
    expect_stale(&r, "endpoints row");
    // The reports themselves stay accepted with the matching pair.
    let r = fresh("good_again");
    assert_eq!(plan_hold(&r).unwrap().status, "planned");
}

#[test]
fn later_reports_on_an_earlier_netlist_are_refused_by_the_sta_record() {
    // Phase-B1/round-1 verifier F1: reports of a later STA run (timed on the
    // ECO netlist) whose rows also exist in the earlier netlist were accepted
    // with the earlier netlist and its own (correct) sha256, and produced a
    // wrong plan. The STA revision record names the netlist it timed.
    let rows = [("ss", "r0/D", -0.10, 8.0), ("ss", "r1/D", -0.10, 8.0)];
    let c = case(&rows, 8.0, default_profile());
    assert_eq!(plan_hold(&c.req).unwrap().sinks.len(), 2);
    let eco = c.req.out.clone();
    // rev4: the STA run on the ECO netlist still lists r2/D (a direct path,
    // identical in both netlists) and nothing else.
    record(&c.d, "rev4", &eco);
    let later = [("ss", "r2/D", -0.02, 8.0)];
    let hold4 = write(
        &c.d,
        "hold_rev4.json",
        &hold_report_rev(0.05, &later, "spef_rc", "rev4", None),
    );
    let ss4 = write(
        &c.d,
        "ss_rev4.json",
        &setup_report_rev("ss", &[("r0/D", 8.0), ("r1/D", 8.0), ("r2/D", 8.0)], "rev4"),
    );
    let tt4 = write(
        &c.d,
        "tt_rev4.json",
        &setup_report_rev("tt", &[("r0/D", 9.0), ("r1/D", 9.0), ("r2/D", 9.0)], "rev4"),
    );
    let with = |netlist: &Path, timed: &Path, name: &str| {
        let mut r = c.req.clone();
        r.netlist = netlist.to_path_buf();
        r.timed_netlist = timed.to_path_buf();
        r.timed_netlist_sha256 = Some(format!("{:x}", Sha256::digest(fs::read(timed).unwrap())));
        r.hold_reports = vec![hold4.clone()];
        r.setup_reports = vec![ss4.clone(), tt4.clone()];
        r.out = c.d.join(format!("{name}.v"));
        r.plan_out = c.d.join(format!("{name}.json"));
        r
    };
    // The counterexample: earlier netlist + its own hash + later reports.
    let r = with(&c.req.netlist, &c.req.netlist, "foreign");
    let e = plan_hold(&r).unwrap_err().to_string();
    assert!(
        e.starts_with("STALE_REPORT:") && e.contains("were not produced from this netlist"),
        "{e}"
    );
    assert!(
        e.contains(&xxh3(&eco)) && e.contains(&xxh3(&c.req.netlist)),
        "{e}"
    );
    assert!(!r.out.exists() && !r.plan_out.exists());
    // The matching pair is accepted and records what the STA record says.
    let r = with(&eco, &eco, "matching");
    let plan = plan_hold(&r).unwrap();
    assert_eq!(plan.status, "planned");
    let sta = plan.inputs.binding.sta_record.as_ref().unwrap();
    assert_eq!(sta.netlist_source, eco.display().to_string());
    assert_eq!(sta.timed_netlist_xxh3, xxh3(&eco));
    assert!(sta.source_path_matches);
    assert!(sta.revision_record.ends_with("revisions/rev0004/meta.json"));
    let saved: Value = serde_json::from_slice(&fs::read(&r.plan_out).unwrap()).unwrap();
    assert_eq!(
        saved["inputs"]["binding"]["sta_record"]["netlist_source_xxh3"],
        json!(xxh3(&eco))
    );
    // The hash is the binding: the same bytes under another name are accepted.
    let copy = c.d.join("eco_copy.v");
    fs::copy(&eco, &copy).unwrap();
    let plan = plan_hold(&with(&eco, &copy, "copy")).unwrap();
    assert!(!plan.inputs.binding.sta_record.unwrap().source_path_matches);
}

#[test]
fn unreadable_or_ambiguous_sta_records_are_refused() {
    let rows = [("ss", "r0/D", -0.10, 8.0)];
    let netlist_of = |c: &Case| c.req.netlist.clone();
    let expect = |c: &Case, code: &str, what: &str| {
        let e = plan_hold(&c.req).unwrap_err().to_string();
        assert!(e.starts_with(&format!("{code}:")), "{what}: {e}");
        assert!(!e.contains("INVALID_WORKSPACE"), "{what}: {e}");
        assert!(!c.req.out.exists(), "{what}");
    };
    // No revision record at all.
    let c = case(&rows, 8.0, default_profile());
    fs::remove_dir_all(c.d.join("sta_ws/revisions")).unwrap();
    expect(&c, "REPORT_PROVENANCE", "missing record");
    // No workspace lock (not a timing-engine workspace).
    let c = case(&rows, 8.0, default_profile());
    fs::remove_file(c.d.join("sta_ws/workspace.lock")).unwrap();
    expect(&c, "REPORT_PROVENANCE", "missing lock");
    // A record of another revision in the directory of rev2.
    let c = case(&rows, 8.0, default_profile());
    record_sources(&c.d, "rev2", "rev5", &[&netlist_of(&c)]);
    expect(&c, "REPORT_PROVENANCE", "record of another revision");
    // No netlist source, or two of them.
    let c = case(&rows, 8.0, default_profile());
    record_sources(&c.d, "rev2", "rev2", &[]);
    expect(&c, "REPORT_PROVENANCE", "no netlist source");
    let c = case(&rows, 8.0, default_profile());
    let n = netlist_of(&c);
    record_sources(&c.d, "rev2", "rev2", &[&n, &n]);
    expect(&c, "REPORT_PROVENANCE", "two netlist sources");
    // Malformed record and a report revision that is not rev<N>.
    let c = case(&rows, 8.0, default_profile());
    fs::write(c.d.join("sta_ws/revisions/rev0002/meta.json"), b"{").unwrap();
    expect(&c, "REPORT_PROVENANCE", "malformed record");
    let mut c = case(&rows, 8.0, default_profile());
    c.req.hold_reports = vec![write(
        &c.d,
        "hold_badrev.json",
        &hold_report_rev(0.05, &rows, "spef_rc", "rev02", None),
    )];
    expect(&c, "REPORT_PROVENANCE", "non-canonical revision");
    // A writer holding the workspace lock: bounded wait, then its own code.
    let c = case(&rows, 8.0, default_profile());
    let lock = fs::File::open(c.d.join("sta_ws/workspace.lock")).unwrap();
    lock.lock().unwrap();
    expect(&c, "STA_WORKSPACE_LOCKED", "writer lock");
    lock.unlock().unwrap();
    assert_eq!(plan_hold(&c.req).unwrap().status, "planned");
}

#[test]
fn cts_buffered_timed_netlist_refines_the_mapped_netlist() {
    let rows = [("ss", "r0/D", -0.10, 8.0)];
    let mut c = case(&rows, 8.0, default_profile());
    let cts = NETLIST
        .replace(
            "  wire n0;\n",
            "  wire n0;\n  wire rr_clock_0;\n  CKBUF rr_clkbuf_0 (.A(clk), .X(rr_clock_0));\n",
        )
        .replace(".CLK(clk)", ".CLK(rr_clock_0)");
    let timed = c.d.join("timed.v");
    fs::write(&timed, &cts).unwrap();
    c.req.timed_netlist = timed.clone();
    c.req.timed_netlist_sha256 = Some(format!("{:x}", Sha256::digest(cts.as_bytes())));
    record(&c.d, "rev2", &timed);
    let plan = plan_hold(&c.req).unwrap();
    let b = &plan.inputs.binding;
    assert_eq!(b.inserted_instances, ["rr_clkbuf_0"]);
    assert_eq!(b.redirected_pins, 3);
    assert_eq!((b.hold_rows_bound, b.path_stages_bound), (1, 2));
    assert_eq!(b.sta_revision, "rev2");
    let sta = b.sta_record.as_ref().unwrap();
    assert_eq!(sta.netlist_source_xxh3, xxh3(&timed));
    assert!(sta.source_path_matches);
    // An inserted instance that is not a buffer (or tie) is not a refinement.
    let foreign = cts.replace(
        "  wire n0;\n",
        "  wire n0;\n  wire zz;\n  AND2 gx (.A(en), .B(clk), .X(zz));\n",
    );
    fs::write(&timed, &foreign).unwrap();
    c.req.timed_netlist_sha256 = None;
    c.req.out = c.d.join("foreign.v");
    c.req.plan_out = c.d.join("foreign.json");
    let e = plan_hold(&c.req).unwrap_err().to_string();
    assert!(
        e.starts_with("STALE_REPORT:") && e.contains("neither"),
        "{e}"
    );
}

#[test]
fn antenna_eco_carries_a_partial_input_status_forward() {
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
    let base = antenna_report(
        "a[1]",
        1000.0,
        1.0,
        0.0,
        Value::Null,
        json!({"kind": "value", "ratio": 400.0}),
        json!(15.0),
    );
    // Partial report (unmodeled rules) with nothing violated: never "clean".
    let mut partial = base.clone();
    partial["status"] = json!("partial");
    partial["checks"][0]["violated"] = json!(false);
    let p = write(&d, "partial.json", &partial);
    let plan = plan_antenna(&req(&p, "partial")).unwrap();
    assert_eq!(
        (plan.status, plan.input_status.as_str()),
        ("partial", "partial")
    );
    assert!(plan.diagnostics[0].starts_with("ANTENNA_INPUT_PARTIAL"));
    // Partial with a violation: planned, and the partial input stays visible.
    let mut pv = base.clone();
    pv["status"] = json!("partial");
    let p = write(&d, "partial_violated.json", &pv);
    let plan = plan_antenna(&req(&p, "partial_violated")).unwrap();
    assert_eq!(
        (plan.status, plan.input_status.as_str()),
        ("planned", "partial")
    );
    // A passed report that lists violated rows, or an unknown status, is refused.
    let mut bad = base.clone();
    bad["status"] = json!("passed");
    let p = write(&d, "bad.json", &bad);
    let e = plan_antenna(&req(&p, "bad")).unwrap_err().to_string();
    assert!(e.starts_with("ANTENNA_REPORT:"), "{e}");
    bad["status"] = json!("unverified");
    let p = write(&d, "bad2.json", &bad);
    assert!(plan_antenna(&req(&p, "bad2")).is_err());
    // Clean report: clean.
    let mut clean = base.clone();
    clean["status"] = json!("passed");
    clean["checks"][0]["violated"] = json!(false);
    let p = write(&d, "clean.json", &clean);
    assert_eq!(plan_antenna(&req(&p, "clean")).unwrap().status, "clean");
}

#[test]
fn antenna_eco_sizes_diodes_against_the_safety_margin() {
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
        max_diodes_per_net: 8,
        prefix: "rr_eco_ant_".into(),
        out: d.join(format!("{name}.v")),
        plan_out: d.join(format!("{name}.json")),
    };
    // 2600 um^2 over 1 um^2 gate, gate-plus-diff 15, diode diff 0.4:
    // k diodes give 2600 / (1 + 6k): k = 1 -> 371 (> 360 = 0.9 x 400), k = 2 -> 200.
    let mut r = antenna_report(
        "a[1]",
        2600.0,
        1.0,
        0.0,
        Value::Null,
        json!({"kind": "value", "ratio": 400.0}),
        json!(15.0),
    );
    let p = write(&d, "m10.json", &r);
    let plan = plan_antenna(&req(&p, "m10")).unwrap();
    assert_eq!((plan.nets[0].diodes, plan.safety_margin), (2, 0.10));
    r["safety_margin"]["fraction"] = json!(0.0);
    let p = write(&d, "m0.json", &r);
    assert_eq!(plan_antenna(&req(&p, "m0")).unwrap().nets[0].diodes, 1);
}

/// hold_fix row with the stage fields RustSTA writes (arc, inst, driver).
fn path_row(endpoint: &str, hold: f64, setup: f64) -> Value {
    let stage = |pin: &str, inst: Option<&str>, arc: &str, net: &str, driver: &str| json!({"pin": pin, "inst": inst, "arc": arc, "net": net, "net_driver": driver});
    let stages = match endpoint {
        // a[0] -> g0/A -> g0/X -> r0/D
        "r0/D" => vec![
            stage("a[0]", None, "input", "a[0]", "a[0]"),
            stage("g0/A", Some("g0"), "net", "a[0]", "a[0]"),
            stage("g0/X", Some("g0"), "cell", "n0", "g0/X"),
            stage("r0/D", Some("r0"), "net", "n0", "g0/X"),
        ],
        // a[0] -> r2/D
        "r2/D" => vec![
            stage("a[0]", None, "input", "a[0]", "a[0]"),
            stage("r2/D", Some("r2"), "net", "a[0]", "a[0]"),
        ],
        // en -> g1/B -> g1/X -> r1/D
        _ => vec![
            stage("en", None, "input", "en", "en"),
            stage("g1/B", Some("g1"), "net", "en", "en"),
            stage("g1/X", Some("g1"), "cell", "n1", "g1/X"),
            stage("r1/D", Some("r1"), "net", "n1", "g1/X"),
        ],
    };
    let (cell, net, driver) = binding_of(endpoint);
    json!({"endpoint": endpoint, "inst": endpoint.split('/').next().unwrap(), "cell": cell, "net": net,
           "net_driver": driver, "hold_slack_ns": hold, "setup_slack_ns": setup,
           "required_delay_ns": 0.05 - hold, "path": {"stages": stages}})
}
fn pins_report(corner: &str, rows: &[(&str, f64)]) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| json!({"pin": r.0, "is_port": !r.0.contains('/'), "slack_ns": r.1, "hold_slack_ns": 0.0}))
        .collect();
    json!({"schema": "rusteda.v1", "ok": true, "workspace": "/sta/ws", "rev": "rev2",
           "scope": {"corner": corner, "wire_model": "spef_rc", "clock_model": "propagated"},
           "sections": {"pins": {"total": rows.len(), "matched": rows.len(), "shown": rows.len(), "rows": rows}}})
}
/// Path-aware case: hold rows with stages, pins reports at ss and tt where
/// the a[0] port and its g0/A branch have room but r0/D (and g0/X) do not.
fn path_case(rows: &[(&str, &str, f64, f64)], a0_slack: f64) -> Case {
    let mut c = case(rows, 0.1, default_profile());
    let hold = write(
        &c.d,
        "hold_path.json",
        &hold_report_rev(0.05, rows, "spef_rc", "rev2", Some(&path_row)),
    );
    let pins = |corner: &str, scale: f64| {
        pins_report(
            corner,
            &[
                ("a[0]", a0_slack * scale),
                ("en", 0.12 * scale),
                ("g0/A", a0_slack * scale),
                ("g0/X", 0.1 * scale),
                ("g1/B", 4.0 * scale),
                ("g1/X", 0.1 * scale),
                ("r0/D", 0.1 * scale),
                ("r1/D", 0.1 * scale),
                ("r2/D", 4.0 * scale),
            ],
        )
    };
    c.req.hold_reports = vec![hold];
    c.req.pins_reports = vec![
        write(&c.d, "pins_ss.json", &pins("ss", 1.0)),
        write(&c.d, "pins_tt.json", &pins("tt", 2.0)),
    ];
    c
}

#[test]
fn path_aware_delay_goes_upstream_of_the_setup_critical_part() {
    // r0/D needs 0.30 ns at ss but has 0.1 ns of setup slack: the endpoint
    // planner cannot help it. Its hold path enters through input a[0]; a[0]
    // also feeds r2/D, which needs 0.2 ns: two BUF1 (0.34 ns at ss, area 8,
    // smaller than one DLY1) in front of both a[0] sinks fix both and leave
    // every setup path through a[0] 4 - 0.374 ns.
    let rows = [
        ("ss", "r0/D", -0.25, 0.1),
        ("ss", "r2/D", -0.15, 0.1),
        ("tt", "r0/D", -0.05, 9.0),
    ];
    let mut old = path_case(&rows, 4.0);
    old.req.pins_reports.clear();
    let e = plan_hold(&old.req).unwrap_err().to_string();
    assert!(
        e.contains("HOLD_ECO_INFEASIBLE") && e.contains("r0/D"),
        "{e}"
    );
    let c = path_case(&rows, 4.0);
    let plan = plan_hold(&c.req).unwrap();
    assert_eq!(plan.status, "planned", "{:?}", plan.infeasible);
    assert_eq!(plan.points.len(), 1);
    let p = &plan.points[0];
    assert_eq!((p.kind, p.name.as_str()), ("net", "a[0]"));
    assert_eq!(p.cells, ["BUF1", "BUF1"]);
    assert_eq!(p.instances, ["rr_eco_hold_0", "rr_eco_hold_1"]);
    assert_eq!(p.sinks, ["g0/A", "r2/D"]);
    assert_eq!(p.endpoints, ["r0/D", "r2/D"]);
    assert_eq!(plan.sinks.len(), 2);
    assert!(
        plan.sinks
            .iter()
            .all(|s| s.point.as_deref() == Some("net:a[0]"))
    );
    // The second endpoint is covered by the first one's point (no cells).
    assert_eq!(plan.sinks[1].endpoint, "r2/D");
    assert!(plan.sinks[1].cells.is_empty());
    let out = fs::read_to_string(&c.req.out).unwrap();
    assert!(
        out.contains("BUF1 rr_eco_hold_1 (.A(a[0]), .X(rr_eco_hold_1_n));"),
        "{out}"
    );
    assert!(
        out.contains("BUF1 rr_eco_hold_0 (.A(rr_eco_hold_1_n), .X(rr_eco_hold_0_n));"),
        "{out}"
    );
    assert!(
        out.contains("AND2 g0 (.A(rr_eco_hold_0_n), .B(en), .X(n0));"),
        "{out}"
    );
    assert!(
        out.contains("DFF r2 (.D(rr_eco_hold_0_n), .CLK(clk), .Q(y[0]));"),
        "{out}"
    );
    assert!(
        out.contains("DFF r0 (.D(n0), .CLK(clk), .Q(\\q[0] ));"),
        "{out}"
    );
    let v: Value = serde_json::from_slice(&fs::read(&c.req.plan_out).unwrap()).unwrap();
    assert_eq!(v["inputs"]["binding"]["pin_rows_bound"], 18);
    assert_eq!(v["parameters"]["path_aware"], true);
    // Same inputs, same bytes.
    let mut again = c.req.clone();
    again.out = c.d.join("again.v");
    again.plan_out = c.d.join("again.json");
    plan_hold(&again).unwrap();
    assert_eq!(fs::read(&c.req.out).unwrap(), fs::read(&again.out).unwrap());
}

#[test]
fn path_aware_infeasible_endpoints_carry_evidence_and_partial_plans_apply() {
    // a[0] has no room either: r0/D is truly infeasible (every point of its
    // path has less setup slack than the smallest cell's ss delay). r1/D
    // enters through en -> g1/B, whose branch has room.
    let rows = [("ss", "r0/D", -0.25, 0.1), ("ss", "r1/D", -0.1, 0.1)];
    let c = path_case(&rows, 0.15);
    let e = plan_hold(&c.req).unwrap_err().to_string();
    assert!(e.contains("HOLD_ECO_INFEASIBLE"), "{e}");
    assert!(
        e.contains("r0/D: needs") && e.contains("net:a[0] (setup budget ss 0.1500"),
        "{e}"
    );
    assert!(!c.req.out.exists());
    let v: Value = serde_json::from_slice(&fs::read(&c.req.plan_out).unwrap()).unwrap();
    assert_eq!(v["status"], "infeasible");
    let mut partial = path_case(&rows, 0.15);
    partial.req.allow_partial = true;
    let plan = plan_hold(&partial.req).unwrap();
    assert_eq!(plan.status, "partial");
    assert_eq!(plan.infeasible.len(), 1);
    assert_eq!(plan.points.len(), 1);
    // en drives g0/B and g1/B, but only en's g1 branch is on r1/D's path and
    // en itself has 0.12 ns of room: the sink pin g1/B carries the delay.
    assert_eq!(plan.points[0].name, "g1/B");
    assert_eq!(plan.points[0].kind, "sink_pin");
    let out = fs::read_to_string(&partial.req.out).unwrap();
    assert!(
        out.contains("AND2 g1 (.A(a[1]), .B(rr_eco_hold_0_n), .X(n1));"),
        "{out}"
    );
}
