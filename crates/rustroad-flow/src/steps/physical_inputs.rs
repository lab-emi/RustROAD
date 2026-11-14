//! The `be_profiles` step: cell GDS bundle, GDS layer map and the nominal RC
//! profile of a flow, from pinned original PDK files.
//!
//! Two inputs are accepted.
//!
//! `rusteda.native-physical-inputs.v2` is the Ibex SG13G2/EZ130 recipe:
//! `{schema, pdk_root, files}` with exactly seven file records
//! (`{path, sha256, bytes}`), each re-hashed before and after the work. Output
//! directory (must be fresh): `combined-cells.gds`, `gds-inputs.json`,
//! `gds-map.json`, `ihp-rc-nom.json`, `result.json`, `HEAD` and
//! `workspace.lock`. The v1 schema of the Python helper also pinned
//! `rc_builder`, the profile script it imported; the native step builds the
//! profile itself, so that one record is gone and everything else is unchanged.
//!
//! `rusteda.native-physical-inputs.v3` is the same step for any PDK profile:
//! `{schema, pdk, library, cell_gds, layers, rc, files}`. `cell_gds` lists the
//! file keys of the GDS libraries to bundle in order (one or more: cells, then
//! macros). The layer map is either `files.layer_map` (a KLayout LEF/DEF map,
//! projected onto `layers`, whose kinds come from `files.tech_lef`) or
//! `files.gds_map` (a tracked `rustroad.gds-layer-map.v1`, copied). The RC
//! profile is either built from `files.rules_nom`, `files.magic_tech` and,
//! where the table entry has one, `files.magic_extract` with
//! `rc: {builder, resistance_model}`, or `files.rc_profile` (a tracked
//! `rustroad.rc-profile.v1` for this technology LEF, copied). No file is
//! recognised by its name or place in a PDK tree: the records pin the bytes,
//! and the built profile must name exactly the pinned files as its sources.
//! The RC profile is written as `rc-nom.json`.
//!
//! No historical layout or optimization plan is an input, and nothing here is
//! qualified: the result status is `prepared_not_qualified`.
//!
//! Caps: 32 MiB per GDS, 2 MiB per other input (see the three modules).
use super::layer_map::{Form, IHP_SG13G2_LAYERS, ihp_layers, lef_layers};
use super::rc_profile::{Request, ResistanceModel};
use super::{
    FreshDir, StepResult, dict_of, fail, file_id, gds_bundle, key, layer_map, path_text,
    rc_profile, read_json, render, require, resolve_lenient, str_of, write_json,
};
use rustroad_pyjson::{Dict, FileId, Layout, Value, list};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const CODE: &str = "PHYSICAL_INPUTS";
const SCHEMA: &str = "rusteda.native-physical-inputs.v2";
const SCHEMA_PROFILE: &str = "rusteda.native-physical-inputs.v3";
const KEYS: [&str; 7] = [
    "std_gds",
    "ram_gds",
    "layer_map",
    "tech_lef",
    "rules_nom",
    "magic_tech",
    "magic_extract",
];

fn cap(name: &str) -> u64 {
    if name.ends_with("_gds") {
        32 << 20
    } else {
        2 << 20
    }
}

fn verify(files: &Dict, phase: &str) -> StepResult<()> {
    for (name, record) in files.iter() {
        let expected = FileId::from_value(record)
            .ok_or_else(|| fail(CODE, format!("file identity schema: {name}")))?;
        let actual = file_id(Path::new(&expected.path), u64::MAX, CODE)?;
        require(
            actual == expected,
            CODE,
            format!("input changed{phase}: {name}"),
        )?;
        require(
            actual.bytes <= cap(name),
            CODE,
            format!("input byte budget: {name}"),
        )?;
    }
    Ok(())
}

/// Run the step and return the text of its one-line result
/// (`json.dumps(result)`, also stored indented in `result.json`).
///
/// # Errors
/// `REVISION_CONFLICT` for a used output directory, `PHYSICAL_INPUTS` for a
/// schema, identity or PDK-root mismatch, and the errors of the three builders.
pub fn run(inputs: &Path, out_dir: &Path) -> StepResult<String> {
    let dir = FreshDir::claim(out_dir)?;
    let output = dir.path.as_path();
    let config = read_json(inputs, 2 << 20, CODE)?;
    let config = dict_of(&config, CODE, "inputs")?;
    if config.get("schema").and_then(Value::as_str) == Some(SCHEMA_PROFILE) {
        return profile(config, &dir);
    }
    require(
        config.has_exactly(&["schema", "files", "pdk_root"])
            && config.get("schema").and_then(Value::as_str) == Some(SCHEMA),
        CODE,
        "input schema",
    )?;
    let files = dict_of(key(config, "files", CODE)?, CODE, "files")?;
    require(
        files.has_exactly(&KEYS),
        CODE,
        "explicit PDK input set required",
    )?;
    verify(files, "")?;
    let path = |name: &str| -> StepResult<PathBuf> {
        let record = dict_of(key(files, name, CODE)?, CODE, name)?;
        Ok(PathBuf::from(str_of(
            key(record, "path", CODE)?,
            CODE,
            name,
        )?))
    };
    let resolved = |name: &str| -> StepResult<String> {
        path_text(&std::fs::canonicalize(path(name)?)?, CODE)
    };
    let root = std::fs::canonicalize(str_of(key(config, "pdk_root", CODE)?, CODE, "pdk_root")?)?;
    let pdk = root.join("ihp-sg13g2");
    let choices: Vec<PathBuf> = ["IHP_rcx_patterns.rules", "ihp-sg13g2.nom.magic.rules"]
        .iter()
        .map(|name| resolve_lenient(&pdk.join("libs.tech/librelane/openrcx").join(name)))
        .collect();
    require(
        choices.contains(&resolve_lenient(&path("rules_nom")?))
            && resolve_lenient(&path("magic_tech")?)
                == resolve_lenient(&pdk.join("libs.tech/magic/ihp-sg13g2.tech"))
            && resolve_lenient(&path("magic_extract")?)
                == resolve_lenient(&pdk.join("libs.tech/magic/ihp-sg13g2-extract.tech")),
        CODE,
        "RC PDK root differs from pinned data",
    )?;
    let bundle = gds_bundle::bundle(
        &[path("std_gds")?, path("ram_gds")?],
        &output.join("combined-cells.gds"),
    )?;
    write_json(
        &output.join("gds-inputs.json"),
        &bundle,
        Layout::Indent(2),
        CODE,
    )?;
    let map = layer_map::project(
        &path("layer_map")?,
        "ihp-sg13g2",
        "ez130_8t_sram",
        &ihp_layers(&IHP_SG13G2_LAYERS),
        Form::Ihp,
    )?;
    write_json(&output.join("gds-map.json"), &map, Layout::Indent(2), CODE)?;
    // Both original calibrations are explicit, auditable inputs. The benchmark
    // contract selects the PDK default patterns; the pinned file decides.
    let (rules, lef) = (resolved("rules_nom")?, resolved("tech_lef")?);
    let profile = rc_profile::build(&Request {
        pdk: "ihp",
        corner: "nom",
        pdk_root: &path_text(&root, CODE)?,
        model: ResistanceModel::OpenRcxPdk,
        rules_override: Some(&rules),
        technology_lef: Some(&lef),
        magic: None,
        magic_extract: None,
    })?;
    let mut expected = BTreeSet::new();
    for name in ["rules_nom", "magic_tech", "magic_extract", "tech_lef"] {
        expected.insert(file_id(&path(name)?, 2 << 20, CODE)?.sha256);
    }
    let recorded: BTreeSet<String> = profile
        .get("sources")
        .and_then(Value::as_list)
        .unwrap_or_default()
        .iter()
        .filter_map(|source| source.get("sha256").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    require(recorded == expected, CODE, "RC profile sources differ")?;
    std::fs::write(
        output.join("ihp-rc-nom.json"),
        rc_profile::render(&profile)?,
    )?;
    verify(files, " during preparation")?;
    let mut artifacts = Dict::new();
    for (name, file) in [
        ("cell_gds", "combined-cells.gds"),
        ("gds_map", "gds-map.json"),
        ("rc_nom", "ihp-rc-nom.json"),
    ] {
        artifacts.insert(
            name,
            file_id(&output.join(file), 64 << 20, CODE)?.to_value(),
        );
    }
    let result = Value::Dict(
        Dict::new()
            .with("status", "prepared_not_qualified")
            .with("inputs", Value::Dict(files.clone()))
            .with("artifacts", artifacts)
            .with(
                "unverified",
                list([
                    "layout",
                    "routing",
                    "DRC",
                    "antenna",
                    "LVS",
                    "extracted_timing",
                    "benchmark",
                ]),
            ),
    );
    write_json(
        &output.join("result.json"),
        &result,
        Layout::Indent(2),
        CODE,
    )?;
    dir.publish()?;
    render(&result, Layout::Line, CODE)
}

/// `[A-Za-z0-9_.-]{1,64}`: a PDK or library name.
fn name_ok(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The step for a PDK profile (`rusteda.native-physical-inputs.v3`).
fn profile(config: &Dict, dir: &FreshDir) -> StepResult<String> {
    let output = dir.path.as_path();
    require(
        config.has_exactly(&[
            "schema", "pdk", "library", "cell_gds", "layers", "rc", "files",
        ]),
        CODE,
        "input schema",
    )?;
    let pdk = str_of(key(config, "pdk", CODE)?, CODE, "pdk")?;
    let library = str_of(key(config, "library", CODE)?, CODE, "library")?;
    require(
        name_ok(pdk) && name_ok(library),
        CODE,
        "pdk and library names",
    )?;
    let files = dict_of(key(config, "files", CODE)?, CODE, "files")?;
    require(
        (2..=64).contains(&files.len()),
        CODE,
        "explicit PDK input set required",
    )?;
    verify(files, "")?;
    let path = |name: &str| -> StepResult<PathBuf> {
        let record = dict_of(key(files, name, CODE)?, CODE, name)?;
        Ok(PathBuf::from(str_of(
            key(record, "path", CODE)?,
            CODE,
            name,
        )?))
    };
    let resolved = |name: &str| -> StepResult<String> {
        path_text(&std::fs::canonicalize(path(name)?)?, CODE)
    };
    // The GDS libraries, in the order given: every `*_gds` record, each once.
    let listed = key(config, "cell_gds", CODE)?
        .as_list()
        .ok_or_else(|| fail(CODE, "cell_gds: list required"))?;
    let mut gds_keys: Vec<&str> = Vec::new();
    for item in listed {
        let name = str_of(item, CODE, "cell_gds")?;
        require(
            name.ends_with("_gds") && files.contains_key(name) && !gds_keys.contains(&name),
            CODE,
            format!("cell_gds names no pinned GDS file: {name}"),
        )?;
        gds_keys.push(name);
    }
    let pinned_gds = files.keys().filter(|name| name.ends_with("_gds")).count();
    require(
        !gds_keys.is_empty() && gds_keys.len() == pinned_gds,
        CODE,
        "cell_gds must list every pinned GDS file",
    )?;
    // What the two derived artifacts are made from decides the other records.
    let projected = files.contains_key("layer_map");
    let rc = dict_of(key(config, "rc", CODE)?, CODE, "rc")?;
    let built = !files.contains_key("rc_profile");
    let mut expected: Vec<&str> = vec!["tech_lef"];
    expected.push(if projected { "layer_map" } else { "gds_map" });
    let mut builder = None;
    if built {
        require(
            rc.has_exactly(&["builder", "resistance_model"]),
            CODE,
            "rc: builder and resistance_model required",
        )?;
        let tag = str_of(key(rc, "builder", CODE)?, CODE, "rc.builder")?;
        let model = ResistanceModel::parse(str_of(
            key(rc, "resistance_model", CODE)?,
            CODE,
            "rc.resistance_model",
        )?)?;
        expected.extend(["rules_nom", "magic_tech"]);
        if files.contains_key("magic_extract") {
            expected.push("magic_extract");
        }
        builder = Some((tag, model));
    } else {
        require(
            rc.is_empty(),
            CODE,
            "rc: a tracked profile takes no builder",
        )?;
        expected.push("rc_profile");
    }
    require(
        files
            .keys()
            .all(|name| name.ends_with("_gds") || expected.contains(&name))
            && expected.iter().all(|name| files.contains_key(name)),
        CODE,
        "explicit PDK input set required",
    )?;
    let sources: Vec<PathBuf> = gds_keys
        .iter()
        .map(|name| path(name))
        .collect::<StepResult<_>>()?;
    let bundle = gds_bundle::bundle(&sources, &output.join("combined-cells.gds"))?;
    write_json(
        &output.join("gds-inputs.json"),
        &bundle,
        Layout::Indent(2),
        CODE,
    )?;
    let layers = key(config, "layers", CODE)?
        .as_list()
        .ok_or_else(|| fail(CODE, "layers: list required"))?;
    if projected {
        let names: Vec<&str> = layers
            .iter()
            .map(|item| str_of(item, CODE, "layers"))
            .collect::<StepResult<_>>()?;
        require(
            (1..=64).contains(&names.len()),
            CODE,
            "layers: one to 64 LEF layers required",
        )?;
        let map = layer_map::project(
            &path("layer_map")?,
            pdk,
            library,
            &lef_layers(&path("tech_lef")?, &names)?,
            Form::Pdk,
        )?;
        write_json(&output.join("gds-map.json"), &map, Layout::Indent(2), CODE)?;
    } else {
        require(
            layers.is_empty(),
            CODE,
            "layers: a tracked layer map takes none",
        )?;
        let source = path("gds_map")?;
        let map = read_json(&source, 2 << 20, CODE)?;
        require(
            map.get("schema").and_then(Value::as_str) == Some("rustroad.gds-layer-map.v1"),
            CODE,
            "gds_map: rustroad.gds-layer-map.v1 required",
        )?;
        std::fs::copy(&source, output.join("gds-map.json"))?;
    }
    let lef_sha256 = file_id(&path("tech_lef")?, 2 << 20, CODE)?.sha256;
    if let Some((tag, model)) = builder {
        let (rules, lef, magic) = (
            resolved("rules_nom")?,
            resolved("tech_lef")?,
            resolved("magic_tech")?,
        );
        let extract = if files.contains_key("magic_extract") {
            Some(resolved("magic_extract")?)
        } else {
            None
        };
        // Every file is given explicitly: the PDK root joins nothing.
        let profile = rc_profile::build(&Request {
            pdk: tag,
            corner: "nom",
            pdk_root: "/",
            model,
            rules_override: Some(&rules),
            technology_lef: Some(&lef),
            magic: Some(&magic),
            magic_extract: extract.as_deref(),
        })?;
        let mut pinned = BTreeSet::new();
        for name in expected
            .iter()
            .filter(|name| **name != "layer_map" && **name != "gds_map")
        {
            pinned.insert(file_id(&path(name)?, 2 << 20, CODE)?.sha256);
        }
        let recorded: BTreeSet<String> = profile
            .get("sources")
            .and_then(Value::as_list)
            .unwrap_or_default()
            .iter()
            .filter_map(|source| source.get("sha256").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        require(recorded == pinned, CODE, "RC profile sources differ")?;
        std::fs::write(output.join("rc-nom.json"), rc_profile::render(&profile)?)?;
    } else {
        let source = path("rc_profile")?;
        let tracked = read_json(&source, 2 << 20, CODE)?;
        require(
            tracked.get("schema").and_then(Value::as_str) == Some("rustroad.rc-profile.v1")
                && tracked.get("corner").and_then(Value::as_str) == Some("nom"),
            CODE,
            "rc_profile: a nominal rustroad.rc-profile.v1 required",
        )?;
        require(
            tracked.get("technology_lef_sha256").and_then(Value::as_str)
                == Some(lef_sha256.as_str()),
            CODE,
            "rc_profile was built for another technology LEF",
        )?;
        std::fs::copy(&source, output.join("rc-nom.json"))?;
    }
    verify(files, " during preparation")?;
    let mut artifacts = Dict::new();
    for (name, file) in [
        ("cell_gds", "combined-cells.gds"),
        ("gds_map", "gds-map.json"),
        ("rc_nom", "rc-nom.json"),
    ] {
        artifacts.insert(
            name,
            file_id(&output.join(file), 64 << 20, CODE)?.to_value(),
        );
    }
    let result = Value::Dict(
        Dict::new()
            .with("status", "prepared_not_qualified")
            .with("pdk", pdk)
            .with("library", library)
            .with("inputs", Value::Dict(files.clone()))
            .with("artifacts", artifacts)
            .with(
                "unverified",
                list([
                    "layout",
                    "routing",
                    "DRC",
                    "antenna",
                    "LVS",
                    "extracted_timing",
                    "benchmark",
                ]),
            ),
    );
    write_json(
        &output.join("result.json"),
        &result,
        Layout::Indent(2),
        CODE,
    )?;
    dir.publish()?;
    render(&result, Layout::Line, CODE)
}
