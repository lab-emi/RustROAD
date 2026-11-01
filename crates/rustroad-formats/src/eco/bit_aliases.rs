// SPDX-License-Identifier: Apache-2.0
//! Bounded alias roots for explicitly declared vector bits.
use super::{EcoCells, EcoError, PinView, Result};
use crate::verilog::{Expr, Netlist, VerilogLimits};
use std::collections::{HashMap, HashSet};

// Typed keys keep an escaped scalar `x[0]` distinct from bit zero of vector x.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Scalar(String),
    Bit(String, i64),
}
impl Key {
    fn from_expr(expr: &Expr) -> Option<Self> {
        match expr {
            Expr::Ident(name) => Some(Self::Scalar(name.clone())),
            Expr::BitSelect { name, index } => Some(Self::Bit(name.clone(), *index)),
            _ => None,
        }
    }
    fn name(&self) -> &str {
        match self {
            Self::Scalar(name) | Self::Bit(name, _) => name,
        }
    }
}
#[derive(Debug, Default)]
struct Node {
    parent: Option<usize>,
    assigned: bool,
}

pub(super) struct BitAliases {
    index: HashMap<Key, usize>,
    roots: Vec<usize>,
}
impl BitAliases {
    pub(super) fn new(n: &Netlist, cells: &EcoCells, limits: &VerilogLimits) -> Result<Self> {
        let ports: HashSet<_> = n.ports.iter().map(|p| p.name.as_str()).collect();
        let internal: HashSet<_> = n
            .wires
            .iter()
            .map(|w| w.name.as_str())
            .filter(|name| !ports.contains(name))
            .collect();
        let mut view = PinView::new(n, cells, limits)?;
        let mut index = HashMap::new();
        let mut nodes = Vec::<Node>::new();
        let mut bytes = 0usize;
        let mut intern = |key: Key, nodes: &mut Vec<Node>| -> Result<usize> {
            if let Some(id) = index.get(&key) {
                return Ok(*id);
            }
            // Includes the hash slot, node, root, visit state and bounded path IDs.
            bytes = bytes
                .checked_add(key.name().len().saturating_add(160))
                .ok_or_else(budget)?;
            if bytes > 64 * 1024 * 1024 || nodes.len() >= limits.max_expr_nodes {
                return Err(budget());
            }
            let id = nodes.len();
            index.insert(key, id);
            nodes.push(Node::default());
            Ok(id)
        };
        for assignment in &n.assigns {
            let lhs = view.expand(&assignment.lhs, None)?;
            let rhs = view.expand(&assignment.rhs, Some(lhs.len()))?;
            for (left, right) in lhs.iter().zip(&rhs) {
                let key = Key::from_expr(left).ok_or_else(|| {
                    EcoError::Invalid("alias assignment has a non-net lhs".into())
                })?;
                // Port assignments can move during an ECO, so they remain roots.
                if !internal.contains(key.name()) {
                    continue;
                }
                let at = intern(key, &mut nodes)?;
                if nodes[at].assigned {
                    return Err(EcoError::Invalid(
                        "multiple assignments to a net alias bit".into(),
                    ));
                }
                nodes[at].assigned = true;
                if let Some(key) = Key::from_expr(right) {
                    let to = intern(key, &mut nodes)?;
                    nodes[at].parent = Some(to);
                }
            }
        }
        let mut roots = vec![usize::MAX; nodes.len()];
        let mut state = vec![0u8; nodes.len()];
        let mut path = Vec::new();
        let mut work = 0usize;
        for start in 0..nodes.len() {
            if roots[start] != usize::MAX {
                continue;
            }
            path.clear();
            let mut node = start;
            let root = loop {
                work = work.checked_add(1).ok_or_else(budget)?;
                if work > limits.max_expr_nodes.saturating_mul(4) || path.len() > nodes.len() {
                    return Err(budget());
                }
                if roots[node] != usize::MAX {
                    break roots[node];
                }
                if state[node] == 1 {
                    return Err(EcoError::Invalid("net alias bit cycle".into()));
                }
                let Some(next) = nodes[node].parent else {
                    break node;
                };
                state[node] = 1;
                path.push(node);
                node = next;
            };
            roots[root] = root;
            for at in &path {
                roots[*at] = root;
                state[*at] = 2;
            }
        }
        Ok(Self { index, roots })
    }
    pub(super) fn same(&self, a: &Expr, b: &Expr) -> bool {
        if a == b {
            return true;
        }
        let (Some(a), Some(b)) = (Key::from_expr(a), Key::from_expr(b)) else {
            return false;
        };
        match (self.index.get(&a), self.index.get(&b)) {
            (Some(a), Some(b)) => self.roots[*a] == self.roots[*b],
            _ => false,
        }
    }
}
fn budget() -> EcoError {
    EcoError::Budget("explicit net alias bit index/work".into())
}
