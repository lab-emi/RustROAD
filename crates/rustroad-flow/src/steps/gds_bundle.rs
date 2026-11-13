//! Merge PDK cell GDS libraries into one stream without touching geometry.
//!
//! Each source library is walked record by record; every structure
//! (`BGNSTR`..`ENDSTR`) is copied byte for byte. The output is the first
//! library's header (through `UNITS`), the structures sorted by name, and
//! `ENDLIB`. Libraries must share their `UNITS` record: nothing is rescaled.
//! A structure name that occurs twice must have identical bytes.
//!
//! Caps: 32 MiB of sources and of output, one million records, 8192
//! structures, 512-byte names.
use super::{StepResult, fail, file_id, read_bytes, require};
use rustroad_pyjson::{Dict, Value, list};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CODE: &str = "GDS_BUNDLE";
const MAX_BYTES: u64 = 32 << 20;
const MAX_RECORDS: u64 = 1_000_000;
const MAX_STRUCTURES: usize = 8192;

/// Bundle `sources` into `output` and describe the result
/// (`sources`, `output`, `structures`, `records`, `scope`).
///
/// # Errors
/// `GDS_BUNDLE` on a malformed stream, differing units, a conflicting
/// structure or an exceeded cap; I/O errors.
pub fn bundle(sources: &[PathBuf], output: &Path) -> StepResult<Value> {
    let mut blocks: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut units: Option<Vec<u8>> = None;
    let mut header: Option<Vec<u8>> = None;
    let (mut records, mut total) = (0u64, 0u64);
    for path in sources {
        total += std::fs::metadata(path)
            .map_err(|e| fail(CODE, format!("{}: {e}", path.display())))?
            .len();
        require(
            total <= MAX_BYTES,
            CODE,
            "aggregate original GDS byte budget",
        )?;
        let data = read_bytes(path, MAX_BYTES, CODE)?;
        let mut offset = 0usize;
        let mut start: Option<usize> = None;
        let mut name: Option<Vec<u8>> = None;
        let mut current_units: Option<&[u8]> = None;
        let mut header_end = 0usize;
        let mut end_library = false;
        while offset < data.len() {
            records += 1;
            require(
                records <= MAX_RECORDS && offset + 4 <= data.len(),
                CODE,
                "GDS record budget/truncation",
            )?;
            let size = usize::from(u16::from_be_bytes([data[offset], data[offset + 1]]));
            let (kind, dtype) = (data[offset + 2], data[offset + 3]);
            require(
                size >= 4 && size % 2 == 0 && offset + size <= data.len(),
                CODE,
                "malformed GDS record",
            )?;
            let payload = &data[offset + 4..offset + size];
            match kind {
                3 => {
                    require(
                        dtype == 5 && payload.len() == 16 && current_units.is_none(),
                        CODE,
                        "GDS units record",
                    )?;
                    current_units = Some(payload);
                    header_end = offset + size;
                }
                5 => {
                    require(
                        start.is_none() && current_units.is_some(),
                        CODE,
                        "nested/unscaled GDS structure",
                    )?;
                    start = Some(offset);
                }
                6 => {
                    require(
                        start.is_some() && name.is_none(),
                        CODE,
                        "misplaced GDS structure name",
                    )?;
                    let end = payload
                        .iter()
                        .rposition(|byte| *byte != 0)
                        .map_or(0, |at| at + 1);
                    let text = &payload[..end];
                    require(text.is_ascii(), CODE, "GDS structure name is not ASCII")?;
                    require(
                        !text.is_empty() && text.len() <= 512,
                        CODE,
                        "GDS structure name budget",
                    )?;
                    name = Some(text.to_vec());
                }
                7 => {
                    let (Some(first), Some(key)) = (start.take(), name.take()) else {
                        return Err(fail(CODE, "unmatched GDS ENDSTR"));
                    };
                    let raw = &data[first..offset + size];
                    match blocks.get(&key) {
                        Some(known) => require(
                            known.as_slice() == raw,
                            CODE,
                            "conflicting original GDS structure",
                        )?,
                        None => {
                            blocks.insert(key, raw.to_vec());
                        }
                    }
                    require(
                        blocks.len() <= MAX_STRUCTURES,
                        CODE,
                        "GDS structure count budget",
                    )?;
                }
                4 => {
                    require(
                        start.is_none() && data[offset + size..].iter().all(|byte| *byte == 0),
                        CODE,
                        "GDS trailing/unfinished structure",
                    )?;
                    end_library = true;
                }
                _ => {}
            }
            if end_library {
                break;
            }
            offset += size;
        }
        let Some(current) = current_units.filter(|_| end_library) else {
            return Err(fail(CODE, "incomplete GDS library"));
        };
        require(
            units.as_deref().is_none_or(|known| known == current),
            CODE,
            "GDS units differ; implicit rescale forbidden",
        )?;
        units = Some(current.to_vec());
        if header.is_none() {
            header = Some(data[..header_end].to_vec());
        }
    }
    let Some(mut result) = header.filter(|_| !blocks.is_empty()) else {
        return Err(fail(CODE, "empty GDS bundle"));
    };
    // ASCII names: byte order is Python's string order.
    for raw in blocks.values() {
        result.extend_from_slice(raw);
    }
    result.extend_from_slice(&[0, 4, 4, 0]);
    require(
        result.len() as u64 <= MAX_BYTES,
        CODE,
        "GDS output byte budget",
    )?;
    std::fs::write(output, &result)
        .map_err(|e| fail(CODE, format!("{}: {e}", output.display())))?;
    let mut ids = Vec::new();
    for path in sources {
        ids.push(file_id(path, MAX_BYTES, CODE)?.to_value());
    }
    Ok(Value::Dict(
        Dict::new()
            .with("sources", list(ids))
            .with("output", file_id(output, MAX_BYTES, CODE)?.to_value())
            .with("structures", blocks.len())
            .with("records", records)
            .with(
                "scope",
                "byte-identical original structures; EZ header; no geometric edits",
            ),
    ))
}
