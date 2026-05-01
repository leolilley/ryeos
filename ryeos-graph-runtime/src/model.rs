use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    pub start: String,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub on_error: ErrorMode,
    #[serde(default)]
    pub nodes: HashMap<String, GraphNode>,
    #[serde(default)]
    pub hooks: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
}

fn default_max_steps() -> u32 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[derive(Default)]
pub enum ErrorMode {
    #[default]
    Fail,
    Continue,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    #[serde(default)]
    pub node_type: NodeType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assign: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<EdgeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,
    #[serde(default)]
    pub cache_result: bool,
    #[serde(default, alias = "cache")]
    pub cache: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<String>,
    #[serde(default)]
    pub parallel: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub env_requires: Vec<String>,
}

impl GraphNode {
    pub fn is_cacheable(&self) -> bool {
        self.cache_result || self.cache
    }

    pub fn foreach_var(&self) -> &str {
        self.r#as.as_deref().unwrap_or("item")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[derive(Default)]
pub enum NodeType {
    #[default]
    Action,
    Return,
    Foreach,
    Gate,
}


// NOTE: deny_unknown_fields blocked by #[serde(flatten)]/#[serde(untagged)]. Tracked in 04-FUTURE-WORK.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EdgeSpec {
    Unconditional(String),
    Conditional(Vec<ConditionalEdge>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Value>,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct GraphDefinition {
    pub version: String,
    pub graph_id: String,
    pub file_path: Option<String>,
    pub config: GraphConfig,
    /// Graph-level permissions. Consumed by the graph_permissions
    /// composer at the daemon layer to produce effective_caps. No
    /// longer read by the walker (D15).
    #[allow(dead_code)]
    pub permissions: Vec<String>,
}

impl GraphDefinition {
    pub fn from_yaml(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        let cleaned = lillux::signature::strip_signature_lines(raw);
        let doc: serde_yaml::Value = serde_yaml::from_str(&cleaned)?;
        let category = doc.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let graph_id = if let Some(fp) = file_path {
            let stem = std::path::Path::new(fp)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            format!("{category}/{stem}")
        } else {
            category.to_string()
        };
        let config: GraphConfig = serde_yaml::from_value(
            doc.get("config")
                .cloned()
                .unwrap_or(serde_yaml::Value::Null),
        )?;
        let permissions = doc
            .get("permissions")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let version = doc
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("1.0.0")
            .to_string();
        Ok(Self {
            version,
            graph_id,
            file_path: file_path.map(String::from),
            config,
            permissions,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphResult {
    pub success: bool,
    pub graph_id: String,
    pub graph_run_id: String,
    pub status: String,
    pub steps: u32,
    pub state: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors_suppressed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ErrorRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorRecord {
    pub step: u32,
    pub node: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeReceipt {
    pub node: String,
    pub step: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_hash: Option<String>,
    pub cache_hit: bool,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct WalkContext {
    pub state: Value,
    pub inputs: Value,
    pub result: Option<Value>,
}

impl WalkContext {
    pub fn as_context(&self) -> Value {
        let mut ctx = serde_json::Map::new();
        ctx.insert("state".into(), self.state.clone());
        ctx.insert("inputs".into(), self.inputs.clone());
        if let Some(ref r) = self.result {
            ctx.insert("result".into(), r.clone());
        }
        ctx.insert("_now".into(), Value::String(lillux::time::iso8601_now()));
        ctx.insert(
            "_timestamp".into(),
            Value::Number(lillux::time::timestamp_millis().into()),
        );
        Value::Object(ctx)
    }

    pub fn with_foreach_item(&self, var: &str, item: &Value) -> Value {
        let mut ctx = self.as_context();
        if let Value::Object(ref mut map) = ctx {
            map.insert(var.to_string(), item.clone());
        }
        ctx
    }
}
