//! Bounded scalar-pin edits of original Verilog macro bus connections.
use crate::{Error, Result};
use rustroad_core::connectivity::Master;
use rustroad_formats::verilog::{Expr, Instance, Range};
use std::collections::HashMap;

pub(crate) fn replace_pin(
    instance: &mut Instance,
    master: &Master,
    pin: &str,
    replacement: Expr,
    declarations: &HashMap<String, Option<Range>>,
) -> Result<()> {
    if let Some(connection) = instance.connections.iter_mut().find(|c| c.pin == pin) {
        connection.signal = replacement;
        return Ok(());
    }
    let (bus, range) = master.bus_for_pin(pin).ok_or_else(|| {
        Error::Invalid(format!("missing sink connection {}.{pin}", instance.name))
    })?;
    let index = pin
        .rsplit_once('[')
        .and_then(|(_, i)| i.strip_suffix(']'))
        .and_then(|i| i.parse::<i64>().ok())
        .ok_or_else(|| Error::Invalid("invalid macro bit index".into()))?;
    let offset = (i128::from(index) - i128::from(range[0])).unsigned_abs() as usize;
    let width = (i128::from(range[0]) - i128::from(range[1])).unsigned_abs() + 1;
    if width > 65_536 {
        return Err(Error::Budget("macro connection edit width".into()));
    }
    let connection = instance
        .connections
        .iter_mut()
        .find(|c| c.pin == bus)
        .ok_or_else(|| Error::Invalid("missing macro bus connection".into()))?;
    let mut stack = vec![(&connection.signal, 0usize)];
    let mut bits = Vec::with_capacity(width as usize);
    while let Some((expr, depth)) = stack.pop() {
        if depth > 64 || stack.len() > 65_536 || bits.len() > 65_536 {
            return Err(Error::Budget("macro connection expansion".into()));
        }
        match expr {
            Expr::Ident(name) => match declarations.get(name) {
                Some(Some(range)) => {
                    let n = (i128::from(range.msb) - i128::from(range.lsb)).unsigned_abs() + 1;
                    if n > 65_536 - bits.len() as u128 {
                        return Err(Error::Budget("macro signal width".into()));
                    }
                    for k in 0..n as i64 {
                        let index = if range.msb <= range.lsb {
                            range.msb + k
                        } else {
                            range.msb - k
                        };
                        bits.push(Expr::BitSelect {
                            name: name.clone(),
                            index,
                        });
                    }
                }
                Some(None) => bits.push(expr.clone()),
                None => return Err(Error::Invalid(format!("undeclared macro signal {name}"))),
            },
            Expr::BitSelect { .. } => bits.push(expr.clone()),
            Expr::PartSelect { name, range } => {
                let n = (i128::from(range.msb) - i128::from(range.lsb)).unsigned_abs() + 1;
                if n > 65_536 - bits.len() as u128 {
                    return Err(Error::Budget("macro part-select width".into()));
                }
                for k in 0..n as i64 {
                    bits.push(Expr::BitSelect {
                        name: name.clone(),
                        index: if range.msb <= range.lsb {
                            range.msb + k
                        } else {
                            range.msb - k
                        },
                    });
                }
            }
            Expr::Concat(parts) => {
                if parts.len() > 65_536usize.saturating_sub(stack.len()) {
                    return Err(Error::Budget("macro concatenation expansion".into()));
                }
                stack.extend(parts.iter().rev().map(|p| (p, depth + 1)))
            }
            _ => {
                return Err(Error::Invalid(
                    "macro connection edit requires materialized, connected bits".into(),
                ));
            }
        }
    }
    if bits.len() != width as usize || offset >= bits.len() {
        return Err(Error::Invalid("macro connection width mismatch".into()));
    }
    bits[offset] = replacement;
    connection.signal = Expr::Concat(bits);
    Ok(())
}
