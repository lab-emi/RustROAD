//! Typed, bounded netlist ECO on the flat structural Verilog AST.
//!
//! Two operations exist:
//! - `insert_buffer`: a non-inverting buffer cell is placed between the net
//!   that currently feeds ONE named sink pin and that pin. The driver net keeps
//!   every other sink; only the named connection is redirected to a new net.
//! - `insert_diode`: an antenna diode cell is attached to a named net.
//!
//! Every operation names its new instance and net explicitly. Names must be
//! shell-safe handles (`[A-Za-z][A-Za-z0-9_]*`) and unique against every
//! existing instance, wire and port, so existing instance names and register
//! Q-net names are never changed (state matching in CEC depends on them).
//! Buffer cells are accepted only when the caller-supplied cell data, derived
//! from Liberty `function` attributes, prove an identity function of the
//! declared input pin; an inverting or multi-input cell is rejected.
//!
//! Memory/work preflight: at most `max_operations` (default 4096) operations;
//! one lookup map of existing names (one `&str` per instance/wire/port, bounded
//! by `VerilogLimits`), one instance index and one per-operation name check.
//! For the 250k-instance AST cap this is below 32 MiB plus the output AST
//! clone; no traversal, path or cone is materialized.
use crate::verilog::{self, Connection, Expr, Instance, Netlist, VerilogLimits, Wire};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const ECO_PLAN_SCHEMA: &str = "rustroad.eco-plan.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcoLimits {
    pub max_operations: usize,
    pub max_name_bytes: usize,
}
impl Default for EcoLimits {
    fn default() -> Self {
        Self {
            max_operations: 4096,
            max_name_bytes: 128,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EcoError {
    #[error("ECO_INVALID: {0}")]
    Invalid(String),
    #[error("ECO_NOT_BUFFER: {0}")]
    NotBuffer(String),
    #[error("ECO_NAME: {0}")]
    Name(String),
    #[error("ECO_BUDGET_EXCEEDED: {0}")]
    Budget(String),
    #[error("ECO_VERILOG: {0}")]
    Verilog(#[from] verilog::VerilogError),
}
type Result<T> = std::result::Result<T, EcoError>;

/// One instance pin, `instance/pin` in timing reports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinRef {
    pub instance: String,
    pub pin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum EcoOp {
    /// `sink` is disconnected from its net and driven by `cell` through `net`;
    /// the old net drives the new cell's input.
    InsertBuffer {
        cell: String,
        instance: String,
        net: String,
        sink: PinRef,
    },
    /// Attach an antenna diode `cell` to `net` (a scalar wire/port or `name[i]`).
    InsertDiode {
        cell: String,
        instance: String,
        net: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcoPlan {
    pub schema: String,
    pub module: String,
    pub operations: Vec<EcoOp>,
}

/// Buffer data derived from Liberty; `function` is the output pin's function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BufferCell {
    pub name: String,
    pub input_pin: String,
    pub output_pin: String,
    pub function: String,
}
/// Antenna diode cell with its single (input) pin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiodeCell {
    pub name: String,
    pub pin: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcoCells {
    pub buffers: Vec<BufferCell>,
    pub diodes: Vec<DiodeCell>,
}

/// What each operation did, in plan order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedOp {
    pub op: String,
    pub instance: String,
    pub cell: String,
    pub net: String,
    /// The net that fed the sink before the edit (insert_buffer) or the
    /// diode's net expression (insert_diode), in Verilog syntax.
    pub driver_net: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sink: Option<PinRef>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcoReport {
    pub schema: String,
    pub module: String,
    pub instances_before: usize,
    pub instances_after: usize,
    pub buffers_inserted: usize,
    pub diodes_inserted: usize,
    pub applied: Vec<AppliedOp>,
    /// Existing instances and wires are retained by name; nothing is renamed.
    pub renamed_objects: usize,
}

/// A shell-safe handle that is also a simple (unescaped) Verilog identifier.
pub fn handle_name_ok(name: &str, limits: &EcoLimits) -> bool {
    let mut bytes = name.bytes();
    !name.is_empty()
        && name.len() <= limits.max_name_bytes
        && matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !matches!(
            name,
            "module" | "endmodule" | "input" | "output" | "inout" | "wire" | "assign" | "reg"
        )
}

/// Strip whitespace and fully enclosing parentheses from a Liberty function.
fn normalized_function(function: &str) -> String {
    let mut s: String = function
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect();
    loop {
        let enclosed = s.len() >= 2 && s.starts_with('(') && s.ends_with(')') && {
            // The first '(' must close at the last character.
            let mut depth = 0i32;
            let mut closes_last = true;
            for (i, c) in s.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 && i + 1 != s.len() {
                            closes_last = false;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            closes_last
        };
        if !enclosed {
            return s;
        }
        s = s[1..s.len() - 1].to_string();
    }
}

impl BufferCell {
    /// The Liberty output function must be exactly the input pin: a
    /// non-inverting, single-input buffer. Anything else is rejected.
    pub fn check_identity(&self) -> Result<()> {
        if self.input_pin == self.output_pin
            || self.input_pin.is_empty()
            || self.output_pin.is_empty()
        {
            return Err(EcoError::NotBuffer(format!(
                "{}: input and output pins must differ",
                self.name
            )));
        }
        if normalized_function(&self.function) != self.input_pin {
            return Err(EcoError::NotBuffer(format!(
                "{}: Liberty function {:?} of {} is not the identity of input {}",
                self.name, self.function, self.output_pin, self.input_pin
            )));
        }
        Ok(())
    }
}

fn expr_text(expr: &Expr) -> String {
    match expr {
        Expr::Ident(n) => n.clone(),
        Expr::BitSelect { name, index } => format!("{name}[{index}]"),
        Expr::PartSelect { name, range } => format!("{name}[{}:{}]", range.msb, range.lsb),
        Expr::Concat(_) => "{concat}".into(),
        Expr::Constant(v) => v.clone(),
        Expr::Unconnected => "()".into(),
    }
}

/// Resolve a report net name to a scalar expression. An exact scalar wire or
/// port wins; otherwise `name[i]` must select one bit of a declared vector.
/// A name that matches both forms is ambiguous and rejected.
pub fn resolve_net(netlist: &Netlist, name: &str) -> Result<Expr> {
    let mut scalar = false;
    let mut vectors: HashMap<&str, verilog::Range> = HashMap::new();
    for (n, range) in netlist
        .ports
        .iter()
        .map(|p| (p.name.as_str(), p.range))
        .chain(netlist.wires.iter().map(|w| (w.name.as_str(), w.range)))
    {
        match range {
            None if n == name => scalar = true,
            Some(r) => {
                vectors.insert(n, r);
            }
            None => {}
        }
    }
    let bit = name.strip_suffix(']').and_then(|s| {
        let open = s.rfind('[')?;
        let index: i64 = s[open + 1..].parse().ok()?;
        let base = &s[..open];
        let r = vectors.get(base)?;
        let (lo, hi) = (r.msb.min(r.lsb), r.msb.max(r.lsb));
        (lo..=hi).contains(&index).then(|| Expr::BitSelect {
            name: base.to_string(),
            index,
        })
    });
    match (scalar, bit) {
        (true, None) => Ok(Expr::Ident(name.to_string())),
        (false, Some(b)) => Ok(b),
        (true, Some(_)) => Err(EcoError::Invalid(format!(
            "net {name} is ambiguous (scalar wire and vector bit)"
        ))),
        (false, None) => Err(EcoError::Invalid(format!(
            "net {name} is not a declared scalar wire/port or vector bit"
        ))),
    }
}

/// Apply `plan` to `netlist` and return the edited AST. The input is not
/// modified; on any error nothing is returned.
pub fn apply(
    netlist: &Netlist,
    plan: &EcoPlan,
    cells: &EcoCells,
    limits: &EcoLimits,
    verilog_limits: &VerilogLimits,
) -> Result<(Netlist, EcoReport)> {
    if plan.schema != ECO_PLAN_SCHEMA {
        return Err(EcoError::Invalid(format!(
            "plan schema {:?}, expected {ECO_PLAN_SCHEMA}",
            plan.schema
        )));
    }
    if plan.module != netlist.module {
        return Err(EcoError::Invalid(format!(
            "plan is for module {} but the netlist defines {}",
            plan.module, netlist.module
        )));
    }
    if plan.operations.len() > limits.max_operations {
        return Err(EcoError::Budget(format!(
            "{} operations exceed max_operations {}",
            plan.operations.len(),
            limits.max_operations
        )));
    }
    if netlist
        .instances
        .len()
        .saturating_add(plan.operations.len())
        > verilog_limits.max_instances
        || netlist.wires.len().saturating_add(plan.operations.len()) > verilog_limits.max_nets
    {
        return Err(EcoError::Budget(
            "ECO would exceed the netlist instance/net cap".into(),
        ));
    }
    verilog::validate_netlist(netlist, verilog_limits)?;
    let buffers: HashMap<&str, &BufferCell> =
        cells.buffers.iter().map(|b| (b.name.as_str(), b)).collect();
    let diodes: HashMap<&str, &DiodeCell> =
        cells.diodes.iter().map(|d| (d.name.as_str(), d)).collect();
    if buffers.len() != cells.buffers.len() || diodes.len() != cells.diodes.len() {
        return Err(EcoError::Invalid("duplicate ECO cell entry".into()));
    }
    let mut out = netlist.clone();
    let mut taken: HashSet<String> = out
        .instances
        .iter()
        .map(|i| i.name.clone())
        .chain(out.wires.iter().map(|w| w.name.clone()))
        .chain(out.ports.iter().map(|p| p.name.clone()))
        .collect();
    let mut index: HashMap<String, usize> = out
        .instances
        .iter()
        .enumerate()
        .map(|(i, x)| (x.name.clone(), i))
        .collect();
    let mut claim = |name: &str, what: &str| -> Result<()> {
        if !handle_name_ok(name, limits) {
            return Err(EcoError::Name(format!(
                "new {what} name {name:?} is not a shell-safe handle [A-Za-z][A-Za-z0-9_]*"
            )));
        }
        if !taken.insert(name.to_string()) {
            return Err(EcoError::Name(format!(
                "new {what} name {name} collides with an existing object"
            )));
        }
        Ok(())
    };
    let mut applied = Vec::with_capacity(plan.operations.len());
    let (mut nbuf, mut ndiode) = (0usize, 0usize);
    for op in &plan.operations {
        match op {
            EcoOp::InsertBuffer {
                cell,
                instance,
                net,
                sink,
            } => {
                let b = buffers.get(cell.as_str()).ok_or_else(|| {
                    EcoError::NotBuffer(format!("{cell} is not a profiled buffer cell"))
                })?;
                b.check_identity()?;
                let at = *index.get(&sink.instance).ok_or_else(|| {
                    EcoError::Invalid(format!("sink instance {} absent", sink.instance))
                })?;
                let connection = out.instances[at]
                    .connections
                    .iter_mut()
                    .find(|c| c.pin == sink.pin)
                    .ok_or_else(|| {
                        EcoError::Invalid(format!(
                            "sink pin {}/{} is not connected",
                            sink.instance, sink.pin
                        ))
                    })?;
                let driver = match &connection.signal {
                    e @ (Expr::Ident(_) | Expr::BitSelect { .. }) => e.clone(),
                    other => {
                        return Err(EcoError::Invalid(format!(
                            "sink {}/{} is driven by {}; only a single net bit is supported",
                            sink.instance,
                            sink.pin,
                            expr_text(other)
                        )));
                    }
                };
                claim(instance, "instance")?;
                claim(net, "net")?;
                connection.signal = Expr::Ident(net.clone());
                out.wires.push(Wire {
                    name: net.clone(),
                    range: None,
                });
                index.insert(instance.clone(), out.instances.len());
                out.instances.push(Instance {
                    name: instance.clone(),
                    cell: b.name.clone(),
                    connections: vec![
                        Connection {
                            pin: b.input_pin.clone(),
                            signal: driver.clone(),
                        },
                        Connection {
                            pin: b.output_pin.clone(),
                            signal: Expr::Ident(net.clone()),
                        },
                    ],
                });
                nbuf += 1;
                applied.push(AppliedOp {
                    op: "insert_buffer".into(),
                    instance: instance.clone(),
                    cell: b.name.clone(),
                    net: net.clone(),
                    driver_net: expr_text(&driver),
                    sink: Some(sink.clone()),
                });
            }
            EcoOp::InsertDiode {
                cell,
                instance,
                net,
            } => {
                let d = diodes.get(cell.as_str()).ok_or_else(|| {
                    EcoError::Invalid(format!("{cell} is not a profiled antenna diode cell"))
                })?;
                if d.pin.is_empty() {
                    return Err(EcoError::Invalid(format!("{cell}: empty diode pin")));
                }
                let signal = resolve_net(&out, net)?;
                claim(instance, "instance")?;
                index.insert(instance.clone(), out.instances.len());
                out.instances.push(Instance {
                    name: instance.clone(),
                    cell: d.name.clone(),
                    connections: vec![Connection {
                        pin: d.pin.clone(),
                        signal: signal.clone(),
                    }],
                });
                ndiode += 1;
                applied.push(AppliedOp {
                    op: "insert_diode".into(),
                    instance: instance.clone(),
                    cell: d.name.clone(),
                    net: net.clone(),
                    driver_net: expr_text(&signal),
                    sink: None,
                });
            }
        }
    }
    verilog::validate_netlist(&out, verilog_limits)?;
    // Every original instance keeps its name and cell; every original wire stays.
    debug_assert!(
        netlist
            .instances
            .iter()
            .zip(&out.instances)
            .all(|(a, b)| a.name == b.name && a.cell == b.cell)
    );
    Ok((
        out.clone(),
        EcoReport {
            schema: "rustroad.eco-report.v1".into(),
            module: out.module.clone(),
            instances_before: netlist.instances.len(),
            instances_after: out.instances.len(),
            buffers_inserted: nbuf,
            diodes_inserted: ndiode,
            applied,
            renamed_objects: 0,
        },
    ))
}

/// Next free `<prefix><k>` sequence number: one more than the largest
/// `<prefix><digits>` (or `<prefix><digits>_...`) name present.
pub fn next_sequence(netlist: &Netlist, prefix: &str) -> usize {
    netlist
        .instances
        .iter()
        .map(|i| i.name.as_str())
        .chain(netlist.wires.iter().map(|w| w.name.as_str()))
        .filter_map(|n| n.strip_prefix(prefix))
        .filter_map(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<usize>().ok()
        })
        .max()
        .map_or(0, |m| m + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verilog::{parse_verilog, write_verilog};

    const NETLIST: &str =
        "module top(input wire clk, input wire en, input wire [1:0] a, output wire [1:0] y);
  wire \\q[0] ;
  wire n0;
  wire n1;
  cellx g0 (.A(a[0]), .B(en), .X(n0));
  cellx g1 (.A(a[1]), .B(en), .X(n1));
  dff r0 (.D(n0), .CLK(clk), .Q(\\q[0] ));
  dff r1 (.D(n1), .CLK(clk), .Q(y[1]));
  dff r2 (.D(n0), .CLK(clk), .Q(y[0]));
endmodule
";
    fn netlist() -> Netlist {
        parse_verilog(NETLIST, &VerilogLimits::default()).unwrap()
    }
    fn cells() -> EcoCells {
        EcoCells {
            buffers: vec![
                BufferCell {
                    name: "dly_1".into(),
                    input_pin: "A".into(),
                    output_pin: "X".into(),
                    function: "(A)".into(),
                },
                BufferCell {
                    name: "inv_1".into(),
                    input_pin: "A".into(),
                    output_pin: "Y".into(),
                    function: "!A".into(),
                },
                BufferCell {
                    name: "and_1".into(),
                    input_pin: "A".into(),
                    output_pin: "X".into(),
                    function: "(A&B)".into(),
                },
            ],
            diodes: vec![DiodeCell {
                name: "diode_2".into(),
                pin: "DIODE".into(),
            }],
        }
    }
    fn plan(operations: Vec<EcoOp>) -> EcoPlan {
        EcoPlan {
            schema: ECO_PLAN_SCHEMA.into(),
            module: "top".into(),
            operations,
        }
    }
    fn buffer(cell: &str, instance: &str, net: &str, sink: (&str, &str)) -> EcoOp {
        EcoOp::InsertBuffer {
            cell: cell.into(),
            instance: instance.into(),
            net: net.into(),
            sink: PinRef {
                instance: sink.0.into(),
                pin: sink.1.into(),
            },
        }
    }
    fn run(ops: Vec<EcoOp>) -> Result<(Netlist, EcoReport)> {
        apply(
            &netlist(),
            &plan(ops),
            &cells(),
            &EcoLimits::default(),
            &VerilogLimits::default(),
        )
    }

    #[test]
    fn buffer_moves_exactly_one_sink_and_keeps_names() {
        let (out, report) = run(vec![
            buffer("dly_1", "rr_eco_hold_0", "rr_eco_hold_0_n", ("r0", "D")),
            buffer("dly_1", "rr_eco_hold_1", "rr_eco_hold_1_n", ("r0", "D")),
        ])
        .unwrap();
        // r2/D still sees n0; r0/D sees the end of a two-cell chain.
        let pin = |inst: &str, pin: &str| {
            out.instances
                .iter()
                .find(|i| i.name == inst)
                .unwrap()
                .connections
                .iter()
                .find(|c| c.pin == pin)
                .unwrap()
                .signal
                .clone()
        };
        assert_eq!(pin("r2", "D"), Expr::Ident("n0".into()));
        assert_eq!(pin("r0", "D"), Expr::Ident("rr_eco_hold_1_n".into()));
        assert_eq!(pin("rr_eco_hold_0", "A"), Expr::Ident("n0".into()));
        assert_eq!(
            pin("rr_eco_hold_1", "A"),
            Expr::Ident("rr_eco_hold_0_n".into())
        );
        assert_eq!(pin("r0", "Q"), Expr::Ident("q[0]".into()));
        assert_eq!(report.buffers_inserted, 2);
        assert_eq!(report.applied[1].driver_net, "rr_eco_hold_0_n");
        let original = netlist();
        for (a, b) in original.instances.iter().zip(&out.instances) {
            assert_eq!((&a.name, &a.cell), (&b.name, &b.cell));
        }
        assert!(original.wires.iter().all(|w| out.wires.contains(w)));
        // Round-trips through the canonical writer.
        let mut text = Vec::new();
        write_verilog(&out, &mut text).unwrap();
        let again = parse_verilog(
            std::str::from_utf8(&text).unwrap(),
            &VerilogLimits::default(),
        )
        .unwrap();
        assert_eq!(again, out);
    }

    #[test]
    fn bit_select_sink_and_port_bit_diode() {
        let (out, report) = run(vec![
            buffer("dly_1", "e0", "e0_n", ("g1", "A")),
            EcoOp::InsertDiode {
                cell: "diode_2".into(),
                instance: "ant0".into(),
                net: "a[1]".into(),
            },
            EcoOp::InsertDiode {
                cell: "diode_2".into(),
                instance: "ant1".into(),
                net: "q[0]".into(),
            },
        ])
        .unwrap();
        assert_eq!(report.diodes_inserted, 2);
        let e0 = out.instances.iter().find(|i| i.name == "e0").unwrap();
        assert_eq!(
            e0.connections[0].signal,
            Expr::BitSelect {
                name: "a".into(),
                index: 1
            }
        );
        let ant0 = out.instances.iter().find(|i| i.name == "ant0").unwrap();
        assert_eq!(ant0.connections.len(), 1);
        assert_eq!(report.applied[1].driver_net, "a[1]");
        assert_eq!(report.applied[2].driver_net, "q[0]");
    }

    #[test]
    fn inverting_or_multi_input_cells_are_rejected() {
        for cell in ["inv_1", "and_1", "unknown_cell"] {
            let e = run(vec![buffer(cell, "e0", "e0_n", ("r0", "D"))]).unwrap_err();
            assert!(matches!(e, EcoError::NotBuffer(_)), "{cell}: {e}");
        }
        // A diode cell is not a buffer and vice versa.
        assert!(run(vec![buffer("diode_2", "e0", "e0_n", ("r0", "D"))]).is_err());
        assert!(
            run(vec![EcoOp::InsertDiode {
                cell: "dly_1".into(),
                instance: "e0".into(),
                net: "n0".into()
            }])
            .is_err()
        );
    }

    #[test]
    fn names_must_be_new_and_shell_safe() {
        for (instance, net) in [
            ("r0", "fresh"),
            ("fresh", "n0"),
            ("fresh", "q[0]"),
            ("fresh", "clk"),
            ("bad#1", "fresh"),
            ("fresh", "has space"),
            ("1abc", "fresh"),
            ("wire", "fresh"),
            ("same", "same"),
        ] {
            let e = run(vec![buffer("dly_1", instance, net, ("r0", "D"))]).unwrap_err();
            assert!(matches!(e, EcoError::Name(_)), "{instance}/{net}: {e}");
        }
        let e = run(vec![
            buffer("dly_1", "x0", "x0_n", ("r0", "D")),
            buffer("dly_1", "x0", "x1_n", ("r1", "D")),
        ])
        .unwrap_err();
        assert!(matches!(e, EcoError::Name(_)));
    }

    #[test]
    fn bad_sinks_nets_and_budgets_fail_explicitly() {
        assert!(run(vec![buffer("dly_1", "e0", "e0_n", ("nope", "D"))]).is_err());
        assert!(run(vec![buffer("dly_1", "e0", "e0_n", ("r0", "QN"))]).is_err());
        for net in ["a", "a[2]", "zz", "a[x]"] {
            assert!(
                run(vec![EcoOp::InsertDiode {
                    cell: "diode_2".into(),
                    instance: "d0".into(),
                    net: net.into()
                }])
                .is_err(),
                "{net}"
            );
        }
        let mut p = plan(vec![buffer("dly_1", "e0", "e0_n", ("r0", "D"))]);
        p.module = "other".into();
        assert!(
            apply(
                &netlist(),
                &p,
                &cells(),
                &EcoLimits::default(),
                &VerilogLimits::default()
            )
            .is_err()
        );
        p.module = "top".into();
        p.schema = "x".into();
        assert!(
            apply(
                &netlist(),
                &p,
                &cells(),
                &EcoLimits::default(),
                &VerilogLimits::default()
            )
            .is_err()
        );
        let limits = EcoLimits {
            max_operations: 1,
            ..EcoLimits::default()
        };
        let p = plan(vec![
            buffer("dly_1", "e0", "e0_n", ("r0", "D")),
            buffer("dly_1", "e1", "e1_n", ("r1", "D")),
        ]);
        assert!(matches!(
            apply(&netlist(), &p, &cells(), &limits, &VerilogLimits::default()),
            Err(EcoError::Budget(_))
        ));
        // A sink tied to a constant or left open cannot be buffered.
        let n = parse_verilog(
            "module top(input wire a); dff r0 (.D(1'b0), .CLK(a), .Q()); endmodule",
            &VerilogLimits::default(),
        )
        .unwrap();
        for pin in ["D", "Q"] {
            assert!(
                apply(
                    &n,
                    &plan(vec![buffer("dly_1", "e0", "e0_n", ("r0", pin))]),
                    &cells(),
                    &EcoLimits::default(),
                    &VerilogLimits::default()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn function_normalization_and_sequence() {
        assert_eq!(normalized_function(" ((A)) "), "A");
        assert_eq!(normalized_function("(A)&(B)"), "(A)&(B)");
        assert_eq!(normalized_function("\"I\""), "I");
        let (out, _) = run(vec![buffer(
            "dly_1",
            "rr_eco_hold_7",
            "rr_eco_hold_7_n",
            ("r0", "D"),
        )])
        .unwrap();
        assert_eq!(next_sequence(&out, "rr_eco_hold_"), 8);
        assert_eq!(next_sequence(&netlist(), "rr_eco_hold_"), 0);
    }
}
