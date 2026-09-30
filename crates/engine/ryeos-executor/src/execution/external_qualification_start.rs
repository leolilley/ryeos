//! Bounded readiness custody for one qualification-only external guest.
//!
//! Ready/Release is the handoff to the one-shot local Worker owner. The full
//! driver below retains that owner through normal settlement; readiness or
//! transport completion alone never publishes qualification.

use anyhow::{Context as _, Result, bail, ensure};
use ryeos_app::external_placement::{
    ExternalCandidateCleanupLifeline, ExternalCandidateStartProgress, ExternalQualificationStart,
};
use ryeos_app::operator_external_content::product_qualification::PreparedExternalQualificationRoot;
use ryeos_state::external_execution::ExecutionChannelBinding;

use super::external_qualification_inputs::PreparedExternalQualificationGuest;

/// One transport-only attempt from retained operation coordinates. The
/// accepted root, signed Worker program, backend binding, scenario and limits
/// are selected by the existing authenticated preparers and current policy;
/// callers cannot supply replacement guest descriptors or a verifier verdict.
/// Scratch cleanup is attempted after every prepared attempt. If allocation
/// or native/provider settlement is uncertain, the original descriptor stays
/// in the daemon registry and this operation returns a cleanup fence.
pub fn run_consumer_runtime_verification_transport(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
) -> Result<()> {
    let placement = ryeos_state::external_execution::qualification_session::qualification_candidate_placement_key(
        coordinate,
        qualification_operation_id,
    )?;
    let limits = state
        .node_policy
        .require::<ryeos_app::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
        .closure_limits()?;
    if ryeos_app::external_qualification_guest_scratch::prove_retained_qualification_transport_completed(
        state,
        context,
        coordinate,
        qualification_operation_id,
        limits,
    )? {
        return Ok(());
    }
    let scratch = ryeos_app::external_qualification_guest_scratch::prepare_original_qualification_guest_scratch(
        state,
        context,
        coordinate,
        qualification_operation_id,
        limits,
    )?;
    let transport = (|| {
        ensure!(
            scratch.placement() == placement,
            "Q prepared scratch changed its stable placement"
        );
        let prepared = super::external_qualification_inputs::prepare_external_qualification_guest(
            state,
            context,
            coordinate,
            qualification_operation_id,
            limits,
            scratch.pinned_root()?,
        )?;
        run_prepared_external_qualification_to_normal_terminal(
            state,
            context,
            coordinate,
            qualification_operation_id,
            limits,
            prepared,
        )
    })();
    let cleanup = ryeos_app::external_qualification_guest_scratch::cleanup_original_qualification_guest_scratch_if_present(
        state,
        &placement,
    );
    match (transport, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(cleanup_error
            .context("Q transport settled but original guest scratch remains cleanup-unproved")),
        (Err(error), Err(cleanup_error)) => Err(error.context(format!(
            "Q original guest scratch remains cleanup-unproved: {cleanup_error:#}"
        ))),
    }
}

/// The exact released channel and its remote cleanup obligation. Dropping this
/// value requests cleanup; a future provider owner must retain it throughout
/// connector use and independently prove eventual settlement.
pub(crate) struct ExternalQualificationReady {
    binding: ExecutionChannelBinding,
    root: PreparedExternalQualificationRoot,
    _cleanup: ExternalCandidateCleanupLifeline,
}

impl ExternalQualificationReady {
    pub(crate) fn binding(&self) -> &ExecutionChannelBinding {
        &self.binding
    }

    pub(crate) fn placement(&self) -> &str {
        &self.binding.placement_thread_id
    }

    pub(crate) fn root(&self) -> &PreparedExternalQualificationRoot {
        &self.root
    }

    /// Release the process-local cancellation obligation only after the app
    /// has rejoined exact normal provider terminal evidence in its journal.
    pub(crate) fn disarm_after_normal_terminal(self) -> Result<()> {
        self._cleanup.disarm_after_qualification_normal_terminal()
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

/// Run one prepared Q occurrence through its only signed Worker command and
/// exact normal settlement. This is a transport lifecycle result, never a
/// verifier verdict or portable product qualification claim. The caller must
/// first provide independently authenticated Q guest inputs; admission still
/// refuses any signed backend lacking the required offline capabilities.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_prepared_external_qualification_to_normal_terminal(
    state: &ryeos_app::state::AppState,
    context: &ryeos_app::handler_context::HandlerContext,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    prepared: PreparedExternalQualificationGuest,
) -> Result<()> {
    let ready = reserve_and_await_external_qualification_ready(
        state,
        context,
        coordinate,
        qualification_operation_id,
        limits,
        prepared,
    )?;
    super::external_qualification_local_worker::run_external_qualification_to_normal_terminal(
        state, coordinate, ready,
    )
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
            root: start.into_authenticated_root(),
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
