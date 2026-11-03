//! Commit-time validation receipts (`revisions/<rev>/validated.json`).
//!
//! `commit_in_place` runs every check a full read would run (routing geometry,
//! connectivity and rules, the bundle link, connection-table lowering, the
//! placement and the prepared physical layout) and then records, next to the
//! revision files it has just written, a receipt binding
//!
//! * the validator identity: [`VALIDATION_EPOCH`] and the build identity
//!   (package version plus a digest of the validator sources, see `build.rs`),
//! * the exact bytes of every revision file (length and XXH3-128), and
//! * a digest of the technology, physical and routing configuration.
//!
//! A read whose receipt matches all of these can skip the semantic
//! re-derivations: the bytes are the ones that passed full validation under
//! the same validator. A missing, unreadable, foreign-build or mismatching
//! receipt falls back to full validation, so hand-edited or older workspaces
//! keep the previous behaviour. `RUSTROAD_FORCE_VALIDATE=1` (or the CLI's
//! `--verify-workspace`) always validates fully.
//!
//! A receipt is an integrity cache, not an authenticity proof: whoever can
//! write a revision directory can also write a receipt (exactly as they can
//! replace the revision wholesale). `rustroad verify-workspace` re-validates.
use crate::{Error, Metadata, Result};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Receipt file name inside a revision directory.
pub const RECEIPT_FILE: &str = "validated.json";
const SCHEMA: &str = "rustroad.validation-receipt.v1";
/// Bump whenever the semantics of a read-time validator change in a way the
/// build identity would not capture (it already changes with every validator
/// source edit): `routing::validate` (geometry, connectivity, route rules),
/// `pdk::validate_bundle`/link, connection-table lowering,
/// `physical::validate_state`, `rustroad_core::validate_placement` or the
/// revision storage format.
pub const VALIDATION_EPOCH: u32 = 1;
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
const MAX_REVISION_FILES: usize = 32;
/// What commit-time validation covered (documentation inside the receipt).
const COVERAGE: [&str; 6] = [
    "design",
    "physical_bundle_link",
    "connection_tables",
    "placement",
    "prepared_layout",
    "routing_geometry_connectivity_rules",
];

static FORCE: AtomicBool = AtomicBool::new(false);

/// Force full validation for every load of this process (`--verify-workspace`).
pub fn set_force_validate(force: bool) {
    FORCE.store(force, Ordering::Relaxed);
}

/// Full validation requested by flag or by `RUSTROAD_FORCE_VALIDATE=1`.
pub fn force_validate() -> bool {
    FORCE.load(Ordering::Relaxed)
        || std::env::var_os("RUSTROAD_FORCE_VALIDATE").is_some_and(|v| v != "0" && !v.is_empty())
}

/// Validator identity of this build.
pub fn build_identity() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        env!("RUSTROAD_VALIDATOR_DIGEST")
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDigest {
    pub bytes: u64,
    pub xxh3_128: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema: String,
    pub epoch: u32,
    pub build: String,
    pub revision: String,
    pub coverage: Vec<String>,
    pub config_xxh3_128: String,
    pub files: BTreeMap<String, FileDigest>,
}

/// How a snapshot's semantic validity was established.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "reason")]
pub enum Validation {
    /// A matching commit-time receipt; semantic re-derivations were skipped.
    Receipt,
    /// Full validation; the reason the receipt was not used.
    Full(&'static str),
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// Digest of the technology, physical configuration and routing setup.
pub(crate) fn config_digest(metadata: &Metadata) -> Result<String> {
    #[derive(Serialize)]
    struct View<'a> {
        format_version: u32,
        stage: &'a crate::Stage,
        technology: Option<&'a crate::pdk::Technology>,
        config: Option<&'a crate::physical::PhysicalConfig>,
        routing: Option<RoutingView<'a>>,
    }
    #[derive(Serialize)]
    struct RoutingView<'a> {
        config: &'a crate::routing::RoutingConfig,
        scope: crate::routing::RouteScope,
        layers: &'a [String],
        via_names: &'a [String],
        via_lower_layers: &'a [u32],
        table: Option<&'a crate::routing::RoutesTable>,
    }
    let physical = metadata.physical.as_ref();
    let view = View {
        format_version: metadata.format_version,
        stage: &metadata.stage,
        technology: physical.map(|p| &p.technology),
        config: physical.and_then(|p| p.config.as_ref()),
        routing: physical
            .and_then(|p| p.routing.as_ref())
            .map(|r| RoutingView {
                config: &r.config,
                scope: r.scope,
                layers: &r.layers,
                via_names: &r.via_names,
                via_lower_layers: &r.via_lower_layers,
                table: r.table.as_ref(),
            }),
    };
    Ok(digest(&serde_json::to_vec(&view)?))
}

/// Names of the revision files a receipt must cover (every regular file but
/// the receipt itself), sorted.
pub(crate) fn revision_files(directory: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Invalid("non-UTF8 revision file name".into()))?;
        if name == RECEIPT_FILE || name.starts_with(".validated.") {
            continue;
        }
        if names.len() >= MAX_REVISION_FILES {
            return Err(Error::Budget("revision file count".into()));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// Read-only mapping of one revision file (`None` for an empty file).
pub(crate) fn map_file(path: &Path) -> Result<Option<Mmap>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(Error::Invalid(
            "revision file must be a regular file".into(),
        ));
    }
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(None);
    }
    // SAFETY: immutable published revision files read under the workspace lock.
    #[allow(unsafe_code)]
    let map = unsafe { Mmap::map(&file)? };
    Ok(Some(map))
}

/// Build the receipt of a fully validated revision from its files on disk.
pub(crate) fn build(directory: &Path, metadata: &Metadata) -> Result<Receipt> {
    use rayon::prelude::*;
    let names = revision_files(directory)?;
    let files = names
        .par_iter()
        .map(|name| {
            let map = map_file(&directory.join(name))?;
            let bytes = map.as_deref().unwrap_or(&[]);
            Ok((
                name.clone(),
                FileDigest {
                    bytes: bytes.len() as u64,
                    xxh3_128: digest(bytes),
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(Receipt {
        schema: SCHEMA.into(),
        epoch: VALIDATION_EPOCH,
        build: build_identity(),
        revision: metadata.revision.clone(),
        coverage: COVERAGE.iter().map(|s| (*s).to_owned()).collect(),
        config_xxh3_128: config_digest(metadata)?,
        files,
    })
}

/// Publish a receipt for a revision that is not visible yet (commit).
pub(crate) fn write_new(directory: &Path, receipt: &Receipt) -> Result<()> {
    crate::write_new(
        &directory.join(RECEIPT_FILE),
        &serde_json::to_vec_pretty(receipt)?,
    )
}

/// Atomically (re)place the receipt of a published revision (stamping).
pub(crate) fn replace(directory: &Path, receipt: &Receipt) -> Result<()> {
    let temporary = directory.join(format!(".validated.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = file
        .write_all(&serde_json::to_vec_pretty(receipt)?)
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.map_err(Error::from).and_then(|()| {
        fs::rename(&temporary, directory.join(RECEIPT_FILE))?;
        crate::sync_directory(directory)
    }) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

/// The receipt of `revision` if it is well-formed and from this validator;
/// otherwise the reason it cannot be used.
pub(crate) fn read(directory: &Path, revision: &str) -> std::result::Result<Receipt, &'static str> {
    let path = directory.join(RECEIPT_FILE);
    let bytes = match crate::read_bounded(&path, MAX_RECEIPT_BYTES) {
        Ok(bytes) => bytes,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("receipt_absent");
        }
        Err(_) => return Err("receipt_unreadable"),
    };
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| "receipt_unreadable")?;
    if receipt.schema != SCHEMA || receipt.files.len() > MAX_REVISION_FILES {
        return Err("receipt_unreadable");
    }
    if receipt.epoch != VALIDATION_EPOCH || receipt.build != build_identity() {
        return Err("receipt_other_validator");
    }
    if receipt.revision != revision {
        return Err("receipt_revision_mismatch");
    }
    Ok(receipt)
}

/// Pre-mapped revision tables of a receipt-checked load; each one is hashed
/// once and then used by the loader, so the verified bytes are the read bytes.
pub(crate) struct Premapped {
    maps: Vec<(String, Option<Mmap>)>,
}
impl Premapped {
    pub(crate) fn empty() -> Self {
        Self { maps: Vec::new() }
    }
    /// Map every receipt file but `design.json` (already read into memory).
    /// `None` when the directory listing or a file length disagrees.
    pub(crate) fn open(directory: &Path, receipt: &Receipt) -> Option<Self> {
        let names = revision_files(directory).ok()?;
        if !names.iter().eq(receipt.files.keys()) {
            return None;
        }
        let mut maps = Vec::with_capacity(names.len());
        for (name, expected) in &receipt.files {
            if name == "design.json" {
                continue;
            }
            let map = map_file(&directory.join(name)).ok()?;
            if map.as_ref().map_or(0, |m| m.len() as u64) != expected.bytes {
                return None;
            }
            maps.push((name.clone(), map));
        }
        Some(Self { maps })
    }
    /// Whether `design` and every mapped table hash to the receipt.
    pub(crate) fn matches(&self, receipt: &Receipt, design: &[u8]) -> bool {
        use rayon::prelude::*;
        let Some(expected) = receipt.files.get("design.json") else {
            return false;
        };
        let check = |bytes: &[u8], want: &FileDigest| {
            bytes.len() as u64 == want.bytes && digest(bytes) == want.xxh3_128
        };
        let (design_ok, tables_ok) = rayon::join(
            || check(design, expected),
            || {
                self.maps.par_iter().all(|(name, map)| {
                    receipt
                        .files
                        .get(name)
                        .is_some_and(|want| check(map.as_deref().unwrap_or(&[]), want))
                })
            },
        );
        design_ok && tables_ok && self.maps.len() + 1 == receipt.files.len()
    }
    /// Take a pre-mapped table whose length is `bytes`; `None` (the caller
    /// maps the file itself and reports any mismatch) otherwise.
    pub(crate) fn take(&mut self, name: &str, bytes: usize) -> Option<Option<Mmap>> {
        let index = self.maps.iter().position(|(n, _)| n == name)?;
        let (_, map) = self.maps.swap_remove(index);
        (map.as_ref().map_or(0, |m| m.len()) == bytes).then_some(map)
    }
}
