//! Side-effect-free validation of a completely composed graph definition.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use ryeos_engine::hooks::{EffectiveHookPlan, ExpressionCondition, HookDefinition};
use ryeos_engine::resolution::KindComposedView;
use ryeos_runtime::{
    CompilationLimits, CompiledActionTemplate, CompiledJsonTemplate, EvaluationContext,
    EvaluationLimits, EvaluationSession, Reference, ReferenceSegment, ReferenceSet,
    compile_condition_for, compile_effective_hook_plan, compile_template_for,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GRAPH_EFFECTIVE_VALIDATION_SCHEMA: &str = "ryeos.graph.effective_validation.v1";
pub const MAX_GRAPH_STEPS: u32 = 500;
pub const MAX_GRAPH_SEGMENT_STEPS: u32 = MAX_GRAPH_STEPS;
pub const MAX_RETRY_BACKOFF_MS: u64 = 300_000;
pub const MAX_NODE_CONCURRENCY: usize = 256;

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub config_schema: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
    #[serde(default)]
    pub state: Option<Value>,
    #[serde(default)]
    pub segment_steps: Option<u32>,
}

fn default_max_steps() -> u32 {
    100
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorMode {
    #[default]
    Fail,
    Continue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNode {
    #[serde(default)]
    pub node_type: NodeType,
    #[serde(default)]
    pub action: Option<Value>,
    #[serde(default)]
    pub assign: Option<Value>,
    #[serde(default)]
    pub next: Option<EdgeSpec>,
    #[serde(default)]
    pub on_error: Option<String>,
    #[serde(default)]
    pub cache_result: bool,
    #[serde(default)]
    pub follow: bool,
    #[serde(default)]
    pub detach: bool,
    #[serde(default)]
    pub facets: Option<Value>,
    #[serde(default)]
    pub over: Option<String>,
    #[serde(default)]
    pub r#as: Option<String>,
    #[serde(default)]
    pub collect: Option<String>,
    #[serde(default)]
    pub parallel: bool,
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub env_requires: Vec<String>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    #[default]
    Action,
    Return,
    Foreach,
    Gate,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EdgeSpec {
    Unconditional { to: String },
    Conditional { branches: Vec<ConditionalEdge> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalEdge {
    #[serde(default)]
    pub when: ExpressionCondition,
    pub to: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    pub attempts: u32,
    pub backoff_ms: u64,
    #[serde(default)]
    pub max_backoff_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationSummary {
    pub schema: String,
    pub node_count: usize,
    pub edge_count: usize,
    pub authored_hook_ids: Vec<String>,
}

/// Decode and validate the exact composed graph and captured hook plan.
pub fn validate_effective_graph(view: &KindComposedView) -> Result<ValidationSummary> {
    let file: GraphFile =
        serde_json::from_value(view.composed.clone()).context("strictly decode composed graph")?;
    validate_graph_file(&file)?;

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
    validate_compilation(&file.config, &plan)?;
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
    Ok(ValidationSummary {
        schema: GRAPH_EFFECTIVE_VALIDATION_SCHEMA.to_string(),
        node_count: file.config.nodes.len(),
        edge_count,
        authored_hook_ids: file
            .config
            .hooks
            .iter()
            .map(|hook| hook.id.clone())
            .collect(),
    })
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
        && (node.over.is_some() || node.r#as.is_some() || node.collect.is_some() || node.parallel)
    {
        bail!("node `{name}` declares iteration fields without foreach or follow fanout");
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
    Ok(())
}

fn validate_target(name: &str, target: &str, config: &GraphConfig) -> Result<()> {
    if target.is_empty() || !config.nodes.contains_key(target) {
        bail!("node `{name}` targets missing node `{target}`");
    }
    Ok(())
}

/// Compile every expression/template and the complete admitted hook plan at
/// admission. The runtime repeats this work defensively, but no authored
/// compilation failure is allowed to survive until process spawn.
fn validate_compilation(config: &GraphConfig, plan: &EffectiveHookPlan) -> Result<()> {
    let limits = CompilationLimits::default();
    if let Some(state) = config.state.as_ref() {
        EvaluationSession::with_context(&EvaluationContext::new(), &EvaluationLimits::default())
            .validate_value(state, "config.state")
            .map_err(|error| anyhow!(error))?;
    }
    let input_properties = config
        .config_schema
        .as_ref()
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .keys()
                .map(String::as_str)
                .collect::<HashSet<_>>()
        });

    for (name, node) in &config.nodes {
        validate_iteration_variable(name, node)?;
        compile_node(name, node, &limits, input_properties.as_ref())?;
    }

    let hooks = compile_effective_hook_plan(plan, &limits)
        .map_err(|error| anyhow!("compile captured effective graph hooks: {error}"))?;
    for (index, hook) in hooks.iter().enumerate() {
        let field = format!("hook[{index}] (id={})", hook.id());
        for reference in hook.references().iter() {
            validate_input_reference(&field, reference, input_properties.as_ref())?;
        }
    }
    Ok(())
}

fn compile_node(
    name: &str,
    node: &GraphNode,
    limits: &CompilationLimits,
    input_properties: Option<&HashSet<&str>>,
) -> Result<()> {
    let foreach_root = if node.node_type == NodeType::Foreach
        || (node.node_type == NodeType::Action && node.follow && node.over.is_some())
    {
        node.r#as.as_deref()
    } else {
        None
    };
    let state_roots = allowed_roots(false, None);
    let action_roots = allowed_roots(false, foreach_root);
    let assign_roots = allowed_roots(node.action.is_some(), foreach_root);
    let action_condition_roots = allowed_roots(node.action.is_some(), None);

    if let Some(source) = &node.action {
        let mut source = source.clone();
        if node.detach
            && let Some(action) = source.as_object_mut()
        {
            action.insert("thread".to_string(), Value::String("detached".to_string()));
            if let Some(facets) = &node.facets {
                action.insert("facets".to_string(), facets.clone());
            }
        }
        let field = format!("node {name}.action");
        let compiled = CompiledActionTemplate::compile(&source, &field, limits)
            .map_err(|error| anyhow!(error))?;
        validate_references(
            &field,
            compiled.references(),
            &action_roots,
            input_properties,
        )?;
    }
    if let Some(source) = &node.assign {
        let field = format!("node {name}.assign");
        let compiled = CompiledJsonTemplate::compile(source, &field, limits)
            .map_err(|error| anyhow!(error))?;
        validate_references(
            &field,
            compiled.references(),
            &assign_roots,
            input_properties,
        )?;
    }
    if let Some(source) = &node.output {
        let field = format!("node {name}.output");
        let compiled = CompiledJsonTemplate::compile(source, &field, limits)
            .map_err(|error| anyhow!(error))?;
        validate_references(
            &field,
            compiled.references(),
            &state_roots,
            input_properties,
        )?;
    }
    if let Some(source) = &node.over {
        let field = format!("node {name}.over");
        let compiled =
            compile_template_for(source, &field, limits).map_err(|error| anyhow!(error))?;
        validate_references(
            &field,
            compiled.references(),
            &state_roots,
            input_properties,
        )?;
    }
    if let Some(source) = &node.facets {
        let field = format!("node {name}.facets");
        let compiled = CompiledJsonTemplate::compile(source, &field, limits)
            .map_err(|error| anyhow!(error))?;
        validate_references(
            &field,
            compiled.references(),
            &action_roots,
            input_properties,
        )?;
    }

    if let Some(EdgeSpec::Conditional { branches }) = &node.next {
        let roots = if node.node_type == NodeType::Action {
            &action_condition_roots
        } else {
            &state_roots
        };
        for (index, branch) in branches.iter().enumerate() {
            if let ExpressionCondition::Expression(source) = &branch.when {
                let field = format!("node {name}.next.branches[{index}].when");
                let compiled = compile_condition_for(source, &field, limits)
                    .map_err(|error| anyhow!(error))?;
                validate_references(&field, compiled.references(), roots, input_properties)?;
            }
        }
    }
    Ok(())
}

fn allowed_roots(include_result: bool, foreach_root: Option<&str>) -> HashSet<&str> {
    let mut roots = HashSet::from(["state", "inputs", "_execution", "_run"]);
    if include_result {
        roots.insert("result");
    }
    if let Some(root) = foreach_root {
        roots.insert(root);
    }
    roots
}

fn validate_references(
    field: &str,
    references: &ReferenceSet,
    allowed_roots: &HashSet<&str>,
    input_properties: Option<&HashSet<&str>>,
) -> Result<()> {
    for reference in references.iter() {
        if !allowed_roots.contains(reference.root()) {
            let mut roots = allowed_roots.iter().copied().collect::<Vec<_>>();
            roots.sort_unstable();
            bail!(
                "{field}: expression root `{}` is unavailable; allowed roots are {}",
                reference.root(),
                roots.join(", ")
            );
        }
        if matches!(reference.root(), "state" | "inputs" | "_execution" | "_run")
            && matches!(
                reference.segments().first(),
                Some(ReferenceSegment::Index(_))
            )
        {
            bail!(
                "{field}: expression root `{}` is an object and cannot be indexed by number",
                reference.root()
            );
        }
        validate_input_reference(field, reference, input_properties)?;
    }
    Ok(())
}

fn validate_input_reference(
    field: &str,
    reference: &Reference,
    input_properties: Option<&HashSet<&str>>,
) -> Result<()> {
    if reference.root() != "inputs" {
        return Ok(());
    }
    let Some(properties) = input_properties else {
        return Ok(());
    };
    let Some(ReferenceSegment::Key(key)) = reference.segments().first() else {
        return Ok(());
    };
    if !properties.contains(key.as_str()) {
        bail!("{field}: input `{key}` is not declared in config.config_schema.properties");
    }
    Ok(())
}

fn validate_iteration_variable(name: &str, node: &GraphNode) -> Result<()> {
    let Some(variable) = node.r#as.as_deref() else {
        return Ok(());
    };
    let mut bytes = variable.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    let valid_rest = bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_start || !valid_rest {
        bail!("node `{name}` iteration variable `{variable}` has an invalid rye-expr name");
    }
    if matches!(
        variable,
        "true" | "false" | "null" | "in" | "state" | "inputs" | "result" | "_execution" | "_run"
    ) {
        bail!("node `{name}` iteration variable `{variable}` is reserved by rye-expr/1");
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

    #[test]
    fn validates_a_complete_inherited_effective_graph() {
        let summary = validate_effective_graph(&view(composed_graph(), Vec::new())).unwrap();
        assert_eq!(summary.node_count, 2);
        assert_eq!(summary.edge_count, 1);
        assert!(summary.authored_hook_ids.is_empty());
    }

    #[test]
    fn rejects_cross_key_incoherence_after_shallow_merge() {
        let mut graph = composed_graph();
        graph["config"]["nodes"] = serde_json::json!({
            "finish": {"node_type": "return", "output": "done"}
        });

        let error = validate_effective_graph(&view(graph, Vec::new())).unwrap_err();
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
        let error = validate_effective_graph(&view(composed, Vec::new())).unwrap_err();
        assert!(error.to_string().contains("conditional next"));
    }

    #[test]
    fn rejects_unavailable_expression_roots_during_admission() {
        let mut composed = composed_graph();
        composed["config"]["nodes"]["finish"]["output"] = serde_json::json!("${result.value}");
        let error = validate_effective_graph(&view(composed, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expression root `result` is unavailable")
        );
    }

    #[test]
    fn rejects_unknown_nested_capability_fields_during_strict_decode() {
        let mut composed = composed_graph();
        composed["requires"]["capabilities"]["undeclared"] = serde_json::json!([]);
        let error = validate_effective_graph(&view(composed, Vec::new())).unwrap_err();
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

        let error = validate_effective_graph(&view(graph, Vec::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("captured authored hook layer differs")
        );

        validate_effective_graph(&view(
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
}
