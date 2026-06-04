use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRouteSpec {
    #[serde(default)]
    pub section: String,
    /// Metadata field from signed YAML — ignored at runtime.
    #[serde(default, skip_serializing)]
    pub category: Option<String>,
    pub id: String,
    pub path: String,
    pub methods: HashSet<String>,
    pub auth: String,
    #[serde(default)]
    pub auth_config: Option<Value>,
    #[serde(default)]
    pub limits: RawLimits,
    pub response: RawResponseSpec,
    #[serde(default)]
    pub execute: Option<RawExecute>,
    #[serde(default)]
    pub request: RawRequest,
    #[serde(default)]
    pub source_file: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLimits {
    #[serde(default = "default_body_max")]
    pub body_bytes_max: u64,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_concurrent_max")]
    pub concurrent_max: u32,
}

fn default_body_max() -> u64 {
    1_048_576
}
fn default_timeout() -> u64 {
    30_000
}
fn default_concurrent_max() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawResponseSpec {
    pub mode: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_config: Value,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub body_b64: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExecute {
    pub item_ref: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRequest {
    #[serde(default)]
    pub body: RawRequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RawRequestBody {
    #[default]
    None,
    Raw,
    Text,
    Json,
}

impl Default for RawRequest {
    fn default() -> Self {
        Self {
            body: RawRequestBody::None,
        }
    }
}

impl Default for RawLimits {
    fn default() -> Self {
        Self {
            body_bytes_max: default_body_max(),
            timeout_ms: default_timeout(),
            concurrent_max: default_concurrent_max(),
        }
    }
}
