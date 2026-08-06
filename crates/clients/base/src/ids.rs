//! Newtype IDs for the TUI model.
//!
//! All IDs are thin wrappers around u64 for type safety.
//! Deterministic constructors support testing.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
id_type!(ThreadId);
id_type!(ThreadTurnId);
id_type!(ThreadRowId);
id_type!(RemoteId);
id_type!(ProjectId);
id_type!(GraphId);
id_type!(ExecutionId);
id_type!(ItemId);

/// Stable identity of one mounted view instance, independent of its bound
/// `view:` ref. Two hosts rendering the same view have different keys, while a
/// view replacement in the same host retains its key.
///
/// The string representation is part of the renderer/event boundary. Callers
/// construct keys through the host-specific constructors rather than composing
/// layout addresses themselves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RyeOsViewInstanceKey(String);

impl RyeOsViewInstanceKey {
    pub fn workspace_tile(tile_id: TileId) -> Self {
        Self(format!("tile:{}", tile_id.0))
    }

    pub fn surface_slot(edge: &str) -> Self {
        Self(format!("dock:{edge}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn workspace_tile_id(&self) -> Option<TileId> {
        self.0
            .strip_prefix("tile:")?
            .parse::<u64>()
            .ok()
            .map(TileId::new)
    }

    pub(crate) fn from_canonical(value: &str) -> Option<Self> {
        if let Some(raw) = value.strip_prefix("tile:") {
            let tile_id = raw.parse::<u64>().ok().map(TileId::new)?;
            let key = Self::workspace_tile(tile_id);
            return (key.as_str() == value).then_some(key);
        }
        if let Some(edge) = value.strip_prefix("dock:")
            && matches!(edge, "top" | "bottom" | "left" | "right")
        {
            return Some(Self::surface_slot(edge));
        }
        None
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
