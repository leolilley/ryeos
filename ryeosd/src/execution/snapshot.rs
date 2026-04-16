//! Execution snapshot and runtime outputs CAS objects.
//!
//! An `ExecutionSnapshot` captures the pre-execution state (project +
//! user manifests, thread info). A `RuntimeOutputsBundle` captures the
//! post-execution outputs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cas::CasStore;

/// Pre-execution snapshot stored in CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    pub thread_id: String,
    pub project_manifest_hash: String,
    pub user_manifest_hash: Option<String>,
    pub item_ref: String,
    pub parameters: Option<Value>,
    pub created_at: String,
}

impl ExecutionSnapshot {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "kind": "execution_snapshot",
            "thread_id": self.thread_id,
            "project_manifest_hash": self.project_manifest_hash,
            "item_ref": self.item_ref,
            "created_at": self.created_at,
        });
        let m = v.as_object_mut().unwrap();
        if let Some(ref umh) = self.user_manifest_hash {
            m.insert("user_manifest_hash".into(), json!(umh));
        }
        if let Some(ref params) = self.parameters {
            m.insert("parameters".into(), params.clone());
        }
        v
    }

    pub fn store(&self, cas: &CasStore) -> Result<String> {
        cas.store_object(&self.to_json())
    }
}

/// Post-execution outputs bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeOutputsBundle {
    pub thread_id: String,
    pub execution_snapshot_hash: String,
    pub output_manifest_hash: Option<String>,
    pub artifacts: Vec<ArtifactEntry>,
    pub status: String,
    pub created_at: String,
}

/// A single artifact produced by execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub artifact_type: String,
    pub blob_hash: String,
    pub path: Option<String>,
    pub metadata: Option<Value>,
}

impl RuntimeOutputsBundle {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": "runtime_outputs_bundle",
            "thread_id": self.thread_id,
            "execution_snapshot_hash": self.execution_snapshot_hash,
            "output_manifest_hash": self.output_manifest_hash,
            "artifacts": self.artifacts,
            "status": self.status,
            "created_at": self.created_at,
        })
    }

    pub fn store(&self, cas: &CasStore) -> Result<String> {
        cas.store_object(&self.to_json())
    }
}
