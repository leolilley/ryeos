//! Durable, coordinate-keyed publication of a prepared guest owner runtime.
//!
//! The node signs a materialization claim, not an execution-capture witness.
//! This private entrypoint does not grant a snapshot or provider operation;
//! consumers must independently recheck current admission and qualification.

use std::fs::File;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use ryeos_state::objects::{
    Attestation, GUEST_RUNTIME_MATERIALIZATION_SCHEMA, GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND,
    GuestRuntimeMaterializationSubject,
};

use super::{GuestOwnerMaterializationSource, PreparedGuestOwnerMaterialization, retained};
use crate::handler_context::HandlerContext;
use crate::state::AppState;
use crate::state_store::NodeIdentitySigner;

const HEAD_NAMESPACE: &str = "guest-runtime-materialization";
const MAX_ATTESTATION_BYTES: u64 = 64 * 1024;

pub struct PublishedGuestOwnerMaterialization {
    pub coordinate_digest: String,
    pub source_evidence_hash: String,
    pub runtime_manifest_hash: String,
    pub subject_hash: String,
    pub attestation_hash: String,
    pub reused_existing: bool,
}

pub fn publish_prepared_guest_owner_runtime(
    state: &AppState,
    context: &HandlerContext,
    prepared: &PreparedGuestOwnerMaterialization,
) -> Result<PublishedGuestOwnerMaterialization> {
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire materialization publication permit: {error}")
        })?;
    state.engine.with_checked_bundle_generation(|generation| {
        publish_checked_generation(
            state,
            context,
            prepared,
            generation.request_engine_generation_identity(),
        )
    })
}

fn publish_checked_generation(
    state: &AppState,
    context: &HandlerContext,
    prepared: &PreparedGuestOwnerMaterialization,
    checked_generation: &str,
) -> Result<PublishedGuestOwnerMaterialization> {
    let source = prepared.source();
    let coordinate_digest = source.coordinate_digest()?;
    require_matching_operator(state, context, source)?;
    ensure!(
        source.node_site_id == state.identity.site_id()
            && source.controller_public_root
                == format!(
                    "ed25519:{}",
                    base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        state.identity.verifying_key().to_bytes()
                    )
                ),
        "materialization node identity changed after preparation"
    );
    let binding = state
        .node_config
        .guest_runtime_materialization
        .iter()
        .find(|binding| binding.id() == source.materialization_binding_id)
        .context("materialization binding was removed after preparation")?;
    ensure!(
        binding.digest() == source.materialization_binding_digest,
        "materialization binding changed after preparation"
    );
    ensure!(
        checked_generation == source.bundle_generation,
        "materialization Bundle generation changed after preparation"
    );
    for signer in &prepared.signer_keys {
        let current = state
            .engine
            .node_trust_store
            .get(&signer.signer_fingerprint)
            .context("materialization Bundle publisher is no longer trusted")?;
        ensure!(
            current.fingerprint == signer.signer_fingerprint
                && format!(
                    "ed25519:{}",
                    base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        current.verifying_key.to_bytes()
                    )
                ) == signer.verifying_key,
            "materialization Bundle publisher key changed after preparation"
        );
    }
    prepared.ensure_current()?;
    let evidence = prepared.source_evidence()?;

    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let key = ryeos_state::DurableCasPublicationKey::guest_runtime_materialization(
        &source.materialization_binding_digest,
        &coordinate_digest,
    )?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            &source.operator_authority.owner_principal,
            "guest-runtime-materialization",
            &key,
            None,
        )?;

    let mut budget = ryeos_state::LaunchCaptureBudget::bounded(
        3,
        4,
        source.maximum_owner_bytes,
        source.maximum_owner_bytes + 4 * 1024 + 64,
    )?;
    let manifest = ryeos_state::capture_tree_exact(
        prepared.root(),
        &mut budget,
        &mut MaterializationContentSink { cas: &cas },
    )?;
    let manifest_value = serde_json::to_value(&manifest)?;
    let manifest_hash = stage.store_object(&guard, &cas, &manifest_value)?;
    ensure!(
        manifest_hash == source.runtime_manifest_hash,
        "prepared guest runtime changed during exact CAS capture"
    );

    for item in prepared.signed_recipe_sources() {
        stage.store_blob(&guard, &cas, &item.signed_bytes)?;
    }
    for bundle in prepared.signed_bundle_manifests() {
        stage.store_blob(&guard, &cas, &bundle.signed_bytes)?;
    }
    let executor = prepared.executor_source_proof();
    stage.store_blob(&guard, &cas, &executor.signed_manifest_ref)?;
    stage.store_blob(
        &guard,
        &cas,
        lillux::canonical_json(&executor.manifest_object)?.as_bytes(),
    )?;
    stage.store_object(&guard, &cas, &executor.item_source_object)?;
    stage.store_blob(&guard, &cas, &executor.signed_sidecar)?;
    // The exact owner payload is already retained by the output capture.
    ensure!(
        cas.get_blob_bounded(&source.owner_executable_sha256, source.maximum_owner_bytes)?
            .is_some(),
        "materialized owner payload was not captured"
    );
    let source_evidence_hash = stage.store_object(&guard, &cas, &evidence.to_value()?)?;
    let subject = GuestRuntimeMaterializationSubject {
        schema: GUEST_RUNTIME_MATERIALIZATION_SCHEMA,
        kind: GUEST_RUNTIME_MATERIALIZATION_SUBJECT_KIND.to_owned(),
        coordinate_digest: coordinate_digest.clone(),
        runtime_manifest_hash: manifest_hash.clone(),
        source_evidence_hash: source_evidence_hash.clone(),
    };
    let subject_hash = stage.store_object(&guard, &cas, &subject.to_value()?)?;
    stage.protect_cas_closure(&guard, [subject_hash.as_str()], std::iter::empty())?;

    let claim =
        retained::MaterializationClaimEvidence::from_checked_source(source, &prepared.signer_keys)?;
    let signer = NodeIdentitySigner::from_identity(&state.identity);
    let attestation = Attestation::unsigned(
        subject_hash.clone(),
        retained::CLAIM.to_owned(),
        retained::POLICY.to_owned(),
        lillux::time::iso8601_now(),
        None,
        serde_json::to_value(claim)?,
    )
    .sign(&signer)?;
    let candidate_hash = stage.store_object(&guard, &cas, &attestation.to_value())?;
    // This is the publication linearization check. The checked Bundle
    // generation stays borrowed through the immutable head transaction; an
    // operator grant or installed binding changed during CAS capture refuses
    // before the signed head is exposed.
    require_matching_operator(state, context, source)?;
    ensure!(
        state
            .node_config
            .guest_runtime_materialization
            .iter()
            .any(|binding| {
                binding.id() == source.materialization_binding_id
                    && binding.digest() == source.materialization_binding_digest
            }),
        "materialization operator or binding changed during publication"
    );
    for signer in &prepared.signer_keys {
        let current = state
            .engine
            .node_trust_store
            .get(&signer.signer_fingerprint)
            .context("materialization publisher was revoked during publication")?;
        ensure!(
            current.fingerprint == signer.signer_fingerprint
                && format!(
                    "ed25519:{}",
                    base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        current.verifying_key.to_bytes()
                    )
                ) == signer.verifying_key,
            "materialization publisher key changed during publication"
        );
    }
    let (_, reused_existing) = ryeos_state::immutable_testimony::publish_immutable_attestation(
        &authority,
        HEAD_NAMESPACE,
        &coordinate_digest,
        &attestation,
        &signer,
        &guard,
        |candidate| retained::verify_testimony(&cas, candidate, state.identity.verifying_key()),
        |hash| {
            let value = retained::load_object_bounded(&cas, hash, MAX_ATTESTATION_BYTES)?;
            let incumbent = Attestation::from_value(&value)?;
            let verified =
                retained::verify_testimony(&cas, &incumbent, state.identity.verifying_key())?;
            Ok((incumbent, verified))
        },
    )?;
    let head = ryeos_state::immutable_testimony::read_immutable_attestation_head(
        &authority,
        HEAD_NAMESPACE,
        &coordinate_digest,
        &guard,
    )?
    .context("materialization immutable head is absent after publication")?;
    ensure!(
        head.signer == state.identity.fingerprint()
            && (reused_existing || head.target_hash == candidate_hash),
        "materialization immutable head changed node authority"
    );
    // A missing or contradictory incumbent was already rejected above. The
    // target must still be present and independently verified after the head.
    let attestation_value =
        retained::load_object_bounded(&cas, &head.target_hash, MAX_ATTESTATION_BYTES)?;
    let retained_attestation = Attestation::from_value(&attestation_value)?;
    let retained =
        retained::verify_testimony(&cas, &retained_attestation, state.identity.verifying_key())?;
    ensure!(
        retained.subject == subject
            && retained.source == *source
            && retained.output.manifest_hash == manifest_hash
            && retained.output.owner_executable_sha256 == source.owner_executable_sha256,
        "materialization head differs from the prepared exact source"
    );
    stage.protect_cas_closure(&guard, [head.target_hash.as_str()], std::iter::empty())?;
    stage.finish_admitted(&guard, &head.target_hash)?;
    Ok(PublishedGuestOwnerMaterialization {
        coordinate_digest,
        source_evidence_hash,
        runtime_manifest_hash: manifest_hash,
        subject_hash,
        attestation_hash: head.target_hash,
        reused_existing,
    })
}

fn require_matching_operator(
    state: &AppState,
    context: &HandlerContext,
    source: &GuestOwnerMaterializationSource,
) -> Result<()> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let current = crate::operator_authority::admitted_operator_authority_for_principal(
        state,
        &context.fingerprint,
    )?;
    ensure!(
        current == source.operator_authority
            && context.authorized_key_class == Some(current.principal_class)
            && (current.principal_class
                == crate::identity::AuthorizedKeyPrincipalClass::LocalClient
                || context.authenticated_origin_site_id.as_deref()
                    == Some(current.origin_site_id.as_str())),
        "materialization operator authority changed after preparation"
    );
    Ok(())
}

struct MaterializationContentSink<'a> {
    cas: &'a lillux::CasStore,
}

impl ryeos_state::ExternalContentBlobSink for MaterializationContentSink<'_> {
    fn store_file(&mut self, file: File, path: &str, expected_size: u64) -> Result<(String, u64)> {
        let outcome = self.cas.put_blob_from_open_regular_bounded(
            file,
            Path::new(path),
            ryeos_state::MAX_CAPTURE_FILE_BYTES,
        )?;
        ensure!(
            outcome.size == expected_size,
            "materialization file changed size during exact capture"
        );
        Ok((outcome.hash, outcome.size))
    }
}
