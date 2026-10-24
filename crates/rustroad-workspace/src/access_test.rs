//! Read-only pin-access test layout: every used logic master, in each legal
//! row orientation, gets one instance per signal pin whose net is routed by the
//! production router (access table, via variants, same-net cleanup) to a small
//! target pin on the access layer over an adjacent spacer row. The DEF is for
//! external foundry-deck checks only; it is never written into the workspace.
//!
//! Resource preflight: tests <= masters x 2 orientations x pins (bounded by the
//! imported master set, here < 64); rows = 2 x tests; every row holds two taps,
//! one test cell and largest-first fillers. Routing reuses `RouteLimits` from
//! the routing profile, so its work and memory caps are unchanged.
use crate::routing::{RoutingConfig, access_class, route_technology, write_paths};
use crate::{Error, Result};
use rustroad_core::connectivity::{Direction, Geometry, Master};
use rustroad_core::routing::{self, Blockage, LayerRect, Terminal};
use rustroad_core::{Limits, Orientation, Rect, def_name, transform_rect};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

const MAX_TESTS: usize = 512;

fn fail(s: &str) -> Error {
    Error::Invalid(s.into())
}

struct Placed<'a> {
    name: String,
    test: Option<usize>,
    master: &'a Master,
    x: i64,
    y: i64,
    orientation: Orientation,
}

/// Largest-first filler decomposition of [x, end) (a one-site filler exists).
fn fill_run<'a>(fillers: &[&'a Master], mut x: i64, end: i64) -> Result<Vec<(&'a Master, i64)>> {
    let mut out = Vec::new();
    while x < end {
        let m = fillers
            .iter()
            .find(|m| x + m.size[0] <= end)
            .ok_or_else(|| fail("filler decomposition"))?;
        out.push((*m, x));
        x += m.size[0];
    }
    Ok(out)
}

/// Write the access-test DEF for the workspace HEAD with the given profile.
pub fn export_access_test(
    path: &Path,
    config_path: &Path,
    limits: &Limits,
    output: &mut impl Write,
) -> Result<usize> {
    let snapshot = crate::read(path, limits)?;
    let metadata = snapshot.metadata();
    let design = &metadata.design;
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("physical design required"))?;
    let cfg = bundle
        .config
        .as_ref()
        .ok_or_else(|| fail("physical configuration required"))?;
    let tap_name = &cfg
        .tap
        .as_ref()
        .ok_or_else(|| fail("access test needs a tap policy"))?
        .cell;
    let (text, _) = crate::pdk::read_input(config_path, 1024 * 1024, "routing_config")?;
    let config: RoutingConfig = serde_json::from_str(&text)?;
    let (layers, vias, indices) = route_technology(bundle, &config)?;
    let access_layer = *indices
        .get(config.access_layer.as_str())
        .ok_or_else(|| fail("configured access layer absent"))?;
    let grid = bundle
        .technology
        .manufacturing_grid
        .ok_or_else(|| fail("manufacturing grid missing"))?;
    let model = &bundle.connectivity;
    let by_name: HashMap<&str, (usize, &Master)> = model
        .masters
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), (i, m)))
        .collect();
    let tap = by_name
        .get(tap_name.as_str())
        .ok_or_else(|| fail("tap master absent"))?
        .1;
    let mut fillers: Vec<&Master> = cfg
        .filler_cells
        .iter()
        .map(|n| by_name.get(n.as_str()).map(|m| m.1))
        .collect::<Option<_>>()
        .ok_or_else(|| fail("filler master absent"))?;
    fillers.sort_by_key(|m| std::cmp::Reverse(m.size[0]));
    let (sw, sh) = (design.site.width, design.site.height);
    if fillers.last().is_none_or(|m| m.size[0] != sw) {
        return Err(fail("access test needs a one-site filler"));
    }
    // Signal pins with routing-layer geometry of every used logic master.
    let mut used: Vec<&str> = bundle
        .netlist
        .instances
        .iter()
        .map(|i| i.cell.as_str())
        .filter(|c| c != tap_name && !cfg.filler_cells.iter().any(|f| f == c))
        .collect();
    used.sort_unstable();
    used.dedup();
    let routable = |g: &Geometry| match g {
        Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => indices
            .get(layer.as_str())
            .is_some_and(|i| layers[*i as usize].vertical.is_some()),
    };
    let mut tests = Vec::new();
    for name in used {
        let (mi, m) = by_name[name];
        let orientations: &[Orientation] = if m.symmetry.iter().any(|s| s == "X") {
            &[Orientation::N, Orientation::FS]
        } else {
            &[Orientation::N]
        };
        for &o in orientations {
            for (pi, pin) in m.pins.iter().enumerate() {
                if !pin.use_.is_supply() && pin.ports.iter().flatten().any(routable) {
                    tests.push((mi, m, o, pi));
                }
            }
        }
    }
    if tests.is_empty() || tests.len() > MAX_TESTS {
        return Err(Error::Budget("access test count".into()));
    }
    let slot = tests.iter().map(|t| t.1.size[0]).max().unwrap();
    let row_width = 2 * tap.size[0] + slot;
    let (x0, y0) = (16 * sw, 4 * sh);
    let rows = 2 * tests.len() as i64;
    let die = Rect {
        lx: 0,
        ly: 0,
        ux: 2 * x0 + row_width,
        uy: 2 * y0 + rows * sh,
    };
    let mut placed: Vec<Placed> = Vec::new();
    let mut test_rows = Vec::new();
    for (k, t) in tests.iter().enumerate() {
        let base = 2 * k as i64;
        let (test_row, spacer_row) = if t.2 == Orientation::N {
            (base, base + 1)
        } else {
            (base + 1, base)
        };
        for row in [base, base + 1] {
            let o = if row % 2 == 0 {
                Orientation::N
            } else {
                Orientation::FS
            };
            let y = y0 + row * sh;
            placed.push(Placed {
                name: format!("tap_{row}_l"),
                test: None,
                master: tap,
                x: x0,
                y,
                orientation: o,
            });
            let mut x = x0 + tap.size[0];
            if row == test_row {
                placed.push(Placed {
                    name: format!("t{k}"),
                    test: Some(k),
                    master: t.1,
                    x,
                    y,
                    orientation: o,
                });
                x += t.1.size[0];
            }
            for (m, fx) in fill_run(&fillers, x, x0 + tap.size[0] + slot)? {
                placed.push(Placed {
                    name: format!("fill_{row}_{fx}"),
                    test: None,
                    master: m,
                    x: fx,
                    y,
                    orientation: o,
                });
            }
            placed.push(Placed {
                name: format!("tap_{row}_r"),
                test: None,
                master: tap,
                x: x0 + tap.size[0] + slot,
                y,
                orientation: o,
            });
        }
        test_rows.push((test_row, spacer_row));
    }
    // Obstacles (every placed pin/OBS shape), terminals and targets.
    let mut obstacles = Vec::new();
    let mut terminals = Vec::new();
    let mut classes = Vec::new();
    let mut class_ids = HashMap::new();
    let mut targets = Vec::new();
    let rule = &layers[access_layer as usize];
    let side = {
        let area = u128::try_from(rule.min_area.unwrap_or(0)).unwrap_or(0);
        let mut s = area.isqrt();
        if s * s < area {
            s += 1;
        }
        let s = i64::try_from(s).map_err(|_| fail("target size"))?;
        let unit = 2 * grid;
        (s.max(rule.width) + unit - 1) / unit * unit
    };
    for cell in &placed {
        let test = cell.test;
        for (pi, pin) in cell.master.pins.iter().enumerate() {
            let own = test.filter(|k| tests[*k].3 == pi);
            let mut shapes = Vec::new();
            for g in pin.ports.iter().flatten() {
                let layer = match g {
                    Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
                };
                let Some(&li) = indices.get(layer.as_str()) else {
                    continue;
                };
                let Geometry::Rect { rect, .. } = g else {
                    return Err(fail("access test polygon pin unsupported"));
                };
                let r = Rect {
                    lx: rect[0],
                    ly: rect[1],
                    ux: rect[2],
                    uy: rect[3],
                };
                let r = transform_rect(r, cell.x, cell.y, sh, cell.orientation)?;
                obstacles.push(Blockage {
                    layer: li,
                    rect: r,
                    net: own.map(|k| k as u32),
                });
                if layers[li as usize].vertical.is_some() {
                    shapes.push(LayerRect { layer: li, rect: r });
                }
            }
            if let Some(k) = own {
                let (mi, _, o, _) = tests[k];
                let key = (mi, o == Orientation::FS, pi);
                let id = match class_ids.get(&key) {
                    Some(id) => *id,
                    None => {
                        let id = classes.len() as u32;
                        classes.push(access_class(cell.master, o, sh, pi, &indices, &layers)?);
                        class_ids.insert(key, id);
                        id
                    }
                };
                let driver = pin.direction == Some(Direction::Output);
                let cx = (shapes.iter().map(|s| s.rect.lx + s.rect.ux).sum::<i64>()
                    / (2 * shapes.len() as i64))
                    .div_euclid(grid)
                    * grid;
                let cy = y0 + test_rows[k].1 * sh + sh / 2;
                let cy = cy.div_euclid(grid) * grid;
                let target = Rect {
                    lx: cx - side / 2,
                    ly: cy - side / 2,
                    ux: cx + side / 2,
                    uy: cy + side / 2,
                };
                obstacles.push(Blockage {
                    layer: access_layer,
                    rect: target,
                    net: Some(k as u32),
                });
                terminals.push(Terminal {
                    pin: 2 * k as u32,
                    net: k as u32,
                    driver,
                    shapes,
                    class: config.lef_rules.then_some((id, [cell.x, cell.y])),
                });
                terminals.push(Terminal {
                    pin: 2 * k as u32 + 1,
                    net: k as u32,
                    driver: !driver,
                    shapes: vec![LayerRect {
                        layer: access_layer,
                        rect: target,
                    }],
                    class: None,
                });
                targets.push(target);
            }
        }
        for g in &cell.master.obstructions {
            let layer = match g {
                Geometry::Rect { layer, .. } | Geometry::Polygon { layer, .. } => layer,
            };
            if let Some(&li) = indices.get(layer.as_str()) {
                let Geometry::Rect { rect, .. } = g else {
                    return Err(fail("access test polygon OBS unsupported"));
                };
                let r = Rect {
                    lx: rect[0],
                    ly: rect[1],
                    ux: rect[2],
                    uy: rect[3],
                };
                obstacles.push(Blockage {
                    layer: li,
                    rect: transform_rect(r, cell.x, cell.y, sh, cell.orientation)?,
                    net: None,
                });
            }
        }
    }
    if !config.lef_rules {
        classes.clear();
    }
    let input = routing::RouteInput {
        die,
        grid,
        step: config.grid_step,
        access_layer,
        layers,
        vias,
        obstacles,
        terminals,
        rules: config.lef_rules,
        access_classes: classes,
        tracks: None,
        ties: Vec::new(),
        held_bytes: 0,
    };
    // The access test always uses the lattice router; a track-router profile
    // may carry limits above the lattice caps.
    let limits = if config.track_router.is_some() {
        routing::RouteLimits::default()
    } else {
        config.limits.clone()
    };
    let routes = routing::route_cpu_ref(&input, &limits)?;
    // DEF
    let names: Vec<String> = input.layers.iter().map(|l| l.name.clone()).collect();
    let via_names: Vec<String> = input.vias.iter().map(|v| v.name.clone()).collect();
    let lower: Vec<u32> = input.vias.iter().map(|v| v.lower).collect();
    let o = |o: Orientation| if o == Orientation::N { "N" } else { "FS" };
    writeln!(output, "VERSION 5.8 ;")?;
    writeln!(output, "DIVIDERCHAR \"/\" ;")?;
    writeln!(output, "BUSBITCHARS \"[]\" ;")?;
    writeln!(output, "DESIGN rustroad_access_test ;")?;
    writeln!(
        output,
        "UNITS DISTANCE MICRONS {} ;",
        design.units_per_micron
    )?;
    writeln!(
        output,
        "DIEAREA ( {} {} ) ( {} {} ) ;",
        die.lx, die.ly, die.ux, die.uy
    )?;
    for row in 0..rows {
        writeln!(
            output,
            "ROW row{row} {} {x0} {} {} DO {} BY 1 STEP {sw} 0 ;",
            def_name(&design.site.name),
            y0 + row * sh,
            o(if row % 2 == 0 {
                Orientation::N
            } else {
                Orientation::FS
            }),
            row_width / sw
        )?;
    }
    writeln!(output, "COMPONENTS {} ;", placed.len())?;
    for c in &placed {
        writeln!(
            output,
            "- {} {} + PLACED ( {} {} ) {} ;",
            c.name,
            def_name(&c.master.name),
            c.x,
            c.y,
            o(c.orientation)
        )?;
    }
    writeln!(output, "END COMPONENTS")?;
    writeln!(output, "PINS {} ;", targets.len())?;
    for (k, t) in targets.iter().enumerate() {
        writeln!(
            output,
            "- acc{k} + NET acc{k} + DIRECTION INPUT + USE SIGNAL + PORT + LAYER {} ( 0 0 ) ( {} {} ) + FIXED ( {} {} ) N ;",
            def_name(&names[access_layer as usize]),
            t.ux - t.lx,
            t.uy - t.ly,
            t.lx,
            t.ly
        )?;
    }
    writeln!(output, "END PINS")?;
    writeln!(output, "NETS {} ;", tests.len())?;
    for (k, t) in tests.iter().enumerate() {
        write!(
            output,
            "- acc{k} ( t{k} {} ) ( PIN acc{k} ) + USE SIGNAL",
            def_name(&t.1.pins[t.3].name)
        )?;
        let wires: Vec<_> = routes.wires.iter().filter(|w| w.net == k as u32).collect();
        let vs: Vec<_> = routes.vias.iter().filter(|v| v.net == k as u32).collect();
        let ps: Vec<_> = routes
            .patches
            .iter()
            .filter(|p| p.net == k as u32)
            .collect();
        write_paths(&names, &via_names, &lower, &wires, &vs, &ps, output)?;
        writeln!(output, " ;")?;
    }
    writeln!(output, "END NETS")?;
    writeln!(output, "END DESIGN")?;
    Ok(tests.len())
}
