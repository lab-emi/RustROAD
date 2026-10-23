//! Bounded constant-bit materialization into caller-selected tie masters.
//!
//! Upfront budget: one immutable input AST (counted serialization <=16 MiB),
//! one rewritten AST, <=100,000 new cells/wires and <=1,000,000 decoded bits.
//! New records/bindings use approximately 40 MiB; work charges include copied
//! identifier bytes, bounded by 8 million work units by default. Together with
//! declaration/name maps and the input clone, planned additional RSS is <256 MiB.
//! One worker/corner; no path or fanout enumeration, recursion capped at 32.
//! Small tests use <1 MiB each and the workspace's two-worker resource guards.
//!
//! A new cell is made for each constant bit occurrence. An assignment alias can
//! still give that cell several downstream loads: the caller MUST check linked
//! fanout against its explicit bound. LEF does not define Boolean functions;
//! high/low semantics are selected by the caller and require an independent
//! Liberty/simulation/equivalence oracle. Power/ground pins remain omitted for
//! the explicit power-network construction stage, never guessed here.

use rustroad_core::valid_object_name;
use rustroad_formats::{lef, verilog};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TieLimits {
    pub max_work: usize,
    pub max_new_cells: usize,
    /// Global number of decoded/materialized literal bits.
    pub max_bits: usize,
}
impl Default for TieLimits {
    fn default() -> Self {
        Self {
            max_work: 8_000_000,
            max_new_cells: 100_000,
            max_bits: 1_000_000,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TieBinding {
    pub instance: String,
    pub net: String,
    pub pin: String,
    pub value: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TieReport {
    pub new_cells: usize,
    pub constant_bits: usize,
    pub high_cells: usize,
    pub low_cells: usize,
    /// Constant occurrences, not a proof of downstream electrical fanout.
    pub bindings: Vec<TieBinding>,
}
#[derive(Debug, thiserror::Error)]
pub enum TieError {
    #[error("TIE_INVALID: {0}")]
    Invalid(String),
    #[error("TIE_BUDGET: {0}")]
    Budget(String),
}
pub type TieResult<T> = std::result::Result<T, TieError>;
fn invalid(message: impl Into<String>) -> TieError {
    TieError::Invalid(message.into())
}
fn budget(message: &str) -> TieError {
    TieError::Budget(message.into())
}

struct Work<'a> {
    used: usize,
    limits: &'a TieLimits,
}
impl Work<'_> {
    fn charge(&mut self, amount: usize) -> TieResult<()> {
        self.used = self
            .used
            .checked_add(amount)
            .ok_or_else(|| budget("work count overflow"))?;
        if self.used > self.limits.max_work {
            return Err(budget("max_work exceeded"));
        }
        Ok(())
    }
}
struct Master<'a> {
    name: &'a str,
    selected: &'a str,
    outputs: Vec<&'a str>,
}
fn master<'a>(
    cell: &'a lef::LefMacro,
    selected: &'a str,
    work: &mut Work<'_>,
) -> TieResult<Master<'a>> {
    if !valid_object_name(&cell.name) || !valid_object_name(selected) {
        return Err(invalid("invalid tie master or output name"));
    }
    let mut names = HashSet::new();
    let mut outputs = Vec::new();
    for pin in &cell.pins {
        work.charge(1 + pin.name.len())?;
        if !valid_object_name(&pin.name) || !names.insert(pin.name.as_str()) {
            return Err(invalid("invalid/duplicate tie macro pin"));
        }
        let usage = pin.use_.as_deref().unwrap_or("SIGNAL");
        if matches!(usage, "POWER" | "GROUND") {
            continue;
        }
        if usage != "SIGNAL" || pin.direction.as_deref() != Some("OUTPUT") {
            return Err(invalid(
                "tie masters may contain only output SIGNAL pins and POWER/GROUND pins",
            ));
        }
        outputs.push(pin.name.as_str());
    }
    if !outputs.contains(&selected) {
        return Err(invalid(
            "selected tie output pin does not exist as an output SIGNAL",
        ));
    }
    Ok(Master {
        name: &cell.name,
        selected,
        outputs,
    })
}

// Count serialization without constructing a second unbounded JSON buffer.
fn input_bound(netlist: &verilog::Netlist) -> TieResult<()> {
    struct Counter {
        remaining: usize,
        exceeded: bool,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(std::io::Error::other("tie input AST byte cap"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter {
        remaining: 16 * 1024 * 1024,
        exceeded: false,
    };
    if let Err(error) = serde_json::to_writer(&mut counter, netlist) {
        if counter.exceeded {
            return Err(budget("input AST exceeds 16 MiB serialized limit"));
        }
        return Err(invalid(error.to_string()));
    }
    Ok(())
}

enum Literal<'a> {
    Fill(bool),
    Based {
        width: usize,
        base: u32,
        digits: &'a str,
    },
}
fn literal<'a>(text: &'a str, work: &mut Work<'_>) -> TieResult<Literal<'a>> {
    match text {
        "'0" => return Ok(Literal::Fill(false)),
        "'1" => return Ok(Literal::Fill(true)),
        _ => {}
    }
    let (size, data) = text
        .split_once('\'')
        .ok_or_else(|| invalid("unsized decimal constants require an explicit width"))?;
    if size.is_empty() {
        return Err(invalid("unsized based/X/Z constants are unsupported"));
    }
    let mut width = 0usize;
    let mut saw_digit = false;
    for byte in size.bytes() {
        work.charge(1)?;
        if byte == b'_' {
            continue;
        }
        if !byte.is_ascii_digit() {
            return Err(invalid("invalid literal width"));
        }
        saw_digit = true;
        width = width
            .checked_mul(10)
            .and_then(|v| v.checked_add(usize::from(byte - b'0')))
            .ok_or_else(|| budget("literal width overflow"))?;
    }
    if !saw_digit || width == 0 {
        return Err(invalid("literal width must be positive"));
    }
    if width > work.limits.max_bits {
        return Err(budget("literal exceeds max_bits"));
    }
    let mut chars = data.chars();
    let base = match chars.next() {
        Some('b' | 'B') => 2,
        Some('o' | 'O') => 8,
        Some('h' | 'H') => 16,
        Some('d' | 'D') => 10,
        Some('s' | 'S') => return Err(invalid("signed constants are unsupported")),
        _ => return Err(invalid("invalid literal base")),
    };
    let digits = chars.as_str();
    if digits.is_empty() {
        return Err(invalid("literal has no digits"));
    }
    Ok(Literal::Based {
        width,
        base,
        digits,
    })
}

struct Transform<'a> {
    work: Work<'a>,
    high: Master<'a>,
    low: Master<'a>,
    declared: HashMap<&'a str, Option<verilog::Range>>,
    names: HashSet<String>,
    next_name: usize,
    base_cells: usize,
    base_wires: usize,
    connections: usize,
    cells: Vec<verilog::Instance>,
    wires: Vec<verilog::Wire>,
    report: TieReport,
}
impl Transform<'_> {
    fn range_width(&self, range: verilog::Range) -> TieResult<usize> {
        let n = (i128::from(range.msb) - i128::from(range.lsb)).unsigned_abs() + 1;
        if n > self.work.limits.max_bits as u128 {
            return Err(budget("expression width exceeds max_bits"));
        }
        Ok(n as usize)
    }
    fn width(&mut self, expr: &verilog::Expr, depth: usize) -> TieResult<usize> {
        self.work.charge(1)?;
        if depth > 32 {
            return Err(budget("expression depth exceeds 32"));
        }
        match expr {
            verilog::Expr::Ident(name) => match self
                .declared
                .get(name.as_str())
                .ok_or_else(|| invalid("undeclared expression"))?
            {
                Some(range) => self.range_width(*range),
                None => Ok(1),
            },
            verilog::Expr::BitSelect { .. } => Ok(1),
            verilog::Expr::PartSelect { range, .. } => self.range_width(*range),
            verilog::Expr::Concat(parts) => {
                let mut total = 0usize;
                for part in parts {
                    total = total
                        .checked_add(self.width(part, depth + 1)?)
                        .ok_or_else(|| budget("concat width overflow"))?;
                    if total > self.work.limits.max_bits {
                        return Err(budget("concat width exceeds max_bits"));
                    }
                }
                Ok(total)
            }
            verilog::Expr::Constant(text) => match literal(text, &mut self.work)? {
                Literal::Based { width, .. } => Ok(width),
                Literal::Fill(_) => Err(invalid(
                    "unbased fill literal inside self-sized concatenation is unsupported",
                )),
            },
            verilog::Expr::Unconnected => Err(invalid("open expression has no width")),
        }
    }
    fn decode(&mut self, text: &str, context: Option<usize>) -> TieResult<Vec<bool>> {
        let literal = literal(text, &mut self.work)?;
        let width = match literal {
            Literal::Fill(_) => context
                .ok_or_else(|| invalid("fill literal requires an explicit destination width"))?,
            Literal::Based { width, .. } => {
                if context.is_some_and(|n| n != width) {
                    return Err(invalid(
                        "literal/destination width mismatch; implicit coercion is unsupported",
                    ));
                }
                width
            }
        };
        if width == 0
            || width
                > self
                    .work
                    .limits
                    .max_bits
                    .saturating_sub(self.report.constant_bits)
        {
            return Err(budget("global max_bits exceeded"));
        }
        self.work.charge(width)?;
        self.report.constant_bits += width;
        match literal {
            Literal::Fill(value) => Ok(vec![value; width]),
            Literal::Based {
                base: 10, digits, ..
            } => {
                let mut bytes = vec![0u8; width.div_ceil(8)];
                let mut any = false;
                for digit in digits.bytes() {
                    self.work.charge(1)?;
                    if digit == b'_' {
                        continue;
                    }
                    if !digit.is_ascii_digit() {
                        return Err(invalid("X/Z or invalid decimal literal digit"));
                    }
                    any = true;
                    let mut carry = u16::from(digit - b'0');
                    for byte in &mut bytes {
                        self.work.charge(1)?;
                        let next = u16::from(*byte) * 10 + carry;
                        *byte = next as u8;
                        carry = next >> 8;
                    }
                    if carry != 0 || (width % 8 != 0 && bytes[bytes.len() - 1] >> (width % 8) != 0)
                    {
                        return Err(invalid(
                            "literal value exceeds its declared width; truncation is unsupported",
                        ));
                    }
                }
                if !any {
                    return Err(invalid("literal has no digits"));
                }
                Ok((0..width)
                    .rev()
                    .map(|i| (bytes[i / 8] >> (i % 8)) & 1 != 0)
                    .collect())
            }
            Literal::Based { base, digits, .. } => {
                let per_digit = match base {
                    2 => 1,
                    8 => 3,
                    16 => 4,
                    _ => return Err(invalid("unsupported literal base")),
                };
                let mut result = vec![false; width];
                let mut position = 0usize;
                let mut any = false;
                for digit in digits.chars().rev() {
                    self.work.charge(1)?;
                    if digit == '_' {
                        continue;
                    }
                    let value = digit
                        .to_digit(base)
                        .ok_or_else(|| invalid("X/Z or invalid based literal digit"))?;
                    any = true;
                    for bit in 0..per_digit {
                        self.work.charge(1)?;
                        let value = (value >> bit) & 1 != 0;
                        if position < width {
                            result[width - 1 - position] = value;
                        } else if value {
                            return Err(invalid(
                                "literal value exceeds its declared width; truncation is unsupported",
                            ));
                        }
                        position = position
                            .checked_add(1)
                            .ok_or_else(|| budget("literal digit position overflow"))?;
                    }
                }
                if !any {
                    return Err(invalid("literal has no digits"));
                }
                Ok(result)
            }
        }
    }
    fn new_bit(&mut self, value: bool) -> TieResult<verilog::Expr> {
        if self.cells.len() >= self.work.limits.max_new_cells {
            return Err(budget("max_new_cells exceeded"));
        }
        let vlimits = verilog::VerilogLimits::default();
        if self.base_cells + self.cells.len() >= vlimits.max_instances
            || self.base_wires + self.wires.len() >= vlimits.max_nets
        {
            return Err(budget("rewritten AST object limit exceeded"));
        }
        let (instance, net) = loop {
            self.work.charge(1)?;
            let n = self.next_name;
            self.next_name = self
                .next_name
                .checked_add(1)
                .ok_or_else(|| budget("tie name counter exhausted"))?;
            let instance = format!("rr_tie_cell_{n}");
            let net = format!("rr_tie_net_{n}");
            if !self.names.contains(&instance) && !self.names.contains(&net) {
                break (instance, net);
            }
        };
        let master = if value { &self.high } else { &self.low };
        self.connections = self
            .connections
            .checked_add(master.outputs.len())
            .ok_or_else(|| budget("connection count overflow"))?;
        if self.connections > vlimits.max_connections {
            return Err(budget("rewritten AST connection limit exceeded"));
        }
        self.work.charge(
            instance.len() * 3 + net.len() * 4 + master.name.len() + master.selected.len() + 1,
        )?;
        self.names.insert(instance.clone());
        self.names.insert(net.clone());
        let mut connections = Vec::with_capacity(master.outputs.len());
        for pin in &master.outputs {
            self.work.charge(1 + pin.len())?;
            connections.push(verilog::Connection {
                pin: (*pin).into(),
                signal: if *pin == master.selected {
                    verilog::Expr::Ident(net.clone())
                } else {
                    verilog::Expr::Unconnected
                },
            });
        }
        self.cells.push(verilog::Instance {
            name: instance.clone(),
            cell: master.name.into(),
            connections,
        });
        self.wires.push(verilog::Wire {
            name: net.clone(),
            range: None,
        });
        self.report.bindings.push(TieBinding {
            instance,
            net: net.clone(),
            pin: master.selected.into(),
            value,
        });
        if value {
            self.report.high_cells += 1;
        } else {
            self.report.low_cells += 1;
        }
        Ok(verilog::Expr::Ident(net))
    }
    fn rewrite(
        &mut self,
        expr: &verilog::Expr,
        context: Option<usize>,
        depth: usize,
    ) -> TieResult<verilog::Expr> {
        self.work.charge(1)?;
        if depth > 32 {
            return Err(budget("expression depth exceeds 32"));
        }
        if let verilog::Expr::Unconnected = expr {
            return Ok(verilog::Expr::Unconnected);
        }
        if let verilog::Expr::Constant(text) = expr {
            let bits = self.decode(text, context)?;
            let mut parts = Vec::with_capacity(bits.len());
            for bit in bits {
                parts.push(self.new_bit(bit)?);
            }
            return if parts.len() == 1 {
                parts.pop().ok_or_else(|| invalid("missing decoded bit"))
            } else {
                Ok(verilog::Expr::Concat(parts))
            };
        }
        let width = self.width(expr, depth)?;
        if context.is_some_and(|n| n != width) {
            return Err(invalid(
                "expression/destination width mismatch; implicit coercion is unsupported",
            ));
        }
        if let verilog::Expr::Concat(parts) = expr {
            let mut rewritten = Vec::new();
            for part in parts {
                match self.rewrite(part, None, depth + 1)? {
                    verilog::Expr::Concat(parts) => rewritten.extend(parts),
                    part => rewritten.push(part),
                }
            }
            Ok(verilog::Expr::Concat(rewritten))
        } else {
            Ok(expr.clone())
        }
    }
}

/// Replace unsigned, context-sized constant occurrences with dedicated tie cells.
/// Actual linked fanout and high/low Boolean meaning are caller verification gates.
pub fn materialize_constants(
    netlist: &verilog::Netlist,
    high: &lef::LefMacro,
    high_pin: &str,
    low: &lef::LefMacro,
    low_pin: &str,
    limits: &TieLimits,
) -> TieResult<(verilog::Netlist, TieReport)> {
    verilog::validate_netlist(netlist, &verilog::VerilogLimits::default())
        .map_err(|e| invalid(e.to_string()))?;
    input_bound(netlist)?;
    if high.name == low.name && high_pin == low_pin {
        return Err(invalid(
            "one master output cannot be selected as both high and low",
        ));
    }
    let mut work = Work { used: 0, limits };
    let high = master(high, high_pin, &mut work)?;
    let low = master(low, low_pin, &mut work)?;
    let mut declared = HashMap::new();
    let mut names = HashSet::new();
    let mut reserve = |name: &str| -> TieResult<()> {
        work.charge(1 + name.len())?;
        names.insert(name.to_owned());
        Ok(())
    };
    reserve(&netlist.module)?;
    for port in &netlist.ports {
        reserve(&port.name)?;
        declared.insert(port.name.as_str(), port.range);
    }
    for wire in &netlist.wires {
        reserve(&wire.name)?;
        declared.insert(wire.name.as_str(), wire.range);
    }
    let mut connections = 0usize;
    for instance in &netlist.instances {
        reserve(&instance.name)?;
        reserve(&instance.cell)?;
        for connection in &instance.connections {
            reserve(&connection.pin)?;
            connections += 1;
        }
    }
    reserve(high.name)?;
    reserve(low.name)?;
    for pin in high.outputs.iter().chain(&low.outputs) {
        reserve(pin)?;
    }
    let mut transform = Transform {
        work,
        high,
        low,
        declared,
        names,
        next_name: 0,
        base_cells: netlist.instances.len(),
        base_wires: netlist.wires.len(),
        connections,
        cells: Vec::new(),
        wires: Vec::new(),
        report: TieReport {
            new_cells: 0,
            constant_bits: 0,
            high_cells: 0,
            low_cells: 0,
            bindings: Vec::new(),
        },
    };
    let mut rewritten = verilog::Netlist {
        module: netlist.module.clone(),
        ports: netlist.ports.clone(),
        wires: netlist.wires.clone(),
        assigns: Vec::with_capacity(netlist.assigns.len()),
        instances: Vec::with_capacity(netlist.instances.len()),
    };
    for assign in &netlist.assigns {
        let width = transform.width(&assign.lhs, 1)?;
        rewritten.assigns.push(verilog::Assign {
            lhs: assign.lhs.clone(),
            rhs: transform.rewrite(&assign.rhs, Some(width), 1)?,
        });
    }
    for instance in &netlist.instances {
        let mut connections = Vec::with_capacity(instance.connections.len());
        for connection in &instance.connections {
            connections.push(verilog::Connection {
                pin: connection.pin.clone(),
                signal: transform.rewrite(&connection.signal, Some(1), 1)?,
            });
        }
        rewritten.instances.push(verilog::Instance {
            name: instance.name.clone(),
            cell: instance.cell.clone(),
            connections,
        });
    }
    transform.report.new_cells = transform.cells.len();
    rewritten.wires.extend(transform.wires);
    rewritten.instances.extend(transform.cells);
    verilog::validate_netlist(&rewritten, &verilog::VerilogLimits::default())
        .map_err(|e| invalid(e.to_string()))?;
    Ok((rewritten, transform.report))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> verilog::Netlist {
        verilog::parse_verilog(s, &verilog::VerilogLimits::default()).unwrap()
    }
    fn cell(name: &str) -> lef::LefMacro {
        lef::LefMacro {
            name: name.into(),
            class: Some("CORE".into()),
            origin: [0, 0],
            size: [1, 1],
            site: Some("s".into()),
            symmetry: vec![],
            obstructions: vec![],
            foreign: vec![],
            pins: vec![
                lef::LefPin {
                    name: "Y".into(),
                    direction: Some("OUTPUT".into()),
                    use_: Some("SIGNAL".into()),
                    shape: None,
                    ports: vec![],
                },
                lef::LefPin {
                    name: "UNUSED".into(),
                    direction: Some("OUTPUT".into()),
                    use_: None,
                    shape: None,
                    ports: vec![],
                },
                lef::LefPin {
                    name: "VDD".into(),
                    direction: Some("INOUT".into()),
                    use_: Some("POWER".into()),
                    shape: None,
                    ports: vec![],
                },
            ],
        }
    }
    fn run(n: &verilog::Netlist) -> (verilog::Netlist, TieReport) {
        materialize_constants(n, &cell("HI"), "Y", &cell("LO"), "Y", &TieLimits::default()).unwrap()
    }
    fn bits(expr: &verilog::Expr, report: &TieReport) -> Vec<bool> {
        match expr {
            verilog::Expr::Ident(name) => vec![
                report
                    .bindings
                    .iter()
                    .find(|b| b.net == *name)
                    .unwrap()
                    .value,
            ],
            verilog::Expr::Concat(p) => p.iter().flat_map(|p| bits(p, report)).collect(),
            _ => panic!("unexpected expression"),
        }
    }
    #[test]
    fn literal_bits_and_tie_connections_are_preserved() {
        let n = parse(
            "module m(input a, output [3:0] y); assign y={2'b10,2'h1}; C u(.A(1'b0),.B(a),.Y()); endmodule",
        );
        let (out, report) = run(&n);
        assert_eq!(
            bits(&out.assigns[0].rhs, &report),
            [true, false, false, true]
        );
        assert_eq!(
            (
                report.new_cells,
                report.constant_bits,
                report.high_cells,
                report.low_cells
            ),
            (5, 5, 2, 3)
        );
        for instance in &out.instances[1..] {
            assert_eq!(instance.connections.len(), 2);
            assert!(
                instance
                    .connections
                    .iter()
                    .any(|c| c.pin == "UNUSED" && c.signal == verilog::Expr::Unconnected)
            );
            assert!(!instance.connections.iter().any(|c| c.pin == "VDD"));
        }
        assert_eq!(
            n,
            parse(
                "module m(input a, output [3:0] y); assign y={2'b10,2'h1}; C u(.A(1'b0),.B(a),.Y()); endmodule"
            )
        );
    }
    #[test]
    fn fill_decimal_octal_and_hex_have_explicit_bit_order() {
        for constant in ["8'd165", "8'o245", "8'ha5", "8'b10100101"] {
            let (out, report) = run(&parse(&format!(
                "module m(output [7:0] y); assign y={constant}; endmodule"
            )));
            assert_eq!(
                bits(&out.assigns[0].rhs, &report),
                [true, false, true, false, false, true, false, true]
            );
        }
        let (out, report) = run(&parse("module m(output [3:0] y); assign y='1; endmodule"));
        assert_eq!(bits(&out.assigns[0].rhs, &report), [true; 4]);
        let (out, report) = run(&parse("module m(output [3:0] y); assign y='0; endmodule"));
        assert_eq!(bits(&out.assigns[0].rhs, &report), [false; 4]);
    }
    #[test]
    fn generated_names_skip_all_collisions_and_are_repeatable() {
        let n = parse(
            "module m(output y); wire rr_tie_net_0; C rr_tie_cell_1(.Y()); assign y=1'b0; endmodule",
        );
        let a = run(&n);
        let b = run(&n);
        assert_eq!(a, b);
        assert_eq!(a.1.bindings[0].instance, "rr_tie_cell_2");
        assert_eq!(a.1.bindings[0].net, "rr_tie_net_2");
    }
    #[test]
    fn invalid_macros_unknown_bits_and_coercions_are_rejected() {
        let n = parse("module m(output y); assign y=1'b0; endmodule");
        let mut bad = cell("HI");
        bad.pins[0].direction = Some("INPUT".into());
        assert!(
            materialize_constants(&n, &bad, "Y", &cell("LO"), "Y", &TieLimits::default()).is_err()
        );
        assert!(
            materialize_constants(
                &n,
                &cell("HI"),
                "missing",
                &cell("LO"),
                "Y",
                &TieLimits::default()
            )
            .is_err()
        );
        assert!(
            materialize_constants(
                &n,
                &cell("HI"),
                "Y",
                &cell("HI"),
                "Y",
                &TieLimits::default()
            )
            .is_err()
        );
        for constant in ["1'bx", "1'bz", "2'b00", "0", "1'd2"] {
            let n = parse(&format!(
                "module m(output y); assign y={constant}; endmodule"
            ));
            assert!(
                materialize_constants(
                    &n,
                    &cell("HI"),
                    "Y",
                    &cell("LO"),
                    "Y",
                    &TieLimits::default()
                )
                .is_err(),
                "accepted {constant}"
            );
        }
    }
    #[test]
    fn budgets_fail_before_unbounded_generation() {
        let n = parse("module m(output [1:0] y); assign y=2'b10; endmodule");
        for limits in [
            TieLimits {
                max_new_cells: 0,
                ..TieLimits::default()
            },
            TieLimits {
                max_bits: 1,
                ..TieLimits::default()
            },
            TieLimits {
                max_work: 1,
                ..TieLimits::default()
            },
        ] {
            assert!(matches!(
                materialize_constants(&n, &cell("HI"), "Y", &cell("LO"), "Y", &limits),
                Err(TieError::Budget(_))
            ));
        }
    }
}
