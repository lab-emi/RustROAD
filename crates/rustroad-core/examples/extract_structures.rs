//! Evaluate the native extraction model (`extract_cpu_ref`) on canonical
//! structures for `scripts/rc_structures.py`.
//!
//! Input (text, one record per line, integer DBU):
//! ```text
//! D <dbu_per_um>
//! L <name> <metal> <width> <vertical 0|1> <sheet_ohm> <area_cap_pf_per_um2 or ->
//! O <lower_metal> <pf_per_um2>          # overlap data of the last L
//! C <over> <under or -1> <rows>         # context of the last L, then rows:
//! <spacing_um> <coupling_pf_per_um> <ground_pf_per_um_per_side>
//! W <net> <layer> <x0> <y0> <x1> <y1>   # routed wire, driver pin at (x0, y0)
//! G <layer> <lx> <ly> <ux> <uy>         # grounded passive conductor
//! ```
//! Output on stdout: `N <net> <ground_pf> <coupling_pf>` per routed net.
//! usage: extract_structures INPUT
use rustroad_core::Rect;
use rustroad_core::extract::{
    ExtractInput, ExtractLimits, PassiveOwner, PassiveShape, RcContext, RcLayer, RcTech,
    extract_cpu_ref,
};
use rustroad_core::routing::{Access, RouteLayer, RouteStats, Routes, Terminal, Wire};
use std::collections::BTreeSet;
use std::process::ExitCode;

fn parse<T: std::str::FromStr>(token: Option<&str>, line: &str) -> Result<T, String> {
    token
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| format!("bad record: {line}"))
}

fn run(text: &str) -> Result<String, String> {
    let mut dbu = 1000.0;
    let mut layers: Vec<RouteLayer> = Vec::new();
    let mut tech: Vec<Option<RcLayer>> = Vec::new();
    let mut wires: Vec<Wire> = Vec::new();
    let mut passive: Vec<PassiveShape> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let mut t = line.split_whitespace();
        match t.next() {
            None => {}
            Some("D") => dbu = parse(t.next(), line)?,
            Some("L") => {
                let name: String = parse(t.next(), line)?;
                let metal: u32 = parse(t.next(), line)?;
                let width: i64 = parse(t.next(), line)?;
                let vertical: u8 = parse(t.next(), line)?;
                let sheet_ohm: f64 = parse(t.next(), line)?;
                let area: String = parse(t.next(), line)?;
                layers.push(RouteLayer {
                    name,
                    width,
                    spacing: width,
                    vertical: Some(vertical == 1),
                    min_area: None,
                    min_step: None,
                });
                tech.push(Some(RcLayer {
                    metal,
                    width,
                    sheet_ohm,
                    contexts: Vec::new(),
                    area_cap: area.parse().ok(),
                    overlap: Vec::new(),
                }));
            }
            Some("O") => {
                let lower: u32 = parse(t.next(), line)?;
                let value: f64 = parse(t.next(), line)?;
                let layer = tech.last_mut().and_then(Option::as_mut);
                layer
                    .ok_or_else(|| format!("O before L: {line}"))?
                    .overlap
                    .push((lower, value));
            }
            Some("C") => {
                let over: u32 = parse(t.next(), line)?;
                let under: i64 = parse(t.next(), line)?;
                let count: usize = parse(t.next(), line)?;
                let mut rows = Vec::with_capacity(count);
                for _ in 0..count {
                    let row = lines.next().ok_or("truncated rows")?;
                    let v: Vec<f64> = row
                        .split_whitespace()
                        .map(|x| x.parse().map_err(|_| format!("bad row: {row}")))
                        .collect::<Result<_, _>>()?;
                    if v.len() != 3 {
                        return Err(format!("bad row: {row}"));
                    }
                    rows.push([v[0], v[1], v[2]]);
                }
                let layer = tech.last_mut().and_then(Option::as_mut);
                layer
                    .ok_or_else(|| format!("C before L: {line}"))?
                    .contexts
                    .push(RcContext {
                        over,
                        under: u32::try_from(under).ok(),
                        rows,
                    });
            }
            Some("W") => {
                let v: Vec<i64> = t
                    .map(|x| x.parse().map_err(|_| format!("bad record: {line}")))
                    .collect::<Result<_, _>>()?;
                if v.len() != 6 {
                    return Err(format!("bad record: {line}"));
                }
                wires.push(Wire {
                    net: v[0] as u32,
                    layer: v[1] as u32,
                    start: [v[2], v[3]],
                    end: [v[4], v[5]],
                });
            }
            Some("G") => {
                let v: Vec<i64> = t
                    .map(|x| x.parse().map_err(|_| format!("bad record: {line}")))
                    .collect::<Result<_, _>>()?;
                if v.len() != 5 {
                    return Err(format!("bad record: {line}"));
                }
                passive.push(PassiveShape {
                    layer: v[0] as u32,
                    rect: Rect {
                        lx: v[1],
                        ly: v[2],
                        ux: v[3],
                        uy: v[4],
                    },
                    owner: PassiveOwner::Ground,
                });
            }
            Some(other) => return Err(format!("unknown record {other}")),
        }
    }
    let nets: Vec<u32> = wires
        .iter()
        .map(|w| w.net)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut access = Vec::new();
    let mut terminals = Vec::new();
    for &net in &nets {
        let w = wires
            .iter()
            .find(|w| w.net == net)
            .ok_or("net without wire")?;
        access.push(Access {
            pin: net,
            net,
            layer: w.layer,
            at: w.start,
        });
        terminals.push(Terminal {
            pin: net,
            net,
            driver: true,
            shapes: Vec::new(),
            class: None,
        });
    }
    let routes = Routes {
        wires,
        vias: Vec::new(),
        access,
        patches: Vec::new(),
        nets,
        stats: RouteStats::default(),
    };
    let tech = RcTech {
        dbu_per_um: dbu,
        layers: tech,
        via_ohm: Vec::new(),
    };
    let network = extract_cpu_ref(
        &ExtractInput {
            layers: &layers,
            vias: &[],
            routes: &routes,
            terminals: &terminals,
            passive: &passive,
            tech: &tech,
        },
        &ExtractLimits::default(),
    )
    .map_err(|e| e.to_string())?;
    let mut out = String::new();
    for n in &network.nets {
        let ground: f64 = n.ground.iter().sum();
        let coupling: f64 = network
            .couplings
            .iter()
            .filter(|c| c.a.0 == n.net || c.b.0 == n.net)
            .map(|c| c.pf)
            .sum();
        out += &format!("N {} {ground:.9e} {coupling:.9e}\n", n.net);
    }
    Ok(out)
}

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: extract_structures INPUT");
        return ExitCode::from(2);
    };
    let text = match std::fs::metadata(&path) {
        Ok(m) if m.len() <= 16 * 1024 * 1024 => std::fs::read_to_string(&path),
        Ok(_) => {
            eprintln!("input over 16 MiB");
            return ExitCode::from(2);
        }
        Err(e) => Err(e),
    };
    match text.map_err(|e| e.to_string()).and_then(|t| run(&t)) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}
