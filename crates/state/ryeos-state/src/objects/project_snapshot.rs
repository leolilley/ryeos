//! ProjectSnapshot — a snapshot of a project's source state.

use std::collections::HashSet;

use anyhow::Context;
use serde::Deserialize;
use serde_json::{json, Value};

use super::thread_snapshot::{parse_canonical_timestamp, validate_canonical_hash};
use crate::project_sync::ProjectSyncScope;

/// A snapshot of a project's source files at a point in time.
///
/// Stored in CAS as an immutable object. Linked from thread snapshots
/// via `base_project_snapshot_hash` and `result_project_snapshot_hash`.
#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    /// Hash of the project manifest (root descriptor).
    pub project_manifest_hash: String,
    /// Hash of the user-level manifest (if any).
    pub user_manifest_hash: Option<String>,
    /// Optional operator-facing description for manually created snapshots.
    pub message: Option<String>,
    /// Declared scope of the project manifest.
    pub project_sync_scope: ProjectSyncScope,
    /// Parent snapshot hashes (DAG structure for versioning).
    pub parent_hashes: Vec<String>,
    /// ISO-8601 timestamp of creation.
    pub created_at: String,
    /// Source type (e.g. "fold_back", "manual_push").
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectSnapshotWire {
    kind: String,
    schema: u32,
    project_manifest_hash: String,
    user_manifest_hash: Value,
    message: Value,
    project_sync_scope: ProjectSyncScope,
    parent_hashes: Vec<String>,
    created_at: String,
    source: String,
}

impl ProjectSnapshot {
    /// The single current schema for project snapshots.
    pub const SCHEMA: u32 = 1;

    /// Serialize to a CAS JSON object.
    pub fn to_value(&self) -> Value {
        json!({
            "kind": "project_snapshot",
            "schema": Self::SCHEMA,
            "project_manifest_hash": self.project_manifest_hash,
            "user_manifest_hash": self.user_manifest_hash,
            "message": self.message,
            "project_sync_scope": self.project_sync_scope,
            "parent_hashes": self.parent_hashes,
            "created_at": self.created_at,
            "source": self.source,
        })
    }

    /// Deserialize from a CAS JSON value.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let wire: ProjectSnapshotWire = serde_json::from_value(value.clone())
            .context("failed to deserialize current project_snapshot schema")?;
        if wire.kind != "project_snapshot" {
            anyhow::bail!(
                "project_snapshot kind mismatch: expected project_snapshot, got {}",
                wire.kind
            );
        }
        if wire.schema != Self::SCHEMA {
            anyhow::bail!(
                "project_snapshot schema mismatch: expected {}, got {}",
                Self::SCHEMA,
                wire.schema
            );
        }
        validate_canonical_hash("project_manifest_hash", &wire.project_manifest_hash)?;
        let user_manifest_hash = required_nullable_string(
            "project_snapshot user_manifest_hash",
            &wire.user_manifest_hash,
        )?;
        if let Some(hash) = &user_manifest_hash {
            validate_canonical_hash("user_manifest_hash", hash)?;
        }
        let message = required_nullable_string("project_snapshot message", &wire.message)?;
        let mut parents = HashSet::with_capacity(wire.parent_hashes.len());
        for hash in &wire.parent_hashes {
            validate_canonical_hash("parent_hash", hash)?;
            if !parents.insert(hash.as_str()) {
                anyhow::bail!("project_snapshot contains duplicate parent hash {hash}");
            }
        }
        parse_canonical_timestamp(&wire.created_at)
            .map_err(|error| anyhow::anyhow!("invalid project_snapshot created_at: {error}"))?;
        super::validate_trimmed_control_free("project_snapshot source", &wire.source, false)?;
        if let Some(message) = &message {
            super::validate_trimmed_control_free("project_snapshot message", message, false)?;
        }

        Ok(Self {
            project_manifest_hash: wire.project_manifest_hash,
            user_manifest_hash,
            message,
            project_sync_scope: wire.project_sync_scope,
            parent_hashes: wire.parent_hashes,
            created_at: wire.created_at,
            source: wire.source,
        })
    }
}

fn required_nullable_string(label: &str, value: &Value) -> anyhow::Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => anyhow::bail!("{label} must be a string or null"),
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
            message: None,
            project_sync_scope: ProjectSyncScope::FullProject,
            parent_hashes: vec![],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "fold_back".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(
            restored.project_manifest_hash,
            original.project_manifest_hash
        );
        assert_eq!(restored.source, "fold_back");
    }

    #[test]
    fn roundtrip_with_user_manifest() {
        let original = ProjectSnapshot {
            project_manifest_hash: "ab".repeat(32),
            user_manifest_hash: Some("cd".repeat(32)),
            message: None,
            project_sync_scope: ProjectSyncScope::FullProject,
            parent_hashes: vec!["ef".repeat(32)],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "manual_push".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(restored.user_manifest_hash, Some("cd".repeat(32)));
        assert_eq!(restored.parent_hashes.len(), 1);
    }

    #[test]
    fn rejects_non_current_or_incomplete_wire_objects() {
        let value = json!({
            "kind": "project_snapshot",
            "schema": 2,
            "project_manifest_hash": "ab".repeat(32),
            "parent_hashes": [],
            "created_at": "2026-04-23T00:00:00Z",
            "source": "old_push",
        });
        assert!(ProjectSnapshot::from_value(&value).is_err());

        let mut current = value;
        current["schema"] = json!(ProjectSnapshot::SCHEMA);
        assert!(ProjectSnapshot::from_value(&current).is_err());
        current["project_sync_scope"] = json!("full_project");
        assert!(ProjectSnapshot::from_value(&current).is_err());
        current["user_manifest_hash"] = Value::Null;
        current["message"] = Value::Null;
        assert!(ProjectSnapshot::from_value(&current).is_ok());
    }

    #[test]
    fn rejects_malformed_parent_entries_instead_of_dropping_them() {
        let value = json!({
            "kind": "project_snapshot",
            "schema": ProjectSnapshot::SCHEMA,
            "project_manifest_hash": "ab".repeat(32),
            "user_manifest_hash": null,
            "message": null,
            "project_sync_scope": "full_project",
            "parent_hashes": ["cd".repeat(32), 7],
            "created_at": "2026-04-23T00:00:00Z",
            "source": "manual_push",
        });
        assert!(ProjectSnapshot::from_value(&value).is_err());
    }

    #[test]
    fn ai_only_scope_roundtrips() {
        let original = ProjectSnapshot {
            project_manifest_hash: "ab".repeat(32),
            user_manifest_hash: None,
            message: None,
            project_sync_scope: ProjectSyncScope::AiOnly,
            parent_hashes: vec![],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "remote_ai_sync".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(restored.project_sync_scope, ProjectSyncScope::AiOnly);
    }

    #[test]
    fn roundtrip_with_message() {
        let original = ProjectSnapshot {
            project_manifest_hash: "ab".repeat(32),
            user_manifest_hash: None,
            message: Some("Update deployment workflow".to_string()),
            project_sync_scope: ProjectSyncScope::FullProject,
            parent_hashes: vec![],
            created_at: "2026-04-23T00:00:00Z".to_string(),
            source: "snapshot_create".to_string(),
        };
        let value = original.to_value();
        let restored = ProjectSnapshot::from_value(&value).unwrap();
        assert_eq!(
            restored.message,
            Some("Update deployment workflow".to_string())
        );
    }
}
