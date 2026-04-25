//! `NativeResumeHandler` — claims the top-level `native_resume` block.
//!
//! Presence of this block on a chain element flags the resulting
//! subprocess as **replay-aware**: the daemon will allocate a per-thread
//! checkpoint directory under `<thread_state_dir>/checkpoints/` and
//! inject `RYE_CHECKPOINT_DIR=<that path>` into the spawn env. On
//! daemon restart, `reconcile.rs` consults the durable launch manifest
//! and, when `native_resume` is declared, attempts automatic respawn
//! with `RYE_RESUME=1` up to `max_auto_resume_attempts` times before
//! giving up and marking the thread `failed`.
//!
//! The tool itself is responsible for:
//!   - writing checkpoints to `RYE_CHECKPOINT_DIR/latest.json` (use
//!     `ryeos_runtime::CheckpointWriter`),
//!   - being idempotent / replay-safe when started with `RYE_RESUME=1`.
//!
//! Phase / cardinality: `DecorateSpec` / `FirstWins`. Resume policy
//! must be unambiguous; the FIRST chain element that declares the
//! block wins, mirroring `native_async`.
//!
//! ## YAML shapes accepted
//!
//! Bool shorthand:
//! ```yaml
//! native_resume: true
//! ```
//!
//! Rich form:
//! ```yaml
//! native_resume:
//!   checkpoint_interval_secs: 30   # default: 30 (advisory only)
//!   max_auto_resume_attempts: 1    # default: 1 (single retry)
//! ```
//!
//! `native_resume: false` is a hard error (omit the block to disable).
//!
//! Note: unlike `native_async`, this handler does NOT inject any env
//! at compile time — `RYE_CHECKPOINT_DIR` is allocated and injected by
//! the daemon at spawn time (it depends on the spawn-time thread state
//! dir, which the engine doesn't own). The handler's only job is to
//! mark the spec so the daemon knows to do that allocation.

use serde::Deserialize;
use serde_json::Value;

use crate::contracts::NativeResumeSpec;
use crate::error::EngineError;
use crate::runtime::{CompileContext, RuntimeHandler};

pub const KEY: &str = "native_resume";

const DEFAULT_CHECKPOINT_INTERVAL_SECS: u64 = 30;
const DEFAULT_MAX_AUTO_RESUME_ATTEMPTS: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RichForm {
    #[serde(default = "default_checkpoint_interval")]
    checkpoint_interval_secs: u64,
    #[serde(default = "default_max_attempts")]
    max_auto_resume_attempts: u32,
}

fn default_checkpoint_interval() -> u64 {
    DEFAULT_CHECKPOINT_INTERVAL_SECS
}

fn default_max_attempts() -> u32 {
    DEFAULT_MAX_AUTO_RESUME_ATTEMPTS
}

pub struct NativeResumeHandler;

impl RuntimeHandler for NativeResumeHandler {
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
        name = "engine:native_resume",
        skip(self, block, ctx),
        fields(
            item_ref = %ctx.chain[ctx.current_index].resolved_ref,
            chain_index = ctx.current_index,
        )
    )]
    fn apply(&self, block: &Value, ctx: &mut CompileContext<'_>) -> Result<(), EngineError> {
        let intermediate = &ctx.chain[ctx.current_index];

        let spec = match block {
            Value::Bool(true) => NativeResumeSpec::default(),
            Value::Bool(false) => {
                return Err(EngineError::InvalidRuntimeConfig {
                    path: intermediate.source_path.display().to_string(),
                    reason:
                        "`native_resume: false` is not supported — omit the block to disable"
                            .to_string(),
                });
            }
            other => {
                let rich: RichForm =
                    serde_json::from_value(other.clone()).map_err(|e| {
                        EngineError::InvalidRuntimeConfig {
                            path: intermediate.source_path.display().to_string(),
                            reason: format!("invalid native_resume block: {e}"),
                        }
                    })?;
                NativeResumeSpec {
                    checkpoint_interval_secs: rich.checkpoint_interval_secs,
                    max_auto_resume_attempts: rich.max_auto_resume_attempts,
                }
            }
        };

        ctx.spec_overrides.execution.native_resume = Some(spec);

        // Note: `RYE_CHECKPOINT_DIR` is intentionally NOT injected here.
        // It depends on the spawn-time `<thread_state_dir>/checkpoints/`
        // path, which the daemon allocates per-thread at attach time.
        // See `RuntimeLaunchMetadata::checkpoint_dir` and the runner's
        // env injection in `ryeosd::execution::runner`.

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

    fn run(block: Value) -> Result<SpecOverrides, EngineError> {
        let chain = vec![ChainIntermediate {
            executor_id: "tool:demo".into(),
            resolved_ref: "tool:demo".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/demo.yaml"),
            parsed: json!({ "native_resume": block.clone() }),
        }];
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: Value::Null,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index: 0,
            roots: &roots,
            parsers: &parsers,
            kinds: &kinds,
            trust_store: &trust,
            project_root: None,
        };
        NativeResumeHandler.apply(&block, &mut ctx)?;
        Ok(ctx.spec_overrides)
    }

    #[test]
    fn bool_true_uses_defaults() {
        let overrides = run(json!(true)).unwrap();
        let spec = overrides.execution.native_resume.unwrap();
        assert_eq!(spec.checkpoint_interval_secs, DEFAULT_CHECKPOINT_INTERVAL_SECS);
        assert_eq!(spec.max_auto_resume_attempts, DEFAULT_MAX_AUTO_RESUME_ATTEMPTS);
    }

    #[test]
    fn bool_false_is_loud_error() {
        let err = run(json!(false)).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(reason.contains("not supported"), "got: {reason}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rich_form_overrides_defaults() {
        let overrides = run(json!({
            "checkpoint_interval_secs": 60,
            "max_auto_resume_attempts": 3
        }))
        .unwrap();
        let spec = overrides.execution.native_resume.unwrap();
        assert_eq!(spec.checkpoint_interval_secs, 60);
        assert_eq!(spec.max_auto_resume_attempts, 3);
    }

    #[test]
    fn rich_form_partial_uses_defaults_for_missing_fields() {
        let overrides = run(json!({"max_auto_resume_attempts": 5})).unwrap();
        let spec = overrides.execution.native_resume.unwrap();
        assert_eq!(spec.checkpoint_interval_secs, DEFAULT_CHECKPOINT_INTERVAL_SECS);
        assert_eq!(spec.max_auto_resume_attempts, 5);
    }

    #[test]
    fn rich_form_unknown_field_is_loud_error() {
        let err = run(json!({"surprise": 1})).unwrap_err();
        match err {
            EngineError::InvalidRuntimeConfig { reason, .. } => {
                assert!(reason.contains("invalid native_resume"), "got: {reason}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn handler_does_not_inject_env_at_compile_time() {
        // RYE_CHECKPOINT_DIR is daemon-allocated at spawn time, not
        // compile-time-derived. Verify the handler leaves env alone.
        let chain = vec![ChainIntermediate {
            executor_id: "tool:demo".into(),
            resolved_ref: "tool:demo".into(),
            kind: "tool".into(),
            source_path: PathBuf::from("/tmp/demo.yaml"),
            parsed: json!({ "native_resume": true }),
        }];
        let parsers = dispatcher_with_canonical_bundle_descriptors();
        let kinds = KindRegistry::empty();
        let trust = TrustStore::empty();
        let roots = ResolutionRoots { ordered: vec![] };
        let mut ctx = CompileContext {
            template_ctx: TemplateContext::new(PathBuf::from("/dev/null")),
            env: HashMap::new(),
            spec_overrides: SpecOverrides::default(),
            params: Value::Null,
            original_params: &NULL_PARAMS,
            chain: &chain,
            current_index: 0,
            roots: &roots,
            parsers: &parsers,
            kinds: &kinds,
            trust_store: &trust,
            project_root: None,
        };
        NativeResumeHandler.apply(&json!(true), &mut ctx).unwrap();
        assert!(
            !ctx.env.contains_key("RYE_CHECKPOINT_DIR"),
            "handler must not inject RYE_CHECKPOINT_DIR; daemon owns that"
        );
        assert!(!ctx.env.contains_key("RYE_RESUME"));
    }
}
