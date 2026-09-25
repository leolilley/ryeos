//! One-shot contained producer start owned by the admitted verifier root.
//!
//! This service accepts no executable or filesystem path from a callback.
//! A failed transaction never retries the attempt; its durable row remains
//! available for exact-scope recovery if immediate cleanup cannot prove death.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use ryeos_engine::isolation::{
    IsolationCommandAuthority, IsolationFilesystemAuthorityCeiling, IsolationLaunchContext,
    IsolationNetworkAuthorityCeiling, IsolationProjectAuthority, IsolationTargetChannelAuthority,
};
use ryeos_isolation_protocol::IsolationLoopbackIngress;
use ryeos_runtime::scoped_relay_handoff::{
    HANDOFF_SCHEMA, ScopedRelayHandoff, receive_ready, send_handoff,
};
use ryeos_state::external_content::products::producer_recipe::{
    ProducerExecutableSource, ProducerStdinSource,
};
use ryeos_state::external_content::products::qualification::ProductQualificationLaunchPurpose;

use crate::operator_external_content::product_qualification::{
    CurrentBundleProducerRecipe, resolve_current_bundle_producer_recipe_for_purpose,
};
use crate::scoped_producer_authority::{
    ScopedProducerAttemptCoordinate, ScopedProducerAuthorityKey,
};
use crate::scoped_producer_io::ScopedProducerInteractiveIo;
use crate::scoped_producer_process::{ScopedProducerProcessKey, ScopedProducerRunningChild};
use crate::state::AppState;

fn admit_recipe_subject(
    executable: &ProducerExecutableSource,
    subject_declaration_id: &str,
    subject_manifest_hash: &str,
) -> Result<()> {
    if let ProducerExecutableSource::AdmittedRealizationMember {
        realization_id,
        manifest_hash,
        ..
    } = executable
    {
        ensure!(
            realization_id == subject_declaration_id && manifest_hash == subject_manifest_hash,
            "producer executable is not the sealed qualification subject"
        );
    }
    Ok(())
}

#[cfg(test)]
mod recipe_subject_tests {
    use super::*;

    #[test]
    fn realization_executable_must_be_the_sealed_subject() {
        let executable = ProducerExecutableSource::AdmittedRealizationMember {
            realization_id: "codex_runtime".to_owned(),
            manifest_hash: "a".repeat(64),
            relative_path: "bin/codex".to_owned(),
            executable_sha256: "b".repeat(64),
        };
        assert!(admit_recipe_subject(&executable, "codex_runtime", &"a".repeat(64)).is_ok());
        assert!(admit_recipe_subject(&executable, "other_runtime", &"a".repeat(64)).is_err());
        assert!(admit_recipe_subject(&executable, "codex_runtime", &"c".repeat(64)).is_err());
        assert!(
            admit_recipe_subject(
                &ProducerExecutableSource::AdmittedVerifierExecutable,
                "codex_runtime",
                &"a".repeat(64)
            )
            .is_ok()
        );
    }

}

/// The successful return is only an attempt locator. The process and all
/// descriptor lifelines have already been transferred to the live registry.
pub fn start_scoped_producer(
    state: &AppState,
    key: &ScopedProducerAuthorityKey,
    purpose: &ProductQualificationLaunchPurpose,
    scenario_id: &str,
    selected: &CurrentBundleProducerRecipe,
    admitted_stdin: &str,
    coordinate: &ScopedProducerAttemptCoordinate,
    expected_isolation_class: &ryeos_engine::isolation::IsolationLaunchProvenance,
    release_deadline: lillux::time::MonotonicDeadline,
) -> Result<String> {
    ensure!(
        !release_deadline.has_elapsed(),
        "scoped producer deadline elapsed"
    );
    ensure!(
        state.isolation.is_enforced(),
        "scoped producer requires enforced isolation"
    );
    ensure!(
        state.isolation.admission_class_provenance()? == *expected_isolation_class,
        "scoped producer isolation class moved before reservation"
    );
    ScopedProducerAuthorityKey::new(key.root_thread_id.clone(), key.launch_owner.clone())?;
    purpose.validate()?;
    let current = resolve_current_bundle_producer_recipe_for_purpose(state, purpose, scenario_id)?;
    ensure!(
        current == *selected,
        "selected producer recipe is no longer current"
    );
    admit_recipe_subject(
        &selected.recipe.executable_source,
        &purpose.subject_declaration_id,
        &purpose.subject_manifest_hash,
    )?;
    let source = selected.source_identity()?;
    let derived =
        ScopedProducerAttemptCoordinate::derive(key, scenario_id, &source, admitted_stdin)?;
    ensure!(
        derived == *coordinate,
        "scoped producer coordinate differs from signed selection"
    );
    state.state_store.assert_launch_owner(
        &key.root_thread_id,
        &serde_json::to_string(&key.launch_owner)?,
    )?;

    // The one-shot authority cut precedes durable reservation. Losing it on
    // any later error is intentional: no callback may re-open this attempt.
    let live = state.scoped_producer_authorities.consume_for_attempt(key)?;
    let admitted_command =
        live.admitted_command_for_recipe(scenario_id, &source, &selected.recipe.executable_source)?;
    let prepared_launch =
        live.request_for_recipe(&selected.recipe, admitted_stdin, &admitted_command)?;
    let request = prepared_launch.request;
    let prepared_mounts = prepared_launch.prepared_mounts;
    let workspace_view = live.workspace_view()?;
    let allocation = state
        .isolation
        .plan_process_scope(coordinate.scope_allocation_name())?;
    let initial = coordinate.journal_attempt(key, allocation)?;
    let attempt_id = initial.attempt_id.clone();
    let process_key = ScopedProducerProcessKey::new(attempt_id.clone(), key.launch_owner.clone())?;
    state
        .scoped_producer_processes
        .register_starting(process_key.clone())?;
    if let Err(error) = state.state_store.reserve_scoped_child_attempt(&initial) {
        let _ = state
            .scoped_producer_processes
            .finish_start_failure(&process_key, true);
        return Err(error);
    }

    let mut unbound_scope_retirement_uncertain = false;
    let mut wrapper_reap_uncertain = false;
    let launched = (|| -> Result<()> {
        #[cfg(feature = "test-support")]
        if let Some(gate) = state.extensions.get::<test_support::ReservedAttemptGate>() {
            gate.reach(key, coordinate, release_deadline)?;
        }
        let scope = state
            .isolation
            .allocate_process_scope(&initial.scope_allocation)?;
        let recovery = scope.recovery().clone();
        if let Err(error) = state
            .state_store
            .bind_scoped_child_scope(&attempt_id, &recovery)
        {
            // The row still names only an allocation. Retire the exact empty
            // scope now; do not let a dropped unbound handle hide it.
            let cleanup = scope.retire_unlaunched(recovery.control_timeout());
            if cleanup.is_err() {
                // The journal has no recovery binding yet. Do not mark its
                // allocation discarded when the exact allocated scope could
                // not be proved empty and retired. Startup must refuse or
                // reconcile that unresolved allocation.
                unbound_scope_retirement_uncertain = true;
            }
            return Err(error.context(format!("unbound scope retirement: {cleanup:?}")));
        }
        let limited = scope
            .into_resource_limited(lillux::ProcessScopeResourceLimits {
                maximum_memory_bytes: selected.recipe.bounds.maximum_memory_bytes,
                maximum_processes: selected.recipe.bounds.maximum_processes,
            })
            .map_err(|(_, error)| anyhow::anyhow!(error))?;
        let (interactive_parent, target_channels) =
            if matches!(selected.recipe.stdin_source, ProducerStdinSource::InteractiveVerifierChannel { .. }) {
                let (parent, child) =
                    lillux::inherited_duplex_channel_pair().map_err(anyhow::Error::msg)?;
                let channel = IsolationTargetChannelAuthority::new(
                    child,
                    0,
                    "RYEOS_PRODUCER_STDIN_FD",
                )?;
                (Some(parent), vec![channel])
            } else {
                (None, Vec::new())
            };
        let context = IsolationLaunchContext {
            project_path: live.workspace().path(),
            project_authority: IsolationProjectAuthority::EphemeralScratch,
            immutable_project: None,
            workspace_view: Some(&workspace_view),
            filesystem_authority_ceiling: IsolationFilesystemAuthorityCeiling::CapturedExecution,
            network_authority_ceiling: IsolationNetworkAuthorityCeiling::Isolated,
            live_access: None,
            state_root: None,
            checkpoint_dir: None,
            checkpoint_authority: None,
            daemon_socket_path: None,
            bundle_roots: &[],
            node_trusted_keys_dir: None,
            verified_code: std::slice::from_ref(admitted_command.authority().identity()),
            verified_command: Some(&admitted_command),
            external_read_only_mounts: live.read_only_mounts(),
            writable_runtime_view_mounts: &[],
            producer_prepared_mounts: &prepared_mounts,
            target_channels: &target_channels,
            item_ref: "scoped-producer",
            thread_id: &key.root_thread_id,
        };
        let (mut held, provenance, expected_applied_launch, expected_mount_preparation, mut validated_listener) =
            if let Some(ingress) = &selected.recipe.loopback_ingress {
                let compiled = state
                    .isolation
                    .apply_awaiting_attachment_in_scope_with_loopback_ingress(
                        request,
                        context,
                        limited.into_process_scope(),
                        &IsolationLoopbackIngress {
                            address: ingress.address.clone(),
                        },
                    )?;
                let provenance = compiled.provenance.clone();
                ensure!(
                    provenance.plan_digest.is_some()
                        && provenance.has_same_admission_class(expected_isolation_class),
                    "scoped producer compiled isolation differs from root admission class"
                );
                let expected = compiled.expected_applied_launch.clone();
                let expected_mounts = compiled.expected_mount_preparation.clone();
                let spawned = compiled
                    .require_applied_launch_receipt()?
                    .spawn()
                    .map_err(|failure| anyhow::anyhow!("held producer spawn failed: {failure:?}"))?;
                let (held, listener) = match spawned.receive_listener(release_deadline) {
                    Ok(received) => received,
                    Err((error, held)) => {
                        let abort = held.abort_and_reap();
                        if abort.is_err() {
                            wrapper_reap_uncertain = true;
                        }
                        return Err(anyhow::anyhow!(error).context(format!(
                            "held listener transfer checked abort: {abort:?}"
                        )));
                    }
                };
                (Some(held), provenance, expected, expected_mounts, Some(listener))
            } else {
                let applied = state
                    .isolation
                    .apply_awaiting_attachment_in_scope_with_provenance(
                        request,
                        context,
                        Some(limited.into_process_scope()),
                    )?;
                let expected = applied
                    .expected_applied_launch
                    .clone()
                    .context("enforced scoped producer has no compiled target commitments")?;
                let expected_mounts = applied
                    .expected_mount_preparation
                    .clone()
                    .context("enforced scoped producer has no compiled mount commitments")?;
                let provenance = applied.provenance;
                ensure!(
                    provenance.plan_digest.is_some()
                        && provenance.has_same_admission_class(expected_isolation_class),
                    "scoped producer compiled isolation differs from root admission class"
                );
                let held = applied
                    .request
                    .require_applied_launch_receipt()?
                    .spawn()
                    .map_err(|failure| anyhow::anyhow!("held producer spawn failed: {failure:?}"))?;
                (Some(held), provenance, expected, expected_mounts, None)
            };
        let mut ingress_handoff = None;
        let mut relay_handoff = None;
        let prepared = (|| -> Result<_> {
            let exact = held
                .as_ref()
                .expect("held producer is owned until release")
                .exact_process_identity()
                .map_err(anyhow::Error::msg)?;
            let identity = crate::process::execution_process_identity_from_lillux(
                exact,
                Some(recovery.clone()),
            )?;
            let mount_preparation = held
                .as_ref()
                .expect("held producer is owned until release")
                .mount_preparation_receipt()
                .context("held producer has no final-root mount preparation")?;
            ensure!(
                mount_preparation.schema == 1
                    && mount_preparation.owned_child_pid > 0
                    && i64::from(mount_preparation.owned_child_pid) == identity.target_pid
                    && mount_preparation.matches_commitments(&expected_mount_preparation),
                "final-root mount preparation differs from exact held identity or compiled plan"
            );
            state
                .state_store
                .attach_scoped_child_process(
                    &attempt_id,
                    &identity,
                    &crate::runtime_db::scoped_child_attempt::ScopedChildMountPreparationEvidence {
                        schema: 1,
                        plan_digest: provenance.plan_digest.clone()
                            .context("compiled scoped producer has no exact plan digest")?,
                        expected: expected_mount_preparation.clone(),
                        observed: mount_preparation.clone(),
                    },
                )?;

            if let Some(validated) = validated_listener.take() {
                let mut channel = live.take_ingress_handoff()?;
                let (listener, receipt) = validated.into_parts();
                let ingress = selected.recipe.loopback_ingress.as_ref().context(
                    "validated listener has no signed ingress",
                )?;
                let handoff = ScopedRelayHandoff {
                    schema: HANDOFF_SCHEMA.to_owned(),
                    root_thread_id: key.root_thread_id.clone(),
                    attempt_id: attempt_id.clone(),
                    scenario_id: scenario_id.to_owned(),
                    recipe_source: source.clone(),
                    adapter_request_digest: receipt.request_digest,
                    ingress_address: ingress.address.clone(),
                    expected_applied_launch_digest: lillux::sha256_hex(
                        lillux::canonical_json(&serde_json::to_value(&expected_applied_launch)?)?
                            .as_bytes(),
                    ),
                    held_process_identity_digest: lillux::sha256_hex(
                        lillux::canonical_json(&serde_json::to_value(&identity)?)?.as_bytes(),
                    ),
                };
                handoff.validate()?;
                listener.transfer_over_inherited_duplex(&mut channel, release_deadline)?;
                send_handoff(&mut channel, &handoff, release_deadline)?;
                receive_ready(&mut channel, &handoff, release_deadline)?;
                relay_handoff = Some(handoff);
                ingress_handoff = Some(channel);
            }

            // The signed source, root owner, and deadline are checked at the
            // last reversible cut, after held attachment and before release.
            ensure!(
                !release_deadline.has_elapsed(),
                "scoped producer deadline elapsed before release"
            );
            ensure!(
                resolve_current_bundle_producer_recipe_for_purpose(state, purpose, scenario_id)?
                    == *selected,
                "scoped producer signed recipe changed before release"
            );
            ensure!(
                state.isolation.admission_class_provenance()? == *expected_isolation_class,
                "scoped producer isolation class moved before held release"
            );
            state.state_store.assert_launch_owner(
                &key.root_thread_id,
                &serde_json::to_string(&key.launch_owner)?,
            )?;
            let start = lillux::time::occupancy_now().map_err(anyhow::Error::msg)?;
            let natural_wait_deadline = lillux::time::MonotonicDeadline::after(
                Duration::from_millis(selected.recipe.bounds.maximum_wall_time_ms),
            );
            let maximum_ns = selected
                .recipe
                .bounds
                .maximum_wall_time_ms
                .checked_mul(1_000_000)
                .context("scoped producer wall-time bound overflows")?;
            let limit =
                lillux::time::OccupancyLimit::new(start, maximum_ns).map_err(anyhow::Error::msg)?;
            state
                .state_store
                .permit_scoped_child_release(&attempt_id, &identity)?;
            Ok((limit, natural_wait_deadline))
        })();
        let (limit, natural_wait_deadline) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let abort = held
                    .take()
                    .expect("held producer remains owned before release")
                    .abort_and_reap();
                if abort.is_err() {
                    wrapper_reap_uncertain = true;
                }
                return Err(error.context(format!("held producer checked abort: {abort:?}")));
            }
        };
        let mut running = held
            .take()
            .expect("held producer remains owned at release")
            .release_after_attachment_with_occupancy(limit, Duration::from_millis(0))
            .map_err(|error| {
                if !error.cleanup_is_settled() {
                    wrapper_reap_uncertain = true;
                }
                anyhow::anyhow!("scoped producer release failed: {error}")
            })?;
        let interactive_io = if let Some(parent) = interactive_parent {
            let prepared = running
                .take_stdout_reader()
                .context("direct scoped target has no bounded stdout reader")
                .and_then(|reader| ScopedProducerInteractiveIo::new(parent, reader));
            match prepared {
                Ok(io) => Some(std::sync::Arc::new(io)),
                Err(error) => {
                    let abort = running.abort_and_reap_checked();
                    if abort.is_err() {
                        wrapper_reap_uncertain = true;
                    }
                    return Err(error.context(format!(
                        "direct scoped I/O owner failed after release; checked abort: {abort:?}"
                    )));
                }
            }
        } else {
            None
        };
        let record = state
            .state_store
            .scoped_child_attempt(&attempt_id)?
            .context("released scoped producer attempt disappeared")?;
        if let Err(error) = state.scoped_producer_processes.insert(
            &record,
            ScopedProducerRunningChild {
                process: running,
                authority: live,
                interactive_io,
                ingress_handoff,
                relay_handoff,
                producer_recipe: selected.recipe.clone(),
                producer_source: source.clone(),
                natural_wait_deadline,
                maximum_stdout_bytes: selected.recipe.bounds.maximum_stdout_bytes,
                maximum_stderr_bytes: selected.recipe.bounds.maximum_stderr_bytes,
                isolation_provenance: provenance,
                expected_applied_launch,
            },
        ) {
            let abort = error.child.process.abort_and_reap_checked();
            if abort.is_err() {
                wrapper_reap_uncertain = true;
            }
            return Err(error
                .error
                .context(format!("released child abort: {abort:?}")));
        }
        Ok(())
    })();

    if let Err(error) = launched {
        // Cleanup never changes this attempt back to launchable. If kernel or
        // journal proof fails, leave the row unsettled for startup recovery.
        if let Ok(Some(record)) = state.state_store.scoped_child_attempt(&attempt_id) {
            if let Some(recovery) = record.scope_recovery {
                if !wrapper_reap_uncertain {
                    let _ = state
                        .state_store
                        .claim_bound_scoped_child_retirement(&attempt_id, &recovery);
                    let _ = state
                        .state_store
                        .prove_bound_scoped_child_death(&attempt_id, &recovery);
                    let _ = state
                        .state_store
                        .complete_bound_scoped_child_retirement(&attempt_id);
                }
            } else if !unbound_scope_retirement_uncertain {
                let _ = state
                    .state_store
                    .claim_unbound_scoped_child_discard(&attempt_id);
                let _ = state
                    .state_store
                    .complete_unbound_scoped_child_discard(&attempt_id);
            }
        }
        let settled = state
            .state_store
            .scoped_child_attempt(&attempt_id)
            .ok()
            .flatten()
            .is_some_and(|record| {
                record.phase == crate::runtime_db::scoped_child_attempt::ScopedChildPhase::Retired
            });
        let _ = state
            .scoped_producer_processes
            .finish_start_failure(&process_key, settled);
        return Err(error.context("scoped producer start failed after durable reservation"));
    }
    Ok(attempt_id)
}

/// Test-only, descriptor-backed pause at the committed reservation cut. It
/// cannot select a child or change production admission; a missing or late
/// release follows the ordinary failed-start retirement path.
#[cfg(feature = "test-support")]
pub mod test_support {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Condvar, Mutex};

    use anyhow::{Context, Result, ensure};
    use serde::Serialize;

    use crate::runtime_db::LaunchOwner;
    use crate::scoped_producer_authority::{
        ScopedProducerAttemptCoordinate, ScopedProducerAuthorityKey,
    };

    pub const RESERVED_ATTEMPT_GATE_FD_ENV: &str = "RYEOS_RESERVED_ATTEMPT_GATE_FD";

    #[derive(Serialize)]
    #[serde(deny_unknown_fields)]
    struct ReservedAttemptEvidence<'a> {
        schema: &'static str,
        root_thread_id: &'a str,
        launch_owner: &'a LaunchOwner,
        attempt_id: &'a str,
        scenario_digest: &'a str,
    }

    pub struct ReservedAttemptGate {
        channel: Mutex<lillux::InheritedDuplexChannel>,
        consumed: AtomicBool,
        finished: AtomicBool,
        pending: Mutex<Option<PendingResume>>,
        changed: Condvar,
    }

    struct PendingResume {
        root_thread_id: String,
        launch_owner: LaunchOwner,
        attempt_id: String,
        observed: bool,
    }

    impl ReservedAttemptGate {
        pub fn new(channel: lillux::InheritedDuplexChannel) -> Self {
            Self {
                channel: Mutex::new(channel),
                consumed: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                pending: Mutex::new(None),
                changed: Condvar::new(),
            }
        }

        /// Called only after RESUME has read the owner-unique Reserved row and
        /// found that START has not yet registered a live process.
        pub fn note_resume_pending(
            &self,
            key: &ScopedProducerAuthorityKey,
            attempt_id: &str,
        ) -> Result<()> {
            if self.finished.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("reserved-attempt pending lock poisoned"))?;
            if let Some(waiting) = pending.as_mut() {
                ensure!(
                    waiting.root_thread_id == key.root_thread_id
                        && waiting.launch_owner == key.launch_owner
                        && waiting.attempt_id == attempt_id,
                    "reserved-attempt RESUME differs from gate owner"
                );
                waiting.observed = true;
            } else {
                // RESUME can read the committed row just before START enters
                // this gate. Latch that exact observation rather than lose it.
                *pending = Some(PendingResume {
                    root_thread_id: key.root_thread_id.clone(),
                    launch_owner: key.launch_owner.clone(),
                    attempt_id: attempt_id.to_owned(),
                    observed: true,
                });
            }
            self.changed.notify_all();
            Ok(())
        }

        pub fn reach(
            &self,
            key: &ScopedProducerAuthorityKey,
            coordinate: &ScopedProducerAttemptCoordinate,
            deadline: lillux::time::MonotonicDeadline,
        ) -> Result<()> {
            if self.consumed.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            {
                let mut pending = self
                    .pending
                    .lock()
                    .map_err(|_| anyhow::anyhow!("reserved-attempt pending lock poisoned"))?;
                if let Some(waiting) = pending.as_ref() {
                    ensure!(
                        waiting.root_thread_id == key.root_thread_id
                            && waiting.launch_owner == key.launch_owner
                            && waiting.attempt_id == coordinate.attempt_id(),
                        "reserved-attempt gate differs from early RESUME owner"
                    );
                } else {
                    *pending = Some(PendingResume {
                        root_thread_id: key.root_thread_id.clone(),
                        launch_owner: key.launch_owner.clone(),
                        attempt_id: coordinate.attempt_id().to_owned(),
                        observed: false,
                    });
                }
                while !pending.as_ref().is_some_and(|waiting| waiting.observed) {
                    let remaining = deadline.remaining();
                    ensure!(
                        !remaining.is_zero(),
                        "reserved-attempt RESUME was not observed before deadline"
                    );
                    let (next, _) = self
                        .changed
                        .wait_timeout(pending, remaining)
                        .map_err(|_| anyhow::anyhow!("reserved-attempt pending wait poisoned"))?;
                    pending = next;
                }
            }
            let evidence = ReservedAttemptEvidence {
                schema: "ryeos.scoped_reserved_attempt_gate.v1",
                root_thread_id: &key.root_thread_id,
                launch_owner: &key.launch_owner,
                attempt_id: coordinate.attempt_id(),
                scenario_digest: coordinate.scenario_digest(),
            };
            let mut bytes = lillux::canonical_json(&serde_json::to_value(evidence)?)?.into_bytes();
            ensure!(
                bytes.len() <= 512,
                "reserved-attempt gate evidence exceeds bound"
            );
            bytes.push(b'\n');
            let mut channel = self
                .channel
                .lock()
                .map_err(|_| anyhow::anyhow!("reserved-attempt gate lock poisoned"))?;
            let mut bounded = channel.with_deadline(deadline);
            bounded
                .write_all(&bytes)
                .context("write reserved-attempt gate evidence")?;
            let mut release = [0u8; 1];
            bounded
                .read_exact(&mut release)
                .context("read reserved-attempt gate release")?;
            ensure!(
                release == [b'R'],
                "reserved-attempt gate release token is invalid"
            );
            self.finished.store(true, Ordering::Release);
            Ok(())
        }
    }
}
