use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSource {
    pub item_ref: String,
    pub content_blob_hash: String,
    pub integrity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_info: Option<Value>,
}

impl ItemSource {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "kind": "item_source",
            "item_ref": self.item_ref,
            "content_blob_hash": self.content_blob_hash,
            "integrity": self.integrity,
        });
        if let Some(ref sig) = self.signature_info {
            v.as_object_mut()
                .unwrap()
                .insert("signature_info".into(), sig.clone());
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifest {
    pub item_source_hashes: HashMap<String, String>,
}

impl SourceManifest {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": "source_manifest",
            "item_source_hashes": self.item_source_hashes,
        })
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        let item_source_hashes = value
            .get("item_source_hashes")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("missing item_source_hashes"))?
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
        Ok(Self { item_source_hashes })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub project_manifest_hash: String,
    pub user_manifest_hash: Option<String>,
    pub parent_hashes: Vec<String>,
    pub created_at: String,
    pub source: String,
}

impl ProjectSnapshot {
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "kind": "project_snapshot",
            "schema": 2,
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

    pub fn from_json(value: &Value) -> Result<Self> {
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
