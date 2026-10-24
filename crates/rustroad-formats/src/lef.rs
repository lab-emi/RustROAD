//! Clean-room LEF geometry reader; standard-format PDK data is the only corpus.
//!
//! Memory/work preflight: the lexer borrows the input and keeps one lookahead;
//! statements retain at most 8192 borrowed tokens. Default input <=32 MiB,
//! <=4 million consumed tokens, <=500,000 shapes and <=2 million polygon points.
//! Every retained AST object/string is charged conservatively (2x vector entry
//! size to cover capacity growth) against a 256 MiB AST budget. Counts, individual
//! names, statements and diagnostics also have explicit limits. Expected real
//! sky130/gf180 cell-file RSS is <64 MiB; this estimate excludes the caller's
//! retained documents. Parse libraries once, share immutable results across
//! corners/threads. No graph, path, cut, clause, or recursive traversal exists.
//! Small unit fixtures retain <8 KiB each; use two test/build workers and OS caps.
//!
//! Coordinates are micron decimals converted exactly to caller-selected i64 DBU.
//! Unsupported rule/electrical statements yield aggregated, counted diagnostics;
//! unsupported structural/geometry forms fail, rather than guessing their ends.
//! This reader supplies geometry, not a foundry DRC, antenna, or extraction model.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::mem::size_of;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefLimits {
    pub max_input_bytes: usize,
    pub max_tokens: usize,
    pub max_statement_tokens: usize,
    pub max_name_bytes: usize,
    pub max_layers: usize,
    pub max_sites: usize,
    pub max_macros: usize,
    pub max_pins: usize,
    pub max_ports: usize,
    pub max_vias: usize,
    pub max_shapes: usize,
    pub max_points: usize,
    pub max_points_per_polygon: usize,
    pub max_diagnostics: usize,
    pub max_ast_bytes: usize,
}
impl Default for LefLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 32 * 1024 * 1024,
            max_tokens: 4_000_000,
            max_statement_tokens: 8192,
            max_name_bytes: 2048,
            max_layers: 256,
            max_sites: 1024,
            max_macros: 20_000,
            max_pins: 200_000,
            max_ports: 250_000,
            max_vias: 20_000,
            max_shapes: 500_000,
            max_points: 2_000_000,
            max_points_per_polygon: 4096,
            max_diagnostics: 256,
            max_ast_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefDocument {
    pub version: Option<String>,
    pub dbu_per_micron: u32,
    pub source_dbu_per_micron: Option<u32>,
    pub manufacturing_grid: Option<i64>,
    pub bus_bit_chars: String,
    pub divider_char: String,
    pub layers: Vec<LefLayer>,
    pub sites: Vec<LefSite>,
    pub macros: Vec<LefMacro>,
    pub vias: Vec<LefVia>,
    pub diagnostics: Vec<LefDiagnostic>,
    /// Process-antenna data retained exactly for `check-antenna`; import keeps
    /// its "not modeled" diagnostics because the workspace does not use it.
    #[serde(default, skip_serializing_if = "LefAntenna::is_empty")]
    pub antenna: LefAntenna,
}
/// Exact LEF antenna data. Areas are integer DBU^2 at the parse DBU; ratios
/// and factors are integer millionths (`RATIO_SCALE`); PWL points are
/// (diffusion area DBU^2, ratio millionths). Values that are not exact at
/// this resolution are rejected rather than rounded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefAntenna {
    pub layers: Vec<LayerAntenna>,
    pub pins: Vec<PinAntenna>,
    /// Antenna statements the checker does not model, with their owner.
    pub unsupported: Vec<AntennaUnsupported>,
}
impl LefAntenna {
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.pins.is_empty() && self.unsupported.is_empty()
    }
}
pub const RATIO_SCALE: i64 = 1_000_000;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AntennaRatio {
    Value(i64),
    Pwl(Vec<[i64; 2]>),
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerAntenna {
    pub layer: String,
    pub model: Option<String>,
    /// LEF THICKNESS in DBU (side area = perimeter x thickness).
    pub thickness: Option<i64>,
    pub area_ratio: Option<i64>,
    pub diff_area_ratio: Option<AntennaRatio>,
    pub side_area_ratio: Option<i64>,
    pub diff_side_area_ratio: Option<AntennaRatio>,
    pub cum_area_ratio: Option<i64>,
    pub cum_diff_area_ratio: Option<AntennaRatio>,
    pub cum_side_area_ratio: Option<i64>,
    pub cum_diff_side_area_ratio: Option<AntennaRatio>,
    pub gate_plus_diff: Option<i64>,
    pub area_minus_diff: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinAntenna {
    pub macro_name: String,
    pub pin: String,
    /// Sum of ANTENNAGATEAREA values, DBU^2.
    pub gate_area: i64,
    /// Sum of ANTENNADIFFAREA values, DBU^2.
    pub diff_area: i64,
    /// Some value was restricted to one LAYER (kept, summed, reported).
    pub layer_qualified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AntennaUnsupported {
    pub owner: String,
    pub keyword: String,
    pub line: usize,
    /// Why it is not modeled (unsupported form or a value that is not exact).
    pub reason: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefLayer {
    pub name: String,
    pub kind: Option<String>,
    pub direction: Option<String>,
    pub pitch: Option<[i64; 2]>,
    pub offset: Option<[i64; 2]>,
    pub width: Option<i64>,
    /// A simple unconditional spacing only; complex rules remain diagnostics.
    pub spacing: Option<i64>,
    /// Unconditional `AREA` minimum, exact integer DBU^2 at the caller DBU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_area: Option<i64>,
    /// Unconditional `MINSTEP` length in DBU (option forms stay diagnostics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_step: Option<i64>,
    /// Cut-layer `ENCLOSURE BELOW a b` overhangs in DBU (either orientation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosure_below: Option<[i64; 2]>,
    /// Cut-layer `ENCLOSURE ABOVE a b` overhangs in DBU (either orientation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosure_above: Option<[i64; 2]>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefSite {
    pub name: String,
    pub class: Option<String>,
    pub size: [i64; 2],
    pub symmetry: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefMacro {
    pub name: String,
    pub class: Option<String>,
    pub origin: [i64; 2],
    pub size: [i64; 2],
    pub site: Option<String>,
    pub symmetry: Vec<String>,
    pub pins: Vec<LefPin>,
    pub obstructions: Vec<LefGeometry>,
    /// `FOREIGN name [x y [orient]]` bindings to an external (GDS) cell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign: Vec<LefForeign>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefForeign {
    pub name: String,
    /// Offset of the foreign cell origin, in DBU.
    pub origin: [i64; 2],
    pub orientation: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefPin {
    pub name: String,
    pub direction: Option<String>,
    pub use_: Option<String>,
    pub shape: Option<String>,
    pub ports: Vec<LefPort>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefPort {
    pub class: Option<String>,
    pub geometries: Vec<LefGeometry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefVia {
    pub name: String,
    pub is_default: bool,
    pub geometries: Vec<LefGeometry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LefGeometry {
    Rect {
        layer: String,
        rect: [i64; 4],
    },
    Polygon {
        layer: String,
        points: Vec<[i64; 2]>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LefDiagnostic {
    pub code: String,
    pub scope: String,
    pub keyword: String,
    pub first_line: usize,
    pub occurrences: usize,
    pub message: String,
}
#[derive(Debug, thiserror::Error)]
#[error("{code}: LEF line {line}: {message}")]
pub struct LefError {
    pub code: &'static str,
    pub line: usize,
    pub message: String,
}
fn error(code: &'static str, line: usize, message: impl Into<String>) -> LefError {
    LefError {
        code,
        line,
        message: message.into(),
    }
}
type Result<T> = std::result::Result<T, LefError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Word,
    Quoted,
    Semi,
}
#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    text: &'a str,
    line: usize,
    kind: Kind,
}
impl Token<'_> {
    fn is(self, text: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(text)
    }
}
struct Lexer<'a> {
    input: &'a str,
    offset: usize,
    line: usize,
    tokens: usize,
    max_tokens: usize,
}
impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>> {
        let bytes = self.input.as_bytes();
        loop {
            while self.offset < bytes.len() && bytes[self.offset].is_ascii_whitespace() {
                if bytes[self.offset] == b'\n' {
                    self.line += 1;
                }
                self.offset += 1;
            }
            if self.offset < bytes.len() && bytes[self.offset] == b'#' {
                while self.offset < bytes.len() && bytes[self.offset] != b'\n' {
                    self.offset += 1;
                }
            } else {
                break;
            }
        }
        if self.offset == bytes.len() {
            return Ok(None);
        }
        if self.tokens >= self.max_tokens {
            return Err(error("LEF_BUDGET", self.line, "max_tokens exceeded"));
        }
        self.tokens += 1;
        let line = self.line;
        let start = self.offset;
        if bytes[start] == b';' {
            self.offset += 1;
            return Ok(Some(Token {
                text: ";",
                line,
                kind: Kind::Semi,
            }));
        }
        if bytes[start] == b'"' {
            self.offset += 1;
            let value_start = self.offset;
            while self.offset < bytes.len() {
                let c = bytes[self.offset];
                if c == b'"' {
                    let text = &self.input[value_start..self.offset];
                    self.offset += 1;
                    return Ok(Some(Token {
                        text,
                        line,
                        kind: Kind::Quoted,
                    }));
                }
                if c == b'\n' {
                    self.line += 1;
                }
                if c == b'\\' {
                    self.offset += 1;
                    if self.offset == bytes.len() {
                        break;
                    }
                    if bytes[self.offset] == b'\n' {
                        self.line += 1;
                    }
                }
                self.offset += 1;
                if self.offset - value_start > 65536 {
                    return Err(error("LEF_BUDGET", line, "quoted token exceeds 64 KiB"));
                }
            }
            return Err(error("LEF_SYNTAX", line, "unterminated quoted string"));
        }
        while self.offset < bytes.len()
            && !bytes[self.offset].is_ascii_whitespace()
            && bytes[self.offset] != b';'
            && bytes[self.offset] != b'#'
        {
            self.offset += 1;
            if self.offset - start > 65536 {
                return Err(error("LEF_BUDGET", line, "token exceeds 64 KiB"));
            }
        }
        if self.offset == start {
            return Err(error("LEF_SYNTAX", line, "unexpected character"));
        }
        Ok(Some(Token {
            text: &self.input[start..self.offset],
            line,
            kind: Kind::Word,
        }))
    }
}

struct Parser<'a, 'b> {
    lexer: Lexer<'a>,
    lookahead: Option<Token<'a>>,
    limits: &'b LefLimits,
    doc: LefDocument,
    charged_bytes: usize,
    pending_pin_antenna: Vec<PinAntenna>,
    pins: usize,
    ports: usize,
    shapes: usize,
    points: usize,
}
impl<'a> Parser<'a, '_> {
    fn next_opt(&mut self) -> Result<Option<Token<'a>>> {
        if let Some(token) = self.lookahead.take() {
            Ok(Some(token))
        } else {
            self.lexer.next()
        }
    }
    fn next(&mut self) -> Result<Token<'a>> {
        self.next_opt()?
            .ok_or_else(|| error("LEF_SYNTAX", self.lexer.line, "unexpected end of input"))
    }
    fn peek(&mut self) -> Result<Option<Token<'a>>> {
        if self.lookahead.is_none() {
            self.lookahead = self.lexer.next()?;
        }
        Ok(self.lookahead)
    }
    fn expect(&mut self, word: &str) -> Result<Token<'a>> {
        let token = self.next()?;
        if !token.is(word) {
            return Err(error(
                "LEF_SYNTAX",
                token.line,
                format!("expected {word}, got {}", token.text),
            ));
        }
        Ok(token)
    }
    fn end_named(&mut self, name: &str) -> Result<()> {
        let token = self.next()?;
        if token.kind != Kind::Word || token.text != name {
            return Err(error(
                "LEF_SYNTAX",
                token.line,
                format!("END name does not match {name}"),
            ));
        }
        Ok(())
    }
    fn charge(&mut self, bytes: usize, line: usize) -> Result<()> {
        self.charged_bytes = self
            .charged_bytes
            .checked_add(bytes)
            .ok_or_else(|| error("LEF_BUDGET", line, "AST size overflow"))?;
        if self.charged_bytes > self.limits.max_ast_bytes {
            return Err(error("LEF_BUDGET", line, "max_ast_bytes exceeded"));
        }
        Ok(())
    }
    fn owned(&mut self, token: Token<'a>) -> Result<String> {
        if token.text.len() > self.limits.max_name_bytes {
            return Err(error("LEF_BUDGET", token.line, "max_name_bytes exceeded"));
        }
        if token.text.contains('\\') || !token.text.is_ascii() {
            return Err(error(
                "LEF_UNSUPPORTED",
                token.line,
                "escaped/non-ASCII identifier or retained value",
            ));
        }
        self.charge(token.text.len(), token.line)?;
        Ok(token.text.to_owned())
    }
    fn name(&mut self) -> Result<Token<'a>> {
        let token = self.next()?;
        if token.kind != Kind::Word || token.text.is_empty() {
            return Err(error(
                "LEF_SYNTAX",
                token.line,
                "expected unquoted identifier",
            ));
        }
        if token.text.len() > self.limits.max_name_bytes || token.text.contains('\\') {
            return Err(error(
                "LEF_UNSUPPORTED",
                token.line,
                "identifier exceeds bound or uses escaping",
            ));
        }
        Ok(token)
    }
    fn statement(&mut self) -> Result<Vec<Token<'a>>> {
        let mut result = Vec::new();
        loop {
            let token = self.next()?;
            if token.kind == Kind::Semi {
                return Ok(result);
            }
            if token.is("END") {
                return Err(error(
                    "LEF_SYNTAX",
                    token.line,
                    "missing semicolon before END",
                ));
            }
            if result.len() >= self.limits.max_statement_tokens {
                return Err(error(
                    "LEF_BUDGET",
                    token.line,
                    "max_statement_tokens exceeded",
                ));
            }
            result.push(token);
        }
    }
    fn arity(&self, tokens: &[Token<'a>], expected: usize, line: usize) -> Result<()> {
        if tokens.len() != expected {
            return Err(error(
                "LEF_UNSUPPORTED",
                line,
                format!("expected {expected} statement values, got {}", tokens.len()),
            ));
        }
        Ok(())
    }
    fn coord(&self, token: Token<'a>) -> Result<i64> {
        decimal_dbu(token.text, self.doc.dbu_per_micron, token.line)
    }
    /// Square-micron value converted exactly to DBU^2 (never rounded).
    fn area(&self, token: Token<'a>) -> Result<i64> {
        let dbu = i128::from(self.doc.dbu_per_micron);
        decimal_scaled(token.text, dbu * dbu, token.line)
    }
    fn pair(&self, values: &[Token<'a>], line: usize) -> Result<[i64; 2]> {
        self.arity(values, 2, line)?;
        Ok([self.coord(values[0])?, self.coord(values[1])?])
    }
    fn size(&self, values: &[Token<'a>], line: usize) -> Result<[i64; 2]> {
        self.arity(values, 3, line)?;
        if !values[1].is("BY") {
            return Err(error("LEF_SYNTAX", line, "SIZE requires BY"));
        }
        let size = [self.coord(values[0])?, self.coord(values[2])?];
        if size[0] <= 0 || size[1] <= 0 {
            return Err(error("LEF_GEOMETRY", line, "SIZE must be positive"));
        }
        Ok(size)
    }
    fn text_values(&mut self, values: &[Token<'a>], line: usize) -> Result<String> {
        if values.is_empty() {
            return Err(error("LEF_SYNTAX", line, "missing statement value"));
        }
        let mut length = values.len() - 1;
        for value in values {
            if value.kind != Kind::Word || !value.text.is_ascii() || value.text.contains('\\') {
                return Err(error(
                    "LEF_SYNTAX",
                    value.line,
                    "expected unescaped ASCII word",
                ));
            }
            length = length
                .checked_add(value.text.len())
                .ok_or_else(|| error("LEF_BUDGET", line, "retained text length overflow"))?;
        }
        if length > self.limits.max_name_bytes {
            return Err(error(
                "LEF_BUDGET",
                line,
                "retained attribute exceeds max_name_bytes",
            ));
        }
        self.charge(length, line)?;
        let mut out = String::with_capacity(length);
        for (i, value) in values.iter().enumerate() {
            if i != 0 {
                out.push(' ');
            }
            out.extend(value.text.chars().map(|c| c.to_ascii_uppercase()));
        }
        Ok(out)
    }
    fn symmetry(&mut self, values: &[Token<'a>], line: usize) -> Result<Vec<String>> {
        if values.is_empty() || values.len() > 3 {
            return Err(error(
                "LEF_SYNTAX",
                line,
                "SYMMETRY requires X, Y and/or R90",
            ));
        }
        let mut result = Vec::new();
        for value in values {
            if !value.is("X") && !value.is("Y") && !value.is("R90") {
                return Err(error("LEF_UNSUPPORTED", value.line, "unknown symmetry"));
            }
            let text = self.text_values(&[*value], value.line)?;
            if result.contains(&text) {
                return Err(error("LEF_SYNTAX", line, "duplicate symmetry"));
            }
            self.charge(2 * size_of::<String>(), line)?;
            result.push(text);
        }
        Ok(result)
    }
    fn warn(&mut self, key: Token<'a>, scope: &str, message: &str) -> Result<()> {
        if let Some(diagnostic) = self
            .doc
            .diagnostics
            .iter_mut()
            .find(|d| d.scope == scope && d.keyword.eq_ignore_ascii_case(key.text))
        {
            diagnostic.occurrences = diagnostic
                .occurrences
                .checked_add(1)
                .ok_or_else(|| error("LEF_BUDGET", key.line, "diagnostic count overflow"))?;
            return Ok(());
        }
        if self.doc.diagnostics.len() >= self.limits.max_diagnostics {
            return Err(error("LEF_BUDGET", key.line, "max_diagnostics exceeded"));
        }
        self.charge(
            2 * size_of::<LefDiagnostic>() + key.text.len() + scope.len() + message.len() + 32,
            key.line,
        )?;
        self.doc.diagnostics.push(LefDiagnostic {
            code: "LEF_SEMANTICS_UNMODELED".into(),
            scope: scope.into(),
            keyword: key.text.to_ascii_uppercase(),
            first_line: key.line,
            occurrences: 1,
            message: message.into(),
        });
        Ok(())
    }
    fn unsupported(&self, token: Token<'a>, scope: &str) -> LefError {
        error(
            "LEF_UNSUPPORTED",
            token.line,
            format!(
                "unsupported structural/geometry construct {} in {scope}",
                token.text
            ),
        )
    }
    fn site(&mut self) -> Result<LefSite> {
        let name = self.name()?;
        let mut class = None;
        let mut size = None;
        let mut symmetry = Vec::new();
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.end_named(name.text)?;
                break;
            }
            let values = self.statement()?;
            match key.text.to_ascii_uppercase().as_str() {
                "CLASS" => {
                    if class.is_some() {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate CLASS"));
                    }
                    class = Some(self.text_values(&values, key.line)?);
                }
                "SIZE" => {
                    if size.is_some() {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate SIZE"));
                    }
                    size = Some(self.size(&values, key.line)?);
                }
                "SYMMETRY" => {
                    if !symmetry.is_empty() {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate SYMMETRY"));
                    }
                    symmetry = self.symmetry(&values, key.line)?;
                }
                _ => return Err(self.unsupported(key, "SITE")),
            }
        }
        Ok(LefSite {
            name: self.owned(name)?,
            class,
            size: size.ok_or_else(|| error("LEF_SYNTAX", name.line, "SITE missing SIZE"))?,
            symmetry,
        })
    }
    fn layer(&mut self) -> Result<LefLayer> {
        let name = self.name()?;
        let mut antenna = LayerAntenna {
            layer: name.text.to_string(),
            ..LayerAntenna::default()
        };
        let mut layer = LefLayer {
            name: self.owned(name)?,
            kind: None,
            direction: None,
            pitch: None,
            offset: None,
            width: None,
            spacing: None,
            min_area: None,
            min_step: None,
            enclosure_below: None,
            enclosure_above: None,
        };
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.end_named(name.text)?;
                if antenna
                    != (LayerAntenna {
                        layer: layer.name.clone(),
                        ..LayerAntenna::default()
                    })
                {
                    self.charge(2 * size_of::<LayerAntenna>() + layer.name.len(), key.line)?;
                    self.doc.antenna.layers.push(antenna);
                }
                return Ok(layer);
            }
            let values = self.statement()?;
            if key.is("THICKNESS") && values.len() == 1 && antenna.thickness.is_none() {
                let t = self.coord(values[0])?;
                if t <= 0 {
                    return Err(error(
                        "LEF_GEOMETRY",
                        key.line,
                        "THICKNESS must be positive",
                    ));
                }
                antenna.thickness = Some(t);
            }
            if key.text.to_ascii_uppercase().starts_with("ANTENNA") {
                let outcome = self.layer_antenna(&mut antenna, key, &values);
                let owner = antenna.layer.clone();
                self.antenna_or_unsupported(&owner, key, outcome)?;
            }
            match key.text.to_ascii_uppercase().as_str() {
                "TYPE" => {
                    if layer.kind.is_some() { return Err(error("LEF_SYNTAX", key.line, "duplicate TYPE")); }
                    self.arity(&values, 1, key.line)?;
                    let kind = self.text_values(&values, key.line)?;
                    if !matches!(kind.as_str(), "ROUTING" | "CUT" | "MASTERSLICE" | "OVERLAP" | "IMPLANT") { return Err(self.unsupported(key,"LAYER TYPE")); }
                    layer.kind = Some(kind);
                }
                "DIRECTION" => { if layer.direction.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate layer DIRECTION")); } self.arity(&values, 1, key.line)?; if !values[0].is("HORIZONTAL") && !values[0].is("VERTICAL") { return Err(self.unsupported(values[0], "LAYER DIRECTION")); } layer.direction = Some(self.text_values(&values, key.line)?); }
                "PITCH" | "OFFSET" => {
                    if (key.is("PITCH") && layer.pitch.is_some()) || (key.is("OFFSET") && layer.offset.is_some()) { return Err(error("LEF_SYNTAX",key.line,"duplicate layer PITCH/OFFSET")); }
                    let pair = match values.len() { 1 => { let v = self.coord(values[0])?; [v,v] }, 2 => self.pair(&values, key.line)?, _ => return Err(self.unsupported(key, "LAYER")) };
                    if key.is("PITCH") { if pair.iter().any(|v| *v <= 0) { return Err(error("LEF_GEOMETRY", key.line, "PITCH must be positive")); } layer.pitch = Some(pair); } else { layer.offset = Some(pair); }
                }
                "WIDTH" => { if layer.width.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate layer WIDTH")); } self.arity(&values, 1, key.line)?; let width = self.coord(values[0])?; if width <= 0 { return Err(error("LEF_GEOMETRY", key.line, "WIDTH must be positive")); } layer.width = Some(width); }
                "SPACING" if values.len() == 1 && layer.spacing.is_none() => { let value = self.coord(values[0])?; if value < 0 { return Err(error("LEF_GEOMETRY", key.line, "SPACING must not be negative")); } layer.spacing = Some(value); }
                "AREA" if values.len() == 1 && layer.min_area.is_none() => {
                    let area = self.area(values[0])?;
                    if area <= 0 { return Err(error("LEF_GEOMETRY", key.line, "AREA must be positive")); }
                    layer.min_area = Some(area);
                }
                "MINSTEP" if values.len() == 1 && layer.min_step.is_none() => {
                    let step = self.coord(values[0])?;
                    if step <= 0 { return Err(error("LEF_GEOMETRY", key.line, "MINSTEP must be positive")); }
                    layer.min_step = Some(step);
                    self.warn(key, "layer", "MINSTEP retained; router enforces it only for pin-access landing pads")?;
                }
                "ENCLOSURE" if values.len() == 3 && (values[0].is("BELOW") || values[0].is("ABOVE")) && (if values[0].is("BELOW") { layer.enclosure_below.is_none() } else { layer.enclosure_above.is_none() }) => {
                    let pair = [self.coord(values[1])?, self.coord(values[2])?];
                    if pair.iter().any(|v| *v < 0) { return Err(error("LEF_GEOMETRY", key.line, "ENCLOSURE overhang must not be negative")); }
                    if values[0].is("BELOW") { layer.enclosure_below = Some(pair); } else { layer.enclosure_above = Some(pair); }
                }
                "SPACING" | "SPACINGTABLE" | "MINWIDTH" | "AREA" | "THICKNESS" | "MINENCLOSEDAREA" | "EDGECAPACITANCE" | "CAPACITANCE" | "RESISTANCE" | "ENCLOSURE" | "PREFERENCLOSURE" | "DCCURRENTDENSITY" | "ACCURRENTDENSITY" | "MINIMUMDENSITY" | "MAXIMUMDENSITY" | "DENSITYCHECKWINDOW" | "DENSITYCHECKSTEP" | "ARRAYSPACING" | "PROPERTY" | "MAXWIDTH" | "MINSTEP" | "MINIMUMCUT" | "MINSIZE" | "PROTRUSIONWIDTH" | "HEIGHT" | "WIREEXTENSION" | "SLOTWIREWIDTH" | "SLOTWIRELENGTH" | "SLOTWIDTH" | "SLOTLENGTH" | "MAXADJACENTSLOTSPACING" | "MAXCOAXIALSLOTSPACING" | "MAXEDGESLOTSPACING" | "SPLITWIREWIDTH" => self.warn(key, "layer", "Technology rule/electrical value is not modeled; DRC/RC/antenna remain unchecked")?,
                _ if key.text.to_ascii_uppercase().starts_with("ANTENNA") => self.warn(key, "layer", "Antenna rule is not modeled; antenna status remains unchecked")?,
                _ => return Err(self.unsupported(key, "LAYER")),
            }
        }
    }
    /// Ratio or factor as exact millionths.
    fn ratio(&self, token: Token<'a>) -> Result<i64> {
        let v = decimal_scaled(token.text, i128::from(RATIO_SCALE), token.line)?;
        if v < 0 {
            return Err(error(
                "LEF_SYNTAX",
                token.line,
                "antenna ratio must not be negative",
            ));
        }
        Ok(v)
    }
    /// `value` or `PWL ( ( d1 r1 ) ( d2 r2 ) ... )` with increasing diffusion areas.
    fn antenna_ratio(&self, values: &[Token<'a>], line: usize) -> Result<AntennaRatio> {
        if values.len() == 1 {
            return Ok(AntennaRatio::Value(self.ratio(values[0])?));
        }
        let bad = || error("LEF_SYNTAX", line, "malformed antenna PWL");
        if values.len() < 7
            || !values[0].is("PWL")
            || values[1].text != "("
            || values[values.len() - 1].text != ")"
        {
            return Err(bad());
        }
        let body = &values[2..values.len() - 1];
        if !body.len().is_multiple_of(4) {
            return Err(bad());
        }
        let mut points: Vec<[i64; 2]> = Vec::with_capacity(body.len() / 4);
        for q in body.chunks_exact(4) {
            if q[0].text != "(" || q[3].text != ")" {
                return Err(bad());
            }
            let d = self.area(q[1])?;
            let r = self.ratio(q[2])?;
            if d < 0 || points.last().is_some_and(|p| p[0] >= d) {
                return Err(error(
                    "LEF_SYNTAX",
                    line,
                    "PWL diffusion areas must increase",
                ));
            }
            points.push([d, r]);
        }
        if points.len() > 64 {
            return Err(error(
                "LEF_BUDGET",
                line,
                "antenna PWL longer than 64 points",
            ));
        }
        Ok(AntennaRatio::Pwl(points))
    }
    fn antenna_unsupported(&mut self, owner: &str, key: Token<'a>, reason: &str) -> Result<()> {
        if self.doc.antenna.unsupported.len() >= 4096 {
            return Err(error("LEF_BUDGET", key.line, "antenna diagnostic budget"));
        }
        self.charge(
            2 * size_of::<AntennaUnsupported>() + owner.len() + key.text.len() + reason.len(),
            key.line,
        )?;
        self.doc.antenna.unsupported.push(AntennaUnsupported {
            owner: owner.to_string(),
            keyword: key.text.to_ascii_uppercase(),
            line: key.line,
            reason: reason.to_string(),
        });
        Ok(())
    }
    /// Antenna data never fails the LEF parse (import does not use it): a
    /// malformed or inexact value is recorded as unsupported instead.
    fn antenna_or_unsupported(
        &mut self,
        owner: &str,
        key: Token<'a>,
        outcome: Result<()>,
    ) -> Result<()> {
        match outcome {
            Ok(()) => Ok(()),
            Err(e) if e.code == "LEF_BUDGET" => Err(e),
            Err(e) => self.antenna_unsupported(owner, key, &e.message),
        }
    }
    fn layer_antenna(
        &mut self,
        a: &mut LayerAntenna,
        key: Token<'a>,
        values: &[Token<'a>],
    ) -> Result<()> {
        let k = key.text.to_ascii_uppercase();
        if k == "ANTENNAMODEL" {
            self.arity(values, 1, key.line)?;
            a.model = Some(values[0].text.to_ascii_uppercase());
            return Ok(());
        }
        // Values after a non-OXIDE1 model belong to another oxide: not modeled.
        if a.model.as_deref().is_some_and(|m| m != "OXIDE1") {
            let owner = a.layer.clone();
            return self.antenna_unsupported(&owner, key, "rule of a non-OXIDE1 antenna model");
        }
        let dup = || error("LEF_SYNTAX", key.line, format!("duplicate {k}"));
        let single = |p: &Self, slot: &mut Option<i64>| -> Result<()> {
            if slot.is_some() {
                return Err(dup());
            }
            p.arity(values, 1, key.line)?;
            *slot = Some(p.ratio(values[0])?);
            Ok(())
        };
        let ratio = |p: &Self, slot: &mut Option<AntennaRatio>| -> Result<()> {
            if slot.is_some() {
                return Err(dup());
            }
            *slot = Some(p.antenna_ratio(values, key.line)?);
            Ok(())
        };
        match k.as_str() {
            "ANTENNAAREARATIO" => single(self, &mut a.area_ratio),
            "ANTENNASIDEAREARATIO" => single(self, &mut a.side_area_ratio),
            "ANTENNACUMAREARATIO" => single(self, &mut a.cum_area_ratio),
            "ANTENNACUMSIDEAREARATIO" => single(self, &mut a.cum_side_area_ratio),
            "ANTENNAGATEPLUSDIFF" => single(self, &mut a.gate_plus_diff),
            "ANTENNAAREAMINUSDIFF" => single(self, &mut a.area_minus_diff),
            "ANTENNADIFFAREARATIO" => ratio(self, &mut a.diff_area_ratio),
            "ANTENNADIFFSIDEAREARATIO" => ratio(self, &mut a.diff_side_area_ratio),
            "ANTENNACUMDIFFAREARATIO" => ratio(self, &mut a.cum_diff_area_ratio),
            "ANTENNACUMDIFFSIDEAREARATIO" => ratio(self, &mut a.cum_diff_side_area_ratio),
            _ => {
                let owner = a.layer.clone();
                self.antenna_unsupported(&owner, key, "antenna statement not modeled")
            }
        }
    }
    fn geometry(
        &mut self,
        key: Token<'a>,
        values: &[Token<'a>],
        layer: &str,
    ) -> Result<LefGeometry> {
        if self.shapes >= self.limits.max_shapes {
            return Err(error("LEF_BUDGET", key.line, "max_shapes exceeded"));
        }
        if layer.is_empty() {
            return Err(error(
                "LEF_SYNTAX",
                key.line,
                "geometry requires preceding LAYER",
            ));
        }
        self.shapes += 1;
        self.charge(2 * size_of::<LefGeometry>() + layer.len(), key.line)?;
        if key.is("RECT") {
            self.arity(values, 4, key.line)?;
            let rect = [
                self.coord(values[0])?,
                self.coord(values[1])?,
                self.coord(values[2])?,
                self.coord(values[3])?,
            ];
            if rect[0] >= rect[2] || rect[1] >= rect[3] {
                return Err(error(
                    "LEF_GEOMETRY",
                    key.line,
                    "RECT must have positive ordered extents",
                ));
            }
            Ok(LefGeometry::Rect {
                layer: layer.into(),
                rect,
            })
        } else if key.is("POLYGON") {
            if values.len() < 8 || !values.len().is_multiple_of(2) {
                return Err(error(
                    "LEF_GEOMETRY",
                    key.line,
                    "POLYGON requires at least four coordinate pairs",
                ));
            }
            let count = values.len() / 2;
            if count > self.limits.max_points_per_polygon
                || count > self.limits.max_points.saturating_sub(self.points)
            {
                return Err(error(
                    "LEF_BUDGET",
                    key.line,
                    "polygon point budget exceeded",
                ));
            }
            self.points += count;
            self.charge(2 * count * size_of::<[i64; 2]>(), key.line)?;
            let mut points = Vec::with_capacity(count);
            for pair in values.chunks_exact(2) {
                points.push(self.pair(pair, key.line)?);
            }
            for i in 0..points.len() {
                let a = points[i];
                let b = points[(i + 1) % points.len()];
                if a[0] != b[0] && a[1] != b[1] {
                    return Err(error(
                        "LEF_GEOMETRY",
                        key.line,
                        "POLYGON edge is not orthogonal",
                    ));
                }
            }
            let min_x = points.iter().map(|p| p[0]).min().unwrap();
            let max_x = points.iter().map(|p| p[0]).max().unwrap();
            let min_y = points.iter().map(|p| p[1]).min().unwrap();
            let max_y = points.iter().map(|p| p[1]).max().unwrap();
            if min_x == max_x || min_y == max_y {
                return Err(error("LEF_GEOMETRY", key.line, "degenerate POLYGON"));
            }
            self.warn(key, "geometry", "Polygon coordinates/orthogonal edges retained; self-intersection and DRC not verified")?;
            Ok(LefGeometry::Polygon {
                layer: layer.into(),
                points,
            })
        } else {
            Err(self.unsupported(key, "geometry"))
        }
    }
    fn geometry_block(&mut self, end_name: Option<&str>, scope: &str) -> Result<LefPort> {
        let mut current_layer = String::new();
        let mut result = LefPort {
            class: None,
            geometries: Vec::new(),
        };
        loop {
            let key = self.next()?;
            if key.is("END") {
                if let Some(name) = end_name {
                    self.end_named(name)?;
                }
                return Ok(result);
            }
            let values = self.statement()?;
            match key.text.to_ascii_uppercase().as_str() {
                "CLASS" if scope == "port" => {
                    if result.class.is_some() {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate port CLASS"));
                    }
                    result.class = Some(self.text_values(&values, key.line)?);
                }
                "LAYER" => {
                    if values.is_empty() {
                        return Err(error("LEF_SYNTAX", key.line, "LAYER missing name"));
                    }
                    self.arity(&values, 1, key.line)?;
                    current_layer = self.owned(values[0])?;
                }
                "RECT" | "POLYGON" => {
                    result
                        .geometries
                        .push(self.geometry(key, &values, &current_layer)?)
                }
                "RESISTANCE" if scope == "via" => self.warn(
                    key,
                    "via",
                    "Via resistance is not modeled; extraction remains unchecked",
                )?,
                _ => return Err(self.unsupported(key, scope)),
            }
        }
    }
    fn pin(&mut self) -> Result<LefPin> {
        let name = self.name()?;
        let mut antenna = PinAntenna {
            macro_name: String::new(),
            pin: name.text.to_string(),
            gate_area: 0,
            diff_area: 0,
            layer_qualified: false,
        };
        let mut antenna_seen = false;
        let mut pin = LefPin {
            name: self.owned(name)?,
            direction: None,
            use_: None,
            shape: None,
            ports: Vec::new(),
        };
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.end_named(name.text)?;
                if antenna_seen {
                    self.charge(2 * size_of::<PinAntenna>() + antenna.pin.len(), key.line)?;
                    self.pending_pin_antenna.push(antenna);
                }
                return Ok(pin);
            }
            if key.is("PORT") {
                if self.ports >= self.limits.max_ports {
                    return Err(error("LEF_BUDGET", key.line, "max_ports exceeded"));
                }
                self.ports += 1;
                self.charge(2 * size_of::<LefPort>(), key.line)?;
                pin.ports.push(self.geometry_block(None, "port")?);
                continue;
            }
            let values = self.statement()?;
            let upper = key.text.to_ascii_uppercase();
            if upper == "ANTENNAGATEAREA" || upper == "ANTENNADIFFAREA" {
                let qualified = values.len() == 3 && values[1].is("LAYER");
                let parsed = if values.len() != 1 && !qualified {
                    Err(error(
                        "LEF_SYNTAX",
                        key.line,
                        format!("{upper} value [LAYER name]"),
                    ))
                } else {
                    self.area(values[0]).and_then(|v| {
                        if v < 0 {
                            Err(error(
                                "LEF_SYNTAX",
                                key.line,
                                "antenna area must not be negative",
                            ))
                        } else {
                            Ok(v)
                        }
                    })
                };
                match parsed {
                    Ok(v) => {
                        let slot = if upper == "ANTENNAGATEAREA" {
                            &mut antenna.gate_area
                        } else {
                            &mut antenna.diff_area
                        };
                        *slot = slot.checked_add(v).ok_or_else(|| {
                            error("LEF_BUDGET", key.line, "antenna area overflow")
                        })?;
                        antenna.layer_qualified |= qualified;
                        antenna_seen = true;
                    }
                    Err(e) => {
                        let owner = format!("pin {}", name.text);
                        self.antenna_or_unsupported(&owner, key, Err(e))?;
                    }
                }
            } else if upper.starts_with("ANTENNA") {
                let owner = format!("pin {}", name.text);
                self.antenna_unsupported(&owner, key, "pin antenna statement not modeled")?;
            }
            match key.text.to_ascii_uppercase().as_str() {
                "DIRECTION" => { if pin.direction.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate pin DIRECTION")); } let direction=self.text_values(&values,key.line)?; if !matches!(direction.as_str(),"INPUT"|"OUTPUT"|"OUTPUT TRISTATE"|"INOUT"|"FEEDTHRU") { return Err(self.unsupported(key,"pin DIRECTION")); } pin.direction=Some(direction); }
                "USE" => {
                    if pin.use_.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate pin USE")); }
                    self.arity(&values,1,key.line)?;
                    let usage=self.text_values(&values,key.line)?;
                    if !matches!(usage.as_str(),"SIGNAL"|"ANALOG"|"POWER"|"GROUND"|"CLOCK"|"TIEOFF"|"SCAN"|"RESET") { return Err(self.unsupported(key,"pin USE")); }
                    pin.use_=Some(usage);
                }
                "SHAPE" => {
                    if pin.shape.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate pin SHAPE")); }
                    self.arity(&values,1,key.line)?;
                    let shape=self.text_values(&values,key.line)?;
                    if !matches!(shape.as_str(),"ABUTMENT"|"RING"|"FEEDTHRU") { return Err(self.unsupported(key,"pin SHAPE")); }
                    pin.shape=Some(shape);
                }
                "PROPERTY"|"MUSTJOIN"|"NETEXPR"|"SUPPLYSENSITIVITY"|"GROUNDSENSITIVITY"|"TAPERRULE"|"IV_TABLES"|"LEQ"|"FOREIGN"|"POWER"|"LEAKAGE"|"RISETHRESH"|"FALLTHRESH"|"RISESATCUR"|"FALLSATCUR"|"VLO"|"VHI"|"TIEOFFR"|"OUTPUTNOISEMARGIN"|"OUTPUTRESISTANCE"|"INPUTNOISEMARGIN"|"CAPACITANCE"|"RESISTANCE"|"PULLDOWNRES"|"CURRENTSOURCE" => self.warn(key,"pin","Pin electrical/connectivity rule is not modeled; only declared geometry and direction are retained")?,
                _ if key.text.to_ascii_uppercase().starts_with("ANTENNA") => self.warn(key,"pin","Antenna value is not modeled; antenna status remains unchecked")?,
                _ => return Err(self.unsupported(key,"PIN")),
            }
        }
    }
    fn macro_(&mut self) -> Result<LefMacro> {
        let name = self.name()?;
        let mut result = LefMacro {
            name: self.owned(name)?,
            class: None,
            origin: [0, 0],
            size: [0, 0],
            site: None,
            symmetry: Vec::new(),
            pins: Vec::new(),
            obstructions: Vec::new(),
            foreign: Vec::new(),
        };
        let mut size_seen = false;
        let mut origin_seen = false;
        let mut obs_seen = false;
        let mut pin_names = HashSet::new();
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.end_named(name.text)?;
                break;
            }
            if key.is("PIN") {
                if self.pins >= self.limits.max_pins {
                    return Err(error("LEF_BUDGET", key.line, "max_pins exceeded"));
                }
                self.pins += 1;
                self.charge(2 * size_of::<LefPin>() + 64, key.line)?;
                let pin = self.pin()?;
                self.charge(pin.name.len(), key.line)?;
                if !pin_names.insert(pin.name.clone()) {
                    return Err(error("LEF_SYNTAX", key.line, "duplicate pin name"));
                }
                result.pins.push(pin);
                continue;
            }
            if key.is("OBS") {
                if obs_seen {
                    return Err(error("LEF_SYNTAX", key.line, "duplicate OBS block"));
                }
                obs_seen = true;
                result.obstructions = self.geometry_block(None, "OBS")?.geometries;
                continue;
            }
            let values = self.statement()?;
            match key.text.to_ascii_uppercase().as_str() {
                "CLASS" => { if result.class.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate macro CLASS")); } result.class=Some(self.text_values(&values,key.line)?); }
                "ORIGIN" => { if origin_seen { return Err(error("LEF_SYNTAX",key.line,"duplicate ORIGIN")); } origin_seen=true; result.origin=self.pair(&values,key.line)?; }
                "SIZE" => { if size_seen { return Err(error("LEF_SYNTAX",key.line,"duplicate SIZE")); } size_seen=true; result.size=self.size(&values,key.line)?; }
                "SITE" => { self.arity(&values,1,key.line)?; if result.site.is_some() { return Err(error("LEF_UNSUPPORTED",key.line,"multiple macro sites")); } result.site=Some(self.owned(values[0])?); }
                "SYMMETRY" => { if !result.symmetry.is_empty() { return Err(error("LEF_SYNTAX",key.line,"duplicate SYMMETRY")); } result.symmetry=self.symmetry(&values,key.line)?; }
                "FOREIGN" => { let f = self.foreign(&values, key.line)?; if result.foreign.len() >= 16 { return Err(error("LEF_BUDGET", key.line, "more than 16 FOREIGN statements")); } result.foreign.push(f); self.warn(key,"macro","FOREIGN is retained for GDS cell binding (checked by export-gds); import uses LEF geometry only")?; }
                "PROPERTY"|"EEQ"|"LEQ"|"SOURCE"|"POWER"|"FIXEDMASK" => self.warn(key,"macro","Macro annotation/foreign mapping is not modeled; use only retained local geometry")?,
                _ => return Err(self.unsupported(key,"MACRO")),
            }
        }
        if !size_seen {
            return Err(error("LEF_SYNTAX", name.line, "MACRO missing SIZE"));
        }
        for mut a in std::mem::take(&mut self.pending_pin_antenna) {
            self.charge(result.name.len(), name.line)?;
            a.macro_name.clone_from(&result.name);
            self.doc.antenna.pins.push(a);
        }
        for u in self.doc.antenna.unsupported.iter_mut() {
            if u.owner.starts_with("pin ") && !u.owner.contains('/') {
                u.owner = format!("{}/{}", result.name, &u.owner[4..]);
            }
        }
        Ok(result)
    }
    fn foreign(&mut self, values: &[Token<'a>], line: usize) -> Result<LefForeign> {
        if !matches!(values.len(), 1 | 3 | 4) || values[0].kind != Kind::Word {
            return Err(error(
                "LEF_SYNTAX",
                line,
                "FOREIGN requires name [x y [orientation]]",
            ));
        }
        let name = self.owned(values[0])?;
        let origin = if values.len() >= 3 {
            [self.coord(values[1])?, self.coord(values[2])?]
        } else {
            [0, 0]
        };
        let orientation = match values.get(3) {
            Some(token) => {
                let text = token.text.to_ascii_uppercase();
                if !matches!(
                    text.as_str(),
                    "N" | "S" | "E" | "W" | "FN" | "FS" | "FE" | "FW"
                ) {
                    return Err(error("LEF_SYNTAX", line, "invalid FOREIGN orientation"));
                }
                self.charge(text.len(), line)?;
                Some(text)
            }
            None => None,
        };
        self.charge(2 * size_of::<LefForeign>(), line)?;
        Ok(LefForeign {
            name,
            origin,
            orientation,
        })
    }
    fn units(&mut self) -> Result<()> {
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.expect("UNITS")?;
                return Ok(());
            }
            let values = self.statement()?;
            if key.is("DATABASE") {
                self.arity(&values, 2, key.line)?;
                if !values[0].is("MICRONS") {
                    return Err(self.unsupported(values[0], "UNITS"));
                }
                let dbu = values[1].text.parse::<u32>().map_err(|_| {
                    error("LEF_SYNTAX", key.line, "invalid source DATABASE MICRONS")
                })?;
                if dbu == 0 || self.doc.source_dbu_per_micron.is_some() {
                    return Err(error(
                        "LEF_SYNTAX",
                        key.line,
                        "zero or duplicate DATABASE MICRONS",
                    ));
                }
                self.doc.source_dbu_per_micron = Some(dbu);
            } else if matches!(
                key.text.to_ascii_uppercase().as_str(),
                "TIME"
                    | "CAPACITANCE"
                    | "RESISTANCE"
                    | "POWER"
                    | "CURRENT"
                    | "VOLTAGE"
                    | "FREQUENCY"
            ) {
                self.warn(key, "units", "Non-geometric unit system is not modeled")?;
            } else {
                return Err(self.unsupported(key, "UNITS"));
            }
        }
    }
    fn property_definitions(&mut self) -> Result<()> {
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.expect("PROPERTYDEFINITIONS")?;
                return Ok(());
            }
            if !matches!(
                key.text.to_ascii_uppercase().as_str(),
                "LIBRARY"
                    | "COMPONENTPIN"
                    | "PIN"
                    | "MACRO"
                    | "VIA"
                    | "VIARULE"
                    | "LAYER"
                    | "NONDEFAULTRULE"
            ) {
                return Err(self.unsupported(key, "PROPERTYDEFINITIONS"));
            }
            let _values = self.statement()?;
            self.warn(
                key,
                "property_definitions",
                "Property schema is not interpreted; property uses produce separate diagnostics",
            )?;
        }
    }
    fn via_rule(&mut self, key: Token<'a>) -> Result<()> {
        let name = self.name()?;
        if self.peek()?.is_some_and(|t| t.is("GENERATE")) {
            self.next()?;
        }
        if self.peek()?.is_some_and(|t| t.is("DEFAULT")) {
            self.next()?;
        }
        self.warn(
            key,
            "via_rule",
            "Via generation rule is not modeled; routing/via DRC remain unchecked",
        )?;
        loop {
            let key = self.next()?;
            if key.is("END") {
                self.end_named(name.text)?;
                return Ok(());
            }
            if !matches!(
                key.text.to_ascii_uppercase().as_str(),
                "LAYER"
                    | "ENCLOSURE"
                    | "WIDTH"
                    | "SPACING"
                    | "RECT"
                    | "DIRECTION"
                    | "OVERHANG"
                    | "METALOVERHANG"
                    | "RESISTANCE"
                    | "VIA"
                    | "PROPERTY"
            ) {
                return Err(self.unsupported(key, "VIARULE"));
            }
            self.statement()?;
        }
    }
    fn parse(mut self) -> Result<LefDocument> {
        let mut layer_names = HashSet::new();
        let mut site_names = HashSet::new();
        let mut macro_names = HashSet::new();
        let mut via_names = HashSet::new();
        let mut saw_content = false;
        while let Some(key) = self.next_opt()? {
            saw_content = true;
            match key.text.to_ascii_uppercase().as_str() {
                "UNITS" => self.units()?,
                "PROPERTYDEFINITIONS" => self.property_definitions()?,
                "LAYER" => {
                    if self.doc.layers.len() >= self.limits.max_layers {
                        return Err(error("LEF_BUDGET", key.line, "max_layers exceeded"));
                    }
                    self.charge(2 * size_of::<LefLayer>() + 64, key.line)?;
                    let item = self.layer()?;
                    self.charge(item.name.len(), key.line)?;
                    if !layer_names.insert(item.name.clone()) {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate layer name"));
                    }
                    self.doc.layers.push(item);
                }
                "SITE" => {
                    if self.doc.sites.len() >= self.limits.max_sites {
                        return Err(error("LEF_BUDGET", key.line, "max_sites exceeded"));
                    }
                    self.charge(2 * size_of::<LefSite>() + 64, key.line)?;
                    let item = self.site()?;
                    self.charge(item.name.len(), key.line)?;
                    if !site_names.insert(item.name.clone()) {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate site name"));
                    }
                    self.doc.sites.push(item);
                }
                "MACRO" => {
                    if self.doc.macros.len() >= self.limits.max_macros {
                        return Err(error("LEF_BUDGET", key.line, "max_macros exceeded"));
                    }
                    self.charge(2 * size_of::<LefMacro>() + 64, key.line)?;
                    let item = self.macro_()?;
                    self.charge(item.name.len(), key.line)?;
                    if !macro_names.insert(item.name.clone()) {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate macro name"));
                    }
                    self.doc.macros.push(item);
                }
                "VIA" => {
                    if self.doc.vias.len() >= self.limits.max_vias {
                        return Err(error("LEF_BUDGET", key.line, "max_vias exceeded"));
                    }
                    self.charge(2 * size_of::<LefVia>() + 64, key.line)?;
                    let name = self.name()?;
                    let is_default = self.peek()?.is_some_and(|t| t.is("DEFAULT"));
                    if is_default {
                        self.next()?;
                    }
                    let geometry = self.geometry_block(Some(name.text), "via")?;
                    let item = LefVia {
                        name: self.owned(name)?,
                        is_default,
                        geometries: geometry.geometries,
                    };
                    self.charge(item.name.len(), key.line)?;
                    if !via_names.insert(item.name.clone()) {
                        return Err(error("LEF_SYNTAX", key.line, "duplicate via name"));
                    }
                    self.doc.vias.push(item);
                }
                "VIARULE" => self.via_rule(key)?,
                "BEGINEXT" => {
                    self.warn(key, "extension", "Extension content is not interpreted")?;
                    loop {
                        if self.next()?.is("ENDEXT") {
                            break;
                        }
                    }
                }
                "END" => {
                    self.expect("LIBRARY")?;
                    if self.next_opt()?.is_some() {
                        return Err(error("LEF_SYNTAX", key.line, "content after END LIBRARY"));
                    }
                    break;
                }
                "VERSION"
                | "BUSBITCHARS"
                | "DIVIDERCHAR"
                | "MANUFACTURINGGRID"
                | "NAMESCASESENSITIVE"
                | "NOWIREEXTENSIONATPIN"
                | "USEMINSPACING"
                | "CLEARANCEMEASURE"
                | "FIXEDMASK"
                | "MAXVIASTACK"
                | "PROPERTY" => {
                    let values = self.statement()?;
                    match key.text.to_ascii_uppercase().as_str() {
                        "VERSION" => { self.arity(&values,1,key.line)?; if self.doc.version.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate VERSION")); }
                        if !matches!(values[0].text,"5.0"|"5.1"|"5.2"|"5.3"|"5.4"|"5.5"|"5.6"|"5.7"|"5.8") { return Err(self.unsupported(key,"VERSION")); } self.doc.version=Some(self.owned(values[0])?); }
                        "BUSBITCHARS" => { self.arity(&values,1,key.line)?; if values[0].text.len()!=2 { return Err(error("LEF_SYNTAX",key.line,"BUSBITCHARS requires two ASCII characters")); } self.doc.bus_bit_chars=self.owned(values[0])?; }
                        "DIVIDERCHAR" => { self.arity(&values,1,key.line)?; if values[0].text.len()!=1 { return Err(error("LEF_SYNTAX",key.line,"DIVIDERCHAR requires one ASCII character")); } self.doc.divider_char=self.owned(values[0])?; }
                        "MANUFACTURINGGRID" => { if self.doc.manufacturing_grid.is_some() { return Err(error("LEF_SYNTAX",key.line,"duplicate MANUFACTURINGGRID")); } self.arity(&values,1,key.line)?; let grid=self.coord(values[0])?; if grid<=0 { return Err(error("LEF_GEOMETRY",key.line,"MANUFACTURINGGRID must be positive")); } self.doc.manufacturing_grid=Some(grid); }
                        "NAMESCASESENSITIVE" => { self.arity(&values,1,key.line)?; if !values[0].is("ON") { return Err(self.unsupported(key,"case-insensitive names")); } }
                        _ => self.warn(key,"library","Library rule is not modeled; downstream DRC/routing semantics remain unchecked")?,
                    }
                }
                _ => return Err(self.unsupported(key, "library")),
            }
        }
        if !saw_content {
            return Err(error("LEF_SYNTAX", 1, "empty LEF input"));
        }
        Ok(self.doc)
    }
}

/// Read one LEF file. Technology and macro files may be parsed separately at the
/// same selected DBU and combined by their caller with duplicate-name checks.
pub fn parse_lef(input: &str, dbu: u32, limits: &LefLimits) -> Result<LefDocument> {
    if input.len() > limits.max_input_bytes {
        return Err(error("LEF_BUDGET", 1, "max_input_bytes exceeded"));
    }
    if dbu == 0 {
        return Err(error("LEF_GEOMETRY", 1, "target DBU must be positive"));
    }
    if !input.is_ascii() {
        return Err(error("LEF_UNSUPPORTED", 1, "non-ASCII LEF input"));
    }
    Parser {
        lexer: Lexer {
            input,
            offset: 0,
            line: 1,
            tokens: 0,
            max_tokens: limits.max_tokens,
        },
        lookahead: None,
        limits,
        doc: LefDocument {
            version: None,
            dbu_per_micron: dbu,
            source_dbu_per_micron: None,
            manufacturing_grid: None,
            bus_bit_chars: "[]".into(),
            divider_char: "/".into(),
            layers: Vec::new(),
            sites: Vec::new(),
            macros: Vec::new(),
            vias: Vec::new(),
            diagnostics: Vec::new(),
            antenna: LefAntenna::default(),
        },
        charged_bytes: 0,
        pending_pin_antenna: Vec::new(),
        pins: 0,
        ports: 0,
        shapes: 0,
        points: 0,
    }
    .parse()
}

fn decimal_dbu(text: &str, dbu: u32, line: usize) -> Result<i64> {
    decimal_scaled(text, i128::from(dbu), line)
}

fn decimal_scaled(text: &str, dbu: i128, line: usize) -> Result<i64> {
    let bad = || {
        error(
            "LEF_GEOMETRY",
            line,
            format!("invalid/out-of-range coordinate {text}"),
        )
    };
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(i) => (&text[..i], text[i + 1..].parse::<i32>().map_err(|_| bad())?),
        None => (text, 0),
    };
    let (negative, unsigned) = if let Some(s) = mantissa.strip_prefix('-') {
        (true, s)
    } else {
        (false, mantissa.strip_prefix('+').unwrap_or(mantissa))
    };
    let mut value = 0_i128;
    let mut fractional = 0_i32;
    let mut dot = false;
    let mut digits = 0;
    for b in unsigned.bytes() {
        if b == b'.' && !dot {
            dot = true;
            continue;
        }
        if !b.is_ascii_digit() {
            return Err(bad());
        }
        digits += 1;
        value = value
            .checked_mul(10)
            .and_then(|n| n.checked_add(i128::from(b - b'0')))
            .ok_or_else(bad)?;
        if dot {
            fractional = fractional.checked_add(1).ok_or_else(bad)?;
        }
    }
    if digits == 0 {
        return Err(bad());
    }
    if value == 0 {
        return Ok(0);
    }
    value = value.checked_mul(dbu).ok_or_else(bad)?;
    let scale = fractional.checked_sub(exponent).ok_or_else(bad)?;
    if scale >= 0 {
        let power = 10_i128
            .checked_pow(u32::try_from(scale).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        if value % power != 0 {
            return Err(error(
                "LEF_GEOMETRY",
                line,
                format!("value {text} is not exactly representable at scale {dbu} per micron unit"),
            ));
        }
        value /= power;
    } else {
        let power = 10_i128.checked_pow(scale.unsigned_abs()).ok_or_else(bad)?;
        value = value.checked_mul(power).ok_or_else(bad)?;
    }
    if negative {
        value = -value;
    }
    i64::try_from(value).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn antenna_rules_and_pin_areas_are_retained_exactly() {
        let lef = "VERSION 5.8 ; UNITS DATABASE MICRONS 1000 ; END UNITS
LAYER li1 TYPE ROUTING ; DIRECTION VERTICAL ; WIDTH 0.17 ; THICKNESS 0.1 ;
  ANTENNAMODEL OXIDE1 ;
  ANTENNADIFFSIDEAREARATIO PWL ( ( 0 75 ) ( 0.0125 75 ) ( 0.0225 85.125 ) ( 22.5 10200 ) ) ; END li1
LAYER via TYPE CUT ; ANTENNAAREARATIO 20.0 ; ANTENNADIFFAREARATIO 6 ; END via
LAYER M2 TYPE ROUTING ; DIRECTION HORIZONTAL ; WIDTH 0.28 ; THICKNESS 0.54 ;
  ANTENNAMODEL OXIDE1 ; ANTENNADIFFSIDEAREARATIO 400 ; ANTENNAGATEPLUSDIFF 15 ;
  ANTENNACUMAREARATIO 5000 ; ANTENNAMODEL OXIDE2 ; ANTENNASIDEAREARATIO 900 ; END M2
LAYER M3 TYPE ROUTING ; DIRECTION HORIZONTAL ; WIDTH 0.28 ;
  ANTENNADIFFSIDEAREARATIO PWL ( ( 1 5 ) ( 0.5 6 ) ) ; ANTENNASIDEAREARATIO 0.0000001 ; END M3
SITE s CLASS CORE ; SIZE 1 BY 1 ; END s
MACRO d CLASS CORE ANTENNACELL ; SIZE 1 BY 1 ; SITE s ;
PIN DIODE DIRECTION INPUT ; ANTENNAGATEAREA 0.4347 ; ANTENNADIFFAREA 0.4347 ;
  ANTENNADIFFAREA 0.1 LAYER M2 ; ANTENNAPARTIALMETALAREA 1 ; ANTENNAGATEAREA 0.0000001 ;
PORT LAYER li1 ; RECT 0 0 0.5 0.5 ; END END DIODE END d END LIBRARY";
        let doc = parse_lef(lef, 1000, &LefLimits::default()).unwrap();
        let a = &doc.antenna;
        let li1 = a.layers.iter().find(|l| l.layer == "li1").unwrap();
        assert_eq!(li1.thickness, Some(100));
        assert_eq!(li1.model.as_deref(), Some("OXIDE1"));
        assert_eq!(
            li1.diff_side_area_ratio,
            Some(AntennaRatio::Pwl(vec![
                [0, 75_000_000],
                [12_500, 75_000_000],
                [22_500, 85_125_000],
                [22_500_000, 10_200_000_000]
            ]))
        );
        let via = a.layers.iter().find(|l| l.layer == "via").unwrap();
        assert_eq!(via.area_ratio, Some(20_000_000));
        assert_eq!(via.diff_area_ratio, Some(AntennaRatio::Value(6_000_000)));
        let m2 = a.layers.iter().find(|l| l.layer == "M2").unwrap();
        assert_eq!(
            (m2.thickness, m2.gate_plus_diff),
            (Some(540), Some(15_000_000))
        );
        assert_eq!(m2.cum_area_ratio, Some(5_000_000_000));
        // The OXIDE2 rule is not attributed to OXIDE1.
        assert_eq!(m2.side_area_ratio, None);
        let pin = &a.pins[0];
        assert_eq!((pin.macro_name.as_str(), pin.pin.as_str()), ("d", "DIODE"));
        assert_eq!(
            (pin.gate_area, pin.diff_area, pin.layer_qualified),
            (434_700, 534_700, true)
        );
        let kinds: Vec<_> = a
            .unsupported
            .iter()
            .map(|u| (u.owner.as_str(), u.keyword.as_str()))
            .collect();
        // Decreasing PWL, an inexact ratio, OXIDE2, a partial-metal pin area and an
        // inexact pin area are recorded, never silently used or fatal.
        for expected in [
            ("M3", "ANTENNADIFFSIDEAREARATIO"),
            ("M3", "ANTENNASIDEAREARATIO"),
            ("M2", "ANTENNASIDEAREARATIO"),
            ("d/DIODE", "ANTENNAPARTIALMETALAREA"),
            ("d/DIODE", "ANTENNAGATEAREA"),
        ] {
            assert!(
                kinds.contains(&expected),
                "{expected:?} missing from {kinds:?}"
            );
        }
        assert_eq!(doc.macros[0].class.as_deref(), Some("CORE ANTENNACELL"));
        // Import-facing diagnostics are unchanged: antenna is still "not modeled" there.
        assert!(
            doc.diagnostics
                .iter()
                .any(|d| d.keyword.starts_with("ANTENNA"))
        );
    }
    const TINY: &str = r#"VERSION 5.8 ; UNITS DATABASE MICRONS 2000 ; END UNITS
SITE rowsite CLASS CORE ; SIZE 0.5 BY 1 ; SYMMETRY X Y ; END rowsite
MACRO cell CLASS CORE ; ORIGIN 0 0 ; SIZE 1 BY 1 ; SITE rowsite ;
PIN A DIRECTION INPUT ; USE SIGNAL ; ANTENNAGATEAREA 0.12345 ;
PORT LAYER Metal1 ; RECT -0.005 0 0.25 0.5 ; END END A
PIN Z DIRECTION OUTPUT ; PORT LAYER Metal1 ;
POLYGON 0.5 0 1 0 1 1 0.5 1 ; END END Z
OBS LAYER Metal1 ; RECT 0.3 0.1 0.4 0.2 ; END END cell END LIBRARY"#;
    #[test]
    fn preserves_integer_site_pin_and_obstruction_geometry() {
        let doc = parse_lef(TINY, 1000, &LefLimits::default()).unwrap();
        assert_eq!(doc.source_dbu_per_micron, Some(2000));
        assert_eq!(doc.sites[0].size, [500, 1000]);
        let cell = &doc.macros[0];
        assert_eq!(cell.size, [1000, 1000]);
        assert_eq!(
            cell.pins[0].ports[0].geometries[0],
            LefGeometry::Rect {
                layer: "Metal1".into(),
                rect: [-5, 0, 250, 500]
            }
        );
        assert_eq!(cell.obstructions.len(), 1);
        assert!(
            matches!(&cell.pins[1].ports[0].geometries[0],LefGeometry::Polygon{points,..} if points.len()==4)
        );
        assert!(
            doc.diagnostics
                .iter()
                .any(|d| d.keyword == "ANTENNAGATEAREA")
        );
    }
    #[test]
    fn foreign_bindings_are_retained_exactly() {
        let s = "MACRO a FOREIGN a ; SIZE 1 BY 1 ; END a\n\
MACRO b FOREIGN other 0.5 -1 FS ; FOREIGN b ; SIZE 1 BY 1 ; END b";
        let doc = parse_lef(s, 2000, &LefLimits::default()).unwrap();
        assert_eq!(
            doc.macros[0].foreign,
            vec![LefForeign {
                name: "a".into(),
                origin: [0, 0],
                orientation: None
            }]
        );
        assert_eq!(doc.macros[1].foreign[0].origin, [1000, -2000]);
        assert_eq!(doc.macros[1].foreign[0].orientation.as_deref(), Some("FS"));
        assert_eq!(doc.macros[1].foreign.len(), 2);
        assert!(doc.diagnostics.iter().any(|d| d.keyword == "FOREIGN"));
        for bad in [
            "MACRO a FOREIGN a 1 ; SIZE 1 BY 1 ; END a",
            "MACRO a FOREIGN a 0 0 R90 ; SIZE 1 BY 1 ; END a",
            "MACRO a FOREIGN a 0.00001 0 ; SIZE 1 BY 1 ; END a",
        ] {
            assert!(
                parse_lef(bad, 1000, &LefLimits::default()).is_err(),
                "{bad}"
            );
        }
    }
    #[test]
    fn decimals_never_round_or_use_float_geometry() {
        for (s, expected) in [
            ("1.25", 1250),
            ("-0.005", -5),
            ("1.2e-2", 12),
            (".5", 500),
            ("+0", 0),
        ] {
            assert_eq!(decimal_dbu(s, 1000, 1).unwrap(), expected);
        }
        for s in [
            "0.0005",
            "1e1000",
            "NaN",
            "inf",
            "1.2.3",
            "9223372036854775808",
        ] {
            assert!(decimal_dbu(s, 1000, 1).is_err(), "{s}");
        }
        assert_eq!(decimal_dbu("0.0005", 2000, 1).unwrap(), 1);
    }
    #[test]
    fn structural_errors_and_geometry_modes_fail_closed() {
        for s in [
            "MACRO a SIZE 1 BY 1 ; END b",
            "MACRO a SIZE 1 BY 1 ; PIN A PORT LAYER M1 ; PATH 0 0 1 1 ; END END A END a",
            "SITE s SIZE 1 BY 1 END s",
            "NONDEFAULTRULE x END x",
            "MACRO a SIZE 1 BY 1 ; PIN A PORT RECT 0 0 1 1 ; END END A END a",
        ] {
            assert!(parse_lef(s, 1000, &LefLimits::default()).is_err(), "{s}");
        }
    }
    #[test]
    fn budgets_stop_before_unbounded_accumulation() {
        let bounds = [
            LefLimits {
                max_tokens: 5,
                ..LefLimits::default()
            },
            LefLimits {
                max_pins: 1,
                ..LefLimits::default()
            },
            LefLimits {
                max_shapes: 1,
                ..LefLimits::default()
            },
            LefLimits {
                max_ast_bytes: 32,
                ..LefLimits::default()
            },
            LefLimits {
                max_points: 3,
                ..LefLimits::default()
            },
        ];
        for limits in bounds {
            assert_eq!(
                parse_lef(TINY, 1000, &limits).unwrap_err().code,
                "LEF_BUDGET"
            );
        }
    }
    #[test]
    fn diagnostics_aggregate_repeated_semantics() {
        let s = "MACRO a SIZE 1 BY 1 ; PIN A ANTENNAGATEAREA 1 ; ANTENNAGATEAREA 2 ; END A END a";
        let doc = parse_lef(s, 1000, &LefLimits::default()).unwrap();
        assert_eq!(doc.diagnostics.len(), 1);
        assert_eq!(doc.diagnostics[0].occurrences, 2);
    }
    #[test]
    fn quoted_properties_do_not_terminate_at_inner_semicolons() {
        let s = "LAYER M1 TYPE ROUTING ; PROPERTY LEF58_X \"a ; END M1 ; \\\" quote\" ; END M1";
        let doc = parse_lef(s, 1000, &LefLimits::default()).unwrap();
        assert_eq!(doc.layers.len(), 1);
        assert_eq!(doc.diagnostics[0].keyword, "PROPERTY");
    }
    #[test]
    #[ignore = "requires explicit pinned Ciel PDK LEF paths; absence is unverified"]
    fn real_dual_pdk_corpus() {
        let cases = [
            ("RUSTROAD_SKY130_TECH_LEF", 14, 2, 0, 25),
            ("RUSTROAD_SKY130_CELL_LEF", 0, 0, 437, 0),
            ("RUSTROAD_GF180_TECH_LEF", 15, 1, 0, 56),
            ("RUSTROAD_GF180_CELL_LEF", 0, 0, 229, 0),
        ];
        for (variable, layers, sites, macros, vias) in cases {
            let path =
                std::env::var(variable).expect("explicit PDK path required for real verification");
            let source = std::fs::read_to_string(&path).unwrap();
            let document = parse_lef(&source, 2000, &LefLimits::default())
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(document.layers.len(), layers, "{variable}");
            assert_eq!(document.sites.len(), sites, "{variable}");
            assert_eq!(document.macros.len(), macros, "{variable}");
            assert_eq!(document.vias.len(), vias, "{variable}");
            if variable == "RUSTROAD_SKY130_TECH_LEF" {
                assert_eq!(
                    document
                        .sites
                        .iter()
                        .find(|s| s.name == "unithd")
                        .unwrap()
                        .size,
                    [920, 5440]
                );
                assert_eq!(document.source_dbu_per_micron, Some(1000));
            }
            if variable == "RUSTROAD_GF180_TECH_LEF" {
                assert_eq!(document.sites[0].size, [1120, 7840]);
                assert_eq!(document.source_dbu_per_micron, Some(2000));
            }
            let pins: usize = document.macros.iter().map(|m| m.pins.len()).sum();
            let shapes: usize = document
                .macros
                .iter()
                .map(|m| {
                    m.obstructions.len()
                        + m.pins
                            .iter()
                            .map(|p| p.ports.iter().map(|p| p.geometries.len()).sum::<usize>())
                            .sum::<usize>()
                })
                .sum();
            // Every PDK cell binds to the same-named GDS cell at the origin.
            assert!(document.macros.iter().all(|m| m.foreign.len() == 1
                && m.foreign[0].name == m.name
                && m.foreign[0].origin == [0, 0]
                && m.foreign[0].orientation.is_none()));
            if variable == "RUSTROAD_SKY130_CELL_LEF" {
                assert_eq!((pins, shapes), (3526, 22849));
            }
            if variable == "RUSTROAD_GF180_CELL_LEF" {
                assert_eq!((pins, shapes), (1748, 10486));
            }
            println!(
                "{variable}: layers={layers} sites={sites} macros={macros} vias={vias} pins={pins} shapes={shapes} diagnostic_kinds={}",
                document.diagnostics.len()
            );
        }
    }
    #[test]
    fn area_minstep_and_cut_enclosure_are_exact_integer_rules() {
        let s = "LAYER via TYPE CUT ; WIDTH 0.15 ; ENCLOSURE BELOW 0.055 0.085 ; ENCLOSURE ABOVE 0.055 0.085 ; END via
LAYER met1 TYPE ROUTING ; DIRECTION HORIZONTAL ; WIDTH 0.14 ; AREA 0.083 ; MINSTEP 0.05 ; END met1
LAYER Metal2 TYPE ROUTING ; DIRECTION VERTICAL ; WIDTH 0.28 ; AREA 0.1444 ; AREA 0.2 ; END Metal2
LAYER Via9 TYPE CUT ; ENCLOSURE 0.01 0.06 ; END Via9";
        let doc = parse_lef(s, 1000, &LefLimits::default()).unwrap();
        assert_eq!(doc.layers[0].enclosure_below, Some([55, 85]));
        assert_eq!(doc.layers[0].enclosure_above, Some([55, 85]));
        assert_eq!(doc.layers[1].min_area, Some(83_000));
        assert_eq!(doc.layers[1].min_step, Some(50));
        // The first unconditional AREA is kept; a second one stays a diagnostic.
        assert_eq!(doc.layers[2].min_area, Some(144_400));
        assert!(doc.diagnostics.iter().any(|d| d.keyword == "AREA"));
        // ENCLOSURE without BELOW/ABOVE is not modeled and is reported.
        assert_eq!(doc.layers[3].enclosure_below, None);
        assert!(doc.diagnostics.iter().any(|d| d.keyword == "ENCLOSURE"));
        assert!(doc.diagnostics.iter().any(|d| d.keyword == "MINSTEP"));
        // 2000 DBU/um: 0.1444 um^2 = 577,600 DBU^2 exactly.
        let doc = parse_lef(s, 2000, &LefLimits::default()).unwrap();
        assert_eq!(doc.layers[2].min_area, Some(577_600));
        // An area that is not an integer number of DBU^2 is an error, not rounded.
        let bad = "LAYER m TYPE ROUTING ; AREA 0.0000001 ; END m";
        assert!(parse_lef(bad, 1000, &LefLimits::default()).is_err());
        assert!(
            parse_lef(
                "LAYER m TYPE ROUTING ; AREA 0 ; END m",
                1000,
                &LefLimits::default()
            )
            .is_err()
        );
    }
    #[test]
    fn duplicate_attributes_and_invalid_kinds_are_rejected() {
        for source in [
            "LAYER M TYPE ROUTING ; WIDTH 1 ; WIDTH 2 ; END M",
            "LAYER M TYPE UNKNOWN ; END M",
            "MACRO M SIZE 1 BY 1 ; PIN P USE SOMETHING ; END P END M",
            "MANUFACTURINGGRID 0.1 ; MANUFACTURINGGRID 0.2 ;",
            "VERSION 100 ;",
        ] {
            assert!(
                parse_lef(source, 1000, &LefLimits::default()).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    #[ignore = "requires explicit ORFS GF180 polygon LEF path; absence is unverified"]
    fn real_gf180_polygon_corpus() {
        let path =
            std::env::var("RUSTROAD_GF180_POLYGON_LEF").expect("explicit polygon PDK LEF required");
        let source = std::fs::read_to_string(&path).unwrap();
        let doc = parse_lef(&source, 2000, &LefLimits::default()).unwrap();
        assert_eq!(doc.macros.len(), 229);
        let mut count = 0;
        let mut max_points = 0;
        for cell in &doc.macros {
            for shape in cell.obstructions.iter().chain(
                cell.pins
                    .iter()
                    .flat_map(|p| p.ports.iter())
                    .flat_map(|p| p.geometries.iter()),
            ) {
                if let LefGeometry::Polygon { points, .. } = shape {
                    count += 1;
                    max_points = max_points.max(points.len());
                }
            }
        }
        assert_eq!(count, 2157);
        assert!(max_points > 40);
        println!("GF180 real polygons={count}; largest polygon has {max_points} points");
    }
}
