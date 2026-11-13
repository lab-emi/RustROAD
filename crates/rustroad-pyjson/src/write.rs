//! `json.dumps` layouts.
use crate::{Value, float_repr};

/// The separators and line structure of one `json.dumps` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// `json.dumps(v)`: one line, `", "` and `": "`.
    Line,
    /// `json.dumps(v, indent=n)`: one item per line, `","` and `": "`.
    Indent(usize),
    /// `json.dumps(v, ensure_ascii=False, separators=(',', ':'))`: the
    /// fingerprint form; non-ASCII text is written as UTF-8.
    Compact,
    /// `json.dumps(v, ensure_ascii=False)`: [`Layout::Line`] with non-ASCII
    /// text written as UTF-8.
    LineUtf8,
}
impl Layout {
    fn ascii(self) -> bool {
        matches!(self, Self::Line | Self::Indent(_))
    }
}

/// Serialize `value`; `None` if it holds a NaN or an infinity
/// (`allow_nan=False`).
#[must_use]
pub fn dumps(value: &Value, layout: Layout) -> Option<String> {
    let mut text = String::new();
    dumps_into(&mut text, value, layout).then_some(text)
}

/// Append the serialization of `value` to `out`; returns `false` (leaving a
/// partial tail) if a number is not finite.
pub fn dumps_into(out: &mut String, value: &Value, layout: Layout) -> bool {
    emit(out, value, layout, 0)
}

fn newline(out: &mut String, width: usize, level: usize) {
    out.push('\n');
    for _ in 0..width * level {
        out.push(' ');
    }
}

fn emit(out: &mut String, value: &Value, layout: Layout, level: usize) -> bool {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(number) => out.push_str(&number.to_string()),
        Value::Float(number) => match float_repr(*number) {
            Some(text) => out.push_str(&text),
            None => return false,
        },
        Value::Str(text) => string(out, text, layout.ascii()),
        Value::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return true;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                separator(out, layout, level + 1, index == 0);
                if !emit(out, item, layout, level + 1) {
                    return false;
                }
            }
            close(out, layout, level);
            out.push(']');
        }
        Value::Dict(dict) => {
            if dict.is_empty() {
                out.push_str("{}");
                return true;
            }
            out.push('{');
            for (index, (key, item)) in dict.iter().enumerate() {
                separator(out, layout, level + 1, index == 0);
                string(out, key, layout.ascii());
                out.push_str(if layout == Layout::Compact { ":" } else { ": " });
                if !emit(out, item, layout, level + 1) {
                    return false;
                }
            }
            close(out, layout, level);
            out.push('}');
        }
    }
    true
}

fn separator(out: &mut String, layout: Layout, level: usize, first: bool) {
    match layout {
        Layout::Line | Layout::LineUtf8 => {
            if !first {
                out.push_str(", ");
            }
        }
        Layout::Compact => {
            if !first {
                out.push(',');
            }
        }
        Layout::Indent(width) => {
            if !first {
                out.push(',');
            }
            newline(out, width, level);
        }
    }
}

fn close(out: &mut String, layout: Layout, level: usize) {
    if let Layout::Indent(width) = layout {
        newline(out, width, level);
    }
}

fn string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            // `ensure_ascii` escapes everything outside space..=tilde, DEL included.
            c if ensure_ascii && (c as u32) >= 0x7f => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
