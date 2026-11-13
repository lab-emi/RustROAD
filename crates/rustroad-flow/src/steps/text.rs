//! Python `str` operations whose exact behaviour reaches step outputs.

/// `str.isspace()` for one character: Unicode White_Space plus the four
/// information separators U+001C..U+001F.
pub(crate) fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `text.split()`.
pub(crate) fn split(text: &str) -> Vec<&str> {
    text.split(is_space)
        .filter(|part| !part.is_empty())
        .collect()
}

/// `text.strip()`.
pub(crate) fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `text.splitlines()`: every Python line boundary, `\r\n` as one.
pub(crate) fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&text[start..at]);
        let mut end = at + c.len_utf8();
        if c == '\r' && matches!(chars.peek(), Some((_, '\n'))) {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `float(text)` for the spellings PDK data files use: decimal digits with an
/// optional sign, fraction and exponent, single underscores between digits,
/// and `inf`/`infinity`/`nan` in any case. Surrounding whitespace is the
/// caller's (tokens are already split).
pub(crate) fn float(text: &str) -> Option<f64> {
    let cleaned;
    let text = if text.contains('_') {
        let bytes = text.as_bytes();
        for (at, byte) in bytes.iter().enumerate() {
            if *byte == b'_' {
                let digit = |i: Option<&u8>| i.is_some_and(u8::is_ascii_digit);
                if at == 0 || !digit(bytes.get(at - 1)) || !digit(bytes.get(at + 1)) {
                    return None;
                }
            }
        }
        cleaned = text.replace('_', "");
        cleaned.as_str()
    } else {
        text
    };
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    let word = body.to_ascii_lowercase();
    if matches!(word.as_str(), "inf" | "infinity" | "nan") {
        return text.parse().ok();
    }
    // Rust accepts exactly Python's decimal grammar once the words are handled.
    if body.is_empty()
        || !body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    text.parse().ok()
}

/// `int(text)` for ASCII decimal digits with an optional sign and single
/// underscores between digits.
pub(crate) fn int(text: &str) -> Option<i64> {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    if !body.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return None;
    }
    let digits = body.replace('_', "");
    let value: i64 = digits.parse().ok()?;
    Some(if text.starts_with('-') { -value } else { value })
}

/// All ASCII digits (`\d+` on the data files this crate reads).
pub(crate) fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `format(value, 'g')`: six significant digits, trailing zeros removed,
/// exponent form outside `1e-4 <= |value| < 1e6`.
pub(crate) fn format_g(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.into();
    }
    if !value.is_finite() {
        return if value.is_nan() {
            "nan".into()
        } else if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trim = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text.to_owned()
        }
    };
    if (-4..6).contains(&exponent) {
        let decimals = (5 - exponent).max(0) as usize;
        trim(&format!("{value:.decimals$}"))
    } else {
        format!(
            "{}e{}{:02}",
            trim(mantissa),
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_string_operations() {
        assert_eq!(split(" a\tb\u{1f}c \u{a0}d\n"), ["a", "b", "c", "d"]);
        assert_eq!(strip("\u{1c} x y \r\n"), "x y");
        assert_eq!(
            splitlines("a\nb\r\nc\rd\u{c}e\u{2028}f\n"),
            ["a", "b", "c", "d", "e", "f"]
        );
        assert_eq!(splitlines("\n\nx"), ["", "", "x"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(float("1_000.5"), Some(1000.5));
        assert_eq!(float("+.5e1"), Some(5.0));
        assert_eq!(float("5."), Some(5.0));
        assert_eq!(float("-Inf"), Some(f64::NEG_INFINITY));
        assert_eq!(float("1__0"), None);
        assert_eq!(float("0x10"), None);
        assert_eq!(float(""), None);
        assert_eq!(int("-12"), Some(-12));
        assert_eq!(int("1_0"), Some(10));
        assert_eq!(int("1.0"), None);
        assert_eq!(int("_1"), None);
    }

    #[test]
    fn general_format_matches_python() {
        // format(x, 'g') in CPython 3.13.
        for (value, text) in [
            (0.0, "0"),
            (1.0, "1"),
            (4.5, "4.5"),
            (15000.0, "15000"),
            (123456.0, "123456"),
            (1234567.0, "1.23457e+06"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (0.000123456789, "0.000123457"),
            (2.2, "2.2"),
            (999999.5, "1e+06"),
            (0.1 + 0.2, "0.3"),
            (-22.0, "-22"),
            (1.5e-7, "1.5e-07"),
        ] {
            assert_eq!(format_g(value), text);
        }
    }
}
