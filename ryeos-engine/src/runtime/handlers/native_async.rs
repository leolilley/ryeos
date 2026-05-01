//! `NativeAsyncHandler` — claims the top-level `native_async` block.
//!
//! Presence of this block on a chain element flags the resulting
//! subprocess as **owning its own event stream**: the runner injects
//! `RYE_NATIVE_ASYNC=1` into the spawn env so the subprocess knows
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
    grace_secs: u64,
}

fn resolve_policy_from_config(
    ctx: &CompileContext<'_>,
    default_mode: &str,
    default_grace: u64,
) -> ResolvedPolicy {
    let mut mode = default_mode.to_owned();
    let mut grace = default_grace;

    let Some(resolved) = ctx.params.get("resolved_config") else {
        return ResolvedPolicy { mode, grace_secs: grace };
    };

    // Layer 1: system / user / project defaults (already merged by
    // `ConfigResolveHandler` deep_merge resolution).
    if let Some(defaults) = resolved.get("defaults") {
        if let Some(s) = defaults.get("cancellation_mode").and_then(Value::as_str) {
            mode = s.to_owned();
        }
        if let Some(n) = defaults.get("cancellation_grace_secs").and_then(Value::as_u64) {
            grace = n;
        }
    }

    // Layer 2: per-tool overrides keyed by chain[0]'s executor_id.
    let root_tool_id = ctx.chain.first().map(|c| c.executor_id.as_str()).unwrap_or("");
    if let Some(tool_overrides) = resolved
        .get("tools")
        .and_then(|t| t.get(root_tool_id))
    {
        if let Some(s) = tool_overrides.get("cancellation_mode").and_then(Value::as_str) {
            mode = s.to_owned();
        }
        if let Some(n) = tool_overrides
            .get("cancellation_grace_secs")
            .and_then(Value::as_u64)
        {
            grace = n;
        }
    }

    ResolvedPolicy { mode, grace_secs: grace }
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
                    resolve_policy_from_config(ctx, "graceful", DEFAULT_GRACEFUL_SECS);
                match policy.mode.as_str() {
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
                }
            }
            Value::Bool(false) => {
                return Err(EngineError::InvalidRuntimeConfig {
                    path: intermediate.source_path.display().to_string(),
                    reason:
                        "`native_async: false` is not supported — omit the block to disable"
                            .to_string(),
                });
            }
            other => {
                let rich: RichForm =
                    serde_json::from_value(other.clone()).map_err(|e| {
                        EngineError::InvalidRuntimeConfig {
                            path: intermediate.source_path.display().to_string(),
                            reason: format!("invalid native_async block: {e}"),
                        }
                    })?;
                match rich.cancel_mode {
                    CancelModeChoice::Hard => CancellationMode::Hard,
                    CancelModeChoice::Graceful => CancellationMode::Graceful {
                        grace_secs: rich.graceful_shutdown_secs,
                    },
                }
            }
        };

        ctx.spec_overrides.execution.native_async = Some(NativeAsyncSpec { cancellation_mode });

        // Subprocess-facing flag so tools can branch on
        // "do I drive my own event stream?". Spec field stays the
        // canonical source of truth for the daemon/runner; this env
        // var is purely a runtime convenience for tool code.
        ctx.env
            .insert("RYE_NATIVE_ASYNC".to_owned(), "1".to_owned());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_resolution::ResolutionRoots;
    use crate::kind_registry::KindRegistry;
    use crate::parsers::test_helpers::dispatcher_with_canonical_bundle_descriptors;
    use crate::runtime::{ChainIntermediate, SpecOverrides, TemplateContext};
    use crate::trust::TrustStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    static NULL_PARAMS: Value = Value::Null;

    fn run(block: Value) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        run_with_params(block, Value::Null)
    }

    fn run_with_params(
        block: Value,
        initial_params: Value,
    ) -> Result<(SpecOverrides, HashMap<String, String>), EngineError> {
        let chain = vec![ChainIntermediate {
            executor_id: "tool:demo".into(),
            resolved_ref: "tool:demo".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/demo.yaml"),
            parsed: json!({ "native_async": block.clone() }),
        }];
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: initial_params,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index: 0,
            roots: &roots,
            parsers: &parsers,
            kinds: &kinds,
            trust_store: &trust,
            project_root: None,
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
        assert_eq!(env.get("RYE_NATIVE_ASYNC").map(String::as_str), Some("1"));
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
                    "tool:demo": { "cancellation_grace_secs": 90 }
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
        // Bool shorthand defers to system config; rich form is
        // explicit per-tool intent and wins.
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
