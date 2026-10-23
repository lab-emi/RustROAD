//! Upfront resource estimate: <1 KiB self-authored input per case, two instances,
//! three nets, six connected pins, two rectangles, one corner. Model entries and
//! short-name tables are a few KiB/test; no paths, cuts, clauses or unbounded graph
//! search. Bounded serialization/write buffers add <=1 MiB transient storage.
//! Two test workers and two Rayon threads; expected test RSS <64 MiB. Run under
//! an 8 GiB VM guard/180 s deadline. All fixtures live in repository `.work`.

use rustroad_core::Limits;
use rustroad_core::connectivity::{ConnectivityLimits, PinOwner};
use rustroad_formats::verilog::{VerilogLimits, parse_verilog};
use rustroad_workspace::pdk::{ImportSources, import_pdk};
use rustroad_workspace::{Error, place, read};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const TECH: &str = "VERSION 5.8 ; UNITS DATABASE MICRONS 1000 ; END UNITS\n\
LAYER M1 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.1 ; WIDTH 0.05 ; END M1\n\
SITE site1 CLASS CORE ; SYMMETRY X Y ; SIZE 0.5 BY 1 ; END site1\n\
MANUFACTURINGGRID 0.001 ; END LIBRARY\n";
const CELLS: &str = "VERSION 5.8 ; MACRO BUF CLASS CORE ; ORIGIN 0 0 ;\n\
SIZE 1 BY 1 ; SITE site1 ; SYMMETRY X Y ;\n\
PIN A DIRECTION INPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.1 0.1 0.2 0.2 ; END END A\n\
PIN Z DIRECTION OUTPUT ; USE SIGNAL ; PORT LAYER M1 ; RECT 0.8 0.1 0.9 0.2 ; END END Z\n\
END BUF END LIBRARY\n";
const NETLIST: &str = "module demo(input a, output z); wire mid;\n\
BUF u0 (.A(a), .Z(mid)); BUF u1 (.A(mid), .Z(z)); endmodule\n";
const FLOORPLAN: &str = r#"{
  "name":"demo","units_per_micron":1000,
  "die_area":{"lx":0,"ly":0,"ux":4000,"uy":2000},
  "core_area":{"lx":0,"ly":0,"ux":4000,"uy":2000},
  "site":{"name":"site1","width":500,"height":1000},
  "rows":[{"x_start":0,"x_end":4000,"y":0}]
}"#;

fn placement_limits() -> Limits {
    Limits {
        max_cells: 2,
        max_rows: 1,
        max_expansions: 3,
    }
}
fn connectivity_limits() -> ConnectivityLimits {
    ConnectivityLimits {
        max_instances: 2,
        max_nets: 8,
        max_pins: 16,
        max_aliases: 16,
        max_shapes: 8,
        max_vertices: 16,
    }
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    sources: ImportSources,
}
impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/pdk-import-tests")
            .join(format!(
                "{}-{stamp}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        fs::create_dir(&root).unwrap();
        let sources = ImportSources {
            technology_lef: root.join("tech.lef"),
            cell_lef: root.join("cells.lef"),
            netlist: root.join("mapped.v"),
            floorplan: root.join("floorplan.json"),
        };
        fs::write(&sources.technology_lef, TECH).unwrap();
        fs::write(&sources.cell_lef, CELLS).unwrap();
        fs::write(&sources.netlist, NETLIST).unwrap();
        fs::write(&sources.floorplan, FLOORPLAN).unwrap();
        Self {
            workspace: root.join("workspace"),
            root,
            sources,
        }
    }
    fn import(&self) {
        let summary = import_pdk(
            &self.workspace,
            "rev0",
            &self.sources,
            &placement_limits(),
            &connectivity_limits(),
        )
        .unwrap();
        assert_eq!(summary.revision, "rev1");
    }
    fn assert_rejected_without_publication(&self, expected: &str) {
        let original_head = fs::read(self.workspace.join("HEAD")).unwrap();
        let error = import_pdk(
            &self.workspace,
            "rev1",
            &self.sources,
            &placement_limits(),
            &connectivity_limits(),
        )
        .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        self.assert_same_head(&original_head);
    }
    fn assert_same_head(&self, original: &[u8]) {
        assert_eq!(fs::read(self.workspace.join("HEAD")).unwrap(), original);
        let snapshot = read(&self.workspace, &placement_limits()).unwrap();
        assert_eq!(snapshot.summary().revision, "rev1");
        assert_eq!(snapshot.connectivity().unwrap().nets.len(), 3);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn import_place_and_export_preserve_mapped_connections() {
    let fixture = Fixture::new();
    fixture.import();
    assert_eq!(
        place(&fixture.workspace, "rev1", &placement_limits())
            .unwrap()
            .revision,
        "rev2"
    );
    let snapshot = read(&fixture.workspace, &placement_limits()).unwrap();
    assert_eq!(snapshot.metadata().format_version, 2);
    let physical = snapshot.metadata().physical.as_ref().unwrap();
    assert_eq!(physical.sources.len(), 4);
    assert!(physical.sources.iter().all(|s| s.sha256.len() == 64));
    let model = snapshot.connectivity().unwrap();
    assert_eq!(
        (model.masters.len(), model.nets.len(), model.pins.len()),
        (1, 3, 6)
    );
    assert_eq!(model.masters[0].size, [1000, 1000]);
    let net_for = |instance, terminal| {
        let pin = model
            .pins
            .iter()
            .find(|pin| pin.owner == PinOwner::Instance { instance, terminal })
            .unwrap();
        model.nets[pin.net.unwrap() as usize].name.as_str()
    };
    assert_eq!(net_for(0, 0), "a");
    assert_eq!(net_for(0, 1), "mid");
    assert_eq!(net_for(1, 0), "mid");
    assert_eq!(net_for(1, 1), "z");

    let mut def = Vec::new();
    snapshot.export_def(&mut def).unwrap();
    let def = String::from_utf8(def).unwrap();
    assert!(def.contains("COMPONENTS 2 ;"));
    assert!(def.contains("PINS 2 ;"));
    assert!(def.contains("NETS 3 ;"));
    assert!(def.contains("- mid ( u0 Z ) ( u1 A ) + USE SIGNAL ;"));
    assert!(def.contains("+ PLACED ( 1000 0 ) N ;"));
    assert!(!def.contains("+ FIXED"));

    let mut verilog = Vec::new();
    snapshot.export_verilog(&mut verilog).unwrap();
    let exported = parse_verilog(
        std::str::from_utf8(&verilog).unwrap(),
        &VerilogLimits::default(),
    )
    .unwrap();
    let original = parse_verilog(NETLIST, &VerilogLimits::default()).unwrap();
    assert_eq!(exported, original);
    let summary = snapshot.summary();
    assert_eq!(summary.drc_status, "unchecked");
    assert_eq!(summary.power_grid_status, "unchecked");
    assert_eq!(summary.io_placement_status, "not_placed");
}

fn corrupt_one_byte(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    assert!(!bytes.is_empty());
    bytes[0] ^= 0x80;
    fs::write(path, bytes).unwrap();
}

#[test]
fn side_table_corruption_is_rejected_before_export() {
    for (table, diagnostic) in [
        ("pins.rbin", "pin side table"),
        ("net_offsets.rbin", "net offsets"),
        ("net_pins.rbin", "net CSR"),
    ] {
        let fixture = Fixture::new();
        fixture.import();
        // No snapshot/mmap is alive while simulating offline corruption.
        corrupt_one_byte(&fixture.workspace.join("revisions/rev1").join(table));
        match read(&fixture.workspace, &placement_limits()) {
            Err(Error::Invalid(message)) => {
                assert!(message.contains(diagnostic), "{table}: {message}")
            }
            Err(other) => panic!("{table}: expected invalid snapshot, got {other}"),
            Ok(_) => panic!("{table}: corrupted side table accepted"),
        }
    }
}

#[test]
fn invalid_pin_master_driver_and_width_leave_head_unchanged() {
    let cases = [
        (
            NETLIST.replace(".Z(mid)", ".Z(mid), .BAD(a)"),
            "unknown LEF pin",
        ),
        (
            NETLIST.replacen("BUF u0", "UNKNOWN u0", 1),
            "missing LEF master",
        ),
        (
            "module demo(input a, output z); BUF u0(.A(a),.Z(z)); BUF u1(.A(a),.Z(z)); endmodule"
                .into(),
            "2 drivers",
        ),
        (
            NETLIST.replace(".A(a)", ".A({a,a})"),
            "scalar but expression is a vector",
        ),
    ];
    for (bad_netlist, expected) in cases {
        let fixture = Fixture::new();
        fixture.import();
        fs::write(&fixture.sources.netlist, bad_netlist).unwrap();
        fixture.assert_rejected_without_publication(expected);
    }
}

#[test]
fn nested_expressions_and_alias_cycles_survive_persistence() {
    let fixture = Fixture::new();
    let nested = format!("{}a{}", "{".repeat(24), "}".repeat(24));
    let netlist = NETLIST.replace(".A(a)", &format!(".A({nested})")).replace(
        "wire mid;",
        "wire mid; wire alias_net; assign alias_net = mid; assign mid = alias_net;",
    );
    fs::write(&fixture.sources.netlist, &netlist).unwrap();
    // This case intentionally has more expression nodes than connected pins.
    let mut nested_limits = connectivity_limits();
    nested_limits.max_pins = 128;
    import_pdk(
        &fixture.workspace,
        "rev0",
        &fixture.sources,
        &placement_limits(),
        &nested_limits,
    )
    .unwrap();
    let snapshot = read(&fixture.workspace, &placement_limits()).unwrap();
    let mut exported = Vec::new();
    snapshot.export_verilog(&mut exported).unwrap();
    assert_eq!(
        parse_verilog(&netlist, &VerilogLimits::default()).unwrap(),
        parse_verilog(
            std::str::from_utf8(&exported).unwrap(),
            &VerilogLimits::default()
        )
        .unwrap()
    );
    let mut invalid_design = snapshot.metadata().design.clone();
    invalid_design.site.width = 0;
    assert!(
        snapshot
            .connectivity()
            .unwrap()
            .lower_csr(&invalid_design, &connectivity_limits())
            .is_err()
    );
}

#[test]
fn mismatched_logical_ast_and_physical_connections_are_rejected() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.sources.netlist,
        NETLIST.replace("input a,", "input a, input b,"),
    )
    .unwrap();
    fixture.import();
    let metadata_path = fixture.workspace.join("revisions/rev1/design.json");
    let mut metadata: rustroad_workspace::Metadata =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata.physical.as_mut().unwrap().netlist.instances[0].connections[0].signal =
        rustroad_formats::verilog::Expr::Ident("b".into());
    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    match read(&fixture.workspace, &placement_limits()) {
        Err(error) => assert!(
            error
                .to_string()
                .contains("logical netlist and connection tables differ"),
            "{error}"
        ),
        Ok(_) => panic!("inconsistent logical and physical circuits were accepted"),
    }
}

#[test]
fn incompatible_physical_width_and_nonzero_origin_leave_head_unchanged() {
    for (bad_cells, expected) in [
        (
            CELLS.replace("SIZE 1 BY 1", "SIZE 1.1 BY 1"),
            "site-aligned cell",
        ),
        (
            CELLS.replace("ORIGIN 0 0", "ORIGIN 0.001 0"),
            "unsupported nonzero LEF origin",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.import();
        fs::write(&fixture.sources.cell_lef, bad_cells).unwrap();
        fixture.assert_rejected_without_publication(expected);
    }
}

#[test]
fn connectivity_budgets_fail_without_changing_head() {
    let base = connectivity_limits();
    let cases = [
        ConnectivityLimits {
            max_instances: 1,
            ..base.clone()
        },
        ConnectivityLimits {
            max_nets: 2,
            ..base.clone()
        },
        ConnectivityLimits {
            max_pins: 5,
            ..base.clone()
        },
        ConnectivityLimits {
            max_aliases: 2,
            ..base.clone()
        },
        ConnectivityLimits {
            max_shapes: 1,
            ..base.clone()
        },
        ConnectivityLimits {
            max_vertices: 3,
            ..base
        },
    ];
    for limits in cases {
        let fixture = Fixture::new();
        fixture.import();
        let original = fs::read(fixture.workspace.join("HEAD")).unwrap();
        let error = import_pdk(
            &fixture.workspace,
            "rev1",
            &fixture.sources,
            &placement_limits(),
            &limits,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("BUDGET") || message.contains("max_") || message.contains("limit"),
            "{message}"
        );
        fixture.assert_same_head(&original);
    }
}
