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


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EdgeSpec {
    Unconditional { to: String },
    Conditional { branches: Vec<ConditionalEdge> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalEdge {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Value>,
    pub to: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphFile {
    version: String,
    category: String,
    config: GraphConfig,
    /// Accepted but unused by the walker. The graph_permissions composer
    /// (upstream, in the daemon's compose chain) reads this field from
    /// the same YAML to populate effective_caps on the callback token.
    /// Without acknowledging it here, deny_unknown_fields would reject
    /// every YAML the composer needs to consume.
    #[serde(default)]
    #[allow(dead_code)]
    permissions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GraphDefinition {
    pub version: String,
    pub graph_id: String,
    pub file_path: Option<String>,
    pub config: GraphConfig,
}

impl GraphDefinition {
    pub fn from_yaml(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        let cleaned = lillux::signature::strip_signature_lines(raw);
        let file: GraphFile = serde_yaml::from_str(&cleaned)?;
        let graph_id = if let Some(fp) = file_path {
            let stem = std::path::Path::new(fp)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            // Empty category is a legitimate value for top-level graphs;
            // joining "/{stem}" produced ids like "/flow" which then
            // broke `write_knowledge_transcript` because Path::join with
            // an absolute-looking second segment replaces the base path
            // entirely (writing to /flow/... and getting EACCES).
            if file.category.is_empty() {
                stem.to_string()
            } else {
                format!("{}/{}", file.category, stem)
            }
        } else if file.category.is_empty() {
            "unknown".to_string()
        } else {
            file.category
        };
        Ok(Self {
            version: file.version,
            graph_id,
            file_path: file_path.map(String::from),
            config: file.config,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_top_level_field_rejects() {
        let yaml = r#"
version: "1.0.0"
category: test
cattegory: typo
config:
  start: a
"#;
        let err = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        assert!(err.to_string().contains("cattegory"), "error should mention unknown field: {}", err);
    }

    #[test]
    fn missing_version_rejects() {
        let yaml = r#"
category: test
config:
  start: a
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }

    #[test]
    fn missing_category_rejects() {
        let yaml = r#"
version: "1.0.0"
config:
  start: a
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }

    #[test]
    fn missing_config_rejects() {
        let yaml = r#"
version: "1.0.0"
category: test
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }
}
