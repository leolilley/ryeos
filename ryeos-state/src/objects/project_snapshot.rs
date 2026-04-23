//! ProjectSnapshot — a snapshot of a project's source state.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A snapshot of a project's source files at a point in time.
///
/// Stored in CAS as an immutable object. Linked from thread snapshots
/// via `base_project_snapshot_hash` and `result_project_snapshot_hash`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    /// Hash of the project manifest (root descriptor).
    pub project_manifest_hash: String,
    /// Hash of the user-level manifest (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_manifest_hash: Option<String>,
    /// Parent snapshot hashes (DAG structure for versioning).
    pub parent_hashes: Vec<String>,
    /// ISO-8601 timestamp of creation.
    pub created_at: String,
    /// Source type (e.g. "fold_back", "manual_push").
    pub source: String,
}

impl ProjectSnapshot {
    /// Schema version for project snapshots.
    pub const SCHEMA: u32 = 2;

    /// Serialize to a CAS JSON object.
    pub fn to_value(&self) -> Value {
        let mut v = json!({
            "kind": "project_snapshot",
            "schema": Self::SCHEMA,
            "project_manifest_hash": self.project_manifest_hash,
            "parent_hashes": self.parent_hashes,
            "created_at": self.created_at,
            "source": self.source,
        });
        if let Some(ref umh) = self.user_manifest_hash {
            v.as_object_mut()
                .unwrap()
                .insert("user_manifest_hash".into(), json!(umh));
        }
        v
    }

    /// Deserialize from a CAS JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        Ok(Self {
            project_manifest_hash: value
                .get("project_manifest_hash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing project_manifest_hash"))?
                .to_string(),
            user_manifest_hash: value
                .get("user_manifest_hash")
                .and_then(|v| v.as_str())
                .map(String::from),
            parent_hashes: value
                .get("parent_hashes")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            created_at: value
                .get("created_at")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing created_at"))?
                .to_string(),
            source: value
                .get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing source"))?
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let original = ProjectSnapshot {
            project_manifest_hash: "ab".repeat(32),
            user_manifest_hash: None,
            parent_hashes: vec![],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "fold_back".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(restored.project_manifest_hash, original.project_manifest_hash);
        assert_eq!(restored.source, "fold_back");
    }

    #[test]
    fn roundtrip_with_user_manifest() {
        let original = ProjectSnapshot {
            project_manifest_hash: "ab".repeat(32),
            user_manifest_hash: Some("cd".repeat(32)),
            parent_hashes: vec!["ef".repeat(32)],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "manual_push".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(restored.user_manifest_hash, Some("cd".repeat(32)));
        assert_eq!(restored.parent_hashes.len(), 1);
    }
}
