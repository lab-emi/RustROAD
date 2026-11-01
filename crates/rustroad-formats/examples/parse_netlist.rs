//! Bounded standalone measurement of the structural netlist reader.
//!
//! The full OpenC910 case retains an estimated 3–5 GiB AST and validation maps.
//! Run one process under an 8 GiB memory / 300 s wall-clock guard; this is a
//! scale benchmark, not a default unit test. The lexer keeps one token and a
//! 1 MiB read buffer regardless of source size.
use rustroad_formats::verilog::{VerilogLimits, parse_verilog_reader};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("expected a netlist path")?;
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let limits = VerilogLimits {
        max_input_bytes: 2 * 1024 * 1024 * 1024,
        max_tokens: 300_000_000,
        max_instances: 5_000_000,
        max_connections: 32_000_000,
        max_nets: 10_000_000,
        max_expr_nodes: 32_000_000,
        ..VerilogLimits::default()
    };
    let netlist = parse_verilog_reader(BufReader::with_capacity(1024 * 1024, file), &limits)?;
    let mut cells = BTreeMap::new();
    let mut connections = 0usize;
    for instance in &netlist.instances {
        *cells.entry(instance.cell.as_str()).or_insert(0usize) += 1;
        connections += instance.connections.len();
    }
    serde_json::to_writer_pretty(
        io::stdout().lock(),
        &serde_json::json!({
            "schema": "rustroad.parse_stats.v1",
            "module": netlist.module,
            "input_bytes": bytes,
            "instances": netlist.instances.len(),
            "connections": connections,
            "wires": netlist.wires.len(),
            "assigns": netlist.assigns.len(),
            "cells": cells,
        }),
    )?;
    Ok(())
}
