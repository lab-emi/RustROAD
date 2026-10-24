//! Read-only native RC extraction of a routed revision to detailed SPEF.
//!
//! The workspace is opened under its shared lock and never written. The SPEF
//! is written atomically next to `--out` (temporary file, fsync, rename) and is
//! refused inside the workspace directory. Model: docs/extraction.md.
use crate::pdk::read_input;
use crate::routing::{ObstacleOwner, build_input_with_owners};
use crate::{Error, Result};
use rustroad_core::Limits;
use rustroad_core::connectivity::{Direction, PinOwner};
use rustroad_core::extract::{
    self, ExtractLimits, ExtractStats, PassiveOwner, PassiveShape, RcContext, RcLayer, RcNode,
    RcTech,
};
use rustroad_formats::spef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

pub const RC_PROFILE_SCHEMA: &str = "rustroad.rc-profile.v1";
const MAX_PROFILE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_SPEF_BYTES: u64 = 32 * 1024 * 1024;
const HARD_MAX_SPEF_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileUnits {
    pub length: String,
    pub capacitance: String,
    pub resistance: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSource {
    pub role: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileContext {
    pub over: u32,
    pub under: Option<u32>,
    pub rows: Vec<[f64; 3]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLayer {
    pub name: String,
    pub rules_metal: u32,
    pub width_um: f64,
    pub sheet_resistance_ohm_per_square: f64,
    pub contexts: Vec<ProfileContext>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileCut {
    pub name: String,
    pub lower: String,
    pub upper: String,
    pub resistance_ohm_per_cut: f64,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lef_resistance_ohm_nominal: Option<f64>,
}
/// `rustroad.rc-profile.v1`, produced by scripts/build_rc_profile.py.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RcProfile {
    pub schema: String,
    pub pdk: String,
    pub library: String,
    pub corner: String,
    pub pdk_version: String,
    pub technology_lef_sha256: String,
    pub units: ProfileUnits,
    pub magic_variant: String,
    pub lef_parasitics: String,
    pub sources: Vec<ProfileSource>,
    pub model: serde_json::Value,
    pub layers: Vec<ProfileLayer>,
    pub cuts: Vec<ProfileCut>,
    pub cross_checks: serde_json::Value,
    pub diagnostics: Vec<String>,
    pub generator: String,
}

#[derive(Clone, Debug)]
pub struct ExtractRequest {
    pub profile: PathBuf,
    pub corner: String,
    pub out: PathBuf,
    pub max_spef_bytes: u64,
    pub limits: ExtractLimits,
}

#[derive(Debug, Serialize)]
pub struct ArtifactPin {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Serialize)]
pub struct ExtractScope {
    pub corner: String,
    pub pdk: String,
    pub wire_model: &'static str,
    pub coupling: &'static str,
    pub coupling_reason: &'static str,
    pub supply_nets: &'static str,
    pub pin_capacitance: &'static str,
    pub via_capacitance: &'static str,
    pub wire_ends: &'static str,
    pub metal_union: &'static str,
    pub diagonal_coupling: &'static str,
    pub crossing_fringe: &'static str,
    pub cell_internal_metal: &'static str,
    pub routing_patches: &'static str,
    pub max_segment_length_dbu: i64,
    pub drc_status: &'static str,
    pub lvs_status: &'static str,
}
#[derive(Debug, Serialize)]
pub struct ExtractReport {
    pub status: &'static str,
    pub schema: &'static str,
    pub workspace_revision: String,
    pub design: String,
    pub output: ArtifactPin,
    pub rc_profile: ArtifactPin,
    pub scope: ExtractScope,
    pub stats: ExtractStats,
    pub routed_nets: usize,
    pub unannotated_signal_nets: Vec<String>,
    pub diagnostics: Vec<String>,
}

fn fail(s: impl Into<String>) -> Error {
    Error::Invalid(s.into())
}

fn load_profile(path: &Path) -> Result<(RcProfile, ArtifactPin)> {
    let (text, source) = read_input(path, MAX_PROFILE_BYTES, "rc_profile")?;
    let profile: RcProfile = serde_json::from_str(&text)?;
    if profile.schema != RC_PROFILE_SCHEMA
        || profile.units.length != "um"
        || profile.units.capacitance != "pF"
        || profile.units.resistance != "ohm"
    {
        return Err(fail("RC_PROFILE: unsupported schema or units"));
    }
    Ok((
        profile,
        ArtifactPin {
            path: source.path,
            sha256: source.sha256,
            bytes: source.bytes as u64,
        },
    ))
}

/// Writer that enforces a byte cap and hashes everything it forwards.
struct Capped<W: Write> {
    inner: W,
    written: u64,
    cap: u64,
    hash: Sha256,
}
impl<W: Write> Write for Capped<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let next = self.written.saturating_add(buf.len() as u64);
        if next > self.cap {
            return Err(io::Error::other("BUDGET_EXCEEDED: SPEF byte cap"));
        }
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn output_outside_workspace(workspace: &Path, out: &Path) -> Result<PathBuf> {
    let name = out
        .file_name()
        .ok_or_else(|| fail("OUTPUT_PATH: --out must name a file"))?;
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let parent = fs::canonicalize(&parent)
        .map_err(|e| fail(format!("OUTPUT_PATH: output directory: {e}")))?;
    let root = fs::canonicalize(workspace)?;
    if parent.starts_with(&root) {
        return Err(fail(
            "OUTPUT_IN_WORKSPACE: extraction is read-only; write the SPEF outside the workspace",
        ));
    }
    Ok(parent.join(name))
}

/// Extract the HEAD revision of a routed workspace into detailed SPEF.
pub fn extract(
    workspace: &Path,
    request: &ExtractRequest,
    limits: &Limits,
) -> Result<ExtractReport> {
    if request.max_spef_bytes == 0 || request.max_spef_bytes > HARD_MAX_SPEF_BYTES {
        return Err(Error::Budget("SPEF byte cap outside hard bounds".into()));
    }
    let out = output_outside_workspace(workspace, &request.out)?;
    let (profile, profile_pin) = load_profile(&request.profile)?;
    if profile.corner != request.corner {
        return Err(fail(format!(
            "CORNER_MISMATCH: --corner {} but the RC profile describes corner {}",
            request.corner, profile.corner
        )));
    }
    let snapshot = crate::read(workspace, limits)?;
    let metadata = snapshot.metadata();
    let design = &metadata.design;
    let bundle = metadata
        .physical
        .as_ref()
        .ok_or_else(|| fail("NO_PHYSICAL: import a PDK design first"))?;
    let state = bundle
        .routing
        .as_ref()
        .ok_or_else(|| fail("NO_ROUTING: route before extraction"))?;
    let tech_lef = bundle
        .sources
        .iter()
        .find(|s| s.role == "technology_lef")
        .ok_or_else(|| fail("RC_PROFILE: workspace has no technology LEF identity"))?;
    if tech_lef.sha256 != profile.technology_lef_sha256 {
        return Err(fail(format!(
            "RC_PROFILE: profile is for technology LEF {} but the workspace imported {}",
            profile.technology_lef_sha256, tech_lef.sha256
        )));
    }
    let placed: Vec<_> = snapshot.records().collect();
    let (input, owners) =
        build_input_with_owners(design, bundle, &placed, &state.config, state.scope)?;
    // Bind the profile to the routing layers by name.
    let dbu = f64::from(design.units_per_micron);
    let by_name: HashMap<_, _> = profile
        .layers
        .iter()
        .map(|l| (l.name.as_str(), l))
        .collect();
    let cuts: HashMap<_, _> = profile.cuts.iter().map(|c| (c.name.as_str(), c)).collect();
    let mut layers = Vec::with_capacity(input.layers.len());
    for l in &input.layers {
        if l.vertical.is_none() {
            layers.push(None);
            continue;
        }
        let p = by_name.get(l.name.as_str()).ok_or_else(|| {
            fail(format!(
                "RC_PROFILE: routing layer {} absent from profile",
                l.name
            ))
        })?;
        let width = p.width_um * dbu;
        if (width - width.round()).abs() > 1e-6 {
            return Err(fail("RC_PROFILE: calibrated width is not an integer DBU"));
        }
        layers.push(Some(RcLayer {
            metal: p.rules_metal,
            width: width.round() as i64,
            sheet_ohm: p.sheet_resistance_ohm_per_square,
            contexts: p
                .contexts
                .iter()
                .map(|c| RcContext {
                    over: c.over,
                    under: c.under,
                    rows: c.rows.clone(),
                })
                .collect(),
        }));
    }
    let mut via_ohm = Vec::with_capacity(input.vias.len());
    for v in &input.vias {
        let cut_layers: Vec<_> = v
            .shapes
            .iter()
            .filter(|s| input.layers[s.layer as usize].vertical.is_none())
            .collect();
        let first = cut_layers
            .first()
            .ok_or_else(|| fail("RC_PROFILE: via without a cut shape"))?;
        if cut_layers.iter().any(|s| s.layer != first.layer) {
            return Err(fail("RC_PROFILE: multi-layer cut vias are unsupported"));
        }
        let name = &input.layers[first.layer as usize].name;
        let cut = cuts
            .get(name.as_str())
            .ok_or_else(|| fail(format!("RC_PROFILE: cut layer {name} absent from profile")))?;
        let lower = &input.layers[v.lower as usize].name;
        let upper = &input.layers[v.upper as usize].name;
        if &cut.lower != lower || &cut.upper != upper {
            return Err(fail(format!(
                "RC_PROFILE: cut {name} joins {}/{} but via {} joins {lower}/{upper}",
                cut.lower, cut.upper, v.name
            )));
        }
        if !(cut.resistance_ohm_per_cut.is_finite() && cut.resistance_ohm_per_cut >= 0.0) {
            return Err(fail("RC_PROFILE: invalid via resistance"));
        }
        via_ohm.push(cut.resistance_ohm_per_cut / cut_layers.len() as f64);
    }
    let tech = RcTech {
        dbu_per_um: dbu,
        layers,
        via_ohm,
    };
    let model = &bundle.connectivity;
    let supply: Vec<bool> = model.nets.iter().map(|n| n.use_.is_supply()).collect();
    let passive: Vec<PassiveShape> = input
        .obstacles
        .iter()
        .zip(&owners)
        .map(|(b, o)| PassiveShape {
            layer: b.layer,
            rect: b.rect,
            owner: match (o, b.net) {
                (ObstacleOwner::InstancePin(pin) | ObstacleOwner::PortPin(pin), Some(net))
                    if !supply[net as usize] =>
                {
                    PassiveOwner::Pin { net, pin: *pin }
                }
                _ => PassiveOwner::Ground,
            },
        })
        .collect();
    let network = extract::extract_cpu_ref(
        &extract::ExtractInput {
            layers: &input.layers,
            vias: &input.vias,
            routes: &state.routes,
            terminals: &input.terminals,
            passive: &passive,
            tech: &tech,
        },
        &request.limits,
    )?;
    // Names: nets by connectivity name, pins as instance/pin, ports by bit.
    let pin_label = |pin: u32| -> Result<(spef::Node, spef::Direction, Option<String>)> {
        let c = model
            .pins
            .get(pin as usize)
            .ok_or_else(|| fail("pin index outside connectivity"))?;
        Ok(match c.owner {
            PinOwner::Instance { instance, terminal } => {
                let master = &model.masters[model.instance_masters[instance as usize] as usize];
                let mp = &master.pins[terminal as usize];
                (
                    spef::Node::Pin {
                        instance: design.cells[instance as usize].name.clone(),
                        pin: mp.name.clone(),
                    },
                    match mp.direction {
                        Some(Direction::Input) => spef::Direction::Input,
                        Some(Direction::Output) => spef::Direction::Output,
                        _ => spef::Direction::Bidirectional,
                    },
                    Some(master.name.clone()),
                )
            }
            PinOwner::Port { port, bit } => {
                let p = &model.ports[port as usize];
                (
                    spef::Node::Port(p.bits[bit as usize].clone()),
                    match p.direction {
                        Direction::Input => spef::Direction::Input,
                        Direction::Output => spef::Direction::Output,
                        Direction::Inout => spef::Direction::Bidirectional,
                    },
                    None,
                )
            }
        })
    };
    let index: HashMap<u32, usize> = network
        .nets
        .iter()
        .enumerate()
        .map(|(i, n)| (n.net, i))
        .collect();
    let mut spef_nets = Vec::with_capacity(network.nets.len());
    let mut ports = BTreeMap::<u32, (String, spef::Direction)>::new();
    for rc in &network.nets {
        let mut nodes = Vec::with_capacity(rc.nodes.len());
        let mut connections = Vec::new();
        for (i, node) in rc.nodes.iter().enumerate() {
            match *node {
                RcNode::Pin(pin) => {
                    let (n, direction, cell) = pin_label(pin)?;
                    if let spef::Node::Port(name) = &n {
                        ports.insert(pin, (name.clone(), direction));
                    }
                    nodes.push(n);
                    connections.push(spef::Connection {
                        node: i as u32,
                        direction,
                        cell,
                    });
                }
                RcNode::Internal(k) => nodes.push(spef::Node::Internal(k)),
            }
        }
        spef_nets.push(spef::Net {
            name: model.nets[rc.net as usize].name.clone(),
            nodes,
            connections,
            ground: rc.ground.clone(),
            couplings: Vec::new(),
            resistors: rc.resistors.clone(),
        });
    }
    for c in &network.couplings {
        let (ia, ib) = (index[&c.a.0], index[&c.b.0]);
        spef_nets[ia].couplings.push((c.a.1, ib, c.b.1, c.pf));
        spef_nets[ib].couplings.push((c.b.1, ia, c.a.1, c.pf));
    }
    let routed: std::collections::BTreeSet<u32> = network.nets.iter().map(|n| n.net).collect();
    let unannotated: Vec<String> = model
        .nets
        .iter()
        .enumerate()
        .filter(|(i, n)| !n.use_.is_supply() && !routed.contains(&(*i as u32)))
        .map(|(_, n)| n.name.clone())
        .collect();
    let header = spef::Header {
        design: design.name.clone(),
        vendor: "RustROAD".into(),
        program: "rustroad extract".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        date: "1970-01-01 00:00:00 (fixed for byte reproducibility)".into(),
    };
    let ports: Vec<_> = ports.into_values().collect();
    // Atomic, capped write.
    let temp = out.with_file_name(format!(
        ".{}.tmp-{}",
        out.file_name().and_then(|n| n.to_str()).unwrap_or("spef"),
        std::process::id()
    ));
    let written = (|| -> Result<(u64, String)> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut capped = Capped {
            inner: BufWriter::with_capacity(256 * 1024, file),
            written: 0,
            cap: request.max_spef_bytes,
            hash: Sha256::new(),
        };
        spef::write(&header, &ports, &spef_nets, &mut capped).map_err(|e| match e {
            spef::SpefError::Io(io) if io.to_string().contains("BUDGET_EXCEEDED") => {
                Error::Budget("SPEF byte cap".into())
            }
            spef::SpefError::Io(io) => Error::Io(io),
            spef::SpefError::Invalid(s) => fail(format!("INVALID_SPEF: {s}")),
        })?;
        capped.flush()?;
        let digest = format!("{:x}", capped.hash.clone().finalize());
        let bytes = capped.written;
        let file: File = capped
            .inner
            .into_inner()
            .map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        Ok((bytes, digest))
    })();
    let (bytes, sha256) = match written {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
    };
    if let Err(e) = fs::rename(&temp, &out) {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    let mut diagnostics = profile.diagnostics.clone();
    if network.stats.opened_loop_resistors > 0 {
        diagnostics.push(format!(
            "RC_LOOP_OPENED: {} loop-closing conductors kept their capacitance but not their resistance",
            network.stats.opened_loop_resistors
        ));
    }
    if network.stats.context_fallback_intervals > 0 {
        diagnostics.push(format!(
            "RC_CONTEXT_FALLBACK: {} intervals used a reduced over/under calibration context",
            network.stats.context_fallback_intervals
        ));
    }
    if network.stats.foreign_overlaps > 0 {
        diagnostics.push(format!(
            "RC_FOREIGN_OVERLAP: {} interval checks found a different-net shape overlapping a wire",
            network.stats.foreign_overlaps
        ));
    }
    if !unannotated.is_empty() {
        diagnostics.push(format!(
            "RC_UNROUTED_NETS: {} signal nets have no routed RC tree and no D_NET",
            unannotated.len()
        ));
    }
    Ok(ExtractReport {
        status: "ok",
        schema: "rustroad.extract.v1",
        workspace_revision: metadata.revision.clone(),
        design: design.name.clone(),
        output: ArtifactPin {
            path: out,
            sha256,
            bytes,
        },
        rc_profile: profile_pin,
        scope: ExtractScope {
            corner: profile.corner.clone(),
            pdk: profile.pdk.clone(),
            wire_model: "rc_tree_per_net",
            coupling: "cross_net_spef",
            coupling_reason: "listed in both nets so totals and SPEF consumers see the same \
                              capacitance; the timing engine grounds coupling at factor 1",
            supply_nets: "excluded",
            pin_capacitance: "excluded (Liberty pin capacitance)",
            via_capacitance: "landing pads extracted (pad metal outside the wires is a \
                              capacitance-only piece at the via node); cut capacitance unmodeled",
            wire_ends: "half-width end extensions (DEF default) extracted as capacitance-only \
                        pieces at the run end node",
            metal_union: "wire bodies and pieces (end extensions, via pads, patches) tile the \
                          routed metal without overlap; sides inside or touching same-net metal \
                          (including the net's own pins) are not exposed",
            diagonal_coupling: "unmodeled",
            crossing_fringe: "unmodeled (a crossing is a plane over the footprint only)",
            cell_internal_metal: "pins and obstructions are neighbours and planes, not extracted",
            routing_patches: "extracted: each piece not covered by same-net metal adds ground and \
                              coupling on all four edges at the RC node of the same-net conductor \
                              it overlaps (no new node or resistor)",
            max_segment_length_dbu: request.limits.max_segment_length,
            drc_status: "unchecked",
            lvs_status: "unchecked",
        },
        stats: network.stats,
        routed_nets: network.nets.len(),
        unannotated_signal_nets: unannotated,
        diagnostics,
    })
}
