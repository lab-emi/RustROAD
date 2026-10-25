//! Self-authored export-gds integration fixture: four 1 x 2 um masters (one
//! with a child GDS structure), three routing layers, two cut layers/vias, two
//! ten-site rows (N and FS) and a fully routed seven-net design.
//! Before implementation: each case holds < 1 MiB of LEF/JSON/GDS text and
//! < 64 KiB of streamed output; four test workers stay < 16 MiB incremental.
//! No path, cut or clause enumeration; routing uses the existing hard caps.
//! Independent geometry/DRC/LVS oracles for real PDKs live in
//! scripts/verify_gds.py; this test checks structure, determinism, read-only
//! behaviour and every explicit failure mode.

use rustroad_core::Limits;
use rustroad_core::connectivity::ConnectivityLimits;
use rustroad_formats::gds::{self, ElementKind, GdsLimits, GdsWriter, Real8, Strans};
use rustroad_workspace::gds::{ExportOptions, FIXED_DATES, export_gds};
use rustroad_workspace::pdk::{ImportSources, import_pdk_with_config};
use rustroad_workspace::physical::{PhysicalConfig, TieCell};
use rustroad_workspace::{Error, place};
use std::collections::BTreeMap;
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

fn library(foreign: &dyn Fn(&str) -> String) -> String {
    let mut text = String::from("VERSION 5.8 ;\n");
    for name in ["BUF", "HI", "LO", "FILL"] {
        text.push_str(&format!(
            "MACRO {name}\nCLASS CORE ; {} ORIGIN 0 0 ; SIZE 1 BY 2 ; SITE s ; SYMMETRY X Y ;\n",
            foreign(name)
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
fn same_name(name: &str) -> String {
    format!("FOREIGN {name} ;")
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
    cell_lef: PathBuf,
    head: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn fixture(technology: &str, foreign: &dyn Fn(&str) -> String, dbu: i64) -> Fixture {
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.work/gds-tests");
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
    fs::write(&sources.cell_lef, library(foreign)).unwrap();
    fs::write(
        &sources.netlist,
        "module chip(input a, output y, output h, output l); BUF b0(.A(a),.Y(y)); BUF b1(.A(a),.Y(h)); BUF b2(.A(a),.Y(l)); endmodule",
    )
    .unwrap();
    let k = dbu / 1000;
    let floorplan = serde_json::json!({
        "name":"chip", "units_per_micron":dbu,
        "die_area":{"lx":0,"ly":0,"ux":20000*k,"uy":20000*k},
        "core_area":{"lx":5000*k,"ly":5000*k,"ux":15000*k,"uy":9000*k},
        "site":{"name":"s","width":1000*k,"height":2000*k},
        "rows":[{"x_start":5000*k,"x_end":15000*k,"y":5000*k,"orientation":"N"},
                 {"x_start":5000*k,"x_end":15000*k,"y":7000*k,"orientation":"FS"}]
    });
    fs::write(&sources.floorplan, serde_json::to_vec(&floorplan).unwrap()).unwrap();
    let config_path = root.join("physical.json");
    fs::write(&config_path, serde_json::to_vec(&config()).unwrap()).unwrap();
    let workspace = root.join("workspace");
    let imported = import_pdk_with_config(
        &workspace,
        "rev0",
        &sources,
        &limits(),
        &ConnectivityLimits::default(),
        Some(&config_path),
    )
    .unwrap();
    let placed = place(&workspace, &imported.revision, &limits()).unwrap();
    let sdc = root.join("clock.sdc");
    fs::write(&sdc, "create_clock -name core -period 10 [get_ports a]\nset_clock_transition 0.1 [get_clocks core]\nset_max_fanout 2 [current_design]\n").unwrap();
    let clocked = rustroad_workspace::clock::synthesize_with_sdc(
        &workspace,
        &placed.revision,
        &limits(),
        Some(&sdc),
    )
    .unwrap();
    let routing = root.join("routing.json");
    let step = 400 * k;
    let value = serde_json::json!({"grid_step":step,"access_layer":"M2","layers":[{"name":"M1","spacing":200*k},{"name":"M2","spacing":200*k},{"name":"M3","spacing":200*k}],"vias":[{"name":"V12","cut_spacing":60*k},{"name":"V23","cut_spacing":60*k}],"max_skew_ns":1.0});
    fs::write(&routing, serde_json::to_vec(&value).unwrap()).unwrap();
    let routed = rustroad_workspace::routing::route(
        &workspace,
        &clocked.revision,
        &routing,
        rustroad_workspace::routing::RouteScope::All,
        &limits(),
    )
    .unwrap();
    assert_eq!(routed.routed_nets, Some(7));
    Fixture {
        head: format!("{}\n", routed.revision),
        cell_lef: sources.cell_lef,
        root,
        workspace,
    }
}

const UNITS_HEX: [&str; 2] = ["3e4189374bc6a7f0", "3944b82fa09b5a54"];
fn units() -> [Real8; 2] {
    UNITS_HEX.map(|h| Real8(u64::from_str_radix(h, 16).unwrap().to_be_bytes()))
}
/// Synthetic "PDK" cell library: BUF references LEAF; UNUSED is never placed.
fn cell_gds(path: &Path, omit: &str, extra: &[&str]) -> Vec<u8> {
    let mut w = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
    w.begin_library(5, &[71, 2, 3, 4, 5, 6, 71, 2, 3, 4, 5, 6], "cells", units())
        .unwrap();
    for name in ["LEAF", "BUF", "HI", "LO", "FILL", "UNUSED"]
        .into_iter()
        .chain(extra.iter().copied())
    {
        if name == omit {
            continue;
        }
        w.begin_structure(name, &[71, 2, 3, 4, 5, 6, 71, 2, 3, 4, 5, 6])
            .unwrap();
        w.rectangle(64, 20, [0, 0, 1000, 2000]).unwrap();
        w.text(68, 5, [400, 500], "A").unwrap();
        if name == "BUF" {
            w.sref("LEAF", None, [100, 100]).unwrap();
        }
        w.end_structure().unwrap();
    }
    w.end_library().unwrap();
    let bytes = w.into_inner().unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}
fn layer_map(path: &Path, edit: &dyn Fn(&mut serde_json::Value)) {
    let sha = "0".repeat(64);
    let cite = serde_json::json!([{"source":"spec","sha256":sha,"lines":[1]}]);
    let routing = |lef: &str, n: i64| {
        serde_json::json!({"lef":lef,"kind":"routing","net":[[n,0]],"special_net":[[n,0]],"via":[[n,0]],
            "pin":[[n,0],[n,2]],"label":[n,1],"cite":cite})
    };
    let cut =
        |lef: &str, n: i64| serde_json::json!({"lef":lef,"kind":"cut","via":[[n,0]],"cite":cite});
    let mut value = serde_json::json!({
        "schema":"rustroad.gds-layer-map.v1","pdk":"toy","library":"toy_cells",
        "sources":[{"id":"spec","role":"test","path":"tests/gds_export.rs","sha256":sha,"bytes":1}],
        "layers":[routing("M1",1),cut("V1",10),routing("M2",2),cut("V2",11),routing("M3",3)],
        "boundary":{"layer":[100,0],"cite":cite}
    });
    edit(&mut value);
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    out
}
fn code(error: Error) -> &'static str {
    match error {
        Error::Gds { code, .. } => code,
        other => panic!("expected a coded GDS error, got {other}"),
    }
}

#[test]
fn routed_workspace_streams_deterministically_and_read_only() {
    let f = fixture(TECHNOLOGY, &same_name, 1000);
    let cells = f.root.join("cells.gds");
    let source = cell_gds(&cells, "", &[]);
    let map = f.root.join("map.json");
    layer_map(&map, &|_| {});
    let before = tree(&f.workspace);
    let first = f.root.join("first.gds");
    let report = export_gds(
        &f.workspace,
        &ExportOptions::new(cells.clone(), map.clone(), first.clone()),
        &limits(),
    )
    .unwrap();
    assert_eq!(
        tree(&f.workspace),
        before,
        "export must not touch the workspace"
    );
    assert_eq!(
        fs::read_to_string(f.workspace.join("HEAD")).unwrap(),
        f.head
    );
    assert_eq!(report.status, "written");
    assert_eq!(report.coordinate_divisor, 1);
    // Only placed masters (BUF, FILL) and BUF's child LEAF are copied.
    assert_eq!(report.foreign_bindings_checked, 2);
    assert_eq!(report.pdk_structures, ["LEAF", "BUF", "FILL"]);
    assert_eq!(report.via_structures, ["VIA_V12", "VIA_V23"]);
    let bytes = fs::read(&first).unwrap();
    assert_eq!(report.output.bytes, bytes.len() as u64);
    assert_eq!(report.output.sha256, {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(&bytes))
    });

    // Re-export is byte-identical; copied cells equal the source structures.
    let second = f.root.join("second.gds");
    export_gds(
        &f.workspace,
        &ExportOptions::new(cells.clone(), map.clone(), second.clone()),
        &limits(),
    )
    .unwrap();
    assert_eq!(fs::read(&second).unwrap(), bytes);
    let input = gds::parse_library(&source, &GdsLimits::default(), true).unwrap();
    let output = gds::parse_library(&bytes, &GdsLimits::default(), true).unwrap();
    assert_eq!(output.units, input.units);
    assert_eq!(output.dates, FIXED_DATES);
    let index = input.index();
    for s in output.structures.iter().take(3) {
        assert_eq!(
            &bytes[s.source.clone()],
            &source[input.structures[index[s.name.as_str()]].source.clone()]
        );
    }
    let top = output.structures.last().unwrap();
    assert_eq!(top.name, "chip");
    assert_eq!(top.dates, FIXED_DATES);
    let count = |k: ElementKind| top.elements.iter().filter(|e| e.kind == k).count();
    let e = &report.elements;
    assert_eq!(
        count(ElementKind::Sref),
        e.instances + e.routed_vias + e.supply_vias
    );
    assert_eq!(
        count(ElementKind::Boundary),
        e.routed_wires + e.routed_patches + e.supply_wires + e.pin_shapes + 1
    );
    assert!(e.routed_wires < e.stored_wire_records, "runs are merged");
    assert_eq!(count(ElementKind::Text), 6, "a, y, h, l, VDD, VSS");
    assert!(e.routed_wires > 0 && e.routed_vias > 0 && e.supply_vias > 0);
    // FS rows reflect about x at y + height; N rows have no STRANS.
    let snapshot = rustroad_workspace::read(&f.workspace, &limits()).unwrap();
    let records: Vec<_> = snapshot.records().collect();
    for (r, element) in records.iter().zip(top.elements.iter()) {
        if r.y == 7000 {
            assert_eq!(element.strans, Some(Strans::reflect_x()));
            assert_eq!(element.xy, vec![[r.x as i32, (r.y + r.height) as i32]]);
        } else {
            assert_eq!(element.strans, None);
            assert_eq!(element.xy, vec![[r.x as i32, r.y as i32]]);
        }
    }
    // Labels sit at pin centres on the label datatype; pins on both targets.
    let state = snapshot
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .state
        .clone()
        .unwrap();
    drop(snapshot);
    for pin in &state.io_pins {
        let text = top
            .elements
            .iter()
            .find(|x| x.string.as_deref() == Some(pin.name.as_str()))
            .unwrap();
        assert_eq!((text.layer, text.datatype), (Some(3), Some(1)));
        assert_eq!(
            text.xy,
            vec![[
                ((pin.rect.lx + pin.rect.ux) / 2) as i32,
                ((pin.rect.ly + pin.rect.uy) / 2) as i32
            ]]
        );
    }
    let boundary = top.elements.iter().find(|x| x.layer == Some(100)).unwrap();
    assert_eq!(boundary.xy[2], [20000, 20000]);

    // Output inside the workspace, onto an input, or over a directory: refused.
    for out in [
        f.workspace.join("x.gds"),
        cells.clone(),
        f.root.join("revisions-dir"),
    ] {
        if out.ends_with("revisions-dir") {
            fs::create_dir_all(&out).unwrap();
        }
        let error = export_gds(
            &f.workspace,
            &ExportOptions::new(cells.clone(), map.clone(), out),
            &limits(),
        )
        .unwrap_err();
        assert!(matches!(
            code(error),
            "GDS_OUTPUT_IN_WORKSPACE" | "GDS_OUTPUT"
        ));
    }
    assert_eq!(tree(&f.workspace), before);
    // Output caps are hard errors and leave nothing behind.
    let capped = f.root.join("capped.gds");
    let mut options = ExportOptions::new(cells.clone(), map.clone(), capped.clone());
    options.max_output_bytes = 512;
    assert_eq!(
        code(export_gds(&f.workspace, &options, &limits()).unwrap_err()),
        "GDS_BUDGET"
    );
    assert!(!capped.exists());
    let leftovers = fs::read_dir(&f.root)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        })
        .count();
    assert_eq!(leftovers, 0);
}

/// Exact area of a union of rectangles by coordinate compression (test-only,
/// independent of the export code).
fn union_area(rects: &[[i64; 4]]) -> i128 {
    let mut xs: Vec<i64> = rects.iter().flat_map(|r| [r[0], r[2]]).collect();
    let mut ys: Vec<i64> = rects.iter().flat_map(|r| [r[1], r[3]]).collect();
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    let mut area = 0i128;
    for i in 0..xs.len().saturating_sub(1) {
        for j in 0..ys.len().saturating_sub(1) {
            if rects
                .iter()
                .any(|r| r[0] <= xs[i] && xs[i] < r[2] && r[1] <= ys[j] && ys[j] < r[3])
            {
                area += i128::from(xs[i + 1] - xs[i]) * i128::from(ys[j + 1] - ys[j]);
            }
        }
    }
    area
}

#[test]
fn def_and_gds_write_merged_runs_with_the_stored_lattice_metal() {
    let f = fixture(TECHNOLOGY, &same_name, 1000);
    let snapshot = rustroad_workspace::read(&f.workspace, &limits()).unwrap();
    let mut def = Vec::new();
    snapshot.export_def(&mut def).unwrap();
    let routing = snapshot
        .metadata()
        .physical
        .as_ref()
        .unwrap()
        .routing
        .clone()
        .unwrap();
    let nets: Vec<String> = snapshot
        .connectivity()
        .unwrap()
        .nets
        .iter()
        .map(|n| n.name.clone())
        .collect();
    drop(snapshot);
    // All toy routing layers have WIDTH 0.2 um: half width 100 DBU.
    let rect = |a: [i64; 2], b: [i64; 2]| {
        [
            a[0].min(b[0]) - 100,
            a[1].min(b[1]) - 100,
            a[0].max(b[0]) + 100,
            a[1].max(b[1]) + 100,
        ]
    };
    let mut stored = BTreeMap::<(String, String), Vec<[i64; 4]>>::new();
    for w in &routing.routes.wires {
        stored
            .entry((
                nets[w.net as usize].clone(),
                routing.layers[w.layer as usize].clone(),
            ))
            .or_default()
            .push(rect(w.start, w.end));
    }
    // Wire paths of the DEF NETS section, per (net, layer).
    let text = String::from_utf8(def).unwrap();
    let body = text.split("\nNETS ").nth(1).unwrap();
    let body = body.split("\nEND NETS").next().unwrap();
    let mut written = BTreeMap::<(String, String), Vec<[i64; 4]>>::new();
    let mut net = String::new();
    let mut paths = 0usize;
    for line in body.lines() {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.first() == Some(&"-") {
            net = t[1].to_string();
            continue;
        }
        let at = match t.first() {
            Some(&"NEW") => 1,
            Some(&"+") if t.get(1) == Some(&"ROUTED") => 2,
            _ => continue,
        };
        // `<layer> ( x y ) ( x y )` is a wire; vias and RECT patches differ.
        if t.len() >= at + 9 && t[at + 1] == "(" && t[at + 5] == "(" {
            let n = |i: usize| t[at + i].parse::<i64>().unwrap();
            written
                .entry((net.clone(), t[at].to_string()))
                .or_default()
                .push(rect([n(2), n(3)], [n(6), n(7)]));
            paths += 1;
        }
    }
    assert_eq!(
        stored.keys().collect::<Vec<_>>(),
        written.keys().collect::<Vec<_>>()
    );
    for (key, rects) in &stored {
        let both: Vec<[i64; 4]> = rects.iter().chain(&written[key]).copied().collect();
        let a = union_area(rects);
        assert_eq!(a, union_area(&written[key]), "{key:?}");
        assert_eq!(a, union_area(&both), "{key:?} unions differ");
    }
    assert!(paths < routing.routes.wires.len(), "{paths} paths");
    // GDS writes the same runs: one rectangle per run (one net target here).
    let cells = f.root.join("cells.gds");
    cell_gds(&cells, "", &[]);
    let map = f.root.join("map.json");
    layer_map(&map, &|_| {});
    let report = export_gds(
        &f.workspace,
        &ExportOptions::new(cells, map, f.root.join("runs.gds")),
        &limits(),
    )
    .unwrap();
    assert_eq!(report.elements.routed_wires, paths);
    assert_eq!(
        report.elements.stored_wire_records,
        routing.routes.wires.len()
    );
    assert_eq!(report.elements.routed_patches, routing.routes.patches.len());
}

#[test]
fn missing_cells_collisions_and_wrong_maps_fail_explicitly() {
    let f = fixture(TECHNOLOGY, &same_name, 1000);
    let map = f.root.join("map.json");
    layer_map(&map, &|_| {});
    let out = f.root.join("out.gds");
    let run = |cells: &Path, map: &Path| {
        export_gds(
            &f.workspace,
            &ExportOptions::new(cells.to_path_buf(), map.to_path_buf(), out.clone()),
            &limits(),
        )
    };
    let cells = f.root.join("cells.gds");
    for omit in ["BUF", "LEAF"] {
        cell_gds(&cells, omit, &[]);
        assert_eq!(
            code(run(&cells, &map).unwrap_err()),
            "GDS_CELL_MISSING",
            "{omit}"
        );
    }
    for extra in ["VIA_V12", "chip"] {
        cell_gds(&cells, "", &[extra]);
        assert_eq!(
            code(run(&cells, &map).unwrap_err()),
            "GDS_NAME_COLLISION",
            "{extra}"
        );
    }
    cell_gds(&cells, "", &[]);
    let wrong = f.root.join("wrong.json");
    layer_map(&wrong, &|v| {
        v["layers"].as_array_mut().unwrap().remove(2);
    });
    assert_eq!(code(run(&cells, &wrong).unwrap_err()), "GDS_LAYER_UNMAPPED");
    layer_map(&wrong, &|v| {
        v["layers"][0]["lef"] = "V1".into();
        v["layers"][1]["lef"] = "M1".into();
    });
    assert_eq!(code(run(&cells, &wrong).unwrap_err()), "GDS_LAYER_KIND");
    layer_map(&wrong, &|v| v["layers"][0]["cite"] = serde_json::json!([]));
    assert_eq!(code(run(&cells, &wrong).unwrap_err()), "GDS_LAYER_MAP");
    // A half-nanometre GDS grid would require rescaling library units: refused.
    let mut w = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
    w.begin_library(
        5,
        &FIXED_DATES,
        "cells",
        [
            Real8::from_f64(5e-4).unwrap(),
            Real8::from_f64(5e-10).unwrap(),
        ],
    )
    .unwrap();
    w.end_library().unwrap();
    fs::write(&cells, w.into_inner().unwrap()).unwrap();
    assert_eq!(
        code(run(&cells, &map).unwrap_err()),
        "GDS_UNITS_UNSUPPORTED"
    );
    // Changing the cell LEF after import is detected through its sha256.
    cell_gds(&cells, "", &[]);
    fs::write(&f.cell_lef, library(&same_name) + "\n").unwrap();
    assert_eq!(code(run(&cells, &map).unwrap_err()), "GDS_SOURCE_CHANGED");
    assert!(!out.exists());
}

#[test]
fn foreign_bindings_must_name_the_same_cell_at_the_origin() {
    for (foreign, expected) in [
        (
            &(|n: &str| {
                if n == "BUF" {
                    "FOREIGN BUF 0.01 0 ;".into()
                } else {
                    same_name(n)
                }
            }) as &dyn Fn(&str) -> String,
            "GDS_FOREIGN_MISMATCH",
        ),
        (
            &|n: &str| {
                if n == "FILL" {
                    "FOREIGN OTHER ;".into()
                } else {
                    same_name(n)
                }
            },
            "GDS_FOREIGN_MISMATCH",
        ),
        (&|_: &str| String::new(), "GDS_FOREIGN_MISSING"),
    ] {
        let f = fixture(TECHNOLOGY, foreign, 1000);
        let cells = f.root.join("cells.gds");
        cell_gds(&cells, "", &[]);
        let map = f.root.join("map.json");
        layer_map(&map, &|_| {});
        let error = export_gds(
            &f.workspace,
            &ExportOptions::new(cells, map, f.root.join("out.gds")),
            &limits(),
        )
        .unwrap_err();
        assert_eq!(code(error), expected);
    }
}

#[test]
fn finer_lef_grid_divides_exactly_and_rejects_odd_coordinates() {
    // 2000 DBU/um LEF against a 0.001 um cell GDS: divide by two exactly.
    let fine = TECHNOLOGY
        .replace("MICRONS 1000", "MICRONS 2000")
        .replace("MANUFACTURINGGRID 0.01", "MANUFACTURINGGRID 0.0005");
    let f = fixture(&fine, &same_name, 2000);
    let cells = f.root.join("cells.gds");
    cell_gds(&cells, "", &[]);
    let map = f.root.join("map.json");
    layer_map(&map, &|_| {});
    let report = export_gds(
        &f.workspace,
        &ExportOptions::new(cells.clone(), map.clone(), f.root.join("even.gds")),
        &limits(),
    )
    .unwrap();
    assert_eq!(report.coordinate_divisor, 2);
    let output = gds::parse_library(
        &fs::read(&report.output.path).unwrap(),
        &GdsLimits::default(),
        true,
    )
    .unwrap();
    let boundary = output
        .structures
        .last()
        .unwrap()
        .elements
        .iter()
        .find(|x| x.layer == Some(100))
        .unwrap();
    assert_eq!(
        boundary.xy[2],
        [20000, 20000],
        "40000 LEF DBU -> 20000 GDS DBU"
    );
    // A 0.0005 um cut edge (odd LEF DBU) has no exact 0.001 um representation.
    let odd = fine.replace(
        "LAYER V2 ; RECT -0.03 -0.03 0.03 0.03 ;",
        "LAYER V2 ; RECT -0.0305 -0.03 0.03 0.03 ;",
    );
    let f = fixture(&odd, &same_name, 2000);
    let cells = f.root.join("cells.gds");
    cell_gds(&cells, "", &[]);
    let map = f.root.join("map.json");
    layer_map(&map, &|_| {});
    let out = f.root.join("odd.gds");
    let error = export_gds(
        &f.workspace,
        &ExportOptions::new(cells, map, out.clone()),
        &limits(),
    )
    .unwrap_err();
    assert_eq!(code(error), "GDS_ODD_COORDINATE");
    assert!(!out.exists());
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

#[test]
#[ignore = "requires explicit routed real-PDK workspaces and pinned cell GDS; absence is unverified"]
fn real_pdk_routed_workspaces_stream_verbatim_cells() {
    // Each workspace is copied first: the source flow run is never touched.
    for (tag, map, divisor) in [
        ("SKY130", "sky130-gds-map.json", 1),
        ("GF180", "gf180-gds-map.json", 2),
    ] {
        let source = PathBuf::from(
            std::env::var(format!("RUSTROAD_{tag}_GDS_WORKSPACE"))
                .expect("explicit routed workspace required"),
        );
        let cells = PathBuf::from(
            std::env::var(format!("RUSTROAD_{tag}_CELL_GDS")).expect("explicit cell GDS required"),
        );
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.work/gds-tests")
            .join(format!("real-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        copy_tree(&source, &workspace);
        let before = tree(&workspace);
        let map = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/pdk")
            .join(map);
        let limits = Limits {
            max_cells: 2000,
            max_rows: 128,
            max_expansions: 350_000,
        };
        let out = root.join("native.gds");
        let report = export_gds(
            &workspace,
            &ExportOptions::new(cells.clone(), map.clone(), out.clone()),
            &limits,
        )
        .unwrap();
        let again = root.join("again.gds");
        export_gds(
            &workspace,
            &ExportOptions::new(cells.clone(), map, again.clone()),
            &limits,
        )
        .unwrap();
        assert_eq!(tree(&workspace), before);
        assert_eq!(report.coordinate_divisor, divisor);
        // Lattice records are merged into far fewer DEF/GDS runs.
        let e = &report.elements;
        assert!(e.stored_wire_records > 1000 && e.routed_wires > 50, "{e:?}");
        assert!(
            e.routed_wires < e.stored_wire_records && e.labels == 12,
            "{e:?}"
        );
        let bytes = fs::read(&out).unwrap();
        assert_eq!(bytes, fs::read(&again).unwrap());
        let source_bytes = fs::read(&cells).unwrap();
        let library = gds::parse_library(&source_bytes, &GdsLimits::default(), false).unwrap();
        let output = gds::parse_library(&bytes, &GdsLimits::default(), false).unwrap();
        let index = library.index();
        for name in &report.pdk_structures {
            let s = output.structures.iter().find(|s| &s.name == name).unwrap();
            assert_eq!(
                &bytes[s.source.clone()],
                &source_bytes[library.structures[index[name.as_str()]].source.clone()],
                "{name} copied verbatim"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
