//! Ordinary external commands retain the same runner claim and result owner.
//! The host task owns execution; its oneshot is only result notification.

use super::*;
use anyhow::ensure;
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_app::external_placement::{
    self as placement, ExternalCandidateCleanupProgress, ExternalCandidateStartProgress,
};

#[derive(Clone, Copy)]
pub(super) enum Entry {
    Fresh,
    Recover,
}

// Field order matters on spawn failure and unwind: retain the launch claim
// until both cleanup owners have been dropped. No receiver owns these fields.
struct Owners {
    _remote: placement::ExternalCandidateCleanupLifeline,
    guard: ExecutionGuard,
    claim: ThreadLaunchClaim,
}

impl Drop for Owners {
    fn drop(&mut self) {
        // Unwinding must not let the generic local-process guard turn an
        // already-committed normal termination into a Kill tombstone. These
        // reads confer no success; the next claimed owner still finalizes.
        let preserve = !self.guard.state.state_store.process_attachment_admission_is_open()
            || self.guard.thread_id.as_deref().is_some_and(|thread_id| {
                matches!(self.guard.state.state_store.external_direct_normal_settlement_output(thread_id), Ok(Some(_)))
                    && matches!(self.guard.state.threads.get_thread(thread_id), Ok(Some(thread)) if thread.runtime.stop_intent.is_none())
            });
        if preserve {
            self.guard.mark_finalized();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn(
    state: AppState,
    thread_id: String,
    chain_root_id: String,
    params: ExecutionParams,
    entry: Entry,
    claim: ThreadLaunchClaim,
    guard: ExecutionGuard,
    effect: Option<ryeos_effect_contract::DispatchEffectIdentity>,
) -> Result<tokio::sync::oneshot::Receiver<Result<WaitResult>>> {
    let owners = Owners {
        _remote: placement::ExternalCandidateCleanupLifeline::new(&state, &thread_id),
        guard,
        claim,
    };
    let body_state = state.clone();
    let body_thread_id = thread_id.clone();
    spawn_owned(state, thread_id, owners, move |owners| {
        run(
            &body_state,
            &body_thread_id,
            &chain_root_id,
            &params,
            entry,
            owners,
            effect,
        )
    })
}

// One scheduling boundary shared by every entry mode. Keeping the owning
// envelope here also allows testing handoff without fabricating a born root.
fn spawn_owned(
    state: AppState,
    thread_id: String,
    owners: Owners,
    body: impl FnOnce(&mut Owners) -> Result<WaitResult> + Send + 'static,
) -> Result<tokio::sync::oneshot::Receiver<Result<WaitResult>>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    lillux::task::spawn_host_task("external-direct", move || {
        let mut owners = owners;
        let result = body(&mut owners);
        if let Err(error) = &result {
            tracing::warn!(thread_id, error = %format!("{error:#}"), "external direct owner stopped");
            finish_error(&state, &thread_id, &mut owners.guard, error);
        }
        owners.guard.cleanup();
        // Even a cancelled Wait cannot abandon the claim, remote cleanup
        // obligation, or immutable terminal publication halfway through.
        drop(owners);
        let _ = sender.send(result);
    })?
    .detach();
    Ok(receiver)
}

fn check_control(state: &AppState, thread_id: &str) -> Result<()> {
    ensure!(
        state.state_store.process_attachment_admission_is_open(),
        "external direct execution preserved for daemon shutdown"
    );
    let thread = state
        .threads
        .get_thread(thread_id)?
        .context("external direct thread disappeared")?;
    ensure!(
        thread.runtime.stop_intent.is_none(),
        "external direct execution has a durable stop request"
    );
    ensure!(
        !is_terminal_status(&thread.status),
        "external direct thread is already terminal"
    );
    Ok(())
}

fn deadline_from_retained_ms(issued_at_ms: i64, expires_at_ms: i64) -> Result<MonotonicDeadline> {
    Ok(MonotonicDeadline::after(retained_remaining(
        issued_at_ms,
        expires_at_ms,
        lillux::time::timestamp_millis(),
    )?))
}

fn retained_remaining(issued_at_ms: i64, expires_at_ms: i64, now: i64) -> Result<Duration> {
    ensure!(
        now >= issued_at_ms,
        "external direct clock precedes its retained authority"
    );
    ensure!(
        expires_at_ms > issued_at_ms,
        "external direct retained deadline is invalid"
    );
    // This projection is made once per phase, never renewed by a poll. An
    // expired deadline still permits inspecting already-retained output.
    Ok(Duration::from_millis(u64::try_from(
        expires_at_ms.saturating_sub(now).max(0),
    )?))
}

fn pause(deadline: MonotonicDeadline) {
    lillux::time::sleep(deadline.remaining().min(Duration::from_millis(50)));
}

fn permits_normal_settlement(
    output: &ryeos_app::runtime_db::external_execution::ExternalDirectOutput,
) -> bool {
    output.termination.reason
        == ryeos_state::external_execution::ExternalCommandTerminationReason::TargetExited
        && !output.termination.stdout.truncated
        && !output.termination.stderr.truncated
}

#[allow(clippy::too_many_arguments)]
fn run(
    state: &AppState,
    thread_id: &str,
    chain_root_id: &str,
    params: &ExecutionParams,
    entry: Entry,
    owners: &mut Owners,
    effect: Option<ryeos_effect_contract::DispatchEffectIdentity>,
) -> Result<WaitResult> {
    check_control(state, thread_id)?;
    let thread = state
        .threads
        .get_thread(thread_id)?
        .context("external direct thread disappeared")?;
    ensure!(
        thread.chain_root_id == chain_root_id,
        "external direct chain root changed"
    );
    let launch_owner = owners.claim.canonical_owner()?;
    let candidate_authority = params
        .provenance
        .candidate_evaluation_scope()
        .map(|scope| scope.authority());
    ensure!(
        !candidate_authority.is_some_and(|authority| matches!(
            &authority.purpose,
            ryeos_app::thread_lifecycle::CandidateOperationPurpose::Integrate { .. }
        )),
        "external readonly direct execution cannot publish a candidate integration generation"
    );

    // This historical read is crucial on restart: normal settlement must not
    // replay startup or require the now-expired readiness budget.
    let mut output = state
        .state_store
        .external_direct_normal_settlement_output(thread_id)?;
    if output.is_none() {
        let capsule = state
            .state_store
            .admitted_launch_capsule(thread_id)?
            .context("external direct execution has no retained capsule")?;
        let protocol = recovered_direct_protocol(
            params.provenance.request_engine(),
            &capsule,
            &params.resolved.resolved_item.kind,
        )?;
        let mut startup = match entry {
            Entry::Fresh => {
                ensure!(
                    state.state_store.external_allocation(thread_id)?.is_none(),
                    "fresh external direct execution already has an allocation"
                );
                let inputs =
                    crate::execution::external_direct_inputs::prepare_external_direct_inputs(
                        state,
                        &capsule,
                        &protocol,
                        thread_id,
                        chain_root_id,
                    )?;
                check_control(state, thread_id)?;
                placement::prepare_external_direct_start(
                    state,
                    thread_id,
                    chain_root_id,
                    &protocol,
                    inputs.source.as_ref(),
                    inputs.authority,
                )?
            }
            Entry::Recover => placement::recover_external_direct_start(state, thread_id)?,
        };
        let reservation = state
            .state_store
            .external_allocation(thread_id)?
            .context("external direct startup lost its reservation")?
            .reservation;
        let startup_deadline = deadline_from_retained_ms(
            reservation.startup_started_at_ms,
            reservation.startup_deadline_ms,
        )?;
        let channel = loop {
            check_control(state, thread_id)?;
            ensure!(
                !startup_deadline.has_elapsed(),
                "external direct signed startup deadline expired"
            );
            match startup.advance().map_err(anyhow::Error::from)? {
                ExternalCandidateStartProgress::Ready(channel) => break channel,
                ExternalCandidateStartProgress::CleanupRequired => {
                    bail!("external direct startup requires cleanup")
                }
                ExternalCandidateStartProgress::CleanupProved => {
                    bail!("external direct startup ended without a target")
                }
                _ => pause(startup_deadline),
            }
        };
        drop(startup);
        check_control(state, thread_id)?;
        state.threads.mark_running(thread_id)?;
        let output_deadline =
            deadline_from_retained_ms(channel.issued_at_ms, channel.expires_at_ms)?;
        loop {
            check_control(state, thread_id)?;
            output = state
                .state_store
                .collect_external_direct_output(thread_id)?;
            if let Some(retained) = output.as_ref() {
                if !permits_normal_settlement(retained) {
                    break;
                }
                // Complete stdout/termination may arrive before the signed
                // receipt for Release. Absence is pending, not a protocol
                // failure or permission to terminate the occurrence early.
                if let Some(applied) = state
                    .state_store
                    .external_direct_applied_output(thread_id)?
                {
                    ensure!(
                        &applied == retained,
                        "external direct applied output changed its retained identity"
                    );
                    output = Some(applied);
                    break;
                }
            }
            ensure!(
                !output_deadline.has_elapsed(),
                "external direct complete output was not retained before channel expiry"
            );
            pause(output_deadline);
        }
    }
    let output = output.context("external direct completion has no complete observation")?;
    let settlement_deadline = MonotonicDeadline::after(
        placement::external_candidate_settlement_timeout(state, thread_id)?,
    );
    let normal_settlement = permits_normal_settlement(&output);
    if !normal_settlement {
        placement::request_external_candidate_cleanup(state, thread_id)?;
    }
    loop {
        check_control(state, thread_id)?;
        ensure!(
            !settlement_deadline.has_elapsed(),
            "external direct settlement remains unproved"
        );
        let progress = if normal_settlement {
            placement::advance_external_direct_settlement(state, thread_id)?
        } else {
            placement::advance_external_candidate_cleanup(state, thread_id)?
        };
        if progress == ExternalCandidateCleanupProgress::Proved {
            break;
        }
        ensure!(
            !settlement_deadline.has_elapsed(),
            "external direct settlement remains unproved"
        );
        pause(settlement_deadline);
    }
    check_control(state, thread_id)?;
    let completion = ryeos_engine::dispatch::interpret_external_terminal(
        &output.termination,
        &output.stdout,
        &output.stderr,
    );
    // Preserve the rooted candidate-operation bookkeeping seam. Evaluate has
    // no integration publication; Integrate was refused before any contact.
    ensure!(
        record_candidate_integration_process_completion(
            state,
            thread_id,
            candidate_authority,
            &completion,
        )?
        .is_none(),
        "external readonly direct execution acquired integration publication authority"
    );
    let finalized = match finalize_completion(state, thread_id, completion, None, &launch_owner) {
        Ok(finalized) => finalized,
        Err(failure) => {
            if failure.cleanup_disarms_guard() {
                owners.guard.mark_finalized();
            }
            return Err(anyhow::Error::new(failure));
        }
    };
    owners.guard.mark_finalized();
    build_wait_result(state, finalized, None, None, effect)
}

fn finish_error(
    state: &AppState,
    thread_id: &str,
    guard: &mut ExecutionGuard,
    error: &anyhow::Error,
) {
    if guard.thread_finalized {
        return;
    }
    if !state.state_store.process_attachment_admission_is_open() {
        // This disarms owner-drop terminalization, not evidence of completion.
        // The remote lifeline preserves exact normal settlement, otherwise
        // records sticky cleanup for the daemon's retained recovery owner.
        guard.mark_finalized();
        return;
    }
    let stopped = state
        .threads
        .get_thread(thread_id)
        .ok()
        .flatten()
        .is_some_and(|thread| thread.runtime.stop_intent.is_some());
    if !stopped
        && matches!(
            state
                .state_store
                .external_direct_normal_settlement_output(thread_id),
            Ok(Some(_))
        )
    {
        // Never turn an ambiguous normal termination response into a new
        // cancellation or erase recoverable ordinary completion evidence.
        guard.mark_finalized();
        return;
    }
    if let Err(cleanup) = placement::request_external_candidate_cleanup(state, thread_id) {
        tracing::error!(thread_id, error = %cleanup, "external direct cleanup request remains unresolved");
    }
    // Administrative failure does not release occurrence capacity. Its
    // independent retained cleanup journal remains authoritative.
    guard.fail_thread_with_error(
        "external_direct_execution_failed",
        json!({ "reason": format!("{error:#}") }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are real AppState/SQLite launch-claim and host-handoff tests, not
    // RootAdmission or external-provider qualification. A fresh claim may be
    // reserved before birth; deliberately do not fabricate a thread capsule.
    fn reserved_owners(state: &AppState, thread_id: &str) -> Owners {
        let claim = ThreadLaunchClaim::acquire_fresh(state, thread_id).unwrap();
        let mut guard = ExecutionGuard::new(state.clone());
        guard.track_launch_owner(claim.canonical_owner().unwrap());
        Owners {
            _remote: placement::ExternalCandidateCleanupLifeline::new(state, thread_id),
            guard,
            claim,
        }
    }

    #[test]
    fn external_direct_notification_loss_does_not_release_live_host_claim() {
        let root = tempfile::tempdir().unwrap();
        let state = ryeos_app::state::test_support::build(root.path()).unwrap();
        let thread_id = "T-external-host-notification-loss";
        let owners = reserved_owners(&state, thread_id);
        let (entered, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release, release_rx) = std::sync::mpsc::sync_channel(1);
        let inside_state = state.clone();
        let notification = spawn_owned(state.clone(), thread_id.into(), owners, move |owners| {
            assert!(
                inside_state
                    .state_store
                    .get_launch_claim(thread_id)?
                    .is_some()
            );
            assert!(owners.guard.launch_owner.is_some());
            entered.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            // Receiver loss did not become cancellation, finalization, or a
            // replacement claim. The actual execution owner is still here.
            assert!(
                inside_state
                    .state_store
                    .get_launch_claim(thread_id)?
                    .is_some()
            );
            bail!("intentional host fixture completion")
        })
        .unwrap();
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(notification);
        assert!(
            state
                .state_store
                .get_launch_claim(thread_id)
                .unwrap()
                .is_some()
        );
        assert!(ThreadLaunchClaim::acquire_fresh(&state, thread_id).is_err());
        assert!(
            state
                .state_store
                .external_allocation(thread_id)
                .unwrap()
                .is_none()
        );
        release.send(()).unwrap();
        let deadline = MonotonicDeadline::after(Duration::from_secs(5));
        while state
            .state_store
            .get_launch_claim(thread_id)
            .unwrap()
            .is_some()
        {
            assert!(
                !deadline.has_elapsed(),
                "host did not release its completed claim"
            );
            lillux::time::sleep(Duration::from_millis(1));
        }
        assert!(state.threads.get_thread(thread_id).unwrap().is_none());
        // Exact release also retired the process-local claim registration.
        drop(ThreadLaunchClaim::acquire_fresh(&state, thread_id).unwrap());
    }

    #[test]
    fn external_direct_notification_follows_guard_and_claim_release() {
        let root = tempfile::tempdir().unwrap();
        let state = ryeos_app::state::test_support::build(root.path()).unwrap();
        let thread_id = "T-external-host-completed";
        let owners = reserved_owners(&state, thread_id);
        let notification = spawn_owned(state.clone(), thread_id.into(), owners, |_| {
            bail!("intentional owner fixture refusal")
        })
        .unwrap();
        let result = notification.blocking_recv().unwrap();
        assert!(
            matches!(result, Err(error) if error.to_string() == "intentional owner fixture refusal")
        );
        assert!(
            state
                .state_store
                .get_launch_claim(thread_id)
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .state_store
                .external_allocation(thread_id)
                .unwrap()
                .is_none()
        );
        drop(ThreadLaunchClaim::acquire_fresh(&state, thread_id).unwrap());
    }

    #[test]
    fn external_direct_shutdown_refuses_before_thread_lookup_or_allocation() {
        let root = tempfile::tempdir().unwrap();
        let state = ryeos_app::state::test_support::build(root.path()).unwrap();
        let thread_id = "T-external-host-shutdown";
        let owners = reserved_owners(&state, thread_id);
        state
            .state_store
            .close_process_attachment_admission()
            .unwrap();
        let error = check_control(&state, thread_id).unwrap_err();
        assert!(error.to_string().contains("preserved for daemon shutdown"));
        assert!(!error.to_string().contains("disappeared"));
        drop(owners);
        assert!(state.threads.get_thread(thread_id).unwrap().is_none());
        assert!(
            state
                .state_store
                .external_allocation(thread_id)
                .unwrap()
                .is_none()
        );
        assert!(
            state
                .state_store
                .get_launch_claim(thread_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn external_direct_retained_deadline_never_renews_on_later_projection() {
        assert_eq!(
            retained_remaining(1_000, 2_000, 1_000).unwrap(),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            retained_remaining(1_000, 2_000, 1_900).unwrap(),
            Duration::from_millis(100)
        );
        assert_eq!(
            retained_remaining(1_000, 2_000, 2_000).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            retained_remaining(1_000, 2_000, 2_001).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            retained_remaining(1_000, 2_000, i64::MAX).unwrap(),
            Duration::ZERO
        );
    }

    #[test]
    fn external_direct_retained_deadline_refuses_rollback_and_invalid_anchor() {
        assert!(
            retained_remaining(1_000, 2_000, 999)
                .unwrap_err()
                .to_string()
                .contains("clock precedes")
        );
        for expiry in [999, 1_000] {
            assert!(
                retained_remaining(1_000, expiry, 1_000)
                    .unwrap_err()
                    .to_string()
                    .contains("deadline is invalid")
            );
        }
    }
}
