//! Canonical complete project file tree.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use super::thread_snapshot::validate_canonical_hash;

/// Maps canonical project-relative regular-file paths to [`super::ProjectFile`]
/// object hashes. Empty directories are intentionally not represented in v1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTree {
    pub files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectTreeWire {
    kind: String,
    schema: u32,
    files: BTreeMap<String, String>,
}

impl ProjectTree {
    pub const SCHEMA: u32 = 1;

    pub fn validate(&self) -> anyhow::Result<()> {
        for (path, file_hash) in &self.files {
            super::validate_canonical_project_relative_path(path)
                .with_context(|| format!("project_tree contains invalid path {path:?}"))?;
            validate_canonical_hash("project_tree file hash", file_hash)?;
            let mut end = path.len();
            while let Some(separator) = path[..end].rfind('/') {
                let ancestor = &path[..separator];
                if self.files.contains_key(ancestor) {
                    anyhow::bail!(
                        "project_tree path {path:?} is nested below regular file {ancestor:?}"
                    );
                }
                end = separator;
            }
        }
        Ok(())
    }

    pub fn to_value(&self) -> Value {
        json!({
            "kind": "project_tree",
            "schema": Self::SCHEMA,
            "files": self.files,
        })
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ProjectTreeWire = serde_json::from_value(value.clone())
            .context("failed to deserialize project_tree schema 1")?;
        if wire.kind != "project_tree" {
            anyhow::bail!(
                "project_tree kind mismatch: expected project_tree, got {}",
                wire.kind
            );
        }
        if wire.schema != Self::SCHEMA {
            anyhow::bail!(
                "project_tree schema mismatch: expected {}, got {}",
                Self::SCHEMA,
                wire.schema
            );
        }
        let tree = Self { files: wire.files };
        tree.validate()?;
        Ok(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_is_path_order_independent() {
        let mut files = BTreeMap::new();
        files.insert("weights/model.bin".into(), "ab".repeat(32));
        files.insert("src/main.rs".into(), "cd".repeat(32));
        let tree = ProjectTree { files };
        assert_eq!(ProjectTree::from_value(&tree.to_value()).unwrap(), tree);
    }
}
