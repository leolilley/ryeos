use std::collections::{BTreeSet, HashMap};
use std::fmt;

use serde_json::Value;

use crate::compiled_template::{CompiledActionTemplate, CompiledTemplateError};
use crate::expression::{
    CompilationLimits, CompiledExpression, EvaluationSession, ExpressionError, ReferenceSet,
    compile_condition_for,
};

pub use ryeos_engine::hooks::{ExpressionCondition, HookDefinition, HookLayer, HookResultMode};

/// Every hook source modeled by the runtime. The runtime owner fills
/// `authored`; the remaining layers come from verified configured roots.
#[derive(Debug, Clone, Default)]
pub struct HookSources {
    pub authored: Vec<HookDefinition>,
    pub builtin: Vec<HookDefinition>,
    pub infrastructure: Vec<HookDefinition>,
    pub context: Vec<HookDefinition>,
    pub operator: Vec<HookDefinition>,
    pub project: Vec<HookDefinition>,
}

impl HookSources {
    /// Project shared configured layers onto the events owned by one runtime.
    /// Authored hooks are deliberately retained so an unsupported authored
    /// event fails compilation instead of disappearing silently.
    pub fn retain_configured_events(&mut self, events: &[&str]) {
        let retain = |hook: &HookDefinition| events.contains(&hook.event.as_str());
        self.builtin.retain(retain);
        self.infrastructure.retain(retain);
        self.context.retain(retain);
        self.operator.retain(retain);
        self.project.retain(retain);
    }

    fn into_layered(self) -> Vec<LayeredHookDefinition> {
        let mut layered = Vec::new();
        for (layer, hooks) in [
            (HookLayer::Authored, self.authored),
            (HookLayer::Builtin, self.builtin),
            (HookLayer::Infrastructure, self.infrastructure),
            (HookLayer::Context, self.context),
            (HookLayer::Operator, self.operator),
            (HookLayer::Project, self.project),
        ] {
            layered.extend(
                hooks
                    .into_iter()
                    .map(|definition| LayeredHookDefinition { layer, definition }),
            );
        }
        layered
    }

    pub fn from_effective_plan(plan: &ryeos_engine::hooks::EffectiveHookPlan) -> Self {
        Self {
            authored: plan.authored.hooks.clone(),
            builtin: plan.builtin.hooks.clone(),
            infrastructure: plan.infrastructure.hooks.clone(),
            context: plan.context.hooks.clone(),
            operator: plan.operator.hooks.clone(),
            project: plan.project.hooks.clone(),
        }
    }
}

struct LayeredHookDefinition {
    layer: HookLayer,
    definition: HookDefinition,
}

/// The only context roots a hook event is allowed to observe. Runtime owners
/// supply these schemas when source layers have been merged and hooks are
/// compiled; privileged hook layers do not receive undeclared ambient roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookContextSchema {
    event: String,
    roots: BTreeSet<String>,
}

impl HookContextSchema {
    pub fn new<I, S>(event: impl Into<String>, roots: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            event: event.into(),
            roots: roots
                .into_iter()
                .map(|root| root.as_ref().to_string())
                .collect(),
        }
    }

    pub fn event(&self) -> &str {
        &self.event
    }

    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(String::as_str)
    }

    pub fn allows(&self, root: &str) -> bool {
        self.roots.contains(root)
    }

    pub fn validate_context(&self, context: &Value) -> Result<(), String> {
        let object = context
            .as_object()
            .ok_or_else(|| format!("hook event `{}` context must be an object", self.event))?;
        if let Some(root) = object.keys().find(|root| !self.allows(root)) {
            return Err(format!(
                "hook event `{}` context supplied undeclared root `{root}`",
                self.event
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum CompiledHookCondition {
    Always,
    Constant(bool),
    Expression(CompiledExpression),
}

impl CompiledHookCondition {
    pub fn references(&self) -> Option<&ReferenceSet> {
        match self {
            Self::Expression(expression) => Some(expression.references()),
            Self::Always | Self::Constant(_) => None,
        }
    }

    pub fn evaluate(&self, session: &mut EvaluationSession<'_>) -> Result<bool, ExpressionError> {
        match self {
            Self::Always => Ok(true),
            Self::Constant(value) => Ok(*value),
            Self::Expression(expression) => session.evaluate_bool(expression),
        }
    }
}

/// Execution-ready hook. Conditions and action string leaves have already
/// been parsed, and all AST roots have been checked against `context_schema`.
#[derive(Debug, Clone)]
pub struct CompiledHook {
    id: String,
    event: String,
    layer: HookLayer,
    result: HookResultMode,
    condition: CompiledHookCondition,
    action: CompiledActionTemplate,
    references: ReferenceSet,
    context_schema: HookContextSchema,
}

impl CompiledHook {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn event(&self) -> &str {
        &self.event
    }

    pub fn layer(&self) -> HookLayer {
        self.layer
    }

    pub fn result_mode(&self) -> HookResultMode {
        self.result
    }

    pub fn condition(&self) -> &CompiledHookCondition {
        &self.condition
    }

    pub fn action(&self) -> &CompiledActionTemplate {
        &self.action
    }

    pub fn references(&self) -> &ReferenceSet {
        &self.references
    }

    pub fn context_schema(&self) -> &HookContextSchema {
        &self.context_schema
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCompilationError {
    hook_index: Option<usize>,
    hook_id: Option<String>,
    event: String,
    message: String,
}

impl HookCompilationError {
    fn hook(hook_index: usize, hook_id: &str, event: &str, message: impl Into<String>) -> Self {
        Self {
            hook_index: Some(hook_index),
            hook_id: Some(hook_id.to_string()),
            event: event.to_string(),
            message: message.into(),
        }
    }

    fn schema(event: &str, message: impl Into<String>) -> Self {
        Self {
            hook_index: None,
            hook_id: None,
            event: event.to_string(),
            message: message.into(),
        }
    }

    pub fn hook_index(&self) -> Option<usize> {
        self.hook_index
    }

    pub fn hook_id(&self) -> Option<&str> {
        self.hook_id.as_deref()
    }

    pub fn event(&self) -> &str {
        &self.event
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for HookCompilationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.hook_index, self.hook_id.as_deref()) {
            (Some(index), Some(id)) => write!(
                formatter,
                "hook[{index}] (id={id}, event={}): {}",
                self.event, self.message
            ),
            _ => write!(
                formatter,
                "hook context schema for event `{}`: {}",
                self.event, self.message
            ),
        }
    }
}

impl std::error::Error for HookCompilationError {}

fn validate_reference_roots(
    references: &ReferenceSet,
    schema: &HookContextSchema,
    field: &str,
) -> Result<(), String> {
    for root in references.roots() {
        if !schema.allows(root) {
            let allowed = schema.roots().collect::<Vec<_>>().join(", ");
            return Err(format!(
                "{field} references undeclared root `{root}`; event `{}` allows only [{}]",
                schema.event(),
                allowed
            ));
        }
    }
    Ok(())
}

fn compile_condition(
    condition: ExpressionCondition,
    field: &str,
    limits: &CompilationLimits,
) -> Result<CompiledHookCondition, ExpressionError> {
    match condition {
        ExpressionCondition::Absent => Ok(CompiledHookCondition::Always),
        ExpressionCondition::Boolean(value) => Ok(CompiledHookCondition::Constant(value)),
        ExpressionCondition::Expression(source) => {
            compile_condition_for(&source, field, limits).map(CompiledHookCondition::Expression)
        }
    }
}

/// Merge the six source layers in fixed precedence order and compile once.
pub fn compile_hooks(
    sources: HookSources,
    schemas: &[HookContextSchema],
    limits: &CompilationLimits,
) -> Result<Vec<CompiledHook>, HookCompilationError> {
    let mut by_event = HashMap::new();
    for schema in schemas {
        if by_event.insert(schema.event(), schema).is_some() {
            return Err(HookCompilationError::schema(
                schema.event(),
                "event is declared more than once",
            ));
        }
    }

    let hooks = sources.into_layered();
    let mut compiled = Vec::with_capacity(hooks.len());
    let mut ids = HashMap::with_capacity(hooks.len());
    for (index, layered) in hooks.into_iter().enumerate() {
        let HookDefinition {
            id,
            event,
            result,
            condition: source_condition,
            action: source_action,
        } = layered.definition;
        if let Some(previous_layer) = ids.insert(id.clone(), layered.layer) {
            return Err(HookCompilationError::hook(
                index,
                &id,
                &event,
                format!(
                    "duplicate hook id `{id}` across {} and {} layers",
                    previous_layer.as_str(),
                    layered.layer.as_str()
                ),
            ));
        }
        let schema = by_event.get(event.as_str()).copied().ok_or_else(|| {
            HookCompilationError::hook(index, &id, &event, "event has no HookContextSchema")
        })?;
        if layered.layer.is_observer_only() && result == HookResultMode::Control {
            return Err(HookCompilationError::hook(
                index,
                &id,
                &event,
                "infrastructure hooks cannot declare result `control`",
            ));
        }
        let condition_field = format!("hook[{index}] (id={id}).condition");
        let condition =
            compile_condition(source_condition, &condition_field, limits).map_err(|error| {
                HookCompilationError::hook(
                    index,
                    &id,
                    &event,
                    format!("{error}; expression {:?}", error.source()),
                )
            })?;
        if let Some(references) = condition.references() {
            validate_reference_roots(references, schema, &condition_field)
                .map_err(|message| HookCompilationError::hook(index, &id, &event, message))?;
        }

        let action_field = format!("hook[{index}] (id={id}).action");
        let action = CompiledActionTemplate::compile(&source_action, &action_field, limits)
            .map_err(|error: CompiledTemplateError| {
                HookCompilationError::hook(index, &id, &event, error.to_string())
            })?;
        validate_reference_roots(action.references(), schema, &action_field)
            .map_err(|message| HookCompilationError::hook(index, &id, &event, message))?;

        let mut references = ReferenceSet::default();
        if let Some(condition_references) = condition.references() {
            references.extend(condition_references);
        }
        references.extend(action.references());
        compiled.push(CompiledHook {
            id,
            event,
            layer: layered.layer,
            result,
            condition,
            action,
            references,
            context_schema: schema.clone(),
        });
    }
    Ok(compiled)
}

/// Compile every captured layer against the signed event contracts embedded in
/// the effective plan. Runtime owners do not supply event arrays or reload
/// configured policy.
pub fn compile_effective_hook_plan(
    plan: &ryeos_engine::hooks::EffectiveHookPlan,
    limits: &CompilationLimits,
) -> Result<Vec<CompiledHook>, HookCompilationError> {
    plan.validate()
        .map_err(|error| HookCompilationError::schema("effective_hook_plan", error.to_string()))?;
    let schemas = plan
        .event_contracts
        .iter()
        .map(|(event, contract)| {
            HookContextSchema::new(
                event,
                contract
                    .context_contract
                    .allowed_roots
                    .iter()
                    .map(String::as_str),
            )
        })
        .collect::<Vec<_>>();
    compile_hooks(HookSources::from_effective_plan(plan), &schemas, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_layer_has_closed_named_wire_values() {
        assert_eq!(
            serde_json::to_value(HookLayer::Infrastructure).unwrap(),
            serde_json::json!("infrastructure")
        );
        assert!(serde_json::from_value::<HookLayer>(serde_json::json!("legacy")).is_err());
        assert!(serde_json::from_value::<HookLayer>(serde_json::json!(3)).is_err());
    }

    #[test]
    fn runtime_event_projection_never_hides_invalid_authored_hooks() {
        let hook = |id: &str, event: &str| HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
            result: HookResultMode::Discard,
            condition: ExpressionCondition::Absent,
            action: serde_json::json!({"item_id": "tool:test/noop"}),
        };
        let mut sources = HookSources {
            authored: vec![hook("authored-typo", "graph_finishd")],
            builtin: vec![hook("directive-only", "continuation")],
            project: vec![hook("graph", "graph_completed")],
            ..HookSources::default()
        };

        sources.retain_configured_events(&["graph_completed"]);

        assert_eq!(sources.authored[0].event, "graph_finishd");
        assert!(sources.builtin.is_empty());
        assert_eq!(sources.project[0].event, "graph_completed");
    }

    #[test]
    fn source_documents_reject_unknown_fields_and_authored_precedence() {
        assert!(
            serde_yaml::from_str::<HookDefinition>(
                "id: forged\nevent: after_step\nlayer: 6\naction: {item_id: tool:test/noop}\n"
            )
            .is_err()
        );
    }

    #[test]
    fn hook_definition_deserializes() {
        let yaml = "id: test\nevent: start\nresult: observation\naction:\n  primary: execute\n";
        let hook: HookDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(hook.id, "test");
        assert_eq!(hook.event, "start");
        assert_eq!(hook.result, HookResultMode::Observation);
    }

    #[test]
    fn hook_result_mode_is_required_and_closed() {
        let omitted = "id: test\nevent: start\naction: {}\n";
        let unknown = "id: test\nevent: start\nresult: inspect\naction: {}\n";
        assert!(serde_yaml::from_str::<HookDefinition>(omitted).is_err());
        assert!(serde_yaml::from_str::<HookDefinition>(unknown).is_err());
        for (wire, expected) in [
            ("discard", HookResultMode::Discard),
            ("control", HookResultMode::Control),
            ("observation", HookResultMode::Observation),
        ] {
            let yaml = format!("id: test\nevent: start\nresult: {wire}\naction: {{}}\n");
            assert_eq!(
                serde_yaml::from_str::<HookDefinition>(&yaml)
                    .unwrap()
                    .result,
                expected
            );
        }
    }

    #[test]
    fn infrastructure_hook_cannot_compile_as_control() {
        let error = compile_hooks(
            HookSources {
                infrastructure: vec![HookDefinition {
                    id: "infra-control".to_string(),
                    event: "after_step".to_string(),
                    result: HookResultMode::Control,
                    condition: ExpressionCondition::Absent,
                    action: serde_json::json!({"item_id": "tool:test/noop"}),
                }],
                ..HookSources::default()
            },
            &[HookContextSchema::new("after_step", ["turn"])],
            &CompilationLimits::default(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot declare result `control`")
        );
    }
}
