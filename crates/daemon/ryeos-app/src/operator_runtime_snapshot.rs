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
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_ADAPTER_PROTOCOL, RUNTIME_SNAPSHOT_INTENT_SCHEMA,
    RuntimeSnapshotAdapterRequest, RuntimeSnapshotAdapterResponse, RuntimeSnapshotIntent,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::products::{ProductShape, ProductStorage};
use ryeos_state::object_closure::load_exact_cas_object_with_cas;

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::runtime_db::runtime_snapshot::{RuntimeSnapshotAttemptClaim, RuntimeSnapshotRecord};
use crate::state::AppState;

pub struct SnapshotProductionRequest {
    pub binding_id: String,
    pub witness_hash: String,
    pub source: ProductWitnessSource,
    pub source_occurrence_id: String,
    pub staged_identity: GuestOwnerRuntimeManifestIdentity,
    pub staged_root: lillux::PinnedDirectory,
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
    let source_ceiling = binding.maximum_upload_bytes() - 16 * 1024;

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
