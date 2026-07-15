use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use ryeos_runtime::events::RuntimeEventType;
use ryeos_runtime::{
    compile_condition_for, compile_hooks, compile_template_for, CompilationLimits,
    CompiledActionTemplate, CompiledExpression, CompiledHook, CompiledJsonTemplate,
    CompiledTemplate, ExpressionCondition, HookContextSchema, HookSources, Reference,
    ReferenceSegment, ReferenceSet,
};

use crate::model::{EdgeSpec, GraphConfig, GraphNode, NodeType};

#[derive(Debug, Clone)]
pub(crate) struct CompiledGraph {
    nodes: HashMap<String, CompiledNode>,
    hooks: Vec<CompiledHook>,
}

impl CompiledGraph {
    pub(crate) fn compile(config: &GraphConfig, mut hook_sources: HookSources) -> Result<Self> {
        let limits = CompilationLimits::default();
        if let Some(state) = config.state.as_ref() {
            crate::evaluation::validate_runtime_value(state, "config.state")
                .context("validate authored graph state bounds")?;
        }
        let input_properties = config
            .config_schema
            .as_ref()
            .and_then(|schema| schema.get("properties"))
            .and_then(|properties| properties.as_object())
            .map(|properties| properties.keys().map(String::as_str).collect::<HashSet<_>>());
        let mut nodes = HashMap::with_capacity(config.nodes.len());

        for (name, node) in &config.nodes {
            validate_iteration_variable(name, node)?;
            let compiled = CompiledNode::compile(
                name,
                node,
                &limits,
                input_properties.as_ref(),
            )
            .with_context(|| format!("compile expressions for graph node `{name}`"))?;
            nodes.insert(name.clone(), compiled);
        }

        hook_sources.retain_configured_events(&[
            RuntimeEventType::GraphStarted.as_str(),
            RuntimeEventType::GraphStepCompleted.as_str(),
            RuntimeEventType::GraphCompleted.as_str(),
        ]);
        let hooks = compile_hooks(hook_sources, &graph_hook_context_schemas(), &limits)
            .context("compile graph hooks")?;
        for (index, hook) in hooks.iter().enumerate() {
            let field = format!("hook[{index}] (id={})", hook.id());
            for reference in hook.references().iter() {
                validate_input_reference(&field, reference, input_properties.as_ref())?;
            }
        }

        Ok(Self { nodes, hooks })
    }

    pub(crate) fn node(&self, name: &str) -> &CompiledNode {
        self.nodes
            .get(name)
            .unwrap_or_else(|| panic!("compiled graph missing source node `{name}`"))
    }

    pub(crate) fn hooks(&self) -> &[CompiledHook] {
        &self.hooks
    }

    pub(crate) fn references(&self) -> impl Iterator<Item = &Reference> {
        self.nodes
            .values()
            .flat_map(|node| node.references.iter())
            .chain(
                self.hooks
                    .iter()
                    .flat_map(|hook| hook.references().iter()),
            )
    }
}

fn graph_hook_context_schemas() -> [HookContextSchema; 3] {
    [
        HookContextSchema::new(
            RuntimeEventType::GraphStarted.as_str(),
            ["event", "graph_id", "graph_run_id", "state", "inputs"],
        ),
        HookContextSchema::new(
            RuntimeEventType::GraphStepCompleted.as_str(),
            [
                "event",
                "graph_id",
                "graph_run_id",
                "node",
                "step",
                "status",
                "state",
                "error",
            ],
        ),
        HookContextSchema::new(
            RuntimeEventType::GraphCompleted.as_str(),
            [
                "event",
                "graph_id",
                "graph_run_id",
                "status",
                "settled",
                "steps",
                "success",
                "state",
                "inputs",
            ],
        ),
    ]
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledNode {
    pub(crate) action: Option<CompiledActionTemplate>,
    pub(crate) assign: Option<CompiledJsonTemplate>,
    pub(crate) output: Option<CompiledJsonTemplate>,
    pub(crate) over: Option<CompiledTemplate>,
    pub(crate) facets: Option<CompiledJsonTemplate>,
    pub(crate) next: Option<CompiledEdgeSpec>,
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
        let state_roots = allowed_roots(false, None);
        let action_roots = allowed_roots(false, foreach_root);
        let result_available = node.action.is_some();
        let assign_roots = allowed_roots(result_available, foreach_root);
        let action_condition_roots = allowed_roots(result_available, None);

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
                Ok(compiled)
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
                Ok(compiled)
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
                Ok(compiled)
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
                Ok(compiled)
            })
            .transpose()?;

        // A detached node folds facets into the compiled callback action. A
        // follow-fanout stamps them separately, so compile them as a standalone
        // tree as well and retain them on the action-side root contract.
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
                Ok(compiled)
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
                CompiledEdgeSpec::compile(
                    name,
                    source,
                    condition_roots,
                    input_properties,
                    limits,
                )
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
pub(crate) enum CompiledEdgeSpec {
    Unconditional { to: String },
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
                                bail!("{field}: conditional edge contains more than one default branch");
                            }
                            default_seen = true;
                            CompiledCondition::Default
                        }
                        ExpressionCondition::Boolean(value) => {
                            CompiledCondition::Constant(*value)
                        }
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

    pub(crate) fn references(&self) -> &ReferenceSet {
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
pub(crate) struct CompiledConditionalEdge {
    pub(crate) condition: CompiledCondition,
    pub(crate) to: String,
}

#[derive(Debug, Clone)]
pub(crate) enum CompiledCondition {
    Default,
    Constant(bool),
    Expression(CompiledExpression),
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
            bail!(
                "{field}: expression root `{}` is not available here; allowed roots are {}",
                reference.root(),
                sorted_roots(allowed_roots).join(", ")
            );
        }
        if matches!(
            reference.root(),
            "state" | "inputs" | "_execution" | "_run"
        ) && matches!(reference.segments().first(), Some(ReferenceSegment::Index(_)))
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
        // Bare `inputs` and dynamic indexing cannot be resolved statically.
        return Ok(());
    };
    if !properties.contains(key.as_str()) {
        bail!(
            "{field}: input `{key}` is not declared in config.config_schema.properties"
        );
    }
    Ok(())
}

fn sorted_roots(roots: &HashSet<&str>) -> Vec<&str> {
    let mut roots = roots.iter().copied().collect::<Vec<_>>();
    roots.sort_unstable();
    roots
}

fn validate_iteration_variable(node_name: &str, node: &GraphNode) -> Result<()> {
    let Some(variable) = node.r#as.as_deref() else {
        return Ok(());
    };
    let mut bytes = variable.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    let valid_rest = bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid_start || !valid_rest {
        bail!(
            "node `{node_name}` iteration variable `{variable}` must match [A-Za-z_][A-Za-z0-9_]*"
        );
    }
    if matches!(
        variable,
        "true" | "false" | "null" | "in" | "state" | "inputs" | "result" | "_execution" | "_run"
    ) {
        bail!(
            "node `{node_name}` iteration variable `{variable}` is reserved by rye-expr/1"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::{ExpressionCondition, HookDefinition, HookLayer, HookSources};
    use serde_json::json;

    fn hook(id: &str) -> HookDefinition {
        HookDefinition {
            id: id.to_string(),
            event: "graph_started".to_string(),
            condition: ExpressionCondition::Absent,
            action: json!({"item_id": "tool:test/noop"}),
        }
    }

    #[test]
    fn graph_compiles_all_layers_and_drops_raw_hook_definitions() {
        let raw = r#"
version: "1.0.0"
category: test
config:
  start: done
  hooks:
    - {id: authored, event: graph_started, action: {item_id: "tool:test/noop"}}
  nodes:
    done: {node_type: return}
"#;
        let graph = crate::model::GraphDefinition::from_yaml_with_hook_sources(
            raw,
            None,
            HookSources {
                builtin: vec![hook("builtin")],
                infrastructure: vec![hook("infra")],
                context: vec![hook("context")],
                operator: vec![hook("operator")],
                project: vec![hook("project")],
                ..HookSources::default()
            },
        )
        .unwrap();

        assert!(graph.config.hooks.is_empty());
        assert_eq!(
            graph
                .compiled
                .hooks()
                .iter()
                .map(CompiledHook::layer)
                .collect::<Vec<_>>(),
            vec![
                HookLayer::Authored,
                HookLayer::Builtin,
                HookLayer::Infrastructure,
                HookLayer::Context,
                HookLayer::Operator,
                HookLayer::Project,
            ]
        );
    }

    #[test]
    fn graph_rejects_statically_non_boolean_condition() {
        let raw = r#"
version: "1.0.0"
category: test
config:
  start: choose
  nodes:
    choose:
      node_type: gate
      next:
        branches:
          - {when: "1", to: done}
    done: {node_type: return}
"#;

        let error = crate::model::GraphDefinition::from_yaml(raw, None).unwrap_err();

        assert!(error.to_string().contains("expected bool"));
    }

    #[test]
    fn known_object_context_roots_reject_numeric_first_segment() {
        for expression in [
            "inputs[0] == null",
            "state[0] == null",
            "_run[0] == null",
            "_execution[0] == null",
        ] {
            let raw = format!(
                r#"
version: "1.0.0"
category: test
config:
  start: choose
  nodes:
    choose:
      node_type: gate
      next:
        branches:
          - when: '{expression}'
            to: done
    done: {{node_type: return}}
"#
            );

            let error = crate::model::GraphDefinition::from_yaml(&raw, None).unwrap_err();
            assert!(
                error.to_string().contains("cannot be indexed by number"),
                "{expression}: {error}"
            );
        }
    }
}
