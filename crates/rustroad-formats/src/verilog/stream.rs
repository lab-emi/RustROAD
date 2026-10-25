//! Semantic, bounded structural output without retaining a second netlist AST.
use super::{
    Direction, Expr, Range, Result, VerilogLimits, budget, emit_expr, emit_name, emit_range,
    identifier_ok, syntax, validate_expr,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;

#[derive(Debug)]
struct Capped<'a, W> {
    writer: &'a mut W,
    remaining: usize,
    exceeded: bool,
}

impl<W: Write> Write for Capped<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::other("Verilog output byte budget exceeded"));
        }
        let n = self.writer.write(bytes)?;
        self.remaining -= n;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    Declarations,
    Statements,
    Done,
    Failed,
}

/// Checked record writer with bounded declarations, names, work and output bytes.
///
/// Borrowed names avoid copying the original AST. Owned generated declarations
/// remain charged to the state budget. An error may leave a partial output stream.
#[derive(Debug)]
pub struct StreamWriter<'names, 'out, W> {
    output: Capped<'out, W>,
    limits: &'names VerilogLimits,
    nets: HashMap<Cow<'names, str>, Option<Range>>,
    wires: HashSet<Cow<'names, str>>,
    instances: HashSet<Cow<'names, str>>,
    phase: Phase,
    state_bytes: u64,
    max_state_bytes: u64,
    work: usize,
    max_work: usize,
    connections: usize,
    expr_nodes: usize,
}

impl<'names, 'out, W: Write> StreamWriter<'names, 'out, W> {
    /// Create an empty writer; call `module`, declarations, statements, then `finish`.
    pub fn new(
        output: &'out mut W,
        limits: &'names VerilogLimits,
        max_state_bytes: u64,
        max_work: usize,
    ) -> Self {
        Self {
            output: Capped {
                writer: output,
                remaining: limits.max_input_bytes,
                exceeded: false,
            },
            limits,
            nets: HashMap::new(),
            wires: HashSet::new(),
            instances: HashSet::new(),
            phase: Phase::Start,
            state_bytes: 0,
            max_state_bytes,
            work: 0,
            max_work,
            connections: 0,
            expr_nodes: 0,
        }
    }

    fn charge(&mut self, bytes: u64, work: usize) -> Result<()> {
        self.state_bytes = self
            .state_bytes
            .checked_add(bytes)
            .filter(|&n| n <= self.max_state_bytes)
            .ok_or_else(|| budget("Verilog writer state memory"))?;
        self.work = self
            .work
            .checked_add(work)
            .filter(|&n| n <= self.max_work)
            .ok_or_else(|| budget("Verilog writer work"))?;
        Ok(())
    }

    fn status(&mut self, result: Result<()>) -> Result<()> {
        let result = if self.output.exceeded {
            Err(budget("Verilog output byte limit"))
        } else {
            result
        };
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    /// Charge bounded transformation work performed by the record producer.
    ///
    /// # Errors
    /// Returns a budget error before the requested work exceeds the allowance.
    pub fn charge_work(&mut self, units: usize) -> Result<()> {
        let result = self.charge(0, units);
        self.status(result)
    }

    /// Remaining work available to a bounded record producer.
    pub fn remaining_work(&self) -> usize {
        self.max_work.saturating_sub(self.work)
    }

    /// Start one module and validate its ordered port declarations.
    ///
    /// # Errors
    /// Rejects repeated modules, duplicate/invalid ports and exhausted limits.
    pub fn module(
        &mut self,
        name: &str,
        ports: impl IntoIterator<Item = (&'names str, Direction, Option<Range>)>,
    ) -> Result<()> {
        let result = (|| {
            if self.phase != Phase::Start {
                return Err(syntax(0, "module already started"));
            }
            identifier_ok(name, self.limits)?;
            self.charge(65536, name.len() + 1)?;
            write!(self.output, "module ")?;
            emit_name(name, &mut self.output)?;
            writeln!(self.output, " (")?;
            let mut ports = ports.into_iter().peekable();
            let mut count = 0usize;
            while let Some((name, direction, range)) = ports.next() {
                if count >= self.limits.max_ports {
                    return Err(budget("Verilog port count"));
                }
                count += 1;
                identifier_ok(name, self.limits)?;
                self.charge(256, name.len() + 1)?;
                self.nets
                    .try_reserve(1)
                    .map_err(|_| budget("Verilog declaration allocation"))?;
                if self.nets.insert(Cow::Borrowed(name), range).is_some() {
                    return Err(syntax(0, "duplicate port"));
                }
                let direction = match direction {
                    Direction::Input => "input",
                    Direction::Output => "output",
                    Direction::Inout => "inout",
                };
                write!(self.output, "  {direction} wire ")?;
                emit_range(range, &mut self.output)?;
                emit_name(name, &mut self.output)?;
                writeln!(
                    self.output,
                    "{}",
                    if ports.peek().is_some() { "," } else { "" }
                )?;
            }
            writeln!(self.output, ");")?;
            self.phase = Phase::Declarations;
            Ok(())
        })();
        self.status(result)
    }

    /// Declare a wire before any assignment or instance.
    ///
    /// # Errors
    /// Rejects duplicates, inconsistent port redeclarations and exhausted limits.
    pub fn wire(&mut self, name: Cow<'names, str>, range: Option<Range>) -> Result<()> {
        let result = (|| {
            if self.phase != Phase::Declarations {
                return Err(syntax(0, "wire outside declaration phase"));
            }
            if self.wires.len() >= self.limits.max_nets {
                return Err(budget("Verilog wire count"));
            }
            identifier_ok(&name, self.limits)?;
            let payload = if matches!(name, Cow::Owned(_)) {
                (name.len() as u64).saturating_mul(4) + 64
            } else {
                0
            };
            self.charge(256 + payload, name.len() + 1)?;
            self.wires
                .try_reserve(1)
                .map_err(|_| budget("Verilog wire allocation"))?;
            self.nets
                .try_reserve(1)
                .map_err(|_| budget("Verilog declaration allocation"))?;
            if self.wires.contains(name.as_ref()) {
                return Err(syntax(0, "duplicate wire"));
            }
            if let Some(prior) = self.nets.get(name.as_ref())
                && *prior != range
            {
                return Err(syntax(0, "wire redeclaration disagrees with port range"));
            }
            write!(self.output, "  wire ")?;
            emit_range(range, &mut self.output)?;
            emit_name(&name, &mut self.output)?;
            writeln!(self.output, ";")?;
            self.nets.insert(name.clone(), range);
            self.wires.insert(name);
            Ok(())
        })();
        self.status(result)
    }

    fn expression(&mut self, expr: &Expr, lhs: bool, open: bool) -> Result<()> {
        let before = self.expr_nodes;
        let mut syntax = self.limits.clone();
        syntax.max_expr_nodes = syntax.max_expr_nodes.min(
            self.expr_nodes
                .saturating_add(self.max_work.saturating_sub(self.work)),
        );
        validate_expr(
            expr,
            &self.nets,
            &syntax,
            &mut self.expr_nodes,
            1,
            lhs,
            open,
        )?;
        self.charge(0, self.expr_nodes - before)?;
        Ok(())
    }

    fn statements(&mut self) -> Result<()> {
        if !matches!(self.phase, Phase::Declarations | Phase::Statements) {
            return Err(syntax(0, "statement outside module"));
        }
        self.phase = Phase::Statements;
        Ok(())
    }

    /// Emit a checked continuous assignment.
    ///
    /// # Errors
    /// Rejects undeclared references, invalid expressions and exhausted limits.
    pub fn assign(&mut self, lhs: &Expr, rhs: &Expr) -> Result<()> {
        let result = (|| {
            self.statements()?;
            self.expression(lhs, true, false)?;
            self.expression(rhs, false, false)?;
            write!(self.output, "  assign ")?;
            emit_expr(lhs, &mut self.output)?;
            write!(self.output, " = ")?;
            emit_expr(rhs, &mut self.output)?;
            writeln!(self.output, ";")?;
            Ok(())
        })();
        self.status(result)
    }

    /// Emit one instance, retaining only its name after its connections are written.
    ///
    /// # Errors
    /// Rejects duplicate names/pins, undeclared references and exhausted limits.
    pub fn instance<'pin>(
        &mut self,
        cell: &str,
        name: Cow<'names, str>,
        connections: impl IntoIterator<Item = (&'pin str, Cow<'pin, Expr>)>,
    ) -> Result<()> {
        let result = (|| {
            self.statements()?;
            if self.instances.len() >= self.limits.max_instances {
                return Err(budget("Verilog instance count"));
            }
            identifier_ok(cell, self.limits)?;
            identifier_ok(&name, self.limits)?;
            if self.instances.contains(name.as_ref()) {
                return Err(syntax(0, "duplicate instance"));
            }
            let payload = if matches!(name, Cow::Owned(_)) {
                name.len() as u64 * 2 + 32
            } else {
                0
            };
            self.charge(96 + payload, cell.len() + name.len() + 1)?;
            self.instances
                .try_reserve(1)
                .map_err(|_| budget("Verilog instance-name allocation"))?;
            write!(self.output, "  ")?;
            emit_name(cell, &mut self.output)?;
            write!(self.output, " ")?;
            emit_name(&name, &mut self.output)?;
            write!(self.output, " (")?;
            let mut pins = HashSet::new();
            let mut pin_bytes = 0;
            for (i, (pin, expr)) in connections.into_iter().enumerate() {
                if self.connections >= self.limits.max_connections {
                    return Err(budget("Verilog connection count"));
                }
                self.connections += 1;
                identifier_ok(pin, self.limits)?;
                self.charge(64, pin.len() + 1)?;
                pin_bytes += 64;
                pins.try_reserve(1)
                    .map_err(|_| budget("Verilog pin-name allocation"))?;
                if !pins.insert(pin) {
                    return Err(syntax(0, "duplicate named pin connection"));
                }
                self.expression(&expr, false, true)?;
                if i != 0 {
                    write!(self.output, ", ")?;
                }
                write!(self.output, ".")?;
                emit_name(pin, &mut self.output)?;
                write!(self.output, "(")?;
                emit_expr(&expr, &mut self.output)?;
                write!(self.output, ")")?;
            }
            writeln!(self.output, ");")?;
            self.state_bytes -= pin_bytes;
            self.instances.insert(name);
            Ok(())
        })();
        self.status(result)
    }

    /// Finish the module; no further records are accepted.
    ///
    /// # Errors
    /// Rejects an absent/already finished module and output failures.
    pub fn finish(&mut self) -> Result<()> {
        let result = (|| {
            self.statements()?;
            writeln!(self.output, "endmodule")?;
            self.phase = Phase::Done;
            Ok(())
        })();
        self.status(result)
    }
}
