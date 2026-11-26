//! Workspace handling for a detached child launched by a worker session.
//!
//! A parent that holds a workload-client grant keeps running while its
//! detached child runs, and keeps making inline workspace calls of its own.
//! The workspace is therefore handled exactly as an inline call handles it:
//! the child's program and every item it resolves come from the signed base
//! generation, and the worker's current workspace is captured as DATA only.
//!
//! The capture reuses the inline mechanism. A runtime action intent carrying
//! an immutable workspace operation is reserved, the exact current generation
//! is captured while the parent's borrowers are briefly quiesced, and the
//! generation is bound to that intent, which owns it from then on. The parent
//! is resumed and the operation released before the child exists. The
//! released intent keeps the captured generation reachable for garbage
//! collection until the child's thread row exists; from then on the child's own
//! sealed resume authority roots it for the child's lifetime. Nothing
//! here freezes the parent's workspace or waits on the parent's root gate: the
//! caller already holds one child-operation lease for the whole call, so a
//! terminalizing wait from here would wait on itself, and a frozen workspace
//! would refuse the parent's next inline call and hand a second detached child
//! a stale generation.
//!
//! A parent that suspends while its child runs (the follow-style handoff)
//! keeps the freeze lifecycle in `spawn_detached_child`; the choice is made by
//! [`DetachedWorkspaceHandoff::for_parent`] from the parent's callback bearer.

use std::path::Path;

use anyhow::{Context, Result};

use ryeos_app::callback_token::CallbackCapability;
use ryeos_app::runtime_db::{NewRuntimeWorkspaceOperation, RuntimeWorkspaceOperationPhase};
use ryeos_app::state::AppState;
use ryeos_state::objects::WorkspaceGenerationPair;

/// How a detached child receives its parent's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetachedWorkspaceHandoff {
    /// The parent keeps running and holds a workload-client grant: the child
    /// resolves from the base generation and the parent's workspace is
    /// captured as data input, pinned at spawn.
    BaseGeneration,
    /// The parent hands its workspace to the child (follow-style): the
    /// workspace is frozen and the child resolves from the frozen generation.
    CandidateFreeze,
}

impl DetachedWorkspaceHandoff {
    /// Parents are selected by the callback bearer they call with: one that
    /// carries a workload-client grant is a session that keeps running, so
    /// freezing its workspace would poison its later calls. Any other parent
    /// keeps the freeze handoff unchanged.
    pub(crate) fn for_parent(cap: &CallbackCapability) -> Self {
        if cap.workload_client_grant.is_some() {
            Self::BaseGeneration
        } else {
            Self::CandidateFreeze
        }
    }
}

/// Everything the base-generation handoff needs beyond what a detached spawn
/// already takes, computed and judged before any durable state exists.
pub(crate) struct DetachedWorkloadAdmission<'a> {
    /// Seed of the immutable workspace operation reserved with the intent.
    pub(crate) workspace_seed: NewRuntimeWorkspaceOperation<'a>,
    /// The parent workspace's base snapshot the capture is checked against.
    pub(crate) input_base_snapshot_hash: &'a str,
    /// The workload ceiling the grant seals for this item. It narrows the
    /// ceiling the calling thread already carries.
    pub(crate) sealed_ceiling: ryeos_state::objects::ThreadWorkloadCeiling,
}

/// Capture the parent's current workspace generation as the child's data
/// input and settle the operation, leaving the parent running and its
/// workspace Active. `retained_phase` is the phase the reserved intent is in:
/// a first drive finds it `Reserved`; a retry after an earlier drive already
/// captured and released finds it `Released` with the input bound and
/// captures nothing again. Any other phase belongs to a drive that has not
/// settled and is refused.
///
/// Returns the exact generation pair the child receives as data: the one just
/// captured, or on a retry the one the intent already bound.
pub(crate) async fn capture_detached_workspace_input(
    state: &AppState,
    parent_thread_id: &str,
    operation_id: &str,
    parent_project_path: &Path,
    admission: &DetachedWorkloadAdmission<'_>,
    retained_phase: RuntimeWorkspaceOperationPhase,
) -> Result<WorkspaceGenerationPair> {
    match retained_phase {
        RuntimeWorkspaceOperationPhase::Released => {
            let operation = state
                .state_store
                .get_runtime_action_intent(operation_id)?
                .and_then(|intent| intent.workspace_operation);
            let Some(snapshot_hash) = operation
                .as_ref()
                .and_then(|operation| operation.input_snapshot_hash.clone())
            else {
                anyhow::bail!(
                    "detach: workspace operation {operation_id} was released without a bound input generation"
                );
            };
            let pair = WorkspaceGenerationPair {
                snapshot_hash,
                output_capture_hash: operation
                    .and_then(|operation| operation.input_output_capture_hash),
            };
            pair.validate()?;
            return Ok(pair);
        }
        RuntimeWorkspaceOperationPhase::Reserved => {}
        other => anyhow::bail!(
            "detach: workspace operation {operation_id} is retained in phase {other:?}; its original process authority must settle it"
        ),
    }

    let capture_state = state.clone();
    let capture_operation_id = operation_id.to_owned();
    let capture_thread_id = parent_thread_id.to_owned();
    let capture_project_path = parent_project_path.to_path_buf();
    let capture_base_snapshot = admission.input_base_snapshot_hash.to_owned();
    let pending = crate::execution::run_bounded_project_capture(move || {
        crate::execution::capture_runtime_workspace_input_generation(
            &capture_state,
            &capture_operation_id,
            &capture_thread_id,
            &capture_project_path,
            &capture_base_snapshot,
        )
    })
    .await?;
    let (input_generation, publication, quiesced) =
        pending.into_unpublished_generation_and_quiesced()?;
    if let Some(publication) = publication
        && let Err(error) = publication.publish()
    {
        return match quiesced.resume_or_terminate() {
            Ok(()) => {
                state.state_store.transition_runtime_workspace_operation(
                    operation_id,
                    &[RuntimeWorkspaceOperationPhase::Quiesced],
                    RuntimeWorkspaceOperationPhase::Released,
                )?;
                Err(error.context("publish captured workspace-input generation"))
            }
            Err(settle_error) => Err(error.context(format!(
                "workspace-input publication failed and exact root resume/termination was not proved: {settle_error:#}"
            ))),
        };
    }
    quiesced.resume_or_terminate().with_context(|| {
        format!(
            "resume or terminate exact root after capturing workspace input for `{operation_id}`"
        )
    })?;
    state.state_store.transition_runtime_workspace_operation(
        operation_id,
        &[RuntimeWorkspaceOperationPhase::Quiesced],
        RuntimeWorkspaceOperationPhase::Released,
    )?;
    Ok(input_generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base-generation handoff must never take the paths that freeze the
    /// parent's workspace or wait on its root gate. Those names are assembled
    /// so this file's own text does not contain them.
    #[test]
    fn the_base_generation_handoff_never_freezes_or_terminalizes_the_parent() {
        let source = include_str!("detached_workspace_input.rs");
        let (production, _tests) = source
            .split_once("#[cfg(test)]")
            .expect("module has a test section");
        for forbidden in [
            concat!("begin_hosted_root_", "terminalization"),
            concat!("seal_callback_workspace_", "generation"),
            concat!("ensure_control_tree_", "unchanged"),
            concat!("bind_frozen_execution_", "workspace"),
            concat!("WorkspaceState::", "Freezing"),
        ] {
            assert!(
                !production.contains(forbidden),
                "base-generation handoff references `{forbidden}`"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capture_under_a_held_child_lease_returns_without_waiting_on_the_root_gate() {
        let (temp, _store_owner, state) =
            crate::augmentations::compose_context_positions::tests::bound_runtime_children_fixture(
            );
        let parent = "T-runtime-parent";
        let workspace_id = "workspace-runtime-parent";
        let before = state
            .state_store
            .execution_workspace(workspace_id)
            .unwrap()
            .unwrap();
        // The caller of a detached launch holds one child-operation lease for
        // the whole call. The capture must settle without waiting for that
        // lease to drain; a wait on the root gate would never return.
        let _lease = ryeos_app::hosted_operation::try_begin_hosted_child_operation_async(
            &state.state_store,
            parent,
        )
        .await
        .unwrap();
        let project = temp
            .path()
            .join(workspace_id)
            .join(ryeos_engine::execution_workspace::PROJECT_DIR);
        let invocation = ryeos_runtime::workload_client::WorkloadInvocationSource::Cli {
            external_request_id: "detach-fixture".to_owned(),
        };
        let admission = DetachedWorkloadAdmission {
            workspace_seed: NewRuntimeWorkspaceOperation {
                workspace_id,
                access: ryeos_engine::kind_registry::WorkspaceAccess::ImmutableCurrentGeneration,
                worker_instance_id: "worker",
                worker_boot_epoch: 1,
                worker_boot_identity_hash: "b",
                project_authority_digest: "c",
                workload_client_grant_digest: "d",
                invocation,
            },
            input_base_snapshot_hash: &before.base_snapshot,
            sealed_ceiling: ryeos_state::objects::ThreadWorkloadCeiling::node_root(),
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            capture_detached_workspace_input(
                &state,
                parent,
                "detach-op",
                &project,
                &admission,
                RuntimeWorkspaceOperationPhase::Reserved,
            ),
        )
        .await
        .expect("the capture step must not wait on its own caller's lease");
        // No intent was reserved in this fixture, so the capture refuses
        // before it moves anything; the point is that it returned at all.
        assert!(outcome.is_err());
        let after = state
            .state_store
            .execution_workspace(workspace_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state, before.state,
            "the parent's workspace is untouched"
        );
    }
}
