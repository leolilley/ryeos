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
