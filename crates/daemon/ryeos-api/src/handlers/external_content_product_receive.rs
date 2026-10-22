//! Receive one exact admitted product through the existing pinned remote owner.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use ryeos_state::large_object_store::{LargeObjectExpectation, LargeObjectStore};
use ryeos_state::objects::{ExternalLargeContentManifestEntry, load_if_large_content_manifest};
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::{
    NodeAdmittedObjectsClosureRequestOptions, ObjectsClosureRequestOptions, RemoteClient,
};
use crate::remote::import::{
    VerifiedRemoteImportRequest, import_admitted_root_with_job,
    require_local_large_object_dependencies,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub remote: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if self.remote.is_empty() || self.remote.len() > 256 || self.maximum_bytes == 0 {
            bail!("product receipt requires an exact remote and positive byte bound");
        }
        for hash in [&self.witness_hash, &self.origin_admission_hash] {
            if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
                bail!("product receipt requires exact canonical witness and admission hashes");
            }
        }
        Ok(())
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    req.validate()?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if req.maximum_bytes > policy.limits.max_total_bytes {
        bail!("product receipt byte bound exceeds current import policy");
    }
    // This is node-owned receipt, not project-selected trust or a caller key
    // override. Recovery later retains the authenticated key in local testimony.
    let remotes = crate::remote::config::load_remotes_layered(&state.config.app_root, None)?;
    let remote = crate::remote::config::get_remote(&remotes, &req.remote)?;
    let key = remote
        .pinned_signing_key()
        .context("product origin has no pinned signing key")?;
    let client = RemoteClient::from_remote_cfg(&state, &remote);
    let imported = import_admitted_root_with_job(
        &state,
        &client,
        VerifiedRemoteImportRequest {
            subject_hash: req.witness_hash.clone(),
            policy: ryeos_state::admission::LOCAL_ADMISSION_POLICY.into(),
            expected_issuer: format!("fp:{}", lillux::crypto::fingerprint(&key)),
            expected_key: key,
            expected_attestation_hash: Some(req.origin_admission_hash.clone()),
            source_peer: Some(remote.name.clone()),
            job_id: None,
            closure_options: NodeAdmittedObjectsClosureRequestOptions::for_node(
                &state,
                ObjectsClosureRequestOptions {
                    max_total_blob_bytes: Some(req.maximum_bytes),
                    allow_incomplete: false,
                    // Large objects are fetched below through the large-object tier.
                    allow_untransported_large_objects: true,
                    ..Default::default()
                },
            )?,
        },
    )
    .await?;
    let large_hashes = fetch_large_objects(&state, &client, &req.witness_hash).await?;
    // Existing Mirrored attribution durably roots imported CAS entries. A
    // refused local acceptance leaves only mirrored bytes, never consumer
    // authority. No private head or origin qualification is copied.
    let acceptance_state = Arc::clone(&state);
    let (accepted, product) = tokio::task::spawn_blocking(move || {
        require_local_large_object_dependencies(&acceptance_state, &large_hashes)?;
        ryeos_app::operator_external_content::product_receipt::accept_imported_product(
            &acceptance_state,
            &ctx,
            &req.origin_admission_hash,
            &req.witness_hash,
            &key,
            req.maximum_bytes,
        )
    })
    .await
    .context("product receiver acceptance task stopped")??;
    Ok(serde_json::json!({
        "witness_hash": product.attestation_hash,
        "origin_admission_hash": imported.import.attestation_hash,
        "acceptance_hash": accepted.attestation_hash,
        "reused_existing": accepted.reused_existing,
        "manifest_hash": product.evidence.manifest_hash,
        "job_id": imported.job_id,
        "attempt_id": imported.attempt_id,
    }))
}

/// Large objects the imported closure references, with the expectation each
/// manifest entry promises, plus the local store that receives them.
struct LargeReceivePlan {
    hashes: Vec<String>,
    expectations: BTreeMap<String, LargeObjectExpectation>,
    store: LargeObjectStore,
}

/// Build the upload expectation a large-content manifest entry promises.
/// `None` for an entry that names no large object. A large entry must carry a
/// positive size and chunk size, and exactly one chunk hash per chunk.
fn expectation_from_entry(
    entry: &ExternalLargeContentManifestEntry,
) -> Result<Option<(String, LargeObjectExpectation)>> {
    let Some(file_sha256) = &entry.file_sha256 else {
        return Ok(None);
    };
    let (Some(size), Some(chunk_size)) = (entry.size, entry.chunk_size) else {
        bail!(
            "large-content entry {} omits its size or chunk size",
            entry.path
        );
    };
    if size == 0 || chunk_size == 0 {
        bail!(
            "large-content entry {} declares a zero size or chunk size",
            entry.path
        );
    }
    let chunks = size.div_ceil(chunk_size);
    if u64::try_from(entry.chunk_hashes.len())? != chunks {
        bail!(
            "large-content entry {} declares {} chunk hashes but {size} bytes in {chunk_size}-byte chunks need {chunks}",
            entry.path,
            entry.chunk_hashes.len()
        );
    }
    Ok(Some((
        file_sha256.clone(),
        LargeObjectExpectation {
            size,
            chunk_size,
            chunk_hashes: entry.chunk_hashes.clone(),
        },
    )))
}

/// Walk the imported witness closure under the shared CAS guard, collecting
/// each referenced large object and its manifest-declared expectation. The
/// guard is released before this returns, so no CAS permit spans the network.
fn plan_large_receive(
    state: &ryeos_app::state::AppState,
    witness_hash: &str,
) -> Result<LargeReceivePlan> {
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [witness_hash.to_string()],
        limits,
    )?;
    if !report.is_complete() {
        bail!("imported closure of witness {witness_hash} is incomplete");
    }
    let mut expectations = BTreeMap::<String, LargeObjectExpectation>::new();
    for object_hash in &report.object_hashes {
        let Some(manifest) = load_if_large_content_manifest(&cas, object_hash)? else {
            continue;
        };
        for entry in &manifest.entries {
            let Some((file_sha256, expectation)) = expectation_from_entry(entry)
                .with_context(|| format!("large-content manifest {object_hash}"))?
            else {
                continue;
            };
            if let Some(prior) = expectations.get(&file_sha256) {
                if *prior != expectation {
                    bail!(
                        "large-content manifests declare conflicting shapes for large object {file_sha256}"
                    );
                }
                continue;
            }
            expectations.insert(file_sha256, expectation);
        }
    }
    let hashes = report
        .large_object_hashes
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    for hash in &hashes {
        if !expectations.contains_key(hash) {
            bail!("closure names large object {hash} that no present manifest declares");
        }
    }
    let store = authority.large_object_store()?;
    drop(guard);
    Ok(LargeReceivePlan {
        hashes,
        expectations,
        store,
    })
}

/// Stream every referenced large object that is not already resident with a
/// sidecar from the remote into the local large-object store. Returns the
/// referenced hashes for the final residency check.
async fn fetch_large_objects(
    state: &Arc<AppState>,
    client: &RemoteClient,
    witness_hash: &str,
) -> Result<Vec<String>> {
    let plan_state = Arc::clone(state);
    let witness = witness_hash.to_string();
    let plan = tokio::task::spawn_blocking(move || plan_large_receive(&plan_state, &witness))
        .await
        .context("product receiver large-object planning task stopped")??;
    for hash in &plan.hashes {
        let expectation = plan
            .expectations
            .get(hash)
            .with_context(|| format!("no expectation for large object {hash}"))?;
        if plan.store.object_size(hash)?.is_some() && plan.store.sidecar(hash)?.is_some() {
            continue;
        }
        // Always stream from offset 0: `stage_chunk` byte-compares any
        // overlap with an already-staged prefix and appends only the tail, so
        // a resumed transfer is verified rather than trusted.
        let mut offset = 0_u64;
        let mut complete = false;
        let streamed = client
            .stream_large_object(hash, |bytes| {
                let progress = plan.store.stage_chunk(hash, expectation, offset, bytes)?;
                offset += u64::try_from(bytes.len())?;
                complete = progress.complete;
                Ok(())
            })
            .await
            .with_context(|| format!("stream large object {hash}"))?;
        if streamed != expectation.size || !complete {
            bail!(
                "remote stream of large object {hash} ended at {streamed} of {} expected bytes",
                expectation.size
            );
        }
    }
    Ok(plan.hashes)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/receive-product",
    endpoint: "external-content.receive-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/receive-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_requires_exact_origin_coordinates_and_no_caller_key() {
        let valid = serde_json::json!({
            "remote": "origin",
            "witness_hash": "a".repeat(64),
            "origin_admission_hash": "b".repeat(64),
            "maximum_bytes": 1024,
        });
        serde_json::from_value::<Request>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for field in [
            "remote",
            "witness_hash",
            "origin_admission_hash",
            "maximum_bytes",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Request>(missing).is_err());
        }
        for (field, value) in [
            ("remote", serde_json::json!("")),
            ("witness_hash", serde_json::json!("A".repeat(64))),
            ("origin_admission_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut substituted = valid;
        substituted["signing_key"] = serde_json::json!("caller-selected-key");
        assert!(serde_json::from_value::<Request>(substituted).is_err());
    }

    fn entry(size: u64, chunk_size: u64, chunks: usize) -> ExternalLargeContentManifestEntry {
        ExternalLargeContentManifestEntry {
            path: "content".into(),
            kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
            mode: None,
            blob_hash: None,
            file_sha256: Some("a".repeat(64)),
            size: Some(size),
            chunk_size: Some(chunk_size),
            chunk_hashes: vec!["b".repeat(64); chunks],
            target: None,
        }
    }

    #[test]
    fn expectation_follows_a_consistent_entry() {
        let (hash, expectation) = expectation_from_entry(&entry(10, 4, 3)).unwrap().unwrap();
        assert_eq!(hash, "a".repeat(64));
        assert_eq!(expectation.size, 10);
        assert_eq!(expectation.chunk_size, 4);
        assert_eq!(expectation.chunk_hashes.len(), 3);
        let mut content_tier = entry(10, 4, 3);
        content_tier.file_sha256 = None;
        assert!(expectation_from_entry(&content_tier).unwrap().is_none());
    }

    #[test]
    fn expectation_refuses_contradictory_entries() {
        assert!(expectation_from_entry(&entry(10, 4, 2)).is_err());
        assert!(expectation_from_entry(&entry(0, 4, 0)).is_err());
        assert!(expectation_from_entry(&entry(10, 0, 3)).is_err());
        let mut no_size = entry(10, 4, 3);
        no_size.size = None;
        assert!(expectation_from_entry(&no_size).is_err());
    }
}
