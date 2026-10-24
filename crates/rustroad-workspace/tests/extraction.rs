//! Self-authored routed fixture for native RC extraction: three 1 x 2 um
//! buffers on two rows, three routing layers and two fixed vias. Before
//! implementation: < 1 MiB of sources, workspace and SPEF per test, two test
//! workers, no PDK parse. Real-PDK comparisons live in scripts/compare_spef.py.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_core::extract::ExtractLimits;
use rustroad_workspace::extract::{ExtractRequest, extract};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
use rustroad_workspace::{Error, place, read};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const TECHNOLOGY: &str = "VERSION 5.8 ;\n\
UNITS DATABASE MICRONS 1000 ; END UNITS\n\
MANUFACTURINGGRID 0.01 ;\n\
LAYER M1 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M1\n\
LAYER V1 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V1\n\
LAYER M2 TYPE ROUTING ; DIRECTION VERTICAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M2\n\
LAYER V2 TYPE CUT ; WIDTH 0.06 ; SPACING 0.06 ; END V2\n\
LAYER M3 TYPE ROUTING ; DIRECTION HORIZONTAL ; PITCH 0.4 ; WIDTH 0.2 ; SPACING 0.2 ; END M3\n\
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
        clock: Some(rustroad_workspace::clock::ClockConfig {
            port: "a".into(),
            cell: "BUF".into(),
            input_pin: "A".into(),
            output_pin: "Y".into(),
            sink_pin: "A".into(),
            max_fanout: 2,
            max_buffers: 10,
            max_work: 1000,
            level_cells: Vec::new(),
        }),
        placer: None,
        io_plan: None,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const NETLIST: &str = "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); \
     BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule";

/// Import, place, build the clock tree and route every signal net.
fn routed() -> Fixture {
    routed_netlist(NETLIST)
}

fn routed_netlist(netlist: &str) -> Fixture {
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/extraction-tests");
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
    fs::write(&sources.technology_lef, TECHNOLOGY).unwrap();
    fs::write(&sources.cell_lef, library()).unwrap();
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
    rustroad_workspace::routing::route(
        &workspace,
        &clk.revision,
        &routing,
        rustroad_workspace::routing::RouteScope::All,
        &limits(),
    )
    .unwrap();
    Fixture { root, workspace }
}

fn profile(f: &Fixture, corner: &str, tech_sha: &str, skip: Option<&str>) -> PathBuf {
    let rows = |g: f64| {
        serde_json::json!([
            [0.2, 1e-4, 1e-5],
            [0.6, 3e-5, 2e-5 * g],
            [1.2, 0.0, 4e-5 * g]
        ])
    };
    let layer = |name: &str, metal: u32| {
        let mut contexts = vec![serde_json::json!({"over":0,"under":null,"rows":rows(1.0)})];
        for below in 1..metal {
            contexts.push(serde_json::json!({"over":below,"under":null,"rows":rows(1.5)}));
        }
        serde_json::json!({"name":name,"rules_metal":metal,"width_um":0.2,
            "sheet_resistance_ohm_per_square":0.1,"contexts":contexts})
    };
    let layers: Vec<_> = [("M1", 1), ("M2", 2), ("M3", 3)]
        .into_iter()
        .filter(|(n, _)| Some(*n) != skip)
        .map(|(n, m)| layer(n, m))
        .collect();
    let value = serde_json::json!({
        "schema":"rustroad.rc-profile.v1","pdk":"toy","library":"toy","corner":corner,
        "pdk_version":"test","technology_lef_sha256":tech_sha,
        "units":{"length":"um","capacitance":"pF","resistance":"ohm"},
        "magic_variant":"()","lef_parasitics":"none","sources":[],"model":{},
        "layers":layers,
        "cuts":[{"name":"V1","lower":"M1","upper":"M2","resistance_ohm_per_cut":2.0,"source":"test"},
                {"name":"V2","lower":"M2","upper":"M3","resistance_ohm_per_cut":3.0,"source":"test"}],
        "cross_checks":[],"diagnostics":[],"generator":"test"});
    let path = f.root.join(format!(
        "profile-{corner}-{}-{}.json",
        skip.unwrap_or("all"),
        &tech_sha[..8]
    ));
    fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    path
}

fn tree(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.clone(),
                    format!("{:x}", Sha256::digest(fs::read(&p).unwrap())),
                ));
            }
        }
    }
    out.sort();
    out
}

fn request(profile: PathBuf, corner: &str, out: PathBuf, cap: u64) -> ExtractRequest {
    ExtractRequest {
        profile,
        corner: corner.into(),
        out,
        max_spef_bytes: cap,
        limits: ExtractLimits::default(),
    }
}

#[test]
fn extraction_is_read_only_deterministic_and_complete() {
    let f = routed();
    let tech = read(&f.workspace, &limits())
        .unwrap()
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .sources
        .iter()
        .find(|s| s.role == "technology_lef")
        .unwrap()
        .sha256
        .clone();
    let p = profile(&f, "nom", &tech, None);
    let before = tree(&f.workspace);
    let a = f.root.join("a.spef");
    let report = extract(
        &f.workspace,
        &request(p.clone(), "nom", a.clone(), 1 << 20),
        &limits(),
    )
    .unwrap();
    let b = f.root.join("b.spef");
    extract(
        &f.workspace,
        &request(p.clone(), "nom", b.clone(), 1 << 20),
        &limits(),
    )
    .unwrap();
    assert_eq!(
        fs::read(&a).unwrap(),
        fs::read(&b).unwrap(),
        "byte-identical rerun"
    );
    assert_eq!(before, tree(&f.workspace), "workspace unchanged");
    let text = fs::read_to_string(&a).unwrap();
    assert_eq!(
        report.output.sha256,
        format!("{:x}", Sha256::digest(text.as_bytes()))
    );
    assert_eq!(report.output.bytes, text.len() as u64);
    assert_eq!(report.routed_nets, 7);
    assert_eq!(text.matches("*D_NET ").count(), 7);
    assert_eq!(text.matches("*END\n").count(), 7);
    assert!(text.starts_with("*SPEF \"IEEE 1481-1999\"\n"));
    assert!(text.contains("*NAME_MAP\n") && text.contains("*PORTS\n"));
    // Each connection is a routed terminal; each net is a tree.
    let terminals: usize = report.stats.nodes - report.stats.resistors;
    assert_eq!(terminals, 7, "one tree per net");
    assert_eq!(report.stats.opened_loop_resistors, 0);
    assert!(report.stats.wire_length_dbu > 0 && report.stats.ground_pf > 0.0);
    assert_eq!(report.scope.corner, "nom");
    assert!(report.unannotated_signal_nets.is_empty());
    assert!(report.pinless_signal_nets.is_empty());
    // No temporary file is left behind.
    assert!(
        fs::read_dir(&f.root).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-"))
    );
}

#[test]
fn dangling_declared_nets_are_listed_as_pinless_not_unannotated() {
    // RustSys can leave a declared wire without any connection; it has no
    // driver and no load, so nothing is routed or extracted for it.
    let f = routed_netlist(
        "module chip(input a, output y, output h, output l); wire spare; BUF b0(.A(a),.Y(y)); \
         BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule",
    );
    let tech = read(&f.workspace, &limits())
        .unwrap()
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .sources
        .iter()
        .find(|s| s.role == "technology_lef")
        .unwrap()
        .sha256
        .clone();
    let p = profile(&f, "nom", &tech, None);
    let out = f.root.join("spare.spef");
    let report = extract(
        &f.workspace,
        &request(p, "nom", out.clone(), 1 << 20),
        &limits(),
    )
    .unwrap();
    assert!(report.unannotated_signal_nets.is_empty());
    assert_eq!(report.pinless_signal_nets, vec!["spare".to_string()]);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.starts_with("RC_PINLESS_NETS: 1 "))
    );
    assert_eq!(report.routed_nets, 7);
    assert!(!fs::read_to_string(&out).unwrap().contains("spare"));
}

#[test]
fn extraction_refuses_unsafe_or_mismatched_requests_without_output() {
    let f = routed();
    let tech = read(&f.workspace, &limits())
        .unwrap()
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .sources
        .iter()
        .find(|s| s.role == "technology_lef")
        .unwrap()
        .sha256
        .clone();
    let good = profile(&f, "nom", &tech, None);
    let before = tree(&f.workspace);
    let inside = f.workspace.join("x.spef");
    let e = extract(
        &f.workspace,
        &request(good.clone(), "nom", inside.clone(), 1 << 20),
        &limits(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("OUTPUT_IN_WORKSPACE"), "{e}");
    assert!(!inside.exists());
    let out = f.root.join("out.spef");
    let e = extract(
        &f.workspace,
        &request(good.clone(), "max", out.clone(), 1 << 20),
        &limits(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("CORNER_MISMATCH"), "{e}");
    let wrong = profile(&f, "nom", &"0".repeat(64), None);
    let e = extract(
        &f.workspace,
        &request(wrong, "nom", out.clone(), 1 << 20),
        &limits(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("technology LEF"), "{e}");
    let partial = profile(&f, "nom", &tech, Some("M3"));
    let e = extract(
        &f.workspace,
        &request(partial, "nom", out.clone(), 1 << 20),
        &limits(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("absent from profile"), "{e}");
    let e = extract(
        &f.workspace,
        &request(good.clone(), "nom", out.clone(), 64),
        &limits(),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Budget(_)), "{e}");
    let e = extract(
        &f.workspace,
        &request(good, "nom", out.clone(), 0),
        &limits(),
    )
    .unwrap_err();
    assert!(matches!(e, Error::Budget(_)), "{e}");
    assert!(!out.exists());
    assert!(
        fs::read_dir(&f.root).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-"))
    );
    assert_eq!(before, tree(&f.workspace));
}
