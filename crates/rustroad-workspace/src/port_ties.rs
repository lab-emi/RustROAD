//! Bounded separation of constant output aliases without changing RTL values.
use crate::{Error, Result};
use rustroad_formats::verilog::{Assign, Expr, Netlist, Range, Wire};
use std::collections::{HashMap, HashSet};

struct PortMap {
    range: Option<Range>,
    bits: HashMap<Option<i64>, String>,
}

fn expand(name: &str, range: Range, map: &PortMap, work: &mut usize, cap: usize) -> Result<Expr> {
    let width = (i128::from(range.msb) - i128::from(range.lsb)).unsigned_abs() + 1;
    if width > 65_536 {
        return Err(Error::Budget("constant-port vector width".into()));
    }
    *work = work
        .checked_add(width as usize)
        .filter(|&n| n <= cap)
        .ok_or_else(|| Error::Budget("constant-port expanded-node limit".into()))?;
    let direction = if range.msb <= range.lsb { 1 } else { -1 };
    let mut parts = Vec::with_capacity(width as usize);
    for offset in 0..width {
        let index = (i128::from(range.msb) + direction * offset as i128) as i64;
        parts.push(match map.bits.get(&Some(index)) {
            Some(alias) => Expr::Ident(alias.clone()),
            None => Expr::BitSelect {
                name: name.into(),
                index,
            },
        });
    }
    Ok(if parts.len() == 1 {
        parts.pop().unwrap()
    } else {
        Expr::Concat(parts)
    })
}

fn rewrite(
    expr: &mut Expr,
    maps: &HashMap<String, PortMap>,
    work: &mut usize,
    cap: usize,
    depth: usize,
) -> Result<()> {
    *work = work
        .checked_add(1)
        .filter(|&n| n <= cap)
        .ok_or_else(|| Error::Budget("constant-port rewrite work".into()))?;
    if depth > 32 {
        return Err(Error::Budget("constant-port rewrite depth".into()));
    }
    let replacement =
        match expr {
            Expr::Ident(name) => match maps.get(name) {
                Some(map) => match map.range {
                    Some(range) => Some(expand(name, range, map, work, cap)?),
                    None => map.bits.get(&None).map(|s| Expr::Ident(s.clone())),
                },
                None => None,
            },
            Expr::BitSelect { name, index } => maps
                .get(name)
                .and_then(|m| m.bits.get(&Some(*index)))
                .map(|s| Expr::Ident(s.clone())),
            Expr::PartSelect { name, range } => match maps.get(name) {
                Some(map)
                    if map.bits.keys().flatten().any(|i| {
                        *i >= range.msb.min(range.lsb) && *i <= range.msb.max(range.lsb)
                    }) =>
                {
                    Some(expand(name, *range, map, work, cap)?)
                }
                _ => None,
            },
            _ => None,
        };
    if let Some(replacement) = replacement {
        *expr = replacement;
    } else if let Expr::Concat(parts) = expr {
        for part in parts {
            rewrite(part, maps, work, cap, depth + 1)?;
        }
    }
    Ok(())
}

pub(crate) fn split(
    netlist: &mut Netlist,
    moved: Vec<(Expr, String)>,
    names: &mut HashSet<String>,
    sequence: &mut usize,
    cap: usize,
) -> Result<()> {
    if moved.is_empty() {
        return Ok(());
    }
    if netlist.wires.len().saturating_add(moved.len()) > 32_000_000 {
        return Err(Error::Budget("constant-port declaration count".into()));
    }
    let mut maps: HashMap<String, PortMap> = HashMap::new();
    for (port, _) in &moved {
        let (name, bit) = match port {
            Expr::Ident(n) => (n, None),
            Expr::BitSelect { name, index } => (name, Some(*index)),
            _ => return Err(Error::Invalid("constant-port scalar identity".into())),
        };
        let range = netlist
            .ports
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| Error::Invalid("constant-port declaration missing".into()))?
            .range;
        let alias = loop {
            if *sequence > cap {
                return Err(Error::Budget("constant-port name search".into()));
            }
            let alias = format!("rr_tie_copy_port_{sequence}");
            *sequence += 1;
            if names.insert(alias.clone()) {
                break alias;
            }
        };
        netlist.wires.push(Wire {
            name: alias.clone(),
            range: None,
        });
        if maps
            .entry(name.clone())
            .or_insert_with(|| PortMap {
                range,
                bits: HashMap::new(),
            })
            .bits
            .insert(bit, alias)
            .is_some()
        {
            return Err(Error::Invalid("duplicate constant-port split".into()));
        }
    }
    let mut work = 0;
    for assign in &mut netlist.assigns {
        rewrite(&mut assign.lhs, &maps, &mut work, cap, 0)?;
        rewrite(&mut assign.rhs, &maps, &mut work, cap, 0)?;
    }
    for instance in &mut netlist.instances {
        for pin in &mut instance.connections {
            rewrite(&mut pin.signal, &maps, &mut work, cap, 0)?;
        }
    }
    netlist
        .assigns
        .extend(moved.into_iter().map(|(lhs, net)| Assign {
            lhs,
            rhs: Expr::Ident(net),
        }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expanded_bits_are_charged_before_vector_allocation() {
        let mut netlist = rustroad_formats::verilog::parse_verilog(
            "module m(output [63:0] y); assign y=64'b0; endmodule",
            &Default::default(),
        )
        .unwrap();
        let result = split(
            &mut netlist,
            vec![(
                Expr::BitSelect {
                    name: "y".into(),
                    index: 0,
                },
                "source".into(),
            )],
            &mut HashSet::new(),
            &mut 0,
            10,
        );
        assert!(matches!(result, Err(Error::Budget(_))));
    }
}
