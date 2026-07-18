//! Versioned regular-file object used by full project snapshots.

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use super::thread_snapshot::validate_canonical_hash;

/// Immutable regular-file facts. The project-relative path lives only in the
/// containing [`super::ProjectTree`], so it cannot contradict this object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub blob_hash: String,
    pub size: u64,
    pub normalized_mode: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFileWire {
    kind: String,
    schema: u32,
    blob_hash: String,
    size: u64,
    normalized_mode: u32,
}

impl ProjectFile {
    pub const SCHEMA: u32 = 1;
    pub const REGULAR_MODE: u32 = 0o644;
    pub const EXECUTABLE_MODE: u32 = 0o755;

    pub fn normalize_mode(mode: u32) -> u32 {
        if mode & 0o111 == 0 {
            Self::REGULAR_MODE
        } else {
            Self::EXECUTABLE_MODE
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_hash("project_file blob_hash", &self.blob_hash)?;
        if !matches!(
            self.normalized_mode,
            Self::REGULAR_MODE | Self::EXECUTABLE_MODE
        ) {
            anyhow::bail!(
                "project_file normalized_mode must be 0o644 or 0o755, got {:#o}",
                self.normalized_mode
            );
        }
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "kind": "project_file",
            "schema": Self::SCHEMA,
            "blob_hash": self.blob_hash,
            "size": self.size,
            "normalized_mode": self.normalized_mode,
        })
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ProjectFileWire = serde_json::from_value(value.clone())
            .context("failed to deserialize project_file schema 1")?;
        if wire.kind != "project_file" {
            anyhow::bail!(
                "project_file kind mismatch: expected project_file, got {}",
                wire.kind
            );
        }
        if wire.schema != Self::SCHEMA {
            anyhow::bail!(
                "project_file schema mismatch: expected {}, got {}",
                Self::SCHEMA,
                wire.schema
            );
        }
        let file = Self {
            blob_hash: wire.blob_hash,
            size: wire.size,
            normalized_mode: wire.normalized_mode,
        };
        file.validate()?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_blob_can_have_distinct_normalized_modes() {
        let regular = ProjectFile {
            blob_hash: "ab".repeat(32),
            size: 7,
            normalized_mode: ProjectFile::REGULAR_MODE,
        };
        let executable = ProjectFile {
            normalized_mode: ProjectFile::EXECUTABLE_MODE,
            ..regular.clone()
        };
        assert_ne!(regular.to_value(), executable.to_value());
        assert!(ProjectFile::from_value(&regular.to_value()).is_ok());
        assert!(ProjectFile::from_value(&executable.to_value()).is_ok());
    }
}
