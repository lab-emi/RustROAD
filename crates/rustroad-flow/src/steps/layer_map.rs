//! Project a KLayout LEF/DEF layer map onto `rustroad.gds-layer-map.v1`.
//!
//! The source is the PDK's `<layer> <purposes> <gds layer> <datatype>` map
//! (four whitespace-separated columns; other lines are not part of the
//! projection). For each requested LEF layer the entry lists the GDS
//! layer/datatype pairs of its `NET`, `SPNET`, `VIA` and `PIN` purposes, the
//! pin-label pair of its `NAME <layer>/PIN` row, and the source lines it was
//! read from. A cut layer keeps only its `VIA` pairs. `DIEAREA` gives the
//! boundary layer.
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

/// Build the layer-map profile for `layers` (a layer whose name contains
/// `Via` is a cut layer).
///
/// # Errors
/// `LAYER_MAP` on a missing layer, incomplete geometry purposes, a missing or
/// repeated die boundary, or more than 4096 rows / 2 MiB of text.
pub fn project(path: &Path, pdk: &str, library: &str, layers: &[&str]) -> StepResult<Value> {
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
    let cite = |lines: Vec<usize>| {
        list([Dict::new()
            .with("source", "ihp_map")
            .with("sha256", source.sha256.as_str())
            .with("lines", list(lines))])
    };
    let mut entries = Vec::new();
    for name in layers {
        let label_purpose = format!("{name}/PIN");
        let own: Vec<&Row> = rows
            .iter()
            .filter(|r| r.name == *name || (r.name == "NAME" && r.purpose == label_purpose))
            .collect();
        require(
            !own.is_empty(),
            CODE,
            format!("missing mapped layer {name}"),
        )?;
        let cut = name.contains("Via");
        let mut geometry: [Vec<(i64, i64)>; 4] = Default::default();
        let mut label = None;
        for row in &own {
            if row.purpose == label_purpose {
                label = Some((row.layer, row.datatype));
                continue;
            }
            for (slot, token) in ["NET", "SPNET", "VIA", "PIN"].into_iter().enumerate() {
                let is_via = token == "VIA";
                if row.purpose.split(',').any(|part| part == token)
                    && !(cut && !is_via)
                    && !geometry[slot].contains(&(row.layer, row.datatype))
                {
                    geometry[slot].push((row.layer, row.datatype));
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
        entries.push(Value::Dict(
            Dict::new()
                .with("lef", *name)
                .with("kind", if cut { "cut" } else { "routing" })
                .with("net", pairs(net))
                .with("special_net", pairs(special_net))
                .with("via", pairs(via))
                .with("pin", pairs(pin))
                .with("label", label.map(|(l, d)| pair(l, d)))
                .with("cite", cite(own.iter().map(|r| r.line).collect())),
        ));
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
                    .with("id", "ihp_map")
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
