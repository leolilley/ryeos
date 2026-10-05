//! `RuntimeConfigHandler` — claims the top-level `config` block on a
//! tool/runtime item. Provides `command`, `args`, `input_data`,
//! `timeout_secs`, and per-handler `env`.
//!
//! Singleton semantics: at most one chain element may declare a
//! `config` block. Two ⇒ `EngineError::MultipleRuntimeConfigs`.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::config_loading::ConfigLoadContext;
use crate::contracts::RuntimeEnvSource;
use crate::error::EngineError;
use crate::execution_policy::ExecutionPolicyResolver;
use crate::runtime::{CompileContext, RuntimeHandler, expand_template, is_reserved_env_name};

pub const KEY: &str = "config";

/// One subprocess argv entry. Strings are rye-expr/1 templates. Embedded
/// program source and other bytes that must not be parsed are explicit
/// `{literal: ...}` values.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RuntimeArgument {
    Template(String),
    Literal(LiteralRuntimeArgument),
    SourceMember(SourceMemberRuntimeArgument),
    ExecutionProjectFile(ExecutionProjectFileRuntimeArgument),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProjectFileRuntimeArgument {
    pub execution_project_file: crate::contracts::PlanExecutionProjectFileInput,
}

/// A member of this execution's admitted source closure, never a host path.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceMemberRuntimeArgument {
    pub source_member: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiteralRuntimeArgument {
    pub literal: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<RuntimeArgument>,
    pub input_data: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional working directory override (templated).
    #[serde(default)]
    pub cwd: Option<String>,
}

fn default_timeout_secs() -> u64 {
    300
}

fn timeout_secs_from_params(
    params: &Value,
    descriptor_timeout_secs: u64,
    source_path: &Path,
) -> Result<u64, EngineError> {
    let Some(timeout) = params.get("timeout") else {
        return Ok(descriptor_timeout_secs);
    };

    timeout
        .as_u64()
        .ok_or_else(|| EngineError::InvalidRuntimeConfig {
            path: source_path.display().to_string(),
            reason: format!(
                "invalid execution timeout: expected unsigned integer seconds, got {timeout}"
            ),
        })
}

fn timeout_secs_for_config(
    ctx: &CompileContext<'_>,
    descriptor_timeout_secs: u64,
    source_path: &Path,
) -> Result<(u64, String), EngineError> {
    if ctx.original_params.get("timeout").is_some() {
        return Ok((
            timeout_secs_from_params(ctx.original_params, descriptor_timeout_secs, source_path)?,
            "caller param `timeout`".to_string(),
        ));
    }

    let root_ref = CanonicalRef::parse(&ctx.chain[0].resolved_ref).map_err(|e| {
        EngineError::InvalidRuntimeConfig {
            path: ctx.chain[0].source_path.display().to_string(),
            reason: format!("invalid root item ref for execution policy: {e}"),
        }
    })?;
    let policy = ExecutionPolicyResolver::new(ConfigLoadContext {
        roots: ctx.roots,
        parsers: ctx.parsers,
        kinds: ctx.kinds,
        trust_store: ctx.trust_store,
        project_authority: ctx.project_authority,
    })
    .resolve_for_item(&root_ref)?;
    if let Some(timeout) = policy.timeout {
        return Ok((timeout.value, timeout.source.describe()));
    }

    if ctx.params.get("timeout").is_some() {
        return Ok((
            timeout_secs_from_params(&ctx.params, descriptor_timeout_secs, source_path)?,
            "execution policy param `timeout`".to_string(),
        ));
    }

    Ok((
        descriptor_timeout_secs,
        "runtime descriptor `config.timeout_secs`".to_string(),
    ))
}

pub struct RuntimeConfigHandler;

impl RuntimeHandler for RuntimeConfigHandler {
    fn key(&self) -> &'static str {
        KEY
    }

    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::BuildSpec
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        // Only one chain element may provide a runtime config —
        // duplicates are a hard error.
        crate::runtime::HandlerCardinality::Singleton
    }

    #[tracing::instrument(
        name = "engine:runtime_config",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        // Singleton: a previous chain element already wrote to spec
        // overrides ⇒ collision.
        if ctx.spec_overrides.command.is_some() {
            let chain_strs: Vec<String> = ctx.chain.iter().map(|c| c.executor_id.clone()).collect();
            return Err(EngineError::MultipleRuntimeConfigs { chain: chain_strs });
        }

        let intermediate = &ctx.chain[ctx.current_index];
        let config: RuntimeConfig = serde_json::from_value(block.clone()).map_err(|e| {
            EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("{e}"),
            }
        })?;
        let runtime_command_owner = ctx
            .spec_overrides
            .authenticated_chain_authorities
            .as_ref()
            .and_then(|authorities| authorities.get(ctx.current_index))
            .cloned()
            .map(|definition_owner| crate::runtime::RuntimeCommandOwner {
                definition_owner,
                definition_owner_chain_index: ctx.current_index,
                authored_command: config.command.clone(),
            });
        for argument in &config.args {
            if let RuntimeArgument::ExecutionProjectFile(input) = argument {
                if ctx.current_index != 0 {
                    return Err(EngineError::InvalidRuntimeConfig {
                        path: "config.args.execution_project_file".to_owned(),
                        reason: "execution project file must be declared by the root definition"
                            .to_owned(),
                    });
                }
                input.execution_project_file.validate().map_err(|error| {
                    EngineError::InvalidRuntimeConfig {
                        path: "config.args.execution_project_file".to_owned(),
                        reason: error.to_string(),
                    }
                })?;
            }
        }
        let (timeout_secs, timeout_source) =
            timeout_secs_for_config(ctx, config.timeout_secs, &intermediate.source_path)?;
        tracing::info!(
            item_ref = %intermediate.resolved_ref,
            timeout_secs,
            timeout_source,
            descriptor_timeout_secs = config.timeout_secs,
            "runtime config timeout resolved"
        );

        ctx.spec_overrides.command = Some(config.command);
        ctx.spec_overrides.runtime_command_owner = runtime_command_owner;
        ctx.spec_overrides.args = Some(config.args);
        ctx.spec_overrides.stdin_data = config.input_data;
        ctx.spec_overrides.timeout_secs = Some(timeout_secs);

        // Expand cwd template now (template_ctx has the always-
        // present `tool_dir` / `tool_parent` extras and any
        // `ResolveContext`-phase additions like `runtime_dir` /
        // `interpreter`). Stored as a literal PathBuf — no further
        // expansion downstream.
        if let Some(cwd_template) = config.cwd {
            let resolved = expand_template(&cwd_template, &ctx.template_ctx)?;
            ctx.spec_overrides.cwd = Some(std::path::PathBuf::from(resolved));
        }

        for (k, v) in config.env {
            if is_reserved_env_name(&k) {
                return Err(EngineError::ReservedEnvKey { key: k });
            }
            ctx.env_sources
                .insert(k.clone(), RuntimeEnvSource::RuntimeDescriptor);
            ctx.env.insert(k, v);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn descriptor_timeout_wins_when_no_execution_override_present() {
        let path = PathBuf::from("/tmp/runtime.yaml");
        let timeout = timeout_secs_from_params(&json!({}), 300, &path).unwrap();
        assert_eq!(timeout, 300);
    }

    #[test]
    fn execution_timeout_override_wins_over_descriptor_default() {
        let path = PathBuf::from("/tmp/runtime.yaml");
        let timeout = timeout_secs_from_params(&json!({"timeout": 7200}), 300, &path).unwrap();
        assert_eq!(timeout, 7200);
    }

    #[test]
    fn execution_timeout_override_must_be_unsigned_integer_seconds() {
        let path = PathBuf::from("/tmp/runtime.yaml");
        let err = timeout_secs_from_params(&json!({"timeout": "7200"}), 300, &path)
            .expect_err("string timeout should fail");
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { ref reason, .. } if reason.contains("invalid execution timeout")),
            "got {err:?}"
        );
    }

    #[test]
    fn runtime_arguments_distinguish_templates_from_literal_source() {
        let config: RuntimeConfig = serde_json::from_value(json!({
            "command": "${interpreter}",
            "args": [
                "${tool_path}",
                {"literal": "print(f'{tool_path}')"}
            ]
        }))
        .unwrap();

        assert_eq!(
            config.args,
            vec![
                RuntimeArgument::Template("${tool_path}".into()),
                RuntimeArgument::Literal(LiteralRuntimeArgument {
                    literal: "print(f'{tool_path}')".into(),
                }),
            ]
        );
    }

    #[test]
    fn runtime_argument_literal_wrapper_rejects_unknown_fields() {
        let error = serde_json::from_value::<RuntimeConfig>(json!({
            "command": "tool",
            "args": [{"literal": "source", "template": "${tool_path}"}]
        }))
        .unwrap_err();

        assert!(error.to_string().contains("did not match"), "{error}");
    }
    #[test]
    fn execution_project_file_is_owned_by_root_config_not_params_or_parent() {
        use crate::item_resolution::ResolutionRoots;
        use crate::kind_registry::KindRegistry;
        use crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors;
        use crate::runtime::{ChainIntermediate, HostEnvBindings, SpecOverrides, TemplateContext};
        use crate::trust::TrustStore;
        let block = json!({"command":"runtime", "args":[{"execution_project_file": {
            "relative_path":"candidate/module.py", "destination":"/candidate-input",
            "max_bytes":4096, "normalized_mode":420, "access":"read_only"
        }}]});
        let chain: Vec<_> = ["tool:test/root", "tool:test/parent"]
            .into_iter()
            .map(|reference| ChainIntermediate {
                executor_id: reference.to_owned(),
                resolved_ref: reference.to_owned(),
                kind: "tool".into(),
                source_path: PathBuf::from("/signed/config.yaml"),
                source_space: crate::contracts::ItemSpace::Project,
                source_root: crate::contracts::ItemSourceRoot::Search {
                    label: "test".into(),
                },
                parsed: json!({"config":block.clone()}),
            })
            .collect();
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let host = HostEnvBindings::default();
        let params =
            json!({"execution_project_file":{"relative_path":"wrong.py","max_bytes":65536}});
        for current_index in [0, 1] {
            let mut ctx = CompileContext {
                template_ctx: TemplateContext::new(PathBuf::from("/signed/config.yaml")),
                env: HashMap::new(),
                env_sources: HashMap::new(),
                spec_overrides: SpecOverrides::default(),
                params: params.clone(),
                original_params: &params,
                chain: &chain,
                current_index,
                roots: &roots,
                parsers: &parsers,
                kinds: &kinds,
                trust_store: &trust,
                node_trust_store: &trust,
                project_root: None,
                project_authority: None,
                sealed_content: None,
                root_trust_class: crate::resolution::TrustClass::TrustedBundle,
                host_env: &host,
            };
            let result = RuntimeConfigHandler
                .apply(chain[current_index].parsed.get("config").unwrap(), &mut ctx);
            if current_index == 0 {
                result.unwrap();
                let args = ctx.spec_overrides.args.unwrap();
                let RuntimeArgument::ExecutionProjectFile(input) = &args[0] else {
                    panic!("root lost typed member authority");
                };
                assert_eq!(
                    input.execution_project_file.relative_path,
                    "candidate/module.py"
                );
                assert_eq!(input.execution_project_file.max_bytes, 4096);
            } else {
                assert!(
                    matches!(result,Err(EngineError::InvalidRuntimeConfig { reason,.. }) if reason.contains("root definition"))
                );
                assert!(ctx.spec_overrides.command.is_none());
            }
        }
    }
}
