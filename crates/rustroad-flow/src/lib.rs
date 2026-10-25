//! Bounded, read-only assessment of the inputs to a black-box RTL-to-GDS flow.
//!
//! Availability is never a correctness verdict. Engine adapters must attach
//! independent evidence before a stage can be considered verified.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

fn open_regular(path: &Path) -> Result<File, FlowError> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(FlowError::Manifest("input is not a regular file".into()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A path can change between metadata and open. Nonblocking open keeps a
    // raced FIFO from hanging the caller; the opened descriptor is checked too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(FlowError::Manifest(
            "opened input is not a regular file".into(),
        ));
    }
    Ok(file)
}

const MAX_MANIFEST_BYTES: u64 = 1_048_576;
const MAX_FILES: usize = 64;
const MAX_ARTIFACT_BYTES: u64 = 268_435_456;

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("FLOW_IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("FLOW_JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("FLOW_BUDGET: {0}")]
    Budget(String),
    #[error("FLOW_MANIFEST: {0}")]
    Manifest(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub id: String,
    pub executable: FilePin,
    pub version: String,
    /// Marks a pinned host tool that must never produce a signoff verdict
    /// (for example a host Magic/KLayout/Netgen build that differs from the
    /// pinned signoff image). The text says why and where the signoff
    /// identity lives. Such a tool never makes a stage `ready_to_attempt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_for_signoff: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdkSpec {
    pub name: String,
    pub corner: String,
    pub liberty: Option<FilePin>,
    pub technology_lef: Option<FilePin>,
    pub cell_lef: Option<FilePin>,
    pub cell_gds: Option<FilePin>,
    pub drc_deck: Option<FilePin>,
    pub lvs_setup: Option<FilePin>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowManifest {
    pub schema: String,
    pub tools: Vec<ToolSpec>,
    pub pdks: Vec<PdkSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityEvidence {
    pub role: String,
    pub path: Option<PathBuf>,
    pub expected_sha256: Option<String>,
    pub actual_sha256: Option<String>,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnalysisScope {
    pub corner: Option<String>,
    pub wire_model: Option<String>,
    pub unconstrained_endpoints: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StageReadiness {
    pub stage: String,
    pub pdk: String,
    pub status: String,
    pub missing: Vec<String>,
    pub verification_status: String,
    pub analysis_scope: AnalysisScope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadinessReport {
    pub schema: String,
    pub status: String,
    pub scope: String,
    pub analysis_scope: AnalysisScope,
    pub identities: Vec<IdentityEvidence>,
    pub stages: Vec<StageReadiness>,
    pub full_rtl_to_gds_status: String,
    pub limitations: Vec<String>,
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn identity(role: String, pin: Option<&FilePin>) -> IdentityEvidence {
    let mut result = IdentityEvidence {
        role,
        path: pin.map(|p| p.path.clone()),
        expected_sha256: pin.map(|p| p.sha256.clone()),
        actual_sha256: None,
        status: "unverified".into(),
        reason: None,
    };
    let Some(pin) = pin else {
        result.reason = Some("no pinned input configured".into());
        return result;
    };
    let read = || -> Result<String, FlowError> {
        let mut file = open_regular(&pin.path)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() > MAX_ARTIFACT_BYTES {
            return Err(FlowError::Budget(
                "input must be a regular file at most 256 MiB".into(),
            ));
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65_536];
        let mut total = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .ok_or_else(|| FlowError::Budget("input length overflow".into()))?;
            if total > MAX_ARTIFACT_BYTES {
                return Err(FlowError::Budget("input grew beyond 256 MiB".into()));
            }
            hasher.update(&buffer[..n]);
        }
        let after = file.metadata()?;
        if total != before.len()
            || after.len() != before.len()
            || before.modified()? != after.modified()?
        {
            return Err(FlowError::Manifest("input changed while hashing".into()));
        }
        Ok(format!("{:x}", hasher.finalize()))
    };
    match read() {
        Ok(actual) => {
            result.status = if actual == pin.sha256 {
                "available"
            } else {
                "unverified"
            }
            .into();
            if actual != pin.sha256 {
                result.reason = Some("SHA-256 does not match pin".into());
            }
            result.actual_sha256 = Some(actual);
        }
        Err(e) => result.reason = Some(e.to_string()),
    }
    result
}

/// Does not launch tools, mutate the workspace, or treat version strings as verified.
/// Caller holds its workspace's shared lock while publishing/using this assessment.
pub fn assess_path(path: &Path) -> Result<ReadinessReport, FlowError> {
    let file = open_regular(path)?;
    if file.metadata()?.len() > MAX_MANIFEST_BYTES {
        return Err(FlowError::Budget("manifest exceeds 1 MiB".into()));
    }
    let mut contents = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(FlowError::Budget("manifest grew beyond 1 MiB".into()));
    }
    let manifest: FlowManifest = serde_json::from_slice(&contents)?;
    assess(&manifest)
}

pub fn assess(manifest: &FlowManifest) -> Result<ReadinessReport, FlowError> {
    if manifest.schema != "rustroad.flow.v1" {
        return Err(FlowError::Manifest("expected rustroad.flow.v1".into()));
    }
    if manifest.tools.len() > 16
        || manifest.pdks.len() > 8
        || manifest.tools.len() + manifest.pdks.len() * 6 > MAX_FILES
    {
        return Err(FlowError::Budget(
            "at most 16 tools, 8 PDKs and 64 file identities".into(),
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut identities = Vec::new();
    let mut not_for_signoff = std::collections::BTreeSet::new();
    for tool in &manifest.tools {
        if !valid_id(&tool.id) || !ids.insert(tool.id.clone()) {
            return Err(FlowError::Manifest("invalid or duplicate tool id".into()));
        }
        if tool.version.is_empty() || tool.version.len() > 1024 {
            return Err(FlowError::Manifest(
                "tool version must be 1..1024 bytes".into(),
            ));
        }
        validate_pin(&tool.executable)?;
        if tool
            .not_for_signoff
            .as_ref()
            .is_some_and(|why| why.is_empty() || why.len() > 1024)
        {
            return Err(FlowError::Manifest(
                "not_for_signoff must be 1..1024 bytes".into(),
            ));
        }
        let mut evidence = identity(format!("tool_{}", tool.id), Some(&tool.executable));
        #[cfg(unix)]
        if evidence.status == "available" {
            use std::os::unix::fs::PermissionsExt;
            let executable = std::fs::metadata(&tool.executable.path)
                .is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
            if !executable {
                evidence.status = "unverified".into();
                evidence.reason = Some("tool file has no executable permission".into());
            }
        }
        if let Some(why) = &tool.not_for_signoff {
            if evidence.status == "available" {
                evidence.status = "not_for_signoff".into();
                evidence.reason = Some(why.clone());
            }
            not_for_signoff.insert(format!("tool_{}", tool.id));
        }
        identities.push(evidence);
    }
    let mut pdk_names = std::collections::BTreeSet::new();
    for pdk in &manifest.pdks {
        if !valid_id(&pdk.name) || !valid_id(&pdk.corner) || !pdk_names.insert(pdk.name.clone()) {
            return Err(FlowError::Manifest(
                "invalid or duplicate PDK name/corner".into(),
            ));
        }
        for (kind, pin) in [
            ("liberty", &pdk.liberty),
            ("technology_lef", &pdk.technology_lef),
            ("cell_lef", &pdk.cell_lef),
            ("cell_gds", &pdk.cell_gds),
            ("drc_deck", &pdk.drc_deck),
            ("lvs_setup", &pdk.lvs_setup),
        ] {
            if let Some(pin) = pin {
                validate_pin(pin)?;
            }
            identities.push(identity(format!("{}_{}", pdk.name, kind), pin.as_ref()));
        }
    }
    let available = |role: &str| {
        identities
            .iter()
            .any(|i| i.role == role && i.status == "available")
    };
    let mut stages = Vec::new();
    // Always retain both baseline rows even when neither library is configured.
    let mut baseline_pdks = vec![
        "sky130_fd_sc_hd".to_string(),
        "gf180mcu_fd_sc_mcu7t5v0".to_string(),
    ];
    if pdk_names.contains("gf180mcu_fd_sc_mcu9t5v0") {
        baseline_pdks[1] = "gf180mcu_fd_sc_mcu9t5v0".into();
    }
    for pdk in baseline_pdks {
        for (stage, mut deps) in [
            (
                "synthesis",
                vec!["tool_rustsys".to_string(), format!("{pdk}_liberty")],
            ),
            (
                "cec",
                vec![
                    "tool_rustsys".into(),
                    "tool_yosys".into(),
                    format!("{pdk}_liberty"),
                ],
            ),
            (
                "simulation",
                vec!["tool_iverilog".into(), "tool_vvp".into()],
            ),
            (
                "prelayout_sta",
                vec![
                    "tool_ruststa".into(),
                    "tool_rusteda".into(),
                    "tool_opensta".into(),
                    format!("{pdk}_liberty"),
                ],
            ),
            (
                "placement_routing",
                vec![
                    "tool_rustroad".into(),
                    format!("{pdk}_technology_lef"),
                    format!("{pdk}_cell_lef"),
                ],
            ),
            (
                "gds_streamout",
                vec!["tool_klayout".into(), format!("{pdk}_cell_gds")],
            ),
            ("drc", vec!["tool_magic".into(), format!("{pdk}_drc_deck")]),
            (
                "lvs",
                vec!["tool_netgen".into(), format!("{pdk}_lvs_setup")],
            ),
        ] {
            if stage == "simulation" && available("tool_verilator") && available("tool_yosys") {
                deps.clear();
            }
            deps.retain(|dep| !available(dep));
            for dep in &mut deps {
                if not_for_signoff.contains(dep.as_str()) {
                    dep.push_str("_not_for_signoff");
                }
            }
            if stage == "placement_routing" {
                deps.push("rustroad_routing_engine_not_implemented".into());
            }
            stages.push(StageReadiness {
                stage: stage.into(),
                pdk: pdk.clone(),
                status: if deps.is_empty() {
                    "ready_to_attempt"
                } else {
                    "unverified"
                }
                .into(),
                missing: deps,
                verification_status: "not_run".into(),
                analysis_scope: AnalysisScope {
                    corner: manifest
                        .pdks
                        .iter()
                        .find(|p| p.name == pdk)
                        .map(|p| p.corner.clone()),
                    wire_model: None,
                    unconstrained_endpoints: None,
                },
            });
        }
        stages.push(StageReadiness {
            stage: "extracted_sta".into(),
            pdk,
            status: "unverified".into(),
            missing: vec![
                "extracted_parasitics_adapter_not_implemented_for_frozen_probed_ruststa_binary"
                    .into(),
            ],
            verification_status: "not_run".into(),
            analysis_scope: AnalysisScope {
                corner: None,
                wire_model: None,
                unconstrained_endpoints: None,
            },
        });
    }
    Ok(ReadinessReport { schema: "rustroad.readiness.v1".into(), status: "assessed".into(),
        scope: "Read-only file identity and dependency assessment; tools and PDK decks have not been validated by this function.".into(),
        analysis_scope: AnalysisScope { corner: None, wire_model: None, unconstrained_endpoints: None },
        identities, stages, full_rtl_to_gds_status: "unverified".into(), limitations: vec![
            "A matching executable hash is only an entrypoint pin; shared libraries, wrapper descendants and runtime dependencies require separate provenance.".into(),
            "Tool version strings are declared metadata until a guarded probe captures them; availability is not successful execution.".into(),
            "Every flow stage needs independent semantic evidence; a successful process exit is insufficient.".into(),
            "The frozen RustSTA binary probed for this integration has no SPEF import adapter; this is not a claim about every future source revision. Prelayout timing is not post-route signoff.".into(),
        ] })
}

fn validate_pin(pin: &FilePin) -> Result<(), FlowError> {
    if !pin.path.is_absolute() || pin.path.as_os_str().len() > 4096 {
        return Err(FlowError::Manifest(
            "pinned paths must be absolute and at most 4096 bytes".into(),
        ));
    }
    if pin.sha256.len() != 64
        || !pin
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(FlowError::Manifest(
            "SHA-256 pins must be 64 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "rustroad-flow-{}-{tag}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&p).unwrap();
        p
    }
    fn empty() -> FlowManifest {
        FlowManifest {
            schema: "rustroad.flow.v1".into(),
            tools: vec![],
            pdks: vec![],
        }
    }
    #[test]
    fn unavailable_inputs_never_claim_validation() {
        let r = assess(&empty()).unwrap();
        assert_eq!(r.full_rtl_to_gds_status, "unverified");
        assert_eq!(r.stages.len(), 18);
        assert!(
            r.stages
                .iter()
                .all(|s| s.status == "unverified" && s.verification_status == "not_run")
        );
        assert!(
            r.stages
                .iter()
                .all(|s| s.analysis_scope.unconstrained_endpoints.is_none())
        );
    }
    #[test]
    fn file_identity_and_tamper_control() {
        let dir = scratch("identity");
        let path = dir.join("tool");
        fs::write(&path, b"abc").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut manifest = empty();
        manifest.tools.push(ToolSpec {
            id: "yosys".into(),
            executable: FilePin {
                path: path.clone(),
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            },
            version: "declared".into(),
            not_for_signoff: None,
        });
        assert_eq!(assess(&manifest).unwrap().identities[0].status, "available");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                assess(&manifest).unwrap().identities[0].status,
                "unverified"
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(&path, b"bad").unwrap();
        let evidence = &assess(&manifest).unwrap().identities[0];
        assert_eq!(evidence.status, "unverified");
        assert!(evidence.reason.as_ref().unwrap().contains("does not match"));
        fs::remove_file(&path).unwrap();
        assert_eq!(
            assess(&manifest).unwrap().identities[0].status,
            "unverified"
        );
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn not_for_signoff_tool_never_readies_a_signoff_stage() {
        let dir = scratch("nfs");
        let path = dir.join("magic");
        fs::write(&path, b"abc").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let deck = dir.join("deck");
        fs::write(&deck, b"abc").unwrap();
        let pin = |p: &Path| FilePin {
            path: p.to_path_buf(),
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        let mut manifest = empty();
        manifest.tools.push(ToolSpec {
            id: "magic".into(),
            executable: pin(&path),
            version: "declared".into(),
            not_for_signoff: None,
        });
        manifest.pdks.push(PdkSpec {
            name: "sky130_fd_sc_hd".into(),
            corner: "tt".into(),
            liberty: None,
            technology_lef: None,
            cell_lef: None,
            cell_gds: None,
            drc_deck: Some(pin(&deck)),
            lvs_setup: None,
        });
        let drc = |r: &ReadinessReport| {
            r.stages
                .iter()
                .find(|s| s.stage == "drc" && s.pdk == "sky130_fd_sc_hd")
                .unwrap()
                .clone()
        };
        assert_eq!(drc(&assess(&manifest).unwrap()).status, "ready_to_attempt");
        manifest.tools[0].not_for_signoff =
            Some("host build; signoff uses the pinned image".into());
        let report = assess(&manifest).unwrap();
        assert_eq!(report.identities[0].status, "not_for_signoff");
        let stage = drc(&report);
        assert_eq!(stage.status, "unverified");
        assert_eq!(
            stage.missing,
            vec!["tool_magic_not_for_signoff".to_string()]
        );
        let json = serde_json::to_string(&manifest).unwrap();
        let back: FlowManifest = serde_json::from_str(&json).unwrap();
        assert!(back.tools[0].not_for_signoff.is_some());
        manifest.tools[0].not_for_signoff = Some(String::new());
        assert!(matches!(assess(&manifest), Err(FlowError::Manifest(_))));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rejects_oversize_and_malformed_manifest() {
        let dir = scratch("limits");
        let path = dir.join("manifest.json");
        File::create(&path)
            .unwrap()
            .set_len(MAX_MANIFEST_BYTES + 1)
            .unwrap();
        assert!(matches!(assess_path(&path), Err(FlowError::Budget(_))));
        fs::write(&path, b"{broken").unwrap();
        assert!(matches!(assess_path(&path), Err(FlowError::Json(_))));
        let mut m = empty();
        m.schema = "not-supported".into();
        assert!(matches!(assess(&m), Err(FlowError::Manifest(_))));
        fs::remove_dir_all(dir).unwrap();
    }
}
