//! Real-PDK regression from OpenC910 `ct_core` full-route-38 (batch13): input
//! B2 of sky130 `a222oi_1` i13095 had a single legal landing and the cheapest
//! escape of its neighbour B1 covered it. Repair of the vertically adjacent
//! a222oi_1 kept re-planning B1 before the unplanned B2, so B2 reached its
//! attempt limit and the batch failed. The fixture is a geometry-only
//! projection of that neighbourhood: 10 cells at their exact full-design
//! coordinates and a delta-minimized set of 10 nets (removing any one lets the
//! old router pass). `fut` and dangling outputs stand in for later-batch nets;
//! the netlist has no functional meaning.
//!
//! Upfront resource estimate: one parse of the pinned sky130 HD technology and
//! cell LEF (1.4 MB, 437 macros); 10 logic cells plus fillers in 5 rows, 21
//! nets/174 pins; the fixture's routing limits (1 GiB track-router cap, 10k
//! active nodes, 2M expansions). The replay harness measured <16 MiB RSS per
//! stage; expected test RSS <128 MiB. Run with two test threads under a
//! 4 GiB/no-swap/180 s guard. Disk output lives under `.work/access-landing`.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::routing::{RouteScope, route};
use rustroad_workspace::{place_with, read};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/routing/openc910-a222oi-landing")
        .join(name)
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires explicit pinned Ciel PDK LEF paths; absence is unverified"]
fn a222oi_b2_landing_survives_neighbour_repair() {
    let lef = |variable: &str| {
        PathBuf::from(
            std::env::var(variable).expect("explicit PDK path required for real verification"),
        )
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Scratch(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/access-landing")
            .join(format!("{}-{stamp}", std::process::id())),
    );
    fs::create_dir_all(&root.0).unwrap();
    let workspace = root.0.join("ws");
    let limits = Limits {
        max_cells: 250_000,
        max_rows: 100_000,
        max_expansions: 100_000_000,
    };
    let connectivity = ConnectivityLimits {
        max_instances: 250_000,
        max_nets: 250_000,
        max_pins: 1_000_000,
        max_aliases: 500_000,
        ..Default::default()
    };
    let sources = ImportSources {
        technology_lef: lef("RUSTROAD_SKY130_TECH_LEF"),
        cell_lef: lef("RUSTROAD_SKY130_CELL_LEF"),
        netlist: fixture("input.v"),
        floorplan: fixture("floorplan.json"),
    };
    let imported = import_pdk_with_config(
        &workspace,
        "rev0",
        &sources,
        &limits,
        &connectivity,
        Some(&fixture("physical.json")),
    )
    .unwrap();
    assert_eq!(imported.revision, "rev1");
    let placed = place_with(
        &workspace,
        "rev1",
        &limits,
        None,
        Some(&fixture("seed.json")),
    )
    .unwrap();
    assert_eq!(placed.revision, "rev2");

    // Every seeded cell keeps its exact full-design placement.
    let seed: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture("seed.json")).unwrap()).unwrap();
    let mut def = Vec::new();
    read(&workspace, &limits)
        .unwrap()
        .export_def(&mut def)
        .unwrap();
    let def = String::from_utf8(def).unwrap();
    for (name, at) in seed["cells"].as_object().unwrap() {
        let component = format!("- {name} ");
        let site = format!("( {} {} )", at[0], at[1]);
        assert!(
            def.lines()
                .any(|l| l.trim_start().starts_with(&component) && l.contains(&site)),
            "{name} is not placed at {site}"
        );
    }

    // Negative control: without a retry (no alternate access) the greedy
    // reservation fails on this fixture and HEAD stays unchanged.
    let head = fs::read(workspace.join("HEAD")).unwrap();
    let mut greedy: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture("routing.json")).unwrap()).unwrap();
    greedy["limits"]["max_batch_retries"] = 0.into();
    let greedy_path = root.0.join("greedy-routing.json");
    fs::write(&greedy_path, serde_json::to_vec(&greedy).unwrap()).unwrap();
    let error = route(&workspace, "rev2", &greedy_path, RouteScope::All, &limits).unwrap_err();
    assert!(
        error.to_string().contains("batch recovery retry limit"),
        "{error}"
    );
    assert_eq!(fs::read(workspace.join("HEAD")).unwrap(), head);

    let routed = route(
        &workspace,
        "rev2",
        &fixture("routing.json"),
        RouteScope::All,
        &limits,
    )
    .unwrap();
    assert_eq!(routed.revision, "rev3");
    assert_eq!(routed.routing_status, "signal_geometry_checked");
}
