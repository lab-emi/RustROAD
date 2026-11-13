//! Build a `rustroad.rc-profile.v1` extraction profile from PDK data files.
//!
//! This is the IHP SG13G2 path of `scripts/build_rc_profile.py`, value for
//! value and byte for byte: the same three PDK data files are read as data
//! (the OpenRCX calibration table, the Magic technology `extract` section and
//! the technology LEF), the same arithmetic is done in the same order on IEEE
//! doubles, and the profile is rendered as `json.dumps(profile, indent=1)`.
//! The physical model is documented in `docs/extraction.md`; no coefficient
//! is fitted to an extractor output.
//!
//! Only the `ihp` table entry is ported: the SKY130 and GF180 profiles in
//! `fixtures/pdk` are still produced by the script, whose PDK data this port
//! was not checked against.
//!
//! Caps: 8 MiB per input file.
use super::text::{float, format_g, is_digits, split, splitlines, strip};
use super::{StepResult, fail, read_bytes};
use rustroad_pyjson::{Dict, Layout, Value, list, pure_path, sha256_hex};
use std::collections::BTreeMap;
use std::path::Path;

const CODE: &str = "RC_PROFILE";
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// The resistance source of a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResistanceModel {
    /// Sheet and contact resistance from the Magic `extract` section.
    Magic,
    /// Wire resistance from the calibration table's constant RESOVER column
    /// and cut resistance from the technology LEF (the matched IHP model).
    OpenRcxPdk,
}
impl ResistanceModel {
    /// # Errors
    /// `RC_PROFILE` for a name other than `magic` or `openrcx-pdk`.
    pub fn parse(name: &str) -> StepResult<Self> {
        match name {
            "magic" => Ok(Self::Magic),
            "openrcx-pdk" => Ok(Self::OpenRcxPdk),
            _ => Err(fail(CODE, format!("unsupported resistance model {name}"))),
        }
    }
}

/// One profile request. Paths are used as given (joined lexically, as the
/// script joins them) and recorded in the profile's `sources`.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    /// `min`, `nom` or `max`.
    pub corner: &'a str,
    /// Directory that holds `ihp-sg13g2/`.
    pub pdk_root: &'a str,
    pub model: ResistanceModel,
    /// Calibration table to use instead of the PDK's per-corner Magic rules.
    pub rules_override: Option<&'a str>,
    /// Technology LEF to use instead of the PDK standard-cell technology LEF.
    pub technology_lef: Option<&'a str>,
}

struct Spec {
    library: &'static str,
    base: &'static str,
    technology_lef: &'static str,
    rules: [&'static str; 2],
    magic: &'static str,
    magic_extract: &'static str,
    routing: [&'static str; 7],
    cuts: [(&'static str, &'static str); 6],
    pdk_version: &'static str,
}

const IHP: Spec = Spec {
    library: "ez130_8t",
    base: "ihp-sg13g2",
    technology_lef: "libs.ref/sg13g2_stdcell/lef/sg13g2_tech.lef",
    rules: ["libs.tech/librelane/openrcx/ihp-sg13g2.", ".magic.rules"],
    magic: "libs.tech/magic/ihp-sg13g2.tech",
    magic_extract: "libs.tech/magic/ihp-sg13g2-extract.tech",
    routing: [
        "Metal1",
        "Metal2",
        "Metal3",
        "Metal4",
        "Metal5",
        "TopMetal1",
        "TopMetal2",
    ],
    cuts: [
        ("Via1", "via1"),
        ("Via2", "via2"),
        ("Via3", "via3"),
        ("Via4", "via4"),
        ("TopVia1", "via5"),
        ("TopVia2", "via6"),
    ],
    pdk_version: "5cccb161f7492697cfa52eb14dc03beb00bdca9e",
};

fn magic_type(index: usize) -> String {
    format!("allm{index}")
}
fn magic_plane(index: usize) -> String {
    format!("metal{index}")
}

fn bad(message: impl Into<String>) -> super::StepError {
    fail(CODE, message)
}

fn number(token: &str) -> StepResult<f64> {
    float(token).ok_or_else(|| bad(format!("could not convert string to float: '{token}'")))
}

/// `pdk_root / a / b` as `pathlib` joins and prints it: an absolute right
/// operand replaces the left.
fn join(left: &str, right: &str) -> String {
    if right.starts_with('/') {
        pure_path(right)
    } else {
        pure_path(&format!("{left}/{right}"))
    }
}

struct Source {
    text: String,
    sha256: String,
    bytes: usize,
}

fn load(path: &str) -> StepResult<Source> {
    if !Path::new(path).is_file() {
        return Err(bad(format!("missing PDK data file {path}")));
    }
    let data = read_bytes(Path::new(path), MAX_INPUT_BYTES, CODE)
        .map_err(|_| bad(format!("input over byte cap: {path}")))?;
    let sha256 = sha256_hex(&data);
    let bytes = data.len();
    let text = String::from_utf8(data).map_err(|_| bad(format!("{path}: not UTF-8")))?;
    Ok(Source {
        text,
        sha256,
        bytes,
    })
}

fn fingerprint(role: &str, path: &str, source: &Source) -> Value {
    Value::Dict(
        Dict::new()
            .with("role", role)
            .with("path", path)
            .with("sha256", source.sha256.as_str())
            .with("bytes", source.bytes),
    )
}

#[derive(Default, Clone)]
struct LefLayer {
    name: String,
    kind: Option<String>,
    width: Option<f64>,
    rpersq: Option<f64>,
    resistance: Option<f64>,
    cpersqdist: Option<f64>,
    edgecap: Option<f64>,
}

struct Lef {
    layers: Vec<LefLayer>,
    placeholder_parasitics: bool,
}

/// Ordered layers with TYPE, WIDTH and RC statements (cross-check data).
fn parse_lef(text: &str) -> StepResult<Lef> {
    let mut layers = Vec::new();
    let mut current: Option<LefLayer> = None;
    for raw in splitlines(text) {
        let line = strip(raw.split('#').next().unwrap_or(""));
        if line.is_empty() {
            continue;
        }
        let tokens = split(line);
        if current.is_none() {
            if tokens.len() == 2 && tokens[0] == "LAYER" {
                current = Some(LefLayer {
                    name: tokens[1].to_owned(),
                    ..LefLayer::default()
                });
            }
            continue;
        }
        let layer = current.as_mut().ok_or_else(|| bad("LEF layer state"))?;
        if line == format!("END {}", layer.name) {
            layers.extend(current.take());
            continue;
        }
        let tokens = split(line.trim_end_matches(';'));
        match tokens.as_slice() {
            ["TYPE", rest @ ..] => {
                let kind = rest
                    .first()
                    .ok_or_else(|| bad("LEF TYPE without a value"))?;
                layer.kind = Some((*kind).to_owned());
            }
            ["WIDTH", value] => layer.width = Some(number(value)?),
            ["RESISTANCE", "RPERSQ", rest @ ..] => {
                let value = rest
                    .first()
                    .ok_or_else(|| bad("LEF RPERSQ without a value"))?;
                layer.rpersq = Some(number(value)?);
            }
            ["RESISTANCE", value] => layer.resistance = Some(number(value)?),
            ["CAPACITANCE", "CPERSQDIST", rest @ ..] => {
                let value = rest
                    .first()
                    .ok_or_else(|| bad("LEF CPERSQDIST without a value"))?;
                layer.cpersqdist = Some(number(value)?);
            }
            ["EDGECAPACITANCE", rest @ ..] => {
                let value = rest
                    .first()
                    .ok_or_else(|| bad("LEF EDGECAPACITANCE without a value"))?;
                layer.edgecap = Some(number(value)?);
            }
            _ => {}
        }
    }
    if current.is_some() {
        return Err(bad("unterminated LEF LAYER block"));
    }
    Ok(Lef {
        layers,
        placeholder_parasitics: text.contains("not correct Parasitic Information"),
    })
}

type TableKey = (i64, i64, Option<i64>);
/// One calibration context of a metal: `(over, under, rows)`.
type Context<'a> = (i64, Option<i64>, &'a Vec<[f64; 3]>);

struct Rules {
    layer_count: i64,
    widths: BTreeMap<i64, f64>,
    /// `(metal, over, under)` -> rows `[spacing, coupling, ground]`.
    tables: BTreeMap<TableKey, Vec<[f64; 3]>>,
    resover: BTreeMap<(i64, i64), Vec<f64>>,
}

fn integer(token: &str) -> StepResult<i64> {
    token
        .parse()
        .map_err(|_| bad(format!("calibration index out of range: {token}")))
}

fn parse_rules(text: &str) -> StepResult<Rules> {
    let lines = splitlines(text);
    if lines.first().map(|line| strip(line)) != Some("Extraction Rules for OpenRCX") {
        return Err(bad("unexpected calibration-table header"));
    }
    let at = |index: usize| -> StepResult<&str> {
        lines
            .get(index)
            .copied()
            .ok_or_else(|| bad("calibration table ends inside a block"))
    };
    let mut rules = Rules {
        layer_count: -1,
        widths: BTreeMap::new(),
        tables: BTreeMap::new(),
        resover: BTreeMap::new(),
    };
    let mut count = None;
    let mut i = 0;
    while i < lines.len() {
        let line = strip(lines[i]);
        let tokens = split(line);
        if let ["LayerCount", value] = tokens.as_slice()
            && is_digits(value)
        {
            count = Some(integer(value)?);
        }
        let metal_line = tokens.len() >= 3 && tokens[0] == "Metal" && is_digits(tokens[1]);
        if metal_line
            && tokens.len() == 3
            && matches!(
                tokens[2],
                "RESOVER" | "OVER" | "UNDER" | "DIAGUNDER" | "OVERUNDER"
            )
        {
            // `WIDTH Table <n> entries: <values>` on the next line.
            let next = strip(at(i + 1)?);
            let parsed = next.strip_prefix("WIDTH Table ").and_then(|rest| {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                let (entries, tail) = rest.split_at(digits);
                (digits > 0)
                    .then(|| tail.strip_prefix(" entries:"))
                    .flatten()
                    .map(|values| (entries, values))
            });
            let Some((entries, values)) = parsed else {
                return Err(bad(format!("missing width table after {}", repr(line))));
            };
            let values = split(values);
            if integer(entries)? > 1 || values.len() > 1 {
                return Err(bad(format!(
                    "multi-width calibration table unsupported: {}",
                    repr(line)
                )));
            }
            if let Some(value) = values.first() {
                let key = integer(tokens[1])?;
                let width = number(value)?;
                if *rules.widths.entry(key).or_insert(width) != width {
                    return Err(bad(format!("inconsistent table width for metal {key}")));
                }
            }
            i += 2;
            continue;
        }
        let context = metal_line
            && matches!(tokens[2], "RESOVER" | "OVER" | "UNDER" | "DIAGUNDER")
            && match tokens.len() {
                4 => is_digits(tokens[3]),
                6 => is_digits(tokens[3]) && tokens[4] == "UNDER" && is_digits(tokens[5]),
                _ => false,
            };
        if context {
            let (metal, kind, a) = (integer(tokens[1])?, tokens[2], integer(tokens[3])?);
            let b = if tokens.len() == 6 {
                Some(integer(tokens[5])?)
            } else {
                None
            };
            let head = split(strip(at(i + 1)?));
            let n = match head.as_slice() {
                ["DIST", "count", n, "width", _] if is_digits(n) && dist_header(at(i + 1)?) => {
                    usize::try_from(integer(n)?).map_err(|_| bad("DIST count"))?
                }
                _ => return Err(bad(format!("missing DIST header after {}", repr(line)))),
            };
            let mut rows = Vec::with_capacity(n.min(4096));
            for k in 0..n {
                let fields = split(at(i + 2 + k)?);
                if fields.len() != 4 {
                    return Err(bad(format!(
                        "calibration row must have 4 fields: {}",
                        repr(line)
                    )));
                }
                let mut row = [0.0f64; 4];
                for (slot, field) in row.iter_mut().zip(&fields) {
                    *slot = number(field)?;
                }
                rows.push(row);
            }
            if strip(at(i + 2 + n)?) != "END DIST" {
                return Err(bad(format!("unterminated DIST table {}", repr(line))));
            }
            i += 3 + n;
            if n == 0 {
                continue;
            }
            match kind {
                "RESOVER" => {
                    if rules.resover.contains_key(&(metal, a)) {
                        return Err(bad(format!(
                            "duplicate RESOVER context metal {metal} over {a}"
                        )));
                    }
                    rules
                        .resover
                        .insert((metal, a), rows.iter().map(|r| r[3]).collect());
                    continue;
                }
                "DIAGUNDER" => continue,
                _ => {}
            }
            let (over, under) = if kind == "OVER" {
                (a, b)
            } else {
                if b.is_some() {
                    return Err(bad("UNDER with second UNDER index"));
                }
                (0, Some(a))
            };
            let key = (metal, over, under);
            if rules.tables.contains_key(&key) {
                return Err(bad(format!(
                    "duplicate calibration context ({metal}, {over}, {})",
                    under.map_or("None".into(), |u| u.to_string())
                )));
            }
            let mut previous = -1.0;
            for row in &rows {
                if !row.iter().all(|x| x.is_finite()) || row[1].min(row[2]) < 0.0 {
                    return Err(bad(format!(
                        "non-finite/negative calibration value in {}",
                        repr(line)
                    )));
                }
                if row[0] <= previous {
                    return Err(bad(format!("non-increasing spacing in {}", repr(line))));
                }
                previous = row[0];
            }
            rules
                .tables
                .insert(key, rows.iter().map(|r| [r[0], r[1], r[2]]).collect());
            continue;
        }
        i += 1;
    }
    rules.layer_count = count.ok_or_else(|| bad("LayerCount missing"))?;
    Ok(rules)
}

/// `^DIST count (\d+) width (\S+)$` on the stripped line: single spaces.
fn dist_header(raw: &str) -> bool {
    let line = strip(raw);
    let Some(rest) = line.strip_prefix("DIST count ") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && rest[digits..]
            .strip_prefix(" width ")
            .is_some_and(|tail| !tail.is_empty() && !tail.contains(super::text::is_space))
}

/// `repr(str)` for the diagnostics above (single quotes unless the text
/// holds one and no double quote).
fn repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Insertion-ordered `{key: value}` with last-value-wins updates.
#[derive(Default, Clone)]
struct Ordered<V>(Vec<(String, V)>);
impl<V> Ordered<V> {
    fn set(&mut self, key: &str, value: V) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }
    fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Default, Clone)]
struct Magic {
    resist: Ordered<f64>,
    contact: Ordered<f64>,
    areacap: Ordered<f64>,
    perimeter: Ordered<f64>,
    sidewall: Ordered<[f64; 2]>,
    overlap: Vec<((String, String), f64)>,
    all_contacts: Ordered<f64>,
}

/// Read the extract section for one variant: resist/contact/area/perimeter/sidewall.
fn parse_magic(
    text: &str,
    variant: &str,
    types: &[String],
    cut_types: &[&str],
) -> StepResult<Magic> {
    let lines = splitlines(text);
    let start = lines
        .iter()
        .position(|line| strip(line) == "extract")
        .ok_or_else(|| bad("magic extract section missing"))?;
    let is_type = |name: &str| types.iter().any(|t| t == name);
    let mut active: Vec<&str> = vec!["*"];
    let mut out = Magic::default();
    for raw in &lines[start + 1..] {
        let line = strip(raw.split('#').next().unwrap_or(""));
        if line == "end" {
            break;
        }
        if line.is_empty() || line.starts_with("style") {
            continue;
        }
        let tokens = split(line);
        if tokens[0] == "variants" {
            let names = tokens
                .get(1)
                .ok_or_else(|| bad("magic variants line without names"))?;
            active = if *names == "*" {
                vec!["*"]
            } else {
                names.split(',').collect()
            };
            continue;
        }
        if !active.contains(&variant) && !active.contains(&"*") {
            continue;
        }
        match tokens[0] {
            "resist" if tokens.len() >= 3 => {
                // `(types)/plane`: the group must close before the slash.
                let name = tokens[1].strip_prefix('(').and_then(|rest| {
                    let (inner, tail) = rest.split_once(')')?;
                    (tail.len() > 1 && tail.starts_with('/')).then_some(inner)
                });
                if let Some(name) = name.filter(|name| is_type(name)) {
                    out.resist.set(name, number(tokens[2])?);
                }
            }
            "contact" if tokens.len() == 3 => {
                // Every contact line of the variant (cuts, diffusion, poly, MIM):
                // a block that gives all of them one value is not characterized.
                let value = number(tokens[2])?;
                out.all_contacts.set(tokens[1], value);
                if cut_types.contains(&tokens[1]) {
                    out.contact.set(tokens[1], value);
                }
            }
            "defaultareacap" if tokens.len() == 4 && is_type(tokens[1]) => {
                out.areacap.set(tokens[1], number(tokens[3])?);
            }
            "defaultperimeter" if tokens.len() == 4 && is_type(tokens[1]) => {
                out.perimeter.set(tokens[1], number(tokens[3])?);
            }
            "defaultoverlap" if tokens.len() == 6 && is_type(tokens[1]) && is_type(tokens[3]) => {
                let key = (tokens[1].to_owned(), tokens[3].to_owned());
                let value = number(tokens[5])?;
                match out.overlap.iter_mut().find(|(k, _)| *k == key) {
                    Some(entry) => entry.1 = value,
                    None => out.overlap.push((key, value)),
                }
            }
            "defaultsidewall" if matches!(tokens.len(), 4 | 5) && is_type(tokens[1]) => {
                let offset = if tokens.len() == 5 {
                    number(tokens[4])?
                } else {
                    0.0
                };
                out.sidewall.set(tokens[1], [number(tokens[3])?, offset]);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The cut-to-contact-type map must agree with the Magic contact/types sections.
fn check_magic_contacts(text: &str, spec: &Spec) -> StepResult<()> {
    let lines = splitlines(text);
    let mut aliases: Ordered<String> = Ordered::default();
    for line in &lines {
        let tokens = split(line);
        if tokens.len() == 2 && tokens[1].contains(',') {
            let names: Vec<&str> = tokens[1].split(',').collect();
            for alias in &names {
                if aliases.get(alias).is_none() {
                    aliases.set(alias, names[0].to_owned());
                }
            }
        }
    }
    let mut residues: Ordered<[String; 2]> = Ordered::default();
    let mut in_contact = false;
    for line in &lines {
        let s = strip(line);
        if s == "contact" {
            in_contact = true;
            continue;
        }
        if in_contact && s == "end" {
            break;
        }
        let tokens = split(s);
        if in_contact && tokens.len() == 3 {
            residues.set(tokens[0], [tokens[1].to_owned(), tokens[2].to_owned()]);
        }
    }
    for (index, (cut, ctype)) in spec.cuts.iter().enumerate() {
        let canonical = aliases.get(ctype).map_or(*ctype, String::as_str);
        let want = [magic_plane(index + 1), magic_plane(index + 2)];
        if residues.get(ctype) != Some(&want) && residues.get(canonical) != Some(&want) {
            return Err(bad(format!(
                "magic contact {ctype} ({canonical}) does not join ['{}', '{}'] for cut {cut}",
                want[0], want[1]
            )));
        }
    }
    Ok(())
}

fn rel(a: f64, b: f64) -> Value {
    if b == 0.0 {
        Value::Null
    } else {
        Value::Float((a - b) / b)
    }
}

/// Number of contact types when a corner variant gives every contact type
/// one and the same value although the nominal block distinguishes them.
fn corner_contacts_blanket(magic: &Magic, nominal: &Magic) -> Option<usize> {
    let values: Vec<f64> = magic.all_contacts.0.iter().map(|(_, v)| *v).collect();
    let mut nominal_values: Vec<u64> = nominal
        .all_contacts
        .0
        .iter()
        .map(|(_, v)| (v + 0.0).to_bits())
        .collect();
    nominal_values.sort_unstable();
    nominal_values.dedup();
    (values.len() >= 3
        && values.iter().all(|v| *v == values[0])
        && values[0] > 0.0
        && nominal_values.len() > 1)
        .then_some(values.len())
}

fn isolated_side(rules: &Rules, metal: i64, over: i64) -> Option<f64> {
    rules
        .tables
        .get(&(metal, over, None))
        .and_then(|rows| rows.last())
        .map(|row| row[2])
}

/// Face area capacitance to the substrate and over each lower routing layer
/// (pF/um^2): Magic nominal values moved to the corner by the table's ratio.
fn overlap_area(
    index: usize,
    rules: &Rules,
    rules_nom: &Rules,
    nominal: &Magic,
) -> StepResult<(f64, Vec<Value>)> {
    let mtype = magic_type(index);
    let metal = index as i64;
    let area = nominal.areacap.get(&mtype).copied();
    let (sub, sub_nom) = (
        isolated_side(rules, metal, 0),
        isolated_side(rules_nom, metal, 0),
    );
    let (Some(area), Some(sub), Some(sub_nom)) = (
        area,
        sub.filter(|v| *v != 0.0),
        sub_nom.filter(|v| *v != 0.0),
    ) else {
        return Err(bad(format!(
            "ihp: magic nominal area capacitance or substrate row missing for {mtype}"
        )));
    };
    let area_cap = area * (sub / sub_nom) * 1e-6;
    let mut out = Vec::new();
    for lower in 1..index {
        let lower_type = magic_type(lower);
        let ov = nominal
            .overlap
            .iter()
            .find(|((a, b), _)| *a == mtype && *b == lower_type)
            .map(|(_, v)| *v)
            .ok_or_else(|| {
                bad(format!(
                    "ihp: magic nominal overlap {mtype} over {lower_type} missing"
                ))
            })?;
        let (Some(plate), Some(plate_nom)) = (
            isolated_side(rules, metal, lower as i64),
            isolated_side(rules_nom, metal, lower as i64),
        ) else {
            return Err(bad(format!(
                "ihp: calibration plate context metal {index} over {lower} missing"
            )));
        };
        let (ratio, source) = if plate - sub > 0.0 && plate_nom - sub_nom > 0.0 {
            (
                (plate - sub) / (plate_nom - sub_nom),
                "plate excess of the pair",
            )
        } else {
            // A distant plate the table rates at or below the substrate row
            // has no usable excess; the substrate row's ratio moves it.
            (
                sub / sub_nom,
                "substrate row (pair plate excess not positive)",
            )
        };
        out.push(Value::Dict(
            Dict::new()
                .with("over", lower)
                .with("pf_per_um2", ov * ratio * 1e-6)
                .with("magic_nominal_af_per_um2", ov)
                .with("table_corner_ratio", ratio)
                .with("table_corner_ratio_source", source),
        ));
    }
    Ok((area_cap, out))
}

/// Build the profile object for one corner.
///
/// # Errors
/// `RC_PROFILE` on a missing or oversized data file, a table the model does
/// not support, or PDK files that disagree with each other.
#[allow(clippy::too_many_lines)]
pub fn build(request: &Request) -> StepResult<Value> {
    let spec = &IHP;
    let matched = request.model == ResistanceModel::OpenRcxPdk;
    let corner = request.corner;
    let variant = match corner {
        "nom" => "()",
        "max" => "(hrhc)",
        "min" => "(lrlc)",
        _ => return Err(bad(format!("unsupported corner {corner}"))),
    };
    let base = join(request.pdk_root, spec.base);
    let rules_for = |corner: &str| {
        join(
            &base,
            &format!("{}{corner}{}", spec.rules[0], spec.rules[1]),
        )
    };
    let lef_path = join(&base, request.technology_lef.unwrap_or(spec.technology_lef));
    let rules_path = request
        .rules_override
        .map_or_else(|| rules_for(corner), pure_path);
    let magic_path = join(&base, spec.magic);
    let extract_path = join(&base, spec.magic_extract);
    let lef_source = load(&lef_path)?;
    let lef = parse_lef(&lef_source.text)?;
    let rules_source = load(&rules_path)?;
    let rules = parse_rules(&rules_source.text)?;
    let nominal_rules_path = if request.rules_override.is_some() && corner == "nom" {
        rules_path.clone()
    } else {
        rules_for("nom")
    };
    let nominal_source = if corner == "nom" {
        None
    } else {
        Some(load(&nominal_rules_path)?)
    };
    let parsed_nominal = match &nominal_source {
        Some(source) => Some(parse_rules(&source.text)?),
        None => None,
    };
    let rules_nom = parsed_nominal.as_ref().unwrap_or(&rules);
    let magic_source = load(&magic_path)?;
    let extract_source = load(&extract_path)?;
    let magic_text = format!("{}\n{}", magic_source.text, extract_source.text);
    check_magic_contacts(&magic_text, spec)?;
    let types: Vec<String> = (1..=7).map(magic_type).collect();
    let cut_types: Vec<&str> = spec.cuts.iter().map(|(_, ctype)| *ctype).collect();
    let magic = parse_magic(&magic_text, variant, &types, &cut_types)?;
    let nominal = parse_magic(&magic_text, "()", &types, &cut_types)?;
    // The IHP technology LEF carries valid parasitics (cross-check data).
    if lef.placeholder_parasitics {
        return Err(bad(
            "ihp: LEF parasitic validity marker disagrees with the PDK profile table",
        ));
    }
    let routing: Vec<&LefLayer> = lef
        .layers
        .iter()
        .filter(|layer| layer.kind.as_deref() == Some("ROUTING"))
        .collect();
    if !routing.iter().map(|l| l.name.as_str()).eq(spec.routing) {
        return Err(bad(format!(
            "ihp: routing layer order [{}] differs from profile table",
            routing
                .iter()
                .map(|l| format!("'{}'", l.name))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if rules.layer_count != routing.len() as i64 {
        return Err(bad(format!(
            "ihp: calibration LayerCount {} != {} routing layers",
            rules.layer_count,
            routing.len()
        )));
    }
    let by_name = |name: &str| lef.layers.iter().rev().find(|layer| layer.name == name);
    let mut diagnostics: Vec<String> = vec![
        if matched {
            "wire resistance is taken from constant IHP RESOVER column4 (Ohm/nm); \
             via resistance is the original LEF cut value, fixed across corners"
        } else {
            "calibration column 4 is not used; resistance is taken from the Magic extract section"
        }
        .into(),
        "DIAGUNDER (diagonal adjacent-layer) tables are not modeled".into(),
        "via cut and via landing-pad self capacitance are not modeled".into(),
    ];
    let (mut layers, mut checks) = (Vec::new(), Vec::new());
    for (position, lef_layer) in routing.iter().enumerate() {
        let index = position + 1;
        let metal = index as i64;
        let name = lef_layer.name.as_str();
        let lef_width = lef_layer
            .width
            .ok_or_else(|| bad(format!("ihp: LEF WIDTH missing for {name}")))?;
        let width = match rules.widths.get(&metal) {
            Some(width) if (width - lef_width).abs() <= 1e-9 => *width,
            other => {
                return Err(bad(format!(
                    "ihp: calibration width for {name} ({}) != LEF WIDTH {lef_width}",
                    other.map_or("None".into(), |w| w.to_string())
                )));
            }
        };
        let mtype = magic_type(index);
        let sheet = *magic.resist.get(&mtype).ok_or_else(|| {
            bad(format!(
                "ihp: magic sheet resistance missing for {mtype} {variant}"
            ))
        })?;
        let mut sheet_ohm = sheet / 1000.0;
        if matched {
            let resistance = rules
                .resover
                .get(&(metal, 0))
                .filter(|values| {
                    !values.is_empty() && values.iter().all(|v| 0.0 < *v && *v < f64::INFINITY)
                })
                .ok_or_else(|| {
                    bad(format!(
                        "ihp: invalid/missing RESOVER0 resistance for metal {index}"
                    ))
                })?;
            if resistance.iter().any(|v| (v - resistance[0]).abs() > 1e-15) {
                return Err(bad(
                    "ihp: context-dependent RESOVER resistance is not supported",
                ));
            }
            sheet_ohm = resistance[0] * 1000.0 * width;
        }
        // Sorted by (metal, over, under or -1): the map order with `None` first.
        let contexts: Vec<Context> = rules
            .tables
            .iter()
            .filter(|((m, _, _), _)| *m == metal)
            .map(|((_, over, under), rows)| (*over, *under, rows))
            .collect();
        let over0 = contexts
            .iter()
            .find(|(over, under, _)| *over == 0 && under.is_none())
            .map(|(_, _, rows)| *rows)
            .ok_or_else(|| bad(format!("ihp: no OVER 0 calibration context for {name}")))?;
        let (area_cap, overlap) = overlap_area(index, &rules, rules_nom, &nominal)?;
        let rendered: Vec<Value> = contexts
            .iter()
            .map(|(over, under, rows)| {
                Value::Dict(Dict::new().with("over", *over).with("under", *under).with(
                    "rows",
                    list(rows.iter().map(|row| list(row.iter().copied()))),
                ))
            })
            .collect();
        layers.push(Value::Dict(
            Dict::new()
                .with("name", name)
                .with("rules_metal", index)
                .with("width_um", width)
                .with("sheet_resistance_ohm_per_square", sheet_ohm)
                .with("contexts", list(rendered))
                .with("area_cap_pf_per_um2", area_cap)
                .with("overlap_area", list(overlap)),
        ));
        let (first, last) = (
            over0.first().ok_or_else(|| bad("empty OVER 0 table"))?,
            over0.last().ok_or_else(|| bad("empty OVER 0 table"))?,
        );
        let isolated = 2.0 * last[2];
        let (s0, cc0) = (first[0], first[1]);
        let mut check = Dict::new()
            .with("layer", name)
            .with("rules_isolated_ground_pf_per_um", isolated)
            .with("rules_min_spacing_um", s0)
            .with("rules_coupling_at_min_spacing_pf_per_um", cc0)
            .with("sheet_resistance_ohm_per_square", sheet_ohm);
        if matched {
            check.insert(
                "resistance_source",
                format!("IHP RESOVER metal{index} over0, column4 Ohm/nm"),
            );
            check.insert("magic_sheet_resistance_ohm_per_square", sheet / 1000.0);
            check.insert("magic_relative_difference", rel(sheet_ohm, sheet / 1000.0));
        }
        if let (Some(area), Some(perimeter)) =
            (magic.areacap.get(&mtype), magic.perimeter.get(&mtype))
        {
            let m_iso = (area * width + 2.0 * perimeter) * 1e-6;
            check.insert("magic_isolated_ground_pf_per_um", m_iso);
            check.insert("isolated_ground_relative_difference", rel(isolated, m_iso));
        }
        if let Some(side) = magic.sidewall.get(&mtype) {
            let m_cc = side[0] / (s0 + side[1]) * 1e-6;
            check.insert("magic_coupling_at_min_spacing_pf_per_um", m_cc);
            check.insert("coupling_relative_difference", rel(cc0, m_cc));
        }
        if let Some(rpersq) = lef_layer.rpersq {
            let nominal_sheet = nominal.resist.get(&mtype).ok_or_else(|| {
                bad(format!(
                    "ihp: magic nominal sheet resistance missing for {mtype}"
                ))
            })?;
            check.insert("lef_rpersq_ohm", rpersq);
            check.insert(
                "lef_vs_magic_nominal_resistance_relative_difference",
                rel(rpersq, nominal_sheet / 1000.0),
            );
        }
        if let (Some(cpersqdist), Some(edgecap)) = (lef_layer.cpersqdist, lef_layer.edgecap) {
            check.insert(
                "lef_isolated_ground_pf_per_um_nominal",
                cpersqdist * width + 2.0 * edgecap,
            );
        }
        checks.push(Value::Dict(check));
    }
    let order: Vec<&str> = lef.layers.iter().map(|l| l.name.as_str()).collect();
    let position = |name: &str| order.iter().position(|n| *n == name);
    let mut cuts = Vec::new();
    for (index, (cut, ctype)) in spec.cuts.iter().enumerate() {
        let cut_layer = by_name(cut)
            .filter(|layer| layer.kind.as_deref() == Some("CUT"))
            .ok_or_else(|| bad(format!("ihp: cut layer {cut} missing from LEF")))?;
        let (lower, upper) = (spec.routing[index], spec.routing[index + 1]);
        match (position(lower), position(cut), position(upper)) {
            (Some(l), Some(c), Some(u)) if l < c && c < u => {}
            _ => {
                return Err(bad(format!(
                    "ihp: LEF order does not place {cut} between {lower} and {upper}"
                )));
            }
        }
        let mut value = *magic.contact.get(ctype).ok_or_else(|| {
            bad(format!(
                "ihp: magic contact resistance missing for {ctype} {variant}"
            ))
        })?;
        let mut source = format!("magic contact {ctype} {variant}");
        let blanket = corner_contacts_blanket(&magic, &nominal);
        let nominal_contact = || {
            nominal.contact.get(ctype).copied().ok_or_else(|| {
                bad(format!(
                    "ihp: magic nominal contact resistance missing for {ctype}"
                ))
            })
        };
        if value <= 0.0 {
            let fallback = nominal_contact()?;
            diagnostics.push(format!(
                "VIA_R_CORNER_UNAVAILABLE: magic {variant} lists {ctype} contact resistance {} mOhm \
                 (all contacts are 0 in that block, i.e. no corner data); using nominal {} mOhm",
                format_g(value),
                format_g(fallback)
            ));
            value = fallback;
            source = format!("magic contact {ctype} () fallback");
        } else if let Some(blanket) = blanket {
            let fallback = nominal_contact()?;
            diagnostics.push(format!(
                "VIA_R_CORNER_UNAVAILABLE: magic {variant} gives every one of its {blanket} contact types \
                 (diffusion, poly, via and MIM contacts alike) the same {} mOhm while the nominal \
                 block distinguishes them, i.e. a placeholder, not corner data; using nominal \
                 {} mOhm for {ctype}",
                format_g(value),
                format_g(fallback)
            ));
            value = fallback;
            source = format!("magic contact {ctype} () fallback (blanket {variant} block)");
        }
        if matched {
            let lef_cut = cut_layer
                .resistance
                .filter(|r| 0.0 < *r && *r < f64::INFINITY)
                .ok_or_else(|| bad(format!("ihp: LEF cut resistance absent/invalid for {cut}")))?;
            value = lef_cut * 1000.0;
            source =
                format!("original technology LEF LAYER {cut} RESISTANCE (fixed across RC corners)");
        }
        let mut entry = Dict::new()
            .with("name", *cut)
            .with("lower", lower)
            .with("upper", upper)
            .with("resistance_ohm_per_cut", value / 1000.0)
            .with("source", source);
        if let Some(resistance) = cut_layer.resistance {
            entry.insert("lef_resistance_ohm_nominal", resistance);
        }
        cuts.push(Value::Dict(entry));
    }
    let mut sources = vec![
        fingerprint("capacitance_calibration_table", &rules_path, &rules_source),
        fingerprint("resistance_and_crosscheck_tech", &magic_path, &magic_source),
        fingerprint("technology_lef", &lef_path, &lef_source),
        fingerprint("resistance_extract_tech", &extract_path, &extract_source),
    ];
    if let Some(source) = &nominal_source {
        sources.push(fingerprint(
            "nominal_calibration_table_for_overlap_corner_ratio",
            &nominal_rules_path,
            source,
        ));
    }
    let mut model = Dict::new()
        .with(
            "rows",
            "[edge_spacing_um, coupling_pf_per_um_to_that_neighbour, ground_pf_per_um_for_that_side]",
        )
        .with(
            "per_side",
            "each wire side is looked up separately; an isolated wire has 2 * ground(last row)",
        )
        .with(
            "beyond_last_row",
            "coupling 0, ground = last-row ground (isolated side)",
        )
        .with(
            "context",
            "over = calibration metal index of the nearest conductor below (0 = substrate), under = nearest above",
        )
        .with(
            "resistance",
            "sheet_resistance * centerline_length / width; via = resistance_ohm_per_cut / cuts",
        )
        .with(
            "overlap_area",
            "metal of another layer that covers part of a wire face without forming a \
             calibration plate couples over the covered width at overlap_area.pf_per_um2 \
             (counted once, from the upper face), and the covered face loses \
             area_cap_pf_per_um2 to the substrate; values are the Magic nominal \
             defaultoverlap/defaultareacap times the calibration table's corner ratio \
             (table_corner_ratio: plate excess of the same pair; substrate row for area_cap)",
        );
    if matched {
        model.insert(
            "resistance_sources",
            Dict::new()
                .with(
                    "wire",
                    format!("{rules_path}: constant RESOVER0 column4 Ohm/nm x1000 xwidth_um"),
                )
                .with(
                    "cut",
                    format!("{lef_path}: LAYER CUT RESISTANCE, fixed across RC corners"),
                )
                .with(
                    "independent_cross_check",
                    format!("{magic_path} and {extract_path}: Magic sheet/contact values"),
                )
                .with(
                    "matching_scope",
                    "use one nominal SPEF for TT/SS Liberty PVT comparison",
                ),
        );
    }
    Ok(Value::Dict(
        Dict::new()
            .with("schema", "rustroad.rc-profile.v1")
            .with("pdk", "ihp")
            .with("library", spec.library)
            .with("corner", corner)
            .with("pdk_version", spec.pdk_version)
            .with("technology_lef_sha256", lef_source.sha256.as_str())
            .with(
                "units",
                Dict::new()
                    .with("length", "um")
                    .with("capacitance", "pF")
                    .with("resistance", "ohm"),
            )
            .with("magic_variant", variant)
            .with(
                "lef_parasitics",
                if matched {
                    "cut_resistance_used; routing_RC_is_cross_check_only"
                } else {
                    "cross_check_only"
                },
            )
            .with("sources", list(sources))
            .with("model", model)
            .with("layers", list(layers))
            .with("cuts", list(cuts))
            .with("cross_checks", list(checks))
            .with("diagnostics", list(diagnostics))
            .with("generator", "scripts/build_rc_profile.py"),
    ))
}

/// `json.dumps(profile, indent=1) + "\n"`, the profile file's text.
///
/// # Errors
/// `RC_PROFILE` if the profile holds a non-finite number.
pub fn render(profile: &Value) -> StepResult<String> {
    let mut text = super::render(profile, Layout::Indent(1), CODE)?;
    text.push('\n');
    Ok(text)
}
