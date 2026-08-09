use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result, bail};
use ryeos_engine::hooks::EffectiveHookPlan;
use ryeos_runtime::{
    CompilationLimits, CompiledActionTemplate, CompiledExpression, CompiledHook,
    CompiledJsonTemplate, CompiledTemplate, EvaluationContext, EvaluationLimits, EvaluationSession,
    ExpressionCondition, Reference, ReferenceSegment, ReferenceSet, compile_condition_for,
    compile_effective_hook_plan, compile_template_for,
};

use crate::{EdgeSpec, GraphConfig, GraphNode, NodeType};

/// Immutable executable sidecar produced by the graph kind's one admission
/// compiler. It is transient process state, never a second serialized graph
/// contract.
#[derive(Debug, Clone)]
pub struct CompiledGraph {
    nodes: HashMap<String, CompiledNode>,
    hooks: Vec<CompiledHook>,
}

impl CompiledGraph {
    pub(crate) fn compile_effective(
        config: &GraphConfig,
        plan: &EffectiveHookPlan,
    ) -> Result<Self> {
        let limits = CompilationLimits::default();
        if let Some(state) = config.state.as_ref() {
            EvaluationSession::with_context(
                &EvaluationContext::new(),
                &EvaluationLimits::default(),
            )
            .validate_value(state, "config.state")
            .map_err(|error| anyhow::anyhow!(error))
            .context("validate authored graph state bounds")?;
        }
        let input_properties = config
            .config_schema
            .as_ref()
            .and_then(|schema| schema.get("properties"))
            .and_then(|properties| properties.as_object())
            .map(|properties| {
                properties
                    .keys()
                    .map(String::as_str)
                    .collect::<HashSet<_>>()
            });
        let mut nodes = HashMap::with_capacity(config.nodes.len());
        for (name, node) in &config.nodes {
            validate_iteration_variable(name, node)?;
            let compiled = CompiledNode::compile(name, node, &limits, input_properties.as_ref())
                .with_context(|| format!("compile expressions for graph node `{name}`"))?;
            nodes.insert(name.clone(), compiled);
        }

        let hooks = compile_effective_hook_plan(plan, &limits)
            .context("compile captured effective graph hooks")?;
        for (index, hook) in hooks.iter().enumerate() {
            let field = format!("hook[{index}] (id={})", hook.id());
            for reference in hook.references().iter() {
                validate_input_reference(&field, reference, input_properties.as_ref())?;
            }
        }
        Ok(Self { nodes, hooks })
    }

    pub fn node(&self, name: &str) -> &CompiledNode {
        self.nodes
            .get(name)
            .unwrap_or_else(|| panic!("compiled graph missing source node `{name}`"))
    }

    pub fn hooks(&self) -> &[CompiledHook] {
        &self.hooks
    }

    pub fn references(&self) -> impl Iterator<Item = &Reference> {
        self.nodes
            .values()
            .flat_map(|node| node.references.iter())
            .chain(self.hooks.iter().flat_map(|hook| hook.references().iter()))
    }
}

#[derive(Debug, Clone)]
pub struct CompiledNode {
    pub action: Option<CompiledActionTemplate>,
    pub assign: Option<CompiledJsonTemplate>,
    pub output: Option<CompiledJsonTemplate>,
    pub over: Option<CompiledTemplate>,
    pub facets: Option<CompiledJsonTemplate>,
    pub next: Option<CompiledEdgeSpec>,
    references: ReferenceSet,
}

impl CompiledNode {
    fn compile(
        name: &str,
        node: &GraphNode,
        limits: &CompilationLimits,
        input_properties: Option<&HashSet<&str>>,
    ) -> Result<Self> {
        let foreach_root = if node.node_type == NodeType::Foreach
            || (node.node_type == NodeType::Action && node.follow && node.over.is_some())
        {
            node.r#as.as_deref()
        } else {
            None
        };
        let state_roots = allowed_roots(false, false, None);
        // `_dispatch` becomes available only after this node's action has
        // returned daemon-owned dispatch evidence.
        let action_roots = allowed_roots(false, false, foreach_root);
        let result_available = node.action.is_some();
        let assign_roots = allowed_roots(result_available, result_available, foreach_root);
        let action_condition_roots = allowed_roots(result_available, result_available, None);

        let action = node
            .action
            .as_ref()
            .map(|source| {
                let mut source = source.clone();
                node.fold_detach_into_action(&mut source);
                let field = format!("node {name}.action");
                let compiled = CompiledActionTemplate::compile(&source, field.clone(), limits)?;
                validate_references(
                    &field,
                    compiled.references(),
                    &action_roots,
                    input_properties,
                )?;
                Ok::<_, anyhow::Error>(compiled)
            })
            .transpose()?;
        let assign = node
            .assign
            .as_ref()
            .map(|source| {
                let field = format!("node {name}.assign");
                let compiled = CompiledJsonTemplate::compile(source, field.clone(), limits)?;
                validate_references(
                    &field,
                    compiled.references(),
                    &assign_roots,
                    input_properties,
                )?;
                Ok::<_, anyhow::Error>(compiled)
            })
            .transpose()?;
        let output = node
            .output
            .as_ref()
            .map(|source| {
                let field = format!("node {name}.output");
                let compiled = CompiledJsonTemplate::compile(source, field.clone(), limits)?;
                validate_references(
                    &field,
                    compiled.references(),
                    &state_roots,
                    input_properties,
                )?;
                Ok::<_, anyhow::Error>(compiled)
            })
            .transpose()?;
        let over = node
            .over
            .as_ref()
            .map(|source| {
                let field = format!("node {name}.over");
                let compiled = compile_template_for(source, field.clone(), limits)?;
                validate_references(
                    &field,
                    compiled.references(),
                    &state_roots,
                    input_properties,
                )?;
                Ok::<_, anyhow::Error>(compiled)
            })
            .transpose()?;
        let facets = node
            .facets
            .as_ref()
            .map(|source| {
                let field = format!("node {name}.facets");
                let compiled = CompiledJsonTemplate::compile(source, field.clone(), limits)?;
                validate_references(
                    &field,
                    compiled.references(),
                    &action_roots,
                    input_properties,
                )?;
                Ok::<_, anyhow::Error>(compiled)
            })
            .transpose()?;

        let condition_roots = match node.node_type {
            NodeType::Gate => &state_roots,
            NodeType::Action => &action_condition_roots,
            NodeType::Foreach | NodeType::Return => &state_roots,
        };
        let next = node
            .next
            .as_ref()
            .map(|source| {
                CompiledEdgeSpec::compile(name, source, condition_roots, input_properties, limits)
            })
            .transpose()?;

        let mut references = ReferenceSet::default();
        for set in [
            action.as_ref().map(CompiledActionTemplate::references),
            assign.as_ref().map(CompiledJsonTemplate::references),
            output.as_ref().map(CompiledJsonTemplate::references),
            over.as_ref().map(CompiledTemplate::references),
            facets.as_ref().map(CompiledJsonTemplate::references),
            next.as_ref().map(CompiledEdgeSpec::references),
        ]
        .into_iter()
        .flatten()
        {
            references.extend(set);
        }

        Ok(Self {
            action,
            assign,
            output,
            over,
            facets,
            next,
            references,
        })
    }
}

#[derive(Debug, Clone)]
pub enum CompiledEdgeSpec {
    Unconditional {
        to: String,
    },
    Conditional {
        branches: Vec<CompiledConditionalEdge>,
        references: ReferenceSet,
    },
}

impl CompiledEdgeSpec {
    fn compile(
        node: &str,
        source: &EdgeSpec,
        allowed_roots: &HashSet<&str>,
        input_properties: Option<&HashSet<&str>>,
        limits: &CompilationLimits,
    ) -> Result<Self> {
        match source {
            EdgeSpec::Unconditional { to } => Ok(Self::Unconditional { to: to.clone() }),
            EdgeSpec::Conditional { branches } => {
                let mut compiled = Vec::with_capacity(branches.len());
                let mut references = ReferenceSet::default();
                let mut default_seen = false;
                for (index, branch) in branches.iter().enumerate() {
                    let field = format!("node {node}.next.branches[{index}].when");
                    let condition = match &branch.when {
                        ExpressionCondition::Absent => {
                            if default_seen {
                                bail!(
                                    "{field}: conditional edge contains more than one default branch"
                                );
                            }
                            default_seen = true;
                            CompiledCondition::Default
                        }
                        ExpressionCondition::Boolean(value) => CompiledCondition::Constant(*value),
                        ExpressionCondition::Expression(source) => {
                            let expression = compile_condition_for(source, field.clone(), limits)?;
                            validate_references(
                                &field,
                                expression.references(),
                                allowed_roots,
                                input_properties,
                            )?;
                            references.extend(expression.references());
                            CompiledCondition::Expression(expression)
                        }
                    };
                    compiled.push(CompiledConditionalEdge {
                        condition,
                        to: branch.to.clone(),
                    });
                }
                Ok(Self::Conditional {
                    branches: compiled,
                    references,
                })
            }
        }
    }

    fn references(&self) -> &ReferenceSet {
        match self {
            Self::Unconditional { .. } => empty_references(),
            Self::Conditional { references, .. } => references,
        }
    }
}

fn empty_references() -> &'static ReferenceSet {
    static EMPTY: std::sync::OnceLock<ReferenceSet> = std::sync::OnceLock::new();
    EMPTY.get_or_init(ReferenceSet::default)
}

#[derive(Debug, Clone)]
pub struct CompiledConditionalEdge {
    pub condition: CompiledCondition,
    pub to: String,
}

#[derive(Debug, Clone)]
pub enum CompiledCondition {
    Default,
    Constant(bool),
    Expression(CompiledExpression),
}

fn allowed_roots(
    include_result: bool,
    include_dispatch: bool,
    foreach_root: Option<&str>,
) -> HashSet<&str> {
    let mut roots = HashSet::from(["state", "inputs", "_execution", "_run"]);
    if include_result {
        roots.insert("result");
    }
    if include_dispatch {
        roots.insert("_dispatch");
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
        if matches!(
            reference.root(),
            "state" | "inputs" | "_execution" | "_run" | "_dispatch"
        ) && matches!(
            reference.segments().first(),
            Some(ReferenceSegment::Index(_))
        ) {
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
        "true"
            | "false"
            | "null"
            | "in"
            | "state"
            | "inputs"
            | "result"
            | "_execution"
            | "_run"
            | "_dispatch"
    ) {
        bail!("node `{name}` iteration variable `{variable}` is reserved by rye-expr/1");
    }
    Ok(())
}
