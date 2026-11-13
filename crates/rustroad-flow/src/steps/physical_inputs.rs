//! The `be_profiles` step: cell GDS bundle, GDS layer map and the nominal RC
//! profile of the Ibex SG13G2/EZ130 recipe, from pinned original PDK files.
//!
//! Input: `rusteda.native-physical-inputs.v2`, `{schema, pdk_root, files}`
//! with exactly the seven file records below (`{path, sha256, bytes}`), each
//! re-hashed before and after the work. Output directory (must be fresh):
//! `combined-cells.gds`, `gds-inputs.json`, `gds-map.json`, `ihp-rc-nom.json`,
//! `result.json`, `HEAD` and `workspace.lock`. No historical layout or
//! optimization plan is an input, and nothing here is qualified: the result
//! status is `prepared_not_qualified`.
//!
//! The v1 schema of the Python helper also pinned `rc_builder`, the profile
//! script it imported; the native step builds the profile itself, so that one
//! record is gone and everything else is unchanged.
//!
//! Caps: 32 MiB per GDS, 2 MiB per other input (see the three modules).
use super::layer_map::IHP_SG13G2_LAYERS;
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
        &IHP_SG13G2_LAYERS,
    )?;
    write_json(&output.join("gds-map.json"), &map, Layout::Indent(2), CODE)?;
    // Both original calibrations are explicit, auditable inputs. The benchmark
    // contract selects the PDK default patterns; the pinned file decides.
    let (rules, lef) = (resolved("rules_nom")?, resolved("tech_lef")?);
    let profile = rc_profile::build(&Request {
        corner: "nom",
        pdk_root: &path_text(&root, CODE)?,
        model: ResistanceModel::OpenRcxPdk,
        rules_override: Some(&rules),
        technology_lef: Some(&lef),
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
