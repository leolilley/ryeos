use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_runtime::envelope::RuntimeCost;

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
    /// Per-thread step budget. When set and a run reaches it without hitting a
    /// terminal node, the walker checkpoints and cuts a machine continuation
    /// successor (which resumes mid-graph in a fresh thread) instead of running
    /// to `max_steps`. `step` stays cumulative across the chain; `max_steps`
    /// remains the hard total ceiling. `None` = no segmentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_steps: Option<u32>,
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
    /// FOLLOW node: instead of dispatching the action inline, the daemon launches
    /// it as a detached child and suspends this graph until the child's whole
    /// continuation chain reaches terminal, then resumes with its result. Only
    /// valid on an action node, and never cacheable (the result does not exist at
    /// suspend time). Validated in `validation.rs`.
    #[serde(default)]
    pub follow: bool,
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
    /// Return-node output template. A YAML scalar deserializes to a
    /// `Value::String` and a YAML map/list to `Value::Object`/`Array`;
    /// `interpolate` recurses through all of them, so both
    /// `output: "${state.x}"` and `output: {id: "${state.id}"}` work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
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
///    YAML into a generic JSON `Value` to lift
///    `requires.capabilities.declared` into `effective_caps` on the
///    callback token.
///
/// `requires` therefore lives in two parsing paths. We keep it on the
/// typed shape (rather than dropping `deny_unknown_fields`) so that:
///   - the runtime is the strict gatekeeper: malformed entries
///     (unknown keys, bad operations) hard-error here before the
///     composer's more permissive read ever sees them.
///   - the declared list is propagated to
///     `GraphDefinition.declared_permissions` and surfaced by callers
///     (logged at launch in `main.rs`), making it live and verifying the
///     runtime received the same declared cap-set the daemon composed
///     for the callback token.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphFile {
    version: String,
    category: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    config: GraphConfig,
    /// Unified capability requirements (`requires.capabilities`): `declared`
    /// (self-asserted action authority, composed into effective_caps) and
    /// `manifest` (runtime callback authority minted from the signed manifest).
    /// `deny_unknown_fields` makes a legacy top-level `permissions:` fail to
    /// parse — there is no back-compat path.
    #[serde(default)]
    requires: Option<ryeos_bundle::runtime_authority::RuntimeRequires>,
}

#[derive(Debug, Clone)]
pub struct GraphDefinition {
    pub version: String,
    pub graph_id: String,
    /// Human/item reference for this authored execution definition.
    ///
    /// `graph_id` is the human/runtime identifier. This ref is the
    /// stable conceptual bridge from a realized execution trace back to
    /// the signed portable capability that was invoked.
    pub definition_ref: String,
    /// Content identity of the signature-stripped authored definition body.
    ///
    /// This is not a trust decision by itself; it is the exact identity
    /// that runtime events, receipts, and later trace projections can
    /// use to connect consequence back to capability.
    pub definition_hash: String,
    pub file_path: Option<String>,
    pub config: GraphConfig,
    /// Self-asserted action authority the graph declares for itself
    /// (`requires.capabilities.declared`). The daemon's
    /// `graph_permissions` composer reads the same path to populate
    /// `effective_caps` on the callback token; the runtime side keeps it
    /// visible for traceability + parity checks (see `main.rs` launch log).
    pub declared_permissions: Vec<String>,
    /// Structured runtime capability requirements declared by the graph
    /// (`requires.capabilities`). The daemon is the authority: it mints the
    /// requested manifest-backed subset into the callback token at launch.
    /// Retained here for traceability and static validation parity.
    pub runtime_capability_requirements:
        Option<ryeos_bundle::runtime_authority::RuntimeCapabilityRequirements>,
}

impl GraphDefinition {
    pub fn from_yaml(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        let cleaned = lillux::signature::strip_signature_lines(raw);
        let definition_hash = lillux::cas::sha256_hex(cleaned.as_bytes());
        let file: GraphFile = serde_yaml::from_str(&cleaned)?;
        let runtime_capability_requirements = match file.requires {
            Some(requires) => {
                let caps = requires.capabilities;
                ryeos_bundle::runtime_authority::validate_runtime_capability_requirements(&caps)
                    .map_err(|e| anyhow::anyhow!("invalid `requires.capabilities`: {e}"))?;
                Some(caps)
            }
            None => None,
        };
        let declared_permissions = runtime_capability_requirements
            .as_ref()
            .map(|caps| caps.declared.clone())
            .unwrap_or_default();
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
            declared_permissions,
            runtime_capability_requirements,
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
    /// Aggregate token/spend cost across every cost-bearing node in the
    /// run. `None` when no node reported cost (e.g. a pure-tool graph).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
    /// Per-node cost breakdown, one record per cost-bearing node. Empty
    /// when no node reported cost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_costs: Vec<NodeCostRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorRecord {
    pub step: u32,
    pub node: String,
    pub error: String,
}

/// Cost attributed to a single node's action (a directive or sub-graph
/// child that reported usage). Foreach nodes aggregate all iteration
/// costs into one record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCostRecord {
    pub node: String,
    pub step: u32,
    pub item_id: String,
    pub cost: RuntimeCost,
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
    /// Cost reported by this node's native child, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
}

pub struct WalkContext {
    pub state: Value,
    pub inputs: Value,
    pub result: Option<Value>,
    pub execution: Option<Value>,
}

impl WalkContext {
    pub fn as_context(&self) -> Value {
        let mut ctx = serde_json::Map::new();
        ctx.insert("state".into(), self.state.clone());
        ctx.insert("inputs".into(), self.inputs.clone());
        if let Some(ref r) = self.result {
            ctx.insert("result".into(), r.clone());
        }
        if let Some(ref execution) = self.execution {
            ctx.insert("_execution".into(), execution.clone());
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

    /// `requires.capabilities.declared` propagates to `declared_permissions` —
    /// the same path the `graph_permissions` composer lifts into
    /// `effective_caps`, so the runtime can log/verify parity.
    #[test]
    fn declared_execute_propagates_to_definition() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.echo
      - ryeos.execute.tool.read
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

    /// No back-compat: a legacy top-level `permissions:` block fails the strict
    /// `deny_unknown_fields` parse rather than being silently ignored.
    #[test]
    fn legacy_top_level_permissions_rejected() {
        let yaml = r#"
version: "1.0.0"
category: test
permissions:
  - ryeos.execute.tool.echo
config:
  start: a
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }

    /// A graph without `requires:` parses and yields an empty
    /// `declared_permissions`.
    #[test]
    fn missing_requires_yields_empty_declared() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        assert!(def.declared_permissions.is_empty());
        assert!(def.runtime_capability_requirements.is_none());
    }

    /// `requires.capabilities.manifest` parses into the structured requirement
    /// field, separate from `declared`.
    #[test]
    fn requires_capabilities_parse_into_definition() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
requires:
  capabilities:
    declared:
      - ryeos.execute.tool.echo
    manifest:
      bundle_events:
        - event_kind: arc_pattern_event
          operations: [append]
      runtime_vault:
        - namespace: oauth
          operations: [get]
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        assert_eq!(
            def.declared_permissions,
            vec!["ryeos.execute.tool.echo".to_string()]
        );
        let reqs = def
            .runtime_capability_requirements
            .expect("requirements parsed");
        let caps = ryeos_bundle::runtime_authority::requested_runtime_caps(&reqs, "arc");
        assert_eq!(
            caps.into_iter().collect::<Vec<_>>(),
            vec![
                "ryeos.append.bundle-events.arc/arc_pattern_event".to_string(),
                "ryeos.get.vault.arc/oauth".to_string(),
            ]
        );
    }

    /// Static validation runs at parse time: empty operation lists are
    /// rejected by the runtime before the daemon ever sees the graph.
    #[test]
    fn requires_with_empty_operations_rejected() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
requires:
  capabilities:
    manifest:
      bundle_events:
        - event_kind: arc_pattern_event
          operations: []
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
    }

    /// Unknown keys under `requires` fail the strict typed parse.
    #[test]
    fn requires_with_unknown_key_rejected() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
requires:
  capabilities:
    manifest:
      bundle_events:
        - event_kind: arc_pattern_event
          operations: [append]
          extra: nope
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
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
