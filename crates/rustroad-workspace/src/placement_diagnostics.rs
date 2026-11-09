//! Read-only stage-current placement observations in a fresh diagnostic workspace.
//!
//! This command preserves ordinary placement selection and publishes no design IR.
//! Source/target geometry is retained for a separately qualified integer observer;
//! no native centre metric is relabelled as physical terminal HPWL or signoff.
mod front;
mod output;
#[cfg(test)]
mod tests;

use crate::{Error, Limits, Result};
use rustroad_core::place::{DiagnosticContext, DiagnosticPhaseView};
use serde::Serialize;
use std::path::{Path, PathBuf};

const IO_CAP: u64 = 2 << 30;
const OUTPUT_CAP: u64 = 64 << 20;
// Source rehash(64MiB), maximum output verification(128MiB), fixed result tail.
const FINAL_HOLD: u64 = (192 << 20) + (1 << 20);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileId {
    /// Actual absolute file path; this is not a shell handle.
    pub path: PathBuf,
    /// Length of the bound regular file.
    pub bytes: u64,
    /// SHA-256 of those exact bytes.
    pub sha256: String,
}

#[derive(Debug)]
struct ByteLedger {
    charged: u64,
    held: u64,
    limit: u64,
}
impl ByteLedger {
    fn new() -> Self {
        Self {
            charged: 0,
            held: FINAL_HOLD,
            limit: IO_CAP,
        }
    }
    fn charge(&mut self, bytes: u64) -> Result<()> {
        self.charged = self
            .charged
            .checked_add(bytes)
            .filter(|n| {
                n.checked_add(self.held)
                    .is_some_and(|sum| sum <= self.limit)
            })
            .ok_or_else(|| Error::Budget("diagnostic explicit byte-pass budget".into()))?;
        Ok(())
    }
    fn final_phase(&mut self) -> u64 {
        std::mem::take(&mut self.held)
    }
    fn admit_remaining(&self, bytes: u64) -> Result<()> {
        self.charged
            .checked_add(self.held)
            .and_then(|n| n.checked_add(bytes))
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| Error::Budget("diagnostic fixed publication/CLI tail".into()))?;
        Ok(())
    }
}

/// Actual completion is still unverified for physical placement/signoff purposes.
#[derive(Debug, Serialize)]
pub struct DiagnosticReceipt {
    pub schema: &'static str,
    pub status: &'static str,
    pub revision: &'static str,
    pub source_revision: String,
    pub observation: FileId,
    pub summary: FileId,
    pub actual_pid: u32,
    pub source_unchanged: bool,
    pub final_defined_work: u64,
    pub explicit_byte_passes_before_cli_stdout: u64,
    pub byte_pass_scope: &'static str,
    pub physical_pin_hpwl_status: &'static str,
    pub integer_displacement_status: &'static str,
    pub drc_status: &'static str,
    pub lvs_status: &'static str,
    pub timing_status: &'static str,
    pub corner: &'static str,
    pub wire_model: &'static str,
    pub unconstrained_endpoints: Option<u64>,
}

/// Run the explicitly requested stage-current coordinate diagnostic.
///
/// The input must be a sealed inline imported schema3/4 snapshot. The output is
/// disjoint and fresh, and `out_base` must be `empty`. Source SH and output EX
/// locks remain held through final source verification and output publication.
///
/// # Errors
/// Rejects aliases, unsupported schema/arithmetic, incomplete phases and resource
/// limits. Precommit failures retain partial private output without HEAD. A
/// postcommit IO/termination failure can leave a provisional HEAD: only actual
/// exit zero, typed completion and current identities establish completion.
/// The result is never promoted
/// to a successful ordinary baseline or a physical acceptance result.
pub fn diagnose(
    source: &Path,
    base: &str,
    out: &Path,
    out_base: &str,
    limits: &Limits,
) -> Result<DiagnosticReceipt> {
    if !rayon_threads().is_some_and(|n| n <= 4) {
        return Err(Error::Budget(
            "diagnostic supports at most four numerical pool threads".into(),
        ));
    }
    output::check_fresh(out, out_base)?;
    front::disjoint(source, out)?;
    let mut io = ByteLedger::new();
    let opened = front::open(source, base, limits, &mut io)?;
    let front::Opened {
        metadata,
        lock: source_lock,
        proof,
    } = opened;
    let output = output::OwnedOutput::create(out, out_base)?;
    let attempt = (|| -> Result<(FileId, FileId, u64)> {
        let bundle = metadata
            .physical
            .as_ref()
            .ok_or_else(|| Error::Invalid("diagnostic requires physical snapshot".into()))?;
        let initial_admission = crate::physical::diagnostic_admit(&metadata.design, bundle)?;
        let initial = rustroad_core::place_design_cpu_ref(&metadata.design, limits)?;
        let mut preparation = crate::physical::DiagnosticPreparation::default();
        let input = crate::physical::diagnostic_problem(
            &metadata.design,
            bundle,
            &initial.cells,
            limits,
            &mut preparation,
        )?;
        let caller_work = preparation
            .work
            .checked_add(proof.lexical.nodes)
            .and_then(|n| n.checked_add(initial_admission))
            .and_then(|n| n.checked_add(initial.expansions as u64))
            .ok_or_else(|| Error::Budget("diagnostic preparation total".into()))?;
        let mut stream = output::ObservationWriter::create(&output, &mut io)?;
        stream.header(
            &metadata,
            &proof,
            &input,
            &initial.cells,
            preparation
                .io_pins
                .as_deref()
                .ok_or_else(|| Error::Invalid("missing effective diagnostic I/O".into()))?,
        )?;
        let mut observer_error = None;
        let result = {
            let mut observer = |view: DiagnosticPhaseView<'_>| -> rustroad_core::Result<()> {
                match stream.phase(&view) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        observer_error = Some(error);
                        Err(rustroad_core::Error::Budget(
                            "diagnostic output observer failed".into(),
                        ))
                    }
                }
            };
            rustroad_core::place::diagnose_cpu_ref(
                &input.problem,
                input.config,
                &input.limits,
                DiagnosticContext {
                    core: metadata.design.core_area,
                    preparation_work: caller_work,
                },
                &mut observer,
            )
        };
        if let Some(error) = observer_error {
            return Err(error);
        }
        let result = result?;
        let path = stream.finish(&result)?;
        // No data-dependent operation can consume the final-source/result hold.
        let released = io.final_phase();
        let observation = front::identity(&path, OUTPUT_CAP, true, &mut io)?;
        front::recheck(&proof, &mut io)?;
        let summary = output.summary(&proof, &result, &observation, released, &mut io)?;
        Ok((observation, summary, result.summary.final_work))
    })();
    let result = match attempt {
        Ok((observation, summary, work)) => {
            output.publish(&mut io)?;
            Ok(DiagnosticReceipt {
                schema: "rustroad.p1-stage-diagnostic-receipt.v1",
                status: "observed_unverified_complete",
                revision: "rev1",
                source_revision: proof.revision.clone(),
                observation,
                summary,
                actual_pid: std::process::id(),
                source_unchanged: true,
                final_defined_work: work,
                explicit_byte_passes_before_cli_stdout: io.charged,
                byte_pass_scope: "new diagnostic named read/hash/scan/encode/write passes; old loader mapped extents admitted once; not OS syscall tracing; failed counters are charged/attempted scope",
                physical_pin_hpwl_status: "unchecked_external_integer_observer_required",
                integer_displacement_status: "unchecked_external_integer_observer_required",
                drc_status: "unchecked",
                lvs_status: "unchecked",
                timing_status: "unchecked",
                corner: "not_applicable",
                wire_model: "native_cell_centres_and_fixed_points",
                unconstrained_endpoints: None,
            })
        }
        Err(error) => {
            io.final_phase();
            let recheck = front::recheck(&proof, &mut io);
            // This bounded receipt cannot turn a failed phase into a HEAD.
            output.failure(&error, recheck.as_ref().err(), &mut io)?;
            Err(error)
        }
    };
    drop(source_lock);
    result
}

fn rayon_threads() -> Option<usize> {
    // The core report is the authoritative actual numerical worker count.
    // This crate needs no Rayon dependency just to query that bound.
    rustroad_core::place::diagnostic_pool_threads()
}
