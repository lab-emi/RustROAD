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
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
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
        },
        tie_low: TieCell {
            cell: "LO".into(),
            pin: "Y".into(),
        },
        filler_cells: vec!["FILL".into()],
        alternating_rows: true,
        io_width: None,
        io_spacing: None,
        strap_width: None,
        strap_spacing: None,
        max_added_cells: 64,
        max_tie_fanout: 8,
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
