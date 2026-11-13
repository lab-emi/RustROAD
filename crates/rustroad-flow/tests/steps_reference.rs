//! The native flow steps against outputs of the Python reference helpers
//! (`tests/fixtures/steps/generate.py` wrote both the synthetic inputs and
//! the expectations). Paths are instantiated in a private temporary root.
use rustroad_flow::steps::postroute::policy::{Thresholds, families, plan};
use rustroad_flow::steps::postroute::{lef, liberty, netlist};
use rustroad_flow::steps::rc_profile::{self, Request, ResistanceModel};
use rustroad_flow::steps::{gds_bundle, layer_map, physical_inputs};
use rustroad_pyjson::{Dict, FileId, Layout, Value, dumps, list, parse};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/steps")
}

fn text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

struct Root(PathBuf);
impl Root {
    fn new(tag: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustroad-steps-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
    fn text(&self) -> &str {
        self.0.to_str().unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Fill a stored expectation: the root, then the identities of the files
/// whose own text holds the root (recomputed from the files under `root`).
fn instantiate(template: &str, root: &Root) -> String {
    let mut out = template.replace("@ROOT@", root.text());
    for (open, quoted) in [("\"@BYTES:", true), ("@SHA:", false)] {
        while let Some(start) = out.find(open) {
            let name_start = start + open.len();
            let end = name_start + out[name_start..].find('@').unwrap();
            let id = FileId::of(&root.0.join(&out[name_start..end]), 1 << 30).unwrap();
            let (value, stop) = if quoted {
                (id.bytes.to_string(), end + 2)
            } else {
                (id.sha256, end + 1)
            };
            out.replace_range(start..stop, &value);
        }
    }
    out
}

fn physical_root(tag: &str) -> Root {
    let root = Root::new(tag);
    copy_tree(&fixtures().join("physical/inputs"), &root.0);
    root
}

#[test]
fn rc_profiles_equal_the_reference_builder_at_every_corner_and_model() {
    let root = physical_root("rc");
    let pdk_root = format!("{}/pdk", root.text());
    for corner in ["min", "nom", "max"] {
        for (name, model) in [
            ("magic", ResistanceModel::Magic),
            ("openrcx-pdk", ResistanceModel::OpenRcxPdk),
        ] {
            let profile = rc_profile::build(&Request {
                corner,
                pdk_root: &pdk_root,
                model,
                rules_override: None,
                technology_lef: None,
            })
            .unwrap();
            let expected =
                text(&fixtures().join(format!("physical/expected/rc-{corner}-{name}.json")));
            assert_eq!(
                rc_profile::render(&profile).unwrap(),
                instantiate(&expected, &root),
                "{corner} {name}"
            );
        }
    }
    // The recipe's choice: the PDK default pattern table as an explicit override.
    let rules = format!("{pdk_root}/ihp-sg13g2/libs.tech/librelane/openrcx/IHP_rcx_patterns.rules");
    let profile = rc_profile::build(&Request {
        corner: "nom",
        pdk_root: &pdk_root,
        model: ResistanceModel::OpenRcxPdk,
        rules_override: Some(&rules),
        technology_lef: None,
    })
    .unwrap();
    let expected = text(&fixtures().join("physical/expected/rc-nom-openrcx-pdk-patterns.json"));
    assert_eq!(
        rc_profile::render(&profile).unwrap(),
        instantiate(&expected, &root)
    );
    // The corner diagnostics came through Python's `{value:g}`.
    let max = text(&fixtures().join("physical/expected/rc-max-magic.json"));
    assert!(max.contains("the same 15000 mOhm"));
    let failure = rc_profile::build(&Request {
        corner: "typ",
        pdk_root: &pdk_root,
        model: ResistanceModel::Magic,
        rules_override: None,
        technology_lef: None,
    })
    .unwrap_err();
    assert_eq!(failure.code, "RC_PROFILE");
    let missing = rc_profile::build(&Request {
        corner: "nom",
        pdk_root: root.text(),
        model: ResistanceModel::Magic,
        rules_override: None,
        technology_lef: None,
    })
    .unwrap_err();
    assert!(
        missing.message.starts_with("missing PDK data file"),
        "{missing}"
    );
    assert!(ResistanceModel::parse("spice").is_err());
}

#[test]
fn gds_bundle_copies_structures_and_reports_like_the_reference() {
    let root = physical_root("bundle");
    std::fs::create_dir(root.0.join("bundle")).unwrap();
    let sources = [root.0.join("cells-a.gds"), root.0.join("cells-b.gds")];
    let out = root.0.join("bundle/combined.gds");
    let report = gds_bundle::bundle(&sources, &out).unwrap();
    let expected = text(&fixtures().join("physical/expected/bundle.json"));
    assert_eq!(
        dumps(&report, Layout::Indent(2)).unwrap() + "\n",
        instantiate(&expected, &root)
    );
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(fixtures().join("physical/expected/combined.gds")).unwrap()
    );
    // Units are never rescaled, a name never changes its bytes, a stream must end.
    let original = std::fs::read(&sources[0]).unwrap();
    let units = original
        .windows(4)
        .position(|w| w == [0, 20, 3, 5])
        .unwrap();
    let mut rescaled = original.clone();
    rescaled[units + 5] ^= 1;
    std::fs::write(root.0.join("rescaled.gds"), &rescaled).unwrap();
    let mut changed = original.clone();
    let boundary = changed.windows(4).position(|w| w == [0, 4, 8, 0]).unwrap();
    changed[boundary + 9] ^= 1;
    std::fs::write(root.0.join("changed.gds"), &changed).unwrap();
    std::fs::write(root.0.join("cut.gds"), &original[..original.len() - 4]).unwrap();
    for (name, message) in [
        (
            "rescaled.gds",
            "GDS units differ; implicit rescale forbidden",
        ),
        ("changed.gds", "conflicting original GDS structure"),
        ("cut.gds", "incomplete GDS library"),
    ] {
        let failure = gds_bundle::bundle(
            &[sources[0].clone(), root.0.join(name)],
            &root.0.join("x.gds"),
        )
        .unwrap_err();
        assert_eq!(
            (failure.code, failure.message.as_str()),
            ("GDS_BUNDLE", message)
        );
    }
}

#[test]
fn layer_map_projection_equals_the_reference() {
    let root = physical_root("map");
    let map = layer_map::project(
        &root.0.join("layers.map"),
        "ihp-sg13g2",
        "ez130_8t_sram",
        &layer_map::IHP_SG13G2_LAYERS,
    )
    .unwrap();
    let expected = text(&fixtures().join("physical/expected/layer-map.json"));
    assert_eq!(
        dumps(&map, Layout::Indent(2)).unwrap() + "\n",
        instantiate(&expected, &root)
    );
    let failure =
        layer_map::project(&root.0.join("layers.map"), "p", "l", &["Metal9"]).unwrap_err();
    assert_eq!(
        failure.to_string(),
        "LAYER_MAP: missing mapped layer Metal9"
    );
    let source = text(&root.0.join("layers.map"));
    std::fs::write(root.0.join("two.map"), source + "DIEAREA ALL 1 2\n").unwrap();
    let failure = layer_map::project(
        &root.0.join("two.map"),
        "p",
        "l",
        &layer_map::IHP_SG13G2_LAYERS,
    )
    .unwrap_err();
    assert_eq!(failure.message, "unique die boundary map required");
}

#[test]
fn physical_inputs_step_writes_the_reference_files_and_refuses_reuse() {
    let root = physical_root("step");
    let inputs = root.0.join("step-inputs.json");
    std::fs::write(
        &inputs,
        instantiate(&text(&fixtures().join("physical/step-inputs.json")), &root),
    )
    .unwrap();
    let out = root.0.join("step");
    let stdout = physical_inputs::run(&inputs, &out).unwrap();
    let expected = fixtures().join("physical/expected");
    for name in ["gds-inputs.json", "gds-map.json", "ihp-rc-nom.json"] {
        assert_eq!(
            text(&out.join(name)),
            instantiate(&text(&expected.join("step").join(name)), &root),
            "{name}"
        );
    }
    assert_eq!(
        std::fs::read(out.join("combined-cells.gds")).unwrap(),
        std::fs::read(expected.join("step/combined-cells.gds")).unwrap()
    );
    assert_eq!(
        text(&out.join("result.json")),
        instantiate(&text(&expected.join("step-result.json")), &root)
    );
    assert_eq!(
        stdout + "\n",
        instantiate(&text(&expected.join("step-stdout.txt")), &root)
    );
    assert_eq!(text(&out.join("HEAD")), "rev1\n");
    assert!(out.join("workspace.lock").is_file());
    // One run per directory; a changed input is caught before anything is written.
    let again = physical_inputs::run(&inputs, &out).unwrap_err();
    assert_eq!(again.code, "REVISION_CONFLICT");
    std::fs::write(root.0.join("layers.map"), "DIEAREA ALL 1 2\n").unwrap();
    let stale = physical_inputs::run(&inputs, &root.0.join("step2")).unwrap_err();
    assert_eq!(
        stale.to_string(),
        "PHYSICAL_INPUTS: input changed: layer_map"
    );
    assert_eq!(std::fs::read_dir(root.0.join("step2")).unwrap().count(), 1);
    let v1 = text(&inputs).replace("inputs.v2", "inputs.v1");
    std::fs::write(&inputs, v1).unwrap();
    let schema = physical_inputs::run(&inputs, &root.0.join("step3")).unwrap_err();
    assert_eq!(schema.to_string(), "PHYSICAL_INPUTS: input schema");
}

fn postroute(name: &str) -> PathBuf {
    fixtures().join("postroute").join(name)
}

fn libraries() -> [liberty::Library; 2] {
    [
        liberty::read(&postroute("inputs/tt.lib")).unwrap(),
        liberty::read(&postroute("inputs/ss.lib")).unwrap(),
    ]
}

fn library_value(library: &liberty::Library) -> Value {
    Value::Dict(
        library
            .cells
            .iter()
            .map(|(name, cell)| {
                let pins: Dict = cell
                    .pins
                    .0
                    .iter()
                    .map(|(pin, attributes)| {
                        let mut entry = Dict::new();
                        for (key, value) in [
                            ("direction", &attributes.direction),
                            ("function", &attributes.function),
                            ("max_capacitance", &attributes.max_capacitance),
                            ("max_transition", &attributes.max_transition),
                        ] {
                            if let Some(value) = value {
                                entry.insert(key, value.as_str());
                            }
                        }
                        (pin.clone(), Value::Dict(entry))
                    })
                    .collect();
                (
                    name.clone(),
                    Value::Dict(
                        Dict::new()
                            .with("area", cell.area)
                            .with("dont_use", cell.dont_use)
                            .with("unsupported", cell.unsupported)
                            .with("pins", pins),
                    ),
                )
            })
            .collect(),
    )
}

#[test]
fn liberty_lef_and_netlist_readers_see_what_the_reference_sees() {
    let libs = libraries();
    let expected = parse(&text(&postroute("expected/liberty.json"))).unwrap();
    for (corner, library) in ["tt", "ss"].into_iter().zip(&libs) {
        let ours = library_value(library);
        assert_eq!(&ours, expected.get(corner).unwrap(), "{corner}");
        // Same cell order as the file.
        let names: Vec<&str> = expected
            .get(corner)
            .unwrap()
            .as_dict()
            .unwrap()
            .keys()
            .collect();
        assert!(library.cells.iter().map(|(n, _)| n.as_str()).eq(names));
    }
    // A pin whose attributes share the line that opens it has none (BUFX1/A).
    assert_eq!(
        libs[0]
            .get("BUFX1")
            .unwrap()
            .pins
            .get("A")
            .unwrap()
            .direction,
        None
    );
    let sizes = lef::sizes(&text(&postroute("inputs/cells.lef"))).unwrap();
    let rendered = Value::Dict(
        sizes
            .iter()
            .map(|(name, (w, h))| (name.clone(), list([*w, *h])))
            .collect(),
    );
    assert_eq!(
        dumps(&rendered, Layout::Indent(1)).unwrap() + "\n",
        text(&postroute("expected/sizes.json"))
    );
    let instances = netlist::instances(&text(&postroute("inputs/netlist.v")));
    let rendered = Value::Dict(
        instances
            .0
            .iter()
            .map(|(name, instance)| {
                let pins: Dict = instance
                    .pins
                    .0
                    .iter()
                    .map(|(pin, net)| (pin.clone(), Value::from(net)))
                    .collect();
                (
                    name.clone(),
                    Value::List(vec![Value::from(&instance.master), Value::Dict(pins)]),
                )
            })
            .collect(),
    );
    assert_eq!(
        dumps(&rendered, Layout::Indent(1)).unwrap() + "\n",
        text(&postroute("expected/instances.json"))
    );
    let logical = text(&postroute("inputs/netlist.v"));
    let lef_text = text(&postroute("inputs/cells.lef"));
    let names = lef::physical_only_cells(&lef_text, &libs, &logical).unwrap();
    assert_eq!(
        dumps(&list(names.iter()), Layout::Line).unwrap() + "\n",
        text(&postroute("expected/physical-only.json"))
    );
    for corner in ["tt", "ss"] {
        assert_eq!(
            lef::stub_library(corner, &names),
            text(&postroute(&format!("expected/{corner}-physical.lib")))
        );
    }
    let connected = logical.replace("FILL1 fill_1 ();", "FILL1 fill_1 (.VDD(vdd));");
    let failure = lef::physical_only_cells(&lef_text, &libs, &connected).unwrap_err();
    assert_eq!(
        failure.to_string(),
        "POSTROUTE: connected physical-only cell: FILL1"
    );
    assert!(liberty::parse("library(x) {\n cell(a) {\n", "x").is_err());
    assert!(liberty::parse("library(x) {\n}\n", "x").is_err());
    assert!(lef::sizes("MACRO A\n SIZE 1.2.3 BY 1 ;\nEND A\n").is_err());
}

#[test]
fn sizing_families_and_repair_plans_equal_the_reference_policy() {
    let libs = libraries();
    let sizes = lef::sizes(&text(&postroute("inputs/cells.lef"))).unwrap();
    let classes = families(&libs, &sizes).unwrap();
    assert_eq!(
        dumps(
            &list(classes.list.iter().map(|family| family.to_value())),
            Layout::Indent(2)
        )
        .unwrap()
            + "\n",
        text(&postroute("expected/families.json"))
    );
    let instances = netlist::instances(&text(&postroute("inputs/netlist.v")));
    let prects_text = text(&postroute("inputs/prects.v"));
    let prects = netlist::instances(&prects_text);
    let known: HashSet<&str> = prects.0.iter().map(|(name, _)| name.as_str()).collect();
    let thresholds = Thresholds {
        near_limit_ratio: 0.9,
        target_ratio: 0.8,
        max_resizes: 256,
    };
    for scenario in ["empty", "mixed", "second_round"] {
        let rows = parse(&text(&postroute(&format!("inputs/rows-{scenario}.json")))).unwrap();
        let corners = rows.get("corners").unwrap();
        let (tt, ss) = (
            corners.get("tt").unwrap().as_list().unwrap(),
            corners.get("ss").unwrap().as_list().unwrap(),
        );
        let resized = rows.get("resized").unwrap().as_dict().unwrap();
        let decided = plan(
            &[("tt", tt), ("ss", ss)],
            &instances.0,
            &known,
            &libs,
            &classes,
            resized,
            &thresholds,
        )
        .unwrap();
        let rendered = Value::Dict(
            Dict::new()
                .with("ops", list(decided.ops.iter().cloned()))
                .with("residual", list(decided.residual.iter().cloned()))
                .with("skipped", list(decided.skipped.iter().cloned())),
        );
        assert_eq!(
            dumps(&rendered, Layout::Indent(2)).unwrap() + "\n",
            text(&postroute(&format!("expected/plan-{scenario}.json"))),
            "{scenario}"
        );
    }
    // The cumulative resize budget is a hard stop.
    let rows = parse(&text(&postroute("inputs/rows-mixed.json"))).unwrap();
    let corners = rows.get("corners").unwrap();
    let failure = plan(
        &[("tt", corners.get("tt").unwrap().as_list().unwrap())],
        &instances.0,
        &known,
        &libs,
        &classes,
        &Dict::new(),
        &Thresholds {
            max_resizes: 1,
            ..thresholds
        },
    )
    .unwrap_err();
    assert_eq!(failure.to_string(), "POSTROUTE: post-route resize budget");
}
