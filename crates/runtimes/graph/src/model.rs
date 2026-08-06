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

/// Strict executable shape of the graph's finalized composed value.
///
/// The generic composer and graph effective validator run before launch. The
/// runtime parses that same admitted composed value and checks its effective
/// digest and captured hook plan before executing it. `requires` remains on
/// the strict runtime shape so malformed or divergent capability declarations
/// fail before the first graph step.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphFile {
    #[serde(default)]
    extends: Option<String>,
    version: String,
    category: String,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    config: GraphConfig,
    /// Unified capability requirements (`requires.capabilities`): `declared`
    /// (self-asserted action authority, composed into effective_caps) and
    /// `manifest` (runtime callback authority minted from the signed manifest).
    /// `deny_unknown_fields` makes the removed top-level `permissions:` fail
    /// to parse.
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
    pub root_raw_content_digest: String,
    pub effective_definition_digest: String,
    pub file_path: Option<String>,
    pub config: GraphConfig,
    /// Immutable execution sidecar compiled once from the strict source shape.
    /// The walker never parses expressions or scans templates at runtime.
    pub(crate) compiled: crate::compiled_graph::CompiledGraph,
    /// Self-asserted action authority in the finalized composed graph
    /// (`requires.capabilities.declared`). Launch admission narrows this
    /// against the manifest and binds the result into callback authority; the
    /// runtime keeps the declaration visible for traceability and parity.
    pub declared_permissions: Vec<String>,
    /// Structured runtime capability requirements declared by the graph
    /// (`requires.capabilities`). The daemon is the authority: it mints the
    /// requested manifest-backed subset into the callback token at launch.
    /// Retained here for traceability and static validation parity.
    pub runtime_capability_requirements:
        Option<ryeos_bundle::runtime_authority::RuntimeCapabilityRequirements>,
}

impl GraphDefinition {
    /// Construct only from the exact finalized resolution shipped in the
    /// managed envelope. Composed content is executable; root bytes remain
    /// provenance evidence.
    pub fn from_effective_resolution(
        resolution: &ryeos_engine::resolution::ResolutionOutput,
        expected_digest: &ryeos_engine::resolution::EffectiveDefinitionDigest,
        file_path: Option<&str>,
    ) -> anyhow::Result<Self> {
        let observed = resolution.effective_definition_digest()?;
        if &observed != expected_digest {
            anyhow::bail!(
                "effective graph digest mismatch: envelope={expected_digest}, runtime={observed}"
            );
        }
        ryeos_graph_definition::validate_effective_graph(&resolution.composed)?;
        let mut file: GraphFile = serde_json::from_value(resolution.composed.composed.clone())?;
        validate_extends_provenance(
            file.extends.as_deref(),
            resolution
                .ancestors
                .iter()
                .map(|ancestor| ancestor.requested_id.as_str()),
        )?;
        let plan_value = resolution
            .composed
            .derived
            .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
            .ok_or_else(|| anyhow::anyhow!("effective graph has no captured hook plan"))?;
        let plan = ryeos_engine::hooks::EffectiveHookPlan::from_value(plan_value)
            .map_err(|error| anyhow::anyhow!(error))?;
        if plan.authored.hooks != file.config.hooks {
            anyhow::bail!("effective graph authored hooks differ from captured hook plan");
        }
        let runtime_capability_requirements = match file.requires {
            Some(requires) => {
                let caps = requires.capabilities;
                ryeos_bundle::runtime_authority::validate_runtime_capability_requirements(&caps)
                    .map_err(|error| anyhow::anyhow!("invalid `requires.capabilities`: {error}"))?;
                Some(caps)
            }
            None => None,
        };
        let declared_permissions = runtime_capability_requirements
            .as_ref()
            .map(|caps| caps.declared.clone())
            .unwrap_or_default();
        let compiled =
            crate::compiled_graph::CompiledGraph::compile_effective(&file.config, &plan)?;
        // The captured, admitted plan is the only executable hook authority.
        // Do not retain a second raw authored copy in the live runtime model.
        file.config.hooks.clear();
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)
                .map_err(|error| anyhow::anyhow!("invalid resolved graph ref: {error}"))?;
        if canonical.kind != "graph" {
            anyhow::bail!("effective graph root has kind `{}`", canonical.kind);
        }
        // `category` is required and strictly decoded as authored metadata,
        // but canonical execution identity comes from the admitted root ref.
        let _ = &file.category;
        Ok(Self {
            version: file.version,
            graph_id: canonical.bare_id,
            definition_ref: resolution.root.resolved_ref.clone(),
            root_raw_content_digest: resolution.root.raw_content_digest.clone(),
            effective_definition_digest: expected_digest.to_string(),
            file_path: file_path.map(String::from),
            config: file.config,
            compiled,
            declared_permissions,
            runtime_capability_requirements,
        })
    }

    #[cfg(test)]
    pub fn from_yaml(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        Self::from_yaml_effective_fixture(raw, file_path)
    }

    /// Test adapter into the production effective-resolution constructor. No
    /// test-only graph compiler or alternate runtime parser exists.
    ///
    /// Fidelity note (post-activation ledger E16): this fixture hand-duplicates
    /// the graph event contracts. They match the signed kind schema today and
    /// drift fails closed (plan validation rejects unknown contracts), but a
    /// schema change must be mirrored here or these tests go stale.
    #[cfg(test)]
    pub fn from_yaml_effective_fixture(raw: &str, file_path: Option<&str>) -> anyhow::Result<Self> {
        Self::from_yaml_effective_fixture_with_hook_sources(
            raw,
            file_path,
            ryeos_runtime::HookSources::default(),
        )
    }

    #[cfg(test)]
    pub fn from_yaml_effective_fixture_with_hook_sources(
        raw: &str,
        file_path: Option<&str>,
        mut hook_sources: ryeos_runtime::HookSources,
    ) -> anyhow::Result<Self> {
        use ryeos_engine::hooks::{
            EFFECTIVE_HOOK_PLAN_DERIVED_KEY, EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer,
            EffectiveHookPlan, HOOK_CONTEXT_SCHEMA, HookContextContract, HookEventContract,
            HookLayer, HookResultMode, HookSourceEvidence,
        };
        use ryeos_engine::resolution::{
            KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
        };
        use std::collections::{BTreeMap, BTreeSet, HashMap};

        let raw = lillux::signature::strip_signature_lines(raw);
        let composed: Value = serde_yaml::from_str(&raw)?;
        let file: GraphFile = serde_json::from_value(composed.clone())?;
        hook_sources.authored = file.config.hooks.clone();
        let event_contract = |roots: &[&str]| HookEventContract {
            context_contract: HookContextContract {
                schema: HOOK_CONTEXT_SCHEMA.to_string(),
                allowed_roots: roots.iter().map(|root| root.to_string()).collect(),
            },
            allowed_results: BTreeSet::from([HookResultMode::Discard, HookResultMode::Observation]),
        };
        let event_contracts = BTreeMap::from([
            (
                "graph_started".to_string(),
                event_contract(&["event", "graph_id", "graph_run_id", "state", "inputs"]),
            ),
            (
                "graph_step_completed".to_string(),
                event_contract(&[
                    "event",
                    "graph_id",
                    "graph_run_id",
                    "node",
                    "step",
                    "status",
                    "state",
                    "error",
                ]),
            ),
            (
                "graph_completed".to_string(),
                event_contract(&[
                    "event",
                    "graph_id",
                    "graph_run_id",
                    "status",
                    "settled",
                    "steps",
                    "success",
                    "state",
                    "inputs",
                ]),
            ),
        ]);
        let layer = |hooks| EffectiveHookLayer {
            hooks,
            dispatch_caps: Vec::new(),
        };
        let configured_layers = [
            (HookLayer::Builtin, &hook_sources.builtin),
            (HookLayer::Infrastructure, &hook_sources.infrastructure),
            (HookLayer::Context, &hook_sources.context),
            (HookLayer::Operator, &hook_sources.operator),
            (HookLayer::Project, &hook_sources.project),
        ];
        let sources = configured_layers
            .iter()
            .filter(|(_, hooks)| !hooks.is_empty())
            .map(|(layer, _)| HookSourceEvidence {
                layer: *layer,
                canonical_ref: format!("config:test/{}", layer.as_str()),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: "e".repeat(64),
                source_raw_content_digest: lillux::cas::sha256_hex(layer.as_str().as_bytes()),
            })
            .collect();
        let plan = EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "graph".to_string(),
            event_contracts,
            authored: EffectiveHookLayer {
                hooks: hook_sources.authored,
                dispatch_caps: file
                    .requires
                    .as_ref()
                    .map(|requires| requires.capabilities.declared.clone())
                    .unwrap_or_default(),
            },
            builtin: layer(hook_sources.builtin),
            infrastructure: layer(hook_sources.infrastructure),
            context: layer(hook_sources.context),
            operator: layer(hook_sources.operator),
            project: layer(hook_sources.project),
            sources,
        };
        let graph_id = file_path
            .and_then(|path| std::path::Path::new(path).file_stem())
            .and_then(|stem| stem.to_str())
            .unwrap_or("fixture");
        let bare_id = if file.category.is_empty() {
            graph_id.to_string()
        } else {
            format!("{}/{graph_id}", file.category)
        };
        let definition_ref = format!("graph:{bare_id}");
        let raw_content_digest = lillux::cas::sha256_hex(raw.as_bytes());
        let effective_caps = file
            .requires
            .as_ref()
            .map(|requires| requires.capabilities.declared.clone())
            .unwrap_or_default();
        let resolution = ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: definition_ref.clone(),
                resolved_ref: definition_ref,
                source_path: file_path.unwrap_or("fixture.yaml").into(),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                trust_class: TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: raw,
                source_content_digest: raw_content_digest.clone(),
                raw_content_digest,
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: HashMap::new(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView {
                composed,
                derived: HashMap::from([(
                    EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_string(),
                    plan.to_value().map_err(|error| anyhow::anyhow!(error))?,
                )]),
                policy_facts: HashMap::from([(
                    "effective_caps".to_string(),
                    serde_json::to_value(effective_caps)?,
                )]),
            },
        };
        let digest = resolution.effective_definition_digest()?;
        Self::from_effective_resolution(&resolution, &digest, file_path)
    }
}

fn validate_extends_provenance<'a>(
    declared: Option<&str>,
    mut ancestor_requested_ids: impl DoubleEndedIterator<Item = &'a str>,
) -> anyhow::Result<()> {
    // Extends resolution is deepest-first, so the final admitted ancestor is
    // the root's immediate parent. `requested_id` deliberately preserves the
    // exact authored spelling, including aliases.
    match (declared, ancestor_requested_ids.next_back()) {
        (None, None) => Ok(()),
        (Some(_), None) => {
            anyhow::bail!("effective graph declares `extends` but has no admitted ancestor")
        }
        (None, Some(_)) => {
            anyhow::bail!("effective graph has admitted ancestors but declares no `extends`")
        }
        (Some(declared), Some(parent)) if declared == parent => Ok(()),
        (Some(declared), Some(parent)) => anyhow::bail!(
            "effective graph `extends` provenance mismatch: composed={declared}, admitted={}",
            parent
        ),
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
    pub effective_definition_digest: String,
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
    pub effective_definition_digest: String,
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
    pub(crate) state_anchors: Vec<Value>,
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
            state_anchors: Vec::new(),
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
        let state_anchors = result
            .get("state_anchors")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if child_thread_id.is_none() && milestones.is_empty() && state_anchors.is_empty() {
            None
        } else {
            Some(Self {
                item_id: item_id.into(),
                child_thread_id,
                milestones,
                state_anchors,
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
    use ryeos_engine::contracts::ItemSpace;
    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, EffectiveHookPlan, HOOK_CONTEXT_SCHEMA,
        HookContextContract, HookEventContract, HookResultMode,
    };
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    fn resolution_node(
        requested_id: &str,
        resolved_ref: &str,
        digest_byte: char,
        added_by: ResolutionStepName,
    ) -> ResolvedAncestor {
        ResolvedAncestor {
            requested_id: requested_id.to_string(),
            resolved_ref: resolved_ref.to_string(),
            source_path: PathBuf::from(format!("/diagnostic/{digest_byte}.yaml")),
            source_space: ItemSpace::Bundle,
            trust_class: TrustClass::TrustedBundle,
            signer_fingerprint: Some("f".repeat(64)),
            alias_resolution: None,
            added_by,
            raw_content: format!("source:{digest_byte}"),
            source_content_digest: digest_byte.to_string().repeat(64),
            raw_content_digest: digest_byte.to_string().repeat(64),
        }
    }

    fn empty_graph_plan() -> EffectiveHookPlan {
        let empty = EffectiveHookLayer::empty();
        EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "graph".to_string(),
            event_contracts: BTreeMap::from([(
                "graph_started".to_string(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_string(),
                        allowed_roots: BTreeSet::from(["event".to_string()]),
                    },
                    allowed_results: BTreeSet::from([
                        HookResultMode::Discard,
                        HookResultMode::Observation,
                    ]),
                },
            )]),
            authored: empty.clone(),
            builtin: empty.clone(),
            infrastructure: empty.clone(),
            context: empty.clone(),
            operator: empty.clone(),
            project: empty,
            sources: Vec::new(),
        }
    }

    fn inherited_effective_resolution() -> ResolutionOutput {
        let plan = empty_graph_plan();
        ResolutionOutput {
            root: resolution_node(
                "graph:test/effective",
                "graph:test/effective",
                'a',
                ResolutionStepName::PipelineInit,
            ),
            ancestors: vec![resolution_node(
                "graph:test/base",
                "graph:test/base",
                'b',
                ResolutionStepName::ResolveExtendsChain,
            )],
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: HashMap::new(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView {
                composed: json!({
                    "version": "1.0.0",
                    "category": "test",
                    "extends": "graph:test/base",
                    "config": {
                        "start": "inherited",
                        "nodes": {
                            "inherited": {
                                "next": {"type": "unconditional", "to": "finish"}
                            },
                            "finish": {"node_type": "return", "output": "done"}
                        }
                    }
                }),
                derived: HashMap::from([(
                    "effective_hook_plan".to_string(),
                    plan.to_value().unwrap(),
                )]),
                policy_facts: HashMap::from([("effective_caps".to_string(), json!([]))]),
            },
        }
    }

    #[test]
    fn runtime_constructs_from_the_complete_admitted_effective_resolution() {
        let resolution = inherited_effective_resolution();
        let digest = resolution.effective_definition_digest().unwrap();
        let graph = GraphDefinition::from_effective_resolution(
            &resolution,
            &digest,
            Some("/diagnostic/root.yaml"),
        )
        .unwrap();

        assert!(graph.config.nodes.contains_key("inherited"));
        assert_eq!(graph.definition_ref, "graph:test/effective");
        assert_eq!(graph.root_raw_content_digest, "a".repeat(64));
        assert_eq!(graph.effective_definition_digest, digest.to_string());

        let wrong =
            ryeos_engine::resolution::EffectiveDefinitionDigest::parse("0".repeat(64)).unwrap();
        let error =
            GraphDefinition::from_effective_resolution(&resolution, &wrong, None).unwrap_err();
        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn effective_extends_matches_immediate_admitted_parent() {
        validate_extends_provenance(
            Some("@base"),
            ["graph:test/grandparent", "@base"].into_iter(),
        )
        .unwrap();
        validate_extends_provenance(None, std::iter::empty()).unwrap();
    }

    #[test]
    fn effective_extends_rejects_missing_or_divergent_provenance() {
        assert!(validate_extends_provenance(Some("graph:test/base"), std::iter::empty()).is_err());
        assert!(validate_extends_provenance(None, ["graph:test/base"].into_iter()).is_err());
        assert!(
            validate_extends_provenance(Some("graph:test/other"), ["graph:test/base"].into_iter(),)
                .is_err()
        );
    }

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
    fn dispatch_observation_carries_authoritative_state_anchor_requests_separately() {
        let observation = DispatchObservation::from_success(
            "tool:test",
            None,
            &json!({
                "milestones": [{"kind": "domain.fact", "payload": {}}],
                "state_anchors": [{
                    "contract": "domain.restore.v1",
                    "restore": {"value": 1}
                }]
            }),
        )
        .unwrap();
        assert_eq!(observation.milestones.len(), 1);
        assert_eq!(observation.state_anchors.len(), 1);
        assert_eq!(
            observation.state_anchors[0]["contract"],
            "domain.restore.v1"
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

    /// `requires.capabilities.declared` propagates to `declared_permissions`
    /// from the same finalized composed value used by launch admission.
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

    /// The removed top-level `permissions:` block fails strict decoding.
    #[test]
    fn removed_top_level_permissions_rejected() {
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
        project_snapshots: [status]
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
                "ryeos.status.project-snapshots.live".to_string(),
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
    fn root_source_identity_uses_signature_stripped_body() {
        let yaml = r#"<!-- ryeos:signed:old -->
version: "1.0.0"
category: test
config:
  start: a
  nodes:
    a: {node_type: return}
"#;
        let def = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
        let cleaned = lillux::signature::strip_signature_lines(yaml);
        assert_eq!(def.definition_ref, "graph:test/test");
        assert_eq!(
            def.root_raw_content_digest,
            lillux::cas::sha256_hex(cleaned.as_bytes())
        );
    }

    #[test]
    fn effective_fixture_preserves_authored_signature_marker_text() {
        let yaml = r#"
version: "1.0.0"
category: self_asserted
description: "literal ryeos:signed: marker"
config:
  start: a
  nodes:
    a: {node_type: return}
"#;
        GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap();
    }

    #[test]
    fn effective_definition_digest_ignores_signature_line_changes_but_not_body() {
        let body = r#"version: "1.0.0"
category: test
config:
  start: a
  nodes:
    a: {node_type: return, output: original}
"#;

        let signed_a = format!("<!-- ryeos:signed:old -->\n{body}");
        let signed_b = format!("<!-- ryeos:signed:new -->\n{body}");
        let changed_body = r#"version: "1.0.0"
category: test
config:
  start: a
  nodes:
    a: {node_type: return, output: changed}
"#;
        let signed_changed = format!("<!-- ryeos:signed:new -->\n{changed_body}");

        let a = GraphDefinition::from_yaml(&signed_a, Some("test.yaml")).unwrap();
        let b = GraphDefinition::from_yaml(&signed_b, Some("test.yaml")).unwrap();
        let changed = GraphDefinition::from_yaml(&signed_changed, Some("test.yaml")).unwrap();

        assert_eq!(a.effective_definition_digest, b.effective_definition_digest);
        assert_ne!(
            a.effective_definition_digest,
            changed.effective_definition_digest
        );
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
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        let error = format!("{error:#}");
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
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        let error = format!("{error:#}");
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
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        let error = format!("{error:#}");
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
            assert!(
                error.contains("iteration variable"),
                "unexpected error: {error}"
            );
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
        let error = GraphDefinition::from_yaml(yaml, Some("test.yaml")).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains("more than one default"),
            "unexpected error: {error}"
        );
    }
}
