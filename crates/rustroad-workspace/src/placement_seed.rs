//! Bounded read-only placement seeds for a fresh workspace after logical ECOs.

use crate::physical::{MAX_WEIGHTED_NETS, PLACEMENT_SEED_SCHEMA, PlacementSeed};
use crate::{Error, Result, Snapshot, Stage};
use std::collections::{BTreeMap, HashSet};

/// Maximum encoded seed, matching the `place --seed` input limit.
pub const MAX_OUTPUT_BYTES: usize = 64 << 20;

/// Capture placed logical cells, omitting generated filler and tap instances.
///
/// The source records the current revision and imported netlist identity.
/// Coordinates use the source design's integer DBU. The returned seed is a
/// starting point for legalization, rather than a fixed-placement constraint.
/// Additional state is bounded by one million cells and a conservative 64 MiB
/// JSON-size estimate before cloning names into the ordered output table.
///
/// # Errors
/// Returns an error for an unplaced snapshot or an exhausted output budget.
pub fn from_snapshot(snapshot: &Snapshot) -> Result<PlacementSeed> {
    let metadata = snapshot.metadata();
    if metadata.stage != Stage::Placed {
        return Err(Error::Invalid(
            "place the design before exporting a placement seed".into(),
        ));
    }
    let design = &metadata.design;
    if design.cells.len() > MAX_WEIGHTED_NETS {
        return Err(Error::Budget("placement seed: too many cells".into()));
    }
    let mut excluded = HashSet::new();
    if let Some(bundle) = &metadata.physical {
        for name in bundle
            .generated_fillers
            .iter()
            .chain(&bundle.generated_taps)
        {
            excluded.insert(name.as_str());
        }
    }
    // Six output bytes per input byte bounds JSON escaping. The fixed allowance
    // includes indentation, two signed i64 coordinates and object delimiters.
    let mut encoded = 1024usize.saturating_add(design.name.len().saturating_mul(6));
    for cell in &design.cells {
        if !excluded.contains(cell.name.as_str()) {
            encoded = encoded.saturating_add(cell.name.len().saturating_mul(6) + 128);
            if encoded > MAX_OUTPUT_BYTES {
                return Err(Error::Budget(format!(
                    "placement seed needs at most {encoded} encoded bytes, limit {MAX_OUTPUT_BYTES}"
                )));
            }
        }
    }
    let mut cells = BTreeMap::new();
    for (cell, placed) in design.cells.iter().zip(snapshot.records()) {
        if !excluded.contains(cell.name.as_str()) {
            cells.insert(cell.name.clone(), [placed.x, placed.y]);
        }
    }
    let imported_netlist = metadata.physical.as_ref().and_then(|bundle| {
        bundle
            .sources
            .iter()
            .find(|source| source.role == "mapped_verilog")
    });
    let seed = PlacementSeed {
        schema: PLACEMENT_SEED_SCHEMA.into(),
        source: serde_json::json!({
            "revision": metadata.revision,
            "design": design.name,
            "units_per_micron": design.units_per_micron,
            "imported_netlist_sha256": imported_netlist.map(|source| &source.sha256),
            "excluded_generated_cells": excluded.len(),
        }),
        cells,
    };
    seed.validate()?;
    Ok(seed)
}
