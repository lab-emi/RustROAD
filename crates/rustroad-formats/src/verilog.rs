//! Bounded structural Verilog interchange, independently implemented for RustROAD.
//!
//! Before implementation: at the 16 MiB input cap, at most 2M lexer tokens
//! (approximately 96 MiB including vector capacity), token/AST identifier copies
//! (approximately 64 MiB), 1M expression nodes (approximately 64 MiB), and bounded
//! declaration/validation maps (approximately 96 MiB) give a conservative peak
//! below 384 MiB. Data is single-corner and shared; no graph paths, cones, clauses
//! or per-thread design copies are materialized. Expression recursion is capped
//! at 32 by default, leaving room for tagged AST JSON inside workspace metadata.
//! Tiny unit fixtures consume less than 1 MiB each; run at most
//! two tests/build jobs under the repository's documented OS/time guards.
//!
//! Supported: one flat module, ANSI/non-ANSI unsigned wire ports, wire vectors,
//! named cell connections, numeric bit/part selects, concatenation, constants,
//! and continuous aliases. Behavioral statements, operators, parameters, delays,
//! attributes, preprocessing and implicit nets produce explicit errors.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerilogLimits {
    pub max_input_bytes: usize,
    pub max_tokens: usize,
    pub max_instances: usize,
    pub max_connections: usize,
    pub max_nets: usize,
    pub max_ports: usize,
    pub max_expr_nodes: usize,
    pub max_depth: usize,
    pub max_identifier_bytes: usize,
    pub max_constant_bits: usize,
}
impl Default for VerilogLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_tokens: 2_000_000,
            max_instances: 250_000,
            max_connections: 1_000_000,
            max_nets: 250_000,
            max_ports: 65_536,
            max_expr_nodes: 1_000_000,
            max_depth: 32,
            max_identifier_bytes: 4096,
            max_constant_bits: 65_536,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerilogError {
    #[error("UNSUPPORTED_VERILOG at byte {offset}: {message}")]
    Syntax { offset: usize, message: String },
    #[error("VERILOG_BUDGET_EXCEEDED: {0}")]
    Budget(String),
    #[error("VERILOG_IO: {0}")]
    Io(#[from] std::io::Error),
}
type Result<T> = std::result::Result<T, VerilogError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,
    Output,
    Inout,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub msb: i64,
    pub lsb: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Port {
    pub name: String,
    pub direction: Direction,
    pub range: Option<Range>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wire {
    pub name: String,
    pub range: Option<Range>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Expr {
    Ident(String),
    BitSelect {
        name: String,
        index: i64,
    },
    PartSelect {
        name: String,
        range: Range,
    },
    Concat(Vec<Expr>),
    Constant(String),
    /// Legal only for an explicitly open named cell connection, `.PIN()`.
    Unconnected,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assign {
    pub lhs: Expr,
    pub rhs: Expr,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub pin: String,
    pub signal: Expr,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub name: String,
    pub cell: String,
    pub connections: Vec<Connection>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Netlist {
    pub module: String,
    pub ports: Vec<Port>,
    pub wires: Vec<Wire>,
    pub assigns: Vec<Assign>,
    pub instances: Vec<Instance>,
}

#[derive(Clone, Debug)]
enum Kind {
    Ident(String, bool),
    Number(String),
    Punct(u8),
}
#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    offset: usize,
}
fn syntax(offset: usize, message: impl Into<String>) -> VerilogError {
    VerilogError::Syntax {
        offset,
        message: message.into(),
    }
}
fn budget(message: &str) -> VerilogError {
    VerilogError::Budget(message.into())
}
fn identifier_ok(name: &str, limits: &VerilogLimits) -> Result<()> {
    if name.is_empty() || name.len() > limits.max_identifier_bytes {
        return Err(budget("empty or oversized identifier"));
    }
    if !name.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(syntax(
            0,
            "identifiers must contain printable ASCII without whitespace",
        ));
    }
    Ok(())
}
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "module"
            | "endmodule"
            | "input"
            | "output"
            | "inout"
            | "wire"
            | "tri"
            | "tri0"
            | "tri1"
            | "wand"
            | "wor"
            | "triand"
            | "trior"
            | "trireg"
            | "assign"
            | "deassign"
            | "reg"
            | "logic"
            | "bit"
            | "integer"
            | "time"
            | "real"
            | "realtime"
            | "signed"
            | "unsigned"
            | "parameter"
            | "localparam"
            | "defparam"
            | "supply0"
            | "supply1"
            | "always"
            | "always_comb"
            | "always_ff"
            | "always_latch"
            | "initial"
            | "final"
            | "begin"
            | "end"
            | "if"
            | "else"
            | "case"
            | "casex"
            | "casez"
            | "endcase"
            | "default"
            | "for"
            | "while"
            | "repeat"
            | "forever"
            | "generate"
            | "endgenerate"
            | "genvar"
            | "function"
            | "endfunction"
            | "task"
            | "endtask"
            | "specify"
            | "endspecify"
            | "primitive"
            | "endprimitive"
            | "table"
            | "endtable"
            | "and"
            | "nand"
            | "or"
            | "nor"
            | "xor"
            | "xnor"
            | "buf"
            | "not"
            | "bufif0"
            | "bufif1"
            | "notif0"
            | "notif1"
            | "nmos"
            | "pmos"
            | "rnmos"
            | "rpmos"
            | "cmos"
            | "rcmos"
            | "tran"
            | "rtran"
            | "tranif0"
            | "tranif1"
            | "rtranif0"
            | "rtranif1"
            | "pullup"
            | "pulldown"
            | "posedge"
            | "negedge"
            | "edge"
            | "event"
            | "wait"
            | "disable"
            | "fork"
            | "join"
            | "join_any"
            | "join_none"
            | "force"
            | "release"
            | "automatic"
            | "static"
            | "macromodule"
            | "config"
            | "endconfig"
            | "design"
            | "instance"
            | "cell"
            | "library"
            | "liblist"
            | "use"
            | "interface"
            | "endinterface"
            | "modport"
            | "package"
            | "endpackage"
            | "import"
            | "export"
            | "typedef"
            | "struct"
            | "union"
            | "enum"
            | "class"
            | "endclass"
            | "virtual"
            | "const"
            | "var"
            | "ref"
            | "int"
            | "longint"
            | "shortint"
            | "byte"
            | "string"
            | "void"
            | "return"
            | "break"
            | "continue"
            | "do"
            | "foreach"
            | "assert"
            | "assume"
            | "cover"
            | "property"
            | "endproperty"
            | "sequence"
            | "endsequence"
            | "clocking"
            | "endclocking"
            | "program"
            | "endprogram"
            | "accept_on"
            | "alias"
            | "before"
            | "bind"
            | "bins"
            | "binsof"
            | "chandle"
            | "checker"
            | "endchecker"
            | "constraint"
            | "context"
            | "covergroup"
            | "coverpoint"
            | "cross"
            | "dist"
            | "endgroup"
            | "eventually"
            | "expect"
            | "extends"
            | "extern"
            | "first_match"
            | "forkjoin"
            | "global"
            | "iff"
            | "ifnone"
            | "ignore_bins"
            | "illegal_bins"
            | "implements"
            | "implies"
            | "incdir"
            | "include"
            | "inside"
            | "interconnect"
            | "intersect"
            | "large"
            | "let"
            | "local"
            | "matches"
            | "medium"
            | "nettype"
            | "new"
            | "nexttime"
            | "noshowcancelled"
            | "null"
            | "packed"
            | "priority"
            | "protected"
            | "pull0"
            | "pull1"
            | "pure"
            | "pulsestyle_ondetect"
            | "pulsestyle_onevent"
            | "rand"
            | "randc"
            | "randcase"
            | "randsequence"
            | "reject_on"
            | "s_always"
            | "s_eventually"
            | "s_nexttime"
            | "s_until"
            | "s_until_with"
            | "scalared"
            | "shortreal"
            | "showcancelled"
            | "small"
            | "soft"
            | "solve"
            | "specparam"
            | "strong"
            | "strong0"
            | "strong1"
            | "super"
            | "sync_accept_on"
            | "sync_reject_on"
            | "tagged"
            | "this"
            | "throughout"
            | "timeprecision"
            | "timeunit"
            | "type"
            | "unique"
            | "unique0"
            | "until"
            | "until_with"
            | "untyped"
            | "uwire"
            | "vectored"
            | "wait_order"
            | "weak"
            | "weak0"
            | "weak1"
            | "wildcard"
            | "with"
            | "within"
            | "highz0"
            | "highz1"
    )
}
fn simple_identifier(name: &str) -> bool {
    let mut chars = name.bytes();
    matches!(chars.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && chars.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        && !reserved(name)
}

fn lex(input: &str, limits: &VerilogLimits) -> Result<Vec<Token>> {
    if input.len() > limits.max_input_bytes {
        return Err(budget("input byte limit"));
    }
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if bytes[i..].starts_with(b"//") || bytes[i..].starts_with(b"/*") {
            let block = bytes[i + 1] == b'*';
            i += 2;
            let content_start = i;
            if block {
                while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                if i + 1 == bytes.len() || i >= bytes.len() {
                    return Err(syntax(start, "unterminated block comment"));
                }
            } else {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            let comment = input[content_start..i].to_ascii_lowercase();
            if [
                "translate_off",
                "translate_on",
                "synopsys ",
                "synthesis ",
                "syn_black_box",
                "verilator ",
            ]
            .iter()
            .any(|p| comment.contains(p))
            {
                return Err(syntax(start, "semantic comment pragma is unsupported"));
            }
            if block {
                i += 2;
            }
            continue;
        }
        if out.len() >= limits.max_tokens {
            return Err(budget("token limit"));
        }
        let kind = if bytes[i] == b'\\' {
            i += 1;
            let begin = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i == bytes.len() {
                return Err(syntax(
                    start,
                    "escaped identifier needs a whitespace terminator",
                ));
            }
            let name = &input[begin..i];
            identifier_ok(name, limits)?;
            Kind::Ident(name.into(), true)
        } else if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            let name = &input[start..i];
            identifier_ok(name, limits)?;
            Kind::Ident(name.into(), false)
        } else if bytes[i].is_ascii_digit() || bytes[i] == b'\'' {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'\'' | b'?'))
            {
                i += 1;
            }
            let value = &input[start..i];
            constant_ok(value, limits).map_err(|e| match e {
                VerilogError::Syntax { message, .. } => syntax(start, message),
                other => other,
            })?;
            Kind::Number(value.into())
        } else if b"();,.:[]{}=-#".contains(&bytes[i]) {
            i += 1;
            Kind::Punct(bytes[start])
        } else {
            return Err(syntax(
                start,
                "unsupported token/operator/preprocessor/attribute",
            ));
        };
        out.push(Token {
            kind,
            offset: start,
        });
    }
    Ok(out)
}

fn constant_ok(value: &str, limits: &VerilogLimits) -> Result<()> {
    if value.len() > limits.max_input_bytes {
        return Err(budget("constant text limit"));
    }
    let lower = value.to_ascii_lowercase();
    if matches!(lower.as_str(), "'0" | "'1" | "'x" | "'z") {
        return Ok(());
    }
    if let Some((size, suffix)) = lower.split_once('\'') {
        if suffix.contains('\'') {
            return Err(syntax(0, "multiple apostrophes in constant"));
        }
        if !size.is_empty() {
            let width = unsigned_decimal(size)?;
            if width == 0 || width > limits.max_constant_bits as u64 {
                return Err(budget("constant bit width limit"));
            }
        }
        let Some(base) = suffix.bytes().next() else {
            return Err(syntax(0, "constant base is missing"));
        };
        if base == b's' {
            return Err(syntax(0, "signed constants are unsupported"));
        }
        let digits: String = suffix[1..].chars().filter(|c| *c != '_').collect();
        if digits.is_empty() {
            return Err(syntax(0, "constant digits are missing"));
        }
        let digit_ok = |b: u8| {
            matches!(b, b'x' | b'z' | b'?')
                || match base {
                    b'b' => matches!(b, b'0' | b'1'),
                    b'o' => (b'0'..=b'7').contains(&b),
                    b'h' => b.is_ascii_hexdigit(),
                    b'd' => b.is_ascii_digit(),
                    _ => false,
                }
        };
        if !matches!(base, b'b' | b'o' | b'd' | b'h')
            || !digits.bytes().all(digit_ok)
            || (base == b'd'
                && !digits.bytes().all(|b| b.is_ascii_digit())
                && !matches!(digits.as_str(), "x" | "z" | "?"))
        {
            return Err(syntax(0, "invalid based constant"));
        }
        // Bounds text even when the literal omits an explicit width.
        if digits.len() > limits.max_constant_bits {
            return Err(budget("constant digit limit"));
        }
        Ok(())
    } else {
        unsigned_decimal(value)?;
        Ok(())
    }
}
fn unsigned_decimal(value: &str) -> Result<u64> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        || !value.bytes().any(|b| b.is_ascii_digit())
    {
        return Err(syntax(0, "expected unsigned decimal integer"));
    }
    value
        .replace('_', "")
        .parse()
        .map_err(|_| syntax(0, "decimal integer exceeds u64"))
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    limits: &'a VerilogLimits,
    expr_nodes: usize,
    connections: usize,
}
impl Parser<'_> {
    fn offset(&self) -> usize {
        self.tokens.get(self.pos).map_or(0, |t| t.offset)
    }
    fn error(&self, message: &str) -> VerilogError {
        syntax(self.offset(), message)
    }
    fn keyword(&mut self, value: &str) -> bool {
        if matches!(self.tokens.get(self.pos), Some(Token { kind: Kind::Ident(s, false), .. }) if s == value)
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn punct(&mut self, value: u8) -> bool {
        if matches!(self.tokens.get(self.pos), Some(Token { kind: Kind::Punct(b), .. }) if *b == value)
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, value: u8) -> Result<()> {
        if self.punct(value) {
            Ok(())
        } else {
            Err(self.error(&format!(
                "expected '{}' in structural subset",
                value as char
            )))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.tokens.get(self.pos) {
            Some(Token {
                kind: Kind::Ident(s, escaped),
                ..
            }) if *escaped || (!reserved(s) && !s.starts_with('$')) => {
                let result = s.clone();
                self.pos += 1;
                Ok(result)
            }
            _ => {
                Err(self
                    .error("expected identifier; keyword/behavioral constructs are unsupported"))
            }
        }
    }
    fn integer(&mut self) -> Result<i64> {
        let negative = self.punct(b'-');
        let Some(Token {
            kind: Kind::Number(s),
            ..
        }) = self.tokens.get(self.pos)
        else {
            return Err(self.error("range/select requires a decimal integer"));
        };
        let number = unsigned_decimal(s)?;
        self.pos += 1;
        let value = if negative {
            -i128::from(number)
        } else {
            i128::from(number)
        };
        i64::try_from(value).map_err(|_| self.error("range/select integer exceeds i64"))
    }
    fn range(&mut self) -> Result<Option<Range>> {
        if !self.punct(b'[') {
            return Ok(None);
        }
        let msb = self.integer()?;
        self.expect(b':')?;
        let lsb = self.integer()?;
        self.expect(b']')?;
        Ok(Some(Range { msb, lsb }))
    }
    fn direction(&mut self) -> Option<Direction> {
        if self.keyword("input") {
            Some(Direction::Input)
        } else if self.keyword("output") {
            Some(Direction::Output)
        } else if self.keyword("inout") {
            Some(Direction::Inout)
        } else {
            None
        }
    }
    fn expr(&mut self, depth: usize) -> Result<Expr> {
        if depth > self.limits.max_depth.min(128) {
            return Err(budget("expression nesting limit"));
        }
        if self.expr_nodes >= self.limits.max_expr_nodes {
            return Err(budget("expression node limit"));
        }
        self.expr_nodes += 1;
        if self.punct(b'{') {
            let mut parts = Vec::new();
            loop {
                parts.push(self.expr(depth + 1)?);
                if self.punct(b'}') {
                    return Ok(Expr::Concat(parts));
                }
                self.expect(b',')?;
            }
        }
        if let Some(Token {
            kind: Kind::Number(s),
            ..
        }) = self.tokens.get(self.pos)
        {
            let value = s.clone();
            self.pos += 1;
            return Ok(Expr::Constant(value));
        }
        let name = self.ident()?;
        if !self.punct(b'[') {
            return Ok(Expr::Ident(name));
        }
        let left = self.integer()?;
        let result = if self.punct(b':') {
            Expr::PartSelect {
                name,
                range: Range {
                    msb: left,
                    lsb: self.integer()?,
                },
            }
        } else {
            Expr::BitSelect { name, index: left }
        };
        self.expect(b']')?;
        Ok(result)
    }
    fn parse(mut self) -> Result<Netlist> {
        if !self.keyword("module") {
            return Err(self.error("expected one flat module"));
        }
        let module = self.ident()?;
        let mut order = Vec::new();
        let mut header_names = HashSet::new();
        let mut declarations = HashMap::new();
        if self.punct(b'(') && !self.punct(b')') {
            let mut current: Option<(Direction, Option<Range>)> = None;
            loop {
                if let Some(direction) = self.direction() {
                    self.keyword("wire");
                    current = Some((direction, self.range()?));
                }
                let name = self.ident()?;
                if order.len() >= self.limits.max_ports {
                    return Err(budget("port limit"));
                }
                if !header_names.insert(name.clone()) {
                    return Err(self.error("duplicate module port"));
                }
                if let Some((direction, range)) = current {
                    if !order.is_empty() && declarations.is_empty() {
                        return Err(self.error("mixed ANSI/non-ANSI ports are unsupported"));
                    }
                    declarations.insert(
                        name.clone(),
                        Port {
                            name: name.clone(),
                            direction,
                            range,
                        },
                    );
                } else if !declarations.is_empty() {
                    return Err(self.error("mixed ANSI/non-ANSI ports are unsupported"));
                }
                order.push(name);
                if self.punct(b')') {
                    break;
                }
                self.expect(b',')?;
            }
        }
        self.expect(b';')?;
        let mut wires = Vec::new();
        let mut assigns = Vec::new();
        let mut instances = Vec::new();
        let port_names: HashSet<_> = order.iter().cloned().collect();
        while !self.keyword("endmodule") {
            if self.pos == self.tokens.len() {
                return Err(self.error("missing endmodule"));
            }
            if let Some(direction) = self.direction() {
                self.keyword("wire");
                let range = self.range()?;
                loop {
                    let name = self.ident()?;
                    if !port_names.contains(&name) {
                        return Err(self.error("port declaration absent from module header"));
                    }
                    if declarations
                        .insert(
                            name.clone(),
                            Port {
                                name,
                                direction,
                                range,
                            },
                        )
                        .is_some()
                    {
                        return Err(self.error("duplicate port declaration"));
                    }
                    if self.punct(b';') {
                        break;
                    }
                    self.expect(b',')?;
                }
            } else if self.keyword("wire") {
                let range = self.range()?;
                loop {
                    if wires.len() >= self.limits.max_nets {
                        return Err(budget("wire limit"));
                    }
                    let name = self.ident()?;
                    wires.push(Wire {
                        name: name.clone(),
                        range,
                    });
                    if self.punct(b'=') {
                        if self.expr_nodes >= self.limits.max_expr_nodes {
                            return Err(budget("expression node limit"));
                        }
                        self.expr_nodes += 1;
                        assigns.push(Assign {
                            lhs: Expr::Ident(name),
                            rhs: self.expr(1)?,
                        });
                    }
                    if self.punct(b';') {
                        break;
                    }
                    self.expect(b',')?;
                }
            } else if self.keyword("assign") {
                loop {
                    let lhs = self.expr(1)?;
                    self.expect(b'=')?;
                    let rhs = self.expr(1)?;
                    assigns.push(Assign { lhs, rhs });
                    if self.punct(b';') {
                        break;
                    }
                    self.expect(b',')?;
                }
            } else {
                let cell = self.ident()?;
                loop {
                    if instances.len() >= self.limits.max_instances {
                        return Err(budget("instance limit"));
                    }
                    let name = self.ident()?;
                    self.expect(b'(')?;
                    let mut connections = Vec::new();
                    if !self.punct(b')') {
                        loop {
                            if self.connections >= self.limits.max_connections {
                                return Err(budget("connection limit"));
                            }
                            self.connections += 1;
                            self.expect(b'.')?;
                            let pin = self.ident()?;
                            self.expect(b'(')?;
                            let signal = if self.punct(b')') {
                                Expr::Unconnected
                            } else {
                                let signal = self.expr(1)?;
                                self.expect(b')')?;
                                signal
                            };
                            connections.push(Connection { pin, signal });
                            if self.punct(b')') {
                                break;
                            }
                            self.expect(b',')?;
                        }
                    }
                    instances.push(Instance {
                        name,
                        cell: cell.clone(),
                        connections,
                    });
                    if self.punct(b';') {
                        break;
                    }
                    self.expect(b',')?;
                }
            }
        }
        if self.pos != self.tokens.len() {
            return Err(self.error("multiple modules/trailing constructs are unsupported"));
        }
        let ports = order
            .into_iter()
            .map(|name| {
                declarations
                    .remove(&name)
                    .ok_or_else(|| self.error("module port has no direction declaration"))
            })
            .collect::<Result<Vec<_>>>()?;
        let netlist = Netlist {
            module,
            ports,
            wires,
            assigns,
            instances,
        };
        validate_netlist(&netlist, self.limits)?;
        Ok(netlist)
    }
}

/// Parse a flat mapped module. Names retain their exact logical spelling;
/// the leading backslash and terminating whitespace are syntax, not name data.
pub fn parse_verilog(input: &str, limits: &VerilogLimits) -> Result<Netlist> {
    Parser {
        tokens: lex(input, limits)?,
        pos: 0,
        limits,
        expr_nodes: 0,
        connections: 0,
    }
    .parse()
}

fn validate_expr(
    expr: &Expr,
    nets: &HashMap<&str, Option<Range>>,
    limits: &VerilogLimits,
    nodes: &mut usize,
    depth: usize,
    lhs: bool,
    open: bool,
) -> Result<()> {
    if depth > limits.max_depth.min(128) || *nodes >= limits.max_expr_nodes {
        return Err(budget("expression depth/node limit"));
    }
    *nodes += 1;
    let reference = |name: &str, select: Option<Range>| -> Result<()> {
        identifier_ok(name, limits)?;
        let declared = nets.get(name).ok_or_else(|| {
            syntax(
                0,
                format!("undeclared net {name:?}; implicit nets are unsupported"),
            )
        })?;
        if let Some(select) = select {
            let declared = declared.unwrap_or(Range { msb: 0, lsb: 0 });
            let lo = declared.msb.min(declared.lsb);
            let hi = declared.msb.max(declared.lsb);
            if select.msb < lo
                || select.msb > hi
                || select.lsb < lo
                || select.lsb > hi
                || (select.msb != select.lsb
                    && (select.msb > select.lsb) != (declared.msb > declared.lsb))
            {
                return Err(syntax(
                    0,
                    "select outside declaration or reversed part-select",
                ));
            }
        }
        Ok(())
    };
    match expr {
        Expr::Ident(name) => reference(name, None),
        Expr::BitSelect { name, index } => reference(
            name,
            Some(Range {
                msb: *index,
                lsb: *index,
            }),
        ),
        Expr::PartSelect { name, range } => reference(name, Some(*range)),
        Expr::Concat(parts) => {
            if parts.is_empty() {
                return Err(syntax(0, "empty concatenation"));
            }
            for part in parts {
                validate_expr(part, nets, limits, nodes, depth + 1, lhs, false)?;
            }
            Ok(())
        }
        Expr::Constant(value) if !lhs => constant_ok(value, limits),
        Expr::Unconnected if open && !lhs => Ok(()),
        _ => Err(syntax(
            0,
            "constant/open expression is not a valid assignment destination",
        )),
    }
}

/// Validate deserialized/programmatically constructed ASTs using the same limits.
pub fn validate_netlist(netlist: &Netlist, limits: &VerilogLimits) -> Result<()> {
    if netlist.ports.len() > limits.max_ports
        || netlist.wires.len() > limits.max_nets
        || netlist.instances.len() > limits.max_instances
    {
        return Err(budget("netlist object count limit"));
    }
    identifier_ok(&netlist.module, limits)?;
    let mut nets = HashMap::new();
    for port in &netlist.ports {
        identifier_ok(&port.name, limits)?;
        if nets.insert(port.name.as_str(), port.range).is_some() {
            return Err(syntax(0, "duplicate port"));
        }
    }
    let mut wire_names = HashSet::new();
    for wire in &netlist.wires {
        identifier_ok(&wire.name, limits)?;
        if !wire_names.insert(wire.name.as_str()) {
            return Err(syntax(0, "duplicate wire"));
        }
        if let Some(prior) = nets.insert(wire.name.as_str(), wire.range)
            && prior != wire.range
        {
            return Err(syntax(0, "wire redeclaration disagrees with port range"));
        }
    }
    let mut nodes = 0;
    for assign in &netlist.assigns {
        validate_expr(&assign.lhs, &nets, limits, &mut nodes, 1, true, false)?;
        validate_expr(&assign.rhs, &nets, limits, &mut nodes, 1, false, false)?;
    }
    let mut instances = HashSet::new();
    let mut connections = 0usize;
    for instance in &netlist.instances {
        identifier_ok(&instance.name, limits)?;
        identifier_ok(&instance.cell, limits)?;
        if !instances.insert(instance.name.as_str()) {
            return Err(syntax(0, "duplicate instance"));
        }
        let mut pins = HashSet::new();
        for connection in &instance.connections {
            if connections >= limits.max_connections {
                return Err(budget("connection limit"));
            }
            connections += 1;
            identifier_ok(&connection.pin, limits)?;
            if !pins.insert(connection.pin.as_str()) {
                return Err(syntax(0, "duplicate named pin connection"));
            }
            validate_expr(
                &connection.signal,
                &nets,
                limits,
                &mut nodes,
                1,
                false,
                true,
            )?;
        }
    }
    Ok(())
}

fn emit_name(name: &str, output: &mut impl Write) -> Result<()> {
    if simple_identifier(name) {
        write!(output, "{name}")?;
    } else {
        write!(output, "\\{name} ")?;
    }
    Ok(())
}
fn emit_range(range: Option<Range>, output: &mut impl Write) -> Result<()> {
    if let Some(range) = range {
        write!(output, "[{}:{}] ", range.msb, range.lsb)?;
    }
    Ok(())
}
fn emit_expr(expr: &Expr, output: &mut impl Write) -> Result<()> {
    match expr {
        Expr::Ident(name) => emit_name(name, output)?,
        Expr::BitSelect { name, index } => {
            emit_name(name, output)?;
            write!(output, "[{index}]")?;
        }
        Expr::PartSelect { name, range } => {
            emit_name(name, output)?;
            write!(output, "[{}:{}]", range.msb, range.lsb)?;
        }
        Expr::Constant(value) => write!(output, "{value}")?,
        Expr::Concat(parts) => {
            write!(output, "{{")?;
            for (i, part) in parts.iter().enumerate() {
                if i != 0 {
                    write!(output, ", ")?;
                }
                emit_expr(part, output)?;
            }
            write!(output, "}}")?;
        }
        Expr::Unconnected => {}
    }
    Ok(())
}
/// Write deterministic structural Verilog. This validates the AST before output;
/// no names are sanitized or collapsed. Escaped names always get a delimiter.
pub fn write_verilog(netlist: &Netlist, output: &mut impl Write) -> Result<()> {
    write_verilog_with_limits(netlist, output, &VerilogLimits::default())
}

/// Like `write_verilog`, with explicit AST and output-byte budgets. Exhausting
/// output capacity can leave a partial stream; publish to a new file atomically.
pub fn write_verilog_with_limits(
    netlist: &Netlist,
    output: &mut impl Write,
    limits: &VerilogLimits,
) -> Result<()> {
    validate_netlist(netlist, limits)?;
    struct Capped<'a, W> {
        output: &'a mut W,
        remaining: usize,
        exceeded: bool,
    }
    impl<W: Write> Write for Capped<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(std::io::Error::other("Verilog output byte budget exceeded"));
            }
            let n = self.output.write(bytes)?;
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.output.flush()
        }
    }
    let mut capped = Capped {
        output,
        remaining: limits.max_input_bytes,
        exceeded: false,
    };
    let result = write_body(netlist, &mut capped);
    if capped.exceeded {
        Err(budget("Verilog output byte limit"))
    } else {
        result
    }
}

fn write_body(netlist: &Netlist, output: &mut impl Write) -> Result<()> {
    write!(output, "module ")?;
    emit_name(&netlist.module, output)?;
    writeln!(output, " (")?;
    for (i, port) in netlist.ports.iter().enumerate() {
        let direction = match port.direction {
            Direction::Input => "input",
            Direction::Output => "output",
            Direction::Inout => "inout",
        };
        write!(output, "  {direction} wire ")?;
        emit_range(port.range, output)?;
        emit_name(&port.name, output)?;
        writeln!(
            output,
            "{}",
            if i + 1 == netlist.ports.len() {
                ""
            } else {
                ","
            }
        )?;
    }
    writeln!(output, ");")?;
    for wire in &netlist.wires {
        write!(output, "  wire ")?;
        emit_range(wire.range, output)?;
        emit_name(&wire.name, output)?;
        writeln!(output, ";")?;
    }
    for assign in &netlist.assigns {
        write!(output, "  assign ")?;
        emit_expr(&assign.lhs, output)?;
        write!(output, " = ")?;
        emit_expr(&assign.rhs, output)?;
        writeln!(output, ";")?;
    }
    for instance in &netlist.instances {
        write!(output, "  ")?;
        emit_name(&instance.cell, output)?;
        write!(output, " ")?;
        emit_name(&instance.name, output)?;
        write!(output, " (")?;
        for (i, connection) in instance.connections.iter().enumerate() {
            if i != 0 {
                write!(output, ", ")?;
            }
            write!(output, ".")?;
            emit_name(&connection.pin, output)?;
            write!(output, "(")?;
            emit_expr(&connection.signal, output)?;
            write!(output, ")")?;
        }
        writeln!(output, ");")?;
    }
    writeln!(output, "endmodule")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> Netlist {
        parse_verilog(s, &VerilogLimits::default()).unwrap()
    }
    #[test]
    fn mapped_pdk_cells_and_open_pin_roundtrip() {
        for cell in ["sky130_fd_sc_hd__xor2_4", "gf180mcu_fd_sc_mcu7t5v0__xor2_2"] {
            let source = format!(
                "module top(input wire a, b, output wire y); {cell} g0 (.A(a), .B(b), .X(y), .UNUSED()); endmodule"
            );
            let netlist = parse(&source);
            assert_eq!(netlist.instances[0].cell, cell);
            assert_eq!(netlist.ports[1].direction, Direction::Input);
            let mut out = Vec::new();
            write_verilog(&netlist, &mut out).unwrap();
            assert_eq!(parse(std::str::from_utf8(&out).unwrap()), netlist);
        }
    }
    #[test]
    fn escaped_identifiers_do_not_lose_brackets_dollars_or_keywords() {
        let source = r"module \module (input \a[0] , output \y/out ); wire \$auto$1 ; \cell/type \inst.name (.\input (\a[0] ), .Y(\$auto$1 )); assign \y/out = \$auto$1 ; endmodule";
        let netlist = parse(source);
        assert_eq!(netlist.module, "module");
        assert_eq!(netlist.ports[0].name, "a[0]");
        assert_eq!(netlist.instances[0].connections[0].pin, "input");
        let mut out = Vec::new();
        write_verilog(&netlist, &mut out).unwrap();
        assert_eq!(parse(std::str::from_utf8(&out).unwrap()), netlist);
    }
    #[test]
    fn non_ansi_vectors_aliases_and_constants() {
        let n = parse(
            "module m(a,y); input [7:0] a; output [3:0] y; wire [3:0] n; assign n={a[7:6], 1'b0, a[0]}; assign y=n; endmodule",
        );
        assert_eq!(n.ports[0].range, Some(Range { msb: 7, lsb: 0 }));
        assert_eq!(n.assigns.len(), 2);
        assert!(matches!(n.assigns[0].rhs, Expr::Concat(_)));
    }
    #[test]
    fn unsupported_semantics_and_malformed_inputs_fail() {
        for source in [
            "module m(input a, output y); assign y=~a; endmodule",
            "module m(input a, output y); always @(a) y=a; endmodule",
            "module m #(parameter W=2)(); endmodule",
            "module m(input a); C c(a); endmodule",
            "module m(input a); C c(.A(a),.A(a)); endmodule",
            "module m(output y); assign y=unknown; endmodule",
            "module m(output y); assign 1'b0=y; endmodule",
            "module m(input [3:0] a, output y); assign y=a[4]; endmodule",
            "module m(input [3:0] a, output [1:0] y); assign y=a[0:1]; endmodule",
            "module m(output y); assign y=2'b2; endmodule",
            "module m(output y); assign y=1'sb0; endmodule",
            "module m(); endmodule module n(); endmodule",
            "`default_nettype none\nmodule m(); endmodule",
            "/* synthesis translate_off */ module m(); endmodule",
            "module m(); endmodule /*",
        ] {
            assert!(
                parse_verilog(source, &VerilogLimits::default()).is_err(),
                "accepted {source}"
            );
        }
    }
    #[test]
    fn caps_apply_before_unbounded_allocation_or_recursion() {
        let limits = VerilogLimits {
            max_depth: 2,
            ..VerilogLimits::default()
        };
        assert!(
            parse_verilog(
                "module m(input a,output y); assign y={{{a}}}; endmodule",
                &limits
            )
            .is_err()
        );
        let limits = VerilogLimits {
            max_tokens: 2,
            ..VerilogLimits::default()
        };
        assert!(matches!(
            parse_verilog("module m(); endmodule", &limits),
            Err(VerilogError::Budget(_))
        ));
        let limits = VerilogLimits {
            max_instances: 0,
            ..VerilogLimits::default()
        };
        assert!(parse_verilog("module m(input a); C c(.A(a)); endmodule", &limits).is_err());
        let limits = VerilogLimits {
            max_constant_bits: 4,
            ..VerilogLimits::default()
        };
        assert!(parse_verilog("module m(output y); assign y=100'b0; endmodule", &limits).is_err());
        let netlist = parse("module m(); endmodule");
        let mut output = Vec::new();
        let limits = VerilogLimits {
            max_input_bytes: 8,
            ..VerilogLimits::default()
        };
        assert!(matches!(
            write_verilog_with_limits(&netlist, &mut output, &limits),
            Err(VerilogError::Budget(_))
        ));
        assert!(output.len() <= 8);
    }
}
