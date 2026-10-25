//! Per-PDK GDS layer-map profile: LEF layer name -> GDS (layer, datatype) per
//! purpose, plus the placement/routing boundary layer. Pure data with
//! provenance: every entry cites the PDK files it was derived from by sha256
//! and line number; `scripts/verify_gds.py` re-checks each citation against the
//! cited files. Unknown fields, duplicate entries and missing purposes are
//! rejected, so a layer can never silently disappear from a stream.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const SCHEMA: &str = "rustroad.gds-layer-map.v1";
const MAX_SOURCES: usize = 32;
const MAX_LAYERS: usize = 256;
const MAX_TARGETS: usize = 8;
const MAX_CITED_LINES: usize = 32;

/// `[layer, datatype]`, both in the GDSII two-byte range 0..=32767.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GdsLayer(pub i16, pub i16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKind {
    Routing,
    Cut,
}

/// What a shape represents in the streamed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Regular routed wiring (DEF NETS).
    Net,
    /// Power/ground wiring (DEF SPECIALNETS).
    SpecialNet,
    /// Geometry inside a fixed LEF VIA cell.
    Via,
    /// Top-level I/O and supply pin shapes (DEF PINS).
    Pin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapSource {
    pub id: String,
    pub role: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cite {
    pub source: String,
    pub sha256: String,
    pub lines: Vec<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerEntry {
    pub lef: String,
    pub kind: LayerKind,
    #[serde(default)]
    pub net: Vec<GdsLayer>,
    #[serde(default)]
    pub special_net: Vec<GdsLayer>,
    #[serde(default)]
    pub via: Vec<GdsLayer>,
    #[serde(default)]
    pub pin: Vec<GdsLayer>,
    /// Text layer for pin-name labels.
    #[serde(default)]
    pub label: Option<GdsLayer>,
    pub cite: Vec<Cite>,
}
impl LayerEntry {
    pub fn targets(&self, purpose: Purpose) -> &[GdsLayer] {
        match purpose {
            Purpose::Net => &self.net,
            Purpose::SpecialNet => &self.special_net,
            Purpose::Via => &self.via,
            Purpose::Pin => &self.pin,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryEntry {
    pub layer: GdsLayer,
    pub cite: Vec<Cite>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerMap {
    pub schema: String,
    pub pdk: String,
    pub library: String,
    pub sources: Vec<MapSource>,
    pub layers: Vec<LayerEntry>,
    pub boundary: BoundaryEntry,
    #[serde(default)]
    pub notes: Vec<String>,
}

fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl LayerMap {
    pub fn entry(&self, lef: &str) -> Option<&LayerEntry> {
        self.layers.iter().find(|l| l.lef == lef)
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!("layer map schema must be {SCHEMA}"));
        }
        if !identifier(&self.pdk) || !identifier(&self.library) {
            return Err("layer map pdk/library must be identifiers".into());
        }
        if self.sources.is_empty() || self.sources.len() > MAX_SOURCES {
            return Err(format!("layer map needs 1..={MAX_SOURCES} sources"));
        }
        let mut ids = HashSet::new();
        for s in &self.sources {
            if !identifier(&s.id) || !ids.insert(s.id.as_str()) {
                return Err(format!("invalid or duplicate source id {:?}", s.id));
            }
            if s.path.is_empty() || s.role.is_empty() || !digest(&s.sha256) || s.bytes == 0 {
                return Err(format!("source {} lacks path/role/sha256/bytes", s.id));
            }
        }
        let check_cites = |what: &str, cites: &[Cite]| -> Result<(), String> {
            if cites.is_empty() {
                return Err(format!("{what} has no PDK citation"));
            }
            for c in cites {
                let source = self
                    .sources
                    .iter()
                    .find(|s| s.id == c.source)
                    .ok_or_else(|| format!("{what} cites unknown source {}", c.source))?;
                if c.sha256 != source.sha256 {
                    return Err(format!("{what} citation sha256 differs from {}", c.source));
                }
                if c.lines.is_empty() || c.lines.len() > MAX_CITED_LINES || c.lines.contains(&0) {
                    return Err(format!("{what} citation needs 1..={MAX_CITED_LINES} lines"));
                }
            }
            Ok(())
        };
        let check_target = |what: &str, t: &GdsLayer| -> Result<(), String> {
            if t.0 < 0 || t.1 < 0 {
                return Err(format!("{what} has a negative GDS layer/datatype"));
            }
            Ok(())
        };
        if self.layers.is_empty() || self.layers.len() > MAX_LAYERS {
            return Err(format!("layer map needs 1..={MAX_LAYERS} layers"));
        }
        let mut names = HashSet::new();
        for l in &self.layers {
            if !identifier(&l.lef) || !names.insert(l.lef.as_str()) {
                return Err(format!("invalid or duplicate LEF layer {:?}", l.lef));
            }
            check_cites(&l.lef, &l.cite)?;
            for purpose in [
                Purpose::Net,
                Purpose::SpecialNet,
                Purpose::Via,
                Purpose::Pin,
            ] {
                let targets = l.targets(purpose);
                if targets.len() > MAX_TARGETS
                    || targets.iter().collect::<HashSet<_>>().len() != targets.len()
                {
                    return Err(format!(
                        "{} {purpose:?} targets repeat or exceed cap",
                        l.lef
                    ));
                }
                for t in targets {
                    check_target(&l.lef, t)?;
                }
            }
            if let Some(t) = &l.label {
                check_target(&l.lef, t)?;
            }
            let complete = match l.kind {
                LayerKind::Routing => {
                    !l.net.is_empty() && !l.special_net.is_empty() && !l.via.is_empty()
                }
                LayerKind::Cut => {
                    !l.via.is_empty()
                        && l.net.is_empty()
                        && l.special_net.is_empty()
                        && l.pin.is_empty()
                        && l.label.is_none()
                }
            };
            if !complete {
                return Err(format!(
                    "{} purposes are incomplete for a {:?} layer",
                    l.lef, l.kind
                ));
            }
        }
        check_cites("boundary", &self.boundary.cite)?;
        check_target("boundary", &self.boundary.layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> LayerMap {
        let sha = "a".repeat(64);
        let cite = vec![Cite {
            source: "map".into(),
            sha256: sha.clone(),
            lines: vec![1],
        }];
        LayerMap {
            schema: SCHEMA.into(),
            pdk: "demo".into(),
            library: "cells".into(),
            sources: vec![MapSource {
                id: "map".into(),
                role: "klayout_lefdef_map".into(),
                path: "/pdk/demo.map".into(),
                sha256: sha,
                bytes: 10,
            }],
            layers: vec![
                LayerEntry {
                    lef: "M1".into(),
                    kind: LayerKind::Routing,
                    net: vec![GdsLayer(1, 0)],
                    special_net: vec![GdsLayer(1, 0)],
                    via: vec![GdsLayer(1, 0)],
                    pin: vec![GdsLayer(1, 0), GdsLayer(1, 2)],
                    label: Some(GdsLayer(1, 1)),
                    cite: cite.clone(),
                },
                LayerEntry {
                    lef: "V1".into(),
                    kind: LayerKind::Cut,
                    net: vec![],
                    special_net: vec![],
                    via: vec![GdsLayer(2, 0)],
                    pin: vec![],
                    label: None,
                    cite: cite.clone(),
                },
            ],
            boundary: BoundaryEntry {
                layer: GdsLayer(235, 4),
                cite,
            },
            notes: vec![],
        }
    }
    #[test]
    fn valid_profiles_pass_and_every_defect_is_rejected() {
        assert!(sample().validate().is_ok());
        type Edit = Box<dyn Fn(&mut LayerMap)>;
        let edits: Vec<Edit> = vec![
            Box::new(|m| m.schema = "v0".into()),
            Box::new(|m| m.layers[0].cite.clear()),
            Box::new(|m| m.layers[0].cite[0].sha256 = "b".repeat(64)),
            Box::new(|m| m.layers[0].cite[0].source = "other".into()),
            Box::new(|m| m.layers[0].cite[0].lines = vec![0]),
            Box::new(|m| m.layers[1].lef = "M1".into()),
            Box::new(|m| m.layers[0].net.clear()),
            Box::new(|m| m.layers[0].pin.push(GdsLayer(1, 0))),
            Box::new(|m| m.layers[1].net.push(GdsLayer(2, 0))),
            Box::new(|m| m.layers[1].label = Some(GdsLayer(2, 5))),
            Box::new(|m| m.boundary.layer = GdsLayer(-1, 0)),
            Box::new(|m| m.sources[0].sha256 = "XYZ".into()),
            Box::new(|m| m.sources.push(m.sources[0].clone())),
        ];
        for (i, edit) in edits.iter().enumerate() {
            let mut m = sample();
            edit(&mut m);
            assert!(m.validate().is_err(), "edit {i} accepted");
        }
    }
    #[test]
    fn unknown_fields_are_rejected_by_the_schema() {
        let mut value: serde_json::Value = serde_json::to_value(sample()).unwrap();
        value["layers"][0]["datatype"] = 3.into();
        assert!(serde_json::from_value::<LayerMap>(value).is_err());
    }
}
