//! Receiver-local acceptance of an already imported, origin-admitted product.
//! No transport, acquisition, consumer grant, or local producer replay is
//! performed here. Origin product testimony remains signed by its origin node.

use anyhow::{Context as _, bail};

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, ProductWitnessLookup, VerifiedProductWitness,
    load_product_attestation_value, lookup_product_witness_hash_guarded,
};
use ryeos_state::external_content::products::transfer::{
    MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES, ProductWitnessSource, RECEIVED_PRODUCT_POLICY,
    RECEIVED_PRODUCT_SCHEMA, ReceivedProductEvidence, verify_received_product_guarded,
};
use ryeos_state::object_closure::{ObjectClosureLimits, load_exact_cas_object_with_cas};
use ryeos_state::{Attestation, CasMutationGuard, PinnedStateAuthority};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductSourceVerification {
    Fresh,
    Retained,
}

/// The API must obtain `pinned_origin_key` from the existing authenticated
/// configured-remote import owner, never from an unchecked request key. The
/// exact origin admission and its witness must already have been imported.
/// Import's Mirrored rows retain those bytes independently of this acceptance.
/// This function still holds its own CAS guard through publication.
pub fn accept_imported_product(
    state: &AppState,
    context: &HandlerContext,
    origin_admission_hash: &str,
    witness_hash: &str,
    pinned_origin_key: &lillux::crypto::VerifyingKey,
    maximum_bytes: u64,
) -> anyhow::Result<(ryeos_state::AdmissionResult, VerifiedProductWitness)> {
    crate::operator_authority::require_local_configured_operator(state, context)?;
    require_hash(origin_admission_hash)?;
    require_hash(witness_hash)?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if maximum_bytes == 0 || maximum_bytes > policy.limits.max_total_bytes {
        bail!("received product maximum_bytes exceeds current import policy");
    }
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let evidence = ReceivedProductEvidence {
        schema: RECEIVED_PRODUCT_SCHEMA.into(),
        owner_principal: context.fingerprint.clone(),
        origin_verifying_key: *pinned_origin_key.as_bytes(),
    };
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let candidate =
        evidence.sign_attestation(origin_admission_hash, &signer, lillux::time::iso8601_now())?;
    // This proves both exact signatures, same operator, full ordinary CAS
    // bytes, and absence of sidecar dependencies before any publication.
    let verified = verify_received_product_guarded(
        &authority,
        &guard,
        limits,
        &candidate,
        state.identity.verifying_key(),
        &context.fingerprint,
    )?;
    let witness = verified.admitted_product.product;
    require_witness(&witness, witness_hash, &context.fingerprint)?;
    super::retained_product::validate_import_bounds(
        &authority,
        &guard,
        limits,
        &policy.limits,
        maximum_bytes,
        &witness,
    )?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire product receipt write permit: {error}"))?;
    // Reuse the exact admission/submit publication owner with our existing
    // guard, not the StateStore wrapper that acquires a second guard.
    let published = state.state_store.with_state_db(|db| {
        ryeos_state::admission::publish_admission_attestation(
            db, &candidate, limits, &signer, &guard,
        )
    })?;
    Ok((published, witness))
}

/// Source verification for already-authorized application callers. `owner`
/// must be the exact admitted acting operator, not a manifest-supplied value.
/// Retained verification never consults a current head or reapplies import
/// eligibility. Its caller supplies the retained read/closure safety bounds.
#[allow(clippy::too_many_arguments)]
pub fn load_product_source(
    state: &AppState,
    authority: &PinnedStateAuthority,
    guard: &CasMutationGuard,
    limits: ObjectClosureLimits,
    owner: &str,
    witness_hash: &str,
    source: &ProductWitnessSource,
    verification: ProductSourceVerification,
) -> anyhow::Result<VerifiedProductWitness> {
    authority.ensure_guard(guard)?;
    require_hash(witness_hash)?;
    source.validate()?;
    if verification == ProductSourceVerification::Fresh {
        require_current_closure_limits(state, limits)?;
    }
    let witness = match source {
        ProductWitnessSource::LocalCapture {} => match verification {
            ProductSourceVerification::Fresh => super::retained_product::load_current_product(
                state,
                authority,
                guard,
                limits,
                owner,
                witness_hash,
            )?,
            ProductSourceVerification::Retained => {
                let value = load_product_attestation_value(authority, witness_hash, limits, guard)?
                    .context("retained local product witness is absent")?;
                let attestation = Attestation::from_value(&value)?;
                let evidence = ryeos_state::external_content::products::ProductCaptureEvidence::from_attestation(&attestation)?;
                let coordinate = ProductCaptureCoordinate::from_evidence(&evidence)?;
                let ProductWitnessLookup::Found(witness) = lookup_product_witness_hash_guarded(
                    authority,
                    &coordinate,
                    witness_hash,
                    state.identity.verifying_key(),
                    limits,
                    guard,
                )?
                else {
                    bail!("retained local product witness is absent");
                };
                witness
            }
        },
        ProductWitnessSource::Received { acceptance_hash } => {
            let value = load_exact_cas_object_with_cas(
                &authority.cas_store()?,
                acceptance_hash,
                limits
                    .max_object_bytes
                    .min(MAX_PRODUCT_ADMISSION_ATTESTATION_BYTES),
            )?;
            let acceptance = Attestation::from_value(&value)?;
            if verification == ProductSourceVerification::Fresh {
                let head = state
                    .state_store
                    .with_state_db(|db| {
                        db.read_generic_head_ref(
                            &format!("admissions/{RECEIVED_PRODUCT_POLICY}"),
                            &acceptance.subject_hash,
                        )
                    })?
                    .context("received product acceptance has no current local head")?;
                if head.signer != state.identity.fingerprint()
                    || head.target_hash != *acceptance_hash
                {
                    bail!("received product is not the exact current local acceptance");
                }
            }
            let received = verify_received_product_guarded(
                authority,
                guard,
                limits,
                &acceptance,
                state.identity.verifying_key(),
                owner,
            )?;
            if received.acceptance_hash != *acceptance_hash {
                bail!("received product acceptance changed identity");
            }
            let witness = received.admitted_product.product;
            if verification == ProductSourceVerification::Fresh {
                let policy = state
                    .node_policy
                    .require::<ExternalContentImportPolicyRecord>()?;
                super::retained_product::validate_import_bounds(
                    authority,
                    guard,
                    limits,
                    &policy.limits,
                    policy.limits.max_total_bytes,
                    &witness,
                )?;
            }
            witness
        }
    };
    require_witness(&witness, witness_hash, owner)?;
    Ok(witness)
}

fn require_current_closure_limits(
    state: &AppState,
    limits: ObjectClosureLimits,
) -> anyhow::Result<()> {
    let current = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    if limits.max_objects > current.max_objects
        || limits.max_blobs > current.max_blobs
        || limits.max_object_bytes > current.max_object_bytes
        || limits.max_total_object_bytes > current.max_total_object_bytes
        || limits.max_blob_bytes > current.max_blob_bytes
        || limits.max_total_blob_bytes > current.max_total_blob_bytes
        || limits.max_links_per_object > current.max_links_per_object
    {
        bail!("fresh product verification exceeds current closure policy");
    }
    Ok(())
}

fn require_hash(hash: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("product receipt requires exact canonical hashes");
    }
    Ok(())
}

fn require_witness(
    witness: &VerifiedProductWitness,
    expected: &str,
    owner: &str,
) -> anyhow::Result<()> {
    if witness.attestation_hash != expected || witness.evidence.owner_principal != owner {
        bail!("product receipt contradicts the exact selected witness or operator");
    }
    Ok(())
}
