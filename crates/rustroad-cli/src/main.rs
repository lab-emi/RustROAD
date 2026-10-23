use clap::{Parser, Subcommand};
use rustroad_core::Limits;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Bounded clean-room physical-design workspace")]
struct Cli {
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[arg(long, global = true, default_value_t = 2, value_parser = clap::value_parser!(u16).range(1..=64))]
    threads: u16,
    #[arg(long, global = true, default_value_t = 250_000)]
    max_cells: usize,
    #[arg(long, global = true, default_value_t = 100_000)]
    max_rows: usize,
    #[arg(long, global = true, default_value_t = 350_000)]
    max_expansions: usize,
    #[arg(long, global = true, default_value_t = 250_000)]
    max_nets: usize,
    #[arg(long, global = true, default_value_t = 1_000_000)]
    max_pins: usize,
    #[arg(long, global = true, default_value_t = 500_000)]
    max_aliases: usize,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Import the strict JSON physical-design subset into a new workspace.
    Init {
        #[arg(long)]
        base_rev: String,
        #[arg(long)]
        input: PathBuf,
    },
    /// Import real LEF technology/cells and a flat mapped Verilog netlist.
    ImportPdk {
        #[arg(long)]
        base_rev: String,
        #[arg(long)]
        tech_lef: PathBuf,
        #[arg(long)]
        cell_lef: PathBuf,
        #[arg(long)]
        netlist: PathBuf,
        #[arg(long)]
        floorplan: PathBuf,
        #[arg(long)]
        physical_config: Option<PathBuf>,
    },
    /// Deterministic, single-height fixed-order row placement (not global optimization).
    Place {
        #[arg(long)]
        base_rev: String,
    },
    /// Build a bounded level-balanced clock-buffer topology, then replace placement.
    ClockTree {
        #[arg(long)]
        base_rev: String,
        /// Strict primary-clock SDC; unsupported timing semantics fail explicitly.
        #[arg(long)]
        sdc: Option<PathBuf>,
    },
    /// Bounded obstacle-aware routing with explicit technology and search limits.
    Route {
        #[arg(long)]
        base_rev: String,
        #[arg(long)]
        config: PathBuf,
        #[arg(long, value_parser=["clock", "all"], default_value="clock")]
        scope: String,
    },
    /// Write checked routed geometry and scope as JSON.
    ExportRoutes,
    /// Read the current revision and honest analysis coverage.
    Inspect,
    /// Write placed DEF, including imported PINS/NETS when present.
    ExportDef,
    /// Write the imported logical netlist to stdout with canonical syntax.
    ExportVerilog {
        #[arg(long)]
        include_power: bool,
    },
    /// Write the linked connectivity/LEF geometry snapshot as JSON.
    ExportConnectivity,
    /// Inspect pinned external tools and PDK files; readiness is not signoff.
    FlowCheck {
        #[arg(long)]
        manifest: PathBuf,
    },
}

fn emit(value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = cli
        .workspace
        .ok_or("MISSING_WORKSPACE: --workspace <dir> is required")?;
    let limits = Limits {
        max_cells: cli.max_cells,
        max_rows: cli.max_rows,
        max_expansions: cli.max_expansions,
    };
    let connection_limits = rustroad_core::connectivity::ConnectivityLimits {
        max_instances: cli.max_cells,
        max_nets: cli.max_nets,
        max_pins: cli.max_pins,
        max_aliases: cli.max_aliases,
        ..Default::default()
    };
    match cli.command {
        Command::Init { base_rev, input } => {
            let design = rustroad_workspace::read_design(&input, &limits)?;
            emit(&rustroad_workspace::init(
                &workspace, &base_rev, design, &limits,
            )?)
        }
        Command::ImportPdk {
            base_rev,
            tech_lef,
            cell_lef,
            netlist,
            floorplan,
            physical_config,
        } => {
            let sources = rustroad_workspace::pdk::ImportSources {
                technology_lef: tech_lef,
                cell_lef,
                netlist,
                floorplan,
            };
            emit(&rustroad_workspace::pdk::import_pdk_with_config(
                &workspace,
                &base_rev,
                &sources,
                &limits,
                &connection_limits,
                physical_config.as_deref(),
            )?)
        }
        Command::Place { base_rev } => {
            emit(&rustroad_workspace::place(&workspace, &base_rev, &limits)?)
        }
        Command::ClockTree { base_rev, sdc } => {
            emit(&rustroad_workspace::clock::synthesize_with_sdc(
                &workspace,
                &base_rev,
                &limits,
                sdc.as_deref(),
            )?)
        }
        Command::Route {
            base_rev,
            config,
            scope,
        } => emit(&rustroad_workspace::routing::route(
            &workspace,
            &base_rev,
            &config,
            if scope == "clock" {
                rustroad_workspace::routing::RouteScope::Clock
            } else {
                rustroad_workspace::routing::RouteScope::All
            },
            &limits,
        )?),
        Command::ExportRoutes => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            emit(
                snapshot
                    .metadata()
                    .physical
                    .as_ref()
                    .and_then(|p| p.routing.as_ref())
                    .ok_or("NO_ROUTING: route first")?,
            )
        }
        Command::Inspect => emit(&rustroad_workspace::read(&workspace, &limits)?.summary()),
        Command::ExportDef => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            let stdout = io::stdout();
            let mut output = io::BufWriter::with_capacity(256 * 1024, stdout.lock());
            snapshot.export_def(&mut output)?;
            output.flush()?;
            Ok(())
        }
        Command::ExportVerilog { include_power } => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            let stdout = io::stdout();
            let mut output = io::BufWriter::with_capacity(256 * 1024, stdout.lock());
            snapshot.export_verilog_with_power(include_power, &mut output)?;
            output.flush()?;
            Ok(())
        }
        Command::ExportConnectivity => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            let model = snapshot
                .connectivity()
                .ok_or("NO_CONNECTIVITY: import a mapped netlist first")?;
            emit(model)
        }
        Command::FlowCheck { manifest } => {
            let _snapshot = rustroad_workspace::read(&workspace, &limits)?;
            emit(&rustroad_flow::assess_path(&manifest)?)
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(usize::from(cli.threads))
        .build();
    let outcome = match pool {
        Ok(pool) => pool.install(|| run(cli).map_err(|e| e.to_string())),
        Err(error) => Err(error.to_string()),
    };
    if let Err(message) = outcome {
        let code = message
            .split(':')
            .next()
            .filter(|s| s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'))
            .unwrap_or("RUSTROAD_ERROR");
        let _ = writeln!(
            io::stderr().lock(),
            "{}",
            serde_json::json!({ "status": "error", "code": code, "message": message })
        );
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    #[test]
    fn command_shape_accepts_workspace_after_subcommand() {
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from([
                "rustroad",
                "init",
                "--workspace",
                "demo",
                "--base-rev",
                "rev0",
                "--input",
                "design.json"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["rustroad", "place", "--workspace", "demo"]).is_err());
        assert!(run(Cli::try_parse_from(["rustroad", "inspect"]).unwrap()).is_err());
    }
}
