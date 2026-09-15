//! Fresh ordinary import staging from an exact published product witness.
//!
//! The product head retains bytes and node testimony. This operation grants no
//! consumer authority: it creates a new one-use import stage which must still
//! pass the existing exact-manifest bind checks for its eventual consumer.

use super::*;
use crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_state::external_content::products::ProductCaptureEvidence;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, ProductWitnessLookup, VerifiedProductWitness,
    load_product_attestation_value, lookup_product_witness_guarded,
};

pub(super) fn import(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedProductImportRequest,
) -> anyhow::Result<ImportResponse> {
    import_with_verified(state, context, request, |_, _, _, _| Ok(()))
        .map(|(response, ())| response)
}

/// Run one caller-owned proof against the exact current authenticated witness
/// under the import's guard, then create the ordinary fresh stage. A failed
/// proof returns before stage publication and the verified witness is not
/// loaded or scrubbed a second time by composition.
pub(super) fn import_with_verified<T>(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedProductImportRequest,
    before_stage: impl FnOnce(
        &ryeos_state::PinnedStateAuthority,
        &ryeos_state::CasMutationGuard,
        &VerifiedProductWitness,
        ryeos_state::object_closure::ObjectClosureLimits,
    ) -> anyhow::Result<T>,
) -> anyhow::Result<(ImportResponse, T)> {
    // Exact owned product bytes only. Ambient and general retained-result
    // import retain their separate local-operator entry boundaries.
    let operator = crate::operator_authority::require_admitted_operator(&state, &context)?;
    validate_request(&request)?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if request.maximum_bytes > policy.limits.max_total_bytes {
        bail!("retained product import maximum_bytes exceeds node policy");
    }
    let closure_policy = state.node_policy.require::<NodeObjectClosurePolicy>()?;
    let limits = closure_policy.closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;

    let exact = super::product_receipt::load_product_source(
        &state,
        &authority,
        &guard,
        limits,
        &context.fingerprint,
        &request.witness_hash,
        &request.witness_source,
        super::product_receipt::ProductSourceVerification::Fresh,
    )?;
    let verified = before_stage(&authority, &guard, &exact, limits)?;

    validate_import_bounds(
        &authority,
        &guard,
        limits,
        &policy.limits,
        request.maximum_bytes,
        &exact,
    )?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [exact.evidence.manifest_hash.clone()],
        limits,
    )?;
    if !closure.is_complete() {
        bail!("retained product manifest closure is incomplete");
    }
    require_import_store_capacity(
        "external-content CAS",
        cas.filesystem_capacity()?,
        policy.limits.minimum_free_bytes,
        ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
        exact.evidence.entry_count,
    )?;

    let digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!({
            "source": "retained_product",
            "request": request,
            "coordinate_id": exact.coordinate_id,
            "operator": operator,
            "node": state.identity.fingerprint(),
            "limits": policy.limits,
            "closure_policy": closure_policy,
        }))?
        .as_bytes(),
    );
    let key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    // Read verification needs only the shared CAS guard. Take a publication
    // permit only when creating the durable stage, before its per-stage lock;
    // large-byte scrubbing must not hold up maintenance's writer drain.
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let stage = begin_fresh_stage(
        &authority,
        &guard,
        &operator,
        &key,
        &exact.evidence.manifest_hash,
        &closure.large_object_hashes,
    )?;
    Ok((
        ImportResponse {
            staging_id: stage.staging_id().to_owned(),
            request_digest: digest,
            manifest_hash: exact.evidence.manifest_hash,
            manifest_kind: exact.evidence.manifest_kind,
            entry_count: exact.evidence.entry_count,
            total_bytes: exact.evidence.total_bytes,
        },
        verified,
    ))
}

pub(super) fn validate_import_bounds(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    policy: &crate::node_policy::sections::external_content::ExternalContentImportLimits,
    maximum_bytes: u64,
    exact: &VerifiedProductWitness,
) -> anyhow::Result<()> {
    authority.ensure_guard(guard)?;
    if maximum_bytes == 0 || maximum_bytes > policy.max_total_bytes {
        bail!("retained product import maximum_bytes exceeds node policy");
    }
    let manifest_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        &exact.evidence.manifest_hash,
        limits
            .max_object_bytes
            .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
    )?;
    if manifest_value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        != Some(exact.evidence.manifest_kind.as_str())
    {
        bail!("retained product manifest kind is inconsistent");
    }
    match exact.evidence.manifest_kind.as_str() {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
            let manifest =
                ryeos_state::objects::ExternalContentManifestObject::from_value(&manifest_value)?;
            super::retained_binding::validate_bounds(
                policy,
                maximum_bytes,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.entries.iter().map(|entry| {
                    (
                        entry.path.as_str(),
                        entry.size,
                        entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                    )
                }),
            )?;
        }
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                &manifest_value,
            )?;
            super::retained_binding::validate_bounds(
                policy,
                maximum_bytes,
                manifest.entry_count,
                manifest.total_bytes,
                manifest.entries.iter().map(|entry| {
                    (
                        entry.path.as_str(),
                        entry.size,
                        entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::Dir,
                    )
                }),
            )?;
        }
        _ => bail!("retained product names an unsupported manifest kind"),
    }
    if exact.evidence.entry_count > policy.max_entries || exact.evidence.total_bytes > maximum_bytes
    {
        bail!("retained product testimony exceeds current import bounds");
    }
    Ok(())
}

fn begin_fresh_stage(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    operator: &str,
    key: &ryeos_state::DurableCasPublicationKey,
    manifest_hash: &str,
    large_object_hashes: &std::collections::BTreeSet<String>,
) -> anyhow::Result<ryeos_state::DurableCasUploadStage> {
    // `begin`, rather than `open`, is intentional: the same retained witness
    // may be presented for separate consumer grants, each with its own stage.
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(guard, operator, "external-content-import", key, None)?;
    for hash in large_object_hashes {
        stage.protect_large_object_hash(guard, hash)?;
    }
    // Bind requires this explicit manifest root in the stage even though the
    // separately durable product head reaches it through Attestation.subject.
    stage.protect_cas_closure(guard, [manifest_hash], std::iter::empty())?;
    Ok(stage)
}

fn found(
    lookup: ProductWitnessLookup,
    missing: &'static str,
) -> anyhow::Result<ryeos_state::external_content::products::publication::VerifiedProductWitness> {
    match lookup {
        ProductWitnessLookup::Found(witness) => Ok(witness),
        ProductWitnessLookup::Missing => bail!(missing),
    }
}

/// Verify the selected hash through its immutable current coordinate once.
/// Authenticate the bounded testimony before using its coordinate for lookup;
/// current-head lookup then verifies its publication and complete subject
/// closure. Loading the same closure by hash first would scrub large files twice.
pub(super) fn load_current_product(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    owner_principal: &str,
    witness_hash: &str,
) -> anyhow::Result<VerifiedProductWitness> {
    let value = load_product_attestation_value(authority, witness_hash, limits, guard)?
        .context("retained product witness is absent")?;
    let attestation = ryeos_state::Attestation::from_value(&value)?;
    attestation
        .verify_with_key(state.identity.verifying_key())
        .context("verify selected product testimony node signature")?;
    let evidence = ProductCaptureEvidence::from_attestation(&attestation)?;
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence)?;
    if coordinate.owner_principal != owner_principal {
        bail!("retained product witness is not owned by the configured operator");
    }
    let current = found(
        lookup_product_witness_guarded(
            authority,
            &coordinate,
            state.identity.verifying_key(),
            limits,
            guard,
        )?,
        "retained product capture is not currently published",
    )?;
    if current.attestation_hash != witness_hash {
        bail!("retained product witness is not the exact current published capture");
    }
    Ok(current)
}

fn validate_request(request: &RetainedProductImportRequest) -> anyhow::Result<()> {
    request.witness_source.validate()?;
    if !lillux::valid_hash(&request.witness_hash)
        || request
            .witness_hash
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
        || request.maximum_bytes == 0
    {
        bail!(
            "retained product import requires a canonical witness hash and positive byte ceiling"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn retained_product_request_requires_exact_witness_and_positive_bound() {
        validate_request(&RetainedProductImportRequest {
            witness_hash: "a".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            maximum_bytes: 1,
        })
        .unwrap();
        for (hash, maximum_bytes) in [
            ("A".repeat(64), 1),
            ("a".repeat(63), 1),
            ("a".repeat(64), 0),
        ] {
            assert!(
                validate_request(&RetainedProductImportRequest {
                    witness_hash: hash,
                    witness_source: ProductWitnessSource::LocalCapture {},
                    maximum_bytes,
                })
                .is_err()
            );
        }

        let value = serde_json::json!({
            "source": "retained_product",
            "witness_hash": "a".repeat(64),
            "witness_source": {"kind":"local_capture"},
            "maximum_bytes": 1,
        });
        let ImportRequest::RetainedProduct(decoded) =
            serde_json::from_value::<ImportRequest>(value.clone()).unwrap()
        else {
            panic!("retained product import source changed")
        };
        validate_request(&decoded).unwrap();
        let mut missing_source = value.clone();
        missing_source
            .as_object_mut()
            .unwrap()
            .remove("witness_source");
        assert!(serde_json::from_value::<ImportRequest>(missing_source).is_err());
        let mut invalid_source = value.clone();
        invalid_source["witness_source"] = serde_json::json!({
            "kind":"received", "acceptance_hash":"A".repeat(64)
        });
        let ImportRequest::RetainedProduct(invalid_source) =
            serde_json::from_value::<ImportRequest>(invalid_source).unwrap()
        else {
            panic!("retained product import source changed")
        };
        assert!(validate_request(&invalid_source).is_err());
        let mut restated = value;
        restated["manifest_hash"] = serde_json::json!("b".repeat(64));
        assert!(serde_json::from_value::<ImportRequest>(restated).is_err());
    }

    #[test]
    fn repeated_product_imports_receive_independent_fresh_stages() {
        let temp = tempfile::tempdir().unwrap();
        let db = ryeos_state::StateDb::open(temp.path(), Arc::new(ryeos_state::TrustStore::new()))
            .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let manifest_hash = authority
            .cas_store()
            .unwrap()
            .store_object(&serde_json::json!({
                "kind": "source_manifest",
                "item_source_hashes": {},
            }))
            .unwrap();
        let key = ryeos_state::DurableCasPublicationKey::external_content_import(&"b".repeat(64))
            .unwrap();
        let owner = "a".repeat(64);
        let first = begin_fresh_stage(
            &authority,
            &guard,
            &owner,
            &key,
            &manifest_hash,
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        let second = begin_fresh_stage(
            &authority,
            &guard,
            &owner,
            &key,
            &manifest_hash,
            &std::collections::BTreeSet::new(),
        )
        .unwrap();
        assert_ne!(first.staging_id(), second.staging_id());
        first.ensure_protects_object(&manifest_hash).unwrap();
        second.ensure_protects_object(&manifest_hash).unwrap();
    }
}
