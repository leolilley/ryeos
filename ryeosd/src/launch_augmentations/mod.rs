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

pub mod compose_context_positions;
pub mod projection;

use ryeos_engine::kind_registry::{ExecutionSchema, LaunchAugmentationDecl};
use ryeos_engine::resolution::ResolutionOutput;

use crate::dispatch_error::DispatchError;

/// Run all launch augmentations declared on the kind's schema.
///
/// Mutates `resolution.composed.derived` in place — successful
/// augmentations write their outputs back into the composed view so
/// the parent runtime receives them via the envelope.
pub async fn run_augmentations(
    exec: &ExecutionSchema,
    resolution: &mut ResolutionOutput,
    parent_thread_id: &str,
    project_path: &std::path::Path,
    engine: &ryeos_engine::engine::Engine,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    state: &crate::state::AppState,
) -> Result<(), LaunchAugmentationError> {
    for decl in &exec.launch_augmentations {
        match decl {
            LaunchAugmentationDecl::ComposeContextPositions { .. } => {
                compose_context_positions::run(
                    decl,
                    resolution,
                    parent_thread_id,
                    project_path,
                    engine,
                    plan_ctx,
                    principal_fingerprint,
                    state,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// Convert a `LaunchAugmentationError` into a `DispatchError` for
/// the dispatch layer to surface to the HTTP caller.
impl From<LaunchAugmentationError> for DispatchError {
    fn from(e: LaunchAugmentationError) -> Self {
        match &e {
            LaunchAugmentationError::BadRef { .. }
            | LaunchAugmentationError::ProjectionInvariant { .. }
            | LaunchAugmentationError::ParseRef(_) => DispatchError::InvalidRef(
                "launch_augmentation".to_string(),
                e.to_string(),
            ),
            LaunchAugmentationError::ResolutionFailed { .. } => DispatchError::InvalidRef(
                "launch_augmentation".to_string(),
                e.to_string(),
            ),
            LaunchAugmentationError::ChildBootstrap { .. }
            | LaunchAugmentationError::ChildFailed { .. }
            | LaunchAugmentationError::RuntimeRegistry(_) => {
                DispatchError::SubprocessRunFailed {
                    item_ref: "launch_augmentation".to_string(),
                    detail: e.to_string(),
                }
            }
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

    #[error("parse canonical ref: {0}")]
    ParseRef(String),

    #[error("projection invariant violated: {reason}")]
    ProjectionInvariant { reason: String },

    #[error("child {kind}/{op} bootstrap failed: exit={exit_code}, stderr={stderr}")]
    ChildBootstrap {
        kind: String,
        op: String,
        exit_code: i32,
        stderr: String,
    },

    #[error("child {kind}/{op} returned failure: {error:?}")]
    ChildFailed {
        kind: String,
        op: String,
        error: Option<ryeos_runtime::op_wire::BatchOpError>,
    },

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("runtime registry lookup failed: {0}")]
    RuntimeRegistry(String),

    #[error("thread infra: {0}")]
    Threads(String),
}
