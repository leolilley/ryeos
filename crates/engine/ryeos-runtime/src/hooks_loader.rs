use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compiled_template::{CompiledActionTemplate, CompiledTemplateError};
use crate::events::RuntimeEventType;
use crate::expression::{
    compile_condition_for, CompilationLimits, CompiledExpression, EvaluationSession,
    ExpressionError, ReferenceSet,
};
use crate::ExpressionCondition;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDefinition {
    pub id: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "ExpressionCondition::is_absent")]
    pub condition: ExpressionCondition,
    pub action: Value,
}

/// Deterministic source precedence for runtime hooks. A source owns its layer;
/// authored data cannot forge or reorder this trust/provenance boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum HookLayer {
    Authored = 1,
    Builtin = 2,
    Infrastructure = 3,
    Context = 4,
    Operator = 5,
    Project = 6,
}

impl HookLayer {
    pub const fn precedence(self) -> u8 {
        self as u8
    }

    pub const fn is_observer_only(self) -> bool {
        matches!(self, Self::Infrastructure)
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Builtin => "builtin",
            Self::Infrastructure => "infrastructure",
            Self::Context => "context",
            Self::Operator => "operator",
            Self::Project => "project",
        }
    }
}

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
            condition,
            action,
            references,
            context_schema: schema.clone(),
        });
    }
    Ok(compiled)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConditionsConfig {
    #[serde(default)]
    pub builtin_hooks: Vec<HookDefinition>,
    #[serde(default)]
    pub infra_hooks: Vec<HookDefinition>,
    #[serde(default)]
    pub context_hooks: Vec<HookDefinition>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksFile {
    #[serde(default)]
    pub hooks: Vec<HookDefinition>,
}

/// Load every non-authored hook source through the launch-owned verified
/// loader. The operator config root is derived only from the explicit envelope
/// trusted-keys directory and must have the documented
/// `<app-root>/.ai/config/keys/trusted` shape; no environment fallback exists.
pub fn load_configured_hook_sources(
    loader: &crate::verified_loader::VerifiedLoader,
) -> anyhow::Result<HookSources> {
    let conditions = loader
        .load_bundle_config_strict_signed::<HookConditionsConfig>("ryeos-runtime/hook_conditions")
        .map_err(|error| anyhow::anyhow!("loading configured runtime hook conditions: {error}"))?
        .unwrap_or_default();

    let project_path = loader.project_root().join(".ai/config/agent/hooks.yaml");
    let project = loader
        .load_config_file_strict::<HooksFile>(&project_path)
        .map_err(|error| anyhow::anyhow!("loading configured project hooks: {error}"))?
        .unwrap_or_default()
        .hooks;

    let operator_path = operator_hooks_path(loader.node_trusted_keys_dir())?;
    let operator = loader
        .load_config_file_strict::<HooksFile>(&operator_path)
        .map_err(|error| anyhow::anyhow!("loading configured operator hooks: {error}"))?
        .unwrap_or_default()
        .hooks;

    let sources = HookSources {
        authored: Vec::new(),
        builtin: conditions.builtin_hooks,
        infrastructure: conditions.infra_hooks,
        context: conditions.context_hooks,
        operator,
        project,
    };
    validate_configured_hook_events(&sources)?;
    Ok(sources)
}

fn validate_configured_hook_events(sources: &HookSources) -> anyhow::Result<()> {
    let known_events = [
        "after_step",
        "continuation",
        RuntimeEventType::GraphStarted.as_str(),
        RuntimeEventType::GraphStepCompleted.as_str(),
        RuntimeEventType::GraphCompleted.as_str(),
    ];
    let mut ids = HashMap::new();
    for (layer, hooks) in [
        ("builtin", sources.builtin.as_slice()),
        ("infrastructure", sources.infrastructure.as_slice()),
        ("context", sources.context.as_slice()),
        ("operator", sources.operator.as_slice()),
        ("project", sources.project.as_slice()),
    ] {
        for hook in hooks {
            if let Some(previous_layer) = ids.insert(hook.id.as_str(), layer) {
                anyhow::bail!(
                    "duplicate configured hook id `{}` across {previous_layer} and {layer} layers",
                    hook.id
                );
            }
            if !known_events.contains(&hook.event.as_str()) {
                anyhow::bail!(
                    "configured {layer} hook `{}` declares unknown event `{}`; known runtime hook events are [{}]",
                    hook.id,
                    hook.event,
                    known_events.join(", ")
                );
            }
        }
    }
    Ok(())
}

fn operator_hooks_path(trusted_keys_dir: &Path) -> anyhow::Result<PathBuf> {
    let trusted = trusted_keys_dir.file_name().and_then(|name| name.to_str());
    let keys_dir = trusted_keys_dir.parent();
    let keys = keys_dir
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let config_dir = keys_dir.and_then(Path::parent);
    let config = config_dir
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let ai = config_dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    if trusted != Some("trusted")
        || keys != Some("keys")
        || config != Some("config")
        || ai != Some(".ai")
    {
        anyhow::bail!(
            "operator trusted-keys directory {} must end in `.ai/config/keys/trusted`",
            trusted_keys_dir.display()
        );
    }
    Ok(config_dir
        .expect("validated operator config parent")
        .join("agent/hooks.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_and_trust(yaml_body: &str, trust_dir: &Path) -> String {
        use base64::Engine;
        use ed25519_dalek::SigningKey;
        use lillux::signature::{compute_fingerprint, sign_content_at};

        let signing_key = SigningKey::from_bytes(&[73u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = compute_fingerprint(&verifying_key);
        let signed = sign_content_at(yaml_body, &signing_key, "#", None, "2026-01-01T00:00:00Z");

        std::fs::create_dir_all(trust_dir).unwrap();
        let public_key = base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes());
        std::fs::write(
            trust_dir.join("hooks-test.toml"),
            format!(
                "fingerprint = \"{fingerprint}\"\npem = \"ed25519:{public_key}\"\nowner = \"test\"\n"
            ),
        )
        .unwrap();
        signed
    }

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
    fn operator_hook_path_is_anchored_to_configured_trust_root() {
        assert_eq!(
            operator_hooks_path(Path::new("/app/.ai/config/keys/trusted")).unwrap(),
            PathBuf::from("/app/.ai/config/agent/hooks.yaml")
        );
        assert!(operator_hooks_path(Path::new("/tmp/arbitrary-keys")).is_err());
    }

    #[test]
    fn configured_loader_collects_every_verified_non_authored_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let bundle = tmp.path().join("bundle");
        let app = tmp.path().join("app");
        let runtime_config = bundle.join(".ai/config/ryeos-runtime");
        let project_hooks = project.join(".ai/config/agent");
        let operator_hooks = app.join(".ai/config/agent");
        let trusted_keys = app.join(".ai/config/keys/trusted");
        for directory in [
            &runtime_config,
            &project_hooks,
            &operator_hooks,
            &trusted_keys,
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let runtime_conditions = r#"
builtin_hooks: [{id: builtin, event: after_step, action: {item_id: tool:test/noop, ref_bindings: {}}}]
infra_hooks: [{id: infra, event: after_step, action: {item_id: tool:test/noop, ref_bindings: {}}}]
context_hooks: [{id: context, event: after_step, action: {item_id: tool:test/noop, ref_bindings: {}}}]
"#;
        std::fs::write(
            runtime_config.join("hook_conditions.yaml"),
            sign_and_trust(runtime_conditions, &trusted_keys),
        )
        .unwrap();
        let project_config = "hooks: [{id: project, event: after_step, action: {item_id: 'tool:test/noop', ref_bindings: {}}}]\n";
        std::fs::write(
            project_hooks.join("hooks.yaml"),
            sign_and_trust(project_config, &trusted_keys),
        )
        .unwrap();
        let operator_config = "hooks: [{id: operator, event: after_step, action: {item_id: 'tool:test/noop', ref_bindings: {}}}]\n";
        std::fs::write(
            operator_hooks.join("hooks.yaml"),
            sign_and_trust(operator_config, &trusted_keys),
        )
        .unwrap();

        let loader =
            crate::verified_loader::VerifiedLoader::new(project, vec![bundle], &trusted_keys);
        let sources = load_configured_hook_sources(&loader).unwrap();

        assert_eq!(sources.builtin[0].id, "builtin");
        assert_eq!(sources.infrastructure[0].id, "infra");
        assert_eq!(sources.context[0].id, "context");
        assert_eq!(sources.operator[0].id, "operator");
        assert_eq!(sources.project[0].id, "project");
    }

    #[test]
    fn runtime_event_projection_never_hides_invalid_authored_hooks() {
        let hook = |id: &str, event: &str| HookDefinition {
            id: id.to_string(),
            event: event.to_string(),
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
    fn configured_event_typos_fail_before_runtime_projection() {
        let sources = HookSources {
            operator: vec![HookDefinition {
                id: "typo".to_string(),
                event: "graph_finishd".to_string(),
                condition: ExpressionCondition::Absent,
                action: serde_json::json!({"item_id": "tool:test/noop"}),
            }],
            ..HookSources::default()
        };

        let error = validate_configured_hook_events(&sources).unwrap_err();

        assert!(error
            .to_string()
            .contains("configured operator hook `typo`"));
        assert!(error.to_string().contains("unknown event `graph_finishd`"));
    }

    #[test]
    fn source_documents_reject_unknown_fields_and_authored_precedence() {
        assert!(serde_yaml::from_str::<HookConditionsConfig>(
            "builtin_hooks: []\nunknown_hooks: []\n"
        )
        .is_err());
        assert!(serde_yaml::from_str::<HookDefinition>(
            "id: forged\nevent: after_step\nlayer: 6\naction: {item_id: tool:test/noop}\n"
        )
        .is_err());
    }

    #[test]
    fn hook_definition_deserializes() {
        let yaml = "id: test\nevent: start\naction:\n  primary: execute\n";
        let hook: HookDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(hook.id, "test");
        assert_eq!(hook.event, "start");
    }
}
