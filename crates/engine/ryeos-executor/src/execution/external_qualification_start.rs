//! Bounded readiness custody for one qualification-only external guest.
//!
//! This driver stops at the authenticated Ready/Release boundary. A local
//! provider spawn owner must exist before the one-shot connector is prepared;
//! readiness alone never starts a provider or publishes qualification.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_app::external_placement::{
    ExternalCandidateCleanupLifeline, ExternalCandidateStartProgress, ExternalQualificationStart,
};
use ryeos_state::external_execution::ExecutionChannelBinding;

use super::external_qualification_inputs::PreparedExternalQualificationGuest;

/// The exact released channel and its remote cleanup obligation. Dropping this
/// value requests cleanup; a future provider owner must retain it throughout
/// connector use and independently prove eventual settlement.
pub(crate) struct ExternalQualificationReady {
    binding: ExecutionChannelBinding,
    _cleanup: ExternalCandidateCleanupLifeline,
}

impl ExternalQualificationReady {
    pub(crate) fn binding(&self) -> &ExecutionChannelBinding {
        &self.binding
    }

    pub(crate) fn placement(&self) -> &str {
        &self.binding.placement_thread_id
    }
}

/// Reserve the prepared Q guest and immediately arm its cleanup lifeline before
/// polling. Callers cannot leave a successfully reserved owner without this
/// process-local cancellation obligation. The one-shot connector remains for
/// a future local provider spawn owner that can retain all of its resources.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reserve_and_await_external_qualification_ready(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    prepared: PreparedExternalQualificationGuest,
) -> Result<ExternalQualificationReady> {
    let start = super::external_qualification_inputs::reserve_external_qualification_guest(
        state,
        context,
        coordinate,
        qualification_operation_id,
        limits,
        prepared,
    )?;
    await_external_qualification_ready(start)
}

/// Consume the one-use start owner and its original guest descriptors until
/// signed Ready and controller Release are both durable.
fn await_external_qualification_ready(
    mut start: ExternalQualificationStart<'_>,
) -> Result<ExternalQualificationReady> {
    let deadline = start.deadline();
    let cleanup = start.take_cleanup_lifeline()?;
    let ready = (|| -> Result<ExecutionChannelBinding> {
        loop {
            ensure!(
                !deadline.has_elapsed(),
                "external qualification did not reach authenticated readiness before its deadline"
            );
            match start.advance().map_err(anyhow::Error::from)? {
                ExternalCandidateStartProgress::Ready(binding) => {
                    ensure!(
                        binding.placement_thread_id == start.placement()
                            && matches!(
                                &binding.execution_mode,
                                ryeos_state::external_execution::ExternalExecutionMode::QualificationStructuredSession {}
                            )
                            && binding.candidate_export_max_bytes == 0,
                        "external qualification readiness changed its zero-export placement"
                    );
                    return Ok(binding);
                }
                ExternalCandidateStartProgress::AllocationPending
                | ExternalCandidateStartProgress::OccurrenceBound
                | ExternalCandidateStartProgress::SupervisorPending
                | ExternalCandidateStartProgress::AttachmentPending
                | ExternalCandidateStartProgress::ChannelAttached => {
                    lillux::time::sleep(
                        deadline
                            .remaining()
                            .min(lillux::time::Duration::from_millis(50)),
                    );
                }
                ExternalCandidateStartProgress::CleanupProved => {
                    bail!("external qualification ended without an executable occurrence")
                }
                ExternalCandidateStartProgress::CleanupRequired => {
                    bail!("external qualification startup requires authoritative cleanup")
                }
            }
        }
    })();
    match ready {
        Ok(binding) => Ok(ExternalQualificationReady {
            binding,
            _cleanup: cleanup,
        }),
        Err(error) => {
            // The lifeline retains cancellation even when no local provider
            // was launched. Cleanup proof is a separate durable observation.
            drop(cleanup);
            Err(error
                .context(
                    "external qualification startup failed; cleanup attempted by retained owner",
                )
                .context(ryeos_app::persistent_session::PersistentSessionCleanupUnproved))
        }
    }
}
