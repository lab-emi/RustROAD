//! The native flow steps against outputs of the Python reference helpers
//! (`tests/fixtures/steps/generate.py` wrote both the synthetic inputs and
//! the expectations). Paths are instantiated in a private temporary root.
use rustroad_flow::steps::layer_map::Form;
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
                pdk: "ihp",
                corner,
                pdk_root: &pdk_root,
                model,
                rules_override: None,
                technology_lef: None,
                magic: None,
                magic_extract: None,
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
    let patterns = Request {
        pdk: "ihp",
        corner: "nom",
        pdk_root: &pdk_root,
        model: ResistanceModel::OpenRcxPdk,
        rules_override: Some(&rules),
        technology_lef: None,
        magic: None,
        magic_extract: None,
    };
    let profile = rc_profile::build(&patterns).unwrap();
    let expected = text(&fixtures().join("physical/expected/rc-nom-openrcx-pdk-patterns.json"));
    assert_eq!(
        rc_profile::render(&profile).unwrap(),
        instantiate(&expected, &root)
    );
    // The corner diagnostics came through Python's `{value:g}`.
    let max = text(&fixtures().join("physical/expected/rc-max-magic.json"));
    assert!(max.contains("the same 15000 mOhm"));
    // Every file given explicitly, wherever it lies: the same profile, whatever the PDK root.
    let base = format!("{pdk_root}/ihp-sg13g2");
    let explicit = rc_profile::build(&Request {
        pdk_root: "/nowhere",
        technology_lef: Some(&format!(
            "{base}/libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef"
        )),
        magic: Some(&format!("{base}/libs.tech/magic/ihp-sg13g2.tech")),
        magic_extract: Some(&format!("{base}/libs.tech/magic/ihp-sg13g2-extract.tech")),
        ..patterns.clone()
    })
    .unwrap();
    assert_eq!(
        rc_profile::render(&explicit).unwrap(),
        instantiate(&expected, &root)
    );
    let failure = rc_profile::build(&Request {
        corner: "typ",
        model: ResistanceModel::Magic,
        rules_override: None,
        ..patterns.clone()
    })
    .unwrap_err();
    assert_eq!(failure.code, "RC_PROFILE");
    let unknown = rc_profile::build(&Request {
        pdk: "sky90",
        ..patterns.clone()
    })
    .unwrap_err();
    assert_eq!(unknown.to_string(), "RC_PROFILE: unsupported PDK sky90");
    let missing = rc_profile::build(&Request {
        pdk_root: root.text(),
        model: ResistanceModel::Magic,
        rules_override: None,
        ..patterns.clone()
    })
    .unwrap_err();
    assert!(
        missing.message.starts_with("missing PDK data file"),
        "{missing}"
    );
    assert!(ResistanceModel::parse("spice").is_err());
}

/// The `sky130` and `gf180` table entries against the reference builder
/// (`fixtures/steps/generate_pdk_profiles.py`): trees in the layout and with the
/// layer, Magic type and contact names of the two PDKs, at every corner.
#[test]
fn rc_profiles_of_the_other_pdks_equal_the_reference_builder() {
    let root = Root::new("pdk-rc");
    copy_tree(&fixtures().join("pdk-profiles/inputs"), &root.0);
    let pdk_root = format!("{}/pdk", root.text());
    for pdk in ["sky130", "gf180"] {
        for corner in ["min", "nom", "max"] {
            let profile = rc_profile::build(&Request {
                pdk,
                corner,
                pdk_root: &pdk_root,
                model: ResistanceModel::Magic,
                rules_override: None,
                technology_lef: None,
                magic: None,
                magic_extract: None,
            })
            .unwrap();
            let expected =
                text(&fixtures().join(format!("pdk-profiles/expected/{pdk}-rc-{corner}.json")));
            assert_eq!(
                rc_profile::render(&profile).unwrap(),
                instantiate(&expected, &root),
                "{pdk} {corner}"
            );
        }
    }
    let request = Request {
        pdk: "gf180",
        corner: "nom",
        pdk_root: &pdk_root,
        model: ResistanceModel::Magic,
        rules_override: None,
        technology_lef: None,
        magic: None,
        magic_extract: None,
    };
    // The matched resistance model is qualified for IHP only, as in the reference.
    let matched = rc_profile::build(&Request {
        model: ResistanceModel::OpenRcxPdk,
        ..request.clone()
    })
    .unwrap_err();
    assert_eq!(
        matched.message,
        "openrcx-pdk resistance is qualified only for IHP; baseline PDKs use Magic"
    );
    // The GF180 technology LEF must say that its parasitics are placeholders, the SKY130 one
    // must not: the other PDK's LEF is refused by each entry.
    let sky_lef = format!(
        "{pdk_root}/sky130/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/sky130A/libs.ref/\
         sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef"
    );
    let marker = rc_profile::build(&Request {
        technology_lef: Some(&sky_lef),
        ..request.clone()
    })
    .unwrap_err();
    assert_eq!(
        marker.message,
        "gf180: LEF parasitic validity marker disagrees with the PDK profile table"
    );
    // A second Magic file is only read where the table entry has one.
    let extract = rc_profile::build(&Request {
        magic_extract: Some(&sky_lef),
        ..request
    })
    .unwrap_err();
    assert!(
        extract
            .message
            .starts_with("gf180: the Magic technology file holds the extract"),
        "{extract}"
    );
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
    let ihp = layer_map::ihp_layers(&layer_map::IHP_SG13G2_LAYERS);
    let map = layer_map::project(
        &root.0.join("layers.map"),
        "ihp-sg13g2",
        "ez130_8t_sram",
        &ihp,
        Form::Ihp,
    )
    .unwrap();
    let expected = text(&fixtures().join("physical/expected/layer-map.json"));
    assert_eq!(
        dumps(&map, Layout::Indent(2)).unwrap() + "\n",
        instantiate(&expected, &root)
    );
    let failure = layer_map::project(
        &root.0.join("layers.map"),
        "p",
        "l",
        &layer_map::ihp_layers(&["Metal9"]),
        Form::Ihp,
    )
    .unwrap_err();
    assert_eq!(
        failure.to_string(),
        "LAYER_MAP: missing mapped layer Metal9"
    );
    let source = text(&root.0.join("layers.map"));
    std::fs::write(root.0.join("two.map"), source + "DIEAREA ALL 1 2\n").unwrap();
    let failure =
        layer_map::project(&root.0.join("two.map"), "p", "l", &ihp, Form::Ihp).unwrap_err();
    assert_eq!(failure.message, "unique die boundary map required");
}

/// The first lines of the SKY130 KLayout LEF/DEF layer map: lower-case cut names, a
/// `NAME <layer>/LABEL,<layer>/LEFPIN` label row, purposes the projection does not read.
const SKY130_MAP: &str = "\
li1     NET,SPNET,VIA            67  20
li1     LEFPIN,PIN               67  16
li1     LEFOBS                   67  5
NAME    li1/LABEL,li1/LEFPIN     67  5

mcon    LEFOBS                   67  44
mcon    LEFPIN                   67  48
NAME    mcon/LABEL,mcon/LEFPIN   67  48

mcon    VIA,LEFPIN,PIN           67  44
mcon    LEFPIN,PIN               67  48

met1    LEFPIN,NET,SPNET,PIN,VIA 68  20
met1    LEFPIN,PIN               68  16
met1    LEFOBS                   68  4
NAME    met1/LABEL,met1/LEFPIN   68  5

DIEAREA ALL                      235 4
";

const SKY130_LEF: &str = "\
LAYER nwell
  TYPE MASTERSLICE ;
END nwell
LAYER li1
  TYPE ROUTING ;   # local interconnect
  DIRECTION VERTICAL ;
END li1
LAYER mcon
  TYPE CUT ;
END mcon
LAYER met1
  TYPE ROUTING ;
END met1
END LIBRARY
";

/// The projection of another PDK's map: cut layers come from the technology LEF, the label
/// row is `NAME <layer>/LABEL`, and the entries have the form of the tracked
/// `fixtures/pdk/sky130-gds-map.json` (its `layers` entries without the Magic citation).
#[test]
fn layer_map_projection_reads_the_maps_of_other_pdks() {
    let root = Root::new("pdk-map");
    let (map_path, lef_path) = (root.0.join("sky130A.map"), root.0.join("tech.lef"));
    std::fs::write(&map_path, SKY130_MAP).unwrap();
    std::fs::write(&lef_path, SKY130_LEF).unwrap();
    let layers = layer_map::lef_layers(&lef_path, &["li1", "mcon", "met1"]).unwrap();
    assert_eq!(
        layers.iter().map(|layer| layer.cut).collect::<Vec<_>>(),
        [false, true, false]
    );
    let map =
        layer_map::project(&map_path, "sky130A", "sky130_fd_sc_hd", &layers, Form::Pdk).unwrap();
    let sha = FileId::of(&map_path, 1 << 20).unwrap().sha256;
    let expected = format!(
        r#"[{{"lef": "li1", "kind": "routing", "net": [[67, 20]], "special_net": [[67, 20]], "via": [[67, 20]], "pin": [[67, 16]], "label": [67, 5], "cite": [{{"source": "klayout_map", "sha256": "{sha}", "lines": [1, 2, 4]}}]}}, {{"lef": "mcon", "kind": "cut", "via": [[67, 44]], "cite": [{{"source": "klayout_map", "sha256": "{sha}", "lines": [10]}}]}}, {{"lef": "met1", "kind": "routing", "net": [[68, 20]], "special_net": [[68, 20]], "via": [[68, 20]], "pin": [[68, 20], [68, 16]], "label": [68, 5], "cite": [{{"source": "klayout_map", "sha256": "{sha}", "lines": [13, 14, 16]}}]}}]"#
    );
    assert_eq!(
        dumps(map.get("layers").unwrap(), Layout::Line).unwrap(),
        expected
    );
    assert_eq!(
        dumps(map.get("boundary").unwrap(), Layout::Line).unwrap(),
        format!(
            r#"{{"layer": [235, 4], "cite": [{{"source": "klayout_map", "sha256": "{sha}", "lines": [18]}}]}}"#
        )
    );
    assert_eq!(map.get("pdk").and_then(Value::as_str), Some("sky130A"));
    // The IHP form does not read this map: its label rows are `NAME <layer>/PIN`.
    let ihp = layer_map::project(&map_path, "p", "l", &layers, Form::Ihp).unwrap_err();
    assert_eq!(ihp.message, "incomplete mapped geometry: li1");
    // A layer the technology LEF does not declare as routing or cut is refused.
    for name in ["nwell", "met9"] {
        let failure = layer_map::lef_layers(&lef_path, &[name]).unwrap_err();
        assert_eq!(
            failure.to_string(),
            format!("LAYER_MAP: {name} is not a routing or cut layer of the technology LEF")
        );
    }
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

fn record(path: &Path) -> Value {
    FileId::of(path, 1 << 30).unwrap().to_value()
}

/// The step for a PDK profile (`rusteda.native-physical-inputs.v3`): any number of GDS
/// libraries, files recognised by their pinned bytes only, a projected or a tracked layer
/// map, a built or a tracked RC profile, and the profile written as `rc-nom.json`.
#[test]
fn physical_inputs_step_serves_any_pdk_profile() {
    let root = physical_root("profile-step");
    copy_tree(&fixtures().join("pdk-profiles/inputs"), &root.0);
    let sky = root
        .0
        .join("pdk/sky130/versions/f6eeac7dad085ffcc829ccfd721f7b4ce39edcf7/sky130A");
    // The files of a PDK tree under names and in places no table knows.
    let files = [
        ("std_gds", root.0.join("cells-a.gds")),
        ("ram_gds", root.0.join("cells-b.gds")),
        ("tech_lef", root.0.join("anywhere/technology.lef")),
        ("layer_map", root.0.join("anywhere/layers.txt")),
        ("rules_nom", root.0.join("anywhere/nominal.rules")),
        ("magic_tech", root.0.join("anywhere/magic.tech")),
    ];
    std::fs::create_dir_all(root.0.join("anywhere")).unwrap();
    for (from, to) in [
        (
            sky.join("libs.ref/sky130_fd_sc_hd/techlef/sky130_fd_sc_hd__nom.tlef"),
            &files[2].1,
        ),
        (
            sky.join("libs.tech/librelane/rules.openrcx.sky130A.nom.calibre"),
            &files[4].1,
        ),
        (sky.join("libs.tech/magic/sky130A.tech"), &files[5].1),
    ] {
        std::fs::copy(from, to).unwrap();
    }
    let mut map = String::from("DIEAREA ALL 235 4\n");
    for (index, name) in ["li1", "met1", "met2", "met3", "met4", "met5"]
        .into_iter()
        .enumerate()
    {
        let layer = 67 + index;
        map += &format!("{name} NET,SPNET,VIA,PIN {layer} 20\nNAME {name}/LABEL {layer} 5\n");
    }
    for (index, name) in ["mcon", "via", "via2", "via3", "via4"]
        .into_iter()
        .enumerate()
    {
        map += &format!("{name} VIA {} 44\n", 67 + index);
    }
    std::fs::write(&files[3].1, map).unwrap();
    let mut records = Dict::new();
    for (name, path) in &files {
        records.insert(*name, record(path));
    }
    let layers = [
        "li1", "mcon", "met1", "via", "met2", "via2", "met3", "via3", "met4", "via4", "met5",
    ];
    let config = |files: &Dict, layers: &[&str], rc: Dict| {
        Value::Dict(
            Dict::new()
                .with("schema", "rusteda.native-physical-inputs.v3")
                .with("pdk", "sky130A")
                .with("library", "sky130_fd_sc_hd")
                .with("cell_gds", list(["std_gds", "ram_gds"]))
                .with("layers", list(layers.iter().copied()))
                .with("rc", rc)
                .with("files", files.clone()),
        )
    };
    let built = || {
        Dict::new()
            .with("builder", "sky130")
            .with("resistance_model", "magic")
    };
    let write = |name: &str, value: &Value| {
        let path = root.0.join(name);
        std::fs::write(&path, dumps(value, Layout::Indent(2)).unwrap()).unwrap();
        path
    };
    let inputs = write("profile-inputs.json", &config(&records, &layers, built()));
    let out = root.0.join("profile-step");
    let stdout = physical_inputs::run(&inputs, &out).unwrap();
    // The bundle is the reference bundle of the two libraries.
    assert_eq!(
        std::fs::read(out.join("combined-cells.gds")).unwrap(),
        std::fs::read(fixtures().join("physical/expected/step/combined-cells.gds")).unwrap()
    );
    // The profile is the reference builder's for these bytes; only the source paths differ.
    let expected = instantiate(
        &text(&fixtures().join("pdk-profiles/expected/sky130-rc-nom.json")),
        &root,
    );
    let mut expected = parse(&expected).unwrap();
    let built_profile = parse(&text(&out.join("rc-nom.json"))).unwrap();
    let paths = |profile: &Value| -> Vec<(String, String)> {
        profile
            .get("sources")
            .and_then(Value::as_list)
            .unwrap()
            .iter()
            .map(|source| {
                (
                    source
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_owned(),
                    source
                        .get("sha256")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_owned(),
                )
            })
            .collect()
    };
    assert_eq!(paths(&built_profile), paths(&expected));
    if let Value::Dict(dict) = &mut expected {
        dict.insert("sources", built_profile.get("sources").cloned());
    }
    assert_eq!(built_profile, expected);
    assert!(!out.join("ihp-rc-nom.json").exists());
    let map = parse(&text(&out.join("gds-map.json"))).unwrap();
    assert_eq!(map.get("pdk").and_then(Value::as_str), Some("sky130A"));
    let kinds: Vec<&str> = map
        .get("layers")
        .and_then(Value::as_list)
        .unwrap()
        .iter()
        .map(|layer| layer.get("kind").and_then(Value::as_str).unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "routing", "cut", "routing", "cut", "routing", "cut", "routing", "cut", "routing",
            "cut", "routing"
        ]
    );
    let result = parse(&text(&out.join("result.json"))).unwrap();
    assert_eq!(dumps(&result, Layout::Line).unwrap(), stdout);
    assert_eq!(
        result.get("status").and_then(Value::as_str),
        Some("prepared_not_qualified")
    );
    assert_eq!(result.get("inputs"), Some(&Value::Dict(records.clone())));
    for (name, file) in [
        ("cell_gds", "combined-cells.gds"),
        ("gds_map", "gds-map.json"),
        ("rc_nom", "rc-nom.json"),
    ] {
        assert_eq!(
            result.get("artifacts").and_then(|a| a.get(name)),
            Some(&record(&out.join(file))),
            "{name}"
        );
    }
    assert_eq!(text(&out.join("HEAD")), "rev1\n");

    // Tracked artifacts instead of the PDK sources: both are copied byte for byte.
    let mut tracked = Dict::new();
    for name in ["std_gds", "ram_gds", "tech_lef"] {
        tracked.insert(name, records.get(name).cloned());
    }
    tracked.insert("gds_map", record(&out.join("gds-map.json")));
    tracked.insert("rc_profile", record(&out.join("rc-nom.json")));
    let inputs = write("tracked-inputs.json", &config(&tracked, &[], Dict::new()));
    let copy = root.0.join("tracked-step");
    physical_inputs::run(&inputs, &copy).unwrap();
    for name in ["combined-cells.gds", "gds-map.json", "rc-nom.json"] {
        assert_eq!(
            std::fs::read(copy.join(name)).unwrap(),
            std::fs::read(out.join(name)).unwrap(),
            "{name}"
        );
    }

    // Refusals: each names what is wrong and leaves nothing but the lock.
    let refuse = |tag: &str, value: &Value, message: &str| {
        let inputs = write(&format!("{tag}.json"), value);
        let dir = root.0.join(tag);
        let failure = physical_inputs::run(&inputs, &dir).unwrap_err();
        assert_eq!(failure.message, message, "{tag}");
        assert!(!dir.join("HEAD").exists(), "{tag}");
    };
    let without = |name: &str| {
        let mut files = records.clone();
        files.remove(name);
        files
    };
    refuse(
        "no-rules",
        &config(&without("rules_nom"), &layers, built()),
        "explicit PDK input set required",
    );
    let mut extra = records.clone();
    extra.insert("notes", record(&files[3].1));
    refuse(
        "extra-file",
        &config(&extra, &layers, built()),
        "explicit PDK input set required",
    );
    let mut one = config(&records, &layers, built());
    if let Value::Dict(dict) = &mut one {
        dict.insert("cell_gds", list(["std_gds"]));
    }
    refuse("one-gds", &one, "cell_gds must list every pinned GDS file");
    refuse(
        "unknown-layer",
        &config(&records, &["li1", "poly"], built()),
        "poly is not a routing or cut layer of the technology LEF",
    );
    refuse(
        "other-builder",
        &config(
            &records,
            &layers,
            Dict::new()
                .with("builder", "gf180")
                .with("resistance_model", "magic"),
        ),
        "gf180: LEF parasitic validity marker disagrees with the PDK profile table",
    );
    // A tracked profile of another technology LEF is refused.
    let mut other = tracked.clone();
    other.insert(
        "tech_lef",
        record(
            &root
                .0
                .join("pdk/ihp-sg13g2/libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef"),
        ),
    );
    refuse(
        "other-lef",
        &config(&other, &[], Dict::new()),
        "rc_profile was built for another technology LEF",
    );
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
    let sizes = lef::sizes(&text(&postroute("inputs/cells.lef")), 1000).unwrap();
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
    assert!(lef::sizes("MACRO A\n SIZE 1.2.3 BY 1 ;\nEND A\n", 1000).is_err());
    // Sizes are in the database unit of the flow (2000 per micron on GF180MCU).
    assert_eq!(
        lef::sizes("MACRO A\n SIZE 0.56 BY 3.92 ;\nEND A\n", 2000).unwrap(),
        [("A".to_owned(), (1120, 7840))]
    );
}

#[test]
fn sizing_families_and_repair_plans_equal_the_reference_policy() {
    let libs = libraries();
    let sizes = lef::sizes(&text(&postroute("inputs/cells.lef")), 1000).unwrap();
    let classes = families(&libs, &sizes).unwrap();
    assert_eq!(
        dumps(
            &list(
                classes
                    .list
                    .iter()
                    .map(|family| family.to_value("TT", "SS"))
            ),
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
