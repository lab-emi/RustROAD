//! Deterministic IEEE 1481 SPEF writer for detailed (D_NET) parasitics.
//!
//! The writer only serializes an already-built RC network. It uses a name map
//! for nets and instances, `*<net>:<N>` internal nodes, `*<inst>:<PIN>` pins
//! and bare port names. Units are fixed (NS, PF, OHM). Every value is written
//! with a fixed number of significant digits and the declared total of each
//! net is the sum of the rounded values it lists, so consumers that check
//! totals against records see an exact match.
use std::io::Write;

#[derive(Debug, thiserror::Error)]
pub enum SpefError {
    #[error("IO_ERROR: {0}")]
    Io(#[from] std::io::Error),
    #[error("INVALID_SPEF: {0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, SpefError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Input,
    Output,
    Bidirectional,
}
impl Direction {
    fn code(self) -> &'static str {
        match self {
            Self::Input => "I",
            Self::Output => "O",
            Self::Bidirectional => "B",
        }
    }
}
/// A node of one D_NET.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Port(String),
    Pin { instance: String, pin: String },
    Internal(u32),
}
#[derive(Clone, Debug)]
pub struct Connection {
    /// Index into the net's nodes; must be a port or pin node.
    pub node: u32,
    pub direction: Direction,
    /// Cell master for instance pins (`*D`).
    pub cell: Option<String>,
}
#[derive(Clone, Debug)]
pub struct Net {
    pub name: String,
    pub nodes: Vec<Node>,
    pub connections: Vec<Connection>,
    /// Grounded capacitance per node, pF.
    pub ground: Vec<f64>,
    /// `(node, other net index, other node, pF)`.
    pub couplings: Vec<(u32, usize, u32, f64)>,
    /// `(node, node, ohm)`.
    pub resistors: Vec<(u32, u32, f64)>,
}
#[derive(Clone, Debug)]
pub struct Header {
    pub design: String,
    pub vendor: String,
    pub program: String,
    pub version: String,
    /// Fixed string; a wall-clock date would break byte reproducibility.
    pub date: String,
}

/// Escape a name for SPEF: keep `[A-Za-z0-9_]` and bus brackets, escape others.
pub fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']')) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Six significant digits, deterministic.
fn value(v: f64) -> Result<(String, f64)> {
    if !v.is_finite() || v < 0.0 {
        return Err(SpefError::Invalid(format!(
            "non-finite or negative value {v}"
        )));
    }
    let text = if v == 0.0 {
        "0".to_string()
    } else {
        format!("{v:.5e}")
    };
    let parsed: f64 = text
        .parse()
        .map_err(|_| SpefError::Invalid("value formatting".into()))?;
    Ok((text, parsed))
}

fn quoted(s: &str) -> Result<String> {
    if s.contains('"') || s.chars().any(|c| c.is_control()) {
        return Err(SpefError::Invalid(
            "header text must not contain quotes".into(),
        ));
    }
    Ok(format!("\"{s}\""))
}

/// Write the SPEF document. Ports are `(name, direction)` in output order.
pub fn write(
    header: &Header,
    ports: &[(String, Direction)],
    nets: &[Net],
    out: &mut impl Write,
) -> Result<()> {
    // Name map: nets first, then instances in first-use order.
    let mut instances: Vec<&str> = Vec::new();
    let mut instance_index = std::collections::HashMap::<&str, usize>::new();
    for net in nets {
        if net.ground.len() != net.nodes.len() {
            return Err(SpefError::Invalid("ground vector length".into()));
        }
        for node in &net.nodes {
            if let Node::Pin { instance, .. } = node
                && !instance_index.contains_key(instance.as_str())
            {
                instance_index.insert(instance.as_str(), nets.len() + 1 + instances.len());
                instances.push(instance.as_str());
            }
        }
    }
    writeln!(out, "*SPEF \"IEEE 1481-1999\"")?;
    writeln!(out, "*DESIGN {}", quoted(&header.design)?)?;
    writeln!(out, "*DATE {}", quoted(&header.date)?)?;
    writeln!(out, "*VENDOR {}", quoted(&header.vendor)?)?;
    writeln!(out, "*PROGRAM {}", quoted(&header.program)?)?;
    writeln!(out, "*VERSION {}", quoted(&header.version)?)?;
    writeln!(out, "*DESIGN_FLOW \"NAME_SCOPE LOCAL\" \"PIN_CAP NONE\"")?;
    writeln!(out, "*DIVIDER /")?;
    writeln!(out, "*DELIMITER :")?;
    writeln!(out, "*BUS_DELIMITER []")?;
    writeln!(out, "*T_UNIT 1 NS")?;
    writeln!(out, "*C_UNIT 1 PF")?;
    writeln!(out, "*R_UNIT 1 OHM")?;
    writeln!(out, "*L_UNIT 1 HENRY")?;
    writeln!(out)?;
    writeln!(out, "*NAME_MAP")?;
    for (i, net) in nets.iter().enumerate() {
        writeln!(out, "*{} {}", i + 1, escape(&net.name))?;
    }
    for (i, name) in instances.iter().enumerate() {
        writeln!(out, "*{} {}", nets.len() + 1 + i, escape(name))?;
    }
    writeln!(out)?;
    writeln!(out, "*PORTS")?;
    for (name, direction) in ports {
        writeln!(out, "{} {}", escape(name), direction.code())?;
    }
    for (ni, net) in nets.iter().enumerate() {
        let node_name = |n: &Node| -> String {
            match n {
                Node::Port(p) => escape(p),
                Node::Pin { instance, pin } => {
                    format!("*{}:{}", instance_index[instance.as_str()], escape(pin))
                }
                Node::Internal(k) => format!("*{}:{}", ni + 1, k),
            }
        };
        let other_name = |net_index: usize, node: u32| -> Result<String> {
            let other = nets
                .get(net_index)
                .ok_or_else(|| SpefError::Invalid("coupling net index".into()))?;
            let n = other
                .nodes
                .get(node as usize)
                .ok_or_else(|| SpefError::Invalid("coupling node index".into()))?;
            Ok(match n {
                Node::Port(p) => escape(p),
                Node::Pin { instance, pin } => {
                    format!("*{}:{}", instance_index[instance.as_str()], escape(pin))
                }
                Node::Internal(k) => format!("*{}:{}", net_index + 1, k),
            })
        };
        let mut lines = Vec::with_capacity(net.nodes.len() + net.couplings.len());
        let mut total = 0.0f64;
        for (i, g) in net.ground.iter().enumerate() {
            let (text, parsed) = value(*g)?;
            total += parsed;
            lines.push(format!("{} {}", node_name(&net.nodes[i]), text));
        }
        for &(node, other, other_node, pf) in &net.couplings {
            if other == ni {
                return Err(SpefError::Invalid("coupling within one net".into()));
            }
            let (text, parsed) = value(pf)?;
            if parsed == 0.0 {
                continue;
            }
            total += parsed;
            let own = net
                .nodes
                .get(node as usize)
                .ok_or_else(|| SpefError::Invalid("coupling node index".into()))?;
            lines.push(format!(
                "{} {} {}",
                node_name(own),
                other_name(other, other_node)?,
                text
            ));
        }
        writeln!(out)?;
        writeln!(out, "*D_NET *{} {:.9e}", ni + 1, total)?;
        writeln!(out, "*CONN")?;
        for c in &net.connections {
            let node = net
                .nodes
                .get(c.node as usize)
                .ok_or_else(|| SpefError::Invalid("connection node index".into()))?;
            match node {
                Node::Port(_) => writeln!(out, "*P {} {}", node_name(node), c.direction.code())?,
                Node::Pin { .. } => {
                    write!(out, "*I {} {}", node_name(node), c.direction.code())?;
                    if let Some(cell) = &c.cell {
                        write!(out, " *D {}", escape(cell))?;
                    }
                    writeln!(out)?;
                }
                Node::Internal(_) => {
                    return Err(SpefError::Invalid("connection on an internal node".into()));
                }
            }
        }
        writeln!(out, "*CAP")?;
        for (i, line) in lines.iter().enumerate() {
            writeln!(out, "{} {}", i + 1, line)?;
        }
        writeln!(out, "*RES")?;
        for (i, &(a, b, ohm)) in net.resistors.iter().enumerate() {
            let (text, _) = value(ohm)?;
            let na = net
                .nodes
                .get(a as usize)
                .ok_or_else(|| SpefError::Invalid("resistor node index".into()))?;
            let nb = net
                .nodes
                .get(b as usize)
                .ok_or_else(|| SpefError::Invalid("resistor node index".into()))?;
            writeln!(
                out,
                "{} {} {} {}",
                i + 1,
                node_name(na),
                node_name(nb),
                text
            )?;
        }
        writeln!(out, "*END")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Net> {
        vec![
            Net {
                name: "a[0]".into(),
                nodes: vec![
                    Node::Port("a[0]".into()),
                    Node::Internal(1),
                    Node::Pin {
                        instance: "g0".into(),
                        pin: "A".into(),
                    },
                ],
                connections: vec![
                    Connection {
                        node: 0,
                        direction: Direction::Input,
                        cell: None,
                    },
                    Connection {
                        node: 2,
                        direction: Direction::Input,
                        cell: Some("and2".into()),
                    },
                ],
                ground: vec![1e-4, 2.5e-4, 0.0],
                couplings: vec![(1, 1, 0, 3.333333333e-5)],
                resistors: vec![(0, 1, 1.5), (1, 2, 12.25)],
            },
            Net {
                name: "n.1".into(),
                nodes: vec![
                    Node::Pin {
                        instance: "g0".into(),
                        pin: "X".into(),
                    },
                    Node::Port("y".into()),
                ],
                connections: vec![
                    Connection {
                        node: 0,
                        direction: Direction::Output,
                        cell: Some("and2".into()),
                    },
                    Connection {
                        node: 1,
                        direction: Direction::Output,
                        cell: None,
                    },
                ],
                ground: vec![1e-5, 1e-5],
                couplings: vec![(0, 0, 1, 3.333333333e-5)],
                resistors: vec![(0, 1, 2.0)],
            },
        ]
    }

    #[test]
    fn writes_name_mapped_detailed_nets_with_exact_totals() {
        let header = Header {
            design: "top".into(),
            vendor: "v".into(),
            program: "p".into(),
            version: "1".into(),
            date: "fixed".into(),
        };
        let ports = vec![
            ("a[0]".to_string(), Direction::Input),
            ("y".to_string(), Direction::Output),
        ];
        let mut a = Vec::new();
        write(&header, &ports, &sample(), &mut a).unwrap();
        let mut b = Vec::new();
        write(&header, &ports, &sample(), &mut b).unwrap();
        assert_eq!(a, b);
        let text = String::from_utf8(a).unwrap();
        assert!(text.starts_with("*SPEF \"IEEE 1481-1999\"\n"));
        for want in [
            "*C_UNIT 1 PF",
            "*R_UNIT 1 OHM",
            "*1 a[0]\n",
            "*2 n\\.1\n",
            "*3 g0\n",
            "*P a[0] I\n",
            "*I *3:A I *D and2\n",
            "2 *1:1 2.50000e-4\n",
            "4 *1:1 *3:X 3.33333e-5\n",
            "2 *1:1 *3:A 1.22500e1\n",
            "*I *3:X O *D and2\n",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert_eq!(text.matches("*END").count(), 2);
        // Declared totals equal the sum of the listed rounded values.
        let total: f64 = [1e-4, 2.5e-4, 0.0, 3.33333e-5].iter().sum();
        assert!(text.contains(&format!("*D_NET *1 {total:.9e}")));
    }

    #[test]
    fn rejects_invalid_values_and_internal_connections() {
        let header = Header {
            design: "top".into(),
            vendor: "v".into(),
            program: "p".into(),
            version: "1".into(),
            date: "fixed".into(),
        };
        let mut nets = sample();
        nets[0].ground[0] = f64::NAN;
        assert!(write(&header, &[], &nets, &mut Vec::new()).is_err());
        let mut nets = sample();
        nets[0].resistors[0].2 = -1.0;
        assert!(write(&header, &[], &nets, &mut Vec::new()).is_err());
        let mut nets = sample();
        nets[0].connections[0].node = 1;
        assert!(write(&header, &[], &nets, &mut Vec::new()).is_err());
        let bad = Header {
            design: "a\"b".into(),
            ..header
        };
        assert!(write(&bad, &[], &sample(), &mut Vec::new()).is_err());
        assert_eq!(escape("x/y:z*"), "x\\/y\\:z\\*");
    }
}
