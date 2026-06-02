//! `NativeAsyncHandler` — claims the top-level `native_async` block.
//!
//! Presence of this block on a chain element flags the resulting
//! subprocess as **owning its own event stream**: the runner injects
//! `RYEOS_NATIVE_ASYNC=1` into the spawn env so the subprocess knows
//! to call `emit_progress` / `emit_status` / `publish_artifact`
//! against `RuntimeCallbackAPI` itself, instead of having the daemon
//! synthesise them from stdout buffering.
//!
//! The block also picks the cancellation policy that the daemon's
//! pgid termination path uses (`Hard` = SIGKILL immediately,
//! `Graceful` = SIGTERM, wait `grace_secs`, then SIGKILL).
//!
//! Phase / cardinality: `DecorateSpec` / `FirstWins`. Cancellation
//! policy must be unambiguous; the FIRST chain element that declares
//! the block wins, matching how `verify_deps` resolves chain
//! conflicts.
//!
//! ## YAML shapes accepted
//!
//! Bool shorthand:
//! ```yaml
//! native_async: true
//! ```
//!
//! Rich form (no `enabled:` field — presence already means enabled):
//! ```yaml
//! native_async:
//!   cancel_mode: graceful        # default: graceful
//!   graceful_shutdown_secs: 5    # default: 5
//! ```
//!
//! `native_async: false` is a hard error (omit the block to disable
//! — otherwise a `false` shadow on the first chain element would
//! silently suppress a real config later).

use serde::Deserialize;
use serde_json::Value;

use crate::contracts::{CancellationMode, NativeAsyncSpec};
use crate::error::EngineError;
use crate::runtime::{CompileContext, RuntimeHandler};

pub const KEY: &str = "native_async";

/// Last-resort default when neither the per-tool YAML nor the
/// resolved system execution config (`config/execution/execution.yaml`)
/// supplies a value. Mirrors the conservative default in
/// `CancellationMode::default()` on the contracts side.
const DEFAULT_GRACEFUL_SECS: u64 = 5;

/// Resolved cancellation policy from `execution.yaml`. Looked up
/// from `ctx.params["resolved_config"]` (populated by
/// `ConfigResolveHandler` at chain[0]). Falls back to the universal
/// defaults when the entry is missing.
struct ResolvedPolicy {
    mode: String,
    mode_source: &'static str,
    grace_secs: u64,
    grace_source: &'static str,
}

fn resolve_policy_from_config(
    ctx: &CompileContext<'_>,
    default_mode: &str,
    default_grace: u64,
    apply_defaults: bool,
) -> Result<ResolvedPolicy, EngineError> {
    let mut mode = default_mode.to_owned();
    let mut mode_source = "native_async default";
    let mut grace = default_grace;
    let mut grace_source = "native_async default";

    if let Some(resolved) = ctx.params.get("resolved_config") {
        // Layer 1: system / user / project defaults (already merged by
        // `ConfigResolveHandler` deep_merge resolution). Runtime descriptor rich
        // form is also an implementation default, so callers may skip this layer
        // when preserving an item-local rich form while still allowing exact
        // execution.yaml item overrides below.
        if apply_defaults {
            if let Some(defaults) = resolved.get("defaults") {
                if let Some(s) = defaults.get("cancellation_mode").and_then(Value::as_str) {
                    mode = s.to_owned();
                    mode_source = "resolved_config.defaults.cancellation_mode";
                }
                if let Some(n) = defaults
                    .get("cancellation_grace_secs")
                    .and_then(Value::as_u64)
                {
                    grace = n;
                    grace_source = "resolved_config.defaults.cancellation_grace_secs";
                }
            }
        }

        // Layer 2: per-tool overrides keyed by the root item id, not by the
        // runtime selected by that item.
        let root_tool_id = crate::runtime::root_item_bare_id(ctx.chain)?;
        if let Some(tool_overrides) = resolved.get("tools").and_then(|t| t.get(&root_tool_id)) {
            if let Some(s) = tool_overrides
                .get("cancellation_mode")
                .and_then(Value::as_str)
            {
                mode = s.to_owned();
                mode_source = "resolved_config.tools.<root>.cancellation_mode";
            }
            if let Some(n) = tool_overrides
                .get("cancellation_grace_secs")
                .and_then(Value::as_u64)
            {
                grace = n;
                grace_source = "resolved_config.tools.<root>.cancellation_grace_secs";
            }
        }
    }

    // Layer 3: direct params win. This covers caller-provided cancellation
    // params and the common non-root chain where ConfigResolveHandler runs on
    // the runtime hop and injects the already-merged execution policy keys,
    // rather than storing `resolved_config` for NativeAsyncHandler to inspect.
    if let Some(raw_mode) = ctx.params.get("cancellation_mode") {
        if let Some(s) = raw_mode.as_str() {
            mode = s.to_owned();
            mode_source = "params.cancellation_mode";
        } else {
            return Err(EngineError::InvalidRuntimeConfig {
                path: ctx.chain[ctx.current_index]
                    .source_path
                    .display()
                    .to_string(),
                reason: "cancellation_mode must be a string (`graceful` | `hard`)".to_string(),
            });
        }
    }
    if let Some(raw_grace) = ctx.params.get("cancellation_grace_secs") {
        if let Some(n) = raw_grace.as_u64() {
            grace = n;
            grace_source = "params.cancellation_grace_secs";
        } else {
            return Err(EngineError::InvalidRuntimeConfig {
                path: ctx.chain[ctx.current_index]
                    .source_path
                    .display()
                    .to_string(),
                reason: "cancellation_grace_secs must be an unsigned integer".to_string(),
            });
        }
    }

    Ok(ResolvedPolicy {
        mode,
        mode_source,
        grace_secs: grace,
        grace_source,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RichForm {
    #[serde(default)]
    cancel_mode: CancelModeChoice,
    #[serde(default = "default_grace")]
    graceful_shutdown_secs: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum CancelModeChoice {
    #[default]
    Graceful,
    Hard,
}

fn default_grace() -> u64 {
    DEFAULT_GRACEFUL_SECS
}

pub struct NativeAsyncHandler;

impl RuntimeHandler for NativeAsyncHandler {
    fn key(&self) -> &'static str {
        KEY
    }

    fn phase(&self) -> crate::runtime::HandlerPhase {
        crate::runtime::HandlerPhase::DecorateSpec
    }

    fn cardinality(&self) -> crate::runtime::HandlerCardinality {
        crate::runtime::HandlerCardinality::FirstWins
    }

    #[tracing::instrument(
        name = "engine:native_async",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];

        // Bool shorthand: `native_async: true` → resolve policy
        // from the system execution config (`execution.yaml` →
        // `defaults.cancellation_*` + per-tool `tools.<id>` overrides),
        // falling back to hardcoded constants only when nothing else
        // provides a value.
        // `native_async: false` is rejected loudly.
        let cancellation_mode = match block {
            Value::Bool(true) => {
                let policy =
                    resolve_policy_from_config(ctx, "graceful", DEFAULT_GRACEFUL_SECS, true)?;
                let cancellation_mode = match policy.mode.as_str() {
                    "hard" => CancellationMode::Hard,
                    "graceful" => CancellationMode::Graceful {
                        grace_secs: policy.grace_secs,
                    },
                    other => {
                        return Err(EngineError::InvalidRuntimeConfig {
                            path: intermediate.source_path.display().to_string(),
                            reason: format!(
                                "unknown cancellation_mode `{other}` in resolved \
                                 execution config (expected `graceful` | `hard`)"
                            ),
                        });
                    }
                };
                tracing::info!(
                    root_item_ref = %ctx.chain[0].resolved_ref,
                    item_ref = %intermediate.resolved_ref,
                    cancellation_mode = %policy.mode,
                    mode_source = policy.mode_source,
                    grace_secs = policy.grace_secs,
                    grace_source = policy.grace_source,
                    "native_async cancellation policy resolved"
                );
                cancellation_mode
            }
            Value::Bool(false) => {
                return Err(EngineError::InvalidRuntimeConfig {
                    path: intermediate.source_path.display().to_string(),
                    reason: "`native_async: false` is not supported — omit the block to disable"
                        .to_string(),
                });
            }
            other => {
                let rich: RichForm = serde_json::from_value(other.clone()).map_err(|e| {
                    EngineError::InvalidRuntimeConfig {
                        path: intermediate.source_path.display().to_string(),
                        reason: format!("invalid native_async block: {e}"),
                    }
                })?;
                let (default_mode, default_grace) = match rich.cancel_mode {
                    CancelModeChoice::Hard => ("hard", rich.graceful_shutdown_secs),
                    CancelModeChoice::Graceful => ("graceful", rich.graceful_shutdown_secs),
                };
                let policy = resolve_policy_from_config(
                    ctx,
                    default_mode,
                    default_grace,
                    ctx.current_index > 0,
                )?;
                let cancellation_mode = match policy.mode.as_str() {
                    "hard" => CancellationMode::Hard,
                    "graceful" => CancellationMode::Graceful {
                        grace_secs: policy.grace_secs,
                    },
                    other => {
                        return Err(EngineError::InvalidRuntimeConfig {
                            path: intermediate.source_path.display().to_string(),
                            reason: format!(
                                "unknown cancellation_mode `{other}` in resolved \
                                 execution config (expected `graceful` | `hard`)"
                            ),
                        });
                    }
                };
                tracing::info!(
                    root_item_ref = %ctx.chain[0].resolved_ref,
                    item_ref = %intermediate.resolved_ref,
                    cancellation_mode = %policy.mode,
                    mode_source = policy.mode_source,
                    grace_secs = policy.grace_secs,
                    grace_source = policy.grace_source,
                    "native_async cancellation policy resolved"
                );
                cancellation_mode
            }
        };

        ctx.spec_overrides.execution.native_async = Some(NativeAsyncSpec { cancellation_mode });

        // Subprocess-facing flag so tools can branch on
        // "do I drive my own event stream?". Spec field stays the
        // canonical source of truth for the daemon/runner; this env
        // var is purely a runtime convenience for tool code.
        ctx.env
            .insert("RYEOS_NATIVE_ASYNC".to_owned(), "1".to_owned());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_resolution::ResolutionRoots;
    use crate::kind_registry::KindRegistry;
    use crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors;
    use crate::runtime::{ChainIntermediate, HostEnvBindings, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    static NULL_PARAMS: Value = Value::Null;
    static EMPTY_HOST_ENV: std::sync::LazyLock<HostEnvBindings> =
        std::sync::LazyLock::new(HostEnvBindings::default);

    fn run(block: Value) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        run_with_params(block, Value::Null)
    }

    fn run_with_params(
        block: Value,
        initial_params: Value,
    ) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        run_with_chain(block, initial_params, None)
    }

    fn run_with_chain(
        block: Value,
        initial_params: Value,
        chain_override: Option<Vec<ChainIntermediate>>,
    ) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        run_with_chain_at(block, initial_params, chain_override, 0)
    }

    fn run_with_chain_at(
        block: Value,
        initial_params: Value,
        chain_override: Option<Vec<ChainIntermediate>>,
        current_index: usize,
    ) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        let chain = vec![ChainIntermediate {
            executor_id: "tool:demo".into(),
            resolved_ref: "tool:demo".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/demo.yaml"),
            parsed: json!({ "native_async": block.clone() }),
        }];
        let chain = chain_override.unwrap_or(chain);
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            env_sources: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: initial_params,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index,
            roots: &roots,
            parsers: &parsers,
            kinds: &kinds,
            trust_store: &trust,
            project_root: None,
            root_trust_class: crate::resolution::TrustClass::TrustedSystem,
            host_env: &EMPTY_HOST_ENV,
        };
        NativeAsyncHandler.apply(&block, &mut ctx)?;
        Ok((ctx.spec_overrides, ctx.env))
    }

    #[test]
    fn bool_true_sets_graceful_default() {
        let (overrides, env) = run(json!(true)).unwrap();
        let spec = overrides.execution.native_async.unwrap();
        assert_eq!(
            spec.cancellation_mode,
            CancellationMode::Graceful {
                grace_secs: DEFAULT_GRACEFUL_SECS
            }
        );
        assert_eq!(env.get("RYEOS_NATIVE_ASYNC").map(String::as_str), Some("1"));
    }

    #[test]
    fn bool_false_is_loud_error() {
        let err = run(json!(false)).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(
                    reason.contains("not supported"),
                    "expected explicit rejection message, got {reason}"
                );
            }
            other => panic!("expected InvalidRuntimeConfig, got {other:?}"),
        }
    }

    #[test]
    fn rich_form_hard_cancel() {
        let (overrides, _) = run(json!({ "cancel_mode": "hard" })).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Hard
        );
    }

    #[test]
    fn rich_form_graceful_with_custom_secs() {
        let (overrides, _) = run(json!({
            "cancel_mode": "graceful",
            "graceful_shutdown_secs": 30
        }))
        .unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 30 }
        );
    }

    #[test]
    fn rich_form_unknown_field_rejected() {
        let err = run(json!({ "cancel_mode": "graceful", "bogus": 1 })).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig, got {err:?}"
        );
    }

    #[test]
    fn bool_true_reads_grace_secs_from_resolved_config_defaults() {
        // Simulate config_resolve having populated `resolved_config`
        // from execution.yaml with a non-default grace value.
        let params = json!({
            "resolved_config": {
                "defaults": {
                    "cancellation_mode": "graceful",
                    "cancellation_grace_secs": 30
                }
            }
        });
        let (overrides, _) = run_with_params(json!(true), params).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 30 }
        );
    }

    #[test]
    fn bool_true_reads_hard_mode_from_resolved_config() {
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_mode": "hard" }
            }
        });
        let (overrides, _) = run_with_params(json!(true), params).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Hard
        );
    }

    #[test]
    fn per_tool_override_beats_defaults() {
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_grace_secs": 5 },
                "tools": {
                    "demo": { "cancellation_grace_secs": 90 }
                }
            }
        });
        let (overrides, _) = run_with_params(json!(true), params).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 90 }
        );
    }

    #[test]
    fn rich_form_overrides_resolved_config() {
        // Bool shorthand defers to system config; a root-item rich form is
        // explicit per-item intent and wins over execution defaults.
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_grace_secs": 999 }
            }
        });
        let (overrides, _) = run_with_params(
            json!({ "cancel_mode": "graceful", "graceful_shutdown_secs": 7 }),
            params,
        )
        .unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 7 }
        );
    }

    #[test]
    fn exact_execution_override_beats_root_rich_form() {
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_grace_secs": 999 },
                "tools": {
                    "demo": { "cancellation_grace_secs": 90 }
                }
            }
        });
        let (overrides, _) = run_with_params(
            json!({ "cancel_mode": "graceful", "graceful_shutdown_secs": 7 }),
            params,
        )
        .unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 90 }
        );
    }

    #[test]
    fn runtime_rich_form_is_default_for_root_execution_policy() {
        let block = json!({ "cancel_mode": "graceful", "graceful_shutdown_secs": 7 });
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_grace_secs": 30 },
                "tools": {
                    "my/app/tool": { "cancellation_grace_secs": 90 }
                }
            }
        });
        let chain = vec![
            ChainIntermediate {
                executor_id: "tool:my/runtimes/native".into(),
                resolved_ref: "tool:my/app/tool".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/tmp/tool.yaml"),
                parsed: json!({}),
            },
            ChainIntermediate {
                executor_id: "tool:my/runtimes/native".into(),
                resolved_ref: "tool:my/runtimes/native".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/tmp/runtime.yaml"),
                parsed: json!({ "native_async": block.clone() }),
            },
        ];

        let (overrides, _) = run_with_chain_at(block, params, Some(chain), 1).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 90 }
        );
    }

    #[test]
    fn direct_cancellation_params_beat_runtime_rich_form() {
        let block = json!({ "cancel_mode": "graceful", "graceful_shutdown_secs": 7 });
        let params = json!({
            "cancellation_mode": "graceful",
            "cancellation_grace_secs": 90
        });
        let chain = vec![
            ChainIntermediate {
                executor_id: "tool:my/runtimes/native".into(),
                resolved_ref: "tool:my/app/tool".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/tmp/tool.yaml"),
                parsed: json!({}),
            },
            ChainIntermediate {
                executor_id: "tool:my/runtimes/native".into(),
                resolved_ref: "tool:my/runtimes/native".into(),
                kind: "tool".into(),
                source_path: PathBuf::from("/tmp/runtime.yaml"),
                parsed: json!({ "native_async": block.clone() }),
            },
        ];

        let (overrides, _) = run_with_chain_at(block, params, Some(chain), 1).unwrap();
        assert_eq!(
            overrides.execution.native_async.unwrap().cancellation_mode,
            CancellationMode::Graceful { grace_secs: 90 }
        );
    }

    #[test]
    fn unknown_resolved_mode_is_loud_error() {
        let params = json!({
            "resolved_config": {
                "defaults": { "cancellation_mode": "yolo" }
            }
        });
        let err = run_with_params(json!(true), params).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(reason.contains("yolo"), "got {reason}");
            }
            other => panic!("expected InvalidRuntimeConfig, got {other:?}"),
        }
    }

    #[test]
    fn rich_form_no_enabled_field() {
        // Reject `enabled: true` since "presence means enabled".
        let err = run(json!({ "enabled": true })).unwrap_err();
        assert!(
            matches!(err, EngineError::InvalidRuntimeConfig { .. }),
            "expected InvalidRuntimeConfig (deny_unknown_fields catches `enabled`), \
             got {err:?}"
        );
    }
}
