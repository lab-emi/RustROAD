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
        /// Timing-driven net weights (`rustroad.net-weights.v1`, analytic placer only).
        #[arg(long)]
        net_weights: Option<PathBuf>,
        /// ECO placement seed (`rustroad.placement-seed.v1`): seeded cells are
        /// legalized from their seed, new cells start at their neighbours.
        #[arg(long)]
        seed: Option<PathBuf>,
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
    /// Write a read-only pin-access test DEF (each used master, orientation
    /// and signal pin routed by this profile) for external DRC decks.
    ExportAccessTest {
        #[arg(long)]
        config: PathBuf,
    },
    /// Read the current revision and honest analysis coverage.
    Inspect,
    /// Write placed DEF, including imported PINS/NETS when present.
    ExportDef,
    /// Write the imported logical netlist to stdout with canonical syntax.
    ExportVerilog {
        #[arg(long)]
        include_power: bool,
    },
    /// Read-only native RC extraction of the routed HEAD revision to SPEF.
    Extract {
        /// rustroad.rc-profile.v1 JSON (scripts/build_rc_profile.py).
        #[arg(long)]
        rc_profile: PathBuf,
        /// Must equal the profile corner (min, nom or max).
        #[arg(long)]
        corner: String,
        /// SPEF output path, written atomically outside the workspace.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u64).range(1..=256))]
        max_spef_mib: u64,
        /// Straight wire runs longer than this are split into several resistors.
        #[arg(long, default_value_t = 25.0)]
        max_segment_um: f64,
        #[arg(long, default_value_t = 500_000)]
        max_rc_nodes: usize,
        #[arg(long, default_value_t = 1_000_000)]
        max_coupling_caps: usize,
        /// Resistors + nodes + 2 x couplings (hard ceiling 8x the default).
        #[arg(long, default_value_t = 1_000_000)]
        max_rc_elements: usize,
        #[arg(long, default_value_t = 200_000_000)]
        max_extract_work: usize,
        /// Candidate shapes per wire run (scratch 44 B each; ceiling
        /// rustroad_core::extract::MAX_CANDIDATES_CEILING = 48 MiB of scratch).
        #[arg(long, default_value_t = 4096)]
        max_extract_candidates: usize,
    },
    /// Write the linked connectivity/LEF geometry snapshot as JSON.
    ExportConnectivity,
    /// Write a deterministic hierarchical GDSII stream (read-only; HEAD unchanged).
    ExportGds {
        /// PDK standard-cell GDS; used structures are copied byte-identically.
        #[arg(long)]
        cell_gds: PathBuf,
        /// Per-PDK layer map profile (schema rustroad.gds-layer-map.v1).
        #[arg(long)]
        layer_map: PathBuf,
        /// Destination file, outside the workspace; written atomically.
        #[arg(long)]
        out: PathBuf,
        /// Cell LEF override for the FOREIGN check; must match the imported sha256.
        #[arg(long)]
        cell_lef: Option<PathBuf>,
        #[arg(long, default_value_t = rustroad_workspace::gds::DEFAULT_MAX_OUTPUT_BYTES)]
        max_gds_bytes: u64,
        #[arg(long, default_value_t = rustroad_workspace::gds::DEFAULT_MAX_OUTPUT_RECORDS)]
        max_gds_records: u64,
    },
    /// Inspect pinned external tools and PDK files; readiness is not signoff.
    FlowCheck {
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Plan and apply hold-fix delay cells on a mapped netlist from RustSTA
    /// hold_fix/endpoints reports (files only; no RustROAD workspace is read or
    /// written). Reports are bound to --timed-netlist through the revision
    /// record of the STA workspace they name (read under a shared lock: its
    /// netlist source must have the xxh3-64 of --timed-netlist); stale reports
    /// fail STALE_REPORT, an unreadable record REPORT_PROVENANCE.
    HoldEco {
        /// Flat mapped Verilog netlist to edit (never overwritten).
        #[arg(long)]
        netlist: PathBuf,
        /// The netlist the timing engine imported for these reports (the
        /// post-CTS export of --netlist); --netlist must be refined by it and
        /// the reports' STA revision must record it as its netlist source.
        #[arg(long)]
        timed_netlist: PathBuf,
        /// SHA-256 recorded when --timed-netlist was handed to the timing engine.
        #[arg(long)]
        timed_netlist_sha256: Option<String>,
        /// Override of the profile's cell-LEF path (same SHA-256 required).
        #[arg(long)]
        cell_lef: Option<PathBuf>,
        /// `ruststa report --section hold_fix --target-margin M --limit 0` JSON (repeatable).
        #[arg(long = "hold-report", required = true)]
        hold_reports: Vec<PathBuf>,
        /// `ruststa report --section endpoints --corner C --limit 0` JSON (repeatable).
        #[arg(long = "setup-report", required = true)]
        setup_reports: Vec<PathBuf>,
        /// Liberty-derived profile, schema rustroad.delay-cells.v1.
        #[arg(long)]
        delay_cells: PathBuf,
        /// Corners whose hold slack must reach the target margin (repeatable).
        #[arg(long = "hold-corner", required = true)]
        hold_corners: Vec<String>,
        #[arg(long, default_value = "ss")]
        setup_corner: String,
        #[arg(long, default_value_t = 0.05)]
        target_margin_ns: f64,
        #[arg(long, default_value_t = 0.05)]
        setup_margin_ns: f64,
        #[arg(long, default_value_t = 4)]
        max_chain: usize,
        #[arg(long, default_value_t = 8)]
        max_total_chain: usize,
        #[arg(long, default_value_t = 4096)]
        max_sinks: usize,
        #[arg(long, default_value_t = 200_000)]
        max_combinations: usize,
        #[arg(long, default_value = "rr_eco_hold_")]
        prefix: String,
        /// `ruststa report --section pins --corner C --limit 0` JSON of the same
        /// revision (repeatable, one per setup corner; must cover
        /// --setup-corner). Enables path-aware placement: delay goes where every
        /// path through the point keeps its setup slack (a sink pin on the worst
        /// hold path, or every sink of one net, e.g. an input port).
        #[arg(long = "pins-report")]
        pins_reports: Vec<PathBuf>,
        /// Apply the feasible part and list the infeasible endpoints (status
        /// `partial`) instead of failing the whole plan.
        #[arg(long)]
        allow_partial: bool,
        /// Largest sink count of a net-level point (path-aware mode).
        #[arg(long, default_value_t = 8)]
        max_group_sinks: usize,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        plan_out: PathBuf,
    },
    /// Plan and apply antenna diodes for the violations of a check-antenna
    /// report on a mapped netlist (files only; no workspace is read or written).
    AntennaEco {
        #[arg(long)]
        netlist: PathBuf,
        /// `rustroad check-antenna` JSON (schema rustroad.antenna-check.v1).
        #[arg(long)]
        antenna_report: PathBuf,
        /// Profile whose antenna_cells name the diode (rustroad.delay-cells.v1).
        #[arg(long)]
        delay_cells: PathBuf,
        #[arg(long)]
        cell: Option<String>,
        #[arg(long, default_value_t = 1)]
        min_diodes: usize,
        #[arg(long, default_value_t = 4)]
        max_diodes_per_net: usize,
        #[arg(long, default_value = "rr_eco_ant_")]
        prefix: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        plan_out: PathBuf,
    },
    /// Read-only native process-antenna check of the routed HEAD (JSON report).
    CheckAntenna {
        /// What-if shapes [{net, layer, rect:[lx,ly,ux,uy]}] in DBU merged into nets.
        #[arg(long)]
        extra_shapes: Option<PathBuf>,
        #[arg(long, default_value_t = 200_000)]
        max_shapes_per_net: usize,
        #[arg(long, default_value_t = 200_000_000)]
        max_pair_checks: usize,
        #[arg(long, default_value_t = 4_000_000)]
        max_union_cells: usize,
        #[arg(long, default_value_t = 100_000)]
        max_rows: usize,
        /// Fraction of each LEF limit kept as margin for the native verdict
        /// (default derived from the measured native-vs-deck under-read).
        #[arg(long, default_value_t = rustroad_workspace::antenna::DEFAULT_SAFETY_MARGIN)]
        safety_margin: f64,
    },
    /// Diode-free antenna repair of HEAD: layer jumpers on the violating
    /// nets of the native check (new revision; netlist and placement kept).
    RepairAntenna {
        #[arg(long)]
        base_rev: String,
        /// `check-antenna` report of --base-rev (same revision, margin and
        /// violated rows required; STALE_REPORT otherwise).
        #[arg(long)]
        antenna_report: Option<PathBuf>,
        #[arg(long, default_value_t = rustroad_workspace::antenna::DEFAULT_SAFETY_MARGIN)]
        safety_margin: f64,
        #[arg(long, default_value_t = 16)]
        max_jumpers_per_net: usize,
        #[arg(long, default_value_t = 4096)]
        max_jumpers: usize,
        /// Cut positions probed per straight run of the violating layer.
        #[arg(long, default_value_t = 512)]
        max_positions: usize,
        /// Legal candidates evaluated per jumper decision.
        #[arg(long, default_value_t = 32)]
        max_candidates: usize,
    },
    /// Derive die/core/rows and a connectivity-aware I/O plan from cell area,
    /// utilization and aspect (files only; writes a floorplan and a physical
    /// configuration for import-pdk).
    Floorplan {
        #[arg(long)]
        tech_lef: PathBuf,
        #[arg(long)]
        cell_lef: PathBuf,
        #[arg(long)]
        netlist: PathBuf,
        /// Physical profile (tap/tie/filler/clock/I/O roles).
        #[arg(long)]
        physical_config: PathBuf,
        /// Routing profile (track pitches for I/O landing and snapping).
        #[arg(long)]
        routing_config: PathBuf,
        #[arg(long)]
        site: String,
        #[arg(long, default_value_t = 0.5)]
        utilization: f64,
        #[arg(long, default_value_t = 1.0)]
        aspect: f64,
        #[arg(long, default_value_t = 10.0)]
        margin_um: f64,
        #[arg(long)]
        clock_port: Option<String>,
        #[arg(long)]
        no_clock: bool,
        #[arg(long)]
        fixed_order: bool,
        #[arg(long)]
        out_floorplan: PathBuf,
        #[arg(long)]
        out_physical_config: PathBuf,
    },
    /// Apply a typed rustroad.eco-plan.v1 to a mapped netlist (files only).
    /// insert_buffer sinks must be cell-LEF INPUT pins.
    ApplyEco {
        #[arg(long)]
        netlist: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        delay_cells: PathBuf,
        /// Override of the profile's cell-LEF path (same SHA-256 required).
        #[arg(long)]
        cell_lef: Option<PathBuf>,
        /// Sizing profile (scripts/build_sizing_profile.py); required for resize_cell
        #[arg(long)]
        sizing_profile: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
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
    // File-only ECO commands read netlists/reports and write new files; they
    // never open a workspace, so --workspace is not required for them.
    match &cli.command {
        Command::HoldEco {
            netlist,
            timed_netlist,
            timed_netlist_sha256,
            cell_lef,
            hold_reports,
            setup_reports,
            delay_cells,
            hold_corners,
            setup_corner,
            target_margin_ns,
            setup_margin_ns,
            max_chain,
            max_total_chain,
            max_sinks,
            max_combinations,
            prefix,
            pins_reports,
            allow_partial,
            max_group_sinks,
            out,
            plan_out,
        } => {
            return emit(&rustroad_workspace::eco::plan_hold(
                &rustroad_workspace::eco::HoldEcoRequest {
                    netlist: netlist.clone(),
                    hold_reports: hold_reports.clone(),
                    setup_reports: setup_reports.clone(),
                    profile: delay_cells.clone(),
                    hold_corners: hold_corners.clone(),
                    setup_corner: setup_corner.clone(),
                    target_margin_ns: *target_margin_ns,
                    setup_margin_ns: *setup_margin_ns,
                    max_chain: *max_chain,
                    max_total_chain: *max_total_chain,
                    max_sinks: *max_sinks,
                    max_combinations: *max_combinations,
                    prefix: prefix.clone(),
                    out: out.clone(),
                    plan_out: plan_out.clone(),
                    timed_netlist: timed_netlist.clone(),
                    timed_netlist_sha256: timed_netlist_sha256.clone(),
                    cell_lef: cell_lef.clone(),
                    pins_reports: pins_reports.clone(),
                    allow_partial: *allow_partial,
                    max_group_sinks: *max_group_sinks,
                },
            )?);
        }
        Command::Floorplan {
            tech_lef,
            cell_lef,
            netlist,
            physical_config,
            routing_config,
            site,
            utilization,
            aspect,
            margin_um,
            clock_port,
            no_clock,
            fixed_order,
            out_floorplan,
            out_physical_config,
        } => {
            return emit(&rustroad_workspace::floorplan::floorplan(
                &rustroad_workspace::floorplan::FloorplanRequest {
                    tech_lef: tech_lef.clone(),
                    cell_lef: cell_lef.clone(),
                    netlist: netlist.clone(),
                    physical_config: physical_config.clone(),
                    routing_config: routing_config.clone(),
                    site: site.clone(),
                    utilization: *utilization,
                    aspect: *aspect,
                    margin_um: *margin_um,
                    clock_port: clock_port.clone(),
                    no_clock: *no_clock,
                    fixed_order: *fixed_order,
                    out_floorplan: out_floorplan.clone(),
                    out_physical_config: out_physical_config.clone(),
                },
            )?);
        }
        Command::ApplyEco {
            netlist,
            plan,
            delay_cells,
            cell_lef,
            sizing_profile,
            out,
        } => {
            return emit(&rustroad_workspace::eco::apply_plan(
                netlist,
                plan,
                delay_cells,
                cell_lef.as_deref(),
                sizing_profile.as_deref(),
                out,
            )?);
        }
        Command::AntennaEco {
            netlist,
            antenna_report,
            delay_cells,
            cell,
            min_diodes,
            max_diodes_per_net,
            prefix,
            out,
            plan_out,
        } => {
            return emit(&rustroad_workspace::eco::plan_antenna(
                &rustroad_workspace::eco::AntennaEcoRequest {
                    netlist: netlist.clone(),
                    report: antenna_report.clone(),
                    profile: delay_cells.clone(),
                    cell: cell.clone(),
                    min_diodes: *min_diodes,
                    max_diodes_per_net: *max_diodes_per_net,
                    prefix: prefix.clone(),
                    out: out.clone(),
                    plan_out: plan_out.clone(),
                },
            )?);
        }
        _ => {}
    }
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
        Command::Place {
            base_rev,
            net_weights,
            seed,
        } => emit(&rustroad_workspace::place_with(
            &workspace,
            &base_rev,
            &limits,
            net_weights.as_deref(),
            seed.as_deref(),
        )?),
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
        Command::ExportAccessTest { config } => {
            let stdout = io::stdout();
            let mut output = io::BufWriter::with_capacity(256 * 1024, stdout.lock());
            rustroad_workspace::access_test::export_access_test(
                &workspace,
                &config,
                &limits,
                &mut output,
            )?;
            output.flush()?;
            Ok(())
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
        Command::ExportGds {
            cell_gds,
            layer_map,
            out,
            cell_lef,
            max_gds_bytes,
            max_gds_records,
        } => {
            let mut options = rustroad_workspace::gds::ExportOptions::new(cell_gds, layer_map, out);
            options.cell_lef = cell_lef;
            options.max_output_bytes = max_gds_bytes;
            options.max_output_records = max_gds_records;
            emit(&rustroad_workspace::gds::export_gds(
                &workspace, &options, &limits,
            )?)
        }
        Command::Extract {
            rc_profile,
            corner,
            out,
            max_spef_mib,
            max_segment_um,
            max_rc_nodes,
            max_coupling_caps,
            max_rc_elements,
            max_extract_work,
            max_extract_candidates,
        } => {
            if !(max_segment_um.is_finite() && (0.1..=10_000.0).contains(&max_segment_um)) {
                return Err("INVALID_ARGUMENT: --max-segment-um must be within 0.1..=10000".into());
            }
            let snapshot_units = rustroad_workspace::read(&workspace, &limits)?
                .metadata()
                .design
                .units_per_micron;
            let extract_limits = rustroad_core::extract::ExtractLimits {
                max_nodes: max_rc_nodes,
                max_couplings: max_coupling_caps,
                max_elements: max_rc_elements,
                max_work: max_extract_work,
                max_candidates: max_extract_candidates,
                max_segment_length: (max_segment_um * f64::from(snapshot_units)).round() as i64,
                ..Default::default()
            };
            emit(&rustroad_workspace::extract::extract(
                &workspace,
                &rustroad_workspace::extract::ExtractRequest {
                    profile: rc_profile,
                    corner,
                    out,
                    max_spef_bytes: max_spef_mib * 1024 * 1024,
                    limits: extract_limits,
                },
                &limits,
            )?)
        }
        Command::FlowCheck { manifest } => {
            let _snapshot = rustroad_workspace::read(&workspace, &limits)?;
            emit(&rustroad_flow::assess_path(&manifest)?)
        }
        Command::CheckAntenna {
            extra_shapes,
            max_shapes_per_net,
            max_pair_checks,
            max_union_cells,
            max_rows,
            safety_margin,
        } => emit(&rustroad_workspace::antenna::check_antenna(
            &workspace,
            &rustroad_workspace::antenna::AntennaRequest {
                extra_shapes,
                max_shapes_per_net,
                max_pair_checks,
                max_union_cells,
                max_rows,
                safety_margin,
            },
            &limits,
        )?),
        Command::RepairAntenna {
            base_rev,
            antenna_report,
            safety_margin,
            max_jumpers_per_net,
            max_jumpers,
            max_positions,
            max_candidates,
        } => emit(&rustroad_workspace::antenna::repair_antenna(
            &workspace,
            &base_rev,
            &rustroad_workspace::antenna::RepairRequest {
                antenna: rustroad_workspace::antenna::AntennaRequest {
                    safety_margin,
                    ..Default::default()
                },
                report: antenna_report,
                max_jumpers_per_net,
                max_jumpers,
                jumper: rustroad_core::jumper::JumperLimits {
                    max_positions,
                    max_candidates,
                    ..Default::default()
                },
            },
            &limits,
        )?),
        Command::HoldEco { .. }
        | Command::ApplyEco { .. }
        | Command::AntennaEco { .. }
        | Command::Floorplan { .. } => {
            unreachable!("file-only commands return before the workspace check")
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
        // Export is read-only: it takes no --base-rev and needs all three paths.
        assert!(
            Cli::try_parse_from([
                "rustroad",
                "export-gds",
                "--workspace",
                "w",
                "--cell-gds",
                "c.gds",
                "--layer-map",
                "m.json",
                "--out",
                "o.gds"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "rustroad",
                "export-gds",
                "--workspace",
                "w",
                "--cell-gds",
                "c.gds",
                "--out",
                "o.gds"
            ])
            .is_err()
        );
        assert!(run(Cli::try_parse_from(["rustroad", "inspect"]).unwrap()).is_err());
    }
}
