// SPDX-License-Identifier: Apache-2.0
//! Explicit master-bus projection for bounded file ECO and report binding.
use super::{EcoCells, EcoError, Result};
use crate::verilog::{Expr, Instance, Netlist, Range, VerilogLimits};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Master -> formal port -> original Verilog [msb, lsb]. Never inferred from names.
pub type PinBuses = BTreeMap<String, BTreeMap<String, [i64; 2]>>;
const MAX_BUS_BITS: usize = 65_536;
const MAX_SCHEMA_MASTERS: usize = 256;

/// Owned declaration index and a cumulative expression-work allowance.
#[derive(Debug)]
pub struct PinView<'a> {
    cells: &'a EcoCells,
    declarations: HashMap<String, Option<Range>>,
    remaining: usize,
    remaining_storage: usize,
    max_depth: usize,
    max_declarations: usize,
}

fn invalid(text: impl Into<String>) -> EcoError {
    EcoError::Invalid(text.into())
}
fn budget() -> EcoError {
    EcoError::Budget("explicit bus pin expansion".into())
}
fn width(range: [i64; 2]) -> Result<usize> {
    let n = (i128::from(range[0]) - i128::from(range[1])).unsigned_abs() + 1;
    if n > MAX_BUS_BITS as u128 {
        return Err(budget());
    }
    Ok(n as usize)
}
fn position(range: [i64; 2], index: i64) -> Result<usize> {
    if index < range[0].min(range[1]) || index > range[0].max(range[1]) {
        return Err(invalid("formal bus index outside its explicit range"));
    }
    Ok((i128::from(index) - i128::from(range[0])).unsigned_abs() as usize)
}
fn indices(range: [i64; 2]) -> Result<impl Iterator<Item = i64>> {
    let n = width(range)?;
    Ok((0..n).map(move |i| {
        if range[0] <= range[1] {
            range[0] + i as i64
        } else {
            range[0] - i as i64
        }
    }))
}

impl<'a> PinView<'a> {
    /// Validate explicit formal ranges and their scalar LEF member directions.
    ///
    /// # Errors
    /// Rejects excessive schemas, missing/mixed directions and ambiguous declarations.
    pub fn new(n: &Netlist, cells: &'a EcoCells, limits: &VerilogLimits) -> Result<Self> {
        if cells.pin_buses.len() > MAX_SCHEMA_MASTERS {
            return Err(budget());
        }
        let mut total = 0usize;
        let mut name_bytes = 0usize;
        for (master, buses) in &cells.pin_buses {
            for (port, range) in buses {
                total = total.checked_add(width(*range)?).ok_or_else(budget)?;
                if total > MAX_BUS_BITS {
                    return Err(budget());
                }
                name_bytes =
                    name_bytes
                        .checked_add(width(*range)?.saturating_mul(
                            master.len().saturating_add(port.len()).saturating_add(24),
                        ))
                        .ok_or_else(budget)?;
                if name_bytes > 8 * 1024 * 1024 {
                    return Err(budget());
                }
                let directions = cells.pin_directions.get(master).ok_or_else(|| {
                    invalid(format!("bus master {master} lacks LEF pin directions"))
                })?;
                let mut expected = None;
                for bit in indices(*range)? {
                    let pin = format!("{port}[{bit}]");
                    let d = directions.get(&pin).ok_or_else(|| {
                        invalid(format!(
                            "explicit bus member {master}/{pin} has no LEF direction"
                        ))
                    })?;
                    if !matches!(d.as_str(), "INPUT" | "OUTPUT" | "INOUT")
                        || expected.is_some_and(|e| e != d)
                    {
                        return Err(invalid(format!(
                            "explicit bus {master}/{port} has unsupported or mixed directions"
                        )));
                    }
                    expected = Some(d);
                }
            }
        }
        let mut declarations = HashMap::new();
        for (name, range) in n
            .ports
            .iter()
            .map(|p| (&p.name, p.range))
            .chain(n.wires.iter().map(|w| (&w.name, w.range)))
        {
            if let Some(old) = declarations.insert(name.clone(), range)
                && old != range
            {
                return Err(invalid("inconsistent scalar/vector declarations"));
            }
        }
        Ok(Self {
            cells,
            declarations,
            remaining: limits.max_expr_nodes,
            remaining_storage: limits
                .max_expr_nodes
                .saturating_mul(128)
                .min(64 * 1024 * 1024),
            max_depth: limits.max_depth.min(64),
            max_declarations: limits.max_nets.saturating_add(limits.max_ports),
        })
    }
    fn tick(&mut self, count: usize) -> Result<()> {
        self.remaining = self.remaining.checked_sub(count).ok_or_else(budget)?;
        Ok(())
    }
    fn storage(&mut self, count: usize) -> Result<()> {
        self.remaining_storage = self
            .remaining_storage
            .checked_sub(count)
            .ok_or_else(budget)?;
        Ok(())
    }
    fn cloned(&mut self, expr: &Expr) -> Result<Expr> {
        let mut stack = vec![(expr, 0usize)];
        while let Some((node, depth)) = stack.pop() {
            if depth > self.max_depth || stack.len() > MAX_BUS_BITS {
                return Err(budget());
            }
            self.tick(1)?;
            let bytes = match node {
                Expr::Ident(s) | Expr::Constant(s) => s.len(),
                Expr::BitSelect { name, .. } | Expr::PartSelect { name, .. } => name.len(),
                _ => 0,
            };
            self.storage(std::mem::size_of::<Expr>().saturating_add(bytes))?;
            if let Expr::Concat(parts) = node {
                if parts.len() > MAX_BUS_BITS.saturating_sub(stack.len()) {
                    return Err(budget());
                }
                stack.extend(parts.iter().map(|p| (p, depth + 1)));
            }
        }
        Ok(expr.clone())
    }
    /// Register a newly created scalar ECO wire before projecting a later operation.
    pub fn add_scalar(&mut self, name: &str) -> Result<()> {
        if self.declarations.len() >= self.max_declarations {
            return Err(budget());
        }
        if self.declarations.insert(name.into(), None).is_some() {
            return Err(invalid("new ECO wire was already declared"));
        }
        Ok(())
    }
    fn member(&self, master: &str, pin: &str) -> Result<Option<(String, [i64; 2], usize)>> {
        let Some((port, tail)) = pin.rsplit_once('[') else {
            return Ok(None);
        };
        let Some(range) = self
            .cells
            .pin_buses
            .get(master)
            .and_then(|b| b.get(port))
            .copied()
        else {
            return Ok(None);
        };
        let index = tail
            .strip_suffix(']')
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| invalid("invalid formal bus bit name"))?;
        Ok(Some((port.into(), range, position(range, index)?)))
    }
    fn constant(&mut self, literal: &str, context: Option<usize>) -> Result<Vec<Expr>> {
        if literal.len() > MAX_BUS_BITS * 2 + 32 {
            return Err(budget());
        }
        self.tick(literal.len())?;
        let text = literal.replace('_', "").to_ascii_lowercase();
        if matches!(text.as_str(), "'0" | "'1" | "'x" | "'z") {
            let n = context.unwrap_or(1);
            self.tick(n)?;
            self.storage(n.saturating_mul(std::mem::size_of::<Expr>() + 4))?;
            return Ok(vec![Expr::Constant(format!("1'b{}", &text[1..])); n]);
        }
        let (size, suffix) = text
            .split_once('\'')
            .ok_or_else(|| invalid("bus ECO requires explicitly sized constants"))?;
        let n = size
            .parse::<usize>()
            .map_err(|_| invalid("bus ECO requires explicitly sized constants"))?;
        if n == 0 || n > MAX_BUS_BITS {
            return Err(budget());
        }
        self.tick(n)?;
        self.storage(n.saturating_mul(std::mem::size_of::<Expr>() + 4))?;
        if suffix.is_empty() {
            return Err(invalid("constant base is missing"));
        }
        let (base, digits) = suffix.split_at(1);
        let mut bits = Vec::new();
        match base {
            "b" | "o" | "h" => {
                let radix: u32 = if base == "b" {
                    2
                } else if base == "o" {
                    8
                } else {
                    16
                };
                let step = if base == "b" {
                    1
                } else if base == "o" {
                    3
                } else {
                    4
                };
                let selected: Vec<char> = digits.chars().rev().take(n.div_ceil(step)).collect();
                for d in selected.into_iter().rev() {
                    if matches!(d, 'x' | 'z' | '?') {
                        bits.extend(std::iter::repeat_n(if d == '?' { 'z' } else { d }, step));
                    } else {
                        let value = d
                            .to_digit(radix)
                            .ok_or_else(|| invalid("invalid bus constant digit"))?;
                        for shift in (0..step).rev() {
                            bits.push(if value & (1 << shift) == 0 { '0' } else { '1' });
                        }
                    }
                }
            }
            "d" if matches!(digits, "x" | "z" | "?") => {
                bits.resize(n, if digits == "x" { 'x' } else { 'z' })
            }
            "d" => {
                let value = digits.parse::<u128>().map_err(|_| {
                    invalid("decimal bus constant exceeds supported u128; use binary/hex")
                })?;
                for shift in (0..n).rev() {
                    bits.push(if shift >= 128 || value & (1u128 << shift) == 0 {
                        '0'
                    } else {
                        '1'
                    });
                }
            }
            _ => return Err(invalid("unsupported bus constant base")),
        }
        if bits.len() > MAX_BUS_BITS.saturating_add(4) {
            return Err(budget());
        }
        let pad = bits
            .first()
            .copied()
            .filter(|b| matches!(b, 'x' | 'z'))
            .unwrap_or('0');
        if bits.len() < n {
            let mut prefix = vec![pad; n - bits.len()];
            prefix.extend(bits);
            bits = prefix;
        }
        let start = bits.len().saturating_sub(n);
        Ok(bits[start..]
            .iter()
            .map(|b| Expr::Constant(format!("1'b{b}")))
            .collect())
    }
    /// Expand an unsigned structural expression in Verilog MSB-first order.
    /// Exact widths are required; implicit truncation/extension is rejected.
    pub fn expand(&mut self, expr: &Expr, context: Option<usize>) -> Result<Vec<Expr>> {
        let mut todo = vec![(expr, 0usize, context)];
        let mut bits = Vec::new();
        while let Some((e, depth, ctx)) = todo.pop() {
            self.tick(1)?;
            if depth > self.max_depth || bits.len() > MAX_BUS_BITS || todo.len() > MAX_BUS_BITS {
                return Err(budget());
            }
            match e {
                Expr::Ident(name) => match self.declarations.get(name).copied() {
                    Some(Some(r)) => {
                        let range = [r.msb, r.lsb];
                        self.tick(width(range)?)?;
                        self.storage(
                            width(range)?.saturating_mul(std::mem::size_of::<Expr>() + name.len()),
                        )?;
                        bits.extend(indices(range)?.map(|index| Expr::BitSelect {
                            name: name.clone(),
                            index,
                        }));
                    }
                    Some(None) => bits.push(self.cloned(e)?),
                    None => return Err(invalid(format!("undeclared bus signal {name}"))),
                },
                Expr::BitSelect { name, index } => {
                    match self.declarations.get(name) {
                        Some(Some(r)) => {
                            position([r.msb, r.lsb], *index)?;
                        }
                        _ => return Err(invalid("bit select requires a declared vector")),
                    }
                    bits.push(self.cloned(e)?);
                }
                Expr::PartSelect { name, range } => {
                    let declared = self
                        .declarations
                        .get(name)
                        .copied()
                        .flatten()
                        .ok_or_else(|| invalid("part select requires a declared vector"))?;
                    position([declared.msb, declared.lsb], range.msb)?;
                    position([declared.msb, declared.lsb], range.lsb)?;
                    if (declared.msb < declared.lsb) != (range.msb < range.lsb)
                        && range.msb != range.lsb
                    {
                        return Err(invalid(
                            "part select reverses the declared signal direction",
                        ));
                    }
                    self.tick(width([range.msb, range.lsb])?)?;
                    self.storage(
                        width([range.msb, range.lsb])?
                            .saturating_mul(std::mem::size_of::<Expr>() + name.len()),
                    )?;
                    bits.extend(
                        indices([range.msb, range.lsb])?.map(|index| Expr::BitSelect {
                            name: name.clone(),
                            index,
                        }),
                    );
                }
                Expr::Concat(parts) => {
                    if parts.len() > MAX_BUS_BITS.saturating_sub(todo.len()) {
                        return Err(budget());
                    }
                    todo.extend(parts.iter().rev().map(|p| (p, depth + 1, None)));
                }
                Expr::Constant(value) => bits.extend(self.constant(value, ctx)?),
                Expr::Unconnected => {
                    let n =
                        ctx.ok_or_else(|| invalid("open expression is not a whole formal bus"))?;
                    self.tick(n)?;
                    self.storage(n.saturating_mul(std::mem::size_of::<Expr>()))?;
                    bits.extend(std::iter::repeat_n(Expr::Unconnected, n));
                }
            }
        }
        if bits.len() > MAX_BUS_BITS || context.is_some_and(|n| n != bits.len()) {
            return Err(invalid("explicit bus connection width mismatch"));
        }
        Ok(bits)
    }
    /// Enumerate formal scalar members without guessing bracketed scalar names.
    pub fn connections(&mut self, inst: &Instance) -> Result<Vec<(String, Expr)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for c in &inst.connections {
            self.tick(1)?;
            if let Some(range) = self
                .cells
                .pin_buses
                .get(&inst.cell)
                .and_then(|b| b.get(&c.pin))
                .copied()
            {
                let bits = self.expand(&c.signal, Some(width(range)?))?;
                for (index, bit) in indices(range)?.zip(bits) {
                    self.storage(c.pin.len().saturating_add(48))?;
                    let pin = format!("{}[{index}]", c.pin);
                    if !seen.insert(pin.clone()) {
                        return Err(invalid("duplicate explicit bus member connection"));
                    }
                    out.push((pin, bit));
                }
            } else {
                if self.member(&inst.cell, &c.pin)?.is_some() {
                    return Err(invalid(
                        "explicit bus member supplied as a standalone named port",
                    ));
                }
                if !seen.insert(c.pin.clone()) {
                    return Err(invalid("duplicate scalar connection"));
                }
                self.storage(c.pin.len().saturating_add(24))?;
                out.push((c.pin.clone(), self.cloned(&c.signal)?));
            }
            if out.len() > MAX_BUS_BITS {
                return Err(budget());
            }
        }
        Ok(out)
    }
    /// Obtain exactly one named scalar/member expression.
    pub fn get(&mut self, inst: &Instance, pin: &str) -> Result<Expr> {
        self.connections(inst)?
            .into_iter()
            .find_map(|(p, e)| (p == pin).then_some(e))
            .ok_or_else(|| invalid(format!("sink pin {}/{pin} is not connected", inst.name)))
    }
    /// Replace one bit while retaining the grouped formal port and all siblings.
    pub fn replace(&mut self, inst: &mut Instance, pin: &str, replacement: Expr) -> Result<Expr> {
        if let Some((port, range, offset)) = self.member(&inst.cell, pin)? {
            if inst.connections.iter().any(|c| c.pin == pin) {
                return Err(invalid("ambiguous explicit bus member port"));
            }
            let c = inst
                .connections
                .iter_mut()
                .find(|c| c.pin == port)
                .ok_or_else(|| invalid("grouped formal bus is absent"))?;
            let mut bits = self.expand(&c.signal, Some(width(range)?))?;
            let old = std::mem::replace(&mut bits[offset], replacement);
            c.signal = Expr::Concat(bits);
            Ok(old)
        } else {
            let c = inst
                .connections
                .iter_mut()
                .find(|c| c.pin == pin)
                .ok_or_else(|| invalid("scalar formal pin is absent"))?;
            Ok(std::mem::replace(&mut c.signal, replacement))
        }
    }
}
