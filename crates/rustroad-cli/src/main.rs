use clap::{Parser, Subcommand};
use rustroad_core::Limits;
use serde::Serialize;
use std::io::{self, Read, Write};
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
    /// Maximum mapped-Verilog input size; storage/parser scratch stays bounded.
    #[arg(long, global = true, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=2048))]
    max_input_mib: u64,
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
#[derive(clap::Args)]
struct EcoBudgetArgs {
    #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u64).range(1..=65536))]
    eco_memory_mib: u64,
    #[arg(long, default_value_t = 4096)]
    max_eco_operations: usize,
}
impl EcoBudgetArgs {
    fn limits(
        &self,
        verilog: &rustroad_formats::verilog::VerilogLimits,
    ) -> rustroad_workspace::eco::EcoInputLimits {
        let mut verilog = verilog.clone();
        let defaults = rustroad_formats::verilog::VerilogLimits::default();
        if verilog.max_input_bytes <= defaults.max_input_bytes
            && verilog.max_instances <= defaults.max_instances
            && verilog.max_connections <= defaults.max_connections
        {
            verilog.max_tokens = defaults.max_tokens;
        }
        rustroad_workspace::eco::EcoInputLimits {
            verilog,
            memory_mib: self.eco_memory_mib,
            max_operations: self.max_eco_operations,
        }
    }
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
    /// Research-only stage-current observations; writes no physical design revision.
    PlaceP1Diagnose {
        #[arg(long)]
        base_rev: String,
        /// Fresh disjoint private diagnostic workspace (never adopted).
        #[arg(long)]
        out_workspace: PathBuf,
        #[arg(long)]
        out_base_rev: String,
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
        /// ECO: an earlier routed workspace of the same design (read-only);
        /// nets whose pins are unchanged keep its geometry when still valid.
        #[arg(long, requires = "prior_rev")]
        prior_workspace: Option<PathBuf>,
        /// HEAD revision the prior workspace must be at (REVISION_CONFLICT otherwise).
        #[arg(long, requires = "prior_workspace")]
        prior_rev: Option<String>,
    },
    /// Read-only diagnostic: geometric cleanup on a `RUSTROAD_RULE_DUMP` file.
    CleanupReplay {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        dump: PathBuf,
    },
    /// Read-only local replay; does not publish a routing revision or signoff verdict.
    ReplayRoute {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        max_search_expansions: Option<usize>,
        #[arg(long, num_args = 4, allow_hyphen_values = true)]
        guide_bounds: Vec<i64>,
        /// Diagnose immediate terminal geometry without running a route search.
        #[arg(long)]
        probe_terminals: bool,
        /// Explicit bounded search experiment after validating the captured source.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=16))]
        heuristic_weight: Option<u64>,
        /// Read-only no-rip-up diagnostic; requires --max-search-expansions.
        #[arg(
            long,
            requires = "max_search_expansions",
            conflicts_with = "probe_terminals"
        )]
        hard_search: bool,
        /// Report <=4096 discovered states of the final hard diagnostic sink.
        #[arg(long, requires = "hard_search")]
        probe_search_frontier: bool,
        /// Permit only this routed net to be displaced in a bounded soft replay.
        #[arg(long, requires = "max_search_expansions", conflicts_with_all = ["hard_search", "probe_terminals"])]
        allowed_victim: Option<u32>,
        /// Explicit bounded production-stage policy for a read-only replay.
        #[arg(long, conflicts_with_all = ["allowed_victim", "hard_search", "probe_terminals", "probe_search_frontier"])]
        single_owner_repair_config: Option<PathBuf>,
        /// Exact query-index experiment, validated after the captured source.
        #[arg(long)]
        query_indices_config: Option<PathBuf>,
    },
    /// Write checked routed geometry and scope as JSON.
    ExportRoutes,
    /// Explicit routed logical-net names, including no-geometry members.
    AntennaInventory {
        #[arg(long, default_value_t = 250_000)]
        max_inventory_nets: usize,
        #[arg(long, default_value_t = 1_000_000)]
        max_inventory_routed_ids: usize,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=64))]
        max_inventory_name_mib: u64,
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(1..=512))]
        max_inventory_route_mib: u64,
        #[arg(long, default_value_t = 10_000_000)]
        max_inventory_work: usize,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..=128))]
        max_inventory_output_mib: u64,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(1..=128))]
        inventory_memory_mib: u64,
    },
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
    /// Write a bounded, read-only seed for `place --seed` in a fresh workspace.
    ExportPlacementSeed,
    /// Write the imported logical netlist to stdout with canonical syntax.
    ExportVerilog {
        #[arg(long)]
        include_power: bool,
        /// Maximum emitted bytes; output may be partial on failure.
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u64).range(1..=16384))]
        max_output_mib: u64,
        /// Whole-process export memory allowance including the loaded snapshot.
        #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u64).range(1..=65536))]
        export_memory_mib: u64,
        #[arg(long, default_value_t = 200_000_000)]
        max_export_work: usize,
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
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u64).range(1..=16384))]
        max_spef_mib: u64,
        #[arg(long, default_value_t = 2048, value_parser = clap::value_parser!(u64).range(1..=65536))]
        extract_memory_mib: u64,
        #[arg(long, default_value_t = 2_000_000)]
        max_extract_shapes: usize,
        #[arg(long, default_value_t = 8_000_000)]
        max_extract_bin_entries: usize,
        #[arg(long, default_value_t = 256_000_000)]
        max_virtual_shapes: usize,
        #[arg(long, default_value_t = 64_000_000)]
        max_instance_bin_entries: usize,
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
        /// Candidate shapes per wire run (scratch 256 B each; ceiling
        /// rustroad_core::extract::MAX_CANDIDATES_CEILING = 48 MiB of scratch).
        #[arg(long, default_value_t = 4096)]
        max_extract_candidates: usize,
        /// Opt-in candidate-query workers, bounded by --threads and extraction memory.
        #[arg(long, default_value_t = 1)]
        extract_query_workers: usize,
        /// Candidate queries held per tile (1..=64); interval arithmetic stays serial.
        #[arg(long, default_value_t = 8)]
        extract_query_batch_runs: usize,
        /// Additional query scratch cap within --extract-memory-mib (1..=64 MiB).
        #[arg(long, default_value_t = 32)]
        extract_query_scratch_mib: u64,
        /// Couplings below this value (fF) become ground capacitance at both
        /// nodes; 0 keeps every coupling capacitor.
        #[arg(long, default_value_t = 0.0)]
        min_coupling_ff: f64,
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
        #[command(flatten)]
        budget: EcoBudgetArgs,
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
        /// Explicit `{master:{port:[msb,lsb]}}` metadata; no bracket-name guessing.
        #[arg(long)]
        cell_buses: Option<PathBuf>,
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
        #[command(flatten)]
        budget: EcoBudgetArgs,
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
        /// What-if shapes `[{net, layer, rect:[lx,ly,ux,uy]}]` in DBU merged into nets.
        #[arg(long)]
        extra_shapes: Option<PathBuf>,
        /// Explicit bounded routed-net subset; omitted keeps the legacy complete report.
        #[arg(long)]
        selected_nets: Option<PathBuf>,
        #[arg(long, default_value_t = 200_000)]
        max_shapes_per_net: usize,
        #[arg(long, default_value_t = 2_000_000)]
        max_total_shapes: usize,
        #[arg(long, default_value_t = 200_000_000)]
        max_pair_checks: usize,
        #[arg(long, default_value_t = 4_000_000)]
        max_union_cells: usize,
        #[arg(long, default_value_t = 100_000)]
        max_rows: usize,
        /// Total process-stage and geometry-grid work budget.
        #[arg(long, default_value_t = 200_000_000)]
        max_work: usize,
        /// Fraction of each LEF limit kept as margin for the native verdict
        /// (default derived from the measured native-vs-deck under-read).
        #[arg(long, default_value_t = rustroad_workspace::antenna::DEFAULT_SAFETY_MARGIN)]
        safety_margin: f64,
    },
    /// Stream ordered selected checks while decoding and preparing HEAD once.
    CheckAntennaBatch {
        /// Bounded rustroad.antenna-check-batch.v1 plan, pinned to one revision.
        #[arg(long)]
        plan: PathBuf,
    },
    /// Repair canonical antenna groups in one bounded, exclusively locked process.
    RepairAntennaGroups {
        #[arg(long)]
        base_rev: String,
        #[arg(long)]
        plan: PathBuf,
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
        /// New selected inventory with exact scope/limits/source binding and live memory cap.
        /// The row bound is inherited unless --max-antenna-rows explicitly matches it.
        #[arg(long, conflicts_with = "antenna_report", requires = "selected_nets")]
        selected_report: Option<PathBuf>,
        /// JSON array of routed, supported net names. Permits an explicit
        /// partial-route experiment; requires full independent revalidation.
        #[arg(long)]
        selected_nets: Option<PathBuf>,
        /// Antenna report rows; separate from the global placement-row --max-rows.
        #[arg(long)]
        max_antenna_rows: Option<usize>,
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
        /// Cumulative model work; also caps planner/validation comparisons.
        #[arg(long, default_value_t = 200_000_000)]
        max_work: usize,
        /// Optional lower cap on the jumper planner's own work; once spent,
        /// the remaining selected nets are reported unrepaired.
        #[arg(long)]
        max_planner_work: Option<usize>,
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
        #[command(flatten)]
        budget: EcoBudgetArgs,
        #[arg(long)]
        netlist: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        delay_cells: PathBuf,
        /// Override of the profile's cell-LEF path (same SHA-256 required).
        #[arg(long)]
        cell_lef: Option<PathBuf>,
        /// Explicit `{master:{port:[msb,lsb]}}` metadata, bound into the receipt.
        #[arg(long)]
        cell_buses: Option<PathBuf>,
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

fn emit_diagnostic(value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    // A broken pipe/serialization failure after publication leaves a provisional
    // HEAD, not an accepted diagnostic. The outer actual receipt must show exit0.
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self
                .0
                .len()
                .checked_add(bytes.len())
                .is_none_or(|n| n > 64 << 10)
            {
                return Err(io::Error::other("diagnostic CLI receipt cap"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut bytes = Bounded(Vec::new());
    bytes.0.try_reserve_exact(64 << 10)?;
    serde_json::to_writer(&mut bytes, value)?;
    bytes.write_all(b"\n")?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output.write_all(&bytes.0)?;
    output.flush()?;
    Ok(())
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let parser_limits = rustroad_formats::verilog::VerilogLimits {
        max_input_bytes: cli.max_input_mib as usize * 1024 * 1024,
        max_tokens: (cli.max_input_mib as usize * 1024 * 1024).clamp(2_000_000, 500_000_000),
        max_instances: cli.max_cells,
        max_connections: cli.max_pins,
        max_expr_nodes: cli.max_pins,
        max_nets: cli.max_nets,
        ..Default::default()
    };
    let connectivity_limits = rustroad_core::connectivity::ConnectivityLimits {
        max_instances: cli.max_cells,
        max_nets: cli.max_nets,
        max_pins: cli.max_pins,
        max_aliases: cli.max_aliases,
        ..Default::default()
    };
    let mut floor_limits = rustroad_workspace::floorplan::FloorplanLimits {
        verilog: parser_limits.clone(),
        connectivity: connectivity_limits.clone(),
        placement: rustroad_core::place::PlaceLimits {
            max_cells: cli.max_cells,
            max_pins: cli.max_pins,
            max_work: (cli.max_cells as u64)
                .saturating_mul(50_000)
                .max(20_000_000_000),
        },
        ..Default::default()
    };
    if cli.max_input_mib > 16 || cli.max_cells > 250_000 {
        floor_limits.ties.max_work = (cli.max_input_mib as usize * 1024 * 1024).saturating_mul(16);
    }
    // File-only ECO commands read netlists/reports and write new files; they
    // never open a workspace, so --workspace is not required for them.
    match &cli.command {
        Command::HoldEco {
            budget,
            netlist,
            timed_netlist,
            timed_netlist_sha256,
            cell_lef,
            cell_buses,
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
            return emit(&rustroad_workspace::eco::plan_hold_with_limits(
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
                    cell_buses: cell_buses.clone(),
                    pins_reports: pins_reports.clone(),
                    allow_partial: *allow_partial,
                    max_group_sinks: *max_group_sinks,
                },
                &budget.limits(&parser_limits),
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
            return emit(&rustroad_workspace::floorplan::floorplan_with_limits(
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
                &floor_limits,
            )?);
        }
        Command::ApplyEco {
            budget,
            netlist,
            plan,
            delay_cells,
            cell_lef,
            cell_buses,
            sizing_profile,
            out,
        } => {
            return emit(&rustroad_workspace::eco::apply_plan_with_options(
                netlist,
                plan,
                delay_cells,
                out,
                &rustroad_workspace::eco::ApplyOptions {
                    cell_lef: cell_lef.as_deref(),
                    sizing: sizing_profile.as_deref(),
                    cell_buses: cell_buses.as_deref(),
                },
                &budget.limits(&parser_limits),
            )?);
        }
        Command::AntennaEco {
            budget,
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
            return emit(&rustroad_workspace::eco::plan_antenna_with_limits(
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
                &budget.limits(&parser_limits),
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
            emit(&rustroad_workspace::pdk::import_pdk_bounded(
                &workspace,
                &base_rev,
                &sources,
                &limits,
                &connection_limits,
                physical_config.as_deref(),
                &parser_limits,
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
        Command::PlaceP1Diagnose {
            base_rev,
            out_workspace,
            out_base_rev,
        } => emit_diagnostic(&rustroad_workspace::placement_diagnostics::diagnose(
            &workspace,
            &base_rev,
            &out_workspace,
            &out_base_rev,
            &limits,
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
            prior_workspace,
            prior_rev,
        } => {
            let prior = prior_workspace
                .as_deref()
                .zip(prior_rev.as_deref())
                .map(|(w, r)| rustroad_workspace::routing::PriorSource {
                    workspace: w,
                    revision: r,
                });
            emit(&rustroad_workspace::routing::route_eco(
                &workspace,
                &base_rev,
                &config,
                if scope == "clock" {
                    rustroad_workspace::routing::RouteScope::Clock
                } else {
                    rustroad_workspace::routing::RouteScope::All
                },
                prior.as_ref(),
                &limits,
            )?)
        }
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
        Command::AntennaInventory {
            max_inventory_nets,
            max_inventory_routed_ids,
            max_inventory_name_mib,
            max_inventory_route_mib,
            max_inventory_work,
            max_inventory_output_mib,
            inventory_memory_mib,
        } => {
            let report = rustroad_workspace::antenna::antenna_inventory(
                &workspace,
                &rustroad_workspace::antenna::InventoryBounds {
                    max_logical_nets: max_inventory_nets,
                    max_routed_ids: max_inventory_routed_ids,
                    max_name_bytes: (max_inventory_name_mib << 20) as usize,
                    max_route_bytes: max_inventory_route_mib << 20,
                    max_work: max_inventory_work,
                    max_output_bytes: (max_inventory_output_mib << 20) as usize,
                    memory_bytes: (inventory_memory_mib << 20) as usize,
                },
                &limits,
            )?;
            let stdout = io::stdout();
            let mut output = io::BufWriter::with_capacity(256 * 1024, stdout.lock());
            report.write_json(&mut output)?;
            writeln!(output)?;
            output.flush()?;
            Ok(())
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
        Command::CleanupReplay { config, dump } => emit(
            &rustroad_workspace::routing::cleanup_replay(&workspace, &config, &dump, &limits)?,
        ),
        Command::ReplayRoute {
            config,
            snapshot,
            max_search_expansions,
            guide_bounds,
            probe_terminals,
            heuristic_weight,
            hard_search,
            probe_search_frontier,
            allowed_victim,
            single_owner_repair_config,
            query_indices_config,
        } => {
            let query_indices = match query_indices_config {
                None => None,
                Some(path) => {
                    let mut data = Vec::new();
                    std::fs::File::open(path)?
                        .take(4097)
                        .read_to_end(&mut data)?;
                    if data.len() > 4096 {
                        return Err("INVALID_ARGUMENT: query policy exceeds4096bytes".into());
                    }
                    Some(serde_json::from_slice::<
                        rustroad_core::routing::QueryIndices,
                    >(&data)?)
                }
            };
            let single_owner_repair = match single_owner_repair_config {
                None => None,
                Some(path) => {
                    let mut data = Vec::new();
                    std::fs::File::open(path)?
                        .take(4097)
                        .read_to_end(&mut data)?;
                    if data.len() > 4096 {
                        return Err("INVALID_ARGUMENT: owner repair policy exceeds4096bytes".into());
                    }
                    Some(serde_json::from_slice::<
                        rustroad_core::routing::SingleOwnerRepairConfig,
                    >(&data)?)
                }
            };
            let guide = match guide_bounds.as_slice() {
                [] => None,
                &[lx, ly, ux, uy] => Some(rustroad_core::Rect { lx, ly, ux, uy }),
                _ => {
                    return Err(
                        "INVALID_ARGUMENT: guide bounds require four DBU coordinates".into(),
                    );
                }
            };
            emit(&rustroad_workspace::routing::replay_route(
                &workspace,
                &config,
                &snapshot,
                &rustroad_core::routing::ReplayOptions {
                    max_search_expansions,
                    guide_bounds: guide,
                    probe_terminals,
                    heuristic_weight,
                    hard_search,
                    probe_search_frontier,
                    allowed_victim,
                    single_owner_repair,
                    query_indices,
                },
                &limits,
            )?)
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
        Command::ExportPlacementSeed => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            emit(&rustroad_workspace::placement_seed::from_snapshot(
                &snapshot,
            )?)
        }
        Command::ExportVerilog {
            include_power,
            max_output_mib,
            export_memory_mib,
            max_export_work,
        } => {
            let snapshot = rustroad_workspace::read(&workspace, &limits)?;
            let stdout = io::stdout();
            let mut output = io::BufWriter::with_capacity(256 * 1024, stdout.lock());
            let mut syntax = parser_limits.clone();
            syntax.max_input_bytes = usize::try_from(max_output_mib.saturating_mul(1 << 20))?;
            snapshot.export_verilog_bounded(
                if include_power {
                    rustroad_workspace::netlist_export::SupplyMode::Powered
                } else {
                    rustroad_workspace::netlist_export::SupplyMode::Logical
                },
                &rustroad_workspace::netlist_export::ExportLimits {
                    verilog: syntax,
                    memory_mib: export_memory_mib,
                    max_work: max_export_work,
                },
                &mut output,
            )?;
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
            extract_memory_mib,
            max_extract_shapes,
            max_extract_bin_entries,
            max_virtual_shapes,
            max_instance_bin_entries,
            max_segment_um,
            max_rc_nodes,
            max_coupling_caps,
            max_rc_elements,
            max_extract_work,
            max_extract_candidates,
            extract_query_workers,
            extract_query_batch_runs,
            extract_query_scratch_mib,
            min_coupling_ff,
        } => {
            if !(max_segment_um.is_finite() && (0.1..=10_000.0).contains(&max_segment_um)) {
                return Err("INVALID_ARGUMENT: --max-segment-um must be within 0.1..=10000".into());
            }
            let extract_limits = rustroad_core::extract::ExtractLimits {
                memory_mib: extract_memory_mib,
                max_shapes: max_extract_shapes,
                max_bin_entries: max_extract_bin_entries,
                max_virtual_shapes,
                max_instance_bin_entries,
                max_nodes: max_rc_nodes,
                max_couplings: max_coupling_caps,
                max_elements: max_rc_elements,
                max_work: max_extract_work,
                max_candidates: max_extract_candidates,
                max_segment_length: 1, // converted after loading the locked snapshot
                query_workers: extract_query_workers,
                query_batch_runs: extract_query_batch_runs,
                query_scratch_mib: extract_query_scratch_mib,
                min_coupling_pf: min_coupling_ff / 1000.0,
            };
            emit(&rustroad_workspace::extract::extract_with_segment_um(
                &workspace,
                &rustroad_workspace::extract::ExtractRequest {
                    profile: rc_profile,
                    corner,
                    out,
                    max_spef_bytes: max_spef_mib * 1024 * 1024,
                    limits: extract_limits,
                },
                &limits,
                max_segment_um,
            )?)
        }
        Command::FlowCheck { manifest } => {
            let _snapshot = rustroad_workspace::read(&workspace, &limits)?;
            emit(&rustroad_flow::assess_path(&manifest)?)
        }
        Command::CheckAntenna {
            extra_shapes,
            selected_nets,
            max_shapes_per_net,
            max_total_shapes,
            max_pair_checks,
            max_union_cells,
            max_rows,
            max_work,
            safety_margin,
        } => emit(&rustroad_workspace::antenna::check_antenna(
            &workspace,
            &rustroad_workspace::antenna::AntennaRequest {
                extra_shapes,
                selected_nets: selected_nets
                    .as_deref()
                    .map(rustroad_workspace::antenna::read_check_selection)
                    .transpose()?,
                max_shapes_per_net,
                max_total_shapes,
                max_pair_checks,
                max_union_cells,
                max_rows,
                max_work,
                safety_margin,
            },
            &limits,
        )?),
        Command::CheckAntennaBatch { plan } => {
            let plan = rustroad_workspace::antenna::read_check_batch_plan(&plan)?;
            let stdout = io::stdout();
            rustroad_workspace::antenna::check_antenna_batch(
                &workspace,
                &plan,
                &limits,
                &mut stdout.lock(),
            )?;
            Ok(())
        }
        Command::RepairAntennaGroups { base_rev, plan } => {
            let plan = rustroad_workspace::antenna::read_group_repair_plan(&plan)?;
            emit(&rustroad_workspace::antenna::repair_antenna_groups(
                &workspace, &base_rev, &plan, &limits,
            )?)
        }
        Command::RepairAntenna {
            base_rev,
            antenna_report,
            selected_report,
            selected_nets,
            max_antenna_rows,
            safety_margin,
            max_jumpers_per_net,
            max_jumpers,
            max_positions,
            max_candidates,
            max_work,
            max_planner_work,
        } => emit(&rustroad_workspace::antenna::repair_antenna(
            &workspace,
            &base_rev,
            &rustroad_workspace::antenna::RepairRequest {
                antenna: rustroad_workspace::antenna::AntennaRequest {
                    safety_margin,
                    max_work,
                    ..Default::default()
                },
                report: antenna_report,
                selected_report,
                max_rows: max_antenna_rows,
                scope: selected_nets
                    .as_deref()
                    .map(rustroad_workspace::antenna::read_repair_selection)
                    .transpose()?
                    .unwrap_or_default(),
                max_jumpers_per_net,
                max_jumpers,
                jumper: rustroad_core::jumper::JumperLimits {
                    max_positions,
                    max_candidates,
                    ..Default::default()
                },
                max_planner_work,
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
    if matches!(&cli.command, Command::PlaceP1Diagnose { .. }) && cli.threads > 4 {
        let _ = writeln!(
            io::stderr().lock(),
            "{}",
            serde_json::json!({"status":"error","code":"BUDGET_EXCEEDED","message":"diagnostic supports at most four numerical pool threads"})
        );
        std::process::exit(2);
    }
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
    fn diagnostic_output_base_reaches_workspace_conflict_before_source_loading() {
        // Three HEAD-only outputs <128B plus a two-thread local pool. This
        // exercises CLI dispatch/error variants, not geometry or a PDK oracle.
        struct Temp(PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = Temp(std::env::temp_dir().join(format!(
            "rustroad-p1-output-base-{}-{stamp}",
            std::process::id()
        )));
        std::fs::create_dir(&root.0).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        for (name, head, base, prefix) in [
            (
                "mismatch",
                b"rev7\n".as_slice(),
                "empty",
                "REVISION_CONFLICT:",
            ),
            ("matching", b"rev7\n", "rev7", "INVALID_WORKSPACE:"),
            ("invalid", b"rev07\n", "empty", "INVALID_WORKSPACE:"),
        ] {
            let output = root.0.join(name);
            std::fs::create_dir(&output).unwrap();
            std::fs::write(output.join("HEAD"), head).unwrap();
            let mut permissions = std::fs::metadata(output.join("HEAD"))
                .unwrap()
                .permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(output.join("HEAD"), permissions).unwrap();
            let source = root.0.join("source-not-created");
            let cli = Cli::try_parse_from([
                "rustroad",
                "--workspace",
                source.to_str().unwrap(),
                "--threads",
                "2",
                "place-p1-diagnose",
                "--base-rev",
                "rev1",
                "--out-workspace",
                output.to_str().unwrap(),
                "--out-base-rev",
                base,
            ])
            .unwrap();
            let message = pool.install(|| run(cli).unwrap_err().to_string());
            assert!(message.starts_with(prefix), "{message}");
            if name == "matching" {
                assert!(message.contains("OUTPUT_NOT_FRESH"));
            }
            assert_eq!(std::fs::read(output.join("HEAD")).unwrap(), head);
            assert_eq!(std::fs::read_dir(&output).unwrap().count(), 1);
            assert!(!source.exists());
        }
    }
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
