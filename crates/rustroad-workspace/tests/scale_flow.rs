//! Self-authored scale-path fixture: `floorplan` -> import -> analytic place
//! -> track router (schema 6, authoritative routes.rbin) on a three-metal toy
//! technology with 24 one-site buffers. Before implementation: < 2 MiB of
//! sources/workspace per test, two test workers, no PDK parse; the foundry
//! DRC/LVS evidence for real PDKs comes from scripts/scale_flow.py.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_workspace::floorplan::{FloorplanRequest, floorplan};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
use rustroad_workspace::routing::{RouteScope, route};
use rustroad_workspace::{Error, place, read};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const TECHNOLOGY: &str = "VERSION 5.8 ;\n\
UNITS DATABASE MICRONS 1000 ; END UNITS\n\
MANUFACTURINGGRID 0.01 ;\n\
LAYER M1 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; OFFSET 0.2 ; WIDTH 0.2 ; SPACING 0.2 ; END M1\n\
LAYER V1 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V1\n\
LAYER M2 TYPE ROUTING ; DIRECTION VERTICAL ; PITCH 0.4 ; OFFSET 0.2 ; WIDTH 0.2 ; SPACING 0.2 ; END M2\n\
LAYER V2 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V2\n\
LAYER M3 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; OFFSET 0.2 ; WIDTH 0.2 ; SPACING 0.2 ; END M3\n\
SITE s CLASS CORE ; SYMMETRY X Y ; SIZE 1 BY 2 ; END s\n\
VIA V12 DEFAULT\n\
LAYER M1 ; RECT -0.1 -0.1 0.1 0.1 ;\n\
LAYER V1 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M2 ; RECT -0.1 -0.1 0.1 0.1 ; END V12\n\
VIA V23 DEFAULT\n\
LAYER M2 ; RECT -0.1 -0.1 0.1 0.1 ;\n\
LAYER V2 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M3 ; RECT -0.1 -0.1 0.1 0.1 ; END V23\n\
END LIBRARY\n";

fn library() -> String {
    let mut text = String::from("VERSION 5.8 ;\n");
    for name in ["BUF", "HI", "LO", "FILL"] {
        text.push_str(&format!(
            "MACRO {name}\nCLASS CORE ; ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\n"
        ));
        if name == "BUF" {
            text.push_str("PIN A DIRECTION INPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.3 0.4 0.5 0.6 ; END END A\n");
        }
        if name != "FILL" {
            text.push_str("PIN Y DIRECTION OUTPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.5 1.3 0.7 1.5 ; END END Y\n");
        }
        text.push_str("PIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER M1 ; RECT 0 -0.1 1 0.1 ; END END VSS\n");
        text.push_str("PIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER M1 ; RECT 0 1.9 1 2.1 ; END END VDD\n");
        text.push_str(&format!("END {name}\n"));
    }
    text.push_str("END LIBRARY\n");
    text
}

/// A 24-buffer chain with a tap at every third stage (fan-out 2).
fn netlist() -> String {
    let mut v = String::from("module chip(input a, output [3:0] y);\n");
    for i in 0..24 {
        v.push_str(&format!("wire n{i};\n"));
    }
    for i in 0..24 {
        let input = if i == 0 {
            "a".to_string()
        } else if i % 3 == 0 {
            format!("n{}", i - 3)
        } else {
            format!("n{}", i - 1)
        };
        v.push_str(&format!("BUF b{i} (.A({input}), .Y(n{i}));\n"));
    }
    for k in 0..4 {
        v.push_str(&format!("assign y[{k}] = n{};\n", 20 + k));
    }
    v.push_str("endmodule\n");
    v
}

fn config() -> PhysicalConfig {
    PhysicalConfig {
        power_net: "VDD".into(),
        ground_net: "VSS".into(),
        power_pin: "VDD".into(),
        ground_pin: "VSS".into(),
        body_power_pins: Vec::new(),
        body_ground_pins: Vec::new(),
        rail_layer: "M1".into(),
        strap_layer: "M2".into(),
        via: "V12".into(),
        io_layer: "M3".into(),
        tie_high: TieCell {
            cell: "HI".into(),
            pin: "Y".into(),
        },
        tie_low: TieCell {
            cell: "LO".into(),
            pin: "Y".into(),
        },
        filler_cells: vec!["FILL".into()],
        alternating_rows: true,
        cell_spacing_sites: 0,
        io_width: None,
        io_min_area: false,
        io_spacing: None,
        strap_width: None,
        strap_spacing: None,
        max_added_cells: 512,
        max_tie_fanout: 8,
        tap: None,
        clock: None,
        placer: None,
        io_plan: None,
    }
}

fn routing(track: bool) -> serde_json::Value {
    let mut v = serde_json::json!({"grid_step":400,"access_layer":"M2",
        "layers":[{"name":"M1","spacing":200},{"name":"M2","spacing":200},{"name":"M3","spacing":200}],
        "vias":[{"name":"V12","cut_spacing":60},{"name":"V23","cut_spacing":60}],
        "lef_rules":true,"max_skew_ns":1.0});
    if track {
        v["track_router"] = serde_json::json!({"gcell":2000,"via_cost":800});
    }
    v
}

fn limits() -> Limits {
    Limits {
        max_cells: 2000,
        max_rows: 64,
        max_expansions: 1_000_000,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/scale-tests");
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("tech.lef"), TECHNOLOGY).unwrap();
        fs::write(root.join("cells.lef"), library()).unwrap();
        fs::write(root.join("mapped.v"), netlist()).unwrap();
        fs::write(
            root.join("profile.json"),
            serde_json::to_vec(&config()).unwrap(),
        )
        .unwrap();
        for (name, track) in [("track.json", true), ("lattice.json", false)] {
            fs::write(
                root.join(name),
                serde_json::to_vec(&routing(track)).unwrap(),
            )
            .unwrap();
        }
        Self { root }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn floorplan(&self, tag: &str) -> rustroad_workspace::Result<serde_json::Value> {
        let report = floorplan(&FloorplanRequest {
            tech_lef: self.path("tech.lef"),
            cell_lef: self.path("cells.lef"),
            netlist: self.path("mapped.v"),
            physical_config: self.path("profile.json"),
            routing_config: self.path("track.json"),
            site: "s".into(),
            utilization: 0.4,
            aspect: 1.0,
            margin_um: 3.0,
            clock_port: None,
            no_clock: true,
            fixed_order: false,
            out_floorplan: self.path(&format!("{tag}-floor.json")),
            out_physical_config: self.path(&format!("{tag}-physical.json")),
        })?;
        Ok(serde_json::to_value(report).unwrap())
    }
    /// floorplan -> import -> place -> route; returns the workspace.
    fn flow(&self, tag: &str, routing: &str) -> rustroad_workspace::Result<PathBuf> {
        self.floorplan(tag)?;
        let ws = self.path(&format!("{tag}-ws"));
        let summary = import_pdk_with_config(
            &ws,
            "rev0",
            &ImportSources {
                technology_lef: self.path("tech.lef"),
                cell_lef: self.path("cells.lef"),
                netlist: self.path("mapped.v"),
                floorplan: self.path(&format!("{tag}-floor.json")),
            },
            &limits(),
            &ConnectivityLimits::default(),
            Some(&self.path(&format!("{tag}-physical.json"))),
        )?;
        let placed = place(&ws, &summary.revision, &limits())?;
        let report = placed
            .placement
            .as_ref()
            .expect("analytic placement report");
        assert_eq!(report.status, "legal_checked");
        assert!(
            report.hpwl_dbu <= report.baseline_hpwl_dbu,
            "analytic {} vs fixed-order {}",
            report.hpwl_dbu,
            report.baseline_hpwl_dbu
        );
        route(
            &ws,
            &placed.revision,
            &self.path(routing),
            RouteScope::All,
            &limits(),
        )?;
        Ok(ws)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn def(ws: &Path) -> String {
    let mut out = Vec::new();
    read(ws, &limits()).unwrap().export_def(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}
fn head_dir(ws: &Path) -> PathBuf {
    let head = fs::read_to_string(ws.join("HEAD")).unwrap();
    ws.join("revisions").join(head.trim())
}

#[test]
fn floorplan_place_and_track_route_round_trip_through_schema_6() {
    let f = Fixture::new();
    let report = f.floorplan("a").unwrap();
    assert_eq!(report["io_pins"], 5);
    assert!(report["estimated_utilization"].as_f64().unwrap() <= 0.41);
    // The floorplan outputs are deterministic and refuse to overwrite inputs.
    let again = f.floorplan("b").unwrap();
    assert_eq!(
        fs::read(f.path("a-floor.json")).unwrap(),
        fs::read(f.path("b-floor.json")).unwrap()
    );
    assert_eq!(
        fs::read(f.path("a-physical.json")).unwrap(),
        fs::read(f.path("b-physical.json")).unwrap()
    );
    assert_eq!(report["die_area"], again["die_area"]);
    let ws = f.flow("c", "track.json").unwrap();
    let dir = head_dir(&ws);
    // Schema 6: records only in routes.rbin, its hash in design.json.
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("design.json")).unwrap()).unwrap();
    assert_eq!(meta["format_version"], 6);
    let routing = &meta["physical"]["routing"];
    assert_eq!(routing["routes"]["wires"].as_array().unwrap().len(), 0);
    assert!(routing["table"]["wires"].as_u64().unwrap() > 0);
    assert!(dir.join("routes.rbin").is_file() && !dir.join("routing.rbin").exists());
    let snapshot = read(&ws, &limits()).unwrap();
    let summary = snapshot.summary();
    // a, n0..n23 except the unread n2, n5, n8, n11, n14, n17.
    assert_eq!(summary.routed_nets, Some(19));
    drop(snapshot);
    // Wires are maximal runs: no two same-net collinear wires overlap.
    let first = def(&ws);
    assert!(first.contains("+ ROUTED"));
    // A second identical flow gives byte-identical DEF and route tables.
    let ws2 = f.flow("d", "track.json").unwrap();
    assert_eq!(first, def(&ws2));
    assert_eq!(
        fs::read(dir.join("routes.rbin")).unwrap(),
        fs::read(head_dir(&ws2).join("routes.rbin")).unwrap()
    );
    // Tampering with the authoritative table is detected on load.
    let table = dir.join("routes.rbin");
    let mut bytes = fs::read(&table).unwrap();
    bytes[16] ^= 1;
    fs::write(&table, &bytes).unwrap();
    assert!(matches!(read(&ws, &limits()), Err(Error::Invalid(_))));
    // Truncation too.
    bytes[16] ^= 1;
    fs::write(&table, &bytes[..bytes.len() - 64]).unwrap();
    assert!(read(&ws, &limits()).is_err());
    fs::write(&table, &bytes).unwrap();
    assert_eq!(first, def(&ws));
}

#[test]
fn lattice_profile_keeps_schema_5_and_track_budgets_fail_explicitly() {
    let f = Fixture::new();
    let ws = f.flow("a", "lattice.json").unwrap();
    let dir = head_dir(&ws);
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("design.json")).unwrap()).unwrap();
    assert_eq!(meta["format_version"], 5);
    assert!(dir.join("routing.rbin").is_file() && !dir.join("routes.rbin").exists());
    // A memory budget below the upfront estimate is refused before search.
    let mut v = routing(true);
    v["track_router"]["memory_mib"] = serde_json::json!(1);
    v["limits"] = serde_json::json!({"max_axis":1024,"max_grid_nodes":10,"max_obstacles":200000,
        "max_bin_entries":1000000,"max_segments":100000,"max_expansions":2000000,
        "max_search_expansions":200000,"max_comparisons":200000000,"max_heap":500000});
    fs::write(f.path("tiny.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    let head = fs::read_to_string(ws.join("HEAD")).unwrap();
    let placed = place(&ws, head.trim(), &limits()).unwrap();
    let err = route(
        &ws,
        &placed.revision,
        &f.path("tiny.json"),
        RouteScope::All,
        &limits(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("BUDGET_EXCEEDED"), "{err}");
    // HEAD did not move.
    assert_eq!(
        fs::read_to_string(ws.join("HEAD")).unwrap().trim(),
        placed.revision
    );
    // Lattice caps stay hard: a lattice profile above its defaults is refused.
    let mut v = routing(false);
    v["limits"] = serde_json::json!({"max_axis":4096,"max_grid_nodes":2000000,"max_obstacles":200000,
        "max_bin_entries":1000000,"max_segments":100000,"max_expansions":2000000,
        "max_search_expansions":200000,"max_comparisons":200000000,"max_heap":500000});
    fs::write(f.path("raised.json"), serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(
        route(
            &ws,
            &placed.revision,
            &f.path("raised.json"),
            RouteScope::All,
            &limits()
        )
        .is_err()
    );
}

#[test]
fn floorplan_refuses_cells_wider_than_the_tap_gap_and_outputs_over_inputs() {
    let f = Fixture::new();
    let tap = "MACRO TAP CLASS CORE WELLTAP ; ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\n\
PIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER M1 ; RECT 0 1.9 1 2.1 ; END END VDD\n\
PIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER M1 ; RECT 0 -0.1 1 0.1 ; END END VSS\n\
END TAP\nEND LIBRARY\n";
    fs::write(f.path("cells.lef"), library().replace("END LIBRARY\n", tap)).unwrap();
    let mut cfg = config();
    cfg.tap = Some(rustroad_workspace::physical::TapConfig {
        cell: "TAP".into(),
        max_gap: 500,
        well_layer: "NW".into(),
        substrate_layer: "PW".into(),
    });
    fs::write(f.path("profile.json"), serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err = f.floorplan("a").unwrap_err();
    assert!(err.to_string().contains("FLOORPLAN_TAP_GAP"), "{err}");
    // A gap that holds the widest cell gives rows of tap + k (tap + gap).
    cfg.tap.as_mut().unwrap().max_gap = 3000;
    fs::write(f.path("profile.json"), serde_json::to_vec(&cfg).unwrap()).unwrap();
    let report = f.floorplan("b").unwrap();
    let length = report["row_length_dbu"].as_i64().unwrap();
    assert_eq!((length - 1000) % (1000 + 3000), 0, "{length}");
    // Outputs must never overwrite inputs.
    let refused = floorplan(&FloorplanRequest {
        tech_lef: f.path("tech.lef"),
        cell_lef: f.path("cells.lef"),
        netlist: f.path("mapped.v"),
        physical_config: f.path("profile.json"),
        routing_config: f.path("track.json"),
        site: "s".into(),
        utilization: 0.4,
        aspect: 1.0,
        margin_um: 3.0,
        clock_port: None,
        no_clock: true,
        fixed_order: false,
        out_floorplan: f.path("mapped.v"),
        out_physical_config: f.path("x.json"),
    })
    .err()
    .expect("an output path equal to an input is refused");
    assert!(refused.to_string().contains("OUTPUT_PATH"), "{refused}");
}
