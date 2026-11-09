// SPDX-License-Identifier: Apache-2.0
//! Explicit, hash-reported master bus metadata for file-only ECOs.
use super::{Artifact, artifact_of, fail, read_file};
use crate::Result;
use rustroad_formats::eco::PinBuses;
use serde::de::{Deserialize, Deserializer, Error, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::path::Path;

struct UniqueMap<T>(BTreeMap<String, T>);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for UniqueMap<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct MapVisitor<T>(PhantomData<fn() -> T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for MapVisitor<T> {
            type Value = UniqueMap<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded map with unique keys")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, T>()? {
                    if out.len() >= 65536 || out.insert(key, value).is_some() {
                        return Err(M::Error::custom(
                            "duplicate or excessive bus-schema entries",
                        ));
                    }
                }
                Ok(UniqueMap(out))
            }
        }
        d.deserialize_map(MapVisitor(PhantomData))
    }
}

pub(super) fn load(path: Option<&Path>) -> Result<(PinBuses, Option<Artifact>)> {
    let Some(path) = path else {
        return Ok((PinBuses::new(), None));
    };
    let (text, pin) = read_file(path, 2 * 1024 * 1024, "cell_buses")?;
    let parsed: UniqueMap<UniqueMap<[i64; 2]>> =
        serde_json::from_str(&text).map_err(|e| fail("ECO_BUS_SCHEMA", e))?;
    if parsed.0.len() > 256 {
        return Err(fail(
            "ECO_BUS_SCHEMA",
            "more than256 master bus definitions",
        ));
    }
    Ok((
        parsed.0.into_iter().map(|(k, v)| (k, v.0)).collect(),
        Some(artifact_of(&pin)),
    ))
}
