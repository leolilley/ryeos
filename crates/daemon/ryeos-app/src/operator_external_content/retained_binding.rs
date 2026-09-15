//! Exact retained content reuse, not acquisition or a new binding authority.
//!
//! A completed import receipt has one publication target. Reusing its bytes
//! for another consumer requires a fresh ordinary stage and explicit bind;
//! never loosen that receipt's settlement or reopen a node cache as a host root.

use super::*;
use crate::node_policy::sections::external_content::{
    ExternalContentImportLimits, ExternalContentImportPolicyRecord,
};

pub(super) fn import(
    state: Arc<AppState>,
    context: HandlerContext,
    request: RetainedBindingImportRequest,
) -> anyhow::Result<ImportResponse> {
    let operator = crate::operator_authority::require_local_configured_operator(&state, &context)?;
    validate_request(&request)?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if request.maximum_bytes > policy.limits.max_total_bytes {
        bail!("retained binding import maximum_bytes exceeds node policy");
    }
    let closure_policy = state.node_policy.require::<NodeObjectClosurePolicy>()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    // Release quiesces this same barrier before replacing the source head.
    // Keep admission, exact-head verification and durable stage publication
    // in one permit, without holding the SQLite lock during content checks.
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let cas = authority.cas_store()?;
    let binding = ryeos_state::objects::ExternalContentBinding::from_value(
        &ryeos_state::object_closure::load_exact_cas_object_with_cas(
            &cas,
            &request.binding_hash,
            closure_policy.max_object_bytes,
        )?,
    )?;
    if binding.authorized_by != operator {
        bail!("retained binding is not owned by the configured operator");
    }
    let current = active_binding_from_store(
        &state.state_store,
        &cas,
        &binding.manifest_hash,
        &binding.consumer,
        state.identity.fingerprint(),
    )?
    .context("retained binding is not active on this node")?;
    if current.0 != request.binding_hash || current.1 != binding {
        bail!("retained binding is not the exact current head");
    }
    require_current_binding_authorizer(&state, &binding)?;
    let manifest_value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &cas,
        &binding.manifest_hash,
        closure_policy
            .max_object_bytes
            .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
    )?;
    if manifest_value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        != Some(binding.manifest_kind.as_str())
    {
        bail!("binding manifest kind is inconsistent");
    }
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [binding.manifest_hash.clone()],
        closure_policy.closure_limits()?,
    )?;
    if !closure.is_complete() {
        bail!("retained binding manifest closure is incomplete");
    }
    let large_store = authority.large_object_store()?;
    let (entry_count, total_bytes) = match binding.manifest_kind.as_str() {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
            let manifest =
                ryeos_state::objects::ExternalContentManifestObject::from_value(&manifest_value)?;
            validate_bounds(
                &policy.limits,
                request.maximum_bytes,
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
            ryeos_state::VerifiedExternalContentClosure::load(&cas, &binding.manifest_hash)?;
            (manifest.entry_count, manifest.total_bytes)
        }
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                &manifest_value,
            )?;
            validate_bounds(
                &policy.limits,
                request.maximum_bytes,
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
            for entry in &manifest.entries {
                if entry.file_sha256.is_some() {
                    large_store.verify_manifest_commitment(entry)?;
                } else if let Some(hash) = &entry.blob_hash {
                    // Existing CAS read verifies its address; the closed manifest
                    // already bounds each ordinary blob to the content tier.
                    let bytes = ryeos_state::object_closure::load_exact_cas_blob_with_cas(
                        &cas,
                        hash,
                        ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES
                            .min(policy.limits.max_file_bytes),
                    )?;
                    if entry.size != Some(bytes.len() as u64) {
                        bail!("retained binding blob size contradicts the manifest");
                    }
                }
            }
            (manifest.entry_count, manifest.total_bytes)
        }
        _ => bail!("retained binding names an unsupported manifest kind"),
    };
    // Reuse has no payload allocation or storage conversion. Reserve only
    // ordinary stage metadata/inodes, using the same import capacity owner.
    require_import_store_capacity(
        "external-content CAS",
        cas.filesystem_capacity()?,
        policy.limits.minimum_free_bytes,
        ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64,
        entry_count,
    )?;
    let digest = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!({
            "source": "retained_binding", "request": request,
            "operator": operator, "node": state.identity.fingerprint(),
            "limits": policy.limits, "closure_policy": closure_policy,
        }))?
        .as_bytes(),
    );
    let key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            &operator,
            "external-content-import",
            &key,
            None,
        )?;
    for hash in &closure.large_object_hashes {
        stage.protect_large_object_hash(&guard, hash)?;
    }
    stage.protect_cas_closure(&guard, [binding.manifest_hash.as_str()], std::iter::empty())?;
    // Admission now owns an independent import receipt. Later release of the
    // source binding is not retroactive cancellation of this authorized stage.
    // Destination declaration, owner and project generation are checked by bind.
    Ok(ImportResponse {
        staging_id: stage.staging_id().to_owned(),
        request_digest: digest,
        manifest_hash: binding.manifest_hash,
        manifest_kind: binding.manifest_kind,
        entry_count,
        total_bytes,
    })
}

fn validate_request(request: &RetainedBindingImportRequest) -> anyhow::Result<()> {
    if !lillux::valid_hash(&request.binding_hash)
        || request
            .binding_hash
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
        || request.maximum_bytes == 0
    {
        bail!(
            "retained binding import requires a canonical binding hash and positive byte ceiling"
        );
    }
    Ok(())
}

pub(super) fn validate_bounds<'a>(
    limits: &ExternalContentImportLimits,
    maximum_bytes: u64,
    entry_count: usize,
    total_bytes: u64,
    entries: impl Iterator<Item = (&'a str, Option<u64>, bool)>,
) -> anyhow::Result<()> {
    if entry_count > limits.max_entries || total_bytes > maximum_bytes {
        bail!("retained binding manifest exceeds import bounds");
    }
    for (path, size, is_directory) in entries {
        // Match capture's strict directory-depth ceiling. Files live in their
        // parent directory; even an empty directory must be enterable.
        let depth = path
            .split('/')
            .count()
            .saturating_sub(usize::from(!is_directory));
        if depth >= limits.max_depth || size.is_some_and(|size| size > limits.max_file_bytes) {
            bail!("retained binding entry exceeds node import bounds");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_binding_request_is_closed_and_canonical() {
        let value = serde_json::json!({"source":"retained_binding", "binding_hash":"a".repeat(64), "maximum_bytes":12});
        let ImportRequest::RetainedBinding(request) =
            serde_json::from_value(value.clone()).unwrap()
        else {
            panic!("source changed")
        };
        validate_request(&request).unwrap();
        for (field, extra) in [
            ("path", serde_json::json!("cache")),
            ("storage", serde_json::json!("content")),
            ("consumer_ref", serde_json::json!("tool:test/run")),
        ] {
            let mut invalid = value.clone();
            invalid[field] = extra;
            assert!(serde_json::from_value::<ImportRequest>(invalid).is_err());
        }
        for hash in ["A".repeat(64), "a".repeat(63), String::new()] {
            assert!(
                validate_request(&RetainedBindingImportRequest {
                    binding_hash: hash,
                    maximum_bytes: 1
                })
                .is_err()
            );
        }
        assert!(
            validate_request(&RetainedBindingImportRequest {
                binding_hash: "a".repeat(64),
                maximum_bytes: 0
            })
            .is_err()
        );
    }

    #[test]
    fn retained_binding_uses_current_import_bounds_without_recapturing() {
        let limits = ExternalContentImportLimits {
            max_depth: 2,
            max_entries: 2,
            max_file_bytes: 8,
            max_total_bytes: 16,
            store_budget_bytes: 32,
            minimum_free_bytes: 1,
        };
        assert!(
            validate_bounds(
                &limits,
                16,
                2,
                16,
                [("a", Some(8), false), ("b/c", Some(8), false)].into_iter()
            )
            .is_ok()
        );
        for (count, total, path, size) in [
            (3, 16, "a", 8),
            (2, 17, "a", 8),
            (2, 16, "a/b/c", 8),
            (2, 16, "a", 9),
        ] {
            assert!(
                validate_bounds(
                    &limits,
                    16,
                    count,
                    total,
                    [(path, Some(size), false)].into_iter()
                )
                .is_err()
            );
        }
        assert!(validate_bounds(&limits, 16, 1, 0, [("a/b", None, true)].into_iter()).is_err());
    }
}
