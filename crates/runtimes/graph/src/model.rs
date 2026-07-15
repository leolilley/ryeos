use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::events::RuntimeEventType;

/// Default total node-transition budget for one graph run.
pub(crate) const DEFAULT_GRAPH_MAX_STEPS: u32 = 100;
/// One graph step publishes one durable node receipt. Keep the authored hard
/// ceiling below the per-thread artifact collection ceiling, leaving room for
/// terminal transcript/output artifacts as well.
pub const MAX_GRAPH_STEPS: u32 = 500;
/// A continuation segment cannot exceed the graph's cumulative transition
/// ceiling. Keeping one limit avoids admitting a segment shape the full run
/// can never execute.
pub(crate) const MAX_GRAPH_SEGMENT_STEPS: u32 = MAX_GRAPH_STEPS;

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
    /// Authored observer hooks fired at graph lifecycle events
    /// (`graph_started`, `graph_step_completed`, `graph_completed`). Typed with
    /// the same `HookDefinition` vocabulary directives use — one hook grammar
    /// across runtimes. Each matching hook's action dispatches through the same
    /// callback path a node action uses (effective_caps enforced, cost accrued,
    /// braid-visible). Hooks observe; they do not steer the walk.
    #[serde(default)]
    pub hooks: Vec<ryeos_runtime::HookDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_state"
    )]
    pub state: Option<Value>,
    /// Per-thread step budget. When set and a run reaches it without hitting a
    /// terminal node, the walker checkpoints and cuts a machine continuation
    /// successor (which resumes mid-graph in a fresh thread) instead of running
    /// to `max_steps`. `step` stays cumulative across the chain; `max_steps`
    /// remains the hard total ceiling. `None` = no segmentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment_steps: Option<u32>,
}

fn default_max_steps() -> u32 {
    DEFAULT_GRAPH_MAX_STEPS
}

fn deserialize_state<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_object() {
        Ok(Some(value))
    } else {
        Err(serde::de::Error::custom(
            "`config.state` must be a mapping; omit the field when no initial state is needed",
        ))
    }
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_assign"
    )]
    pub assign: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<EdgeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<String>,
    #[serde(default)]
    pub cache_result: bool,
    /// FOLLOW node: instead of dispatching the action inline, the daemon launches
    /// it as a detached child and suspends this graph until the child's whole
    /// continuation chain reaches terminal, then resumes with its result. Only
    /// valid on an action node, and never cacheable (the result does not exist at
    /// suspend time). Validated in `validation.rs`.
    #[serde(default)]
    pub follow: bool,
    /// DETACH node: launch the action as a detached, lineage-linked child
    /// (fire-and-forget) and CONTINUE — unlike `follow`, the graph does not
    /// suspend or wait for the child's result. The node's result is the spawned
    /// `{child_thread_id}`. The child is lineage-linked (a cancel/kill cascade
    /// reaches it, it appears in `threads.children`) and inherits the parent's
    /// depth+1 and hard limits. With `over:`, this is a lineage-preserving fanout
    /// — the fleet fix. Only valid on an action node; mutually exclusive with
    /// `follow`; never cacheable. Validated in `validation.rs`.
    #[serde(default)]
    pub detach: bool,
    /// Cohort/fleet tags stamped on a `detach` child at spawn — a map of
    /// `key: "<template>"` rendered per iteration
    /// (e.g. `{fleet: "${_run.graph_run_id}", game: "${item}"}`), so a detached child
    /// is tagged by construction with no post-launch race. Ignored without
    /// `detach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<String>,
    #[serde(default)]
    pub parallel: bool,
    /// Fanout width. On a plain foreach this bounds concurrent dispatch
    /// tasks. On a `detach: true` foreach it is the LAUNCH WINDOW: detached
    /// spawns return immediately, so the daemon keeps at most this many
    /// child chains launched-and-live at once and admits the next queued
    /// child when a live one reaches a hard terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    /// Return-node output template. A YAML scalar deserializes to a
    /// `Value::String` and a YAML map/list to `Value::Object`/`Array`; the
    /// compiled rye-expr/1 template tree recursively preserves native
    /// whole-expression values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
    /// Per-step dispatch retry. When a dispatch fails and attempts remain, the
    /// walker sleeps the backoff and re-dispatches — each attempt consuming a
    /// walker step and the attempt count riding the checkpoint. Only valid on
    /// action nodes (incl. foreach); rejected on `follow` nodes. Validated in
    /// `validation.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryConfig>,
}

fn deserialize_assign<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_object() {
        Ok(Some(value))
    } else {
        Err(serde::de::Error::custom(
            "`assign` must be a mapping; omit the field when no assignment is needed",
        ))
    }
}

impl GraphNode {
    pub fn is_cacheable(&self) -> bool {
        self.cache_result
    }

    pub fn foreach_var(&self) -> &str {
        self.r#as.as_deref().unwrap_or("item")
    }

    /// Fold the node's `detach` dispatch mode into a cloned action: set
    /// `thread: "detached"` and carry the node's `facets:` templates so they
    /// render alongside the action (per iteration under `over:`, with the
    /// item variable in scope) and the daemon stamps them on the child at
    /// spawn. No-op unless `detach: true`.
    ///
    /// Every action clone site that dispatches (plain action node, sequential
    /// foreach, parallel foreach) must route through this fold BEFORE
    /// compilation — `dispatch_action` defaults a missing `thread` to
    /// `"inline"`, and the callback boundary rejects an inline dispatch of a
    /// thread-run kind, so a site that skips the fold fails the node.
    pub fn fold_detach_into_action(&self, action: &mut Value) {
        if !self.detach {
            return;
        }
        if let Some(obj) = action.as_object_mut() {
            obj.insert(
                ryeos_runtime::callback::action_keys::THREAD.to_string(),
                Value::String("detached".to_string()),
            );
            if let Some(facets) = &self.facets {
                obj.insert(
                    ryeos_runtime::callback::action_keys::FACETS.to_string(),
                    facets.clone(),
                );
            }
        }
    }
}

/// Maximum authored delay for any one graph retry backoff (five minutes).
pub const MAX_RETRY_BACKOFF_MS: u64 = 300_000;

/// Per-step retry policy on an action node.
///
/// `attempts` is the TOTAL number of dispatches including the first, so
/// `attempts: 3` means one initial dispatch plus up to two retries. The
/// backoff before the retry that follows a failed attempt `n` (1-based) is
/// `backoff_ms * 2^(n-1)`, capped at `max_backoff_ms` when set. Bounds
/// (`attempts` 1..=10, delay fields within `MAX_RETRY_BACKOFF_MS`, and
/// `max_backoff_ms` >= `backoff_ms`)
/// are enforced in `validation.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_backoff_ms: Option<u64>,
}

impl RetryConfig {
    /// Backoff before the retry that follows `failed_attempt` (1-based: the
    /// number of the attempt that just failed). Exponential, capped.
    pub fn delay_ms(&self, failed_attempt: u32) -> u64 {
        // `failed_attempt` is validated to be at least 1; the shift exponent is
        // clamped so a pathological attempt count can never overflow the shift.
        let exp = failed_attempt.saturating_sub(1).min(63);
        let grown = self.backoff_ms.saturating_mul(1u64 << exp);
        let authored = match self.max_backoff_ms {
            Some(cap) => grown.min(cap),
            None => grown,
        };
        // Defense in depth for programmatically constructed definitions that
        // have not passed graph validation.
        authored.min(MAX_RETRY_BACKOFF_MS)
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
    #[serde(
        default,
        skip_serializing_if = "ryeos_runtime::ExpressionCondition::is_absent"
    )]
    pub when: ryeos_runtime::ExpressionCondition,
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
    /// Immutable execution sidecar compiled once from the strict source shape.
    /// The walker never parses expressions or scans templates at runtime.
    pub(crate) compiled: crate::compiled_graph::CompiledGraph,
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
        Self::from_yaml_with_hook_sources(
            raw,
            file_path,
            ryeos_runtime::HookSources::default(),
        )
    }

    pub fn from_yaml_with_hook_sources(
        raw: &str,
        file_path: Option<&str>,
        hook_sources: ryeos_runtime::HookSources,
    ) -> anyhow::Result<Self> {
        Self::from_yaml_with_identity(raw, file_path, hook_sources, None)
    }

    /// Build the runtime definition from the exact bytes and canonical item
    /// identity carried by a verified launch envelope.
    pub fn from_verified_yaml_with_hook_sources(
        raw: &str,
        file_path: Option<&str>,
        resolved_ref: &str,
        expected_digest: &str,
        hook_sources: ryeos_runtime::HookSources,
    ) -> anyhow::Result<Self> {
        Self::from_yaml_with_identity(
            raw,
            file_path,
            hook_sources,
            Some((resolved_ref, expected_digest)),
        )
    }

    fn from_yaml_with_identity(
        raw: &str,
        file_path: Option<&str>,
        mut hook_sources: ryeos_runtime::HookSources,
        verified_identity: Option<(&str, &str)>,
    ) -> anyhow::Result<Self> {
        // Verified launch envelopes already carry the exact, envelope-aware
        // signature-stripped bytes whose digest was checked by the engine.
        // Never run the generic line stripper over them again: authored YAML
        // scalar content may legitimately contain `ryeos:signed:`.
        let definition_content: std::borrow::Cow<'_, str> = if verified_identity.is_some() {
            std::borrow::Cow::Borrowed(raw)
        } else {
            std::borrow::Cow::Owned(lillux::signature::strip_signature_lines(raw))
        };
        let definition_hash = lillux::cas::sha256_hex(definition_content.as_bytes());
        if let Some((_, expected_digest)) = verified_identity {
            if definition_hash != expected_digest {
                anyhow::bail!(
                    "verified graph content digest mismatch: envelope={expected_digest}, runtime={definition_hash}"
                );
            }
        }
        let mut file: GraphFile = serde_yaml::from_str(definition_content.as_ref())?;
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
        hook_sources.authored = std::mem::take(&mut file.config.hooks);
        let compiled = crate::compiled_graph::CompiledGraph::compile(&file.config, hook_sources)?;
        let (graph_id, definition_ref) = if let Some((resolved_ref, _)) = verified_identity {
            let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(resolved_ref)
                .map_err(|error| anyhow::anyhow!("invalid resolved graph ref `{resolved_ref}`: {error}"))?;
            if canonical.kind != "graph" {
                anyhow::bail!(
                    "resolved graph definition must have kind `graph`, got `{}`",
                    canonical.kind
                );
            }
            (canonical.bare_id, resolved_ref.to_string())
        } else if let Some(fp) = file_path {
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
                (stem.to_string(), format!("graph:{stem}"))
            } else {
                let graph_id = format!("{}/{}", file.category, stem);
                let definition_ref = format!("graph:{graph_id}");
                (graph_id, definition_ref)
            }
        } else if file.category.is_empty() {
            ("unknown".to_string(), "graph:unknown".to_string())
        } else {
            let graph_id = file.category;
            let definition_ref = format!("graph:{graph_id}");
            (graph_id, definition_ref)
        };
        Ok(Self {
            version: file.version,
            definition_ref,
            definition_hash,
            graph_id,
            file_path: file_path.map(String::from),
            config: file.config,
            compiled,
            declared_permissions,
            runtime_capability_requirements,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphRunStatus {
    Valid,
    Invalid,
    Completed,
    CompletedWithErrors,
    Continued,
    Error,
    MaxStepsExceeded,
    Cancelled,
    Killed,
}

impl GraphRunStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Completed => "completed",
            Self::CompletedWithErrors => "completed_with_errors",
            Self::Continued => "continued",
            Self::Error => "error",
            Self::MaxStepsExceeded => "max_steps_exceeded",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphStepStatus {
    Ok,
    Error,
    Retry,
}

impl GraphStepStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Retry => "retry",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphToolCallStatus {
    Ok,
    Error,
    ExpressionFailed,
    IntegrityFailed,
    DispatchFailed,
}

impl GraphToolCallStatus {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::ExpressionFailed => "expression_failed",
            Self::IntegrityFailed => "integrity_failed",
            Self::DispatchFailed => "dispatch_failed",
        }
    }
}

pub use ryeos_runtime::checkpoint::FanoutItemStatus;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphResult {
    pub success: bool,
    pub graph_id: String,
    pub definition_ref: String,
    pub definition_hash: String,
    pub graph_run_id: String,
    pub status: GraphRunStatus,
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
    /// Cost incurred by observer hooks, retained separately from node actions
    /// while still contributing to the graph's aggregate `cost`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hook_costs: Vec<HookCostRecord>,
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
pub struct HookCostRecord {
    pub event: RuntimeEventType,
    /// Present for step/completion events and absent for `graph_started`.
    pub step: Option<u32>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fanout: Option<FanoutReceiptSummary>,
}

/// Dispatch observations held behind the node's expression fence. The child
/// already exists, but its lineage/milestone events are not published by the
/// execution phase; commit emits them after assignment and branch evaluation
/// settle, including when that later expression fails.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DispatchObservation {
    pub(crate) item_id: String,
    pub(crate) child_thread_id: Option<String>,
    pub(crate) milestones: Vec<Value>,
}

impl DispatchObservation {
    pub(crate) fn child_only(
        item_id: impl Into<String>,
        child_thread_id: Option<String>,
    ) -> Option<Self> {
        child_thread_id.map(|child_thread_id| Self {
            item_id: item_id.into(),
            child_thread_id: Some(child_thread_id),
            milestones: Vec::new(),
        })
    }

    pub(crate) fn from_success(
        item_id: impl Into<String>,
        child_thread_id: Option<String>,
        result: &Value,
    ) -> Option<Self> {
        let milestones = result
            .get("milestones")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if child_thread_id.is_none() && milestones.is_empty() {
            None
        } else {
            Some(Self {
                item_id: item_id.into(),
                child_thread_id,
                milestones,
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanoutReceiptSummary {
    pub statuses: Vec<FanoutItemStatus>,
    pub failed: usize,
    pub expected: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<Value>>,
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
      runtime_authority:
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
      runtime_authority:
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
      runtime_authority:
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
    fn verified_definition_uses_envelope_ref_and_digest() {
        let yaml = r#"
version: "1.0.0"
category: self_asserted
config:
  start: a
"#;
        let digest = lillux::cas::sha256_hex(yaml.as_bytes());
        let definition = GraphDefinition::from_verified_yaml_with_hook_sources(
            yaml,
            Some("/diagnostic/wrong-name.yaml"),
            "graph:canonical/identity",
            &digest,
            ryeos_runtime::HookSources::default(),
        )
        .unwrap();

        assert_eq!(definition.graph_id, "canonical/identity");
        assert_eq!(definition.definition_ref, "graph:canonical/identity");
        assert_eq!(definition.definition_hash, digest);
    }

    #[test]
    fn verified_definition_preserves_authored_signature_marker_text() {
        let yaml = r#"
version: "1.0.0"
category: self_asserted
description: "literal ryeos:signed: marker"
config:
  start: a
"#;
        let digest = lillux::cas::sha256_hex(yaml.as_bytes());
        let definition = GraphDefinition::from_verified_yaml_with_hook_sources(
            yaml,
            Some("/diagnostic/wrong-name.yaml"),
            "graph:canonical/identity",
            &digest,
            ryeos_runtime::HookSources::default(),
        )
        .unwrap();

        assert_eq!(definition.definition_hash, digest);
    }

    #[test]
    fn verified_definition_rejects_digest_mismatch() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: a
"#;
        let error = GraphDefinition::from_verified_yaml_with_hook_sources(
            yaml,
            Some("test.yaml"),
            "graph:test/item",
            &"0".repeat(64),
            ryeos_runtime::HookSources::default(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("content digest mismatch"));
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
    fn retry_delay_is_exponential_and_capped() {
        let rc = RetryConfig {
            attempts: 5,
            backoff_ms: 100,
            max_backoff_ms: Some(500),
        };
        // failed_attempt 1 → 100 * 2^0, 2 → 200, 3 → 400, 4 → 800 capped to 500.
        assert_eq!(rc.delay_ms(1), 100);
        assert_eq!(rc.delay_ms(2), 200);
        assert_eq!(rc.delay_ms(3), 400);
        assert_eq!(rc.delay_ms(4), 500, "capped at max_backoff_ms");
        assert_eq!(rc.delay_ms(10), 500, "still capped");
    }

    #[test]
    fn retry_delay_uncapped_when_no_max() {
        let rc = RetryConfig {
            attempts: 3,
            backoff_ms: 250,
            max_backoff_ms: None,
        };
        assert_eq!(rc.delay_ms(1), 250);
        assert_eq!(rc.delay_ms(3), 1000);
    }

    #[test]
    fn retry_delay_is_defensively_bounded_without_validation() {
        let rc = RetryConfig {
            attempts: 2,
            backoff_ms: u64::MAX,
            max_backoff_ms: None,
        };
        assert_eq!(rc.delay_ms(1), MAX_RETRY_BACKOFF_MS);
    }

    #[test]
    fn retry_block_parses_on_action_node() {
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      action: {item_id: "tool:test/fetch", ref_bindings: {}}
      retry: {attempts: 3, backoff_ms: 1000, max_backoff_ms: 30000}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        let retry = def.config.nodes["fetch"]
            .retry
            .as_ref()
            .expect("retry parsed");
        assert_eq!(retry.attempts, 3);
        assert_eq!(retry.backoff_ms, 1000);
        assert_eq!(retry.max_backoff_ms, Some(30000));
    }

    #[test]
    fn retry_block_rejects_unknown_field() {
        // deny_unknown_fields: a typo'd retry key fails the parse rather than
        // being silently ignored.
        let yaml = r#"
version: "1.0.0"
category: test
config:
  start: fetch
  nodes:
    fetch:
      action: {item_id: "tool:test/fetch", ref_bindings: {}}
      retry: {attempts: 3, backoff_ms: 1000, backof_max: 30000}
      next: {type: unconditional, to: done}
    done:
      node_type: return
"#;
        assert!(GraphDefinition::from_yaml(yaml, Some("test.yaml")).is_err());
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

    #[test]
    fn initial_state_must_be_an_authored_mapping() {
        for state in ["null", "[]", "1", "\"not-a-state-map\""] {
            let yaml = format!(
                r#"
version: "1"
category: test
config:
  start: done
  state: {state}
  nodes:
    done: {{node_type: return}}
"#
            );
            let error = GraphDefinition::from_yaml(&yaml, Some("test.yaml"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("config.state"), "unexpected error: {error}");
        }
    }

    #[test]
    fn assign_must_be_an_authored_mapping() {
        for assign in ["null", "[]", "1", "\"state.value\""] {
            let yaml = format!(
                r#"
version: "1"
category: test
config:
  start: step
  nodes:
    step:
      action: {{item_id: "tool:test/noop"}}
      assign: {assign}
"#
            );
            let error = GraphDefinition::from_yaml(&yaml, Some("test.yaml"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("assign"), "unexpected error: {error}");
        }
    }

    #[test]
    fn graph_load_compiles_templates_and_rejects_unknown_roots() {
        let yaml = r#"
version: "1"
category: test
config:
  start: step
  nodes:
    step:
      action:
        item_id: tool:test/noop
        params: {secret: "${secrets.token}"}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("secrets"), "unexpected error: {error}");
        assert!(error.contains("allowed roots"), "unexpected error: {error}");
    }

    #[test]
    fn gate_condition_cannot_reference_result() {
        let yaml = r#"
version: "1"
category: test
config:
  start: gate
  nodes:
    gate:
      node_type: gate
      next:
        type: conditional
        branches:
          - when: result.ok
            to: done
          - to: done
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("result"), "unexpected error: {error}");
    }

    #[test]
    fn action_condition_can_reference_result_and_candidate_state() {
        let yaml = r#"
version: "1"
category: test
config:
  start: step
  nodes:
    step:
      action: {item_id: tool:test/noop}
      assign: {ready: "${result.ok}"}
      next:
        type: conditional
        branches:
          - when: result.ok && state.ready
            to: done
          - to: done
    done: {node_type: return}
"#;
        GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
    }

    #[test]
    fn static_input_reference_must_be_declared_by_schema() {
        let yaml = r#"
version: "1"
category: test
config:
  start: step
  config_schema:
    type: object
    properties:
      declared: {type: string}
  nodes:
    step:
      action:
        item_id: tool:test/noop
        params: {value: "${inputs.missing}"}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("inputs.missing") || error.contains("input `missing`"));
        assert!(error.contains("config_schema.properties"));
    }

    #[test]
    fn foreach_variable_must_be_portable_and_unreserved() {
        for variable in ["bad-name", "state", "true"] {
            let yaml = format!(
                r#"
version: "1"
category: test
config:
  start: each
  nodes:
    each:
      node_type: foreach
      over: "${{state.items}}"
      as: {variable}
      action: {{item_id: tool:test/noop}}
"#
            );
            let error = GraphDefinition::from_yaml(&yaml, Some("test.yaml"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("iteration variable"), "unexpected error: {error}");
        }
    }

    #[test]
    fn conditional_edge_rejects_multiple_defaults() {
        let yaml = r#"
version: "1"
category: test
config:
  start: gate
  nodes:
    gate:
      node_type: gate
      next:
        type: conditional
        branches:
          - {to: done}
          - {to: done}
    done: {node_type: return}
"#;
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than one default"), "unexpected error: {error}");
    }
}
