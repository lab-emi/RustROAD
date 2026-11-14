//! Project a KLayout LEF/DEF layer map onto `rustroad.gds-layer-map.v1`.
//!
//! The source is the PDK's `<layer> <purposes> <gds layer> <datatype>` map
//! (four whitespace-separated columns; other lines are not part of the
//! projection). For each requested LEF layer the entry lists the GDS
//! layer/datatype pairs of its `NET`, `SPNET`, `VIA` and `PIN` purposes, the
//! pin-label pair of its `NAME` row, and the source lines it was read from.
//! A cut layer keeps only its `VIA` pairs. `DIEAREA` gives the boundary layer.
//!
//! Two forms are written. [`Form::Ihp`] is the projection of the Ibex
//! SG13G2/EZ130 recipe, unchanged: the label row is `NAME <layer>/PIN`, every
//! row that names the layer is cited, a cut entry carries empty lists, and the
//! source is called `ihp_map`. [`Form::Pdk`] is the form of the tracked
//! `fixtures/pdk/*-gds-map.json` files: the label row is a `NAME` row that
//! lists `<layer>/LABEL` or `<layer>/PIN` among its purposes (the SKY130 and
//! GF180 maps write `NAME <layer>/LABEL[,<layer>/LEFPIN]`), only the rows the
//! entry was read from are cited, a cut entry has its `via` pairs only, and
//! the source is called `klayout_map`.
use super::text::{int, split, splitlines};
use super::{StepResult, fail, file_id, read_text, require};
use rustroad_pyjson::{Dict, Value, list};
use std::path::Path;

const CODE: &str = "LAYER_MAP";

/// The IHP SG13G2 metal stack, bottom to top, as the Ibex recipe exports it.
pub const IHP_SG13G2_LAYERS: [&str; 13] = [
    "Metal1",
    "Via1",
    "Metal2",
    "Via2",
    "Metal3",
    "Via3",
    "Metal4",
    "Via4",
    "Metal5",
    "TopVia1",
    "TopMetal1",
    "TopVia2",
    "TopMetal2",
];

struct Row<'a> {
    name: &'a str,
    purpose: &'a str,
    layer: i64,
    datatype: i64,
    line: usize,
}

fn pair(layer: i64, datatype: i64) -> Value {
    list([layer, datatype])
}

/// Which of the two forms of the module documentation is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Ihp,
    Pdk,
}

/// One LEF layer to project, bottom to top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layer<'a> {
    pub name: &'a str,
    /// A cut layer (`TYPE CUT` in the technology LEF).
    pub cut: bool,
}

/// The IHP SG13G2 stack: a layer whose name contains `Via` is a cut layer.
#[must_use]
pub fn ihp_layers<'a>(names: &[&'a str]) -> Vec<Layer<'a>> {
    names
        .iter()
        .map(|name| Layer {
            name,
            cut: name.contains("Via"),
        })
        .collect()
}

/// The requested layers with their kind from the technology LEF: a layer is a
/// cut layer when the LEF declares it `TYPE CUT`, a routing layer when it
/// declares it `TYPE ROUTING`.
///
/// # Errors
/// `LAYER_MAP` on a layer the LEF does not declare as a routing or cut layer,
/// or a LEF over 2 MiB.
pub fn lef_layers<'a>(technology_lef: &Path, names: &[&'a str]) -> StepResult<Vec<Layer<'a>>> {
    let text = read_text(technology_lef, 2 << 20, CODE)?;
    let mut kinds: Vec<(&str, &str)> = Vec::new();
    let mut current: Option<&str> = None;
    for raw in splitlines(&text) {
        // A layer is declared by `LAYER <name>` on a line of its own; `LAYER <name> ;` is a
        // geometry statement of a via or a macro.
        let tokens = split(raw.split('#').next().unwrap_or(""));
        match (current, tokens.as_slice()) {
            (None, ["LAYER", name]) => current = Some(name),
            (Some(layer), ["END", name]) if layer == *name => current = None,
            (Some(layer), ["TYPE", kind, ..]) => kinds.push((layer, kind.trim_end_matches(';'))),
            _ => {}
        }
    }
    names
        .iter()
        .map(|name| {
            // The last declaration of a layer wins, as LEF readers take it.
            match kinds.iter().rev().find(|(layer, _)| layer == name) {
                Some((_, "CUT")) => Ok(Layer { name, cut: true }),
                Some((_, "ROUTING")) => Ok(Layer { name, cut: false }),
                _ => Err(fail(
                    CODE,
                    format!("{name} is not a routing or cut layer of the technology LEF"),
                )),
            }
        })
        .collect()
}

/// Build the layer-map profile for `layers`.
///
/// # Errors
/// `LAYER_MAP` on a missing layer, incomplete geometry purposes, a missing or
/// repeated die boundary, or more than 4096 rows / 2 MiB of text.
pub fn project(
    path: &Path,
    pdk: &str,
    library: &str,
    layers: &[Layer],
    form: Form,
) -> StepResult<Value> {
    let data = read_text(path, 2 << 20, CODE)?;
    let source = file_id(path, 2 << 20, CODE)?;
    let mut rows = Vec::new();
    for (index, line) in splitlines(&data).into_iter().enumerate() {
        let columns = split(line);
        if columns.len() == 4 && !columns[0].starts_with('#') {
            // A row whose last two columns are not integers is another map construct.
            if let (Some(layer), Some(datatype)) = (int(columns[2]), int(columns[3])) {
                rows.push(Row {
                    name: columns[0],
                    purpose: columns[1],
                    layer,
                    datatype,
                    line: index + 1,
                });
            }
        }
    }
    require(rows.len() <= 4096, CODE, "layer-map row budget")?;
    let source_id = match form {
        Form::Ihp => "ihp_map",
        Form::Pdk => "klayout_map",
    };
    let cite = |lines: Vec<usize>| {
        list([Dict::new()
            .with("source", source_id)
            .with("sha256", source.sha256.as_str())
            .with("lines", list(lines))])
    };
    let mut entries = Vec::new();
    for Layer { name, cut } in layers.iter().copied() {
        let label_purpose = format!("{name}/PIN");
        let label_purposes = [format!("{name}/LABEL"), label_purpose.clone()];
        // Whether a `NAME` row gives this layer's label pair.
        let labels = |row: &Row| match form {
            Form::Ihp => row.purpose == label_purpose,
            Form::Pdk => row
                .purpose
                .split(',')
                .any(|part| label_purposes.iter().any(|purpose| purpose == part)),
        };
        // Whether a row of the layer is its label row.
        let is_label = |row: &Row| match form {
            Form::Ihp => row.purpose == label_purpose,
            Form::Pdk => row.name == "NAME" && labels(row),
        };
        let own: Vec<&Row> = rows
            .iter()
            .filter(|r| r.name == name || (r.name == "NAME" && labels(r)))
            .collect();
        require(
            !own.is_empty(),
            CODE,
            format!("missing mapped layer {name}"),
        )?;
        let mut geometry: [Vec<(i64, i64)>; 4] = Default::default();
        let mut label = None;
        // The rows the entry was read from.
        let mut used: Vec<usize> = Vec::new();
        for row in &own {
            if is_label(row) {
                if !cut || form == Form::Ihp {
                    label = Some((row.layer, row.datatype));
                    used.push(row.line);
                }
                continue;
            }
            for (slot, token) in ["NET", "SPNET", "VIA", "PIN"].into_iter().enumerate() {
                let is_via = token == "VIA";
                if row.purpose.split(',').any(|part| part == token) && !(cut && !is_via) {
                    if !used.contains(&row.line) {
                        used.push(row.line);
                    }
                    if !geometry[slot].contains(&(row.layer, row.datatype)) {
                        geometry[slot].push((row.layer, row.datatype));
                    }
                }
            }
        }
        let [net, special_net, via, pin] = geometry;
        require(
            !via.is_empty()
                && (cut
                    || (!net.is_empty()
                        && !special_net.is_empty()
                        && !pin.is_empty()
                        && label.is_some())),
            CODE,
            format!("incomplete mapped geometry: {name}"),
        )?;
        let pairs = |items: Vec<(i64, i64)>| list(items.into_iter().map(|(l, d)| pair(l, d)));
        let entry = Dict::new()
            .with("lef", name)
            .with("kind", if cut { "cut" } else { "routing" });
        entries.push(Value::Dict(match form {
            Form::Ihp => entry
                .with("net", pairs(net))
                .with("special_net", pairs(special_net))
                .with("via", pairs(via))
                .with("pin", pairs(pin))
                .with("label", label.map(|(l, d)| pair(l, d)))
                .with("cite", cite(own.iter().map(|r| r.line).collect())),
            Form::Pdk if cut => entry.with("via", pairs(via)).with("cite", cite(used)),
            Form::Pdk => entry
                .with("net", pairs(net))
                .with("special_net", pairs(special_net))
                .with("via", pairs(via))
                .with("pin", pairs(pin))
                .with("label", label.map(|(l, d)| pair(l, d)))
                .with("cite", cite(used)),
        }));
    }
    let boundary: Vec<&Row> = rows.iter().filter(|r| r.name == "DIEAREA").collect();
    let [boundary] = boundary.as_slice() else {
        return Err(fail(CODE, "unique die boundary map required"));
    };
    Ok(Value::Dict(
        Dict::new()
            .with("schema", "rustroad.gds-layer-map.v1")
            .with("pdk", pdk)
            .with("library", library)
            .with(
                "sources",
                list([Dict::new()
                    .with("id", source_id)
                    .with("role", "klayout_lefdef_layer_map")
                    .with("path", source.path.as_str())
                    .with("sha256", source.sha256.as_str())
                    .with("bytes", source.bytes)]),
            )
            .with("layers", list(entries))
            .with(
                "boundary",
                Dict::new()
                    .with("layer", pair(boundary.layer, boundary.datatype))
                    .with("cite", cite(vec![boundary.line])),
            ),
    ))
}
