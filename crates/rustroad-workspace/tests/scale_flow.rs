//! Self-authored scale-path fixture: `floorplan` -> import -> analytic place
//! -> track router (schema 6, authoritative routes.rbin) on a three-metal toy
//! technology with 24 one-site buffers. Before implementation: < 2 MiB of
//! sources/workspace per test, two test workers, no PDK parse; the foundry
//! DRC/LVS evidence for real PDKs comes from scripts/scale_flow.py.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_workspace::floorplan::{FloorplanRequest, floorplan};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell, TieRailInput};
use rustroad_workspace::routing::{RouteScope, route};
use rustroad_workspace::{Error, place, place_weighted, read};
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
        macro_supply: None,
        rail_via_stack: Vec::new(),
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
            rail_input: None,
        },
        tie_low: TieCell {
            cell: "LO".into(),
            pin: "Y".into(),
            rail_input: None,
        },
        constant_cells: Vec::new(),
        filler_cells: vec!["FILL".into()],
        alternating_rows: true,
        cell_spacing_sites: 0,
        placement_blockages: Vec::new(),
        io_width: None,
        io_min_area: false,
        io_spacing: None,
        strap_width: None,
        strap_spacing: None,
        max_added_cells: 512,
        max_tie_fanout: 8,
        tap: None,
        port_buffer: None,
        input_buffer: None,
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
fn batched_routing_persists_complete_coverage_and_failed_batches_keep_head() {
    let f = Fixture::new();
    let mut batch = routing(true);
    batch["batch_terminals"] = serde_json::json!(16);
    fs::write(f.path("batch.json"), serde_json::to_vec(&batch).unwrap()).unwrap();
    let first = f.flow("batch-one", "batch.json").unwrap();
    let second = f.flow("batch-two", "batch.json").unwrap();
    let snapshot = read(&first, &limits()).unwrap();
    assert_eq!(snapshot.summary().routed_nets, Some(19));
    drop(snapshot);
    assert_eq!(def(&first), def(&second));
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(head_dir(&first).join("design.json")).unwrap()).unwrap();
    assert_eq!(
        metadata["physical"]["routing"]["config"]["batch_terminals"],
        16
    );
    batch["batch_terminals"] = serde_json::json!(2);
    fs::write(
        f.path("batch-small.json"),
        serde_json::to_vec(&batch).unwrap(),
    )
    .unwrap();
    let error = f.flow("batch-small", "batch-small.json").unwrap_err();
    assert!(
        error.to_string().contains("batch terminal limit"),
        "{error}"
    );
    let unchanged = read(&f.path("batch-small-ws"), &limits()).unwrap();
    assert_eq!(unchanged.summary().routed_nets, None);
    assert_eq!(unchanged.summary().revision, "rev2");
}

#[test]
fn explicit_signal_layer_names_survive_export_and_reject_invalid_ranges() {
    let fixture = Fixture::new();
    let mut profile = routing(true);
    profile["track_router"]["min_routing_layer"] = "M2".into();
    profile["track_router"]["max_routing_layer"] = "M3".into();
    fs::write(
        fixture.path("restricted.json"),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
    let ws = fixture.flow("restricted", "restricted.json").unwrap();
    let snapshot = read(&ws, &limits()).unwrap();
    let config = snapshot
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .routing
        .as_ref()
        .unwrap()
        .config
        .track_router
        .as_ref()
        .unwrap();
    assert_eq!(config.min_routing_layer.as_deref(), Some("M2"));
    assert_eq!(config.max_routing_layer.as_deref(), Some("M3"));
    assert_eq!(snapshot.summary().routed_nets, Some(19));
    drop(snapshot);
    for (tag, lower, upper) in [
        ("absent-layer", "missing", "M3"),
        ("cut-layer", "M2", "V1"),
        ("reversed", "M3", "M2"),
    ] {
        profile["track_router"]["min_routing_layer"] = lower.into();
        profile["track_router"]["max_routing_layer"] = upper.into();
        fs::write(
            fixture.path("invalid-range.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        assert!(fixture.flow(tag, "invalid-range.json").is_err(), "{tag}");
        assert_eq!(
            read(&fixture.path(&format!("{tag}-ws")), &limits())
                .unwrap()
                .summary()
                .revision,
            "rev2",
            "{tag} must not publish partial routing"
        );
    }
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
fn eco_route_reuses_an_unchanged_prior_and_checks_its_revision() {
    let f = Fixture::new();
    let prior = f.flow("p", "track.json").unwrap();
    let prior_rev = fs::read_to_string(prior.join("HEAD"))
        .unwrap()
        .trim()
        .to_string();
    // The same design placed again in a fresh workspace, routed with the
    // first routing as its prior: every routed net is offered and the
    // result is the same validated routing.
    f.floorplan("q").unwrap();
    let ws = f.path("q-ws");
    let imported = import_pdk_with_config(
        &ws,
        "rev0",
        &ImportSources {
            technology_lef: f.path("tech.lef"),
            cell_lef: f.path("cells.lef"),
            netlist: f.path("mapped.v"),
            floorplan: f.path("q-floor.json"),
        },
        &limits(),
        &ConnectivityLimits::default(),
        Some(&f.path("q-physical.json")),
    )
    .unwrap();
    let placed = place(&ws, &imported.revision, &limits()).unwrap();
    let wrong = rustroad_workspace::routing::PriorSource {
        workspace: &prior,
        revision: "rev1",
    };
    assert!(matches!(
        rustroad_workspace::routing::route_eco(
            &ws,
            &placed.revision,
            &f.path("track.json"),
            RouteScope::All,
            Some(&wrong),
            &limits(),
        ),
        Err(Error::Conflict { .. })
    ));
    let source = rustroad_workspace::routing::PriorSource {
        workspace: &prior,
        revision: &prior_rev,
    };
    let routed = rustroad_workspace::routing::route_eco(
        &ws,
        &placed.revision,
        &f.path("track.json"),
        RouteScope::All,
        Some(&source),
        &limits(),
    )
    .unwrap();
    let eco = routed.eco_prior.expect("ECO report");
    assert_eq!(eco.prior_routed_nets, 19);
    assert_eq!(eco.offered_nets, 19);
    assert_eq!((eco.changed_nets, eco.new_nets), (0, 0));
    assert_eq!(def(&ws), def(&prior));
    // The prior workspace is read, never written.
    assert_eq!(
        fs::read_to_string(prior.join("HEAD")).unwrap().trim(),
        prior_rev
    );
}

#[test]
fn eco_route_accepts_one_batch_and_refuses_a_multi_batch_plan() {
    let f = Fixture::new();
    let prior = f.flow("p", "track.json").unwrap();
    let prior_rev = fs::read_to_string(prior.join("HEAD"))
        .unwrap()
        .trim()
        .to_string();
    f.floorplan("q").unwrap();
    let ws = f.path("q-ws");
    let imported = import_pdk_with_config(
        &ws,
        "rev0",
        &ImportSources {
            technology_lef: f.path("tech.lef"),
            cell_lef: f.path("cells.lef"),
            netlist: f.path("mapped.v"),
            floorplan: f.path("q-floor.json"),
        },
        &limits(),
        &ConnectivityLimits::default(),
        Some(&f.path("q-physical.json")),
    )
    .unwrap();
    let placed = place(&ws, &imported.revision, &limits()).unwrap();
    let source = rustroad_workspace::routing::PriorSource {
        workspace: &prior,
        revision: &prior_rev,
    };
    let track: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path("track.json")).unwrap()).unwrap();
    for (name, batch) in [("multi.json", 2), ("single.json", 100000)] {
        let mut v = track.clone();
        v["batch_terminals"] = serde_json::json!(batch);
        fs::write(f.path(name), serde_json::to_vec(&v).unwrap()).unwrap();
    }
    // Two terminals per batch splits the design: refused before any search, HEAD unchanged.
    assert!(matches!(
        rustroad_workspace::routing::route_eco(
            &ws,
            &placed.revision,
            &f.path("multi.json"),
            RouteScope::All,
            Some(&source),
            &limits(),
        ),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        fs::read_to_string(ws.join("HEAD")).unwrap().trim(),
        placed.revision
    );
    // One batch holding every terminal is the whole-input track router.
    let routed = rustroad_workspace::routing::route_eco(
        &ws,
        &placed.revision,
        &f.path("single.json"),
        RouteScope::All,
        Some(&source),
        &limits(),
    )
    .unwrap();
    let eco = routed.eco_prior.expect("ECO report");
    assert_eq!(
        (eco.offered_nets, eco.changed_nets, eco.new_nets),
        (19, 0, 0)
    );
    assert!(routed.route_memory.is_some());
    assert_eq!(def(&ws), def(&prior));
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
    let mut active = routing(true);
    active["limits"] =
        serde_json::to_value(rustroad_core::routing::RouteLimits::default()).unwrap();
    active["limits"]["max_active_nodes"] = serde_json::json!(1);
    fs::write(f.path("active.json"), serde_json::to_vec(&active).unwrap()).unwrap();
    let err = route(
        &ws,
        &placed.revision,
        &f.path("active.json"),
        RouteScope::All,
        &limits(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("active detailed routing topology"),
        "{err}"
    );
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
        pattern: None,
        body_view: Default::default(),
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

#[test]
fn floorplan_does_not_silently_omit_input_buffer_area() {
    let f = Fixture::new();
    let mut profile: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path("profile.json")).unwrap()).unwrap();
    profile["input_buffer"] = serde_json::json!({
        "cell":"BUF","input_pin":"A","output_pin":"Y","max_fanout":8,
        "max_buffers":100,"max_sinks":1000,"max_work":100000,"memory_mib":16
    });
    fs::write(
        f.path("profile.json"),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
    let error = f.floorplan("input-buffer").unwrap_err();
    assert!(
        error.to_string().contains("FLOORPLAN_INPUT_BUFFER_POLICY"),
        "{error}"
    );
    assert!(!f.path("input-buffer-floor.json").exists());
}

/// Rail-input ties: BUF with A on a rail is the constant source. `b24` keeps
/// its literal input as a rail tie (no new cell); `y[3] = 1'b0` gets a new
/// tie BUF. Both inputs are routed to their own rail on the supply net.
#[test]
fn rail_input_ties_are_routed_to_the_cells_own_rail() {
    use rustroad_core::power::RailPolarity;
    let f = Fixture::new();
    let text = netlist()
        .replace("wire n0;", "wire n0;\nwire n24;\nwire n25;")
        .replace("assign y[1] = n21;", "assign y[1] = n25;")
        .replace("assign y[2] = n22;", "assign y[2] = n24;")
        .replace("assign y[3] = n23;", "assign y[3] = 1'b0;")
        .replace(
            "endmodule",
            "BUF b24 (.A(1'b1), .Y(n24));\nLO k0 (.Y(n25));\nendmodule",
        );
    fs::write(f.path("mapped.v"), text).unwrap();
    let mut cfg = config();
    let rail = |supply| {
        Some(TieRailInput {
            pin: "A".into(),
            supply,
        })
    };
    cfg.tie_high = TieCell {
        cell: "BUF".into(),
        pin: "Y".into(),
        rail_input: rail(RailPolarity::Power),
    };
    cfg.tie_low = TieCell {
        cell: "BUF".into(),
        pin: "Y".into(),
        rail_input: rail(RailPolarity::Ground),
    };
    // The synthesized constant cell LO is rebuilt with the tie policy.
    cfg.constant_cells = vec![rustroad_workspace::physical::ConstantCell {
        cell: "LO".into(),
        high: Vec::new(),
        low: vec!["Y".into()],
    }];
    fs::write(f.path("profile.json"), serde_json::to_vec(&cfg).unwrap()).unwrap();
    // The lattice router does not take rail ties.
    assert!(f.flow("l", "lattice.json").is_err());
    let ws = f.flow("t", "track.json").unwrap();
    let snapshot = read(&ws, &limits()).unwrap();
    // b24 and the rebuilt k0 keep their places; only y[3] needs a new cell.
    assert_eq!(snapshot.summary().inserted_tie_cells, Some(1));
    let verilog = |power: bool| {
        let mut out = Vec::new();
        snapshot.export_verilog_with_power(power, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    };
    let logical = verilog(false);
    assert!(
        logical.contains("BUF b24 (.A(1'b1), .Y(n24));"),
        "{logical}"
    );
    assert!(
        logical.contains("BUF rr_tie_cell_0 (.Y(rr_tie_net_0), .A(1'b0));"),
        "{logical}"
    );
    assert!(!logical.contains("rr_tie_rail"), "{logical}");
    assert!(logical.contains("BUF k0 (.Y(n25), .A(1'b0));"), "{logical}");
    let powered = verilog(true);
    assert!(
        powered.contains(".A(VDD)") && powered.contains(".A(VSS)"),
        "{powered}"
    );
    assert!(!powered.contains("rr_tie_rail"), "{powered}");
    drop(snapshot);
    // DEF: each tie's wiring sits in a NETS entry of its supply net, which
    // keeps its SPECIALNETS entry.
    let text = def(&ws);
    let nets = &text[text.find("\nNETS ").unwrap()..text.find("END NETS").unwrap()];
    assert!(
        nets.contains("- VDD ( b24 A ) + USE POWER\n  + ROUTED"),
        "{nets}"
    );
    assert!(
        nets.contains("- VSS ( k0 A ) ( rr_tie_cell_0 A ) + USE GROUND\n  + ROUTED"),
        "{nets}"
    );
    assert!(text.contains("SPECIALNETS 2 ;"));
    // Deterministic, and the stored routes re-validate on load.
    let again = f.flow("u", "track.json").unwrap();
    assert_eq!(text, def(&again));
}

#[test]
fn net_weights_are_stored_applied_and_validated() {
    let f = Fixture::new();
    f.floorplan("w").unwrap();
    let import = |name: &str| {
        import_pdk_with_config(
            &f.path(name),
            "rev0",
            &ImportSources {
                technology_lef: f.path("tech.lef"),
                cell_lef: f.path("cells.lef"),
                netlist: f.path("mapped.v"),
                floorplan: f.path("w-floor.json"),
            },
            &limits(),
            &ConnectivityLimits::default(),
            Some(&f.path("w-physical.json")),
        )
        .unwrap()
    };
    let a = import("plain");
    let plain = place(&f.path("plain"), &a.revision, &limits()).unwrap();
    // Every net weighted 1.0 is the unweighted placement, bit for bit.
    let names: Vec<String> = (0..24)
        .map(|i| format!("n{i}"))
        .chain(["a".into()])
        .collect();
    let unit = serde_json::json!({"schema": "rustroad.net-weights.v1", "source": {},
        "weights": names.iter().map(|n| (n.clone(), serde_json::json!(1000))).collect::<serde_json::Map<_, _>>()});
    fs::write(f.path("unit.json"), unit.to_string()).unwrap();
    let b = import("unit");
    let one = place_weighted(
        &f.path("unit"),
        &b.revision,
        &limits(),
        Some(&f.path("unit.json")),
    )
    .unwrap();
    assert_eq!(def(&f.path("plain")), def(&f.path("unit")));
    let used = one
        .placement
        .as_ref()
        .unwrap()
        .net_weights
        .as_ref()
        .unwrap();
    assert_eq!((used.applied, used.unmatched), (25, 0));
    assert!(plain.placement.as_ref().unwrap().net_weights.is_none());
    // A heavy net gets shorter; unknown names are counted, not guessed.
    let heavy = serde_json::json!({"schema": "rustroad.net-weights.v1", "source": {"rule": "test"},
        "weights": {"n9": 16000, "nope": 2000}});
    fs::write(f.path("heavy.json"), heavy.to_string()).unwrap();
    let c = import("heavy");
    let h = place_weighted(
        &f.path("heavy"),
        &c.revision,
        &limits(),
        Some(&f.path("heavy.json")),
    )
    .unwrap();
    let used = h.placement.as_ref().unwrap().net_weights.as_ref().unwrap();
    assert_eq!(
        (used.applied, used.unmatched, used.max_weight),
        (1, 1, 16000)
    );
    let span = |ws: &Path, net: &str| -> i64 {
        // Centre-to-centre x/y span of the cells on `net` from the DEF.
        let text = def(ws);
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("- {net} ")))
            .unwrap()
            .to_string();
        let cells: Vec<&str> = line
            .split("( ")
            .skip(1)
            .map(|x| x.split_whitespace().next().unwrap())
            .collect();
        let pos: Vec<(i64, i64)> = cells
            .iter()
            .filter_map(|c| {
                text.lines()
                    .find(|l| l.starts_with(&format!("- {c} ")) && l.contains("PLACED"))
                    .map(|l| {
                        let v: Vec<i64> = l
                            .split("( ")
                            .nth(1)
                            .unwrap()
                            .split_whitespace()
                            .take(2)
                            .map(|x| x.parse().unwrap())
                            .collect();
                        (v[0], v[1])
                    })
            })
            .collect();
        let (x0, x1) = (
            pos.iter().map(|p| p.0).min().unwrap(),
            pos.iter().map(|p| p.0).max().unwrap(),
        );
        let (y0, y1) = (
            pos.iter().map(|p| p.1).min().unwrap(),
            pos.iter().map(|p| p.1).max().unwrap(),
        );
        (x1 - x0) + (y1 - y0)
    };
    assert!(span(&f.path("heavy"), "n9") <= span(&f.path("plain"), "n9"));
    // The weights stay with the workspace: a later place without the option
    // still uses them.
    let again = place(&f.path("heavy"), &h.revision, &limits()).unwrap();
    assert!(again.placement.as_ref().unwrap().net_weights.is_some());
    // Out-of-range weights and a wrong schema are refused.
    for bad in [
        serde_json::json!({"schema": "rustroad.net-weights.v1", "source": {}, "weights": {"n9": 0}}),
        serde_json::json!({"schema": "rustroad.net-weights.v1", "source": {}, "weights": {"n9": 16001}}),
        serde_json::json!({"schema": "other", "source": {}, "weights": {}}),
    ] {
        fs::write(f.path("bad.json"), bad.to_string()).unwrap();
        let d = import(&format!("bad{}", bad["weights"]["n9"]));
        assert!(
            place_weighted(
                &f.path(&format!("bad{}", bad["weights"]["n9"])),
                &d.revision,
                &limits(),
                Some(&f.path("bad.json"))
            )
            .is_err()
        );
    }
}

#[test]
fn old_route_limits_default_the_independent_active_topology_bound() {
    let mut value = serde_json::to_value(rustroad_core::routing::RouteLimits::default()).unwrap();
    value.as_object_mut().unwrap().remove("max_active_nodes");
    value.as_object_mut().unwrap().remove("max_rule_violations");
    value.as_object_mut().unwrap().remove("max_rule_grid_cells");
    let mut limits: rustroad_core::routing::RouteLimits = serde_json::from_value(value).unwrap();
    assert_eq!(limits.max_active_nodes, 200_000);
    assert_eq!(limits.max_rule_violations, 100_000);
    assert_eq!(limits.max_rule_grid_cells, 1_000_000);
    limits.validate_track().unwrap();
    limits.max_active_nodes = 0;
    assert!(limits.validate_track().is_err());
}

/// Commit-time validation receipts: a routed (schema 6) revision is trusted
/// only while every byte it binds is unchanged and the receipt comes from this
/// validator; anything else falls back to full validation, which then reports
/// what is wrong. All mode assertions live in this one test because the
/// `--verify-workspace` switch (`set_force_validate`) is process-global.
#[test]
fn receipts_gate_trusted_reads_of_routed_revisions() {
    use rustroad_workspace::{
        ReadOptions, Validation, read_with, receipt_path, set_force_validate, verify_workspace,
    };
    let f = Fixture::new();
    let ws = f.flow("rc", "track.json").unwrap();
    let dir = head_dir(&ws);
    let receipt = receipt_path(&ws, dir.file_name().unwrap().to_str().unwrap());
    assert!(receipt.is_file(), "commit writes the receipt");
    // The receipt lives outside the revision: its directory keeps exactly
    // the revision format's files, each an independent inode.
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "design.json",
            "instances.rbin",
            "net_offsets.rbin",
            "net_pins.rbin",
            "physical.rbin",
            "pins.rbin",
            "routes.rbin"
        ]
    );
    #[cfg(unix)]
    for name in &names {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(dir.join(name)).unwrap().nlink(), 1, "{name}");
    }
    let trusted = read(&ws, &limits()).unwrap();
    assert_eq!(trusted.validation(), &Validation::Receipt);
    let full_summary = serde_json::to_value(trusted.summary()).unwrap();
    drop(trusted);
    let trusted_def = def(&ws);

    // Forced full validation gives identical outputs.
    set_force_validate(true);
    let forced = read(&ws, &limits()).unwrap();
    assert_eq!(forced.validation(), &Validation::Full("forced"));
    drop(forced);
    assert_eq!(def(&ws), trusted_def);
    set_force_validate(false);

    // Route-free reads leave the records out but report the same summary;
    // route consumers refuse such a snapshot.
    let lean = read_with(&ws, &limits(), ReadOptions { routes: false }).unwrap();
    assert_eq!(lean.validation(), &Validation::Receipt);
    assert!(lean.routes_deferred());
    assert_eq!(serde_json::to_value(lean.summary()).unwrap(), full_summary);
    assert!(lean.export_def(&mut Vec::new()).is_err());
    drop(lean);

    // Same content, different bytes (trailing newline): not trusted, but valid.
    let design = dir.join("design.json");
    let original = fs::read(&design).unwrap();
    let mut spaced = original.clone();
    spaced.push(b'\n');
    fs::write(&design, &spaced).unwrap();
    let s = read(&ws, &limits()).unwrap();
    assert_eq!(s.validation(), &Validation::Full("receipt_digest_mismatch"));
    drop(s);
    fs::write(&design, &original).unwrap();

    // A consistent-looking tamper: shift a stored wire and re-pin its SHA-256
    // in design.json. The old receipt no longer matches, so full validation
    // runs and rejects the geometry.
    let table = dir.join("routes.rbin");
    let routes = fs::read(&table).unwrap();
    let mut moved = routes.clone();
    assert_eq!(
        i64::from_le_bytes(moved[..8].try_into().unwrap()),
        1,
        "wire record"
    );
    for field in [3, 5] {
        let at = field * 8;
        let x = i64::from_le_bytes(moved[at..at + 8].try_into().unwrap());
        moved[at..at + 8].copy_from_slice(&(x + 4000).to_le_bytes());
    }
    fs::write(&table, &moved).unwrap();
    let mut meta: serde_json::Value = serde_json::from_slice(&original).unwrap();
    use sha2::Digest;
    meta["physical"]["routing"]["table"]["sha256"] =
        format!("{:x}", sha2::Sha256::digest(&moved)).into();
    fs::write(&design, serde_json::to_vec(&meta).unwrap()).unwrap();
    let error = read(&ws, &limits())
        .err()
        .expect("shifted wire is rejected");
    assert!(
        matches!(error, Error::Design(_) | Error::Invalid(_)),
        "{error}"
    );
    // The route table alone (old design.json): the SHA-256 pin rejects it.
    fs::write(&design, &original).unwrap();
    assert!(matches!(read(&ws, &limits()), Err(Error::Invalid(_))));
    fs::write(&table, &routes).unwrap();
    assert_eq!(
        read(&ws, &limits()).unwrap().validation(),
        &Validation::Receipt
    );

    // A receipt from another validator build is ignored.
    let stamp = fs::read(&receipt).unwrap();
    let mut foreign: serde_json::Value = serde_json::from_slice(&stamp).unwrap();
    foreign["build"] = "0.0.0+other".into();
    fs::write(&receipt, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert_eq!(
        read(&ws, &limits()).unwrap().validation(),
        &Validation::Full("receipt_other_validator")
    );
    // Missing receipt: full validation; stamping restores trust.
    fs::remove_file(&receipt).unwrap();
    assert_eq!(
        read(&ws, &limits()).unwrap().validation(),
        &Validation::Full("receipt_absent")
    );
    let checked = verify_workspace(&ws, &limits(), false).unwrap();
    assert_eq!(
        (checked.previous_receipt, checked.receipt),
        ("receipt_absent", "not_requested")
    );
    assert!(!receipt.exists());
    let stamped = verify_workspace(&ws, &limits(), true).unwrap();
    assert_eq!(stamped.receipt, "written");
    assert_eq!(
        fs::read(&receipt).unwrap(),
        stamp,
        "stamp equals the commit receipt"
    );
    assert_eq!(
        read(&ws, &limits()).unwrap().validation(),
        &Validation::Receipt
    );
    assert_eq!(
        verify_workspace(&ws, &limits(), true).unwrap().receipt,
        "current"
    );
    // Stamping never writes a receipt for a revision that fails validation.
    fs::write(&table, &moved).unwrap();
    fs::write(&design, serde_json::to_vec(&meta).unwrap()).unwrap();
    fs::remove_file(&receipt).unwrap();
    assert!(verify_workspace(&ws, &limits(), true).is_err());
    assert!(!receipt.exists());
    fs::write(&table, &routes).unwrap();
    fs::write(&design, &original).unwrap();
    assert_eq!(def(&ws), trusted_def);
}
