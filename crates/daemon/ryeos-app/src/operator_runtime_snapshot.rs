//! Operator-owned, one-attempt production of an unqualified provider locator.
//!
//! The upload is staged from current CAS authority by the executor. This
//! owner independently rejoins its manifest and controller root to the current
//! product witness before claiming the durable contact attempt. A locator is
//! not an installed-runtime qualification or placement grant.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_runtime_product::{
    GuestOwnerRuntimeManifestIdentity, derive_guest_owner_runtime_manifest_identity,
    seal_guest_owner_snapshot_upload,
};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    RESTORED_OWNER_MEASUREMENT_PROTOCOL, RESTORED_VERIFIER_ADAPTER_PROTOCOL,
    RestoredOwnerChallenge, RestoredVerifierAdapterRequest, RestoredVerifierAdapterResponse,
    RestoredVerifierAttemptIntent,
};
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_INTENT_SCHEMA,
    RUNTIME_SNAPSHOT_QUALIFICATION_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_QUALIFICATION_SCHEMA,
    RUNTIME_SNAPSHOT_READINESS_PROTOCOL, RuntimeSnapshotAdapterRequest,
    RuntimeSnapshotAdapterResponse, RuntimeSnapshotIntent,
    RuntimeSnapshotQualificationAdapterRequest, RuntimeSnapshotQualificationAdapterResponse,
    RuntimeSnapshotQualificationIntent, RuntimeSnapshotReadinessRequest,
    RUNTIME_SNAPSHOT_QUALIFICATION_TERMINATION_PROTOCOL,
    RuntimeSnapshotQualificationTerminationAdapterRequest,
    RuntimeSnapshotQualificationTerminationAdapterResponse,
    RuntimeSnapshotQualificationTerminationIntent,
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
use crate::runtime_db::runtime_snapshot_qualification::{
    SnapshotQualificationAttemptClaim, SnapshotQualificationPhase, SnapshotQualificationRecord,
};
use crate::runtime_db::runtime_snapshot_qualification_termination::{
    QualificationTerminationClaim, QualificationTerminationRecord,
};
use crate::state::AppState;

pub struct SnapshotProductionRequest {
    pub binding_id: String,
    pub witness_hash: String,
    pub source: ProductWitnessSource,
    pub source_occurrence_id: String,
    pub staged_identity: GuestOwnerRuntimeManifestIdentity,
    pub staged_root: lillux::PinnedDirectory,
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

/// Create at most one restored Sandbox for the exact retained snapshot.
/// This operation stops at an occurrence locator; verifier execution,
/// whole-guest settlement, and qualification remain separate authorities.
pub fn create_qualification_occurrence(
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
        schema: 1,
        operation_id: String::new(),
        qualification_operation_id: qualified.intent.operation_id.clone(),
        restored_occurrence_id: occurrence.occurrence_id.clone(),
        verifier_artifact_hash: qualification.verifier_artifact_hash().into(),
        upload_sha256: upload.sha256().into(),
        upload_bytes: upload.bytes(),
        challenge: RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: source.intent.operation_id.clone(),
            snapshot_id: locator.snapshot_id.clone(),
            restored_occurrence_id: occurrence.occurrence_id.clone(),
            nonce_hex: hex::encode(lillux::crypto::generate_random_bytes::<32>()),
        },
        attempt_deadline_ms: now
            .checked_add(i64::from(qualification.contact_timeout_seconds()) * 1_000)
            .context("restored verifier deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state
        .state_store
        .restored_verifier_attempt(&intent.operation_id)?
    {
        intent.challenge = existing.intent.challenge.clone();
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
        },
        Err(error) => {
            state
                .state_store
                .quarantine_restored_verifier_attempt(&request.intent.operation_id)?;
            Err(error)
        }
    }
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
    let qualified = state.state_store
        .snapshot_qualification_operation(qualification_operation_id)?
        .context("snapshot qualification operation is absent")?;
    ensure!(
        qualified.intent.owner_principal == context.fingerprint
            && qualified.phase == SnapshotQualificationPhase::OccurrenceBound,
        "qualification termination has no operator-owned restored occurrence"
    );
    let occurrence = qualified.occurrence.clone()
        .context("qualification termination occurrence is absent")?;
    let qualification = state.node_config.runtime_snapshot_qualification.iter()
        .find(|binding| binding.digest() == qualified.intent.qualification_profile_digest)
        .context("current signed snapshot qualification binding is absent")?;
    let producer = state.node_config.runtime_snapshot_production.iter()
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
    state.external_placement_backends
        .preflight_snapshot_qualification_create(producer, qualification, &credential)?;
    let now = lillux::time::timestamp_millis();
    let mut intent = RuntimeSnapshotQualificationTerminationIntent {
        schema: 1,
        operation_id: String::new(),
        qualification_operation_id: qualified.intent.operation_id.clone(),
        occurrence_id: occurrence.occurrence_id.clone(),
        owner_principal: context.fingerprint.clone(),
        provider_id: producer.backend().to_owned(),
        provider_spec_digest: qualification.provider_spec_digest().to_owned(),
        attempt_deadline_ms: now.checked_add(i64::from(qualification.contact_timeout_seconds()) * 1_000)
            .context("qualification termination deadline overflow")?,
    };
    intent.operation_id = intent.derived_operation_id()?;
    if let Some(existing) = state.state_store
        .qualification_termination_operation(&intent.operation_id)? {
        intent.attempt_deadline_ms = existing.intent.attempt_deadline_ms;
        ensure!(intent == existing.intent,
            "retained qualification termination changed signed coordinates");
    }
    intent.validate_for(&qualified.intent, &occurrence)?;
    state.state_store.reserve_qualification_termination(&intent)?;
    let claim = state.state_store
        .claim_qualification_termination_attempt(&intent.operation_id)?;
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
    let attempted = state.external_placement_backends
        .terminate_snapshot_qualification_occurrence(
            producer, qualification, &credential, &request, first_contact, deadline,
        );
    match attempted {
        Ok(output) => match output.value {
            RuntimeSnapshotQualificationTerminationAdapterResponse::Terminal { mut observation } => {
                observation.contact_deadline_exceeded = output.deadline_exceeded;
                state.state_store.bind_qualification_terminal_observation(&observation)
            }
            RuntimeSnapshotQualificationTerminationAdapterResponse::Pending { .. } => {
                if first_contact {
                    state.state_store.quarantine_qualification_termination_attempt(&request.intent.operation_id)
                } else {
                    state.state_store.qualification_termination_operation(&request.intent.operation_id)?
                        .context("qualification termination disappeared during reconciliation")
                }
            }
        },
        Err(error) => {
            if first_contact {
                state.state_store.quarantine_qualification_termination_attempt(&request.intent.operation_id)?;
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
    let binding = state
        .node_config
        .runtime_snapshot_production
        .iter()
        .find(|binding| binding.digest() == record.intent.production_profile_digest)
        .context("current signed snapshot producer binding is absent")?;
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
    state
        .external_placement_backends
        .preflight_runtime_snapshot(binding, &credential)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(binding.contact_timeout_seconds()),
    ));
    let observed = state
        .external_placement_backends
        .observe_runtime_snapshot_readiness(binding, &credential, &request, deadline)?;
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
    verify_probe_restored_verifier_record(
        state,
        &record,
        proof,
        source,
        provider_id,
        owner_principal,
    )?;
    Ok(Some(named))
}

fn verify_probe_restored_verifier_record(
    state: &AppState,
    snapshot: &RuntimeSnapshotRecord,
    proof: &ryeos_state::external_content::products::composition::AdmittedProductQualification,
    source: &GuestOwnerRuntimeManifestIdentity,
    provider_id: &str,
    owner_principal: &str,
) -> Result<()> {
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
        .context("runtime probe verifier has no complete observation")?;
    ensure!(
        retained.phase == RestoredVerifierAttemptPhase::Observed
            && !observation.contact_deadline_exceeded
            && retained.intent.challenge.operation_id == snapshot.intent.operation_id
            && retained.intent.challenge.snapshot_id
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
            && record.intent.product_witness_hash == proof.evidence.product_witness_hash
            && record.intent.guest_runtime_manifest_hash == source.manifest_hash
            && record.intent.owner_executable_sha256 == source.owner_executable_sha256
            && record.intent.controller_public_root == source.controller_public_root,
        "runtime probe locator differs from its exact bound product and provider attempt"
    );
    named.validate_for(&record.intent)?;
    Ok(())
}

pub fn produce(
    state: &AppState,
    context: &HandlerContext,
    request: SnapshotProductionRequest,
) -> Result<RuntimeSnapshotRecord> {
    crate::operator_authority::require_admitted_operator(state, context)?;
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
    let witness =
        crate::operator_external_content::product_receipt::load_bounded_current_product_source(
            state,
            &authority,
            &guard,
            limits,
            &context.fingerprint,
            &request.witness_hash,
            &request.source,
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
    let exact_identity =
        derive_guest_owner_runtime_manifest_identity(&manifest, state.identity.verifying_key())?;
    ensure!(
        exact_identity == request.staged_identity
            && witness.attestation_hash == request.witness_hash,
        "snapshot upload identity differs from the current product witness"
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

    let now = lillux::time::timestamp_millis();
    let mut intent = RuntimeSnapshotIntent {
        schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
        operation_id: String::new(),
        owner_principal: context.fingerprint.clone(),
        provider_id: binding.backend().to_owned(),
        source_occurrence_id: request.source_occurrence_id,
        provider_group_id: binding.provider_group_id().to_owned(),
        production_profile_digest: binding.digest().to_owned(),
        adapter_artifact_hash: binding.adapter_artifact_hash().to_owned(),
        provider_spec_digest: binding.snapshot_spec_sha256().to_owned(),
        settings_digest: binding.settings_digest().to_owned(),
        product_witness_hash: request.witness_hash,
        guest_runtime_manifest_hash: exact_identity.manifest_hash,
        owner_executable_sha256: exact_identity.owner_executable_sha256,
        controller_public_root: exact_identity.controller_public_root,
        upload_sha256: upload.sha256().to_owned(),
        upload_bytes: upload.bytes(),
        attempt_deadline_ms: now
            .checked_add(i64::from(binding.contact_timeout_seconds()) * 1_000)
            .context("snapshot attempt deadline overflow")?,
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
    }
    intent.validate()?;
    let intent_digest = intent.digest()?;
    let reserved = state.state_store.reserve_runtime_snapshot(&intent)?;
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
            RuntimeSnapshotAdapterResponse::Bound { locator } => {
                state.state_store.bind_runtime_snapshot_locator(&locator)
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ryeos_external_execution_contract::runtime_snapshot::RUNTIME_SNAPSHOT_RESULT_SCHEMA;

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
            provider_group_id: "sbg-group".into(),
            production_profile_digest: "3".repeat(64),
            adapter_artifact_hash: "4".repeat(64),
            provider_spec_digest: "d".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: proof.evidence.product_witness_hash.clone(),
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
        record.intent.product_witness_hash = "f".repeat(64);
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
        record.intent.product_witness_hash = proof.evidence.product_witness_hash.clone();
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
}
