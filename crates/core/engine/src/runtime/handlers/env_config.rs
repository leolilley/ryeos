//! `EnvConfigHandler` — claims the top-level `env_config` block.
//!
//! Owns interpreter resolution (currently only the `local_binary`
//! strategy) and merges declared env entries into the compile
//! context. Sets `template_ctx.interpreter` so downstream templates
//! like `{interpreter}` resolve.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::EngineError;
use crate::runtime::{
    expand_env_value, CompileContext, HostEnvBindings, RuntimeHandler, RESERVED_ENV_PREFIX,
};

pub const KEY: &str = "env_config";

/// Per-platform path separator. The bundle YAMLs target Unix-style
/// hosts; matches Python `os.pathsep`.
const PATH_SEP: &str = ":";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    #[serde(default)]
    pub interpreter: Option<InterpreterConfig>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// PATH-style env-var mutations:
    /// `{VAR_NAME: {prepend: [...], append: [...]}}`. Templated
    /// values are deduplicated against existing entries (current
    /// `ctx.env[var]`, falling back to the host env).
    #[serde(default)]
    pub env_paths: HashMap<String, EnvPathMutation>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvPathMutation {
    #[serde(default)]
    pub prepend: Vec<String>,
    #[serde(default)]
    pub append: Vec<String>,
}

/// Tagged union of interpreter-resolution strategies. Today only
/// `local_binary` exists; new strategies (e.g. container, remote)
/// add a variant here without changing the handler call site.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InterpreterConfig {
    LocalBinary {
        binary: String,
        #[serde(default)]
        candidates: Vec<String>,
        #[serde(default)]
        search_paths: Vec<String>,
        var: Option<String>,
        /// Bare names tried at the end (resolved by the OS via PATH
        /// at spawn time). Replaces the old single-value `fallback`
        /// field with an explicit list.
        #[serde(default)]
        path_candidates: Vec<String>,
    },
}

pub fn resolve_interpreter(
    config: &InterpreterConfig,
    project_root: Option<&Path>,
) -> Result<String, EngineError> {
    match config {
        InterpreterConfig::LocalBinary {
            binary,
            candidates,
            search_paths,
            var,
            path_candidates,
        } => {
            // 1. Env-var override
            if let Some(v) = var {
                if let Ok(val) = std::env::var(v) {
                    return Ok(val);
                }
            }
            // 2. Project-local search paths × {binary, ...candidates}
            if let Some(root) = project_root {
                let binaries = std::iter::once(binary).chain(candidates.iter());
                for search_path in search_paths {
                    for b in binaries.clone() {
                        let candidate = root.join(search_path).join(b);
                        if candidate.exists() {
                            return Ok(candidate.to_string_lossy().to_string());
                        }
                    }
                }
            }
            // 3. PATH-resolved bare names
            if let Some(name) = path_candidates.first() {
                return Ok(name.clone());
            }
            Err(EngineError::RuntimeBinaryNotFound {
                binary: binary.clone(),
            })
        }
    }
}

pub struct EnvConfigHandler;

impl RuntimeHandler for EnvConfigHandler {
    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::ResolveContext
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        // env layers across the chain
        crate::runtime::HandlerCardinality::All
    }

    fn key(&self) -> &'static str {
        KEY
    }

    #[tracing::instrument(
        name = "engine:env_config",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];
        let env_config: EnvConfig =
            serde_json::from_value(block.clone()).map_err(|e| EngineError::InvalidRuntimeConfig {
                path: intermediate.source_path.display().to_string(),
                reason: format!("invalid env_config: {e}"),
            })?;

        // Always-present extra: this element's directory. Templates
        // may reference `{runtime_dir}` to locate sibling files
        // (e.g. PYTHONPATH entries) without cross-element peeking.
        // Last-write-wins across the chain, matching env_paths
        // layering.
        if let Some(parent) = intermediate.source_path.parent() {
            ctx.template_ctx.extra.insert(
                "runtime_dir".to_owned(),
                parent.to_string_lossy().into_owned(),
            );
        }

        // Resolve interpreter (if declared) and seed template ctx.
        if let Some(ic) = env_config.interpreter.as_ref() {
            let resolved = resolve_interpreter(ic, ctx.project_root)?;
            ctx.template_ctx.interpreter = Some(resolved.clone());
            // Inject the var binding into env so downstream subprocesses see it.
            let InterpreterConfig::LocalBinary { var, .. } = ic;
            if let Some(v) = var {
                if v.starts_with(RESERVED_ENV_PREFIX) {
                    // RYEOS_PYTHON et al. are explicitly *intended* to
                    // be set here by the runtime, so they bypass the
                    // reserved-prefix check that applies to user-
                    // declared `env:` entries below.
                }
                ctx.env.insert(v.clone(), resolved);
            }
        }

        for (k, v) in env_config.env {
            if k.starts_with(RESERVED_ENV_PREFIX) {
                return Err(EngineError::ReservedEnvKey { key: k });
            }
            ctx.env.insert(k, v);
        }

        // PATH-style mutations. Templated values are expanded
        // against the same `template_ctx` that `tool_dir`,
        // `runtime_dir`, `interpreter`, etc. already populated, so
        // bundle YAMLs can write `{prepend: ["{tool_dir}",
        // "{runtime_dir}/lib"]}` directly.
        apply_env_paths(&env_config.env_paths, &mut ctx.env, &ctx.template_ctx, ctx.host_env)?;

        Ok(())
    }
}

/// Apply `env_paths` mutations: prepend/append templated values to
/// the existing `VAR` (from `env`, falling back to the host-env
/// bindings), deduplicating against entries already present.
fn apply_env_paths(
    mutations: &HashMap<String, EnvPathMutation>,
    env: &mut HashMap<String, String>,
    template_ctx: &crate::runtime::TemplateContext,
    host_env: &HostEnvBindings,
) -> Result<(), EngineError> {
    for (var_name, mutation) in mutations {
        let existing = env
            .get(var_name)
            .cloned()
            .or_else(|| host_env.values.get(var_name).cloned())
            .unwrap_or_default();
        let mut parts: Vec<String> = if existing.is_empty() {
            Vec::new()
        } else {
            existing.split(PATH_SEP).map(str::to_owned).collect()
        };
        parts.retain(|p| !p.is_empty());

        // Reverse so the first listed prepend ends up at index 0
        // (matches Python `for path in reversed(prepend): parts.insert(0, ...)`).
        for tmpl in mutation.prepend.iter().rev() {
            let resolved = expand_env_value(tmpl, template_ctx, host_env)?;
            if resolved.is_empty() || parts.iter().any(|p| p == &resolved) {
                continue;
            }
            parts.insert(0, resolved);
        }
        for tmpl in &mutation.append {
            let resolved = expand_env_value(tmpl, template_ctx, host_env)?;
            if resolved.is_empty() || parts.iter().any(|p| p == &resolved) {
                continue;
            }
            parts.push(resolved);
        }

        env.insert(var_name.clone(), parts.join(PATH_SEP));
    }
    Ok(())
}
