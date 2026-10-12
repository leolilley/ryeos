//! Newtype IDs for the TUI model.
//!
//! All IDs are thin wrappers around u64 for type safety.
//! Deterministic constructors support testing.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub u64);

        impl $name {
            pub fn new(v: u64) -> Self {
                Self(v)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

id_type!(TileId);
id_type!(ViewGroupId);
id_type!(ViewSetId);
id_type!(ThreadId);
id_type!(ThreadTurnId);
id_type!(ThreadRowId);
id_type!(RemoteId);
id_type!(ProjectId);
id_type!(GraphId);
id_type!(ExecutionId);
id_type!(ItemId);

// Opaque daemon-issued coordinates, not UUIDs parsed by another owner. Syntax
// prevents address delimiters; transport/admission owners enforce byte bounds
// and retained membership. Possessing an ID never grants authority.
pub const MAX_PRESENTATION_COORDINATE_BYTES: usize = 128;

fn address_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

macro_rules! presentation_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn try_new(value: impl Into<String>) -> Result<Self, &'static str> {
                let value = value.into();
                if !address_identifier(&value) || value.len() > MAX_PRESENTATION_COORDINATE_BYTES {
                    return Err("presentation coordinate must be a nonempty opaque identifier");
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = &'static str;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::try_new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

presentation_id!(PresentationId);
presentation_id!(SurfaceInstanceId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiViewMountOwner {
    Shared {
        presentation_id: PresentationId,
    },
    Surface {
        presentation_id: PresentationId,
        surface_instance_id: SurfaceInstanceId,
    },
}

/// Stable identity of one mounted view instance, independent of its bound
/// `view:` ref. Two hosts rendering the same view have different keys, while a
/// view replacement in the same host retains its key.
///
/// The string representation is part of the renderer/event boundary. Callers
/// construct keys through the host-specific constructors rather than composing
/// layout addresses themselves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RyeOsViewInstanceKey(String);

impl RyeOsViewInstanceKey {
    pub fn shared_dock(presentation_id: &PresentationId, edge: &str) -> Result<Self, &'static str> {
        if !matches!(edge, "top" | "bottom" | "left" | "right") {
            return Err("shared dock edge is invalid");
        }
        Ok(Self(format!(
            "seat:{}:dock:{edge}",
            presentation_id.as_str()
        )))
    }

    pub fn shared_dialog(
        presentation_id: &PresentationId,
        mount_id: &str,
    ) -> Result<Self, &'static str> {
        if !address_identifier(mount_id) {
            return Err("dialog mount id is invalid");
        }
        Ok(Self(format!(
            "seat:{}:dialog:{mount_id}",
            presentation_id.as_str()
        )))
    }

    pub fn surface_tile(
        presentation_id: &PresentationId,
        surface_id: &SurfaceInstanceId,
        tile_id: TileId,
    ) -> Self {
        Self(format!(
            "seat:{}:surface:{}:tile:{}",
            presentation_id.as_str(),
            surface_id.as_str(),
            tile_id.0
        ))
    }

    pub fn surface_dialog(
        presentation_id: &PresentationId,
        surface_id: &SurfaceInstanceId,
        mount_id: &str,
    ) -> Result<Self, &'static str> {
        if !address_identifier(mount_id) || mount_id.len() > MAX_PRESENTATION_COORDINATE_BYTES {
            return Err("dialog mount id is invalid");
        }
        Ok(Self(format!(
            "seat:{}:surface:{}:dialog:{mount_id}",
            presentation_id.as_str(),
            surface_id.as_str()
        )))
    }

    pub fn mount_owner(&self) -> Option<UiViewMountOwner> {
        let parts = self.0.split(':').collect::<Vec<_>>();
        match parts.as_slice() {
            ["seat", presentation, "dock" | "dialog", _] => Some(UiViewMountOwner::Shared {
                presentation_id: PresentationId::try_new(*presentation).ok()?,
            }),
            [
                "seat",
                presentation,
                "surface",
                surface,
                "tile" | "dialog",
                _,
            ] => Some(UiViewMountOwner::Surface {
                presentation_id: PresentationId::try_new(*presentation).ok()?,
                surface_instance_id: SurfaceInstanceId::try_new(*surface).ok()?,
            }),
            _ => None,
        }
    }

    /// Tile identity only within the canonical Surface-scoped wire grammar.
    pub fn surface_tile_id(&self) -> Option<TileId> {
        let parts = self.as_str().split(':').collect::<Vec<_>>();
        let ["seat", presentation, "surface", surface, "tile", tile] = parts.as_slice() else {
            return None;
        };
        let presentation = PresentationId::try_new(*presentation).ok()?;
        let surface = SurfaceInstanceId::try_new(*surface).ok()?;
        let tile = TileId::new(tile.parse::<u64>().ok()?);
        (Self::surface_tile(&presentation, &surface, tile).as_str() == self.as_str())
            .then_some(tile)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_canonical(value: &str) -> Option<Self> {
        let parts = value.split(':').collect::<Vec<_>>();
        let scoped = match parts.as_slice() {
            ["seat", presentation, "dock", edge] => {
                Self::shared_dock(&PresentationId::try_new(*presentation).ok()?, edge).ok()
            }
            ["seat", presentation, "dialog", mount] => {
                Self::shared_dialog(&PresentationId::try_new(*presentation).ok()?, mount).ok()
            }
            ["seat", presentation, "surface", surface, "tile", tile] => Some(Self::surface_tile(
                &PresentationId::try_new(*presentation).ok()?,
                &SurfaceInstanceId::try_new(*surface).ok()?,
                TileId::new(tile.parse().ok()?),
            )),
            ["seat", presentation, "surface", surface, "dialog", mount] => Self::surface_dialog(
                &PresentationId::try_new(*presentation).ok()?,
                &SurfaceInstanceId::try_new(*surface).ok()?,
                mount,
            )
            .ok(),
            _ => None,
        };
        if let Some(key) = scoped {
            return (key.as_str() == value).then_some(key);
        }
        None
    }
}

impl<'de> Deserialize<'de> for RyeOsViewInstanceKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(&value)
            .ok_or_else(|| serde::de::Error::custom("noncanonical mounted View address"))
    }
}

impl fmt::Display for RyeOsViewInstanceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reference to a RYE item (directive, tool, knowledge, config).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemRef(pub String);

impl ItemRef {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Type alias for convenience.
pub type TileIdCounter = u64;

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn scoped_mounts_do_not_collide_or_invent_view_sets() {
        let first = PresentationId::try_new("first").unwrap();
        let second = PresentationId::try_new("second").unwrap();
        let a = SurfaceInstanceId::try_new("a").unwrap();
        let b = SurfaceInstanceId::try_new("b").unwrap();
        let keys = [
            RyeOsViewInstanceKey::shared_dock(&first, "left").unwrap(),
            RyeOsViewInstanceKey::shared_dialog(&first, "left").unwrap(),
            RyeOsViewInstanceKey::shared_dock(&second, "left").unwrap(),
            RyeOsViewInstanceKey::surface_tile(&first, &a, TileId::new(1)),
            RyeOsViewInstanceKey::surface_tile(&first, &b, TileId::new(1)),
            RyeOsViewInstanceKey::surface_dialog(&first, &a, "left").unwrap(),
            RyeOsViewInstanceKey::surface_dialog(&first, &b, "left").unwrap(),
        ];
        assert_eq!(
            keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
            keys.len()
        );
        for key in keys {
            assert!(key.mount_owner().is_some());
            assert_eq!(
                RyeOsViewInstanceKey::from_canonical(key.as_str()),
                Some(key.clone())
            );
            assert_eq!(
                serde_json::from_value::<RyeOsViewInstanceKey>(serde_json::to_value(&key).unwrap())
                    .unwrap(),
                key
            );
        }
    }

    #[test]
    fn unchecked_deserialization_and_noncanonical_addresses_are_refused() {
        for value in [
            "seat:p:surface:s:tile:01",
            "seat:p:surface:s:tile:+1",
            "seat:p:surface:s:tile:18446744073709551616",
            "seat:p:dock:middle",
            "seat:p:dialog:",
            "seat::dialog:launch",
            "seat:p:dialog:launch:extra",
            "seat:p:surface:s:dialog:",
            "seat:p:surface:s:dialog:launch:extra",
            "view-set:01:slot:left",
            "tile:01",
            "tile:1",
            "view-set:1:slot:left",
        ] {
            assert!(
                RyeOsViewInstanceKey::from_canonical(value).is_none(),
                "{value}"
            );
            assert!(
                serde_json::from_value::<RyeOsViewInstanceKey>(serde_json::json!(value)).is_err(),
                "{value}"
            );
        }
        for value in ["", "p:q", "p\nq", "p/q"] {
            assert!(PresentationId::try_new(value).is_err());
            assert!(serde_json::from_value::<SurfaceInstanceId>(serde_json::json!(value)).is_err());
        }
        assert!(
            PresentationId::try_new("p".repeat(MAX_PRESENTATION_COORDINATE_BYTES + 1)).is_err()
        );
    }
}
