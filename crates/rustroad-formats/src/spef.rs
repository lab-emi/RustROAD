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

/// Six significant digits, deterministic: the text in `text` (reused
/// scratch) and the value it denotes.
fn value(v: f64, text: &mut String) -> Result<f64> {
    value_text(v, text)?;
    text.parse()
        .map_err(|_| SpefError::Invalid("value formatting".into()))
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
    if nets.iter().any(|n| n.ground.len() != n.nodes.len()) {
        return Err(SpefError::Invalid("ground vector length".into()));
    }
    let names: Vec<&str> = nets.iter().map(|n| n.name.as_str()).collect();
    let instances = nets.iter().flat_map(|n| &n.nodes).filter_map(|n| match n {
        Node::Pin { instance, .. } => Some(instance.as_str()),
        _ => None,
    });
    write_streamed(
        header,
        ports,
        &names,
        instances,
        |i| Ok(nets[i].clone()),
        |n, p| {
            nets.get(n)
                .and_then(|n| n.nodes.get(p as usize))
                .cloned()
                .ok_or_else(|| SpefError::Invalid("coupling node index".into()))
        },
        out,
    )
}

/// Escaped name appended to `out` (same bytes as `escape`).
fn escape_into(out: &mut Vec<u8>, name: &str) {
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']')) {
            out.push(b'\\');
        }
        let mut utf8 = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
    }
}

/// The text of a value in `text` (reused scratch; validated like `value`,
/// not parsed back).
fn value_text(v: f64, text: &mut String) -> Result<()> {
    use std::fmt::Write as _;
    if !v.is_finite() || v < 0.0 {
        return Err(SpefError::Invalid(format!(
            "non-finite or negative value {v}"
        )));
    }
    text.clear();
    if v == 0.0 {
        text.push('0');
    } else {
        write!(text, "{v:.5e}").map_err(|_| SpefError::Invalid("value formatting".into()))?;
    }
    Ok(())
}

/// FxHash-style hasher for instance names (a lookup table, not a security
/// boundary; iteration order is never observed).
#[derive(Default)]
struct NameHasher(u64);
impl std::hash::Hasher for NameHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let (words, rest) = bytes.as_chunks::<8>();
        for &w in words {
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(K);
        }
        for &b in rest {
            self.0 = (self.0.rotate_left(5) ^ u64::from(b)).wrapping_mul(K);
        }
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
type NameIndex<'a> =
    std::collections::HashMap<&'a str, usize, std::hash::BuildHasherDefault<NameHasher>>;

/// Name map shared by every net of one document.
struct NameMap<'a> {
    nets: usize,
    instances: NameIndex<'a>,
}
impl NameMap<'_> {
    fn node(&self, out: &mut Vec<u8>, ni: usize, n: &Node) -> Result<()> {
        match n {
            Node::Port(p) => escape_into(out, p),
            Node::Pin { instance, pin } => {
                let i = self
                    .instances
                    .get(instance.as_str())
                    .ok_or_else(|| SpefError::Invalid("unlisted instance".into()))?;
                write!(out, "*{i}:")?;
                escape_into(out, pin);
            }
            Node::Internal(k) => write!(out, "*{}:{}", ni + 1, k)?,
        }
        Ok(())
    }
    fn other(&self, out: &mut Vec<u8>, net_index: usize, node: Node) -> Result<()> {
        match node {
            Node::Port(p) => escape_into(out, &p),
            Node::Pin { instance, pin } => {
                let i = self
                    .instances
                    .get(instance.as_str())
                    .ok_or_else(|| SpefError::Invalid("unlisted coupling instance".into()))?;
                write!(out, "*{i}:")?;
                escape_into(out, &pin);
            }
            Node::Internal(k) => write!(out, "*{}:{}", net_index + 1, k)?,
        }
        Ok(())
    }
}

/// Appends the `*D_NET` section of net `ni` to `out`, using `cap` as
/// scratch. On error, `out` holds exactly the bytes the section had
/// produced before the error (the streamed writer had written them).
fn net_section(
    names: &NameMap<'_>,
    net_names: &[&str],
    ni: usize,
    net: &Net,
    node_at: &mut impl FnMut(usize, u32) -> Result<Node>,
    out: &mut Vec<u8>,
    cap: &mut Vec<u8>,
) -> Result<()> {
    if net.name != net_names[ni] || net.ground.len() != net.nodes.len() {
        return Err(SpefError::Invalid(
            "generated net identity/ground length".into(),
        ));
    }
    if net.nodes.len().checked_add(net.couplings.len()).is_none() {
        return Err(SpefError::Invalid("capacitance record count".into()));
    }
    cap.clear();
    let mut count = 0usize;
    let mut total = 0.0f64;
    let mut text = String::new();
    for (i, g) in net.ground.iter().enumerate() {
        let parsed = value(*g, &mut text)?;
        total += parsed;
        count += 1;
        write!(cap, "{count} ")?;
        names.node(cap, ni, &net.nodes[i])?;
        writeln!(cap, " {text}")?;
    }
    for &(node, other, other_node, pf) in &net.couplings {
        if other == ni {
            return Err(SpefError::Invalid("coupling within one net".into()));
        }
        let parsed = value(pf, &mut text)?;
        if parsed == 0.0 {
            continue;
        }
        total += parsed;
        let own = net
            .nodes
            .get(node as usize)
            .ok_or_else(|| SpefError::Invalid("coupling node index".into()))?;
        count += 1;
        write!(cap, "{count} ")?;
        names.node(cap, ni, own)?;
        cap.push(b' ');
        if other >= names.nets {
            return Err(SpefError::Invalid("coupling net index".into()));
        }
        names.other(cap, other, node_at(other, other_node)?)?;
        writeln!(cap, " {text}")?;
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
            Node::Port(_) => {
                out.extend_from_slice(b"*P ");
                names.node(out, ni, node)?;
                writeln!(out, " {}", c.direction.code())?;
            }
            Node::Pin { .. } => {
                out.extend_from_slice(b"*I ");
                names.node(out, ni, node)?;
                write!(out, " {}", c.direction.code())?;
                if let Some(cell) = &c.cell {
                    out.extend_from_slice(b" *D ");
                    escape_into(out, cell);
                }
                writeln!(out)?;
            }
            Node::Internal(_) => {
                return Err(SpefError::Invalid("connection on an internal node".into()));
            }
        }
    }
    writeln!(out, "*CAP")?;
    out.extend_from_slice(cap);
    writeln!(out, "*RES")?;
    for (i, &(a, b, ohm)) in net.resistors.iter().enumerate() {
        value_text(ohm, &mut text)?;
        let na = net
            .nodes
            .get(a as usize)
            .ok_or_else(|| SpefError::Invalid("resistor node index".into()))?;
        let nb = net
            .nodes
            .get(b as usize)
            .ok_or_else(|| SpefError::Invalid("resistor node index".into()))?;
        write!(out, "{} ", i + 1)?;
        names.node(out, ni, na)?;
        out.push(b' ');
        names.node(out, ni, nb)?;
        writeln!(out, " {text}")?;
    }
    writeln!(out, "*END")?;
    Ok(())
}

/// Header, name map and ports; returns the name map for the nets.
fn preamble<'a>(
    header: &Header,
    ports: &[(String, Direction)],
    net_names: &[&str],
    instance_names: impl IntoIterator<Item = &'a str>,
    out: &mut impl Write,
) -> Result<NameMap<'a>> {
    if net_names.len() > 64_000_000 {
        return Err(SpefError::Invalid("net-name budget".into()));
    }
    let mut instances = Vec::new();
    let mut instance_index = NameIndex::default();
    for (visited, name) in instance_names.into_iter().enumerate() {
        if visited >= 256_000_000 {
            return Err(SpefError::Invalid("instance-name budget".into()));
        }
        if !instance_index.contains_key(name) {
            if instances.len() >= 16_000_000 {
                return Err(SpefError::Invalid("instance-name budget".into()));
            }
            instance_index.insert(name, net_names.len() + 1 + instances.len());
            instances.push(name);
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
    for (i, name) in net_names.iter().enumerate() {
        writeln!(out, "*{} {}", i + 1, escape(name))?;
    }
    for (i, name) in instances.iter().enumerate() {
        writeln!(out, "*{} {}", net_names.len() + 1 + i, escape(name))?;
    }
    writeln!(out)?;
    writeln!(out, "*PORTS")?;
    for (name, direction) in ports {
        writeln!(out, "{} {}", escape(name), direction.code())?;
    }
    Ok(NameMap {
        nets: net_names.len(),
        instances: instance_index,
    })
}

/// Serialize one generated net at a time. Names and instance first-use order
/// are supplied without cloning a second design-sized graph of labeled nodes.
/// The node resolver handles cross-net coupling endpoints independently of the
/// transient Net. Callbacks are invoked only for bounded existing indices.
pub fn write_streamed<'a>(
    header: &Header,
    ports: &[(String, Direction)],
    net_names: &[&str],
    instance_names: impl IntoIterator<Item = &'a str>,
    mut net_at: impl FnMut(usize) -> Result<Net>,
    mut node_at: impl FnMut(usize, u32) -> Result<Node>,
    out: &mut impl Write,
) -> Result<()> {
    let names = preamble(header, ports, net_names, instance_names, out)?;
    let (mut buf, mut cap) = (Vec::new(), Vec::new());
    for ni in 0..net_names.len() {
        let net = net_at(ni)?;
        buf.clear();
        let section = net_section(
            &names,
            net_names,
            ni,
            &net,
            &mut node_at,
            &mut buf,
            &mut cap,
        );
        out.write_all(&buf)?;
        section?;
    }
    Ok(())
}

/// Nets serialized concurrently before each in-order write.
const PARALLEL_NETS: usize = 256;

/// `write_streamed` with nets serialized on the current Rayon pool in
/// batches and written in net order. Each net's bytes depend only on that
/// net and the name map, so the document is byte-identical to
/// `write_streamed`, and the first error in net order is returned after the
/// bytes the streamed writer would have written before it.
pub fn write_streamed_parallel<'a>(
    header: &Header,
    ports: &[(String, Direction)],
    net_names: &[&str],
    instance_names: impl IntoIterator<Item = &'a str>,
    net_at: impl Fn(usize) -> Result<Net> + Sync,
    node_at: impl Fn(usize, u32) -> Result<Node> + Sync,
    out: &mut impl Write,
) -> Result<()> {
    use rayon::prelude::*;
    let names = preamble(header, ports, net_names, instance_names, out)?;
    let mut start = 0;
    while start < net_names.len() {
        let end = net_names.len().min(start + PARALLEL_NETS);
        let sections: Vec<(Vec<u8>, Result<()>)> = (start..end)
            .into_par_iter()
            .map_init(Vec::new, |cap, ni| {
                let mut buf = Vec::new();
                let section = net_at(ni).and_then(|net| {
                    net_section(
                        &names,
                        net_names,
                        ni,
                        &net,
                        &mut |n, p| node_at(n, p),
                        &mut buf,
                        cap,
                    )
                });
                (buf, section)
            })
            .collect();
        for (buf, section) in sections {
            out.write_all(&buf)?;
            section?;
        }
        start = end;
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
    #[test]
    fn parallel_serialization_is_byte_identical_and_fails_like_the_stream() {
        let header = Header {
            design: "top".into(),
            vendor: "v".into(),
            program: "p".into(),
            version: "1".into(),
            date: "fixed".into(),
        };
        // Many nets so several batches are formatted concurrently.
        let mut nets = Vec::new();
        for k in 0..700usize {
            let mut a = sample()[k % 2].clone();
            a.name = format!("n{k}/x");
            a.ground = a
                .ground
                .iter()
                .map(|g| g * (1.0 + k as f64 / 7.0))
                .collect();
            a.couplings = vec![(0, (k + 1) % 700, 1, 1.25e-5 * k as f64)];
            nets.push(a);
        }
        let names: Vec<_> = nets.iter().map(|n| n.name.as_str()).collect();
        // First-use order of instances, as `write` derives it.
        let instances = ["g0", "g0", "g0"];
        let stream = |nets: &[Net]| {
            let mut out = Vec::new();
            let r = write_streamed(
                &header,
                &[("y".to_string(), Direction::Output)],
                &names,
                instances,
                |i| Ok(nets[i].clone()),
                |n, p| Ok(nets[n].nodes[p as usize].clone()),
                &mut out,
            );
            (out, r.map_err(|e| e.to_string()))
        };
        let parallel = |nets: &[Net], threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let mut out = Vec::new();
            let r = pool.install(|| {
                write_streamed_parallel(
                    &header,
                    &[("y".to_string(), Direction::Output)],
                    &names,
                    instances,
                    |i| Ok(nets[i].clone()),
                    |n, p| Ok(nets[n].nodes[p as usize].clone()),
                    &mut out,
                )
            });
            (out, r.map_err(|e| e.to_string()))
        };
        let want = stream(&nets);
        assert!(want.1.is_ok());
        let mut expected = Vec::new();
        write(
            &header,
            &[("y".to_string(), Direction::Output)],
            &nets,
            &mut expected,
        )
        .unwrap();
        assert_eq!(want.0, expected);
        for threads in [1, 3] {
            assert_eq!(parallel(&nets, threads), want);
        }
        // Errors mid-document: an invalid connection (after the section
        // started) and an invalid resistor; the first in net order wins.
        let mut bad = nets.clone();
        bad[600].connections[0].node = 1;
        bad[300].resistors[0].2 = f64::NAN;
        let want = stream(&bad);
        assert!(want.1.is_err());
        for threads in [1, 4] {
            assert_eq!(parallel(&bad, threads), want);
        }
    }

    #[test]
    fn generated_nets_are_requested_once_in_order_and_match_serialization() {
        let header = Header {
            design: "top".into(),
            vendor: "v".into(),
            program: "p".into(),
            version: "1".into(),
            date: "fixed".into(),
        };
        let nets = sample();
        let mut expected = Vec::new();
        write(&header, &[], &nets, &mut expected).unwrap();
        let names: Vec<_> = nets.iter().map(|n| n.name.as_str()).collect();
        let mut requested = Vec::new();
        let mut actual = Vec::new();
        write_streamed(
            &header,
            &[],
            &names,
            ["g0", "g0"],
            |i| {
                requested.push(i);
                Ok(nets[i].clone())
            },
            |n, p| Ok(nets[n].nodes[p as usize].clone()),
            &mut actual,
        )
        .unwrap();
        assert_eq!(requested, vec![0, 1]);
        assert_eq!(actual, expected);
        assert!(
            write_streamed(
                &header,
                &[],
                &names,
                std::iter::empty(),
                |i| Ok(nets[i].clone()),
                |n, p| Ok(nets[n].nodes[p as usize].clone()),
                &mut Vec::new()
            )
            .is_err()
        );
        assert!(
            write_streamed(
                &header,
                &[],
                &names,
                ["g0"],
                |_| Ok(nets[1].clone()),
                |n, p| Ok(nets[n].nodes[p as usize].clone()),
                &mut Vec::new()
            )
            .is_err()
        );
    }
}
