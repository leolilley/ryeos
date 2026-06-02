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

use crate::contracts::RuntimeEnvSource;
use crate::error::EngineError;
use crate::runtime::{expand_template, is_reserved_env_name, CompileContext, RuntimeHandler};

pub const KEY: &str = "config";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
        let timeout_secs =
            timeout_secs_from_params(&ctx.params, config.timeout_secs, &intermediate.source_path)?;

        ctx.spec_overrides.command = Some(config.command);
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
}
