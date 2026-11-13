//! Line-based Liberty reader: cell -> area, `dont_use`, sequential/bus flag
//! and the four pin attributes the sizing policy reads.
//!
//! One pass, one line at a time: a line that opens a group pushes it, a
//! `key : value ;` line inside a cell or a pin sets an attribute, closing
//! braces pop. Comments start at `/*` and run to the end of the line. This
//! is deliberately the reader of the reference runner, including what it
//! ignores (attributes on a line that also opens a group, multi-line
//! values): the sizing families are defined by what it sees.
use super::super::text::{float, strip};
use super::super::{StepResult, fail, read_bytes};
use std::path::Path;

const CODE: &str = "POSTROUTE";

/// Insertion-ordered string map (a Python `dict`).
#[derive(Clone, Debug, PartialEq)]
pub struct Ordered<V>(pub Vec<(String, V)>);
impl<V> Default for Ordered<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}
impl<V> Ordered<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn set(&mut self, key: &str, value: V) {
        match self.get_mut(key) {
            Some(slot) => *slot = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The attributes of one pin group, as written (unquoted text).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pin {
    pub direction: Option<String>,
    pub function: Option<String>,
    pub max_capacitance: Option<String>,
    pub max_transition: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cell {
    pub area: Option<f64>,
    pub dont_use: bool,
    pub unsupported: bool,
    pub pins: Ordered<Pin>,
}

/// Cells in file order with an index by name.
#[derive(Clone, Debug, Default)]
pub struct Library {
    pub cells: Vec<(String, Cell)>,
    index: std::collections::HashMap<String, usize>,
}
impl Library {
    pub fn get(&self, name: &str) -> Option<&Cell> {
        self.index.get(name).map(|at| &self.cells[*at].1)
    }
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }
    fn entry(&mut self, name: &str) -> usize {
        if let Some(at) = self.index.get(name) {
            return *at;
        }
        self.cells.push((name.to_owned(), Cell::default()));
        self.index.insert(name.to_owned(), self.cells.len() - 1);
        self.cells.len() - 1
    }
}

/// `header[header.find('(') + 1:header.rfind(')')]`, on characters.
fn group_argument(header: &str) -> &str {
    let Some(open) = header.find('(') else {
        return "";
    };
    let start = open + 1;
    let end = match header.rfind(')') {
        Some(close) => close,
        // Python's `rfind` gives -1: the slice stops before the last character.
        None => header.char_indices().next_back().map_or(0, |(at, _)| at),
    };
    if end <= start {
        ""
    } else {
        &header[start..end]
    }
}

/// Parse a Liberty file of at most 128 MiB.
///
/// # Errors
/// `POSTROUTE` on unbalanced braces, nesting over 16, an `area` that is not
/// a number, or a file without cells.
pub fn read(path: &Path) -> StepResult<Library> {
    let data = read_bytes(path, 128 << 20, CODE)?;
    parse(&String::from_utf8_lossy(&data), &path.display().to_string())
}

/// Parse Liberty text; `origin` names it in errors.
///
/// # Errors
/// As [`read`].
pub fn parse(text: &str, origin: &str) -> StepResult<Library> {
    let mut library = Library::default();
    // (kind, argument) of every open group; continuation braces are empty.
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut cell: Option<usize> = None;
    // Universal newlines: `\n`, `\r\n` and a lone `\r` end a line.
    for line in text.split('\n').flat_map(|line| line.split('\r')) {
        let text = strip(line.split("/*").next().unwrap_or(""));
        let opens = text.matches('{').count();
        let closes = text.matches('}').count();
        if opens > 0 {
            let header = strip(text.split('{').next().unwrap_or(""));
            let kind = strip(header.split('(').next().unwrap_or(""));
            let argument = strip(group_argument(header)).trim_matches('"');
            if stack.len() == 1 && kind == "cell" {
                cell = Some(library.entry(argument));
            } else if stack.len() == 2
                && let Some(at) = cell
            {
                let current = &mut library.cells[at].1;
                if matches!(
                    kind,
                    "ff" | "latch"
                        | "statetable"
                        | "bus"
                        | "bundle"
                        | "test_cell"
                        | "ff_bank"
                        | "latch_bank"
                ) {
                    current.unsupported = true;
                } else if kind == "pin" {
                    current.pins.set(argument, Pin::default());
                }
            }
            stack.push((kind.to_owned(), argument.to_owned()));
            for _ in 1..opens {
                stack.push((String::new(), String::new()));
            }
        } else if text.contains(':') && text.ends_with(';') && !stack.is_empty() {
            let body = &text[..text.len() - 1];
            let (key, value) = body.split_once(':').unwrap_or((body, ""));
            let (key, value) = (strip(key), strip(value).trim_matches('"'));
            if let Some(at) = cell {
                let current = &mut library.cells[at].1;
                if stack.len() == 2 {
                    match key {
                        "area" => {
                            current.area = Some(float(value).ok_or_else(|| {
                                fail(CODE, format!("Liberty area is not a number: {origin}"))
                            })?);
                        }
                        "dont_use" => current.dont_use = value == "true",
                        "clock_gating_integrated_cell"
                        | "is_level_shifter"
                        | "is_isolation_cell" => current.unsupported = true,
                        _ => {}
                    }
                } else if stack.len() == 3
                    && stack[2].0 == "pin"
                    && matches!(
                        key,
                        "direction" | "function" | "max_capacitance" | "max_transition"
                    )
                {
                    let pin = current.pins.get_mut(&stack[2].1).ok_or_else(|| {
                        fail(
                            CODE,
                            format!("Liberty pin attribute outside its pin: {origin}"),
                        )
                    })?;
                    let slot = match key {
                        "direction" => &mut pin.direction,
                        "function" => &mut pin.function,
                        "max_capacitance" => &mut pin.max_capacitance,
                        _ => &mut pin.max_transition,
                    };
                    *slot = Some(value.to_owned());
                }
            }
        }
        for _ in 0..closes {
            if stack.pop().is_none() {
                return Err(fail(CODE, format!("unbalanced Liberty braces: {origin}")));
            }
        }
        if stack.len() < 2 {
            cell = None;
        }
        if stack.len() > 16 {
            return Err(fail(CODE, format!("Liberty nesting budget: {origin}")));
        }
    }
    if library.cells.is_empty() || !stack.is_empty() {
        return Err(fail(
            CODE,
            format!("Liberty without cells or unbalanced: {origin}"),
        ));
    }
    Ok(library)
}
