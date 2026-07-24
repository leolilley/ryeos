//! Launch augmentation interpreter.
//!
//! Between resolution and parent runtime spawn, the daemon walks any
//! `execution.launch_augmentations` declared on the kind's schema and
//! interprets each variant. This is purely additive — adding a new
//! augmentation variant = a new arm in `run_augmentations` + a new
//! handler module. Engine code unchanged.
//!
//! Every augmentation failure aborts the parent launch with a typed
//! `LaunchAugmentationError`. No silent fallback.

mod compose_cache;
pub mod compose_context_positions;
pub mod projection;

use ryeos_engine::kind_registry::{ExecutionSchema, LaunchAugmentationDecl};
use ryeos_engine::resolution::ResolutionOutput;

use crate::dispatch_error::DispatchError;

#[derive(Debug, Clone)]
pub struct LaunchAugmentationAudit {
    pub event_type: ryeos_runtime::events::RuntimeEventType,
    pub payload: serde_json::Value,
}

/// Run all launch augmentations declared on the kind's schema.
///
/// Mutates `resolution.composed.derived` in place — successful
/// augmentations write their outputs back into the composed view so
/// the parent runtime receives them via the envelope.
// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub async fn run_augmentations(
    exec: &ExecutionSchema,
    resolution: &mut ResolutionOutput,
    parent_thread_id: &str,
    project_path: &std::path::Path,
    engine: &ryeos_engine::engine::Engine,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    state: &ryeos_app::state::AppState,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
) -> Result<Vec<LaunchAugmentationAudit>, LaunchAugmentationError> {
    let mut audits = Vec::new();
    for decl in &exec.launch_augmentations {
        match decl {
            LaunchAugmentationDecl::ComposeContextPositions { .. } => {
                audits.extend(
                    compose_context_positions::run(
                        decl,
                        resolution,
                        parent_thread_id,
                        project_path,
                        engine,
                        provenance,
                        plan_ctx,
                        principal_fingerprint,
                        state,
                        launch_timings,
                    )
                    .await?,
                );
            }
        }
    }
    Ok(audits)
}

/// Convert a `LaunchAugmentationError` into a `DispatchError` for
/// the dispatch layer to surface to the HTTP caller.
impl From<LaunchAugmentationError> for DispatchError {
    fn from(e: LaunchAugmentationError) -> Self {
        match &e {
            LaunchAugmentationError::BadRef { .. }
            | LaunchAugmentationError::ProjectionInvariant { .. }
            | LaunchAugmentationError::ParseRef(_) => {
                DispatchError::InvalidRef("launch_augmentation".to_string(), e.to_string())
            }
            LaunchAugmentationError::ResolutionFailed { .. } => {
                DispatchError::InvalidRef("launch_augmentation".to_string(), e.to_string())
            }
            LaunchAugmentationError::EffectiveTrustRejected(_) => {
                DispatchError::InvalidRef("launch_augmentation".to_string(), e.to_string())
            }
            LaunchAugmentationError::ChildBootstrap { .. }
            | LaunchAugmentationError::ChildFailed { .. }
            | LaunchAugmentationError::RuntimeRegistry(_) => DispatchError::SubprocessRunFailed {
                item_ref: "launch_augmentation".to_string(),
                detail: e.to_string(),
            },
            LaunchAugmentationError::Threads(_) | LaunchAugmentationError::Serde(_) => {
                DispatchError::Internal(anyhow::anyhow!("{e}"))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchAugmentationError {
    #[error("position `{position}` value `{bad_ref}` is not a canonical ref (must start with `{expected_prefix}`)")]
    BadRef {
        position: String,
        bad_ref: String,
        expected_prefix: String,
    },

    #[error("resolve {ref_} failed: {source}")]
    ResolutionFailed {
        ref_: String,
        #[source]
        source: ryeos_engine::resolution::ResolutionError,
    },

    #[error("effective trust rejected: {0}")]
    EffectiveTrustRejected(String),

    #[error("parse canonical ref: {0}")]
    ParseRef(String),

    #[error("projection invariant violated: {reason}")]
    ProjectionInvariant { reason: String },

    #[error("child {kind}/{method} bootstrap failed: exit={exit_code}, stderr={stderr}")]
    ChildBootstrap {
        kind: String,
        method: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("child {kind}/{method} returned failure: {error:?}")]
    ChildFailed {
        kind: String,
        method: String,
        /// Boxed: the wire error dominates the enum's size, and this
        /// variant rides in every augmentation `Result`.
        error: Option<Box<ryeos_runtime::method_wire::MethodCallError>>,
    },

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("runtime registry lookup failed: {0}")]
    RuntimeRegistry(String),

    #[error("thread infra: {0}")]
    Threads(String),
}
