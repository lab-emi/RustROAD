//! Explicit routed identities, including nets without observable antenna geometry.
use super::{SourceCheck, fail, report, source_fingerprint, verified_lef};
use crate::{Error, Result, SnapshotCaps};
use rustroad_core::Limits;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

/// Bounds checked before route decoding and before retaining names.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryBounds {
    pub max_logical_nets: usize,
    pub max_routed_ids: usize,
    pub max_name_bytes: usize,
    pub max_route_bytes: u64,
    pub max_work: usize,
    pub max_output_bytes: usize,
    pub memory_bytes: usize,
}
impl Default for InventoryBounds {
    fn default() -> Self {
        Self {
            max_logical_nets: 250_000,
            max_routed_ids: 1_000_000,
            max_name_bytes: 16 << 20,
            max_route_bytes: 256 << 20,
            max_work: 10_000_000,
            max_output_bytes: 64 << 20,
            memory_bytes: 64 << 20,
        }
    }
}
impl InventoryBounds {
    fn validate(&self) -> Result<()> {
        if !(1..=1_000_000).contains(&self.max_logical_nets)
            || !(1..=1_000_000).contains(&self.max_routed_ids)
            || !(1..=64 << 20).contains(&self.max_name_bytes)
            || !(64..=512 << 20).contains(&self.max_route_bytes)
            || !(1..=100_000_000).contains(&self.max_work)
            || !(1..=128 << 20).contains(&self.max_output_bytes)
            || !(1..=128 << 20).contains(&self.memory_bytes)
        {
            return Err(Error::Budget("ANTENNA_INVENTORY: invalid bounds".into()));
        }
        Ok(())
    }
}

/// An actual logical member of `routes.nets`, independent of conductor count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryEntry {
    pub net_id: u32,
    pub canonical_name: String,
}

/// Identity-only evidence; this command does not perform an antenna or signoff check.
#[derive(Debug, Serialize)]
pub struct InventoryReport {
    pub schema: &'static str,
    pub workspace_revision: String,
    pub design: String,
    pub routing_scope: String,
    pub source_sha256: String,
    pub inventory_sha256: String,
    pub model_logical_net_count: usize,
    pub routed_logical_net_count: usize,
    pub router_declared_net_count: usize,
    pub excluded_nonlogical_supply_tie_count: usize,
    pub nets: Vec<InventoryEntry>,
    pub sources: Vec<SourceCheck>,
    pub bounds: InventoryBounds,
    /// Counted source-object scans, routed IDs and bounded sorting comparisons.
    pub work: usize,
    pub retained_name_bytes: usize,
    pub estimated_inventory_bytes: usize,
    pub status: &'static str,
    pub antenna_status: &'static str,
    pub drc_status: &'static str,
    pub lvs_status: &'static str,
    pub timing_status: &'static str,
    #[serde(skip)]
    _lock: Option<std::fs::File>,
}

struct CountBytes {
    bytes: usize,
    cap: usize,
}
impl Write for CountBytes {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(data.len())
            .filter(|&n| n <= self.cap)
            .ok_or_else(|| io::Error::other("ANTENNA_INVENTORY: output byte budget"))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl InventoryReport {
    /// Stream the bounded report after a counting pass, without a second JSON buffer.
    ///
    /// # Errors
    /// Fails on the output limit or an I/O error.
    pub fn write_json(&self, output: &mut impl Write) -> Result<()> {
        serde_json::to_writer(
            &mut CountBytes {
                bytes: 0,
                cap: self.bounds.max_output_bytes,
            },
            self,
        )?;
        serde_json::to_writer(output, self)?;
        Ok(())
    }
}

/// Read the routed HEAD's explicit logical-net identities under a shared lock.
///
/// Source hashes use exactly the selected-antenna source fingerprint. Synthetic
/// rail-tie IDs are validated against their known domain and counted separately.
///
/// # Errors
/// Fails on stale LEF sources, unknown/duplicate route IDs or names, malformed
/// snapshots, and bounded allocation/work/byte admission.
pub fn antenna_inventory(
    workspace: &Path,
    bounds: &InventoryBounds,
    limits: &Limits,
) -> Result<InventoryReport> {
    bounds.validate()?;
    let lock = crate::lock(workspace, false, false)?;
    let snapshot = crate::load_capped(
        workspace,
        lock,
        limits,
        Some(SnapshotCaps {
            route_bytes: bounds.max_route_bytes,
            route_records: (bounds.max_route_bytes / 64) as usize,
            routed_ids: bounds.max_routed_ids,
            memory_bytes: 2 << 30,
            inventory_bytes: bounds.memory_bytes as u64,
        }),
    )?;
    let placed: Vec<_> = snapshot.records().collect();
    let mut report = from_prepared(snapshot.metadata(), &placed, bounds, None)?;
    // Keep the original shared lock alive through serialization/caller flush.
    report._lock = Some(snapshot._lock);
    Ok(report)
}

pub(super) fn from_prepared(
    metadata: &crate::Metadata,
    placed: &[rustroad_core::Placed],
    bounds: &InventoryBounds,
    source: Option<&str>,
) -> Result<InventoryReport> {
    bounds.validate()?;
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("NO_PHYSICAL", "import a PDK design first"))?;
    let state = bundle
        .routing
        .as_ref()
        .ok_or_else(|| fail("NO_ROUTING", "route before inventory"))?;
    let model = &bundle.connectivity;
    if model.nets.len() > bounds.max_logical_nets || state.routes.nets.len() > bounds.max_routed_ids
    {
        return Err(Error::Budget("ANTENNA_INVENTORY: net count".into()));
    }
    let rows = &state.routes;
    let mut work = [
        metadata.design.cells.len(),
        metadata.design.rows.len(),
        model.nets.len(),
        model.pins.len(),
        model.masters.len(),
        model.ports.len(),
        rows.wires.len(),
        rows.vias.len(),
        rows.access.len(),
        rows.patches.len(),
        rows.nets.len(),
    ]
    .into_iter()
    .try_fold(0usize, usize::checked_add)
    .ok_or_else(|| Error::Budget("ANTENNA_INVENTORY: source work overflow".into()))?;
    let charge = |work: &mut usize, n: usize| -> Result<()> {
        *work = work
            .checked_add(n)
            .filter(|&n| n <= bounds.max_work)
            .ok_or_else(|| Error::Budget("ANTENNA_INVENTORY: work limit".into()))?;
        Ok(())
    };
    charge(&mut work, 0)?;
    let entry_bytes = rows.nets.len().saturating_mul(128).saturating_add(16384);
    if entry_bytes > bounds.memory_bytes {
        return Err(Error::Budget(
            "ANTENNA_INVENTORY: ID/name memory admission".into(),
        ));
    }
    let export = crate::routing::ExportNets::new(bundle);
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut nets = Vec::new();
    nets.try_reserve_exact(rows.nets.len().min(model.nets.len()))
        .map_err(|_| Error::Budget("ANTENNA_INVENTORY: entry allocation".into()))?;
    let mut retained_name_bytes = 0usize;
    let mut excluded = 0usize;
    for &id in &rows.nets {
        charge(&mut work, 1 + ids.len().saturating_add(1).ilog2() as usize)?;
        if !ids.insert(id) {
            return Err(fail("ANTENNA_INVENTORY", "duplicate routed net ID"));
        }
        export.of(id)?;
        if id as usize >= model.nets.len() {
            excluded += 1;
            continue;
        }
        let name = &model.nets[id as usize].name;
        retained_name_bytes = retained_name_bytes
            .checked_add(name.len())
            .filter(|&n| n <= bounds.max_name_bytes)
            .ok_or_else(|| Error::Budget("ANTENNA_INVENTORY: name bytes".into()))?;
        if name.is_empty() || name.len() > 4096 || !names.insert(name.as_str()) {
            return Err(fail(
                "ANTENNA_INVENTORY",
                "invalid or duplicate canonical name",
            ));
        }
        if entry_bytes.saturating_add(retained_name_bytes) > bounds.memory_bytes {
            return Err(Error::Budget("ANTENNA_INVENTORY: retained memory".into()));
        }
        nets.push(InventoryEntry {
            net_id: id,
            canonical_name: name.clone(),
        });
    }
    charge(
        &mut work,
        nets.len()
            .saturating_mul(nets.len().max(1).ilog2() as usize + 1),
    )?;
    nets.sort_unstable_by(|a, b| {
        a.canonical_name
            .cmp(&b.canonical_name)
            .then(a.net_id.cmp(&b.net_id))
    });
    let mut sources = Vec::new();
    for role in ["technology_lef", "cell_lef"] {
        sources.push(verified_lef(bundle, role)?.1);
    }
    let source_sha256 = match source {
        Some(value) => value.to_owned(),
        None => source_fingerprint(&metadata.design, model, placed, state, &sources)?,
    };
    let routing_scope = match state.scope {
        crate::routing::RouteScope::All => "all_signal_nets",
        crate::routing::RouteScope::Clock => "clock_nets_only",
    }
    .to_owned();
    let inventory_sha256 =
        report::fingerprint(&(&metadata.revision, &routing_scope, &source_sha256, &nets))?;
    let report = InventoryReport {
        schema: "rustroad.antenna-inventory.v1",
        workspace_revision: metadata.revision.clone(),
        design: metadata.design.name.clone(),
        routing_scope,
        source_sha256,
        inventory_sha256,
        model_logical_net_count: model.nets.len(),
        routed_logical_net_count: nets.len(),
        router_declared_net_count: rows.nets.len(),
        excluded_nonlogical_supply_tie_count: excluded,
        nets,
        sources,
        bounds: bounds.clone(),
        work,
        retained_name_bytes,
        estimated_inventory_bytes: entry_bytes.saturating_add(retained_name_bytes),
        status: "inventory_only",
        antenna_status: "unchecked",
        drc_status: "unchecked",
        lvs_status: "unchecked",
        timing_status: "unchecked",
        _lock: None,
    };
    serde_json::to_writer(
        &mut CountBytes {
            bytes: 0,
            cap: bounds.max_output_bytes,
        },
        &report,
    )?;
    Ok(report)
}
