//! Launch envelope and runtime result types — single source of truth in
//! the engine crate.
//!
//! Moved from `ryeos-runtime/src/envelope.rs` so the protocol vocabulary
//! module (in this crate) can reference them without creating a dependency
//! cycle. `ryeos-runtime` re-exports these types for backward compatibility.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::resolution::ResolutionOutput;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// `ItemDescriptor` is defined in the engine (where it's produced by
// `Engine::build_inventory_for_launching_kind`) and re-exported here
// so runtimes have a single canonical import for the launch-contract
// shape.
pub use crate::inventory::ItemDescriptor;

/// Envelope shipped from daemon to runtime over stdin.
///
/// `EnvelopeTarget` has been deleted: the daemon used to embed a second
/// snapshot of the root item (path + digest) computed from
/// `resolved.resolved_item`, while `resolution.root` shipped a *third*
/// snapshot loaded inside `run_resolution_pipeline`. They could disagree,
/// and the runtime would have no way to tell which to trust. Now there
/// is exactly one root snapshot — `resolution.root` — and every consumer
/// reads `path` / `digest` / `kind` / `item_id` from there.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchEnvelope {
    pub invocation_id: String,
    pub thread_id: String,
    pub roots: EnvelopeRoots,
    pub request: EnvelopeRequest,
    pub policy: EnvelopePolicy,
    pub callback: EnvelopeCallback,
    /// Pre-resolved extends/references DAGs and per-step outputs, plus
    /// the verified root payload itself. Always present — empty
    /// `ResolutionOutput.ancestors` if the kind has no extends step,
    /// never `null`. Runtimes consume this directly instead of
    /// re-walking the chain themselves *or* re-reading the root from
    /// disk.
    pub resolution: ResolutionOutput,
    /// Pre-baked **inventory** of items the launching kind asked the
    /// daemon to resolve on its behalf. Keyed by inventoried kind
    /// (`"tool"`, `"knowledge"`, `"graph_node"`, …); each entry is a
    /// fully-parsed `ItemDescriptor` produced via
    /// `Engine::build_inventory_for_launching_kind`.
    ///
    /// Runtimes consume this directly — no re-scanning, no extension
    /// switching, no duplicate parser dispatch in agent code. The set
    /// of inventoried kinds is declared by the launching kind's
    /// schema (`inventory_kinds:` block); a kind that declares none
    /// receives an empty map. Each runtime project-specifically
    /// re-shapes the descriptors into its own typed view (e.g. the
    /// directive-runtime maps `inventory["tool"]` into its
    /// `ToolSchema`).
    #[serde(default)]
    pub inventory: HashMap<String, Vec<ItemDescriptor>>,
    /// Daemon-resolved, frozen provider snapshot. The daemon resolves
    /// the provider config once at preflight, validates it, and embeds
    /// the result here. Runtimes deserialize into their own typed
    /// `ResolvedProviderSnapshot` — the engine cannot depend on
    /// `ryeos-runtime` so this is an opaque JSON value here.
    ///
    /// Absent for non-directive runtimes that don't need provider config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_snapshot: Option<Value>,
}

/// Builder for [`LaunchEnvelope`].
///
/// Centralizes envelope construction so that new fields added to
/// `LaunchEnvelope` only need to be updated in one place. The builder
/// requires all mandatory fields; optional fields default sensibly.
///
/// # Example
///
/// ```ignore
/// let envelope = LaunchEnvelopeBuilder::new(
///     "inv-123".to_string(),
///     "T-456".to_string(),
///     roots,
///     EnvelopeRequest::simple(inputs),
///     EnvelopePolicy::new(caps, limits),
///     EnvelopeCallback::new(socket, token),
///     resolution,
/// )
/// .inventory(inv)
/// .build();
/// ```
#[derive(Debug, Clone)]
pub struct LaunchEnvelopeBuilder {
    invocation_id: String,
    thread_id: String,
    roots: EnvelopeRoots,
    request: EnvelopeRequest,
    policy: EnvelopePolicy,
    callback: EnvelopeCallback,
    resolution: ResolutionOutput,
    inventory: HashMap<String, Vec<ItemDescriptor>>,
    provider_snapshot: Option<Value>,
}

impl LaunchEnvelopeBuilder {
    /// Create a new builder with all mandatory fields.
    pub fn new(
        invocation_id: String,
        thread_id: String,
        roots: EnvelopeRoots,
        request: EnvelopeRequest,
        policy: EnvelopePolicy,
        callback: EnvelopeCallback,
        resolution: ResolutionOutput,
    ) -> Self {
        Self {
            invocation_id,
            thread_id,
            roots,
            request,
            policy,
            callback,
            resolution,
            inventory: HashMap::new(),
            provider_snapshot: None,
        }
    }

    /// Set the pre-baked inventory map.
    pub fn inventory(mut self, inventory: HashMap<String, Vec<ItemDescriptor>>) -> Self {
        self.inventory = inventory;
        self
    }

    /// Set the daemon-resolved provider snapshot for directive runtimes.
    pub fn provider_snapshot(mut self, snapshot: Value) -> Self {
        self.provider_snapshot = Some(snapshot);
        self
    }

    /// Build the final `LaunchEnvelope`.
    pub fn build(self) -> LaunchEnvelope {
        LaunchEnvelope {
            invocation_id: self.invocation_id,
            thread_id: self.thread_id,
            roots: self.roots,
            request: self.request,
            policy: self.policy,
            callback: self.callback,
            resolution: self.resolution,
            inventory: self.inventory,
            provider_snapshot: self.provider_snapshot,
        }
    }
}

impl EnvelopeRequest {
    /// Construct a simple request with just inputs and no parent context.
    pub fn simple(inputs: Value) -> Self {
        Self {
            inputs,
            previous_thread_id: None,
            parent_thread_id: None,
            parent_capabilities: None,
            depth: 0,
        }
    }
}

impl EnvelopePolicy {
    /// Construct a policy from effective capabilities and hard limits.
    pub fn new(effective_caps: Vec<String>, hard_limits: HardLimits) -> Self {
        Self {
            effective_caps,
            hard_limits,
        }
    }
}

impl EnvelopeCallback {
    /// Construct a callback from socket path and token.
    pub fn new(socket_path: PathBuf, token: String) -> Self {
        Self { socket_path, token }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopeRoots {
    pub project_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_root: Option<PathBuf>,
    pub system_roots: Vec<PathBuf>,
}

/// Intentionally open payload — shape is kind-defined.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopeRequest {
    /// Intentionally open — directive/graph inputs are user-defined per-kind.
    #[serde(default)]
    pub inputs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardLimits {
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub spend_usd: f64,
    #[serde(default)]
    pub spawns: u32,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub duration_seconds: u64,
}

impl Default for HardLimits {
    fn default() -> Self {
        Self {
            turns: 25,
            tokens: 200_000,
            spend_usd: 2.0,
            spawns: 10,
            depth: 5,
            duration_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopePolicy {
    #[serde(default)]
    pub effective_caps: Vec<String>,
    pub hard_limits: HardLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvelopeCallback {
    pub socket_path: PathBuf,
    pub token: String,
}

/// Intentionally open payload — the runtime's final output is kind-defined.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeResult {
    pub success: bool,
    pub status: String,
    pub thread_id: String,
    /// Terminal payload from the runtime. `Value` (not `String`) so
    /// runtimes that produce structured terminal output (graph
    /// runtime's `GraphResult`, directive runtime's tool/assistant
    /// final value, knowledge runtime's projection) can ship it
    /// without lossy stringification. The daemon passes this through
    /// verbatim into the `/execute` response envelope.
    ///
    /// Intentionally open — the shape is kind-defined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Intentionally open — same as result.
    #[serde(default)]
    pub outputs: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
    /// Non-fatal contract drift surfaced by the runtime — e.g. a
    /// callback `append_event` returned an error (event-vocabulary
    /// rejection, transient transport hiccup, …). Failed callbacks
    /// no longer terminate the runtime, but they MUST appear here so
    /// the daemon and operator can see what was dropped. Empty when
    /// no warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCost {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_usd: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let envelope = LaunchEnvelope {
            invocation_id: "inv-abc".to_string(),
            thread_id: "T-test".to_string(),
            roots: EnvelopeRoots {
                project_root: PathBuf::from("/project"),
                user_root: None,
                system_roots: vec![],
            },
            request: EnvelopeRequest {
                inputs: serde_json::json!({}),
                previous_thread_id: None,
                parent_thread_id: None,
                parent_capabilities: None,
                depth: 0,
            },
            policy: EnvelopePolicy {
                effective_caps: vec!["ryeos.execute.tool.*".to_string()],
                hard_limits: HardLimits::default(),
            },
            callback: EnvelopeCallback {
                socket_path: PathBuf::from("/tmp/ryeosd.sock"),
                token: "cbt-test".to_string(),
            },
            inventory: HashMap::new(),
            provider_snapshot: None,
            resolution: ResolutionOutput {
                root: crate::resolution::ResolvedAncestor {
                    requested_id: "directive:my/agent".to_string(),
                    resolved_ref: "directive:my/agent".to_string(),
                    source_path: PathBuf::from("/project/.ai/directives/my/agent.directive.md"),
                    trust_class: crate::resolution::TrustClass::Unsigned,
                    alias_resolution: None,
                    added_by: crate::resolution::ResolutionStepName::PipelineInit,
                    raw_content: String::new(),
                    raw_content_digest:
                        "0000000000000000000000000000000000000000000000000000000000000000"
                            .to_string(),
                },
                ancestors: vec![],
                references_edges: vec![],
                step_outputs: std::collections::HashMap::new(),
                executor_trust_class: crate::resolution::TrustClass::Unsigned,
                composed: crate::resolution::KindComposedView::identity(
                    serde_json::json!({}),
                ),
                referenced_items: vec![],
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: LaunchEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id, "T-test");
        assert!(parsed.resolution.ancestors.is_empty());
        assert_eq!(
            parsed.resolution.executor_trust_class,
            crate::resolution::TrustClass::Unsigned
        );
        assert!(parsed.resolution.composed.derived.is_empty());
        assert!(parsed.resolution.composed.policy_facts.is_empty());
    }

    #[test]
    fn runtime_result_round_trip() {
        let result = RuntimeResult {
            success: true,
            status: "completed".to_string(),
            thread_id: "T-test".to_string(),
            result: Some(serde_json::json!("Done")),
            outputs: serde_json::json!({"answer": "42"}),
            cost: Some(RuntimeCost {
                input_tokens: 100,
                output_tokens: 50,
                total_usd: 0.01,
            }),
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: RuntimeResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.cost.unwrap().input_tokens, 100);
    }

    #[test]
    fn hard_limits_defaults() {
        let limits = HardLimits::default();
        assert_eq!(limits.turns, 25);
        assert_eq!(limits.tokens, 200_000);
        assert_eq!(limits.spawns, 10);
    }

    #[test]
    fn builder_produces_same_envelope() {
        let resolution = ResolutionOutput {
            root: crate::resolution::ResolvedAncestor {
                requested_id: "directive:test".to_string(),
                resolved_ref: "directive:test".to_string(),
                source_path: PathBuf::from("/project/.ai/directives/test.directive.md"),
                trust_class: crate::resolution::TrustClass::Unsigned,
                alias_resolution: None,
                added_by: crate::resolution::ResolutionStepName::PipelineInit,
                raw_content: String::new(),
                raw_content_digest:
                    "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
            },
            ancestors: vec![],
            references_edges: vec![],
            step_outputs: std::collections::HashMap::new(),
            executor_trust_class: crate::resolution::TrustClass::Unsigned,
            composed: crate::resolution::KindComposedView::identity(
                serde_json::json!({}),
            ),
            referenced_items: vec![],
        };

        let envelope = LaunchEnvelopeBuilder::new(
            "inv-builder".to_string(),
            "T-builder".to_string(),
            EnvelopeRoots {
                project_root: PathBuf::from("/project"),
                user_root: None,
                system_roots: vec![],
            },
            EnvelopeRequest::simple(serde_json::json!({"key": "value"})),
            EnvelopePolicy::new(
                vec!["ryeos.execute.*".to_string()],
                HardLimits::default(),
            ),
            EnvelopeCallback::new(
                PathBuf::from("/tmp/ryeosd.sock"),
                "token-abc".to_string(),
            ),
            resolution,
        )
        .build();

        assert_eq!(envelope.invocation_id, "inv-builder");
        assert_eq!(envelope.thread_id, "T-builder");
        assert_eq!(envelope.request.depth, 0);
        assert!(envelope.request.previous_thread_id.is_none());
        assert_eq!(envelope.policy.effective_caps, vec!["ryeos.execute.*"]);
        assert_eq!(envelope.callback.socket_path, PathBuf::from("/tmp/ryeosd.sock"));
        assert!(envelope.inventory.is_empty());

        // Round-trip through JSON
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: LaunchEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.thread_id, "T-builder");
    }
}
