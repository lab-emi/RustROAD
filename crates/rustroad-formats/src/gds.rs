//! Clean-room GDSII Stream Format codec.
//!
//! Written only from the public record-format description: a stream is a
//! sequence of big-endian records, each `u16 total length` (even, >= 4),
//! `u8 record type`, `u8 data type`, payload. Reals are 8-byte excess-64,
//! base-16 floating point values (sign bit, 7-bit exponent, 56-bit mantissa).
//! No other implementation was consulted.
//!
//! Resource contract (docs/gds-budget.md): the reader borrows one input
//! buffer, keeps one record of lookahead and enforces explicit caps on bytes,
//! records, structures, elements, points per element, retained points, names,
//! strings, properties and reference depth. Malformed or hostile input fails
//! with a coded [`GdsError`]; nothing is guessed and nothing panics. Index mode
//! validates the complete grammar but retains only structure names, byte
//! ranges and child references, so verbatim copies cost O(input) memory.
//!
//! Unsupported record types (obsolete/unreleased: TEXTNODE, SPACING,
//! UINTEGER, USTRING, STYPTABLE, STRTYPE, ELKEY, LINKTYPE, LINKKEYS, TAPENUM,
//! TAPECODE, RESERVED) are explicit `GDS_UNSUPPORTED` errors; a data type that
//! does not match the record type (including 4-byte reals) is `GDS_SYNTAX`.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::ops::Range;

/// Record type codes from the public GDSII stream description.
pub mod record {
    pub const HEADER: u8 = 0x00;
    pub const BGNLIB: u8 = 0x01;
    pub const LIBNAME: u8 = 0x02;
    pub const UNITS: u8 = 0x03;
    pub const ENDLIB: u8 = 0x04;
    pub const BGNSTR: u8 = 0x05;
    pub const STRNAME: u8 = 0x06;
    pub const ENDSTR: u8 = 0x07;
    pub const BOUNDARY: u8 = 0x08;
    pub const PATH: u8 = 0x09;
    pub const SREF: u8 = 0x0A;
    pub const AREF: u8 = 0x0B;
    pub const TEXT: u8 = 0x0C;
    pub const LAYER: u8 = 0x0D;
    pub const DATATYPE: u8 = 0x0E;
    pub const WIDTH: u8 = 0x0F;
    pub const XY: u8 = 0x10;
    pub const ENDEL: u8 = 0x11;
    pub const SNAME: u8 = 0x12;
    pub const COLROW: u8 = 0x13;
    pub const NODE: u8 = 0x15;
    pub const TEXTTYPE: u8 = 0x16;
    pub const PRESENTATION: u8 = 0x17;
    pub const STRING: u8 = 0x19;
    pub const STRANS: u8 = 0x1A;
    pub const MAG: u8 = 0x1B;
    pub const ANGLE: u8 = 0x1C;
    pub const REFLIBS: u8 = 0x1F;
    pub const FONTS: u8 = 0x20;
    pub const PATHTYPE: u8 = 0x21;
    pub const GENERATIONS: u8 = 0x22;
    pub const ATTRTABLE: u8 = 0x23;
    pub const ELFLAGS: u8 = 0x26;
    pub const NODETYPE: u8 = 0x2A;
    pub const PROPATTR: u8 = 0x2B;
    pub const PROPVALUE: u8 = 0x2C;
    pub const BOX: u8 = 0x2D;
    pub const BOXTYPE: u8 = 0x2E;
    pub const PLEX: u8 = 0x2F;
    pub const BGNEXTN: u8 = 0x30;
    pub const ENDEXTN: u8 = 0x31;
    pub const STRCLASS: u8 = 0x34;
    pub const FORMAT: u8 = 0x36;
    pub const MASK: u8 = 0x37;
    pub const ENDMASKS: u8 = 0x38;
    pub const LIBDIRSIZE: u8 = 0x39;
    pub const SRFNAME: u8 = 0x3A;
    pub const LIBSECUR: u8 = 0x3B;
}
use record as rt;

const NO_DATA: u8 = 0;
const BIT_ARRAY: u8 = 1;
const INT16: u8 = 2;
const INT32: u8 = 3;
const REAL8: u8 = 5;
const ASCII: u8 = 6;
/// Largest even record length representable in the 16-bit length field.
pub const MAX_RECORD_BYTES: usize = 65534;

/// Expected data type and display name of every supported record type.
fn spec(rtype: u8) -> Option<(u8, &'static str)> {
    Some(match rtype {
        rt::HEADER => (INT16, "HEADER"),
        rt::BGNLIB => (INT16, "BGNLIB"),
        rt::LIBNAME => (ASCII, "LIBNAME"),
        rt::UNITS => (REAL8, "UNITS"),
        rt::ENDLIB => (NO_DATA, "ENDLIB"),
        rt::BGNSTR => (INT16, "BGNSTR"),
        rt::STRNAME => (ASCII, "STRNAME"),
        rt::ENDSTR => (NO_DATA, "ENDSTR"),
        rt::BOUNDARY => (NO_DATA, "BOUNDARY"),
        rt::PATH => (NO_DATA, "PATH"),
        rt::SREF => (NO_DATA, "SREF"),
        rt::AREF => (NO_DATA, "AREF"),
        rt::TEXT => (NO_DATA, "TEXT"),
        rt::LAYER => (INT16, "LAYER"),
        rt::DATATYPE => (INT16, "DATATYPE"),
        rt::WIDTH => (INT32, "WIDTH"),
        rt::XY => (INT32, "XY"),
        rt::ENDEL => (NO_DATA, "ENDEL"),
        rt::SNAME => (ASCII, "SNAME"),
        rt::COLROW => (INT16, "COLROW"),
        rt::NODE => (NO_DATA, "NODE"),
        rt::TEXTTYPE => (INT16, "TEXTTYPE"),
        rt::PRESENTATION => (BIT_ARRAY, "PRESENTATION"),
        rt::STRING => (ASCII, "STRING"),
        rt::STRANS => (BIT_ARRAY, "STRANS"),
        rt::MAG => (REAL8, "MAG"),
        rt::ANGLE => (REAL8, "ANGLE"),
        rt::REFLIBS => (ASCII, "REFLIBS"),
        rt::FONTS => (ASCII, "FONTS"),
        rt::PATHTYPE => (INT16, "PATHTYPE"),
        rt::GENERATIONS => (INT16, "GENERATIONS"),
        rt::ATTRTABLE => (ASCII, "ATTRTABLE"),
        rt::ELFLAGS => (BIT_ARRAY, "ELFLAGS"),
        rt::NODETYPE => (INT16, "NODETYPE"),
        rt::PROPATTR => (INT16, "PROPATTR"),
        rt::PROPVALUE => (ASCII, "PROPVALUE"),
        rt::BOX => (NO_DATA, "BOX"),
        rt::BOXTYPE => (INT16, "BOXTYPE"),
        rt::PLEX => (INT32, "PLEX"),
        rt::BGNEXTN => (INT32, "BGNEXTN"),
        rt::ENDEXTN => (INT32, "ENDEXTN"),
        rt::STRCLASS => (BIT_ARRAY, "STRCLASS"),
        rt::FORMAT => (INT16, "FORMAT"),
        rt::MASK => (ASCII, "MASK"),
        rt::ENDMASKS => (NO_DATA, "ENDMASKS"),
        rt::LIBDIRSIZE => (INT16, "LIBDIRSIZE"),
        rt::SRFNAME => (ASCII, "SRFNAME"),
        rt::LIBSECUR => (INT16, "LIBSECUR"),
        _ => return None,
    })
}
/// Display name for diagnostics; unsupported codes are named where known.
pub fn record_name(rtype: u8) -> &'static str {
    match spec(rtype) {
        Some((_, name)) => name,
        None => match rtype {
            0x14 => "TEXTNODE",
            0x18 => "SPACING",
            0x1D => "UINTEGER",
            0x1E => "USTRING",
            0x24 => "STYPTABLE",
            0x25 => "STRTYPE",
            0x27 => "ELKEY",
            0x28 => "LINKTYPE",
            0x29 => "LINKKEYS",
            0x32 => "TAPENUM",
            0x33 => "TAPECODE",
            0x35 => "RESERVED",
            _ => "UNKNOWN",
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message} (byte offset {offset})")]
pub struct GdsError {
    pub code: &'static str,
    pub offset: usize,
    pub message: String,
}
pub type Result<T> = std::result::Result<T, GdsError>;
fn error(code: &'static str, offset: usize, message: impl Into<String>) -> GdsError {
    GdsError {
        code,
        offset,
        message: message.into(),
    }
}

/// Explicit work and memory caps. Defaults fit real PDK cell libraries with
/// large margins; see docs/gds-budget.md for the peak-memory derivation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GdsLimits {
    pub max_bytes: usize,
    pub max_records: usize,
    pub max_structures: usize,
    pub max_elements: usize,
    pub max_points_per_element: usize,
    pub max_retained_points: usize,
    pub max_name_bytes: usize,
    pub max_string_bytes: usize,
    pub max_properties_per_element: usize,
    pub max_references_per_structure: usize,
    pub max_depth: usize,
}
impl Default for GdsLimits {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_records: 16_000_000,
            max_structures: 200_000,
            max_elements: 8_000_000,
            max_points_per_element: 8191,
            max_retained_points: 64_000_000,
            max_name_bytes: 1024,
            max_string_bytes: 1024,
            max_properties_per_element: 256,
            max_references_per_structure: 100_000,
            max_depth: 64,
        }
    }
}

/// An 8-byte GDSII real kept as its exact stream bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Real8(pub [u8; 8]);
impl Real8 {
    pub const ZERO: Self = Self([0; 8]);
    /// Exact conversion: every finite binary64 value inside the GDS exponent
    /// range has an exact base-16 representation (53 <= 56 - 3 mantissa bits).
    pub fn from_f64(value: f64) -> Result<Self> {
        if !value.is_finite() {
            return Err(error("GDS_REAL", 0, "non-finite real"));
        }
        if value == 0.0 {
            return Ok(Self::ZERO);
        }
        let bits = value.to_bits();
        let negative = bits >> 63 == 1;
        let biased = ((bits >> 52) & 0x7ff) as i32;
        let fraction = bits & ((1u64 << 52) - 1);
        let (mantissa, exponent) = if biased == 0 {
            (fraction, -1074)
        } else {
            (fraction | (1u64 << 52), biased - 1075)
        };
        // Normalize to exactly 53 significant bits: value = m * 2^e.
        let shift = mantissa.leading_zeros() as i32 - 11;
        let m = mantissa << shift;
        let e = exponent - shift;
        // value = (m << s) * 16^(E - 64) / 2^56 with s in 0..=3.
        let s = (e + 56).rem_euclid(4);
        let excess = (e - s + 56) / 4 + 64;
        if !(0..=127).contains(&excess) {
            return Err(error("GDS_REAL", 0, "real outside GDSII exponent range"));
        }
        let m56 = m << s;
        let mut out = [0u8; 8];
        out[0] = (u8::from(negative) << 7) | excess as u8;
        out[1..].copy_from_slice(&m56.to_be_bytes()[1..]);
        Ok(Self(out))
    }
    /// Exact value as `(negative, mantissa, exponent)`, value = m * 2^exponent.
    pub fn parts(self) -> (bool, u64, i32) {
        let mut m = [0u8; 8];
        m[1..].copy_from_slice(&self.0[1..]);
        (
            self.0[0] & 0x80 != 0,
            u64::from_be_bytes(m),
            4 * (i32::from(self.0[0] & 0x7f) - 64) - 56,
        )
    }
    /// Correctly rounded (nearest, ties to even) binary64 value. The full GDS
    /// exponent range maps to normal binary64 values, so only the up to three
    /// extra mantissa bits can round.
    pub fn to_f64(self) -> f64 {
        let (negative, mantissa, exponent) = self.parts();
        if mantissa == 0 {
            return 0.0;
        }
        let bits = 64 - mantissa.leading_zeros() as i32;
        let (mut q, mut k) = if bits > 53 {
            let shift = bits - 53;
            let q = mantissa >> shift;
            let rest = mantissa & ((1u64 << shift) - 1);
            let half = 1u64 << (shift - 1);
            let q = if rest > half || (rest == half && q & 1 == 1) {
                q + 1
            } else {
                q
            };
            (q, exponent + shift)
        } else {
            (mantissa << (53 - bits), exponent - (53 - bits))
        };
        if q == 1u64 << 53 {
            q >>= 1;
            k += 1;
        }
        let field = (k + 52 + 1023) as u64;
        f64::from_bits((u64::from(negative) << 63) | (field << 52) | (q & ((1u64 << 52) - 1)))
    }
    /// True when the stored value is exactly a binary64 value in canonical form.
    pub fn is_exact_f64(self) -> bool {
        Self::from_f64(self.to_f64()) == Ok(self)
    }
}

/// Decoded record payload. ASCII payloads keep their exact stream bytes,
/// including any NUL padding, so header records copy back verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    None,
    Bits(u16),
    I16(Vec<i16>),
    I32(Vec<i32>),
    Real(Vec<Real8>),
    Ascii(Vec<u8>),
}
impl Payload {
    /// ASCII payload padded with one NUL to an even length.
    pub fn text(value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        if !bytes.len().is_multiple_of(2) {
            bytes.push(0);
        }
        Self::Ascii(bytes)
    }
    fn dtype(&self) -> u8 {
        match self {
            Self::None => NO_DATA,
            Self::Bits(_) => BIT_ARRAY,
            Self::I16(_) => INT16,
            Self::I32(_) => INT32,
            Self::Real(_) => REAL8,
            Self::Ascii(_) => ASCII,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub rtype: u8,
    pub payload: Payload,
}

/// One framed record borrowed from the input buffer.
#[derive(Clone, Copy, Debug)]
pub struct RawRecord<'a> {
    pub offset: usize,
    pub rtype: u8,
    pub dtype: u8,
    pub data: &'a [u8],
}
impl<'a> RawRecord<'a> {
    pub fn end(&self) -> usize {
        self.offset + 4 + self.data.len()
    }
    fn fail(&self, code: &'static str, message: impl Into<String>) -> GdsError {
        error(code, self.offset, message)
    }
    fn i16s(&self) -> impl Iterator<Item = i16> + 'a {
        self.data
            .chunks_exact(2)
            .map(|c| i16::from_be_bytes([c[0], c[1]]))
    }
    fn i32s(&self) -> impl Iterator<Item = i32> + 'a {
        self.data
            .chunks_exact(4)
            .map(|c| i32::from_be_bytes([c[0], c[1], c[2], c[3]]))
    }
    fn reals(&self) -> impl Iterator<Item = Real8> + 'a {
        self.data
            .chunks_exact(8)
            .map(|c| Real8(c.try_into().expect("exact chunk")))
    }
    fn i16_array<const N: usize>(&self) -> Result<[i16; N]> {
        let values: Vec<i16> = self.i16s().collect();
        values.try_into().map_err(|_| {
            self.fail(
                "GDS_SYNTAX",
                format!("{} requires {N} two-byte values", record_name(self.rtype)),
            )
        })
    }
    fn i16_one(&self) -> Result<i16> {
        Ok(self.i16_array::<1>()?[0])
    }
    fn i32_one(&self) -> Result<i32> {
        let values: Vec<i32> = self.i32s().collect();
        match values.as_slice() {
            [v] => Ok(*v),
            _ => Err(self.fail(
                "GDS_SYNTAX",
                format!("{} requires one four-byte value", record_name(self.rtype)),
            )),
        }
    }
    fn real_one(&self) -> Result<Real8> {
        let values: Vec<Real8> = self.reals().collect();
        match values.as_slice() {
            [v] => Ok(*v),
            _ => Err(self.fail(
                "GDS_SYNTAX",
                format!("{} requires one real", record_name(self.rtype)),
            )),
        }
    }
    fn bits(&self) -> u16 {
        u16::from_be_bytes([self.data[0], self.data[1]])
    }
    /// The payload with trailing NUL padding removed; interior NULs are errors.
    fn trimmed_ascii(&self) -> Result<&'a [u8]> {
        let mut end = self.data.len();
        while end > 0 && self.data[end - 1] == 0 {
            end -= 1;
        }
        let text = &self.data[..end];
        if text.contains(&0) {
            return Err(self.fail("GDS_SYNTAX", "interior NUL in ASCII record"));
        }
        Ok(text)
    }
    /// Structure, reference and library names: printable non-space ASCII.
    fn name(&self, limits: &GdsLimits) -> Result<String> {
        let text = self.trimmed_ascii()?;
        if text.is_empty() || text.len() > limits.max_name_bytes {
            return Err(self.fail("GDS_NAME", "empty or over-long name"));
        }
        if !text.iter().all(|b| (0x21..=0x7e).contains(b)) {
            return Err(self.fail("GDS_NAME", "name must be printable non-space ASCII"));
        }
        Ok(String::from_utf8(text.to_vec()).expect("ASCII"))
    }
    /// Text strings and property values: printable ASCII including space.
    fn string(&self, limits: &GdsLimits) -> Result<String> {
        let text = self.trimmed_ascii()?;
        if text.len() > limits.max_string_bytes {
            return Err(self.fail("GDS_BUDGET", "max_string_bytes exceeded"));
        }
        if !text.iter().all(|b| (0x20..=0x7e).contains(b)) {
            return Err(self.fail("GDS_SYNTAX", "string must be printable ASCII"));
        }
        Ok(String::from_utf8(text.to_vec()).expect("ASCII"))
    }
    pub fn payload(&self) -> Payload {
        match self.dtype {
            NO_DATA => Payload::None,
            BIT_ARRAY => Payload::Bits(self.bits()),
            INT16 => Payload::I16(self.i16s().collect()),
            INT32 => Payload::I32(self.i32s().collect()),
            REAL8 => Payload::Real(self.reals().collect()),
            _ => Payload::Ascii(self.data.to_vec()),
        }
    }
}

/// Bounded record framing with one record of lookahead.
pub struct RecordReader<'a> {
    bytes: &'a [u8],
    position: usize,
    records: usize,
    max_records: usize,
    peeked: Option<RawRecord<'a>>,
}
impl<'a> RecordReader<'a> {
    pub fn new(bytes: &'a [u8], max_records: usize) -> Self {
        Self {
            bytes,
            position: 0,
            records: 0,
            max_records,
            peeked: None,
        }
    }
    /// Byte offset just after the last consumed (not peeked) record.
    pub fn position(&self) -> usize {
        self.peeked.map_or(self.position, |r| r.offset)
    }
    pub fn records(&self) -> usize {
        self.records
    }
    fn frame(&mut self) -> Result<RawRecord<'a>> {
        let at = self.position;
        let rest = self.bytes.len() - at;
        if rest == 0 {
            return Err(error("GDS_TRUNCATED", at, "stream ends before ENDLIB"));
        }
        if rest < 4 {
            return Err(error("GDS_TRUNCATED", at, "partial record header"));
        }
        let length = usize::from(u16::from_be_bytes([self.bytes[at], self.bytes[at + 1]]));
        if length < 4 || !length.is_multiple_of(2) {
            return Err(error(
                "GDS_SYNTAX",
                at,
                format!("invalid record length {length}"),
            ));
        }
        if length > rest {
            return Err(error("GDS_TRUNCATED", at, "record exceeds stream"));
        }
        let (rtype, dtype) = (self.bytes[at + 2], self.bytes[at + 3]);
        let (expected, name) = spec(rtype).ok_or_else(|| {
            error(
                "GDS_UNSUPPORTED",
                at,
                format!(
                    "record type 0x{rtype:02X} ({}) is not supported",
                    record_name(rtype)
                ),
            )
        })?;
        if dtype != expected {
            return Err(error(
                "GDS_SYNTAX",
                at,
                format!("{name} has data type {dtype}, expected {expected}"),
            ));
        }
        let data = &self.bytes[at + 4..at + length];
        let size_ok = match dtype {
            NO_DATA => data.is_empty(),
            BIT_ARRAY => data.len() == 2,
            INT16 => !data.is_empty(),
            INT32 => !data.is_empty() && data.len().is_multiple_of(4),
            REAL8 => !data.is_empty() && data.len().is_multiple_of(8),
            _ => true,
        };
        if !size_ok {
            return Err(error(
                "GDS_SYNTAX",
                at,
                format!("{name} payload length {} is invalid", data.len()),
            ));
        }
        self.records += 1;
        if self.records > self.max_records {
            return Err(error("GDS_BUDGET", at, "max_records exceeded"));
        }
        self.position = at + length;
        Ok(RawRecord {
            offset: at,
            rtype,
            dtype,
            data,
        })
    }
    pub fn peek(&mut self) -> Result<RawRecord<'a>> {
        if let Some(record) = self.peeked {
            return Ok(record);
        }
        let record = self.frame()?;
        self.peeked = Some(record);
        Ok(record)
    }
    pub fn next_record(&mut self) -> Result<RawRecord<'a>> {
        match self.peeked.take() {
            Some(record) => Ok(record),
            None => self.frame(),
        }
    }
    fn take(&mut self, rtype: u8) -> Result<Option<RawRecord<'a>>> {
        if self.peek()?.rtype == rtype {
            return self.next_record().map(Some);
        }
        Ok(None)
    }
    fn expect(&mut self, rtype: u8, context: &str) -> Result<RawRecord<'a>> {
        let record = self.next_record()?;
        if record.rtype != rtype {
            return Err(record.fail(
                "GDS_SYNTAX",
                format!(
                    "{context}: expected {}, found {}",
                    record_name(rtype),
                    record_name(record.rtype)
                ),
            ));
        }
        Ok(record)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ElementKind {
    Boundary,
    Path,
    Sref,
    Aref,
    Text,
    Node,
    Box,
}
impl ElementKind {
    fn from_record(rtype: u8) -> Option<Self> {
        Some(match rtype {
            rt::BOUNDARY => Self::Boundary,
            rt::PATH => Self::Path,
            rt::SREF => Self::Sref,
            rt::AREF => Self::Aref,
            rt::TEXT => Self::Text,
            rt::NODE => Self::Node,
            rt::BOX => Self::Box,
            _ => return None,
        })
    }
    fn record(self) -> u8 {
        match self {
            Self::Boundary => rt::BOUNDARY,
            Self::Path => rt::PATH,
            Self::Sref => rt::SREF,
            Self::Aref => rt::AREF,
            Self::Text => rt::TEXT,
            Self::Node => rt::NODE,
            Self::Box => rt::BOX,
        }
    }
    /// Record holding the element's secondary layer number.
    fn type_record(self) -> u8 {
        match self {
            Self::Text => rt::TEXTTYPE,
            Self::Node => rt::NODETYPE,
            Self::Box => rt::BOXTYPE,
            _ => rt::DATATYPE,
        }
    }
}

/// Reflection about the x axis precedes magnification and rotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Strans {
    pub flags: u16,
    pub mag: Option<Real8>,
    pub angle: Option<Real8>,
}
impl Strans {
    pub const REFLECT_X: u16 = 0x8000;
    pub const ABSOLUTE_MAG: u16 = 0x0004;
    pub const ABSOLUTE_ANGLE: u16 = 0x0002;
    pub fn reflect_x() -> Self {
        Self {
            flags: Self::REFLECT_X,
            mag: None,
            angle: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub kind: ElementKind,
    pub elflags: Option<u16>,
    pub plex: Option<i32>,
    pub layer: Option<i16>,
    /// DATATYPE, TEXTTYPE, NODETYPE or BOXTYPE according to `kind`.
    pub datatype: Option<i16>,
    pub presentation: Option<u16>,
    pub pathtype: Option<i16>,
    pub width: Option<i32>,
    pub bgnextn: Option<i32>,
    pub endextn: Option<i32>,
    pub sname: Option<String>,
    pub strans: Option<Strans>,
    pub colrow: Option<[i16; 2]>,
    pub xy: Vec<[i32; 2]>,
    pub string: Option<String>,
    pub properties: Vec<(i16, String)>,
}
impl Element {
    pub fn new(kind: ElementKind) -> Self {
        Self {
            kind,
            elflags: None,
            plex: None,
            layer: None,
            datatype: None,
            presentation: None,
            pathtype: None,
            width: None,
            bgnextn: None,
            endextn: None,
            sname: None,
            strans: None,
            colrow: None,
            xy: Vec::new(),
            string: None,
            properties: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Structure {
    pub name: String,
    pub dates: [i16; 12],
    pub strclass: Option<u16>,
    /// Retained elements (empty in index mode).
    pub elements: Vec<Element>,
    /// Number of validated elements, retained or not.
    pub element_count: usize,
    /// Distinct SREF/AREF child names in first-use order.
    pub references: Vec<String>,
    /// Source bytes BGNSTR..=ENDSTR; empty for constructed structures.
    pub source: Range<usize>,
}
impl Structure {
    pub fn new(name: impl Into<String>, dates: [i16; 12]) -> Self {
        Self {
            name: name.into(),
            dates,
            strclass: None,
            elements: Vec::new(),
            element_count: 0,
            references: Vec::new(),
            source: 0..0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    pub version: i16,
    pub dates: [i16; 12],
    /// Records between BGNLIB and UNITS in stream order; includes LIBNAME.
    pub header: Vec<Record>,
    /// Database unit in user units, then in meters.
    pub units: [Real8; 2],
    pub structures: Vec<Structure>,
}
impl Library {
    pub fn libname(&self) -> Option<&[u8]> {
        self.header
            .iter()
            .find_map(|r| match (&r.rtype, &r.payload) {
                (&rt::LIBNAME, Payload::Ascii(bytes)) => {
                    let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
                    Some(&bytes[..end])
                }
                _ => None,
            })
    }
    pub fn index(&self) -> HashMap<&str, usize> {
        self.structures
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.as_str(), i))
            .collect()
    }
    /// Structures reachable from `roots` (inclusive), in stream order.
    /// Iterative DFS with an explicit depth cap and a cycle guard; work is
    /// O(structures + references).
    pub fn closure(&self, roots: &[&str], limits: &GdsLimits) -> Result<Vec<usize>> {
        let index = self.index();
        let mut state = vec![0u8; self.structures.len()];
        for root in roots {
            let &start = index.get(root).ok_or_else(|| {
                error(
                    "GDS_MISSING_STRUCTURE",
                    0,
                    format!("structure {root} is absent from the library"),
                )
            })?;
            if state[start] == 2 {
                continue;
            }
            state[start] = 1;
            let mut stack = vec![(start, 0usize)];
            while let Some(top) = stack.last_mut() {
                let structure = &self.structures[top.0];
                if top.1 == structure.references.len() {
                    state[top.0] = 2;
                    stack.pop();
                    continue;
                }
                let child = &structure.references[top.1];
                top.1 += 1;
                let &c = index.get(child.as_str()).ok_or_else(|| {
                    error(
                        "GDS_MISSING_STRUCTURE",
                        structure.source.start,
                        format!("{} references absent structure {child}", structure.name),
                    )
                })?;
                match state[c] {
                    1 => {
                        return Err(error(
                            "GDS_REFERENCE_CYCLE",
                            structure.source.start,
                            format!("reference cycle through {child}"),
                        ));
                    }
                    2 => {}
                    _ => {
                        if stack.len() >= limits.max_depth {
                            return Err(error(
                                "GDS_BUDGET",
                                structure.source.start,
                                "max_depth exceeded in reference closure",
                            ));
                        }
                        state[c] = 1;
                        stack.push((c, 0));
                    }
                }
            }
        }
        Ok((0..self.structures.len())
            .filter(|&i| state[i] == 2)
            .collect())
    }
}

struct Parser<'a, 'l> {
    reader: RecordReader<'a>,
    limits: &'l GdsLimits,
    retain: bool,
    elements: usize,
    points: usize,
}

/// Parse a complete stream. With `retain_elements == false` every element is
/// fully validated but only structure names, byte ranges and references stay.
pub fn parse_library(bytes: &[u8], limits: &GdsLimits, retain_elements: bool) -> Result<Library> {
    if bytes.len() > limits.max_bytes {
        return Err(error("GDS_BUDGET", 0, "max_bytes exceeded"));
    }
    let mut parser = Parser {
        reader: RecordReader::new(bytes, limits.max_records),
        limits,
        retain: retain_elements,
        elements: 0,
        points: 0,
    };
    let library = parser.library()?;
    let end = parser.reader.position();
    // Tape-era writers pad the last block with zero bytes; anything else is data.
    if bytes[end..].iter().any(|b| *b != 0) {
        return Err(error("GDS_SYNTAX", end, "non-zero bytes after ENDLIB"));
    }
    Ok(library)
}

impl<'a> Parser<'a, '_> {
    fn library(&mut self) -> Result<Library> {
        let header = self.reader.expect(rt::HEADER, "stream start")?;
        let version = header.i16_one()?;
        let dates = self
            .reader
            .expect(rt::BGNLIB, "library header")?
            .i16_array()?;
        let mut records = Vec::new();
        let mut seen = HashSet::new();
        let units = loop {
            let r = self.reader.next_record()?;
            match r.rtype {
                rt::UNITS => {
                    let values: Vec<Real8> = r.reals().collect();
                    let units: [Real8; 2] = values
                        .try_into()
                        .map_err(|_| r.fail("GDS_SYNTAX", "UNITS requires two reals"))?;
                    break units;
                }
                rt::LIBNAME => {
                    r.name(self.limits)?;
                }
                rt::LIBDIRSIZE
                | rt::SRFNAME
                | rt::LIBSECUR
                | rt::REFLIBS
                | rt::FONTS
                | rt::ATTRTABLE
                | rt::GENERATIONS
                | rt::FORMAT
                | rt::ENDMASKS => {}
                rt::MASK => {
                    if records.len() > 256 {
                        return Err(r.fail("GDS_BUDGET", "too many library header records"));
                    }
                }
                _ => {
                    return Err(r.fail(
                        "GDS_SYNTAX",
                        format!("{} in library header", record_name(r.rtype)),
                    ));
                }
            }
            if r.rtype != rt::MASK && !seen.insert(r.rtype) {
                return Err(r.fail("GDS_SYNTAX", format!("duplicate {}", record_name(r.rtype))));
            }
            if r.data.len() > self.limits.max_string_bytes.max(64) * 4 {
                return Err(r.fail("GDS_BUDGET", "library header record too large"));
            }
            records.push(Record {
                rtype: r.rtype,
                payload: r.payload(),
            });
        };
        if !seen.contains(&rt::LIBNAME) {
            return Err(error(
                "GDS_SYNTAX",
                self.reader.position(),
                "library header lacks LIBNAME",
            ));
        }
        for unit in units {
            if unit.to_f64() <= 0.0 {
                return Err(error(
                    "GDS_SYNTAX",
                    self.reader.position(),
                    "UNITS must be positive",
                ));
            }
        }
        let mut structures = Vec::new();
        let mut names = HashSet::new();
        loop {
            let r = self.reader.next_record()?;
            match r.rtype {
                rt::ENDLIB => break,
                rt::BGNSTR => {
                    if structures.len() >= self.limits.max_structures {
                        return Err(r.fail("GDS_BUDGET", "max_structures exceeded"));
                    }
                    let s = self.structure(r)?;
                    if !names.insert(s.name.clone()) {
                        return Err(r.fail(
                            "GDS_DUPLICATE_STRUCTURE",
                            format!("structure {} is defined twice", s.name),
                        ));
                    }
                    structures.push(s);
                }
                _ => {
                    return Err(r.fail(
                        "GDS_SYNTAX",
                        format!("{} between structures", record_name(r.rtype)),
                    ));
                }
            }
        }
        Ok(Library {
            version,
            dates,
            header: records,
            units,
            structures,
        })
    }
    fn structure(&mut self, start: RawRecord<'a>) -> Result<Structure> {
        let dates = start.i16_array()?;
        let name = self
            .reader
            .expect(rt::STRNAME, "structure")?
            .name(self.limits)?;
        let mut structure = Structure::new(name, dates);
        structure.strclass = self.reader.take(rt::STRCLASS)?.map(|r| r.bits());
        let mut references = HashSet::new();
        loop {
            let r = self.reader.next_record()?;
            if r.rtype == rt::ENDSTR {
                structure.source = start.offset..r.end();
                return Ok(structure);
            }
            let kind = ElementKind::from_record(r.rtype).ok_or_else(|| {
                r.fail(
                    "GDS_SYNTAX",
                    format!(
                        "{} inside structure {}",
                        record_name(r.rtype),
                        structure.name
                    ),
                )
            })?;
            self.elements += 1;
            if self.elements > self.limits.max_elements {
                return Err(r.fail("GDS_BUDGET", "max_elements exceeded"));
            }
            let element = self.element(kind, r.offset)?;
            structure.element_count += 1;
            if let Some(child) = &element.sname
                && !references.contains(child)
            {
                if references.len() >= self.limits.max_references_per_structure {
                    return Err(r.fail("GDS_BUDGET", "max_references_per_structure exceeded"));
                }
                references.insert(child.clone());
                structure.references.push(child.clone());
            }
            if self.retain {
                self.points += element.xy.len();
                if self.points > self.limits.max_retained_points {
                    return Err(r.fail("GDS_BUDGET", "max_retained_points exceeded"));
                }
                structure.elements.push(element);
            }
        }
    }
    fn element(&mut self, kind: ElementKind, offset: usize) -> Result<Element> {
        let reader = &mut self.reader;
        let limits = self.limits;
        let mut e = Element::new(kind);
        e.elflags = reader.take(rt::ELFLAGS)?.map(|r| r.bits());
        e.plex = reader.take(rt::PLEX)?.map(|r| r.i32_one()).transpose()?;
        match kind {
            ElementKind::Sref | ElementKind::Aref => {
                e.sname = Some(reader.expect(rt::SNAME, "reference")?.name(limits)?);
            }
            _ => {
                e.layer = Some(reader.expect(rt::LAYER, "element")?.i16_one()?);
                e.datatype = Some(reader.expect(kind.type_record(), "element")?.i16_one()?);
            }
        }
        if kind == ElementKind::Text {
            e.presentation = reader.take(rt::PRESENTATION)?.map(|r| r.bits());
        }
        if matches!(kind, ElementKind::Path | ElementKind::Text) {
            e.pathtype = reader
                .take(rt::PATHTYPE)?
                .map(|r| r.i16_one())
                .transpose()?;
            e.width = reader.take(rt::WIDTH)?.map(|r| r.i32_one()).transpose()?;
        }
        if kind == ElementKind::Path {
            e.bgnextn = reader.take(rt::BGNEXTN)?.map(|r| r.i32_one()).transpose()?;
            e.endextn = reader.take(rt::ENDEXTN)?.map(|r| r.i32_one()).transpose()?;
        }
        if matches!(
            kind,
            ElementKind::Sref | ElementKind::Aref | ElementKind::Text
        ) && let Some(r) = reader.take(rt::STRANS)?
        {
            let mag = reader.take(rt::MAG)?.map(|m| m.real_one()).transpose()?;
            if mag.is_some_and(|m| m.to_f64() <= 0.0) {
                return Err(r.fail("GDS_SYNTAX", "MAG must be positive"));
            }
            let angle = reader.take(rt::ANGLE)?.map(|a| a.real_one()).transpose()?;
            e.strans = Some(Strans {
                flags: r.bits(),
                mag,
                angle,
            });
        }
        if kind == ElementKind::Aref {
            let colrow: [i16; 2] = reader.expect(rt::COLROW, "AREF")?.i16_array()?;
            if colrow.iter().any(|&v| v <= 0) {
                return Err(error("GDS_SYNTAX", offset, "COLROW must be positive"));
            }
            e.colrow = Some(colrow);
        }
        let xy = reader.expect(rt::XY, "element")?;
        if !xy.data.len().is_multiple_of(8) {
            return Err(xy.fail("GDS_SYNTAX", "XY requires coordinate pairs"));
        }
        let n = xy.data.len() / 8;
        if n > limits.max_points_per_element {
            return Err(xy.fail("GDS_BUDGET", "max_points_per_element exceeded"));
        }
        let values: Vec<i32> = xy.i32s().collect();
        e.xy = values.chunks_exact(2).map(|p| [p[0], p[1]]).collect();
        let closed = e.xy.first() == e.xy.last();
        let points_ok = match kind {
            ElementKind::Boundary => n >= 4 && closed,
            ElementKind::Box => n == 5 && closed,
            ElementKind::Path => n >= 2,
            ElementKind::Sref | ElementKind::Text => n == 1,
            ElementKind::Aref => n == 3,
            ElementKind::Node => (1..=50).contains(&n),
        };
        if !points_ok {
            return Err(xy.fail(
                "GDS_GEOMETRY",
                format!("{kind:?} has an invalid XY point list ({n} points)"),
            ));
        }
        if kind == ElementKind::Text {
            e.string = Some(reader.expect(rt::STRING, "TEXT")?.string(limits)?);
        }
        loop {
            let r = reader.next_record()?;
            match r.rtype {
                rt::ENDEL => return Ok(e),
                rt::PROPATTR => {
                    if e.properties.len() >= limits.max_properties_per_element {
                        return Err(r.fail("GDS_BUDGET", "max_properties_per_element exceeded"));
                    }
                    let attribute = r.i16_one()?;
                    let value = reader.expect(rt::PROPVALUE, "property")?.string(limits)?;
                    e.properties.push((attribute, value));
                }
                _ => {
                    return Err(r.fail(
                        "GDS_SYNTAX",
                        format!("{} inside {kind:?} element", record_name(r.rtype)),
                    ));
                }
            }
        }
    }
}

/// Bounded record writer. Every record is length-checked; total bytes and
/// records are capped. The caller owns buffering and atomic publication.
pub struct GdsWriter<W: Write> {
    out: W,
    bytes: u64,
    records: u64,
    max_bytes: u64,
    max_records: u64,
}
impl<W: Write> GdsWriter<W> {
    pub fn new(out: W, max_bytes: u64, max_records: u64) -> Self {
        Self {
            out,
            bytes: 0,
            records: 0,
            max_bytes,
            max_records,
        }
    }
    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }
    pub fn records_written(&self) -> u64 {
        self.records
    }
    pub fn into_inner(mut self) -> Result<W> {
        self.out
            .flush()
            .map_err(|e| error("GDS_IO", self.bytes as usize, e.to_string()))?;
        Ok(self.out)
    }
    fn charge(&mut self, bytes: usize, records: u64) -> Result<()> {
        let at = self.bytes as usize;
        self.bytes = self
            .bytes
            .checked_add(bytes as u64)
            .filter(|&b| b <= self.max_bytes)
            .ok_or_else(|| error("GDS_BUDGET", at, "output byte cap exceeded"))?;
        self.records = self
            .records
            .checked_add(records)
            .filter(|&r| r <= self.max_records)
            .ok_or_else(|| error("GDS_BUDGET", at, "output record cap exceeded"))?;
        Ok(())
    }
    fn emit(&mut self, rtype: u8, data: &[u8]) -> Result<()> {
        let (dtype, name) = spec(rtype)
            .ok_or_else(|| error("GDS_UNSUPPORTED", self.bytes as usize, "record type"))?;
        let length = data.len() + 4;
        if length > MAX_RECORD_BYTES || !length.is_multiple_of(2) {
            return Err(error(
                "GDS_RECORD_LENGTH",
                self.bytes as usize,
                format!("{name} record of {length} bytes cannot be framed"),
            ));
        }
        self.charge(length, 1)?;
        let header = [(length >> 8) as u8, length as u8, rtype, dtype];
        self.out
            .write_all(&header)
            .and_then(|_| self.out.write_all(data))
            .map_err(|e| error("GDS_IO", self.bytes as usize, e.to_string()))
    }
    /// Copy already-framed records verbatim (e.g. a validated source
    /// structure). The framing is re-checked before any byte is written.
    pub fn raw(&mut self, bytes: &[u8]) -> Result<()> {
        let mut at = 0usize;
        let mut count = 0u64;
        while at < bytes.len() {
            if bytes.len() - at < 4 {
                return Err(error("GDS_TRUNCATED", at, "raw copy has a partial header"));
            }
            let length = usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]));
            if length < 4 || !length.is_multiple_of(2) || length > bytes.len() - at {
                return Err(error("GDS_SYNTAX", at, "raw copy framing is invalid"));
            }
            at += length;
            count += 1;
        }
        self.charge(bytes.len(), count)?;
        self.out
            .write_all(bytes)
            .map_err(|e| error("GDS_IO", self.bytes as usize, e.to_string()))
    }
    pub fn record(&mut self, record: &Record) -> Result<()> {
        let (dtype, name) = spec(record.rtype)
            .ok_or_else(|| error("GDS_UNSUPPORTED", self.bytes as usize, "record type"))?;
        if record.payload.dtype() != dtype {
            return Err(error(
                "GDS_SYNTAX",
                self.bytes as usize,
                format!("{name} payload has the wrong data type"),
            ));
        }
        match &record.payload {
            Payload::None => self.emit(record.rtype, &[]),
            Payload::Bits(v) => self.emit(record.rtype, &v.to_be_bytes()),
            Payload::I16(v) => self.i16s(record.rtype, v),
            Payload::I32(v) => self.i32s(record.rtype, v),
            Payload::Real(v) => {
                let data: Vec<u8> = v.iter().flat_map(|r| r.0).collect();
                self.emit(record.rtype, &data)
            }
            Payload::Ascii(v) => {
                if !v.len().is_multiple_of(2) {
                    return Err(error(
                        "GDS_SYNTAX",
                        self.bytes as usize,
                        "ASCII payload must be padded to even length",
                    ));
                }
                self.emit(record.rtype, v)
            }
        }
    }
    fn i16s(&mut self, rtype: u8, values: &[i16]) -> Result<()> {
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.emit(rtype, &data)
    }
    fn i32s(&mut self, rtype: u8, values: &[i32]) -> Result<()> {
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
        self.emit(rtype, &data)
    }
    fn ascii(&mut self, rtype: u8, value: &str, name: bool) -> Result<()> {
        let allowed = |b: &u8| {
            if name {
                (0x21..=0x7e).contains(b)
            } else {
                (0x20..=0x7e).contains(b)
            }
        };
        if (name && value.is_empty()) || !value.bytes().all(|b| allowed(&b)) {
            return Err(error(
                "GDS_NAME",
                self.bytes as usize,
                format!("{} value {value:?} is not encodable", record_name(rtype)),
            ));
        }
        let mut bytes = value.as_bytes().to_vec();
        if !bytes.len().is_multiple_of(2) {
            bytes.push(0);
        }
        self.emit(rtype, &bytes)
    }
    fn xy(&mut self, points: &[[i32; 2]]) -> Result<()> {
        let flat: Vec<i32> = points.iter().flatten().copied().collect();
        self.i32s(rt::XY, &flat)
    }
    pub fn begin_library(
        &mut self,
        version: i16,
        dates: &[i16; 12],
        libname: &str,
        units: [Real8; 2],
    ) -> Result<()> {
        self.i16s(rt::HEADER, &[version])?;
        self.i16s(rt::BGNLIB, dates)?;
        self.ascii(rt::LIBNAME, libname, true)?;
        let data: Vec<u8> = units.iter().flat_map(|r| r.0).collect();
        self.emit(rt::UNITS, &data)
    }
    pub fn end_library(&mut self) -> Result<()> {
        self.emit(rt::ENDLIB, &[])
    }
    pub fn begin_structure(&mut self, name: &str, dates: &[i16; 12]) -> Result<()> {
        self.i16s(rt::BGNSTR, dates)?;
        self.ascii(rt::STRNAME, name, true)
    }
    pub fn end_structure(&mut self) -> Result<()> {
        self.emit(rt::ENDSTR, &[])
    }
    /// Closed polygon (first point repeated last).
    pub fn boundary(&mut self, layer: i16, datatype: i16, points: &[[i32; 2]]) -> Result<()> {
        if points.len() < 4 || points.first() != points.last() || points.len() > 8191 {
            return Err(error(
                "GDS_GEOMETRY",
                self.bytes as usize,
                "BOUNDARY needs 4..=8191 points and must be closed",
            ));
        }
        self.emit(rt::BOUNDARY, &[])?;
        self.i16s(rt::LAYER, &[layer])?;
        self.i16s(rt::DATATYPE, &[datatype])?;
        self.xy(points)?;
        self.emit(rt::ENDEL, &[])
    }
    /// Axis-aligned rectangle `[lx, ly, ux, uy]`, counter-clockwise from (lx, ly).
    pub fn rectangle(&mut self, layer: i16, datatype: i16, r: [i32; 4]) -> Result<()> {
        if r[0] >= r[2] || r[1] >= r[3] {
            return Err(error(
                "GDS_GEOMETRY",
                self.bytes as usize,
                "rectangle must have positive width and height",
            ));
        }
        self.boundary(
            layer,
            datatype,
            &[
                [r[0], r[1]],
                [r[2], r[1]],
                [r[2], r[3]],
                [r[0], r[3]],
                [r[0], r[1]],
            ],
        )
    }
    pub fn sref(&mut self, name: &str, strans: Option<&Strans>, at: [i32; 2]) -> Result<()> {
        self.emit(rt::SREF, &[])?;
        self.ascii(rt::SNAME, name, true)?;
        if let Some(s) = strans {
            self.strans(s)?;
        }
        self.xy(&[at])?;
        self.emit(rt::ENDEL, &[])
    }
    fn strans(&mut self, s: &Strans) -> Result<()> {
        self.emit(rt::STRANS, &s.flags.to_be_bytes())?;
        if let Some(m) = s.mag {
            self.emit(rt::MAG, &m.0)?;
        }
        if let Some(a) = s.angle {
            self.emit(rt::ANGLE, &a.0)?;
        }
        Ok(())
    }
    /// Unrotated, unscaled label (no PRESENTATION/STRANS records).
    pub fn text(&mut self, layer: i16, texttype: i16, at: [i32; 2], string: &str) -> Result<()> {
        self.emit(rt::TEXT, &[])?;
        self.i16s(rt::LAYER, &[layer])?;
        self.i16s(rt::TEXTTYPE, &[texttype])?;
        self.xy(&[at])?;
        self.ascii(rt::STRING, string, false)?;
        self.emit(rt::ENDEL, &[])
    }
    /// Encode a complete element in canonical record order.
    pub fn element(&mut self, e: &Element) -> Result<()> {
        self.emit(e.kind.record(), &[])?;
        if let Some(v) = e.elflags {
            self.emit(rt::ELFLAGS, &v.to_be_bytes())?;
        }
        if let Some(v) = e.plex {
            self.i32s(rt::PLEX, &[v])?;
        }
        match e.kind {
            ElementKind::Sref | ElementKind::Aref => {
                let name = e.sname.as_deref().ok_or_else(|| {
                    error("GDS_SYNTAX", self.bytes as usize, "reference lacks SNAME")
                })?;
                self.ascii(rt::SNAME, name, true)?;
            }
            _ => {
                let (Some(layer), Some(datatype)) = (e.layer, e.datatype) else {
                    return Err(error(
                        "GDS_SYNTAX",
                        self.bytes as usize,
                        "element lacks LAYER/type",
                    ));
                };
                self.i16s(rt::LAYER, &[layer])?;
                self.i16s(e.kind.type_record(), &[datatype])?;
            }
        }
        if let Some(v) = e.presentation {
            self.emit(rt::PRESENTATION, &v.to_be_bytes())?;
        }
        if let Some(v) = e.pathtype {
            self.i16s(rt::PATHTYPE, &[v])?;
        }
        if let Some(v) = e.width {
            self.i32s(rt::WIDTH, &[v])?;
        }
        if let Some(v) = e.bgnextn {
            self.i32s(rt::BGNEXTN, &[v])?;
        }
        if let Some(v) = e.endextn {
            self.i32s(rt::ENDEXTN, &[v])?;
        }
        if let Some(s) = &e.strans {
            self.strans(s)?;
        }
        if let Some(c) = e.colrow {
            self.i16s(rt::COLROW, &c)?;
        }
        self.xy(&e.xy)?;
        if let Some(s) = &e.string {
            self.ascii(rt::STRING, s, false)?;
        }
        for (attribute, value) in &e.properties {
            self.i16s(rt::PROPATTR, &[*attribute])?;
            self.ascii(rt::PROPVALUE, value, false)?;
        }
        self.emit(rt::ENDEL, &[])
    }
}

/// Encode a retained library model (all structures must retain elements).
pub fn write_library<W: Write>(library: &Library, writer: &mut GdsWriter<W>) -> Result<()> {
    writer.i16s(rt::HEADER, &[library.version])?;
    writer.i16s(rt::BGNLIB, &library.dates)?;
    for record in &library.header {
        writer.record(record)?;
    }
    let units: Vec<u8> = library.units.iter().flat_map(|r| r.0).collect();
    writer.emit(rt::UNITS, &units)?;
    for s in &library.structures {
        if s.elements.len() != s.element_count {
            return Err(error(
                "GDS_SYNTAX",
                0,
                format!("structure {} was parsed in index mode", s.name),
            ));
        }
        writer.begin_structure(&s.name, &s.dates)?;
        if let Some(c) = s.strclass {
            writer.emit(rt::STRCLASS, &c.to_be_bytes())?;
        }
        for e in &s.elements {
            writer.element(e)?;
        }
        writer.end_structure()?;
    }
    writer.end_library()
}

/// Database units per micron when the user unit is exactly one micron per
/// 10^k database units (k = 0..=6): the UNITS reals must round-trip to the
/// binary64 values nearest 10^-k (user) and 10^-(6+k) (meters). Anything else
/// is reported as unsupported rather than approximated.
pub fn dbu_per_micron(units: &[Real8; 2]) -> Result<u32> {
    for k in 0..=6u32 {
        let user: f64 = format!("1e-{k}").parse().expect("literal");
        let meter: f64 = format!("1e-{}", 6 + k).parse().expect("literal");
        if units[0].to_f64() == user && units[1].to_f64() == meter {
            return Ok(10u32.pow(k));
        }
    }
    Err(error(
        "GDS_UNITS_UNSUPPORTED",
        0,
        format!(
            "UNITS {:e} user/{:e} m is not a decimal micron grid",
            units[0].to_f64(),
            units[1].to_f64()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATES: [i16; 12] = [70, 1, 1, 0, 0, 0, 70, 1, 1, 0, 0, 0];
    fn hex(s: &str) -> [u8; 8] {
        let v = u64::from_str_radix(s, 16).unwrap();
        v.to_be_bytes()
    }
    fn units() -> [Real8; 2] {
        [
            Real8::from_f64(1e-3).unwrap(),
            Real8::from_f64(1e-9).unwrap(),
        ]
    }
    fn sample() -> Library {
        let mut leaf = Structure::new("leaf", DATES);
        let mut b = Element::new(ElementKind::Boundary);
        b.layer = Some(68);
        b.datatype = Some(20);
        b.xy = vec![[0, 0], [10, 0], [10, 5], [0, 5], [0, 0]];
        b.properties = vec![(1, "net=a".into())];
        let mut p = Element::new(ElementKind::Path);
        p.layer = Some(69);
        p.datatype = Some(20);
        p.pathtype = Some(4);
        p.width = Some(140);
        p.bgnextn = Some(70);
        p.endextn = Some(-70);
        p.elflags = Some(1);
        p.plex = Some(7);
        p.xy = vec![[0, 0], [0, 1000], [-500, 1000]];
        let mut t = Element::new(ElementKind::Text);
        t.layer = Some(70);
        t.datatype = Some(5);
        t.presentation = Some(5);
        t.strans = Some(Strans {
            flags: Strans::REFLECT_X | Strans::ABSOLUTE_MAG,
            mag: Some(Real8::from_f64(0.1).unwrap()),
            angle: Some(Real8::from_f64(90.0).unwrap()),
        });
        t.xy = vec![[1, -1]];
        t.string = Some("a[1] odd".into());
        let mut n = Element::new(ElementKind::Node);
        n.layer = Some(1);
        n.datatype = Some(2);
        n.xy = vec![[3, 4]];
        let mut x = Element::new(ElementKind::Box);
        x.layer = Some(3);
        x.datatype = Some(0);
        x.xy = vec![[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]];
        leaf.elements = vec![b, p, t, n, x];
        leaf.element_count = 5;
        let mut top = Structure::new("top_cell", DATES);
        let mut s = Element::new(ElementKind::Sref);
        s.sname = Some("leaf".into());
        s.strans = Some(Strans::reflect_x());
        s.xy = vec![[100, 200]];
        let mut a = Element::new(ElementKind::Aref);
        a.sname = Some("leaf".into());
        a.colrow = Some([2, 3]);
        a.xy = vec![[0, 0], [200, 0], [0, 300]];
        top.elements = vec![s, a];
        top.element_count = 2;
        top.strclass = Some(0);
        top.references = vec!["leaf".into()];
        Library {
            version: 600,
            dates: DATES,
            header: vec![
                Record {
                    rtype: rt::LIBNAME,
                    payload: Payload::text("lib"),
                },
                Record {
                    rtype: rt::GENERATIONS,
                    payload: Payload::I16(vec![3]),
                },
            ],
            units: units(),
            structures: vec![leaf, top],
        }
    }
    fn encode(library: &Library) -> Vec<u8> {
        let mut w = GdsWriter::new(Vec::new(), 1 << 30, 1 << 26);
        write_library(library, &mut w).unwrap();
        w.into_inner().unwrap()
    }
    fn parse(bytes: &[u8]) -> Result<Library> {
        parse_library(bytes, &GdsLimits::default(), true)
    }

    #[test]
    fn real8_encoding_is_exact_and_matches_known_vectors() {
        // Vectors: PDK cell-library UNITS bytes and hand-derived values.
        for (value, bytes) in [
            (1e-3, "3e4189374bc6a7f0"),
            (1e-9, "3944b82fa09b5a54"),
            (1.0, "4110000000000000"),
            (0.5, "4080000000000000"),
            (-1.0, "c110000000000000"),
            (16.0, "4210000000000000"),
            (90.0, "425a000000000000"),
        ] {
            let r = Real8::from_f64(value).unwrap();
            assert_eq!(r.0, hex(bytes), "{value}");
            assert_eq!(r.to_f64().to_bits(), value.to_bits());
            assert!(r.is_exact_f64());
        }
        assert_eq!(Real8::from_f64(0.0).unwrap(), Real8::ZERO);
        assert_eq!(Real8::from_f64(-0.0).unwrap(), Real8::ZERO);
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Random finite doubles whose magnitude lies inside the GDS range.
            let exponent = (state % 400) as i64 - 200;
            let v = f64::from_bits((state & ((1 << 52) - 1)) | (((1023 + exponent) as u64) << 52));
            let v = if state & (1 << 60) != 0 { -v } else { v };
            let r = Real8::from_f64(v).unwrap();
            assert_eq!(r.to_f64().to_bits(), v.to_bits(), "{v:e}");
            let (negative, m, _) = r.parts();
            assert!(m >> 52 != 0, "normalized hex mantissa");
            assert_eq!(negative, v < 0.0);
        }
        for v in [f64::NAN, f64::INFINITY, 1e80, 1e-80, f64::MIN_POSITIVE] {
            assert_eq!(Real8::from_f64(v).unwrap_err().code, "GDS_REAL");
        }
    }

    #[test]
    fn real8_decoding_rounds_extra_mantissa_bits_to_nearest_even() {
        // 56-bit mantissa 2^55 + 1: three bits beyond binary64 precision.
        let make = |m: u64| {
            let mut b = [0u8; 8];
            b[0] = 0x41;
            b[1..].copy_from_slice(&m.to_be_bytes()[1..]);
            Real8(b)
        };
        let base = 1u64 << 55;
        // Exactly 0.5 = 2^55 * 2^(4*1-56) = 2^3... value = m * 2^-52.
        assert_eq!(make(base).to_f64(), 8.0);
        assert_eq!(make(base + 1).to_f64(), 8.0); // below half ulp
        assert_eq!(make(base + 4).to_f64(), 8.0); // tie, even stays
        assert_eq!(make(base + 5).to_f64(), 8.0 + 2f64.powi(-49));
        assert_eq!(make(base + 12).to_f64(), 8.0 + 2.0 * 2f64.powi(-49)); // tie to even up
        assert!(!make(base + 1).is_exact_f64());
        // Unnormalized mantissas decode exactly too.
        assert_eq!(make(1).to_f64(), 2f64.powi(-52));
        assert!(!make(1).is_exact_f64());
        assert_eq!(make(0).to_f64(), 0.0);
    }

    #[test]
    fn records_round_trip_through_the_typed_model() {
        let library = sample();
        let bytes = encode(&library);
        let parsed = parse(&bytes).unwrap();
        assert_eq!(parsed.version, 600);
        assert_eq!(parsed.header, library.header);
        assert_eq!(parsed.units, library.units);
        assert_eq!(parsed.libname(), Some(&b"lib"[..]));
        for (a, b) in parsed.structures.iter().zip(&library.structures) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.elements, b.elements);
            assert_eq!(a.references, b.references);
            assert_eq!(&bytes[a.source.clone()][2..4], &[rt::BGNSTR, INT16]);
        }
        assert_eq!(encode(&parsed), bytes, "re-encoding is byte-identical");
        let index = parse_library(&bytes, &GdsLimits::default(), false).unwrap();
        assert!(index.structures.iter().all(|s| s.elements.is_empty()));
        assert_eq!(index.structures[0].element_count, 5);
        assert_eq!(index.structures[1].references, vec!["leaf".to_string()]);
        // Tape padding after ENDLIB is accepted; other trailing data is not.
        let mut padded = bytes.clone();
        padded.extend([0u8; 6]);
        assert!(parse(&padded).is_ok());
        padded.push(1);
        padded.push(0);
        assert_eq!(parse(&padded).unwrap_err().code, "GDS_SYNTAX");
    }

    #[test]
    fn writer_output_is_deterministic_and_verbatim_copies_are_exact() {
        let library = sample();
        let bytes = encode(&library);
        let parsed = parse(&bytes).unwrap();
        let leaf = &parsed.structures[0];
        let mut w = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
        w.begin_library(600, &DATES, "copy", parsed.units).unwrap();
        w.raw(&bytes[leaf.source.clone()]).unwrap();
        w.begin_structure("top", &DATES).unwrap();
        w.sref("leaf", Some(&Strans::reflect_x()), [5, 7]).unwrap();
        w.rectangle(68, 20, [0, 0, 3, 4]).unwrap();
        w.text(70, 5, [1, 2], "a[0]").unwrap();
        w.end_structure().unwrap();
        w.end_library().unwrap();
        let first = w.into_inner().unwrap();
        let again = {
            let mut w = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
            w.begin_library(600, &DATES, "copy", parsed.units).unwrap();
            w.raw(&bytes[leaf.source.clone()]).unwrap();
            w.begin_structure("top", &DATES).unwrap();
            w.sref("leaf", Some(&Strans::reflect_x()), [5, 7]).unwrap();
            w.rectangle(68, 20, [0, 0, 3, 4]).unwrap();
            w.text(70, 5, [1, 2], "a[0]").unwrap();
            w.end_structure().unwrap();
            w.end_library().unwrap();
            w.into_inner().unwrap()
        };
        assert_eq!(first, again);
        let copy = parse(&first).unwrap();
        assert_eq!(
            &first[copy.structures[0].source.clone()],
            &bytes[leaf.source.clone()]
        );
        assert_eq!(
            copy.closure(&["top"], &GdsLimits::default()).unwrap(),
            vec![0, 1]
        );
        let top = &copy.structures[1].elements;
        assert_eq!(top[0].strans, Some(Strans::reflect_x()));
        assert_eq!(top[1].xy, vec![[0, 0], [3, 0], [3, 4], [0, 4], [0, 0]]);
        assert_eq!(top[2].string.as_deref(), Some("a[0]"));
        // Writer refuses malformed geometry and unencodable names.
        let mut w = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
        assert!(w.rectangle(1, 0, [0, 0, 0, 5]).is_err());
        assert!(w.boundary(1, 0, &[[0, 0], [1, 0], [1, 1], [0, 1]]).is_err());
        assert!(w.sref("bad name", None, [0, 0]).is_err());
        assert!(w.text(1, 0, [0, 0], "tab\t").is_err());
        assert!(w.raw(&[0, 6, 0, 2]).is_err());
    }

    #[test]
    fn budgets_are_hard_errors() {
        let bytes = encode(&sample());
        let cases = [
            GdsLimits {
                max_bytes: bytes.len() - 1,
                ..Default::default()
            },
            GdsLimits {
                max_records: 10,
                ..Default::default()
            },
            GdsLimits {
                max_structures: 1,
                ..Default::default()
            },
            GdsLimits {
                max_elements: 6,
                ..Default::default()
            },
            GdsLimits {
                max_points_per_element: 4,
                ..Default::default()
            },
            GdsLimits {
                max_retained_points: 10,
                ..Default::default()
            },
            GdsLimits {
                max_string_bytes: 4,
                ..Default::default()
            },
            GdsLimits {
                max_properties_per_element: 0,
                ..Default::default()
            },
        ];
        for limits in cases {
            assert_eq!(
                parse_library(&bytes, &limits, true).unwrap_err().code,
                "GDS_BUDGET",
                "{limits:?}"
            );
        }
        let names = GdsLimits {
            max_name_bytes: 3,
            ..Default::default()
        };
        assert_eq!(
            parse_library(&bytes, &names, false).unwrap_err().code,
            "GDS_NAME"
        );
        let mut w = GdsWriter::new(Vec::new(), 10, 100);
        assert_eq!(
            w.begin_library(600, &DATES, "x", units()).unwrap_err().code,
            "GDS_BUDGET"
        );
        let mut w = GdsWriter::new(Vec::new(), 1 << 20, 2);
        assert_eq!(
            w.begin_library(600, &DATES, "x", units()).unwrap_err().code,
            "GDS_BUDGET"
        );
    }

    #[test]
    fn reference_closure_is_bounded_and_rejects_cycles_and_missing_children() {
        let mut library = sample();
        let chain = |n: usize| {
            let mut lib = Library {
                structures: Vec::new(),
                ..sample()
            };
            for i in 0..n {
                let mut s = Structure::new(format!("c{i}"), DATES);
                if i + 1 < n {
                    s.references.push(format!("c{}", i + 1));
                }
                lib.structures.push(s);
            }
            lib
        };
        let limits = GdsLimits {
            max_depth: 8,
            ..Default::default()
        };
        assert_eq!(chain(8).closure(&["c0"], &limits).unwrap().len(), 8);
        assert_eq!(
            chain(9).closure(&["c0"], &limits).unwrap_err().code,
            "GDS_BUDGET"
        );
        library.structures[0].references.push("top_cell".into());
        assert_eq!(
            library.closure(&["top_cell"], &limits).unwrap_err().code,
            "GDS_REFERENCE_CYCLE"
        );
        library.structures[0].references = vec!["ghost".into()];
        assert_eq!(
            library.closure(&["top_cell"], &limits).unwrap_err().code,
            "GDS_MISSING_STRUCTURE"
        );
        assert_eq!(
            library.closure(&["absent"], &limits).unwrap_err().code,
            "GDS_MISSING_STRUCTURE"
        );
        // Diamond: shared children are visited once and returned in stream order.
        let mut d = chain(3);
        d.structures[0].references.push("c2".into());
        assert_eq!(d.closure(&["c1", "c0"], &limits).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn truncated_and_mutated_streams_fail_without_panicking() {
        let bytes = encode(&sample());
        for end in 0..bytes.len() {
            let error = parse(&bytes[..end]).unwrap_err();
            assert!(
                matches!(error.code, "GDS_TRUNCATED" | "GDS_SYNTAX" | "GDS_GEOMETRY"),
                "prefix {end}: {error}"
            );
        }
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut rejected = 0;
        for _ in 0..2000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut corrupt = bytes.clone();
            let at = (state % bytes.len() as u64) as usize;
            corrupt[at] ^= 1 << ((state >> 32) % 8);
            if parse(&corrupt).is_err() {
                rejected += 1;
            }
        }
        // Flipped coordinate bits can still be valid geometry; the property under
        // test is that no corruption panics and framing damage is detected.
        assert!(rejected > 0);
        // Hand-made hostile records.
        let header = |rest: &[u8]| {
            let mut v = vec![0, 6, rt::HEADER, INT16, 2, 88];
            v.extend_from_slice(rest);
            v
        };
        for (stream, code) in [
            (vec![0, 0, 0, 2], "GDS_SYNTAX"),
            (vec![0, 5, 0, 2, 0], "GDS_SYNTAX"),
            (vec![0, 6, 0, 3, 0, 0], "GDS_SYNTAX"),
            (vec![0, 4, 0x14, 0], "GDS_UNSUPPORTED"),
            (vec![0, 4, 0xff, 0], "GDS_UNSUPPORTED"),
            (vec![0, 8, rt::UNITS, 4, 0, 0, 0, 0], "GDS_SYNTAX"),
            (header(&[0, 4, rt::ENDLIB, 0]), "GDS_SYNTAX"),
            (header(&[]), "GDS_TRUNCATED"),
        ] {
            assert_eq!(parse(&stream).unwrap_err().code, code, "{stream:?}");
        }
    }

    #[test]
    fn grammar_violations_are_explicit() {
        let bytes = |edit: &dyn Fn(&mut Library)| {
            let mut library = sample();
            edit(&mut library);
            encode(&library)
        };
        let open = bytes(&|l| {
            l.structures[0].elements[0].xy.pop();
        });
        assert_eq!(parse(&open).unwrap_err().code, "GDS_GEOMETRY");
        let duplicate = bytes(&|l| l.structures[1].name = "leaf".into());
        assert_eq!(
            parse(&duplicate).unwrap_err().code,
            "GDS_DUPLICATE_STRUCTURE"
        );
        let aref = bytes(&|l| l.structures[1].elements[1].xy.truncate(2));
        assert_eq!(parse(&aref).unwrap_err().code, "GDS_GEOMETRY");
        let colrow = bytes(&|l| l.structures[1].elements[1].colrow = Some([0, 1]));
        assert_eq!(parse(&colrow).unwrap_err().code, "GDS_SYNTAX");
        let mag = bytes(&|l| {
            l.structures[0].elements[2].strans.as_mut().unwrap().mag =
                Some(Real8::from_f64(-1.0).unwrap())
        });
        assert_eq!(parse(&mag).unwrap_err().code, "GDS_SYNTAX");
        let nolib = bytes(&|l| {
            l.header.remove(0);
        });
        assert_eq!(parse(&nolib).unwrap_err().code, "GDS_SYNTAX");
        let good = encode(&sample());
        // A MAG record without STRANS is out of order.
        let mut out = GdsWriter::new(Vec::new(), 1 << 20, 1 << 16);
        out.begin_library(600, &DATES, "x", units()).unwrap();
        out.begin_structure("s", &DATES).unwrap();
        out.emit(rt::SREF, &[]).unwrap();
        out.ascii(rt::SNAME, "s", true).unwrap();
        out.emit(rt::MAG, &Real8::from_f64(1.0).unwrap().0).unwrap();
        let bad = out.into_inner().unwrap();
        assert_eq!(parse(&bad).unwrap_err().code, "GDS_SYNTAX");
        assert!(parse(&good).is_ok());
        // Names with spaces or non-ASCII bytes are rejected when read.
        let mut spaced = good.clone();
        let at = spaced.windows(4).position(|w| w == b"leaf").unwrap();
        spaced[at] = b' ';
        assert_eq!(parse(&spaced).unwrap_err().code, "GDS_NAME");
        spaced[at] = 0xC3;
        assert_eq!(parse(&spaced).unwrap_err().code, "GDS_NAME");
    }

    #[test]
    fn unit_grids_are_exact_decimal_microns() {
        assert_eq!(dbu_per_micron(&units()).unwrap(), 1000);
        let half = [
            Real8::from_f64(5e-4).unwrap(),
            Real8::from_f64(5e-10).unwrap(),
        ];
        assert_eq!(
            dbu_per_micron(&half).unwrap_err().code,
            "GDS_UNITS_UNSUPPORTED"
        );
        let pdk = [
            Real8(hex("3e4189374bc6a7f0")),
            Real8(hex("3944b82fa09b5a54")),
        ];
        assert_eq!(dbu_per_micron(&pdk).unwrap(), 1000);
        let nm = [
            Real8::from_f64(1e-4).unwrap(),
            Real8::from_f64(1e-10).unwrap(),
        ];
        assert_eq!(dbu_per_micron(&nm).unwrap(), 10_000);
    }

    #[test]
    #[ignore = "requires explicit pinned Ciel PDK cell GDS paths; absence is unverified"]
    fn real_pdk_cell_libraries_parse_and_round_trip() {
        for (variable, structures) in [
            ("RUSTROAD_SKY130_CELL_GDS", 446usize),
            ("RUSTROAD_GF180_CELL_GDS", 230),
        ] {
            let path = std::env::var(variable).expect("explicit PDK GDS path required");
            let bytes = std::fs::read(path).unwrap();
            let library = parse(&bytes).unwrap();
            assert_eq!(library.structures.len(), structures);
            assert_eq!(dbu_per_micron(&library.units).unwrap(), 1000);
            assert_eq!(
                encode(&library),
                bytes,
                "{variable} re-encodes byte-identically"
            );
            let index = parse_library(&bytes, &GdsLimits::default(), false).unwrap();
            let names: Vec<&str> = index.structures.iter().map(|s| s.name.as_str()).collect();
            let all = index.closure(&names, &GdsLimits::default()).unwrap();
            assert_eq!(all.len(), structures);
            for s in &index.structures {
                let r = &bytes[s.source.clone()];
                assert_eq!(&r[2..4], &[rt::BGNSTR, INT16]);
                assert_eq!(&r[r.len() - 4..], &[0, 4, rt::ENDSTR, NO_DATA]);
            }
        }
    }
}
