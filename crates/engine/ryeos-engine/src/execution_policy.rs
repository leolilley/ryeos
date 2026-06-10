use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::config_loading::{load_and_verify_config_file, ConfigLoadContext};
use crate::contracts::ItemSpace;
use crate::error::EngineError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySourceKind {
    ExecutionYamlDefault,
    ExecutionYamlItemOverride,
    CallerParam,
    RuntimeDescriptor,
    BuiltInDefault,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySource {
    pub kind: PolicySourceKind,
    pub path: Option<PathBuf>,
    pub space: Option<ItemSpace>,
    pub scope: Option<String>,
    pub field: String,
}

impl PolicySource {
    pub fn describe(&self) -> String {
        match (&self.path, &self.scope) {
            (Some(path), Some(scope)) => format!("{}.{}.{}", path.display(), scope, self.field),
            (Some(path), None) => format!("{}.{}", path.display(), self.field),
            (None, Some(scope)) => format!("{}.{}", scope, self.field),
            (None, None) => format!("{:?}.{}", self.kind, self.field),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: PolicySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancellationModeChoice {
    Graceful,
    Hard,
}

impl CancellationModeChoice {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::Hard => "hard",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyLayerSource {
    pub path: PathBuf,
    pub space: ItemSpace,
}

#[derive(Debug, Clone)]
pub struct ResolvedExecutionPolicy {
    pub item_ref: CanonicalRef,
    pub timeout: Option<Sourced<u64>>,
    pub max_steps: Option<Sourced<u32>>,
    pub max_concurrency: Option<Sourced<u32>>,
    pub cancellation_mode: Option<Sourced<CancellationModeChoice>>,
    pub cancellation_grace_secs: Option<Sourced<u64>>,
    pub loaded_layers: Vec<PolicyLayerSource>,
    pub warnings: Vec<String>,
}

impl ResolvedExecutionPolicy {
    pub fn empty(item_ref: CanonicalRef) -> Self {
        Self {
            item_ref,
            timeout: None,
            max_steps: None,
            max_concurrency: None,
            cancellation_mode: None,
            cancellation_grace_secs: None,
            loaded_layers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn get_runtime_param(&self, key: &str) -> Option<Value> {
        match key {
            "timeout" => self.timeout.as_ref().map(|v| Value::from(v.value)),
            "max_steps" => self.max_steps.as_ref().map(|v| Value::from(v.value)),
            "max_concurrency" => self.max_concurrency.as_ref().map(|v| Value::from(v.value)),
            "cancellation_mode" => self
                .cancellation_mode
                .as_ref()
                .map(|v| Value::String(v.value.as_str().to_owned())),
            "cancellation_grace_secs" => self
                .cancellation_grace_secs
                .as_ref()
                .map(|v| Value::from(v.value)),
            _ => None,
        }
    }

    pub fn source_for(&self, key: &str) -> Option<&PolicySource> {
        match key {
            "timeout" => self.timeout.as_ref().map(|v| &v.source),
            "max_steps" => self.max_steps.as_ref().map(|v| &v.source),
            "max_concurrency" => self.max_concurrency.as_ref().map(|v| &v.source),
            "cancellation_mode" => self.cancellation_mode.as_ref().map(|v| &v.source),
            "cancellation_grace_secs" => self.cancellation_grace_secs.as_ref().map(|v| &v.source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicyDocument {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub defaults: ExecutionPolicyValues,
    #[serde(default)]
    pub tools: HashMap<String, ExecutionPolicyValues>,
    #[serde(default)]
    pub graphs: HashMap<String, ExecutionPolicyValues>,
    #[serde(default)]
    pub directives: HashMap<String, ExecutionPolicyValues>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicyValues {
    pub timeout: Option<u64>,
    pub max_steps: Option<u32>,
    pub max_concurrency: Option<u32>,
    pub cancellation_mode: Option<String>,
    pub cancellation_grace_secs: Option<u64>,
}

pub struct ExecutionPolicyResolver<'a> {
    load_ctx: ConfigLoadContext<'a>,
}

pub fn value_has_execution_policy_shape(value: &Value) -> bool {
    value.get("defaults").is_some()
        || value.get("tools").is_some()
        || value.get("graphs").is_some()
        || value.get("directives").is_some()
}

impl<'a> ExecutionPolicyResolver<'a> {
    pub fn new(load_ctx: ConfigLoadContext<'a>) -> Self {
        Self { load_ctx }
    }

    pub fn resolve_for_item(
        &self,
        item_ref: &CanonicalRef,
    ) -> Result<ResolvedExecutionPolicy, EngineError> {
        let mut policy = ResolvedExecutionPolicy::empty(item_ref.clone());
        let mut documents = Vec::new();
        for root in &self.load_ctx.roots.ordered {
            let candidate = root
                .ai_root
                .join("config")
                .join("execution")
                .join("execution.yaml");
            if !candidate.exists() {
                continue;
            }
            let value = load_and_verify_config_file(&candidate, &self.load_ctx)?;
            let doc: ExecutionPolicyDocument =
                serde_json::from_value(value).map_err(|e| EngineError::InvalidRuntimeConfig {
                    path: candidate.display().to_string(),
                    reason: format!("invalid execution policy: {e}"),
                })?;
            policy.loaded_layers.push(PolicyLayerSource {
                path: candidate.clone(),
                space: root.space,
            });
            documents.push((doc, candidate, root.space));
        }
        for (doc, path, space) in &documents {
            apply_defaults_layer(&mut policy, doc, Some(path.clone()), Some(*space))?;
        }
        for (doc, path, space) in &documents {
            apply_item_override_layer(
                &mut policy,
                doc,
                item_ref,
                Some(path.clone()),
                Some(*space),
            )?;
        }
        Ok(policy)
    }

    pub fn resolve_from_value_for_item(
        resolved_config: &Value,
        item_ref: &CanonicalRef,
        source_path: Option<PathBuf>,
        source_space: Option<ItemSpace>,
    ) -> Result<ResolvedExecutionPolicy, EngineError> {
        let doc: ExecutionPolicyDocument = serde_json::from_value(resolved_config.clone())
            .map_err(|e| EngineError::InvalidRuntimeConfig {
                path: source_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<resolved execution config>".to_string()),
                reason: format!("invalid execution policy: {e}"),
            })?;
        let mut policy = ResolvedExecutionPolicy::empty(item_ref.clone());
        apply_document_layer(&mut policy, &doc, item_ref, source_path, source_space)?;
        Ok(policy)
    }
}

fn apply_document_layer(
    policy: &mut ResolvedExecutionPolicy,
    doc: &ExecutionPolicyDocument,
    item_ref: &CanonicalRef,
    path: Option<PathBuf>,
    space: Option<ItemSpace>,
) -> Result<(), EngineError> {
    apply_defaults_layer(policy, doc, path.clone(), space)?;
    apply_item_override_layer(policy, doc, item_ref, path, space)
}

fn apply_defaults_layer(
    policy: &mut ResolvedExecutionPolicy,
    doc: &ExecutionPolicyDocument,
    path: Option<PathBuf>,
    space: Option<ItemSpace>,
) -> Result<(), EngineError> {
    apply_values(
        policy,
        &doc.defaults,
        PolicySourceKind::ExecutionYamlDefault,
        path,
        space,
        "defaults".to_string(),
    )
}

fn apply_item_override_layer(
    policy: &mut ResolvedExecutionPolicy,
    doc: &ExecutionPolicyDocument,
    item_ref: &CanonicalRef,
    path: Option<PathBuf>,
    space: Option<ItemSpace>,
) -> Result<(), EngineError> {
    let (section, values) = match item_ref.kind.as_str() {
        "tool" => ("tools", doc.tools.get(&item_ref.bare_id)),
        "graph" => ("graphs", doc.graphs.get(&item_ref.bare_id)),
        "directive" => ("directives", doc.directives.get(&item_ref.bare_id)),
        _ => ("", None),
    };
    if let Some(values) = values {
        apply_values(
            policy,
            values,
            PolicySourceKind::ExecutionYamlItemOverride,
            path,
            space,
            format!("{section}.{}", item_ref.bare_id),
        )?;
    }
    Ok(())
}

fn apply_values(
    policy: &mut ResolvedExecutionPolicy,
    values: &ExecutionPolicyValues,
    source_kind: PolicySourceKind,
    path: Option<PathBuf>,
    space: Option<ItemSpace>,
    scope: String,
) -> Result<(), EngineError> {
    if let Some(value) = values.timeout {
        policy.timeout = Some(Sourced {
            value,
            source: source(&source_kind, &path, space, &scope, "timeout"),
        });
    }
    if let Some(value) = values.max_steps {
        policy.max_steps = Some(Sourced {
            value,
            source: source(&source_kind, &path, space, &scope, "max_steps"),
        });
    }
    if let Some(value) = values.max_concurrency {
        policy.max_concurrency = Some(Sourced {
            value,
            source: source(&source_kind, &path, space, &scope, "max_concurrency"),
        });
    }
    if let Some(raw) = &values.cancellation_mode {
        let value = match raw.as_str() {
            "graceful" => CancellationModeChoice::Graceful,
            "hard" => CancellationModeChoice::Hard,
            other => {
                return Err(EngineError::InvalidRuntimeConfig {
                    path: path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<resolved execution config>".to_string()),
                    reason: format!(
                        "unknown cancellation_mode `{other}` in execution policy (expected `graceful` | `hard`)"
                    ),
                });
            }
        };
        policy.cancellation_mode = Some(Sourced {
            value,
            source: source(&source_kind, &path, space, &scope, "cancellation_mode"),
        });
    }
    if let Some(value) = values.cancellation_grace_secs {
        policy.cancellation_grace_secs = Some(Sourced {
            value,
            source: source(
                &source_kind,
                &path,
                space,
                &scope,
                "cancellation_grace_secs",
            ),
        });
    }
    Ok(())
}

fn source(
    kind: &PolicySourceKind,
    path: &Option<PathBuf>,
    space: Option<ItemSpace>,
    scope: &str,
    field: &str,
) -> PolicySource {
    PolicySource {
        kind: kind.clone(),
        path: path.clone(),
        space,
        scope: Some(scope.to_string()),
        field: field.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn item_override_beats_defaults_and_preserves_sparse_fields() {
        let item_ref = CanonicalRef::parse("tool:snap-track/scrapers/hydrate-shows").unwrap();
        let value = json!({
            "category": "execution",
            "version": "1",
            "schema_version": "v1",
            "defaults": {
                "timeout": 300,
                "max_steps": 5,
                "max_concurrency": 10,
                "cancellation_mode": "graceful",
                "cancellation_grace_secs": 5
            },
            "tools": {
                "snap-track/scrapers/hydrate-shows": {
                    "timeout": 7200
                }
            }
        });

        let policy = ExecutionPolicyResolver::resolve_from_value_for_item(
            &value,
            &item_ref,
            Some(PathBuf::from(
                "/project/.ai/config/execution/execution.yaml",
            )),
            Some(ItemSpace::Project),
        )
        .unwrap();

        let timeout = policy.timeout.unwrap();
        assert_eq!(timeout.value, 7200);
        assert_eq!(
            timeout.source.kind,
            PolicySourceKind::ExecutionYamlItemOverride
        );
        assert_eq!(
            timeout.source.scope.as_deref(),
            Some("tools.snap-track/scrapers/hydrate-shows")
        );
        assert_eq!(policy.max_steps.unwrap().value, 5);
        assert_eq!(policy.max_concurrency.unwrap().value, 10);
        assert_eq!(
            policy.cancellation_mode.unwrap().value,
            CancellationModeChoice::Graceful
        );
    }

    #[test]
    fn graph_and_directive_maps_are_kind_aware() {
        let graph_ref = CanonicalRef::parse("graph:snap-track/show_rescrape").unwrap();
        let directive_ref = CanonicalRef::parse("directive:agent/workflow").unwrap();
        let value = json!({
            "defaults": { "timeout": 300 },
            "tools": { "snap-track/show_rescrape": { "timeout": 111 } },
            "graphs": { "snap-track/show_rescrape": { "timeout": 7200 } },
            "directives": { "agent/workflow": { "max_steps": 50 } }
        });

        let graph =
            ExecutionPolicyResolver::resolve_from_value_for_item(&value, &graph_ref, None, None)
                .unwrap();
        assert_eq!(graph.timeout.unwrap().value, 7200);

        let directive = ExecutionPolicyResolver::resolve_from_value_for_item(
            &value,
            &directive_ref,
            None,
            None,
        )
        .unwrap();
        assert_eq!(directive.timeout.unwrap().value, 300);
        assert_eq!(directive.max_steps.unwrap().value, 50);
    }

    #[test]
    fn invalid_cancellation_mode_is_loud() {
        let item_ref = CanonicalRef::parse("tool:x").unwrap();
        let value = json!({
            "defaults": { "cancellation_mode": "later" }
        });
        let err =
            ExecutionPolicyResolver::resolve_from_value_for_item(&value, &item_ref, None, None)
                .unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { ref reason, .. } if reason.contains("unknown cancellation_mode")),
            "got {err:?}"
        );
    }

    #[test]
    fn item_override_from_lower_layer_beats_higher_layer_default() {
        let item_ref = CanonicalRef::parse("tool:x").unwrap();
        let system = ExecutionPolicyDocument {
            defaults: ExecutionPolicyValues {
                timeout: Some(300),
                ..Default::default()
            },
            tools: HashMap::from([(
                "x".to_string(),
                ExecutionPolicyValues {
                    timeout: Some(7200),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let project = ExecutionPolicyDocument {
            defaults: ExecutionPolicyValues {
                timeout: Some(60),
                max_steps: Some(5),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut policy = ResolvedExecutionPolicy::empty(item_ref.clone());
        apply_defaults_layer(
            &mut policy,
            &system,
            Some(PathBuf::from("/system/execution.yaml")),
            Some(ItemSpace::Bundle),
        )
        .unwrap();
        apply_defaults_layer(
            &mut policy,
            &project,
            Some(PathBuf::from("/project/execution.yaml")),
            Some(ItemSpace::Project),
        )
        .unwrap();
        apply_item_override_layer(
            &mut policy,
            &system,
            &item_ref,
            Some(PathBuf::from("/system/execution.yaml")),
            Some(ItemSpace::Bundle),
        )
        .unwrap();
        apply_item_override_layer(
            &mut policy,
            &project,
            &item_ref,
            Some(PathBuf::from("/project/execution.yaml")),
            Some(ItemSpace::Project),
        )
        .unwrap();

        let timeout = policy.timeout.unwrap();
        assert_eq!(timeout.value, 7200);
        assert_eq!(
            timeout.source.kind,
            PolicySourceKind::ExecutionYamlItemOverride
        );
        assert_eq!(policy.max_steps.unwrap().value, 5);
    }

    #[test]
    fn unknown_policy_value_field_is_loud() {
        let item_ref = CanonicalRef::parse("tool:x").unwrap();
        let value = json!({
            "defaults": { "max_step": 5 }
        });
        let err =
            ExecutionPolicyResolver::resolve_from_value_for_item(&value, &item_ref, None, None)
                .unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { ref reason, .. } if reason.contains("unknown field `max_step`")),
            "got {err:?}"
        );
    }
}
