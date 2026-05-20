//! `RuntimeConfigHandler` — claims the top-level `config` block on a
//! tool/runtime item. Provides `command`, `args`, `input_data`,
//! `timeout_secs`, and per-handler `env`.
//!
//! Singleton semantics: at most one chain element may declare a
//! `config` block. Two ⇒ `EngineError::MultipleRuntimeConfigs`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

use crate::error::EngineError;
use crate::runtime::{expand_template, CompileContext, RuntimeHandler, RESERVED_ENV_PREFIX};

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
            let chain_strs: Vec<String> =
                ctx.chain.iter().map(|c| c.executor_id.clone()).collect();
            return Err(EngineError::MultipleRuntimeConfigs { chain: chain_strs });
        }

        let intermediate = &ctx.chain[ctx.current_index];
        let config: RuntimeConfig =
            serde_json::from_value(block.clone()).map_err(|e| EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("{e}"),
            })?;

        ctx.spec_overrides.command = Some(config.command);
        ctx.spec_overrides.args = Some(config.args);
        ctx.spec_overrides.stdin_data = config.input_data;
        ctx.spec_overrides.timeout_secs = Some(config.timeout_secs);

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
            if k.starts_with(RESERVED_ENV_PREFIX) {
                return Err(EngineError::ReservedEnvKey { key: k });
            }
            ctx.env.insert(k, v);
        }

        Ok(())
    }
}
