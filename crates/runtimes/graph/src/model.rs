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

/// Top-level graph YAML shape. Two consumers parse this document:
///
/// 1. The graph runtime (this crate) parses it as the strict typed
///    `GraphFile` for walker execution.
/// 2. The daemon-side `graph_permissions` composer parses the same
///    YAML into a generic JSON `Value` to lift `permissions` into
///    `effective_caps` on the callback token.
///
/// The `permissions` field therefore lives in two parsing paths. We
/// keep it on the typed shape (rather than dropping
/// `deny_unknown_fields`) so that:
///   - the runtime is the strict gatekeeper: malformed entries
///     (non-string, etc.) hard-error here before the composer's more
///     permissive `filter_map` ever sees them.
///   - the field is propagated to `GraphDefinition.declared_permissions`
///     and surfaced by callers (logged at launch in `main.rs`), making
///     it live and verifying the runtime received the same declared
///     cap-set the daemon composed for the callback token.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphFile {
    version: String,
    category: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    config: GraphConfig,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct GraphDefinition {
    pub version: String,
    pub graph_id: String,
    /// Human/item reference for this authored execution definition.
    ///
    /// `graph_id` is a legacy human/runtime identifier. This ref is
    /// the stable conceptual bridge from a realized execution trace
    /// back to the signed portable capability that was invoked.
    pub definition_ref: String,
    /// Content identity of the signature-stripped authored definition body.
    ///
    /// This is not a trust decision by itself; it is the exact identity
    /// that runtime events, receipts, and later trace projections can
    /// use to connect consequence back to capability.
    pub definition_hash: String,
    pub file_path: Option<String>,
    pub config: GraphConfig,
    /// Permissions the graph YAML declares for itself. The daemon's
    /// `graph_permissions` composer also reads these from the same
    /// YAML to populate `effective_caps` on the callback token; the
    /// runtime side keeps them visible for traceability + parity
    /// checks (see `main.rs` launch log).
    pub declared_permissions: Vec<String>,
}

impl GraphDefinition {
    pub fn from_yaml(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        let cleaned = lillux::signature::strip_signature_lines(raw);
        let definition_hash = lillux::cas::sha256_hex(cleaned.as_bytes());
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
            definition_ref: format!("graph:{graph_id}"),
            definition_hash,
            graph_id,
            file_path: file_path.map(String::from),
            config: file.config,
            declared_permissions: file.permissions,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphResult {
    pub success: bool,
    pub graph_id: String,
    pub definition_ref: String,
    pub definition_hash: String,
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
    pub definition_ref: String,
    pub definition_hash: String,
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
        assert!(
            err.to_string().contains("cattegory"),
            "error should mention unknown field: {}",
            err
        );
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

    /// Closes the dual-parser concern: graph_permissions composer reads
    /// `permissions` from the same YAML; the runtime must propagate
    /// the declared list to GraphDefinition so callers can log/verify
    /// parity, not silently drop it on the floor.
    #[test]
    fn permissions_propagate_to_definition() {
        let yaml = r#"
version: "1.0.0"
category: test
permissions:
  - ryeos.execute.tool.echo
  - ryeos.execute.tool.read
config:
  start: a
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        assert_eq!(
            def.declared_permissions,
            vec![
                "ryeos.execute.tool.echo".to_string(),
                "ryeos.execute.tool.read".to_string(),
            ]
        );
    }

    /// Structural-shape check on `permissions`: the typed
    /// `Vec<String>` rejects entries that aren't scalar (arrays,
    /// mappings, etc.), so a graph YAML the runtime accepts cannot
    /// hand the composer a structurally-broken permissions list.
    /// (Note: serde_yaml will coerce bare YAML scalars like `42` /
    /// `true` to their string form, so the composer's per-entry
    /// `as_str` filter is still the cap-shape gate for those — the
    /// runtime is the *structural* gatekeeper, not a string-only
    /// gate.)
    #[test]
    fn structural_non_scalar_permissions_rejected_by_runtime() {
        let yaml = r#"
version: "1.0.0"
category: test
permissions:
  - ryeos.execute.tool.echo
  - [nested, array]
config:
  start: a
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }

    /// `permissions` is optional — graphs without a declared cap-set
    /// still parse and yield an empty `declared_permissions`.
    #[test]
    fn missing_permissions_yields_empty_declared() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        assert!(def.declared_permissions.is_empty());
    }

    #[test]
    fn definition_identity_uses_signature_stripped_body() {
        let yaml = r#"<!-- ryeos:signed:old -->
version: "1.0.0"
category: test
config:
  start: a
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        let cleaned = lillux::signature::strip_signature_lines(yaml);
        assert_eq!(def.definition_ref, "graph:test/test");
        assert_eq!(
            def.definition_hash,
            lillux::cas::sha256_hex(cleaned.as_bytes())
        );
    }

    #[test]
    fn definition_hash_ignores_signature_line_changes_but_not_body() {
        let body = r#"version: "1.0.0"
category: test
config:
  start: a
"#;

        let signed_a = format!("<!-- ryeos:signed:old -->\n{body}");
        let signed_b = format!("<!-- ryeos:signed:new -->\n{body}");
        let changed_body = r#"version: "1.0.0"
category: test
config:
  start: b
"#;
        let signed_changed = format!("<!-- ryeos:signed:new -->\n{changed_body}");

        let a = GraphDefinition::from_yaml(&signed_a, Some("test.yaml")).unwrap();
        let b = GraphDefinition::from_yaml(&signed_b, Some("test.yaml")).unwrap();
        let changed = GraphDefinition::from_yaml(&signed_changed, Some("test.yaml")).unwrap();

        assert_eq!(a.definition_hash, b.definition_hash);
        assert_ne!(a.definition_hash, changed.definition_hash);
    }

    #[test]
    fn empty_category_uses_file_stem_without_leading_slash() {
        let yaml = r#"
version: "1.0.0"
category: ""
config:
  start: a
"#;

        let def = GraphDefinition::from_yaml(yaml, Some("/tmp/flow.yaml")).unwrap();

        assert_eq!(def.graph_id, "flow");
        assert_eq!(def.definition_ref, "graph:flow");
        assert!(!def.graph_id.starts_with('/'));
    }
}
