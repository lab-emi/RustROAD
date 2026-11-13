//! The two LEF scans of the post-route runner: macro sizes for the
//! equal-footprint families, and the physical-only macros (every pin is
//! `USE POWER`/`GROUND`) that the timing engine needs stub cells for.
//!
//! Both follow the reference runner's regular expressions exactly, because
//! which text they accept decides which cells may be swapped:
//! `(?ms)^MACRO\s+(\S+)\s*$(.*?)^END\s+\1\s*$` for a macro block,
//! `\bSIZE\s+([\d.]+)\s+BY\s+([\d.]+)\s*;` for its size and
//! `(?ms)^\s*PIN\s+(\S+)\s*$(.*?)^\s*END\s+\1\s*$` for a pin block.
use super::super::text::{is_space, strip};
use super::super::{StepResult, fail};
use super::liberty::Library;

const CODE: &str = "POSTROUTE";

struct Scan<'a> {
    text: &'a str,
}

impl<'a> Scan<'a> {
    fn at(&self, index: usize) -> Option<char> {
        self.text[index..].chars().next()
    }
    /// End of the whitespace run starting at `index`.
    fn skip_space(&self, index: usize) -> usize {
        let rest = &self.text[index..];
        index + rest.find(|c| !is_space(c)).unwrap_or(rest.len())
    }
    /// End of the non-whitespace run starting at `index`.
    fn token_end(&self, index: usize) -> usize {
        let rest = &self.text[index..];
        index + rest.find(is_space).unwrap_or(rest.len())
    }
    /// `\s*$` from `index` under `(?m)`: the greedy run backs off to its last
    /// position that is the end of the text or directly before a newline.
    fn line_end(&self, index: usize) -> Option<usize> {
        let end = self.skip_space(index);
        if end == self.text.len() {
            return Some(end);
        }
        self.text[index..end].rfind('\n').map(|at| index + at)
    }
    fn line_start(&self, index: usize) -> bool {
        index == 0 || self.text.as_bytes()[index - 1] == b'\n'
    }
    /// `WORD\s+(\S+)\s*$` at `index`: the name and the position after the match.
    fn header(&self, index: usize, word: &str) -> Option<(&'a str, usize)> {
        let rest = self.text[index..].strip_prefix(word)?;
        let after = index + word.len();
        if !rest.chars().next().is_some_and(is_space) {
            return None;
        }
        let start = self.skip_space(after);
        let end = self.token_end(start);
        if end == start {
            return None;
        }
        Some((&self.text[start..end], self.line_end(end)?))
    }
    /// `END\s+<name>\s*$` at `index`: the position after the match.
    fn footer(&self, index: usize, name: &str) -> Option<usize> {
        let rest = self.text[index..].strip_prefix("END")?;
        if !rest.chars().next().is_some_and(is_space) {
            return None;
        }
        let start = self.skip_space(index + 3);
        self.text[start..].strip_prefix(name)?;
        self.line_end(start + name.len())
    }
}

/// One `MACRO name ... END name` block: its name and body text.
pub struct Macro<'a> {
    pub name: &'a str,
    pub body: &'a str,
}

/// Every non-overlapping macro block, in file order.
pub fn macros(text: &str) -> Vec<Macro<'_>> {
    let scan = Scan { text };
    let mut found = Vec::new();
    let mut index = 0;
    while index <= text.len() {
        let matched = scan
            .line_start(index)
            .then(|| scan.header(index, "MACRO"))
            .flatten()
            .and_then(|(name, body_start)| {
                // Lazy body: the first line start at or after it where the footer matches.
                let mut probe = body_start;
                loop {
                    if scan.line_start(probe)
                        && let Some(end) = scan.footer(probe, name)
                    {
                        return Some((name, body_start, probe, end));
                    }
                    probe += text[probe..].find('\n')? + 1;
                }
            });
        match matched {
            Some((name, body_start, body_end, end)) => {
                found.push(Macro {
                    name,
                    body: &text[body_start..body_end],
                });
                // An empty match cannot occur (the header is not empty).
                index = end.max(index + 1);
            }
            None => match text[index..].find('\n') {
                Some(offset) => index += offset + 1,
                None => break,
            },
        }
    }
    found
}

fn digits_run(text: &str, index: usize) -> usize {
    let rest = &text[index..];
    index
        + rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len())
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `re.search(r'\bSIZE\s+([\d.]+)\s+BY\s+([\d.]+)\s*;', body)`.
fn size(body: &str) -> Option<(&str, &str)> {
    let scan = Scan { text: body };
    let mut from = 0;
    while let Some(offset) = body[from..].find("SIZE") {
        let start = from + offset;
        from = start + 1;
        // `\b` before `S`: the previous character is not a word character.
        if body[..start].chars().next_back().is_some_and(word_char) {
            continue;
        }
        let mut at = start + 4;
        let mut fields = [""; 2];
        let mut ok = true;
        for (slot, separator) in fields.iter_mut().zip(["", "BY"]) {
            if !separator.is_empty() {
                match body[at..].strip_prefix(separator) {
                    Some(_) => at += separator.len(),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            let begin = scan.skip_space(at);
            let end = digits_run(body, begin);
            if begin == at || end == begin {
                ok = false;
                break;
            }
            *slot = &body[begin..end];
            at = end;
            if separator.is_empty() {
                // `\s+` before `BY`.
                let next = scan.skip_space(at);
                if next == at {
                    ok = false;
                    break;
                }
                at = next;
            }
        }
        if ok && scan.at(scan.skip_space(at)) == Some(';') {
            return Some((fields[0], fields[1]));
        }
    }
    None
}

/// `{macro: (width_dbu, height_dbu)}` at 1000 DBU per micron, rounding half
/// to even as Python's `round` does.
///
/// # Errors
/// `POSTROUTE` if a size is not a decimal number (`1.2.3`).
pub fn sizes(text: &str) -> StepResult<Vec<(String, (i64, i64))>> {
    let mut out: Vec<(String, (i64, i64))> = Vec::new();
    for block in macros(text) {
        let Some((width, height)) = size(block.body) else {
            continue;
        };
        let dbu = |field: &str| -> StepResult<i64> {
            let value: f64 = field
                .parse()
                .map_err(|_| fail(CODE, format!("LEF SIZE is not a number: {field}")))?;
            Ok((value * 1000.0).round_ties_even() as i64)
        };
        let value = (dbu(width)?, dbu(height)?);
        match out.iter_mut().find(|(name, _)| name == block.name) {
            Some(entry) => entry.1 = value,
            None => out.push((block.name.to_owned(), value)),
        }
    }
    Ok(out)
}

/// `re.search(r'\bUSE\s+(POWER|GROUND)\s*;', body)`.
fn supply_use(body: &str) -> bool {
    let scan = Scan { text: body };
    let mut from = 0;
    while let Some(offset) = body[from..].find("USE") {
        let start = from + offset;
        from = start + 1;
        if body[..start].chars().next_back().is_some_and(word_char) {
            continue;
        }
        let begin = scan.skip_space(start + 3);
        if begin == start + 3 {
            continue;
        }
        for word in ["POWER", "GROUND"] {
            if body[begin..].starts_with(word)
                && scan.at(scan.skip_space(begin + word.len())) == Some(';')
            {
                return true;
            }
        }
    }
    false
}

/// The bodies of every `PIN name ... END name` block of a macro body.
fn pins(body: &str) -> Vec<&str> {
    let scan = Scan { text: body };
    let mut found = Vec::new();
    let mut index = 0;
    while index <= body.len() {
        // `^\s*PIN`: from a line start, whitespace (lines included) up to the keyword.
        let matched = scan
            .line_start(index)
            .then(|| scan.header(scan.skip_space(index), "PIN"))
            .flatten()
            .and_then(|(name, body_start)| {
                let mut probe = body_start;
                loop {
                    if scan.line_start(probe)
                        && let Some(end) = scan.footer(scan.skip_space(probe), name)
                    {
                        return Some((body_start, probe, end));
                    }
                    probe += body[probe..].find('\n')? + 1;
                }
            });
        match matched {
            Some((body_start, body_end, end)) => {
                found.push(&body[body_start..body_end]);
                index = end.max(index + 1);
            }
            None => match body[index..].find('\n') {
                Some(offset) => index += offset + 1,
                None => break,
            },
        }
    }
    found
}

/// Macros that have pins and whose every pin is a supply pin, in file order.
pub fn supply_only_macros(text: &str) -> Vec<&str> {
    macros(text)
        .into_iter()
        .filter(|block| {
            let pins = pins(block.body);
            !pins.is_empty() && pins.iter().all(|pin| supply_use(pin))
        })
        .map(|block| block.name)
        .collect()
}

/// `(?m)^[ \t]*NAME[ \t]+\\?[^\s(]+[ \t]*\(([^;]*)\);` with a non-blank group:
/// an instance of `name` that has connections.
fn connected_instance(logical: &str, name: &str) -> bool {
    let blanks = |text: &str| text.len() - text.trim_start_matches([' ', '\t']).len();
    let mut offset = 0;
    for line in logical.split('\n') {
        let start = offset;
        offset += line.len() + 1;
        let lead = blanks(line);
        let Some(rest) = line[lead..].strip_prefix(name) else {
            continue;
        };
        let gap = blanks(rest);
        if gap == 0 {
            continue;
        }
        let rest = &rest[gap..];
        let rest = rest.strip_prefix('\\').unwrap_or(rest);
        let run = rest
            .find(|c: char| c == '(' || is_space(c))
            .unwrap_or(rest.len());
        if run == 0 {
            continue;
        }
        let rest = &rest[run..];
        let Some(rest) = rest[blanks(rest)..].strip_prefix('(') else {
            continue;
        };
        // `[^;]*` may run over the following lines up to the first semicolon.
        let from = start + (line.len() - rest.len());
        let Some(end) = logical[from..].find(';') else {
            continue;
        };
        let group = &logical[from..from + end];
        if let Some(inner) = group.strip_suffix(')')
            && !strip(inner).is_empty()
        {
            return true;
        }
    }
    false
}

/// LEF macros that are physical only (every pin a supply pin) and unknown to
/// both Liberty corners, in LEF order: the timing engine needs a stub cell
/// for each.
///
/// # Errors
/// `POSTROUTE` if the logical netlist connects such a cell, or for more than
/// 256 of them.
pub fn physical_only_cells(
    lef_text: &str,
    libs: &[Library; 2],
    logical: &str,
) -> StepResult<Vec<String>> {
    let mut names = Vec::new();
    for name in supply_only_macros(lef_text) {
        if libs.iter().any(|lib| lib.contains(name)) {
            continue;
        }
        if connected_instance(logical, name) {
            return Err(fail(CODE, format!("connected physical-only cell: {name}")));
        }
        names.push(name.to_owned());
    }
    if names.len() > 256 {
        return Err(fail(CODE, "physical-only cell budget"));
    }
    Ok(names)
}

/// The stub Liberty library of one corner for the physical-only cells.
#[must_use]
pub fn stub_library(corner: &str, names: &[String]) -> String {
    let mut body =
        format!("library(physical_{corner}) {{ time_unit : \"1ns\"; capacitive_load_unit(1,pf);\n");
    for name in names {
        body.push_str(&format!(" cell({name}) {{ area : 0; }}\n"));
    }
    body.push_str("}\n");
    body
}
