//! Exact, one-shot observation of a contained producer child.
//!
//! A natural result is committed as a bounded CAS object before the scope is
//! retired. The object is observation evidence, never a verifier terminal or
//! runtime-qualification claim. An uncertain wait or failed publication only
//! moves toward cleanup; it cannot turn into natural-exit testimony.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::process::ExecutionProcessIdentity;
use crate::runtime_db::LaunchOwner;
use crate::runtime_db::scoped_child_attempt::{
    ScopedChildAttemptRecord, ScopedChildNaturalEmptyReceipt, ScopedChildPhase,
};
use crate::scoped_producer_process::ScopedProducerProcessKey;
use crate::state::AppState;
use ryeos_handler_protocol::ExecutionEvidenceCandidateScopedAttemptWire;
use ryeos_runtime::scoped_relay_handoff::ScopedRelayHandoff;
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;
use ryeos_state::external_content::products::qualification::{
    ProductQualificationLaunchPurpose, ProductQualificationScopedAttemptProof,
};

const OBSERVATION_SCHEMA: &str = "ryeos.scoped_producer_observation.v6";
const MAX_OBSERVATION_RESPONSE_BYTES: usize = 9 * 1024 * 1024;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// The full retained evidence envelope. `producer_exit_clean` says only that
/// this producer process and its descendants completed fault-free; it does not
/// attest the verifier's semantic claims or qualify a runtime product.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopedProducerObservation {
    schema: String,
    attempt_id: String,
    launch_owner: LaunchOwner,
    recipe_digest: String,
    recipe_generation: String,
    producer_source: ProductProducerRecipeSourceIdentity,
    /// Required nullable: old observations without a handoff field cannot be
    /// mistaken for a direct-target result under the new contract.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    relay_handoff: Option<ScopedRelayHandoff>,
    scenario_digest: String,
    process_identity: ExecutionProcessIdentity,
    scope_recovery: lillux::ProcessScopeRecovery,
    isolation_provenance: ryeos_engine::isolation::IsolationLaunchProvenance,
    applied_launch: lillux::LinuxSandboxAppliedLaunchReceipt,
    prepared_immutable_sha256: BTreeMap<String, String>,
    natural_empty_receipt_digest: String,
    subprocess_success: bool,
    producer_exit_clean: bool,
    exit_code: i32,
    timed_out: bool,
    stdout: String,
    stdout_sha256: String,
    stdout_bytes: u64,
    maximum_stdout_bytes: u64,
    stdout_truncated: bool,
    stderr: String,
    stderr_sha256: String,
    stderr_bytes: u64,
    maximum_stderr_bytes: u64,
    stderr_truncated: bool,
    output_limit_exceeded: Option<String>,
    launcher_refusal: Option<String>,
}

impl ScopedProducerObservation {
    fn validate_against(&self, record: &ScopedChildAttemptRecord) -> Result<()> {
        ensure!(
            self.schema == OBSERVATION_SCHEMA,
            "unknown scoped observation schema"
        );
        ensure!(
            self.attempt_id == record.initial.attempt_id,
            "observation attempt differs"
        );
        ensure!(
            self.launch_owner == record.initial.owner,
            "observation owner differs"
        );
        ensure!(
            self.recipe_digest == record.initial.recipe_digest,
            "observation recipe differs"
        );
        ensure!(
            self.recipe_generation == record.initial.recipe_generation,
            "observation generation differs"
        );
        self.producer_source.validate()?;
        if let Some(handoff) = &self.relay_handoff {
            handoff.validate()?;
            ensure!(
                handoff.root_thread_id == record.initial.owner.thread_id
                    && handoff.attempt_id == self.attempt_id
                    && handoff.recipe_source == self.producer_source
                    && handoff.held_process_identity_digest
                        == lillux::sha256_hex(
                            lillux::canonical_json(&serde_json::to_value(&self.process_identity)?)?
                                .as_bytes()
                        ),
                "scoped relay handoff differs from retained process and producer source"
            );
        }
        ensure!(
            self.producer_source.recipe_digest == self.recipe_digest
                && self.producer_source.bundle_generation_identity == self.recipe_generation,
            "observation full producer source differs from retained attempt"
        );
        ensure!(
            self.scenario_digest == record.initial.scenario_digest,
            "observation scenario differs"
        );
        ensure!(
            record.process_identity.as_ref() == Some(&self.process_identity),
            "observation process differs"
        );
        ensure!(
            record.scope_recovery.as_ref() == Some(&self.scope_recovery),
            "observation scope differs"
        );
        ensure!(
            record.natural_empty_receipt_digest.as_deref()
                == Some(self.natural_empty_receipt_digest.as_str()),
            "observation natural receipt differs"
        );
        ensure!(
            self.isolation_provenance.plan_digest.is_some(),
            "observation has no concrete isolation plan"
        );
        let mount_evidence = record
            .mount_preparation_evidence
            .as_ref()
            .context("observation has no retained pre-release mount evidence")?;
        mount_evidence.validate_observation_plan(
            self.isolation_provenance.plan_digest.as_deref().unwrap(),
            &self.process_identity,
        )?;
        validate_prepared_immutable_join(
            &self.prepared_immutable_sha256,
            &mount_evidence.prepared_immutable_sha256,
        )?;
        ensure!(
            self.applied_launch
                .matches_post_release_mounts(&mount_evidence.expected),
            "observation post-release target mounts differ from retained held preparation"
        );
        ensure!(
            i64::from(self.applied_launch.owned_child_pid) == self.process_identity.target_pid
                && self.applied_launch.namespace_pid == 1
                && self.applied_launch.effective_uid == 1
                && self.applied_launch.effective_gid == 1
                && self.applied_launch.no_new_privs
                && self.applied_launch.seccomp_mode == 2,
            "observation applied launch differs from exact contained target"
        );
        ensure!(
            self.maximum_stdout_bytes > 0 && self.maximum_stderr_bytes > 0,
            "observation output bounds absent"
        );
        ensure!(
            self.stdout_bytes == self.stdout.len() as u64
                && self.stdout_bytes <= self.maximum_stdout_bytes,
            "observation stdout exceeds signed bound"
        );
        ensure!(
            self.stderr_bytes == self.stderr.len() as u64
                && self.stderr_bytes <= self.maximum_stderr_bytes,
            "observation stderr exceeds signed bound"
        );
        ensure!(
            self.stdout_sha256 == lillux::sha256_hex(self.stdout.as_bytes()),
            "observation stdout digest differs"
        );
        ensure!(
            self.stderr_sha256 == lillux::sha256_hex(self.stderr.as_bytes()),
            "observation stderr digest differs"
        );
        ScopedChildNaturalEmptyReceipt::verify_recorded_digest(
            record,
            &self.natural_empty_receipt_digest,
            self.subprocess_success,
            self.exit_code,
            self.timed_out,
            &self.stdout_sha256,
            &self.stderr_sha256,
        )?;
        let clean = self.subprocess_success
            && self.exit_code == 0
            && !self.timed_out
            && !self.stdout_truncated
            && !self.stderr_truncated
            && self.output_limit_exceeded.is_none()
            && self.launcher_refusal.is_none();
        ensure!(
            self.producer_exit_clean == clean,
            "observation misclassifies producer exit"
        );
        Ok(())
    }
}

fn validate_prepared_immutable_join(
    observed: &BTreeMap<String, String>,
    held: &BTreeMap<String, String>,
) -> Result<()> {
    ensure!(
        observed == held,
        "observation sealed prepared content differs from retained held mount evidence"
    );
    Ok(())
}

#[cfg(test)]
mod prepared_immutable_tests {
    use super::*;

    #[test]
    fn terminal_content_map_must_match_the_held_attempt_exactly() {
        let expected = BTreeMap::from([
            (
                "/ryeos/producer-prepared/codex-home/config.toml".into(),
                "a".repeat(64),
            ),
            (
                "/ryeos/producer-prepared/codex-home/environments.toml".into(),
                "b".repeat(64),
            ),
        ]);
        validate_prepared_immutable_join(&expected, &expected).unwrap();
        let mut missing = expected.clone();
        missing.remove("/ryeos/producer-prepared/codex-home/config.toml");
        assert!(validate_prepared_immutable_join(&missing, &expected).is_err());
        let mut changed = expected.clone();
        changed.insert(
            "/ryeos/producer-prepared/codex-home/config.toml".into(),
            "c".repeat(64),
        );
        assert!(validate_prepared_immutable_join(&changed, &expected).is_err());
        let mut extra = expected.clone();
        extra.insert(
            "/ryeos/producer-prepared/codex-home/extra.toml".into(),
            "d".repeat(64),
        );
        assert!(validate_prepared_immutable_join(&extra, &expected).is_err());
    }
}

/// Observe exactly the process belonging to this retained row. A lost
/// response can replay only the committed object, after exact scope death and
/// retirement have been proven. It never contacts or relaunches the producer.
pub fn observe_scoped_producer(
    state: &AppState,
    key: &ScopedProducerProcessKey,
    callback_deadline: lillux::time::MonotonicDeadline,
) -> Result<serde_json::Value> {
    ensure!(
        !callback_deadline.has_elapsed(),
        "scoped observation callback expired"
    );
    let record = exact_record(state, key)?;
    if record.observation_object_hash.is_some() {
        ensure!(
            record.phase == ScopedChildPhase::Retired,
            "scoped observation commit is still being settled by its owner"
        );
        return replay_observation(state, &record);
    }
    ensure!(
        record.phase == ScopedChildPhase::ReleasePermitted,
        "scoped child is not releasably observable"
    );
    if state
        .scoped_producer_processes
        .interactive_io_exact(&record)?
        .is_some()
    {
        state
            .state_store
            .assert_scoped_child_input_closed(&record.initial.attempt_id, &record.initial.owner)?;
    }
    let child = state.scoped_producer_processes.take_for_observation(key)?;
    let result = observe_owned_scoped_producer(state, key, &record, child, callback_deadline);
    let settled = state
        .state_store
        .scoped_child_attempt(&key.attempt_id)
        .ok()
        .flatten()
        .is_some_and(|record| record.phase == ScopedChildPhase::Retired);
    state
        .scoped_producer_processes
        .finish_observation(key, settled)?;
    result
}

fn observe_owned_scoped_producer(
    state: &AppState,
    key: &ScopedProducerProcessKey,
    record: &ScopedChildAttemptRecord,
    mut child: crate::scoped_producer_process::ScopedProducerRunningChild,
    callback_deadline: lillux::time::MonotonicDeadline,
) -> Result<serde_json::Value> {
    if let Some(channel) = child.ingress_handoff.as_ref()
        && let Err(error) = channel.ensure_peer_live_and_quiet()
    {
        let abort = child.process.abort_and_reap_checked();
        let settlement = settle_cleanup_only(state, record);
        bail!(
            "scoped relay verifier peer lost before observation: {error}; abort={abort:?}; cleanup={settlement:?}"
        );
    }
    let wait = child
        .natural_wait_deadline
        .remaining()
        .min(callback_deadline.remaining());
    if wait == Duration::ZERO {
        let abort = child.process.abort_and_reap_checked();
        let settlement = settle_cleanup_only(state, &record);
        bail!(
            "scoped producer observation deadline elapsed; abort={abort:?}; cleanup={settlement:?}"
        );
    }
    let provenance = child.isolation_provenance;
    let expected_applied_launch = child.expected_applied_launch;
    let producer_source = child.producer_source;
    let ingress_handoff = child.ingress_handoff;
    let relay_handoff = child.relay_handoff;
    let stdout_limit = child.maximum_stdout_bytes;
    let stderr_limit = child.maximum_stderr_bytes;
    // Retain the admitted command/workspace/mount lifeline until the exact
    // natural wait has consumed the process owner.
    let _authority = child.authority;
    let applied_launch = match child.process.wait_applied_launch_receipt(wait) {
        Ok(receipt) => receipt,
        Err(error) => {
            let abort = child.process.abort_and_reap_checked();
            let settlement = settle_cleanup_only(state, record);
            bail!(
                "scoped producer has no exact applied-launch receipt: {error}; abort={abort:?}; cleanup={settlement:?}"
            );
        }
    };
    let retained_mounts = record
        .mount_preparation_evidence
        .as_ref()
        .context("released scoped child has no retained mount preparation")?;
    if !applied_launch.matches_commitments(&expected_applied_launch)
        || !applied_launch.matches_post_release_mounts(&retained_mounts.expected)
    {
        let abort = child.process.abort_and_reap_checked();
        let settlement = settle_cleanup_only(state, record);
        bail!(
            "scoped producer applied target or post-release mounts differ from independently compiled plan; abort={abort:?}; cleanup={settlement:?}"
        );
    }
    let wait = child
        .natural_wait_deadline
        .remaining()
        .min(callback_deadline.remaining());
    if wait == Duration::ZERO {
        let abort = child.process.abort_and_reap_checked();
        let settlement = settle_cleanup_only(state, record);
        bail!(
            "scoped producer natural observation deadline elapsed after applied launch; abort={abort:?}; cleanup={settlement:?}"
        );
    }
    let (result, receipt) = match ScopedChildNaturalEmptyReceipt::observe(
        child.process,
        &record,
        wait,
    ) {
        Ok(observed) => observed,
        Err(running) => {
            let abort = running.abort_and_reap_checked();
            let settlement = settle_cleanup_only(state, &record);
            bail!(
                "scoped producer did not prove natural scope emptiness; abort={abort:?}; cleanup={settlement:?}"
            );
        }
    };
    let observed = (|| -> Result<()> {
        if let Some(channel) = ingress_handoff.as_ref() {
            channel.ensure_peer_live_and_quiet()?;
        }
        ensure!(
            !callback_deadline.has_elapsed(),
            "scoped observation callback expired before commit"
        );
        ensure!(
            result.stdout.len() as u64 <= stdout_limit,
            "scoped stdout escaped signed bound"
        );
        ensure!(
            result.stderr.len() as u64 <= stderr_limit,
            "scoped stderr escaped signed bound"
        );
        let receipt_digest = receipt.digest()?;
        let observation = ScopedProducerObservation {
            schema: OBSERVATION_SCHEMA.to_owned(),
            attempt_id: record.initial.attempt_id.clone(),
            launch_owner: record.initial.owner.clone(),
            recipe_digest: record.initial.recipe_digest.clone(),
            recipe_generation: record.initial.recipe_generation.clone(),
            producer_source,
            relay_handoff,
            scenario_digest: record.initial.scenario_digest.clone(),
            process_identity: record
                .process_identity
                .clone()
                .context("released scoped child has no process identity")?,
            scope_recovery: record
                .scope_recovery
                .clone()
                .context("released scoped child has no scope recovery")?,
            isolation_provenance: provenance,
            applied_launch,
            prepared_immutable_sha256: record
                .mount_preparation_evidence
                .as_ref()
                .context("released scoped child has no retained mount evidence")?
                .prepared_immutable_sha256
                .clone(),
            natural_empty_receipt_digest: receipt_digest,
            subprocess_success: result.success,
            producer_exit_clean: result.success
                && result.exit_code == 0
                && !result.timed_out
                && !result.stdout_truncated
                && !result.stderr_truncated
                && result.output_limit_exceeded.is_none()
                && result.launcher_refusal.is_none(),
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            stdout_sha256: lillux::sha256_hex(result.stdout.as_bytes()),
            stdout_bytes: result.stdout.len() as u64,
            maximum_stdout_bytes: stdout_limit,
            stdout: result.stdout,
            stdout_truncated: result.stdout_truncated,
            stderr_sha256: lillux::sha256_hex(result.stderr.as_bytes()),
            stderr_bytes: result.stderr.len() as u64,
            maximum_stderr_bytes: stderr_limit,
            stderr: result.stderr,
            stderr_truncated: result.stderr_truncated,
            output_limit_exceeded: result
                .output_limit_exceeded
                .map(|limit| limit.as_str().to_owned()),
            launcher_refusal: result.launcher_refusal,
        };
        let mut expected = record.clone();
        expected.natural_empty_receipt_digest =
            Some(observation.natural_empty_receipt_digest.clone());
        observation.validate_against(&expected)?;
        ensure!(
            serde_json::to_vec(&observation)?.len() <= MAX_OBSERVATION_RESPONSE_BYTES,
            "scoped observation exceeds the bounded control response"
        );
        let value = serde_json::to_value(&observation)?;
        let object_hash = state
            .state_store
            .record_scoped_child_natural_empty(&receipt, &value)?;
        let committed = exact_record(state, key)?;
        ensure!(
            committed.observation_object_hash.as_deref() == Some(object_hash.as_str()),
            "scoped observation CAS pointer differs after commit"
        );
        observation.validate_against(&committed)?;
        Ok(())
    })();
    if let Err(error) = observed {
        let settlement = settle_cleanup_only(state, &record);
        return Err(error.context(format!(
            "scoped natural observation could not commit; cleanup={settlement:?}"
        )));
    }
    let committed = exact_record(state, key)?;
    settle_observed_scope(state, &committed)?;
    let settled = exact_record(state, key)?;
    ensure!(
        !callback_deadline.has_elapsed(),
        "scoped observation callback expired after settlement"
    );
    replay_observation(state, &settled)
}

fn exact_record(
    state: &AppState,
    key: &ScopedProducerProcessKey,
) -> Result<ScopedChildAttemptRecord> {
    let record = state
        .state_store
        .scoped_child_attempt(&key.attempt_id)?
        .context("unknown scoped child attempt")?;
    ensure!(
        record.initial.owner == key.launch_owner,
        "scoped child launch owner differs"
    );
    ensure!(
        record.initial.owner.thread_id == key.launch_owner.thread_id,
        "scoped child root differs"
    );
    Ok(record)
}

fn settle_observed_scope(state: &AppState, record: &ScopedChildAttemptRecord) -> Result<()> {
    ensure!(
        record.observation_object_hash.is_some() && record.natural_empty_receipt_digest.is_some(),
        "scoped child has no committed natural observation"
    );
    settle_bound_scope(state, record)
}

pub(crate) fn settle_cleanup_only(
    state: &AppState,
    record: &ScopedChildAttemptRecord,
) -> Result<()> {
    settle_bound_scope(state, record)
}

fn settle_bound_scope(state: &AppState, record: &ScopedChildAttemptRecord) -> Result<()> {
    let recovery = record
        .scope_recovery
        .as_ref()
        .context("scoped child has no bound scope")?;
    let attempt_id = &record.initial.attempt_id;
    let mut phase = state
        .state_store
        .scoped_child_attempt(attempt_id)?
        .context("scoped child disappeared during settlement")?
        .phase;
    if matches!(
        phase,
        ScopedChildPhase::ScopeBound
            | ScopedChildPhase::ProcessAttached
            | ScopedChildPhase::ReleasePermitted
            | ScopedChildPhase::NaturalScopeEmpty
    ) {
        state
            .state_store
            .claim_bound_scoped_child_retirement(attempt_id, recovery)?;
        phase = ScopedChildPhase::BoundRetirementPending;
    }
    if phase == ScopedChildPhase::BoundRetirementPending {
        state
            .state_store
            .prove_bound_scoped_child_death(attempt_id, recovery)?;
        phase = ScopedChildPhase::BoundDeathProven;
    }
    if phase == ScopedChildPhase::BoundDeathProven {
        state
            .state_store
            .complete_bound_scoped_child_retirement(attempt_id)?;
    }
    let settled = state
        .state_store
        .scoped_child_attempt(attempt_id)?
        .context("scoped child disappeared after settlement")?;
    ensure!(
        settled.phase == ScopedChildPhase::Retired
            && settled.recovery_death_evidence_digest.is_some()
            && settled.retirement_evidence_digest.is_some(),
        "scoped child remains unsettled"
    );
    Ok(())
}

fn replay_observation(
    state: &AppState,
    record: &ScopedChildAttemptRecord,
) -> Result<serde_json::Value> {
    ensure!(
        record.phase == ScopedChildPhase::Retired
            && record.recovery_death_evidence_digest.is_some()
            && record.retirement_evidence_digest.is_some(),
        "scoped observation cannot replay before exact retirement"
    );
    let hash = record
        .observation_object_hash
        .as_deref()
        .context("scoped child has no committed observation")?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let value = authority
        .cas_store()?
        .get_object(hash)?
        .context("committed scoped observation CAS object absent")?;
    let observation: ScopedProducerObservation =
        serde_json::from_value(value.clone()).context("decode committed scoped observation")?;
    observation.validate_against(record)?;
    ensure!(
        serde_json::to_vec(&value)?.len() <= MAX_OBSERVATION_RESPONSE_BYTES,
        "committed scoped observation exceeds bounded control response"
    );
    Ok(value)
}

/// Corroborate a signed projector's proposed subordinate coordinate using
/// the exact daemon-owned attempt and its committed, validated CAS object.
/// The projector interprets the verifier terminal; it cannot mint process,
/// scope, or natural-exit testimony by naming an attempt.
pub(crate) fn qualification_scoped_attempt_proof(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    _guard: &ryeos_state::CasMutationGuard,
    owner: &LaunchOwner,
    purpose: &ProductQualificationLaunchPurpose,
    candidate: &ExecutionEvidenceCandidateScopedAttemptWire,
) -> Result<ProductQualificationScopedAttemptProof> {
    purpose.validate()?;
    let scenario = purpose
        .policy_source
        .policy
        .producer_scenarios
        .get(&candidate.scenario_id)
        .context("projected scoped attempt has no signed scenario")?;
    let source = purpose
        .producer_recipe_sources
        .get(&candidate.scenario_id)
        .context("projected scoped attempt has no pinned recipe source")?;
    ensure!(
        scenario.recipe_ref == source.canonical_ref,
        "projected scoped scenario recipe differs from pinned source"
    );
    let current = crate::operator_external_content::product_qualification::resolve_current_bundle_producer_recipe_for_purpose(
        state,
        purpose,
        &candidate.scenario_id,
    )?;
    ensure!(
        current.source_identity()? == *source,
        "projected scoped recipe source changed before qualification"
    );
    let record = state
        .state_store
        .scoped_child_attempt_for_owner(owner)?
        .context("qualification verifier has no daemon-owned scoped attempt")?;
    let admitted_stdin =
        crate::operator_external_content::product_qualification::admitted_root_producer_stdin(
            state,
            &owner.thread_id,
            purpose,
        )?;
    let coordinate =
        crate::scoped_producer_authority::ScopedProducerAttemptCoordinate::derive_recorded(
            &owner.thread_id,
            owner,
            &candidate.scenario_id,
            source,
            &admitted_stdin,
        )?;
    ensure!(
        record.initial.attempt_id == candidate.attempt_id
            && record.initial.attempt_id == coordinate.attempt_id()
            && record.initial.scenario_digest == coordinate.scenario_digest()
            && record.initial.owner == *owner
            && record.initial.recipe_digest == source.recipe_digest
            && record.initial.recipe_generation == source.bundle_generation_identity
            && record.phase == ScopedChildPhase::Retired
            && record.recovery_death_evidence_digest.is_some()
            && record.retirement_evidence_digest.is_some(),
        "projected scoped attempt differs from settled daemon journal"
    );
    let value = authority
        .cas_store()?
        .get_object(
            record
                .observation_object_hash
                .as_deref()
                .context("settled scoped attempt has no retained observation")?,
        )?
        .context("settled scoped observation CAS object is missing")?;
    let observation: ScopedProducerObservation = serde_json::from_value(value)?;
    observation.validate_against(&record)?;
    ensure!(
        observation.producer_exit_clean
            && observation.producer_source == *source
            && record.observation_object_hash.as_deref()
                == Some(candidate.observation_object_hash.as_str()),
        "projected scoped observation differs from clean retained producer"
    );
    let proof = ProductQualificationScopedAttemptProof {
        attempt_id: record.initial.attempt_id.clone(),
        launch_owner_digest: digest_json(owner)?,
        scenario_id: candidate.scenario_id.clone(),
        producer_source: source.clone(),
        process_identity_digest: digest_json(
            record
                .process_identity
                .as_ref()
                .context("settled scoped attempt has no process")?,
        )?,
        scope_allocation_digest: digest_json(&record.initial.scope_allocation)?,
        scope_recovery_digest: digest_json(
            record
                .scope_recovery
                .as_ref()
                .context("settled scoped attempt has no scope")?,
        )?,
        mount_preparation_digest: digest_json(
            record
                .mount_preparation_evidence
                .as_ref()
                .context("settled scoped attempt has no mount preparation")?,
        )?,
        natural_empty_receipt_digest: record
            .natural_empty_receipt_digest
            .clone()
            .context("settled scoped attempt has no natural-empty receipt")?,
        observation_object_hash: candidate.observation_object_hash.clone(),
        recovery_death_evidence_digest: record
            .recovery_death_evidence_digest
            .clone()
            .context("settled scoped attempt has no recovery-death evidence")?,
        retirement_evidence_digest: record
            .retirement_evidence_digest
            .clone()
            .context("settled scoped attempt has no retirement evidence")?,
        callback_method_surface_digest:
            crate::callback_token::CallbackRuntimeMethodSurface::qualification_scoped_producer()
                .exact_surface_digest()?,
    };
    proof.validate()?;
    Ok(proof)
}

fn digest_json(value: &impl Serialize) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(value)?)?.as_bytes(),
    ))
}
