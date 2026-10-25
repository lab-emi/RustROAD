//! Self-authored integration fixture: four 1 x 2 um masters, three routing
//! layers, one cut layer/via, two ten-site rows, and at most 20 instances.
//! Before implementation: source/AST/geometry/maps/exports total < 1 MiB per
//! test; two concurrent test workers stay < 2 MiB incremental fixture memory.
//! No PDK parse, path traversal, SAT problem or unbounded fixture generation.
//! Use repository CARGO_BUILD_JOBS=2/RUST_TEST_THREADS=2 and OS/time guards.
//! This tests workspace integration; real-PDK OpenDB/CEC remain separate gates.

use rustroad_core::connectivity::{ConnectivityLimits, PinOwner, PinUse};
use rustroad_core::{Limits, Orientation};
use rustroad_formats::verilog;
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TapConfig, TieCell};
use rustroad_workspace::{Error, place, read};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const LOGIC: &str = "module chip(input a, output y, output h, output l);\n\
BUF b0 (.A(a), .Y(y)); BUF b1 (.A(1'b1), .Y(h)); BUF b2 (.A(1'b0), .Y(l));\n\
endmodule\n";

const TECHNOLOGY: &str = "VERSION 5.8 ;\n\
UNITS DATABASE MICRONS 1000 ; END UNITS\n\
MANUFACTURINGGRID 0.01 ;\n\
LAYER M1 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M1\n\
LAYER V1 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V1\n\
LAYER M2 TYPE ROUTING ; DIRECTION VERTICAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M2\n\
LAYER M3 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M3\n\
SITE s CLASS CORE ; SYMMETRY X Y ; SIZE 1 BY 2 ; END s\n\
VIA V12 DEFAULT\n\
LAYER M1 ; RECT -0.05 -0.05 0.05 0.05 ;\n\
LAYER V1 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M2 ; RECT -0.05 -0.05 0.05 0.05 ; END V12\n\
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
        // Both rails straddle the cell boundary. FS therefore shares the top
        // power strip of its preceding N row without shorting ground.
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
        io_width: None,
        io_min_area: false,
        io_spacing: None,
        strap_width: None,
        strap_spacing: None,
        max_added_cells: 64,
        max_tie_fanout: 8,
        tap: None,
        clock: None,
        placer: None,
        io_plan: None,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    sources: ImportSources,
    config_path: PathBuf,
}
impl Fixture {
    fn new(logic: &str, configuration: &PhysicalConfig) -> Self {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/physical-tests");
        fs::create_dir_all(&parent).unwrap();
        let mut root = None;
        for _ in 0..128 {
            let candidate = parent.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("fixture directory: {error}"),
            }
        }
        let root = root.expect("bounded fixture directory attempts exhausted");
        let sources = ImportSources {
            technology_lef: root.join("technology.lef"),
            cell_lef: root.join("cells.lef"),
            netlist: root.join("mapped.v"),
            floorplan: root.join("floorplan.json"),
        };
        fs::write(&sources.technology_lef, TECHNOLOGY).unwrap();
        fs::write(&sources.cell_lef, library()).unwrap();
        fs::write(&sources.netlist, logic).unwrap();
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
        fs::write(&config_path, serde_json::to_vec(configuration).unwrap()).unwrap();
        Self {
            workspace: root.join("workspace"),
            root,
            sources,
            config_path,
        }
    }
    fn import(&self) -> rustroad_workspace::Result<rustroad_workspace::Summary> {
        import_pdk_with_config(
            &self.workspace,
            "rev0",
            &self.sources,
            &limits(),
            &ConnectivityLimits::default(),
            Some(&self.config_path),
        )
    }
    fn head(&self) -> String {
        fs::read_to_string(self.workspace.join("HEAD")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn tapped_fixture() -> Fixture {
    let mut cfg = config();
    cfg.tap = Some(TapConfig {
        cell: "TAP".into(),
        max_gap: 3000,
        well_layer: "NW".into(),
        substrate_layer: "PW".into(),
    });
    let f = Fixture::new(LOGIC, &cfg);
    fs::write(
        &f.sources.technology_lef,
        TECHNOLOGY.replace(
            "END LIBRARY",
            "LAYER NW TYPE MASTERSLICE ; END NW\nLAYER PW TYPE MASTERSLICE ; END PW\nEND LIBRARY",
        ),
    )
    .unwrap();
    let tap = "MACRO TAP CLASS CORE WELLTAP ; ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\nPIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER NW ; RECT 0 1 1 2.1 ; LAYER M1 ; RECT 0 1.9 1 2.1 ; END END VDD\nPIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER PW ; RECT 0 -0.1 1 1 ; LAYER M1 ; RECT 0 -0.1 1 0.1 ; END END VSS\nEND TAP\nEND LIBRARY";
    fs::write(&f.sources.cell_lef, library().replace("END LIBRARY", tap)).unwrap();
    f
}

fn clock_fixture(max_buffers: usize) -> Fixture {
    let mut c = config();
    c.clock = Some(rustroad_workspace::clock::ClockConfig {
        port: "a".into(),
        cell: "BUF".into(),
        input_pin: "A".into(),
        output_pin: "Y".into(),
        sink_pin: "A".into(),
        max_fanout: 2,
        max_buffers,
        max_work: 1000,
        level_cells: Vec::new(),
    });
    Fixture::new(
        "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule",
        &c,
    )
}

#[test]
fn bounded_clock_tree_is_level_balanced_and_persistent() {
    let f = clock_fixture(10);
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    let done = rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits()).unwrap();
    assert_eq!(done.clock_buffers, Some(3));
    assert_eq!(done.clock_topology_status, "unrouted_topology_checked");
    let s = read(&f.workspace, &limits()).unwrap();
    let report = s
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .clock_tree
        .as_ref()
        .unwrap();
    assert_eq!(report.levels, 2);
    assert_eq!(report.sinks.len(), 3);
    let before = s.records().collect::<Vec<_>>();
    let mut definition = Vec::new();
    s.export_def(&mut definition).unwrap();
    drop(s);
    assert!(
        rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits())
            .unwrap_err()
            .to_string()
            .contains("REVISION_CONFLICT")
    );
    let repeated = place(&f.workspace, &done.revision, &limits()).unwrap();
    let s = read(&f.workspace, &limits()).unwrap();
    assert_eq!(before, s.records().collect::<Vec<_>>());
    let mut after = Vec::new();
    s.export_def(&mut after).unwrap();
    assert_eq!(definition, after);
    drop(s);
    assert!(
        rustroad_workspace::clock::synthesize(&f.workspace, &repeated.revision, &limits()).is_err()
    );
    assert_eq!(f.head(), format!("{}\n", repeated.revision));
}

#[test]
fn clock_level_masters_follow_the_policy() {
    let buf2 = "MACRO BUF2\nCLASS CORE ; ORIGIN 0 0 ; SIZE 2 BY 2 ; SITE s ; SYMMETRY X Y ;\n\
PIN A DIRECTION INPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.3 0.4 0.5 0.6 ; END END A\n\
PIN Y DIRECTION OUTPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 1.5 1.3 1.7 1.5 ; END END Y\n\
PIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER M1 ; RECT 0 -0.1 2 0.1 ; END END VSS\n\
PIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER M1 ; RECT 0 1.9 2 2.1 ; END END VDD\nEND BUF2\nEND LIBRARY\n";
    let logic = "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule";
    for (levels, expect_ok) in [
        (vec!["BUF2".to_string(), "BUF".to_string()], true),
        (vec!["BUF2".to_string()], true),
        (vec!["FILL".to_string()], false),
    ] {
        let mut c = config();
        c.clock = Some(rustroad_workspace::clock::ClockConfig {
            port: "a".into(),
            cell: "BUF".into(),
            input_pin: "A".into(),
            output_pin: "Y".into(),
            sink_pin: "A".into(),
            max_fanout: 2,
            max_buffers: 10,
            max_work: 1000,
            level_cells: levels.clone(),
        });
        let f = Fixture::new(logic, &c);
        fs::write(
            &f.sources.cell_lef,
            library().replace("END LIBRARY\n", buf2),
        )
        .unwrap();
        f.import().unwrap();
        place(&f.workspace, "rev1", &limits()).unwrap();
        let done = rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits());
        if !expect_ok {
            assert!(done.is_err(), "{levels:?}");
            assert_eq!(f.head(), "rev2\n");
            continue;
        }
        done.unwrap();
        let s = read(&f.workspace, &limits()).unwrap();
        let bundle = s.metadata().physical.as_ref().unwrap();
        let report = bundle.clock_tree.as_ref().unwrap();
        assert_eq!(report.levels, 2);
        let cell = |name: &str| {
            bundle
                .netlist
                .instances
                .iter()
                .find(|i| i.name == name)
                .unwrap()
                .cell
                .clone()
        };
        // rr_clkbuf_0/1 drive the sinks, rr_clkbuf_2 is the root.
        assert_eq!(cell("rr_clkbuf_0"), "BUF2");
        assert_eq!(cell("rr_clkbuf_1"), "BUF2");
        let root = if levels.len() == 1 { "BUF2" } else { "BUF" };
        assert_eq!(cell("rr_clkbuf_2"), root);
        let width = s
            .metadata()
            .design
            .cells
            .iter()
            .find(|c| c.name == "rr_clkbuf_0")
            .unwrap()
            .width;
        assert_eq!(width, 2000);
    }
}

#[test]
fn antenna_diode_stays_a_passive_load_on_the_clock_root() {
    let diode = "MACRO DIO\nCLASS CORE ANTENNACELL ; ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\n\
PIN D DIRECTION INPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.3 0.4 0.5 0.6 ; END END D\n\
PIN VSS DIRECTION INOUT ; USE GROUND ; PORT LAYER M1 ; RECT 0 -0.1 1 0.1 ; END END VSS\n\
PIN VDD DIRECTION INOUT ; USE POWER ; PORT LAYER M1 ; RECT 0 1.9 1 2.1 ; END END VDD\nEND DIO\nEND LIBRARY\n";
    let mut c = config();
    c.clock = Some(rustroad_workspace::clock::ClockConfig {
        port: "a".into(),
        cell: "BUF".into(),
        input_pin: "A".into(),
        output_pin: "Y".into(),
        sink_pin: "A".into(),
        max_fanout: 2,
        max_buffers: 10,
        max_work: 1000,
        level_cells: Vec::new(),
    });
    let f = Fixture::new(
        "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); DIO d0(.D(a)); endmodule",
        &c,
    );
    fs::write(
        &f.sources.cell_lef,
        library().replace("END LIBRARY\n", diode),
    )
    .unwrap();
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits()).unwrap();
    let s = read(&f.workspace, &limits()).unwrap();
    let bundle = s.metadata().physical.as_ref().unwrap();
    let report = bundle.clock_tree.as_ref().unwrap();
    assert_eq!(report.sinks.len(), 3);
    let d0 = bundle
        .netlist
        .instances
        .iter()
        .find(|i| i.name == "d0")
        .unwrap();
    assert_eq!(d0.connections[0].signal, verilog::Expr::Ident("a".into()));
}

#[test]
fn clock_budget_failure_keeps_previous_revision() {
    let f = clock_fixture(1);
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    assert!(rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits()).is_err());
    assert_eq!(f.head(), "rev2\n");
    assert!(read(&f.workspace, &limits()).is_ok());
}

#[test]
fn constrained_clock_and_routed_snapshot_are_transactional() {
    let f = clock_fixture(10);
    let technology = TECHNOLOGY.replace("END LIBRARY", "LAYER V2 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V2\nVIA V23 DEFAULT\nLAYER M2 ; RECT -0.05 -0.05 0.05 0.05 ;\nLAYER V2 ; RECT -0.03 -0.03 0.03 0.03 ;\nLAYER M3 ; RECT -0.05 -0.05 0.05 0.05 ; END V23\nEND LIBRARY");
    fs::write(&f.sources.technology_lef, technology).unwrap();
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    let sdc = f.root.join("clock.sdc");
    fs::write(&sdc, "create_generated_clock -name unsupported").unwrap();
    assert!(
        rustroad_workspace::clock::synthesize_with_sdc(&f.workspace, "rev2", &limits(), Some(&sdc))
            .is_err()
    );
    assert_eq!(f.head(), "rev2\n");
    fs::write(&sdc,"create_clock -name core -period 10 [get_ports a]\nset_clock_transition 0.1 [get_clocks core]\nset_max_fanout 2 [current_design]\n").unwrap();
    let clk =
        rustroad_workspace::clock::synthesize_with_sdc(&f.workspace, "rev2", &limits(), Some(&sdc))
            .unwrap();
    assert_eq!(clk.clock_constraint_status, "explicit_sdc");
    let config = f.root.join("routing.json");
    let mut value = serde_json::json!({"grid_step":400,"access_layer":"M2","layers":[{"name":"M1","spacing":200},{"name":"M2","spacing":200},{"name":"M3","spacing":200}],"vias":[{"name":"V12","cut_spacing":60},{"name":"V23","cut_spacing":60}],"max_skew_ns":1.0});
    fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
    let routed = rustroad_workspace::routing::route(
        &f.workspace,
        &clk.revision,
        &config,
        rustroad_workspace::routing::RouteScope::All,
        &limits(),
    )
    .unwrap();
    assert_eq!(routed.routed_nets, Some(7));
    let snapshot = read(&f.workspace, &limits()).unwrap();
    assert_eq!(snapshot.metadata().format_version, 5);
    let mut def = Vec::new();
    snapshot.export_def(&mut def).unwrap();
    assert!(String::from_utf8(def).unwrap().contains("\n  NEW M"));
    drop(snapshot);
    value["limits"] = serde_json::to_value(rustroad_core::routing::RouteLimits {
        max_grid_nodes: 1,
        ..Default::default()
    })
    .unwrap();
    fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        rustroad_workspace::routing::route(
            &f.workspace,
            &routed.revision,
            &config,
            rustroad_workspace::routing::RouteScope::All,
            &limits()
        )
        .is_err()
    );
    assert_eq!(f.head(), format!("{}\n", routed.revision));
    let table = f
        .workspace
        .join("revisions")
        .join(&routed.revision)
        .join("routing.rbin");
    let original = fs::read(&table).unwrap();
    let mut corrupt = original.clone();
    corrupt[0] ^= 1;
    fs::write(&table, &corrupt).unwrap();
    assert!(read(&f.workspace, &limits()).is_err());
    fs::write(&table, &original).unwrap();
    let p = place(&f.workspace, &routed.revision, &limits()).unwrap();
    assert_eq!(p.routed_nets, None);
    assert_eq!(
        read(&f.workspace, &limits())
            .unwrap()
            .metadata()
            .format_version,
        4
    );
}

#[test]
fn clock_port_alias_fails_explicitly_without_publication() {
    let f = clock_fixture(10);
    fs::write(
        &f.sources.netlist,
        "module chip(input a, output y, output h, output l, output forwarded); assign forwarded=a; BUF b0(.A(a),.Y(y)); BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule",
    )
    .unwrap();
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    let error = rustroad_workspace::clock::synthesize(&f.workspace, "rev2", &limits()).unwrap_err();
    assert!(error.to_string().contains("aliases another top-level port"));
    assert_eq!(f.head(), "rev2\n");
}

#[test]
fn tap_reservations_are_persistent_and_logically_invisible() {
    let f = tapped_fixture();
    f.import().unwrap();
    let first = place(&f.workspace, "rev1", &limits()).unwrap();
    assert_eq!(first.tap_cells, Some(8));
    let s = read(&f.workspace, &limits()).unwrap();
    assert_eq!(s.metadata().format_version, 4);
    assert_eq!(
        s.metadata()
            .physical
            .as_ref()
            .unwrap()
            .state
            .as_ref()
            .unwrap()
            .tap
            .as_ref()
            .unwrap()
            .max_clear_gap,
        2000
    );
    let before = s.records().collect::<Vec<_>>();
    let mut v = Vec::new();
    s.export_verilog(&mut v).unwrap();
    assert!(!String::from_utf8(v).unwrap().contains("TAP "));
    let mut def = Vec::new();
    s.export_def(&mut def).unwrap();
    drop(s);
    let second = place(&f.workspace, &first.revision, &limits()).unwrap();
    assert_eq!(second.tap_cells, Some(8));
    let s = read(&f.workspace, &limits()).unwrap();
    assert_eq!(before, s.records().collect::<Vec<_>>());
    let mut after = Vec::new();
    s.export_def(&mut after).unwrap();
    assert_eq!(def, after);
}

#[test]
fn tap_identity_and_spacing_fail_without_publishing() {
    let f = tapped_fixture();
    let mut cfg: PhysicalConfig =
        serde_json::from_slice(&fs::read(&f.config_path).unwrap()).unwrap();
    cfg.tap.as_mut().unwrap().cell = "FILL".into();
    fs::write(&f.config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    assert!(f.import().unwrap_err().to_string().contains("WELLTAP"));
    assert!(!f.workspace.join("HEAD").exists());
    cfg.tap.as_mut().unwrap().cell = "TAP".into();
    cfg.tap.as_mut().unwrap().max_gap = 0;
    fs::write(&f.config_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    f.import().unwrap();
    assert!(place(&f.workspace, "rev1", &limits()).is_err());
    assert_eq!(f.head(), "rev1\n");
}

#[test]
fn lost_tap_metadata_is_rejected_on_read() {
    let f = tapped_fixture();
    f.import().unwrap();
    let p = place(&f.workspace, "rev1", &limits()).unwrap();
    let file = f
        .workspace
        .join("revisions")
        .join(p.revision)
        .join("design.json");
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    value["physical"]["generated_taps"]
        .as_array_mut()
        .unwrap()
        .pop();
    fs::write(file, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(read(&f.workspace, &limits()).is_err());
}

#[test]
fn preparation_roundtrip_covers_ties_fillers_orientations_io_and_supplies() {
    let f = Fixture::new(LOGIC, &config());
    assert_eq!(f.import().unwrap().revision, "rev1");
    let summary = place(&f.workspace, "rev1", &limits()).unwrap();
    assert_eq!(summary.revision, "rev2");
    assert_eq!(summary.cells, 20);
    assert_eq!(summary.inserted_tie_cells, Some(2));
    assert_eq!(summary.filler_cells, Some(15));
    assert_eq!(summary.drc_status, "unchecked");
    let snapshot = read(&f.workspace, &limits()).unwrap();
    let metadata = snapshot.metadata();
    assert_eq!(metadata.format_version, 3);
    assert_eq!(metadata.design.rows[0].orientation, Orientation::N);
    assert_eq!(metadata.design.rows[1].orientation, Orientation::FS);
    let bundle = metadata.physical.as_ref().unwrap();
    let state = bundle.state.as_ref().unwrap();
    assert_eq!(state.rails.status, "pass");
    assert_eq!(state.rails.occupied_rows, 2);
    assert_eq!(state.rails.shared_boundaries_checked, 1);
    assert_eq!(state.fillers_added, 15);
    assert_eq!(state.io_pins.len(), 4);
    assert_eq!(state.supply.ports.len(), 2);
    assert!(!state.supply.vias.is_empty());
    assert_eq!(state.supply.drc_status, "unchecked");
    assert_eq!(
        bundle
            .ties
            .as_ref()
            .unwrap()
            .bindings
            .iter()
            .filter(|b| b.value)
            .count(),
        1
    );
    assert_eq!(
        bundle
            .ties
            .as_ref()
            .unwrap()
            .bindings
            .iter()
            .filter(|b| !b.value)
            .count(),
        1
    );
    for pin in &state.io_pins {
        let r = pin.rect;
        assert!(r.lx >= 0 && r.ly >= 0 && r.ux <= 20000 && r.uy <= 20000);
        assert!(r.ux <= 5000 || r.lx >= 15000 || r.uy <= 5000 || r.ly >= 9000);
        assert!([r.lx, r.ly, r.ux, r.uy].iter().all(|c| c % 10 == 0));
    }
    let model = &bundle.connectivity;
    let mut supply_endpoints = 0usize;
    for pin in &model.pins {
        if let PinOwner::Instance { instance, terminal } = pin.owner {
            let spec = &model.masters[model.instance_masters[instance as usize] as usize].pins
                [terminal as usize];
            if spec.use_.is_supply() {
                assert_eq!(model.nets[pin.net.unwrap() as usize].use_, spec.use_);
                supply_endpoints += 1;
            }
        }
    }
    assert_eq!(supply_endpoints, 40);
    assert_eq!(
        model
            .nets
            .iter()
            .filter(|n| n.use_ == PinUse::Power)
            .count(),
        1
    );
    assert_eq!(
        model
            .nets
            .iter()
            .filter(|n| n.use_ == PinUse::Ground)
            .count(),
        1
    );
    let mut def = Vec::new();
    snapshot.export_def(&mut def).unwrap();
    let def = String::from_utf8(def).unwrap();
    assert!(def.contains("ROW row1 s 5000 7000 FS DO 10 BY 1 STEP 1000 0 ;"));
    assert!(def.contains("SPECIALNETS 2 ;"));
    assert!(def.contains("+ USE POWER"));
    assert!(def.contains("+ USE GROUND"));
    assert!(def.contains("+ FIXED"));
    let mut powered = Vec::new();
    snapshot
        .export_verilog_with_power(true, &mut powered)
        .unwrap();
    let powered = verilog::parse_verilog(
        std::str::from_utf8(&powered).unwrap(),
        &verilog::VerilogLimits::default(),
    )
    .unwrap();
    assert_eq!(powered.instances.len(), 20);
    assert_eq!(
        powered
            .ports
            .iter()
            .filter(|p| p.direction == verilog::Direction::Inout)
            .count(),
        2
    );
    assert!(powered.instances.iter().all(|i| {
        ["VDD", "VSS"]
            .iter()
            .all(|p| i.connections.iter().any(|c| c.pin == *p))
    }));
    let mut logical = Vec::new();
    snapshot
        .export_verilog_with_power(false, &mut logical)
        .unwrap();
    let logical = verilog::parse_verilog(
        std::str::from_utf8(&logical).unwrap(),
        &verilog::VerilogLimits::default(),
    )
    .unwrap();
    assert_eq!(logical.instances.len(), 5);
    assert!(
        logical
            .ports
            .iter()
            .all(|p| p.direction != verilog::Direction::Inout)
    );
    let records = snapshot.records().collect::<Vec<_>>();
    let physical = serde_json::to_value(state).unwrap();
    drop(snapshot);
    let second = place(&f.workspace, "rev2", &limits()).unwrap();
    assert_eq!(second.revision, "rev3");
    assert_eq!(
        (second.cells, second.inserted_tie_cells, second.filler_cells),
        (20, Some(2), Some(15))
    );
    let snapshot = read(&f.workspace, &limits()).unwrap();
    assert_eq!(snapshot.records().collect::<Vec<_>>(), records);
    assert_eq!(
        serde_json::to_value(
            snapshot
                .metadata()
                .physical
                .as_ref()
                .unwrap()
                .state
                .as_ref()
                .unwrap()
        )
        .unwrap(),
        physical
    );
    let mut again = Vec::new();
    snapshot.export_def(&mut again).unwrap();
    assert_eq!(String::from_utf8(again).unwrap(), def);
    drop(snapshot);
    let before = f.head();
    assert!(matches!(
        place(&f.workspace, "rev2", &limits()),
        Err(Error::Conflict { .. })
    ));
    assert_eq!(f.head(), before);
}

#[test]
fn physical_side_table_corruption_is_detected_without_live_mappings() {
    let f = Fixture::new(LOGIC, &config());
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    let snapshot = read(&f.workspace, &limits()).unwrap();
    drop(snapshot);
    let table = f.workspace.join("revisions/rev2/physical.rbin");
    let mut bytes = fs::read(&table).unwrap();
    assert!(!bytes.is_empty());
    bytes[0] ^= 1;
    fs::write(&table, bytes).unwrap();
    assert!(matches!(
        read(&f.workspace, &limits()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn invalid_config_fanout_and_added_cell_budgets_do_not_publish() {
    let mut bad = config();
    bad.ground_net = bad.power_net.clone();
    let f = Fixture::new(LOGIC, &bad);
    assert!(f.import().is_err());
    assert!(!f.workspace.join("HEAD").exists());
    let mut limited = config();
    limited.max_added_cells = 0;
    let f = Fixture::new(LOGIC, &limited);
    let error = f.import().unwrap_err();
    assert!(error.to_string().contains("TIE_BUDGET"));
    assert!(!f.workspace.join("HEAD").exists());
    let mut low_fanout = config();
    low_fanout.max_tie_fanout = 1;
    let logic = "module chip(input a,output y,output h,output l); wire k; assign k=1'b1; BUF b0(.A(k),.Y(y)); BUF b1(.A(k),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule";
    let f = Fixture::new(logic, &low_fanout);
    let error = f.import().unwrap_err();
    assert!(error.to_string().contains("fanout"));
    assert!(!f.workspace.join("HEAD").exists());
}

#[test]
fn explicit_spacing_does_not_require_a_scalar_lef_spacing() {
    let mut cfg = config();
    cfg.io_spacing = Some(200);
    cfg.strap_spacing = Some(200);
    let f = Fixture::new(LOGIC, &cfg);
    // A retained layer can lack a simple SPACING scalar (for example when its
    // rules are represented by a more complex table). Explicit caller values
    // must be sufficient; this does not claim that those complex rules pass.
    let technology = TECHNOLOGY
        .replace("WIDTH 0.2 ; SPACING 0.2 ; END M2", "WIDTH 0.2 ; END M2")
        .replace("WIDTH 0.2 ; SPACING 0.2 ; END M3", "WIDTH 0.2 ; END M3");
    fs::write(&f.sources.technology_lef, technology).unwrap();
    f.import().unwrap();
    place(&f.workspace, "rev1", &limits()).unwrap();
    let snapshot = read(&f.workspace, &limits()).unwrap();
    assert!(
        snapshot
            .metadata()
            .physical
            .as_ref()
            .unwrap()
            .state
            .is_some()
    );
}

#[test]
fn flipped_row_requires_master_x_symmetry_before_publication() {
    let f = Fixture::new(LOGIC, &config());
    fs::write(
        &f.sources.cell_lef,
        library().replace("SYMMETRY X Y", "SYMMETRY Y"),
    )
    .unwrap();
    f.import().unwrap();
    let before = f.head();
    let error = place(&f.workspace, "rev1", &limits()).unwrap_err();
    assert!(error.to_string().contains("SYMMETRY X"));
    assert_eq!(f.head(), before);
    let snapshot = read(&f.workspace, &limits()).unwrap();
    assert_eq!(
        snapshot.metadata().stage,
        rustroad_workspace::Stage::Imported
    );
}

#[test]
fn io_and_supply_layers_must_remain_distinct() {
    let mut cfg = config();
    cfg.io_layer = cfg.strap_layer.clone();
    let f = Fixture::new(LOGIC, &cfg);
    assert!(f.import().is_err());
    assert!(!f.workspace.join("HEAD").exists());
}

#[test]
fn configured_snapshot_requires_its_tie_report() {
    let f = Fixture::new(LOGIC, &config());
    f.import().unwrap();
    let before = f.head();
    let path = f.workspace.join("revisions/rev1/design.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    metadata["physical"]["ties"] = serde_json::Value::Null;
    // No active reader/mapping exists while the corruption control is planted.
    fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    assert!(matches!(
        read(&f.workspace, &limits()),
        Err(Error::Invalid(_))
    ));
    assert_eq!(f.head(), before);
}

#[test]
fn conflicting_fixed_via_definitions_do_not_publish() {
    let f = Fixture::new(LOGIC, &config());
    let conflicting_via = "VIA V12 DEFAULT\n\
LAYER M1 ; RECT -0.06 -0.05 0.06 0.05 ;\n\
LAYER V1 ; RECT -0.03 -0.03 0.03 0.03 ;\n\
LAYER M2 ; RECT -0.05 -0.05 0.05 0.05 ; END V12\n\
END LIBRARY\n";
    fs::write(
        &f.sources.cell_lef,
        library().replace("END LIBRARY\n", conflicting_via),
    )
    .unwrap();
    let error = f.import().unwrap_err();
    assert!(error.to_string().contains("conflicting fixed VIA"));
    assert!(!f.workspace.join("HEAD").exists());
}
