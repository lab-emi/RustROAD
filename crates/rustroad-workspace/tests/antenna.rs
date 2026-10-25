//! Self-authored routed fixture for the native antenna check: three 1 x 2 um
//! buffers with LEF gate/diffusion areas, an antenna diode master, three
//! routing layers with OXIDE1 rules. Before implementation: < 1 MiB of
//! sources/workspace per test, two test workers, no PDK parse. Foundry-deck
//! agreement is verified separately by scripts/hold_closure.py.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_workspace::antenna::{AntennaRequest, RepairRequest, check_antenna, repair_antenna};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
use rustroad_workspace::{place, read};
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
            max_fanout: 3,
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
    Fixture {
        root,
        workspace,
        sources,
    }
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
    // Unmodeled cumulative rules make the verdict partial, never passed.
    let mut tech = TECHNOLOGY.replace(
        "ANTENNASIDEAREARATIO 400 ; END M1",
        "ANTENNASIDEAREARATIO 400 ; ANTENNACUMAREARATIO 5000 ; END M1",
    );
    let g = routed_with_technology(PLAIN, &tech);
    let r = check_antenna(&g.workspace, &request(None), &limits()).unwrap();
    assert_eq!(r.status, "partial");
    assert_eq!(r.scope.cumulative, "present_in_lef_unsupported");
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
