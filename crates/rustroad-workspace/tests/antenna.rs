//! Self-authored routed fixture for the native antenna check: three 1 x 2 um
//! buffers with LEF gate/diffusion areas, an antenna diode master, three
//! routing layers with OXIDE1 rules. Before implementation: < 1 MiB of
//! sources/workspace per test, two test workers, no PDK parse. Foundry-deck
//! agreement is verified separately by scripts/hold_closure.py.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_workspace::antenna::{
    AntennaRequest, RepairRequest, RepairScope, check_antenna, repair_antenna,
};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
use rustroad_workspace::{place, read};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const TECHNOLOGY: &str = "VERSION 5.8 ;\n\
UNITS DATABASE MICRONS 1000 ; END UNITS\n\
MANUFACTURINGGRID 0.01 ;\n\
LAYER M1 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; THICKNESS 0.3 ;\n\
 ANTENNAMODEL OXIDE1 ; ANTENNASIDEAREARATIO 400 ; END M1\n\
LAYER V1 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; ANTENNAAREARATIO 20 ; END V1\n\
LAYER M2 TYPE ROUTING ; DIRECTION VERTICAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; THICKNESS 0.3 ;\n\
 ANTENNAMODEL OXIDE1 ; ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ; END M2\n\
LAYER V2 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V2\n\
LAYER M3 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; THICKNESS 0.5 ;\n\
 ANTENNAMODEL OXIDE1 ; ANTENNADIFFSIDEAREARATIO PWL ( ( 0 400 ) ( 0.0125 400 ) ( 0.0225 2609 ) ( 22.5 11600 ) ) ; END M3\n\
SITE s CLASS CORE ; SYMMETRY X Y ; SIZE 1 BY 2 ; END s\n\
VIA V12 DEFAULT\n\
LAYER M1 ; RECT -0.05 -0.05 0.05 0.05 ;\n\
LAYER V1 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M2 ; RECT -0.05 -0.05 0.05 0.05 ; END V12\n\
VIA V23 DEFAULT\n\
LAYER M2 ; RECT -0.05 -0.05 0.05 0.05 ;\n\
LAYER V2 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M3 ; RECT -0.05 -0.05 0.05 0.05 ; END V23\n\
END LIBRARY\n";

fn library() -> String {
    let mut text = String::from("VERSION 5.8 ;\n");
    for name in ["BUF", "HI", "LO", "FILL", "DIO"] {
        let class = if name == "DIO" {
            "CORE ANTENNACELL"
        } else {
            "CORE"
        };
        text.push_str(&format!(
            "MACRO {name}\nCLASS {class} ; ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\n"
        ));
        if name == "BUF" {
            text.push_str("PIN A DIRECTION INPUT ; USE SIGNAL ; ANTENNAGATEAREA 0.1 ; PORT LAYER M1 ; RECT 0.3 0.4 0.5 0.6 ; END END A\n");
        }
        if name == "DIO" {
            text.push_str("PIN D DIRECTION INPUT ; USE SIGNAL ; ANTENNADIFFAREA 0.4 ; PORT LAYER M1 ; RECT 0.3 0.4 0.5 0.6 ; END END D\n");
        }
        if !matches!(name, "FILL" | "DIO") {
            text.push_str("PIN Y DIRECTION OUTPUT ; USE SIGNAL ; ANTENNADIFFAREA 0.2 ; PORT LAYER M1 ; RECT 0.5 1.3 0.7 1.5 ; END END Y\n");
        }
        text.push_str("PIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER M1 ; RECT 0 -0.1 1 0.1 ; END END VSS\n");
        text.push_str("PIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER M1 ; RECT 0 1.9 1 2.1 ; END END VDD\n");
        text.push_str(&format!("END {name}\n"));
    }
    text.push_str("END LIBRARY\n");
    text
}

fn limits() -> Limits {
    Limits {
        max_cells: 1000,
        max_rows: 16,
        max_expansions: 5000,
    }
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
        max_added_cells: 64,
        max_tie_fanout: 8,
        tap: None,
        port_buffer: None,
        input_buffer: None,
        clock: Some(rustroad_workspace::clock::ClockConfig {
            root_net: None,
            port: "a".into(),
            cell: "BUF".into(),
            input_pin: "A".into(),
            output_pin: "Y".into(),
            sink_pin: "A".into(),
            max_fanout: 3,
            max_buffers: 10,
            max_work: 1000,
            level_cells: Vec::new(),
            clustering: Default::default(),
            regroup_prefixes: Vec::new(),
        }),
        placer: None,
        io_plan: None,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    sources: ImportSources,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn routed(netlist: &str) -> Fixture {
    routed_with_technology(netlist, TECHNOLOGY)
}

/// Import, place, build the clock tree and route every signal net.
fn routed_with_technology(netlist: &str, technology: &str) -> Fixture {
    routed_with_scope(
        netlist,
        technology,
        rustroad_workspace::routing::RouteScope::All,
    )
}

fn routed_with_scope(
    netlist: &str,
    technology: &str,
    scope: rustroad_workspace::routing::RouteScope,
) -> Fixture {
    routed_with_cells(netlist, technology, scope, &library())
}

fn routed_with_cells(
    netlist: &str,
    technology: &str,
    scope: rustroad_workspace::routing::RouteScope,
    cells: &str,
) -> Fixture {
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/antenna-tests");
    fs::create_dir_all(&parent).unwrap();
    let root = parent.join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let sources = ImportSources {
        technology_lef: root.join("technology.lef"),
        cell_lef: root.join("cells.lef"),
        netlist: root.join("mapped.v"),
        floorplan: root.join("floorplan.json"),
    };
    fs::write(&sources.technology_lef, technology).unwrap();
    fs::write(&sources.cell_lef, cells).unwrap();
    fs::write(&sources.netlist, netlist).unwrap();
    let floorplan = serde_json::json!({
        "name":"chip", "units_per_micron":1000,
        "die_area":{"lx":0,"ly":0,"ux":20000,"uy":20000},
        "core_area":{"lx":5000,"ly":5000,"ux":15000,"uy":9000},
        "site":{"name":"s","width":1000,"height":2000},
        "rows":[{"x_start":5000,"x_end":15000,"y":5000,"orientation":"N"},
                 {"x_start":5000,"x_end":15000,"y":7000,"orientation":"N"}]
    });
    fs::write(&sources.floorplan, serde_json::to_vec(&floorplan).unwrap()).unwrap();
    let config_path = root.join("physical.json");
    fs::write(&config_path, serde_json::to_vec(&config()).unwrap()).unwrap();
    let workspace = root.join("workspace");
    import_pdk_with_config(
        &workspace,
        "rev0",
        &sources,
        &limits(),
        &ConnectivityLimits::default(),
        Some(&config_path),
    )
    .unwrap();
    place(&workspace, "rev1", &limits()).unwrap();
    let sdc = root.join("clock.sdc");
    fs::write(
        &sdc,
        "create_clock -name core -period 10 [get_ports a]\nset_clock_transition 0.1 [get_clocks core]\n",
    )
    .unwrap();
    let clk =
        rustroad_workspace::clock::synthesize_with_sdc(&workspace, "rev2", &limits(), Some(&sdc))
            .unwrap();
    let routing = root.join("routing.json");
    let value = serde_json::json!({"grid_step":400,"access_layer":"M2",
        "layers":[{"name":"M1","spacing":200},{"name":"M2","spacing":200},{"name":"M3","spacing":200}],
        "vias":[{"name":"V12","cut_spacing":60},{"name":"V23","cut_spacing":60}],"max_skew_ns":1.0});
    fs::write(&routing, serde_json::to_vec(&value).unwrap()).unwrap();
    rustroad_workspace::routing::route(&workspace, &clk.revision, &routing, scope, &limits())
        .unwrap();
    Fixture {
        root,
        workspace,
        sources,
    }
}

#[test]
fn clock_only_routing_is_partial_and_cannot_be_repaired_as_a_full_check() {
    let f = routed_with_scope(
        PLAIN,
        TECHNOLOGY,
        rustroad_workspace::routing::RouteScope::Clock,
    );
    let r = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(r.status, "partial");
    assert_eq!(r.scope.routing_scope, "clock_nets_only");
    assert!(
        r.diagnostics
            .iter()
            .any(|d| d.contains("ANTENNA_PARTIAL_ROUTING"))
    );
    let head = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let error = repair_antenna(
        &f.workspace,
        head.trim(),
        &RepairRequest::default(),
        &limits(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ANTENNA_UNMODELED"));
    assert_eq!(head, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
}

#[test]
fn explicit_clock_selection_stays_partial_and_rejects_unknown_or_duplicate_nets() {
    let f = routed_with_scope(
        PLAIN,
        TECHNOLOGY,
        rustroad_workspace::routing::RouteScope::Clock,
    );
    let before = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let net = before.checks.first().unwrap().net.clone();
    let base = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let make = |names| RepairRequest {
        scope: RepairScope::SelectedSupportedNets(names),
        ..RepairRequest::default()
    };
    let result = repair_antenna(
        &f.workspace,
        base.trim(),
        &make(vec![net.clone()]),
        &limits(),
    )
    .unwrap();
    assert_eq!(result.status, "subset_clean");
    assert_eq!(result.routing_scope, "clock_nets_only");
    assert_eq!(result.repair_scope, "selected_supported_nets");
    assert_eq!(result.selected_nets.as_slice(), std::slice::from_ref(&net));
    assert_eq!(result.antenna_status, "partial_selected_supported_nets");
    assert!(result.requires_full_revalidation);
    for names in [vec![], vec![net.clone(), net], vec!["missing_net".into()]] {
        let error = repair_antenna(&f.workspace, base.trim(), &make(names), &limits()).unwrap_err();
        assert!(error.to_string().contains("ANTENNA_SELECTION"), "{error}");
        assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
    }
}

#[test]
fn named_inventory_matches_routed_members_and_selected_source_without_signoff_claims() {
    use rustroad_workspace::antenna::{InventoryBounds, antenna_inventory};
    use sha2::{Digest, Sha256};
    let f = routed(PLAIN);
    let bounds = InventoryBounds::default();
    let report = antenna_inventory(&f.workspace, &bounds, &limits()).unwrap();
    let snapshot = read(&f.workspace, &limits()).unwrap();
    let bundle = snapshot.metadata().physical.as_ref().unwrap();
    let routes = &bundle.routing.as_ref().unwrap().routes;
    let expected = routes
        .nets
        .iter()
        .copied()
        .filter(|&id| (id as usize) < bundle.connectivity.nets.len())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        report
            .nets
            .iter()
            .map(|e| e.net_id)
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(report.routed_logical_net_count, expected.len());
    assert_eq!(
        report.model_logical_net_count,
        bundle.connectivity.nets.len()
    );
    assert_eq!(
        report.excluded_nonlogical_supply_tie_count + expected.len(),
        report.router_declared_net_count
    );
    let names = report
        .nets
        .iter()
        .map(|e| {
            assert_eq!(
                e.canonical_name,
                bundle.connectivity.nets[e.net_id as usize].name
            );
            e.canonical_name.clone()
        })
        .collect::<Vec<_>>();
    assert!(names.windows(2).all(|w| w[0] < w[1]));
    let selected = check_antenna(
        &f.workspace,
        &AntennaRequest {
            selected_nets: Some(names),
            ..request(None)
        },
        &limits(),
    )
    .unwrap();
    assert_eq!(
        report.source_sha256,
        selected.selection.unwrap().source_sha256
    );
    let canonical = serde_json::to_vec(&(
        &report.workspace_revision,
        &report.routing_scope,
        &report.source_sha256,
        &report.nets,
    ))
    .unwrap();
    assert_eq!(
        report.inventory_sha256,
        format!("{:x}", Sha256::digest(canonical))
    );
    assert_eq!(report.status, "inventory_only");
    assert_eq!(
        (
            report.antenna_status,
            report.drc_status,
            report.lvs_status,
            report.timing_status
        ),
        ("unchecked", "unchecked", "unchecked", "unchecked")
    );
    for b in [
        InventoryBounds {
            max_work: 1,
            ..bounds.clone()
        },
        InventoryBounds {
            max_routed_ids: 1,
            ..bounds.clone()
        },
        InventoryBounds {
            memory_bytes: 1,
            ..bounds.clone()
        },
        InventoryBounds {
            max_output_bytes: 1,
            ..bounds.clone()
        },
        InventoryBounds {
            max_name_bytes: 1,
            ..bounds.clone()
        },
    ] {
        assert!(antenna_inventory(&f.workspace, &b, &limits()).is_err());
    }
    fs::write(&f.sources.cell_lef, "changed").unwrap();
    assert!(
        antenna_inventory(&f.workspace, &bounds, &limits())
            .unwrap_err()
            .to_string()
            .contains("ANTENNA_SOURCE_CHANGED")
    );
}

#[test]
fn inventory_holds_its_shared_lock_until_output_owner_is_dropped() {
    use rustroad_workspace::antenna::{InventoryBounds, antenna_inventory};
    let f = routed(PLAIN);
    let report = antenna_inventory(&f.workspace, &InventoryBounds::default(), &limits()).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.workspace.join("workspace.lock"))
        .unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&file).is_err());
    drop(report);
    fs2::FileExt::try_lock_exclusive(&file).unwrap();
}

#[test]
fn selected_hierarchical_pin_history_is_still_rejected() {
    let cells = library().replace(
        "ANTENNAGATEAREA 0.1 ;",
        "ANTENNAGATEAREA 0.1 ; ANTENNAMAXAREACAR 10 LAYER M1 ;",
    );
    let f = routed_with_cells(
        PLAIN,
        TECHNOLOGY,
        rustroad_workspace::routing::RouteScope::Clock,
        &cells,
    );
    let before = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(before.status, "partial");
    let net = before.checks.first().unwrap().net.clone();
    let base = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let error = repair_antenna(
        &f.workspace,
        base.trim(),
        &RepairRequest {
            scope: RepairScope::SelectedSupportedNets(vec![net]),
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ANTENNA_UNMODELED"), "{error}");
    assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
}

#[test]
fn cumulative_report_keeps_gate_identity() {
    let tech = TECHNOLOGY.replace(
        "ANTENNAGATEPLUSDIFF 15 ; END M2",
        "ANTENNAGATEPLUSDIFF 15 ; ANTENNACUMAREARATIO 0.01 ; END M2",
    );
    let f = routed_with_scope(PLAIN, &tech, rustroad_workspace::routing::RouteScope::Clock);
    let before = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let bad: Vec<_> = before
        .checks
        .iter()
        .filter(|r| r.violated && r.kind == "cumulative_area")
        .collect();
    assert!(!bad.is_empty());
    let base = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let mut altered = serde_json::to_value(&before).unwrap();
    let row = altered["checks"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|r| r["violated"] == true)
        .unwrap();
    row["gates"][0] = serde_json::json!("another_gate");
    let path = f.root.join("before.json");
    fs::write(&path, serde_json::to_vec(&altered).unwrap()).unwrap();
    let request = RepairRequest {
        report: Some(path.clone()),
        scope: RepairScope::SelectedSupportedNets(vec![bad[0].net.clone()]),
        max_jumpers: 1,
        max_jumpers_per_net: 1,
        ..RepairRequest::default()
    };
    let error = repair_antenna(&f.workspace, base.trim(), &request, &limits()).unwrap_err();
    assert!(error.to_string().contains("STALE_REPORT"), "{error}");
    assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
    fs::write(&path, serde_json::to_vec(&before).unwrap()).unwrap();
    let result = repair_antenna(&f.workspace, base.trim(), &request, &limits()).unwrap();
    assert!(result.status.starts_with("subset_"));
    assert!(result.requires_full_revalidation);
}

#[test]
fn repair_work_cap_is_not_reset_for_every_candidate() {
    let tech = TECHNOLOGY.replace(
        "ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
        "ANTENNADIFFSIDEAREARATIO 20 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
    );
    let f = routed_with_technology(PLAIN, &tech);
    let before = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert!(before.violations > 0);
    let base = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let error = repair_antenna(
        &f.workspace,
        base.trim(),
        &RepairRequest {
            antenna: AntennaRequest {
                max_work: before.stats.work,
                ..AntennaRequest::default()
            },
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap_err();
    assert!(
        error.to_string().to_ascii_lowercase().contains("budget"),
        "{error}"
    );
    assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
}

#[test]
fn spent_planner_budget_leaves_nets_unrepaired_instead_of_failing() {
    let tech = TECHNOLOGY.replace(
        "ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
        "ANTENNADIFFSIDEAREARATIO 20 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
    );
    let head = |f: &Path| fs::read_to_string(f.join("HEAD")).unwrap().trim().to_string();
    let f = routed_with_technology(PLAIN, &tech);
    let full = repair_antenna(&f.workspace, &head(&f.workspace), &RepairRequest::default(), &limits())
        .unwrap();
    assert!(!full.jumpers.is_empty() && full.stats.planner_work > 1);
    // A spent planner cap ends in explicit unrepaired nets, not a failed command.
    let f = routed_with_technology(PLAIN, &tech);
    let base = head(&f.workspace);
    let request = RepairRequest {
        max_planner_work: Some(full.stats.planner_work / 2),
        ..RepairRequest::default()
    };
    let r = repair_antenna(&f.workspace, &base, &request, &limits()).unwrap();
    let spent: Vec<_> = r
        .unrepaired
        .iter()
        .filter(|u| u.reason == "jumper planner work budget exhausted")
        .collect();
    assert!(!spent.is_empty(), "{:?}", r.unrepaired);
    assert!(spent.iter().all(|u| u.ratio > u.effective_limit));
    assert_ne!(r.status, "repaired");
    assert!(r.stats.planner_work < full.stats.planner_work);
}

#[test]
fn cumulative_side_and_cut_histories_remain_separate() {
    let mut tech = TECHNOLOGY.to_string();
    for end in ["END M1", "END M2", "END M3"] {
        tech = tech.replace(end, &format!("ANTENNACUMSIDEAREARATIO 1000000 ; {end}"));
    }
    tech = tech
        .replace("END V1\n", "ANTENNACUMAREARATIO 1000000 ; END V1\n")
        .replace(
            "END V2\n",
            "ANTENNAAREARATIO 1000000 ; ANTENNACUMAREARATIO 1000000 ; END V2\n",
        );
    let f = routed_with_technology(PLAIN, &tech);
    let r = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(r.status, "passed");
    let sum = |kind: &str, cut: bool| {
        r.checks
            .iter()
            .filter(|c| {
                c.net == "a" && c.kind == kind && c.layer.starts_with(if cut { 'V' } else { 'M' })
            })
            .map(|c| c.ratio)
            .sum::<f64>()
    };
    let side = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "M3" && c.kind == "cumulative_side_area")
        .unwrap();
    let cut = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "V2" && c.kind == "cumulative_area")
        .unwrap();
    assert!((side.ratio - sum("side_area", false)).abs() < 1e-10);
    assert!((cut.ratio - sum("area", true)).abs() < 1e-10);
}

#[test]
fn cumulative_diffusion_pwl_changes_the_connected_gate_limit() {
    let tech = TECHNOLOGY.replace(
        "END M3",
        "ANTENNACUMDIFFAREARATIO PWL ( ( 0 1 ) ( 0.1 1000 ) ( 100 1000000 ) ) ; END M3",
    );
    let before = routed_with_technology(PLAIN, &tech);
    let r = check_antenna(&before.workspace, &request(None), &limits()).unwrap();
    let root = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "M3" && c.kind == "cumulative_area")
        .unwrap();
    assert_eq!(root.limit, 1.0);
    assert!(root.violated);
    let diode = PLAIN.replace("endmodule", "DIO d0(.D(a)); endmodule");
    let after = routed_with_technology(&diode, &tech);
    let r = check_antenna(&after.workspace, &request(None), &limits()).unwrap();
    let root = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "M3" && c.kind == "cumulative_area")
        .unwrap();
    assert!(root.diff_area_um2 >= 0.4 && root.limit > 1000.0 && !root.violated);
    assert!(root.rule.starts_with("ANTENNACUMDIFFAREARATIO@diffusion"));
    assert_eq!(r.status, "passed");
}

const PLAIN: &str = "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); \
                     BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule";

/// A `length` um M3 wire of minimum width attached to port `a`'s M3 pin.
fn long_wire(f: &Fixture, length: i64) -> PathBuf {
    let s = read(&f.workspace, &limits()).unwrap();
    let pin = s
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .state
        .as_ref()
        .unwrap()
        .io_pins
        .iter()
        .find(|p| p.name == "a")
        .unwrap()
        .clone();
    let r = pin.rect;
    let cx = (r.lx + r.ux) / 2;
    let rect = [cx - 100, r.ly - length * 1000, cx + 100, r.uy];
    let path = f.root.join(format!("extra-{length}.json"));
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!([{"net":"a","layer":"M3","rect":rect}])).unwrap(),
    )
    .unwrap();
    path
}

fn request(extra: Option<&Path>) -> AntennaRequest {
    AntennaRequest {
        extra_shapes: extra.map(Path::to_path_buf),
        ..AntennaRequest::default()
    }
}

#[test]
fn selected_reports_partition_full_rows_and_filter_before_retaining_shapes() {
    let f = routed(PLAIN);
    let full = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let legacy = serde_json::to_value(&full).unwrap();
    assert!(legacy.get("selection").is_none());
    assert!(legacy["stats"].get("report_memory").is_none());
    let names: BTreeSet<_> = full.checks.iter().map(|row| row.net.clone()).collect();
    assert!(names.len() > 1);
    let inventory = |rows: &[rustroad_workspace::antenna::CheckRow]| {
        let mut counts = BTreeMap::new();
        for row in rows {
            *counts
                .entry(serde_json::to_string(row).unwrap())
                .or_insert(0usize) += 1;
        }
        counts
    };
    let mut partitioned = Vec::new();
    for name in names {
        let selected = AntennaRequest {
            selected_nets: Some(vec![name.clone()]),
            ..request(None)
        };
        let report = check_antenna(&f.workspace, &selected, &limits()).unwrap();
        let scope = report.selection.as_ref().unwrap();
        assert_eq!(scope.canonical_nets, vec![name.clone()]);
        assert_eq!(scope.coverage.len(), 1);
        assert_eq!(scope.coverage[0].rows, report.checks.len());
        assert!(report.checks.iter().all(|row| row.net == name));
        let memory = report.stats.report_memory.as_ref().unwrap();
        assert!(memory.live_bytes <= memory.peak_live_bytes);
        assert!(memory.peak_live_bytes <= memory.limit_bytes);
        assert_eq!(
            memory.cumulative_row_bytes,
            report.stats.report_memory_bytes
        );
        assert!(report.stats.shapes < full.stats.shapes);
        let tight = AntennaRequest {
            max_total_shapes: report.stats.shapes,
            ..selected
        };
        let bounded = check_antenna(&f.workspace, &tight, &limits()).unwrap();
        assert_eq!(inventory(&bounded.checks), inventory(&report.checks));
        assert!(
            check_antenna(
                &f.workspace,
                &AntennaRequest {
                    max_total_shapes: report.stats.shapes,
                    ..request(None)
                },
                &limits(),
            )
            .unwrap_err()
            .to_string()
            .contains("max_total_shapes")
        );
        partitioned.extend(report.checks);
    }
    assert_eq!(inventory(&partitioned), inventory(&full.checks));
}

#[test]
fn selected_no_gate_is_explicit_and_cannot_be_used_as_clean_repair_evidence() {
    let f = routed(PLAIN);
    let report = check_antenna(
        &f.workspace,
        &AntennaRequest {
            selected_nets: Some(vec!["y".into()]),
            ..request(None)
        },
        &limits(),
    )
    .unwrap();
    assert_eq!(report.status, "subset_partial");
    assert!(report.checks.is_empty());
    let coverage = &report.selection.as_ref().unwrap().coverage[0];
    assert_eq!(coverage.net, "y");
    assert_eq!(
        coverage.observation,
        rustroad_workspace::antenna::NetObservation::NoGateArea
    );
    let path = f.root.join("no-observation.json");
    fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let error = repair_antenna(
        &f.workspace,
        &report.workspace_revision,
        &RepairRequest {
            scope: RepairScope::SelectedSupportedNets(vec!["y".into()]),
            selected_report: Some(path),
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ANTENNA_SELECTION"), "{error}");
    assert_eq!(
        report.workspace_revision,
        fs::read_to_string(f.workspace.join("HEAD")).unwrap().trim()
    );
    for names in [vec![], vec!["y".into(), "y".into()], vec!["absent".into()]] {
        assert!(
            check_antenna(
                &f.workspace,
                &AntennaRequest {
                    selected_nets: Some(names),
                    ..request(None)
                },
                &limits()
            )
            .unwrap_err()
            .to_string()
            .contains("ANTENNA_SELECTION")
        );
    }
}

#[test]
fn selected_binding_and_fresh_evaluation_share_the_work_cap() {
    let f = routed(PLAIN);
    let full = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let names = vec![full.checks[0].net.clone()];
    let mut selected = AntennaRequest {
        selected_nets: Some(names.clone()),
        ..request(None)
    };
    let measured = check_antenna(&f.workspace, &selected, &limits()).unwrap();
    selected.max_work = measured.stats.work;
    let bounded = check_antenna(&f.workspace, &selected, &limits()).unwrap();
    assert_eq!(bounded.stats.work, selected.max_work);
    let path = f.root.join("bounded-selected.json");
    fs::write(&path, serde_json::to_vec(&bounded).unwrap()).unwrap();
    let error = repair_antenna(
        &f.workspace,
        &bounded.workspace_revision,
        &RepairRequest {
            scope: RepairScope::SelectedSupportedNets(names),
            selected_report: Some(path),
            antenna: AntennaRequest {
                max_work: selected.max_work,
                ..request(None)
            },
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("max_work"), "{error}");
    assert_eq!(
        bounded.workspace_revision,
        fs::read_to_string(f.workspace.join("HEAD")).unwrap().trim()
    );
}

#[test]
fn selected_report_binding_rejects_forgery_and_inherits_only_its_row_bound() {
    let f = routed(PLAIN);
    let full = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let name = full.checks.first().unwrap().net.clone();
    let report = check_antenna(
        &f.workspace,
        &AntennaRequest {
            selected_nets: Some(vec![name.clone()]),
            max_rows: 200_000,
            ..request(None)
        },
        &limits(),
    )
    .unwrap();
    let original = serde_json::to_value(&report).unwrap();
    let path = f.root.join("selected.json");
    let base = fs::read_to_string(f.workspace.join("HEAD")).unwrap();
    let req = RepairRequest {
        scope: RepairScope::SelectedSupportedNets(vec![name]),
        selected_report: Some(path.clone()),
        ..RepairRequest::default()
    };
    type ReportMutation = Box<dyn Fn(&mut serde_json::Value)>;
    let mut controls: Vec<ReportMutation> = vec![
        Box::new(|v| v["workspace_revision"] = serde_json::json!("rev0")),
        Box::new(|v| v["safety_margin"]["fraction"] = serde_json::json!(0.0)),
        Box::new(|v| v["selection"]["source_sha256"] = serde_json::json!("0".repeat(64))),
        Box::new(|v| v["selection"]["selection_sha256"] = serde_json::json!("0".repeat(64))),
        Box::new(|v| v["selection"]["canonical_nets"][0] = serde_json::json!("another_net")),
        Box::new(|v| {
            v["selection"]["canonical_nets"]
                .as_array_mut()
                .unwrap()
                .clear();
        }),
        Box::new(|v| {
            let n = v["selection"]["canonical_nets"][0].clone();
            v["selection"]["canonical_nets"]
                .as_array_mut()
                .unwrap()
                .push(n);
        }),
        Box::new(|v| v["sources"][0]["sha256"] = serde_json::json!("0".repeat(64))),
        Box::new(|v| v["scope"]["extra_shapes"] = serde_json::json!(1)),
        Box::new(|v| v["checks"][0]["ratio"] = serde_json::json!(123456.0)),
        Box::new(|v| v["checks"][0]["gates"][0] = serde_json::json!("other_gate")),
        Box::new(|v| {
            v["checks"].as_array_mut().unwrap().pop();
        }),
        Box::new(|v| {
            let r = v["checks"][0].clone();
            v["checks"].as_array_mut().unwrap().push(r);
        }),
        Box::new(|v| v["status"] = serde_json::json!("passed")),
    ];
    for (index, mutate) in controls.drain(..).enumerate() {
        let mut forged = original.clone();
        mutate(&mut forged);
        fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
        let error = repair_antenna(&f.workspace, base.trim(), &req, &limits()).unwrap_err();
        assert!(
            error.to_string().contains("STALE_REPORT")
                || error.to_string().contains("ANTENNA_SELECTION"),
            "control{index}: {error}"
        );
        assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
    }
    fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let conflict = RepairRequest {
        max_rows: Some(100_000),
        ..req.clone()
    };
    assert!(
        repair_antenna(&f.workspace, base.trim(), &conflict, &limits())
            .unwrap_err()
            .to_string()
            .contains("REPORT_LIMIT_MISMATCH")
    );
    let typed_conflict = RepairRequest {
        antenna: AntennaRequest {
            max_rows: 50_000,
            ..request(None)
        },
        ..req.clone()
    };
    assert!(
        repair_antenna(&f.workspace, base.trim(), &typed_conflict, &limits())
            .unwrap_err()
            .to_string()
            .contains("REPORT_LIMIT_MISMATCH")
    );
    let legacy = RepairRequest {
        report: Some(path.clone()),
        selected_report: None,
        ..req.clone()
    };
    assert!(
        repair_antenna(&f.workspace, base.trim(), &legacy, &limits())
            .unwrap_err()
            .to_string()
            .contains("STALE_REPORT")
    );
    fs::write(&path, serde_json::to_vec(&full).unwrap()).unwrap();
    assert!(
        repair_antenna(&f.workspace, base.trim(), &req, &limits())
            .unwrap_err()
            .to_string()
            .contains("STALE_REPORT")
    );
    fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let repaired = repair_antenna(&f.workspace, base.trim(), &req, &limits()).unwrap();
    assert_eq!(
        repaired.selected_analysis.as_ref().unwrap().limits.max_rows,
        200_000
    );
    assert!(repaired.stats.report_memory.is_some());
    assert!(repaired.requires_full_revalidation);
    assert_eq!(base, fs::read_to_string(f.workspace.join("HEAD")).unwrap());
}

#[test]
fn selected_report_repairs_real_routed_geometry_and_rejects_old_revision() {
    let tech = TECHNOLOGY.replace(
        "ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
        "ANTENNADIFFSIDEAREARATIO 20 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
    );
    let f = routed_with_technology(PLAIN, &tech);
    let full = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    let names: Vec<_> = full
        .checks
        .iter()
        .filter(|r| r.violated)
        .map(|r| r.net.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let report = check_antenna(
        &f.workspace,
        &AntennaRequest {
            selected_nets: Some(names.clone()),
            ..request(None)
        },
        &limits(),
    )
    .unwrap();
    assert!(report.violations > 0);
    let path = f.root.join("selected-before.json");
    fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    let req = RepairRequest {
        selected_report: Some(path),
        scope: RepairScope::SelectedSupportedNets(names.clone()),
        ..RepairRequest::default()
    };
    let result = repair_antenna(&f.workspace, &report.workspace_revision, &req, &limits()).unwrap();
    assert!(!result.jumpers.is_empty());
    assert_eq!(result.status, "subset_repaired");
    assert_eq!(result.after.violations, 0);
    let fresh = check_antenna(
        &f.workspace,
        &AntennaRequest {
            selected_nets: Some(names),
            ..request(None)
        },
        &limits(),
    )
    .unwrap();
    assert_eq!(fresh.workspace_revision, result.revision);
    assert_eq!(fresh.violations, 0);
    assert!(
        repair_antenna(&f.workspace, &result.revision, &req, &limits())
            .unwrap_err()
            .to_string()
            .contains("STALE_REPORT")
    );
    assert_eq!(
        fresh.workspace_revision,
        fs::read_to_string(f.workspace.join("HEAD")).unwrap().trim()
    );
}

#[test]
fn routed_fixture_passes_and_planted_wire_violates_partial_net_rule() {
    let f = routed(PLAIN);
    let before = fs::read(f.workspace.join("HEAD")).unwrap();
    let clean = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(clean.status, "passed", "{:?}", clean.checks.first());
    assert_eq!(clean.violations, 0);
    assert_eq!(clean.scope.checked_layers, ["M1", "V1", "M2", "M3"]);
    assert_eq!(clean.scope.rule_free_layers, ["V2"]);
    assert_eq!(clean.scope.cumulative, "not_present_in_lef");
    assert!(
        clean
            .checks
            .iter()
            .all(|c| c.gate_area_um2 > 0.0 && c.limit > 0.0)
    );
    // 30 um of 0.2 um M3: side area = 2 x (30 + 0.2 + pin) x 0.5 > 30 um^2 over the
    // root clock buffer's 0.1 um^2 gate -> ratio > 300 but the 0.4 um pin keeps
    // it close; 50 um is clearly above the 400 limit at zero diffusion.
    let planted =
        check_antenna(&f.workspace, &request(Some(&long_wire(&f, 50))), &limits()).unwrap();
    assert_eq!(planted.status, "violated");
    let v = planted.checks.iter().find(|c| c.violated).unwrap();
    assert_eq!(
        (v.net.as_str(), v.layer.as_str(), v.kind.as_str()),
        ("a", "M3", "side_area")
    );
    assert_eq!(v.rule, "ANTENNADIFFSIDEAREARATIO");
    assert_eq!(v.limit, 400.0);
    assert!(v.ratio > 450.0 && v.ratio < 700.0, "{}", v.ratio);
    assert_eq!(v.gates, ["rr_clkbuf_0/A"]);
    // At M2 time the M3 wire does not exist: the M2 partial net of `a` passes.
    assert!(
        planted
            .checks
            .iter()
            .filter(|c| c.net == "a" && c.layer != "M3")
            .all(|c| !c.violated)
    );
    // Read-only and deterministic.
    assert_eq!(before, fs::read(f.workspace.join("HEAD")).unwrap());
    let again = check_antenna(&f.workspace, &request(Some(&long_wire(&f, 50))), &limits()).unwrap();
    assert_eq!(
        serde_json::to_string(&again.checks).unwrap(),
        serde_json::to_string(&planted.checks).unwrap()
    );
}

#[test]
fn diode_on_the_clock_root_repairs_the_planted_wire() {
    let f = routed(
        "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); \
         BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); DIO d0(.D(a)); endmodule",
    );
    let r = check_antenna(&f.workspace, &request(Some(&long_wire(&f, 50))), &limits()).unwrap();
    let m3 = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "M3")
        .unwrap();
    // The diode joins the M3 partial net: PWL limit at 0.4 um^2 diffusion.
    assert!(
        m3.diffusions.contains(&"d0/D".to_string()),
        "{:?}",
        m3.diffusions
    );
    assert!((m3.diff_area_um2 - 0.4).abs() < 1e-12);
    assert!(
        m3.limit > 2700.0 && !m3.violated,
        "{} {}",
        m3.ratio,
        m3.limit
    );
    assert_eq!(r.status, "passed");
}

#[test]
fn changed_sources_unsupported_rules_and_budgets_are_explicit() {
    let f = routed(PLAIN);
    // Too small a pair budget fails explicitly.
    let tight = AntennaRequest {
        max_pair_checks: 1,
        ..AntennaRequest::default()
    };
    assert!(
        check_antenna(&f.workspace, &tight, &limits())
            .unwrap_err()
            .to_string()
            .contains("max_pair_checks")
    );
    // A what-if shape on an unknown net or a cut layer is refused.
    let bad = f.root.join("bad.json");
    fs::write(&bad, r#"[{"net":"nope","layer":"M3","rect":[0,0,10,10]}]"#).unwrap();
    assert!(check_antenna(&f.workspace, &request(Some(&bad)), &limits()).is_err());
    fs::write(&bad, r#"[{"net":"a","layer":"V1","rect":[0,0,10,10]}]"#).unwrap();
    assert!(check_antenna(&f.workspace, &request(Some(&bad)), &limits()).is_err());
    // Unmodeled area-minus-diff rules make the verdict partial, never passed.
    let mut tech = TECHNOLOGY.replace(
        "ANTENNASIDEAREARATIO 400 ; END M1",
        "ANTENNASIDEAREARATIO 400 ; ANTENNAAREAMINUSDIFF 2 ; END M1",
    );
    let g = routed_with_technology(PLAIN, &tech);
    let r = check_antenna(&g.workspace, &request(None), &limits()).unwrap();
    assert_eq!(r.status, "partial");
    assert_eq!(r.scope.cumulative, "not_present_in_lef");
    // A technology LEF edited after import is refused.
    tech.push_str("\n# edited\n");
    fs::write(&g.sources.technology_lef, tech).unwrap();
    assert!(
        check_antenna(&g.workspace, &request(None), &limits())
            .unwrap_err()
            .to_string()
            .contains("ANTENNA_SOURCE_CHANGED")
    );
}

#[test]
fn cumulative_only_rules_are_checked_and_sum_process_stage_ratios() {
    let mut tech = TECHNOLOGY.to_string();
    for end in ["END M1", "END M2", "END M3"] {
        tech = tech.replace(
            end,
            &format!("ANTENNAAREARATIO 1000000 ; ANTENNACUMAREARATIO 100 ; {end}"),
        );
    }
    let f = routed_with_technology(PLAIN, &tech);
    let r = check_antenna(
        &f.workspace,
        &AntennaRequest {
            safety_margin: 0.0,
            ..request(Some(&long_wire(&f, 50)))
        },
        &limits(),
    )
    .unwrap();
    let area_sum: f64 = r
        .checks
        .iter()
        .filter(|row| row.net == "a" && row.kind == "area" && row.layer.starts_with('M'))
        .map(|row| row.ratio)
        .sum();
    let cumulative = r
        .checks
        .iter()
        .find(|row| row.net == "a" && row.layer == "M3" && row.kind == "cumulative_area")
        .expect("cumulative-only rules must produce observed rows");
    assert!(
        (cumulative.ratio - area_sum).abs() < 1e-10,
        "routing CAR {} must exclude cut PARs from routing sum {area_sum}",
        cumulative.ratio
    );
    assert!(cumulative.violated && cumulative.ratio > 100.0);
    assert_eq!(r.scope.cumulative, "per_gate_process_stage_dp");
    assert!(r.diagnostics.iter().all(|d| !d.contains("cumulative")));
    // A high cumulative threshold changes the verdict without changing geometry.
    let clean_tech = tech.replace("ANTENNACUMAREARATIO 100 ;", "ANTENNACUMAREARATIO 1000000 ;");
    let clean = routed_with_technology(PLAIN, &clean_tech);
    let report = check_antenna(&clean.workspace, &request(None), &limits()).unwrap();
    assert_eq!(report.status, "passed");
}

#[test]
fn cumulative_rules_reference_fixtures_and_work_guards() {
    // The exported original LEF/DEF is consumed by an independent OpenROAD
    // executable in the bounded integration harness, never a translated model.
    for (name, threshold) in [("positive", 1000000), ("negative", 1)] {
        let mut tech = TECHNOLOGY.to_string();
        for end in ["END M1", "END M2", "END M3"] {
            tech = tech.replace(end, &format!("ANTENNACUMAREARATIO {threshold} ; {end}"));
        }
        let fixture = routed_with_technology(PLAIN, &tech);
        let report = check_antenna(&fixture.workspace, &request(None), &limits()).unwrap();
        assert_eq!(
            report.status,
            if name == "positive" {
                "passed"
            } else {
                "violated"
            }
        );
        if name == "negative" {
            assert!(
                report
                    .checks
                    .iter()
                    .any(|r| r.kind == "cumulative_area" && r.violated)
            );
        }
        let mut tight = request(None);
        tight.max_work = 1;
        assert!(
            check_antenna(&fixture.workspace, &tight, &limits())
                .unwrap_err()
                .to_string()
                .contains("max_work")
        );
        tight = request(None);
        tight.max_total_shapes = 1;
        assert!(
            check_antenna(&fixture.workspace, &tight, &limits())
                .unwrap_err()
                .to_string()
                .contains("max_total_shapes")
        );
        tight = request(None);
        tight.max_rows = 1;
        assert!(
            check_antenna(&fixture.workspace, &tight, &limits())
                .unwrap_err()
                .to_string()
                .contains("max_rows")
        );
        if let Some(root) = std::env::var_os("RUSTROAD_ANTENNA_EVIDENCE") {
            let output = PathBuf::from(root).join(name);
            fs::create_dir_all(&output).unwrap();
            for (input, name) in [
                (&fixture.sources.technology_lef, "tech.lef"),
                (&fixture.sources.cell_lef, "cells.lef"),
                (&fixture.sources.netlist, "mapped.v"),
            ] {
                fs::copy(input, output.join(name)).unwrap();
            }
            let snapshot = read(&fixture.workspace, &limits()).unwrap();
            let mut def = Vec::new();
            snapshot.export_def(&mut def).unwrap();
            fs::write(output.join("design.def"), def).unwrap();
            fs::write(
                output.join("native.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
        }
    }
}

#[test]
fn safety_margin_flags_ratios_just_below_the_lef_limit() {
    // The native ratio reads low against the foundry deck (phase-B1 sweep),
    // so the native verdict keeps 10 % of every limit as margin: a planted
    // wire whose ratio lies between 0.9 x 400 and 400 passes the raw LEF
    // comparison but is reported violated.
    let f = routed(PLAIN);
    let raw = |length: i64| {
        let r = check_antenna(
            &f.workspace,
            &AntennaRequest {
                safety_margin: 0.0,
                ..request(Some(&long_wire(&f, length)))
            },
            &limits(),
        )
        .unwrap();
        let m3 = r
            .checks
            .iter()
            .find(|c| c.net == "a" && c.layer == "M3")
            .unwrap()
            .clone();
        (r.status, m3)
    };
    let (length, (status, row)) = (20..50)
        .map(|l| (l, raw(l)))
        .find(|(_, (_, row))| row.ratio > 365.0 && row.ratio < 395.0)
        .expect("a planted length in the 0.9..1.0 band");
    assert_eq!(status, "passed", "{}", row.ratio);
    assert!(!row.violated && !row.exceeds_lef_limit && row.effective_limit == row.limit);
    let r = check_antenna(
        &f.workspace,
        &request(Some(&long_wire(&f, length))),
        &limits(),
    )
    .unwrap();
    assert_eq!(r.status, "violated");
    assert_eq!((r.violations, r.lef_violations), (1, 0));
    assert_eq!(r.safety_margin.fraction, 0.10);
    let m3 = r
        .checks
        .iter()
        .find(|c| c.net == "a" && c.layer == "M3")
        .unwrap();
    assert!(m3.violated && !m3.exceeds_lef_limit);
    assert!((m3.effective_limit - 0.9 * m3.limit).abs() < 1e-9);
    // A margin outside 0 ..= 0.5 is refused.
    for bad in [-0.1, 0.6, f64::NAN] {
        let e = check_antenna(
            &f.workspace,
            &AntennaRequest {
                safety_margin: bad,
                ..request(None)
            },
            &limits(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("safety-margin"), "{e}");
    }
}

#[test]
fn layer_jumpers_repair_partial_net_violations_without_diodes() {
    // A low M2 limit makes the routed M2 runs to the gates violate; the
    // repair cuts each near its gates and bridges it on M3.
    let tech = TECHNOLOGY.replace(
        "ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
        "ANTENNADIFFSIDEAREARATIO 20 ; ANTENNAGATEPLUSDIFF 15 ; END M2",
    );
    let f = routed_with_technology(PLAIN, &tech);
    let head = || {
        fs::read_to_string(f.workspace.join("HEAD"))
            .unwrap()
            .trim()
            .to_string()
    };
    let netlist = |w: &Path| {
        let mut out = Vec::new();
        read(w, &limits())
            .unwrap()
            .export_verilog_with_power(true, &mut out)
            .unwrap();
        out
    };
    let before = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(before.status, "violated");
    assert!(
        before
            .checks
            .iter()
            .filter(|c| c.violated)
            .all(|c| c.layer == "M2")
    );
    let report = f.root.join("before.json");
    fs::write(&report, serde_json::to_vec(&before).unwrap()).unwrap();
    let base = head();
    let verilog = netlist(&f.workspace);
    // A wrong base revision or a stale report is refused, HEAD unchanged.
    assert!(
        repair_antenna(&f.workspace, "rev1", &RepairRequest::default(), &limits())
            .unwrap_err()
            .to_string()
            .contains("REVISION_CONFLICT")
    );
    let stale = f.root.join("stale.json");
    let mut v: serde_json::Value = serde_json::to_value(&before).unwrap();
    v["violations"] = serde_json::json!(0);
    fs::write(&stale, serde_json::to_vec(&v).unwrap()).unwrap();
    let e = repair_antenna(
        &f.workspace,
        &base,
        &RepairRequest {
            report: Some(stale),
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("STALE_REPORT"), "{e}");
    assert_eq!(head(), base);
    let r = repair_antenna(
        &f.workspace,
        &base,
        &RepairRequest {
            report: Some(report),
            ..RepairRequest::default()
        },
        &limits(),
    )
    .unwrap();
    assert_eq!(r.before.violations, before.violations);
    assert!(!r.jumpers.is_empty(), "{:?}", r.unrepaired);
    assert!(
        r.jumpers
            .iter()
            .all(|j| j.layer == "M2" && j.upper == "M3" && j.ratio_after < j.ratio_before)
    );
    assert_eq!((r.status, r.after.violations), ("repaired", 0));
    assert!(r.unrepaired.is_empty());
    assert_ne!(r.revision, base);
    assert_eq!(head(), r.revision);
    assert_eq!(
        r.stats.vias_after,
        r.stats.vias_before + 2 * r.jumpers.len()
    );
    assert_eq!(
        r.stats.wire_length_dbu_after,
        r.stats.wire_length_dbu_before
    );
    // The committed revision re-validates; the check agrees with the report;
    // the netlist (no diode) is unchanged.
    let after = check_antenna(&f.workspace, &request(None), &limits()).unwrap();
    assert_eq!(after.violations, r.after.violations);
    assert_eq!(after.workspace_revision, r.revision);
    assert_eq!(netlist(&f.workspace), verilog);
    // A second repair of a repaired layout changes nothing further or
    // reports what is left.
    let again = repair_antenna(
        &f.workspace,
        &r.revision,
        &RepairRequest::default(),
        &limits(),
    )
    .unwrap();
    assert_eq!(again.before.violations, r.after.violations);
    if r.after.violations == 0 {
        assert_eq!(
            (again.status, again.revision.as_str()),
            ("clean", r.revision.as_str())
        );
    }
}
