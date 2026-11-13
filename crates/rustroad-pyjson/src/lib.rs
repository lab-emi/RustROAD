//! Order-preserving JSON values whose text form equals CPython's `json` module.
//!
//! The flow records this crate serves (step inputs, lineage, review and repair
//! results) are consumed by tools that pin them by SHA-256, so their bytes must
//! not depend on which implementation wrote them. The reference writer is
//! CPython 3 `json.dumps`: insertion-ordered objects, `repr(float)` numbers,
//! `ensure_ascii` escapes, and the `indent` / separator layouts. The reader
//! keeps object order and the integer/float distinction of the source text.
//!
//! Bounded by the caller: every reader takes complete, already size-checked
//! text and a nesting budget; nothing here reads a file without a byte cap.
mod float;
mod read;
mod write;

pub use float::float_repr;
pub use read::{Duplicates, ParseError, parse, parse_prefix, parse_with};
pub use write::{Layout, dumps, dumps_into};

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// A JSON value as CPython's decoder models it.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Dict(Dict),
}

/// An insertion-ordered string-keyed object (a Python `dict`).
#[derive(Clone, Debug, Default)]
pub struct Dict {
    entries: Vec<(String, Value)>,
}

impl Dict {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    /// Python `d[key] = value`: an existing key keeps its position.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        let (key, value) = (key.into(), value.into());
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key, value)),
        }
    }
    /// Builder form of [`Dict::insert`].
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.insert(key, value);
        self
    }
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(at).1)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries.iter().map(|(_, v)| v)
    }
    /// Python `set(d) == {names...}`.
    #[must_use]
    pub fn has_exactly(&self, names: &[&str]) -> bool {
        self.len() == names.len() && names.iter().all(|name| self.contains_key(name))
    }
    /// Python `{names...} <= set(d)`.
    #[must_use]
    pub fn has_all(&self, names: &[&str]) -> bool {
        names.iter().all(|name| self.contains_key(name))
    }
    pub(crate) fn push_unchecked(&mut self, key: String, value: Value) {
        self.entries.push((key, value));
    }
}

impl FromIterator<(String, Value)> for Dict {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut dict = Self::new();
        for (key, value) in iter {
            dict.insert(key, value);
        }
        dict
    }
}

/// Python `==` on decoded JSON: object order is irrelevant and numbers compare
/// by value across `bool`, `int` and `float` (`True == 1 == 1.0`).
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::List(a), Self::List(b)) => a == b,
            (Self::Dict(a), Self::Dict(b)) => a == b,
            _ => match (self.number(), other.number()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }
}
impl PartialEq for Dict {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .entries
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

#[derive(Clone, Copy, Debug)]
enum Number {
    Int(i128),
    Float(f64),
}
impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a == b,
            (Self::Int(i), Self::Float(f)) | (Self::Float(f), Self::Int(i)) => {
                // Exact, as Python compares: only an integral float in range can match.
                f.is_finite()
                    && f.fract() == 0.0
                    && f.abs() < 1.7e38
                    && (f as i128) == i
                    && (i as f64) == f
            }
        }
    }
}

impl Value {
    fn number(&self) -> Option<Number> {
        match self {
            Self::Bool(b) => Some(Number::Int(i128::from(*b))),
            Self::Int(i) => Some(Number::Int(*i)),
            Self::Float(f) => Some(Number::Float(*f)),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }
    /// A JSON integer (`type(x) is int`); booleans and floats are not integers.
    #[must_use]
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }
    /// A JSON integer or float as `f64` (`type(x) in (int, float)`).
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(i) => Some(*i as f64),
            Self::Float(f) => Some(*f),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Self::Dict(dict) => Some(dict),
            _ => None,
        }
    }
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
    /// Python truthiness of a decoded JSON value.
    #[must_use]
    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Int(i) => *i != 0,
            Self::Float(f) => *f != 0.0,
            Self::Str(text) => !text.is_empty(),
            Self::List(items) => !items.is_empty(),
            Self::Dict(dict) => !dict.is_empty(),
        }
    }
    /// `value[key]` on an object; `None` for a missing key or another type.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict().and_then(|dict| dict.get(key))
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}
impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}
impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::Str(value.clone())
    }
}
impl From<Dict> for Value {
    fn from(value: Dict) -> Self {
        Self::Dict(value)
    }
}
impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Self::List(value)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}
impl From<&Value> for Value {
    fn from(value: &Value) -> Self {
        value.clone()
    }
}
macro_rules! from_integer {
    ($($kind:ty),*) => {$(
        impl From<$kind> for Value {
            fn from(value: $kind) -> Self {
                Self::Int(value as i128)
            }
        }
    )*};
}
from_integer!(i32, i64, u32, u64, usize, i128);

/// Build a [`Value::List`] from anything convertible.
pub fn list<T: Into<Value>>(items: impl IntoIterator<Item = T>) -> Value {
    Value::List(items.into_iter().map(Into::into).collect())
}

/// `{"path": ..., "sha256": ..., "bytes": ...}` of a regular file, the record
/// every flow step uses to pin a file. The path is canonical (symlinks
/// resolved), as Python's `Path.resolve(strict=True)` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileId {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl FileId {
    /// Hash `path` with a streaming reader; fails on a non-regular file, a
    /// file over `cap` bytes, a path that is not UTF-8, or a file that
    /// changed size while it was read.
    ///
    /// # Errors
    /// I/O errors and the conditions above, as a plain message.
    pub fn of(path: &Path, cap: u64) -> Result<Self, String> {
        let resolved =
            std::fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let text = resolved
            .to_str()
            .ok_or_else(|| format!("{}: path is not UTF-8", resolved.display()))?
            .to_owned();
        let mut file = File::open(&resolved).map_err(|error| format!("{text}: {error}"))?;
        let before = file
            .metadata()
            .map_err(|error| format!("{text}: {error}"))?;
        if !before.is_file() {
            return Err(format!("{text}: not a regular file"));
        }
        if before.len() > cap {
            return Err(format!("{text}: file byte budget"));
        }
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 16];
        let mut total = 0u64;
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("{text}: {error}"))?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > cap {
                return Err(format!("{text}: file byte budget"));
            }
            hasher.update(&buffer[..count]);
        }
        let after = file
            .metadata()
            .map_err(|error| format!("{text}: {error}"))?;
        if after.len() != total || before.len() != total {
            return Err(format!("{text}: file changed while hashing"));
        }
        Ok(Self {
            path: text,
            sha256: hex(&hasher.finalize()),
            bytes: total,
        })
    }
    /// The record as an object in the canonical key order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Dict(
            Dict::new()
                .with("path", self.path.as_str())
                .with("sha256", self.sha256.as_str())
                .with("bytes", self.bytes),
        )
    }
    /// Decode a record with exactly the three keys and the expected types.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let dict = value.as_dict()?;
        if !dict.has_exactly(&["path", "sha256", "bytes"]) {
            return None;
        }
        let bytes = u64::try_from(dict.get("bytes")?.as_int()?).ok()?;
        Some(Self {
            path: dict.get("path")?.as_str()?.to_owned(),
            sha256: dict.get("sha256")?.as_str()?.to_owned(),
            bytes,
        })
    }
}

/// Lowercase hexadecimal text of a digest.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 15)] as char);
    }
    text
}

/// SHA-256 of a byte string as lowercase hexadecimal.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `[a-f0-9]{64}`.
#[must_use]
pub fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `str(PurePosixPath(text))`: repeated slashes and `.` components collapse,
/// a trailing slash is dropped, exactly two leading slashes are kept, and
/// `..` is left alone.
#[must_use]
pub fn pure_path(text: &str) -> String {
    if text.is_empty() {
        return ".".into();
    }
    let root = if text.starts_with("//") && !text.starts_with("///") {
        "//"
    } else if text.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<&str> = text
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = parts.join("/");
    if root.is_empty() && joined.is_empty() {
        ".".into()
    } else {
        format!("{root}{joined}")
    }
}
