//! File-only flow steps (`rustroad_flow::steps`): no RustROAD workspace is
//! opened by this process; a step that needs engine work runs the pinned
//! executables named in its inputs.
use clap::Subcommand;
use rustroad_flow::steps;
use rustroad_pyjson::{Dict, Layout, Value, dumps};
use std::io::Write;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum StepCommand {
    /// Flow step: bundle the PDK cell GDS, project the GDS layer map and
    /// build the nominal RC profile from pinned PDK files.
    PhysicalInputs {
        /// `rusteda.native-physical-inputs.v2` (IHP SG13G2/EZ130) or `.v3`
        /// (any PDK profile) JSON.
        #[arg(long)]
        inputs: PathBuf,
        /// Fresh output directory (created; REVISION_CONFLICT if used).
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Flow step: screen the routed, antenna-repaired, extracted layout for
    /// max-capacitance/max-transition violations, repair them by equal-width
    /// upsizing with ECO rebuilds, and publish the final workspace and SPEF.
    /// Runs the pinned rustroad, rusteda and ruststa named in the inputs.
    PostrouteRepair {
        /// `rusteda.native-postroute-inputs.v3` JSON.
        #[arg(long)]
        inputs: PathBuf,
        /// Output directory; must not exist (REVISION_CONFLICT otherwise).
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Flow step: run one `repair-antenna-groups` session of the pinned tool
    /// as a captured child process and write its lineage records.
    AntennaSession {
        /// `rusteda.native-antenna-inputs.v2` JSON.
        #[arg(long)]
        inputs: PathBuf,
        /// Fresh output directory (created; REVISION_CONFLICT if used).
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Flow step: independently re-check an antenna-session lineage against
    /// the files it names (metadata review; never runs an EDA command).
    VerifyAntennaLineage {
        /// `rusteda.antenna-v3-independent-verifier-inputs.v1` or `.v2` JSON.
        #[arg(long)]
        inputs: PathBuf,
        /// Fresh output directory (created; REVISION_CONFLICT if used).
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Merge cell GDS libraries into one stream; structures are copied byte
    /// for byte, units must agree, nothing is rescaled.
    BundleCellGds {
        /// Source library (repeatable; the first one gives the header).
        #[arg(long = "gds", required = true)]
        gds: Vec<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Project a KLayout LEF/DEF layer map onto `rustroad.gds-layer-map.v1`.
    GdsLayerMap {
        /// `<layer> <purposes> <gds layer> <datatype>` map of the PDK.
        #[arg(long)]
        map: PathBuf,
        #[arg(long, default_value = "ihp-sg13g2")]
        pdk: String,
        #[arg(long, default_value = "ez130_8t_sram")]
        library: String,
        /// LEF layer to export, bottom to top (repeatable; default: the IHP
        /// SG13G2 stack Metal1..TopMetal2). Without `--technology-lef` a
        /// name containing `Via` is a cut.
        #[arg(long = "layer")]
        layers: Vec<String>,
        /// Technology LEF that declares the layers: a layer is a cut when
        /// its `TYPE` is `CUT`. With it the map may label layers with
        /// `NAME <layer>/LABEL` rows and `--layer` is required.
        #[arg(long)]
        technology_lef: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a `rustroad.rc-profile.v1` extraction profile from PDK data files.
    RcProfile {
        /// The table entry: which PDK's files, layers and Magic names.
        #[arg(long, value_parser = steps::rc_profile::PDKS, default_value = "ihp")]
        pdk: String,
        #[arg(long, value_parser = ["min", "nom", "max"])]
        corner: String,
        /// Directory that holds the PDK tree of the entry (`ihp-sg13g2/`,
        /// `sky130/versions/<version>/sky130A/`,
        /// `gf180mcu/versions/<version>/gf180mcuD/`). Not read when
        /// `--rules`, `--technology-lef` and `--magic` (and, for `ihp`,
        /// `--magic-extract`) are absolute paths.
        #[arg(long)]
        pdk_root: PathBuf,
        /// `openrcx-pdk` is qualified for `ihp` only.
        #[arg(long, value_parser = ["magic", "openrcx-pdk"], default_value = "magic")]
        resistance_model: String,
        /// Calibration table instead of the PDK's per-corner rules.
        #[arg(long)]
        rules: Option<PathBuf>,
        /// Technology LEF instead of the PDK standard-cell technology LEF.
        #[arg(long)]
        technology_lef: Option<PathBuf>,
        /// Magic technology file instead of the PDK's.
        #[arg(long)]
        magic: Option<PathBuf>,
        /// Second Magic file with the `extract` section instead of the
        /// PDK's (`ihp` only).
        #[arg(long)]
        magic_extract: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
}

fn print(text: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    output.write_all(text.as_bytes())?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn pretty(value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    print(&dumps(value, Layout::Indent(2)).ok_or("STEP_JSON: non-finite number")?)
}

fn text(path: &std::path::Path) -> Result<&str, Box<dyn std::error::Error>> {
    path.to_str()
        .ok_or_else(|| "INVALID_ARGUMENT: path is not UTF-8".into())
}

fn output_id(path: &std::path::Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(rustroad_pyjson::FileId::of(path, 1 << 30)
        .map_err(|message| format!("STEP_IO: {message}"))?
        .to_value())
}

pub fn run(command: StepCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        StepCommand::PhysicalInputs { inputs, out_dir } => {
            print(&steps::physical_inputs::run(&inputs, &out_dir)?)
        }
        StepCommand::PostrouteRepair { inputs, out_dir } => {
            print(&steps::postroute::run(&inputs, &out_dir)?)
        }
        StepCommand::AntennaSession { inputs, out_dir } => {
            print(&steps::antenna_session::run(&inputs, &out_dir)?)
        }
        StepCommand::VerifyAntennaLineage { inputs, out_dir } => {
            match rustroad_antenna_verify::run(&inputs, &out_dir) {
                Ok(review) => print(&review),
                Err(rejected) if rejected.0 == "REVISION_CONFLICT" => {
                    Err("REVISION_CONFLICT: review output directory must be fresh".into())
                }
                Err(rejected) => Err(format!("ANTENNA_LINEAGE: {rejected}").into()),
            }
        }
        StepCommand::BundleCellGds { gds, out } => pretty(&steps::gds_bundle::bundle(&gds, &out)?),
        StepCommand::GdsLayerMap {
            map,
            pdk,
            library,
            layers,
            technology_lef,
            out,
        } => {
            use steps::layer_map::{Form, IHP_SG13G2_LAYERS, ihp_layers, lef_layers, project};
            let names: Vec<&str> = if layers.is_empty() {
                IHP_SG13G2_LAYERS.to_vec()
            } else {
                layers.iter().map(String::as_str).collect()
            };
            let profile = match &technology_lef {
                Some(_) if layers.is_empty() => {
                    return Err("INVALID_ARGUMENT: --technology-lef requires --layer".into());
                }
                Some(lef) => project(&map, &pdk, &library, &lef_layers(lef, &names)?, Form::Pdk)?,
                None => project(&map, &pdk, &library, &ihp_layers(&names), Form::Ihp)?,
            };
            let mut rendered =
                dumps(&profile, Layout::Indent(2)).ok_or("STEP_JSON: non-finite number")?;
            rendered.push('\n');
            std::fs::write(&out, rendered)?;
            pretty(&Value::Dict(
                Dict::new()
                    .with("schema", "rustroad.gds-layer-map-result.v1")
                    .with("output", output_id(&out)?)
                    .with("layers", names.len()),
            ))
        }
        StepCommand::RcProfile {
            pdk,
            corner,
            pdk_root,
            resistance_model,
            rules,
            technology_lef,
            magic,
            magic_extract,
            out,
        } => {
            let rules = rules.as_deref().map(text).transpose()?;
            let technology_lef = technology_lef.as_deref().map(text).transpose()?;
            let magic = magic.as_deref().map(text).transpose()?;
            let magic_extract = magic_extract.as_deref().map(text).transpose()?;
            let profile = steps::rc_profile::build(&steps::rc_profile::Request {
                pdk: &pdk,
                corner: &corner,
                pdk_root: text(&pdk_root)?,
                model: steps::rc_profile::ResistanceModel::parse(&resistance_model)?,
                rules_override: rules,
                technology_lef,
                magic,
                magic_extract,
            })?;
            std::fs::write(&out, steps::rc_profile::render(&profile)?)?;
            pretty(&Value::Dict(
                Dict::new()
                    .with("schema", "rustroad.rc-profile-result.v1")
                    .with("pdk", pdk)
                    .with("corner", corner)
                    .with("output", output_id(&out)?)
                    .with("sources", profile.get("sources").cloned()),
            ))
        }
    }
}
