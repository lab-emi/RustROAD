//! A strict JSON reader with CPython's value model.
use crate::{Dict, Value};

/// What a repeated object key means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duplicates {
    /// `json.loads` default: the last value wins and keeps the first position.
    LastWins,
    /// An `object_pairs_hook` that rejects repeated keys.
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("JSON syntax error at byte {0}")]
    Syntax(usize),
    #[error("JSON ends inside a value")]
    Truncated,
    #[error("JSON nesting budget exceeded")]
    Depth,
    #[error("duplicate JSON object key")]
    DuplicateKey,
    #[error("JSON integer outside the supported range")]
    IntegerRange,
    #[error("unpaired UTF-16 surrogate in a JSON string")]
    Surrogate,
    #[error("trailing data after the JSON value at byte {0}")]
    Trailing(usize),
}

const MAX_DEPTH: usize = 256;

/// `json.loads(text)` for strict JSON (no NaN/Infinity constants).
///
/// # Errors
/// Any deviation from RFC 8259, nesting deeper than 256, or an integer
/// outside `i128`.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    parse_with(text, Duplicates::LastWins)
}

/// [`parse`] with an explicit duplicate-key policy.
///
/// # Errors
/// As [`parse`], plus `DuplicateKey` under [`Duplicates::Reject`].
pub fn parse_with(text: &str, duplicates: Duplicates) -> Result<Value, ParseError> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        text,
        at: 0,
        duplicates,
    };
    reader.space();
    let value = reader.value(0)?;
    reader.space();
    if reader.at != reader.bytes.len() {
        return Err(ParseError::Trailing(reader.at));
    }
    Ok(value)
}

/// `JSONDecoder.raw_decode(text, start)`: one value starting exactly at byte
/// `start` (no leading whitespace), returning it and the byte after it.
///
/// A value that runs into the end of `text` is `Truncated`; a number that
/// ends exactly at the end of `text` is returned (the caller decides whether
/// more input could extend it).
///
/// # Errors
/// As [`parse_with`].
pub fn parse_prefix(
    text: &str,
    start: usize,
    duplicates: Duplicates,
) -> Result<(Value, usize), ParseError> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        text,
        at: start,
        duplicates,
    };
    let value = reader.value(0)?;
    Ok((value, reader.at))
}

struct Reader<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
    duplicates: Duplicates,
}

impl Reader<'_> {
    fn space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.at) {
            self.at += 1;
        }
    }
    fn peek(&self) -> Result<u8, ParseError> {
        self.bytes
            .get(self.at)
            .copied()
            .ok_or(ParseError::Truncated)
    }
    fn literal(&mut self, word: &str, value: Value) -> Result<Value, ParseError> {
        let rest = &self.bytes[self.at..];
        if rest.starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else if word.as_bytes().starts_with(rest) {
            Err(ParseError::Truncated)
        } else {
            Err(ParseError::Syntax(self.at))
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::Depth);
        }
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::Str),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(ParseError::Syntax(self.at)),
        }
    }
    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.at += 1;
        let mut dict = Dict::new();
        let mut index: Option<std::collections::HashMap<String, ()>> = None;
        self.space();
        if self.peek()? == b'}' {
            self.at += 1;
            return Ok(Value::Dict(dict));
        }
        loop {
            self.space();
            if self.peek()? != b'"' {
                return Err(ParseError::Syntax(self.at));
            }
            let key = self.string()?;
            self.space();
            if self.peek()? != b':' {
                return Err(ParseError::Syntax(self.at));
            }
            self.at += 1;
            self.space();
            let value = self.value(depth + 1)?;
            // A linear probe is fine for small objects; large ones get an index.
            let known = match &mut index {
                Some(index) => index.insert(key.clone(), ()).is_some(),
                None => dict.contains_key(&key),
            };
            if known {
                if self.duplicates == Duplicates::Reject {
                    return Err(ParseError::DuplicateKey);
                }
                dict.insert(key, value);
            } else {
                dict.push_unchecked(key, value);
                if index.is_none() && dict.len() >= 32 {
                    index = Some(dict.keys().map(|k| (k.to_owned(), ())).collect());
                }
            }
            self.space();
            match self.peek()? {
                b',' => self.at += 1,
                b'}' => {
                    self.at += 1;
                    return Ok(Value::Dict(dict));
                }
                _ => return Err(ParseError::Syntax(self.at)),
            }
        }
    }
    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.peek()? == b']' {
            self.at += 1;
            return Ok(Value::List(items));
        }
        loop {
            self.space();
            items.push(self.value(depth + 1)?);
            self.space();
            match self.peek()? {
                b',' => self.at += 1,
                b']' => {
                    self.at += 1;
                    return Ok(Value::List(items));
                }
                _ => return Err(ParseError::Syntax(self.at)),
            }
        }
    }
    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or(ParseError::Truncated)?;
        let mut unit = 0u32;
        for digit in digits {
            unit = unit * 16
                + match digit {
                    b'0'..=b'9' => u32::from(digit - b'0'),
                    b'a'..=b'f' => u32::from(digit - b'a') + 10,
                    b'A'..=b'F' => u32::from(digit - b'A') + 10,
                    _ => return Err(ParseError::Syntax(self.at)),
                };
        }
        self.at += 4;
        Ok(unit)
    }
    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while let Some(&byte) = self.bytes.get(self.at) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.at += 1;
            }
            out.push_str(&self.text[start..self.at]);
            match self.peek()? {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self.peek()?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            let scalar = if (0xD800..0xDC00).contains(&unit) {
                                if self.bytes.get(self.at..self.at + 2) != Some(b"\\u") {
                                    return Err(ParseError::Surrogate);
                                }
                                self.at += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(ParseError::Surrogate);
                                }
                                0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                unit
                            };
                            out.push(char::from_u32(scalar).ok_or(ParseError::Surrogate)?);
                        }
                        _ => return Err(ParseError::Syntax(self.at - 1)),
                    }
                }
                _ => return Err(ParseError::Syntax(self.at)),
            }
        }
    }
    fn digits(&mut self) -> usize {
        let start = self.at;
        while let Some(b'0'..=b'9') = self.bytes.get(self.at) {
            self.at += 1;
        }
        self.at - start
    }
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.at;
        if self.bytes[self.at] == b'-' {
            self.at += 1;
        }
        match self.bytes.get(self.at) {
            None => return Err(ParseError::Truncated),
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            Some(_) => return Err(ParseError::Syntax(self.at)),
        }
        // CPython's scanner takes the fraction and the exponent only when
        // they are complete; an incomplete tail is left for the caller, who
        // then fails on the unexpected character.
        let mut float = false;
        if self.bytes.get(self.at) == Some(&b'.')
            && matches!(self.bytes.get(self.at + 1), Some(b'0'..=b'9'))
        {
            self.at += 1;
            self.digits();
            float = true;
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.at) {
            let mut probe = self.at + 1;
            if let Some(b'+' | b'-') = self.bytes.get(probe) {
                probe += 1;
            }
            if matches!(self.bytes.get(probe), Some(b'0'..=b'9')) {
                self.at = probe;
                self.digits();
                float = true;
            }
        }
        let text = &self.text[start..self.at];
        if float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| ParseError::Syntax(start))
        } else {
            text.parse::<i128>()
                .map(Value::Int)
                .map_err(|_| ParseError::IntegerRange)
        }
    }
}
