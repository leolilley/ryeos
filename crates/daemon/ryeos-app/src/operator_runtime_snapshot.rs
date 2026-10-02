//! Operator-owned, one-attempt production of an unqualified provider locator.
//!
//! The upload is staged from current CAS authority by the executor. This
//! owner independently rejoins its manifest and controller root to the current
//! product witness before claiming the durable contact attempt. A locator is
//! not an installed-runtime qualification or placement grant.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, GuestOwnerSnapshotUpload,
    derive_guest_owner_runtime_manifest_identity, seal_guest_owner_snapshot_upload,
};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    RESTORED_OWNER_MEASUREMENT_PROTOCOL, RESTORED_VERIFIER_ADAPTER_PROTOCOL,
    RestoredOwnerChallenge, RestoredVerifierAdapterRequest, RestoredVerifierAdapterResponse,
    RestoredVerifierAttemptIntent,
};
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL,
    RUNTIME_SNAPSHOT_INTENT_SCHEMA, RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL,
    RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA, RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL,
    RUNTIME_SNAPSHOT_READINESS_PROTOCOL, RUNTIME_SNAPSHOT_STAGE_SCHEMA,
    RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL, RuntimeSnapshotAdapterRequest,
    RuntimeSnapshotAdapterResponse, RuntimeSnapshotCreateAdapterRequest, RuntimeSnapshotIntent,
    RuntimeSnapshotQualificationAdapterRequest, RuntimeSnapshotQualificationAdapterResponse,
    RuntimeSnapshotQualificationIntent, RuntimeSnapshotQualificationTerminationAdapterRequest,
    RuntimeSnapshotQualificationTerminationAdapterResponse,
    RuntimeSnapshotQualificationTerminationIntent, RuntimeSnapshotReadinessRequest,
    RuntimeSnapshotSource, RuntimeSnapshotStage, RuntimeSnapshotStageIntent,
    RuntimeSnapshotUploadAdapterRequest, RuntimeSnapshotUploadReceipt,
};
use ryeos_external_execution_contract::runtime_snapshot_bootstrap::{
    BOOTSTRAP_ADAPTER_PROTOCOL, BOOTSTRAP_INTENT_SCHEMA, BOOTSTRAP_READINESS_PROTOCOL,
    BOOTSTRAP_TERMINATION_PROTOCOL, RuntimeSnapshotBootstrapAdapterRequest,
    RuntimeSnapshotBootstrapAdapterResponse, RuntimeSnapshotBootstrapIntent,
    RuntimeSnapshotBootstrapReadinessAdapterResponse, RuntimeSnapshotBootstrapReadinessObservation,
    RuntimeSnapshotBootstrapReadinessRequest, RuntimeSnapshotBootstrapTerminationAdapterRequest,
    RuntimeSnapshotBootstrapTerminationAdapterResponse, RuntimeSnapshotBootstrapTerminationIntent,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::products::{ProductShape, ProductStorage};
use ryeos_state::object_closure::load_exact_cas_object_with_cas;

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::runtime_db::restored_verifier_attempt::{
    RestoredVerifierAttemptClaim, RestoredVerifierAttemptPhase, RestoredVerifierAttemptRecord,
};
use crate::runtime_db::runtime_snapshot::{
    RuntimeSnapshotAttemptClaim, RuntimeSnapshotPhase, RuntimeSnapshotRecord,
};
use crate::runtime_db::runtime_snapshot_bootstrap::{
    SnapshotBootstrapAttemptClaim, SnapshotBootstrapPhase, SnapshotBootstrapRecord,
};
use crate::runtime_db::runtime_snapshot_bootstrap_termination::{
    BootstrapTerminationClaim, BootstrapTerminationPhase, BootstrapTerminationRecord,
};
use crate::runtime_db::runtime_snapshot_qualification::{
    SnapshotQualificationAttemptClaim, SnapshotQualificationPhase, SnapshotQualificationRecord,
};
use crate::runtime_db::runtime_snapshot_qualification_termination::{
    QualificationTerminationClaim, QualificationTerminationPhase, QualificationTerminationRecord,
};
use crate::runtime_db::runtime_snapshot_stage::{
    RuntimeSnapshotStageClaim, RuntimeSnapshotStagePhase,
};
use crate::state::AppState;

pub struct SnapshotProductionRequest {
    pub binding_id: String,
    pub source: SnapshotProductionSource,
    pub staged_identity: GuestOwnerRuntimeManifestIdentity,
    pub staged_root: lillux::PinnedDirectory,
}

pub enum SnapshotProductionSource {
    CapturedProduct {
        witness_hash: String,
        source: ProductWitnessSource,
        source_occurrence_id: String,
    },
    BundleMaterialization {
        materialization_binding_id: String,
        coordinate_digest: String,
        attestation_hash: String,
        bootstrap_operation_id: String,
    },
}

pub struct SnapshotBootstrapRequest {
    pub binding_id: String,
    pub materialization_binding_id: String,
    pub coordinate_digest: String,
    pub attestation_hash: String,
    pub maximum_lifetime_seconds: u32,
}

/// Create only a source occurrence. A bound create response is not readiness,
/// upload authority, or group proof; the snapshot path remains closed until
/// those independent joins are installed.
pub fn bootstrap_source(
    state: &AppState,
    context: &HandlerContext,
    request: SnapshotBootstrapRequest,
) -> Result<SnapshotBootstrapRecord> {
    // Fresh provider contact belongs to the same installed signed Bundle
    // generation that supplied the adapter capture. Supported publisher
    // replacement is excluded through escrow publication and the one-shot
    // provider attempt; historical cleanup uses the retained closure instead.
    state
        .engine
        .with_checked_bundle_generation(|_| bootstrap_source_checked(state, context, request))
}

fn bootstrap_source_checked(
    state: &AppState,
    context: &HandlerContext,
    request: SnapshotBootstrapRequest,
) -> Result<SnapshotBootstrapRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let binding = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| binding.id() == request.binding_id)
        .context("current signed snapshot producer binding is absent")?;
    ensure!(
        (1..=binding.maximum_bootstrap_lifetime_seconds())
            .contains(&request.maximum_lifetime_seconds),
        "bootstrap lifetime exceeds current signed producer ceiling"
    );
    let source_ceiling = staging_source_ceiling(state, context, &request.binding_id)?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current =
        crate::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
            state,
            context,
            &request.materialization_binding_id,
            &request.coordinate_digest,
            &request.attestation_hash,
            &authority,
            &guard,
        )?;
    ensure!(
        current.source.maximum_owner_bytes <= source_ceiling,
        "bootstrap materialization exceeds signed source ceiling"
    );
    let source = RuntimeSnapshotSource::BundleMaterialization {
        materialization_attestation_hash: current.attestation_hash,
        source_coordinate_digest: request.coordinate_digest.clone(),
        materialization_binding_digest: current.source.materialization_binding_digest,
    };
    let identity = current.identity;
    drop(guard);
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    state
        .external_placement_backends
        .preflight_runtime_snapshot(binding, &credential)?;
    let (bootstrap_profile_digest, provider_spec_digest) = state
        .external_placement_backends
        .bootstrap_source_profile(binding)?;
    let now = i64::try_from(lillux::time::timestamp_millis())?;
    let mut intent = RuntimeSnapshotBootstrapIntent {
        schema: BOOTSTRAP_INTENT_SCHEMA,
        operation_id: String::new(),
        owner_principal: context.fingerprint.clone(),
        provider_id: binding.backend().to_owned(),
        provider_group_id: binding.provider_group_id().to_owned(),
        production_binding_digest: binding.digest().to_owned(),
        bootstrap_profile_digest,
        adapter_artifact_hash: binding.adapter_artifact_hash().to_owned(),
        provider_spec_digest,
        settings_digest: binding.settings_digest().to_owned(),
        source,
        guest_runtime_manifest_hash: identity.manifest_hash.clone(),
        maximum_lifetime_seconds: request.maximum_lifetime_seconds,
        attempt_deadline_ms: now
            .checked_add(i64::from(binding.contact_timeout_seconds()) * 1_000)
            .context("bootstrap attempt deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .snapshot_bootstrap_operation(&intent.operation_id)?
    {
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent == existing.intent,
            "retained bootstrap attempt contradicts current source coordinates"
        );
    }
    intent.validate()?;
    // New contact rechecks current source authority; historical retained
    // materialization bytes alone do not authorize a fresh provider POST.
    let contact_guard = authority.acquire_shared_guard()?;
    let again =
        crate::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
            state,
            context,
            &request.materialization_binding_id,
            &request.coordinate_digest,
            &request.attestation_hash,
            &authority,
            &contact_guard,
        )?;
    ensure!(
        again.identity == identity && again.attestation_hash == request.attestation_hash,
        "bootstrap source changed before contact admission"
    );
    // Keep the CAS/head mutation guard through the one-shot provider attempt.
    // A source recheck followed by an unguarded contact window would permit
    // the materialization head to change after admission but before POST.
    let _contact_guard = contact_guard;
    let lifecycle_capture = state
        .external_placement_backends
        .lifecycle_escrow_capture(binding)?;
    let published_lifecycle = crate::external_artifacts::publish_retained_lifecycle_artifacts(
        &state.state_store,
        &context.fingerprint,
        &lifecycle_capture,
    )?;
    let retained_binding = binding.retained_generation()?;
    let reserved = state
        .state_store
        .reserve_snapshot_bootstrap(&intent, &retained_binding)?;
    let linked = state
        .state_store
        .attach_snapshot_bootstrap_artifacts(&intent.operation_id, &published_lifecycle)?;
    ensure!(
        linked.lifecycle_artifacts_root.as_deref() == Some(published_lifecycle.root_hash()),
        "bootstrap journal did not retain the protected lifecycle closure"
    );
    // Prove the exact journaled closure can independently reconstruct the
    // cleanup-only backend before the first provider mutation is claimed.
    let _cleanup = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
        state,
        &linked,
        binding,
        &credential,
    )?;
    let claim = state
        .state_store
        .claim_snapshot_bootstrap_attempt(&intent.operation_id)?;
    let SnapshotBootstrapAttemptClaim::StartAttempt(_) = claim else {
        return Ok(match claim {
            SnapshotBootstrapAttemptClaim::Reconcile(record)
            | SnapshotBootstrapAttemptClaim::OccurrenceBound(record)
            | SnapshotBootstrapAttemptClaim::LateOccurrenceBound(record)
            | SnapshotBootstrapAttemptClaim::RejectedOccurrenceBound(record) => record,
            SnapshotBootstrapAttemptClaim::StartAttempt(_) => unreachable!(),
        });
    };
    ensure!(
        reserved.intent == intent,
        "bootstrap reservation changed before contact claim"
    );
    let adapter_request = RuntimeSnapshotBootstrapAdapterRequest {
        protocol: BOOTSTRAP_ADAPTER_PROTOCOL.into(),
        intent,
        provider_spec_digest: reserved.intent.provider_spec_digest.clone(),
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let attempted = state
        .external_placement_backends
        .create_snapshot_bootstrap_source(binding, &credential, &adapter_request, deadline);
    match attempted {
        Ok(observed) => match observed.value {
            RuntimeSnapshotBootstrapAdapterResponse::OccurrenceBound { mut occurrence } => {
                occurrence.contact_deadline_exceeded = observed.deadline_exceeded;
                state
                    .state_store
                    .bind_snapshot_bootstrap_occurrence(&occurrence)
            }
            RuntimeSnapshotBootstrapAdapterResponse::Uncertain { .. } => state
                .state_store
                .quarantine_snapshot_bootstrap_attempt(&adapter_request.intent.operation_id),
        },
        Err(error) => {
            state
                .state_store
                .quarantine_snapshot_bootstrap_attempt(&adapter_request.intent.operation_id)?;
            Err(error)
        }
    }
}

/// Exact point read of the one-shot bootstrap operation. An uncertain result
/// remains uncertain; reading it never authorizes a replacement create.
pub fn get_bootstrap_source(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<SnapshotBootstrapRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        lillux::valid_hash(operation_id),
        "bootstrap operation ID is invalid"
    );
    let record = state
        .state_store
        .snapshot_bootstrap_operation(operation_id)?
        .context("bootstrap operation is absent")?;
    ensure!(
        record.intent.owner_principal == context.fingerprint,
        "bootstrap operation belongs to another operator"
    );
    Ok(record)
}

/// Observe the exact created source through an authenticated read-only GET.
/// A pending result remains pending; only a checked running observation is
/// durably retained, and neither result grants snapshot upload by itself.
pub fn observe_bootstrap_source(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<SnapshotBootstrapRecord> {
    let record = get_bootstrap_source(state, context, operation_id)?;
    ensure!(
        record.phase == SnapshotBootstrapPhase::OccurrenceBound,
        "bootstrap source is not a timely verified occurrence"
    );
    if record.readiness.is_some() {
        return Ok(record);
    }
    match fetch_bootstrap_source_readiness(state, &record)? {
        Some(observation) => state
            .state_store
            .bind_snapshot_bootstrap_readiness(&observation),
        None => get_bootstrap_source(state, context, operation_id),
    }
}

/// Historical readiness is not a live lease. Snapshot upload performs this
/// authenticated GET again before the atomic source-use reservation and
/// provider contact.
fn fetch_bootstrap_source_readiness(
    state: &AppState,
    record: &SnapshotBootstrapRecord,
) -> Result<Option<RuntimeSnapshotBootstrapReadinessObservation>> {
    ensure!(
        record.phase == SnapshotBootstrapPhase::OccurrenceBound,
        "bootstrap source is not a timely verified occurrence"
    );
    let operation_id = &record.intent.operation_id;
    let binding = state
        .state_store
        .retained_snapshot_bootstrap_binding(operation_id)?
        .context("bootstrap source lost exact retained observation authority")?
        .recovered_binding()?;
    ensure!(
        binding.backend() == record.intent.provider_id
            && binding.provider_group_id() == record.intent.provider_group_id
            && binding.adapter_artifact_hash() == record.intent.adapter_artifact_hash
            && binding.settings_digest() == record.intent.settings_digest,
        "bootstrap source differs from retained signed producer"
    );
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    let recovered = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
        state,
        record,
        &binding,
        &credential,
    )?;
    let request = RuntimeSnapshotBootstrapReadinessRequest {
        protocol: BOOTSTRAP_READINESS_PROTOCOL.into(),
        intent: record.intent.clone(),
        occurrence: record
            .occurrence
            .clone()
            .context("bound source has no occurrence")?,
        provider_spec_digest: record.intent.provider_spec_digest.clone(),
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let observed = recovered.observe(&binding, &credential, &request, deadline)?;
    Ok(match observed.value {
        RuntimeSnapshotBootstrapReadinessAdapterResponse::Running { observation } => {
            ensure!(
                !observed.deadline_exceeded,
                "bootstrap readiness exceeded its contact deadline"
            );
            observation.validate_for(&request)?;
            let now = i64::try_from(lillux::time::timestamp_millis())?;
            ensure!(
                observation.observed_at_ms <= now
                    && now.saturating_sub(observation.observed_at_ms) <= 60_000
                    && record.occurrence.as_ref().and_then(|source| source
                        .provider_creation_observation["created_at"]
                        .as_str())
                        == Some(observation.observed_created_at.as_str()),
                "bootstrap source live observation is stale or changed creation identity"
            );
            Some(observation)
        }
        RuntimeSnapshotBootstrapReadinessAdapterResponse::NotReady { .. } => None,
    })
}

pub fn get_bootstrap_source_termination(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<BootstrapTerminationRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        lillux::valid_hash(operation_id),
        "bootstrap termination ID is invalid"
    );
    let record = state
        .state_store
        .bootstrap_termination_operation(operation_id)?
        .context("bootstrap termination is absent")?;
    ensure!(
        record.intent.owner_principal == context.fingerprint,
        "bootstrap termination belongs to another operator"
    );
    Ok(record)
}

/// One mutation opportunity per exact source occurrence. An ambiguous POST
/// only permits a later authenticated GET, never another termination POST.
pub fn terminate_bootstrap_source(
    state: &AppState,
    context: &HandlerContext,
    bootstrap_operation_id: &str,
) -> Result<BootstrapTerminationRecord> {
    let source = get_bootstrap_source(state, context, bootstrap_operation_id)?;
    terminate_retained_bootstrap_source(state, source)
}

/// Advance cleanup from the original journal, without asking whether its
/// operator still has permission to create new work. Only the public wrapper
/// above accepts a request context; daemon recovery selects exact due sources
/// from retained state and uses this same one-shot termination state machine.
fn terminate_retained_bootstrap_source(
    state: &AppState,
    source: SnapshotBootstrapRecord,
) -> Result<BootstrapTerminationRecord> {
    let bootstrap_operation_id = &source.intent.operation_id;
    let occurrence = source
        .occurrence
        .clone()
        .context("bootstrap source has no exact occurrence")?;
    let unsettled = state
        .state_store
        .bootstrap_has_unsettled_snapshot(bootstrap_operation_id)?;
    let mut intent = RuntimeSnapshotBootstrapTerminationIntent {
        schema: 2,
        mode: if unsettled {
            ryeos_external_execution_contract::runtime_snapshot_bootstrap::BootstrapCleanupMode::ObserveOnly
        } else {
            ryeos_external_execution_contract::runtime_snapshot_bootstrap::BootstrapCleanupMode::TerminateOnce
        },
        operation_id: String::new(),
        bootstrap_operation_id: source.intent.operation_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        owner_principal: source.intent.owner_principal.clone(),
        provider_id: source.intent.provider_id.clone(),
        provider_group_id: source.intent.provider_group_id.clone(),
        provider_spec_digest: source.intent.provider_spec_digest.clone(),
        attempt_deadline_ms: i64::try_from(lillux::time::timestamp_millis())?
            .checked_add(60_000)
            .context("bootstrap termination deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .bootstrap_termination_operation(&intent.operation_id)?
    {
        if existing.phase == BootstrapTerminationPhase::Terminal {
            return Ok(existing);
        }
        // A cleanup choice is immutable for this occurrence. An observation-
        // only reservation can never later turn into a termination mutation.
        intent.mode = existing.intent.mode;
        if existing.phase != BootstrapTerminationPhase::Reserved
            || existing.intent.attempt_deadline_ms >= intent.attempt_deadline_ms
        {
            intent = existing.intent;
        }
    }
    intent.validate_for(&source.intent, &occurrence)?;
    let binding = state
        .state_store
        .retained_snapshot_bootstrap_binding(bootstrap_operation_id)?
        .context("bootstrap source lost exact retained cleanup authority")?
        .recovered_binding()?;
    ensure!(
        binding.backend() == source.intent.provider_id
            && binding.adapter_artifact_hash() == source.intent.adapter_artifact_hash
            && binding.provider_group_id() == source.intent.provider_group_id
            && binding.settings_digest() == source.intent.settings_digest,
        "bootstrap cleanup differs from retained signed producer"
    );
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    let recovered = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
        state,
        &source,
        &binding,
        &credential,
    )?;
    let reserved = state.state_store.reserve_bootstrap_termination(&intent)?;
    let request = RuntimeSnapshotBootstrapTerminationAdapterRequest {
        protocol: BOOTSTRAP_TERMINATION_PROTOCOL.into(),
        intent: reserved.intent,
        bootstrap_intent: source.intent,
        occurrence,
        provider_spec_digest: intent.provider_spec_digest.clone(),
    };
    request.validate()?;
    let claim = state
        .state_store
        .claim_bootstrap_termination_attempt(&intent.operation_id)?;
    let first_contact = matches!(claim, BootstrapTerminationClaim::StartAttempt(_))
        && request.intent.mode
            == ryeos_external_execution_contract::runtime_snapshot_bootstrap::BootstrapCleanupMode::TerminateOnce;
    if let BootstrapTerminationClaim::Terminal(record) = claim {
        return Ok(record);
    }
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let attempted = recovered.terminate(&binding, &credential, &request, first_contact, deadline);
    match attempted {
        Ok(observed) => match observed.value {
            RuntimeSnapshotBootstrapTerminationAdapterResponse::Terminal { mut observation } => {
                observation.contact_deadline_exceeded = observed.deadline_exceeded;
                state
                    .state_store
                    .bind_bootstrap_terminal_observation(&observation)
            }
            RuntimeSnapshotBootstrapTerminationAdapterResponse::Uncertain { .. } => {
                if first_contact {
                    state
                        .state_store
                        .quarantine_bootstrap_termination_attempt(&request.intent.operation_id)
                } else {
                    state
                        .state_store
                        .bootstrap_termination_operation(&request.intent.operation_id)?
                        .context("bootstrap termination disappeared during reconciliation")
                }
            }
        },
        Err(error) => {
            if first_contact {
                state
                    .state_store
                    .quarantine_bootstrap_termination_attempt(&request.intent.operation_id)?;
            }
            Err(error)
        }
    }
}

#[derive(Debug, Default)]
pub struct BootstrapSourceCleanupRecovery {
    pub discovered: usize,
    pub terminal: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
}

/// Advance at most one original termination contact or exact reconciliation
/// for each due source. This runs under daemon recovery ownership: revoked
/// new-work grants cannot strand a known provider occurrence, and a failed
/// source does not prevent other retained obligations from advancing.
pub fn recover_bootstrap_source_cleanups(
    state: &AppState,
) -> Result<BootstrapSourceCleanupRecovery> {
    let now_ms = i64::try_from(lillux::time::timestamp_millis())?;
    let sources = state.state_store.recoverable_bootstrap_sources(now_ms)?;
    let mut report = BootstrapSourceCleanupRecovery {
        discovered: sources.len(),
        ..Default::default()
    };
    for id in sources {
        let result = (|| {
            let source = state
                .state_store
                .snapshot_bootstrap_operation(&id)?
                .context("recoverable bootstrap source disappeared")?;
            ensure!(
                source.occurrence.is_some(),
                "recoverable source lost occurrence"
            );
            terminate_retained_bootstrap_source(state, source)
        })();
        match result {
            Ok(record) if record.phase == BootstrapTerminationPhase::Terminal => {
                report.terminal += 1
            }
            Ok(_) => report.pending += 1,
            Err(error) => report.failures.push((id, format!("{error:#}"))),
        }
    }
    Ok(report)
}

/// Resolve a bounded staging ceiling exclusively from the current signed
/// producer binding. The API must not accept an upload budget from its caller.
pub fn staging_source_ceiling(
    state: &AppState,
    context: &HandlerContext,
    binding_id: &str,
) -> Result<u64> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let binding = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| binding.id() == binding_id)
        .context("current signed snapshot producer binding is absent")?;
    binding
        .maximum_upload_bytes()
        .checked_sub(16 * 1024)
        .filter(|ceiling| *ceiling > 0)
        .context("signed snapshot producer upload ceiling is too small")
}

/// Exact, read-only recovery lookup. A lost response to a non-idempotent
/// provider attempt must be reconciled through its retained operation, never
/// by issuing another create request.
pub fn get_operation(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<RuntimeSnapshotRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        operation_id.len() == 64
            && operation_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "runtime snapshot operation ID is not a canonical SHA-256 digest"
    );
    let operation = state
        .state_store
        .runtime_snapshot_operation(operation_id)?
        .context("runtime snapshot operation is absent")?;
    ensure!(
        operation.intent.owner_principal == context.fingerprint,
        "runtime snapshot operation belongs to another operator"
    );
    Ok(operation)
}

pub fn get_qualification_operation(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<SnapshotQualificationRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        lillux::valid_hash(operation_id),
        "qualification operation ID is not a canonical digest"
    );
    let record = state
        .state_store
        .snapshot_qualification_operation(operation_id)?
        .context("snapshot qualification operation is absent")?;
    ensure!(
        record.intent.owner_principal == context.fingerprint,
        "snapshot qualification belongs to another operator"
    );
    Ok(record)
}

pub fn get_qualification_termination(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<QualificationTerminationRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        lillux::valid_hash(operation_id),
        "qualification termination ID is not a canonical digest"
    );
    let record = state
        .state_store
        .qualification_termination_operation(operation_id)?
        .context("qualification termination operation is absent")?;
    ensure!(
        record.intent.owner_principal == context.fingerprint,
        "qualification termination belongs to another operator"
    );
    Ok(record)
}

/// Create at most one restored Sandbox for the exact retained snapshot.
/// This operation stops at an occurrence locator; verifier execution,
/// whole-guest settlement, and qualification remain separate authorities.
pub fn create_qualification_occurrence(
    state: &AppState,
    context: &HandlerContext,
    qualification_binding_id: &str,
    snapshot_operation_id: &str,
) -> Result<SnapshotQualificationRecord> {
    state.engine.with_checked_bundle_generation(|_| {
        create_qualification_occurrence_checked(
            state,
            context,
            qualification_binding_id,
            snapshot_operation_id,
        )
    })
}

fn create_qualification_occurrence_checked(
    state: &AppState,
    context: &HandlerContext,
    qualification_binding_id: &str,
    snapshot_operation_id: &str,
) -> Result<SnapshotQualificationRecord> {
    let source = get_operation(state, context, snapshot_operation_id)?;
    ensure!(
        source.phase == RuntimeSnapshotPhase::Bound && source.readiness.is_some(),
        "qualification source snapshot is not ready"
    );
    let locator = source
        .locator
        .clone()
        .context("ready source has no locator")?;
    let readiness = source
        .readiness
        .clone()
        .context("ready source has no readiness")?;
    let qualification = state
        .node_config
        .runtime_snapshot_qualification
        .iter()
        .find(|binding| binding.id() == qualification_binding_id)
        .context("current signed snapshot qualification binding is absent")?;
    let producer = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| {
            binding.id() == qualification.production_binding_id()
                && binding.digest() == qualification.production_binding_digest()
        })
        .context("qualification lost its exact signed producer binding")?;
    ensure!(
        source.intent.production_profile_digest == producer.digest()
            && source.intent.provider_id == producer.backend()
            && source.intent.adapter_artifact_hash == producer.adapter_artifact_hash()
            && source.intent.provider_group_id == producer.provider_group_id(),
        "qualification source differs from its signed producer"
    );
    // A retained materialization proves historical bytes, not permission for
    // a new restored occurrence. Keep the exact current head and Bundle
    // generation fenced through the one-shot provider contact below.
    let materialization_authority = if matches!(
        &source.intent.source,
        RuntimeSnapshotSource::BundleMaterialization { .. }
    ) {
        Some(state.state_store.pinned_state_authority()?)
    } else {
        None
    };
    let _materialization_guard = materialization_authority
        .as_ref()
        .map(|authority| authority.acquire_shared_guard())
        .transpose()?;
    if let RuntimeSnapshotSource::BundleMaterialization {
        materialization_attestation_hash,
        source_coordinate_digest,
        materialization_binding_digest,
    } = &source.intent.source
    {
        let mut bindings = state
            .node_config
            .guest_runtime_materialization
            .iter()
            .filter(|binding| binding.digest() == materialization_binding_digest);
        let binding = bindings
            .next()
            .context("materialized qualification lost its exact signed binding")?;
        ensure!(
            bindings.next().is_none(),
            "materialized qualification binding digest is ambiguous"
        );
        let current = crate::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
            state,
            context,
            binding.id(),
            source_coordinate_digest,
            materialization_attestation_hash,
            materialization_authority.as_ref().unwrap(),
            _materialization_guard.as_ref().unwrap(),
        )?;
        ensure!(
            current.source.materialization_binding_digest == *materialization_binding_digest
                && current.identity.manifest_hash == source.intent.guest_runtime_manifest_hash
                && current.identity.owner_executable_sha256
                    == source.intent.owner_executable_sha256
                && current.identity.controller_public_root == source.intent.controller_public_root,
            "materialized qualification differs from current exact runtime source"
        );
    }
    let access = producer.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    state
        .external_placement_backends
        .preflight_snapshot_qualification_create(producer, qualification, &credential)?;
    let now = lillux::time::timestamp_millis();
    let mut intent = RuntimeSnapshotQualificationIntent {
        schema: RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
        operation_id: String::new(),
        owner_principal: context.fingerprint.clone(),
        snapshot_operation_id: source.intent.operation_id.clone(),
        snapshot_intent_digest: source.intent.digest()?,
        snapshot_id: locator.snapshot_id.clone(),
        provider_id: producer.backend().to_owned(),
        provider_group_id: producer.provider_group_id().to_owned(),
        qualification_profile_digest: qualification.digest().to_owned(),
        adapter_artifact_hash: producer.adapter_artifact_hash().to_owned(),
        provider_spec_digest: qualification.provider_spec_digest().to_owned(),
        settings_digest: qualification.settings_digest().to_owned(),
        verifier_artifact_hash: qualification.verifier_artifact_hash().to_owned(),
        maximum_lifetime_seconds: qualification.maximum_lifetime_seconds(),
        attempt_deadline_ms: now
            .checked_add(i64::from(qualification.contact_timeout_seconds()) * 1_000)
            .context("qualification create deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .snapshot_qualification_operation(&intent.operation_id)?
    {
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent == existing.intent,
            "retained qualification attempt contradicts current signed coordinates"
        );
    }
    intent.validate_for(&source.intent, &locator)?;
    let reserved = state.state_store.reserve_snapshot_qualification(&intent)?;
    let claim = state
        .state_store
        .claim_snapshot_qualification_attempt(&intent.operation_id)?;
    let SnapshotQualificationAttemptClaim::StartAttempt(_) = claim else {
        return Ok(match claim {
            SnapshotQualificationAttemptClaim::Reconcile(record)
            | SnapshotQualificationAttemptClaim::OccurrenceBound(record) => record,
            SnapshotQualificationAttemptClaim::StartAttempt(_) => unreachable!(),
        });
    };
    ensure!(
        reserved.intent == intent,
        "qualification reservation changed before contact claim"
    );
    let request = RuntimeSnapshotQualificationAdapterRequest {
        protocol: RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL.into(),
        provider_spec_digest: qualification.provider_spec_digest().to_owned(),
        intent,
        source_intent: source.intent,
        locator,
        readiness,
    };
    request.validate()?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(qualification.contact_timeout_seconds()),
    ));
    let attempted = state
        .external_placement_backends
        .create_snapshot_qualification_occurrence(
            producer,
            qualification,
            &credential,
            &request,
            deadline,
        );
    match attempted {
        Ok(observation) => match observation.value {
            RuntimeSnapshotQualificationAdapterResponse::OccurrenceBound { mut occurrence } => {
                occurrence.contact_deadline_exceeded = observation.deadline_exceeded;
                state
                    .state_store
                    .bind_snapshot_qualification_occurrence(&occurrence)
            }
            RuntimeSnapshotQualificationAdapterResponse::Uncertain { .. } => state
                .state_store
                .quarantine_snapshot_qualification_attempt(&request.intent.operation_id),
        },
        Err(error) => {
            state
                .state_store
                .quarantine_snapshot_qualification_attempt(&request.intent.operation_id)?;
            Err(error)
        }
    }
}

/// Execute one independently admitted verifier inside the exact restored
/// Sandbox. This returns a retained content observation, not a runtime
/// qualification or activation grant.
pub fn verify_qualification_occurrence(
    state: &AppState,
    context: &HandlerContext,
    qualification_operation_id: &str,
) -> Result<RestoredVerifierAttemptRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    crate::hosted_operation::with_qualification_occurrence_contact(
        qualification_operation_id,
        || {
            verify_qualification_occurrence_under_contact_gate(
                state,
                context,
                qualification_operation_id,
            )
        },
    )
}

fn verify_qualification_occurrence_under_contact_gate(
    state: &AppState,
    context: &HandlerContext,
    qualification_operation_id: &str,
) -> Result<RestoredVerifierAttemptRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let qualified = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("snapshot qualification operation is absent")?;
    ensure!(
        qualified.intent.owner_principal == context.fingerprint
            && qualified.phase == SnapshotQualificationPhase::OccurrenceBound,
        "restored verifier has no operator-owned bound occurrence"
    );
    let occurrence = qualified
        .occurrence
        .clone()
        .context("restored occurrence is absent")?;
    ensure!(
        !occurrence.contact_deadline_exceeded,
        "restored occurrence was created late"
    );
    let source = get_operation(state, context, &qualified.intent.snapshot_operation_id)?;
    ensure!(
        source.phase == RuntimeSnapshotPhase::Bound,
        "restored verifier source is not bound"
    );
    let locator = source
        .locator
        .clone()
        .context("restored verifier source has no locator")?;
    let readiness = source
        .readiness
        .clone()
        .context("restored verifier source is not ready")?;
    let qualification = state
        .node_config
        .runtime_snapshot_qualification
        .iter()
        .find(|binding| binding.digest() == qualified.intent.qualification_profile_digest)
        .context("current signed snapshot qualification binding is absent")?;
    let producer = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| {
            binding.id() == qualification.production_binding_id()
                && binding.digest() == qualification.production_binding_digest()
        })
        .context("restored verifier lost its exact signed producer binding")?;
    ensure!(
        qualified.intent.adapter_artifact_hash == producer.adapter_artifact_hash()
            && qualified.intent.provider_spec_digest == qualification.provider_spec_digest()
            && qualified.intent.settings_digest == qualification.settings_digest()
            && qualified.intent.verifier_artifact_hash == qualification.verifier_artifact_hash(),
        "restored verifier differs from exact signed qualification authority"
    );
    let upload = state
        .external_placement_backends
        .seal_restoration_verifier_upload(producer, qualification)?;
    let access = producer.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    state
        .external_placement_backends
        .preflight_snapshot_qualification_create(producer, qualification, &credential)?;
    let now = lillux::time::timestamp_millis();
    let mut intent = RestoredVerifierAttemptIntent {
        schema: 2,
        operation_id: String::new(),
        qualification_operation_id: qualified.intent.operation_id.clone(),
        restored_occurrence_id: occurrence.occurrence_id.clone(),
        verifier_artifact_hash: qualification.verifier_artifact_hash().into(),
        upload_sha256: upload.sha256().into(),
        upload_bytes: upload.bytes(),
        purpose: ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { challenge: RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: source.intent.operation_id.clone(),
            snapshot_id: locator.snapshot_id.clone(),
            restored_occurrence_id: occurrence.occurrence_id.clone(),
            nonce_hex: hex::encode(lillux::crypto::generate_random_bytes::<32>()),
        } },
        attempt_deadline_ms: now
            .checked_add(i64::from(qualification.contact_timeout_seconds()) * 1_000)
            .context("restored verifier deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .restored_verifier_attempt(&intent.operation_id)?
    {
        intent.purpose = existing.intent.purpose.clone();
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent == existing.intent,
            "retained verifier attempt contradicts current signed coordinates"
        );
    }
    intent.validate_for(&source.intent, &locator, &qualified.intent, &occurrence)?;
    let reserved = state
        .state_store
        .reserve_restored_verifier_attempt(&intent)?;
    let claim = state
        .state_store
        .claim_restored_verifier_attempt(&intent.operation_id)?;
    let RestoredVerifierAttemptClaim::StartAttempt(_) = claim else {
        return Ok(match claim {
            RestoredVerifierAttemptClaim::Reconcile(record)
            | RestoredVerifierAttemptClaim::Observed(record) => record,
            RestoredVerifierAttemptClaim::StartAttempt(_) => unreachable!(),
        });
    };
    ensure!(
        reserved.intent == intent,
        "verifier reservation changed before contact claim"
    );
    let request = RestoredVerifierAdapterRequest {
        protocol: RESTORED_VERIFIER_ADAPTER_PROTOCOL.into(),
        provider_spec_digest: qualification.provider_spec_digest().into(),
        intent,
        consumer_selection: None,
        source_intent: source.intent,
        locator,
        readiness,
        qualification_intent: qualified.intent,
        occurrence,
        upload_descriptor: upload
            .descriptor()
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        upload_bytes: upload.bytes(),
        upload_sha256: upload.sha256().into(),
    };
    request.validate()?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(qualification.contact_timeout_seconds()),
    ));
    let attempted = state
        .external_placement_backends
        .verify_restored_snapshot_once(
            producer,
            qualification,
            &credential,
            &request,
            upload.descriptor(),
            deadline,
        );
    match attempted {
        Ok(output) => match output.value {
            RestoredVerifierAdapterResponse::Observed { mut observation } => {
                observation.contact_deadline_exceeded = output.deadline_exceeded;
                state
                    .state_store
                    .bind_restored_verifier_observation(&observation)
            }
            RestoredVerifierAdapterResponse::Uncertain { .. } => state
                .state_store
                .quarantine_restored_verifier_attempt(&request.intent.operation_id),
            RestoredVerifierAdapterResponse::ConsumerObserved { .. } => {
                state
                    .state_store
                    .quarantine_restored_verifier_attempt(&request.intent.operation_id)?;
                anyhow::bail!("owner measurement contact returned consumer evidence")
            }
        },
        Err(error) => {
            state
                .state_store
                .quarantine_restored_verifier_attempt(&request.intent.operation_id)?;
            Err(error)
        }
    }
}

/// Prepare a challenge under the actual callback, without claiming contact.
/// On replay the existing journal supplies the original nonce and deadline;
/// changed archive bytes or protected selection refuse, never mint a new run.
pub struct PreparedConsumerAttempt {
    intent: RestoredVerifierAttemptIntent,
    retained: Option<RestoredVerifierAttemptRecord>,
    selection: ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection,
    deadline: lillux::time::MonotonicDeadline,
}

impl PreparedConsumerAttempt {
    pub fn deadline(&self) -> lillux::time::MonotonicDeadline {
        self.deadline
    }

    /// Bind mechanically copied bytes before this draft may become an intent.
    /// Neither this custody nor successful binding grants provider contact.
    pub fn bind_archive(
        mut self,
        archive: &ryeos_external_execution::restoration_verifier_delivery::PreparedConsumerArchive,
    ) -> Result<RestoredVerifierAttemptIntent> {
        ensure!(
            !self.deadline.has_elapsed(),
            "consumer delivery exceeded original preparation deadline"
        );
        archive.require_verifier_selection(&self.selection)?;
        self.intent.upload_sha256 = archive.sha256().into();
        self.intent.upload_bytes = archive.bytes();
        if let Some(retained) = self.retained {
            ensure!(
                self.intent == retained.intent
                    && retained.consumer_selection.as_ref() == Some(&self.selection),
                "consumer challenge replay changed immutable upload or selection"
            );
        }
        self.intent.consumer_challenge_digest()?;
        Ok(self.intent)
    }
}

pub fn prepare_consumer_verifier_attempt_for_callback(
    state: &AppState,
    token: &str,
    prepared: &crate::operator_external_content::product_qualification::PreparedRetainedConsumerVerifier,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> Result<PreparedConsumerAttempt> {
    use crate::operator_external_content::product_qualification::authenticate_consumer_callback_root;
    use ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose;
    let started_at_ms = lillux::time::timestamp_millis();
    let preparation_timer = lillux::time::MonotonicTimer::start();
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let admitted = authenticate_consumer_callback_root(
        state,
        &authority,
        &guard,
        limits,
        token,
        "runtime.consumer_verification_start",
        prepared.coordinate(),
        qualification_operation_id,
    )?;
    ensure!(
        admitted.purpose() == prepared.purpose() && admitted.selection() == prepared.selection(),
        "consumer challenge preparation differs from exact retained input authority"
    );
    let qualification = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("consumer challenge qualification disappeared")?;
    let occurrence = qualification
        .occurrence
        .as_ref()
        .context("consumer challenge has no retained occurrence")?;
    let source = state
        .state_store
        .runtime_snapshot_operation(&qualification.intent.snapshot_operation_id)?
        .context("consumer challenge snapshot disappeared")?;
    ensure!(
        source.intent.owner_principal == admitted.purpose().owner_fingerprint,
        "consumer challenge snapshot belongs to another owner"
    );
    let profile = state
        .node_config
        .runtime_snapshot_qualification
        .iter()
        .find(|profile| profile.digest() == qualification.intent.qualification_profile_digest)
        .context("consumer challenge protected profile disappeared")?;
    let mut intent = RestoredVerifierAttemptIntent {
        schema: 2,
        operation_id: String::new(),
        qualification_operation_id: qualification_operation_id.into(),
        restored_occurrence_id: occurrence.occurrence_id.clone(),
        verifier_artifact_hash: admitted.selection().verifier_artifact_hash.clone(),
        // Private draft only: exact upload is supplied by bind_archive.
        upload_sha256: String::new(),
        upload_bytes: 0,
        purpose: RemoteVerificationPurpose::ConsumerRuntime {
            coordinate: admitted.coordinate().clone(),
            nonce_hex: hex::encode(lillux::crypto::generate_random_bytes::<32>()),
            guest_runtime_manifest_hash: source.intent.guest_runtime_manifest_hash.clone(),
        },
        attempt_deadline_ms: started_at_ms
            .checked_add(i64::from(profile.contact_timeout_seconds()) * 1000)
            .context("consumer challenge deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    let retained = state
        .state_store
        .restored_verifier_attempt(&intent.operation_id)?;
    if let Some(existing) = retained.as_ref() {
        match (&mut intent.purpose, &existing.intent.purpose) {
            (
                RemoteVerificationPurpose::ConsumerRuntime {
                    coordinate,
                    nonce_hex,
                    guest_runtime_manifest_hash,
                },
                RemoteVerificationPurpose::ConsumerRuntime {
                    coordinate: retained_coordinate,
                    nonce_hex: retained_nonce,
                    guest_runtime_manifest_hash: retained_manifest,
                },
            ) => {
                ensure!(
                    coordinate == retained_coordinate
                        && guest_runtime_manifest_hash == retained_manifest,
                    "consumer challenge replay changed accepted coordinate or owner runtime"
                );
                *nonce_hex = retained_nonce.clone();
            }
            _ => anyhow::bail!("consumer challenge replay changed verification lane"),
        }
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent.qualification_operation_id == existing.intent.qualification_operation_id
                && intent.restored_occurrence_id == existing.intent.restored_occurrence_id
                && intent.verifier_artifact_hash == existing.intent.verifier_artifact_hash
                && existing.consumer_selection.as_ref() == Some(admitted.selection()),
            "consumer challenge replay changed occurrence or selection"
        );
    }
    intent.consumer_challenge_digest()?;
    let remaining_ms = intent
        .attempt_deadline_ms
        .checked_sub(lillux::time::timestamp_millis())
        .filter(|remaining| *remaining > 0)
        .context("consumer original preparation deadline expired")?;
    let remaining = lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?).min(
        lillux::time::Duration::from_secs(u64::from(profile.contact_timeout_seconds()))
            .saturating_sub(preparation_timer.elapsed()),
    );
    ensure!(
        !remaining.is_zero(),
        "consumer preparation exhausted signed timeout"
    );
    let deadline = lillux::time::MonotonicDeadline::after(remaining);
    Ok(PreparedConsumerAttempt {
        intent,
        retained,
        selection: admitted.selection().clone(),
        deadline,
    })
}

/// Actual invoking-verifier lane. Contact remains owned by the same retained
/// attempt and occurrence gate; no operator principal is synthesized.
pub fn verify_consumer_qualification_occurrence_for_callback(
    state: &AppState,
    token: &str,
    intent: &RestoredVerifierAttemptIntent,
    archive: &ryeos_external_execution::restoration_verifier_delivery::PreparedConsumerArchive,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<RestoredVerifierAttemptRecord> {
    verify_consumer_occurrence_with_callback(state, token, intent, archive, limits, deadline)
}

/// Read one exact retained consumer attempt. Observation never starts or
/// retries provider work and does not turn a retained record into qualification.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConsumerVerificationObservation {
    /// Absence at this read is not permission to retry a pending start.
    NotReserved { operation_id: String },
    Retained {
        attempt: RestoredVerifierAttemptRecord,
        /// Exact bounded canonical CAS bytes decoded as opaque JSON. Only the
        /// independently admitted product verifier may interpret their semantics.
        evidence: Option<serde_json::Value>,
    },
}

pub fn observe_consumer_verifier_attempt_for_callback(
    state: &AppState,
    token: &str,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> Result<ConsumerVerificationObservation> {
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let admission = crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
        state, &authority, &guard, limits, token,
        "runtime.consumer_verification_observe", coordinate, qualification_operation_id,
    )?;
    let qualified = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("consumer qualification disappeared")?;
    let occurrence = qualified
        .occurrence
        .as_ref()
        .context("consumer observation has no retained occurrence")?;
    let source = state
        .state_store
        .runtime_snapshot_operation(&qualified.intent.snapshot_operation_id)?
        .context("consumer observation snapshot disappeared")?;
    ensure!(
        source.intent.owner_principal == admission.purpose().owner_fingerprint,
        "consumer observation snapshot belongs to another owner"
    );
    let operation_id = ryeos_external_execution_contract::restored_runtime_measurement::consumer_verifier_operation_id(
        qualification_operation_id, &occurrence.occurrence_id,
        &admission.selection().verifier_artifact_hash, coordinate,
        &source.intent.guest_runtime_manifest_hash,
    )?;
    // Serialize the short retained-record read with bearer revocation. No
    // provider operation or CAS traversal runs under this gate.
    let attempt = admission.with_live_callback(|| {
        let Some(record) = state.state_store.restored_verifier_attempt(&operation_id)? else {
            return Ok(None);
        };
        admission.require_attempt(&record.intent, &qualified.intent)?;
        ensure!(
            record.intent.operation_id == operation_id
                && record.intent.derived_operation_id()? == operation_id
                && record.consumer_selection.as_ref() == Some(admission.selection()),
            "retained consumer observation changed attempt or executable selection"
        );
        Ok(Some(record))
    })?;
    let Some(attempt) = attempt else {
        let final_admission = crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
            state, &authority, &guard, limits, token,
            "runtime.consumer_verification_observe", coordinate, qualification_operation_id,
        )?;
        return final_admission.with_live_callback(|| {
            Ok(ConsumerVerificationObservation::NotReserved { operation_id })
        });
    };
    let evidence = match attempt.observation.as_ref() {
        None => None,
        Some(ryeos_external_execution_contract::restored_runtime_measurement::RestoredVerifierObservation::ConsumerRuntime { observation }) => {
            observation.validate_for_intent(&attempt.intent)?;
            let bytes = authority.cas_store()?.get_blob_bounded(
                &observation.evidence_sha256,
                ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES,
            )?.context("retained consumer evidence blob is absent")?;
            Some(observation.verify_evidence_bytes(&bytes)?)
        }
        Some(_) => anyhow::bail!("consumer observation cannot expose owner measurement evidence"),
    };
    // Keep potentially large CAS reads outside the bearer-store gate, then
    // reauthenticate current root/profile/owner before exposing their bytes.
    let final_admission = crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
        state, &authority, &guard, limits, token,
        "runtime.consumer_verification_observe", coordinate, qualification_operation_id,
    )?;
    final_admission.require_attempt(&attempt.intent, &qualified.intent)?;
    final_admission
        .with_live_callback(|| Ok(ConsumerVerificationObservation::Retained { attempt, evidence }))
}

fn verify_consumer_occurrence_with_callback(
    state: &AppState,
    token: &str,
    intent: &RestoredVerifierAttemptIntent,
    archive: &ryeos_external_execution::restoration_verifier_delivery::PreparedConsumerArchive,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<RestoredVerifierAttemptRecord> {
    let remaining_ms = intent
        .attempt_deadline_ms
        .checked_sub(lillux::time::timestamp_millis())
        .filter(|remaining| *remaining > 0)
        .context("consumer original contact deadline expired")?;
    let deadline = deadline.min(lillux::time::MonotonicDeadline::after(
        lillux::time::Duration::from_millis(u64::try_from(remaining_ms)?),
    ));
    crate::hosted_operation::with_qualification_occurrence_contact(
        &intent.qualification_operation_id,
        || {
            use ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose;
            let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &intent.purpose
            else {
                anyhow::bail!("consumer contact cannot run owner measurement");
            };
            let authority = state.state_store.pinned_state_authority()?;
            let guard = authority.acquire_shared_guard()?;
            let admission = crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
                state,
                &authority,
                &guard,
                limits,
                token,
                "runtime.consumer_verification_start",
                coordinate,
                &intent.qualification_operation_id,
            )?;
            let selection = admission.selection().clone();
            archive.require_verifier_selection(&selection)?;
            let qualified = state
                .state_store
                .snapshot_qualification_operation(&intent.qualification_operation_id)?
                .context("consumer qualification disappeared")?;
            let occurrence = qualified
                .occurrence
                .clone()
                .context("consumer occurrence absent")?;
            let source = state
                .state_store
                .runtime_snapshot_operation(&qualified.intent.snapshot_operation_id)?
                .context("consumer snapshot disappeared")?;
            ensure!(
                source.intent.owner_principal == admission.purpose().owner_fingerprint,
                "consumer snapshot belongs to another qualification owner"
            );
            let locator = source
                .locator
                .clone()
                .context("consumer snapshot locator absent")?;
            let readiness = source
                .readiness
                .clone()
                .context("consumer snapshot readiness absent")?;
            let qualification = state
                .node_config
                .runtime_snapshot_qualification
                .iter()
                .find(|binding| binding.digest() == qualified.intent.qualification_profile_digest)
                .context("consumer signed qualification absent")?;
            let producer = state
                .node_config
                .runtime_snapshot_production
                .iter()
                .find(|binding| {
                    binding.id() == qualification.production_binding_id()
                        && binding.digest() == qualification.production_binding_digest()
                })
                .context("consumer signed producer absent")?;
            let challenge = ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge {
                schema: 1, intent: intent.clone(), selection: selection.clone(),
            };
            let upload = archive.delivery_descriptor_for_challenge(&challenge)?;
            let request = RestoredVerifierAdapterRequest {
                protocol: RESTORED_VERIFIER_ADAPTER_PROTOCOL.into(),
                provider_spec_digest: qualification.provider_spec_digest().into(),
                intent: intent.clone(),
                consumer_selection: Some(selection),
                source_intent: source.intent,
                locator,
                readiness,
                qualification_intent: qualified.intent,
                occurrence,
                upload_descriptor: upload.inherited_descriptor().map_err(anyhow::Error::msg)?,
                upload_bytes: archive.bytes(),
                upload_sha256: archive.sha256().into(),
            };
            request.validate()?;
            let access = producer.credential_access()?;
            let credential = access.decode(state.vault.placement_credential(&access)?)?;
            state
                .external_placement_backends
                .preflight_snapshot_qualification_create(producer, qualification, &credential)?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer preparation deadline expired before reservation"
            );
            state
                .state_store
                .reserve_consumer_verifier_attempt(intent, admission)?;
            // Root authentication is intentionally repeated and consumed, not
            // cloned from reservation. The transaction checks live occurrence
            // and same-occurrence prerequisite immediately before first contact.
            let claim_admission = crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
                state,
                &authority,
                &guard,
                limits,
                token,
                "runtime.consumer_verification_start",
                coordinate,
                &intent.qualification_operation_id,
            )?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer original deadline expired before contact claim"
            );
            let claim = state
                .state_store
                .claim_consumer_verifier_attempt(&intent.operation_id, claim_admission)?;
            let RestoredVerifierAttemptClaim::StartAttempt(_) = claim else {
                return Ok(match claim {
                    RestoredVerifierAttemptClaim::Reconcile(record)
                    | RestoredVerifierAttemptClaim::Observed(record) => record,
                    RestoredVerifierAttemptClaim::StartAttempt(_) => unreachable!(),
                });
            };
            if deadline.has_elapsed() {
                state
                    .state_store
                    .quarantine_restored_verifier_attempt(&intent.operation_id)?;
                anyhow::bail!("consumer original deadline expired after contact claim");
            }
            let attempted = state
                .external_placement_backends
                .verify_restored_snapshot_once(
                    producer,
                    qualification,
                    &credential,
                    &request,
                    &upload,
                    deadline,
                );
            match attempted {
                Ok(output) => match output.value {
                    RestoredVerifierAdapterResponse::ConsumerObserved {
                        mut observation,
                        evidence,
                    } => {
                        observation.contact_deadline_exceeded = output.deadline_exceeded;
                        let retained = ryeos_external_execution_contract::canonical_json(&evidence)
                            .and_then(|bytes| {
                                state
                                    .state_store
                                    .bind_consumer_verifier_observation(&observation, &bytes)
                            });
                        match retained {
                            Ok(record) => Ok(record),
                            Err(error) => {
                                state
                                    .state_store
                                    .quarantine_restored_verifier_attempt(&intent.operation_id)?;
                                Err(error)
                            }
                        }
                    }
                    RestoredVerifierAdapterResponse::Uncertain { .. } => state
                        .state_store
                        .quarantine_restored_verifier_attempt(&intent.operation_id),
                    RestoredVerifierAdapterResponse::Observed { .. } => {
                        state
                            .state_store
                            .quarantine_restored_verifier_attempt(&intent.operation_id)?;
                        anyhow::bail!("consumer contact returned owner measurement")
                    }
                },
                Err(error) => {
                    state
                        .state_store
                        .quarantine_restored_verifier_attempt(&intent.operation_id)?;
                    Err(error)
                }
            }
        },
    )
}

/// Terminate one exact restored qualification Sandbox. A replayed or
/// uncertain first contact can only observe the retained occurrence; it
/// cannot repeat the POST. Provider terminal status is not writer exclusion.
pub fn terminate_qualification_occurrence(
    state: &AppState,
    context: &HandlerContext,
    qualification_operation_id: &str,
) -> Result<QualificationTerminationRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    crate::hosted_operation::with_qualification_occurrence_contact(
        qualification_operation_id,
        || {
            terminate_qualification_occurrence_under_contact_gate(
                state,
                &QualificationTerminationInvocation::Operator(context),
                qualification_operation_id,
            )
        },
    )
}

pub fn terminate_consumer_qualification_occurrence_for_callback(
    state: &AppState,
    token: &str,
    coordinate: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
    qualification_operation_id: &str,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
) -> Result<QualificationTerminationRecord> {
    crate::hosted_operation::with_qualification_occurrence_contact(
        qualification_operation_id,
        || {
            terminate_qualification_occurrence_under_contact_gate(
                state,
                &QualificationTerminationInvocation::Consumer {
                    token,
                    coordinate,
                    limits,
                },
                qualification_operation_id,
            )
        },
    )
}

enum QualificationTerminationInvocation<'a> {
    Operator(&'a HandlerContext),
    Consumer {
        token: &'a str,
        coordinate: &'a ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerificationCoordinate,
        limits: ryeos_state::object_closure::ObjectClosureLimits,
    },
}

impl QualificationTerminationInvocation<'_> {
    fn authenticate_consumer(
        &self,
        state: &AppState,
        qualification_operation_id: &str,
    ) -> Result<
        Option<crate::operator_external_content::product_qualification::AuthenticatedConsumerRoot>,
    > {
        match self {
            Self::Operator(context) => {
                crate::operator_authority::require_admitted_operator(state, context)?;
                Ok(None)
            }
            Self::Consumer {
                token,
                coordinate,
                limits,
            } => {
                let authority = state.state_store.pinned_state_authority()?;
                let guard = authority.acquire_shared_guard()?;
                Ok(Some(crate::operator_external_content::product_qualification::authenticate_consumer_callback_root(
                    state, &authority, &guard, *limits, token,
                    "runtime.consumer_verification_settle", coordinate, qualification_operation_id,
                )?))
            }
        }
    }
}

fn terminate_qualification_occurrence_under_contact_gate(
    state: &AppState,
    invocation: &QualificationTerminationInvocation<'_>,
    qualification_operation_id: &str,
) -> Result<QualificationTerminationRecord> {
    let admission = invocation.authenticate_consumer(state, qualification_operation_id)?;
    let owner = match (invocation, admission.as_ref()) {
        (QualificationTerminationInvocation::Operator(context), None) => {
            context.fingerprint.clone()
        }
        (QualificationTerminationInvocation::Consumer { .. }, Some(admission)) => {
            admission.purpose().owner_fingerprint.clone()
        }
        _ => anyhow::bail!("qualification termination invocation lost authority"),
    };
    let qualified = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("snapshot qualification operation is absent")?;
    ensure!(
        qualified.intent.owner_principal == owner
            && qualified.phase == SnapshotQualificationPhase::OccurrenceBound,
        "qualification termination has no authenticated owned restored occurrence"
    );
    let occurrence = qualified
        .occurrence
        .clone()
        .context("qualification termination occurrence is absent")?;
    let qualification = state
        .node_config
        .runtime_snapshot_qualification
        .iter()
        .find(|binding| binding.digest() == qualified.intent.qualification_profile_digest)
        .context("current signed snapshot qualification binding is absent")?;
    let producer = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| {
            binding.id() == qualification.production_binding_id()
                && binding.digest() == qualification.production_binding_digest()
        })
        .context("qualification termination lost exact producer binding")?;
    ensure!(
        qualified.intent.adapter_artifact_hash == producer.adapter_artifact_hash()
            && qualified.intent.provider_spec_digest == qualification.provider_spec_digest()
            && qualified.intent.settings_digest == qualification.settings_digest()
            && qualified.intent.provider_id == producer.backend(),
        "qualification termination differs from signed provider authority"
    );
    let access = producer.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    state
        .external_placement_backends
        .preflight_snapshot_qualification_create(producer, qualification, &credential)?;
    let now = lillux::time::timestamp_millis();
    let mut intent = RuntimeSnapshotQualificationTerminationIntent {
        schema: 1,
        operation_id: String::new(),
        qualification_operation_id: qualified.intent.operation_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        owner_principal: owner,
        provider_id: producer.backend().to_owned(),
        provider_spec_digest: qualification.provider_spec_digest().to_owned(),
        attempt_deadline_ms: now
            .checked_add(i64::from(qualification.contact_timeout_seconds()) * 1_000)
            .context("qualification termination deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .qualification_termination_operation(&intent.operation_id)?
    {
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent == existing.intent,
            "retained qualification termination changed signed coordinates"
        );
    }
    intent.validate_for(&qualified.intent, &occurrence)?;
    match invocation.authenticate_consumer(state, qualification_operation_id)? {
        Some(admission) => {
            state
                .state_store
                .reserve_consumer_qualification_termination(&intent, admission)?;
        }
        None => {
            state
                .state_store
                .reserve_qualification_termination(&intent)?;
        }
    }
    let claim = match invocation.authenticate_consumer(state, qualification_operation_id)? {
        Some(admission) => state
            .state_store
            .claim_consumer_qualification_termination(&intent.operation_id, admission)?,
        None => state
            .state_store
            .claim_qualification_termination_attempt(&intent.operation_id)?,
    };
    let first_contact = match claim {
        QualificationTerminationClaim::StartAttempt(_) => true,
        QualificationTerminationClaim::Reconcile(_) => false,
        QualificationTerminationClaim::Terminal(record) => return Ok(record),
    };
    let request = RuntimeSnapshotQualificationTerminationAdapterRequest {
        protocol: RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL.into(),
        provider_spec_digest: qualification.provider_spec_digest().to_owned(),
        intent,
        qualification_intent: qualified.intent,
        occurrence,
    };
    request.validate()?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(qualification.contact_timeout_seconds()),
    ));
    let attempted = state
        .external_placement_backends
        .terminate_snapshot_qualification_occurrence(
            producer,
            qualification,
            &credential,
            &request,
            first_contact,
            deadline,
        );
    match attempted {
        Ok(output) => match output.value {
            RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal {
                mut observation,
            } => {
                observation.contact_deadline_exceeded = output.deadline_exceeded;
                state
                    .state_store
                    .bind_qualification_terminal_observation(&observation)
            }
            RuntimeSnapshotQualificationTerminationAdapterResponse::Pending { .. } => {
                if first_contact {
                    state
                        .state_store
                        .quarantine_qualification_termination_attempt(&request.intent.operation_id)
                } else {
                    state
                        .state_store
                        .qualification_termination_operation(&request.intent.operation_id)?
                        .context("qualification termination disappeared during reconciliation")
                }
            }
        },
        Err(error) => {
            if first_contact {
                state
                    .state_store
                    .quarantine_qualification_termination_attempt(&request.intent.operation_id)?;
            }
            Err(error)
        }
    }
}

/// Observe availability of the exact already-bound provider locator. This is
/// a read-only provider request and retains only provider readiness; restored
/// bytes still require independent qualification before activation.
pub fn observe_readiness(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<RuntimeSnapshotRecord> {
    let record = get_operation(state, context, operation_id)?;
    ensure!(
        record.phase == RuntimeSnapshotPhase::Bound,
        "snapshot has no bound locator"
    );
    if record.readiness.is_some() {
        return Ok(record);
    }
    let bootstrap = record
        .intent
        .source_bootstrap_operation_id
        .as_deref()
        .map(|id| get_bootstrap_source(state, context, id))
        .transpose()?;
    let binding = if let Some(bootstrap) = &bootstrap {
        state
            .state_store
            .retained_snapshot_bootstrap_binding(&bootstrap.intent.operation_id)?
            .context("snapshot readiness lost retained producer binding")?
            .recovered_binding()?
    } else {
        state
            .node_config
            .runtime_snapshot_production
            .iter()
            .find(|binding| binding.digest() == record.intent.production_profile_digest)
            .cloned()
            .context("current signed snapshot producer binding is absent")?
    };
    ensure!(
        binding.backend() == record.intent.provider_id
            && binding.adapter_artifact_hash() == record.intent.adapter_artifact_hash
            && binding.snapshot_spec_sha256() == record.intent.provider_spec_digest
            && binding.settings_digest() == record.intent.settings_digest
            && binding.provider_group_id() == record.intent.provider_group_id,
        "snapshot readiness differs from its exact signed producer"
    );
    let locator = record
        .locator
        .clone()
        .context("bound snapshot has no locator")?;
    let request = RuntimeSnapshotReadinessRequest {
        protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
        intent: record.intent.clone(),
        locator,
        provider_spec_digest: record.intent.provider_spec_digest.clone(),
    };
    request.validate()?;
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let observed = if let Some(bootstrap) = &bootstrap {
        let recovered = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
            state,
            bootstrap,
            &binding,
            &credential,
        )?;
        recovered.observe_snapshot_readiness(&binding, &credential, &request, deadline)?
    } else {
        state
            .external_placement_backends
            .preflight_runtime_snapshot(&binding, &credential)?;
        state
            .external_placement_backends
            .observe_runtime_snapshot_readiness(&binding, &credential, &request, deadline)?
    };
    ensure!(
        !observed.deadline_exceeded,
        "snapshot readiness exceeded its contact deadline"
    );
    state
        .state_store
        .bind_runtime_snapshot_readiness(&observed.value)
}

/// Rejoin an independently qualified runtime probe to the daemon's one-shot
/// provider journal. The probe can name a locator, but cannot manufacture a
/// bound attempt or replace the source product, controller, or operator.
pub(crate) fn verify_probe_snapshot_locator(
    state: &AppState,
    proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    source: &GuestOwnerRuntimeManifestIdentity,
    provider_id: &str,
    owner_principal: &str,
) -> Result<Option<ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator>> {
    let Some(value) = proof
        .evidence
        .result
        .probe_evidence
        .get("runtime_snapshot_locator")
    else {
        return Ok(None);
    };
    let named: ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator =
        serde_json::from_value(value.clone())
            .context("runtime probe has an invalid snapshot locator")?;
    let record = state
        .state_store
        .runtime_snapshot_operation(&named.operation_id)?
        .context("runtime probe names no retained snapshot operation")?;
    validate_probe_snapshot_record(&record, &named, proof, source, provider_id, owner_principal)?;
    let verifier = verify_probe_restored_verifier_record(
        state,
        &record,
        proof,
        source,
        provider_id,
        owner_principal,
    )?;
    verify_probe_provider_terminal_record(state, proof, &verifier, provider_id, owner_principal)?;
    Ok(Some(named))
}

fn verify_probe_restored_verifier_record(
    state: &AppState,
    snapshot: &RuntimeSnapshotRecord,
    proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    source: &GuestOwnerRuntimeManifestIdentity,
    provider_id: &str,
    owner_principal: &str,
) -> Result<RestoredVerifierAttemptRecord> {
    let evidence = &proof.evidence.result.probe_evidence;
    let operation_id = evidence
        .get("restored_verifier_operation_id")
        .and_then(serde_json::Value::as_str)
        .context("runtime probe lacks exact restored verifier operation")?;
    let observation_hash = evidence
        .get("restored_verifier_observation_hash")
        .and_then(serde_json::Value::as_str)
        .context("runtime probe lacks exact restored verifier observation")?;
    ensure!(
        lillux::valid_hash(operation_id) && lillux::valid_hash(observation_hash),
        "runtime probe has invalid verifier evidence identity"
    );
    let retained = state
        .state_store
        .restored_verifier_attempt(operation_id)?
        .context("runtime probe names no retained verifier attempt")?;
    let observation = retained
        .observation
        .as_ref()
        .context("runtime probe verifier has no complete observation")?
        .owner_measurement()?;
    ensure!(
        retained.phase == RestoredVerifierAttemptPhase::Observed
            && !observation.contact_deadline_exceeded
            && retained.intent.owner_challenge()?.operation_id == snapshot.intent.operation_id
            && retained.intent.owner_challenge()?.snapshot_id
                == snapshot
                    .locator
                    .as_ref()
                    .context("probe snapshot lost locator")?
                    .snapshot_id
            && retained.intent.restored_occurrence_id == observation.occurrence_id
            && snapshot.intent.provider_id == provider_id
            && snapshot.intent.owner_principal == owner_principal
            && observation.measurement.manifest_hash == source.manifest_hash
            && observation.measurement.owner_executable_sha256 == source.owner_executable_sha256
            && observation.measurement.controller_public_root == source.controller_public_root
            && lillux::sha256_hex(&ryeos_external_execution_contract::canonical_json(
                observation
            )?) == observation_hash,
        "runtime probe verifier differs from exact timely retained observation"
    );
    Ok(retained)
}

fn verify_probe_provider_terminal_record(
    state: &AppState,
    proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    verifier: &RestoredVerifierAttemptRecord,
    provider_id: &str,
    owner_principal: &str,
) -> Result<()> {
    let evidence = &proof.evidence.result.probe_evidence;
    let operation_id = evidence
        .get("qualification_termination_operation_id")
        .and_then(serde_json::Value::as_str)
        .context("runtime probe lacks exact qualification termination operation")?;
    let observation_hash = evidence
        .get("provider_terminal_observation_hash")
        .and_then(serde_json::Value::as_str)
        .context("runtime probe lacks exact provider terminal observation")?;
    ensure!(
        lillux::valid_hash(operation_id) && lillux::valid_hash(observation_hash),
        "runtime probe has invalid provider terminal evidence identity"
    );
    let retained = state
        .state_store
        .qualification_termination_operation(operation_id)?
        .context("runtime probe names no retained qualification termination")?;
    validate_provider_terminal_join(
        &retained,
        &verifier.intent.qualification_operation_id,
        &verifier.intent.restored_occurrence_id,
        provider_id,
        owner_principal,
        observation_hash,
    )
}

fn validate_provider_terminal_join(
    retained: &QualificationTerminationRecord,
    qualification_operation_id: &str,
    restored_occurrence_id: &str,
    provider_id: &str,
    owner_principal: &str,
    observation_hash: &str,
) -> Result<()> {
    let observation = retained
        .observation
        .as_ref()
        .context("runtime probe termination has no provider terminal observation")?;
    ensure!(
        retained.phase == QualificationTerminationPhase::Terminal
            && !observation.contact_deadline_exceeded
            && retained.intent.qualification_operation_id == qualification_operation_id
            && retained.intent.occurrence_id == restored_occurrence_id
            && retained.intent.provider_id == provider_id
            && retained.intent.owner_principal == owner_principal
            && lillux::sha256_hex(&ryeos_external_execution_contract::canonical_json(
                observation
            )?) == observation_hash,
        "runtime probe provider terminal status differs from exact retained occurrence"
    );
    Ok(())
}

fn validate_probe_snapshot_record(
    record: &RuntimeSnapshotRecord,
    named: &ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator,
    proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    source: &GuestOwnerRuntimeManifestIdentity,
    provider_id: &str,
    owner_principal: &str,
) -> Result<()> {
    ensure!(
        record.phase == RuntimeSnapshotPhase::Bound
            && record.locator.as_ref() == Some(&named)
            && record.intent.owner_principal == owner_principal
            && record.intent.owner_principal == proof.evidence.product_coordinate.owner_principal
            && record.intent.provider_id == provider_id
            && record.intent.source
                == RuntimeSnapshotSource::CapturedProduct {
                    product_witness_hash: proof.evidence.product_witness_hash.clone(),
                }
            && record.intent.guest_runtime_manifest_hash == source.manifest_hash
            && record.intent.owner_executable_sha256 == source.owner_executable_sha256
            && record.intent.controller_public_root == source.controller_public_root,
        "runtime probe locator differs from its exact bound product and provider attempt"
    );
    named.validate_for(&record.intent)?;
    Ok(())
}

/// Rejoin a materialized source to the complete retained restoration journal.
/// This is deliberately distinct from product qualification: it proves exact
/// provenance and observations, but does not by itself assert that the runtime
/// profile's semantic claims or guest-writer exclusion have been qualified.
fn verify_materialized_restoration_journal(
    snapshot: &RuntimeSnapshotRecord,
    qualification: &SnapshotQualificationRecord,
    verifier: &RestoredVerifierAttemptRecord,
    termination: &QualificationTerminationRecord,
    expected_source: &RuntimeSnapshotSource,
    identity: &GuestOwnerRuntimeManifestIdentity,
    provider_id: &str,
    owner_principal: &str,
) -> Result<()> {
    ensure!(
        matches!(
            expected_source,
            RuntimeSnapshotSource::BundleMaterialization { .. }
        ) && snapshot.intent.source == *expected_source
            && snapshot.intent.owner_principal == owner_principal
            && snapshot.intent.provider_id == provider_id
            && snapshot.intent.guest_runtime_manifest_hash == identity.manifest_hash
            && snapshot.intent.owner_executable_sha256 == identity.owner_executable_sha256
            && snapshot.intent.controller_public_root == identity.controller_public_root
            && snapshot.phase == RuntimeSnapshotPhase::Bound
            && snapshot.runner_deadline_exceeded == Some(false)
            && snapshot
                .completion_at_ms
                .is_some_and(|completed| completed <= snapshot.intent.attempt_deadline_ms),
        "materialized restoration differs from its exact timely source"
    );
    let locator = snapshot
        .locator
        .as_ref()
        .context("materialized restoration has no bound snapshot locator")?;
    let readiness = snapshot
        .readiness
        .as_ref()
        .context("materialized restoration has no snapshot readiness")?;
    locator.validate_for(&snapshot.intent)?;
    readiness.validate_for(&RuntimeSnapshotReadinessRequest {
        protocol: RUNTIME_SNAPSHOT_READINESS_PROTOCOL.into(),
        intent: snapshot.intent.clone(),
        locator: locator.clone(),
        provider_spec_digest: snapshot.intent.provider_spec_digest.clone(),
    })?;
    ensure!(
        qualification.phase == SnapshotQualificationPhase::OccurrenceBound,
        "materialized restoration has no bound qualification occurrence"
    );
    qualification
        .intent
        .validate_for(&snapshot.intent, locator)?;
    let occurrence = qualification
        .occurrence
        .as_ref()
        .context("materialized restoration has no qualification occurrence")?;
    occurrence.validate_for(&qualification.intent)?;
    ensure!(
        !occurrence.contact_deadline_exceeded
            && verifier.phase == RestoredVerifierAttemptPhase::Observed,
        "materialized restoration has no timely verifier attempt"
    );
    let observation = verifier
        .observation
        .as_ref()
        .context("materialized restoration has no verifier observation")?
        .owner_measurement()?;
    ensure!(
        !observation.contact_deadline_exceeded,
        "materialized restoration verifier exceeded its deadline"
    );
    observation.validate_for_retained(
        &verifier.intent,
        &snapshot.intent,
        locator,
        readiness,
        &qualification.intent,
        occurrence,
    )?;
    ensure!(
        termination.phase == QualificationTerminationPhase::Terminal,
        "materialized restoration has no terminal qualification occurrence"
    );
    let terminal = termination
        .observation
        .as_ref()
        .context("materialized restoration has no terminal observation")?;
    termination
        .intent
        .validate_for(&qualification.intent, occurrence)?;
    terminal.validate_for(&termination.intent)?;
    let terminal_hash = lillux::sha256_hex(&ryeos_external_execution_contract::canonical_json(
        terminal,
    )?);
    validate_provider_terminal_join(
        termination,
        &qualification.intent.operation_id,
        &occurrence.occurrence_id,
        provider_id,
        owner_principal,
        &terminal_hash,
    )?;
    Ok(())
}

/// Authenticate the complete historical materialization-to-restoration chain
/// from retained state. This is a read-only journal proof, not fresh provider
/// authority or a published runtime qualification claim.
pub fn verify_retained_materialized_restoration(
    state: &AppState,
    owner_principal: &str,
    snapshot_operation_id: &str,
    qualification_operation_id: &str,
    verifier_operation_id: &str,
    termination_operation_id: &str,
) -> Result<()> {
    for operation_id in [
        snapshot_operation_id,
        qualification_operation_id,
        verifier_operation_id,
        termination_operation_id,
    ] {
        ensure!(
            lillux::valid_hash(operation_id),
            "materialized restoration operation ID is invalid"
        );
    }
    let snapshot = state
        .state_store
        .runtime_snapshot_operation(snapshot_operation_id)?
        .context("materialized restoration snapshot operation is absent")?;
    ensure!(
        snapshot.intent.owner_principal == owner_principal,
        "materialized restoration snapshot belongs to another owner"
    );
    let RuntimeSnapshotSource::BundleMaterialization {
        materialization_attestation_hash,
        source_coordinate_digest,
        materialization_binding_digest,
    } = &snapshot.intent.source
    else {
        anyhow::bail!("restoration source is not a materialization");
    };
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let source =
        crate::operator_guest_runtime_materialization::load_retained_guest_owner_materialization(
            state,
            owner_principal,
            source_coordinate_digest,
            materialization_attestation_hash,
            &authority,
            &guard,
        )?;
    ensure!(
        source.source.materialization_binding_digest == *materialization_binding_digest,
        "materialized restoration source binding changed"
    );
    let bootstrap_id = snapshot
        .intent
        .source_bootstrap_operation_id
        .as_deref()
        .context("materialized restoration lost source bootstrap")?;
    let bootstrap = state
        .state_store
        .snapshot_bootstrap_operation(bootstrap_id)?
        .context("materialized restoration bootstrap is absent")?;
    ensure!(
        bootstrap.phase == SnapshotBootstrapPhase::OccurrenceBound
            && bootstrap.readiness.is_some()
            && bootstrap.intent.source == snapshot.intent.source
            && bootstrap.intent.owner_principal == owner_principal
            && bootstrap.intent.provider_id == snapshot.intent.provider_id
            && bootstrap.intent.provider_group_id == snapshot.intent.provider_group_id
            && bootstrap.intent.production_binding_digest
                == snapshot.intent.production_profile_digest
            && bootstrap.intent.guest_runtime_manifest_hash
                == snapshot.intent.guest_runtime_manifest_hash,
        "materialized restoration differs from its ready bootstrap source"
    );
    let mut upload_intent = RuntimeSnapshotStageIntent {
        schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        operation_id: String::new(),
        parent_operation_id: snapshot_operation_id.to_owned(),
        parent_intent_digest: snapshot.intent.digest()?,
        stage: RuntimeSnapshotStage::Upload,
        accepted_upload_receipt_digest: None,
        attempt_deadline_ms: snapshot.intent.attempt_deadline_ms,
    };
    upload_intent.operation_id = upload_intent.derived_operation_id()?;
    let upload = state
        .state_store
        .runtime_snapshot_stage(&upload_intent.operation_id)?
        .context("materialized restoration has no upload stage")?;
    ensure!(
        upload.phase == RuntimeSnapshotStagePhase::Accepted
            && upload.runner_deadline_exceeded == Some(false),
        "materialized restoration upload was not timely accepted"
    );
    upload.intent.validate_for(&snapshot.intent, None)?;
    let receipt = upload
        .receipt
        .as_ref()
        .context("materialized restoration accepted upload lost receipt")?;
    receipt.validate_for_stage(&snapshot.intent, &upload.intent)?;
    let mut create_intent = RuntimeSnapshotStageIntent {
        schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        operation_id: String::new(),
        parent_operation_id: snapshot_operation_id.to_owned(),
        parent_intent_digest: snapshot.intent.digest()?,
        stage: RuntimeSnapshotStage::Create,
        accepted_upload_receipt_digest: Some(receipt.digest()?),
        attempt_deadline_ms: snapshot.intent.attempt_deadline_ms,
    };
    create_intent.operation_id = create_intent.derived_operation_id()?;
    let create = state
        .state_store
        .runtime_snapshot_stage(&create_intent.operation_id)?
        .context("materialized restoration has no create stage")?;
    ensure!(
        create.phase == RuntimeSnapshotStagePhase::Bound
            && create.runner_deadline_exceeded == Some(false)
            && create.locator == snapshot.locator,
        "materialized restoration create did not bind the exact parent locator"
    );
    create
        .intent
        .validate_for(&snapshot.intent, Some((&upload.intent, receipt)))?;
    let qualification = state
        .state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("materialized restoration qualification is absent")?;
    let verifier = state
        .state_store
        .restored_verifier_attempt(verifier_operation_id)?
        .context("materialized restoration verifier is absent")?;
    let termination = state
        .state_store
        .qualification_termination_operation(termination_operation_id)?
        .context("materialized restoration termination is absent")?;
    verify_materialized_restoration_journal(
        &snapshot,
        &qualification,
        &verifier,
        &termination,
        &bootstrap.intent.source,
        &source.identity,
        &snapshot.intent.provider_id,
        owner_principal,
    )
}

pub fn produce(
    state: &AppState,
    context: &HandlerContext,
    request: SnapshotProductionRequest,
) -> Result<RuntimeSnapshotRecord> {
    state
        .engine
        .with_checked_bundle_generation(|_| produce_checked(state, context, request))
}

fn produce_checked(
    state: &AppState,
    context: &HandlerContext,
    request: SnapshotProductionRequest,
) -> Result<RuntimeSnapshotRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    // A timely proxy upload acknowledgment is not a remote writer fence.
    // The materialized path may therefore yield an unusable snapshot. Its
    // one-shot staged journal permits only an unqualified locator; captured-
    // product qualification and every placement/Worker consumer still reject
    // this source until a separate materialized qualification is published.
    let binding = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| binding.id() == request.binding_id)
        .context("current signed snapshot producer binding is absent")?;
    let source_ceiling = staging_source_ceiling(state, context, &request.binding_id)?;

    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let (source, exact_identity) = match &request.source {
        SnapshotProductionSource::CapturedProduct {
            witness_hash,
            source,
            source_occurrence_id: _,
        } => {
            let witness = crate::operator_external_content::product_receipt::load_bounded_current_product_source(
                state,
                &authority,
                &guard,
                limits,
                &context.fingerprint,
                witness_hash,
                source,
                source_ceiling,
            )?;
            ensure!(
                witness.evidence.declaration.shape == ProductShape::Tree
                    && witness.evidence.declaration.storage == ProductStorage::Content,
                "snapshot source is not an ordinary retained product tree"
            );
            let manifest = load_exact_cas_object_with_cas(
                &authority.cas_store()?,
                &witness.evidence.manifest_hash,
                limits.max_object_bytes,
            )?;
            let identity = derive_guest_owner_runtime_manifest_identity(
                &manifest,
                state.identity.verifying_key(),
            )?;
            ensure!(
                identity.manifest_hash == witness.evidence.manifest_hash
                    && witness.attestation_hash == *witness_hash,
                "snapshot product differs from its current witness"
            );
            (
                RuntimeSnapshotSource::CapturedProduct {
                    product_witness_hash: witness.attestation_hash,
                },
                identity,
            )
        }
        SnapshotProductionSource::BundleMaterialization {
            materialization_binding_id,
            coordinate_digest,
            attestation_hash,
            bootstrap_operation_id: _,
        } => {
            let current = crate::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
                state,
                context,
                materialization_binding_id,
                coordinate_digest,
                attestation_hash,
                &authority,
                &guard,
            )?;
            ensure!(
                current.source.maximum_owner_bytes <= source_ceiling,
                "materialization owner exceeds signed snapshot source ceiling"
            );
            (
                RuntimeSnapshotSource::BundleMaterialization {
                    materialization_attestation_hash: current.attestation_hash,
                    source_coordinate_digest: coordinate_digest.clone(),
                    materialization_binding_digest: current.source.materialization_binding_digest,
                },
                current.identity,
            )
        }
    };
    ensure!(
        exact_identity == request.staged_identity,
        "snapshot upload identity differs from the current retained source"
    );
    drop(guard);
    let upload = seal_guest_owner_snapshot_upload(
        &request.staged_root,
        &exact_identity.manifest_hash,
        source_ceiling,
    )?;
    ensure!(
        upload.bytes() <= binding.maximum_upload_bytes(),
        "snapshot upload exceeds its signed byte budget"
    );

    // Credential and exact installed adapter existence are checked before the
    // irreversible journal claim. The following attempt is never retried after
    // any uncertain provider sequence.
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    state
        .external_placement_backends
        .preflight_runtime_snapshot(binding, &credential)?;
    let _contact_guard = if let SnapshotProductionSource::BundleMaterialization {
        materialization_binding_id,
        coordinate_digest,
        attestation_hash,
        bootstrap_operation_id: _,
    } = &request.source
    {
        let contact_guard = authority.acquire_shared_guard()?;
        let current = crate::operator_guest_runtime_materialization::load_current_guest_owner_materialization(
            state,
            context,
            materialization_binding_id,
            coordinate_digest,
            attestation_hash,
            &authority,
            &contact_guard,
        )?;
        ensure!(
            current.identity == exact_identity
                && source
                    == RuntimeSnapshotSource::BundleMaterialization {
                        materialization_attestation_hash: current.attestation_hash,
                        source_coordinate_digest: coordinate_digest.clone(),
                        materialization_binding_digest: current
                            .source
                            .materialization_binding_digest,
                    },
            "materialization source changed before snapshot contact admission"
        );
        Some(contact_guard)
    } else {
        None
    };

    let (
        source_occurrence_id,
        source_bootstrap_operation_id,
        source_created_at,
        source_timeout_seconds,
        bootstrap,
    ) = match &request.source {
        SnapshotProductionSource::CapturedProduct {
            source_occurrence_id,
            ..
        } => (source_occurrence_id.clone(), None, None, None, None),
        SnapshotProductionSource::BundleMaterialization {
            bootstrap_operation_id,
            ..
        } => {
            let bootstrap = get_bootstrap_source(state, context, bootstrap_operation_id)?;
            ensure!(
                bootstrap.phase == SnapshotBootstrapPhase::OccurrenceBound
                    && bootstrap.readiness.is_some()
                    && bootstrap.intent.source == source
                    && bootstrap.intent.owner_principal == context.fingerprint
                    && bootstrap.intent.provider_id == binding.backend()
                    && bootstrap.intent.provider_group_id == binding.provider_group_id()
                    && bootstrap.intent.production_binding_digest == binding.digest()
                    && bootstrap.intent.adapter_artifact_hash == binding.adapter_artifact_hash()
                    && bootstrap.intent.settings_digest == binding.settings_digest()
                    && bootstrap.intent.guest_runtime_manifest_hash == exact_identity.manifest_hash,
                "materialized snapshot differs from exact ready bootstrap source"
            );
            let occurrence = bootstrap
                .occurrence
                .as_ref()
                .context("ready bootstrap source lost bound occurrence")?;
            let created_at = occurrence.provider_creation_observation["created_at"]
                .as_str()
                .context("bootstrap source has no exact creation timestamp")?
                .to_owned();
            let timeout_seconds = u32::try_from(
                occurrence.provider_creation_observation["timeout_seconds"]
                    .as_u64()
                    .context("bootstrap source has no exact timeout")?,
            )?;
            (
                occurrence.occurrence_id.clone(),
                Some(bootstrap_operation_id.clone()),
                Some(created_at),
                Some(timeout_seconds),
                Some(bootstrap),
            )
        }
    };
    let now = lillux::time::timestamp_millis();
    let attempt_deadline_ms = snapshot_attempt_deadline(
        now,
        binding.contact_timeout_seconds(),
        binding.maximum_bootstrap_lifetime_seconds(),
        source_created_at.as_deref(),
        source_timeout_seconds,
    )?;
    let mut intent = RuntimeSnapshotIntent {
        schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
        operation_id: String::new(),
        owner_principal: context.fingerprint.clone(),
        provider_id: binding.backend().to_owned(),
        source_occurrence_id,
        source_bootstrap_operation_id,
        source_created_at,
        source_timeout_seconds,
        provider_group_id: binding.provider_group_id().to_owned(),
        production_profile_digest: binding.digest().to_owned(),
        adapter_artifact_hash: binding.adapter_artifact_hash().to_owned(),
        provider_spec_digest: binding.snapshot_spec_sha256().to_owned(),
        settings_digest: binding.settings_digest().to_owned(),
        source,
        guest_runtime_manifest_hash: exact_identity.manifest_hash,
        owner_executable_sha256: exact_identity.owner_executable_sha256,
        controller_public_root: exact_identity.controller_public_root,
        upload_sha256: upload.sha256().to_owned(),
        upload_bytes: upload.bytes(),
        attempt_deadline_ms,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .runtime_snapshot_operation(&intent.operation_id)?
    {
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(
            intent == existing.intent,
            "retained snapshot attempt contradicts current producer coordinates"
        );
        if existing.phase != RuntimeSnapshotPhase::Reserved {
            return Ok(existing);
        }
    }
    intent.validate()?;
    if let Some(bootstrap) = &bootstrap {
        ensure!(
            fetch_bootstrap_source_readiness(state, bootstrap)?.is_some(),
            "bootstrap source is no longer running before snapshot upload"
        );
    }
    let intent_digest = intent.digest()?;
    let reserved = state.state_store.reserve_runtime_snapshot(&intent)?;
    if matches!(
        &request.source,
        SnapshotProductionSource::BundleMaterialization { .. }
    ) {
        // One accepted upload may authorize one create. Pending, late or
        // quarantined stages cannot be retried or promoted to qualification.
        return produce_staged_snapshot(
            state,
            binding,
            &credential,
            &intent,
            &upload,
            bootstrap
                .as_ref()
                .context("staged snapshot lost bootstrap source")?,
        );
    }
    let claim = state
        .state_store
        .claim_runtime_snapshot_attempt(&intent.operation_id, &intent_digest)?;
    let RuntimeSnapshotAttemptClaim::StartAttempt(_) = claim else {
        return Ok(match claim {
            RuntimeSnapshotAttemptClaim::Reconcile(record)
            | RuntimeSnapshotAttemptClaim::Bound(record) => record,
            RuntimeSnapshotAttemptClaim::StartAttempt(_) => unreachable!(),
        });
    };
    ensure!(
        reserved.intent == intent,
        "snapshot reservation changed before contact claim"
    );
    let adapter_request = RuntimeSnapshotAdapterRequest {
        protocol: RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL.into(),
        intent,
        provider_spec_digest: binding.snapshot_spec_sha256().to_owned(),
        upload_descriptor: upload
            .descriptor()
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        upload_bytes: upload.bytes(),
        upload_sha256: upload.sha256().to_owned(),
    };
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let attempted = state.external_placement_backends.produce_runtime_snapshot(
        binding,
        &credential,
        &adapter_request,
        upload.descriptor(),
        deadline,
    );
    match attempted {
        Ok(observation) => match observation.value {
            RuntimeSnapshotAdapterResponse::Bound { locator } => state
                .state_store
                .bind_runtime_snapshot_locator(&locator, observation.deadline_exceeded),
            RuntimeSnapshotAdapterResponse::Uncertain { .. } => {
                state.state_store.quarantine_runtime_snapshot_attempt(
                    &adapter_request.intent.operation_id,
                    &intent_digest,
                )
            }
        },
        Err(error) => {
            state.state_store.quarantine_runtime_snapshot_attempt(
                &adapter_request.intent.operation_id,
                &intent_digest,
            )?;
            Err(error)
        }
    }
}

fn snapshot_attempt_deadline(
    now: i64,
    contact_timeout_seconds: u32,
    maximum_bootstrap_lifetime_seconds: u32,
    source_created_at: Option<&str>,
    source_timeout_seconds: Option<u32>,
) -> Result<i64> {
    let deadline = match (source_created_at, source_timeout_seconds) {
        (Some(created_at), Some(timeout_seconds)) => {
            let created_ms = chrono::DateTime::parse_from_rfc3339(created_at)?.timestamp_millis();
            let source_expiry_ms = created_ms
                .checked_add(i64::from(timeout_seconds) * 1_000)
                .context("snapshot source expiry overflow")?;
            let signed_ceiling_ms = now
                .checked_add(i64::from(maximum_bootstrap_lifetime_seconds) * 1_000)
                .context("snapshot source lifetime ceiling overflow")?;
            source_expiry_ms.min(signed_ceiling_ms)
        }
        (None, None) => now
            .checked_add(i64::from(contact_timeout_seconds) * 1_000)
            .context("snapshot attempt deadline overflow")?,
        _ => anyhow::bail!("snapshot source lifetime is incomplete"),
    };
    ensure!(
        deadline > now,
        "snapshot source lifetime expired before upload"
    );
    Ok(deadline)
}

fn produce_staged_snapshot(
    state: &AppState,
    binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    credential: &crate::vault::placement::PlacementCredential,
    intent: &RuntimeSnapshotIntent,
    upload: &GuestOwnerSnapshotUpload,
    bootstrap: &SnapshotBootstrapRecord,
) -> Result<RuntimeSnapshotRecord> {
    // This exact adapter closure was published before bootstrap first contact
    // and is joined to the retained source operation. A current bundle lookup
    // must not silently replace it between upload and create.
    let adapter = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
        state, bootstrap, binding, credential,
    )?;
    let mut upload_stage = RuntimeSnapshotStageIntent {
        schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        operation_id: String::new(),
        parent_operation_id: intent.operation_id.clone(),
        parent_intent_digest: intent.digest()?,
        stage: RuntimeSnapshotStage::Upload,
        accepted_upload_receipt_digest: None,
        attempt_deadline_ms: intent.attempt_deadline_ms,
    };
    upload_stage.operation_id = upload_stage.derived_operation_id()?;
    state
        .state_store
        .reserve_runtime_snapshot_upload_stage(&upload_stage)?;
    let receipt = match state
        .state_store
        .claim_runtime_snapshot_upload_stage(&upload_stage.operation_id, &upload_stage.digest()?)?
    {
        RuntimeSnapshotStageClaim::Accepted(record) => record
            .receipt
            .context("accepted snapshot upload lost its receipt")?,
        RuntimeSnapshotStageClaim::Reconcile(_) => return retained_snapshot_parent(state, intent),
        RuntimeSnapshotStageClaim::StartAttempt(_) => {
            let adapter_request = RuntimeSnapshotUploadAdapterRequest {
                protocol: RUNTIME_SNAPSHOT_UPLOAD_ADAPTER_PROTOCOL.into(),
                intent: intent.clone(),
                stage: upload_stage.clone(),
                provider_spec_digest: intent.provider_spec_digest.clone(),
                upload_descriptor: upload
                    .descriptor()
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                upload_bytes: upload.bytes(),
                upload_sha256: upload.sha256().to_owned(),
            };
            let attempted = adapter.upload_snapshot(
                binding,
                credential,
                &adapter_request,
                upload.descriptor(),
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
                    u64::from(binding.contact_timeout_seconds()),
                )),
            );
            let observed = match attempted {
                Ok(observed) => observed,
                Err(error) => {
                    state.state_store.quarantine_runtime_snapshot_upload_stage(
                        &upload_stage.operation_id,
                        &upload_stage.digest()?,
                    )?;
                    return Err(error);
                }
            };
            let record = state.state_store.bind_runtime_snapshot_upload_receipt(
                &observed.value,
                observed.deadline_exceeded,
            )?;
            if record.phase != RuntimeSnapshotStagePhase::Accepted {
                return retained_snapshot_parent(state, intent);
            }
            record
                .receipt
                .context("accepted snapshot upload lost its receipt")?
        }
        RuntimeSnapshotStageClaim::Bound(_) => {
            anyhow::bail!("snapshot upload cannot bind a provider locator")
        }
    };
    finish_staged_create(
        state,
        binding,
        credential,
        intent,
        upload_stage,
        receipt,
        &adapter,
    )
}

fn finish_staged_create(
    state: &AppState,
    binding: &crate::node_config::sections::runtime_snapshot_production::InstalledRuntimeSnapshotProductionBinding,
    credential: &crate::vault::placement::PlacementCredential,
    intent: &RuntimeSnapshotIntent,
    upload_stage: RuntimeSnapshotStageIntent,
    receipt: RuntimeSnapshotUploadReceipt,
    adapter: &crate::external_artifacts::RecoveredBootstrapLifecycle,
) -> Result<RuntimeSnapshotRecord> {
    let mut create_stage = RuntimeSnapshotStageIntent {
        schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        operation_id: String::new(),
        parent_operation_id: intent.operation_id.clone(),
        parent_intent_digest: intent.digest()?,
        stage: RuntimeSnapshotStage::Create,
        accepted_upload_receipt_digest: Some(receipt.digest()?),
        attempt_deadline_ms: intent.attempt_deadline_ms,
    };
    create_stage.operation_id = create_stage.derived_operation_id()?;
    state
        .state_store
        .reserve_runtime_snapshot_create_stage(&create_stage)?;
    match state
        .state_store
        .claim_runtime_snapshot_create_stage(&create_stage.operation_id, &create_stage.digest()?)?
    {
        RuntimeSnapshotStageClaim::Bound(_) | RuntimeSnapshotStageClaim::Reconcile(_) => {
            retained_snapshot_parent(state, intent)
        }
        RuntimeSnapshotStageClaim::StartAttempt(_) => {
            let adapter_request = RuntimeSnapshotCreateAdapterRequest {
                protocol: RUNTIME_SNAPSHOT_CREATE_ADAPTER_PROTOCOL.into(),
                intent: intent.clone(),
                upload_stage,
                accepted_upload: receipt,
                create_stage: create_stage.clone(),
                provider_spec_digest: intent.provider_spec_digest.clone(),
            };
            let attempted = adapter.create_snapshot(
                binding,
                credential,
                &adapter_request,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
                    u64::from(binding.contact_timeout_seconds()),
                )),
            );
            let observed = match attempted {
                Ok(observed) => observed,
                Err(error) => {
                    state.state_store.quarantine_runtime_snapshot_create_stage(
                        &create_stage.operation_id,
                        &create_stage.digest()?,
                    )?;
                    return Err(error);
                }
            };
            state.state_store.bind_runtime_snapshot_create_locator(
                &observed.value,
                observed.deadline_exceeded,
            )?;
            retained_snapshot_parent(state, intent)
        }
        RuntimeSnapshotStageClaim::Accepted(_) => {
            anyhow::bail!("snapshot create cannot accept an upload receipt")
        }
    }
}

/// Continue only the create stage of an already accepted upload. Unlike fresh
/// production, this resolves the original signed binding and executable
/// closure from the retained bootstrap operation, so a Bundle replacement
/// cannot silently substitute a new adapter. It has no upload descriptor and
/// cannot mint another upload attempt.
pub fn continue_staged_create(
    state: &AppState,
    context: &HandlerContext,
    operation_id: &str,
) -> Result<RuntimeSnapshotRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    ensure!(
        lillux::valid_hash(operation_id),
        "snapshot operation ID is invalid"
    );
    let parent = state
        .state_store
        .runtime_snapshot_operation(operation_id)?
        .context("snapshot operation is absent")?;
    ensure!(
        parent.intent.owner_principal == context.fingerprint
            && matches!(
                &parent.intent.source,
                RuntimeSnapshotSource::BundleMaterialization { .. }
            ),
        "snapshot continuation has no owned materialized source"
    );
    if parent.phase == RuntimeSnapshotPhase::Bound {
        return Ok(parent);
    }
    ensure!(
        parent.phase == RuntimeSnapshotPhase::Reserved,
        "snapshot continuation is not an unbound staged operation"
    );
    let bootstrap_id = parent
        .intent
        .source_bootstrap_operation_id
        .as_deref()
        .context("staged snapshot lost its bootstrap operation")?;
    let bootstrap = get_bootstrap_source(state, context, bootstrap_id)?;
    ensure!(
        bootstrap.phase == SnapshotBootstrapPhase::OccurrenceBound
            && bootstrap.readiness.is_some()
            && bootstrap.intent.source == parent.intent.source
            && bootstrap.intent.production_binding_digest
                == parent.intent.production_profile_digest,
        "snapshot continuation differs from its retained ready bootstrap"
    );
    let RuntimeSnapshotSource::BundleMaterialization {
        materialization_attestation_hash,
        source_coordinate_digest,
        materialization_binding_digest,
    } = &parent.intent.source
    else {
        unreachable!("owned staged continuation already required materialization")
    };
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let retained =
        crate::operator_guest_runtime_materialization::load_retained_guest_owner_materialization(
            state,
            &context.fingerprint,
            source_coordinate_digest,
            materialization_attestation_hash,
            &authority,
            &guard,
        )?;
    ensure!(
        retained.source.materialization_binding_digest == *materialization_binding_digest
            && retained.identity.manifest_hash == parent.intent.guest_runtime_manifest_hash
            && retained.identity.owner_executable_sha256 == parent.intent.owner_executable_sha256
            && retained.identity.controller_public_root == parent.intent.controller_public_root,
        "snapshot continuation changed its retained materialized runtime"
    );
    let binding = state
        .state_store
        .retained_snapshot_bootstrap_binding(bootstrap_id)?
        .context("snapshot continuation lost retained signed producer")?
        .recovered_binding()?;
    ensure!(
        binding.digest() == parent.intent.production_profile_digest
            && binding.adapter_artifact_hash() == parent.intent.adapter_artifact_hash
            && binding.snapshot_spec_sha256() == parent.intent.provider_spec_digest
            && binding.settings_digest() == parent.intent.settings_digest,
        "snapshot continuation changed its exact producer generation"
    );
    let mut upload_identity = RuntimeSnapshotStageIntent {
        schema: RUNTIME_SNAPSHOT_STAGE_SCHEMA,
        operation_id: String::new(),
        parent_operation_id: parent.intent.operation_id.clone(),
        parent_intent_digest: parent.intent.digest()?,
        stage: RuntimeSnapshotStage::Upload,
        accepted_upload_receipt_digest: None,
        attempt_deadline_ms: parent.intent.attempt_deadline_ms,
    };
    upload_identity.operation_id = upload_identity.derived_operation_id()?;
    let upload_record = state
        .state_store
        .runtime_snapshot_stage(&upload_identity.operation_id)?
        .context("snapshot continuation has no retained upload stage")?;
    ensure!(
        upload_record.phase == RuntimeSnapshotStagePhase::Accepted,
        "snapshot continuation requires an accepted upload"
    );
    upload_record.intent.validate_for(&parent.intent, None)?;
    let receipt = upload_record
        .receipt
        .context("accepted snapshot upload lost its receipt")?;
    receipt.validate_for_stage(&parent.intent, &upload_record.intent)?;
    let access = binding.credential_access()?;
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    let adapter = crate::external_artifacts::RecoveredBootstrapLifecycle::from_operation(
        state,
        &bootstrap,
        &binding,
        &credential,
    )?;
    finish_staged_create(
        state,
        &binding,
        &credential,
        &parent.intent,
        upload_record.intent,
        receipt,
        &adapter,
    )
}

fn retained_snapshot_parent(
    state: &AppState,
    intent: &RuntimeSnapshotIntent,
) -> Result<RuntimeSnapshotRecord> {
    let record = state
        .state_store
        .runtime_snapshot_operation(&intent.operation_id)?
        .context("snapshot stage lost its retained parent")?;
    ensure!(
        record.intent == *intent,
        "snapshot stage changed its retained parent"
    );
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_external_execution_contract::restored_runtime_measurement::{
        ConsumerVerifierAdapterObservation, RestoredOwnerMeasurement,
        RestoredVerifierAdapterObservation, RestoredVerifierObservation,
    };
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_RESULT_SCHEMA, RuntimeSnapshotLocator,
        RuntimeSnapshotQualificationOccurrence, RuntimeSnapshotQualificationTerminalObservation,
        RuntimeSnapshotReadinessObservation,
    };

    fn owner_observation_mut(
        record: &mut RestoredVerifierAttemptRecord,
    ) -> &mut RestoredVerifierAdapterObservation {
        match record.observation.as_mut().unwrap() {
            RestoredVerifierObservation::OwnerMeasurement { observation } => observation,
            RestoredVerifierObservation::ConsumerRuntime { .. } => {
                panic!("owner-measurement fixture contains a consumer result")
            }
        }
    }

    #[test]
    fn materialized_snapshot_window_ends_at_source_expiry_not_first_contact() {
        let created = "2026-09-29T00:00:00Z";
        let created_ms = chrono::DateTime::parse_from_rfc3339(created)
            .unwrap()
            .timestamp_millis();
        let now = created_ms + 30_000;
        assert_eq!(
            snapshot_attempt_deadline(now, 60, 3_600, Some(created), Some(900)).unwrap(),
            created_ms + 900_000
        );
        assert_eq!(
            snapshot_attempt_deadline(now, 60, 120, Some(created), Some(900)).unwrap(),
            now + 120_000
        );
        assert_eq!(
            snapshot_attempt_deadline(now, 60, 3_600, None, None).unwrap(),
            now + 60_000
        );
        assert!(snapshot_attempt_deadline(now, 60, 3_600, Some(created), Some(30)).is_err());
        assert!(snapshot_attempt_deadline(now, 60, 3_600, Some(created), None).is_err());
    }

    #[test]
    fn materialized_restoration_requires_exact_complete_journals() {
        let owner = format!("fp:{}", "1".repeat(64));
        let root = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(
                lillux::crypto::SigningKey::from_bytes(&[3; 32])
                    .verifying_key()
                    .to_bytes()
            )
        );
        let identity = GuestOwnerRuntimeManifestIdentity {
            manifest_hash: "2".repeat(64),
            owner_executable_sha256: "3".repeat(64),
            controller_root_blob_sha256: "4".repeat(64),
            controller_public_root: root.clone(),
        };
        let source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "5".repeat(64),
            source_coordinate_digest: "6".repeat(64),
            materialization_binding_digest: "7".repeat(64),
        };
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: owner.clone(),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbx-source".into(),
            source_bootstrap_operation_id: Some("8".repeat(64)),
            source_created_at: Some("2026-09-29T00:00:00Z".into()),
            source_timeout_seconds: Some(900),
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "9".repeat(64),
            adapter_artifact_hash: "a".repeat(64),
            provider_spec_digest: "b".repeat(64),
            settings_digest: "c".repeat(64),
            source: source.clone(),
            guest_runtime_manifest_hash: identity.manifest_hash.clone(),
            owner_executable_sha256: identity.owner_executable_sha256.clone(),
            controller_public_root: root,
            upload_sha256: "d".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 1_000_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let locator = RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-exact".into(),
            provider_response_sha256: "e".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        };
        let readiness = RuntimeSnapshotReadinessObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            creation_response_sha256: locator.provider_response_sha256.clone(),
            readiness_response_sha256: "f".repeat(64),
            captured_at: "2026-09-29T00:01:00Z".into(),
            size_bytes: 4096,
        };
        let mut snapshot = RuntimeSnapshotRecord {
            intent,
            phase: RuntimeSnapshotPhase::Bound,
            locator: Some(locator.clone()),
            readiness: Some(readiness),
            completion_at_ms: Some(999_000),
            runner_deadline_exceeded: Some(false),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        let mut qualification_intent = RuntimeSnapshotQualificationIntent {
            schema: RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
            operation_id: String::new(),
            owner_principal: owner.clone(),
            snapshot_operation_id: snapshot.intent.operation_id.clone(),
            snapshot_intent_digest: snapshot.intent.digest().unwrap(),
            snapshot_id: locator.snapshot_id.clone(),
            provider_id: snapshot.intent.provider_id.clone(),
            provider_group_id: snapshot.intent.provider_group_id.clone(),
            qualification_profile_digest: "1".repeat(64),
            adapter_artifact_hash: "2".repeat(64),
            provider_spec_digest: "3".repeat(64),
            settings_digest: "4".repeat(64),
            verifier_artifact_hash: "5".repeat(64),
            maximum_lifetime_seconds: 900,
            attempt_deadline_ms: 1_100_000,
        };
        qualification_intent.operation_id = qualification_intent.derived_operation_id().unwrap();
        let occurrence = RuntimeSnapshotQualificationOccurrence {
            schema: 1,
            operation_id: qualification_intent.operation_id.clone(),
            occurrence_id: "sbx-restored".into(),
            provider_response_sha256: "6".repeat(64),
            contact_deadline_exceeded: false,
        };
        let qualification = SnapshotQualificationRecord {
            intent: qualification_intent,
            phase: SnapshotQualificationPhase::OccurrenceBound,
            occurrence: Some(occurrence.clone()),
            created_at_ms: 3,
            updated_at_ms: 4,
        };
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: snapshot.intent.operation_id.clone(),
            snapshot_id: locator.snapshot_id.clone(),
            restored_occurrence_id: occurrence.occurrence_id.clone(),
            nonce_hex: "7".repeat(64),
        };
        let mut verifier_intent = RestoredVerifierAttemptIntent {
            schema: 2,
            operation_id: String::new(),
            qualification_operation_id: qualification.intent.operation_id.clone(),
            restored_occurrence_id: occurrence.occurrence_id.clone(),
            verifier_artifact_hash: qualification.intent.verifier_artifact_hash.clone(),
            upload_sha256: "8".repeat(64),
            upload_bytes: 1024,
            purpose: ryeos_external_execution_contract::restored_runtime_measurement::RemoteVerificationPurpose::OwnerMeasurement { challenge: challenge.clone() },
            attempt_deadline_ms: 1_200_000,
        };
        verifier_intent.operation_id = verifier_intent.derived_operation_id().unwrap();
        let observation = RestoredVerifierAdapterObservation {
            schema: 1,
            operation_id: verifier_intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            upload_token_execution_id: "exe-upload".into(),
            run_token_execution_id: "exe-run".into(),
            upload_response_sha256: "9".repeat(64),
            run_stream_sha256: "a".repeat(64),
            measurement: RestoredOwnerMeasurement {
                schema: 1,
                protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
                challenge_digest: challenge.digest().unwrap(),
                manifest_hash: identity.manifest_hash.clone(),
                owner_executable_sha256: identity.owner_executable_sha256.clone(),
                controller_public_root: identity.controller_public_root.clone(),
            },
            contact_deadline_exceeded: false,
        };
        let mut verifier = RestoredVerifierAttemptRecord {
            intent: verifier_intent,
            consumer_selection: None,
            phase: RestoredVerifierAttemptPhase::Observed,
            observation: Some(RestoredVerifierObservation::OwnerMeasurement { observation }),
            created_at_ms: 5,
            updated_at_ms: 6,
        };
        let mut termination_intent = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: qualification.intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            owner_principal: owner.clone(),
            provider_id: snapshot.intent.provider_id.clone(),
            provider_spec_digest: qualification.intent.provider_spec_digest.clone(),
            attempt_deadline_ms: 1_300_000,
        };
        termination_intent.operation_id = termination_intent.derived_operation_id().unwrap();
        let termination_observation = RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: termination_intent.operation_id.clone(),
            occurrence_id: occurrence.occurrence_id.clone(),
            provider_response_sha256: "b".repeat(64),
            terminated_at: "2026-09-29T00:02:00Z".into(),
            contact_deadline_exceeded: false,
        };
        let termination = QualificationTerminationRecord {
            intent: termination_intent,
            phase: QualificationTerminationPhase::Terminal,
            observation: Some(termination_observation),
            created_at_ms: 7,
            updated_at_ms: 8,
        };
        let check = |snapshot: &RuntimeSnapshotRecord,
                     verifier: &RestoredVerifierAttemptRecord,
                     expected: &RuntimeSnapshotSource| {
            verify_materialized_restoration_journal(
                snapshot,
                &qualification,
                verifier,
                &termination,
                expected,
                &identity,
                "render-sandbox-early-access",
                &owner,
            )
        };
        check(&snapshot, &verifier, &source).unwrap();
        let mut wrong_lane = verifier.clone();
        wrong_lane.observation = Some(RestoredVerifierObservation::ConsumerRuntime {
            observation: ConsumerVerifierAdapterObservation {
                schema: 1,
                operation_id: verifier.intent.operation_id.clone(),
                occurrence_id: occurrence.occurrence_id.clone(),
                verifier_artifact_hash: verifier.intent.verifier_artifact_hash.clone(),
                challenge_digest: "1".repeat(64),
                upload_token_execution_id: "exe-upload".into(),
                run_token_execution_id: "exe-run".into(),
                upload_response_sha256: "9".repeat(64),
                run_stream_sha256: "a".repeat(64),
                evidence_sha256: "b".repeat(64),
                evidence_bytes: 2,
                contact_deadline_exceeded: false,
            },
        });
        assert!(
            check(&snapshot, &wrong_lane, &source)
                .unwrap_err()
                .to_string()
                .contains("consumer result is not owner measurement")
        );
        snapshot.intent.source = RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: "c".repeat(64),
        };
        assert!(check(&snapshot, &verifier, &source).is_err());
        snapshot.intent.source = source.clone();
        snapshot.runner_deadline_exceeded = Some(true);
        assert!(check(&snapshot, &verifier, &source).is_err());
        snapshot.runner_deadline_exceeded = Some(false);
        let mut wrong_source = source.clone();
        if let RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash,
            ..
        } = &mut wrong_source
        {
            *materialization_attestation_hash = "e".repeat(64);
        }
        assert!(check(&snapshot, &verifier, &wrong_source).is_err());
        owner_observation_mut(&mut verifier).contact_deadline_exceeded = true;
        assert!(check(&snapshot, &verifier, &source).is_err());
        owner_observation_mut(&mut verifier).contact_deadline_exceeded = false;
        owner_observation_mut(&mut verifier)
            .measurement
            .owner_executable_sha256 = "d".repeat(64);
        assert!(check(&snapshot, &verifier, &source).is_err());
        owner_observation_mut(&mut verifier)
            .measurement
            .owner_executable_sha256 = identity.owner_executable_sha256.clone();
        let mut late_terminal = termination.clone();
        late_terminal
            .observation
            .as_mut()
            .unwrap()
            .contact_deadline_exceeded = true;
        assert!(
            verify_materialized_restoration_journal(
                &snapshot,
                &qualification,
                &verifier,
                &late_terminal,
                &source,
                &identity,
                "render-sandbox-early-access",
                &owner,
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_probe_requires_exact_bound_snapshot_product_and_owner() {
        let manifest_hash = "7".repeat(64);
        let selections = ryeos_state::external_content::products::qualification::test_support::qualified_runtime_selections(&manifest_hash).unwrap();
        let proof = selections
            .get("auxiliary")
            .unwrap()
            .qualification
            .as_ref()
            .unwrap();
        let owner = proof.evidence.product_coordinate.owner_principal.clone();
        let root_key = lillux::crypto::SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let source = GuestOwnerRuntimeManifestIdentity {
            manifest_hash: manifest_hash.clone(),
            owner_executable_sha256: "8".repeat(64),
            controller_root_blob_sha256: "9".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(root_key.to_bytes())
            ),
        };
        let now = i64::try_from(lillux::time::timestamp_millis()).unwrap();
        let mut intent = RuntimeSnapshotIntent {
            schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
            operation_id: String::new(),
            owner_principal: owner.clone(),
            provider_id: "render-sandbox-early-access".into(),
            source_occurrence_id: "sbx-source".into(),
            source_bootstrap_operation_id: None,
            source_created_at: None,
            source_timeout_seconds: None,
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            source: RuntimeSnapshotSource::CapturedProduct {
                product_witness_hash: proof.evidence.product_witness_hash.clone(),
            },
            guest_runtime_manifest_hash: manifest_hash,
            owner_executable_sha256: source.owner_executable_sha256.clone(),
            controller_public_root: source.controller_public_root.clone(),
            upload_sha256: "a".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: now + 60_000,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let named = ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotLocator {
            schema: RUNTIME_SNAPSHOT_RESULT_SCHEMA,
            operation_id: intent.operation_id.clone(),
            intent_digest: intent.digest().unwrap(),
            source_occurrence_id: intent.source_occurrence_id.clone(),
            provider_group_id: intent.provider_group_id.clone(),
            snapshot_id: "snp-exact".into(),
            provider_response_sha256: "b".repeat(64),
            provider_creation_observation: serde_json::json!({"schema": 1}),
            adapter_observation_sha256: lillux::sha256_hex(br#"{"schema":1}"#),
        };
        let mut record = RuntimeSnapshotRecord {
            intent,
            phase: RuntimeSnapshotPhase::Bound,
            locator: Some(named.clone()),
            readiness: None,
            completion_at_ms: Some(now),
            runner_deadline_exceeded: Some(false),
            created_at_ms: now,
            updated_at_ms: now,
        };
        validate_probe_snapshot_record(
            &record,
            &named,
            proof,
            &source,
            "render-sandbox-early-access",
            &owner,
        )
        .unwrap();
        record.phase = RuntimeSnapshotPhase::Quarantined;
        assert!(
            validate_probe_snapshot_record(
                &record,
                &named,
                proof,
                &source,
                "render-sandbox-early-access",
                &owner
            )
            .is_err()
        );
        record.phase = RuntimeSnapshotPhase::Bound;
        record.intent.source = RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: "f".repeat(64),
        };
        assert!(
            validate_probe_snapshot_record(
                &record,
                &named,
                proof,
                &source,
                "render-sandbox-early-access",
                &owner
            )
            .is_err()
        );
        record.intent.source = RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: proof.evidence.product_witness_hash.clone(),
        };
        record.intent.source = RuntimeSnapshotSource::BundleMaterialization {
            materialization_attestation_hash: "a".repeat(64),
            source_coordinate_digest: "b".repeat(64),
            materialization_binding_digest: "c".repeat(64),
        };
        assert!(
            validate_probe_snapshot_record(
                &record,
                &named,
                proof,
                &source,
                "render-sandbox-early-access",
                &owner,
            )
            .is_err()
        );
        record.intent.source = RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: proof.evidence.product_witness_hash.clone(),
        };
        assert!(
            validate_probe_snapshot_record(
                &record,
                &named,
                proof,
                &source,
                "another-provider",
                &owner
            )
            .is_err()
        );
        record.locator.as_mut().unwrap().snapshot_id = "snp-other".into();
        assert!(
            validate_probe_snapshot_record(
                &record,
                &named,
                proof,
                &source,
                "render-sandbox-early-access",
                &owner
            )
            .is_err()
        );
    }

    #[test]
    fn provider_terminal_join_requires_same_qualification_occurrence_and_observation() {
        let owner = format!("fp:{}", "1".repeat(64));
        let mut intent = RuntimeSnapshotQualificationTerminationIntent {
            schema: 1,
            operation_id: String::new(),
            qualification_operation_id: "2".repeat(64),
            occurrence_id: "sbx-restored".into(),
            owner_principal: owner.clone(),
            provider_id: "render-sandbox-early-access".into(),
            provider_spec_digest: "3".repeat(64),
            attempt_deadline_ms: 1,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let observation = ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotQualificationTerminalObservation {
            schema: 1,
            operation_id: intent.operation_id.clone(),
            occurrence_id: intent.occurrence_id.clone(),
            provider_response_sha256: "4".repeat(64),
            terminated_at: "2026-09-28T00:02:00Z".into(),
            contact_deadline_exceeded: false,
        };
        let hash = lillux::sha256_hex(
            &ryeos_external_execution_contract::canonical_json(&observation).unwrap(),
        );
        let mut retained = QualificationTerminationRecord {
            intent,
            phase: QualificationTerminationPhase::Terminal,
            observation: Some(observation),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let check = |retained: &QualificationTerminationRecord,
                     qualification: &str,
                     occurrence: &str,
                     hash: &str| {
            validate_provider_terminal_join(
                retained,
                qualification,
                occurrence,
                "render-sandbox-early-access",
                &owner,
                hash,
            )
        };
        assert!(check(&retained, &"2".repeat(64), "sbx-restored", &hash).is_ok());
        assert!(check(&retained, &"5".repeat(64), "sbx-restored", &hash).is_err());
        assert!(check(&retained, &"2".repeat(64), "sbx-other", &hash).is_err());
        assert!(check(&retained, &"2".repeat(64), "sbx-restored", &"6".repeat(64)).is_err());
        retained
            .observation
            .as_mut()
            .unwrap()
            .contact_deadline_exceeded = true;
        assert!(check(&retained, &"2".repeat(64), "sbx-restored", &hash).is_err());
        retained
            .observation
            .as_mut()
            .unwrap()
            .contact_deadline_exceeded = false;
        retained.phase = QualificationTerminationPhase::Quarantined;
        assert!(check(&retained, &"2".repeat(64), "sbx-restored", &hash).is_err());
    }
}
