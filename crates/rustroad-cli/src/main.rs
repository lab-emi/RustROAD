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
    /// Deterministic, single-height fixed-order row placement (not global optimization).
    Place {
        #[arg(long)]
        base_rev: String,
    },
    /// Read the current revision and honest analysis coverage.
    Inspect,
    /// Write placement-only DEF to stdout. No connectivity/routing is invented.
    ExportDef,
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
    match cli.command {
        Command::Init { base_rev, input } => {
            let design = rustroad_workspace::read_design(&input, &limits)?;
            emit(&rustroad_workspace::init(
                &workspace, &base_rev, design, &limits,
            )?)
        }
        Command::Place { base_rev } => {
            emit(&rustroad_workspace::place(&workspace, &base_rev, &limits)?)
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
