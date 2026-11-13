//! Instance view of a flat canonical Verilog export (`rustroad
//! export-verilog`): one instance per line, named connections.
//!
//! The accepted text is exactly what the reference runner's expressions
//! accept, per line:
//! `^[ \t]+MASTER[ \t]+(\\\S+ |IDENT)[ \t]*\((.*)\);[ \t]*$` and, inside the
//! connection list, `\.PIN\(\s*(\\\S+ |[^(),\s]+)?\s*\)`. Anything else (bus
//! concatenations, unconnected pins, declarations) contributes nothing.
use super::super::text::is_space;
use super::liberty::Ordered;

/// `[A-Za-z_][\w$]*` at the start of `text`: its byte length.
fn identifier(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    for (at, c) in chars {
        if !(c.is_alphanumeric() || c == '_' || c == '$') {
            return Some(at);
        }
    }
    Some(text.len())
}

fn blanks(text: &str) -> usize {
    text.len() - text.trim_start_matches([' ', '\t']).len()
}

/// Netlist/STA names without the Verilog escape.
pub fn base(name: &str) -> &str {
    match name.strip_prefix('\\') {
        Some(rest) => rest.trim_end_matches(is_space),
        None => name,
    }
}

/// `\\\S+ ` at the start of `text`: the byte length including the space.
fn escaped(text: &str) -> Option<usize> {
    let rest = text.strip_prefix('\\')?;
    let run = rest.find(is_space).unwrap_or(rest.len());
    (run > 0 && rest[run..].starts_with(' ')).then_some(1 + run + 1)
}

/// One instance: its master and `{pin: net}` in connection order.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    pub master: String,
    pub pins: Ordered<String>,
}

fn connections(list: &str) -> Ordered<String> {
    let mut pins = Ordered::default();
    let mut at = 0;
    while let Some(offset) = list[at..].find('.') {
        let dot = at + offset;
        at = dot + 1;
        let Some(name) = identifier(&list[dot + 1..]) else {
            continue;
        };
        let pin = &list[dot + 1..dot + 1 + name];
        let mut cursor = dot + 1 + name;
        if !list[cursor..].starts_with('(') {
            continue;
        }
        cursor += 1;
        cursor += list[cursor..].len() - list[cursor..].trim_start_matches(is_space).len();
        let close = |from: usize| {
            let rest = &list[from..];
            let space = rest.len() - rest.trim_start_matches(is_space).len();
            rest[space..].starts_with(')').then_some(from + space + 1)
        };
        // The optional net: an escaped identifier first, then a plain token;
        // the token backs off one character at a time, then the group is empty.
        let mut found: Option<(&str, usize)> = None;
        if let Some(length) = escaped(&list[cursor..])
            && let Some(end) = close(cursor + length)
        {
            found = Some((&list[cursor..cursor + length], end));
        }
        if found.is_none() {
            let rest = &list[cursor..];
            let run = rest
                .find(|c: char| matches!(c, '(' | ')' | ',') || is_space(c))
                .unwrap_or(rest.len());
            let mut length = run;
            while length > 0 {
                if rest.is_char_boundary(length)
                    && let Some(end) = close(cursor + length)
                {
                    found = Some((&rest[..length], end));
                    break;
                }
                length -= 1;
            }
        }
        match found {
            Some((net, end)) => {
                pins.set(pin, base(net).to_owned());
                at = end;
            }
            None => {
                if let Some(end) = close(cursor) {
                    at = end;
                }
            }
        }
    }
    pins
}

/// Flat canonical export: instance -> (master, {pin: net}), in file order
/// (a repeated instance name keeps its first position and its last value).
pub fn instances(text: &str) -> Ordered<Instance> {
    let mut out: Ordered<Instance> = Ordered::default();
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in text.split('\n') {
        let lead = blanks(line);
        if lead == 0 {
            continue;
        }
        let rest = &line[lead..];
        let Some(master_len) = identifier(rest) else {
            continue;
        };
        let master = &rest[..master_len];
        let rest = &rest[master_len..];
        let gap = blanks(rest);
        if gap == 0 {
            continue;
        }
        let rest = &rest[gap..];
        let (name, rest) = match escaped(rest) {
            Some(length) => (&rest[..length], &rest[length..]),
            None => match identifier(rest) {
                Some(length) => (&rest[..length], &rest[length..]),
                None => continue,
            },
        };
        let rest = &rest[blanks(rest)..];
        let Some(rest) = rest.strip_prefix('(') else {
            continue;
        };
        // Greedy `(.*)\);[ \t]*$`: the line ends with `);` and blanks.
        let Some(list) = rest.trim_end_matches([' ', '\t']).strip_suffix(");") else {
            continue;
        };
        let instance = Instance {
            master: master.to_owned(),
            pins: connections(list),
        };
        let key = base(name);
        match index.get(key) {
            Some(at) => out.0[*at].1 = instance,
            None => {
                index.insert(key, out.0.len());
                out.0.push((key.to_owned(), instance));
            }
        }
    }
    out
}
