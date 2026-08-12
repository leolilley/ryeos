//! Side-effect-free validation of a completely composed graph definition.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use ryeos_engine::hooks::{EffectiveHookPlan, ExpressionCondition, HookDefinition};
use ryeos_engine::resolution::KindComposedView;
use ryeos_runtime::envelope::RuntimeCost;
use ryeos_runtime::events::RuntimeEventType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod compiled;

pub use compiled::{
    CompiledCondition, CompiledConditionalEdge, CompiledEdgeSpec, CompiledGraph, CompiledNode,
};

pub const GRAPH_EFFECTIVE_VALIDATION_SCHEMA: &str = "ryeos.graph.effective_validation.v1";
pub const DEFAULT_GRAPH_MAX_STEPS: u32 = 100;
pub const MAX_GRAPH_STEPS: u32 = 500;
pub const MAX_GRAPH_SEGMENT_STEPS: u32 = MAX_GRAPH_STEPS;
pub const MAX_RETRY_BACKOFF_MS: u64 = 300_000;
pub const MAX_NODE_CONCURRENCY: usize = 256;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_costs: Vec<NodeCostRecord>,
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
    pub step: Option<u32>,
    pub cost: RuntimeCost,
}

/// Project a graph runtime's complete durable result to the value exposed to
/// an authored parent action. The complete state remains on the child thread;
/// follow transport carries only this return value.
pub fn project_graph_action_result(result: Value, outputs: Value) -> Result<Value, String> {
    if !outputs.is_null() {
        return Err("graph runtime envelope must carry null `outputs`".to_string());
    }

    let graph_result: GraphResult = serde_json::from_value(result)
        .map_err(|error| format!("graph runtime returned malformed GraphResult: {error}"))?;
    let definition_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(
        &graph_result.definition_ref,
    )
    .map_err(|error| {
        format!(
            "graph runtime returned GraphResult with invalid definition_ref `{}`: {error}",
            graph_result.definition_ref
        )
    })?;
    if definition_ref.kind != "graph" {
        return Err(format!(
            "graph runtime returned GraphResult with non-graph definition_ref `{}`",
            graph_result.definition_ref
        ));
    }
    let successful_status = matches!(
        graph_result.status,
        GraphRunStatus::Valid | GraphRunStatus::Completed | GraphRunStatus::CompletedWithErrors
    );
    if !graph_result.success || !successful_status {
        return Err(format!(
            "graph runtime returned success envelope with contradictory GraphResult success={} status=`{}`",
            graph_result.success,
            graph_result.status.as_str()
        ));
    }

    Ok(graph_result.result.unwrap_or(Value::Null))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphFile {
    pub version: String,
    pub category: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub extends: Option<String>,
    pub config: GraphConfig,
    #[serde(default)]
    pub requires: Option<ryeos_bundle::runtime_authority::RuntimeRequires>,
    /// Item-level external content declaration. Validated, captured, and
    /// realized entirely by the engine's admission path; opaque to graph
    /// semantics, so the strict decode names the field without
    /// interpreting it.
    #[serde(default)]
    pub external_content: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphConfig {
    pub start: String,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub on_error: ErrorMode,
    #[serde(default)]
    pub nodes: BTreeMap<String, GraphNode>,
    #[serde(default)]
    pub hooks: Vec<HookDefinition>,
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

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ErrorMode {
    #[default]
    Fail,
    Continue,
}

/// Determinism class of a node's action: whether its result may be replayed
/// across runs from a durable effect record. `sealed` re-derives or replays
/// (a divergence is a substrate bug), `recorded` replays the record, and
/// `live` — the default — never replays across runs. Orthogonal to
/// `cache_result`, which governs reuse within one execution only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Sealed,
    Recorded,
    #[default]
    Live,
}

impl EffectClass {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Recorded => "recorded",
            Self::Live => "live",
        }
    }
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
    #[serde(default, skip_serializing_if = "EffectClass::is_live")]
    pub effects: EffectClass,
    #[serde(default)]
    pub follow: bool,
    #[serde(default)]
    pub detach: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facets: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#as: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collect_threads: Option<String>,
    #[serde(default)]
    pub parallel: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
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

    pub fn effect_class(&self) -> EffectClass {
        self.effects
    }

    pub fn foreach_var(&self) -> &str {
        self.r#as.as_deref().unwrap_or("item")
    }

    pub fn fold_detach_into_action(&self, action: &mut Value) {
        if !self.detach {
            return;
        }
        if let Some(object) = action.as_object_mut() {
            object.insert(
                ryeos_runtime::callback::action_keys::THREAD.to_owned(),
                Value::String("detached".to_owned()),
            );
            if let Some(facets) = &self.facets {
                object.insert(
                    ryeos_runtime::callback::action_keys::FACETS.to_owned(),
                    facets.clone(),
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
    pub when: ExpressionCondition,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_backoff_ms: Option<u64>,
}

impl RetryConfig {
    pub fn delay_ms(&self, failed_attempt: u32) -> u64 {
        let exponent = failed_attempt.saturating_sub(1).min(63);
        let grown = self.backoff_ms.saturating_mul(1_u64 << exponent);
        self.max_backoff_ms
            .map_or(grown, |maximum| grown.min(maximum))
            .min(MAX_RETRY_BACKOFF_MS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationSummary {
    pub schema: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub authored_hook_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EffectiveGraphValidation {
    pub summary: ValidationSummary,
    pub effect_authorizations: Vec<ryeos_effect_contract::EffectAuthorizationProjection>,
}

/// One kind-owned preparation result consumed both at admission and by the
/// graph runtime. The compiled sidecar is transient; the file and validation
/// are derived from the same exact composed program and ancestor provenance.
#[derive(Debug, Clone)]
pub struct PreparedEffectiveGraph {
    pub canonical_ref: ryeos_engine::canonical_ref::CanonicalRef,
    pub file: GraphFile,
    pub compiled: CompiledGraph,
    pub declared_permissions: Vec<String>,
    pub runtime_capability_requirements:
        Option<ryeos_bundle::runtime_authority::RuntimeCapabilityRequirements>,
    pub validation: EffectiveGraphValidation,
}

/// Decode and validate the exact composed graph and captured hook plan.
pub fn validate_effective_graph(
    canonical_ref: &str,
    view: &KindComposedView,
    ancestor_requested_ids: &[String],
) -> Result<EffectiveGraphValidation> {
    prepare_effective_graph(canonical_ref, view, ancestor_requested_ids)
        .map(|prepared| prepared.validation)
}

/// Prepare the one strict graph product whose validity is proven at admission
/// and consumed by the runtime. No second parser or compiler is permitted
/// after launch authority has been minted.
pub fn prepare_effective_graph(
    canonical_ref: &str,
    view: &KindComposedView,
    ancestor_requested_ids: &[String],
) -> Result<PreparedEffectiveGraph> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(canonical_ref)
        .map_err(|error| anyhow!("invalid resolved graph ref: {error}"))?;
    if canonical.kind != "graph" || canonical.to_string() != canonical_ref {
        bail!("effective graph canonical ref is not an exact graph ref");
    }
    let file: GraphFile =
        serde_json::from_value(view.composed.clone()).context("strictly decode composed graph")?;
    validate_graph_file(&file)?;
    validate_extends_provenance(file.extends.as_deref(), ancestor_requested_ids)?;
    let runtime_capability_requirements = file
        .requires
        .as_ref()
        .map(|requires| requires.capabilities.clone());
    if let Some(requirements) = runtime_capability_requirements.as_ref() {
        ryeos_bundle::runtime_authority::validate_runtime_capability_requirements(requirements)
            .map_err(|error| anyhow!("invalid `requires.capabilities`: {error}"))?;
    }
    let declared_permissions = runtime_capability_requirements
        .as_ref()
        .map(|requirements| requirements.declared.clone())
        .unwrap_or_default();

    let plan_value = view
        .derived
        .get(ryeos_engine::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY)
        .ok_or_else(|| anyhow!("composed graph has no captured `effective_hook_plan`"))?;
    let plan = EffectiveHookPlan::from_value(plan_value).map_err(|error| anyhow!(error))?;
    if plan.owner_kind != "graph" {
        bail!("effective hook plan owner_kind must be `graph`");
    }
    if plan.authored.hooks != file.config.hooks {
        bail!("captured authored hook layer differs from composed config.hooks");
    }
    let compiled = CompiledGraph::compile_effective(&file.config, &plan)?;
    let declared = serde_json::to_value(
        file.requires
            .as_ref()
            .map(|requires| requires.capabilities.declared.as_slice())
            .unwrap_or_default(),
    )?;
    let effective_caps = view
        .policy_facts
        .get("effective_caps")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    if declared != effective_caps {
        bail!("composed declared capabilities differ from effective_caps policy fact");
    }

    let edge_count = file
        .config
        .nodes
        .values()
        .map(|node| match &node.next {
            Some(EdgeSpec::Unconditional { .. }) => 1,
            Some(EdgeSpec::Conditional { branches }) => branches.len(),
            None => 0,
        })
        .sum();
    let effect_authorizations = compile_effect_authorizations(&file)?;
    let validation = EffectiveGraphValidation {
        summary: ValidationSummary {
            schema: GRAPH_EFFECTIVE_VALIDATION_SCHEMA.to_string(),
            node_count: file.config.nodes.len(),
            edge_count,
            authored_hook_ids: file
                .config
                .hooks
                .iter()
                .map(|hook| hook.id.clone())
                .collect(),
        },
        effect_authorizations,
    };
    Ok(PreparedEffectiveGraph {
        canonical_ref: canonical,
        file,
        compiled,
        declared_permissions,
        runtime_capability_requirements,
        validation,
    })
}

fn validate_extends_provenance(
    declared: Option<&str>,
    ancestor_requested_ids: &[String],
) -> Result<()> {
    // Extends resolution is deepest-first, so the final admitted ancestor is
    // the root's immediate parent. requested_id preserves authored spelling.
    match (declared, ancestor_requested_ids.last().map(String::as_str)) {
        (None, None) => Ok(()),
        (Some(_), None) => {
            bail!("effective graph declares `extends` but has no admitted ancestor")
        }
        (None, Some(_)) => {
            bail!("effective graph has admitted ancestors but declares no `extends`")
        }
        (Some(declared), Some(parent)) if declared == parent => Ok(()),
        (Some(declared), Some(parent)) => bail!(
            "effective graph `extends` provenance mismatch: composed={declared}, admitted={parent}"
        ),
    }
}

fn compile_effect_authorizations(
    file: &GraphFile,
) -> Result<Vec<ryeos_effect_contract::EffectAuthorizationProjection>> {
    let mut projections = Vec::new();
    for (name, node) in &file.config.nodes {
        let class = match node.effects {
            EffectClass::Live => continue,
            EffectClass::Recorded => ryeos_effect_contract::EffectClass::Recorded,
            EffectClass::Sealed => ryeos_effect_contract::EffectClass::Sealed,
        };
        let action = node.action.as_ref().ok_or_else(|| {
            anyhow!("durable graph node `{name}` has no authored action contract")
        })?;
        let action_contract_digest = ryeos_effect_contract::canonical_value_digest(action)?;
        let policy_digest = ryeos_effect_contract::canonical_value_digest(&serde_json::json!({
            "node_type": node.node_type,
            "follow": node.follow,
            "detach": node.detach,
            "retry": node.retry.as_ref().map(|retry| serde_json::json!({
                "attempts": retry.attempts,
                "backoff_ms": retry.backoff_ms,
                "max_backoff_ms": retry.max_backoff_ms,
            })),
            "effect_class": node.effects,
        }))?;
        projections.push(ryeos_effect_contract::EffectAuthorizationProjection {
            authorization_id: format!("node:{name}"),
            policy_digest,
            action_contract_digest,
            class,
        });
    }
    ryeos_effect_contract::validate_authorization_projections(&projections)?;
    Ok(projections)
}

pub fn validate_graph_file(file: &GraphFile) -> Result<()> {
    if file.version.trim().is_empty() {
        bail!("graph version must be non-empty");
    }
    if file.category.trim().is_empty() {
        bail!("graph category must be non-empty");
    }
    if let Some(requires) = &file.requires {
        ryeos_bundle::runtime_authority::validate_runtime_capability_requirements(
            &requires.capabilities,
        )
        .map_err(|error| anyhow!("invalid `requires.capabilities`: {error}"))?;
        ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(
            &requires.capabilities.declared,
        )
        .map_err(|error| anyhow!("graph declared capability rejected: {error}"))?;
    }
    let config = &file.config;
    if config
        .state
        .as_ref()
        .is_some_and(|state| !state.is_object())
    {
        bail!("config.state must be a mapping");
    }
    if !(1..=MAX_GRAPH_STEPS).contains(&config.max_steps) {
        bail!("config.max_steps must be between 1 and {MAX_GRAPH_STEPS}");
    }
    if let Some(segment) = config.segment_steps
        && (!(1..=MAX_GRAPH_SEGMENT_STEPS).contains(&segment) || segment > config.max_steps)
    {
        bail!("config.segment_steps must be in range and no greater than max_steps");
    }
    if config.start.is_empty() || !config.nodes.contains_key(&config.start) {
        bail!("config.start must name an existing node");
    }
    if config.nodes.is_empty() {
        bail!("config.nodes must not be empty");
    }
    for requirement in &config.env_requires {
        if requirement.trim().is_empty() {
            bail!("config.env_requires contains an empty name");
        }
    }

    for (name, node) in &config.nodes {
        validate_node(name, node, config)?;
    }
    Ok(())
}

fn validate_node(name: &str, node: &GraphNode, config: &GraphConfig) -> Result<()> {
    if name.is_empty() {
        bail!("graph contains an empty node id");
    }
    if node
        .assign
        .as_ref()
        .is_some_and(|assign| !assign.is_object())
    {
        bail!("node `{name}` assign must be a mapping");
    }
    for requirement in &node.env_requires {
        if requirement.trim().is_empty() {
            bail!("node `{name}` env_requires contains an empty name");
        }
    }

    let assign_keys = node
        .assign
        .as_ref()
        .and_then(Value::as_object)
        .map(|assign| assign.keys().map(String::as_str).collect::<HashSet<_>>())
        .unwrap_or_default();
    let follow_fanout = node.node_type == NodeType::Action && node.follow && node.over.is_some();
    let iterates = node.node_type == NodeType::Foreach || follow_fanout;

    if node.parallel && node.assign.is_some() && !follow_fanout {
        bail!("node `{name}` cannot combine parallel with assign");
    }
    if node.assign.is_some() && !matches!(node.node_type, NodeType::Action | NodeType::Foreach) {
        bail!("node `{name}` assignment is only valid on action and foreach nodes");
    }
    if node.output.is_some() && node.node_type != NodeType::Return {
        bail!("node `{name}` output is only valid on a return node");
    }
    if node.action.is_some() && matches!(node.node_type, NodeType::Gate | NodeType::Return) {
        bail!("node `{name}` cannot declare action on a gate or return node");
    }
    if !iterates
        && (node.over.is_some()
            || node.r#as.is_some()
            || node.collect.is_some()
            || node.collect_threads.is_some()
            || node.parallel)
    {
        bail!("node `{name}` declares iteration fields without foreach or follow fanout");
    }
    if node.collect_threads.is_some() && !follow_fanout {
        bail!("node `{name}` collect_threads requires follow fanout");
    }
    if node.node_type == NodeType::Action && node.action.is_none() && name != config.start {
        bail!("node `{name}` is an ambiguous action node without action");
    }

    match node.node_type {
        NodeType::Foreach => {
            if node.action.is_none() || node.over.is_none() || node.r#as.is_none() {
                bail!("foreach node `{name}` requires action, over, and as");
            }
            if node.cache_result {
                bail!("foreach node `{name}` cannot cache its aggregate dispatch");
            }
            if !node.effects.is_live() {
                bail!(
                    "foreach node `{name}` cannot declare a durable effect class \
                     for its aggregate dispatch"
                );
            }
            if !node.env_requires.is_empty() {
                bail!("foreach node `{name}` cannot declare inert env_requires");
            }
            validate_iteration_keys(name, node, &assign_keys)?;
        }
        NodeType::Gate => match &node.next {
            Some(EdgeSpec::Conditional { branches }) if !branches.is_empty() => {}
            _ => bail!("gate node `{name}` requires non-empty conditional next"),
        },
        NodeType::Return => {
            if node.next.is_some() {
                bail!("return node `{name}` cannot declare next");
            }
        }
        NodeType::Action => {}
    }

    if node.follow {
        if node.node_type != NodeType::Action || node.action.is_none() {
            bail!("follow node `{name}` requires an action node with action");
        }
        if node.detach {
            bail!("node `{name}` cannot combine follow and detach");
        }
        if node.cache_result {
            bail!("follow node `{name}` cannot cache a result before its child settles");
        }
        if !node.effects.is_live() {
            bail!(
                "follow node `{name}` cannot declare a durable effect class \
                 before its child settles"
            );
        }
        if node.over.is_some() {
            if !node.parallel || node.assign.is_some() || node.r#as.is_none() {
                bail!("follow fanout node `{name}` requires parallel and as, and forbids assign");
            }
            validate_iteration_keys(name, node, &assign_keys)?;
        } else if node.parallel {
            bail!("single follow node `{name}` cannot set parallel");
        }
    }

    if node.detach {
        if !matches!(node.node_type, NodeType::Action | NodeType::Foreach) || node.action.is_none()
        {
            bail!("detach node `{name}` requires an action or foreach node with action");
        }
        if node.follow {
            bail!("node `{name}` cannot combine detach and follow");
        }
        if node.cache_result {
            bail!("detach node `{name}` cannot cache a result at launch time");
        }
        if !node.effects.is_live() {
            bail!("detach node `{name}` cannot declare a durable effect class at launch time");
        }
    }
    if node.facets.is_some() && !(node.detach || follow_fanout) {
        bail!("node `{name}` facets require detach or follow fanout");
    }

    if let Some(max) = node.max_concurrency {
        let has_consumer = (node.node_type == NodeType::Foreach && node.parallel)
            || (node.node_type == NodeType::Foreach && node.detach)
            || follow_fanout;
        if !(1..=MAX_NODE_CONCURRENCY).contains(&max) || !has_consumer {
            bail!("node `{name}` has invalid or inert max_concurrency");
        }
    }
    if let Some(retry) = &node.retry
        && (!matches!(node.node_type, NodeType::Action | NodeType::Foreach)
            || node.follow
            || !(1..=10).contains(&retry.attempts)
            || retry.backoff_ms == 0
            || retry.backoff_ms > MAX_RETRY_BACKOFF_MS
            || retry
                .max_backoff_ms
                .is_some_and(|max| max < retry.backoff_ms || max > MAX_RETRY_BACKOFF_MS))
    {
        bail!("node `{name}` has invalid retry policy");
    }
    if let Some(target) = &node.on_error
        && !config.nodes.contains_key(target)
    {
        bail!("node `{name}` on_error target `{target}` does not exist");
    }
    match &node.next {
        Some(EdgeSpec::Unconditional { to }) => validate_target(name, to, config)?,
        Some(EdgeSpec::Conditional { branches }) => {
            if branches.is_empty() {
                bail!("node `{name}` has an empty conditional edge list");
            }
            let mut default_seen = false;
            for branch in branches {
                validate_target(name, &branch.to, config)?;
                match &branch.when {
                    ExpressionCondition::Absent => {
                        if default_seen {
                            bail!("node `{name}` has more than one default branch");
                        }
                        default_seen = true;
                    }
                    ExpressionCondition::Expression(source) => {
                        ryeos_expression::compile_expression_for(
                            source,
                            &format!("nodes.{name}.next.when"),
                            &ryeos_expression::CompilationLimits::default(),
                        )
                        .map_err(|error| anyhow!(error))?;
                    }
                    ExpressionCondition::Boolean(_) => {}
                }
            }
        }
        None => {}
    }
    Ok(())
}

fn validate_iteration_keys(
    name: &str,
    node: &GraphNode,
    assign_keys: &HashSet<&str>,
) -> Result<()> {
    if let (Some(collect), Some(as_var)) = (&node.collect, &node.r#as)
        && collect == as_var
    {
        bail!("node `{name}` cannot use `{collect}` for both collect and as");
    }
    if let Some(as_var) = &node.r#as
        && assign_keys.contains(as_var.as_str())
    {
        bail!("node `{name}` uses `{as_var}` as both its iteration variable and assign key");
    }
    if let Some(collect) = &node.collect
        && assign_keys.contains(collect.as_str())
    {
        bail!("node `{name}` uses `{collect}` as both its collect and assign key");
    }
    if let (Some(collect_threads), Some(as_var)) = (&node.collect_threads, &node.r#as)
        && collect_threads == as_var
    {
        bail!("node `{name}` cannot use `{collect_threads}` for both collect_threads and as");
    }
    if let (Some(collect_threads), Some(collect)) = (&node.collect_threads, &node.collect)
        && collect_threads == collect
    {
        bail!("node `{name}` cannot use `{collect_threads}` for both collect_threads and collect");
    }
    if let Some(collect_threads) = &node.collect_threads
        && assign_keys.contains(collect_threads.as_str())
    {
        bail!("node `{name}` uses `{collect_threads}` as both its collect_threads and assign key");
    }
    Ok(())
}

fn validate_target(name: &str, target: &str, config: &GraphConfig) -> Result<()> {
    if target.is_empty() || !config.nodes.contains_key(target) {
        bail!("node `{name}` targets missing node `{target}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::hooks::{
        EFFECTIVE_HOOK_PLAN_SCHEMA, EffectiveHookLayer, HOOK_CONTEXT_SCHEMA, HookContextContract,
        HookEventContract, HookResultMode,
    };

    fn empty_layer() -> EffectiveHookLayer {
        EffectiveHookLayer::empty()
    }

    fn plan(authored: Vec<HookDefinition>) -> EffectiveHookPlan {
        EffectiveHookPlan {
            schema: EFFECTIVE_HOOK_PLAN_SCHEMA.to_string(),
            owner_kind: "graph".to_string(),
            event_contracts: BTreeMap::from([(
                "graph_started".to_string(),
                HookEventContract {
                    context_contract: HookContextContract {
                        schema: HOOK_CONTEXT_SCHEMA.to_string(),
                        allowed_roots: std::collections::BTreeSet::from(["event".to_string()]),
                    },
                    allowed_results: std::collections::BTreeSet::from([
                        HookResultMode::Discard,
                        HookResultMode::Observation,
                    ]),
                },
            )]),
            authored: EffectiveHookLayer {
                hooks: authored,
                dispatch_caps: vec!["ryeos.execute.tool.test/audit".to_string()],
            },
            builtin: empty_layer(),
            infrastructure: empty_layer(),
            context: empty_layer(),
            operator: empty_layer(),
            project: empty_layer(),
            sources: Vec::new(),
        }
    }

    fn view(composed: Value, authored: Vec<HookDefinition>) -> KindComposedView {
        KindComposedView {
            composed,
            derived: std::collections::HashMap::from([(
                "effective_hook_plan".to_string(),
                plan(authored).to_value().unwrap(),
            )]),
            policy_facts: std::collections::HashMap::from([(
                "effective_caps".to_string(),
                serde_json::json!(["ryeos.execute.tool.test/audit"]),
            )]),
        }
    }

    fn composed_graph() -> Value {
        serde_json::json!({
            "version": "1.0.0",
            "category": "test",
            "extends": "graph:test/base",
            "config": {
                "start": "inherited",
                "state": {"goal": "child", "attempt": 0},
                "nodes": {
                    "inherited": {
                        "next": {"type": "unconditional", "to": "finish"}
                    },
                    "finish": {"node_type": "return", "output": "${state.goal}"}
                }
            },
            "requires": {
                "capabilities": {
                    "declared": ["ryeos.execute.tool.test/audit"]
                }
            }
        })
    }

    fn validate_view(view: &KindComposedView) -> Result<EffectiveGraphValidation> {
        validate_effective_graph("graph:test/child", view, &["graph:test/base".to_string()])
    }

    #[test]
    fn validates_a_complete_inherited_effective_graph() {
        let summary = validate_view(&view(composed_graph(), Vec::new())).unwrap();
        assert_eq!(summary.summary.node_count, 2);
        assert_eq!(summary.summary.edge_count, 1);
        assert!(summary.summary.authored_hook_ids.is_empty());
    }

    #[test]
    fn rejects_cross_key_incoherence_after_shallow_merge() {
        let mut graph = composed_graph();
        graph["config"]["nodes"] = serde_json::json!({
            "finish": {"node_type": "return", "output": "done"}
        });

        let error = validate_view(&view(graph, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("config.start must name an existing node")
        );
    }

    #[test]
    fn rejects_runtime_only_topology_errors_during_admission() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["inherited"]["node_type"] = serde_json::json!("gate");
        let error = validate_view(&view(composed, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("conditional next"));
    }

    #[test]
    fn admits_follow_fanout_child_thread_collection() {
        let mut graph = composed_graph();
        graph["config"]["nodes"]["inherited"] = serde_json::json!({
            "follow": true,
            "over": "${state.jobs}",
            "as": "job",
            "parallel": true,
            "collect": "results",
            "collect_threads": "child_threads",
            "action": {
                "item_id": "graph:test/child",
                "ref_bindings": {},
                "params": {"job": "${job}"}
            },
            "next": {"type": "unconditional", "to": "finish"}
        });

        validate_view(&view(graph, Vec::new())).unwrap();
    }

    #[test]
    fn rejects_child_thread_collection_outside_follow_fanout() {
        let mut graph = composed_graph();
        graph["config"]["nodes"]["inherited"] = serde_json::json!({
            "node_type": "foreach",
            "over": "${state.jobs}",
            "as": "job",
            "parallel": true,
            "collect_threads": "child_threads",
            "action": {
                "item_id": "tool:test/audit",
                "ref_bindings": {},
                "params": {"job": "${job}"}
            },
            "next": {"type": "unconditional", "to": "finish"}
        });

        let error = validate_view(&view(graph, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("collect_threads requires follow fanout")
        );
    }

    #[test]
    fn rejects_colliding_follow_fanout_collection_keys() {
        let mut graph = composed_graph();
        graph["config"]["nodes"]["inherited"] = serde_json::json!({
            "follow": true,
            "over": "${state.jobs}",
            "as": "job",
            "parallel": true,
            "collect": "children",
            "collect_threads": "children",
            "action": {
                "item_id": "graph:test/child",
                "ref_bindings": {},
                "params": {"job": "${job}"}
            },
            "next": {"type": "unconditional", "to": "finish"}
        });

        let error = validate_view(&view(graph, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("for both collect_threads and collect")
        );
    }

    #[test]
    fn rejects_unavailable_expression_roots_during_admission() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["finish"]["output"] = serde_json::json!("${result.value}");
        let error = validate_view(&view(composed, Vec::new())).unwrap_err();
        assert!(
            format!("{error:#}").contains("expression root `result` is unavailable"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn run_step_is_admitted_everywhere_run_metadata_is_available() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["inherited"] = serde_json::json!({
            "action": {
                "item_id": "tool:test/audit",
                "ref_bindings": {},
                "params": {
                    "execution": "${execution}",
                    "step": "${run.step}"
                }
            },
            "assign": {
                "action_step": "${run.step}",
                "dispatch_source": "${dispatch.source}"
            },
            "next": {
                "type": "conditional",
                "branches": [
                    {"when": "run.step >= 0", "to": "finish"},
                    {"to": "finish"}
                ]
            }
        });
        composed["config"]["nodes"]["finish"]["output"] =
            serde_json::json!({"step": "${run.step}"});

        validate_view(&view(composed, Vec::new())).unwrap();
    }

    #[test]
    fn dispatch_remains_unavailable_in_action_templates() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["inherited"] = serde_json::json!({
            "action": {
                "item_id": "tool:test/audit",
                "ref_bindings": {},
                "params": {"source": "${dispatch.source}"}
            },
            "next": {"type": "unconditional", "to": "finish"}
        });

        let error = validate_view(&view(composed, Vec::new())).unwrap_err();
        assert!(format!("{error:#}").contains("expression root `dispatch` is unavailable"));
    }

    #[test]
    fn underscored_runtime_roots_are_not_compatibility_aliases() {
        for root in ["_execution", "_run", "_dispatch"] {
            let mut composed = composed_graph();
            composed["config"]["nodes"]["finish"]["output"] =
                serde_json::json!(format!("${{{root}}}"));

            let error = validate_view(&view(composed, Vec::new())).unwrap_err();
            assert!(
                format!("{error:#}").contains(&format!("expression root `{root}` is unavailable")),
                "unexpected error for {root}: {error:#}"
            );
        }
    }

    #[test]
    fn rejects_runtime_reserved_iteration_variable_during_admission() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["inherited"] = serde_json::json!({
            "node_type": "foreach",
            "action": {"item_id": "tool:test/audit"},
            "over": "${state.goal}",
            "as": "dispatch",
            "next": {"type": "unconditional", "to": "finish"}
        });
        let error = validate_view(&view(composed, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("reserved by rye-expr/1"));
    }

    #[test]
    fn extends_provenance_is_part_of_kind_admission() {
        let view = view(composed_graph(), Vec::new());
        let missing = validate_effective_graph("graph:test/child", &view, &[]).unwrap_err();
        assert!(missing.to_string().contains("has no admitted ancestor"));

        let wrong =
            validate_effective_graph("graph:test/child", &view, &["graph:test/other".to_string()])
                .unwrap_err();
        assert!(wrong.to_string().contains("provenance mismatch"));
    }

    #[test]
    fn rejects_unknown_nested_capability_fields_during_strict_decode() {
        let mut composed = composed_graph();
        composed["requires"]["capabilities"]["undeclared"] = serde_json::json!([]);
        let error = validate_view(&view(composed, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("strictly decode composed graph"));
    }

    #[test]
    fn rejects_captured_authored_hook_divergence() {
        let authored = HookDefinition {
            id: "audit".to_string(),
            event: "graph_started".to_string(),
            result: HookResultMode::Observation,
            condition: ExpressionCondition::Absent,
            action: serde_json::json!({"item_id": "tool:test/audit"}),
        };
        let mut graph = composed_graph();
        graph["config"]["hooks"] = serde_json::to_value([authored.clone()]).unwrap();

        let error = validate_view(&view(graph, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("captured authored hook layer differs")
        );

        validate_view(&view(
            composed_graph_with_hook(authored.clone()),
            vec![authored],
        ))
        .unwrap();
    }

    fn composed_graph_with_hook(hook: HookDefinition) -> Value {
        let mut graph = composed_graph();
        graph["config"]["hooks"] = serde_json::to_value([hook]).unwrap();
        graph
    }

    #[test]
    fn durable_effect_classes_are_declared_and_bounded() {
        // A plain action node may declare a durable class; the declaration
        // lives in the composed value and therefore in the digest.
        let mut graph = composed_graph();
        graph["config"]["nodes"]["inherited"]["action"] =
            serde_json::json!({"item_id": "tool:test/audit"});
        graph["config"]["nodes"]["inherited"]["effects"] = serde_json::json!("recorded");
        validate_view(&view(graph, Vec::new())).unwrap();

        // Absent means live: cross-run replay is never implied.
        let file: GraphFile = serde_json::from_value(composed_graph()).unwrap();
        assert!(file.config.nodes["inherited"].effects.is_live());

        // Shapes whose results cannot exist at publication time refuse the
        // declaration outright.
        for (patch, expect) in [
            (
                serde_json::json!({
                    "node_type": "foreach",
                    "action": {"item_id": "tool:test/audit"},
                    "over": "state.goal",
                    "as": "item",
                    "effects": "recorded",
                    "next": {"type": "unconditional", "to": "finish"}
                }),
                "aggregate dispatch",
            ),
            (
                serde_json::json!({
                    "action": {"item_id": "tool:test/audit"},
                    "follow": true,
                    "effects": "sealed",
                    "next": {"type": "unconditional", "to": "finish"}
                }),
                "child settles",
            ),
            (
                serde_json::json!({
                    "action": {"item_id": "tool:test/audit"},
                    "detach": true,
                    "effects": "recorded",
                    "next": {"type": "unconditional", "to": "finish"}
                }),
                "launch time",
            ),
        ] {
            let mut graph = composed_graph();
            graph["config"]["nodes"]["inherited"] = patch;
            let error = validate_view(&view(graph, Vec::new())).unwrap_err();
            assert!(error.to_string().contains(expect), "unexpected: {error:#}");
        }
    }
}
