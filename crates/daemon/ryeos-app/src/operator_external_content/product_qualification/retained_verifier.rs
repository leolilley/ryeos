//! Historical verifier byte authentication beneath an accepted purpose.
//! This is not occurrence admission, semantic qualification or contact authority.

use anyhow::{Context as _, Result, ensure};
use ryeos_engine::binary_resolver::{BundlePayloadIdentity, BundlePayloadSourceProof};
use ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView;

use crate::retained_bundle_evidence::{
    retained_verifier, verify_retained_signed_bundle_item, verify_retained_signed_bundle_manifest,
    verify_retained_signed_envelope,
};

/// Authenticate retained envelopes and exact executor bytes without reopening
/// installed paths. The caller must still reconstruct source-definition joins,
/// bind the accepted root and protected occurrence, and settle guest writers.
pub(super) fn authenticate_retained_verifier_bytes(
    cas: &lillux::CasStore,
    purpose: &QualificationExecutionPurposeView<'_>,
    scenario_id: &str,
) -> Result<Vec<u8>> {
    let source = purpose
        .remote_verifier_source(scenario_id)
        .context("accepted purpose has no retained remote verifier")?;
    source.validate_for(purpose.policy_source(), scenario_id)?;
    // These are accepted-purpose-owned blobs, not caller-supplied resolution
    // documents. Restoring reproduces identity; contributor authentication
    // remains a separate required join before execution authority is issued.
    for (hash, reference, digest) in [
        (
            &source.policy_resolution_blob_hash,
            &purpose.policy_source().canonical_ref,
            &purpose.policy_source().effective_definition_digest,
        ),
        (
            &source.recipe_resolution_blob_hash,
            &source.producer_source.canonical_ref,
            &source.producer_source.effective_definition_digest,
        ),
    ] {
        let retained: ryeos_engine::resolution::RetainedResolutionOutput =
            serde_json::from_value(canonical_blob(cas, hash, 4 * 1024 * 1024)?)?;
        ensure!(
            retained.root_ref() == reference
                && retained.effective_definition_digest()?.as_str() == digest,
            "retained resolution differs from accepted source identity"
        );
    }
    let signer = |fingerprint: &str| {
        source
            .signer_keys
            .iter()
            .find(|key| key.signer_fingerprint == fingerprint)
            .context("retained verifier source has no historical key")
    };
    let mut authenticated_bodies = std::collections::BTreeMap::new();
    for item in &source.signed_items {
        let bytes = blob(cas, &item.signed_blob_hash, 4 * 1024 * 1024)?;
        verify_retained_signed_bundle_item(item, signer(&item.signer_fingerprint)?, &bytes)?;
        authenticated_bodies.insert(
            item.resolved_ref.clone(),
            verify_retained_signed_envelope(
                &bytes,
                &item.signed_blob_hash,
                &item.raw_content_digest,
                &item.signature_envelope,
                signer(&item.signer_fingerprint)?,
            )?,
        );
    }
    let policy = purpose.policy_source();
    let policy_resolution = verify_resolution_contributors(
        cas,
        &source.policy_resolution_blob_hash,
        source,
        &authenticated_bodies,
    )?;
    ensure!(
        policy_resolution.root.raw_content_digest == policy.raw_content_digest
            && policy_resolution.root.signer_fingerprint.as_deref()
                == Some(policy.publisher_fingerprint.as_str()),
        "retained policy root differs from accepted source"
    );
    let parsed_policy = ryeos_state::external_content::products::qualification::ProductQualificationPolicy::from_value(
        policy_resolution.composed.composed.get(super::QUALIFICATION_POLICY_FIELD)
            .context("retained resolution has no qualification policy")?,
    )?;
    ensure!(
        parsed_policy == policy.policy,
        "retained composed policy differs from accepted policy"
    );
    let recipe_resolution = verify_resolution_contributors(
        cas,
        &source.recipe_resolution_blob_hash,
        source,
        &authenticated_bodies,
    )?;
    ensure!(
        recipe_resolution.root.raw_content_digest == source.producer_source.raw_content_digest
            && recipe_resolution.root.signer_fingerprint.as_deref()
                == Some(source.producer_source.publisher_fingerprint.as_str()),
        "retained recipe root differs from accepted source"
    );
    let recipe = ryeos_state::external_content::products::producer_recipe::ProductProducerRecipe::from_value(
        recipe_resolution.composed.composed.get(super::PRODUCER_RECIPE_FIELD)
            .context("retained resolution has no producer recipe")?.clone(),
    )?;
    ensure!(
        recipe.digest()? == source.producer_source.recipe_digest,
        "retained composed recipe differs from accepted source"
    );
    for manifest in &source.signed_bundle_manifests {
        let bytes = blob(cas, &manifest.signed_blob_hash, 4 * 1024 * 1024)?;
        verify_retained_signed_bundle_manifest(
            manifest,
            signer(&manifest.signer_fingerprint)?,
            &bytes,
        )?;
    }
    let executor = &source.executor;
    let proof = BundlePayloadSourceProof {
        selected_item_ref: executor.item_ref.clone(),
        signed_manifest_ref: blob(
            cas,
            &executor.signed_manifest_ref_blob_hash,
            ryeos_engine::executor_resolution::MAX_EXECUTOR_MANIFEST_REF_BYTES,
        )?,
        manifest_object: canonical_blob(cas, &executor.manifest_object_blob_hash, 4 * 1024 * 1024)?,
        item_source_object: canonical_object(cas, &executor.item_source_object_hash, 1024 * 1024)?,
        signed_sidecar: blob(cas, &executor.signed_sidecar_blob_hash, 1024 * 1024)?,
    };
    let identity = BundlePayloadIdentity {
        target_triple: executor.target_triple.clone(),
        signer_fingerprint: executor.signer_fingerprint.clone(),
        manifest_hash: executor.manifest_object_blob_hash.clone(),
        item_source_hash: executor.item_source_object_hash.clone(),
        content_hash: executor.payload_blob_hash.clone(),
    };
    ryeos_engine::binary_resolver::verify_retained_bundle_payload_proof(
        &proof,
        &identity,
        source.payload_mode,
        &retained_verifier(signer(&executor.signer_fingerprint)?)?,
    )?;
    let payload = blob(cas, &executor.payload_blob_hash, source.payload_bytes)?;
    ensure!(
        payload.len() as u64 == source.payload_bytes,
        "retained verifier payload size differs from accepted source"
    );
    Ok(payload)
}

fn verify_resolution_contributors(
    cas: &lillux::CasStore,
    hash: &str,
    source: &ryeos_state::external_content::products::qualification::remote_verifier_source::QualificationRemoteVerifierSource,
    authenticated_bodies: &std::collections::BTreeMap<String, String>,
) -> Result<ryeos_engine::resolution::ResolutionOutput> {
    let retained: ryeos_engine::resolution::RetainedResolutionOutput =
        serde_json::from_value(canonical_blob(cas, hash, 4 * 1024 * 1024)?)?;
    let resolution = retained.restore();
    for contributor in std::iter::once(&resolution.root)
        .chain(&resolution.ancestors)
        .chain(&resolution.referenced_items)
    {
        let item = source
            .signed_items
            .iter()
            .find(|item| item.resolved_ref == contributor.resolved_ref)
            .context("retained resolution contributor has no authenticated envelope")?;
        ensure!(
            contributor.source_space == ryeos_engine::contracts::ItemSpace::Bundle
                && contributor.source_root
                    == (ryeos_engine::contracts::ItemSourceRoot::Bundle {
                        name: item.bundle_name.clone()
                    })
                && contributor.trust_class == ryeos_engine::resolution::TrustClass::TrustedBundle
                && contributor.signer_fingerprint.as_deref()
                    == Some(item.signer_fingerprint.as_str())
                && contributor.raw_content_digest == item.raw_content_digest
                && authenticated_bodies.get(&contributor.resolved_ref)
                    == Some(&contributor.raw_content),
            "retained resolution contributor differs from authenticated Bundle source"
        );
    }
    Ok(resolution)
}

fn blob(cas: &lillux::CasStore, hash: &str, maximum: u64) -> Result<Vec<u8>> {
    let bytes = cas
        .get_blob_bounded(hash, maximum)?
        .with_context(|| format!("retained verifier CAS blob {hash} is missing"))?;
    ensure!(
        lillux::sha256_hex(&bytes) == hash,
        "retained verifier blob address changed"
    );
    Ok(bytes)
}

fn canonical_blob(cas: &lillux::CasStore, hash: &str, maximum: u64) -> Result<serde_json::Value> {
    canonical_value(&blob(cas, hash, maximum)?)
}

fn canonical_object(cas: &lillux::CasStore, hash: &str, maximum: u64) -> Result<serde_json::Value> {
    let (file, size) = cas
        .open_object(hash)?
        .with_context(|| format!("retained verifier CAS object {hash} is missing"))?;
    let bytes = lillux::read_open_regular_file_exact_bounded(file, size, maximum)?;
    ensure!(
        lillux::sha256_hex(&bytes) == hash,
        "retained verifier object address changed"
    );
    canonical_value(&bytes)
}

fn canonical_value(bytes: &[u8]) -> Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    ensure!(
        lillux::canonical_json(&value)?.as_bytes() == bytes,
        "retained verifier object is not canonical JSON"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::canonical_value;

    #[test]
    fn retained_objects_require_exact_canonical_encoding() {
        let value = serde_json::json!({"member": "bin/target/verifier", "mode": 493});
        let canonical = lillux::canonical_json(&value).unwrap();
        assert_eq!(canonical_value(canonical.as_bytes()).unwrap(), value);
        assert!(canonical_value(serde_json::to_string_pretty(&value).unwrap().as_bytes()).is_err());
        assert!(canonical_value(b"{\"mode\":1,\"mode\":493}").is_err());
        assert!(canonical_value(b"{\"mode\":493}\n").is_err());
        assert!(canonical_value(b"not-json").is_err());
    }
}
